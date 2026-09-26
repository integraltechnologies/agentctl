//! Executor attempts: the one disposable executor a generation gets, what
//! agentctl found it did in its workspace, and installing that into the
//! project's working tree.
//!
//! An execution is intended atomically with its executor agent, the
//! journal entry for its action and the repository baseline observed
//! beforehand, once the generation owns its task's whole scope. The
//! executor works in a disposable copy of that baseline outside the project
//! (see `crate::source::Workspace`). Its journal entry is attempted, before
//! any executor process exists, by the invocation embodying the executor.
//! Only capturing the workspace afterwards reconciles it, in the same
//! transaction that records every changed path and the outcome, which
//! `Store` derives itself from the durable baseline and the generation's
//! authority. Until then nothing is established about what the executor
//! did, however its invocation ended.
//!
//! The attempt never changes, and its capture is a record of its own that
//! the schema accepts only when consistent with the facts beside it: an
//! outcome is derived, never assigned. The workspace is given to the
//! executor alone, so no other actor agentctl knows of writes it, which is
//! still no proof of who wrote what it observed.
//!
//! A candidate is then installed into the working tree under a journal
//! entry of its own, attempted before any path there is written, and
//! reconciled with a result only once every path holds either the
//! candidate or, should installing stop short, what it held before. Until
//! then what the working tree holds is unknown.
//!
//! Nothing here accepts source, touches CodeGraph, ends the generation or
//! releases ownership: an installed candidate awaits verification.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result, bail, ensure};
use rusqlite::types::Type;
use rusqlite::{Connection, OptionalExtension, Row, Transaction, params};
use serde_json::json;

use super::ownership::owner;
use super::{
    ActionOutcome, AgentId, AgentScope, Attribution, Evidence, ExecutionId, ExecutionOutcome,
    FailureKind, GenerationId, GenerationState, InstallOutcome, Intent, InvocationId,
    InvocationState, JournalId, PlanState, Reported, Role, Store, TaskId, act_entry,
    active_generation, check_hash, check_path, event, generation_info, insert_agent, insert_intent,
    json_column, now, plan_state, reconcile_entry, unsatisfied_dependencies,
};

/// The journal action of an execution.
const ACTION: &str = "executor.run";
/// The journal action of installing its candidate.
const INSTALL: &str = "executor.install";

/// A repository path's entry, as agentctl observed it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Content {
    Absent,
    /// A regular file, by the SHA-256 of its bytes.
    File(String),
    /// A symlink, by the SHA-256 of its target.
    Symlink(String),
    /// A directory or other entry, whose content is not identified.
    Other,
}

impl Content {
    fn columns(&self) -> (&'static str, Option<&str>) {
        match self {
            Self::Absent => ("absent", None),
            Self::File(hash) => ("file", Some(hash)),
            Self::Symlink(hash) => ("symlink", Some(hash)),
            Self::Other => ("other", None),
        }
    }

    /// Reads the kind and hash columns at `i` and `i + 1`, which the
    /// schema keeps consistent.
    pub(super) fn read(r: &Row, i: usize) -> rusqlite::Result<Self> {
        let hash = || -> rusqlite::Result<String> { r.get(i + 1) };
        match r.get::<_, String>(i)?.as_str() {
            "absent" => Ok(Self::Absent),
            "file" => Ok(Self::File(hash()?)),
            "symlink" => Ok(Self::Symlink(hash()?)),
            "other" => Ok(Self::Other),
            kind => Err(rusqlite::Error::FromSqlConversionFailure(
                i,
                Type::Text,
                format!("unknown entry kind `{kind}`").into(),
            )),
        }
    }
}

/// An executor attempt of one generation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Execution {
    pub id: ExecutionId,
    pub generation: GenerationId,
    /// The executor: a logical agent serving only this attempt.
    pub agent: AgentId,
    /// The journal entry of its action.
    pub journal: JournalId,
    /// The literal paths the executor was authorized to mutate, in order.
    pub authority: Vec<String>,
    pub started_at: i64,
    pub status: ExecutionStatus,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecutionStatus {
    /// Intended, and never attempted: no executor ran.
    Intended,
    /// Attempted: an executor may have run and changed its workspace, and
    /// nothing about what it did is established, whether or not the
    /// invocation's own record says it ended.
    OutcomeUnknown {
        invocation: Option<InvocationId>,
    },
    Captured(Capture),
}

/// What capturing the changes in an execution's workspace established.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Capture {
    pub outcome: ExecutionOutcome,
    /// Why the changes could not be attributed to the executor, exactly
    /// when the outcome is unattributable.
    pub attribution: Option<Attribution>,
    pub invocation: InvocationId,
    /// What the executor reported and claimed to have modified, when its
    /// result was well formed: evidence, never proof.
    pub reported: Option<Reported>,
    pub claimed: Option<Vec<String>>,
    /// Every path agentctl observed change in the workspace, in order.
    pub changes: Vec<Change>,
    /// Whether the project's Git HEAD moved between the baseline and the
    /// capture. The executor's workspace holds no Git directory, so this is
    /// evidence of the project's activity, not of the executor's.
    pub head_moved: bool,
    pub at: i64,
    /// What became of installing the changes into the working tree.
    pub install: Install,
}

/// Installing a captured candidate into the project's working tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Install {
    /// Not attempted: nothing was written to the working tree. Only a
    /// candidate is ever installed.
    NotAttempted,
    /// Attempted without a recorded end: any changed path may hold its
    /// baseline or its candidate content.
    OutcomeUnknown,
    Finished {
        outcome: InstallOutcome,
        /// The paths found drifted from the baseline, when that stopped it.
        drifted: Vec<String>,
        at: i64,
    },
}

/// A path whose entry changed between an execution's baseline and its
/// capture.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    pub path: String,
    pub before: Content,
    pub after: Content,
    /// Whether the execution was authorized to mutate the path.
    pub authorized: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeKind {
    Created,
    Modified,
    Deleted,
}

impl Change {
    pub fn kind(&self) -> ChangeKind {
        match (&self.before, &self.after) {
            (Content::Absent, _) => ChangeKind::Created,
            (_, Content::Absent) => ChangeKind::Deleted,
            _ => ChangeKind::Modified,
        }
    }
}

impl Capture {
    /// Every literal path mutated beyond the execution's authority, in
    /// order.
    pub fn unauthorized(&self) -> Vec<&str> {
        self.changes
            .iter()
            .filter(|c| !c.authorized)
            .map(|c| c.path.as_str())
            .collect()
    }

    /// Changed paths the executor did not claim.
    pub fn unclaimed(&self) -> Vec<&str> {
        let claimed = self.claimed.as_deref().unwrap_or_default();
        self.changes
            .iter()
            .map(|c| c.path.as_str())
            .filter(|path| !claimed.iter().any(|claim| claim == path))
            .collect()
    }

    /// Paths the executor claimed to have modified that did not change.
    pub fn claimed_unchanged(&self) -> Vec<&str> {
        let claimed = self.claimed.as_deref().unwrap_or_default();
        claimed
            .iter()
            .map(String::as_str)
            .filter(|&claim| !self.changes.iter().any(|c| c.path == claim))
            .collect()
    }
}

/// What an executor's invocation yielded, as far as agentctl could read it.
#[derive(Debug, Clone, Copy)]
pub(crate) enum ExecutorResult<'a> {
    /// The invocation did not succeed, so it has no result.
    None,
    /// The invocation succeeded with a result breaking the protocol.
    Malformed,
    Reported {
        status: Reported,
        claimed: &'a [String],
    },
}

