//! The scheduler: runs a plan's DAG as bounded concurrent pipelines.
//!
//! It owns execution truth, never engineering judgment. It derives which
//! tasks of a running plan are eligible (see [`TaskStatus`]), claims them
//! within the project's concurrency ceiling (see [`Store::claim`]), and runs
//! each claimed generation's pipeline: the executor and the install of its
//! candidate (`crate::executor`), an independent verifier
//! (`crate::verifier`) and, only on a pass, acceptance
//! (`crate::acceptance`). Those blocks decide and record what happened; the
//! scheduler reads it back from canonical state. Only a completed
//! acceptance completes a task and so lets its dependents become eligible.
//!
//! The scheduler never creates, changes or reorders tasks, dependencies or
//! scopes, never retries or corrects a pipeline that did not complete, and
//! never completes a plan: a pipeline that stopped short leaves its task to
//! the planner, and a plan whose every task completed stays running until
//! final integration verification.
//!
//! Scheduling is event driven. The scheduler claims what it can, then
//! sleeps until one of its pipelines ends, and only then looks again; it
//! never polls canonical state or providers. Eligible tasks are claimed in
//! planner order: the order the planner added them to the plan (ascending
//! task id). Every look starts from the first task again, so an earlier
//! task waiting for ownership or capacity is always offered the next free
//! slot before any later one.
//!
//! Each pipeline runs on a thread of its own, with a store connection of
//! its own, and releases its claim once it ends. One pipeline's failure
//! never stops another. Should the scheduler itself fail to read or claim,
//! it launches nothing more, and waits for the pipelines it launched.
//!
//! The ceiling is read once, when scheduling starts; changing the
//! configuration affects the next run only. Every claim, of every plan and
//! scheduler, counts against it, so a lower ceiling launches nothing until
//! claims held fall below it, and running pipelines are never stopped for it.
//!
//! Concurrent pipelines share the project's working tree, where each
//! installs its candidate before it is verified. What a verifier judges
//! never depends on that sharing: it is given its candidate over accepted
//! state, never another pipeline's unaccepted candidate, whichever process
//! installed it (see `crate::verifier`). Within one process, pipelines
//! observe and write the working tree one at a time (see [`Gate`]), so that
//! one's observation never sees another's install half done; providers
//! themselves run concurrently. Pipelines of another agentctl process are
//! not held back that way: such an observation may then fail, which is
//! recorded truthfully, never retried.

use std::num::NonZeroU32;
use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError, mpsc};
use std::thread;

use anyhow::Result;

use crate::acceptance;
use crate::executor;
use crate::project::Project;
use crate::state::{
    Claim, ExecutionOutcome, GenerationId, Install, InstallOutcome, PlanId, Release, Snapshot,
    Store, TaskId, TaskStatus, VerificationOutcome, VerificationStatus,
};
use crate::verifier;

/// One claimed generation of a task, for a pipeline to run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Work {
    pub plan: PlanId,
    pub task: TaskId,
    pub generation: GenerationId,
}

/// Runs the pipeline of one claimed generation as far as it goes, through
/// the blocks that record what happens. What it returns is only for whoever
/// reads the report: canonical state alone says how it ended.
pub trait Pipeline: Sync {
    /// Runs `work` with a store connection of its own, holding `gate` while
    /// observing or writing the working tree, and never while a provider
    /// runs.
    fn run(&self, store: &mut Store, work: &Work, gate: &Gate) -> Result<()>;
}

/// Serializes one process's pipelines' observations and writes of the
/// working tree. It orders work; it authorizes nothing.
pub struct Gate(Mutex<()>);

impl Gate {
    pub fn hold(&self) -> MutexGuard<'_, ()> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

static WORKING_TREE: Gate = Gate(Mutex::new(()));

/// The pipeline of Blocks 11 to 13: an executor, installing its candidate,
/// an independent verifier and, on a pass, acceptance.
pub struct Blocks<'a> {
    project: &'a Project,
    /// The provider CLI for both roles; `None` finds each on `PATH`.
    executable: Option<PathBuf>,
}

impl<'a> Blocks<'a> {
    pub fn new(project: &'a Project, executable: Option<PathBuf>) -> Self {
        Self {
            project,
            executable,
        }
    }
}

impl Pipeline for Blocks<'_> {
    fn run(&self, store: &mut Store, work: &Work, gate: &Gate) -> Result<()> {
        let (project, task, generation) = (self.project, work.task, work.generation);
        let executor = {
            let _held = gate.hold();
            executor::start(project, store, task, generation, self.executable.clone())?
        };
        let executed = executor.finish_holding(project, store, || gate.hold())?;
        let installed = matches!(
            executed.capture.install,
            Install::Finished {
                outcome: InstallOutcome::Installed,
                ..
            }
        );
        if executed.capture.outcome != ExecutionOutcome::Candidate || !installed {
            return Ok(());
        }
        let verifier = {
            let _held = gate.hold();
            verifier::start(project, store, task, generation, self.executable.clone())?
        };
        let verified = verifier.finish_holding(project, store, || gate.hold())?;
        let passed = matches!(
            &verified.verification.status,
            VerificationStatus::Finished(result) if result.outcome == VerificationOutcome::Passed
        );
        if !passed {
            return Ok(());
        }
        let _held = gate.hold();
        acceptance::accept(project, store, task, generation)?;
        Ok(())
    }
}

/// How one launched pipeline ended.
#[derive(Debug)]
pub struct Finished {
    pub work: Work,
    /// Why the pipeline stopped with an error, if it did: diagnostic only.
    pub error: Option<String>,
    /// What releasing its claim established, or why that failed, in which
    /// case the claim is still held.
    pub release: std::result::Result<Release, String>,
}

/// What one scheduling run did and where it left the plan.
#[derive(Debug)]
pub struct Report {
    /// Every pipeline launched, in the order they ended.
    pub finished: Vec<Finished>,
    /// Why scheduling stopped launching early, if it did.
    pub stopped: Option<String>,
    /// The plan's scheduling state once every launched pipeline ended.
    pub snapshot: Snapshot,
}

/// Runs `plan`, starting it if it is ready, with Blocks 11 to 13 through
/// `executable` or else each provider's CLI on `PATH`, under the project's
/// configured concurrency ceiling. Returns once nothing more can be
/// launched and every pipeline launched has ended.
pub fn run(project: &Project, plan: PlanId, executable: Option<PathBuf>) -> Result<Report> {
    let blocks = Blocks::new(project, executable);
    let limit = project.config.agents.max_concurrency;
    schedule(&project.state_path(), plan, limit, &blocks)
}

/// Runs `plan`, starting it if it is ready, with `pipeline` under the
/// concurrency ceiling `limit`, on the store at `state`; see [`run`].
pub fn schedule(
    state: &Path,
    plan: PlanId,
    limit: NonZeroU32,
    pipeline: &impl Pipeline,
) -> Result<Report> {
    let mut store = Store::open(state)?;
    store.start_plan(plan)?;
    let (done, ended) = mpsc::channel::<Finished>();
    let mut finished = Vec::new();
    let mut stopped = None;
    thread::scope(|scope| {
        let mut running = 0usize;
        loop {
            if stopped.is_none() {
                let launch = |work: Work| {
                    let done = done.clone();
                    scope.spawn(move || {
                        let _ = done.send(pipe(state, work, pipeline));
                    });
                };
                match claim_eligible(&mut store, plan, limit, launch) {
                    Ok(launched) => running += launched,
                    Err(e) => stopped = Some(format!("{e:#}")),
                }
            }
            if running == 0 {
                break;
            }
            // Sleeps until a pipeline ends: only then can anything change
            // that this scheduler would act on.
            match ended.recv() {
                Ok(end) => {
                    running -= 1;
                    finished.push(end);
                }
                Err(_) => break,
            }
        }
    });
    let snapshot = store.snapshot(plan, limit)?;
    Ok(Report {
        finished,
        stopped,
        snapshot,
    })
}

/// Claims every eligible task of `plan` it can, in planner order,
/// launching each claimed pipeline at once. Returns how many it launched.
fn claim_eligible(
    store: &mut Store,
    plan: PlanId,
    limit: NonZeroU32,
    mut launch: impl FnMut(Work),
) -> Result<usize> {
    let snapshot = store.snapshot(plan, limit)?;
    let mut launched = 0;
    for (task, status) in &snapshot.tasks {
        if *status != TaskStatus::Eligible {
            continue;
        }
        match store.claim(*task, limit)? {
            Claim::Claimed(generation) => {
                launch(Work {
                    plan,
                    task: *task,
                    generation,
                });
                launched += 1;
            }
            // No later task can be claimed either.
            Claim::CapacityFull { .. } | Claim::PlanNotRunning(_) | Claim::InvalidDag(_) => break,
            // Claimed by another scheduler, or blocked meanwhile.
            Claim::Ineligible(_) => {}
        }
    }
    Ok(launched)
}

