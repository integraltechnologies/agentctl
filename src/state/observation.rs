//! Read-only queries for observing the store: what every observation
//! surface (status, logs, live views) consumes.
//!
//! Observation never influences execution. Nothing here writes, starts a
//! session, or takes a session or recovery lock, so a poller can neither
//! make a dead session look alive nor hold back a scheduler barrier.
//! Unknown stays unknown: an invocation that has not ended has no usage
//! yet, which is neither unavailable nor zero.

use anyhow::Result;
use rusqlite::params;

use super::{
    AgentId, Event, GenerationId, INVOCATION_COLUMNS, InvocationId, InvocationState, PLAN_COLUMNS,
    Plan, PlanId, Role, Store, TaskId, Usage, invocation_row, plan_row,
};

/// The token usage of one invocation, attributed only through canonical
/// rows: a planner or plan-scoped verifier serves its plan alone, and what
/// a generation's agent serves is that generation's task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsageRecord {
    pub invocation: InvocationId,
    pub agent: AgentId,
    pub role: Role,
    pub plan: PlanId,
    pub task: Option<TaskId>,
    /// The generation and its number within the task.
    pub generation: Option<(GenerationId, i64)>,
    pub provider: String,
    pub model: String,
    pub state: InvocationState,
    pub started_at: i64,
    pub ended_at: Option<i64>,
    /// `None` exactly while no end is recorded: not yet known.
    pub usage: Option<Usage>,
}

/// Which events to list. Filters combine with AND.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventQuery {
    /// Only events after this sequence number.
    pub after: Option<i64>,
    pub plan: Option<PlanId>,
    /// Matches the event's own task.
    pub task: Option<TaskId>,
    pub agent: Option<AgentId>,
    /// An exact kind, or a prefix ending in `.` (`invocation.`). Always
    /// matched literally, never as a pattern.
    pub kind: Option<String>,
    pub limit: u32,
    /// The last `limit` matches instead of the first.
    pub newest: bool,
}

impl Store {
    /// Every plan, by id.
    pub fn plans(&self) -> Result<Vec<Plan>> {
        self.conn
            .prepare(&format!("{PLAN_COLUMNS} ORDER BY id"))?
            .query_map([], plan_row)?
            .collect::<rusqlite::Result<_>>()
            .map_err(Into::into)
    }

    /// The usage of every invocation, by id, read in one transaction.
    pub fn usage_records(&self) -> Result<Vec<UsageRecord>> {
        let tx = self.conn.unchecked_transaction()?;
        let invocations = tx
            .prepare(&format!("{INVOCATION_COLUMNS} ORDER BY id"))?
            .query_map([], invocation_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut subject = tx.prepare(
            "SELECT a.role, coalesce(a.plan_id, t.plan_id), g.task_id, g.id, g.number
             FROM agents a
             LEFT JOIN generations g ON g.id = a.generation_id
             LEFT JOIN tasks t ON t.id = g.task_id
             WHERE a.id = ?1",
        )?;
        let mut records = Vec::with_capacity(invocations.len());
        for i in invocations {
            let (role, plan, task, generation, number): (
                Role,
                PlanId,
                Option<TaskId>,
                Option<GenerationId>,
                Option<i64>,
            ) = subject.query_row([i.agent], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })?;
            records.push(UsageRecord {
                invocation: i.id,
                agent: i.agent,
                role,
                plan,
                task,
                generation: generation.zip(number),
                provider: i.provider,
                model: i.model,
                state: i.state,
                started_at: i.started_at,
                ended_at: i.ended_at,
                usage: i.end.map(|end| end.usage),
            });
        }
        Ok(records)
    }

