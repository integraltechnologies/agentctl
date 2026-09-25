//! Acceptance: turning the exact candidate an independent verification
//! passed into accepted repository state.
//!
//! Acceptance advances in phases, each its own transaction, so that what an
//! interruption left unfinished is known from this store alone:
//!
//! 1. published: the acceptance, bound to its generation, execution and
//!    the one verification whose pass it acts on, is recorded together with
//!    the identity it publishes at every changed path of the candidate, and
//!    those identities become accepted source, all in one transaction, and
//!    only while the working tree, observed within it, holds the candidate;
//! 2. synchronized: CodeGraph holds no graph of a changed path derived
//!    from anything but its published identity;
//! 3. completed: the generation is accepted, completing its task, and its
//!    whole ownership is released, in one transaction.
//!
//! The schema refuses any phase whose facts do not hold, and any other way
//! of accepting a generation that executed. Until an acceptance completes,
//! what it published stays accepted source, its generation stays active
//! and keeps every path it owns, and it ends no other way. Published
//! identities are the execution's captured changes, never bytes read at
//! acceptance: observing the working tree only decides whether publishing
//! may go ahead.

use std::collections::BTreeMap;

use anyhow::{Context, Result, bail, ensure};
use rusqlite::{Connection, OptionalExtension, params};

use super::execution::Content;
use super::graph;
use super::ownership::owner;
use super::verification::drifted;
use super::{
    AcceptancePhase, ExecutionId, ExecutionOutcome, GenerationId, GenerationState, InstallOutcome,
    PlanState, Store, TaskId, VerificationId, VerificationOutcome, active_generation, event,
    generation_info, now, plan_state,
};
use crate::graph::Contribution;

/// The acceptance of one generation's verified candidate, as recorded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Acceptance {
    pub generation: GenerationId,
    /// The execution whose captured changes are the candidate.
    pub execution: ExecutionId,
    /// The verification whose pass it acts on.
    pub verification: VerificationId,
    pub started_at: i64,
    pub phase: AcceptancePhase,
    /// What it publishes at each changed path of the candidate, in order.
    pub changes: Vec<AcceptedChange>,
}

/// The accepted identity an acceptance publishes at one path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcceptedChange {
    pub path: String,
    /// The candidate's content hash, or `None` for accepted absence.
    pub hash: Option<String>,
    /// The accepted identity it replaced: `None` when the path had none,
    /// `Some(None)` when it was accepted as absent.
    pub replaced: Option<Option<String>>,
}

/// A verified candidate that may be accepted now: see
/// [`Store::acceptable`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Acceptable {
    pub execution: ExecutionId,
    pub verification: VerificationId,
    /// Each changed path and what the candidate put there, in order: a
    /// regular file's content or absence.
    pub candidate: Vec<(String, Content)>,
}

/// What trying to publish an acceptance's source established.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Publication {
    /// Every identity is accepted source.
    Published,
    /// The working tree no longer held the candidate at these paths, so
    /// nothing was recorded.
    Drifted(Vec<String>),
    /// The candidate may not be accepted, for the reason given, so nothing
    /// was recorded.
    Refused(String),
}

impl Store {
    /// The candidate of `generation`, of `task`, if it may be accepted now,
    /// or why not: the generation active in a ready or running plan and
    /// owning its task's whole scope and its execution's authority; its
    /// execution captured a candidate of authorized changes to regular
    /// files, which was installed; and the latest verification of that
    /// candidate is reconciled as passed.
    pub(crate) fn acceptable(
        &self,
        task: TaskId,
        generation: GenerationId,
    ) -> Result<std::result::Result<Acceptable, String>> {
        acceptable(&self.conn, task, generation)
    }

