use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};

use agentctl::protocol::*;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
#[allow(dead_code)]
pub fn sql(path: &std::path::Path) -> rusqlite::Connection {
    let c = rusqlite::Connection::open(path).unwrap();
    c.create_scalar_function(
        "agentctl_runtime_authorized",
        2,
        rusqlite::functions::FunctionFlags::SQLITE_INNOCUOUS,
        |_| Ok(false),
    )
    .unwrap();
    c
}
#[allow(dead_code)]
pub fn strip_experiments(c: &rusqlite::Connection) {
    strip_experiment_events(c);
    for name in [
        "experiment_runs_insert",
        "experiment_runs_update",
        "experiment_runs_delete",
    ] {
        c.execute_batch(&format!("DROP TRIGGER IF EXISTS {name};"))
            .unwrap();
    }
    c.execute_batch(
        "DROP TABLE IF EXISTS experiment_runs; DELETE FROM schema_migrations WHERE version=8;",
    )
    .unwrap();
}
#[allow(dead_code)]
pub fn strip_experiment_events(c: &rusqlite::Connection) {
    strip_experiment_decisions(c);
    for name in [
        "experiment_events_insert",
        "experiment_events_update",
        "experiment_events_delete",
    ] {
        c.execute_batch(&format!("DROP TRIGGER IF EXISTS {name};"))
            .unwrap();
    }
    c.execute_batch(
        "DROP TABLE IF EXISTS experiment_events; DELETE FROM schema_migrations WHERE version=9;",
    )
    .unwrap();
}
#[allow(dead_code)]
pub fn strip_experiment_decisions(c: &rusqlite::Connection) {
    strip_experiment_decision_cursors(c);
    for name in [
        "experiment_decisions_insert",
        "experiment_decisions_update",
        "experiment_decisions_delete",
        "experiment_wakeups_insert",
        "experiment_wakeups_update",
        "experiment_wakeups_delete",
    ] {
        c.execute_batch(&format!("DROP TRIGGER IF EXISTS {name};"))
            .unwrap();
    }
    c.execute_batch(
        "DROP TABLE IF EXISTS experiment_wakeups; DROP TABLE IF EXISTS experiment_decisions; DELETE FROM schema_migrations WHERE version=10;",
    )
    .unwrap();
}
/// Removes the additive v13 ontology-lifecycle schema, and every migration
/// layered above it, so a test can restore an older accepted schema. Every
/// older downgrade path strips it first.
#[allow(dead_code)]
pub fn strip_ontology_lifecycle(c: &rusqlite::Connection) {
    c.execute_batch(
        // Simulating a v12 database: every later migration goes too.
        "DROP TABLE IF EXISTS ontology_generations; DROP TABLE IF EXISTS ontology_blobs; DELETE FROM schema_migrations WHERE version>=13;",
    )
    .unwrap();
    let hashed = c
        .prepare("SELECT 1 FROM pragma_table_info('graph_entities') WHERE name='text_hash'")
        .unwrap()
        .exists([])
        .unwrap();
    if hashed {
        c.execute_batch("ALTER TABLE graph_entities DROP COLUMN text_hash;")
            .unwrap();
    }
}
/// Removes the additive v12 graph-resolution schema (and everything newer).
#[allow(dead_code)]
pub fn strip_graph_resolutions(c: &rusqlite::Connection) {
    strip_ontology_lifecycle(c);
    c.execute_batch(
        // Simulating a v11 database means removing every migration layered on
        // top of it, not just the one that created this table.
        "DROP TABLE IF EXISTS graph_resolutions; DROP INDEX IF EXISTS graph_edges_hinted; DELETE FROM schema_migrations WHERE version>=12;",
    )
    .unwrap();
    let hinted = c
        .prepare("SELECT 1 FROM pragma_table_info('graph_edges') WHERE name='path_hint'")
        .unwrap()
        .exists([])
        .unwrap();
    if hinted {
        c.execute_batch("ALTER TABLE graph_edges DROP COLUMN path_hint;")
            .unwrap();
    }
}
#[allow(dead_code)]
pub fn strip_experiment_decision_cursors(c: &rusqlite::Connection) {
    strip_graph_resolutions(c);
    for name in [
        "experiment_decision_cursors_insert",
        "experiment_decision_cursors_update",
        "experiment_decision_cursors_delete",
    ] {
        c.execute_batch(&format!("DROP TRIGGER IF EXISTS {name};"))
            .unwrap();
    }
    c.execute_batch(
        "DROP TABLE IF EXISTS experiment_decision_cursors; DELETE FROM schema_migrations WHERE version=11;",
    )
    .unwrap();
}
#[allow(dead_code)]
pub fn strip_runtime(c: &rusqlite::Connection) {
    strip_experiments(c);
    for name in [
        "runtime_runs_insert",
        "runtime_runs_update",
        "runtime_runs_delete",
        "runtime_jobs_insert",
        "runtime_jobs_update",
        "runtime_jobs_delete",
        "runtime_task_gate",
        "runtime_job_create_gate",
        "runtime_job_update_gate",
        "runtime_plan_gate",
    ] {
        c.execute_batch(&format!("DROP TRIGGER IF EXISTS {name};"))
            .unwrap();
    }
    c.execute_batch("DROP TABLE IF EXISTS runtime_jobs; DROP TABLE IF EXISTS runtime_runs; DELETE FROM schema_migrations WHERE version=7;").unwrap();
}

