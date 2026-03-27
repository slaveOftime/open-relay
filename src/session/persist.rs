use super::logs::refresh_persisted_log_index;
use crate::error::Result;
use std::{fs, io::Write, path::Path};

#[allow(dead_code)]
pub fn append_output(dir: &Path, chunk: &str) -> Result<()> {
    let path = dir.join("output.log");
    let mut file = fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(path)?;
    file.write_all(chunk.as_bytes())?;
    file.flush()?;
    refresh_persisted_log_index(dir)?;
    Ok(())
}

/// Write raw PTY bytes directly to `output.log` without any UTF-8 conversion.
/// This is the preferred path for the new byte-oriented ring/log design.
pub fn append_output_raw(dir: &Path, data: &[u8]) -> Result<()> {
    let path = dir.join("output.log");
    let mut file = fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(path)?;
    file.write_all(data)?;
    file.flush()?;
    refresh_persisted_log_index(dir)?;
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

pub fn current_output_offset_by_id(dir: &Path, session_id: &str) -> u64 {
    current_output_offset(&dir.join(session_id))
}
