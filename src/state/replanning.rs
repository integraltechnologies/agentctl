//! Replanning: a finalized plan's task DAG as its planner revises it in
//! response to what happened, and the explicit authorizations that alone
//! let a task that already ran be claimed again.
//!
//! A replan is one transaction. Its proposal was made against the plan's
//! state as [`Store::replan_basis`] identified it; unless that state is
//! unchanged, nothing is applied ([`Replan::Stale`]). Each command is then
//! validated against the state its predecessors leave, and the result as a
//! whole: every command applies, or none does.
//!
//! What a replan may change is the frontier nothing is running for: a task
//! no generation served, one whose latest generation's pipeline
//! conclusively stopped short of acceptance (its claim released, and
//! `scheduler_outcomes` still establishing how it ended), and one already
//! authorized to run again. A completed task, a task whose work is live or
//! unresolved, a task never scheduled (served beside the scheduler) and a
//! cancelled task are beyond it. Human intent is beyond every
//! command, and nothing a generation or its agents recorded is changed.
//!
//! Before a replan first changes a task, the definition it had is recorded
//! as a revision, if none records it yet; the definition the replan leaves
//! is recorded as the next, naming the replan. A claim binds each fresh
//! generation to the revision it executes, so a revision never changes
//! what an earlier generation is known to have executed.
//!
//! Authorizing a retry, or cancelling a stopped task, abandons its stopped
//! generation: the planner decided that attempt will never be accepted.
//! The replan records the abandonment, releases what the generation owns
//! and ends it (rejected when a verifier failed it, failed otherwise),
//! which the schema lets nothing else do to a scheduled generation short
//! of acceptance, and restores the candidate it installed, if any, to the
//! last accepted state, so that nothing of a failed attempt becomes the
//! baseline of later work.
//!
//! Authorizing starts nothing, and authorizes one exact revision: the
//! task's definition when authorized. A scheduler's claim uses the
//! authorization for one fresh generation bound to that revision, once.
//! Revising the task again leaves the authorization stale, never usable,
//! so the planner authorizes the new definition explicitly or it never
//! runs; within one replan, a task is therefore revised before it is
//! retried, never after.
//!
//! Restoring happens inside the replan's transaction, once every command
//! was validated and before it commits: while it holds the database's
//! write lock, no other agentctl process commits anything, least of all a
//! claim or an install at those paths, which the abandoned generation owns
//! until the replan commits. Each path is restored only while it still
//! holds exactly what the candidate installed, or holds its accepted state
//! already; should one hold anything else, nothing is written and nothing
//! is replanned. SQLite cannot roll a working tree back: should agentctl
//! stop, or writing fail, part way, nothing canonical changed, the
//! generation stays stopped owning its paths, each holding its candidate
//! or its accepted state, and replanning again restores the rest.

use std::collections::BTreeSet;

use anyhow::{Result, anyhow, bail, ensure};
use rusqlite::types::Value;
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use sha2::{Digest, Sha256};

use super::attention::{blocked, raise, resume};
use super::ownership::release;
use super::planning::{self, lookup};
use super::scheduling::{dag_defects, task_status};
use super::{
    ActionOutcome, Content, Evidence, GenerationId, GenerationState, InvocationId, JournalId,
    PipelineOutcome, PlanId, PlanState, ReplanId, Store, TaskId, TaskStatus, end_generation, event,
    generation_info, json_column, now, plan_state, reconcile_entry,
};
use crate::planner::{Command, Rejection};

/// Identifies a plan's replanning state: its state and tasks, their
/// definitions and dependencies, everything recorded about their
/// generations, claims, authorizations, cancellations and revisions, and
/// its concerns and their human decisions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Basis(String);

impl Basis {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// What applying a replan established. Only `Applied` wrote anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use]
pub enum Replan {
    Applied(ReplanId),
    /// The plan changed since the proposal's basis: it was made against
    /// state that no longer holds.
    Stale,
}

/// One definition a task had, as recorded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Revision {
    pub number: i64,
    pub objective: String,
    pub context: String,
    pub scope: Vec<String>,
    pub depends_on: Vec<TaskId>,
    /// The replan that made it current; `None` when planned before any
    /// replan revised the task.
    pub replan: Option<ReplanId>,
    pub recorded_at: i64,
}

/// A planner's authorization of one more attempt at a task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetryAuthorization {
    /// The stopped generation it follows.
    pub after: GenerationId,
    /// The one revision of the task it authorizes: usable only while that
    /// is the task's current definition.
    pub revision: i64,
    pub replan: ReplanId,
    /// The fresh generation whose claim used it, once one did.
    pub used_by: Option<GenerationId>,
    pub at: i64,
}

/// A path an abandoned generation's installed candidate changed in the
/// working tree: what the candidate installed there, and what restoring it
/// writes back, the path's last accepted state. That is its accepted source
/// or, for a path that has none, what the install replaced. Both are
/// recovery objects, or absence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Restoration {
    pub path: String,
    pub candidate: Content,
    pub accepted: Content,
}

/// A replan, as recorded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplanRecord {
    pub id: ReplanId,
    pub plan: PlanId,
    /// The planner action that proposed it, when an invocation did.
    pub journal: Option<JournalId>,
    pub basis: String,
    pub commands: i64,
    pub applied_at: i64,
}

/// Where a task stands for replanning, which decides what a replan may do
/// with it: revise or cancel it when unstarted or stopped, and authorize a
/// retry only when stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Standing {
    /// No generation served it, or its retry is authorized and unused.
    Unstarted {
        authorized: bool,
    },
    /// Its latest generation's pipeline conclusively stopped short.
    Stopped {
        generation: GenerationId,
        outcome: PipelineOutcome,
    },
    Completed,
    Cancelled,
    /// Scheduled, with work live or unresolved, or never scheduled: what
    /// its generation did is not established for replanning.
    Unresolved(TaskStatus),
}

impl Store {
    /// Where `task` stands for replanning.
    pub fn standing(&self, task: TaskId) -> Result<Standing> {
        let tx = self.conn.unchecked_transaction()?;
        standing(&tx, task)
    }

    /// The current basis of `plan`'s replanning state; see [`Basis`].
    pub fn replan_basis(&self, plan: PlanId) -> Result<Basis> {
        let tx = self.conn.unchecked_transaction()?;
        plan_state(&tx, plan)?;
        basis(&tx, plan)
    }

