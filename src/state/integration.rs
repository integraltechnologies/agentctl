//! Final integration verification: a fresh verifier's judgment of a plan's
//! accepted result as a whole, and the only way a plan is completed.
//!
//! A planner proposes that its plan's objective is met, as the whole of a
//! replan, only while the plan is settled (the `settled_plans` view): its
//! work is wholly accepted or cancelled, nothing of it is live or unknown,
//! no concern blocks it, no acceptance of its work is unfinished and the
//! source its work accepted is synchronized with CodeGraph. Other plans'
//! unfinished acceptances and stale graphs do not block it: its basis
//! binds what it relies on of them. That proposal only makes verification
//! possible. A verification of it is intended, with a verifier agent of the
//! plan created for it alone and that agent's one journal entry, while the
//! proposal is the plan's latest replan and the plan is settled, together
//! with its basis: the exact accepted source it verifies, every other
//! repository input its verifier is given (`integration_inputs`) and the
//! plan's replanning state. Its entry is attempted before any verifier
//! process exists, and only recording how it ended reconciles it.
//!
//! How it ended is derived first from whether its basis still holds once
//! the verifier ended: the proposal still the plan's latest replan, the plan
//! still settled with the same replanning state, accepted source exactly
//! what it verified, and the repository inputs agentctl observed then
//! exactly those recorded. Otherwise nothing the verifier reported is about
//! the plan as it stands. Only then do its workspace and its report
//! decide. A pass completes the plan in the very transaction recording it;
//! a failure's blockers are durable feedback for the plan's next planner,
//! and complete, retry or change nothing.

use std::collections::BTreeMap;

use anyhow::{Context, Result, bail, ensure};
use rusqlite::types::Type;
use rusqlite::{Connection, OptionalExtension, Row, Transaction, params};
use serde_json::json;

use super::execution::check_content;
use super::replanning::basis;
use super::scheduling::task_status;
use super::verification::{PATH_LIMIT, VerifierResult};
use super::{
    ActionOutcome, AgentId, AgentScope, Content, Evidence, IntegrationId, IntegrationOutcome,
    Intent, InvocationId, InvocationState, JournalId, PlanId, PlanState, ReplanId, Role, Store,
    TaskStatus, Verdict, VerifierReport, check_path, event, insert_agent, insert_intent,
    json_column, now, plan_state, reconcile_entry,
};

/// The journal action of an integration verification.
const ACTION: &str = "verifier.integrate";
/// At most this many mutated workspace paths are recorded, then the first
/// ones in order.
const MUTATED_LIMIT: usize = 256;

/// One final integration verification of a plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Integration {
    pub id: IntegrationId,
    pub plan: PlanId,
    /// The replan proposing completion that it acts on.
    pub proposal: ReplanId,
    /// Which verification of the proposal this is, from 1.
    pub number: i64,
    /// The verifier: a logical agent of the plan serving only this.
    pub agent: AgentId,
    pub journal: JournalId,
    /// The plan's replanning state when intended (see `Store::replan_basis`).
    pub basis: String,
    pub started_at: i64,
    pub status: IntegrationStatus,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IntegrationStatus {
    /// Intended, and never attempted: no verifier ran.
    Intended,
    /// Attempted: a verifier may have run, and nothing about how the
    /// verification ended is established.
    OutcomeUnknown {
        invocation: Option<InvocationId>,
    },
    Finished(IntegrationResult),
}

/// How an integration verification ended, as recorded with its
/// reconciliation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IntegrationResult {
    pub outcome: IntegrationOutcome,
    pub invocation: InvocationId,
    /// Observed by agentctl: repository source the verifier changed in its
    /// workspace, in order, at most `MUTATED_LIMIT` paths.
    pub mutated: Vec<String>,
    /// The verifier's own report, when well formed: its claims, whatever
    /// the outcome.
    pub report: Option<VerifierReport>,
    pub at: i64,
}

/// What agentctl observed once an integration verifier's invocation ended.
#[derive(Debug, Clone, Copy)]
pub(crate) struct IntegrationObserved<'a> {
    /// Repository source paths whose entry in the verifier's workspace
    /// differed from what was staged there.
    pub mutated: &'a [String],
    /// The repository inputs standing then (see `crate::integration`), or
    /// `None` when they could not be observed.
    pub inputs: Option<&'a [(String, Content)]>,
    pub result: VerifierResult<'a>,
}

