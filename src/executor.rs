//! The executor: a disposable implementation worker.
//!
//! An executor is a logical agent serving one attempt of one generation,
//! embodied by one fresh provider invocation that is never resumed. It gets
//! a bounded packet: its task, the human intent the task serves, the exact
//! literal paths it may mutate and targeted repository context drawn from
//! accepted source and CodeGraph. It may inspect its workspace itself, which
//! is not canonical knowledge.
//!
//! The executor never works in the project. agentctl observes the
//! repository, then copies what it observed into a disposable workspace
//! outside the project ([`source::Workspace`]): the repository's content,
//! but neither agentctl's canonical state nor Git's directory, which exist
//! only in the project and are never within the executor's reach through
//! its working directory. The provider edits there in its own editable
//! mode; agentctl adds no sandbox, and relies on none.
//!
//! Authority precedes action. The generation must already own its task's
//! whole scope; agentctl then observes the repository, stages the
//! workspace, and journals the attempt as intended together with that
//! baseline. The attempt is recorded as acted on before any executor
//! process exists. Once the invocation ends, agentctl observes the
//! workspace, and the store derives what changed there, whether every
//! change stayed within authority, and so the outcome, reconciling the
//! journal in the same transaction. What the executor reports and claims to
//! have modified is kept as evidence, never taken as proof.
//!
//! A candidate is an authorized workspace delta, safely staged against the
//! observed repository baseline: structurally valid work awaiting
//! verification, never accepted source, never a completed task, and no
//! proof that the executor process wrote every byte. agentctl then installs
//! it into the working tree, provisionally, as an action journaled on its
//! own: only if every changed path there still holds what the baseline
//! observed, all of it or, as far as agentctl can ensure, nothing. Anything
//! else the executor did stays in its workspace, which is discarded:
//! nothing of an execution that is not a candidate reaches the project.
//! Executing changes neither accepted source nor CodeGraph, and the
//! generation keeps its ownership whatever the outcome.
//!
//! What agentctl can observe bounds what it establishes: the paths the
//! project's Git takes for repository content, and anything in agentctl's
//! or Git's state, in the workspace; ignored paths there are not observed.
//! The provider runs as the user, so nothing but its own editing mode keeps
//! it from writing elsewhere by an absolute path, the project included;
//! installing then refuses whatever it finds changed in the project at a
//! path the candidate would write.

use std::collections::HashSet;
use std::path::PathBuf;
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::graph::Freshness;
use crate::project::Project;
use crate::runtime::{self, Control, Launch, Outcome, Provider};
use crate::source::{self, Installation, Preparation, Snapshot, Workspace};
use crate::state::{
    Capture, ExecutionId, ExecutionOutcome, ExecutionStatus, ExecutorResult, GenerationId,
    InstallOutcome, Observed, Reported, Store, TaskId, check_path,
};

/// Bounds on an executor's result.
const SUMMARY_LIMIT: usize = 4096;
const CLAIMED_LIMIT: usize = 256;
const PATH_LIMIT: usize = 4096;
/// How long the repository or workspace must hold still for an observation
/// to count.
const SETTLE: Duration = Duration::from_millis(100);
/// Roughly how many bytes of other source paths one packet lists, and how
/// many entities it lists per mutable path.
const SOURCES_BUDGET: usize = 16 * 1024;
const ENTITIES_LIMIT: usize = 200;

const INSTRUCTIONS: &str = "\
You are an executor of agentctl, an engineering control plane: a disposable \
worker implementing one task. You run once; nothing of any earlier session \
carries over. Your working directory is a disposable copy of the project's \
repository: its files, without Git's directory or agentctl's state.

Your input is JSON holding the task, the human intent it serves, your \
authority and a map of the repository's accepted source from its code \
graph. Read files in your working directory when the map is not precise \
enough; they may hold changes that are not accepted.

