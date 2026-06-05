use std::borrow::Cow;
use std::sync::Arc;

use codex_protocol::config_types::WebSearchMode;
use codex_protocol::items::TurnItem;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::AgentMessageContentDeltaEvent;
use codex_protocol::protocol::AgentMessageDeltaEvent;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ExitedReviewModeEvent;
use codex_protocol::protocol::ItemCompletedEvent;
use codex_protocol::protocol::ReviewOutputEvent;
use codex_protocol::protocol::SandboxPolicy;
use codex_protocol::protocol::SubAgentSource;
use tokio_util::sync::CancellationToken;

use crate::codex_delegate::DelegateRuntimeContextParams;
use crate::codex_delegate::apply_delegate_model_provider;
use crate::codex_delegate::run_codex_thread_one_shot;
use crate::config::Config;
use crate::config::Constrained;
use crate::error::CodexErr;
use crate::features::Feature;
use crate::review_format::format_review_findings_block;
use crate::review_format::render_review_output_text;
use crate::session::session::Session;
use crate::session::turn_context::TurnContext;
use crate::state::TaskKind;
use codex_protocol::user_input::UserInput;

use super::SessionTask;
use super::SessionTaskContext;

#[derive(Clone, Copy)]
pub(crate) struct ReviewTask;

impl ReviewTask {
    pub(crate) fn new() -> Self {
        Self
    }
}

#[derive(Clone)]
pub(crate) struct ReviewDelegateConfigParams<'a> {
    pub(crate) base_instructions: &'a str,
    pub(crate) sandbox_policy: SandboxPolicy,
    pub(crate) disable_collab: bool,
    pub(crate) instruction_profile: ReviewDelegateInstructionProfile,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReviewDelegateInstructionProfile {
    Review,
    PostTurnCompletionReview,
}

impl ReviewDelegateInstructionProfile {
    pub(crate) fn host_instruction_filenames(self, config: &Config) -> Vec<&str> {
        match self {
            ReviewDelegateInstructionProfile::Review => {
                vec![
                    config.review_agents_filename.as_str(),
                    config.host_agents_filename.as_str(),
                ]
            }
            ReviewDelegateInstructionProfile::PostTurnCompletionReview => {
                vec![
                    config.post_turn_completion_review_agents_filename.as_str(),
                    config.review_agents_filename.as_str(),
                    config.host_agents_filename.as_str(),
                ]
            }
        }
    }
}

pub(crate) fn configure_review_delegate_config(
    parent_config: &Config,
    parent_model_slug: &str,
    params: ReviewDelegateConfigParams<'_>,
) -> Result<Config, CodexErr> {
    let mut sub_agent_config = parent_config.clone();
    sub_agent_config.web_search_mode = Some(WebSearchMode::Disabled);
    sub_agent_config
        .features
        .disable(Feature::WebSearchRequest)
        .disable(Feature::WebSearchCached);
    if params.disable_collab {
        sub_agent_config.features.disable(Feature::Collab);
    }

    sub_agent_config.base_instructions = Some(params.base_instructions.to_string());
    let host_instruction_filenames = params
        .instruction_profile
        .host_instruction_filenames(parent_config);
    sub_agent_config.user_instructions =
        parent_config.load_host_instructions_from_filenames(&host_instruction_filenames)?;
    sub_agent_config.approval_policy = Constrained::allow_any(AskForApproval::Never);
    sub_agent_config.sandbox_policy = Constrained::allow_any(params.sandbox_policy);

    let model = parent_config
        .review_model
        .clone()
        .unwrap_or_else(|| parent_model_slug.to_string());
    sub_agent_config.model = Some(model);
    let provider_id = parent_config
        .review_model_provider
        .as_deref()
        .or_else(|| {
            sub_agent_config
                .model
                .as_deref()
                .and_then(|model| parent_config.overlay_model_provider_id_for_model(model))
        })
        .map(str::to_string);
    if let Some(provider_id) = provider_id {
        apply_delegate_model_provider(&mut sub_agent_config, &provider_id)?;
        sub_agent_config.features.disable(Feature::RemoteModels);
    }

    Ok(sub_agent_config)
}

impl SessionTask for ReviewTask {
    fn kind(&self) -> TaskKind {
        TaskKind::Review
    }

