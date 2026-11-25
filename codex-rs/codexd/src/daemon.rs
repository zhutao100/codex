use crate::protocol::ActiveTurnSnapshot;
use crate::protocol::CodexdEventEnvelope;
use crate::protocol::CodexdEventPayload;
use crate::protocol::CodexdHelloResponse;
use crate::protocol::CodexdSnapshotResponse;
use crate::protocol::CodexdSubscribeParams;
use crate::protocol::CodexdSubscribeResponse;
use crate::protocol::HubNotification;
use crate::protocol::REQUEST_STATE_METHOD;
use crate::protocol::RequestState;
use crate::protocol::RuntimeEventParams;
use crate::protocol::RuntimeRegisterParams;
use crate::protocol::RuntimeSnapshot;
use crate::protocol::RuntimeUpdateMetadataParams;
use crate::protocol::RuntimeUpdateStateParams;
use crate::protocol::WorkActivity;
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
const CODEXD_HELLO_METHOD: &str = "codexd/hello";
const CODEXD_EVENT_METHOD: &str = "codexd/event";
const RUNTIME_EVENT_METHOD: &str = "codexd/runtime/event";
const RUNTIME_REGISTER_METHOD: &str = "codexd/runtime/register";
const RUNTIME_UNREGISTER_METHOD: &str = "codexd/runtime/unregister";
const RUNTIME_UPDATE_METADATA_METHOD: &str = "codexd/runtime/updateMetadata";
const RUNTIME_UPDATE_STATE_METHOD: &str = "codexd/runtime/updateState";
const LAUNCHD_SOCKET_NAME: &str = "codexd";
const MAX_RECENT_EVENTS: usize = 1024;
const PROTOCOL_VERSION: u32 = 1;

#[derive(Debug, Clone, Default)]
pub(crate) struct RuntimeState {
    pid: Option<u32>,
    session_source: Option<String>,
    cwd: Option<String>,
    display_name: Option<String>,
    pub(crate) active_turns: BTreeMap<String, ActiveTurnSnapshot>,
    pub(crate) work_activity: Option<WorkActivity>,
    thread_token_usage: BTreeMap<String, JsonValue>,
}

impl RuntimeState {
    pub(crate) fn apply_scheduler_work(&mut self, work: WorkActivity) {
        self.work_activity = Some(work);
        self.prune_unowned_turns();
    }

    fn prune_unowned_turns(&mut self) {
        let Some(work) = &self.work_activity else {
            return;
        };
        self.active_turns.retain(|_, turn| work.permits_turn(turn));
    }

    pub(crate) fn with_register(params: &RuntimeRegisterParams) -> Self {
        Self {
            pid: params.pid,
            session_source: params.session_source.clone(),
            cwd: params.cwd.clone(),
            display_name: params.display_name.clone(),
            ..Self::default()
        }
    }

    pub(crate) fn apply_metadata_update(&mut self, params: &RuntimeUpdateMetadataParams) {
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

    pub(crate) fn as_snapshot(&self, runtime_id: String) -> RuntimeSnapshot {
        let active_turns = self.active_turns.values().cloned().collect();

        RuntimeSnapshot {
            runtime_id,
            pid: self.pid,
            session_source: self.session_source.clone(),
            cwd: self.cwd.clone(),
            display_name: self.display_name.clone(),
            active_turns,
            work_activity_supported: self.work_activity.is_some(),
            work_activity: self.work_activity.clone(),
        }
    }
}

impl WorkActivity {
    fn permits_turn(&self, turn: &ActiveTurnSnapshot) -> bool {
        !self.items.is_empty()
            && self
                .running_turn_keys
                .as_ref()
                .is_none_or(|running| running.contains(&turn_key(&turn.thread_id, &turn.turn_id)))
    }

    fn validate(&self) -> Result<(), String> {
        if self.items.iter().any(String::is_empty)
            || self
                .running_turn_keys
                .iter()
                .flatten()
                .any(String::is_empty)
        {
            return Err("workActivity contains an empty owner or turn key".into());
        }
        if self.items.is_empty()
            && self
                .running_turn_keys
                .as_ref()
                .is_some_and(|keys| !keys.is_empty())
        {
            return Err("idle workActivity conflicts with runningTurnKeys".into());
        }
        Ok(())
    }
}

#[derive(Default)]
struct ProducerSlot {
    connection: u64,
    observed: Option<(String, u64)>,
    requested_state: Option<RequestState>,
}

#[derive(Default)]
struct DaemonState {
    instance_id: std::sync::OnceLock<uuid::Uuid>,
    next_connection_id: u64,
    seq: u64,
    runtimes: BTreeMap<String, RuntimeState>,
    runtime_connections: HashMap<String, ProducerSlot>,
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
            .map(|(runtime_id, runtime)| {
                let mut snapshot = runtime.as_snapshot(runtime_id.clone());
                self.project_pending_repair(&mut snapshot);
                snapshot
            })
            .collect();

        CodexdSnapshotResponse {
            seq: self.seq,
            runtimes,
        }
    }

    fn hello(&self) -> CodexdHelloResponse {
        CodexdHelloResponse {
            protocol_version: PROTOCOL_VERSION,
            instance_id: Some(self.instance_id.get_or_init(uuid::Uuid::new_v4).to_string()),
            capabilities: vec![
                "eventReplay".to_string(),
                "runtimeState".to_string(),
                "activeTurnContext".to_string(),
                "workActivity".to_string(),
            ],
            seq: self.seq,
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
            let runtime = self.runtimes.entry(runtime_id.clone()).or_default();
            runtime.apply_metadata_update(&params);
            runtime.as_snapshot(runtime_id)
        };

        self.broadcast_event(CodexdEventPayload::RuntimeUpsert { runtime: snapshot });
    }

    fn update_runtime_state(&mut self, params: RuntimeUpdateStateParams) {
        let runtime_id = params.runtime_id.clone();
        let snapshot = {
            let runtime = self.runtimes.entry(runtime_id.clone()).or_default();

            if let (Some(old), Some(new)) = (&runtime.work_activity, &params.work_activity)
                && old.producer_generation == new.producer_generation
                && old.revision > new.revision
            {
                return;
            }
            runtime.work_activity = params.work_activity;
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
            let mut thread_token_usage = runtime.thread_token_usage.clone();
            runtime.active_turns = params
                .active_turns
                .into_iter()
                .map(|mut turn| {
                    let turn_key =
                        projected_key(turn.turn_key.as_deref(), &turn.thread_id, &turn.turn_id);
                    turn.turn_key = Some(turn_key.clone());
                    // A complete producer projection already owns the baseline.
                    // Its current-turn usage must not become its own starting total.
                    if runtime.work_activity.is_none() && turn.token_usage_baseline.is_none() {
                        turn.token_usage_baseline =
                            thread_token_usage.get(turn.thread_id.as_str()).cloned();
                    }
                    if let Some(token_usage) = turn.token_usage.clone() {
                        thread_token_usage.insert(turn.thread_id.clone(), token_usage);
                    }
                    (turn_key, turn)
                })
                .collect();
            runtime.thread_token_usage = thread_token_usage;
            runtime.prune_unowned_turns();
            runtime.as_snapshot(runtime_id)
        };

        self.broadcast_event(CodexdEventPayload::RuntimeUpsert { runtime: snapshot });
    }