impl Store {
    /// The replan proposing `plan`'s completion, while it is the plan's
    /// latest.
    pub fn completion_proposal(&self, plan: PlanId) -> Result<Option<ReplanId>> {
        plan_state(&self.conn, plan)?;
        proposal(&self.conn, plan)
    }

    /// INTEND: records a final integration verification of `plan`'s current
    /// completion proposal, with a fresh verifier agent of the plan and its
    /// journal entry, from `since` on, verifying exactly `accepted`: every
    /// tracked path's accepted identity, which must be what accepted source
    /// holds now; and `inputs`: every other repository entry its verifier
    /// is given, none of them accepted source, `agentctl.toml` among them.
    /// Both are sealed as this commits. The plan must be settled, and
    /// every earlier integration verification of it reconciled, none of
    /// this proposal with a judgment.
    pub(crate) fn begin_integration(
        &mut self,
        plan: PlanId,
        accepted: &BTreeMap<String, Option<String>>,
        inputs: &[(String, Content)],
        since: i64,
    ) -> Result<(IntegrationId, AgentId, JournalId)> {
        ensure!(
            since <= now(),
            "accepted state cannot be read in the future"
        );
        let inputs = normalized(inputs)?;
        self.barrier(plan)?;
        self.write(|tx| {
            let proposal = proposal(tx, plan)?.with_context(|| {
                format!("plan {plan} has no current completion proposal to verify")
            })?;
            if let Some(why) = unsettled(tx, plan)? {
                bail!("plan {plan} is not settled, so it is not verified: {why}");
            }
            ensure!(
                accepted_sources(tx)? == *accepted,
                "accepted source changed while the verifier's view was staged"
            );
            let number: i64 = tx.query_row(
                "SELECT count(*) + 1 FROM integration_verifications WHERE replan_id = ?1",
                [proposal],
                |r| r.get(0),
            )?;
            let basis = basis(tx, plan)?;
            let agent = insert_agent(tx, Role::Verifier, AgentScope::Plan(plan))?;
            let parameters = json!({"plan": plan, "proposal": proposal, "number": number});
            let intent = Intent {
                action: ACTION.into(),
                parameters: parameters.as_object().cloned().unwrap_or_default(),
            };
            let entry = insert_intent(tx, agent, &intent)?;
            // The snapshot is constructed first, under the id the
            // verification is about to take, and inserting the verification
            // seals it: from this transaction's commit on, nothing is added.
            let id = IntegrationId(tx.query_row(
                "SELECT coalesce(max(id), 0) + 1 FROM integration_verifications",
                [],
                |r| r.get(0),
            )?);
            let mut insert = tx.prepare(
                "INSERT INTO integration_sources (verification_id, path, hash) VALUES (?1, ?2, ?3)",
            )?;
            for (path, hash) in accepted {
                insert.execute(params![id, path, hash])?;
            }
            let mut insert = tx.prepare(
                "INSERT INTO integration_inputs (verification_id, path, kind, hash)
                 VALUES (?1, ?2, ?3, ?4)",
            )?;
            for (path, content) in &inputs {
                let (kind, hash) = content.columns();
                insert
                    .execute(params![id, path, kind, hash])
                    .with_context(|| format!("recording repository input `{path}`"))?;
            }
            tx.execute(
                "INSERT INTO integration_verifications
                   (id, plan_id, replan_id, number, basis, agent_id, journal_id, started_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    id,
                    plan,
                    proposal,
                    number,
                    basis.as_str(),
                    agent,
                    entry,
                    since
                ],
            )
            .with_context(|| {
                format!("intending integration verification {number} of plan {plan}")
            })?;
            let detail = format!("integration verification {number} of replan {proposal}");
            event(
                tx,
                "integration.intended",
                Some(plan),
                None,
                Some(agent),
                &detail,
            )?;
            Ok((id, agent, entry))
        })
    }

