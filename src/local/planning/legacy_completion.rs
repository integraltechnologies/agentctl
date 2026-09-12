//! Read-only v5 migration preflight. Historical completion must be justified by
//! durable data, not today's filesystem/policy or a synthesized replacement audit.
use super::*;
use crate::local::repository::{RepositoryId, WorkspaceId};

pub(in crate::local) fn validate(c: &Connection) -> Result<()> {
    let mut rows = c.prepare(
        "SELECT repo_id,plan_id,workspace_id,integration_json,final_source_json FROM execution_plans WHERE state='COMPLETE' ORDER BY repo_id,plan_id",
    )?;
    let mut rows = rows.query([])?;
    while let Some(row) = rows.next()? {
        let repo: String = row.get(0)?;
        let plan: String = row.get(1)?;
        let result = (|| {
            let repo = RepositoryId::try_from(repo.clone()).map_err(Error::Invalid)?;
            let id = PlanId::new(plan.clone()).map_err(Error::Invalid)?;
            let workspace =
                WorkspaceId::try_from(row.get::<_, String>(2)?).map_err(Error::Invalid)?;
            let proof: VerificationPacket = serde_json::from_str(&row.get::<_, String>(3)?)
                .map_err(|e| {
                    Error::Invalid(format!("invalid persisted integration verification: {e}"))
                })?;
            let source: SourceStateRef = serde_json::from_str(&row.get::<_, String>(4)?)
                .map_err(|e| Error::Invalid(format!("invalid persisted final source: {e}")))?;
            validate_plan(c, &repo, &workspace, &id, &proof, &source)
        })();
        result.map_err(|e: Error| {
            // Deserialization errors can include untrusted field names; keep the
            // diagnostic useful without dumping large legacy payload fragments.
            let reason: String = e.to_string().chars().take(1024).collect();
            Error::Invalid(format!(
                "schema v5→v6 migration aborted: legacy COMPLETE plan {plan} (repository {repo}) could not be validated: {reason}; no changes committed. Review/restore the legacy completion records from trusted history before retrying; no automatic repair was performed"
            ))
        })?;
    }
    Ok(())
}

