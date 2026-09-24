//! Runtime tests against a fake provider: this test binary itself, copied
//! under a name that makes it act as one, with the scenario chosen by the
//! launch's model. They spend no provider tokens.
//!
//! Live smoke tests against the installed CLIs run only when `AGENTCTL_LIVE`
//! names them, as in `AGENTCTL_LIVE=claude,codex cargo test --test runtime live`.

use std::env;
use std::ffi::OsStr;
use std::io::{Read, Write};
use std::panic;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::OnceLock;
use std::thread;
use std::time::{Duration, Instant};

use agentctl::config::ReasoningEffort;
use agentctl::runtime::{
    self, FailureKind, InvocationState, Launch, Outcome, Provider, TokenUsage, Usage,
};
use agentctl::state::{AgentId, AgentScope, Role, Store};
use serde_json::{Value, json};
use tempfile::TempDir;

const FAKE: &str = "fake-provider";
const SECRET: &str = "AGENTCTL_TEST_SECRET";
/// Stands for a credential a provider might write anywhere in its output.
const LEAK: &str = "sk-LEAKED-CREDENTIAL-4f7c";

fn main() -> ExitCode {
    let argv0 = env::args_os().next().unwrap_or_default();
    if Path::new(&argv0).file_stem() == Some(OsStr::new(FAKE)) {
        return fake();
    }
    // SAFETY: no other thread exists yet.
    unsafe {
        env::set_var(SECRET, "hunter2");
        env::set_var("ANTHROPIC_TEST_PASSTHROUGH", "1");
        env::set_var("OPENAI_TEST_PASSTHROUGH", "1");
    }
    let tests: &[(&str, fn())] = &[
        ("claude_success_is_recorded", claude_success_is_recorded),
        (
            "codex_meets_the_same_contract",
            codex_meets_the_same_contract,
        ),
        (
            "failures_are_classified_and_recorded",
            failures_are_classified_and_recorded,
        ),
        (
            "results_must_satisfy_the_schema",
            results_must_satisfy_the_schema,
        ),
        ("a_failed_codex_turn_is_final", a_failed_codex_turn_is_final),
        (
            "provider_text_is_never_recorded",
            provider_text_is_never_recorded,
        ),
        (
            "unreported_usage_is_unavailable",
            unreported_usage_is_unavailable,
        ),
        (
            "unlaunchable_providers_fail_durably",
            unlaunchable_providers_fail_durably,
        ),
        (
            "invalid_launches_record_nothing",
            invalid_launches_record_nothing,
        ),
        (
            "providers_get_input_and_a_scrubbed_environment",
            providers_get_input_and_a_scrubbed_environment,
        ),
        (
            "undelivered_input_fails_closed",
            undelivered_input_fails_closed,
        ),
        (
            "cancellation_is_observed_and_reaped",
            cancellation_is_observed_and_reaped,
        ),
        #[cfg(unix)]
        (
            "cancellation_escalates_to_kill",
            cancellation_escalates_to_kill,
        ),
        (
            "abandoned_invocations_stay_unresolved",
            abandoned_invocations_stay_unresolved,
        ),
        (
            "independent_invocations_run_concurrently",
            independent_invocations_run_concurrently,
        ),
        ("live_claude", live_claude),
        ("live_codex", live_codex),
    ];
    let filters: Vec<String> = env::args()
        .skip(1)
        .filter(|a| !a.starts_with('-'))
        .collect();
    let mut failed = Vec::new();
    for (name, test) in tests {
        if !filters.is_empty() && !filters.iter().any(|f| name.contains(f.as_str())) {
            continue;
        }
        println!("test {name} ...");
        if panic::catch_unwind(test).is_err() {
            failed.push(name);
        }
    }
    if failed.is_empty() {
        println!("test result: ok");
        ExitCode::SUCCESS
    } else {
        println!("test result: FAILED {failed:?}");
        ExitCode::FAILURE
    }
}

