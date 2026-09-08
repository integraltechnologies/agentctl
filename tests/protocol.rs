mod common;

use std::{collections::BTreeMap, fmt::Debug, path::Path, process::Command};

use agentctl::{Validate, ValidationError, protocol::*, schema};
use common::*;
use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Value, json};

fn roundtrip<T: Serialize + DeserializeOwned + PartialEq + Debug>(value: T) {
    let encoded = serde_json::to_string(&value).unwrap();
    let decoded: T = serde_json::from_str(&encoded).unwrap();
    assert_eq!(value, decoded);
    assert_eq!(encoded, serde_json::to_string(&decoded).unwrap());
}

#[test]
fn every_public_document_roundtrips_and_validates() {
    let fixtures = samples();
    assert_eq!(fixtures.len(), schema::DOCUMENT_TYPES.len());
    for (kind, fixture) in &fixtures {
        schema::validate_json(kind, &fixture.to_string()).unwrap();
    }
    macro_rules! check {
        ($($kind:literal => $ty:ty),+ $(,)?) => { $(roundtrip::<$ty>(decode(fixtures[$kind].clone()));)+ };
    }
    check!("plan" => PlanPacket, "task" => TaskPacket, "result" => ResultPacket,
        "verification" => VerificationPacket, "resume" => ResumePacket,
        "evidence" => EvidenceRecord, "agent-job" => AgentJob, "agent-event" => AgentEvent,
        "probe" => ProbeSnapshot, "token-usage" => TokenUsageEvent,
        "experiment" => ExperimentSpec, "experiment-event" => ExperimentEvent,
        "memory-provenance" => MemoryProvenance);
    roundtrip(verification(true));
    roundtrip(finding());
    roundtrip(EvidenceRef(EvidenceId::new("evidence:1").unwrap()));
}

#[test]
fn duplicate_task_ids_are_rejected() {
    let mut value = plan();
    value.tasks.push(value.tasks[0].clone());
    assert_eq!(
        value.validate(),
        Err(ValidationError::DuplicateTask("a".into()))
    );
}

#[test]
fn missing_dependencies_are_rejected() {
    let mut value = plan();
    value.tasks[1].dependencies = vec![TaskId::new("missing").unwrap()];
    assert_eq!(
        value.validate(),
        Err(ValidationError::MissingDependency {
            task: "b".into(),
            dependency: "missing".into()
        })
    );
}

#[test]
fn cycles_are_rejected_even_in_disconnected_components() {
    let mut value = plan();
    value.tasks[0].dependencies.push(TaskId::new("b").unwrap());
    value.tasks.push(task("independent", &[]));
    assert_eq!(value.validate(), Err(ValidationError::DependencyCycle));
}

#[test]
fn self_dependencies_are_rejected() {
    assert_eq!(
        task("a", &["a"]).validate(),
        Err(ValidationError::SelfDependency("a".into()))
    );
}

#[test]
fn task_requires_objective_done_and_verification() {
    let base = json!(task("a", &[]));
    for (field, value) in [
        ("objective", json!(" \n")),
        ("definition_of_done", json!([])),
        ("definition_of_done", json!([" "])),
    ] {
        let mut invalid = base.clone();
        invalid[field] = value;
        assert!(
            schema::validate_json("task", &invalid.to_string()).is_err(),
            "{field}"
        );
    }
    let mut value = task("a", &[]);
    value.verification.requirement_refs.clear();
    assert!(value.validate().is_err());
    let mut missing = base;
    missing.as_object_mut().unwrap().remove("verification");
    assert!(serde_json::from_value::<TaskPacket>(missing).is_err());
}

#[test]
fn empty_plans_and_repeated_dependencies_are_rejected() {
    let mut value = plan();
    value.tasks.clear();
    assert!(value.validate().is_err());
    assert!(task("b", &["a", "a"]).validate().is_err());
    let mut value = plan();
    value.integration_verification.requirement_refs.clear();
    assert!(value.validate().is_err());
}

#[test]
fn valid_diamond_and_deep_dag_are_accepted() {
    let mut value = plan();
    value.tasks = vec![
        task("d", &["b", "c"]),
        task("c", &["a"]),
        task("a", &[]),
        task("b", &["a"]),
    ];
    value.validate().unwrap();
    value.tasks = (0..2000)
        .map(|i| {
            if i == 0 {
                task("0", &[])
            } else {
                task(&i.to_string(), &[&(i - 1).to_string()])
            }
        })
        .collect();
    value.validate().unwrap();
}

