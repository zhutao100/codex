use crate::daemon::RuntimeState;
use crate::daemon::apply_notification_to_runtime;
use crate::protocol::HubNotification;
use crate::protocol::RuntimeEventParams;
use crate::protocol::RuntimeRegisterParams;
use crate::protocol::RuntimeSnapshot;
use crate::protocol::RuntimeUpdateMetadataParams;
use crate::protocol::WorkActivity;
use codex_app_server_protocol::ServerNotification;
use serde_json::Value as JsonValue;
use std::collections::BTreeMap;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::PoisonError;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::net::UnixStream;
use tokio::sync::watch;
use tracing::warn;

const MAX_PENDING: usize = 2048;

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

enum Publication {
    State(RuntimeSnapshot),
    Event(RuntimeEventParams),
}

// Projection and its outbound ordering barrier change under the same lock.
// Losing buffered detail never loses the state required to resume a quiet turn.
struct Outbox {
    id: String,
    state: RuntimeState,
    work: watch::Receiver<BTreeMap<String, Option<String>>>,
    generation: String,
    revision: u64,
    pending: Vec<Publication>,
    replace: bool,
    stopped: bool,
}

impl Outbox {
    fn snapshot(&self) -> RuntimeSnapshot {
        self.state.as_snapshot(self.id.clone())
    }

    fn update_work(&mut self) {
        self.revision += 1;
        let owners = self.work.borrow_and_update();
        self.state.apply_scheduler_work(WorkActivity {
            producer_generation: self.generation.clone(),
            revision: self.revision,
            items: owners.keys().cloned().collect(),
            running_turn_keys: Some(owners.values().flatten().cloned().collect()),
        });
    }

    fn push(&mut self, update: Publication) {
        if self.replace {
            return;
        }
        if self.pending.len() == MAX_PENDING {
            self.pending.clear();
            self.replace = true;
        } else {
            self.pending.push(update);
        }
    }

    fn publish(&mut self, notification: HubNotification) {
        let changed = apply_notification_to_runtime(&mut self.state, &notification);
        let work_activity = if changed || self.work.has_changed().unwrap_or(false) {
            self.update_work();
            self.state.work_activity.clone()
        } else {
            None
        };
        self.push(Publication::Event(RuntimeEventParams {
            runtime_id: self.id.clone(),
            notification,
            work_activity,
        }));
    }

    fn drain(&mut self, reconnect: bool) -> Vec<Publication> {
        if reconnect || self.replace {
            self.update_work();
            self.pending.clear();
            self.replace = false;
            return vec![Publication::State(self.snapshot())];
        }
        if self.work.has_changed().unwrap_or(false) {
            self.update_work();
            self.pending.push(Publication::State(self.snapshot()));
        }
        std::mem::take(&mut self.pending)
    }
}

#[derive(Clone)]
pub struct CodexdProducerClient {
    outbox: Arc<Mutex<Outbox>>,
    wake: watch::Sender<()>,
}

impl CodexdProducerClient {
    pub fn spawn(
        codex_home: &Path,
        metadata: RuntimeMetadata,
        work: watch::Receiver<BTreeMap<String, Option<String>>>,
    ) -> Self {
        let socket_path = super::default_socket_path(codex_home);
        Self::spawn_with_socket_path(socket_path, metadata, work)
    }

    pub fn spawn_with_socket_path(
        socket_path: PathBuf,
        metadata: RuntimeMetadata,
        work: watch::Receiver<BTreeMap<String, Option<String>>>,
    ) -> Self {
        let (wake, receiver) = watch::channel(());
        let outbox = Arc::new(Mutex::new(Outbox {
            state: RuntimeState::with_register(&metadata.to_register_params()),
            id: metadata.runtime_id,
            generation: uuid::Uuid::new_v4().to_string(),
            revision: 0,
            work: work.clone(),
            pending: Vec::new(),
            replace: true,
            stopped: false,
        }));
        let writer = Arc::clone(&outbox);
        tokio::spawn(async move {
            run_producer_task(socket_path, writer, receiver, work).await;
        });
        Self { outbox, wake }
    }

    pub async fn update_metadata(&self, metadata: RuntimeMetadata) {
        let mut outbox = self.outbox.lock().unwrap_or_else(PoisonError::into_inner);
        outbox
            .state
            .apply_metadata_update(&metadata.to_update_params());
        outbox.update_work();
        let snapshot = outbox.snapshot();
        outbox.push(Publication::State(snapshot));
        self.wake.send_replace(());
    }

    pub async fn publish_hub_notification(&self, notification: HubNotification) {
        self.try_publish_hub_notification(notification);
    }

