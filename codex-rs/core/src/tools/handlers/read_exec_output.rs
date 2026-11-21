use codex_protocol::models::FunctionCallOutputBody;
use codex_utils_pty::OutputStream;
use serde::Deserialize;

use crate::function_tool::FunctionCallError;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolOutput;
use crate::tools::context::ToolPayload;
use crate::tools::registry::ToolHandler;
use crate::tools::registry::ToolKind;

pub(crate) struct ReadExecOutputHandler;

#[derive(Deserialize)]
struct Args {
    artifact: String,
    stream: Stream,
    #[serde(default)]
    offset: u64,
    limit: Option<usize>,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum Stream {
    Stdout,
    Stderr,
    Terminal,
}

impl ToolHandler for ReadExecOutputHandler {
    fn kind(&self) -> ToolKind {
        ToolKind::Function
    }

    async fn handle(&self, invocation: ToolInvocation) -> Result<ToolOutput, FunctionCallError> {
        let ToolPayload::Function { arguments } = invocation.payload else {
            return Err(FunctionCallError::RespondToModel(
                "expected artifact range arguments".into(),
            ));
        };
        let args: Args = super::parse_arguments(&arguments)?;
        let stream = match args.stream {
            Stream::Stdout => OutputStream::Stdout,
            Stream::Stderr => OutputStream::Stderr,
            Stream::Terminal => OutputStream::Terminal,
        };
        let budget = invocation.turn.truncation_policy.byte_budget();
        // Allow for the longest receipt and for JSON escaping control bytes.
        let limit = args
            .limit
            .unwrap_or(4096)
            .min(8192)
            .min(budget.saturating_sub(512) / 6);
        if limit == 0 {
            return Err(FunctionCallError::RespondToModel(
                "Artifact range or result budget is too small.".into(),
            ));
        }
        let (header, bytes) = invocation
            .session
            .services
            .output_store
            .retrieve(
                &invocation.turn.config.codex_home,
                invocation.session.conversation_id,
                &args.artifact,
                stream,
                args.offset,
                limit,
            )
            .await
            .map_err(|reason| {
                FunctionCallError::RespondToModel(format!("Output artifact unavailable: {reason}."))
            })?;
        let preview = super::encode_byte_range(bytes);
        let text = crate::truncate::fit_result(&header, &preview, budget);
        Ok(ToolOutput::Function {
            success: Some(header.len() + 1 + preview.len() <= budget),
            body: FunctionCallOutputBody::Text(text),
        })
    }
}
