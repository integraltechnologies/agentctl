//! Planning persistence: a plan's task DAG as planners revise it, and its
//! finalization. A revision is one transaction: each command is validated
//! against the state its predecessors leave, and either every command
//! applies or none does.

use std::collections::HashSet;

use anyhow::{Context, Result, anyhow, bail, ensure};
use rusqlite::{OptionalExtension, Transaction, params};

use super::{
    AgentId, PlanId, PlanState, Role, Store, TaskId, agent_subject, check_path, check_text, depend,
    event, now, plan_state,
};
use crate::planner::{Command, Rejection};

/// Bounds on what a plan's tasks hold.
const TASKS_LIMIT: usize = 256;
const KEY_LIMIT: usize = 64;
const OBJECTIVE_LIMIT: usize = 2048;
const CONTEXT_LIMIT: usize = 16 * 1024;
const SCOPE_LIMIT: usize = 256;
const PATH_LIMIT: usize = 4096;

impl Store {
    /// Applies a revision a planner proposed for a planning plan, returning
    /// whether it finalized the plan, which then is ready. `scope` checks a
    /// requested path against the project; finalizing checks every task's
    /// again. A refused revision changes nothing, and its error carries the
    /// [`Rejection`].
    pub(crate) fn revise_plan(
        &mut self,
        plan: PlanId,
        commands: &[Command],
        scope: &dyn Fn(&str) -> Result<()>,
    ) -> Result<bool> {
        self.write(|tx| {
            let state = plan_state(tx, plan)?;
            if state != PlanState::Planning {
                let why = anyhow!("plan {plan} is {state}; only a planning plan is revised");
                return Err(Rejection::response(why).into());
            }
            let mut ready = false;
            for (i, command) in commands.iter().enumerate() {
                let applied = match ready {
                    true => Err(anyhow!("the plan was already finalized")),
                    false => apply(tx, plan, command, scope),
                };
                ready = applied.map_err(|why| Rejection::command(i + 1, command.op(), why))?;
            }
            let detail = format!("{} commands applied", commands.len());
            event(tx, "plan.revised", Some(plan), None, None, &detail)?;
            if ready {
                tx.execute(
                    "UPDATE plans SET state = ?2, updated_at = ?3 WHERE id = ?1",
                    params![plan, PlanState::Ready, now()],
                )?;
                let detail = format!("{} -> {}", PlanState::Planning, PlanState::Ready);
                event(tx, "plan.state", Some(plan), None, None, &detail)?;
            }
            Ok(ready)
        })
    }

    /// The logical planner of a plan, created when it has none. Whatever
    /// embodies it starts from the plan's canonical state alone.
    pub fn planner(&mut self, plan: PlanId) -> Result<AgentId> {
        self.write(|tx| {
            plan_state(tx, plan)?;
            let existing: Option<AgentId> = tx.query_row(
                "SELECT min(id) FROM agents WHERE role = ?1 AND plan_id = ?2",
                params![Role::Planner, plan],
                |r| r.get(0),
            )?;
            if let Some(agent) = existing {
                return Ok(agent);
            }
            tx.execute(
                "INSERT INTO agents (role, plan_id, created_at) VALUES (?1, ?2, ?3)",
                params![Role::Planner, plan, now()],
            )?;
            let agent = AgentId(tx.last_insert_rowid());
            let role = Role::Planner.to_string();
            event(tx, "agent.created", Some(plan), None, Some(agent), &role)?;
            Ok(agent)
        })
    }

    /// Records that what a planner proposed was refused. `detail` is
    /// agentctl's account, never the planner's words.
    pub(crate) fn planner_refused(&mut self, agent: AgentId, detail: &str) -> Result<()> {
        self.write(|tx| {
            let (plan, _) = agent_subject(tx, agent)?;
            event(tx, "planner.refused", Some(plan), None, Some(agent), detail)
        })
    }
}