/// What agentctl observed once an execution's invocation ended.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Observed<'a> {
    /// The entry at every eligible path of the workspace and every baseline
    /// path, in strict path order.
    pub entries: &'a [(String, Content)],
    /// The project's Git HEAD.
    pub head: &'a str,
    /// Whether the workspace held still across repeated observation.
    pub settled: bool,
    pub result: ExecutorResult<'a>,
}

impl Store {
    /// The literal paths an executor of `generation` would be authorized to
    /// mutate: the whole current scope of `task`, which the generation must
    /// own. See [`Store::begin_execution`] for when one may start.
    pub(crate) fn execution_authority(
        &self,
        task: TaskId,
        generation: GenerationId,
    ) -> Result<Vec<String>> {
        authority(&self.conn, task, generation)
    }

    /// INTEND: records the executor attempt of an active generation of
    /// `task`, with its executor agent, its journal entry and the
    /// repository `baseline` and Git `head` observed beforehand, from
    /// `since` on. The task must be ready to run in a ready or running plan,
    /// and the generation must own the task's whole scope, which becomes the
    /// attempt's authority and must still be `expected`. A generation gets
    /// one attempt.
    pub(crate) fn begin_execution(
        &mut self,
        task: TaskId,
        generation: GenerationId,
        expected: &[String],
        baseline: &[(String, Content)],
        head: &str,
        since: i64,
    ) -> Result<(ExecutionId, AgentId, JournalId)> {
        ensure!(
            since <= now(),
            "the baseline cannot be observed in the future"
        );
        self.write(|tx| {
            let authority = authority(tx, task, generation)?;
            ensure!(
                authority == expected,
                "the authority of generation {generation} changed"
            );
            let agent = insert_agent(tx, Role::Executor, AgentScope::Generation(generation))?;
            let parameters = json!({"generation": generation, "task": task});
            let intent = Intent {
                action: ACTION.into(),
                parameters: parameters.as_object().cloned().unwrap_or_default(),
            };
            let entry = insert_intent(tx, agent, &intent)?;
            tx.execute(
                "INSERT INTO executions
                   (generation_id, agent_id, journal_id, authority, head, started_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    generation,
                    agent,
                    entry,
                    serde_json::to_string(&authority)?,
                    head,
                    since
                ],
            )?;
            let execution = ExecutionId(tx.last_insert_rowid());
            let mut insert = tx.prepare(
                "INSERT INTO execution_baseline (execution_id, path, kind, hash)
                 VALUES (?1, ?2, ?3, ?4)",
            )?;
            for (path, content) in baseline {
                check_content(path, content)?;
                let (kind, hash) = content.columns();
                insert
                    .execute(params![execution, path, kind, hash])
                    .with_context(|| format!("recording the baseline of `{path}`"))?;
            }
            Ok((execution, agent, entry))
        })
    }

    /// RECONCILE: records what capturing an attempted execution observed in
    /// its workspace, once its invocation has ended, and derives the
    /// outcome. The changes are the observed entries that differ from the
    /// durable baseline, each authorized only if the attempt's authority
    /// names it. Changes are unattributable when the workspace did not hold
    /// still or no executor process was ever launched. Otherwise any change
    /// beyond authority violates the scope. Only then does what the
    /// invocation yielded decide the outcome.
    pub(crate) fn finish_execution(
        &mut self,
        execution: ExecutionId,
        observed: &Observed,
    ) -> Result<ExecutionOutcome> {
        let sorted = observed.entries.windows(2).all(|w| w[0].0 < w[1].0);
        ensure!(sorted, "observed entries must be in strict path order");
        let claimed = match observed.result {
            ExecutorResult::Reported { claimed, .. } => {
                claimed.iter().try_for_each(|path| check_path(path))?;
                Some(serde_json::to_string(claimed)?)
            }
            _ => None,
        };
        self.write(|tx| {
            let (generation, entry, authority, captured): (
                GenerationId,
                JournalId,
                String,
                Option<ExecutionOutcome>,
            ) = tx
                .query_row(
                    "SELECT e.generation_id, e.journal_id, e.authority, c.outcome
                     FROM executions e LEFT JOIN execution_captures c ON c.execution_id = e.id
                     WHERE e.id = ?1",
                    [execution],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
                )
                .optional()?
                .with_context(|| format!("execution {execution} does not exist"))?;
            if let Some(outcome) = captured {
                bail!("execution {execution} was already captured ({outcome})");
            }
            let (state, invocation): (String, Option<InvocationId>) = tx.query_row(
                "SELECT state, invocation_id FROM journal WHERE id = ?1",
                [entry],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            ensure!(
                state == "attempted",
                "execution {execution} is {state}; only an attempted execution is captured"
            );
            let invocation = invocation
                .with_context(|| format!("execution {execution} was attempted without one"))?;
            let (ended, failure): (InvocationState, Option<FailureKind>) = tx.query_row(
                "SELECT state, failure FROM invocations WHERE id = ?1",
                [invocation],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            ensure!(
                ended.is_terminal(),
                "invocation {invocation} has not ended, so execution {execution} is not captured"
            );
            ensure!(
                (ended == InvocationState::Succeeded)
                    != matches!(observed.result, ExecutorResult::None),
                "an executor has a result exactly when its invocation succeeded"
            );

            let authority: BTreeSet<String> = serde_json::from_str(&authority)?;
            let mut baseline: BTreeMap<String, Content> = tx
                .prepare("SELECT path, kind, hash FROM execution_baseline WHERE execution_id = ?1")?
                .query_map([execution], |r| Ok((r.get(0)?, Content::read(r, 1)?)))?
                .collect::<rusqlite::Result<_>>()?;
            let mut changes = Vec::new();
            for (path, after) in observed.entries {
                check_content(path, after)?;
                let before = baseline.remove(path).unwrap_or(Content::Absent);
                if before != *after {
                    let authorized = authority.contains(path);
                    changes.push((path, before, after, authorized));
                }
            }
            if let Some(path) = baseline.keys().next() {
                bail!("the observation omits baseline path `{path}`");
            }
            let beyond = changes.iter().filter(|c| !c.3).count();

            let launched = !(ended == InvocationState::Failed
                && failure.is_some_and(FailureKind::before_launch));
            // The workspace is the executor's alone, so neither another
            // action in flight nor another generation's ownership casts
            // doubt on what changed there.
            let attribution = if !observed.settled {
                Some(Attribution::Unsettled)
            } else if !launched && !changes.is_empty() {
                Some(Attribution::NeverLaunched)
            } else {
                None
            };
            let reported = match observed.result {
                ExecutorResult::Reported { status, .. } => Some(status),
                _ => None,
            };
            use ExecutionOutcome::*;
            let outcome = match (attribution, observed.result) {
                (Some(_), _) => Unattributable,
                _ if beyond > 0 => ScopeViolated,
                (None, ExecutorResult::None) => InvocationFailed,
                (None, ExecutorResult::Malformed) => MalformedResult,
                (None, ExecutorResult::Reported { status, .. }) => match status {
                    Reported::Succeeded => Candidate,
                    Reported::Failed => ReportedFailed,
                },
            };

            let mut insert = tx.prepare(
                "INSERT INTO execution_changes (execution_id, path, before_kind, before_hash,
                   after_kind, after_hash, authorized)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            )?;
            for (path, before, after, authorized) in &changes {
                let (before_kind, before_hash) = before.columns();
                let (after_kind, after_hash) = after.columns();
                insert.execute(params![
                    execution,
                    path,
                    before_kind,
                    before_hash,
                    after_kind,
                    after_hash,
                    authorized
                ])?;
            }
            tx.execute(
                "INSERT INTO execution_captures (execution_id, outcome, attribution, reported,
                   claimed, head_after, captured_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    execution,
                    outcome,
                    attribution,
                    reported,
                    claimed,
                    observed.head,
                    now()
                ],
            )?;

            let mut evidence = vec![
                Evidence::Invocation { invocation },
                Evidence::Fact {
                    name: format!("execution.{outcome}"),
                },
            ];
            if let Some(attribution) = attribution {
                let name = format!("attribution.{attribution}");
                evidence.push(Evidence::Fact { name });
            }
            let action = match outcome {
                Candidate => ActionOutcome::CompletedAsIntended,
                ScopeViolated | Unattributable => ActionOutcome::CompletedWithDeviation,
                ReportedFailed | MalformedResult | InvocationFailed => ActionOutcome::Failed,
            };
            reconcile_entry(tx, entry, action, &evidence)?;
            let (plan, task, number, _) = generation_info(tx, generation)?;
            let detail = format!(
                "generation {number}: {outcome}, {} changed, {beyond} beyond authority",
                changes.len()
            );
            event(
                tx,
                "execution.captured",
                Some(plan),
                Some(task),
                None,
                &detail,
            )?;
            Ok(outcome)
        })
    }

