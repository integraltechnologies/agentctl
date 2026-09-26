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
//! A plan's tasks are written only through `crate::planner`, which validates
//! what planners propose against the project. Mutation ownership is
//! acquired only within the scope planning authorized (see [`Acquisition`]).
//! Executor attempts, the changes they are found to have made in their
//! workspaces and installing those into the working tree are recorded only
//! through `crate::executor`, which observes both itself (see
//! [`Execution`]). Verifications of installed candidates are recorded only
//! through `crate::verifier`, and a verified candidate is accepted only
//! through `crate::acceptance` (see [`Acceptance`]).

mod acceptance;
mod execution;
mod graph;
mod ownership;
mod planning;
mod replanning;
mod scheduling;
mod verification;

pub(crate) use acceptance::Publication;
pub use acceptance::{Acceptance, AcceptedChange};
pub use execution::{Capture, Change, ChangeKind, Content, Execution, ExecutionStatus, Install};
pub(crate) use execution::{ExecutorResult, Observed};
pub use ownership::{Acquisition, Conflict, Owner};
pub(crate) use replanning::Restoration;
pub use replanning::{Basis, Replan, ReplanRecord, RetryAuthorization, Revision, Standing};
pub use scheduling::{
    Capacity, Claim, ClaimRecord, Condition, DagDefect, Release, Snapshot, TaskStatus,
};
pub use verification::{
    Blocker, Check, CheckOutcome, Note, Verdict, Verification, VerificationResult,
    VerificationStatus, VerifierReport,
};
pub(crate) use verification::{VerifierObserved, VerifierResult, ViewBasis};

use std::fmt;
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail, ensure};
use rusqlite::types::{FromSql, FromSqlError, FromSqlResult, ToSql, ToSqlOutput, Type, ValueRef};
use rusqlite::{
    Connection, ErrorCode, OpenFlags, OptionalExtension, Row, Transaction, TransactionBehavior,
    params,
};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Stamped into the SQLite header (`application_id`) so an agentctl store is
/// recognized by what it is, not merely by its schema version number.
const APPLICATION_ID: i32 = i32::from_be_bytes(*b"agct");
/// The one canonical schema. Until agentctl is first dogfooded there is no
/// persistence compatibility boundary: a schema change replaces `SCHEMA`
/// outright, and a store of any other schema is refused, never upgraded.
const SCHEMA_VERSION: i64 = 1;
const SCHEMA: &str = include_str!("state/schema.sql");
/// How long a transaction waits for another process's writer to finish.
const BUSY_TIMEOUT: Duration = Duration::from_secs(10);
/// Bounds on what the journal records, so that it holds structure rather
/// than opaque blobs or prose.
const IDENTIFIER_LIMIT: usize = 64;
const PARAMETERS_LIMIT: usize = 8 * 1024;
const PARAMETER_TEXT_LIMIT: usize = 1024;
const PARAMETER_DEPTH: usize = 8;
const EVIDENCE_LIMIT: usize = 32;
const EVIDENCE_PATH_LIMIT: usize = 4096;
/// Bounds on a plan's human intent.
const OBJECTIVE_LIMIT: usize = 4096;
const STATEMENT_LIMIT: usize = 1024;
const STATEMENTS_LIMIT: usize = 32;

macro_rules! ids {
    ($($(#[$doc:meta])* $name:ident),+ $(,)?) => {$(
        $(#[$doc])*
        #[derive(
            Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
        )]
        #[serde(transparent)]
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

        /// Parses an id as agentctl prints it; whether anything has that id
        /// is for `Store` to say.
        impl std::str::FromStr for $name {
            type Err = std::num::ParseIntError;

            fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
                s.parse().map(Self)
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
    JournalId,
    ExecutionId,
    VerificationId,
    ReplanId
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

text_enum!(
    /// What reconciliation established that an attempted action did.
    ActionOutcome {
        CompletedAsIntended = "completed_as_intended",
        CompletedWithDeviation = "completed_with_deviation",
        Failed = "failed",
    }
);

text_enum!(
    /// Where a provider invocation is in its lifecycle, as agentctl knows it.
    InvocationState {
        /// Recorded before launch; a process may or may not exist.
        Starting = "starting",
        /// Launched, with no end recorded. After agentctl disappears this
        /// says nothing about whether the process is still alive.
        Running = "running",
        Succeeded = "succeeded",
        Failed = "failed",
        /// agentctl terminated the process and observed it end.
        Cancelled = "cancelled",
        /// agentctl lost authoritative knowledge of how it ended.
        Interrupted = "interrupted",
    }
);

impl InvocationState {
    pub fn is_terminal(self) -> bool {
        !matches!(self, Self::Starting | Self::Running)
    }
}

impl FailureKind {
    /// Whether the failure happened before any provider process existed.
    pub fn before_launch(self) -> bool {
        matches!(self, Self::ExecutableMissing | Self::SpawnFailed)
    }
}

impl InvocationEnd {
    /// Why an invocation in state `from` cannot end like this, if it cannot.
    ///
    /// A `starting` invocation has launched no process that agentctl knows
    /// of, so it can only fail to launch, or be interrupted when agentctl
    /// loses track of whether it launched. Only a `running` one can succeed,
    /// fail once launched, or be cancelled; success needs a zero exit code.
    fn refusal(&self, from: InvocationState) -> Option<String> {
        use InvocationState::*;
        let launched = match from {
            Starting => false,
            Running => true,
            ended => return Some(format!("it already ended as {ended}")),
        };
        match (self.state, self.failure) {
            (Starting | Running, _) => Some(format!("an invocation cannot end as {}", self.state)),
            (Succeeded | Cancelled, _) if !launched => {
                Some(format!("a {from} invocation cannot end as {}", self.state))
            }
            (Succeeded, _) if self.exit_code != Some(0) => {
                Some("a succeeded invocation must have exited with code 0".into())
            }
            (Failed, Some(kind)) if kind.before_launch() == launched => {
                Some(format!("a {from} invocation cannot fail with {kind}"))
            }
            (Failed, _) if !launched && self.exit_code.is_some() => {
                Some("an invocation that never launched has no exit code".into())
            }
            _ => None,
        }
    }
}

text_enum!(
    /// What capturing an execution's repository changes established. Only a
    /// candidate may become accepted work, and only once verified.
    ExecutionOutcome {
        /// Structurally valid: the invocation succeeded, its well-formed
        /// result reported success, and every change observed in the
        /// executor's workspace, staged against the observed repository
        /// baseline, was authorized. Nothing more: agentctl observes the
        /// workspace, not who writes to it, so this is no proof that the
        /// executor process wrote every byte.
        Candidate = "candidate",
        /// The executor reported that it failed.
        ReportedFailed = "reported_failed",
        /// The invocation succeeded, but its result broke the executor
        /// protocol.
        MalformedResult = "malformed_result",
        /// The invocation failed, was cancelled or was interrupted.
        InvocationFailed = "invocation_failed",
        /// The executor's workspace was mutated beyond the execution's
        /// authority.
        ScopeViolated = "scope_violated",
        /// The workspace changed in a way that cannot safely be attributed
        /// to the executor (see [`Attribution`]).
        Unattributable = "unattributable",
    }
);

text_enum!(
    /// Why observed changes could not be attributed to an executor.
    Attribution {
        /// The workspace kept changing after the invocation ended.
        Unsettled = "unsettled",
        /// It changed although no executor process was ever launched.
        NeverLaunched = "never_launched",
        /// Only for executors working in the project's working tree, which none
        /// does now: a path another generation owns changed.
        Contested = "contested",
        /// Only for executors working in the project's working tree, which none
        /// does now: it changed while another journaled action was in flight too.
        Concurrent = "concurrent",
    }
);

text_enum!(
    /// How installing a candidate into the project's working tree ended.
    InstallOutcome {
        /// Every change was written: the working tree holds the candidate,
        /// provisionally, until verified.
        Installed = "installed",
        /// The working tree no longer held what the baseline observed at
        /// some changed path, so nothing was written.
        Drifted = "drifted",
        /// The candidate held a change agentctl does not install, or the
        /// workspace no longer held what was captured: nothing was written.
        Refused = "refused",
        /// Writing failed, and every path written was restored.
        Failed = "failed",
    }
);

text_enum!(
    /// How one independent verification of an installed candidate ended.
    /// Only `Passed` and `Failed` are judgments, and neither is acceptance:
    /// a verification never accepts source, refreshes CodeGraph, releases
    /// ownership, ends the generation or completes the task. It is evidence
    /// that acceptance may weigh, about the exact candidate verified.
    VerificationOutcome {
        /// The verifier reported no blocking defect in this exact candidate,
        /// with at least one check that passed, while the working tree held
        /// the candidate throughout and the verifier left repository source
        /// in its workspace untouched, as agentctl observed. Nothing more.
        Passed = "passed",
        /// The verifier reported the candidate blocked, with every blocker
        /// it found, under the same observations as a pass.
        Failed = "failed",
        /// Before any verifier ran, the working tree no longer held the
        /// installed candidate at some changed path: nothing was verified.
        CandidateDrifted = "candidate_drifted",
        /// The working tree stopped holding the candidate while it was
        /// verified: whatever the verifier reported is about other bytes.
        CandidateChanged = "candidate_changed",
        /// The verifier changed repository source in its workspace, which a
        /// verifier never does, so whatever it reported cannot pass.
        BoundaryViolated = "boundary_violated",
        /// The verifier's invocation failed, was cancelled or was
        /// interrupted: no judgment, and nothing about the candidate.
        InvocationFailed = "invocation_failed",
        /// The invocation succeeded with a result breaking the verifier
        /// protocol: no judgment either.
        MalformedResult = "malformed_result",
    }
);

text_enum!(
    /// How far accepting a generation's verified candidate got. Each phase
    /// holds only once every earlier one does.
    AcceptancePhase {
        /// Bound to its candidate and verification, with nothing published:
        /// agentctl records an acceptance only together with publishing its
        /// source, so this is never left behind by an interruption.
        Intended = "intended",
        /// Every changed path of the candidate is accepted source, exactly
        /// as captured: the candidate's content, or accepted absence. Their
        /// graphs are stale until synchronized; ownership is still held and
        /// the generation still active.
        Published = "published",
        /// CodeGraph holds no graph of a changed path derived from anything
        /// but its published identity: each is current, has none because no
        /// frontend derives one, or, when absent, is gone.
        Synchronized = "synchronized",
        /// The generation is accepted, completing its task, and owns
        /// nothing any more.
        Completed = "completed",
    }
);

text_enum!(
    /// How a scheduled pipeline ended, as canonical state establishes it
    /// once its claim may be released. Only `Accepted` completes the task;
    /// every other outcome leaves the task for planner action, and its
    /// generation as the pipeline left it.
    PipelineOutcome {
        /// Its acceptance completed: the task is completed.
        Accepted = "accepted",
        /// No execution was ever attempted: no executor ran.
        NotExecuted = "not_executed",
        /// The execution's capture is not a candidate.
        ExecutionFailed = "execution_failed",
        /// The candidate was not installed into the working tree.
        InstallFailed = "install_failed",
        /// The latest verification of the candidate failed it.
        VerificationFailed = "verification_failed",
        /// The candidate was never judged: no verification finished, or
        /// the latest one reconciled without a judgment.
        VerificationInconclusive = "verification_inconclusive",
        /// The latest verification passed, and no acceptance was recorded.
        AcceptanceDeclined = "acceptance_declined",
    }
);

text_enum!(
    /// What an executor reported of its own work: a claim, never proof.
    Reported {
        Succeeded = "succeeded",
        Failed = "failed",
    }
);

text_enum!(
    /// Why an invocation failed.
    FailureKind {
        ExecutableMissing = "executable_missing",
        SpawnFailed = "spawn_failed",
        /// The task input could not be delivered to the provider.
        InputFailed = "input_failed",
        /// The provider reported an error in its structured output.
        ProviderError = "provider_error",
        /// The provider's structured output was unparseable or inconsistent.
        MalformedOutput = "malformed_output",
        /// The provider exited successfully without a result.
        NoResult = "no_result",
        /// The provider exited unsuccessfully without reporting why.
        ExitStatus = "exit_status",
    }
);

/// Token counts. `input` counts every input token processed, cached or not;
/// the optional counts are subsets a provider reports separately.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TokenUsage {
    pub input: u64,
    pub output: u64,
    pub cached_input: Option<u64>,
    pub cache_write: Option<u64>,
    /// Output spent on reasoning, included in `output`.
    pub reasoning: Option<u64>,
}

/// Token usage together with its provenance. Counts are never fabricated:
/// what no provider reported and agentctl did not estimate is unavailable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Usage {
    ProviderReported(TokenUsage),
    LocalEstimate(TokenUsage),
    Unavailable,
}

/// How an invocation ended, as recorded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvocationEnd {
    /// A terminal state.
    pub state: InvocationState,
    /// Present exactly when the invocation failed.
    pub failure: Option<FailureKind>,
    /// Concise evidence; required for failed and interrupted invocations.
    pub diagnostic: Option<String>,
    pub exit_code: Option<i32>,
    /// The provider's own session identifier, as noncanonical metadata.
    pub provider_session: Option<String>,
    pub usage: Usage,
}

/// What a logical agent serves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentScope {
    Plan(PlanId),
    Generation(GenerationId),
}

