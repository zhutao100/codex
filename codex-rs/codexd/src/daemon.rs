use crate::protocol::ActiveTurnSnapshot;
use crate::protocol::CodexdEventEnvelope;
use crate::protocol::CodexdEventPayload;
use crate::protocol::CodexdHelloResponse;
use crate::protocol::CodexdSnapshotResponse;
use crate::protocol::CodexdSubscribeParams;
use crate::protocol::CodexdSubscribeResponse;
use crate::protocol::HubNotification;
use crate::protocol::ProducerState;
use crate::protocol::REQUEST_STATE_METHOD;
use crate::protocol::RequestState;
use crate::protocol::RuntimeEventParams;
use crate::protocol::RuntimeRegisterParams;
use crate::protocol::RuntimeSnapshot;
use crate::protocol::RuntimeUpdateMetadataParams;
use crate::protocol::RuntimeUpdateStateParams;
use crate::protocol::WorkActivity;
use anyhow::Context;
use serde::Deserialize;
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

#[cfg(test)]
mod reconnect_tests;

#[derive(Debug, Clone, Default)]
pub(crate) struct RuntimeState {
    pid: Option<u32>,
    session_source: Option<String>,
    cwd: Option<String>,
    display_name: Option<String>,
    pub(crate) active_turns: BTreeMap<String, ActiveTurnSnapshot>,
    last_start_order: u64,
    pub(crate) work_activity: Option<WorkActivity>,
    thread_token_usage: BTreeMap<String, JsonValue>,
    associations: TurnAssociations,
}

#[derive(Debug, Clone, Default)]
struct TurnAssociations(VecDeque<ObservedTurn>);

#[derive(Debug, Clone)]
struct ObservedTurn {
    key: String,
    thread: String,
    id: String,
    // EOF preserves identity, not evidence that a missing turn completed.
    predecessor: bool,
}

fn context_estimate(usage: &JsonValue, field: &str) -> bool {
    let counts = &usage[field];
    let Some(amount) = counts["totalTokens"].as_u64() else {
        return false;
    };
    let categories = [
        "inputTokens",
        "cachedInputTokens",
        "outputTokens",
        "reasoningOutputTokens",
    ];
    // Only the exact full-window total is synthetic. `last` can independently be a context estimate.
    amount > 0
        && !categories
            .iter()
            .any(|k| counts[*k].as_u64().is_some_and(|n| n > 0))
        && (usage["modelContextWindow"].as_u64() == Some(amount)
            || (field == "last" && categories.iter().any(|k| counts.get(*k).is_some())))
}

fn cumulative_total(usage: &JsonValue) -> Option<u64> {
    usage["total"]["totalTokens"]
        .as_u64()
        .filter(|_| !context_estimate(usage, "total"))
}

fn turn_usage(incoming: Option<JsonValue>, previous: Option<&JsonValue>) -> Option<JsonValue> {
    let Some(mut usage) = incoming else {
        return previous.cloned();
    };
    if !usage.is_object()
        || context_estimate(&usage, "total")
        || (cumulative_total(&usage).is_none() && context_estimate(&usage, "last"))
    {
        return previous.cloned();
    }
    if cumulative_total(&usage).is_none()
        && let Some(prior) = previous.filter(|p| cumulative_total(p).is_some())
    {
        usage["total"] = prior["total"].clone();
    }
    // Compaction can retain the measured lifetime but replace `last` with an estimate.
    if context_estimate(&usage, "last")
        && let Some(last) = previous
            .filter(|p| !context_estimate(p, "last"))
            .and_then(|p| p.get("last"))
    {
        usage["last"] = last.clone();
    }
    Some(usage)
}

fn observe_thread_usage(heads: &mut BTreeMap<String, JsonValue>, thread: &str, usage: &JsonValue) {
    if let Some(n) = cumulative_total(usage)
        && heads
            .get(thread)
            .and_then(cumulative_total)
            .is_none_or(|prior| n >= prior)
    {
        heads.insert(thread.to_owned(), starting_usage(usage));
    }
}

fn starting_usage(usage: &JsonValue) -> JsonValue {
    let mut measured = usage.clone();
    if context_estimate(usage, "last")
        && let Some(fields) = measured.as_object_mut()
    {
        fields.remove("last");
    }
    measured
}

fn observe_turn_usage(heads: &mut BTreeMap<String, JsonValue>, turn: &ActiveTurnSnapshot) {
    // A restored start can be our only measurement. Don't replace a real sample at the same total with a baseline.
    if let Some(start) = &turn.token_usage_baseline
        && heads.get(&turn.thread_id).and_then(cumulative_total) < cumulative_total(start)
    {
        observe_thread_usage(heads, &turn.thread_id, start);
    }
    if let Some(usage) = &turn.token_usage {
        observe_thread_usage(heads, &turn.thread_id, usage);
    }
}

impl TurnAssociations {
    fn latest_key(&self, thread: &str) -> Option<&str> {
        self.0
            .iter()
            .rev()
            .find(|t| t.predecessor && t.thread == thread)
            .map(|t| t.key.as_str())
    }

    fn get(&self, key: &str) -> Option<&str> {
        self.0
            .iter()
            .find(|t| t.key == key)
            .map(|t| t.thread.as_str())
    }

    fn remember(&mut self, key: &str, thread: &str, id: &str, predecessor: bool) {
        if let Some(i) = self.0.iter().position(|t| t.key == key) {
            self.0.remove(i);
        } else if self.0.len() == crate::protocol::MAX_RECOVERED_TERMINALS {
            self.0.pop_front();
        }
        self.0.push_back(ObservedTurn {
            key: key.into(),
            thread: thread.into(),
            id: id.into(),
            predecessor,
        });
    }

    fn retire_once(&mut self, key: &str, thread: &str, id: &str) {
        if !self.0.iter().any(|t| t.key == key && t.predecessor) {
            self.remember(key, thread, id, true);
        }
    }
}

impl RuntimeState {
    fn disconnected_identities(&self) -> Option<(String, TurnAssociations)> {
        let generation = self.work_activity.as_ref()?.producer_generation.clone();
        let mut ids = self.associations.clone();
        for t in &mut ids.0 {
            t.predecessor = false;
        }
        for (key, t) in &self.active_turns {
            ids.remember(key, &t.thread_id, &t.turn_id, false);
        }
        Some((generation, ids))
    }

    // Ordinary app-server usage carries the pair, not the daemon key. Scope only the hub copy.
    pub(crate) fn scope_usage(&self, notification: &mut HubNotification) -> bool {
        if notification.method != "thread/tokenUsage/updated" {
            return true;
        }
        let Some(fields) = notification.params.as_mut() else {
            return true;
        };
        if !fields["turnKey"].is_null() {
            return true;
        }
        let (Some(thread), Some(id)) = (fields["threadId"].as_str(), fields["turnId"].as_str())
        else {
            return true;
        };
        let mut keys = self
            .active_turns
            .iter()
            .filter(|(_, t)| t.thread_id == thread && t.turn_id == id)
            .map(|(k, _)| k.as_str())
            .chain(
                self.associations
                    .0
                    .iter()
                    .filter(|t| t.thread == thread && t.id == id)
                    .map(|t| t.key.as_str()),
            );
        if let Some(first) = keys.next() {
            if keys.any(|k| k != first) {
                return false;
            }
            fields["turnKey"] = first.into();
        }
        true
    }

    fn remember_recovered_threads(
        &mut self,
        state: &RuntimeUpdateStateParams,
        recovered: &[(String, HubNotification)],
    ) {
        for (key, terminal) in recovered {
            let Some(identity) = parse_active_turn_completed(terminal) else {
                continue;
            };
            if state
                .active_turns
                .iter()
                .any(|t| projected_key(t.turn_key.as_deref(), &t.thread_id, &t.turn_id) == *key)
                || self.scheduler_holds_terminal(state.work_activity.as_ref(), key, &identity)
            {
                continue;
            }
            if let Some(thread) = identity
                .thread_id
                .as_deref()
                .or_else(|| self.associations.get(key))
                .map(str::to_owned)
            {
                self.associations
                    .retire_once(key, &thread, &identity.turn_id);
            }
            if let Some(usage) = completion_usage(terminal) {
                self.record_usage(Some(key), identity.thread_id.as_deref(), usage);
            }
        }
    }

    fn prepare_active_turn(
        &self,
        mut turn: ActiveTurnSnapshot,
        infer_new_baseline: bool,
        overlapping: bool,
    ) -> (String, ActiveTurnSnapshot) {
        let key = projected_key(turn.turn_key.as_deref(), &turn.thread_id, &turn.turn_id);
        turn.turn_key = Some(key.clone());
        let previous = self.active_turns.get(&key);
        turn.token_usage = turn_usage(
            turn.token_usage.take(),
            previous.and_then(|t| t.token_usage.as_ref()),
        );
        if turn
            .token_usage_baseline
            .as_ref()
            .is_some_and(|u| context_estimate(u, "total"))
        {
            turn.token_usage_baseline = previous.and_then(|t| t.token_usage_baseline.clone());
            if let Some(previous) = previous {
                turn.token_usage_baseline_predecessor_key
                    .clone_from(&previous.token_usage_baseline_predecessor_key);
            }
        }
        turn.started_at = turn
            .started_at
            .or_else(|| previous.and_then(|t| t.started_at));
        // All previously unseen legacy turns in one full share an observation rank.
        // Iteration order is not evidence that one of them started later.
        turn.start_order = Some(
            turn.start_order
                .filter(|&n| n > 0)
                .or_else(|| previous.and_then(|t| t.start_order))
                .unwrap_or_else(|| self.last_start_order.saturating_add(1)),
        );
        let inferred = turn.token_usage_baseline_predecessor_key.is_some();
        let missing_origin =
            !turn.token_usage_baseline_known && !inferred && turn.token_usage_baseline.is_none();
        if let Some(previous) = previous {
            // A headerless replay is not a new start. Don't undo a later correction from
            // the same predecessor if an older projection of the start arrives.
            let fixed = previous.token_usage_baseline.is_some()
                && previous.token_usage_baseline_predecessor_key.is_none();
            let baseline =
                |t: &ActiveTurnSnapshot| t.token_usage_baseline.as_ref().and_then(cumulative_total);
            let stale = inferred
                && turn.token_usage_baseline_predecessor_key
                    == previous.token_usage_baseline_predecessor_key
                && baseline(&turn) < baseline(previous);
            if missing_origin || (inferred && !turn.token_usage_baseline_known && fixed) || stale {
                turn.token_usage_baseline
                    .clone_from(&previous.token_usage_baseline);
                turn.token_usage_baseline_predecessor_key
                    .clone_from(&previous.token_usage_baseline_predecessor_key);
            }
        } else if missing_origin && !overlapping {
            if infer_new_baseline {
                turn.token_usage_baseline = self.thread_token_usage.get(&turn.thread_id).cloned();
            }
            turn.token_usage_baseline_predecessor_key = self
                .associations
                .latest_key(&turn.thread_id)
                .filter(|p| *p != key)
                .map(str::to_owned);
        }
        turn.token_usage_baseline_known = true;
        (key, turn)
    }

    // Bind a legacy terminal before its upsert removes the only identifiable
    // turn. The producer journal, daemon ledger and subscriber see the same key.
    pub(crate) fn identify_live_completion(
        &self,
        notification: &mut HubNotification,
    ) -> Option<String> {
        let identity = parse_active_turn_completed(notification)?;
        let thread = identity.thread_id.as_deref();
        let key = resolve_active_key(self, identity.turn_key, thread, Some(&identity.turn_id))
            .or_else(|| {
                let thread = thread?;
                // A threaded missed start can still name history; an ambiguous or
                // wholly unscoped short ID cannot.
                (!self
                    .active_turns
                    .values()
                    .any(|t| t.thread_id == thread && t.turn_id == identity.turn_id))
                .then(|| turn_key(thread, &identity.turn_id))
            })?;
        let fields = notification.params.as_mut()?;
        fields["turnKey"] = key.clone().into();
        if thread.is_none()
            && let Some(turn) = self.active_turns.get(&key)
            && turn.turn_id == identity.turn_id
        {
            fields["threadId"] = turn.thread_id.clone().into();
        }
        Some(key)
    }

