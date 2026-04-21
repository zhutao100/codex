use crate::message_processor::ConnectionSessionState;
use crate::outgoing_message::ConnectionId;
use crate::outgoing_message::OutgoingEnvelope;
use crate::outgoing_message::OutgoingMessage;
use codex_app_server_protocol::JSONRPCMessage;
use futures::SinkExt;
use futures::StreamExt;
use std::collections::HashMap;
use std::io::ErrorKind;
use std::io::Result as IoResult;
use std::net::SocketAddr;
#[cfg(unix)]
use std::path::Path;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use tokio::io::AsyncBufReadExt;
use tokio::io::AsyncWriteExt;
use tokio::io::BufReader;
use tokio::io::{self};
use tokio::net::TcpListener;
use tokio::net::TcpStream;
#[cfg(unix)]
use tokio::net::UnixListener;
#[cfg(unix)]
use tokio::net::UnixStream;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_tungstenite::accept_async;
use tokio_tungstenite::accept_hdr_async;
use tokio_tungstenite::tungstenite::Message as WebSocketMessage;
use tokio_tungstenite::tungstenite::handshake::server::Request as WebSocketRequest;
use tokio_tungstenite::tungstenite::handshake::server::Response as WebSocketResponse;
use tokio_tungstenite::tungstenite::http::Response as HttpResponse;
use tokio_tungstenite::tungstenite::http::StatusCode;
use tracing::debug;
use tracing::error;
use tracing::info;
use tracing::warn;

/// Size of the bounded channels used to communicate between tasks. The value
/// is a balance between throughput and memory usage - 128 messages should be
/// plenty for an interactive CLI.
pub(crate) const CHANNEL_CAPACITY: usize = 128;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AppServerTransport {
    Stdio,
    WebSocket { bind_address: SocketAddr },
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub enum AppServerTransportParseError {
    UnsupportedListenUrl(String),
    InvalidWebSocketListenUrl(String),
}

impl std::fmt::Display for AppServerTransportParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AppServerTransportParseError::UnsupportedListenUrl(listen_url) => write!(
                f,
                "unsupported --listen URL `{listen_url}`; expected `stdio://` or `ws://IP:PORT`"
            ),
            AppServerTransportParseError::InvalidWebSocketListenUrl(listen_url) => write!(
                f,
                "invalid websocket --listen URL `{listen_url}`; expected `ws://IP:PORT`"
            ),
        }
    }
}

impl std::error::Error for AppServerTransportParseError {}

impl AppServerTransport {
    pub const DEFAULT_LISTEN_URL: &'static str = "stdio://";

    pub fn from_listen_url(listen_url: &str) -> Result<Self, AppServerTransportParseError> {
        if listen_url == Self::DEFAULT_LISTEN_URL {
            return Ok(Self::Stdio);
        }

        if let Some(socket_addr) = listen_url.strip_prefix("ws://") {
            let bind_address = socket_addr.parse::<SocketAddr>().map_err(|_| {
                AppServerTransportParseError::InvalidWebSocketListenUrl(listen_url.to_string())
            })?;
            return Ok(Self::WebSocket { bind_address });
        }

        Err(AppServerTransportParseError::UnsupportedListenUrl(
            listen_url.to_string(),
        ))
    }
}

impl FromStr for AppServerTransport {
    type Err = AppServerTransportParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::from_listen_url(s)
    }
}

#[derive(Debug)]
pub(crate) enum TransportEvent {
    ConnectionOpened {
        connection_id: ConnectionId,
        writer: mpsc::Sender<OutgoingMessage>,
    },
    ConnectionClosed {
        connection_id: ConnectionId,
    },
    IncomingMessage {
        connection_id: ConnectionId,
        message: JSONRPCMessage,
    },
}

pub(crate) struct ConnectionState {
    pub(crate) writer: mpsc::Sender<OutgoingMessage>,
    pub(crate) session: ConnectionSessionState,
}

impl ConnectionState {
    pub(crate) fn new(writer: mpsc::Sender<OutgoingMessage>) -> Self {
        Self {
            writer,
            session: ConnectionSessionState::default(),
        }
    }
}

