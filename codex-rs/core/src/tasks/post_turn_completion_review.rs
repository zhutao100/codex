use std::fmt::Write as _;
use std::sync::Arc;

use codex_protocol::items::TurnItem;
use codex_protocol::models::DeveloperInstructions;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::AgentMessageContentDeltaEvent;
use codex_protocol::protocol::AgentMessageDeltaEvent;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ExitedReviewModeEvent;
use codex_protocol::protocol::InitialHistory;
use codex_protocol::protocol::ItemCompletedEvent;
use codex_protocol::protocol::PostTurnCompletionReviewOutputEvent;
use codex_protocol::protocol::SandboxPolicy;
use codex_protocol::protocol::SubAgentSource;
use codex_protocol::protocol::TurnContinuationSource;
use codex_protocol::user_input::UserInput;
use tokio_util::sync::CancellationToken;

use crate::codex_delegate::DelegateRuntimeContextParams;
use crate::codex_delegate::run_codex_thread_one_shot;
use crate::error::CodexErr;
use crate::session::session::Session;
use crate::session::turn_context::TurnContext;
use crate::state::CompletedTurnForReview;
use crate::state::PendingContinuation;
use crate::state::TaskKind;

use super::ReviewDelegateConfigParams;
use super::ReviewDelegateInstructionProfile;
use super::SessionTask;
use super::SessionTaskContext;
use super::configure_review_delegate_config;

#[derive(Clone)]
pub(crate) struct PostTurnCompletionReviewTask {
    completed_turn: CompletedTurnForReview,
}

impl PostTurnCompletionReviewTask {
    pub(crate) fn new(completed_turn: CompletedTurnForReview) -> Self {
        Self { completed_turn }
    }
}

impl SessionTask for PostTurnCompletionReviewTask {
    fn kind(&self) -> TaskKind {
        TaskKind::PostTurnCompletionReview
    }

    async fn run(
        self: Arc<Self>,
        session: Arc<SessionTaskContext>,
        ctx: Arc<TurnContext>,
        _input: Vec<UserInput>,
        cancellation_token: CancellationToken,
    ) -> Option<String> {
        session.session.services.otel_manager.counter(
            "codex.task.post_turn_completion_review",
            1,
            &[],
        );

        let output = match start_post_turn_completion_review_conversation(
            session.clone(),
            ctx.clone(),
            &self.completed_turn,
            cancellation_token.clone(),
        )
        .await
        {
            Ok(receiver) => {
                process_post_turn_completion_review_events(session.clone(), ctx.clone(), receiver)
                    .await
            }
            Err(err) => {
                session
                    .clone_session()
                    .send_event(
                        ctx.as_ref(),
                        EventMsg::Error(err.to_error_event(Some(
                            "Post-turn completion review failed".to_string(),
                        ))),
                    )
                    .await;
                None
            }
        };

        if !cancellation_token.is_cancelled() {
            exit_post_turn_completion_review_mode(
                session.clone_session(),
                output.clone(),
                ctx.clone(),
            )
            .await;
            if let Some(output) = output
                && output.fix_actions_advised
            {
                record_advisory_and_request_continuation(
                    session.clone_session(),
                    ctx,
                    &self.completed_turn.turn_id,
                    &output,
                )
                .await;
            }
        }
        None
    }

    async fn abort(&self, session: Arc<SessionTaskContext>, ctx: Arc<TurnContext>) {
        exit_post_turn_completion_review_mode(session.clone_session(), None, ctx).await;
    }
}

