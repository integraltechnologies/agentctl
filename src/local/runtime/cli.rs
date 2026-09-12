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
        ["run",command,id] if ["plan","resume","planner"].contains(command) => {
            let mut store=Store::open(&paths.database,machine.busy_timeout_ms)?;
            let adapters:BTreeMap<String,Box<dyn ProviderAdapter>>=machine.runtime.providers.iter().map(|(name,p)| {
                let adapter:Box<dyn ProviderAdapter>=if p.adapter=="codex" {Box::new(CodexAdapter{executable:p.executable.clone(),authentication:p.authentication.clone()})}else{Box::new(ClaudeAdapter{executable:p.executable.clone(),authentication:p.authentication.clone()})};
                (name.clone(),adapter)
            }).collect();
            let mut runtime=Runtime::new(&mut store,paths.clone(),machine.runtime.clone(),adapters)?;
            let value=if *command=="planner" {serde_json::to_value(runtime.plan(&root,&planning::PlanningRequestId::new(*id).map_err(Error::Invalid)?)?)?}else{serde_json::to_value(runtime.run(&root,&PlanId::new(*id).map_err(Error::Invalid)?)?)?};
            super::super::cli::output(json_mode,&value,&serde_json::to_string_pretty(&value)?)
        }
        _=>Err(Error::Invalid("expected run planner <request-id>, run plan <plan-id> [--dry-run], run resume/status/cancel <plan-id>, or provider list/doctor".into())),
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
