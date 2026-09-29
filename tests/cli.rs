//! The agentctl command line, end to end, as a user runs it: `init`, plan
//! creation and planning, `run`, pause, resume, cancellation, interruption
//! and recovery, with no library call standing in for any step. Every
//! provider is a deterministic fake runtime speaking the generic protocol
//! under an arbitrary provider name: this test binary, copied as
//! `fake-runtime`, playing whichever role its instructions name. It spends
//! no provider tokens.
//!
//! Wherever procd can terminate a process tree at all, enforced (Linux in a
//! delegated cgroup, Windows) or best effort (macOS), every run is agentctl
//! as users get it, built without the lifecycle test double (or the binary
//! `AGENTCTL_PRODUCTION_BIN` names), with procd's own evidence: production
//! qualification, and every end is checked to be recorded at exactly the
//! strength the host has. Where procd supports none, agents cannot run,
//! which this proves of the production binary; the other scenarios then run
//! the test build with its lifecycle double, which checks agentctl's own
//! logic and is no evidence about procd.
//!
//! Every fake ends by itself within bounded time, and whatever a test
//! starts is killed and reaped when it ends, however it ends.

use std::env;
use std::ffi::OsStr;
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

const FAKE: &str = "fake-runtime";
/// Where the fakes log and wait, passed on as an environment variable the
/// provider's configuration lets through.
const MARKERS: &str = "AGENTCTL_FAKE_MARKERS";
/// How long a fake holds before it goes on by itself.
const HOLD: Duration = Duration::from_secs(90);

fn main() -> ExitCode {
    let argv0 = env::args_os().next().unwrap_or_default();
    if Path::new(&argv0).file_stem() == Some(OsStr::new(FAKE)) {
        return fake();
    }
    #[cfg(not(unix))]
    {
        println!("test result: ok (the command-line suite runs on Unix hosts)");
        ExitCode::SUCCESS
    }
    #[cfg(unix)]
    unix::main()
}

/// The fake runtime: one generic-protocol request on standard input, one
/// structured result on standard output.
fn fake() -> ExitCode {
    let markers = PathBuf::from(env::var_os(MARKERS).expect("the markers directory"));
    let mut text = String::new();
    std::io::stdin().read_to_string(&mut text).unwrap();
    let request: Value = serde_json::from_str(&text).unwrap();
    let instructions = request["instructions"].as_str().unwrap_or_default();
    let input: Value = serde_json::from_str(request["input"].as_str().unwrap()).unwrap();
    let say = |event: Value| {
        println!("{event}");
        std::io::stdout().flush().unwrap();
    };
    say(json!({"type": "session", "id": "fake-session"}));
    let value = if instructions.starts_with("You are the planning agent") {
        if instructions.contains("replanning a plan") {
            replan(&markers, &input)
        } else {
            log(&markers, "start planner");
            match fs::read_to_string(markers.join("plan.json")) {
                Ok(plan) => json!({
                    "commands": serde_json::from_str::<Value>(&plan).unwrap(),
                    "explanation": "planned",
                }),
                Err(_) => {
                    say(json!({"type": "error", "message": "no plan to propose"}));
                    return ExitCode::FAILURE;
                }
            }
        }
    } else if instructions.starts_with("You are the integration verifier") {
        log(&markers, "start integration");
        verdict(true)
    } else if instructions.starts_with("You are a verifier") {
        let key = input["task"]["key"].as_str().unwrap();
        log(&markers, &format!("start verifier {key}"));
        let objective = input["task"]["objective"].as_str().unwrap();
        verdict(!objective.split_whitespace().any(|t| t == "verify=fail"))
    } else if instructions.starts_with("You are an executor") {
        execute(&markers, &input)
    } else {
        say(json!({"type": "error", "message": "no role recognized"}));
        return ExitCode::FAILURE;
    };
    say(json!({"type": "usage", "input": 3, "output": 2}));
    say(json!({"type": "result", "value": value}));
    ExitCode::SUCCESS
}

fn log(markers: &Path, line: &str) {
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(markers.join("log"))
        .unwrap();
    // One write per line, so concurrent fakes never interleave within one.
    file.write_all(format!("{line}\n").as_bytes()).unwrap();
}

fn verdict(pass: bool) -> Value {
    let checked = json!([{"check": "read", "command": null,
                          "outcome": if pass { "passed" } else { "failed" }, "evidence": "e"}]);
    match pass {
        true => json!({"verdict": "pass", "checked": checked, "blockers": [], "non_blocking": []}),
        false => json!({"verdict": "fail", "checked": checked, "non_blocking": [],
            "blockers": [{"id": "b1", "summary": "wrong", "paths": [], "evidence": "e",
                          "location": null}]}),
    }
}

