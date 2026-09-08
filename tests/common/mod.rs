use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};

use agentctl::protocol::*;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

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
            json!({"version": "1", "experiment_id": "experiment:1", "command": command(), "input_refs": ["data:training"], "source_state": {"revision": "git:abc123", "worktree_diff_hash": null}, "metric_refs": ["metric:loss"], "output_refs": ["artifact:model"], "decision_boundaries": [{"boundary_id": "exit", "condition": {"kind": "PROCESS_EXIT"}}, {"boundary_id": "threshold", "condition": {"kind": "METRIC_THRESHOLD", "metric": "loss", "comparison": "LESS_THAN", "value": 0.5}}]}),
        ),
        ("experiment-event", json!(experiment_event())),
        (
            "memory-provenance",
            json!({"version": "1", "trust_class": "AGENT_NOTE", "source_refs": ["task:a"], "evidence": [], "author_job_id": "job:executor-a"}),
        ),
    ])
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
