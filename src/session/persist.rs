//! On-disk session state: the output log, the event log and their offsets.
//!
//! A session directory holds `output.log` (the canonical filtered PTY byte
//! stream), `events.log` (lifecycle and resize records) and the index sidecars
//! maintained by [`super::logs`].

use crate::error::Result;
use std::{
    fs,
    io::{Read, Seek, SeekFrom, Write},
    path::Path,
};

/// Create an empty `output.log` so readers can open it before the child has
/// produced anything.
pub fn create_output_log(dir: &Path) -> Result<()> {
    fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(dir.join("output.log"))?;
    Ok(())
}

/// Append raw PTY bytes to `output.log` in one shot.
///
/// The reader thread uses [`OutputLog`] instead; this is for callers that write
/// a single chunk and do not amortise the open.
#[cfg(test)]
pub fn append_output_raw(dir: &Path, data: &[u8]) -> Result<()> {
    OutputLog::open(dir).append(data)
}

/// A persistent append handle for a session's `output.log`.
///
/// The PTY reader thread writes one chunk per read syscall, so reopening the
/// file each time costs an `open`/`close` pair per chunk — a significant share
/// of the per-chunk budget during a large paste. The handle is opened in append
/// mode and never buffered in user space, so bytes are visible to the replay
/// readers (`read_output_from`) as soon as `append` returns.
pub struct OutputLog {
    file: Option<fs::File>,
    path: std::path::PathBuf,
}

impl OutputLog {
    pub fn open(dir: &Path) -> Self {
        let path = dir.join("output.log");
        let file = fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(&path)
            .ok();
        Self { file, path }
    }

    /// Append one chunk, transparently reopening the file if the handle was
    /// lost (for example because the previous write failed).
    pub fn append(&mut self, data: &[u8]) -> Result<()> {
        if data.is_empty() {
            return Ok(());
        }
        if self.file.is_none() {
            self.file = Some(
                fs::OpenOptions::new()
                    .append(true)
                    .create(true)
                    .open(&self.path)?,
            );
        }
        let file = self.file.as_mut().expect("handle opened above");
        if let Err(err) = file.write_all(data) {
            self.file = None;
            return Err(err.into());
        }
        Ok(())
    }
}

pub fn append_event(dir: &Path, event: &str) -> Result<()> {
    let path = dir.join("events.log");
    let mut file = fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(path)?;
    file.write_all(event.as_bytes())?;
    file.write_all(b"\n")?;
    file.flush()?;
    Ok(())
}

pub fn append_resize_event(dir: &Path, offset: u64, rows: u16, cols: u16) -> Result<()> {
    append_event(
        dir,
        &format!(
            "resize offset={offset} rows={} cols={}",
            rows.max(1),
            cols.max(1)
        ),
    )
}

pub fn current_output_offset(dir: &Path) -> u64 {
    fs::metadata(dir.join("output.log"))
        .map(|meta| meta.len())
        .unwrap_or(0)
}

/// Truncate a session's `output.log` back to zero length.
///
/// The reader thread's [`OutputLog`] handle is opened in append mode, so it
/// keeps writing at the new end-of-file after truncation without needing to
/// be reopened. Callers must also reset the persisted log index (via
/// [`super::logs::discard_persisted_log_index`]) and the runtime byte counters
/// so downstream offsets stay consistent.
pub fn truncate_output_log(dir: &Path) -> Result<()> {
    let path = dir.join("output.log");
    match fs::OpenOptions::new().write(true).open(&path) {
        Ok(file) => {
            file.set_len(0)?;
            Ok(())
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err.into()),
    }
}

pub fn read_output_from(dir: &Path, from_offset: u64) -> Result<(Vec<u8>, u64)> {
    let path = dir.join("output.log");
    let mut file = match fs::File::open(path) {
        Ok(file) => file,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok((Vec::new(), 0)),
        Err(err) => return Err(err.into()),
    };
    let end_offset = file.seek(SeekFrom::End(0))?;
    if from_offset >= end_offset {
        return Ok((Vec::new(), end_offset));
    }
    file.seek(SeekFrom::Start(from_offset))?;
    // Read exactly the range that was committed when the EOF offset was
    // captured. A concurrent append between the seek and the read must not
    // leak into the result: the caller pairs `bytes` with `end_offset` as a
    // replay boundary, so returning more than `end_offset - from_offset`
    // would silently double-deliver those bytes to a subscriber that then
    // resumes from `end_offset` (PLAN.md §2.2 / invariant I2).
    let committed_len = end_offset - from_offset;
    let mut bytes = Vec::with_capacity(committed_len as usize);
    file.take(committed_len).read_to_end(&mut bytes)?;
    Ok((bytes, end_offset))
}