pub(crate) async fn start_stdio_connection(
    transport_event_tx: mpsc::Sender<TransportEvent>,
    stdio_handles: &mut Vec<JoinHandle<()>>,
) -> IoResult<()> {
    let connection_id = ConnectionId(0);
    let (writer_tx, mut writer_rx) = mpsc::channel::<OutgoingMessage>(CHANNEL_CAPACITY);
    transport_event_tx
        .send(TransportEvent::ConnectionOpened {
            connection_id,
            writer: writer_tx,
        })
        .await
        .map_err(|_| std::io::Error::new(ErrorKind::BrokenPipe, "processor unavailable"))?;

    let transport_event_tx_for_reader = transport_event_tx.clone();
    stdio_handles.push(tokio::spawn(async move {
        let stdin = io::stdin();
        let reader = BufReader::new(stdin);
        let mut lines = reader.lines();

        loop {
            match lines.next_line().await {
                Ok(Some(line)) => {
                    if !forward_incoming_message(
                        &transport_event_tx_for_reader,
                        connection_id,
                        &line,
                    )
                    .await
                    {
                        break;
                    }
                }
                Ok(None) => break,
                Err(err) => {
                    error!("Failed reading stdin: {err}");
                    break;
                }
            }
        }

        let _ = transport_event_tx_for_reader
            .send(TransportEvent::ConnectionClosed { connection_id })
            .await;
        debug!("stdin reader finished (EOF)");
    }));

    stdio_handles.push(tokio::spawn(async move {
        let mut stdout = io::stdout();
        while let Some(outgoing_message) = writer_rx.recv().await {
            let Some(mut json) = serialize_outgoing_message(outgoing_message) else {
                continue;
            };
            json.push('\n');
            if let Err(err) = stdout.write_all(json.as_bytes()).await {
                error!("Failed to write to stdout: {err}");
                break;
            }
        }
        info!("stdout writer exited (channel closed)");
    }));

    Ok(())
}

pub(crate) async fn start_websocket_acceptor(
    bind_address: SocketAddr,
    transport_event_tx: mpsc::Sender<TransportEvent>,
    required_bearer_token: Option<String>,
) -> IoResult<(SocketAddr, JoinHandle<()>)> {
    let listener = TcpListener::bind(bind_address).await?;
    let local_addr = listener.local_addr()?;
    info!("app-server websocket listening on ws://{local_addr}");

    let connection_counter = Arc::new(AtomicU64::new(1));
    let required_bearer_token = required_bearer_token.map(Arc::<str>::from);
    Ok((
        local_addr,
        tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, _peer_addr)) => {
                        let connection_id =
                            ConnectionId(connection_counter.fetch_add(1, Ordering::Relaxed));
                        let transport_event_tx_for_connection = transport_event_tx.clone();
                        let required_bearer_token = required_bearer_token.clone();
                        tokio::spawn(async move {
                            run_websocket_connection(
                                connection_id,
                                stream,
                                transport_event_tx_for_connection,
                                required_bearer_token,
                            )
                            .await;
                        });
                    }
                    Err(err) => {
                        error!("failed to accept websocket connection: {err}");
                    }
                }
            }
        }),
    ))
}

async fn run_websocket_connection(
    connection_id: ConnectionId,
    stream: TcpStream,
    transport_event_tx: mpsc::Sender<TransportEvent>,
    required_bearer_token: Option<Arc<str>>,
) {
    let websocket_stream = if let Some(required_bearer_token) = required_bearer_token {
        let callback = move |request: &WebSocketRequest, response: WebSocketResponse| {
            if websocket_request_is_authorized(request, required_bearer_token.as_ref()) {
                Ok(response)
            } else {
                let mut denied = HttpResponse::new(Some("Unauthorized".to_string()));
                *denied.status_mut() = StatusCode::UNAUTHORIZED;
                Err(denied)
            }
        };

        match accept_hdr_async(stream, callback).await {
            Ok(stream) => stream,
            Err(err) => {
                warn!("failed to complete authenticated websocket handshake: {err}");
                return;
            }
        }
    } else {
        match accept_async(stream).await {
            Ok(stream) => stream,
            Err(err) => {
                warn!("failed to complete websocket handshake: {err}");
                return;
            }
        }
    };

    let (writer_tx, mut writer_rx) = mpsc::channel::<OutgoingMessage>(CHANNEL_CAPACITY);
    if transport_event_tx
        .send(TransportEvent::ConnectionOpened {
            connection_id,
            writer: writer_tx,
        })
        .await
        .is_err()
    {
        return;
    }

    let (mut websocket_writer, mut websocket_reader) = websocket_stream.split();
    loop {
        tokio::select! {
            outgoing_message = writer_rx.recv() => {
                let Some(outgoing_message) = outgoing_message else {
                    break;
                };
                let Some(json) = serialize_outgoing_message(outgoing_message) else {
                    continue;
                };
                if websocket_writer.send(WebSocketMessage::Text(json.into())).await.is_err() {
                    break;
                }
            }
            incoming_message = websocket_reader.next() => {
                match incoming_message {
                    Some(Ok(WebSocketMessage::Text(text))) => {
                        if !forward_incoming_message(&transport_event_tx, connection_id, &text).await {
                            break;
                        }
                    }
                    Some(Ok(WebSocketMessage::Ping(payload))) => {
                        if websocket_writer.send(WebSocketMessage::Pong(payload)).await.is_err() {
                            break;
                        }
                    }
                    Some(Ok(WebSocketMessage::Pong(_))) => {}
                    Some(Ok(WebSocketMessage::Close(_))) | None => break,
                    Some(Ok(WebSocketMessage::Binary(_))) => {
                        warn!("dropping unsupported binary websocket message");
                    }
                    Some(Ok(WebSocketMessage::Frame(_))) => {}
                    Some(Err(err)) => {
                        warn!("websocket receive error: {err}");
                        break;
                    }
                }
            }
        }
    }

    let _ = transport_event_tx
        .send(TransportEvent::ConnectionClosed { connection_id })
        .await;
}

