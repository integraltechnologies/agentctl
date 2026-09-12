use super::*;
use std::fs;

pub(super) fn text(s: &str, max: usize, name: &str) -> Result<()> {
    require(
        !s.trim().is_empty() && !s.contains('\0') && s.len() <= max,
        format!("{name} must be nonblank, NUL-free and at most {max} bytes"),
    )
}
fn texts(values: &[String], name: &str) -> Result<()> {
    require(values.len() <= 32, format!("too many {name}"))?;
    for s in values {
        text(s, 1024, name)?;
    }
    Ok(())
}
fn provenance(p: &PlanningProvenance) -> Result<()> {
    text(&p.actor, 128, "planning actor")?;
    texts(&p.source_refs, "provenance refs")?;
    require(
        !p.source_refs.is_empty(),
        "planning provenance requires a source reference",
    )?;
    if let Some(provider) = &p.provider {
        provider.validate()?;
    }
    Ok(())
}
pub(super) fn check_intent(d: &RequestDraft) -> Result<()> {
    text(&d.objective, 4096, "objective")?;
    provenance(&d.provenance)?;
    if let Some(q) = &d.query {
        text(q, 512, "planning query")?;
    }
    texts(&d.constraints, "constraints")?;
    texts(&d.definition_of_done, "done criteria")?;
    require(
        size(d)? <= 16384 && d.scope.len() <= 32 && d.invariant_refs.len() <= 32,
        "planning intent exceeds bounds",
    )?;
    for p in &d.scope {
        crate::validation::repo_path(p.path())?;
    }
    if let Some(v) = &d.verification {
        v.validate()?;
    }
    Ok(())
}
pub(super) fn checks(policy: &ProjectConfig, v: &VerificationRequirements) -> Result<()> {
    v.validate()?;
    require(
        v.requirement_refs.len() <= 32,
        "too many verification requirements",
    )?;
    for r in &v.requirement_refs {
        require(
            policy.verification.contains_key(r),
            format!("verification reference {r} is not defined in project policy"),
        )?;
    }
    Ok(())
}
pub(super) fn validate_invariants(
    c: &Connection,
    info: &RepositoryInfo,
    policy: &ProjectConfig,
    refs: &[String],
) -> Result<()> {
    require(refs.len() <= 32, "at most 32 critical invariants")?;
    for r in refs {
        GraphEntityId::new(r.clone()).map_err(Error::Invalid)?;
        if policy.invariants.contains_key(r) {
            continue;
        }
        let n:i64=c.query_row("SELECT count(*) FROM memory_entries WHERE repo_id=?1 AND workspace_id IS NULL AND trust='CANONICAL' AND kind='INVARIANT' AND status='ACTIVE' AND canonical_key=?2",params![info.repository_id.as_str(),r],|row|row.get(0))?;
        require(
            n == 1,
            format!(
                "invariant {r} is neither live project policy nor an active canonical keyed invariant"
            ),
        )?;
    }
    Ok(())
}
fn inside(path: &str, parent: &str) -> bool {
    path == parent
        || path
            .strip_prefix(parent)
            .is_some_and(|s| s.starts_with('/'))
}