pub fn current_output_offset_by_id(dir: &Path, session_id: &str) -> u64 {
    current_output_offset(&dir.join(session_id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    fn test_dir(name: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("oly_persist_test_{}_{}", name, std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn read_output_from_never_returns_bytes_beyond_the_captured_eof() {
        // Regression: `read_output_from` used to capture EOF and then
        // `read_to_end`, so a concurrent append between the two calls made
        // the payload disagree with the reported end offset (PLAN.md §2.2).
        const PREFIX: usize = 64 * 1024;
        const APPEND_BUDGET: usize = 2 * 1024 * 1024;
        let dir = test_dir("bounded_read");
        let path = dir.join("output.log");
        // A sizeable committed prefix widens the race window enough that the
        // old implementation failed this loop reliably.
        fs::write(&path, vec![b'a'; PREFIX]).unwrap();

        let stop = std::sync::atomic::AtomicBool::new(false);
        std::thread::scope(|scope| {
            let stop_ref = &stop;
            let appender = scope.spawn(move || {
                let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
                let mut appended = 0;
                // Bounded budget: this simulates a busy session, it must
                // never grow the file without limit even if readers stall.
                while appended < APPEND_BUDGET
                    && !stop_ref.load(std::sync::atomic::Ordering::Relaxed)
                {
                    file.write_all(&[b'b'; 256]).unwrap();
                    appended += 256;
                }
            });

            for _ in 0..100 {
                let (bytes, end_offset) = read_output_from(&dir, 0).unwrap();
                assert_eq!(
                    bytes.len() as u64,
                    end_offset,
                    "payload must end exactly at the captured EOF offset"
                );
                // The committed prefix itself is immutable in an append-only
                // log; bytes past it may legitimately include appends that
                // landed before the EOF capture.
                assert!(
                    bytes[..PREFIX.min(bytes.len())]
                        .iter()
                        .all(|byte| *byte == b'a'),
                    "the committed prefix must never change under a reader"
                );
            }
            stop.store(true, std::sync::atomic::Ordering::Relaxed);
            appender.join().unwrap();
        });

        let _ = fs::remove_dir_all(&dir);
    }

    /// M0 reproduction (PLAN.md invariant I3): `truncate_oversized_logs`
    /// resets `output.log` to zero length, so a client cursor captured before
    /// truncation silently aliases the *new* stream. A reader at offset 75
    /// after the file was truncated and rewritten to 50 bytes gets an empty
    /// `Ok` — indistinguishable from "no new output" — and a reader at
    /// offset 10 gets bytes that were never at offset 10 of the stream it
    /// subscribed to. The 1.0 segmented journal must surface
    /// `HistoryExpired` instead; this test pins the desired behaviour and
    /// stays ignored until the journal lands.
    #[test]
    #[ignore = "M0 reproduction (PLAN I3): offset reuse after truncation; fixed by the segmented journal in M1"]
    fn repro_truncated_log_reuses_offsets() {
        let dir = test_dir("offset_reuse");
        append_output_raw(&dir, &[b'x'; 100]).unwrap();
        truncate_output_log(&dir).unwrap();
        append_output_raw(&dir, &[b'y'; 50]).unwrap();

        // Desired: a cursor beyond the rewritten length is an explicit
        // expiry error, not a silent empty success.
        let stale_cursor = read_output_from(&dir, 75);
        assert!(
            stale_cursor.is_err(),
            "a cursor invalidated by retention must fail loudly, got {stale_cursor:?}"
        );

        // Desired: a cursor inside the rewritten range must not alias new
        // bytes onto the old stream position.
        let (bytes, _) = read_output_from(&dir, 10).unwrap();
        assert!(
            bytes.iter().all(|byte| *byte == b'x'),
            "offset 10 of the pre-truncation stream must not return post-truncation bytes"
        );

        let _ = fs::remove_dir_all(&dir);
    }
}
