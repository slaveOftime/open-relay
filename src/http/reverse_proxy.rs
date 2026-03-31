use axum::{
    body::{Body, to_bytes},
    extract::{Request, WebSocketUpgrade, ws::Message as AxumWsMessage},
    http::{HeaderMap, HeaderName, Method, StatusCode},
    response::{IntoResponse, Response},
};
use futures_util::{SinkExt, StreamExt};
use reqwest::Url;
use std::sync::OnceLock;
use tokio::net::TcpStream;
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream, connect_async,
    tungstenite::{self, Message as TungsteniteMessage},
};
use tracing::{error, warn};

const HOP_BY_HOP_HEADERS: [&str; 8] = [
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

static APP_PROXY_CLIENT: OnceLock<reqwest::Client> = OnceLock::new();

pub(super) async fn proxy(
    request: Request,
    ws_upgrade: Option<WebSocketUpgrade>,
    target_urls: &[Url],
) -> Response {
    if target_urls.is_empty() {
        error!("proxied app request was missing upstream targets");
        return StatusCode::BAD_GATEWAY.into_response();
    }

    if is_websocket_upgrade_request(request.headers()) {
        let Some(ws_upgrade) = ws_upgrade else {
            error!("proxied app WebSocket request was missing an upgrade extractor");
            return StatusCode::BAD_REQUEST.into_response();
        };

        return proxy_app_websocket_request(request.headers(), ws_upgrade, target_urls).await;
    }

    proxy_request(request, target_urls).await
}

async fn proxy_request(request: Request, target_urls: &[Url]) -> Response {
    let (parts, body) = request.into_parts();
    let request_method = parts.method;
    let method = match reqwest::Method::from_bytes(request_method.as_str().as_bytes()) {
        Ok(method) => method,
        Err(err) => {
            error!(%err, method = %request_method, "invalid proxied app method");
            return StatusCode::BAD_GATEWAY.into_response();
        }
    };

    let request_headers = parts.headers;
    let forwarded_request_connection_headers = connection_header_tokens(&request_headers);
    let (buffered_body, mut streaming_body) = if target_urls.len() > 1 {
        let buffered_body = match to_bytes(body, usize::MAX).await {
            Ok(bytes) => bytes,
            Err(err) => {
                error!(%err, method = %request_method, "failed to buffer proxied app request body");
                return StatusCode::BAD_GATEWAY.into_response();
            }
        };
        (Some(buffered_body), None)
    } else {
        (None, Some(body))
    };

    let mut last_not_found = None;
    for target_url in target_urls {
        let mut upstream = app_proxy_client().request(method.clone(), target_url.clone());
        for (header_name, value) in &request_headers {
            if should_forward_http_header(header_name, &forwarded_request_connection_headers) {
                upstream = upstream.header(header_name, value.clone());
            }
        }
        if let Some(body) = buffered_body.clone() {
            upstream = upstream.body(body);
        } else if let Some(body) = streaming_body.take() {
            upstream = upstream.body(reqwest::Body::wrap_stream(body.into_data_stream()));
        }

        let upstream = match upstream.send().await {
            Ok(response) => response,
            Err(err) => {
                error!(%err, url = %target_url, "failed to proxy app request");
                return StatusCode::BAD_GATEWAY.into_response();
            }
        };

        let status =
            StatusCode::from_u16(upstream.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
        let mut builder = axum::http::Response::builder().status(status);
        let response_connection_headers = connection_header_tokens(upstream.headers());
        for (header_name, value) in upstream.headers() {
            if should_forward_http_header(header_name, &response_connection_headers) {
                builder = builder.header(header_name, value.clone());
            }
        }

        if status == StatusCode::NOT_FOUND {
            last_not_found = Some(
                builder
                    .body(Body::empty())
                    .unwrap_or_else(|_| StatusCode::NOT_FOUND.into_response()),
            );
            continue;
        }

        if request_method == Method::HEAD {
            return builder
                .body(Body::empty())
                .unwrap_or_else(|_| StatusCode::BAD_GATEWAY.into_response());
        }

        return builder
            .body(Body::from_stream(upstream.bytes_stream()))
            .unwrap_or_else(|_| StatusCode::BAD_GATEWAY.into_response());
    }

    last_not_found.unwrap_or_else(|| StatusCode::BAD_GATEWAY.into_response())
}

async fn proxy_app_websocket_request(
    headers: &HeaderMap,
    ws_upgrade: WebSocketUpgrade,
    target_urls: &[Url],
) -> Response {
    let mut last_not_found = None;

    for target_url in target_urls {
        match connect_app_websocket(headers, target_url).await {
            Ok((upstream_socket, selected_protocol)) => {
                let ws_upgrade = if let Some(protocol) = selected_protocol {
                    ws_upgrade.protocols([protocol])
                } else {
                    ws_upgrade
                };

                return ws_upgrade
                    .on_upgrade(move |socket| bridge_app_websocket(socket, upstream_socket));
            }
            Err(AppWebsocketConnectError::NotFound) => {
                last_not_found = Some(StatusCode::NOT_FOUND.into_response());
            }
            Err(AppWebsocketConnectError::UpstreamStatus(status)) => {
                return status.into_response();
            }
            Err(AppWebsocketConnectError::Connect(err)) => {
                error!(%err, url = %target_url, "failed to proxy app WebSocket request");
                return StatusCode::BAD_GATEWAY.into_response();
            }
        }
    }

    last_not_found.unwrap_or_else(|| StatusCode::BAD_GATEWAY.into_response())
}

async fn connect_app_websocket(
    headers: &HeaderMap,
    target_url: &Url,
) -> Result<ProxyAppWebSocket, AppWebsocketConnectError> {
    let upstream_url = websocket_target_url(target_url)
        .map_err(|err| AppWebsocketConnectError::Connect(err.to_string()))?;

    let mut request = tungstenite::handshake::client::Request::builder()
        .uri(upstream_url.as_str())
        .header("host", websocket_host_header(&upstream_url));
    for (header_name, value) in headers {
        if should_forward_websocket_header(header_name) {
            request = request.header(header_name, value.clone());
        }
    }

    let request = request
        .body(())
        .map_err(|err| AppWebsocketConnectError::Connect(err.to_string()))?;
    match connect_async(request).await {
        Ok((socket, response)) => Ok((
            socket,
            response
                .headers()
                .get("sec-websocket-protocol")
                .and_then(|value| value.to_str().ok())
                .map(str::to_string),
        )),
        Err(tungstenite::Error::Http(response)) => {
            let status =
                StatusCode::from_u16(response.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
            if status == StatusCode::NOT_FOUND {
                Err(AppWebsocketConnectError::NotFound)
            } else {
                Err(AppWebsocketConnectError::UpstreamStatus(status))
            }
        }
        Err(err) => Err(AppWebsocketConnectError::Connect(err.to_string())),
    }
}

async fn bridge_app_websocket(
    socket: axum::extract::ws::WebSocket,
    upstream_socket: WebSocketStream<MaybeTlsStream<TcpStream>>,
) {
    let (mut client_sender, mut client_receiver) = socket.split();
    let (mut upstream_sender, mut upstream_receiver) = upstream_socket.split();

    let client_to_upstream = async {
        while let Some(message) = client_receiver.next().await {
            let message = message.map_err(|err| err.to_string())?;
            match message {
                AxumWsMessage::Text(text) => upstream_sender
                    .send(TungsteniteMessage::Text(text.to_string().into()))
                    .await
                    .map_err(|err| err.to_string())?,
                AxumWsMessage::Binary(data) => upstream_sender
                    .send(TungsteniteMessage::Binary(data))
                    .await
                    .map_err(|err| err.to_string())?,
                AxumWsMessage::Ping(data) => upstream_sender
                    .send(TungsteniteMessage::Ping(data))
                    .await
                    .map_err(|err| err.to_string())?,
                AxumWsMessage::Pong(data) => upstream_sender
                    .send(TungsteniteMessage::Pong(data))
                    .await
                    .map_err(|err| err.to_string())?,
                AxumWsMessage::Close(_) => {
                    let _ = upstream_sender.send(TungsteniteMessage::Close(None)).await;
                    break;
                }
            }
        }

        Ok::<(), String>(())
    };

    let upstream_to_client = async {
        while let Some(message) = upstream_receiver.next().await {
            let message = message.map_err(|err| err.to_string())?;
            match message {
                TungsteniteMessage::Text(text) => client_sender
                    .send(AxumWsMessage::Text(text.to_string().into()))
                    .await
                    .map_err(|err| err.to_string())?,
                TungsteniteMessage::Binary(data) => client_sender
                    .send(AxumWsMessage::Binary(data))
                    .await
                    .map_err(|err| err.to_string())?,
                TungsteniteMessage::Ping(data) => client_sender
                    .send(AxumWsMessage::Ping(data))
                    .await
                    .map_err(|err| err.to_string())?,
                TungsteniteMessage::Pong(data) => client_sender
                    .send(AxumWsMessage::Pong(data))
                    .await
                    .map_err(|err| err.to_string())?,
                TungsteniteMessage::Close(_) => {
                    let _ = client_sender.send(AxumWsMessage::Close(None)).await;
                    break;
                }
                TungsteniteMessage::Frame(_) => {}
            }
        }

        Ok::<(), String>(())
    };

    tokio::select! {
        result = client_to_upstream => {
            if let Err(err) = result {
                warn!(%err, "proxied app WebSocket client stream ended with an error");
            }
        }
        result = upstream_to_client => {
            if let Err(err) = result {
                warn!(%err, "proxied app WebSocket upstream stream ended with an error");
            }
        }
    }
}

type ProxyAppWebSocket = (WebSocketStream<MaybeTlsStream<TcpStream>>, Option<String>);

enum AppWebsocketConnectError {
    NotFound,
    UpstreamStatus(StatusCode),
    Connect(String),
}

fn should_forward_http_header(header_name: &HeaderName, connection_headers: &[String]) -> bool {
    !header_name.as_str().eq_ignore_ascii_case("host")
        && !header_name.as_str().eq_ignore_ascii_case("content-length")
        && !HOP_BY_HOP_HEADERS
            .iter()
            .any(|candidate| header_name.as_str().eq_ignore_ascii_case(candidate))
        && !connection_headers
            .iter()
            .any(|candidate| header_name.as_str().eq_ignore_ascii_case(candidate))
}

fn should_forward_websocket_header(header_name: &HeaderName) -> bool {
    !header_name.as_str().eq_ignore_ascii_case("host")
        && !header_name
            .as_str()
            .eq_ignore_ascii_case("sec-websocket-extensions")
}

fn connection_header_tokens(headers: &HeaderMap) -> Vec<String> {
    headers
        .get_all("connection")
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| value.to_ascii_lowercase())
        .collect()
}

fn is_websocket_upgrade_request(headers: &HeaderMap) -> bool {
    let has_upgrade = headers
        .get("upgrade")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.eq_ignore_ascii_case("websocket"));
    let has_connection = headers
        .get("connection")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value
                .split(',')
                .any(|part| part.trim().eq_ignore_ascii_case("upgrade"))
        });

    has_upgrade
        && has_connection
        && headers.contains_key("sec-websocket-key")
        && headers.contains_key("sec-websocket-version")
}

