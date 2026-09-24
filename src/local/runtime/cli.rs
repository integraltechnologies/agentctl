use super::*;
use crate::local::terminal::{Report, name, yes_no};
use provider::{ClaudeAdapter, CodexAdapter, ProviderAdapter};
use serde_json::{Value, json};

pub(crate) fn run(
    args: &[&str],
    machine: &super::super::config::MachineConfig,
    paths: &paths::MachinePaths,
    json_mode: bool,
) -> Result<()> {
    let root = std::env::current_dir()?;
    // Overrides are accepted only at this user-facing boundary, not in packets.
    let mut overrides = BTreeMap::new();
    let mut clean = vec![];
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--override" {
            let value = args
                .get(i + 1)
                .ok_or_else(|| Error::Invalid("--override needs role:provider[:model]".into()))?;
            let fields: Vec<_> = value.split(':').collect();
            require(
                (2..=3).contains(&fields.len())
                    && fields
                        .iter()
                        .all(|v| !v.trim().is_empty() && *v == v.trim()),
                "--override requires exactly 2 or 3 nonempty segments: role:provider[:model]",
            )?;
            require(
                !overrides.contains_key(fields[0]),
                "duplicate role override",
            )?;
            overrides.insert(
                fields[0].to_owned(),
                routing::RolePatch {
                    provider: Some(fields[1].into()),
                    model: fields.get(2).map(|v| (*v).into()),
                    ..Default::default()
                },
            );
            i += 2;
        } else {
            clean.push(args[i]);
            i += 1;
        }
    }
    let args = clean.as_slice();
    if matches!(args.first(), Some(&"roles" | &"role" | &"route")) {
        let project = if paths::project_config(&root).exists() {
            ProjectConfig::load(&root)?.routing
        } else {
            routing::ProjectRoles::default()
        };
        let names = routing::roles(&machine.runtime, &project);
        require(
            overrides.keys().all(|name| names.contains(name)),
            "explicit override names an unknown role",
        )?;
        match args {
            ["route", "check"] => {}
            ["role", "show", role] | ["route", role] => require(
                overrides.keys().all(|name| name == role),
                "override must target the requested role",
            )?,
            _ => require(
                overrides.is_empty(),
                "overrides require a route inspection or execution command",
            )?,
        }
        let inspect = |name: &str| -> Result<serde_json::Value> {
            // A name that is not a role at all is reported as such, rather than
            // as a role that merely lacks configuration.
            require(
                names.contains(name),
                format!(
                    "unknown role {name}; agentctl launches {}, and configuration may add inspection profiles",
                    routing::BUILTINS.join(", ")
                ),
            )?;
            let route = routing::resolve(&machine.runtime, &project, name, overrides.get(name))?;
            let candidates:Vec<_> = std::iter::once(&route.primary).chain(&route.fallbacks).map(|r| json!({"route":r,"executable_exists":machine.runtime.providers[&r.provider].executable.is_file(),"authentication":"NOT_PROBED (use provider doctor)","fresh_session":true,"structured_output":true,"model":"opaque passthrough","sandbox_available":process::sandbox_available()})).collect();
            Ok(
                json!({"resolved":route,"candidates":candidates,"token_budget":"ADVISORY","context_timeout_permissions":"ENFORCED","configuration":"resolved without launching providers"}),
            )
        };
        let mut invalid = false;
        let value = match args {
            ["roles"] => serde_json::to_value(&names)?,
            ["role", "show", name] => match inspect(name) {
                Ok(v) => v,
                Err(e) => {
                    invalid = true;
                    json!({"builtin":routing::builtin(name,&machine.runtime),"configuration_error":e.to_string()})
                }
            },
            ["route", "check"] => {
                let mut rows = vec![];
                for name in &names {
                    rows.push(match inspect(name) {
                        Ok(v) => v,
                        Err(e) => {
                            invalid = true;
                            json!({"role":name,"error":e.to_string()})
                        }
                    });
                }
                json!({"roles":rows})
            }
            ["route", name] => match inspect(name) {
                Ok(v) => v,
                Err(e) => {
                    invalid = true;
                    json!({"role":name,"error":e.to_string()})
                }
            },
            _ => {
                return Err(Error::Invalid(
                    "expected roles, role show <role>, route <role>, or route check".into(),
                ));
            }
        };
        let human = match args {
            ["roles"] => names.iter().cloned().collect::<Vec<_>>().join("\n"),
            ["route", "check"] => {
                let mut report = Report::default();
                for row in value["roles"].as_array().into_iter().flatten() {
                    route_report(&mut report, row);
                }
                report.to_string()
            }
            _ => {
                let mut report = Report::default();
                route_report(&mut report, &value);
                report.to_string()
            }
        };
        super::super::cli::output(json_mode, &value, &human)?;
        return require(
            !invalid,
            "routing validation failed; configure the reported roles in runtime.roles or runtime.profiles",
        );
    }
    require(
        overrides.is_empty() || matches!(args, ["run", "plan" | "resume" | "planner", _]),
        "--override is only supported for route inspection or worker execution",
    )?;
    require(
        overrides
            .keys()
            .all(|name| ["planner", "executor", "verifier"].contains(&name.as_str())),
        "run overrides must name an executable role: planner, executor, verifier",
    )?;
    if let ["provider", command] = args {
        require(
            ["list", "doctor"].contains(command),
            "expected provider list or provider doctor",
        )?;
        let mut rows = vec![];
        for (name, config) in &machine.runtime.providers {
            let exists = config.executable.is_file();
            let version = if *command == "doctor" && exists {
                // Explicit, token-free version query; never invoke a model prompt.
                Some(version(&config.executable)?)
            } else {
                None
            };
            let authentication = if *command == "doctor" && exists {
                Some(config.authentication.preflight(
                    &config.executable,
                    &super::credentials::NativeAuth::discover(&config.adapter)?,
                )?)
            } else {
                None
            };
            rows.push(json!({"provider":name,"adapter":config.adapter,"executable":config.executable,"exists":exists,"version":version,"authentication":authentication,"fresh_sessions":true,"sandbox_available":process::sandbox_available()}));
        }
        let mut report = Report::default();
        if rows.is_empty() {
            report
                .text("No providers configured")
                .next(["Add a [runtime.providers.NAME] entry to the machine config (see docs/providers.md)."]);
        }
        for row in &rows {
            report
                .section(format!(
                    "Provider {}",
                    row["provider"].as_str().unwrap_or("?")
                ))
                .field("Adapter", row["adapter"].as_str().unwrap_or("?"))
                .field(
                    "Executable",
                    format!(
                        "{}{}",
                        row["executable"].as_str().unwrap_or("?"),
                        if row["exists"] == true {
                            ""
                        } else {
                            "  (MISSING)"
                        }
                    ),
                )
                .field_opt("Version", row["version"].as_str());
            if let Some(auth) = row["authentication"].as_object() {
                report.field(
                    "Auth",
                    format!(
                        "{} ({})",
                        if auth["authenticated"] == true {
                            "AUTHENTICATED"
                        } else {
                            "NOT AUTHENTICATED"
                        },
                        auth["method"].as_str().unwrap_or("?")
                    ),
                );
                if auth["authenticated"] != true {
                    report.field_opt("Guidance", auth["guidance"].as_str());
                }
            }
            report.field("Sandbox", yes_no(row["sandbox_available"] == true));
        }
        return super::super::cli::output(json_mode, &rows, &report.to_string());
    }
    match args {
        ["run", "replace", old, new] => {
            let mut store = Store::open(&paths.database, machine.busy_timeout_ms)?;
            Runtime::new(
                &mut store,
                paths.clone(),
                machine.runtime.clone(),
                BTreeMap::new(),
            )?
            .replace(
                &root,
                &PlanId::new(*old).map_err(Error::Invalid)?,
                &PlanId::new(*new).map_err(Error::Invalid)?,
            )?;
            // `run replace` has no machine output: success is the exit status.
            if json_mode {
                return Ok(());
            }
            println!(
                "{}",
                crate::local::terminal::human(
                    &Report::new("Plan replaced")
                        .field("Plan", old)
                        .field("Replacement", format!("{new}  (VALIDATED, not active)"))
                        .next([
                            format!("agentctl plan activate {new}"),
                            format!("agentctl run plan {new}"),
                        ])
                        .to_string()
                )
            );
            Ok(())
        }
        ["run", "status", id] => {
            let store = Store::read_only(&paths.database, machine.busy_timeout_ms)?;
            let id = PlanId::new(*id).map_err(Error::Invalid)?;
            let run = store.runtime_status(&root, &id)?;
            let jobs = store.runtime_jobs(&root, Some(&id))?;
            let value = json!({"run":run,"jobs":jobs});
            let human = match &run {
                Some(run) => run_report(run, &jobs),
                None => Report::new("No run recorded")
                    .field("Plan", id.as_str())
                    .next([
                        format!("agentctl plan show {}", id.as_str()),
                        format!("agentctl run plan {}", id.as_str()),
                    ])
                    .to_string(),
            };
            super::super::cli::output(json_mode, &value, &human)
        }
        ["run", "capabilities", id] => {
            let store = Store::read_only(&paths.database, machine.busy_timeout_ms)?;
            let id = PlanId::new(*id).map_err(Error::Invalid)?;
            let artifacts = Artifacts::new(&paths.data_root.join("runtime/blobs"))?;
            let value = store.control_plane_capabilities(&artifacts, &root, &id)?;
            let mut report = Report::new("Capabilities");
            report
                .field("Plan", value.plan_id.as_str())
                .field("Plan state", name(&value.plan_state))
                .field(
                    "Generation",
                    value.accepted_generation_id.as_deref().unwrap_or("none"),
                );
            report.section("Findings");
            for finding in &value.capabilities {
                report.field(
                    &name(&finding.capability),
                    format!(
                        "{}  ({} evidence)",
                        name(&finding.status),
                        finding.evidence.len()
                    ),
                );
            }
            report.text("").text(format!(
                "Evidence and limitations: agentctl run capabilities {} --json",
                id.as_str()
            ));
            super::super::cli::output(json_mode, &value, &report.to_string())
        }
        ["run", "plan", id, "--dry-run"] => {
            let store = Store::read_only(&paths.database, machine.busy_timeout_ms)?;
            let id = PlanId::new(*id).map_err(Error::Invalid)?;
            let info = graph::checked_workspace(&store, &root)?;
            let ready = store
                .execution_tasks(&root, &id)?
                .into_iter()
                .filter(|t| t.structurally_ready)
                .collect::<Vec<_>>();
            let mut report = Report::new("Dry run");
            report.field("Plan",id.as_str()).field("Workspace",info.workspace_id.as_str()).field("Root",info.root.display()).field("Strategy","compatible executors use isolated Git worktrees; reconciliation and acceptance stay serialized");
            report.section("Roles");
            if machine.runtime.roles.is_empty() {
                report.text("none configured (agentctl route check)");
            }
            for (role, route) in &machine.runtime.roles {
                report.field(role, route_label(&serde_json::to_value(route)?));
            }
            report.section(format!("Ready tasks ({})", ready.len()));
            if ready.is_empty() {
                report.text("none");
            }
            for task in &ready {
                report.field(task.packet.task_id.as_str(), &task.packet.objective);
            }
            let value = json!({"workspace":info.workspace_id,"root":info.root,"strategy":"compatible executors use isolated Git worktrees; reconciliation and acceptance remain serialized","roles":machine.runtime.roles,"ready":ready,"compatibility":store.runtime_compatibility(&root,&id)?});
            report.text("").text(format!(
                "Compatibility analysis: agentctl run plan {} --dry-run --json",
                id.as_str()
            ));
            super::super::cli::output(json_mode, &value, &report.to_string())
        }
        ["run", "cancel", id] => {
            let mut store = Store::open(&paths.database, machine.busy_timeout_ms)?;
            let id = PlanId::new(*id).map_err(Error::Invalid)?;
            let state = Runtime::new(
                &mut store,
                paths.clone(),
                machine.runtime.clone(),
                BTreeMap::new(),
            )?
            .cancel(&root, &id)?;
            let value = json!({"plan_id":id,"run_state":state});
            super::super::cli::output(
                json_mode,
                &value,
                &match state {
                    RunState::Running => Report::new("Cancellation requested")
                        .field("Plan", id.as_str())
                        .field("Status", "RUNNING")
                        .text("The running controller stops its owned child processes.")
                        .next([format!("agentctl run status {}", id.as_str())])
                        .to_string(),
                    _ => Report::new("Plan cancelled")
                        .field("Plan", id.as_str())
                        .field("Status", name(&state))
                        .text("The workspace is free for a replacement plan.")
                        .next([
                            "agentctl plan prepare --objective TEXT".to_string(),
                            format!("agentctl run replace {} <replacement-plan-id>", id.as_str()),
                        ])
                        .to_string(),
                },
            )
        }
        // Discards a refused executor result. Never an acceptance: task states,
        // dependency locks and the ontology are untouched.
        ["run", "restore", id] => {
            let mut store = Store::open(&paths.database, machine.busy_timeout_ms)?;
            let id = PlanId::new(*id).map_err(Error::Invalid)?;
            let run = Runtime::new(
                &mut store,
                paths.clone(),
                machine.runtime.clone(),
                BTreeMap::new(),
            )?
            .restore(&root, &id)?;
            let value = serde_json::to_value(&run)?;
            super::super::cli::output(json_mode,&value,&Report::new("Refused result discarded").field("Plan",id.as_str()).field("Workspace","restored to the last verified source").field("Unchanged","task states, dependency locks, accepted ontology").next([format!("Prepare a replacement plan, then: agentctl run replace {} <replacement-plan-id>",id.as_str())]).to_string())
        }
        // The context relay: inspect it, then decide an escalated request. A
        // decision is an operator/planner document, never provider output.
        ["run", "context", "decide", id, file] => {
            let mut store = Store::open(&paths.database, machine.busy_timeout_ms)?;
            let bytes = source::read_file(Path::new(file), 64 * 1024)?;
            let decision: context::ContextDecision = serde_json::from_slice(&bytes)?;
            require(
                decision.plan_id.as_str() == *id,
                "decision names another plan",
            )?;
            let run = Runtime::new(
                &mut store,
                paths.clone(),
                machine.runtime.clone(),
                BTreeMap::new(),
            )?
            .decide_context(&root, &decision)?;
            let value = serde_json::to_value(&run)?;
            super::super::cli::output(json_mode, &value, &run_report(&run, &[]))
        }
        ["run", "context", id] => {
            let store = Store::read_only(&paths.database, machine.busy_timeout_ms)?;
            let artifacts = Artifacts::new(&paths.data_root.join("runtime/blobs"))?;
            let value = context::report(
                &store,
                &artifacts,
                &root,
                &PlanId::new(*id).map_err(Error::Invalid)?,
                &machine.runtime.context,
            )?;
            super::super::cli::output(json_mode, &value, &context_report(id, &value))
        }
        ["run", command, id] if ["plan", "resume", "planner"].contains(command) => {
            let mut store = Store::open(&paths.database, machine.busy_timeout_ms)?;
            let adapters: BTreeMap<String, Box<dyn ProviderAdapter>> = machine
                .runtime
                .providers
                .iter()
                .map(|(name, p)| {
                    let adapter: Box<dyn ProviderAdapter> = if p.adapter == "codex" {
                        Box::new(CodexAdapter {
                            executable: p.executable.clone(),
                            authentication: p.authentication.clone(),
                        })
                    } else {
                        Box::new(ClaudeAdapter {
                            executable: p.executable.clone(),
                            authentication: p.authentication.clone(),
                        })
                    };
                    (name.clone(), adapter)
                })
                .collect();
            let mut runtime =
                Runtime::new(&mut store, paths.clone(), machine.runtime.clone(), adapters)?
                    .with_role_overrides(overrides)?;
            if *command == "planner" {
                let view = runtime.plan(
                    &root,
                    &planning::PlanningRequestId::new(*id).map_err(Error::Invalid)?,
                )?;
                let plan = view.plan.packet.plan_id.as_str();
                let human = Report::new("Plan produced")
                    .field("Plan", plan)
                    .field("Status", name(&view.state))
                    .field("Tasks", view.plan.packet.tasks.len())
                    .field("Objective", &view.plan.packet.objective)
                    .next([
                        format!("agentctl plan show {plan}"),
                        format!("agentctl plan activate {plan}"),
                    ])
                    .to_string();
                super::super::cli::output(json_mode, &view, &human)
            } else {
                let plan = PlanId::new(*id).map_err(Error::Invalid)?;
                let run = runtime.run(&root, &plan)?;
                let jobs = store.runtime_jobs(&root, Some(&plan))?;
                super::super::cli::output(json_mode, &run, &run_report(&run, &jobs))
            }
        }
        _ => Err(Error::Invalid(
            "unrecognized run command; run agentctl run --help".into(),
        )),
    }
}
/// `provider[/model][ effort]` for a serialized RoleConfig.
fn route_label(route: &Value) -> String {
    let mut label = route["provider"].as_str().unwrap_or("?").to_string();
    if let Some(model) = route["model"].as_str() {
        label.push_str(&format!("/{model}"));
    }
    if let Some(effort) = route["effort"].as_str() {
        label.push_str(&format!(" (effort {effort})"));
    }
    label
}

