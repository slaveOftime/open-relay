//! Client-side attach-stream cursor verification (M3-3, ADR-0004).
//!
//! The server guarantees contiguous, in-order stream cursors: the init frame
//! names the snapshot boundary C and every chunk names the offset of its
//! first byte. The client must never apply out-of-order or gapped data
//! silently — a mismatch means the stream is corrupt and the attach aborts
//! loudly instead of rendering a wrong screen (PLAN §7.2, invariant I2).

use crate::error::AppError;

/// Tracks the next expected stream offset for one attach session.
#[derive(Debug, Clone)]
pub(crate) struct StreamCursor {
    expected: u64,
}

impl StreamCursor {
    /// Start tracking at the snapshot boundary delivered by the init frame.
    pub fn new(snapshot_end: u64) -> Self {
        Self {
            expected: snapshot_end,
        }
    }

    /// Verify and apply one chunk: `offset` must be exactly the next expected
    /// offset (no gap, no duplication).
    pub fn accept(&mut self, offset: u64, len: usize) -> Result<(), AppError> {
        if offset != self.expected {
            return Err(AppError::Protocol(format!(
                "attach stream cursor mismatch: expected offset {}, chunk starts at {} \
                 ({}); aborting instead of rendering a corrupt screen",
                self.expected,
                offset,
                if offset < self.expected {
                    "duplicate/overlap"
                } else {
                    "gap"
                }
            )));
        }
        self.expected = offset.saturating_add(len as u64);
        Ok(())
    }

    /// Verify stream completion: the final cursor must match exactly what the
    /// client applied. `final_offset = 0` means the sender did not know the
    /// final cursor (abnormal teardown) and is not checked.
    pub fn finish(&self, final_offset: u64) -> Result<(), AppError> {
        if final_offset != 0 && final_offset != self.expected {
            return Err(AppError::Protocol(format!(
                "attach stream ended at offset {} but {} bytes are unaccounted for \
                 (applied up to {})",
                final_offset,
                final_offset.abs_diff(self.expected),
                self.expected
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contiguous_chunks_advance_the_cursor() {
        let mut cursor = StreamCursor::new(100);
        cursor.accept(100, 10).unwrap();
        cursor.accept(110, 5).unwrap();
        cursor.finish(115).unwrap();
    }

    #[test]
    fn a_gap_is_rejected() {
        let mut cursor = StreamCursor::new(100);
        let err = cursor.accept(105, 10).unwrap_err();
        assert!(err.to_string().contains("gap"), "{err}");
    }

    #[test]
    fn an_overlap_is_rejected() {
        let mut cursor = StreamCursor::new(100);
        cursor.accept(100, 10).unwrap();
        let err = cursor.accept(105, 10).unwrap_err();
        assert!(err.to_string().contains("duplicate/overlap"), "{err}");
    }

    #[test]
    fn a_short_final_cursor_is_rejected() {
        let mut cursor = StreamCursor::new(100);
        cursor.accept(100, 10).unwrap();
        let err = cursor.finish(114).unwrap_err();
        assert!(err.to_string().contains("unaccounted"), "{err}");
        // Zero means "unknown" (abnormal teardown) and passes.
        cursor.finish(0).unwrap();
    }
}
