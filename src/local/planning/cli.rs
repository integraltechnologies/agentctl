use super::*;
use crate::local::terminal::{Report, name};
use std::{env, fs::OpenOptions, io::Read};

pub(crate) fn read(path: &str, limit: u64) -> Result<String> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options.open(path)?;
    require(
        file.metadata()?.is_file() && file.metadata()?.len() <= limit,
        "planning input must be a bounded regular file",
    )?;
    let mut text = String::new();
    file.take(limit + 1).read_to_string(&mut text)?;
    require(text.len() as u64 <= limit, "planning input exceeds limit")?;
    Ok(text)
}
pub(crate) fn run(store: &mut Store, command: &str, args: &[&str], json: bool) -> Result<()> {
    let root = env::current_dir()?;
    if command == "prepare" {
        require(args.len().is_multiple_of(2), "prepare flags require values")?;
        let mut flags = BTreeMap::new();
        for pair in args.as_chunks::<2>().0 {
            require(
                [
                    "--objective",
                    "--objective-file",
                    "--request-file",
                    "--verify",
                    "--query",
                    "--bytes",
                    "--primary",
                    "--neighbors",
                    "--tests",
                    "--files",
                    "--canonical",
                    "--facts",
                    "--notes",
                    "--excerpt-bytes",
                    "--excerpt-lines",
                ]
                .contains(&pair[0]),
                format!("unknown prepare flag {}", pair[0]),
            )?;
            require(
                flags.insert(pair[0], pair[1]).is_none(),
                "repeated prepare flag",
            )?;
        }
        require(
            ["--objective", "--objective-file", "--request-file"]
                .iter()
                .filter(|k| flags.contains_key(**k))
                .count()
                == 1,
            "choose one objective, objective-file or request-file",
        )?;
        let mut intent = if let Some(path) = flags.get("--request-file") {
            serde_json::from_str(&read(path, 16384)?)?
        } else {
            RequestDraft {
                objective: if let Some(s) = flags.get("--objective") {
                    s.to_string()
                } else {
                    read(flags["--objective-file"], 4096)?
                },
                query: None,
                scope: vec![],
                constraints: vec![],
                definition_of_done: vec![],
                verification: None,
                invariant_refs: vec![],
                provenance: PlanningProvenance {
                    actor: "local-cli".into(),
                    source_refs: vec!["explicit-user-objective".into()],
                    provider: None,
                },
            }
        };
        if let Some(q) = flags.get("--query") {
            intent.query = Some(q.to_string());
        }
        // Every entry path must yield a request `run planner` can plan: the
        // checks each task and the integration are judged by are part of it.
        if let Some(keys) = flags.get("--verify") {
            require(
                intent.verification.is_none(),
                "--verify conflicts with the request file's own verification",
            )?;
            intent.verification = Some(VerificationRequirements {
                requirement_refs: keys
                    .split(',')
                    .map(|k| k.trim().to_string())
                    .filter(|k| !k.is_empty())
                    .collect(),
                evidence_required: true,
            });
        }
        if intent.verification.is_none() {
            let policy = ProjectConfig::load(&root)?;
            let declared: Vec<String> = policy.verification.keys().cloned().collect();
            match declared.as_slice() {
                [only] => {
                    intent.verification = Some(VerificationRequirements {
                        requirement_refs: vec![only.clone()],
                        evidence_required: true,
                    })
                }
                [] => {
                    return Err(Error::Invalid(
                        "plan prepare: the project declares no [verification.KEY] profile, so no plan could be verified; declare one in .agentctl/project.toml".into(),
                    ));
                }
                many => {
                    return Err(Error::Invalid(format!(
                        "plan prepare: choose the checks the plan is judged by with --verify KEY[,KEY] (declared: {})",
                        many.join(", ")
                    )));
                }
            }
        }
        let mut limits = PlanningLimits::default();
        for (name, target) in [
            ("--bytes", &mut limits.bytes),
            ("--primary", &mut limits.graph.primary),
            ("--neighbors", &mut limits.graph.neighbors),
            ("--tests", &mut limits.graph.tests),
            ("--files", &mut limits.files),
            ("--canonical", &mut limits.memory.canonical),
            ("--facts", &mut limits.memory.facts),
            ("--notes", &mut limits.memory.notes),
            ("--excerpt-bytes", &mut limits.excerpt_bytes),
            ("--excerpt-lines", &mut limits.excerpt_lines),
        ] {
            if let Some(s) = flags.get(name) {
                *target = s
                    .parse()
                    .map_err(|_| Error::Invalid(format!("invalid numeric {name}")))?;
            }
        }
        let p = store.prepare_plan(&root, intent, limits)?;
        return output(json, &p, &{
            let id = p.request.request_id.as_str();
            Report::new("Planning request prepared")
                .field("Request", id)
                .field("Objective", first_line(&p.request.intent.objective))
                .field(
                    "Verify",
                    p.request
                        .intent
                        .verification
                        .as_ref()
                        .map(|v| v.requirement_refs.join(", "))
                        .unwrap_or_default(),
                )
                .field(
                    "Context",
                    format!(
                        "{} bytes{}",
                        p.serialized_bytes,
                        if p.context.truncated {
                            " (truncated to limits)"
                        } else {
                            ""
                        }
                    ),
                )
                .next([
                    format!("agentctl run planner {id}"),
                    format!("Inspect the frozen planner input: agentctl plan context {id} --json"),
                ])
                .to_string()
        });
    }
    if command == "list" {
        let mut all = false;
        let mut limit = 20;
        let mut rest = args;
        while let Some((first, tail)) = rest.split_first() {
            match *first {
                "--all" if !all => {
                    all = true;
                    rest = tail;
                }
                "--limit" if tail.len() == 1 => {
                    limit = tail[0]
                        .parse()
                        .map_err(|_| Error::Invalid("invalid plan limit".into()))?;
                    rest = &[];
                }
                _ => return Err(Error::Invalid("plan list [--all] [--limit N]".into())),
            }
        }
        let list = store.execution_plans(&root, all, limit)?;
        let human = if list.is_empty() {
            format!(
                "No {}plans in this workspace",
                if all { "" } else { "validated or active " }
            )
        } else {
            let width = list
                .iter()
                .map(|p| p.plan_id.as_str().len())
                .max()
                .unwrap_or(0);
            let mut lines = vec![format!("{:<width$}  {:<10}  OBJECTIVE", "PLAN", "STATUS")];
            for p in &list {
                lines.push(format!(
                    "{:<width$}  {:<10}  {}",
                    p.plan_id.as_str(),
                    name(&p.state),
                    first_line(&p.objective)
                ));
            }
            lines.join("\n")
        };
        return output(json, &list, &human);
    }
    if command == "import" {
        require(args.len() == 1, "plan import requires one JSON file")?;
        let plan: ExecutionPlan = serde_json::from_str(&read(args[0], 262144)?)?;
        let view = store.import_execution_plan(&root, &plan)?;
        return output(
            json,
            &view,
            &plan_report(
                &format!("Plan imported ({} bytes)", size(&view.plan)?),
                &view,
            ),
        );
    }
    if command == "context" {
        require(
            args.len() == 1 || args.len() == 2 && args[1] == "--manifest",
            "plan context <request-id> [--manifest]",
        )?;
        let packet = store.planning_context(
            &root,
            &PlanningRequestId::new(args[0]).map_err(Error::Invalid)?,
        )?;
        if args.len() == 2 {
            // Derived deterministically from the immutable packet; describes it
            // without copying repository content.
            let manifest = crate::local::runtime::manifest::for_packet(&packet)?;
            return output(json, &manifest, &serde_json::to_string_pretty(&manifest)?);
        }
        let id = packet.request.request_id.as_str();
        let human = Report::new("Planning request")
            .field("Request", id)
            .field("Objective", first_line(&packet.request.intent.objective))
            .field(
                "Context",
                format!(
                    "{} bytes{}",
                    packet.serialized_bytes,
                    if packet.context.truncated {
                        " (truncated to limits)"
                    } else {
                        ""
                    }
                ),
            )
            .text("")
            .text(format!(
                "Frozen planner input:  agentctl plan context {id} --json"
            ))
            .text(format!(
                "Context manifest:      agentctl plan context {id} --manifest"
            ))
            .to_string();
        return output(json, &packet, &human);
    }
    let id = PlanId::new(
        *args
            .first()
            .ok_or_else(|| Error::Invalid("plan command requires an ID".into()))?,
    )
    .map_err(Error::Invalid)?;
    match command {
        "supersede" => {
            require(
                args.len() == 3 && args[1] == "--with",
                "plan supersede OLD --with NEW",
            )?;
            store.supersede_execution_plan(
                &root,
                &id,
                &PlanId::new(args[2]).map_err(Error::Invalid)?,
            )?;
        }
        "cancel" => {
            require(
                args.len() == 3 && args[1] == "--reason",
                "plan cancel ID --reason TEXT",
            )?;
            store.cancel_execution_plan(&root, &id, args[2])?;
        }
        "activate" | "validate" | "show" | "export" | "tasks" | "ready" | "blocked" => {
            require(args.len() == 1, "unexpected plan arguments")?;
            if command == "activate" {
                store.activate_execution_plan(&root, &id)?;
            }
            if command == "validate" {
                store.validate_execution_plan(&root, &id)?;
            }
            if ["tasks", "ready", "blocked"].contains(&command) {
                let mut tasks = store.execution_tasks(&root, &id)?;
                if command == "ready" {
                    tasks.retain(|t| t.structurally_ready);
                }
                if command == "blocked" {
                    tasks.retain(|t| !t.structurally_ready && t.state != TaskState::Verified);
                }
                let human = if tasks.is_empty() {
                    format!("No {command} tasks")
                } else {
                    let mut report = Report::default();
                    for t in &tasks {
                        report
                            .section(format!("Task {}", t.packet.task_id.as_str()))
                            .field("Status", name(&t.state))
                            .field(
                                "Ready",
                                crate::local::terminal::yes_no(t.structurally_ready),
                            )
                            .field("Objective", first_line(&t.packet.objective));
                        if !t.packet.dependencies.is_empty() {
                            report.field(
                                "Depends",
                                t.packet
                                    .dependencies
                                    .iter()
                                    .map(|d| d.as_str())
                                    .collect::<Vec<_>>()
                                    .join(", "),
                            );
                        }
                        if !t.reasons.is_empty() {
                            report.field("Waiting", t.reasons.join("\n"));
                        }
                    }
                    report.to_string()
                };
                return output(json, &tasks, &human);
            }
        }
        _ => {
            return Err(Error::Invalid(
                "unknown planning command; run agentctl --help".into(),
            ));
        }
    }
    let view = store.execution_plan(&root, &id)?;
    if command == "export" {
        // A document for `plan import`, not a report: JSON in both modes.
        return output(json, &view.plan, &serde_json::to_string_pretty(&view.plan)?);
    }
    let title = match command {
        "validate" => "Plan validated",
        "activate" => "Plan activated",
        "supersede" => "Plan superseded",
        "cancel" => "Plan cancelled",
        _ => "Plan",
    };
    output(json, &view, &plan_report(title, &view))
}

