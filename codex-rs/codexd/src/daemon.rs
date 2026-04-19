use crate::protocol::ActiveTurnSnapshot;
use crate::protocol::CodexdEventEnvelope;
use crate::protocol::CodexdEventPayload;
use crate::protocol::CodexdSnapshotResponse;
use crate::protocol::CodexdSubscribeParams;
use crate::protocol::CodexdSubscribeResponse;
use crate::protocol::HubNotification;
use crate::protocol::RuntimeEventParams;
use crate::protocol::RuntimeRegisterParams;
use crate::protocol::RuntimeSnapshot;
use crate::protocol::RuntimeUnregisterParams;
use crate::protocol::RuntimeUpdateMetadataParams;
use anyhow::Context;
use serde::de::DeserializeOwned;
use serde_json::Value as JsonValue;
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::VecDeque;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::AsyncBufReadExt;
use tokio::io::AsyncWriteExt;
use tokio::io::BufReader;
use tokio::net::UnixListener;
use tokio::net::UnixStream;
use tokio::sync::Mutex;
use tokio::sync::mpsc;
use tracing::debug;
use tracing::warn;

const CODEXD_SUBSCRIBE_METHOD: &str = "codexd/subscribe";
const CODEXD_SNAPSHOT_METHOD: &str = "codexd/snapshot";
const CODEXD_EVENT_METHOD: &str = "codexd/event";
const RUNTIME_EVENT_METHOD: &str = "codexd/runtime/event";
const RUNTIME_REGISTER_METHOD: &str = "codexd/runtime/register";
const RUNTIME_UNREGISTER_METHOD: &str = "codexd/runtime/unregister";
const RUNTIME_UPDATE_METADATA_METHOD: &str = "codexd/runtime/updateMetadata";
const LAUNCHD_SOCKET_NAME: &str = "codexd";
const MAX_RECENT_EVENTS: usize = 1024;

#[derive(Debug, Clone)]
struct RuntimeState {
    pid: Option<u32>,
    session_source: Option<String>,
    cwd: Option<String>,
    display_name: Option<String>,
    active_turns: BTreeMap<String, String>,
}

impl RuntimeState {
    fn with_register(params: &RuntimeRegisterParams) -> Self {
        Self {
            pid: params.pid,
            session_source: params.session_source.clone(),
            cwd: params.cwd.clone(),
            display_name: params.display_name.clone(),
            active_turns: BTreeMap::new(),
        }
    }

    fn apply_metadata_update(&mut self, params: &RuntimeUpdateMetadataParams) {
        self.pid = params.pid.or(self.pid);
        self.session_source = params
            .session_source
            .clone()
            .or_else(|| self.session_source.clone());
        self.cwd = params.cwd.clone().or_else(|| self.cwd.clone());
        self.display_name = params
            .display_name
            .clone()
            .or_else(|| self.display_name.clone());
    }

    fn as_snapshot(&self, runtime_id: String) -> RuntimeSnapshot {
        let active_turns = self
            .active_turns
            .iter()
            .map(|(turn_id, thread_id)| ActiveTurnSnapshot {
                thread_id: thread_id.clone(),
                turn_id: turn_id.clone(),
            })
            .collect();

        RuntimeSnapshot {
            runtime_id,
            pid: self.pid,
            session_source: self.session_source.clone(),
            cwd: self.cwd.clone(),
            display_name: self.display_name.clone(),
            active_turns,
        }
    }
}

#[derive(Default)]
struct DaemonState {
    next_connection_id: u64,
    seq: u64,
    runtimes: BTreeMap<String, RuntimeState>,
    subscribers: HashMap<u64, mpsc::UnboundedSender<String>>,
    recent_events: VecDeque<(u64, String)>,
}

impl DaemonState {
    fn alloc_connection_id(&mut self) -> u64 {
        self.next_connection_id = self.next_connection_id.saturating_add(1);
        self.next_connection_id
    }

    fn snapshot(&self) -> CodexdSnapshotResponse {
        let runtimes = self
            .runtimes
            .iter()
            .map(|(runtime_id, runtime)| runtime.as_snapshot(runtime_id.clone()))
            .collect();

        CodexdSnapshotResponse {
            seq: self.seq,
            runtimes,
        }
    }

