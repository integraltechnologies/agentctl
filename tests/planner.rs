//! Planner protocol tests against a fake planning provider: this test binary
//! itself, copied under a name that makes it act as Claude Code, with the
//! scenario chosen by the configured planner model. They spend no provider
//! tokens.

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

use agentctl::planner::{self, Planned};
use agentctl::project::Project;
use agentctl::runtime::{FailureKind, InvocationState};
use agentctl::state::{HumanIntent, Plan, PlanId, PlanState, Store, Task};
use agentctl::{graph, source};
use serde_json::{Value, json};
use tempfile::TempDir;

const FAKE: &str = "fake-planner";
/// Fixed argument that makes the fake speak the generic protocol.
const GENERIC: &str = "--generic-runtime";
/// Stands for anything a planner might say that must never be recorded.
const PROSE: &str = "PLANNER-PROSE-7d1e";

fn main() -> ExitCode {
    let argv0 = env::args_os().next().unwrap_or_default();
    if Path::new(&argv0).file_stem() == Some(OsStr::new(FAKE)) {
        return fake();
    }
    let tests: &[(&str, fn())] = &[
        (
            "planning_reaches_ready_across_fresh_invocations",
            planning_reaches_ready_across_fresh_invocations,
        ),
        (
            "failed_planning_leaves_the_plan_intact",
            failed_planning_leaves_the_plan_intact,
        ),
        (
            "a_declared_provider_plans_through_the_generic_protocol",
            a_declared_provider_plans_through_the_generic_protocol,
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
    if failed.is_empty() {
        println!("test result: ok");
        ExitCode::SUCCESS
    } else {
        println!("test result: FAILED {failed:?}");
        ExitCode::FAILURE
    }
}

/// Acts as Claude Code planning, following the scenario named by `--model=`.
fn fake() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    let scenario = args
        .iter()
        .find_map(|a| a.strip_prefix("--model="))
        .unwrap_or_default()
        .to_owned();
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input).unwrap();
    let mut input: Value = serde_json::from_str(&input).unwrap();
    let generic = args.iter().any(|a| a == GENERIC);
    let scenario = if generic {
        // The request of the generic protocol, whose input is the task.
        assert_eq!(input["protocol"], 1);
        assert_eq!(input["workspace"], "read_only");
        assert!(input["output_schema"].is_object());
        let request = input.take();
        input = serde_json::from_str(request["input"].as_str().unwrap()).unwrap();
        request["model"].as_str().unwrap().to_owned()
    } else {
        // Nothing is resumed: the input is all a planner knows.
        assert!(args.iter().any(|a| a == "--no-session-persistence"));
        assert!(!args.iter().any(|a| a.starts_with("--resume")));
        scenario
    };
    let add = |task: &str, paths: Value, depends_on: Value| {
        json!({"op": "add_task", "task": task, "objective": format!("Complete {task}"),
               "context": "Keep the public API.", "paths": paths, "depends_on": depends_on})
    };
    let commands = match scenario.as_str() {
        "draft" => json!([
            add("parse", json!(["src/lib.rs"]), json!([])),
            add("check", json!(["src/[check].rs"]), json!(["parse"])),
        ]),
        // Continues from whatever tasks the plan holds.
        "continue" => {
            assert_eq!(input["intent"]["objective"], "Parse configuration once");
            let existing: Vec<&Value> = input["plan"]["tasks"]
                .as_array()
                .unwrap()
                .iter()
                .map(|t| &t["task"])
                .collect();
            assert!(!existing.is_empty(), "planning continues from state");
            json!([add("document", json!(["src/lib.rs"]), json!(existing)), {"op": "finalize"}])
        }
        "late-invalid" => json!([
            add("extra", json!([]), json!([])),
            add("orphan", json!([]), json!(["missing"])),
        ]),
        "outside-roots" => json!([add("escape", json!(["../etc/passwd"]), json!([]))]),
        "malformed" => json!("not a list of commands"),
        "crash" => {
            eprintln!("{PROSE}");
            return ExitCode::from(3);
        }
        "hang" => hang_bounded(),
        other => panic!("unknown scenario `{other}`"),
    };
    if generic {
        let value = json!({"commands": commands, "explanation": PROSE});
        println!("{}", json!({"type": "usage", "input": 3, "output": 2}));
        println!("{}", json!({"type": "result", "value": value}));
        return ExitCode::SUCCESS;
    }
    let init = json!({"type": "system", "subtype": "init", "session_id": "fake-session"});
    let result = json!({"type": "result", "subtype": "success", "is_error": false,
        "session_id": "fake-session", "result": PROSE,
        "structured_output": {"commands": commands, "explanation": PROSE},
        "usage": {"input_tokens": 3, "output_tokens": 2}});
    println!("{init}\n{result}");
    std::io::stdout().flush().unwrap();
    ExitCode::SUCCESS
}