fn validate_plan(
    c: &Connection,
    repo: &RepositoryId,
    workspace: &WorkspaceId,
    id: &PlanId,
    proof: &VerificationPacket,
    source: &SourceStateRef,
) -> Result<()> {
    source.validate()?;
    let packet = store::plan(c, repo, id)?
        .ok_or_else(|| Error::Invalid("canonical PlanPacket is missing".into()))?;
    require(
        packet.plan_id == *id,
        "PlanPacket ID differs from its storage key",
    )?;
    let states = store::task_states(c, repo, id)?;
    packet.validate_completion(&states, proof)?;

    let mut audit = c.prepare("SELECT sequence,workspace_id,task_id,job_id,entry_json FROM events WHERE repo_id=?1 AND plan_id=?2 AND json_extract(entry_json,'$.kind')='EXECUTION_PLAN_COMPLETED' ORDER BY sequence")?;
    let mut audits = audit.query(params![repo.as_str(), id.as_str()])?;
    let row = audits
        .next()?
        .ok_or_else(|| Error::Invalid("matching completion audit is missing".into()))?;
    let completion_sequence: i64 = row.get(0)?;
    require(
        row.get::<_, Option<String>>(1)?.as_deref() == Some(workspace.as_str())
            && row.get::<_, Option<String>>(2)?.is_none()
            && row.get::<_, Option<String>>(3)?.is_none(),
        "completion audit has incorrect workspace/task/job ownership",
    )?;
    let entry: JournalEntry = serde_json::from_str(&row.get::<_, String>(4)?)?;
    require(
        matches!(entry, JournalEntry::ExecutionPlanCompleted { plan_id, verification, source: recorded }
            if plan_id == *id && verification == *proof && recorded == *source),
        "completion audit does not match the stored plan, integration proof and final source",
    )?;
    require(audits.next()?.is_none(), "multiple completion audits exist")?;

    // Replay the recorded task transitions using the unchanged Stage 0 guards.
    // This proves packet PASS/check/invariant/evidence requirements and dependency
    // ordering, rather than trusting a raw VERIFIED row or an isolated event label.
    let mut replay: BTreeMap<_, _> = packet
        .tasks
        .iter()
        .map(|t| (t.task_id.clone(), TaskState::Planned))
        .collect();
    let mut executors = BTreeSet::new();
    let mut changes = c.prepare("SELECT sequence,task_id,workspace_id,job_id,entry_json FROM events WHERE repo_id=?1 AND plan_id=?2 AND json_extract(entry_json,'$.kind')='TASK_STATE_CHANGED' ORDER BY sequence")?;
    let mut changes = changes.query(params![repo.as_str(), id.as_str()])?;
    while let Some(row) = changes.next()? {
        require(
            row.get::<_, i64>(0)? < completion_sequence,
            "task transition occurs after completion audit",
        )?;
        let task_id = TaskId::new(row.get::<_, String>(1)?).map_err(Error::Invalid)?;
        let entry: JournalEntry = serde_json::from_str(&row.get::<_, String>(4)?)?;
        let JournalEntry::TaskStateChanged {
            from,
            to,
            verification,
        } = entry
        else {
            return Err(Error::Invalid("invalid task transition audit".into()));
        };
        require(
            replay.get(&task_id) == Some(&from),
            format!(
                "task {} history has an inconsistent prior state",
                task_id.as_str()
            ),
        )?;
        packet.validate_task_transition(&task_id, &replay, to, verification.as_ref())?;
        if let Some(v) = &verification {
            let task = packet
                .tasks
                .iter()
                .find(|t| t.task_id == task_id)
                .ok_or_else(|| Error::Invalid("audit references an unknown task".into()))?;
            store::validate_verifier(
                c,
                repo,
                &store::StoredTask {
                    plan_id: id.clone(),
                    packet: task.clone(),
                    state: from,
                },
                v,
            )?;
            require(
                row.get::<_, Option<String>>(2)?.as_deref() == Some(workspace.as_str())
                    && row.get::<_, Option<String>>(3)?.as_deref()
                        == Some(v.verifier_job_id.as_str()),
                "packet verification audit has incorrect workspace/job ownership",
            )?;
            if to == TaskState::Verified {
                if let VerificationTarget::Packet {
                    executor_job_id, ..
                } = &v.target
                {
                    executors.insert(executor_job_id.clone());
                }
            }
        }
        replay.insert(task_id, to);
    }
    require(
        replay == states,
        "persisted task states are not justified by task verification history",
    )?;
    let VerificationTarget::Integration {
        executor_job_ids, ..
    } = &proof.target
    else {
        return Err(Error::Invalid("expected integration verification".into()));
    };
    require(
        executors.len() == packet.tasks.len()
            && executors == executor_job_ids.iter().cloned().collect(),
        "integration executor set does not match accepted packet verifications",
    )?;
    let verifier = store::job(c, repo, &proof.verifier_job_id)?
        .ok_or_else(|| Error::Invalid("integration verifier is missing".into()))?;
    require(
        verifier.role == AgentRole::Verifier
            && verifier.plan_id == *id
            && verifier.task_id.is_none()
            && verifier.state == JobState::Succeeded,
        "integration requires a successful plan-level verifier job",
    )?;
    let verifier_workspace: Option<String> = c.query_row(
        "SELECT workspace_id FROM jobs WHERE repo_id=?1 AND job_id=?2",
        params![repo.as_str(), proof.verifier_job_id.as_str()],
        |r| r.get(0),
    )?;
    require(
        verifier_workspace.as_deref() == Some(workspace.as_str()),
        "integration verifier belongs to a different or unbound workspace",
    )?;
    store::validate_evidence(c, repo, &proof.evidence)?;
    require(
        !proof.evidence.is_empty(),
        "integration requires final evidence",
    )?;
    for e in &proof.evidence {
        let (json, owner): (String, Option<String>) = c.query_row(
            "SELECT record_json,workspace_id FROM evidence WHERE repo_id=?1 AND evidence_id=?2",
            params![repo.as_str(), e.0.as_str()],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        let evidence: EvidenceRecord = serde_json::from_str(&json)?;
        require(
            evidence.source_state.as_ref() == Some(source)
                && owner.as_deref() == Some(workspace.as_str()),
            "integration evidence must match the stored final source and plan workspace",
        )?;
    }
    Ok(())
}