    /// The events `query` selects, in ascending sequence order.
    pub fn query_events(&self, query: &EventQuery) -> Result<Vec<Event>> {
        let (kind, prefix) = match &query.kind {
            Some(kind) => (Some(kind.as_str()), kind.ends_with('.')),
            None => (None, false),
        };
        let mut events = self
            .conn
            .prepare(&format!(
                "SELECT seq, at, kind, plan_id, task_id, agent_id, detail FROM events
                 WHERE seq > ?1
                   AND (?2 IS NULL OR plan_id = ?2)
                   AND (?3 IS NULL OR task_id = ?3)
                   AND (?4 IS NULL OR agent_id = ?4)
                   AND (?5 IS NULL
                        OR (?6 AND substr(kind, 1, length(?5)) = ?5)
                        OR (NOT ?6 AND kind = ?5))
                 ORDER BY seq {} LIMIT ?7",
                if query.newest { "DESC" } else { "ASC" }
            ))?
            .query_map(
                params![
                    query.after.unwrap_or(i64::MIN),
                    query.plan,
                    query.task,
                    query.agent,
                    kind,
                    prefix,
                    query.limit
                ],
                |r| {
                    Ok(Event {
                        seq: r.get(0)?,
                        at: r.get(1)?,
                        kind: r.get(2)?,
                        plan: r.get(3)?,
                        task: r.get(4)?,
                        agent: r.get(5)?,
                        detail: r.get(6)?,
                    })
                },
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        if query.newest {
            events.reverse();
        }
        Ok(events)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, mpsc};
    use std::thread;
    use std::time::{Duration, Instant};

    use super::super::tests::{ended, objective, ready_plan, store, succeeded};
    use super::super::{AgentScope, FailureKind, InvocationEnd, TokenUsage};
    use super::*;
    use crate::project::Project;

    const DEADLINE: Duration = Duration::from_secs(60);

    fn all() -> EventQuery {
        EventQuery {
            after: None,
            plan: None,
            task: None,
            agent: None,
            kind: None,
            limit: 10_000,
            newest: false,
        }
    }

    fn tokens(input: u64, output: u64) -> TokenUsage {
        TokenUsage {
            input,
            output,
            cached_input: None,
            cache_write: None,
            reasoning: None,
        }
    }

    /// Runs one invocation of `agent` to its end.
    fn run(store: &mut Store, agent: AgentId, end: &InvocationEnd) -> InvocationId {
        let id = store.start_invocation(agent, "claude", "m", None).unwrap();
        store.invocation_running(id).unwrap();
        store.finish_invocation(id, end).unwrap();
        id
    }

    fn record(records: &[UsageRecord], id: InvocationId) -> UsageRecord {
        records.iter().find(|r| r.invocation == id).unwrap().clone()
    }

    #[test]
    fn opening_an_existing_store_never_creates_one() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        assert!(Store::open_existing(&path).is_err());
        assert!(!path.exists());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);

