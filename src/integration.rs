//! Final integration verification: a disposable, independent judge of a
//! plan's accepted result as a whole, and the only way a plan completes.
//!
//! Every task completing is not evidence that the human's intent is met.
//! Once a plan is settled (every task completed or cancelled, nothing of it
//! live or unknown, the source its work accepted synchronized with
//! CodeGraph) and its
//! planner proposed that its objective is met, agentctl invokes a fresh
//! verifier of the plan: a logical agent created for this one verification,
//! embodied by one fresh provider invocation that continues no executor,
//! verifier or planner, and is never resumed.
//!
//! Its packet is built from canonical facts alone: the human's intent, the
//! plan's tasks and where each ended, the paths each completed task's
//! accepted work changed, the human's decisions on the plan's concerns, and
//! a map of accepted source from CodeGraph. Nothing any executor, task
//! verifier or planner said is given: a task's pass is evidence about one
//! candidate, never about the result as a whole.
//!
//! The verifier works in a disposable copy of the accepted repository
//! outside the project, staged as a task verifier's view is with no
//! candidate (see `crate::verifier`), and every byte there has one of two
//! provenances. Accepted source, at every path with accepted state, comes
//! from recovery objects: never another pipeline's unaccepted bytes in the
//! working tree. Every other entry is a repository input: what the
//! repository holds beside accepted source that may still decide a build or
//! test, such as manifests, lock files, build scripts, fixtures and
//! configuration. Repository inputs are exactly the entries Git lists as
//! repository content (tracked, or untracked and not ignored, as literal
//! paths) outside agentctl's and Git's own state, plus `agentctl.toml`
//! whatever Git makes of it, less every path with accepted state; at a path
//! a candidate may have written, the input is what was there before any
//! did, from recovery objects. Each is captured by its kind and content
//! hash, and staged only if the copy is then observed to hold exactly that.
//! Ignored and generated files are no input and are never staged: their
//! changing invalidates nothing. Nothing is made CodeGraph source by being
//! an input.
//!
//! `agentctl.toml` configures the verification itself (its verifier role,
//! and the source roots its packet maps), so it is always an input, and
//! a verification is begun only while its captured bytes configure exactly
//! what this process loaded. Machine-global configuration is never staged.
//!
//! There the verifier may read, build, test and lint; any change to
//! repository source is a boundary violation. The verification is intended
//! together with its basis, the exact accepted source and repository inputs
//! staged, then journaled as attempted before any verifier process exists.
//! Once the invocation ends, agentctl observes the repository inputs
//! standing then, as it did when staging, and the store derives how the
//! verification ended, first from whether the plan, accepted source and
//! repository inputs still stand as verified: any input modified, deleted,
//! created, renamed or become eligible or ineligible since leaves nothing
//! judged. It completes the plan on a pass in the same transaction. A
//! failure changes nothing but the record: its blockers reach the plan's
//! next planner, which alone decides what to do about them.

use std::fs;
use std::path::PathBuf;
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};

use crate::config::Config;
use crate::executor::observe;
use crate::planner;
use crate::project::{CONFIG_FILE, Project};
use crate::runtime::{self, Control, Launch, Outcome, Provider};
use crate::source::{self, Snapshot, Workspace};
use crate::state::{
    AgentId, ChangeKind, Content, ExecutionStatus, GenerationState, HumanDecision, Integration,
    IntegrationId, IntegrationObserved, JournalId, PlanId, Store, TaskStatus, VerifierResult,
    ViewBasis,
};
use crate::verifier;

/// How long the workspace must hold still between two observations.
const SETTLE: Duration = Duration::from_millis(100);
/// How many changed paths the packet lists per task.
const CHANGES_LIMIT: usize = 64;

const INSTRUCTIONS: &str = "\
You are the integration verifier of agentctl, an engineering control plane: \
a disposable, independent reviewer judging whether a plan's accepted result, \
as a whole, meets its human's intent. You run once; nothing of any earlier \
session carries over, and you wrote none of it.

Your working directory is a disposable copy of the project's accepted \
repository: every task of the plan was implemented, verified on its own and \
accepted, and this is the assembled result, without Git's directory or \
agentctl's state. Your input is JSON: the human's intent (objective, \
constraints and completion criteria), the plan's tasks and the paths their \
accepted work changed, the human's decisions, and a map of the accepted \
source. Each task passing its own verification is no proof that the parts \
work together or that the intent is met.

Independently check the assembled result against the intent: run the \
repository's relevant builds, tests and linters yourself, check every \
completion criterion and constraint, and look for integration defects, \
regressions, broken invariants and missing work. After a failing check, \
keep examining every other concern you still can: report all blockers you \
find in this pass, not only the first.

You only observe. Never create, modify or delete repository files in your \
working directory: agentctl observes it before and after you run, and any \
change to repository source invalidates your verification. Build and test \
artifacts may go only where the repository's ignore rules already put them, \
or in the temporary directory. Fix nothing, and touch nothing outside your \
working directory. You complete nothing: your verdict is evidence.

Answer only with the structured result. `verdict` is `pass` only if you \
found no blocking defect and the intent is met, and `checked` must then show \
what you verified, with at least one check that passed; otherwise it is \
`fail`, with every blocker. Each check names what was checked, the command \
run or null, its outcome and concise evidence. Each blocker has a short \
unique id, a concise summary, the literal paths involved, concise evidence \
and a location or null. `non_blocking` holds observations that block \
nothing. Keep all text concise.";

/// Everything a fresh integration verifier of `plan` is given, from
/// canonical state alone: see the module documentation.
pub fn input(project: &Project, store: &Store, plan: PlanId) -> Result<Value> {
    let current = store.plan(plan)?;
    let tasks = store.tasks(plan)?;
    let snapshot = store.snapshot(plan, std::num::NonZeroU32::MIN)?;
    let key = |id| tasks.iter().find(|t| t.id == id).map(|t| t.key.as_str());
    let mut listed = Vec::new();
    for task in &tasks {
        let status = match snapshot.status(task.id) {
            Some(TaskStatus::Completed) => "completed",
            Some(TaskStatus::Cancelled) => "cancelled",
            _ => "unfinished",
        };
        // Only what agentctl captured of the accepted work: never what its
        // executor or verifier said.
        let mut changes = Vec::new();
        for generation in store.generations(task.id)? {
            if generation.state != GenerationState::Accepted {
                continue;
            }
            let Some(execution) = store.execution(generation.id)? else {
                continue;
            };
            if let ExecutionStatus::Captured(capture) = execution.status {
                for change in capture.changes.iter().take(CHANGES_LIMIT) {
                    let kind = match change.kind() {
                        ChangeKind::Created => "created",
                        ChangeKind::Modified => "modified",
                        ChangeKind::Deleted => "deleted",
                    };
                    changes.push(json!({"path": change.path, "change": kind}));
                }
            }
        }
        listed.push(json!({
            "task": task.key,
            "objective": task.objective,
            "context": task.context,
            "paths": task.scope,
            "depends_on": task.depends_on.iter().map(|&d| key(d)).collect::<Vec<_>>(),
            "status": status,
            "accepted_changes": changes,
        }));
    }
    let decisions: Vec<Value> = store
        .attention(plan)?
        .iter()
        .filter_map(|c| {
            let decision = match &c.decision.as_ref()?.0 {
                HumanDecision::Accept => json!({"decision": "accept"}),
                HumanDecision::Instruct(text) => {
                    json!({"decision": "instruct", "instruction": text})
                }
                HumanDecision::Stop => json!({"decision": "stop"}),
            };
            Some(json!({"concern": c.key, "reason": c.reason, "human": decision}))
        })
        .collect();
    let roots: Vec<&str> = project
        .config
        .codegraph
        .roots
        .iter()
        .map(|r| r.as_str())
        .collect();
    Ok(json!({
        "role": "integration_verifier",
        "intent": {
            "objective": current.intent.objective,
            "constraints": current.intent.constraints,
            "completion_criteria": current.intent.completion_criteria,
        },
        "plan": {"tasks": listed},
        "human_decisions": decisions,
        "source_roots": roots,
        "repository": planner::repository(project, store)?,
        "knowledge": [
            "Your working directory holds the plan's assembled accepted repository: the result to verify.",
            "`repository` maps accepted source with its code graph entities, marking any graph that is not current; `accepted_changes` lists the paths each task's accepted work changed.",
            "Every other repository file in your working directory, such as manifests, lock files, build scripts, fixtures and configuration, is exactly as it stood when this verification began.",
            "No account by any implementer, task verifier or planner is given: judge the result itself.",
        ],
        "verification": {
            "objective": "Independently determine whether the assembled accepted result meets the human's objective, respects every constraint and satisfies every completion criterion, without defects, regressions or broken invariants.",
            "checks": "Choose the relevant repository-local checks yourself, such as builds, tests, linters and each completion criterion, and run them in your working directory.",
        },
        "limits": [
            "Change no repository file in your working directory; write build and test artifacts only where the repository's ignore rules put them, or in the temporary directory.",
            "Fix nothing, touch nothing outside your working directory, and complete nothing.",
            "Report every blocker found in this pass.",
        ],
    }))
}

