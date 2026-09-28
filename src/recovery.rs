//! Crash and restart recovery: making canonical state truthful once an
//! agentctl process, or the work it supervised, was interrupted.
//!
//! The store survives whatever ends a process; what that process was doing
//! may not have. Its providers may still run, an install may have written
//! part of a candidate, an acceptance may have stopped between phases, and
//! how an invocation ended may be lost with it. Recovery establishes what
//! can be proven from canonical state and the host, records exactly that,
//! and preserves the rest as unresolved: it never retries, never judges,
//! never accepts, never completes a plan and never guesses what a process
//! that ended did. It is reconciliation, not retry: whatever work follows
//! is fresh, under Blocks 14 to 17.
//!
//! What may be interrupted is whatever `Store::unresolved` lists, each with
//! the session of the process that recorded it (see `crate::state`). A
//! session whose process still runs is left alone. Of one that ended,
//! recovery settles, in order:
//!
//! 1. Each invocation with no recorded end. Only once its lifecycle is
//!    settled ([`Lifecycle`]) is it interrupted, which says nothing of how
//!    it would have ended. Agentctl has no authority of its own to settle a
//!    provider's processes (see [`settle_lifecycle`]): until one is
//!    provided, the invocation stays unresolved, and so does everything
//!    waiting on it.
//! 2. Each attempted action, from what its own record and the working tree
//!    establish: an execution, verification or integration verification
//!    whose process ended before recording what its invocation yielded is
//!    interrupted, judging and installing nothing; a replan that no
//!    committed replan applied failed; an install is completed where every
//!    path holds the candidate, and otherwise restored to what each path
//!    held before and failed, unless a path holds anything else, which is
//!    never overwritten.
//! 3. Each action left intended, which the process never attempted, since
//!    it acts only once its attempt is durable: withdrawn.
//! 4. Each acceptance that published its source and did not complete,
//!    which is finished exactly as recorded (see `crate::acceptance`): its
//!    next phases need no judgment, and ownership is released only by the
//!    last.
//! 5. Each claim, released only once its pipeline's outcome is established
//!    (see `crate::scheduler`).
//!
//! Every step is durable before the next relies on it, and each is
//! established afresh when recovery runs again: a recovery interrupted
//! itself is finished by the next, and one with nothing to settle changes
//! nothing. Until a plan's interrupted work is settled, no new work starts
//! on it (see `Store::recovery_required`); other plans run on.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs::TryLockError;
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::acceptance::{self, Outcome as Accepted};
use crate::project::Project;
use crate::source::{self, Interrupted};
use crate::state::{
    ActedOn, Ended, ExecutionStatus, GenerationId, InstallOutcome, InvocationId, JournalId,
    Liveness, PlanId, Release, Store, Unresolved, UnresolvedKind,
};

/// What settling an interrupted invocation's lifecycle established.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Lifecycle {
    /// Its processes are proven gone.
    Gone,
    /// They may remain, or nothing can prove otherwise, for the reason given.
    Uncertain(String),
}

/// Settles the lifecycle of an interrupted invocation.
pub type Settle<'a> = &'a dyn Fn(InvocationId) -> Lifecycle;

/// Agentctl's production settlement: none. It owns no authority over a
/// provider's processes, and a missing process is no proof that they ended,
/// so this never says `Gone`. A later block supplies the authority.
pub fn settle_lifecycle(_: InvocationId) -> Lifecycle {
    Lifecycle::Uncertain("agentctl has no authority to settle a provider's lifecycle yet".into())
}

/// What recovery found and did.
#[derive(Debug, Default)]
pub struct Report {
    /// Everything that was unresolved, in the order considered.
    pub items: Vec<Item>,
}

/// One unresolved record and what became of it.
#[derive(Debug)]
pub struct Item {
    pub plan: PlanId,
    pub generation: Option<GenerationId>,
    pub subject: Subject,
    pub outcome: Outcome,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Subject {
    Invocation(InvocationId),
    Action { entry: JournalId, action: String },
    Claim(GenerationId),
    Acceptance(GenerationId),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Settled, as said.
    Recovered(String),
    /// The work of an agentctl process still running: left alone.
    Running,
    /// Left unresolved, for the reason given, as it was found.
    Blocked(String),
    /// Left unresolved: this host cannot provide what settling it needs.
    Unsupported(String),
}

impl Report {
    /// Whether nothing was unresolved.
    pub fn clean(&self) -> bool {
        self.items.is_empty()
    }

    /// Whether everything unresolved of a process that ended was settled.
    pub fn settled(&self) -> bool {
        self.items
            .iter()
            .all(|i| matches!(i.outcome, Outcome::Recovered(_) | Outcome::Running))
    }
}

impl fmt::Display for Subject {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invocation(id) => write!(f, "invocation {id}"),
            Self::Action { entry, action } => write!(f, "action {entry} ({action})"),
            Self::Claim(generation) => write!(f, "claim of generation {generation}"),
            Self::Acceptance(generation) => write!(f, "acceptance of generation {generation}"),
        }
    }
}

impl fmt::Display for Outcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Recovered(what) => write!(f, "recovered: {what}"),
            Self::Running => f.write_str("in use by a running agentctl process; left alone"),
            Self::Blocked(why) => write!(f, "blocked: {why}"),
            Self::Unsupported(why) => write!(f, "unsupported: {why}"),
        }
    }
}

/// Recovers the project: see the module documentation.
pub fn recover(project: &Project) -> Result<Report> {
    let mut store = project.hydrate()?;
    recover_store(project, &mut store)
}

/// [`recover`], through `store`, the project's.
pub fn recover_store(project: &Project, store: &mut Store) -> Result<Report> {
    recover_with(project, store, &settle_lifecycle)
}

/// [`recover_store`], with `settle` deciding whether an interrupted
/// invocation's lifecycle is settled.
pub fn recover_with(project: &Project, store: &mut Store, settle: Settle) -> Result<Report> {
    let lock = store.recovery_lock()?;
    // Asked a few times over when found held: a process that only checked
    // it shares its hold with any process it was starting meanwhile, until
    // that one executes (see `liveness`).
    let mut asked = 0;
    loop {
        match lock.try_lock() {
            Ok(()) => break,
            Err(TryLockError::WouldBlock) if asked < 25 => {
                asked += 1;
                thread::sleep(Duration::from_millis(20));
            }
            Err(TryLockError::WouldBlock) => {
                bail!("another `agentctl recover` is settling interrupted work")
            }
            Err(TryLockError::Error(e)) => return Err(e).context("taking the recovery lock"),
        }
    }
    let mut by_session: BTreeMap<String, Vec<Unresolved>> = BTreeMap::new();
    for unresolved in store.unresolved(None)? {
        by_session
            .entry(unresolved.session.clone())
            .or_default()
            .push(unresolved);
    }
    let mut report = Report::default();
    for (session, records) in by_session {
        match liveness(store, &session)? {
            Liveness::Running => {
                for record in &records {
                    report.push(store, record, Outcome::Running)?;
                }
            }
            Liveness::Unknown(why) => {
                for record in &records {
                    report.push(store, record, Outcome::Blocked(why.clone()))?;
                }
            }
            Liveness::Ended(ended) => {
                let mut session = Session {
                    project,
                    store,
                    ended: &ended,
                    report: &mut report,
                    settle,
                    unsettled: BTreeSet::new(),
                };
                session.settle(&records)?;
                let left = store.unresolved(None)?;
                if !left.iter().any(|u| u.session == ended.session()) {
                    ended.retire();
                }
            }
        }
    }
    Ok(report)
}