/// Acts as a provider CLI, following the scenario named by `--model=`.
fn fake() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    let scenario = args
        .iter()
        .find_map(|a| a.strip_prefix("--model="))
        .unwrap_or_default()
        .to_owned();
    let mut input = String::new();
    if scenario != "ignore-input" {
        std::io::stdin().read_to_string(&mut input).unwrap();
    }
    let init = json!({"type": "system", "subtype": "init", "session_id": "fake-session"});
    let usage = json!({"input_tokens": 3, "output_tokens": 2, "cache_read_input_tokens": 5});
    let result = |structured: Value| {
        json!({"type": "result", "subtype": "success", "is_error": false,
               "session_id": "fake-session", "structured_output": structured, "usage": usage})
    };
    let say = |line: &Value| println!("{line}");
    let hang = || -> ! {
        eprintln!("working");
        loop {
            thread::sleep(Duration::from_secs(1));
        }
    };
    match scenario.as_str() {
        "ok" | "ignore-input" => {
            say(&init);
            say(&result(json!({"n": 7})));
        }
        "report-env" => {
            let mut names: Vec<String> = env::vars().map(|(name, _)| name).collect();
            names.sort();
            say(&result(json!({"args": args, "env": names, "input": input})));
        }
        "no-usage" => {
            let mut ending = result(json!({"n": 7}));
            ending.as_object_mut().unwrap().remove("usage");
            say(&ending);
        }
        "error" => {
            say(
                &json!({"type": "result", "subtype": "success", "is_error": true,
                        "result": "boom api", "api_error_status": 529, "usage": usage}),
            );
            return ExitCode::from(1);
        }
        "prose" => say(
            &json!({"type": "result", "subtype": "success", "is_error": false,
                               "result": "n is 7"}),
        ),
        "garbage" => {
            println!("not json");
            say(&result(json!({"n": 7})));
        }
        "eof" => say(&init),
        "crash" => {
            eprintln!("boom: disk on fire");
            return ExitCode::from(3);
        }
        "late-exit" => {
            say(&result(json!({"n": 7})));
            return ExitCode::from(1);
        }
        "hang" => {
            say(&init);
            hang();
        }
        #[cfg(unix)]
        "stubborn" => {
            // SAFETY: ignoring a signal installs no handler code.
            unsafe { libc::signal(libc::SIGTERM, libc::SIG_IGN) };
            say(&init);
            hang();
        }
        "codex-ok" => {
            say(&json!({"type": "thread.started", "thread_id": "fake-thread"}));
            say(
                &json!({"type": "item.completed", "item": {"type": "agent_message", "text": "Checking."}}),
            );
            say(
                &json!({"type": "item.completed", "item": {"type": "agent_message", "text": "{\"n\":7}"}}),
            );
            say(
                &json!({"type": "turn.completed", "usage": {"input_tokens": 10,
                        "cached_input_tokens": 4, "output_tokens": 1, "reasoning_output_tokens": 0}}),
            );
        }
        "wrong-shape" => say(&result(json!({"wrong": true}))),
        "codex-wrong-shape" => {
            say(&json!({"type": "turn.started"}));
            say(
                &json!({"type": "item.completed", "item": {"type": "agent_message", "text": "{\"wrong\":true}"}}),
            );
            say(&json!({"type": "turn.completed"}));
        }
        "codex-contradiction" => {
            say(&json!({"type": "turn.started"}));
            say(&json!({"type": "turn.failed", "error": {"message": "fatal"}}));
            say(
                &json!({"type": "item.completed", "item": {"type": "agent_message", "text": "{\"n\":7}"}}),
            );
            say(&json!({"type": "turn.completed"}));
        }
        "leak-stderr" => {
            eprintln!("auth failed for key {LEAK}");
            return ExitCode::from(3);
        }
        "leak-stdout" => {
            println!("token={LEAK}");
            say(&result(json!({"n": 7})));
        }
        "leak-invalid-result" => {
            say(&json!({"type": "result", "subtype": "success", "is_error": LEAK}));
        }
        "leak-error" => {
            eprintln!("{LEAK}");
            say(&json!({"type": "result", "subtype": LEAK, "is_error": true,
                        "result": format!("invalid key {LEAK}"), "api_error_status": LEAK}));
            return ExitCode::from(1);
        }
        "codex-leak-fail" => {
            say(&json!({"type": "turn.started"}));
            say(&json!({"type": "turn.failed", "error": {"message": format!("key {LEAK}")}}));
            return ExitCode::from(1);
        }
        "codex-leak-prose" => {
            say(&json!({"type": "turn.started"}));
            say(
                &json!({"type": "item.completed", "item": {"type": "agent_message", "text": format!("the key is {LEAK}")}}),
            );
            say(&json!({"type": "turn.completed", "usage": {"input_tokens": LEAK}}));
        }
        "codex-fail" => {
            say(&json!({"type": "thread.started", "thread_id": "fake-thread"}));
            say(&json!({"type": "turn.failed", "error": {"message": "usage limit reached"}}));
            return ExitCode::from(1);
        }
        other => panic!("unknown scenario `{other}`"),
    }
    std::io::stdout().flush().unwrap();
    ExitCode::SUCCESS
}

