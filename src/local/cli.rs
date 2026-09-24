use std::{env, fs, path::Path};

use serde::Serialize;
use serde_json::json;

use super::{
    Error, Result,
    config::{MachineConfig, ProjectConfig},
    paths::{self, MachinePaths, PathContext},
    repository::{RepositoryId, RepositoryInfo},
    require,
    store::Store,
    terminal::{Report, yes_no},
};
use crate::protocol::{JobId, TaskId};

pub fn run(args: &[&str]) -> Result<()> {
    let json_mode = args.last() == Some(&"--json");
    let args = if json_mode {
        &args[..args.len() - 1]
    } else {
        args
    };
    let paths = MachinePaths::resolve(&PathContext::from_env())?;
    match args {
        ["analytics", rest @ ..] => super::analytics::run(&paths, rest, json_mode),
        ["observe", rest @ ..] => super::observe::cli(&paths, rest, json_mode),
        ["init"] => {
            paths.create_directories()?;
            let config = MachineConfig::initialize(&paths.machine_config)?;
            let store = Store::open(&paths.database, config.busy_timeout_ms)?;
            output(
                json_mode,
                &json!({"paths": paths, "state": store.status()?}),
                &Report::new("Local state initialized")
                    .field("Config", paths.machine_config.display())
                    .field("Database", paths.database.display())
                    .next(["agentctl repo init    (inside a Git checkout)"])
                    .to_string(),
            )
        }
        ["doctor"] => doctor(&paths, json_mode),
        ["security", "doctor"] => security_doctor(&paths, json_mode),
        runtime @ (["run", ..]
        | ["provider", ..]
        | ["roles", ..]
        | ["role", ..]
        | ["route", ..]) => {
            let config = load_machine(&paths)?;
            super::runtime::cli::run(runtime, &config, &paths, json_mode)
        }
        ["plan", command, rest @ ..] => {
            let config = load_machine(&paths)?;
            let mut store = if [
                "prepare",
                "import",
                "validate",
                "activate",
                "cancel",
                "supersede",
            ]
            .contains(command)
            {
                Store::open(&paths.database, config.busy_timeout_ms)?
            } else {
                Store::read_only(&paths.database, config.busy_timeout_ms)?
            };
            super::planning::cli::run(&mut store, command, rest, json_mode)
        }
        ["experiment", command, rest @ ..] => {
            let config = load_machine(&paths)?;
            let mut store = if ["run", "cancel", "restart", "reconcile"].contains(command) {
                Store::open(&paths.database, config.busy_timeout_ms)?
            } else {
                Store::read_only(&paths.database, config.busy_timeout_ms)?
            };
            super::runtime::experiment_cli::run(
                &mut store,
                &paths,
                &config.runtime.security,
                command,
                rest,
                json_mode,
            )
        }
        ["memory", command, rest @ ..] => {
            let config = load_machine(&paths)?;
            let mut store = if ["add", "derive", "observe", "promote", "supersede", "reject"]
                .contains(command)
            {
                Store::open(&paths.database, config.busy_timeout_ms)?
            } else {
                Store::read_only(&paths.database, config.busy_timeout_ms)?
            };
            super::memory::cli::run(&mut store, command, rest, json_mode)
        }
        graph @ (["repo", "index" | "enrich", ..] | ["code", ..] | ["ontology", ..]) => {
            let config = load_machine(&paths)?;
            let mut store = if matches!(graph, ["repo", "index" | "enrich"])
                || matches!(graph, ["ontology", "accept" | "reject", ..])
            {
                Store::open(&paths.database, config.busy_timeout_ms)?
            } else {
                Store::read_only(&paths.database, config.busy_timeout_ms)?
            };
            super::graph::cli::run(&mut store, graph, json_mode)
        }
        ["repo", "init"] => {
            let config = load_machine(&paths)?;
            let info = RepositoryInfo::discover(&env::current_dir()?)?;
            // Validate database health before creating repository policy. A
            // missing database is not ill health on a fresh machine: the open
            // below creates it, so only an existing non-regular file refuses.
            paths::check_file(&paths.database, true)?;
            let mut store = Store::open(&paths.database, config.busy_timeout_ms)?;
            ProjectConfig::initialize(&info.root)?;
            let info = RepositoryInfo::discover(&info.root)?;
            let record = store.register_repository(info)?;
            output(
                json_mode,
                &record,
                &Report::new("Repository registered")
                    .field("Repository", record.info.repository_id.as_str())
                    .field("Workspace", record.info.workspace_id.as_str())
                    .field("Root", record.info.root.display())
                    .field("Config", paths::project_config(&record.info.root).display())
                    .next(["agentctl repo index"])
                    .to_string(),
            )
        }
        ["repo", "status"] => {
            let config = load_machine(&paths)?;
            let store = Store::read_only(&paths.database, config.busy_timeout_ms)?;
            let info = RepositoryInfo::discover(&env::current_dir()?)?;
            let repository = store.repository(&info.repository_id)?;
            let registered = store.workspace(&info.workspace_id)?;
            let project = match fs::symlink_metadata(info.root.join(".agentctl")) {
                Ok(_) => Some(ProjectConfig::load(&info.root)?),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                Err(e) => return Err(e.into()),
            };
            let conflicting_roots: Vec<_> = store
                .workspaces(None)?
                .into_iter()
                .filter(|w| w.info.root == info.root && w.info.workspace_id != info.workspace_id)
                .collect();
            let healthy = conflicting_roots.is_empty()
                && registered.as_ref().is_none_or(|w| {
                    w.info.root == info.root
                        && w.info.git_directory_identity == info.git_directory_identity
                })
                && repository.as_ref().is_none_or(|r| {
                    r.common_directory_identity
                        .as_ref()
                        .is_none_or(|id| Some(id) == info.common_directory_identity.as_ref())
                });
            let response = json!({"repository": repository, "repository_id": info.repository_id,
                "workspace_id": info.workspace_id, "workspace": info, "registered": registered.is_some(),
                "repository_registered": repository.is_some(), "registration": registered,
                "project_config_path": paths::project_config(&info.root), "project_config": project,
                "conflicting_registrations": conflicting_roots, "healthy": healthy, "database": store.status()?});
            output(json_mode, &response, &{
                let mut report = Report::new("Repository");
                report
                    .field("Repository", info.repository_id.as_str())
                    .field("Workspace", info.workspace_id.as_str())
                    .field("Root", info.root.display())
                    .field("Registered", yes_no(registered.is_some()))
                    .field("Config", if project.is_some() { "valid" } else { "absent" })
                    .field(
                        "HEAD",
                        info.source.head_commit.as_deref().unwrap_or("unborn"),
                    )
                    .field(
                        "Dirty",
                        format!(
                            "{} (working tree not fingerprinted)",
                            yes_no(info.source.dirty)
                        ),
                    )
                    .field("State", if healthy { "HEALTHY" } else { "MISMATCH" });
                if !healthy {
                    report.next([
                        "Registered metadata no longer matches this checkout.",
                        "Inspect with: agentctl repo list",
                    ]);
                } else if registered.is_none() || project.is_none() {
                    report.next(["agentctl repo init"]);
                }
                report.to_string()
            })?;
            require(
                healthy,
                "registered repository/workspace metadata no longer matches this checkout",
            )
        }
        ["repo", "list"] => {
            let config = load_machine(&paths)?;
            let store = Store::read_only(&paths.database, config.busy_timeout_ms)?;
            let mut rows = vec![];
            let mut human = vec![];
            for repository in store.repositories()? {
                let workspaces = store.workspaces(Some(&repository.repository_id))?;
                if !human.is_empty() {
                    human.push(String::new());
                }
                human.push(format!(
                    "{}  ({} workspace{})",
                    repository.repository_id.as_str(),
                    workspaces.len(),
                    if workspaces.len() == 1 { "" } else { "s" }
                ));
                let mut workspace_rows = vec![];
                for registered in workspaces {
                    let (health, detail) = match RepositoryInfo::discover(&registered.info.root) {
                        Ok(current)
                            if current.repository_id != repository.repository_id
                                || current.workspace_id != registered.info.workspace_id
                                || current.git_directory_identity
                                    != registered.info.git_directory_identity
                                || repository.common_directory_identity.as_ref().is_some_and(
                                    |id| Some(id) != current.common_directory_identity.as_ref(),
                                ) =>
                        {
                            (
                                "identity_changed",
                                Some("root now resolves to different Git metadata".to_string()),
                            )
                        }
                        Ok(current)
                            if current.remotes != repository.remotes
                                || current.source.head_commit
                                    != registered.info.source.head_commit
                                || current.source.dirty != registered.info.source.dirty =>
                        {
                            (
                                "metadata_changed",
                                Some(
                                    "run agentctl repo init to refresh observed metadata"
                                        .to_string(),
                                ),
                            )
                        }
                        Ok(_) => ("present", None),
                        Err(error) => ("unavailable", Some(error.to_string())),
                    };
                    human.push(format!(
                        "  {:<16}  {}\n  {:<16}  {}",
                        health.to_uppercase(),
                        registered.info.workspace_id.as_str(),
                        "",
                        registered.info.root.display()
                    ));
                    if let Some(detail) = &detail {
                        human.push(format!("  {:<16}  {detail}", ""));
                    }
                    workspace_rows.push(
                        json!({"registration": registered, "health": health, "detail": detail}),
                    );
                }
                rows.push(json!({"repository": repository, "workspaces": workspace_rows}));
            }
            let human = if human.is_empty() {
                "No registered repositories".to_string()
            } else {
                human.join("\n")
            };
            output(json_mode, &rows, &human)
        }
        ["state", "status"] => {
            let config = load_machine(&paths)?;
            let state = Store::read_only(&paths.database, config.busy_timeout_ms)?.status()?;
            output(
                json_mode,
                &state,
                &Report::new("Local state")
                    .field(
                        "Schema",
                        format!("{} ({})", state.schema_version, state.journal_mode),
                    )
                    .section("Records")
                    .field("Repositories", state.repositories)
                    .field("Workspaces", state.workspaces)
                    .field("Plans", state.plans)
                    .field("Tasks", state.tasks)
                    .field("Jobs", state.jobs)
                    .field("Evidence", state.evidence)
                    .field("Events", state.events)
                    .to_string(),
            )
        }
        ["events", "list", filters @ ..] => {
            let mut repo = None;
            let mut task = None;
            let mut job = None;
            let mut limit = 20;
            require(
                filters.len() % 2 == 0,
                "event filters require values: --repo ID --task ID --job ID --limit N",
            )?;
            for pair in filters.as_chunks::<2>().0 {
                match pair[0] {
                    "--repo" if repo.is_none() => {
                        repo = Some(
                            RepositoryId::try_from(pair[1].to_string()).map_err(Error::Invalid)?,
                        )
                    }
                    "--task" if task.is_none() => {
                        task = Some(TaskId::new(pair[1]).map_err(Error::Invalid)?)
                    }
                    "--job" if job.is_none() => {
                        job = Some(JobId::new(pair[1]).map_err(Error::Invalid)?)
                    }
                    "--limit" => {
                        limit = pair[1].parse().map_err(|_| {
                            Error::Invalid("--limit must be a number from 1 to 1000".into())
                        })?
                    }
                    _ => {
                        return Err(Error::Invalid(format!(
                            "unknown or repeated event filter {}",
                            pair[0]
                        )));
                    }
                }
            }
            require(
                repo.is_some() || (task.is_none() && job.is_none()),
                "--task/--job require --repo to disambiguate repository-scoped IDs",
            )?;
            let config = load_machine(&paths)?;
            let store = Store::read_only(&paths.database, config.busy_timeout_ms)?;
            let events = store.events(repo.as_ref(), task.as_ref(), job.as_ref(), limit)?;
            let human: Vec<_> = events
                .iter()
                .map(|event| {
                    let value =
                        serde_json::to_value(&event.entry).expect("validated finite journal entry");
                    let mut line = format!(
                        "{:>6}  {}  {}",
                        event.sequence,
                        event.timestamp_ms,
                        value["kind"].as_str().unwrap_or("UNKNOWN"),
                    );
                    if let Some(task) = &event.task_id {
                        line.push_str(&format!("  task {}", task.as_str()));
                    }
                    if let Some(job) = &event.job_id {
                        line.push_str(&format!("  job {}", job.as_str()));
                    }
                    line
                })
                .collect();
            let human = if human.is_empty() {
                "No events".to_string()
            } else {
                format!(
                    "{:>6}  {:<13}  EVENT\n{}",
                    "SEQ",
                    "TIME (ms)",
                    human.join("\n")
                )
            };
            output(json_mode, &events, &human)
        }
        _ => Err(Error::Invalid(format!(
            "unrecognized `{}` command; run agentctl {} --help (place --json last)",
            args.join(" "),
            args.first().unwrap_or(&"")
        ))),
    }
}