/// A human's deliberate ontology decision: accept the workspace's open ontology
/// candidate (an observed change that indexing alone never makes canonical).
#[allow(dead_code)]
pub fn accept_observation(
    store: &mut agentctl::local::store::Store,
    root: &std::path::Path,
) -> String {
    let id = store
        .ontology_status(root)
        .unwrap()
        .candidate
        .expect("an open ontology candidate")
        .generation_id;
    store
        .accept_generation(
            root,
            &id,
            Some("deliberate acceptance of the observed change"),
        )
        .unwrap();
    id
}

pub fn decode<T: DeserializeOwned>(value: Value) -> T {
    serde_json::from_value(value).unwrap()
}

pub fn task(id: &str, dependencies: &[&str]) -> TaskPacket {
    decode(json!({
        "version": "1", "task_id": id, "objective": "Add bounded parser validation",
        "read_scope": [{"kind": "DIRECTORY", "path": "src"}],
        "write_scope": [{"kind": "FILE", "path": "src/parser.rs"}],
        "graph_entities": ["entity:parser"], "invariant_refs": ["inv:compatibility"],
        "dependencies": dependencies, "definition_of_done": ["Invalid input is rejected"],
        "verification": {"requirement_refs": ["check:unit"], "evidence_required": true}
    }))
}

pub fn plan() -> PlanPacket {
    PlanPacket {
        version: ProtocolVersion::V1,
        plan_id: PlanId::new("plan:1").unwrap(),
        objective: "Validate parser inputs and consumers".into(),
        tasks: vec![task("a", &[]), task("b", &["a"])],
        integration_verification: VerificationRequirements {
            requirement_refs: vec!["check:integration".into()],
            evidence_required: true,
        },
    }
}

pub fn verification(integration: bool) -> VerificationPacket {
    decode(json!({
        "version": "1", "verification_id": "verification:1",
        "target": if integration {
            json!({"scope": "INTEGRATION", "plan_id": "plan:1", "executor_job_ids": ["job:executor-a", "job:executor-b"]})
        } else {
            json!({"scope": "PACKET", "task_id": "a", "executor_job_id": "job:executor-a"})
        },
        "verifier_job_id": "job:verifier", "decision": "PASS", "findings": [],
        "evidence": ["evidence:1"], "requirement_refs": [if integration { "check:integration" } else { "check:unit" }],
        "invariant_refs": ["inv:compatibility"], "notes": "Checks passed against the referenced source state"
    }))
}