/// One role's resolution (`role show`, `route <role>`, a `route check` row).
fn route_report(report: &mut Report, row: &Value) {
    let role = row["resolved"]["profile"]["role"]
        .as_str()
        .or(row["role"].as_str())
        .or(row["builtin"]["role"].as_str())
        .unwrap_or("?");
    report.section(format!("Role {role}"));
    if let Some(error) = row["error"]
        .as_str()
        .or(row["configuration_error"].as_str())
    {
        report
            .field("Status", "UNCONFIGURED")
            .field("Reason", error);
        return;
    }
    let resolved = &row["resolved"];
    report
        .field("Status", "OK")
        .field("Primary", route_label(&resolved["primary"]));
    let fallbacks: Vec<String> = resolved["fallbacks"]
        .as_array()
        .into_iter()
        .flatten()
        .map(route_label)
        .collect();
    report.field(
        "Fallbacks",
        if fallbacks.is_empty() {
            "none".into()
        } else {
            fallbacks.join(", ")
        },
    );
    let skipped: Vec<String> = resolved["policy_skipped"]
        .as_array()
        .into_iter()
        .flatten()
        .map(route_label)
        .collect();
    if !skipped.is_empty() {
        report.field(
            "Skipped",
            format!("{} (project policy)", skipped.join(", ")),
        );
    }
    let profile = &resolved["profile"];
    if let Some(ms) = profile["timeout_ms"].as_u64() {
        report.field("Timeout", format!("{}s", ms / 1000));
    }
    report
        .field("Network", yes_no(profile["network"] == true))
        .field("Read-only", yes_no(profile["read_only"] == true));
    let missing: Vec<&str> = row["candidates"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|c| c["executable_exists"] != true)
        .filter_map(|c| c["route"]["provider"].as_str())
        .collect();
    if !missing.is_empty() {
        report.field("Missing", format!("executable for {}", missing.join(", ")));
    }
}

