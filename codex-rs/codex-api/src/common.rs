use crate::error::ApiError;
use codex_protocol::config_types::ReasoningSummary as ReasoningSummaryConfig;
use codex_protocol::config_types::Verbosity as VerbosityConfig;
use codex_protocol::models::DeveloperInstructions;
use codex_protocol::models::ResponseItem;
use codex_protocol::openai_models::ReasoningEffort as ReasoningEffortConfig;
use codex_protocol::protocol::ModelVerification;
use codex_protocol::protocol::RateLimitSnapshot;
use codex_protocol::protocol::TokenUsage;
use futures::Stream;
use serde::Deserialize;
use serde::Serialize;
use serde::Serializer;
use serde::ser::SerializeSeq;
use serde_json::Value;
use std::collections::HashMap;
use std::pin::Pin;
use std::task::Context;
use std::task::Poll;
use tokio::sync::mpsc;

/// Canonical prompt input for Responses endpoints.
#[derive(Debug, Clone)]
pub struct Prompt {
    pub input: ResponsesInput,
    /// JSON-encoded tool definitions compatible with the target API.
    // TODO(jif) have a proper type here
    pub tools: Vec<Value>,
    /// Whether parallel tool calls are permitted.
    pub parallel_tool_calls: bool,
    /// Optional output schema used to build the `text.format` controls.
    pub output_schema: Option<Value>,
}

/// A request-only prefix, never a second copy of the instructions in history.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BaseInstructionsMessage {
    id: String,
    #[serde(flatten)]
    message: ResponseItem,
}

impl BaseInstructionsMessage {
    pub fn new(id: String, text: &str) -> Self {
        Self {
            id,
            message: DeveloperInstructions::new(text).into(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ResponsesInput {
    pub base: Option<BaseInstructionsMessage>,
    pub items: Vec<ResponseItem>,
}

impl From<Vec<ResponseItem>> for ResponsesInput {
    fn from(items: Vec<ResponseItem>) -> Self {
        Self { base: None, items }
    }
}

impl Serialize for ResponsesInput {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut seq =
            serializer.serialize_seq(Some(self.items.len() + usize::from(self.base.is_some())))?;
        if let Some(base) = &self.base {
            seq.serialize_element(base)?;
        }
        for item in &self.items {
            seq.serialize_element(item)?;
        }
        seq.end()
    }
}

/// Canonical input payload for the compaction endpoint.
#[derive(Debug, Clone, Serialize)]
pub struct CompactionInput<'a> {
    pub model: &'a str,
    pub input: &'a [ResponseItem],
    pub instructions: &'a str,
}

/// Canonical input payload for the memory trace summarize endpoint.
#[derive(Debug, Clone, Serialize)]
pub struct MemoryTraceSummarizeInput {
    pub model: String,
    pub traces: Vec<MemoryTrace>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<Reasoning>,
}

#[derive(Debug, Clone, Serialize)]
pub struct MemoryTrace {
    pub id: String,
    pub metadata: MemoryTraceMetadata,
    pub items: Vec<Value>,
}

#[derive(Debug, Clone, Serialize)]
pub struct MemoryTraceMetadata {
    pub source_path: String,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct MemoryTraceSummaryOutput {
    pub trace_summary: String,
    pub memory_summary: String,
}

/// A bounded reason for removing a replayed reasoning item from model input.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ReasoningRemovalReason {
    OrganizationMismatch,
    EndUserMismatch,
    ModelMismatch,
    PrefixMismatch,
    TurnBoundary,
    Other,
}

impl ReasoningRemovalReason {
    /// Classify the provider's report without interpreting an absent report as zero removals.
    pub fn report_label(counts: Option<&std::collections::BTreeMap<Self, i64>>) -> &'static str {
        match counts {
            None => "unavailable",
            Some(counts) if counts.is_empty() => "no_removals_reported",
            Some(counts) if counts.contains_key(&Self::PrefixMismatch) => {
                "prefix_mismatch_reported"
            }
            Some(_) => "removals_reported",
        }
    }

    /// Every metric category, including unrecognized API reasons.
    pub const ALL: [Self; 6] = [
        Self::OrganizationMismatch,
        Self::EndUserMismatch,
        Self::ModelMismatch,
        Self::PrefixMismatch,
        Self::TurnBoundary,
        Self::Other,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::OrganizationMismatch => "organization_mismatch",
            Self::EndUserMismatch => "end_user_mismatch",
            Self::ModelMismatch => "model_mismatch",
            Self::PrefixMismatch => "prefix_mismatch",
            Self::TurnBoundary => "turn_boundary",
            Self::Other => "other",
        }
    }
}