fn websocket_target_url(target_url: &Url) -> Result<Url, &'static str> {
    let mut upstream_url = target_url.clone();
    let scheme = match target_url.scheme() {
        "http" => "ws",
        "https" => "wss",
        "ws" | "wss" => target_url.scheme(),
        _ => return Err("proxied app WebSocket target must use http, https, ws, or wss"),
    };
    upstream_url
        .set_scheme(scheme)
        .map_err(|_| "failed to convert proxied app WebSocket target scheme")?;
    Ok(upstream_url)
}

fn websocket_host_header(target_url: &Url) -> String {
    let host = match target_url.host_str() {
        Some(host) if host.contains(':') => format!("[{host}]"),
        Some(host) => host.to_string(),
        None => String::from("localhost"),
    };
    match (target_url.scheme(), target_url.port()) {
        ("ws", Some(80)) | ("wss", Some(443)) | (_, None) => host,
        (_, Some(port)) => format!("{host}:{port}"),
    }
}

fn app_proxy_client() -> &'static reqwest::Client {
    APP_PROXY_CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("app proxy client should build")
    })
}

#[cfg(test)]
mod tests {
    use super::{
        connection_header_tokens, is_websocket_upgrade_request, should_forward_http_header,
        websocket_host_header, websocket_target_url,
    };
    use axum::http::{HeaderMap, HeaderName, HeaderValue};
    use reqwest::Url;