    fn add_subscriber(
        &mut self,
        connection_id: u64,
        sender: mpsc::UnboundedSender<String>,
        after_seq: Option<u64>,
    ) -> Result<CodexdSubscribeResponse, String> {
        if let Some(after_seq) = after_seq
            && after_seq > self.seq
        {
            return Err(format!(
                "afterSeq {after_seq} is ahead of current sequence {}",
                self.seq
            ));
        }

        let replay_sender = sender.clone();
        self.subscribers.insert(connection_id, sender);

        if let Some(after_seq) = after_seq {
            for (seq, line) in &self.recent_events {
                if *seq > after_seq {
                    let _ = replay_sender.send(line.clone());
                }
            }
        }
        Ok(CodexdSubscribeResponse { seq: self.seq })
    }

    fn remove_subscriber(&mut self, connection_id: u64) {
        self.subscribers.remove(&connection_id);
    }

    fn upsert_runtime_from_register(&mut self, params: RuntimeRegisterParams) {
        let runtime_id = params.runtime_id.clone();
        let snapshot = {
            let runtime = self
                .runtimes
                .entry(runtime_id.clone())
                .or_insert_with(|| RuntimeState::with_register(&params));
            runtime.pid = params.pid.or(runtime.pid);
            runtime.session_source = params
                .session_source
                .clone()
                .or_else(|| runtime.session_source.clone());
            runtime.cwd = params.cwd.clone().or_else(|| runtime.cwd.clone());
            runtime.display_name = params
                .display_name
                .clone()
                .or_else(|| runtime.display_name.clone());
            runtime.as_snapshot(runtime_id)
        };

        self.broadcast_event(CodexdEventPayload::RuntimeUpsert { runtime: snapshot });
    }

    fn update_runtime_metadata(&mut self, params: RuntimeUpdateMetadataParams) {
        let runtime_id = params.runtime_id.clone();
        let snapshot = {
            let runtime = self
                .runtimes
                .entry(runtime_id.clone())
                .or_insert_with(|| RuntimeState {
                    pid: params.pid,
                    session_source: params.session_source.clone(),
                    cwd: params.cwd.clone(),
                    display_name: params.display_name.clone(),
                    active_turns: BTreeMap::new(),
                });
            runtime.apply_metadata_update(&params);
            runtime.as_snapshot(runtime_id)
        };

        self.broadcast_event(CodexdEventPayload::RuntimeUpsert { runtime: snapshot });
    }

    fn apply_runtime_notification(&mut self, params: RuntimeEventParams) {
        let runtime = self
            .runtimes
            .entry(params.runtime_id.clone())
            .or_insert_with(|| RuntimeState {
                pid: None,
                session_source: None,
                cwd: None,
                display_name: None,
                active_turns: BTreeMap::new(),
            });

        if let Some((thread_id, turn_id)) = parse_active_turn_started(&params.notification) {
            runtime.active_turns.insert(turn_id, thread_id);
        }

        if let Some(turn_id) = parse_active_turn_completed(&params.notification) {
            runtime.active_turns.remove(&turn_id);
        }

        self.broadcast_event(CodexdEventPayload::RuntimeNotification {
            runtime_id: params.runtime_id,
            notification: params.notification,
        });
    }

    fn unregister_runtime(&mut self, runtime_id: &str) {
        if self.runtimes.remove(runtime_id).is_none() {
            return;
        }

        self.broadcast_event(CodexdEventPayload::RuntimeRemoved {
            runtime_id: runtime_id.to_string(),
        });
    }

