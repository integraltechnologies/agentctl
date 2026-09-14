//! Provider-neutral local execution. Canonical packets and guarded transitions are reused.
pub(super) mod auth;
pub(crate) mod cli;
pub mod config;
pub mod credentials;
pub(crate) mod liveness;
pub mod process;
pub mod prompt;
pub mod provider;
pub mod routing;
pub mod session;
pub mod source;
pub use session::{AgentLifetime, AgentOwnership, EngineeringSession};
mod state;
use state::*;
pub use state::{RunRecord, RunState, RuntimeJob, RuntimeJobState};
mod engine;
mod experiment;
pub(crate) mod experiment_cli;
mod experiment_decisions;
mod experiment_events;
mod experiment_wakeups;
pub(crate) mod planner;
use super::{
    Error, Result,
    config::ProjectConfig,
    graph, memory, now_ms, paths, planning,
    repository::{RepositoryId, RepositoryInfo, WorkspaceId},
    require,
    store::{self, JournalEntry, Links, Store},
};
use crate::{Validate, protocol::*};
pub use config::*;
pub use engine::Runtime;
pub use experiment::{
    DEFAULT_MAX_PLANNER_WAKEUPS, DEFAULT_TIMEOUT_MS as EXPERIMENT_DEFAULT_TIMEOUT_MS,
    ExperimentAttempt, ExperimentInput, ExperimentObservation, ExperimentRun, ExperimentRuntime,
    ExperimentState, MAX_DECISION_BOUNDARIES, MAX_PLANNER_WAKEUPS,
    MAX_TIMEOUT_MS as EXPERIMENT_MAX_TIMEOUT_MS,
};
pub use experiment_decisions::{ExperimentBoundarySummary, ExperimentDecision};
pub use experiment_events::{
    DEFAULT_EVENT_QUERY_LIMIT, EVENT_FILE_ENV, ExperimentEventData, ExperimentEventQuery,
    ExperimentEventSummary, ExperimentHealthKind, ExperimentRuntimeEvent, MAX_EVENT_FRAME_BYTES,
    MAX_EVENT_QUERY_LIMIT,
};
pub use experiment_wakeups::{
    ExperimentControlSummary, ExperimentWakeup, ExperimentWakeupObservation,
    PlannerInvocationStatus, PlannerJobSummary,
};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use source::permits;
pub use source::{ArtifactRef, Artifacts, CapturedDiff, SourceSnapshot};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};
