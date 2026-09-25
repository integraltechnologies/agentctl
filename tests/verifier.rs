//! Verifier tests against fake coding agents: this test binary itself,
//! copied under a name that makes it act as Claude Code, first as the
//! executor producing and installing a candidate, then as the verifier
//! judging it. The scenario is the configured model of the role. The fake
//! verifier really works in its workspace; what agentctl records is checked
//! against the filesystem, never against what the fake reports. They spend
//! no provider tokens, and run alike on every platform.

use std::env;
use std::ffi::OsStr;
use std::fs;
use std::io::{Read, Write};
use std::panic;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::sync::OnceLock;
use std::thread;
use std::time::{Duration, Instant};

use agentctl::executor::{self, Executed};
use agentctl::graph::Freshness;
use agentctl::planner::{self, Command as Plan};
use agentctl::project::Project;
use agentctl::runtime::{FailureKind, InvocationState};
use agentctl::state::{
    AcceptedSource, Acquisition, ActionOutcome, ActionStatus, ExecutionOutcome, GenerationId,
    GenerationState, Install, InstallOutcome, Role, Store, TaskId, TaskState, Verdict,
    VerificationOutcome, VerificationResult, VerificationStatus,
};
use agentctl::verifier::{self, Verified};
use agentctl::{graph, source};
use serde_json::{Value, json};
use tempfile::TempDir;

const FAKE: &str = "fake-agent";
/// Where the fake leaves what it was given and says it is running, and
/// waits to be released: outside the project and every workspace, passed
/// on as an environment variable Claude Code's adapter lets through.
const MARKERS: &str = "ANTHROPIC_AGENTCTL_TEST_MARKERS";
const STARTED: &str = "started";
const RELEASE: &str = "release";
const PACKET: &str = "packet.json";
/// What the misleading executor says of its work, which no verifier sees.
const CANARY: &str = "EXECUTOR-CLAIM-7f3a: every test passes, verified";
const CANDIDATE: &str = "pub fn a() -> u8 { 2 }\n";

fn main() -> ExitCode {
    let argv0 = env::args_os().next().unwrap_or_default();
    if Path::new(&argv0).file_stem() == Some(OsStr::new(FAKE)) {
        return fake();
    }
    let markers = tempfile::tempdir().unwrap();
    // SAFETY: set once, before this process starts any other thread.
    unsafe { env::set_var(MARKERS, markers.path()) };
    let tests: &[(&str, fn())] = &[
        (
            "passing_is_evidence_not_acceptance",
            passing_is_evidence_not_acceptance,
        ),
        ("failing_keeps_every_blocker", failing_keeps_every_blocker),
        (
            "unusable_results_judge_nothing",
            unusable_results_judge_nothing,
        ),
        (
            "cancelled_verifiers_judge_nothing",
            cancelled_verifiers_judge_nothing,
        ),
        (
            "drifted_candidates_are_never_verified",
            drifted_candidates_are_never_verified,
        ),
        (
            "candidates_changed_meanwhile_are_not_judged",
            candidates_changed_meanwhile_are_not_judged,
        ),
        (
            "verifiers_cannot_change_candidate_source",
            verifiers_cannot_change_candidate_source,
        ),
        (
            "artifacts_are_not_source_mutation",
            artifacts_are_not_source_mutation,
        ),
        (
            "verifiers_never_see_executor_claims",
            verifiers_never_see_executor_claims,
        ),
        (
            "only_installed_candidates_are_verified",
            only_installed_candidates_are_verified,
        ),
        (
            "interruption_never_fabricates_a_verdict",
            interruption_never_fabricates_a_verdict,
        ),
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
    drop(markers);
    if failed.is_empty() {
        println!("test result: ok");
        ExitCode::SUCCESS
    } else {
        println!("test result: FAILED {failed:?}");
        ExitCode::FAILURE
    }
}

fn marker(name: &str) -> PathBuf {
    Path::new(&env::var_os(MARKERS).unwrap()).join(name)
}

/// Acts as Claude Code: as the executor or the verifier, as its
/// instructions say, running the scenario named by `--model=`.
fn fake() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    let scenario = args
        .iter()
        .find_map(|a| a.strip_prefix("--model="))
        .unwrap_or_default()
        .to_owned();
    let bootstrap = args
        .iter()
        .find_map(|a| a.strip_prefix("--append-system-prompt="))
        .unwrap_or_default();
    // Always a fresh invocation, nothing resumed, no permission check
    // skipped.
    assert!(args.iter().any(|a| a == "--no-session-persistence"));
    assert!(!args.iter().any(|a| a.starts_with("--resume")));
    assert!(!args.iter().any(|a| a.contains("dangerously")));
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input).unwrap();
    // Neither role can reach agentctl's or Git's state from where it works.
    let cwd = env::current_dir().unwrap();
    assert!(
        !cwd.ancestors()
            .any(|dir| dir.join(".agentctl").exists() || dir.join(".git").exists())
    );
    if bootstrap.starts_with("You are a verifier") {
        assert!(args.iter().any(|a| a == "--permission-mode=acceptEdits"));
        assert!(args.iter().any(|a| a == "--allowedTools=Bash,PowerShell"));
        fs::write(marker(PACKET), &input).unwrap();
        verify(&scenario)
    } else {
        assert!(bootstrap.starts_with("You are an executor"));
        assert!(!args.iter().any(|a| a.starts_with("--allowedTools")));
        execute(&scenario)
    }
}