    async fn run(
        self: Arc<Self>,
        session: Arc<SessionTaskContext>,
        ctx: Arc<TurnContext>,
        input: Vec<UserInput>,
        cancellation_token: CancellationToken,
    ) -> Option<String> {
        session
            .session
            .services
            .otel_manager
            .counter("codex.task.review", 1, &[]);

        // Start sub-codex conversation and get the receiver for events.
        let output = match start_review_conversation(
            session.clone(),
            ctx.clone(),
            input,
            cancellation_token.clone(),
        )
        .await
        {
            Ok(receiver) => process_review_events(session.clone(), ctx.clone(), receiver).await,
            Err(err) => {
                session
                    .clone_session()
                    .send_event(
                        ctx.as_ref(),
                        EventMsg::Error(err.to_error_event(Some("Review task failed".to_string()))),
                    )
                    .await;
                None
            }
        };
        if !cancellation_token.is_cancelled() {
            exit_review_mode(session.clone_session(), output.clone(), ctx.clone()).await;
        }
        None
    }

    async fn abort(
        &self,
        session: Arc<SessionTaskContext>,
        ctx: Arc<TurnContext>,
        _reason: super::TaskStopReason,
    ) {
        exit_review_mode(session.clone_session(), None, ctx).await;
    }
}

async fn start_review_conversation(
    session: Arc<SessionTaskContext>,
    ctx: Arc<TurnContext>,
    input: Vec<UserInput>,
    cancellation_token: CancellationToken,
) -> Result<async_channel::Receiver<Event>, CodexErr> {
    let config = ctx.config.clone();
    let instruction_profile = ReviewDelegateInstructionProfile::Review;
    let sub_agent_config = configure_review_delegate_config(
        config.as_ref(),
        ctx.model_info.slug.as_str(),
        ReviewDelegateConfigParams {
            base_instructions: config.review_prompt(),
            sandbox_policy: ctx.sandbox_policy.clone(),
            disable_collab: true,
            instruction_profile,
        },
    )?;
    let agents_summary = instruction_profile
        .host_instruction_filenames(config.as_ref())
        .join(", ");

    run_codex_thread_one_shot(
        sub_agent_config,
        session.auth_manager(),
        session.models_manager(),
        input,
        session.clone_session(),
        ctx.clone(),
        cancellation_token,
        SubAgentSource::Review,
        DelegateRuntimeContextParams {
            task_kind: Some("review".to_string()),
            parent_turn_id: None,
            agents_summary: Some(agents_summary),
        },
        None,
    )
    .await
    .map(|io| io.rx_event)
}

async fn process_review_events(
    session: Arc<SessionTaskContext>,
    ctx: Arc<TurnContext>,
    receiver: async_channel::Receiver<Event>,
) -> Option<ReviewOutputEvent> {
    let mut prev_agent_message: Option<Event> = None;
    while let Ok(event) = receiver.recv().await {
        match event.clone().msg {
            EventMsg::AgentMessage(_) => {
                if let Some(prev) = prev_agent_message.take() {
                    session
                        .clone_session()
                        .send_event_transient(ctx.as_ref(), prev.msg)
                        .await;
                }
                prev_agent_message = Some(event);
            }
            // Suppress ItemCompleted only for assistant messages: forwarding it
            // would trigger legacy AgentMessage via as_legacy_events(), which this
            // review flow intentionally hides in favor of structured output.
            EventMsg::ItemCompleted(ItemCompletedEvent {
                item: TurnItem::AgentMessage(_),
                ..
            })
            | EventMsg::AgentMessageDelta(AgentMessageDeltaEvent { .. })
            | EventMsg::AgentMessageContentDelta(AgentMessageContentDeltaEvent { .. }) => {}
            EventMsg::UserMessage(_)
            | EventMsg::AgentReasoning(_)
            | EventMsg::AgentReasoningRawContent(_) => {
                // The delegate emits these legacy events immediately after the
                // structured ItemCompleted events. Forwarding both would make
                // the parent regenerate and persist duplicate legacy records.
            }
            EventMsg::TurnComplete(task_complete) => {
                // Parse review output from the last agent message (if present).
                let out = task_complete
                    .last_agent_message
                    .as_deref()
                    .map(parse_review_output_event);
                return out;
            }
            EventMsg::TurnAborted(_) | EventMsg::TurnPaused(_) => {
                // Cancellation or abort: consumer will finalize with None.
                return None;
            }
            _ => {
                session
                    .clone_session()
                    .send_event_transient_raw(event)
                    .await;
            }
        }
    }
    // Channel closed without TurnComplete: treat as interrupted.
    None
}