/// The liveness of `session`, asked a few times over when its lock is
/// found held, as another process may hold it only while it asks too.
fn liveness(store: &Store, session: &str) -> Result<Liveness> {
    for _ in 0..5 {
        match store.liveness(session)? {
            Liveness::Running => thread::sleep(Duration::from_millis(20)),
            other => return Ok(other),
        }
    }
    Ok(Liveness::Running)
}

impl Report {
    /// Adds what became of `record`, recording why it stays unresolved,
    /// if it does, with its plan.
    fn push(&mut self, store: &mut Store, record: &Unresolved, outcome: Outcome) -> Result<()> {
        let subject = match record.kind {
            UnresolvedKind::Invocation(id) => Subject::Invocation(id),
            UnresolvedKind::Intended(entry) | UnresolvedKind::Attempted(entry) => Subject::Action {
                entry,
                action: store.journal_entry(entry)?.intent.action,
            },
            UnresolvedKind::Claim(generation) => Subject::Claim(generation),
            UnresolvedKind::Acceptance(generation) => Subject::Acceptance(generation),
        };
        if let Outcome::Blocked(why) | Outcome::Unsupported(why) = &outcome {
            store.recovery_blocked(record.plan, &format!("{subject}: {why}"))?;
        }
        self.items.push(Item {
            plan: record.plan,
            generation: record.generation,
            subject,
            outcome,
        });
        Ok(())
    }
}

/// Settling what one ended session left unresolved.
struct Session<'a> {
    project: &'a Project,
    store: &'a mut Store,
    ended: &'a Ended,
    report: &'a mut Report,
    settle: Settle<'a>,
    /// Its invocations left unresolved: nothing waiting on them is settled.
    unsettled: BTreeSet<InvocationId>,
}

impl Session<'_> {
    fn settle(&mut self, records: &[Unresolved]) -> Result<()> {
        let of = |kind: fn(&UnresolvedKind) -> bool| records.iter().filter(move |r| kind(&r.kind));
        for record in of(|k| matches!(k, UnresolvedKind::Invocation(_))) {
            let UnresolvedKind::Invocation(id) = record.kind else {
                continue;
            };
            let outcome = self.invocation(id);
            if !matches!(outcome, Outcome::Recovered(_)) {
                self.unsettled.insert(id);
            }
            self.report.push(self.store, record, outcome)?;
        }
        for record in of(|k| matches!(k, UnresolvedKind::Attempted(_))) {
            let UnresolvedKind::Attempted(entry) = record.kind else {
                continue;
            };
            let outcome = self.attempted(entry, record.generation);
            self.report.push(self.store, record, outcome)?;
        }
        for record in of(|k| matches!(k, UnresolvedKind::Intended(_))) {
            let UnresolvedKind::Intended(entry) = record.kind else {
                continue;
            };
            let outcome = self.intended(entry);
            self.report.push(self.store, record, outcome)?;
        }
        for record in of(|k| matches!(k, UnresolvedKind::Acceptance(_))) {
            let UnresolvedKind::Acceptance(generation) = record.kind else {
                continue;
            };
            let outcome = self.acceptance(generation);
            self.report.push(self.store, record, outcome)?;
        }
        failpoint!("recovery.claims");
        for record in of(|k| matches!(k, UnresolvedKind::Claim(_))) {
            let UnresolvedKind::Claim(generation) = record.kind else {
                continue;
            };
            let outcome = self.claim(generation);
            self.report.push(self.store, record, outcome)?;
        }
        Ok(())
    }

    /// Records `invocation` interrupted once its lifecycle is settled.
    fn invocation(&mut self, invocation: InvocationId) -> Outcome {
        if let Lifecycle::Uncertain(why) = (self.settle)(invocation) {
            return Outcome::Unsupported(format!("its provider's processes may still run: {why}"));
        }
        let mut ended = || -> Result<Outcome> {
            failpoint!("recovery.terminated");
            let diagnostic = "agentctl stopped supervising it; its lifecycle was settled: how \
                              it would have ended is unknown";
            self.store
                .interrupt_invocation(self.ended, invocation, diagnostic)?;
            Ok(Outcome::Recovered(
                "interrupted; its lifecycle was settled".into(),
            ))
        };
        ended().unwrap_or_else(|e| Outcome::Blocked(format!("{e:#}")))
    }

    /// Establishes what the attempted action `entry` did, as far as can be.
    fn attempted(&mut self, entry: JournalId, generation: Option<GenerationId>) -> Outcome {
        let mut settled = || -> Result<Outcome> {
            if let Some(invocation) = self.waits_on(entry)? {
                return Ok(Outcome::Blocked(format!(
                    "its invocation {invocation} is unresolved"
                )));
            }
            let (store, ended) = (&mut *self.store, self.ended);
            Ok(match store.attempt(entry)? {
                ActedOn::Execution(_) => {
                    let head = source::head(self.project)?;
                    store.interrupt_execution(ended, entry, &head)?;
                    Outcome::Recovered(
                        "interrupted before its workspace was captured: nothing it did is \
                         established, and nothing of it was installed"
                            .into(),
                    )
                }
                ActedOn::Install(_) => {
                    let generation = generation.context("an install serves a generation")?;
                    let execution = store
                        .execution(generation)?
                        .context("the execution vanished")?;
                    let ExecutionStatus::Captured(capture) = execution.status else {
                        bail!("execution {} was never captured", execution.id);
                    };
                    match source::recover_install(self.project, &capture.changes)? {
                        Interrupted::Installed => {
                            store.recover_install(ended, entry, InstallOutcome::Installed)?;
                            Outcome::Recovered(
                                "every changed path held the candidate: installed, and still \
                                 provisional"
                                    .into(),
                            )
                        }
                        Interrupted::Restored => {
                            store.recover_install(ended, entry, InstallOutcome::Failed)?;
                            Outcome::Recovered(
                                "every changed path holds what it held before: nothing of \
                                 the candidate stays"
                                    .into(),
                            )
                        }
                        Interrupted::Drifted(paths) => Outcome::Blocked(format!(
                            "{paths:?} hold neither the candidate nor what they held before, \
                             so nothing was written: they, and the generation's ownership, \
                             stay as they are"
                        )),
                    }
                }
                ActedOn::Verification(_) => {
                    store.interrupt_verification(ended, entry)?;
                    Outcome::Recovered("interrupted: no judgment of the candidate".into())
                }
                ActedOn::Integration(_) => {
                    store.interrupt_integration(ended, entry)?;
                    Outcome::Recovered(
                        "interrupted: no judgment, and the plan is not completed".into(),
                    )
                }
                ActedOn::Replan => {
                    store.interrupt_replan(ended, entry)?;
                    Outcome::Recovered("interrupted: nothing it proposed was applied".into())
                }
                ActedOn::Other(action) => {
                    Outcome::Blocked(format!("recovery cannot establish what `{action}` did"))
                }
            })
        };
        settled().unwrap_or_else(|e| Outcome::Blocked(format!("{e:#}")))
    }

    /// Withdraws the intended action `entry`, never attempted.
    fn intended(&mut self, entry: JournalId) -> Outcome {
        let mut settled = || -> Result<Outcome> {
            if let Some(invocation) = self.waits_on(entry)? {
                return Ok(Outcome::Blocked(format!(
                    "its agent's invocation {invocation} is unresolved"
                )));
            }
            self.store.withdraw(self.ended, entry)?;
            Ok(Outcome::Recovered("never attempted: withdrawn".into()))
        };
        settled().unwrap_or_else(|e| Outcome::Blocked(format!("{e:#}")))
    }

    /// Finishes the acceptance of `generation` from the phase it reached.
    fn acceptance(&mut self, generation: GenerationId) -> Outcome {
        let mut settled = || -> Result<Outcome> {
            let task = self.store.task_of(generation)?;
            Ok(
                match acceptance::accept(self.project, self.store, task, generation)? {
                    Accepted::Completed(_) => Outcome::Recovered(
                        "completed as published: CodeGraph synchronized, the generation \
                         accepted and its ownership released"
                            .into(),
                    ),
                    Accepted::Incomplete { phase, reason } => {
                        Outcome::Blocked(format!("it stays {phase}: {reason}"))
                    }
                    other => Outcome::Blocked(format!("{other:?}")),
                },
            )
        };
        settled().unwrap_or_else(|e| Outcome::Blocked(format!("{e:#}")))
    }

    /// Releases the claim of `generation` once its pipeline's outcome is
    /// established.
    fn claim(&mut self, generation: GenerationId) -> Outcome {
        match self.store.release_claim(generation) {
            Ok(Release::Released(outcome) | Release::AlreadyReleased(outcome)) => {
                Outcome::Recovered(format!("released: {outcome}"))
            }
            Ok(Release::Retained) => Outcome::Blocked(
                "its pipeline still has work whose outcome is unknown, so it keeps its capacity"
                    .into(),
            ),
            Err(e) => Outcome::Blocked(format!("{e:#}")),
        }
    }

    /// An unresolved invocation of the agent of `entry`, if it has one.
    fn waits_on(&self, entry: JournalId) -> Result<Option<InvocationId>> {
        let agent = self.store.journal_entry(entry)?.agent;
        Ok(self
            .store
            .invocations(agent)?
            .into_iter()
            .map(|i| i.id)
            .find(|id| self.unsettled.contains(id)))
    }
}