#[test]
fn ids_and_scope_paths_reject_ambiguous_or_unbounded_values() {
    for id in ["", " ", "a/b", "a\n", "é", ":prefix"] {
        assert!(TaskId::new(id).is_err());
        assert!(serde_json::from_value::<TaskId>(json!(id)).is_err());
    }
    assert!(TaskId::new("x".repeat(129)).is_err());
    assert!(TaskId::new("x".repeat(128)).is_ok());
    for path in [
        "",
        "/tmp/file",
        "../secret",
        "src/../secret",
        ".",
        "src//file",
        "src/",
        "src/**",
        "C:\\file",
        "src\n/file",
    ] {
        let mut value = task("a", &[]);
        value.write_scope = vec![ScopePath::File { path: path.into() }];
        assert!(value.validate().is_err(), "{path:?}");
    }
    let mut value = task("a", &[]);
    value.write_scope.clear(); // Explicit read-only task, not unbounded write access.
    value.validate().unwrap();
}

#[test]
fn versions_and_unknown_fields_fail_closed() {
    for kind in schema::DOCUMENT_TYPES {
        let mut value = samples()[kind].clone();
        value["version"] = json!("2");
        assert!(
            schema::validate_json(kind, &value.to_string()).is_err(),
            "{kind}"
        );
        value.as_object_mut().unwrap().remove("version");
        assert!(
            schema::validate_json(kind, &value.to_string()).is_err(),
            "{kind}"
        );
    }
    let mut value = json!(task("a", &[]));
    value["executor_reasoning"] = json!("unsupported");
    assert!(serde_json::from_value::<TaskPacket>(value).is_err());
}

#[test]
fn verification_target_is_structurally_unambiguous() {
    assert_eq!(
        verification(false).target.scope(),
        VerificationScope::Packet
    );
    assert_eq!(
        verification(true).target.scope(),
        VerificationScope::Integration
    );
    for target in [
        json!({"scope": "PACKET", "plan_id": "plan:1", "executor_job_id": "job:1"}),
        json!({"scope": "INTEGRATION", "task_id": "a", "executor_job_ids": ["job:1"]}),
        json!({"scope": "PACKET", "task_id": "a", "plan_id": "plan:1", "executor_job_id": "job:1"}),
    ] {
        assert!(serde_json::from_value::<VerificationTarget>(target).is_err());
    }
}

#[test]
fn verification_requires_independence_and_consistent_findings() {
    for integration in [false, true] {
        let mut value = verification(integration);
        value.verifier_job_id = JobId::new("job:executor-a").unwrap();
        assert!(value.validate().is_err());
    }
    let mut value = verification(false);
    value.findings.push(finding());
    assert!(value.validate().is_err());
    value.decision = VerificationDecision::Reject;
    value.validate().unwrap();
    value.findings.clear();
    assert!(value.validate().is_err());
    value.decision = VerificationDecision::Blocked;
    value.notes = None;
    assert!(value.validate().is_err());
    value.notes = Some("Test environment unavailable".into());
    value.validate().unwrap();
}

#[test]
fn only_verified_dependencies_unlock_ready_and_execution() {
    let value = plan();
    let a = TaskId::new("a").unwrap();
    let b = TaskId::new("b").unwrap();
    for upstream in [
        TaskState::Planned,
        TaskState::Ready,
        TaskState::Executing,
        TaskState::AwaitingVerification,
        TaskState::Verifying,
        TaskState::Rejected,
        TaskState::Blocked,
    ] {
        let states = BTreeMap::from([(a.clone(), upstream), (b.clone(), TaskState::Planned)]);
        assert!(
            value
                .validate_task_transition(&b, &states, TaskState::Ready, None)
                .is_err(),
            "{upstream:?}"
        );
        assert!(!value.task_is_runnable(&b, &states).unwrap());
    }
    let mut states = BTreeMap::from([(b.clone(), TaskState::Planned)]);
    assert!(
        value
            .validate_task_transition(&b, &states, TaskState::Ready, None)
            .is_err()
    );
    states.insert(a, TaskState::Verified);
    value
        .validate_task_transition(&b, &states, TaskState::Ready, None)
        .unwrap();
    states.insert(b.clone(), TaskState::Ready);
    assert!(value.task_is_runnable(&b, &states).unwrap());
    value
        .validate_task_transition(&b, &states, TaskState::Executing, None)
        .unwrap();
}

