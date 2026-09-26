//! Scheduling: which tasks of a running plan may run now, and the durable
//! claims through which schedulers start them within the project's
//! concurrency ceiling.
//!
//! A task is eligible only while its plan is running with a DAG that can be
//! executed, no generation ever served it, every task it depends on is
//! completed (see [`unsatisfied_dependencies`]: only a completed acceptance
//! completes a task) and no other generation owns any path of its scope. A
//! task with any generation is never claimed: an active one is running,
//! unresolved or awaiting acceptance, and one that ended without completing
//! awaits planner action, which is not the scheduler's.
//!
//! Claiming is one transaction: the task is found eligible, fewer claims
//! than the ceiling are held, and then its generation is started, owns the
//! task's whole scope and is claimed, all of it or nothing. Transactions of
//! every agentctl process serialize on the database, so racing schedulers
//! cannot both start a task, or together exceed the ceiling they work
//! under; the schema refuses both beneath `Store`.
//!
//! A claim holds its capacity until released, which the store allows only
//! once nothing its pipeline began may still be live and how the pipeline
//! ended is established from canonical state (see [`PipelineOutcome`]).
//! Releasing never accepts anything, ends no generation and releases no
//! ownership. A claim nobody releases, because agentctl stopped, holds its
//! capacity until recovery establishes what happened.

use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroU32;

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, params};

use super::ownership::{acquire, owner};
use super::{
    Acquisition, Conflict, GenerationId, GenerationState, PipelineOutcome, PlanId, PlanState,
    Store, TaskId, event, generation_info, insert_generation, now, plan_state,
    unsatisfied_dependencies,
};

/// Where a task stands for scheduling, derived from canonical state alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskStatus {
    /// A generation of it completed its acceptance: its dependents may run.
    Completed,
    /// It may be claimed now, capacity permitting.
    Eligible,
    /// These tasks it depends on are not completed, in order.
    WaitingForDependencies(Vec<TaskId>),
    /// Other generations own these paths of its scope, in path order: a
    /// runtime constraint, never a dependency.
    WaitingForOwnership(Vec<Conflict>),
    /// A scheduler claimed this generation for it and holds capacity for
    /// its pipeline: running, or stopped without its outcome established
    /// (an attempted action, a live invocation or an unfinished
    /// acceptance), which only recovery settles.
    Scheduled(GenerationId),
    /// Its scheduled pipeline ended without completing it and released its
    /// capacity. What happens next is for the planner.
    Stopped {
        generation: GenerationId,
        outcome: PipelineOutcome,
    },
    /// A generation no scheduler claimed, from before scheduling existed or
    /// started by other means, is its latest: never scheduled over.
    Unscheduled {
        generation: GenerationId,
        state: GenerationState,
    },
}

/// A defect of a plan's canonical DAG that makes it unsafe to execute.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DagDefect {
    SelfDependency {
        task: TaskId,
    },
    /// The task, or the task it depends on, does not exist.
    MissingTask {
        task: TaskId,
        depends_on: TaskId,
    },
    /// The edge, the task or the task it depends on is of another plan.
    CrossPlan {
        task: TaskId,
        depends_on: TaskId,
    },
    /// These tasks depend on one another in a cycle, in order.
    Cycle(Vec<TaskId>),
}

/// The project's scheduling capacity: claims held, of every plan, against
/// a scheduler's ceiling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capacity {
    pub held: u32,
    pub limit: u32,
}

/// What a request to claim a task established. Only `Claimed` wrote
/// anything.
#[derive(Debug, Clone, PartialEq, Eq)]
#[must_use]
pub enum Claim {
    /// The task's generation is started, owns its whole scope and holds a
    /// unit of capacity.
    Claimed(GenerationId),
    /// The task's plan is not running.
    PlanNotRunning(PlanState),
    /// The task's plan cannot be executed safely.
    InvalidDag(Vec<DagDefect>),
    /// The task is not eligible, as its status says.
    Ineligible(TaskStatus),
    /// `held` claims already use up the ceiling.
    CapacityFull { held: u32, limit: u32 },
}

/// What a request to release a claim established.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use]
pub enum Release {
    Released(PipelineOutcome),
    /// Released before; nothing changed.
    AlreadyReleased(PipelineOutcome),
    /// Kept: something its pipeline began may still be live, an action's
    /// outcome is unknown, or its acceptance is unfinished.
    Retained,
}

/// A claim, as recorded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaimRecord {
    pub generation: GenerationId,
    pub task: TaskId,
    /// The ceiling it was taken under.
    pub capacity: u32,
    pub claimed_at: i64,
    /// How its pipeline ended and when its capacity was released, once it
    /// was.
    pub released: Option<(PipelineOutcome, i64)>,
}

/// A plan's scheduling state: every task's status, in planner order, and
/// the project's capacity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub plan: PlanId,
    pub state: PlanState,
    pub capacity: Capacity,
    pub defects: Vec<DagDefect>,
    pub tasks: Vec<(TaskId, TaskStatus)>,
}

/// Why a plan's scheduling is where it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Condition {
    /// The plan is not running, so nothing of it is claimed.
    NotRunning(PlanState),
    /// Its DAG cannot be executed safely, so nothing of it is claimed.
    InvalidDag,
    /// Some task is eligible, and capacity is free.
    Runnable,
    /// Some task is eligible, and every unit of capacity is held.
    CapacityFull,
    /// Every task is completed. The plan stays running: completing it is
    /// for final integration verification, never for scheduling.
    AllCompleted,
    /// Nothing is eligible now, while tasks are unfinished: each waits for
    /// dependencies, ownership or a scheduled pipeline, or stopped for the
    /// planner, as its status says.
    Waiting,
}

