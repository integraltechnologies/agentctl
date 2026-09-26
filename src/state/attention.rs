//! Attention: the durable boundary where a plan stops for a human's
//! engineering decision, because continuing needs judgment or authority
//! agentctl does not have.
//!
//! Only a plan's planner raises a concern, as a command of a replan (see
//! `crate::planner`), validated and recorded in the replan's transaction,
//! which makes the plan need attention. A concern is known by its key,
//! which its plan raises once: however its prose changes, a concern a
//! human decided is never raised again as if undecided, while a concern
//! under another key still stops the plan. Nothing decides whether two
//! concerns mean the same beyond that.
//!
//! A plan that needs attention is never claimed (the claim requires a
//! running plan), so no new work of it starts, while every other plan runs
//! on. Work already claimed is not preempted: a provider already running
//! runs to its end and is recorded as it ends, and the pipeline stops at the
//! next step that requires a ready or running plan (starting an executor,
//! accepting), as for a paused plan. Nothing accepted, recorded or owned is
//! rewritten.
//!
//! Only a human decides a concern, once ([`Store::decide`], which planner
//! output never reaches): the schema refuses a decision while the plan does
//! not need attention, or while any planner of it acts, and never lets one
//! change. Accepting lets the plan continue unchanged despite the concern;
//! once no concern blocks it and no instruction awaits its planner, it runs
//! again in that very transaction. An instruction is for the planner: the
//! plan still needs attention until a replan, given the decision in its
//! input, acts on it, possibly by changing nothing. Stopping authorizes no
//! autonomous continuation: the plan needs attention for good, and a
//! changed intent is a new plan.

use anyhow::{Context, Result, bail, ensure};
use rusqlite::{Connection, OptionalExtension, Transaction, params};

use super::planning::{check_key, lookup};
use super::{
    ConcernId, PlanId, PlanState, ReplanId, Store, check_text, event, json_column, now, plan_state,
};

/// Bounds on a concern and a human's instruction.
const REASON_LIMIT: usize = 4096;
const EVIDENCE_LIMIT: usize = 16;
const EVIDENCE_TEXT_LIMIT: usize = 1024;
const TASKS_LIMIT: usize = 256;
const INSTRUCTION_LIMIT: usize = 16 * 1024;

/// A human's decision on one concern.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HumanDecision {
    /// The plan may continue unchanged despite the concern.
    Accept,
    /// Authoritative guidance for the planner to act on, exactly as given.
    Instruct(String),
    /// No autonomous continuation of the plan.
    Stop,
}

impl HumanDecision {
    fn kind(&self) -> &'static str {
        match self {
            Self::Accept => "accept",
            Self::Instruct(_) => "instruct",
            Self::Stop => "stop",
        }
    }
}

/// A concern a planner raised, as recorded, with its human's decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Concern {
    pub id: ConcernId,
    pub plan: PlanId,
    /// Its canonical identity within its plan.
    pub key: String,
    pub reason: String,
    pub evidence: Vec<String>,
    /// The keys of the tasks it affects.
    pub tasks: Vec<String>,
    /// The replan that raised it.
    pub replan: ReplanId,
    pub raised_at: i64,
    /// Once decided, how and when.
    pub decision: Option<(HumanDecision, i64)>,
}

impl Concern {
    /// Whether it stops its plan: undecided, or decided `Stop`.
    pub fn blocks(&self) -> bool {
        matches!(self.decision, None | Some((HumanDecision::Stop, _)))
    }
}

/// What recording a human decision established.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use]
pub enum Decided {
    /// Recorded; `resumed` when the plan runs again because of it.
    Recorded { resumed: bool },
    /// This very decision was recorded before; nothing changed.
    AlreadyRecorded,
}

impl Store {
    /// Every concern raised for `plan`, in order, with its decision.
    pub fn attention(&self, plan: PlanId) -> Result<Vec<Concern>> {
        plan_state(&self.conn, plan)?;
        self.conn
            .prepare(
                "SELECT c.id, c.key, c.reason, c.evidence, c.tasks, c.replan_id, c.raised_at,
                     d.kind, d.instruction, d.decided_at
                 FROM attention_concerns c
                 LEFT JOIN attention_decisions d ON d.concern_id = c.id
                 WHERE c.plan_id = ?1 ORDER BY c.id",
            )?
            .query_map([plan], |r| {
                let kind: Option<String> = r.get(7)?;
                let decision = match kind.as_deref() {
                    None => None,
                    Some("accept") => Some(HumanDecision::Accept),
                    Some("instruct") => Some(HumanDecision::Instruct(r.get(8)?)),
                    Some(_) => Some(HumanDecision::Stop),
                };
                Ok(Concern {
                    id: r.get(0)?,
                    plan,
                    key: r.get(1)?,
                    reason: r.get(2)?,
                    evidence: json_column(r, 3)?,
                    tasks: json_column(r, 4)?,
                    replan: r.get(5)?,
                    raised_at: r.get(6)?,
                    decision: match decision {
                        Some(d) => Some((d, r.get(9)?)),
                        None => None,
                    },
                })
            })?
            .collect::<rusqlite::Result<_>>()
            .map_err(Into::into)
    }

