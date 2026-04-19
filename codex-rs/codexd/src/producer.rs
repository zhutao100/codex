use crate::protocol::HubNotification;
use crate::protocol::RuntimeRegisterParams;
use crate::protocol::RuntimeUpdateMetadataParams;
use codex_app_server_protocol::ServerNotification;
use serde_json::Value as JsonValue;
use std::collections::VecDeque;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::net::UnixStream;
use tokio::sync::mpsc;
use tracing::debug;
use tracing::warn;

const DEFAULT_CHANNEL_CAPACITY: usize = 2048;
const MAX_PENDING_LINES: usize = 4096;

#[derive(Debug, Clone)]
pub struct RuntimeMetadata {
    pub runtime_id: String,
    pub pid: Option<u32>,
    pub session_source: Option<String>,
    pub cwd: Option<String>,
    pub display_name: Option<String>,
}

impl RuntimeMetadata {
    pub fn to_register_params(&self) -> RuntimeRegisterParams {
        RuntimeRegisterParams {
            runtime_id: self.runtime_id.clone(),
            pid: self.pid,
            session_source: self.session_source.clone(),
            cwd: self.cwd.clone(),
            display_name: self.display_name.clone(),
        }
    }

    pub fn to_update_params(&self) -> RuntimeUpdateMetadataParams {
        RuntimeUpdateMetadataParams {
            runtime_id: self.runtime_id.clone(),
            pid: self.pid,
            session_source: self.session_source.clone(),
            cwd: self.cwd.clone(),
            display_name: self.display_name.clone(),
        }
    }
}

#[derive(Debug)]
enum ProducerCommand {
    UpdateMetadata(RuntimeMetadata),
    PublishNotification(HubNotification),
    Shutdown,
}

#[derive(Clone)]
pub struct CodexdProducerClient {
    sender: mpsc::Sender<ProducerCommand>,
}

impl CodexdProducerClient {
    pub fn spawn(codex_home: &Path, metadata: RuntimeMetadata) -> Self {
        let socket_path = super::default_socket_path(codex_home);
        Self::spawn_with_socket_path(socket_path, metadata)
    }

    pub fn spawn_with_socket_path(socket_path: PathBuf, metadata: RuntimeMetadata) -> Self {
        let (sender, receiver) = mpsc::channel(DEFAULT_CHANNEL_CAPACITY);

        tokio::spawn(async move {
            run_producer_task(socket_path, metadata, receiver).await;
        });

        Self { sender }
    }

    pub async fn update_metadata(&self, metadata: RuntimeMetadata) {
        if self
            .sender
            .send(ProducerCommand::UpdateMetadata(metadata))
            .await
            .is_err()
        {
            debug!("codexd producer task stopped before metadata update could be queued");
        }
    }

    pub async fn publish_hub_notification(&self, notification: HubNotification) {
        if self
            .sender
            .send(ProducerCommand::PublishNotification(notification))
            .await
            .is_err()
        {
            debug!("codexd producer task stopped before notification could be queued");
        }
    }

    pub async fn publish_server_notification(&self, notification: &ServerNotification) {
        if let Some(hub_notification) = hub_notification_from_server_notification(notification) {
            self.publish_hub_notification(hub_notification).await;
        }
    }

    pub async fn shutdown(&self) {
        let _ = self.sender.send(ProducerCommand::Shutdown).await;
    }
}

fn hub_notification_from_server_notification(
    notification: &ServerNotification,
) -> Option<HubNotification> {
    let value = serde_json::to_value(notification).ok()?;
    let object = value.as_object()?;
    let method = object.get("method")?.as_str()?.to_string();
    let params = object.get("params").cloned();
    Some(HubNotification { method, params })
}

