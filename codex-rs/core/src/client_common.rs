use crate::client_common::tools::ToolSpec;
use crate::config::types::Personality;
use crate::error::Result;
pub use codex_api::common::ResponseEvent;
use codex_protocol::config_types::Verbosity;
use codex_protocol::models::BaseInstructions;
use codex_protocol::models::FunctionCallOutputBody;
use codex_protocol::models::ResponseItem;
use codex_protocol::openai_models::ModelInfo;
use codex_protocol::openai_models::ReasoningEffort;
use futures::Stream;
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashSet;
use std::pin::Pin;
use std::sync::Arc;
use std::task::Context;
use std::task::Poll;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Review thread system prompt. Edit `core/src/review_prompt.md` to customize.
pub const REVIEW_PROMPT: &str = include_str!("../review_prompt.md");

/// Post-turn completion review system prompt.
pub const POST_TURN_COMPLETION_REVIEW_PROMPT: &str =
    include_str!("../post_turn_completion_review_prompt.md");

// Centralized templates for review-related user messages
pub const REVIEW_EXIT_SUCCESS_TMPL: &str = include_str!("../templates/review/exit_success.xml");
pub const REVIEW_EXIT_INTERRUPTED_TMPL: &str =
    include_str!("../templates/review/exit_interrupted.xml");

/// API request payload for a single model turn
#[derive(Default, Debug, Clone)]
pub struct Prompt {
    /// Model-facing input. Select tools before fitting or sending tool results.
    pub input: Vec<ResponseItem>,

    /// Tools available to the model, including additional tools sourced from
    /// external MCP servers.
    pub(crate) tools: ToolSurface,

    /// Whether parallel tool calls are permitted for this prompt.
    pub(crate) parallel_tool_calls: bool,

    pub base_instructions: BaseInstructions,

    /// Optionally specify the personality of the model.
    pub personality: Option<Personality>,
    pub(crate) verbosity: Option<SampledVerbosity>,
    pub(crate) sampled_controls: Option<SampledControls>,
    pub(crate) helper_fit: Option<crate::compact::HelperFit>,
    pub(crate) review_admission: Option<ReviewAdmission>,

    /// Optional the output schema for the model's response.
    pub output_schema: Option<Value>,
}

pub(crate) fn response_format_tokens(schema: &Option<Value>) -> i64 {
    codex_api::create_text_param_for_request(None, schema)
        .and_then(|text| text.format)
        .map_or(0, |format| {
            serde_json::to_string(&format).map_or(i64::MAX, |json| {
                crate::truncate::approx_token_count(&json) as i64
            })
        })
}

#[derive(Clone, Debug)]
pub(crate) struct ReviewAdmission {
    pub(crate) target: i64,
    pub(crate) proposal: Option<Arc<crate::session::ReviewFrameProposal>>,
    /// The ordinary estimate is attributable only to this authenticated endpoint.
    pub(crate) measured: Option<(String, i64)>,
}

impl ReviewAdmission {
    pub(crate) async fn admit(
        &self,
        prompt: &Prompt,
        items: &mut [ResponseItem],
        endpoint: Option<&str>,
    ) -> Result<()> {
        let estimated = match &self.measured {
            Some((owner, tokens)) if Some(owner.as_str()) == endpoint => *tokens,
            _ => items.iter().fold(
                (crate::truncate::approx_token_count(&prompt.base_instructions.text) as i64)
                    .saturating_add(prompt.request_overhead_tokens()),
                |cost, item| {
                    cost.saturating_add(crate::context_manager::estimate_item_token_count(item))
                },
            ),
        };
        if let Some(proposal) = &self.proposal {
            return proposal
                .admit(estimated, self.target, items, endpoint)
                .await;
        }
        Self::check_size(estimated, self.target, endpoint)
    }

    pub(crate) fn check_size(estimated: i64, target: i64, endpoint: Option<&str>) -> Result<()> {
        if estimated > target {
            return Err(crate::error::CodexErr::ReviewPayloadExceeded {
                estimated,
                target,
                endpoint: endpoint.map(str::to_owned),
            });
        }
        Ok(())
    }
}

/// The same ordered inventory is used for admission, sampling and its usage record.
#[derive(Clone, Debug, Default)]
pub(crate) struct ToolSurface {
    wire: Arc<[Value]>,
    tokens: i64,
    pub(crate) structured_outputs: bool,
}

impl ToolSurface {
    pub(crate) fn new(specs: &[ToolSpec]) -> Result<Self> {
        let wire = crate::tools::spec::create_tools_json_for_responses_api(specs)?;
        let tokens = if wire.is_empty() {
            0
        } else {
            crate::truncate::approx_token_count(&serde_json::to_string(&wire)?) as i64
        };
        Ok(Self {
            wire: wire.into(),
            tokens,
            structured_outputs: specs
                .iter()
                .any(|tool| matches!(tool, ToolSpec::Freeform(f) if f.name == "apply_patch")),
        })
    }