Create, modify or delete only the files in `authority.mutable_paths`, in \
your working directory. Each is one exact path relative to its root, never \
a pattern: `src/[id].rs` names that file only. Change nothing else: no \
other file, nothing in `.agentctl/` or `.git/`, no Git repository, commits \
or branches, and nothing outside your working directory. If the task \
cannot be done within these paths, stop and report that you failed. \
agentctl observes your working directory itself before and after you run: \
any mutation beyond your authority invalidates your work, whatever you \
report.

You do not decide whether your work is accepted; it is verified \
independently. Answer only with the structured result: `status` is \
`succeeded` if you completed the task within your authority, otherwise \
`failed`; `summary` says concisely what you did or why you could not; \
`modified_paths` lists the paths you changed.";

/// The JSON Schema of an executor's result, in the subset that providers
/// enforce strictly. agentctl enforces the protocol's bounds itself.
pub fn result_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "status": {"type": "string", "enum": ["succeeded", "failed"]},
            "summary": {"type": "string"},
            "modified_paths": {"type": "array", "items": {"type": "string"}},
        },
        "required": ["status", "summary", "modified_paths"],
        "additionalProperties": false,
    })
}

/// An executor's structured account of its work.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Report {
    status: Status,
    summary: String,
    modified_paths: Vec<String>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Status {
    Succeeded,
    Failed,
}

impl Report {
    /// Reads a result, which must keep to the protocol's bounds: a summary
    /// of at most `SUMMARY_LIMIT` bytes and at most `CLAIMED_LIMIT` distinct
    /// canonical paths.
    fn read(payload: &Value) -> Result<Self> {
        let report = Self::deserialize(payload)?;
        ensure!(report.summary.len() <= SUMMARY_LIMIT, "summary too long");
        ensure!(
            report.modified_paths.len() <= CLAIMED_LIMIT,
            "too many paths"
        );
        let mut seen = HashSet::new();
        for path in &report.modified_paths {
            ensure!(path.len() <= PATH_LIMIT, "path too long");
            check_path(path)?;
            ensure!(seen.insert(path), "path repeated");
        }
        Ok(report)
    }
}

/// Everything a fresh executor invocation is given, from canonical state
/// alone: its task, the human intent the task serves, the literal paths of
/// its `authority`, and repository context targeted at those paths.
pub fn input(
    project: &Project,
    store: &Store,
    task: TaskId,
    authority: &[String],
) -> Result<Value> {
    let task = store.task(task)?;
    let intent = store.plan(task.plan)?.intent;
    let roots: Vec<&str> = project
        .config
        .codegraph
        .roots
        .iter()
        .map(|r| r.as_str())
        .collect();
    Ok(json!({
        "task": {"key": task.key, "objective": task.objective, "context": task.context},
        "intent": {
            "objective": intent.objective,
            "constraints": intent.constraints,
            "completion_criteria": intent.completion_criteria,
        },
        "authority": {
            "mutable_paths": authority,
            "rules": [
                "Create, modify or delete only mutable_paths, each one literal path, never a pattern.",
                "Change no other file, nothing in .agentctl/ or .git/, no Git state, and nothing outside the working directory.",
                "What you report is a claim: agentctl observes the working directory itself.",
            ],
        },
        "source_roots": roots,
        "repository": repository(project, store, authority)?,
    }))
}

/// Accepted-source and CodeGraph context targeted at the paths an executor
/// may mutate: each one's accepted state and the entities its current graph
/// defines, and, within a budget, the other accepted sources of the roots by
/// path alone. Stale or missing graph facts are marked, never given, and
/// working-tree bytes never appear.
fn repository(project: &Project, store: &Store, authority: &[String]) -> Result<Value> {
    let mut mutable = Vec::new();
    for path in authority {
        let accepted = match store.accepted_source(path)? {
            None => {
                mutable.push(json!({"path": path, "accepted": "untracked"}));
                continue;
            }
            Some(source) if source.hash.is_none() => "absent",
            Some(_) => "present",
        };
        let mut entry = json!({"path": path, "accepted": accepted});
        entry["graph"] = match store.entities(path)? {
            Freshness::Current(entities) => {
                let listed: Vec<Value> = entities
                    .iter()
                    .take(ENTITIES_LIMIT)
                    .map(|e| json!({"kind": e.id.kind, "symbol": e.id.symbol}))
                    .collect();
                entry["entities_omitted"] = json!(entities.len() - listed.len());
                entry["entities"] = listed.into();
                "current"
            }
            Freshness::Stale => "stale",
            Freshness::Unindexed => "unindexed",
            Freshness::Absent => "absent",
        }
        .into();
        mutable.push(entry);
    }
    let roots = &project.config.codegraph.roots;
    let mut sources = Vec::new();
    let mut used = 0;
    let mut omitted = 0;
    for path in store.accepted_paths()? {
        if authority.contains(&path) || !roots.iter().any(|root| root.contains_path(&path)) {
            continue;
        }
        if used + path.len() > SOURCES_BUDGET {
            omitted += 1;
            continue;
        }
        used += path.len();
        sources.push(path);
    }
    Ok(json!({"mutable": mutable, "sources": sources, "sources_omitted": omitted}))
}

