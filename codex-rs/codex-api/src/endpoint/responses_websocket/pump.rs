use crate::error::ApiError;
use bytes::Bytes;
use futures::Sink;
use futures::SinkExt;
use futures::Stream;
use futures::StreamExt;
use std::future::poll_fn;
use std::task::Context;
use std::task::Poll;
use std::task::ready;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio::time::Instant;
use tungstenite::Error as WsError;
use tungstenite::Message;
use tungstenite::Utf8Bytes;
use tungstenite::protocol::frame::Frame;
use tungstenite::protocol::frame::coding::Data;
use tungstenite::protocol::frame::coding::OpCode;

pub(super) struct WsStream {
    tx_command: mpsc::Sender<WsCommand>,
    rx_message: mpsc::UnboundedReceiver<Result<Message, WsError>>,
    pump_task: tokio::task::JoinHandle<()>,
}

struct WsCommand {
    deadline: Instant,
    request: Utf8Bytes,
    tx_result: oneshot::Sender<WsSendOutcome>,
}

fn send_error(error: WsError) -> ApiError {
    ApiError::Stream(format!("failed to send websocket request: {error}"))
}

pub(super) enum WsSendOutcome {
    Flushed,
    NotStarted(ApiError),
    Interrupted(ApiError),
}

const FRAGMENT_SIZE: usize = 1024 * 1024;
const RECEIVE_BATCH_SIZE: usize = 32;
const UPLOAD_READ_AHEAD_BYTES: usize = 1024 * 1024;

fn request_frames(text: Utf8Bytes) -> impl Iterator<Item = Message> {
    let (message, mut data) = if text.len() <= FRAGMENT_SIZE {
        (Some(Message::Text(text)), Bytes::new())
    } else {
        (None, Bytes::from(text))
    };
    let mut opcode = Data::Text;
    message.into_iter().chain(std::iter::from_fn(move || {
        if data.is_empty() {
            return None;
        }
        let payload = data.split_to(data.len().min(FRAGMENT_SIZE));
        let frame = Frame::message(payload, OpCode::Data(opcode), data.is_empty());
        opcode = Data::Continue;
        Some(Message::Frame(frame))
    }))
}

/// Drives the socket while waiting for commands and while flushing their frames.
struct WsPump<S> {
    socket: S,
    messages: mpsc::UnboundedSender<Result<Message, WsError>>,
    read_budget: Option<usize>,
    queued_pongs: usize,
}

