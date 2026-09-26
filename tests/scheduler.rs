//! Scheduler tests against fake coding agents: this test binary itself,
//! copied under a name that makes it act as Claude Code, as each task's
//! executor and verifier, running real Blocks 11 to 13 pipelines through
//! `scheduler::run`. Each fake plays the scenario its task's objective
//! names, logs when it starts and ends, and, when told to, waits to be let
//! go. What the scheduler did is checked against canonical state and that
//! log, never against what the fakes report. They spend no provider tokens.

use std::env;
use std::ffi::OsStr;
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::panic;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode};
use std::sync::{Barrier, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use agentctl::planner::{self, Command as Plan, Planned};
use agentctl::project::Project;
use agentctl::scheduler::{self, Report};
use agentctl::source;
use agentctl::state::{
    AcceptancePhase, ActionOutcome, ActionStatus, Condition, Evidence, GenerationState,
    HumanIntent, PipelineOutcome, PlanId, PlanState, Release, Replan, Store, TaskId, TaskStatus,
    VerificationOutcome, VerificationStatus,
};
use serde_json::{Value, json};
use tempfile::TempDir;

const FAKE: &str = "fake-agent";
/// This test binary, copied under this name, runs one plan as a scheduler
/// process of its own.
const CHILD: &str = "scheduler-child";
/// Where the fakes log and wait, outside the project and every workspace,
/// passed on as an environment variable Claude Code's adapter lets through.
const MARKERS: &str = "ANTHROPIC_AGENTCTL_TEST_MARKERS";

fn main() -> ExitCode {
    let argv0 = env::args_os().next().unwrap_or_default();
    if Path::new(&argv0).file_stem() == Some(OsStr::new(FAKE)) {
        return fake();
    }
    if Path::new(&argv0).file_stem() == Some(OsStr::new(CHILD)) {
        return child();
    }
    let tests: &[(&str, fn())] = &[
        (
            "one_task_runs_through_blocks_11_to_13",
            one_task_runs_through_blocks_11_to_13,
        ),
        (
            "dependents_run_only_after_completed_acceptance",
            dependents_run_only_after_completed_acceptance,
        ),
        (
            "independent_tasks_overlap_within_the_configured_ceiling",
            independent_tasks_overlap_within_the_configured_ceiling,
        ),
        (
            "ownership_conflicts_serialize_without_edges",
            ownership_conflicts_serialize_without_edges,
        ),
        (
            "failures_release_capacity_and_block_dependents",
            failures_release_capacity_and_block_dependents,
        ),
        (
            "a_losing_scheduler_launches_no_provider",
            a_losing_scheduler_launches_no_provider,
        ),
        ("literal_paths_stay_literal", literal_paths_stay_literal),
        (
            "verifiers_never_see_other_pipelines_candidates",
            verifiers_never_see_other_pipelines_candidates,
        ),
        (
            "verifiers_never_see_other_processes_candidates",
            verifiers_never_see_other_processes_candidates,
        ),
        (
            "a_fresh_planner_retries_stopped_work_from_canonical_state",
            a_fresh_planner_retries_stopped_work_from_canonical_state,
        ),
        (
            "failed_replanning_changes_nothing",
            failed_replanning_changes_nothing,
        ),
        (
            "a_stale_replan_is_never_applied",
            a_stale_replan_is_never_applied,
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
        // Each test gets markers of its own.
        let markers = tempfile::tempdir().unwrap();
        // SAFETY: tests run one at a time, and set this before any thread
        // of theirs starts.
        unsafe { env::set_var(MARKERS, markers.path()) };
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

fn marker(name: &str) -> PathBuf {
    Path::new(&env::var_os(MARKERS).unwrap()).join(name)
}

fn log(line: &str) {
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(marker("log"))
        .unwrap();
    // One write per line, so concurrent fakes never interleave within one.
    file.write_all(format!("{line}\n").as_bytes()).unwrap();
}

/// Acts as Claude Code, as the executor or verifier its instructions say,
/// playing the scenario its task's objective names.
fn fake() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    let bootstrap = args
        .iter()
        .find_map(|a| a.strip_prefix("--append-system-prompt="))
        .unwrap_or_default();
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input).unwrap();
    let packet: Value = serde_json::from_str(&input).unwrap();
    if bootstrap.starts_with("You are the planning agent") {
        return replan(&args, &packet);
    }
    let key = packet["task"]["key"].as_str().unwrap().to_owned();
    let objective = packet["task"]["objective"].as_str().unwrap().to_owned();
    let says = |token: &str| objective.split_whitespace().any(|t| t == token);
    let role = if bootstrap.starts_with("You are a verifier") {
        "verifier"
    } else {
        "executor"
    };
    log(&format!("start {role} {key}"));
    if role == "verifier" {
        let mut view = serde_json::Map::new();
        record_view(Path::new("."), "", &mut view);
        fs::write(
            marker(&format!("view-{key}")),
            Value::from(view).to_string(),
        )
        .unwrap();
        if says("hold-verify") {
            fs::write(marker(&format!("verifying-{key}")), "").unwrap();
            let deadline = Instant::now() + Duration::from_secs(60);
            while !marker(&format!("release-verify-{key}")).exists() && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(10));
            }
        }
    }
    let result = if role == "executor" {
        if says("hold") {
            fs::write(marker(&format!("started-{key}")), "").unwrap();
            let deadline = Instant::now() + Duration::from_secs(60);
            while !marker(&format!("release-{key}")).exists() && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(10));
            }
        }
        let paths = packet["authority"]["mutable_paths"].as_array().unwrap();
        for path in paths {
            let path = Path::new(path.as_str().unwrap());
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, format!("// {key} was here\n")).unwrap();
        }
        let status = if says("exec=fail") {
            "failed"
        } else {
            "succeeded"
        };
        json!({"status": status, "summary": "done", "modified_paths": paths})
    } else if says("verify=fail") {
        json!({"verdict": "fail",
               "checked": [{"check": "read", "command": null, "outcome": "failed",
                            "evidence": "wrong"}],
               "blockers": [{"id": "b1", "summary": "wrong", "paths": [], "evidence": "e",
                             "location": null}],
               "non_blocking": []})
    } else {
        json!({"verdict": "pass",
               "checked": [{"check": "read", "command": null, "outcome": "passed",
                            "evidence": "right"}],
               "blockers": [], "non_blocking": []})
    };
    log(&format!("end {role} {key}"));
    let init = json!({"type": "system", "subtype": "init", "session_id": "fake-session"});
    let result = json!({"type": "result", "subtype": "success", "is_error": false,
        "session_id": "fake-session", "result": "prose", "structured_output": result,
        "usage": {"input_tokens": 3, "output_tokens": 2}});
    println!("{init}\n{result}");
    std::io::stdout().flush().unwrap();
    ExitCode::SUCCESS
}

