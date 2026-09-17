use super::*;
use provider::{ClaudeAdapter, CodexAdapter, ProviderAdapter};
use serde_json::json;

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
        super::super::cli::output(json_mode, &value, &serde_json::to_string_pretty(&value)?)?;
        return require(!invalid, "routing validation failed; see diagnostics");
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
        return super::super::cli::output(json_mode, &rows, &serde_json::to_string_pretty(&rows)?);
    }
    match args {
        ["run", "replace", old, new] => {
            let mut store = Store::open(&paths.database, machine.busy_timeout_ms)?;
            Runtime::new(&mut store, paths.clone(), machine.runtime.clone(), BTreeMap::new())?.replace(&root, &PlanId::new(*old).map_err(Error::Invalid)?, &PlanId::new(*new).map_err(Error::Invalid)?)
        }
        ["run","status",id] => {
            let store=Store::read_only(&paths.database,machine.busy_timeout_ms)?;
            let id=PlanId::new(*id).map_err(Error::Invalid)?;
            let value=json!({"run":store.runtime_status(&root,&id)?,"jobs":store.runtime_jobs(&root,Some(&id))?});
            super::super::cli::output(json_mode,&value,&serde_json::to_string_pretty(&value)?)
        }
        ["run","plan",id,"--dry-run"] => {
            let store=Store::read_only(&paths.database,machine.busy_timeout_ms)?;
            let id=PlanId::new(*id).map_err(Error::Invalid)?;
            let info=graph::checked_workspace(&store,&root)?;
            let value=json!({"workspace":info.workspace_id,"root":info.root,"strategy":"serialized writers; no automatic worktrees/commits","roles":machine.runtime.roles,"ready":store.execution_tasks(&root,&id)?.into_iter().filter(|t|t.structurally_ready).collect::<Vec<_>>()});
            super::super::cli::output(json_mode,&value,&serde_json::to_string_pretty(&value)?)
        }
        ["run","cancel",id] => Store::open(&paths.database,machine.busy_timeout_ms)?.runtime_cancel(&root,&PlanId::new(*id).map_err(Error::Invalid)?),
        // The context relay: inspect it, then decide an escalated request. A
        // decision is an operator/planner document, never provider output.
        ["run","context","decide",id,file] => {
            let mut store=Store::open(&paths.database,machine.busy_timeout_ms)?;
            let bytes=source::read_file(Path::new(file),64*1024)?;
            let decision:context::ContextDecision=serde_json::from_slice(&bytes)?;
            require(decision.plan_id.as_str()==*id,"decision names another plan")?;
            let run=Runtime::new(&mut store,paths.clone(),machine.runtime.clone(),BTreeMap::new())?.decide_context(&root,&decision)?;
            let value=serde_json::to_value(&run)?;
            super::super::cli::output(json_mode,&value,&serde_json::to_string_pretty(&value)?)
        }
        ["run","context",id] => {
            let store=Store::read_only(&paths.database,machine.busy_timeout_ms)?;
            let artifacts=Artifacts::new(&paths.data_root.join("runtime/blobs"))?;
            let value=context::report(&store,&artifacts,&root,&PlanId::new(*id).map_err(Error::Invalid)?,&machine.runtime.context)?;
            super::super::cli::output(json_mode,&value,&serde_json::to_string_pretty(&value)?)
        }
        ["run",command,id] if ["plan","resume","planner"].contains(command) => {
            let mut store=Store::open(&paths.database,machine.busy_timeout_ms)?;
            let adapters:BTreeMap<String,Box<dyn ProviderAdapter>>=machine.runtime.providers.iter().map(|(name,p)| {
                let adapter:Box<dyn ProviderAdapter>=if p.adapter=="codex" {Box::new(CodexAdapter{executable:p.executable.clone(),authentication:p.authentication.clone()})}else{Box::new(ClaudeAdapter{executable:p.executable.clone(),authentication:p.authentication.clone()})};
                (name.clone(),adapter)
            }).collect();
            let mut runtime=Runtime::new(&mut store,paths.clone(),machine.runtime.clone(),adapters)?.with_role_overrides(overrides)?;
            let value=if *command=="planner" {serde_json::to_value(runtime.plan(&root,&planning::PlanningRequestId::new(*id).map_err(Error::Invalid)?)?)?}else{serde_json::to_value(runtime.run(&root,&PlanId::new(*id).map_err(Error::Invalid)?)?)?};
            super::super::cli::output(json_mode,&value,&serde_json::to_string_pretty(&value)?)
        }
        _=>Err(Error::Invalid("expected run planner <request-id>, run plan <plan-id> [--dry-run], run resume/status/cancel/context <plan-id>, run context decide <plan-id> <decision.json>, or provider list/doctor".into())),
    }
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
