use serde::Deserialize;
use serde::Serialize;
use serde_json::Value as JsonValue;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct HubNotification {
    pub method: String,
    #[serde(default)]
    pub params: Option<JsonValue>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ActiveTurnSnapshot {
    pub thread_id: String,
    pub turn_id: String,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub started_at: Option<i64>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub latest_label: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeSnapshot {
    pub runtime_id: String,
    #[serde(default)]
    pub pid: Option<u32>,
    #[serde(default)]
    pub session_source: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub active_turns: Vec<ActiveTurnSnapshot>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CodexdSnapshotResponse {
    pub seq: u64,
    pub runtimes: Vec<RuntimeSnapshot>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CodexdHelloResponse {
    pub protocol_version: u32,
    pub capabilities: Vec<String>,
    pub seq: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CodexdSubscribeParams {
    #[serde(default)]
    pub after_seq: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CodexdSubscribeResponse {
    pub seq: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeRegisterParams {
    pub runtime_id: String,
    #[serde(default)]
    pub pid: Option<u32>,
    #[serde(default)]
    pub session_source: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub display_name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeUpdateMetadataParams {
    pub runtime_id: String,
    #[serde(default)]
    pub pid: Option<u32>,
    #[serde(default)]
    pub session_source: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub display_name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeUpdateStateParams {
    pub runtime_id: String,
    #[serde(default)]
    pub pid: Option<u32>,
    #[serde(default)]
    pub session_source: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub active_turns: Vec<ActiveTurnSnapshot>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeEventParams {
    pub runtime_id: String,
    pub notification: HubNotification,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeUnregisterParams {
    pub runtime_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum CodexdEventPayload {
    RuntimeUpsert {
        runtime: RuntimeSnapshot,
    },
    RuntimeRemoved {
        runtime_id: String,
    },
    RuntimeNotification {
        runtime_id: String,
        notification: HubNotification,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CodexdEventEnvelope {
    pub seq: u64,
    pub event: CodexdEventPayload,
}
