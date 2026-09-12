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
    pub reason: Option<String>,
    pub correction_round: u32,
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
pub(super) fn save_job(
    store: &mut Store,
    info: &RepositoryInfo,
    job: &RuntimeJob,
    phase: &str,
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
