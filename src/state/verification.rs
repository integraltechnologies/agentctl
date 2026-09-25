//! Independent verification: a fresh verifier's judgment of the exact
//! candidate an execution captured and installed into the working tree.
//!
//! A verification is intended, with a verifier agent created for it alone
//! and that agent's one journal entry, only while its generation is active,
//! owns its task's whole scope and has an installed candidate that the
//! working tree was just observed to hold at every changed path. Should the
//! working tree no longer hold it, the verification is declined instead:
//! attempted without any invocation and reconciled at once, having verified
//! nothing. Otherwise its entry is attempted, before any verifier process
//! exists, by the invocation embodying the verifier, and only recording how
//! the verification ended reconciles it. Until then nothing is established,
//! however the invocation ended.
//!
//! How it ended is derived here from what agentctl observed once the
//! invocation ended: whether the working tree still held the candidate,
//! and whether the verifier changed repository source in its workspace.
//! Only then does what the verifier reported decide it. Its report is kept
//! as its claim, apart from those observations. A candidate is verified
//! again, by another fresh verifier, only once every earlier verification
//! of it ended without a judgment.
//!
//! Nothing here accepts source, touches CodeGraph, ends the generation or
//! releases ownership: a pass is evidence for acceptance, not acceptance.

use std::collections::{BTreeSet, HashSet};

use anyhow::{Context, Result, bail, ensure};
use rusqlite::types::Type;
use rusqlite::{Connection, OptionalExtension, Row, Transaction, params};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::json;

use super::execution::Content;
use super::ownership::owner;
use super::{
    ActionOutcome, AgentId, AgentScope, Evidence, ExecutionId, GenerationId, GenerationState,
    Intent, InvocationId, InvocationState, JournalId, Role, Store, TaskId, VerificationId,
    VerificationOutcome, act_entry, check_path, check_text, event, generation_info, insert_agent,
    insert_intent, json_column, now, reconcile_entry,
};

/// The journal action of a verification.
const ACTION: &str = "verifier.run";

/// Bounds on a verifier's report.
pub(crate) const CHECKED_LIMIT: usize = 64;
pub(crate) const BLOCKERS_LIMIT: usize = 64;
pub(crate) const NOTES_LIMIT: usize = 32;
pub(crate) const ITEM_PATHS_LIMIT: usize = 16;
pub(crate) const PATH_LIMIT: usize = 4096;
const ID_LIMIT: usize = 64;
const LABEL_LIMIT: usize = 256;
const COMMAND_LIMIT: usize = 1024;
const SUMMARY_LIMIT: usize = 1024;
const EVIDENCE_LIMIT: usize = 2048;
pub(crate) const REPORT_LIMIT: usize = 64 * 1024;
/// At most this many mutated workspace paths are recorded, then the first
/// ones in order.
const MUTATED_LIMIT: usize = 256;

/// What a verifier concluded of a candidate: a claim, never proof.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Pass,
    Fail,
}

impl Verdict {
    fn as_str(self) -> &'static str {
        match self {
            Self::Pass => "pass",
            Self::Fail => "fail",
        }
    }
}

/// How one check a verifier made came out, as it reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckOutcome {
    Passed,
    Failed,
    Inconclusive,
}

/// One check a verifier reports having made: what it checked, the command
/// it ran if any, how it came out and concise evidence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Check {
    pub check: String,
    #[serde(deserialize_with = "present")]
    pub command: Option<String>,
    pub outcome: CheckOutcome,
    pub evidence: String,
}

/// A blocking defect a verifier reports: a stable id within its report, a
/// concise summary, the literal paths involved, the evidence found and,
/// optionally, where.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Blocker {
    pub id: String,
    pub summary: String,
    pub paths: Vec<String>,
    pub evidence: String,
    #[serde(deserialize_with = "present")]
    pub location: Option<String>,
}

/// An observation a verifier reports that blocks nothing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Note {
    pub summary: String,
    pub paths: Vec<String>,
}

/// A verifier's structured report: its verdict, what it checked, every
/// blocker it found and anything else worth noting. Provider-controlled
/// claims, bounded and checked for consistency, never agentctl's own
/// observations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VerifierReport {
    pub verdict: Verdict,
    pub checked: Vec<Check>,
    pub blockers: Vec<Blocker>,
    pub non_blocking: Vec<Note>,
}

/// A nullable field that must nonetheless be present.
fn present<'de, D: Deserializer<'de>, T: Deserialize<'de>>(d: D) -> Result<Option<T>, D::Error> {
    Option::deserialize(d)
}

impl VerifierReport {
    /// Checks the report keeps to the protocol: within its bounds, with
    /// meaningful text and literal canonical paths, and consistent. A pass
    /// needs at least one check that passed and no blocker; a failure at
    /// least one blocker.
    pub fn check(&self) -> Result<()> {
        ensure!(self.checked.len() <= CHECKED_LIMIT, "too many checks");
        ensure!(self.blockers.len() <= BLOCKERS_LIMIT, "too many blockers");
        ensure!(self.non_blocking.len() <= NOTES_LIMIT, "too many notes");
        for check in &self.checked {
            check_text("a check", &check.check, LABEL_LIMIT, true)?;
            if let Some(command) = &check.command {
                check_line("a command", command, COMMAND_LIMIT)?;
            }
            check_text("check evidence", &check.evidence, EVIDENCE_LIMIT, true)?;
        }
        let mut ids = HashSet::new();
        for blocker in &self.blockers {
            let valid = blocker.id.len() <= ID_LIMIT
                && blocker.id.starts_with(|c: char| c.is_ascii_alphanumeric())
                && blocker
                    .id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'));
            ensure!(
                valid,
                "blocker id `{:.64}` is not an identifier",
                blocker.id
            );
            ensure!(ids.insert(&blocker.id), "blocker id repeated");
            check_text("a blocker summary", &blocker.summary, SUMMARY_LIMIT, true)?;
            check_text("blocker evidence", &blocker.evidence, EVIDENCE_LIMIT, true)?;
            if let Some(location) = &blocker.location {
                check_line("a location", location, LABEL_LIMIT)?;
            }
            check_paths(&blocker.paths)?;
        }
        for note in &self.non_blocking {
            check_text("a note", &note.summary, SUMMARY_LIMIT, true)?;
            check_paths(&note.paths)?;
        }
        match self.verdict {
            Verdict::Pass => {
                ensure!(self.blockers.is_empty(), "a pass reports no blocker");
                ensure!(
                    self.checked
                        .iter()
                        .any(|c| c.outcome == CheckOutcome::Passed),
                    "a pass needs checked evidence: at least one check that passed"
                );
            }
            Verdict::Fail => ensure!(!self.blockers.is_empty(), "a failure needs a blocker"),
        }
        ensure!(
            serde_json::to_string(self)?.len() <= REPORT_LIMIT,
            "the report exceeds {REPORT_LIMIT} bytes"
        );
        Ok(())
    }
}