/// The first line of operator/planner text, bounded for one-line display.
fn first_line(text: &str) -> String {
    let line = text.lines().next().unwrap_or("").trim();
    if line.chars().count() > 96 {
        format!("{}…", line.chars().take(95).collect::<String>())
    } else {
        line.to_string()
    }
}

fn plan_report(title: &str, view: &ExecutionPlanView) -> String {
    let packet = &view.plan.packet;
    let plan = packet.plan_id.as_str();
    let mut report = Report::new(title);
    report
        .field("Plan", plan)
        .field("Status", name(&view.state))
        .field("Objective", first_line(&packet.objective))
        .field("Request", view.plan.metadata.request_id.as_str())
        .field_opt(
            "Replaces",
            view.plan
                .metadata
                .replan
                .as_ref()
                .map(|r| r.previous_plan_id.as_str()),
        )
        .field_opt(
            "Superseded",
            view.superseded_by.as_ref().map(PlanId::as_str),
        )
        .field_opt(
            "Integration",
            view.integration_proof.as_ref().map(|p| name(&p.decision)),
        )
        .field_opt(
            "Final source",
            view.final_source.as_ref().map(|s| s.revision.as_str()),
        );
    report.section(format!("Tasks ({})", packet.tasks.len()));
    for task in &packet.tasks {
        report.field(task.task_id.as_str(), first_line(&task.objective));
    }
    match view.state {
        PlanState::Validated => {
            report.next([format!("agentctl plan activate {plan}")]);
        }
        PlanState::Active => {
            report.next([
                format!("agentctl run plan {plan} --dry-run"),
                format!("agentctl run plan {plan}"),
            ]);
        }
        _ => {}
    }
    report.to_string()
}
fn output(json: bool, value: &impl serde::Serialize, human: &str) -> Result<()> {
    if json {
        println!("{}", crate::local::terminal::json(value)?);
    } else {
        println!("{}", crate::local::terminal::human(human));
    }
    Ok(())
}