/// Acts as Claude Code replanning, as the `planner-script` marker says:
/// crashing, answering malformed output, deriving its commands from its
/// input alone (`auto`: retry every stopped task, without the tokens that
/// made it fail), or answering the commands the marker holds; after being
/// held until let go, when it starts with `hold `. Records its input.
fn replan(args: &[String], packet: &Value) -> ExitCode {
    // Nothing is resumed: the input is all a planner knows.
    assert!(args.iter().any(|a| a == "--no-session-persistence"));
    assert!(!args.iter().any(|a| a.starts_with("--resume")));
    let n = (0..).find(|n| !marker(&format!("planner-input-{n}")).exists());
    let recorded = marker(&format!("planner-input-{}", n.unwrap()));
    fs::write(recorded, packet.to_string()).unwrap();
    log("start planner");
    let script = fs::read_to_string(marker("planner-script")).unwrap_or_default();
    let script = match script.strip_prefix("hold ") {
        Some(rest) => {
            fs::write(marker("planning"), "").unwrap();
            let deadline = Instant::now() + Duration::from_secs(60);
            while !marker("release-planner").exists() && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(10));
            }
            rest
        }
        None => script.as_str(),
    };
    let commands = match script {
        "crash" => return ExitCode::from(3),
        "malformed" => json!("not a list of commands"),
        "auto" => {
            let mut commands = Vec::new();
            for task in packet["plan"]["tasks"].as_array().unwrap() {
                if task["status"] != "stopped" {
                    continue;
                }
                let objective = task["objective"].as_str().unwrap();
                let objective: Vec<&str> = objective
                    .split_whitespace()
                    .filter(|t| !t.ends_with("=fail"))
                    .collect();
                commands.push(json!({"op": "update_task", "task": task["task"],
                    "objective": objective.join(" "), "context": null, "paths": null}));
                commands.push(json!({"op": "retry_task", "task": task["task"]}));
            }
            Value::from(commands)
        }
        literal => serde_json::from_str(literal).unwrap(),
    };
    log("end planner");
    let init = json!({"type": "system", "subtype": "init", "session_id": "fake-session"});
    let result = json!({"type": "result", "subtype": "success", "is_error": false,
        "session_id": "fake-session", "result": "prose",
        "structured_output": {"commands": commands, "explanation": "prose"},
        "usage": {"input_tokens": 3, "output_tokens": 2}});
    println!("{init}\n{result}");
    std::io::stdout().flush().unwrap();
    ExitCode::SUCCESS
}

/// What the `n`th planner invocation was given.
fn planner_input(n: usize) -> Value {
    let text = fs::read_to_string(marker(&format!("planner-input-{n}"))).unwrap();
    serde_json::from_str(&text).unwrap()
}

/// Every file beneath `dir`, a verifier's working directory, by its path
/// there, with its text.
fn record_view(dir: &Path, prefix: &str, view: &mut serde_json::Map<String, Value>) {
    for entry in fs::read_dir(dir).unwrap() {
        let entry = entry.unwrap();
        let path = format!("{prefix}{}", entry.file_name().to_str().unwrap());
        if entry.file_type().unwrap().is_dir() {
            record_view(&entry.path(), &format!("{path}/"), view);
        } else {
            let text = String::from_utf8_lossy(&fs::read(entry.path()).unwrap()).into_owned();
            view.insert(path, text.into());
        }
    }
}

/// Acts as `agentctl run`: runs the plan its arguments name, in the project
/// they name, with the fake agent they name, in a process of its own.
fn child() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    let project = Project::load(Path::new(&args[0])).unwrap();
    let plan: PlanId = args[1].parse().unwrap();
    let report = scheduler::run(&project, plan, Some(PathBuf::from(&args[2]))).unwrap();
    let clean = report.stopped.is_none() && report.finished.iter().all(|f| f.error.is_none());
    if clean {
        ExitCode::SUCCESS
    } else {
        eprintln!("{report:?}");
        ExitCode::FAILURE
    }
}