/// How one executor attempt ended, as agentctl established it.
#[derive(Debug)]
pub struct Executed {
    pub execution: ExecutionId,
    /// How the invocation ended, with what the provider returned.
    pub invocation: Box<Outcome>,
    /// What agentctl observed of the workspace, the outcome, and whether a
    /// candidate was installed into the working tree.
    pub capture: Capture,
    /// The executor's own account, for whoever reads it now. Never recorded.
    pub summary: Option<String>,
    /// Why a candidate was refused or failed to install, for whoever reads
    /// it now. Never recorded.
    pub install_diagnostic: Option<String>,
}

/// A live executor attempt.
pub struct Executor {
    generation: GenerationId,
    execution: ExecutionId,
    /// Every path the baseline observed, which the capture observes again.
    observed: Vec<String>,
    invocation: runtime::Invocation,
    /// Removed once the attempt is finished or abandoned.
    workspace: Workspace,
}

/// Invokes the configured executor role afresh for an active generation of
/// `task`, through `executable` or else the provider's CLI on `PATH`, in a
/// workspace staged from the repository. The generation must own its task's
/// whole scope, and gets one attempt; see the module documentation for what
/// is recorded when. Nothing is recorded unless the workspace was staged.
pub fn start(
    project: &Project,
    store: &mut Store,
    task: TaskId,
    generation: GenerationId,
    executable: Option<PathBuf>,
) -> Result<Executor> {
    let authority = store.execution_authority(task, generation)?;
    let role = &project.config.agents.executor;
    let provider = role.provider.as_str().parse::<Provider>()?;
    let input = serde_json::to_string_pretty(&input(project, store, task, &authority)?)?;
    let since = crate::state::now();
    // Paths the executor may create are observed even where Git ignores
    // them, so that creating one is never missed.
    let (baseline, settled) = observe(|| source::snapshot(project, &authority))?;
    ensure!(
        settled,
        "the repository kept changing, so no baseline could be established"
    );
    // Staging writes nothing in the project, so it may precede the intent.
    let workspace = Workspace::stage(project, &baseline)?;
    let (execution, agent, entry) = store.begin_execution(
        task,
        generation,
        &authority,
        &baseline.entries,
        &baseline.head,
        since,
    )?;
    let launch = Launch {
        agent,
        provider,
        executable,
        model: role.model.to_string(),
        effort: Some(role.reasoning_effort),
        bootstrap: INSTRUCTIONS.into(),
        input,
        output_schema: result_schema(),
        cwd: workspace.root().to_path_buf(),
        workspace: runtime::Workspace::Editable,
    };
    let invocation = runtime::spawn_after(store, &launch, |store, invocation| {
        store.act(entry, Some(invocation))
    })?;
    Ok(Executor {
        generation,
        execution,
        observed: baseline.entries.into_iter().map(|(path, _)| path).collect(),
        invocation,
        workspace,
    })
}

impl Executor {
    pub fn control(&self) -> Control {
        self.invocation.control()
    }