impl Snapshot {
    pub fn condition(&self) -> Condition {
        let eligible = self.tasks.iter().any(|(_, s)| *s == TaskStatus::Eligible);
        if self.state != PlanState::Running {
            Condition::NotRunning(self.state)
        } else if !self.defects.is_empty() {
            Condition::InvalidDag
        } else if self.tasks.iter().all(|(_, s)| *s == TaskStatus::Completed) {
            Condition::AllCompleted
        } else if eligible && self.capacity.held < self.capacity.limit {
            Condition::Runnable
        } else if eligible {
            Condition::CapacityFull
        } else {
            Condition::Waiting
        }
    }

    pub fn status(&self, task: TaskId) -> Option<&TaskStatus> {
        self.tasks.iter().find(|(t, _)| *t == task).map(|(_, s)| s)
    }
}

impl Store {
    /// Starts running a ready plan, or finds it running already, returning
    /// its state. Any other plan is not run.
    pub fn start_plan(&mut self, plan: PlanId) -> Result<PlanState> {
        self.write(|tx| {
            let from = plan_state(tx, plan)?;
            match from {
                PlanState::Running => return Ok(from),
                PlanState::Ready => {}
                _ => bail!("plan {plan} is {from}; only a ready or running plan is run"),
            }
            tx.execute(
                "UPDATE plans SET state = ?2, updated_at = ?3 WHERE id = ?1",
                params![plan, PlanState::Running, now()],
            )?;
            let detail = format!("{from} -> {}", PlanState::Running);
            event(tx, "plan.state", Some(plan), None, None, &detail)?;
            Ok(PlanState::Running)
        })
    }

    /// Claims `task` under the concurrency ceiling `limit`, in one
    /// transaction: see the module documentation. Anything but a claim
    /// changes nothing.
    pub fn claim(&mut self, task: TaskId, limit: NonZeroU32) -> Result<Claim> {
        self.write(|tx| {
            let plan: PlanId = tx
                .query_row("SELECT plan_id FROM tasks WHERE id = ?1", [task], |r| {
                    r.get(0)
                })
                .optional()?
                .with_context(|| format!("task {task} does not exist"))?;
            let state = plan_state(tx, plan)?;
            if state != PlanState::Running {
                return Ok(Claim::PlanNotRunning(state));
            }
            let defects = dag_defects(tx, plan)?;
            if !defects.is_empty() {
                return Ok(Claim::InvalidDag(defects));
            }
            let status = task_status(tx, task)?;
            if status != TaskStatus::Eligible {
                return Ok(Claim::Ineligible(status));
            }
            let held = held(tx)?;
            if held >= limit.get() {
                return Ok(Claim::CapacityFull {
                    held,
                    limit: limit.get(),
                });
            }
            let (_, generation, number) = insert_generation(tx, task)?;
            let scope = scope(tx, task)?;
            let scope: Vec<&str> = scope.iter().map(String::as_str).collect();
            if let Acquisition::Conflicted(conflicts) = acquire(tx, generation, &scope)? {
                // Found free within this transaction; rolls everything back.
                bail!("ownership of task {task}'s scope changed while claiming: {conflicts:?}");
            }
            tx.execute(
                "INSERT INTO scheduler_claims (generation_id, task_id, capacity, claimed_at)
                 VALUES (?1, ?2, ?3, ?4)",
                params![generation, task, limit.get(), now()],
            )?;
            let detail = format!("generation {number}: {} of {limit} claims held", held + 1);
            event(
                tx,
                "scheduler.claimed",
                Some(plan),
                Some(task),
                None,
                &detail,
            )?;
            Ok(Claim::Claimed(generation))
        })
    }

    /// Releases the capacity `generation`'s claim holds, once how its
    /// pipeline ended is established and nothing it began may still be
    /// live. Releasing again changes nothing.
    pub fn release_claim(&mut self, generation: GenerationId) -> Result<Release> {
        self.write(|tx| {
            let released: Option<PipelineOutcome> = tx
                .query_row(
                    "SELECT r.outcome FROM scheduler_claims c
                     LEFT JOIN scheduler_releases r ON r.generation_id = c.generation_id
                     WHERE c.generation_id = ?1",
                    [generation],
                    |r| r.get(0),
                )
                .optional()?
                .with_context(|| format!("generation {generation} was never claimed"))?;
            if let Some(outcome) = released {
                return Ok(Release::AlreadyReleased(outcome));
            }
            let outcome: Option<PipelineOutcome> = tx.query_row(
                "SELECT outcome FROM scheduler_outcomes WHERE generation_id = ?1",
                [generation],
                |r| r.get(0),
            )?;
            let Some(outcome) = outcome else {
                return Ok(Release::Retained);
            };
            tx.execute(
                "INSERT INTO scheduler_releases (generation_id, outcome, released_at)
                 VALUES (?1, ?2, ?3)",
                params![generation, outcome, now()],
            )?;
            let (plan, task, number, _) = generation_info(tx, generation)?;
            let detail = format!("generation {number}: {outcome}");
            event(
                tx,
                "scheduler.released",
                Some(plan),
                Some(task),
                None,
                &detail,
            )?;
            Ok(Release::Released(outcome))
        })
    }

    /// Every claim of the project, of every plan, in the order taken.
    pub fn claims(&self) -> Result<Vec<ClaimRecord>> {
        self.conn
            .prepare(
                "SELECT c.generation_id, c.task_id, c.capacity, c.claimed_at, r.outcome,
                   r.released_at
                 FROM scheduler_claims c
                 LEFT JOIN scheduler_releases r ON r.generation_id = c.generation_id
                 ORDER BY c.claimed_at, c.generation_id",
            )?
            .query_map([], |r| {
                let outcome: Option<PipelineOutcome> = r.get(4)?;
                Ok(ClaimRecord {
                    generation: r.get(0)?,
                    task: r.get(1)?,
                    capacity: r.get(2)?,
                    claimed_at: r.get(3)?,
                    released: match outcome {
                        Some(outcome) => Some((outcome, r.get(5)?)),
                        None => None,
                    },
                })
            })?
            .collect::<rusqlite::Result<_>>()
            .map_err(Into::into)
    }

