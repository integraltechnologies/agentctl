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
};
use crate::protocol::{JobId, TaskId};

pub fn run(args: &[&str]) -> Result<()> {
    if args == ["run", "packet-hashes"] {
        return super::runtime::planner::packet_hashes();
    }
    let json_mode = args.last() == Some(&"--json");
    let args = if json_mode {
        &args[..args.len() - 1]
    } else {
        args
    };
    let paths = MachinePaths::resolve(&PathContext::from_env())?;
    match args {
        ["analytics", rest @ ..] => super::analytics::run(&paths, rest, json_mode),
        ["observe", rest @ ..] => super::observe::cli(&paths, rest),
        ["init"] => {
            paths.create_directories()?;
            let config = MachineConfig::initialize(&paths.machine_config)?;
            let store = Store::open(&paths.database, config.busy_timeout_ms)?;
            output(
                json_mode,
                &json!({"paths": paths, "state": store.status()?}),
                &format!("Initialized local state at {}", paths.database.display()),
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
            let mut store = if ["run", "cancel", "restart"].contains(command) {
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
        graph @ (["repo", "index", ..] | ["code", ..] | ["ontology", ..]) => {
            let config = load_machine(&paths)?;
            let mut store = if graph == ["repo", "index"]
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
            // Validate database health before creating repository policy.
            paths::check_file(&paths.database, false)?;
            let mut store = Store::open(&paths.database, config.busy_timeout_ms)?;
            ProjectConfig::initialize(&info.root)?;
            let info = RepositoryInfo::discover(&info.root)?;
            let record = store.register_repository(info)?;
            output(
                json_mode,
                &record,
                &format!(
                    "Repository: {}\nRegistered workspace: {}\nRoot: {}\nProject config: {}",
                    record.info.repository_id.as_str(),
                    record.info.workspace_id.as_str(),
                    record.info.root.display(),
                    paths::project_config(&record.info.root).display()
                ),
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
            output(
                json_mode,
                &response,
                &format!(
                    "Repository: {}\nWorkspace: {}\nRoot: {}\nWorkspace registered: {}\nProject config: {}\nHEAD: {}\nDirty: {} (working tree not fingerprinted)\nLocal state: {}",
                    info.repository_id.as_str(),
                    info.workspace_id.as_str(),
                    info.root.display(),
                    registered.is_some(),
                    if project.is_some() {
                        "valid"
                    } else {
                        "absent; run agentctl repo init"
                    },
                    info.source.head_commit.as_deref().unwrap_or("unborn"),
                    info.source.dirty,
                    if healthy {
                        "healthy"
                    } else {
                        "metadata mismatch; inspect registration"
                    }
                ),
            )?;
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
                human.push(format!(
                    "{}  {} workspace(s)",
                    repository.repository_id.as_str(),
                    workspaces.len()
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
                        "  {}  {}  {health}",
                        registered.info.workspace_id.as_str(),
                        registered.info.root.display()
                    ));
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
                &format!(
                    "Schema {} ({})\n{} repositories, {} workspaces, {} plans, {} tasks, {} jobs, {} evidence records, {} events",
                    state.schema_version,
                    state.journal_mode,
                    state.repositories,
                    state.workspaces,
                    state.plans,
                    state.tasks,
                    state.jobs,
                    state.evidence,
                    state.events
                ),
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
                    format!(
                        "{}  {}  {}  task={} job={}",
                        event.sequence,
                        event.timestamp_ms,
                        value["kind"].as_str().unwrap_or("UNKNOWN"),
                        event.task_id.as_ref().map(TaskId::as_str).unwrap_or("-"),
                        event.job_id.as_ref().map(JobId::as_str).unwrap_or("-")
                    )
                })
                .collect();
            let human = if human.is_empty() {
                "No events".to_string()
            } else {
                human.join("\n")
            };
            output(json_mode, &events, &human)
        }
        _ => Err(Error::Invalid(
            "invalid local command; run agentctl --help (place --json last)".into(),
        )),
    }
}

fn load_machine(paths: &MachinePaths) -> Result<MachineConfig> {
    paths::check_directory(&paths.config_root)?;
    MachineConfig::load(&paths.machine_config)
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
    let human = checks
        .iter()
        .map(|check| {
            format!(
                "{} {}: {}",
                if check.ok { "ok" } else { "FAIL" },
                check.name,
                check.detail
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
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
    let mut human = vec![format!(
        "Security backend: {} ({}){}",
        report.backend,
        report.platform,
        if report.running_as_root {
            " — RUNNING AS ROOT: workers refused"
        } else {
            ""
        }
    )];
    for c in &report.capabilities {
        human.push(format!(
            "{:<12} {:<27} {} — {}",
            serde_json::to_value(c.status)?.as_str().unwrap_or_default(),
            serde_json::to_value(c.capability)?
                .as_str()
                .unwrap_or_default(),
            c.mechanism,
            c.detail
        ));
    }
    human.push(match &self_test {
        Ok(()) => {
            "Self-test: PASSED (planted secret unreadable, outside/.git writes denied)".into()
        }
        Err(e) => format!("Self-test: FAILED: {e}"),
    });
    human.push(if unsupported.is_empty() {
        "Hard requirements: all enforced".into()
    } else {
        format!("Hard requirements NOT enforced: {unsupported:?} — affected jobs are refused before launch")
    });
    output(json_mode, &value, &human.join("\n"))?;
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