#[derive(Debug)]
pub enum ResponseEvent {
    Created,
    OutputItemDone(ResponseItem),
    OutputItemAdded(ResponseItem),
    /// Emitted when the server reports the effective model for the stream.
    ServerModel(String),
    /// Emitted when the server recommends additional model/account verification.
    ModelVerifications(Vec<ModelVerification>),
    /// Emitted when `X-Reasoning-Included: true` is present on the response,
    /// meaning the server already accounted for past reasoning tokens and the
    /// client should not re-estimate them.
    ServerReasoningIncluded(bool),
    Completed {
        response_id: String,
        token_usage: Option<TokenUsage>,
        /// Provider value before the accounting default for missing cache detail.
        reported_cached_tokens: Option<i64>,
        /// Did the model affirmatively end its turn? Some providers do not set this,
        /// so callers rely on fallback logic when this is `None`.
        end_turn: Option<bool>,
        reasoning_removals: Option<std::collections::BTreeMap<ReasoningRemovalReason, i64>>,
    },
    OutputTextDelta(String),
    ToolCallInputDelta {
        item_id: String,
        call_id: Option<String>,
        delta: String,
    },
    ReasoningSummaryDelta {
        delta: String,
        summary_index: i64,
    },
    ReasoningContentDelta {
        delta: String,
        content_index: i64,
    },
    ReasoningSummaryPartAdded {
        summary_index: i64,
    },
    RateLimits(RateLimitSnapshot),
    ModelsEtag(String),
}

#[derive(Debug, Serialize, Clone, PartialEq)]
pub struct Reasoning {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effort: Option<ReasoningEffortConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<ReasoningSummaryConfig>,
}

#[derive(Debug, Serialize, Default, Clone, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum TextFormatType {
    #[default]
    JsonSchema,
}

#[derive(Debug, Serialize, Default, Clone, PartialEq)]
pub struct TextFormat {
    /// Format type used by the OpenAI text controls.
    pub r#type: TextFormatType,
    /// When true, the server is expected to strictly validate responses.
    pub strict: bool,
    /// JSON schema for the desired output.
    pub schema: Value,
    /// Friendly name for the format, used in telemetry/debugging.
    pub name: String,
}

/// Controls the `text` field for the Responses API, combining verbosity and
/// optional JSON schema output formatting.
#[derive(Debug, Serialize, Default, Clone, PartialEq)]
pub struct TextControls {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verbosity: Option<OpenAiVerbosity>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub format: Option<TextFormat>,
}

#[derive(Debug, Serialize, Default, Clone, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum OpenAiVerbosity {
    Low,
    #[default]
    Medium,
    High,
}

impl From<VerbosityConfig> for OpenAiVerbosity {
    fn from(v: VerbosityConfig) -> Self {
        match v {
            VerbosityConfig::Low => OpenAiVerbosity::Low,
            VerbosityConfig::Medium => OpenAiVerbosity::Medium,
            VerbosityConfig::High => OpenAiVerbosity::High,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct ResponsesApiRequest<'a> {
    pub model: &'a str,
    pub input: &'a ResponsesInput,
    pub tools: &'a [serde_json::Value],
    pub tool_choice: &'static str,
    pub parallel_tool_calls: bool,
    pub reasoning: Option<Reasoning>,
    pub store: bool,
    pub stream: bool,
    pub include: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service_tier: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_cache_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<TextControls>,
}

#[derive(Debug, Serialize, Clone, PartialEq)]
pub struct ResponseCreateWsRequest {
    pub model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous_response_id: Option<String>,
    pub input: ResponsesInput,
    pub tools: Vec<Value>,
    pub tool_choice: String,
    pub parallel_tool_calls: bool,
    pub reasoning: Option<Reasoning>,
    pub store: bool,
    pub stream: bool,
    pub include: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service_tier: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_cache_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<TextControls>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub generate: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_metadata: Option<HashMap<String, String>>,
}

#[derive(Debug, Serialize)]
#[serde(tag = "type")]
#[allow(clippy::large_enum_variant)]
pub enum ResponsesWsRequest {
    #[serde(rename = "response.create")]
    ResponseCreate(ResponseCreateWsRequest),
}

pub fn create_text_param_for_request(
    verbosity: Option<VerbosityConfig>,
    output_schema: &Option<Value>,
) -> Option<TextControls> {
    if verbosity.is_none() && output_schema.is_none() {
        return None;
    }

    Some(TextControls {
        verbosity: verbosity.map(std::convert::Into::into),
        format: output_schema.as_ref().map(|schema| TextFormat {
            r#type: TextFormatType::JsonSchema,
            strict: true,
            schema: schema.clone(),
            name: "codex_output_schema".to_string(),
        }),
    })
}

pub struct ResponseStream {
    pub rx_event: mpsc::Receiver<Result<ResponseEvent, ApiError>>,
}

impl Stream for ResponseStream {
    type Item = Result<ResponseEvent, ApiError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.rx_event.poll_recv(cx)
    }
}