/// Plays an executor as its task's objective says: `tree` starts a
/// descendant in a process group of its own, `hold` waits to be let go,
/// `meet` waits for another `meet` executor to arrive too.
// The descendant is left running on purpose: ending it is procd's.
#[allow(clippy::zombie_processes)]
fn execute(markers: &Path, input: &Value) -> Value {
    let key = input["task"]["key"].as_str().unwrap().to_owned();
    let objective = input["task"]["objective"].as_str().unwrap().to_owned();
    let says = |token: &str| objective.split_whitespace().any(|t| t == token);
    log(markers, &format!("start executor {key}"));
    fs::write(
        markers.join(format!("pid-{key}")),
        std::process::id().to_string(),
    )
    .unwrap();
    #[cfg(unix)]
    if says("tree") {
        use std::os::unix::process::CommandExt;
        use std::process::{Command, Stdio};
        let child = Command::new("sleep")
            .arg("60")
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        fs::write(
            markers.join(format!("descendant-{key}")),
            child.id().to_string(),
        )
        .unwrap();
    }
    if says("hold") {
        fs::write(markers.join(format!("started-{key}")), "").unwrap();
        let deadline = Instant::now() + HOLD;
        while !markers.join(format!("release-{key}")).exists() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }
    }
    if says("meet") {
        fs::write(markers.join(format!("arrived-{key}")), "").unwrap();
        let deadline = Instant::now() + Duration::from_secs(30);
        let arrived = || {
            fs::read_dir(markers)
                .unwrap()
                .filter(|e| {
                    let name = e.as_ref().unwrap().file_name();
                    name.to_string_lossy().starts_with("arrived-")
                })
                .count()
        };
        while arrived() < 2 && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }
        if arrived() >= 2 {
            log(markers, &format!("met {key}"));
        }
    }
    let paths = input["authority"]["mutable_paths"].as_array().unwrap();
    for path in paths {
        let path = Path::new(path.as_str().unwrap());
        let name = path
            .file_stem()
            .unwrap()
            .to_str()
            .unwrap()
            .replace('-', "_");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, format!("pub fn {name}_{key}() {{}}\n")).unwrap();
    }
    log(markers, &format!("end executor {key}"));
    let status = if says("exec=fail") {
        "failed"
    } else {
        "succeeded"
    };
    json!({"status": status, "summary": "done", "modified_paths": paths})
}

/// Replans from canonical feedback alone: every stopped task is retried,
/// without the tokens that held or failed it; once every task completed,
/// completion is proposed.
fn replan(markers: &Path, input: &Value) -> Value {
    log(markers, "start replanner");
    let tasks = input["plan"]["tasks"].as_array().unwrap();
    let mut commands = Vec::new();
    for task in tasks.iter().filter(|t| t["status"] == "stopped") {
        let objective: Vec<&str> = task["objective"]
            .as_str()
            .unwrap()
            .split_whitespace()
            .filter(|t| !matches!(*t, "hold" | "tree" | "exec=fail" | "verify=fail"))
            .collect();
        commands.push(json!({"op": "update_task", "task": task["task"],
            "objective": objective.join(" "), "context": null, "paths": null}));
        commands.push(json!({"op": "retry_task", "task": task["task"]}));
    }
    if commands.is_empty() && tasks.iter().all(|t| t["status"] == "completed") {
        commands.push(json!({"op": "propose_completion"}));
    }
    json!({"commands": commands, "explanation": "replanned"})
}

#[cfg(unix)]
mod unix {
    use super::*;

    use std::fs::File;
    use std::panic;
    use std::process::{Child, Command, ExitStatus, Stdio};
    use std::sync::OnceLock;

    use agentctl::graph::Freshness;
    use agentctl::platform::Level;
    use agentctl::runtime::InvocationState;
    use agentctl::state::{
        ExecutionOutcome, ExecutionStatus, GenerationId, PipelineOutcome, PlanId, PlanState, Store,
        TaskId, TaskStatus, Termination,
    };
    use tempfile::TempDir;

    /// How long any command may take.
    const COMMAND_LIMIT: Duration = Duration::from_secs(240);