#[test]
fn verification_proof_must_match_task_decision_checks_and_evidence() {
    let value = plan();
    let a = TaskId::new("a").unwrap();
    let states = BTreeMap::from([(a.clone(), TaskState::Verifying)]);
    assert!(
        value
            .validate_task_transition(&a, &states, TaskState::Verified, None)
            .is_err()
    );
    for mutation in 0..7 {
        let mut proof = verification(false);
        match mutation {
            0 => proof = verification(true),
            1 => {
                proof.target = VerificationTarget::Packet {
                    task_id: TaskId::new("b").unwrap(),
                    executor_job_id: JobId::new("job:executor-b").unwrap(),
                }
            }
            2 => {
                proof.decision = VerificationDecision::Reject;
                proof.findings.push(finding());
            }
            3 => proof.requirement_refs = vec!["check:unrelated".into()],
            4 => proof.evidence.clear(),
            5 => proof.invariant_refs.clear(),
            _ => proof.verifier_job_id = JobId::new("job:executor-a").unwrap(),
        }
        assert!(
            value
                .validate_task_transition(&a, &states, TaskState::Verified, Some(&proof))
                .is_err(),
            "mutation {mutation}"
        );
    }
    value
        .validate_task_transition(&a, &states, TaskState::Verified, Some(&verification(false)))
        .unwrap();
}

#[test]
fn lifecycle_has_no_verification_shortcut_or_rejection_loop() {
    let value = plan();
    let a = TaskId::new("a").unwrap();
    let mut states = BTreeMap::from([(a.clone(), TaskState::Planned)]);
    for next in [
        TaskState::Ready,
        TaskState::Executing,
        TaskState::AwaitingVerification,
        TaskState::Verifying,
    ] {
        value
            .validate_task_transition(&a, &states, next, None)
            .unwrap();
        assert!(!next.is_complete());
        states.insert(a.clone(), next);
    }
    let mut rejection = verification(false);
    rejection.decision = VerificationDecision::Reject;
    rejection.findings.push(finding());
    value
        .validate_task_transition(&a, &states, TaskState::Rejected, Some(&rejection))
        .unwrap();
    for terminal in [TaskState::Verified, TaskState::Rejected] {
        for next in [
            TaskState::Planned,
            TaskState::Ready,
            TaskState::Executing,
            TaskState::AwaitingVerification,
            TaskState::Verifying,
            TaskState::Verified,
            TaskState::Rejected,
            TaskState::Blocked,
        ] {
            assert!(!terminal.can_transition_to(next));
        }
    }
    states.insert(a.clone(), TaskState::Executing);
    assert!(
        value
            .validate_task_transition(&a, &states, TaskState::Verified, Some(&verification(false)))
            .is_err()
    );
    assert!(TaskState::Verified.is_complete());
    assert!(!TaskState::Rejected.is_complete());
    assert!(TaskState::Blocked.can_transition_to(TaskState::Planned));
    assert!(!TaskState::Blocked.can_transition_to(TaskState::Executing));
}

#[test]
fn state_snapshots_reject_unknown_tasks_and_inconsistent_dependencies() {
    let value = plan();
    let b = TaskId::new("b").unwrap();
    let states = BTreeMap::from([(b.clone(), TaskState::Ready)]);
    assert!(value.task_is_runnable(&b, &states).is_err());
    let unknown = TaskId::new("unknown").unwrap();
    assert!(value.task_is_runnable(&unknown, &BTreeMap::new()).is_err());
    assert!(
        value
            .task_is_runnable(&b, &BTreeMap::from([(unknown, TaskState::Planned)]))
            .is_err()
    );
}

