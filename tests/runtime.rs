//! Runtime tests against a fake provider: this test binary itself, copied
//! under a name that makes it act as one, with the scenario chosen by the
//! launch's model. They spend no provider tokens.
//!
//! Live smoke tests against the installed CLIs run only when `AGENTCTL_LIVE`
//! names them, as in `AGENTCTL_LIVE=claude,codex cargo test --test runtime live`.

use std::env;
use std::ffi::OsStr;
use std::io::{Read, Write};
use std::num::NonZeroU32;
use std::panic;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use agentctl::observe::{self, Dimension, GroupKey};
use agentctl::project::Project;
use agentctl::report;

use agentctl::config::ReasoningEffort;
use agentctl::platform::{self, Capability, Level, Need};
use agentctl::procd::{self, Domain, Settlement};
use agentctl::recovery::{self, Lifecycle};
use agentctl::runtime::{
    self, FailureKind, InvocationState, Launch, Outcome, Provider, TokenUsage, Usage, Workspace,
};
use agentctl::state::{
    ActionStatus, AgentId, AgentScope, Attempt, EventQuery, HumanIntent, Intent, InvocationId,
    Role, Store, UnresolvedKind,
};
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
    if env::var_os("AGENTCTL_TEST_CRASH_STATE").is_some() {
        crash_child();
        return ExitCode::SUCCESS;
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
        (
            "lifecycle_is_owned_before_execution",
            lifecycle_is_owned_before_execution,
        ),
        (
            "required_enforcement_is_never_downgraded",
            required_enforcement_is_never_downgraded,
        ),
        (
            "cancellation_terminates_the_whole_tree",
            cancellation_terminates_the_whole_tree,
        ),
        (
            "an_ordinary_end_leaves_no_descendants",
            an_ordinary_end_leaves_no_descendants,
        ),
        (
            "a_result_with_output_held_by_a_descendant_settles_cleanly",
            a_result_with_output_held_by_a_descendant_settles_cleanly,
        ),
        (
            "a_lost_domain_is_never_taken_for_gone_without_proof",
            a_lost_domain_is_never_taken_for_gone_without_proof,
        ),
        (
            "a_successful_provider_with_an_unproven_lifecycle_stays_unresolved",
            a_successful_provider_with_an_unproven_lifecycle_stays_unresolved,
        ),
        (
            "a_failed_provider_with_an_unproven_lifecycle_stays_unresolved",
            a_failed_provider_with_an_unproven_lifecycle_stays_unresolved,
        ),
        (
            "a_cancellation_with_an_unproven_lifecycle_stays_unresolved",
            a_cancellation_with_an_unproven_lifecycle_stays_unresolved,
        ),
        (
            "an_abandoned_launch_with_an_unproven_lifecycle_stays_unresolved",
            an_abandoned_launch_with_an_unproven_lifecycle_stays_unresolved,
        ),
        (
            "an_escaped_writer_never_becomes_settled_success",
            an_escaped_writer_never_becomes_settled_success,
        ),
        (
            "abandoned_invocations_stay_unresolved",
            abandoned_invocations_stay_unresolved,
        ),
        (
            "journaled_attempts_outlive_abandoned_invocations",
            journaled_attempts_outlive_abandoned_invocations,
        ),
        (
            "independent_invocations_run_concurrently",
            independent_invocations_run_concurrently,
        ),
        (
            "observation_keeps_provenance_of_real_invocations",
            observation_keeps_provenance_of_real_invocations,
        ),
        (
            "observation_counts_concurrent_invocations_once",
            observation_counts_concurrent_invocations_once,
        ),
        (
            "an_unproven_end_is_never_observed_as_success",
            an_unproven_end_is_never_observed_as_success,
        ),
        (
            "a_crashed_session_is_observed_until_recovery_settles_it",
            a_crashed_session_is_observed_until_recovery_settles_it,
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

/// The most a fixture meant to hang stays alive by itself. Tests end it far
/// sooner, through cancellation or process-tree termination; this only
/// bounds it when its supervisor is lost outright (killed, crashed), since
/// nothing may rely on a parent's death to end its descendants.
const HANG_BOUND: Duration = Duration::from_secs(60);

/// Hangs for [`HANG_BOUND`] at most, then exits.
fn hang_bounded() -> ! {
    let deadline = Instant::now() + HANG_BOUND;
    while Instant::now() < deadline {
        thread::sleep(Duration::from_secs(1));
    }
    std::process::exit(2)
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
    if let Some(secs) = scenario.strip_prefix("sleeper:") {
        // A bounded descendant: ends on its own whatever else happens.
        thread::sleep(Duration::from_secs(secs.parse().unwrap()));
        return ExitCode::SUCCESS;
    }
    if scenario == "escaper" {
        // Leaves its containment as far as a process can on Unix, then
        // starts a bounded descendant and goes.
        #[cfg(unix)]
        // SAFETY: setsid takes no pointers.
        unsafe {
            libc::setsid();
        }
        start_descendant();
        return ExitCode::SUCCESS;
    }
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
        hang_bounded()
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
        "mark" => {
            std::fs::write("ran", "").unwrap();
            say(&result(json!({"n": 7})));
        }
        // Starts a bounded descendant of its own and waits, itself bounded.
        "tree" => {
            say(&init);
            start_descendant();
            for _ in 0..30 {
                thread::sleep(Duration::from_secs(1));
            }
        }
        // Answers and exits at once, leaving its descendant behind.
        "orphaning" => {
            start_descendant();
            say(&result(json!({"n": 7})));
        }
        // Answers and exits at once, leaving a descendant that holds the
        // provider's own output (and error) streams open.
        "holding" => {
            start_holding_descendant();
            say(&result(json!({"n": 7})));
        }
        // Answers and exits at once, leaving behind a descendant that has
        // escaped into a session of its own by way of a second process.
        "escaping" => {
            std::process::Command::new(env::current_exe().unwrap())
                .arg("--model=escaper")
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .unwrap();
            say(&result(json!({"n": 7})));
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

fn intent(objective: &str) -> HumanIntent {
    HumanIntent {
        objective: objective.into(),
        constraints: Vec::new(),
        completion_criteria: Vec::new(),
    }
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
        let plan = store.create_plan(&intent("exercise the runtime")).unwrap();
        let agent = store
            .create_agent(Role::Planner, AgentScope::Plan(plan))
            .unwrap();
        Self { dir, store, agent }
    }

    fn agent(&mut self) -> AgentId {
        let plan = self.store.create_plan(&intent("another")).unwrap();
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
            workspace: Workspace::ReadOnly,
            lifecycle: runtime::ROLE_LIFECYCLE,
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
                format!("invocation {id}: succeeded; usage provider_reported: 8 in, 2 out")
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
        // The status text is the host's (`exit status: 3` / `exit code: 3`);
        // the code itself is asserted structurally below.
        (Provider::Claude, "crash", ExitStatus, "the provider"),
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
    assert_eq!(crash.end.failure, Some(FailureKind::ExitStatus));
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

/// Starts a descendant that ends on its own within 20 seconds, and records
/// its process id beside the working directory for the test to check.
fn start_descendant() {
    spawn_descendant(std::process::Stdio::null, std::process::Stdio::null);
}

/// A descendant that keeps this provider's stdout and stderr open.
fn start_holding_descendant() {
    spawn_descendant(std::process::Stdio::inherit, std::process::Stdio::inherit);
}

fn spawn_descendant(stdout: fn() -> std::process::Stdio, stderr: fn() -> std::process::Stdio) {
    let child = std::process::Command::new(env::current_exe().unwrap())
        .arg("--model=sleeper:20")
        .stdin(std::process::Stdio::null())
        .stdout(stdout())
        .stderr(stderr())
        .spawn()
        .unwrap();
    std::fs::write("descendant.tmp", child.id().to_string()).unwrap();
    std::fs::rename("descendant.tmp", "descendant").unwrap();
    // Left running, and never waited for.
    std::mem::forget(child);
}

/// The id of the descendant a fake provider started in `dir`.
fn descendant(dir: &Path) -> u32 {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Ok(id) = std::fs::read_to_string(dir.join("descendant")) {
            return id.trim().parse().unwrap();
        }
        assert!(Instant::now() < deadline, "no descendant started");
        thread::sleep(Duration::from_millis(10));
    }
}

/// Waits for `pid` to be gone, which a domain's termination must cause well
/// within the 20 seconds the descendant lives on its own.
#[cfg(unix)]
fn assert_gone_soon(pid: u32) {
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        // SAFETY: signal 0 only checks that the process exists.
        if unsafe { libc::kill(pid as libc::pid_t, 0) } != 0 {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "descendant {pid} survived its domain's termination"
        );
        thread::sleep(Duration::from_millis(50));
    }
}

#[cfg(not(unix))]
fn assert_gone_soon(_pid: u32) {}

/// Whether procd can enforce process-tree termination on this host.
fn host_enforces() -> bool {
    platform::capabilities()
        .get(Capability::ProcessTreeTermination)
        .level
        == Level::Enforced
}

fn lifecycle_is_owned_before_execution() {
    let mut f = Fixture::new();
    let launch = f.launch(Provider::Claude, "mark");
    let ran = f.dir.path().join("ran");
    let seen = std::cell::Cell::new(false);
    let invocation = runtime::spawn_after(&mut f.store, &launch, |store, id| {
        // Before any provider process exists, its lifecycle domain's
        // identity is durable with the invocation, which is still starting.
        let identity = store.containment(id)?.expect("a recorded identity");
        assert!(!identity.is_empty() && identity.is_ascii());
        assert_eq!(store.invocation(id)?.state, InvocationState::Starting);
        assert!(
            !ran.exists(),
            "the provider ran before its domain was recorded"
        );
        seen.set(true);
        Ok(())
    })
    .unwrap();
    assert!(seen.get());
    let id = invocation.id();
    let outcome = invocation.wait(&mut f.store).unwrap();
    assert_eq!(outcome.end.state, InvocationState::Succeeded);
    // Durable, and the same for another process reading the store.
    let recorded = f.reopen().containment(id).unwrap().unwrap();
    assert_eq!(f.store.containment(id).unwrap().unwrap(), recorded);
    assert!(ran.exists());
}

fn required_enforcement_is_never_downgraded() {
    let mut f = Fixture::new();
    let launch = Launch {
        lifecycle: Need::RequireEnforced,
        ..f.launch(Provider::Claude, "mark")
    };
    let result = runtime::spawn(&mut f.store, &launch);
    if host_enforces() {
        let outcome = result.unwrap().wait(&mut f.store).unwrap();
        assert_eq!(outcome.end.state, InvocationState::Succeeded);
        return;
    }
    // Refused before anything is recorded or run: no best-effort stand-in.
    let refused = result.err().expect("the launch is refused").to_string();
    assert!(refused.contains("refusing to launch"), "{refused}");
    assert!(f.store.invocations(f.agent).unwrap().is_empty());
    assert!(!f.dir.path().join("ran").exists());
}

fn cancellation_terminates_the_whole_tree() {
    let mut f = Fixture::new();
    let invocation = f.spawn("tree").unwrap();
    let pid = descendant(f.dir.path());
    let control = invocation.control();
    control.cancel();
    let started = Instant::now();
    let outcome = invocation.wait(&mut f.store).unwrap();
    assert!(started.elapsed() < Duration::from_secs(10));
    assert_eq!(outcome.end.state, InvocationState::Cancelled);
    assert_gone_soon(pid);
}

fn an_ordinary_end_leaves_no_descendants() {
    let mut f = Fixture::new();
    let outcome = f.run(&f.launch(Provider::Claude, "orphaning"));
    assert_succeeded(&outcome, "orphaning");
    // Nothing relies on the provider's death, or agentctl's: the domain is
    // terminated whatever the provider did.
    assert_gone_soon(descendant(f.dir.path()));
}

/// Asserts `outcome` succeeded, and if it did not, prints everything the
/// outcome structurally says. Settlement is only ever reached with the
/// domain proven empty (an unproven one is an `Unresolved` error, never an
/// `Outcome`), so an `Outcome` here already carries that proof: what
/// differs is how the provider's own end classified.
fn assert_succeeded(outcome: &Outcome, scenario: &str) {
    if outcome.end.state == InvocationState::Succeeded {
        return;
    }
    panic!(
        "{scenario}: expected Succeeded (lifecycle emptiness was proven: an \
         unproven domain returns Unresolved, not an Outcome)\n\
         state: {:?}\nfailure kind: {:?}\nexit code: {:?}\n\
         diagnostic: {:?}\nresult payload present: {}\n\
         provider session: {:?}\nusage: {:?}\nstderr: {:?}\n\
         procd capabilities: {:?}\n\
         full outcome: {outcome:#?}",
        outcome.end.state,
        outcome.end.failure,
        outcome.end.exit_code,
        outcome.end.diagnostic,
        outcome.payload.is_some(),
        outcome.end.provider_session,
        outcome.end.usage,
        outcome.stderr,
        procd::capabilities(),
    );
}

/// The provider answers completely and exits 0, and a descendant it started
/// keeps its output streams open. The shim ends the channels on purpose
/// (`held`), so the reader sees a clean end of stream rather than whatever
/// the host does to a socket when the shim and domain are destroyed under
/// it; with the domain's emptiness proven, the invocation settles, its
/// output noted as held. This is the settlement the leftover descendant
/// must not be able to turn into a failure, nor a success into more than it
/// is: the note stays.
fn a_result_with_output_held_by_a_descendant_settles_cleanly() {
    let mut f = Fixture::new();
    // Where procd can prove emptiness, its own evidence settles this; the
    // test double stands in only where the backend cannot.
    let _real = host_enforces().then(|| runtime::testing::evidence(runtime::testing::Mode::Real));
    let outcome = f.run(&f.launch(Provider::Claude, "holding"));
    assert_succeeded(&outcome, "holding");
    let diagnostic = outcome.end.diagnostic.as_deref().unwrap_or_default();
    assert!(
        diagnostic.contains("stayed open after it exited"),
        "held output is reported: {diagnostic:?}"
    );
    assert_eq!(outcome.payload, Some(json!({"n": 7})));
    assert_gone_soon(descendant(f.dir.path()));
}

fn a_lost_domain_is_never_taken_for_gone_without_proof() {
    // agentctl is lost while its domain has a live process: the handle is
    // released, not terminated, as a crash would.
    let domain = Domain::create(Need::AllowBestEffort, "agentctl-test").unwrap();
    let fake = fake_provider().to_str().unwrap();
    domain.spawn(&[fake, "--model=sleeper:4"]).unwrap();
    let identity = domain.identity().to_owned();
    drop(domain);

    // After the restart, only procd's proof says it is gone.
    let settled = recovery::settle_lifecycle(runtime_invocation_id(), Some(&identity));
    if host_enforces() {
        // Recovered and terminated, or proven destroyed; or, where procd
        // cannot say, uncertain: never anything else.
        assert!(matches!(settled, Lifecycle::Gone | Lifecycle::Uncertain(_)));
    } else {
        let Lifecycle::Uncertain(why) = settled else {
            panic!("a best-effort host took its own guess for proof");
        };
        assert!(why.contains("nothing it reports"), "{why}");
    }
    assert!(matches!(
        recovery::settle_lifecycle(runtime_invocation_id(), Some("garbage")),
        Lifecycle::Uncertain(_)
    ));
    assert!(matches!(
        recovery::settle_lifecycle(runtime_invocation_id(), None),
        Lifecycle::Uncertain(_)
    ));
    // The recorded process ends within its own 4 seconds.
    assert!(matches!(procd::settle(None), Settlement::Uncertain(_)));
    // The recorded process ends within its own 4 seconds.
    thread::sleep(Duration::from_secs(5));
}

/// An invocation id to name in a settlement, which never consults it.
fn runtime_invocation_id() -> agentctl::state::InvocationId {
    let mut f = Fixture::new();
    let launch = f.launch(Provider::Claude, "ok");
    f.run(&launch).invocation
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
        "terminating a domain does not wait on the provider"
    );
    assert_eq!(outcome.end.state, InvocationState::Cancelled);
    assert_eq!(outcome.end.failure, None);
    // Recorded as cancelled only because the domain was proven empty.
    let diagnostic = outcome.end.diagnostic.as_ref().unwrap();
    assert!(
        diagnostic.contains("terminated and proven empty"),
        "{diagnostic}"
    );
    assert_eq!(outcome.payload, None);
    assert!(outcome.stderr.contains("working"));
    assert_eq!(
        other.invocation(outcome.invocation).unwrap().state,
        InvocationState::Cancelled
    );
    assert_reaped(pid);
}

/// What `wait` said of an invocation whose lifecycle domain was not proven
/// empty: it is unresolved, and nothing of how it ended is recorded.
fn assert_unresolved(f: &mut Fixture, id: agentctl::state::InvocationId, error: anyhow::Error) {
    let unresolved = error
        .downcast_ref::<runtime::Unresolved>()
        .unwrap_or_else(|| panic!("not an unresolved invocation: {error:#}"));
    assert_eq!(unresolved.invocation, id);
    // Durable for another process: still live as far as anyone can prove.
    let other = f.reopen();
    let recorded = other.invocation(id).unwrap();
    assert_eq!(recorded.state, InvocationState::Running);
    assert_eq!(recorded.end, None);
    let listed = other.unresolved(None).unwrap();
    assert!(
        listed
            .iter()
            .any(|u| u.kind == agentctl::state::UnresolvedKind::Invocation(id)),
        "{listed:?}"
    );
    // Its agent stays embodied: nothing else runs as it.
    assert!(f.spawn("ok").is_err());
}

fn a_successful_provider_with_an_unproven_lifecycle_stays_unresolved() {
    let mut f = Fixture::new();
    let _unproven = runtime::testing::evidence(runtime::testing::Mode::Unproven);
    let invocation = f.spawn("ok").unwrap();
    let id = invocation.id();
    // A valid result, exit 0, and procd terminated the domain without
    // proving it empty: not a success, and nothing else settled either.
    let error = invocation.wait(&mut f.store).unwrap_err();
    assert!(
        format!("{error:#}").contains("not proven empty"),
        "{error:#}"
    );
    assert_unresolved(&mut f, id, error);
}

fn a_failed_provider_with_an_unproven_lifecycle_stays_unresolved() {
    for scenario in ["crash", "error", "wrong-shape", "eof"] {
        let mut f = Fixture::new();
        let _unproven = runtime::testing::evidence(runtime::testing::Mode::Unproven);
        let invocation = f.spawn(scenario).unwrap();
        let id = invocation.id();
        let error = invocation.wait(&mut f.store).unwrap_err();
        assert_unresolved(&mut f, id, error);
    }
}

fn a_cancellation_with_an_unproven_lifecycle_stays_unresolved() {
    let mut f = Fixture::new();
    let _unproven = runtime::testing::evidence(runtime::testing::Mode::Unproven);
    let invocation = f.spawn("hang").unwrap();
    let id = invocation.id();
    let pid = wait_for_first_event(&invocation);
    invocation.control().cancel();
    let error = invocation.wait(&mut f.store).unwrap_err();
    assert_unresolved(&mut f, id, error);
    // The termination itself was real.
    assert_reaped(pid);
}

fn an_abandoned_launch_with_an_unproven_lifecycle_stays_unresolved() {
    let mut f = Fixture::new();
    let _unproven = runtime::testing::evidence(runtime::testing::Mode::Unproven);
    // The provider cannot be started in a directory that is not there: the
    // shim reports it, and the launch is abandoned by terminating the domain.
    let mut launch = f.launch(Provider::Claude, "ok");
    launch.cwd = f.dir.path().join("no such directory");
    let error = runtime::spawn(&mut f.store, &launch).err().unwrap();
    assert!(
        format!("{error:#}").contains("not proven empty"),
        "{error:#}"
    );
    let recorded = f.store.invocations(f.agent).unwrap();
    assert_eq!(recorded.len(), 1);
    assert_eq!(recorded[0].state, InvocationState::Starting);
    assert_eq!(recorded[0].end, None);
    let listed = f.reopen().unresolved(None).unwrap();
    assert!(
        listed
            .iter()
            .any(|u| u.kind == agentctl::state::UnresolvedKind::Invocation(recorded[0].id)),
        "{listed:?}"
    );
}

/// A descendant that escaped its domain is the case emptiness proofs exist
/// for. With procd's own evidence, an enforcing host contains it and the
/// invocation settles; a best-effort one cannot prove it gone, and the
/// invocation is never recorded as a success.
fn an_escaped_writer_never_becomes_settled_success() {
    let mut f = Fixture::new();
    let _real = runtime::testing::evidence(runtime::testing::Mode::Real);
    let invocation = f.spawn("escaping").unwrap();
    let id = invocation.id();
    let result = invocation.wait(&mut f.store);
    let pid = descendant(f.dir.path());
    // Whatever happens, the bounded fixture does not outlive the test: it
    // is ended here, not by anyone's death.
    let outcome = match result {
        Ok(outcome) => {
            assert!(host_enforces(), "a best-effort host settled an escapee");
            assert_succeeded(&outcome, "escaping");
            assert_gone_soon(pid);
            None
        }
        Err(error) => Some(error),
    };
    #[cfg(unix)]
    // SAFETY: the pid is of a descendant this test's provider started.
    unsafe {
        libc::kill(pid as libc::pid_t, libc::SIGKILL);
    }
    assert_gone_soon(pid);
    if let Some(error) = outcome {
        assert!(!host_enforces(), "{error:#}");
        assert_unresolved(&mut f, id, error);
    }
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

fn journaled_attempts_outlive_abandoned_invocations() {
    let mut f = Fixture::new();
    let intent = Intent {
        action: "source.write".into(),
        parameters: json!({"path": "src/a.rs"}).as_object().unwrap().clone(),
    };
    let entry = f.store.intend(f.agent, &intent).unwrap();
    let invocation = f.spawn("hang").unwrap();
    let pid = wait_for_first_event(&invocation);
    let id = invocation.id();
    f.store.act(entry, Some(id)).unwrap();
    drop(invocation);
    assert_reaped(pid);

    // Neither the invocation's end nor a later invocation of the agent
    // stands in for reconciling the attempt.
    let interrupted = runtime::InvocationEnd {
        state: InvocationState::Interrupted,
        failure: None,
        diagnostic: Some("agentctl lost the invocation".into()),
        exit_code: None,
        provider_session: None,
        usage: Usage::Unavailable,
    };
    f.store.finish_invocation(id, &interrupted).unwrap();
    let later = f.run(&f.launch(Provider::Claude, "ok"));
    assert_eq!(later.end.state, InvocationState::Succeeded);
    let continuation = f.reopen().continuation(f.agent).unwrap();
    assert_eq!(continuation.len(), 1);
    assert_eq!(continuation[0].intent, intent);
    assert!(matches!(
        continuation[0].status,
        ActionStatus::OutcomeUnknown(Attempt { invocation: Some(i), .. }) if i == id
    ));
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

const OBSERVED_CONFIG: &str = r#"[project]
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

fn observed_project() -> (TempDir, Project, Store) {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    std::fs::write(root.join("agentctl.toml"), OBSERVED_CONFIG).unwrap();
    std::fs::create_dir_all(root.join(".agentctl")).unwrap();
    let project = Project::load(root).unwrap();
    let store = Store::open(&project.state_path()).unwrap();
    (dir, project, store)
}

fn observed_launch(root: &Path, agent: AgentId, provider: Provider, scenario: &str) -> Launch {
    Launch {
        agent,
        provider,
        executable: Some(fake_provider().to_owned()),
        model: scenario.to_owned(),
        effort: None,
        bootstrap: "Answer with the structured result only.".to_owned(),
        input: "the task".to_owned(),
        output_schema: json!({"type": "object"}),
        cwd: root.to_path_buf(),
        workspace: Workspace::ReadOnly,
        lifecycle: runtime::ROLE_LIFECYCLE,
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

fn invocation_events(store: &Store) -> Vec<agentctl::state::Event> {
    store
        .query_events(&EventQuery {
            after: None,
            plan: None,
            task: None,
            agent: None,
            kind: Some("invocation.".to_owned()),
            limit: 10_000,
            newest: false,
        })
        .unwrap()
}

fn is_unresolved(store: &Store, id: InvocationId) -> bool {
    store
        .unresolved(None)
        .unwrap()
        .iter()
        .any(|u| matches!(&u.kind, UnresolvedKind::Invocation(i) if *i == id))
}

fn observation_keeps_provenance_of_real_invocations() {
    let (dir, project, mut store) = observed_project();
    let root = dir.path();
    let plan = store.create_plan(&intent("observe")).unwrap();
    let scenarios = [
        (Provider::Claude, "ok"),
        (Provider::Codex, "codex-ok"),
        (Provider::Claude, "no-usage"),
        (Provider::Codex, "codex-leak-prose"),
    ];
    for (provider, scenario) in scenarios {
        let agent = store
            .create_agent(Role::Planner, AgentScope::Plan(plan))
            .unwrap();
        runtime::spawn(
            &mut store,
            &observed_launch(root, agent, provider, scenario),
        )
        .unwrap()
        .wait(&mut store)
        .unwrap();
    }
    drop(store);

    let reader = Store::open_existing(&project.state_path()).unwrap();
    let records = reader.usage_records().unwrap();
    assert_eq!(records.len(), 4);
    let total = observe::total(&records);
    assert_eq!(total.reported.invocations, 2);
    assert_eq!(total.reported.input, 18);
    assert_eq!(total.reported.output, 3);
    assert_eq!(total.reported.cached_input, Some(9));
    assert_eq!(total.reported.cache_write, None);
    assert_eq!(total.reported.reasoning, None);
    assert_eq!(total.reported.total(), 21);
    assert_eq!(total.estimated.invocations, 0);
    assert_eq!(total.unavailable, 2);
    assert_eq!(total.pending, 0);

    let groups = observe::group(&records, Dimension::Provider);
    let find = |name: &str| {
        groups
            .iter()
            .find(|(k, _)| matches!(k, GroupKey::Provider(p) if p == name))
            .map(|(_, a)| a)
            .unwrap()
    };
    let claude = find("claude");
    assert_eq!(claude.reported.input, 8);
    assert_eq!(claude.reported.output, 2);
    assert_eq!(claude.unavailable, 1);
    let codex = find("codex");
    assert_eq!(codex.reported.input, 10);
    assert_eq!(codex.reported.output, 1);
    assert_eq!(codex.unavailable, 1);

    let windows = observe::token_rate(&records, now_ms(), 60);
    let last = windows.last().unwrap();
    assert_eq!(last.reported, 21);
    assert_eq!(last.estimated, 0);
    assert_eq!(last.unavailable, 2);
    assert_eq!(windows.iter().map(|w| w.reported).sum::<u64>(), 21);

    let overview = observe::overview(&reader, NonZeroU32::new(4).unwrap()).unwrap();
    let text = report::status(root, Some(&overview));
    assert!(
        text.contains("18 in / 3 out reported; 2 unavailable"),
        "{text}"
    );
    assert!(!text.contains("pending"), "{text}");

    let events = invocation_events(&reader);
    assert_eq!(events.len(), 12, "{events:?}");
    for record in &records {
        let n = events
            .iter()
            .filter(|e| {
                let own = format!("invocation {}", record.invocation);
                e.detail == own || e.detail.starts_with(&format!("{own}: "))
            })
            .count();
        assert_eq!(n, 3, "{events:?}");
    }
    let malformed = records
        .iter()
        .find(|r| r.model == "codex-leak-prose")
        .unwrap();
    let ended = events
        .iter()
        .find(|e| {
            e.kind == "invocation.ended"
                && e.detail
                    .starts_with(&format!("invocation {}: ", malformed.invocation))
        })
        .unwrap();
    assert!(ended.detail.ends_with("; usage unavailable"), "{ended:?}");
    assert!(events.iter().all(|e| !e.detail.contains(LEAK)));
}

fn observation_counts_concurrent_invocations_once() {
    let (dir, project, mut store) = observed_project();
    let root = dir.path();
    let path = project.state_path();
    let mut launches = Vec::new();
    let mut plans = Vec::new();
    for _ in 0..3 {
        let plan = store.create_plan(&intent("observe")).unwrap();
        plans.push(plan);
        for k in 0..2 {
            let agent = store
                .create_agent(Role::Planner, AgentScope::Plan(plan))
                .unwrap();
            let launch = if k == 0 {
                observed_launch(root, agent, Provider::Codex, "codex-ok")
            } else {
                observed_launch(root, agent, Provider::Claude, "ok")
            };
            launches.push(launch);
        }
    }
    drop(store);

    let done = AtomicBool::new(false);
    thread::scope(|scope| {
        let observer = scope.spawn(|| {
            let deadline = Instant::now() + Duration::from_secs(60);
            let mut last = 0;
            loop {
                let finished = done.load(Ordering::SeqCst);
                let reader = Store::open_existing(&path).unwrap();
                let records = reader.usage_records().unwrap();
                let total = observe::total(&records);
                assert!(total.reported.invocations >= last);
                assert!(total.reported.invocations <= 6);
                last = total.reported.invocations;
                let groups = observe::group(&records, Dimension::Provider);
                let count = |name: &str| {
                    groups
                        .iter()
                        .find(|(k, _)| matches!(k, GroupKey::Provider(p) if p == name))
                        .map_or(0, |(_, a)| a.reported.invocations)
                };
                assert_eq!(
                    total.reported.total(),
                    count("claude") * 10 + count("codex") * 11
                );
                if finished || Instant::now() > deadline {
                    break;
                }
                thread::sleep(Duration::from_millis(5));
            }
        });
        let writers: Vec<_> = launches
            .into_iter()
            .map(|launch| {
                let path = path.clone();
                scope.spawn(move || {
                    let mut store = Store::open(&path).unwrap();
                    runtime::spawn(&mut store, &launch)
                        .unwrap()
                        .wait(&mut store)
                        .unwrap();
                })
            })
            .collect();
        for writer in writers {
            writer.join().unwrap();
        }
        done.store(true, Ordering::SeqCst);
        observer.join().unwrap();
    });

    let reader = Store::open_existing(&path).unwrap();
    let records = reader.usage_records().unwrap();
    let total = observe::total(&records);
    assert_eq!(total.reported.invocations, 6);
    assert_eq!(total.reported.input, 54);
    assert_eq!(total.reported.output, 9);
    assert_eq!(total.pending, 0);
    assert_eq!(total.unavailable, 0);
    let mut ids: Vec<_> = records.iter().map(|r| r.invocation).collect();
    ids.sort();
    ids.dedup();
    assert_eq!(ids.len(), 6);

    let by_plan = observe::group(&records, Dimension::Plan);
    assert_eq!(by_plan.len(), 3);
    for plan in &plans {
        let (_, aggregate) = by_plan
            .iter()
            .find(|(k, _)| matches!(k, GroupKey::Plan(p) if p == plan))
            .unwrap();
        assert_eq!(aggregate.reported.invocations, 2);
        assert_eq!(aggregate.reported.input, 18);
        assert_eq!(aggregate.reported.output, 3);
    }

    let again = observe::total(&reader.usage_records().unwrap());
    let fresh = observe::total(
        &Store::open_existing(&path)
            .unwrap()
            .usage_records()
            .unwrap(),
    );
    for other in [&again, &fresh] {
        assert_eq!(other.reported.invocations, total.reported.invocations);
        assert_eq!(other.reported.input, total.reported.input);
        assert_eq!(other.reported.output, total.reported.output);
        assert_eq!(other.reported.cached_input, total.reported.cached_input);
        assert_eq!(other.unavailable, total.unavailable);
        assert_eq!(other.pending, total.pending);
    }
}

fn an_unproven_end_is_never_observed_as_success() {
    let mut f = Fixture::new();
    let _unproven = runtime::testing::evidence(runtime::testing::Mode::Unproven);
    let invocation = f.spawn("ok").unwrap();
    let id = invocation.id();
    let _error = invocation.wait(&mut f.store).unwrap_err();

    let reader = f.reopen();
    let records = reader.usage_records().unwrap();
    let record = records.iter().find(|r| r.invocation == id).unwrap();
    assert!(record.usage.is_none());
    assert_eq!(record.state, InvocationState::Running);
    let total = observe::total(&records);
    assert_eq!(total.pending, 1);
    assert_eq!(total.reported.invocations, 0);
    let overview = observe::overview(&reader, NonZeroU32::new(4).unwrap()).unwrap();
    assert!(
        overview
            .unresolved
            .iter()
            .any(|u| matches!(&u.kind, UnresolvedKind::Invocation(i) if *i == id))
    );
    let text = report::status(f.dir.path(), Some(&overview));
    assert!(
        text.contains("1 invocations with no end recorded"),
        "{text}"
    );
    assert!(text.contains("unresolved records"), "{text}");
    let prefix = format!("invocation {id}: ");
    assert!(
        !invocation_events(&reader)
            .iter()
            .any(|e| e.kind == "invocation.ended" && e.detail.starts_with(&prefix))
    );
}

fn crash_child() {
    let path = env::var("AGENTCTL_TEST_CRASH_STATE").unwrap();
    let agent: AgentId = env::var("AGENTCTL_TEST_CRASH_AGENT")
        .unwrap()
        .parse()
        .unwrap();
    let mut store = Store::open(Path::new(&path)).unwrap();
    let id = store
        .start_invocation(agent, "claude", "crashed", None)
        .unwrap();
    store.invocation_running(id).unwrap();
    println!("{id}");
    std::process::exit(0)
}

fn a_crashed_session_is_observed_until_recovery_settles_it() {
    let (dir, project, mut store) = observed_project();
    let root = dir.path();
    let plan = store.create_plan(&intent("crash")).unwrap();
    let agent = store
        .create_agent(Role::Planner, AgentScope::Plan(plan))
        .unwrap();
    drop(store);

    let output = std::process::Command::new(env::current_exe().unwrap())
        .env("AGENTCTL_TEST_CRASH_STATE", project.state_path())
        .env("AGENTCTL_TEST_CRASH_AGENT", agent.to_string())
        .output()
        .unwrap();
    assert!(output.status.success());
    let id: InvocationId = String::from_utf8(output.stdout)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let prefix = format!("invocation {id}: ");
    let no_end = |events: &[agentctl::state::Event]| {
        !events
            .iter()
            .any(|e| e.kind == "invocation.ended" && e.detail.starts_with(&prefix))
    };

    let reader = Store::open_existing(&project.state_path()).unwrap();
    let records = reader.usage_records().unwrap();
    let record = records.iter().find(|r| r.invocation == id).unwrap();
    assert!(record.usage.is_none());
    assert_eq!(record.state, InvocationState::Running);
    assert_eq!(observe::total(&records).pending, 1);
    assert!(is_unresolved(&reader, id));
    let overview = observe::overview(&reader, NonZeroU32::new(4).unwrap()).unwrap();
    let text = report::status(root, Some(&overview));
    assert!(text.contains("unresolved records"), "{text}");
    assert!(
        text.contains("1 invocations with no end recorded"),
        "{text}"
    );
    assert!(no_end(&invocation_events(&reader)));
    drop(reader);

    let mut store = Store::open(&project.state_path()).unwrap();
    recovery::recover_with(&project, &mut store, &|_, _| {
        Lifecycle::Uncertain("cannot prove".into())
    })
    .unwrap();
    let reader = Store::open_existing(&project.state_path()).unwrap();
    let records = reader.usage_records().unwrap();
    let record = records.iter().find(|r| r.invocation == id).unwrap();
    assert!(record.usage.is_none());
    assert!(is_unresolved(&reader, id));
    assert!(no_end(&invocation_events(&reader)));
    drop(reader);

    recovery::recover_with(&project, &mut store, &|_, _| Lifecycle::Gone).unwrap();
    drop(store);
    let reader = Store::open_existing(&project.state_path()).unwrap();
    let records = reader.usage_records().unwrap();
    let record = records.iter().find(|r| r.invocation == id).unwrap();
    assert_eq!(record.state, InvocationState::Interrupted);
    assert!(matches!(record.usage, Some(Usage::Unavailable)));
    let total = observe::total(&records);
    assert_eq!(total.unavailable, 1);
    assert_eq!(total.pending, 0);
    assert_eq!(total.reported.invocations, 0);
    assert!(!is_unresolved(&reader, id));
    let overview = observe::overview(&reader, NonZeroU32::new(4).unwrap()).unwrap();
    let text = report::status(root, Some(&overview));
    assert!(text.contains("1 unavailable"), "{text}");
    assert!(!text.contains("no end recorded"), "{text}");
    let events = invocation_events(&reader);
    assert!(
        events.iter().any(|e| e.kind == "invocation.ended"
            && e.detail == format!("invocation {id}: interrupted; usage unavailable")),
        "{events:?}"
    );
    assert!(events.iter().any(|e| e.kind == "invocation.started"));
    assert!(events.iter().any(|e| e.kind == "invocation.running"));

    let second = Store::open_existing(&project.state_path()).unwrap();
    let again = second.usage_records().unwrap();
    assert_eq!(again.len(), records.len());
    let again_record = again.iter().find(|r| r.invocation == id).unwrap();
    assert_eq!(again_record.state, record.state);
    assert_eq!(again_record.ended_at, record.ended_at);
    assert_eq!(invocation_events(&second).len(), events.len());
}