/// Parse a ReviewOutputEvent from a text blob returned by the reviewer model.
/// If the text is valid JSON matching ReviewOutputEvent, deserialize it.
/// Otherwise, attempt to extract the first JSON object substring and parse it.
/// If parsing still fails, return a structured fallback carrying the plain text
/// in `overall_explanation`.
fn parse_review_output_event(text: &str) -> ReviewOutputEvent {
    if let Ok(ev) = serde_json::from_str::<ReviewOutputEvent>(text) {
        return ev;
    }
    if let (Some(start), Some(end)) = (text.find('{'), text.rfind('}'))
        && start < end
        && let Some(slice) = text.get(start..=end)
        && let Ok(ev) = serde_json::from_str::<ReviewOutputEvent>(slice)
    {
        return ev;
    }
    ReviewOutputEvent {
        overall_explanation: text.to_string(),
        ..Default::default()
    }
}

/// Emits an ExitedReviewMode Event with optional ReviewOutput,
/// and records a developer message with the review output.
pub(crate) async fn exit_review_mode(
    session: Arc<Session>,
    review_output: Option<ReviewOutputEvent>,
    ctx: Arc<TurnContext>,
) {
    const REVIEW_USER_MESSAGE_ID: &str = "review_rollout_user";
    const REVIEW_ASSISTANT_MESSAGE_ID: &str = "review_rollout_assistant";
    let (user_message, assistant_message) = if let Some(out) = review_output.clone() {
        let mut findings_str = String::new();
        let text = out.overall_explanation.trim();
        if !text.is_empty() {
            findings_str.push_str(text);
        }
        if !out.findings.is_empty() {
            let block = format_review_findings_block(&out.findings, None);
            findings_str.push_str(&format!("\n{block}"));
        }
        let rendered = render_review_exit_success(&findings_str);
        let assistant_message = render_review_output_text(&out);
        (rendered, assistant_message)
    } else {
        let rendered = normalize_review_template_line_endings(
            crate::client_common::REVIEW_EXIT_INTERRUPTED_TMPL,
        )
        .into_owned();
        let assistant_message =
            "Review was interrupted. Please re-run /review and wait for it to complete."
                .to_string();
        (rendered, assistant_message)
    };

    session
        .record_conversation_items(
            &ctx,
            &[ResponseItem::Message {
                id: Some(REVIEW_USER_MESSAGE_ID.to_string()),
                role: "user".to_string(),
                content: vec![ContentItem::InputText { text: user_message }],
                end_turn: None,
                phase: None,
            }],
        )
        .await;
    session
        .send_event(
            ctx.as_ref(),
            EventMsg::ExitedReviewMode(ExitedReviewModeEvent {
                review_output,
                post_turn_completion_review_output: None,
            }),
        )
        .await;
    session
        .record_response_item_and_emit_turn_item(
            ctx.as_ref(),
            ResponseItem::Message {
                id: Some(REVIEW_ASSISTANT_MESSAGE_ID.to_string()),
                role: "assistant".to_string(),
                content: vec![ContentItem::OutputText {
                    text: assistant_message,
                }],
                end_turn: None,
                phase: None,
            },
        )
        .await;

    // Review output is synthetic and can be the first user-visible turn data.
    // Flush after client-facing events/items have been emitted so persistence is
    // durable without delaying review-mode exit.
    session.flush_rollout().await;
}

fn render_review_exit_success(results: &str) -> String {
    const RESULTS_PLACEHOLDER: &str = "{{results}}";

    let template =
        normalize_review_template_line_endings(crate::client_common::REVIEW_EXIT_SUCCESS_TMPL);
    let count = template.match_indices(RESULTS_PLACEHOLDER).count();
    assert_eq!(
        1, count,
        "review exit success template must contain exactly one {RESULTS_PLACEHOLDER} placeholder"
    );
    template.replacen(RESULTS_PLACEHOLDER, results, 1)
}

fn normalize_review_template_line_endings(template: &str) -> Cow<'_, str> {
    if template.contains('\r') {
        Cow::Owned(template.replace("\r\n", "\n").replace('\r', "\n"))
    } else {
        Cow::Borrowed(template)
    }
}

