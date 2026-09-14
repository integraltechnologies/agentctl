use super::*;
use crate::{
    Validate,
    local::{
        self,
        observe::label,
        store::{JournalEntry, Store},
    },
    protocol::*,
};
use rusqlite::params;
use serde_json::Value;

fn field(v: &Value, key: &str) -> Option<String> {
    v[key].as_str().filter(|s| !s.is_empty()).map(label)
}
fn tag(v: &impl serde::Serialize) -> String {
    serde_json::to_value(v)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_else(|| "UNKNOWN".into())
}
fn route_provenance(v: &Value) -> Option<RouteProvenance> {
    v.as_object()?;
    let source = |key: &str| {
        v["sources"][key]
            .as_str()
            .filter(|s| {
                [
                    "machine runtime.roles (legacy)",
                    "machine profiles",
                    "project profiles",
                    "explicit user",
                ]
                .contains(s)
            })
            .map(str::to_owned)
    };
    let identifiers = |v: &Value| RouteIdentifiers {
        provider: field(v, "provider"),
        model: field(v, "model"),
    };
    let depth = v["attempt"].as_u64();
    // Policy may promote an alternative at attempt zero. Its provider/model
    // still came from the fallback list, not the configured primary fields.
    let alternative = depth.is_some_and(|n| n > 0)
        || (v["selected"].is_object() && v["primary"].is_object() && v["selected"] != v["primary"]);
    Some(RouteProvenance {
        provider_source: source(if alternative { "fallbacks" } else { "provider" }),
        model_source: source(if alternative { "fallbacks" } else { "model" }),
        configured_provider_source: source("provider"),
        configured_model_source: source("model"),
        fallback_source: source("fallbacks"),
        explicit_override_fields: ["provider", "model", "fallbacks"]
            .into_iter()
            .filter(|k| source(k).as_deref() == Some("explicit user"))
            .map(str::to_owned)
            .collect(),
        actual_provider: field(&v["selected"], "provider"),
        actual_model: field(&v["selected"], "model"),
        fallback_used: depth.map(|n| n > 0),
        fallback_depth: depth,
        preceding_failures: v["failures"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|f| PrecedingFailure {
                route: identifiers(&f["route"]),
                reason: serde_json::from_value(f["reason"].clone()).ok(),
            })
            .collect(),
        policy_skipped: v["policy_skipped"]
            .as_array()
            .into_iter()
            .flatten()
            .map(identifiers)
            .collect(),
    })
}
fn add_usage(t: &mut Tokens, u: &TokenUsageEvent) {
    let p = tag(&u.provenance);
    t.input.add(u.input_tokens, &p);
    t.output.add(u.output_tokens, &p);
    t.cached_read.add(u.cached_tokens, &p);
    t.cache_write.add(None, &p);
    t.reasoning.add(u.reasoning_tokens, &p);
    let total = u
        .total_tokens
        .or_else(|| u.input_tokens?.checked_add(u.output_tokens?));
    t.total.add(total, &p);
    if u.total_tokens.is_none()
        && u.input_tokens
            .zip(u.output_tokens)
            .is_some_and(|(a, b)| a.checked_add(b).is_none())
    {
        t.total.overflow = true;
    }
}
fn missing(t: &mut Tokens) {
    t.input.add(None, "UNKNOWN");
    t.output.add(None, "UNKNOWN");
    t.cached_read.add(None, "UNKNOWN");
    t.cache_write.add(None, "UNKNOWN");
    t.reasoning.add(None, "UNKNOWN");
    t.total.add(None, "UNKNOWN");
}