async fn run_producer_task(
    socket_path: PathBuf,
    mut metadata: RuntimeMetadata,
    mut receiver: mpsc::Receiver<ProducerCommand>,
) {
    if socket_path_too_long(socket_path.as_path()) {
        warn!(
            "codexd producer disabled because socket path exceeds unix domain socket length: {}",
            socket_path.display()
        );

        while let Some(command) = receiver.recv().await {
            if matches!(command, ProducerCommand::Shutdown) {
                break;
            }
        }

        return;
    }

    let mut connection: Option<UnixStream> = None;
    let mut pending_lines = VecDeque::<String>::new();
    let mut needs_register = true;
    let mut flush_interval = tokio::time::interval(Duration::from_millis(500));

    loop {
        tokio::select! {
            maybe_command = receiver.recv() => {
                let Some(command) = maybe_command else {
                    break;
                };

                match command {
                    ProducerCommand::UpdateMetadata(next_metadata) => {
                        metadata = next_metadata;
                        needs_register = true;
                    }
                    ProducerCommand::PublishNotification(notification) => {
                        if let Some(line) = encode_notification(
                            "codexd/runtime/event",
                            serde_json::json!({
                                "runtimeId": metadata.runtime_id,
                                "notification": notification,
                            }),
                        ) {
                            push_pending_line(&mut pending_lines, line);
                        }
                    }
                    ProducerCommand::Shutdown => {
                        break;
                    }
                }
            }
            _ = flush_interval.tick() => {}
        }

        flush_pending_lines(
            socket_path.as_path(),
            &metadata,
            &mut connection,
            &mut pending_lines,
            &mut needs_register,
        )
        .await;
    }
}

fn push_pending_line(pending_lines: &mut VecDeque<String>, line: String) {
    pending_lines.push_back(line);

    if pending_lines.len() <= MAX_PENDING_LINES {
        return;
    }

    let overflow = pending_lines.len() - MAX_PENDING_LINES;
    for _ in 0..overflow {
        pending_lines.pop_front();
    }

    warn!("codexd producer queue overflowed; dropped {overflow} pending messages");
}

async fn flush_pending_lines(
    socket_path: &Path,
    metadata: &RuntimeMetadata,
    connection: &mut Option<UnixStream>,
    pending_lines: &mut VecDeque<String>,
    needs_register: &mut bool,
) {
    if pending_lines.is_empty() && !*needs_register {
        return;
    }

    if connection.is_none() {
        match UnixStream::connect(socket_path).await {
            Ok(stream) => {
                debug!("connected codexd producer socket {}", socket_path.display());
                *connection = Some(stream);
                *needs_register = true;
            }
            Err(err) => {
                debug!(
                    "failed to connect codexd producer socket {}: {err}",
                    socket_path.display()
                );
                return;
            }
        }
    }

    let Some(stream) = connection.as_mut() else {
        return;
    };

    if *needs_register {
        let register_line = encode_notification(
            "codexd/runtime/register",
            serde_json::to_value(metadata.to_register_params()).unwrap_or(JsonValue::Null),
        );

        if let Some(register_line) = register_line
            && write_line(stream, &register_line).await.is_err()
        {
            *connection = None;
            return;
        }

        *needs_register = false;
    }

    while let Some(line) = pending_lines.front() {
        if write_line(stream, line).await.is_ok() {
            pending_lines.pop_front();
            continue;
        }

        *connection = None;
        return;
    }
}

fn encode_notification(method: &str, params: JsonValue) -> Option<String> {
    let message = serde_json::json!({
        "method": method,
        "params": params,
    });

    serde_json::to_string(&message).ok()
}

async fn write_line(stream: &mut UnixStream, line: &str) -> std::io::Result<()> {
    stream.write_all(line.as_bytes()).await?;
    stream.write_all(b"\n").await?;
    stream.flush().await
}

fn socket_path_too_long(socket_path: &Path) -> bool {
    let max_len = max_unix_socket_path_len();
    socket_path.as_os_str().as_bytes().len() >= max_len
}

fn max_unix_socket_path_len() -> usize {
    std::mem::size_of::<libc::sockaddr_un>() - std::mem::offset_of!(libc::sockaddr_un, sun_path)
}
