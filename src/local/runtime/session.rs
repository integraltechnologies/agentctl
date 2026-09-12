//! Runtime ownership, not provider authentication or reusable conversation state.
use super::*;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EngineeringSession {
    pub id: String,
    pub supervisor_instance_id: AgentId,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum AgentLifetime {
    SessionNative,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentOwnership {
    pub engineering_session_id: String,
    pub agent_instance_id: AgentId,
    pub parent_agent_instance_id: Option<AgentId>,
    pub lifetime: AgentLifetime,
}
impl AgentOwnership {
    /// No caller-selectable session/persistence override for implicit children.
    pub fn child(&self, instance: AgentId) -> Self {
        Self {
            engineering_session_id: self.engineering_session_id.clone(),
            agent_instance_id: instance,
            parent_agent_instance_id: Some(self.agent_instance_id.clone()),
            lifetime: AgentLifetime::SessionNative,
        }
    }
}
impl EngineeringSession {
    pub fn worker(&self, instance: AgentId) -> AgentOwnership {
        AgentOwnership {
            engineering_session_id: self.id.clone(),
            agent_instance_id: instance,
            parent_agent_instance_id: Some(self.supervisor_instance_id.clone()),
            lifetime: AgentLifetime::SessionNative,
        }
    }
}
pub(super) fn for_request(
    info: &RepositoryInfo,
    request: &planning::PlanningRequestId,
) -> Result<EngineeringSession> {
    let id = format!(
        "engineering:{}",
        planning::hash(&(
            info.repository_id.clone(),
            info.workspace_id.clone(),
            request
        ))?
    );
    Ok(EngineeringSession {
        supervisor_instance_id: AgentId::new(format!("supervisor:{id}")).map_err(Error::Invalid)?,
        id,
    })
}
pub(super) fn for_plan(
    store: &Store,
    info: &RepositoryInfo,
    plan: &PlanId,
) -> Result<EngineeringSession> {
    let mut next = plan.clone();
    let mut visited = BTreeSet::new();
    loop {
        require(
            visited.insert(next.clone()) && visited.len() <= 64,
            "invalid engineering-session replan lineage",
        )?;
        let view = store.execution_plan(&info.root, &next)?;
        if let Some(prior) = view.plan.metadata.replan {
            next = prior.previous_plan_id;
        } else {
            return for_request(info, &view.plan.metadata.request_id);
        }
    }
}
pub(super) fn issued(
    store: &Store,
    info: &RepositoryInfo,
    plan: Option<&PlanId>,
    request: Option<&planning::PlanningRequestId>,
    task: Option<&TaskId>,
    role: AgentRole,
    job: &JobId,
) -> Result<AgentOwnership> {
    let session = if let Some(plan) = plan {
        for_plan(store, info, plan)?
    } else {
        for_request(
            info,
            request
                .ok_or_else(|| Error::Invalid("worker has no engineering undertaking".into()))?,
        )?
    };
    let instance = AgentId::new(format!("agent:{}", job.as_str())).map_err(Error::Invalid)?;
    let jobs = store.runtime_jobs(&info.root, None)?;
    // The accepted plan is the supervisor's request for the default topology.
    // Verifiers are children of completed executors, not resumed conversations.
    let parent = jobs
        .iter()
        .filter(|j| j.state == RuntimeJobState::Succeeded)
        .find(|j| {
            j.ownership
                .as_ref()
                .is_some_and(|o| o.engineering_session_id == session.id)
                && if role == AgentRole::Verifier && task.is_some() {
                    j.role == AgentRole::Executor
                        && j.plan_id.as_ref() == plan
                        && j.task_id.as_ref() == task
                } else {
                    j.role == AgentRole::Planner
                }
        });
    Ok(parent
        .and_then(|j| j.ownership.as_ref())
        .map(|o| o.child(instance.clone()))
        .unwrap_or_else(|| session.worker(instance)))
}
pub(super) fn validate_job(store: &Store, info: &RepositoryInfo, job: &RuntimeJob) -> Result<()> {
    let expected = if let Some(plan) = &job.plan_id {
        for_plan(store, info, plan)?
    } else {
        for_request(
            info,
            job.request_id
                .as_ref()
                .ok_or_else(|| Error::Invalid("runtime request missing".into()))?,
        )?
    };
    let owner=job.ownership.as_ref().ok_or_else(||Error::Invalid("legacy runtime job has no engineering-session ownership; cannot resume; retain history and replan".into()))?;
    require(
        owner.engineering_session_id == expected.id
            && owner.agent_instance_id.as_str() == format!("agent:{}", job.job_id.as_str()),
        "runtime job belongs to a different engineering session/agent instance",
    )?;
    let parent = owner
        .parent_agent_instance_id
        .as_ref()
        .ok_or_else(|| Error::Invalid("session-native worker requires an owning parent".into()))?;
    require(
        parent == &expected.supervisor_instance_id
            || store.runtime_jobs(&info.root, None)?.iter().any(|j| {
                j.ownership.as_ref().is_some_and(|o| {
                    &o.agent_instance_id == parent && o.engineering_session_id == expected.id
                })
            }),
        "worker parent belongs to a different engineering session",
    )
}
