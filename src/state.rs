//! Canonical durable project state (`.agentctl/state.db`).
//!
//! [`Store`] is the only writer. Each write is one `IMMEDIATE` transaction
//! that also appends the event describing it, so concurrent agentctl
//! processes serialize on the database rather than racing, and state and
//! history never diverge. The schema enforces structure; `Store` enforces
//! lifecycle transitions. Scheduling, verification and other orchestration
//! policy belong to the layers that will use this store.
//!
//! Accepted source is written only through `crate::source`, which publishes
//! the recovery object of any content before recording it here. CodeGraph
//! facts are written only through `crate::graph`, which validates them first.

mod graph;

use std::fmt;
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail, ensure};
use rusqlite::types::{FromSql, FromSqlError, FromSqlResult, ToSql, ToSqlOutput, ValueRef};
use rusqlite::{
    Connection, ErrorCode, OpenFlags, OptionalExtension, Transaction, TransactionBehavior, params,
};

/// Stamped into the SQLite header (`application_id`) so an agentctl store is
/// recognized by what it is, not merely by its schema version number.
const APPLICATION_ID: i32 = i32::from_be_bytes(*b"agct");
const SCHEMA_VERSION: i64 = 3;
const SCHEMA: &str = include_str!("state/schema.sql");
const MIGRATE_V1: &str = include_str!("state/migrate_v1.sql");
const MIGRATE_V2: &str = include_str!("state/migrate_v2.sql");
/// The version 1 `accepted_sources` definition, exactly as SQLite keeps it.
const V1_ACCEPTED_SOURCES: &str = "CREATE TABLE accepted_sources (
    path          TEXT    PRIMARY KEY,
    hash          TEXT    NOT NULL CHECK (hash <> ''),
    generation_id INTEGER REFERENCES generations (id)
) STRICT, WITHOUT ROWID";
/// How long a transaction waits for another process's writer to finish.
const BUSY_TIMEOUT: Duration = Duration::from_secs(10);

macro_rules! ids {
    ($($(#[$doc:meta])* $name:ident),+ $(,)?) => {$(
        $(#[$doc])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub struct $name(i64);

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(f)
            }
        }

        impl ToSql for $name {
            fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
                self.0.to_sql()
            }
        }

        impl FromSql for $name {
            fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
                i64::column_result(value).map(Self)
            }
        }
    )+};
}

ids!(
    PlanId,
    TaskId,
    GenerationId,
    AgentId,
    InvocationId,
    JournalId
);

macro_rules! text_enum {
    ($(#[$doc:meta])* $name:ident {
        $($(#[$variant_doc:meta])* $variant:ident = $text:literal),+ $(,)?
    }) => {
        $(#[$doc])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub enum $name {
            $($(#[$variant_doc])* $variant),+
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(match self {
                    $(Self::$variant => $text),+
                })
            }
        }

        impl ToSql for $name {
            fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
                Ok(self.to_string().into())
            }
        }

        impl FromSql for $name {
            fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
                match value.as_str()? {
                    $($text => Ok(Self::$variant),)+
                    other => Err(FromSqlError::Other(
                        format!("unknown {} `{other}`", stringify!($name)).into(),
                    )),
                }
            }
        }
    };
}

text_enum!(PlanState {
    Planning = "planning",
    Ready = "ready",
    Running = "running",
    Paused = "paused",
    NeedsAttention = "needs_attention",
    Completed = "completed",
});

impl PlanState {
    fn can_become(self, to: Self) -> bool {
        use PlanState::*;
        matches!(
            (self, to),
            (Planning, Ready | NeedsAttention)
                | (Ready, Running | Planning)
                | (Running, Paused | NeedsAttention | Planning | Completed)
                | (Paused, Running)
                | (NeedsAttention, Planning | Ready | Running)
        )
    }
}

text_enum!(
    /// Derived from the task's generations.
    TaskState {
        Pending = "pending",
        Running = "running",
        Completed = "completed",
    }
);

text_enum!(GenerationState {
    Active = "active",
    Accepted = "accepted",
    Rejected = "rejected",
    Failed = "failed",
});

text_enum!(Role {
    Planner = "planner",
    Executor = "executor",
    Verifier = "verifier",
});

text_enum!(JournalState {
    /// Intended, not known to have been attempted.
    Intended = "intended",
    /// Attempted, outcome unknown.
    Attempted = "attempted",
    Completed = "completed",
    Deviated = "deviated",
    Failed = "failed",
});

impl JournalState {
    fn can_become(self, to: Self) -> bool {
        use JournalState::*;
        match self {
            Intended => to != Intended,
            Attempted => matches!(to, Completed | Deviated | Failed),
            Completed | Deviated | Failed => false,
        }
    }
}

/// What a logical agent serves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentScope {
    Plan(PlanId),
    Generation(GenerationId),
}

