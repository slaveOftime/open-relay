//! On-disk session state helpers: the event log and legacy output-log reads.
//!
//! Since M3-1 the journal is the canonical persisted stream; `output.log`
//! remains only as a legacy fallback for sessions created before the journal
//! became always-on (retired fully in M6). `events.log` (lifecycle and
//! resize records) is still written and read for resize replays.

use crate::error::Result;
use std::{
    fs,
    io::{Read, Seek, SeekFrom, Write},
    path::Path,
};

/// Append raw PTY bytes to `output.log` in one shot.
///
/// The reader thread uses [`OutputLog`] instead; this is for callers that write
/// a single chunk and do not amortise the open.
#[cfg(test)]
pub fn append_output_raw(dir: &Path, data: &[u8]) -> Result<()> {
    let path = dir.join("output.log");
    let mut file = fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(path)?;
    use std::io::Write;
    file.write_all(data)?;
    Ok(())
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
}