/// How a generation ends without being accepted. Acceptance establishes
/// accepted source, so it goes through `crate::acceptance` for a generation
/// that executed, or `source::accept_generation` for one that never did.
#[derive(Debug, Clone, Copy)]
pub enum GenerationEnd {
    Rejected,
    Failed,
}

/// What a human wants of a plan: the objective, the constraints and
/// invariants the work must respect, and the criteria by which it is
/// complete, each as the human stated it. It is fixed when the plan is
/// created: planning decides how to meet it, never what it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HumanIntent {
    pub objective: String,
    pub constraints: Vec<String>,
    pub completion_criteria: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub id: PlanId,
    pub intent: HumanIntent,
    pub state: PlanState,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Task {
    pub id: TaskId,
    pub plan: PlanId,
    /// The planner-chosen name identifying it within its plan.
    pub key: String,
    pub objective: String,
    /// What a worker is told beyond the objective.
    pub context: String,
    /// The literal project paths it requests to mutate, in order.
    pub scope: Vec<String>,
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
    pub agent: AgentId,
    pub provider: String,
    pub model: String,
    pub effort: Option<String>,
    pub state: InvocationState,
    pub started_at: i64,
    pub ended_at: Option<i64>,
    /// How it ended, once recorded.
    pub end: Option<InvocationEnd>,
}

/// An engineering-control action a logical agent intends: an
/// agentctl-defined `action` kind such as `source.write`, with parameters
/// identifying what it acts on. What an action means belongs to the layer
/// that defines it.
///
/// An intent is agentctl's structured description, never provider prose.
/// The kind and every parameter name are identifiers (`[a-z][a-z0-9_.]*`),
/// and parameter values are bounded JSON whose strings are literal data (a
/// path names a path, never a pattern) of one line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Intent {
    pub action: String,
    pub parameters: Map<String, Value>,
}

/// A structured fact supporting a reconciliation: a reference to canonical
/// state or an agentctl-defined fact, never prose. Evidence says what was
/// established, not whether the work is acceptable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Evidence {
    /// How an ended invocation ended, as recorded.
    Invocation { invocation: InvocationId },
    /// The content a canonical project path was observed to hold: its
    /// content hash, or `None` when it did not exist.
    Content { path: String, hash: Option<String> },
    /// An agentctl-defined fact, named by an identifier.
    Fact { name: String },
}