    fn has_attached_terminal(&self, key: &str, notification: &HubNotification) -> bool {
        self.active_turns.contains_key(key)
            || parse_active_turn_completed(notification).is_some_and(|identity| {
                self.scheduler_holds_terminal(self.work_activity.as_ref(), key, &identity)
            })
    }

    fn scheduler_holds_terminal(
        &self,
        work: Option<&WorkActivity>,
        key: &str,
        id: &TurnIdentity,
    ) -> bool {
        let Some(owners) = work.and_then(|w| w.running_turn_keys.as_ref()) else {
            return false;
        };
        let thread = id
            .thread_id
            .as_deref()
            .or_else(|| {
                self.active_turns
                    .get(key)
                    .filter(|t| t.turn_id == id.turn_id)
                    .map(|t| t.thread_id.as_str())
            })
            .or_else(|| {
                self.associations
                    .0
                    .iter()
                    .find(|t| t.key == key && t.id == id.turn_id)
                    .map(|t| t.thread.as_str())
            });
        owners.contains(key)
            || thread.is_some_and(|thread| owners.contains(&turn_key(thread, &id.turn_id)))
    }

    fn record_usage(
        &mut self,
        key: Option<&str>,
        explicit_thread: Option<&str>,
        usage: &JsonValue,
    ) -> ProjectionChange {
        if !usage.is_object() {
            return ProjectionChange::None;
        }
        if let Some(turn) = key.and_then(|k| self.active_turns.get(k)) {
            observe_thread_usage(&mut self.thread_token_usage, &turn.thread_id, usage);
            return ProjectionChange::None;
        }
        let retired = key.and_then(|k| self.associations.get(k));
        let next = key.and_then(|key| {
            self.active_turns.values_mut().find(|t| {
                t.token_usage_baseline_predecessor_key.as_deref() == Some(key)
                    && retired
                        .or(explicit_thread)
                        .is_none_or(|owner| t.thread_id == owner)
            })
        });
        let history = retired.or_else(|| next.as_ref().map(|t| t.thread_id.as_str()));
        if let Some(thread) = history.or(explicit_thread) {
            observe_thread_usage(&mut self.thread_token_usage, thread, usage);
        }
        if let (Some(next), Some(n)) = (next, cumulative_total(usage))
            && next
                .token_usage_baseline
                .as_ref()
                .and_then(cumulative_total)
                .is_none_or(|p| n >= p)
        {
            let measured = starting_usage(usage);
            if next.token_usage_baseline.as_ref() != Some(&measured) {
                next.token_usage_baseline = Some(measured);
                return ProjectionChange::SuccessorBaseline;
            }
        }
        ProjectionChange::None
    }

    fn account_settled_terminal(&mut self, notification: &HubNotification) -> ProjectionChange {
        let Some(identity) = parse_active_turn_completed(notification) else {
            return ProjectionChange::None;
        };
        let Some(key) = identity.turn_key.as_deref() else {
            return ProjectionChange::None;
        };
        if let Some(thread) = identity
            .thread_id
            .as_deref()
            .or_else(|| self.associations.get(key))
            .map(str::to_owned)
        {
            self.associations
                .retire_once(key, &thread, &identity.turn_id);
        }
        completion_usage(notification).map_or(ProjectionChange::None, |u| {
            self.record_usage(Some(key), identity.thread_id.as_deref(), u)
        })
    }

    pub(crate) fn apply_scheduler_work(&mut self, work: WorkActivity) {
        self.work_activity = Some(work);
        self.prune_unowned_turns();
    }

    fn prune_unowned_turns(&mut self) {
        let Some(work) = &self.work_activity else {
            return;
        };
        self.active_turns.retain(|key, turn| {
            let keep = work.permits_turn(turn);
            if !keep {
                self.associations
                    .remember(key, &turn.thread_id, &turn.turn_id, true);
            }
            keep
        });
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
        let active_turns = self
            .active_turns
            .values()
            .cloned()
            .map(|mut t| {
                t.thread_token_usage = self
                    .thread_token_usage
                    .get(&t.thread_id)
                    .filter(|head| {
                        cumulative_total(head) > t.token_usage.as_ref().and_then(cumulative_total)
                    })
                    .cloned();
                t
            })
            .collect();

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
        if !valid_id(&self.producer_generation) || i64::try_from(self.revision).is_err() {
            return Err("invalid workActivity generation or revision".into());
        }
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
struct ConnectionClaims {
    owned: HashSet<String>,
    // A rejected capable challenger owes full work on this socket, not on the incumbent's.
    pending_work: HashSet<String>,
}

#[derive(Default)]
struct ProducerSlot {
    connection: u64,
    requires_work_activity: bool,
    cursors: RevisionCursors,
    socket_revision: Option<SocketRevision>,
    requested_state: Option<RequestState>,
    request_sent_on: Option<u64>,
    // At most one outcome per attached turn; not an owner or a visible completion.
    deferred_terminals: Vec<DeferredTerminal>,
    published_terminals: VecDeque<TerminalStamp>,
}

struct DeferredTerminal {
    stamp: TerminalStamp,
    notification: HubNotification,
}

struct TerminalStamp {
    key: String,
    generation: Option<String>,
    revision: Option<u64>,
}

#[derive(Default)]
struct RevisionCursors {
    accepted_generation: Option<String>,
    // Belongs only to the last disconnected source. Never restore its turns, totals or work.
    disconnected_identities: Option<(String, TurnAssociations)>,
    // Admission evidence for the daemon instance. It is not a live work projection.
    generations: HashMap<String, GenerationCursor>,
}

struct GenerationCursor {
    revision: u64,
    recovery_epoch: Option<u64>,
}

struct SocketRevision {
    connection: u64,
    generation: String,
    highest: u64,
}

impl RevisionCursors {
    fn floor(&self, generation: &str) -> Option<u64> {
        self.generations.get(generation).map(|c| c.revision)
    }

    fn observe(&mut self, generation: &str, revision: u64) {
        if let Some(cursor) = self.generations.get_mut(generation) {
            cursor.revision = cursor.revision.max(revision);
        } else {
            self.generations.insert(
                generation.to_owned(),
                GenerationCursor {
                    revision,
                    recovery_epoch: None,
                },
            );
        }
    }

    fn accept_recovery(&mut self, generation: &str, epoch: u64) -> bool {
        let Some(cursor) = self.generations.get_mut(generation) else {
            return false;
        };
        if cursor.recovery_epoch.is_some_and(|last| epoch <= last) {
            return false;
        }
        cursor.recovery_epoch = Some(epoch);
        true
    }

    fn request(&self, id: &str, generation: Option<&str>) -> RequestState {
        RequestState {
            runtime_id: id.to_owned(),
            producer_generation: generation.map(str::to_owned),
            minimum_revision: generation.and_then(|g| self.floor(g)).unwrap_or(0),
        }
    }
}

struct DetachedRevision {
    cursors: RevisionCursors,
    request: Option<RequestState>,
    negotiated_repair: bool,
    published_terminals: VecDeque<TerminalStamp>,
}

impl ProducerSlot {
    // One bounded publication ledger for ordinary, recovered and archive-only
    // outcomes. A full ownership snapshot is not a new turn history.
    fn record_published_terminal(&mut self, stamp: TerminalStamp) -> bool {
        let seen = &mut self.published_terminals;
        if let Some(i) = seen
            .iter()
            .position(|p| p.key == stamp.key && p.generation == stamp.generation)
        {
            if seen[i].revision >= stamp.revision {
                return false;
            }
            seen.remove(i);
        }
        if seen.len() == crate::protocol::MAX_RECOVERED_TERMINALS {
            seen.pop_front();
        }
        seen.push_back(stamp);
        true
    }

    fn terminal_stamp(&self, key: String, frame: Option<(&str, u64)>) -> TerminalStamp {
        TerminalStamp {
            key,
            generation: frame
                .map(|(g, _)| g.to_owned())
                .or_else(|| self.cursors.accepted_generation.clone()),
            revision: frame.map(|(_, r)| r),
        }
    }

    fn admit_live_terminal(&mut self, key: String, work: Option<&WorkActivity>) -> bool {
        let stamp = self.terminal_stamp(
            key,
            work.map(|w| (w.producer_generation.as_str(), w.revision)),
        );
        if self.deferred_terminals.iter().any(|p| {
            p.stamp.key == stamp.key
                && p.stamp.generation == stamp.generation
                && p.stamp.revision >= stamp.revision
        }) {
            return false;
        }
        // A genuinely unversioned legacy stream is FIFO.
        if stamp.revision.is_none() {
            return !self
                .published_terminals
                .iter()
                .any(|p| p.key == stamp.key && p.revision.is_some());
        }
        let key = stamp.key.clone();
        if !self.record_published_terminal(stamp) {
            return false;
        }
        self.deferred_terminals.retain(|p| p.stamp.key != key);
        true
    }
    fn current_socket(&self) -> Option<&SocketRevision> {
        self.socket_revision
            .as_ref()
            .filter(|s| s.connection == self.connection)
    }

    fn socket_request(&self, id: &str) -> RequestState {
        if let Some(s) = self.current_socket() {
            return RequestState {
                runtime_id: id.to_owned(),
                producer_generation: Some(s.generation.clone()),
                minimum_revision: s.highest,
            };
        }
        self.replacement_floor(id)
    }

    fn replacement_floor(&self, id: &str) -> RequestState {
        let generation = self
            .current_socket()
            .map(|s| s.generation.as_str())
            .or_else(|| {
                self.requested_state
                    .as_ref()?
                    .producer_generation
                    .as_deref()
            })
            .or(self.cursors.accepted_generation.as_deref());
        self.cursors.request(id, generation)
    }
}

#[derive(Default)]
struct DaemonState {
    instance_id: std::sync::OnceLock<uuid::Uuid>,
    next_connection_id: u64,
    seq: u64,
    runtimes: BTreeMap<String, RuntimeState>,
    runtime_connections: HashMap<String, ProducerSlot>,
    // Detached ordering evidence is never projected as a runtime or an owner.
    detached_revisions: HashMap<String, DetachedRevision>,
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
            let incoming: BTreeMap<_, _> = params
                .active_turns
                .into_iter()
                .map(|t| {
                    (
                        projected_key(t.turn_key.as_deref(), &t.thread_id, &t.turn_id),
                        t,
                    )
                })
                .collect();
            runtime.active_turns.retain(|key, turn| {
                let keep = incoming.contains_key(key);
                if !keep {
                    runtime
                        .associations
                        .remember(key, &turn.thread_id, &turn.turn_id, true);
                }
                keep
            });
            let mut running = HashMap::<String, usize>::new();
            for t in incoming.values().filter(|t| {
                unfinished(t)
                    && runtime
                        .work_activity
                        .as_ref()
                        .is_none_or(|w| w.permits_turn(t))
            }) {
                *running.entry(t.thread_id.clone()).or_default() += 1;
            }
            let mut thread_token_usage = runtime.thread_token_usage.clone();
            runtime.active_turns = incoming
                .into_values()
                .map(|turn| {
                    let overlapping = running.get(&turn.thread_id).copied().unwrap_or(0)
                        > usize::from(unfinished(&turn));
                    let (turn_key, mut turn) = runtime.prepare_active_turn(
                        turn,
                        runtime.work_activity.is_none(),
                        overlapping,
                    );
                    observe_turn_usage(&mut thread_token_usage, &turn);
                    if let Some(head) = turn.thread_token_usage.take() {
                        observe_thread_usage(&mut thread_token_usage, &turn.thread_id, &head);
                    }
                    (turn_key, turn)
                })
                .collect();
            runtime.thread_token_usage = thread_token_usage;
            if let Some(n) = runtime
                .active_turns
                .values()
                .filter_map(|t| t.start_order)
                .max()
            {
                runtime.last_start_order = runtime.last_start_order.max(n);
            }
            runtime.prune_unowned_turns();
            runtime.as_snapshot(runtime_id)
        };

        self.broadcast_event(CodexdEventPayload::RuntimeUpsert { runtime: snapshot });
    }

