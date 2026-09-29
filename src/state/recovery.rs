//! Recovery's canonical side: what agentctl processes that ended left
//! unresolved, the barrier that keeps new work from starting beside it,
//! and the records settling it once recovery established what can be
//! proven (see `crate::recovery`).
//!
//! Whatever may still be live, or whose outcome is not yet established, is
//! listed by `unresolved_authority`, each with the session that recorded
//! it. While that session runs, it is its work; once it ended, it is
//! interrupted, and nothing new starts on its plan until recovery settled
//! it: no claim, ownership, executor, verifier, planner or integration
//! verification, and no acceptance. Other plans are not held back: nothing
//! interrupted in one can be acted on through another's work, as ownership
//! is exclusive and every other mutation is of its own plan.
//!
//! Settling never guesses. Each record here needs the proof that its
//! session ended ([`Ended`]), and records only what recovery established:
//! that an invocation's lifecycle is settled, which never says how it
//! ended ('interrupted'); that an action left intended was never attempted
//! (withdrawn); that an attempted action's outcome was lost with its
//! process ('interrupted', which judges nothing and accepts nothing); or
//! how far an install got, from what the working tree holds.

use std::fs::{File, OpenOptions, TryLockError};

use anyhow::{Context, Result, bail, ensure};
use rusqlite::{Connection, OptionalExtension, Transaction, params};

use super::session::Ended;
use super::{
    ActionOutcome, Evidence, ExecutionId, ExecutionOutcome, GenerationId, InstallOutcome,
    IntegrationId, IntegrationOutcome, InvocationEnd, InvocationId, InvocationState, JournalId,
    Liveness, PlanId, Store, TaskId, Usage, VerificationId, VerificationOutcome, event,
    generation_info, now, reconcile_entry,
};

/// One thing an agentctl process recorded that is unresolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unresolved {
    pub kind: UnresolvedKind,
    pub plan: PlanId,
    /// The generation it serves, if it serves one.
    pub generation: Option<GenerationId>,
    /// The session that recorded it.
    pub session: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnresolvedKind {
    /// An invocation with no recorded end.
    Invocation(InvocationId),
    /// A journaled action never attempted, nor withdrawn.
    Intended(JournalId),
    /// A journaled action attempted, with its outcome unknown.
    Attempted(JournalId),
    /// A claim not released.
    Claim(GenerationId),
    /// An acceptance not completed.
    Acceptance(GenerationId),
}

/// What an attempted action acted on, as its journal entry's use says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ActedOn {
    Execution(ExecutionId),
    /// Installing the candidate of the execution.
    Install(ExecutionId),
    Verification(VerificationId),
    Integration(IntegrationId),
    /// A planner's replanning.
    Replan,
    /// An action recovery knows nothing of.
    Other(String),
}

