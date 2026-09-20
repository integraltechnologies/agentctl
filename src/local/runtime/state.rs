use super::*;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum RunState {
    Running,
    Blocked,
    Complete,
    Cancelled,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AcceptedTask {
    pub executor: JobId,
    pub verifier: JobId,
    pub diff: ArtifactRef,
    pub evidence: Vec<EvidenceRef>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PendingTask {
    pub task_id: TaskId,
    pub executor: JobId,
    pub before: ArtifactRef,
    pub after: ArtifactRef,
    pub diff: ArtifactRef,
    pub evidence: Vec<EvidenceRef>,
    pub verifier: Option<JobId>,
    pub proof: Option<VerificationPacket>,
    /// Isolated mutation surface used by a concurrent executor. Serial results
    /// omit this. The path is controller-owned and is retained across an
    /// interruption until reconciliation or explicit replacement.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_workspace: Option<String>,
    /// Typed eligibility evidence frozen when this branch was dispatched.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub compatibility: Vec<super::concurrency::CompatibilityDecision>,
    /// Ontology generation against which executor context was issued.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ontology_generation: Option<String>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BatchLaunchAuthority {
    /// Exact canonical source observed while the task claims were committed.
    pub source: ArtifactRef,
    /// Accepted ontology row whose sequence/fingerprint backed compatibility.
    pub ontology_generation: String,
    pub tasks: Vec<TaskId>,
    pub compatibility: Vec<super::concurrency::CompatibilityDecision>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReconciliationPath {
    pub path: String,
    pub before: Option<source::FileState>,
    pub after: Option<source::FileState>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReconciliationIntent {
    pub task_id: TaskId,
    pub executor: JobId,
    pub diff: ArtifactRef,
    pub expected_source: ArtifactRef,
    pub intended_source: ArtifactRef,
    pub paths: Vec<ReconciliationPath>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunRecord {
    #[serde(default)]
    pub engineering_session: Option<EngineeringSession>,
    pub plan_id: PlanId,
    pub workspace_id: WorkspaceId,
    pub state: RunState,
    pub baseline: ArtifactRef,
    pub expected: ArtifactRef,
    pub policy_hash: String,
    pub accepted: BTreeMap<TaskId, AcceptedTask>,
    pub pending: Option<PendingTask>,
    /// Captured isolated results awaiting deterministic reconciliation. This is
    /// part of the RunRecord rather than scheduler state: a restarted
    /// controller can prove exactly what exists and must never duplicate it.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub branches: BTreeMap<TaskId, PendingTask>,
    /// Coherent authority checkpoint written in the same SQLite transaction as
    /// all claims in a concurrent batch. It is cleared after all branch
    /// results have been captured or contained.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub batch_authority: Option<BatchLaunchAuthority>,
    /// Branch-specific filesystem publication intent, durable before the first
    /// canonical write and cleared only with BRANCH_RECONCILED.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reconciliation: Option<ReconciliationIntent>,
    pub reason: Option<String>,
    pub correction_round: u32,
    /// Context-relay ledgers keyed by worker subject (`executor:<task>`,
    /// `verifier:<task>`, `integration`). Absent in runs recorded before the
    /// relay, and omitted while empty.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub context: BTreeMap<String, context::ContextLedger>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum RuntimeJobState {
    Queued,
    Running,
    Succeeded,
    Failed,
    Interrupted,
    Cancelled,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeJob {
    /// Physical mutation/read surface. `workspace_id` remains the canonical
    /// authority identity; this path makes isolation inspectable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_root: Option<String>,
    /// Planner jobs have no Stage 0 plan/job row. This is their sole usage
    /// observation; plan-associated jobs continue using canonical usage events.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub planner_usage: Option<TokenUsageEvent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reported_verification: Option<VerificationDecision>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub availability_failure: Option<routing::FailureClass>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub route: Option<routing::RouteSnapshot>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<prompt::PromptProvenance>,
    /// What agentctl intentionally supplied to this job, byte-accounted. Absent
    /// on jobs recorded before manifests existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_manifest: Option<manifest::ContextManifest>,
    /// The typed ContextRequest artifact this job returned instead of a result
    /// or decision. Such a job finished its protocol exchange but is never an
    /// accepted executor or a verification decision.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_request: Option<ArtifactRef>,
    #[serde(default)]
    pub ownership: Option<AgentOwnership>,
    #[serde(default)]
    pub task_id: Option<TaskId>,
    pub job_id: JobId,
    pub session_id: String,
    pub role: AgentRole,
    pub plan_id: Option<PlanId>,
    pub request_id: Option<planning::PlanningRequestId>,
    pub workspace_id: WorkspaceId,
    pub config: RoleConfig,
    pub state: RuntimeJobState,
    pub input: ArtifactRef,
    pub output: Option<ArtifactRef>,
    pub stdout: Option<ArtifactRef>,
    pub stderr: Option<ArtifactRef>,
    pub pid: Option<u32>,
    pub created_at_ms: u64,
    pub started_at_ms: Option<u64>,
    pub finished_at_ms: Option<u64>,
    pub failure: Option<String>,
}

pub(super) fn event(
    store: &mut Store,
    info: &RepositoryInfo,
    plan: Option<&PlanId>,
    job: Option<&JobId>,
    kind: &str,
    detail: &str,
) -> Result<()> {
    let canonical = job
        .map(|j| store.job(&info.repository_id, j))
        .transpose()?
        .flatten();
    let tx = store
        .connection
        .transaction_with_behavior(TransactionBehavior::Immediate)?;
    store::append(
        &tx,
        &info.repository_id,
        now_ms()?,
        &Links::runtime(info.workspace_id.clone(), plan.cloned(), canonical.as_ref()),
        None,
        &JournalEntry::Runtime {
            job_id: job.cloned(),
            phase: kind.into(),
            detail: detail.chars().take(1024).collect(),
        },
    )?;
    tx.commit()?;
    Ok(())
}
pub(super) fn save_run(
    store: &mut Store,
    info: &RepositoryInfo,
    run: &RunRecord,
    phase: &str,
) -> Result<()> {
    require(
        run.engineering_session.as_ref() == Some(&session::for_plan(store, info, &run.plan_id)?),
        "runtime run has missing or foreign engineering-session ownership",
    )?;
    require(
        store.connection.query_row(
            "SELECT agentctl_runtime_session_authorized(?1,?2,?3)",
            params![
                info.repository_id.as_str(),
                run.plan_id.as_str(),
                run.engineering_session
                    .as_ref()
                    .expect("checked session")
                    .id
            ],
            |r| r.get::<_, bool>(0),
        )?,
        "runtime session authorization required",
    )?;
    let tx = store
        .connection
        .transaction_with_behavior(TransactionBehavior::Immediate)?;
    tx.execute("INSERT INTO runtime_runs(repo_id,plan_id,workspace_id,record_json) VALUES (?1,?2,?3,?4) ON CONFLICT(repo_id,plan_id) DO UPDATE SET record_json=excluded.record_json", params![info.repository_id.as_str(),run.plan_id.as_str(),info.workspace_id.as_str(),serde_json::to_string(run)?])?;
    store::append(
        &tx,
        &info.repository_id,
        now_ms()?,
        &Links::planning(info.workspace_id.clone(), Some(run.plan_id.clone())),
        None,
        &JournalEntry::Runtime {
            job_id: None,
            phase: phase.into(),
            detail: run.reason.clone().unwrap_or_default(),
        },
    )?;
    tx.commit()?;
    Ok(())
}

/// Merge one isolated branch checkpoint without replacing sibling progress.
/// The read, duplicate check, merge, journal append and write share one
/// IMMEDIATE transaction, closing the lost-update race between executors.
pub(super) fn merge_branch(
    store: &mut Store,
    info: &RepositoryInfo,
    task: &TaskId,
    pending: PendingTask,
    executor_context: Option<context::ContextLedger>,
) -> Result<()> {
    let session = session::for_plan(store, info, &pending_task_plan(store, info, task)?)?;
    require(
        store.connection.query_row(
            "SELECT agentctl_runtime_session_authorized(?1,?2,?3)",
            params![
                info.repository_id.as_str(),
                pending_task_plan(store, info, task)?.as_str(),
                session.id
            ],
            |row| row.get::<_, bool>(0),
        )?,
        "runtime session authorization required",
    )?;
    let tx = store
        .connection
        .transaction_with_behavior(TransactionBehavior::Immediate)?;
    let json: String = tx.query_row(
        "SELECT record_json FROM runtime_runs WHERE repo_id=?1 AND plan_id=(SELECT plan_id FROM tasks WHERE repo_id=?1 AND task_id=?2)",
        params![info.repository_id.as_str(), task.as_str()],
        |row| row.get(0),
    )?;
    let mut run: RunRecord = serde_json::from_str(&json)?;
    require(
        !run.accepted.contains_key(task) && run.pending.as_ref().is_none_or(|p| &p.task_id != task),
        "stale branch cannot replace accepted/pending task state",
    )?;
    match run.branches.get(task) {
        Some(existing) => require(
            existing.executor == pending.executor && existing.diff == pending.diff,
            "a different branch result already owns this task",
        )?,
        None => {
            run.branches.insert(task.clone(), pending);
        }
    }
    if let Some(ledger) = executor_context {
        let key = context::subject_key(AgentRole::Executor, Some(task));
        match run.context.get(&key) {
            Some(existing) => require(
                existing.base == ledger.base && existing.rounds == ledger.rounds,
                "concurrent context ledger conflict",
            )?,
            None => {
                run.context.insert(key, ledger);
            }
        }
    }
    tx.execute(
        "UPDATE runtime_runs SET record_json=?1 WHERE repo_id=?2 AND plan_id=?3",
        params![
            serde_json::to_string(&run)?,
            info.repository_id.as_str(),
            run.plan_id.as_str()
        ],
    )?;
    store::append(
        &tx,
        &info.repository_id,
        now_ms()?,
        &Links::planning(info.workspace_id.clone(), Some(run.plan_id.clone())),
        None,
        &JournalEntry::Runtime {
            job_id: Some(run.branches[task].executor.clone()),
            phase: "BRANCH_RESULT_CAPTURED".into(),
            detail: task.as_str().into(),
        },
    )?;
    tx.commit()?;
    Ok(())
}

fn pending_task_plan(store: &Store, info: &RepositoryInfo, task: &TaskId) -> Result<PlanId> {
    let value: String = store.connection.query_row(
        "SELECT plan_id FROM tasks WHERE repo_id=?1 AND task_id=?2",
        params![info.repository_id.as_str(), task.as_str()],
        |row| row.get(0),
    )?;
    PlanId::new(value).map_err(Error::Invalid)
}
pub(super) fn save_job(
    store: &mut Store,
    info: &RepositoryInfo,
    job: &RuntimeJob,
    phase: &str,
) -> Result<()> {
    save_job_impl(store, info, job, phase, None)
}
/// Sole choke point that admits a brand-new runtime job (any role: planner,
/// executor, verifier, integration verifier). Counts only currently active
/// (QUEUED/RUNNING) jobs machine-wide -- historical/completed jobs never
/// count -- and refuses admission over `max_agents` instead of launching.
/// The count and the admitting insert share one IMMEDIATE transaction, so
/// concurrent callers across processes/workspaces are serialized by SQLite's
/// writer lock and cannot race past the cap.
pub(super) fn create_job(
    store: &mut Store,
    info: &RepositoryInfo,
    job: &RuntimeJob,
    max_agents: usize,
) -> Result<()> {
    save_job_impl(store, info, job, "JOB_CREATED", Some(max_agents))
}
fn save_job_impl(
    store: &mut Store,
    info: &RepositoryInfo,
    job: &RuntimeJob,
    phase: &str,
    capacity: Option<usize>,
) -> Result<()> {
    session::validate_job(store, info, job)?;
    require(
        store.connection.query_row(
            "SELECT agentctl_runtime_session_authorized(?1,?2,?3)",
            params![
                info.repository_id.as_str(),
                job.plan_id.as_ref().map(PlanId::as_str).or_else(|| job
                    .request_id
                    .as_ref()
                    .map(planning::PlanningRequestId::as_str)),
                job.ownership
                    .as_ref()
                    .expect("validated ownership")
                    .engineering_session_id
            ],
            |r| r.get::<_, bool>(0),
        )?,
        "worker session authorization required",
    )?;
    let canonical = store.job(&info.repository_id, &job.job_id)?;
    let tx = store
        .connection
        .transaction_with_behavior(TransactionBehavior::Immediate)?;
    if let Some(limit) = capacity {
        let active: i64 = tx.query_row(
            "SELECT COUNT(*) FROM runtime_jobs WHERE json_extract(record_json,'$.state') IN ('QUEUED','RUNNING')",
            [],
            |r| r.get(0),
        )?;
        if active as usize >= limit {
            return Err(Error::CapacityExceeded {
                active: active as usize,
                limit,
            });
        }
    }
    tx.execute("INSERT INTO runtime_jobs(job_id,repo_id,workspace_id,plan_id,request_id,record_json) VALUES (?1,?2,?3,?4,?5,?6) ON CONFLICT(job_id) DO UPDATE SET record_json=excluded.record_json", params![job.job_id.as_str(),info.repository_id.as_str(),info.workspace_id.as_str(),job.plan_id.as_ref().map(PlanId::as_str),job.request_id.as_ref().map(planning::PlanningRequestId::as_str),serde_json::to_string(job)?])?;
    store::append(
        &tx,
        &info.repository_id,
        now_ms()?,
        &Links::runtime(
            info.workspace_id.clone(),
            job.plan_id.clone(),
            canonical.as_ref(),
        ),
        None,
        &JournalEntry::Runtime {
            job_id: Some(job.job_id.clone()),
            phase: phase.into(),
            detail: job.failure.clone().unwrap_or_default(),
        },
    )?;
    tx.commit()?;
    Ok(())
}
pub(super) fn load_run(
    store: &Store,
    info: &RepositoryInfo,
    id: &PlanId,
) -> Result<Option<RunRecord>> {
    let json: Option<String> = store.connection.query_row("SELECT record_json FROM runtime_runs WHERE repo_id=?1 AND plan_id=?2 AND workspace_id=?3", params![info.repository_id.as_str(),id.as_str(),info.workspace_id.as_str()], |r| r.get(0)).optional()?;
    json.map(|s| serde_json::from_str(&s).map_err(Error::from))
        .transpose()
}
impl Store {
    pub fn runtime_status(&self, root: &Path, id: &PlanId) -> Result<Option<RunRecord>> {
        let info = graph::checked_workspace(self, root)?;
        load_run(self, &info, id)
    }
    pub fn runtime_jobs(&self, root: &Path, plan: Option<&PlanId>) -> Result<Vec<RuntimeJob>> {
        let info = graph::checked_workspace(self, root)?;
        self.connection.prepare("SELECT record_json FROM runtime_jobs WHERE repo_id=?1 AND workspace_id=?2 AND (?3 IS NULL OR plan_id=?3) ORDER BY job_id")?.query_map(params![info.repository_id.as_str(),info.workspace_id.as_str(),plan.map(PlanId::as_str)], |r| r.get::<_,String>(0))?.map(|s| Ok(serde_json::from_str(&s?)?)).collect()
    }
    pub fn runtime_cancel(&mut self, root: &Path, id: &PlanId) -> Result<()> {
        let info = graph::checked_workspace(self, root)?;
        let run = load_run(self, &info, id)?
            .ok_or_else(|| Error::Invalid("runtime plan not found".into()))?;
        if run.state == RunState::Cancelled {
            return Ok(());
        }
        require(
            run.state == RunState::Running,
            "only a running plan can be cancelled",
        )?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        require(tx.execute("UPDATE runtime_runs SET cancel_requested=1 WHERE repo_id=?1 AND workspace_id=?2 AND plan_id=?3",params![info.repository_id.as_str(),info.workspace_id.as_str(),id.as_str()])? == 1,"runtime plan not found")?;
        store::append(
            &tx,
            &info.repository_id,
            now_ms()?,
            &Links::planning(info.workspace_id, Some(id.clone())),
            None,
            &JournalEntry::Runtime {
                job_id: None,
                phase: "CANCELLATION_REQUESTED".into(),
                detail: "controller will stop the owned child family".into(),
            },
        )?;
        tx.commit()?;
        Ok(())
    }
}