/// Accepts one non-blank line of at most `limit` bytes.
fn check_line(what: &str, text: &str, limit: usize) -> Result<()> {
    check_text(what, text, limit, true)?;
    ensure!(!text.contains('\n'), "{what} must be one line");
    Ok(())
}

fn check_paths(paths: &[String]) -> Result<()> {
    ensure!(paths.len() <= ITEM_PATHS_LIMIT, "too many paths");
    let mut seen = HashSet::new();
    for path in paths {
        ensure!(path.len() <= PATH_LIMIT, "path too long");
        check_path(path)?;
        ensure!(seen.insert(path), "path repeated");
    }
    Ok(())
}

/// One verification of an installed candidate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verification {
    pub id: VerificationId,
    /// The execution whose installed candidate is verified: its changes
    /// identify the candidate's exact bytes.
    pub execution: ExecutionId,
    pub generation: GenerationId,
    /// Which verification of the candidate this is, from 1.
    pub number: i64,
    /// The verifier: a logical agent serving only this verification, never
    /// the executor.
    pub agent: AgentId,
    /// The journal entry of its action.
    pub journal: JournalId,
    /// When agentctl began observing the working tree for it.
    pub started_at: i64,
    pub status: VerificationStatus,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerificationStatus {
    /// Intended, and never attempted: no verifier ran.
    Intended,
    /// Attempted: a verifier may have run, and nothing about how the
    /// verification ended is established, whatever the invocation's own
    /// record says.
    OutcomeUnknown {
        invocation: Option<InvocationId>,
    },
    Finished(VerificationResult),
}

/// How a verification ended, as recorded with its reconciliation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerificationResult {
    pub outcome: VerificationOutcome,
    /// The invocation embodying the verifier; `None` when the candidate had
    /// drifted before any was launched.
    pub invocation: Option<InvocationId>,
    /// Observed by agentctl: the candidate's changed paths the working tree
    /// no longer held, before the verifier ran or once it had ended.
    pub drifted: Vec<String>,
    /// Observed by agentctl: repository source the verifier changed in its
    /// workspace, in order, at most `MUTATED_LIMIT` paths.
    pub mutated: Vec<String>,
    /// The verifier's own report, when well formed: its claims, whatever
    /// the outcome.
    pub report: Option<VerifierReport>,
    pub at: i64,
}

/// What a verifier's invocation yielded, as far as agentctl could read it.
#[derive(Debug, Clone, Copy)]
pub(crate) enum VerifierResult<'a> {
    /// The invocation did not succeed, so it has no result.
    None,
    /// The invocation succeeded with a result breaking the protocol.
    Malformed,
    Reported(&'a VerifierReport),
}

/// What agentctl observed once a verifier's invocation ended.
#[derive(Debug, Clone, Copy)]
pub(crate) struct VerifierObserved<'a> {
    /// The entry at each of the candidate's changed paths in the working
    /// tree, in strict path order.
    pub project: &'a [(String, Content)],
    /// Repository source paths whose entry in the verifier's workspace
    /// differed from what was staged there.
    pub mutated: &'a [String],
    pub result: VerifierResult<'a>,
}

/// An installed candidate that may be verified now.
struct Verifiable {
    execution: ExecutionId,
    /// Each changed path and the content the candidate put there, in order.
    candidate: Vec<(String, Content)>,
    number: i64,
}

impl Store {
    /// The changed paths of the installed candidate of `generation` and
    /// what the candidate put at each, in order, if it may be verified now:
    /// see [`Store::begin_verification`].
    pub(crate) fn verification_candidate(
        &self,
        task: TaskId,
        generation: GenerationId,
    ) -> Result<Vec<(String, Content)>> {
        Ok(verifiable(&self.conn, task, generation)?.candidate)
    }

    /// INTEND: records a verification of the installed candidate of an
    /// active generation of `task`, with a fresh verifier agent and its
    /// journal entry, from `since` on, once the working tree was `observed`
    /// to hold the candidate at every changed path. The generation must own
    /// its task's whole scope, the execution's authority included, and
    /// every earlier verification of the candidate must have ended without
    /// a judgment.
    pub(crate) fn begin_verification(
        &mut self,
        task: TaskId,
        generation: GenerationId,
        observed: &[(String, Content)],
        since: i64,
    ) -> Result<(VerificationId, AgentId, JournalId)> {
        ensure!(
            since <= now(),
            "the working tree cannot be observed in the future"
        );
        self.write(|tx| {
            let verifiable = verifiable(tx, task, generation)?;
            let drifted = drifted(&verifiable.candidate, observed)?;
            if let Some(path) = drifted.first() {
                bail!("the working tree no longer holds the candidate at `{path}`");
            }
            intend(tx, generation, &verifiable, since)
        })
    }

    /// INTEND, ACT and RECONCILE at once: records that the working tree,
    /// `observed` at the changed paths of the installed candidate of
    /// `generation`, no longer held it, so that no verifier ran. The same
    /// conditions as [`Store::begin_verification`] hold otherwise.
    pub(crate) fn decline_verification(
        &mut self,
        task: TaskId,
        generation: GenerationId,
        observed: &[(String, Content)],
        since: i64,
    ) -> Result<VerificationId> {
        ensure!(
            since <= now(),
            "the working tree cannot be observed in the future"
        );
        self.write(|tx| {
            let verifiable = verifiable(tx, task, generation)?;
            let drifted = drifted(&verifiable.candidate, observed)?;
            ensure!(
                !drifted.is_empty(),
                "the working tree holds the candidate; nothing is declined"
            );
            let (verification, _, entry) = intend(tx, generation, &verifiable, since)?;
            act_entry(tx, entry, None)?;
            record(
                tx,
                verification,
                VerificationOutcome::CandidateDrifted,
                &drifted,
                &[],
                None,
            )?;
            Ok(verification)
        })
    }