    fn broadcast_event(&mut self, payload: CodexdEventPayload) {
        self.seq = self.seq.saturating_add(1);
        let envelope = CodexdEventEnvelope {
            seq: self.seq,
            event: payload,
        };

        let Some(line) = encode_notification(CODEXD_EVENT_METHOD, serde_json::json!(envelope))
        else {
            return;
        };

        self.recent_events.push_back((self.seq, line.clone()));
        while self.recent_events.len() > MAX_RECENT_EVENTS {
            self.recent_events.pop_front();
        }

        let mut dead_connections = Vec::new();
        for (&connection_id, sender) in &self.subscribers {
            if sender.send(line.clone()).is_err() {
                dead_connections.push(connection_id);
            }
        }

        for connection_id in dead_connections {
            self.subscribers.remove(&connection_id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subscribe_replays_events_after_seq() {
        let mut state = DaemonState::default();
        state.broadcast_event(CodexdEventPayload::RuntimeRemoved {
            runtime_id: "rt-1".to_string(),
        });
        state.broadcast_event(CodexdEventPayload::RuntimeRemoved {
            runtime_id: "rt-2".to_string(),
        });

        let (tx, mut rx) = mpsc::unbounded_channel::<String>();
        let response = state
            .add_subscriber(1, tx, Some(1))
            .expect("subscribe should succeed");
        assert_eq!(response, CodexdSubscribeResponse { seq: 2 });

        let line = rx.try_recv().expect("expected replay event");
        let value: JsonValue = serde_json::from_str(&line).expect("expected valid JSON");
        assert_eq!(value["method"], CODEXD_EVENT_METHOD);
        assert_eq!(value["params"]["seq"], 2);
        assert_eq!(value["params"]["event"]["type"], "runtimeRemoved");
        assert_eq!(value["params"]["event"]["runtimeId"], "rt-2");

        assert!(rx.try_recv().is_err());
    }
}

pub async fn run_daemon(codex_home: &Path, socket_path: Option<PathBuf>) -> anyhow::Result<()> {
    let fallback_socket_path = super::default_socket_path(codex_home);
    let socket_path = socket_path.unwrap_or(fallback_socket_path);

    let (listener, owns_socket_path) = match launchd_listener().await? {
        Some(listener) => {
            debug!("codexd using launchd socket activation");
            (listener, false)
        }
        None => {
            let listener = bind_listener(&socket_path).await?;
            (listener, true)
        }
    };

    let state = Arc::new(Mutex::new(DaemonState::default()));

    loop {
        tokio::select! {
            accept_result = listener.accept() => {
                let (stream, _addr) = match accept_result {
                    Ok(value) => value,
                    Err(err) => {
                        warn!("codexd failed to accept connection: {err}");
                        continue;
                    }
                };

                let state = Arc::clone(&state);
                tokio::spawn(async move {
                    if let Err(err) = handle_connection(state, stream).await {
                        debug!("codexd connection closed with error: {err}");
                    }
                });
            }
            _ = tokio::signal::ctrl_c() => {
                break;
            }
        }
    }

    if owns_socket_path {
        let _ = tokio::fs::remove_file(&socket_path).await;
    }

    Ok(())
}

async fn handle_connection(
    state: Arc<Mutex<DaemonState>>,
    stream: UnixStream,
) -> anyhow::Result<()> {
    let connection_id = {
        let mut state = state.lock().await;
        state.alloc_connection_id()
    };

    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();

    let (writer_tx, mut writer_rx) = mpsc::unbounded_channel::<String>();
    let writer_handle = tokio::spawn(async move {
        while let Some(line) = writer_rx.recv().await {
            if writer.write_all(line.as_bytes()).await.is_err() {
                break;
            }
            if writer.write_all(b"\n").await.is_err() {
                break;
            }
            if writer.flush().await.is_err() {
                break;
            }
        }
    });

    let mut owned_runtime_ids = HashSet::<String>::new();
    let mut subscribed = false;

    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }

        let value: JsonValue = match serde_json::from_str(&line) {
            Ok(value) => value,
            Err(err) => {
                warn!("codexd ignored invalid JSON line: {err}");
                continue;
            }
        };

        let Some(method) = value
            .get("method")
            .and_then(JsonValue::as_str)
            .map(ToString::to_string)
        else {
            continue;
        };

        let params = value.get("params").cloned();
        let request_id = value.get("id").cloned();

        let result = match method.as_str() {
            CODEXD_SNAPSHOT_METHOD => handle_snapshot_method(Arc::clone(&state)).await,
            CODEXD_SUBSCRIBE_METHOD => {
                handle_subscribe_method(
                    Arc::clone(&state),
                    connection_id,
                    writer_tx.clone(),
                    params,
                    &mut subscribed,
                )
                .await
            }
            RUNTIME_REGISTER_METHOD => {
                handle_runtime_register_method(Arc::clone(&state), params, &mut owned_runtime_ids)
                    .await
            }
            RUNTIME_UPDATE_METADATA_METHOD => {
                handle_runtime_update_metadata_method(
                    Arc::clone(&state),
                    params,
                    &mut owned_runtime_ids,
                )
                .await
            }
            RUNTIME_EVENT_METHOD => {
                handle_runtime_event_method(Arc::clone(&state), params, &mut owned_runtime_ids)
                    .await
            }
            RUNTIME_UNREGISTER_METHOD => {
                handle_runtime_unregister_method(Arc::clone(&state), params, &mut owned_runtime_ids)
                    .await
            }
            _ => Err(format!("unknown method `{method}`")),
        };

        if let Some(request_id) = request_id {
            let line = match result {
                Ok(result_value) => encode_response(request_id, result_value),
                Err(message) => encode_error(request_id, -32000, message),
            };

            if let Some(line) = line {
                let _ = writer_tx.send(line);
            }
        }
    }

    cleanup_connection(state, connection_id, owned_runtime_ids, subscribed).await;

    drop(writer_tx);
    let _ = writer_handle.await;

    Ok(())
}

async fn cleanup_connection(
    state: Arc<Mutex<DaemonState>>,
    connection_id: u64,
    owned_runtime_ids: HashSet<String>,
    subscribed: bool,
) {
    let mut state = state.lock().await;

    if subscribed {
        state.remove_subscriber(connection_id);
    }

    for runtime_id in owned_runtime_ids {
        state.unregister_runtime(&runtime_id);
    }
}

async fn handle_snapshot_method(state: Arc<Mutex<DaemonState>>) -> Result<JsonValue, String> {
    let state = state.lock().await;
    serde_json::to_value(state.snapshot()).map_err(|err| err.to_string())
}

async fn handle_subscribe_method(
    state: Arc<Mutex<DaemonState>>,
    connection_id: u64,
    writer_tx: mpsc::UnboundedSender<String>,
    params: Option<JsonValue>,
    subscribed: &mut bool,
) -> Result<JsonValue, String> {
    let params: CodexdSubscribeParams = deserialize_params(params)?;

    let mut state = state.lock().await;
    let response = state.add_subscriber(connection_id, writer_tx, params.after_seq)?;
    *subscribed = true;

    serde_json::to_value(response).map_err(|err| err.to_string())
}

async fn handle_runtime_register_method(
    state: Arc<Mutex<DaemonState>>,
    params: Option<JsonValue>,
    owned_runtime_ids: &mut HashSet<String>,
) -> Result<JsonValue, String> {
    let params: RuntimeRegisterParams = deserialize_params(params)?;

    owned_runtime_ids.insert(params.runtime_id.clone());

    let mut state = state.lock().await;
    state.upsert_runtime_from_register(params);
    Ok(serde_json::json!({}))
}

async fn handle_runtime_update_metadata_method(
    state: Arc<Mutex<DaemonState>>,
    params: Option<JsonValue>,
    owned_runtime_ids: &mut HashSet<String>,
) -> Result<JsonValue, String> {
    let params: RuntimeUpdateMetadataParams = deserialize_params(params)?;

    owned_runtime_ids.insert(params.runtime_id.clone());

    let mut state = state.lock().await;
    state.update_runtime_metadata(params);
    Ok(serde_json::json!({}))
}

async fn handle_runtime_event_method(
    state: Arc<Mutex<DaemonState>>,
    params: Option<JsonValue>,
    owned_runtime_ids: &mut HashSet<String>,
) -> Result<JsonValue, String> {
    let params: RuntimeEventParams = deserialize_params(params)?;

    owned_runtime_ids.insert(params.runtime_id.clone());

    let mut state = state.lock().await;
    state.apply_runtime_notification(params);
    Ok(serde_json::json!({}))
}

async fn handle_runtime_unregister_method(
    state: Arc<Mutex<DaemonState>>,
    params: Option<JsonValue>,
    owned_runtime_ids: &mut HashSet<String>,
) -> Result<JsonValue, String> {
    let params: RuntimeUnregisterParams = deserialize_params(params)?;

    owned_runtime_ids.remove(&params.runtime_id);

    let mut state = state.lock().await;
    state.unregister_runtime(&params.runtime_id);
    Ok(serde_json::json!({}))
}

fn parse_active_turn_started(notification: &HubNotification) -> Option<(String, String)> {
    if notification.method != "turn/started" {
        return None;
    }

    let params = notification.params.as_ref()?.as_object()?;
    let thread_id = params.get("threadId")?.as_str()?.to_string();
    let turn = params.get("turn")?.as_object()?;
    let turn_id = turn.get("id")?.as_str()?.to_string();

    Some((thread_id, turn_id))
}

fn parse_active_turn_completed(notification: &HubNotification) -> Option<String> {
    if notification.method != "turn/completed" {
        return None;
    }

    let params = notification.params.as_ref()?.as_object()?;
    let turn = params.get("turn")?.as_object()?;
    let turn_id = turn.get("id")?.as_str()?.to_string();

    Some(turn_id)
}

fn deserialize_params<T: DeserializeOwned>(params: Option<JsonValue>) -> Result<T, String> {
    let value = params.unwrap_or(JsonValue::Null);
    serde_json::from_value(value).map_err(|err| err.to_string())
}

fn encode_notification(method: &str, params: JsonValue) -> Option<String> {
    serde_json::to_string(&serde_json::json!({
        "method": method,
        "params": params,
    }))
    .ok()
}

fn encode_response(id: JsonValue, result: JsonValue) -> Option<String> {
    serde_json::to_string(&serde_json::json!({
        "id": id,
        "result": result,
    }))
    .ok()
}

fn encode_error(id: JsonValue, code: i64, message: String) -> Option<String> {
    serde_json::to_string(&serde_json::json!({
        "id": id,
        "error": {
            "code": code,
            "message": message,
        },
    }))
    .ok()
}

async fn bind_listener(socket_path: &Path) -> anyhow::Result<UnixListener> {
    let parent = socket_path
        .parent()
        .context("codexd socket path has no parent directory")?;

    tokio::fs::create_dir_all(parent).await.with_context(|| {
        format!(
            "failed to create codexd runtime directory {}",
            parent.display()
        )
    })?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let _ = tokio::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700)).await;
    }

    if socket_path.exists() {
        let _ = tokio::fs::remove_file(socket_path).await;
    }

    let listener = UnixListener::bind(socket_path)
        .with_context(|| format!("failed to bind codexd socket {}", socket_path.display()))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let _ =
            tokio::fs::set_permissions(socket_path, std::fs::Permissions::from_mode(0o600)).await;
    }

    Ok(listener)
}