/// The fake provider executable, shared by every test.
fn fake_provider() -> &'static Path {
    static FAKE_DIR: OnceLock<(TempDir, PathBuf)> = OnceLock::new();
    &FAKE_DIR
        .get_or_init(|| {
            let dir = tempfile::tempdir().unwrap();
            let path = dir
                .path()
                .join(format!("{FAKE}{}", env::consts::EXE_SUFFIX));
            std::fs::copy(env::current_exe().unwrap(), &path).unwrap();
            (dir, path)
        })
        .1
}

struct Fixture {
    dir: TempDir,
    store: Store,
    agent: AgentId,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(&dir.path().join("state.db")).unwrap();
        let plan = store.create_plan("exercise the runtime").unwrap();
        let agent = store
            .create_agent(Role::Planner, AgentScope::Plan(plan))
            .unwrap();
        Self { dir, store, agent }
    }

    fn agent(&mut self) -> AgentId {
        let plan = self.store.create_plan("another").unwrap();
        self.store
            .create_agent(Role::Planner, AgentScope::Plan(plan))
            .unwrap()
    }

    /// A second connection, as another agentctl process would open.
    fn reopen(&self) -> Store {
        Store::open(&self.dir.path().join("state.db")).unwrap()
    }

    fn launch(&self, provider: Provider, scenario: &str) -> Launch {
        Launch {
            agent: self.agent,
            provider,
            executable: Some(fake_provider().to_owned()),
            model: scenario.into(),
            effort: None,
            bootstrap: "Answer with the structured result only.".into(),
            input: "the task".into(),
            output_schema: json!({"type": "object"}),
            cwd: self.dir.path().to_owned(),
        }
    }

    fn spawn(&mut self, scenario: &str) -> anyhow::Result<runtime::Invocation> {
        let launch = self.launch(Provider::Claude, scenario);
        runtime::spawn(&mut self.store, &launch)
    }

    fn run(&mut self, launch: &Launch) -> Outcome {
        let outcome = runtime::spawn(&mut self.store, launch)
            .unwrap()
            .wait(&mut self.store)
            .unwrap();
        let recorded = self.reopen().invocation(outcome.invocation).unwrap();
        assert_eq!(recorded.state, outcome.end.state);
        assert_eq!(
            recorded.end.as_ref(),
            Some(&outcome.end),
            "recorded as returned"
        );
        outcome
    }
}

fn claude_success_is_recorded() {
    let mut f = Fixture::new();
    let outcome = f.run(&f.launch(Provider::Claude, "ok"));
    assert_eq!(outcome.end.state, InvocationState::Succeeded);
    assert_eq!(outcome.payload, Some(json!({"n": 7})));
    assert_eq!((outcome.end.failure, outcome.end.diagnostic), (None, None));
    assert_eq!(outcome.end.exit_code, Some(0));
    assert_eq!(
        outcome.end.provider_session.as_deref(),
        Some("fake-session")
    );
    assert_eq!(
        outcome.end.usage,
        Usage::ProviderReported(TokenUsage {
            input: 8,
            output: 2,
            cached_input: Some(5),
            cache_write: None,
            reasoning: None,
        })
    );
    let recorded = f.store.invocation(outcome.invocation).unwrap();
    assert_eq!(
        (
            recorded.agent,
            recorded.provider.as_str(),
            recorded.model.as_str()
        ),
        (f.agent, "claude", "ok")
    );
    let kinds: Vec<_> = f
        .store
        .events_after(0, 100)
        .unwrap()
        .into_iter()
        .filter(|e| e.kind.starts_with("invocation."))
        .map(|e| (e.kind, e.detail))
        .collect();
    let id = outcome.invocation;
    assert_eq!(
        kinds,
        [
            (
                "invocation.started".into(),
                format!("invocation {id}: claude ok")
            ),
            ("invocation.running".into(), format!("invocation {id}")),
            (
                "invocation.ended".into(),
                format!("invocation {id}: succeeded")
            ),
        ]
    );
}

