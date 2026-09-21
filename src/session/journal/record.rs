//! On-disk record format, primary types and CRC-32 (PLAN2 S1.1).
//!
//! Owns the wire format: header layout, sequence + elapsed_ms framing,
//! CRC-32 (IEEE 802.3 reflected — hand-rolled to avoid a new dependency,
//! verified against the standard check vector) and the streaming
//! [`Crc32`] state used to checksum sealed-part manifests without
//! re-reading the part.

use std::io;

pub(crate) const RECORD_MAGIC: &[u8; 4] = b"OJRN";
pub(crate) const RECORD_VERSION: u16 = 1;
/// magic + version + kind + flags + seq + elapsed_ms + payload_len + crc32.
pub(crate) const HEADER_LEN: usize = 4 + 2 + 2 + 4 + 8 + 8 + 4 + 4;
/// Refuse absurd length fields before allocating (PLAN.md §7.4: limits are
/// checked before allocation).
pub(crate) const MAX_PAYLOAD_LEN: u32 = 64 * 1024 * 1024;

/// Typed journal record kinds. Unknown kinds are unrecoverable corruption for
/// this format version: a torn or aliased tail must stop the scan, never be
/// silently skipped (I3 — missing history is explicit, not empty success).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub enum RecordKind {
    /// Original PTY output bytes.
    Output = 1,
    /// A successful PTY resize barrier: `rows(u16) cols(u16)` payload.
    Resize = 2,
    /// Lifecycle transition (spawned/completed/capture state), UTF-8 payload.
    Lifecycle = 3,
    /// Points at the checkpoint that reconstructs state at `seq`.
    CheckpointRef = 4,
    /// Terminal profile/policy facts needed to interpret the stream.
    Policy = 5,
}

impl RecordKind {
    pub(crate) fn from_u16(value: u16) -> Option<Self> {
        match value {
            1 => Some(Self::Output),
            2 => Some(Self::Resize),
            3 => Some(Self::Lifecycle),
            4 => Some(Self::CheckpointRef),
            5 => Some(Self::Policy),
            _ => None,
        }
    }
}

/// One decoded journal record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    pub kind: RecordKind,
    pub seq: u64,
    /// Milliseconds since the writer's start (monotonic, per incarnation).
    pub elapsed_ms: u64,
    pub payload: Vec<u8>,
}

const fn crc32_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut crc = i as u32;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
            bit += 1;
        }
        table[i] = crc;
        i += 1;
    }
    table
}

pub(crate) static CRC32_TABLE: [u32; 256] = crc32_table();

#[cfg(test)]
pub fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &byte in bytes {
        crc = CRC32_TABLE[((crc ^ u32::from(byte)) & 0xFF) as usize] ^ (crc >> 8);
    }
    !crc
}

/// Resumable CRC-32 (same polynomial/table as [`crc32`]): sealed-part
/// manifests checksum a segment incrementally as records are appended, so
/// sealing never re-reads the part (M3-6).
#[derive(Clone, Copy)]
pub struct Crc32 {
    state: u32,
}

impl Crc32 {
    pub fn new() -> Self {
        Self { state: !0 }
    }

    pub fn update(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.state =
                CRC32_TABLE[((self.state ^ u32::from(byte)) & 0xFF) as usize] ^ (self.state >> 8);
        }
    }

    pub fn finish(&self) -> u32 {
        !self.state
    }

    /// CRC-32 of a whole byte slice.
    pub fn of(bytes: &[u8]) -> u32 {
        let mut crc = Self::new();
        crc.update(bytes);
        crc.finish()
    }
}

impl Default for Crc32 {
    fn default() -> Self {
        Self::new()
    }
}

/// Encode one record header (including its CRC) for `payload`.
pub(crate) fn encode_record_header(
    kind: RecordKind,
    seq: u64,
    elapsed_ms: u64,
    payload: &[u8],
) -> io::Result<[u8; HEADER_LEN]> {
    let mut header = [0u8; HEADER_LEN];
    header[0..4].copy_from_slice(RECORD_MAGIC);
    header[4..6].copy_from_slice(&RECORD_VERSION.to_le_bytes());
    header[6..8].copy_from_slice(&(kind as u16).to_le_bytes());
    // flags [8..12] stay zero until a feature needs them.
    header[12..20].copy_from_slice(&seq.to_le_bytes());
    header[20..28].copy_from_slice(&elapsed_ms.to_le_bytes());
    header[28..32].copy_from_slice(&(payload.len() as u32).to_le_bytes());
    let crc = crc32_two(&header[..32], payload);
    header[32..36].copy_from_slice(&crc.to_le_bytes());
    Ok(header)
}

pub(crate) fn crc32_two(first: &[u8], second: &[u8]) -> u32 {
    let mut crc = Crc32::new();
    crc.update(first);
    crc.update(second);
    crc.finish()
}
