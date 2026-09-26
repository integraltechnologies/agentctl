//! Acceptance: turning the exact candidate an independent verification
//! passed into accepted repository state.
//!
//! A pass changes nothing by itself. Accepting acts on it in phases, each
//! recorded durably before the next begins (see [`AcceptancePhase`]):
//!
//! 1. The candidate is established acceptable again from canonical state
//!    alone: its generation active and owning everything its execution and
//!    task require, its capture, install and latest verification reconciled
//!    as a candidate, installed and passed. Every content it would publish
//!    must be an available recovery object whose bytes hash to its name.
//! 2. Its source is published: in one transaction, the working tree is
//!    observed to hold the candidate at every changed path, and only then
//!    are the acceptance and the candidate's captured identities recorded,
//!    as accepted source. Identities come from the capture, never from the
//!    bytes observed, which only decide whether to go ahead. Should the
//!    working tree hold anything else, nothing is recorded.
//! 3. CodeGraph is synchronized, in one transaction, with graphs derived
//!    from the published content, read from recovery objects: a changed
//!    path's graph is replaced, or removed when it is absent, no frontend
//!    derives one, or its frontend finds the content invalid.
//! 4. Only then, in one transaction, is the generation accepted, completing
//!    its task, and its whole ownership released.
//!
//! Failing before publication leaves nothing recorded: accepted source,
//! CodeGraph, ownership and the generation stay as they were, and the
//! installed candidate stays in the working tree. Failing after it leaves
//! the acceptance durably incomplete at the phase it reached: published
//! source stays accepted, its generation active and owning its paths, and
//! calling [`accept`] again resumes from there, never publishing anything
//! else. Nothing here launches an agent, restores the working tree, or
//! completes a plan.
//!
//! Observing the working tree and publishing are one database transaction,
//! which serializes them with every other agentctl writer, but not with
//! other processes writing files: one could still change a changed path
//! after it was observed and before the transaction commits. What is
//! published is the verified candidate either way; the working tree would
//! then have drifted from accepted source, as it may at any time.

use anyhow::{Context, Result};

use crate::graph::{self, Derivation};
use crate::project::Project;
use crate::source;
use crate::state::{
    Acceptance, AcceptancePhase, Content, GenerationId, Publication, Store, TaskId,
};

/// How a call to [`accept`] ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The candidate is accepted source, CodeGraph is synchronized with it,
    /// the generation is accepted and owns nothing.
    Completed(Acceptance),
    /// The working tree no longer holds the verified candidate at these
    /// paths, so nothing was accepted, and nothing in the working tree was
    /// touched.
    CandidateDrifted(Vec<String>),
    /// The recovery object of the candidate's content at `path` is missing
    /// or corrupt, so nothing was accepted.
    ObjectUnavailable {
        path: String,
        hash: String,
        reason: String,
    },
    /// The candidate may not be accepted now, for the reason given, so
    /// nothing was accepted.
    InvalidPrecondition(String),
    /// The acceptance stopped after reaching `phase`, which stays durable:
    /// its source is accepted, while the generation stays active, owning
    /// its paths, until the acceptance is resumed.
    Incomplete {
        phase: AcceptancePhase,
        reason: String,
    },
}

/// Accepts the verified candidate of `generation`, of `task`, or resumes
/// its acceptance from the phase it reached; see the module documentation.
/// Once completed, returns the acceptance as recorded, changing nothing.
/// An error before publication leaves nothing recorded.
pub fn accept(
    project: &Project,
    store: &mut Store,
    task: TaskId,
    generation: GenerationId,
) -> Result<Outcome> {
    accept_with(project, store, task, generation, |_| Ok(()))
}

/// [`accept`], calling `deriving` with each changed path before deriving
/// its graph, which fails synchronizing if it fails.
pub(crate) fn accept_with(
    project: &Project,
    store: &mut Store,
    task: TaskId,
    generation: GenerationId,
    deriving: impl FnMut(&str) -> Result<()>,
) -> Result<Outcome> {
    let phase = match store.acceptance(generation)? {
        None => AcceptancePhase::Intended,
        Some(acceptance) => {
            let belongs = store.generations(task)?.iter().any(|g| g.id == generation);
            if !belongs {
                return Ok(Outcome::InvalidPrecondition(format!(
                    "generation {generation} does not belong to task {task}"
                )));
            }
            acceptance.phase
        }
    };
    if phase == AcceptancePhase::Intended
        && let Some(declined) = publish(project, store, task, generation)?
    {
        return Ok(declined);
    }
    if matches!(
        phase,
        AcceptancePhase::Intended | AcceptancePhase::Published
    ) && let Err(e) = synchronize(project, store, generation, deriving)
    {
        return Ok(Outcome::Incomplete {
            phase: AcceptancePhase::Published,
            reason: format!("synchronizing CodeGraph failed: {e:#}"),
        });
    }
    if phase != AcceptancePhase::Completed
        && let Err(e) = store.complete_acceptance(generation)
    {
        return Ok(Outcome::Incomplete {
            phase: AcceptancePhase::Synchronized,
            reason: format!("completing the acceptance failed: {e:#}"),
        });
    }
    let acceptance = store
        .acceptance(generation)?
        .context("the acceptance vanished")?;
    Ok(Outcome::Completed(acceptance))
}

/// Publishes the candidate's source, unless it is not to be accepted now,
/// and why not.
fn publish(
    project: &Project,
    store: &mut Store,
    task: TaskId,
    generation: GenerationId,
) -> Result<Option<Outcome>> {
    let acceptable = match store.acceptable(task, generation)? {
        Ok(acceptable) => acceptable,
        Err(why) => return Ok(Some(Outcome::InvalidPrecondition(why))),
    };
    // Accepted source may name only content that is durably recoverable.
    for (path, content) in &acceptable.candidate {
        if let Content::File(hash) = content
            && let Err(e) = source::check_object(project, hash)
        {
            return Ok(Some(Outcome::ObjectUnavailable {
                path: path.clone(),
                hash: hash.clone(),
                reason: format!("{e:#}"),
            }));
        }
    }
    source::sync_objects(project)?;
    let observe = |paths: &[String]| source::observe_paths(project, paths);
    Ok(match store.publish_acceptance(task, generation, observe)? {
        Publication::Published => None,
        Publication::Drifted(paths) => Some(Outcome::CandidateDrifted(paths)),
        Publication::Refused(why) => Some(Outcome::InvalidPrecondition(why)),
    })
}

/// Derives the graph of every changed path from its published content and
/// makes CodeGraph hold exactly those, in one transaction.
fn synchronize(
    project: &Project,
    store: &mut Store,
    generation: GenerationId,
    mut deriving: impl FnMut(&str) -> Result<()>,
) -> Result<()> {
    let acceptance = store
        .acceptance(generation)?
        .context("the acceptance vanished")?;
    let mut contributions = Vec::new();
    for change in acceptance.changes.iter().filter(|c| c.hash.is_some()) {
        deriving(&change.path)?;
        match graph::derive(project, store, &change.path)? {
            Derivation::Indexed(c) => contributions.push(c),
            Derivation::Unsupported | Derivation::Declined => {}
        }
    }
    store.synchronize_acceptance(generation, &contributions)
}