    /// RECONCILE: records what agentctl observed once the attempted
    /// verification's invocation ended, and derives how it ended. The
    /// candidate changed if the working tree no longer holds it at some
    /// changed path; otherwise the verifier violated its boundary if it
    /// changed any repository source in its workspace. Only then does what
    /// the invocation yielded decide it.
    pub(crate) fn finish_verification(
        &mut self,
        verification: VerificationId,
        observed: &VerifierObserved,
    ) -> Result<VerificationOutcome> {
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
        self.write(|tx| {
            let (execution, entry, recorded): (ExecutionId, JournalId, Option<String>) = tx
                .query_row(
                    "SELECT v.execution_id, v.journal_id, r.outcome FROM verifications v
                     LEFT JOIN verification_results r ON r.verification_id = v.id
                     WHERE v.id = ?1",
                    [verification],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .optional()?
                .with_context(|| format!("verification {verification} does not exist"))?;
            if let Some(outcome) = recorded {
                bail!("verification {verification} already ended ({outcome})");
            }
            let (state, invocation): (String, Option<InvocationId>) = tx.query_row(
                "SELECT state, invocation_id FROM journal WHERE id = ?1",
                [entry],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            ensure!(
                state == "attempted",
                "verification {verification} is {state}; only an attempted one ends"
            );
            let invocation = invocation.with_context(|| {
                format!("verification {verification} was attempted without a verifier")
            })?;
            let ended: InvocationState = tx.query_row(
                "SELECT state FROM invocations WHERE id = ?1",
                [invocation],
                |r| r.get(0),
            )?;
            ensure!(
                ended.is_terminal(),
                "invocation {invocation} has not ended, so verification {verification} has not"
            );
            ensure!(
                (ended == InvocationState::Succeeded)
                    != matches!(observed.result, VerifierResult::None),
                "a verifier has a result exactly when its invocation succeeded"
            );
            let drifted = drifted(&candidate(tx, execution)?, observed.project)?;

            use VerificationOutcome::*;
            let outcome = match observed.result {
                _ if !drifted.is_empty() => CandidateChanged,
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
            record(tx, verification, outcome, &drifted, &mutated, report)?;
            Ok(outcome)
        })
    }

    /// A verification, as recorded.
    pub fn verification(&self, id: VerificationId) -> Result<Verification> {
        self.conn
            .query_row(
                &format!("{VERIFICATION_COLUMNS} WHERE v.id = ?1"),
                [id],
                verification_row,
            )
            .optional()?
            .with_context(|| format!("verification {id} does not exist"))
    }

    /// Every verification of the candidate of `generation`, in order.
    pub fn verifications(&self, generation: GenerationId) -> Result<Vec<Verification>> {
        let mut stmt = self.conn.prepare(&format!(
            "{VERIFICATION_COLUMNS} WHERE e.generation_id = ?1 ORDER BY v.number"
        ))?;
        let verifications = stmt.query_map([generation], verification_row)?;
        Ok(verifications.collect::<rusqlite::Result<_>>()?)
    }
}

/// The installed candidate of `generation`, of `task`, if it may be
/// verified now: the generation active and owning its task's whole scope,
/// the execution's authority included, its candidate captured and
/// installed, and every earlier verification of it ended without a
/// judgment.
fn verifiable(conn: &Connection, task: TaskId, generation: GenerationId) -> Result<Verifiable> {
    let (_, of, _, state) = generation_info(conn, generation)?;
    ensure!(
        of == task,
        "generation {generation} belongs to task {of}, not task {task}"
    );
    ensure!(
        state == GenerationState::Active,
        "generation {generation} has already ended ({state})"
    );
    let (execution, authority, installed): (ExecutionId, String, Option<String>) = conn
        .query_row(
            "SELECT e.id, e.authority, r.outcome FROM executions e
             LEFT JOIN execution_installs i ON i.execution_id = e.id
             LEFT JOIN journal j ON j.id = i.journal_id
             LEFT JOIN execution_install_results r
               ON r.execution_id = e.id AND j.state = 'reconciled'
             WHERE e.generation_id = ?1",
            [generation],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?
        .with_context(|| format!("generation {generation} has no execution"))?;
    ensure!(
        installed.as_deref() == Some("installed"),
        "generation {generation} has no installed candidate to verify"
    );
    let mut scope: BTreeSet<String> = serde_json::from_str(&authority)?;
    let current = conn
        .prepare("SELECT path FROM task_scope WHERE task_id = ?1")?
        .query_map([task], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    scope.extend(current);
    for path in &scope {
        ensure!(
            owner(conn, path)?.is_some_and(|o| o.generation == generation),
            "generation {generation} no longer owns `{path}`"
        );
    }
    let mut number = 1;
    let mut stmt = conn.prepare(
        "SELECT v.number, j.state, r.outcome FROM verifications v
         JOIN journal j ON j.id = v.journal_id
         LEFT JOIN verification_results r ON r.verification_id = v.id
         WHERE v.execution_id = ?1 ORDER BY v.number",
    )?;
    let earlier = stmt.query_map([execution], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, Option<VerificationOutcome>>(2)?,
        ))
    })?;
    for row in earlier {
        let (n, state, outcome) = row?;
        ensure!(
            state == "reconciled",
            "verification {n} of the candidate is {state}, not yet reconciled"
        );
        if let Some(judged @ (VerificationOutcome::Passed | VerificationOutcome::Failed)) = outcome
        {
            bail!("the candidate was already verified ({judged})");
        }
        number = n + 1;
    }
    Ok(Verifiable {
        execution,
        candidate: candidate(conn, execution)?,
        number,
    })
}

/// Each changed path of `execution` and what its candidate put there, in
/// order.
fn candidate(conn: &Connection, execution: ExecutionId) -> Result<Vec<(String, Content)>> {
    let mut stmt = conn.prepare(
        "SELECT path, after_kind, after_hash FROM execution_changes
         WHERE execution_id = ?1 ORDER BY path",
    )?;
    let changes = stmt.query_map([execution], |r| Ok((r.get(0)?, Content::read(r, 1)?)))?;
    Ok(changes.collect::<rusqlite::Result<_>>()?)
}

/// The paths of `candidate` at which `observed`, which must be the entries
/// at exactly its paths in order, differs from it.
fn drifted(candidate: &[(String, Content)], observed: &[(String, Content)]) -> Result<Vec<String>> {
    ensure!(
        candidate.len() == observed.len()
            && candidate.iter().zip(observed).all(|(c, o)| c.0 == o.0),
        "the observation must cover exactly the candidate's changed paths, in order"
    );
    Ok(candidate
        .iter()
        .zip(observed)
        .filter(|(c, o)| c.1 != o.1)
        .map(|(c, _)| c.0.clone())
        .collect())
}

/// Creates the verifier and its journal entry, and records the verification.
fn intend(
    tx: &Transaction,
    generation: GenerationId,
    verifiable: &Verifiable,
    since: i64,
) -> Result<(VerificationId, AgentId, JournalId)> {
    let agent = insert_agent(tx, Role::Verifier, AgentScope::Generation(generation))?;
    let parameters = json!({
        "generation": generation,
        "execution": verifiable.execution,
        "number": verifiable.number,
    });
    let intent = Intent {
        action: ACTION.into(),
        parameters: parameters.as_object().cloned().unwrap_or_default(),
    };
    let entry = insert_intent(tx, agent, &intent)?;
    tx.execute(
        "INSERT INTO verifications (execution_id, number, agent_id, journal_id, started_at)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![verifiable.execution, verifiable.number, agent, entry, since],
    )?;
    Ok((VerificationId(tx.last_insert_rowid()), agent, entry))
}

/// Records how an attempted verification ended, reconciling its journal
/// entry.
fn record(
    tx: &Transaction,
    verification: VerificationId,
    outcome: VerificationOutcome,
    drifted: &[String],
    mutated: &[String],
    report: Option<&VerifierReport>,
) -> Result<()> {
    let list = |paths: &[String]| -> Result<Option<String>> {
        Ok((!paths.is_empty())
            .then(|| serde_json::to_string(paths))
            .transpose()?)
    };
    let (verdict, checked, blockers, notes) = match report {
        None => (None, None, None, None),
        Some(r) => (
            Some(r.verdict.as_str()),
            Some(serde_json::to_string(&r.checked)?),
            Some(serde_json::to_string(&r.blockers)?),
            Some(serde_json::to_string(&r.non_blocking)?),
        ),
    };
    tx.execute(
        "INSERT INTO verification_results (verification_id, outcome, drifted, mutated, verdict,
           checked, blockers, non_blocking, finished_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            verification,
            outcome,
            list(drifted)?,
            list(mutated)?,
            verdict,
            checked,
            blockers,
            notes,
            now()
        ],
    )?;
    let (entry, invocation, generation, number): (
        JournalId,
        Option<InvocationId>,
        GenerationId,
        i64,
    ) = tx.query_row(
        "SELECT v.journal_id, j.invocation_id, e.generation_id, v.number FROM verifications v
         JOIN journal j ON j.id = v.journal_id JOIN executions e ON e.id = v.execution_id
         WHERE v.id = ?1",
        [verification],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
    )?;
    let mut evidence: Vec<Evidence> = invocation
        .map(|invocation| Evidence::Invocation { invocation })
        .into_iter()
        .collect();
    evidence.push(Evidence::Fact {
        name: format!("verification.{outcome}"),
    });
    let action = match outcome {
        VerificationOutcome::Passed | VerificationOutcome::Failed => {
            ActionOutcome::CompletedAsIntended
        }
        VerificationOutcome::CandidateChanged | VerificationOutcome::BoundaryViolated => {
            ActionOutcome::CompletedWithDeviation
        }
        _ => ActionOutcome::Failed,
    };
    reconcile_entry(tx, entry, action, &evidence)?;
    let (plan, task, generation_number, _) = generation_info(tx, generation)?;
    let blockers = report.map_or(0, |r| r.blockers.len());
    let detail = format!(
        "generation {generation_number} verification {number}: {outcome}, {blockers} blockers"
    );
    event(
        tx,
        "verification.finished",
        Some(plan),
        Some(task),
        None,
        &detail,
    )
}

const VERIFICATION_COLUMNS: &str = "SELECT v.id, v.execution_id, e.generation_id, v.number,
       v.agent_id, v.journal_id, v.started_at, j.state, j.invocation_id, r.outcome, r.drifted,
       r.mutated, r.verdict, r.checked, r.blockers, r.non_blocking, r.finished_at
     FROM verifications v JOIN executions e ON e.id = v.execution_id
     JOIN journal j ON j.id = v.journal_id
     LEFT JOIN verification_results r ON r.verification_id = v.id AND j.state = 'reconciled'";

/// A verification from `VERIFICATION_COLUMNS`. A result counts only once
/// its journal entry is reconciled; until then how it ended is unknown.
fn verification_row(r: &Row) -> rusqlite::Result<Verification> {
    let invocation: Option<InvocationId> = r.get(8)?;
    let paths = |i: usize| -> rusqlite::Result<Vec<String>> {
        Ok(r.get::<_, Option<String>>(i)?
            .map(|_| json_column(r, i))
            .transpose()?
            .unwrap_or_default())
    };
    let status = match (r.get::<_, String>(7)?.as_str(), r.get(9)?) {
        ("intended", _) => VerificationStatus::Intended,
        (_, None) => VerificationStatus::OutcomeUnknown { invocation },
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
            VerificationStatus::Finished(VerificationResult {
                outcome,
                invocation,
                drifted: paths(10)?,
                mutated: paths(11)?,
                report,
                at: r.get(16)?,
            })
        }
    };
    Ok(Verification {
        id: r.get(0)?,
        execution: r.get(1)?,
        generation: r.get(2)?,
        number: r.get(3)?,
        agent: r.get(4)?,
        journal: r.get(5)?,
        started_at: r.get(6)?,
        status,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::tests::{acquire, ended, err, ready_plan, store, succeeded};
    use crate::state::{
        ActionStatus, ExecutionStatus, ExecutorResult, FailureKind, GenerationEnd, InstallOutcome,
        InvocationEnd, Observed, Reported,
    };
    use std::thread;
    use std::time::Duration;

    const HEAD: &str = "refs/heads/main unborn";

    fn file(n: u8) -> Content {
        Content::File(format!("{n:064x}"))
    }

    fn since() -> i64 {
        thread::sleep(Duration::from_millis(2));
        now()
    }

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    /// A generation of a new task owning `scope`, whose executor changed
    /// every path of it from `file(1)` to `file(2)`: captured as a
    /// candidate, and installed unless `install` says otherwise.
    struct Candidate {
        task: TaskId,
        generation: GenerationId,
        execution: ExecutionId,
        executor: AgentId,
        executor_invocation: InvocationId,
        /// What the candidate put at each changed path, in order.
        entries: Vec<(String, Content)>,
    }

    fn candidate(store: &mut Store, scope: &[&str], install: Option<InstallOutcome>) -> Candidate {
        let (_, tasks) = ready_plan(store, &[("t", scope, &[])]);
        let generation = store.start_generation(tasks[0]).unwrap();
        acquire(store, generation, scope);
        let mut authority = strings(scope);
        authority.sort();
        let entries = |n: u8| -> Vec<(String, Content)> {
            authority.iter().map(|p| (p.clone(), file(n))).collect()
        };
        let (execution, executor, entry) = store
            .begin_execution(tasks[0], generation, &authority, &entries(1), HEAD, since())
            .unwrap();
        let invocation = store
            .start_invocation(executor, "claude", "m", None)
            .unwrap();
        store.act(entry, Some(invocation)).unwrap();
        store.invocation_running(invocation).unwrap();
        store.finish_invocation(invocation, &succeeded()).unwrap();
        let after = entries(2);
        let observed = Observed {
            entries: &after,
            head: HEAD,
            settled: true,
            result: ExecutorResult::Reported {
                status: Reported::Succeeded,
                claimed: &authority,
            },
        };
        store.finish_execution(execution, &observed).unwrap();
        if let Some(outcome) = install {
            let entry = store.begin_install(execution).unwrap();
            store.act(entry, None).unwrap();
            let drifted = match outcome {
                InstallOutcome::Drifted => authority.clone(),
                _ => Vec::new(),
            };
            store.finish_install(execution, outcome, &drifted).unwrap();
        }
        Candidate {
            task: tasks[0],
            generation,
            execution,
            executor,
            executor_invocation: invocation,
            entries: after,
        }
    }

    fn installed(store: &mut Store, scope: &[&str]) -> Candidate {
        candidate(store, scope, Some(InstallOutcome::Installed))
    }

    /// A verification of `c`, attempted by a running invocation.
    fn attempted(store: &mut Store, c: &Candidate) -> (VerificationId, AgentId, InvocationId) {
        let (verification, agent, entry) = store
            .begin_verification(c.task, c.generation, &c.entries, since())
            .unwrap();
        let invocation = store.start_invocation(agent, "claude", "m", None).unwrap();
        store.act(entry, Some(invocation)).unwrap();
        store.invocation_running(invocation).unwrap();
        (verification, agent, invocation)
    }

    fn passed_check() -> Check {
        Check {
            check: "unit tests".into(),
            command: Some("cargo test".into()),
            outcome: CheckOutcome::Passed,
            evidence: "42 passed".into(),
        }
    }

    fn blocker(id: &str, path: &str) -> Blocker {
        Blocker {
            id: id.into(),
            summary: format!("{id} is broken"),
            paths: vec![path.into()],
            evidence: "the test fails".into(),
            location: None,
        }
    }

    fn pass() -> VerifierReport {
        VerifierReport {
            verdict: Verdict::Pass,
            checked: vec![passed_check()],
            blockers: Vec::new(),
            non_blocking: vec![Note {
                summary: "naming could be clearer".into(),
                paths: Vec::new(),
            }],
        }
    }

    fn fail(blockers: Vec<Blocker>) -> VerifierReport {
        VerifierReport {
            verdict: Verdict::Fail,
            blockers,
            ..pass()
        }
    }

    fn observed<'a>(
        project: &'a [(String, Content)],
        mutated: &'a [String],
        result: VerifierResult<'a>,
    ) -> VerifierObserved<'a> {
        VerifierObserved {
            project,
            mutated,
            result,
        }
    }

