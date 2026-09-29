//! `agentctl status` and `agentctl logs`, run as the real binary against a
//! project on disk.

use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use agentctl::config::Config;
use agentctl::state::{
    AgentScope, HumanIntent, InvocationEnd, InvocationState, Role, Store, TokenUsage, Usage,
};

const CONFIG: &str = r#"[project]
name = "demo"
version = "0.1.0"

[codegraph]
roots = ["src"]

[agents]
max_concurrency = 4

[agents.planner]
provider = "claude"
model = "claude-opus-5-5"
reasoning_effort = "high"

[agents.executor]
provider = "claude"
model = "claude-opus-5-5"
reasoning_effort = "medium"

[agents.verifier]
provider = "claude"
model = "claude-opus-5-5"
reasoning_effort = "xhigh"
"#;

const EVIL: &str = "\x1b[31mred\x1b]52;c;SGVsbG8=\x07\u{9b}2J\rover\u{202e}write";
const DEADLINE: Duration = Duration::from_secs(60);

fn project() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    Config::parse(CONFIG).expect("the fixture config parses");
    std::fs::write(dir.path().join("agentctl.toml"), CONFIG).unwrap();
    dir
}

fn open(root: &Path) -> Store {
    std::fs::create_dir_all(root.join(".agentctl")).unwrap();
    Store::open(&root.join(".agentctl/state.db")).unwrap()
}

fn intent(objective: &str) -> HumanIntent {
    HumanIntent {
        objective: objective.into(),
        constraints: vec![],
        completion_criteria: vec![],
    }
}

/// One planner invocation of `plan`, run to a reported end.
fn invoke(store: &mut Store, plan: agentctl::state::PlanId, model: &str) {
    let agent = store
        .create_agent(Role::Planner, AgentScope::Plan(plan))
        .unwrap();
    let id = store
        .start_invocation(agent, "claude", model, None)
        .unwrap();
    store.invocation_running(id).unwrap();
    store
        .finish_invocation(
            id,
            &InvocationEnd {
                state: InvocationState::Succeeded,
                failure: None,
                diagnostic: None,
                exit_code: Some(0),
                provider_session: None,
                usage: Usage::ProviderReported(TokenUsage {
                    input: 1_234,
                    output: 567,
                    cached_input: None,
                    cache_write: None,
                    reasoning: None,
                }),
            },
        )
        .unwrap();
}

fn agentctl(root: &Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_agentctl"))
        .args(args)
        .current_dir(root)
        .output()
        .unwrap()
}