/// Human view of a run: state, progress, the blocking reason (with its
/// canonical code), recent jobs, and what the operator can do next.
fn run_report(run: &RunRecord, jobs: &[RuntimeJob]) -> String {
    let plan = run.plan_id.as_str();
    let mut report = Report::new("Run");
    report
        .field("Plan", plan)
        .field("Status", name(&run.state))
        .field("Verified", format!("{} task(s)", run.accepted.len()));
    if let Some(pending) = &run.pending {
        report.field("Pending", pending.task_id.as_str());
    }
    if !run.branches.is_empty() {
        report.field(
            "Branches",
            run.branches
                .keys()
                .map(TaskId::as_str)
                .collect::<Vec<_>>()
                .join(", "),
        );
    }
    if run.correction_round > 0 {
        report.field("Correction", format!("round {}", run.correction_round));
    }
    report.field("Source", &run.expected.hash);
    if let Some(reason) = &run.reason {
        report.section(match run.state {
            RunState::Blocked => "Blocked",
            RunState::Cancelled => "Cancelled",
            _ => "Note",
        });
        report.reason(reason);
    }
    if !jobs.is_empty() {
        report.section("Jobs");
        for job in jobs.iter().rev().take(8).rev() {
            let mut line = format!("{:<9}  {:<11}", name(&job.role), name(&job.state));
            if let Some(task) = &job.task_id {
                line.push_str(&format!("  {}", task.as_str()));
            }
            if let Some(class) = &job.failure_class {
                line.push_str(&format!("  {}", name(class)));
            }
            report.field(job.job_id.as_str(), line);
        }
        if jobs.len() > 8 {
            report.text(format!(
                "… {} earlier job(s); agentctl run status {plan} --json",
                jobs.len() - 8
            ));
        }
    }
    let awaiting_context = run
        .context
        .values()
        .any(|l| l.state == context::LedgerState::NeedsPlannerContextApproval);
    let next: Vec<String> = match run.state {
        RunState::Complete => {
            vec!["Review the changes (git diff) and commit them before the next plan.".into()]
        }
        RunState::Running => vec![
            format!("agentctl run status {plan}"),
            format!("agentctl run cancel {plan}"),
        ],
        RunState::Cancelled => vec![format!(
            "Prepare a replacement plan, then: agentctl run replace {plan} <replacement-plan-id>"
        )],
        RunState::Blocked if awaiting_context => vec![
            format!("Review the escalated context request: agentctl run context {plan}"),
            format!("Then decide it: agentctl run context decide {plan} <decision.json>"),
        ],
        RunState::Blocked if run.refused.is_some() => vec![
            format!("Discard the refused result: agentctl run restore {plan}"),
            format!("Then replace the plan: agentctl run replace {plan} <replacement-plan-id>"),
        ],
        RunState::Blocked => vec![format!(
            "Prepare a replacement plan, then: agentctl run replace {plan} <replacement-plan-id>"
        )],
    };
    report.next(next);
    report.to_string()
}