pub(super) fn resolve_invariants(
    c: &Connection,
    info: &RepositoryInfo,
    policy: &ProjectConfig,
    refs: &[String],
) -> Result<BTreeMap<String, String>> {
    validate_invariants(c, info, policy, refs)?;
    refs.iter().map(|key|{
        let content=if let Some(d)=policy.invariants.get(key){d.description.clone()}else{
            c.query_row("SELECT json_extract(record_json,'$.content') FROM memory_entries WHERE repo_id=?1 AND canonical_key=?2 AND status='ACTIVE' AND trust='CANONICAL' AND kind='INVARIANT'",params![info.repository_id.as_str(),key],|r|r.get(0))?
        };
        Ok((key.clone(),content))
    }).collect()
}
pub(super) fn permits(scope: &ScopePath, path: &str) -> bool {
    match scope {
        ScopePath::File { path: p } => p == path,
        ScopePath::Directory { path: p } => inside(path, p),
    }
}
fn overlap(a: &ScopePath, b: &ScopePath) -> bool {
    permits(a, b.path()) || permits(b, a.path())
}
pub(super) fn safe_scope(
    root: &Path,
    policy: &ProjectConfig,
    p: &ScopePath,
    write: bool,
) -> Result<()> {
    crate::validation::repo_path(p.path())?;
    require(
        !p.path().split('/').any(|s| s == ".git"),
        "Git administrative paths are not task scope",
    )?;
    for protected in &policy.protected {
        if (protected.deny_read || write && protected.deny_write)
            && overlap(
                p,
                &ScopePath::Directory {
                    path: protected.path.clone(),
                },
            )
        {
            return Err(Error::Invalid(format!(
                "scope {} intersects protected path {}",
                p.path(),
                protected.path
            )));
        }
    }
    let mut current = root.to_path_buf();
    for segment in Path::new(p.path()).components() {
        current.push(segment);
        match fs::symlink_metadata(&current) {
            Ok(m) => require(!m.file_type().is_symlink(), "scope crosses a symlink")?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => break,
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}
pub(super) fn source_matches(
    c: &Connection,
    info: &RepositoryInfo,
    s: &PlanningSource,
    policy: &ProjectConfig,
) -> Result<()> {
    require(
        s.observation.repository_id == info.repository_id
            && s.observation.workspace_id == info.workspace_id,
        "planning source belongs to another repository/workspace",
    )?;
    require(
        s.observation.head_commit == info.source.head_commit
            && s.observation.dirty == info.source.dirty,
        "HEAD/dirty observation drifted; prepare a new request",
    )?;
    require(
        s.observation.worktree_fingerprint.is_none() && s.graph_version == graph::INDEX_VERSION,
        "unsupported planning source guarantee/version",
    )?;
    require(
        s.policy_hash == hash(policy)?,
        "project policy changed; prepare a new request",
    )?;
    require(s.support.len() <= 16, "too many planning source files")?;
    for p in &s.support {
        require(
            p.repository_id == info.repository_id
                && p.workspace_id == info.workspace_id
                && p.backend == p.language.backend(),
            "invalid supporting source provenance",
        )?;
        let stored:Option<(Option<String>,String,Option<String>)>=c.query_row("SELECT content_hash,backend,diagnostic FROM indexed_files WHERE workspace_id=?1 AND path=?2",params![info.workspace_id.as_str(),p.path],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
        require(
            stored == Some((Some(p.content_hash.clone()), p.backend.clone(), None)),
            "supporting graph provenance drifted",
        )?;
        require(
            graph::files::contains_source(&info.root, &p.path)?
                && graph::files::read(&info.root, &p.path)?.0 == p.content_hash,
            "supporting source changed/excluded; reindex and prepare a new request",
        )?;
    }
    Ok(())
}

pub(super) fn validate(c: &Connection, info: &RepositoryInfo, p: &ExecutionPlan) -> Result<()> {
    require(
        size(p)? <= 262144 && p.packet.tasks.len() <= 32,
        "execution plan exceeds 256 KiB / 32 tasks",
    )?;
    p.packet.validate()?;
    provenance(&p.metadata.provenance)?;
    let prepared = request(c, info, &p.metadata.request_id)?;
    check_intent(&prepared.request.intent)?;
    require(
        p.metadata.source == prepared.request.source,
        "plan must preserve its prepared source assumptions",
    )?;
    require(
        p.packet.objective == prepared.request.intent.objective,
        "plan objective differs from durable user intent",
    )?;
    require(
        p.metadata.created_at_ms >= prepared.request.created_at_ms
            && p.metadata.created_at_ms <= now_ms()?,
        "plan creation timestamp is inconsistent with request/current time",
    )?;
    let policy = ProjectConfig::load(&info.root)?;
    source_matches(c, info, &p.metadata.source, &policy)?;
    require(
        graph::status(c, info)?.fresh,
        "plan validation requires a complete fresh code index",
    )?;
    validate_invariants(c, info, &policy, &prepared.request.intent.invariant_refs)?;
    require(
        resolve_invariants(c, info, &policy, &prepared.request.intent.invariant_refs)?
            == prepared.context.invariants,
        "critical invariant content drifted; prepare a new request",
    )?;
    checks(&policy, &p.packet.integration_verification)?;
    if let Some(required) = &prepared.request.intent.verification {
        require(
            required.requirement_refs.iter().all(|r| {
                p.packet
                    .integration_verification
                    .requirement_refs
                    .contains(r)
            }) && (!required.evidence_required
                || p.packet.integration_verification.evidence_required),
            "integration weakens requested verification",
        )?;
    }
    let integration = &p.metadata.integration;
    require(
        integration.plan_id == p.packet.plan_id
            && integration.plan_packet_hash == hash(&p.packet)?
            && integration.independent_verifier
            && integration.require_all_task_verifications
            && integration.require_final_diff_and_evidence,
        "invalid/missing independent final integration contract",
    )?;
    texts(&integration.expectations, "integration expectations")?;
    require(
        !integration.expectations.is_empty()
            && prepared
                .request
                .intent
                .definition_of_done
                .iter()
                .all(|d| integration.expectations.contains(d)),
        "integration must carry requested done criteria",
    )?;
    require(
        p.metadata.contracts.len() == p.packet.tasks.len(),
        "every task requires exactly one verification contract",
    )?;
    let mut seen = BTreeSet::new();
    let mut graph_refs = BTreeSet::new();
    let mut memory_refs = BTreeSet::new();
    for contract in &p.metadata.contracts {
        require(
            seen.insert(contract.task_id.clone()),
            "duplicate verification contract",
        )?;
        let task = p
            .packet
            .tasks
            .iter()
            .find(|t| t.task_id == contract.task_id)
            .ok_or_else(|| Error::Invalid("contract names an unknown task".into()))?;
        require(
            size(task)? <= 16384 && size(contract)? <= 8192,
            "task/contract exceeds 16/8 KiB",
        )?;
        require(
            contract.independent_verifier && contract.task_packet_hash == hash(task)?,
            "verification contract must independently bind the exact TaskPacket",
        )?;
        require(
            !task.read_scope.is_empty() || !task.write_scope.is_empty(),
            "task must have bounded explicit scope",
        )?;
        require(
            task.read_scope.len() + task.write_scope.len() <= 32
                && task.graph_entities.len() <= 32
                && contract.memory_refs.len() <= 32
                && contract.exclusions.len() <= 32,
            "task reference/scope limits exceeded",
        )?;
        texts(&contract.non_goals, "task non-goals")?;
        for exclude in &contract.exclusions {
            crate::validation::repo_path(exclude.path())?;
        }
        checks(&policy, &task.verification)?;
        validate_invariants(c, info, &policy, &task.invariant_refs)?;
        require(
            task.invariant_refs
                .iter()
                .all(|r| prepared.context.invariants.contains_key(r)),
            "task adds an invariant outside prepared intent; prepare a new request including it",
        )?;
        require(
            prepared
                .request
                .intent
                .invariant_refs
                .iter()
                .all(|r| task.invariant_refs.contains(r)),
            "task omits a critical request/project invariant",
        )?;
        for (scopes, write) in [(&task.read_scope, false), (&task.write_scope, true)] {
            for s in scopes {
                safe_scope(&info.root, &policy, s, write)?;
                require(
                    prepared.request.intent.scope.is_empty()
                        || prepared.request.intent.scope.iter().any(|parent| {
                            permits(parent, s.path())
                                && (matches!(parent, ScopePath::Directory { .. })
                                    || matches!(s, ScopePath::File { .. }))
                        }),
                    "task exceeds requested scope",
                )?;
                require(
                    !contract
                        .exclusions
                        .iter()
                        .any(|excluded| overlap(excluded, s)),
                    "task scope contradicts an exclusion",
                )?;
            }
        }
        for id in &task.graph_entities {
            graph_refs.insert(id.clone());
            let json: Option<String> = c
                .query_row(
                    "SELECT record_json FROM graph_entities WHERE workspace_id=?1 AND entity_id=?2",
                    params![info.workspace_id.as_str(), id.as_str()],
                    |r| r.get(0),
                )
                .optional()?;
            let e: graph::Entity = serde_json::from_str(&json.ok_or_else(|| {
                Error::Invalid(format!(
                    "graph reference {} is missing in this workspace",
                    id.as_str()
                ))
            })?)?;
            require(
                p.metadata.source.support.contains(&e.provenance),
                "graph reference is outside prepared source support; prepare context including its file",
            )?;
            require(
                task.read_scope
                    .iter()
                    .any(|s| permits(s, &e.provenance.path)),
                "graph reference requires explicit read scope",
            )?;
        }
        memory_refs.extend(contract.memory_refs.iter().cloned());
    }
    require(
        graph_refs.len() <= 128 && memory_refs.len() <= 128,
        "plan reference bounds exceeded",
    )?;
    for id in memory_refs {
        memory::planning_reference(c, info, &id, prepared.context.limits.memory.notes > 0)?;
    }
    if let Some(replan) = &p.metadata.replan {
        text(&replan.reason, 1024, "replan reason")?;
        require(
            replan.previous_plan_id != p.packet.plan_id,
            "self-replan is invalid",
        )?;
        let prior = load(c, info, &replan.previous_plan_id)?;
        require(
            matches!(prior.state, PlanState::Validated | PlanState::Active)
                || prior.state == PlanState::Superseded
                    && prior.superseded_by.as_ref() == Some(&p.packet.plan_id),
            "prior plan is terminal or was superseded by another replacement",
        )?;
        let states = store::task_states(c, &info.repository_id, &replan.previous_plan_id)?;
        require(
            replan.previously_verified_tasks.len() + replan.replaced_tasks.len() <= 64,
            "too many replan references",
        )?;
        for id in &replan.previously_verified_tasks {
            require(
                states.get(id) == Some(&TaskState::Verified),
                "replan preserved-history reference is not VERIFIED",
            )?;
        }
        for id in &replan.replaced_tasks {
            require(
                states.contains_key(id),
                "replan replaced-history task is missing",
            )?;
        }
        require(
            replan
                .previously_verified_tasks
                .iter()
                .all(|id| !replan.replaced_tasks.contains(id)),
            "replan marks a task both retained and replaced",
        )?;
    }
    Ok(())
}

pub(super) fn integration_proof(
    c: &Connection,
    info: &RepositoryInfo,
    view: &ExecutionPlanView,
    proof: &VerificationPacket,
    source: &SourceStateRef,
) -> Result<()> {
    let id = &view.plan.packet.plan_id;
    let prepared = request(c, info, &view.plan.metadata.request_id)?;
    let policy = ProjectConfig::load(&info.root)?;
    require(
        resolve_invariants(c, info, &policy, &prepared.request.intent.invariant_refs)?
            == prepared.context.invariants,
        "critical invariant text drifted before completion",
    )?;
    require(
        view.plan.metadata.source.policy_hash == hash(&policy)?,
        "verification policy drifted; explicit replan required",
    )?;
    let verifier = store::job(c, &info.repository_id, &proof.verifier_job_id)?
        .ok_or_else(|| Error::Invalid("integration verifier is not registered".into()))?;
    require(
        verifier.role == AgentRole::Verifier
            && verifier.plan_id == *id
            && verifier.task_id.is_none()
            && verifier.state == JobState::Succeeded,
        "integration requires a succeeded plan-level verifier job",
    )?;
    let VerificationTarget::Integration {
        executor_job_ids, ..
    } = &proof.target
    else {
        return Err(Error::Invalid("expected integration target".into()));
    };
    let mut accepted = BTreeSet::new();
    for task in &view.plan.packet.tasks {
        let json:String=c.query_row("SELECT entry_json FROM events WHERE repo_id=?1 AND task_id=?2 AND json_extract(entry_json,'$.kind')='TASK_STATE_CHANGED' AND json_extract(entry_json,'$.to')='VERIFIED' ORDER BY sequence DESC LIMIT 1",params![info.repository_id.as_str(),task.task_id.as_str()],|r|r.get(0))?;
        let event: JournalEntry = serde_json::from_str(&json)?;
        if let JournalEntry::TaskStateChanged {
            verification: Some(v),
            ..
        } = event
        {
            // Recheck historical packet ownership too: older Stage 4 databases
            // may contain proofs accepted before workspace binding was enforced.
            v.validate()?;
            require(
                v.decision == VerificationDecision::Pass
                    && matches!(&v.target, VerificationTarget::Packet { task_id, .. } if task_id == &task.task_id),
                "accepted packet verification must be PASS for this task",
            )?;
            store::validate_verifier(
                c,
                &info.repository_id,
                &store::StoredTask {
                    plan_id: id.clone(),
                    packet: task.clone(),
                    state: TaskState::Verified,
                },
                &v,
            )?;
            if let VerificationTarget::Packet {
                executor_job_id, ..
            } = v.target
            {
                accepted.insert(executor_job_id);
            }
        }
    }
    require(
        accepted.len() == view.plan.packet.tasks.len()
            && accepted == executor_job_ids.iter().cloned().collect(),
        "integration executor set must match every accepted packet verification",
    )?;
    for job_id in executor_job_ids
        .iter()
        .chain(std::iter::once(&proof.verifier_job_id))
    {
        let job = store::job(c, &info.repository_id, job_id)?
            .ok_or_else(|| Error::Invalid("integration job is not registered".into()))?;
        require(
            job.plan_id == *id && job.state == JobState::Succeeded,
            "integration job has wrong plan or unfinished state",
        )?;
        let workspace: Option<String> = c.query_row(
            "SELECT workspace_id FROM jobs WHERE repo_id=?1 AND job_id=?2",
            params![info.repository_id.as_str(), job_id.as_str()],
            |r| r.get(0),
        )?;
        require(
            workspace.as_deref() == Some(info.workspace_id.as_str()),
            "integration jobs must belong to the plan workspace",
        )?;
    }
    store::validate_evidence(c, &info.repository_id, &proof.evidence)?;
    require(
        !proof.evidence.is_empty(),
        "integration requires final evidence",
    )?;
    for e in &proof.evidence {
        let (json, workspace): (String, Option<String>) = c.query_row(
            "SELECT record_json,workspace_id FROM evidence WHERE repo_id=?1 AND evidence_id=?2",
            params![info.repository_id.as_str(), e.0.as_str()],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        let record: EvidenceRecord = serde_json::from_str(&json)?;
        require(
            record.source_state.as_ref() == Some(source)
                && workspace.as_deref() == Some(info.workspace_id.as_str()),
            "integration evidence must bind the submitted final source and workspace",
        )?;
    }
    Ok(())
}