#[cfg(test)]
pub(crate) mod tests {
    /// The environment variable naming the failpoint (see `failpoint!`) at
    /// which a test's child process is to end.
    pub(crate) const FAILPOINT: &str = "AGENTCTL_TEST_FAILPOINT";

    /// Ends this process at once if its test asked it to end at `name`: no
    /// destructor runs, as if agentctl were killed there.
    pub(crate) fn failpoint(name: &str) {
        if std::env::var(FAILPOINT).as_deref() == Ok(name) {
            #[cfg(unix)]
            // SAFETY: `_exit` ends the process; nothing after it runs.
            unsafe {
                libc::_exit(crashes::ENDED)
            };
            #[cfg(not(unix))]
            std::process::abort();
        }
    }

    /// Real interruptions, by child processes of this test binary that
    /// end at failpoints, of work through the blocks that do it, with a
    /// fake provider whose process trees are POSIX ones.
    #[cfg(unix)]
    mod crashes {
        use std::env;
        use std::fs;
        use std::num::NonZeroU32;
        use std::os::unix::fs::PermissionsExt;
        use std::path::{Path, PathBuf};
        use std::process::{Command, Stdio};
        use std::time::Instant;

        use tempfile::TempDir;

        use super::super::*;
        use super::FAILPOINT;
        use crate::graph::tests::Fixture;
        use crate::integration;
        use crate::planner::{self, Command as Plan};
        use crate::scheduler::{self, tests::project_in};
        use crate::state::tests::ready_plan;
        use crate::state::{
            AcceptancePhase, ActionOutcome, ActionStatus, Decided, ExecutionOutcome,
            GenerationState, HumanDecision, Install, IntegrationOutcome, IntegrationStatus,
            InvocationState, PipelineOutcome, PlanState, Replan, TaskId, VerificationOutcome,
            VerificationStatus,
        };

        /// How a child process ended at its failpoint.
        pub(super) const ENDED: i32 = 86;
        /// What a child process of this test binary does, and where.
        const CHILD: &str = "AGENTCTL_TEST_RECOVERY_CHILD";
        const ROOT: &str = "AGENTCTL_TEST_RECOVERY_ROOT";
        const PLAN: &str = "AGENTCTL_TEST_RECOVERY_PLAN";
        const PROVIDER_DIR: &str = "AGENTCTL_TEST_RECOVERY_PROVIDER";

        /// A fake provider for every role, told what to do by files beside it.
        /// Whatever it starts ends within 30 seconds on its own.
        const PROVIDER: &str = r#"#!/bin/sh
dir=$(dirname "$0")
cat > /dev/null
case "$*" in
  *modified_paths*) role=executor ;;
  *propose_completion*) role=planner ;;
  *) role=verifier ;;
esac
mode=$(cat "$dir/$role-mode" 2>/dev/null)
case $mode in
  tree)
    (sleep 30 & echo $! > "$dir/grandchild.tmp"; mv "$dir/grandchild.tmp" "$dir/grandchild"
     exec sleep 30) &
    echo $! > "$dir/child.tmp"; mv "$dir/child.tmp" "$dir/child"
    echo $$ > "$dir/leader.tmp"; mv "$dir/leader.tmp" "$dir/leader"
    exec sleep 30 ;;
  hang) exec sleep 30 ;;