async fn launchd_listener() -> anyhow::Result<Option<UnixListener>> {
    #[cfg(target_os = "macos")]
    {
        use std::ffi::CString;
        use std::os::fd::FromRawFd;

        unsafe extern "C" {
            fn launch_activate_socket(
                name: *const libc::c_char,
                fds: *mut *mut libc::c_int,
                cnt: *mut libc::size_t,
            ) -> libc::c_int;
        }

        let socket_name = CString::new(LAUNCHD_SOCKET_NAME)?;
        let mut fds_ptr: *mut libc::c_int = std::ptr::null_mut();
        let mut count: libc::size_t = 0;

        let status =
            unsafe { launch_activate_socket(socket_name.as_ptr(), &mut fds_ptr, &mut count) };

        if status != 0 || fds_ptr.is_null() || count == 0 {
            return Ok(None);
        }

        let fds = unsafe { std::slice::from_raw_parts(fds_ptr, count as usize) };
        let active_fd = fds[0];

        for fd in &fds[1..] {
            let _ = unsafe { libc::close(*fd) };
        }

        unsafe {
            libc::free(fds_ptr.cast());
        }

        let std_listener = unsafe { std::os::unix::net::UnixListener::from_raw_fd(active_fd) };
        std_listener.set_nonblocking(true)?;

        let listener = UnixListener::from_std(std_listener)?;
        Ok(Some(listener))
    }

    #[cfg(not(target_os = "macos"))]
    {
        Ok(None)
    }
}