/// How one integration verification ended, as agentctl established it.
#[derive(Debug)]
pub struct Integrated {
    /// The verification as recorded.
    pub integration: Integration,
    /// How the verifier's invocation ended, with what the provider returned.
    pub invocation: Box<Outcome>,
    /// Why the verifier's result broke the protocol, for whoever reads it
    /// now. Never recorded.
    pub malformed: Option<String>,
}

/// An integration verification begun: its verifier running.
pub struct Integrator {
    id: IntegrationId,
    invocation: runtime::Invocation,
    /// Removed once the verification is finished or abandoned.
    workspace: Workspace,
    /// What the workspace was staged with.
    staged: Snapshot,
}

/// Verifies the accepted result of settled `plan`, whose latest replan
/// proposed its completion, by invoking the configured verifier role
/// afresh, through `executable` or else the provider's CLI on `PATH`, in a
/// workspace staged with the accepted repository. Nothing is recorded
/// unless the plan is eligible and the workspace could be staged with
/// accepted source as it still stands.
pub fn start(
    project: &Project,
    store: &mut Store,
    plan: PlanId,
    executable: Option<PathBuf>,
) -> Result<Integrator> {
    let role = &project.config.agents.verifier;
    let provider = Provider::resolve(&project.config, role.provider.as_str())?;
    let intended = intend(project, store, plan)?;
    let launch = Launch {
        agent: intended.agent,
        provider,
        executable,
        model: role.model.to_string(),
        effort: Some(role.reasoning_effort),
        bootstrap: INSTRUCTIONS.into(),
        input: intended.input,
        output_schema: verifier::result_schema(),
        cwd: intended.workspace.root().to_path_buf(),
        workspace: runtime::Workspace::Disposable,
        lifecycle: runtime::ROLE_LIFECYCLE,
    };
    let entry = intended.entry;
    let invocation = runtime::spawn_after(store, &launch, |store, invocation| {
        store.act(entry, Some(invocation))
    })?;
    failpoint!("integration.spawned");
    Ok(Integrator {
        id: intended.id,
        invocation,
        workspace: intended.workspace,
        staged: intended.staged,
    })
}

/// An integration verification intended, its verifier's workspace staged,
/// and no verifier yet.
struct Intended {
    id: IntegrationId,
    agent: AgentId,
    entry: JournalId,
    input: String,
    workspace: Workspace,
    staged: Snapshot,
}

/// INTEND, for [`start`]: stages the accepted repository and records the
/// verification with the snapshot it was staged with.
fn intend(project: &Project, store: &mut Store, plan: PlanId) -> Result<Intended> {
    let since = crate::state::now();
    let (tree, settled) = observe(|| source::snapshot(project, &control()))?;
    ensure!(
        settled,
        "the working tree kept changing, so the accepted repository could not be staged"
    );
    // Staging writes nothing in the project, so it may precede the intent.
    let (staged, workspace, basis) = verifier::stage(project, store, None, &tree, &[])?;
    configured(project, &workspace)?;
    // Built once staged, so that no graph fact it gives as current is of
    // accepted source other than what was staged: the intent below
    // requires accepted source to be exactly that still.
    let input = serde_json::to_string_pretty(&input(project, store, plan)?)?;
    let inputs = inputs(&staged, &basis);
    // What `configured` checked is bound as an input, never as accepted
    // source, whatever the roots (see `crate::source`).
    ensure!(
        inputs
            .iter()
            .any(|(path, content)| path == CONFIG_FILE && matches!(content, Content::File(_))),
        "`{CONFIG_FILE}` is not bound as this verification's configuration"
    );
    let (id, agent, entry) = store.begin_integration(plan, &basis.accepted, &inputs, since)?;
    Ok(Intended {
        id,
        agent,
        entry,
        input,
        workspace,
        staged,
    })
}

/// [`start`] and [`Integrator::finish`]: one final integration verification
/// of `plan`, completing it on a pass.
pub fn verify(
    project: &Project,
    store: &mut Store,
    plan: PlanId,
    executable: Option<PathBuf>,
) -> Result<Integrated> {
    start(project, store, plan, executable)?.finish(project, store)
}

impl Integrator {
    pub fn id(&self) -> IntegrationId {
        self.id
    }

    /// Observes and cancels the verifier.
    pub fn control(&self) -> Control {
        self.invocation.control()
    }

    /// Waits for the verifier to end, then observes its workspace and
    /// records how the verification ended, completing the plan on a pass.
    /// Should observing fail, nothing is established: the verification
    /// stays attempted, with its outcome unknown.
    pub fn finish(self, project: &Project, store: &mut Store) -> Result<Integrated> {
        let outcome = self.invocation.wait(store)?;
        let report = outcome.payload.as_ref().map(verifier::read);
        let result = match &report {
            None => VerifierResult::None,
            Some(Err(_)) => VerifierResult::Malformed,
            Some(Ok(report)) => VerifierResult::Reported(report),
        };
        let staged: Vec<String> = self.staged.entries.iter().map(|(p, _)| p.clone()).collect();
        let first = self.workspace.observe(project, &staged)?;
        thread::sleep(SETTLE);
        let second = self.workspace.observe(project, &staged)?;
        let mutated = verifier::mutated(&self.staged.entries, &[&first.entries, &second.entries]);
        let inputs = standing(project, store)?;
        let observed = IntegrationObserved {
            mutated: &mutated,
            inputs: inputs.as_deref(),
            result,
        };
        store.finish_integration(self.id, &observed)?;
        Ok(Integrated {
            integration: store.integration(self.id)?,
            invocation: Box::new(outcome),
            malformed: report.and_then(|r| r.err()).map(|e| format!("{e:#}")),
        })
    }
}

/// Paths observed whatever Git makes of them: `agentctl.toml`.
fn control() -> Vec<String> {
    vec![CONFIG_FILE.to_owned()]
}

/// The repository inputs of `view`, a view of the accepted repository on
/// `basis`: every entry it holds at a path with no accepted state.
fn inputs(view: &Snapshot, basis: &ViewBasis) -> Vec<(String, Content)> {
    view.entries
        .iter()
        .filter(|(path, content)| *content != Content::Absent && !basis.accepted.contains_key(path))
        .cloned()
        .collect()
}

/// The repository inputs standing now, as staging a verification would
/// capture them, or `None` when the working tree kept changing, so that
/// none could be established.
pub(crate) fn standing(project: &Project, store: &Store) -> Result<Option<Vec<(String, Content)>>> {
    let (tree, settled) = observe(|| source::snapshot(project, &control()))?;
    if !settled {
        return Ok(None);
    }
    let (view, basis) = verifier::accepted_view(store, &tree)?;
    Ok(Some(inputs(&view, &basis)))
}

