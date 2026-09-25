//! Executor tests against a fake coding agent: this test binary itself,
//! copied under a name that makes it act as Claude Code, with the scenario
//! chosen by the configured executor model. The fake really mutates its
//! working directory, the executor's workspace; what agentctl records, and
//! what it installs into the project, is checked against the filesystem,
//! never against what the fake reports. They spend no provider tokens, and
//! run alike on every platform: nothing depends on an OS sandbox.

use std::env;
use std::ffi::OsStr;
use std::fs;
use std::io::{Read, Write};
use std::panic;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::sync::OnceLock;
use std::thread;
use std::time::{Duration, Instant};

use agentctl::executor::{self, Executed};
use agentctl::graph::Freshness;
use agentctl::planner::{self, Command as Plan};
use agentctl::project::Project;
use agentctl::runtime::{FailureKind, InvocationState};
use agentctl::state::{
    AcceptedSource, Acquisition, ActionOutcome, ActionStatus, AgentScope, Attribution, ChangeKind,
    Content, ExecutionOutcome, ExecutionStatus, GenerationId, GenerationState, Install,
    InstallOutcome, Intent, JournalId, Role, Store, TaskId, TaskState,
};
use agentctl::{graph, source};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tempfile::TempDir;

const FAKE: &str = "fake-executor";
const LINGER: &str = "--linger";
/// Where the fake says it is running, and waits to be released: a directory
/// outside both the project and the workspace, passed on as an environment
/// variable Claude Code's adapter lets through.
const MARKERS: &str = "ANTHROPIC_AGENTCTL_TEST_MARKERS";
const STARTED: &str = "started";
const RELEASE: &str = "release";

