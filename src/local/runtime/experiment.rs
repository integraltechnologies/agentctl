//! Stage 9A: durable long-running job/experiment process execution.
//!
//! An experiment is a plain OS process (no provider, no model call) launched under
//! the same sandboxed, argv-only process boundary as Stage 5 checks. It is not an
//! AgentInstance and has no EngineeringSession ownership: there is no AI conversation
//! to own. Liveness follows the unchanged Stage 6 philosophy: persisted RUNNING is a
//! historical fact, never proof that a process is still alive; only the exact
//! controller process that is currently polling a child handle may report LIVE.
use super::experiment_events::EventIngestor;
use super::*;
use process::{
    CancellationOutcome, CheckLauncher, NativeChecks, ProcessSpec, ScratchCleanup, WorkspaceLease,
};
use serde_json::json;
use std::time::Duration;

/// Hard sanity bound distinct from AI-role timeouts (Stage 7 caps those at one hour).
/// Experiments are not an AI role and may legitimately run far longer.
pub const MAX_TIMEOUT_MS: u64 = 30 * 24 * 60 * 60 * 1000;
pub const DEFAULT_TIMEOUT_MS: u64 = 24 * 60 * 60 * 1000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ExperimentState {
    Created,
    Running,
    Succeeded,
    Failed,
    Cancelled,
    Interrupted,
}
impl ExperimentState {
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Succeeded | Self::Failed | Self::Cancelled | Self::Interrupted
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExperimentAttempt {
    pub attempt: u32,
    pub pid: Option<u32>,
    pub started_at_ms: Option<u64>,
    pub finished_at_ms: Option<u64>,
    pub exit_status: Option<i32>,
    pub evidence: Option<EvidenceRef>,
    pub failure: Option<String>,
    /// A factual controller-side ingestion failure. It never changes process state
    /// and is separate from the process `failure` outcome.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event_ingestion_error: Option<String>,
    pub state: ExperimentState,
}

/// Durable record. Reuses the Stage 0 `CommandSpec` (structured argv, never a shell
/// string) and canonical `EvidenceRef`/`EvidenceRecord` conventions for captured output.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExperimentRun {
    pub experiment_id: ExperimentId,
    pub workspace_id: WorkspaceId,
    /// Reserved for future association with an EngineeringSession; Stage 9A never
    /// populates it; no AgentInstance/session ownership is created for a plain process.
    #[serde(default)]
    pub engineering_session_id: Option<String>,
    pub command: CommandSpec,
    /// Present only when the command came from `[commands.KEY]`. Restarts refuse
    /// to run the old argv if that project declaration no longer matches.
    #[serde(default)]
    pub project_command_key: Option<String>,
    /// Effective grant captured at creation. It is re-intersected with the current
    /// hard project denial on every explicit restart.
    #[serde(default)]
    pub network: bool,
    /// Environment variable NAMEs only; values are read from the controller's own
    /// process environment at launch, never accepted as inline text.
    #[serde(default)]
    pub env_passthrough: Vec<String>,
    pub timeout_ms: u64,
    pub created_at_ms: u64,
    pub state: ExperimentState,
    pub attempts: Vec<ExperimentAttempt>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExperimentObservation {
    pub run: ExperimentRun,
    /// Freshly computed at read time, never persisted. LIVE requires this exact
    /// process to hold the owned child handle; a separate CLI invocation always
    /// observes UNKNOWN, identically to Stage 6 runtime-job liveness.
    pub liveness: crate::local::observe::Liveness,
    pub events: ExperimentEventSummary,
}

pub struct ExperimentInput {
    pub command: CommandSpec,
    pub network: bool,
    pub env_passthrough: Vec<String>,
    pub timeout_ms: u64,
}

fn save_experiment(
    store: &mut Store,
    info: &RepositoryInfo,
    run: &ExperimentRun,
    phase: &str,
) -> Result<()> {
    let tx = store
        .connection
        .transaction_with_behavior(TransactionBehavior::Immediate)?;
    tx.execute(
        "INSERT INTO experiment_runs(experiment_id,repo_id,workspace_id,created_at_ms,record_json) VALUES (?1,?2,?3,?4,?5) \
         ON CONFLICT(experiment_id) DO UPDATE SET record_json=excluded.record_json",
        params![
            run.experiment_id.as_str(),
            info.repository_id.as_str(),
            info.workspace_id.as_str(),
            i64::try_from(run.created_at_ms).map_err(|e| Error::Invalid(e.to_string()))?,
            serde_json::to_string(run)?
        ],
    )?;
    let detail = run
        .attempts
        .last()
        .and_then(|a| a.failure.clone())
        .unwrap_or_default();
    store::append(
        &tx,
        &info.repository_id,
        now_ms()?,
        &Links::workspace(info.workspace_id.clone()),
        None,
        &JournalEntry::Experiment {
            experiment_id: run.experiment_id.clone(),
            phase: phase.into(),
            detail,
        },
    )?;
    tx.commit()?;
    Ok(())
}

fn journal_experiment(
    store: &mut Store,
    info: &RepositoryInfo,
    run: &ExperimentRun,
    phase: &str,
    detail: String,
) -> Result<()> {
    let tx = store
        .connection
        .transaction_with_behavior(TransactionBehavior::Immediate)?;
    store::append(
        &tx,
        &info.repository_id,
        now_ms()?,
        &Links::workspace(info.workspace_id.clone()),
        None,
        &JournalEntry::Experiment {
            experiment_id: run.experiment_id.clone(),
            phase: phase.into(),
            detail,
        },
    )?;
    tx.commit()?;
    Ok(())
}

fn load_experiment(
    store: &Store,
    info: &RepositoryInfo,
    id: &ExperimentId,
) -> Result<Option<ExperimentRun>> {
    let json: Option<String> = store
        .connection
        .query_row(
            "SELECT record_json FROM experiment_runs WHERE repo_id=?1 AND workspace_id=?2 AND experiment_id=?3",
            params![info.repository_id.as_str(), info.workspace_id.as_str(), id.as_str()],
            |r| r.get(0),
        )
        .optional()?;
    json.map(|s| serde_json::from_str(&s).map_err(Error::from))
        .transpose()
}

fn cancel_requested(store: &Store, info: &RepositoryInfo, id: &ExperimentId) -> Result<bool> {
    Ok(store
        .connection
        .query_row(
            "SELECT cancel_requested FROM experiment_runs WHERE repo_id=?1 AND experiment_id=?2",
            params![info.repository_id.as_str(), id.as_str()],
            |r| r.get(0),
        )
        .optional()?
        .unwrap_or(false))
}

fn is_live(store: &Store, info: &RepositoryInfo, id: &ExperimentId) -> bool {
    liveness::is_live(
        store.connection.path().unwrap_or(""),
        info.repository_id.as_str(),
        info.workspace_id.as_str(),
        "",
        "",
        id.as_str(),
    )
}

fn observation(
    store: &Store,
    info: &RepositoryInfo,
    run: ExperimentRun,
) -> Result<ExperimentObservation> {
    use crate::local::observe::Liveness;
    let liveness = if is_live(store, info, &run.experiment_id) {
        Liveness::Live
    } else {
        Liveness::Unknown
    };
    let events = store.experiment_event_summary(info, &run.experiment_id)?;
    Ok(ExperimentObservation {
        run,
        liveness,
        events,
    })
}

impl Store {
    pub fn experiment_status(
        &self,
        root: &Path,
        id: &ExperimentId,
    ) -> Result<Option<ExperimentObservation>> {
        let info = graph::checked_workspace(self, root)?;
        load_experiment(self, &info, id)?
            .map(|run| observation(self, &info, run))
            .transpose()
    }
    pub fn experiment_list(&self, root: &Path) -> Result<Vec<ExperimentObservation>> {
        let info = graph::checked_workspace(self, root)?;
        let rows: Vec<String> = self
            .connection
            .prepare(
                "SELECT record_json FROM experiment_runs WHERE repo_id=?1 AND workspace_id=?2 ORDER BY created_at_ms",
            )?
            .query_map(
                params![info.repository_id.as_str(), info.workspace_id.as_str()],
                |r| r.get(0),
            )?
            .collect::<std::result::Result<Vec<String>, _>>()?;
        rows.into_iter()
            .map(|s| observation(self, &info, serde_json::from_str(&s)?))
            .collect()
    }
    /// Requests cancellation from another process. Only sets a flag a live poll loop
    /// checks; it never claims a stale/unowned job has actually been terminated.
    pub fn experiment_cancel(&mut self, root: &Path, id: &ExperimentId) -> Result<()> {
        let info = graph::checked_workspace(self, root)?;
        let run = load_experiment(self, &info, id)?
            .ok_or_else(|| Error::Invalid("experiment not found".into()))?;
        require(
            run.state == ExperimentState::Running,
            "only a running experiment can be cancelled",
        )?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        require(
            tx.execute(
                "UPDATE experiment_runs SET cancel_requested=1 WHERE repo_id=?1 AND workspace_id=?2 AND experiment_id=?3",
                params![info.repository_id.as_str(), info.workspace_id.as_str(), id.as_str()],
            )? == 1,
            "experiment not found",
        )?;
        store::append(
            &tx,
            &info.repository_id,
            now_ms()?,
            &Links::workspace(info.workspace_id.clone()),
            None,
            &JournalEntry::Experiment {
                experiment_id: id.clone(),
                phase: "EXPERIMENT_CANCELLATION_REQUESTED".into(),
                detail: "effective only while a live controller is polling this experiment; a crashed controller cannot observe this flag".into(),
            },
        )?;
        tx.commit()?;
        Ok(())
    }
}

/// Rejects a cwd that escapes the workspace root before anything is persisted or
/// launched. Called both eagerly on acceptance and again at spec-build time.
fn resolve_cwd(info: &RepositoryInfo, command: &CommandSpec) -> Result<PathBuf> {
    let cwd = if command.cwd == "." {
        info.root.clone()
    } else {
        info.root.join(&command.cwd)
    };
    require(
        std::fs::canonicalize(&cwd)?.starts_with(&info.root),
        "experiment cwd escapes workspace",
    )?;
    Ok(cwd)
}

fn build_spec(
    paths: &paths::MachinePaths,
    info: &RepositoryInfo,
    policy: &ProjectConfig,
    run: &ExperimentRun,
    lease: &WorkspaceLease,
    attempt: u32,
) -> Result<ProcessSpec> {
    let scratch = paths
        .data_root
        .join("runtime/experiments/scratch")
        .join(run.experiment_id.as_str().replace(':', "-"));
    paths::ensure_directory(&scratch)?;
    let cwd = resolve_cwd(info, &run.command)?;
    Ok(ProcessSpec {
        project_policy_hash: Some(planning::hash(policy)?),
        native_auth: None,
        api_key: None,
        executable: PathBuf::from(&run.command.program),
        args: run.command.args.clone(),
        input: vec![],
        cwd,
        workspace: info.root.clone(),
        scratch: std::fs::canonicalize(&scratch)?,
        data_root: std::fs::canonicalize(&paths.data_root)?,
        config_root: std::fs::canonicalize(&paths.config_root)?,
        writable: true,
        network: run.network && !policy.routing.deny_network,
        timeout_ms: run.timeout_ms,
        git_directories: vec![info.git_directory.clone(), info.common_directory.clone()],
        protected: policy.protected.clone(),
        credential_env: run.env_passthrough.clone(),
        experiment_event_file: Some(scratch.join(format!("attempt-{attempt}-events.jsonl"))),
        lock_fd: lease.fd(),
    })
}

fn drive(
    store: &mut Store,
    paths: &paths::MachinePaths,
    launcher: &mut dyn CheckLauncher,
    artifacts: &Artifacts,
    info: &RepositoryInfo,
    policy: &ProjectConfig,
    run: &mut ExperimentRun,
) -> Result<()> {
    let lease = WorkspaceLease::acquire(
        &paths
            .data_root
            .join("runtime/locks")
            .join(info.workspace_id.as_str()),
    )?;
    let attempt_number = run.attempts.len() as u32 + 1;
    let spec = build_spec(paths, info, policy, run, &lease, attempt_number)?;
    let mut event_ingestor = EventIngestor::create(
        spec.experiment_event_file
            .as_ref()
            .expect("experiment specs have an event file"),
    )?;
    let _scratch = ScratchCleanup(spec.scratch.clone());
    run.state = ExperimentState::Running;
    run.attempts.push(ExperimentAttempt {
        attempt: attempt_number,
        pid: None,
        started_at_ms: None,
        finished_at_ms: None,
        exit_status: None,
        evidence: None,
        failure: None,
        event_ingestion_error: None,
        state: ExperimentState::Running,
    });
    save_experiment(store, info, run, "EXPERIMENT_ATTEMPT_STARTED")?;
    let mut process = match spec.recheck_policy().and_then(|_| launcher.launch(&spec)) {
        Ok(p) => p,
        Err(e) => {
            let attempt = run.attempts.last_mut().expect("just pushed");
            attempt.state = ExperimentState::Failed;
            attempt.failure = Some(e.to_string());
            attempt.finished_at_ms = Some(now_ms()?);
            run.state = ExperimentState::Failed;
            save_experiment(store, info, run, "EXPERIMENT_START_FAILED")?;
            return Err(e);
        }
    };
    {
        let attempt = run.attempts.last_mut().expect("just pushed");
        attempt.pid = process.pid();
        attempt.started_at_ms = Some(now_ms()?);
    }
    save_experiment(store, info, run, "EXPERIMENT_RUNNING")?;
    let db_path = store.connection.path().unwrap_or("").to_string();
    let guard = liveness::Guard::new(
        &db_path,
        info.repository_id.as_str(),
        info.workspace_id.as_str(),
        "",
        "",
        run.experiment_id.as_str(),
    );
    let mut cancellation_checked = false;
    let mut cancellation_applied = false;
    let mut cancellation_failure = None;
    let mut event_ingestion_error = None;
    let output = loop {
        guard.set(false);
        if let Err(error) = event_ingestor.drain(
            store,
            info,
            policy,
            &run.experiment_id,
            attempt_number,
            false,
        ) {
            event_ingestion_error = Some(format!("event ingestion retry pending: {error}"));
        }
        if !cancellation_checked && cancel_requested(store, info, &run.experiment_id)? {
            cancellation_checked = true;
            if let Some(output) = process.poll()? {
                journal_experiment(
                    store,
                    info,
                    run,
                    "EXPERIMENT_CANCELLATION_NOT_APPLIED",
                    "the owned process had already exited; its observed exit outcome is authoritative"
                        .into(),
                )?;
                break output;
            }
            match process.cancel()? {
                CancellationOutcome::Applied => {
                    cancellation_applied = true;
                    journal_experiment(
                        store,
                        info,
                        run,
                        "EXPERIMENT_CANCELLATION_APPLIED",
                        "termination was applied to the owned live process".into(),
                    )?;
                }
                CancellationOutcome::AlreadyExited => {
                    journal_experiment(
                        store,
                        info,
                        run,
                        "EXPERIMENT_CANCELLATION_NOT_APPLIED",
                        "the owned process had already exited; its observed exit outcome is authoritative".into(),
                    )?;
                }
                CancellationOutcome::Failed(error) => {
                    cancellation_failure = Some(error.clone());
                    journal_experiment(
                        store,
                        info,
                        run,
                        "EXPERIMENT_CANCELLATION_NOT_APPLIED",
                        format!(
                            "termination failed: {error}; the observed process exit remains authoritative"
                        ),
                    )?;
                }
            }
        } else if let Some(output) = process.poll()? {
            break output;
        }
        guard.set(process.liveness_confirmed());
        std::thread::sleep(Duration::from_millis(50));
    };
    drop(guard);
    loop {
        let drained = event_ingestor.drain(
            store,
            info,
            policy,
            &run.experiment_id,
            attempt_number,
            true,
        );
        let (_, caught_up) = match drained {
            Ok(result) => result,
            Err(error) => {
                event_ingestion_error = Some(format!("event ingestion failed: {error}"));
                break;
            }
        };
        if caught_up {
            break;
        }
    }
    let stdout = artifacts.put(&output.stdout)?;
    let stderr = artifacts.put(&output.stderr)?;
    let log = artifacts.json(&json!({
        "stdout": stdout, "stderr": stderr, "command": run.command,
        "runner": launcher.provenance(),
        "cancellation": {
            "requested": cancellation_checked,
            "applied": cancellation_applied,
            "failure": cancellation_failure,
        },
    }))?;
    let evidence_id = EvidenceId::new(format!(
        "evidence:{}-attempt-{attempt_number}",
        run.experiment_id.as_str()
    ))
    .map_err(Error::Invalid)?;
    let started_at_ms = run
        .attempts
        .last()
        .and_then(|a| a.started_at_ms)
        .unwrap_or(run.created_at_ms);
    store.record_evidence_in_workspace(
        &info.repository_id,
        &info.workspace_id,
        &EvidenceRecord {
            version: ProtocolVersion::V1,
            evidence_id: evidence_id.clone(),
            command: Some(run.command.clone()),
            source_state: None,
            started_at_ms,
            finished_at_ms: Some(now_ms()?),
            exit_status: output.exit,
            stdout_hash: Some(stdout.hash.clone()),
            stderr_hash: Some(stderr.hash.clone()),
            full_log_ref: Some(artifacts.path(&log)?.display().to_string()),
            summary: format!(
                "experiment {} attempt {attempt_number}: exit {:?}, failure {:?}",
                run.experiment_id.as_str(),
                output.exit,
                output.failure
            ),
        },
    )?;
    let finished_state = if cancellation_applied
        && output.failure.as_deref() == Some("cancelled")
        && output.exit != Some(0)
    {
        ExperimentState::Cancelled
    } else if output.exit == Some(0) && output.failure.is_none() {
        ExperimentState::Succeeded
    } else {
        ExperimentState::Failed
    };
    {
        let attempt = run.attempts.last_mut().expect("just pushed");
        attempt.finished_at_ms = Some(now_ms()?);
        attempt.exit_status = output.exit;
        attempt.evidence = Some(EvidenceRef(evidence_id));
        attempt.failure = output.failure.clone();
        attempt.event_ingestion_error = event_ingestion_error;
        attempt.state = finished_state;
    }
    run.state = finished_state;
    save_experiment(store, info, run, "EXPERIMENT_FINISHED")?;
    Ok(())
}

/// No LLM/provider involvement anywhere in this type: launch, poll, cancel, reopen
/// and restart are all plain OS-process supervision.
pub struct ExperimentRuntime<'a> {
    store: &'a mut Store,
    paths: paths::MachinePaths,
    artifacts: Artifacts,
    launcher: Box<dyn CheckLauncher + Send>,
}
impl<'a> ExperimentRuntime<'a> {
    pub fn new(store: &'a mut Store, paths: paths::MachinePaths) -> Result<Self> {
        let artifacts = Artifacts::new(&paths.data_root.join("runtime/blobs"))?;
        Ok(Self {
            store,
            paths,
            artifacts,
            launcher: Box::new(NativeChecks),
        })
    }
    pub fn with_check_launcher(mut self, launcher: Box<dyn CheckLauncher + Send>) -> Self {
        self.launcher = launcher;
        self
    }
    pub fn run(&mut self, root: &Path, input: ExperimentInput) -> Result<ExperimentRun> {
        self.start(
            root,
            ExperimentCommand::Explicit(input.command),
            input.network,
            input.env_passthrough,
            input.timeout_ms,
        )
    }
    pub fn run_project_command(
        &mut self,
        root: &Path,
        key: String,
        network: bool,
        env_passthrough: Vec<String>,
        timeout_ms: u64,
    ) -> Result<ExperimentRun> {
        self.start(
            root,
            ExperimentCommand::Project(key),
            network,
            env_passthrough,
            timeout_ms,
        )
    }
    fn start(
        &mut self,
        root: &Path,
        command: ExperimentCommand,
        network: bool,
        env_passthrough: Vec<String>,
        timeout_ms: u64,
    ) -> Result<ExperimentRun> {
        require(
            (1..=MAX_TIMEOUT_MS).contains(&timeout_ms),
            format!("experiment timeout_ms must be 1..={MAX_TIMEOUT_MS}"),
        )?;
        let info = graph::checked_workspace(self.store, root)?;
        let policy = ProjectConfig::load(&info.root)?;
        let (command, project_command_key) = match command {
            ExperimentCommand::Explicit(command) => (command, None),
            ExperimentCommand::Project(key) => {
                let command = policy.commands.get(&key).cloned().ok_or_else(|| {
                    Error::Invalid(format!(
                        "unknown project command {key}; declare it under [commands.{key}] in .agentctl/project.toml"
                    ))
                })?;
                (command, Some(key))
            }
        };
        command.validate()?;
        resolve_cwd(&info, &command)?;
        let hex: String =
            self.store
                .connection
                .query_row("SELECT lower(hex(randomblob(16)))", [], |r| r.get(0))?;
        let id = ExperimentId::new(format!("experiment:{hex}")).map_err(Error::Invalid)?;
        let _permit = auth::authorize(
            &self.store.connection,
            &info.repository_id,
            id.as_str(),
            id.as_str(),
        )?;
        let mut run = ExperimentRun {
            experiment_id: id,
            workspace_id: info.workspace_id.clone(),
            engineering_session_id: None,
            command,
            project_command_key,
            network: network && !policy.routing.deny_network,
            env_passthrough,
            timeout_ms,
            created_at_ms: now_ms()?,
            state: ExperimentState::Created,
            attempts: vec![],
        };
        save_experiment(self.store, &info, &run, "EXPERIMENT_CREATED")?;
        drive(
            self.store,
            &self.paths,
            self.launcher.as_mut(),
            &self.artifacts,
            &info,
            &policy,
            &mut run,
        )?;
        Ok(run)
    }
    /// Explicit operator action only; never automatic. Preserves every prior attempt.
    pub fn restart(&mut self, root: &Path, id: &ExperimentId) -> Result<ExperimentRun> {
        let info = graph::checked_workspace(self.store, root)?;
        let policy = ProjectConfig::load(&info.root)?;
        let mut run = load_experiment(self.store, &info, id)?
            .ok_or_else(|| Error::Invalid("experiment not found".into()))?;
        if let Some(key) = &run.project_command_key {
            require(
                policy.commands.get(key) == Some(&run.command),
                format!(
                    "SOURCE_DRIFT: project command {key} changed or was removed; create a new experiment to revalidate it"
                ),
            )?;
        }
        require(
            !(run.state == ExperimentState::Running && is_live(self.store, &info, id)),
            "experiment appears to still be actively supervised by a live controller; cancel it or wait for it to finish, then restart",
        )?;
        let _permit = auth::authorize(
            &self.store.connection,
            &info.repository_id,
            id.as_str(),
            id.as_str(),
        )?;
        if run.state == ExperimentState::Running {
            // Reconcile: an in-flight record from a crashed controller. Never guess
            // success; the interrupted attempt's outcome stays explicitly unproven.
            if let Some(last) = run.attempts.last_mut() {
                if !last.state.is_terminal() {
                    last.state = ExperimentState::Interrupted;
                    last.failure.get_or_insert_with(|| {
                        "controller interrupted; success unproven; no automatic retry".into()
                    });
                }
            }
            run.state = ExperimentState::Interrupted;
            save_experiment(self.store, &info, &run, "EXPERIMENT_INTERRUPTED")?;
        }
        require(
            self.store.connection.execute(
                "UPDATE experiment_runs SET cancel_requested=0 WHERE repo_id=?1 AND workspace_id=?2 AND experiment_id=?3",
                params![info.repository_id.as_str(), info.workspace_id.as_str(), id.as_str()],
            )? == 1,
            "experiment not found",
        )?;
        drive(
            self.store,
            &self.paths,
            self.launcher.as_mut(),
            &self.artifacts,
            &info,
            &policy,
            &mut run,
        )?;
        Ok(run)
    }
}

enum ExperimentCommand {
    Explicit(CommandSpec),
    Project(String),
}