#[test]
fn integration_pass_is_required_after_all_packets_are_verified() {
    let value = plan();
    let mut states = BTreeMap::from([
        (TaskId::new("a").unwrap(), TaskState::Verified),
        (TaskId::new("b").unwrap(), TaskState::Verifying),
    ]);
    assert!(
        value
            .validate_completion(&states, &verification(true))
            .is_err()
    );
    states.insert(TaskId::new("b").unwrap(), TaskState::Verified);
    assert!(
        value
            .validate_completion(&states, &verification(false))
            .is_err()
    );
    for mutation in 0..5 {
        let mut proof = verification(true);
        match mutation {
            0 => {
                proof.target = VerificationTarget::Integration {
                    plan_id: PlanId::new("other-plan").unwrap(),
                    executor_job_ids: vec![JobId::new("job:executor-a").unwrap()],
                }
            }
            1 => {
                proof.decision = VerificationDecision::Reject;
                proof.findings.push(finding());
            }
            2 => proof.requirement_refs = vec!["check:unit".into()],
            3 => proof.evidence.clear(),
            _ => proof.invariant_refs.clear(),
        }
        assert!(
            value.validate_completion(&states, &proof).is_err(),
            "mutation {mutation}"
        );
    }
    value
        .validate_completion(&states, &verification(true))
        .unwrap();
}

#[test]
fn job_lifecycle_is_separate_from_task_completion() {
    for (from, to) in [
        (JobState::Queued, JobState::Running),
        (JobState::Running, JobState::Waiting),
        (JobState::Waiting, JobState::Running),
        (JobState::Running, JobState::Succeeded),
        (JobState::Queued, JobState::Cancelled),
    ] {
        from.validate_transition(to).unwrap();
    }
    for terminal in [JobState::Succeeded, JobState::Failed, JobState::Cancelled] {
        assert!(terminal.is_terminal());
        assert!(terminal.validate_transition(JobState::Running).is_err());
    }
    assert!(
        JobState::Queued
            .validate_transition(JobState::Succeeded)
            .is_err()
    );
    let mut job: AgentJob = decode(samples()["agent-job"].clone());
    job.validate().unwrap();
    job.state = JobState::Succeeded;
    assert!(job.validate().is_err());
    job.finished_at_ms = Some(120);
    job.validate().unwrap();
    job.task_id = None;
    assert!(job.validate().is_err());
}

#[test]
fn token_provenance_is_required_and_unknown_is_not_zero() {
    let mut value = json!(token());
    value.as_object_mut().unwrap().remove("provenance");
    assert!(serde_json::from_value::<TokenUsageEvent>(value).is_err());
    let mut usage = token();
    usage.provenance = TokenUsageProvenance::Unknown;
    assert!(usage.validate().is_err());
    usage.input_tokens = None;
    usage.output_tokens = None;
    usage.cached_tokens = None;
    usage.total_tokens = None;
    usage.validate().unwrap();
    roundtrip(usage.clone());
    usage.total_tokens = Some(0);
    assert!(usage.validate().is_err());
    usage.provenance = TokenUsageProvenance::Exact;
    usage.validate().unwrap();
    roundtrip(usage);
}

#[test]
fn roles_require_no_provider_and_opaque_provider_names_roundtrip() {
    for role in [AgentRole::Planner, AgentRole::Executor, AgentRole::Verifier] {
        let mut job: AgentJob = decode(samples()["agent-job"].clone());
        job.role = role;
        job.provider = None;
        job.validate().unwrap();
        roundtrip(job.clone());
        job.provider = Some(ProviderMetadata {
            provider: "future/vendor-42".into(),
            model: Some("arbitrary model identity".into()),
        });
        job.validate().unwrap();
        roundtrip(job);
    }
    let mut invalid = context();
    invalid.packet_id = Some(TaskId::new("other").unwrap());
    assert!(invalid.validate().is_err());
}

#[test]
fn evidence_resume_and_memory_preserve_state_and_provenance() {
    let mut evidence: EvidenceRecord = decode(samples()["evidence"].clone());
    evidence.finished_at_ms = Some(99);
    assert!(evidence.validate().is_err());
    evidence.finished_at_ms = None;
    assert!(evidence.validate().is_err());
    let mut resume: ResumePacket = decode(samples()["resume"].clone());
    resume.completed_tasks.push(TaskId::new("b").unwrap());
    assert!(resume.validate().is_err());
    resume.completed_tasks.pop();
    resume.phase = ResumePhase::Complete;
    assert!(resume.validate().is_err());
    resume.phase = ResumePhase::Executing;
    resume.active_task.as_mut().unwrap().state = TaskState::Verified;
    assert!(resume.validate().is_err());
    let mut memory: MemoryProvenance = decode(samples()["memory-provenance"].clone());
    memory.author_job_id = None;
    assert!(memory.validate().is_err());
    for class in [
        MemoryTrustClass::Canonical,
        MemoryTrustClass::Derived,
        MemoryTrustClass::Observed,
        MemoryTrustClass::AgentNote,
    ] {
        roundtrip(class);
    }
}