    /// Waits for the invocation to end, then captures what changed in the
    /// workspace and records the outcome, and installs a candidate into the
    /// working tree. Should observing the workspace fail, nothing is
    /// established: the attempt stays attempted, with its outcome unknown,
    /// and nothing is installed. Should preparing to install fail, nothing
    /// is written and the install is never attempted. Should installing
    /// stop short with the working tree holding part of the candidate, the
    /// install stays attempted, with its outcome unknown, and this fails.
    pub fn finish(self, project: &Project, store: &mut Store) -> Result<Executed> {
        self.finish_holding(project, store, || ())
    }

    /// [`Executor::finish`], holding what `hold` returns from when the
    /// invocation ended until the attempt is finished: while observing the
    /// workspace and installing, never while the provider runs.
    pub fn finish_holding<H>(
        self,
        project: &Project,
        store: &mut Store,
        hold: impl FnOnce() -> H,
    ) -> Result<Executed> {
        let outcome = self.invocation.wait(store)?;
        let _held = hold();
        let report = outcome.payload.as_ref().map(Report::read);
        let result = match &report {
            None => ExecutorResult::None,
            Some(Err(_)) => ExecutorResult::Malformed,
            Some(Ok(report)) => ExecutorResult::Reported {
                status: match report.status {
                    Status::Succeeded => Reported::Succeeded,
                    Status::Failed => Reported::Failed,
                },
                claimed: &report.modified_paths,
            },
        };
        let (snapshot, settled) = observe(|| self.workspace.observe(project, &self.observed))?;
        let observed = Observed {
            entries: &snapshot.entries,
            head: &snapshot.head,
            settled,
            result,
        };
        let captured = store.finish_execution(self.execution, &observed)?;
        let install_diagnostic = match captured {
            ExecutionOutcome::Candidate => {
                let capture = capture(store, self.generation)?;
                install(project, store, self.execution, &capture, &self.workspace)?
            }
            _ => None,
        };
        Ok(Executed {
            execution: self.execution,
            invocation: Box::new(outcome),
            capture: capture(store, self.generation)?,
            summary: report.and_then(Result::ok).map(|r| r.summary),
            install_diagnostic,
        })
    }
}

/// The capture of `generation`'s execution, as recorded.
fn capture(store: &Store, generation: GenerationId) -> Result<Capture> {
    let execution = store
        .execution(generation)?
        .context("the execution vanished")?;
    let ExecutionStatus::Captured(capture) = execution.status else {
        bail!("execution {} was not captured", execution.id);
    };
    Ok(capture)
}

/// Installs the candidate `capture` of `execution` from `workspace` into
/// the working tree, journaled: intended, then prepared (validated, and
/// every byte to be written or restored kept as a durable recovery object,
/// writing nothing in the working tree), then attempted, and only then
/// written, and reconciled only once every changed path holds either its
/// candidate or its baseline content. Once attempted, installing reads
/// nothing from the workspace. Returns why nothing was installed, if
/// preparing failed, or installing was declined or failed.
pub(crate) fn install(
    project: &Project,
    store: &mut Store,
    execution: ExecutionId,
    capture: &Capture,
    workspace: &Workspace,
) -> Result<Option<String>> {
    install_with(project, store, execution, capture, workspace, |_| Ok(()))
}

/// [`install`], calling `acted` once the attempt is durable, before
/// anything in the working tree is written.
fn install_with(
    project: &Project,
    store: &mut Store,
    execution: ExecutionId,
    capture: &Capture,
    workspace: &Workspace,
    acted: impl FnOnce(&mut Store) -> Result<()>,
) -> Result<Option<String>> {
    let entry = store.begin_install(execution)?;
    let prepared = match workspace.prepare(project, &capture.changes) {
        // Nothing was written, nor attempted: the entry stays intended.
        Err(e) => return Ok(Some(format!("preparing to install failed: {e:#}"))),
        Ok(Preparation::Declined(verdict)) => {
            let (outcome, drifted, diagnostic) = verdict_of(verdict);
            store.decline_install(execution, outcome, &drifted)?;
            return Ok(diagnostic);
        }
        Ok(Preparation::Ready(prepared)) => prepared,
    };
    store.act(entry, None)?;
    acted(store)?;
    let (outcome, drifted, diagnostic) = verdict_of(prepared.apply(project)?);
    store.finish_install(execution, outcome, &drifted)?;
    Ok(diagnostic)
}