fn write(path: &str, text: &str) {
    let path = Path::new(path);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

fn execute(scenario: &str) -> ExitCode {
    let (summary, claimed) = match scenario {
        "modify" => {
            write("src/a.rs", CANDIDATE);
            ("did modify".to_owned(), json!(["src/a.rs"]))
        }
        "literal" => {
            write("src/[id].rs", "pub fn id() -> u8 { 1 }\n");
            ("did literal".to_owned(), json!(["src/[id].rs"]))
        }
        "misleading" => {
            write("src/a.rs", CANDIDATE);
            (CANARY.to_owned(), json!(["src/evil.rs", "src/b.rs"]))
        }
        "report-failed" => {
            write("src/a.rs", "half\n");
            ("gave up".to_owned(), json!(["src/a.rs"]))
        }
        "unauthorized" => {
            write("README.md", "# rewritten\n");
            write("src/a.rs", CANDIDATE);
            ("did more".to_owned(), json!(["src/a.rs"]))
        }
        other => panic!("unknown executor scenario `{other}`"),
    };
    let status = match scenario {
        "report-failed" => "failed",
        _ => "succeeded",
    };
    respond(json!({"status": status, "summary": summary, "modified_paths": claimed}))
}

/// A check the fake verifier really makes: reading the candidate in its
/// workspace.
fn inspected() -> Value {
    let text = fs::read_to_string("src/a.rs").unwrap_or_default();
    json!({"check": "src/a.rs holds the candidate", "command": null,
           "outcome": if text == CANDIDATE { "passed" } else { "failed" },
           "evidence": format!("{} bytes read", text.len())})
}

fn pass() -> Value {
    json!({"verdict": "pass",
           "checked": [inspected(),
                       {"check": "unit tests", "command": "cargo test", "outcome": "passed",
                        "evidence": "3 passed"}],
           "blockers": [], "non_blocking": []})
}

fn verify(scenario: &str) -> ExitCode {
    let wait = || {
        fs::write(marker(STARTED), "").unwrap();
        let deadline = Instant::now() + Duration::from_secs(20);
        while !marker(RELEASE).exists() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
    };
    let result = match scenario {
        "pass" => pass(),
        "fail" => json!({
            "verdict": "fail",
            "checked": [inspected()],
            "blockers": [
                {"id": "b1", "summary": "id() returns the wrong value",
                 "paths": ["src/[id].rs"], "evidence": "expected 0, found 1",
                 "location": "src/[id].rs:1"},
                {"id": "b2", "summary": "no test covers id()", "paths": ["src/[id].rs"],
                 "evidence": "no test names id", "location": null},
                {"id": "b3", "summary": "parse() is unaffected but untested",
                 "paths": ["src/lib.rs"], "evidence": "no test calls parse", "location": null},
            ],
            "non_blocking": [{"summary": "i.rs looks similar", "paths": ["src/i.rs"]}],
        }),
        "naked-pass" => json!({"verdict": "pass", "checked": [], "blockers": [],
                               "non_blocking": []}),
        "pass-with-blockers" => {
            let mut result = pass();
            result["blockers"] = json!([{"id": "b1", "summary": "s", "paths": [],
                                         "evidence": "e", "location": null}]);
            result
        }
        "fail-without-blockers" => json!({"verdict": "fail", "checked": [inspected()],
                                          "blockers": [], "non_blocking": []}),
        "schema-invalid" => json!({"verdict": "accepted", "checked": [], "blockers": [],
                                   "non_blocking": []}),
        "crash" => return ExitCode::from(3),
        "mutate" => {
            write(
                "src/a.rs",
                "pub fn a() -> u8 { 3 } // fixed by the verifier\n",
            );
            pass()
        }
        "create" => {
            write("src/extra.rs", "// a verifier's own test\n");
            pass()
        }
        "artifacts" => {
            // Where the project's Git ignores output, and the temporary
            // directory: no source.
            write("target/debug/deps/out.o", "object");
            write("target/.rustc_info.json", "{}");
            let temp = env::temp_dir().join(format!("agentctl-verify-{}", std::process::id()));
            fs::write(&temp, "scratch").unwrap();
            fs::remove_file(temp).unwrap();
            pass()
        }
        "await" => {
            wait();
            pass()
        }
        "hang" => {
            fs::write(marker(STARTED), "").unwrap();
            loop {
                thread::sleep(Duration::from_secs(1));
            }
        }
        other => panic!("unknown verifier scenario `{other}`"),
    };
    respond(result)
}

/// Answers as Claude Code does, with `output` as the structured result.
fn respond(output: Value) -> ExitCode {
    let init = json!({"type": "system", "subtype": "init", "session_id": "fake-session"});
    let result = json!({"type": "result", "subtype": "success", "is_error": false,
        "session_id": "fake-session", "result": "prose", "structured_output": output,
        "usage": {"input_tokens": 3, "output_tokens": 2}});
    println!("{init}\n{result}");
    std::io::stdout().flush().unwrap();
    ExitCode::SUCCESS
}

/// The fake agent executable, shared by every test.
fn fake_agent() -> PathBuf {
    static FAKE_DIR: OnceLock<(TempDir, PathBuf)> = OnceLock::new();
    FAKE_DIR
        .get_or_init(|| {
            let dir = tempfile::tempdir().unwrap();
            let path = dir
                .path()
                .join(format!("{FAKE}{}", env::consts::EXE_SUFFIX));
            fs::copy(env::current_exe().unwrap(), &path).unwrap();
            (dir, path)
        })
        .1
        .clone()
}

/// A Git repository holding a project whose accepted source is indexed.
struct Fixture {
    _dir: TempDir,
    root: PathBuf,
}

/// Everything accepted source and CodeGraph establish.
type Accepted = Vec<(
    String,
    Option<AcceptedSource>,
    Freshness<Vec<graph::Entity>>,
)>;

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let git = Command::new("git")
            .arg("-C")
            .arg(&root)
            .args(["init", "-q"])
            .status()
            .unwrap();
        assert!(git.success());
        let role = "provider = \"claude\"\nmodel = \"none\"\nreasoning_effort = \"high\"\n";
        let config = format!(
            "[project]\nname = \"demo\"\nversion = \"0.1.0\"\n\n\
             [codegraph]\nroots = [\"src\"]\n\n\
             [agents]\nmax_concurrency = 1\n\n\
             [agents.planner]\n{role}\n[agents.executor]\n{role}\n[agents.verifier]\n{role}"
        );
        let fx = Self { _dir: dir, root };
        fx.write("agentctl.toml", &config);
        fx.write("README.md", "# demo\n");
        fx.write(".gitignore", "target/\n");
        fx.write(
            "src/lib.rs",
            "pub mod a;\npub fn parse() -> u8 { a::a() }\n",
        );
        fx.write("src/a.rs", "pub fn a() -> u8 { 1 }\n");
        fx.write("src/b.rs", "pub fn b() {}\n");
        fx.write("src/[id].rs", "pub fn id() -> u8 { 0 }\n");
        fx.write("src/i.rs", "pub fn i() {}\n");
        let (project, mut store) = fx.open("none", "none");
        source::baseline(&project, &mut store).unwrap();
        for path in ["src/lib.rs", "src/a.rs", "src/[id].rs"] {
            graph::rust::index(&project, &mut store, path).unwrap();
        }
        fx
    }

    fn write(&self, path: &str, text: &str) {
        let path = self.root.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    fn read(&self, path: &str) -> Option<Vec<u8>> {
        fs::read(self.root.join(path)).ok()
    }

    /// Loads the project afresh, as another agentctl process would, with
    /// the executor and the verifier running their scenarios.
    fn open(&self, executor: &str, verifier: &str) -> (Project, Store) {
        let mut project = Project::load(&self.root).unwrap();
        project.config.agents.executor.model = executor.parse().unwrap();
        project.config.agents.verifier.model = verifier.parse().unwrap();
        let store = project.hydrate().unwrap();
        (project, store)
    }

    /// A ready task of its own plan allowed to mutate `scope`, whose first
    /// generation owns it and has executed `scenario`.
    fn executed(&self, scenario: &str, scope: &[&str]) -> (TaskId, GenerationId, Executed) {
        let (project, mut store) = self.open(scenario, "none");
        let plan = store
            .create_plan(&agentctl::state::HumanIntent {
                objective: "Improve the demo".into(),
                constraints: vec!["Keep the public API".into()],
                completion_criteria: vec!["cargo test passes".into()],
            })
            .unwrap();
        let add = Plan::AddTask {
            task: "change".into(),
            objective: "Make a() return 2".into(),
            context: "Only what the objective needs.".into(),
            paths: scope.iter().map(|p| p.to_string()).collect(),
            depends_on: Vec::new(),
        };
        assert!(planner::apply(&project, &mut store, plan, &[add, Plan::Finalize {}]).unwrap());
        let task = store.tasks(plan).unwrap()[0].id;
        let generation = store.start_generation(task).unwrap();
        let acquired = store.acquire_ownership(generation, scope).unwrap();
        assert_eq!(acquired, Acquisition::Acquired);
        let executed = executor::start(&project, &mut store, task, generation, Some(fake_agent()))
            .unwrap()
            .finish(&project, &mut store)
            .unwrap();
        (task, generation, executed)
    }

    /// As [`Fixture::executed`], the candidate installed.
    fn installed(&self, scenario: &str, scope: &[&str]) -> (TaskId, GenerationId) {
        let (task, generation, executed) = self.executed(scenario, scope);
        assert_eq!(executed.capture.outcome, ExecutionOutcome::Candidate);
        let Install::Finished {
            outcome: InstallOutcome::Installed,
            ..
        } = executed.capture.install
        else {
            panic!("{:?}", executed.capture.install);
        };
        (task, generation)
    }

    /// Runs one verification of the candidate of `generation` to its end.
    fn verify(&self, scenario: &str, task: TaskId, generation: GenerationId) -> Verified {
        let (project, mut store) = self.open("none", scenario);
        verifier::start(&project, &mut store, task, generation, Some(fake_agent()))
            .unwrap()
            .finish(&project, &mut store)
            .unwrap()
    }

    fn accepted(&self) -> Accepted {
        let (_, store) = self.open("none", "none");
        store
            .accepted_paths()
            .unwrap()
            .into_iter()
            .map(|path| {
                let source = store.accepted_source(&path).unwrap();
                let entities = store.entities(&path).unwrap();
                (path, source, entities)
            })
            .collect()
    }

    /// Every file of the working tree besides agentctl's state and Git's,
    /// with its bytes.
    fn tree(&self) -> Vec<(String, Vec<u8>)> {
        fn walk(dir: &Path, prefix: &str, found: &mut Vec<(String, Vec<u8>)>) {
            for entry in fs::read_dir(dir).unwrap() {
                let entry = entry.unwrap();
                let name = entry.file_name().into_string().unwrap();
                let path = format!("{prefix}{name}");
                if path == ".agentctl" || path == ".git" {
                    continue;
                }
                if entry.file_type().unwrap().is_dir() {
                    walk(&entry.path(), &format!("{path}/"), found);
                } else {
                    found.push((path, fs::read(entry.path()).unwrap()));
                }
            }
        }
        let mut found = Vec::new();
        walk(&self.root, "", &mut found);
        found.sort();
        found
    }
}