async fn start_post_turn_completion_review_conversation(
    session: Arc<SessionTaskContext>,
    ctx: Arc<TurnContext>,
    completed_turn: &CompletedTurnForReview,
    cancellation_token: CancellationToken,
) -> Result<async_channel::Receiver<Event>, CodexErr> {
    let instruction_profile = ReviewDelegateInstructionProfile::PostTurnCompletionReview;
    let sub_agent_config = configure_review_delegate_config(
        ctx.config.as_ref(),
        ctx.model_info.slug.as_str(),
        ReviewDelegateConfigParams {
            base_instructions: ctx.config.post_turn_completion_review_prompt(),
            sandbox_policy: SandboxPolicy::ReadOnly,
            disable_collab: true,
            instruction_profile,
        },
    )?;
    let agents_summary = instruction_profile
        .host_instruction_filenames(ctx.config.as_ref())
        .join(", ");

    let input = vec![UserInput::Text {
        text: render_completed_turn_context(completed_turn),
        text_elements: Vec::new(),
    }];

    run_codex_thread_one_shot(
        sub_agent_config,
        session.auth_manager(),
        session.models_manager(),
        input,
        session.clone_session(),
        ctx,
        cancellation_token,
        SubAgentSource::Review,
        DelegateRuntimeContextParams {
            task_kind: Some("post_turn_completion_review".to_string()),
            parent_turn_id: Some(completed_turn.turn_id.clone()),
            agents_summary: Some(agents_summary),
        },
        Some(InitialHistory::New),
    )
    .await
    .map(|io| io.rx_event)
}

async fn process_post_turn_completion_review_events(
    session: Arc<SessionTaskContext>,
    ctx: Arc<TurnContext>,
    receiver: async_channel::Receiver<Event>,
) -> Option<PostTurnCompletionReviewOutputEvent> {
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
            EventMsg::ItemCompleted(ItemCompletedEvent {
                item: TurnItem::AgentMessage(_),
                ..
            })
            | EventMsg::AgentMessageDelta(AgentMessageDeltaEvent { .. })
            | EventMsg::AgentMessageContentDelta(AgentMessageContentDeltaEvent { .. }) => {}
            EventMsg::UserMessage(_)
            | EventMsg::AgentReasoning(_)
            | EventMsg::AgentReasoningRawContent(_) => {}
            EventMsg::TurnComplete(task_complete) => {
                let out = task_complete
                    .last_agent_message
                    .as_deref()
                    .map(parse_post_turn_completion_review_output_event)
                    .unwrap_or_else(empty_post_turn_completion_review_output);
                return Some(out);
            }
            EventMsg::TurnAborted(_) | EventMsg::TurnPaused(_) => {
                return None;
            }
            other => {
                session
                    .clone_session()
                    .send_event_transient(ctx.as_ref(), other)
                    .await;
            }
        }
    }
    None
}

fn parse_post_turn_completion_review_output_event(
    text: &str,
) -> PostTurnCompletionReviewOutputEvent {
    if let Ok(ev) = serde_json::from_str::<PostTurnCompletionReviewOutputEvent>(text) {
        return normalize_post_turn_completion_review_output(ev);
    }
    if let (Some(start), Some(end)) = (text.find('{'), text.rfind('}'))
        && start < end
        && let Some(slice) = text.get(start..=end)
        && let Ok(ev) = serde_json::from_str::<PostTurnCompletionReviewOutputEvent>(slice)
    {
        return normalize_post_turn_completion_review_output(ev);
    }

    let evaluation = if text.trim().is_empty() {
        "Post-turn reviewer produced no evaluation.".to_string()
    } else {
        text.to_string()
    };
    PostTurnCompletionReviewOutputEvent {
        evaluation,
        fix_actions_advised: false,
    }
}

fn normalize_post_turn_completion_review_output(
    mut output: PostTurnCompletionReviewOutputEvent,
) -> PostTurnCompletionReviewOutputEvent {
    if output.evaluation.trim().is_empty() {
        output.evaluation = "Post-turn reviewer produced no evaluation.".to_string();
    }
    output
}

fn empty_post_turn_completion_review_output() -> PostTurnCompletionReviewOutputEvent {
    PostTurnCompletionReviewOutputEvent {
        evaluation: "Post-turn reviewer produced no evaluation.".to_string(),
        fix_actions_advised: false,
    }
}