fn codex_meets_the_same_contract() {
    let mut f = Fixture::new();
    let outcome = f.run(&f.launch(Provider::Codex, "codex-ok"));
    assert_eq!(outcome.end.state, InvocationState::Succeeded);
    assert_eq!(outcome.payload, Some(json!({"n": 7})));
    assert_eq!(outcome.end.provider_session.as_deref(), Some("fake-thread"));
    assert_eq!(
        outcome.end.usage,
        Usage::ProviderReported(TokenUsage {
            input: 10,
            output: 1,
            cached_input: Some(4),
            cache_write: None,
            reasoning: Some(0),
        })
    );
    assert_eq!(
        f.store.invocation(outcome.invocation).unwrap().provider,
        "codex"
    );
}

fn failures_are_classified_and_recorded() {
    use FailureKind::*;
    let mut f = Fixture::new();
    for (provider, scenario, kind, evidence) in [
        (
            Provider::Claude,
            "error",
            ProviderError,
            "claude reported an error",
        ),
        (
            Provider::Claude,
            "prose",
            MalformedOutput,
            "no structured output",
        ),
        (Provider::Claude, "garbage", MalformedOutput, "not JSON"),
        (Provider::Claude, "eof", NoResult, "without a result"),
        (Provider::Claude, "crash", ExitStatus, "exit status: 3"),
        (
            Provider::Claude,
            "late-exit",
            ExitStatus,
            "after its result",
        ),
        (Provider::Codex, "codex-fail", ProviderError, "turn failed"),
    ] {
        let outcome = f.run(&f.launch(provider, scenario));
        assert_eq!(outcome.end.state, InvocationState::Failed, "{scenario}");
        assert_eq!(outcome.end.failure, Some(kind), "{scenario}");
        let diagnostic = outcome.end.diagnostic.unwrap();
        assert!(diagnostic.contains(evidence), "{scenario}: {diagnostic}");
        assert_eq!(outcome.payload, None, "{scenario}: no result on failure");
    }
    let crash = f.run(&f.launch(Provider::Claude, "crash"));
    assert_eq!(crash.end.exit_code, Some(3));
    assert!(crash.stderr.contains("disk on fire"));
    // A provider error keeps the usage the provider reported.
    let error = f.run(&f.launch(Provider::Claude, "error"));
    assert!(matches!(error.end.usage, Usage::ProviderReported(_)));
    // What the provider said is returned, not recorded.
    assert_eq!(error.provider_metadata["error_message"], json!("boom api"));
}

fn results_must_satisfy_the_schema() {
    let mut f = Fixture::new();
    let schema = json!({
        "type": "object",
        "properties": {"n": {"type": "integer"}},
        "required": ["n"]
    });
    for (provider, scenario) in [
        (Provider::Claude, "wrong-shape"),
        (Provider::Codex, "codex-wrong-shape"),
    ] {
        let launch = Launch {
            output_schema: schema.clone(),
            ..f.launch(provider, scenario)
        };
        let outcome = f.run(&launch);
        assert_eq!(outcome.end.state, InvocationState::Failed, "{scenario}");
        assert_eq!(outcome.end.failure, Some(FailureKind::MalformedOutput));
        assert_eq!(
            outcome.end.diagnostic.as_deref(),
            Some("the structured result does not satisfy the output schema")
        );
        assert_eq!(outcome.end.exit_code, Some(0));
        assert_eq!(outcome.payload, None, "{scenario}");
    }
    // A conforming result still succeeds under the same schema.
    for (provider, scenario) in [(Provider::Claude, "ok"), (Provider::Codex, "codex-ok")] {
        let launch = Launch {
            output_schema: schema.clone(),
            ..f.launch(provider, scenario)
        };
        let outcome = f.run(&launch);
        assert_eq!(outcome.end.state, InvocationState::Succeeded, "{scenario}");
        assert_eq!(outcome.payload, Some(json!({"n": 7})));
    }
}