/// The fake planner executable, shared by every test.
fn fake_planner() -> PathBuf {
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

/// A Git repository holding a project whose accepted source is indexed.
struct Fixture {
    _dir: TempDir,
    root: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        Self::configured("claude", "")
    }

    /// A project whose roles use `provider`, with `declarations` appended
    /// to its configuration.
    fn configured(provider: &str, declarations: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let git = Command::new("git")
            .arg("-C")
            .arg(&root)
            .args(["init", "-q"])
            .status()
            .unwrap();
        assert!(git.success());
        let role =
            format!("provider = \"{provider}\"\nmodel = \"draft\"\nreasoning_effort = \"high\"\n");
        let config = format!(
            "[project]\nname = \"demo\"\nversion = \"0.1.0\"\n\n\
             [codegraph]\nroots = [\"src\"]\n\n\
             [agents]\nmax_concurrency = 1\n\n\
             [agents.planner]\n{role}\n[agents.executor]\n{role}\n[agents.verifier]\n{role}\n{declarations}"
        );
        fs::write(root.join("agentctl.toml"), config).unwrap();
        fs::create_dir(root.join("src")).unwrap();
        fs::write(root.join("src/lib.rs"), "pub fn parse() {}\n").unwrap();
        let fx = Self { _dir: dir, root };
        let (project, mut store) = fx.open("draft");
        source::baseline(&project, &mut store).unwrap();
        graph::rust::index(&project, &mut store, "src/lib.rs").unwrap();
        fx
    }

    /// Loads the project afresh, as another agentctl process would, with
    /// the planner running `scenario`.
    fn open(&self, scenario: &str) -> (Project, Store) {
        let mut project = Project::load(&self.root).unwrap();
        project.config.agents.planner.model = scenario.parse().unwrap();
        let store = project.hydrate().unwrap();
        (project, store)
    }

    fn plan(&self, store: &mut Store) -> PlanId {
        store
            .create_plan(&HumanIntent {
                objective: "Parse configuration once".into(),
                constraints: vec!["Keep the public API".into()],
                completion_criteria: vec!["cargo test passes".into()],
            })
            .unwrap()
    }

    fn run(&self, plan: PlanId, scenario: &str) -> Planned {
        let (project, mut store) = self.open(scenario);
        planner::start(&project, &mut store, plan, Some(fake_planner()))
            .unwrap()
            .finish(&project, &mut store)
            .unwrap()
    }
}

/// A plan's canonical record and task DAG.
fn state(store: &Store, plan: PlanId) -> (Plan, Vec<Task>) {
    (store.plan(plan).unwrap(), store.tasks(plan).unwrap())
}

fn planning_reaches_ready_across_fresh_invocations() {
    let fx = Fixture::new();
    let (_, mut store) = fx.open("draft");
    let plan = fx.plan(&mut store);
    let intent = store.plan(plan).unwrap().intent;

    let Planned::Applied {
        ready: false,
        explanation,
        invocation: first,
    } = fx.run(plan, "draft")
    else {
        panic!("the draft applies");
    };
    assert_eq!(
        explanation.as_deref(),
        Some(PROSE),
        "returned to the caller"
    );
    let (current, tasks) = state(&store, plan);
    assert_eq!(current.state, PlanState::Planning);
    let keys: Vec<_> = tasks.iter().map(|t| t.key.as_str()).collect();
    assert_eq!(keys, ["parse", "check"]);
    assert_eq!(tasks[1].scope, ["src/[check].rs"]);

    // A new process and a fresh invocation continue from the store alone.
    drop(store);
    let Planned::Applied {
        ready: true,
        invocation: second,
        ..
    } = fx.run(plan, "continue")
    else {
        panic!("the continuation finalizes");
    };
    let (_, store) = fx.open("continue");
    let (current, tasks) = state(&store, plan);
    assert_eq!((current.state, current.intent), (PlanState::Ready, intent));
    let document = tasks.iter().find(|t| t.key == "document").unwrap();
    assert_eq!(document.depends_on, [tasks[0].id, tasks[1].id]);

    // One logical planner, embodied by two distinct invocations, both of
    // which succeeded; what the planner said is nowhere in canonical state.
    let (a, b) = (
        store.invocation(first).unwrap(),
        store.invocation(second).unwrap(),
    );
    assert_eq!(a.agent, b.agent);
    assert_eq!(store.invocations(a.agent).unwrap().len(), 2);
    let events = store.events_after(0, 1000).unwrap();
    assert!(
        events.iter().all(|e| !e.detail.contains(PROSE)),
        "{events:#?}"
    );

    // A ready plan is not planned again.
    let (project, mut store) = fx.open("draft");
    let refused = planner::start(&project, &mut store, plan, Some(fake_planner()));
    let message = format!("{:#}", refused.err().unwrap());
    assert!(
        message.contains("only a planning plan is planned"),
        "{message}"
    );
    assert_eq!(store.invocations(a.agent).unwrap().len(), 2);
}