/// Human view of the context relay report produced by `context::report`.
fn context_report(plan: &str, value: &Value) -> String {
    let mut report = Report::new("Context relay");
    report
        .field("Plan", plan)
        .field("Run", value["run_state"].as_str().unwrap_or("?"));
    if let Some(reason) = value["reason"].as_str() {
        report.reason(reason);
    }
    let subjects = value["subjects"].as_array().cloned().unwrap_or_default();
    if subjects.is_empty() {
        report.text("No context requests recorded.");
    }
    let mut pending = false;
    for subject in &subjects {
        report
            .section(format!(
                "Subject {}",
                subject["subject"].as_str().unwrap_or("?")
            ))
            .field("State", subject["state"].as_str().unwrap_or("?"))
            .field(
                "Rounds",
                format!("{}/{}", subject["rounds_used"], subject["max_rounds"]),
            )
            .field(
                "Granted",
                format!(
                    "{} bytes ({} remaining)",
                    subject["granted_bytes"], subject["task_bytes_remaining"]
                ),
            )
            .field("Escalations", &subject["escalations"]);
        let decision = &subject["pending_decision"];
        if decision.is_object() {
            pending = true;
            let paths: Vec<&str> = decision["outside_paths"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .collect();
            report.field("Requested", paths.join("\n")).field(
                "Approvable",
                if decision["approvable"] == true {
                    "yes".to_string()
                } else {
                    format!("no — {}", decision["detail"].as_str().unwrap_or(""))
                },
            );
        }
    }
    if pending {
        report.next([
            format!("Write a decision from the template in: agentctl run context {plan} --json"),
            format!("agentctl run context decide {plan} <decision.json>"),
        ]);
    }
    report.to_string()
}

fn version(executable: &Path) -> Result<String> {
    use std::{
        io::Read,
        process::{Command, Stdio},
        time::{Duration, Instant},
    };
    let mut child = Command::new(executable)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let started = Instant::now();
    loop {
        if let Some(status) = child.try_wait()? {
            require(status.success(), "provider version query failed")?;
            let mut text = String::new();
            if let Some(stdout) = child.stdout.take() {
                stdout.take(4096).read_to_string(&mut text)?;
            }
            return Ok(String::from_utf8_lossy(&super::credentials::redact(
                text.trim().as_bytes(),
            ))
            .into_owned());
        }
        if started.elapsed() > Duration::from_secs(5) {
            child.kill()?;
            child.wait()?;
            return Err(Error::Invalid("provider version query timed out".into()));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}