    /// INTEND: records installing the captured candidate of `execution`
    /// into the project's working tree, as the executor's own action. Its
    /// generation must still be active and own every path of the attempt's
    /// authority. A candidate is installed at most once; the entry must be
    /// attempted before any path in the working tree is written.
    pub(crate) fn begin_install(&mut self, execution: ExecutionId) -> Result<JournalId> {
        self.write(|tx| {
            let (generation, agent, authority, outcome): (
                GenerationId,
                AgentId,
                String,
                Option<ExecutionOutcome>,
            ) = tx
                .query_row(
                    "SELECT e.generation_id, e.agent_id, e.authority, c.outcome
                     FROM executions e JOIN journal j ON j.id = e.journal_id
                     LEFT JOIN execution_captures c
                       ON c.execution_id = e.id AND j.state = 'reconciled'
                     WHERE e.id = ?1",
                    [execution],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
                )
                .optional()?
                .with_context(|| format!("execution {execution} does not exist"))?;
            ensure!(
                outcome == Some(ExecutionOutcome::Candidate),
                "execution {execution} has no captured candidate to install"
            );
            active_generation(tx, generation)?;
            let authority: Vec<String> = serde_json::from_str(&authority)?;
            for path in &authority {
                ensure!(
                    owner(tx, path)?.is_some_and(|o| o.generation == generation),
                    "generation {generation} no longer owns `{path}`"
                );
            }
            let installed: bool = tx.query_row(
                "SELECT EXISTS (SELECT 1 FROM execution_installs WHERE execution_id = ?1)",
                [execution],
                |r| r.get(0),
            )?;
            ensure!(
                !installed,
                "execution {execution} was already installed, or tried to be"
            );
            let parameters = json!({"execution": execution, "generation": generation});
            let intent = Intent {
                action: INSTALL.into(),
                parameters: parameters.as_object().cloned().unwrap_or_default(),
            };
            let entry = insert_intent(tx, agent, &intent)?;
            tx.execute(
                "INSERT INTO execution_installs (execution_id, journal_id) VALUES (?1, ?2)",
                params![execution, entry],
            )?;
            Ok(entry)
        })
    }

    /// RECONCILE: records how installing the candidate of `execution` ended,
    /// once attempted: `drifted` names the paths that stopped it, exactly
    /// when it drifted. Only an end that leaves every changed path holding
    /// either its candidate content (installed) or what it held before
    /// (anything else) is recorded.
    pub(crate) fn finish_install(
        &mut self,
        execution: ExecutionId,
        outcome: InstallOutcome,
        drifted: &[String],
    ) -> Result<()> {
        check_install(outcome, drifted)?;
        self.write(|tx| record_install(tx, execution, outcome, drifted))
    }

    /// ACT and RECONCILE at once: records that preparing to install the
    /// candidate of `execution` found it was not to be installed, as
    /// `drifted` or `refused`, so that nothing was written. One transaction,
    /// so the entry is never attempted without this result.
    pub(crate) fn decline_install(
        &mut self,
        execution: ExecutionId,
        outcome: InstallOutcome,
        drifted: &[String],
    ) -> Result<()> {
        ensure!(
            matches!(outcome, InstallOutcome::Drifted | InstallOutcome::Refused),
            "an install is declined only as drifted or refused"
        );
        check_install(outcome, drifted)?;
        self.write(|tx| {
            let (entry, _) = install_entry(tx, execution)?;
            act_entry(tx, entry, None)?;
            record_install(tx, execution, outcome, drifted)
        })
    }

    /// The executor attempt of `generation`, if one was ever intended.
    pub fn execution(&self, generation: GenerationId) -> Result<Option<Execution>> {
        // One read transaction, so the attempt and its changes agree. A
        // capture counts only once the journal is reconciled with it.
        let tx = self.conn.unchecked_transaction()?;
        let Some((mut execution, captured)) = tx
            .query_row(
                "SELECT e.id, e.agent_id, e.journal_id, e.authority, e.started_at,
                   j.state, j.invocation_id, c.outcome, c.attribution, c.reported, c.claimed,
                   e.head <> c.head_after, c.captured_at, ij.state, r.outcome, r.drifted,
                   r.finished_at
                 FROM executions e JOIN journal j ON j.id = e.journal_id
                 LEFT JOIN execution_captures c
                   ON c.execution_id = e.id AND j.state = 'reconciled'
                 LEFT JOIN execution_installs i ON i.execution_id = e.id
                 LEFT JOIN journal ij ON ij.id = i.journal_id
                 LEFT JOIN execution_install_results r
                   ON r.execution_id = e.id AND ij.state = 'reconciled'
                 WHERE e.generation_id = ?1",
                [generation],
                |r| {
                    let invocation: Option<InvocationId> = r.get(6)?;
                    let status = match r.get::<_, String>(5)?.as_str() {
                        "intended" => ExecutionStatus::Intended,
                        _ => ExecutionStatus::OutcomeUnknown { invocation },
                    };
                    let execution = Execution {
                        id: r.get(0)?,
                        generation,
                        agent: r.get(1)?,
                        journal: r.get(2)?,
                        authority: json_column(r, 3)?,
                        started_at: r.get(4)?,
                        status,
                    };
                    // A result counts only once its journal entry is
                    // reconciled; until then an attempt's end is unknown.
                    let install = match (r.get::<_, Option<String>>(13)?.as_deref(), r.get(14)?) {
                        (None | Some("intended"), _) => Install::NotAttempted,
                        (_, None) => Install::OutcomeUnknown,
                        (_, Some(outcome)) => Install::Finished {
                            outcome,
                            drifted: r
                                .get::<_, Option<String>>(15)?
                                .map(|_| json_column(r, 15))
                                .transpose()?
                                .unwrap_or_default(),
                            at: r.get(16)?,
                        },
                    };
                    let captured = match r.get::<_, Option<ExecutionOutcome>>(7)? {
                        None => None,
                        Some(outcome) => Some(Capture {
                            outcome,
                            attribution: r.get(8)?,
                            invocation: r.get(6)?,
                            reported: r.get(9)?,
                            claimed: r
                                .get::<_, Option<String>>(10)?
                                .map(|_| json_column(r, 10))
                                .transpose()?,
                            changes: Vec::new(),
                            head_moved: r.get(11)?,
                            at: r.get(12)?,
                            install,
                        }),
                    };
                    Ok((execution, captured))
                },
            )
            .optional()?
        else {
            return Ok(None);
        };
        if let Some(mut capture) = captured {
            capture.changes = tx
                .prepare(
                    "SELECT path, before_kind, before_hash, after_kind, after_hash, authorized
                     FROM execution_changes WHERE execution_id = ?1 ORDER BY path",
                )?
                .query_map([execution.id], |r| {
                    Ok(Change {
                        path: r.get(0)?,
                        before: Content::read(r, 1)?,
                        after: Content::read(r, 3)?,
                        authorized: r.get(5)?,
                    })
                })?
                .collect::<rusqlite::Result<_>>()?;
            execution.status = ExecutionStatus::Captured(capture);
        }
        Ok(Some(execution))
    }
}