/// How a generation ends without being accepted. Acceptance establishes
/// accepted source, so it goes through `source::accept_generation`.
#[derive(Debug, Clone, Copy)]
pub enum GenerationEnd {
    Rejected,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub id: PlanId,
    pub intent: String,
    pub state: PlanState,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Task {
    pub id: TaskId,
    pub plan: PlanId,
    pub description: String,
    pub state: TaskState,
    pub depends_on: Vec<TaskId>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Generation {
    pub id: GenerationId,
    pub number: i64,
    pub state: GenerationState,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invocation {
    pub id: InvocationId,
    pub provider: String,
    pub model: String,
    pub started_at: i64,
    pub ended_at: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JournalEntry {
    pub id: JournalId,
    pub intent: String,
    pub state: JournalState,
    pub outcome: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decision {
    pub concern: String,
    pub decision: String,
    pub decided_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    pub seq: i64,
    pub at: i64,
    pub kind: String,
    pub plan: Option<PlanId>,
    pub task: Option<TaskId>,
    pub agent: Option<AgentId>,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcceptedSource {
    /// Content hash of the accepted bytes; `None` when the path is accepted
    /// as not existing.
    pub hash: Option<String>,
    /// The generation whose acceptance produced this content; `None` for
    /// the baseline accepted state.
    pub generation: Option<GenerationId>,
}

#[derive(Debug)]
pub struct Store {
    conn: Connection,
}

impl Store {
    /// Opens the store at `path`, creating it if no file exists there. An
    /// existing file must already be an agentctl store.
    pub fn open(path: &Path) -> Result<Self> {
        let open = || -> Result<Self> {
            if !path.try_exists()? {
                create(path)?;
            }
            // Never create a file here: one that vanished since is an error.
            let flags = OpenFlags::default().difference(OpenFlags::SQLITE_OPEN_CREATE);
            let mut conn = Connection::open_with_flags(path, flags)?;
            conn.busy_timeout(BUSY_TIMEOUT)?;
            conn.pragma_update(None, "foreign_keys", true)?;
            // Identify the file before switching it to WAL, which rewrites
            // its header, so a refused file is left untouched.
            migrate(&mut conn)?;
            enable_wal(&conn)?;
            Ok(Self { conn })
        };
        open().with_context(|| format!("opening state {}", path.display()))
    }

    fn write<T>(&mut self, f: impl FnOnce(&Transaction) -> Result<T>) -> Result<T> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let value = f(&tx)?;
        tx.commit()?;
        Ok(value)
    }

    pub fn create_plan(&mut self, intent: &str) -> Result<PlanId> {
        self.write(|tx| {
            tx.execute(
                "INSERT INTO plans (intent, state, created_at, updated_at) VALUES (?1, ?2, ?3, ?3)",
                params![intent, PlanState::Planning, now()],
            )?;
            let plan = PlanId(tx.last_insert_rowid());
            event(tx, "plan.created", Some(plan), None, None, intent)?;
            Ok(plan)
        })
    }

    pub fn plan(&self, id: PlanId) -> Result<Plan> {
        self.conn
            .query_row(
                "SELECT intent, state, created_at, updated_at FROM plans WHERE id = ?1",
                [id],
                |r| {
                    Ok(Plan {
                        id,
                        intent: r.get(0)?,
                        state: r.get(1)?,
                        created_at: r.get(2)?,
                        updated_at: r.get(3)?,
                    })
                },
            )
            .optional()?
            .with_context(|| format!("plan {id} does not exist"))
    }

    /// A plan completes only once every task in it is completed.
    pub fn set_plan_state(&mut self, plan: PlanId, to: PlanState) -> Result<()> {
        self.write(|tx| {
            let from = plan_state(tx, plan)?;
            ensure!(
                from.can_become(to),
                "plan {plan} cannot go from {from} to {to}"
            );
            if to == PlanState::Completed {
                let open: i64 = tx.query_row(
                    "SELECT count(*) FROM tasks t WHERE t.plan_id = ?1 AND NOT EXISTS
                     (SELECT 1 FROM generations g WHERE g.task_id = t.id AND g.state = 'accepted')",
                    [plan],
                    |r| r.get(0),
                )?;
                ensure!(open == 0, "plan {plan} still has {open} uncompleted tasks");
            }
            tx.execute(
                "UPDATE plans SET state = ?2, updated_at = ?3 WHERE id = ?1",
                params![plan, to, now()],
            )?;
            event(
                tx,
                "plan.state",
                Some(plan),
                None,
                None,
                &format!("{from} -> {to}"),
            )
        })
    }

    /// Adds a task depending on existing tasks of the same plan.
    pub fn add_task(
        &mut self,
        plan: PlanId,
        description: &str,
        depends_on: &[TaskId],
    ) -> Result<TaskId> {
        self.write(|tx| {
            let state = plan_state(tx, plan)?;
            ensure!(state != PlanState::Completed, "plan {plan} is completed");
            tx.execute(
                "INSERT INTO tasks (plan_id, description, created_at) VALUES (?1, ?2, ?3)",
                params![plan, description, now()],
            )?;
            let task = TaskId(tx.last_insert_rowid());
            for &dep in depends_on {
                depend(tx, plan, task, dep)?;
            }
            event(
                tx,
                "task.created",
                Some(plan),
                Some(task),
                None,
                description,
            )?;
            Ok(task)
        })
    }

    /// Makes `task` depend on another task of its plan, whichever was created
    /// first, unless that would close a cycle.
    pub fn add_dependency(&mut self, task: TaskId, depends_on: TaskId) -> Result<()> {
        self.write(|tx| {
            let plan: PlanId = tx
                .query_row("SELECT plan_id FROM tasks WHERE id = ?1", [task], |r| {
                    r.get(0)
                })
                .optional()?
                .with_context(|| format!("task {task} does not exist"))?;
            let state = plan_state(tx, plan)?;
            ensure!(state != PlanState::Completed, "plan {plan} is completed");
            depend(tx, plan, task, depends_on)?;
            event(
                tx,
                "task.dependency",
                Some(plan),
                Some(task),
                None,
                &format!("depends on task {depends_on}"),
            )
        })
    }

    pub fn task(&self, id: TaskId) -> Result<Task> {
        let (plan, description, state) = self
            .conn
            .query_row(
                "SELECT plan_id, description, CASE
                   WHEN EXISTS (SELECT 1 FROM generations WHERE task_id = ?1 AND state = 'accepted')
                     THEN 'completed'
                   WHEN EXISTS (SELECT 1 FROM generations WHERE task_id = ?1 AND state = 'active')
                     THEN 'running'
                   ELSE 'pending' END
                 FROM tasks WHERE id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?
            .with_context(|| format!("task {id} does not exist"))?;
        let depends_on = self
            .conn
            .prepare("SELECT depends_on FROM task_dependencies WHERE task_id = ?1 ORDER BY 1")?
            .query_map([id], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        Ok(Task {
            id,
            plan,
            description,
            state,
            depends_on,
        })
    }

    /// Whether every task `task` depends on is completed.
    pub fn dependencies_satisfied(&self, task: TaskId) -> Result<bool> {
        Ok(self.conn.query_row(
            "SELECT NOT EXISTS (SELECT 1 FROM task_dependencies d WHERE d.task_id = ?1 AND NOT EXISTS
               (SELECT 1 FROM generations g WHERE g.task_id = d.depends_on AND g.state = 'accepted'))",
            [task],
            |r| r.get(0),
        )?)
    }

    /// Starts the next generation of a pending task.
    pub fn start_generation(&mut self, task: TaskId) -> Result<GenerationId> {
        self.write(|tx| {
            let plan: PlanId = tx
                .query_row("SELECT plan_id FROM tasks WHERE id = ?1", [task], |r| {
                    r.get(0)
                })
                .optional()?
                .with_context(|| format!("task {task} does not exist"))?;
            let live: Option<GenerationState> = tx
                .query_row(
                    "SELECT state FROM generations
                     WHERE task_id = ?1 AND state IN ('active', 'accepted')",
                    [task],
                    |r| r.get(0),
                )
                .optional()?;
            if let Some(state) = live {
                bail!("task {task} already has an {state} generation");
            }
            let number: i64 = tx.query_row(
                "SELECT coalesce(max(number), 0) + 1 FROM generations WHERE task_id = ?1",
                [task],
                |r| r.get(0),
            )?;
            tx.execute(
                "INSERT INTO generations (task_id, number, state, started_at) VALUES (?1, ?2, ?3, ?4)",
                params![task, number, GenerationState::Active, now()],
            )?;
            let generation = GenerationId(tx.last_insert_rowid());
            event(
                tx,
                "generation.started",
                Some(plan),
                Some(task),
                None,
                &format!("generation {number}"),
            )?;
            Ok(generation)
        })
    }

    /// Ends an active generation without accepting it. The generation keeps
    /// the paths it owns until they are explicitly released.
    pub fn finish_generation(
        &mut self,
        generation: GenerationId,
        end: GenerationEnd,
    ) -> Result<()> {
        let to = match end {
            GenerationEnd::Rejected => GenerationState::Rejected,
            GenerationEnd::Failed => GenerationState::Failed,
        };
        self.end_generation(generation, to, &[])
    }

    /// Accepts an active generation, atomically recording the source it
    /// establishes: `(path, Some(hash))` as accepted content, `(path, None)`
    /// as accepted absence. The generation keeps the paths it owns until they
    /// are explicitly released.
    ///
    /// Canonical state may name only content the recovery object store
    /// durably holds, which this store cannot check: the caller must have
    /// published and synced every object `sources` names.
    pub(crate) fn accept_generation(
        &mut self,
        generation: GenerationId,
        sources: &[(&str, Option<&str>)],
    ) -> Result<()> {
        self.end_generation(generation, GenerationState::Accepted, sources)
    }

    fn end_generation(
        &mut self,
        generation: GenerationId,
        to: GenerationState,
        sources: &[(&str, Option<&str>)],
    ) -> Result<()> {
        self.write(|tx| {
            let (plan, task, number) = active_generation(tx, generation)?;
            tx.execute(
                "UPDATE generations SET state = ?2, ended_at = ?3 WHERE id = ?1",
                params![generation, to, now()],
            )?;
            for &(path, hash) in sources {
                check_identity(path, hash)?;
                tx.execute(
                    "INSERT INTO accepted_sources (path, hash, generation_id) VALUES (?1, ?2, ?3)
                     ON CONFLICT (path) DO UPDATE
                     SET hash = excluded.hash, generation_id = excluded.generation_id",
                    params![path, hash, generation],
                )?;
            }
            event(
                tx,
                "generation.ended",
                Some(plan),
                Some(task),
                None,
                &format!("generation {number} {to}"),
            )
        })
    }

    pub fn generations(&self, task: TaskId) -> Result<Vec<Generation>> {
        self.conn
            .prepare(
                "SELECT id, number, state FROM generations WHERE task_id = ?1 ORDER BY number",
            )?
            .query_map([task], |r| {
                Ok(Generation {
                    id: r.get(0)?,
                    number: r.get(1)?,
                    state: r.get(2)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()
            .map_err(Into::into)
    }

    /// Claims canonical project-relative paths for an active generation:
    /// all of them, or none if any is already owned.
    pub fn claim_paths(&mut self, generation: GenerationId, paths: &[&str]) -> Result<()> {
        self.write(|tx| {
            let (plan, task, number) = active_generation(tx, generation)?;
            for path in paths {
                check_path(path)?;
                if let Some(owner) = owner(tx, path)? {
                    bail!("`{path}` is already owned by generation {owner}");
                }
                tx.execute(
                    "INSERT INTO ownership (path, generation_id) VALUES (?1, ?2)",
                    params![path, generation],
                )?;
            }
            event(
                tx,
                "ownership.claimed",
                Some(plan),
                Some(task),
                None,
                &format!("generation {number}: {}", paths.join(", ")),
            )
        })
    }

    /// Releases every path an ended generation owns. When that is safe is
    /// for the caller's acceptance or recovery lifecycle to decide.
    pub fn release_ownership(&mut self, generation: GenerationId) -> Result<()> {
        self.write(|tx| {
            let (plan, task, number, state) = generation_info(tx, generation)?;
            ensure!(
                state != GenerationState::Active,
                "generation {generation} is still active"
            );
            let paths = tx
                .prepare("DELETE FROM ownership WHERE generation_id = ?1 RETURNING path")?
                .query_map([generation], |r| r.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            event(
                tx,
                "ownership.released",
                Some(plan),
                Some(task),
                None,
                &format!("generation {number}: {}", paths.join(", ")),
            )
        })
    }

    pub fn owner(&self, path: &str) -> Result<Option<GenerationId>> {
        owner(&self.conn, path)
    }

    /// Records the baseline accepted identity of paths that have none yet:
    /// repository content (`Some(hash)`) or absence (`None`) accepted without
    /// any producing generation. As for [`Store::accept_generation`], the
    /// caller must have published and synced every object `sources` names.
    pub(crate) fn record_baseline(&mut self, sources: &[(&str, Option<&str>)]) -> Result<()> {
        self.write(|tx| {
            for &(path, hash) in sources {
                check_identity(path, hash)?;
                tx.execute(
                    "INSERT INTO accepted_sources (path, hash) VALUES (?1, ?2)",
                    params![path, hash],
                )
                .with_context(|| format!("recording the baseline of `{path}`"))?;
            }
            event(
                tx,
                "source.baseline",
                None,
                None,
                None,
                &format!("{} paths", sources.len()),
            )
        })
    }

    pub fn accepted_source(&self, path: &str) -> Result<Option<AcceptedSource>> {
        self.conn
            .query_row(
                "SELECT hash, generation_id FROM accepted_sources WHERE path = ?1",
                [path],
                |r| {
                    Ok(AcceptedSource {
                        hash: r.get(0)?,
                        generation: r.get(1)?,
                    })
                },
            )
            .optional()
            .map_err(Into::into)
    }

    /// Creates a logical agent. Planners serve a plan; executors and
    /// verifiers serve an active generation, which has one executor.
    pub fn create_agent(&mut self, role: Role, scope: AgentScope) -> Result<AgentId> {
        self.write(|tx| {
            let (plan, task, generation) = match scope {
                AgentScope::Plan(plan) => {
                    plan_state(tx, plan)?;
                    (plan, None, None)
                }
                AgentScope::Generation(generation) => {
                    let (plan, task, _) = active_generation(tx, generation)?;
                    (plan, Some(task), Some(generation))
                }
            };
            ensure!(
                (role == Role::Planner) == generation.is_none(),
                "a {role} cannot serve {scope:?}"
            );
            tx.execute(
                "INSERT INTO agents (role, plan_id, generation_id, created_at) VALUES (?1, ?2, ?3, ?4)",
                params![role, generation.is_none().then_some(plan), generation, now()],
            )
            .with_context(|| format!("creating {role} for {scope:?}"))?;
            let agent = AgentId(tx.last_insert_rowid());
            event(tx, "agent.created", Some(plan), task, Some(agent), &role.to_string())?;
            Ok(agent)
        })
    }

    /// Records a physical provider invocation embodying `agent`; an agent has
    /// at most one live invocation.
    pub fn start_invocation(
        &mut self,
        agent: AgentId,
        provider: &str,
        model: &str,
    ) -> Result<InvocationId> {
        self.write(|tx| {
            let (plan, task) = agent_subject(tx, agent)?;
            tx.execute(
                "INSERT INTO invocations (agent_id, provider, model, started_at) VALUES (?1, ?2, ?3, ?4)",
                params![agent, provider, model, now()],
            )
            .with_context(|| format!("starting an invocation of agent {agent}"))?;
            let invocation = InvocationId(tx.last_insert_rowid());
            let detail = format!("invocation {invocation}: {provider} {model}");
            event(tx, "invocation.started", Some(plan), task, Some(agent), &detail)?;
            Ok(invocation)
        })
    }

    pub fn end_invocation(&mut self, invocation: InvocationId) -> Result<()> {
        self.write(|tx| {
            let agent: AgentId = tx
                .query_row(
                    "UPDATE invocations SET ended_at = ?2 WHERE id = ?1 AND ended_at IS NULL
                     RETURNING agent_id",
                    params![invocation, now()],
                    |r| r.get(0),
                )
                .optional()?
                .with_context(|| format!("invocation {invocation} is not live"))?;
            let (plan, task) = agent_subject(tx, agent)?;
            let detail = format!("invocation {invocation}");
            event(
                tx,
                "invocation.ended",
                Some(plan),
                task,
                Some(agent),
                &detail,
            )
        })
    }

    pub fn invocations(&self, agent: AgentId) -> Result<Vec<Invocation>> {
        self.conn
            .prepare(
                "SELECT id, provider, model, started_at, ended_at FROM invocations
                 WHERE agent_id = ?1 ORDER BY id",
            )?
            .query_map([agent], |r| {
                Ok(Invocation {
                    id: r.get(0)?,
                    provider: r.get(1)?,
                    model: r.get(2)?,
                    started_at: r.get(3)?,
                    ended_at: r.get(4)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()
            .map_err(Into::into)
    }

    /// Journals what `agent` intends to do, before it acts.
    pub fn record_intent(&mut self, agent: AgentId, intent: &str) -> Result<JournalId> {
        self.write(|tx| {
            let (plan, task) = agent_subject(tx, agent)?;
            let at = now();
            tx.execute(
                "INSERT INTO journal (agent_id, intent, state, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?4)",
                params![agent, intent, JournalState::Intended, at],
            )?;
            let entry = JournalId(tx.last_insert_rowid());
            let detail = format!("entry {entry}: {intent}");
            event(
                tx,
                "journal.intended",
                Some(plan),
                task,
                Some(agent),
                &detail,
            )?;
            Ok(entry)
        })
    }

    /// Marks a journal entry attempted, or reconciles it with what actually
    /// happened. Deviations and failures require an `outcome`.
    pub fn update_journal(
        &mut self,
        entry: JournalId,
        to: JournalState,
        outcome: Option<&str>,
    ) -> Result<()> {
        self.write(|tx| {
            let (agent, from): (AgentId, JournalState) = tx
                .query_row(
                    "SELECT agent_id, state FROM journal WHERE id = ?1",
                    [entry],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?
                .with_context(|| format!("journal entry {entry} does not exist"))?;
            ensure!(
                from.can_become(to),
                "journal entry {entry} cannot go from {from} to {to}"
            );
            tx.execute(
                "UPDATE journal SET state = ?2, outcome = ?3, updated_at = ?4 WHERE id = ?1",
                params![entry, to, outcome, now()],
            )
            .with_context(|| format!("recording journal entry {entry} as {to}"))?;
            let (plan, task) = agent_subject(tx, agent)?;
            let detail = format!("entry {entry}: {from} -> {to}");
            event(
                tx,
                "journal.updated",
                Some(plan),
                task,
                Some(agent),
                &detail,
            )
        })
    }

    pub fn journal(&self, agent: AgentId) -> Result<Vec<JournalEntry>> {
        self.conn
            .prepare(
                "SELECT id, intent, state, outcome FROM journal WHERE agent_id = ?1 ORDER BY id",
            )?
            .query_map([agent], |r| {
                Ok(JournalEntry {
                    id: r.get(0)?,
                    intent: r.get(1)?,
                    state: r.get(2)?,
                    outcome: r.get(3)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()
            .map_err(Into::into)
    }

    /// Records an explicit, immutable human decision on a plan's concern.
    pub fn record_decision(&mut self, plan: PlanId, concern: &str, decision: &str) -> Result<()> {
        self.write(|tx| {
            tx.execute(
                "INSERT INTO decisions (plan_id, concern, decision, decided_at) VALUES (?1, ?2, ?3, ?4)",
                params![plan, concern, decision, now()],
            )
            .with_context(|| format!("recording a decision for plan {plan}"))?;
            event(tx, "decision.recorded", Some(plan), None, None, concern)
        })
    }

    pub fn decisions(&self, plan: PlanId) -> Result<Vec<Decision>> {
        self.conn
            .prepare(
                "SELECT concern, decision, decided_at FROM decisions WHERE plan_id = ?1 ORDER BY id",
            )?
            .query_map([plan], |r| {
                Ok(Decision {
                    concern: r.get(0)?,
                    decision: r.get(1)?,
                    decided_at: r.get(2)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()
            .map_err(Into::into)
    }

    /// Up to `limit` events after sequence number `after`, in order.
    pub fn events_after(&self, after: i64, limit: u32) -> Result<Vec<Event>> {
        self.conn
            .prepare(
                "SELECT seq, at, kind, plan_id, task_id, agent_id, detail FROM events
                 WHERE seq > ?1 ORDER BY seq LIMIT ?2",
            )?
            .query_map(params![after, limit], |r| {
                Ok(Event {
                    seq: r.get(0)?,
                    at: r.get(1)?,
                    kind: r.get(2)?,
                    plan: r.get(3)?,
                    task: r.get(4)?,
                    agent: r.get(5)?,
                    detail: r.get(6)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()
            .map_err(Into::into)
    }
}

/// Builds a complete store in a private file beside `path`, then links it
/// into place only if `path` is still absent. A file agentctl did not create
/// is never claimed, however empty, and racing creators cannot observe a
/// half-built store: all but one link loses and opens the winner's.
fn create(path: &Path) -> Result<()> {
    let dir = path.parent().context("state path has no directory")?;
    let temp = tempfile::Builder::new()
        .prefix(".state-")
        .tempfile_in(dir)?
        .into_temp_path();
    let mut conn = Connection::open(&temp)?;
    let tx = conn.transaction()?;
    tx.execute_batch(SCHEMA)?;
    tx.pragma_update(None, "application_id", APPLICATION_ID)?;
    tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    tx.commit()?;
    conn.close().map_err(|(_, e)| e)?;
    match std::fs::hard_link(&temp, path) {
        Err(e) if e.kind() != std::io::ErrorKind::AlreadyExists => Err(e.into()),
        _ => Ok(()),
    }
}

/// Brings an existing store's schema to `SCHEMA_VERSION`, refusing files it
/// does not own or understand. Each version migrates to the next in turn,
/// all in one transaction.
fn migrate(conn: &mut Connection) -> Result<()> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let (id, version): (i32, i64) = tx.query_row(
        "SELECT (SELECT application_id FROM pragma_application_id),
                (SELECT user_version FROM pragma_user_version)",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    ensure!(id == APPLICATION_ID, "not an agentctl state database");
    match version {
        1..=SCHEMA_VERSION => {}
        v if v > SCHEMA_VERSION => bail!(
            "state schema version {v} is newer than this agentctl supports \
             ({SCHEMA_VERSION}); upgrade agentctl"
        ),
        v => bail!("unknown state schema version {v}"),
    }
    let mismatch = || format!("state database does not match agentctl schema version {version}");
    if version == 1 {
        let sources: Option<String> = tx
            .query_row(
                "SELECT sql FROM sqlite_schema WHERE name = 'accepted_sources'",
                [],
                |r| r.get(0),
            )
            .optional()?;
        ensure!(sources.as_deref() == Some(V1_ACCEPTED_SOURCES), mismatch());
        tx.execute_batch(MIGRATE_V1)
            .context("migrating state schema version 1")?;
    }
    if version <= 2 {
        tx.execute_batch(MIGRATE_V2).with_context(mismatch)?;
    }
    let expected = Connection::open_in_memory()?;
    expected.execute_batch(SCHEMA)?;
    // A failed check rolls back any migration, leaving the file as found.
    ensure!(
        schema_objects(&tx)? == schema_objects(&expected)?,
        mismatch()
    );
    if version != SCHEMA_VERSION {
        tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    }
    tx.commit()?;
    Ok(())
}

/// Every schema object `conn` defines, with the SQL that defined it. SQLite
/// keeps that SQL as written, so comparing against `SCHEMA` built afresh
/// detects any missing, extra or altered table, index or trigger.
fn schema_objects(conn: &Connection) -> Result<Vec<(String, String, Option<String>)>> {
    let mut stmt = conn.prepare(
        "SELECT type, name, sql FROM sqlite_schema
         WHERE name NOT LIKE 'sqlite\\_%' ESCAPE '\\' ORDER BY type, name",
    )?;
    let objects = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
    Ok(objects.collect::<rusqlite::Result<_>>()?)
}

/// WAL lets readers proceed alongside the single writer, and persists in
/// the file. Switching to it takes an exclusive lock without consulting the
/// busy handler, so processes racing to create the store retry here.
fn enable_wal(conn: &Connection) -> Result<()> {
    let deadline = Instant::now() + BUSY_TIMEOUT;
    loop {
        match conn.pragma_update_and_check(None, "journal_mode", "wal", |r| r.get::<_, String>(0)) {
            Ok(mode) => {
                ensure!(mode == "wal", "cannot enable WAL (journal mode is {mode})");
                return Ok(());
            }
            Err(e)
                if e.sqlite_error_code() == Some(ErrorCode::DatabaseBusy)
                    && Instant::now() < deadline =>
            {
                thread::sleep(Duration::from_millis(10));
            }
            Err(e) => return Err(e.into()),
        }
    }
}

fn now() -> i64 {
    let since_epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock is after 1970");
    since_epoch.as_millis() as i64
}

fn event(
    tx: &Transaction,
    kind: &str,
    plan: Option<PlanId>,
    task: Option<TaskId>,
    agent: Option<AgentId>,
    detail: &str,
) -> Result<()> {
    tx.execute(
        "INSERT INTO events (at, kind, plan_id, task_id, agent_id, detail)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![now(), kind, plan, task, agent, detail],
    )?;
    Ok(())
}

/// Records that `task` depends on `dep`, refusing an edge from which `task`
/// is already reachable.
fn depend(tx: &Transaction, plan: PlanId, task: TaskId, dep: TaskId) -> Result<()> {
    let cycle: bool = tx.query_row(
        "WITH RECURSIVE reach (id) AS (
           SELECT ?2 UNION
           SELECT d.depends_on FROM task_dependencies d JOIN reach r ON d.task_id = r.id)
         SELECT EXISTS (SELECT 1 FROM reach WHERE id = ?1)",
        params![task, dep],
        |r| r.get(0),
    )?;
    ensure!(
        !cycle,
        "task {task} depending on task {dep} would form a cycle"
    );
    tx.execute(
        "INSERT INTO task_dependencies (plan_id, task_id, depends_on) VALUES (?1, ?2, ?3)",
        params![plan, task, dep],
    )
    .with_context(|| format!("depending on task {dep} within plan {plan}"))?;
    Ok(())
}

fn plan_state(tx: &Transaction, plan: PlanId) -> Result<PlanState> {
    tx.query_row("SELECT state FROM plans WHERE id = ?1", [plan], |r| {
        r.get(0)
    })
    .optional()?
    .with_context(|| format!("plan {plan} does not exist"))
}

/// The plan, task, number and state of a generation.
fn generation_info(
    tx: &Transaction,
    generation: GenerationId,
) -> Result<(PlanId, TaskId, i64, GenerationState)> {
    tx.query_row(
        "SELECT t.plan_id, g.task_id, g.number, g.state
         FROM generations g JOIN tasks t ON t.id = g.task_id WHERE g.id = ?1",
        [generation],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
    )
    .optional()?
    .with_context(|| format!("generation {generation} does not exist"))
}

/// The plan, task and number of an active generation.
fn active_generation(tx: &Transaction, generation: GenerationId) -> Result<(PlanId, TaskId, i64)> {
    let (plan, task, number, state) = generation_info(tx, generation)?;
    ensure!(
        state == GenerationState::Active,
        "generation {generation} has already ended ({state})"
    );
    Ok((plan, task, number))
}

/// The plan and, for generation-scoped agents, the task `agent` serves.
fn agent_subject(tx: &Transaction, agent: AgentId) -> Result<(PlanId, Option<TaskId>)> {
    tx.query_row(
        "SELECT coalesce(a.plan_id, t.plan_id), g.task_id FROM agents a
         LEFT JOIN generations g ON g.id = a.generation_id
         LEFT JOIN tasks t ON t.id = g.task_id WHERE a.id = ?1",
        [agent],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
    .optional()?
    .with_context(|| format!("agent {agent} does not exist"))
}

fn owner(conn: &Connection, path: &str) -> Result<Option<GenerationId>> {
    conn.query_row(
        "SELECT generation_id FROM ownership WHERE path = ?1",
        [path],
        |r| r.get(0),
    )
    .optional()
    .map_err(Into::into)
}

/// Accepts only canonical `/`-separated project-relative paths (`src/lib.rs`),
/// so each file has exactly one key. Paths are literal: any other character a
/// filename may hold, however special elsewhere (`[slug]`, `:`, `*`), is kept.
pub(crate) fn check_path(path: &str) -> Result<()> {
    let canonical =
        !path.contains('\0') && path.split('/').all(|part| !matches!(part, "" | "." | ".."));
    ensure!(
        canonical,
        "`{path}` is not a canonical project-relative path"
    );
    Ok(())
}

/// Accepts only lowercase hex SHA-256 content hashes, the names of recovery
/// objects.
pub(crate) fn check_hash(hash: &str) -> Result<()> {
    let valid = hash.len() == 64 && hash.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
    ensure!(valid, "`{hash}` is not a SHA-256 content hash");
    Ok(())
}

/// Checks an accepted source identity: a canonical path, and a content hash
/// unless the path is accepted as absent.
fn check_identity(path: &str, hash: Option<&str>) -> Result<()> {
    check_path(path)?;
    hash.map_or(Ok(()), check_hash)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use rusqlite::types::Value;
    use std::collections::HashSet;
    use std::sync::{Arc, Barrier};

    fn store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("state.db")).unwrap();
        (dir, store)
    }

    /// A structurally valid content hash.
    fn hash(n: u8) -> String {
        format!("{n:064x}")
    }

    fn err(result: Result<impl fmt::Debug>) -> String {
        format!("{:#}", result.unwrap_err())
    }

    fn version(path: &Path) -> i64 {
        Connection::open(path)
            .unwrap()
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap()
    }

    /// A plan with one task running its first generation.
    fn running_generation(store: &mut Store) -> (PlanId, TaskId, GenerationId) {
        let plan = store.create_plan("intent").unwrap();
        let task = store.add_task(plan, "task", &[]).unwrap();
        let generation = store.start_generation(task).unwrap();
        (plan, task, generation)
    }

    #[test]
    fn creates_schema_and_reopens_without_reset() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        let plan = Store::open(&path).unwrap().create_plan("ship it").unwrap();
        assert_eq!(version(&path), SCHEMA_VERSION);

        let store = Store::open(&path).unwrap();
        assert_eq!(store.plan(plan).unwrap().intent, "ship it");
        assert_eq!(store.events_after(0, 10).unwrap().len(), 1);
        let mode: String = store
            .conn
            .pragma_query_value(None, "journal_mode", |r| r.get(0))
            .unwrap();
        assert_eq!(mode, "wal");
    }

    #[test]
    fn refuses_newer_or_foreign_databases_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let newer = dir.path().join("newer.db");
        Store::open(&newer).unwrap();
        Connection::open(&newer)
            .unwrap()
            .pragma_update(None, "user_version", SCHEMA_VERSION + 1)
            .unwrap();
        let message = err(Store::open(&newer));
        let expected = format!("version {} is newer", SCHEMA_VERSION + 1);
        assert!(message.contains(&expected), "{message}");
        assert_eq!(version(&newer), SCHEMA_VERSION + 1);

        // Neither an unrelated schema nor one spoofing the current version
        // number is mistaken for an agentctl store, or modified.
        for (name, version) in [("foreign.db", 0), ("spoofed.db", SCHEMA_VERSION)] {
            let foreign = dir.path().join(name);
            let conn = Connection::open(&foreign).unwrap();
            conn.execute_batch("CREATE TABLE notes (body TEXT)")
                .unwrap();
            conn.pragma_update(None, "user_version", version).unwrap();
            drop(conn);
            let before = std::fs::read(&foreign).unwrap();
            let message = err(Store::open(&foreign));
            assert!(
                message.contains("not an agentctl state database"),
                "{message}"
            );
            assert_eq!(std::fs::read(&foreign).unwrap(), before, "{name}");
        }
    }

    #[test]
    fn refuses_agentctl_headers_without_agentctl_schema_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let cases = [
            ("headers_only.db", ""),
            ("partial.db", "DROP TABLE accepted_sources"),
            ("altered.db", "DROP TRIGGER events_no_update"),
            ("extra.db", "CREATE TABLE notes (body TEXT)"),
        ];
        for (name, change) in cases {
            let path = dir.path().join(name);
            let conn = Connection::open(&path).unwrap();
            if !change.is_empty() {
                conn.execute_batch(SCHEMA).unwrap();
                conn.execute_batch(change).unwrap();
            }
            conn.pragma_update(None, "application_id", APPLICATION_ID)
                .unwrap();
            conn.pragma_update(None, "user_version", SCHEMA_VERSION)
                .unwrap();
            drop(conn);

            let before = std::fs::read(&path).unwrap();
            let message = err(Store::open(&path));
            assert!(
                message.contains("does not match agentctl schema"),
                "{name}: {message}"
            );
            assert_eq!(std::fs::read(&path).unwrap(), before, "{name}");
            for suffix in ["-wal", "-shm", "-journal"] {
                let side = dir.path().join(format!("{name}{suffix}"));
                assert!(!side.exists(), "{name}{suffix}");
            }
        }
    }

    /// Rewrites the store at `path` as schema version 2, which lacks only
    /// the CodeGraph tables.
    pub(crate) fn downgrade_to_v2(path: &Path) {
        let conn = Connection::open(path).unwrap();
        conn.execute_batch(
            "DROP TABLE graph_sites; DROP TABLE graph_relations;
             DROP TABLE graph_entities; DROP TABLE graph_sources;",
        )
        .unwrap();
        conn.pragma_update(None, "user_version", 2).unwrap();
    }

    /// Rewrites the store at `path` as schema version 1, whose only
    /// difference from version 2 is the `accepted_sources` table, holding
    /// `accepted`.
    pub(crate) fn downgrade_to_v1(path: &Path, accepted: &[(&str, &str, Option<GenerationId>)]) {
        downgrade_to_v2(path);
        let conn = Connection::open(path).unwrap();
        conn.execute_batch(&format!(
            "DROP TABLE accepted_sources; {V1_ACCEPTED_SOURCES};"
        ))
        .unwrap();
        for (path, hash, generation) in accepted {
            conn.execute(
                "INSERT INTO accepted_sources (path, hash, generation_id) VALUES (?1, ?2, ?3)",
                params![path, hash, generation],
            )
            .unwrap();
        }
        conn.pragma_update(None, "user_version", 1).unwrap();
    }

    /// Every row of every table except `excluded`, by table.
    fn rows_besides(path: &Path, excluded: &str) -> Vec<(String, Vec<Vec<Value>>)> {
        let conn = Connection::open(path).unwrap();
        let tables: Vec<String> = conn
            .prepare(
                "SELECT name FROM sqlite_schema
                 WHERE type = 'table' AND name <> ?1 ORDER BY name",
            )
            .unwrap()
            .query_map([excluded], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        tables
            .into_iter()
            .map(|table| {
                let mut stmt = conn.prepare(&format!("SELECT * FROM {table}")).unwrap();
                let columns = stmt.column_count();
                let rows = stmt
                    .query_map([], |r| (0..columns).map(|i| r.get(i)).collect())
                    .unwrap()
                    .collect::<rusqlite::Result<_>>()
                    .unwrap();
                (table, rows)
            })
            .collect()
    }

    #[test]
    fn migrating_version_1_keeps_state_but_not_unrecoverable_sources() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("v1.db");
        let mut store = Store::open(&path).unwrap();
        let (plan, first, accepted) = running_generation(&mut store);
        let agent = store
            .create_agent(Role::Planner, AgentScope::Plan(plan))
            .unwrap();
        let invocation = store.start_invocation(agent, "codex", "gpt").unwrap();
        store.end_invocation(invocation).unwrap();
        let intent = store.record_intent(agent, "write a.rs").unwrap();
        store
            .update_journal(intent, JournalState::Attempted, None)
            .unwrap();
        store.record_decision(plan, "concern", "decision").unwrap();
        store.claim_paths(accepted, &["src/a.rs"]).unwrap();
        store
            .accept_generation(accepted, &[("src/a.rs", Some(&hash(1)))])
            .unwrap();
        let second = store.add_task(plan, "next", &[first]).unwrap();
        let active = store.start_generation(second).unwrap();
        store.claim_paths(active, &["src/b.rs"]).unwrap();
        drop(store);
        let before = rows_besides(&path, "accepted_sources");
        for (table, rows) in &before {
            assert!(
                !rows.is_empty() || table.starts_with("graph_"),
                "{table} is exercised"
            );
        }

        // Well-formed or not, no version 1 hash names a known recovery object.
        downgrade_to_v1(
            &path,
            &[
                ("src/a.rs", &hash(0), Some(accepted)),
                ("src/b.rs", &hash(2), None),
                ("src/c.rs", "h0", None),
            ],
        );
        let mut store = Store::open(&path).unwrap();
        assert_eq!(version(&path), SCHEMA_VERSION);
        for source in ["src/a.rs", "src/b.rs", "src/c.rs"] {
            assert_eq!(store.accepted_source(source).unwrap(), None, "{source}");
        }
        let count: i64 = store
            .conn
            .query_row("SELECT count(*) FROM accepted_sources", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0);
        assert_eq!(rows_besides(&path, "accepted_sources"), before);

        // The migrated store accepts source state established afresh.
        store
            .record_baseline(&[("src/a.rs", Some(&hash(3))), ("src/c.rs", None)])
            .unwrap();
        drop(store);
        let store = Store::open(&path).unwrap();
        assert_eq!(
            store.accepted_source("src/a.rs").unwrap(),
            Some(AcceptedSource {
                hash: Some(hash(3)),
                generation: None
            })
        );
    }

    #[test]
    fn refuses_version_1_files_without_agentctl_schema_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("altered.db");
        Store::open(&path).unwrap();
        downgrade_to_v1(&path, &[("src/a.rs", &hash(0), None)]);
        Connection::open(&path)
            .unwrap()
            .execute_batch("DROP TRIGGER events_no_update")
            .unwrap();
        let message = err(Store::open(&path));
        assert!(message.contains("schema version 1"), "{message}");
        assert_eq!(version(&path), 1);
        let conn = Connection::open(&path).unwrap();
        let stored: String = conn
            .query_row("SELECT hash FROM accepted_sources", [], |r| r.get(0))
            .unwrap();
        assert_eq!(stored, hash(0));
        assert!(
            conn.execute(
                "INSERT INTO accepted_sources (path, hash) VALUES ('x', NULL)",
                []
            )
            .is_err(),
            "the version 1 table is left in place"
        );
    }

    /// A store at `path` holding state of every kind the version 2 schema
    /// has, including accepted content and accepted absence.
    fn populated_v2(path: &Path) {
        let mut store = Store::open(path).unwrap();
        let (plan, first, accepted) = running_generation(&mut store);
        let agent = store
            .create_agent(Role::Planner, AgentScope::Plan(plan))
            .unwrap();
        let invocation = store.start_invocation(agent, "codex", "gpt").unwrap();
        store.end_invocation(invocation).unwrap();
        store.record_intent(agent, "write a.rs").unwrap();
        store.record_decision(plan, "concern", "decision").unwrap();
        store.claim_paths(accepted, &["src/a.rs"]).unwrap();
        store
            .record_baseline(&[("src/base.rs", Some(&hash(4))), ("src/none.rs", None)])
            .unwrap();
        store
            .accept_generation(
                accepted,
                &[("src/a.rs", Some(&hash(1))), ("src/gone.rs", None)],
            )
            .unwrap();
        let second = store.add_task(plan, "next", &[first]).unwrap();
        let active = store.start_generation(second).unwrap();
        store.claim_paths(active, &["src/b.rs"]).unwrap();
        drop(store);
        downgrade_to_v2(path);
    }

    #[test]
    fn migrating_version_2_keeps_all_state_and_indexes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("v2.db");
        populated_v2(&path);
        let before = rows_besides(&path, "");
        for (table, rows) in &before {
            assert!(!rows.is_empty(), "{table} is exercised");
        }

        let store = Store::open(&path).unwrap();
        assert_eq!(version(&path), SCHEMA_VERSION);
        drop(store);
        let after = rows_besides(&path, "");
        let (graph, rest): (Vec<_>, Vec<_>) = after
            .into_iter()
            .partition(|(table, _)| table.starts_with("graph_"));
        assert_eq!(rest, before, "every version 2 row is kept exactly");
        assert_eq!(graph.len(), 4);
        for (table, rows) in graph {
            assert!(rows.is_empty(), "{table} starts empty");
        }

        // Nothing is promoted into graph facts: sources are merely unindexed.
        let store = Store::open(&path).unwrap();
        use crate::graph::Freshness;
        assert_eq!(
            store.graph_status("src/a.rs").unwrap(),
            Freshness::Unindexed
        );
        assert_eq!(
            store.graph_status("src/base.rs").unwrap(),
            Freshness::Unindexed
        );
        assert_eq!(
            store.graph_status("src/gone.rs").unwrap(),
            Freshness::Absent
        );
        assert_eq!(
            store.graph_status("src/none.rs").unwrap(),
            Freshness::Absent
        );
        assert!(store.graph_status("src/b.rs").is_err(), "untracked");
    }

    #[test]
    fn refuses_version_2_files_without_agentctl_schema_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let cases = [
            ("altered.db", "DROP TRIGGER events_no_update"),
            ("extra.db", "CREATE TABLE notes (body TEXT)"),
            // Claims version 2 yet already holds a (forged) graph table.
            ("forged.db", "CREATE TABLE graph_sources (path TEXT)"),
        ];
        for (name, change) in cases {
            let path = dir.path().join(name);
            populated_v2(&path);
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(change).unwrap();
            drop(conn);
            let before = std::fs::read(&path).unwrap();
            let message = err(Store::open(&path));
            assert!(
                message.contains("does not match agentctl schema version 2"),
                "{name}: {message}"
            );
            assert_eq!(std::fs::read(&path).unwrap(), before, "{name}");
            assert_eq!(version(&path), 2, "{name}");
        }
    }

    #[test]
    fn refuses_preexisting_empty_databases_untouched() {
        let dir = tempfile::tempdir().unwrap();
        // A valid SQLite database with a header but no identity or schema.
        let empty = dir.path().join("empty.db");
        let conn = Connection::open(&empty).unwrap();
        conn.execute_batch("VACUUM").unwrap();
        let objects: i64 = conn
            .query_row("SELECT count(*) FROM sqlite_schema", [], |r| r.get(0))
            .unwrap();
        assert_eq!(objects, 0);
        drop(conn);
        // A zero-length file is also a valid, empty SQLite database.
        let blank = dir.path().join("blank.db");
        std::fs::write(&blank, "").unwrap();

        for path in [empty, blank] {
            let before = std::fs::read(&path).unwrap();
            let message = err(Store::open(&path));
            assert!(
                message.contains("not an agentctl state database"),
                "{message}"
            );
            assert_eq!(std::fs::read(&path).unwrap(), before, "{path:?}");
        }
    }

    #[test]
    fn plan_lifecycle_rejects_invalid_transitions() {
        use PlanState::*;
        let (_dir, mut store) = store();
        let plan = store.create_plan("intent").unwrap();

        for to in [Running, Paused, Completed, Planning] {
            assert!(err(store.set_plan_state(plan, to)).contains("cannot go from planning"));
        }
        for to in [
            NeedsAttention,
            Ready,
            Running,
            Paused,
            Running,
            Planning,
            Ready,
            Running,
        ] {
            store.set_plan_state(plan, to).unwrap();
        }

        let task = store.add_task(plan, "task", &[]).unwrap();
        let message = err(store.set_plan_state(plan, Completed));
        assert!(message.contains("1 uncompleted tasks"), "{message}");
        let generation = store.start_generation(task).unwrap();
        store.accept_generation(generation, &[]).unwrap();
        store.set_plan_state(plan, Completed).unwrap();
        assert_eq!(store.plan(plan).unwrap().state, Completed);

        for to in [Planning, Ready, Running, Paused, NeedsAttention, Completed] {
            assert!(store.set_plan_state(plan, to).is_err());
        }
        assert!(err(store.add_task(plan, "late", &[])).contains("is completed"));
    }

    #[test]
    fn references_are_enforced() {
        let (_dir, mut store) = store();
        let a = store.create_plan("a").unwrap();
        let b = store.create_plan("b").unwrap();
        let in_b = store.add_task(b, "b1", &[]).unwrap();

        assert!(store.add_task(PlanId(99), "orphan", &[]).is_err());
        assert!(store.add_task(a, "cross-plan", &[in_b]).is_err());
        assert!(store.add_task(b, "dangling", &[TaskId(99)]).is_err());
        assert!(store.record_decision(PlanId(99), "c", "d").is_err());
        assert!(store.start_generation(TaskId(99)).is_err());
        assert!(
            store
                .create_agent(Role::Planner, AgentScope::Plan(PlanId(99)))
                .is_err()
        );
        assert!(store.start_invocation(AgentId(99), "p", "m").is_err());

        // The database itself refuses dangling references and self-cycles.
        let raw = |sql: &str| store.conn.execute(sql, []).unwrap_err().to_string();
        assert!(
            raw("INSERT INTO tasks (plan_id, description, created_at) VALUES (99, 'x', 0)")
                .contains("FOREIGN KEY")
        );
        let cycle = format!("INSERT INTO task_dependencies VALUES ({b}, {in_b}, {in_b})");
        assert!(raw(&cycle).contains("CHECK"));
        // Rejected writes left no trace.
        assert_eq!(store.task(in_b).unwrap().depends_on, vec![]);
        assert_eq!(store.events_after(0, 100).unwrap().len(), 3);
    }

    #[test]
    fn tasks_dependencies_and_generations_are_durable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        let mut store = Store::open(&path).unwrap();
        let plan = store.create_plan("intent").unwrap();
        let a = store.add_task(plan, "a", &[]).unwrap();
        let b = store.add_task(plan, "b", &[a]).unwrap();
        let c = store.add_task(plan, "c", &[a, b]).unwrap();
        assert!(store.dependencies_satisfied(a).unwrap());
        assert!(!store.dependencies_satisfied(b).unwrap());

        let first = store.start_generation(a).unwrap();
        assert_eq!(store.task(a).unwrap().state, TaskState::Running);
        assert!(err(store.start_generation(a)).contains("already has an active generation"));
        store
            .finish_generation(first, GenerationEnd::Failed)
            .unwrap();
        assert!(err(store.finish_generation(first, GenerationEnd::Rejected)).contains("ended"));
        assert_eq!(store.task(a).unwrap().state, TaskState::Pending);

        let second = store.start_generation(a).unwrap();
        assert_ne!(first, second);
        store.accept_generation(second, &[]).unwrap();
        assert!(err(store.start_generation(a)).contains("already has an accepted generation"));
        drop(store);

        let store = Store::open(&path).unwrap();
        let task = store.task(c).unwrap();
        assert_eq!((task.plan, task.depends_on), (plan, vec![a, b]));
        assert_eq!(store.task(a).unwrap().state, TaskState::Completed);
        assert!(store.dependencies_satisfied(b).unwrap());
        assert!(!store.dependencies_satisfied(c).unwrap());
        let history: Vec<_> = store
            .generations(a)
            .unwrap()
            .into_iter()
            .map(|g| (g.id, g.number, g.state))
            .collect();
        assert_eq!(
            history,
            [
                (first, 1, GenerationState::Failed),
                (second, 2, GenerationState::Accepted)
            ]
        );
    }

    #[test]
    fn dependencies_form_any_acyclic_graph() {
        let (_dir, mut store) = store();
        let plan = store.create_plan("intent").unwrap();
        let [a, b, c, d] = ["a", "b", "c", "d"].map(|t| store.add_task(plan, t, &[]).unwrap());

        // Edges run against creation order as freely as with it: a diamond
        // where the earliest task depends on later ones.
        store.add_dependency(a, b).unwrap();
        store.add_dependency(a, c).unwrap();
        store.add_dependency(b, d).unwrap();
        store.add_dependency(c, d).unwrap();
        let e = store.add_task(plan, "e", &[a, d]).unwrap();
        assert_eq!(store.task(a).unwrap().depends_on, vec![b, c]);
        assert_eq!(store.task(e).unwrap().depends_on, vec![a, d]);
        assert!(!store.dependencies_satisfied(a).unwrap());
        assert!(store.dependencies_satisfied(d).unwrap());

        let events = store.events_after(0, 100).unwrap();
        for (task, dep) in [(d, d), (b, a), (d, a), (d, e)] {
            let message = err(store.add_dependency(task, dep));
            assert!(message.contains("would form a cycle"), "{message}");
        }
        assert!(store.add_dependency(a, b).is_err(), "duplicate edge");
        assert_eq!(store.task(d).unwrap().depends_on, vec![]);
        assert_eq!(store.events_after(0, 100).unwrap(), events);
    }

    #[test]
    fn logical_agents_are_distinct_from_invocations() {
        let (_dir, mut store) = store();
        let (plan, _, generation) = running_generation(&mut store);

        store
            .create_agent(Role::Planner, AgentScope::Plan(plan))
            .unwrap();
        assert!(
            store
                .create_agent(Role::Planner, AgentScope::Generation(generation))
                .is_err()
        );
        assert!(
            store
                .create_agent(Role::Executor, AgentScope::Plan(plan))
                .is_err()
        );
        let executor = store
            .create_agent(Role::Executor, AgentScope::Generation(generation))
            .unwrap();
        assert!(
            store
                .create_agent(Role::Executor, AgentScope::Generation(generation))
                .is_err()
        );
        store
            .create_agent(Role::Verifier, AgentScope::Generation(generation))
            .unwrap();

        let first = store.start_invocation(executor, "claude", "m1").unwrap();
        let message = err(store.start_invocation(executor, "claude", "m1"));
        assert!(message.contains("UNIQUE"), "{message}");
        store.end_invocation(first).unwrap();
        assert!(store.end_invocation(first).is_err());
        let second = store.start_invocation(executor, "codex", "m2").unwrap();

        let attempts = store.invocations(executor).unwrap();
        assert_eq!(
            attempts.iter().map(|i| i.id).collect::<Vec<_>>(),
            [first, second]
        );
        assert!(attempts[0].ended_at.is_some() && attempts[1].ended_at.is_none());
        assert_eq!(attempts[1].provider, "codex");

        store
            .finish_generation(generation, GenerationEnd::Rejected)
            .unwrap();
        assert!(
            err(store.create_agent(Role::Verifier, AgentScope::Generation(generation)))
                .contains("already ended")
        );
    }

    #[test]
    fn journal_distinguishes_intend_act_reconcile_states() {
        use JournalState::*;
        let (_dir, mut store) = store();
        let plan = store.create_plan("intent").unwrap();
        let agent = store
            .create_agent(Role::Planner, AgentScope::Plan(plan))
            .unwrap();

        let done = store.record_intent(agent, "write a.rs").unwrap();
        store.update_journal(done, Attempted, None).unwrap();
        assert!(store.update_journal(done, Intended, None).is_err());
        store.update_journal(done, Completed, None).unwrap();
        assert!(store.update_journal(done, Failed, Some("late")).is_err());

        let deviated = store.record_intent(agent, "edit b.rs").unwrap();
        store.update_journal(deviated, Attempted, None).unwrap();
        assert!(err(store.update_journal(deviated, Deviated, None)).contains("CHECK"));
        store
            .update_journal(deviated, Deviated, Some("edited c.rs too"))
            .unwrap();

        // Reconciliation may find an intent was never attempted.
        let failed = store.record_intent(agent, "run tests").unwrap();
        store
            .update_journal(failed, Failed, Some("never started"))
            .unwrap();

        let open = store.record_intent(agent, "push").unwrap();
        let unknown = store.record_intent(agent, "migrate").unwrap();
        store.update_journal(unknown, Attempted, None).unwrap();
        assert!(store.update_journal(open, Intended, None).is_err());
        assert!(store.update_journal(open, Attempted, Some("note")).is_err());

        let states: Vec<_> = store
            .journal(agent)
            .unwrap()
            .into_iter()
            .map(|e| (e.id, e.state, e.outcome))
            .collect();
        assert_eq!(
            states,
            [
                (done, Completed, None),
                (deviated, Deviated, Some("edited c.rs too".into())),
                (failed, Failed, Some("never started".into())),
                (open, Intended, None),
                (unknown, Attempted, None),
            ]
        );
    }

    #[test]
    fn decisions_persist_immutably() {
        let (_dir, mut store) = store();
        let plan = store.create_plan("intent").unwrap();
        store
            .record_decision(plan, "tests are flaky on CI", "quarantine them")
            .unwrap();
        let decisions = store.decisions(plan).unwrap();
        assert_eq!(
            (
                decisions[0].concern.as_str(),
                decisions[0].decision.as_str()
            ),
            ("tests are flaky on CI", "quarantine them")
        );
        assert!(
            store
                .conn
                .execute("UPDATE decisions SET decision = 'x'", [])
                .is_err()
        );
        assert!(store.conn.execute("DELETE FROM decisions", []).is_err());
        assert_eq!(store.decisions(plan).unwrap(), decisions);
    }

    #[test]
    fn events_are_ordered_associated_and_append_only() {
        let (_dir, mut store) = store();
        let (plan, task, generation) = running_generation(&mut store);
        let agent = store
            .create_agent(Role::Executor, AgentScope::Generation(generation))
            .unwrap();
        store.record_intent(agent, "work").unwrap();

        let events = store.events_after(0, 100).unwrap();
        let kinds: Vec<_> = events.iter().map(|e| e.kind.as_str()).collect();
        assert_eq!(
            kinds,
            [
                "plan.created",
                "task.created",
                "generation.started",
                "agent.created",
                "journal.intended"
            ]
        );
        assert!(events.windows(2).all(|w| w[0].seq < w[1].seq));
        let last = events.last().unwrap();
        assert_eq!(
            (last.plan, last.task, last.agent),
            (Some(plan), Some(task), Some(agent))
        );
        assert_eq!(store.events_after(events[2].seq, 100).unwrap(), events[3..]);
        assert_eq!(store.events_after(0, 2).unwrap(), events[..2]);

        assert!(
            store
                .conn
                .execute("UPDATE events SET detail = 'x'", [])
                .is_err()
        );
        assert!(store.conn.execute("DELETE FROM events", []).is_err());
        assert_eq!(store.events_after(0, 100).unwrap(), events);
    }

    #[test]
    fn ownership_and_accepted_sources() {
        let (_dir, mut store) = store();
        let (plan, _, first) = running_generation(&mut store);
        let other_task = store.add_task(plan, "other", &[]).unwrap();
        let second = store.start_generation(other_task).unwrap();

        store.claim_paths(first, &["src/a.rs", "src/b.rs"]).unwrap();
        assert_eq!(store.owner("src/a.rs").unwrap(), Some(first));
        let message = err(store.claim_paths(second, &["src/c.rs", "src/b.rs"]));
        assert!(message.contains("already owned by generation"), "{message}");
        assert_eq!(
            store.owner("src/c.rs").unwrap(),
            None,
            "claims are all-or-nothing"
        );
        for bad in ["", "/abs", "a//b", "./a", "a/../b", "a/", "a\0b"] {
            assert!(store.claim_paths(second, &[bad]).is_err(), "{bad:?}");
        }

        let message = err(store.release_ownership(first));
        assert!(message.contains("still active"), "{message}");
        let (h1, h2, h3) = (hash(1), hash(2), hash(3));
        store
            .accept_generation(first, &[("src/a.rs", Some(&h1)), ("src/b.rs", Some(&h2))])
            .unwrap();
        assert_eq!(
            store.owner("src/a.rs").unwrap(),
            Some(first),
            "acceptance alone does not release ownership"
        );
        store.release_ownership(first).unwrap();
        assert_eq!(store.owner("src/a.rs").unwrap(), None);
        store.claim_paths(second, &["src/b.rs"]).unwrap();
        store
            .accept_generation(second, &[("src/a.rs", Some(&h3)), ("src/b.rs", None)])
            .unwrap();
        assert_eq!(
            store.accepted_source("src/a.rs").unwrap(),
            Some(AcceptedSource {
                hash: Some(h3),
                generation: Some(second)
            })
        );
        assert_eq!(
            store.accepted_source("src/b.rs").unwrap(),
            Some(AcceptedSource {
                hash: None,
                generation: Some(second)
            }),
            "an accepted deletion is recorded as accepted absence"
        );
        assert_eq!(store.owner("src/b.rs").unwrap(), Some(second));
    }

    #[test]
    fn failed_and_rejected_generations_keep_ownership() {
        let (_dir, mut store) = store();
        let (plan, task, failed) = running_generation(&mut store);
        let other = store.add_task(plan, "other", &[]).unwrap();
        let rejected = store.start_generation(other).unwrap();
        store.claim_paths(failed, &["src/a.rs"]).unwrap();
        store.claim_paths(rejected, &["src/b.rs"]).unwrap();

        store
            .finish_generation(failed, GenerationEnd::Failed)
            .unwrap();
        store
            .finish_generation(rejected, GenerationEnd::Rejected)
            .unwrap();
        assert_eq!(store.owner("src/a.rs").unwrap(), Some(failed));
        assert_eq!(store.owner("src/b.rs").unwrap(), Some(rejected));

        // Not even the task's own next generation may take them implicitly.
        let retry = store.start_generation(task).unwrap();
        assert!(err(store.claim_paths(retry, &["src/a.rs"])).contains("already owned"));
        store.release_ownership(failed).unwrap();
        store.claim_paths(retry, &["src/a.rs"]).unwrap();
        assert_eq!(store.owner("src/b.rs").unwrap(), Some(rejected));
    }

    #[test]
    fn baseline_sources_need_no_generation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        let mut store = Store::open(&path).unwrap();
        let (h0, h1, h2) = (hash(0), hash(1), hash(2));
        store
            .record_baseline(&[
                ("src/a.rs", Some(&h0)),
                ("src/b.rs", Some(&h1)),
                ("src/new.rs", None),
            ])
            .unwrap();
        assert!(store.record_baseline(&[("src/a.rs", Some(&h2))]).is_err());
        assert!(store.record_baseline(&[("src/new.rs", None)]).is_err());
        assert!(store.record_baseline(&[("../x", Some(&h2))]).is_err());

        let (_, _, generation) = running_generation(&mut store);
        store
            .accept_generation(generation, &[("src/a.rs", Some(&h2))])
            .unwrap();
        drop(store);

        let store = Store::open(&path).unwrap();
        let source = |p| store.accepted_source(p).unwrap().unwrap();
        assert_eq!(
            source("src/a.rs"),
            AcceptedSource {
                hash: Some(h2),
                generation: Some(generation)
            }
        );
        assert_eq!(
            source("src/b.rs"),
            AcceptedSource {
                hash: Some(h1),
                generation: None
            }
        );
        assert_eq!(
            source("src/new.rs"),
            AcceptedSource {
                hash: None,
                generation: None
            }
        );
        assert_eq!(store.accepted_source("src/c.rs").unwrap(), None);
    }

    #[test]
    fn accepted_content_must_be_a_sha256_hash() {
        let (_dir, mut store) = store();
        let (_, task, generation) = running_generation(&mut store);
        let valid = hash(0xab);
        let bad = [
            "",
            "not-a-sha256-object",
            &valid[1..],
            &format!("{valid}0"),
            &valid.to_uppercase(),
            &format!("{}g", &valid[1..]),
            &format!("{}é", &valid[1..]),
        ];
        for bad in bad {
            let message = err(store.record_baseline(&[("src/a.rs", Some(bad))]));
            assert!(message.contains("not a SHA-256 content hash"), "{message}");
            let message = err(store.accept_generation(generation, &[("src/a.rs", Some(bad))]));
            assert!(message.contains("not a SHA-256 content hash"), "{message}");
            let insert = "INSERT INTO accepted_sources (path, hash) VALUES ('src/a.rs', ?1)";
            assert!(
                store.conn.execute(insert, [bad]).is_err(),
                "the schema refuses {bad:?} too"
            );
        }
        assert_eq!(store.accepted_source("src/a.rs").unwrap(), None);
        assert_eq!(store.task(task).unwrap().state, TaskState::Running);
    }

    #[test]
    fn failed_operation_rolls_back_entirely() {
        let (_dir, mut store) = store();
        let (_, task, generation) = running_generation(&mut store);
        store.claim_paths(generation, &["src/a.rs"]).unwrap();
        let events = store.events_after(0, 100).unwrap();

        // The invalid second path fails after the state update and first
        // source write have already executed.
        let h1 = hash(1);
        let sources = [("src/a.rs", Some(h1.as_str())), ("../escape", Some(&h1))];
        assert!(store.accept_generation(generation, &sources).is_err());

        assert_eq!(store.task(task).unwrap().state, TaskState::Running);
        assert_eq!(store.owner("src/a.rs").unwrap(), Some(generation));
        assert_eq!(store.accepted_source("src/a.rs").unwrap(), None);
        assert_eq!(store.events_after(0, 100).unwrap(), events);
    }

    #[test]
    fn concurrent_processes_coordinate_through_the_store() {
        const WRITERS: usize = 8;
        let dir = tempfile::tempdir().unwrap();
        let path = Arc::new(dir.path().join("state.db"));

        // Independent connections race to create the schema and write.
        let barrier = Arc::new(Barrier::new(WRITERS));
        let handles: Vec<_> = (0..WRITERS)
            .map(|i| {
                let (path, barrier) = (path.clone(), barrier.clone());
                thread::spawn(move || {
                    barrier.wait();
                    let mut store = Store::open(&path).unwrap();
                    for j in 0..10 {
                        let plan = store.create_plan(&format!("plan {i}.{j}")).unwrap();
                        store.add_task(plan, "task", &[]).unwrap();
                    }
                })
            })
            .collect();
        handles.into_iter().for_each(|h| h.join().unwrap());
        // The losing creators' private files are gone.
        for entry in std::fs::read_dir(dir.path()).unwrap() {
            let name = entry.unwrap().file_name();
            assert!(name.to_str().unwrap().starts_with("state.db"), "{name:?}");
        }

        let mut store = Store::open(&path).unwrap();
        let events = store.events_after(0, 1000).unwrap();
        assert_eq!(events.len(), WRITERS * 10 * 2);
        assert!(events.windows(2).all(|w| w[0].seq < w[1].seq));
        let plans: HashSet<_> = events.iter().map(|e| e.plan.unwrap().0).collect();
        assert_eq!(plans.len(), WRITERS * 10);

        // Exactly one of many racing processes may start a task's generation.
        let (_, task, generation) = running_generation(&mut store);
        store
            .finish_generation(generation, GenerationEnd::Failed)
            .unwrap();
        let barrier = Arc::new(Barrier::new(WRITERS));
        let started = (0..WRITERS)
            .map(|_| {
                let (path, barrier) = (path.clone(), barrier.clone());
                thread::spawn(move || {
                    let mut store = Store::open(&path).unwrap();
                    barrier.wait();
                    store.start_generation(task)
                })
            })
            .collect::<Vec<_>>()
            .into_iter()
            .map(|h| h.join().unwrap())
            .filter(|r| match r {
                Ok(_) => true,
                Err(e) => {
                    assert!(format!("{e:#}").contains("already has an active"), "{e:#}");
                    false
                }
            })
            .count();
        assert_eq!(started, 1);
        assert_eq!(store.generations(task).unwrap().len(), 2);
    }
}

/// Only `crate::source`, which publishes recovery objects first, can record
/// accepted source. Other crates can neither record it directly:
///
/// ```compile_fail
/// # fn f(store: &mut agentctl::state::Store) {
/// store.record_baseline(&[("src/a.rs", Some("not-a-sha256-object"))]);
/// # }
/// ```
///
/// ```compile_fail
/// # fn f(store: &mut agentctl::state::Store, g: agentctl::state::GenerationId) {
/// store.accept_generation(g, &[("src/a.rs", Some("not-a-sha256-object"))]);
/// # }
/// ```
///
/// nor accept a generation through its state-only lifecycle:
///
/// ```compile_fail
/// # use agentctl::state::{GenerationEnd, GenerationId, Store};
/// # fn f(store: &mut Store, g: GenerationId) {
/// store.finish_generation(g, GenerationEnd::Accepted(&[("src/a.rs", None)]));
/// # }
/// ```
///
/// which remains usable for other ends:
///
/// ```no_run
/// # use agentctl::state::{GenerationEnd, GenerationId, Store};
/// # fn f(store: &mut Store, g: GenerationId) {
/// store.finish_generation(g, GenerationEnd::Rejected).unwrap();
/// # }
/// ```
#[cfg(doctest)]
pub struct AcceptedSourceIsRestricted;