    /// Publishes the source of the verified candidate of `generation`, of
    /// `task`: records its acceptance, the identity it publishes at every
    /// changed path, and those identities as accepted source, in one
    /// transaction. Within it, once the candidate was found acceptable
    /// still, `observe` must return the working tree's entry at each of
    /// the given changed paths, in order; unless that is the candidate,
    /// nothing is recorded. An acceptance's source is published once: if
    /// it already was, nothing changes.
    pub(crate) fn publish_acceptance(
        &mut self,
        task: TaskId,
        generation: GenerationId,
        observe: impl FnOnce(&[String]) -> Result<Vec<(String, Content)>>,
    ) -> Result<Publication> {
        self.write(|tx| {
            let recorded = recorded(tx, generation)?;
            // Published meanwhile, by a racing call: never again.
            if let Some((_, _, _, phase)) = recorded
                && phase != AcceptancePhase::Intended
            {
                return Ok(Publication::Published);
            }
            let acceptable = match acceptable(tx, task, generation)? {
                Ok(acceptable) => acceptable,
                Err(why) => return Ok(Publication::Refused(why)),
            };
            let paths: Vec<String> = acceptable
                .candidate
                .iter()
                .map(|(p, _)| p.clone())
                .collect();
            let drifted = drifted(&acceptable.candidate, &observe(&paths)?)?;
            if !drifted.is_empty() {
                return Ok(Publication::Drifted(drifted));
            }
            match recorded {
                None => {
                    tx.execute(
                        "INSERT INTO acceptances
                           (generation_id, execution_id, verification_id, started_at)
                         VALUES (?1, ?2, ?3, ?4)",
                        params![
                            generation,
                            acceptable.execution,
                            acceptable.verification,
                            now()
                        ],
                    )?;
                }
                Some((execution, verification, ..)) => ensure!(
                    (execution, verification) == (acceptable.execution, acceptable.verification),
                    "generation {generation}'s acceptance is bound to another verification"
                ),
            }
            for (path, content) in &acceptable.candidate {
                let hash = match content {
                    Content::File(hash) => Some(hash.as_str()),
                    Content::Absent => None,
                    other => bail!("`{path}` would be accepted as {other:?}"),
                };
                let replaced: Option<Option<String>> = tx
                    .query_row(
                        "SELECT hash FROM accepted_sources WHERE path = ?1",
                        [path],
                        |r| r.get(0),
                    )
                    .optional()?;
                // An intent recorded before, never by agentctl, may already
                // name this path's identity, which the schema checked.
                tx.execute(
                    "INSERT INTO acceptance_sources
                       (generation_id, path, hash, prior_known, prior_hash)
                     SELECT ?1, ?2, ?3, ?4, ?5 WHERE NOT EXISTS (SELECT 1 FROM acceptance_sources
                       WHERE generation_id = ?1 AND path = ?2)",
                    params![
                        generation,
                        path,
                        hash,
                        replaced.is_some(),
                        replaced.flatten()
                    ],
                )?;
                tx.execute(
                    "INSERT INTO accepted_sources (path, hash, generation_id) VALUES (?1, ?2, ?3)
                     ON CONFLICT (path) DO UPDATE
                     SET hash = excluded.hash, generation_id = excluded.generation_id",
                    params![path, hash, generation],
                )?;
            }
            advance(tx, generation, AcceptancePhase::Published)?;
            let (plan, task, number, _) = generation_info(tx, generation)?;
            let detail = format!("generation {number}: {} paths", paths.len());
            event(
                tx,
                "acceptance.published",
                Some(plan),
                Some(task),
                None,
                &detail,
            )?;
            Ok(Publication::Published)
        })
    }