    fn apply_runtime_notification(&mut self, params: RuntimeEventParams) {
        let runtime_id = params.runtime_id;
        let mut notification = params.notification;
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
            let forward = match notification.method.as_str() {
                "turn/started" => effective_work.is_none_or(|work| {
                    parse_active_turn_started(&notification)
                        .is_some_and(|turn| work.permits_turn(&turn))
                }),
                "turn/completed" => runtime
                    .identify_live_completion(&mut notification)
                    .is_some_and(|key| {
                        self.runtime_connections
                            .get_mut(&runtime_id)
                            .is_none_or(|slot| slot.admit_live_terminal(key, effective_work))
                    }),
                "thread/tokenUsage/updated" => runtime.scope_usage(&mut notification),
                _ => true,
            };
            // A legacy terminal can end an attached turn even if its unversioned
            // archive detail must not overwrite an earlier versioned outcome.
            let project =
                forward || (effective_work.is_none() && notification.method == "turn/completed");
            let changed = project
                && apply_notification_to_runtime(runtime, &mut notification)
                    != ProjectionChange::None;
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
        let settled = match &mut payload {
            CodexdEventPayload::RuntimeUpsert { runtime } => {
                self.project_pending_repair(runtime);
                self.take_settled_terminals(&runtime.runtime_id, false)
            }
            CodexdEventPayload::RuntimeRemoved { runtime_id } => {
                self.take_settled_terminals(runtime_id, true)
            }
            _ => Vec::new(),
        };
        self.sequence_event(payload);
        for terminal in settled {
            self.sequence_event(terminal);
        }
    }

    fn take_settled_terminals(&mut self, id: &str, removed: bool) -> Vec<CodexdEventPayload> {
        let Some(slot) = self.runtime_connections.get_mut(id) else {
            return Vec::new();
        };
        if !removed && slot.requested_state.is_some() {
            return Vec::new();
        }
        let mut runtime = self.runtimes.get_mut(id);
        let mut released = Vec::new();
        // Deliver after the projection, never before. A different generation that
        // explicitly owns the same key cannot inherit the old task's terminal.
        for pending in std::mem::take(&mut slot.deferred_terminals) {
            if let Some(r) = runtime.as_deref()
                && r.has_attached_terminal(&pending.stamp.key, &pending.notification)
            {
                if r.work_activity.as_ref().map(|w| &w.producer_generation)
                    == pending.stamp.generation.as_ref()
                {
                    slot.deferred_terminals.push(pending);
                }
            } else {
                let same_source = runtime.as_deref().is_some_and(|r| {
                    r.work_activity.as_ref().map(|w| &w.producer_generation)
                        == pending.stamp.generation.as_ref()
                });
                if !slot.record_published_terminal(pending.stamp) {
                    continue;
                }
                let corrected = runtime
                    .as_deref_mut()
                    .filter(|_| same_source)
                    .and_then(|r| {
                        (r.account_settled_terminal(&pending.notification)
                            == ProjectionChange::SuccessorBaseline)
                            .then(|| r.as_snapshot(id.to_owned()))
                    });
                released.push(CodexdEventPayload::RuntimeArchiveDetail {
                    runtime_id: id.to_owned(),
                    notification: pending.notification,
                });
                if let Some(runtime) = corrected {
                    released.push(CodexdEventPayload::RuntimeUpsert { runtime });
                }
            }
        }
        released
    }

    fn sequence_event(&mut self, payload: CodexdEventPayload) {
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

    let mut claims = ConnectionClaims::default();
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

        let mut repair_notification = None;
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
                let result = daemon.producer_message(connection_id, &mut claims, &method, params);
                if let Some(id) = runtime_id {
                    if result.is_err()
                        && matches!(
                            method.as_str(),
                            RUNTIME_UPDATE_STATE_METHOD | RUNTIME_EVENT_METHOD
                        )
                    {
                        daemon.require_replacement(connection_id, &id);
                    }
                    if let Some(request) = result
                        .as_ref()
                        .ok()
                        .cloned()
                        .flatten()
                        .or_else(|| daemon.take_replacement_request(connection_id, &id))
                    {
                        repair_notification =
                            encode_notification(REQUEST_STATE_METHOD, serde_json::json!(request));
                    }
                }
                result.map(|_| serde_json::json!({}))
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
        if let Some(line) = repair_notification {
            let _ = writer_tx.send(line);
        }
    }

    cleanup_connection(state, connection_id, claims, subscribed).await;

    drop(writer_tx);
    let _ = writer_handle.await;

    Ok(())
}