/// Refuses a verification whose staged `agentctl.toml` does not configure
/// exactly what this process loaded, and therefore runs.
fn configured(project: &Project, workspace: &source::Workspace) -> Result<()> {
    let text = fs::read_to_string(workspace.root().join(CONFIG_FILE))
        .with_context(|| format!("reading the staged `{CONFIG_FILE}`"))?;
    let staged = Config::parse(&text).with_context(|| format!("invalid staged `{CONFIG_FILE}`"))?;
    ensure!(
        staged == project.config,
        "`{CONFIG_FILE}` changed since agentctl loaded it, so it no longer configures this \
         verification"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU32;
    use std::sync::{Arc, Barrier};

    use super::*;
    use crate::graph::Freshness;
    use crate::graph::tests::Fixture;
    use crate::planner::{Command, Rejection};
    use crate::scheduler::schedule;
    use crate::scheduler::tests::{
        Judge, PASS, Script, Simulated, project, project_in, schedule_with,
    };
    use crate::state::tests::{ended, err, succeeded};
    use crate::state::{
        ActionOutcome, ActionStatus, Check, CheckOutcome, Claim, Condition, FailureKind,
        IntegrationOutcome, IntegrationStatus, InvocationEnd, InvocationId, InvocationState,
        PlanState, Replan, ReplanId, TaskId, Verdict, VerifierReport,
    };

    /// A project whose plan of `tasks` `(key, scope, depends_on)` ran to
    /// the end through Blocks 11 to 13: every task completed, and the plan
    /// still running.
    fn settled(tasks: &[(&str, &[&str], &[&str])]) -> (Fixture, PlanId, Vec<TaskId>) {
        settled_in("src", tasks)
    }

    /// [`settled`], with source roots `roots`.
    fn settled_in(
        roots: &str,
        tasks: &[(&str, &[&str], &[&str])],
    ) -> (Fixture, PlanId, Vec<TaskId>) {
        let (fx, plan, ids) = project_in(roots, tasks, &[]);
        let sim = Simulated::new(&fx.project, &[]);
        let report = schedule_with(&fx.project, plan, 2, &sim);
        assert_eq!(report.snapshot.condition(), Condition::AllCompleted);
        assert_eq!(fx.store.plan(plan).unwrap().state, PlanState::Running);
        (fx, plan, ids)
    }

    /// Applies `commands` as a replan against the plan's current basis.
    fn replan(fx: &mut Fixture, plan: PlanId, commands: &[Command]) -> Result<Replan> {
        let basis = fx.store.replan_basis(plan)?;
        planner::apply_replan(&fx.project, &mut fx.store, plan, &basis, commands)
    }

    fn applied(replan: Result<Replan>) -> ReplanId {
        match replan.unwrap() {
            Replan::Applied(id) => id,
            Replan::Stale => panic!("stale"),
        }
    }

    fn propose(fx: &mut Fixture, plan: PlanId) -> ReplanId {
        let proposal = applied(replan(fx, plan, &[Command::ProposeCompletion {}]));
        assert_eq!(fx.store.completion_proposal(plan).unwrap(), Some(proposal));
        proposal
    }

    fn add_task(key: &str, path: &str) -> Command {
        Command::AddTask {
            task: key.into(),
            objective: key.into(),
            context: String::new(),
            paths: vec![path.into()],
            depends_on: Vec::new(),
        }
    }

    /// The repository inputs standing now, which must be observable.
    fn inputs_now(project: &Project, store: &Store) -> Vec<(String, Content)> {
        standing(project, store)
            .unwrap()
            .expect("the working tree kept changing")
    }

    /// Begins an integration verification of `plan` against accepted source
    /// and repository inputs as they stand, with its verifier's invocation
    /// running.
    fn begin(
        project: &Project,
        store: &mut Store,
        plan: PlanId,
    ) -> Result<(IntegrationId, InvocationId)> {
        let since = crate::state::now();
        let inputs = inputs_now(project, store);
        let accepted = store.view_basis(None)?.accepted;
        let (id, agent, entry) = store.begin_integration(plan, &accepted, &inputs, since)?;
        let invocation = store.start_invocation(agent, "claude", "fake", None)?;
        store.act(entry, Some(invocation))?;
        store.invocation_running(invocation)?;
        Ok((id, invocation))
    }

    /// How a simulated integration verifier ends.
    #[derive(Clone, Copy)]
    enum Ends {
        Pass,
        Fail,
        Malformed,
        /// A pass by a verifier that changed repository source.
        Mutated,
        /// Its invocation fails, or is cancelled or interrupted.
        Ended(InvocationState),
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
                    id: "missing-feature".into(),
                    summary: "the criterion is unmet".into(),
                    paths: vec!["src/a.rs".into()],
                    evidence: "cargo test fails".into(),
                    location: None,
                }],
            },
            non_blocking: Vec::new(),
        }
    }

    /// Ends integration verification `id`, as `how` says, observing the
    /// repository inputs standing then.
    fn end(
        project: &Project,
        store: &mut Store,
        id: IntegrationId,
        invocation: InvocationId,
        how: Ends,
    ) -> Result<IntegrationOutcome> {
        let run = match how {
            Ends::Ended(InvocationState::Failed) => InvocationEnd {
                failure: Some(FailureKind::ExitStatus),
                diagnostic: Some("failed".into()),
                exit_code: Some(1),
                ..ended(InvocationState::Failed)
            },
            Ends::Ended(InvocationState::Interrupted) => InvocationEnd {
                diagnostic: Some("lost".into()),
                ..ended(InvocationState::Interrupted)
            },
            Ends::Ended(state) => ended(state),
            _ => succeeded(),
        };
        store.finish_invocation(invocation, &run)?;
        let (pass, fail) = (report(Verdict::Pass), report(Verdict::Fail));
        let result = match how {
            Ends::Pass | Ends::Mutated => VerifierResult::Reported(&pass),
            Ends::Fail => VerifierResult::Reported(&fail),
            Ends::Malformed => VerifierResult::Malformed,
            Ends::Ended(_) => VerifierResult::None,
        };
        let mutated = match how {
            Ends::Mutated => vec!["src/a.rs".to_owned()],
            _ => Vec::new(),
        };
        let inputs = inputs_now(project, store);
        let observed = IntegrationObserved {
            mutated: &mutated,
            inputs: Some(&inputs),
            result,
        };
        store.finish_integration(id, &observed)
    }

    fn outcome(store: &Store, id: IntegrationId) -> IntegrationOutcome {
        match store.integration(id).unwrap().status {
            IntegrationStatus::Finished(result) => result.outcome,
            other => panic!("not finished: {other:?}"),
        }
    }

    fn sql(store: &Store, sql: &str) -> String {
        match store.raw().execute_batch(sql) {
            Ok(()) => "applied".into(),
            Err(e) => {
                // A refused batch leaves no transaction open behind it.
                let _ = store.raw().execute_batch("ROLLBACK");
                e.to_string()
            }
        }
    }

    fn count(store: &Store, sql: &str) -> i64 {
        store.raw().query_row(sql, [], |r| r.get(0)).unwrap()
    }

    fn is_rejection(result: Result<Replan>, contains: &str) {
        let error = result.unwrap_err();
        assert!(error.downcast_ref::<Rejection>().is_some(), "{error:#}");
        assert!(format!("{error:#}").contains(contains), "{error:#}");
    }

    #[test]
    fn an_exhausted_dag_never_completes_a_plan() {
        let (mut fx, plan, _) = settled(&[("a", &["src/a.rs"], &[]), ("b", &["src/b.rs"], &["a"])]);
        // Every task completed, and nothing proposes, verifies or completes.
        assert_eq!(fx.store.completion_proposal(plan).unwrap(), None);
        let message = err(begin(&fx.project, &mut fx.store, plan));
        assert!(
            message.contains("no current completion proposal"),
            "{message}"
        );
        let message = err(fx.store.set_plan_state(plan, PlanState::Completed));
        assert!(
            message.contains("final integration verification"),
            "{message}"
        );
        let refused = sql(
            &fx.store,
            &format!("UPDATE plans SET state = 'completed' WHERE id = {plan}"),
        );
        assert!(
            refused.contains("final integration verification"),
            "{refused}"
        );
        assert_eq!(fx.store.plan(plan).unwrap().state, PlanState::Running);
    }

    #[test]
    fn only_a_settled_plans_planner_proposes_completion_alone() {
        let scripts = [(
            "open",
            Script {
                judge: Judge::Unresolved,
                ..PASS
            },
        )];
        let tasks: [(&str, &[&str], &[&str]); 2] =
            [("a", &["src/a.rs"], &[]), ("open", &["src/o.rs"], &[])];
        let (mut fx, plan, _) = project(&tasks);
        let sim = Simulated::new(&fx.project, &scripts);
        let report = schedule(&fx.project.state_path(), plan, NonZeroU32::MIN, &sim).unwrap();
        assert_eq!(report.snapshot.condition(), Condition::Waiting);
        // Work of it is live: its generation is active, owns its scope and
        // holds a claim, and its verifier's end is unknown.
        is_rejection(
            replan(&mut fx, plan, &[Command::ProposeCompletion {}]),
            "not settled",
        );
        let refused = sql(
            &fx.store,
            &format!(
                "INSERT INTO completion_proposals SELECT max(id), {plan}, 0 FROM replans
                 WHERE plan_id = {plan}"
            ),
        );
        assert!(refused.contains("settled plan"), "{refused}");
        assert_eq!(fx.store.replans(plan).unwrap().len(), 0);

        let (mut fx, plan, _) = settled(&[("a", &["src/a.rs"], &[])]);
        // A proposal is the whole of its replan.
        is_rejection(
            replan(
                &mut fx,
                plan,
                &[Command::ProposeCompletion {}, add_task("more", "src/m.rs")],
            ),
            "whole of its replan",
        );
        let proposal = propose(&mut fx, plan);
        // It completes nothing, and a later replan leaves it no longer current.
        assert_eq!(fx.store.plan(plan).unwrap().state, PlanState::Running);
        let later = applied(replan(&mut fx, plan, &[add_task("more", "src/m.rs")]));
        assert!(later > proposal);
        assert_eq!(fx.store.completion_proposal(plan).unwrap(), None);
        let message = err(begin(&fx.project, &mut fx.store, plan));
        assert!(
            message.contains("no current completion proposal"),
            "{message}"
        );
    }

    #[test]
    fn a_current_pass_completes_the_plan_for_good() {
        let (mut fx, plan, ids) = settled(&[("a", &["src/a.rs"], &[])]);
        let proposal = propose(&mut fx, plan);
        let (id, invocation) = begin(&fx.project, &mut fx.store, plan).unwrap();
        let integration = fx.store.integration(id).unwrap();
        assert_eq!((integration.proposal, integration.number), (proposal, 1));
        assert_eq!(
            integration.status,
            IntegrationStatus::OutcomeUnknown {
                invocation: Some(invocation)
            }
        );
        // The verifier is fresh, and serves the plan alone.
        let planner = fx.store.planner(plan).unwrap();
        assert_ne!(integration.agent, planner);
        assert_eq!(
            end(&fx.project, &mut fx.store, id, invocation, Ends::Pass).unwrap(),
            IntegrationOutcome::Passed
        );
        assert_eq!(fx.store.plan(plan).unwrap().state, PlanState::Completed);
        let entry = fx.store.journal_entry(integration.journal).unwrap();
        let ActionStatus::Reconciled(_, reconciled) = entry.status else {
            panic!("not reconciled")
        };
        assert_eq!(reconciled.outcome, ActionOutcome::CompletedAsIntended);
        assert_eq!(
            count(
                &fx.store,
                &format!("SELECT count(*) FROM plan_completions WHERE verification_id = {id}")
            ),
            1
        );

        // Nothing autonomous continues a completed plan.
        assert!(matches!(
            fx.store.claim(ids[0], NonZeroU32::MIN).unwrap(),
            Claim::PlanNotRunning(PlanState::Completed)
        ));
        assert!(err(fx.store.start_plan(plan)).contains("completed"));
        is_rejection(
            replan(&mut fx, plan, &[add_task("more", "src/m.rs")]),
            "only a finalized plan",
        );
        for to in [PlanState::Running, PlanState::Paused, PlanState::Planning] {
            assert!(fx.store.set_plan_state(plan, to).is_err());
        }
        let refused = sql(
            &fx.store,
            &format!("UPDATE plans SET state = 'running' WHERE id = {plan}"),
        );
        assert!(refused.contains("for good"), "{refused}");
        // Its tasks' generations own nothing, and none is started to own.
        let generation = fx.store.start_generation(ids[0]);
        assert!(generation.is_err());
        let owned = count(&fx.store, "SELECT count(*) FROM ownership");
        assert_eq!(owned, 0);
        let refused = sql(
            &fx.store,
            &format!("INSERT INTO completion_proposals VALUES ({proposal}, {plan}, 0)"),
        );
        assert!(refused.contains("settled plan"), "{refused}");
        for forged in [
            format!(
                "INSERT OR REPLACE INTO plan_completions VALUES ({plan}, {id}, 'completed', 0)"
            ),
            format!("DELETE FROM plan_completions WHERE plan_id = {plan}"),
        ] {
            let refused = sql(&fx.store, &forged);
            assert!(refused.contains("complet"), "{forged}: {refused}");
        }
    }

    #[test]
    fn a_failure_is_durable_feedback_and_only_fresh_work_is_verified_again() {
        let (mut fx, plan, _) = settled(&[("a", &["src/a.rs"], &[])]);
        let first = propose(&mut fx, plan);
        let (failed, invocation) = begin(&fx.project, &mut fx.store, plan).unwrap();
        assert_eq!(
            end(&fx.project, &mut fx.store, failed, invocation, Ends::Fail).unwrap(),
            IntegrationOutcome::Failed
        );
        assert_eq!(fx.store.plan(plan).unwrap().state, PlanState::Running);
        // Nothing is retried: not the tasks, and not this proposal.
        let message = err(begin(&fx.project, &mut fx.store, plan));
        assert!(message.contains("once at a time"), "{message}");
        assert_eq!(
            fx.store
                .generations(fx.store.tasks(plan).unwrap()[0].id)
                .unwrap()
                .len(),
            1
        );

        // A fresh planner is given every blocker, as the verifier's claim.
        let feedback = planner::replan_input(&fx.project, &fx.store, plan).unwrap();
        let listed = &feedback["integration"][0];
        assert_eq!(listed["outcome"], "failed");
        assert_eq!(listed["current"], true);
        assert_eq!(listed["proposed_by_replan"], json!(first));
        let blockers = &listed["claimed_by_integration_verifier"]["blockers"];
        assert_eq!(blockers[0]["id"], "missing-feature");
        assert_eq!(blockers[0]["paths"], json!(["src/a.rs"]));

        // Its planner answers with corrective work, which runs normally.
        applied(replan(&mut fx, plan, &[add_task("fix", "src/fix.rs")]));
        let feedback = planner::replan_input(&fx.project, &fx.store, plan).unwrap();
        assert_eq!(feedback["integration"][0]["current"], false);
        let refused = sql(
            &fx.store,
            &format!("INSERT INTO plan_completions VALUES ({plan}, {failed}, 'completed', 0)"),
        );
        assert!(refused.contains("only the current pass"), "{refused}");
        let sim = Simulated::new(&fx.project, &[]);
        let report = schedule_with(&fx.project, plan, 1, &sim);
        assert_eq!(sim.launches(), ["fix"]);
        assert_eq!(report.snapshot.condition(), Condition::AllCompleted);

        let second = propose(&mut fx, plan);
        let (passed, invocation) = begin(&fx.project, &mut fx.store, plan).unwrap();
        let integration = fx.store.integration(passed).unwrap();
        assert_eq!((integration.proposal, integration.number), (second, 1));
        assert_eq!(
            end(&fx.project, &mut fx.store, passed, invocation, Ends::Pass).unwrap(),
            IntegrationOutcome::Passed
        );
        assert_eq!(fx.store.plan(plan).unwrap().state, PlanState::Completed);
        // The failure stays, as history.
        assert_eq!(outcome(&fx.store, failed), IntegrationOutcome::Failed);
    }

    #[test]
    fn nothing_but_a_verdict_judges_and_uncertainty_fails_closed() {
        let (mut fx, plan, _) = settled(&[("a", &["src/a.rs"], &[])]);
        propose(&mut fx, plan);
        let mut numbers = Vec::new();
        for (how, expected) in [
            (Ends::Malformed, IntegrationOutcome::MalformedResult),
            (
                Ends::Ended(InvocationState::Failed),
                IntegrationOutcome::InvocationFailed,
            ),
            (
                Ends::Ended(InvocationState::Cancelled),
                IntegrationOutcome::InvocationFailed,
            ),
            (
                Ends::Ended(InvocationState::Interrupted),
                IntegrationOutcome::InvocationFailed,
            ),
            (Ends::Mutated, IntegrationOutcome::BoundaryViolated),
        ] {
            let (id, invocation) = begin(&fx.project, &mut fx.store, plan).unwrap();
            assert_eq!(
                end(&fx.project, &mut fx.store, id, invocation, how).unwrap(),
                expected
            );
            let IntegrationStatus::Finished(result) = fx.store.integration(id).unwrap().status
            else {
                panic!("not finished")
            };
            // A malformed result, or none, invents no verdict.
            if !matches!(how, Ends::Mutated) {
                assert_eq!(result.report, None);
            }
            assert_eq!(fx.store.plan(plan).unwrap().state, PlanState::Running);
            numbers.push(fx.store.integration(id).unwrap().number);
        }
        assert_eq!(numbers, [1, 2, 3, 4, 5]);

        // An invocation whose end is unknown establishes nothing, and no
        // other verification begins meanwhile.
        let (id, invocation) = begin(&fx.project, &mut fx.store, plan).unwrap();
        let (pass, mutated) = (report(Verdict::Pass), Vec::new());
        let inputs = inputs_now(&fx.project, &fx.store);
        let observed = IntegrationObserved {
            mutated: &mutated,
            inputs: Some(&inputs),
            result: VerifierResult::Reported(&pass),
        };
        let message = err(fx.store.finish_integration(id, &observed));
        assert!(message.contains("has not ended"), "{message}");
        assert!(err(begin(&fx.project, &mut fx.store, plan)).contains("once at a time"));
        // A verifier with no result is never taken to have passed.
        fx.store
            .finish_invocation(invocation, &succeeded())
            .unwrap();
        let none = IntegrationObserved {
            mutated: &mutated,
            inputs: Some(&inputs),
            result: VerifierResult::None,
        };
        let message = err(fx.store.finish_integration(id, &none));
        assert!(message.contains("exactly when"), "{message}");
        let refused = sql(
            &fx.store,
            &format!(
                "INSERT INTO integration_results (verification_id, outcome, finished_at)
                 VALUES ({id}, 'invocation_failed', 0)"
            ),
        );
        assert!(refused.contains("consistently"), "{refused}");
        assert_eq!(
            fx.store.integration(id).unwrap().status,
            IntegrationStatus::OutcomeUnknown {
                invocation: Some(invocation)
            }
        );
        assert_eq!(fx.store.plan(plan).unwrap().state, PlanState::Running);
    }

    #[test]
    fn a_pass_about_a_changed_basis_completes_nothing() {
        // Accepted source changes while the verifier runs.
        let (mut fx, plan, _) = settled(&[("a", &["src/a.rs"], &[])]);
        propose(&mut fx, plan);
        let (id, invocation) = begin(&fx.project, &mut fx.store, plan).unwrap();
        fx.accept("src/other.rs", Some("// accepted elsewhere\n"));
        // Neither a forged judgment nor a completion takes.
        fx.store
            .finish_invocation(invocation, &succeeded())
            .unwrap();
        let refused = sql(
            &fx.store,
            &format!(
                "INSERT INTO integration_results (verification_id, outcome, verdict, checked,
                   blockers, non_blocking, finished_at)
                 VALUES ({id}, 'passed', 'pass', '[{{\"outcome\": \"passed\"}}]', '[]', '[]', 0)"
            ),
        );
        assert!(refused.contains("consistently"), "{refused}");
        // What was observed and claimed is kept, as no judgment.
        let pass = report(Verdict::Pass);
        let mutated = ["src/a.rs".to_owned()];
        let inputs = inputs_now(&fx.project, &fx.store);
        let observed = IntegrationObserved {
            mutated: &mutated,
            inputs: Some(&inputs),
            result: VerifierResult::Reported(&pass),
        };
        assert_eq!(
            fx.store.finish_integration(id, &observed).unwrap(),
            IntegrationOutcome::BasisChanged
        );
        let IntegrationStatus::Finished(result) = fx.store.integration(id).unwrap().status else {
            panic!("not finished")
        };
        assert_eq!(
            (result.mutated, result.report),
            (mutated.to_vec(), Some(pass))
        );
        assert_eq!(fx.store.plan(plan).unwrap().state, PlanState::Running);
        let refused = sql(
            &fx.store,
            &format!("INSERT INTO plan_completions VALUES ({plan}, {id}, 'completed', 0)"),
        );
        assert!(refused.contains("only the current pass"), "{refused}");
        // The same proposal is verified again against what now stands.
        let (again, invocation) = begin(&fx.project, &mut fx.store, plan).unwrap();
        assert_eq!(
            end(&fx.project, &mut fx.store, again, invocation, Ends::Pass).unwrap(),
            IntegrationOutcome::Passed
        );

        // A replan, or a concern it raises, while the verifier runs.
        for command in [
            add_task("more", "src/m.rs"),
            Command::RaiseAttention {
                concern: "unsure".into(),
                reason: "the intent is ambiguous".into(),
                evidence: vec!["two readings".into()],
                tasks: vec!["a".into()],
            },
        ] {
            let (mut fx, plan, _) = settled(&[("a", &["src/a.rs"], &[])]);
            propose(&mut fx, plan);
            let (id, invocation) = begin(&fx.project, &mut fx.store, plan).unwrap();
            applied(replan(&mut fx, plan, &[command]));
            assert_eq!(
                end(&fx.project, &mut fx.store, id, invocation, Ends::Pass).unwrap(),
                IntegrationOutcome::BasisChanged
            );
            let state = fx.store.plan(plan).unwrap().state;
            assert!(matches!(
                state,
                PlanState::Running | PlanState::NeedsAttention
            ));
            // A stale failure is no judgment either.
            let refused = sql(
                &fx.store,
                &format!("UPDATE plans SET state = 'completed' WHERE id = {plan}"),
            );
            assert!(
                refused.contains("final integration verification"),
                "{refused}"
            );
        }
    }

    /// Writes `text` at `path` in the project, or removes it for `None`.
    fn put(fx: &Fixture, path: &str, text: Option<&str>) {
        let file = fx.project.root.join(path);
        match text {
            Some(text) => {
                fs::create_dir_all(file.parent().unwrap()).unwrap();
                fs::write(file, text).unwrap();
            }
            None => fs::remove_file(file).unwrap(),
        }
    }

    fn git(fx: &Fixture, args: &[&str]) {
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(&fx.project.root)
            .args(args)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    }

    /// A settled plan changing `src/a.rs`, in a repository that also holds
    /// what builds and tests it besides accepted source: a tracked manifest,
    /// untracked lock file, build script and fixture, and ignored output.
    fn with_inputs() -> (Fixture, PlanId) {
        let (fx, plan, _) = settled(&[("a", &["src/a.rs"], &[])]);
        put(&fx, "Cargo.toml", Some("[package]\nname = \"demo\"\n"));
        put(&fx, "Cargo.lock", Some("version = 4\n"));
        put(&fx, "build.rs", Some("fn main() {}\n"));
        put(&fx, "tests/fixture [1].txt", Some("expected\n"));
        let ignore = fs::read_to_string(fx.project.root.join(".gitignore")).unwrap();
        put(&fx, ".gitignore", Some(&format!("{ignore}target/\n")));
        put(&fx, "target/debug/out.o", Some("object"));
        git(&fx, &["add", "--", "Cargo.toml"]);
        (fx, plan)
    }

    fn paths(inputs: &[(String, Content)]) -> Vec<&str> {
        inputs.iter().map(|(p, _)| p.as_str()).collect()
    }

    #[test]
    fn every_repository_input_is_bound_and_only_those() {
        let (mut fx, plan) = with_inputs();
        propose(&mut fx, plan);
        // Everything beside accepted source a build or test may read, as
        // literal paths, and nothing ignored, accepted or agentctl's.
        let inputs = inputs_now(&fx.project, &fx.store);
        assert_eq!(
            paths(&inputs),
            [
                ".gitignore",
                "Cargo.lock",
                "Cargo.toml",
                "agentctl.toml",
                "build.rs",
                "tests/fixture [1].txt"
            ]
        );
        let (id, invocation) = begin(&fx.project, &mut fx.store, plan).unwrap();
        let recorded: Vec<String> = fx
            .store
            .raw()
            .prepare("SELECT path FROM integration_inputs WHERE verification_id = ?1 ORDER BY path")
            .unwrap()
            .query_map([id], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(recorded, paths(&inputs));
        // Ignored output changing while the verifier builds is no change of
        // its basis.
        put(&fx, "target/debug/out.o", Some("rebuilt"));
        put(&fx, "target/debug/new.o", Some("new"));
        assert_eq!(
            end(&fx.project, &mut fx.store, id, invocation, Ends::Pass).unwrap(),
            IntegrationOutcome::Passed
        );
        assert_eq!(fx.store.plan(plan).unwrap().state, PlanState::Completed);
    }

    #[test]
    fn a_pass_about_changed_repository_inputs_completes_nothing() {
        let (mut fx, plan) = with_inputs();
        propose(&mut fx, plan);
        let exclude = fx.project.root.join(".git/info/exclude");
        let excluded = fs::read_to_string(&exclude).unwrap_or_default();
        type Change = Box<dyn Fn(&Fixture)>;
        let cases: Vec<(&str, Change, Change)> = vec![
            (
                "a tracked manifest modified",
                Box::new(|fx| put(fx, "Cargo.toml", Some("[package]\nname = \"other\"\n"))),
                Box::new(|fx| put(fx, "Cargo.toml", Some("[package]\nname = \"demo\"\n"))),
            ),
            (
                "an untracked lock file deleted",
                Box::new(|fx| put(fx, "Cargo.lock", None)),
                Box::new(|fx| put(fx, "Cargo.lock", Some("version = 4\n"))),
            ),
            (
                "an eligible untracked input created",
                Box::new(|fx| put(fx, "tests/new.rs", Some("#[test] fn t() {}\n"))),
                Box::new(|fx| put(fx, "tests/new.rs", None)),
            ),
            (
                "a build script renamed",
                Box::new(|fx| {
                    let root = &fx.project.root;
                    fs::rename(root.join("build.rs"), root.join("build2.rs")).unwrap();
                }),
                Box::new(|fx| {
                    let root = &fx.project.root;
                    fs::rename(root.join("build2.rs"), root.join("build.rs")).unwrap();
                }),
            ),
            (
                "a fixture made ineligible, no repository file changing",
                Box::new(move |fx| {
                    let exclude = fx.project.root.join(".git/info/exclude");
                    fs::create_dir_all(exclude.parent().unwrap()).unwrap();
                    fs::write(exclude, "tests/\n").unwrap();
                }),
                Box::new(move |fx| {
                    let exclude = fx.project.root.join(".git/info/exclude");
                    fs::write(exclude, &excluded).unwrap();
                }),
            ),
            (
                "agentctl.toml changed",
                Box::new(|fx| {
                    let path = fx.project.root.join(CONFIG_FILE);
                    let text = fs::read_to_string(&path).unwrap();
                    fs::write(path, format!("{text}# retuned\n")).unwrap();
                }),
                Box::new(|fx| {
                    let path = fx.project.root.join(CONFIG_FILE);
                    let text = fs::read_to_string(&path).unwrap();
                    fs::write(path, text.trim_end_matches("# retuned\n")).unwrap();
                }),
            ),
        ];
        let before = inputs_now(&fx.project, &fx.store);
        for (case, change, undo) in &cases {
            let (id, invocation) = begin(&fx.project, &mut fx.store, plan).unwrap();
            change(&fx);
            assert_ne!(inputs_now(&fx.project, &fx.store), before, "{case}");
            assert_eq!(
                end(&fx.project, &mut fx.store, id, invocation, Ends::Pass).unwrap(),
                IntegrationOutcome::BasisChanged,
                "{case}"
            );
            assert_eq!(fx.store.plan(plan).unwrap().state, PlanState::Running);
            // What was observed then is kept with the result.
            let observed: String = fx
                .store
                .raw()
                .query_row(
                    "SELECT inputs FROM integration_results WHERE verification_id = ?1",
                    [id],
                    |r| r.get(0),
                )
                .unwrap();
            let listed: Vec<Value> = serde_json::from_str(&observed).unwrap();
            let now = inputs_now(&fx.project, &fx.store);
            assert_eq!(listed.len(), now.len(), "{case}");
            undo(&fx);
            assert_eq!(inputs_now(&fx.project, &fx.store), before, "{case}");
        }
        // Inputs that could not be observed are no current basis either.
        let (id, invocation) = begin(&fx.project, &mut fx.store, plan).unwrap();
        fx.store
            .finish_invocation(invocation, &succeeded())
            .unwrap();
        let pass = report(Verdict::Pass);
        let unobserved = IntegrationObserved {
            mutated: &[],
            inputs: None,
            result: VerifierResult::Reported(&pass),
        };
        assert_eq!(
            fx.store.finish_integration(id, &unobserved).unwrap(),
            IntegrationOutcome::BasisChanged
        );
        // Restored exactly, the same proposal is verified and passes.
        let (id, invocation) = begin(&fx.project, &mut fx.store, plan).unwrap();
        assert_eq!(
            end(&fx.project, &mut fx.store, id, invocation, Ends::Pass).unwrap(),
            IntegrationOutcome::Passed
        );
        assert_eq!(fx.store.integration(id).unwrap().number, 8);
    }

    #[test]
    fn the_database_keeps_a_stale_repository_basis_stale() {
        // Repository inputs are recorded only as intended, never as
        // accepted source.
        let (mut fx, plan) = with_inputs();
        propose(&mut fx, plan);
        let accepted = fx.store.view_basis(None).unwrap().accepted;
        let inputs = inputs_now(&fx.project, &fx.store);
        let (intended, _, _) = fx
            .store
            .begin_integration(plan, &accepted, &inputs, crate::state::now())
            .unwrap();
        let hash = format!("{:064x}", 7);
        for forged in [
            format!(
                "INSERT INTO integration_inputs VALUES ({intended}, 'src/a.rs', 'file', '{hash}')"
            ),
            format!(
                "INSERT OR REPLACE INTO integration_inputs VALUES ({intended}, 'Cargo.toml',
                   'file', '{hash}')"
            ),
        ] {
            let refused = sql(&fx.store, &forged);
            assert!(refused.contains("repository inputs"), "{forged}: {refused}");
        }

        let (mut fx, plan) = with_inputs();
        propose(&mut fx, plan);
        let inputs = inputs_now(&fx.project, &fx.store);
        let (id, invocation) = begin(&fx.project, &mut fx.store, plan).unwrap();
        put(&fx, "Cargo.toml", Some("[package]\nname = \"other\"\n"));
        fx.store
            .finish_invocation(invocation, &succeeded())
            .unwrap();
        let stale = inputs_now(&fx.project, &fx.store);
        let listed = |inputs: &[(String, Content)]| -> String {
            let listed: Vec<Value> = inputs
                .iter()
                .map(|(path, content)| {
                    let (kind, hash) = match content {
                        Content::File(h) => ("file", Some(h.as_str())),
                        Content::Symlink(h) => ("symlink", Some(h.as_str())),
                        _ => ("other", None),
                    };
                    json!({"path": path, "kind": kind, "hash": hash})
                })
                .collect();
            Value::from(listed).to_string()
        };
        let passing = |inputs: &str| {
            format!(
                "INSERT INTO integration_results (verification_id, outcome, inputs, verdict,
                   checked, blockers, non_blocking, finished_at)
                 VALUES ({id}, 'passed', {inputs}, 'pass', '[{{\"outcome\": \"passed\"}}]',
                   '[]', '[]', 0)"
            )
        };
        let mut duplicated = inputs.clone();
        duplicated[0] = duplicated[1].clone();
        // A judgment is refused about inputs observed stale, unobserved,
        // partly or twice observed; and nothing recorded can be rewritten
        // to make the stale basis look current.
        for forged in [
            passing(&format!("'{}'", listed(&stale))),
            passing("NULL"),
            passing(&format!("'{}'", listed(&inputs[1..]))),
            passing(&format!("'{}'", listed(&duplicated))),
            format!(
                "UPDATE integration_inputs SET hash = '{hash}' WHERE verification_id = {id}
                   AND path = 'Cargo.toml'"
            ),
            format!("DELETE FROM integration_inputs WHERE verification_id = {id}"),
            format!(
                "INSERT INTO integration_inputs VALUES ({id}, 'tests/new.rs', 'file', '{hash}')"
            ),
            format!(
                "REPLACE INTO integration_inputs VALUES ({id}, 'Cargo.toml', 'file', '{hash}')"
            ),
        ] {
            let refused = sql(&fx.store, &forged);
            assert!(!refused.contains("applied"), "{forged}: {refused}");
        }
        let pass = report(Verdict::Pass);
        let observed = IntegrationObserved {
            mutated: &[],
            inputs: Some(&stale),
            result: VerifierResult::Reported(&pass),
        };
        assert_eq!(
            fx.store.finish_integration(id, &observed).unwrap(),
            IntegrationOutcome::BasisChanged
        );
        for forged in [
            format!(
                "UPDATE integration_results SET outcome = 'passed', inputs = '{}'
                 WHERE verification_id = {id}",
                listed(&inputs)
            ),
            format!(
                "REPLACE INTO integration_results (verification_id, outcome, inputs, verdict,
                   checked, blockers, non_blocking, finished_at)
                 VALUES ({id}, 'passed', '{}', 'pass', '[{{\"outcome\": \"passed\"}}]',
                   '[]', '[]', 0)",
                listed(&inputs)
            ),
            format!("DELETE FROM integration_results WHERE verification_id = {id}"),
            format!("INSERT INTO plan_completions VALUES ({plan}, {id}, 'completed', 0)"),
            format!("UPDATE plans SET state = 'completed' WHERE id = {plan}"),
        ] {
            let refused = sql(&fx.store, &forged);
            assert!(!refused.contains("applied"), "{forged}: {refused}");
        }
        assert_eq!(outcome(&fx.store, id), IntegrationOutcome::BasisChanged);
        assert_eq!(fx.store.plan(plan).unwrap().state, PlanState::Running);
        assert_eq!(count(&fx.store, "SELECT count(*) FROM plan_completions"), 0);
    }

    #[test]
    fn other_plans_unfinished_work_neither_blocks_nor_passes_for_a_plans_basis() {
        let (mut fx, one, _) = settled(&[("a", &["src/a.rs"], &[])]);
        // Another plan's acceptance published and never synchronized.
        let (two, _) = crate::state::tests::ready_plan(&mut fx.store, &[("b", &["src/b.rs"], &[])]);
        let unfinished = Script {
            unfinished: true,
            ..PASS
        };
        let sim = Simulated::new(&fx.project, &[("b", unfinished)]);
        schedule(&fx.project.state_path(), two, NonZeroU32::MIN, &sim).unwrap();
        let acceptances = count(
            &fx.store,
            "SELECT count(*) FROM acceptances a WHERE NOT EXISTS (SELECT 1 FROM acceptance_phases
               x WHERE x.generation_id = a.generation_id AND x.phase = 'completed')",
        );
        assert_eq!(acceptances, 1);
        // And a stale graph of accepted source no work of the plan changed.
        fx.accept("src/g.rs", Some("pub fn g() {}\n"));
        crate::graph::rust::index(&fx.project, &mut fx.store, "src/g.rs").unwrap();
        fx.accept("src/g.rs", Some("pub fn h() {}\n"));
        assert_eq!(fx.status("src/g.rs"), Freshness::Stale);

        propose(&mut fx, one);
        // The stale graph is marked, never given as current.
        let given = input(&fx.project, &fx.store, one).unwrap();
        let sources = given["repository"]["sources"].as_array().unwrap();
        let g = sources.iter().find(|s| s["path"] == "src/g.rs").unwrap();
        assert_eq!(g, &json!({"path": "src/g.rs", "graph": "stale"}));
        let (first, invocation) = begin(&fx.project, &mut fx.store, one).unwrap();
        // Work of a third plan accepted while the verifier runs changes the
        // repository it verified.
        let (three, _) =
            crate::state::tests::ready_plan(&mut fx.store, &[("c", &["src/c.rs"], &[])]);
        let sim = Simulated::new(&fx.project, &[]);
        // The unfinished acceptance still holds its claim on capacity.
        let report = schedule_with(&fx.project, three, 3, &sim);
        assert_eq!(report.snapshot.condition(), Condition::AllCompleted);
        assert_eq!(
            end(&fx.project, &mut fx.store, first, invocation, Ends::Pass).unwrap(),
            IntegrationOutcome::BasisChanged
        );

        // A fourth plan's candidate installed in the working tree, a path
        // with no accepted state, while the verifier runs: no input of the
        // plan's basis, which passes.
        let (second, invocation) = begin(&fx.project, &mut fx.store, one).unwrap();
        let (four, _) =
            crate::state::tests::ready_plan(&mut fx.store, &[("d", &["src/d.rs"], &[])]);
        let pending = Script {
            judge: Judge::Unresolved,
            ..PASS
        };
        let sim = Simulated::new(&fx.project, &[("d", pending)]);
        schedule(
            &fx.project.state_path(),
            four,
            NonZeroU32::new(3).unwrap(),
            &sim,
        )
        .unwrap();
        assert_eq!(sim.launches(), ["d"]);
        assert!(fx.project.root.join("src/d.rs").exists());
        assert!(!paths(&inputs_now(&fx.project, &fx.store)).contains(&"src/d.rs"));
        assert_eq!(
            end(&fx.project, &mut fx.store, second, invocation, Ends::Pass).unwrap(),
            IntegrationOutcome::Passed
        );
        assert_eq!(fx.store.plan(one).unwrap().state, PlanState::Completed);
    }

    #[test]
    fn a_plan_waits_for_its_own_acceptances_and_the_graph_of_its_work() {
        // Its own acceptance unfinished.
        let scripts = [(
            "a",
            Script {
                unfinished: true,
                ..PASS
            },
        )];
        let (mut fx, plan, _) = project(&[("a", &["src/a.rs"], &[])]);
        let sim = Simulated::new(&fx.project, &scripts);
        schedule(&fx.project.state_path(), plan, NonZeroU32::MIN, &sim).unwrap();
        is_rejection(
            replan(&mut fx, plan, &[Command::ProposeCompletion {}]),
            "not settled",
        );

        // Source its work accepted moves ahead of its graph.
        let (mut fx, plan, _) = settled(&[("a", &["src/a.rs"], &[])]);
        assert_eq!(fx.status("src/a.rs"), Freshness::Current(()));
        propose(&mut fx, plan);
        fx.accept("src/a.rs", Some("pub fn z() {}\n"));
        assert_eq!(fx.status("src/a.rs"), Freshness::Stale);
        let message = err(begin(&fx.project, &mut fx.store, plan));
        assert!(message.contains("CodeGraph"), "{message}");
        is_rejection(
            replan(&mut fx, plan, &[Command::ProposeCompletion {}]),
            "CodeGraph",
        );
        crate::graph::rust::index(&fx.project, &mut fx.store, "src/a.rs").unwrap();
        propose(&mut fx, plan);
        let (id, invocation) = begin(&fx.project, &mut fx.store, plan).unwrap();
        assert_eq!(
            end(&fx.project, &mut fx.store, id, invocation, Ends::Pass).unwrap(),
            IntegrationOutcome::Passed
        );
    }

    #[test]
    fn one_plan_has_one_verification_at_a_time_and_plans_verify_independently() {
        let (mut fx, one, _) = settled(&[("a", &["src/a.rs"], &[])]);
        let (two, _) = crate::state::tests::ready_plan(&mut fx.store, &[("b", &["src/b.rs"], &[])]);
        let sim = Simulated::new(&fx.project, &[]);
        schedule_with(&fx.project, two, 1, &sim);
        propose(&mut fx, one);
        propose(&mut fx, two);

        // Racing verifications of one plan: one begins.
        let (path, project) = (fx.project.state_path(), &fx.project);
        let barrier = Arc::new(Barrier::new(2));
        let begun: Vec<Result<(IntegrationId, InvocationId)>> = thread::scope(|scope| {
            let handles: Vec<_> = (0..2)
                .map(|_| {
                    let (barrier, path) = (Arc::clone(&barrier), &path);
                    scope.spawn(move || {
                        let mut store = Store::open(path).unwrap();
                        barrier.wait();
                        begin(project, &mut store, one)
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        let won: Vec<_> = begun.iter().filter_map(|b| b.as_ref().ok()).collect();
        assert_eq!(won.len(), 1, "{begun:?}");
        let (first, first_invocation) = *won[0];

        // Another plan verifies meanwhile.
        let (second, second_invocation) = begin(&fx.project, &mut fx.store, two).unwrap();
        for (id, invocation, plan) in [
            (first, first_invocation, one),
            (second, second_invocation, two),
        ] {
            assert_eq!(
                end(&fx.project, &mut fx.store, id, invocation, Ends::Pass).unwrap(),
                IntegrationOutcome::Passed
            );
            assert_eq!(fx.store.plan(plan).unwrap().state, PlanState::Completed);
        }

        // A result is recorded once, and never grants anything again.
        let message = err(end(
            &fx.project,
            &mut fx.store,
            first,
            first_invocation,
            Ends::Pass,
        ));
        assert!(message.contains("already ended") || message.contains("not live"));
        let pass = report(Verdict::Pass);
        let inputs = inputs_now(&fx.project, &fx.store);
        let observed = IntegrationObserved {
            mutated: &[],
            inputs: Some(&inputs),
            result: VerifierResult::Reported(&pass),
        };
        assert!(err(fx.store.finish_integration(first, &observed)).contains("already ended"));
        assert_eq!(count(&fx.store, "SELECT count(*) FROM plan_completions"), 2);
        let refused = sql(
            &fx.store,
            &format!("INSERT INTO plan_completions VALUES ({two}, {first}, 'completed', 0)"),
        );
        assert!(!refused.contains("applied"), "{refused}");
    }

    #[test]
    fn the_database_refuses_completion_without_a_current_pass() {
        let (mut fx, plan, _) = settled(&[("a", &["src/a.rs"], &[])]);
        let completing = |verification: &str| {
            format!(
                "BEGIN;
                 INSERT INTO plan_completions VALUES ({plan}, {verification}, 'completed', 0);
                 UPDATE plans SET state = 'completed' WHERE id = {plan};
                 COMMIT;"
            )
        };
        // Unverified, created completed, or completed around the guard.
        for forged in [
            format!("UPDATE plans SET state = 'completed' WHERE id = {plan}"),
            "INSERT INTO plans (objective, constraints, completion_criteria, state, created_at,
               updated_at) VALUES ('forged', '[]', '[]', 'completed', 0, 0)"
                .into(),
            format!(
                "REPLACE INTO plans (id, objective, constraints, completion_criteria, state,
                   created_at, updated_at)
                 SELECT id, objective, constraints, completion_criteria, 'completed', created_at,
                   updated_at FROM plans WHERE id = {plan}"
            ),
            completing("NULL"),
        ] {
            let refused = sql(&fx.store, &forged);
            assert!(!refused.contains("applied"), "{forged}: {refused}");
        }

        // After a failure, and with evidence rewritten.
        propose(&mut fx, plan);
        let (failed, invocation) = begin(&fx.project, &mut fx.store, plan).unwrap();
        end(&fx.project, &mut fx.store, failed, invocation, Ends::Fail).unwrap();
        for forged in [
            completing(&failed.to_string()),
            format!("UPDATE integration_results SET outcome = 'passed' WHERE verification_id = {failed}"),
            format!("DELETE FROM integration_results WHERE verification_id = {failed}"),
            format!(
                "INSERT OR REPLACE INTO integration_results (verification_id, outcome, verdict,
                   checked, blockers, non_blocking, finished_at)
                 VALUES ({failed}, 'passed', 'pass', '[{{\"outcome\": \"passed\"}}]', '[]', '[]', 0)"
            ),
            format!("DELETE FROM integration_verifications WHERE id = {failed}"),
            format!("UPDATE integration_verifications SET number = 2 WHERE id = {failed}"),
            format!("DELETE FROM integration_sources WHERE verification_id = {failed}"),
            format!(
                "INSERT INTO integration_sources VALUES ({failed}, 'src/forged.rs', NULL)"
            ),
            "DELETE FROM completion_proposals".into(),
            "UPDATE journal SET state = 'attempted' WHERE state = 'reconciled'".into(),
        ] {
            let refused = sql(&fx.store, &forged);
            assert!(!refused.contains("applied"), "{forged}: {refused}");
        }
        assert_eq!(outcome(&fx.store, failed), IntegrationOutcome::Failed);

        // While a concern blocks it, even a verification begun before.
        applied(replan(&mut fx, plan, &[add_task("fix", "src/fix.rs")]));
        let sim = Simulated::new(&fx.project, &[]);
        schedule_with(&fx.project, plan, 1, &sim);
        propose(&mut fx, plan);
        let (live, invocation) = begin(&fx.project, &mut fx.store, plan).unwrap();
        applied(replan(
            &mut fx,
            plan,
            &[Command::RaiseAttention {
                concern: "unsure".into(),
                reason: "the intent is ambiguous".into(),
                evidence: vec!["two readings".into()],
                tasks: vec!["fix".into()],
            }],
        ));
        fx.store
            .finish_invocation(invocation, &succeeded())
            .unwrap();
        let forged = format!(
            "INSERT INTO integration_results (verification_id, outcome, verdict, checked,
               blockers, non_blocking, finished_at)
             VALUES ({live}, 'passed', 'pass', '[{{\"outcome\": \"passed\"}}]', '[]', '[]', 0)"
        );
        assert!(sql(&fx.store, &forged).contains("consistently"));
        assert!(!sql(&fx.store, &completing(&live.to_string())).contains("applied"));
        assert_eq!(
            fx.store.plan(plan).unwrap().state,
            PlanState::NeedsAttention
        );
        assert_eq!(count(&fx.store, "SELECT count(*) FROM plan_completions"), 0);
    }

    /// Attempts intended verification `entry`: its verifier's invocation
    /// running.
    fn activate(store: &mut Store, agent: AgentId, entry: JournalId) -> InvocationId {
        let invocation = store
            .start_invocation(agent, "claude", "fake", None)
            .unwrap();
        store.act(entry, Some(invocation)).unwrap();
        store.invocation_running(invocation).unwrap();
        invocation
    }

    #[test]
    fn a_snapshot_is_sealed_by_the_transaction_intending_it() {
        let (mut fx, plan) = with_inputs();
        propose(&mut fx, plan);
        let intended = intend(&fx.project, &mut fx.store, plan).unwrap();
        let (id, entry) = (intended.id, intended.entry);
        // Committed, and not yet attempted: the window before ACT.
        assert_eq!(
            fx.store.integration(id).unwrap().status,
            IntegrationStatus::Intended
        );
        let state: String = fx
            .store
            .raw()
            .query_row("SELECT state FROM journal WHERE id = ?1", [entry], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(state, "intended");
        let snapshot = |store: &Store| -> Vec<(String, String, Option<String>)> {
            store
                .raw()
                .prepare(
                    "SELECT 'input:' || path, kind, hash FROM integration_inputs
                     WHERE verification_id = ?1
                     UNION ALL SELECT 'source:' || path, '', hash FROM integration_sources
                     WHERE verification_id = ?1 ORDER BY 1",
                )
                .unwrap()
                .query_map([id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap()
        };
        let before = snapshot(&fx.store);
        // Source accepted since, which an appended row would claim verified.
        fx.accept("src/late.rs", Some("// accepted later\n"));
        let late = fx
            .store
            .accepted_source("src/late.rs")
            .unwrap()
            .unwrap()
            .hash
            .unwrap();
        let hash = format!("{:064x}", 7);
        let next: i64 = count(
            &fx.store,
            "SELECT max(id) + 1 FROM integration_verifications",
        );
        for forged in [
            format!(
                "INSERT INTO integration_inputs VALUES ({id}, 'tests/new.rs', 'file', '{hash}')"
            ),
            format!(
                "INSERT OR REPLACE INTO integration_inputs VALUES ({id}, 'tests/new.rs', 'file',
                   '{hash}')"
            ),
            format!(
                "REPLACE INTO integration_inputs VALUES ({id}, 'tests/new.rs', 'file', '{hash}')"
            ),
            format!(
                "INSERT OR REPLACE INTO integration_inputs VALUES ({id}, 'Cargo.toml', 'file',
                   '{hash}')"
            ),
            format!(
                "REPLACE INTO integration_inputs VALUES ({id}, 'Cargo.toml', 'file', '{hash}')"
            ),
            format!(
                "UPDATE integration_inputs SET hash = '{hash}' WHERE verification_id = {id}
                   AND path = 'Cargo.toml'"
            ),
            format!("DELETE FROM integration_inputs WHERE verification_id = {id}"),
            format!("INSERT INTO integration_sources VALUES ({id}, 'src/late.rs', '{late}')"),
            format!(
                "INSERT OR REPLACE INTO integration_sources VALUES ({id}, 'src/late.rs', '{late}')"
            ),
            format!("REPLACE INTO integration_sources VALUES ({id}, 'src/late.rs', '{late}')"),
            format!("UPDATE integration_sources SET hash = NULL WHERE verification_id = {id}"),
            format!("DELETE FROM integration_sources WHERE verification_id = {id}"),
            // Nor is construction authority manufactured: rows for the
            // next id commit only with a verification sealing them, which
            // is refused unless it takes that id, is fresh and settled.
            format!(
                "BEGIN; INSERT INTO integration_inputs VALUES ({next}, 'x.txt', 'file', '{hash}');
                 COMMIT;"
            ),
            format!(
                "INSERT INTO integration_verifications (plan_id, replan_id, number, basis,
                   agent_id, journal_id, started_at)
                 SELECT plan_id, replan_id, number + 1, basis, agent_id, journal_id, 0
                 FROM integration_verifications WHERE id = {id}"
            ),
            format!("UPDATE integration_verifications SET id = {next} WHERE id = {id}"),
        ] {
            let refused = sql(&fx.store, &forged);
            assert!(!refused.contains("applied"), "{forged}: {refused}");
        }
        assert_eq!(snapshot(&fx.store), before);
        assert_eq!(
            count(
                &fx.store,
                &format!("SELECT count(*) FROM integration_inputs WHERE verification_id = {next}")
            ),
            0
        );
        // The truthful order stands: attempted only now, and its basis, the
        // later acceptance, no longer current.
        let invocation = activate(&mut fx.store, intended.agent, entry);
        assert_eq!(
            end(&fx.project, &mut fx.store, id, invocation, Ends::Pass).unwrap(),
            IntegrationOutcome::BasisChanged
        );
        let (again, invocation) = begin(&fx.project, &mut fx.store, plan).unwrap();
        assert_eq!(
            end(&fx.project, &mut fx.store, again, invocation, Ends::Pass).unwrap(),
            IntegrationOutcome::Passed
        );
        assert_eq!(fx.store.plan(plan).unwrap().state, PlanState::Completed);
    }

    #[test]
    fn agentctl_toml_is_bound_as_configuration_even_under_root_dot() {
        let (mut fx, plan, _) = settled_in(".", &[("a", &["src/a.rs"], &[])]);
        assert_eq!(fx.project.config.codegraph.roots.to_string(), ".");
        // Never accepted source, graphed, or in any task's scope.
        assert_eq!(fx.store.accepted_source("agentctl.toml").unwrap(), None);
        assert!(fx.store.accepted_source(".gitignore").unwrap().is_some());
        let given = input(&fx.project, &fx.store, plan).unwrap();
        let sources = given["repository"]["sources"].as_array().unwrap();
        assert!(
            sources.iter().all(|s| s["path"] != "agentctl.toml"),
            "{sources:?}"
        );
        is_rejection(
            replan(&mut fx, plan, &[add_task("retune", "agentctl.toml")]),
            "project-control file",
        );
        propose(&mut fx, plan);
        let config = fx.project.root.join(CONFIG_FILE);
        let original = fs::read(&config).unwrap();
        let other = fx.project.root.join("other.toml");
        type Change = Box<dyn Fn(&Fixture)>;
        let cases: Vec<(&str, Change)> = vec![
            (
                "modified",
                Box::new(|fx| {
                    let path = fx.project.root.join(CONFIG_FILE);
                    let text = fs::read_to_string(&path).unwrap();
                    fs::write(path, format!("{text}# retuned\n")).unwrap();
                }),
            ),
            (
                "deleted",
                Box::new(|fx| fs::remove_file(fx.project.root.join(CONFIG_FILE)).unwrap()),
            ),
            (
                "replaced",
                Box::new(move |fx| {
                    let root = &fx.project.root;
                    let text = fs::read_to_string(root.join(CONFIG_FILE)).unwrap();
                    fs::write(root.join("other.toml"), format!("# replacement\n{text}")).unwrap();
                    fs::rename(root.join("other.toml"), root.join(CONFIG_FILE)).unwrap();
                }),
            ),
        ];
        for (case, change) in &cases {
            let intended = intend(&fx.project, &mut fx.store, plan).unwrap();
            // Bound as a repository input, and staged as loaded.
            let bound: Vec<String> = fx
                .store
                .raw()
                .prepare("SELECT path FROM integration_inputs WHERE verification_id = ?1")
                .unwrap()
                .query_map([intended.id], |r| r.get(0))
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap();
            assert!(bound.iter().any(|p| p == CONFIG_FILE), "{case}: {bound:?}");
            let staged = intended.workspace.root().join(CONFIG_FILE);
            assert_eq!(fs::read(&staged).unwrap(), original, "{case}");
            let invocation = activate(&mut fx.store, intended.agent, intended.entry);
            change(&fx);
            // The verifier still sees what it was staged with.
            assert_eq!(fs::read(&staged).unwrap(), original, "{case}");
            assert_eq!(
                end(
                    &fx.project,
                    &mut fx.store,
                    intended.id,
                    invocation,
                    Ends::Pass
                )
                .unwrap(),
                IntegrationOutcome::BasisChanged,
                "{case}"
            );
            assert_eq!(
                fx.store.plan(plan).unwrap().state,
                PlanState::Running,
                "{case}"
            );
            fs::write(&config, &original).unwrap();
            assert!(!other.exists());
        }
        // A configuration other than the one loaded constructs nothing.
        let text = String::from_utf8(original.clone()).unwrap();
        let mut changed = fx.project.config.clone();
        changed.codegraph.roots = "src".parse().unwrap();
        assert_ne!(changed, fx.project.config);
        fs::write(&config, changed.to_toml()).unwrap();
        let verifications = count(&fx.store, "SELECT count(*) FROM integration_verifications");
        let message = err(intend(&fx.project, &mut fx.store, plan).map(|_| ()));
        assert!(
            message.contains("changed since agentctl loaded it"),
            "{message}"
        );
        assert_eq!(
            count(&fx.store, "SELECT count(*) FROM integration_verifications"),
            verifications
        );
        fs::write(&config, text).unwrap();
        // As loaded and unchanged, it passes.
        let intended = intend(&fx.project, &mut fx.store, plan).unwrap();
        let invocation = activate(&mut fx.store, intended.agent, intended.entry);
        assert_eq!(
            end(
                &fx.project,
                &mut fx.store,
                intended.id,
                invocation,
                Ends::Pass
            )
            .unwrap(),
            IntegrationOutcome::Passed
        );
        assert_eq!(fx.store.plan(plan).unwrap().state, PlanState::Completed);
    }

    #[test]
    fn the_verifier_is_given_accepted_facts_only() {
        let (mut fx, plan, _) = settled(&[("a", &["src/a.rs"], &[])]);
        propose(&mut fx, plan);
        let input = input(&fx.project, &fx.store, plan).unwrap();
        assert_eq!(input["intent"]["objective"], "intent");
        let task = &input["plan"]["tasks"][0];
        assert_eq!(task["status"], "completed");
        assert_eq!(
            task["accepted_changes"],
            json!([{"path": "src/a.rs", "change": "modified"}])
        );
        // No executor's, task verifier's or planner's own words.
        let text = input.to_string();
        for claimed in [
            "claimed",
            "summary",
            "modified_paths",
            "verdict",
            "explanation",
        ] {
            assert!(!text.contains(claimed), "{claimed}: {text}");
        }
    }
}
