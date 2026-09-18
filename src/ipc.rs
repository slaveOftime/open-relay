use std::{io, time::Duration};

use interprocess::local_socket::{
    GenericFilePath, GenericNamespaced, ListenerOptions,
    tokio::{Listener, Stream, prelude::*},
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, ReadHalf, WriteHalf};

use crate::{
    config::AppConfig,
    error::{AppError, Result},
    protocol::{PROTOCOL_VERSION, RpcEnvelope, RpcRequest, RpcResponse},
};

/// Maximum size of a single IPC message line (10 MB).
/// Prevents OOM from malicious clients sending data without a newline.
const MAX_IPC_LINE_BYTES: usize = 10 * 1024 * 1024;

/// Read a newline-terminated line from a buffered reader, returning an error
/// if the accumulated data exceeds [`MAX_IPC_LINE_BYTES`] before a newline is
/// found.  This prevents a malicious local client from exhausting memory by
/// sending an infinitely long line without a terminator.
///
/// Bytes are accumulated raw and decoded once, after the terminator is found.
/// Decoding each `fill_buf` chunk individually would reject payloads whose
/// multi-byte UTF-8 characters straddle a chunk boundary — the socket hands us
/// arbitrary byte counts, so a 3-byte character can easily be split across two
/// reads (e.g. "incomplete utf-8 byte sequence from index 4094").
async fn read_line_bounded<R: tokio::io::AsyncBufRead + Unpin>(
    reader: &mut R,
    buf: &mut String,
) -> io::Result<usize> {
    let mut raw: Vec<u8> = Vec::new();
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            break;
        }
        let newline_pos = available.iter().position(|&b| b == b'\n');
        let used = match newline_pos {
            Some(pos) => pos + 1,
            None => available.len(),
        };
        if raw.len() + used > MAX_IPC_LINE_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("IPC message exceeds {MAX_IPC_LINE_BYTES} byte limit"),
            ));
        }
        raw.extend_from_slice(&available[..used]);
        reader.consume(used);
        if newline_pos.is_some() {
            break;
        }
    }

    let total = raw.len();
    if total == 0 {
        return Ok(0);
    }
    let text = String::from_utf8(raw)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err.utf8_error()))?;
    buf.push_str(&text);
    Ok(total)
}

pub async fn connect(config: &AppConfig) -> Result<Stream> {
    let stream = if GenericNamespaced::is_supported() {
        let name = config
            .socket_name
            .as_str()
            .to_ns_name::<GenericNamespaced>()
            .map_err(AppError::Io)?;
        Stream::connect(name).await
    } else {
        let socket_file = config.socket_file.to_string_lossy().to_string();
        let name = socket_file
            .as_str()
            .to_fs_name::<GenericFilePath>()
            .map_err(AppError::Io)?;
        Stream::connect(name).await
    };

    stream.map_err(|err| AppError::DaemonUnavailable(err.to_string()))
}

pub fn bind(config: &AppConfig) -> io::Result<Listener> {
    if GenericNamespaced::is_supported() {
        let name = config
            .socket_name
            .as_str()
            .to_ns_name::<GenericNamespaced>()?;
        ListenerOptions::new().name(name).create_tokio()
    } else {
        let socket_file = config.socket_file.to_string_lossy().to_string();
        let name = socket_file.as_str().to_fs_name::<GenericFilePath>()?;
        let listener = ListenerOptions::new()
            .name(name)
            .reclaim_name(true)
            .try_overwrite(true)
            .max_spin_time(Duration::from_millis(250))
            .create_tokio()?;

        // Restrict socket file to owner-only access.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::Permissions::from_mode(0o600);
            std::fs::set_permissions(&socket_file, perms)?;
        }

        Ok(listener)
    }
}

pub async fn send_request(config: &AppConfig, request: RpcRequest) -> Result<RpcResponse> {
    let stream = connect(config).await?;
    send_request_on_stream(stream, request).await
}

pub async fn send_request_checked(config: &AppConfig, request: RpcRequest) -> Result<RpcResponse> {
    let response = send_request(config, request).await?;
    ensure_success_response(response)
}

pub async fn send_request_on_stream(
    mut stream: Stream,
    request: RpcRequest,
) -> Result<RpcResponse> {
    let envelope = RpcEnvelope {
        version: PROTOCOL_VERSION,
        payload: request,
    };
    let message = serde_json::to_string(&envelope)?;

    stream.write_all(message.as_bytes()).await?;
    stream.write_all(b"\n").await?;
    stream.flush().await?;

    let mut reader = BufReader::new(&mut stream);
    let mut line = String::new();
    let read = read_line_bounded(&mut reader, &mut line).await?;
    if read == 0 {
        return Err(AppError::Protocol(
            "daemon closed the connection".to_string(),
        ));
    }

    let response: RpcEnvelope<RpcResponse> = serde_json::from_str(line.trim_end())?;
    if response.version != PROTOCOL_VERSION {
        return Err(AppError::Protocol(format!(
            "protocol mismatch: client={}, daemon={}",
            PROTOCOL_VERSION, response.version
        )));
    }

    Ok(response.payload)
}