#[cfg(test)]
mod tests {
    use super::ReviewDelegateConfigParams;
    use super::ReviewDelegateInstructionProfile;
    use super::configure_review_delegate_config;
    use super::normalize_review_template_line_endings;
    use super::render_review_exit_success;
    use crate::config::test_config;
    use crate::features::Feature;
    use crate::protocol::AskForApproval;
    use crate::protocol::SandboxPolicy;
    use codex_protocol::config_types::WebSearchMode;
    use pretty_assertions::assert_eq;
    use tempfile::TempDir;

    #[test]
    fn render_review_exit_success_replaces_results_placeholder() {
        assert_eq!(
            render_review_exit_success("Finding A\nFinding B"),
            "<user_action>\n  <context>User initiated a review task. Here's the full review output from reviewer model. User may select one or more comments to resolve.</context>\n  <action>review</action>\n  <results>\n  Finding A\nFinding B\n  </results>\n  </user_action>\n"
        );
    }

    #[test]
    fn normalize_review_template_line_endings_rewrites_crlf_and_cr() {
        assert_eq!(
            normalize_review_template_line_endings("<user_action>\r\n  <results>\r  None.\r\n"),
            "<user_action>\n  <results>\n  None.\n"
        );
    }

    #[test]
    fn configure_review_delegate_forces_review_restrictions() {
        let mut config = test_config();
        config.review_model = Some("review-model".to_string());
        config.features.enable(Feature::WebSearchRequest);
        config.features.enable(Feature::WebSearchCached);
        config.features.enable(Feature::Collab);

        let delegate = configure_review_delegate_config(
            &config,
            "parent-model",
            ReviewDelegateConfigParams {
                base_instructions: "review prompt",
                sandbox_policy: SandboxPolicy::new_read_only_policy(),
                disable_collab: true,
                instruction_profile: ReviewDelegateInstructionProfile::Review,
            },
        )
        .expect("delegate config");

        assert_eq!(delegate.model.as_deref(), Some("review-model"));
        assert_eq!(delegate.base_instructions.as_deref(), Some("review prompt"));
        assert_eq!(delegate.web_search_mode, Some(WebSearchMode::Disabled));
        assert!(!delegate.features.enabled(Feature::WebSearchRequest));
        assert!(!delegate.features.enabled(Feature::WebSearchCached));
        assert!(!delegate.features.enabled(Feature::Collab));
        assert_eq!(*delegate.approval_policy.get(), AskForApproval::Never);
        assert!(matches!(
            delegate.sandbox_policy.get(),
            SandboxPolicy::ReadOnly { .. }
        ));
    }

    #[test]
    fn review_delegate_loads_review_host_instructions() {
        let codex_home = TempDir::new().expect("codex home");
        std::fs::write(codex_home.path().join("AGENTS.md"), "main host")
            .expect("write host agents");
        std::fs::write(codex_home.path().join("AGENTS.review.md"), "review host")
            .expect("write review agents");
        let mut config = test_config();
        config.codex_home = codex_home.path().to_path_buf();
        config.user_instructions = Some("stale parent host".to_string());

        let delegate = configure_review_delegate_config(
            &config,
            "parent-model",
            ReviewDelegateConfigParams {
                base_instructions: "review prompt",
                sandbox_policy: SandboxPolicy::new_read_only_policy(),
                disable_collab: true,
                instruction_profile: ReviewDelegateInstructionProfile::Review,
            },
        )
        .expect("delegate config");

        assert_eq!(delegate.user_instructions.as_deref(), Some("review host"));
    }

    #[test]
    fn review_delegate_falls_back_to_shared_host_instructions() {
        let codex_home = TempDir::new().expect("codex home");
        std::fs::write(codex_home.path().join("AGENTS.md"), "main host")
            .expect("write host agents");
        let mut config = test_config();
        config.codex_home = codex_home.path().to_path_buf();
        config.user_instructions = Some("stale parent host".to_string());

        let delegate = configure_review_delegate_config(
            &config,
            "parent-model",
            ReviewDelegateConfigParams {
                base_instructions: "review prompt",
                sandbox_policy: SandboxPolicy::new_read_only_policy(),
                disable_collab: true,
                instruction_profile: ReviewDelegateInstructionProfile::Review,
            },
        )
        .expect("delegate config");

        assert_eq!(delegate.user_instructions.as_deref(), Some("main host"));
    }