fn result(verified: &Verified) -> &VerificationResult {
    match &verified.verification.status {
        VerificationStatus::Finished(result) => result,
        status => panic!("{status:?}"),
    }
}

/// Checks what every verification leaves: the generation active and
/// owning its scope, the task still running, the executor's record as it
/// was, the journal reconciled with the result, and nothing accepted.
fn held(
    fx: &Fixture,
    (task, generation): (TaskId, GenerationId),
    scope: &[&str],
    verified: &Verified,
) {
    let (_, store) = fx.open("none", "none");
    let mut owned: Vec<&str> = scope.to_vec();
    owned.sort();
    assert_eq!(store.owned_paths(generation).unwrap(), owned);
    let execution = store.execution(generation).unwrap().unwrap();
    let recorded = store.verification(verified.verification.id).unwrap();
    assert_eq!(recorded, verified.verification, "a fresh store agrees");
    assert_eq!(store.task(task).unwrap().state, TaskState::Running);
    assert_eq!(
        store.generations(task).unwrap()[0].state,
        GenerationState::Active
    );
    // A fresh verifier, never the executor, embodied by its own invocation.
    assert_ne!(recorded.agent, execution.agent);
    let invocations = store.invocations(recorded.agent).unwrap();
    assert!(invocations.len() <= 1);
    assert!(
        store
            .invocations(execution.agent)
            .unwrap()
            .iter()
            .all(|i| !invocations.contains(i))
    );
    let entry = store.journal_entry(recorded.journal).unwrap();
    assert!(matches!(entry.status, ActionStatus::Reconciled(..)));
    assert_eq!(entry.intent.action, "verifier.run");
    let continuation = store.continuation(recorded.agent).unwrap();
    assert_eq!(continuation.len(), 1, "one action per verifier");
}