        drop(Store::open(&path).unwrap());
        assert!(Store::open_existing(&path).is_ok());
    }

    #[test]
    fn observing_a_project_without_a_store_creates_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let project = Project::create(dir.path(), crate::config::tests::sample()).unwrap();
        assert!(project.observe().unwrap().is_none());
        assert!(!dir.path().join(".agentctl").exists());
        assert!(!dir.path().join(".gitignore").exists());

        drop(project.hydrate().unwrap());
        assert!(project.observe().unwrap().is_some());
    }

    #[test]
    fn plans_are_listed_by_id() {
        let (_dir, mut store) = store();
        assert!(store.plans().unwrap().is_empty());
        let a = store.create_plan(&objective("a")).unwrap();
        let b = store.create_plan(&objective("b")).unwrap();
        let plans = store.plans().unwrap();
        assert_eq!(plans.iter().map(|p| p.id).collect::<Vec<_>>(), [a, b]);
        assert_eq!(plans[1], store.plan(b).unwrap());
    }

    #[test]
    fn usage_is_attributed_from_canonical_rows() {
        let (_dir, mut store) = store();
        let (plan, tasks) = ready_plan(&mut store, &[("t", &[], &[])]);
        let task = tasks[0];
        let planner = store
            .create_agent(Role::Planner, AgentScope::Plan(plan))
            .unwrap();
        let generation = store.start_generation(task).unwrap();
        let executor = store
            .create_agent(Role::Executor, AgentScope::Generation(generation))
            .unwrap();
        let verifier = store
            .create_agent(Role::Verifier, AgentScope::Generation(generation))
            .unwrap();
        let integrating = store
            .create_agent(Role::Verifier, AgentScope::Plan(plan))
            .unwrap();
        let other = store.create_plan(&objective("other")).unwrap();
        let other_planner = store
            .create_agent(Role::Planner, AgentScope::Plan(other))
            .unwrap();

        let done = InvocationEnd {
            usage: Usage::ProviderReported(tokens(1, 2)),
            ..succeeded()
        };
        let p = run(&mut store, planner, &done);
        let e = run(&mut store, executor, &done);
        let v = run(&mut store, verifier, &done);
        let i = run(&mut store, integrating, &done);
        let o = run(&mut store, other_planner, &done);
        let live = store.start_invocation(planner, "codex", "n", None).unwrap();

        let records = store.usage_records().unwrap();
        assert_eq!(
            records.iter().map(|r| r.invocation).collect::<Vec<_>>(),
            [p, e, v, i, o, live]
        );
        let shape = |id| {
            let r = record(&records, id);
            (r.role, r.plan, r.task, r.generation)
        };
        assert_eq!(shape(p), (Role::Planner, plan, None, None));
        assert_eq!(
            shape(e),
            (Role::Executor, plan, Some(task), Some((generation, 1)))
        );
        assert_eq!(
            shape(v),
            (Role::Verifier, plan, Some(task), Some((generation, 1)))
        );
        assert_eq!(shape(i), (Role::Verifier, plan, None, None));
        assert_eq!(shape(o), (Role::Planner, other, None, None));

        let pending = record(&records, live);
        assert_eq!(pending.usage, None);
        assert_eq!(pending.ended_at, None);
        assert_eq!(pending.state, InvocationState::Starting);
        assert_eq!(
            (pending.provider.as_str(), pending.model.as_str()),
            ("codex", "n")
        );
        let settled = record(&records, p);
        assert_eq!(settled.usage, Some(Usage::ProviderReported(tokens(1, 2))));
        assert!(settled.ended_at.is_some());
    }

    #[test]
    fn every_provenance_round_trips_exactly() {
        let (_dir, mut store) = store();
        let plan = store.create_plan(&objective("p")).unwrap();
        let mut ids = Vec::new();
        let cases = [
            Usage::ProviderReported(tokens(5, 6)),
            Usage::ProviderReported(TokenUsage {
                input: 10,
                output: 3,
                cached_input: Some(0),
                cache_write: Some(4),
                reasoning: Some(0),
            }),
            Usage::LocalEstimate(tokens(0, 0)),
            Usage::LocalEstimate(TokenUsage {
                input: 7,
                output: 8,
                cached_input: Some(2),
                cache_write: None,
                reasoning: Some(1),
            }),
            Usage::Unavailable,
        ];
        for usage in cases {
            let agent = store
                .create_agent(Role::Planner, AgentScope::Plan(plan))
                .unwrap();
            ids.push(run(
                &mut store,
                agent,
                &InvocationEnd {
                    usage,
                    ..succeeded()
                },
            ));
        }
        let records = store.usage_records().unwrap();
        for (id, usage) in ids.into_iter().zip(cases) {
            assert_eq!(record(&records, id).usage, Some(usage));
        }
    }

    #[test]
    fn records_and_events_survive_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        let (records, events) = {
            let mut store = Store::open(&path).unwrap();
            let plan = store.create_plan(&objective("p")).unwrap();
            let agent = store
                .create_agent(Role::Planner, AgentScope::Plan(plan))
                .unwrap();
            run(
                &mut store,
                agent,
                &InvocationEnd {
                    usage: Usage::LocalEstimate(tokens(9, 1)),
                    ..succeeded()
                },
            );
            store.start_invocation(agent, "claude", "m", None).unwrap();
            (
                store.usage_records().unwrap(),
                store.query_events(&all()).unwrap(),
            )
        };
        let store = Store::open_existing(&path).unwrap();
        assert_eq!(store.usage_records().unwrap(), records);
        assert_eq!(store.query_events(&all()).unwrap(), events);
        assert_eq!(records.len(), 2);
        assert_eq!(records[1].usage, None);
    }

    /// Two plans, one task and agent each, and a few invocations.
    fn populated() -> (tempfile::TempDir, Store, [PlanId; 2], TaskId, [AgentId; 2]) {
        let (dir, mut store) = store();
        let (a, tasks) = ready_plan(&mut store, &[("t", &[], &[])]);
        let b = store.create_plan(&objective("b")).unwrap();
        let generation = store.start_generation(tasks[0]).unwrap();
        let executor = store
            .create_agent(Role::Executor, AgentScope::Generation(generation))
            .unwrap();
        let planner = store
            .create_agent(Role::Planner, AgentScope::Plan(b))
            .unwrap();
        run(&mut store, executor, &succeeded());
        run(&mut store, planner, &ended(InvocationState::Cancelled));
        (dir, store, [a, b], tasks[0], [executor, planner])
    }

    #[test]
    fn events_are_ordered_and_filtered() {
        let (_dir, store, [a, b], task, [executor, planner]) = populated();
        let every = store.query_events(&all()).unwrap();
        assert!(every.windows(2).all(|w| w[0].seq < w[1].seq));
        assert_eq!(every, store.events_after(0, 10_000).unwrap());

        let only = |q: EventQuery| store.query_events(&q).unwrap();
        let plan_a = only(EventQuery {
            plan: Some(a),
            ..all()
        });
        assert!(!plan_a.is_empty() && plan_a.iter().all(|e| e.plan == Some(a)));
        assert!(
            only(EventQuery {
                plan: Some(b),
                ..all()
            })
            .iter()
            .all(|e| e.plan == Some(b))
        );
        let by_task = only(EventQuery {
            task: Some(task),
            ..all()
        });
        assert!(!by_task.is_empty() && by_task.iter().all(|e| e.task == Some(task)));
        let by_agent = only(EventQuery {
            agent: Some(planner),
            ..all()
        });
        assert!(!by_agent.is_empty() && by_agent.iter().all(|e| e.agent == Some(planner)));
        let exact = only(EventQuery {
            kind: Some("invocation.ended".into()),
            ..all()
        });
        assert_eq!(exact.len(), 2);
        let prefix = only(EventQuery {
            kind: Some("invocation.".into()),
            ..all()
        });
        assert!(prefix.len() > 2 && prefix.iter().all(|e| e.kind.starts_with("invocation.")));
        // A prefix must end in a dot; "invocation" is an exact kind.
        assert!(
            only(EventQuery {
                kind: Some("invocation".into()),
                ..all()
            })
            .is_empty()
        );

        // AND-combination.
        let both = only(EventQuery {
            plan: Some(a),
            kind: Some("invocation.".into()),
            agent: Some(executor),
            task: Some(task),
            ..all()
        });
        assert_eq!(both.len(), 3);
        assert!(
            only(EventQuery {
                plan: Some(b),
                task: Some(task),
                ..all()
            })
            .is_empty()
        );

        // `after` is exclusive.
        let mid = every[every.len() / 2].seq;
        let later = only(EventQuery {
            after: Some(mid),
            ..all()
        });
        assert_eq!(
            later,
            every
                .iter()
                .filter(|e| e.seq > mid)
                .cloned()
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn newest_takes_the_tail_in_ascending_order() {
        let (_dir, store, ..) = populated();
        let every = store.query_events(&all()).unwrap();
        let head = store
            .query_events(&EventQuery { limit: 3, ..all() })
            .unwrap();
        assert_eq!(head, every[..3]);
        let tail = store
            .query_events(&EventQuery {
                limit: 3,
                newest: true,
                ..all()
            })
            .unwrap();
        assert_eq!(tail, every[every.len() - 3..]);
        let after = every[2].seq;
        let tail_after = store
            .query_events(&EventQuery {
                after: Some(after),
                limit: 2,
                newest: true,
                ..all()
            })
            .unwrap();
        assert_eq!(tail_after, every[every.len() - 2..]);
        let first_after = store
            .query_events(&EventQuery {
                after: Some(after),
                limit: 2,
                ..all()
            })
            .unwrap();
        assert_eq!(first_after, every[3..5]);
        let none = store
            .query_events(&EventQuery {
                limit: 0,
                newest: true,
                ..all()
            })
            .unwrap();
        assert!(none.is_empty());
    }

    #[test]
    fn kind_matching_is_literal() {
        let (_dir, store, ..) = populated();
        for kind in [
            "%",
            "_",
            "*",
            "?",
            "[a-z]",
            "invocation.%",
            "invocation._nded",
            "inv%.",
            "%.",
            "_.",
            "*.",
            "?.",
            "[i].",
            "invocation.*",
            "invocation.end?d",
            "plan.crea[t]ed",
        ] {
            let found = store
                .query_events(&EventQuery {
                    kind: Some(kind.into()),
                    ..all()
                })
                .unwrap();
            assert!(found.is_empty(), "{kind} matched {found:?}");
        }
        assert!(
            !store
                .query_events(&EventQuery {
                    kind: Some("plan.created".into()),
                    ..all()
                })
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn ended_events_say_how_usage_was_established() {
        let (_dir, mut store) = store();
        let plan = store.create_plan(&objective("p")).unwrap();
        let mut id = |end: InvocationEnd| {
            let agent = store
                .create_agent(Role::Planner, AgentScope::Plan(plan))
                .unwrap();
            let id = run(&mut store, agent, &end);
            (
                id,
                store
                    .query_events(&EventQuery {
                        kind: Some("invocation.ended".into()),
                        newest: true,
                        limit: 1,
                        ..all()
                    })
                    .unwrap()
                    .remove(0)
                    .detail,
            )
        };
        let (n, detail) = id(InvocationEnd {
            usage: Usage::ProviderReported(tokens(1200, 300)),
            ..succeeded()
        });
        assert_eq!(
            detail,
            format!("invocation {n}: succeeded; usage provider_reported: 1200 in, 300 out")
        );
        let (n, detail) = id(InvocationEnd {
            failure: Some(FailureKind::MalformedOutput),
            diagnostic: Some("bad".into()),
            ..ended(InvocationState::Failed)
        });
        assert_eq!(
            detail,
            format!("invocation {n}: failed (malformed_output); usage unavailable")
        );
        let (n, detail) = id(InvocationEnd {
            usage: Usage::LocalEstimate(tokens(1200, 300)),
            ..succeeded()
        });
        assert_eq!(
            detail,
            format!("invocation {n}: succeeded; usage local_estimate: ~1200 in, ~300 out")
        );
    }

    #[test]
    fn a_reader_sees_every_event_once_while_writers_commit() {
        const WRITERS: usize = 4;
        const ROUNDS: usize = 6;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        drop(Store::open(&path).unwrap());
        let done = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel();

        let writers: Vec<_> = (0..WRITERS)
            .map(|n| {
                let path = path.clone();
                let tx = tx.clone();
                thread::spawn(move || {
                    let mut store = Store::open(&path).unwrap();
                    let plan = store.create_plan(&objective(&format!("plan {n}"))).unwrap();
                    let agent = store
                        .create_agent(Role::Planner, AgentScope::Plan(plan))
                        .unwrap();
                    for _ in 0..ROUNDS {
                        run(&mut store, agent, &succeeded());
                    }
                    tx.send(()).unwrap();
                })
            })
            .collect();
        drop(tx);

        let reader = {
            let (path, done) = (path.clone(), done.clone());
            thread::spawn(move || {
                let store = Store::open_existing(&path).unwrap();
                let start = Instant::now();
                let mut seen: Vec<Event> = Vec::new();
                loop {
                    let finished = done.load(Ordering::SeqCst);
                    let after = seen.last().map(|e| e.seq);
                    seen.extend(
                        store
                            .query_events(&EventQuery {
                                after,
                                limit: 7,
                                ..all()
                            })
                            .unwrap(),
                    );
                    let more = store
                        .query_events(&EventQuery {
                            after: seen.last().map(|e| e.seq),
                            limit: 1,
                            ..all()
                        })
                        .unwrap();
                    if finished && more.is_empty() {
                        return seen;
                    }
                    assert!(start.elapsed() < DEADLINE, "reader timed out");
                    thread::sleep(Duration::from_millis(1));
                }
            })
        };

        let start = Instant::now();
        for _ in 0..WRITERS {
            rx.recv_timeout(DEADLINE.saturating_sub(start.elapsed()))
                .expect("a writer finished in time");
        }
        for writer in writers {
            writer.join().unwrap();
        }
        done.store(true, Ordering::SeqCst);
        let seen = reader.join().unwrap();

        let store = Store::open_existing(&path).unwrap();
        let full = store.query_events(&all()).unwrap();
        assert_eq!(seen, full);
        assert!(seen.windows(2).all(|w| w[0].seq < w[1].seq));
        // plan.created + agent.created + started/running/ended per round, per writer.
        assert_eq!(full.len(), WRITERS * (2 + 3 * ROUNDS));
        assert_eq!(store.usage_records().unwrap().len(), WRITERS * ROUNDS);
    }
}