/// The fake agent executable, shared by every test.
fn fake_agent() -> PathBuf {
    static FAKE_DIR: OnceLock<(TempDir, PathBuf)> = OnceLock::new();
    copy_named(&FAKE_DIR, FAKE)
}

/// The scheduler process executable, shared by every test.
fn scheduler_child() -> PathBuf {
    static CHILD_DIR: OnceLock<(TempDir, PathBuf)> = OnceLock::new();
    copy_named(&CHILD_DIR, CHILD)
}

fn copy_named(copy: &OnceLock<(TempDir, PathBuf)>, name: &str) -> PathBuf {
    copy.get_or_init(|| {
        let dir = tempfile::tempdir().unwrap();
        let path = dir
            .path()
            .join(format!("{name}{}", env::consts::EXE_SUFFIX));
        fs::copy(env::current_exe().unwrap(), &path).unwrap();
        (dir, path)
    })
    .1
    .clone()
}

/// What the fakes logged, line by line.
fn logged() -> Vec<String> {
    fs::read_to_string(marker("log"))
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
}

/// The tasks whose executors started, in order.
fn launched() -> Vec<String> {
    logged()
        .iter()
        .filter_map(|l| l.strip_prefix("start executor ").map(str::to_owned))
        .collect()
}

/// The most executors that ever ran at once.
fn peak() -> usize {
    let (mut now, mut peak) = (0usize, 0);
    for line in logged() {
        if line.starts_with("start executor") {
            now += 1;
            peak = peak.max(now);
        } else if line.starts_with("end executor") {
            now -= 1;
        }
    }
    peak
}

fn await_marker(name: &str) {
    let deadline = Instant::now() + Duration::from_secs(60);
    while !marker(name).exists() {
        assert!(Instant::now() < deadline, "{name} never appeared");
        thread::sleep(Duration::from_millis(10));
    }
}

fn release(key: &str) {
    fs::write(marker(&format!("release-{key}")), "").unwrap();
}

/// Lets every fake waiting on these markers go once dropped, even should
/// the test fail first, so that no fake outlives it by more than a moment.
struct Unblock(&'static [&'static str]);

impl Drop for Unblock {
    fn drop(&mut self) {
        for name in self.0 {
            let _ = fs::write(marker(name), "");
        }
    }
}

/// A scheduler process of its own, which, once dropped, is let go as
/// [`Unblock`] would and waited for, only so long.
struct Scheduler {
    child: Child,
    unblock: &'static [&'static str],
}

impl Scheduler {
    /// Waits for the process to end, only so long, and whether it succeeded.
    fn wait(&mut self) -> bool {
        let deadline = Instant::now() + Duration::from_secs(90);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status.success();
            }
            if Instant::now() > deadline {
                // Defense in depth only: every fake ends on its own.
                let _ = self.child.kill();
                let _ = self.child.wait();
                return false;
            }
            thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for Scheduler {
    fn drop(&mut self) {
        drop(Unblock(self.unblock));
        self.wait();
    }
}

/// The view a fake verifier of `key` was given.
fn view(key: &str) -> Value {
    serde_json::from_str(&fs::read_to_string(marker(&format!("view-{key}"))).unwrap()).unwrap()
}

/// A Git repository holding a project whose accepted source is recorded,
/// with a ready plan of tasks `(key, objective, scope, depends_on)`.
struct Fixture {
    _dir: TempDir,
    project: Project,
    plan: PlanId,
    tasks: Vec<TaskId>,
}

impl Fixture {
    fn new(max_concurrency: u32, tasks: &[(&str, &str, &[&str], &[&str])]) -> Self {
        Self::new_with(max_concurrency, tasks, &[])
    }