fn passing_is_evidence_not_acceptance() {
    let fx = Fixture::new();
    let (task, generation) = fx.installed("modify", &["src/a.rs"]);
    let accepted = fx.accepted();
    let installed = fx.tree();
    let verified = fx.verify("pass", task, generation);
    let result = result(&verified);
    assert_eq!(result.outcome, VerificationOutcome::Passed);
    let report = result.report.as_ref().unwrap();
    assert_eq!(report.verdict, Verdict::Pass);
    assert_eq!(report.checked.len(), 2);
    assert_eq!(
        report.checked[0].evidence,
        format!("{} bytes read", CANDIDATE.len())
    );
    assert!(report.blockers.is_empty());
    assert!(result.drifted.is_empty() && result.mutated.is_empty());
    let invocation = verified.invocation.as_ref().unwrap();
    assert_eq!(invocation.end.state, InvocationState::Succeeded);
    assert_eq!(result.invocation, Some(invocation.invocation));
    assert_eq!(verified.verification.number, 1);
    held(&fx, (task, generation), &["src/a.rs"], &verified);
    // Passing accepts nothing: accepted source and CodeGraph stay as they
    // were, the candidate stays installed, provisionally.
    assert_eq!(fx.accepted(), accepted);
    assert_eq!(fx.tree(), installed);
    let (_, store) = fx.open("none", "none");
    let entry = store.journal_entry(verified.verification.journal).unwrap();
    let ActionStatus::Reconciled(_, reconciliation) = entry.status else {
        panic!()
    };
    assert_eq!(reconciliation.outcome, ActionOutcome::CompletedAsIntended);
    let role = store
        .events_after(0, 10_000)
        .unwrap()
        .into_iter()
        .find(|e| e.kind == "agent.created" && e.agent == Some(verified.verification.agent))
        .unwrap()
        .detail;
    assert_eq!(role, Role::Verifier.to_string());
    assert!(
        store
            .events_after(0, 10_000)
            .unwrap()
            .iter()
            .all(|e| !e.kind.contains("accept"))
    );
    // A judgment is final: the candidate is not verified again.
    let (project, mut store) = fx.open("none", "pass");
    let refused = verifier::start(&project, &mut store, task, generation, Some(fake_agent()));
    let message = format!("{:#}", refused.err().unwrap());
    assert!(message.contains("already verified (passed)"), "{message}");
}