esac
result() {
  printf '{"type":"result","subtype":"success","is_error":false,"session_id":"fake","structured_output":%s,"usage":{"input_tokens":1,"output_tokens":1}}\n' "$1"
}
case $role in
  executor)
    if [ -f "$dir/edits" ]; then . "$dir/edits"; fi
    result '{"status":"succeeded","summary":"done","modified_paths":[]}' ;;
  verifier)
    result '{"verdict":"pass","checked":[{"check":"tests","command":null,"outcome":"passed","evidence":"ok"}],"blockers":[],"non_blocking":[]}' ;;
  *) exit 3 ;;
esac
"#;

        /// Acts only as the child process [`Harness::crash`] starts: runs its
        /// scenario against its project until its failpoint ends it.
        #[test]
        fn child() {
            let (Ok(scenario), Some(root)) = (env::var(CHILD), env::var_os(ROOT)) else {
                return;
            };
            let project = Project::load(Path::new(&root)).unwrap();
            let plan: PlanId = env::var(PLAN).unwrap().parse().unwrap();
            let fake = Some(PathBuf::from(env::var_os(PROVIDER_DIR).unwrap()).join("provider"));
            let mut store = project.hydrate().unwrap();
            match scenario.as_str() {
                "run" => {
                    let report = scheduler::run(&project, plan, fake).unwrap();
                    eprintln!("{report:?}");
                }
                "replan" => {
                    let planner = planner::replan(&project, &mut store, plan, fake).unwrap();
                    planner.finish(&project, &mut store).unwrap();
                }
                "verify" => {
                    integration::verify(&project, &mut store, plan, fake).unwrap();
                }
                "recover" => {
                    let settled = recover_with(&project, &mut store, &|_| Lifecycle::Gone);
                    eprintln!("{:?}", settled.unwrap());
                }
                other => panic!("unknown scenario {other}"),
            }
            panic!("{scenario} never reached its failpoint");
        }

        fn limit() -> NonZeroU32 {
            NonZeroU32::new(4).unwrap()
        }

        /// A project with a ready plan and a fake provider for its agents.
        struct Harness {
            fx: Fixture,
            plan: PlanId,
            tasks: Vec<TaskId>,
            provider: TempDir,
        }

        impl Harness {
            /// A plan of `tasks` `(key, scope, depends_on)`, every path of which
            /// is accepted source but those `absent`.
            fn new(tasks: &[(&str, &[&str], &[&str])], absent: &[&str]) -> Self {
                let (fx, plan, tasks) = project_in("src", tasks, absent);
                let provider = tempfile::tempdir().unwrap();
                let script = provider.path().join("provider");
                fs::write(&script, PROVIDER).unwrap();
                fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
                Self {
                    fx,
                    plan,
                    tasks,
                    provider,
                }
            }

            /// One task, of `src/a.rs`.
            fn one() -> Self {
                Self::new(&[("t", &["src/a.rs"], &[])], &[])
            }

            fn fake(&self) -> Option<PathBuf> {
                Some(self.provider.path().join("provider"))
            }

            /// Makes the fake `role` act as `mode` says: `tree`, starting a
            /// child and a grandchild, or `hang`.
            fn mode(&self, role: &str, mode: &str) {
                fs::write(self.provider.path().join(format!("{role}-mode")), mode).unwrap();
            }

            /// What the fake executor runs in its workspace.
            fn edits(&self, script: &str) {
                fs::write(self.provider.path().join("edits"), script).unwrap();
            }

            /// Runs `scenario` in a child agentctl process, which must end at
            /// failpoint `at`, within a minute.
            fn crash(&self, scenario: &str, at: &str) {
                self.crash_with(scenario, at, &[]);
            }

            fn crash_with(&self, scenario: &str, at: &str, vars: &[(&str, String)]) {
                let log = self.provider.path().join(format!("{scenario}-{at}.log"));
                let mut child = Command::new(env::current_exe().unwrap())
                    .args(["recovery::tests::crashes::child", "--exact", "--nocapture"])
                    .env(CHILD, scenario)
                    .env(ROOT, &self.fx.project.root)
                    .env(PLAN, self.plan.to_string())
                    .env(PROVIDER_DIR, self.provider.path())
                    .env(FAILPOINT, at)
                    .envs(vars.iter().map(|(k, v)| (k, v)))
                    .stdin(Stdio::null())
                    .stdout(fs::File::create(&log).unwrap())
                    .stderr(fs::File::create(log.with_extension("err")).unwrap())
                    .spawn()
                    .unwrap();
                let deadline = Instant::now() + Duration::from_secs(60);
                let status = loop {
                    if let Some(status) = child.try_wait().unwrap() {
                        break status;
                    }
                    if Instant::now() > deadline {
                        let _ = child.kill();
                        let _ = child.wait();
                        panic!("{scenario} did not end at {at} in time");
                    }
                    thread::sleep(Duration::from_millis(20));
                };
                let output = fs::read_to_string(log.with_extension("err")).unwrap_or_default();
                assert_eq!(status.code(), Some(ENDED), "{scenario} at {at}: {output}");
            }

            /// Recovers with every interrupted invocation's lifecycle
            /// injected as settled: nothing here can prove one.
            fn recover(&mut self) -> Report {
                self.recover_with(&|_| Lifecycle::Gone)
            }

            fn recover_with(&mut self, settle: Settle) -> Report {
                let report = recover_with(&self.fx.project, &mut self.fx.store, settle).unwrap();
                for item in &report.items {
                    println!("{}: {}", item.subject, item.outcome);
                }
                report
            }

            fn read(&self, path: &str) -> Option<String> {
                fs::read_to_string(self.fx.project.root.join(path)).ok()
            }

            fn write(&self, path: &str, content: &str) {
                fs::write(self.fx.project.root.join(path), content).unwrap();
            }

            /// The latest generation of the first task.
            fn generation(&self) -> GenerationId {
                let generations = self.fx.store.generations(self.tasks[0]).unwrap();
                generations.last().unwrap().id
            }

            fn claim(&self) -> Option<PipelineOutcome> {
                let generation = self.generation();
                let claims = self.fx.store.claims().unwrap();
                let claim = claims.iter().find(|c| c.generation == generation).unwrap();
                claim.released.map(|(outcome, _)| outcome)
            }

            fn execution(&self) -> ExecutionStatus {
                let execution = self.fx.store.execution(self.generation()).unwrap();
                execution.unwrap().status
            }

            fn capture(&self) -> crate::state::Capture {
                match self.execution() {
                    ExecutionStatus::Captured(capture) => capture,
                    other => panic!("not captured: {other:?}"),
                }
            }

            fn owned(&self) -> Vec<String> {
                self.fx.store.owned_paths(self.generation()).unwrap()
            }

            fn refused(&self, sql: &str, expected: &str) {
                let message = self
                    .fx
                    .store
                    .raw()
                    .execute_batch(sql)
                    .unwrap_err()
                    .to_string();
                assert!(message.contains(expected), "{sql}: {message}");
            }

            /// Whether running the plan now is refused, launching nothing.
            fn barred(&self) -> bool {
                match scheduler::run(&self.fx.project, self.plan, self.fake()) {
                    Err(e) => {
                        assert!(format!("{e:#}").contains("recovery required"), "{e:#}");
                        true
                    }
                    Ok(_) => false,
                }
            }
        }

        fn outcome(report: &Report, subject: impl Fn(&Subject) -> bool) -> &Outcome {
            let found = report.items.iter().find(|i| subject(&i.subject));
            &found.unwrap_or_else(|| panic!("{report:?}")).outcome
        }

        fn recovered(outcome: &Outcome) -> &str {
            match outcome {
                Outcome::Recovered(what) => what,
                other => panic!("not recovered: {other:?}"),
            }
        }

        fn blocked(outcome: &Outcome) -> &str {
            match outcome {
                Outcome::Blocked(why) => why,
                other => panic!("not blocked: {other:?}"),
            }
        }

        fn is_invocation(s: &Subject) -> bool {
            matches!(s, Subject::Invocation(_))
        }

        fn is_action(action: &'static str) -> impl Fn(&Subject) -> bool {
            move |s| matches!(s, Subject::Action { action: a, .. } if a == action)
        }

        fn is_claim(s: &Subject) -> bool {
            matches!(s, Subject::Claim(_))
        }

        #[test]
        fn a_clean_project_has_nothing_to_recover_however_often() {
            let mut h = Harness::one();
            // A plan awaiting its human is no interrupted work: it stays so.
            let basis = h.fx.store.replan_basis(h.plan).unwrap();
            let raise = [Plan::RaiseAttention {
                concern: "scope".into(),
                reason: "unclear".into(),
                evidence: vec!["the task names no interface".into()],
                tasks: vec!["t".into()],
            }];
            let (project, store) = (&h.fx.project, &mut h.fx.store);
            let raised = planner::apply_replan(project, store, h.plan, &basis, &raise).unwrap();
            assert!(matches!(raised, Replan::Applied(_)));
            let attention = h.fx.store.attention(h.plan).unwrap();
            assert!(h.recover().clean());
            let events = h.fx.store.events_after(0, 100_000).unwrap();
            assert!(h.recover().clean());
            assert_eq!(h.fx.store.events_after(0, 100_000).unwrap(), events);
            assert_eq!(h.fx.store.attention(h.plan).unwrap(), attention);
            let plan = h.fx.store.plan(h.plan).unwrap();
            assert_eq!(plan.state, PlanState::NeedsAttention);
            assert_eq!(crate::state::tests::version(&h.fx.project.state_path()), 1);
        }

        #[test]
        fn an_action_never_attempted_is_withdrawn() {
            let mut h = Harness::one();
            h.crash("run", "execution.intended");
            assert_eq!(h.execution(), ExecutionStatus::Intended);
            assert!(h.barred());
            let report = h.recover();
            assert!(report.settled(), "{report:?}");
            let what = recovered(outcome(&report, is_action("executor.run")));
            assert!(what.contains("withdrawn"), "{what}");
            let entry =
                h.fx.store
                    .execution(h.generation())
                    .unwrap()
                    .unwrap()
                    .journal;
            let status = h.fx.store.journal_entry(entry).unwrap().status;
            assert!(
                matches!(status, ActionStatus::Withdrawn { .. }),
                "{status:?}"
            );
            // Nothing ran, so the pipeline ended executing nothing; the task is
            // for its planner now, its generation still owning its scope.
            assert_eq!(h.claim(), Some(PipelineOutcome::NotExecuted));
            assert_eq!(h.owned(), ["src/a.rs"]);
            assert_eq!(h.read("src/a.rs").as_deref(), Some("// accepted\n"));
            assert!(h.recover().clean());
            assert!(!h.barred());
        }

        #[test]
        fn an_attempt_whose_provider_never_launched_judges_nothing() {
            let mut h = Harness::one();
            h.crash("run", "execution.attempted");
            let report = h.recover();
            assert!(report.settled(), "{report:?}");
            assert!(recovered(outcome(&report, is_invocation)).contains("lifecycle was settled"));
            let capture = h.capture();
            assert_eq!(capture.outcome, ExecutionOutcome::Interrupted);
            assert!(capture.changes.is_empty());
            let invocation = h.fx.store.invocation(capture.invocation).unwrap();
            assert_eq!(invocation.state, InvocationState::Interrupted);
            assert_eq!(h.claim(), Some(PipelineOutcome::ExecutionFailed));
        }

        #[test]
        fn an_invocation_whose_lifecycle_is_unproven_stays_unresolved() {
            let mut h = Harness::one();
            h.mode("executor", "tree");
            h.crash("run", "execution.spawned");
            // Until then its authority stays: nothing new starts, the claim is
            // held, and the plan can still be read.
            assert!(h.barred());
            let why = h.fx.store.recovery_required(h.plan).unwrap().unwrap();
            assert!(why.contains("run `agentctl recover`"), "{why}");
            let fake = h.fake();
            assert!(planner::replan(&h.fx.project, &mut h.fx.store, h.plan, fake).is_err());
            assert_eq!(
                h.fx.store.release_claim(h.generation()).unwrap(),
                Release::Retained
            );
            assert_eq!(h.fx.store.plan(h.plan).unwrap().state, PlanState::Running);
            assert!(!h.fx.store.unresolved(Some(h.plan)).unwrap().is_empty());
            let snapshot = h.fx.store.snapshot(h.plan, limit()).unwrap();
            assert_eq!(
                snapshot.status(h.tasks[0]),
                Some(&crate::state::TaskStatus::Scheduled(h.generation()))
            );
            assert!(h.fx.store.attention(h.plan).unwrap().is_empty());
            let g = h.generation();
            h.refused(
                &format!(
                    "UPDATE journal SET session = (SELECT id FROM sessions LIMIT 1)
                          WHERE agent_id IN (SELECT id FROM agents WHERE generation_id = {g})"
                ),
                "journal history is immutable",
            );

            // Agentctl has no authority to settle it: whatever else is
            // settled, it stays as it was found, uncertain, and holds back
            // everything that waits on it.
            let report = recover_store(&h.fx.project, &mut h.fx.store).unwrap();
            assert!(!report.settled(), "{report:?}");
            let Outcome::Unsupported(why) = outcome(&report, is_invocation) else {
                panic!("{report:?}");
            };
            assert!(why.contains("no authority"), "{why}");
            assert!(blocked(outcome(&report, is_action("executor.run"))).contains("unresolved"));
            assert!(blocked(outcome(&report, is_claim)).contains("capacity"));
            assert!(!matches!(h.execution(), ExecutionStatus::Captured(_)));
            assert!(h.barred());
            assert_eq!(
                h.fx.store.release_claim(h.generation()).unwrap(),
                Release::Retained
            );
            let unresolved = h.fx.store.unresolved(Some(h.plan)).unwrap();
            let id = unresolved
                .iter()
                .find_map(|u| match u.kind {
                    UnresolvedKind::Invocation(id) => Some(id),
                    _ => None,
                })
                .unwrap();
            let invocation = h.fx.store.invocation(id).unwrap();
            assert_eq!(invocation.state, InvocationState::Running);
            assert!(invocation.end.is_none());
            // Said uncertain, it is settled only once something proves it.
            let uncertain = |_: InvocationId| Lifecycle::Uncertain("still there".into());
            assert!(!h.recover_with(&uncertain).settled());
            assert!(h.barred());

            let report = h.recover();
            assert!(report.settled(), "{report:?}");
            let what = recovered(outcome(&report, is_invocation));
            assert!(what.contains("lifecycle was settled"), "{what}");
            // The provider's disappearance judges nothing.
            let capture = h.capture();
            assert_eq!(capture.outcome, ExecutionOutcome::Interrupted);
            let invocation = h.fx.store.invocation(capture.invocation).unwrap();
            assert_eq!(invocation.state, InvocationState::Interrupted);
            assert!(
                invocation
                    .end
                    .unwrap()
                    .diagnostic
                    .unwrap()
                    .contains("unknown")
            );
            assert_eq!(h.claim(), Some(PipelineOutcome::ExecutionFailed));
            assert_eq!(h.owned(), ["src/a.rs"]);
            assert!(h.recover().clean());

            // Beneath the store, nothing makes it out to have succeeded.
            let id = capture.invocation;
            h.refused(
                &format!(
                    "UPDATE invocations SET state = 'succeeded', exit_code = 0 WHERE id = {id}"
                ),
                "invocation history is immutable",
            );
            h.refused(
                "UPDATE sessions SET started_at = 0",
                "sessions are immutable",
            );
            h.refused("DELETE FROM sessions", "sessions are immutable");
        }

        #[test]
        fn a_captured_candidate_never_installed_is_never_accepted() {
            let mut h = Harness::one();
            h.edits("printf 'changed\\n' > src/a.rs\n");
            h.crash("run", "execution.captured");
            assert_eq!(h.capture().outcome, ExecutionOutcome::Candidate);
            let report = h.recover();
            assert!(report.settled(), "{report:?}");
            assert_eq!(h.capture().install, Install::NotAttempted);
            assert_eq!(h.claim(), Some(PipelineOutcome::InstallFailed));
            assert_eq!(h.read("src/a.rs").as_deref(), Some("// accepted\n"));
            let accepted = h.fx.store.accepted_source("src/a.rs").unwrap().unwrap();
            assert_eq!(accepted.generation, None);
            assert_eq!(h.owned(), ["src/a.rs"]);
        }

        const WEIRD: &str = "src/c d[1]*.rs";

        /// Modifies `src/a.rs`, deletes `src/b.rs` and creates `WEIRD`.
        fn three_changes() -> Harness {
            let h = Harness::new(&[("t", &["src/a.rs", "src/b.rs", WEIRD], &[])], &[WEIRD]);
            h.edits(
                "printf 'changed\\n' > src/a.rs\nrm src/b.rs\nprintf 'created\\n' > 'src/c d[1]*.rs'\n",
            );
            h
        }

        fn untouched(h: &Harness) -> bool {
            h.read("src/a.rs").as_deref() == Some("// accepted\n")
                && h.read("src/b.rs").as_deref() == Some("// accepted\n")
                && h.read(WEIRD).is_none()
        }

        #[test]
        fn a_partial_install_is_restored_exactly_before_ownership_can_go() {
            let mut h = three_changes();
            h.crash("run", "install.written.1");
            assert_eq!(h.read("src/a.rs").as_deref(), Some("changed\n"));
            assert_eq!(h.read("src/b.rs").as_deref(), Some("// accepted\n"));
            assert_eq!(h.capture().install, Install::OutcomeUnknown);
            // Ownership stays while what the working tree holds is unknown.
            let g = h.generation();
            h.refused(
                &format!("DELETE FROM ownership WHERE generation_id = {g}"),
                "released only by accepting or abandoning",
            );
            assert_eq!(h.fx.store.release_claim(g).unwrap(), Release::Retained);

            let report = h.recover();
            assert!(report.settled(), "{report:?}");
            let what = recovered(outcome(&report, is_action("executor.install")));
            assert!(what.contains("held before"), "{what}");
            assert!(untouched(&h));
            let Install::Finished { outcome, .. } = h.capture().install else {
                panic!("not finished");
            };
            assert_eq!(outcome, InstallOutcome::Failed);
            assert_eq!(h.claim(), Some(PipelineOutcome::InstallFailed));
            assert_eq!(h.owned().len(), 3);
            assert!(h.recover().clean());
            // Only its planner lets the generation go now, as ever.
            let basis = h.fx.store.replan_basis(h.plan).unwrap();
            let retry = [Plan::RetryTask { task: "t".into() }];
            let replanned =
                planner::apply_replan(&h.fx.project, &mut h.fx.store, h.plan, &basis, &retry);
            assert!(matches!(replanned.unwrap(), Replan::Applied(_)));
            assert!(h.owned().is_empty());
            h.refused(
                &format!("UPDATE generations SET state = 'active', ended_at = NULL WHERE id = {g}"),
                "an ended generation stays ended",
            );
        }

        #[test]
        fn restoring_interrupted_itself_is_finished_by_the_next_recovery() {
            let mut h = three_changes();
            h.crash("run", "install.written.2");
            assert_eq!(h.read("src/b.rs"), None);
            h.crash("recover", "recovery.restored.1");
            // One path restored, the other not: still unresolved, still held.
            assert_eq!(h.read("src/a.rs").as_deref(), Some("// accepted\n"));
            assert_eq!(h.read("src/b.rs"), None);
            assert_eq!(h.capture().install, Install::OutcomeUnknown);
            assert!(h.barred());
            let report = h.recover();
            assert!(report.settled(), "{report:?}");
            assert!(untouched(&h));
            assert_eq!(h.claim(), Some(PipelineOutcome::InstallFailed));
        }

        #[test]
        fn unexplained_bytes_are_never_overwritten() {
            let mut h = three_changes();
            h.crash("run", "install.written.2");
            h.write("src/a.rs", "someone's work\n");
            let report = h.recover();
            assert!(!report.settled());
            let why = blocked(outcome(&report, is_action("executor.install")));
            assert!(why.contains("src/a.rs") && why.contains("neither"), "{why}");
            assert!(blocked(outcome(&report, is_claim)).contains("capacity"));
            // Nothing was written, and the evidence stays with the plan.
            assert_eq!(h.read("src/a.rs").as_deref(), Some("someone's work\n"));
            assert_eq!(h.read("src/b.rs"), None);
            assert_eq!(h.capture().install, Install::OutcomeUnknown);
            assert_eq!(h.owned().len(), 3);
            let events = h.fx.store.events_after(0, 100_000).unwrap();
            assert!(events.iter().any(|e| e.kind == "recovery.blocked"));
            assert!(h.barred());
            // Finding it so again establishes, and records, nothing new.
            assert!(!h.recover().settled());
            assert_eq!(h.fx.store.events_after(0, 100_000).unwrap(), events);
            // Once the path holds what the install left there, it is settled.
            h.write("src/a.rs", "changed\n");
            let report = h.recover();
            assert!(report.settled(), "{report:?}");
            assert!(untouched(&h));
        }

        #[test]
        fn a_complete_install_stays_installed_and_provisional() {
            let mut h = three_changes();
            h.crash("run", "install.applied");
            let report = h.recover();
            assert!(report.settled(), "{report:?}");
            let Install::Finished { outcome, .. } = h.capture().install else {
                panic!("not finished");
            };
            assert_eq!(outcome, InstallOutcome::Installed);
            assert_eq!(h.read("src/a.rs").as_deref(), Some("changed\n"));
            assert_eq!(h.read(WEIRD).as_deref(), Some("created\n"));
            assert_eq!(h.claim(), Some(PipelineOutcome::VerificationInconclusive));
            let accepted = h.fx.store.accepted_source("src/a.rs").unwrap().unwrap();
            assert_eq!(accepted.generation, None);
        }

        #[test]
        fn an_interrupted_verifier_never_judges() {
            let mut h = Harness::one();
            h.edits("printf 'changed\\n' > src/a.rs\n");
            h.mode("verifier", "tree");
            h.crash("run", "verification.spawned");
            let report = h.recover();
            assert!(report.settled(), "{report:?}");
            let verifications = h.fx.store.verifications(h.generation()).unwrap();
            let VerificationStatus::Finished(result) = &verifications[0].status else {
                panic!("not finished: {verifications:?}");
            };
            assert_eq!(result.outcome, VerificationOutcome::Interrupted);
            assert_eq!(h.claim(), Some(PipelineOutcome::VerificationInconclusive));
            assert!(h.fx.store.acceptance(h.generation()).unwrap().is_none());
            let id = verifications[0].id;
            h.refused(
                &format!(
                    "INSERT INTO verification_results (verification_id, outcome, verdict, checked,
                       blockers, non_blocking, finished_at)
                     VALUES ({id}, 'passed', 'pass', '[{{\"outcome\":\"passed\"}}]', '[]', '[]', 0)"
                ),
                "follows its attempt",
            );
        }

        /// The phases the acceptance of the first task's generation reached.
        fn phase(h: &Harness) -> Option<AcceptancePhase> {
            let acceptance = h.fx.store.acceptance(h.generation()).unwrap();
            acceptance.map(|a| a.phase)
        }

        #[test]
        fn a_published_acceptance_is_finished_in_order_before_ownership_goes() {
            for (at, reached) in [
                ("acceptance.published", AcceptancePhase::Published),
                ("acceptance.synchronized", AcceptancePhase::Synchronized),
            ] {
                let mut h = Harness::one();
                h.edits("printf 'pub fn changed() {}\\n' > src/a.rs\n");
                h.crash("run", at);
                assert_eq!(phase(&h), Some(reached));
                assert_eq!(h.owned(), ["src/a.rs"]);
                let g = h.generation();
                h.refused(
                    &format!("INSERT INTO acceptance_phases VALUES ({g}, 'completed', 0)"),
                    "acceptance",
                );
                let report = h.recover();
                assert!(report.settled(), "{report:?}");
                assert_eq!(phase(&h), Some(AcceptancePhase::Completed));
                assert!(h.owned().is_empty());
                assert_eq!(h.claim(), Some(PipelineOutcome::Accepted));
                let accepted = h.fx.store.accepted_source("src/a.rs").unwrap().unwrap();
                assert_eq!(accepted.generation, Some(g));
                let generations = h.fx.store.generations(h.tasks[0]).unwrap();
                assert_eq!(generations[0].state, GenerationState::Accepted);
                let kinds: Vec<String> =
                    h.fx.store
                        .events_after(0, 100_000)
                        .unwrap()
                        .into_iter()
                        .map(|e| e.kind)
                        .filter(|k| k.starts_with("acceptance.") || k == "ownership.released")
                        .collect();
                assert_eq!(
                    kinds,
                    [
                        "acceptance.published",
                        "acceptance.synchronized",
                        "ownership.released",
                        "acceptance.completed"
                    ]
                );
                assert!(h.recover().clean());
            }
        }

        #[test]
        fn a_recovery_interrupted_while_accepting_is_finished_by_the_next() {
            let mut h = Harness::one();
            h.edits("printf 'pub fn changed() {}\\n' > src/a.rs\n");
            h.crash("run", "acceptance.published");
            h.crash("recover", "acceptance.synchronized");
            assert_eq!(phase(&h), Some(AcceptancePhase::Synchronized));
            assert_eq!(h.owned(), ["src/a.rs"]);
            assert_eq!(h.claim(), None);
            let report = h.recover();
            assert!(report.settled(), "{report:?}");
            assert_eq!(phase(&h), Some(AcceptancePhase::Completed));
            assert_eq!(h.claim(), Some(PipelineOutcome::Accepted));
        }

        #[test]
        fn a_recovery_interrupted_before_releasing_claims_or_recording_ends_is_finished() {
            let mut h = Harness::one();
            h.crash("run", "execution.intended");
            h.crash("recover", "recovery.claims");
            assert_eq!(h.claim(), None);
            let report = h.recover();
            assert!(report.settled(), "{report:?}");
            assert_eq!(h.claim(), Some(PipelineOutcome::NotExecuted));

            let mut h = Harness::one();
            h.mode("executor", "tree");
            h.crash("run", "execution.spawned");
            h.crash("recover", "recovery.terminated");
            let report = h.recover();
            assert!(report.settled(), "{report:?}");
            assert!(recovered(outcome(&report, is_invocation)).contains("lifecycle was settled"));
            assert_eq!(h.claim(), Some(PipelineOutcome::ExecutionFailed));
        }

        #[test]
        fn an_interrupted_replan_applies_nothing_and_decisions_stand() {
            let mut h = Harness::one();
            let basis = h.fx.store.replan_basis(h.plan).unwrap();
            let raise = [Plan::RaiseAttention {
                concern: "scope".into(),
                reason: "unclear".into(),
                evidence: vec!["the task names no interface".into()],
                tasks: vec!["t".into()],
            }];
            let (project, store) = (&h.fx.project, &mut h.fx.store);
            let raised = planner::apply_replan(project, store, h.plan, &basis, &raise).unwrap();
            assert!(matches!(raised, Replan::Applied(_)));
            let concern = h.fx.store.attention(h.plan).unwrap()[0].id;
            let instruct = HumanDecision::Instruct("keep it small".into());
            let decided = h.fx.store.decide(h.plan, concern, &instruct).unwrap();
            assert_eq!(decided, Decided::Recorded { resumed: false });
            let before = h.fx.store.attention(h.plan).unwrap();
            let tasks = h.fx.store.tasks(h.plan).unwrap();

            h.mode("planner", "hang");
            h.crash("replan", "replan.spawned");
            assert!(
                h.fx.store
                    .decide(h.plan, concern, &HumanDecision::Stop)
                    .is_err()
            );
            let report = h.recover();
            assert!(report.settled(), "{report:?}");
            let what = recovered(outcome(&report, is_action("planner.replan")));
            assert!(what.contains("nothing it proposed"), "{what}");
            // Nothing applied, nothing decided or resolved anew: the
            // instruction still awaits a planner.
            assert_eq!(h.fx.store.attention(h.plan).unwrap(), before);
            assert_eq!(h.fx.store.tasks(h.plan).unwrap(), tasks);
            let plan = h.fx.store.plan(h.plan).unwrap();
            assert_eq!(plan.state, PlanState::NeedsAttention);
            let planner = h.fx.store.planner(h.plan).unwrap();
            let entries = h.fx.store.continuation(planner).unwrap();
            let ActionStatus::Reconciled(_, reconciliation) = &entries.last().unwrap().status
            else {
                panic!("{entries:?}");
            };
            assert_eq!(reconciliation.outcome, ActionOutcome::Failed);
            assert_eq!(
                h.fx.store.decide(h.plan, concern, &instruct).unwrap(),
                Decided::AlreadyRecorded
            );
            assert!(h.fx.store.recovery_required(h.plan).unwrap().is_none());
        }

        #[test]
        fn an_interrupted_integration_verification_completes_nothing() {
            let mut h = Harness::one();
            h.edits("printf 'pub fn changed() {}\\n' > src/a.rs\n");
            let report = scheduler::run(&h.fx.project, h.plan, h.fake()).unwrap();
            assert_eq!(
                report.snapshot.condition(),
                crate::state::Condition::AllCompleted
            );
            let basis = h.fx.store.replan_basis(h.plan).unwrap();
            let propose = [Plan::ProposeCompletion {}];
            let (project, store) = (&h.fx.project, &mut h.fx.store);
            let proposed = planner::apply_replan(project, store, h.plan, &basis, &propose).unwrap();
            assert!(matches!(proposed, Replan::Applied(_)));

            h.mode("verifier", "tree");
            h.crash("verify", "integration.spawned");
            let report = h.recover();
            assert!(report.settled(), "{report:?}");
            let integrations = h.fx.store.integrations(h.plan).unwrap();
            let IntegrationStatus::Finished(result) = &integrations[0].status else {
                panic!("{integrations:?}");
            };
            assert_eq!(result.outcome, IntegrationOutcome::Interrupted);
            assert_eq!(h.fx.store.plan(h.plan).unwrap().state, PlanState::Running);
            let id = integrations[0].id;
            let plan = h.plan;
            h.refused(
                &format!(
                    "INSERT INTO plan_completions (plan_id, verification_id, completed_at)
                          VALUES ({plan}, {id}, 0)"
                ),
                "completes it",
            );

            // A fresh verification is eligible as ever, and its pass completes
            // the plan, which recovery then leaves completed.
            fs::remove_file(h.provider.path().join("verifier-mode")).unwrap();
            let fake = h.fake();
            integration::verify(&h.fx.project, &mut h.fx.store, h.plan, fake).unwrap();
            assert_eq!(h.fx.store.plan(h.plan).unwrap().state, PlanState::Completed);
            assert!(h.recover().clean());
            assert_eq!(h.fx.store.plan(h.plan).unwrap().state, PlanState::Completed);
        }

        #[test]
        fn interrupted_work_holds_back_only_its_own_plan() {
            let mut h = Harness::one();
            h.mode("executor", "tree");
            h.crash("run", "execution.spawned");
            fs::remove_file(h.provider.path().join("executor-mode")).unwrap();
            let (other, _) = ready_plan(&mut h.fx.store, &[("q", &["src/q.rs"], &[])]);
            h.edits("printf 'pub fn q() {}\\n' > src/q.rs\n");
            assert!(h.fx.store.recovery_required(other).unwrap().is_none());
            let report = scheduler::run(&h.fx.project, other, h.fake()).unwrap();
            assert_eq!(
                report.snapshot.condition(),
                crate::state::Condition::AllCompleted
            );
            assert!(h.barred());
            assert!(h.recover().settled());
        }

        #[test]
        fn withdrawals_are_final_and_only_of_intended_entries() {
            let h = Harness::one();
            h.crash("run", "execution.attempted");
            let entry =
                h.fx.store
                    .execution(h.generation())
                    .unwrap()
                    .unwrap()
                    .journal;
            h.refused(
                &format!("INSERT INTO journal_withdrawals VALUES ({entry}, 0)"),
                "only an intended entry is withdrawn",
            );
            let mut h = Harness::one();
            h.crash("run", "execution.intended");
            assert!(h.recover().settled());
            let entry =
                h.fx.store
                    .execution(h.generation())
                    .unwrap()
                    .unwrap()
                    .journal;
            h.refused(
                &format!(
                    "UPDATE journal SET state = 'attempted', attempted_at = 1 WHERE id = {entry}"
                ),
                "never acted on",
            );
            h.refused("DELETE FROM journal_withdrawals", "immutable");
            h.refused(
                "UPDATE journal_withdrawals SET withdrawn_at = 1",
                "immutable",
            );
        }
    }
}