    /// Synchronizes CodeGraph with the source the acceptance of
    /// `generation` published, in one transaction: each changed path's
    /// graph becomes its contribution among `contributions`, which must be
    /// derived from its published content, or none at all. The source must
    /// be published; if CodeGraph already was synchronized, nothing
    /// changes.
    pub(crate) fn synchronize_acceptance(
        &mut self,
        generation: GenerationId,
        contributions: &[Contribution],
    ) -> Result<()> {
        self.write(|tx| {
            match recorded(tx, generation)?.map(|(.., phase)| phase) {
                Some(AcceptancePhase::Published) => {}
                // Synchronized meanwhile, by a racing call.
                Some(AcceptancePhase::Synchronized | AcceptancePhase::Completed) => return Ok(()),
                phase => bail!(
                    "generation {generation}'s acceptance has published no source ({phase:?})"
                ),
            }
            let changes = changes(tx, generation)?;
            let mut derived = BTreeMap::new();
            for c in contributions {
                let published = changes.iter().find(|change| change.path == c.path);
                ensure!(
                    published.is_some_and(|change| change.hash.as_deref() == Some(&c.hash)),
                    "the graph of `{}` is not derived from its published content",
                    c.path
                );
                ensure!(
                    derived.insert(c.path.as_str(), c).is_none(),
                    "`{}` has two graphs",
                    c.path
                );
            }
            for change in &changes {
                tx.execute("DELETE FROM graph_sources WHERE path = ?1", [&change.path])?;
                if let Some(c) = derived.get(change.path.as_str()) {
                    graph::replace(tx, c)?;
                }
            }
            advance(tx, generation, AcceptancePhase::Synchronized)?;
            let (plan, task, number, _) = generation_info(tx, generation)?;
            let detail = format!(
                "generation {number}: {} indexed, {} without a graph",
                derived.len(),
                changes.len() - derived.len()
            );
            event(
                tx,
                "acceptance.synchronized",
                Some(plan),
                Some(task),
                None,
                &detail,
            )
        })
    }

    /// Completes the acceptance of `generation`, once CodeGraph is
    /// synchronized, in one transaction: the generation is accepted, which
    /// completes its task, and every path it owns is released. If it
    /// already was completed, nothing changes.
    pub(crate) fn complete_acceptance(&mut self, generation: GenerationId) -> Result<()> {
        self.write(|tx| {
            match recorded(tx, generation)?.map(|(.., phase)| phase) {
                Some(AcceptancePhase::Synchronized) => {}
                // Completed meanwhile, by a racing call.
                Some(AcceptancePhase::Completed) => return Ok(()),
                phase => bail!(
                    "generation {generation}'s acceptance has not synchronized CodeGraph ({phase:?})"
                ),
            }
            let (plan, task, number) = active_generation(tx, generation)?;
            // Leave to accept the generation and release what it owns, which
            // the schema grants only while its whole ownership is held and
            // takes back as the phase 'completed' is recorded.
            tx.execute(
                "INSERT INTO acceptance_completions (generation_id) VALUES (?1)",
                [generation],
            )
            .with_context(|| format!("taking leave to complete generation {generation}"))?;
            tx.execute(
                "UPDATE generations SET state = ?2, ended_at = ?3 WHERE id = ?1",
                params![generation, GenerationState::Accepted, now()],
            )?;
            let released = tx.execute(
                "DELETE FROM ownership WHERE generation_id = ?1",
                [generation],
            )?;
            advance(tx, generation, AcceptancePhase::Completed)?;
            let ended = format!("generation {number} {}", GenerationState::Accepted);
            event(tx, "generation.ended", Some(plan), Some(task), None, &ended)?;
            if released > 0 {
                let detail = format!("generation {number}: {released} paths");
                event(
                    tx,
                    "ownership.released",
                    Some(plan),
                    Some(task),
                    None,
                    &detail,
                )?;
            }
            let detail = format!("generation {number}");
            event(
                tx,
                "acceptance.completed",
                Some(plan),
                Some(task),
                None,
                &detail,
            )
        })
    }

    /// The acceptance of `generation`, if one was ever recorded.
    pub fn acceptance(&self, generation: GenerationId) -> Result<Option<Acceptance>> {
        // One read transaction, so the phase and the changes agree.
        let tx = self.conn.unchecked_transaction()?;
        let Some((execution, verification, started_at, phase)) = recorded(&tx, generation)? else {
            return Ok(None);
        };
        Ok(Some(Acceptance {
            generation,
            execution,
            verification,
            started_at,
            phase,
            changes: changes(&tx, generation)?,
        }))
    }
}