    /// Where `plan`'s scheduling stands against the ceiling `limit`, read
    /// consistently.
    pub fn snapshot(&self, plan: PlanId, limit: NonZeroU32) -> Result<Snapshot> {
        let tx = self.conn.unchecked_transaction()?;
        let state = plan_state(&tx, plan)?;
        let ids: Vec<TaskId> = tx
            .prepare("SELECT id FROM tasks WHERE plan_id = ?1 ORDER BY id")?
            .query_map([plan], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        let tasks = ids
            .into_iter()
            .map(|task| Ok((task, task_status(&tx, task)?)))
            .collect::<Result<_>>()?;
        Ok(Snapshot {
            plan,
            state,
            capacity: Capacity {
                held: held(&tx)?,
                limit: limit.get(),
            },
            defects: dag_defects(&tx, plan)?,
            tasks,
        })
    }

    /// Every defect of `plan`'s DAG that makes it unsafe to execute.
    pub fn dag_defects(&self, plan: PlanId) -> Result<Vec<DagDefect>> {
        plan_state(&self.conn, plan)?;
        dag_defects(&self.conn, plan)
    }
}

/// How many claims of the project hold capacity.
fn held(conn: &Connection) -> Result<u32> {
    Ok(conn.query_row(
        "SELECT count(*) FROM scheduler_claims c WHERE NOT EXISTS
           (SELECT 1 FROM scheduler_releases r WHERE r.generation_id = c.generation_id)",
        [],
        |r| r.get(0),
    )?)
}

fn scope(conn: &Connection, task: TaskId) -> Result<Vec<String>> {
    conn.prepare("SELECT path FROM task_scope WHERE task_id = ?1 ORDER BY path")?
        .query_map([task], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()
        .map_err(Into::into)
}

/// Where `task` stands, apart from its plan and the capacity held.
fn task_status(conn: &Connection, task: TaskId) -> Result<TaskStatus> {
    let completed: bool = conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM completed_tasks WHERE task_id = ?1)",
        [task],
        |r| r.get(0),
    )?;
    if completed {
        return Ok(TaskStatus::Completed);
    }
    let latest: Option<(GenerationId, GenerationState, bool, Option<PipelineOutcome>)> = conn
        .query_row(
            "SELECT g.id, g.state, c.generation_id IS NOT NULL, r.outcome FROM generations g
             LEFT JOIN scheduler_claims c ON c.generation_id = g.id
             LEFT JOIN scheduler_releases r ON r.generation_id = g.id
             WHERE g.task_id = ?1 ORDER BY g.number DESC LIMIT 1",
            [task],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional()?;
    if let Some((generation, state, claimed, released)) = latest {
        return Ok(match (claimed, released) {
            (true, None) => TaskStatus::Scheduled(generation),
            (true, Some(outcome)) => TaskStatus::Stopped {
                generation,
                outcome,
            },
            (false, _) => TaskStatus::Unscheduled { generation, state },
        });
    }
    let waiting = unsatisfied_dependencies(conn, task)?;
    if !waiting.is_empty() {
        return Ok(TaskStatus::WaitingForDependencies(waiting));
    }
    let mut conflicts = Vec::new();
    for path in scope(conn, task)? {
        if let Some(owner) = owner(conn, &path)? {
            conflicts.push(Conflict { path, owner });
        }
    }
    if !conflicts.is_empty() {
        return Ok(TaskStatus::WaitingForOwnership(conflicts));
    }
    Ok(TaskStatus::Eligible)
}

/// Every defect of `plan`'s DAG: edges naming a missing task, a task of
/// another plan or the task itself, and cycles among its other edges. The
/// store refuses each of them; finding one means canonical state was
/// changed beneath it, and nothing of the plan is safe to run.
fn dag_defects(conn: &Connection, plan: PlanId) -> Result<Vec<DagDefect>> {
    /// A dependency edge: its plan, task and dependency, and the plans the
    /// task and its dependency belong to, if they exist.
    type Edge = (PlanId, TaskId, TaskId, Option<PlanId>, Option<PlanId>);
    let edges: Vec<Edge> = conn
        .prepare(
            "SELECT d.plan_id, d.task_id, d.depends_on, t.plan_id, u.plan_id
             FROM task_dependencies d
             LEFT JOIN tasks t ON t.id = d.task_id LEFT JOIN tasks u ON u.id = d.depends_on
             WHERE d.plan_id = ?1 OR t.plan_id = ?1
             ORDER BY d.task_id, d.depends_on",
        )?
        .query_map([plan], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        })?
        .collect::<rusqlite::Result<_>>()?;
    let mut defects = Vec::new();
    let mut graph: BTreeMap<TaskId, Vec<TaskId>> = BTreeMap::new();
    for (of, task, depends_on, task_plan, dep_plan) in edges {
        if task == depends_on {
            defects.push(DagDefect::SelfDependency { task });
        } else if task_plan.is_none() || dep_plan.is_none() {
            defects.push(DagDefect::MissingTask { task, depends_on });
        } else if of != plan || task_plan != Some(plan) || dep_plan != Some(plan) {
            defects.push(DagDefect::CrossPlan { task, depends_on });
        } else {
            graph.entry(task).or_default().push(depends_on);
        }
    }
    // Two tasks share a cycle when each reaches the other.
    let reach = |from: TaskId| {
        let mut seen = BTreeSet::new();
        let mut stack = graph.get(&from).cloned().unwrap_or_default();
        while let Some(next) = stack.pop() {
            if seen.insert(next) {
                stack.extend(graph.get(&next).into_iter().flatten().copied());
            }
        }
        seen
    };
    let reaches: BTreeMap<TaskId, BTreeSet<TaskId>> =
        graph.keys().map(|&task| (task, reach(task))).collect();
    let mut cycled = BTreeSet::new();
    for (&task, reached) in &reaches {
        if !reached.contains(&task) || cycled.contains(&task) {
            continue;
        }
        let cycle: Vec<TaskId> = reached
            .iter()
            .copied()
            .filter(|other| reaches.get(other).is_some_and(|r| r.contains(&task)))
            .collect();
        cycled.extend(cycle.iter().copied());
        defects.push(DagDefect::Cycle(cycle));
    }
    Ok(defects)
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::{Arc, Barrier};
    use std::thread;