    /// [`Fixture::new`], with the files at `also` accepted too.
    fn new_with(
        max_concurrency: u32,
        tasks: &[(&str, &str, &[&str], &[&str])],
        also: &[&str],
    ) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let git = Command::new("git")
            .arg("-C")
            .arg(&root)
            .args(["init", "-q"])
            .status()
            .unwrap();
        assert!(git.success());
        let role = "provider = \"claude\"\nmodel = \"fake\"\nreasoning_effort = \"high\"\n";
        let config = format!(
            "[project]\nname = \"demo\"\nversion = \"0.1.0\"\n\n\
             [codegraph]\nroots = [\"src\"]\n\n\
             [agents]\nmax_concurrency = {max_concurrency}\n\n\
             [agents.planner]\n{role}\n[agents.executor]\n{role}\n[agents.verifier]\n{role}"
        );
        let write = |path: &str, text: &str| {
            let path = root.join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, text).unwrap();
        };
        write("agentctl.toml", &config);
        write("README.md", "# demo\n");
        write("src/lib.rs", "pub fn lib() {}\n");
        write("src/i.rs", "pub fn decoy() {}\n");
        for (_, _, scope, _) in tasks {
            // A path named new is one the task creates: it never existed,
            // so it has no accepted history.
            for path in scope.iter().filter(|p| !p.contains("new")) {
                write(path, "// accepted\n");
            }
        }
        for path in also {
            write(path, "// accepted\n");
        }
        let project = Project::load(&root).unwrap();
        let mut store = project.hydrate().unwrap();
        source::baseline(&project, &mut store).unwrap();
        let (plan, tasks) = add_plan(&project, &mut store, tasks);
        Self {
            _dir: dir,
            project,
            plan,
            tasks,
        }
    }

    /// Adds another ready plan of `tasks`.
    fn add_plan(&self, tasks: &[(&str, &str, &[&str], &[&str])]) -> (PlanId, Vec<TaskId>) {
        add_plan(&self.project, &mut self.store(), tasks)
    }

    fn store(&self) -> Store {
        Store::open(&self.project.state_path()).unwrap()
    }

    /// Runs the plan as `agentctl run` would, with the fakes as providers.
    fn run(&self) -> Report {
        let report = scheduler::run(&self.project, self.plan, Some(fake_agent())).unwrap();
        assert_eq!(report.stopped, None);
        for end in &report.finished {
            assert_eq!(end.error, None, "{end:?}");
        }
        report
    }

    /// Replans the plan as `agentctl plan update` would, from a store of
    /// its own, with the fake as its planner.
    fn replan(&self) -> Planned {
        let project = Project::load(&self.project.root).unwrap();
        let mut store = project.hydrate().unwrap();
        planner::replan(&project, &mut store, self.plan, Some(fake_agent()))
            .unwrap()
            .finish(&project, &mut store)
            .unwrap()
    }

    /// Everything replanning could change of the plan, or of how it runs.
    fn replanning_state(&self) -> Value {
        let store = self.store();
        let tasks: Vec<Value> = store
            .tasks(self.plan)
            .unwrap()
            .iter()
            .map(|t| {
                let generations: Vec<Value> = store
                    .generations(t.id)
                    .unwrap()
                    .iter()
                    .map(|g| {
                        json!([
                            g.id,
                            g.number,
                            g.state.to_string(),
                            store.generation_revision(g.id).unwrap(),
                            store.owned_paths(g.id).unwrap()
                        ])
                    })
                    .collect();
                let revisions: Vec<Value> = store
                    .revisions(t.id)
                    .unwrap()
                    .iter()
                    .map(|r| json!([r.number, r.objective, r.scope]))
                    .collect();
                let authorizations: Vec<Value> = store
                    .retry_authorizations(t.id)
                    .unwrap()
                    .iter()
                    .map(|a| json!([a.after, a.used_by]))
                    .collect();
                json!({"key": t.key, "objective": t.objective, "context": t.context,
                       "scope": t.scope, "depends_on": t.depends_on,
                       "generations": generations, "revisions": revisions,
                       "authorizations": authorizations,
                       "cancelled": store.cancellation(t.id).unwrap()})
            })
            .collect();
        let claims: Vec<Value> = store
            .claims()
            .unwrap()
            .iter()
            .map(|c| json!([c.generation, c.released.map(|(o, _)| o.to_string())]))
            .collect();
        let plan = store.plan(self.plan).unwrap();
        json!({"state": plan.state.to_string(), "intent": plan.intent.objective, "tasks": tasks,
               "claims": claims, "replans": store.replans(self.plan).unwrap().len()})
    }

    /// The planner's journal: each replanning action and what it
    /// established.
    fn planner_journal(&self) -> Vec<(String, Option<ActionOutcome>, Vec<Evidence>)> {
        let mut store = self.store();
        let agent = store.planner(self.plan).unwrap();
        store
            .continuation(agent)
            .unwrap()
            .into_iter()
            .map(|e| match e.status {
                ActionStatus::Reconciled(_, r) => (e.intent.action, Some(r.outcome), r.evidence),
                _ => (e.intent.action, None, Vec::new()),
            })
            .collect()
    }

    fn status(&self, task: TaskId) -> TaskStatus {
        let snapshot = self
            .store()
            .snapshot(self.plan, std::num::NonZeroU32::MIN)
            .unwrap();
        snapshot.status(task).unwrap().clone()
    }

    fn seq(&self, kind: &str, task: TaskId) -> i64 {
        let events = self.store().events_after(0, 1_000_000).unwrap();
        events
            .iter()
            .find(|e| e.kind == kind && e.task == Some(task))
            .unwrap_or_else(|| panic!("no {kind} for task {task}"))
            .seq
    }

    fn read(&self, path: &str) -> String {
        fs::read_to_string(self.project.root.join(path)).unwrap()
    }
}

/// Creates a ready plan of `tasks` `(key, objective, scope, depends_on)`.
fn add_plan(
    project: &Project,
    store: &mut Store,
    tasks: &[(&str, &str, &[&str], &[&str])],
) -> (PlanId, Vec<TaskId>) {
    let plan = store
        .create_plan(&HumanIntent {
            objective: "Improve the demo".into(),
            constraints: Vec::new(),
            completion_criteria: vec!["cargo test passes".into()],
        })
        .unwrap();
    let mut commands: Vec<Plan> = tasks
        .iter()
        .map(|(key, objective, scope, depends_on)| Plan::AddTask {
            task: key.to_string(),
            objective: objective.to_string(),
            context: String::new(),
            paths: scope.iter().map(|p| p.to_string()).collect(),
            depends_on: depends_on.iter().map(|d| d.to_string()).collect(),
        })
        .collect();
    commands.push(Plan::Finalize {});
    assert!(planner::apply(project, store, plan, &commands).unwrap());
    let tasks = store.tasks(plan).unwrap().iter().map(|t| t.id).collect();
    (plan, tasks)
}