#[test]
fn failed_results_require_failure_information() {
    let mut result: ResultPacket = decode(samples()["result"].clone());
    result.status = ResultStatus::Failed;
    assert!(result.validate().is_err());
    result.failure = Some(FailureInfo {
        code: "check-failed".into(),
        summary: "Unit test failed".into(),
    });
    result.validate().unwrap();
    result.status = ResultStatus::Blocked;
    result.validate().unwrap();
    result.status = ResultStatus::Succeeded;
    assert!(result.validate().is_err());
}

#[test]
fn every_observable_event_variant_roundtrips() {
    let variants = vec![
        json!({"kind": "AGENT_STARTED"}),
        json!({"kind": "AGENT_FINISHED", "state": "SUCCEEDED"}),
        json!({"kind": "TASK_PACKET_LOADED", "task_id": "a"}),
        json!({"kind": "PLAN_STEP_STARTED", "step": "Check parser"}),
        json!({"kind": "FILE_READ", "path": "src/parser.rs"}),
        json!({"kind": "SYMBOL_READ", "entity": "entity:parser"}),
        json!({"kind": "FILE_EDITED", "path": "src/parser.rs"}),
        json!({"kind": "TOOL_STARTED", "invocation_id": "tool:1", "tool": "test"}),
        json!({"kind": "TOOL_FINISHED", "invocation_id": "tool:1", "succeeded": true, "evidence": ["evidence:1"]}),
        json!({"kind": "COMMAND_STARTED", "invocation_id": "command:1", "command": command()}),
        json!({"kind": "COMMAND_FINISHED", "invocation_id": "command:1", "exit_status": 0, "evidence": ["evidence:1"]}),
        json!({"kind": "VERIFICATION_STARTED", "target": verification(false).target}),
        json!({"kind": "VERIFICATION_CHECK_STARTED", "requirement_ref": "check:unit"}),
        json!({"kind": "VERIFICATION_FINDING_CREATED", "finding": finding()}),
        json!({"kind": "WAITING_ON_DEPENDENCY", "task_ids": ["b"]}),
        json!({"kind": "EXPERIMENT_BOUNDARY", "boundary": experiment_event()}),
        json!({"kind": "TOKEN_USAGE_OBSERVED", "usage": token()}),
    ];
    for event in variants {
        let value: AgentEvent = decode(
            json!({"version": "1", "event_id": "event:1", "timestamp_ms": 120, "context": context(), "event": event}),
        );
        value.validate().unwrap();
        roundtrip(value);
    }
}

#[test]
fn nested_observations_cannot_disagree_with_the_event_envelope() {
    let mut event: AgentEvent = decode(samples()["agent-event"].clone());
    let mut usage = token();
    usage.timestamp_ms += 1;
    event.event = AgentEventKind::TokenUsageObserved { usage };
    assert!(event.validate().is_err());
    event.event = AgentEventKind::AgentFinished {
        state: JobState::Running,
    };
    assert!(event.validate().is_err());
    let mut probe: ProbeSnapshot = decode(samples()["probe"].clone());
    probe.idle_ms = probe.elapsed_ms + 1;
    assert!(probe.validate().is_err());
}

#[test]
fn experiment_boundaries_are_generic_and_finite() {
    for boundary in [
        ExperimentBoundary::ProcessExit,
        ExperimentBoundary::Crash,
        ExperimentBoundary::NoProgress { timeout_ms: 1000 },
        ExperimentBoundary::NanMetric {
            metric: "loss".into(),
        },
        ExperimentBoundary::EpochComplete,
        ExperimentBoundary::MetricThreshold {
            metric: "loss".into(),
            comparison: MetricComparison::LessThan,
            value: 0.1,
        },
    ] {
        boundary.validate().unwrap();
        roundtrip(boundary);
    }
    assert!(
        ExperimentBoundary::NoProgress { timeout_ms: 0 }
            .validate()
            .is_err()
    );
    let mut event = experiment_event();
    event.metrics.insert("loss".into(), f64::NAN);
    assert!(event.validate().is_err());
    let mut experiment: ExperimentSpec = decode(samples()["experiment"].clone());
    experiment
        .decision_boundaries
        .push(experiment.decision_boundaries[0].clone());
    assert!(experiment.validate().is_err());
}