/// How an install ended, as recorded, and why nothing was installed.
fn verdict_of(installation: Installation) -> (InstallOutcome, Vec<String>, Option<String>) {
    match installation {
        Installation::Installed => (InstallOutcome::Installed, Vec::new(), None),
        Installation::Drifted(paths) => (InstallOutcome::Drifted, paths, None),
        Installation::Refused(why) => (InstallOutcome::Refused, Vec::new(), Some(why)),
        Installation::Failed(e) => (InstallOutcome::Failed, Vec::new(), Some(format!("{e:#}"))),
    }
}

/// Observes twice, `SETTLE` apart: the later observation, and whether the
/// two found the same entries. Agreement only says that nothing observed
/// changed meanwhile, nothing of who wrote it.
pub(crate) fn observe(snapshot: impl Fn() -> Result<Snapshot>) -> Result<(Snapshot, bool)> {
    let first = snapshot()?;
    thread::sleep(SETTLE);
    let second = snapshot()?;
    let settled = first.entries == second.entries;
    Ok((second, settled))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::tests::Fixture;
    use crate::planner::{self, Command};
    use crate::project::{STATE_DB, STATE_DIR};
    use crate::state::tests::{acquire, ready_plan, succeeded};
    use crate::state::{Content, Install};
    use sha2::Digest;
    use std::fs;

    const SCOPE: [&str; 3] = ["src/a.rs", "src/b.rs", "src/c.rs"];

    /// A captured candidate, staged in its workspace, modifying `src/a.rs`,
    /// deleting `src/b.rs` and creating `src/c.rs`, not yet installed.
    struct Staged {
        fx: Fixture,
        generation: GenerationId,
        execution: ExecutionId,
        capture: Capture,
        workspace: Workspace,
    }

    impl Staged {
        fn new() -> Self {
            let mut fx = Fixture::new("src");
            fs::create_dir_all(fx.project.root.join("src")).unwrap();
            fs::write(fx.project.root.join("src/a.rs"), "a").unwrap();
            fs::write(fx.project.root.join("src/b.rs"), "b").unwrap();
            let (_, tasks) = ready_plan(&mut fx.store, &[("t", &SCOPE, &[])]);
            let generation = fx.store.start_generation(tasks[0]).unwrap();
            acquire(&mut fx.store, generation, &SCOPE);
            let authority = fx.store.execution_authority(tasks[0], generation).unwrap();
            thread::sleep(Duration::from_millis(2));
            let since = crate::state::now();
            let baseline = source::snapshot(&fx.project, &authority).unwrap();
            let workspace = Workspace::stage(&fx.project, &baseline).unwrap();
            let (execution, agent, entry) = fx
                .store
                .begin_execution(
                    tasks[0],
                    generation,
                    &authority,
                    &baseline.entries,
                    &baseline.head,
                    since,
                )
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
            let ws = workspace.root();
            fs::write(ws.join("src/a.rs"), "A").unwrap();
            fs::remove_file(ws.join("src/b.rs")).unwrap();
            fs::write(ws.join("src/c.rs"), "C").unwrap();
            let paths: Vec<String> = baseline.entries.iter().map(|(p, _)| p.clone()).collect();
            let observed = workspace.observe(&fx.project, &paths).unwrap();
            let captured = fx
                .store
                .finish_execution(
                    execution,
                    &Observed {
                        entries: &observed.entries,
                        head: &observed.head,
                        settled: true,
                        result: ExecutorResult::Reported {
                            status: Reported::Succeeded,
                            claimed: &authority,
                        },
                    },
                )
                .unwrap();
            assert_eq!(captured, ExecutionOutcome::Candidate);
            let capture = capture(&fx.store, generation).unwrap();
            assert_eq!(capture.changes.len(), 3);
            Self {
                fx,
                generation,
                execution,
                capture,
                workspace,
            }
        }

        fn install(
            &mut self,
            acted: impl FnOnce(&mut Store) -> Result<()>,
        ) -> Result<Option<String>> {
            install_with(
                &self.fx.project,
                &mut self.fx.store,
                self.execution,
                &self.capture,
                &self.workspace,
                acted,
            )
        }

        /// What the working tree holds at each path of the scope.
        fn tree(&self) -> Vec<Option<Vec<u8>>> {
            tree(&self.fx.project)
        }

        fn status(&self) -> Install {
            capture(&self.fx.store, self.generation).unwrap().install
        }
    }

    fn tree(project: &Project) -> Vec<Option<Vec<u8>>> {
        SCOPE
            .iter()
            .map(|path| fs::read(project.root.join(path)).ok())
            .collect()
    }

    fn baseline_tree() -> Vec<Option<Vec<u8>>> {
        vec![Some(b"a".to_vec()), Some(b"b".to_vec()), None]
    }

    /// Whether every byte an attempted install of `execution` may write,
    /// candidate or baseline, is a recovery object, as the store alone
    /// identifies them: nothing of the workspace is consulted.
    fn recoverable(project: &Project, store: &Store, generation: GenerationId) -> bool {
        let capture = capture(store, generation).unwrap();
        let objects = project.root.join(STATE_DIR).join("objects");
        capture
            .changes
            .iter()
            .flat_map(|c| [&c.before, &c.after])
            .all(|content| match content {
                Content::Absent => true,
                Content::File(hash) => fs::read(objects.join(hash)).is_ok_and(|bytes| {
                    let digest = sha2::Sha256::digest(&bytes);
                    digest
                        .iter()
                        .map(|b| format!("{b:02x}"))
                        .collect::<String>()
                        == *hash
                }),
                _ => false,
            })
    }

    /// Makes recording any journal entry as attempted fail, through another
    /// connection, as a failing write would.
    fn refuse_acts(project: &Project) {
        let conn = rusqlite::Connection::open(project.root.join(STATE_DIR).join(STATE_DB)).unwrap();
        conn.execute_batch(
            "CREATE TRIGGER test_refuse_acts BEFORE UPDATE ON journal
             WHEN NEW.state = 'attempted'
             BEGIN SELECT RAISE(ABORT, 'forced ACT failure'); END;",
        )
        .unwrap();
    }

    #[cfg(unix)]
    fn unreadable(path: &std::path::Path) {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o000)).unwrap();
    }

    #[cfg(unix)]
    fn readable(path: &std::path::Path) {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o644)).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn failing_to_preserve_the_candidate_attempts_nothing() {
        let mut staged = Staged::new();
        unreadable(&staged.workspace.root().join("src/a.rs"));
        let diagnostic = staged.install(|_| panic!("never attempted")).unwrap();
        assert!(diagnostic.unwrap().contains("preparing to install failed"));
        assert_eq!(staged.status(), Install::NotAttempted);
        assert_eq!(staged.tree(), baseline_tree());
    }

    #[cfg(unix)]
    #[test]
    fn failing_to_preserve_the_baseline_attempts_nothing() {
        let mut staged = Staged::new();
        let real = staged.fx.project.root.join("src/a.rs");
        unreadable(&real);
        let diagnostic = staged.install(|_| panic!("never attempted")).unwrap();
        assert!(diagnostic.unwrap().contains("preparing to install failed"));
        assert_eq!(staged.status(), Install::NotAttempted);
        readable(&real);
        assert_eq!(staged.tree(), baseline_tree());
    }

    #[test]
    fn failing_to_record_the_attempt_writes_nothing() {
        let mut staged = Staged::new();
        refuse_acts(&staged.fx.project);
        let error = staged.install(|_| panic!("never attempted")).unwrap_err();
        assert!(format!("{error:#}").contains("forced ACT failure"));
        assert_eq!(staged.status(), Install::NotAttempted);
        assert_eq!(staged.tree(), baseline_tree());
    }

    #[test]
    fn attempts_are_recoverable_before_anything_is_written() {
        // Stopping right after the attempt is recorded, as a crash would:
        // nothing written yet, and everything needed to recover durable.
        let mut staged = Staged::new();
        let (project, generation) = (&staged.fx.project, staged.generation);
        let error = install_with(
            project,
            &mut staged.fx.store,
            staged.execution,
            &staged.capture,
            &staged.workspace,
            |store| {
                let found = capture(store, generation)?.install;
                ensure!(found == Install::OutcomeUnknown, "{found:?}");
                ensure!(tree(project) == baseline_tree(), "written before ACT");
                ensure!(recoverable(project, store, generation), "unrecoverable");
                bail!("crashed")
            },
        )
        .unwrap_err();
        assert_eq!(format!("{error:#}"), "crashed");
        assert_eq!(staged.status(), Install::OutcomeUnknown);
        assert_eq!(staged.tree(), baseline_tree());
        assert!(recoverable(
            &staged.fx.project,
            &staged.fx.store,
            generation
        ));
    }

    #[test]
    fn attempts_never_need_the_workspace() {
        // The workspace lost as soon as the attempt is recorded: the store
        // and its recovery objects still identify every change, and
        // installing completes from them alone.
        let mut staged = Staged::new();
        let (project, generation) = (&staged.fx.project, staged.generation);
        let root = staged.workspace.root().to_path_buf();
        let installed = install_with(
            project,
            &mut staged.fx.store,
            staged.execution,
            &staged.capture,
            &staged.workspace,
            |store| {
                fs::remove_dir_all(&root)?;
                ensure!(recoverable(project, store, generation), "unrecoverable");
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(installed, None);
        assert!(!root.exists());
        assert!(matches!(
            staged.status(),
            Install::Finished {
                outcome: InstallOutcome::Installed,
                ..
            }
        ));
        assert_eq!(
            staged.tree(),
            vec![Some(b"A".to_vec()), None, Some(b"C".to_vec())]
        );
    }

    #[test]
    fn partial_installs_are_restored_from_recovery_objects() {
        // Another writer creates `src/c.rs` once the attempt is recorded,
        // with the workspace gone: `src/a.rs` and `src/b.rs` are written,
        // then restored from their baseline objects, and the rival kept.
        let mut staged = Staged::new();
        let project = &staged.fx.project;
        let root = staged.workspace.root().to_path_buf();
        let installed = install_with(
            project,
            &mut staged.fx.store,
            staged.execution,
            &staged.capture,
            &staged.workspace,
            |_| {
                fs::remove_dir_all(&root)?;
                fs::write(project.root.join("src/c.rs"), "rival")?;
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(installed, None);
        let Install::Finished {
            outcome: InstallOutcome::Drifted,
            drifted,
            ..
        } = staged.status()
        else {
            panic!("{:?}", staged.status());
        };
        assert_eq!(drifted, ["src/c.rs"]);
        assert_eq!(
            staged.tree(),
            vec![
                Some(b"a".to_vec()),
                Some(b"b".to_vec()),
                Some(b"rival".to_vec())
            ]
        );
    }

    #[test]
    fn results_are_strict_and_bounded() {
        let validator = jsonschema::validator_for(&result_schema()).unwrap();
        let valid = json!({"status": "succeeded", "summary": "done",
                           "modified_paths": ["src/[id].rs", "src/a b.rs"]});
        assert!(validator.is_valid(&valid));
        let report = Report::read(&valid).unwrap();
        assert!(matches!(report.status, Status::Succeeded));
        assert_eq!(report.modified_paths, ["src/[id].rs", "src/a b.rs"]);

        // Neither the schema nor agentctl accepts any other shape, nor any
        // claim to acceptance.
        for invalid in [
            json!({"status": "accepted", "summary": "", "modified_paths": []}),
            json!({"status": "succeeded", "summary": "", "modified_paths": [], "verified": true}),
            json!({"status": "succeeded", "summary": ""}),
            json!({"status": "succeeded", "summary": "", "modified_paths": "src/a.rs"}),
        ] {
            assert!(!validator.is_valid(&invalid), "{invalid}");
            assert!(Report::read(&invalid).is_err(), "{invalid}");
        }
        // Within the schema, agentctl enforces the protocol's bounds.
        let many: Vec<String> = (0..=CLAIMED_LIMIT).map(|n| format!("src/{n}.rs")).collect();
        for (invalid, expected) in [
            (
                json!({"summary": "x".repeat(SUMMARY_LIMIT + 1)}),
                "summary too long",
            ),
            (json!({"modified_paths": many}), "too many paths"),
            (
                json!({"modified_paths": ["src/a.rs", "src/a.rs"]}),
                "path repeated",
            ),
            (json!({"modified_paths": ["../a.rs"]}), "not a canonical"),
            (
                json!({"modified_paths": ["/etc/passwd"]}),
                "not a canonical",
            ),
            (
                json!({"modified_paths": [format!("src/{}", "a".repeat(PATH_LIMIT))]}),
                "too long",
            ),
        ] {
            let mut result = json!({"status": "failed", "summary": "", "modified_paths": []});
            for (key, value) in invalid.as_object().unwrap() {
                result[key] = value.clone();
            }
            assert!(validator.is_valid(&result));
            let message = format!("{:#}", Report::read(&result).unwrap_err());
            assert!(message.contains(expected), "{message}");
        }
    }

    #[test]
    fn packets_carry_targeted_accepted_context() {
        let mut fx = Fixture::new("src");
        fx.accept(
            "src/lib.rs",
            Some("pub mod a;\npub fn parse() -> u8 { a::a() }\n"),
        );
        fx.accept("src/a.rs", Some("pub fn a() -> u8 { 1 }\n"));
        fx.accept("src/unrelated.rs", Some("pub fn unrelated() {}\n"));
        fx.accept("src/gone.rs", Some("pub fn gone() {}\n"));
        fx.accept("src/gone.rs", None);
        for path in ["src/lib.rs", "src/a.rs", "src/unrelated.rs"] {
            crate::graph::rust::index(&fx.project, &mut fx.store, path).unwrap();
        }
        // In the working tree only: not accepted, so never context.
        std::fs::write(fx.project.root.join("src/a.rs"), "pub fn draft() {}\n").unwrap();
        let plan = fx
            .store
            .create_plan(&crate::state::tests::objective("demo"))
            .unwrap();
        let paths = ["src/a.rs", "src/gone.rs", "src/new.rs"];
        let add = Command::AddTask {
            task: "change".into(),
            objective: "Change a".into(),
            context: "Keep it small.".into(),
            paths: paths.iter().map(|p| p.to_string()).collect(),
            depends_on: Vec::new(),
        };
        planner::apply(
            &fx.project,
            &mut fx.store,
            plan,
            &[add, Command::Finalize {}],
        )
        .unwrap();
        let task = fx.store.tasks(plan).unwrap()[0].id;
        let authority: Vec<String> = paths.iter().map(|p| p.to_string()).collect();

        let packet = input(&fx.project, &fx.store, task, &authority).unwrap();
        assert_eq!(
            packet["task"],
            json!({"key": "change", "objective": "Change a", "context": "Keep it small."})
        );
        assert_eq!(packet["intent"]["objective"], "demo");
        assert_eq!(packet["authority"]["mutable_paths"], json!(paths));
        let repository = &packet["repository"];
        let mutable = repository["mutable"].as_array().unwrap();
        assert_eq!(
            (&mutable[0]["accepted"], &mutable[0]["graph"]),
            (&json!("present"), &json!("current"))
        );
        let symbols: Vec<&Value> = mutable[0]["entities"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| &e["symbol"])
            .collect();
        assert!(symbols.contains(&&json!("a")) && !symbols.contains(&&json!("draft")));
        assert_eq!(
            (&mutable[1]["accepted"], &mutable[1]["graph"]),
            (&json!("absent"), &json!("absent"))
        );
        assert_eq!(
            mutable[2],
            json!({"path": "src/new.rs", "accepted": "untracked"})
        );
        // Other accepted sources appear by path; absent ones do not.
        assert_eq!(
            (&repository["sources"], &repository["sources_omitted"]),
            (&json!(["src/lib.rs", "src/unrelated.rs"]), &json!(0))
        );
    }
}