/// The literal paths an executor of `generation` is authorized to mutate:
/// the whole current scope of `task`, every path of which the generation
/// must own. Only an active generation of a task whose dependencies are
/// complete, in a ready or running plan, gets an executor, and only once.
fn authority(conn: &Connection, task: TaskId, generation: GenerationId) -> Result<Vec<String>> {
    let (plan, of, _, state) = generation_info(conn, generation)?;
    ensure!(
        of == task,
        "generation {generation} belongs to task {of}, not task {task}"
    );
    ensure!(
        state == GenerationState::Active,
        "generation {generation} has already ended ({state})"
    );
    let plan_state = plan_state(conn, plan)?;
    ensure!(
        matches!(plan_state, PlanState::Ready | PlanState::Running),
        "plan {plan} is {plan_state}; only a ready or running plan executes tasks"
    );
    ensure!(
        unsatisfied_dependencies(conn, task)?.is_empty(),
        "task {task} depends on tasks not yet completed"
    );
    let executed: bool = conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM executions WHERE generation_id = ?1)",
        [generation],
        |r| r.get(0),
    )?;
    ensure!(
        !executed,
        "generation {generation} already had its executor attempt"
    );
    let scope: Vec<String> = conn
        .prepare("SELECT path FROM task_scope WHERE task_id = ?1 ORDER BY path")?
        .query_map([task], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    for path in &scope {
        match owner(conn, path)? {
            Some(o) if o.generation == generation => {}
            Some(o) => bail!(
                "`{path}` is owned by generation {} of task {}, not generation {generation}",
                o.generation,
                o.task
            ),
            None => bail!("generation {generation} does not own `{path}`"),
        }
    }
    Ok(scope)
}

fn check_install(outcome: InstallOutcome, drifted: &[String]) -> Result<()> {
    ensure!(
        (outcome == InstallOutcome::Drifted) != drifted.is_empty(),
        "drifted paths are given exactly when an install drifted"
    );
    drifted.iter().try_for_each(|path| check_path(path))
}

