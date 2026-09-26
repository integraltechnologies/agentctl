//! Mutation ownership: which generation holds exclusive authority to mutate
//! each canonical project path.
//!
//! A task's scope is what planning authorized its generations to mutate;
//! ownership is what one generation currently holds. A generation acquires
//! a set of paths within its task's scope as one transaction: all of them,
//! or, if another generation owns any, none. Acquiring more later is the
//! same operation, so a generation's ownership only ever grows by whole
//! sets. Paths are exact literals: owning one says nothing about any path
//! it contains, is contained by or would match as a pattern. Reading is
//! never restricted.
//!
//! Ownership is canonical state. Nothing but an explicit release ends it,
//! not a generation's end, not a change of scope and not reopening the
//! store: stale ownership is safe, and deciding when to release is for the
//! acceptance and recovery lifecycles.

use std::collections::BTreeSet;

use anyhow::{Result, ensure};
use rusqlite::{Connection, OptionalExtension, Transaction, params};

use super::scheduling::scheduled;
use super::{
    GenerationId, GenerationState, PlanId, PlanState, Store, TaskId, active_generation, check_path,
    event, generation_info, plan_state,
};

/// The generation owning a path, with the task and plan it works for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Owner {
    pub generation: GenerationId,
    pub task: TaskId,
    pub plan: PlanId,
}

/// A requested path another generation owns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conflict {
    pub path: String,
    pub owner: Owner,
}

/// What a request for ownership established.
#[derive(Debug, Clone, PartialEq, Eq)]
#[must_use]
pub enum Acquisition {
    /// The generation owns every requested path.
    Acquired,
    /// Other generations own these requested paths, in path order, so the
    /// generation acquired none of them.
    Conflicted(Vec<Conflict>),
}

impl Store {
    /// Acquires ownership of `paths` for an active generation: every one of
    /// them, or, if another generation owns any, none. Paths the generation
    /// already owns, or requests twice, are acquired once.
    ///
    /// Each path must be in the current scope of the generation's task, and
    /// scopes authorize nothing while their plan is being planned. A refused
    /// request, like a conflicted one, changes nothing.
    pub fn acquire_ownership(
        &mut self,
        generation: GenerationId,
        paths: &[&str],
    ) -> Result<Acquisition> {
        self.write(|tx| acquire(tx, generation, paths))
    }

    /// Releases every path an ended generation owns, and only those. When
    /// that is safe is for the caller's acceptance or recovery lifecycle to
    /// decide. An ended generation never acquires again. A scheduled
    /// generation's ownership is released only by completing its
    /// acceptance or by the replan abandoning it, never here.
    pub fn release_ownership(&mut self, generation: GenerationId) -> Result<()> {
        self.write(|tx| {
            let (_, _, _, state) = generation_info(tx, generation)?;
            ensure!(
                state != GenerationState::Active,
                "generation {generation} is still active"
            );
            ensure!(
                !scheduled(tx, generation)?,
                "generation {generation} was scheduled: only accepting it, or a replan \
                 abandoning it, releases its ownership"
            );
            release(tx, generation)
        })
    }

    /// Who owns `path`, if anyone.
    pub fn owner(&self, path: &str) -> Result<Option<Owner>> {
        owner(&self.conn, path)
    }