    /// RECONCILE: records what agentctl observed once the attempted
    /// integration verification's invocation ended, and derives how it
    /// ended: see the module documentation. A pass completes the plan in
    /// the same transaction.
    pub(crate) fn finish_integration(
        &mut self,
        id: IntegrationId,
        observed: &IntegrationObserved,
    ) -> Result<IntegrationOutcome> {
        let mut mutated: Vec<String> = observed.mutated.to_vec();
        mutated.sort_unstable();
        mutated.dedup();
        mutated.truncate(MUTATED_LIMIT);
        for path in &mutated {
            ensure!(path.len() <= PATH_LIMIT, "path too long");
            check_path(path)?;
        }
        if let VerifierResult::Reported(report) = observed.result {
            report.check()?;
        }
        let inputs = observed.inputs.map(normalized).transpose()?;
        self.write(|tx| {
            let (plan, entry, recorded, recorded_basis): (
                PlanId,
                JournalId,
                Option<String>,
                String,
            ) = tx
                .query_row(
                    "SELECT v.plan_id, v.journal_id, r.outcome, v.basis
                     FROM integration_verifications v
                     LEFT JOIN integration_results r ON r.verification_id = v.id
                     WHERE v.id = ?1",
                    [id],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
                )
                .optional()?
                .with_context(|| format!("integration verification {id} does not exist"))?;
            if let Some(outcome) = recorded {
                bail!("integration verification {id} already ended ({outcome})");
            }
            let (state, invocation): (String, Option<InvocationId>) = tx.query_row(
                "SELECT state, invocation_id FROM journal WHERE id = ?1",
                [entry],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            ensure!(
                state == "attempted",
                "integration verification {id} is {state}; only an attempted one ends"
            );
            let invocation = invocation.with_context(|| {
                format!("integration verification {id} was attempted without a verifier")
            })?;
            let ended: InvocationState = tx.query_row(
                "SELECT state FROM invocations WHERE id = ?1",
                [invocation],
                |r| r.get(0),
            )?;
            ensure!(
                ended.is_terminal(),
                "invocation {invocation} has not ended, so integration verification {id} has not"
            );
            ensure!(
                (ended == InvocationState::Succeeded)
                    != matches!(observed.result, VerifierResult::None),
                "a verifier has a result exactly when its invocation succeeded"
            );
            let current: bool = tx.query_row(
                "SELECT EXISTS (SELECT 1 FROM current_integrations WHERE verification_id = ?1)",
                [id],
                |r| r.get(0),
            )?;
            let current = current
                && basis(tx, plan)?.as_str() == recorded_basis
                && inputs.as_ref() == Some(&recorded_inputs(tx, id)?);

            use IntegrationOutcome::*;
            let outcome = match observed.result {
                _ if !current => BasisChanged,
                _ if !mutated.is_empty() => BoundaryViolated,
                VerifierResult::None => InvocationFailed,
                VerifierResult::Malformed => MalformedResult,
                VerifierResult::Reported(report) => match report.verdict {
                    Verdict::Pass => Passed,
                    Verdict::Fail => Failed,
                },
            };
            let report = match observed.result {
                VerifierResult::Reported(report) => Some(report),
                _ => None,
            };
            let inputs = inputs.as_deref();
            record(tx, id, invocation, outcome, &mutated, inputs, report)?;
            if outcome == Passed {
                complete(tx, plan, id)?;
            }
            Ok(outcome)
        })
    }

    /// An integration verification, as recorded.
    pub fn integration(&self, id: IntegrationId) -> Result<Integration> {
        self.conn
            .query_row(
                &format!("{INTEGRATION_COLUMNS} WHERE v.id = ?1"),
                [id],
                integration_row,
            )
            .optional()?
            .with_context(|| format!("integration verification {id} does not exist"))
    }

    /// Every integration verification of `plan`, in order.
    pub fn integrations(&self, plan: PlanId) -> Result<Vec<Integration>> {
        let mut stmt = self.conn.prepare(&format!(
            "{INTEGRATION_COLUMNS} WHERE v.plan_id = ?1 ORDER BY v.id"
        ))?;
        let integrations = stmt.query_map([plan], integration_row)?;
        Ok(integrations.collect::<rusqlite::Result<_>>()?)
    }
}

/// The latest replan of `plan`, if it proposes the plan's completion.
fn proposal(conn: &Connection, plan: PlanId) -> Result<Option<ReplanId>> {
    Ok(conn
        .query_row(
            "SELECT c.replan_id FROM completion_proposals c WHERE c.plan_id = ?1
               AND c.replan_id = (SELECT max(id) FROM replans WHERE plan_id = ?1)",
            [plan],
            |r| r.get(0),
        )
        .optional()?)
}

/// Why `plan` is not settled (see `settled_plans`), if it is not: the first
/// unfinished task found, or else what the view establishes.
pub(super) fn unsettled(conn: &Connection, plan: PlanId) -> Result<Option<String>> {
    let settled: bool = conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM settled_plans WHERE plan_id = ?1)",
        [plan],
        |r| r.get(0),
    )?;
    if settled {
        return Ok(None);
    }
    let state = plan_state(conn, plan)?;
    if state != PlanState::Running {
        return Ok(Some(format!("it is {state}, not running")));
    }
    let tasks: Vec<(super::TaskId, String)> = conn
        .prepare("SELECT id, key FROM tasks WHERE plan_id = ?1 ORDER BY id")?
        .query_map([plan], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    for (task, key) in tasks {
        let status = task_status(conn, task)?;
        if !matches!(status, TaskStatus::Completed | TaskStatus::Cancelled) {
            return Ok(Some(format!(
                "task `{key}` is neither completed nor cancelled ({status:?})"
            )));
        }
    }
    Ok(Some(
        "work of it may still be live or unknown, a concern or instruction awaits, its planner \
         acts, an acceptance of its work is unfinished, or source its work accepted is not \
         synchronized with CodeGraph"
            .into(),
    ))
}