pub fn context() -> EventContext {
    decode(
        json!({"agent_id": "agent:1", "plan_id": "plan:1", "task_id": "a", "packet_id": "a", "job_id": "job:executor-a", "role": "EXECUTOR", "provider": null}),
    )
}

pub fn command() -> Value {
    json!({"program": "cargo", "args": ["test"], "cwd": "."})
}

pub fn finding() -> VerificationFinding {
    decode(
        json!({"severity": "ERROR", "requirement_refs": ["check:unit"], "invariant_refs": ["inv:compatibility"], "location": {"path": "src/parser.rs", "graph_entity": "entity:parser", "line": 12}, "problem": "Empty input is accepted"}),
    )
}

pub fn token() -> TokenUsageEvent {
    decode(
        json!({"version": "1", "timestamp_ms": 120, "context": context(), "provenance": "ESTIMATED", "input_tokens": 20, "output_tokens": 10, "cached_tokens": 5, "reasoning_tokens": null, "total_tokens": 30}),
    )
}

pub fn experiment_event() -> ExperimentEvent {
    decode(
        json!({"version": "1", "experiment_id": "experiment:1", "job_id": "job:executor-a", "timestamp_ms": 120, "boundary_id": "epoch", "boundary": {"kind": "EPOCH_COMPLETE"}, "metrics": {"loss": 0.5}, "evidence": ["evidence:1"], "summary": "First epoch completed"}),
    )
}

pub fn samples() -> BTreeMap<&'static str, Value> {
    BTreeMap::from([
        ("task", json!(task("a", &[]))),
        ("plan", json!(plan())),
        (
            "result",
            json!({"version": "1", "task_id": "a", "executor_job_id": "job:executor-a", "status": "SUCCEEDED", "changed_paths": ["src/parser.rs"], "changed_entities": ["entity:parser"], "evidence": ["evidence:1"], "notes": "Added input validation", "failure": null}),
        ),
        ("verification", json!(verification(false))),
        (
            "resume",
            json!({"version": "1", "plan_id": "plan:1", "active_task": {"task_id": "b", "state": "PLANNED"}, "phase": "EXECUTING", "completed_tasks": ["a"], "pending_tasks": ["b"], "latest_verification": "verification:1", "next_action": "Mark task b READY", "source_state": {"revision": "git:abc123", "worktree_diff_hash": null}}),
        ),
        (
            "evidence",
            json!({"version": "1", "evidence_id": "evidence:1", "command": command(), "source_state": {"revision": "git:abc123", "worktree_diff_hash": "sha256:example"}, "started_at_ms": 100, "finished_at_ms": 120, "exit_status": 0, "stdout_hash": "sha256:stdout", "stderr_hash": "sha256:stderr", "full_log_ref": "logs/evidence-1", "summary": "All checks passed"}),
        ),
        (
            "agent-job",
            json!({"version": "1", "job_id": "job:executor-a", "agent_id": "agent:1", "role": "EXECUTOR", "plan_id": "plan:1", "task_id": "a", "state": "RUNNING", "provider": null, "created_at_ms": 90, "started_at_ms": 100, "finished_at_ms": null}),
        ),
        (
            "agent-event",
            json!({"version": "1", "event_id": "event:1", "timestamp_ms": 120, "context": context(), "event": {"kind": "FILE_EDITED", "path": "src/parser.rs"}}),
        ),
        (
            "probe",
            json!({"version": "1", "timestamp_ms": 120, "context": context(), "phase": "implementation", "current_step": "Running checks", "current_target": {"path": "src/parser.rs", "graph_entity": null, "line": null}, "current_tool": "command", "current_command": command(), "last_event_id": "event:1", "elapsed_ms": 20, "idle_ms": 0, "blocker": null, "waiting_on": [], "current_verification_check": null}),
        ),
        ("token-usage", json!(token())),
        (
            "experiment",
            json!({"version": "1", "experiment_id": "experiment:1", "command": command(), "input_refs": ["data:training"], "source_state": {"revision": "git:abc123", "worktree_diff_hash": null}, "metric_refs": ["metric:loss"], "output_refs": ["artifact:model"], "decision_boundaries": [{"boundary_id": "exit", "condition": {"kind": "PROCESS_EXIT"}, "action": {"kind": "RECORD_ONLY"}}, {"boundary_id": "threshold", "condition": {"kind": "METRIC_THRESHOLD", "metric": "loss", "comparison": "LESS_THAN", "value": 0.5}, "action": {"kind": "REQUIRE_PLANNER_REVIEW", "verification_ref": "check:integration"}}]}),
        ),
        ("experiment-event", json!(experiment_event())),
        (
            "memory-provenance",
            json!({"version": "1", "trust_class": "AGENT_NOTE", "source_refs": ["task:a"], "evidence": [], "author_job_id": "job:executor-a"}),
        ),
        (
            "context-request",
            json!({"version": "1", "task_id": "a", "job_id": "job:executor-a", "reason": "The parser caller is not in the issued context", "items": [{"kind": "SYMBOL_RELATIONS", "entity_id": "entity:parser", "relation": "CALLERS"}, {"kind": "FILE_RANGE", "path": "src/parser.rs", "start_line": 1, "end_line": 40}], "max_bytes": 4096}),
        ),
    ])
}

