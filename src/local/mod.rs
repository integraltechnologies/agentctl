//! Local configuration, Git discovery, canonical state, and provider-neutral runtime.

pub mod cli;
pub mod config;
pub mod graph;
pub mod memory;
mod migrations;
pub mod paths;
pub mod planning;
pub mod repository;
pub mod runtime;
pub mod store;

use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Invalid(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("SQLite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("stored JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Validation(#[from] crate::ValidationError),
}

pub type Result<T> = std::result::Result<T, Error>;

pub(crate) fn require(condition: bool, message: impl Into<String>) -> Result<()> {
    if condition {
        Ok(())
    } else {
        Err(Error::Invalid(message.into()))
    }
}

pub fn now_ms() -> Result<u64> {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| Error::Invalid(e.to_string()))?;
    u64::try_from(duration.as_millis()).map_err(|e| Error::Invalid(e.to_string()))
}