    use super::*;
    use crate::acceptance::Outcome;
    use crate::acceptance::tests::{Case, Judged};
    use crate::project::{STATE_DB, STATE_DIR};
    use crate::state::tests::{
        acquire, downgrade_to_v12, err, ready_plan, rows_besides, store, version,
    };
    use crate::state::{AcceptancePhase, AgentScope, Role};

    pub(crate) fn limit(n: u32) -> NonZeroU32 {
        NonZeroU32::new(n).unwrap()
    }

    /// A running plan of tasks `(key, scope, depends_on)`, and their ids.
    fn running_plan(
        store: &mut Store,
        tasks: &[(&str, &[&str], &[&str])],
    ) -> (PlanId, Vec<TaskId>) {
        let (plan, ids) = ready_plan(store, tasks);
        assert_eq!(store.start_plan(plan).unwrap(), PlanState::Running);
        (plan, ids)
    }

    fn claimed(claim: Claim) -> GenerationId {
        match claim {
            Claim::Claimed(generation) => generation,
            other => panic!("not claimed: {other:?}"),
        }
    }

    fn count(store: &Store, sql: &str) -> i64 {
        store.raw().query_row(sql, [], |r| r.get(0)).unwrap()
    }

    fn events(store: &Store, kind: &str) -> usize {
        let events = store.events_after(0, 1_000_000).unwrap();
        events.iter().filter(|e| e.kind == kind).count()
    }

    fn path(dir: &tempfile::TempDir) -> std::path::PathBuf {
        dir.path().join("state.db")
    }

