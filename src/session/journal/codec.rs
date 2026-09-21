//! Typed payload codecs for non-output records (PLAN2 S1.5 step 1).
//!
//! Lifted from `session/journal/mod.rs`. The actual codec helpers
//! (`encode_resize_payload`, `policy_payload`, `parse_policy`,
//! `decode_resize_payload`, `LifecycleCode`, `encode_lifecycle_payload`,
//! `decode_lifecycle_payload`) are byte-identical to the previous inline
//! definitions.

// ---------------------------------------------------------------------------
// Typed payload codecs for non-output records
// ---------------------------------------------------------------------------

/// Resize payload: `rows u16 LE | cols u16 LE`. Geometry is part of the
/// ordered record (ADR-0003): replay applies resizes at their stream
/// position instead of reconstructing them from side channels.
pub fn encode_resize_payload(rows: u16, cols: u16) -> [u8; 4] {
    let mut payload = [0u8; 4];
    payload[0..2].copy_from_slice(&rows.to_le_bytes());
    payload[2..4].copy_from_slice(&cols.to_le_bytes());
    payload
}

/// Policy record codec: one `key=value` line. Keys must be nonempty and
/// free of `=` and newlines; values must be newline-free. That keeps
/// policy payloads line-oriented and grep-able in a hexdump.
pub fn policy_payload(key: &str, value: &str) -> Vec<u8> {
    format!("{key}={value}").into_bytes()
}

/// Inverse of [`policy_payload`]; `None` for malformed payloads.
#[cfg(test)]
pub fn parse_policy(payload: &[u8]) -> Option<(&str, &str)> {
    let text = std::str::from_utf8(payload).ok()?;
    let (key, value) = text.split_once('=')?;
    if key.is_empty() || value.contains('\n') {
        return None;
    }
    Some((key, value))
}

pub fn decode_resize_payload(payload: &[u8]) -> Option<(u16, u16)> {
    if payload.len() != 4 {
        return None;
    }
    let rows = u16::from_le_bytes([payload[0], payload[1]]);
    let cols = u16::from_le_bytes([payload[2], payload[3]]);
    (rows > 0 && cols > 0).then_some((rows, cols))
}

/// Lifecycle facts worth ordering against the output stream. Only terminal
/// transitions and the start fact are journaled; transient states
/// (`running`, `stopping`) are observable from metadata, not stream facts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum LifecycleCode {
    Started = 1,
    Stopped = 2,
    Killed = 3,
    Failed = 4,
    /// The PTY output stream reached its end (EOF, read error or writer
    /// teardown). Process exit and PTY EOF are separate facts (PLAN.md
    /// I10): completion is only journaled after this record.
    OutputClosed = 5,
}

impl LifecycleCode {
    #[cfg(test)]
    fn from_u8(value: u8) -> Option<Self> {
        match value {
            1 => Some(Self::Started),
            2 => Some(Self::Stopped),
            3 => Some(Self::Killed),
            4 => Some(Self::Failed),
            5 => Some(Self::OutputClosed),
            _ => None,
        }
    }
}

/// Lifecycle payload: `code u8 | exit_code i32 LE | detail UTF-8`.
/// `i32::MIN` is the sentinel for "no exit code" so `Some(0)` (a clean
/// exit) stays distinguishable from an absent code.
pub fn encode_lifecycle_payload(
    code: LifecycleCode,
    exit_code: Option<i32>,
    detail: &str,
) -> Vec<u8> {
    let mut payload = Vec::with_capacity(5 + detail.len());
    payload.push(code as u8);
    payload.extend_from_slice(&exit_code.unwrap_or(i32::MIN).to_le_bytes());
    payload.extend_from_slice(detail.as_bytes());
    payload
}

#[cfg(test)]
pub fn decode_lifecycle_payload(payload: &[u8]) -> Option<(LifecycleCode, Option<i32>, &str)> {
    if payload.len() < 5 {
        return None;
    }
    let code = LifecycleCode::from_u8(payload[0])?;
    let raw_exit = i32::from_le_bytes([payload[1], payload[2], payload[3], payload[4]]);
    let exit_code = (raw_exit != i32::MIN).then_some(raw_exit);
    let detail = std::str::from_utf8(&payload[5..]).ok()?;
    Some((code, exit_code, detail))
}