    fn apply_runtime_notification(&mut self, params: RuntimeEventParams) {
        let runtime_id = params.runtime_id;
        let notification = params.notification;
        let (updated_snapshot, forward) = {
            let runtime = self.runtimes.entry(runtime_id.clone()).or_default();

            if let (Some(work), Some(old)) = (&params.work_activity, &runtime.work_activity)
                && (old.producer_generation != work.producer_generation
                    || old.revision >= work.revision)
            {
                return;
            }
            let effective_work = params
                .work_activity
                .as_ref()
                .or(runtime.work_activity.as_ref());
            let forward = notification.method != "turn/started"
                || effective_work.is_none_or(|work| {
                    parse_active_turn_started(&notification)
                        .is_some_and(|turn| work.permits_turn(&turn))
                });
            let changed = forward && apply_notification_to_runtime(runtime, &notification);
            let changed_work = params.work_activity.is_some();
            if let Some(work) = params.work_activity {
                runtime.apply_scheduler_work(work);
            } else if changed {
                runtime.prune_unowned_turns();
            }
            (
                (changed || changed_work).then(|| runtime.as_snapshot(runtime_id.clone())),
                forward,
            )
        };

        if let Some(runtime) = updated_snapshot {
            self.broadcast_event(CodexdEventPayload::RuntimeUpsert { runtime });
        }

        if forward {
            self.broadcast_event(CodexdEventPayload::RuntimeNotification {
                runtime_id,
                notification,
            });
        }
    }

    fn unregister_runtime(&mut self, runtime_id: &str) {
        if self.runtimes.remove(runtime_id).is_none() {
            return;
        }

        self.broadcast_event(CodexdEventPayload::RuntimeRemoved {
            runtime_id: runtime_id.to_string(),
        });
    }

    fn project_pending_repair(&self, runtime: &mut RuntimeSnapshot) {
        if self
            .runtime_connections
            .get(&runtime.runtime_id)
            .is_some_and(|slot| slot.requested_state.is_some())
        {
            // Keep the accepted turn details, but don't certify cached owners to a new observer.
            runtime.work_activity_supported = true;
            runtime.work_activity = None;
        }
    }

