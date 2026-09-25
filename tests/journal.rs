//! Replacement continuity across real interruptions: a child process
//! journals an action up to a lifecycle boundary and then aborts, and this
//! process, holding none of the child's objects, provider sessions or
//! transcripts, reconstructs the continuation from the store alone.

use std::env;
use std::path::Path;
use std::process::{self, Command};

use agentctl::state::{
    ActionOutcome, ActionStatus, AgentId, AgentScope, Evidence, FailureKind, Intent, InvocationEnd,
    InvocationId, InvocationState, JournalEntry, Role, Store, Usage,
};
use serde_json::json;
use tempfile::TempDir;

const BOUNDARY: &str = "AGENTCTL_TEST_JOURNAL_BOUNDARY";
const STATE: &str = "AGENTCTL_TEST_JOURNAL_STATE";

fn write_intent() -> Intent {
    let parameters = json!({ "path": "src/a.rs", "expected": null });
    Intent {
        action: "source.write".into(),
        parameters: parameters.as_object().unwrap().clone(),
    }
}

/// How the child reconciles its action at `boundary`, if it gets that far.
fn reconciliation(
    boundary: &str,
    invocation: InvocationId,
) -> Option<(ActionOutcome, Vec<Evidence>)> {
    let ran = Evidence::Invocation { invocation };
    let content = |path: &str, n: u8| Evidence::Content {
        path: path.into(),
        hash: Some(format!("{n:064x}")),
    };
    match boundary {
        "reconciled_as_intended" => Some((
            ActionOutcome::CompletedAsIntended,
            vec![ran, content("src/a.rs", 1)],
        )),
        "reconciled_with_deviation" => Some((
            ActionOutcome::CompletedWithDeviation,
            vec![ran, content("src/a.rs", 1), content("src/c.rs", 2)],
        )),
        "reconciled_as_failed" => Some((
            ActionOutcome::Failed,
            vec![
                ran,
                Evidence::Content {
                    path: "src/a.rs".into(),
                    hash: None,
                },
            ],
        )),
        _ => None,
    }
}

fn invocation_end(outcome: ActionOutcome) -> InvocationEnd {
    let succeeded = InvocationEnd {
        state: InvocationState::Succeeded,
        failure: None,
        diagnostic: None,
        exit_code: Some(0),
        provider_session: Some("provider-session".into()),
        usage: Usage::Unavailable,
    };
    match outcome {
        ActionOutcome::Failed => InvocationEnd {
            state: InvocationState::Failed,
            failure: Some(FailureKind::ExitStatus),
            diagnostic: Some("exited with code 1".into()),
            exit_code: Some(1),
            ..succeeded
        },
        _ => succeeded,
    }
}

/// Acts only as the child `interrupted_at` runs: journals up to the
/// boundary it is given, reports the continuation it holds, and aborts.
#[test]
fn interrupted_child() {
    let (Ok(boundary), Some(path)) = (env::var(BOUNDARY), env::var_os(STATE)) else {
        return;
    };
    let mut store = Store::open(Path::new(&path)).unwrap();
    let plan = store.create_plan("continue after interruption").unwrap();
    let agent = store
        .create_agent(Role::Planner, AgentScope::Plan(plan))
        .unwrap();
    if boundary != "before_intend" {
        let entry = store.intend(agent, &write_intent()).unwrap();
        if boundary != "intended" {
            let invocation = store
                .start_invocation(agent, "claude", "model", None)
                .unwrap();
            store.invocation_running(invocation).unwrap();
            store.act(entry, Some(invocation)).unwrap();
            if let Some((outcome, evidence)) = reconciliation(&boundary, invocation) {
                store
                    .finish_invocation(invocation, &invocation_end(outcome))
                    .unwrap();
                store.reconcile(entry, outcome, &evidence).unwrap();
            }
        }
    }
    println!("continuation {:?}", store.continuation(agent).unwrap());
    process::abort();
}

/// Interrupts a child process at `boundary`, then opens the store afresh.
fn interrupted_at(boundary: &str) -> (TempDir, Store, AgentId, Vec<JournalEntry>) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    let child = Command::new(env::current_exe().unwrap())
        .args(["interrupted_child", "--exact", "--nocapture"])
        .env(BOUNDARY, boundary)
        .env(STATE, &path)
        .output()
        .unwrap();
    assert!(!child.status.success(), "the child is interrupted");

    let store = Store::open(&path).unwrap();
    let agent = store
        .events_after(0, 100)
        .unwrap()
        .into_iter()
        .find(|e| e.kind == "agent.created")
        .and_then(|e| e.agent)
        .expect("the child created its agent");
    let continuation = store.continuation(agent).unwrap();
    // The fresh process reconstructs exactly what the child held.
    let stdout = String::from_utf8_lossy(&child.stdout);
    let held = format!("continuation {continuation:?}");
    assert!(stdout.contains(&held), "{boundary}: {stdout}");
    (dir, store, agent, continuation)
}

#[test]
fn interrupted_before_intend_leaves_nothing_to_continue() {
    let (_dir, store, agent, continuation) = interrupted_at("before_intend");
    assert_eq!(continuation, []);
    assert_eq!(store.invocations(agent).unwrap(), []);
}

#[test]
fn intended_without_act_was_never_attempted() {
    let (_dir, store, agent, continuation) = interrupted_at("intended");
    let [entry] = &continuation[..] else {
        panic!("{continuation:?}")
    };
    assert_eq!(entry.agent, agent);
    assert_eq!(entry.intent, write_intent());
    assert_eq!(entry.status, ActionStatus::NotAttempted);
    assert_eq!(store.invocations(agent).unwrap(), []);
}

#[test]
fn act_without_reconcile_is_attempted_with_outcome_unknown() {
    let (_dir, store, agent, continuation) = interrupted_at("attempted");
    let [entry] = &continuation[..] else {
        panic!("{continuation:?}")
    };
    assert_eq!(entry.intent, write_intent());
    let ActionStatus::OutcomeUnknown(attempt) = &entry.status else {
        panic!("{:?}", entry.status)
    };
    assert!(attempt.at >= entry.intended_at);
    let invocation = store.invocation(attempt.invocation.unwrap()).unwrap();
    assert_eq!(invocation.agent, agent);
    // agentctl never learned how the invocation ended, and assumes nothing:
    // no end, no provider session, and nothing retried in its place.
    assert_eq!(
        (invocation.state, invocation.end),
        (InvocationState::Running, None)
    );
    assert_eq!(store.invocations(agent).unwrap().len(), 1);
}

#[test]
fn reconciled_actions_are_reconstructed_unambiguously() {
    for boundary in [
        "reconciled_as_intended",
        "reconciled_with_deviation",
        "reconciled_as_failed",
    ] {
        let (_dir, store, _, continuation) = interrupted_at(boundary);
        let [entry] = &continuation[..] else {
            panic!("{boundary}: {continuation:?}")
        };
        assert_eq!(entry.intent, write_intent(), "{boundary}");
        let ActionStatus::Reconciled(attempt, reconciliation) = &entry.status else {
            panic!("{boundary}: {:?}", entry.status)
        };
        let invocation = attempt.invocation.unwrap();
        let (outcome, evidence) = self::reconciliation(boundary, invocation).unwrap();
        assert_eq!(
            (reconciliation.outcome, &reconciliation.evidence),
            (outcome, &evidence),
            "{boundary}"
        );
        assert!(reconciliation.at >= attempt.at, "{boundary}");
        let recorded = store.invocation(invocation).unwrap();
        assert_eq!(recorded.end, Some(invocation_end(outcome)), "{boundary}");
    }
}