fn a_failed_codex_turn_is_final() {
    let mut f = Fixture::new();
    let outcome = f.run(&f.launch(Provider::Codex, "codex-contradiction"));
    assert_eq!(outcome.end.state, InvocationState::Failed);
    assert_eq!(outcome.end.failure, Some(FailureKind::ProviderError));
    assert_eq!(outcome.payload, None);
}

fn provider_text_is_never_recorded() {
    let mut f = Fixture::new();
    for (provider, scenario, kind) in [
        (Provider::Claude, "leak-stderr", FailureKind::ExitStatus),
        (
            Provider::Claude,
            "leak-stdout",
            FailureKind::MalformedOutput,
        ),
        (
            Provider::Claude,
            "leak-invalid-result",
            FailureKind::MalformedOutput,
        ),
        (Provider::Claude, "leak-error", FailureKind::ProviderError),
        (
            Provider::Codex,
            "codex-leak-fail",
            FailureKind::ProviderError,
        ),
        (
            Provider::Codex,
            "codex-leak-prose",
            FailureKind::MalformedOutput,
        ),
    ] {
        let launch = Launch {
            agent: f.agent(),
            ..f.launch(provider, scenario)
        };
        let outcome = f.run(&launch);
        assert_eq!(outcome.end.failure, Some(kind), "{scenario}");
        let recorded = format!("{:?}", f.store.invocation(outcome.invocation).unwrap());
        assert!(!recorded.contains(LEAK), "{scenario}: {recorded}");
        if scenario == "leak-stderr" {
            assert!(
                outcome.stderr.contains(LEAK),
                "still returned for diagnosis"
            );
        }
    }
    let events = format!("{:?}", f.store.events_after(0, 10_000).unwrap());
    assert!(!events.contains(LEAK), "{events}");
    // Nor anywhere in the database's files, write-ahead log included.
    for entry in std::fs::read_dir(f.dir.path()).unwrap() {
        let path = entry.unwrap().path();
        if path.is_file() {
            let bytes = std::fs::read(&path).unwrap();
            assert!(
                !bytes.windows(LEAK.len()).any(|w| w == LEAK.as_bytes()),
                "{} holds provider text",
                path.display()
            );
        }
    }
}

fn unreported_usage_is_unavailable() {
    let mut f = Fixture::new();
    let outcome = f.run(&f.launch(Provider::Claude, "no-usage"));
    assert_eq!(outcome.end.state, InvocationState::Succeeded);
    assert_eq!(outcome.end.usage, Usage::Unavailable);
}

fn unlaunchable_providers_fail_durably() {
    let mut f = Fixture::new();
    let missing = Launch {
        executable: Some(f.dir.path().join("absent")),
        ..f.launch(Provider::Claude, "ok")
    };
    let nowhere = Launch {
        cwd: f.dir.path().join("no such directory"),
        ..f.launch(Provider::Codex, "ok")
    };
    for (launch, kind) in [
        (missing, FailureKind::ExecutableMissing),
        (nowhere, FailureKind::SpawnFailed),
    ] {
        let invocation = runtime::spawn(&mut f.store, &launch).unwrap();
        let observed = invocation.control().observe();
        assert_eq!(
            (observed.state, observed.pid),
            (InvocationState::Failed, None)
        );
        let outcome = invocation.wait(&mut f.store).unwrap();
        assert_eq!(outcome.end.failure, Some(kind));
        assert_eq!(outcome.end.usage, Usage::Unavailable);
        let recorded = f.store.invocation(outcome.invocation).unwrap();
        assert_eq!(recorded.end, Some(outcome.end));
    }
}

fn invalid_launches_record_nothing() {
    let mut f = Fixture::new();
    let relative = Launch {
        cwd: "relative".into(),
        ..f.launch(Provider::Claude, "ok")
    };
    let minimal = Launch {
        effort: Some(ReasoningEffort::Minimal),
        ..f.launch(Provider::Claude, "ok")
    };
    let unschematic = Launch {
        output_schema: json!(true),
        ..f.launch(Provider::Codex, "ok")
    };
    let invalid = Launch {
        output_schema: json!({"type": 5}),
        ..f.launch(Provider::Claude, "ok")
    };
    let remote = Launch {
        output_schema: json!({"$ref": "https://example.com/schema.json"}),
        ..f.launch(Provider::Codex, "codex-ok")
    };
    for launch in [relative, minimal, unschematic, invalid, remote] {
        assert!(runtime::spawn(&mut f.store, &launch).is_err());
    }
    assert!(f.store.invocations(f.agent).unwrap().is_empty());
}