fn stdout_of(root: &Path, args: &[&str]) -> String {
    let out = agentctl(root, args);
    assert!(
        out.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

fn assert_terminal_safe(text: &str) {
    for c in text.chars() {
        assert!(
            c == '\n' || !(c.is_control() || matches!(c, '\u{202a}'..='\u{202e}')),
            "raw {c:?} in {text:?}"
        );
    }
    for byte in [0x1b, 0x07, 0x0d] {
        assert!(!text.as_bytes().contains(&byte), "byte {byte:#x}");
    }
}

/// Two plans: the first invoked twice, the second once.
fn populated() -> tempfile::TempDir {
    let dir = project();
    let mut store = open(dir.path());
    let one = store.create_plan(&intent("first objective")).unwrap();
    let two = store.create_plan(&intent("second objective")).unwrap();
    invoke(&mut store, one, "m1");
    invoke(&mut store, one, "m1");
    invoke(&mut store, two, "m2");
    dir
}

fn seq_of(line: &str) -> i64 {
    line.split_whitespace().next().unwrap().parse().unwrap()
}

#[test]
fn status_without_state_says_so_and_creates_nothing() {
    let dir = project();
    let text = stdout_of(dir.path(), &["status"]);
    assert!(text.contains("no state yet"), "{text}");
    assert!(!dir.path().join(".agentctl").exists());
    let logs = stdout_of(dir.path(), &["logs"]);
    assert!(logs.contains("no state yet"), "{logs}");
    assert!(!dir.path().join(".agentctl").exists());
}

#[test]
fn status_reports_plans_and_usage() {
    let dir = populated();
    let text = stdout_of(dir.path(), &["status"]);
    assert!(text.contains("plan 1 planning"), "{text}");
    assert!(text.contains("plan 2 planning"), "{text}");
    assert!(text.contains("objective: first objective"), "{text}");
    assert!(text.contains("2,468 in / 1,134 out reported"), "{text}");
    assert!(
        text.contains("usage: 3,702 in / 1,701 out reported"),
        "{text}"
    );
    assert!(!text.contains("no end recorded"), "{text}");
}

#[test]
fn logs_show_the_newest_events_in_ascending_order() {
    let dir = populated();
    let all = stdout_of(dir.path(), &["logs", "-n", "1000"]);
    let all: Vec<&str> = all.lines().collect();
    assert!(all.len() > 6);
    let seqs: Vec<i64> = all.iter().map(|l| seq_of(l)).collect();
    assert!(seqs.windows(2).all(|w| w[0] < w[1]));

    let tail = stdout_of(dir.path(), &["logs", "-n", "3"]);
    let tail: Vec<&str> = tail.lines().collect();
    assert_eq!(tail, all[all.len() - 3..]);
    // The default is the newest 50.
    let default = stdout_of(dir.path(), &["logs"]);
    assert_eq!(default.lines().count(), all.len().min(50));
}

#[test]
fn logs_filter_by_kind_plan_and_after() {
    let dir = populated();
    let invocations = stdout_of(dir.path(), &["logs", "--kind", "invocation."]);
    assert!(invocations.lines().count() >= 9);
    assert!(invocations.lines().all(|l| {
        l.split_whitespace()
            .nth(2)
            .unwrap()
            .starts_with("invocation.")
    }));
    let exact = stdout_of(dir.path(), &["logs", "--kind", "invocation.ended"]);
    assert_eq!(exact.lines().count(), 3);

    let plan_two = stdout_of(dir.path(), &["logs", "--plan", "2"]);
    assert!(!plan_two.is_empty());
    assert!(plan_two.lines().all(|l| l.contains("plan 2")), "{plan_two}");

    let all = stdout_of(dir.path(), &["logs", "-n", "1000"]);
    let all: Vec<&str> = all.lines().collect();
    let after = seq_of(all[2]).to_string();
    let page = stdout_of(dir.path(), &["logs", "--after", &after, "-n", "2"]);
    assert_eq!(page.lines().collect::<Vec<_>>(), all[3..5]);
}

#[test]
fn untrusted_text_is_escaped_in_status_and_logs() {
    let dir = project();
    let mut store = open(dir.path());
    // The store refuses control characters in an objective, not bidi ones.
    let plan = store.create_plan(&intent("a\u{202e}b\u{2066}c")).unwrap();
    invoke(&mut store, plan, EVIL);
    drop(store);
    for args in [&["status"][..], &["logs", "-n", "1000"][..]] {
        let text = stdout_of(dir.path(), args);
        assert!(!text.is_empty());
        assert_terminal_safe(&text);
    }
    let status = stdout_of(dir.path(), &["status"]);
    assert!(status.contains("a\\u{202e}b\\u{2066}c"), "{status}");
}

/// Kills and reaps the child however the test ends.
struct Guard(Child);

impl Drop for Guard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn follow_prints_events_recorded_after_it_started() {
    let dir = populated();
    let child = Command::new(env!("CARGO_BIN_EXE_agentctl"))
        .args(["logs", "-n", "2", "--follow"])
        .current_dir(dir.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut guard = Guard(child);
    let stdout = guard.0.stdout.take().unwrap();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { return };
            if tx.send(line).is_err() {
                return;
            }
        }
    });

    let start = Instant::now();
    let wait = |rx: &mpsc::Receiver<String>| {
        rx.recv_timeout(DEADLINE.saturating_sub(start.elapsed()))
            .expect("a line arrived in time")
    };
    let first = wait(&rx);
    let second = wait(&rx);
    assert!(seq_of(&first) < seq_of(&second));

    let mut store = open(dir.path());
    let plan = store.create_plan(&intent("appended later")).unwrap();
    let plan_text = format!("plan {plan}");
    loop {
        let line = wait(&rx);
        assert!(seq_of(&line) > seq_of(&second));
        if line.contains("plan.created") && line.contains(&plan_text) {
            break;
        }
    }
}