fn one_task_runs_through_blocks_11_to_13() {
    let fx = Fixture::new(1, &[("only", "Change a", &["src/a.rs"], &[])]);
    let report = fx.run();
    assert_eq!(launched(), ["only"]);
    assert_eq!(
        logged(),
        [
            "start executor only",
            "end executor only",
            "start verifier only",
            "end verifier only"
        ]
    );
    let finished = &report.finished[0];
    assert_eq!(
        finished.release,
        Ok(Release::Released(PipelineOutcome::Accepted))
    );
    let store = fx.store();
    let generation = finished.work.generation;
    let phase = store.acceptance(generation).unwrap().unwrap().phase;
    assert_eq!(phase, AcceptancePhase::Completed);
    assert_eq!(fx.read("src/a.rs"), "// only was here\n");
    let source = store.accepted_source("src/a.rs").unwrap().unwrap();
    assert_eq!(source.generation, Some(generation));
    assert!(store.owned_paths(generation).unwrap().is_empty());
    assert_eq!(report.snapshot.condition(), Condition::AllCompleted);
    assert_eq!(store.plan(fx.plan).unwrap().state, PlanState::Running);
}

fn dependents_run_only_after_completed_acceptance() {
    let fx = Fixture::new(
        4,
        &[
            ("base", "Change a", &["src/a.rs"], &[]),
            ("next", "Change b", &["src/b.rs"], &["base"]),
        ],
    );
    let report = fx.run();
    assert_eq!(launched(), ["base", "next"]);
    // Nothing of `next` started before `base`'s acceptance completed.
    let base_verified = logged().iter().position(|l| l == "end verifier base");
    let next_started = logged().iter().position(|l| l == "start executor next");
    assert!(base_verified < next_started);
    let completed = fx.seq("acceptance.completed", fx.tasks[0]);
    assert!(completed < fx.seq("scheduler.claimed", fx.tasks[1]));
    assert_eq!(report.snapshot.condition(), Condition::AllCompleted);
    assert_eq!(peak(), 1);
}

fn independent_tasks_overlap_within_the_configured_ceiling() {
    let fx = Fixture::new(
        2,
        &[
            ("a", "Change a hold", &["src/a.rs"], &[]),
            ("b", "Change b hold", &["src/b.rs"], &[]),
            ("c", "Change c hold", &["src/c.rs"], &[]),
        ],
    );
    thread::scope(|scope| {
        let run = scope.spawn(|| fx.run());
        await_marker("started-a");
        await_marker("started-b");
        // Two providers run at once; the third task is neither claimed nor
        // launched.
        let snapshot = fx
            .store()
            .snapshot(fx.plan, std::num::NonZeroU32::new(2).unwrap())
            .unwrap();
        assert_eq!(snapshot.capacity.held, 2);
        assert_eq!(snapshot.status(fx.tasks[2]), Some(&TaskStatus::Eligible));
        assert!(fx.store().generations(fx.tasks[2]).unwrap().is_empty());
        assert!(!launched().contains(&"c".to_owned()));
        for key in ["a", "b", "c"] {
            release(key);
        }
        let report = run.join().unwrap();
        assert_eq!(report.snapshot.condition(), Condition::AllCompleted);
    });
    assert_eq!(peak(), 2);
    assert_eq!(launched().len(), 3);
}

fn ownership_conflicts_serialize_without_edges() {
    let fx = Fixture::new(
        2,
        &[
            ("first", "Change shared hold", &["src/shared.rs"], &[]),
            (
                "second",
                "Change shared too",
                &["src/shared.rs", "src/x.rs"],
                &[],
            ),
        ],
    );
    thread::scope(|scope| {
        let run = scope.spawn(|| fx.run());
        await_marker("started-first");
        assert!(matches!(
            fx.status(fx.tasks[1]),
            TaskStatus::WaitingForOwnership(_)
        ));
        // Neither ownership nor anything else of `x.rs` was taken.
        assert_eq!(fx.store().owner("src/x.rs").unwrap(), None);
        release("first");
        run.join().unwrap();
    });
    assert_eq!(launched(), ["first", "second"]);
    assert_eq!(peak(), 1);
    assert!(fx.store().task(fx.tasks[1]).unwrap().depends_on.is_empty());
    assert_eq!(fx.read("src/shared.rs"), "// second was here\n");
    assert_eq!(fx.status(fx.tasks[1]), TaskStatus::Completed);
}

fn failures_release_capacity_and_block_dependents() {
    let fx = Fixture::new(
        1,
        &[
            ("broken", "Change a exec=fail", &["src/a.rs"], &[]),
            ("after-broken", "Change b", &["src/b.rs"], &["broken"]),
            ("rejected", "Change c verify=fail", &["src/c.rs"], &[]),
            ("after-rejected", "Change d", &["src/d.rs"], &["rejected"]),
        ],
    );
    let report = fx.run();
    // With a ceiling of one, the second ran only because the first
    // released its capacity.
    assert_eq!(launched(), ["broken", "rejected"]);
    let outcomes: Vec<_> = report.finished.iter().map(|f| f.release.clone()).collect();
    assert_eq!(
        outcomes,
        [
            Ok(Release::Released(PipelineOutcome::ExecutionFailed)),
            Ok(Release::Released(PipelineOutcome::VerificationFailed)),
        ]
    );
    for dependent in [1, 3] {
        assert_eq!(
            fx.status(fx.tasks[dependent]),
            TaskStatus::WaitingForDependencies(vec![fx.tasks[dependent - 1]])
        );
    }
    assert_eq!(report.snapshot.capacity.held, 0);
    assert_eq!(report.snapshot.condition(), Condition::Waiting);
    // The failed candidate was never accepted, nor retried.
    assert_eq!(fx.read("src/a.rs"), "// accepted\n");
    let again = fx.run();
    assert!(again.finished.is_empty());
    assert_eq!(launched().len(), 2);
    assert_eq!(fx.store().plan(fx.plan).unwrap().state, PlanState::Running);
}