fn providers_get_input_and_a_scrubbed_environment() {
    let mut f = Fixture::new();
    let launch = Launch {
        input: "multi\nline task with \"quotes\" and $HOME".into(),
        ..f.launch(Provider::Claude, "report-env")
    };
    let report = f.run(&launch).payload.unwrap();
    assert_eq!(report["input"], json!(launch.input));
    let args: Vec<&str> = report["args"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a.as_str().unwrap())
        .collect();
    assert!(args.contains(&"--append-system-prompt=Answer with the structured result only."));
    assert!(args.contains(&"--json-schema={\"type\":\"object\"}"));
    let env: Vec<&str> = report["env"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a.as_str().unwrap())
        .collect();
    assert!(env.contains(&"ANTHROPIC_TEST_PASSTHROUGH"), "{env:?}");
    assert!(
        env.iter().any(|n| n.eq_ignore_ascii_case("PATH")),
        "{env:?}"
    );
    for hidden in [SECRET, "OPENAI_TEST_PASSTHROUGH", "CARGO", "RUST_BACKTRACE"] {
        assert!(!env.contains(&hidden), "{hidden} leaked: {env:?}");
    }
}

fn undelivered_input_fails_closed() {
    let mut f = Fixture::new();
    // The provider answers without reading, so most of this never arrives.
    let launch = Launch {
        input: "x".repeat(8 << 20),
        ..f.launch(Provider::Claude, "ignore-input")
    };
    let outcome = f.run(&launch);
    assert_eq!(outcome.end.failure, Some(FailureKind::InputFailed));
    assert_eq!(outcome.payload, None);
}

/// Waits until `invocation` has reported an event, and returns its pid.
fn wait_for_first_event(invocation: &runtime::Invocation) -> u32 {
    let control = invocation.control();
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let observed = control.observe();
        if observed.events > 0 {
            return observed.pid.unwrap();
        }
        assert!(Instant::now() < deadline, "no event: {observed:?}");
        thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(unix)]
fn assert_reaped(pid: u32) {
    // SAFETY: signal 0 only checks that the process exists.
    let exists = unsafe { libc::kill(pid as libc::pid_t, 0) } == 0;
    assert!(
        !exists,
        "process {pid} still exists (or is an unreaped zombie)"
    );
}

#[cfg(not(unix))]
fn assert_reaped(_pid: u32) {}

fn cancellation_is_observed_and_reaped() {
    let mut f = Fixture::new();
    let invocation = f.spawn("hang").unwrap();
    let pid = wait_for_first_event(&invocation);
    let control = invocation.control();
    let live = control.observe();
    assert!(live.alive() && !live.cancel_requested, "{live:?}");
    assert_eq!(live.last_event.as_deref(), Some("system"));
    assert_eq!(live.provider_session.as_deref(), Some("fake-session"));
    assert_eq!(live.usage, Usage::Unavailable);
    assert!(live.quiet_for.is_some());
    // Durable before it ends: another process sees it running.
    let other = f.reopen();
    assert_eq!(
        other.invocation(invocation.id()).unwrap().state,
        InvocationState::Running
    );

    let canceller = thread::spawn(move || control.cancel());
    let started = Instant::now();
    let outcome = invocation.wait(&mut f.store).unwrap();
    canceller.join().unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "SIGTERM suffices"
    );
    assert_eq!(outcome.end.state, InvocationState::Cancelled);
    assert_eq!(outcome.end.failure, None);
    assert!(
        outcome
            .end
            .diagnostic
            .as_ref()
            .unwrap()
            .contains("not tracked")
    );
    assert_eq!(outcome.payload, None);
    assert!(outcome.stderr.contains("working"));
    assert_eq!(
        other.invocation(outcome.invocation).unwrap().state,
        InvocationState::Cancelled
    );
    assert_reaped(pid);
}

