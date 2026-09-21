//! Sealed-part manifest (M3-6). PLAN2 S1.5 step 2.
//!
//! Lifted from `session/journal/mod.rs`. The on-disk format
//! (`SegmentManifestEntry`, `RetiredIncarnation`, `ManifestLine`,
//! `read_manifest*`, `verify_manifest`, `append_manifest_line`) is
//! byte-identical to the previous inline definitions.

use std::{fs, io, io::Write, path::Path};

use super::{Crc32, MANIFEST_FILE_NAME, list_segments, segment_path};

// ---------------------------------------------------------------------------
// Sealed-part manifest (M3-6)
// ---------------------------------------------------------------------------

/// One sealed segment part, checksummed at seal time. JSON-line in
/// `journal/manifest.log`; readers/compaction verify a part against its
/// entry before trusting or dropping it (I3/I8).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SegmentManifestEntry {
    pub incarnation: u64,
    pub part: u64,
    pub first_seq: u64,
    pub last_seq: u64,
    /// Exact byte length of the sealed part file.
    pub bytes: u64,
    /// CRC-32 (IEEE) of the whole part file, computed incrementally as
    /// records were appended.
    pub crc32: u32,
}

/// Retention tombstone (post-review corrective increment): written to the
/// manifest BEFORE an incarnation's parts are deleted, so intentional
/// deletion is distinguishable from corruption. A crash between the
/// tombstone and the last unlink leaves leftover part files, which
/// [`open`] finishes deleting and [`verify_manifest`] reports until then.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RetiredIncarnation {
    /// Incarnation whose parts checkpoint-gated retention removed.
    pub retired: u64,
}

/// One line of `manifest.log`: either a sealed-part entry or a retention
/// tombstone. Sealed entries keep their original bare-JSON shape, so
/// journals written before tombstones existed still parse.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(untagged)]
pub enum ManifestLine {
    Sealed(SegmentManifestEntry),
    Retired(RetiredIncarnation),
}

impl SegmentManifestEntry {
    /// Append this entry to `journal_dir/manifest.log` and fsync it.
    pub(crate) fn append_to(&self, journal_dir: &Path) -> io::Result<()> {
        append_manifest_line(journal_dir, &ManifestLine::Sealed(*self))
    }
}

impl RetiredIncarnation {
    pub(crate) fn append_to(&self, journal_dir: &Path) -> io::Result<()> {
        append_manifest_line(journal_dir, &ManifestLine::Retired(*self))
    }
}

fn append_manifest_line(journal_dir: &Path, line: &ManifestLine) -> io::Result<()> {
    let mut bytes =
        serde_json::to_vec(line).map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
    bytes.push(b'\n');
    let mut manifest = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(journal_dir.join(MANIFEST_FILE_NAME))?;
    manifest.write_all(&bytes)?;
    manifest.sync_data()
}

/// Read every manifest line, in file order. A malformed line fails the
/// whole read: the manifest is written by us and never edited, so
/// corruption must surface, not be skipped.
pub fn read_manifest_lines(journal_dir: &Path) -> io::Result<Vec<ManifestLine>> {
    let path = journal_dir.join(MANIFEST_FILE_NAME);
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err),
    };
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            serde_json::from_str(line).map_err(|err| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("{}: malformed manifest line: {err}", path.display()),
                )
            })
        })
        .collect()
}

/// Read every sealed-part manifest entry (retention tombstones filtered
/// out), in file order.
pub fn read_manifest(journal_dir: &Path) -> io::Result<Vec<SegmentManifestEntry>> {
    Ok(read_manifest_lines(journal_dir)?
        .into_iter()
        .filter_map(|line| match line {
            ManifestLine::Sealed(entry) => Some(entry),
            ManifestLine::Retired(_) => None,
        })
        .collect())
}