fn a_losing_scheduler_launches_no_provider() {
    let fx = Fixture::new(4, &[("only", "Change a", &["src/a.rs"], &[])]);
    let barrier = Barrier::new(2);
    let reports: Vec<Report> = thread::scope(|scope| {
        let runs: Vec<_> = (0..2)
            .map(|_| {
                scope.spawn(|| {
                    barrier.wait();
                    fx.run()
                })
            })
            .collect();
        runs.into_iter().map(|r| r.join().unwrap()).collect()
    });
    assert_eq!(launched(), ["only"]);
    assert_eq!(reports.iter().map(|r| r.finished.len()).sum::<usize>(), 1);
    let store = fx.store();
    assert_eq!(store.generations(fx.tasks[0]).unwrap().len(), 1);
    assert_eq!(store.claims().unwrap().len(), 1);
}

fn literal_paths_stay_literal() {
    let scope: &[&str] = &["src/[id].rs", "src/with space.rs"];
    let fx = Fixture::new(1, &[("literal", "Change literally", scope, &[])]);
    let report = fx.run();
    let generation = report.finished[0].work.generation;
    let store = fx.store();
    let changes: Vec<String> = store
        .acceptance(generation)
        .unwrap()
        .unwrap()
        .changes
        .into_iter()
        .map(|c| c.path)
        .collect();
    assert_eq!(changes, scope);
    for path in scope {
        assert_eq!(fx.read(path), "// literal was here\n");
    }
    // `src/[id].rs` names that file, never `src/i.rs`, which it would match
    // as a pattern.
    assert_eq!(fx.read("src/i.rs"), "pub fn decoy() {}\n");
    assert_eq!(fx.status(fx.tasks[0]), TaskStatus::Completed);
}

/// A path task `b` creates, and one task `a` changes: literal names, never
/// patterns.
const NEW: &str = "src/b [id] (new)+@ü.rs";
const LITERAL: &str = "src/a (x)+@ü [id].rs";