    fn broadcast_event(&mut self, mut payload: CodexdEventPayload) {
        if let CodexdEventPayload::RuntimeUpsert { runtime } = &mut payload {
            self.project_pending_repair(runtime);
        }
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

    while let Ok(Some(line)) = lines.next_line().await {
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
            CODEXD_HELLO_METHOD => handle_hello_method(Arc::clone(&state)).await,
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
            RUNTIME_REGISTER_METHOD
            | RUNTIME_UPDATE_METADATA_METHOD
            | RUNTIME_UPDATE_STATE_METHOD
            | RUNTIME_EVENT_METHOD
            | RUNTIME_UNREGISTER_METHOD => {
                let runtime_id = params
                    .as_ref()
                    .and_then(|p| p.get("runtimeId"))
                    .and_then(JsonValue::as_str)
                    .map(str::to_owned);
                let mut daemon = state.lock().await;
                let result =
                    daemon.producer_message(connection_id, &mut owned_runtime_ids, &method, params);
                if result.is_err()
                    && matches!(
                        method.as_str(),
                        RUNTIME_UPDATE_STATE_METHOD | RUNTIME_EVENT_METHOD
                    )
                    && let Some(id) = runtime_id
                    && let Some(request) = daemon.request_replacement(connection_id, id)
                    && request_id.is_none()
                    && let Some(line) =
                        encode_notification(REQUEST_STATE_METHOD, serde_json::json!(request))
                {
                    let _ = writer_tx.send(line);
                }
                result
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
        if state
            .runtime_connections
            .get(&runtime_id)
            .is_some_and(|slot| slot.connection == connection_id)
        {
            state.runtime_connections.remove(&runtime_id);
            state.unregister_runtime(&runtime_id);
        }
    }
}

async fn handle_snapshot_method(state: Arc<Mutex<DaemonState>>) -> Result<JsonValue, String> {
    let state = state.lock().await;
    serde_json::to_value(state.snapshot()).map_err(|err| err.to_string())
}

async fn handle_hello_method(state: Arc<Mutex<DaemonState>>) -> Result<JsonValue, String> {
    let state = state.lock().await;
    serde_json::to_value(state.hello()).map_err(|err| err.to_string())
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

impl DaemonState {
    fn request_replacement(&mut self, connection: u64, id: String) -> Option<RequestState> {
        let slot = self.runtime_connections.get_mut(&id)?;
        if slot.connection != connection
            || slot
                .requested_state
                .as_ref()
                .is_some_and(|sent| sent.producer_generation.is_some() || slot.observed.is_none())
        {
            return None;
        }
        let (producer_generation, minimum_revision) = slot
            .observed
            .as_ref()
            .map_or((None, 0), |(g, revision)| (Some(g.clone()), *revision));
        let request = RequestState {
            runtime_id: id,
            producer_generation,
            minimum_revision,
        };
        let first = slot.requested_state.replace(request.clone()).is_none();
        if first && let Some(runtime) = self.runtimes.get(&request.runtime_id) {
            let runtime = runtime.as_snapshot(request.runtime_id.clone());
            self.broadcast_event(CodexdEventPayload::RuntimeUpsert { runtime });
        }
        Some(request)
    }

    fn producer_message(
        &mut self,
        connection: u64,
        owned: &mut HashSet<String>,
        method: &str,
        params: Option<JsonValue>,
    ) -> Result<JsonValue, String> {
        let id = params
            .as_ref()
            .and_then(|p| p.get("runtimeId"))
            .and_then(JsonValue::as_str)
            .filter(|id| valid_id(id))
            .ok_or("missing runtimeId")?
            .to_owned();
        let owner = self.runtime_connections.get(&id).map(|s| s.connection);
        if owner.is_some_and(|n| n > connection)
            || (owned.contains(&id) && owner != Some(connection))
        {
            return Err("superseded producer connection".into());
        }
        let work = params.as_ref().and_then(|p| p.get("workActivity"));
        let generation = work
            .and_then(|w| w.get("producerGeneration"))
            .and_then(JsonValue::as_str)
            .filter(|g| valid_id(g));
        // Remember the cursor even when the very first publication is unusable, without
        // exposing it as an empty runtime. Closing a superseded socket must not remove it.
        let slot = self.runtime_connections.entry(id.clone()).or_default();
        if owner != Some(connection) {
            slot.connection = connection;
            slot.requested_state = None;
            if slot
                .observed
                .as_ref()
                .is_some_and(|(g, _)| Some(g.as_str()) != generation)
            {
                slot.observed = None;
            }
        }
        owned.insert(id.clone());
        if let Some(work) = work.filter(|w| !w.is_null())
            && matches!(method, RUNTIME_UPDATE_STATE_METHOD | RUNTIME_EVENT_METHOD)
        {
            let generation = generation.ok_or("invalid workActivity producerGeneration")?;
            let revision = work
                .get("revision")
                .and_then(JsonValue::as_u64)
                .filter(|r| i64::try_from(*r).is_ok())
                .ok_or("invalid workActivity revision")?;
            if let Some((prior, highest)) = &slot.observed {
                if generation != prior {
                    return Err("producer generation changed on a live connection".into());
                }
                let applied_event = method == RUNTIME_EVENT_METHOD
                    && self
                        .runtimes
                        .get(&id)
                        .and_then(|r| r.work_activity.as_ref())
                        .is_some_and(|w| {
                            w.producer_generation == generation && w.revision == revision
                        });
                if revision < *highest || applied_event {
                    if method == RUNTIME_EVENT_METHOD
                        && self.runtimes.contains_key(&id)
                        && let Some(notification) = archive_detail(params.as_ref())
                    {
                        self.broadcast_event(CodexdEventPayload::RuntimeArchiveDetail {
                            runtime_id: id,
                            notification,
                        });
                    } else if method == RUNTIME_UPDATE_STATE_METHOD
                        && slot
                            .requested_state
                            .as_ref()
                            .is_some_and(|request| request.minimum_revision < *highest)
                    {
                        slot.requested_state = None;
                        return Err("replacement predates the observed revision".into());
                    }
                    return Ok(serde_json::json!({}));
                }
            }
            slot.observed = Some((generation.to_owned(), revision));
        }
        match method {
            RUNTIME_REGISTER_METHOD => {
                let registration = deserialize_params(params)?;
                if owner != Some(connection) {
                    self.runtimes.remove(&id);
                }
                self.upsert_runtime_from_register(registration);
            }
            RUNTIME_UPDATE_METADATA_METHOD => {
                self.update_runtime_metadata(deserialize_params(params)?)
            }
            RUNTIME_UPDATE_STATE_METHOD => {
                let has_turns = params
                    .as_ref()
                    .is_some_and(|p| p.get("activeTurns").is_some());
                let state: RuntimeSnapshot = deserialize_params(params)?;
                if !has_turns && (state.work_activity_supported || state.work_activity.is_some()) {
                    return Err("missing negotiated activeTurns".into());
                }
                if state.work_activity_supported && state.work_activity.is_none() {
                    return Err("missing negotiated workActivity".into());
                }
                if let Some(work) = &state.work_activity {
                    work.validate()?;
                    if state
                        .active_turns
                        .iter()
                        .any(|turn| !valid_id(&turn.thread_id) || !valid_id(&turn.turn_id))
                    {
                        return Err("negotiated activeTurns contains an unidentified turn".into());
                    }
                    if work.items.is_empty()
                        && state.active_turns.iter().any(|t| {
                            !matches!(
                                t.status.as_deref(),
                                Some("completed" | "failed" | "paused" | "interrupted")
                            )
                        })
                    {
                        return Err("idle workActivity conflicts with reported turns".into());
                    }
                }
                if slot.observed.is_some() && state.work_activity.is_none() {
                    return Err("missing negotiated workActivity".into());
                }
                if let Some(prior) = self
                    .runtimes
                    .get(&id)
                    .and_then(|r| r.work_activity.as_ref())
                    && state.work_activity.as_ref().map(|w| &w.producer_generation)
                        != Some(&prior.producer_generation)
                {
                    self.runtimes.remove(&id);
                }
                slot.requested_state = None;
                self.update_runtime_state(state);
            }
            RUNTIME_EVENT_METHOD => {
                let mut event: RuntimeEventParams = deserialize_params(params)?;
                if let Some(work) = &event.work_activity {
                    work.validate()?;
                }
                if slot.requested_state.is_some() {
                    if !self.runtimes.contains_key(&id)
                        || event.notification.method == "turn/started"
                    {
                        return Ok(serde_json::json!({}));
                    }
                    // Preserve valid terminal and item detail, not a power transition based
                    // on a delta. Only a complete replacement closes the repair.
                    event.work_activity = None;
                }
                self.apply_runtime_notification(event);
            }
            RUNTIME_UNREGISTER_METHOD => {
                self.unregister_runtime(&id);
                self.runtime_connections.remove(&id);
                owned.remove(&id);
                return Ok(serde_json::json!({}));
            }
            _ => unreachable!(),
        }
        Ok(serde_json::json!({}))
    }
}

fn archive_detail(params: Option<&JsonValue>) -> Option<HubNotification> {
    let notification: HubNotification =
        serde_json::from_value(params?.get("notification")?.clone()).ok()?;
    if !matches!(
        notification.method.as_str(),
        "item/started" | "item/completed" | "turn/progressTrace"
    ) {
        return None;
    }
    let fields = notification.params.as_ref()?;
    let named = |field| {
        fields
            .get(field)
            .and_then(JsonValue::as_str)
            .is_some_and(|s| !s.trim().is_empty())
    };
    (named("turnKey") || (named("threadId") && named("turnId"))).then_some(notification)
}

pub(crate) fn apply_notification_to_runtime(
    runtime: &mut RuntimeState,
    notification: &HubNotification,
) -> bool {
    if let Some(mut snapshot) = parse_active_turn_started(notification) {
        let turn_key = snapshot
            .turn_key
            .clone()
            .unwrap_or_else(|| turn_key(snapshot.thread_id.as_str(), snapshot.turn_id.as_str()));
        if snapshot.token_usage_baseline.is_none() {
            snapshot.token_usage_baseline = runtime
                .thread_token_usage
                .get(snapshot.thread_id.as_str())
                .cloned();
        }
        if let Some(token_usage) = snapshot.token_usage.clone() {
            runtime
                .thread_token_usage
                .insert(snapshot.thread_id.clone(), token_usage);
        }
        runtime.active_turns.insert(turn_key, snapshot);
        return true;
    }

    if let Some(completion) = parse_active_turn_completed(notification) {
        return remove_completed_turn(runtime, completion);
    }

    if let Some(update) = parse_turn_context_update(notification) {
        record_thread_token_usage(runtime, &update);
        return apply_turn_context_update(runtime, update);
    }

    if let Some(update) = parse_thread_token_usage_update(notification) {
        record_thread_token_usage(runtime, &update);
        return apply_turn_context_update(runtime, update);
    }

    false
}

fn record_thread_token_usage(runtime: &mut RuntimeState, update: &TurnContextUpdate) {
    if let (Some(thread_id), Some(token_usage)) = (&update.thread_id, &update.token_usage) {
        runtime
            .thread_token_usage
            .insert(thread_id.clone(), token_usage.clone());
    }
}

struct TurnCompletion {
    turn_key: Option<String>,
    thread_id: Option<String>,
    turn_id: String,
}

struct TurnContextUpdate {
    turn_key: Option<String>,
    thread_id: Option<String>,
    turn_id: Option<String>,
    status: Option<String>,
    model: Option<String>,
    latest_label: Option<String>,
    scope: Option<String>,
    task_kind: Option<String>,
    session_source: Option<String>,
    sub_agent_source: Option<String>,
    parent_thread_id: Option<String>,
    parent_turn_id: Option<String>,
    model_provider: Option<String>,
    thinking_level: Option<String>,
    cwd: Option<String>,
    approval: Option<String>,
    sandbox: Option<String>,
    model_context_window: Option<i64>,
    context_remaining_percent: Option<i64>,
    token_usage: Option<JsonValue>,
    thread_name: Option<String>,
}

fn parse_active_turn_started(notification: &HubNotification) -> Option<ActiveTurnSnapshot> {
    if notification.method != "turn/started" {
        return None;
    }

    let params = notification.params.as_ref()?.as_object()?;
    let thread_id = params
        .get("threadId")?
        .as_str()
        .filter(|s| valid_id(s))?
        .to_owned();
    let turn = params.get("turn")?.as_object()?;
    let turn_id = turn.get("id")?.as_str().filter(|s| valid_id(s))?.to_owned();
    let turn_key = projected_key(
        turn.get("key").and_then(JsonValue::as_str),
        &thread_id,
        &turn_id,
    );

    Some(ActiveTurnSnapshot {
        turn_key: Some(turn_key),
        thread_id,
        turn_id,
        status: parse_string_field(turn, "status").or_else(|| Some("inProgress".to_string())),
        started_at: parse_i64_field(turn, "startedAt"),
        model: parse_string_field(turn, "model"),
        scope: parse_string_field(turn, "scope"),
        task_kind: parse_string_field(turn, "taskKind"),
        session_source: parse_string_field(turn, "sessionSource"),
        sub_agent_source: parse_string_field(turn, "subAgentSource"),
        parent_thread_id: parse_string_field(turn, "parentThreadId"),
        parent_turn_id: parse_string_field(turn, "parentTurnId"),
        model_provider: parse_string_field(turn, "modelProvider"),
        thinking_level: parse_string_field(turn, "thinkingLevel"),
        cwd: parse_string_field(turn, "cwd"),
        approval: parse_string_field(turn, "approval"),
        sandbox: parse_string_field(turn, "sandbox"),
        model_context_window: parse_i64_field(turn, "modelContextWindow"),
        context_remaining_percent: parse_i64_field(turn, "contextRemainingPercent"),
        token_usage: turn.get("tokenUsage").cloned(),
        token_usage_baseline: turn.get("tokenUsageBaseline").cloned(),
        thread_name: parse_string_field(turn, "threadName"),
        latest_label: parse_string_field(turn, "latestLabel"),
    })
}

fn parse_active_turn_completed(notification: &HubNotification) -> Option<TurnCompletion> {
    if notification.method != "turn/completed" {
        return None;
    }

    let params = notification.params.as_ref()?.as_object()?;
    let thread_id = params
        .get("threadId")
        .and_then(JsonValue::as_str)
        .map(ToString::to_string);
    let turn = params.get("turn")?.as_object()?;
    let turn_id = turn.get("id")?.as_str()?.to_string();
    let turn_key = parse_string_field(turn, "key")
        .or_else(|| params.get("turnKey")?.as_str().map(ToString::to_string));

    Some(TurnCompletion {
        turn_key,
        thread_id,
        turn_id,
    })
}

fn parse_turn_context_update(notification: &HubNotification) -> Option<TurnContextUpdate> {
    if !matches!(
        notification.method.as_str(),
        "turn/contextUpdated" | "turn/stateUpdated"
    ) {
        return None;
    }

    let params = notification.params.as_ref()?.as_object()?;
    Some(TurnContextUpdate {
        turn_key: parse_string_field(params, "turnKey"),
        thread_id: parse_string_field(params, "threadId"),
        turn_id: parse_string_field(params, "turnId"),
        status: parse_string_field(params, "status"),
        model: parse_string_field(params, "model"),
        latest_label: parse_string_field(params, "latestLabel"),
        scope: parse_string_field(params, "scope"),
        task_kind: parse_string_field(params, "taskKind"),
        session_source: parse_string_field(params, "sessionSource"),
        sub_agent_source: parse_string_field(params, "subAgentSource"),
        parent_thread_id: parse_string_field(params, "parentThreadId"),
        parent_turn_id: parse_string_field(params, "parentTurnId"),
        model_provider: parse_string_field(params, "modelProvider"),
        thinking_level: parse_string_field(params, "thinkingLevel"),
        cwd: parse_string_field(params, "cwd"),
        approval: parse_string_field(params, "approval"),
        sandbox: parse_string_field(params, "sandbox"),
        model_context_window: parse_i64_field(params, "modelContextWindow"),
        context_remaining_percent: parse_i64_field(params, "contextRemainingPercent"),
        token_usage: params.get("tokenUsage").cloned(),
        thread_name: parse_string_field(params, "threadName"),
    })
}

fn parse_thread_token_usage_update(notification: &HubNotification) -> Option<TurnContextUpdate> {
    if notification.method != "thread/tokenUsage/updated" {
        return None;
    }

    let params = notification.params.as_ref()?.as_object()?;
    let token_usage = params.get("tokenUsage").cloned();
    let model_context_window = token_usage
        .as_ref()
        .and_then(|usage| usage.as_object())
        .and_then(|usage| parse_i64_field(usage, "modelContextWindow"));

    Some(TurnContextUpdate {
        turn_key: parse_string_field(params, "turnKey"),
        thread_id: parse_string_field(params, "threadId"),
        turn_id: parse_string_field(params, "turnId"),
        status: None,
        model: None,
        latest_label: None,
        scope: None,
        task_kind: None,
        session_source: None,
        sub_agent_source: None,
        parent_thread_id: None,
        parent_turn_id: None,
        model_provider: None,
        thinking_level: None,
        cwd: None,
        approval: None,
        sandbox: None,
        model_context_window,
        context_remaining_percent: parse_i64_field(params, "contextRemainingPercent"),
        token_usage,
        thread_name: None,
    })
}

fn remove_completed_turn(runtime: &mut RuntimeState, completion: TurnCompletion) -> bool {
    let key = resolve_active_key(
        runtime,
        completion.turn_key,
        completion.thread_id.as_deref(),
        Some(&completion.turn_id),
    );
    key.is_some_and(|key| runtime.active_turns.remove(&key).is_some())
}

fn resolve_active_key(
    runtime: &RuntimeState,
    key: Option<String>,
    thread: Option<&str>,
    id: Option<&str>,
) -> Option<String> {
    if let Some(key) = key {
        return Some(key);
    }
    let id = id?;
    if let Some(thread) = thread {
        let key = turn_key(thread, id);
        if runtime.active_turns.contains_key(&key) {
            return Some(key);
        }
    }
    let mut matches = runtime
        .active_turns
        .iter()
        .filter(|(_, turn)| turn.turn_id == id && thread.is_none_or(|t| t == turn.thread_id));
    let (key, _) = matches.next()?;
    matches.next().is_none().then(|| key.clone())
}

fn apply_turn_context_update(runtime: &mut RuntimeState, update: TurnContextUpdate) -> bool {
    let key = resolve_active_key(
        runtime,
        update.turn_key,
        update.thread_id.as_deref(),
        update.turn_id.as_deref(),
    );
    let Some(key) = key else {
        return false;
    };
    let Some(turn) = runtime.active_turns.get_mut(&key) else {
        return false;
    };

    merge_opt(&mut turn.status, update.status);
    merge_opt(&mut turn.model, update.model);
    merge_opt(&mut turn.latest_label, update.latest_label);
    merge_opt(&mut turn.scope, update.scope);
    merge_opt(&mut turn.task_kind, update.task_kind);
    merge_opt(&mut turn.session_source, update.session_source);
    merge_opt(&mut turn.sub_agent_source, update.sub_agent_source);
    merge_opt(&mut turn.parent_thread_id, update.parent_thread_id);
    merge_opt(&mut turn.parent_turn_id, update.parent_turn_id);
    merge_opt(&mut turn.model_provider, update.model_provider);
    merge_opt(&mut turn.thinking_level, update.thinking_level);
    merge_opt(&mut turn.cwd, update.cwd);
    merge_opt(&mut turn.approval, update.approval);
    merge_opt(&mut turn.sandbox, update.sandbox);
    merge_opt(&mut turn.model_context_window, update.model_context_window);
    merge_opt(
        &mut turn.context_remaining_percent,
        update.context_remaining_percent,
    );
    merge_opt(&mut turn.token_usage, update.token_usage);
    merge_opt(&mut turn.thread_name, update.thread_name);
    true
}

fn merge_opt<T>(target: &mut Option<T>, update: Option<T>) {
    if update.is_some() {
        *target = update;
    }
}

fn parse_string_field(object: &serde_json::Map<String, JsonValue>, field: &str) -> Option<String> {
    object
        .get(field)
        .and_then(JsonValue::as_str)
        .map(ToString::to_string)
}

fn parse_i64_field(object: &serde_json::Map<String, JsonValue>, field: &str) -> Option<i64> {
    object.get(field).and_then(JsonValue::as_i64)
}

fn turn_key(thread_id: &str, turn_id: &str) -> String {
    format!("{thread_id}:{turn_id}")
}

fn valid_id(value: &str) -> bool {
    !value.is_empty() && value == value.trim()
}

fn projected_key(explicit: Option<&str>, thread: &str, turn: &str) -> String {
    explicit
        .map(str::trim)
        .filter(|key| !key.is_empty())
        .map_or_else(|| turn_key(thread, turn), str::to_owned)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anonymous_starts_never_enter_the_projection_and_stale_detail_is_archive_only() {
        use serde_json::json;
        let mut daemon = DaemonState::default();
        let mut owner = HashSet::new();
        let mut local = RuntimeState::default();
        let work =
            json!({"producerGeneration":"g","revision":8,"items":["queued"],"runningTurnKeys":[]});
        for (thread, turn) in [("T", ""), (" \n", "0")] {
            let start = HubNotification {
                method: "turn/started".into(),
                params: Some(json!({"threadId":thread,"turn":{"id":turn}})),
            };
            assert!(!apply_notification_to_runtime(&mut local, &start));
            let invalid = json!({"runtimeId":"r","activeTurns":[{"threadId":thread,"turnId":turn}],"workActivity":work});
            assert!(
                daemon
                    .producer_message(1, &mut owner, RUNTIME_UPDATE_STATE_METHOD, Some(invalid))
                    .is_err()
            );
        }
        assert!(daemon.snapshot().runtimes.is_empty());
        daemon
            .producer_message(
                1,
                &mut owner,
                RUNTIME_UPDATE_STATE_METHOD,
                Some(json!({"runtimeId":"r","activeTurns":[],"workActivity":work})),
            )
            .unwrap();
        let before = daemon.snapshot();
        for revision in [7, 8] {
            let evt = json!({"runtimeId":"r","workActivity":{"producerGeneration":"g","revision":revision,"items":[]},
                "notification":{"method":"item/completed","params":{"turnKey":"A:0"}}});
            daemon
                .producer_message(1, &mut owner, RUNTIME_EVENT_METHOD, Some(evt))
                .unwrap();
        }
        assert_eq!(daemon.snapshot().runtimes, before.runtimes);
        let kinds: Vec<_> = daemon
            .recent_events
            .iter()
            .filter(|(seq, _)| *seq > before.seq)
            .map(|(_, line)| {
                let msg: JsonValue = serde_json::from_str(line).unwrap();
                msg["params"]["event"]["type"].clone()
            })
            .collect();
        assert_eq!(
            kinds,
            [json!("runtimeArchiveDetail"), json!("runtimeArchiveDetail")]
        );
    }

    #[tokio::test]
    async fn quiet_notification_producer_repairs_first_and_live_rejections_on_the_same_socket() {
        use crate::producer::CodexdProducerClient;
        use crate::producer::RuntimeMetadata;
        use serde_json::json;
        use tokio::net::unix::OwnedReadHalf;
        use tokio::net::unix::OwnedWriteHalf;
        use tokio::sync::watch;
        use tokio::time::Duration;
        use tokio::time::timeout;

        async fn write(tx: &mut OwnedWriteHalf, msg: &JsonValue) {
            tx.write_all(format!("{msg}\n").as_bytes()).await.unwrap();
        }
        async fn read(rx: &mut tokio::io::Lines<BufReader<OwnedReadHalf>>) -> JsonValue {
            let line = timeout(Duration::from_secs(2), rx.next_line())
                .await
                .expect("wire message")
                .unwrap()
                .unwrap();
            serde_json::from_str(&line).unwrap()
        }
        // Relay one socket without generating producer actions. Only the malformed messages
        // and the read-only barrier are injected; the production outbox must originate repairs.
        let path = PathBuf::from(format!("/tmp/cdx-{}.sock", uuid::Uuid::new_v4()));
        let listener = UnixListener::bind(&path).unwrap();
        let (_owner, work) = watch::channel(BTreeMap::from([("owner".into(), Some("B:1".into()))]));
        let client = CodexdProducerClient::spawn_with_socket_path(
            path.clone(),
            RuntimeMetadata {
                runtime_id: "r".into(),
                pid: None,
                session_source: None,
                cwd: None,
                display_name: None,
            },
            work,
        );
        let (producer, _) = listener.accept().await.unwrap();
        tokio::fs::remove_file(path).await.unwrap();
        let (p_rx, mut p_tx) = producer.into_split();
        let mut p_rx = BufReader::new(p_rx).lines();
        let (relay, daemon) = UnixStream::pair().unwrap();
        let state = Arc::new(Mutex::new(DaemonState::default()));
        let handler = tokio::spawn(handle_connection(Arc::clone(&state), daemon));
        let (d_rx, mut d_tx) = relay.into_split();
        let mut d_rx = BufReader::new(d_rx).lines();
        let initial = read(&mut p_rx).await;
        assert!(initial.get("id").is_none());
        let barrier = json!({"id":17,"method":"codexd/snapshot"});
        let mut good = initial;
        for invalid in ["", " \n"] {
            let mut queued = good.clone();
            queued["params"]["workActivity"]["producerGeneration"] = invalid.into();
            queued["params"]["workActivity"]["revision"] = 999.into();
            write(&mut d_tx, &queued).await;
        }
        write(&mut d_tx, &barrier).await;
        let unbound = read(&mut d_rx).await;
        assert_eq!(unbound["method"], "codexd/runtime/requestState");
        assert!(unbound["params"]["producerGeneration"].is_null());
        assert_eq!(unbound["params"]["minimumRevision"], 0);
        assert_eq!(read(&mut d_rx).await["result"]["runtimes"], json!([]));
        write(&mut p_tx, &unbound).await;
        good = read(&mut p_rx).await;
        assert!(
            !good["params"]["workActivity"]["producerGeneration"]
                .as_str()
                .unwrap()
                .is_empty()
        );
        assert!(good["params"]["workActivity"]["revision"].as_u64().unwrap() < 999);
        // The producer answered. Now damage the reply before forwarding it.
        for first in [true, false] {
            let before = state.lock().await.snapshot();
            let floor = good["params"]["workActivity"]["revision"].as_u64().unwrap() + 7;
            let mut invalid = good.clone();
            invalid["params"]["workActivity"]["revision"] = floor.into();
            if first {
                invalid["params"]
                    .as_object_mut()
                    .unwrap()
                    .remove("activeTurns");
            } else {
                invalid["method"] = RUNTIME_EVENT_METHOD.into();
                invalid["params"]["notification"] =
                    json!({"method":"turn/completed","params":{"threadId":"B","turn":{"id":"1"}}});
                invalid["params"]["workActivity"]["items"] = json!([]);
            }
            write(&mut d_tx, &invalid).await;
            write(&mut d_tx, &invalid).await;
            let mut stale = good.clone();
            stale["params"]["workActivity"] = json!({"producerGeneration":good["params"]["workActivity"]["producerGeneration"],
                "revision":floor-1,"items":[],"runningTurnKeys":[]});
            write(&mut d_tx, &stale).await;
            write(&mut d_tx, &barrier).await;
            let repair = read(&mut d_rx).await;
            assert_eq!(repair["method"], "codexd/runtime/requestState");
            assert_eq!(repair["params"]["minimumRevision"], floor);
            assert!(repair.get("id").is_none());
            let mut pending = before.clone();
            if !first {
                pending.seq += 1;
                pending.runtimes[0].work_activity = None;
                assert!(pending.runtimes[0].work_activity_supported);
            }
            assert_eq!(read(&mut d_rx).await["result"], json!(pending));
            if !first {
                let daemon = state.lock().await;
                assert_eq!(
                    daemon.runtimes["r"].work_activity,
                    before.runtimes[0].work_activity
                );
                let (_, frame) = daemon.recent_events.back().unwrap();
                let frame: JsonValue = serde_json::from_str(frame).unwrap();
                assert_eq!(
                    frame["params"]["event"]["runtime"],
                    json!(pending.runtimes[0])
                );
                drop(daemon);
                let mut detail = stale.clone();
                detail["method"] = RUNTIME_EVENT_METHOD.into();
                for (revision, kind) in [
                    (floor - 1, "commandExecution"),
                    (floor - 1, "fileChange"),
                    (floor, "commandExecution"),
                ] {
                    detail["params"]["workActivity"]["revision"] = revision.into();
                    detail["params"]["notification"] = json!({"method":"item/completed","params":{
                        "turnId":"1","turnKey":"A:1","item":{"type":kind}}});
                    write(&mut d_tx, &detail).await;
                }
                write(&mut d_tx, &barrier).await;
                let while_waiting = read(&mut d_rx).await;
                assert_eq!(while_waiting["result"]["runtimes"], json!(pending.runtimes));
                let daemon = state.lock().await;
                let detail: Vec<_> = daemon
                    .recent_events
                    .iter()
                    .filter(|(seq, _)| *seq > pending.seq)
                    .map(|(_, line)| {
                        let msg: JsonValue = serde_json::from_str(line).unwrap();
                        let evt = &msg["params"]["event"];
                        json!([evt["type"], evt["notification"]["params"]["item"]["type"]])
                    })
                    .collect();
                assert_eq!(
                    detail,
                    [
                        json!(["runtimeArchiveDetail", "commandExecution"]),
                        json!(["runtimeArchiveDetail", "fileChange"]),
                        json!(["runtimeNotification", "commandExecution"])
                    ]
                );
            }
            write(&mut p_tx, &repair).await;
            good = read(&mut p_rx).await;
            assert_eq!(good["method"], RUNTIME_UPDATE_STATE_METHOD);
            assert!(good.get("id").is_none());
            assert!(good["params"]["workActivity"]["revision"].as_u64().unwrap() >= floor);
            write(&mut d_tx, &good).await;
            write(&mut d_tx, &barrier).await;
            let after = read(&mut d_rx).await;
            assert_eq!(
                after["result"]["runtimes"][0]["workActivity"]["items"],
                json!(["owner"])
            );
            assert_eq!(
                after["result"]["seq"],
                pending.seq + 1 + 3 * u64::from(!first)
            );
        }
        client.shutdown().await;
        drop(d_tx);
        handler.await.unwrap().unwrap();
    }

    #[test]
    fn connected_late_start_never_reenters_work_but_archive_details_still_flow() {
        use serde_json::json;
        let mut daemon = DaemonState::default();
        let mut owned = HashSet::new();
        daemon.producer_message(1, &mut owned, RUNTIME_UPDATE_STATE_METHOD, Some(json!({
            "runtimeId":"r","activeTurns":[{"threadId":"B","turnId":"1"}],
            "workActivity":{"producerGeneration":"g","revision":7,"items":["owner"],"runningTurnKeys":["B:1"]}
        }))).unwrap();
        let work = daemon.snapshot().runtimes[0].work_activity.clone().unwrap();
        for method in ["turn/started", "item/completed", "turn/completed"] {
            daemon.producer_message(1, &mut owned, RUNTIME_EVENT_METHOD, Some(json!({
                "runtimeId":"r", "notification":{"method":method,"params":{"threadId":"A","turn":{"id":"1"}}}
            }))).unwrap();
        }
        let snapshot = daemon.snapshot();
        assert_eq!(snapshot.runtimes[0].active_turns[0].thread_id, "B");
        assert_eq!(snapshot.runtimes[0].work_activity.as_ref(), Some(&work));
        let methods: Vec<_> = daemon
            .recent_events
            .iter()
            .filter_map(|(_, line)| {
                let msg: JsonValue = serde_json::from_str(line).unwrap();
                msg["params"]["event"]["notification"]["method"]
                    .as_str()
                    .map(str::to_owned)
            })
            .collect();
        assert_eq!(methods, ["item/completed", "turn/completed"]);
    }

    #[test]
    fn capable_replacement_requires_turns_and_prunes_only_unowned_projections() {
        let mut daemon = DaemonState::default();
        let mut owned = HashSet::new();
        let state = serde_json::json!({
            "runtimeId":"r", "workActivitySupported":true,
            "activeTurns":[{"threadId":"A","turnId":"1"},{"threadId":"B","turnId":"1"}],
            "workActivity":{"producerGeneration":"g","revision":5,
                            "items":["B-task","A-queued"],"runningTurnKeys":["B:1"]}
        });
        daemon
            .producer_message(
                1,
                &mut owned,
                RUNTIME_UPDATE_STATE_METHOD,
                Some(state.clone()),
            )
            .unwrap();
        let before = daemon.snapshot();
        assert_eq!(before.runtimes[0].active_turns.len(), 1);
        assert_eq!(before.runtimes[0].active_turns[0].thread_id, "B");
        let mut missing = state;
        missing["workActivity"]["revision"] = 8.into();
        missing.as_object_mut().unwrap().remove("activeTurns");
        assert!(
            daemon
                .producer_message(
                    1,
                    &mut owned,
                    RUNTIME_UPDATE_STATE_METHOD,
                    Some(missing.clone())
                )
                .is_err()
        );
        assert_eq!(daemon.snapshot(), before);
        let mut idle = missing.clone();
        idle["workActivity"]["revision"] = 6.into();
        idle["workActivity"]["items"] = serde_json::json!([]);
        idle["workActivity"]["runningTurnKeys"] = serde_json::json!([]);
        idle["activeTurns"] = serde_json::json!([]);
        daemon
            .producer_message(1, &mut owned, RUNTIME_UPDATE_STATE_METHOD, Some(idle))
            .unwrap();
        assert_eq!(daemon.snapshot(), before);
        missing["activeTurns"] = serde_json::json!([]);
        daemon
            .producer_message(1, &mut owned, RUNTIME_UPDATE_STATE_METHOD, Some(missing))
            .unwrap();
        let queued = daemon.snapshot();
        assert!(queued.runtimes[0].active_turns.is_empty());
        assert!(
            !queued.runtimes[0]
                .work_activity
                .as_ref()
                .unwrap()
                .items
                .is_empty()
        );
    }

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

    #[test]
    fn metadata_cannot_recertify_owners_while_a_replacement_is_pending() {
        use serde_json::json;

        let mut daemon = DaemonState::default();
        let mut owned = HashSet::new();
        let live = json!({"runtimeId":"r", "activeTurns":[], "workActivity":{
            "producerGeneration":"g", "revision":1, "items":["queued"]}});
        daemon
            .producer_message(
                1,
                &mut owned,
                RUNTIME_UPDATE_STATE_METHOD,
                Some(live.clone()),
            )
            .unwrap();
        assert!(daemon.request_replacement(1, "r".into()).is_some());
        let pending_seq = daemon.seq;
        assert!(daemon.request_replacement(1, "r".into()).is_none());
        daemon
            .producer_message(
                1,
                &mut owned,
                RUNTIME_UPDATE_METADATA_METHOD,
                Some(json!({"runtimeId":"r","displayName":"renamed"})),
            )
            .unwrap();
        assert_eq!(daemon.seq, pending_seq + 1);
        let view = &daemon.snapshot().runtimes[0];
        assert!(view.work_activity_supported && view.work_activity.is_none());
        assert_eq!(view.display_name.as_deref(), Some("renamed"));
        let (_, frame) = daemon.recent_events.back().unwrap();
        let frame: JsonValue = serde_json::from_str(frame).unwrap();
        assert_eq!(frame["params"]["event"]["runtime"], json!(view));
        daemon
            .producer_message(1, &mut owned, RUNTIME_UPDATE_STATE_METHOD, Some(live))
            .unwrap();
        assert!(daemon.snapshot().runtimes[0].work_activity.is_some());
    }

    #[test]
    fn hello_reports_protocol_capabilities_and_seq() {
        let mut state = DaemonState::default();
        let instance = state.hello().instance_id.unwrap();
        assert!(uuid::Uuid::parse_str(&instance).is_ok());
        assert_ne!(
            DaemonState::default().hello().instance_id.as_ref(),
            Some(&instance)
        );
        state.broadcast_event(CodexdEventPayload::RuntimeRemoved {
            runtime_id: "rt-1".to_string(),
        });

        assert_eq!(
            state.hello(),
            CodexdHelloResponse {
                protocol_version: PROTOCOL_VERSION,
                instance_id: Some(instance),
                capabilities: vec![
                    "eventReplay".to_string(),
                    "runtimeState".to_string(),
                    "activeTurnContext".to_string(),
                    "workActivity".to_string(),
                ],
                seq: 1,
            }
        );
    }

    #[test]
    fn update_state_replaces_active_turn_summary() {
        let mut state = DaemonState::default();
        state.update_runtime_state(RuntimeUpdateStateParams {
            work_activity_supported: false,
            work_activity: None,
            runtime_id: "rt-1".to_string(),
            pid: Some(123),
            session_source: Some("cli".to_string()),
            cwd: Some("/tmp/work".to_string()),
            display_name: Some("codex-tui".to_string()),
            active_turns: vec![ActiveTurnSnapshot {
                thread_id: "thread-1".to_string(),
                turn_id: "turn-1".to_string(),
                status: Some("inProgress".to_string()),
                started_at: Some(1_760_000_000),
                model: Some("gpt-5-codex".to_string()),
                latest_label: Some("Running tests".to_string()),
                ..Default::default()
            }],
        });

        state.update_runtime_state(RuntimeUpdateStateParams {
            work_activity_supported: false,
            work_activity: None,
            runtime_id: "rt-1".to_string(),
            pid: None,
            session_source: None,
            cwd: None,
            display_name: None,
            active_turns: vec![ActiveTurnSnapshot {
                thread_id: "thread-2".to_string(),
                turn_id: "turn-2".to_string(),
                status: Some("inProgress".to_string()),
                started_at: None,
                model: None,
                latest_label: Some("Reading files".to_string()),
                ..Default::default()
            }],
        });

        let snapshot = state.snapshot();
        assert_eq!(
            snapshot.runtimes,
            vec![RuntimeSnapshot {
                work_activity_supported: false,
                work_activity: None,
                runtime_id: "rt-1".to_string(),
                pid: Some(123),
                session_source: Some("cli".to_string()),
                cwd: Some("/tmp/work".to_string()),
                display_name: Some("codex-tui".to_string()),
                active_turns: vec![ActiveTurnSnapshot {
                    turn_key: Some("thread-2:turn-2".to_string()),
                    thread_id: "thread-2".to_string(),
                    turn_id: "turn-2".to_string(),
                    status: Some("inProgress".to_string()),
                    started_at: None,
                    model: None,
                    latest_label: Some("Reading files".to_string()),
                    ..Default::default()
                }],
            }]
        );
    }

    #[tokio::test]
    async fn superseded_socket_and_older_revision_cannot_restore_or_remove_work() {
        let mut state = DaemonState::default();
        let mut a = HashSet::new();
        let mut b = HashSet::new();
        let params = |revision, items: Vec<&str>| {
            Some(serde_json::json!({
                "runtimeId": "rt", "activeTurns": [], "workActivitySupported": true,
                "workActivity": {"producerGeneration": "g", "revision": revision, "items": items}
            }))
        };
        state
            .producer_message(
                1,
                &mut a,
                RUNTIME_UPDATE_STATE_METHOD,
                params(1, vec!["review"]),
            )
            .unwrap();
        state
            .producer_message(2, &mut b, RUNTIME_UPDATE_STATE_METHOD, params(3, vec![]))
            .unwrap();
        assert!(
            state
                .producer_message(
                    1,
                    &mut a,
                    RUNTIME_UPDATE_STATE_METHOD,
                    params(2, vec!["old"])
                )
                .is_err()
        );
        state
            .producer_message(
                2,
                &mut b,
                RUNTIME_UPDATE_STATE_METHOD,
                params(2, vec!["old"]),
            )
            .unwrap();
        let state = Arc::new(Mutex::new(state));
        cleanup_connection(Arc::clone(&state), 1, a, false).await;
        let work = state
            .lock()
            .await
            .snapshot()
            .runtimes
            .remove(0)
            .work_activity
            .unwrap();
        assert_eq!(work.revision, 3);
        assert!(work.items.is_empty());
    }

    #[test]
    fn repeating_a_complete_state_does_not_use_current_usage_as_its_own_baseline() {
        let mut state = DaemonState::default();
        let params: RuntimeSnapshot = serde_json::from_value(serde_json::json!({
            "runtimeId": "rt", "workActivitySupported": true,
            "workActivity": {"producerGeneration": "g", "revision": 1, "items": ["t:1"]},
            "activeTurns": [{"threadId":"t","turnId":"1","tokenUsage":{"total":{"totalTokens":42}}}]
        }))
        .unwrap();
        state.update_runtime_state(params.clone());
        state.update_runtime_state(params);
        assert_eq!(
            state.snapshot().runtimes[0].active_turns[0].token_usage_baseline,
            None
        );
    }

    #[test]
    fn runtime_notifications_key_active_turns_by_thread_and_turn() {
        let mut state = DaemonState::default();

        for thread_id in ["thread-a", "thread-b"] {
            state.apply_runtime_notification(RuntimeEventParams {
                work_activity: None,
                runtime_id: "rt-1".to_string(),
                notification: HubNotification {
                    method: "turn/started".to_string(),
                    params: Some(serde_json::json!({
                        "threadId": thread_id,
                        "turn": {
                            "id": "turn-1",
                            "status": "inProgress",
                            "model": format!("model-{thread_id}"),
                        },
                    })),
                },
            });
        }

        let snapshot = state.snapshot();
        assert_eq!(
            snapshot.runtimes[0].active_turns,
            vec![
                ActiveTurnSnapshot {
                    turn_key: Some("thread-a:turn-1".to_string()),
                    thread_id: "thread-a".to_string(),
                    turn_id: "turn-1".to_string(),
                    status: Some("inProgress".to_string()),
                    model: Some("model-thread-a".to_string()),
                    ..Default::default()
                },
                ActiveTurnSnapshot {
                    turn_key: Some("thread-b:turn-1".to_string()),
                    thread_id: "thread-b".to_string(),
                    turn_id: "turn-1".to_string(),
                    status: Some("inProgress".to_string()),
                    model: Some("model-thread-b".to_string()),
                    ..Default::default()
                },
            ]
        );

        state.apply_runtime_notification(RuntimeEventParams {
            work_activity: None,
            runtime_id: "rt-1".to_string(),
            notification: HubNotification {
                method: "turn/completed".to_string(),
                params: Some(serde_json::json!({
                    "threadId": "thread-b",
                    "turn": {
                        "id": "turn-1",
                        "status": "completed",
                    },
                })),
            },
        });

        assert_eq!(
            state.snapshot().runtimes[0].active_turns,
            vec![ActiveTurnSnapshot {
                turn_key: Some("thread-a:turn-1".to_string()),
                thread_id: "thread-a".to_string(),
                turn_id: "turn-1".to_string(),
                status: Some("inProgress".to_string()),
                model: Some("model-thread-a".to_string()),
                ..Default::default()
            }]
        );
    }

    #[test]
    fn scoped_and_ambiguous_legacy_terminals_cannot_remove_another_child() {
        let mut runtime = RuntimeState::default();
        let publish = |r: &mut RuntimeState, method, params| {
            apply_notification_to_runtime(
                r,
                &HubNotification {
                    method: String::from(method),
                    params: Some(params),
                },
            )
        };
        for thread in ["A", "B"] {
            publish(
                &mut runtime,
                "turn/started",
                serde_json::json!({"threadId":thread,"turn":{"id":"0"}}),
            );
        }
        assert!(!publish(
            &mut runtime,
            "turn/completed",
            serde_json::json!({"turn":{"id":"0"}})
        ));
        let done = serde_json::json!({"threadId":"A","turn":{"id":"0"}});
        assert!(publish(&mut runtime, "turn/completed", done.clone()));
        assert!(!publish(&mut runtime, "turn/completed", done));
        assert!(!publish(
            &mut runtime,
            "turn/contextUpdated",
            serde_json::json!({"threadId":"A","turnId":"0","model":"other"})
        ));
        assert_eq!(runtime.active_turns.keys().collect::<Vec<_>>(), ["B:0"]);
        assert!(publish(
            &mut runtime,
            "turn/completed",
            serde_json::json!({"turn":{"id":"0"}})
        ));
    }

    #[test]
    fn stale_completion_key_does_not_remove_reused_child_turn_id() {
        let mut state = DaemonState::default();
        state.apply_runtime_notification(RuntimeEventParams {
            work_activity: None,
            runtime_id: "rt-1".to_string(),
            notification: HubNotification {
                method: "turn/started".to_string(),
                params: Some(serde_json::json!({
                    "threadId": "thread-new",
                    "turn": {
                        "id": "0",
                        "key": "thread-new:0",
                        "status": "inProgress",
                        "taskKind": "post_turn_completion_review",
                    },
                })),
            },
        });

        state.apply_runtime_notification(RuntimeEventParams {
            work_activity: None,
            runtime_id: "rt-1".to_string(),
            notification: HubNotification {
                method: "turn/completed".to_string(),
                params: Some(serde_json::json!({
                    "turn": {
                        "id": "0",
                        "key": "stale-thread:0",
                        "status": "completed",
                    },
                })),
            },
        });

        assert_eq!(
            state.snapshot().runtimes[0].active_turns,
            vec![ActiveTurnSnapshot {
                turn_key: Some("thread-new:0".to_string()),
                thread_id: "thread-new".to_string(),
                turn_id: "0".to_string(),
                status: Some("inProgress".to_string()),
                task_kind: Some("post_turn_completion_review".to_string()),
                ..Default::default()
            }]
        );
    }

    #[test]
    fn runtime_context_updates_active_turn_and_broadcasts_snapshot() {
        let mut state = DaemonState::default();
        state.apply_runtime_notification(RuntimeEventParams {
            work_activity: None,
            runtime_id: "rt-1".to_string(),
            notification: HubNotification {
                method: "turn/started".to_string(),
                params: Some(serde_json::json!({
                    "threadId": "thread-1",
                    "turn": {
                        "id": "turn-1",
                        "key": "thread-1:turn-1",
                        "status": "inProgress",
                        "scope": "delegate",
                        "taskKind": "review",
                        "sessionSource": "subagent_review",
                        "subAgentSource": "review",
                        "parentThreadId": "parent-thread",
                        "parentTurnId": "parent-turn",
                        "model": "delegate-model",
                        "modelProvider": "delegate-provider",
                        "thinkingLevel": "high",
                        "cwd": "/tmp/delegate",
                        "approval": "never",
                        "sandbox": "read-only",
                        "modelContextWindow": 100000,
                    },
                })),
            },
        });
        let seq_before_update = state.seq;

        state.apply_runtime_notification(RuntimeEventParams {
            work_activity: None,
            runtime_id: "rt-1".to_string(),
            notification: HubNotification {
                method: "thread/tokenUsage/updated".to_string(),
                params: Some(serde_json::json!({
                    "threadId": "thread-1",
                    "turnId": "turn-1",
                    "turnKey": "thread-1:turn-1",
                    "contextRemainingPercent": 42,
                    "tokenUsage": {
                        "total": { "totalTokens": 90000 },
                        "last": { "totalTokens": 12000 },
                        "modelContextWindow": 100000,
                    },
                })),
            },
        });

        assert_eq!(state.seq, seq_before_update + 2);
        let snapshot = state.snapshot();
        assert_eq!(
            snapshot.runtimes[0].active_turns,
            vec![ActiveTurnSnapshot {
                turn_key: Some("thread-1:turn-1".to_string()),
                thread_id: "thread-1".to_string(),
                turn_id: "turn-1".to_string(),
                status: Some("inProgress".to_string()),
                model: Some("delegate-model".to_string()),
                scope: Some("delegate".to_string()),
                task_kind: Some("review".to_string()),
                session_source: Some("subagent_review".to_string()),
                sub_agent_source: Some("review".to_string()),
                parent_thread_id: Some("parent-thread".to_string()),
                parent_turn_id: Some("parent-turn".to_string()),
                model_provider: Some("delegate-provider".to_string()),
                thinking_level: Some("high".to_string()),
                cwd: Some("/tmp/delegate".to_string()),
                approval: Some("never".to_string()),
                sandbox: Some("read-only".to_string()),
                model_context_window: Some(100000),
                context_remaining_percent: Some(42),
                token_usage: Some(serde_json::json!({
                    "total": { "totalTokens": 90000 },
                    "last": { "totalTokens": 12000 },
                    "modelContextWindow": 100000,
                })),
                ..Default::default()
            }]
        );

        let upsert_line = &state.recent_events[state.recent_events.len() - 2].1;
        let upsert: JsonValue = serde_json::from_str(upsert_line).expect("upsert json");
        assert_eq!(
            upsert["params"]["event"]["type"],
            JsonValue::String("runtimeUpsert".to_string())
        );
        assert_eq!(
            upsert["params"]["event"]["runtime"]["activeTurns"][0]["contextRemainingPercent"],
            JsonValue::from(42)
        );

        let notification_line = &state.recent_events[state.recent_events.len() - 1].1;
        let notification: JsonValue =
            serde_json::from_str(notification_line).expect("notification json");
        assert_eq!(
            notification["params"]["event"]["type"],
            JsonValue::String("runtimeNotification".to_string())
        );
    }

    #[test]
    fn active_turn_snapshot_carries_prior_thread_token_usage_baseline() {
        let mut state = DaemonState::default();
        let prior_usage = serde_json::json!({
            "total": {
                "inputTokens": 30000,
                "cachedInputTokens": 8000,
                "outputTokens": 4000,
                "reasoningOutputTokens": 1000,
                "totalTokens": 43000,
            },
            "last": {
                "inputTokens": 14000,
                "cachedInputTokens": 3000,
                "outputTokens": 1200,
                "reasoningOutputTokens": 300,
                "totalTokens": 15500,
            },
            "modelContextWindow": 128000,
        });
        let current_usage = serde_json::json!({
            "total": {
                "inputTokens": 90000,
                "cachedInputTokens": 21000,
                "outputTokens": 10000,
                "reasoningOutputTokens": 4500,
                "totalTokens": 125500,
            },
            "last": {
                "inputTokens": 34000,
                "cachedInputTokens": 7000,
                "outputTokens": 3000,
                "reasoningOutputTokens": 1000,
                "totalTokens": 39000,
            },
            "modelContextWindow": 128000,
        });

        state.apply_runtime_notification(RuntimeEventParams {
            work_activity: None,
            runtime_id: "rt-1".to_string(),
            notification: HubNotification {
                method: "thread/tokenUsage/updated".to_string(),
                params: Some(serde_json::json!({
                    "threadId": "thread-1",
                    "turnId": "turn-1",
                    "turnKey": "thread-1:turn-1",
                    "tokenUsage": prior_usage,
                })),
            },
        });
        state.apply_runtime_notification(RuntimeEventParams {
            work_activity: None,
            runtime_id: "rt-1".to_string(),
            notification: HubNotification {
                method: "turn/started".to_string(),
                params: Some(serde_json::json!({
                    "threadId": "thread-1",
                    "turn": {
                        "id": "turn-2",
                        "key": "thread-1:turn-2",
                        "status": "inProgress",
                    },
                })),
            },
        });
        state.apply_runtime_notification(RuntimeEventParams {
            work_activity: None,
            runtime_id: "rt-1".to_string(),
            notification: HubNotification {
                method: "thread/tokenUsage/updated".to_string(),
                params: Some(serde_json::json!({
                    "threadId": "thread-1",
                    "turnId": "turn-2",
                    "turnKey": "thread-1:turn-2",
                    "tokenUsage": current_usage,
                })),
            },
        });

        assert_eq!(
            state.snapshot().runtimes[0].active_turns,
            vec![ActiveTurnSnapshot {
                turn_key: Some("thread-1:turn-2".to_string()),
                thread_id: "thread-1".to_string(),
                turn_id: "turn-2".to_string(),
                status: Some("inProgress".to_string()),
                model_context_window: Some(128000),
                token_usage: Some(serde_json::json!({
                    "total": {
                        "inputTokens": 90000,
                        "cachedInputTokens": 21000,
                        "outputTokens": 10000,
                        "reasoningOutputTokens": 4500,
                        "totalTokens": 125500,
                    },
                    "last": {
                        "inputTokens": 34000,
                        "cachedInputTokens": 7000,
                        "outputTokens": 3000,
                        "reasoningOutputTokens": 1000,
                        "totalTokens": 39000,
                    },
                    "modelContextWindow": 128000,
                })),
                token_usage_baseline: Some(serde_json::json!({
                    "total": {
                        "inputTokens": 30000,
                        "cachedInputTokens": 8000,
                        "outputTokens": 4000,
                        "reasoningOutputTokens": 1000,
                        "totalTokens": 43000,
                    },
                    "last": {
                        "inputTokens": 14000,
                        "cachedInputTokens": 3000,
                        "outputTokens": 1200,
                        "reasoningOutputTokens": 300,
                        "totalTokens": 15500,
                    },
                    "modelContextWindow": 128000,
                })),
                ..Default::default()
            }]
        );
    }
}