fn websocket_request_is_authorized(request: &WebSocketRequest, expected_token: &str) -> bool {
    if expected_token.is_empty() {
        return true;
    }

    let expected_header_value = format!("Bearer {expected_token}");
    if request
        .headers()
        .get("Authorization")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value == expected_header_value)
    {
        return true;
    }

    let query = request.uri().query().unwrap_or_default();
    for pair in query.split('&') {
        let mut pieces = pair.splitn(2, '=');
        let Some(key) = pieces.next() else {
            continue;
        };
        let Some(value) = pieces.next() else {
            continue;
        };
        if key == "token" && value == expected_token {
            return true;
        }
    }

    false
}

#[cfg(unix)]
pub(crate) async fn start_uds_acceptor(
    socket_path: &Path,
    transport_event_tx: mpsc::Sender<TransportEvent>,
) -> IoResult<JoinHandle<()>> {
    if socket_path.exists() {
        let _ = tokio::fs::remove_file(socket_path).await;
    }

    let listener = UnixListener::bind(socket_path)?;
    info!("app-server uds listening on {}", socket_path.display());

    let connection_counter = Arc::new(AtomicU64::new(1));
    Ok(tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((stream, _peer_addr)) => {
                    let connection_id =
                        ConnectionId(connection_counter.fetch_add(1, Ordering::Relaxed));
                    let transport_event_tx_for_connection = transport_event_tx.clone();
                    tokio::spawn(async move {
                        run_uds_connection(
                            connection_id,
                            stream,
                            transport_event_tx_for_connection,
                        )
                        .await;
                    });
                }
                Err(err) => {
                    error!("failed to accept uds connection: {err}");
                }
            }
        }
    }))
}

#[cfg(unix)]
async fn run_uds_connection(
    connection_id: ConnectionId,
    stream: UnixStream,
    transport_event_tx: mpsc::Sender<TransportEvent>,
) {
    let (writer_tx, mut writer_rx) = mpsc::channel::<OutgoingMessage>(CHANNEL_CAPACITY);
    if transport_event_tx
        .send(TransportEvent::ConnectionOpened {
            connection_id,
            writer: writer_tx,
        })
        .await
        .is_err()
    {
        return;
    }

    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();

    loop {
        tokio::select! {
            outgoing_message = writer_rx.recv() => {
                let Some(outgoing_message) = outgoing_message else {
                    break;
                };
                let Some(mut json) = serialize_outgoing_message(outgoing_message) else {
                    continue;
                };
                json.push('\n');
                if writer.write_all(json.as_bytes()).await.is_err() {
                    break;
                }
            }
            incoming = lines.next_line() => {
                match incoming {
                    Ok(Some(line)) => {
                        if !forward_incoming_message(&transport_event_tx, connection_id, &line).await {
                            break;
                        }
                    }
                    Ok(None) => break,
                    Err(err) => {
                        warn!("uds receive error: {err}");
                        break;
                    }
                }
            }
        }
    }

    let _ = transport_event_tx
        .send(TransportEvent::ConnectionClosed { connection_id })
        .await;
}

async fn forward_incoming_message(
    transport_event_tx: &mpsc::Sender<TransportEvent>,
    connection_id: ConnectionId,
    payload: &str,
) -> bool {
    match serde_json::from_str::<JSONRPCMessage>(payload) {
        Ok(message) => transport_event_tx
            .send(TransportEvent::IncomingMessage {
                connection_id,
                message,
            })
            .await
            .is_ok(),
        Err(err) => {
            error!("Failed to deserialize JSONRPCMessage: {err}");
            true
        }
    }
}