fn failing_keeps_every_blocker() {
    let fx = Fixture::new();
    let scope = ["src/[id].rs"];
    let (task, generation) = fx.installed("literal", &scope);
    let accepted = fx.accepted();
    let installed = fx.tree();
    let verified = fx.verify("fail", task, generation);
    let result = result(&verified);
    assert_eq!(result.outcome, VerificationOutcome::Failed);
    let report = result.report.as_ref().unwrap();
    let blockers: Vec<(&str, &[String])> = report
        .blockers
        .iter()
        .map(|b| (b.id.as_str(), b.paths.as_slice()))
        .collect();
    assert_eq!(
        blockers,
        [
            ("b1", &["src/[id].rs".to_string()][..]),
            ("b2", &["src/[id].rs".to_string()][..]),
            ("b3", &["src/lib.rs".to_string()][..]),
        ]
    );
    assert_eq!(
        report.blockers[0].location.as_deref(),
        Some("src/[id].rs:1")
    );
    assert_eq!(report.non_blocking[0].paths, ["src/i.rs"]);
    held(&fx, (task, generation), &scope, &verified);
    // Failing restores nothing, releases nothing and accepts nothing.
    assert_eq!(fx.tree(), installed);
    assert_eq!(fx.accepted(), accepted);
    assert_eq!(
        fx.read("src/[id].rs").unwrap(),
        b"pub fn id() -> u8 { 1 }\n"
    );
    assert_eq!(fx.read("src/i.rs").unwrap(), b"pub fn i() {}\n");
    // The packet named the literal path, never a pattern's matches.
    let packet: Value = serde_json::from_slice(&fs::read(marker(PACKET)).unwrap()).unwrap();
    let changed: Vec<&Value> = packet["candidate"]["changes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| &c["path"])
        .collect();
    assert_eq!(changed, [&json!("src/[id].rs")]);
}

fn unusable_results_judge_nothing() {
    // One candidate, verified again and again by fresh verifiers, since
    // none of these reaches a judgment.
    let fx = Fixture::new();
    let (task, generation) = fx.installed("modify", &["src/a.rs"]);
    let accepted = fx.accepted();
    let mut agents = Vec::new();
    for (n, (scenario, outcome, failure)) in [
        ("naked-pass", VerificationOutcome::MalformedResult, None),
        (
            "pass-with-blockers",
            VerificationOutcome::MalformedResult,
            None,
        ),
        (
            "fail-without-blockers",
            VerificationOutcome::MalformedResult,
            None,
        ),
        (
            "schema-invalid",
            VerificationOutcome::InvocationFailed,
            Some(FailureKind::MalformedOutput),
        ),
        (
            "crash",
            VerificationOutcome::InvocationFailed,
            Some(FailureKind::ExitStatus),
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let verified = fx.verify(scenario, task, generation);
        let result = result(&verified);
        assert_eq!(result.outcome, outcome, "{scenario}");
        assert_eq!(result.report, None, "{scenario}: no claim is kept");
        let invocation = verified.invocation.as_ref().unwrap();
        assert_eq!(invocation.end.failure, failure, "{scenario}");
        assert_eq!(
            verified.malformed.is_some(),
            outcome == VerificationOutcome::MalformedResult
        );
        assert_eq!(verified.verification.number, n as i64 + 1);
        held(&fx, (task, generation), &["src/a.rs"], &verified);
        let (_, store) = fx.open("none", "none");
        let ActionStatus::Reconciled(_, reconciliation) = store
            .journal_entry(verified.verification.journal)
            .unwrap()
            .status
        else {
            panic!()
        };
        assert_eq!(reconciliation.outcome, ActionOutcome::Failed, "{scenario}");
        agents.push(verified.verification.agent);
    }
    agents.dedup();
    assert_eq!(agents.len(), 5, "a fresh verifier every time");
    assert_eq!(fx.accepted(), accepted);
    // A malformed result explains itself to whoever reads it now.
    let verified = fx.verify("naked-pass", task, generation);
    let malformed = verified.malformed.unwrap();
    assert!(malformed.contains("checked evidence"), "{malformed}");
}

fn cancelled_verifiers_judge_nothing() {
    let fx = Fixture::new();
    let (task, generation) = fx.installed("modify", &["src/a.rs"]);
    let (project, mut store) = fx.open("none", "hang");
    let running =
        verifier::start(&project, &mut store, task, generation, Some(fake_agent())).unwrap();
    wait_started();
    let control = running.control().unwrap();
    assert!(control.observe().alive());
    control.cancel();
    let verified = running.finish(&project, &mut store).unwrap();
    fs::remove_file(marker(STARTED)).unwrap();
    let result = result(&verified);
    assert_eq!(result.outcome, VerificationOutcome::InvocationFailed);
    assert_eq!(
        verified.invocation.as_ref().unwrap().end.state,
        InvocationState::Cancelled
    );
    assert_eq!(result.report, None);
    held(&fx, (task, generation), &["src/a.rs"], &verified);
}

fn drifted_candidates_are_never_verified() {
    // Someone rewrites the installed candidate before verification: those
    // bytes are not the candidate, so no verifier runs.
    let fx = Fixture::new();
    let (task, generation) = fx.installed("modify", &["src/a.rs"]);
    let rival = "pub fn a() -> u8 { 9 } // a human's\n";
    fx.write("src/a.rs", rival);
    let _ = fs::remove_file(marker(PACKET));
    let verified = fx.verify("pass", task, generation);
    let result = result(&verified);
    assert_eq!(result.outcome, VerificationOutcome::CandidateDrifted);
    assert_eq!(result.drifted, ["src/a.rs"]);
    assert_eq!((result.invocation, &result.report), (None, &None));
    assert!(verified.invocation.is_none());
    assert!(!marker(PACKET).exists(), "no verifier ran");
    // Nothing is restored: the rival's bytes stay.
    assert_eq!(fx.read("src/a.rs").unwrap(), rival.as_bytes());
    let (_, store) = fx.open("none", "none");
    assert!(
        store
            .invocations(verified.verification.agent)
            .unwrap()
            .is_empty()
    );
    held(&fx, (task, generation), &["src/a.rs"], &verified);

    // Should the working tree hold the candidate again, a fresh verifier
    // verifies it.
    fx.write("src/a.rs", CANDIDATE);
    let verified = fx.verify("pass", task, generation);
    assert_eq!(result_outcome(&verified), VerificationOutcome::Passed);
    assert_eq!(verified.verification.number, 2);
}

fn result_outcome(verified: &Verified) -> VerificationOutcome {
    result(verified).outcome
}

fn wait_started() {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !marker(STARTED).exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    assert!(marker(STARTED).exists());
}

fn candidates_changed_meanwhile_are_not_judged() {
    let fx = Fixture::new();
    let (task, generation) = fx.installed("modify", &["src/a.rs"]);
    let (project, mut store) = fx.open("none", "await");
    let running =
        verifier::start(&project, &mut store, task, generation, Some(fake_agent())).unwrap();
    wait_started();
    fx.write("src/a.rs", "pub fn a() -> u8 { 4 } // meanwhile\n");
    fs::write(marker(RELEASE), "").unwrap();
    let verified = running.finish(&project, &mut store).unwrap();
    for name in [STARTED, RELEASE] {
        fs::remove_file(marker(name)).unwrap();
    }
    let result = result(&verified);
    assert_eq!(result.outcome, VerificationOutcome::CandidateChanged);
    assert_eq!(result.drifted, ["src/a.rs"]);
    // What the verifier said is kept as its claim, about other bytes.
    assert_eq!(result.report.as_ref().unwrap().verdict, Verdict::Pass);
    held(&fx, (task, generation), &["src/a.rs"], &verified);
    let ActionStatus::Reconciled(_, reconciliation) = store
        .journal_entry(verified.verification.journal)
        .unwrap()
        .status
    else {
        panic!()
    };
    assert_eq!(
        reconciliation.outcome,
        ActionOutcome::CompletedWithDeviation
    );
}

fn verifiers_cannot_change_candidate_source() {
    for (scenario, path) in [("mutate", "src/a.rs"), ("create", "src/extra.rs")] {
        let fx = Fixture::new();
        let (task, generation) = fx.installed("modify", &["src/a.rs"]);
        let installed = fx.tree();
        let verified = fx.verify(scenario, task, generation);
        let result = result(&verified);
        assert_eq!(
            result.outcome,
            VerificationOutcome::BoundaryViolated,
            "{scenario}"
        );
        assert_eq!(result.mutated, [path], "{scenario}");
        // Its verdict is kept as a claim, and cannot pass.
        assert_eq!(result.report.as_ref().unwrap().verdict, Verdict::Pass);
        // Nothing it wrote reached the project.
        assert_eq!(fx.tree(), installed, "{scenario}");
        held(&fx, (task, generation), &["src/a.rs"], &verified);
    }
}

fn artifacts_are_not_source_mutation() {
    let fx = Fixture::new();
    let (task, generation) = fx.installed("modify", &["src/a.rs"]);
    let installed = fx.tree();
    let verified = fx.verify("artifacts", task, generation);
    let result = result(&verified);
    assert_eq!(result.outcome, VerificationOutcome::Passed);
    assert!(result.mutated.is_empty());
    assert_eq!(fx.tree(), installed, "no artifact reached the project");
    assert!(fx.read("target/debug/deps/out.o").is_none());
    held(&fx, (task, generation), &["src/a.rs"], &verified);
}

fn verifiers_never_see_executor_claims() {
    let fx = Fixture::new();
    let (task, generation, executed) = fx.executed("misleading", &["src/a.rs", "src/b.rs"]);
    // The executor did say this, and its claims are on record...
    assert_eq!(executed.summary.as_deref(), Some(CANARY));
    assert_eq!(
        executed.capture.claimed.as_deref(),
        Some(&["src/evil.rs".to_string(), "src/b.rs".to_string()][..])
    );
    // ...but the verifier gets only what agentctl observed.
    let verified = fx.verify("pass", task, generation);
    assert_eq!(result_outcome(&verified), VerificationOutcome::Passed);
    let given = fs::read_to_string(marker(PACKET)).unwrap();
    for claim in [
        CANARY,
        "EXECUTOR-CLAIM",
        "src/evil.rs",
        "claimed",
        "summary",
        "reported",
    ] {
        assert!(!given.contains(claim), "the packet holds `{claim}`");
    }
    let packet: Value = serde_json::from_str(&given).unwrap();
    let (project, store) = fx.open("none", "none");
    assert_eq!(
        packet,
        verifier::input(&project, &store, task, generation).unwrap()
    );
    assert_eq!(packet["role"], "verifier");
    assert_eq!(packet["task"]["objective"], "Make a() return 2");
    assert_eq!(
        packet["intent"]["completion_criteria"],
        json!(["cargo test passes"])
    );
    assert_eq!(
        packet["authority"]["mutable_paths"],
        json!(["src/a.rs", "src/b.rs"])
    );
    // The candidate as captured: only `src/a.rs` changed, whatever was
    // claimed, quoted exactly, and apart from accepted knowledge.
    let changes = packet["candidate"]["changes"].as_array().unwrap();
    assert_eq!(changes.len(), 1);
    let change = &changes[0];
    assert_eq!(change["path"], "src/a.rs");
    assert_eq!(change["change"], "modified");
    assert_eq!(change["accepted"]["state"], "present");
    assert_eq!(change["accepted_graph"], "current");
    let symbols: Vec<&Value> = change["accepted_entities"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| &e["symbol"])
        .collect();
    assert!(symbols.contains(&&json!("a")));
    assert_eq!(change["provisional"]["kind"], "file");
    assert_ne!(
        change["provisional"]["sha256"],
        change["accepted"]["sha256"]
    );
    assert_eq!(
        packet["candidate"]["provisional_sources"],
        json!([{"path": "src/a.rs", "text": CANDIDATE}])
    );
    let sources = packet["accepted_sources"]["paths"].as_array().unwrap();
    assert!(sources.contains(&json!("src/lib.rs")) && !sources.contains(&json!("src/a.rs")));
    // A different agent and invocation from the executor's.
    let recorded = &verified.verification;
    let execution = store.execution(generation).unwrap().unwrap();
    assert_ne!(recorded.agent, execution.agent);
    assert_ne!(
        verified.invocation.as_ref().unwrap().invocation,
        executed.invocation.invocation
    );
}

fn only_installed_candidates_are_verified() {
    for scenario in ["report-failed", "unauthorized"] {
        let fx = Fixture::new();
        let (task, generation, executed) = fx.executed(scenario, &["src/a.rs"]);
        assert_ne!(executed.capture.outcome, ExecutionOutcome::Candidate);
        let (project, mut store) = fx.open("none", "pass");
        let refused = verifier::start(&project, &mut store, task, generation, Some(fake_agent()));
        let message = format!("{:#}", refused.err().unwrap());
        assert!(message.contains("no installed candidate"), "{message}");
        assert!(store.verifications(generation).unwrap().is_empty());
    }
    // Nor one whose generation has ended.
    let fx = Fixture::new();
    let (task, generation) = fx.installed("modify", &["src/a.rs"]);
    let (project, mut store) = fx.open("none", "pass");
    store
        .finish_generation(generation, agentctl::state::GenerationEnd::Failed)
        .unwrap();
    let refused = verifier::start(&project, &mut store, task, generation, Some(fake_agent()));
    let message = format!("{:#}", refused.err().unwrap());
    assert!(message.contains("already ended"), "{message}");
    assert!(store.verifications(generation).unwrap().is_empty());
}

fn interruption_never_fabricates_a_verdict() {
    // agentctl disappears while the verifier acts.
    let fx = Fixture::new();
    let (task, generation) = fx.installed("modify", &["src/a.rs"]);
    let installed = fx.tree();
    let (project, mut store) = fx.open("none", "hang");
    let running =
        verifier::start(&project, &mut store, task, generation, Some(fake_agent())).unwrap();
    let id = running.id();
    wait_started();
    fs::remove_file(marker(STARTED)).unwrap();
    drop(running);
    drop(store);
    let (_, store) = fx.open("none", "none");
    let verification = store.verification(id).unwrap();
    let VerificationStatus::OutcomeUnknown {
        invocation: Some(invocation),
    } = verification.status
    else {
        panic!("{:?}", verification.status);
    };
    assert_eq!(
        store.invocation(invocation).unwrap().state,
        InvocationState::Running
    );
    let entry = store.journal_entry(verification.journal).unwrap();
    assert!(matches!(entry.status, ActionStatus::OutcomeUnknown(_)));
    assert_eq!(store.owned_paths(generation).unwrap(), ["src/a.rs"]);
    assert_eq!(fx.tree(), installed);
    // Nor is another verification begun while that one is unresolved.
    let (project, mut store) = fx.open("none", "pass");
    let refused = verifier::start(&project, &mut store, task, generation, Some(fake_agent()));
    let message = format!("{:#}", refused.err().unwrap());
    assert!(message.contains("not yet reconciled"), "{message}");

    // Intended, but no invocation was ever recorded or launched.
    let fx = Fixture::new();
    let (task, generation) = fx.installed("modify", &["src/a.rs"]);
    let (mut project, mut store) = fx.open("none", "pass");
    project.config.agents.verifier.reasoning_effort = agentctl::config::ReasoningEffort::Minimal;
    let refused = verifier::start(&project, &mut store, task, generation, Some(fake_agent()));
    assert!(refused.is_err());
    let verifications = store.verifications(generation).unwrap();
    assert_eq!(verifications.len(), 1);
    assert_eq!(verifications[0].status, VerificationStatus::Intended);
    assert!(
        store
            .invocations(verifications[0].agent)
            .unwrap()
            .is_empty()
    );
    let entry = store.journal_entry(verifications[0].journal).unwrap();
    assert_eq!(entry.status, ActionStatus::NotAttempted);
}