    pub(super) fn main() -> ExitCode {
        let tests: &[(&str, fn())] = &[
            (
                "ends_are_recorded_at_the_host_s_strength",
                ends_are_recorded_at_the_host_s_strength,
            ),
            (
                "a_plan_is_created_planned_run_and_completed",
                a_plan_is_created_planned_run_and_completed,
            ),
            (
                "a_failed_planner_leaves_its_plan_planning",
                a_failed_planner_leaves_its_plan_planning,
            ),
            (
                "working_tree_drift_holds_its_task_back",
                working_tree_drift_holds_its_task_back,
            ),
            (
                "pause_holds_new_work_and_resume_continues",
                pause_holds_new_work_and_resume_continues,
            ),
            (
                "cancel_ends_live_work_and_keeps_its_ownership",
                cancel_ends_live_work_and_keeps_its_ownership,
            ),
            (
                "ctrl_c_ends_live_work_in_order",
                ctrl_c_ends_live_work_in_order,
            ),
            (
                "sigterm_ends_live_work_in_order",
                sigterm_ends_live_work_in_order,
            ),
            (
                "abrupt_controller_death_fails_closed",
                abrupt_controller_death_fails_closed,
            ),
        ];
        let filters: Vec<String> = env::args()
            .skip(1)
            .filter(|a| !a.starts_with('-'))
            .collect();
        // Where qualification is required, never fall back to the double.
        if env::var_os("AGENTCTL_REQUIRE_PRODUCTION").is_some() {
            assert!(
                host_enforces(),
                "production qualification is required, and procd does not enforce \
                 process-tree termination here: {:?}",
                agentctl::procd::capabilities()
            );
        }
        println!("{}", agentctl().describe());
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

    /// How strongly procd terminates a process tree on this host.
    fn host_level() -> Level {
        match agentctl::procd::capabilities() {
            Ok(caps) => caps.process_tree_termination,
            Err(_) => Level::Unsupported,
        }
    }

    fn host_enforces() -> bool {
        host_level() == Level::Enforced
    }

    /// How every end is recorded here, exactly: never stronger than the
    /// host.
    fn expected_termination() -> Termination {
        match host_level() {
            Level::Enforced => Termination::Enforced,
            _ => Termination::BestEffort,
        }
    }

    /// Whether procd can reacquire a domain after its controller died.
    fn host_recovers() -> bool {
        matches!(agentctl::procd::capabilities(),
            Ok(caps) if caps.safe_recovery == Level::Enforced)
    }

    /// agentctl as users get it: built without the test double, as a
    /// dependency of nothing, in a target directory of its own.
    fn production() -> PathBuf {
        static BUILT: OnceLock<PathBuf> = OnceLock::new();
        BUILT
            .get_or_init(|| {
                if let Some(bin) = env::var_os("AGENTCTL_PRODUCTION_BIN") {
                    return PathBuf::from(bin);
                }
                let target = Path::new(env!("CARGO_TARGET_TMPDIR")).join("production");
                let cargo = Path::new(env!("CARGO"));
                let mut build = Command::new(cargo);
                // The compiler of the toolchain that built this test, never
                // one a dependency's own toolchain file names.
                let rustc = cargo.with_file_name(format!("rustc{}", env::consts::EXE_SUFFIX));
                if rustc.is_file() {
                    build.env("RUSTC", rustc);
                }
                let status = build
                    .args(["build", "--locked", "--bins", "--manifest-path"])
                    .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml"))
                    .arg("--target-dir")
                    .arg(&target)
                    .status()
                    .unwrap();
                assert!(status.success(), "the production build failed");
                target.join("debug").join("agentctl")
            })
            .clone()
    }

    /// The agentctl the scenarios run.
    struct Agentctl {
        bin: PathBuf,
        /// Whether it is the test build, with its lifecycle double.
        double: bool,
    }

    impl Agentctl {
        fn describe(&self) -> String {
            match self.double {
                false => format!(
                    "cli: production agentctl {} with procd's own evidence",
                    self.bin.display()
                ),
                true => "cli: procd supports no process-tree termination here, so the \
                         production binary is checked to refuse, and the other scenarios run \
                         the test build with its lifecycle double: agentctl's logic only, no \
                         evidence about procd"
                    .to_owned(),
            }
        }
    }

    fn agentctl() -> &'static Agentctl {
        static CHOSEN: OnceLock<Agentctl> = OnceLock::new();
        CHOSEN.get_or_init(|| match host_level() != Level::Unsupported {
            true => Agentctl {
                bin: production(),
                double: false,
            },
            false => Agentctl {
                bin: PathBuf::from(env!("CARGO_BIN_EXE_agentctl")),
                double: true,
            },
        })
    }

    /// The fake runtime executable, shared by every test.
    fn fake_runtime() -> PathBuf {
        static COPY: OnceLock<(TempDir, PathBuf)> = OnceLock::new();
        COPY.get_or_init(|| {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join(FAKE);
            fs::copy(env::current_exe().unwrap(), &path).unwrap();
            (dir, path)
        })
        .1
        .clone()
    }

    fn alive(pid: u32) -> bool {
        // SAFETY: signal 0 only checks that the process exists.
        unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
    }

    fn assert_gone_soon(pid: u32, what: &str) {
        let deadline = Instant::now() + Duration::from_secs(15);
        while alive(pid) {
            assert!(Instant::now() < deadline, "{what} {pid} still runs");
            thread::sleep(Duration::from_millis(20));
        }
    }

    /// What one command did.
    struct Ran {
        status: ExitStatus,
        stdout: String,
        stderr: String,
    }

    impl Ran {
        fn ok(self) -> Self {
            assert!(self.status.success(), "{}\n{}", self.stdout, self.stderr);
            self
        }

        fn failed(self) -> Self {
            assert!(!self.status.success(), "{}\n{}", self.stdout, self.stderr);
            self
        }
    }

    /// An agentctl command running in a process group of its own, as a
    /// terminal's foreground job does. Dropped while it still runs, as when
    /// its test failed, it is killed and reaped, and every fake of its
    /// fixture killed.
    struct Background {
        child: Child,
        out: PathBuf,
        err: PathBuf,
        markers: PathBuf,
    }

    impl Background {
        fn pid(&self) -> u32 {
            self.child.id()
        }

        fn signal(&self, signal: libc::c_int) {
            // SAFETY: the pid is this test's own child's.
            assert_eq!(unsafe { libc::kill(self.pid() as libc::pid_t, signal) }, 0);
        }

        /// Signals the whole process group, as a terminal's Ctrl-C does.
        fn signal_group(&self, signal: libc::c_int) {
            // SAFETY: the group is this test's own child's.
            assert_eq!(
                unsafe { libc::killpg(self.pid() as libc::pid_t, signal) },
                0
            );
        }

        /// Waits, within `limit`, for the command to end.
        fn wait(&mut self, limit: Duration) -> Ran {
            let deadline = Instant::now() + limit;
            let status = loop {
                if let Some(status) = self.child.try_wait().unwrap() {
                    break status;
                }
                assert!(Instant::now() < deadline, "the command outlived {limit:?}");
                thread::sleep(Duration::from_millis(20));
            };
            Ran {
                status,
                stdout: fs::read_to_string(&self.out).unwrap_or_default(),
                stderr: fs::read_to_string(&self.err).unwrap_or_default(),
            }
        }
    }