impl<S> WsPump<S>
where
    S: Stream<Item = Result<Message, WsError>> + Sink<Message, Error = WsError> + Unpin,
{
    fn new(socket: S, messages: mpsc::UnboundedSender<Result<Message, WsError>>) -> Self {
        Self {
            socket,
            messages,
            read_budget: None,
            queued_pongs: 0,
        }
    }

    fn poll_incoming(
        &mut self,
        cx: &mut Context<'_>,
        recovering: bool,
    ) -> Poll<Result<(), WsError>> {
        if self.queued_pongs != 0 && !recovering && self.socket.poll_flush_unpin(cx)?.is_ready() {
            self.queued_pongs = 0;
        }
        for _ in 0..RECEIVE_BATCH_SIZE {
            if self.read_budget == Some(0) {
                return Poll::Ready(Ok(()));
            }
            if !recovering && self.queued_pongs >= RECEIVE_BATCH_SIZE {
                return Poll::Pending;
            }
            let next = self.socket.poll_next_unpin(cx);
            if recovering && next.is_pending() {
                return Poll::Ready(Ok(()));
            }
            let Some(message) = ready!(next).transpose()? else {
                return Poll::Ready(Err(WsError::ConnectionClosed));
            };
            if let Some(budget) = &mut self.read_budget
                && (recovering || !matches!(message, Message::Ping(_) | Message::Pong(_)))
            {
                *budget = budget.saturating_sub(message.len() + std::mem::size_of::<Message>());
            }
            match message {
                Message::Ping(_) => {
                    // Queue the automatic Pong before starting the next data frame.
                    match self.socket.poll_flush_unpin(cx) {
                        Poll::Ready(Ok(())) => self.queued_pongs = 0,
                        Poll::Ready(Err(error)) if !recovering => return Poll::Ready(Err(error)),
                        Poll::Pending | Poll::Ready(Err(_)) => self.queued_pongs += 1,
                    }
                }
                Message::Pong(_) => {}
                Message::Text(_) | Message::Binary(_) | Message::Close(_) | Message::Frame(_) => {
                    let closed = matches!(message, Message::Close(_));
                    if self.messages.send(Ok(message)).is_err() || closed {
                        return Poll::Ready(Err(WsError::ConnectionClosed));
                    }
                }
            }
        }
        // The last read was ready, so arrange another poll after the batch.
        cx.waker().wake_by_ref();
        Poll::Pending
    }

    async fn drive<T>(
        &mut self,
        mut operation: impl FnMut(&mut S, &mut Context<'_>) -> Poll<Result<T, WsError>>,
    ) -> Result<T, WsError> {
        let mut first = self.read_budget.is_some();
        poll_fn(|cx| {
            // Read before starting a frame. Once blocked, give the write the first
            // use of the task's cooperative budget on each wake.
            let read_first = std::mem::take(&mut first);
            if read_first {
                let _ = self.poll_incoming(cx, /*recovering*/ false)?;
            }
            let result = operation(&mut self.socket, cx);
            if !read_first && result.is_pending() {
                let _ = self.poll_incoming(cx, /*recovering*/ false)?;
            }
            result
        })
        .await
    }

    async fn recover(&mut self, error: WsError, deadline: Instant) -> WsError {
        if !matches!(error, WsError::ConnectionClosed) {
            // The peer may already have sent the actual response before the local write failed.
            let _ =
                tokio::time::timeout_at(deadline, poll_fn(|cx| self.poll_incoming(cx, true))).await;
        }
        error
    }

    async fn send_frames(&mut self, text: Utf8Bytes, deadline: Instant) -> WsSendOutcome {
        // Limit how far we read ahead of the response consumer during a send.
        self.read_budget = Some(UPLOAD_READ_AHEAD_BYTES);
        let mut started = false;
        for frame in request_frames(text) {
            let mut frame = Some(frame);
            let write = self.drive(|socket, cx| {
                if frame.is_some() {
                    ready!(socket.poll_ready_unpin(cx))?;
                }
                if let Some(frame) = frame.take() {
                    // Even an I/O failure can follow a partially written frame header.
                    let result = socket.start_send_unpin(frame);
                    started |= matches!(result, Ok(()) | Err(WsError::Io(_)));
                    result?;
                }
                socket.poll_flush_unpin(cx)
            });
            let error = match tokio::time::timeout_at(deadline, write).await {
                Ok(Ok(())) => continue,
                Ok(Err(error)) if started => send_error(self.recover(error, deadline).await),
                Ok(Err(error)) => send_error(error),
                Err(_) => ApiError::Stream("idle timeout sending websocket request".into()),
            };
            return if started {
                WsSendOutcome::Interrupted(error)
            } else {
                WsSendOutcome::NotStarted(error)
            };
        }
        self.read_budget = None;
        WsSendOutcome::Flushed
    }

    async fn run(mut self, mut commands: mpsc::Receiver<WsCommand>) {
        loop {
            let command = self.drive(|_, cx| commands.poll_recv(cx).map(Ok)).await;
            let WsCommand {
                request,
                deadline,
                tx_result,
            } = match command {
                Ok(Some(command)) => command,
                Ok(None) | Err(WsError::ConnectionClosed) => break,
                Err(error) => {
                    let _ = self.messages.send(Err(error));
                    break;
                }
            };
            let outcome = self.send_frames(request, deadline).await;
            let flushed = matches!(outcome, WsSendOutcome::Flushed);
            let _ = tx_result.send(outcome);
            if !flushed {
                break;
            }
        }
    }
}

impl WsStream {
    pub(super) fn new(
        socket: impl Stream<Item = Result<Message, WsError>>
        + Sink<Message, Error = WsError>
        + Send
        + Unpin
        + 'static,
    ) -> Self {
        let (tx_command, commands) = mpsc::channel(32);
        let (messages, rx_message) = mpsc::unbounded_channel();
        let pump_task = tokio::spawn(WsPump::new(socket, messages).run(commands));
        Self {
            tx_command,
            rx_message,
            pump_task,
        }
    }

    pub(super) async fn send(&mut self, request: String, timeout: Duration) -> WsSendOutcome {
        let (tx_result, rx_result) = oneshot::channel();
        let command = WsCommand {
            deadline: tokio::time::sleep(timeout).deadline(),
            request: request.into(),
            tx_result,
        };
        let _ = self.tx_command.send(command).await;
        let outcome = rx_result
            .await
            .unwrap_or_else(|_| WsSendOutcome::NotStarted(send_error(WsError::ConnectionClosed)));
        if let WsSendOutcome::NotStarted(mut error) = outcome {
            // Replies to an earlier request must not be attributed to a create that never started.
            while let Some(message) = self.next().await {
                if let Err(idle_error) = message {
                    error = send_error(idle_error);
                    break;
                }
            }
            return WsSendOutcome::NotStarted(error);
        }
        outcome
    }

    pub(super) async fn next(&mut self) -> Option<Result<Message, WsError>> {
        self.rx_message.recv().await
    }

    pub(super) fn is_closed(&self) -> bool {
        self.pump_task.is_finished() || self.tx_command.is_closed()
    }
}

impl Drop for WsStream {
    fn drop(&mut self) {
        self.pump_task.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::super::run_websocket_response_stream;
    use super::*;
    use codex_client::TransportError;
    use http::StatusCode;
    use pretty_assertions::assert_eq;
    use tokio::io::AsyncBufReadExt;
    use tokio::io::BufStream;
    use tokio::io::DuplexStream;
    use tokio_tungstenite::WebSocketStream;
    use tungstenite::protocol::Role;

    const DEADLINE: Duration = Duration::from_secs(5);

    async fn socket_pair(capacity: usize) -> (WsStream, WebSocketStream<BufStream<DuplexStream>>) {
        let (client, server) = tokio::io::duplex(capacity);
        (
            WsStream::new(WebSocketStream::from_raw_socket(client, Role::Client, None).await),
            WebSocketStream::from_raw_socket(BufStream::new(server), Role::Server, None).await,
        )
    }

    #[tokio::test]
    async fn heartbeat_is_answered_before_the_complete_request_arrives() {
        let (mut client, mut server) = socket_pair(1024).await;
        let text = "test".repeat(FRAGMENT_SIZE / 4 + 100);
        let serve = async {
            assert!(!server.get_mut().fill_buf().await.unwrap().is_empty());
            server
                .send(Message::Ping("heartbeat".into()))
                .await
                .unwrap();
            assert_eq!(
                server.next().await.unwrap().unwrap(),
                Message::Pong("heartbeat".into())
            );
            let Message::Text(received) = server.next().await.unwrap().unwrap() else {
                panic!("expected one assembled text request")
            };
            assert!(received.as_str() == text);
        };
        let (outcome, ()) = tokio::time::timeout(DEADLINE, async {
            tokio::join!(client.send(text.clone(), DEADLINE), serve)
        })
        .await
        .unwrap();
        assert!(matches!(outcome, WsSendOutcome::Flushed));
    }

    #[tokio::test]
    async fn blocked_upload_delivers_the_peers_status_before_the_write_failure() {
        let (mut client, mut server) = socket_pair(64).await;
        let (events, _reader) = mpsc::channel(1);
        let serve = async {
            assert!(!server.get_mut().fill_buf().await.unwrap().is_empty());
            server
                .send(Message::text(
                    r#"{"type":"error","status":429,"error":{"code":"rate_limit_exceeded"}}"#,
                ))
                .await
                .unwrap();
            server.close(None).await.unwrap();
        };
        let (result, ()) = tokio::time::timeout(DEADLINE, async {
            tokio::join!(
                run_websocket_response_stream(
                    &mut client,
                    events,
                    "x".repeat(FRAGMENT_SIZE + 1),
                    DEADLINE,
                    None,
                    None
                ),
                serve
            )
        })
        .await
        .unwrap();
        assert!(matches!(
            result,
            Err(ApiError::Transport(TransportError::Http {
                status: StatusCode::TOO_MANY_REQUESTS,
                ..
            }))
        ));
    }

    #[tokio::test]
    async fn idle_pong_finishes_when_the_peer_drains_the_link() {
        let (_client, mut server) = socket_pair(8).await;
        let payload = Bytes::from_static(b"idle heartbeat");
        let reply = tokio::time::timeout(DEADLINE, async {
            server.send(Message::Ping(payload.clone())).await.unwrap();
            server.next().await
        })
        .await
        .unwrap();
        assert_eq!(reply.unwrap().unwrap(), Message::Pong(payload));
    }
}