fn main() -> ExitCode {
    let argv0 = env::args_os().next().unwrap_or_default();
    if env::args().nth(1).as_deref() == Some(LINGER) {
        return linger(&env::args().nth(2).unwrap());
    }
    if Path::new(&argv0).file_stem() == Some(OsStr::new(FAKE)) {
        return fake();
    }
    let markers = tempfile::tempdir().unwrap();
    // SAFETY: set once, before this process starts any other thread.
    unsafe { env::set_var(MARKERS, markers.path()) };
    let tests: &[(&str, fn())] = &[
        (
            "authorized_changes_are_installed",
            authorized_changes_are_installed,
        ),
        (
            "read_only_tasks_get_a_bounded_packet",
            read_only_tasks_get_a_bounded_packet,
        ),
        (
            "failures_are_never_candidates",
            failures_are_never_candidates,
        ),
        (
            "claims_are_evidence_not_proof",
            claims_are_evidence_not_proof,
        ),
        (
            "unauthorized_mutation_violates_scope",
            unauthorized_mutation_violates_scope,
        ),
        ("paths_are_literal", paths_are_literal),
        (
            "ignore_rules_are_the_projects",
            ignore_rules_are_the_projects,
        ),
        (
            "preexisting_drift_is_not_attributed",
            preexisting_drift_is_not_attributed,
        ),
        (
            "lingering_writers_are_unattributable",
            lingering_writers_are_unattributable,
        ),
        (
            "workspaces_hold_no_canonical_state",
            workspaces_hold_no_canonical_state,
        ),
        (
            "concurrent_project_changes_are_never_overwritten",
            concurrent_project_changes_are_never_overwritten,
        ),
        (
            "other_actions_do_not_block_installing",
            other_actions_do_not_block_installing,
        ),
        (
            "failed_installs_restore_the_working_tree",
            failed_installs_restore_the_working_tree,
        ),
        (
            "executors_need_their_whole_scope_owned",
            executors_need_their_whole_scope_owned,
        ),
        (
            "interruption_never_fabricates_a_candidate",
            interruption_never_fabricates_a_candidate,
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

/// Every entry beneath `dir` other than a directory, relative to it, in
/// order.
fn files(dir: &Path) -> Vec<String> {
    fn walk(dir: &Path, prefix: &str, found: &mut Vec<String>) {
        for entry in fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            let name = entry.file_name().into_string().unwrap();
            if entry.file_type().unwrap().is_dir() {
                walk(&entry.path(), &format!("{prefix}{name}/"), found);
            } else {
                found.push(format!("{prefix}{name}"));
            }
        }
    }
    let mut found = Vec::new();
    walk(dir, "", &mut found);
    found.sort();
    found
}

/// Acts as Claude Code executing, in its working directory, the scenario
/// named by `--model=`.
fn fake() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    let scenario = args
        .iter()
        .find_map(|a| a.strip_prefix("--model="))
        .unwrap_or_default()
        .to_owned();
    // A fresh invocation in Claude's own editing mode, nothing resumed, and
    // no permission check skipped.
    assert!(args.iter().any(|a| a == "--no-session-persistence"));
    assert!(args.iter().any(|a| a == "--permission-mode=acceptEdits"));
    assert!(!args.iter().any(|a| a.starts_with("--resume")));
    assert!(!args.iter().any(|a| a.contains("dangerously")));
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input).unwrap();
    let packet: Value = serde_json::from_str(&input).unwrap();
    let write = |path: &str, text: &str| {
        let path = Path::new(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    };
    let append = |path: &str| {
        let mut text = fs::read_to_string(path).unwrap();
        text.push_str("// executor\n");
        fs::write(path, text).unwrap();
    };
    let multi = || {
        write("src/a.rs", "pub fn a() -> u8 { 3 }\n");
        write("src/new/c.rs", "pub fn c() {}\n");
        fs::remove_file("src/b.rs").unwrap();
    };
    let wait = || {
        fs::write(marker(STARTED), "").unwrap();
        let deadline = Instant::now() + Duration::from_secs(20);
        while !marker(RELEASE).exists() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
    };
    let (status, claimed) = match scenario.as_str() {
        "modify" => {
            write("src/a.rs", "pub fn a() -> u8 { 2 }\n");
            ("succeeded", json!(["src/a.rs"]))
        }
        "create" => {
            write("src/new/c.rs", "pub fn c() {}\n");
            ("succeeded", json!(["src/new/c.rs"]))
        }
        "delete" => {
            fs::remove_file("src/b.rs").unwrap();
            ("succeeded", json!(["src/b.rs"]))
        }
        "multi" => {
            multi();
            ("succeeded", json!(["src/a.rs", "src/b.rs", "src/new/c.rs"]))
        }
        "nothing" => ("succeeded", json!([])),
        "echo" => {
            let echo = json!({
                "task": packet["task"],
                "intent": packet["intent"]["objective"],
                "authority": packet["authority"]["mutable_paths"],
                "mutable": packet["repository"]["mutable"],
                "sources": packet["repository"]["sources"],
            });
            return respond(json!({"status": "succeeded", "summary": echo.to_string(),
                                  "modified_paths": []}));
        }
        "report-failed" => {
            write("src/a.rs", "half done\n");
            ("failed", json!(["src/a.rs"]))
        }
        "crash" => return ExitCode::from(3),
        "crash-after-write" => {
            write("src/a.rs", "partial\n");
            return ExitCode::from(3);
        }
        "schema-invalid" => {
            return respond(json!({"status": "done", "summary": "", "modified_paths": []}));
        }
        "bounds-invalid" => ("succeeded", json!(["../outside.rs"])),
        "wrong-claims" => {
            write("src/a.rs", "pub fn a() -> u8 { 4 }\n");
            ("succeeded", json!(["src/b.rs"]))
        }
        "unauthorized" => {
            append("README.md");
            ("succeeded", json!([]))
        }
        "mixed" => {
            write("src/a.rs", "pub fn a() -> u8 { 5 }\n");
            append("README.md");
            ("succeeded", json!(["src/a.rs"]))
        }
        "git-init" => {
            // No Git repository is reachable from the workspace...
            let found = Command::new("git")
                .args(["rev-parse", "--git-dir"])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .unwrap();
            assert!(!found.success());
            // ...and creating one there is Git state all the same.
            let status = Command::new("git").args(["init", "-q"]).status().unwrap();
            assert!(status.success());
            write("src/a.rs", "pub fn a() -> u8 { 9 }\n");
            ("succeeded", json!(["src/a.rs"]))
        }
        "literal" => {
            append("src/[id].rs");
            ("succeeded", json!(["src/[id].rs"]))
        }
        "glob-decoy" => {
            append("src/i.rs");
            ("succeeded", json!(["src/[id].rs"]))
        }
        "build" => {
            // Build output the project's Git ignores is no mutation.
            write("target/debug/out.o", "object");
            write("src/a.rs", "pub fn a() -> u8 { 10 }\n");
            ("succeeded", json!(["src/a.rs"]))
        }
        "hide" => {
            // Rules of its own cannot hide a mutation.
            write("src/.gitignore", "*\n");
            write("src/evil.rs", "hidden?\n");
            write("src/a.rs", "pub fn a() -> u8 { 11 }\n");
            ("succeeded", json!(["src/a.rs"]))
        }
        "touch-a" => {
            append("src/a.rs");
            ("succeeded", json!(["src/a.rs"]))
        }
        "rival-write" => {
            append("src/b.rs");
            ("succeeded", json!([]))
        }
        "linger" => {
            // Leaves a process behind that keeps writing after the executor
            // has ended, holding none of its output streams: never waited for.
            #[allow(clippy::zombie_processes)]
            Command::new(env::current_exe().unwrap())
                .args([LINGER, "src/a.rs"])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap();
            ("succeeded", json!(["src/a.rs"]))
        }
        "probe" => {
            // What the executor can see of the project and its state from
            // where it works, and what writing agentctl's state does there.
            let cwd = env::current_dir().unwrap();
            let seen = files(&cwd);
            let state_reachable = cwd
                .ancestors()
                .any(|dir| dir.join(".agentctl").exists() || dir.join(".git").exists());
            let direct = fs::write(".agentctl/state.db", "forged").is_ok();
            write(".agentctl/fake", "forged");
            write(".agentctl/state.db", "forged");
            write("src/a.rs", "pub fn a() -> u8 { 6 }\n");
            let probe = json!({"cwd": cwd, "seen": seen, "state_reachable": state_reachable,
                               "direct": direct});
            return respond(json!({"status": "succeeded", "summary": probe.to_string(),
                                  "modified_paths": ["src/a.rs"]}));
        }
        "await" => {
            wait();
            ("succeeded", json!([]))
        }
        "await-write" => {
            write("src/a.rs", "pub fn a() -> u8 { 8 }\n");
            wait();
            ("succeeded", json!(["src/a.rs"]))
        }
        "await-multi" => {
            multi();
            wait();
            ("succeeded", json!(["src/a.rs", "src/b.rs", "src/new/c.rs"]))
        }
        "unreadable" => {
            write("src/locked.rs", "secret");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions("src/locked.rs", fs::Permissions::from_mode(0o000)).unwrap();
            }
            ("succeeded", json!([]))
        }
        "hang" => {
            write("src/a.rs", "in progress\n");
            fs::write(marker(STARTED), "").unwrap();
            loop {
                thread::sleep(Duration::from_secs(1));
            }
        }
        other => panic!("unknown scenario `{other}`"),
    };
    respond(
        json!({"status": status, "summary": format!("did {scenario}"),
                   "modified_paths": claimed}),
    )
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

/// Keeps rewriting `path` for a while, until it cannot.
fn linger(path: &str) -> ExitCode {
    for n in 0..60 {
        if fs::write(path, format!("lingering {n}\n")).is_err() {
            break;
        }
        thread::sleep(Duration::from_millis(25));
    }
    ExitCode::SUCCESS
}

/// The fake executor executable, shared by every test.
fn fake_executor() -> PathBuf {
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

fn sha256(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// A Git repository holding a project whose accepted source is indexed.
struct Fixture {
    _dir: TempDir,
    root: PathBuf,
}

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
        let role = "provider = \"claude\"\nmodel = \"nothing\"\nreasoning_effort = \"high\"\n";
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
        fx.write("src/[id].rs", "pub fn id() {}\n");
        fx.write("src/i.rs", "pub fn i() {}\n");
        let (project, mut store) = fx.open("nothing");
        source::baseline(&project, &mut store).unwrap();
        for path in ["src/lib.rs", "src/a.rs"] {
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

    /// The content agentctl should have observed at `path`.
    fn content(&self, path: &str) -> Content {
        match self.read(path) {
            Some(bytes) => Content::File(sha256(&bytes)),
            None => Content::Absent,
        }
    }

    /// Loads the project afresh, as another agentctl process would, with
    /// the executor running `scenario`.
    fn open(&self, scenario: &str) -> (Project, Store) {
        let mut project = Project::load(&self.root).unwrap();
        project.config.agents.executor.model = scenario.parse().unwrap();
        let store = project.hydrate().unwrap();
        (project, store)
    }

    /// A ready task of its own plan, allowed to mutate `scope`, and its
    /// first generation, which owns all of `scope`.
    fn task(&self, key: &str, scope: &[&str]) -> (TaskId, GenerationId) {
        let (project, mut store) = self.open("nothing");
        let (task, generation) = self.planned(&project, &mut store, key, scope);
        let acquired = store.acquire_ownership(generation, scope).unwrap();
        assert_eq!(acquired, Acquisition::Acquired);
        (task, generation)
    }

    /// A ready task as above, whose generation owns nothing yet.
    fn planned(
        &self,
        project: &Project,
        store: &mut Store,
        key: &str,
        scope: &[&str],
    ) -> (TaskId, GenerationId) {
        let plan = store
            .create_plan(&agentctl::state::HumanIntent {
                objective: "Improve the demo".into(),
                constraints: vec!["Keep the public API".into()],
                completion_criteria: vec!["cargo test passes".into()],
            })
            .unwrap();
        let add = Plan::AddTask {
            task: key.into(),
            objective: format!("Complete {key}"),
            context: "Only what the objective needs.".into(),
            paths: scope.iter().map(|p| p.to_string()).collect(),
            depends_on: Vec::new(),
        };
        assert!(planner::apply(project, store, plan, &[add, Plan::Finalize {}]).unwrap());
        let task = store.tasks(plan).unwrap()[0].id;
        (task, store.start_generation(task).unwrap())
    }

    /// Runs one executor attempt of `generation` to its end.
    fn execute(&self, scenario: &str, task: TaskId, generation: GenerationId) -> Executed {
        let (project, mut store) = self.open(scenario);
        executor::start(
            &project,
            &mut store,
            task,
            generation,
            Some(fake_executor()),
        )
        .unwrap()
        .finish(&project, &mut store)
        .unwrap()
    }

    /// Plans, owns and executes a task in one go.
    fn run(&self, scenario: &str, scope: &[&str]) -> (GenerationId, Executed) {
        let (task, generation) = self.task(scenario, scope);
        (generation, self.execute(scenario, task, generation))
    }

    /// Everything accepted source and CodeGraph establish.
    fn accepted(
        &self,
    ) -> Vec<(
        String,
        Option<AcceptedSource>,
        Freshness<Vec<graph::Entity>>,
    )> {
        let (_, store) = self.open("nothing");
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
}

/// The changes a capture holds, as `(path, kind, authorized)`.
fn changes(executed: &Executed) -> Vec<(&str, ChangeKind, bool)> {
    executed
        .capture
        .changes
        .iter()
        .map(|c| (c.path.as_str(), c.kind(), c.authorized))
        .collect()
}

/// Checks what every attempt leaves: the generation active and owning its
/// scope, the task still running, the journal reconciled with the capture,
/// and nothing accepted.
fn held(fx: &Fixture, generation: GenerationId, scope: &[&str], executed: &Executed) {
    let (_, store) = fx.open("nothing");
    let mut owned: Vec<&str> = scope.to_vec();
    owned.sort();
    assert_eq!(store.owned_paths(generation).unwrap(), owned);
    let execution = store.execution(generation).unwrap().unwrap();
    assert_eq!(
        execution.status,
        ExecutionStatus::Captured(executed.capture.clone())
    );
    let ActionStatus::Reconciled(attempt, _) =
        store.journal_entry(execution.journal).unwrap().status
    else {
        panic!("the journal is reconciled with the capture");
    };
    assert_eq!(attempt.invocation, Some(executed.invocation.invocation));
    let task = store
        .events_after(0, 10_000)
        .unwrap()
        .into_iter()
        .rev()
        .find(|e| e.kind == "execution.captured")
        .and_then(|e| e.task)
        .unwrap();
    assert_eq!(store.task(task).unwrap().state, TaskState::Running);
    let generations = store.generations(task).unwrap();
    assert_eq!(generations[0].state, GenerationState::Active);
}

/// How installing ended, when it did.
fn installed(executed: &Executed) -> Option<(InstallOutcome, Vec<String>)> {
    match &executed.capture.install {
        Install::Finished {
            outcome, drifted, ..
        } => Some((*outcome, drifted.clone())),
        _ => None,
    }
}

/// The files the project's working tree holds, besides agentctl's state
/// and Git's, with their bytes.
fn tree(fx: &Fixture) -> Vec<(String, Vec<u8>)> {
    files(&fx.root)
        .into_iter()
        .filter(|path| !path.starts_with(".agentctl/") && !path.starts_with(".git/"))
        .map(|path| {
            let bytes = fx.read(&path).unwrap();
            (path, bytes)
        })
        .collect()
}

fn authorized_changes_are_installed() {
    for (scenario, scope, expected) in [
        (
            "modify",
            &["src/a.rs"][..],
            &[("src/a.rs", ChangeKind::Modified, true)][..],
        ),
        (
            "create",
            &["src/new/c.rs"],
            &[("src/new/c.rs", ChangeKind::Created, true)],
        ),
        (
            "delete",
            &["src/b.rs"],
            &[("src/b.rs", ChangeKind::Deleted, true)],
        ),
        (
            "multi",
            &["src/a.rs", "src/b.rs", "src/new/c.rs"],
            &[
                ("src/a.rs", ChangeKind::Modified, true),
                ("src/b.rs", ChangeKind::Deleted, true),
                ("src/new/c.rs", ChangeKind::Created, true),
            ],
        ),
    ] {
        let fx = Fixture::new();
        let before = fx.accepted();
        let untouched = tree(&fx);
        let (generation, executed) = fx.run(scenario, scope);
        let capture = &executed.capture;
        assert_eq!(capture.outcome, ExecutionOutcome::Candidate, "{scenario}");
        assert_eq!(changes(&executed), expected, "{scenario}");
        assert_eq!(
            installed(&executed),
            Some((InstallOutcome::Installed, Vec::new())),
            "{scenario}: {:?}",
            executed.install_diagnostic
        );
        // What was recorded is what the working tree now holds, each
        // change as the kind it was: created, modified or deleted.
        for change in &capture.changes {
            assert_eq!(change.after, fx.content(&change.path), "{scenario}");
        }
        assert_eq!(
            capture.changes[0].before != Content::Absent,
            expected[0].1 != ChangeKind::Created
        );
        // Nothing else in the working tree changed.
        let now = tree(&fx);
        let unchanged = |list: &[(String, Vec<u8>)]| -> Vec<(String, Vec<u8>)> {
            list.iter()
                .filter(|(path, _)| !scope.contains(&path.as_str()))
                .cloned()
                .collect()
        };
        assert_eq!(unchanged(&now), unchanged(&untouched), "{scenario}");
        assert_eq!(
            executed.summary.as_deref(),
            Some(&*format!("did {scenario}"))
        );
        assert!(capture.unclaimed().is_empty() && capture.claimed_unchanged().is_empty());
        assert_eq!(
            executed.invocation.end.state,
            InvocationState::Succeeded,
            "{scenario}"
        );
        held(&fx, generation, scope, &executed);
        // Installed is not accepted: accepted source and CodeGraph stay.
        assert_eq!(fx.accepted(), before, "{scenario}: nothing accepted");
        let (_, store) = fx.open("nothing");
        let install = store
            .events_after(0, 10_000)
            .unwrap()
            .into_iter()
            .find(|e| e.kind == "execution.installed")
            .unwrap();
        assert!(install.detail.ends_with("installed"), "{}", install.detail);
    }
}

fn read_only_tasks_get_a_bounded_packet() {
    let fx = Fixture::new();
    let before = fx.accepted();
    let (generation, executed) = fx.run("nothing", &[]);
    assert_eq!(executed.capture.outcome, ExecutionOutcome::Candidate);
    assert!(executed.capture.changes.is_empty());
    // Installing nothing is still journaled.
    assert_eq!(
        installed(&executed),
        Some((InstallOutcome::Installed, Vec::new()))
    );
    held(&fx, generation, &[], &executed);
    assert_eq!(fx.accepted(), before);

    // The packet: the task, the intent, the literal authority and context
    // targeted at it from accepted source and CodeGraph.
    let (_, executed) = fx.run("echo", &["src/a.rs"]);
    let echo: Value = serde_json::from_str(executed.summary.as_deref().unwrap()).unwrap();
    assert_eq!(echo["task"]["objective"], "Complete echo");
    assert_eq!(echo["task"]["context"], "Only what the objective needs.");
    assert_eq!(echo["intent"], "Improve the demo");
    assert_eq!(echo["authority"], json!(["src/a.rs"]));
    let mutable = &echo["mutable"][0];
    assert_eq!(
        (&mutable["path"], &mutable["accepted"], &mutable["graph"]),
        (&json!("src/a.rs"), &json!("present"), &json!("current"))
    );
    assert!(
        mutable["entities"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["symbol"] == "a")
    );
    assert_eq!(
        echo["sources"],
        json!(["src/[id].rs", "src/b.rs", "src/i.rs", "src/lib.rs"])
    );
}

fn failures_are_never_candidates() {
    let fx = Fixture::new();
    let before = fx.accepted();
    let scope = ["src/a.rs"];
    for (scenario, outcome, failure, changed) in [
        (
            "report-failed",
            ExecutionOutcome::ReportedFailed,
            None,
            true,
        ),
        (
            "crash",
            ExecutionOutcome::InvocationFailed,
            Some(FailureKind::ExitStatus),
            false,
        ),
        (
            "crash-after-write",
            ExecutionOutcome::InvocationFailed,
            Some(FailureKind::ExitStatus),
            true,
        ),
        (
            "schema-invalid",
            ExecutionOutcome::InvocationFailed,
            Some(FailureKind::MalformedOutput),
            false,
        ),
        (
            "bounds-invalid",
            ExecutionOutcome::MalformedResult,
            None,
            false,
        ),
    ] {
        let fixture = Fixture::new();
        let untouched = tree(&fixture);
        let (generation, executed) = fixture.run(scenario, &scope);
        let capture = &executed.capture;
        assert_eq!(capture.outcome, outcome, "{scenario}");
        assert_eq!(executed.invocation.end.failure, failure, "{scenario}");
        // Whatever the executor managed to do stays observed, and in its
        // workspace: nothing reaches the working tree.
        let expected = [("src/a.rs", ChangeKind::Modified, true)];
        assert_eq!(
            changes(&executed),
            &expected[..usize::from(changed)],
            "{scenario}"
        );
        assert_eq!(capture.install, Install::NotAttempted);
        assert_eq!(tree(&fixture), untouched, "{scenario}");
        assert_eq!(capture.reported.is_some(), scenario == "report-failed");
        assert_eq!(executed.summary.is_some(), scenario == "report-failed");
        held(&fixture, generation, &scope, &executed);
    }
    assert_eq!(fx.accepted(), before);
}

fn claims_are_evidence_not_proof() {
    let fx = Fixture::new();
    let (_, executed) = fx.run("wrong-claims", &["src/a.rs", "src/b.rs"]);
    let capture = &executed.capture;
    assert_eq!(capture.outcome, ExecutionOutcome::Candidate);
    assert_eq!(
        changes(&executed),
        [("src/a.rs", ChangeKind::Modified, true)]
    );
    assert_eq!(
        capture.claimed.as_deref(),
        Some(&["src/b.rs".to_string()][..])
    );
    assert_eq!(capture.unclaimed(), ["src/a.rs"]);
    assert_eq!(capture.claimed_unchanged(), ["src/b.rs"]);
    // What is installed is what was observed, not what was claimed.
    assert_eq!(fx.read("src/a.rs").unwrap(), b"pub fn a() -> u8 { 4 }\n");
    assert_eq!(fx.read("src/b.rs").unwrap(), b"pub fn b() {}\n");

    // Claiming to have modified nothing does not hide a modification.
    let fx = Fixture::new();
    let (_, executed) = fx.run("unauthorized", &["src/a.rs"]);
    assert_eq!(executed.capture.outcome, ExecutionOutcome::ScopeViolated);
    assert_eq!(executed.capture.unclaimed(), ["README.md"]);
}

fn unauthorized_mutation_violates_scope() {
    let fx = Fixture::new();
    let before = fx.accepted();
    let untouched = tree(&fx);
    let scope = ["src/a.rs"];
    let (generation, executed) = fx.run("mixed", &scope);
    let capture = &executed.capture;
    assert_eq!(capture.outcome, ExecutionOutcome::ScopeViolated);
    assert_eq!(capture.attribution, None);
    assert_eq!(
        changes(&executed),
        [
            ("README.md", ChangeKind::Modified, false),
            ("src/a.rs", ChangeKind::Modified, true),
        ]
    );
    assert_eq!(capture.unauthorized(), ["README.md"]);
    // Nothing is copied back, not even the authorized change.
    assert_eq!(capture.install, Install::NotAttempted);
    assert_eq!(tree(&fx), untouched);
    held(&fx, generation, &scope, &executed);
    assert_eq!(fx.accepted(), before);
    let (_, store) = fx.open("nothing");
    let entry = store.execution(generation).unwrap().unwrap().journal;
    let ActionStatus::Reconciled(_, reconciliation) = store.journal_entry(entry).unwrap().status
    else {
        panic!()
    };
    assert_eq!(
        reconciliation.outcome,
        ActionOutcome::CompletedWithDeviation
    );

    // A path another generation owns is the executor's to answer for when
    // it changes in its own workspace.
    let fx = Fixture::new();
    fx.task("rival", &["src/b.rs"]);
    let (_, executed) = fx.run("rival-write", &["src/a.rs"]);
    assert_eq!(executed.capture.outcome, ExecutionOutcome::ScopeViolated);
    assert_eq!(executed.capture.attribution, None);
    assert_eq!(executed.capture.unauthorized(), ["src/b.rs"]);
    assert_eq!(fx.read("src/b.rs").unwrap(), b"pub fn b() {}\n");

    // No Git repository is within reach; creating one in the workspace is
    // Git state all the same, and the project's stays as it was.
    let fx = Fixture::new();
    let head = || {
        let output = Command::new("git")
            .arg("-C")
            .arg(&fx.root)
            .args(["status", "--porcelain=v2", "--branch"])
            .output()
            .unwrap();
        String::from_utf8(output.stdout).unwrap()
    };
    let git_before = head();
    let untouched = tree(&fx);
    let (_, executed) = fx.run("git-init", &scope);
    assert_eq!(executed.capture.outcome, ExecutionOutcome::ScopeViolated);
    let unauthorized = executed.capture.unauthorized();
    assert!(unauthorized.contains(&".git/HEAD"), "{unauthorized:?}");
    assert!(unauthorized.iter().all(|path| path.starts_with(".git/")));
    assert!(!executed.capture.head_moved);
    assert_eq!(head(), git_before);
    assert_eq!(tree(&fx), untouched);
}

fn paths_are_literal() {
    let fx = Fixture::new();
    let (_, executed) = fx.run("literal", &["src/[id].rs"]);
    assert_eq!(executed.capture.outcome, ExecutionOutcome::Candidate);
    assert_eq!(
        changes(&executed),
        [("src/[id].rs", ChangeKind::Modified, true)]
    );
    assert_eq!(
        installed(&executed),
        Some((InstallOutcome::Installed, Vec::new()))
    );
    assert!(fx.read("src/[id].rs").unwrap().ends_with(b"// executor\n"));
    assert_eq!(fx.read("src/i.rs").unwrap(), b"pub fn i() {}\n");

    // `src/[id].rs` would match `src/i.rs` as a pattern; it authorizes only
    // itself.
    let fx = Fixture::new();
    let (_, executed) = fx.run("glob-decoy", &["src/[id].rs"]);
    assert_eq!(executed.capture.outcome, ExecutionOutcome::ScopeViolated);
    assert_eq!(executed.capture.unauthorized(), ["src/i.rs"]);
    assert_eq!(executed.capture.claimed_unchanged(), ["src/[id].rs"]);
    assert_eq!(fx.read("src/i.rs").unwrap(), b"pub fn i() {}\n");
}

fn ignore_rules_are_the_projects() {
    // Build output the project ignores is neither a mutation nor copied
    // back.
    let fx = Fixture::new();
    let (_, executed) = fx.run("build", &["src/a.rs"]);
    assert_eq!(executed.capture.outcome, ExecutionOutcome::Candidate);
    assert_eq!(
        changes(&executed),
        [("src/a.rs", ChangeKind::Modified, true)]
    );
    assert!(fx.read("target/debug/out.o").is_none());

    // Ignore rules the executor writes itself hide nothing.
    let fx = Fixture::new();
    let (_, executed) = fx.run("hide", &["src/a.rs"]);
    assert_eq!(executed.capture.outcome, ExecutionOutcome::ScopeViolated);
    assert_eq!(
        executed.capture.unauthorized(),
        ["src/.gitignore", "src/evil.rs"]
    );
}

fn preexisting_drift_is_not_attributed() {
    // Unowned drift from before the attempt is neither blamed on the
    // executor nor part of its candidate, and survives installing.
    let fx = Fixture::new();
    fx.write("README.md", "# edited by a human\n");
    fx.write("src/untracked.rs", "// new and untracked\n");
    fs::remove_file(fx.root.join("src/b.rs")).unwrap();
    let (_, executed) = fx.run("modify", &["src/a.rs"]);
    assert_eq!(executed.capture.outcome, ExecutionOutcome::Candidate);
    assert_eq!(
        changes(&executed),
        [("src/a.rs", ChangeKind::Modified, true)]
    );
    assert_eq!(
        installed(&executed),
        Some((InstallOutcome::Installed, Vec::new()))
    );
    assert_eq!(fx.read("README.md").unwrap(), b"# edited by a human\n");
    assert!(fx.read("src/untracked.rs").is_some() && fx.read("src/b.rs").is_none());

    // An owned file already dirty is staged, attributed and installed from
    // its dirty state, not from its accepted content.
    let fx = Fixture::new();
    fx.write("src/a.rs", "dirty before\n");
    let (_, executed) = fx.run("touch-a", &["src/a.rs"]);
    assert_eq!(executed.capture.outcome, ExecutionOutcome::Candidate);
    let change = &executed.capture.changes[0];
    assert_eq!(change.before, Content::File(sha256(b"dirty before\n")));
    assert_eq!(change.after, fx.content("src/a.rs"));
    assert_eq!(fx.read("src/a.rs").unwrap(), b"dirty before\n// executor\n");

    // An unowned dirty file the executor changes further is its mutation.
    let fx = Fixture::new();
    fx.write("README.md", "# edited by a human\n");
    let (_, executed) = fx.run("unauthorized", &["src/a.rs"]);
    assert_eq!(executed.capture.outcome, ExecutionOutcome::ScopeViolated);
    let change = &executed.capture.changes[0];
    assert_eq!(change.path, "README.md");
    assert_eq!(
        change.before,
        Content::File(sha256(b"# edited by a human\n"))
    );
    assert_eq!(fx.read("README.md").unwrap(), b"# edited by a human\n");
}

fn lingering_writers_are_unattributable() {
    // The workspace keeps changing after the executor ended.
    let fx = Fixture::new();
    let untouched = tree(&fx);
    let (_, executed) = fx.run("linger", &["src/a.rs"]);
    assert_eq!(executed.capture.outcome, ExecutionOutcome::Unattributable);
    assert_eq!(executed.capture.attribution, Some(Attribution::Unsettled));
    assert_eq!(executed.capture.install, Install::NotAttempted);
    thread::sleep(Duration::from_secs(2));
    assert_eq!(tree(&fx), untouched);
}

fn workspaces_hold_no_canonical_state() {
    let fx = Fixture::new();
    let state = fx.read(".agentctl/state.db").unwrap();
    let scope = ["src/a.rs"];
    let (generation, executed) = fx.run("probe", &scope);
    let probe: Value = serde_json::from_str(executed.summary.as_deref().unwrap()).unwrap();
    // The executor worked outside the project, seeing the repository's
    // content and neither agentctl's state nor Git's, anywhere above it
    // either.
    let cwd = PathBuf::from(probe["cwd"].as_str().unwrap());
    assert!(!cwd.starts_with(&fx.root) && !fx.root.starts_with(&cwd));
    assert_eq!(
        probe["seen"],
        json!([
            ".gitignore",
            "README.md",
            "agentctl.toml",
            "src/[id].rs",
            "src/a.rs",
            "src/b.rs",
            "src/i.rs",
            "src/lib.rs"
        ])
    );
    assert_eq!(probe["state_reachable"], false);
    assert_eq!(probe["direct"], false);
    // What it wrote as agentctl's state stayed in its workspace, observed
    // as a mutation beyond its authority, which blocks its candidate.
    let capture = &executed.capture;
    assert_eq!(capture.outcome, ExecutionOutcome::ScopeViolated);
    assert_eq!(
        capture.unauthorized(),
        [".agentctl/fake", ".agentctl/state.db"]
    );
    assert_eq!(capture.install, Install::NotAttempted);
    assert!(fx.read(".agentctl/fake").is_none());
    let now = fx.read(".agentctl/state.db").unwrap();
    assert!(!now.starts_with(b"forged") && now.len() >= state.len());
    assert_eq!(fx.read("src/a.rs").unwrap(), b"pub fn a() -> u8 { 1 }\n");
    // The canonical state reads as agentctl left it.
    held(&fx, generation, &scope, &executed);
    // The workspace is gone once the attempt is finished.
    assert!(!cwd.exists());

    // No task can be authorized to mutate agentctl's or Git's state.
    let (project, mut store) = fx.open("nothing");
    let plan = store
        .create_plan(&agentctl::state::HumanIntent {
            objective: "escape".into(),
            constraints: Vec::new(),
            completion_criteria: Vec::new(),
        })
        .unwrap();
    for path in [".agentctl/state.db", "src/.git/config"] {
        let add = Plan::AddTask {
            task: "escape".into(),
            objective: "escape".into(),
            context: String::new(),
            paths: vec![path.into()],
            depends_on: Vec::new(),
        };
        assert!(
            planner::apply(&project, &mut store, plan, &[add]).is_err(),
            "{path}"
        );
    }
}

/// An action of another agent, in flight until reconciled.
fn in_flight(store: &mut Store) -> JournalId {
    let plan = store
        .create_plan(&agentctl::state::HumanIntent {
            objective: "Elsewhere".into(),
            constraints: Vec::new(),
            completion_criteria: Vec::new(),
        })
        .unwrap();
    let agent = store
        .create_agent(Role::Planner, AgentScope::Plan(plan))
        .unwrap();
    let intent = Intent {
        action: "rewrite".into(),
        parameters: serde_json::Map::new(),
    };
    let entry = store.intend(agent, &intent).unwrap();
    store.act(entry, None).unwrap();
    entry
}

/// Starts an executor running `scenario`, which waits once started until
/// released; `meanwhile` runs in between.
fn awaited(
    fx: &Fixture,
    scenario: &str,
    scope: &[&str],
    meanwhile: impl FnOnce(&mut Store),
) -> (GenerationId, Executed) {
    let (task, generation) = fx.task(scenario, scope);
    let (project, mut store) = fx.open(scenario);
    let running = executor::start(
        &project,
        &mut store,
        task,
        generation,
        Some(fake_executor()),
    )
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !marker(STARTED).exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    assert!(running.control().observe().alive());
    meanwhile(&mut store);
    fs::write(marker(RELEASE), "").unwrap();
    let executed = running.finish(&project, &mut store).unwrap();
    for name in [STARTED, RELEASE] {
        fs::remove_file(marker(name)).unwrap();
    }
    (generation, executed)
}

fn concurrent_project_changes_are_never_overwritten() {
    // Someone rewrites the one path the executor may mutate in the project
    // while it works: its candidate stands, but installing it would
    // overwrite that change, so nothing is installed.
    let fx = Fixture::new();
    let before = fx.accepted();
    let rival = "pub fn a() -> u8 { 7 } // rival\n";
    let (generation, executed) = awaited(&fx, "await-write", &["src/a.rs"], |_| {
        fx.write("src/a.rs", rival);
    });
    let capture = &executed.capture;
    assert_eq!(capture.outcome, ExecutionOutcome::Candidate);
    assert_eq!(
        changes(&executed),
        [("src/a.rs", ChangeKind::Modified, true)]
    );
    assert_eq!(
        installed(&executed),
        Some((InstallOutcome::Drifted, vec!["src/a.rs".to_string()]))
    );
    assert_eq!(fx.read("src/a.rs").unwrap(), rival.as_bytes());
    held(&fx, generation, &["src/a.rs"], &executed);
    assert_eq!(fx.accepted(), before);
    let (_, store) = fx.open("nothing");
    let entries = store.continuation(store.execution(generation).unwrap().unwrap().agent);
    let install = entries.unwrap().pop().unwrap();
    assert_eq!(install.intent.action, "executor.install");
    let ActionStatus::Reconciled(_, reconciliation) = install.status else {
        panic!()
    };
    assert_eq!(reconciliation.outcome, ActionOutcome::Failed);

    // Every path is validated before any is written: one drifted path of
    // three leaves all three as they were.
    let fx = Fixture::new();
    let scope = ["src/a.rs", "src/b.rs", "src/new/c.rs"];
    let (_, executed) = awaited(&fx, "await-multi", &scope, |_| {
        fx.write("src/b.rs", "pub fn b() {} // kept by a human\n");
    });
    assert_eq!(executed.capture.outcome, ExecutionOutcome::Candidate);
    assert_eq!(
        installed(&executed),
        Some((InstallOutcome::Drifted, vec!["src/b.rs".to_string()]))
    );
    assert_eq!(fx.read("src/a.rs").unwrap(), b"pub fn a() -> u8 { 1 }\n");
    assert_eq!(
        fx.read("src/b.rs").unwrap(),
        b"pub fn b() {} // kept by a human\n"
    );
    assert!(fx.read("src/new/c.rs").is_none());

    // A path created where the executor creates one is someone's work too.
    let fx = Fixture::new();
    let (_, executed) = awaited(&fx, "await-multi", &scope, |_| {
        fx.write("src/new/c.rs", "// a human's\n");
    });
    assert_eq!(
        installed(&executed),
        Some((InstallOutcome::Drifted, vec!["src/new/c.rs".to_string()]))
    );
    assert_eq!(fx.read("src/new/c.rs").unwrap(), b"// a human's\n");
    assert!(fx.read("src/b.rs").is_some());
}

fn other_actions_do_not_block_installing() {
    // Another agent's action in flight, and changes elsewhere in the
    // project, cast no doubt on what changed in the workspace, nor stand in
    // the way of installing it.
    let fx = Fixture::new();
    let (_, executed) = awaited(&fx, "await-write", &["src/a.rs"], |store| {
        in_flight(store);
        fx.write("README.md", "# changed meanwhile\n");
    });
    assert_eq!(executed.capture.outcome, ExecutionOutcome::Candidate);
    assert_eq!(executed.capture.attribution, None);
    assert_eq!(
        installed(&executed),
        Some((InstallOutcome::Installed, Vec::new()))
    );
    assert_eq!(fx.read("src/a.rs").unwrap(), b"pub fn a() -> u8 { 8 }\n");
    assert_eq!(fx.read("README.md").unwrap(), b"# changed meanwhile\n");

    // Nor does another executor, working and installing meanwhile.
    let fx = Fixture::new();
    let mut other = None;
    let (_, executed) = awaited(&fx, "await-write", &["src/a.rs"], |_| {
        other = Some(fx.run("delete", &["src/b.rs"]).1);
    });
    for executed in [&executed, other.as_ref().unwrap()] {
        assert_eq!(executed.capture.outcome, ExecutionOutcome::Candidate);
        assert_eq!(
            installed(executed),
            Some((InstallOutcome::Installed, Vec::new()))
        );
    }
    assert_eq!(fx.read("src/a.rs").unwrap(), b"pub fn a() -> u8 { 8 }\n");
    assert!(fx.read("src/b.rs").is_none());
}

fn failed_installs_restore_the_working_tree() {
    // Where the candidate creates `src/new/c.rs`, a file now stands at
    // `src/new`: no drift at any changed path, but the last write fails,
    // and what the first two wrote is restored.
    let fx = Fixture::new();
    let scope = ["src/a.rs", "src/b.rs", "src/new/c.rs"];
    let (generation, executed) = awaited(&fx, "await-multi", &scope, |_| {
        fx.write("src/new", "// a file, not a directory\n");
    });
    assert_eq!(executed.capture.outcome, ExecutionOutcome::Candidate);
    assert_eq!(
        installed(&executed),
        Some((InstallOutcome::Failed, Vec::new()))
    );
    let diagnostic = executed.install_diagnostic.as_deref().unwrap();
    assert!(diagnostic.contains("src/new/c.rs"), "{diagnostic}");
    assert_eq!(fx.read("src/a.rs").unwrap(), b"pub fn a() -> u8 { 1 }\n");
    assert_eq!(fx.read("src/b.rs").unwrap(), b"pub fn b() {}\n");
    assert_eq!(fx.read("src/new").unwrap(), b"// a file, not a directory\n");
    held(&fx, generation, &scope, &executed);
}

fn executors_need_their_whole_scope_owned() {
    let fx = Fixture::new();
    let (project, mut store) = fx.open("modify");
    let (task, generation) = fx.planned(&project, &mut store, "pair", &["src/a.rs", "src/b.rs"]);
    let partial = store.acquire_ownership(generation, &["src/a.rs"]).unwrap();
    assert_eq!(partial, Acquisition::Acquired);
    let refused = executor::start(
        &project,
        &mut store,
        task,
        generation,
        Some(fake_executor()),
    );
    let message = format!("{:#}", refused.err().unwrap());
    assert!(message.contains("does not own `src/b.rs`"), "{message}");

    // Nor while another generation owns part of it.
    fx.task("rival", &["src/b.rs"]);
    let (project, mut store) = fx.open("modify");
    let refused = executor::start(
        &project,
        &mut store,
        task,
        generation,
        Some(fake_executor()),
    );
    let message = format!("{:#}", refused.err().unwrap());
    assert!(
        message.contains("`src/b.rs` is owned by generation"),
        "{message}"
    );
    // No executor was recorded or ran.
    assert_eq!(store.execution(generation).unwrap(), None);
    assert_eq!(fx.read("src/a.rs").unwrap(), b"pub fn a() -> u8 { 1 }\n");
}

fn interruption_never_fabricates_a_candidate() {
    // agentctl disappears while the executor acts.
    let fx = Fixture::new();
    let untouched = tree(&fx);
    let (task, generation) = fx.task("hang", &["src/a.rs"]);
    let (project, mut store) = fx.open("hang");
    let running = executor::start(
        &project,
        &mut store,
        task,
        generation,
        Some(fake_executor()),
    )
    .unwrap();
    let control = running.control();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !marker(STARTED).exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
    fs::remove_file(marker(STARTED)).unwrap();
    assert!(control.observe().alive());
    drop(running);
    drop(store);
    let (_, store) = fx.open("hang");
    let execution = store.execution(generation).unwrap().unwrap();
    let ExecutionStatus::OutcomeUnknown {
        invocation: Some(invocation),
    } = execution.status
    else {
        panic!("{:?}", execution.status);
    };
    assert_eq!(
        store.invocation(invocation).unwrap().state,
        InvocationState::Running
    );
    let entry = store.journal_entry(execution.journal).unwrap();
    assert!(matches!(entry.status, ActionStatus::OutcomeUnknown(_)));
    assert_eq!(store.owned_paths(generation).unwrap(), ["src/a.rs"]);
    // What it wrote never left its workspace.
    assert_eq!(tree(&fx), untouched);

    // The invocation ended, but the workspace could not be captured.
    #[cfg(unix)]
    {
        let fx = Fixture::new();
        let (task, generation) = fx.task("unreadable", &[]);
        let (project, mut store) = fx.open("unreadable");
        let failed = executor::start(
            &project,
            &mut store,
            task,
            generation,
            Some(fake_executor()),
        )
        .unwrap()
        .finish(&project, &mut store);
        assert!(failed.is_err());
        let execution = store.execution(generation).unwrap().unwrap();
        let ExecutionStatus::OutcomeUnknown {
            invocation: Some(invocation),
        } = execution.status
        else {
            panic!("{:?}", execution.status);
        };
        let ended = store.invocation(invocation).unwrap().state;
        assert_eq!(ended, InvocationState::Succeeded);
        assert!(fx.read("src/locked.rs").is_none());
    }

    // Intended, but no invocation was ever recorded or launched.
    let fx = Fixture::new();
    let (task, generation) = fx.task("minimal", &["src/a.rs"]);
    let (mut project, mut store) = fx.open("modify");
    project.config.agents.executor.reasoning_effort = agentctl::config::ReasoningEffort::Minimal;
    let refused = executor::start(
        &project,
        &mut store,
        task,
        generation,
        Some(fake_executor()),
    );
    assert!(refused.is_err());
    let execution = store.execution(generation).unwrap().unwrap();
    assert_eq!(execution.status, ExecutionStatus::Intended);
    assert!(store.invocations(execution.agent).unwrap().is_empty());
}