fn assert_local_refs_resolve(node: &Value, root: &Value) {
    match node {
        Value::Object(map) => {
            if let Some(reference) = map.get("$ref") {
                let pointer = reference
                    .as_str()
                    .unwrap()
                    .strip_prefix('#')
                    .expect("schemas must be self-contained");
                assert!(
                    root.pointer(pointer).is_some(),
                    "unresolved reference {reference}"
                );
            }
            for child in map.values() {
                assert_local_refs_resolve(child, root);
            }
        }
        Value::Array(values) => {
            for child in values {
                assert_local_refs_resolve(child, root);
            }
        }
        _ => {}
    }
}

#[test]
fn schemas_are_deterministic_self_contained_and_match_checked_in_output() {
    let first = schema::schemas();
    let second = schema::schemas();
    assert_eq!(first.len(), 13);
    for (name, document) in first {
        let json = serde_json::to_value(&document).unwrap();
        assert_eq!(
            json["$schema"],
            "https://json-schema.org/draft/2020-12/schema"
        );
        assert_eq!(json["type"], "object");
        assert_eq!(json["additionalProperties"], false);
        assert_local_refs_resolve(&json, &json);
        let encoded = format!("{}\n", serde_json::to_string_pretty(&document).unwrap());
        assert_eq!(
            encoded,
            format!("{}\n", serde_json::to_string_pretty(&second[name]).unwrap())
        );
        let committed = std::fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("schemas")
                .join(name),
        )
        .expect("run cargo run -- schemas generate before tests");
        assert_eq!(encoded, committed, "regenerate stale schema {name}");
    }
    let task_schema = serde_json::to_value(&schema::schemas()["task.schema.json"]).unwrap();
    assert_eq!(task_schema["$defs"]["TaskId"]["minLength"], 1);
    assert_eq!(task_schema["$defs"]["TaskId"]["maxLength"], 128);
    assert!(task_schema["$defs"]["TaskId"]["pattern"].is_string());
}

#[test]
fn cli_generates_schemas_and_validates_every_document_with_useful_failures() {
    let temp = TempDir::new();
    let cli = env!("CARGO_BIN_EXE_agentctl");
    let version = Command::new(cli).arg("--version").output().unwrap();
    assert!(version.status.success());
    assert!(
        String::from_utf8(version.stdout)
            .unwrap()
            .contains(env!("CARGO_PKG_VERSION"))
    );
    let generated = Command::new(cli)
        .args(["schemas", "generate", "--output"])
        .arg(&temp.0)
        .output()
        .unwrap();
    assert!(generated.status.success(), "{generated:?}");
    for (name, document) in schema::schemas() {
        assert_eq!(
            std::fs::read_to_string(temp.0.join(name)).unwrap(),
            format!("{}\n", serde_json::to_string_pretty(&document).unwrap())
        );
    }
    for (kind, fixture) in samples() {
        let path = temp.0.join(format!("{kind}.json"));
        std::fs::write(&path, fixture.to_string()).unwrap();
        let result = Command::new(cli)
            .args(["protocol", "validate", kind])
            .arg(&path)
            .output()
            .unwrap();
        assert!(result.status.success(), "{kind}: {result:?}");
    }
    let path = temp.0.join("invalid.json");
    let mut invalid = json!(task("a", &[]));
    invalid["objective"] = json!(" ");
    std::fs::write(&path, invalid.to_string()).unwrap();
    let result = Command::new(cli)
        .args(["protocol", "validate", "task"])
        .arg(&path)
        .output()
        .unwrap();
    assert!(!result.status.success());
    assert!(
        String::from_utf8(result.stderr)
            .unwrap()
            .contains("task.objective")
    );
    for args in [
        vec!["unknown"],
        vec!["schemas", "generate", "--output"],
        vec!["protocol", "validate", "unknown", "missing.json"],
        vec!["protocol", "validate", "task", "missing.json"],
    ] {
        assert!(
            !Command::new(cli)
                .args(args)
                .current_dir(&temp.0)
                .output()
                .unwrap()
                .status
                .success()
        );
    }
    assert!(schema::validate_json("unknown", "{}").is_err());
    assert!(schema::validate_json("task", "not json").is_err());
}