#[cfg(unix)]
fn cancellation_escalates_to_kill() {
    let mut f = Fixture::new();
    let invocation = f.spawn("stubborn").unwrap();
    let pid = wait_for_first_event(&invocation);
    let control = invocation.control();
    control.cancel();
    assert!(control.observe().cancel_requested);
    let started = Instant::now();
    let outcome = invocation.wait(&mut f.store).unwrap();
    assert!(
        started.elapsed() >= Duration::from_secs(2),
        "SIGTERM was ignored"
    );
    assert_eq!(outcome.end.state, InvocationState::Cancelled);
    assert!(outcome.end.diagnostic.unwrap().contains("SIGKILL"));
    let observed = control.observe();
    assert_eq!(observed.state, InvocationState::Cancelled);
    assert!(observed.exited && !observed.alive());
    assert_reaped(pid);
}

fn abandoned_invocations_stay_unresolved() {
    let mut f = Fixture::new();
    let invocation = f.spawn("hang").unwrap();
    let pid = wait_for_first_event(&invocation);
    let id = invocation.id();
    // agentctl loses the invocation without learning how it ended.
    drop(invocation);
    assert_reaped(pid);
    let recorded = f.store.invocation(id).unwrap();
    assert_eq!(
        (recorded.state, recorded.end),
        (InvocationState::Running, None)
    );
    // Nothing assumes it is alive or dead: the agent stays embodied until a
    // coordinator resolves it.
    assert!(f.spawn("ok").is_err());
    let resolved = runtime::InvocationEnd {
        state: InvocationState::Interrupted,
        failure: None,
        diagnostic: Some("agentctl lost the invocation".into()),
        exit_code: None,
        provider_session: None,
        usage: Usage::Unavailable,
    };
    f.store.finish_invocation(id, &resolved).unwrap();
    let outcome = f.run(&f.launch(Provider::Claude, "ok"));
    assert_eq!(outcome.end.state, InvocationState::Succeeded);
}

fn independent_invocations_run_concurrently() {
    let mut f = Fixture::new();
    let launches: Vec<Launch> = (0..4)
        .map(|_| Launch {
            agent: f.agent(),
            ..f.launch(Provider::Codex, "codex-ok")
        })
        .collect();
    let path = f.dir.path().join("state.db");
    let outcomes: Vec<Outcome> = thread::scope(|s| {
        let runs: Vec<_> = launches
            .iter()
            .map(|launch| {
                let path = &path;
                s.spawn(move || {
                    let mut store = Store::open(path).unwrap();
                    runtime::spawn(&mut store, launch)
                        .unwrap()
                        .wait(&mut store)
                        .unwrap()
                })
            })
            .collect();
        runs.into_iter().map(|r| r.join().unwrap()).collect()
    });
    let mut ids: Vec<_> = outcomes.iter().map(|o| o.invocation.to_string()).collect();
    ids.sort();
    ids.dedup();
    assert_eq!(ids.len(), 4);
    for outcome in outcomes {
        assert_eq!(outcome.end.state, InvocationState::Succeeded);
    }
}

fn live(provider: Provider, model: &str) {
    let enabled = env::var("AGENTCTL_LIVE").unwrap_or_default();
    if !enabled.split(',').any(|p| p.trim() == provider.name()) {
        println!("  skipped: AGENTCTL_LIVE does not name {provider}");
        return;
    }
    let mut f = Fixture::new();
    let launch = Launch {
        executable: None,
        model: model.into(),
        effort: Some(ReasoningEffort::Low),
        bootstrap: "You are a connectivity probe. Do not use any tools.".into(),
        input: "Return the number 7 as the field n.".into(),
        output_schema: json!({
            "type": "object",
            "properties": {"n": {"type": "integer"}},
            "required": ["n"],
            "additionalProperties": false
        }),
        ..f.launch(provider, model)
    };
    let outcome = f.run(&launch);
    println!("  {outcome:#?}");
    assert_eq!(outcome.end.state, InvocationState::Succeeded);
    assert_eq!(outcome.payload, Some(json!({"n": 7})));
    assert!(matches!(outcome.end.usage, Usage::ProviderReported(_)));
    assert!(outcome.end.provider_session.is_some());
}

fn live_claude() {
    live(Provider::Claude, "haiku");
}

fn live_codex() {
    live(Provider::Codex, "gpt-5.6-luna");
}
