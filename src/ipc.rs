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
        ListenerOptions::new()
            .name(name)
            .reclaim_name(true)
            .try_overwrite(true)
            .max_spin_time(Duration::from_millis(250))
            .create_tokio()
    }
}

pub async fn send_request(config: &AppConfig, request: RpcRequest) -> Result<RpcResponse> {
    let stream = connect(config).await?;
    send_request_on_stream(stream, request).await
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
    let read = reader.read_line(&mut line).await?;
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

#[allow(dead_code)]
pub async fn read_request(stream: &mut Stream) -> Result<RpcRequest> {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    let read = reader.read_line(&mut line).await?;
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
    let read = reader.read_line(&mut line).await?;
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

/// Write a single `RpcResponse` to the write-half of a split stream.
pub async fn write_response_to_writer(
    writer: &mut WriteHalf<Stream>,
    payload: RpcResponse,
) -> Result<()> {
    let envelope = RpcEnvelope {
        version: PROTOCOL_VERSION,
        payload,
    };
    let message = serde_json::to_string(&envelope)?;
    writer.write_all(message.as_bytes()).await?;
    writer.write_all(b"\n").await?;
    writer.flush().await?;
    Ok(())
}

/// Read a single `RpcResponse` from the read-half of a split stream.
pub async fn read_response_from_reader(
    reader: &mut BufReader<ReadHalf<Stream>>,
) -> Result<RpcResponse> {
    let mut line = String::new();
    let read = reader.read_line(&mut line).await?;
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

/// Write a single `RpcRequest` to the write-half of a split stream (used by client).
pub async fn write_request_to_writer(
    writer: &mut WriteHalf<Stream>,
    payload: RpcRequest,
) -> Result<()> {
    let envelope = RpcEnvelope {
        version: PROTOCOL_VERSION,
        payload,
    };
    let message = serde_json::to_string(&envelope)?;
    writer.write_all(message.as_bytes()).await?;
    writer.write_all(b"\n").await?;
    writer.flush().await?;
    Ok(())
}
