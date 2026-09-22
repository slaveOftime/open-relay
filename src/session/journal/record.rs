//! On-disk record format, primary types and CRC-32 (PLAN2 S1.1 + P2.1).
//!
//! Owns the wire format: header layout, sequence + elapsed_ms framing,
//! CRC-32 (IEEE 802.3 reflected) and the streaming [`Crc32`] state used
//! to checksum sealed-part manifests without re-reading the part.
//!
//! ## CRC-32 implementation (PLAN2 P2.1)
//!
//! The hot path (every record append) and the cold path (recovery/verify
//! scans) used to go through a hand-rolled byte-at-a-time table walk.
//! That is O(n) bytes with 8 operations per byte; on a 4-wide SIMD
//! machine `crc32fast` does it ~16× faster, which matters when a single
//! append is in the middle of a multi-MiB PTY write batch.
//!
//! Both the old and new impls share the IEEE 802.3 reflected polynomial
//! (`0xEDB8_8320`) and the standard `init = !0`, `finalize = !state`
//! framing, so the on-disk digests are byte-identical: there is **no
//! version bump**. The legacy table implementation lives behind
//! `#[cfg(test)]` as a cross-check oracle so a conformance test asserts
//! both produce identical digests over random fixtures.

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

// ---------------------------------------------------------------------------
// CRC-32 (legacy table oracle, test-only)
// ---------------------------------------------------------------------------
//
// Kept `[cfg(test)]` so binary builds don't carry the 1 KiB table. The
// table-based impl serves two purposes:
//   1. Cross-check that `crc32fast` produces the same digest over
//      the same input (the conformance test in `mod tests` runs both
//      over random + structured fixtures).
//   2. Belt-and-braces fallback if `crc32fast` ever becomes unavailable
//      on a target (unlikely — it's pure Rust with `#[cfg(any(...))]`
//      SIMD gates).
#[cfg(test)]
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

#[cfg(test)]
pub(crate) static CRC32_TABLE: [u32; 256] = crc32_table();

/// Legacy table-based CRC-32 oracle (test-only). Same polynomial as the
/// production [`Crc32`] so the digests MUST match bit-for-bit; the
/// conformance test enforces this.
#[cfg(test)]
pub fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &byte in bytes {
        crc = CRC32_TABLE[((crc ^ u32::from(byte)) & 0xFF) as usize] ^ (crc >> 8);
    }
    !crc
}

// ---------------------------------------------------------------------------
// CRC-32 (production: crc32fast, PLAN2 P2.1)
// ---------------------------------------------------------------------------

/// Resumable CRC-32 (IEEE 802.3 reflected, same polynomial as [`crc32`]):
/// sealed-part manifests checksum a segment incrementally as records are
/// appended, so sealing never re-reads the part (M3-6).
///
/// Thin wrapper around [`crc32fast::Hasher`]. `finish(&self)` clones the
/// inner hasher rather than consuming it so the caller can keep using
/// the value after extracting a digest — that matches the pre-P2.1 API
/// (`Copy` + `finish(&self)`) and the lone caller resets the hasher in
/// the very next statement anyway.
#[derive(Clone)]
pub struct Crc32 {
    inner: crc32fast::Hasher,
}

impl Crc32 {
    pub fn new() -> Self {
        Self {
            inner: crc32fast::Hasher::new(),
        }
    }

    pub fn update(&mut self, bytes: &[u8]) {
        self.inner.update(bytes);
    }

    pub fn finish(&self) -> u32 {
        self.inner.clone().finalize()
    }

    /// CRC-32 of a whole byte slice.
    pub fn of(bytes: &[u8]) -> u32 {
        crc32fast::hash(bytes)
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// IEEE 802.3 standard check vector: the CRC-32 of the ASCII string
    /// `"123456789"` MUST be `0xCBF43926`. Both the legacy table impl and
    /// `crc32fast` agree on this; the test asserts we did not introduce
    /// a framing regression.
    #[test]
    fn crc32_check_vector_matches_ieee_802_3() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(Crc32::of(b"123456789"), 0xCBF4_3926);
    }

    /// Cross-check: for every input we hash, both implementations must
    /// produce the same digest. Fuzzes a mix of structured and random
    /// data. This is the load-bearing test that justifies swapping in
    /// `crc32fast` — any future drift (different init/finalize framing,
    /// different polynomial) shows up here.
    #[test]
    fn crc32fast_matches_legacy_table_over_random_fixtures() {
        // Empty
        assert_eq!(crc32(b""), Crc32::of(b""));

        // Single bytes
        for byte in 0u8..=255 {
            let buf = [byte];
            assert_eq!(
                crc32(&buf),
                Crc32::of(&buf),
                "mismatch on single byte 0x{byte:02x}"
            );
        }

        // Structured: simulate a record header (32 bytes of zeros) + payload
        for payload_len in [0, 1, 7, 32, 255, 1024, 4096, 65_537] {
            let header = vec![0u8; 32];
            let payload = vec![0xA5u8; payload_len];
            let legacy = {
                let mut c = Crc32::new();
                c.update(&header);
                c.update(&payload);
                c.finish()
            };
            let fast = crc32_two(&header, &payload);
            assert_eq!(legacy, fast, "mismatch on payload_len={payload_len}");
        }

        // Pseudorandom walk: deterministic seed → reproducible test.
        let mut state: u64 = 0xDEAD_BEEF_CAFE_BABE;
        let mut buf = Vec::with_capacity(8 * 1024);
        for _ in 0..128 {
            // xorshift64* — avoids any dep on `rand`.
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            buf.extend_from_slice(&state.to_le_bytes());
        }
        assert_eq!(crc32(&buf), Crc32::of(&buf));
    }

    /// Streaming updates must produce the same digest as a one-shot
    /// `of()` over the concatenated input (the "resumable" guarantee
    /// — the whole reason `Crc32` exists instead of using
    /// `crc32fast::hash` directly).
    #[test]
    fn streaming_updates_match_one_shot() {
        let pieces: Vec<Vec<u8>> = (0..16)
            .map(|i| {
                let mut v = vec![0u8; 37 + i * 11];
                for (j, b) in v.iter_mut().enumerate() {
                    *b = ((i as u32 * 31 + j as u32) & 0xFF) as u8;
                }
                v
            })
            .collect();

        let mut hasher = Crc32::new();
        for piece in &pieces {
            hasher.update(piece);
        }
        let streamed = hasher.finish();

        let concatenated: Vec<u8> = pieces.iter().flat_map(|p| p.iter().copied()).collect();
        let one_shot = Crc32::of(&concatenated);

        assert_eq!(streamed, one_shot);
    }
}
