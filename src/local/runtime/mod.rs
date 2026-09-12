//! Provider-neutral local execution. Canonical packets and guarded transitions are reused.
pub(super) mod auth;
pub(crate) mod cli;
pub mod config;
pub mod credentials;
pub mod process;
pub mod provider;
pub mod session;
pub mod source;
pub use session::{AgentLifetime, AgentOwnership, EngineeringSession};
mod state;
use state::*;
pub use state::{RunRecord, RunState, RuntimeJob, RuntimeJobState};
mod engine;
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
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use source::permits;
pub use source::{ArtifactRef, Artifacts, CapturedDiff, SourceSnapshot};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};