/// Records `plan`'s current replan as proposing its completion, which the
/// plan must be settled for.
pub(super) fn propose(tx: &Transaction, plan: PlanId, replan: ReplanId) -> Result<()> {
    if let Some(why) = unsettled(tx, plan)? {
        bail!("plan {plan} is not settled, so its completion is not proposed: {why}");
    }
    tx.execute(
        "INSERT INTO completion_proposals (replan_id, plan_id, proposed_at) VALUES (?1, ?2, ?3)",
        params![replan, plan, now()],
    )?;
    let detail = format!("replan {replan}");
    event(
        tx,
        "plan.completion_proposed",
        Some(plan),
        None,
        None,
        &detail,
    )
}

/// Repository inputs as recorded: no absent entry, each path once, in
/// order.
fn normalized(inputs: &[(String, Content)]) -> Result<Vec<(String, Content)>> {
    let mut normalized: BTreeMap<&str, &Content> = BTreeMap::new();
    for (path, content) in inputs {
        ensure!(path.len() <= PATH_LIMIT, "path too long");
        check_content(path, content)?;
        if *content == Content::Absent {
            continue;
        }
        let known = normalized.insert(path, content);
        ensure!(
            known.is_none_or(|known| known == content),
            "repository input `{path}` observed twice, differently"
        );
    }
    Ok(normalized
        .into_iter()
        .map(|(path, content)| (path.to_owned(), content.clone()))
        .collect())
}

/// The repository inputs integration verification `id` recorded, in order.
fn recorded_inputs(conn: &Connection, id: IntegrationId) -> Result<Vec<(String, Content)>> {
    Ok(conn
        .prepare(
            "SELECT path, kind, hash FROM integration_inputs WHERE verification_id = ?1
             ORDER BY path",
        )?
        .query_map([id], |r| Ok((r.get(0)?, Content::read(r, 1)?)))?
        .collect::<rusqlite::Result<_>>()?)
}

/// Every tracked path's accepted identity.
fn accepted_sources(conn: &Connection) -> Result<BTreeMap<String, Option<String>>> {
    Ok(conn
        .prepare("SELECT path, hash FROM accepted_sources")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?)
}

