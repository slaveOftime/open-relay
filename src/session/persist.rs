use crate::error::Result;
use std::{
    fs,
    io::{Read, Seek, SeekFrom, Write},
    path::Path,
};

pub fn append_output(dir: &Path, chunk: &str) -> Result<()> {
    let path = dir.join("output.log");
    let mut file = fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(path)?;
    file.write_all(chunk.as_bytes())?;
    file.flush()?;
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
    let mut bytes = Vec::with_capacity((end_offset - from_offset) as usize);
    file.read_to_end(&mut bytes)?;
    Ok((bytes, end_offset))
}

pub fn current_output_offset_by_id(dir: &Path, session_id: &str) -> u64 {
    current_output_offset(&dir.join(session_id))
}
