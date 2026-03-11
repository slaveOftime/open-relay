use std::{fs, io::Write, path::Path};

use chrono::{DateTime, Utc};

use crate::error::Result;

#[allow(dead_code)]
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

pub fn format_age(
    created_at: DateTime<Utc>,
    started_at: Option<DateTime<Utc>>,
    ended_at: Option<DateTime<Utc>>,
) -> String {
    let age = match (started_at, ended_at) {
        (Some(started), Some(ended)) => ended - started,
        (Some(started), None) => Utc::now() - started,
        (None, Some(ended)) => ended - created_at,
        (None, None) => Utc::now() - created_at,
    };

    if age.num_hours() > 0 {
        format!("{}h", age.num_hours())
    } else if age.num_minutes() > 0 {
        format!("{}m", age.num_minutes())
    } else {
        format!("{}s", age.num_seconds().max(0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn ts(year: i32, month: u32, day: u32, h: u32, m: u32, s: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(year, month, day, h, m, s).unwrap()
    }

    // -----------------------------------------------------------------------
    // format_age
    // -----------------------------------------------------------------------

    #[test]
    fn test_format_age_hours() {
        let started = ts(2026, 1, 1, 0, 0, 0);
        let ended = ts(2026, 1, 1, 3, 30, 0);
        assert_eq!(format_age(started, Some(started), Some(ended)), "3h");
    }

    #[test]
    fn test_format_age_minutes() {
        let started = ts(2026, 1, 1, 0, 0, 0);
        let ended = ts(2026, 1, 1, 0, 45, 0);
        assert_eq!(format_age(started, Some(started), Some(ended)), "45m");
    }

    #[test]
    fn test_format_age_seconds() {
        let started = ts(2026, 1, 1, 0, 0, 0);
        let ended = ts(2026, 1, 1, 0, 0, 42);
        assert_eq!(format_age(started, Some(started), Some(ended)), "42s");
    }

    #[test]
    fn test_format_age_no_started_uses_created() {
        let created = ts(2026, 1, 1, 0, 0, 0);
        let ended = ts(2026, 1, 1, 0, 5, 0);
        assert_eq!(format_age(created, None, Some(ended)), "5m");
    }

    #[test]
    fn test_format_age_zero_seconds_not_negative() {
        let started = ts(2026, 1, 1, 0, 0, 0);
        let result = format_age(started, Some(started), Some(started));
        assert_eq!(result, "0s");
    }
}
