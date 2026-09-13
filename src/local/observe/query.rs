use super::*;
use crate::{
    Validate,
    local::{runtime::RunRecord, store::JournalEntry},
    protocol::*,
};
use rusqlite::params;
use serde_json::Value;
use std::collections::BTreeMap;

const JOBS: usize = 512;
const PLANS: usize = 64;
const EVENTS: usize = 2048;
const TASKS: usize = 4096;
const EXPERIMENTS: usize = 256;

fn field(v: &Value, key: &str) -> Option<String> {
    v[key].as_str().filter(|s| !s.is_empty()).map(label)
}
fn tag(v: &impl Serialize) -> String {
    serde_json::to_value(v)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_else(|| "UNKNOWN".into())
}
fn failure(reason: &str) -> Blocker {
    let r = reason.to_ascii_lowercase();
    let (kind, description) = if r.contains("source_drift") {
        (
            "SOURCE_DRIFT",
            "Source state no longer matches captured input",
        )
    } else if r.contains("correction limit reached") {
        ("CORRECTION_LIMIT", "Correction policy blocks continuation")
    } else if r.contains("workspace has a live controller/provider lease") {
        ("WORKSPACE_BUSY", "Workspace is owned by another controller")
    } else if r.contains("authentication") || r.contains("provider adapter is unavailable") {
        (
            "PROVIDER_UNAVAILABLE",
            "Provider failed or authentication is unavailable",
        )
    } else if r.contains("verifier rejected") {
        (
            "VERIFICATION_REJECTED",
            "Verification rejected; dependents remain blocked",
        )
    } else if r.contains("interrupt") || r.contains("uncertain") {
        (
            "INTERRUPTED",
            "Job outcome is uncertain; explicit recovery required",
        )
    } else {
        (
            "RUNTIME_BLOCKED",
            "Runtime stopped; inspect canonical run status",
        )
    };
    blocker(kind, description)
}
fn activity(state: &str) -> String {
    match state {
        "QUEUED" => "PROVIDER_STARTING",
        "RUNNING" => "PROVIDER_EXECUTION",
        "SUCCEEDED" => "PROVIDER_RETURNED",
        "COMPLETE" => "COMPLETE",
        "FAILED" => "FAILED",
        "INTERRUPTED" => "INTERRUPTED",
        "CANCELLED" => "CANCELLED",
        _ => "UNKNOWN",
    }
    .into()
}
fn session(id: String, repo: String, workspace: String, root: String) -> Session {
    Session {
        id,
        ownership_known: false,
        repository_id: repo,
        workspace_id: workspace,
        root,
        supervisor_id: None,
        plans: vec![],
        current_plan: None,
        title: "Engineering undertaking".into(),
        state: "UNKNOWN".into(),
        verified: 0,
        task_count: 0,
        progress_complete: true,
        correction_round: None,
        blocker: None,
        activity: "UNKNOWN".into(),
        check: None,
        last_event: None,
    }
}

