//! Shadow journal: bundles the sequencing core with the appender for
//! the M1 shadow wiring (PLAN2 S1.5 step 8).
//!
//! Lifted from `session/journal/mod.rs`. The `ShadowJournal` type, its
//! four impl blocks (drain/test, open/options, shutdown/test, public
//! façade), are byte-identical to the previous inline definitions.

use std::{io, path::Path};

use super::{
    Checkpoint, DEFAULT_SEGMENT_MAX_BYTES, DEFAULT_SYNC_INTERVAL, JournalAck, JournalAppender,
    JournalCursor, JournalSubmitError, LifecycleCode, RecordKind, RecoveryReport, SequencerCore,
    encode_checkpoint, encode_lifecycle_payload, encode_resize_payload, policy_payload,
};

/// M1 shadow journal: bundles the sequencing core with the appender for
/// the shadow wiring behind [`shadow_enabled`].
pub struct ShadowJournal {
    pub core: SequencerCore,
    pub appender: JournalAppender,
    pub acks: std::sync::mpsc::Receiver<JournalAck>,
}

impl ShadowJournal {
    /// Drain queued records and stop the appender (sealing the tail part
    /// into the manifest, M3-6).
    #[cfg(test)]
    pub fn shutdown(&self) {
        self.appender.shutdown();
    }
}

impl ShadowJournal {
    pub fn open(session_dir: &Path) -> io::Result<(Self, u64, Option<RecoveryReport>)> {
        Self::open_with_sync_interval(session_dir, DEFAULT_SYNC_INTERVAL)
    }

    pub fn open_with_sync_interval(
        session_dir: &Path,
        sync_interval: std::time::Duration,
    ) -> io::Result<(Self, u64, Option<RecoveryReport>)> {
        Self::open_with_options(session_dir, sync_interval, DEFAULT_SEGMENT_MAX_BYTES)
    }

    /// Open with explicit sync cadence and segment-part size (tests use
    /// tiny part sizes to exercise rollover).
    pub fn open_with_options(
        session_dir: &Path,
        sync_interval: std::time::Duration,
        max_part_bytes: u64,
    ) -> io::Result<(Self, u64, Option<RecoveryReport>)> {
        let (appender, incarnation, report, acks) =
            JournalAppender::spawn_with_options(session_dir, sync_interval, max_part_bytes)?;
        Ok((
            Self {
                core: SequencerCore::new(incarnation),
                appender,
                acks,
            },
            incarnation,
            report,
        ))
    }

    /// Sequence an event, retain it in the recent cache and submit it to
    /// the appender. Returns the assigned cursor; a submission failure
    /// degrades the core and returns the error for the caller to log.
    pub fn record(
        &mut self,
        kind: RecordKind,
        payload: bytes::Bytes,
    ) -> Result<JournalCursor, JournalSubmitError> {
        self.poll_acks();
        // Once persistence has failed, stop publishing: caching further
        // events that can never be journaled would let a disk stall grow
        // memory indefinitely. The degrade point is the explicit
        // incomplete-capture boundary (I8/I10).
        if self.core.is_degraded() {
            return Err(JournalSubmitError::PersistenceDegraded);
        }
        let event = self.core.publish(kind, payload);
        let cursor = event.cursor;
        if let Err(err) = self.appender.try_submit(event) {
            self.core.degrade(err.to_string());
            return Err(err);
        }
        Ok(cursor)
    }

    pub fn record_output(
        &mut self,
        payload: bytes::Bytes,
    ) -> Result<JournalCursor, JournalSubmitError> {
        self.record(RecordKind::Output, payload)
    }

    pub fn record_resize(
        &mut self,
        rows: u16,
        cols: u16,
    ) -> Result<JournalCursor, JournalSubmitError> {
        self.record(
            RecordKind::Resize,
            bytes::Bytes::copy_from_slice(&encode_resize_payload(rows, cols)),
        )
    }

    /// Record a checkpoint anchoring this stream position: restore and
    /// retention may both start from it (PLAN §5.3).
    pub fn record_checkpoint(
        &mut self,
        checkpoint: &Checkpoint,
    ) -> Result<JournalCursor, JournalSubmitError> {
        self.record(RecordKind::CheckpointRef, encode_checkpoint(checkpoint))
    }

    /// Record a terminal-relevant revision (e.g. a mode flip) at its
    /// ordered stream position, right after the output that caused it.
    pub fn record_policy(
        &mut self,
        key: &str,
        value: &str,
    ) -> Result<JournalCursor, JournalSubmitError> {
        if key.is_empty() || key.contains(['=', '\n']) || value.contains('\n') {
            return Err(JournalSubmitError::InvalidEvent(format!(
                "invalid policy key/value: {key:?}"
            )));
        }
        self.record(
            RecordKind::Policy,
            bytes::Bytes::from(policy_payload(key, value)),
        )
    }

    pub fn record_lifecycle(
        &mut self,
        code: LifecycleCode,
        exit_code: Option<i32>,
        detail: &str,
    ) -> Result<JournalCursor, JournalSubmitError> {
        self.record(
            RecordKind::Lifecycle,
            bytes::Bytes::from(encode_lifecycle_payload(code, exit_code, detail)),
        )
    }

    /// Drain pending acknowledgements into the cursors.
    pub fn poll_acks(&mut self) {
        while let Ok(ack) = self.acks.try_recv() {
            match ack {
                JournalAck::Journaled(seq) => self.core.note_journaled(seq),
                JournalAck::Durable(seq) => self.core.note_durable(seq),
                JournalAck::Failed(reason) => self.core.degrade(reason),
            }
        }
    }

    /// Request a group sync (durability cadence; see PLAN.md §4.2).
    pub fn request_sync(&self) {
        self.appender.request_sync();
    }
}
