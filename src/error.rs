//! Application error categories.
//!
//! These variants are the branch points for users and scripts:
//! `DaemonUnavailable` means the local IPC endpoint could not be reached or
//! answered; `RequestFailed` carries a daemon-side rejection of a request
//! that *was* delivered (for example `session not running: <id>`);
//! `Protocol` is a wire-contract violation (malformed frame, unsupported
//! version, stale cursor); the remaining variants are environment failures.
//!
//! Honesty rules:
//! - A delivered-but-rejected request is never reported as unavailable, and
//!   an unavailable daemon is never reported as a request failure.
//! - Unsupported capabilities are explicit refusals, never a success-shaped
//!   fallback or a silently different behavior. A dedicated
//!   capability-negotiation error arrives with the planned public interface;
//!   until then, do not invent success paths for unknown versions.

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
    #[error("{0}")]
    RequestFailed(String),
    #[error("protocol error: {0}")]
    Protocol(String),
    #[error("node not connected: {0}")]
    NodeNotConnected(String),
    #[error("max running sessions limit reached ({0})")]
    MaxSessionsReached(usize),
}

pub type Result<T> = std::result::Result<T, AppError>;
