use std::io;

use interprocess::local_socket::{
    GenericFilePath, GenericNamespaced, ListenerOptions,
    tokio::{Listener, Stream, prelude::*},
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

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
        ListenerOptions::new().name(name).create_tokio()
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
