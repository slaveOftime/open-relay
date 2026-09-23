// App-level types and helpers: status filter, sort strategy, the
// opened-terminal registry, the rate-state tracker, and the comparatively
// tiny helpers (is_active_status, session_is_active, sort_sessions,
// session_search_text, session_key, session_sort_label) used by both the
// App impl block and the renderer when deriving keys/status words/labels.
use std::path::PathBuf;
use std::time::Instant;

use std::collections::VecDeque;
use chrono::{DateTime, Utc};
use crate::protocol::SessionSummary;

use super::constants::{RATE_HISTORY_LEN, REFRESH_INTERVAL};
use super::constants::SPARK_BLOCKS;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum StatusFilter {
    #[default]
    All,
    Active,
    Inactive,
}

pub fn is_active_status(status: &str) -> bool {
    matches!(status, "created" | "running" | "stopping")
}

/// A session is "active" for sorting while it is alive (created/running/
/// stopping) or waiting for input, so attention-needed rows never sink below
/// finished ones.
pub fn session_is_active(session: &SessionSummary) -> bool {
    is_active_status(&session.status) || session.input_needed
}

/// Row ordering strategies for the session list, cycled with Ctrl+O.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum SortStrategy {
    /// Active sessions (running, waiting for input, …) first, then stopped/
    /// failed ones; each group sorted by creation time, newest first.
    #[default]
    ActiveFirst,
    /// All sessions by creation time, newest first.
    CreatedDesc,
    /// All sessions by creation time, oldest first.
    CreatedAsc,
}

impl SortStrategy {
    pub fn label(self) -> &'static str {
        match self {
            Self::ActiveFirst => "active first",
            Self::CreatedDesc => "newest",
            Self::CreatedAsc => "oldest",
        }
    }

    pub fn next(self) -> Self {
        match self {
            Self::ActiveFirst => Self::CreatedDesc,
            Self::CreatedDesc => Self::CreatedAsc,
            Self::CreatedAsc => Self::ActiveFirst,
        }
    }
}

pub fn sort_sessions(sessions: &mut [SessionSummary], strategy: SortStrategy) {
    match strategy {
        SortStrategy::ActiveFirst => sessions.sort_by(|a, b| {
            session_is_active(b)
                .cmp(&session_is_active(a))
                .then_with(|| b.created_at.cmp(&a.created_at))
        }),
        SortStrategy::CreatedDesc => {
            sessions.sort_by_key(|session| std::cmp::Reverse(session.created_at))
        }
        SortStrategy::CreatedAsc => sessions.sort_by_key(|session| session.created_at),
    }
}

impl StatusFilter {
    pub fn label(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Active => "active",
            Self::Inactive => "inactive",
        }
    }
}

#[derive(Debug)]
pub struct OpenedTerminal {
    pub marker: PathBuf,
    pub launched_at: Instant,
}

#[derive(Debug)]
pub struct RateState {
    pub total_bytes: u64,
    pub output_epoch: Option<DateTime<Utc>>,
    pub sampled_at: Instant,
    pub previous_rate: f64,
    pub rate: f64,
    pub history: VecDeque<f64>,
}

impl RateState {
    pub fn new(session: &SessionSummary, now: Instant) -> Self {
        Self {
            total_bytes: session.last_total_bytes,
            output_epoch: session.last_output_epoch,
            sampled_at: now,
            previous_rate: 0.0,
            rate: 0.0,
            history: VecDeque::from([0.0]),
        }
    }

    pub fn sample(&mut self, session: &SessionSummary, now: Instant) {
        let elapsed = now.saturating_duration_since(self.sampled_at).as_secs_f64();
        let bytes = session.last_total_bytes.saturating_sub(self.total_bytes);
        self.previous_rate = self.display_rate(now);
        self.rate = if bytes > 0 && elapsed > 0.0 {
            bytes as f64 / elapsed
        } else {
            0.0
        };
        self.total_bytes = session.last_total_bytes;
        self.output_epoch = session.last_output_epoch;
        self.sampled_at = now;
        self.history.push_back(self.rate);
        if self.history.len() > RATE_HISTORY_LEN {
            self.history.pop_front();
        }
    }

    pub fn display_rate(&self, now: Instant) -> f64 {
        let progress = now.saturating_duration_since(self.sampled_at).as_secs_f64()
            / REFRESH_INTERVAL.as_secs_f64();
        let eased = progress.clamp(0.0, 1.0);
        self.previous_rate + (self.rate - self.previous_rate) * eased
    }
}

pub fn session_search_text(session: &SessionSummary) -> String {
    let mut text = format!("{}\n{}", session.id, session.command);
    if let Some(node) = &session.node {
        text.push('\n');
        text.push_str(node);
    }
    if let Some(title) = &session.title {
        text.push('\n');
        text.push_str(title);
    }
    text.to_lowercase()
}

pub fn session_key(session: &SessionSummary) -> String {
    session.node.as_ref().map_or_else(
        || session.id.clone(),
        |node| format!("{node}\0{}", session.id),
    )
}

/// Display label used to sort sessions inside a folder. Mirrors what the
/// tree-view renders: command + args + title. Title is appended last so
/// sessions sharing a command line still bubble up in their own alpha
/// order.
pub fn session_sort_label(session: &SessionSummary) -> String {
    let mut label = session.command.clone();
    if !session.args.is_empty() {
        label.push(' ');
        label.push_str(&session.args.join(" "));
    }
    if let Some(title) = &session.title
        && !title.is_empty()
    {
        label.push(' ');
        label.push_str(title);
    }
    label
}