    #[test]
    fn connection_header_tokens_collect_named_headers() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "connection",
            HeaderValue::from_static("keep-alive, x-proxy-test"),
        );

        assert_eq!(
            connection_header_tokens(&headers),
            vec!["keep-alive".to_string(), "x-proxy-test".to_string()]
        );
    }

    #[test]
    fn should_forward_http_header_filters_hop_by_hop_headers() {
        let connection_headers = vec!["x-proxy-test".to_string()];

        assert!(should_forward_http_header(
            &HeaderName::from_static("content-type"),
            &connection_headers,
        ));
        assert!(!should_forward_http_header(
            &HeaderName::from_static("connection"),
            &connection_headers,
        ));
        assert!(!should_forward_http_header(
            &HeaderName::from_static("x-proxy-test"),
            &connection_headers,
        ));
    }

    #[test]
    fn websocket_upgrade_detection_requires_upgrade_headers() {
        let mut headers = HeaderMap::new();
        headers.insert("upgrade", HeaderValue::from_static("websocket"));
        headers.insert("connection", HeaderValue::from_static("Upgrade"));
        headers.insert(
            "sec-websocket-key",
            HeaderValue::from_static("dGhlIHNhbXBsZSBub25jZQ=="),
        );
        headers.insert("sec-websocket-version", HeaderValue::from_static("13"));

        assert!(is_websocket_upgrade_request(&headers));

        headers.remove("sec-websocket-key");
        assert!(!is_websocket_upgrade_request(&headers));
    }

    #[test]
    fn websocket_target_url_uses_ws_scheme() {
        let url = websocket_target_url(&Url::parse("https://example.com/app/socket").unwrap())
            .expect("target URL should convert to WebSocket");

        assert_eq!(url.as_str(), "wss://example.com/app/socket");
    }

    #[test]
    fn websocket_host_header_omits_default_port() {
        let host = websocket_host_header(&Url::parse("wss://example.com/socket").unwrap());

        assert_eq!(host, "example.com");
    }
}
