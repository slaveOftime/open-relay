//! OSC 7501 Program Status Protocol, revision 0.4.
//! Spec: https://www.superlogical.com/rex/docs/build/program-status
//! Records are derived hints from untrusted PTY output, never approval authority.

use std::time::Instant;

use base64::{
    Engine,
    engine::general_purpose::{STANDARD, STANDARD_NO_PAD},
};
use serde::{Deserialize, Serialize};

const MAX_RECORDS: usize = 256;
pub(crate) const QUERY_REPLY: &[u8] =
    b"?:states=idle,working,done,blocked,error:kinds=permission,question,auth";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProgramState {
    Idle,
    Working,
    Done,
    Blocked,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlockedKind {
    Permission,
    Question,
    Auth,
}

/// The most relevant live report, independent of process lifecycle status.
/// Text is decoded, validated plain UTF-8; absent fields replace previous values.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProgramStatus {
    pub state: ProgramState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<BlockedKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub msg: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ProgramStatuses {
    // Least recently updated first. Bounded, and IDs are bounded to 128 bytes.
    records: Vec<ProgramStatus>,
    /// Epoch of the selected report, not background workers' progress updates.
    pub changed_at: Option<Instant>,
}

impl ProgramStatuses {
    pub fn report(&mut self, body: &[u8]) -> bool {
        let Some((id, record)) = parse(body) else {
            return false;
        };
        if let Some(record) = record {
            let current = self.current();
            let previous = self.records.iter().position(|r| r.id == id);
            let changed = previous.is_none_or(|i| self.records[i] != record);
            if let Some(i) = previous {
                self.records.remove(i);
            }
            if self.records.len() == MAX_RECORDS {
                self.records.remove(0);
            }
            self.records.push(record);
            let selected_changed = self.current() != current;
            if selected_changed {
                self.changed_at = Some(Instant::now());
            }
            changed || selected_changed
        } else {
            self.remove_where(|r| match id.as_deref() {
                None => true,
                Some(id) => r.id.as_deref().is_some_and(|other| {
                    other == id
                        || other
                            .strip_prefix(id)
                            .is_some_and(|tail| tail.starts_with('/'))
                }),
            })
        }
    }

    pub fn clear(&mut self) -> bool {
        self.remove_where(|_| true)
    }

    /// Process/output EOF and OSC 133 A expire working/blocked, not done/error.
    pub fn drop_transient(&mut self) -> bool {
        self.remove_where(|r| matches!(r.state, ProgramState::Working | ProgramState::Blocked))
    }

    fn remove_where(&mut self, predicate: impl Fn(&ProgramStatus) -> bool) -> bool {
        let current = self.current();
        let len = self.records.len();
        self.records.retain(|r| !predicate(r));
        let changed = self.records.len() != len;
        if self.current() != current {
            self.changed_at = Some(Instant::now());
        }
        changed
    }

    pub fn memory_bytes(&self) -> usize {
        self.records.capacity() * std::mem::size_of::<ProgramStatus>()
            + self
                .records
                .iter()
                .map(|r| {
                    [&r.id, &r.app, &r.title, &r.msg]
                        .into_iter()
                        .flatten()
                        .map(|s| s.capacity())
                        .sum::<usize>()
                })
                .sum::<usize>()
    }

    /// Prefer blocked > error > working > done > idle; newest wins ties.
    /// Resolve app from the nearest existing ancestor without changing records.
    pub fn current(&self) -> Option<ProgramStatus> {
        let mut record = self
            .records
            .iter()
            .max_by_key(|r| match r.state {
                ProgramState::Blocked => 5,
                ProgramState::Error => 4,
                ProgramState::Working => 3,
                ProgramState::Done => 2,
                ProgramState::Idle => 1,
            })?
            .clone();
        if record.app.is_none() {
            let mut id = record.id.as_deref();
            while let Some(path) = id {
                id = path.rsplit_once('/').map(|(parent, _)| parent);
                if let Some(app) = self
                    .records
                    .iter()
                    .find(|r| r.id.as_deref() == id)
                    .and_then(|r| r.app.as_ref())
                {
                    record.app = Some(app.clone());
                    break;
                }
            }
        }
        Some(record)
    }
}

fn name(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_.+-".contains(&b))
}

fn text(value: Option<&str>, max: usize) -> Option<Option<String>> {
    let Some(value) = value.filter(|v| !v.is_empty()) else {
        return Some(None);
    };
    let bytes = if value.ends_with('=') {
        STANDARD.decode(value)
    } else {
        STANDARD_NO_PAD.decode(value)
    }
    .ok()?;
    if bytes.len() > max {
        return None;
    }
    let text = String::from_utf8(bytes).ok()?;
    if text.chars().any(char::is_control) {
        return None;
    }
    // Disarm invisible formatting/direction overrides in control-plane labels.
    let text = text
        .chars()
        .filter(|c| {
            !matches!(c,
        '\u{061c}' | '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' |
        '\u{2060}'..='\u{206f}' | '\u{feff}')
        })
        .collect();
    Some(Some(text))
}

// None = invalid report. Some((id, None)) = clear; otherwise a replacement.
fn parse(body: &[u8]) -> Option<(Option<String>, Option<ProgramStatus>)> {
    if body.len() > 4088 {
        return None;
    } // 4096 minus OSC header and BEL
    let body = std::str::from_utf8(body).ok()?;
    let (mut state, mut id, mut kind, mut progress, mut app, mut title, mut msg) =
        (None, None, None, None, None, None, None);
    for pair in body.split(':') {
        let Some((key, value)) = pair.split_once('=') else {
            continue;
        };
        let key = key.trim_matches(|c: char| c.is_ascii_whitespace());
        let value = value.trim_matches(|c: char| c.is_ascii_whitespace());
        if key.len() > 16 {
            return None;
        }
        if key.is_empty() || !key.bytes().all(|b| b.is_ascii_lowercase()) {
            continue;
        }
        // Check every occurrence, including ones replaced by later pairs.
        // A malformed id must never fall back to the root (especially clear).
        match key {
            "id" if value.len() > 128
                || value.split('/').count() > 8
                || value.split('/').any(|s| s.len() > 32 || !name(s)) =>
            {
                return None;
            }
            "app" if value.len() > 32 => return None,
            "title" if value.len() > 256 => return None,
            "msg" if value.len() > 2732 => return None,
            _ => {}
        }
        if !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_.,+/=-".contains(&b))
        {
            continue;
        }
        match key {
            "state" => state = Some(value),
            "id" => id = Some(value),
            "app" => app = Some(value),
            "title" => title = text(Some(value), 192)?,
            "msg" => msg = text(Some(value), 2048)?,
            "kind" => kind = Some(value),
            "progress" => progress = Some(value),
            _ => {}
        }
    }
    let id = id.map(str::to_owned);
    let state = match state? {
        "idle" => ProgramState::Idle,
        "working" => ProgramState::Working,
        "done" => ProgramState::Done,
        "blocked" => ProgramState::Blocked,
        "error" => ProgramState::Error,
        "clear" => return Some((id, None)),
        _ => return None,
    };
    let kind = if state == ProgramState::Blocked {
        match kind {
            Some("permission") => Some(BlockedKind::Permission),
            Some("question") => Some(BlockedKind::Question),
            Some("auth") => Some(BlockedKind::Auth),
            _ => None,
        }
    } else {
        None
    };
    let progress = progress
        .filter(|v| !v.is_empty() && v.bytes().all(|b| b.is_ascii_digit()))
        .and_then(|v| v.parse::<u8>().ok())
        .filter(|v| *v <= 100)
        .filter(|_| matches!(state, ProgramState::Working | ProgramState::Blocked));
    Some((
        id.clone(),
        Some(ProgramStatus {
            state,
            id,
            kind,
            progress,
            app: app.filter(|v| name(v)).map(str::to_owned),
            title,
            msg,
        }),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parsing_and_replacement() {
        let mut statuses = ProgramStatuses::default();
        assert!(
            statuses
                .report(b"state=blocked:kind=permission:progress=42:app=claude-code:msg=UGxhbg")
        );
        let r = statuses.current().unwrap();
        assert_eq!(r.kind, Some(BlockedKind::Permission));
        assert_eq!(r.msg.as_deref(), Some("Plan"));
        assert_eq!(r.progress, Some(42));
        let epoch = statuses.changed_at;
        assert!(
            !statuses
                .report(b"state=blocked:kind=permission:progress=42:app=claude-code:msg=UGxhbg")
        );
        assert_eq!(statuses.changed_at, epoch);
        assert!(statuses.report(b"state=done:kind=auth:progress=101"));
        let r = statuses.current().unwrap();
        assert_eq!(r.state, ProgramState::Done);
        assert!(r.msg.is_none() && r.app.is_none() && r.kind.is_none() && r.progress.is_none());
        assert!(statuses.report(b"oops:future=yes: state = idle :state=working:app=b@d:app=ok"));
        assert_eq!(statuses.current().unwrap().app.as_deref(), Some("ok"));
    }

    #[test]
    fn background_progress_does_not_postpone_blocked_notifications() {
        let mut statuses = ProgramStatuses::default();
        statuses.report(b"state=working:app=agent");
        statuses.report(b"state=blocked:id=worker");
        let epoch = statuses.changed_at;
        statuses.report(b"state=working:app=agent:progress=80");
        assert_eq!(statuses.changed_at, epoch);
        assert_eq!(statuses.current().unwrap().state, ProgramState::Blocked);
        statuses.report(b"state=blocked:id=other");
        assert_eq!(statuses.current().unwrap().id.as_deref(), Some("other"));
        statuses.report(b"state=blocked:id=worker");
        assert_eq!(statuses.current().unwrap().id.as_deref(), Some("worker"));
    }

    #[test]
    fn invalid_reports_are_atomic() {
        let mut statuses = ProgramStatuses::default();
        statuses.report(b"state=working");
        for body in [
            "state=sleeping",
            "app=ok",
            "state=clear:id=",
            "state=clear:id=a//b",
            "state=clear:id=b@d",
            "state=done:msg=YQpi:msg=UGxhbg",
            "state=done:title=a:title=UGxhbg",
            "state=clear:id=a/b/c/d/e/f/g/h/i",
            "state=done:msg=YQpi",
            "state=done:msg=wps=",
            "state=done:msg=/w==",
            "state=done:msg=a",
            "state=done:title=QQ=",
            "state=clear:toolongunknownkeyxx=1",
        ] {
            assert!(!statuses.report(body.as_bytes()), "{body}");
            assert_eq!(statuses.current().unwrap().state, ProgramState::Working);
        }
        for body in [
            format!("state=clear:app={}:app=ok", "a".repeat(33)),
            format!("state=clear:id={}", "a".repeat(33)),
            format!("state=clear:msg={}", STANDARD.encode("a".repeat(2049))),
            format!("state=clear:title={}", STANDARD.encode("a".repeat(193))),
        ] {
            assert!(!statuses.report(body.as_bytes()));
        }
    }

    #[test]
    fn unicode_text_is_plain_and_invisible_formatting_is_disarmed() {
        let encoded = STANDARD.encode("安全\u{202e}text\u{2066}");
        let (_, record) = parse(format!("state=done:msg={encoded}").as_bytes()).unwrap();
        assert_eq!(record.unwrap().msg.as_deref(), Some("安全text"));
    }

    #[test]
    fn hierarchy_lifetime_and_capacity() {
        let mut statuses = ProgramStatuses::default();
        statuses.report(b"state=idle:app=root");
        statuses.report(b"state=working:id=build:app=cargo");
        statuses.report(b"state=blocked:id=build/test");
        assert_eq!(statuses.current().unwrap().app.as_deref(), Some("cargo"));
        statuses.report(b"state=error:id=builder");
        statuses.report(b"state=clear:id=build");
        assert_eq!(statuses.records.len(), 2); // must not clear builder
        statuses.report(b"state=done:id=result");
        statuses.report(b"state=blocked:id=worker");
        assert!(statuses.drop_transient());
        assert_eq!(statuses.records.len(), 3); // idle, error, done survive
        assert!(statuses.clear());
        for i in 0..=MAX_RECORDS {
            statuses.report(format!("state=working:id=job{i}").as_bytes());
        }
        assert_eq!(statuses.records.len(), MAX_RECORDS);
        assert_eq!(statuses.records[0].id.as_deref(), Some("job1"));
    }
}
