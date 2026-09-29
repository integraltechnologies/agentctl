//! Cancelling a plan's live provider work at a human's request.
//!
//! A request is recorded here, canonically, and pauses a running plan in
//! the same transaction, so that nothing new of it is claimed. The agentctl
//! process running each invocation it covers observes it and ends that
//! invocation by procd's authority, recording how it ended as ever (see
//! `crate::runtime`); a pipeline it covers starts no further invocation.
//! Recording a request ends nothing by itself, marks nothing cancelled and
//! releases nothing: an invocation is cancelled only once procd proved its
//! processes gone, and whatever its generation owns stays owned until the
//! planner decides what follows, as for any attempt that stopped short.

use anyhow::{Result, ensure};
use rusqlite::params;

use super::{
    GenerationId, InvocationId, InvocationState, Liveness, PlanId, PlanState, Store, event, now,
    plan_state,
};

/// Invocations a request covers, joined as `i` with its agent `a` and
/// cancellation `c`: those serving the plan's generations up to
/// `through_generation`, and its other invocations up to
/// `through_invocation`.
const COVERED: &str = "
    FROM invocations i JOIN agents a ON a.id = i.agent_id
    LEFT JOIN generations g ON g.id = a.generation_id
    LEFT JOIN tasks t ON t.id = g.task_id
    JOIN plan_cancellations c ON c.plan_id = coalesce(a.plan_id, t.plan_id)
        AND CASE WHEN a.generation_id IS NULL THEN i.id <= c.through_invocation
            ELSE a.generation_id <= c.through_generation END";

/// A cancellation request, as recorded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CancellationRequest {
    pub id: i64,
    pub plan: PlanId,
    /// Whether recording it paused the plan, which was running.
    pub paused: bool,
}

/// Where one invocation a cancellation request covers stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Covered {
    pub invocation: InvocationId,
    pub state: InvocationState,
    /// While it has no recorded end: whether the agentctl process running
    /// it still runs, as its session's lock establishes. `false` means that
    /// process ended, or whether it did cannot be established: nothing then
    /// ends the invocation but recovery, where procd can establish its fate.
    pub controlled: bool,
}

impl Store {
    /// Requests that every live provider invocation of `plan` end, pausing
    /// the plan if it is running. A completed plan has nothing to cancel.
    pub fn request_cancellation(&mut self, plan: PlanId) -> Result<CancellationRequest> {
        self.write(|tx| {
            let state = plan_state(tx, plan)?;
            ensure!(
                state != PlanState::Completed,
                "plan {plan} is completed; nothing of it runs"
            );
            let paused = state == PlanState::Running;
            if paused {
                tx.execute(
                    "UPDATE plans SET state = ?2, updated_at = ?3 WHERE id = ?1",
                    params![plan, PlanState::Paused, now()],
                )?;
                let detail = format!("{state} -> {}", PlanState::Paused);
                event(tx, "plan.state", Some(plan), None, None, &detail)?;
            }
            tx.execute(
                "INSERT INTO plan_cancellations
                   (plan_id, through_generation, through_invocation, requested_at)
                 VALUES (?1, (SELECT coalesce(max(id), 0) FROM generations),
                     (SELECT coalesce(max(id), 0) FROM invocations), ?2)",
                params![plan, now()],
            )?;
            let id = tx.last_insert_rowid();
            event(
                tx,
                "plan.cancellation_requested",
                Some(plan),
                None,
                None,
                &format!("request {id}"),
            )?;
            Ok(CancellationRequest { id, plan, paused })
        })
    }

    /// Whether a human requested that `invocation` end.
    pub fn cancellation_requested(&self, invocation: InvocationId) -> Result<bool> {
        Ok(self.conn.query_row(
            &format!("SELECT EXISTS (SELECT 1 {COVERED} WHERE i.id = ?1)"),
            [invocation],
            |r| r.get(0),
        )?)
    }

    /// Whether a human requested that the live work of `generation` end:
    /// its pipeline then starts no further invocation.
    pub fn generation_cancelled(&self, generation: GenerationId) -> Result<bool> {
        Ok(self.conn.query_row(
            "SELECT EXISTS (SELECT 1 FROM generations g JOIN tasks t ON t.id = g.task_id
                 JOIN plan_cancellations c ON c.plan_id = t.plan_id
                 WHERE g.id = ?1 AND g.id <= c.through_generation)",
            [generation],
            |r| r.get(0),
        )?)
    }

    /// Every invocation the cancellation request `request` covers that had
    /// not ended when it was recorded, in order, with where each stands now.
    pub fn covered(&self, request: i64) -> Result<Vec<Covered>> {
        let rows: Vec<(InvocationId, InvocationState, String)> = self
            .conn
            .prepare(&format!(
                "SELECT i.id, i.state, i.session {COVERED}
                 WHERE c.id = ?1 AND (i.ended_at IS NULL OR i.ended_at >= c.requested_at)
                 ORDER BY i.id"
            ))?
            .query_map([request], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<rusqlite::Result<_>>()?;
        rows.into_iter()
            .map(|(invocation, state, session)| {
                let controlled =
                    !state.is_terminal() && matches!(self.liveness(&session)?, Liveness::Running);
                Ok(Covered {
                    invocation,
                    state,
                    controlled,
                })
            })
            .collect()
    }
}