/// Applies one command, returning whether it finalized the plan.
pub(super) fn apply(
    tx: &Transaction,
    plan: PlanId,
    command: &Command,
    scope: &dyn Fn(&str) -> Result<()>,
) -> Result<bool> {
    match command {
        Command::AddTask {
            task,
            objective,
            context,
            paths,
            depends_on,
        } => {
            check_key(task)?;
            ensure!(
                find(tx, plan, task)?.is_none(),
                "task `{task}` already exists"
            );
            let count: i64 = tx.query_row(
                "SELECT count(*) FROM tasks WHERE plan_id = ?1",
                [plan],
                |r| r.get(0),
            )?;
            ensure!(
                count < TASKS_LIMIT as i64,
                "a plan has at most {TASKS_LIMIT} tasks"
            );
            check_objective(objective)?;
            check_context(context)?;
            tx.execute(
                "INSERT INTO tasks (plan_id, key, objective, context, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![plan, task, objective, context, now()],
            )?;
            let id = TaskId(tx.last_insert_rowid());
            set_scope(tx, id, paths, scope)?;
            set_dependencies(tx, plan, id, task, depends_on)?;
        }
        Command::UpdateTask {
            task,
            objective,
            context,
            paths,
        } => {
            let id = lookup(tx, plan, task)?;
            if let Some(objective) = objective {
                check_objective(objective)?;
                tx.execute(
                    "UPDATE tasks SET objective = ?2 WHERE id = ?1",
                    params![id, objective],
                )?;
            }
            if let Some(context) = context {
                check_context(context)?;
                tx.execute(
                    "UPDATE tasks SET context = ?2 WHERE id = ?1",
                    params![id, context],
                )?;
            }
            if let Some(paths) = paths {
                tx.execute("DELETE FROM task_scope WHERE task_id = ?1", [id])?;
                set_scope(tx, id, paths, scope)?;
            }
        }
        Command::RemoveTask { task } => {
            let id = lookup(tx, plan, task)?;
            let dependents: Vec<String> = tx
                .prepare(
                    "SELECT t.key FROM task_dependencies d JOIN tasks t ON t.id = d.task_id
                     WHERE d.depends_on = ?1 ORDER BY t.key",
                )?
                .query_map([id], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?;
            ensure!(
                dependents.is_empty(),
                "task `{task}` cannot be removed while `{}` depends on it",
                dependents.join("`, `")
            );
            tx.execute("DELETE FROM task_dependencies WHERE task_id = ?1", [id])?;
            tx.execute("DELETE FROM tasks WHERE id = ?1", [id])
                .with_context(|| format!("removing task `{task}`"))?;
        }
        Command::SetDependencies { task, depends_on } => {
            let id = lookup(tx, plan, task)?;
            tx.execute("DELETE FROM task_dependencies WHERE task_id = ?1", [id])?;
            set_dependencies(tx, plan, id, task, depends_on)?;
        }
        Command::Finalize {} => {
            check_ready(tx, plan, scope)?;
            return Ok(true);
        }
        Command::CancelTask { .. } | Command::RetryTask { .. } => {
            bail!("only a finalized plan's tasks are cancelled or retried")
        }
    }
    Ok(false)
}

/// Checks the plan is a structurally executable DAG: at least one task,
/// each with an objective, valid context and a valid requested scope, and
/// no dependency cycle.
fn check_ready(tx: &Transaction, plan: PlanId, scope: &dyn Fn(&str) -> Result<()>) -> Result<()> {
    let tasks: Vec<(TaskId, String, String, String)> = tx
        .prepare("SELECT id, key, objective, context FROM tasks WHERE plan_id = ?1 ORDER BY id")?
        .query_map([plan], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
        .collect::<rusqlite::Result<_>>()?;
    ensure!(
        !tasks.is_empty(),
        "a plan without tasks cannot be finalized"
    );
    for (id, key, objective, context) in tasks {
        let paths: Vec<String> = tx
            .prepare("SELECT path FROM task_scope WHERE task_id = ?1")?
            .query_map([id], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        check_key(&key)
            .and_then(|()| check_objective(&objective))
            .and_then(|()| check_context(&context))
            .and_then(|()| paths.iter().try_for_each(|p| check_scope_path(p, scope)))
            .with_context(|| format!("task `{key}` is not executable"))?;
    }
    let cyclic: bool = tx.query_row(
        "WITH RECURSIVE reach (origin, id) AS (
           SELECT task_id, depends_on FROM task_dependencies WHERE plan_id = ?1 UNION
           SELECT r.origin, d.depends_on FROM reach r JOIN task_dependencies d ON d.task_id = r.id)
         SELECT EXISTS (SELECT 1 FROM reach WHERE origin = id)",
        [plan],
        |r| r.get(0),
    )?;
    ensure!(!cyclic, "the plan's dependencies form a cycle");
    Ok(())
}

fn set_scope(
    tx: &Transaction,
    task: TaskId,
    paths: &[String],
    scope: &dyn Fn(&str) -> Result<()>,
) -> Result<()> {
    ensure!(
        paths.len() <= SCOPE_LIMIT,
        "a task requests at most {SCOPE_LIMIT} paths"
    );
    let mut seen = HashSet::new();
    for path in paths {
        check_scope_path(path, scope)?;
        ensure!(seen.insert(path), "`{path}` is requested more than once");
        tx.execute(
            "INSERT INTO task_scope (task_id, path) VALUES (?1, ?2)",
            params![task, path],
        )?;
    }
    Ok(())
}

/// Makes `task`, which has no dependencies, depend on the tasks keyed
/// `depends_on`.
fn set_dependencies(
    tx: &Transaction,
    plan: PlanId,
    task: TaskId,
    key: &str,
    depends_on: &[String],
) -> Result<()> {
    let mut seen = HashSet::new();
    for dep in depends_on {
        ensure!(dep != key, "task `{key}` cannot depend on itself");
        ensure!(
            seen.insert(dep),
            "task `{key}` lists `{dep:.64}` more than once"
        );
        let id = lookup(tx, plan, dep)?;
        depend(tx, plan, task, id).with_context(|| format!("task `{key}` depending on `{dep}`"))?;
    }
    Ok(())
}

fn find(tx: &Transaction, plan: PlanId, key: &str) -> Result<Option<TaskId>> {
    tx.query_row(
        "SELECT id FROM tasks WHERE plan_id = ?1 AND key = ?2",
        params![plan, key],
        |r| r.get(0),
    )
    .optional()
    .map_err(Into::into)
}

pub(super) fn lookup(tx: &Transaction, plan: PlanId, key: &str) -> Result<TaskId> {
    find(tx, plan, key)?.with_context(|| format!("plan {plan} has no task `{key:.64}`"))
}

/// Accepts only task keys: `[a-z][a-z0-9_-]*`, at most 64 bytes.
fn check_key(key: &str) -> Result<()> {
    let valid = key.len() <= KEY_LIMIT
        && key.starts_with(|c: char| c.is_ascii_lowercase())
        && key
            .bytes()
            .all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'_' | b'-'));
    ensure!(valid, "`{key:.64}` is not a task key");
    Ok(())
}

fn check_objective(objective: &str) -> Result<()> {
    check_text("a task objective", objective, OBJECTIVE_LIMIT, true)
}

fn check_context(context: &str) -> Result<()> {
    check_text("a task context", context, CONTEXT_LIMIT, false)
}

/// Accepts a canonical literal path that `scope` accepts for the project.
fn check_scope_path(path: &str, scope: &dyn Fn(&str) -> Result<()>) -> Result<()> {
    ensure!(
        path.len() <= PATH_LIMIT,
        "requested paths are at most {PATH_LIMIT} bytes"
    );
    check_path(path)?;
    scope(path)
}