impl Store {
    /// One SQLite read transaction, bounded rows and payload sizes. No repository
    /// discovery, graph freshness scans, artifacts or provider configuration reads.
    pub fn observe(&self, at_ms: u64) -> Result<Snapshot> {
        let tx = self.connection.unchecked_transaction()?;
        let mut out = Snapshot {
            at_ms,
            ..Default::default()
        };
        let mut sessions = BTreeMap::<String, Session>::new();
        let mut request_owners = BTreeMap::new();
        let mut statement = tx.prepare("SELECT j.job_id,j.repo_id,j.workspace_id,w.root,CASE WHEN length(j.record_json)<=65536 THEN j.record_json ELSE '{}' END FROM runtime_jobs j JOIN workspaces w ON w.workspace_id=j.workspace_id AND w.repo_id=j.repo_id ORDER BY j.rowid DESC LIMIT ?1")?;
        let rows = statement.query_map([JOBS as i64 + 1], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
            ))
        })?;
        for (i, row) in rows.enumerate() {
            if i == JOBS {
                out.truncated = true;
                break;
            }
            let (job_id, repo, workspace, root, json) = row?;
            let v: Value = serde_json::from_str(&json).unwrap_or(Value::Null);
            if v["job_id"].as_str() != Some(job_id.as_str()) {
                out.truncated = true;
                out.warnings
                    .push("Malformed or oversized historical job metadata marked unknown".into());
            }
            let owner = &v["ownership"];
            let id =
                field(owner, "agent_instance_id").unwrap_or_else(|| format!("legacy:{job_id}"));
            let mut sid = field(owner, "engineering_session_id");
            if let Some(s) = sid.as_ref().and_then(|s| sessions.get(s)) {
                if s.repository_id != repo || s.workspace_id != workspace {
                    sid = None;
                }
            }
            if let Some(sid) = &sid {
                sessions
                    .entry(sid.clone())
                    .or_insert_with(|| {
                        session(sid.clone(), repo.clone(), workspace.clone(), label(&root))
                    })
                    .ownership_known = true;
                if let Some(request) = field(&v, "request_id") {
                    request_owners.insert((repo.clone(), workspace.clone(), request), sid.clone());
                }
            }
            let state = field(&v, "state").unwrap_or_else(|| "UNKNOWN".into());
            let task_id = field(&v, "task_id");
            let role = field(&v, "role")
                .map(|r| {
                    if r == "VERIFIER" && task_id.is_none() {
                        "INTEGRATION_VERIFIER".into()
                    } else {
                        r
                    }
                })
                .unwrap_or_else(|| "UNKNOWN".into());
            out.agents.push(Agent {
                id,
                session_id: sid.clone(),
                parent_id: field(owner, "parent_agent_instance_id"),
                repository_id: repo,
                workspace_id: workspace,
                root: label(&root),
                role,
                provider: field(&v["config"], "provider"),
                model: field(&v["config"], "model"),
                requested_role: field(&v["route"], "requested_role"),
                route_attempt: v["route"]["attempt"].as_u64(),
                route_origin: field(&v["route"]["primary"], "provider"),
                policy_skip_reason: v["route"]["policy_skipped"]
                    .as_array()
                    .filter(|s| !s.is_empty())
                    .map(|_| "PROJECT_POLICY_RESTRICTION".into()),
                fallback_reason: v["route"]["failures"]
                    .as_array()
                    .and_then(|a| a.last())
                    .and_then(|v| field(v, "reason")),
                plan_id: field(&v, "plan_id"),
                task_id,
                job_id,
                state: state.clone(),
                liveness: Liveness::Unknown,
                activity: activity(&state),
                verification: None,
                blocker: if ["FAILED", "INTERRUPTED", "CANCELLED"].contains(&state.as_str()) {
                    Some(failure(v["failure"].as_str().unwrap_or(&state)))
                } else {
                    None
                },
                created_at_ms: v["created_at_ms"].as_u64(),
                started_at_ms: v["started_at_ms"].as_u64(),
                finished_at_ms: v["finished_at_ms"].as_u64(),
                last_event: None,
                ownership_uncertain: sid.is_none(),
            });
        }
        drop(statement);
        // Plans are selected independently of runtime jobs so pending TaskPackets
        // remain visible before agentctl has created any executor.
        let mut statement = tx.prepare("SELECT e.repo_id,e.workspace_id,w.root,e.plan_id,e.state,CASE WHEN length(p.packet_json)<=262144 THEN p.packet_json ELSE '{}' END,CASE WHEN length(r.record_json)<=262144 THEN r.record_json WHEN r.record_json IS NOT NULL THEN '{}' ELSE NULL END,e.request_id FROM execution_plans e JOIN plans p USING(repo_id,plan_id) JOIN workspaces w ON w.repo_id=e.repo_id AND w.workspace_id=e.workspace_id LEFT JOIN runtime_runs r ON r.repo_id=e.repo_id AND r.plan_id=e.plan_id ORDER BY (e.state='ACTIVE') DESC,e.updated_at_ms DESC,e.repo_id,e.plan_id LIMIT ?1")?;
        let rows = statement.query_map([PLANS as i64 + 1], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, String>(5)?,
                r.get::<_, Option<String>>(6)?,
                r.get::<_, String>(7)?,
            ))
        })?;
        for (i, row) in rows.enumerate() {
            if i == PLANS {
                out.truncated = true;
                break;
            }
            let (repo, workspace, root, plan_id, plan_state, json, run_json, request_id) = row?;
            let run = run_json
                .as_ref()
                .and_then(|s| serde_json::from_str::<RunRecord>(s).ok());
            let owned = run.as_ref().and_then(|r| r.engineering_session.as_ref());
            let sid = owned
                .map(|s| s.id.clone())
                .or_else(|| {
                    if run_json.is_none() {
                        request_owners
                            .get(&(repo.clone(), workspace.clone(), request_id))
                            .cloned()
                    } else {
                        None
                    }
                })
                .unwrap_or_else(|| format!("unowned:{repo}:{plan_id}"));
            if sessions
                .get(&sid)
                .is_some_and(|s| s.repository_id != repo || s.workspace_id != workspace)
            {
                out.warnings
                    .push("Conflicting historical session ownership omitted".into());
                continue;
            }
            let s = sessions.entry(sid.clone()).or_insert_with(|| {
                session(sid.clone(), repo.clone(), workspace.clone(), label(&root))
            });
            s.ownership_known |= owned.is_some();
            s.supervisor_id = owned.map(|s| s.supervisor_instance_id.as_str().into());
            s.plans.push(plan_id.clone());
            // Most recent/current plan defines progress, not superseded attempts.
            let current = s.plans.len() == 1;
            if current {
                s.current_plan = Some(plan_id.clone());
                s.state = run
                    .as_ref()
                    .map(|r| tag(&r.state))
                    .unwrap_or_else(|| plan_state.clone());
                if ["SUPERSEDED", "CANCELLED", "COMPLETE"].contains(&plan_state.as_str()) {
                    s.state = plan_state.clone()
                }
                s.correction_round = run.as_ref().map(|r| r.correction_round);
                s.blocker = run.as_ref().and_then(|r| r.reason.as_deref()).map(failure);
                s.activity = activity(&s.state);
                if run_json.is_some() && run.is_none() {
                    s.state = "UNKNOWN".into();
                    s.blocker = Some(blocker(
                        "MALFORMED_HISTORY",
                        "Runtime metadata cannot be interpreted",
                    ));
                }
            }
            let Ok(packet) = serde_json::from_str::<PlanPacket>(&json) else {
                out.truncated = true;
                s.progress_complete = false;
                out.warnings
                    .push("Malformed or oversized plan omitted".into());
                continue;
            };
            if packet.validate().is_err() {
                out.truncated = true;
                s.progress_complete = false;
                out.warnings.push("Invalid historical plan omitted".into());
                continue;
            }
            if current {
                s.title = label(&packet.objective);
                s.task_count = packet.tasks.len();
            }
            let mut state_stmt=tx.prepare("SELECT task_id,state_json FROM tasks WHERE repo_id=?1 AND plan_id=?2 ORDER BY task_id LIMIT ?3")?;
            let states: BTreeMap<TaskId, TaskState> = state_stmt
                .query_map(params![repo, plan_id, TASKS as i64 + 1], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
                })?
                .filter_map(|r| r.ok())
                .filter_map(|(id, state)| {
                    Some((TaskId::new(id).ok()?, serde_json::from_str(&state).ok()?))
                })
                .collect();
            if states.len() > TASKS || states.len() != packet.tasks.len() {
                s.progress_complete = false;
                out.truncated = true;
            }
            for task in &packet.tasks {
                if out.tasks.len() == TASKS {
                    out.truncated = true;
                    s.progress_complete = false;
                    break;
                }
                let state = states.get(&task.task_id).copied();
                let dependencies: Vec<_> = task
                    .dependencies
                    .iter()
                    .map(|d| d.as_str().to_owned())
                    .collect();
                let missing: Vec<_> = task
                    .dependencies
                    .iter()
                    .filter(|d| states.get(*d) != Some(&TaskState::Verified))
                    .map(|d| d.as_str().to_owned())
                    .collect();
                let lifecycle = state.map(|s| tag(&s)).unwrap_or_else(|| "UNKNOWN".into());
                let ready = plan_state == "ACTIVE"
                    && match state {
                        Some(TaskState::Planned) => packet
                            .validate_task_transition(
                                &task.task_id,
                                &states,
                                TaskState::Ready,
                                None,
                            )
                            .is_ok(),
                        Some(TaskState::Ready) => packet
                            .task_is_runnable(&task.task_id, &states)
                            .unwrap_or(false),
                        _ => false,
                    };
                let mut presentation = if ready {
                    "READY".into()
                } else if !missing.is_empty()
                    && matches!(
                        state,
                        Some(TaskState::Planned | TaskState::Ready | TaskState::Blocked)
                    )
                {
                    "BLOCKED".into()
                } else {
                    lifecycle.clone()
                };
                let mut blocker = if !missing.is_empty() {
                    Some(Blocker {
                        kind: "DEPENDENCY_NOT_VERIFIED".into(),
                        description: "Waiting for VERIFIED dependencies".into(),
                        dependencies: missing,
                    })
                } else if plan_state != "ACTIVE" && !matches!(state, Some(TaskState::Verified)) {
                    Some(blocker("PLAN_INACTIVE", "Plan is not active"))
                } else {
                    match state {
                        Some(TaskState::AwaitingVerification) => Some(blocker(
                            "WAITING_FOR_VERIFIER",
                            "Executor returned; independent verification pending",
                        )),
                        Some(TaskState::Rejected) => {
                            Some(blocker("VERIFICATION_REJECTED", "Verification rejected"))
                        }
                        Some(TaskState::Blocked) => {
                            Some(blocker("TASK_BLOCKED", "Canonical task is blocked"))
                        }
                        _ => None,
                    }
                };
                if current && ready && s.state == "BLOCKED" {
                    presentation = "BLOCKED".into();
                    blocker = s.blocker.clone().or_else(|| {
                        Some(super::blocker(
                            "RUNTIME_BLOCKED",
                            "Runtime continuation is blocked",
                        ))
                    });
                }
                if current && state == Some(TaskState::Verified) {
                    s.verified += 1;
                }
                out.tasks.push(Task {
                    id: task.task_id.as_str().into(),
                    repository_id: repo.clone(),
                    workspace_id: workspace.clone(),
                    session_id: Some(sid.clone()),
                    plan_id: plan_id.clone(),
                    objective: label(&task.objective),
                    dependencies,
                    lifecycle,
                    presentation,
                    blocker,
                    executor_job: None,
                    verifier_job: None,
                    attempts_in_view: 0,
                    last_event: None,
                });
            }
            if current
                && s.verified == s.task_count
                && s.task_count > 0
                && s.state != "COMPLETE"
                && s.blocker.is_none()
            {
                s.blocker = Some(blocker(
                    "INTEGRATION_PENDING",
                    "Packet proofs accepted; integration verification pending",
                ));
            }
        }
        drop(statement);
        out.agents.sort_by(|a, b| {
            (&a.session_id, a.created_at_ms, &a.id).cmp(&(&b.session_id, b.created_at_ms, &b.id))
        });
        let mut statement=tx.prepare("SELECT sequence,repo_id,workspace_id,timestamp_ms,plan_id,task_id,job_id,CASE WHEN length(entry_json)<=65536 THEN entry_json ELSE '{}' END FROM events ORDER BY sequence DESC LIMIT ?1")?;
        let rows = statement.query_map([EVENTS as i64 + 1], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, u64>(3)?,
                r.get::<_, Option<String>>(4)?,
                r.get::<_, Option<String>>(5)?,
                r.get::<_, Option<String>>(6)?,
                r.get::<_, String>(7)?,
            ))
        })?;
        for (i, row) in rows.enumerate() {
            if i == EVENTS {
                out.truncated = true;
                break;
            }
            let (sequence, repo, workspace, at_ms, plan_id, task_id, job_id, json) = row?;
            let Ok(entry) = serde_json::from_str::<JournalEntry>(&json) else {
                out.truncated = true;
                out.warnings
                    .push("Malformed or oversized historical event omitted".into());
                continue;
            };
            let mut e = Event {
                sequence,
                at_ms,
                repository_id: repo.clone(),
                workspace_id: workspace,
                plan_id,
                task_id,
                job_id,
                phase: "STATE_CHANGED".into(),
                summary: "Engineering state changed".into(),
                check: None,
            };
            match entry {
                JournalEntry::Runtime {
                    job_id,
                    phase,
                    detail,
                } => {
                    e.job_id = e.job_id.or(job_id.map(|j| j.as_str().into()));
                    e.phase = if phase.len() <= 64
                        && phase.bytes().all(|c| c.is_ascii_uppercase() || c == b'_')
                    {
                        phase
                    } else {
                        "RUNTIME_EVENT".into()
                    };
                    e.summary = e.phase.replace('_', " ");
                    if e.phase == "ROUTE_FALLBACK" {
                        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&detail) {
                            e.summary = format!(
                                "Fallback {}: {} → {} ({})",
                                field(&v, "role").unwrap_or_default(),
                                field(&v["primary"], "provider").unwrap_or_default(),
                                field(&v["selected"], "provider").unwrap_or_default(),
                                v["failures"]
                                    .as_array()
                                    .and_then(|a| a.last())
                                    .and_then(|f| field(f, "reason"))
                                    .unwrap_or_else(|| "UNKNOWN".into())
                            );
                        }
                    }
                    if e.phase.starts_with("VERIFICATION_CHECK_")
                        && detail.len() <= 80
                        && detail
                            .bytes()
                            .all(|c| c.is_ascii_alphanumeric() || b"_-.:".contains(&c))
                    {
                        e.check = Some(label(&detail));
                    }
                }
                JournalEntry::TaskStateChanged {
                    to, verification, ..
                } => {
                    e.phase = format!("TASK_{}", tag(&to));
                    e.summary = e.phase.replace('_', " ");
                    if let Some(v) = verification {
                        if let Some(a) = out.agents.iter_mut().find(|a| {
                            a.repository_id == repo && a.job_id == v.verifier_job_id.as_str()
                        }) {
                            a.verification = Some(tag(&v.decision));
                        }
                    }
                }
                JournalEntry::ExecutionPlanCompleted { verification, .. } => {
                    e.phase = "COMPLETE".into();
                    e.summary = "Integration accepted; plan complete".into();
                    if let Some(a) = out.agents.iter_mut().find(|a| {
                        a.repository_id == repo && a.job_id == verification.verifier_job_id.as_str()
                    }) {
                        a.verification = Some(tag(&verification.decision));
                    }
                }
                JournalEntry::Agent { event } => {
                    if event.validate().is_err() {
                        out.truncated = true;
                        out.warnings
                            .push("Invalid historical telemetry omitted".into());
                        continue;
                    }
                    if let AgentEventKind::TokenUsageObserved { usage } = event.event {
                        let agent = out.agents.iter().find(|a| {
                            a.repository_id == repo
                                && e.workspace_id.as_deref() == Some(a.workspace_id.as_str())
                                && usage
                                    .context
                                    .job_id
                                    .as_ref()
                                    .is_some_and(|j| j.as_str() == a.job_id)
                        });
                        let total = if usage.provenance == TokenUsageProvenance::Unknown {
                            None
                        } else {
                            usage.total_tokens.or_else(|| {
                                usage
                                    .input_tokens
                                    .zip(usage.output_tokens)
                                    .and_then(|(a, b)| a.checked_add(b))
                            })
                        };
                        out.usage.push(Usage {
                            observation_id: format!("{repo}:{sequence}"),
                            at_ms: usage.timestamp_ms,
                            session_id: agent.and_then(|a| a.session_id.clone()),
                            agent_id: agent.map(|a| a.id.clone()),
                            job_id: agent.map(|a| a.job_id.clone()),
                            task_id: agent.and_then(|a| a.task_id.clone()),
                            provider: agent.and_then(|a| a.provider.clone()),
                            model: agent.and_then(|a| a.model.clone()),
                            role: agent.map(|a| a.role.clone()),
                            input: usage.input_tokens,
                            output: usage.output_tokens,
                            total,
                            provenance: usage.provenance,
                        });
                        continue;
                    }
                    continue;
                }
                JournalEntry::JobStateChanged { to, .. } => {
                    e.phase = format!("JOB_{}", tag(&to));
                    e.summary = e.phase.replace('_', " ");
                }
                JournalEntry::EvidenceRecorded { .. } => {
                    e.phase = "EVIDENCE_RECORDED".into();
                    e.summary = "Evidence recorded".into();
                }
                _ => continue,
            }
            out.events.push(e);
        }
        drop(statement);
        out.events.sort_by_key(|e| e.sequence);
        out.usage
            .sort_by(|a, b| (a.at_ms, &a.observation_id).cmp(&(b.at_ms, &b.observation_id)));
        for a in &mut out.agents {
            a.last_event = out
                .events
                .iter()
                .rev()
                .find(|e| {
                    e.repository_id == a.repository_id && e.job_id.as_deref() == Some(&a.job_id)
                })
                .cloned();
            if a.state == "RUNNING" {
                if let Some(e) = &a.last_event {
                    if e.phase == "JOB_OUTPUT_RECEIVED" {
                        a.activity = "PROVIDER_RETURNED".into();
                    }
                }
            }
        }
        for t in &mut out.tasks {
            let jobs: Vec<_> = out
                .agents
                .iter()
                .filter(|a| {
                    a.repository_id == t.repository_id
                        && a.workspace_id == t.workspace_id
                        && a.plan_id.as_deref() == Some(&t.plan_id)
                        && a.task_id.as_deref() == Some(&t.id)
                })
                .collect();
            t.executor_job = jobs
                .iter()
                .rev()
                .find(|a| a.role == "EXECUTOR")
                .map(|a| a.job_id.clone());
            t.verifier_job = jobs
                .iter()
                .rev()
                .find(|a| a.role == "VERIFIER")
                .map(|a| a.job_id.clone());
            t.attempts_in_view = jobs.iter().filter(|a| a.role == "EXECUTOR").count();
            t.last_event = out
                .events
                .iter()
                .rev()
                .find(|e| e.repository_id == t.repository_id && e.task_id.as_deref() == Some(&t.id))
                .cloned();
        }
        for s in sessions.values_mut() {
            s.last_event = out
                .events
                .iter()
                .rev()
                .find(|e| {
                    e.repository_id == s.repository_id
                        && e.workspace_id.as_deref() == Some(&s.workspace_id)
                        && (e.plan_id.as_ref().is_some_and(|p| s.plans.contains(p))
                            || e.job_id.as_ref().is_some_and(|j| {
                                out.agents.iter().any(|a| {
                                    a.session_id.as_deref() == Some(&s.id) && a.job_id == *j
                                })
                            }))
                })
                .cloned();
            if s.state == "UNKNOWN" && s.plans.is_empty() {
                s.state = out
                    .agents
                    .iter()
                    .rev()
                    .find(|a| a.session_id.as_deref() == Some(&s.id))
                    .map(|a| a.state.clone())
                    .unwrap_or_else(|| "UNKNOWN".into());
            }
            if let Some(e) = &s.last_event {
                s.activity = if e.phase == "JOB_STARTED" {
                    "PROVIDER_EXECUTION".into()
                } else {
                    e.phase.clone()
                };
                if e.phase == "VERIFICATION_CHECK_STARTED" {
                    s.check = e.check.clone();
                }
            }
            s.plans.sort();
        }
        let parents: std::collections::BTreeSet<_> = out
            .agents
            .iter()
            .map(|a| (a.session_id.clone(), a.id.clone()))
            .collect();
        for a in &mut out.agents {
            a.ownership_uncertain |= a.session_id.as_ref().is_none_or(|id| {
                sessions.get(id).is_none_or(|s| {
                    a.parent_id.as_ref().is_none_or(|parent| {
                        s.supervisor_id.as_ref() != Some(parent)
                            && !parents.contains(&(a.session_id.clone(), parent.clone()))
                    })
                })
            });
            if a.state == "RUNNING"
                && a.session_id.as_deref().is_some_and(|session| {
                    crate::local::runtime::liveness::is_live(
                        self.connection.path().unwrap_or(""),
                        &a.repository_id,
                        &a.workspace_id,
                        session,
                        &a.id,
                        &a.job_id,
                    )
                })
            {
                a.liveness = Liveness::Live;
            }
        }
        out.sessions = sessions.into_values().collect();
        out.sessions.sort_by(|a, b| {
            (b.state == "RUNNING")
                .cmp(&(a.state == "RUNNING"))
                .then_with(|| {
                    b.last_event
                        .as_ref()
                        .map(|e| e.sequence)
                        .cmp(&a.last_event.as_ref().map(|e| e.sequence))
                })
                .then_with(|| a.id.cmp(&b.id))
        });
        out.tasks.sort_by(|a, b| {
            (&a.repository_id, &a.plan_id, &a.id).cmp(&(&b.repository_id, &b.plan_id, &b.id))
        });
        {
            let mut statement = tx.prepare("SELECT experiment_id,repo_id,workspace_id,created_at_ms,CASE WHEN length(record_json)<=65536 THEN record_json ELSE '{}' END,cancel_requested FROM experiment_runs ORDER BY rowid DESC LIMIT ?1")?;
            let rows = statement.query_map([EXPERIMENTS as i64 + 1], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, i64>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, bool>(5)?,
                ))
            })?;
            for (i, row) in rows.enumerate() {
                if i == EXPERIMENTS {
                    out.truncated = true;
                    break;
                }
                let (id, repo, workspace, created_at_ms, json, cancel_requested) = row?;
                let v: Value = serde_json::from_str(&json).unwrap_or(Value::Null);
                if v["experiment_id"].as_str() != Some(id.as_str()) {
                    out.truncated = true;
                    out.warnings
                        .push("Malformed or oversized experiment metadata marked unknown".into());
                    continue;
                }
                let command = &v["command"];
                let program = command["program"].as_str().unwrap_or("?");
                let args: Vec<String> = command["args"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|x| x.as_str().map(str::to_owned))
                            .collect()
                    })
                    .unwrap_or_default();
                let attempts = v["attempts"].as_array().cloned().unwrap_or_default();
                let last = attempts.last();
                let state = v["state"].as_str().unwrap_or("UNKNOWN").to_string();
                let liveness = if state == "RUNNING"
                    && crate::local::runtime::liveness::is_live(
                        self.connection.path().unwrap_or(""),
                        &repo,
                        &workspace,
                        "",
                        "",
                        &id,
                    ) {
                    Liveness::Live
                } else {
                    Liveness::Unknown
                };
                out.experiments.push(Experiment {
                    id,
                    repository_id: repo,
                    workspace_id: workspace,
                    command_summary: label(&format!("{program} {}", args.join(" "))),
                    state,
                    liveness,
                    attempt: attempts.len(),
                    created_at_ms: u64::try_from(created_at_ms).unwrap_or(0),
                    started_at_ms: last.and_then(|a| a["started_at_ms"].as_u64()),
                    finished_at_ms: last.and_then(|a| a["finished_at_ms"].as_u64()),
                    exit_status: last
                        .and_then(|a| a["exit_status"].as_i64())
                        .and_then(|v| i32::try_from(v).ok()),
                    cancel_requested,
                });
            }
        }
        out.warnings.sort();
        out.warnings.dedup();
        if out.truncated {
            out.warnings.push("Bounded recent view: older/oversized rows may be omitted; usage is observed, not complete coverage.".into());
        }
        tx.rollback()?;
        Ok(out)
    }
}