    /// Records a human's decision on `concern` of `plan`, in one
    /// transaction: see the module documentation. This is the human's path
    /// alone; nothing a provider proposes calls it. Deciding again exactly
    /// as decided changes nothing; any other decision of a decided concern
    /// is refused, as is one while a planner of the plan acts.
    pub fn decide(
        &mut self,
        plan: PlanId,
        concern: ConcernId,
        decision: &HumanDecision,
    ) -> Result<Decided> {
        if let HumanDecision::Instruct(text) = decision {
            check_text("an instruction", text, INSTRUCTION_LIMIT, true)?;
        }
        self.write(|tx| {
            let found: Option<(PlanId, String)> = tx
                .query_row(
                    "SELECT plan_id, key FROM attention_concerns WHERE id = ?1",
                    [concern],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            let (of, key) = found.with_context(|| format!("concern {concern} does not exist"))?;
            ensure!(
                of == plan,
                "concern {concern} is plan {of}'s, not plan {plan}'s"
            );
            let decided = self::decided(tx, concern)?;
            if let Some(earlier) = decided {
                ensure!(
                    earlier == *decision,
                    "concern {concern} (`{key}`) was already decided ({}); a human decision \
                     is never changed",
                    earlier.kind()
                );
                return Ok(Decided::AlreadyRecorded);
            }
            let acting: bool = tx.query_row(
                "SELECT EXISTS (SELECT 1 FROM agents a JOIN invocations i ON i.agent_id = a.id
                     WHERE a.role = 'planner' AND a.plan_id = ?1 AND i.ended_at IS NULL)
                 OR EXISTS (SELECT 1 FROM agents a JOIN journal j ON j.agent_id = a.id
                     WHERE a.role = 'planner' AND a.plan_id = ?1 AND j.state = 'attempted')",
                [plan],
                |r| r.get(0),
            )?;
            ensure!(
                !acting,
                "plan {plan}'s planner is acting, so nothing is decided until it ends"
            );
            let instruction = match decision {
                HumanDecision::Instruct(text) => Some(text.as_str()),
                _ => None,
            };
            tx.execute(
                "INSERT INTO attention_decisions
                   (concern_id, plan_id, kind, instruction, replan_id, decided_at)
                 VALUES (?1, ?2, ?3, ?4, (SELECT max(id) FROM replans WHERE plan_id = ?2), ?5)",
                params![concern, plan, decision.kind(), instruction, now()],
            )
            .with_context(|| format!("deciding concern {concern} of plan {plan}"))?;
            let detail = format!("concern {concern} `{key}`: {}", decision.kind());
            event(tx, "attention.decided", Some(plan), None, None, &detail)?;
            Ok(Decided::Recorded {
                resumed: resume(tx, plan)?,
            })
        })
    }
}

/// The decision recorded for `concern`, if any.
fn decided(conn: &Connection, concern: ConcernId) -> Result<Option<HumanDecision>> {
    let found: Option<(String, Option<String>)> = conn
        .query_row(
            "SELECT kind, instruction FROM attention_decisions WHERE concern_id = ?1",
            [concern],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    Ok(
        found.map(|(kind, instruction)| match (kind.as_str(), instruction) {
            ("accept", _) => HumanDecision::Accept,
            ("instruct", Some(text)) => HumanDecision::Instruct(text),
            _ => HumanDecision::Stop,
        }),
    )
}

/// Whether a concern of `plan` blocks it: undecided, or decided `stop`.
pub(super) fn blocked(conn: &Connection, plan: PlanId) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM attention_concerns c
             LEFT JOIN attention_decisions d ON d.concern_id = c.id
             WHERE c.plan_id = ?1 AND coalesce(d.kind, 'stop') = 'stop')",
        [plan],
        |r| r.get(0),
    )?)
}

/// Raises concern `key` of `plan` for `replan`, its latest, with `reason`,
/// `evidence` and the keys of the `tasks` it affects: see the module
/// documentation. The schema makes the plan need attention as the concern
/// is inserted, which is only recorded here.
pub(super) fn raise(
    tx: &Transaction,
    plan: PlanId,
    replan: ReplanId,
    key: &str,
    reason: &str,
    evidence: &[String],
    tasks: &[String],
) -> Result<()> {
    check_key("concern", key)?;
    check_text("a concern's reason", reason, REASON_LIMIT, true)?;
    ensure!(
        !evidence.is_empty() && evidence.len() <= EVIDENCE_LIMIT,
        "a concern gives 1 to {EVIDENCE_LIMIT} items of evidence"
    );
    for item in evidence {
        check_text("evidence", item, EVIDENCE_TEXT_LIMIT, true)?;
    }
    ensure!(
        tasks.len() <= TASKS_LIMIT,
        "a concern affects at most {TASKS_LIMIT} tasks"
    );
    for (i, task) in tasks.iter().enumerate() {
        lookup(tx, plan, task)?;
        ensure!(!tasks[..i].contains(task), "task `{task}` is listed twice");
    }
    let state = plan_state(tx, plan)?;
    ensure!(
        matches!(
            state,
            PlanState::Ready | PlanState::Running | PlanState::NeedsAttention
        ),
        "plan {plan} is {state}: its human stopped it already"
    );
    let earlier: Option<ConcernId> = tx
        .query_row(
            "SELECT id FROM attention_concerns WHERE plan_id = ?1 AND key = ?2",
            params![plan, key],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(earlier) = earlier {
        match decided(tx, earlier)? {
            Some(decision) => bail!(
                "concern `{key}` was raised before and its human decided it ({}): a decided \
                 concern is never raised again; a materially different one is raised under a \
                 key of its own",
                decision.kind()
            ),
            None => bail!("concern `{key}` was raised already and awaits its human's decision"),
        }
    }
    tx.execute(
        "INSERT INTO attention_concerns
           (plan_id, key, reason, evidence, tasks, replan_id, raised_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            plan,
            key,
            reason,
            serde_json::to_string(evidence)?,
            serde_json::to_string(tasks)?,
            replan,
            now()
        ],
    )?;
    let concern = ConcernId(tx.last_insert_rowid());
    let detail = format!("concern {concern} `{key}` by replan {replan}");
    event(tx, "attention.raised", Some(plan), None, None, &detail)?;
    if state != PlanState::NeedsAttention {
        state_event(tx, plan, state, PlanState::NeedsAttention)?;
    }
    Ok(())
}

/// Runs `plan` again if it needs attention while no concern blocks it and
/// no instruction awaits its planner, returning whether it did.
pub(super) fn resume(tx: &Transaction, plan: PlanId) -> Result<bool> {
    let from = plan_state(tx, plan)?;
    let pending: bool = tx.query_row(
        "SELECT EXISTS (SELECT 1 FROM attention_decisions WHERE plan_id = ?1
             AND kind = 'instruct'
             AND replan_id = (SELECT max(id) FROM replans WHERE plan_id = ?1))",
        [plan],
        |r| r.get(0),
    )?;
    if from != PlanState::NeedsAttention || pending || blocked(tx, plan)? {
        return Ok(false);
    }
    set_state(tx, plan, from, PlanState::Running)?;
    event(tx, "attention.resolved", Some(plan), None, None, "running")?;
    Ok(true)
}