async fn exit_post_turn_completion_review_mode(
    session: Arc<Session>,
    output: Option<PostTurnCompletionReviewOutputEvent>,
    ctx: Arc<TurnContext>,
) {
    session
        .send_event(
            ctx.as_ref(),
            EventMsg::ExitedReviewMode(ExitedReviewModeEvent {
                review_output: None,
                post_turn_completion_review_output: output,
            }),
        )
        .await;
}

async fn record_advisory_and_request_continuation(
    session: Arc<Session>,
    ctx: Arc<TurnContext>,
    reviewed_turn_id: &str,
    output: &PostTurnCompletionReviewOutputEvent,
) {
    let message = render_advisory_developer_message(output);
    let item: ResponseItem = DeveloperInstructions::new(message).into();
    session
        .record_conversation_items(ctx.as_ref(), std::slice::from_ref(&item))
        .await;
    session
        .set_pending_post_turn_completion_review_continuation(Some(PendingContinuation {
            source: TurnContinuationSource::PostTurnCompletionReview,
            continued_from_turn_id: Some(reviewed_turn_id.to_string()),
        }))
        .await;
}

fn render_completed_turn_context(completed_turn: &CompletedTurnForReview) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "<completed_turn_review_context>");
    let _ = writeln!(out, "  <turn_id>{}</turn_id>", completed_turn.turn_id);
    let _ = writeln!(
        out,
        "  <cwd><![CDATA[{}]]></cwd>",
        cdata_escape(&completed_turn.cwd.display().to_string())
    );
    let _ = writeln!(out, "  <user_messages>");
    for (index, message) in completed_turn.user_messages.iter().enumerate() {
        let _ = writeln!(
            out,
            "    <message index=\"{}\"><![CDATA[{}]]></message>",
            index + 1,
            cdata_escape(message)
        );
    }
    let _ = writeln!(out, "  </user_messages>");
    let _ = writeln!(
        out,
        "  <final_agent_message><![CDATA[{}]]></final_agent_message>",
        cdata_escape(&completed_turn.final_agent_message)
    );
    let _ = writeln!(out, "</completed_turn_review_context>");
    out
}

fn render_advisory_developer_message(output: &PostTurnCompletionReviewOutputEvent) -> String {
    format!(
        "<post_turn_completion_review>\n  <context>An independent post-turn review inspected the completed turn. The review may be correct or incorrect. Treat it as advisory input, verify the claims against the repository, and only act on findings that are actually applicable.</context>\n  <fix_actions_advised>true</fix_actions_advised>\n  <evaluation>\n{}\n  </evaluation>\n</post_turn_completion_review>",
        output.evaluation
    )
}

fn cdata_escape(text: &str) -> String {
    text.replace("]]>", "]]]]><![CDATA[>")
}

#[cfg(test)]
mod tests {
    use super::parse_post_turn_completion_review_output_event;
    use pretty_assertions::assert_eq;

    #[test]
    fn parses_json_object_inside_text() {
        let output = parse_post_turn_completion_review_output_event(
            "prefix {\"evaluation\":\"Needs a test\",\"fix_actions_advised\":true} suffix",
        );

        assert_eq!(output.evaluation, "Needs a test");
        assert!(output.fix_actions_advised);
    }

    #[test]
    fn unparsable_text_is_non_continuing_evaluation() {
        let output = parse_post_turn_completion_review_output_event("not json");

        assert_eq!(output.evaluation, "not json");
        assert!(!output.fix_actions_advised);
    }

    #[test]
    fn prompt_requires_coverage_driven_inspection() {
        let prompt = crate::POST_TURN_COMPLETION_REVIEW_PROMPT;

        assert!(prompt.contains("Do not perform a generic code review"));
        assert!(prompt.contains("keyword-search plus narrow range reads"));
        assert!(prompt.contains("coverage-driven"));
        assert!(prompt.contains("Inspection coverage:"));
    }
}
