use std::io;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum AppError {
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
    #[error("serialization error: {0}")]
    Serde(#[from] serde_json::Error),
    #[error("database error: {0}")]
    Sqlx(#[from] sqlx::Error),
    #[error("database migration error: {0}")]
    SqlxMigrate(#[from] sqlx::migrate::MigrateError),
    #[error("daemon is already running")]
    DaemonAlreadyRunning,
    #[error("daemon is unavailable: {0}")]
    DaemonUnavailable(String),
    /// Reserved for future milestone feature gates.
    #[allow(dead_code)]
    #[error("unsupported command in this milestone: {0}")]
    Unimplemented(&'static str),
    #[error("protocol error: {0}")]
    Protocol(String),
    #[error("node not connected: {0}")]
    NodeNotConnected(String),
    #[error("max running sessions limit reached ({0})")]
    MaxSessionsReached(usize),
}

pub type Result<T> = std::result::Result<T, AppError>;