fn await_status(fx: &Fixture, task: TaskId, status: TaskStatus) {
    let deadline = Instant::now() + Duration::from_secs(60);
    while fx.status(task) != status {
        assert!(
            Instant::now() < deadline,
            "task {task} never became {status:?}"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

fn line(text: &str) -> usize {
    logged()
        .iter()
        .position(|l| l == text)
        .unwrap_or_else(|| panic!("never logged {text}"))
}

/// What `a`'s verifier was given, with `b`'s unaccepted candidate installed
/// in the working tree: `a`'s candidate over accepted state, which holds
/// its dependency's accepted change and nothing of `b`.
fn assert_isolated(view: &Value, dependency: Option<&str>) {
    assert_eq!(view["src/a.rs"], "// a was here\n");
    assert_eq!(view["src/b.rs"], "// accepted\n");
    assert_eq!(view.get(NEW), None, "{view}");
    if let Some(text) = dependency {
        assert_eq!(view["src/dep.rs"], text);
    }
    // Repository content that nobody's candidate touched.
    assert_eq!(view["README.md"], "# demo\n");
    assert_eq!(view["src/i.rs"], "pub fn decoy() {}\n");
    assert!(view.get("agentctl.toml").is_some());
    assert!(
        view.as_object()
            .unwrap()
            .keys()
            .all(|p| !p.starts_with(".agentctl"))
    );
}

fn verifiers_never_see_other_pipelines_candidates() {
    let _unblock = Unblock(&["release-a", "release-verify-b"]);
    let fx = Fixture::new(
        3,
        &[
            ("dep", "Change dep", &["src/dep.rs"], &[]),
            (
                "b",
                "Change b hold-verify verify=fail",
                &["src/b.rs", NEW],
                &[],
            ),
            ("a", "Change a hold", &["src/a.rs", LITERAL], &["dep"]),
        ],
    );
    let (b, a) = (fx.tasks[1], fx.tasks[2]);
    let report = thread::scope(|scope| {
        let run = scope.spawn(|| fx.run());
        // `b` installed its candidate, whose verifier now runs, unresolved,
        // before `a`'s executor even finishes.
        await_marker("verifying-b");
        await_marker("started-a");
        assert_eq!(fx.read("src/b.rs"), "// b was here\n");
        assert_eq!(fx.read(NEW), "// b was here\n");
        release("a");
        await_status(&fx, a, TaskStatus::Completed);
        assert!(!logged().contains(&"end verifier b".to_owned()));
        fs::write(marker("release-verify-b"), "").unwrap();
        run.join().unwrap()
    });
    // The providers overlapped: `a` was verified while `b` was.
    assert!(line("start verifier b") < line("start verifier a"));
    assert!(line("end verifier a") < line("end verifier b"));

    let view_a = view("a");
    assert_isolated(&view_a, Some("// dep was here\n"));
    assert_eq!(view_a[LITERAL], "// a was here\n");
    // `b` was judged on its own candidate, over the same accepted state.
    let view_b = view("b");
    assert_eq!(view_b["src/b.rs"], "// b was here\n");
    assert_eq!(view_b[NEW], "// b was here\n");
    assert_eq!(view_b["src/a.rs"], "// accepted\n");
    assert_eq!(view_b[LITERAL], "// accepted\n");

    // `b` failed and stays in the working tree, never accepted; what `a`'s
    // verification established of the state it verified stands.
    let outcome = |task: TaskId| {
        let end = report
            .finished
            .iter()
            .find(|f| f.work.task == task)
            .unwrap();
        (end.work.generation, end.release.clone())
    };
    let (a_generation, a_release) = outcome(a);
    assert_eq!(a_release, Ok(Release::Released(PipelineOutcome::Accepted)));
    assert_eq!(
        outcome(b).1,
        Ok(Release::Released(PipelineOutcome::VerificationFailed))
    );
    assert_eq!(fx.read("src/b.rs"), "// b was here\n");
    let store = fx.store();
    assert_eq!(
        store
            .accepted_source("src/b.rs")
            .unwrap()
            .unwrap()
            .generation,
        None
    );
    // `b`'s new path gained no accepted state, not even absence.
    assert_eq!(store.accepted_source(NEW).unwrap(), None);
    for path in ["src/a.rs", LITERAL] {
        let accepted = store.accepted_source(path).unwrap().unwrap();
        assert_eq!(accepted.generation, Some(a_generation), "{path}");
    }
    let verifications = store.verifications(a_generation).unwrap();
    assert_eq!(verifications.len(), 1);
    assert!(matches!(
        &verifications[0].status,
        VerificationStatus::Finished(r) if r.outcome == VerificationOutcome::Passed
    ));
    assert_eq!(fx.status(a), TaskStatus::Completed);
}

fn verifiers_never_see_other_processes_candidates() {
    let fx = Fixture::new_with(4, &[("a", "Change a", &["src/a.rs"], &[])], &["src/b.rs"]);
    let (other, _) = fx.add_plan(&[(
        "b",
        "Change b hold-verify verify=fail",
        &["src/b.rs", NEW],
        &[],
    )]);
    let mut process = Scheduler {
        child: Command::new(scheduler_child())
            .arg(&fx.project.root)
            .arg(other.to_string())
            .arg(fake_agent())
            .spawn()
            .unwrap(),
        unblock: &["release-verify-b"],
    };
    // Another process's pipeline installed `b`'s candidate and verifies it.
    await_marker("verifying-b");
    assert_eq!(fx.read(NEW), "// b was here\n");
    let report = fx.run();
    assert_eq!(
        report.finished[0].release,
        Ok(Release::Released(PipelineOutcome::Accepted))
    );
    assert!(!logged().contains(&"end verifier b".to_owned()));
    assert_isolated(&view("a"), None);
    fs::write(marker("release-verify-b"), "").unwrap();
    assert!(process.wait(), "the other scheduler failed");
    let store = fx.store();
    let generation = report.finished[0].work.generation;
    assert_eq!(
        store
            .accepted_source("src/a.rs")
            .unwrap()
            .unwrap()
            .generation,
        Some(generation)
    );
    assert_eq!(
        store
            .accepted_source("src/b.rs")
            .unwrap()
            .unwrap()
            .generation,
        None
    );
    assert_eq!(fx.read("src/b.rs"), "// b was here\n");
}

fn a_fresh_planner_retries_stopped_work_from_canonical_state() {
    let fx = Fixture::new(
        1,
        &[
            ("broken", "Change a exec=fail", &["src/a.rs"], &[]),
            ("rejected", "Change b verify=fail", &["src/b.rs"], &[]),
            ("after", "Change c", &["src/c.rs"], &["broken", "rejected"]),
        ],
    );
    fx.run();
    // Nothing reruns on the scheduler's account.
    assert!(fx.run().finished.is_empty());
    assert_eq!(launched(), ["broken", "rejected"]);

    fs::write(marker("planner-script"), "auto").unwrap();
    let Planned::Replanned { replan, .. } = fx.replan() else {
        panic!("the replan applies");
    };
    // Its input: canonical feedback, with provider claims marked as such.
    let input = planner_input(0);
    let tasks = &input["plan"]["tasks"];
    assert_eq!(input["intent"]["objective"], "Improve the demo");
    let broken = &tasks[0]["generations"][0];
    assert_eq!(broken["pipeline"], "execution_failed");
    assert_eq!(broken["execution"]["outcome"], "reported_failed");
    assert_eq!(
        broken["execution"]["claimed_by_executor"]["reported"],
        "failed"
    );
    let rejected = &tasks[1]["generations"][0];
    assert_eq!(rejected["pipeline"], "verification_failed");
    let blockers = &rejected["verifications"][0]["claimed_by_verifier"]["blockers"];
    assert_eq!(blockers[0]["summary"], "wrong");
    assert_eq!(tasks[2]["status"], "waiting_for_dependencies");
    // A verifier failure is not an invocation failure.
    let invocation = &rejected["verifications"][0]["invocation"];
    assert_eq!(invocation["state"], "succeeded");

    let report = fx.run();
    assert_eq!(report.snapshot.condition(), Condition::AllCompleted);
    assert_eq!(
        launched(),
        ["broken", "rejected", "broken", "rejected", "after"]
    );
    let store = fx.store();
    for (task, first) in [
        (fx.tasks[0], GenerationState::Failed),
        (fx.tasks[1], GenerationState::Rejected),
    ] {
        let states: Vec<_> = store
            .generations(task)
            .unwrap()
            .iter()
            .map(|g| g.state)
            .collect();
        assert_eq!(states, [first, GenerationState::Accepted]);
        let authorizations = store.retry_authorizations(task).unwrap();
        assert_eq!(authorizations.len(), 1);
        assert_eq!(authorizations[0].replan, replan);
    }
    assert_eq!(store.task(fx.tasks[0]).unwrap().objective, "Change a");
    assert_eq!(fx.read("src/a.rs"), "// broken was here\n");
    // The planner's action was reconciled with the replan itself.
    let journal = fx.planner_journal();
    assert_eq!(journal.len(), 1);
    assert_eq!(
        (journal[0].0.as_str(), journal[0].1),
        ("planner.replan", Some(ActionOutcome::CompletedAsIntended))
    );
    assert!(fx.run().finished.is_empty());
}

fn failed_replanning_changes_nothing() {
    let fx = Fixture::new(
        1,
        &[
            ("broken", "Change a exec=fail", &["src/a.rs"], &[]),
            ("next", "Change b", &["src/b.rs"], &["broken"]),
        ],
    );
    fx.run();
    let before = fx.replanning_state();
    let one_invalid = json!([
        {"op": "update_task", "task": "broken", "objective": "Change a", "context": null,
         "paths": null},
        {"op": "retry_task", "task": "broken"},
        {"op": "add_task", "task": "extra", "objective": "More", "context": "",
         "paths": [], "depends_on": ["missing"]},
    ]);
    for (script, refused) in [
        ("crash", false),
        ("malformed", false),
        // Outside the replanning protocol: the provider's output is refused.
        (r#"[{"op": "remove_task", "task": "next"}]"#, false),
        (r#"[{"op": "finalize"}]"#, false),
        // Within it, but not allowed: agentctl refuses it.
        (r#"[{"op": "retry_task", "task": "next"}]"#, true),
        (&one_invalid.to_string(), true),
    ] {
        fs::write(marker("planner-script"), script).unwrap();
        match fx.replan() {
            Planned::NoResult(_) if !refused => {}
            Planned::Refused { .. } if refused => {}
            other => panic!("{script}: {other:?}"),
        }
        assert_eq!(fx.replanning_state(), before, "{script}");
        let (action, outcome, evidence) = fx.planner_journal().pop().unwrap();
        assert_eq!(
            (action.as_str(), outcome),
            ("planner.replan", Some(ActionOutcome::Failed))
        );
        let refusal = Evidence::Fact {
            name: "replan.refused".into(),
        };
        assert_eq!(evidence.contains(&refusal), refused, "{script}");
    }
    assert!(fx.run().finished.is_empty());
    assert_eq!(launched(), ["broken"]);
}

fn a_stale_replan_is_never_applied() {
    let _unblock = Unblock(&["release-planner"]);
    let fx = Fixture::new(
        1,
        &[
            ("broken", "Change a exec=fail", &["src/a.rs"], &[]),
            ("next", "Change b", &["src/b.rs"], &["broken"]),
        ],
    );
    fx.run();
    fs::write(marker("planner-script"), "hold auto").unwrap();
    let planned = thread::scope(|scope| {
        let replanning = scope.spawn(|| fx.replan());
        await_marker("planning");
        // Meanwhile another planner's replan applies.
        let mut store = fx.store();
        let basis = store.replan_basis(fx.plan).unwrap();
        let other = [Plan::UpdateTask {
            task: "next".into(),
            objective: None,
            context: Some("Changed meanwhile.".into()),
            paths: None,
        }];
        let applied = planner::apply_replan(&fx.project, &mut store, fx.plan, &basis, &other);
        assert!(matches!(applied.unwrap(), Replan::Applied(_)));
        fs::write(marker("release-planner"), "").unwrap();
        replanning.join().unwrap()
    });
    assert!(matches!(planned, Planned::Stale { .. }), "{planned:?}");
    // Nothing of the stale proposal applied: no retry, no revision.
    let store = fx.store();
    assert!(store.retry_authorizations(fx.tasks[0]).unwrap().is_empty());
    assert_eq!(
        store.task(fx.tasks[0]).unwrap().objective,
        "Change a exec=fail"
    );
    assert_eq!(
        store.task(fx.tasks[1]).unwrap().context,
        "Changed meanwhile."
    );
    assert_eq!(store.replans(fx.plan).unwrap().len(), 1);
    let (_, outcome, evidence) = fx.planner_journal().pop().unwrap();
    assert_eq!(outcome, Some(ActionOutcome::Failed));
    assert!(evidence.contains(&Evidence::Fact {
        name: "replan.stale".into()
    }));
    assert!(fx.run().finished.is_empty());
    assert_eq!(launched(), ["broken"]);
}