#[cfg(test)]
pub(crate) mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::thread;
    use std::time::Duration;

    use rusqlite::types::Value;
    use sha2::{Digest, Sha256};

    use super::*;
    use crate::executor;
    use crate::graph::tests::Fixture;
    use crate::graph::{Entity, Freshness};
    use crate::project::{STATE_DB, STATE_DIR};
    use crate::source::Workspace;
    use crate::state::tests::{acquire, ended, err, ready_plan, succeeded};
    use crate::state::{
        AcceptedChange, Acquisition, Check, CheckOutcome, ExecutionId, ExecutionStatus,
        ExecutorResult, FailureKind, GenerationEnd, GenerationState, InvocationEnd,
        InvocationState, Observed, PlanId, PlanState, Reported, TaskState, Verdict,
        VerificationOutcome, VerificationStatus, VerifierObserved, VerifierReport, VerifierResult,
    };

    const HEAD: &str = "unused";

    fn sha256(bytes: &str) -> String {
        Sha256::digest(bytes.as_bytes())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    fn since() -> i64 {
        thread::sleep(Duration::from_millis(2));
        crate::state::now()
    }

    /// How a verification of the candidate ends.
    #[derive(Clone, Copy)]
    pub(crate) enum Judged {
        Pass,
        Fail,
        Malformed,
        /// A pass, once the working tree stopped holding the candidate.
        Changed,
        /// A pass by a verifier that changed repository source.
        Violated,
        InvocationFailed,
        /// Attempted, with the verifier still running.
        Unresolved,
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

    /// A project whose `accepted` sources (Rust ones indexed) are accepted,
    /// and a generation of task `t`, owning `scope`, whose executor made
    /// `edits` in its workspace (`None` deletes): captured as a candidate
    /// and installed in the working tree, not yet verified. Task `next`
    /// depends on `t`.
    pub(crate) struct Case {
        pub(crate) fx: Fixture,
        pub(crate) plan: PlanId,
        pub(crate) task: TaskId,
        pub(crate) next: TaskId,
        pub(crate) generation: GenerationId,
        execution: ExecutionId,
    }

    impl Case {
        pub(crate) fn installed(
            accepted: &[(&str, &str)],
            scope: &[&str],
            edits: &[(&str, Option<&str>)],
        ) -> Self {
            let mut fx = Fixture::new("src");
            for (path, content) in accepted {
                write(&fx.project.root, path, content);
            }
            source::baseline(&fx.project, &mut fx.store).unwrap();
            for (path, _) in accepted.iter().filter(|(p, _)| p.ends_with(".rs")) {
                graph::rust::index(&fx.project, &mut fx.store, path).unwrap();
            }
            let (plan, tasks) = ready_plan(
                &mut fx.store,
                &[("t", scope, &[]), ("next", &["src/next.rs"], &["t"])],
            );
            let task = tasks[0];
            let generation = fx.store.start_generation(task).unwrap();
            acquire(&mut fx.store, generation, scope);
            let authority = fx.store.execution_authority(task, generation).unwrap();
            let since = since();
            let baseline = source::snapshot(&fx.project, &authority).unwrap();
            let workspace = Workspace::stage(&fx.project, &baseline).unwrap();
            let (execution, agent, entry) = fx
                .store
                .begin_execution(task, generation, &authority, &baseline.entries, HEAD, since)
                .unwrap();
            let invocation = fx
                .store
                .start_invocation(agent, "claude", "m", None)
                .unwrap();
            fx.store.act(entry, Some(invocation)).unwrap();
            fx.store.invocation_running(invocation).unwrap();
            fx.store
                .finish_invocation(invocation, &succeeded())
                .unwrap();
            for (path, content) in edits {
                match content {
                    Some(content) => write(workspace.root(), path, content),
                    None => fs::remove_file(workspace.root().join(path)).unwrap(),
                }
            }
            let paths: Vec<String> = baseline.entries.iter().map(|(p, _)| p.clone()).collect();
            let observed = workspace.observe(&fx.project, &paths).unwrap();
            let observed = Observed {
                entries: &observed.entries,
                head: HEAD,
                settled: true,
                result: ExecutorResult::Reported {
                    status: Reported::Succeeded,
                    claimed: &authority,
                },
            };
            fx.store.finish_execution(execution, &observed).unwrap();
            let Some(ExecutionStatus::Captured(capture)) =
                fx.store.execution(generation).unwrap().map(|e| e.status)
            else {
                panic!("not captured");
            };
            assert_eq!(capture.changes.len(), edits.len());
            let installed =
                executor::install(&fx.project, &mut fx.store, execution, &capture, &workspace);
            assert_eq!(installed.unwrap(), None, "installed");
            Self {
                fx,
                plan,
                task,
                next: tasks[1],
                generation,
                execution,
            }
        }

        /// [`Case::installed`], verified as passed.
        pub(crate) fn passed(
            accepted: &[(&str, &str)],
            scope: &[&str],
            edits: &[(&str, Option<&str>)],
        ) -> Self {
            let mut case = Self::installed(accepted, scope, edits);
            case.verify(Judged::Pass);
            case
        }

        pub(crate) fn verify(&mut self, judged: Judged) {
            let store = &mut self.fx.store;
            let candidate = store
                .verification_candidate(self.task, self.generation)
                .unwrap();
            let paths: Vec<String> = candidate.iter().map(|(p, _)| p.clone()).collect();
            let observed = source::observe_paths(&self.fx.project, &paths).unwrap();
            let (verification, agent, entry) = store
                .begin_verification(self.task, self.generation, &observed, since())
                .unwrap();
            let invocation = store.start_invocation(agent, "claude", "m", None).unwrap();
            store.act(entry, Some(invocation)).unwrap();
            store.invocation_running(invocation).unwrap();
            let (end, report) = match judged {
                Judged::Unresolved => return,
                Judged::InvocationFailed => (
                    InvocationEnd {
                        failure: Some(FailureKind::ExitStatus),
                        diagnostic: Some("failed".into()),
                        exit_code: Some(1),
                        ..ended(InvocationState::Failed)
                    },
                    None,
                ),
                Judged::Pass | Judged::Changed | Judged::Violated => {
                    (succeeded(), Some(report(Verdict::Pass)))
                }
                Judged::Fail => (succeeded(), Some(report(Verdict::Fail))),
                Judged::Malformed => (succeeded(), None),
            };
            store.finish_invocation(invocation, &end).unwrap();
            let result = match (&report, judged) {
                (Some(report), _) => VerifierResult::Reported(report),
                (None, Judged::Malformed) => VerifierResult::Malformed,
                (None, _) => VerifierResult::None,
            };
            let mut project = observed.clone();
            if let Judged::Changed = judged {
                project[0].1 = Content::Other;
            }
            let mutated = match judged {
                Judged::Violated => vec![project[0].0.clone()],
                _ => Vec::new(),
            };
            let observed = VerifierObserved {
                project: &project,
                mutated: &mutated,
                result,
            };
            store.finish_verification(verification, &observed).unwrap();
        }

        pub(crate) fn accept(&mut self) -> Outcome {
            accept(
                &self.fx.project,
                &mut self.fx.store,
                self.task,
                self.generation,
            )
            .unwrap()
        }

        pub(crate) fn accept_with(&mut self, deriving: impl FnMut(&str) -> Result<()>) -> Outcome {
            let (project, store) = (&self.fx.project, &mut self.fx.store);
            accept_with(project, store, self.task, self.generation, deriving).unwrap()
        }

        fn root(&self) -> &Path {
            &self.fx.project.root
        }

        fn read(&self, path: &str) -> Option<String> {
            fs::read_to_string(self.root().join(path)).ok()
        }

        pub(crate) fn phase(&self) -> Option<AcceptancePhase> {
            let acceptance = self.fx.store.acceptance(self.generation).unwrap();
            acceptance.map(|a| a.phase)
        }

        fn generation_state(&self) -> GenerationState {
            let generations = self.fx.store.generations(self.task).unwrap();
            generations
                .iter()
                .find(|g| g.id == self.generation)
                .unwrap()
                .state
        }

        fn hash(&self, path: &str) -> Option<Option<String>> {
            let source = self.fx.store.accepted_source(path).unwrap();
            source.map(|s| s.hash)
        }

        fn status(&self, path: &str) -> Freshness<()> {
            self.fx.store.graph_status(path).unwrap()
        }

        fn symbols(&self, path: &str) -> Vec<String> {
            match self.fx.store.entities(path).unwrap() {
                Freshness::Current(entities) => {
                    let mut symbols: Vec<String> =
                        entities.into_iter().map(|e: Entity| e.id.symbol).collect();
                    symbols.sort_unstable();
                    symbols
                }
                other => panic!("`{path}` is {other:?}"),
            }
        }

        /// How often events of `kind` were recorded.
        fn events(&self, kind: &str) -> usize {
            let events = self.fx.store.events_after(0, 1_000_000).unwrap();
            events.iter().filter(|e| e.kind == kind).count()
        }

        /// Everything acceptance may change, as the store holds it.
        fn canonical(&self) -> Vec<Vec<Vec<Value>>> {
            [
                "SELECT * FROM accepted_sources ORDER BY path",
                "SELECT path, hash, language FROM graph_sources ORDER BY path",
                "SELECT count(*) FROM graph_entities",
                "SELECT count(*) FROM graph_relations",
                "SELECT * FROM ownership ORDER BY path",
                "SELECT id, state, ended_at FROM generations ORDER BY id",
                "SELECT id, state FROM plans ORDER BY id",
                "SELECT * FROM acceptances",
                "SELECT * FROM acceptance_sources",
                "SELECT * FROM acceptance_phases",
            ]
            .iter()
            .map(|sql| rows(&self.fx.store, sql))
            .collect()
        }

        /// Opens the store afresh.
        fn reopen(self) -> Self {
            let Self {
                fx,
                plan,
                task,
                next,
                generation,
                execution,
            } = self;
            Self {
                fx: fx.reopen(),
                plan,
                task,
                next,
                generation,
                execution,
            }
        }

        /// A connection of its own, as another process would have.
        pub(crate) fn connection(&self) -> rusqlite::Connection {
            rusqlite::Connection::open(self.root().join(STATE_DIR).join(STATE_DB)).unwrap()
        }

        fn object(&self, content: &str) -> PathBuf {
            self.root()
                .join(STATE_DIR)
                .join("objects")
                .join(sha256(content))
        }
    }

    fn write(root: &Path, path: &str, content: &str) {
        let path = root.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    fn rows(store: &Store, sql: &str) -> Vec<Vec<Value>> {
        let mut stmt = store.raw().prepare(sql).unwrap();
        let columns = stmt.column_count();
        stmt.query_map([], |r| (0..columns).map(|i| r.get(i)).collect())
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    }

    fn completed(outcome: Outcome) -> Acceptance {
        match outcome {
            Outcome::Completed(acceptance) => acceptance,
            other => panic!("not completed: {other:?}"),
        }
    }

    fn precondition(outcome: Outcome) -> String {
        match outcome {
            Outcome::InvalidPrecondition(why) => why,
            other => panic!("not an invalid precondition: {other:?}"),
        }
    }

    const ACCEPTED: [(&str, &str); 4] = [
        ("src/lib.rs", "pub fn old() {}\n"),
        ("src/gone.rs", "pub fn gone() {}\n"),
        ("src/notes.txt", "old notes\n"),
        ("src/other.rs", "pub fn other() {}\n"),
    ];
    const SCOPE: [&str; 5] = [
        "src/lib.rs",
        "src/gone.rs",
        "src/new.rs",
        "src/notes.txt",
        "src/bad.rs",
    ];
    const EDITS: [(&str, Option<&str>); 5] = [
        ("src/lib.rs", Some("pub fn new_lib() {}\n")),
        ("src/gone.rs", None),
        ("src/new.rs", Some("pub struct Created;\n")),
        ("src/notes.txt", Some("new notes\n")),
        ("src/bad.rs", Some("fn broken( {\n")),
    ];

    #[test]
    fn a_passed_candidate_becomes_accepted_source_and_graph_exactly_once() {
        let mut case = Case::installed(&ACCEPTED, &SCOPE, &EDITS);
        case.verify(Judged::Pass);
        // A pass alone changes nothing canonical.
        assert_eq!(case.hash("src/lib.rs"), Some(Some(sha256(ACCEPTED[0].1))));
        assert_eq!(case.symbols("src/lib.rs"), ["old", "self"]);
        assert_eq!(case.phase(), None);
        assert_eq!(case.generation_state(), GenerationState::Active);
        // Unrelated dirty bytes need not match anything.
        write(case.root(), "src/other.rs", "unrelated, uncommitted work\n");
        let agents = (
            case.events("agent.created"),
            case.events("invocation.started"),
        );

        let acceptance = completed(case.accept());
        assert_eq!(
            (acceptance.generation, acceptance.execution),
            (case.generation, case.execution)
        );
        assert_eq!(acceptance.phase, AcceptancePhase::Completed);
        let change =
            |path: &str, hash: Option<&str>, replaced: Option<Option<&str>>| AcceptedChange {
                path: path.into(),
                hash: hash.map(sha256),
                replaced: replaced.map(|r| r.map(sha256)),
            };
        assert_eq!(
            acceptance.changes,
            [
                change("src/bad.rs", Some("fn broken( {\n"), None),
                change("src/gone.rs", None, Some(Some(ACCEPTED[1].1))),
                change(
                    "src/lib.rs",
                    Some("pub fn new_lib() {}\n"),
                    Some(Some(ACCEPTED[0].1))
                ),
                change("src/new.rs", Some("pub struct Created;\n"), None),
                change(
                    "src/notes.txt",
                    Some("new notes\n"),
                    Some(Some(ACCEPTED[2].1))
                ),
            ]
        );
        // Accepted source is exactly the candidate, produced by this generation.
        for c in &acceptance.changes {
            let source = case.fx.store.accepted_source(&c.path).unwrap().unwrap();
            assert_eq!(source.hash, c.hash, "{}", c.path);
            assert_eq!(source.generation, Some(case.generation), "{}", c.path);
        }
        assert_eq!(case.hash("src/other.rs"), Some(Some(sha256(ACCEPTED[3].1))));
        // CodeGraph: refreshed, removed, and never fabricated.
        assert_eq!(case.symbols("src/lib.rs"), ["new_lib", "self"]);
        assert_eq!(case.symbols("src/new.rs"), ["Created", "self"]);
        assert_eq!(case.status("src/gone.rs"), Freshness::Absent);
        assert_eq!(case.status("src/notes.txt"), Freshness::Unindexed);
        assert_eq!(case.status("src/bad.rs"), Freshness::Unindexed);
        assert_eq!(case.symbols("src/other.rs"), ["other", "self"]);
        let located = match case.fx.store.entities("src/lib.rs").unwrap() {
            Freshness::Current(entities) => entities[1].location.hash.clone(),
            other => panic!("{other:?}"),
        };
        assert_eq!(located, sha256("pub fn new_lib() {}\n"));
        // Ownership released whole; the task completed, the plan not.
        for path in SCOPE {
            assert_eq!(case.fx.store.owner(path).unwrap(), None, "{path}");
        }
        assert!(
            case.fx
                .store
                .owned_paths(case.generation)
                .unwrap()
                .is_empty()
        );
        assert_eq!(case.generation_state(), GenerationState::Accepted);
        assert_eq!(
            case.fx.store.task(case.task).unwrap().state,
            TaskState::Completed
        );
        assert_eq!(
            case.fx.store.plan(case.plan).unwrap().state,
            PlanState::Ready
        );
        // A dependent task becomes eligible, and nothing is dispatched.
        assert!(case.fx.store.dependencies_satisfied(case.next).unwrap());
        assert!(case.fx.store.generations(case.next).unwrap().is_empty());
        assert_eq!(
            (
                case.events("agent.created"),
                case.events("invocation.started")
            ),
            agents,
            "acceptance launches no agent"
        );
        // Working tree untouched.
        assert_eq!(
            case.read("src/lib.rs").as_deref(),
            Some("pub fn new_lib() {}\n")
        );
        assert_eq!(case.read("src/gone.rs"), None);
        assert_eq!(
            case.read("src/other.rs").as_deref(),
            Some("unrelated, uncommitted work\n")
        );

        // Idempotent, without repeating anything.
        let once = [
            "acceptance.published",
            "acceptance.synchronized",
            "acceptance.completed",
            "ownership.released",
        ];
        for kind in once {
            assert_eq!(case.events(kind), 1, "{kind}");
        }
        let generations_ended = case.events("generation.ended");
        let before = case.canonical();
        assert_eq!(completed(case.accept()), acceptance);
        assert_eq!(case.canonical(), before);
        for kind in once {
            assert_eq!(case.events(kind), 1, "{kind}");
        }
        assert_eq!(case.events("generation.ended"), generations_ended);
        // Reconstructed by a fresh store.
        let case = case.reopen();
        assert_eq!(
            case.fx.store.acceptance(case.generation).unwrap(),
            Some(acceptance)
        );
    }

    #[test]
    fn only_a_latest_reconciled_pass_is_accepted() {
        let unchanged = |case: &mut Case, judged: Option<Judged>, expected: &str| {
            if let Some(judged) = judged {
                case.verify(judged);
            }
            let before = case.canonical();
            let why = precondition(case.accept());
            assert!(why.contains(expected), "{why}");
            assert_eq!(case.canonical(), before, "{expected}");
            assert_eq!(case.phase(), None);
            assert_eq!(
                case.read("src/lib.rs").as_deref(),
                Some("pub fn new_lib() {}\n")
            );
        };
        let edits = [("src/lib.rs", Some("pub fn new_lib() {}\n"))];
        let mut case = Case::installed(&ACCEPTED, &["src/lib.rs"], &edits);
        unchanged(&mut case, None, "never verified");
        unchanged(
            &mut case,
            Some(Judged::InvocationFailed),
            "ended invocation_failed",
        );
        unchanged(&mut case, Some(Judged::Malformed), "ended malformed_result");
        unchanged(&mut case, Some(Judged::Changed), "ended candidate_changed");
        unchanged(&mut case, Some(Judged::Violated), "ended boundary_violated");
        unchanged(&mut case, Some(Judged::Unresolved), "outcome unknown");

        let mut case = Case::installed(&ACCEPTED, &["src/lib.rs"], &edits);
        unchanged(&mut case, Some(Judged::Fail), "ended failed, not passed");

        // The pass of a later verification, after one that judged nothing.
        let mut case = Case::installed(&ACCEPTED, &["src/lib.rs"], &edits);
        case.verify(Judged::InvocationFailed);
        case.verify(Judged::Pass);
        let acceptance = completed(case.accept());
        let verifications = case.fx.store.verifications(case.generation).unwrap();
        assert_eq!(acceptance.verification, verifications[1].id);
        let VerificationStatus::Finished(result) = &verifications[1].status else {
            panic!("unfinished");
        };
        assert_eq!(result.outcome, VerificationOutcome::Passed);

        // Another task's generation, or one that ended.
        let mut case = Case::passed(&ACCEPTED, &["src/lib.rs"], &edits);
        let refused = accept(
            &case.fx.project,
            &mut case.fx.store,
            case.next,
            case.generation,
        );
        assert!(precondition(refused.unwrap()).contains("belongs to task"));
        case.fx
            .store
            .finish_generation(case.generation, GenerationEnd::Rejected)
            .unwrap();
        unchanged(&mut case, None, "already ended");
    }

    #[test]
    fn a_drifted_candidate_is_refused_before_anything_is_published() {
        let mut case = Case::passed(&ACCEPTED, &SCOPE, &EDITS);
        let before = case.canonical();
        let drift = |case: &mut Case, path: &str, change: &dyn Fn(&Path)| {
            let full = case.root().join(path);
            change(&full);
            let outcome = case.accept();
            assert_eq!(
                outcome,
                Outcome::CandidateDrifted(vec![path.to_string()]),
                "{path}"
            );
            assert_eq!(case.canonical(), before, "{path}");
            full
        };
        // Changed bytes, which stay as found: nothing is restored.
        let lib = drift(&mut case, "src/lib.rs", &|p| {
            fs::write(p, "pub fn edited() {}\n").unwrap()
        });
        assert_eq!(
            case.read("src/lib.rs").as_deref(),
            Some("pub fn edited() {}\n")
        );
        fs::write(&lib, "pub fn new_lib() {}\n").unwrap();
        // Vanished, replaced by a directory, or by a symlink to the same bytes.
        let new = drift(&mut case, "src/new.rs", &|p| fs::remove_file(p).unwrap());
        fs::create_dir(&new).unwrap();
        assert!(matches!(case.accept(), Outcome::CandidateDrifted(_)));
        fs::remove_dir(&new).unwrap();
        #[cfg(unix)]
        {
            write(case.root(), "src/elsewhere.rs", "pub struct Created;\n");
            std::os::unix::fs::symlink("elsewhere.rs", &new).unwrap();
            assert_eq!(
                case.accept(),
                Outcome::CandidateDrifted(vec!["src/new.rs".into()])
            );
            fs::remove_file(&new).unwrap();
            fs::remove_file(case.root().join("src/elsewhere.rs")).unwrap();
        }
        fs::write(&new, "pub struct Created;\n").unwrap();
        // A verified deletion recreated.
        let gone = drift(&mut case, "src/gone.rs", &|p| {
            fs::write(p, "pub fn gone() {}\n").unwrap()
        });
        assert_eq!(case.canonical(), before);
        assert_eq!(case.generation_state(), GenerationState::Active);
        assert_eq!(
            case.fx.store.owned_paths(case.generation).unwrap().len(),
            SCOPE.len()
        );
        fs::remove_file(gone).unwrap();

        // Holding the exact candidate again, it is accepted.
        completed(case.accept());
        assert_eq!(case.symbols("src/lib.rs"), ["new_lib", "self"]);
    }

    #[test]
    fn missing_ownership_is_refused_before_anything_is_published() {
        let edits = [("src/lib.rs", Some("pub fn new_lib() {}\n"))];
        let mut case = Case::passed(&ACCEPTED, &["src/lib.rs", "src/extra.rs"], &edits);
        let before = case.canonical();
        // Lost ownership of a path of the task's scope, even an unchanged one.
        case.fx
            .store
            .raw()
            .execute("DELETE FROM ownership WHERE path = 'src/extra.rs'", [])
            .unwrap();
        let why = precondition(case.accept());
        assert!(why.contains("does not own `src/extra.rs`"), "{why}");
        // Held by another generation meanwhile.
        let (_, tasks) = ready_plan(&mut case.fx.store, &[("rival", &["src/extra.rs"], &[])]);
        let rival = case.fx.store.start_generation(tasks[0]).unwrap();
        acquire(&mut case.fx.store, rival, &["src/extra.rs"]);
        let why = precondition(case.accept());
        assert!(
            why.contains(&format!("owned by generation {rival}")),
            "{why}"
        );
        let mut expected = before.clone();
        expected[4] = rows(&case.fx.store, "SELECT * FROM ownership ORDER BY path");
        expected[5] = rows(
            &case.fx.store,
            "SELECT id, state, ended_at FROM generations ORDER BY id",
        );
        expected[6] = rows(&case.fx.store, "SELECT id, state FROM plans ORDER BY id");
        assert_eq!(case.canonical(), expected);
        assert_eq!(case.phase(), None);
    }

    #[test]
    fn unavailable_objects_are_refused_before_anything_is_published() {
        let mut case = Case::passed(&ACCEPTED, &SCOPE, &EDITS);
        let before = case.canonical();
        let object = case.object("pub struct Created;\n");
        fs::remove_file(&object).unwrap();
        let unavailable = |outcome: Outcome, expected: &str| match outcome {
            Outcome::ObjectUnavailable { path, hash, reason } => {
                assert_eq!(path, "src/new.rs");
                assert_eq!(hash, sha256("pub struct Created;\n"));
                assert!(reason.contains(expected), "{reason}");
            }
            other => panic!("{other:?}"),
        };
        unavailable(case.accept(), "unavailable");
        fs::write(&object, "not what it is named after").unwrap();
        unavailable(case.accept(), "corrupt");
        assert_eq!(case.canonical(), before);
        assert_eq!(
            case.read("src/new.rs").as_deref(),
            Some("pub struct Created;\n")
        );

        fs::write(&object, "pub struct Created;\n").unwrap();
        completed(case.accept());
    }

    #[test]
    fn failing_graph_synchronization_leaves_a_durable_incomplete_acceptance() {
        let mut case = Case::passed(&ACCEPTED, &SCOPE, &EDITS);
        let (_, tasks) = ready_plan(&mut case.fx.store, &[("rival", &["src/lib.rs"], &[])]);
        let rival = case.fx.store.start_generation(tasks[0]).unwrap();
        let published_source = |case: &Case| {
            for (path, content) in EDITS {
                assert_eq!(case.hash(path), Some(content.map(sha256)), "{path}");
            }
        };
        // Stale everywhere it changed: nothing claims the old graph current,
        // and no path's graph was replaced before the failure.
        let stale = |case: &Case| {
            for path in ["src/lib.rs", "src/gone.rs"] {
                assert_eq!(case.status(path), Freshness::Stale, "{path}");
            }
            assert_eq!(case.status("src/new.rs"), Freshness::Unindexed);
        };
        let held = |case: &mut Case| {
            assert_eq!(case.generation_state(), GenerationState::Active);
            assert_eq!(
                case.fx.store.task(case.task).unwrap().state,
                TaskState::Running
            );
            let owned = case.fx.store.owned_paths(case.generation).unwrap();
            assert_eq!(owned.len(), SCOPE.len());
            let conflicted = case
                .fx
                .store
                .acquire_ownership(rival, &["src/lib.rs"])
                .unwrap();
            assert!(matches!(conflicted, Acquisition::Conflicted(_)));
        };

        // Failing part way: the first graph derived, the second not.
        let mut derived = Vec::new();
        let outcome = case.accept_with(|path| {
            derived.push(path.to_string());
            match derived.len() {
                1 => Ok(()),
                _ => anyhow::bail!("injected failure at `{path}`"),
            }
        });
        assert_eq!(derived, ["src/bad.rs", "src/lib.rs"]);
        let Outcome::Incomplete { phase, reason } = outcome else {
            panic!("{outcome:?}");
        };
        assert_eq!(phase, AcceptancePhase::Published);
        assert!(
            reason.contains("injected failure at `src/lib.rs`"),
            "{reason}"
        );
        assert_eq!(case.phase(), Some(AcceptancePhase::Published));
        published_source(&case);
        stale(&case);
        held(&mut case);
        assert_eq!(case.events("acceptance.published"), 1);
        assert_eq!(case.events("acceptance.synchronized"), 0);

        // What remains is known to a fresh store, and nothing ends it early.
        let mut case = case.reopen();
        assert_eq!(case.phase(), Some(AcceptancePhase::Published));
        let failed = case
            .fx
            .store
            .finish_generation(case.generation, GenerationEnd::Failed);
        assert!(err(failed).contains("being accepted"));
        let conn = case.connection();
        for sql in [
            "UPDATE generations SET state = 'failed', ended_at = 1 WHERE id = ?1",
            "UPDATE generations SET state = 'accepted', ended_at = 1 WHERE id = ?1",
        ] {
            let refused = conn.execute(sql, [case.generation]).unwrap_err();
            assert!(
                refused.to_string().contains("only by its acceptance"),
                "{refused}"
            );
        }
        let refused = conn
            .execute(
                "DELETE FROM ownership WHERE generation_id = ?1",
                [case.generation],
            )
            .unwrap_err();
        assert!(
            refused.to_string().contains("ownership is held"),
            "{refused}"
        );
        let refused = conn
            .execute(
                "UPDATE accepted_sources SET hash = NULL WHERE path = 'src/lib.rs'",
                [],
            )
            .unwrap_err();
        assert!(
            refused.to_string().contains("holds its accepted source"),
            "{refused}"
        );
        let refused = conn
            .execute(
                "INSERT INTO acceptance_phases VALUES (?1, 'synchronized', 1)",
                [case.generation],
            )
            .unwrap_err();
        assert!(
            refused.to_string().contains("synchronized only with"),
            "{refused}"
        );
        drop(conn);
        held(&mut case);

        // The store failing the synchronizing transaction itself.
        let conn = case.connection();
        conn.execute_batch(
            "CREATE TRIGGER test_refuse_graph BEFORE INSERT ON graph_sources
             BEGIN SELECT RAISE(ABORT, 'forced graph failure'); END;",
        )
        .unwrap();
        let outcome = case.accept();
        let Outcome::Incomplete { phase, reason } = outcome else {
            panic!("{outcome:?}");
        };
        assert_eq!(phase, AcceptancePhase::Published);
        assert!(reason.contains("forced graph failure"), "{reason}");
        stale(&case);
        conn.execute_batch("DROP TRIGGER test_refuse_graph")
            .unwrap();

        // Resuming synchronizes from the published content, whatever the
        // working tree holds now, publishing nothing else.
        write(case.root(), "src/lib.rs", "pub fn rewritten_since() {}\n");
        let acceptance = completed(case.accept());
        published_source(&case);
        assert_eq!(case.symbols("src/lib.rs"), ["new_lib", "self"]);
        assert_eq!(case.symbols("src/new.rs"), ["Created", "self"]);
        assert_eq!(case.status("src/gone.rs"), Freshness::Absent);
        assert_eq!(case.events("acceptance.published"), 1);
        assert_eq!(case.events("acceptance.synchronized"), 1);
        assert_eq!(acceptance.phase, AcceptancePhase::Completed);
        assert!(
            case.fx
                .store
                .owned_paths(case.generation)
                .unwrap()
                .is_empty()
        );
        acquire(&mut case.fx.store, rival, &["src/lib.rs"]);
    }

    #[test]
    fn failing_before_graph_derivation_leaves_every_graph_stale() {
        let mut case = Case::passed(&ACCEPTED, &SCOPE, &EDITS);
        let outcome = case.accept_with(|_| anyhow::bail!("before any graph"));
        assert!(matches!(
            outcome,
            Outcome::Incomplete {
                phase: AcceptancePhase::Published,
                ..
            }
        ));
        assert_eq!(case.status("src/lib.rs"), Freshness::Stale);
        assert_eq!(case.status("src/gone.rs"), Freshness::Stale);
        completed(case.accept());
    }

    #[test]
    fn failing_to_complete_keeps_ownership_and_resumes_once() {
        let mut case = Case::passed(&ACCEPTED, &SCOPE, &EDITS);
        let conn = case.connection();
        conn.execute_batch(
            "CREATE TRIGGER test_refuse_completion BEFORE INSERT ON acceptance_phases
             WHEN NEW.phase = 'completed'
             BEGIN SELECT RAISE(ABORT, 'forced completion failure'); END;",
        )
        .unwrap();
        let outcome = case.accept();
        let Outcome::Incomplete { phase, reason } = outcome else {
            panic!("{outcome:?}");
        };
        assert_eq!(phase, AcceptancePhase::Synchronized);
        assert!(reason.contains("forced completion failure"), "{reason}");
        conn.execute_batch("DROP TRIGGER test_refuse_completion")
            .unwrap();
        let mut case = case.reopen();
        assert_eq!(case.phase(), Some(AcceptancePhase::Synchronized));
        assert_eq!(case.symbols("src/lib.rs"), ["new_lib", "self"]);
        assert_eq!(case.generation_state(), GenerationState::Active);
        assert_eq!(
            case.fx.store.task(case.task).unwrap().state,
            TaskState::Running
        );
        assert_eq!(
            case.fx.store.owned_paths(case.generation).unwrap().len(),
            SCOPE.len()
        );
        // The failure came after the leave was taken, the generation
        // accepted and its ownership released: all of it rolled back.
        assert_eq!(leave(&case.fx.store), 0);
        assert_eq!(case.events("acceptance.completed"), 0);
        assert_eq!(case.events("ownership.released"), 0);
        let ended = case.events("generation.ended");

        completed(case.accept());
        assert_eq!(case.events("generation.ended"), ended + 1);
        assert_eq!(case.events("acceptance.synchronized"), 1);
        assert_eq!(case.events("acceptance.completed"), 1);
        assert!(
            case.fx
                .store
                .owned_paths(case.generation)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn racing_acceptances_complete_one_acceptance_once() {
        use std::sync::Barrier;
        for _ in 0..5 {
            let case = Case::passed(&ACCEPTED, &SCOPE, &EDITS);
            let db = case.root().join(STATE_DIR).join(STATE_DB);
            let barrier = Barrier::new(2);
            let outcomes: Vec<Outcome> = thread::scope(|scope| {
                let racers: Vec<_> = (0..2)
                    .map(|_| {
                        scope.spawn(|| {
                            let mut store = Store::open(&db).unwrap();
                            barrier.wait();
                            accept(&case.fx.project, &mut store, case.task, case.generation)
                                .unwrap()
                        })
                    })
                    .collect();
                racers.into_iter().map(|r| r.join().unwrap()).collect()
            });
            assert_eq!(outcomes[0], outcomes[1]);
            assert_eq!(
                completed(outcomes[0].clone()).phase,
                AcceptancePhase::Completed
            );
            for kind in [
                "acceptance.published",
                "acceptance.synchronized",
                "acceptance.completed",
                "ownership.released",
            ] {
                assert_eq!(case.events(kind), 1, "{kind}");
            }
            assert_eq!(case.symbols("src/lib.rs"), ["new_lib", "self"]);
        }
    }

    #[test]
    fn raw_sql_cannot_fabricate_or_rewrite_an_acceptance() {
        let edits = [
            ("src/lib.rs", Some("pub fn new_lib() {}\n")),
            ("src/new.rs", Some("pub struct Created;\n")),
        ];
        let mut failed = Case::installed(&ACCEPTED, &["src/lib.rs", "src/new.rs"], &edits);
        failed.verify(Judged::Fail);
        let mut case = failed;
        let conn = case.connection();
        let refused = |result: rusqlite::Result<usize>, expected: &str| {
            let message = result.unwrap_err().to_string();
            let any = expected.split('|').any(|e| message.contains(e));
            assert!(any, "{expected}: {message}");
        };
        let verification = |case: &Case| case.fx.store.verifications(case.generation).unwrap();
        let intend = "INSERT INTO acceptances VALUES (?1, ?2, ?3, 1)";
        let is_accepted = "is accepted, once";
        let failed_verification = verification(&case)[0].id;
        refused(
            conn.execute(
                intend,
                rusqlite::params![case.generation, case.execution, failed_verification],
            ),
            is_accepted,
        );
        // A generation that executed is accepted only by its acceptance.
        refused(
            conn.execute(
                "UPDATE generations SET state = 'accepted', ended_at = 1 WHERE id = ?1",
                [case.generation],
            ),
            "only by its acceptance",
        );
        let legacy = source::accept_generation(
            &case.fx.project,
            &mut case.fx.store,
            case.generation,
            &["src/lib.rs"],
        );
        assert!(err(legacy).contains("only accepting its verified candidate"));

        // Passed now, as a later verification cannot be: a fresh candidate.
        let mut case = Case::installed(&ACCEPTED, &["src/lib.rs", "src/new.rs"], &edits);
        let conn = case.connection();
        case.verify(Judged::Pass);
        let passed = verification(&case)[0].id;
        let phase = "INSERT INTO acceptance_phases VALUES (?1, ?2, 1)";
        refused(
            conn.execute(phase, rusqlite::params![case.generation, "published"]),
            "one at a time",
        );
        refused(
            conn.execute(
                intend,
                rusqlite::params![case.generation, case.execution, 999],
            ),
            is_accepted,
        );
        // Recording the intent alone publishes nothing and completes nothing.
        conn.execute(
            intend,
            rusqlite::params![case.generation, case.execution, passed],
        )
        .unwrap();
        assert_eq!(case.phase(), Some(AcceptancePhase::Intended));
        for phase_name in ["synchronized", "completed"] {
            refused(
                conn.execute(phase, rusqlite::params![case.generation, phase_name]),
                "one at a time",
            );
        }
        refused(
            conn.execute(phase, rusqlite::params![case.generation, "published"]),
            "whole candidate",
        );
        let source = "INSERT INTO acceptance_sources VALUES (?1, ?2, ?3, 1, ?4)";
        let lib_hash = sha256("pub fn new_lib() {}\n");
        let old_hash = sha256(ACCEPTED[0].1);
        refused(
            conn.execute(
                source,
                rusqlite::params![case.generation, "src/lib.rs", old_hash, old_hash],
            ),
            "exactly its candidate",
        );
        refused(
            conn.execute(
                source,
                rusqlite::params![case.generation, "src/other.rs", old_hash, old_hash],
            ),
            "exactly its candidate",
        );
        // One path of two published is not the candidate published.
        conn.execute(
            source,
            rusqlite::params![case.generation, "src/lib.rs", lib_hash, old_hash],
        )
        .unwrap();
        conn.execute(
            "UPDATE accepted_sources SET hash = ?2, generation_id = ?1 WHERE path = 'src/lib.rs'",
            rusqlite::params![case.generation, lib_hash],
        )
        .unwrap();
        refused(
            conn.execute(phase, rusqlite::params![case.generation, "published"]),
            "whole candidate",
        );
        refused(
            conn.execute(
                source,
                rusqlite::params![case.generation, "src/lib.rs", lib_hash, old_hash],
            ),
            "exactly its candidate",
        );
        let reprioritized =
            "INSERT OR REPLACE INTO acceptance_sources VALUES (?1, 'src/lib.rs', ?2, 0, NULL)";
        refused(
            conn.execute(reprioritized, rusqlite::params![case.generation, lib_hash]),
            "exactly its candidate",
        );

        // agentctl resumes from the recorded intent, as the schema allows.
        let acceptance = completed(case.accept());
        assert_eq!(acceptance.verification, passed);
        assert_eq!(acceptance.changes.len(), 2);

        // Terminal history stays, and nothing replaces it.
        for sql in [
            "DELETE FROM acceptances",
            "DELETE FROM acceptance_sources",
            "DELETE FROM acceptance_phases",
            "UPDATE acceptances SET started_at = 2",
            "UPDATE acceptance_sources SET hash = NULL",
            "UPDATE acceptance_phases SET at = 2",
        ] {
            refused(conn.execute(sql, []), "acceptance history is immutable");
        }
        refused(
            conn.execute(
                "INSERT OR REPLACE INTO acceptances VALUES (?1, ?2, ?3, 3)",
                rusqlite::params![case.generation, case.execution, passed],
            ),
            is_accepted,
        );
        refused(
            conn.execute(
                "INSERT OR REPLACE INTO acceptance_phases VALUES (?1, 'completed', 3)",
                [case.generation],
            ),
            "one at a time",
        );
        refused(
            conn.execute(
                "UPDATE generations SET state = 'failed' WHERE id = ?1",
                [case.generation],
            ),
            "only by its acceptance",
        );
        assert_eq!(completed(case.accept()), acceptance);
        // A completed acceptance holds nothing against later work.
        conn.execute(
            "UPDATE accepted_sources SET hash = NULL, generation_id = NULL WHERE path = 'src/lib.rs'",
            [],
        )
        .unwrap();
    }

    /// How many leaves to complete an acceptance are held.
    fn leave(store: &Store) -> i64 {
        let held = rows(store, "SELECT count(*) FROM acceptance_completions");
        match held[0][0] {
            Value::Integer(n) => n,
            ref other => panic!("{other:?}"),
        }
    }

    #[test]
    fn raw_sql_cannot_complete_an_acceptance_in_part() {
        let mut case = Case::passed(&ACCEPTED, &SCOPE, &EDITS);
        let conn = case.connection();
        let refused = |result: rusqlite::Result<()>, expected: &str| {
            let message = result.unwrap_err().to_string();
            assert!(message.contains(expected), "{expected}: {message}");
        };
        let g = case.generation;
        let accept_generation =
            format!("UPDATE generations SET state = 'accepted', ended_at = 1 WHERE id = {g}");
        let release = format!("DELETE FROM ownership WHERE generation_id = {g}");
        let complete = format!("INSERT INTO acceptance_phases VALUES ({g}, 'completed', 1)");
        let take = format!("INSERT INTO acceptance_completions (generation_id) VALUES ({g})");
        let by_acceptance = "only by its acceptance";
        let held = "held until its acceptance is completed";
        let not_completable = "only a synchronized acceptance";

        // Published: no leave, so nothing completes.
        let outcome = case.accept_with(|_| anyhow::bail!("forced derivation failure"));
        assert!(matches!(
            outcome,
            Outcome::Incomplete {
                phase: AcceptancePhase::Published,
                ..
            }
        ));
        refused(conn.execute_batch(&take), not_completable);
        refused(conn.execute_batch(&accept_generation), by_acceptance);

        // Synchronized, as a failed completion leaves it.
        conn.execute_batch(
            "CREATE TRIGGER test_refuse_completion BEFORE INSERT ON acceptance_phases
             WHEN NEW.phase = 'completed'
             BEGIN SELECT RAISE(ABORT, 'forced completion failure'); END;",
        )
        .unwrap();
        assert!(matches!(
            case.accept(),
            Outcome::Incomplete {
                phase: AcceptancePhase::Synchronized,
                ..
            }
        ));
        conn.execute_batch("DROP TRIGGER test_refuse_completion")
            .unwrap();
        assert_eq!(case.phase(), Some(AcceptancePhase::Synchronized));
        let synchronized = case.canonical();
        let events = case.fx.store.events_after(0, 1_000_000).unwrap().len();
        let unchanged = |case: &Case| {
            assert_eq!(case.canonical(), synchronized);
            assert_eq!(case.phase(), Some(AcceptancePhase::Synchronized));
            assert_eq!(case.generation_state(), GenerationState::Active);
            let owned = case.fx.store.owned_paths(case.generation).unwrap();
            assert_eq!(owned.len(), SCOPE.len());
            assert_eq!(leave(&case.fx.store), 0);
            let now = case.fx.store.events_after(0, 1_000_000).unwrap().len();
            assert_eq!(now, events);
        };

        // Each protected mutation alone, and both in turn, in either order.
        refused(conn.execute_batch(&accept_generation), by_acceptance);
        unchanged(&case);
        refused(conn.execute_batch(&release), held);
        unchanged(&case);
        for (first, second) in [
            (&accept_generation, &release),
            (&release, &accept_generation),
        ] {
            conn.execute_batch("BEGIN").unwrap();
            assert!(conn.execute_batch(first).is_err());
            assert!(conn.execute_batch(second).is_err());
            conn.execute_batch("ROLLBACK").unwrap();
            unchanged(&case);
        }
        // Nor is it recorded as completed.
        refused(conn.execute_batch(&complete), "accepted and owns nothing");
        unchanged(&case);

        // Leave is only for this synchronized acceptance, and is never
        // committed: not alone, nor having used it without completing.
        for other in [999_999, 0] {
            let wrong =
                format!("INSERT INTO acceptance_completions (generation_id) VALUES ({other})");
            refused(conn.execute_batch(&wrong), not_completable);
        }
        let fk = "FOREIGN KEY constraint failed";
        refused(conn.execute_batch(&take), fk);
        unchanged(&case);
        let used = format!("BEGIN; {take}; {accept_generation}; {release}; COMMIT;");
        refused(conn.execute_batch(&used), fk);
        conn.execute_batch("ROLLBACK").unwrap();
        unchanged(&case);
        // Leave held is given back only by completing.
        conn.execute_batch("BEGIN").unwrap();
        conn.execute_batch(&take).unwrap();
        refused(
            conn.execute_batch("DELETE FROM acceptance_completions"),
            "only by completing it",
        );
        refused(
            conn.execute_batch("UPDATE acceptance_completions SET generation_id = 0"),
            "only by completing it",
        );
        // Nor completed while anything is still owned.
        conn.execute_batch(&accept_generation).unwrap();
        refused(conn.execute_batch(&complete), "accepted and owns nothing");
        conn.execute_batch("ROLLBACK").unwrap();
        unchanged(&case);
        // Reopening changes nothing, and reaching for leave the store held
        // nowhere finds it.
        let mut case = case.reopen();
        unchanged(&case);

        // Completing takes it, uses it and gives it back, all at once.
        let acceptance = completed(case.accept());
        assert_eq!(acceptance.phase, AcceptancePhase::Completed);
        assert_eq!(case.generation_state(), GenerationState::Accepted);
        assert_eq!(
            case.fx.store.task(case.task).unwrap().state,
            TaskState::Completed
        );
        assert!(case.fx.store.owned_paths(g).unwrap().is_empty());
        assert_eq!(leave(&case.fx.store), 0);
        for kind in [
            "acceptance.completed",
            "ownership.released",
            "acceptance.synchronized",
        ] {
            assert_eq!(case.events(kind), 1, "{kind}");
        }
        let accepted = case.canonical();
        // Completed, it grants no leave again, and completing again changes
        // nothing.
        refused(conn.execute_batch(&take), not_completable);
        assert_eq!(completed(case.accept()), acceptance);
        assert_eq!(case.canonical(), accepted);
        assert_eq!(case.events("acceptance.completed"), 1);
    }

    #[test]
    fn literal_paths_are_accepted_as_named() {
        let paths = [
            "src/[id].rs",
            "src/with space.rs",
            "src/(group)/a+b@c.rs",
            "src/ünïcødé/文件.rs",
        ];
        let accepted = [
            ("src/[id].rs", "pub fn id() {}\n"),
            ("src/with space.rs", "pub fn spaced() {}\n"),
            ("src/(group)/a+b@c.rs", "pub fn grouped() {}\n"),
            ("src/i.rs", "pub fn decoy() {}\n"),
        ];
        let edits = [
            ("src/[id].rs", Some("pub fn id_changed() {}\n")),
            ("src/with space.rs", None),
            (
                "src/(group)/a+b@c.rs",
                Some("pub fn grouped_changed() {}\n"),
            ),
            ("src/ünïcødé/文件.rs", Some("pub fn unicode() {}\n")),
        ];
        let mut case = Case::passed(&accepted, &paths, &edits);
        let acceptance = completed(case.accept());
        let mut expected: Vec<&str> = paths.to_vec();
        expected.sort_unstable();
        let published: Vec<&str> = acceptance.changes.iter().map(|c| c.path.as_str()).collect();
        assert_eq!(published, expected);
        assert_eq!(case.symbols("src/[id].rs"), ["id_changed", "self"]);
        assert_eq!(case.status("src/with space.rs"), Freshness::Absent);
        assert_eq!(case.hash("src/with space.rs"), Some(None));
        assert_eq!(
            case.symbols("src/(group)/a+b@c.rs"),
            ["grouped_changed", "self"]
        );
        assert_eq!(case.symbols("src/ünïcødé/文件.rs"), ["self", "unicode"]);
        // `src/[id].rs` names itself, never `src/i.rs`.
        assert_eq!(
            case.hash("src/i.rs"),
            Some(Some(sha256("pub fn decoy() {}\n")))
        );
        assert_eq!(case.symbols("src/i.rs"), ["decoy", "self"]);
        let source = case.fx.store.accepted_source("src/i.rs").unwrap().unwrap();
        assert_eq!(source.generation, None);
    }
}