fn set_state(tx: &Transaction, plan: PlanId, from: PlanState, to: PlanState) -> Result<()> {
    tx.execute(
        "UPDATE plans SET state = ?2, updated_at = ?3 WHERE id = ?1",
        params![plan, to, now()],
    )?;
    state_event(tx, plan, from, to)
}

fn state_event(tx: &Transaction, plan: PlanId, from: PlanState, to: PlanState) -> Result<()> {
    event(
        tx,
        "plan.state",
        Some(plan),
        None,
        None,
        &format!("{from} -> {to}"),
    )
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU32;
    use std::sync::Barrier;
    use std::thread;

    use super::*;
    use crate::planner::{Command, Rejection};
    use crate::state::tests::{err, ready_plan, store, version};
    use crate::state::{
        Claim, GenerationId, GenerationState, Intent, PipelineOutcome, Release, Replan, TaskId,
    };

    const LIMIT: NonZeroU32 = NonZeroU32::new(8).unwrap();

    fn raising(key: &str, reason: &str, tasks: &[&str]) -> Command {
        Command::RaiseAttention {
            concern: key.into(),
            reason: reason.into(),
            evidence: vec![format!("evidence for {key}")],
            tasks: tasks.iter().map(|t| t.to_string()).collect(),
        }
    }

    fn update(task: &str, objective: &str) -> Command {
        Command::UpdateTask {
            task: task.into(),
            objective: Some(objective.into()),
            context: None,
            paths: None,
        }
    }

    fn replan(store: &mut Store, plan: PlanId, commands: &[Command]) -> Result<Replan> {
        let basis = store.replan_basis(plan).unwrap();
        store.replan(plan, &basis, None, commands, &|_| Ok(()), &|_| {
            Ok(Vec::new())
        })
    }

    fn applied(store: &mut Store, plan: PlanId, commands: &[Command]) {
        assert!(matches!(
            replan(store, plan, commands).unwrap(),
            Replan::Applied(_)
        ));
    }

    /// A running plan of tasks `(key, scope, depends_on)`, and their ids.
    fn running(store: &mut Store, tasks: &[(&str, &[&str], &[&str])]) -> (PlanId, Vec<TaskId>) {
        let (plan, ids) = ready_plan(store, tasks);
        store.start_plan(plan).unwrap();
        (plan, ids)
    }

    fn state(store: &Store, plan: PlanId) -> PlanState {
        store.plan(plan).unwrap().state
    }

    fn concern(store: &Store, plan: PlanId, key: &str) -> Concern {
        store
            .attention(plan)
            .unwrap()
            .into_iter()
            .find(|c| c.key == key)
            .unwrap()
    }

    fn events(store: &Store, kind: &str) -> Vec<String> {
        store
            .events_after(0, 10_000)
            .unwrap()
            .into_iter()
            .filter(|e| e.kind == kind)
            .map(|e| e.detail)
            .collect()
    }

    fn forged(store: &Store, sql: &str, expected: &str) {
        let message = store.raw().execute_batch(sql).unwrap_err().to_string();
        assert!(message.contains(expected), "{sql}: {message}");
    }

    /// The rows the scheduler and acceptance recorded, which attention
    /// never touches.
    fn work(store: &Store) -> Vec<Vec<rusqlite::types::Value>> {
        let mut rows = Vec::new();
        for table in [
            "generations",
            "ownership",
            "scheduler_claims",
            "scheduler_releases",
            "accepted_sources",
            "tasks",
            "task_scope",
        ] {
            let conn = store.raw();
            let mut stmt = conn.prepare(&format!("SELECT * FROM {table}")).unwrap();
            let columns = stmt.column_count();
            let found: Vec<Vec<rusqlite::types::Value>> = stmt
                .query_map([], |r| (0..columns).map(|i| r.get(i)).collect())
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap();
            rows.extend(found);
        }
        rows
    }

    #[test]
    fn a_raised_concern_stops_its_plan_alone_and_durably() {
        let (dir, mut store) = store();
        let (plan, ids) = running(
            &mut store,
            &[("a", &["src/a.rs"], &[]), ("b", &["src/b.rs"], &[])],
        );
        let (other, others) = running(&mut store, &[("c", &["src/c.rs"], &[])]);
        // Accepted work of the plan, and a generation in flight.
        let accepted = store.start_generation(ids[0]).unwrap();
        store.accept_generation(accepted, &[]).unwrap();
        let before = work(&store);

        applied(
            &mut store,
            plan,
            &[raising("schema", "Dropping the column loses data", &["b"])],
        );
        assert_eq!(state(&store, plan), PlanState::NeedsAttention);
        let raised = concern(&store, plan, "schema");
        assert_eq!(
            (
                &*raised.reason,
                &raised.evidence,
                &raised.tasks,
                &raised.decision
            ),
            (
                "Dropping the column loses data",
                &vec!["evidence for schema".to_owned()],
                &vec!["b".to_owned()],
                &None
            )
        );
        assert!(raised.blocks());
        assert_eq!(events(&store, "attention.raised").len(), 1);
        // Durable: a fresh connection finds it.
        drop(store);
        let mut store = Store::open(&dir.path().join("state.db")).unwrap();
        assert_eq!(store.attention(plan).unwrap(), vec![raised]);

        // No new work of it is claimed, beneath `Store` too.
        assert_eq!(
            store.claim(ids[1], LIMIT).unwrap(),
            Claim::PlanNotRunning(PlanState::NeedsAttention)
        );
        store.raw().execute_batch("BEGIN").unwrap();
        store
            .raw()
            .execute(
                "INSERT INTO generations (task_id, number, state, started_at)
                 VALUES (?1, 1, 'active', 0)",
                [ids[1]],
            )
            .unwrap();
        let generation = GenerationId(store.raw().last_insert_rowid());
        let message = store
            .raw()
            .execute(
                "INSERT INTO scheduler_claims (generation_id, task_id, capacity, claimed_at)
                 VALUES (?1, ?2, 8, 0)",
                rusqlite::params![generation, ids[1]],
            )
            .unwrap_err()
            .to_string();
        assert!(message.contains("only an eligible task"), "{message}");
        store.raw().execute_batch("ROLLBACK").unwrap();
        // Accepted work and history stand untouched.
        assert_eq!(work(&store), before);
        assert_eq!(
            store.generations(ids[0]).unwrap()[0].state,
            GenerationState::Accepted
        );
        // Another plan runs on.
        assert_eq!(state(&store, other), PlanState::Running);
        assert!(matches!(
            store.claim(others[0], LIMIT).unwrap(),
            Claim::Claimed(_)
        ));
        // Nor does `set_plan_state` enter or leave attention.
        assert!(err(store.set_plan_state(plan, PlanState::Running)).contains("cannot go"));
        assert!(err(store.set_plan_state(other, PlanState::NeedsAttention)).contains("cannot go"));
        forged(
            &store,
            &format!("UPDATE plans SET state = 'running' WHERE id = {plan}"),
            "a plan needs attention exactly while",
        );
        forged(
            &store,
            &format!("UPDATE plans SET state = 'needs_attention' WHERE id = {other}"),
            "a plan needs attention exactly while",
        );
        // A fresh store holds the canonical schema, still version 1.
        assert_eq!(version(&dir.path().join("state.db")), 1);
    }

    #[test]
    fn work_in_flight_is_never_rewritten_by_attention() {
        let (_dir, mut store) = store();
        let (plan, ids) = running(
            &mut store,
            &[("a", &["src/a.rs"], &[]), ("b", &["src/b.rs"], &[])],
        );
        let Claim::Claimed(generation) = store.claim(ids[0], LIMIT).unwrap() else {
            panic!("not claimed");
        };
        let before = work(&store);
        let ended = events(&store, "generation.ended").len();

        applied(&mut store, plan, &[raising("scope", "Unclear", &["b"])]);
        // The claimed generation stays active, owning its scope and holding
        // its claim: nothing preempts or rewrites it.
        assert_eq!(work(&store), before);
        assert_eq!(events(&store, "generation.ended").len(), ended);
        assert_eq!(
            store.generations(ids[0]).unwrap()[0].state,
            GenerationState::Active
        );
        assert_eq!(
            store.owned_paths(generation).unwrap(),
            vec!["src/a.rs".to_owned()]
        );
        // Nor is it replanned beneath the human.
        assert!(err(replan(&mut store, plan, &[update("b", "x")])).contains("awaits a human"));
        // Its pipeline still releases its claim as it ended.
        assert_eq!(
            store.release_claim(generation).unwrap(),
            Release::Released(PipelineOutcome::NotExecuted)
        );
        assert_eq!(
            store.claim(ids[1], LIMIT).unwrap(),
            Claim::PlanNotRunning(PlanState::NeedsAttention)
        );
    }

    #[test]
    fn accepting_settles_exactly_that_concern() {
        let (_dir, mut store) = store();
        let (plan, ids) = running(
            &mut store,
            &[("a", &["src/a.rs"], &[]), ("b", &["src/b.rs"], &[])],
        );
        applied(
            &mut store,
            plan,
            &[
                raising("naming", "Ambiguous naming", &["a"]),
                raising("deletion", "Deletes user data", &["b"]),
            ],
        );
        let naming = concern(&store, plan, "naming").id;
        let deletion = concern(&store, plan, "deletion").id;
        assert_eq!(
            store.decide(plan, naming, &HumanDecision::Accept).unwrap(),
            Decided::Recorded { resumed: false }
        );
        // The other concern still blocks.
        assert_eq!(state(&store, plan), PlanState::NeedsAttention);
        assert!(!concern(&store, plan, "naming").blocks());
        assert!(concern(&store, plan, "deletion").blocks());
        // Repeating it changes nothing; changing it is refused.
        assert_eq!(
            store.decide(plan, naming, &HumanDecision::Accept).unwrap(),
            Decided::AlreadyRecorded
        );
        assert!(err(store.decide(plan, naming, &HumanDecision::Stop)).contains("never changed"));
        assert_eq!(
            store
                .decide(plan, deletion, &HumanDecision::Accept)
                .unwrap(),
            Decided::Recorded { resumed: true }
        );
        assert_eq!(state(&store, plan), PlanState::Running);
        assert_eq!(events(&store, "attention.decided").len(), 2);
        assert_eq!(events(&store, "attention.resolved").len(), 1);
        assert!(matches!(
            store.claim(ids[0], LIMIT).unwrap(),
            Claim::Claimed(_)
        ));

        // The accepted concern is never raised again, whatever its prose.
        let replayed = err(replan(
            &mut store,
            plan,
            &[raising("naming", "Reworded: names are ambiguous", &[])],
        ));
        assert!(replayed.contains("never raised again"), "{replayed}");
        assert_eq!(state(&store, plan), PlanState::Running);
        assert_eq!(store.attention(plan).unwrap().len(), 2);
        // A materially different concern, under a key of its own, still
        // stops the plan.
        applied(
            &mut store,
            plan,
            &[raising("naming-api", "The public API name conflicts", &[])],
        );
        assert_eq!(state(&store, plan), PlanState::NeedsAttention);
    }

    #[test]
    fn an_instruction_waits_for_its_planner_and_stop_stops_for_good() {
        let (_dir, mut store) = store();
        let (plan, ids) = running(&mut store, &[("a", &["src/a.rs"], &[])]);
        applied(
            &mut store,
            plan,
            &[raising("approach", "Two designs", &["a"])],
        );
        let approach = concern(&store, plan, "approach").id;
        let text = "Use the second design.\n  Keep `a` small.";
        let decided = store.decide(plan, approach, &HumanDecision::Instruct(text.into()));
        assert_eq!(decided.unwrap(), Decided::Recorded { resumed: false });
        // Preserved exactly, and the plan waits for its planner.
        assert_eq!(
            concern(&store, plan, "approach").decision.unwrap().0,
            HumanDecision::Instruct(text.into())
        );
        assert_eq!(state(&store, plan), PlanState::NeedsAttention);
        assert_eq!(
            store.claim(ids[0], LIMIT).unwrap(),
            Claim::PlanNotRunning(PlanState::NeedsAttention)
        );
        forged(
            &store,
            &format!("UPDATE plans SET state = 'running' WHERE id = {plan}"),
            "a plan needs attention exactly while",
        );
        // A replan acting on it, even changing nothing, runs the plan again.
        applied(&mut store, plan, &[]);
        assert_eq!(state(&store, plan), PlanState::Running);
        // Only acting on decisions proposes no command.
        assert!(err(replan(&mut store, plan, &[])).contains("proposes no command"));

        applied(&mut store, plan, &[raising("wipe", "Needs a wipe", &[])]);
        let wipe = concern(&store, plan, "wipe").id;
        assert_eq!(
            store.decide(plan, wipe, &HumanDecision::Stop).unwrap(),
            Decided::Recorded { resumed: false }
        );
        assert_eq!(state(&store, plan), PlanState::NeedsAttention);
        assert!(concern(&store, plan, "wipe").blocks());
        // Nothing continues it: no replan, no claim, no state change.
        assert!(err(replan(&mut store, plan, &[])).contains("awaits a human"));
        assert!(err(replan(&mut store, plan, &[update("a", "x")])).contains("awaits a human"));
        assert!(err(store.set_plan_state(plan, PlanState::Running)).contains("cannot go"));
        forged(
            &store,
            &format!("UPDATE plans SET state = 'running' WHERE id = {plan}"),
            "a plan needs attention exactly while",
        );
        assert!(err(store.decide(plan, wipe, &HumanDecision::Accept)).contains("never changed"));
        assert_eq!(state(&store, plan), PlanState::NeedsAttention);
    }

    #[test]
    fn stale_and_mismatched_decisions_are_refused() {
        let (_dir, mut store) = store();
        let (a, _) = running(&mut store, &[("a", &["src/a.rs"], &[])]);
        let (b, _) = running(&mut store, &[("b", &["src/b.rs"], &[])]);
        applied(&mut store, a, &[raising("x", "Concern of a", &[])]);
        applied(&mut store, b, &[raising("x", "Concern of b", &[])]);
        let of_a = concern(&store, a, "x").id;
        let of_b = concern(&store, b, "x").id;
        // Resolving a's concern never touches b, nor names b.
        assert!(err(store.decide(b, of_a, &HumanDecision::Accept)).contains("not plan"));
        assert!(err(store.decide(a, ConcernId(99), &HumanDecision::Accept)).contains("not exist"));
        let blank = HumanDecision::Instruct(" \n".into());
        assert!(err(store.decide(a, of_a, &blank)).contains("must not be blank"));
        forged(
            &store,
            &format!(
                "INSERT INTO attention_decisions (concern_id, plan_id, kind, replan_id, decided_at)
                 VALUES ({of_a}, {b}, 'accept', (SELECT max(id) FROM replans WHERE plan_id = {b}), 0)"
            ),
            "FOREIGN KEY",
        );
        let decided = store.decide(a, of_a, &HumanDecision::Accept).unwrap();
        assert_eq!(decided, Decided::Recorded { resumed: true });
        assert_eq!(state(&store, a), PlanState::Running);
        assert_eq!(state(&store, b), PlanState::NeedsAttention);
        assert!(concern(&store, b, "x").blocks());

        // A decision of a concern no longer outstanding is refused, beneath
        // `Store` too, and history is never rewritten or replaced.
        assert!(err(store.decide(a, of_a, &HumanDecision::Stop)).contains("never changed"));
        for sql in [
            format!(
                "INSERT INTO attention_decisions (concern_id, plan_id, kind, replan_id, decided_at)
                 VALUES ({of_a}, {a}, 'stop', (SELECT max(id) FROM replans WHERE plan_id = {a}), 0)"
            ),
            format!(
                "INSERT OR REPLACE INTO attention_decisions
                   (concern_id, plan_id, kind, replan_id, decided_at)
                 VALUES ({of_a}, {a}, 'stop', (SELECT max(id) FROM replans WHERE plan_id = {a}), 0)"
            ),
        ] {
            forged(&store, &sql, "a human decides an undecided concern once");
        }
        forged(
            &store,
            "UPDATE attention_decisions SET kind = 'stop'",
            "human decisions are immutable",
        );
        forged(
            &store,
            "DELETE FROM attention_decisions",
            "human decisions are immutable",
        );
        forged(
            &store,
            "UPDATE attention_concerns SET reason = 'other'",
            "concerns are immutable",
        );
        forged(
            &store,
            "DELETE FROM attention_concerns",
            "concerns are immutable",
        );
        forged(
            &store,
            &format!(
                "INSERT OR REPLACE INTO attention_concerns
                   (id, plan_id, key, reason, evidence, tasks, replan_id, raised_at)
                 VALUES ({of_b}, {b}, 'x', 'r', '[]', '[]',
                     (SELECT max(id) FROM replans WHERE plan_id = {b}), 0)"
            ),
            "a concern is raised once",
        );
        assert_eq!(
            concern(&store, a, "x").decision.unwrap().0,
            HumanDecision::Accept
        );
    }

    #[test]
    fn racing_decisions_record_exactly_one() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        let mut store = Store::open(&path).unwrap();
        let (plan, _) = running(&mut store, &[("a", &["src/a.rs"], &[])]);
        applied(&mut store, plan, &[raising("risk", "Risky", &[])]);
        let risk = concern(&store, plan, "risk").id;
        let barrier = Barrier::new(2);
        let results: Vec<(HumanDecision, Result<Decided>)> = thread::scope(|s| {
            let racers: Vec<_> = [HumanDecision::Accept, HumanDecision::Stop]
                .into_iter()
                .map(|decision| {
                    let (path, barrier) = (&path, &barrier);
                    s.spawn(move || {
                        let mut store = Store::open(path).unwrap();
                        barrier.wait();
                        let result = store.decide(plan, risk, &decision);
                        (decision, result)
                    })
                })
                .collect();
            racers.into_iter().map(|r| r.join().unwrap()).collect()
        });
        let won: Vec<&HumanDecision> = results
            .iter()
            .filter(|(_, r)| r.as_ref().is_ok_and(|d| *d != Decided::AlreadyRecorded))
            .map(|(d, _)| d)
            .collect();
        assert_eq!(won.len(), 1, "{results:?}");
        let lost = results.iter().find(|(_, r)| r.is_err()).unwrap();
        assert!(format!("{:#}", lost.1.as_ref().unwrap_err()).contains("never changed"));
        assert_eq!(concern(&store, plan, "risk").decision.unwrap().0, *won[0]);
        let expected = match won[0] {
            HumanDecision::Accept => PlanState::Running,
            _ => PlanState::NeedsAttention,
        };
        assert_eq!(state(&store, plan), expected);
    }

    #[test]
    fn proposals_made_before_a_decision_are_stale() {
        let (_dir, mut store) = store();
        let (plan, _) = running(&mut store, &[("a", &["src/a.rs"], &[])]);
        let before_raise = store.replan_basis(plan).unwrap();
        applied(
            &mut store,
            plan,
            &[
                raising("one", "First", &["a"]),
                raising("two", "Second", &["a"]),
            ],
        );
        let (one, two) = (
            concern(&store, plan, "one").id,
            concern(&store, plan, "two").id,
        );
        let instruct = HumanDecision::Instruct("Do it this way".into());
        let decided = store.decide(plan, one, &instruct).unwrap();
        assert_eq!(decided, Decided::Recorded { resumed: false });
        let before_decision = store.replan_basis(plan).unwrap();
        let decided = store.decide(plan, two, &HumanDecision::Accept).unwrap();
        assert_eq!(decided, Decided::Recorded { resumed: false });
        // An instruction awaits the planner: the plan still needs attention.
        assert_eq!(state(&store, plan), PlanState::NeedsAttention);
        for basis in [&before_raise, &before_decision] {
            let stale = store.replan(plan, basis, None, &[update("a", "x")], &|_| Ok(()), &|_| {
                Ok(Vec::new())
            });
            assert_eq!(stale.unwrap(), Replan::Stale);
        }
        assert_eq!(state(&store, plan), PlanState::NeedsAttention);
        applied(&mut store, plan, &[update("a", "Done the human's way")]);
        assert_eq!(state(&store, plan), PlanState::Running);
        // Replaying a proposal of the decided state is stale too.
        let replay = store.replan(plan, &before_decision, None, &[], &|_| Ok(()), &|_| {
            Ok(Vec::new())
        });
        assert_eq!(replay.unwrap(), Replan::Stale);
    }

    #[test]
    fn invalid_or_duplicate_concerns_are_refused_whole() {
        let (_dir, mut store) = store();
        let (plan, _) = running(&mut store, &[("a", &["src/a.rs"], &[])]);
        let before = work(&store);
        for (commands, expected) in [
            (
                vec![raising("dup", "One", &[]), raising("dup", "Two", &[])],
                "awaits its human's decision",
            ),
            (vec![raising("Bad Key", "r", &[])], "not a concern key"),
            (vec![raising("k", "  ", &[])], "must not be blank"),
            (vec![raising("k", "r", &["missing"])], "no task `missing`"),
            (vec![raising("k", "r", &["a", "a"])], "listed twice"),
            (
                vec![Command::RaiseAttention {
                    concern: "k".into(),
                    reason: "r".into(),
                    evidence: Vec::new(),
                    tasks: Vec::new(),
                }],
                "items of evidence",
            ),
        ] {
            let refused = replan(&mut store, plan, &commands).unwrap_err();
            assert!(refused.downcast_ref::<Rejection>().is_some());
            assert!(format!("{refused:#}").contains(expected), "{refused:#}");
        }
        assert!(store.attention(plan).unwrap().is_empty());
        assert_eq!(state(&store, plan), PlanState::Running);
        assert_eq!(work(&store), before);
        // A paused plan was stopped by its human already.
        store.set_plan_state(plan, PlanState::Paused).unwrap();
        let paused = err(replan(&mut store, plan, &[raising("k", "r", &[])]));
        assert!(paused.contains("stopped it already"), "{paused}");
        // Planning never raises one.
        let planning = store
            .create_plan(&crate::state::tests::objective("i"))
            .unwrap();
        let message = err(store.revise_plan(planning, &[raising("k", "r", &[])], &|_| Ok(())));
        assert!(
            message.contains("only a finalized plan's replanning"),
            "{message}"
        );
    }

    #[test]
    fn no_decision_is_made_while_a_planner_acts() {
        let (_dir, mut store) = store();
        let (plan, _) = running(&mut store, &[("a", &["src/a.rs"], &[])]);
        applied(&mut store, plan, &[raising("risk", "Risky", &[])]);
        let risk = concern(&store, plan, "risk").id;
        let planner = store.planner(plan).unwrap();
        let intent = Intent {
            action: "planner.replan".into(),
            parameters: serde_json::Map::new(),
        };
        let entry = store.intend(planner, &intent).unwrap();
        store.act(entry, None).unwrap();
        // Whatever planner code runs, it cannot decide for the human.
        assert!(
            err(store.decide(plan, risk, &HumanDecision::Accept)).contains("planner is acting")
        );
        forged(
            &store,
            &format!(
                "INSERT INTO attention_decisions (concern_id, plan_id, kind, replan_id, decided_at)
                 VALUES ({risk}, {plan}, 'accept',
                     (SELECT max(id) FROM replans WHERE plan_id = {plan}), 0)"
            ),
            "no planner acts",
        );
        assert!(concern(&store, plan, "risk").blocks());
        let evidence = [crate::state::Evidence::Fact {
            name: "replan.refused".into(),
        }];
        store
            .reconcile(entry, crate::state::ActionOutcome::Failed, &evidence)
            .unwrap();
        let invocation = store
            .start_invocation(planner, "claude", "m", None)
            .unwrap();
        assert!(
            err(store.decide(plan, risk, &HumanDecision::Accept)).contains("planner is acting")
        );
        store.invocation_running(invocation).unwrap();
        store
            .finish_invocation(invocation, &crate::state::tests::succeeded())
            .unwrap();
        // Once it ended, the human decides.
        let decided = store.decide(plan, risk, &HumanDecision::Accept).unwrap();
        assert_eq!(decided, Decided::Recorded { resumed: true });
    }

    /// Inserts, beneath `Store`, a replan of `plan` and returns its id.
    fn forged_replan(store: &Store, plan: PlanId) -> i64 {
        store
            .raw()
            .execute(
                "INSERT INTO replans (plan_id, basis, commands, applied_at)
                 VALUES (?1, printf('%064d', 0), 1, 0)",
                [plan],
            )
            .unwrap();
        store.raw().last_insert_rowid()
    }

    /// Inserts, beneath `Store`, concern `key` of `plan` affecting `tasks`
    /// (JSON) for its latest replan, in its own autocommitted statement.
    fn forged_concern(store: &Store, plan: PlanId, key: &str, tasks: &str) -> Result<()> {
        store.raw().execute(
            "INSERT INTO attention_concerns
               (plan_id, key, reason, evidence, tasks, replan_id, raised_at)
             VALUES (?1, ?2, 'r', '[\"e\"]', ?3,
                 (SELECT max(id) FROM replans WHERE plan_id = ?1), 7)",
            params![plan, key, tasks],
        )?;
        Ok(())
    }

    fn concerns(store: &Store, plan: PlanId) -> i64 {
        store
            .raw()
            .query_row(
                "SELECT count(*) FROM attention_concerns WHERE plan_id = ?1",
                [plan],
                |r| r.get(0),
            )
            .unwrap()
    }

    #[test]
    fn a_concern_inserted_beneath_store_stops_its_plan_in_that_statement() {
        let (_dir, mut store) = store();
        let (plan, ids) = running(
            &mut store,
            &[("a", &["src/a.rs"], &[]), ("b", &["src/b.rs"], &[])],
        );
        let (other, others) = running(&mut store, &[("c", &["src/c.rs"], &[])]);
        let before = work(&store);

        // A running plan: the committed concern stopped exactly it.
        forged_replan(&store, plan);
        forged_concern(&store, plan, "risk", r#"["b"]"#).unwrap();
        assert_eq!(concerns(&store, plan), 1);
        assert_eq!(state(&store, plan), PlanState::NeedsAttention);
        assert_eq!(store.plan(plan).unwrap().updated_at, 7);
        assert!(concern(&store, plan, "risk").blocks());
        // No new work of it is claimed; the other plan runs and claims on.
        assert_eq!(
            store.claim(ids[1], LIMIT).unwrap(),
            Claim::PlanNotRunning(PlanState::NeedsAttention)
        );
        assert_eq!(state(&store, other), PlanState::Running);
        assert!(matches!(
            store.claim(others[0], LIMIT).unwrap(),
            Claim::Claimed(_)
        ));
        assert_eq!(concerns(&store, other), 0);
        // Nothing accepted or recorded of the plan changed but the claim of
        // the other plan's task.
        let claimed = work(&store);
        assert!(before.iter().all(|row| claimed.contains(row)));

        // An additional concern while it needs attention stands, and the
        // plan stays stopped.
        forged_concern(&store, plan, "more", "[]").unwrap();
        assert_eq!(concerns(&store, plan), 2);
        assert_eq!(state(&store, plan), PlanState::NeedsAttention);
        // A replayed key is refused, as is REPLACE of a concern.
        forged(
            &store,
            &format!(
                "INSERT INTO attention_concerns
                   (plan_id, key, reason, evidence, tasks, replan_id, raised_at)
                 VALUES ({plan}, 'risk', 'again', '[\"e\"]', '[]',
                     (SELECT max(id) FROM replans WHERE plan_id = {plan}), 0)"
            ),
            "a concern is raised once",
        );
        forged(
            &store,
            &format!(
                "REPLACE INTO attention_concerns
                   (plan_id, key, reason, evidence, tasks, replan_id, raised_at)
                 VALUES ({plan}, 'risk', 'again', '[\"e\"]', '[]',
                     (SELECT max(id) FROM replans WHERE plan_id = {plan}), 0)"
            ),
            "a concern is raised once",
        );
        assert_eq!(concerns(&store, plan), 2);

        // A ready plan, the same.
        let (ready, _) = ready_plan(&mut store, &[("d", &["src/d.rs"], &[])]);
        forged_replan(&store, ready);
        forged_concern(&store, ready, "risk", r#"["d"]"#).unwrap();
        assert_eq!(state(&store, ready), PlanState::NeedsAttention);

        // Nor does replacing the plan, or removing a task a concern names,
        // undo it.
        let row = |plan: PlanId| {
            format!(
                "REPLACE INTO plans
                   (id, objective, constraints, completion_criteria, state, created_at, updated_at)
                 SELECT id, objective, constraints, completion_criteria, 'running', 0, 0
                 FROM plans WHERE id = {plan}"
            )
        };
        forged(&store, &row(plan), "a plan is never replaced");
        forged(&store, &row(other), "a plan is never replaced");
        assert_eq!(state(&store, plan), PlanState::NeedsAttention);
        forged(
            &store,
            &format!("DELETE FROM tasks WHERE plan_id = {ready} AND key = 'd'"),
            "a task a concern names is never removed",
        );
        assert_eq!(concern(&store, ready, "risk").tasks, vec!["d".to_owned()]);
        // The human's decisions settle it exactly as through `Store`.
        let risk = concern(&store, plan, "risk").id;
        let more = concern(&store, plan, "more").id;
        assert_eq!(
            store.decide(plan, risk, &HumanDecision::Accept).unwrap(),
            Decided::Recorded { resumed: false }
        );
        assert_eq!(
            store.decide(plan, more, &HumanDecision::Accept).unwrap(),
            Decided::Recorded { resumed: true }
        );
        assert_eq!(state(&store, plan), PlanState::Running);
    }

    #[test]
    fn a_concern_is_never_inserted_for_a_plan_it_cannot_stop() {
        let (_dir, mut store) = store();
        let (plan, _) = running(&mut store, &[("a", &["src/a.rs"], &[])]);
        forged_replan(&store, plan);
        store.set_plan_state(plan, PlanState::Paused).unwrap();
        let paused = forged_concern(&store, plan, "risk", "[]").unwrap_err();
        assert!(format!("{paused:#}").contains("a concern is raised once"));
        assert_eq!(state(&store, plan), PlanState::Paused);
        // A plan in planning has no replan, whichever replan is named.
        let planning = store
            .create_plan(&crate::state::tests::objective("i"))
            .unwrap();
        for replan in [format!("{}", forged_replan(&store, plan)), "NULL".into()] {
            forged(
                &store,
                &format!(
                    "INSERT INTO attention_concerns
                       (plan_id, key, reason, evidence, tasks, replan_id, raised_at)
                     VALUES ({planning}, 'risk', 'r', '[\"e\"]', '[]', {replan}, 0)"
                ),
                "a concern is raised once",
            );
        }
        assert_eq!(state(&store, planning), PlanState::Planning);
        assert_eq!(concerns(&store, plan) + concerns(&store, planning), 0);
    }

    #[test]
    fn a_concern_affects_only_tasks_of_its_own_plan() {
        let (_dir, mut store) = store();
        let (plan, _) = running(
            &mut store,
            &[("a", &["src/a.rs"], &[]), ("b", &["src/b.rs"], &[])],
        );
        let (other, _) = running(&mut store, &[("c", &["src/c.rs"], &[])]);
        forged_replan(&store, plan);
        for tasks in [
            r#"["missing"]"#,
            r#"["a", "missing"]"#,
            r#"["c"]"#,
            r#"["a", "a"]"#,
            r#"[["a"]]"#,
            r#"[1]"#,
            r#"[null]"#,
            r#"{"a": "a"}"#,
            r#""a""#,
            r#"["a""#,
            "",
        ] {
            let refused = forged_concern(&store, plan, "risk", tasks).unwrap_err();
            let message = format!("{refused:#}");
            assert!(
                message.contains("a concern names each task it affects once")
                    || message.contains("malformed JSON"),
                "{tasks}: {message}"
            );
            assert_eq!(concerns(&store, plan), 0, "{tasks}");
            assert_eq!(state(&store, plan), PlanState::Running, "{tasks}");
        }
        assert_eq!(state(&store, other), PlanState::Running);
        forged_concern(&store, plan, "risk", r#"["a", "b"]"#).unwrap();
        assert_eq!(
            concern(&store, plan, "risk").tasks,
            vec!["a".to_owned(), "b".to_owned()]
        );
        assert_eq!(state(&store, plan), PlanState::NeedsAttention);
    }

    #[test]
    fn raising_through_store_records_the_stop_once() {
        let (_dir, mut store) = store();
        let (plan, _) = running(&mut store, &[("a", &["src/a.rs"], &[])]);
        let before = events(&store, "plan.state");
        applied(
            &mut store,
            plan,
            &[raising("one", "r", &["a"]), raising("two", "r", &[])],
        );
        assert_eq!(state(&store, plan), PlanState::NeedsAttention);
        let after = events(&store, "plan.state");
        assert_eq!(after[before.len()..], ["running -> needs_attention"]);
    }
}