    impl Drop for Background {
        fn drop(&mut self) {
            if self.child.try_wait().ok().flatten().is_none() {
                // SAFETY: the pid is this test's own child's.
                unsafe { libc::kill(self.pid() as libc::pid_t, libc::SIGKILL) };
                let _ = self.child.wait();
                kill_fakes(&self.markers);
            }
        }
    }

    /// Kills every fake and descendant whose pid the markers hold: only
    /// ever what a test itself started.
    fn kill_fakes(markers: &Path) {
        let Ok(entries) = fs::read_dir(markers) else {
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !(name.starts_with("pid-") || name.starts_with("descendant-")) {
                continue;
            }
            if let Ok(pid) = fs::read_to_string(entry.path())
                .unwrap_or_default()
                .parse::<i32>()
            {
                // SAFETY: the pid is of a fake, or its descendant, this test
                // started, which the marker names.
                unsafe { libc::kill(pid, libc::SIGKILL) };
            }
        }
    }

    /// A Git repository holding an agentctl project whose every role is the
    /// fake runtime under an arbitrary provider name, initialized with
    /// `agentctl init`, and whose planner proposes `tasks`
    /// `(key, objective, paths, depends_on)`.
    struct Fixture {
        _dir: TempDir,
        root: PathBuf,
        markers: TempDir,
    }

    impl Fixture {
        fn new(max_concurrency: u32, tasks: &[(&str, &str, &[&str], &[&str])]) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().canonicalize().unwrap();
            let markers = tempfile::tempdir().unwrap();
            let fx = Self {
                _dir: dir,
                root,
                markers,
            };
            let git = Command::new("git")
                .arg("-C")
                .arg(&fx.root)
                .args(["init", "-q"])
                .status()
                .unwrap();
            assert!(git.success());
            let role = "provider = \"house-runtime\"\nmodel = \"fake\"\n\
                        reasoning_effort = \"high\"\n";
            let fake = json!(fake_runtime().to_str().unwrap());
            let config = format!(
                "[project]\nname = \"demo\"\nversion = \"0.1.0\"\n\n\
                 [codegraph]\nroots = [\"src\"]\n\n\
                 [agents]\nmax_concurrency = {max_concurrency}\n\n\
                 [agents.planner]\n{role}\n[agents.executor]\n{role}\n[agents.verifier]\n{role}\n\
                 [providers.house-runtime]\nadapter = \"generic\"\ncommand = {fake}\n\
                 env = [\"{MARKERS}\"]\n"
            );
            fx.write("agentctl.toml", &config);
            fx.write("README.md", "# demo\n");
            for name in ["a", "b", "c", "d"] {
                fx.write(
                    &format!("src/{name}.rs"),
                    &format!("pub fn {name}() {{}}\n"),
                );
            }
            let init = fx.agentctl_with(&["init"], "y\n").ok();
            assert!(
                init.stdout.contains("Indexed 4 source files"),
                "{}",
                init.stdout
            );
            fx.propose(tasks);
            fx
        }

        /// Makes the planner propose `tasks`, then finalize.
        fn propose(&self, tasks: &[(&str, &str, &[&str], &[&str])]) {
            let mut commands: Vec<Value> = tasks
                .iter()
                .map(|(key, objective, paths, depends_on)| {
                    json!({"op": "add_task", "task": key, "objective": objective,
                           "context": "", "paths": paths, "depends_on": depends_on})
                })
                .collect();
            commands.push(json!({"op": "finalize"}));
            fs::write(self.marker("plan.json"), Value::from(commands).to_string()).unwrap();
        }