impl Store {
    /// A bounded repository/workspace job cohort, read in one SQLite snapshot.
    /// Outcomes are current durable facts; this is not historical state replay.
    pub fn analytics(&self, q: Query, at_ms: u64) -> local::Result<Snapshot> {
        local::require(
            !q.repository.is_empty()
                && q.from_ms < q.to_ms
                && q.to_ms <= at_ms
                && at_ms <= i64::MAX as u64
                && (1..=20000).contains(&q.limit),
            "invalid analytics repository/window/limit",
        )?;
        let tx = self.connection.unchecked_transaction()?;
        let mut out=Snapshot{task_correction_distribution:BTreeMap::new(),as_of_ms:at_ms,providers:vec![],models:vec![],role_providers:vec![],analytics_version:1,query:q.clone(),truncated:false,warnings:vec!["Job-created cohort; outcomes are current durable state, not historical replay. Task mix differs across routes; comparisons are descriptive. Monetary cost and active execution duration are UNKNOWN.".into()],summary:Summary::default(),jobs:vec![],tasks:vec![],plans:vec![],sessions:vec![],roles:vec![],routes:vec![],integration:Summary::default(),unattributed:Summary::default(),orphan_usage:Tokens::default(),orphan_observations:0,verified_task_usage:Tokens::default(),verified_tasks_with_complete_usage:0,verified_tasks_with_partial_usage:0,tokens_per_complete_verified_task:None,task_correction_rate:Rate::default(),task_correction_unknown:0};
        let mut plans = BTreeMap::new();
        let mut planner_usage = BTreeMap::new();
        {
            let mut stmt=tx.prepare("SELECT e.plan_id,e.workspace_id,e.state,CASE WHEN length(e.metadata_json)<=262144 THEN e.metadata_json ELSE '{}' END,CASE WHEN length(p.packet_json)<=262144 THEN p.packet_json ELSE '{}' END,CASE WHEN length(r.record_json)<=262144 THEN r.record_json ELSE NULL END FROM execution_plans e JOIN plans p ON p.repo_id=e.repo_id AND p.plan_id=e.plan_id LEFT JOIN runtime_runs r ON r.repo_id=e.repo_id AND r.plan_id=e.plan_id WHERE e.repo_id=?1 AND (?2 IS NULL OR e.workspace_id=?2) AND (?4 IS NULL OR (json_valid(r.record_json) AND json_extract(r.record_json,'$.engineering_session.id')=?4)) AND ((json_valid(e.metadata_json) AND json_extract(e.metadata_json,'$.created_at_ms')>=?5 AND json_extract(e.metadata_json,'$.created_at_ms')<?6) OR EXISTS (SELECT 1 FROM runtime_jobs j WHERE j.repo_id=e.repo_id AND j.plan_id=e.plan_id AND json_valid(j.record_json) AND json_extract(j.record_json,'$.created_at_ms')>=?5 AND json_extract(j.record_json,'$.created_at_ms')<?6)) ORDER BY e.updated_at_ms DESC,e.plan_id LIMIT ?3")?;
            let rows = stmt.query_map(
                params![
                    q.repository,
                    q.workspace,
                    (q.limit + 1) as i64,
                    q.session,
                    q.from_ms,
                    q.to_ms
                ],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, String>(3)?,
                        r.get::<_, String>(4)?,
                        r.get::<_, Option<String>>(5)?,
                    ))
                },
            )?;
            for (i, row) in rows.enumerate() {
                if i == q.limit {
                    out.truncated = true;
                    break;
                }
                let (id, workspace, state, metadata, packet, run) = row?;
                if metadata.len() > 262144
                    || packet.len() > 262144
                    || run.as_ref().is_some_and(|r| r.len() > 262144)
                {
                    out.truncated = true;
                    continue;
                }
                let (Ok(m), Ok(p)) = (
                    serde_json::from_str::<Value>(&metadata),
                    serde_json::from_str::<Value>(&packet),
                ) else {
                    out.truncated = true;
                    continue;
                };
                if !p["tasks"].is_array() || m.as_object().is_none_or(|m| m.is_empty()) {
                    out.truncated = true;
                    continue;
                }
                let r: Value = run
                    .as_deref()
                    .and_then(|s| serde_json::from_str(s).ok())
                    .unwrap_or(Value::Null);
                let plan = Plan {
                    runtime_lifecycle: field(&r, "state"),
                    id: label(&id),
                    workspace: label(&workspace),
                    session: field(&r["engineering_session"], "id"),
                    lifecycle: label(&state),
                    correction_round: r["correction_round"]
                        .as_u64()
                        .and_then(|n| n.try_into().ok()),
                    previous_plan: field(&m["replan"], "previous_plan_id"),
                    replaced_tasks: m["replan"]["replaced_tasks"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_str)
                        .map(label)
                        .collect(),
                    task_ids: p["tasks"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|t| field(t, "task_id"))
                        .collect(),
                    verified_tasks: 0,
                };
                plans.insert(id, (plan, p));
            }
        }
        {
            let mut stmt=tx.prepare("SELECT job_id,workspace_id,CASE WHEN length(record_json)<=65536 THEN record_json ELSE '{}' END FROM runtime_jobs WHERE repo_id=?1 AND (?2 IS NULL OR workspace_id=?2) AND json_valid(record_json) AND json_extract(record_json,'$.created_at_ms')>=?3 AND json_extract(record_json,'$.created_at_ms')<?4 AND (?5 IS NULL OR json_extract(record_json,'$.ownership.engineering_session_id')=?5) AND (?6 IS NULL OR json_extract(record_json,'$.task_id')=?6) AND (?7 IS NULL OR job_id=?7 OR json_extract(record_json,'$.ownership.agent_instance_id')=?7) AND (?9 IS NULL OR coalesce(json_extract(record_json,'$.route.requested_role'),lower(json_extract(record_json,'$.role')))=?9) AND (?10 IS NULL OR json_extract(record_json,'$.config.provider')=?10) AND (?11 IS NULL OR json_extract(record_json,'$.config.model')=?11) AND (?12 IS NULL OR json_extract(record_json,'$.state')=?12) ORDER BY json_extract(record_json,'$.created_at_ms'),job_id LIMIT ?8")?;
            let rows = stmt.query_map(
                params![
                    q.repository,
                    q.workspace,
                    q.from_ms,
                    q.to_ms,
                    q.session,
                    q.task,
                    q.job,
                    (q.limit + 1) as i64,
                    q.role,
                    q.provider,
                    q.model,
                    q.lifecycle
                ],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                    ))
                },
            )?;
            for (i, row) in rows.enumerate() {
                if i == q.limit {
                    out.truncated = true;
                    break;
                }
                let (id, workspace, json) = row?;
                if json.len() > 65536 {
                    out.truncated = true;
                    continue;
                }
                let v: Value = serde_json::from_str(&json)?;
                if v["created_at_ms"].as_u64().is_none() {
                    out.truncated = true;
                    continue;
                }
                let role = field(&v["route"], "requested_role")
                    .or_else(|| field(&v, "role").map(|s| s.to_ascii_lowercase()))
                    .unwrap_or_else(|| "UNKNOWN".into());
                let provider = field(&v["config"], "provider");
                let model = field(&v["config"], "model");
                let lifecycle = field(&v, "state").unwrap_or_else(|| "UNKNOWN".into());
                if q.role.as_ref().is_some_and(|s| s != &role)
                    || q.provider
                        .as_ref()
                        .is_some_and(|s| Some(s) != provider.as_ref())
                    || q.model.as_ref().is_some_and(|s| Some(s) != model.as_ref())
                    || q.lifecycle.as_ref().is_some_and(|s| s != &lifecycle)
                {
                    continue;
                }
                let plan = field(&v, "plan_id");
                let correction_round = plan
                    .as_ref()
                    .and_then(|id| plans.get(id))
                    .and_then(|(p, _)| p.correction_round);
                let created = v["created_at_ms"].as_u64();
                let started = v["started_at_ms"].as_u64();
                let finished = v["finished_at_ms"].as_u64();
                let completed = lifecycle == "SUCCEEDED";
                if plan.is_none()
                    && role == "planner"
                    && let Ok(u) =
                        serde_json::from_value::<TokenUsageEvent>(v["planner_usage"].clone())
                    && u.validate().is_ok()
                    && u.timestamp_ms <= at_ms
                    && u.context.job_id.as_ref().is_some_and(|j| j.as_str() == id)
                {
                    planner_usage.insert((label(&workspace), label(&id)), u);
                }
                out.jobs.push(Job {
                    id: label(&id),
                    agent: field(&v["ownership"], "agent_instance_id"),
                    session: field(&v["ownership"], "engineering_session_id"),
                    parent: field(&v["ownership"], "parent_agent_instance_id"),
                    workspace: label(&workspace),
                    plan,
                    task: field(&v, "task_id"),
                    role,
                    provider,
                    model,
                    wall_elapsed_ms: created
                        .and_then(|s| finished.unwrap_or(at_ms).min(at_ms).checked_sub(s)),
                    completed_execution_ms: if ["SUCCEEDED", "FAILED"].contains(&lifecycle.as_str())
                    {
                        finished
                            .filter(|n| *n <= at_ms)
                            .and_then(|f| f.checked_sub(started?))
                    } else {
                        None
                    },
                    active_execution_ms: None,
                    created_ms: created,
                    started_ms: started,
                    finished_ms: finished,
                    lifecycle,
                    correction_round,
                    reported_decision: field(&v, "reported_verification")
                        .filter(|_| completed)
                        .filter(|s| ["PASS", "REJECT"].contains(&s.as_str())),
                    canonical_decision: None,
                    accepted_executor: None,
                    executor_disposition: None,
                    tokens: Tokens::default(),
                    usage_quality: "UNKNOWN".into(),
                    usage_observations: 0,
                    prompt_bytes: v["prompt"]["bytes"].as_u64(),
                    context_bytes: v["prompt"]["context_bytes"].as_u64(),
                    instruction_bytes: v["prompt"]["instruction_bytes"].as_u64(),
                    context_budget: v["prompt"]["context_budget"].as_u64(),
                    context_truncated: v["prompt"]["context_truncated"].as_bool(),
                    route_attempt: v["route"]["attempt"].as_u64(),
                    policy_skipped: v["route"]["policy_skipped"].as_array().map(Vec::len),
                    route_provenance: route_provenance(&v["route"]),
                    availability_failure: field(&v, "availability_failure").filter(|s| {
                        [
                            "PROVIDER_UNAVAILABLE",
                            "AUTH_UNAVAILABLE",
                            "CAPABILITY_UNSUPPORTED",
                            "STARTUP_FAILURE",
                        ]
                        .contains(&s.as_str())
                    }),
                    cost: None,
                });
            }
        }
        let index: BTreeMap<_, _> = out
            .jobs
            .iter()
            .enumerate()
            .map(|(i, j)| ((j.workspace.clone(), j.id.clone()), i))
            .collect();
        let mut seen = BTreeSet::new();
        let mut observed_decisions = BTreeSet::new();
        {
            // Repo/workspace indexes bound selection; no per-job event query.
            let mut stmt=tx.prepare("SELECT workspace_id,CASE WHEN length(entry_json)<=65536 THEN entry_json ELSE '{}' END FROM events WHERE repo_id=?1 AND (?2 IS NULL OR workspace_id=?2) AND timestamp_ms<=?3 AND (job_id IN (SELECT value FROM json_each(?7)) OR (job_id IS NULL AND plan_id IN (SELECT value FROM json_each(?8))) OR (job_id IS NULL AND plan_id IS NULL AND ?9 AND timestamp_ms>=?4 AND timestamp_ms<?5)) ORDER BY sequence DESC LIMIT ?6")?;
            let rows = stmt.query_map(
                params![
                    q.repository,
                    q.workspace,
                    at_ms,
                    q.from_ms,
                    q.to_ms,
                    (q.limit * 10 + 1) as i64,
                    serde_json::to_string(&out.jobs.iter().map(|j| &j.id).collect::<Vec<_>>())?,
                    serde_json::to_string(&plans.keys().collect::<Vec<_>>())?,
                    q.session.is_none()
                        && q.role.is_none()
                        && q.provider.is_none()
                        && q.model.is_none()
                        && q.task.is_none()
                        && q.job.is_none()
                        && q.lifecycle.is_none()
                ],
                |r| Ok((r.get::<_, Option<String>>(0)?, r.get::<_, String>(1)?)),
            )?;
            for (i, row) in rows.enumerate() {
                if i == q.limit * 10 {
                    out.truncated = true;
                    break;
                }
                let (workspace, json) = row?;
                if json.len() > 65536 {
                    out.truncated = true;
                    continue;
                }
                let Ok(e) = serde_json::from_str::<JournalEntry>(&json) else {
                    out.truncated = true;
                    continue;
                };
                match e {
                    JournalEntry::Agent { event } => {
                        if event.validate().is_err() {
                            out.truncated = true;
                            continue;
                        }
                        if let AgentEventKind::TokenUsageObserved { usage } = event.event {
                            let key = (
                                workspace.clone().unwrap_or_default(),
                                label(&event.event_id),
                            );
                            if !seen.insert(key) {
                                continue;
                            }
                            let idx = usage
                                .context
                                .job_id
                                .as_ref()
                                .and_then(|id| {
                                    index.get(&(
                                        workspace.clone().unwrap_or_default(),
                                        label(id.as_str()),
                                    ))
                                })
                                .copied();
                            if let Some(i) = idx {
                                add_usage(&mut out.jobs[i].tokens, &usage);
                                out.jobs[i].usage_observations += 1;
                            } else if usage.context.job_id.is_none()
                                && q.session.is_none()
                                && q.role.is_none()
                                && q.provider.is_none()
                                && q.model.is_none()
                                && q.task.is_none()
                                && q.job.is_none()
                                && event.timestamp_ms >= q.from_ms
                                && event.timestamp_ms < q.to_ms
                            {
                                add_usage(&mut out.orphan_usage, &usage);
                                out.orphan_observations += 1;
                            }
                        }
                    }
                    JournalEntry::TaskStateChanged {
                        verification: Some(v),
                        ..
                    }
                    | JournalEntry::ExecutionPlanCompleted {
                        verification: v, ..
                    } => {
                        if v.validate().is_err()
                            || !observed_decisions.insert(v.verification_id.clone())
                        {
                            continue;
                        }
                        if let Some(i) = index.get(&(
                            workspace.clone().unwrap_or_default(),
                            label(v.verifier_job_id.as_str()),
                        )) {
                            out.jobs[*i].canonical_decision = Some(tag(&v.decision));
                            if let VerificationTarget::Packet {
                                executor_job_id, ..
                            } = &v.target
                            {
                                out.jobs[*i].accepted_executor =
                                    Some(label(executor_job_id.as_str()));
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        let dispositions: Vec<_> = out
            .jobs
            .iter()
            .filter_map(|j| {
                Some((
                    j.workspace.clone(),
                    j.accepted_executor.clone()?,
                    j.canonical_decision.clone()?,
                ))
            })
            .collect();
        for (w, id, d) in dispositions {
            if let Some(i) = index.get(&(w, id)) {
                out.jobs[*i].executor_disposition = Some(d);
            }
        }
        for j in &mut out.jobs {
            if j.usage_observations == 0
                && let Some(u) = planner_usage.get(&(j.workspace.clone(), j.id.clone()))
            {
                add_usage(&mut j.tokens, u);
                j.usage_observations = 1;
            }
            if j.usage_observations == 0
                || ["RUNNING", "QUEUED", "INTERRUPTED", "CANCELLED"].contains(&j.lifecycle.as_str())
            {
                missing(&mut j.tokens);
            }
            j.usage_quality = j.tokens.total.quality().into();
        }
        let mut job_tasks: BTreeMap<(String, String), Vec<&Job>> = BTreeMap::new();
        for j in &out.jobs {
            if let Some((p, t)) = j.plan.as_ref().zip(j.task.as_ref()) {
                job_tasks.entry((p.clone(), t.clone())).or_default().push(j);
            }
        }
        let mut dependencies = BTreeMap::new();
        let mut replacements: BTreeMap<String, usize> = BTreeMap::new();
        for (id, (p, packet)) in &plans {
            for old in &p.replaced_tasks {
                *replacements.entry(old.clone()).or_default() += 1;
            }
            for t in packet["tasks"].as_array().into_iter().flatten() {
                if let Some(task) = field(t, "task_id") {
                    dependencies.insert(
                        (id.clone(), task),
                        t["dependencies"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .filter_map(Value::as_str)
                            .map(label)
                            .collect::<Vec<_>>(),
                    );
                }
            }
        }
        {
            let mut stmt=tx.prepare("SELECT task_id,plan_id,state_json FROM tasks WHERE repo_id=?1 AND plan_id IN (SELECT value FROM json_each(?2)) ORDER BY task_id LIMIT ?3")?;
            let rows = stmt.query_map(
                params![
                    q.repository,
                    serde_json::to_string(&plans.keys().collect::<Vec<_>>())?,
                    (q.limit + 1) as i64
                ],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                    ))
                },
            )?;
            for (i, row) in rows.enumerate() {
                if i == q.limit {
                    out.truncated = true;
                    break;
                }
                let (id, plan_id, state) = row?;
                let Some((p, _packet)) = plans.get_mut(&plan_id) else {
                    continue;
                };
                let lifecycle =
                    serde_json::from_str::<String>(&state).unwrap_or_else(|_| "UNKNOWN".into());
                p.verified_tasks += usize::from(lifecycle == "VERIFIED");
                if q.session
                    .as_ref()
                    .is_some_and(|s| Some(s) != p.session.as_ref())
                    || q.task.as_ref().is_some_and(|s| s != &id)
                {
                    continue;
                }
                let jobs = job_tasks
                    .get(&(plan_id.clone(), id.clone()))
                    .cloned()
                    .unwrap_or_default();
                if jobs.is_empty()
                    && (q.role.is_some()
                        || q.provider.is_some()
                        || q.model.is_some()
                        || q.job.is_some()
                        || q.lifecycle.is_some())
                {
                    continue;
                }
                let mut t = Task {
                    id: label(&id),
                    plan: label(&plan_id),
                    workspace: p.workspace.clone(),
                    session: p.session.clone(),
                    lifecycle,
                    dependencies: dependencies
                        .get(&(plan_id.clone(), id.clone()))
                        .cloned()
                        .unwrap_or_default(),
                    correction_round: p.correction_round,
                    correction_count: if let Some(n) = replacements.get(&id) {
                        Some(*n)
                    } else if p.correction_round == Some(0) && p.lifecycle != "SUPERSEDED" {
                        Some(0)
                    } else {
                        None
                    },
                    executor_jobs: vec![],
                    verifier_jobs: vec![],
                    helper_jobs: vec![],
                    executor_usage: Tokens::default(),
                    verifier_usage: Tokens::default(),
                    helper_usage: Tokens::default(),
                    rejected_attempt_usage: Tokens::default(),
                    accepted_attempt_usage: Tokens::default(),
                    correction_round_usage: Tokens::default(),
                    summary: summarize(&jobs),
                };
                for j in jobs {
                    match j.role.as_str() {
                        "executor" => {
                            t.executor_jobs.push(j.id.clone());
                            t.executor_usage.merge(&j.tokens)
                        }
                        "verifier" => {
                            t.verifier_jobs.push(j.id.clone());
                            t.verifier_usage.merge(&j.tokens)
                        }
                        _ => {
                            t.helper_jobs.push(j.id.clone());
                            t.helper_usage.merge(&j.tokens)
                        }
                    }
                    match j
                        .executor_disposition
                        .as_deref()
                        .or(j.canonical_decision.as_deref())
                    {
                        Some("PASS") => t.accepted_attempt_usage.merge(&j.tokens),
                        Some("REJECT") => t.rejected_attempt_usage.merge(&j.tokens),
                        _ => {}
                    }
                    if j.correction_round.is_some_and(|n| n > 0) {
                        t.correction_round_usage.merge(&j.tokens);
                    }
                }
                if t.lifecycle == "VERIFIED" {
                    if t.summary.jobs > 0
                        && ["EXACT", "ESTIMATED", "MIXED"]
                            .contains(&t.summary.token_quality.as_str())
                        && !t.executor_jobs.is_empty()
                        && !t.verifier_jobs.is_empty()
                    {
                        out.verified_tasks_with_complete_usage += 1;
                        out.verified_task_usage.merge(&t.summary.tokens);
                    } else {
                        out.verified_tasks_with_partial_usage += 1;
                    }
                }
                out.tasks.push(t);
            }
        }
        out.plans = plans
            .into_values()
            .map(|(p, _)| p)
            .filter(|p| {
                q.session
                    .as_ref()
                    .is_none_or(|s| p.session.as_ref() == Some(s))
            })
            .collect();
        let mut sessions: BTreeMap<(String, String), Vec<&Job>> = BTreeMap::new();
        let mut session_plans: BTreeMap<(String, String), Vec<&Plan>> = BTreeMap::new();
        for p in &out.plans {
            if let Some(id) = &p.session {
                let key = (p.workspace.clone(), id.clone());
                sessions.entry(key.clone()).or_default();
                session_plans.entry(key).or_default().push(p);
            }
        }
        for j in &out.jobs {
            if let Some(s) = &j.session {
                sessions
                    .entry((j.workspace.clone(), s.clone()))
                    .or_default()
                    .push(j);
            }
        }
        for ((workspace, id), jobs) in sessions {
            let plans = session_plans
                .get(&(workspace.clone(), id.clone()))
                .cloned()
                .unwrap_or_default();
            let current = plans
                .iter()
                .filter(|p| p.lifecycle != "SUPERSEDED")
                .max_by_key(|p| p.correction_round);
            out.sessions.push(Session {
                id,
                workspace,
                plans: plans.iter().map(|p| p.id.clone()).collect(),
                lifecycle: current
                    .map(|p| {
                        if ["COMPLETE", "CANCELLED", "SUPERSEDED"].contains(&p.lifecycle.as_str()) {
                            p.lifecycle.clone()
                        } else {
                            p.runtime_lifecycle
                                .clone()
                                .unwrap_or_else(|| p.lifecycle.clone())
                        }
                    })
                    .unwrap_or_else(|| "UNKNOWN".into()),
                current_tasks: current.map(|p| p.task_ids.len()).unwrap_or(0),
                verified: current.map(|p| p.verified_tasks).unwrap_or(0),
                correction_round: current.and_then(|p| p.correction_round),
                summary: summarize(&jobs),
                integration: summarize(
                    &jobs
                        .iter()
                        .copied()
                        .filter(|j| j.role == "verifier" && j.task.is_none())
                        .collect::<Vec<_>>(),
                ),
                unattributed: summarize(
                    &jobs
                        .iter()
                        .copied()
                        .filter(|j| j.task.is_none() && j.role != "verifier")
                        .collect::<Vec<_>>(),
                ),
            });
        }
        let refs: Vec<_> = out.jobs.iter().collect();
        out.summary = summarize(&refs);
        out.integration = summarize(
            &refs
                .iter()
                .copied()
                .filter(|j| j.task.is_none() && j.role == "verifier")
                .collect::<Vec<_>>(),
        );
        out.unattributed = summarize(
            &refs
                .iter()
                .copied()
                .filter(|j| j.task.is_none() && j.role != "verifier")
                .collect::<Vec<_>>(),
        );
        out.roles = groups(&out.jobs, "roles");
        out.routes = groups(&out.jobs, "routes");
        out.providers = groups(&out.jobs, "providers");
        out.models = groups(&out.jobs, "models");
        out.role_providers = groups(&out.jobs, "role_providers");
        let v = &out.verified_task_usage.total;
        out.tokens_per_complete_verified_task =
            if out.verified_tasks_with_complete_usage > 0 && !out.truncated && !v.overflow {
                v.exact
                    .unwrap_or(0)
                    .checked_add(v.estimated.unwrap_or(0))
                    .map(|n| n as f64 / out.verified_tasks_with_complete_usage as f64)
            } else {
                None
            };
        out.task_correction_unknown = out
            .tasks
            .iter()
            .filter(|t| t.correction_count.is_none())
            .count();
        out.task_correction_rate = Rate::new(
            out.tasks
                .iter()
                .filter(|t| t.correction_count.is_some_and(|n| n > 0))
                .count(),
            out.tasks.len() - out.task_correction_unknown,
        );
        for count in out.tasks.iter().filter_map(|t| t.correction_count) {
            *out.task_correction_distribution.entry(count).or_default() += 1;
        }
        if out.truncated {
            out.warnings.push("Bounded history incomplete: counts and distributions describe only selected rows; no complete-population claims.".into());
            out.summary.token_quality = "PARTIAL".into();
        }
        out.warnings.push("Correction rounds are explicit plan lineage. Replaced TaskPackets have distinct IDs; absent one-to-one mappings, per-task correction counts remain UNKNOWN. Cache-write tokens and historical missing prompt dimensions remain UNKNOWN. Failed prelaunch jobs are not assumed to consume zero tokens.".into());
        tx.commit()?;
        out.query.repository = label(&out.query.repository);
        for value in [
            &mut out.query.workspace,
            &mut out.query.session,
            &mut out.query.role,
            &mut out.query.provider,
            &mut out.query.model,
            &mut out.query.task,
            &mut out.query.job,
            &mut out.query.lifecycle,
        ]
        .into_iter()
        .flatten()
        {
            *value = label(value);
        }
        Ok(out)
    }
}