async fn cleanup_connection(
    state: Arc<Mutex<DaemonState>>,
    connection_id: u64,
    claims: ConnectionClaims,
    subscribed: bool,
) {
    let mut state = state.lock().await;

    if subscribed {
        state.remove_subscriber(connection_id);
    }

    for runtime_id in claims.owned {
        if state
            .runtime_connections
            .get(&runtime_id)
            .is_some_and(|slot| slot.connection == connection_id)
        {
            state.detach_producer(&runtime_id);
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
    fn detach_producer(&mut self, id: &str) {
        let identities = self
            .runtimes
            .get(id)
            .and_then(RuntimeState::disconnected_identities);
        self.unregister_runtime(id);
        let Some(mut slot) = self.runtime_connections.remove(id) else {
            return;
        };
        if identities.is_some() {
            slot.cursors.disconnected_identities = identities;
        }
        let negotiated_repair = slot.requires_work_activity && slot.requested_state.is_some();
        if negotiated_repair
            || !slot.cursors.generations.is_empty()
            || !slot.published_terminals.is_empty()
        {
            self.detached_revisions.insert(
                id.to_owned(),
                DetachedRevision {
                    cursors: slot.cursors,
                    request: slot.requested_state,
                    negotiated_repair,
                    published_terminals: slot.published_terminals,
                },
            );
        }
    }

    fn require_replacement(&mut self, connection: u64, id: &str) {
        let Some(slot) = self.runtime_connections.get_mut(id) else {
            return;
        };
        // Reject malformed legacy frames without changing the last visible-turn projection.
        if slot.connection != connection
            || (!slot.requires_work_activity && slot.requested_state.is_none())
        {
            return;
        }
        let request = slot.socket_request(id);
        if slot.requested_state.as_ref() == Some(&request) {
            return;
        }
        let first = slot.requested_state.replace(request).is_none();
        slot.request_sent_on = None;
        if first && let Some(runtime) = self.runtimes.get(id) {
            let runtime = runtime.as_snapshot(id.to_owned());
            self.broadcast_event(CodexdEventPayload::RuntimeUpsert { runtime });
        }
    }

    fn take_replacement_request(&mut self, connection: u64, id: &str) -> Option<RequestState> {
        let slot = self.runtime_connections.get_mut(id)?;
        if slot.connection != connection || slot.request_sent_on == Some(connection) {
            return None;
        }
        slot.requested_state.as_ref()?;
        let request = slot.socket_request(id);
        slot.requested_state = Some(request.clone());
        slot.request_sent_on = Some(connection);
        Some(request)
    }

    fn producer_message(
        &mut self,
        connection: u64,
        claims: &mut ConnectionClaims,
        method: &str,
        params: Option<JsonValue>,
    ) -> Result<Option<RequestState>, String> {
        let id = params
            .as_ref()
            .and_then(|p| p.get("runtimeId"))
            .and_then(JsonValue::as_str)
            .filter(|id| valid_id(id))
            .ok_or("missing runtimeId")?
            .to_owned();
        let owner = self.runtime_connections.get(&id).map(|s| s.connection);
        // Connection IDs order accepts, not publications. Only a socket that
        // actually claimed this runtime can have been superseded.
        if claims.owned.contains(&id) && owner != Some(connection) {
            return Err("superseded producer connection".into());
        }
        if method == RUNTIME_UNREGISTER_METHOD && owner != Some(connection) {
            return Ok(None);
        }
        let work = params.as_ref().and_then(|p| p.get("workActivity"));
        let header = work
            .filter(|w| {
                !w.is_null() && matches!(method, RUNTIME_UPDATE_STATE_METHOD | RUNTIME_EVENT_METHOD)
            })
            .map(work_header);
        let declares_work = header.is_some()
            || (method == RUNTIME_UPDATE_STATE_METHOD
                && params
                    .as_ref()
                    .and_then(|p| p["workActivitySupported"].as_bool())
                    == Some(true));
        let mut admitted_state = None;
        if let Some(incumbent) = self.runtime_connections.get_mut(&id)
            && incumbent.connection != connection
        {
            // The first socket can still answer its repair even if nothing has
            // been published yet. Only a coherent full state transfers the slot.
            let challenger = header.as_ref().and_then(|h| h.as_ref().ok()).copied();
            if let Some((g, n)) = challenger {
                incumbent.cursors.observe(g, n);
            }
            if method == RUNTIME_UPDATE_STATE_METHOD {
                admitted_state = checked_producer_state(params.as_ref()).ok();
            }
            let eligible = admitted_state
                .as_ref()
                .is_some_and(|p| match &p.state.work_activity {
                    Some(w) => incumbent
                        .cursors
                        .floor(&w.producer_generation)
                        .is_none_or(|min| w.revision >= min),
                    None => {
                        !claims.pending_work.contains(&id)
                            && incumbent.requested_state.is_none()
                            && self
                                .runtimes
                                .get(&id)
                                .is_some_and(|r| r.work_activity.is_none())
                    }
                });
            if !eligible {
                if declares_work {
                    claims.pending_work.insert(id.clone());
                }
                let request = challenger.map_or_else(
                    || incumbent.replacement_floor(&id),
                    |(g, _)| incumbent.cursors.request(&id, Some(g)),
                );
                // During repair a contender may deliver keyed history, never an
                // ownership delta. Its request and EOF do not belong to the incumbent.
                if incumbent.requested_state.is_some() && method == RUNTIME_EVENT_METHOD {
                    self.publish_repair_detail(id, params.as_ref(), challenger);
                }
                return Ok(Some(request));
            }
        } else if owner.is_none()
            && method == RUNTIME_UPDATE_STATE_METHOD
            && !declares_work
            && !claims.pending_work.contains(&id)
            && self
                .detached_revisions
                .get(&id)
                .is_none_or(|p| p.request.is_none())
        {
            // An invalid first legacy full cannot acquire an otherwise unclaimed runtime.
            admitted_state = Some(checked_producer_state(params.as_ref())?);
        }
        // Remember the cursor even when the very first publication is unusable, without
        // exposing it as an empty runtime. Closing a superseded socket must not remove it.
        let slot = self.runtime_connections.entry(id.clone()).or_default();
        if slot.connection != connection {
            slot.requires_work_activity = false;
        }
        if let Some(cursor) = self.detached_revisions.remove(&id) {
            slot.cursors = cursor.cursors;
            slot.requested_state = cursor.request;
            slot.requires_work_activity = cursor.negotiated_repair;
            slot.published_terminals = cursor.published_terminals;
            // A fresh legacy registration/full state has no negotiated cursor. It
            // can replace a cleanly closed producer, but not bypass a pending repair.
            let legacy_handshake = work.is_none_or(JsonValue::is_null)
                && matches!(
                    method,
                    RUNTIME_REGISTER_METHOD | RUNTIME_UPDATE_STATE_METHOD
                );
            if slot.requested_state.is_none()
                && !slot.cursors.generations.is_empty()
                && !legacy_handshake
            {
                slot.requested_state = Some(slot.replacement_floor(&id));
            }
        }
        slot.connection = connection;
        claims.owned.insert(id.clone());
        if claims.pending_work.remove(&id) {
            slot.requires_work_activity = true;
            // The incumbent may have gone away before this challenger answered.
            if admitted_state.is_none() && slot.requested_state.is_none() {
                slot.requested_state = Some(slot.replacement_floor(&id));
            }
        }
        slot.requires_work_activity |= declares_work;
        if let Some(header) = header {
            let (generation, revision) = header?;
            let current = slot
                .socket_revision
                .as_ref()
                .filter(|s| s.connection == connection);
            if current.is_some_and(|s| s.generation != generation) {
                return Err("producer generation changed on a live connection".into());
            }
            // Seed this socket with the admission floor it inherited. Later
            // unaccepted challengers cannot raise this socket's own repair floor.
            let highest = current
                .map(|s| s.highest)
                .or_else(|| slot.cursors.floor(generation));
            slot.socket_revision = Some(SocketRevision {
                connection,
                generation: generation.to_owned(),
                highest: highest.unwrap_or(revision).max(revision),
            });
            slot.cursors.observe(generation, revision);
            let applied = self
                .runtimes
                .get(&id)
                .and_then(|r| r.work_activity.as_ref())
                .filter(|w| w.producer_generation == generation)
                .map(|w| w.revision);
            // A registration can be genuinely legacy. Once this socket advertises
            // work, neither a delta nor a stale first state is a negotiated baseline.
            if applied.is_none()
                && slot.requested_state.is_none()
                && (method == RUNTIME_EVENT_METHOD || highest.is_some_and(|min| revision < min))
            {
                self.require_replacement(connection, &id);
                if method == RUNTIME_EVENT_METHOD {
                    self.publish_repair_detail(id, params.as_ref(), Some((generation, revision)));
                }
                return Ok(None);
            }
            if let Some(highest) = highest {
                let applied_event = method == RUNTIME_EVENT_METHOD && applied == Some(revision);
                if revision < highest || applied_event {
                    if method == RUNTIME_EVENT_METHOD
                        && let Some(notification) = params
                            .as_ref()
                            .and_then(|p| p.get("notification"))
                            .and_then(|n| serde_json::from_value(n.clone()).ok())
                    {
                        self.publish_archive_detail(id, notification, Some((generation, revision)));
                    } else if method == RUNTIME_UPDATE_STATE_METHOD
                        && slot.requested_state.as_ref().is_some_and(|request| {
                            request.producer_generation.as_deref() == Some(generation)
                                && request.minimum_revision < highest
                        })
                    {
                        return Err("replacement predates the observed revision".into());
                    }
                    return Ok(None);
                }
            }
        }
        let detached_repair = slot.requested_state.is_some() && !self.runtimes.contains_key(&id);
        match method {
            RUNTIME_REGISTER_METHOD => {
                let registration = deserialize_params(params)?;
                if !detached_repair {
                    if owner != Some(connection) {
                        self.runtimes.remove(&id);
                    }
                    self.upsert_runtime_from_register(registration);
                }
            }
            RUNTIME_UPDATE_METADATA_METHOD => {
                let metadata = deserialize_params(params)?;
                if !detached_repair {
                    self.update_runtime_metadata(metadata);
                }
            }
            RUNTIME_UPDATE_STATE_METHOD => {
                let ProducerState {
                    state,
                    history_recovery,
                } = match admitted_state {
                    Some(full) => full,
                    None => checked_producer_state(params.as_ref())?,
                };
                // Legacy turns can be authoritative without resetting a previous
                // generation's revision floor. The cursor is not legacy work.
                if state.work_activity.is_none()
                    && (slot.requires_work_activity
                        || (slot.requested_state.is_some() && !slot.cursors.generations.is_empty()))
                {
                    return Err("missing negotiated workActivity".into());
                }
                let mut displaced_turns = false;
                if let Some(prior) = self.runtimes.get(&id) {
                    let old = prior.work_activity.as_ref().map(|w| &w.producer_generation);
                    let next = state.work_activity.as_ref().map(|w| &w.producer_generation);
                    let new_socket = owner.is_some_and(|old| old != connection);
                    if old != next || (next.is_none() && new_socket) {
                        displaced_turns = !prior.active_turns.is_empty();
                        // An in-place legacy registration can supply metadata before the first
                        // negotiated state. Never seed a successor's turns from the old source.
                        if new_socket || old.is_some() || displaced_turns {
                            self.runtimes.remove(&id);
                        }
                    }
                }
                slot.requested_state = None;
                if let Some((generation, identities)) = slot.cursors.disconnected_identities.take()
                    && state
                        .work_activity
                        .as_ref()
                        .is_some_and(|w| w.producer_generation == generation)
                {
                    self.runtimes.entry(id.clone()).or_default().associations = identities;
                }
                if let Some(work) = &state.work_activity {
                    slot.cursors.accepted_generation = Some(work.producer_generation.clone());
                }
                let recovery = history_recovery.filter(|h| {
                    state.work_activity.as_ref().is_some_and(|w| {
                        slot.cursors
                            .accept_recovery(&w.producer_generation, h.epoch)
                    })
                });
                // A subscriber joining during repair has turns but no incumbent generation.
                let history_lost = recovery.is_some() || displaced_turns;
                let mut recovered = Vec::new();
                if let Some(h) = recovery {
                    recovered.extend(
                        h.terminals
                            .into_iter()
                            .rev()
                            .take(crate::protocol::MAX_RECOVERED_TERMINALS)
                            .rev()
                            .filter_map(recovered_notification),
                    );
                    slot.deferred_terminals
                        .retain(|p| !recovered.iter().any(|(key, _)| key == &p.stamp.key));
                }
                if history_lost {
                    self.broadcast_event(CodexdEventPayload::RuntimeNotification {
                        runtime_id: id.clone(),
                        notification: HubNotification {
                            method: "runtime/historyLost".into(),
                            params: None,
                        },
                    });
                }
                let recovered_at = state
                    .work_activity
                    .as_ref()
                    .filter(|_| !recovered.is_empty())
                    .map(|w| (w.producer_generation.clone(), w.revision));
                if !recovered.is_empty() {
                    // Bind accounting before any successor in this same full state. Do not complete an attached turn.
                    self.runtimes
                        .entry(id.clone())
                        .or_default()
                        .remember_recovered_threads(&state, &recovered);
                }
                self.update_runtime_state(state);
                for (key, notification) in recovered {
                    let stamp = recovered_at.as_ref().map(|(g, n)| (g.as_str(), *n));
                    if !self.defer_attached_terminal(&id, &key, &notification, stamp)
                        && self.record_keyed_terminal(&id, key, stamp)
                    {
                        self.broadcast_event(CodexdEventPayload::RuntimeNotification {
                            runtime_id: id.clone(),
                            notification,
                        });
                    }
                }
            }
            RUNTIME_EVENT_METHOD => {
                let mut event: RuntimeEventParams = deserialize_params(params)?;
                if let Some(work) = &event.work_activity {
                    work.validate()?;
                }
                if slot.requested_state.is_some() {
                    if !self.runtimes.contains_key(&id)
                        || event.notification.method == "turn/completed"
                    {
                        let stamp = event
                            .work_activity
                            .as_ref()
                            .map(|w| (w.producer_generation.as_str(), w.revision));
                        self.publish_archive_detail(id, event.notification, stamp);
                        return Ok(None);
                    }
                    if event.notification.method == "turn/started" {
                        return Ok(None);
                    }
                    // Preserve valid terminal and item detail, not a power transition based
                    // on a delta. Only a complete replacement closes the repair.
                    event.work_activity = None;
                }
                if let Some(w) = &event.work_activity {
                    if self
                        .runtimes
                        .get(&id)
                        .and_then(|r| r.work_activity.as_ref())
                        .is_some_and(|old| old.producer_generation != w.producer_generation)
                    {
                        return Err("new producer generation requires a full state".into());
                    }
                    slot.cursors.accepted_generation = Some(w.producer_generation.clone());
                }
                self.apply_runtime_notification(event);
            }
            RUNTIME_UNREGISTER_METHOD => {
                self.detach_producer(&id);
                claims.owned.remove(&id);
            }
            _ => unreachable!(),
        }
        Ok(None)
    }

    fn publish_repair_detail(
        &mut self,
        id: String,
        params: Option<&JsonValue>,
        stamp: Option<(&str, u64)>,
    ) {
        if let Some(event) = params.and_then(|p| RuntimeEventParams::deserialize(p).ok())
            && event
                .work_activity
                .as_ref()
                .is_none_or(|w| w.validate().is_ok())
        {
            self.publish_archive_detail(id, event.notification, stamp);
        }
    }

    fn publish_archive_detail(
        &mut self,
        id: String,
        mut notification: HubNotification,
        stamp: Option<(&str, u64)>,
    ) {
        if let Some(terminal) = recovery_terminal(&notification) {
            if self
                .runtime_connections
                .get(&id)
                .is_none_or(|s| s.requested_state.is_none())
            {
                return;
            }
            // Repair requires a wire-scoped outcome; if a turn is attached,
            // its explicit key can differ from the thread:id fallback.
            let key = if let Some(runtime) = self.runtimes.get(&id) {
                let Some(key) = runtime.identify_live_completion(&mut notification) else {
                    return;
                };
                key
            } else {
                if let Some(fields) = &mut notification.params {
                    fields["turnKey"] = terminal.turn_key.clone().into();
                }
                terminal.turn_key
            };
            if self.defer_attached_terminal(&id, &key, &notification, stamp) {
                return;
            }
            if !self.record_keyed_terminal(&id, key, stamp) {
                return;
            }
        } else if !self.runtimes.contains_key(&id) || !keyed_item_or_progress(&notification) {
            return;
        }
        self.broadcast_event(CodexdEventPayload::RuntimeArchiveDetail {
            runtime_id: id,
            notification,
        });
    }

    fn record_keyed_terminal(&mut self, id: &str, key: String, frame: Option<(&str, u64)>) -> bool {
        self.runtime_connections.get_mut(id).is_some_and(|slot| {
            let stamp = slot.terminal_stamp(key, frame);
            slot.record_published_terminal(stamp)
        })
    }

    fn defer_attached_terminal(
        &mut self,
        id: &str,
        key: &str,
        notification: &HubNotification,
        stamp: Option<(&str, u64)>,
    ) -> bool {
        let Some(r) = self
            .runtimes
            .get(id)
            .filter(|r| r.has_attached_terminal(key, notification))
        else {
            return false;
        };
        let Some(slot) = self.runtime_connections.get_mut(id) else {
            return false;
        };
        let generation = r
            .work_activity
            .as_ref()
            .map(|w| w.producer_generation.as_str());
        if let (Some((g, _)), Some(attached)) = (stamp, generation)
            && g != attached
        {
            return true; // A reused key cannot end an attached turn of a different generation.
        }
        let revision = stamp.map(|(_, r)| r);
        let stamp = TerminalStamp {
            key: key.to_owned(),
            generation: generation.map(str::to_owned),
            revision,
        };
        if let Some(prior) = slot
            .deferred_terminals
            .iter_mut()
            .find(|p| p.stamp.key == key)
        {
            if prior.stamp.generation == stamp.generation && prior.stamp.revision >= revision {
                return true;
            }
            prior.stamp = stamp;
            prior.notification = notification.clone();
            return true;
        }
        slot.deferred_terminals.push(DeferredTerminal {
            stamp,
            notification: notification.clone(),
        });
        true
    }
}

fn work_header(work: &JsonValue) -> Result<(&str, u64), String> {
    let generation = work
        .get("producerGeneration")
        .and_then(JsonValue::as_str)
        .filter(|g| valid_id(g))
        .ok_or("invalid workActivity producerGeneration")?;
    let revision = work
        .get("revision")
        .and_then(JsonValue::as_u64)
        .filter(|r| i64::try_from(*r).is_ok())
        .ok_or("invalid workActivity revision")?;
    Ok((generation, revision))
}

fn checked_producer_state(params: Option<&JsonValue>) -> Result<ProducerState, String> {
    let params = params.ok_or("missing producer state")?;
    if !params.get("activeTurns").is_some_and(JsonValue::is_array) {
        return Err("activeTurns must be an explicit array".into());
    }
    let publication = ProducerState::deserialize(params).map_err(|err| err.to_string())?;
    let state = &publication.state;
    if state
        .active_turns
        .iter()
        .any(|t| !valid_id(&t.thread_id) || !valid_id(&t.turn_id))
    {
        return Err("activeTurns contains an unidentified turn".into());
    }
    if state.active_turns.iter().any(|t| {
        t.token_usage_baseline_predecessor_key
            .as_deref()
            .is_some_and(|p| {
                !valid_id(p) || p == projected_key(t.turn_key.as_deref(), &t.thread_id, &t.turn_id)
            })
    }) {
        return Err("invalid tokenUsageBaselinePredecessorKey".into());
    }
    if state.work_activity_supported && state.work_activity.is_none() {
        return Err("missing negotiated workActivity".into());
    }
    if let Some(work) = &state.work_activity {
        work.validate()?;
        if work.items.is_empty() && state.active_turns.iter().any(unfinished) {
            return Err("idle workActivity conflicts with reported turns".into());
        }
    }
    Ok(publication)
}

fn unfinished(turn: &ActiveTurnSnapshot) -> bool {
    !matches!(
        turn.status.as_deref(),
        Some("completed" | "failed" | "paused" | "interrupted")
    )
}

fn recovered_notification(
    t: crate::protocol::RecoveryTerminal,
) -> Option<(String, HubNotification)> {
    if !matches!(
        t.status.as_str(),
        "completed" | "failed" | "paused" | "interrupted"
    ) || !valid_id(&t.turn_key)
        || !valid_id(&t.turn_id)
    {
        return None;
    }
    let mut turn = serde_json::json!({"id":t.turn_id,"key":t.turn_key,"status":t.status});
    if let Some(usage) = t.token_usage {
        turn["tokenUsage"] = usage;
    }
    let notification = HubNotification {
        method: "turn/completed".into(),
        params: Some(serde_json::json!({"threadId":t.thread_id,"turnKey":t.turn_key,"turn":turn})),
    };
    Some((t.turn_key, notification))
}

fn keyed_item_or_progress(notification: &HubNotification) -> bool {
    if !matches!(
        notification.method.as_str(),
        "item/started" | "item/completed" | "turn/progressTrace"
    ) {
        return false;
    }
    let Some(fields) = notification.params.as_ref() else {
        return false;
    };
    let named = |field| {
        fields
            .get(field)
            .and_then(JsonValue::as_str)
            .is_some_and(|s| !s.trim().is_empty())
    };
    named("turnKey") || (named("threadId") && named("turnId"))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ProjectionChange {
    None,
    LiveTurn,
    SuccessorBaseline,
}

pub(crate) fn apply_notification_to_runtime(
    runtime: &mut RuntimeState,
    notification: &mut HubNotification,
) -> ProjectionChange {
    if let Some(snapshot) = parse_active_turn_started(notification) {
        let key = projected_key(
            snapshot.turn_key.as_deref(),
            &snapshot.thread_id,
            &snapshot.turn_id,
        );
        let overlap = runtime
            .active_turns
            .iter()
            .any(|(k, t)| k != &key && t.thread_id == snapshot.thread_id && unfinished(t));
        let (turn_key, snapshot) = runtime.prepare_active_turn(snapshot, true, overlap);
        if let Some(n) = snapshot.start_order {
            runtime.last_start_order = runtime.last_start_order.max(n);
        }
        observe_turn_usage(&mut runtime.thread_token_usage, &snapshot);
        // The daemon receiving a delta may have joined after the prior turn retired.
        // Carry the producer's start boundary on deltas as well as on full projections.
        if let Some(turn) = notification
            .params
            .as_mut()
            .and_then(|p| p["turn"].as_object_mut())
        {
            turn.insert("startOrder".into(), snapshot.start_order.into());
            if let Some(at) = snapshot.started_at {
                turn.insert("startedAt".into(), at.into());
            }
            turn.insert("tokenUsageBaselineKnown".into(), true.into());
            if let Some(value) = &snapshot.token_usage_baseline {
                turn.insert("tokenUsageBaseline".into(), value.clone());
            } else {
                turn.remove("tokenUsageBaseline");
            }
            if let Some(key) = &snapshot.token_usage_baseline_predecessor_key {
                turn.insert(
                    "tokenUsageBaselinePredecessorKey".into(),
                    key.clone().into(),
                );
            } else {
                turn.remove("tokenUsageBaselinePredecessorKey");
            }
        }
        runtime.active_turns.insert(turn_key, snapshot);
        return ProjectionChange::LiveTurn;
    }

    if let Some(completion) = parse_active_turn_completed(notification) {
        return remove_completed_turn(runtime, completion, completion_usage(notification));
    }

    if let Some(update) = parse_turn_context_update(notification)
        .or_else(|| parse_thread_token_usage_update(notification))
    {
        return apply_turn_context_update(runtime, update);
    }

    ProjectionChange::None
}

struct TurnIdentity {
    turn_key: Option<String>,
    thread_id: Option<String>,
    turn_id: String,
}

impl TurnIdentity {
    fn parse(
        params: &serde_json::Map<String, JsonValue>,
        turn: &serde_json::Map<String, JsonValue>,
    ) -> Option<Self> {
        let optional_id = |v: Option<&JsonValue>| match v {
            None | Some(JsonValue::Null) => Some(None),
            Some(v) => Some(Some(v.as_str().filter(|s| valid_id(s))?.to_owned())),
        };
        let key = turn
            .get("key")
            .filter(|v| !v.is_null())
            .or_else(|| params.get("turnKey"));
        Some(Self {
            turn_key: optional_id(key)?,
            thread_id: optional_id(params.get("threadId"))?,
            turn_id: turn.get("id")?.as_str().filter(|s| valid_id(s))?.to_owned(),
        })
    }
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
    let turn = params.get("turn")?.as_object()?;
    let identity = TurnIdentity::parse(params, turn)?;
    let thread_id = identity.thread_id?;
    let turn_id = identity.turn_id;
    let turn_key = identity
        .turn_key
        .unwrap_or_else(|| turn_key(&thread_id, &turn_id));

    Some(ActiveTurnSnapshot {
        turn_key: Some(turn_key),
        thread_id,
        turn_id,
        status: parse_string_field(turn, "status").or_else(|| Some("inProgress".to_string())),
        started_at: parse_i64_field(turn, "startedAt"),
        start_order: turn.get("startOrder").and_then(JsonValue::as_u64),
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
        thread_token_usage: None,
        token_usage_baseline: turn
            .get("tokenUsageBaseline")
            .filter(|v| !v.is_null())
            .cloned(),
        token_usage_baseline_known: turn
            .get("tokenUsageBaselineKnown")
            .and_then(JsonValue::as_bool)
            .unwrap_or(false),
        token_usage_baseline_predecessor_key: parse_string_field(
            turn,
            "tokenUsageBaselinePredecessorKey",
        ),
        thread_name: parse_string_field(turn, "threadName"),
        latest_label: parse_string_field(turn, "latestLabel"),
    })
}

fn parse_active_turn_completed(notification: &HubNotification) -> Option<TurnIdentity> {
    if notification.method != "turn/completed" {
        return None;
    }

    let params = notification.params.as_ref()?.as_object()?;
    let turn = params.get("turn")?.as_object()?;
    TurnIdentity::parse(params, turn)
}

pub(crate) fn recovery_terminal(
    notification: &HubNotification,
) -> Option<crate::protocol::RecoveryTerminal> {
    let completed = parse_active_turn_completed(notification)?;
    let key = completed.turn_key.or_else(|| {
        Some(turn_key(
            completed.thread_id.as_deref()?,
            &completed.turn_id,
        ))
    })?;
    let status = notification
        .params
        .as_ref()?
        .get("turn")?
        .get("status")?
        .as_str()?;
    matches!(status, "completed" | "failed" | "paused" | "interrupted").then(|| {
        crate::protocol::RecoveryTerminal {
            thread_id: completed.thread_id,
            turn_id: completed.turn_id,
            turn_key: key,
            status: status.to_owned(),
            token_usage: completion_usage(notification).cloned(),
        }
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

fn completion_usage(notification: &HubNotification) -> Option<&JsonValue> {
    notification
        .params
        .as_ref()?
        .get("turn")?
        .get("tokenUsage")
        .filter(|u| u.is_object())
}

fn remove_completed_turn(
    runtime: &mut RuntimeState,
    completion: TurnIdentity,
    usage: Option<&JsonValue>,
) -> ProjectionChange {
    let key = resolve_active_key(
        runtime,
        completion.turn_key,
        completion.thread_id.as_deref(),
        Some(&completion.turn_id),
    );
    let Some(key) = key else {
        return ProjectionChange::None;
    };
    let removed = runtime.active_turns.remove(&key);
    if let Some(turn) = &removed {
        runtime
            .associations
            .remember(&key, &turn.thread_id, &turn.turn_id, true);
    } else if let Some(thread) = completion.thread_id.as_deref() {
        runtime
            .associations
            .retire_once(&key, thread, &completion.turn_id);
    }
    let change = usage.map_or(ProjectionChange::None, |u| {
        runtime.record_usage(Some(&key), completion.thread_id.as_deref(), u)
    });
    if removed.is_some() {
        ProjectionChange::LiveTurn
    } else {
        change
    }
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

fn apply_turn_context_update(
    runtime: &mut RuntimeState,
    update: TurnContextUpdate,
) -> ProjectionChange {
    let key = resolve_active_key(
        runtime,
        update.turn_key,
        update.thread_id.as_deref(),
        update.turn_id.as_deref(),
    );
    let accounting = update
        .token_usage
        .as_ref()
        .map_or(ProjectionChange::None, |usage| {
            runtime.record_usage(key.as_deref(), update.thread_id.as_deref(), usage)
        });
    let Some(turn) = key.as_ref().and_then(|k| runtime.active_turns.get_mut(k)) else {
        return accounting;
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
    if let Some(usage) = update.token_usage {
        turn.token_usage = turn_usage(Some(usage), turn.token_usage.as_ref());
    }
    merge_opt(&mut turn.thread_name, update.thread_name);
    ProjectionChange::LiveTurn
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
        let mut owner = ConnectionClaims::default();
        let mut local = RuntimeState::default();
        let work =
            json!({"producerGeneration":"g","revision":8,"items":["queued"],"runningTurnKeys":[]});
        for (thread, turn) in [("T", ""), (" \n", "0")] {
            let mut start = HubNotification {
                method: "turn/started".into(),
                params: Some(json!({"threadId":thread,"turn":{"id":turn}})),
            };
            assert_eq!(
                apply_notification_to_runtime(&mut local, &mut start),
                ProjectionChange::None
            );
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
        let mut owned = ConnectionClaims::default();
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
        let mut owned = ConnectionClaims::default();
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
    fn replacement_socket_cannot_certify_cached_work_below_the_repair_floor() {
        use serde_json::json;
        let mut d = DaemonState::default();
        let (mut old, mut new) = (ConnectionClaims::default(), ConnectionClaims::default());
        let state = |n| json!({"runtimeId":"r","activeTurns":[], "workActivity":{"producerGeneration":"g","revision":n,"items":["queued"]}});
        d.producer_message(1, &mut old, RUNTIME_UPDATE_STATE_METHOD, Some(state(5)))
            .unwrap();
        let mut bad = state(8);
        bad.as_object_mut().unwrap().remove("activeTurns");
        assert!(
            d.producer_message(1, &mut old, RUNTIME_UPDATE_STATE_METHOD, Some(bad))
                .is_err()
        );
        d.require_replacement(1, "r");
        assert_eq!(
            d.take_replacement_request(1, "r").unwrap().minimum_revision,
            8
        );
        let request = d
            .producer_message(
                2,
                &mut new,
                RUNTIME_UPDATE_METADATA_METHOD,
                Some(json!({"runtimeId":"r","displayName":"next"})),
            )
            .unwrap();
        assert_eq!(request.unwrap().minimum_revision, 8);
        let stale = d
            .producer_message(2, &mut new, RUNTIME_UPDATE_STATE_METHOD, Some(state(7)))
            .unwrap();
        assert_eq!(stale.unwrap().minimum_revision, 8);
        assert!(d.snapshot().runtimes[0].work_activity.is_none());
        d.producer_message(1, &mut old, RUNTIME_UPDATE_STATE_METHOD, Some(state(9)))
            .unwrap();
        assert_eq!(d.runtime_connections["r"].connection, 1);
        assert_eq!(
            d.producer_message(2, &mut new, RUNTIME_UPDATE_STATE_METHOD, Some(state(8)))
                .unwrap()
                .unwrap()
                .minimum_revision,
            9
        );
        d.producer_message(2, &mut new, RUNTIME_UPDATE_STATE_METHOD, Some(state(9)))
            .unwrap();
        assert_eq!(d.runtime_connections["r"].connection, 2);
        assert_eq!(
            d.snapshot().runtimes[0]
                .work_activity
                .as_ref()
                .unwrap()
                .revision,
            9
        );
    }

    #[tokio::test]
    async fn healthy_owner_survives_unconfirmed_challenger() {
        use serde_json::json;
        let frame = |n, busy| {
            json!({"runtimeId":"r","activeTurns":[], "workActivity":{
            "producerGeneration":"g","revision":n,"items":if busy {vec!["queued"]} else {vec![]}}})
        };
        for (method, body) in [
            (RUNTIME_UPDATE_STATE_METHOD, frame(9, true)),
            (RUNTIME_UPDATE_STATE_METHOD, {
                let mut f = frame(15, false);
                f["workActivity"]["producerGeneration"] = "".into();
                f
            }),
            (RUNTIME_UPDATE_STATE_METHOD, {
                let mut f = frame(15, false);
                f["workActivity"]["revision"] = u64::MAX.into();
                f
            }),
            (RUNTIME_UPDATE_STATE_METHOD, {
                let mut f = frame(15, false);
                f.as_object_mut().unwrap().remove("activeTurns");
                f
            }),
            (
                RUNTIME_UPDATE_METADATA_METHOD,
                json!({"runtimeId":"r","displayName":"old"}),
            ),
            (RUNTIME_REGISTER_METHOD, json!({"runtimeId":"r"})),
            (RUNTIME_UNREGISTER_METHOD, json!({"runtimeId":"r"})),
            (
                RUNTIME_EVENT_METHOD,
                json!({"runtimeId":"r","notification":{"method":"turn/completed"}}),
            ),
        ] {
            let d = Arc::new(Mutex::new(DaemonState::default()));
            let (mut a, mut b) = (ConnectionClaims::default(), ConnectionClaims::default());
            {
                let mut state = d.lock().await;
                state
                    .producer_message(
                        1,
                        &mut a,
                        RUNTIME_UPDATE_STATE_METHOD,
                        Some(frame(10, true)),
                    )
                    .unwrap();
                let busy = state.snapshot();
                let _ = state.producer_message(2, &mut b, method, Some(body));
                assert_eq!(
                    state.runtime_connections["r"].connection, 1,
                    "a challenger cannot displace a healthy owner before confirmation"
                );
                assert!(b.owned.is_empty());
                assert_eq!(state.snapshot(), busy);
                state
                    .producer_message(
                        1,
                        &mut a,
                        RUNTIME_UPDATE_STATE_METHOD,
                        Some(frame(11, false)),
                    )
                    .unwrap();
                let idle = state.snapshot();
                state
                    .producer_message(
                        2,
                        &mut b,
                        RUNTIME_UPDATE_STATE_METHOD,
                        Some(frame(10, true)),
                    )
                    .unwrap();
                assert_eq!(state.snapshot(), idle);
            }
            cleanup_connection(Arc::clone(&d), 1, a, false).await;
            let mut state = d.lock().await;
            assert!(state.snapshot().runtimes.is_empty());
            state
                .producer_message(
                    2,
                    &mut b,
                    RUNTIME_UPDATE_STATE_METHOD,
                    Some(frame(16, true)),
                )
                .unwrap();
            assert_eq!(state.runtime_connections["r"].connection, 2);
            assert_eq!(
                state.snapshot().runtimes[0]
                    .work_activity
                    .as_ref()
                    .unwrap()
                    .revision,
                16
            );
        }
    }

    #[test]
    fn explicit_unregister_preserves_order_not_work() {
        use serde_json::json;
        let frame = |g, n, busy| {
            json!({"runtimeId":"r","activeTurns":[], "workActivity":{
            "producerGeneration":g,"revision":n,"items":if busy {vec!["queued"]} else {vec![]}}})
        };
        for (gen_after, rev_after, legacy_between) in
            [("g", 11, false), ("other", 1, false), ("g", 10, true)]
        {
            let mut d = DaemonState::default();
            let (mut a, mut b) = (ConnectionClaims::default(), ConnectionClaims::default());
            d.producer_message(
                1,
                &mut a,
                RUNTIME_UPDATE_STATE_METHOD,
                Some(frame("g", 10, false)),
            )
            .unwrap();
            d.producer_message(
                1,
                &mut a,
                RUNTIME_UNREGISTER_METHOD,
                Some(json!({"runtimeId":"r"})),
            )
            .unwrap();
            assert!(a.owned.is_empty());
            if legacy_between {
                let mut legacy = ConnectionClaims::default();
                d.producer_message(
                    2,
                    &mut legacy,
                    RUNTIME_UPDATE_STATE_METHOD,
                    Some(json!({"runtimeId":"r","activeTurns":[{"threadId":"T","turnId":"0"}]})),
                )
                .unwrap();
                let r = d.snapshot().runtimes.remove(0);
                assert!(r.work_activity.is_none() && r.active_turns.len() == 1);
                d.producer_message(
                    2,
                    &mut legacy,
                    RUNTIME_UNREGISTER_METHOD,
                    Some(json!({"runtimeId":"r"})),
                )
                .unwrap();
            }
            let absent = d.snapshot();
            d.producer_message(
                3,
                &mut b,
                RUNTIME_UPDATE_STATE_METHOD,
                Some(frame("g", 9, true)),
            )
            .unwrap();
            assert_eq!(
                d.snapshot(),
                absent,
                "explicit unregister must not let stale work reach a new subscriber"
            );
            assert_eq!(
                d.take_replacement_request(3, "r").unwrap().minimum_revision,
                10
            );
            d.producer_message(
                4,
                &mut ConnectionClaims::default(),
                RUNTIME_UPDATE_STATE_METHOD,
                Some(frame(gen_after, rev_after, true)),
            )
            .unwrap();
            assert_eq!(
                d.snapshot().runtimes[0]
                    .work_activity
                    .as_ref()
                    .unwrap()
                    .revision,
                rev_after
            );
        }
    }

    #[tokio::test]
    async fn clean_eof_keeps_revision_but_not_runtime_or_work() {
        use serde_json::json;
        let frame = |g, n, items: Vec<&str>| {
            json!({"runtimeId":"r","activeTurns":[],
            "workActivity":{"producerGeneration":g,"revision":n,"items":items}})
        };
        for (g, n) in [("g", 10), ("new", 1)] {
            let d = Arc::new(Mutex::new(DaemonState::default()));
            let mut first = ConnectionClaims::default();
            d.lock()
                .await
                .producer_message(
                    1,
                    &mut first,
                    RUNTIME_UPDATE_STATE_METHOD,
                    Some(frame("g", 10, vec![])),
                )
                .unwrap();
            cleanup_connection(Arc::clone(&d), 1, first, false).await;
            let mut second = ConnectionClaims::default();
            {
                let mut state = d.lock().await;
                let seq = state.seq;
                assert!(state.snapshot().runtimes.is_empty());
                state
                    .producer_message(
                        2,
                        &mut second,
                        RUNTIME_UPDATE_STATE_METHOD,
                        Some(frame("g", 9, vec!["stale"])),
                    )
                    .unwrap();
                assert!(
                    state.snapshot().runtimes.is_empty(),
                    "a clean EOF cannot admit revision 9 after idle 10"
                );
                assert_eq!(state.seq, seq);
                assert_eq!(
                    state
                        .take_replacement_request(2, "r")
                        .unwrap()
                        .minimum_revision,
                    10
                );
            }
            cleanup_connection(Arc::clone(&d), 2, second, false).await;
            let mut state = d.lock().await;
            state
                .producer_message(
                    3,
                    &mut ConnectionClaims::default(),
                    RUNTIME_UPDATE_STATE_METHOD,
                    Some(frame(g, n, vec![])),
                )
                .unwrap();
            assert!(
                state.snapshot().runtimes[0]
                    .work_activity
                    .as_ref()
                    .unwrap()
                    .items
                    .is_empty()
            );
            assert!(state.take_replacement_request(3, "r").is_none());
        }
    }

    #[tokio::test]
    async fn healthy_challenger_records_its_header_without_stealing_owner_authority() {
        use serde_json::json;
        for same_generation in [false, true] {
            let g = if same_generation { "g" } else { "h" };
            let full = |generation: &str, rev, busy| {
                json!({"runtimeId":"r","activeTurns":[],
                "workActivity":{"producerGeneration":generation,"revision":rev,"items":if busy {vec!["work"]}else{vec![]}}})
            };
            let mut d = DaemonState::default();
            let (mut a, mut b) = (ConnectionClaims::default(), ConnectionClaims::default());
            d.producer_message(
                1,
                &mut a,
                RUNTIME_UPDATE_STATE_METHOD,
                Some(full("g", 10, true)),
            )
            .unwrap();
            let mut bad = full(g, 30, true);
            bad.as_object_mut().unwrap().remove("activeTurns");
            let request = d
                .producer_message(2, &mut b, RUNTIME_UPDATE_STATE_METHOD, Some(bad))
                .unwrap()
                .unwrap();
            assert_eq!(
                (
                    request.producer_generation.as_deref(),
                    request.minimum_revision
                ),
                (Some(g), 30),
                "a challenger must be asked for its own observed generation and floor"
            );
            d.producer_message(
                1,
                &mut a,
                RUNTIME_UPDATE_STATE_METHOD,
                Some(full("g", 11, false)),
            )
            .unwrap();
            let idle = d.snapshot();
            for c in [2, 3] {
                let request = d
                    .producer_message(
                        c,
                        &mut b,
                        RUNTIME_UPDATE_STATE_METHOD,
                        Some(full(g, 29, true)),
                    )
                    .unwrap();
                assert_eq!(
                    d.snapshot(),
                    idle,
                    "older challenger must not acquire even on a fresh socket"
                );
                assert_eq!(request.unwrap().minimum_revision, 30);
            }
            d.producer_message(
                3,
                &mut b,
                RUNTIME_UPDATE_STATE_METHOD,
                Some(full(g, 30, true)),
            )
            .unwrap();
            assert_eq!(d.runtime_connections["r"].connection, 3);
        }
    }

    #[test]
    fn accepting_another_generation_does_not_retire_previous_floors() {
        use serde_json::json;
        let full = |g: &str, n, busy| {
            json!({"runtimeId":"r","activeTurns":[],
            "workActivity":{"producerGeneration":g,"revision":n,"items":if busy {vec!["work"]}else{vec![]}}})
        };
        let mut d = DaemonState::default();
        for (c, g, r) in [(1, "g", 10), (2, "h", 30), (3, "i", 1)] {
            d.producer_message(
                c,
                &mut ConnectionClaims::default(),
                RUNTIME_UPDATE_STATE_METHOD,
                Some(full(g, r, false)),
            )
            .unwrap();
        }
        let idle = d.snapshot();
        for (c, g, floor) in [(4, "g", 10), (5, "h", 30)] {
            let request = d
                .producer_message(
                    c,
                    &mut ConnectionClaims::default(),
                    RUNTIME_UPDATE_STATE_METHOD,
                    Some(full(g, floor - 1, true)),
                )
                .unwrap();
            assert_eq!(
                d.snapshot(),
                idle,
                "accepting H or I must not forget a returning generation"
            );
            assert_eq!(request.unwrap().minimum_revision, floor);
        }
        d.producer_message(
            6,
            &mut ConnectionClaims::default(),
            RUNTIME_UPDATE_STATE_METHOD,
            Some(full("g", 11, true)),
        )
        .unwrap();
        assert_eq!(
            d.snapshot().runtimes[0]
                .work_activity
                .as_ref()
                .unwrap()
                .revision,
            11
        );
    }

    #[tokio::test]
    async fn live_and_recovered_terminal_stamps_survive_full_state_and_eof() {
        use serde_json::json;
        let frame = |n, active| {
            json!({"runtimeId":"r","activeTurns":if active{vec![json!({"threadId":"review","turnId":"0"})]}else{vec![]},
            "workActivity":{"producerGeneration":"g","revision":n,"items":if active{vec!["review:0"]}else{vec![]}}})
        };
        let event = |n, status| {
            json!({"runtimeId":"r","workActivity":frame(n,false)["workActivity"],
            "notification":{"method":"turn/completed","params":{"turnKey":"review:0","turn":{"id":"0","status":status}}}})
        };
        for origin in ["live", "unversioned live", "recovered"] {
            let d = Arc::new(Mutex::new(DaemonState::default()));
            let mut a = ConnectionClaims::default();
            let mut s = d.lock().await;
            s.producer_message(
                1,
                &mut a,
                RUNTIME_UPDATE_STATE_METHOD,
                Some(frame(11, true)),
            )
            .unwrap();
            if origin == "recovered" {
                let mut state = frame(13, false);
                state["historyRecovery"] = json!({"epoch":1,"terminals":[{"turnKey":"review:0","threadId":"review","turnId":"0","status":"failed"}]});
                s.producer_message(1, &mut a, RUNTIME_UPDATE_STATE_METHOD, Some(state))
                    .unwrap();
            } else {
                let mut failure = event(13, "failed");
                if origin == "unversioned live" {
                    s.producer_message(
                        1,
                        &mut a,
                        RUNTIME_UPDATE_STATE_METHOD,
                        Some(frame(13, false)),
                    )
                    .unwrap();
                    failure.as_object_mut().unwrap().remove("workActivity");
                }
                s.producer_message(1, &mut a, RUNTIME_EVENT_METHOD, Some(failure))
                    .unwrap();
            }
            assert!(
                s.recent_events
                    .back()
                    .unwrap()
                    .1
                    .contains("\"status\":\"failed\""),
                "{origin}"
            );
            s.producer_message(
                1,
                &mut a,
                RUNTIME_UPDATE_STATE_METHOD,
                Some(frame(14, false)),
            )
            .unwrap();
            drop(s);
            cleanup_connection(Arc::clone(&d), 1, a, false).await;
            let mut s = d.lock().await;
            let absent = s.snapshot();
            let mut b = ConnectionClaims::default();
            s.producer_message(
                2,
                &mut b,
                RUNTIME_EVENT_METHOD,
                Some(event(12, "completed")),
            )
            .unwrap();
            assert_eq!(
                s.snapshot(),
                absent,
                "a normally published or recovered failure must survive the socket boundary: {origin}"
            );
            s.producer_message(
                2,
                &mut b,
                RUNTIME_EVENT_METHOD,
                Some(event(15, "interrupted")),
            )
            .unwrap();
            assert!(
                s.recent_events
                    .back()
                    .unwrap()
                    .1
                    .contains("\"status\":\"interrupted\"")
            );
            assert!(s.snapshot().runtimes.is_empty());
        }
    }

    #[tokio::test]
    async fn rejected_generations_do_not_displace_accepted_or_repair_floors() {
        use serde_json::json;
        let frame = |g, n, busy| {
            json!({"runtimeId":"r","activeTurns":[], "workActivity":{
            "producerGeneration":g,"revision":n,"items":if busy {vec!["work"]} else {vec![]}}})
        };
        for (next, n) in [("g", 10), ("h", 30), ("fresh", 1)] {
            let d = Arc::new(Mutex::new(DaemonState::default()));
            let mut a = ConnectionClaims::default();
            let mut s = d.lock().await;
            s.producer_message(
                1,
                &mut a,
                RUNTIME_UPDATE_STATE_METHOD,
                Some(frame("g", 10, false)),
            )
            .unwrap();
            s.producer_message(
                1,
                &mut a,
                RUNTIME_UNREGISTER_METHOD,
                Some(json!({"runtimeId":"r"})),
            )
            .unwrap();
            let absent = s.snapshot();
            drop(s);
            for (conn, g, rev, invalid) in [
                (2, "h", 30, true),
                (3, "i", 40, true),
                (4, "g", 9, false),
                (5, "h", 29, false),
            ] {
                let mut owned = ConnectionClaims::default();
                let mut s = d.lock().await;
                let mut msg = frame(g, rev, true);
                if invalid {
                    msg.as_object_mut().unwrap().remove("activeTurns");
                }
                let result =
                    s.producer_message(conn, &mut owned, RUNTIME_UPDATE_STATE_METHOD, Some(msg));
                if invalid {
                    assert!(result.is_err());
                    s.require_replacement(conn, "r");
                } else {
                    result.unwrap();
                }
                assert_eq!(
                    s.snapshot(),
                    absent,
                    "rejected generations cannot erase accepted or repair floors"
                );
                let request = s.take_replacement_request(conn, "r").unwrap();
                assert_eq!(request.producer_generation.as_deref(), Some(g));
                assert_eq!(
                    request.minimum_revision,
                    if invalid { rev } else { rev + 1 }
                );
                drop(s);
                cleanup_connection(Arc::clone(&d), conn, owned, false).await;
            }
            let mut s = d.lock().await;
            s.producer_message(
                6,
                &mut ConnectionClaims::default(),
                RUNTIME_UPDATE_STATE_METHOD,
                Some(frame(next, n, true)),
            )
            .unwrap();
            assert_eq!(
                s.snapshot().runtimes[0]
                    .work_activity
                    .as_ref()
                    .unwrap()
                    .producer_generation,
                next
            );
        }
    }

    #[tokio::test]
    async fn legacy_incumbent_survives_unconfirmed_overlap_in_both_close_orders() {
        use serde_json::json;
        let full = |active| json!({"runtimeId":"r","activeTurns":if active{vec![json!({"threadId":"p","turnId":"0"})]}else{vec![]}});
        for close_challenger in [true, false] {
            let d = Arc::new(Mutex::new(DaemonState::default()));
            let (mut a, mut b) = (ConnectionClaims::default(), ConnectionClaims::default());
            let mut s = d.lock().await;
            s.producer_message(1, &mut a, RUNTIME_UPDATE_STATE_METHOD, Some(full(true)))
                .unwrap();
            let busy = s.snapshot();
            for (method, params) in [
                (
                    RUNTIME_UPDATE_METADATA_METHOD,
                    json!({"runtimeId":"r","displayName":"B"}),
                ),
                (RUNTIME_REGISTER_METHOD, json!({"runtimeId":"r"})),
                (RUNTIME_UPDATE_STATE_METHOD, json!({"runtimeId":"r"})),
            ] {
                let request = s.producer_message(2, &mut b, method, Some(params)).unwrap();
                assert_eq!(
                    s.snapshot(),
                    busy,
                    "a non-owning peer cannot change a legacy projection"
                );
                assert!(b.owned.is_empty());
                let request = request.unwrap();
                assert_eq!(
                    (request.producer_generation, request.minimum_revision),
                    (None, 0)
                );
            }
            drop(s);
            if close_challenger {
                cleanup_connection(Arc::clone(&d), 2, b, false).await;
                let mut s = d.lock().await;
                assert_eq!(s.snapshot(), busy);
                s.producer_message(1, &mut a, RUNTIME_UPDATE_STATE_METHOD, Some(full(false)))
                    .unwrap();
                assert!(s.snapshot().runtimes[0].active_turns.is_empty());
            } else {
                cleanup_connection(Arc::clone(&d), 1, a, false).await;
                let mut s = d.lock().await;
                assert!(s.snapshot().runtimes.is_empty());
                s.producer_message(2, &mut b, RUNTIME_UPDATE_STATE_METHOD, Some(full(true)))
                    .unwrap();
                s.producer_message(
                    3,
                    &mut ConnectionClaims::default(),
                    RUNTIME_UPDATE_STATE_METHOD,
                    Some(full(false)),
                )
                .unwrap();
                assert!(s.snapshot().runtimes[0].active_turns.is_empty());
            }
        }
    }

    #[tokio::test]
    async fn overlap_repair_sequences_terminal_only_after_its_turn_is_unowned() {
        use serde_json::json;
        for finish in [
            "omitted",
            "still-active",
            "newer-terminal",
            "recovered-active",
            "recovered-idle",
            "eof",
        ] {
            let d = Arc::new(Mutex::new(DaemonState::default()));
            let mut owner = ConnectionClaims::default();
            let mut replacement = ConnectionClaims::default();
            let frame = |n, active: bool| {
                json!({"runtimeId":"r","activeTurns":if active {vec![json!({"threadId":"review","turnId":"0"})]}else{vec![]},
                "workActivity":{"producerGeneration":"g","revision":n,"items":if active{vec!["work"]}else{vec![]},
                    "runningTurnKeys":if active{vec!["review:0"]}else{vec![]}}})
            };
            let event = |n, status| {
                json!({"runtimeId":"r","workActivity":frame(n,false)["workActivity"],
                "notification":{"method":"turn/completed","params":{"turnKey":"review:0","turn":{"id":"0","status":status}}}})
            };
            let mut state = d.lock().await;
            state
                .producer_message(
                    1,
                    &mut owner,
                    RUNTIME_UPDATE_STATE_METHOD,
                    Some(frame(10, true)),
                )
                .unwrap();
            let mut bad = frame(14, true);
            bad.as_object_mut().unwrap().remove("activeTurns");
            assert!(
                state
                    .producer_message(1, &mut owner, RUNTIME_UPDATE_STATE_METHOD, Some(bad))
                    .is_err()
            );
            state.require_replacement(1, "r");
            let request = state
                .producer_message(
                    2,
                    &mut replacement,
                    RUNTIME_UPDATE_STATE_METHOD,
                    Some(frame(13, false)),
                )
                .unwrap();
            assert_eq!(request.unwrap().minimum_revision, 14);
            let start = state.seq;
            for (rev, outcome) in [
                (12, "completed"),
                (13, "failed"),
                (13, "failed"),
                (12, "completed"),
            ] {
                state
                    .producer_message(
                        2,
                        &mut replacement,
                        RUNTIME_EVENT_METHOD,
                        Some(event(rev, outcome)),
                    )
                    .unwrap();
            }
            assert_eq!(
                state.seq, start,
                "an attached turn must not end on the stale frame"
            );
            assert_eq!(state.snapshot().runtimes[0].active_turns.len(), 1);
            if finish == "eof" {
                drop(state);
                cleanup_connection(Arc::clone(&d), 2, replacement, false).await;
                assert_eq!(
                    d.lock().await.seq,
                    start,
                    "a history-only sender cannot revoke its owner"
                );
                cleanup_connection(Arc::clone(&d), 1, owner, false).await;
                state = d.lock().await;
            } else if finish.starts_with("recovered-") {
                let active = finish == "recovered-active";
                let mut correction = frame(14, active);
                correction["historyRecovery"] = json!({"epoch":2,"terminals":[{"turnKey":"review:0","threadId":"review","turnId":"0","status":"interrupted"}]});
                state
                    .producer_message(
                        2,
                        &mut replacement,
                        RUNTIME_UPDATE_STATE_METHOD,
                        Some(correction),
                    )
                    .unwrap();
                if active {
                    assert_eq!(
                        state.seq,
                        start + 2,
                        "historyLost and an active replacement, no premature terminal"
                    );
                    state
                        .producer_message(
                            2,
                            &mut replacement,
                            RUNTIME_UPDATE_STATE_METHOD,
                            Some(frame(15, false)),
                        )
                        .unwrap();
                }
            } else {
                if finish != "omitted" {
                    state
                        .producer_message(
                            2,
                            &mut replacement,
                            RUNTIME_UPDATE_STATE_METHOD,
                            Some(frame(14, true)),
                        )
                        .unwrap();
                    assert_eq!(
                        state.seq,
                        start + 1,
                        "a correction still owning the key is not terminal"
                    );
                }
                if finish == "newer-terminal" {
                    state
                        .producer_message(
                            2,
                            &mut replacement,
                            RUNTIME_EVENT_METHOD,
                            Some(event(15, "interrupted")),
                        )
                        .unwrap();
                } else {
                    state
                        .producer_message(
                            2,
                            &mut replacement,
                            RUNTIME_UPDATE_STATE_METHOD,
                            Some(frame(15, false)),
                        )
                        .unwrap();
                }
            }
            let records: Vec<JsonValue> = state
                .recent_events
                .iter()
                .filter(|(n, _)| *n > start)
                .map(|(_, line)| {
                    serde_json::from_str::<JsonValue>(line).unwrap()["params"]["event"].clone()
                })
                .collect();
            let outcomes: Vec<_> = records
                .iter()
                .filter(|v| v["notification"]["method"] == "turn/completed")
                .collect();
            assert_eq!(outcomes.len(), 1, "{finish}: {records:?}");
            let authoritative = finish == "newer-terminal" || finish == "recovered-idle";
            assert_eq!(
                outcomes[0]["type"],
                if authoritative {
                    "runtimeNotification"
                } else {
                    "runtimeArchiveDetail"
                }
            );
            assert_eq!(
                outcomes[0]["notification"]["params"]["turn"]["status"],
                if authoritative || finish == "recovered-active" {
                    "interrupted"
                } else {
                    "failed"
                }
            );
            assert_eq!(records.last(), Some(outcomes[0]));
        }
    }

    #[tokio::test]
    async fn pending_floor_survives_eof_without_retaining_live_ownership() {
        use serde_json::json;
        for reset_generation in [false, true] {
            let d = Arc::new(Mutex::new(DaemonState::default()));
            let frame = |g, n, items: Vec<&str>| json!({"runtimeId":"r","activeTurns":[],"workActivity":{"producerGeneration":g,"revision":n,"items":items}});
            let mut owned = ConnectionClaims::default();
            {
                let mut state = d.lock().await;
                state
                    .producer_message(
                        1,
                        &mut owned,
                        RUNTIME_UPDATE_STATE_METHOD,
                        Some(frame("g", 5, vec!["live"])),
                    )
                    .unwrap();
                let mut bad = frame("g", 10, vec![]);
                bad.as_object_mut().unwrap().remove("activeTurns");
                assert!(
                    state
                        .producer_message(1, &mut owned, RUNTIME_UPDATE_STATE_METHOD, Some(bad))
                        .is_err()
                );
                state.require_replacement(1, "r");
                assert_eq!(
                    state
                        .take_replacement_request(1, "r")
                        .unwrap()
                        .minimum_revision,
                    10
                );
            }
            cleanup_connection(Arc::clone(&d), 1, owned, false).await;
            let mut second = ConnectionClaims::default();
            {
                let mut state = d.lock().await;
                assert!(
                    state.snapshot().runtimes.is_empty() && state.runtime_connections.is_empty()
                );
                assert!(
                    state
                        .recent_events
                        .back()
                        .unwrap()
                        .1
                        .contains("runtimeRemoved")
                );
                state
                    .producer_message(
                        2,
                        &mut second,
                        RUNTIME_UPDATE_STATE_METHOD,
                        Some(frame("g", 9, vec!["stale"])),
                    )
                    .unwrap();
                assert!(state.snapshot().runtimes.is_empty());
                assert_eq!(
                    state
                        .take_replacement_request(2, "r")
                        .unwrap()
                        .minimum_revision,
                    10
                );
            }
            // A second EOF before the correction must not consume the checkpoint either.
            cleanup_connection(Arc::clone(&d), 2, second, false).await;
            let mut state = d.lock().await;
            let (g, n) = if reset_generation {
                ("new", 1)
            } else {
                ("g", 10)
            };
            state
                .producer_message(
                    3,
                    &mut ConnectionClaims::default(),
                    RUNTIME_UPDATE_STATE_METHOD,
                    Some(frame(g, n, vec!["restored"])),
                )
                .unwrap();
            assert_eq!(
                state.snapshot().runtimes[0]
                    .work_activity
                    .as_ref()
                    .unwrap()
                    .revision,
                n
            );
            assert!(
                state.detached_revisions.is_empty()
                    && state.take_replacement_request(3, "r").is_none()
            );
        }
    }

    #[tokio::test]
    async fn detached_repair_forwards_only_keyed_terminal_history_without_a_runtime() {
        use serde_json::json;
        let d = Arc::new(Mutex::new(DaemonState::default()));
        let frame = |rev| {
            json!({"runtimeId":"r","activeTurns":[],
            "workActivity":{"producerGeneration":"g","revision":rev,"items":[]}})
        };
        let mut owner = ConnectionClaims::default();
        {
            let mut state = d.lock().await;
            state
                .producer_message(1, &mut owner, RUNTIME_UPDATE_STATE_METHOD, Some(frame(1)))
                .unwrap();
            let mut bad = frame(4);
            bad.as_object_mut().unwrap().remove("activeTurns");
            assert!(
                state
                    .producer_message(1, &mut owner, RUNTIME_UPDATE_STATE_METHOD, Some(bad))
                    .is_err()
            );
            state.require_replacement(1, "r");
        }
        cleanup_connection(Arc::clone(&d), 1, owner, false).await;
        let mut second = ConnectionClaims::default();
        let mut state = d.lock().await;
        state
            .producer_message(2, &mut second, RUNTIME_UPDATE_STATE_METHOD, Some(frame(3)))
            .unwrap();
        assert_eq!(
            state
                .take_replacement_request(2, "r")
                .unwrap()
                .minimum_revision,
            4
        );
        let before = state.seq;
        state
            .producer_message(
                2,
                &mut second,
                RUNTIME_UPDATE_METADATA_METHOD,
                Some(json!({"runtimeId":"r","displayName":"reviewer"})),
            )
            .unwrap();
        assert!(state.snapshot().runtimes.is_empty());
        for (method, params, revision) in [
            (
                "turn/started",
                json!({"threadId":"review","turn":{"id":"0"}}),
                None,
            ),
            (
                "turn/completed",
                json!({"turn":{"id":"0","status":"failed"}}),
                None,
            ),
            (
                "turn/completed",
                json!({"turnKey":"review:0","turn":{"id":"0","status":"inProgress"}}),
                None,
            ),
            (
                "turn/completed",
                json!({"threadId":"earlier","turn":{"id":"0","status":"interrupted"}}),
                Some(3),
            ),
            (
                "turn/completed",
                json!({"turnKey":"review:0","turn":{"id":"0","status":"failed","promptPreview":"independent review"}}),
                Some(5),
            ),
            (
                "turn/completed",
                json!({"turnKey":"review:0","turn":{"id":"0","status":"completed"}}),
                Some(3),
            ),
        ] {
            let mut event =
                json!({"runtimeId":"r","notification":{"method":method,"params":params}});
            if let Some(rev) = revision {
                event["workActivity"] = frame(rev)["workActivity"].clone();
            }
            state
                .producer_message(2, &mut second, RUNTIME_EVENT_METHOD, Some(event))
                .unwrap();
            assert!(state.snapshot().runtimes.is_empty());
        }
        let events: Vec<_> = state
            .recent_events
            .iter()
            .filter(|(n, _)| *n > before)
            .map(|(_, line)| {
                serde_json::from_str::<JsonValue>(line).unwrap()["params"]["event"].clone()
            })
            .collect();
        assert_eq!(
            events,
            [
                json!({"type":"runtimeArchiveDetail","runtimeId":"r","notification":{"method":"turn/completed","params":{"turnKey":"earlier:0","threadId":"earlier","turn":{"id":"0","status":"interrupted"}}}}),
                json!({"type":"runtimeArchiveDetail","runtimeId":"r","notification":{"method":"turn/completed","params":{"turnKey":"review:0","turn":{"id":"0","status":"failed","promptPreview":"independent review"}}}}),
            ]
        );
        drop(state);
        cleanup_connection(Arc::clone(&d), 2, second, false).await;
        let mut third = ConnectionClaims::default();
        let mut state = d.lock().await;
        for (revision, status) in [(3, "completed"), (6, "interrupted")] {
            state.producer_message(3,&mut third,RUNTIME_EVENT_METHOD,Some(json!({"runtimeId":"r",
                "workActivity":frame(revision)["workActivity"],"notification":{"method":"turn/completed","params":
                {"turnKey":"review:0","turn":{"id":"0","status":status}}}}))).unwrap();
            assert_eq!(state.seq, before + if revision == 3 { 2 } else { 3 });
        }
        state
            .producer_message(3, &mut third, RUNTIME_UPDATE_STATE_METHOD, Some(frame(6)))
            .unwrap();
        assert_eq!(
            state.seq,
            before + 4,
            "idle correction must not replay an already sequenced terminal"
        );
        assert!(
            state.snapshot().runtimes[0]
                .work_activity
                .as_ref()
                .unwrap()
                .items
                .is_empty()
        );
    }

    #[test]
    fn history_loss_precedes_idle_and_keyed_outcomes_replay_only_once() {
        use serde_json::json;
        let mut d = DaemonState::default();
        let mut owned = ConnectionClaims::default();
        let mut frame = json!({"runtimeId":"r","activeTurns":[],"workActivity":{"producerGeneration":"g","revision":1,"items":[]},
            "historyRecovery":{"epoch":1,"terminals":[{"threadId":"review","turnId":"0","turnKey":"review:0","status":"failed"}]}});
        for revision in [1, 2] {
            frame["workActivity"]["revision"] = revision.into();
            d.producer_message(
                1,
                &mut owned,
                RUNTIME_UPDATE_STATE_METHOD,
                Some(frame.clone()),
            )
            .unwrap();
        }
        let frames: Vec<_> = d
            .recent_events
            .iter()
            .map(|(_, s)| serde_json::from_str::<JsonValue>(s).unwrap()["params"]["event"].clone())
            .collect();
        assert_eq!(frames.len(), 4);
        assert_eq!(frames[0]["notification"]["method"], "runtime/historyLost");
        assert_eq!(frames[1]["runtime"]["workActivity"]["items"], json!([]));
        assert_eq!(
            frames[2]["notification"]["params"]["turn"],
            json!({"id":"0","key":"review:0","status":"failed"})
        );
        assert_eq!(frames[3]["type"], "runtimeUpsert");
    }

    #[test]
    fn metadata_cannot_recertify_owners_while_a_replacement_is_pending() {
        use serde_json::json;

        let mut daemon = DaemonState::default();
        let mut owned = ConnectionClaims::default();
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
        daemon.require_replacement(1, "r");
        assert!(daemon.take_replacement_request(1, "r").is_some());
        let pending_seq = daemon.seq;
        daemon.require_replacement(1, "r");
        assert!(daemon.take_replacement_request(1, "r").is_none());
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
                    token_usage_baseline_known: true,
                    turn_key: Some("thread-2:turn-2".to_string()),
                    start_order: Some(2),
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
        let mut a = ConnectionClaims::default();
        let mut b = ConnectionClaims::default();
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
                    token_usage_baseline_known: true,
                    turn_key: Some("thread-a:turn-1".to_string()),
                    start_order: Some(1),
                    thread_id: "thread-a".to_string(),
                    turn_id: "turn-1".to_string(),
                    status: Some("inProgress".to_string()),
                    model: Some("model-thread-a".to_string()),
                    ..Default::default()
                },
                ActiveTurnSnapshot {
                    token_usage_baseline_known: true,
                    turn_key: Some("thread-b:turn-1".to_string()),
                    start_order: Some(2),
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
                token_usage_baseline_known: true,
                turn_key: Some("thread-a:turn-1".to_string()),
                start_order: Some(1),
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
                &mut HubNotification {
                    method: String::from(method),
                    params: Some(params),
                },
            ) != ProjectionChange::None
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
                token_usage_baseline_known: true,
                turn_key: Some("thread-new:0".to_string()),
                start_order: Some(1),
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
                token_usage_baseline_known: true,
                turn_key: Some("thread-1:turn-1".to_string()),
                start_order: Some(1),
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
                token_usage_baseline_known: true,
                turn_key: Some("thread-1:turn-2".to_string()),
                start_order: Some(1),
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