    pub(crate) fn tokens(&self) -> i64 {
        self.tokens
    }
    pub(crate) fn wire(&self) -> &[Value] {
        &self.wire
    }
}

pub(crate) fn reasoning_effort_for_request(effort: ReasoningEffort) -> ReasoningEffort {
    match effort {
        ReasoningEffort::Ultra => ReasoningEffort::Max,
        effort => effort,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SampledReasoning {
    Disabled,
    Enabled(Option<ReasoningEffort>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SampledVerbosity {
    Omitted,
    Level(Verbosity),
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub(crate) struct SampledControls {
    pub(crate) route: crate::model_provider_info::CheckpointRoute,
    pub(crate) reasoning: SampledReasoning,
    pub(crate) verbosity: SampledVerbosity,
    /// Older receipts did not record this setting.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) parallel_tool_calls: Option<bool>,
}

impl SampledControls {
    pub(crate) fn owns_endpoint(&self, endpoint: Option<&str>) -> bool {
        self.route.owns_endpoint(endpoint)
    }
}

impl SampledVerbosity {
    pub(crate) fn for_request(model: &ModelInfo, configured: Option<Verbosity>) -> Self {
        if model.support_verbosity {
            configured
                .or(model.default_verbosity)
                .map_or(Self::Omitted, Self::Level)
        } else {
            Self::Omitted
        }
    }

    pub(crate) fn level(self) -> Option<Verbosity> {
        match self {
            Self::Omitted => None,
            Self::Level(value) => Some(value),
        }
    }
}

impl SampledReasoning {
    pub(crate) fn for_request(model: &ModelInfo, effort: Option<ReasoningEffort>) -> Self {
        if model.supports_reasoning_summaries {
            Self::Enabled(
                effort
                    .or(model.default_reasoning_level)
                    .map(reasoning_effort_for_request),
            )
        } else {
            Self::Disabled
        }
    }
}

impl Prompt {
    pub(crate) fn request_overhead_tokens(&self) -> i64 {
        self.tools
            .tokens()
            .saturating_add(response_format_tokens(&self.output_schema))
    }

    pub(crate) fn parallel_tool_calls_at(&self, endpoint: Option<&str>) -> bool {
        self.parallel_tool_calls
            && self
                .sampled_controls
                .as_ref()
                .filter(|c| c.owns_endpoint(endpoint))
                .and_then(|c| c.parallel_tool_calls)
                .unwrap_or(true)
    }

    pub(crate) fn with_tools(mut self, tools: ToolSurface) -> Self {
        self.tools = tools;
        self.format_tool_results();
        self
    }

    pub(crate) fn format_tool_results(&mut self) {
        if self.tools.structured_outputs {
            let mut formatter = ShellOutputFormatter::default();
            for item in &mut self.input {
                if let Some(formatted) = formatter.project(item) {
                    *item = formatted;
                }
            }
        }
    }
}

#[derive(Clone, Debug, Default)]
pub(crate) struct ShellOutputFormatter {
    pending: HashSet<String>,
}

impl ShellOutputFormatter {
    pub(crate) fn forget(&mut self, id: &str) {
        self.pending.remove(id);
    }

    pub(crate) fn project(&mut self, item: &ResponseItem) -> Option<ResponseItem> {
        match item {
            ResponseItem::LocalShellCall { call_id, id, .. } => {
                if let Some(identifier) = call_id.as_ref().or(id.as_ref()) {
                    self.pending.insert(identifier.clone());
                }
            }
            ResponseItem::CustomToolCall { call_id, name, .. } if name == "apply_patch" => {
                self.pending.insert(call_id.clone());
            }
            ResponseItem::CustomToolCallOutput { call_id, output } => {
                if self.pending.remove(call_id)
                    && let Some(structured) = parse_structured_shell_output(output)
                {
                    return Some(ResponseItem::CustomToolCallOutput {
                        call_id: call_id.clone(),
                        output: structured,
                    });
                }
            }
            ResponseItem::FunctionCall { name, call_id, .. }
                if is_shell_tool_name(name) || name == "apply_patch" =>
            {
                self.pending.insert(call_id.clone());
            }
            ResponseItem::FunctionCallOutput { call_id, output } => {
                if self.pending.remove(call_id)
                    && let Some(structured) = output
                        .text_content()
                        .and_then(parse_structured_shell_output)
                {
                    return Some(ResponseItem::FunctionCallOutput {
                        call_id: call_id.clone(),
                        output: codex_protocol::models::FunctionCallOutputPayload {
                            body: FunctionCallOutputBody::Text(structured),
                            success: output.success,
                        },
                    });
                }
            }
            _ => {}
        }
        None
    }
}

fn is_shell_tool_name(name: &str) -> bool {
    matches!(name, "shell" | "container.exec")
}

#[derive(Deserialize)]
struct ExecOutputJson {
    output: String,
    metadata: ExecOutputMetadataJson,
}

#[derive(Deserialize)]
struct ExecOutputMetadataJson {
    exit_code: i32,
    duration_seconds: f32,
    output_artifact: Option<String>,
}

fn parse_structured_shell_output(raw: &str) -> Option<String> {
    let parsed: ExecOutputJson = serde_json::from_str(raw).ok()?;
    Some(build_structured_output(&parsed))
}

fn build_structured_output(parsed: &ExecOutputJson) -> String {
    let mut sections = Vec::new();
    sections.push(format!("Exit code: {}", parsed.metadata.exit_code));
    sections.push(format!(
        "Wall time: {} seconds",
        parsed.metadata.duration_seconds
    ));

    let mut output = parsed.output.clone();
    if let Some((stripped, total_lines)) = strip_total_output_header(&parsed.output) {
        sections.push(format!("Total output lines: {total_lines}"));
        output = stripped.to_string();
    }
    if let Some(receipt) = &parsed.metadata.output_artifact {
        sections.push(receipt.clone());
    }

    sections.push("Output:".to_string());
    sections.push(output);

    sections.join("\n")
}

fn strip_total_output_header(output: &str) -> Option<(&str, u32)> {
    let after_prefix = output.strip_prefix("Total output lines: ")?;
    let (total_segment, remainder) = after_prefix.split_once('\n')?;
    let total_lines = total_segment.parse::<u32>().ok()?;
    let remainder = remainder.strip_prefix('\n').unwrap_or(remainder);
    Some((remainder, total_lines))
}

pub(crate) mod tools {
    use crate::tools::spec::JsonSchema;
    use serde::Deserialize;
    use serde::Serialize;

    /// When serialized as JSON, this produces a valid "Tool" in the OpenAI
    /// Responses API.
    #[derive(Debug, Clone, Serialize, PartialEq)]
    #[serde(tag = "type")]
    pub(crate) enum ToolSpec {
        #[serde(rename = "function")]
        Function(ResponsesApiTool),
        #[serde(rename = "local_shell")]
        LocalShell {},
        // TODO: Understand why we get an error on web_search although the API docs say it's supported.
        // https://platform.openai.com/docs/guides/tools-web-search?api-mode=responses#:~:text=%7B%20type%3A%20%22web_search%22%20%7D%2C
        // The `external_web_access` field determines whether the web search is over cached or live content.
        // https://platform.openai.com/docs/guides/tools-web-search#live-internet-access
        #[serde(rename = "web_search")]
        WebSearch {
            #[serde(skip_serializing_if = "Option::is_none")]
            external_web_access: Option<bool>,
        },
        #[serde(rename = "custom")]
        Freeform(FreeformTool),
    }

    impl ToolSpec {
        pub(crate) fn name(&self) -> &str {
            match self {
                ToolSpec::Function(tool) => tool.name.as_str(),
                ToolSpec::LocalShell {} => "local_shell",
                ToolSpec::WebSearch { .. } => "web_search",
                ToolSpec::Freeform(tool) => tool.name.as_str(),
            }
        }
    }

    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
    pub struct FreeformTool {
        pub(crate) name: String,
        pub(crate) description: String,
        pub(crate) format: FreeformToolFormat,
    }

    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
    pub struct FreeformToolFormat {
        pub(crate) r#type: String,
        pub(crate) syntax: String,
        pub(crate) definition: String,
    }

    #[derive(Debug, Clone, Serialize, PartialEq)]
    pub struct ResponsesApiTool {
        pub(crate) name: String,
        pub(crate) description: String,
        /// TODO: Validation. When strict is set to true, the JSON schema,
        /// `required` and `additional_properties` must be present. All fields in
        /// `properties` must be present in `required`.
        pub(crate) strict: bool,
        pub(crate) parameters: JsonSchema,
    }
}

pub struct ResponseStream {
    pub(crate) endpoint_sha256: Option<String>,
    pub(crate) compact_input: Option<crate::compact::SentCompactInput>,
    pub(crate) rx_event: mpsc::Receiver<Result<ResponseEvent>>,
    pub(crate) consumer_dropped: CancellationToken,
}

/// Usage belongs to the endpoint that opened this stream, not a newly resolved configuration.
pub(crate) struct ResponseUsage<'a> {
    pub(crate) tokens: Option<&'a codex_protocol::protocol::TokenUsage>,
    pub(crate) endpoint: Option<&'a str>,
}

impl ResponseStream {
    pub(crate) fn usage<'a>(
        &'a self,
        tokens: Option<&'a codex_protocol::protocol::TokenUsage>,
    ) -> ResponseUsage<'a> {
        ResponseUsage {
            tokens,
            endpoint: self.endpoint_sha256.as_deref(),
        }
    }
}

impl Stream for ResponseStream {
    type Item = Result<ResponseEvent>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.rx_event.poll_recv(cx)
    }
}

impl Drop for ResponseStream {
    fn drop(&mut self) {
        self.consumer_dropped.cancel();
    }
}

#[cfg(test)]
mod tests {
    use super::SampledControls;
    use codex_api::ResponsesApiRequest;
    use codex_api::common::OpenAiVerbosity;
    use codex_api::common::TextControls;
    use codex_api::create_text_param_for_request;
    use pretty_assertions::assert_eq;

    #[test]
    fn legacy_sampled_controls_do_not_override_parallel_calls() -> serde_json::Result<()> {
        let original = serde_json::json!({"route":{"provider":"sample","endpoint_sha256":"A"}, "reasoning":"disabled", "verbosity":"omitted"});
        let controls: SampledControls = serde_json::from_value(original.clone())?;
        assert_eq!(serde_json::to_value(&controls)?, original);
        assert_eq!(
            [false, true].map(|allowed| super::Prompt {
                sampled_controls: Some(controls.clone()),
                parallel_tool_calls: allowed,
                ..Default::default()
            }
            .parallel_tool_calls_at(Some("A"))),
            [false, true]
        );
        Ok(())
    }

    #[test]
    fn serializes_text_verbosity_when_set() {
        let input = codex_api::common::ResponsesInput::default();
        let tools: Vec<serde_json::Value> = vec![];
        let req = ResponsesApiRequest {
            model: "gpt-5.1",
            input: &input,
            tools: &tools,
            tool_choice: "auto",
            parallel_tool_calls: true,
            reasoning: None,
            store: false,
            stream: true,
            include: vec![],
            service_tier: None,
            prompt_cache_key: None,
            text: Some(TextControls {
                verbosity: Some(OpenAiVerbosity::Low),
                format: None,
            }),
        };

        let v = serde_json::to_value(&req).expect("json");
        assert_eq!(
            v.get("text")
                .and_then(|t| t.get("verbosity"))
                .and_then(|s| s.as_str()),
            Some("low")
        );
    }

    #[test]
    fn serializes_text_schema_with_strict_format() {
        let input = codex_api::common::ResponsesInput::default();
        let tools: Vec<serde_json::Value> = vec![];
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "answer": {"type": "string"}
            },
            "required": ["answer"],
        });
        let text_controls =
            create_text_param_for_request(None, &Some(schema.clone())).expect("text controls");

        let req = ResponsesApiRequest {
            model: "gpt-5.1",
            input: &input,
            tools: &tools,
            tool_choice: "auto",
            parallel_tool_calls: true,
            reasoning: None,
            store: false,
            stream: true,
            include: vec![],
            service_tier: None,
            prompt_cache_key: None,
            text: Some(text_controls),
        };

        let v = serde_json::to_value(&req).expect("json");
        let text = v.get("text").expect("text field");
        assert!(text.get("verbosity").is_none());
        let format = text.get("format").expect("format field");

        assert_eq!(
            format.get("name"),
            Some(&serde_json::Value::String("codex_output_schema".into()))
        );
        assert_eq!(
            format.get("type"),
            Some(&serde_json::Value::String("json_schema".into()))
        );
        assert_eq!(format.get("strict"), Some(&serde_json::Value::Bool(true)));
        assert_eq!(format.get("schema"), Some(&schema));
        assert_eq!(
            super::response_format_tokens(&Some(schema)),
            crate::truncate::approx_token_count(&format.to_string()) as i64
        );
        assert_eq!(super::response_format_tokens(&None), 0);
    }

    #[test]
    fn omits_text_when_not_set() {
        let input = codex_api::common::ResponsesInput::default();
        let tools: Vec<serde_json::Value> = vec![];
        let req = ResponsesApiRequest {
            model: "gpt-5.1",
            input: &input,
            tools: &tools,
            tool_choice: "auto",
            parallel_tool_calls: true,
            reasoning: None,
            store: false,
            stream: true,
            include: vec![],
            service_tier: None,
            prompt_cache_key: None,
            text: None,
        };

        let v = serde_json::to_value(&req).expect("json");
        assert!(v.get("text").is_none());
    }
}