/// Everything established about one journaled action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JournalEntry {
    pub id: JournalId,
    pub agent: AgentId,
    /// Exactly what was intended when the attempt began, or, before then,
    /// as last revised.
    pub intent: Intent,
    pub intended_at: i64,
    pub status: ActionStatus,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActionStatus {
    /// Intended, and never attempted.
    NotAttempted,
    /// Attempted: it may have been acted on, and its outcome is unknown.
    /// Nothing, including how the invocation ended, stands in for
    /// reconciliation.
    OutcomeUnknown(Attempt),
    Reconciled(Attempt, Reconciliation),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attempt {
    /// The invocation of the entry's agent that performed or requested it.
    pub invocation: Option<InvocationId>,
    pub at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reconciliation {
    pub outcome: ActionOutcome,
    pub evidence: Vec<Evidence>,
    pub at: i64,
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
            // Identify the file before switching it to WAL, which rewrites
            // its header, so a refused file is left untouched.
            identify(&mut conn)?;
            ensure!(
                conn.pragma_query_value(None, "foreign_keys", |r| r.get::<_, bool>(0))?,
                "foreign key enforcement is off"
            );
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

    /// Creates a plan in planning, establishing its human intent before
    /// anything can plan it.
    pub fn create_plan(&mut self, intent: &HumanIntent) -> Result<PlanId> {
        let (constraints, criteria) = intent.check()?;
        self.write(|tx| {
            tx.execute(
                "INSERT INTO plans
                   (objective, constraints, completion_criteria, state, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?5)",
                params![
                    intent.objective,
                    constraints,
                    criteria,
                    PlanState::Planning,
                    now()
                ],
            )?;
            let plan = PlanId(tx.last_insert_rowid());
            event(
                tx,
                "plan.created",
                Some(plan),
                None,
                None,
                &intent.objective,
            )?;
            Ok(plan)
        })
    }

    pub fn plan(&self, id: PlanId) -> Result<Plan> {
        self.conn
            .query_row(
                "SELECT objective, constraints, completion_criteria, state, created_at, updated_at
                 FROM plans WHERE id = ?1",
                [id],
                |r| {
                    Ok(Plan {
                        id,
                        intent: HumanIntent {
                            objective: r.get(0)?,
                            constraints: json_column(r, 1)?,
                            completion_criteria: json_column(r, 2)?,
                        },
                        state: r.get(3)?,
                        created_at: r.get(4)?,
                        updated_at: r.get(5)?,
                    })
                },
            )
            .optional()?
            .with_context(|| format!("plan {id} does not exist"))
    }

    /// A plan becomes ready only by finalizing its planning (see
    /// `crate::planner`), and completes only once every task in it is
    /// completed.
    pub fn set_plan_state(&mut self, plan: PlanId, to: PlanState) -> Result<()> {
        self.write(|tx| {
            let from = plan_state(tx, plan)?;
            ensure!(
                from.can_become(to),
                "plan {plan} cannot go from {from} to {to}"
            );
            ensure!(
                to != PlanState::Ready,
                "plan {plan} becomes ready only by finalizing its planning"
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

    pub fn task(&self, id: TaskId) -> Result<Task> {
        let (plan, key, objective, context, state) = self
            .conn
            .query_row(
                "SELECT plan_id, key, objective, context, CASE
                   WHEN EXISTS (SELECT 1 FROM generations WHERE task_id = ?1 AND state = 'accepted')
                     THEN 'completed'
                   WHEN EXISTS (SELECT 1 FROM generations WHERE task_id = ?1 AND state = 'active')
                     THEN 'running'
                   ELSE 'pending' END
                 FROM tasks WHERE id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .optional()?
            .with_context(|| format!("task {id} does not exist"))?;
        let scope = self
            .conn
            .prepare("SELECT path FROM task_scope WHERE task_id = ?1 ORDER BY 1")?
            .query_map([id], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        let depends_on = self
            .conn
            .prepare("SELECT depends_on FROM task_dependencies WHERE task_id = ?1 ORDER BY 1")?
            .query_map([id], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        Ok(Task {
            id,
            plan,
            key,
            objective,
            context,
            scope,
            state,
            depends_on,
        })
    }

    /// Every task of a plan, in the order they were added.
    pub fn tasks(&self, plan: PlanId) -> Result<Vec<Task>> {
        self.plan(plan)?;
        let ids: Vec<TaskId> = self
            .conn
            .prepare("SELECT id FROM tasks WHERE plan_id = ?1 ORDER BY id")?
            .query_map([plan], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        ids.into_iter().map(|id| self.task(id)).collect()
    }

    /// Whether every task `task` depends on is completed: by a generation
    /// whose acceptance completed, and by nothing short of that.
    pub fn dependencies_satisfied(&self, task: TaskId) -> Result<bool> {
        Ok(unsatisfied_dependencies(&self.conn, task)?.is_empty())
    }

    /// Starts the next generation of a pending task.
    pub fn start_generation(&mut self, task: TaskId) -> Result<GenerationId> {
        self.write(|tx| insert_generation(tx, task).map(|(_, generation, _)| generation))
    }

    /// Ends an active generation that was never scheduled without
    /// accepting it. The generation keeps the paths it owns until they are
    /// explicitly released. A scheduled generation ends short of acceptance
    /// only as the replan abandoning it ends it (see `crate::planner`).
    pub fn finish_generation(
        &mut self,
        generation: GenerationId,
        end: GenerationEnd,
    ) -> Result<()> {
        let to = match end {
            GenerationEnd::Rejected => GenerationState::Rejected,
            GenerationEnd::Failed => GenerationState::Failed,
        };
        self.write(|tx| {
            ensure!(
                !scheduling::scheduled(tx, generation)?,
                "generation {generation} was scheduled: only a replan abandoning it ends it \
                 short of acceptance"
            );
            end_generation(tx, generation, to, &[])
        })
    }

    /// Accepts an active generation, atomically recording the source it
    /// establishes: `(path, Some(hash))` as accepted content, `(path, None)`
    /// as accepted absence. The generation keeps the paths it owns until they
    /// are explicitly released. Only for a generation that never executed: an
    /// executor's candidate is accepted only through `crate::acceptance`,
    /// once verified.
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
        self.write(|tx| end_generation(tx, generation, to, sources))
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

    /// Every tracked path with accepted content, in order.
    pub fn accepted_paths(&self) -> Result<Vec<String>> {
        self.conn
            .prepare("SELECT path FROM accepted_sources WHERE hash IS NOT NULL ORDER BY path")?
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()
            .map_err(Into::into)
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
        self.write(|tx| insert_agent(tx, role, scope))
    }

    /// Records a physical provider invocation embodying `agent`, before its
    /// process is launched; an agent has at most one live invocation.
    pub fn start_invocation(
        &mut self,
        agent: AgentId,
        provider: &str,
        model: &str,
        effort: Option<&str>,
    ) -> Result<InvocationId> {
        self.write(|tx| {
            let (plan, task) = agent_subject(tx, agent)?;
            tx.execute(
                "INSERT INTO invocations (agent_id, provider, model, effort, state, started_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    agent,
                    provider,
                    model,
                    effort,
                    InvocationState::Starting,
                    now()
                ],
            )
            .with_context(|| format!("starting an invocation of agent {agent}"))?;
            let invocation = InvocationId(tx.last_insert_rowid());
            let detail = format!("invocation {invocation}: {provider} {model}");
            event(
                tx,
                "invocation.started",
                Some(plan),
                task,
                Some(agent),
                &detail,
            )?;
            Ok(invocation)
        })
    }

    /// Records that a starting invocation's process has launched.
    pub fn invocation_running(&mut self, invocation: InvocationId) -> Result<()> {
        self.write(|tx| {
            let agent: AgentId = tx
                .query_row(
                    "UPDATE invocations SET state = ?2 WHERE id = ?1 AND state = ?3
                     RETURNING agent_id",
                    params![
                        invocation,
                        InvocationState::Running,
                        InvocationState::Starting
                    ],
                    |r| r.get(0),
                )
                .optional()?
                .with_context(|| format!("invocation {invocation} is not starting"))?;
            let (plan, task) = agent_subject(tx, agent)?;
            let detail = format!("invocation {invocation}");
            event(
                tx,
                "invocation.running",
                Some(plan),
                task,
                Some(agent),
                &detail,
            )
        })
    }

    /// Records how a live invocation ended. Ends are final, and must follow
    /// from where the invocation is: see [`InvocationEnd`]'s rules.
    pub fn finish_invocation(
        &mut self,
        invocation: InvocationId,
        end: &InvocationEnd,
    ) -> Result<()> {
        ensure!(
            end.state.is_terminal(),
            "an invocation cannot end as {}",
            end.state
        );
        let (source, tokens) = match end.usage {
            Usage::ProviderReported(t) => ("provider_reported", Some(t)),
            Usage::LocalEstimate(t) => ("local_estimate", Some(t)),
            Usage::Unavailable => ("unavailable", None),
        };
        let count = |n: u64| i64::try_from(n).context("token count out of range");
        let optional = |n: Option<u64>| n.map(count).transpose();
        let (input, output, cached, written, reasoning) = match tokens {
            Some(t) => (
                Some(count(t.input)?),
                Some(count(t.output)?),
                optional(t.cached_input)?,
                optional(t.cache_write)?,
                optional(t.reasoning)?,
            ),
            None => (None, None, None, None, None),
        };
        self.write(|tx| {
            let from: InvocationState = tx
                .query_row(
                    "SELECT state FROM invocations WHERE id = ?1",
                    [invocation],
                    |r| r.get(0),
                )
                .optional()?
                .with_context(|| format!("invocation {invocation} does not exist"))?;
            if let Some(why) = end.refusal(from) {
                bail!("invocation {invocation} cannot end as {}: {why}", end.state);
            }
            let agent: AgentId = tx
                .query_row(
                    "UPDATE invocations SET state = ?2, failure = ?3, diagnostic = ?4,
                       exit_code = ?5, provider_session = ?6, usage = ?7, input_tokens = ?8,
                       output_tokens = ?9, cached_input_tokens = ?10, cache_write_tokens = ?11,
                       reasoning_tokens = ?12, ended_at = ?13
                     WHERE id = ?1 AND state = ?14
                     RETURNING agent_id",
                    params![
                        invocation,
                        end.state,
                        end.failure,
                        end.diagnostic,
                        end.exit_code,
                        end.provider_session,
                        source,
                        input,
                        output,
                        cached,
                        written,
                        reasoning,
                        now(),
                        from
                    ],
                    |r| r.get(0),
                )
                .optional()
                .with_context(|| format!("recording invocation {invocation} as {}", end.state))?
                .with_context(|| format!("invocation {invocation} is not live"))?;
            let (plan, task) = agent_subject(tx, agent)?;
            let detail = match end.failure {
                Some(kind) => format!("invocation {invocation}: {} ({kind})", end.state),
                None => format!("invocation {invocation}: {}", end.state),
            };
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

    pub fn invocation(&self, id: InvocationId) -> Result<Invocation> {
        self.conn
            .query_row(
                &format!("{INVOCATION_COLUMNS} WHERE id = ?1"),
                [id],
                invocation_row,
            )
            .optional()?
            .with_context(|| format!("invocation {id} does not exist"))
    }

    pub fn invocations(&self, agent: AgentId) -> Result<Vec<Invocation>> {
        self.conn
            .prepare(&format!(
                "{INVOCATION_COLUMNS} WHERE agent_id = ?1 ORDER BY id"
            ))?
            .query_map([agent], invocation_row)?
            .collect::<rusqlite::Result<_>>()
            .map_err(Into::into)
    }

    /// INTEND: journals an action `agent` intends, before anything attempts
    /// it. That an action was intended is never evidence that it happened.
    pub fn intend(&mut self, agent: AgentId, intent: &Intent) -> Result<JournalId> {
        self.write(|tx| insert_intent(tx, agent, intent))
    }

    /// Revises what an entry intends, until it is attempted. From then on
    /// its intent is history.
    pub fn revise_intent(&mut self, entry: JournalId, intent: &Intent) -> Result<()> {
        let parameters = intent.check()?;
        self.write(|tx| {
            let agent = journal_agent(tx, entry, "intended", "revised")?;
            tx.execute(
                "UPDATE journal SET action = ?2, parameters = ?3 WHERE id = ?1",
                params![entry, intent.action, parameters],
            )?;
            let (plan, task) = agent_subject(tx, agent)?;
            let detail = format!("entry {entry}: {}", intent.action);
            event(
                tx,
                "journal.revised",
                Some(plan),
                task,
                Some(agent),
                &detail,
            )
        })
    }

    /// ACT: records that an intended entry is being attempted, which must
    /// be durable before anything acts on it. `invocation` is the invocation
    /// of the entry's agent that performs or requested it, if any. From then
    /// on the outcome is unknown until the entry is reconciled.
    pub fn act(&mut self, entry: JournalId, invocation: Option<InvocationId>) -> Result<()> {
        self.write(|tx| act_entry(tx, entry, invocation))
    }

    /// RECONCILE: records, once, what was established about an attempted
    /// entry's outcome and the evidence establishing it. This says what
    /// happened, not whether the work is acceptable.
    pub fn reconcile(
        &mut self,
        entry: JournalId,
        outcome: ActionOutcome,
        evidence: &[Evidence],
    ) -> Result<()> {
        self.write(|tx| reconcile_entry(tx, entry, outcome, evidence))
    }

    pub fn journal_entry(&self, entry: JournalId) -> Result<JournalEntry> {
        self.conn
            .query_row(
                &format!("{JOURNAL_COLUMNS} WHERE id = ?1"),
                [entry],
                journal_row,
            )
            .optional()?
            .with_context(|| format!("journal entry {entry} does not exist"))
    }

    /// The canonical continuation state of `agent`'s work, for whatever
    /// embodies it next: every action it journaled, in order, with exactly
    /// what is established about each. It is reconstructed from this store
    /// alone, never from a provider session or transcript, and leaves what
    /// to do about an unknown outcome to its caller.
    pub fn continuation(&self, agent: AgentId) -> Result<Vec<JournalEntry>> {
        agent_subject(&self.conn, agent)?;
        self.conn
            .prepare(&format!(
                "{JOURNAL_COLUMNS} WHERE agent_id = ?1 ORDER BY id"
            ))?
            .query_map([agent], journal_row)?
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

const INVOCATION_COLUMNS: &str = "SELECT id, agent_id, provider, model, effort, state,
    started_at, ended_at, failure, diagnostic, exit_code, provider_session, usage,
    input_tokens, output_tokens, cached_input_tokens, cache_write_tokens, reasoning_tokens
    FROM invocations";

fn invocation_row(r: &rusqlite::Row) -> rusqlite::Result<Invocation> {
    let count = |i: usize| -> rusqlite::Result<Option<u64>> {
        Ok(r.get::<_, Option<i64>>(i)?.map(|n| n as u64))
    };
    let tokens = || -> rusqlite::Result<TokenUsage> {
        Ok(TokenUsage {
            input: count(13)?.unwrap_or_default(),
            output: count(14)?.unwrap_or_default(),
            cached_input: count(15)?,
            cache_write: count(16)?,
            reasoning: count(17)?,
        })
    };
    let usage = match r.get::<_, Option<String>>(12)?.as_deref() {
        None => None,
        Some("provider_reported") => Some(Usage::ProviderReported(tokens()?)),
        Some("local_estimate") => Some(Usage::LocalEstimate(tokens()?)),
        Some(_) => Some(Usage::Unavailable),
    };
    let state: InvocationState = r.get(5)?;
    Ok(Invocation {
        id: r.get(0)?,
        agent: r.get(1)?,
        provider: r.get(2)?,
        model: r.get(3)?,
        effort: r.get(4)?,
        state,
        started_at: r.get(6)?,
        ended_at: r.get(7)?,
        end: match usage {
            Some(usage) => Some(InvocationEnd {
                state,
                failure: r.get(8)?,
                diagnostic: r.get(9)?,
                exit_code: r.get(10)?,
                provider_session: r.get(11)?,
                usage,
            }),
            None => None,
        },
    })
}

const JOURNAL_COLUMNS: &str = "SELECT id, agent_id, action, parameters, intended_at,
    attempted_at, invocation_id, outcome, evidence, reconciled_at FROM journal";

fn journal_row(r: &Row) -> rusqlite::Result<JournalEntry> {
    let attempt = |at| -> rusqlite::Result<Attempt> {
        Ok(Attempt {
            invocation: r.get(6)?,
            at,
        })
    };
    // The schema guarantees which columns are set in each state.
    let status = match (r.get(5)?, r.get(7)?) {
        (None, _) => ActionStatus::NotAttempted,
        (Some(at), None) => ActionStatus::OutcomeUnknown(attempt(at)?),
        (Some(at), Some(outcome)) => ActionStatus::Reconciled(
            attempt(at)?,
            Reconciliation {
                outcome,
                evidence: json_column(r, 8)?,
                at: r.get(9)?,
            },
        ),
    };
    Ok(JournalEntry {
        id: r.get(0)?,
        agent: r.get(1)?,
        intent: Intent {
            action: r.get(2)?,
            parameters: json_column(r, 3)?,
        },
        intended_at: r.get(4)?,
        status,
    })
}

fn json_column<T: DeserializeOwned>(r: &Row, i: usize) -> rusqlite::Result<T> {
    let text: String = r.get(i)?;
    serde_json::from_str(&text)
        .map_err(|e| rusqlite::Error::FromSqlConversionFailure(i, Type::Text, Box::new(e)))
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

/// Refuses any file that is not a store of exactly `SCHEMA`: another
/// program's database, a partial or altered one, or one of another schema
/// version, such as a development build's from before the canonical schema.
/// Nothing is migrated or written, so a refused file is left as found.
fn identify(conn: &mut Connection) -> Result<()> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let (id, version): (i32, i64) = tx.query_row(
        "SELECT (SELECT application_id FROM pragma_application_id),
                (SELECT user_version FROM pragma_user_version)",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    ensure!(id == APPLICATION_ID, "not an agentctl state database");
    let reset = "agentctl upgrades no other: remove this file (and its -wal and -shm \
                 files) and run `agentctl init` to start afresh; project sources are \
                 left untouched";
    ensure!(
        version == SCHEMA_VERSION,
        "state schema version {version} is incompatible with this agentctl's \
         ({SCHEMA_VERSION}); {reset}"
    );
    let expected = Connection::open_in_memory()?;
    expected.execute_batch(SCHEMA)?;
    ensure!(
        schema_objects(&tx)? == schema_objects(&expected)?,
        "state database does not match agentctl schema version {SCHEMA_VERSION}; {reset}"
    );
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

pub(crate) fn now() -> i64 {
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

/// Ends an active generation as `to`, recording the accepted source
/// `sources` it establishes; see [`Store::finish_generation`] and
/// [`Store::accept_generation`].
fn end_generation(
    tx: &Transaction,
    generation: GenerationId,
    to: GenerationState,
    sources: &[(&str, Option<&str>)],
) -> Result<()> {
    let (plan, task, number) = active_generation(tx, generation)?;
    let (executed, accepting): (bool, bool) = tx.query_row(
        "SELECT EXISTS (SELECT 1 FROM executions WHERE generation_id = ?1),
                EXISTS (SELECT 1 FROM acceptances WHERE generation_id = ?1)",
        [generation],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    ensure!(
        !accepting,
        "generation {generation} is being accepted; only its acceptance ends it"
    );
    ensure!(
        to != GenerationState::Accepted || !executed,
        "generation {generation} executed, so only accepting its verified candidate \
         accepts it"
    );
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
}

/// Starts the next generation of a pending task, returning its plan, id
/// and number; see [`Store::start_generation`].
fn insert_generation(tx: &Transaction, task: TaskId) -> Result<(PlanId, GenerationId, i64)> {
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
    Ok((plan, generation, number))
}

/// The tasks `task` depends on that are not completed, in order: the one
/// definition of dependency satisfaction, which the `completed_tasks` view
/// holds. A dependency is satisfied only by a generation of the task whose
/// acceptance completed, never by anything short of that.
pub(crate) fn unsatisfied_dependencies(conn: &Connection, task: TaskId) -> Result<Vec<TaskId>> {
    conn.prepare(
        "SELECT d.depends_on FROM task_dependencies d WHERE d.task_id = ?1 AND NOT EXISTS
           (SELECT 1 FROM completed_tasks c WHERE c.task_id = d.depends_on)
         ORDER BY d.depends_on",
    )?
    .query_map([task], |r| r.get(0))?
    .collect::<rusqlite::Result<_>>()
    .map_err(Into::into)
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

fn plan_state(tx: &Connection, plan: PlanId) -> Result<PlanState> {
    tx.query_row("SELECT state FROM plans WHERE id = ?1", [plan], |r| {
        r.get(0)
    })
    .optional()?
    .with_context(|| format!("plan {plan} does not exist"))
}

/// The plan, task, number and state of a generation.
fn generation_info(
    tx: &Connection,
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

/// Creates a logical agent; see [`Store::create_agent`].
fn insert_agent(tx: &Transaction, role: Role, scope: AgentScope) -> Result<AgentId> {
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
        params![
            role,
            generation.is_none().then_some(plan),
            generation,
            now()
        ],
    )
    .with_context(|| format!("creating {role} for {scope:?}"))?;
    let agent = AgentId(tx.last_insert_rowid());
    event(
        tx,
        "agent.created",
        Some(plan),
        task,
        Some(agent),
        &role.to_string(),
    )?;
    Ok(agent)
}

/// Journals an intended action; see [`Store::intend`].
fn insert_intent(tx: &Transaction, agent: AgentId, intent: &Intent) -> Result<JournalId> {
    let parameters = intent.check()?;
    let (plan, task) = agent_subject(tx, agent)?;
    tx.execute(
        "INSERT INTO journal (agent_id, action, parameters, state, intended_at)
         VALUES (?1, ?2, ?3, 'intended', ?4)",
        params![agent, intent.action, parameters, now()],
    )?;
    let entry = JournalId(tx.last_insert_rowid());
    let detail = format!("entry {entry}: {}", intent.action);
    event(
        tx,
        "journal.intended",
        Some(plan),
        task,
        Some(agent),
        &detail,
    )?;
    Ok(entry)
}

/// Reconciles an attempted entry; see [`Store::reconcile`].
fn act_entry(tx: &Transaction, entry: JournalId, invocation: Option<InvocationId>) -> Result<()> {
    let agent = journal_agent(tx, entry, "intended", "attempted")?;
    if let Some(invocation) = invocation {
        let embodies: AgentId = tx
            .query_row(
                "SELECT agent_id FROM invocations WHERE id = ?1",
                [invocation],
                |r| r.get(0),
            )
            .optional()?
            .with_context(|| format!("invocation {invocation} does not exist"))?;
        ensure!(
            embodies == agent,
            "invocation {invocation} embodies agent {embodies}, \
             not agent {agent} whose entry {entry} it would attempt"
        );
    }
    tx.execute(
        "UPDATE journal SET state = 'attempted', invocation_id = ?2, attempted_at = ?3
         WHERE id = ?1",
        params![entry, invocation, now()],
    )?;
    let (plan, task) = agent_subject(tx, agent)?;
    let detail = match invocation {
        Some(invocation) => format!("entry {entry} by invocation {invocation}"),
        None => format!("entry {entry}"),
    };
    event(
        tx,
        "journal.attempted",
        Some(plan),
        task,
        Some(agent),
        &detail,
    )
}

fn reconcile_entry(
    tx: &Transaction,
    entry: JournalId,
    outcome: ActionOutcome,
    evidence: &[Evidence],
) -> Result<()> {
    ensure!(
        (1..=EVIDENCE_LIMIT).contains(&evidence.len()),
        "a reconciliation needs from 1 to {EVIDENCE_LIMIT} items of evidence"
    );
    evidence.iter().try_for_each(Evidence::check)?;
    let recorded = serde_json::to_string(evidence)?;
    let agent = journal_agent(tx, entry, "attempted", "reconciled")?;
    for item in evidence {
        if let Evidence::Invocation { invocation } = item {
            let (embodies, state): (AgentId, InvocationState) = tx
                .query_row(
                    "SELECT agent_id, state FROM invocations WHERE id = ?1",
                    [invocation],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?
                .with_context(|| format!("invocation {invocation} does not exist"))?;
            ensure!(
                embodies == agent,
                "invocation {invocation} embodies agent {embodies}, \
                 not agent {agent} whose entry {entry} it would evidence"
            );
            ensure!(
                state.is_terminal(),
                "invocation {invocation} has not ended, so it is no evidence yet"
            );
        }
    }
    tx.execute(
        "UPDATE journal SET state = 'reconciled', outcome = ?2, evidence = ?3,
           reconciled_at = ?4
         WHERE id = ?1",
        params![entry, outcome, recorded, now()],
    )?;
    let (plan, task) = agent_subject(tx, agent)?;
    let detail = format!("entry {entry}: {outcome}");
    event(
        tx,
        "journal.reconciled",
        Some(plan),
        task,
        Some(agent),
        &detail,
    )
}

/// The plan and, for generation-scoped agents, the task `agent` serves.
fn agent_subject(conn: &Connection, agent: AgentId) -> Result<(PlanId, Option<TaskId>)> {
    conn.query_row(
        "SELECT coalesce(a.plan_id, t.plan_id), g.task_id FROM agents a
         LEFT JOIN generations g ON g.id = a.generation_id
         LEFT JOIN tasks t ON t.id = g.task_id WHERE a.id = ?1",
        [agent],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
    .optional()?
    .with_context(|| format!("agent {agent} does not exist"))
}

/// The agent of journal entry `entry`, which must be in `state` to be
/// `doing` what the caller does.
fn journal_agent(tx: &Transaction, entry: JournalId, state: &str, doing: &str) -> Result<AgentId> {
    let (agent, actual): (AgentId, String) = tx
        .query_row(
            "SELECT agent_id, state FROM journal WHERE id = ?1",
            [entry],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?
        .with_context(|| format!("journal entry {entry} does not exist"))?;
    ensure!(
        actual == state,
        "journal entry {entry} is {actual}; only an {state} entry can be {doing}"
    );
    Ok(agent)
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

/// Accepts only the identifiers naming journal actions, parameters and facts.
fn check_identifier(what: &str, name: &str) -> Result<()> {
    let valid = name.len() <= IDENTIFIER_LIMIT
        && name.starts_with(|c: char| c.is_ascii_lowercase())
        && name
            .bytes()
            .all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'_' | b'.'));
    ensure!(valid, "{what} `{name:.64}` is not an identifier");
    Ok(())
}

impl HumanIntent {
    /// Checks the intent is within its bounds, returning its constraints and
    /// completion criteria as recorded.
    fn check(&self) -> Result<(String, String)> {
        check_text("the objective", &self.objective, OBJECTIVE_LIMIT, true)?;
        for (what, statements) in [
            ("constraints", &self.constraints),
            ("completion criteria", &self.completion_criteria),
        ] {
            ensure!(
                statements.len() <= STATEMENTS_LIMIT,
                "at most {STATEMENTS_LIMIT} {what} can be stated"
            );
            for statement in statements {
                check_text(what, statement, STATEMENT_LIMIT, true)?;
            }
        }
        Ok((
            serde_json::to_string(&self.constraints)?,
            serde_json::to_string(&self.completion_criteria)?,
        ))
    }
}

/// Accepts text of at most `limit` bytes whose only control characters are
/// line breaks and tabs, and, when `required`, that says something.
fn check_text(what: &str, text: &str, limit: usize, required: bool) -> Result<()> {
    ensure!(
        !required || !text.trim().is_empty(),
        "{what} must not be blank"
    );
    ensure!(text.len() <= limit, "{what} must be at most {limit} bytes");
    ensure!(
        !text
            .chars()
            .any(|c| c.is_control() && c != '\n' && c != '\t'),
        "{what} must not contain control characters"
    );
    Ok(())
}

impl Intent {
    /// Checks the intent is within the journal's bounds, returning its
    /// parameters as recorded.
    fn check(&self) -> Result<String> {
        check_identifier("action", &self.action)?;
        self.parameters
            .iter()
            .try_for_each(|(name, value)| check_parameter(name, value, 1))?;
        let recorded = serde_json::to_string(&self.parameters)?;
        ensure!(
            recorded.len() <= PARAMETERS_LIMIT,
            "intent parameters exceed {PARAMETERS_LIMIT} bytes"
        );
        Ok(recorded)
    }
}

fn check_parameter(name: &str, value: &Value, depth: usize) -> Result<()> {
    check_identifier("parameter", name)?;
    ensure!(
        depth <= PARAMETER_DEPTH,
        "intent parameters nest deeper than {PARAMETER_DEPTH} levels"
    );
    match value {
        Value::String(text) => ensure!(
            text.len() <= PARAMETER_TEXT_LIMIT && !text.chars().any(char::is_control),
            "parameter `{name}` is not one line of at most {PARAMETER_TEXT_LIMIT} bytes"
        ),
        Value::Array(items) => items
            .iter()
            .try_for_each(|item| check_parameter(name, item, depth + 1))?,
        Value::Object(fields) => fields
            .iter()
            .try_for_each(|(name, value)| check_parameter(name, value, depth + 1))?,
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
    Ok(())
}

impl Evidence {
    fn check(&self) -> Result<()> {
        match self {
            Self::Invocation { .. } => Ok(()),
            Self::Content { path, hash } => {
                ensure!(
                    path.len() <= EVIDENCE_PATH_LIMIT,
                    "evidence paths are at most {EVIDENCE_PATH_LIMIT} bytes"
                );
                check_identity(path, hash.as_deref())
            }
            Self::Fact { name } => check_identifier("fact", name),
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::planner::Command;
    use serde_json::json;
    use std::collections::HashSet;
    use std::sync::{Arc, Barrier};

    /// Human intent stating only an objective.
    pub(crate) fn objective(text: &str) -> HumanIntent {
        HumanIntent {
            objective: text.into(),
            constraints: Vec::new(),
            completion_criteria: Vec::new(),
        }
    }

    impl Store {
        /// Adds the task `key` to a planning plan, as a planner would.
        pub(crate) fn add_task(
            &mut self,
            plan: PlanId,
            key: &str,
            depends_on: &[TaskId],
        ) -> Result<TaskId> {
            let depends_on = depends_on
                .iter()
                .map(|&task| Ok(self.task(task)?.key))
                .collect::<Result<_>>()?;
            let add = Command::AddTask {
                task: key.into(),
                objective: key.into(),
                context: String::new(),
                paths: Vec::new(),
                depends_on,
            };
            self.revise_plan(plan, &[add], &|_| Ok(()))?;
            let tasks = self.tasks(plan)?;
            Ok(tasks.into_iter().find(|t| t.key == key).unwrap().id)
        }
    }

    pub(super) fn store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("state.db")).unwrap();
        (dir, store)
    }

    /// A finalized plan of tasks `(key, scope, depends_on)`, as a planner
    /// would make it, and their ids in order.
    pub(crate) fn ready_plan(
        store: &mut Store,
        tasks: &[(&str, &[&str], &[&str])],
    ) -> (PlanId, Vec<TaskId>) {
        let plan = store.create_plan(&objective("intent")).unwrap();
        let strings = |items: &[&str]| items.iter().map(|&s| s.into()).collect();
        let mut commands: Vec<_> = tasks
            .iter()
            .map(|&(key, paths, depends_on)| Command::AddTask {
                task: key.into(),
                objective: key.into(),
                context: String::new(),
                paths: strings(paths),
                depends_on: strings(depends_on),
            })
            .collect();
        commands.push(Command::Finalize {});
        assert!(store.revise_plan(plan, &commands, &|_| Ok(())).unwrap());
        let ids = store.tasks(plan).unwrap().iter().map(|t| t.id).collect();
        (plan, ids)
    }

    /// Acquires `paths` for `generation`, which must succeed.
    pub(crate) fn acquire(store: &mut Store, generation: GenerationId, paths: &[&str]) {
        let acquired = store.acquire_ownership(generation, paths).unwrap();
        assert_eq!(acquired, Acquisition::Acquired, "{paths:?}");
    }

    /// A structurally valid content hash.
    fn hash(n: u8) -> String {
        format!("{n:064x}")
    }

    pub(crate) fn err(result: Result<impl fmt::Debug>) -> String {
        format!("{:#}", result.unwrap_err())
    }

    pub(crate) fn version(path: &Path) -> i64 {
        Connection::open(path)
            .unwrap()
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap()
    }

    pub(crate) fn agent_id(id: i64) -> AgentId {
        AgentId(id)
    }

    pub(crate) fn ended(state: InvocationState) -> InvocationEnd {
        InvocationEnd {
            state,
            failure: None,
            diagnostic: None,
            exit_code: None,
            provider_session: None,
            usage: Usage::Unavailable,
        }
    }

    pub(crate) fn succeeded() -> InvocationEnd {
        InvocationEnd {
            exit_code: Some(0),
            ..ended(InvocationState::Succeeded)
        }
    }

    /// A plan with one task running its first generation.
    fn running_generation(store: &mut Store) -> (PlanId, TaskId, GenerationId) {
        let plan = store.create_plan(&objective("intent")).unwrap();
        let task = store.add_task(plan, "task", &[]).unwrap();
        let generation = store.start_generation(task).unwrap();
        (plan, task, generation)
    }

    #[test]
    fn creates_schema_and_reopens_without_reset() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        let plan = Store::open(&path)
            .unwrap()
            .create_plan(&objective("ship it"))
            .unwrap();
        assert_eq!(version(&path), SCHEMA_VERSION);

        let store = Store::open(&path).unwrap();
        assert_eq!(store.plan(plan).unwrap().intent, objective("ship it"));
        assert_eq!(store.events_after(0, 10).unwrap().len(), 1);
        let mode: String = store
            .conn
            .pragma_query_value(None, "journal_mode", |r| r.get(0))
            .unwrap();
        assert_eq!(mode, "wal");
    }

    /// Every object of the canonical schema, as `(type, name)`, in
    /// `schema_objects` order: what a fresh store must hold, whatever
    /// `SCHEMA` itself says.
    const CANONICAL_OBJECTS: [(&str, &str); 128] = [
        ("index", "agents_one_executor"),
        ("index", "generations_live"),
        ("index", "generations_state"),
        ("index", "graph_relations_by_source"),
        ("index", "graph_relations_from"),
        ("index", "graph_relations_to"),
        ("index", "invocations_live"),
        ("index", "journal_by_agent"),
        ("index", "ownership_by_generation"),
        ("table", "acceptance_completions"),
        ("table", "acceptance_phases"),
        ("table", "acceptance_sources"),
        ("table", "acceptances"),
        ("table", "accepted_sources"),
        ("table", "agents"),
        ("table", "decisions"),
        ("table", "events"),
        ("table", "execution_baseline"),
        ("table", "execution_captures"),
        ("table", "execution_changes"),
        ("table", "execution_install_results"),
        ("table", "execution_installs"),
        ("table", "executions"),
        ("table", "generation_abandonments"),
        ("table", "generation_revisions"),
        ("table", "generations"),
        ("table", "graph_entities"),
        ("table", "graph_relations"),
        ("table", "graph_sites"),
        ("table", "graph_sources"),
        ("table", "invocations"),
        ("table", "journal"),
        ("table", "ownership"),
        ("table", "plans"),
        ("table", "replans"),
        ("table", "retry_authorizations"),
        ("table", "scheduler_claims"),
        ("table", "scheduler_releases"),
        ("table", "task_cancellations"),
        ("table", "task_dependencies"),
        ("table", "task_revisions"),
        ("table", "task_scope"),
        ("table", "tasks"),
        ("table", "verification_results"),
        ("table", "verifications"),
        ("trigger", "acceptance_completions_given_back"),
        ("trigger", "acceptance_completions_held"),
        ("trigger", "acceptance_completions_taken"),
        ("trigger", "acceptance_phases_completed"),
        ("trigger", "acceptance_phases_immutable"),
        ("trigger", "acceptance_phases_no_delete"),
        ("trigger", "acceptance_phases_ordered"),
        ("trigger", "acceptance_sources_derived"),
        ("trigger", "acceptance_sources_immutable"),
        ("trigger", "acceptance_sources_no_delete"),
        ("trigger", "acceptances_immutable"),
        ("trigger", "acceptances_intended"),
        ("trigger", "acceptances_no_delete"),
        ("trigger", "accepted_sources_held_by_acceptance"),
        ("trigger", "decisions_no_delete"),
        ("trigger", "decisions_no_update"),
        ("trigger", "events_no_delete"),
        ("trigger", "events_no_update"),
        ("trigger", "execution_baseline_immutable"),
        ("trigger", "execution_baseline_no_delete"),
        ("trigger", "execution_baseline_precedes_attempt"),
        ("trigger", "execution_captures_derived"),
        ("trigger", "execution_captures_immutable"),
        ("trigger", "execution_captures_no_delete"),
        ("trigger", "execution_changes_derived"),
        ("trigger", "execution_changes_immutable"),
        ("trigger", "execution_changes_no_delete"),
        ("trigger", "execution_install_results_derived"),
        ("trigger", "execution_install_results_immutable"),
        ("trigger", "execution_install_results_no_delete"),
        ("trigger", "execution_installs_immutable"),
        ("trigger", "execution_installs_intended"),
        ("trigger", "execution_installs_no_delete"),
        ("trigger", "executions_immutable"),
        ("trigger", "executions_intended"),
        ("trigger", "executions_no_delete"),
        ("trigger", "generation_abandonments_authorized"),
        ("trigger", "generation_abandonments_immutable"),
        ("trigger", "generation_abandonments_no_delete"),
        ("trigger", "generation_revisions_bound"),
        ("trigger", "generation_revisions_immutable"),
        ("trigger", "generation_revisions_no_delete"),
        ("trigger", "generations_abandoned_by_replan"),
        ("trigger", "generations_accepted_by_acceptance"),
        ("trigger", "journal_forward_only"),
        ("trigger", "journal_no_delete"),
        ("trigger", "journal_reconciles_execution"),
        ("trigger", "journal_reconciles_install"),
        ("trigger", "journal_reconciles_verification"),
        ("trigger", "ownership_acquired"),
        ("trigger", "ownership_held_through_acceptance"),
        ("trigger", "ownership_held_until_abandoned"),
        ("trigger", "ownership_not_transferred"),
        ("trigger", "plans_intent_immutable"),
        ("trigger", "replans_applied"),
        ("trigger", "replans_immutable"),
        ("trigger", "replans_no_delete"),
        ("trigger", "retry_authorizations_given"),
        ("trigger", "retry_authorizations_no_delete"),
        ("trigger", "retry_authorizations_used"),
        ("trigger", "scheduler_claims_immutable"),
        ("trigger", "scheduler_claims_no_delete"),
        ("trigger", "scheduler_claims_taken"),
        ("trigger", "scheduler_releases_derived"),
        ("trigger", "scheduler_releases_immutable"),
        ("trigger", "scheduler_releases_no_delete"),
        ("trigger", "task_cancellations_immutable"),
        ("trigger", "task_cancellations_no_delete"),
        ("trigger", "task_cancellations_recorded"),
        ("trigger", "task_revisions_immutable"),
        ("trigger", "task_revisions_no_delete"),
        ("trigger", "task_revisions_recorded"),
        ("trigger", "tasks_identity_immutable"),
        ("trigger", "tasks_identity_not_replaced"),
        ("trigger", "verification_results_derived"),
        ("trigger", "verification_results_immutable"),
        ("trigger", "verification_results_no_delete"),
        ("trigger", "verifications_immutable"),
        ("trigger", "verifications_intended"),
        ("trigger", "verifications_no_delete"),
        ("view", "completed_tasks"),
        ("view", "scheduler_outcomes"),
        ("view", "task_definitions"),
    ];

    #[test]
    fn a_fresh_store_holds_exactly_the_canonical_schema() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        let store = Store::open(&path).unwrap();
        let objects = schema_objects(&store.conn).unwrap();
        let names: Vec<_> = objects
            .iter()
            .map(|(kind, name, _)| (kind.as_str(), name.as_str()))
            .collect();
        assert_eq!(names, CANONICAL_OBJECTS);
        let built = Connection::open_in_memory().unwrap();
        built.execute_batch(SCHEMA).unwrap();
        assert_eq!(objects, schema_objects(&built).unwrap());
        let id: i32 = store
            .conn
            .pragma_query_value(None, "application_id", |r| r.get(0))
            .unwrap();
        assert_eq!((id, version(&path)), (APPLICATION_ID, 1));
        assert!(enforces_foreign_keys(&store));
        // No other schema is kept to be read or upgraded from.
        let sources = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/state");
        for entry in std::fs::read_dir(sources).unwrap() {
            let name = entry.unwrap().file_name().into_string().unwrap();
            assert!(!name.starts_with("migrate"), "{name}");
        }
    }

    #[test]
    fn refuses_other_schema_versions_untouched() {
        let dir = tempfile::tempdir().unwrap();
        // Earlier development builds' versions, and any other, are never
        // upgraded or reinterpreted, even over the current schema itself.
        for other in [0, 2, 13, 14, SCHEMA_VERSION + 100, -1] {
            let name = format!("v{other}.db");
            let path = dir.path().join(&name);
            drop(Store::open(&path).unwrap());
            Connection::open(&path)
                .unwrap()
                .pragma_update(None, "user_version", other)
                .unwrap();
            let before = std::fs::read(&path).unwrap();
            let message = err(Store::open(&path));
            let expected = format!("state schema version {other} is incompatible");
            assert!(message.contains(&expected), "{message}");
            assert!(message.contains("agentctl init"), "{message}");
            assert_eq!(std::fs::read(&path).unwrap(), before, "{name}");
            assert_eq!(version(&path), other, "{name}");
        }

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
            // The first development builds' stores, numbered version 1 as
            // well, differ from the canonical schema and are never read.
            (
                "development.db",
                "DROP TABLE graph_sites; DROP TABLE graph_relations;
                 DROP TABLE graph_entities; DROP TABLE graph_sources;
                 DROP TABLE accepted_sources;
                 CREATE TABLE accepted_sources (
                     path          TEXT    PRIMARY KEY,
                     hash          TEXT    NOT NULL CHECK (hash <> ''),
                     generation_id INTEGER REFERENCES generations (id)
                 ) STRICT, WITHOUT ROWID",
            ),
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
        let plan = store.create_plan(&objective("intent")).unwrap();

        for to in [Running, Paused, Completed, Planning] {
            assert!(err(store.set_plan_state(plan, to)).contains("cannot go from planning"));
        }
        // Only finalized planning makes a plan ready.
        let finalizing = "becomes ready only by finalizing";
        assert!(err(store.set_plan_state(plan, Ready)).contains(finalizing));
        let task = store.add_task(plan, "task", &[]).unwrap();
        store
            .revise_plan(plan, &[Command::Finalize {}], &|_| Ok(()))
            .unwrap();
        for to in [Running, Paused, Running, Planning, NeedsAttention] {
            store.set_plan_state(plan, to).unwrap();
        }
        assert!(err(store.set_plan_state(plan, Ready)).contains(finalizing));
        store.set_plan_state(plan, Running).unwrap();

        let message = err(store.set_plan_state(plan, Completed));
        assert!(message.contains("1 uncompleted tasks"), "{message}");
        let generation = store.start_generation(task).unwrap();
        store.accept_generation(generation, &[]).unwrap();
        store.set_plan_state(plan, Completed).unwrap();
        assert_eq!(store.plan(plan).unwrap().state, Completed);

        for to in [Planning, Ready, Running, Paused, NeedsAttention, Completed] {
            assert!(store.set_plan_state(plan, to).is_err());
        }
        assert!(err(store.add_task(plan, "late", &[])).contains("only a planning plan"));
    }

    #[test]
    fn references_are_enforced() {
        let (_dir, mut store) = store();
        let a = store.create_plan(&objective("a")).unwrap();
        let b = store.create_plan(&objective("b")).unwrap();
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
        assert!(store.start_invocation(AgentId(99), "p", "m", None).is_err());

        // The database itself refuses dangling references and self-cycles.
        let raw = |sql: &str| store.conn.execute(sql, []).unwrap_err().to_string();
        assert!(
            raw(
                "INSERT INTO tasks (plan_id, key, objective, context, created_at)
                 VALUES (99, 'x', 'x', '', 0)"
            )
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
        let plan = store.create_plan(&objective("intent")).unwrap();
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
        // Accepted without executing, so without a completed acceptance:
        // that satisfies no dependency.
        assert!(!store.dependencies_satisfied(b).unwrap());
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
        let plan = store.create_plan(&objective("intent")).unwrap();
        let [a, b, c, d] = ["a", "b", "c", "d"].map(|t| store.add_task(plan, t, &[]).unwrap());
        let on = |task: &str, deps: &[&str]| Command::SetDependencies {
            task: task.into(),
            depends_on: deps.iter().map(|d| d.to_string()).collect(),
        };

        // Edges run against creation order as freely as with it: a diamond
        // where the earliest task depends on later ones.
        let diamond = [on("a", &["b", "c"]), on("b", &["d"]), on("c", &["d"])];
        store.revise_plan(plan, &diamond, &|_| Ok(())).unwrap();
        let e = store.add_task(plan, "e", &[a, d]).unwrap();
        assert_eq!(store.task(a).unwrap().depends_on, vec![b, c]);
        assert_eq!(store.task(e).unwrap().depends_on, vec![a, d]);
        assert!(!store.dependencies_satisfied(a).unwrap());
        assert!(store.dependencies_satisfied(d).unwrap());

        let events = store.events_after(0, 100).unwrap();
        for (task, dep, refusal) in [
            ("d", "d", "cannot depend on itself"),
            ("b", "a", "would form a cycle"),
            ("d", "a", "would form a cycle"),
            ("d", "e", "would form a cycle"),
            ("a", "b", "more than once"),
        ] {
            let deps = on(task, &[dep, dep]);
            let message = err(store.revise_plan(plan, &[deps], &|_| Ok(())));
            assert!(message.contains(refusal), "{message}");
        }
        assert_eq!(store.task(a).unwrap().depends_on, vec![b, c]);
        assert_eq!(store.task(d).unwrap().depends_on, vec![]);
        assert_eq!(store.events_after(0, 100).unwrap(), events);
    }

    #[test]
    fn invocation_lifecycle_is_explicit_and_final() {
        let (_dir, mut store) = store();
        let plan = store.create_plan(&objective("intent")).unwrap();
        let agent = store
            .create_agent(Role::Planner, AgentScope::Plan(plan))
            .unwrap();
        let id = store
            .start_invocation(agent, "claude", "haiku", Some("low"))
            .unwrap();
        let started = store.invocation(id).unwrap();
        assert_eq!(
            (started.agent, started.state, started.effort.as_deref()),
            (agent, InvocationState::Starting, Some("low"))
        );
        assert_eq!((started.ended_at, started.end), (None, None));

        store.invocation_running(id).unwrap();
        assert!(store.invocation_running(id).is_err(), "already running");
        assert_eq!(
            store.invocation(id).unwrap().state,
            InvocationState::Running
        );
        let live = err(store.finish_invocation(id, &ended(InvocationState::Running)));
        assert!(live.contains("cannot end as running"), "{live}");

        let end = InvocationEnd {
            state: InvocationState::Succeeded,
            failure: None,
            diagnostic: None,
            exit_code: Some(0),
            provider_session: Some("session".into()),
            usage: Usage::ProviderReported(TokenUsage {
                input: 10,
                output: 5,
                cached_input: Some(4),
                cache_write: None,
                reasoning: Some(2),
            }),
        };
        store.finish_invocation(id, &end).unwrap();
        let finished = store.invocation(id).unwrap();
        assert_eq!(finished.state, InvocationState::Succeeded);
        assert!(finished.ended_at.is_some());
        assert_eq!(finished.end, Some(end));
        assert!(
            store
                .finish_invocation(id, &ended(InvocationState::Cancelled))
                .is_err(),
            "ends are final"
        );
        let kinds: Vec<_> = store
            .events_after(0, 100)
            .unwrap()
            .into_iter()
            .map(|e| e.kind)
            .filter(|k| k.starts_with("invocation."))
            .collect();
        assert_eq!(
            kinds,
            [
                "invocation.started",
                "invocation.running",
                "invocation.ended"
            ]
        );
    }

    #[test]
    fn invocation_ends_must_be_consistent() {
        let (_dir, mut store) = store();
        let plan = store.create_plan(&objective("intent")).unwrap();
        let agent = store
            .create_agent(Role::Planner, AgentScope::Plan(plan))
            .unwrap();
        let id = store.start_invocation(agent, "codex", "m", None).unwrap();
        let tokens = TokenUsage::default();
        let failed = |failure, diagnostic: Option<&str>| InvocationEnd {
            failure,
            diagnostic: diagnostic.map(Into::into),
            ..ended(InvocationState::Failed)
        };
        for bad in [
            failed(None, Some("why")),
            failed(Some(FailureKind::NoResult), None),
            InvocationEnd {
                failure: Some(FailureKind::NoResult),
                ..ended(InvocationState::Succeeded)
            },
            ended(InvocationState::Interrupted),
            InvocationEnd {
                provider_session: Some(String::new()),
                ..ended(InvocationState::Cancelled)
            },
            InvocationEnd {
                usage: Usage::ProviderReported(TokenUsage {
                    input: u64::MAX,
                    ..tokens
                }),
                ..ended(InvocationState::Succeeded)
            },
        ] {
            assert!(store.finish_invocation(id, &bad).is_err(), "{bad:?}");
            assert_eq!(
                store.invocation(id).unwrap().state,
                InvocationState::Starting
            );
        }
        // A launch that never happened can still fail: `starting` may end.
        let end = failed(Some(FailureKind::ExecutableMissing), Some("no claude"));
        store.finish_invocation(id, &end).unwrap();
        assert_eq!(store.invocation(id).unwrap().end, Some(end));
    }

    #[test]
    fn invocation_transitions_are_enforced_by_the_store() {
        use FailureKind::*;
        let (_dir, mut store) = store();
        let plan = store.create_plan(&objective("intent")).unwrap();
        let failed = |kind, exit_code| InvocationEnd {
            failure: Some(kind),
            diagnostic: Some("why".into()),
            exit_code,
            ..ended(InvocationState::Failed)
        };
        let invocation = |store: &mut Store| {
            let agent = store
                .create_agent(Role::Planner, AgentScope::Plan(plan))
                .unwrap();
            store.start_invocation(agent, "claude", "m", None).unwrap()
        };
        // Each refusal leaves the row and the event log exactly as they were.
        let refuse = |store: &mut Store, id, end: &InvocationEnd| {
            let (row, events) = (store.invocation(id).unwrap(), store.events_after(0, 1000));
            assert!(store.finish_invocation(id, end).is_err(), "{end:?}");
            assert_eq!(store.invocation(id).unwrap(), row, "{end:?}");
            assert_eq!(store.events_after(0, 1000).unwrap(), events.unwrap());
        };

        // Nothing but a launch failure or interruption ends `starting`.
        let starting = invocation(&mut store);
        for end in [
            succeeded(),
            ended(InvocationState::Cancelled),
            failed(ProviderError, None),
            failed(NoResult, None),
            failed(ExitStatus, Some(1)),
            failed(SpawnFailed, Some(1)),
        ] {
            refuse(&mut store, starting, &end);
        }
        store
            .finish_invocation(starting, &failed(SpawnFailed, None))
            .unwrap();
        let lost = invocation(&mut store);
        let interrupted = InvocationEnd {
            diagnostic: Some("lost".into()),
            ..ended(InvocationState::Interrupted)
        };
        store.finish_invocation(lost, &interrupted).unwrap();

        // Success needs a zero exit; a launched process cannot fail to launch.
        let running = invocation(&mut store);
        store.invocation_running(running).unwrap();
        for end in [
            ended(InvocationState::Succeeded),
            InvocationEnd {
                exit_code: Some(1),
                ..succeeded()
            },
            failed(ExecutableMissing, None),
            failed(SpawnFailed, None),
        ] {
            refuse(&mut store, running, &end);
        }
        store
            .finish_invocation(running, &failed(ExitStatus, Some(2)))
            .unwrap();

        // Terminal states are final, whatever they would become.
        for id in [starting, lost, running] {
            for end in [
                succeeded(),
                ended(InvocationState::Cancelled),
                interrupted.clone(),
                failed(SpawnFailed, None),
                failed(ExitStatus, Some(1)),
            ] {
                refuse(&mut store, id, &end);
            }
        }

        // The schema itself refuses a success with a nonzero exit.
        let other = invocation(&mut store);
        store.invocation_running(other).unwrap();
        store.finish_invocation(other, &succeeded()).unwrap();
        assert!(
            store
                .conn
                .execute(
                    "UPDATE invocations SET exit_code = 1 WHERE id = ?1",
                    [other]
                )
                .is_err()
        );
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

        let first = store
            .start_invocation(executor, "claude", "m1", None)
            .unwrap();
        let message = err(store.start_invocation(executor, "claude", "m1", None));
        assert!(message.contains("UNIQUE"), "{message}");
        store.invocation_running(first).unwrap();
        store
            .finish_invocation(first, &ended(InvocationState::Cancelled))
            .unwrap();
        assert!(
            store
                .finish_invocation(first, &ended(InvocationState::Cancelled))
                .is_err()
        );
        let second = store
            .start_invocation(executor, "codex", "m2", None)
            .unwrap();

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

    pub(super) fn intent(action: &str, parameters: serde_json::Value) -> Intent {
        Intent {
            action: action.into(),
            parameters: parameters.as_object().unwrap().clone(),
        }
    }

    /// An agent with an ended invocation, which can evidence a
    /// reconciliation.
    fn agent_with_ended_invocation(store: &mut Store) -> (AgentId, InvocationId) {
        let plan = store.create_plan(&objective("intent")).unwrap();
        let agent = store
            .create_agent(Role::Planner, AgentScope::Plan(plan))
            .unwrap();
        let invocation = store.start_invocation(agent, "claude", "m", None).unwrap();
        store.invocation_running(invocation).unwrap();
        store.finish_invocation(invocation, &succeeded()).unwrap();
        (agent, invocation)
    }

    #[test]
    fn journal_lifecycle_is_intend_act_reconcile() {
        use ActionOutcome::*;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        let mut store = Store::open(&path).unwrap();
        let (agent, invocation) = agent_with_ended_invocation(&mut store);
        let write = intent("source.write", json!({"path": "src/[slug]/*.rs"}));
        let content = Evidence::Content {
            path: "src/[slug]/*.rs".into(),
            hash: Some(hash(1)),
        };

        let entries: Vec<_> = [
            None,
            Some(CompletedAsIntended),
            Some(CompletedWithDeviation),
        ]
        .into_iter()
        .chain([Some(Failed)])
        .map(|outcome| {
            let entry = store.intend(agent, &write).unwrap();
            if let Some(outcome) = outcome {
                store.act(entry, Some(invocation)).unwrap();
                let evidence = [Evidence::Invocation { invocation }, content.clone()];
                store.reconcile(entry, outcome, &evidence).unwrap();
            }
            entry
        })
        .collect();
        let pending = store.intend(agent, &write).unwrap();
        store
            .revise_intent(
                pending,
                &intent("source.delete", json!({"path": "src/a.rs"})),
            )
            .unwrap();
        let unknown = store.intend(agent, &write).unwrap();
        store.act(unknown, None).unwrap();

        let continuation = store.continuation(agent).unwrap();
        let statuses: Vec<_> = continuation
            .iter()
            .map(|e| match &e.status {
                ActionStatus::NotAttempted => ("not attempted", None, None),
                ActionStatus::OutcomeUnknown(a) => ("unknown", a.invocation, None),
                ActionStatus::Reconciled(a, r) => ("reconciled", a.invocation, Some(r.outcome)),
            })
            .collect();
        assert_eq!(
            statuses,
            [
                ("not attempted", None, None),
                ("reconciled", Some(invocation), Some(CompletedAsIntended)),
                ("reconciled", Some(invocation), Some(CompletedWithDeviation)),
                ("reconciled", Some(invocation), Some(Failed)),
                ("not attempted", None, None),
                ("unknown", None, None),
            ]
        );
        assert_eq!(continuation[0].id, entries[0]);
        assert_eq!(continuation[1].intent, write, "paths are kept literally");
        assert_eq!(continuation[4].intent.action, "source.delete");
        let ActionStatus::Reconciled(_, reconciliation) = &continuation[2].status else {
            unreachable!()
        };
        assert_eq!(
            reconciliation.evidence,
            [Evidence::Invocation { invocation }, content]
        );
        assert_eq!(store.journal_entry(unknown).unwrap(), continuation[5]);

        let kinds: Vec<_> = store
            .events_after(0, 100)
            .unwrap()
            .into_iter()
            .filter(|e| e.kind.starts_with("journal."))
            .map(|e| e.kind)
            .collect();
        assert_eq!(kinds.len(), 1 + 3 * 3 + 2 + 2);
        assert!(kinds.contains(&"journal.revised".to_owned()));

        drop(store);
        let fresh = Store::open(&path).unwrap();
        assert_eq!(fresh.continuation(agent).unwrap(), continuation);
        assert!(err(fresh.continuation(AgentId(99))).contains("does not exist"));
    }

    #[test]
    fn journal_transitions_are_enforced_by_the_store() {
        use ActionOutcome::*;
        let (_dir, mut store) = store();
        let (agent, invocation) = agent_with_ended_invocation(&mut store);
        let (other, foreign) = agent_with_ended_invocation(&mut store);
        let live = store.start_invocation(agent, "claude", "m", None).unwrap();
        let write = intent("source.write", json!({"path": "src/a.rs"}));
        let evidence = [Evidence::Invocation { invocation }];
        let entry = store.intend(agent, &write).unwrap();

        // A refused call changes neither the entry nor history.
        let refused =
            |store: &mut Store, call: &dyn Fn(&mut Store) -> Result<()>, expected: &str| {
                let before = (
                    store.journal_entry(entry).unwrap(),
                    store.events_after(0, 1000).unwrap(),
                );
                let message = err(call(store));
                assert!(message.contains(expected), "{message}");
                let after = (
                    store.journal_entry(entry).unwrap(),
                    store.events_after(0, 1000).unwrap(),
                );
                assert_eq!(after, before);
            };
        let reconcile = |outcome, evidence: Vec<Evidence>| {
            move |store: &mut Store| store.reconcile(entry, outcome, &evidence)
        };
        let attempted_only = "only an attempted entry can be reconciled";
        let intended_only = "only an intended entry can be attempted";

        refused(
            &mut store,
            &reconcile(Failed, evidence.to_vec()),
            attempted_only,
        );
        let embodied = format!("embodies agent {other}");
        refused(&mut store, &|s| s.act(entry, Some(foreign)), &embodied);
        refused(
            &mut store,
            &|s| s.act(entry, Some(InvocationId(99))),
            "does not exist",
        );
        assert!(err(store.act(JournalId(99), None)).contains("does not exist"));

        store.act(entry, Some(live)).unwrap();
        refused(&mut store, &|s| s.act(entry, Some(live)), intended_only);
        refused(&mut store, &|s| s.act(entry, None), intended_only);
        let revised = intent("source.write", json!({}));
        let revise = |s: &mut Store| s.revise_intent(entry, &revised);
        refused(&mut store, &revise, "only an intended entry can be revised");
        refused(
            &mut store,
            &reconcile(CompletedAsIntended, vec![]),
            "from 1 to 32 items",
        );
        let unended = vec![Evidence::Invocation { invocation: live }];
        refused(
            &mut store,
            &reconcile(CompletedAsIntended, unended),
            "has not ended",
        );
        let missing = vec![Evidence::Invocation {
            invocation: InvocationId(99),
        }];
        refused(
            &mut store,
            &reconcile(CompletedAsIntended, missing),
            "does not exist",
        );
        // Evidence of another agent's invocation says nothing about this
        // agent's action, even alongside evidence of its own.
        for foreign in [
            vec![Evidence::Invocation {
                invocation: foreign,
            }],
            vec![
                evidence[0].clone(),
                Evidence::Invocation {
                    invocation: foreign,
                },
            ],
        ] {
            refused(
                &mut store,
                &reconcile(CompletedAsIntended, foreign),
                &embodied,
            );
        }

        store
            .reconcile(entry, CompletedAsIntended, &evidence)
            .unwrap();
        let fact = vec![Evidence::Fact {
            name: "late".into(),
        }];
        // Neither a repeated nor a contradictory reconciliation is accepted.
        for (outcome, evidence) in [
            (CompletedAsIntended, evidence.to_vec()),
            (Failed, evidence.to_vec()),
            (CompletedWithDeviation, fact),
        ] {
            refused(&mut store, &reconcile(outcome, evidence), attempted_only);
        }
        refused(&mut store, &|s| s.act(entry, None), intended_only);
        refused(&mut store, &revise, "only an intended entry can be revised");
    }

    #[test]
    fn journal_schema_refuses_rewriting_history() {
        let (_dir, mut store) = store();
        let (agent, invocation) = agent_with_ended_invocation(&mut store);
        let write = intent("source.write", json!({"path": "src/a.rs"}));
        let evidence = [Evidence::Invocation { invocation }];
        let intended = store.intend(agent, &write).unwrap();
        let attempted = store.intend(agent, &write).unwrap();
        store.act(attempted, Some(invocation)).unwrap();
        let reconciled = store.intend(agent, &write).unwrap();
        store.act(reconciled, None).unwrap();
        store
            .reconcile(reconciled, ActionOutcome::Failed, &evidence)
            .unwrap();
        let recorded = store.continuation(agent).unwrap();

        let refused = |sql: &str, entry: JournalId| {
            let message = err(store.conn.execute(sql, [entry]).map_err(Into::into));
            assert!(
                message.contains("immutable") || message.contains("CHECK"),
                "{sql}: {message}"
            );
        };
        refused(
            "UPDATE journal SET state = 'reconciled', outcome = 'failed', evidence = '[1]', reconciled_at = 1 WHERE id = ?1",
            intended,
        );
        // Skipping ACT is refused even when the update supplies everything a
        // reconciled entry has.
        let message = err(store
            .conn
            .execute(
                "UPDATE journal SET state = 'reconciled', attempted_at = 1, outcome = 'failed',
                   evidence = '[1]', reconciled_at = 1 WHERE id = ?1",
                [intended],
            )
            .map_err(Into::into));
        assert!(message.contains("immutable"), "{message}");
        refused(
            "UPDATE journal SET invocation_id = 1 WHERE id = ?1",
            intended,
        );
        refused("UPDATE journal SET agent_id = 2 WHERE id = ?1", intended);
        refused(
            "UPDATE journal SET parameters = '{}' WHERE id = ?1",
            attempted,
        );
        refused(
            "UPDATE journal SET action = 'other' WHERE id = ?1",
            attempted,
        );
        refused(
            "UPDATE journal SET invocation_id = NULL WHERE id = ?1",
            attempted,
        );
        refused(
            "UPDATE journal SET state = 'intended', attempted_at = NULL WHERE id = ?1",
            attempted,
        );
        refused(
            "UPDATE journal SET attempted_at = attempted_at + 1 WHERE id = ?1",
            attempted,
        );
        refused(
            "UPDATE journal SET state = 'reconciled', outcome = 'failed', reconciled_at = 1 WHERE id = ?1",
            attempted,
        );
        refused(
            "UPDATE journal SET state = 'reconciled', outcome = 'failed', evidence = '[]', reconciled_at = 1 WHERE id = ?1",
            attempted,
        );
        refused(
            "UPDATE journal SET outcome = 'completed_as_intended' WHERE id = ?1",
            reconciled,
        );
        refused(
            "UPDATE journal SET evidence = '[2]' WHERE id = ?1",
            reconciled,
        );
        for entry in [intended, attempted, reconciled] {
            refused("DELETE FROM journal WHERE id = ?1", entry);
        }
        assert_eq!(store.continuation(agent).unwrap(), recorded);
    }

    #[test]
    fn journal_records_only_bounded_structure() {
        let (_dir, mut store) = store();
        let (agent, invocation) = agent_with_ended_invocation(&mut store);
        let deep = (0..PARAMETER_DEPTH).fold(json!(1), |v, _| json!([v]));
        let cases = [
            ("", json!({}), "not an identifier"),
            ("Source.Write", json!({}), "not an identifier"),
            ("source write", json!({}), "not an identifier"),
            (
                &"a".repeat(IDENTIFIER_LIMIT + 1),
                json!({}),
                "not an identifier",
            ),
            ("act", json!({"Bad Key": 1}), "not an identifier"),
            (
                "act",
                json!({"nested": {"bad-key": 1}}),
                "not an identifier",
            ),
            ("act", json!({"note": "line one\nline two"}), "not one line"),
            (
                "act",
                json!({"note": "x".repeat(PARAMETER_TEXT_LIMIT + 1)}),
                "not one line",
            ),
            ("act", json!({"deep": deep}), "nest deeper"),
            ("act", json!({"items": vec!["x".repeat(1000); 9]}), "exceed"),
        ];
        for (action, parameters, expected) in cases {
            let bad = intent(action, parameters);
            let message = err(store.intend(agent, &bad));
            assert!(message.contains(expected), "{action}: {message}");
        }
        let entry = store.intend(agent, &intent("act", json!({}))).unwrap();
        let shallow = (1..PARAMETER_DEPTH).fold(json!(1), |v, _| json!([v]));
        store
            .revise_intent(entry, &intent("act", json!({"deep": shallow})))
            .unwrap();
        let message = err(store.revise_intent(entry, &intent("act", json!({"x": "a\tb"}))));
        assert!(message.contains("not one line"), "{message}");
        assert_eq!(store.continuation(agent).unwrap().len(), 1);
        assert_eq!(
            store.events_after(0, 100).unwrap().last().unwrap().kind,
            "journal.revised"
        );

        store.act(entry, None).unwrap();
        let content = |path: &str, hash: Option<String>| Evidence::Content {
            path: path.into(),
            hash,
        };
        let cases = [
            (vec![content("../escape", None)], "not a canonical"),
            (vec![content("src//a.rs", None)], "not a canonical"),
            (
                vec![content(&"a".repeat(EVIDENCE_PATH_LIMIT + 1), None)],
                "at most",
            ),
            (vec![content("src/a.rs", Some("h".into()))], "not a SHA-256"),
            (
                vec![Evidence::Fact {
                    name: "tests passed".into(),
                }],
                "not an identifier",
            ),
            (
                vec![Evidence::Invocation { invocation }; EVIDENCE_LIMIT + 1],
                "from 1 to",
            ),
        ];
        for (evidence, expected) in cases {
            let message = err(store.reconcile(entry, ActionOutcome::Failed, &evidence));
            assert!(message.contains(expected), "{message}");
        }
        store
            .reconcile(
                entry,
                ActionOutcome::Failed,
                &[
                    content("src/a.rs", None),
                    Evidence::Fact {
                        name: "precondition_unmet".into(),
                    },
                ],
            )
            .unwrap();
    }

    fn enforces_foreign_keys(store: &Store) -> bool {
        store
            .conn
            .pragma_query_value(None, "foreign_keys", |r| r.get(0))
            .unwrap()
    }

    #[test]
    fn opening_a_store_always_enforces_foreign_keys() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        assert!(enforces_foreign_keys(&Store::open(&path).unwrap()));
        let store = Store::open(&path).unwrap();
        assert_eq!(version(&path), SCHEMA_VERSION);
        assert!(enforces_foreign_keys(&store));
    }

    #[test]
    fn decisions_persist_immutably() {
        let (_dir, mut store) = store();
        let plan = store.create_plan(&objective("intent")).unwrap();
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
        store.intend(agent, &intent("work", json!({}))).unwrap();

        let events = store.events_after(0, 100).unwrap();
        let kinds: Vec<_> = events.iter().map(|e| e.kind.as_str()).collect();
        assert_eq!(
            kinds,
            [
                "plan.created",
                "plan.revised",
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
    fn ending_generations_keeps_their_ownership() {
        let (_dir, mut store) = store();
        let scope = ["src/a.rs", "src/b.rs"];
        let tasks: [(&str, &[&str], &[&str]); 3] = [
            ("first", &scope, &[]),
            ("failed", &scope, &[]),
            ("rejected", &scope, &[]),
        ];
        let (_, tasks) = ready_plan(&mut store, &tasks);
        let generations: Vec<_> = tasks
            .iter()
            .map(|&t| store.start_generation(t).unwrap())
            .collect();
        let [first, failed, rejected] = generations[..] else {
            panic!()
        };
        acquire(&mut store, first, &scope);

        let message = err(store.release_ownership(first));
        assert!(message.contains("still active"), "{message}");
        let (h1, h2, h3) = (hash(1), hash(2), hash(3));
        store
            .accept_generation(first, &[("src/a.rs", Some(&h1)), ("src/b.rs", Some(&h2))])
            .unwrap();
        assert_eq!(
            store.owned_paths(first).unwrap(),
            scope,
            "acceptance alone does not release ownership"
        );
        store.release_ownership(first).unwrap();
        assert_eq!(store.owner("src/a.rs").unwrap(), None);

        acquire(&mut store, failed, &["src/a.rs"]);
        acquire(&mut store, rejected, &["src/b.rs"]);
        store
            .finish_generation(failed, GenerationEnd::Failed)
            .unwrap();
        store
            .finish_generation(rejected, GenerationEnd::Rejected)
            .unwrap();
        assert_eq!(store.owned_paths(failed).unwrap(), ["src/a.rs"]);
        assert_eq!(store.owned_paths(rejected).unwrap(), ["src/b.rs"]);

        // Not even the task's own next generation takes them implicitly.
        let task = tasks[1];
        let retry = store.start_generation(task).unwrap();
        let conflicted = store.acquire_ownership(retry, &["src/a.rs"]).unwrap();
        let Acquisition::Conflicted(conflicts) = conflicted else {
            panic!("{conflicted:?}")
        };
        assert_eq!(conflicts[0].owner.generation, failed);
        store.release_ownership(failed).unwrap();
        acquire(&mut store, retry, &["src/a.rs"]);
        store
            .accept_generation(retry, &[("src/a.rs", Some(&h3)), ("src/b.rs", None)])
            .unwrap();
        assert_eq!(
            store.accepted_source("src/b.rs").unwrap(),
            Some(AcceptedSource {
                hash: None,
                generation: Some(retry)
            }),
            "an accepted deletion is recorded as accepted absence"
        );
        assert_eq!(store.owned_paths(rejected).unwrap(), ["src/b.rs"]);
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
        let (_, tasks) = ready_plan(&mut store, &[("task", &["src/a.rs"], &[])]);
        let task = tasks[0];
        let generation = store.start_generation(task).unwrap();
        acquire(&mut store, generation, &["src/a.rs"]);
        let events = store.events_after(0, 100).unwrap();

        // The invalid second path fails after the state update and first
        // source write have already executed.
        let h1 = hash(1);
        let sources = [("src/a.rs", Some(h1.as_str())), ("../escape", Some(&h1))];
        assert!(store.accept_generation(generation, &sources).is_err());

        assert_eq!(store.task(task).unwrap().state, TaskState::Running);
        assert_eq!(store.owned_paths(generation).unwrap(), ["src/a.rs"]);
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
                        let plan = store
                            .create_plan(&objective(&format!("plan {i}.{j}")))
                            .unwrap();
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

/// Only `crate::planner`, which checks requested paths against the project,
/// revises or replans a plan's tasks or makes it ready. Other crates can
/// neither revise a plan directly:
///
/// ```compile_fail,E0624
/// # use agentctl::{planner::Command, state::{PlanId, Store}};
/// # fn f(store: &mut Store, plan: PlanId) {
/// store.revise_plan(plan, &[Command::Finalize {}], &|_| Ok(())).unwrap();
/// # }
/// ```
///
/// nor replan one:
///
/// ```compile_fail,E0624
/// # use agentctl::{planner::Command, state::{PlanId, Store}};
/// # fn f(store: &mut Store, plan: PlanId) {
/// let basis = store.replan_basis(plan).unwrap();
/// let retry = Command::RetryTask { task: "a".into() };
/// store.replan(plan, &basis, None, &[retry], &|_| Ok(()), &|_| Ok(Vec::new())).unwrap();
/// # }
/// ```
///
/// nor make it ready through its lifecycle:
///
/// ```no_run
/// # use agentctl::state::{PlanId, PlanState, Store};
/// # fn f(store: &mut Store, plan: PlanId) {
/// assert!(store.set_plan_state(plan, PlanState::Ready).is_err());
/// # }
/// ```
#[cfg(doctest)]
pub struct PlanningIsRestricted;

/// Only `crate::executor`, which observes the repository and its workspaces
/// itself, records executor attempts, what they changed, and installing it.
/// Other crates can neither intend one directly:
///
/// ```compile_fail,E0624
/// # use agentctl::state::{GenerationId, Store, TaskId};
/// # fn f(store: &mut Store, task: TaskId, generation: GenerationId) {
/// store.begin_execution(task, generation, &[], &[], "HEAD").unwrap();
/// # }
/// ```
///
/// nor capture one from observations of their own:
///
/// ```compile_fail,E0624
/// # use agentctl::state::{ExecutionId, Store};
/// # fn f(store: &mut Store, execution: ExecutionId) {
/// store.finish_execution(execution, todo!()).unwrap();
/// # }
/// ```
///
/// nor record installing a candidate without writing it:
///
/// ```compile_fail,E0624
/// # use agentctl::state::{ExecutionId, InstallOutcome, Store};
/// # fn f(store: &mut Store, execution: ExecutionId) {
/// store.finish_install(execution, InstallOutcome::Installed, &[]).unwrap();
/// # }
/// ```
#[cfg(doctest)]
pub struct ExecutionIsRestricted;

/// Only `crate::verifier`, which observes the working tree and the
/// verifier's workspace itself, records verifications. Other crates can
/// neither intend one directly:
///
/// ```compile_fail,E0624
/// # use agentctl::state::{GenerationId, Store, TaskId};
/// # fn f(store: &mut Store, task: TaskId, generation: GenerationId) {
/// store.begin_verification(task, generation, &[], 0).unwrap();
/// # }
/// ```
///
/// nor record how one ended from observations of their own:
///
/// ```compile_fail,E0624
/// # use agentctl::state::{Store, VerificationId};
/// # fn f(store: &mut Store, verification: VerificationId) {
/// store.finish_verification(verification, todo!()).unwrap();
/// # }
/// ```
#[cfg(doctest)]
pub struct VerificationIsRestricted;

/// Only `crate::acceptance`, which checks recovery objects, observes the
/// working tree and derives CodeGraph from accepted content itself,
/// records acceptances. Other crates can neither publish one's source:
///
/// ```compile_fail,E0624
/// # use agentctl::state::{GenerationId, Store, TaskId};
/// # fn f(store: &mut Store, task: TaskId, g: GenerationId) {
/// store.publish_acceptance(task, g, |_| Ok(Vec::new())).unwrap();
/// # }
/// ```
///
/// nor synchronize CodeGraph with it from contributions of their own:
///
/// ```compile_fail,E0624
/// # use agentctl::state::{GenerationId, Store};
/// # fn f(store: &mut Store, g: GenerationId) {
/// store.synchronize_acceptance(g, &[]).unwrap();
/// # }
/// ```
///
/// nor complete one:
///
/// ```compile_fail,E0624
/// # use agentctl::state::{GenerationId, Store};
/// # fn f(store: &mut Store, g: GenerationId) {
/// store.complete_acceptance(g).unwrap();
/// # }
/// ```
///
/// but can accept a verified candidate, or read what is recorded:
///
/// ```no_run
/// # fn f(p: &agentctl::project::Project, store: &mut agentctl::state::Store,
/// #      t: agentctl::state::TaskId, g: agentctl::state::GenerationId) {
/// agentctl::acceptance::accept(p, store, t, g).unwrap();
/// store.acceptance(g).unwrap();
/// # }
/// ```
#[cfg(doctest)]
pub struct AcceptanceIsRestricted;