    fn finished(store: &Store, verification: VerificationId) -> VerificationResult {
        match store.verification(verification).unwrap().status {
            VerificationStatus::Finished(result) => result,
            status => panic!("{status:?}"),
        }
    }

    fn cancelled() -> InvocationEnd {
        ended(InvocationState::Cancelled)
    }

    fn failed() -> InvocationEnd {
        InvocationEnd {
            failure: Some(FailureKind::ExitStatus),
            diagnostic: Some("failed".into()),
            exit_code: Some(1),
            ..ended(InvocationState::Failed)
        }
    }

    #[test]
    fn only_installed_candidates_of_owning_generations_are_verified() {
        let (_dir, mut store) = store();
        let begin = |store: &mut Store, c: &Candidate| {
            store.begin_verification(c.task, c.generation, &c.entries, since())
        };
        let agents = |store: &Store| {
            let events = store.events_after(0, 10_000).unwrap();
            events.iter().filter(|e| e.kind == "agent.created").count()
        };

        // A generation without an execution.
        let (_, tasks) = ready_plan(&mut store, &[("idle", &["src/idle.rs"], &[])]);
        let idle = store.start_generation(tasks[0]).unwrap();
        let refused = store.begin_verification(tasks[0], idle, &[], since());
        assert!(err(refused).contains("has no execution"));

        // Captured, but never installed, or not installed.
        let before = agents(&store);
        let uninstalled = candidate(&mut store, &["src/a.rs"], None);
        let drifted = candidate(&mut store, &["src/b.rs"], Some(InstallOutcome::Drifted));
        let refused_install = candidate(&mut store, &["src/c.rs"], Some(InstallOutcome::Refused));
        for c in [&uninstalled, &drifted, &refused_install] {
            assert!(err(begin(&mut store, c)).contains("no installed candidate"));
            let declined = store.decline_verification(c.task, c.generation, &[], since());
            assert!(err(declined).contains("no installed candidate"));
        }
        // Installed, but of another task, or of an ended generation.
        let other = installed(&mut store, &["src/d.rs"]);
        let c = installed(&mut store, &["src/e.rs"]);
        let refused = store.begin_verification(other.task, c.generation, &c.entries, since());
        assert!(err(refused).contains("belongs to task"));
        let executors = agents(&store);
        assert_eq!(executors, before + 5, "one executor each, nothing else");

        // Observed not to hold the candidate, or observed elsewhere.
        let mut stale = c.entries.clone();
        stale[0].1 = file(7);
        let refused = store.begin_verification(c.task, c.generation, &stale, since());
        assert!(err(refused).contains("no longer holds the candidate at `src/e.rs`"));
        let elsewhere = vec![("src/f.rs".to_string(), file(2))];
        let refused = store.begin_verification(c.task, c.generation, &elsewhere, since());
        assert!(err(refused).contains("exactly the candidate's changed paths"));
        let refused = store.decline_verification(c.task, c.generation, &c.entries, since());
        assert!(err(refused).contains("nothing is declined"));
        assert_eq!(agents(&store), executors, "no verifier was created");
        assert!(store.verifications(c.generation).unwrap().is_empty());

        store
            .finish_generation(c.generation, GenerationEnd::Failed)
            .unwrap();
        assert!(err(begin(&mut store, &c)).contains("already ended"));

        // A fresh verifier, never the executor, verifies the one candidate.
        let (verification, agent, entry) = begin(&mut store, &other).unwrap();
        assert_ne!(agent, other.executor);
        let recorded = store.verification(verification).unwrap();
        assert_eq!(
            (recorded.execution, recorded.generation, recorded.number),
            (other.execution, other.generation, 1)
        );
        assert_eq!((recorded.agent, recorded.journal), (agent, entry));
        assert_eq!(recorded.status, VerificationStatus::Intended);
        let role: Role = store
            .conn
            .query_row("SELECT role FROM agents WHERE id = ?1", [agent], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(role, Role::Verifier);
    }

    #[test]
    fn outcomes_follow_observation_before_the_verdict() {
        use VerificationOutcome::*;
        let (dir, mut store) = store();
        let two = fail(vec![
            blocker("b1", "src/x.rs"),
            blocker("b2", "src/[id].rs"),
        ]);
        let passing = pass();
        let none: &[String] = &[];
        let touched = strings(&["src/x.rs"]);
        let cases: Vec<(
            InvocationEnd,
            bool,
            &[String],
            VerifierResult,
            VerificationOutcome,
        )> = vec![
            (
                succeeded(),
                true,
                none,
                VerifierResult::Reported(&passing),
                Passed,
            ),
            (
                succeeded(),
                true,
                none,
                VerifierResult::Reported(&two),
                Failed,
            ),
            (
                succeeded(),
                true,
                none,
                VerifierResult::Malformed,
                MalformedResult,
            ),
            (failed(), true, none, VerifierResult::None, InvocationFailed),
            (
                cancelled(),
                true,
                none,
                VerifierResult::None,
                InvocationFailed,
            ),
            // What agentctl observed outranks what the verifier concluded.
            (
                succeeded(),
                true,
                &touched,
                VerifierResult::Reported(&passing),
                BoundaryViolated,
            ),
            (
                failed(),
                true,
                &touched,
                VerifierResult::None,
                BoundaryViolated,
            ),
            (
                succeeded(),
                false,
                &touched,
                VerifierResult::Reported(&passing),
                CandidateChanged,
            ),
            (
                cancelled(),
                false,
                none,
                VerifierResult::None,
                CandidateChanged,
            ),
        ];
        let mut recorded = Vec::new();
        for (n, (end, held, mutated, result, expected)) in cases.into_iter().enumerate() {
            let path = format!("src/case{n}.rs");
            let c = installed(&mut store, &[&path]);
            let (verification, agent, invocation) = attempted(&mut store, &c);
            store.finish_invocation(invocation, &end).unwrap();
            let mut project = c.entries.clone();
            if !held {
                project[0].1 = Content::Absent;
            }
            let outcome = store
                .finish_verification(verification, &observed(&project, mutated, result))
                .unwrap();
            assert_eq!(outcome, expected, "case {n}");
            let result_ = finished(&store, verification);
            assert_eq!(result_.outcome, expected);
            assert_eq!(result_.invocation, Some(invocation));
            assert_eq!(result_.mutated, mutated, "case {n}");
            assert_eq!(
                result_.drifted,
                if held { vec![] } else { vec![path.clone()] }
            );
            // The verifier's claims are kept, apart from observations.
            let report = match result {
                VerifierResult::Reported(report) => Some(report.clone()),
                _ => None,
            };
            assert_eq!(result_.report, report, "case {n}");
            let entry = store.verification(verification).unwrap().journal;
            let ActionStatus::Reconciled(attempt, reconciliation) =
                store.journal_entry(entry).unwrap().status
            else {
                panic!("reconciled with the result");
            };
            assert_eq!(attempt.invocation, Some(invocation));
            let action = match expected {
                Passed | Failed => ActionOutcome::CompletedAsIntended,
                CandidateChanged | BoundaryViolated => ActionOutcome::CompletedWithDeviation,
                _ => ActionOutcome::Failed,
            };
            assert_eq!(reconciliation.outcome, action);
            assert_eq!(
                reconciliation.evidence,
                [
                    Evidence::Invocation { invocation },
                    Evidence::Fact {
                        name: format!("verification.{expected}")
                    }
                ]
            );
            // Nothing else moved: the generation stays active, owning its
            // scope, and the executor's record is as it was.
            assert_eq!(store.owned_paths(c.generation).unwrap(), [path.as_str()]);
            let (.., state) = generation_info(&store.conn, c.generation).unwrap();
            assert_eq!(state, GenerationState::Active);
            let execution = store.execution(c.generation).unwrap().unwrap();
            assert_ne!(execution.agent, agent);
            assert!(matches!(execution.status, ExecutionStatus::Captured(_)));
            assert_ne!(invocation, c.executor_invocation);
            recorded.push((c.generation, store.verification(verification).unwrap()));
        }
        // All blockers are kept, in order.
        let failed_result = finished(&store, recorded[1].1.id);
        let ids: Vec<&str> = failed_result
            .report
            .as_ref()
            .unwrap()
            .blockers
            .iter()
            .map(|b| b.id.as_str())
            .collect();
        assert_eq!(ids, ["b1", "b2"]);
        assert_eq!(store.accepted_paths().unwrap(), Vec::<String>::new());

        // A fresh store reconstructs every verification, which is history.
        drop(store);
        let store = Store::open(&dir.path().join("state.db")).unwrap();
        for (generation, verification) in &recorded {
            assert_eq!(
                store.verifications(*generation).unwrap(),
                std::slice::from_ref(verification)
            );
        }
        for sql in [
            "UPDATE verification_results SET outcome = 'passed'",
            "UPDATE verification_results SET blockers = '[]'",
            "DELETE FROM verification_results",
            "UPDATE verifications SET number = 2",
            "DELETE FROM verifications",
        ] {
            let message = store.conn.execute(sql, []).unwrap_err().to_string();
            assert!(
                message.contains("verification history is immutable"),
                "{sql}: {message}"
            );
        }
    }

    #[test]
    fn verifications_end_only_once_attempted_and_ended() {
        let (_dir, mut store) = store();
        let c = installed(&mut store, &["src/a.rs"]);
        let report = pass();
        let reported = VerifierResult::Reported(&report);
        let (verification, agent, entry) = store
            .begin_verification(c.task, c.generation, &c.entries, since())
            .unwrap();
        let refused = store.finish_verification(verification, &observed(&c.entries, &[], reported));
        assert!(err(refused).contains("only an attempted one ends"));

        // Attempted: until a result is recorded, nothing is established.
        let invocation = store.start_invocation(agent, "claude", "m", None).unwrap();
        store.act(entry, Some(invocation)).unwrap();
        store.invocation_running(invocation).unwrap();
        let unknown = VerificationStatus::OutcomeUnknown {
            invocation: Some(invocation),
        };
        assert_eq!(store.verification(verification).unwrap().status, unknown);
        let refused = store.finish_verification(verification, &observed(&c.entries, &[], reported));
        assert!(err(refused).contains("has not ended"));
        store.finish_invocation(invocation, &succeeded()).unwrap();
        assert_eq!(store.verification(verification).unwrap().status, unknown);
        let evidence = [Evidence::Invocation { invocation }];
        for outcome in [ActionOutcome::CompletedAsIntended, ActionOutcome::Failed] {
            let reconciled = store.reconcile(entry, outcome, &evidence);
            assert!(err(reconciled).contains("reconciled only by its result"));
        }

        // Only consistent, well-formed evidence is recorded.
        let none = VerifierResult::None;
        let refused = store.finish_verification(verification, &observed(&c.entries, &[], none));
        assert!(err(refused).contains("exactly when its invocation succeeded"));
        let refused = store.finish_verification(verification, &observed(&[], &[], reported));
        assert!(err(refused).contains("exactly the candidate's changed paths"));
        let naked = VerifierReport {
            checked: Vec::new(),
            ..pass()
        };
        let blocked_pass = VerifierReport {
            blockers: vec![blocker("b1", "src/a.rs")],
            ..pass()
        };
        let empty_fail = fail(Vec::new());
        for (report, expected) in [
            (&naked, "checked evidence"),
            (&blocked_pass, "a pass reports no blocker"),
            (&empty_fail, "a failure needs a blocker"),
        ] {
            let result = VerifierResult::Reported(report);
            let refused =
                store.finish_verification(verification, &observed(&c.entries, &[], result));
            assert!(err(refused).contains(expected), "{expected}");
        }
        let outside = strings(&["../outside.rs"]);
        let refused =
            store.finish_verification(verification, &observed(&c.entries, &outside, reported));
        assert!(err(refused).contains("not a canonical"));
        assert_eq!(store.verification(verification).unwrap().status, unknown);

        let outcome = store.finish_verification(verification, &observed(&c.entries, &[], reported));
        assert_eq!(outcome.unwrap(), VerificationOutcome::Passed);
        let refused = store.finish_verification(verification, &observed(&c.entries, &[], reported));
        assert!(err(refused).contains("already ended (passed)"));
    }

    #[test]
    fn candidates_are_verified_again_only_without_a_judgment() {
        let (_dir, mut store) = store();
        let c = installed(&mut store, &["src/a.rs"]);
        let begin =
            |store: &mut Store| store.begin_verification(c.task, c.generation, &c.entries, since());

        // One still open, whether intended or attempted, blocks another.
        let (first, first_agent, entry) = begin(&mut store).unwrap();
        assert!(err(begin(&mut store)).contains("verification 1 of the candidate is intended"));
        let invocation = store
            .start_invocation(first_agent, "claude", "m", None)
            .unwrap();
        store.act(entry, Some(invocation)).unwrap();
        store.invocation_running(invocation).unwrap();
        assert!(err(begin(&mut store)).contains("is attempted, not yet reconciled"));

        // Ending without a judgment lets another fresh verifier try.
        store.finish_invocation(invocation, &failed()).unwrap();
        let none = VerifierResult::None;
        store
            .finish_verification(first, &observed(&c.entries, &[], none))
            .unwrap();
        let (second, second_agent, invocation) = attempted(&mut store, &c);
        assert_ne!(first_agent, second_agent);
        store.finish_invocation(invocation, &succeeded()).unwrap();
        let report = fail(vec![blocker("b1", "src/a.rs")]);
        store
            .finish_verification(
                second,
                &observed(&c.entries, &[], VerifierResult::Reported(&report)),
            )
            .unwrap();
        let numbers: Vec<i64> = store
            .verifications(c.generation)
            .unwrap()
            .iter()
            .map(|v| v.number)
            .collect();
        assert_eq!(numbers, [1, 2]);

        // A judgment is final for the candidate.
        assert!(err(begin(&mut store)).contains("already verified (failed)"));
        let mut drifted = c.entries.clone();
        drifted[0].1 = Content::Absent;
        let declined = store.decline_verification(c.task, c.generation, &drifted, since());
        assert!(err(declined).contains("already verified (failed)"));
    }

    #[test]
    fn drifted_candidates_are_declined_without_a_verifier() {
        let (_dir, mut store) = store();
        let c = installed(&mut store, &["src/a.rs", "src/b.rs"]);
        let mut drifted = c.entries.clone();
        drifted[1].1 = file(9);
        let verification = store
            .decline_verification(c.task, c.generation, &drifted, since())
            .unwrap();
        let recorded = store.verification(verification).unwrap();
        let VerificationStatus::Finished(result) = &recorded.status else {
            panic!("{:?}", recorded.status);
        };
        assert_eq!(result.outcome, VerificationOutcome::CandidateDrifted);
        assert_eq!(result.drifted, ["src/b.rs"]);
        assert_eq!((result.invocation, &result.report), (None, &None));
        assert!(store.invocations(recorded.agent).unwrap().is_empty());
        let ActionStatus::Reconciled(attempt, reconciliation) =
            store.journal_entry(recorded.journal).unwrap().status
        else {
            panic!("reconciled at once");
        };
        assert_eq!(attempt.invocation, None);
        assert_eq!(reconciliation.outcome, ActionOutcome::Failed);

        // Should the working tree hold the candidate again, a fresh
        // verifier may verify it.
        let (again, ..) = attempted(&mut store, &c);
        assert_eq!(store.verification(again).unwrap().number, 2);
    }

    #[test]
    fn raw_sql_cannot_forge_a_verification() {
        let (_dir, mut store) = store();
        let c = installed(&mut store, &["src/a.rs"]);
        let insert = |store: &Store, verification: VerificationId, values: &str| {
            let sql = format!(
                "INSERT INTO verification_results (verification_id, outcome, drifted, mutated,
                   verdict, checked, blockers, non_blocking, finished_at)
                 VALUES (?1, {values}, 1)"
            );
            store
                .conn
                .execute(&sql, [verification])
                .map(drop)
                .map_err(|e| e.to_string())
        };
        // Either of `|`-separated refusals: a trigger runs before CHECKs.
        let refused = |result: Result<(), String>, expected: &str| {
            let message = result.unwrap_err();
            let any = expected.split('|').any(|e| message.contains(e));
            assert!(any, "{expected}: {message}");
        };
        let checked = r#"'[{"check":"t","command":null,"outcome":"passed","evidence":"ok"}]'"#;
        let unchecked = r#"'[{"check":"t","command":null,"outcome":"failed","evidence":"no"}]'"#;
        let blockers = r#"'[{"id":"b1","summary":"s","paths":[],"evidence":"e","location":null}]'"#;
        let pass = format!("'passed', NULL, NULL, 'pass', {checked}, '[]', '[]'");
        let follows = "follows its attempt";

        // Intended, then attempted and running: no result yet.
        let (verification, agent, entry) = store
            .begin_verification(c.task, c.generation, &c.entries, since())
            .unwrap();
        refused(insert(&store, verification, &pass), follows);
        let invocation = store.start_invocation(agent, "claude", "m", None).unwrap();
        store.act(entry, Some(invocation)).unwrap();
        store.invocation_running(invocation).unwrap();
        refused(insert(&store, verification, &pass), follows);
        store.finish_invocation(invocation, &succeeded()).unwrap();

        // Ended: only a result consistent with itself and its attempt.
        for (values, expected) in [
            (
                "'passed', NULL, NULL, 'pass', '[]', '[]', '[]'".into(),
                "CHECK|follows its attempt",
            ),
            (
                format!("'passed', NULL, NULL, 'pass', {checked}, {blockers}, '[]'"),
                "CHECK|follows its attempt",
            ),
            (
                "'failed', NULL, NULL, 'fail', '[]', '[]', '[]'".into(),
                "CHECK|follows its attempt",
            ),
            (
                "'passed', NULL, NULL, NULL, NULL, NULL, NULL".into(),
                "CHECK|follows its attempt",
            ),
            (
                format!("'failed', NULL, NULL, 'pass', {checked}, '[]', '[]'"),
                "CHECK|follows its attempt",
            ),
            (
                format!("'passed', NULL, '[\"src/a.rs\"]', 'pass', {checked}, '[]', '[]'"),
                "CHECK|follows its attempt",
            ),
            (
                format!("'passed', NULL, NULL, 'pass', {unchecked}, '[]', '[]'"),
                follows,
            ),
            (
                "'invocation_failed', NULL, NULL, NULL, NULL, NULL, NULL".into(),
                follows,
            ),
            (
                "'candidate_drifted', '[\"src/a.rs\"]', NULL, NULL, NULL, NULL, NULL".into(),
                follows,
            ),
            (
                "'candidate_changed', '[\"src/zz.rs\"]', NULL, NULL, NULL, NULL, NULL".into(),
                follows,
            ),
        ] {
            refused(insert(&store, verification, &values), expected);
        }
        let evidence = [Evidence::Invocation { invocation }];
        let reconciled = store.reconcile(entry, ActionOutcome::CompletedAsIntended, &evidence);
        assert!(err(reconciled).contains("reconciled only by its result"));
        let sql = "UPDATE journal SET state = 'reconciled', outcome = 'completed_as_intended',
                   evidence = '[{\"kind\":\"fact\",\"name\":\"forged\"}]', reconciled_at = 1
                   WHERE id = ?1";
        let message = store.conn.execute(sql, [entry]).unwrap_err().to_string();
        assert!(
            message.contains("reconciled only by its result"),
            "{message}"
        );

        // A consistent result the journal is not reconciled with
        // establishes nothing, and cannot be replaced.
        insert(&store, verification, &pass).unwrap();
        let unknown = VerificationStatus::OutcomeUnknown {
            invocation: Some(invocation),
        };
        assert_eq!(store.verification(verification).unwrap().status, unknown);
        let replace = format!(
            "INSERT OR REPLACE INTO verification_results VALUES
               (?1, 'failed', NULL, NULL, 'fail', {checked}, {blockers}, '[]', 2)"
        );
        let message = store.conn.execute(&replace, [verification]).unwrap_err();
        assert!(message.to_string().contains(follows), "{message}");
        let deviated = store.reconcile(entry, ActionOutcome::Failed, &evidence);
        assert!(err(deviated).contains("reconciled only by its result"));
        store
            .reconcile(entry, ActionOutcome::CompletedAsIntended, &evidence)
            .unwrap();
        assert_eq!(
            finished(&store, verification).outcome,
            VerificationOutcome::Passed
        );

        // Terminal evidence stays, so no attempt masquerades as the first.
        for sql in [
            "DELETE FROM verification_results",
            "DELETE FROM verifications",
            "UPDATE verification_results SET outcome = 'failed', verdict = 'fail'",
            "UPDATE verifications SET execution_id = 99",
        ] {
            let message = store.conn.execute(sql, []).unwrap_err().to_string();
            assert!(message.contains("history is immutable"), "{sql}: {message}");
        }
        let message = store
            .conn
            .execute("DELETE FROM journal WHERE id = ?1", [entry])
            .unwrap_err();
        assert!(message.to_string().contains("journal history is immutable"));

        // Rows asserting a verification only a fresh verifier of an
        // installed, owned candidate may hold.
        let other = installed(&mut store, &["src/b.rs"]);
        let uninstalled = candidate(&mut store, &["src/c.rs"], None);
        let forge = |store: &mut Store, execution: ExecutionId, agent: AgentId, number: i64| {
            let intent = Intent {
                action: ACTION.into(),
                parameters: Default::default(),
            };
            let entry = store.intend(agent, &intent).unwrap();
            store
                .conn
                .execute(
                    "INSERT INTO verifications (execution_id, number, agent_id, journal_id,
                       started_at) VALUES (?1, ?2, ?3, ?4, 0)",
                    params![execution, number, agent, entry],
                )
                .map(drop)
                .map_err(|e| e.to_string())
        };
        let verifier = |store: &mut Store, generation| {
            store
                .create_agent(Role::Verifier, AgentScope::Generation(generation))
                .unwrap()
        };
        let fresh = "by a fresh verifier";
        // Already judged.
        let v = verifier(&mut store, c.generation);
        refused(forge(&mut store, c.execution, v, 2), fresh);
        // The executor, or a verifier of another generation.
        refused(forge(&mut store, other.execution, other.executor, 1), fresh);
        let stranger = verifier(&mut store, c.generation);
        refused(forge(&mut store, other.execution, stranger, 1), fresh);
        // A number out of sequence, or a candidate never installed.
        let v = verifier(&mut store, other.generation);
        refused(forge(&mut store, other.execution, v, 2), fresh);
        let v = verifier(&mut store, uninstalled.generation);
        let refused_fk = forge(&mut store, uninstalled.execution, v, 1);
        assert!(refused_fk.is_err());
        // A verifier that has already acted elsewhere is not fresh.
        let v = verifier(&mut store, other.generation);
        store.start_invocation(v, "claude", "m", None).unwrap();
        refused(forge(&mut store, other.execution, v, 1), fresh);
        let v = verifier(&mut store, other.generation);
        forge(&mut store, other.execution, v, 1).unwrap();
    }

    #[test]
    fn migrating_version_10_records_no_verification() {
        use crate::state::SCHEMA_VERSION;
        use crate::state::tests::{downgrade_to_v10, rows_besides, version};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("v10.db");
        let mut store = Store::open(&path).unwrap();
        let c = installed(&mut store, &["src/a.rs"]);
        let uninstalled = candidate(&mut store, &["src/b.rs"], None);
        let capture = store.execution(c.generation).unwrap();
        drop(store);
        downgrade_to_v10(&path);
        assert_eq!(version(&path), 10);
        let tables = [
            "executions",
            "execution_changes",
            "execution_captures",
            "execution_installs",
            "execution_install_results",
        ];
        let rows = |path: &std::path::Path| {
            let conn = Connection::open(path).unwrap();
            let executions: Vec<Vec<rusqlite::types::Value>> = tables
                .iter()
                .flat_map(|table| {
                    let mut stmt = conn.prepare(&format!("SELECT * FROM {table}")).unwrap();
                    let columns = stmt.column_count();
                    stmt.query_map([], |r| (0..columns).map(|i| r.get(i)).collect())
                        .unwrap()
                        .collect::<rusqlite::Result<Vec<_>>>()
                        .unwrap()
                })
                .collect();
            (rows_besides(path, &[]), executions)
        };
        let before = rows(&path);

        let store = Store::open(&path).unwrap();
        assert_eq!(version(&path), SCHEMA_VERSION);
        assert_eq!(rows(&path), before, "nothing changed");
        for table in ["verifications", "verification_results"] {
            let sql = format!("SELECT count(*) FROM {table}");
            let count: i64 = store.conn.query_row(&sql, [], |r| r.get(0)).unwrap();
            assert_eq!(count, 0, "{table}");
        }
        assert!(store.verifications(c.generation).unwrap().is_empty());
        assert_eq!(store.execution(c.generation).unwrap(), capture);
        // An installed candidate from before may be verified now, afresh.
        drop(store);
        let mut store = Store::open(&path).unwrap();
        let (verification, ..) = attempted(&mut store, &c);
        assert_eq!(store.verification(verification).unwrap().number, 1);
        let refused = store.begin_verification(
            uninstalled.task,
            uninstalled.generation,
            &uninstalled.entries,
            since(),
        );
        assert!(err(refused).contains("no installed candidate"));
    }
}