impl Store {
    /// Everything unresolved, of `plan` or of every plan, in order.
    pub fn unresolved(&self, plan: Option<PlanId>) -> Result<Vec<Unresolved>> {
        self.conn
            .prepare(
                "SELECT kind, subject, plan_id, generation_id, session FROM unresolved_authority
                 WHERE ?1 IS NULL OR plan_id = ?1
                 ORDER BY plan_id, session, CASE kind WHEN 'invocation' THEN 0
                   WHEN 'attempted' THEN 1 WHEN 'intended' THEN 2 WHEN 'acceptance' THEN 3
                   ELSE 4 END, subject",
            )?
            .query_map([plan], |r| {
                let subject: i64 = r.get(1)?;
                let kind = match r.get::<_, String>(0)?.as_str() {
                    "invocation" => UnresolvedKind::Invocation(InvocationId(subject)),
                    "intended" => UnresolvedKind::Intended(JournalId(subject)),
                    "attempted" => UnresolvedKind::Attempted(JournalId(subject)),
                    "claim" => UnresolvedKind::Claim(GenerationId(subject)),
                    _ => UnresolvedKind::Acceptance(GenerationId(subject)),
                };
                Ok(Unresolved {
                    kind,
                    plan: r.get(2)?,
                    generation: r.get(3)?,
                    session: r.get(4)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()
            .map_err(Into::into)
    }

    /// Why no new work may start on `plan` now, if none may: agentctl
    /// processes that ended left some of its work unresolved, or recovery
    /// is settling it. Work of processes still running is theirs, and
    /// holds nothing back.
    pub fn recovery_required(&self, plan: PlanId) -> Result<Option<String>> {
        let sessions: Vec<String> = self
            .conn
            .prepare("SELECT DISTINCT session FROM unresolved_authority WHERE plan_id = ?1")?
            .query_map([plan], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        let mut ended = 0;
        for session in &sessions {
            if !matches!(self.liveness(session)?, Liveness::Running) {
                ended += 1;
            }
        }
        if ended > 0 {
            return Ok(Some(format!(
                "recovery required: agentctl processes that are no longer running left work \
                 of plan {plan} unresolved; run `agentctl recover`"
            )));
        }
        let lock = self.recovery_lock()?;
        match lock.try_lock_shared() {
            Ok(()) => {
                // Given up now, not with the descriptor: see `recovery::Held`.
                let _ = lock.unlock();
                Ok(None)
            }
            Err(TryLockError::WouldBlock) => Ok(Some(format!(
                "recovery required: `agentctl recover` is settling interrupted work, \
                 plan {plan}'s included"
            ))),
            Err(TryLockError::Error(_)) => Ok(None),
        }
    }

    /// Refuses new work on `plan` while [`Store::recovery_required`] says
    /// why it must wait.
    pub(crate) fn barrier(&self, plan: PlanId) -> Result<()> {
        match self.recovery_required(plan)? {
            Some(why) => bail!(why),
            None => Ok(()),
        }
    }

    /// [`Store::barrier`] for `task`'s plan.
    pub(crate) fn barrier_of(&self, task: TaskId) -> Result<()> {
        let plan: PlanId = self
            .conn
            .query_row("SELECT plan_id FROM tasks WHERE id = ?1", [task], |r| {
                r.get(0)
            })
            .optional()?
            .with_context(|| format!("task {task} does not exist"))?;
        self.barrier(plan)
    }

    /// The file recovery holds exclusively while it settles anything, which
    /// [`Store::recovery_required`] finds held.
    pub(crate) fn recovery_lock(&self) -> Result<File> {
        let path = self.path.with_file_name("recovery.lock");
        OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .with_context(|| format!("opening {}", path.display()))
    }

    /// The task `generation` is of.
    pub(crate) fn task_of(&self, generation: GenerationId) -> Result<TaskId> {
        Ok(generation_info(&self.conn, generation)?.1)
    }

    /// What the attempted journal entry `entry` acted on.
    pub(crate) fn attempt(&self, entry: JournalId) -> Result<ActedOn> {
        let conn = &self.conn;
        let find = |sql: &str| -> Result<Option<i64>> {
            Ok(conn.query_row(sql, [entry], |r| r.get(0)).optional()?)
        };
        if let Some(id) = find("SELECT id FROM executions WHERE journal_id = ?1")? {
            return Ok(ActedOn::Execution(ExecutionId(id)));
        }
        if let Some(id) = find("SELECT execution_id FROM execution_installs WHERE journal_id = ?1")?
        {
            return Ok(ActedOn::Install(ExecutionId(id)));
        }
        if let Some(id) = find("SELECT id FROM verifications WHERE journal_id = ?1")? {
            return Ok(ActedOn::Verification(VerificationId(id)));
        }
        if let Some(id) = find("SELECT id FROM integration_verifications WHERE journal_id = ?1")? {
            return Ok(ActedOn::Integration(IntegrationId(id)));
        }
        let (action, role): (String, String) = conn.query_row(
            "SELECT j.action, a.role FROM journal j JOIN agents a ON a.id = j.agent_id
             WHERE j.id = ?1",
            [entry],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        Ok(match (action.as_str(), role.as_str()) {
            ("planner.replan", "planner") => ActedOn::Replan,
            _ => ActedOn::Other(action),
        })
    }

    /// Records that `invocation`, of the ended session, was interrupted:
    /// recovery found no process of it, or terminated every one it found,
    /// as `diagnostic` says. How it would have ended is not known.
    pub(crate) fn interrupt_invocation(
        &mut self,
        ended: &Ended,
        invocation: InvocationId,
        diagnostic: &str,
    ) -> Result<()> {
        let session: String = self.conn.query_row(
            "SELECT session FROM invocations WHERE id = ?1",
            [invocation],
            |r| r.get(0),
        )?;
        // A session never changes, so this holds within the write too.
        ensure!(
            session == ended.session(),
            "invocation {invocation} is not of session {}",
            ended.session()
        );
        self.finish_invocation(
            invocation,
            &InvocationEnd {
                state: InvocationState::Interrupted,
                failure: None,
                diagnostic: Some(diagnostic.into()),
                exit_code: None,
                provider_session: None,
                usage: Usage::Unavailable,
            },
        )
    }

    /// Withdraws `entry`, left intended by the ended session, which never
    /// attempted it and never will.
    pub(crate) fn withdraw(&mut self, ended: &Ended, entry: JournalId) -> Result<()> {
        self.write(|tx| {
            let (plan, task, agent) = entry_of(tx, ended, entry, "intended")?;
            tx.execute(
                "INSERT INTO journal_withdrawals (journal_id, withdrawn_at) VALUES (?1, ?2)",
                params![entry, now()],
            )?;
            let detail = format!("entry {entry}: never attempted");
            event(
                tx,
                "journal.withdrawn",
                Some(plan),
                task,
                Some(agent),
                &detail,
            )
        })
    }

    /// Records that the execution attempted by `entry`, of the ended
    /// session, was interrupted before its workspace was captured, with the
    /// project's Git HEAD `head` now: nothing about what its executor did
    /// is established. Its invocation must have ended.
    pub(crate) fn interrupt_execution(
        &mut self,
        ended: &Ended,
        entry: JournalId,
        head: &str,
    ) -> Result<()> {
        self.write(|tx| {
            let (plan, task, _) = entry_of(tx, ended, entry, "attempted")?;
            let (execution, generation): (ExecutionId, GenerationId) = tx.query_row(
                "SELECT id, generation_id FROM executions WHERE journal_id = ?1",
                [entry],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            let invocation = ended_invocation(tx, entry)?;
            tx.execute(
                "INSERT INTO execution_captures (execution_id, outcome, head_after, captured_at)
                 VALUES (?1, ?2, ?3, ?4)",
                params![execution, ExecutionOutcome::Interrupted, head, now()],
            )?;
            let evidence = [
                Evidence::Invocation { invocation },
                Evidence::Fact {
                    name: "execution.interrupted".into(),
                },
            ];
            reconcile_entry(tx, entry, ActionOutcome::Failed, &evidence)?;
            let (_, _, number, _) = generation_info(tx, generation)?;
            let detail = format!("generation {number}: {}", ExecutionOutcome::Interrupted);
            event(tx, "execution.captured", Some(plan), task, None, &detail)
        })
    }

    /// Records how the install attempted by `entry`, of the ended session,
    /// ended, as recovery found the working tree: `Installed` when every
    /// changed path held the candidate, or `Failed` once every one held
    /// what it held before, having been restored.
    pub(crate) fn recover_install(
        &mut self,
        ended: &Ended,
        entry: JournalId,
        outcome: InstallOutcome,
    ) -> Result<()> {
        ensure!(
            matches!(outcome, InstallOutcome::Installed | InstallOutcome::Failed),
            "an interrupted install ends installed, or failed and restored"
        );
        self.write(|tx| {
            let (plan, task, _) = entry_of(tx, ended, entry, "attempted")?;
            let (execution, generation): (ExecutionId, GenerationId) = tx.query_row(
                "SELECT i.execution_id, e.generation_id FROM execution_installs i
                 JOIN executions e ON e.id = i.execution_id WHERE i.journal_id = ?1",
                [entry],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            tx.execute(
                "INSERT INTO execution_install_results (execution_id, outcome, finished_at)
                 VALUES (?1, ?2, ?3)",
                params![execution, outcome, now()],
            )?;
            let action = match outcome {
                InstallOutcome::Installed => ActionOutcome::CompletedAsIntended,
                _ => ActionOutcome::Failed,
            };
            let evidence = [
                Evidence::Fact {
                    name: format!("install.{outcome}"),
                },
                Evidence::Fact {
                    name: "install.interrupted".into(),
                },
            ];
            reconcile_entry(tx, entry, action, &evidence)?;
            let (_, _, number, _) = generation_info(tx, generation)?;
            let detail = format!("generation {number}: {outcome}, once interrupted");
            event(tx, "execution.installed", Some(plan), task, None, &detail)
        })
    }

    /// Records that the verification attempted by `entry`, of the ended
    /// session, was interrupted: no judgment. Its invocation must have
    /// ended.
    pub(crate) fn interrupt_verification(&mut self, ended: &Ended, entry: JournalId) -> Result<()> {
        self.write(|tx| {
            entry_of(tx, ended, entry, "attempted")?;
            ended_invocation(tx, entry)?;
            let verification: VerificationId = tx.query_row(
                "SELECT id FROM verifications WHERE journal_id = ?1",
                [entry],
                |r| r.get(0),
            )?;
            super::verification::record(
                tx,
                verification,
                VerificationOutcome::Interrupted,
                &[],
                &[],
                None,
            )
        })
    }

    /// Records that the integration verification attempted by `entry`, of
    /// the ended session, was interrupted: no judgment, and its plan is not
    /// completed. Its invocation must have ended.
    pub(crate) fn interrupt_integration(&mut self, ended: &Ended, entry: JournalId) -> Result<()> {
        self.write(|tx| {
            entry_of(tx, ended, entry, "attempted")?;
            let invocation = ended_invocation(tx, entry)?;
            let id: IntegrationId = tx.query_row(
                "SELECT id FROM integration_verifications WHERE journal_id = ?1",
                [entry],
                |r| r.get(0),
            )?;
            super::integration::record(
                tx,
                id,
                invocation,
                IntegrationOutcome::Interrupted,
                &[],
                None,
                None,
            )
        })
    }

    /// Records that the replanning attempted by `entry`, of the ended
    /// session, failed: no replan applied it, as the store holds none, and
    /// what its planner proposed is lost. Its invocation must have ended.
    pub(crate) fn interrupt_replan(&mut self, ended: &Ended, entry: JournalId) -> Result<()> {
        self.write(|tx| {
            let (plan, _, agent) = entry_of(tx, ended, entry, "attempted")?;
            let invocation = ended_invocation(tx, entry)?;
            // A replan reconciles its entry in the transaction applying it.
            let applied: bool = tx.query_row(
                "SELECT EXISTS (SELECT 1 FROM replans WHERE journal_id = ?1)",
                [entry],
                |r| r.get(0),
            )?;
            ensure!(!applied, "entry {entry} was applied by a replan");
            let evidence = [
                Evidence::Invocation { invocation },
                Evidence::Fact {
                    name: "replan.interrupted".into(),
                },
            ];
            reconcile_entry(tx, entry, ActionOutcome::Failed, &evidence)?;
            let detail = format!("entry {entry}: interrupted, nothing applied");
            event(
                tx,
                "planner.interrupted",
                Some(plan),
                None,
                Some(agent),
                &detail,
            )
        })
    }

    /// Records why recovery could not settle something of `plan`, unless
    /// that was recorded already and nothing else of the plan happened
    /// since: evidence that stays with the plan, as does the record it could
    /// not settle, however often recovery finds it so.
    pub(crate) fn recovery_blocked(&mut self, plan: PlanId, detail: &str) -> Result<()> {
        self.write(|tx| {
            let recorded: bool = tx.query_row(
                "SELECT EXISTS (SELECT 1 FROM events
                   WHERE kind = 'recovery.blocked' AND plan_id = ?1 AND detail = ?2
                     AND seq > coalesce((SELECT max(seq) FROM events
                       WHERE plan_id = ?1 AND kind <> 'recovery.blocked'), 0))",
                params![plan, detail],
                |r| r.get(0),
            )?;
            match recorded {
                true => Ok(()),
                false => event(tx, "recovery.blocked", Some(plan), None, None, detail),
            }
        })
    }
}

/// The plan, task and agent of `entry`, which must be of the ended session
/// and in `state`.
fn entry_of(
    tx: &Transaction,
    ended: &Ended,
    entry: JournalId,
    state: &str,
) -> Result<(PlanId, Option<TaskId>, super::AgentId)> {
    let (agent, session, actual, withdrawn): (super::AgentId, String, String, bool) = tx
        .query_row(
            "SELECT agent_id, session, state,
               EXISTS (SELECT 1 FROM journal_withdrawals w WHERE w.journal_id = journal.id)
             FROM journal WHERE id = ?1",
            [entry],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional()?
        .with_context(|| format!("journal entry {entry} does not exist"))?;
    ensure!(
        session == ended.session(),
        "journal entry {entry} is not of session {}",
        ended.session()
    );
    ensure!(
        actual == state && !withdrawn,
        "journal entry {entry} is {actual}, not {state}"
    );
    let (plan, task) = super::agent_subject(tx, agent)?;
    Ok((plan, task, agent))
}

/// The invocation that attempted `entry`, which must have ended.
fn ended_invocation(conn: &Connection, entry: JournalId) -> Result<InvocationId> {
    let (invocation, state): (Option<InvocationId>, Option<InvocationState>) = conn.query_row(
        "SELECT j.invocation_id, i.state FROM journal j
         LEFT JOIN invocations i ON i.id = j.invocation_id WHERE j.id = ?1",
        [entry],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let invocation = invocation.with_context(|| format!("entry {entry} names no invocation"))?;
    ensure!(
        state.is_some_and(InvocationState::is_terminal),
        "invocation {invocation} has not ended"
    );
    Ok(invocation)
}