        fn write(&self, path: &str, text: &str) {
            let path = self.root.join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, text).unwrap();
        }

        fn read(&self, path: &str) -> String {
            fs::read_to_string(self.root.join(path)).unwrap()
        }

        fn marker(&self, name: &str) -> PathBuf {
            self.markers.path().join(name)
        }

        fn await_marker(&self, name: &str) -> String {
            let deadline = Instant::now() + Duration::from_secs(60);
            loop {
                if let Ok(text) = fs::read_to_string(self.marker(name)) {
                    return text;
                }
                assert!(Instant::now() < deadline, "{name} never appeared");
                thread::sleep(Duration::from_millis(20));
            }
        }

        fn pid(&self, name: &str) -> u32 {
            let deadline = Instant::now() + Duration::from_secs(60);
            loop {
                if let Ok(pid) = self.await_marker(name).trim().parse() {
                    return pid;
                }
                assert!(Instant::now() < deadline, "{name} holds no pid");
                thread::sleep(Duration::from_millis(20));
            }
        }

        fn release(&self, key: &str) {
            fs::write(self.marker(&format!("release-{key}")), "").unwrap();
        }

        fn logged(&self) -> Vec<String> {
            fs::read_to_string(self.marker("log"))
                .unwrap_or_default()
                .lines()
                .map(str::to_owned)
                .collect()
        }

        fn store(&self) -> Store {
            Store::open(&self.root.join(".agentctl").join("state.db")).unwrap()
        }

        fn command(&self, bin: &Path, args: &[&str]) -> Command {
            let mut command = Command::new(bin);
            command
                .args(args)
                .current_dir(&self.root)
                .env(MARKERS, self.markers.path())
                .env_remove("AGENTCTL_TEST_EVIDENCE");
            command
        }

        /// Runs `agentctl args` to its end, within bounded time.
        fn agentctl(&self, args: &[&str]) -> Ran {
            self.agentctl_with(args, "")
        }

        fn agentctl_with(&self, args: &[&str], input: &str) -> Ran {
            self.run_bin(&agentctl().bin, args, input)
        }

        fn run_bin(&self, bin: &Path, args: &[&str], input: &str) -> Ran {
            let mut background = self.spawn_bin(bin, args, Stdio::piped());
            let mut stdin = background.child.stdin.take().unwrap();
            stdin.write_all(input.as_bytes()).unwrap();
            drop(stdin);
            background.wait(COMMAND_LIMIT)
        }

        /// Starts `agentctl args` in a process group of its own.
        fn spawn(&self, args: &[&str]) -> Background {
            self.spawn_bin(&agentctl().bin, args, Stdio::null())
        }

        fn spawn_bin(&self, bin: &Path, args: &[&str], stdin: Stdio) -> Background {
            use std::os::unix::process::CommandExt;
            let n = (0..)
                .find(|n| !self.marker(&format!("out-{n}")).exists())
                .unwrap();
            let out = self.marker(&format!("out-{n}"));
            let err = self.marker(&format!("err-{n}"));
            let child = self
                .command(bin, args)
                .process_group(0)
                .stdin(stdin)
                .stdout(File::create(&out).unwrap())
                .stderr(File::create(&err).unwrap())
                .spawn()
                .unwrap();
            Background {
                child,
                out,
                err,
                markers: self.markers.path().to_path_buf(),
            }
        }

        fn tasks(&self, plan: PlanId) -> Vec<TaskId> {
            self.store()
                .tasks(plan)
                .unwrap()
                .iter()
                .map(|t| t.id)
                .collect()
        }

        fn status(&self, plan: PlanId, task: TaskId) -> TaskStatus {
            let snapshot = self
                .store()
                .snapshot(plan, std::num::NonZeroU32::MIN)
                .unwrap();
            snapshot.status(task).unwrap().clone()
        }

        fn state(&self, plan: PlanId) -> PlanState {
            self.store().plan(plan).unwrap().state
        }

        /// The generations of `task`, in order.
        fn generations(&self, task: TaskId) -> Vec<GenerationId> {
            self.store()
                .generations(task)
                .unwrap()
                .iter()
                .map(|g| g.id)
                .collect()
        }

        /// How the executor invocation of `generation` stands, and how the
        /// end of its processes was established, if it ended.
        fn executor_end(&self, generation: GenerationId) -> (InvocationState, Option<Termination>) {
            let store = self.store();
            let execution = store.execution(generation).unwrap().unwrap();
            let invocations = store.invocations(execution.agent).unwrap();
            let last = invocations.last().unwrap();
            (last.state, last.termination)
        }

        /// Replans with the fake, from canonical feedback alone, retrying
        /// every stopped task.
        fn retry_stopped(&self, plan: PlanId) {
            let out = self.agentctl(&["plan", "update", &plan.to_string()]).ok();
            assert!(out.stdout.contains("applied"), "{}", out.stdout);
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            kill_fakes(self.markers.path());
        }
    }

    const ONE: &str = "1";
    fn plan_one() -> PlanId {
        ONE.parse().unwrap()
    }

    /// RB-1: agentctl as users get it runs agents wherever procd can end
    /// their whole process tree, and records each end at exactly the
    /// strength procd establishes here, never stronger; where procd can end
    /// none, it launches nothing and records nothing of a launch.
    fn ends_are_recorded_at_the_host_s_strength() {
        let fx = Fixture::new(1, &[("only", "Change a", &["src/a.rs"], &[])]);
        let bin = production();
        let created = fx.run_bin(&bin, &["plan", "create", "Improve the demo"], "");
        let mut store = fx.store();
        let agent = store.planner(plan_one()).unwrap();
        if host_level() == Level::Unsupported {
            let created = created.failed();
            assert!(
                created.stderr.contains("this host cannot run agents"),
                "{}",
                created.stderr
            );
            assert!(fx.logged().is_empty());
            assert_eq!(store.plan(plan_one()).unwrap().state, PlanState::Planning);
            assert!(store.invocations(agent).unwrap().is_empty());
            return;
        }
        created.ok();
        assert_eq!(store.plan(plan_one()).unwrap().state, PlanState::Ready);
        let planned = store.invocations(agent).unwrap();
        assert_eq!(planned.len(), 1);
        assert_eq!(planned[0].state, InvocationState::Succeeded);
        assert_eq!(planned[0].termination, Some(expected_termination()));
        drop(store);
        fx.run_bin(&bin, &["run", ONE], "").ok();
        let tasks = fx.tasks(plan_one());
        let generation = fx.generations(tasks[0])[0];
        assert_eq!(fx.status(plan_one(), tasks[0]), TaskStatus::Completed);
        let (state, termination) = fx.executor_end(generation);
        assert_eq!(state, InvocationState::Succeeded);
        assert_eq!(termination, Some(expected_termination()));
        let logs = fx.agentctl(&["logs", "--kind", "invocation.ended"]).ok();
        let recorded = format!("termination {}", expected_termination());
        assert!(logs.stdout.contains(&recorded), "{}", logs.stdout);
    }

    /// RB-3 and the whole system: a plan is created from human intent and
    /// planned, then runs concurrent and dependent tasks through executors,
    /// verifiers, acceptance and CodeGraph refresh, and completes by its
    /// integration verification's pass, all through the command line.
    fn a_plan_is_created_planned_run_and_completed() {
        let fx = Fixture::new(
            2,
            &[
                ("alpha", "Change a meet", &["src/a.rs"], &[]),
                ("beta", "Change b meet", &["src/b.rs"], &[]),
                (
                    "gamma",
                    "Change c and add e",
                    &["src/c.rs", "src/e.rs"],
                    &["alpha", "beta"],
                ),
            ],
        );
        let created = fx
            .agentctl(&[
                "plan",
                "create",
                "Make the demo better",
                "--constraint",
                "Keep it building",
                "--criterion",
                "Every function is renamed",
            ])
            .ok();
        assert!(
            created.stdout.contains("plan 1 created"),
            "{}",
            created.stdout
        );
        assert!(
            created.stdout.contains("plan 1: ready"),
            "{}",
            created.stdout
        );
        let status = fx.agentctl(&["status"]).ok();
        assert!(status.stdout.contains("plan 1 ready"), "{}", status.stdout);
        let plan = fx.store().plan(plan_one()).unwrap();
        assert_eq!(plan.intent.constraints, ["Keep it building"]);
        assert_eq!(
            plan.intent.completion_criteria,
            ["Every function is renamed"]
        );

        let ran = fx.agentctl(&["run", ONE]).ok();
        assert!(ran.stdout.contains("AllCompleted"), "{}", ran.stdout);
        let logged = fx.logged();
        // Independent tasks overlapped; the dependent one ran after both.
        assert!(logged.contains(&"met alpha".to_owned()), "{logged:?}");
        assert!(logged.contains(&"met beta".to_owned()), "{logged:?}");
        let start = |line: &str| logged.iter().position(|l| l == line).unwrap();
        assert!(start("start executor gamma") > start("start verifier alpha"));
        assert!(start("start executor gamma") > start("start verifier beta"));
        let store = fx.store();
        for (task, path, name) in [
            (0, "src/a.rs", "a_alpha"),
            (1, "src/b.rs", "b_beta"),
            (2, "src/c.rs", "c_gamma"),
            (2, "src/e.rs", "e_gamma"),
        ] {
            let tasks = fx.tasks(plan_one());
            assert_eq!(fx.status(plan_one(), tasks[task]), TaskStatus::Completed);
            let accepted = store.accepted_source(path).unwrap().unwrap();
            assert!(accepted.generation.is_some(), "{path}");
            assert_eq!(fx.read(path), format!("pub fn {name}() {{}}\n"));
            let Freshness::Current(entities) = store.entities(path).unwrap() else {
                panic!("{path}'s graph is not current");
            };
            assert!(entities.iter().any(|e| e.id.symbol == name), "{entities:?}");
        }
        drop(store);

        // Its planner proposes completion, which its integration verifier
        // then passes.
        let updated = fx.agentctl(&["plan", "update", ONE]).ok();
        assert!(
            updated.stdout.contains("proposed completion"),
            "{}",
            updated.stdout
        );
        assert!(
            updated.stdout.contains("plan 1: completed"),
            "{}",
            updated.stdout
        );
        assert_eq!(fx.state(plan_one()), PlanState::Completed);
        let status = fx.agentctl(&["status"]).ok();
        assert!(
            status.stdout.contains("plan 1 completed"),
            "{}",
            status.stdout
        );
        assert!(fx.store().unresolved(None).unwrap().is_empty());
        let recovered = fx.agentctl(&["recover"]).ok();
        assert!(recovered.stdout.contains("nothing to recover"));
    }

    /// A planner that fails changes nothing: the plan stays planning, and
    /// planning it again goes on from there.
    fn a_failed_planner_leaves_its_plan_planning() {
        let fx = Fixture::new(1, &[("only", "Change a", &["src/a.rs"], &[])]);
        fs::remove_file(fx.marker("plan.json")).unwrap();
        let created = fx
            .agentctl(&["plan", "create", "Improve the demo"])
            .failed();
        assert!(
            created.stdout.contains("plan 1 created"),
            "{}",
            created.stdout
        );
        assert!(
            created.stderr.contains("still planning"),
            "{}",
            created.stderr
        );
        assert_eq!(fx.state(plan_one()), PlanState::Planning);
        assert!(fx.store().tasks(plan_one()).unwrap().is_empty());
        fx.propose(&[("only", "Change a", &["src/a.rs"], &[])]);
        let planned = fx.agentctl(&["plan", "update", ONE]).ok();
        assert!(
            planned.stdout.contains("plan 1: ready"),
            "{}",
            planned.stdout
        );
        fx.agentctl(&["run", ONE]).ok();
        assert_eq!(fx.read("src/a.rs"), "pub fn a_only() {}\n");
    }

    /// RB-5 through the command line: human work in progress holds its task
    /// back, untouched, until the human reconciles it.
    fn working_tree_drift_holds_its_task_back() {
        let fx = Fixture::new(
            2,
            &[
                ("dirty", "Change a", &["src/a.rs"], &[]),
                ("clean", "Change b", &["src/b.rs"], &[]),
            ],
        );
        fx.agentctl(&["plan", "create", "Improve the demo"]).ok();
        fx.write("src/a.rs", "pub fn a() { /* human work in progress */ }\n");
        let ran = fx.agentctl(&["run", ONE]).ok();
        assert!(ran.stdout.contains("not claimed"), "{}", ran.stdout);
        assert!(ran.stdout.contains("src/a.rs"), "{}", ran.stdout);
        let tasks = fx.tasks(plan_one());
        assert!(fx.generations(tasks[0]).is_empty());
        assert_eq!(fx.status(plan_one(), tasks[1]), TaskStatus::Completed);
        assert_eq!(
            fx.read("src/a.rs"),
            "pub fn a() { /* human work in progress */ }\n"
        );
        fx.write("src/a.rs", "pub fn a() {}\n");
        fx.agentctl(&["run", ONE]).ok();
        assert_eq!(fx.status(plan_one(), tasks[0]), TaskStatus::Completed);
    }

    /// RB-4: a paused plan claims nothing more while what runs ends as it
    /// would; resuming it lets it continue, its accepted work intact.
    fn pause_holds_new_work_and_resume_continues() {
        let fx = Fixture::new(
            1,
            &[
                ("first", "Change a hold", &["src/a.rs"], &[]),
                ("second", "Change b", &["src/b.rs"], &[]),
                ("third", "Change c", &["src/c.rs"], &["first"]),
            ],
        );
        fx.agentctl(&["plan", "create", "Improve the demo"]).ok();
        let mut run = fx.spawn(&["run", ONE]);
        fx.await_marker("started-first");
        let paused = fx.agentctl(&["plan", "pause", ONE]).ok();
        assert!(
            paused.stdout.contains("1 pipelines already running"),
            "{}",
            paused.stdout
        );
        assert_eq!(fx.state(plan_one()), PlanState::Paused);
        fx.release("first");
        let ran = run.wait(COMMAND_LIMIT);
        assert!(ran.status.success(), "{}\n{}", ran.stdout, ran.stderr);
        let tasks = fx.tasks(plan_one());
        assert_eq!(fx.status(plan_one(), tasks[0]), TaskStatus::Completed);
        assert_eq!(fx.status(plan_one(), tasks[1]), TaskStatus::Eligible);
        assert!(fx.generations(tasks[1]).is_empty());
        let refused = fx.agentctl(&["run", ONE]).failed();
        assert!(refused.stderr.contains("paused"), "{}", refused.stderr);
        fx.agentctl(&["plan", "resume", ONE]).ok();
        assert_eq!(fx.state(plan_one()), PlanState::Running);
        fx.agentctl(&["run", ONE]).ok();
        for &task in &tasks {
            assert_eq!(fx.status(plan_one(), task), TaskStatus::Completed);
        }
        assert_eq!(fx.read("src/a.rs"), "pub fn a_first() {}\n");
    }

    /// RB-4 and RB-2: cancelling a plan reaches the live executor through
    /// the process running it, whose whole process tree ends; its attempt
    /// stops short, owning its paths until its planner decides, and only
    /// that planner's retry continues it.
    fn cancel_ends_live_work_and_keeps_its_ownership() {
        let fx = Fixture::new(
            2,
            &[
                ("done", "Change a", &["src/a.rs"], &[]),
                ("long", "Change b hold tree", &["src/b.rs"], &["done"]),
            ],
        );
        fx.agentctl(&["plan", "create", "Improve the demo"]).ok();
        let mut run = fx.spawn(&["run", ONE]);
        fx.await_marker("started-long");
        let provider = fx.pid("pid-long");
        let descendant = fx.pid("descendant-long");
        assert!(alive(provider) && alive(descendant));
        let cancelled = fx.agentctl(&["plan", "cancel", ONE]).ok();
        assert!(
            cancelled.stdout.contains(": cancelled"),
            "{}",
            cancelled.stdout
        );
        assert!(
            cancelled.stdout.contains("plan 1: paused"),
            "{}",
            cancelled.stdout
        );
        assert_gone_soon(provider, "the provider");
        assert_gone_soon(descendant, "its detached descendant");
        let ran = run.wait(COMMAND_LIMIT);
        assert!(ran.status.success(), "{}\n{}", ran.stdout, ran.stderr);

        let tasks = fx.tasks(plan_one());
        let generation = fx.generations(tasks[1])[0];
        assert_eq!(
            fx.executor_end(generation),
            (InvocationState::Cancelled, Some(expected_termination()))
        );
        assert_eq!(
            fx.status(plan_one(), tasks[1]),
            TaskStatus::Stopped {
                generation,
                outcome: PipelineOutcome::ExecutionFailed,
            }
        );
        let store = fx.store();
        let ExecutionStatus::Captured(capture) =
            store.execution(generation).unwrap().unwrap().status
        else {
            panic!("the execution was not captured");
        };
        assert_eq!(capture.outcome, ExecutionOutcome::InvocationFailed);
        // Still owned, and nothing of the attempt reached the project.
        assert_eq!(store.owned_paths(generation).unwrap(), ["src/b.rs"]);
        assert_eq!(fx.read("src/b.rs"), "pub fn b() {}\n");
        // Accepted work stays.
        assert_eq!(fx.read("src/a.rs"), "pub fn a_done() {}\n");
        assert_eq!(fx.status(plan_one(), tasks[0]), TaskStatus::Completed);
        assert!(store.unresolved(None).unwrap().is_empty());
        drop(store);

        fx.retry_stopped(plan_one());
        assert!(fx.store().owned_paths(generation).unwrap().is_empty());
        fx.agentctl(&["plan", "resume", ONE]).ok();
        fx.agentctl(&["run", ONE]).ok();
        assert_eq!(fx.status(plan_one(), tasks[1]), TaskStatus::Completed);
        assert_eq!(fx.read("src/b.rs"), "pub fn b_long() {}\n");
    }

    /// Interrupting `agentctl run` ends its live work in order: the whole
    /// tree ends, how it ended is recorded, nothing is left for recovery,
    /// and the plan continues through its planner.
    fn interrupted(signal: libc::c_int, group: bool) {
        let fx = Fixture::new(1, &[("long", "Change a hold tree", &["src/a.rs"], &[])]);
        fx.agentctl(&["plan", "create", "Improve the demo"]).ok();
        let mut run = fx.spawn(&["run", ONE]);
        fx.await_marker("started-long");
        let provider = fx.pid("pid-long");
        let descendant = fx.pid("descendant-long");
        match group {
            true => run.signal_group(signal),
            false => run.signal(signal),
        }
        let ran = run.wait(Duration::from_secs(30));
        assert!(!ran.status.success());
        assert!(ran.stderr.contains("interrupted"), "{}", ran.stderr);
        assert_gone_soon(provider, "the provider");
        assert_gone_soon(descendant, "its detached descendant");

        let tasks = fx.tasks(plan_one());
        let generation = fx.generations(tasks[0])[0];
        let (state, termination) = fx.executor_end(generation);
        assert!(
            state.is_terminal() && state != InvocationState::Succeeded,
            "{state}"
        );
        assert_eq!(termination, Some(expected_termination()));
        assert!(matches!(
            fx.status(plan_one(), tasks[0]),
            TaskStatus::Stopped { .. }
        ));
        assert_eq!(fx.store().owned_paths(generation).unwrap(), ["src/a.rs"]);
        assert!(fx.store().unresolved(None).unwrap().is_empty());
        let recovered = fx.agentctl(&["recover"]).ok();
        assert!(recovered.stdout.contains("nothing to recover"));
        assert_eq!(fx.read("src/a.rs"), "pub fn a() {}\n");

        fx.retry_stopped(plan_one());
        fx.agentctl(&["run", ONE]).ok();
        assert_eq!(fx.status(plan_one(), tasks[0]), TaskStatus::Completed);
    }

    fn ctrl_c_ends_live_work_in_order() {
        // As a terminal's Ctrl-C: every process of the foreground job.
        interrupted(libc::SIGINT, true);
    }

    fn sigterm_ends_live_work_in_order() {
        interrupted(libc::SIGTERM, false);
    }

    /// Killing the controller outright leaves its live work to recovery:
    /// where procd can reacquire the domain, recovery ends it and the plan
    /// continues; where it cannot, nothing new starts on the plan, and its
    /// ownership stays, however long that takes.
    fn abrupt_controller_death_fails_closed() {
        let fx = Fixture::new(1, &[("long", "Change a hold tree", &["src/a.rs"], &[])]);
        fx.agentctl(&["plan", "create", "Improve the demo"]).ok();
        let mut run = fx.spawn(&["run", ONE]);
        fx.await_marker("started-long");
        let provider = fx.pid("pid-long");
        let descendant = fx.pid("descendant-long");
        run.signal(libc::SIGKILL);
        run.wait(Duration::from_secs(10));
        let tasks = fx.tasks(plan_one());
        let generation = fx.generations(tasks[0])[0];
        if host_recovers() && !agentctl().double {
            let recovered = fx.agentctl(&["recover"]).ok();
            assert!(
                recovered.stdout.contains("recovered"),
                "{}",
                recovered.stdout
            );
            assert_gone_soon(provider, "the provider");
            assert_gone_soon(descendant, "its detached descendant");
            fx.retry_stopped(plan_one());
            fx.agentctl(&["run", ONE]).ok();
            assert_eq!(fx.status(plan_one(), tasks[0]), TaskStatus::Completed);
            return;
        }
        // procd cannot establish the orphaned domain's fate here.
        let recovered = fx.agentctl(&["recover"]).failed();
        assert!(
            recovered.stderr.contains("recovery is blocked"),
            "{}",
            recovered.stderr
        );
        let refused = fx.agentctl(&["run", ONE]).failed();
        assert!(refused.stderr.contains("recover"), "{}", refused.stderr);
        assert_eq!(fx.generations(tasks[0]), [generation]);
        assert_eq!(fx.store().owned_paths(generation).unwrap(), ["src/a.rs"]);
        let cancelled = fx.agentctl(&["plan", "cancel", ONE]).failed();
        assert!(
            cancelled.stdout.contains("no longer runs"),
            "{}",
            cancelled.stdout
        );
        // Neither a retry nor a replan can start a conflicting attempt.
        let replanned = fx.agentctl(&["plan", "update", ONE]).failed();
        assert!(replanned.stderr.contains("recover"), "{}", replanned.stderr);
        assert_eq!(fx.generations(tasks[0]), [generation]);
        // The orphans end here, by the test, which started them.
        kill_fakes(fx.markers.path());
        assert_gone_soon(provider, "the provider");
        assert_gone_soon(descendant, "its detached descendant");
        assert_eq!(fx.read("src/a.rs"), "pub fn a() {}\n");
    }
}