/// Seeds the canonical plan/task rows directly.
///
/// Production creates plans only through `Store::import_execution_plan`, which
/// also requires a prepared planning request, a fresh index and project policy.
/// Storage-layer tests need the rows, not that whole stack, so the privileged
/// setup lives here rather than as a second plan-creation API on `Store`.
#[allow(dead_code)]
pub fn seed_plan(database: &std::path::Path, repo: &str, packet: &PlanPacket) {
    let c = sql(database);
    c.execute(
        "INSERT INTO plans(repo_id,plan_id,packet_json) VALUES (?1,?2,?3)",
        rusqlite::params![
            repo,
            packet.plan_id.as_str(),
            serde_json::to_string(packet).unwrap()
        ],
    )
    .unwrap();
    for task in &packet.tasks {
        c.execute(
            "INSERT INTO tasks(repo_id,task_id,plan_id,state_json) VALUES (?1,?2,?3,'\"PLANNED\"')",
            rusqlite::params![repo, task.task_id.as_str(), packet.plan_id.as_str()],
        )
        .unwrap();
    }
}

/// Reads a job's workspace binding straight from the table. Production reads it
/// through the internal `associated_workspace` helper on paths that need it;
/// this exists so a migration test can assert the binding without the crate
/// shipping a public accessor nothing else uses.
#[allow(dead_code)]
pub fn job_workspace(database: &std::path::Path, repo: &str, job: &str) -> Option<String> {
    sql(database)
        .query_row(
            "SELECT workspace_id FROM jobs WHERE repo_id=?1 AND job_id=?2",
            rusqlite::params![repo, job],
            |row| row.get::<_, String>(0),
        )
        .ok()
}

/// The planner wire contract: a provider returns a decision, and agentctl
/// derives the execution-plan envelope (hashes, frozen source, identities, timestamps)
/// itself. Tests keep building whole `ExecutionPlan`s for `import_execution_plan`,
/// so this projects one back down to what a provider is actually asked for.
#[allow(dead_code)]
pub fn plan_decision(plan: &Value) -> Value {
    let metadata = &plan["metadata"];
    json!({
        "version": "1",
        "packet": plan["packet"],
        "task_contracts": metadata["contracts"].as_array().unwrap_or(&vec![]).iter().map(|c| json!({
            "task_id": c["task_id"],
            "memory_refs": c["memory_refs"],
            "exclusions": c["exclusions"],
            "non_goals": c["non_goals"],
        })).collect::<Vec<_>>(),
        "integration_expectations": metadata["integration"]["expectations"],
        "replan": metadata["replan"],
    })
}

pub struct TempDir(pub PathBuf);

impl TempDir {
    pub fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "agentctl-test-{}-{stamp}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