    pub fn try_publish_hub_notification(&self, notification: HubNotification) -> bool {
        if self.wake.is_closed() {
            return false;
        }
        self.outbox
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .publish(notification);
        self.wake.send_replace(());
        true
    }

    pub async fn publish_server_notification(&self, notification: &ServerNotification) {
        if let Some(hub_notification) = HubNotification::from_server(notification) {
            self.publish_hub_notification(hub_notification).await;
        }
    }

    pub async fn shutdown(&self) {
        self.outbox
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .stopped = true;
        self.wake.send_replace(());
    }
}

async fn run_producer_task(
    socket_path: PathBuf,
    outbox: Arc<Mutex<Outbox>>,
    mut wake: watch::Receiver<()>,
    mut work: watch::Receiver<BTreeMap<String, Option<String>>>,
) {
    if socket_path_too_long(socket_path.as_path()) {
        warn!(
            "codexd producer disabled because socket path exceeds unix domain socket length: {}",
            socket_path.display()
        );

        return;
    }
    'connect: while !outbox
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .stopped
        && wake.has_changed().is_ok()
    {
        let Ok(mut stream) = UnixStream::connect(&socket_path).await else {
            tokio::time::sleep(Duration::from_millis(500)).await;
            continue;
        };
        let mut reconnect = true;
        loop {
            let batch = {
                let mut outbox = outbox.lock().unwrap_or_else(PoisonError::into_inner);
                if outbox.stopped {
                    return;
                }
                wake.borrow_and_update();
                work.borrow_and_update();
                outbox.drain(reconnect)
            };
            reconnect = false;
            for publication in batch {
                let line = match publication {
                    Publication::State(state) => {
                        encode_notification("codexd/runtime/updateState", serde_json::json!(state))
                    }
                    Publication::Event(event) => {
                        encode_notification("codexd/runtime/event", serde_json::json!(event))
                    }
                };
                if let Some(line) = line
                    && write_line(&mut stream, &line).await.is_err()
                {
                    continue 'connect;
                }
            }
            tokio::select! {
                signal = wake.changed() => if signal.is_err() { return; },
                signal = work.changed() => if signal.is_err() { return; },
                readable = stream.readable() => {
                    // Notification-only producer connections receive no replies. EOF
                    // is observable even if a long-running task emits no progress.
                    match stream.try_read(&mut [0; 256]) {
                        Err(err) if readable.is_ok() && err.kind() == std::io::ErrorKind::WouldBlock => (),
                        Ok(n) if n > 0 => (),
                        _ => continue 'connect,
                    }
                }
            }
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value as JsonValue;
    use std::time::SystemTime;
    use std::time::UNIX_EPOCH;
    use tokio::io::AsyncBufReadExt;
    use tokio::io::BufReader;
    use tokio::net::UnixListener;
    use tokio::time::Duration;
    use tokio::time::timeout;

    #[tokio::test]
    async fn try_publish_hub_notification_preserves_call_order() {
        let socket_path = test_socket_path("ordered");
        let _ = tokio::fs::remove_file(&socket_path).await;
        let listener = UnixListener::bind(&socket_path).expect("bind test listener");
        let (_work_tx, work) = watch::channel(BTreeMap::new());
        let client = CodexdProducerClient::spawn_with_socket_path(
            socket_path.clone(),
            RuntimeMetadata {
                runtime_id: "rt-test".to_string(),
                pid: Some(123),
                session_source: Some("test".to_string()),
                cwd: Some("/tmp".to_string()),
                display_name: Some("test-producer".to_string()),
            },
            work,
        );

        let (stream, _) = timeout(Duration::from_secs(2), listener.accept())
            .await
            .unwrap()
            .unwrap();
        let mut lines = BufReader::new(stream).lines();
        let register = read_json_line(&mut lines).await;
        assert_eq!(register["method"], "codexd/runtime/updateState");
        assert!(client.try_publish_hub_notification(HubNotification {
            method: "turn/started".to_string(),
            params: Some(serde_json::json!({ "index": 1 })),
        }));
        assert!(client.try_publish_hub_notification(HubNotification {
            method: "turn/completed".to_string(),
            params: Some(serde_json::json!({ "index": 2 })),
        }));

        let first = read_json_line(&mut lines).await;
        let second = read_json_line(&mut lines).await;
        assert_eq!(first["method"], "codexd/runtime/event");
        assert_eq!(first["params"]["notification"]["method"], "turn/started");
        assert_eq!(first["params"]["notification"]["params"]["index"], 1);
        assert_eq!(second["method"], "codexd/runtime/event");
        assert_eq!(second["params"]["notification"]["method"], "turn/completed");
        assert_eq!(second["params"]["notification"]["params"]["index"], 2);

        client.shutdown().await;
        let _ = tokio::fs::remove_file(&socket_path).await;
    }

    async fn read_json_line(
        lines: &mut tokio::io::Lines<BufReader<tokio::net::UnixStream>>,
    ) -> JsonValue {
        let line = timeout(Duration::from_secs(2), lines.next_line())
            .await
            .expect("expected line before timeout")
            .expect("read line")
            .expect("line should be present");
        serde_json::from_str(&line).expect("line should be json")
    }

    #[tokio::test]
    async fn quiet_work_recovers_on_a_new_listener_and_offline_completion_is_not_replayed() {
        let path = test_socket_path("quiet");
        let listener = UnixListener::bind(&path).unwrap();
        let (work_tx, work) = watch::channel(BTreeMap::from([
            ("episode".into(), Some("t:1".into())),
            ("peer".into(), Some("a:1".into())),
        ]));
        let client = CodexdProducerClient::spawn_with_socket_path(
            path.clone(),
            RuntimeMetadata {
                runtime_id: "rt".into(),
                pid: None,
                session_source: None,
                cwd: None,
                display_name: None,
            },
            work,
        );
        let accept = |listener: UnixListener| async move {
            let (stream, _) = timeout(Duration::from_secs(4), listener.accept())
                .await
                .unwrap()
                .unwrap();
            BufReader::new(stream).lines()
        };
        let mut lines = accept(listener).await;
        read_json_line(&mut lines).await;
        let turn = |method: &str| HubNotification {
            method: method.into(),
            params: Some(serde_json::json!({"threadId":"t","turn":{"id":"1"}})),
        };
        client.try_publish_hub_notification(turn("turn/started"));
        let started = read_json_line(&mut lines).await;
        assert_eq!(
            started["params"]["workActivity"]["items"],
            serde_json::json!(["episode", "peer"])
        );
        client.try_publish_hub_notification(HubNotification {
            method: "turn/started".into(),
            params: Some(serde_json::json!({"threadId":"a","turn":{"id":"1"}})),
        });
        read_json_line(&mut lines).await;
        assert_eq!(
            client.outbox.lock().unwrap().snapshot().active_turns.len(),
            2
        );
        client.try_publish_hub_notification(HubNotification {
            method: "turn/completed".into(),
            params: Some(serde_json::json!({"threadId":"old-thread","turn":{"id":"1"}})),
        });
        assert_eq!(
            read_json_line(&mut lines).await["params"]["notification"]["method"],
            "turn/completed"
        );
        drop(lines);
        tokio::fs::remove_file(&path).await.unwrap();
        // A ended offline with no terminal notification. B still owns work in this process.
        work_tx.send_modify(|owners| {
            owners.remove("peer");
        });
        let mut lines = accept(UnixListener::bind(&path).unwrap()).await;
        let restored = read_json_line(&mut lines).await;
        assert_eq!(restored["method"], "codexd/runtime/updateState");
        assert_eq!(restored["params"]["activeTurns"][0]["turnKey"], "t:1");
        assert_eq!(
            restored["params"]["activeTurns"].as_array().unwrap().len(),
            1
        );
        assert_eq!(
            restored["params"]["workActivity"]["runningTurnKeys"],
            serde_json::json!(["t:1"])
        );
        assert_eq!(
            restored["params"]["workActivity"]["items"],
            serde_json::json!(["episode"])
        );
        drop(lines);
        tokio::fs::remove_file(&path).await.unwrap();
        // No completion notification (for example an inactive UI stream).
        work_tx.send_replace(BTreeMap::new());
        client.try_publish_hub_notification(turn("turn/started"));
        let mut lines = accept(UnixListener::bind(&path).unwrap()).await;
        let idle = read_json_line(&mut lines).await;
        assert_eq!(idle["params"]["activeTurns"], serde_json::json!([]));
        assert_eq!(
            idle["params"]["workActivity"]["items"],
            serde_json::json!([])
        );

        // Exercise ingress overflow deterministically with the writer excluded.
        {
            let mut outbox = client.outbox.lock().unwrap();
            work_tx.send_replace(BTreeMap::from([("next".into(), Some("t:1".into()))]));
            for _ in 0..=MAX_PENDING {
                outbox.publish(turn("progress"));
            }
            outbox.publish(turn("turn/started"));
            let batch = outbox.drain(false);
            assert!(matches!(&batch[..], [Publication::State(state)]
                if state.active_turns.len() == 1 && state.work_activity.as_ref().unwrap().items.contains("next")));
        }
        client.shutdown().await;
        tokio::fs::remove_file(path).await.unwrap();
    }

    fn test_socket_path(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after unix epoch")
            .as_nanos();
        let pid = std::process::id();
        std::env::temp_dir().join(format!("codexd-producer-{pid}-{nanos}-{label}.sock"))
    }
}