    /// Runs `f` on `n` threads at once, each with a store connection of its
    /// own, as racing agentctl processes would.
    fn race<T: Send>(path: &Path, n: usize, f: impl Fn(&mut Store, usize) -> T + Sync) -> Vec<T> {
        let barrier = Arc::new(Barrier::new(n));
        thread::scope(|scope| {
            let handles: Vec<_> = (0..n)
                .map(|i| {
                    let barrier = Arc::clone(&barrier);
                    let f = &f;
                    scope.spawn(move || {
                        let mut store = Store::open(path).unwrap();
                        barrier.wait();
                        f(&mut store, i)
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        })
    }

    #[test]
    fn a_claim_starts_one_generation_owning_the_whole_literal_scope() {
        let (dir, mut store) = store();
        let scope: &[&str] = &["src/[id].rs", "src/with space.rs", "src/*.rs"];
        let (plan, tasks) = running_plan(&mut store, &[("task", scope, &[])]);
        let before = store.events_after(0, 1000).unwrap().len();

        let generation = claimed(store.claim(tasks[0], limit(2)).unwrap());
        let mut owned: Vec<&str> = scope.to_vec();
        owned.sort_unstable();
        // Exactly these literal paths, never what they would match.
        assert_eq!(store.owned_paths(generation).unwrap(), owned);
        assert_eq!(store.generations(tasks[0]).unwrap().len(), 1);
        let kinds: Vec<String> = store
            .events_after(before as i64, 100)
            .unwrap()
            .into_iter()
            .map(|e| e.kind)
            .collect();
        assert_eq!(
            kinds,
            [
                "generation.started",
                "ownership.acquired",
                "scheduler.claimed"
            ]
        );

        let fresh = Store::open(&path(&dir)).unwrap();
        let claims = fresh.claims().unwrap();
        assert_eq!(claims.len(), 1);
        assert_eq!(
            (
                claims[0].generation,
                claims[0].task,
                claims[0].capacity,
                claims[0].released
            ),
            (generation, tasks[0], 2, None)
        );
        let snapshot = fresh.snapshot(plan, limit(2)).unwrap();
        assert_eq!(
            snapshot,
            store.snapshot(plan, limit(2)).unwrap(),
            "reconstructed"
        );
        assert_eq!(
            snapshot.tasks,
            [(tasks[0], TaskStatus::Scheduled(generation))]
        );
        assert_eq!(snapshot.capacity, Capacity { held: 1, limit: 2 });
        assert_eq!(snapshot.condition(), Condition::Waiting);
    }

    #[test]
    fn only_eligible_tasks_of_running_plans_are_claimed() {
        let (_dir, mut store) = store();
        let tasks: [(&str, &[&str], &[&str]); 3] = [
            ("first", &["src/a.rs"], &[]),
            ("second", &["src/b.rs"], &["first"]),
            ("legacy", &["src/c.rs"], &[]),
        ];
        let (plan, ids) = ready_plan(&mut store, &tasks);
        let [first, second, legacy] = ids[..] else {
            panic!()
        };
        assert_eq!(
            store.claim(first, limit(4)).unwrap(),
            Claim::PlanNotRunning(PlanState::Ready)
        );
        // Work started before, or beside, scheduling is never scheduled over.
        let old = store.start_generation(legacy).unwrap();
        store.start_plan(plan).unwrap();
        let started = events(&store, "plan.state");
        assert_eq!(
            store.start_plan(plan).unwrap(),
            PlanState::Running,
            "idempotent"
        );
        assert_eq!(events(&store, "plan.state"), started);
        assert_eq!(
            store.claim(legacy, limit(4)).unwrap(),
            Claim::Ineligible(TaskStatus::Unscheduled {
                generation: old,
                state: GenerationState::Active
            })
        );
        assert_eq!(
            store.claim(second, limit(4)).unwrap(),
            Claim::Ineligible(TaskStatus::WaitingForDependencies(vec![first]))
        );
        let generation = claimed(store.claim(first, limit(4)).unwrap());
        // Re-entry never duplicates an active generation.
        assert_eq!(
            store.claim(first, limit(4)).unwrap(),
            Claim::Ineligible(TaskStatus::Scheduled(generation))
        );
        assert_eq!(
            store.release_claim(generation).unwrap(),
            Release::Released(PipelineOutcome::NotExecuted)
        );
        assert_eq!(
            store.release_claim(generation).unwrap(),
            Release::AlreadyReleased(PipelineOutcome::NotExecuted)
        );
        assert_eq!(events(&store, "scheduler.released"), 1);
        // A stopped pipeline is the planner's to follow up, never retried.
        let stopped = TaskStatus::Stopped {
            generation,
            outcome: PipelineOutcome::NotExecuted,
        };
        assert_eq!(
            store.claim(first, limit(4)).unwrap(),
            Claim::Ineligible(stopped)
        );
        assert_eq!(store.generations(first).unwrap().len(), 1);
        assert_eq!(
            store.claim(second, limit(4)).unwrap(),
            Claim::Ineligible(TaskStatus::WaitingForDependencies(vec![first]))
        );
        assert_eq!(events(&store, "scheduler.claimed"), 1);
        store.set_plan_state(plan, PlanState::Paused).unwrap();
        assert!(matches!(
            store.claim(second, limit(4)).unwrap(),
            Claim::PlanNotRunning(_)
        ));
        assert!(err(store.claim(TaskId(99), limit(4))).contains("does not exist"));
        assert!(err(store.release_claim(old)).contains("never claimed"));
    }

    #[test]
    fn ownership_conflicts_wait_without_partial_acquisition_or_edges() {
        let (_dir, mut store) = store();
        let tasks: [(&str, &[&str], &[&str]); 3] = [
            ("x", &["src/a.rs", "src/b.rs"], &[]),
            ("y", &["src/b.rs", "src/c.rs"], &[]),
            ("z", &["src/c.rs"], &[]),
        ];
        let (plan, ids) = running_plan(&mut store, &tasks);
        let edges = count(&store, "SELECT count(*) FROM task_dependencies");
        let x = claimed(store.claim(ids[0], limit(3)).unwrap());
        let Claim::Ineligible(TaskStatus::WaitingForOwnership(conflicts)) =
            store.claim(ids[1], limit(3)).unwrap()
        else {
            panic!()
        };
        assert_eq!(conflicts.len(), 1);
        assert_eq!(
            (conflicts[0].path.as_str(), conflicts[0].owner.generation),
            ("src/b.rs", x)
        );
        // All or nothing: y got no generation, and `src/c.rs` stayed free.
        assert!(store.generations(ids[1]).unwrap().is_empty());
        assert_eq!(store.owner("src/c.rs").unwrap(), None);
        let z = claimed(store.claim(ids[2], limit(3)).unwrap());
        assert_eq!(store.owned_paths(z).unwrap(), ["src/c.rs"]);
        // A runtime constraint, never a planned dependency.
        assert_eq!(
            count(&store, "SELECT count(*) FROM task_dependencies"),
            edges
        );
        assert!(store.task(ids[1]).unwrap().depends_on.is_empty());
        let snapshot = store.snapshot(plan, limit(3)).unwrap();
        assert!(
            matches!(snapshot.status(ids[1]), Some(TaskStatus::WaitingForOwnership(c)) if c.len() == 2)
        );
        assert_eq!(snapshot.condition(), Condition::Waiting);
    }

    #[test]
    fn capacity_is_durable_and_shared_by_every_plan_and_connection() {
        let (dir, mut store) = store();
        let one: [(&str, &[&str], &[&str]); 2] =
            [("a", &["src/a.rs"], &[]), ("b", &["src/b.rs"], &[])];
        let two: [(&str, &[&str], &[&str]); 2] =
            [("c", &["src/c.rs"], &[]), ("d", &["src/d.rs"], &[])];
        let (p1, first) = running_plan(&mut store, &one);
        let (p2, second) = running_plan(&mut store, &two);
        let a = claimed(store.claim(first[0], limit(1)).unwrap());
        let mut other = Store::open(&path(&dir)).unwrap();
        // Another plan, another connection: the same ceiling.
        assert_eq!(
            other.claim(second[0], limit(1)).unwrap(),
            Claim::CapacityFull { held: 1, limit: 1 }
        );
        assert!(store.generations(second[0]).unwrap().is_empty());
        // Raising the ceiling allows more on the next claim.
        let c = claimed(other.claim(second[0], limit(2)).unwrap());
        // Lowering it below what is held stops nothing, and starts nothing.
        assert_eq!(
            store.claim(first[1], limit(1)).unwrap(),
            Claim::CapacityFull { held: 2, limit: 1 }
        );
        assert_eq!(
            store.snapshot(p1, limit(1)).unwrap().condition(),
            Condition::CapacityFull
        );
        assert_eq!(
            store.release_claim(a).unwrap(),
            Release::Released(PipelineOutcome::NotExecuted)
        );
        assert_eq!(
            store.claim(first[1], limit(1)).unwrap(),
            Claim::CapacityFull { held: 1, limit: 1 }
        );
        assert_eq!(
            other.release_claim(c).unwrap(),
            Release::Released(PipelineOutcome::NotExecuted)
        );
        claimed(store.claim(first[1], limit(1)).unwrap());
        assert_eq!(
            store.snapshot(p2, limit(1)).unwrap().capacity,
            Capacity { held: 1, limit: 1 }
        );
        assert_eq!(events(&store, "scheduler.claimed"), 3);
    }

    #[test]
    fn racing_schedulers_start_a_task_once() {
        // Race A.
        let (dir, mut store) = store();
        let (_, ids) = running_plan(&mut store, &[("task", &["src/a.rs", "src/b.rs"], &[])]);
        let results = race(&path(&dir), 8, |store, _| {
            store.claim(ids[0], limit(8)).unwrap()
        });
        let winners: Vec<GenerationId> = results
            .iter()
            .filter_map(|c| match c {
                Claim::Claimed(g) => Some(*g),
                _ => None,
            })
            .collect();
        assert_eq!(winners.len(), 1, "{results:?}");
        for result in &results {
            assert!(
                matches!(result, Claim::Claimed(_))
                    || *result == Claim::Ineligible(TaskStatus::Scheduled(winners[0])),
                "{result:?}"
            );
        }
        assert_eq!(store.generations(ids[0]).unwrap().len(), 1);
        assert_eq!(store.claims().unwrap().len(), 1);
        assert_eq!(
            count(
                &store,
                "SELECT count(DISTINCT generation_id) FROM ownership"
            ),
            1
        );
        assert_eq!(store.owned_paths(winners[0]).unwrap().len(), 2);
        assert_eq!(events(&store, "scheduler.claimed"), 1);
        assert_eq!(events(&store, "generation.started"), 1);
    }

    #[test]
    fn racing_schedulers_never_exceed_the_ceiling() {
        // Race B: one slot left, two schedulers, two eligible tasks.
        for _ in 0..10 {
            let (dir, mut store) = store();
            let tasks: [(&str, &[&str], &[&str]); 3] = [
                ("held", &["src/h.rs"], &[]),
                ("a", &["src/a.rs"], &[]),
                ("b", &["src/b.rs"], &[]),
            ];
            let (_, ids) = running_plan(&mut store, &tasks);
            claimed(store.claim(ids[0], limit(2)).unwrap());
            let results = race(&path(&dir), 2, |store, i| {
                store.claim(ids[1 + i], limit(2)).unwrap()
            });
            let won = results
                .iter()
                .filter(|c| matches!(c, Claim::Claimed(_)))
                .count();
            assert_eq!(won, 1, "{results:?}");
            assert!(
                results.contains(&Claim::CapacityFull { held: 2, limit: 2 }),
                "{results:?}"
            );
            assert_eq!(store.claims().unwrap().len(), 2);
            let started = count(&store, "SELECT count(*) FROM generations");
            assert_eq!(started, 2, "the loser started nothing");
        }
    }

    #[test]
    fn racing_schedulers_claim_disjoint_tasks_together_and_overlapping_ones_once() {
        // Race C: disjoint scopes both run.
        let (_dir, mut first) = store();
        let tasks: [(&str, &[&str], &[&str]); 2] =
            [("a", &["src/a.rs"], &[]), ("b", &["src/b.rs"], &[])];
        let (_, ids) = running_plan(&mut first, &tasks);
        let results = race(&path(&_dir), 2, |store, i| {
            store.claim(ids[i], limit(2)).unwrap()
        });
        assert!(
            results.iter().all(|c| matches!(c, Claim::Claimed(_))),
            "{results:?}"
        );
        assert_eq!(first.claims().unwrap().len(), 2);

        // Race D: scopes sharing one path serialize, and the loser owns nothing.
        for _ in 0..10 {
            let (dir, mut store) = store();
            let tasks: [(&str, &[&str], &[&str]); 2] = [
                ("x", &["src/a.rs", "src/shared.rs"], &[]),
                ("y", &["src/shared.rs", "src/b.rs"], &[]),
            ];
            let (_, ids) = running_plan(&mut store, &tasks);
            let results = race(&path(&dir), 2, |store, i| {
                store.claim(ids[i], limit(2)).unwrap()
            });
            let winner = results
                .iter()
                .position(|c| matches!(c, Claim::Claimed(_)))
                .unwrap();
            let loser = 1 - winner;
            assert!(
                matches!(
                    &results[loser],
                    Claim::Ineligible(TaskStatus::WaitingForOwnership(_))
                ),
                "{results:?}"
            );
            assert!(store.generations(ids[loser]).unwrap().is_empty());
            let owners = count(
                &store,
                "SELECT count(DISTINCT generation_id) FROM ownership",
            );
            assert_eq!(
                (owners, count(&store, "SELECT count(*) FROM ownership")),
                (1, 2)
            );
        }
    }

    fn state(case: &Case) -> std::path::PathBuf {
        case.fx.project.root.join(STATE_DIR).join(STATE_DB)
    }

    fn waiting(case: &mut Case) {
        let blocked = Claim::Ineligible(TaskStatus::WaitingForDependencies(vec![case.task]));
        assert_eq!(case.fx.store.claim(case.next, limit(4)).unwrap(), blocked);
        assert!(!case.fx.store.dependencies_satisfied(case.next).unwrap());
        assert!(case.fx.store.generations(case.next).unwrap().is_empty());
    }

    const ACCEPTED: [(&str, &str); 2] =
        [("src/lib.rs", "pub fn old() {}\n"), ("README.md", "# r\n")];
    const EDITS: [(&str, Option<&str>); 1] = [("src/lib.rs", Some("pub fn new() {}\n"))];

    #[test]
    fn only_a_completed_acceptance_satisfies_a_dependency() {
        let mut case = Case::installed(&ACCEPTED, &["src/lib.rs"], &EDITS);
        case.fx.store.start_plan(case.plan).unwrap();
        // An installed candidate, then a verifier's pass.
        waiting(&mut case);
        case.verify(Judged::Pass);
        waiting(&mut case);
        // Published, then synchronized, without completing.
        let outcome = case.accept_with(|_| anyhow::bail!("forced graph failure"));
        assert!(matches!(
            outcome,
            Outcome::Incomplete {
                phase: AcceptancePhase::Published,
                ..
            }
        ));
        waiting(&mut case);
        let conn = case.connection();
        conn.execute_batch(
            "CREATE TRIGGER test_refuse_completion BEFORE INSERT ON acceptance_phases
             WHEN NEW.phase = 'completed'
             BEGIN SELECT RAISE(ABORT, 'forced completion failure'); END;",
        )
        .unwrap();
        let outcome = case.accept();
        assert!(matches!(
            outcome,
            Outcome::Incomplete {
                phase: AcceptancePhase::Synchronized,
                ..
            }
        ));
        waiting(&mut case);
        conn.execute_batch("DROP TRIGGER test_refuse_completion")
            .unwrap();
        assert!(matches!(case.accept(), Outcome::Completed(_)));
        assert!(case.fx.store.dependencies_satisfied(case.next).unwrap());
        assert_eq!(
            case.fx
                .store
                .snapshot(case.plan, limit(4))
                .unwrap()
                .status(case.task),
            Some(&TaskStatus::Completed)
        );
        claimed(case.fx.store.claim(case.next, limit(4)).unwrap());
        // A completed task is never claimed again.
        assert_eq!(
            case.fx.store.claim(case.task, limit(4)).unwrap(),
            Claim::Ineligible(TaskStatus::Completed)
        );

        for judged in [Judged::Fail, Judged::Unresolved, Judged::InvocationFailed] {
            let mut case = Case::installed(&ACCEPTED, &["src/lib.rs"], &EDITS);
            case.fx.store.start_plan(case.plan).unwrap();
            case.verify(judged);
            waiting(&mut case);
            let outcome = case.accept();
            assert!(
                matches!(outcome, Outcome::InvalidPrecondition(_)),
                "{outcome:?}"
            );
            waiting(&mut case);
        }
    }

    #[test]
    fn a_dependency_accepted_while_selecting_is_seen_by_the_claim() {
        // Race E.
        for _ in 0..3 {
            let mut case = Case::passed(&ACCEPTED, &["src/lib.rs"], &EDITS);
            case.fx.store.start_plan(case.plan).unwrap();
            let (next, plan) = (case.next, case.plan);
            let path = state(&case);
            let mut scheduler = Store::open(&path).unwrap();
            let stale = scheduler.snapshot(plan, limit(4)).unwrap();
            assert!(matches!(
                stale.status(next),
                Some(TaskStatus::WaitingForDependencies(_))
            ));
            let barrier = Barrier::new(2);
            let claim = thread::scope(|scope| {
                let accepting = scope.spawn(|| {
                    barrier.wait();
                    case.accept()
                });
                barrier.wait();
                let claim = scheduler.claim(next, limit(4)).unwrap();
                assert!(matches!(accepting.join().unwrap(), Outcome::Completed(_)));
                claim
            });
            let seq = |kind: &str, task: TaskId| {
                let events = scheduler.events_after(0, 100_000).unwrap();
                events
                    .iter()
                    .find(|e| e.kind == kind && e.task == Some(task))
                    .map(|e| e.seq)
            };
            match claim {
                Claim::Claimed(_) => {
                    let completed = seq("acceptance.completed", case.task).unwrap();
                    assert!(completed < seq("scheduler.claimed", next).unwrap());
                }
                Claim::Ineligible(TaskStatus::WaitingForDependencies(_)) => {
                    claimed(scheduler.claim(next, limit(4)).unwrap());
                }
                other => panic!("{other:?}"),
            }
            assert_eq!(scheduler.generations(next).unwrap().len(), 1);
        }
    }

    #[test]
    fn invalid_dags_fail_closed() {
        let tasks: [(&str, &[&str], &[&str]); 3] = [
            ("a", &["src/a.rs"], &[]),
            ("b", &["src/b.rs"], &[]),
            ("free", &["src/c.rs"], &[]),
        ];
        /// Raw SQL forging a defect from tasks `a` and `b` of a plan.
        type Forge = fn(TaskId, TaskId, PlanId) -> String;
        let cases: [(&str, Forge); 4] = [
            ("cycle", |a, b, p| {
                format!(
                    "INSERT INTO task_dependencies VALUES ({p}, {a}, {b});
                                         INSERT INTO task_dependencies VALUES ({p}, {b}, {a});"
                )
            }),
            ("self", |a, _, p| {
                format!(
                    "PRAGMA ignore_check_constraints = ON;
                                        INSERT INTO task_dependencies VALUES ({p}, {a}, {a});"
                )
            }),
            ("missing", |a, _, p| {
                format!(
                    "PRAGMA foreign_keys = OFF;
                                           INSERT INTO task_dependencies VALUES ({p}, {a}, 999);"
                )
            }),
            ("cross", |a, _, p| {
                format!(
                    "PRAGMA foreign_keys = OFF;
                                         INSERT INTO task_dependencies VALUES ({p}, {a}, {o});",
                    o = a.0 + 3
                )
            }),
        ];
        for (name, sql) in cases {
            let (dir, mut store) = store();
            let (plan, ids) = running_plan(&mut store, &tasks);
            let (_, other) = running_plan(&mut store, &[("elsewhere", &[], &[])]);
            assert_eq!(other[0].0, ids[0].0 + 3);
            rusqlite::Connection::open(path(&dir))
                .unwrap()
                .execute_batch(&sql(ids[0], ids[1], plan))
                .unwrap();
            let defects = store.dag_defects(plan).unwrap();
            let expected = match name {
                "cycle" => DagDefect::Cycle(vec![ids[0], ids[1]]),
                "self" => DagDefect::SelfDependency { task: ids[0] },
                "missing" => DagDefect::MissingTask {
                    task: ids[0],
                    depends_on: TaskId(999),
                },
                _ => DagDefect::CrossPlan {
                    task: ids[0],
                    depends_on: other[0],
                },
            };
            assert_eq!(defects, [expected], "{name}");
            // Not even the task no defect touches is claimed.
            assert_eq!(
                store.claim(ids[2], limit(4)).unwrap(),
                Claim::InvalidDag(defects),
                "{name}"
            );
            let snapshot = store.snapshot(plan, limit(4)).unwrap();
            assert_eq!(snapshot.condition(), Condition::InvalidDag, "{name}");
            assert_eq!(
                count(&store, "SELECT count(*) FROM generations"),
                0,
                "{name}"
            );
            // The other plan is unaffected.
            claimed(store.claim(other[0], limit(4)).unwrap());
        }
    }

    #[test]
    fn the_database_refuses_forged_scheduling_state() {
        let (_dir, mut store) = store();
        let tasks: [(&str, &[&str], &[&str]); 3] = [
            ("a", &["src/a.rs"], &[]),
            ("b", &["src/b.rs"], &["a"]),
            ("c", &["src/c.rs"], &[]),
        ];
        let (plan, ids) = running_plan(&mut store, &tasks);
        let [a, b, c] = ids[..] else { panic!() };
        let held = claimed(store.claim(c, limit(1)).unwrap());
        let refused = |store: &Store, sql: &str| {
            let message = store.raw().execute_batch(sql).unwrap_err().to_string();
            assert!(
                [
                    "claimed, within capacity",
                    "released once",
                    "immutable",
                    "UNIQUE"
                ]
                .iter()
                .any(|m| message.contains(m)),
                "{sql}: {message}"
            );
        };
        // Another active generation of a task that has one.
        refused(
            &store,
            &format!(
                "INSERT INTO generations (task_id, number, state, started_at)
                                  VALUES ({c}, 2, 'active', 0)"
            ),
        );
        let generation = store.start_generation(a).unwrap();
        let claim = |g: GenerationId, t: TaskId, capacity: u32| {
            format!("INSERT INTO scheduler_claims VALUES ({g}, {t}, {capacity}, 0)")
        };
        // Without the task's ownership, then beyond the ceiling, then bound
        // to another task.
        refused(&store, &claim(generation, a, 5));
        acquire(&mut store, generation, &["src/a.rs"]);
        refused(&store, &claim(generation, a, 1));
        refused(&store, &claim(generation, c, 5));
        // A task that depends on an uncompleted one.
        let waiting = store.start_generation(b).unwrap();
        acquire(&mut store, waiting, &["src/b.rs"]);
        refused(&store, &claim(waiting, b, 5));
        // Rewriting history.
        refused(
            &store,
            &format!("INSERT OR REPLACE INTO scheduler_claims VALUES ({held}, {c}, 9, 0)"),
        );
        refused(
            &store,
            &format!("UPDATE scheduler_claims SET capacity = 9 WHERE generation_id = {held}"),
        );
        refused(
            &store,
            &format!("DELETE FROM scheduler_claims WHERE generation_id = {held}"),
        );
        // Releasing with an outcome canonical state does not establish, or
        // while work may be live.
        refused(
            &store,
            &format!("INSERT INTO scheduler_releases VALUES ({held}, 'accepted', 0)"),
        );
        let agent = store
            .create_agent(Role::Executor, AgentScope::Generation(held))
            .unwrap();
        store.start_invocation(agent, "claude", "m", None).unwrap();
        refused(
            &store,
            &format!("INSERT INTO scheduler_releases VALUES ({held}, 'not_executed', 0)"),
        );
        assert_eq!(store.release_claim(held).unwrap(), Release::Retained);
        // A plan no longer running claims nothing, even by raw SQL.
        store.set_plan_state(plan, PlanState::Paused).unwrap();
        let fresh = store.start_generation(b);
        assert!(fresh.is_err(), "b waits for a, whatever");
        assert_eq!(store.claims().unwrap().len(), 1);
        assert_eq!(store.claims().unwrap()[0].released, None);
    }

    #[test]
    fn migrating_version_12_schedules_no_historical_work() {
        let dir = tempfile::tempdir().unwrap();
        let path = path(&dir);
        let mut store = Store::open(&path).unwrap();
        let tasks: [(&str, &[&str], &[&str]); 2] =
            [("old", &["src/a.rs"], &[]), ("new", &["src/b.rs"], &[])];
        let (plan, ids) = running_plan(&mut store, &tasks);
        let generation = store.start_generation(ids[0]).unwrap();
        acquire(&mut store, generation, &["src/a.rs"]);
        drop(store);
        downgrade_to_v12(&path);
        assert_eq!(version(&path), 12);
        let before = rows_besides(&path, &[]);

        let mut store = Store::open(&path).unwrap();
        assert_eq!(version(&path), 13);
        assert_eq!(rows_besides(&path, &[]), before, "nothing changed");
        assert!(store.claims().unwrap().is_empty());
        let unscheduled = TaskStatus::Unscheduled {
            generation,
            state: GenerationState::Active,
        };
        assert_eq!(
            store.claim(ids[0], limit(4)).unwrap(),
            Claim::Ineligible(unscheduled)
        );
        assert_eq!(store.snapshot(plan, limit(4)).unwrap().capacity.held, 0);
        claimed(store.claim(ids[1], limit(4)).unwrap());
    }
}