/// The execution, verification, start and phase of `generation`'s
/// acceptance, if recorded.
fn recorded(
    conn: &Connection,
    generation: GenerationId,
) -> Result<Option<(ExecutionId, VerificationId, i64, AcceptancePhase)>> {
    let Some((execution, verification, started_at)) = conn
        .query_row(
            "SELECT execution_id, verification_id, started_at FROM acceptances
             WHERE generation_id = ?1",
            [generation],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?
    else {
        return Ok(None);
    };
    // Phases are recorded in order, so the last reached is the latest.
    let phase = conn
        .query_row(
            "SELECT phase FROM acceptance_phases WHERE generation_id = ?1
             ORDER BY CASE phase WHEN 'published' THEN 1 WHEN 'synchronized' THEN 2 ELSE 3 END
             DESC LIMIT 1",
            [generation],
            |r| r.get(0),
        )
        .optional()?
        .unwrap_or(AcceptancePhase::Intended);
    Ok(Some((execution, verification, started_at, phase)))
}

/// What `generation`'s acceptance publishes, in path order.
fn changes(conn: &Connection, generation: GenerationId) -> Result<Vec<AcceptedChange>> {
    let mut stmt = conn.prepare(
        "SELECT path, hash, prior_known, prior_hash FROM acceptance_sources
         WHERE generation_id = ?1 ORDER BY path",
    )?;
    let changes = stmt.query_map([generation], |r| {
        let known: bool = r.get(2)?;
        Ok(AcceptedChange {
            path: r.get(0)?,
            hash: r.get(1)?,
            replaced: if known { Some(r.get(3)?) } else { None },
        })
    })?;
    Ok(changes.collect::<rusqlite::Result<_>>()?)
}

/// Records that `generation`'s acceptance reached `phase`.
fn advance(conn: &Connection, generation: GenerationId, phase: AcceptancePhase) -> Result<()> {
    conn.execute(
        "INSERT INTO acceptance_phases (generation_id, phase, at) VALUES (?1, ?2, ?3)",
        params![generation, phase, now()],
    )
    .with_context(|| format!("recording generation {generation}'s acceptance as {phase}"))?;
    Ok(())
}

/// See [`Store::acceptable`]. Only a failure to read the store is an error.
fn acceptable(
    conn: &Connection,
    task: TaskId,
    generation: GenerationId,
) -> Result<std::result::Result<Acceptable, String>> {
    macro_rules! require {
        ($holds:expr, $($why:tt)+) => {
            if !$holds {
                return Ok(Err(format!($($why)+)));
            }
        };
    }
    let found: Option<(TaskId, GenerationState)> = conn
        .query_row(
            "SELECT task_id, state FROM generations WHERE id = ?1",
            [generation],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let Some((of, state)) = found else {
        return Ok(Err(format!("generation {generation} does not exist")));
    };
    require!(
        of == task,
        "generation {generation} belongs to task {of}, not task {task}"
    );
    require!(
        state == GenerationState::Active,
        "generation {generation} has already ended ({state})"
    );
    let (plan, ..) = generation_info(conn, generation)?;
    let plan_state = plan_state(conn, plan)?;
    require!(
        matches!(plan_state, PlanState::Ready | PlanState::Running),
        "plan {plan} is {plan_state}; only a ready or running plan accepts work"
    );

    type Installed = (
        ExecutionId,
        String,
        String,
        Option<ExecutionOutcome>,
        Option<String>,
        Option<InstallOutcome>,
    );
    let installed: Option<Installed> = conn
        .query_row(
            "SELECT e.id, e.authority, j.state, c.outcome, ij.state, r.outcome
             FROM executions e JOIN journal j ON j.id = e.journal_id
             LEFT JOIN execution_captures c ON c.execution_id = e.id AND j.state = 'reconciled'
             LEFT JOIN execution_installs i ON i.execution_id = e.id
             LEFT JOIN journal ij ON ij.id = i.journal_id
             LEFT JOIN execution_install_results r
               ON r.execution_id = e.id AND ij.state = 'reconciled'
             WHERE e.generation_id = ?1",
            [generation],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                ))
            },
        )
        .optional()?;
    let Some((execution, authority, executed, captured, installing, install)) = installed else {
        return Ok(Err(format!("generation {generation} has no execution")));
    };
    require!(
        executed == "reconciled",
        "execution {execution} is {executed}: what its executor did is not established"
    );
    require!(
        captured == Some(ExecutionOutcome::Candidate),
        "execution {execution} captured no candidate ({})",
        captured.map_or("nothing".into(), |o| o.to_string())
    );
    require!(
        installing.as_deref() == Some("reconciled"),
        "the candidate of execution {execution} is {}",
        match installing {
            None => "not installed".to_string(),
            Some(state) => format!("being installed ({state}), with its outcome unknown"),
        }
    );
    require!(
        install == Some(InstallOutcome::Installed),
        "the candidate of execution {execution} was not installed ({})",
        install.map_or("unknown".into(), |o| o.to_string())
    );

    let latest: Option<(VerificationId, i64, String, Option<VerificationOutcome>)> = conn
        .query_row(
            "SELECT v.id, v.number, j.state, r.outcome FROM verifications v
             JOIN journal j ON j.id = v.journal_id
             LEFT JOIN verification_results r
               ON r.verification_id = v.id AND j.state = 'reconciled'
             WHERE v.execution_id = ?1 ORDER BY v.number DESC LIMIT 1",
            [execution],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional()?;
    let Some((verification, number, verifying, verified)) = latest else {
        return Ok(Err(format!(
            "the candidate of execution {execution} was never verified"
        )));
    };
    require!(
        verifying == "reconciled",
        "verification {number} of the candidate is {verifying}, with its outcome unknown"
    );
    require!(
        verified == Some(VerificationOutcome::Passed),
        "verification {number} of the candidate ended {}, not passed",
        verified.map_or("unknown".into(), |o| o.to_string())
    );

    let mut stmt = conn.prepare(
        "SELECT path, after_kind, after_hash, authorized FROM execution_changes
         WHERE execution_id = ?1 ORDER BY path",
    )?;
    let changes = stmt
        .query_map([execution], |r| {
            Ok((r.get::<_, String>(0)?, Content::read(r, 1)?, r.get(3)?))
        })?
        .collect::<rusqlite::Result<Vec<(String, Content, bool)>>>()?;
    let mut candidate = Vec::with_capacity(changes.len());
    for (path, after, authorized) in changes {
        require!(
            authorized,
            "`{path}` changed beyond the execution's authority"
        );
        require!(
            matches!(after, Content::File(_) | Content::Absent),
            "`{path}` would be accepted as neither a regular file nor absent"
        );
        candidate.push((path, after));
    }

    let mut owned: Vec<String> = serde_json::from_str(&authority)?;
    owned.extend(
        conn.prepare("SELECT path FROM task_scope WHERE task_id = ?1")?
            .query_map([task], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?,
    );
    owned.sort_unstable();
    owned.dedup();
    for path in &owned {
        match owner(conn, path)? {
            Some(o) if o.generation == generation => {}
            Some(o) => {
                return Ok(Err(format!(
                    "`{path}` is owned by generation {} of task {}, not generation {generation}",
                    o.generation, o.task
                )));
            }
            None => {
                return Ok(Err(format!(
                    "generation {generation} does not own `{path}`"
                )));
            }
        }
    }
    Ok(Ok(Acceptable {
        execution,
        verification,
        candidate,
    }))
}