    #[test]
    fn post_turn_review_delegate_uses_post_then_review_then_host_instructions() {
        let codex_home = TempDir::new().expect("codex home");
        std::fs::write(codex_home.path().join("AGENTS.md"), "main host")
            .expect("write host agents");
        std::fs::write(codex_home.path().join("AGENTS.review.md"), "review host")
            .expect("write review agents");
        std::fs::write(
            codex_home.path().join("AGENTS.post-turn-review.md"),
            "post host",
        )
        .expect("write post-turn review agents");
        let mut config = test_config();
        config.codex_home = codex_home.path().to_path_buf();

        let delegate = configure_review_delegate_config(
            &config,
            "parent-model",
            ReviewDelegateConfigParams {
                base_instructions: "review prompt",
                sandbox_policy: SandboxPolicy::new_read_only_policy(),
                disable_collab: true,
                instruction_profile: ReviewDelegateInstructionProfile::PostTurnCompletionReview,
            },
        )
        .expect("delegate config");

        assert_eq!(delegate.user_instructions.as_deref(), Some("post host"));

        std::fs::write(codex_home.path().join("AGENTS.post-turn-review.md"), " \n")
            .expect("empty post-turn review agents");
        let delegate = configure_review_delegate_config(
            &config,
            "parent-model",
            ReviewDelegateConfigParams {
                base_instructions: "review prompt",
                sandbox_policy: SandboxPolicy::new_read_only_policy(),
                disable_collab: true,
                instruction_profile: ReviewDelegateInstructionProfile::PostTurnCompletionReview,
            },
        )
        .expect("delegate config");

        assert_eq!(delegate.user_instructions.as_deref(), Some("review host"));

        std::fs::write(codex_home.path().join("AGENTS.review.md"), " \n")
            .expect("empty review agents");
        let delegate = configure_review_delegate_config(
            &config,
            "parent-model",
            ReviewDelegateConfigParams {
                base_instructions: "review prompt",
                sandbox_policy: SandboxPolicy::new_read_only_policy(),
                disable_collab: true,
                instruction_profile: ReviewDelegateInstructionProfile::PostTurnCompletionReview,
            },
        )
        .expect("delegate config");

        assert_eq!(delegate.user_instructions.as_deref(), Some("main host"));
    }

    #[test]
    fn review_delegate_uses_configured_host_instruction_filenames() {
        let codex_home = TempDir::new().expect("codex home");
        std::fs::write(codex_home.path().join("HOST.md"), "custom host")
            .expect("write custom host agents");
        std::fs::write(codex_home.path().join("REVIEW.md"), "custom review")
            .expect("write custom review agents");
        std::fs::write(codex_home.path().join("POST.md"), "custom post")
            .expect("write custom post-turn review agents");
        std::fs::write(codex_home.path().join("AGENTS.review.md"), "default review")
            .expect("write default review agents");
        let mut config = test_config();
        config.codex_home = codex_home.path().to_path_buf();
        config.host_agents_filename = "HOST.md".to_string();
        config.review_agents_filename = "REVIEW.md".to_string();
        config.post_turn_completion_review_agents_filename = "POST.md".to_string();

        let review_delegate = configure_review_delegate_config(
            &config,
            "parent-model",
            ReviewDelegateConfigParams {
                base_instructions: "review prompt",
                sandbox_policy: SandboxPolicy::new_read_only_policy(),
                disable_collab: true,
                instruction_profile: ReviewDelegateInstructionProfile::Review,
            },
        )
        .expect("review delegate config");
        let post_delegate = configure_review_delegate_config(
            &config,
            "parent-model",
            ReviewDelegateConfigParams {
                base_instructions: "review prompt",
                sandbox_policy: SandboxPolicy::new_read_only_policy(),
                disable_collab: true,
                instruction_profile: ReviewDelegateInstructionProfile::PostTurnCompletionReview,
            },
        )
        .expect("post-turn delegate config");

        assert_eq!(
            review_delegate.user_instructions.as_deref(),
            Some("custom review")
        );
        assert_eq!(
            post_delegate.user_instructions.as_deref(),
            Some("custom post")
        );
    }
}