/// Incarnations retired by checkpoint-gated retention, per the manifest.
pub(crate) fn retired_incarnations(
    journal_dir: &Path,
) -> io::Result<std::collections::HashSet<u64>> {
    Ok(read_manifest_lines(journal_dir)?
        .into_iter()
        .filter_map(|line| match line {
            ManifestLine::Retired(retired) => Some(retired.retired),
            ManifestLine::Sealed(_) => None,
        })
        .collect())
}

/// Verify the journal's segments against the manifest. Returns one issue
/// per mismatch; an empty vec means the journal is intact. Checks:
///
/// - every non-retired sealed entry's part exists with its exact length
///   and CRC-32;
/// - no duplicate sealed entries;
/// - every part file on disk is either manifested or the active tail (the
///   newest part of the newest incarnation — being written by a live
///   daemon, or the not-yet-recovered tail after a crash). Anything else
///   is a part the appender never committed, reported instead of guessed;
/// - a retired (tombstoned) incarnation has no leftover part files —
///   leftovers mean a crash interrupted retention (`open` finishes the
///   deletion, so this only fires when doctor runs before any reopen).
///
/// This reads every sealed part in full — call it from integrity
/// tooling/compaction, never from the daemon hot path.
#[allow(dead_code)] // Reserved for offline integrity tooling; exercised by journal tests.
pub fn verify_manifest(journal_dir: &Path) -> Vec<String> {
    let mut issues = Vec::new();
    let lines = match read_manifest_lines(journal_dir) {
        Ok(lines) => lines,
        Err(err) => {
            issues.push(format!(
                "{}: {err}",
                journal_dir.join(MANIFEST_FILE_NAME).display()
            ));
            return issues;
        }
    };
    let mut seen: std::collections::HashSet<(u64, u64)> = std::collections::HashSet::new();
    let mut retired: std::collections::HashSet<u64> = std::collections::HashSet::new();
    let mut sealed = Vec::new();
    for line in lines {
        match line {
            ManifestLine::Sealed(entry) => {
                let key = (entry.incarnation, entry.part);
                if !seen.insert(key) {
                    issues.push(format!(
                        "manifest: duplicate entry for incarnation {} part {}",
                        entry.incarnation, entry.part
                    ));
                    continue;
                }
                sealed.push(entry);
            }
            ManifestLine::Retired(tombstone) => {
                retired.insert(tombstone.retired);
            }
        }
    }
    for entry in sealed
        .into_iter()
        .filter(|e| !retired.contains(&e.incarnation))
    {
        let path = segment_path(journal_dir, entry.incarnation, entry.part);
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(err) => {
                issues.push(format!("{}: sealed part unreadable: {err}", path.display()));
                continue;
            }
        };
        if bytes.len() as u64 != entry.bytes {
            issues.push(format!(
                "{}: sealed part is {} bytes, manifest says {}",
                path.display(),
                bytes.len(),
                entry.bytes
            ));
            continue;
        }
        let crc = Crc32::of(&bytes);
        if crc != entry.crc32 {
            issues.push(format!(
                "{}: sealed part CRC-32 mismatch (manifest {:08x}, actual {:08x})",
                path.display(),
                entry.crc32,
                crc
            ));
        }
    }
    // Orphan / incomplete-retention scan over what is actually on disk.
    match list_segments(journal_dir) {
        Ok(segments) => {
            let active_tail = segments.last().copied();
            for (incarnation, part) in segments {
                if retired.contains(&incarnation) {
                    issues.push(format!(
                        "{}: retention tombstoned incarnation {incarnation} but part files remain on disk",
                        segment_path(journal_dir, incarnation, part).display()
                    ));
                } else if !seen.contains(&(incarnation, part))
                    && Some((incarnation, part)) != active_tail
                {
                    issues.push(format!(
                        "{}: segment part has no manifest entry (not the active tail)",
                        segment_path(journal_dir, incarnation, part).display()
                    ));
                }
            }
        }
        Err(err) => {
            issues.push(format!("{}: {err}", journal_dir.display()));
        }
    }
    issues
}