/// The journal entry installing the candidate of `execution`, and its
/// generation.
fn install_entry(conn: &Connection, execution: ExecutionId) -> Result<(JournalId, GenerationId)> {
    conn.query_row(
        "SELECT i.journal_id, e.generation_id FROM execution_installs i
         JOIN executions e ON e.id = i.execution_id WHERE i.execution_id = ?1",
        [execution],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
    .optional()?
    .with_context(|| format!("execution {execution} was never installed"))
}

/// Records how the attempted install of `execution` ended, reconciling its
/// journal entry.
fn record_install(
    tx: &Transaction,
    execution: ExecutionId,
    outcome: InstallOutcome,
    drifted: &[String],
) -> Result<()> {
    let (entry, generation) = install_entry(tx, execution)?;
    let drifted = (!drifted.is_empty())
        .then(|| serde_json::to_string(drifted))
        .transpose()?;
    tx.execute(
        "INSERT INTO execution_install_results
           (execution_id, outcome, drifted, finished_at)
         VALUES (?1, ?2, ?3, ?4)",
        params![execution, outcome, drifted, now()],
    )?;
    let action = match outcome {
        InstallOutcome::Installed => ActionOutcome::CompletedAsIntended,
        _ => ActionOutcome::Failed,
    };
    let evidence = [Evidence::Fact {
        name: format!("install.{outcome}"),
    }];
    reconcile_entry(tx, entry, action, &evidence)?;
    let (plan, task, number, _) = generation_info(tx, generation)?;
    let detail = format!("generation {number}: {outcome}");
    event(
        tx,
        "execution.installed",
        Some(plan),
        Some(task),
        None,
        &detail,
    )?;
    Ok(())
}

fn check_content(path: &str, content: &Content) -> Result<()> {
    check_path(path)?;
    match content {
        Content::File(hash) | Content::Symlink(hash) => check_hash(hash),
        Content::Absent | Content::Other => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::tests::{
        acquire, ended, err, intent, objective, ready_plan, store, succeeded,
    };
    use crate::state::{ActionStatus, GenerationEnd, InvocationEnd};
    use std::thread;
    use std::time::Duration;

    const HEAD: &str = "refs/heads/main unborn";

    fn file(n: u8) -> Content {
        Content::File(format!("{n:064x}"))
    }

    fn entries(items: &[(&str, Content)]) -> Vec<(String, Content)> {
        let mut entries: Vec<_> = items
            .iter()
            .map(|(p, c)| (p.to_string(), c.clone()))
            .collect();
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        entries
    }

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    /// When a baseline observed now began: later, to the millisecond, than
    /// anything recorded so far, as a real observation would be.
    fn since() -> i64 {
        thread::sleep(Duration::from_millis(2));
        now()
    }

    /// An execution intended against `baseline` by a new task whose
    /// generation owns all of `scope`.
    fn intended(
        store: &mut Store,
        scope: &[&str],
        baseline: &[(String, Content)],
    ) -> (GenerationId, ExecutionId, AgentId, JournalId) {
        let (_, tasks) = ready_plan(store, &[("t", scope, &[])]);
        let generation = store.start_generation(tasks[0]).unwrap();
        acquire(store, generation, scope);
        let mut authority = strings(scope);
        authority.sort();
        let (execution, agent, entry) = store
            .begin_execution(tasks[0], generation, &authority, baseline, HEAD, since())
            .unwrap();
        (generation, execution, agent, entry)
    }

    /// An execution as above, attempted by a running invocation.
    fn attempted(
        store: &mut Store,
        scope: &[&str],
        baseline: &[(String, Content)],
    ) -> (GenerationId, ExecutionId, InvocationId) {
        let (generation, execution, agent, entry) = intended(store, scope, baseline);
        let invocation = store.start_invocation(agent, "claude", "m", None).unwrap();
        store.act(entry, Some(invocation)).unwrap();
        store.invocation_running(invocation).unwrap();
        (generation, execution, invocation)
    }

    /// Ends a generation and releases what it owns, so that the next test
    /// case can own the same paths.
    fn retire(store: &mut Store, generation: GenerationId) {
        store
            .finish_generation(generation, GenerationEnd::Failed)
            .unwrap();
        store.release_ownership(generation).unwrap();
    }

    fn failed(launched: bool) -> InvocationEnd {
        InvocationEnd {
            failure: Some(match launched {
                true => FailureKind::ExitStatus,
                false => FailureKind::ExecutableMissing,
            }),
            diagnostic: Some("failed".into()),
            exit_code: launched.then_some(1),
            ..ended(InvocationState::Failed)
        }
    }

    fn reported(status: Reported, claimed: &[String]) -> ExecutorResult<'_> {
        ExecutorResult::Reported { status, claimed }
    }

    fn observed<'a>(entries: &'a [(String, Content)], result: ExecutorResult<'a>) -> Observed<'a> {
        Observed {
            entries,
            head: HEAD,
            settled: true,
            result,
        }
    }

    fn capture(store: &Store, generation: GenerationId) -> Capture {
        match store.execution(generation).unwrap().unwrap().status {
            ExecutionStatus::Captured(capture) => capture,
            status => panic!("{status:?}"),
        }
    }

    #[test]
    fn executions_start_only_with_complete_authority() {
        let (_dir, mut store) = store();
        let tasks: [(&str, &[&str], &[&str]); 4] = [
            ("pair", &["src/a.rs", "src/b.rs"], &[]),
            ("rival", &["src/b.rs"], &[]),
            ("later", &[], &["pair"]),
            ("read", &[], &[]),
        ];
        let (_, ids) = ready_plan(&mut store, &tasks);
        let [pair, rival, later, read] = ids[..] else {
            panic!()
        };
        let generation = store.start_generation(pair).unwrap();
        let begin = |store: &mut Store, task, generation, authority: &[&str]| {
            store.begin_execution(task, generation, &strings(authority), &[], HEAD, since())
        };

        acquire(&mut store, generation, &["src/a.rs"]);
        let refused = begin(&mut store, pair, generation, &["src/a.rs", "src/b.rs"]);
        assert!(err(refused).contains("does not own `src/b.rs`"));
        let other = store.start_generation(rival).unwrap();
        acquire(&mut store, other, &["src/b.rs"]);
        let refused = begin(&mut store, pair, generation, &["src/a.rs", "src/b.rs"]);
        assert!(err(refused).contains("`src/b.rs` is owned by generation"));
        let refused = begin(&mut store, rival, generation, &["src/a.rs", "src/b.rs"]);
        assert!(err(refused).contains("belongs to task"));
        let waiting = store.start_generation(later).unwrap();
        assert!(err(begin(&mut store, later, waiting, &[])).contains("not yet completed"));
        // Nothing was intended by any refused start.
        assert_eq!(store.execution(generation).unwrap(), None);
        let kinds: Vec<_> = store.events_after(0, 1000).unwrap();
        assert!(kinds.iter().all(|e| e.kind != "agent.created"), "{kinds:?}");

        // A read-only task needs no ownership, and gets one attempt.
        let reading = store.start_generation(read).unwrap();
        let (execution, agent, entry) = begin(&mut store, read, reading, &[]).unwrap();
        let recorded = store.execution(reading).unwrap().unwrap();
        assert_eq!(
            (recorded.id, recorded.agent, recorded.journal),
            (execution, agent, entry)
        );
        assert_eq!(recorded.status, ExecutionStatus::Intended);
        assert!(recorded.authority.is_empty());
        assert!(err(begin(&mut store, read, reading, &[])).contains("already had its executor"));

        // The authority is the task's scope as recorded when intended.
        let refused = begin(&mut store, rival, other, &[]);
        assert!(err(refused).contains("authority of generation"));
        store
            .finish_generation(other, GenerationEnd::Failed)
            .unwrap();
        let refused = begin(&mut store, rival, other, &["src/b.rs"]);
        assert!(err(refused).contains("already ended"));
        let draft = store.create_plan(&objective("draft")).unwrap();
        let task = store.add_task(draft, "draft", &[]).unwrap();
        let drafting = store.start_generation(task).unwrap();
        let refused = begin(&mut store, task, drafting, &[]);
        assert!(err(refused).contains("only a ready or running plan"));
    }

    #[test]
    fn captures_follow_the_attempt_and_happen_once() {
        let (dir, mut store) = store();
        let baseline = entries(&[("src/a.rs", file(1)), ("README.md", file(2))]);
        let (generation, execution, agent, entry) = intended(&mut store, &["src/a.rs"], &baseline);
        let after = entries(&[("src/a.rs", file(3)), ("README.md", file(2))]);
        let claimed = strings(&["src/a.rs"]);
        let result = reported(Reported::Succeeded, &claimed);
        let refused = store.finish_execution(execution, &observed(&after, result));
        assert!(err(refused).contains("only an attempted execution"));

        let invocation = store.start_invocation(agent, "claude", "m", None).unwrap();
        store.act(entry, Some(invocation)).unwrap();
        store.invocation_running(invocation).unwrap();
        let unknown = ExecutionStatus::OutcomeUnknown {
            invocation: Some(invocation),
        };
        assert_eq!(
            store.execution(generation).unwrap().unwrap().status,
            unknown
        );
        let refused = store.finish_execution(execution, &observed(&after, result));
        assert!(err(refused).contains("has not ended"));
        store.finish_invocation(invocation, &succeeded()).unwrap();
        // How the invocation ended establishes nothing about the repository.
        assert_eq!(
            store.execution(generation).unwrap().unwrap().status,
            unknown
        );

        let omitted = entries(&[("src/a.rs", file(3))]);
        let refused = store.finish_execution(execution, &observed(&omitted, result));
        assert!(err(refused).contains("omits baseline path `README.md`"));
        let unsorted = [after[1].clone(), after[0].clone()];
        let refused = store.finish_execution(execution, &observed(&unsorted, result));
        assert!(err(refused).contains("strict path order"));
        let refused = store.finish_execution(execution, &observed(&after, ExecutorResult::None));
        assert!(err(refused).contains("exactly when its invocation succeeded"));

        let outcome = store.finish_execution(execution, &observed(&after, result));
        assert_eq!(outcome.unwrap(), ExecutionOutcome::Candidate);
        let refused = store.finish_execution(execution, &observed(&after, result));
        assert!(err(refused).contains("already captured (candidate)"));
        let captured = capture(&store, generation);
        assert_eq!(
            captured.changes,
            [Change {
                path: "src/a.rs".into(),
                before: file(1),
                after: file(3),
                authorized: true,
            }]
        );
        assert_eq!(captured.claimed, Some(claimed));
        let ActionStatus::Reconciled(attempt, reconciliation) =
            store.journal_entry(entry).unwrap().status
        else {
            panic!("reconciled with the capture")
        };
        assert_eq!(attempt.invocation, Some(invocation));
        assert_eq!(reconciliation.outcome, ActionOutcome::CompletedAsIntended);
        assert_eq!(
            reconciliation.evidence,
            [
                Evidence::Invocation { invocation },
                Evidence::Fact {
                    name: "execution.candidate".into()
                }
            ]
        );

        // A fresh store reconstructs the same capture, which is history.
        drop(store);
        let store = Store::open(&dir.path().join("state.db")).unwrap();
        assert_eq!(capture(&store, generation), captured);
        for sql in [
            "UPDATE execution_captures SET outcome = 'scope_violated'",
            "UPDATE executions SET authority = '[]'",
            "DELETE FROM executions",
            "DELETE FROM execution_captures",
        ] {
            let message = store.conn.execute(sql, []).unwrap_err().to_string();
            assert!(
                message.contains("execution history is immutable"),
                "{message}"
            );
        }
    }

    #[test]
    fn outcomes_follow_observation_before_claims() {
        use ExecutionOutcome::*;
        let (_dir, mut store) = store();
        let baseline = entries(&[
            ("src/a.rs", file(1)),
            ("src/x.rs", file(2)),
            ("src/gone.rs", Content::Absent),
        ]);
        let scope = ["src/a.rs", "src/new.rs"];
        let claim_a = strings(&["src/a.rs"]);
        let a_changed = entries(&[
            ("src/a.rs", file(9)),
            ("src/x.rs", file(2)),
            ("src/gone.rs", Content::Absent),
        ]);
        let x_changed = entries(&[
            ("src/a.rs", file(9)),
            ("src/x.rs", file(8)),
            ("src/gone.rs", Content::Absent),
        ]);
        let run = |store: &mut Store,
                   end: &InvocationEnd,
                   observed: Observed|
         -> (ExecutionOutcome, Option<Attribution>) {
            let (generation, execution, invocation) = attempted(store, &scope, &baseline);
            store.finish_invocation(invocation, end).unwrap();
            let outcome = store.finish_execution(execution, &observed).unwrap();
            let captured = capture(store, generation);
            assert_eq!(captured.outcome, outcome);
            retire(store, generation);
            (outcome, captured.attribution)
        };

        let result = reported(Reported::Succeeded, &claim_a);
        let cases = [
            (succeeded(), observed(&a_changed, result), (Candidate, None)),
            (
                succeeded(),
                observed(&a_changed, reported(Reported::Failed, &claim_a)),
                (ReportedFailed, None),
            ),
            (
                succeeded(),
                observed(&a_changed, ExecutorResult::Malformed),
                (MalformedResult, None),
            ),
            (
                failed(true),
                observed(&a_changed, ExecutorResult::None),
                (InvocationFailed, None),
            ),
            (
                failed(false),
                observed(&baseline, ExecutorResult::None),
                (InvocationFailed, None),
            ),
            // Observed mutation outranks what the executor says of itself.
            (
                succeeded(),
                observed(&x_changed, result),
                (ScopeViolated, None),
            ),
            (
                failed(true),
                observed(&x_changed, ExecutorResult::None),
                (ScopeViolated, None),
            ),
            // The project's HEAD is not the executor's to move: evidence.
            (
                succeeded(),
                Observed {
                    head: "refs/heads/main 0123",
                    ..observed(&a_changed, result)
                },
                (Candidate, None),
            ),
            (
                succeeded(),
                Observed {
                    settled: false,
                    ..observed(&a_changed, result)
                },
                (Unattributable, Some(Attribution::Unsettled)),
            ),
            (
                failed(false),
                observed(&a_changed, ExecutorResult::None),
                (Unattributable, Some(Attribution::NeverLaunched)),
            ),
        ];
        for (end, observed, expected) in cases {
            // A failure before launch leaves the invocation starting.
            let (generation, execution, agent, entry) = intended(&mut store, &scope, &baseline);
            let invocation = store.start_invocation(agent, "claude", "m", None).unwrap();
            store.act(entry, Some(invocation)).unwrap();
            if end.failure.is_none_or(|f| !f.before_launch()) {
                store.invocation_running(invocation).unwrap();
            }
            store.finish_invocation(invocation, &end).unwrap();
            let outcome = store.finish_execution(execution, &observed).unwrap();
            let captured = capture(&store, generation);
            assert_eq!((outcome, captured.attribution), expected, "{observed:?}");
            retire(&mut store, generation);
        }
        let (_, x) = run(&mut store, &succeeded(), observed(&x_changed, result));
        assert_eq!(x, None);

        // A path another generation owns, changed in the workspace: the
        // executor's own mutation beyond its authority.
        let (_, rival) = ready_plan(&mut store, &[("rival", &["src/x.rs"], &[])]);
        let owner = store.start_generation(rival[0]).unwrap();
        acquire(&mut store, owner, &["src/x.rs"]);
        let (outcome, attribution) = run(&mut store, &succeeded(), observed(&x_changed, result));
        assert_eq!((outcome, attribution), (ScopeViolated, None));

        // So is one while another execution acts: that one works in a
        // workspace of its own.
        let (other, ..) = attempted(&mut store, &[], &[]);
        let (generation, execution, invocation) = attempted(&mut store, &scope, &baseline);
        store.finish_invocation(invocation, &succeeded()).unwrap();
        let mut y_created = baseline.clone();
        y_created.push(("src/y.rs".into(), file(4)));
        let outcome = store.finish_execution(execution, &observed(&y_created, result));
        assert_eq!(outcome.unwrap(), ScopeViolated);
        let captured = capture(&store, generation);
        assert_eq!(captured.attribution, None);
        assert_eq!(captured.unauthorized(), ["src/y.rs"]);
        assert_eq!(captured.changes[0].kind(), ChangeKind::Created);
        assert_eq!(captured.install, Install::NotAttempted);
        assert!(matches!(
            store.execution(other).unwrap().unwrap().status,
            ExecutionStatus::OutcomeUnknown { .. }
        ));
    }

    /// An action of another agent, attempted and so in flight until it is
    /// reconciled.
    fn in_flight(store: &mut Store) -> JournalId {
        let plan = store.create_plan(&objective("elsewhere")).unwrap();
        let agent = store
            .create_agent(Role::Planner, AgentScope::Plan(plan))
            .unwrap();
        let entry = store.intend(agent, &intent("rewrite", json!({}))).unwrap();
        store.act(entry, None).unwrap();
        entry
    }

    fn settle(store: &mut Store, entry: JournalId) {
        let evidence = [Evidence::Fact {
            name: "abandoned".into(),
        }];
        store
            .reconcile(entry, ActionOutcome::Failed, &evidence)
            .unwrap();
    }

    fn status(store: &Store, generation: GenerationId) -> ExecutionStatus {
        store.execution(generation).unwrap().unwrap().status
    }

    #[test]
    fn workspace_changes_amid_other_actions_are_still_attributed() {
        use ExecutionOutcome::*;
        let (_dir, mut store) = store();
        let scope = ["src/a.rs"];
        let baseline = entries(&[("src/a.rs", file(1))]);
        let a_changed = entries(&[("src/a.rs", file(2))]);
        let claim = strings(&scope);
        let result = reported(Reported::Succeeded, &claim);
        let run = |store: &mut Store, attempt: (GenerationId, ExecutionId, InvocationId)| {
            let (generation, execution, invocation) = attempt;
            store.finish_invocation(invocation, &succeeded()).unwrap();
            let outcome = store.finish_execution(execution, &observed(&a_changed, result));
            let captured = capture(store, generation);
            assert_eq!(outcome.unwrap(), captured.outcome);
            retire(store, generation);
            captured
        };

        // Another agent's action, or another execution, in flight while
        // this one acts writes nothing in its workspace, so casts no doubt
        // on what changed there.
        let rival = in_flight(&mut store);
        let attempt = attempted(&mut store, &scope, &baseline);
        let (other, ..) = attempted(&mut store, &[], &[]);
        let captured = run(&mut store, attempt);
        assert_eq!((captured.outcome, captured.attribution), (Candidate, None));
        assert_eq!(captured.install, Install::NotAttempted);
        settle(&mut store, rival);

        // The schema holds the same rule beneath `Store`: a consistent
        // candidate is accepted amid other actions, and a candidate with a
        // change beyond authority never is.
        let rival = in_flight(&mut store);
        let (generation, execution, invocation) = attempted(&mut store, &scope, &baseline);
        store.finish_invocation(invocation, &succeeded()).unwrap();
        let change = |path: &str, before: Option<u8>, authorized: bool| {
            let (kind, hash) = match before {
                Some(n) => ("file", Some(format!("{n:064x}"))),
                None => ("absent", None),
            };
            store.conn.execute(
                "INSERT INTO execution_changes VALUES (?1, ?2, ?3, ?4, 'file', ?5, ?6)",
                params![
                    execution,
                    path,
                    kind,
                    hash,
                    format!("{:064x}", 2),
                    authorized
                ],
            )
        };
        change("src/a.rs", Some(1), true).unwrap();
        change("src/b.rs", None, false).unwrap();
        let forge = |outcome: &str| {
            store.conn.execute(
                "INSERT INTO execution_captures VALUES (?1, ?2, NULL, 'succeeded', '[]', ?3, 0)",
                params![execution, outcome, HEAD],
            )
        };
        let message = forge("candidate").unwrap_err().to_string();
        assert!(message.contains("beyond authority"), "{message}");
        forge("scope_violated").unwrap();
        assert!(matches!(
            status(&store, generation),
            ExecutionStatus::OutcomeUnknown { .. }
        ));
        settle(&mut store, rival);
        assert!(matches!(
            status(&store, other),
            ExecutionStatus::OutcomeUnknown { .. }
        ));
    }

    #[test]
    fn raw_sql_cannot_promote_an_execution_to_candidate() {
        let (_dir, mut store) = store();
        let baseline = entries(&[("README.md", file(2)), ("src/a.rs", file(1))]);
        let promote = |store: &Store, execution: ExecutionId| {
            let sql = "UPDATE executions SET outcome = 'candidate' WHERE id = ?1";
            store
                .conn
                .execute(sql, [execution])
                .unwrap_err()
                .to_string()
        };
        let capture_as = |store: &Store, execution: ExecutionId, outcome: &str, head: &str| {
            let sql = "INSERT OR REPLACE INTO execution_captures
                       VALUES (?1, ?2, NULL, 'succeeded', '[]', ?3, 0)";
            let inserted = store.conn.execute(sql, params![execution, outcome, head]);
            inserted.map(drop).map_err(|e| e.to_string())
        };
        let change = |store: &Store, execution: ExecutionId, path: &str, before: u8, authorized| {
            let sql = "INSERT INTO execution_changes VALUES (?1, ?2, 'file', ?3, 'file', ?4, ?5)";
            let before = format!("{before:064x}");
            let after = format!("{:064x}", 9);
            let inserted = store
                .conn
                .execute(sql, params![execution, path, before, after, authorized]);
            inserted.map(drop).map_err(|e| e.to_string())
        };
        let refused = |result: Result<(), String>, expected: &str| {
            let message = result.unwrap_err();
            assert!(message.contains(expected), "{message}");
        };

        // Intended only.
        let (generation, execution, agent, entry) = intended(&mut store, &["src/a.rs"], &baseline);
        assert!(promote(&store, execution).contains("no such column: outcome"));
        let forged = capture_as(&store, execution, "candidate", HEAD);
        refused(forged, "only an attempted execution");
        assert_eq!(status(&store, generation), ExecutionStatus::Intended);

        // Attempted, its invocation still running.
        let invocation = store.start_invocation(agent, "claude", "m", None).unwrap();
        store.act(entry, Some(invocation)).unwrap();
        store.invocation_running(invocation).unwrap();
        assert!(promote(&store, execution).contains("no such column: outcome"));
        let forged = capture_as(&store, execution, "candidate", HEAD);
        refused(forged, "only an attempted execution");
        refused(change(&store, execution, "src/a.rs", 1, true), "derive");

        // The invocation ended, and nothing is captured: the outcome stays
        // unknown, and the journal cannot say otherwise.
        store.finish_invocation(invocation, &succeeded()).unwrap();
        assert!(promote(&store, execution).contains("no such column: outcome"));
        let unknown = ExecutionStatus::OutcomeUnknown {
            invocation: Some(invocation),
        };
        assert_eq!(status(&store, generation), unknown);
        let evidence = [Evidence::Invocation { invocation }];
        let reconciled = store.reconcile(entry, ActionOutcome::CompletedAsIntended, &evidence);
        assert!(err(reconciled).contains("reconciled only by its capture"));
        let sql = "UPDATE journal SET state = 'reconciled', outcome = 'completed_as_intended',
                   evidence = '[{\"kind\":\"fact\",\"name\":\"forged\"}]', reconciled_at = 1
                   WHERE id = ?1";
        let message = store.conn.execute(sql, [entry]).unwrap_err().to_string();
        assert!(
            message.contains("reconciled only by its capture"),
            "{message}"
        );

        // Evidence contradicting the baseline or the authority is refused,
        // and the baseline is fixed once attempted.
        refused(change(&store, execution, "src/a.rs", 7, true), "derive");
        refused(change(&store, execution, "README.md", 2, true), "derive");
        for sql in [
            "INSERT INTO execution_baseline VALUES (?1, 'src/new.rs', 'absent', NULL)",
            "INSERT OR REPLACE INTO execution_baseline VALUES (?1, 'src/a.rs', 'absent', NULL)",
        ] {
            let message = store
                .conn
                .execute(sql, [execution])
                .unwrap_err()
                .to_string();
            assert!(message.contains("precedes its attempt"), "{message}");
        }
        for sql in [
            "UPDATE execution_baseline SET kind = 'absent', hash = NULL",
            "DELETE FROM execution_baseline",
        ] {
            let message = store.conn.execute(sql, []).unwrap_err().to_string();
            assert!(message.contains("history is immutable"), "{message}");
        }

        // Consistent evidence of a change beyond authority leaves no
        // candidate to assert.
        change(&store, execution, "README.md", 2, false).unwrap();
        let forged = capture_as(&store, execution, "candidate", HEAD);
        refused(forged, "beyond authority");
        let forged = capture_as(&store, execution, "reported_failed", HEAD);
        refused(forged, "beyond authority");
        let forged = capture_as(&store, execution, "invocation_failed", HEAD);
        refused(forged, "contradicts how the invocation ended");

        // A capture the journal is not reconciled with establishes nothing.
        capture_as(&store, execution, "scope_violated", HEAD).unwrap();
        assert_eq!(status(&store, generation), unknown);
        refused(change(&store, execution, "src/a.rs", 1, true), "derive");
        let forged = capture_as(&store, execution, "candidate", HEAD);
        refused(forged, "beyond authority");
        let deviated = [Evidence::Invocation { invocation }];
        let reconciled = store.reconcile(entry, ActionOutcome::CompletedAsIntended, &deviated);
        assert!(err(reconciled).contains("reconciled only by its capture"));
        store
            .reconcile(entry, ActionOutcome::CompletedWithDeviation, &deviated)
            .unwrap();
        assert_eq!(
            capture(&store, generation).outcome,
            ExecutionOutcome::ScopeViolated
        );

        // A capture `Store` derived, a failure, is never rewritten into a
        // candidate, nor stripped of its evidence.
        retire(&mut store, generation);
        let (failed, execution, invocation) = attempted(&mut store, &["src/a.rs"], &baseline);
        store.finish_invocation(invocation, &succeeded()).unwrap();
        let mut after = baseline.clone();
        after[1].1 = file(3);
        let claim = strings(&["src/a.rs"]);
        let result = reported(Reported::Failed, &claim);
        store
            .finish_execution(execution, &observed(&after, result))
            .unwrap();
        let before = capture(&store, failed);
        assert_eq!(before.outcome, ExecutionOutcome::ReportedFailed);
        for sql in [
            "UPDATE execution_captures SET outcome = 'candidate', reported = 'succeeded'",
            "DELETE FROM execution_captures",
            "DELETE FROM execution_changes",
            "DELETE FROM execution_baseline",
            "UPDATE execution_changes SET authorized = 1",
            "DELETE FROM executions",
        ] {
            let message = store.conn.execute(sql, []).unwrap_err().to_string();
            assert!(message.contains("history is immutable"), "{sql}: {message}");
        }
        let forged = capture_as(&store, execution, "candidate", HEAD);
        refused(forged, "only an attempted execution");
        refused(change(&store, execution, "README.md", 2, false), "derive");
        let replace = "INSERT OR REPLACE INTO executions
                       SELECT id, generation_id, agent_id, journal_id, '[]', head, started_at
                       FROM executions WHERE id = ?1";
        let message = store.conn.execute(replace, [execution]).unwrap_err();
        assert!(message.to_string().contains("intended with its journal"));
        let reopen = "UPDATE journal SET state = 'attempted' WHERE id = (SELECT journal_id
                      FROM executions WHERE id = ?1)";
        let message = store.conn.execute(reopen, [execution]).unwrap_err();
        assert!(message.to_string().contains("journal history is immutable"));
        assert_eq!(capture(&store, failed), before);
    }

    /// A captured candidate changing `src/a.rs`, of a generation owning it.
    fn candidate(store: &mut Store) -> (GenerationId, ExecutionId) {
        let baseline = entries(&[("src/a.rs", file(1))]);
        let (generation, execution, invocation) = attempted(store, &["src/a.rs"], &baseline);
        store.finish_invocation(invocation, &succeeded()).unwrap();
        let after = entries(&[("src/a.rs", file(2))]);
        let claim = strings(&["src/a.rs"]);
        let result = reported(Reported::Succeeded, &claim);
        let outcome = store.finish_execution(execution, &observed(&after, result));
        assert_eq!(outcome.unwrap(), ExecutionOutcome::Candidate);
        (generation, execution)
    }

    fn install(store: &Store, generation: GenerationId) -> Install {
        capture(store, generation).install
    }

    #[test]
    fn installs_are_journaled_and_never_fabricated() {
        use InstallOutcome::*;
        let (_dir, mut store) = store();
        let (generation, execution) = candidate(&mut store);
        assert_eq!(install(&store, generation), Install::NotAttempted);

        // Intended: nothing written yet, and no result before the attempt.
        let entry = store.begin_install(execution).unwrap();
        assert_eq!(install(&store, generation), Install::NotAttempted);
        let refused = store.begin_install(execution);
        assert!(err(refused).contains("already installed"));
        let refused = store.finish_install(execution, Installed, &[]);
        assert!(err(refused).contains("follows its attempt"));

        // Attempted: until a result is recorded with the reconciliation,
        // what the working tree holds is unknown.
        store.act(entry, None).unwrap();
        assert_eq!(install(&store, generation), Install::OutcomeUnknown);
        let evidence = [Evidence::Fact {
            name: "forged".into(),
        }];
        for outcome in [ActionOutcome::CompletedAsIntended, ActionOutcome::Failed] {
            let reconciled = store.reconcile(entry, outcome, &evidence);
            assert!(err(reconciled).contains("reconciled only by its result"));
        }
        let paths = strings(&["src/a.rs"]);
        let refused = store.finish_install(execution, Drifted, &[]);
        assert!(err(refused).contains("exactly when an install drifted"));
        let refused = store.finish_install(execution, Installed, &paths);
        assert!(err(refused).contains("exactly when an install drifted"));
        let unchanged = strings(&["src/b.rs"]);
        let refused = store.finish_install(execution, Drifted, &unchanged);
        assert!(err(refused).contains("of changed paths"));
        assert_eq!(install(&store, generation), Install::OutcomeUnknown);

        store.finish_install(execution, Installed, &[]).unwrap();
        let Install::Finished {
            outcome: Installed,
            drifted,
            ..
        } = install(&store, generation)
        else {
            panic!("{:?}", install(&store, generation));
        };
        assert!(drifted.is_empty());
        let ActionStatus::Reconciled(_, reconciliation) =
            store.journal_entry(entry).unwrap().status
        else {
            panic!("reconciled with the result")
        };
        assert_eq!(reconciliation.outcome, ActionOutcome::CompletedAsIntended);
        assert_eq!(
            reconciliation.evidence,
            [Evidence::Fact {
                name: "install.installed".into()
            }]
        );
        // Ownership stays held, the generation active.
        assert_eq!(store.owned_paths(generation).unwrap(), ["src/a.rs"]);
        let refused = store.finish_install(execution, Failed, &[]);
        assert!(err(refused).contains("follows its attempt"));
        for sql in [
            "UPDATE execution_install_results SET outcome = 'drifted'",
            "DELETE FROM execution_install_results",
            "UPDATE execution_installs SET journal_id = 1",
            "DELETE FROM execution_installs",
        ] {
            let message = store.conn.execute(sql, []).unwrap_err().to_string();
            assert!(message.contains("history is immutable"), "{sql}: {message}");
        }
        retire(&mut store, generation);

        // Drifting installs nothing, which the journal says.
        let (generation, execution) = candidate(&mut store);
        let entry = store.begin_install(execution).unwrap();
        store.act(entry, None).unwrap();
        store.finish_install(execution, Drifted, &paths).unwrap();
        let Install::Finished {
            outcome: Drifted,
            drifted,
            ..
        } = install(&store, generation)
        else {
            panic!()
        };
        assert_eq!(drifted, paths);
        let ActionStatus::Reconciled(_, reconciliation) =
            store.journal_entry(entry).unwrap().status
        else {
            panic!()
        };
        assert_eq!(reconciliation.outcome, ActionOutcome::Failed);
        retire(&mut store, generation);

        // Declining, found before anything is written, attempts and
        // reconciles at once, and only as drifted or refused.
        let (generation, execution) = candidate(&mut store);
        let entry = store.begin_install(execution).unwrap();
        for (outcome, drifted) in [(Installed, &[][..]), (Failed, &[]), (Drifted, &[])] {
            assert!(store.decline_install(execution, outcome, drifted).is_err());
        }
        assert_eq!(install(&store, generation), Install::NotAttempted);
        store.decline_install(execution, Refused, &[]).unwrap();
        let ActionStatus::Reconciled(_, reconciliation) =
            store.journal_entry(entry).unwrap().status
        else {
            panic!()
        };
        assert_eq!(reconciliation.outcome, ActionOutcome::Failed);
        assert!(matches!(
            install(&store, generation),
            Install::Finished {
                outcome: Refused,
                ..
            }
        ));
        let refused = store.decline_install(execution, Refused, &[]);
        assert!(err(refused).contains("intended"));
        retire(&mut store, generation);

        // A result the journal is not reconciled with establishes nothing.
        let (generation, execution) = candidate(&mut store);
        let entry = store.begin_install(execution).unwrap();
        store.act(entry, None).unwrap();
        store
            .conn
            .execute(
                "INSERT INTO execution_install_results VALUES (?1, 'installed', NULL, 1)",
                [execution],
            )
            .unwrap();
        assert_eq!(install(&store, generation), Install::OutcomeUnknown);
        retire(&mut store, generation);
    }

    #[test]
    fn only_owned_captured_candidates_are_installed() {
        let (_dir, mut store) = store();
        let forge = |store: &mut Store, execution: ExecutionId, generation: GenerationId| {
            let agent = store.execution(generation).unwrap().unwrap().agent;
            let entry = store
                .intend(agent, &intent("executor.install", json!({})))
                .unwrap();
            let sql = "INSERT INTO execution_installs VALUES (?1, ?2)";
            store
                .conn
                .execute(sql, params![execution, entry])
                .unwrap_err()
                .to_string()
        };

        // Not captured, or captured as anything but a candidate.
        let baseline = entries(&[("src/a.rs", file(1))]);
        let (generation, execution, invocation) = attempted(&mut store, &["src/a.rs"], &baseline);
        let refused = store.begin_install(execution);
        assert!(err(refused).contains("no captured candidate"));
        assert!(forge(&mut store, execution, generation).contains("only a captured candidate"));
        store.finish_invocation(invocation, &succeeded()).unwrap();
        let claim = strings(&["src/a.rs"]);
        let result = reported(Reported::Failed, &claim);
        store
            .finish_execution(execution, &observed(&baseline, result))
            .unwrap();
        let refused = store.begin_install(execution);
        assert!(err(refused).contains("no captured candidate"));
        assert!(forge(&mut store, execution, generation).contains("only a captured candidate"));
        retire(&mut store, generation);

        // A candidate whose generation no longer owns its scope, or ended.
        let (generation, execution) = candidate(&mut store);
        store.release_ownership(generation).unwrap_err();
        store
            .finish_generation(generation, crate::state::GenerationEnd::Failed)
            .unwrap();
        let refused = store.begin_install(execution);
        assert!(err(refused).contains("already ended"));
        assert!(forge(&mut store, execution, generation).contains("only a captured candidate"));
        store.release_ownership(generation).unwrap();
        assert_eq!(install(&store, generation), Install::NotAttempted);
    }
}