pub fn ensure_success_response(response: RpcResponse) -> Result<RpcResponse> {
    match response {
        RpcResponse::Error { message } => Err(AppError::RequestFailed(message)),
        other => Ok(other),
    }
}

#[allow(dead_code)]
pub async fn read_request(stream: &mut Stream) -> Result<RpcRequest> {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    let read = read_line_bounded(&mut reader, &mut line).await?;
    if read == 0 {
        return Err(AppError::Protocol(
            "client disconnected before request".to_string(),
        ));
    }

    let envelope: RpcEnvelope<RpcRequest> = serde_json::from_str(line.trim_end())?;
    if envelope.version != PROTOCOL_VERSION {
        return Err(AppError::Protocol(format!(
            "protocol version {} is not supported",
            envelope.version
        )));
    }

    Ok(envelope.payload)
}

#[allow(dead_code)]
pub async fn write_response(stream: &mut Stream, payload: RpcResponse) -> Result<()> {
    let envelope = RpcEnvelope {
        version: PROTOCOL_VERSION,
        payload,
    };
    let message = serde_json::to_string(&envelope)?;
    stream.write_all(message.as_bytes()).await?;
    stream.write_all(b"\n").await?;
    stream.flush().await?;
    Ok(())
}

// ── Streaming-attach split-half helpers ────────────────────────────────────

/// Read a single `RpcRequest` from the read-half of a split stream.
pub async fn read_request_from_reader(
    reader: &mut BufReader<ReadHalf<Stream>>,
) -> Result<RpcRequest> {
    let mut line = String::new();
    let read = read_line_bounded(reader, &mut line).await?;
    if read == 0 {
        return Err(AppError::Protocol("client disconnected".to_string()));
    }
    let envelope: RpcEnvelope<RpcRequest> = serde_json::from_str(line.trim_end())?;
    if envelope.version != PROTOCOL_VERSION {
        return Err(AppError::Protocol(format!(
            "protocol version {} is not supported",
            envelope.version
        )));
    }
    Ok(envelope.payload)
}

/// Serialise one envelope into a newline-terminated frame.
///
/// Building the whole frame in memory first means one `write_all` per frame
/// instead of separate payload/newline writes. On the streaming attach path
/// that halves the syscalls per chunk of PTY output.
fn encode_frame<T: serde::Serialize>(version: u16, payload: T) -> Result<Vec<u8>> {
    let envelope = RpcEnvelope { version, payload };
    let mut frame = serde_json::to_vec(&envelope)?;
    frame.push(b'\n');
    Ok(frame)
}

/// Write a single `RpcResponse` to the write-half of a split stream.
pub async fn write_response_to_writer(
    writer: &mut WriteHalf<Stream>,
    payload: RpcResponse,
) -> Result<()> {
    let frame = encode_frame(PROTOCOL_VERSION, payload)?;
    writer.write_all(&frame).await?;
    writer.flush().await?;
    Ok(())
}

/// Read a single `RpcResponse` from the read-half of a split stream.
pub async fn read_response_from_reader(
    reader: &mut BufReader<ReadHalf<Stream>>,
) -> Result<RpcResponse> {
    let mut line = String::new();
    let read = read_line_bounded(reader, &mut line).await?;
    if read == 0 {
        return Err(AppError::Protocol(
            "daemon closed the connection".to_string(),
        ));
    }
    let envelope: RpcEnvelope<RpcResponse> = serde_json::from_str(line.trim_end())?;
    if envelope.version != PROTOCOL_VERSION {
        return Err(AppError::Protocol(format!(
            "protocol mismatch: client={}, daemon={}",
            PROTOCOL_VERSION, envelope.version
        )));
    }
    Ok(envelope.payload)
}

// ── Binary attach-stream frames (M6-3, ADR-0004) ─────────────────────────
//
// After the `AttachStreamInit` JSON line, the server→client direction of an
// attach stream switches to binary length-delimited frames: PTY output is
// raw bytes, never base64. Client→server stays newline-delimited JSON
// (low-volume control only).
//
//   [u32 LE payload_len][u8 tag][payload]
//   tag 1 = output:  payload = [u64 LE offset][raw bytes]
//   tag 2 = control: payload = bare JSON `RpcResponse` (modes / done /
//                    control-changed / resize-broadcast / error notices)