/// Runs one claimed pipeline to its end, then releases its claim if
/// canonical state allows.
fn pipe(state: &Path, work: Work, pipeline: &impl Pipeline) -> Finished {
    let ran = panic::catch_unwind(AssertUnwindSafe(|| {
        let mut store = Store::open(state)?;
        pipeline.run(&mut store, &work, &WORKING_TREE)
    }));
    let error = match ran {
        Ok(Ok(())) => None,
        Ok(Err(e)) => Some(format!("{e:#}")),
        Err(_) => Some("the pipeline panicked".to_owned()),
    };
    let release = Store::open(state)
        .and_then(|mut store| store.release_claim(work.generation))
        .map_err(|e| format!("{e:#}"));
    Finished {
        work,
        error,
        release,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, HashSet};
    use std::fs;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Barrier, Condvar};

    use super::*;
    use crate::graph::tests::Fixture;
    use crate::planner;
    use crate::source::{self, Workspace};
    use crate::state::tests::{ended, ready_plan, succeeded};
    use crate::state::{
        AcceptancePhase, Check, CheckOutcome, Condition, ExecutionStatus, ExecutorResult,
        FailureKind, GenerationState, InvocationEnd, InvocationState, Observed, PipelineOutcome,
        PlanState, Replan, ReplanId, Reported, Verdict, VerifierObserved, VerifierReport,
        VerifierResult,
    };

    /// The next executor launched, which a test waits for only so long.
    fn next(started: &mpsc::Receiver<String>) -> String {
        started
            .recv_timeout(std::time::Duration::from_secs(30))
            .expect("no executor launched")
    }

    fn limit(n: u32) -> NonZeroU32 {
        NonZeroU32::new(n).unwrap()
    }

    /// What each of some paths holds, in order: a file's text, or nothing.
    type Tree = Vec<(String, Option<String>)>;

    /// How a simulated executor ends.
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Exec {
        /// Writes every path of its authority, and reports success.
        Candidate,
        /// Deletes every path of its authority, and reports success.
        Delete,
        /// Its invocation fails.
        Fail,
        /// Never ends: attempted, with its outcome unknown.
        Unknown,
    }

    /// How a simulated verifier ends.
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Judge {
        Pass,
        Fail,
        /// Never ends.
        Unresolved,
    }

    #[derive(Clone, Copy)]
    struct Script {
        exec: Exec,
        judge: Judge,
        /// Whether synchronizing CodeGraph fails, leaving the acceptance
        /// published and unfinished.
        unfinished: bool,
        /// Whether its executor waits to be let go once running.
        hold: bool,
    }

    const PASS: Script = Script {
        exec: Exec::Candidate,
        judge: Judge::Pass,
        unfinished: false,
        hold: false,
    };

    /// Deterministic fake providers: each pipeline goes through Blocks 11
    /// to 13 as recorded by the store, with providers that write, report
    /// and end as scripted by task key, recording each launch.
    struct Simulated<'a> {
        project: &'a Project,
        scripts: HashMap<String, Script>,
        /// Every executor launched, by task key, in order.
        launches: Mutex<Vec<String>>,
        running: AtomicUsize,
        peak: AtomicUsize,
        /// Executors let go, and whether every one is.
        released: Mutex<HashSet<String>>,
        all_released: AtomicBool,
        let_go: Condvar,
        started: Mutex<Option<mpsc::Sender<String>>>,
        /// Tasks that may run once more, as scripted by `PASS`.
        reruns: HashSet<String>,
        /// What each executor launched found at each path of its
        /// authority, by task key, in order.
        seen: Mutex<Vec<(String, Tree)>>,
    }

    impl<'a> Simulated<'a> {
        fn new(project: &'a Project, scripts: &[(&str, Script)]) -> Self {
            Self {
                reruns: HashSet::new(),
                seen: Mutex::default(),
                project,
                scripts: scripts.iter().map(|(k, s)| (k.to_string(), *s)).collect(),
                launches: Mutex::default(),
                running: AtomicUsize::new(0),
                peak: AtomicUsize::new(0),
                released: Mutex::default(),
                all_released: AtomicBool::new(false),
                let_go: Condvar::new(),
                started: Mutex::new(None),
            }
        }

        /// Reports every executor launched from now on.
        fn watch(&self) -> mpsc::Receiver<String> {
            let (tx, rx) = mpsc::channel();
            *self.started.lock().unwrap_or_else(PoisonError::into_inner) = Some(tx);
            rx
        }

        fn release(&self, key: &str) {
            self.released
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .insert(key.into());
            self.let_go.notify_all();
        }

        fn release_all(&self) {
            self.all_released.store(true, Ordering::SeqCst);
            let _held = self.released.lock().unwrap_or_else(PoisonError::into_inner);
            self.let_go.notify_all();
        }

        fn launches(&self) -> Vec<String> {
            self.launches
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone()
        }

        /// What the latest executor of `key` found at each path.
        fn seen(&self, key: &str) -> Tree {
            let seen = self.seen.lock().unwrap_or_else(PoisonError::into_inner);
            let latest = seen.iter().rev().find(|(k, _)| k == key);
            latest.expect("never launched").1.clone()
        }

        fn peak(&self) -> usize {
            self.peak.load(Ordering::SeqCst)
        }

        /// [`Simulated::new`], letting `reruns` run once more.
        fn rerunning(mut self, reruns: &[&str]) -> Self {
            self.reruns = reruns.iter().map(|k| k.to_string()).collect();
            self
        }

        /// The script of `key`'s next launch.
        fn script(&self, key: &str) -> Script {
            match self.launches().iter().any(|k| k == key) {
                true => PASS,
                false => self.scripts.get(key).copied().unwrap_or(PASS),
            }
        }

        fn launched(&self, key: &str) {
            let before = {
                let mut launches = self.launches.lock().unwrap_or_else(PoisonError::into_inner);
                let before = launches.iter().filter(|k| *k == key).count();
                launches.push(key.into());
                before
            };
            // No test here runs any task twice, unless one retry of it is
            // expected: a task run beyond that stops the test at once.
            let allowed = usize::from(self.reruns.contains(key));
            assert!(before <= allowed, "{key} launched again");
            let now = self.running.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak.fetch_max(now, Ordering::SeqCst);
            if let Some(started) = &*self.started.lock().unwrap_or_else(PoisonError::into_inner) {
                let _ = started.send(key.into());
            }
        }

        /// Waits to be let go; a test that failed without letting go is
        /// not left hanging.
        fn wait_until_released(&self, key: &str) {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
            let mut released = self.released.lock().unwrap_or_else(PoisonError::into_inner);
            while !released.contains(key) && !self.all_released.load(Ordering::SeqCst) {
                let left = deadline.saturating_duration_since(std::time::Instant::now());
                if left.is_zero() {
                    drop(released);
                    panic!("{key} was never let go");
                }
                released = self
                    .let_go
                    .wait_timeout(released, left)
                    .unwrap_or_else(PoisonError::into_inner)
                    .0;
            }
        }
    }

    /// Lets every held executor go once dropped, so that a failing
    /// assertion unwinds instead of waiting on pipelines it held.
    struct Unblock<'s, 'a>(&'s Simulated<'a>);

    impl Drop for Unblock<'_, '_> {
        fn drop(&mut self) {
            self.0.release_all();
        }
    }

    fn report(verdict: Verdict) -> VerifierReport {
        VerifierReport {
            verdict,
            checked: vec![Check {
                check: "tests".into(),
                command: Some("cargo test".into()),
                outcome: CheckOutcome::Passed,
                evidence: "ok".into(),
            }],
            blockers: match verdict {
                Verdict::Pass => Vec::new(),
                Verdict::Fail => vec![crate::state::Blocker {
                    id: "b1".into(),
                    summary: "broken".into(),
                    paths: Vec::new(),
                    evidence: "fails".into(),
                    location: None,
                }],
            },
            non_blocking: Vec::new(),
        }
    }

    impl Pipeline for Simulated<'_> {
        fn run(&self, store: &mut Store, work: &Work, gate: &Gate) -> Result<()> {
            let project = self.project;
            let (task, generation) = (work.task, work.generation);
            let key = store.task(task)?.key;
            let script = self.script(&key);
            let authority = store.execution_authority(task, generation)?;
            let (execution, agent, entry, workspace, baseline) = {
                let _held = gate.hold();
                let since = crate::state::now();
                let baseline = source::snapshot(project, &authority)?;
                let workspace = Workspace::stage(project, &baseline)?;
                let seen = authority
                    .iter()
                    .map(|p| (p.clone(), fs::read_to_string(workspace.root().join(p)).ok()))
                    .collect();
                self.seen
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .push((key.clone(), seen));
                let (execution, agent, entry) = store.begin_execution(
                    task,
                    generation,
                    &authority,
                    &baseline.entries,
                    "head",
                    since,
                )?;
                (execution, agent, entry, workspace, baseline)
            };
            let invocation = store.start_invocation(agent, "claude", "fake", None)?;
            store.act(entry, Some(invocation))?;
            store.invocation_running(invocation)?;
            self.launched(&key);
            if script.hold {
                self.wait_until_released(&key);
            }
            let ran = match script.exec {
                Exec::Unknown => {
                    self.running.fetch_sub(1, Ordering::SeqCst);
                    return Ok(());
                }
                Exec::Fail => InvocationEnd {
                    failure: Some(FailureKind::ExitStatus),
                    diagnostic: Some("failed".into()),
                    exit_code: Some(1),
                    ..ended(InvocationState::Failed)
                },
                Exec::Candidate => {
                    for path in &authority {
                        let file = workspace.root().join(path);
                        fs::create_dir_all(file.parent().unwrap())?;
                        fs::write(file, format!("// {key}, generation {generation}\n"))?;
                    }
                    succeeded()
                }
                Exec::Delete => {
                    for path in &authority {
                        fs::remove_file(workspace.root().join(path))?;
                    }
                    succeeded()
                }
            };
            store.finish_invocation(invocation, &ran)?;
            self.running.fetch_sub(1, Ordering::SeqCst);
            let result = match script.exec {
                Exec::Candidate | Exec::Delete => ExecutorResult::Reported {
                    status: Reported::Succeeded,
                    claimed: &authority,
                },
                _ => ExecutorResult::None,
            };
            {
                let _held = gate.hold();
                let paths: Vec<String> = baseline.entries.iter().map(|(p, _)| p.clone()).collect();
                let observed = workspace.observe(project, &paths)?;
                let observed = Observed {
                    entries: &observed.entries,
                    head: "head",
                    settled: true,
                    result,
                };
                store.finish_execution(execution, &observed)?;
                let Some(ExecutionStatus::Captured(capture)) =
                    store.execution(generation)?.map(|e| e.status)
                else {
                    anyhow::bail!("not captured");
                };
                if capture.outcome != ExecutionOutcome::Candidate {
                    return Ok(());
                }
                crate::executor::install(project, store, execution, &capture, &workspace)?;
            }
            let (verification, observed) = {
                let _held = gate.hold();
                let candidate = store.verification_candidate(task, generation)?;
                let paths: Vec<String> = candidate.iter().map(|(p, _)| p.clone()).collect();
                let observed = source::observe_paths(project, &paths)?;
                let since = crate::state::now();
                let begun = store.begin_verification(task, generation, &observed, since)?;
                (begun, observed)
            };
            let (verification, agent, entry) = verification;
            let invocation = store.start_invocation(agent, "claude", "fake", None)?;
            store.act(entry, Some(invocation))?;
            store.invocation_running(invocation)?;
            let verdict = match script.judge {
                Judge::Unresolved => return Ok(()),
                Judge::Pass => Verdict::Pass,
                Judge::Fail => Verdict::Fail,
            };
            store.finish_invocation(invocation, &succeeded())?;
            let report = report(verdict);
            {
                let _held = gate.hold();
                let observed = VerifierObserved {
                    project: &observed,
                    mutated: &[],
                    result: VerifierResult::Reported(&report),
                };
                store.finish_verification(verification, &observed)?;
            }
            if verdict == Verdict::Fail {
                return Ok(());
            }
            let _held = gate.hold();
            if script.unfinished {
                acceptance::accept_with(project, store, task, generation, |_| {
                    anyhow::bail!("forced graph failure")
                })?;
            } else {
                acceptance::accept(project, store, task, generation)?;
            }
            Ok(())
        }
    }

    /// A project whose `src/` files for every task path are accepted, and
    /// a ready plan of tasks `(key, scope, depends_on)`.
    fn project(tasks: &[(&str, &[&str], &[&str])]) -> (Fixture, PlanId, Vec<TaskId>) {
        project_without(tasks, &[])
    }

    /// [`project`], except that no file, and no accepted state, is at any
    /// of the paths `absent`.
    fn project_without(
        tasks: &[(&str, &[&str], &[&str])],
        absent: &[&str],
    ) -> (Fixture, PlanId, Vec<TaskId>) {
        let mut fx = Fixture::new("src");
        let root = fx.project.root.clone();
        for (_, scope, _) in tasks {
            for path in scope.iter().filter(|p| !absent.contains(p)) {
                let file = root.join(path);
                fs::create_dir_all(file.parent().unwrap()).unwrap();
                fs::write(file, "// accepted\n").unwrap();
            }
        }
        source::baseline(&fx.project, &mut fx.store).unwrap();
        let (plan, ids) = ready_plan(&mut fx.store, tasks);
        (fx, plan, ids)
    }

    fn schedule_with(project: &Project, plan: PlanId, n: u32, pipeline: &impl Pipeline) -> Report {
        let report = schedule(&project.state_path(), plan, limit(n), pipeline).unwrap();
        assert_eq!(report.stopped, None);
        for end in &report.finished {
            assert_eq!(end.error, None, "{end:?}");
        }
        report
    }

    fn status(fx: &Fixture, plan: PlanId, task: TaskId) -> TaskStatus {
        let snapshot = fx.store.snapshot(plan, limit(1)).unwrap();
        snapshot.status(task).unwrap().clone()
    }

    fn event_seq(fx: &Fixture, kind: &str, task: TaskId) -> i64 {
        let events = fx.store.events_after(0, 1_000_000).unwrap();
        events
            .iter()
            .find(|e| e.kind == kind && e.task == Some(task))
            .unwrap_or_else(|| panic!("no {kind} for task {task}"))
            .seq
    }

    fn count(fx: &Fixture, sql: &str) -> i64 {
        fx.store.raw().query_row(sql, [], |r| r.get(0)).unwrap()
    }

    #[test]
    fn one_task_runs_through_blocks_11_to_13() {
        let (fx, plan, ids) = project(&[("only", &["src/a.rs"], &[])]);
        let sim = Simulated::new(&fx.project, &[]);
        let report = schedule_with(&fx.project, plan, 1, &sim);
        assert_eq!(sim.launches(), ["only"]);
        let generation = report.finished[0].work.generation;
        assert_eq!(
            report.finished[0].release,
            Ok(Release::Released(PipelineOutcome::Accepted))
        );
        let acceptance = fx.store.acceptance(generation).unwrap().unwrap();
        assert_eq!(acceptance.phase, AcceptancePhase::Completed);
        let source = fx.store.accepted_source("src/a.rs").unwrap().unwrap();
        assert_eq!(source.generation, Some(generation));
        assert_eq!(report.snapshot.status(ids[0]), Some(&TaskStatus::Completed));
        // Every task completed, and the plan stays running.
        assert_eq!(report.snapshot.condition(), Condition::AllCompleted);
        assert_eq!(fx.store.plan(plan).unwrap().state, PlanState::Running);
        assert_eq!(report.snapshot.capacity.held, 0);
    }

    #[test]
    fn dependents_start_only_once_their_dependency_completed() {
        let tasks: [(&str, &[&str], &[&str]); 3] = [
            ("a", &["src/a.rs"], &[]),
            ("b", &["src/b.rs"], &["a"]),
            ("c", &["src/c.rs"], &["b"]),
        ];
        let (fx, plan, ids) = project(&tasks);
        let [a, b, c] = ids[..] else { panic!() };
        let sim = Simulated::new(&fx.project, &[]);
        let report = schedule_with(&fx.project, plan, 3, &sim);
        assert_eq!(sim.launches(), ["a", "b", "c"]);
        assert_eq!(sim.peak(), 1);
        for (dependency, dependent) in [(a, b), (b, c)] {
            let completed = event_seq(&fx, "acceptance.completed", dependency);
            assert!(completed < event_seq(&fx, "scheduler.claimed", dependent));
        }
        assert_eq!(report.snapshot.condition(), Condition::AllCompleted);
        assert_eq!(fx.store.plan(plan).unwrap().state, PlanState::Running);
    }

    #[test]
    fn pipelines_that_stop_short_never_unlock_dependents_or_rerun() {
        let tasks: [(&str, &[&str], &[&str]); 8] = [
            ("exec-fails", &["src/a.rs"], &[]),
            ("after-exec", &["src/a2.rs"], &["exec-fails"]),
            ("verify-fails", &["src/b.rs"], &[]),
            ("after-verify", &["src/b2.rs"], &["verify-fails"]),
            ("exec-unknown", &["src/c.rs"], &[]),
            ("after-unknown", &["src/c2.rs"], &["exec-unknown"]),
            ("verify-unknown", &["src/d.rs"], &[]),
            ("after-judging", &["src/d2.rs"], &["verify-unknown"]),
        ];
        let (fx, plan, ids) = project(&tasks);
        let scripts = [
            (
                "exec-fails",
                Script {
                    exec: Exec::Fail,
                    ..PASS
                },
            ),
            (
                "verify-fails",
                Script {
                    judge: Judge::Fail,
                    ..PASS
                },
            ),
            (
                "exec-unknown",
                Script {
                    exec: Exec::Unknown,
                    ..PASS
                },
            ),
            (
                "verify-unknown",
                Script {
                    judge: Judge::Unresolved,
                    ..PASS
                },
            ),
        ];
        let sim = Simulated::new(&fx.project, &scripts);
        let report = schedule_with(&fx.project, plan, 4, &sim);
        let mut launched = sim.launches();
        launched.sort();
        assert_eq!(
            launched,
            [
                "exec-fails",
                "exec-unknown",
                "verify-fails",
                "verify-unknown"
            ]
        );
        let released: HashMap<TaskId, std::result::Result<Release, String>> = report
            .finished
            .iter()
            .map(|f| (f.work.task, f.release.clone()))
            .collect();
        assert_eq!(
            released[&ids[0]],
            Ok(Release::Released(PipelineOutcome::ExecutionFailed))
        );
        assert_eq!(
            released[&ids[2]],
            Ok(Release::Released(PipelineOutcome::VerificationFailed))
        );
        // Outcome unknown: capacity stays held until recovery.
        assert_eq!(released[&ids[4]], Ok(Release::Retained));
        assert_eq!(released[&ids[6]], Ok(Release::Retained));
        assert_eq!(report.snapshot.capacity.held, 2);
        for (dependency, dependent) in [(0, 1), (2, 3), (4, 5), (6, 7)] {
            assert_eq!(
                report.snapshot.status(ids[dependent]),
                Some(&TaskStatus::WaitingForDependencies(vec![ids[dependency]]))
            );
        }
        assert!(matches!(
            report.snapshot.status(ids[0]),
            Some(TaskStatus::Stopped {
                outcome: PipelineOutcome::ExecutionFailed,
                ..
            })
        ));
        assert!(matches!(
            report.snapshot.status(ids[4]),
            Some(TaskStatus::Scheduled(_))
        ));
        assert_eq!(report.snapshot.condition(), Condition::Waiting);

        // Re-entry launches nothing, retries nothing and records nothing.
        let events = fx.store.events_after(0, 1_000_000).unwrap().len();
        let again = schedule_with(&fx.project, plan, 4, &sim);
        assert!(again.finished.is_empty());
        assert_eq!(sim.launches().len(), 4);
        assert_eq!(fx.store.events_after(0, 1_000_000).unwrap().len(), events);
        assert_eq!(count(&fx, "SELECT count(*) FROM generations"), 4);
        assert_eq!(fx.store.plan(plan).unwrap().state, PlanState::Running);
    }

    #[test]
    fn an_unfinished_acceptance_keeps_its_capacity_and_is_never_replaced() {
        let tasks: [(&str, &[&str], &[&str]); 3] = [
            ("unfinished", &["src/a.rs"], &[]),
            ("dependent", &["src/b.rs"], &["unfinished"]),
            ("independent", &["src/c.rs"], &[]),
        ];
        let (fx, plan, ids) = project(&tasks);
        let sim = Simulated::new(
            &fx.project,
            &[(
                "unfinished",
                Script {
                    unfinished: true,
                    ..PASS
                },
            )],
        );
        let report = schedule_with(&fx.project, plan, 1, &sim);
        assert_eq!(sim.launches(), ["unfinished"]);
        let generation = report.finished[0].work.generation;
        assert_eq!(report.finished[0].release, Ok(Release::Retained));
        let phase = fx.store.acceptance(generation).unwrap().unwrap().phase;
        assert_eq!(phase, AcceptancePhase::Published);
        assert_eq!(status(&fx, plan, ids[0]), TaskStatus::Scheduled(generation));
        assert!(matches!(
            status(&fx, plan, ids[1]),
            TaskStatus::WaitingForDependencies(_)
        ));
        // Retained fail-closed: its slot is not free for other work.
        assert_eq!(report.snapshot.condition(), Condition::CapacityFull);
        let again = schedule_with(&fx.project, plan, 1, &sim);
        assert!(again.finished.is_empty());
        assert_eq!(fx.store.generations(ids[0]).unwrap().len(), 1);
        // With more capacity, only the independent task runs.
        schedule_with(&fx.project, plan, 2, &sim);
        assert_eq!(sim.launches(), ["unfinished", "independent"]);
    }

    #[test]
    fn independent_tasks_overlap_up_to_the_ceiling_and_never_beyond() {
        let tasks: [(&str, &[&str], &[&str]); 3] = [
            ("a", &["src/a.rs"], &[]),
            ("b", &["src/b.rs"], &[]),
            ("c", &["src/c.rs"], &[]),
        ];
        let (fx, plan, ids) = project(&tasks);
        let hold = Script { hold: true, ..PASS };
        let sim = Simulated::new(&fx.project, &[("a", hold), ("b", hold), ("c", hold)]);
        let started = sim.watch();
        thread::scope(|scope| {
            let _unblock = Unblock(&sim);
            let run = scope.spawn(|| schedule_with(&fx.project, plan, 2, &sim));
            let mut first = vec![next(&started), next(&started)];
            first.sort();
            assert_eq!(first, ["a", "b"]);
            // Both running at once; the third is neither claimed nor launched.
            let snapshot = fx.store.snapshot(plan, limit(2)).unwrap();
            assert_eq!(snapshot.capacity.held, 2);
            assert_eq!(snapshot.status(ids[2]), Some(&TaskStatus::Eligible));
            assert_eq!(snapshot.condition(), Condition::CapacityFull);
            assert!(fx.store.generations(ids[2]).unwrap().is_empty());
            sim.release("a");
            assert_eq!(next(&started), "c");
            sim.release_all();
            let report = run.join().unwrap();
            assert_eq!(report.snapshot.condition(), Condition::AllCompleted);
        });
        assert_eq!(sim.peak(), 2);
    }

    #[test]
    fn a_ceiling_of_one_serializes_in_planner_order() {
        // Keys out of alphabetical order: planner order is the order added.
        let tasks: [(&str, &[&str], &[&str]); 3] = [
            ("zeta", &["src/z.rs"], &[]),
            ("alpha", &["src/a.rs"], &[]),
            ("mid", &["src/m.rs"], &[]),
        ];
        for _ in 0..2 {
            let (fx, plan, _) = project(&tasks);
            let sim = Simulated::new(&fx.project, &[]);
            schedule_with(&fx.project, plan, 1, &sim);
            assert_eq!(sim.launches(), ["zeta", "alpha", "mid"]);
            assert_eq!(sim.peak(), 1);
        }
    }

    #[test]
    fn ownership_conflicts_serialize_without_becoming_dependencies() {
        let tasks: [(&str, &[&str], &[&str]); 2] = [
            ("first", &["src/shared.rs", "src/x.rs"], &[]),
            ("second", &["src/shared.rs"], &[]),
        ];
        let (fx, plan, ids) = project(&tasks);
        let sim = Simulated::new(&fx.project, &[("first", Script { hold: true, ..PASS })]);
        let started = sim.watch();
        thread::scope(|scope| {
            let _unblock = Unblock(&sim);
            let run = scope.spawn(|| schedule_with(&fx.project, plan, 2, &sim));
            assert_eq!(next(&started), "first");
            let TaskStatus::WaitingForOwnership(conflicts) = status(&fx, plan, ids[1]) else {
                panic!()
            };
            assert_eq!(conflicts[0].path, "src/shared.rs");
            assert!(fx.store.generations(ids[1]).unwrap().is_empty());
            sim.release("first");
            let report = run.join().unwrap();
            assert_eq!(report.snapshot.condition(), Condition::AllCompleted);
        });
        assert_eq!(sim.launches(), ["first", "second"]);
        assert_eq!(sim.peak(), 1);
        let released = event_seq(&fx, "acceptance.completed", ids[0]);
        assert!(released < event_seq(&fx, "scheduler.claimed", ids[1]));
        assert_eq!(count(&fx, "SELECT count(*) FROM task_dependencies"), 0);
    }

    #[test]
    fn racing_schedulers_share_one_durable_ceiling_and_launch_each_task_once() {
        let tasks: [(&str, &[&str], &[&str]); 4] = [
            ("a", &["src/a.rs"], &[]),
            ("b", &["src/b.rs"], &[]),
            ("c", &["src/c.rs"], &[]),
            ("d", &["src/d.rs"], &[]),
        ];
        let (fx, plan, _) = project(&tasks);
        let hold = Script { hold: true, ..PASS };
        let scripts = [("a", hold), ("b", hold), ("c", hold), ("d", hold)];
        let sim = Simulated::new(&fx.project, &scripts);
        let started = sim.watch();
        let barrier = Barrier::new(2);
        thread::scope(|scope| {
            let _unblock = Unblock(&sim);
            let runs: Vec<_> = (0..2)
                .map(|_| {
                    scope.spawn(|| {
                        barrier.wait();
                        schedule(&fx.project.state_path(), plan, limit(2), &sim).unwrap()
                    })
                })
                .collect();
            next(&started);
            next(&started);
            assert_eq!(fx.store.snapshot(plan, limit(2)).unwrap().capacity.held, 2);
            assert_eq!(sim.launches().len(), 2);
            sim.release_all();
            for run in runs {
                run.join().unwrap();
            }
        });
        assert!(sim.peak() <= 2, "{}", sim.peak());
        // Whatever each scheduler found, every task launched exactly once.
        let mut launched = sim.launches();
        launched.sort();
        assert!(launched.len() <= 4 && launched.windows(2).all(|w| w[0] != w[1]));
        let snapshot = schedule_with(&fx.project, plan, 2, &sim).snapshot;
        assert_eq!(snapshot.condition(), Condition::AllCompleted);
        let mut launched = sim.launches();
        launched.sort();
        assert_eq!(launched, ["a", "b", "c", "d"]);
        assert_eq!(count(&fx, "SELECT count(*) FROM generations"), 4);
    }

    #[test]
    fn a_scheduler_losing_its_claim_launches_nothing() {
        for _ in 0..3 {
            let (fx, plan, ids) = project(&[("only", &["src/a.rs"], &[])]);
            let sim = Simulated::new(&fx.project, &[]);
            let barrier = Barrier::new(2);
            let reports: Vec<Report> = thread::scope(|scope| {
                let runs: Vec<_> = (0..2)
                    .map(|_| {
                        scope.spawn(|| {
                            barrier.wait();
                            schedule(&fx.project.state_path(), plan, limit(4), &sim).unwrap()
                        })
                    })
                    .collect();
                runs.into_iter().map(|r| r.join().unwrap()).collect()
            });
            assert_eq!(sim.launches(), ["only"]);
            let finished: usize = reports.iter().map(|r| r.finished.len()).sum();
            assert_eq!(finished, 1);
            assert_eq!(fx.store.generations(ids[0]).unwrap().len(), 1);
            assert_eq!(fx.store.claims().unwrap().len(), 1);
        }
    }

    #[test]
    fn completed_tasks_are_never_rerun_and_the_plan_stays_running() {
        let tasks: [(&str, &[&str], &[&str]); 2] =
            [("a", &["src/a.rs"], &[]), ("b", &["src/b.rs"], &["a"])];
        let (fx, plan, _) = project(&tasks);
        let sim = Simulated::new(&fx.project, &[]);
        schedule_with(&fx.project, plan, 2, &sim);
        let events = fx.store.events_after(0, 1_000_000).unwrap().len();
        let claims = fx.store.claims().unwrap();
        for n in [1, 2, 8] {
            let again = schedule_with(&fx.project, plan, n, &sim);
            assert!(again.finished.is_empty());
            assert_eq!(again.snapshot.condition(), Condition::AllCompleted);
        }
        assert_eq!(sim.launches(), ["a", "b"]);
        assert_eq!(fx.store.claims().unwrap(), claims);
        assert_eq!(fx.store.events_after(0, 1_000_000).unwrap().len(), events);
        assert_eq!(fx.store.plan(plan).unwrap().state, PlanState::Running);
    }

    #[test]
    fn plans_run_side_by_side_sharing_capacity_and_ownership() {
        let mut fx = Fixture::new("src");
        fs::create_dir_all(fx.project.root.join("src")).unwrap();
        for path in ["src/a.rs", "src/b.rs"] {
            fs::write(fx.project.root.join(path), "// accepted\n").unwrap();
        }
        source::baseline(&fx.project, &mut fx.store).unwrap();
        let (one, first) = ready_plan(&mut fx.store, &[("x", &["src/a.rs"], &[])]);
        let two_tasks: [(&str, &[&str], &[&str]); 2] =
            [("y", &["src/b.rs"], &[]), ("z", &["src/a.rs"], &[])];
        let (two, second) = ready_plan(&mut fx.store, &two_tasks);
        let sim = Simulated::new(&fx.project, &[("x", Script { hold: true, ..PASS })]);
        let started = sim.watch();
        thread::scope(|scope| {
            let _unblock = Unblock(&sim);
            let x = scope.spawn(|| schedule_with(&fx.project, one, 3, &sim));
            assert_eq!(next(&started), "x");
            // Another plan runs beside it, but not over the path it owns.
            let report = schedule_with(&fx.project, two, 3, &sim);
            assert_eq!(report.finished.len(), 1);
            assert_eq!(
                report.snapshot.status(second[0]),
                Some(&TaskStatus::Completed)
            );
            let TaskStatus::WaitingForOwnership(conflicts) = status(&fx, two, second[1]) else {
                panic!()
            };
            assert_eq!(conflicts[0].owner.plan, one);
            sim.release("x");
            x.join().unwrap();
        });
        assert_eq!(status(&fx, one, first[0]), TaskStatus::Completed);
        schedule_with(&fx.project, two, 3, &sim);
        assert_eq!(sim.launches(), ["x", "y", "z"]);
        assert_eq!(status(&fx, two, second[1]), TaskStatus::Completed);
        assert_eq!(count(&fx, "SELECT count(*) FROM task_dependencies"), 0);
    }

    #[test]
    fn changing_the_ceiling_never_stops_work_and_bounds_new_work() {
        let tasks: [(&str, &[&str], &[&str]); 3] = [
            ("a", &["src/a.rs"], &[]),
            ("b", &["src/b.rs"], &[]),
            ("c", &["src/c.rs"], &[]),
        ];
        let (fx, plan, ids) = project(&tasks);
        let hold = Script { hold: true, ..PASS };
        let sim = Simulated::new(&fx.project, &[("a", hold), ("b", hold), ("c", hold)]);
        let started = sim.watch();
        thread::scope(|scope| {
            let _unblock = Unblock(&sim);
            let wide = scope.spawn(|| schedule_with(&fx.project, plan, 2, &sim));
            next(&started);
            next(&started);
            // Lowered below what is held: nothing new, nothing stopped.
            let lowered = schedule_with(&fx.project, plan, 1, &sim);
            assert!(lowered.finished.is_empty());
            assert_eq!(lowered.snapshot.condition(), Condition::CapacityFull);
            assert!(fx.store.generations(ids[2]).unwrap().is_empty());
            // Raised: the next scheduling claims more.
            let raised = scope.spawn(|| schedule_with(&fx.project, plan, 3, &sim));
            assert_eq!(next(&started), "c");
            assert_eq!(sim.peak(), 3);
            sim.release_all();
            wide.join().unwrap();
            raised.join().unwrap();
        });
        assert_eq!(status(&fx, plan, ids[2]), TaskStatus::Completed);
    }

    #[test]
    fn an_invalid_dag_launches_nothing() {
        let tasks: [(&str, &[&str], &[&str]); 2] =
            [("a", &["src/a.rs"], &[]), ("b", &["src/b.rs"], &[])];
        let (fx, plan, ids) = project(&tasks);
        let conn = rusqlite::Connection::open(fx.project.state_path()).unwrap();
        conn.execute_batch(&format!(
            "INSERT INTO task_dependencies VALUES ({plan}, {a}, {b});
             INSERT INTO task_dependencies VALUES ({plan}, {b}, {a});",
            a = ids[0],
            b = ids[1]
        ))
        .unwrap();
        let sim = Simulated::new(&fx.project, &[]);
        let report = schedule_with(&fx.project, plan, 2, &sim);
        assert!(report.finished.is_empty() && sim.launches().is_empty());
        assert_eq!(report.snapshot.condition(), Condition::InvalidDag);
        assert_eq!(fx.store.plan(plan).unwrap().state, PlanState::Running);
    }

    /// Replans `plan` as its planner proposed `commands`, against the state
    /// its feedback showed.
    fn replan(
        project: &Project,
        store: &mut Store,
        plan: PlanId,
        commands: &[planner::Command],
    ) -> ReplanId {
        let (_, basis) = planner::feedback(project, store, plan).unwrap();
        let replan = planner::apply_replan(project, store, plan, &basis, commands);
        match replan.unwrap() {
            Replan::Applied(replan) => replan,
            Replan::Stale => panic!("stale"),
        }
    }

    fn retry(task: &str) -> planner::Command {
        planner::Command::RetryTask { task: task.into() }
    }

    #[test]
    fn a_planner_retry_reruns_a_stopped_task_once_through_fresh_agents() {
        let tasks: [(&str, &[&str], &[&str]); 3] = [
            ("exec-fails", &["src/a.rs"], &[]),
            ("verify-fails", &["src/b.rs"], &[]),
            ("after", &["src/c.rs"], &["exec-fails", "verify-fails"]),
        ];
        let (mut fx, plan, ids) = project(&tasks);
        let [exec_fails, verify_fails, after] = ids[..] else {
            panic!()
        };
        let scripts = [
            (
                "exec-fails",
                Script {
                    exec: Exec::Fail,
                    ..PASS
                },
            ),
            (
                "verify-fails",
                Script {
                    judge: Judge::Fail,
                    ..PASS
                },
            ),
        ];
        let sim = Simulated::new(&fx.project, &scripts).rerunning(&["exec-fails", "verify-fails"]);
        schedule_with(&fx.project, plan, 2, &sim);
        // Stopped pipelines are never retried on the scheduler's account.
        let again = schedule_with(&fx.project, plan, 2, &sim);
        assert!(again.finished.is_empty());
        assert_eq!(sim.launches().len(), 2);

        // What the planner is told: how each pipeline ended, as recorded,
        // with the verifier's blockers as its claims.
        let (input, _) = planner::feedback(&fx.project, &fx.store, plan).unwrap();
        let listed = &input["plan"]["tasks"];
        let failed = &listed[0]["generations"][0];
        assert_eq!(failed["pipeline"], "execution_failed");
        assert_eq!(failed["execution"]["outcome"], "invocation_failed");
        assert_eq!(failed["execution"]["invocation"]["failure"], "exit_status");
        assert_eq!(failed["retains_capacity"], false);
        let rejected = &listed[1]["generations"][0];
        assert_eq!(rejected["pipeline"], "verification_failed");
        assert_eq!(rejected["execution"]["install"], "installed");
        let verification = &rejected["verifications"][0];
        assert_eq!(verification["outcome"], "failed");
        assert_eq!(
            verification["claimed_by_verifier"]["blockers"][0]["summary"],
            "broken"
        );
        assert_eq!(listed[2]["status"], "waiting_for_dependencies");
        for task in &listed.as_array().unwrap()[..2] {
            assert_eq!(task["status"], "stopped");
            assert!(
                task["may"]
                    .as_array()
                    .unwrap()
                    .contains(&"retry_task".into())
            );
        }

        // The planner revises one task to address the blocker, and retries
        // both.
        let fix = planner::Command::UpdateTask {
            task: "verify-fails".into(),
            objective: None,
            context: Some("Blocker b1: it was broken.".into()),
            paths: None,
        };
        replan(
            &fx.project,
            &mut fx.store,
            plan,
            &[fix, retry("exec-fails"), retry("verify-fails")],
        );
        let report = schedule_with(&fx.project, plan, 2, &sim);
        assert_eq!(report.snapshot.condition(), Condition::AllCompleted);
        let mut launched = sim.launches();
        launched.sort();
        assert_eq!(
            launched,
            [
                "after",
                "exec-fails",
                "exec-fails",
                "verify-fails",
                "verify-fails"
            ]
        );
        for (task, ended) in [
            (exec_fails, GenerationState::Failed),
            (verify_fails, GenerationState::Rejected),
        ] {
            let generations = fx.store.generations(task).unwrap();
            let [first, second] = &generations[..] else {
                panic!("{generations:?}")
            };
            assert_eq!(
                (first.state, second.state),
                (ended, GenerationState::Accepted)
            );
            // A fresh executor, and a fresh verifier, for the fresh
            // generation; the first keeps its own.
            let executor = |g| fx.store.execution(g).unwrap().unwrap().agent;
            assert_ne!(executor(first.id), executor(second.id));
            assert_eq!(fx.store.generation_revision(first.id).unwrap(), Some(1));
        }
        let verifier = |g| fx.store.verifications(g).unwrap()[0].agent;
        let generations = fx.store.generations(verify_fails).unwrap();
        assert_ne!(verifier(generations[0].id), verifier(generations[1].id));
        // The revised task ran its revision; the other, its only one.
        assert_eq!(
            fx.store.generation_revision(generations[1].id).unwrap(),
            Some(2)
        );
        let retried = fx.store.generations(exec_fails).unwrap()[1].id;
        assert_eq!(fx.store.generation_revision(retried).unwrap(), Some(1));
        assert_eq!(fx.store.generations(after).unwrap().len(), 1);
        // Each authorization was used once, and nothing runs again.
        let again = schedule_with(&fx.project, plan, 2, &sim);
        assert!(again.finished.is_empty());
    }

    #[test]
    fn a_replacement_task_runs_instead_of_a_retry() {
        let tasks: [(&str, &[&str], &[&str]); 2] = [
            ("old", &["src/a.rs"], &[]),
            ("next", &["src/b.rs"], &["old"]),
        ];
        let (mut fx, plan, ids) = project(&tasks);
        let fails = Script {
            exec: Exec::Fail,
            ..PASS
        };
        let sim = Simulated::new(&fx.project, &[("old", fails)]);
        schedule_with(&fx.project, plan, 2, &sim);
        let old = fx.store.generations(ids[0]).unwrap()[0].id;
        replan(
            &fx.project,
            &mut fx.store,
            plan,
            &[
                planner::Command::CancelTask { task: "old".into() },
                planner::Command::AddTask {
                    task: "replacement".into(),
                    objective: "Change a another way".into(),
                    context: String::new(),
                    paths: vec!["src/a.rs".into()],
                    depends_on: Vec::new(),
                },
                planner::Command::SetDependencies {
                    task: "next".into(),
                    depends_on: vec!["replacement".into()],
                },
            ],
        );
        let report = schedule_with(&fx.project, plan, 2, &sim);
        assert_eq!(sim.launches(), ["old", "replacement", "next"]);
        assert_eq!(report.snapshot.condition(), Condition::AllCompleted);
        assert_eq!(report.snapshot.status(ids[0]), Some(&TaskStatus::Cancelled));
        // The cancelled task's attempt stays as it happened.
        let generations = fx.store.generations(ids[0]).unwrap();
        assert_eq!(
            (generations.len(), generations[0].id, generations[0].state),
            (1, old, GenerationState::Failed)
        );
        assert!(fx.store.execution(old).unwrap().is_some());
        let replacement = fx.store.tasks(plan).unwrap()[2].id;
        let source = fx.store.accepted_source("src/a.rs").unwrap().unwrap();
        let accepted = fx.store.generations(replacement).unwrap()[0].id;
        assert_eq!(source.generation, Some(accepted));
    }

    #[test]
    fn completed_and_unfinished_work_is_never_replanned() {
        let tasks: [(&str, &[&str], &[&str]); 3] = [
            ("done", &["src/a.rs"], &[]),
            ("unfinished", &["src/b.rs"], &[]),
            ("failing", &["src/c.rs"], &[]),
        ];
        let (mut fx, plan, ids) = project(&tasks);
        let scripts = [
            (
                "unfinished",
                Script {
                    unfinished: true,
                    ..PASS
                },
            ),
            (
                "failing",
                Script {
                    exec: Exec::Fail,
                    ..PASS
                },
            ),
        ];
        let sim = Simulated::new(&fx.project, &scripts).rerunning(&["failing"]);
        schedule_with(&fx.project, plan, 3, &sim);
        let done = fx.store.generations(ids[0]).unwrap()[0].id;
        let unfinished = fx.store.generations(ids[1]).unwrap()[0].id;
        let accepted = |fx: &Fixture| {
            (
                fx.store.accepted_source("src/a.rs").unwrap(),
                fx.store.accepted_source("src/b.rs").unwrap(),
                fx.store.acceptance(done).unwrap(),
                fx.store.acceptance(unfinished).unwrap(),
                fx.store.generations(ids[0]).unwrap(),
                fx.store.generations(ids[1]).unwrap(),
                fx.store.owned_paths(unfinished).unwrap(),
                fx.store.task(ids[0]).unwrap(),
            )
        };
        let before = accepted(&fx);
        assert_eq!(before.3.as_ref().unwrap().phase, AcceptancePhase::Published);
        let (_, basis) = planner::feedback(&fx.project, &fx.store, plan).unwrap();
        for (key, expected) in [("done", "completed"), ("unfinished", "not established")] {
            for command in [
                retry(key),
                planner::Command::CancelTask { task: key.into() },
                planner::Command::SetDependencies {
                    task: key.into(),
                    depends_on: Vec::new(),
                },
                planner::Command::UpdateTask {
                    task: key.into(),
                    objective: Some("Redo it".into()),
                    context: None,
                    paths: None,
                },
            ] {
                let refused =
                    planner::apply_replan(&fx.project, &mut fx.store, plan, &basis, &[command]);
                let message = format!("{:#}", refused.unwrap_err());
                assert!(message.contains(expected), "{message}");
            }
        }
        // Replanning the rest, follow-up work included, leaves both as they
        // were.
        replan(
            &fx.project,
            &mut fx.store,
            plan,
            &[
                retry("failing"),
                planner::Command::AddTask {
                    task: "follow-up".into(),
                    objective: "Build on done".into(),
                    context: String::new(),
                    paths: vec!["src/d.rs".into()],
                    depends_on: vec!["done".into()],
                },
            ],
        );
        schedule_with(&fx.project, plan, 3, &sim);
        assert_eq!(accepted(&fx), before);
        let mut launched = sim.launches();
        launched.sort();
        assert_eq!(
            launched,
            ["done", "failing", "failing", "follow-up", "unfinished"]
        );
        assert_eq!(status(&fx, plan, ids[2]), TaskStatus::Completed);
        assert_eq!(status(&fx, plan, ids[1]), TaskStatus::Scheduled(unfinished));
    }

    const FAILS_VERIFICATION: Script = Script {
        judge: Judge::Fail,
        ..PASS
    };

    const ACCEPTED: Option<&str> = Some("// accepted\n");

    /// What the working tree holds at each of `paths`.
    fn tree(project: &Project, paths: &[&str]) -> Tree {
        paths
            .iter()
            .map(|p| (p.to_string(), fs::read_to_string(project.root.join(p)).ok()))
            .collect()
    }

    fn holding(paths: &[&str], content: Option<&str>) -> Tree {
        paths
            .iter()
            .map(|p| (p.to_string(), content.map(str::to_owned)))
            .collect()
    }

    /// Literal names, in authority order.
    const LITERAL: [&str; 6] = [
        "src/(group).rs",
        "src/@scope.rs",
        "src/[id].rs",
        "src/a+b.rs",
        "src/with space.rs",
        "src/日本語.rs",
    ];

    #[test]
    fn a_retry_starts_from_accepted_state_never_the_failed_candidate() {
        let tasks: [(&str, &[&str], &[&str]); 3] = [
            ("modify", &LITERAL, &[]),
            ("delete", &["src/gone.rs"], &[]),
            ("create", &["src/new.rs"], &[]),
        ];
        let (mut fx, plan, ids) = project_without(&tasks, &["src/new.rs"]);
        let deletes = Script {
            exec: Exec::Delete,
            ..FAILS_VERIFICATION
        };
        let scripts = [
            ("modify", FAILS_VERIFICATION),
            ("delete", deletes),
            ("create", FAILS_VERIFICATION),
        ];
        let sim = Simulated::new(&fx.project, &scripts).rerunning(&["modify", "delete", "create"]);
        schedule_with(&fx.project, plan, 3, &sim);
        // Each failed candidate is installed in the working tree, and no
        // path it changed has accepted state from it.
        let first: Vec<GenerationId> = ids
            .iter()
            .map(|&t| fx.store.generations(t).unwrap()[0].id)
            .collect();
        let candidate =
            |key: &str, generation| Some(format!("// {key}, generation {generation}\n"));
        let installed = tree(&fx.project, &LITERAL);
        for (_, content) in &installed {
            assert_eq!(*content, candidate("modify", first[0]));
        }
        assert_eq!(tree(&fx.project, &["src/gone.rs"])[0].1, None);
        assert_eq!(
            tree(&fx.project, &["src/new.rs"])[0].1,
            candidate("create", first[2])
        );
        assert_eq!(fx.store.accepted_source("src/new.rs").unwrap(), None);

        replan(
            &fx.project,
            &mut fx.store,
            plan,
            &[retry("modify"), retry("delete"), retry("create")],
        );
        // Restored exactly: modified and deleted files to their accepted
        // bytes, the created one to absence, whatever their names.
        assert_eq!(tree(&fx.project, &LITERAL), holding(&LITERAL, ACCEPTED));
        assert_eq!(
            tree(&fx.project, &["src/gone.rs", "src/new.rs"]),
            [
                ("src/gone.rs".to_owned(), Some("// accepted\n".to_owned())),
                ("src/new.rs".to_owned(), None)
            ]
        );
        for (&task, &generation) in ids.iter().zip(&first) {
            assert_eq!(
                fx.store.generations(task).unwrap()[0].state,
                GenerationState::Rejected
            );
            assert!(fx.store.owned_paths(generation).unwrap().is_empty());
        }

        // Each fresh executor starts from accepted state alone.
        let report = schedule_with(&fx.project, plan, 3, &sim);
        assert_eq!(report.snapshot.condition(), Condition::AllCompleted);
        assert_eq!(sim.seen("modify"), holding(&LITERAL, ACCEPTED));
        assert_eq!(sim.seen("delete"), holding(&["src/gone.rs"], ACCEPTED));
        assert_eq!(sim.seen("create"), holding(&["src/new.rs"], None));
    }

    #[test]
    fn abandoning_one_candidate_never_touches_anothers() {
        let tasks: [(&str, &[&str], &[&str]); 2] =
            [("a", &["src/a.rs"], &[]), ("b", &["src/b.rs"], &[])];
        let (mut fx, plan, ids) = project(&tasks);
        let unresolved = Script {
            judge: Judge::Unresolved,
            ..PASS
        };
        let sim = Simulated::new(&fx.project, &[("a", FAILS_VERIFICATION), ("b", unresolved)]);
        schedule_with(&fx.project, plan, 2, &sim);
        let b = fx.store.generations(ids[1]).unwrap()[0].id;
        let provisional = Some(format!("// b, generation {b}\n"));
        assert_eq!(tree(&fx.project, &["src/b.rs"])[0].1, provisional);
        replan(&fx.project, &mut fx.store, plan, &[retry("a")]);
        assert_eq!(tree(&fx.project, &["src/a.rs"])[0].1.as_deref(), ACCEPTED);
        // `b`'s candidate, still being verified, stays installed and owned.
        assert_eq!(tree(&fx.project, &["src/b.rs"])[0].1, provisional);
        assert_eq!(fx.store.owned_paths(b).unwrap(), ["src/b.rs"]);
        assert_eq!(status(&fx, plan, ids[1]), TaskStatus::Scheduled(b));
    }

    #[test]
    fn drifted_paths_are_never_overwritten_and_nothing_is_replanned() {
        let (mut fx, plan, ids) = project(&[("a", &["src/a.rs", "src/b.rs"], &[])]);
        let sim = Simulated::new(&fx.project, &[("a", FAILS_VERIFICATION)]).rerunning(&["a"]);
        schedule_with(&fx.project, plan, 1, &sim);
        let generation = fx.store.generations(ids[0]).unwrap()[0].id;
        let candidate = Some(format!("// a, generation {generation}\n"));
        // Someone changed one of the candidate's paths since it was
        // installed.
        fs::write(fx.project.root.join("src/b.rs"), "// drift\n").unwrap();
        let everything = |fx: &Fixture| {
            (
                fx.store.generations(ids[0]).unwrap(),
                fx.store.owned_paths(generation).unwrap(),
                fx.store.retry_authorizations(ids[0]).unwrap(),
                fx.store.replans(plan).unwrap(),
                fx.store.revisions(ids[0]).unwrap(),
                tree(&fx.project, &["src/a.rs", "src/b.rs"]),
            )
        };
        let before = everything(&fx);
        let (_, basis) = planner::feedback(&fx.project, &fx.store, plan).unwrap();
        let refused =
            planner::apply_replan(&fx.project, &mut fx.store, plan, &basis, &[retry("a")]);
        let reason = refused.unwrap_err();
        assert!(reason.downcast_ref::<planner::Rejection>().is_some());
        let message = format!("{reason:#}");
        assert!(
            message.contains("neither") && message.contains("src/b.rs"),
            "{message}"
        );
        // Nothing written, not even the path still holding the candidate.
        assert_eq!(everything(&fx), before);
        assert_eq!(before.5[0].1, candidate);
        assert_eq!(before.0[0].state, GenerationState::Active);

        // Should a path already hold its accepted state, as an interrupted
        // restoration leaves it, restoring completes the rest.
        fs::write(fx.project.root.join("src/b.rs"), "// accepted\n").unwrap();
        replan(&fx.project, &mut fx.store, plan, &[retry("a")]);
        assert_eq!(
            tree(&fx.project, &["src/a.rs", "src/b.rs"]),
            holding(&["src/a.rs", "src/b.rs"], ACCEPTED)
        );
        schedule_with(&fx.project, plan, 1, &sim);
        assert_eq!(sim.seen("a"), holding(&["src/a.rs", "src/b.rs"], ACCEPTED));
    }

    #[test]
    fn racing_replans_restore_once_and_a_claim_only_follows_restoration() {
        for _ in 0..3 {
            let (fx, plan, ids) = project(&[("a", &["src/a.rs"], &[])]);
            let sim = Simulated::new(&fx.project, &[("a", FAILS_VERIFICATION)]);
            schedule_with(&fx.project, plan, 1, &sim);
            let (_, basis) = planner::feedback(&fx.project, &fx.store, plan).unwrap();
            let project = &fx.project;
            let barrier = Barrier::new(3);
            let (replans, claimed) = thread::scope(|scope| {
                let replanning: Vec<_> = (0..2)
                    .map(|_| {
                        scope.spawn(|| {
                            let mut store = Store::open(&project.state_path()).unwrap();
                            barrier.wait();
                            let commands = [retry("a")];
                            planner::apply_replan(project, &mut store, plan, &basis, &commands)
                                .unwrap()
                        })
                    })
                    .collect();
                // A scheduler of another process claims as soon as it can,
                // and reads what the working tree then holds.
                let claiming = scope.spawn(|| {
                    let mut store = Store::open(&project.state_path()).unwrap();
                    barrier.wait();
                    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
                    loop {
                        if let Claim::Claimed(g) = store.claim(ids[0], limit(4)).unwrap() {
                            let seen = fs::read_to_string(project.root.join("src/a.rs")).unwrap();
                            return (g, seen);
                        }
                        assert!(std::time::Instant::now() < deadline, "never claimed");
                        thread::sleep(std::time::Duration::from_millis(1));
                    }
                });
                let replans: Vec<Replan> =
                    replanning.into_iter().map(|h| h.join().unwrap()).collect();
                (replans, claiming.join().unwrap())
            });
            let applied = replans
                .iter()
                .filter(|r| matches!(r, Replan::Applied(_)))
                .count();
            assert_eq!(applied, 1, "{replans:?}");
            assert!(replans.contains(&Replan::Stale), "{replans:?}");
            assert_eq!(claimed.1, "// accepted\n");
            assert_eq!(fx.store.generations(ids[0]).unwrap()[1].id, claimed.0);
        }
    }

    #[test]
    fn cancelling_discards_the_failed_candidate_for_later_work() {
        let (mut fx, plan, _) = project(&[("old", &["src/a.rs"], &[])]);
        let sim = Simulated::new(&fx.project, &[("old", FAILS_VERIFICATION)]);
        schedule_with(&fx.project, plan, 1, &sim);
        assert_ne!(tree(&fx.project, &["src/a.rs"])[0].1.as_deref(), ACCEPTED);
        replan(
            &fx.project,
            &mut fx.store,
            plan,
            &[
                planner::Command::CancelTask { task: "old".into() },
                planner::Command::AddTask {
                    task: "replacement".into(),
                    objective: "Change a another way".into(),
                    context: String::new(),
                    paths: vec!["src/a.rs".into()],
                    depends_on: Vec::new(),
                },
            ],
        );
        assert_eq!(tree(&fx.project, &["src/a.rs"])[0].1.as_deref(), ACCEPTED);
        schedule_with(&fx.project, plan, 1, &sim);
        assert_eq!(sim.seen("replacement"), holding(&["src/a.rs"], ACCEPTED));
    }

    #[test]
    fn revising_dependencies_needs_a_fresh_authorization() {
        let tasks: [(&str, &[&str], &[&str]); 2] =
            [("b", &["src/b.rs"], &[]), ("c", &["src/c.rs"], &[])];
        let (mut fx, plan, ids) = project(&tasks);
        let fails = Script {
            exec: Exec::Fail,
            ..PASS
        };
        let sim = Simulated::new(&fx.project, &[("b", fails)]).rerunning(&["b"]);
        schedule_with(&fx.project, plan, 2, &sim);
        let first = replan(&fx.project, &mut fx.store, plan, &[retry("b")]);
        // `c` completed, so `b` depending on it changes nothing but its
        // definition, which the authorization did not authorize.
        let on_c = planner::Command::SetDependencies {
            task: "b".into(),
            depends_on: vec!["c".into()],
        };
        replan(&fx.project, &mut fx.store, plan, &[on_c]);
        assert!(
            schedule_with(&fx.project, plan, 2, &sim)
                .finished
                .is_empty()
        );
        let mut launched = sim.launches();
        launched.sort();
        assert_eq!(launched, ["b", "c"]);
        let second = replan(&fx.project, &mut fx.store, plan, &[retry("b")]);
        let report = schedule_with(&fx.project, plan, 2, &sim);
        assert_eq!(report.finished.len(), 1);
        assert_eq!(report.snapshot.condition(), Condition::AllCompleted);
        let retried = fx.store.generations(ids[0]).unwrap()[1].id;
        assert_eq!(fx.store.generation_revision(retried).unwrap(), Some(2));
        let authorizations = fx.store.retry_authorizations(ids[0]).unwrap();
        assert_eq!(
            authorizations
                .iter()
                .map(|a| (a.revision, a.replan, a.used_by))
                .collect::<Vec<_>>(),
            [(1, first, None), (2, second, Some(retried))]
        );
    }

    #[test]
    fn work_accepted_or_in_flight_is_never_abandoned_beneath_the_store() {
        let tasks: [(&str, &[&str], &[&str]); 4] = [
            ("done", &["src/a.rs"], &[]),
            ("unfinished", &["src/b.rs"], &[]),
            ("unknown", &["src/c.rs"], &[]),
            ("other", &["src/d.rs"], &[]),
        ];
        let (mut fx, plan, ids) = project(&tasks);
        let scripts = [
            (
                "unfinished",
                Script {
                    unfinished: true,
                    ..PASS
                },
            ),
            (
                "unknown",
                Script {
                    exec: Exec::Unknown,
                    ..PASS
                },
            ),
            ("other", FAILS_VERIFICATION),
        ];
        let sim = Simulated::new(&fx.project, &scripts);
        schedule_with(&fx.project, plan, 4, &sim);
        let replan = replan(&fx.project, &mut fx.store, plan, &[retry("other")]);
        for &task in &ids[..3] {
            let generation = fx.store.generations(task).unwrap()[0].id;
            let owned = fx.store.owned_paths(generation).unwrap();
            for (sql, expected) in [
                (
                    format!(
                        "INSERT INTO generation_abandonments VALUES
                           ({generation}, 'failed', {replan}, {generation}, NULL, 0)"
                    ),
                    "only a replan abandons",
                ),
                (
                    format!("UPDATE generations SET state = 'failed' WHERE id = {generation}"),
                    "abandons it",
                ),
            ] {
                let message = fx.store.raw().execute_batch(&sql).unwrap_err().to_string();
                assert!(message.contains(expected), "{sql}: {message}");
            }
            // Accepting released what the completed one owned.
            assert_eq!(owned.is_empty(), task == ids[0]);
            let release = format!("DELETE FROM ownership WHERE generation_id = {generation}");
            assert_eq!(
                fx.store.raw().execute_batch(&release).is_err(),
                !owned.is_empty()
            );
            assert_eq!(fx.store.owned_paths(generation).unwrap(), owned);
        }
        let states: Vec<GenerationState> = ids[..3]
            .iter()
            .map(|&t| fx.store.generations(t).unwrap()[0].state)
            .collect();
        assert_eq!(
            states,
            [
                GenerationState::Accepted,
                GenerationState::Active,
                GenerationState::Active
            ]
        );
    }
}