    /// Every path `generation` owns, in order.
    pub fn owned_paths(&self, generation: GenerationId) -> Result<Vec<String>> {
        self.conn
            .prepare("SELECT path FROM ownership WHERE generation_id = ?1 ORDER BY path")?
            .query_map([generation], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()
            .map_err(Into::into)
    }
}

/// Releases every path `generation` owns within `tx`, and only those. The
/// caller establishes that it may; see [`Store::release_ownership`].
pub(super) fn release(tx: &Transaction, generation: GenerationId) -> Result<()> {
    let (plan, task, number, _) = generation_info(tx, generation)?;
    let released = tx.execute(
        "DELETE FROM ownership WHERE generation_id = ?1",
        [generation],
    )?;
    if released == 0 {
        return Ok(());
    }
    let detail = format!("generation {number}: {released} paths");
    event(
        tx,
        "ownership.released",
        Some(plan),
        Some(task),
        None,
        &detail,
    )
}

/// Acquires `paths` for `generation` within `tx`; see
/// [`Store::acquire_ownership`]. A conflicted or refused request writes
/// nothing.
pub(super) fn acquire(
    tx: &Transaction,
    generation: GenerationId,
    paths: &[&str],
) -> Result<Acquisition> {
    let (plan, task, number) = active_generation(tx, generation)?;
    let state = plan_state(tx, plan)?;
    ensure!(
        state != PlanState::Planning,
        "plan {plan} is being planned, so its scopes authorize no mutation"
    );
    let mut free = Vec::new();
    let mut conflicts = Vec::new();
    for path in paths.iter().copied().collect::<BTreeSet<_>>() {
        check_path(path)?;
        let authorized: bool = tx.query_row(
            "SELECT EXISTS (SELECT 1 FROM task_scope WHERE task_id = ?1 AND path = ?2)",
            params![task, path],
            |r| r.get(0),
        )?;
        ensure!(authorized, "`{path}` is not in the scope of task {task}");
        match owner(tx, path)? {
            None => free.push(path),
            Some(owner) if owner.generation == generation => {}
            Some(owner) => conflicts.push(Conflict {
                path: path.into(),
                owner,
            }),
        }
    }
    if !conflicts.is_empty() {
        return Ok(Acquisition::Conflicted(conflicts));
    }
    if free.is_empty() {
        return Ok(Acquisition::Acquired);
    }
    let kind = match owns_any(tx, generation)? {
        true => "ownership.expanded",
        false => "ownership.acquired",
    };
    for path in &free {
        tx.execute(
            "INSERT INTO ownership (path, generation_id) VALUES (?1, ?2)",
            params![path, generation],
        )?;
    }
    let detail = format!("generation {number}: {} paths", free.len());
    event(tx, kind, Some(plan), Some(task), None, &detail)?;
    Ok(Acquisition::Acquired)
}

pub(super) fn owner(conn: &Connection, path: &str) -> Result<Option<Owner>> {
    conn.query_row(
        "SELECT o.generation_id, g.task_id, t.plan_id FROM ownership o
         JOIN generations g ON g.id = o.generation_id JOIN tasks t ON t.id = g.task_id
         WHERE o.path = ?1",
        [path],
        |r| {
            Ok(Owner {
                generation: r.get(0)?,
                task: r.get(1)?,
                plan: r.get(2)?,
            })
        },
    )
    .optional()
    .map_err(Into::into)
}

fn owns_any(conn: &Connection, generation: GenerationId) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM ownership WHERE generation_id = ?1)",
        [generation],
        |r| r.get(0),
    )?)
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Barrier};
    use std::thread;

    use super::*;
    use crate::planner::Command;
    use crate::state::GenerationEnd;
    use crate::state::tests::{acquire, err, objective, ready_plan, store};

    /// The paths and owning generations of a conflicted acquisition.
    fn conflicts(acquisition: Acquisition) -> Vec<(String, GenerationId)> {
        match acquisition {
            Acquisition::Conflicted(conflicts) => conflicts
                .into_iter()
                .map(|c| (c.path, c.owner.generation))
                .collect(),
            Acquisition::Acquired => panic!("acquired"),
        }
    }

    fn events(store: &Store) -> Vec<String> {
        let events = store.events_after(0, 10_000).unwrap();
        events.into_iter().map(|e| e.kind).collect()
    }

    fn start(store: &mut Store, tasks: &[TaskId]) -> Vec<GenerationId> {
        tasks
            .iter()
            .map(|&task| store.start_generation(task).unwrap())
            .collect()
    }

    /// Returns a plan in planning again, as replanning would.
    fn replan(store: &mut Store, plan: PlanId, commands: &[Command]) {
        store.set_plan_state(plan, PlanState::Running).unwrap();
        store.set_plan_state(plan, PlanState::Planning).unwrap();
        store.revise_plan(plan, commands, &|_| Ok(())).unwrap();
    }

    fn set_paths(task: &str, paths: &[&str]) -> Command {
        Command::UpdateTask {
            task: task.into(),
            objective: None,
            context: None,
            paths: Some(paths.iter().map(|&p| p.into()).collect()),
        }
    }

    #[test]
    fn acquires_whole_sets_once() {
        let (_dir, mut store) = store();
        let tasks: [(&str, &[&str], &[&str]); 3] = [
            ("one", &["src/a.rs"], &[]),
            ("many", &["src/b.rs", "src/c.rs", "src/d.rs"], &[]),
            ("read", &[], &[]),
        ];
        let (plan, tasks) = ready_plan(&mut store, &tasks);
        let [one, many, read] = start(&mut store, &tasks)[..] else {
            panic!()
        };

        acquire(&mut store, one, &["src/a.rs"]);
        let owner = Owner {
            generation: one,
            task: tasks[0],
            plan,
        };
        assert_eq!(store.owner("src/a.rs").unwrap(), Some(owner));
        assert_eq!(store.owned_paths(one).unwrap(), ["src/a.rs"]);

        // A request is a set: order and repetition do not matter.
        acquire(
            &mut store,
            many,
            &["src/d.rs", "src/b.rs", "src/c.rs", "src/b.rs"],
        );
        let owned = ["src/b.rs", "src/c.rs", "src/d.rs"];
        assert_eq!(store.owned_paths(many).unwrap(), owned);
        let before = events(&store);
        assert_eq!(
            before.iter().filter(|k| *k == "ownership.acquired").count(),
            2
        );

        // Acquiring what is already owned, or nothing, changes nothing.
        acquire(&mut store, many, &["src/c.rs", "src/b.rs"]);
        acquire(&mut store, many, &[]);
        acquire(&mut store, read, &[]);
        assert_eq!(store.owned_paths(many).unwrap(), owned);
        assert!(store.owned_paths(read).unwrap().is_empty());
        assert_eq!(events(&store), before);
    }

    #[test]
    fn overlapping_requests_acquire_nothing() {
        let (_dir, mut store) = store();
        let tasks: [(&str, &[&str], &[&str]); 3] = [
            ("a", &["src/a.rs", "src/shared.rs"], &[]),
            ("b", &["src/b.rs", "src/shared.rs"], &[]),
            ("c", &["src/c.rs"], &[]),
        ];
        let (plan, tasks) = ready_plan(&mut store, &tasks);
        let [a, b, c] = start(&mut store, &tasks)[..] else {
            panic!()
        };
        let other_tasks: [(&str, &[&str], &[&str]); 1] =
            [("x", &["src/x.rs", "src/shared.rs", "src/a.rs"], &[])];
        let (_, other_tasks) = ready_plan(&mut store, &other_tasks);
        let x = store.start_generation(other_tasks[0]).unwrap();

        acquire(&mut store, a, &["src/a.rs", "src/shared.rs"]);
        let before = events(&store);
        let conflicted = store
            .acquire_ownership(b, &["src/b.rs", "src/shared.rs"])
            .unwrap();
        let Acquisition::Conflicted(found) = conflicted else {
            panic!("{conflicted:?}")
        };
        let owner = Owner {
            generation: a,
            task: tasks[0],
            plan,
        };
        let expected = Conflict {
            path: "src/shared.rs".into(),
            owner,
        };
        assert_eq!(found, [expected]);
        assert!(store.owned_paths(b).unwrap().is_empty());
        assert_eq!(store.owner("src/b.rs").unwrap(), None);
        assert_eq!(events(&store), before, "a conflict records nothing");

        // Disjoint sets coexist; plans do not partition ownership, and every
        // conflict is reported.
        acquire(&mut store, c, &["src/c.rs"]);
        let requested = ["src/x.rs", "src/shared.rs", "src/a.rs"];
        let found = conflicts(store.acquire_ownership(x, &requested).unwrap());
        let expected = [("src/a.rs".into(), a), ("src/shared.rs".into(), a)];
        assert_eq!(found, expected);
        assert_eq!(store.owner("src/x.rs").unwrap(), None);
        assert_eq!(store.owned_paths(a).unwrap(), ["src/a.rs", "src/shared.rs"]);
    }

    #[test]
    fn paths_are_exact_literals() {
        let specials = [
            "src/[slug].rs",
            "src/a*b.rs",
            "src/a?b.rs",
            "src/**/x.rs",
            "src/(group).rs",
            "src/a+b.rs",
            "src/file name.rs",
            "src/日本語.rs",
        ];
        // What each would match or contain as a pattern or directory.
        let lookalikes = [
            "src/s.rs",
            "src/slug.rs",
            "src/axb.rs",
            "src/ab.rs",
            "src/aab.rs",
            "src/x.rs",
            "src/dir/x.rs",
            "src/group.rs",
            "src/file",
            "src",
        ];
        let (_dir, mut store) = store();
        let tasks: [(&str, &[&str], &[&str]); 3] = [
            ("special", &specials, &[]),
            ("lookalike", &lookalikes, &[]),
            ("again", &specials, &[]),
        ];
        let (_, tasks) = ready_plan(&mut store, &tasks);
        let [special, lookalike, again] = start(&mut store, &tasks)[..] else {
            panic!()
        };

        acquire(&mut store, special, &specials);
        acquire(&mut store, lookalike, &lookalikes);
        let mut owned = specials.map(String::from).to_vec();
        owned.sort();
        assert_eq!(store.owned_paths(special).unwrap(), owned);
        for path in specials {
            let found = conflicts(store.acquire_ownership(again, &[path]).unwrap());
            assert_eq!(found, [(path.into(), special)], "{path}");
        }
        for path in lookalikes {
            let owner = store.owner(path).unwrap().unwrap();
            assert_eq!(owner.generation, lookalike, "{path}");
        }
        assert!(store.owned_paths(again).unwrap().is_empty());
    }

    #[test]
    fn refuses_unauthorized_requests_changing_nothing() {
        let (_dir, mut store) = store();
        let tasks: [(&str, &[&str], &[&str]); 2] =
            [("a", &["src/a.rs"], &[]), ("b", &["src/b.rs"], &[])];
        let (_, tasks) = ready_plan(&mut store, &tasks);
        let [a, _] = start(&mut store, &tasks)[..] else {
            panic!()
        };
        let before = events(&store);

        for request in [&["src/a.rs", "src/b.rs"][..], &["src"], &["src/a"]] {
            let message = err(store.acquire_ownership(a, request));
            assert!(message.contains("not in the scope of task"), "{message}");
        }
        for bad in [
            "",
            "/src/a.rs",
            "src//a.rs",
            "./src/a.rs",
            "src/../src/a.rs",
            "src/a.rs/",
            "a\0b",
        ] {
            let message = err(store.acquire_ownership(a, &["src/a.rs", bad]));
            assert!(message.contains("not a canonical"), "{bad:?}: {message}");
        }
        assert!(store.owned_paths(a).unwrap().is_empty());
        assert_eq!(events(&store), before);

        let message = err(store.acquire_ownership(GenerationId(999), &[]));
        assert!(message.contains("does not exist"), "{message}");
        store.finish_generation(a, GenerationEnd::Failed).unwrap();
        let message = err(store.acquire_ownership(a, &["src/a.rs"]));
        assert!(message.contains("already ended"), "{message}");

        // Scopes a planner has not finalized authorize nothing.
        let plan = store.create_plan(&objective("draft")).unwrap();
        let draft = store.add_task(plan, "draft", &[]).unwrap();
        let generation = store.start_generation(draft).unwrap();
        let message = err(store.acquire_ownership(generation, &[]));
        assert!(message.contains("is being planned"), "{message}");
    }

    #[test]
    fn expansion_is_atomic_and_needs_authorization() {
        let (_dir, mut store) = store();
        let tasks: [(&str, &[&str], &[&str]); 2] = [
            ("a", &["src/a1.rs", "src/a2.rs", "src/shared.rs"], &[]),
            ("b", &["src/shared.rs"], &[]),
        ];
        let (plan, tasks) = ready_plan(&mut store, &tasks);
        let [a, b] = start(&mut store, &tasks)[..] else {
            panic!()
        };
        acquire(&mut store, a, &["src/a1.rs"]);
        acquire(&mut store, b, &["src/shared.rs"]);

        let before = events(&store);
        let found = conflicts(
            store
                .acquire_ownership(a, &["src/a2.rs", "src/shared.rs"])
                .unwrap(),
        );
        assert_eq!(found, [("src/shared.rs".into(), b)]);
        assert_eq!(store.owned_paths(a).unwrap(), ["src/a1.rs"]);
        assert_eq!(events(&store), before);
        acquire(&mut store, a, &["src/a1.rs", "src/a2.rs"]);
        assert_eq!(store.owned_paths(a).unwrap(), ["src/a1.rs", "src/a2.rs"]);
        assert_eq!(events(&store).last().unwrap(), "ownership.expanded");

        // Replanning away an owned path neither releases nor transfers it,
        // and while planning, no scope authorizes anything.
        let add = Command::AddTask {
            task: "c".into(),
            objective: "c".into(),
            context: String::new(),
            paths: vec!["src/a2.rs".into()],
            depends_on: Vec::new(),
        };
        replan(&mut store, plan, &[set_paths("a", &["src/a1.rs"]), add]);
        let message = err(store.acquire_ownership(a, &["src/a1.rs"]));
        assert!(message.contains("is being planned"), "{message}");
        store
            .revise_plan(plan, &[Command::Finalize {}], &|_| Ok(()))
            .unwrap();
        assert_eq!(store.owned_paths(a).unwrap(), ["src/a1.rs", "src/a2.rs"]);
        let c = store.tasks(plan).unwrap()[2].id;
        let c = store.start_generation(c).unwrap();
        let found = conflicts(store.acquire_ownership(c, &["src/a2.rs"]).unwrap());
        assert_eq!(found, [("src/a2.rs".into(), a)]);
        let message = err(store.acquire_ownership(a, &["src/a2.rs"]));
        assert!(message.contains("not in the scope"), "{message}");

        // Once planning authorizes a new path, the generation may expand to it.
        let widen = set_paths("a", &["src/a1.rs", "src/a3.rs"]);
        replan(&mut store, plan, &[widen, Command::Finalize {}]);
        acquire(&mut store, a, &["src/a3.rs"]);
        let owned = ["src/a1.rs", "src/a2.rs", "src/a3.rs"];
        assert_eq!(store.owned_paths(a).unwrap(), owned);
    }

    #[test]
    fn release_is_explicit_and_generation_scoped() {
        let (_dir, mut store) = store();
        let tasks: [(&str, &[&str], &[&str]); 2] =
            [("a", &["src/a.rs"], &[]), ("b", &["src/b.rs"], &[])];
        let (_, tasks) = ready_plan(&mut store, &tasks);
        let [a, b] = start(&mut store, &tasks)[..] else {
            panic!()
        };
        acquire(&mut store, a, &["src/a.rs"]);
        acquire(&mut store, b, &["src/b.rs"]);
        store.finish_generation(a, GenerationEnd::Failed).unwrap();
        assert_eq!(store.owned_paths(a).unwrap(), ["src/a.rs"]);

        let message = err(store.release_ownership(b));
        assert!(message.contains("still active"), "{message}");
        store.release_ownership(a).unwrap();
        assert!(store.owned_paths(a).unwrap().is_empty());
        assert_eq!(store.owned_paths(b).unwrap(), ["src/b.rs"]);
        assert_eq!(events(&store).last().unwrap(), "ownership.released");
        let before = events(&store);
        store.release_ownership(a).unwrap();
        assert_eq!(events(&store), before, "nothing more to release");

        // A released generation has ended, so never acquires again; the
        // task's next generation may.
        let message = err(store.acquire_ownership(a, &["src/a.rs"]));
        assert!(message.contains("already ended"), "{message}");
        let retry = store.start_generation(tasks[0]).unwrap();
        acquire(&mut store, retry, &["src/a.rs"]);
    }

    #[test]
    fn ownership_survives_the_process() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        let mut store = Store::open(&path).unwrap();
        let tasks: [(&str, &[&str], &[&str]); 2] = [
            ("a", &["src/a.rs", "src/shared.rs"], &[]),
            ("b", &["src/shared.rs"], &[]),
        ];
        let (_, tasks) = ready_plan(&mut store, &tasks);
        let [a, b] = start(&mut store, &tasks)[..] else {
            panic!()
        };
        acquire(&mut store, a, &["src/a.rs", "src/shared.rs"]);
        store.finish_generation(a, GenerationEnd::Failed).unwrap();
        // Vanish without closing, as a killed process would.
        std::mem::forget(store);

        let mut store = Store::open(&path).unwrap();
        assert_eq!(store.owned_paths(a).unwrap(), ["src/a.rs", "src/shared.rs"]);
        let found = conflicts(store.acquire_ownership(b, &["src/shared.rs"]).unwrap());
        assert_eq!(found, [("src/shared.rs".into(), a)]);
    }

    #[test]
    fn racing_generations_never_split_a_set() {
        const CONTENDERS: usize = 4;
        const ROUNDS: usize = 16;
        let dir = tempfile::tempdir().unwrap();
        let path = Arc::new(dir.path().join("state.db"));
        let mut store = Store::open(&path).unwrap();
        let scopes: Vec<(String, [String; 2])> = (0..ROUNDS)
            .flat_map(|r| {
                (0..CONTENDERS).map(move |c| {
                    let own = format!("src/{r}/{c}.rs");
                    (format!("t{r}-{c}"), [own, format!("src/{r}/shared.rs")])
                })
            })
            .collect();
        let borrowed: Vec<[&str; 2]> = scopes
            .iter()
            .map(|(_, [own, shared])| [own.as_str(), shared.as_str()])
            .collect();
        let tasks: Vec<(&str, &[&str], &[&str])> = scopes
            .iter()
            .zip(&borrowed)
            .map(|((key, _), paths)| (key.as_str(), &paths[..], &[][..]))
            .collect();
        let (_, tasks) = ready_plan(&mut store, &tasks);
        let generations = start(&mut store, &tasks);

        for (round, generations) in generations.chunks(CONTENDERS).enumerate() {
            let barrier = Arc::new(Barrier::new(CONTENDERS));
            let results: Vec<_> = generations
                .iter()
                .zip(&borrowed[round * CONTENDERS..])
                .map(|(&generation, &paths)| {
                    let (path, barrier) = (path.clone(), barrier.clone());
                    let paths = paths.map(String::from);
                    thread::spawn(move || {
                        let mut store = Store::open(&path).unwrap();
                        barrier.wait();
                        let paths = paths.each_ref().map(String::as_str);
                        store.acquire_ownership(generation, &paths).unwrap()
                    })
                })
                .collect::<Vec<_>>()
                .into_iter()
                .map(|h| h.join().unwrap())
                .collect();

            let winners: Vec<_> = (0..CONTENDERS)
                .filter(|&i| results[i] == Acquisition::Acquired)
                .collect();
            assert_eq!(winners.len(), 1, "round {round}: {results:?}");
            let winner = generations[winners[0]];
            let shared = format!("src/{round}/shared.rs");
            for (i, (&generation, result)) in generations.iter().zip(results).enumerate() {
                let owned = store.owned_paths(generation).unwrap();
                if generation == winner {
                    assert_eq!(owned, borrowed[round * CONTENDERS + i]);
                } else {
                    assert!(owned.is_empty(), "round {round}: {owned:?}");
                    assert_eq!(conflicts(result), [(shared.clone(), winner)]);
                }
            }
        }
    }

    #[test]
    fn the_database_refuses_unacquired_ownership() {
        let (_dir, mut store) = store();
        let tasks: [(&str, &[&str], &[&str]); 2] = [
            ("a", &["src/a.rs", "src/b.rs"], &[]),
            ("b", &["src/a.rs"], &[]),
        ];
        let (_, tasks) = ready_plan(&mut store, &tasks);
        let [a, b] = start(&mut store, &tasks)[..] else {
            panic!()
        };
        acquire(&mut store, a, &["src/a.rs"]);

        let attacks = [
            format!("INSERT INTO ownership VALUES ('src/a.rs', {b})"),
            format!("INSERT OR REPLACE INTO ownership VALUES ('src/a.rs', {b})"),
            format!(
                "INSERT INTO ownership VALUES ('src/a.rs', {b})
                 ON CONFLICT (path) DO UPDATE SET generation_id = excluded.generation_id"
            ),
            format!("UPDATE ownership SET generation_id = {b}"),
            format!("INSERT INTO ownership VALUES ('src/c.rs', {a})"),
        ];
        store.finish_generation(a, GenerationEnd::Failed).unwrap();
        let ended = format!("INSERT INTO ownership VALUES ('src/b.rs', {a})");
        for sql in attacks.iter().chain([&ended]) {
            let message = store.conn.execute(sql, []).unwrap_err().to_string();
            assert!(message.starts_with("ownership is"), "{sql}: {message}");
        }

        assert_eq!(store.owned_paths(a).unwrap(), ["src/a.rs"]);
        assert!(store.owned_paths(b).unwrap().is_empty());
    }

    #[test]
    fn the_database_refuses_ownership_from_draft_scope() {
        let (_dir, mut store) = store();
        let sql = |store: &Store, sql: &str| store.conn.execute(sql, []);
        // Everything a planner has drafted, written directly.
        sql(
            &store,
            "INSERT INTO plans (objective, constraints, completion_criteria,
                 state, created_at, updated_at)
             VALUES ('draft', '[]', '[]', 'planning', 0, 0)",
        )
        .unwrap();
        let plan = PlanId(store.conn.last_insert_rowid());
        sql(
            &store,
            &format!(
                "INSERT INTO tasks (plan_id, key, objective, context, created_at)
                 VALUES ({plan}, 'draft', 'draft', '', 0)"
            ),
        )
        .unwrap();
        let task = store.conn.last_insert_rowid();
        sql(
            &store,
            &format!("INSERT INTO task_scope VALUES ({task}, 'src/a.rs'), ({task}, 'src/b.rs')"),
        )
        .unwrap();
        sql(
            &store,
            &format!(
                "INSERT INTO generations (task_id, number, state, started_at)
                 VALUES ({task}, 1, 'active', 0)"
            ),
        )
        .unwrap();
        let generation = GenerationId(store.conn.last_insert_rowid());

        let attacks = |path: &str| {
            [
                format!("INSERT INTO ownership VALUES ('{path}', {generation})"),
                format!("INSERT OR REPLACE INTO ownership VALUES ('{path}', {generation})"),
                format!(
                    "INSERT INTO ownership VALUES ('{path}', {generation})
                     ON CONFLICT (path) DO UPDATE SET generation_id = excluded.generation_id"
                ),
                format!("INSERT OR IGNORE INTO ownership VALUES ('{path}', {generation})"),
            ]
        };
        for attack in attacks("src/a.rs") {
            let message = sql(&store, &attack).unwrap_err().to_string();
            assert!(message.starts_with("ownership is"), "{attack}: {message}");
        }
        assert!(store.owned_paths(generation).unwrap().is_empty());

        // Finalized, the same scope authorizes the same acquisition.
        store
            .revise_plan(plan, &[Command::Finalize {}], &|_| Ok(()))
            .unwrap();
        assert_eq!(store.plan(plan).unwrap().state, PlanState::Ready);
        sql(&store, &attacks("src/a.rs")[0]).unwrap();
        assert_eq!(store.owned_paths(generation).unwrap(), ["src/a.rs"]);

        // Replanning makes the scope a draft again, even for paths the
        // finalized scope already requested.
        let add = set_paths("draft", &["src/a.rs", "src/b.rs", "src/c.rs"]);
        replan(&mut store, plan, &[add]);
        for path in ["src/b.rs", "src/c.rs"] {
            for attack in attacks(path) {
                let message = sql(&store, &attack).unwrap_err().to_string();
                assert!(message.starts_with("ownership is"), "{attack}: {message}");
            }
        }
        assert_eq!(store.owned_paths(generation).unwrap(), ["src/a.rs"]);
    }
}