pub const ATTACH_FRAME_OUTPUT: u8 = 1;
pub const ATTACH_FRAME_CONTROL: u8 = 2;

/// Hard caps checked before any allocation (PLAN §7.4). Output frames carry
/// one canonical chunk (at most 512 KiB by runtime batching); control frames
/// carry small JSON notices only.
const MAX_ATTACH_OUTPUT_FRAME_BYTES: u32 = 1024 * 1024 + 8;
const MAX_ATTACH_CONTROL_FRAME_BYTES: u32 = 64 * 1024;
const ATTACH_FRAME_HEADER_BYTES: usize = 5;

/// One server→client attach-stream frame.
#[derive(Debug)]
pub enum AttachFrame {
    Output { offset: u64, data: Vec<u8> },
    // Boxed: RpcResponse is large and control frames are rare compared to
    // output frames, so keep the enum small.
    Control(Box<RpcResponse>),
}

/// Write one binary output chunk frame (single write: header + payload).
pub async fn write_attach_output_frame<W: AsyncWriteExt + Unpin>(
    writer: &mut W,
    offset: u64,
    data: &[u8],
) -> Result<()> {
    let payload_len = 8 + data.len();
    let mut frame = Vec::with_capacity(ATTACH_FRAME_HEADER_BYTES + payload_len);
    frame.extend_from_slice(&(payload_len as u32).to_le_bytes());
    frame.push(ATTACH_FRAME_OUTPUT);
    frame.extend_from_slice(&offset.to_le_bytes());
    frame.extend_from_slice(data);
    writer.write_all(&frame).await?;
    writer.flush().await?;
    Ok(())
}

/// Write one binary control frame (JSON `RpcResponse` payload).
pub async fn write_attach_control_frame<W: AsyncWriteExt + Unpin>(
    writer: &mut W,
    response: &RpcResponse,
) -> Result<()> {
    let payload = serde_json::to_vec(response)?;
    if payload.len() as u32 > MAX_ATTACH_CONTROL_FRAME_BYTES {
        return Err(AppError::Protocol(format!(
            "attach control frame too large: {} bytes",
            payload.len()
        )));
    }
    let mut frame = Vec::with_capacity(ATTACH_FRAME_HEADER_BYTES + payload.len());
    frame.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    frame.push(ATTACH_FRAME_CONTROL);
    frame.extend_from_slice(&payload);
    writer.write_all(&frame).await?;
    writer.flush().await?;
    Ok(())
}

/// Read one attach-stream frame. Any EOF — clean or mid-frame — surfaces as
/// an error; callers treat it as end-of-stream.
pub async fn read_attach_frame<R: tokio::io::AsyncRead + Unpin>(
    reader: &mut R,
) -> Result<AttachFrame> {
    use tokio::io::AsyncReadExt;

    let mut header = [0u8; ATTACH_FRAME_HEADER_BYTES];
    reader
        .read_exact(&mut header)
        .await
        .map_err(|_| AppError::Protocol("daemon closed the connection".to_string()))?;
    let payload_len = u32::from_le_bytes(header[0..4].try_into().expect("4-byte length"));
    let tag = header[4];
    let cap = match tag {
        ATTACH_FRAME_OUTPUT => MAX_ATTACH_OUTPUT_FRAME_BYTES,
        ATTACH_FRAME_CONTROL => MAX_ATTACH_CONTROL_FRAME_BYTES,
        other => {
            return Err(AppError::Protocol(format!(
                "unknown attach frame tag {other}"
            )));
        }
    };
    if payload_len > cap {
        return Err(AppError::Protocol(format!(
            "attach frame too large: {payload_len} bytes (cap {cap})"
        )));
    }
    let mut payload = vec![0u8; payload_len as usize];
    reader
        .read_exact(&mut payload)
        .await
        .map_err(|_| AppError::Protocol("attach frame truncated by peer".to_string()))?;

    match tag {
        ATTACH_FRAME_OUTPUT => {
            if payload.len() < 8 {
                return Err(AppError::Protocol(
                    "attach output frame too short".to_string(),
                ));
            }
            let offset = u64::from_le_bytes(payload[0..8].try_into().expect("8-byte offset"));
            Ok(AttachFrame::Output {
                offset,
                data: payload.split_off(8),
            })
        }
        ATTACH_FRAME_CONTROL => {
            let response: RpcResponse = serde_json::from_slice(&payload)?;
            Ok(AttachFrame::Control(Box::new(response)))
        }
        other => unreachable!("tag {other} was capped above"),
    }
}