fn serialize_outgoing_message(outgoing_message: OutgoingMessage) -> Option<String> {
    let value = match serde_json::to_value(outgoing_message) {
        Ok(value) => value,
        Err(err) => {
            error!("Failed to convert OutgoingMessage to JSON value: {err}");
            return None;
        }
    };
    match serde_json::to_string(&value) {
        Ok(json) => Some(json),
        Err(err) => {
            error!("Failed to serialize JSONRPCMessage: {err}");
            None
        }
    }
}

pub(crate) async fn route_outgoing_envelope(
    connections: &mut HashMap<ConnectionId, ConnectionState>,
    envelope: OutgoingEnvelope,
) {
    match envelope {
        OutgoingEnvelope::ToConnection {
            connection_id,
            message,
        } => {
            let Some(connection_state) = connections.get(&connection_id) else {
                warn!(
                    "dropping message for disconnected connection: {:?}",
                    connection_id
                );
                return;
            };
            if connection_state.writer.send(message).await.is_err() {
                connections.remove(&connection_id);
            }
        }
        OutgoingEnvelope::Broadcast { message } => {
            let target_connections: Vec<ConnectionId> = connections
                .iter()
                .filter_map(|(connection_id, connection_state)| {
                    if connection_state.session.initialized {
                        Some(*connection_id)
                    } else {
                        None
                    }
                })
                .collect();

            for connection_id in target_connections {
                let Some(connection_state) = connections.get(&connection_id) else {
                    continue;
                };
                if connection_state.writer.send(message.clone()).await.is_err() {
                    connections.remove(&connection_id);
                }
            }
        }
    }
}

pub(crate) fn has_initialized_connections(
    connections: &HashMap<ConnectionId, ConnectionState>,
) -> bool {
    connections
        .values()
        .any(|connection| connection.session.initialized)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use tokio_tungstenite::tungstenite::http::HeaderValue;
    use tokio_tungstenite::tungstenite::http::Request;

    #[test]
    fn app_server_transport_parses_stdio_listen_url() {
        let transport = AppServerTransport::from_listen_url(AppServerTransport::DEFAULT_LISTEN_URL)
            .expect("stdio listen URL should parse");
        assert_eq!(transport, AppServerTransport::Stdio);
    }

    #[test]
    fn app_server_transport_parses_websocket_listen_url() {
        let transport = AppServerTransport::from_listen_url("ws://127.0.0.1:1234")
            .expect("websocket listen URL should parse");
        assert_eq!(
            transport,
            AppServerTransport::WebSocket {
                bind_address: "127.0.0.1:1234".parse().expect("valid socket address"),
            }
        );
    }

    #[test]
    fn app_server_transport_rejects_invalid_websocket_listen_url() {
        let err = AppServerTransport::from_listen_url("ws://localhost:1234")
            .expect_err("hostname bind address should be rejected");
        assert_eq!(
            err.to_string(),
            "invalid websocket --listen URL `ws://localhost:1234`; expected `ws://IP:PORT`"
        );
    }

    #[test]
    fn app_server_transport_rejects_unsupported_listen_url() {
        let err = AppServerTransport::from_listen_url("http://127.0.0.1:1234")
            .expect_err("unsupported scheme should fail");
        assert_eq!(
            err.to_string(),
            "unsupported --listen URL `http://127.0.0.1:1234`; expected `stdio://` or `ws://IP:PORT`"
        );
    }

    #[test]
    fn websocket_auth_accepts_matching_authorization_header() {
        let request = Request::builder()
            .uri("ws://127.0.0.1:8080")
            .header(
                "Authorization",
                HeaderValue::from_static("Bearer secret-token"),
            )
            .body(())
            .expect("request should build");
        assert!(websocket_request_is_authorized(&request, "secret-token"));
    }

    #[test]
    fn websocket_auth_accepts_matching_query_token() {
        let request = Request::builder()
            .uri("ws://127.0.0.1:8080?token=secret-token")
            .body(())
            .expect("request should build");
        assert!(websocket_request_is_authorized(&request, "secret-token"));
    }

    #[test]
    fn websocket_auth_rejects_mismatched_tokens() {
        let request = Request::builder()
            .uri("ws://127.0.0.1:8080?token=wrong")
            .header("Authorization", HeaderValue::from_static("Bearer wrong"))
            .body(())
            .expect("request should build");
        assert!(!websocket_request_is_authorized(&request, "secret-token"));
    }
}