    /// Applies a replan a planner proposed against `basis` for a ready,
    /// running or paused plan, or one that needs attention while no concern
    /// blocks it, as one transaction: see the module documentation and
    /// `attention`'s. A replan raising a concern makes the plan need
    /// attention; one of a plan that needs attention, raising none, runs it
    /// again. `scope` checks a requested path against the project.
    /// `restore` restores the candidates of the generations it abandons in
    /// the working tree, within the transaction, returning the paths it
    /// found holding anything else, having written nothing. `action` is the
    /// planner's journal entry and invocation that proposed it, reconciled
    /// in the same transaction when it applies. A refused replan changes
    /// nothing, and its error carries the [`Rejection`].
    pub(crate) fn replan(
        &mut self,
        plan: PlanId,
        basis: &Basis,
        action: Option<(JournalId, InvocationId)>,
        commands: &[Command],
        scope: &dyn Fn(&str) -> Result<()>,
        restore: &dyn Fn(&[Restoration]) -> Result<Vec<String>>,
    ) -> Result<Replan> {
        self.write(|tx| {
            let state = plan_state(tx, plan)?;
            let why = match state {
                PlanState::Ready | PlanState::Running | PlanState::Paused => None,
                PlanState::NeedsAttention if blocked(tx, plan)? => Some(anyhow!(
                    "plan {plan} awaits a human decision, so it is not replanned"
                )),
                PlanState::NeedsAttention => None,
                _ => Some(anyhow!(
                    "plan {plan} is {state}; only a finalized plan is replanned"
                )),
            };
            if let Some(why) = why {
                return Err(Rejection::response(why).into());
            }
            if self::basis(tx, plan)? != *basis {
                return Ok(Replan::Stale);
            }
            if commands.is_empty() && state != PlanState::NeedsAttention {
                let why = anyhow!("only acting on a human's decisions proposes no command");
                return Err(Rejection::response(why).into());
            }
            tx.execute(
                "INSERT INTO replans (plan_id, journal_id, basis, commands, applied_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    plan,
                    action.map(|(entry, _)| entry),
                    basis.0,
                    commands.len() as i64,
                    now()
                ],
            )?;
            let replan = ReplanId(tx.last_insert_rowid());
            let mut applied = Applied::default();
            for (i, command) in commands.iter().enumerate() {
                apply(tx, plan, replan, command, scope, &mut applied)
                    .map_err(|why| Rejection::command(i + 1, command.op(), why))?;
            }
            check_result(tx, plan).map_err(Rejection::response)?;
            for &task in &applied.revised {
                record_revision(tx, task, Some(replan))?;
            }
            let detail = format!("replan {replan}: {} commands applied", commands.len());
            event(tx, "plan.replanned", Some(plan), None, None, &detail)?;
            // Having acted on its human's decisions, a plan runs again
            // unless it raised another concern, which stopped it already.
            resume(tx, plan)?;
            if let Some((entry, invocation)) = action {
                let evidence = [
                    Evidence::Invocation { invocation },
                    Evidence::Fact {
                        name: "replan.applied".into(),
                    },
                ];
                reconcile_entry(tx, entry, ActionOutcome::CompletedAsIntended, &evidence)?;
            }
            // Last, once everything else applied: only committing remains.
            let mut restorations = Vec::new();
            for &generation in &applied.abandoned {
                restorations.extend(installed(tx, generation)?);
            }
            if !restorations.is_empty() {
                let drifted = restore(&restorations)?;
                if !drifted.is_empty() {
                    let why = anyhow!(
                        "the working tree holds neither an abandoned candidate nor its accepted \
                         state at {drifted:?}, so nothing is restored or replanned"
                    );
                    return Err(Rejection::response(why).into());
                }
            }
            Ok(Replan::Applied(replan))
        })
    }

    /// Every revision recorded of `task`, in order.
    pub fn revisions(&self, task: TaskId) -> Result<Vec<Revision>> {
        self.conn
            .prepare(
                "SELECT number, objective, context, scope, depends_on, replan_id, recorded_at
                 FROM task_revisions WHERE task_id = ?1 ORDER BY number",
            )?
            .query_map([task], |r| {
                Ok(Revision {
                    number: r.get(0)?,
                    objective: r.get(1)?,
                    context: r.get(2)?,
                    scope: json_column(r, 3)?,
                    depends_on: json_column(r, 4)?,
                    replan: r.get(5)?,
                    recorded_at: r.get(6)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()
            .map_err(Into::into)
    }

    /// The revision `generation` was started to execute, when known.
    pub fn generation_revision(&self, generation: GenerationId) -> Result<Option<i64>> {
        self.conn
            .query_row(
                "SELECT revision FROM generation_revisions WHERE generation_id = ?1",
                [generation],
                |r| r.get(0),
            )
            .optional()
            .map_err(Into::into)
    }

    /// Every retry authorized for `task`, in order.
    pub fn retry_authorizations(&self, task: TaskId) -> Result<Vec<RetryAuthorization>> {
        self.conn
            .prepare(
                "SELECT after_generation, revision, replan_id, generation_id, authorized_at
                 FROM retry_authorizations WHERE task_id = ?1 ORDER BY id",
            )?
            .query_map([task], |r| {
                Ok(RetryAuthorization {
                    after: r.get(0)?,
                    revision: r.get(1)?,
                    replan: r.get(2)?,
                    used_by: r.get(3)?,
                    at: r.get(4)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()
            .map_err(Into::into)
    }

    /// The replan that cancelled `task`, if one did.
    pub fn cancellation(&self, task: TaskId) -> Result<Option<ReplanId>> {
        self.conn
            .query_row(
                "SELECT replan_id FROM task_cancellations WHERE task_id = ?1",
                [task],
                |r| r.get(0),
            )
            .optional()
            .map_err(Into::into)
    }

    /// Every replan of `plan`, in order.
    pub fn replans(&self, plan: PlanId) -> Result<Vec<ReplanRecord>> {
        self.conn
            .prepare(
                "SELECT id, journal_id, basis, commands, applied_at FROM replans
                 WHERE plan_id = ?1 ORDER BY id",
            )?
            .query_map([plan], |r| {
                Ok(ReplanRecord {
                    id: r.get(0)?,
                    plan,
                    journal: r.get(1)?,
                    basis: r.get(2)?,
                    commands: r.get(3)?,
                    applied_at: r.get(4)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()
            .map_err(Into::into)
    }
}

/// What a replan's commands did so far.
#[derive(Default)]
struct Applied {
    /// Each task whose definition they may have changed.
    revised: BTreeSet<TaskId>,
    /// Each task they authorized to run again.
    retried: BTreeSet<TaskId>,
    /// Each generation they abandoned, whose candidate is to be restored.
    abandoned: Vec<GenerationId>,
}

/// Applies one replanning command, recording what it did in `applied`.
fn apply(
    tx: &Transaction,
    plan: PlanId,
    replan: ReplanId,
    command: &Command,
    scope: &dyn Fn(&str) -> Result<()>,
    applied: &mut Applied,
) -> Result<()> {
    match command {
        Command::AddTask { task, .. } => {
            planning::apply(tx, plan, command, scope)?;
            applied.revised.insert(lookup(tx, plan, task)?);
        }
        Command::UpdateTask { task, .. } | Command::SetDependencies { task, .. } => {
            let id = lookup(tx, plan, task)?;
            match standing(tx, id)? {
                Standing::Unstarted { .. } | Standing::Stopped { .. } => {}
                other => refuse(task, other)?,
            }
            ensure!(
                !applied.retried.contains(&id),
                "task `{task}` was retried earlier in this replan, which authorized its \
                 definition then: a task is revised before it is retried"
            );
            // What it was before this replan first changes it.
            if applied.revised.insert(id) {
                record_revision(tx, id, None)?;
            }
            planning::apply(tx, plan, command, scope)?;
        }
        Command::RetryTask { task } => {
            let id = lookup(tx, plan, task)?;
            match standing(tx, id)? {
                Standing::Stopped {
                    generation,
                    outcome,
                } => {
                    abandon(tx, replan, id, generation, outcome, true, applied)?;
                    let revised = applied.revised.contains(&id).then_some(replan);
                    let revision = record_revision(tx, id, revised)?;
                    tx.execute(
                        "INSERT INTO retry_authorizations
                           (after_generation, task_id, revision, replan_id, authorized_at)
                         VALUES (?1, ?2, ?3, ?4, ?5)",
                        params![generation, id, revision, replan, now()],
                    )?;
                    applied.retried.insert(id);
                    let (_, _, number, _) = generation_info(tx, generation)?;
                    let detail =
                        format!("replan {replan}: revision {revision}, after generation {number}");
                    event(
                        tx,
                        "task.retry_authorized",
                        Some(plan),
                        Some(id),
                        None,
                        &detail,
                    )?;
                }
                Standing::Unstarted { authorized: true } => {
                    bail!("task `{task}` is already authorized to run again")
                }
                Standing::Unstarted { authorized: false } => {
                    bail!("task `{task}` never ran: it runs once eligible, without a retry")
                }
                other => refuse(task, other)?,
            }
        }
        Command::CancelTask { task } => {
            let id = lookup(tx, plan, task)?;
            match standing(tx, id)? {
                Standing::Unstarted { .. } => {}
                Standing::Stopped {
                    generation,
                    outcome,
                } => abandon(tx, replan, id, generation, outcome, false, applied)?,
                other => refuse(task, other)?,
            }
            tx.execute(
                "INSERT INTO task_cancellations (task_id, replan_id, cancelled_at)
                 VALUES (?1, ?2, ?3)",
                params![id, replan, now()],
            )?;
            let detail = format!("replan {replan}");
            event(tx, "task.cancelled", Some(plan), Some(id), None, &detail)?;
        }
        Command::RaiseAttention {
            concern,
            reason,
            evidence,
            tasks,
        } => {
            raise(tx, plan, replan, concern, reason, evidence, tasks)?;
        }
        Command::RemoveTask { .. } => {
            bail!("a finalized plan's tasks are never removed: cancel_task supersedes one")
        }
        Command::Finalize {} => bail!("the plan was finalized already"),
    }
    Ok(())
}

/// Refuses to replan `task`, which stands as `standing`.
fn refuse(task: &str, standing: Standing) -> Result<()> {
    match standing {
        Standing::Completed => bail!(
            "task `{task}` completed: its accepted work stands, and follow-up work is a new task"
        ),
        Standing::Cancelled => bail!("task `{task}` was cancelled"),
        Standing::Unresolved(status) => bail!(
            "how task `{task}`'s work ended is not established ({status:?}), so it is not \
             replanned"
        ),
        Standing::Unstarted { .. } | Standing::Stopped { .. } => Ok(()),
    }
}

/// Where `task` stands for replanning, from canonical state alone.
fn standing(conn: &Connection, task: TaskId) -> Result<Standing> {
    Ok(match task_status(conn, task)? {
        TaskStatus::Completed => Standing::Completed,
        TaskStatus::Cancelled => Standing::Cancelled,
        TaskStatus::Eligible
        | TaskStatus::WaitingForDependencies(_)
        | TaskStatus::WaitingForOwnership(_) => Standing::Unstarted {
            authorized: retry_pending(conn, task)?.is_some(),
        },
        // Released once its outcome was established; anything begun on it
        // since must be settled again.
        TaskStatus::Stopped {
            generation,
            outcome,
        } => {
            let settled: Option<PipelineOutcome> = conn
                .query_row(
                    "SELECT outcome FROM scheduler_outcomes WHERE generation_id = ?1",
                    [generation],
                    |r| r.get(0),
                )
                .optional()?
                .flatten();
            match settled {
                Some(_) => Standing::Stopped {
                    generation,
                    outcome,
                },
                None => Standing::Unresolved(TaskStatus::Stopped {
                    generation,
                    outcome,
                }),
            }
        }
        status @ (TaskStatus::Scheduled(_) | TaskStatus::Unscheduled { .. }) => {
            Standing::Unresolved(status)
        }
    })
}

/// Abandons `generation`, the latest of stopped `task`, for `replan`, which
/// `retried` the task or else cancels it, unless an earlier replan
/// abandoned it already: records the abandonment, releases what the
/// generation owns, and ends it as its outcome says, if it has not ended.
/// Its installed candidate is restored once the whole replan is known to
/// apply.
fn abandon(
    tx: &Transaction,
    replan: ReplanId,
    task: TaskId,
    generation: GenerationId,
    outcome: PipelineOutcome,
    retried: bool,
    applied: &mut Applied,
) -> Result<()> {
    let earlier: bool = tx.query_row(
        "SELECT EXISTS (SELECT 1 FROM generation_abandonments WHERE generation_id = ?1)",
        [generation],
        |r| r.get(0),
    )?;
    if earlier {
        return Ok(());
    }
    let (_, _, _, state) = generation_info(tx, generation)?;
    let to = match (state, outcome) {
        (GenerationState::Active, PipelineOutcome::VerificationFailed) => GenerationState::Rejected,
        (GenerationState::Active, _) => GenerationState::Failed,
        (ended, _) => ended,
    };
    tx.execute(
        "INSERT INTO generation_abandonments
           (generation_id, state, replan_id, retried, cancelled, abandoned_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            generation,
            to,
            replan,
            retried.then_some(generation),
            (!retried).then_some(task),
            now()
        ],
    )?;
    release(tx, generation)?;
    if state == GenerationState::Active {
        end_generation(tx, generation, to, &[])?;
    }
    applied.abandoned.push(generation);
    Ok(())
}

/// Each path the installed candidate of `generation`, if any, changed,
/// with what restoring it writes back; see [`Restoration`].
fn installed(conn: &Connection, generation: GenerationId) -> Result<Vec<Restoration>> {
    conn.prepare(
        "SELECT x.path, x.after_kind, x.after_hash,
             CASE WHEN s.path IS NULL THEN x.before_kind
                 WHEN s.hash IS NULL THEN 'absent' ELSE 'file' END,
             CASE WHEN s.path IS NULL THEN x.before_hash ELSE s.hash END
         FROM executions e
         JOIN execution_install_results i ON i.execution_id = e.id AND i.outcome = 'installed'
         JOIN execution_changes x ON x.execution_id = e.id
         LEFT JOIN accepted_sources s ON s.path = x.path
         WHERE e.generation_id = ?1 ORDER BY x.path",
    )?
    .query_map([generation], |r| {
        Ok(Restoration {
            path: r.get(0)?,
            candidate: Content::read(r, 1)?,
            accepted: Content::read(r, 3)?,
        })
    })?
    .collect::<rusqlite::Result<_>>()
    .map_err(Into::into)
}

/// Checks the plan a replan leaves: nothing depends on a cancelled task,
/// and its DAG has no defect.
fn check_result(tx: &Transaction, plan: PlanId) -> Result<()> {
    let dangling: Option<(String, String)> = tx
        .query_row(
            "SELECT t.key, u.key FROM task_dependencies d
             JOIN tasks t ON t.id = d.task_id JOIN tasks u ON u.id = d.depends_on
             JOIN task_cancellations c ON c.task_id = d.depends_on
             WHERE t.plan_id = ?1
                 AND NOT EXISTS (SELECT 1 FROM task_cancellations x WHERE x.task_id = t.id)
             ORDER BY t.id, u.id LIMIT 1",
            [plan],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    if let Some((task, cancelled)) = dangling {
        bail!("task `{task}` depends on `{cancelled}`, which is cancelled");
    }
    let defects = dag_defects(tx, plan)?;
    ensure!(defects.is_empty(), "the plan's DAG is invalid: {defects:?}");
    Ok(())
}

/// The usable retry authorization of `task`, if there is one: unused,
/// following its latest generation, and of the revision that is its
/// current definition, which no later revision superseded.
pub(super) fn retry_pending(conn: &Connection, task: TaskId) -> Result<Option<i64>> {
    conn.query_row(
        "SELECT a.id FROM retry_authorizations a
         JOIN task_revisions v ON v.task_id = a.task_id AND v.number = a.revision
         JOIN task_definitions d ON d.task_id = a.task_id
         WHERE a.task_id = ?1 AND a.generation_id IS NULL
             AND a.after_generation =
                 (SELECT id FROM generations WHERE task_id = ?1 ORDER BY number DESC LIMIT 1)
             AND a.revision = (SELECT max(number) FROM task_revisions WHERE task_id = ?1)
             AND d.objective = v.objective AND d.context = v.context
             AND d.scope = v.scope AND d.depends_on = v.depends_on",
        [task],
        |r| r.get(0),
    )
    .optional()
    .map_err(Into::into)
}

pub(super) fn cancelled(conn: &Connection, task: TaskId) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM task_cancellations WHERE task_id = ?1)",
        [task],
        |r| r.get(0),
    )?)
}

/// Records `task`'s current definition as its next revision, made current
/// by `replan`, unless its latest revision records it already. Returns the
/// number of the revision recording it.
pub(super) fn record_revision(
    tx: &Transaction,
    task: TaskId,
    replan: Option<ReplanId>,
) -> Result<i64> {
    let latest: Option<(i64, bool)> = tx
        .query_row(
            "SELECT v.number, d.objective = v.objective AND d.context = v.context
                 AND d.scope = v.scope AND d.depends_on = v.depends_on
             FROM task_revisions v JOIN task_definitions d ON d.task_id = v.task_id
             WHERE v.task_id = ?1 ORDER BY v.number DESC LIMIT 1",
            [task],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let number = match latest {
        Some((number, true)) => return Ok(number),
        Some((number, false)) => number + 1,
        None => 1,
    };
    tx.execute(
        "INSERT INTO task_revisions
           (task_id, number, objective, context, scope, depends_on, replan_id, recorded_at)
         SELECT task_id, ?2, objective, context, scope, depends_on, ?3, ?4
         FROM task_definitions WHERE task_id = ?1",
        params![task, number, replan, now()],
    )?;
    if let Some(replan) = replan {
        let plan: PlanId =
            tx.query_row("SELECT plan_id FROM replans WHERE id = ?1", [replan], |r| {
                r.get(0)
            })?;
        let detail = format!("revision {number} by replan {replan}");
        event(tx, "task.revised", Some(plan), Some(task), None, &detail)?;
    }
    Ok(number)
}

/// Everything recorded about `plan` that replanning depends on, one query
/// after another in order, each row and value delimited.
const BASIS_QUERIES: [&str; 11] = [
    "SELECT state FROM plans WHERE id = ?1",
    "SELECT id, key, objective, context FROM tasks WHERE plan_id = ?1 ORDER BY id",
    "SELECT s.task_id, s.path FROM task_scope s JOIN tasks t ON t.id = s.task_id
     WHERE t.plan_id = ?1 ORDER BY 1, 2",
    "SELECT d.plan_id, d.task_id, d.depends_on FROM task_dependencies d
     LEFT JOIN tasks t ON t.id = d.task_id WHERE d.plan_id = ?1 OR t.plan_id = ?1
     ORDER BY 2, 3",
    "SELECT g.id, g.task_id, g.number, g.state, c.capacity, r.outcome, o.outcome,
         (SELECT count(*) FROM ownership w WHERE w.generation_id = g.id),
         (SELECT count(*) FROM acceptance_phases p WHERE p.generation_id = g.id),
         (SELECT count(*) FROM agents a WHERE a.generation_id = g.id),
         (SELECT count(*) FROM agents a JOIN journal j ON j.agent_id = a.id
             WHERE a.generation_id = g.id),
         (SELECT count(*) FROM agents a JOIN journal j ON j.agent_id = a.id
             WHERE a.generation_id = g.id AND j.state = 'reconciled'),
         (SELECT count(*) FROM agents a JOIN invocations i ON i.agent_id = a.id
             WHERE a.generation_id = g.id AND i.ended_at IS NULL)
     FROM generations g JOIN tasks t ON t.id = g.task_id
     LEFT JOIN scheduler_claims c ON c.generation_id = g.id
     LEFT JOIN scheduler_releases r ON r.generation_id = g.id
     LEFT JOIN scheduler_outcomes o ON o.generation_id = g.id
     WHERE t.plan_id = ?1 ORDER BY g.id",
    "SELECT a.id, a.after_generation, a.task_id, a.revision, a.generation_id
     FROM retry_authorizations a JOIN tasks t ON t.id = a.task_id
     WHERE t.plan_id = ?1 ORDER BY 1",
    "SELECT b.generation_id, b.state, b.replan_id FROM generation_abandonments b
     JOIN generations g ON g.id = b.generation_id JOIN tasks t ON t.id = g.task_id
     WHERE t.plan_id = ?1 ORDER BY 1",
    "SELECT c.task_id FROM task_cancellations c JOIN tasks t ON t.id = c.task_id
     WHERE t.plan_id = ?1 ORDER BY 1",
    "SELECT v.task_id, max(v.number) FROM task_revisions v JOIN tasks t ON t.id = v.task_id
     WHERE t.plan_id = ?1 GROUP BY v.task_id ORDER BY 1",
    "SELECT max(id) FROM replans WHERE plan_id = ?1",
    "SELECT c.id, c.key, d.kind, d.instruction FROM attention_concerns c
     LEFT JOIN attention_decisions d ON d.concern_id = c.id WHERE c.plan_id = ?1 ORDER BY 1",
];

fn basis(conn: &Connection, plan: PlanId) -> Result<Basis> {
    let mut hash = Sha256::new();
    for sql in BASIS_QUERIES {
        let mut statement = conn.prepare(sql)?;
        let columns = statement.column_count();
        let mut rows = statement.query([plan])?;
        while let Some(row) = rows.next()? {
            for i in 0..columns {
                let value: Value = row.get(i)?;
                hash.update(format!("{value:?}\u{1f}"));
            }
            hash.update("\u{1e}");
        }
        hash.update("\u{1d}");
    }
    Ok(Basis(
        hash.finalize().iter().map(|b| format!("{b:02x}")).collect(),
    ))
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU32;
    use std::path::PathBuf;
    use std::sync::Barrier;
    use std::thread;

    use super::*;
    use crate::state::tests::{err, objective, ready_plan, store};
    use crate::state::{AgentScope, Claim, GenerationEnd, Intent, Release, Role, Task};

    const LIMIT: NonZeroU32 = NonZeroU32::new(8).unwrap();

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    fn update(task: &str, objective: &str) -> Command {
        Command::UpdateTask {
            task: task.into(),
            objective: Some(objective.into()),
            context: None,
            paths: None,
        }
    }

    fn add(task: &str, paths: &[&str], depends_on: &[&str]) -> Command {
        Command::AddTask {
            task: task.into(),
            objective: format!("Do {task}"),
            context: String::new(),
            paths: strings(paths),
            depends_on: strings(depends_on),
        }
    }

    fn on(task: &str, depends_on: &[&str]) -> Command {
        Command::SetDependencies {
            task: task.into(),
            depends_on: strings(depends_on),
        }
    }

    fn retry(task: &str) -> Command {
        Command::RetryTask { task: task.into() }
    }

    fn cancel(task: &str) -> Command {
        Command::CancelTask { task: task.into() }
    }

    fn path(dir: &tempfile::TempDir) -> PathBuf {
        dir.path().join("state.db")
    }

    /// A running plan of tasks `(key, scope, depends_on)`, and their ids.
    fn running(store: &mut Store, tasks: &[(&str, &[&str], &[&str])]) -> (PlanId, Vec<TaskId>) {
        let (plan, ids) = ready_plan(store, tasks);
        store.start_plan(plan).unwrap();
        (plan, ids)
    }

    fn claimed(store: &mut Store, task: TaskId) -> GenerationId {
        match store.claim(task, LIMIT).unwrap() {
            Claim::Claimed(generation) => generation,
            other => panic!("not claimed: {other:?}"),
        }
    }

    /// Claims `task` and releases the claim with nothing run: a pipeline
    /// that conclusively stopped short.
    fn stopped(store: &mut Store, task: TaskId) -> GenerationId {
        let generation = claimed(store, task);
        let released = store.release_claim(generation).unwrap();
        assert_eq!(released, Release::Released(PipelineOutcome::NotExecuted));
        generation
    }

    /// Applies `commands` against the plan's current basis.
    fn replan(store: &mut Store, plan: PlanId, commands: &[Command]) -> Result<Replan> {
        let basis = store.replan_basis(plan).unwrap();
        store.replan(plan, &basis, None, commands, &|_| Ok(()), &|_| {
            Ok(Vec::new())
        })
    }

    fn applied(store: &mut Store, plan: PlanId, commands: &[Command]) -> ReplanId {
        match replan(store, plan, commands).unwrap() {
            Replan::Applied(replan) => replan,
            Replan::Stale => panic!("stale"),
        }
    }

    /// Every row of every table.
    fn everything(store: &Store) -> Vec<(String, Vec<Vec<Value>>)> {
        let conn = store.raw();
        let tables: Vec<String> = conn
            .prepare("SELECT name FROM sqlite_schema WHERE type = 'table' ORDER BY name")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        tables
            .into_iter()
            .map(|table| {
                let mut stmt = conn.prepare(&format!("SELECT * FROM {table}")).unwrap();
                let columns = stmt.column_count();
                let rows = stmt
                    .query_map([], |r| (0..columns).map(|i| r.get(i)).collect())
                    .unwrap()
                    .collect::<rusqlite::Result<_>>()
                    .unwrap();
                (table, rows)
            })
            .collect()
    }

    /// Applies `commands`, which must be refused as `expected`, changing
    /// nothing at all.
    fn refused(store: &mut Store, plan: PlanId, commands: &[Command], expected: &str) {
        let before = everything(store);
        let message = err(replan(store, plan, commands));
        assert!(message.contains(expected), "{message}");
        assert_eq!(everything(store), before, "{message}");
    }

    fn forged(store: &Store, sql: &str, expected: &str) {
        let message = store.raw().execute_batch(sql).unwrap_err().to_string();
        assert!(message.contains(expected), "{sql}: {message}");
    }

    #[test]
    fn a_retry_is_explicit_used_once_and_starts_one_fresh_generation() {
        let (_dir, mut store) = store();
        let (plan, ids) = running(&mut store, &[("a", &["src/a.rs"], &[])]);
        let a = ids[0];
        let first = stopped(&mut store, a);
        // The claim recorded what the generation was started to execute.
        assert_eq!(store.generation_revision(first).unwrap(), Some(1));
        let planned = store.revisions(a).unwrap();
        assert_eq!(planned.len(), 1);
        assert_eq!(
            (
                planned[0].objective.as_str(),
                &planned[0].scope,
                planned[0].replan
            ),
            ("a", &strings(&["src/a.rs"]), None)
        );
        // A stopped pipeline alone never runs the task again.
        let stop = TaskStatus::Stopped {
            generation: first,
            outcome: PipelineOutcome::NotExecuted,
        };
        assert_eq!(
            store.claim(a, LIMIT).unwrap(),
            Claim::Ineligible(stop.clone())
        );
        assert_eq!(
            store.standing(a).unwrap(),
            Standing::Stopped {
                generation: first,
                outcome: PipelineOutcome::NotExecuted
            }
        );
        assert_eq!(store.owned_paths(first).unwrap(), ["src/a.rs"]);

        // Revising it runs nothing either.
        applied(&mut store, plan, &[update("a", "Do a better")]);
        assert_eq!(store.claim(a, LIMIT).unwrap(), Claim::Ineligible(stop));
        let generations = store.generations(a).unwrap();
        assert_eq!(generations.len(), 1);

        // Only an explicit retry does, which ends the stopped generation and
        // releases its ownership, starting nothing itself.
        let replan = applied(&mut store, plan, &[retry("a")]);
        assert_eq!(
            store.generations(a).unwrap()[0].state,
            GenerationState::Failed
        );
        assert!(store.owned_paths(first).unwrap().is_empty());
        assert_eq!(store.generations(a).unwrap().len(), 1);
        assert_eq!(
            store.standing(a).unwrap(),
            Standing::Unstarted { authorized: true }
        );
        refused(&mut store, plan, &[retry("a")], "already authorized");
        let second = claimed(&mut store, a);
        assert_eq!(store.generations(a).unwrap()[1].number, 2);
        assert_eq!(
            store.retry_authorizations(a).unwrap(),
            [RetryAuthorization {
                after: first,
                replan,
                revision: 2,
                used_by: Some(second),
                at: store.retry_authorizations(a).unwrap()[0].at,
            }]
        );
        // Each generation stays bound to the definition it was started with.
        let revisions = store.revisions(a).unwrap();
        assert_eq!(revisions.len(), 2);
        assert_eq!(revisions[0], planned[0]);
        assert_eq!(revisions[1].objective, "Do a better");
        assert_eq!(store.generation_revision(first).unwrap(), Some(1));
        assert_eq!(store.generation_revision(second).unwrap(), Some(2));
        assert_eq!(store.task(a).unwrap().objective, "Do a better");

        // Used once, and never again.
        assert_eq!(
            store.claim(a, LIMIT).unwrap(),
            Claim::Ineligible(TaskStatus::Scheduled(second))
        );
        forged(
            &store,
            &format!(
                "UPDATE retry_authorizations SET generation_id = NULL WHERE after_generation = {first}"
            ),
            "once",
        );
        forged(
            &store,
            &format!("DELETE FROM retry_authorizations WHERE after_generation = {first}"),
            "never withdrawn",
        );
        assert_eq!(
            store.release_claim(second).unwrap(),
            Release::Released(PipelineOutcome::NotExecuted)
        );
        assert!(matches!(
            store.claim(a, LIMIT).unwrap(),
            Claim::Ineligible(TaskStatus::Stopped { generation, .. }) if generation == second
        ));
        assert_eq!(store.generations(a).unwrap().len(), 2);
    }

    #[test]
    fn nothing_but_its_abandonment_ends_a_scheduled_generation_short_of_acceptance() {
        let (_dir, mut store) = store();
        let tasks: [(&str, &[&str], &[&str]); 2] =
            [("a", &["src/a.rs"], &[]), ("b", &["src/b.rs"], &[])];
        let (plan, ids) = running(&mut store, &tasks);
        let [a, b] = ids[..] else { panic!() };
        let first = stopped(&mut store, a);
        let other = stopped(&mut store, b);
        // Not through `Store`.
        let message = err(store.finish_generation(first, GenerationEnd::Failed));
        assert!(message.contains("was scheduled"), "{message}");
        let message = err(store.release_ownership(first));
        assert!(message.contains("still active"), "{message}");
        // Nor beneath it, in either order.
        let fail =
            format!("UPDATE generations SET state = 'failed', ended_at = 1 WHERE id = {first}");
        let reject =
            format!("UPDATE generations SET state = 'rejected', ended_at = 1 WHERE id = {first}");
        let free = format!("DELETE FROM ownership WHERE generation_id = {first}");
        for sql in [&fail, &reject] {
            forged(&store, sql, "only as a replan abandons it");
        }
        forged(&store, &free, "released only by accepting or abandoning it");
        forged(&store, &format!("{free}; {fail}"), "released only by");
        forged(
            &store,
            &format!("{fail}; {free}"),
            "only as a replan abandons it",
        );
        let unchanged = |store: &Store| {
            (
                store.generations(a).unwrap(),
                store.owned_paths(first).unwrap(),
            )
        };
        let before = unchanged(&store);
        assert_eq!(before.0[0].state, GenerationState::Active);

        // Abandonment is granted only for the exact generation, by the
        // latest replan of its plan, deciding its task's fate.
        let replan = applied(&mut store, plan, &[update("b", "Revised b")]);
        let (other_plan, _) = running(&mut store, &[("elsewhere", &[], &[])]);
        let elsewhere = applied(&mut store, other_plan, &[update("elsewhere", "x")]);
        let grant = |generation: GenerationId, state: &str, replan: ReplanId, retried: String| {
            format!(
                "INSERT INTO generation_abandonments VALUES
                   ({generation}, '{state}', {replan}, {retried}, NULL, 0)"
            )
        };
        let refused = "only a replan abandons a conclusively stopped scheduled generation";
        for sql in [
            grant(first, "failed", replan, other.to_string()),
            grant(first, "failed", elsewhere, first.to_string()),
            grant(first, "rejected", replan, first.to_string()),
            format!(
                "INSERT INTO generation_abandonments VALUES ({first}, 'failed', {replan}, NULL, {b}, 0)"
            ),
        ] {
            forged(&store, &sql, refused);
        }
        // Once another replan applied, the earlier one grants nothing.
        applied(&mut store, plan, &[update("b", "Revised b again")]);
        forged(
            &store,
            &grant(first, "failed", replan, first.to_string()),
            refused,
        );
        assert_eq!(unchanged(&store), before);

        // Half of the transition never commits: the grant is refused at
        // commit unless its generation ended as it says, owning nothing,
        // and its replan authorized a retry after it.
        let replan: ReplanId = store.replans(plan).unwrap().last().unwrap().id;
        let grant = grant(first, "failed", replan, first.to_string());
        for half in [
            grant.clone(),
            format!("{grant}; {free}"),
            format!("{grant}; {free}; {fail}"),
        ] {
            let message = store
                .raw()
                .execute_batch(&format!("BEGIN; {half}; COMMIT"))
                .unwrap_err()
                .to_string();
            store.raw().execute_batch("ROLLBACK").unwrap();
            assert!(message.contains("FOREIGN KEY"), "{half}: {message}");
            assert_eq!(unchanged(&store), before, "{half}");
        }
        forged(
            &store,
            &format!("BEGIN; {grant}; {fail}"),
            "only as a replan abandons it",
        );
        store.raw().execute_batch("ROLLBACK").unwrap();
        assert_eq!(unchanged(&store), before);

        // The whole transition, as a replan applies it, retires the
        // generation and releases its ownership together.
        let replan = applied(&mut store, plan, &[retry("a")]);
        assert_eq!(
            store.generations(a).unwrap()[0].state,
            GenerationState::Failed
        );
        assert!(store.owned_paths(first).unwrap().is_empty());
        let recorded: (String, ReplanId, Option<GenerationId>) = store
            .raw()
            .query_row(
                "SELECT state, replan_id, retried FROM generation_abandonments
                 WHERE generation_id = ?1",
                [first],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(recorded, ("failed".into(), replan, Some(first)));
        forged(
            &store,
            &format!("DELETE FROM generation_abandonments WHERE generation_id = {first}"),
            "immutable",
        );
        // A fresh generation owning the whole scope and bound to the
        // current definition is still claimed only through a retry.
        let fresh = claimed(&mut store, a);
        assert_eq!(store.generations(a).unwrap()[1].id, fresh);
    }

    #[test]
    fn neither_an_accepted_nor_an_unsettled_generation_is_abandoned() {
        let (_dir, mut store) = store();
        let tasks: [(&str, &[&str], &[&str]); 3] = [
            ("accepted", &["src/a.rs"], &[]),
            ("held", &["src/b.rs"], &[]),
            ("live", &["src/c.rs"], &[]),
        ];
        let (plan, ids) = running(&mut store, &tasks);
        let [accepted, held, live] = ids[..] else {
            panic!()
        };
        // Accepted beside Block 13, never having executed.
        let done = stopped(&mut store, accepted);
        store
            .accept_generation(done, &[("src/a.rs", None)])
            .unwrap();
        // A claim still held.
        let held = claimed(&mut store, held);
        // Released, and then an invocation of it began again.
        let live_generation = stopped(&mut store, live);
        let replan = applied(&mut store, plan, &[update("live", "unrelated")]);
        let agent = store
            .create_agent(Role::Verifier, AgentScope::Generation(live_generation))
            .unwrap();
        store.start_invocation(agent, "claude", "m", None).unwrap();
        for (generation, state) in [
            (done, "failed"),
            (done, "accepted"),
            (held, "failed"),
            (live_generation, "failed"),
        ] {
            let sql = format!(
                "INSERT INTO generation_abandonments VALUES
                   ({generation}, '{state}', {replan}, {generation}, NULL, 0)"
            );
            let message = store.raw().execute_batch(&sql).unwrap_err().to_string();
            assert!(
                message.contains("only a replan abandons")
                    || message.contains("CHECK constraint failed"),
                "{message}"
            );
        }
        forged(
            &store,
            &format!("UPDATE generations SET state = 'failed' WHERE id = {done}"),
            "only as a replan abandons it",
        );
    }

    #[test]
    fn a_retry_authorizes_one_exact_revision_and_revising_leaves_it_stale() {
        let (_dir, mut store) = store();
        let tasks: [(&str, &[&str], &[&str]); 3] = [
            ("a", &["src/a.rs"], &[]),
            ("b", &["src/b.rs"], &[]),
            ("x", &["src/x.rs"], &[]),
        ];
        let (plan, ids) = running(&mut store, &tasks);
        let [a, b, _] = ids[..] else { panic!() };
        let first = stopped(&mut store, a);
        let other = stopped(&mut store, b);
        let given = applied(&mut store, plan, &[retry("a"), retry("b")]);
        // Revising a task afterwards, by its objective or its
        // dependencies alike, leaves its authorization stale: attributable
        // to the revision it authorized, and never usable.
        applied(
            &mut store,
            plan,
            &[update("a", "Do a better"), on("b", &["x"])],
        );
        for (task, generation) in [(a, first), (b, other)] {
            let authorizations = store.retry_authorizations(task).unwrap();
            assert_eq!(
                authorizations,
                [RetryAuthorization {
                    after: generation,
                    revision: 1,
                    replan: given,
                    used_by: None,
                    at: authorizations[0].at,
                }]
            );
            assert_eq!(store.revisions(task).unwrap().len(), 2);
            let stop = TaskStatus::Stopped {
                generation,
                outcome: PipelineOutcome::NotExecuted,
            };
            assert_eq!(store.claim(task, LIMIT).unwrap(), Claim::Ineligible(stop));
            assert_eq!(
                store.standing(task).unwrap(),
                Standing::Stopped {
                    generation,
                    outcome: PipelineOutcome::NotExecuted
                }
            );
        }
        assert_eq!(store.revisions(b).unwrap()[1].depends_on, [ids[2]]);
        // Within one replan, a task is revised before it is retried.
        refused(
            &mut store,
            plan,
            &[retry("b"), on("b", &[])],
            "a task is revised before it is retried",
        );
        // The revision now current runs only once explicitly authorized,
        // and then exactly once.
        let again = applied(&mut store, plan, &[retry("a")]);
        let second = claimed(&mut store, a);
        assert_eq!(store.generation_revision(second).unwrap(), Some(2));
        assert_eq!(
            store.claim(a, LIMIT).unwrap(),
            Claim::Ineligible(TaskStatus::Scheduled(second))
        );
        let authorizations = store.retry_authorizations(a).unwrap();
        assert_eq!(
            authorizations
                .iter()
                .map(|r| (r.revision, r.replan, r.used_by))
                .collect::<Vec<_>>(),
            [(1, given, None), (2, again, Some(second))]
        );

        // Nor beneath `Store`: a fresh generation bound to the current
        // revision neither uses nor claims by the stale authorization, and
        // no authorization names a revision that is not current.
        let fresh = store.start_generation(b).unwrap();
        store
            .raw()
            .execute_batch(&format!(
                "INSERT INTO generation_revisions VALUES ({fresh}, {b}, 2);
                 INSERT INTO ownership VALUES ('src/b.rs', {fresh})"
            ))
            .unwrap();
        forged(
            &store,
            &format!("UPDATE retry_authorizations SET generation_id = {fresh} WHERE task_id = {b}"),
            "of the revision it authorized",
        );
        forged(
            &store,
            &format!("INSERT INTO scheduler_claims VALUES ({fresh}, {b}, 8, 0)"),
            "only an eligible task owning its whole scope is claimed",
        );
        forged(
            &store,
            &format!(
                "INSERT INTO retry_authorizations
                   (after_generation, task_id, revision, replan_id, authorized_at)
                 VALUES ({other}, {b}, 1, {again}, 0)"
            ),
            "only the current revision",
        );
        forged(
            &store,
            &format!("UPDATE retry_authorizations SET revision = 2 WHERE task_id = {b}"),
            "once",
        );
        assert_eq!(store.retry_authorizations(b).unwrap()[0].used_by, None);
    }

    #[test]
    fn a_claim_racing_a_revision_never_crosses_revisions() {
        for _ in 0..8 {
            let (dir, mut store) = store();
            let (plan, ids) = running(&mut store, &[("a", &["src/a.rs"], &[])]);
            stopped(&mut store, ids[0]);
            applied(&mut store, plan, &[retry("a")]);
            let barrier = Barrier::new(2);
            let (claim, revised) = thread::scope(|scope| {
                let claiming = scope.spawn(|| {
                    let mut store = Store::open(&path(&dir)).unwrap();
                    barrier.wait();
                    store.claim(ids[0], LIMIT).unwrap()
                });
                let revising = scope.spawn(|| {
                    let mut store = Store::open(&path(&dir)).unwrap();
                    barrier.wait();
                    replan(&mut store, plan, &[update("a", "Revised")])
                });
                (claiming.join().unwrap(), revising.join().unwrap())
            });
            let authorization = store.retry_authorizations(ids[0]).unwrap().remove(0);
            match (claim, revised) {
                // Claimed first: the revision is refused, or stale.
                (Claim::Claimed(generation), revised) => {
                    match revised {
                        Ok(stale) => assert_eq!(stale, Replan::Stale),
                        Err(e) => assert!(format!("{e:#}").contains("not established")),
                    }
                    assert_eq!(store.generation_revision(generation).unwrap(), Some(1));
                    assert_eq!(authorization.used_by, Some(generation));
                    assert_eq!(store.revisions(ids[0]).unwrap().len(), 1);
                }
                // Revised first: the authorization of revision 1 is stale.
                (Claim::Ineligible(TaskStatus::Stopped { .. }), Ok(Replan::Applied(_))) => {
                    assert_eq!(authorization.used_by, None);
                    assert_eq!(store.generations(ids[0]).unwrap().len(), 1);
                    assert_eq!(store.revisions(ids[0]).unwrap().len(), 2);
                }
                other => panic!("{other:?}"),
            }
        }
    }

    #[test]
    fn a_replan_failing_part_way_through_abandonment_changes_nothing() {
        let (_dir, mut store) = store();
        let tasks: [(&str, &[&str], &[&str]); 2] =
            [("a", &["src/a.rs"], &[]), ("b", &["src/b.rs"], &[])];
        let (plan, ids) = running(&mut store, &tasks);
        let generation = stopped(&mut store, ids[0]);
        stopped(&mut store, ids[1]);
        // Forced failures after the abandonment is recorded and ownership
        // released, and after the generation ended.
        for (table, command) in [
            ("retry_authorizations", retry("a")),
            ("task_cancellations", cancel("a")),
            ("events", retry("a")),
        ] {
            store
                .raw()
                .execute_batch(&format!(
                    "CREATE TEMP TRIGGER forced BEFORE INSERT ON main.{table}
                     WHEN EXISTS (SELECT 1 FROM generation_abandonments)
                     BEGIN SELECT RAISE(ABORT, 'forced'); END"
                ))
                .unwrap();
            refused(&mut store, plan, &[command], "forced");
            store
                .raw()
                .execute_batch("DROP TRIGGER temp.forced")
                .unwrap();
        }
        assert_eq!(store.owned_paths(generation).unwrap(), ["src/a.rs"]);
    }

    #[test]
    fn racing_schedulers_use_one_authorization_once() {
        for _ in 0..5 {
            let (dir, mut store) = store();
            let (plan, ids) = running(&mut store, &[("a", &["src/a.rs", "src/b.rs"], &[])]);
            stopped(&mut store, ids[0]);
            applied(&mut store, plan, &[retry("a")]);
            let barrier = Barrier::new(8);
            let claims: Vec<Claim> = thread::scope(|scope| {
                let handles: Vec<_> = (0..8)
                    .map(|_| {
                        scope.spawn(|| {
                            let mut store = Store::open(&path(&dir)).unwrap();
                            barrier.wait();
                            store.claim(ids[0], LIMIT).unwrap()
                        })
                    })
                    .collect();
                handles.into_iter().map(|h| h.join().unwrap()).collect()
            });
            let won: Vec<GenerationId> = claims
                .iter()
                .filter_map(|c| match c {
                    Claim::Claimed(g) => Some(*g),
                    _ => None,
                })
                .collect();
            assert_eq!(won.len(), 1, "{claims:?}");
            for claim in &claims {
                assert!(
                    matches!(claim, Claim::Claimed(_))
                        || *claim == Claim::Ineligible(TaskStatus::Scheduled(won[0])),
                    "{claim:?}"
                );
            }
            assert_eq!(store.generations(ids[0]).unwrap().len(), 2);
            assert_eq!(store.claims().unwrap().len(), 2);
        }
    }

    #[test]
    fn nothing_unresolved_or_unrun_is_retried() {
        let (_dir, mut store) = store();
        let tasks: [(&str, &[&str], &[&str]); 6] = [
            ("held", &["src/a.rs"], &[]),
            ("live", &["src/b.rs"], &[]),
            ("attempted", &["src/c.rs"], &[]),
            ("legacy", &["src/d.rs"], &[]),
            ("fresh", &["src/e.rs"], &[]),
            ("dependent", &["src/f.rs"], &["held"]),
        ];
        let (plan, ids) = running(&mut store, &tasks);
        let [held, live, attempted, legacy, _, _] = ids[..] else {
            panic!()
        };
        // A claim still held: running, or stopped with its outcome unknown.
        let held_generation = claimed(&mut store, held);
        // Released once how it ended was established; since then, an agent
        // of it has a live invocation again.
        let live_generation = stopped(&mut store, live);
        let agent = store
            .create_agent(Role::Verifier, AgentScope::Generation(live_generation))
            .unwrap();
        store.start_invocation(agent, "claude", "m", None).unwrap();
        // An action attempted, its outcome unknown, so never released.
        let attempted_generation = claimed(&mut store, attempted);
        let agent = store
            .create_agent(Role::Executor, AgentScope::Generation(attempted_generation))
            .unwrap();
        let intent = Intent {
            action: "executor.run".into(),
            parameters: serde_json::Map::new(),
        };
        let entry = store.intend(agent, &intent).unwrap();
        store.act(entry, None).unwrap();
        assert_eq!(
            store.release_claim(attempted_generation).unwrap(),
            Release::Retained
        );
        // Started beside scheduling.
        let legacy_generation = store.start_generation(legacy).unwrap();

        for key in ["held", "live", "attempted", "legacy"] {
            for command in [retry(key), update(key, "Other"), cancel(key), on(key, &[])] {
                refused(&mut store, plan, &[command], "is not established");
            }
        }
        refused(&mut store, plan, &[retry("fresh")], "never ran");
        refused(&mut store, plan, &[retry("dependent")], "never ran");
        for generation in [held_generation, live_generation, attempted_generation] {
            assert!(!store.owned_paths(generation).unwrap().is_empty());
        }
        assert_eq!(
            store.generations(legacy).unwrap()[0].state,
            GenerationState::Active
        );

        // Nor beneath `Store`: none of them is abandoned, and a retry needs
        // an abandoned, ended, conclusively stopped latest generation owning
        // nothing, of a task neither cancelled nor completed.
        let replan = applied(&mut store, plan, &[update("fresh", "Revised")]);
        for generation in [
            held_generation,
            live_generation,
            attempted_generation,
            legacy_generation,
        ] {
            let (_, task, _, _) = generation_info(store.raw(), generation).unwrap();
            forged(
                &store,
                &format!(
                    "INSERT INTO generation_abandonments VALUES
                       ({generation}, 'failed', {replan}, {generation}, NULL, 0)"
                ),
                "only a replan abandons",
            );
            forged(
                &store,
                &format!(
                    "INSERT INTO retry_authorizations
                       (after_generation, task_id, revision, replan_id, authorized_at)
                     VALUES ({generation}, {task}, 1, {replan}, 0)"
                ),
                "is retried",
            );
        }
        // The one generation never scheduled still ends as before.
        forged(
            &store,
            &format!(
                "UPDATE generations SET state = 'failed', ended_at = 1
                   WHERE id = {legacy_generation};
                 DELETE FROM ownership WHERE generation_id = {legacy_generation};
                 INSERT INTO retry_authorizations
                   (after_generation, task_id, revision, replan_id, authorized_at)
                 VALUES ({legacy_generation}, {legacy}, 1, {replan}, 0)"
            ),
            "is retried",
        );
        assert!(store.claims().unwrap().len() == 3);
    }

    #[test]
    fn a_replan_applies_as_a_whole_or_not_at_all() {
        let (_dir, mut store) = store();
        let tasks: [(&str, &[&str], &[&str]); 3] = [
            ("a", &["src/a.rs"], &[]),
            ("b", &["src/b.rs"], &["a"]),
            ("c", &["src/c.rs"], &[]),
        ];
        let (plan, ids) = running(&mut store, &tasks);
        let stop = stopped(&mut store, ids[0]);
        let valid = [
            update("a", "Do a differently"),
            add("x", &["src/x.rs"], &[]),
            cancel("c"),
            on("a", &["x"]),
            retry("a"),
        ];
        // One invalid command refuses every other, however late.
        for (last, expected) in [
            (add("y", &[], &["missing"]), "no task `missing`"),
            (retry("b"), "never ran"),
            (on("x", &["a"]), "would form a cycle"),
            (Command::RemoveTask { task: "b".into() }, "never removed"),
            (Command::Finalize {}, "finalized already"),
            (cancel("a"), "`b` depends on `a`, which is cancelled"),
        ] {
            let mut commands = valid.to_vec();
            commands.push(last);
            refused(&mut store, plan, &commands, expected);
        }
        // Checked as a whole, too: nothing may be left depending on a
        // cancelled task, whatever the order.
        refused(
            &mut store,
            plan,
            &[cancel("a"), add("y", &[], &[])],
            "depends on `a`",
        );
        let replan = applied(&mut store, plan, &valid);
        let tasks = store.tasks(plan).unwrap();
        assert_eq!(tasks[0].depends_on, [tasks[3].id]);
        assert_eq!(store.cancellation(tasks[2].id).unwrap(), Some(replan));
        assert_eq!(
            store.generations(tasks[0].id).unwrap()[0].state,
            GenerationState::Failed
        );
        assert_eq!(store.generations(tasks[0].id).unwrap()[0].id, stop);
        let records = store.replans(plan).unwrap();
        assert_eq!((records.len(), records[0].commands), (1, 5));
        assert_eq!(store.plan(plan).unwrap().intent, objective("intent"));
    }

    #[test]
    fn a_stale_proposal_is_never_applied() {
        let (dir, mut store) = store();
        let tasks: [(&str, &[&str], &[&str]); 2] =
            [("a", &["src/a.rs"], &[]), ("b", &["src/b.rs"], &[])];
        let (plan, ids) = running(&mut store, &tasks);
        stopped(&mut store, ids[0]);
        // The plan changes after the proposal's basis: another task is
        // claimed.
        let basis = store.replan_basis(plan).unwrap();
        claimed(&mut store, ids[1]);
        let before = everything(&store);
        let commands = [update("a", "Changed"), retry("a")];
        let stale = store.replan(plan, &basis, None, &commands, &|_| Ok(()), &|_| {
            Ok(Vec::new())
        });
        assert_eq!(stale.unwrap(), Replan::Stale);
        assert_eq!(everything(&store), before);
        // Any replan changes the basis, even one changing no definition.
        let basis = store.replan_basis(plan).unwrap();
        applied(&mut store, plan, &[update("a", "a")]);
        let stale = store.replan(plan, &basis, None, &commands, &|_| Ok(()), &|_| {
            Ok(Vec::new())
        });
        assert_eq!(stale.unwrap(), Replan::Stale);

        // Two planners proposing against one basis: one applies, the other
        // is stale, and nothing of it is lost into the first.
        for _ in 0..5 {
            let basis = store.replan_basis(plan).unwrap();
            let barrier = Barrier::new(2);
            let results: Vec<Replan> = thread::scope(|scope| {
                let handles: Vec<_> = ["first", "second"]
                    .into_iter()
                    .map(|objective| {
                        let (barrier, basis) = (&barrier, &basis);
                        let dir = &dir;
                        scope.spawn(move || {
                            let mut store = Store::open(&path(dir)).unwrap();
                            barrier.wait();
                            let commands = [update("a", objective)];
                            store
                                .replan(plan, basis, None, &commands, &|_| Ok(()), &|_| {
                                    Ok(Vec::new())
                                })
                                .unwrap()
                        })
                    })
                    .collect();
                handles.into_iter().map(|h| h.join().unwrap()).collect()
            });
            let winner = results
                .iter()
                .position(|r| matches!(r, Replan::Applied(_)))
                .unwrap();
            assert_eq!(results[1 - winner], Replan::Stale, "{results:?}");
            let objective = store.task(ids[0]).unwrap().objective;
            assert_eq!(objective, ["first", "second"][winner]);
        }
    }

    #[test]
    fn cancelling_supersedes_work_without_erasing_it() {
        let (_dir, mut store) = store();
        let tasks: [(&str, &[&str], &[&str]); 3] = [
            ("old", &["src/a.rs"], &[]),
            ("unrun", &["src/b.rs"], &[]),
            ("next", &["src/c.rs"], &["old"]),
        ];
        let (plan, ids) = running(&mut store, &tasks);
        let [old, unrun, next] = ids[..] else {
            panic!()
        };
        let generation = stopped(&mut store, old);
        refused(
            &mut store,
            plan,
            &[cancel("old")],
            "depends on `old`, which is cancelled",
        );
        refused(
            &mut store,
            plan,
            &[add("x", &[], &["unrun"]), cancel("unrun")],
            "cancelled",
        );
        // A replacement that takes over the same path, and the dependent
        // rewired to it, as one change.
        let replan = applied(
            &mut store,
            plan,
            &[
                cancel("old"),
                cancel("unrun"),
                add("replacement", &["src/a.rs"], &[]),
                on("next", &["replacement"]),
            ],
        );
        for task in [old, unrun] {
            assert_eq!(store.cancellation(task).unwrap(), Some(replan));
            assert_eq!(
                store.claim(task, LIMIT).unwrap(),
                Claim::Ineligible(TaskStatus::Cancelled)
            );
            for command in [
                update(&store.task(task).unwrap().key, "Again"),
                retry(&store.task(task).unwrap().key),
                cancel(&store.task(task).unwrap().key),
            ] {
                refused(&mut store, plan, &[command], "was cancelled");
            }
        }
        // Its history stays: the task, its generation, now ended, and the
        // definition it executed.
        assert_eq!(store.tasks(plan).unwrap().len(), 4);
        let generations = store.generations(old).unwrap();
        assert_eq!(
            (generations[0].id, generations[0].state),
            (generation, GenerationState::Failed)
        );
        assert_eq!(store.generation_revision(generation).unwrap(), Some(1));
        let replacement = store.tasks(plan).unwrap()[3].id;
        // Its path is free for the replacement, which `next` now awaits.
        claimed(&mut store, replacement);
        assert_eq!(
            store.claim(next, LIMIT).unwrap(),
            Claim::Ineligible(TaskStatus::WaitingForDependencies(vec![replacement]))
        );
        refused(&mut store, plan, &[on("next", &["old"])], "cancelled");
        let snapshot = store.snapshot(plan, LIMIT).unwrap();
        assert_eq!(snapshot.status(old), Some(&TaskStatus::Cancelled));
        forged(
            &store,
            &format!("DELETE FROM task_cancellations WHERE task_id = {old}"),
            "immutable",
        );
        forged(
            &store,
            &format!("INSERT INTO task_cancellations VALUES ({replacement}, {replan}, 0)"),
            "no active or accepted generation",
        );
    }

    #[test]
    fn dependencies_are_rewired_only_into_a_valid_dag() {
        let (_dir, mut store) = store();
        let tasks: [(&str, &[&str], &[&str]); 3] = [
            ("a", &["src/a.rs"], &[]),
            ("b", &["src/b.rs"], &["a"]),
            ("c", &["src/c.rs"], &["b"]),
        ];
        let (plan, ids) = running(&mut store, &tasks);
        running(&mut store, &[("elsewhere", &[], &[])]);
        refused(&mut store, plan, &[on("a", &["c"])], "would form a cycle");
        refused(
            &mut store,
            plan,
            &[on("b", &["b"])],
            "cannot depend on itself",
        );
        refused(
            &mut store,
            plan,
            &[on("b", &["missing"])],
            "no task `missing`",
        );
        refused(
            &mut store,
            plan,
            &[on("b", &["elsewhere"])],
            "no task `elsewhere`",
        );
        refused(&mut store, plan, &[on("b", &["a", "a"])], "more than once");
        // Valid only as a whole: `b` stops depending on `a` first.
        applied(
            &mut store,
            plan,
            &[on("b", &[]), on("c", &["a"]), on("a", &["b"])],
        );
        assert_eq!(store.task(ids[0]).unwrap().depends_on, [ids[1]]);
        assert!(store.task(ids[1]).unwrap().depends_on.is_empty());
        assert_eq!(store.task(ids[2]).unwrap().depends_on, [ids[0]]);
        assert!(store.dag_defects(plan).unwrap().is_empty());
        // Each change of definition is a revision: first what was planned,
        // then what the replan made current.
        let revisions = store.revisions(ids[0]).unwrap();
        assert_eq!(
            revisions
                .iter()
                .map(|r| (r.depends_on.clone(), r.replan.is_some()))
                .collect::<Vec<_>>(),
            [(vec![], false), (vec![ids[1]], true)]
        );
    }

    #[test]
    fn replanned_paths_stay_literal() {
        let (_dir, mut store) = store();
        let (plan, ids) = running(&mut store, &[("a", &["src/a.rs"], &[])]);
        stopped(&mut store, ids[0]);
        let literal = [
            "src/[id].rs",
            "src/with space.rs",
            "src/(group).rs",
            "src/a+b.rs",
            "src/@scope.rs",
            "src/日本語.rs",
        ];
        let paths = Command::UpdateTask {
            task: "a".into(),
            objective: None,
            context: None,
            paths: Some(strings(&literal)),
        };
        applied(&mut store, plan, &[paths, retry("a")]);
        let generation = claimed(&mut store, ids[0]);
        let mut expected = strings(&literal);
        expected.sort();
        assert_eq!(store.owned_paths(generation).unwrap(), expected);
        assert_eq!(store.revisions(ids[0]).unwrap()[1].scope, expected);
        assert_eq!(store.owner("src/id.rs").unwrap(), None);
    }

    #[test]
    fn the_database_binds_generations_to_true_definitions() {
        let (_dir, mut store) = store();
        let (plan, ids) = running(&mut store, &[("a", &["src/a.rs"], &[])]);
        let generation = stopped(&mut store, ids[0]);
        let a = ids[0];
        // A revision records the definition as it is, in order.
        forged(
            &store,
            &format!(
                "INSERT INTO task_revisions VALUES ({a}, 2, 'other', '', '[]', '[]', NULL, 0)"
            ),
            "current definition",
        );
        forged(
            &store,
            &format!(
                "INSERT INTO task_revisions SELECT task_id, 3, objective, context, scope,
                 depends_on, NULL, 0 FROM task_definitions WHERE task_id = {a}"
            ),
            "current definition",
        );
        forged(
            &store,
            &format!("UPDATE task_revisions SET objective = 'x' WHERE task_id = {a}"),
            "immutable",
        );
        forged(
            &store,
            &format!("DELETE FROM generation_revisions WHERE generation_id = {generation}"),
            "immutable",
        );
        applied(&mut store, plan, &[update("a", "Revised"), retry("a")]);
        // A fresh generation is bound only to the definition now current.
        let fresh = store.start_generation(a).unwrap();
        forged(
            &store,
            &format!("INSERT INTO generation_revisions VALUES ({fresh}, {a}, 1)"),
            "current definition",
        );
        store
            .raw()
            .execute(
                "UPDATE tasks SET objective = 'Behind its back' WHERE id = ?1",
                [a],
            )
            .unwrap();
        forged(
            &store,
            &format!("INSERT INTO generation_revisions VALUES ({fresh}, {a}, 2)"),
            "current definition",
        );
    }

    #[test]
    fn a_task_keeps_its_identity_beneath_any_authorization() {
        let (_dir, mut store) = store();
        let tasks: [(&str, &[&str], &[&str]); 2] =
            [("a", &["src/a.rs"], &[]), ("b", &["src/b.rs"], &[])];
        let (plan, ids) = running(&mut store, &tasks);
        let [a, b] = ids[..] else { panic!() };
        let first = stopped(&mut store, a);
        let given = applied(&mut store, plan, &[retry("a")]);
        let authorized = store.retry_authorizations(a).unwrap()[0].revision;
        // The authorization admits exactly the task it was given for: the
        // claimed generation is bound to the authorized revision, and
        // everything its executor is told of the task is that revision
        // under the task's unchanged identity.
        let renamed = store
            .raw()
            .execute("UPDATE tasks SET key = 'renamed' WHERE id = ?1", [a])
            .is_err();
        let second = claimed(&mut store, a);
        let task = store.task(a).unwrap();
        let revision = &store.revisions(a).unwrap()[authorized as usize - 1];
        assert_eq!(
            (renamed, task.plan, task.key.as_str()),
            (true, plan, "a"),
            "the authorization admitted a task it was not given for"
        );
        assert_eq!(store.generation_revision(second).unwrap(), Some(authorized));
        assert_eq!(
            (
                &task.objective,
                &task.context,
                &task.scope,
                &task.depends_on
            ),
            (
                &revision.objective,
                &revision.context,
                &revision.scope,
                &revision.depends_on
            )
        );
        assert_eq!(
            store.execution_authority(a, second).unwrap(),
            revision.scope
        );
        assert_eq!(
            store.retry_authorizations(a).unwrap()[0].used_by,
            Some(second)
        );
        assert_eq!(store.retry_authorizations(a).unwrap()[0].replan, given);
        // History stays attributed as it was.
        assert_eq!(store.generation_revision(first).unwrap(), Some(1));
        assert_eq!(
            store
                .generations(a)
                .unwrap()
                .iter()
                .map(|g| g.id)
                .collect::<Vec<_>>(),
            [first, second]
        );
        // The same keys in every other state a plan's tasks are in.
        let (ready, ready_ids) = ready_plan(&mut store, &tasks);
        let (paused, paused_ids) = running(
            &mut store,
            &[("a", &["src/p.rs"], &[]), ("b", &["src/q.rs"], &[])],
        );
        stopped(&mut store, paused_ids[0]);
        store.set_plan_state(paused, PlanState::Paused).unwrap();
        let draft = store.create_plan(&objective("draft")).unwrap();
        assert!(
            !store
                .revise_plan(draft, &[add("a", &[], &[])], &|_| Ok(()))
                .unwrap()
        );
        let draft_a = store.tasks(draft).unwrap()[0].id;
        let everyone = [
            a,
            b,
            ready_ids[0],
            ready_ids[1],
            paused_ids[0],
            paused_ids[1],
            draft_a,
        ];

        // Nothing identifying a task changes beneath `Store`: not its key,
        // not its plan (whose intent its executors are given), not its id,
        // whether or not it has generations or an authorization pending.
        // REPLACE conflict resolution deletes a conflicting row without any
        // UPDATE or DELETE trigger firing, so every form is tried, however
        // the replacing row names the task: by id, by plan and key, by the id
        // of one and the key of another, or exactly as it is.
        let keys = |store: &Store| {
            everyone
                .iter()
                .map(|&t| {
                    let task = store.task(t).unwrap();
                    (task.id, task.plan, task.key)
                })
                .collect::<Vec<_>>()
        };
        let before = keys(&store);
        let identity = "a task's identity is immutable";
        for &task in &everyone {
            let Task { plan: of, key, .. } = store.task(task).unwrap();
            let sibling = if key == "a" { "b" } else { "a" };
            let elsewhere = if of == ready { plan } else { ready };
            for sql in [
                format!("UPDATE tasks SET key = 'renamed' WHERE id = {task}"),
                format!("UPDATE OR REPLACE tasks SET key = '{sibling}' WHERE id = {task}"),
                format!("UPDATE tasks SET plan_id = {elsewhere} WHERE id = {task}"),
                format!("UPDATE tasks SET id = 9999 WHERE id = {task}"),
                format!("UPDATE tasks SET created_at = created_at + 1 WHERE id = {task}"),
            ] {
                forged(&store, &sql, identity);
            }
            let columns = "id, plan_id, key, objective, context, created_at";
            for verb in ["INSERT OR REPLACE", "REPLACE"] {
                for replacement in [
                    "id, plan_id, 'renamed', objective, context, created_at".into(),
                    format!("id, {elsewhere}, key, objective, context, created_at"),
                    "id, plan_id, key, objective, context, created_at + 1".into(),
                    "9999, plan_id, key, objective, context, created_at".into(),
                    "NULL, plan_id, key, 'Other', context, created_at".into(),
                    format!("id, plan_id, '{sibling}', objective, context, created_at"),
                    columns.to_string(),
                ] {
                    let sql = format!(
                        "{verb} INTO tasks ({columns}) SELECT {replacement} FROM tasks
                         WHERE id = {task}"
                    );
                    forged(&store, &sql, identity);
                }
            }
        }
        assert_eq!(keys(&store), before);
        for &task in &everyone {
            let definition = store.task(task).unwrap();
            assert!(!definition.objective.is_empty() && definition.objective != "Other");
        }
        // Its definition alone is revised, by its planner.
        assert!(
            !store
                .revise_plan(draft, &[update("a", "Draft again")], &|_| Ok(()))
                .unwrap()
        );
        assert_eq!(store.task(draft_a).unwrap().objective, "Draft again");

        // A key names one task of its plan, and the same key another plan's.
        forged(
            &store,
            &format!(
                "INSERT INTO tasks (plan_id, key, objective, context, created_at)
                 VALUES ({plan}, 'a', 'Again', '', 0)"
            ),
            identity,
        );
        refused(&mut store, plan, &[add("a", &[], &[])], "already exists");
        applied(&mut store, plan, &[add("c", &["src/c.rs"], &[])]);
        assert_eq!(store.task(ids[0]).unwrap().key, "a");
        assert_eq!(store.task(ready_ids[0]).unwrap().key, "a");
        // A wholly new row is inserted as ever, SQLite assigning its id.
        store
            .raw()
            .execute_batch(&format!(
                "INSERT INTO tasks (plan_id, key, objective, context, created_at)
                 VALUES ({draft}, 'new', 'New', '', 0)"
            ))
            .unwrap();
        let new = TaskId(store.raw().last_insert_rowid());
        assert_eq!(store.task(new).unwrap().key, "new");
    }

    #[test]
    fn a_removed_draft_task_is_added_again_as_a_new_task() {
        let (_dir, mut store) = store();
        let draft = store.create_plan(&objective("draft")).unwrap();
        let revise = |store: &mut Store, commands: &[Command]| {
            assert!(!store.revise_plan(draft, commands, &|_| Ok(())).unwrap());
        };
        revise(
            &mut store,
            &[add("a", &["src/a.rs"], &[]), add("b", &[], &[])],
        );
        let [a, b] = store
            .tasks(draft)
            .unwrap()
            .iter()
            .map(|t| t.id)
            .collect::<Vec<_>>()[..]
        else {
            panic!()
        };
        // Its key is free once it is actually removed, and only then.
        let taken = err(store.revise_plan(draft, &[add("a", &[], &[])], &|_| Ok(())));
        assert!(taken.contains("already exists"), "{taken}");
        revise(&mut store, &[Command::RemoveTask { task: "a".into() }]);
        revise(&mut store, &[add("a", &["src/c.rs"], &["b"])]);
        let again = store.tasks(draft).unwrap();
        let readded = again.iter().find(|t| t.key == "a").unwrap();
        assert_eq!(
            (
                readded.objective.as_str(),
                &readded.scope[..],
                &readded.depends_on[..]
            ),
            ("Do a", &["src/c.rs".to_string()][..], &[b][..])
        );
        // Even under the removed task's id, which SQLite may assign again.
        revise(&mut store, &[Command::RemoveTask { task: "a".into() }]);
        store
            .raw()
            .execute_batch(&format!(
                "INSERT INTO tasks (id, plan_id, key, objective, context, created_at)
                 VALUES ({a}, {draft}, 'c', 'Do c', '', 0)"
            ))
            .unwrap();
        assert_eq!(store.task(a).unwrap().key, "c");
    }

    #[test]
    fn replanning_is_for_finalized_plans_only() {
        let (_dir, mut store) = store();
        let plan = store.create_plan(&objective("intent")).unwrap();
        store.add_task(plan, "a", &[]).unwrap();
        refused(
            &mut store,
            plan,
            &[update("a", "x")],
            "only a finalized plan",
        );
        let (ready, _) = ready_plan(&mut store, &[("b", &[], &[])]);
        applied(&mut store, ready, &[update("b", "x")]);
        store.set_plan_state(ready, PlanState::Running).unwrap();
        let raise = Command::RaiseAttention {
            concern: "unclear".into(),
            reason: "Unclear".into(),
            evidence: vec!["b".into()],
            tasks: Vec::new(),
        };
        applied(&mut store, ready, &[raise]);
        refused(
            &mut store,
            ready,
            &[update("b", "y")],
            "awaits a human decision",
        );
        store
            .set_plan_state(ready, PlanState::Planning)
            .unwrap_err();
        refused(&mut store, ready, &[], "awaits a human decision");
    }
}