fn load_machine(paths: &MachinePaths) -> Result<MachineConfig> {
    // An uninitialized machine fails here first, so the directory check carries
    // the same guidance the config load already gives; otherwise the operator's
    // first command reports a raw missing-path error instead of the fix.
    paths::check_directory(&paths.config_root)
        .and_then(|()| MachineConfig::load(&paths.machine_config))
        .map_err(|e| Error::Invalid(format!("{e}; initialize missing state with agentctl init")))
}

#[derive(Serialize)]
struct Check {
    name: &'static str,
    ok: bool,
    detail: String,
}

fn doctor(paths: &MachinePaths, json_mode: bool) -> Result<()> {
    let mut checks = vec![];
    for (name, path) in [
        ("config_directory", &paths.config_root),
        ("data_directory", &paths.data_root),
        ("cache_directory", &paths.cache_root),
    ] {
        let result = check_permissions(path);
        checks.push(Check {
            name,
            ok: result.is_ok(),
            detail: result
                .err()
                .map(|e| e.to_string())
                .unwrap_or_else(|| path.display().to_string()),
        });
    }
    let config = load_machine(paths);
    checks.push(Check {
        name: "machine_config",
        ok: config.is_ok(),
        detail: config
            .as_ref()
            .err()
            .map(|e| e.to_string())
            .unwrap_or_else(|| paths.machine_config.display().to_string()),
    });
    let database = match config {
        Ok(config) => Store::read_only(&paths.database, config.busy_timeout_ms).and_then(|store| {
            require(
                !fs::metadata(&paths.database)?.permissions().readonly(),
                "database file is not writable",
            )?;
            store.status()
        }),
        Err(_) => Err(Error::Invalid(
            "database check skipped until machine config is repaired".into(),
        )),
    };
    checks.push(Check {
        name: "database",
        ok: database.is_ok(),
        detail: database
            .as_ref()
            .err()
            .map(|e| e.to_string())
            .unwrap_or_else(|| paths.database.display().to_string()),
    });
    let healthy = checks.iter().all(|c| c.ok);
    let mut report = Report::new("Local state");
    for check in &checks {
        report.field(
            check.name,
            format!(
                "{:<4}  {}",
                if check.ok { "OK" } else { "FAIL" },
                check.detail
            ),
        );
    }
    report
        .section("Result")
        .field("Status", if healthy { "HEALTHY" } else { "UNHEALTHY" });
    if !healthy {
        report.next([
            "Repair the failing path, config or database above, then run agentctl doctor again.",
            "A missing machine config or database is created by: agentctl init",
        ]);
    }
    let human = report.to_string();
    output(
        json_mode,
        &json!({"healthy": healthy, "checks": checks, "state": database.ok()}),
        &human,
    )?;
    require(
        healthy,
        "doctor found local state problems; repair the reported path/config/database before continuing",
    )
}

