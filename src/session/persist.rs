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
    let mut bytes = Vec::with_capacity((end_offset - from_offset) as usize);
    file.read_to_end(&mut bytes)?;
    Ok((bytes, end_offset))
}

pub fn current_output_offset_by_id(dir: &Path, session_id: &str) -> u64 {
    current_output_offset(&dir.join(session_id))
}