/// Read one attach frame, mapping control-frame errors to request failures
/// (same semantics as [`read_checked_response_from_reader`]).
pub async fn read_checked_attach_frame<R: tokio::io::AsyncRead + Unpin>(
    reader: &mut R,
) -> Result<AttachFrame> {
    match read_attach_frame(reader).await? {
        AttachFrame::Control(resp) => match *resp {
            RpcResponse::Error { message } => Err(AppError::RequestFailed(message)),
            other => Ok(AttachFrame::Control(Box::new(other))),
        },
        frame => Ok(frame),
    }
}

pub async fn read_checked_response_from_reader(
    reader: &mut BufReader<ReadHalf<Stream>>,
) -> Result<RpcResponse> {
    let response = read_response_from_reader(reader).await?;
    ensure_success_response(response)
}

/// Write a single `RpcRequest` to the write-half of a split stream (used by client).
pub async fn write_request_to_writer(
    writer: &mut WriteHalf<Stream>,
    payload: RpcRequest,
) -> Result<()> {
    let frame = encode_frame(PROTOCOL_VERSION, payload)?;
    writer.write_all(&frame).await?;
    writer.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        AttachFrame, MAX_IPC_LINE_BYTES, ensure_success_response, read_attach_frame,
        read_line_bounded, write_attach_control_frame, write_attach_output_frame,
    };
    use crate::{error::AppError, protocol::RpcResponse};
    use tokio::io::{AsyncWriteExt, BufReader};

    /// `BufReader` with a tiny capacity reproduces the socket behaviour of
    /// handing back an arbitrary byte count per `fill_buf`.
    async fn read_line_in_chunks(data: &[u8], capacity: usize) -> std::io::Result<String> {
        let mut reader = BufReader::with_capacity(capacity, data);
        let mut line = String::new();
        read_line_bounded(&mut reader, &mut line).await?;
        Ok(line)
    }

    #[tokio::test]
    async fn read_line_bounded_joins_multibyte_chars_split_across_chunks() {
        // "€" is 3 bytes, so a 4-byte read window splits it in half.
        let payload = "aa€bb ⣿ 中文\n";
        let line = read_line_in_chunks(payload.as_bytes(), 4)
            .await
            .expect("split multi-byte characters must not fail the read");
        assert_eq!(line, payload);
    }

    #[tokio::test]
    async fn read_line_bounded_stops_at_the_first_newline() {
        let line = read_line_in_chunks(b"first\nsecond\n", 4)
            .await
            .expect("read should succeed");
        assert_eq!(line, "first\n");
    }

    #[tokio::test]
    async fn read_line_bounded_returns_partial_data_on_eof() {
        let line = read_line_in_chunks("héllo".as_bytes(), 3)
            .await
            .expect("unterminated input should return what was read");
        assert_eq!(line, "héllo");
    }

    #[tokio::test]
    async fn read_line_bounded_rejects_genuinely_invalid_utf8() {
        let err = read_line_in_chunks(b"ok\xff\xfe\n", 4)
            .await
            .expect_err("invalid UTF-8 must still be rejected");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    #[tokio::test]
    async fn read_line_bounded_enforces_the_line_limit() {
        let oversized = vec![b'a'; MAX_IPC_LINE_BYTES + 16];
        let err = read_line_in_chunks(&oversized, 8192)
            .await
            .expect_err("oversized lines must be rejected");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("exceeds"));
    }

    #[test]
    fn ensure_success_response_preserves_non_error_payloads() {
        let response = ensure_success_response(RpcResponse::Ack).expect("ack should pass through");
        assert!(matches!(response, RpcResponse::Ack));
    }

    #[test]
    fn ensure_success_response_maps_error_payloads_to_request_failed() {
        let err = ensure_success_response(RpcResponse::Error {
            message: "session not running: demo".to_string(),
        })
        .expect_err("error payload should become an application error");

        assert!(matches!(
            err,
            AppError::RequestFailed(ref message) if message == "session not running: demo"
        ));
        assert_eq!(err.to_string(), "session not running: demo");
    }

    /// ADR-0004 acceptance: frames survive arbitrary fragmentation and
    /// coalescing on the wire, and mixed output/control sequences decode in
    /// order with byte-identical payloads.
    #[tokio::test]
    async fn attach_frames_round_trip_through_fragmented_writes() {
        let chunks: Vec<(u64, Vec<u8>)> = (0..6u8)
            .map(|i| (i as u64 * 7, vec![b'0' + i; 1 << (i as usize + 4)]))
            .collect();
        let control = RpcResponse::AttachModeChanged {
            app_cursor_keys: true,
            bracketed_paste_mode: false,
        };

        let (mut tx, rx) = tokio::io::duplex(64 * 1024);
        let control_json = serde_json::to_vec(&control).unwrap();
        let written = chunks.clone();
        let writer = tokio::spawn(async move {
            for (i, (offset, data)) in written.iter().enumerate() {
                // Frame boundaries must not matter: write each frame in
                // odd-sized pieces, then coalesce a control frame with the
                // next output frame in one flush.
                let mut frame = Vec::new();
                frame.extend_from_slice(&((8 + data.len()) as u32).to_le_bytes());
                frame.push(super::ATTACH_FRAME_OUTPUT);
                frame.extend_from_slice(&offset.to_le_bytes());
                frame.extend_from_slice(data);
                let mut pos = 0;
                while pos < frame.len() {
                    let n = (pos % 13 + 1).min(frame.len() - pos);
                    tx.write_all(&frame[pos..pos + n]).await.unwrap();
                    pos += n;
                }
                if i == 2 {
                    let mut ctl = Vec::new();
                    ctl.extend_from_slice(&(control_json.len() as u32).to_le_bytes());
                    ctl.push(super::ATTACH_FRAME_CONTROL);
                    ctl.extend_from_slice(&control_json);
                    tx.write_all(&ctl).await.unwrap();
                }
            }
        });

        let mut reader = BufReader::new(rx);
        let mut seen_control = false;
        for (offset, data) in &chunks {
            match read_attach_frame(&mut reader).await.unwrap() {
                AttachFrame::Output {
                    offset: got_offset,
                    data: got,
                } => {
                    assert_eq!((got_offset, &got), (*offset, data));
                }
                AttachFrame::Control(resp) => {
                    assert!(matches!(
                        *resp,
                        RpcResponse::AttachModeChanged {
                            app_cursor_keys: true,
                            bracketed_paste_mode: false
                        }
                    ));
                    seen_control = true;
                    // Re-read to get the output frame for this iteration.
                    match read_attach_frame(&mut reader).await.unwrap() {
                        AttachFrame::Output {
                            offset: got_offset,
                            data: got,
                        } => assert_eq!((got_offset, &got), (*offset, data)),
                        other => panic!("expected output frame, got {other:?}"),
                    }
                }
            }
        }
        assert!(seen_control);
        writer.await.unwrap();
    }

    /// Oversize or unknown-tag frames are rejected from the header alone —
    /// no payload-sized allocation happens first (PLAN §7.4).
    #[tokio::test]
    async fn attach_frame_caps_are_enforced_before_allocation() {
        let (mut tx, rx) = tokio::io::duplex(1024);
        tx.write_all(&u32::MAX.to_le_bytes()).await.unwrap();
        tx.write_all(&[super::ATTACH_FRAME_OUTPUT]).await.unwrap();
        let mut reader = BufReader::new(rx);
        let err = read_attach_frame(&mut reader).await.unwrap_err();
        assert!(err.to_string().contains("too large"), "{err}");

        let (mut tx, rx) = tokio::io::duplex(1024);
        tx.write_all(&4u32.to_le_bytes()).await.unwrap();
        tx.write_all(&[0xEE]).await.unwrap();
        let mut reader = BufReader::new(rx);
        let err = read_attach_frame(&mut reader).await.unwrap_err();
        assert!(
            err.to_string().contains("unknown attach frame tag"),
            "{err}"
        );
    }

    /// The write helpers emit exactly the frame layout the reader expects.
    #[tokio::test]
    async fn attach_frame_writers_and_reader_agree() {
        let (mut tx, rx) = tokio::io::duplex(64 * 1024);
        write_attach_output_frame(&mut tx, 42, b"hello \x1b[31mred")
            .await
            .unwrap();
        write_attach_control_frame(
            &mut tx,
            &RpcResponse::AttachResized {
                rows: 40,
                cols: 120,
            },
        )
        .await
        .unwrap();
        drop(tx);

        let mut reader = BufReader::new(rx);
        match read_attach_frame(&mut reader).await.unwrap() {
            AttachFrame::Output { offset, data } => {
                assert_eq!(offset, 42);
                assert_eq!(data, b"hello \x1b[31mred");
            }
            other => panic!("expected output frame, got {other:?}"),
        }
        match read_attach_frame(&mut reader).await.unwrap() {
            AttachFrame::Control(resp) => {
                assert!(
                    matches!(*resp, RpcResponse::AttachResized { rows, cols } if (rows, cols) == (40, 120))
                );
            }
            other => panic!("expected control frame, got {other:?}"),
        }
    }
}