fn check_permissions(path: &Path) -> Result<()> {
    paths::check_directory(path)?;
    let metadata = fs::metadata(path)?;
    require(
        !metadata.permissions().readonly(),
        format!("{}: directory is not writable", path.display()),
    )?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        require(
            metadata.permissions().mode() & 0o111 != 0,
            format!("{}: directory is not searchable", path.display()),
        )?;
    }
    Ok(())
}

/// Local, deterministic security diagnostics: which isolation the host backend
/// enforces, whether any hard requirement is unsupported, and a live self-test.
/// No model call, no network request, and no secret values are printed.
fn security_doctor(paths: &MachinePaths, json_mode: bool) -> Result<()> {
    use super::security::{self, Capability, CapabilityStatus};
    let machine = load_machine(paths);
    let config = machine
        .as_ref()
        .map(|m| m.runtime.security.clone())
        .unwrap_or_default();
    let report = security::backend().capabilities();
    let baseline = security::baseline_enforced();
    let unsupported: Vec<Capability> = report
        .capabilities
        .iter()
        .filter(|c| match c.capability {
            Capability::FilesystemRead
            | Capability::FilesystemWrite
            | Capability::EnvironmentIsolation
            | Capability::NetworkDeny
            | Capability::CredentialIsolation => c.status != CapabilityStatus::Enforced,
            Capability::ProcessTree => c.status == CapabilityStatus::Unsupported,
            _ => false,
        })
        .map(|c| c.capability)
        .collect();
    let self_test = if baseline && !report.running_as_root {
        security::self_test(&config)
    } else {
        Err("skipped: baseline isolation is not enforced here; every worker is refused".into())
    };
    #[cfg(unix)]
    let owner_only = |path: &Path| {
        use std::os::unix::fs::PermissionsExt;
        fs::metadata(path)
            .ok()
            .map(|m| m.permissions().mode() & 0o077 == 0)
    };
    #[cfg(not(unix))]
    let owner_only = |_: &Path| None::<bool>;
    let value = json!({
        "backend": report.backend,
        "platform": report.platform,
        "running_as_root": report.running_as_root,
        "baseline_enforced": baseline,
        "unsupported_hard_requirements": unsupported,
        "capabilities": report.capabilities,
        "self_test": match &self_test { Ok(()) => "PASSED".to_string(), Err(e) => format!("FAILED: {e}") },
        "machine_config": machine.as_ref().map(|_| "loaded").unwrap_or("unavailable; defaults shown"),
        "machine_policy": {
            "read_roots": config.read_roots,
            "inherit_env": config.inherit_env,
            "explicit_env_names": config.env.keys().collect::<Vec<_>>(),
            "resources": config.resources,
            "experiment_events": config.experiment_events,
        },
        "default_read_roots": security::platform_read_roots(),
        "state_owner_only": {
            "data_directory": owner_only(&paths.data_root),
            "database": owner_only(&paths.database),
        },
        "trust_boundary": "worker processes are untrusted; same-user host compromise is outside the claimed boundary",
    });
    use super::terminal::name;
    let mut human = Report::new("Security");
    human
        .field(
            "Backend",
            format!("{} ({})", report.backend, report.platform),
        )
        .field_opt(
            "Root",
            report
                .running_as_root
                .then_some("RUNNING AS ROOT — every worker is refused"),
        );
    human.section("Capabilities");
    for c in &report.capabilities {
        human.field(
            &name(&c.capability),
            format!("{:<11}  {}", name(&c.status), c.mechanism),
        );
    }
    human
        .section("Result")
        .field(
            "Self-test",
            match &self_test {
                Ok(()) => {
                    "PASSED (planted secret unreadable; outside and .git writes denied)".into()
                }
                Err(e) => format!("FAILED — {e}"),
            },
        )
        .field(
            "Required",
            if unsupported.is_empty() {
                "all enforced".to_string()
            } else {
                format!(
                    "NOT ENFORCED: {} — affected jobs are refused before launch",
                    unsupported.iter().map(name).collect::<Vec<_>>().join(", ")
                )
            },
        );
    human.text("Capability details: agentctl security doctor --json");
    output(json_mode, &value, &human.to_string())?;
    require(
        baseline && unsupported.is_empty() && self_test.is_ok() && !report.running_as_root,
        "security doctor: required worker isolation is not enforced on this host; workers will be refused",
    )
}

pub(crate) fn output(json_mode: bool, value: &impl Serialize, human: &str) -> Result<()> {
    if json_mode {
        println!("{}", super::terminal::json(value)?);
    } else {
        println!("{}", super::terminal::human(human));
    }
    Ok(())
}