/// Records how an attempted integration verification ended, reconciling
/// its journal entry.
pub(super) fn record(
    tx: &Transaction,
    id: IntegrationId,
    invocation: InvocationId,
    outcome: IntegrationOutcome,
    mutated: &[String],
    inputs: Option<&[(String, Content)]>,
    report: Option<&VerifierReport>,
) -> Result<()> {
    let mutated = (!mutated.is_empty())
        .then(|| serde_json::to_string(mutated))
        .transpose()?;
    let inputs = inputs
        .map(|inputs| {
            let listed: Vec<_> = inputs
                .iter()
                .map(|(path, content)| {
                    let (kind, hash) = content.columns();
                    json!({"path": path, "kind": kind, "hash": hash})
                })
                .collect();
            serde_json::to_string(&listed)
        })
        .transpose()?;
    let (verdict, checked, blockers, notes) = match report {
        None => (None, None, None, None),
        Some(r) => (
            Some(match r.verdict {
                Verdict::Pass => "pass",
                Verdict::Fail => "fail",
            }),
            Some(serde_json::to_string(&r.checked)?),
            Some(serde_json::to_string(&r.blockers)?),
            Some(serde_json::to_string(&r.non_blocking)?),
        ),
    };
    tx.execute(
        "INSERT INTO integration_results (verification_id, outcome, mutated, inputs, verdict,
           checked, blockers, non_blocking, finished_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            id,
            outcome,
            mutated,
            inputs,
            verdict,
            checked,
            blockers,
            notes,
            now()
        ],
    )?;
    let (plan, entry): (PlanId, JournalId) = tx.query_row(
        "SELECT plan_id, journal_id FROM integration_verifications WHERE id = ?1",
        [id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let evidence = [
        Evidence::Invocation { invocation },
        Evidence::Fact {
            name: format!("integration.{outcome}"),
        },
    ];
    let action = match outcome {
        IntegrationOutcome::Passed | IntegrationOutcome::Failed => {
            ActionOutcome::CompletedAsIntended
        }
        IntegrationOutcome::BasisChanged | IntegrationOutcome::BoundaryViolated => {
            ActionOutcome::CompletedWithDeviation
        }
        _ => ActionOutcome::Failed,
    };
    reconcile_entry(tx, entry, action, &evidence)?;
    let blockers = report.map_or(0, |r| r.blockers.len());
    let detail = format!("integration verification {id}: {outcome}, {blockers} blockers");
    event(tx, "integration.finished", Some(plan), None, None, &detail)
}

/// Completes `plan` by the pass of integration verification `id`, which
/// the schema lets nothing else do.
fn complete(tx: &Transaction, plan: PlanId, id: IntegrationId) -> Result<()> {
    tx.execute(
        "INSERT INTO plan_completions (plan_id, verification_id, completed_at)
         VALUES (?1, ?2, ?3)",
        params![plan, id, now()],
    )?;
    tx.execute(
        "UPDATE plans SET state = ?2, updated_at = ?3 WHERE id = ?1",
        params![plan, PlanState::Completed, now()],
    )?;
    let detail = format!("{} -> {}", PlanState::Running, PlanState::Completed);
    event(tx, "plan.state", Some(plan), None, None, &detail)
}

const INTEGRATION_COLUMNS: &str = "SELECT v.id, v.plan_id, v.replan_id, v.number, v.agent_id,
       v.journal_id, v.basis, v.started_at, j.state, j.invocation_id, r.outcome, r.mutated,
       r.verdict, r.checked, r.blockers, r.non_blocking, r.finished_at
     FROM integration_verifications v JOIN journal j ON j.id = v.journal_id
     LEFT JOIN integration_results r ON r.verification_id = v.id AND j.state = 'reconciled'";

/// An integration verification from `INTEGRATION_COLUMNS`. A result counts
/// only once its journal entry is reconciled.
fn integration_row(r: &Row) -> rusqlite::Result<Integration> {
    let invocation: Option<InvocationId> = r.get(9)?;
    let status = match (r.get::<_, String>(8)?.as_str(), r.get(10)?) {
        ("intended", _) => IntegrationStatus::Intended,
        (_, None) => IntegrationStatus::OutcomeUnknown { invocation },
        (_, Some(outcome)) => {
            let verdict = match r.get::<_, Option<String>>(12)?.as_deref() {
                None => None,
                Some("pass") => Some(Verdict::Pass),
                Some("fail") => Some(Verdict::Fail),
                Some(other) => {
                    return Err(rusqlite::Error::FromSqlConversionFailure(
                        12,
                        Type::Text,
                        format!("unknown verdict `{other}`").into(),
                    ));
                }
            };
            let report = match verdict {
                None => None,
                Some(verdict) => Some(VerifierReport {
                    verdict,
                    checked: json_column(r, 13)?,
                    blockers: json_column(r, 14)?,
                    non_blocking: json_column(r, 15)?,
                }),
            };
            let mutated = match r.get::<_, Option<String>>(11)? {
                None => Vec::new(),
                Some(_) => json_column(r, 11)?,
            };
            // A reconciled result always follows an invocation.
            let invocation = invocation.ok_or(rusqlite::Error::InvalidColumnType(
                9,
                "invocation_id".into(),
                Type::Null,
            ))?;
            IntegrationStatus::Finished(IntegrationResult {
                outcome,
                invocation,
                mutated,
                report,
                at: r.get(16)?,
            })
        }
    };
    Ok(Integration {
        id: r.get(0)?,
        plan: r.get(1)?,
        proposal: r.get(2)?,
        number: r.get(3)?,
        agent: r.get(4)?,
        journal: r.get(5)?,
        basis: r.get(6)?,
        started_at: r.get(7)?,
        status,
    })
}

#[cfg(test)]
mod tests {}