/// A provider agentctl was not compiled with, declared in configuration
/// alone, runs the whole planning path.
fn a_declared_provider_plans_through_the_generic_protocol() {
    let fx = Fixture::configured(
        "acme-local",
        &format!(
            "[providers.acme-local]\nadapter = \"generic\"\ncommand = \"acme-agent\"\nargs = [\"{GENERIC}\"]\n"
        ),
    );
    let (_, mut store) = fx.open("draft");
    let plan = fx.plan(&mut store);
    let Planned::Applied { invocation, .. } = fx.run(plan, "draft") else {
        panic!("the draft applies");
    };
    let (_, tasks) = state(&store, plan);
    let keys: Vec<_> = tasks.iter().map(|t| t.key.as_str()).collect();
    assert_eq!(keys, ["parse", "check"]);
    let recorded = store.invocation(invocation).unwrap();
    assert_eq!(recorded.provider, "acme-local");
    assert_eq!(recorded.state, InvocationState::Succeeded);
    assert!(matches!(
        recorded.end.unwrap().usage,
        agentctl::runtime::Usage::ProviderReported(_)
    ));
}

fn failed_planning_leaves_the_plan_intact() {
    let fx = Fixture::new();
    let (_, mut store) = fx.open("draft");
    let plan = fx.plan(&mut store);
    assert!(matches!(fx.run(plan, "draft"), Planned::Applied { .. }));
    let before = state(&store, plan);

    // Succeeded invocations whose commands are refused, as a whole.
    for (scenario, refused) in [
        ("late-invalid", "command 2 (add_task) refused"),
        ("outside-roots", "command 1 (add_task) refused"),
    ] {
        let Planned::Refused { invocation, reason } = fx.run(plan, scenario) else {
            panic!("{scenario} is refused");
        };
        assert!(format!("{reason:#}").contains("was refused"), "{reason:#}");
        assert_eq!(
            store.invocation(invocation).unwrap().state,
            InvocationState::Succeeded,
            "the invocation succeeded even though its commands did not apply"
        );
        let last = store.events_after(0, 1000).unwrap().pop().unwrap();
        assert_eq!(last.kind, "planner.refused");
        assert_eq!(last.detail, format!("invocation {invocation}: {refused}"));
        assert_eq!(state(&store, plan), before, "{scenario}");
    }

    // Invocations that end without a result propose nothing.
    for (scenario, ended, failure) in [
        (
            "malformed",
            InvocationState::Failed,
            Some(FailureKind::MalformedOutput),
        ),
        (
            "crash",
            InvocationState::Failed,
            Some(FailureKind::ExitStatus),
        ),
    ] {
        let Planned::NoResult(outcome) = fx.run(plan, scenario) else {
            panic!("{scenario} has no result");
        };
        assert_eq!((outcome.end.state, outcome.end.failure), (ended, failure));
        assert_eq!(state(&store, plan), before, "{scenario}");
    }

    let (project, mut store) = fx.open("hang");
    let planning = planner::start(&project, &mut store, plan, Some(fake_planner())).unwrap();
    let control = planning.control();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !control.observe().alive() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
    control.cancel();
    let Planned::NoResult(outcome) = planning.finish(&project, &mut store).unwrap() else {
        panic!("a cancelled planner has no result");
    };
    assert_eq!(outcome.end.state, InvocationState::Cancelled);
    assert_eq!(state(&store, plan), before);
    assert_eq!(before.0.state, PlanState::Planning);
}
