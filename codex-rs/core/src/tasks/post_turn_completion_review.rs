use std::fmt::Write as _;
use std::sync::Arc;

use codex_protocol::items::TurnItem;
use codex_protocol::protocol::AgentMessageContentDeltaEvent;
use codex_protocol::protocol::AgentMessageDeltaEvent;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ExitedReviewModeEvent;
use codex_protocol::protocol::InitialHistory;
use codex_protocol::protocol::ItemCompletedEvent;
use codex_protocol::protocol::ItemStartedEvent;
use codex_protocol::protocol::PostTurnCompletionReviewOutputEvent;
use codex_protocol::protocol::SandboxPolicy;
use codex_protocol::protocol::SubAgentSource;
use codex_protocol::user_input::UserInput;
use tokio_util::sync::CancellationToken;

use crate::codex_delegate::DelegateRuntimeContextParams;
use crate::codex_delegate::run_codex_thread_one_shot;
use crate::completion_review::AUTOMATIC_REVIEW_LIMIT;
use crate::completion_review::ReviewTicket;
use crate::error::CodexErr;
use crate::session::session::Session;
use crate::session::turn_context::TurnContext;
use crate::state::CompletedTurnForReview;
use crate::state::PendingContinuation;
use crate::state::TaskKind;
use crate::truncate::approx_token_count;

use super::ReviewDelegateConfigParams;
use super::ReviewDelegateInstructionProfile;
use super::SessionTask;
use super::SessionTaskContext;
use super::configure_review_delegate_config;

#[derive(Clone)]
pub(crate) struct PostTurnCompletionReviewTask {
    completed_turn: CompletedTurnForReview,
    ticket: ReviewTicket,
    resumed_from: Option<PendingContinuation>,
}

impl PostTurnCompletionReviewTask {
    pub(crate) fn new(completed_turn: CompletedTurnForReview, ticket: ReviewTicket) -> Self {
        Self {
            completed_turn,
            ticket,
            resumed_from: None,
        }
    }

    pub(crate) fn resumed(
        completed_turn: CompletedTurnForReview,
        ticket: ReviewTicket,
        resumed_from: PendingContinuation,
    ) -> Self {
        Self {
            completed_turn,
            ticket,
            resumed_from: Some(resumed_from),
        }
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
        let sess = session.clone_session();
        if !self.ticket.matches(&self.completed_turn)
            || !sess.completion_review_ticket_is_current(&self.ticket).await
        {
            return None;
        }
        session.session.services.otel_manager.counter(
            "codex.task.post_turn_completion_review",
            1,
            &[],
        );

        if let Some(checkpoint) = self.resumed_from.clone() {
            session
                .clone_session()
                .send_event_raw_flushed(Event {
                    id: ctx.sub_id.clone(),
                    msg: EventMsg::TurnContinued(codex_protocol::protocol::TurnContinuedEvent {
                        continued_from_turn_id: checkpoint.continued_from_turn_id,
                        source: checkpoint.source,
                    }),
                })
                .await;
        }

        let delegate_cancel = cancellation_token.child_token();
        let output = match start_post_turn_completion_review_conversation(
            session.clone(),
            ctx.clone(),
            &self.completed_turn,
            delegate_cancel.clone(),
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
        delegate_cancel.cancel();

        if !cancellation_token.is_cancelled()
            && sess.completion_review_ticket_is_current(&self.ticket).await
        {
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
                    &self.ticket,
                    &output,
                )
                .await;
            } else {
                sess.finish_completion_review(&self.ticket).await;
            }
        }
        None
    }

    async fn abort(
        &self,
        session: Arc<SessionTaskContext>,
        ctx: Arc<TurnContext>,
        reason: super::TaskStopReason,
    ) {
        if !matches!(reason, super::TaskStopReason::Pause(_)) {
            exit_post_turn_completion_review_mode(session.clone_session(), None, ctx).await;
        }
    }
}

async fn start_post_turn_completion_review_conversation(
    session: Arc<SessionTaskContext>,
    ctx: Arc<TurnContext>,
    completed_turn: &CompletedTurnForReview,
    cancellation_token: CancellationToken,
) -> Result<async_channel::Receiver<Event>, CodexErr> {
    let instruction_profile = ReviewDelegateInstructionProfile::PostTurnCompletionReview;
    let mut sub_agent_config = configure_review_delegate_config(
        ctx.config.as_ref(),
        ctx.model_info.slug.as_str(),
        ReviewDelegateConfigParams {
            base_instructions: ctx.config.post_turn_completion_review_prompt(),
            sandbox_policy: SandboxPolicy::new_read_only_policy(),
            disable_collab: true,
            instruction_profile,
        },
    )?;
    sub_agent_config.cwd = completed_turn.cwd.clone();
    sub_agent_config.base_instructions = Some(format!(
        "{}\n\n{REVIEW_CONTEXT_CONTRACT}",
        sub_agent_config
            .base_instructions
            .as_deref()
            .unwrap_or_default()
    ));
    let agents_summary = instruction_profile
        .host_instruction_filenames(ctx.config.as_ref())
        .join(", ");

    let model = sub_agent_config
        .model
        .as_deref()
        .unwrap_or(&ctx.model_info.slug);
    let model_info = session
        .models_manager()
        .get_model_info(model, &sub_agent_config)
        .await;
    let skills = session
        .clone_session()
        .services
        .skills_manager
        .skills_for_config(&sub_agent_config);
    let enabled_skills = skills.enabled_skills();
    let project_instructions =
        crate::project_doc::get_user_instructions(&sub_agent_config, Some(&enabled_skills)).await;
    // Leave half of the effective model window for tools, output and inspection.
    // Project instructions use the same loader as the fresh delegate.
    let tools_config =
        crate::tools::spec::ToolsConfig::new(&crate::tools::spec::ToolsConfigParams {
            model_info: &model_info,
            features: &sub_agent_config.features,
            web_search_mode: Some(codex_protocol::config_types::WebSearchMode::Disabled),
        });
    let tools = crate::tools::ToolRouter::from_config(&tools_config, None, &[]).specs();
    let tool_tokens = approx_token_count(
        &serde_json::to_string(&tools).map_err(|error| CodexErr::Fatal(error.to_string()))?,
    );
    let instruction_tokens = approx_token_count(
        sub_agent_config
            .base_instructions
            .as_deref()
            .unwrap_or_default(),
    )
    .saturating_add(approx_token_count(
        project_instructions.as_deref().unwrap_or_default(),
    ))
    .saturating_add(approx_token_count(
        sub_agent_config
            .developer_instructions
            .as_deref()
            .unwrap_or_default(),
    ));
    let effective_window = model_info
        .resolved_context_window()
        .map(|window| window.saturating_mul(model_info.effective_context_window_percent) / 100)
        .ok_or_else(|| {
            CodexErr::Fatal(
                "Review coverage unavailable: reviewer context capacity is unknown.".to_string(),
            )
        })?;
    let packet_budget = usize::try_from(effective_window / 2)
        .unwrap_or_default()
        .saturating_sub(instruction_tokens)
        .saturating_sub(tool_tokens)
        .saturating_sub(8_192);
    let text =
        budgeted_completed_turn_context(completed_turn, packet_budget).map_err(CodexErr::Fatal)?;
    let input = vec![UserInput::Text {
        text,
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
    while let Ok(event) = receiver.recv().await {
        match event.clone().msg {
            EventMsg::AgentMessage(_) => {}
            EventMsg::ContextCompacted(_)
            | EventMsg::ItemStarted(ItemStartedEvent {
                item: TurnItem::ContextCompaction(_),
                ..
            })
            | EventMsg::ItemCompleted(ItemCompletedEvent {
                item: TurnItem::ContextCompaction(_),
                ..
            }) => {
                session.clone_session().notify_background_event(
                    ctx.as_ref(),
                    "Post-turn review unavailable: the delegate reached a compaction boundary, so full request and candidate coverage is no longer established. No review verdict was accepted.",
                ).await;
                return None;
            }
            EventMsg::ItemStarted(ItemStartedEvent {
                item: TurnItem::AgentMessage(_) | TurnItem::Plan(_) | TurnItem::UserMessage(_),
                ..
            })
            | EventMsg::ItemCompleted(ItemCompletedEvent {
                item: TurnItem::AgentMessage(_) | TurnItem::Plan(_) | TurnItem::UserMessage(_),
                ..
            })
            | EventMsg::PlanDelta(_)
            | EventMsg::AgentMessageDelta(AgentMessageDeltaEvent { .. })
            | EventMsg::AgentMessageContentDelta(AgentMessageContentDeltaEvent { .. }) => {}
            EventMsg::UserMessage(_)
            | EventMsg::AgentReasoning(_)
            | EventMsg::AgentReasoningRawContent(_) => {}
            EventMsg::TurnComplete(task_complete) => {
                let out = task_complete
                    .last_agent_message
                    .as_deref()
                    .ok_or("Post-turn reviewer produced no final evaluation.")
                    .and_then(parse_post_turn_completion_review_output_event);
                return match out {
                    Ok(output) => Some(output),
                    Err(message) => {
                        session.clone_session().notify_background_event(
                            ctx.as_ref(),
                            format!("Post-turn review unavailable: {message} No review verdict was established."),
                        ).await;
                        None
                    }
                };
            }
            EventMsg::TurnAborted(_) | EventMsg::TurnPaused(_) => {
                return None;
            }
            _ => {
                session
                    .clone_session()
                    .send_event_transient_with_id(event.id, event.msg)
                    .await;
            }
        }
    }
    None
}

fn parse_post_turn_completion_review_output_event(
    text: &str,
) -> Result<PostTurnCompletionReviewOutputEvent, &'static str> {
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

    Err("Reviewer output was empty or not a valid review object.")
}

fn normalize_post_turn_completion_review_output(
    output: PostTurnCompletionReviewOutputEvent,
) -> Result<PostTurnCompletionReviewOutputEvent, &'static str> {
    if output.evaluation.trim().is_empty() {
        return Err("Reviewer evaluation was empty.");
    }
    Ok(output)
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
    ticket: &ReviewTicket,
    output: &PostTurnCompletionReviewOutputEvent,
) {
    if ticket.automatic
        && session
            .completed_turn_for_review()
            .await
            .is_some_and(|episode| episode.automatic_reviews_started >= AUTOMATIC_REVIEW_LIMIT)
    {
        session.notify_background_event(
            ctx.as_ref(),
            "Automatic completion review stopped with unresolved advice after three attempts. The findings above remain unresolved; request /review-completed-turn for another manual review.",
        ).await;
        session.finish_completion_review(ticket).await;
        return;
    }
    if !session
        .prepare_completion_review_handoff(
            ticket,
            render_advisory_developer_message(output),
            ctx.model_info.slug.clone(),
        )
        .await
        && session.completion_review_ticket_is_current(ticket).await
    {
        session.notify_background_event(
            ctx.as_ref(),
            "Post-turn review advice could not be prepared for the current request and configuration; no continuation was started.",
        ).await;
    }
}

pub(crate) fn render_completed_turn_context(
    completed_turn: &CompletedTurnForReview,
    include_reference: bool,
) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "<completed_turn_review_context version=\"2\">");
    let _ = writeln!(
        out,
        "  <episode_id><![CDATA[{}]]></episode_id>",
        cdata_escape(&completed_turn.episode_id)
    );
    let _ = writeln!(
        out,
        "  <request_revision>{}</request_revision>",
        completed_turn.request_revision
    );
    let _ = writeln!(
        out,
        "  <candidate_completion_id><![CDATA[{}]]></candidate_completion_id>",
        cdata_escape(&completed_turn.turn_id)
    );
    let _ = writeln!(
        out,
        "  <cwd><![CDATA[{}]]></cwd>",
        cdata_escape(&completed_turn.cwd.display().to_string())
    );
    let _ = writeln!(
        out,
        "  <request_context provenance=\"exact\">\n    <prior_user_rounds>"
    );
    for (round_index, round) in completed_turn
        .request_frame
        .prior_user_rounds
        .iter()
        .enumerate()
    {
        let _ = writeln!(out, "    <round index=\"{}\">", round_index + 1);
        let _ = writeln!(out, "      <user_messages>");
        for (message_index, message) in round.user_messages.iter().enumerate() {
            let _ = writeln!(
                out,
                "        <message index=\"{}\"><![CDATA[{}]]></message>",
                message_index + 1,
                cdata_escape(message)
            );
        }
        let _ = writeln!(out, "      </user_messages>");
        let _ = writeln!(
            out,
            "      <final_agent_message><![CDATA[{}]]></final_agent_message>",
            cdata_escape(&round.final_agent_message)
        );
        let _ = writeln!(out, "    </round>");
    }
    let _ = writeln!(out, "    </prior_user_rounds>\n    <current_user_messages>");
    for message in &completed_turn.request_frame.current_user_messages {
        let _ = writeln!(
            out,
            "      <message><![CDATA[{}]]></message>",
            cdata_escape(message)
        );
    }
    let _ = writeln!(out, "    </current_user_messages>\n  </request_context>");
    let reference_status = match completed_turn.initial_answer_reference.as_deref() {
        Some(reference) if reference == completed_turn.final_agent_message => {
            let _ = writeln!(
                out,
                "  <initial_answer_reference status=\"identical_to_candidate\" />"
            );
            "identical_to_candidate"
        }
        Some(reference) if include_reference => {
            let _ = writeln!(
                out,
                "  <initial_answer_reference status=\"historical_unverified\"><![CDATA[{}]]></initial_answer_reference>",
                cdata_escape(reference)
            );
            "included"
        }
        Some(_) => {
            let _ = writeln!(
                out,
                "  <initial_answer_reference status=\"omitted_for_budget\" />"
            );
            "omitted_for_budget"
        }
        None => {
            let _ = writeln!(out, "  <initial_answer_reference status=\"unavailable\" />");
            "unavailable"
        }
    };
    let _ = writeln!(
        out,
        "  <candidate_final_answer><![CDATA[{}]]></candidate_final_answer>",
        cdata_escape(&completed_turn.final_agent_message)
    );
    let _ = writeln!(
        out,
        "  <context_coverage request=\"complete\" candidate=\"complete\" reference=\"{reference_status}\" />"
    );
    let _ = writeln!(out, "</completed_turn_review_context>");
    out
}

fn budgeted_completed_turn_context(
    completed_turn: &CompletedTurnForReview,
    budget: usize,
) -> Result<String, String> {
    if !completed_turn.request_frame.exact || !completed_turn.request_frame.text_only {
        return Err("Review coverage unavailable: exact consumed user context or required nontext input is unavailable in the text review projection. No full review was run.".to_string());
    }
    let with_reference = render_completed_turn_context(completed_turn, true);
    if approx_token_count(&with_reference) <= budget {
        return Ok(with_reference);
    }
    let mandatory = render_completed_turn_context(completed_turn, false);
    if approx_token_count(&mandatory) <= budget {
        return Ok(mandatory);
    }
    Err("Review coverage unavailable: mandatory user context and the exact candidate exceed the resolved reviewer model's payload budget. No truncated review was run.".to_string())
}

pub(crate) const REPLACEMENT_ANSWER_CONTRACT: &str = "After completing applicable work, write one self-contained final answer to the user's active request. This final answer replaces your earlier final answer for the user. Preserve still-valid outcomes and requested explanations, incorporate all material corrections, remove superseded claims, identify current deliverables and verification actually performed, and state unresolved work or limitations. The user must be able to understand the current result without reading earlier assistant final messages or review feedback. Do not make the final answer only a response to the reviewer. This requirement also applies when you reject every finding or make no file changes. Respect the user's requested format and brevity.";

const REVIEW_CONTEXT_CONTRACT: &str = "The version 2 completed_turn_review_context identifies the exact candidate_final_answer delivered to the user and the genuine consumed user request. Evaluate both current artifacts and whether that candidate stands alone as the user's current answer. initial_answer_reference and prior user-round answers are historical, unverified context that may contain superseded claims; they cannot compensate for omissions in the candidate. A delta-only answer or dropped still-required explanation is actionable incomplete delivery even when the repository is correct. Inspect artifacts to distinguish answer defects from work defects. Prior reviewer findings are not new user requirements. Return a nonempty evaluation and a boolean fix_actions_advised in the specified JSON object.";

fn render_advisory_developer_message(output: &PostTurnCompletionReviewOutputEvent) -> String {
    format!(
        "<post_turn_completion_review>\n  <context>An independent post-turn review inspected the completed turn. The review may be correct or incorrect. Treat it as advisory input, verify the claims against the repository and active user request, and only act on findings that are actually applicable.</context>\n  <final_answer_contract>{REPLACEMENT_ANSWER_CONTRACT}</final_answer_contract>\n  <fix_actions_advised>true</fix_actions_advised>\n  <evaluation><![CDATA[{}]]></evaluation>\n</post_turn_completion_review>",
        cdata_escape(&output.evaluation)
    )
}

fn cdata_escape(text: &str) -> String {
    text.replace("]]>", "]]]]><![CDATA[>")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::completion_review::CompletedTurnReviewRound;
    use crate::state::CompletedTurnForReview;
    use codex_protocol::ThreadId;
    use codex_protocol::protocol::AskForApproval;
    use codex_protocol::protocol::Event;
    use codex_protocol::protocol::EventMsg;
    use codex_protocol::protocol::ExecCommandBeginEvent;
    use codex_protocol::protocol::ExecCommandEndEvent;
    use codex_protocol::protocol::ExecCommandSource;
    use codex_protocol::protocol::ReasoningContentDeltaEvent;
    use codex_protocol::protocol::RuntimeContextScope;
    use codex_protocol::protocol::RuntimeContextSnapshot;
    use codex_protocol::protocol::RuntimeContextUpdatedEvent;
    use codex_protocol::protocol::SessionSource;
    use codex_protocol::protocol::SubAgentSource;
    use codex_protocol::protocol::TurnAbortReason;
    use codex_protocol::protocol::TurnAbortedEvent;
    use codex_protocol::protocol::TurnCompleteEvent;
    use pretty_assertions::assert_eq;
    use std::path::PathBuf;
    use std::sync::Arc;
    use tokio::time::Duration;
    use tokio::time::timeout;

    #[test]
    fn parses_json_object_inside_text() {
        let output = parse_post_turn_completion_review_output_event(
            "prefix {\"evaluation\":\"Needs a test\",\"fix_actions_advised\":true} suffix",
        )
        .unwrap();

        assert_eq!(output.evaluation, "Needs a test");
        assert!(output.fix_actions_advised);
    }

    #[test]
    fn unparsable_text_is_not_a_review_verdict() {
        let output = parse_post_turn_completion_review_output_event("not json");

        assert!(output.is_err());
    }

    #[test]
    fn prompt_requires_coverage_driven_inspection() {
        let prompt = crate::POST_TURN_COMPLETION_REVIEW_PROMPT;

        assert!(prompt.contains("Do not perform a generic code review"));
        assert!(prompt.contains("keyword-search plus narrow range reads"));
        assert!(prompt.contains("coverage-driven"));
        assert!(prompt.contains("request-fulfillment checklist"));
        assert!(prompt.contains("incomplete fulfillment"));
        assert!(prompt.contains("Inspection coverage:"));
    }

    #[test]
    fn renders_complete_session_interaction_history() {
        let completed_turn = CompletedTurnForReview {
            turn_id: "turn-2".to_string(),
            cwd: PathBuf::from("/tmp/project"),
            request_frame: crate::completion_review::ReviewRequestFrame {
                prior_user_rounds: vec![CompletedTurnReviewRound {
                    user_messages: vec!["first request".to_string()],
                    final_agent_message: "first final".to_string(),
                }],
                current_user_messages: vec!["second request".to_string()],
                exact: true,
                text_only: true,
            },
            final_agent_message: "second final".to_string(),
            ..Default::default()
        };

        let rendered = render_completed_turn_context(&completed_turn, true);

        assert!(rendered.contains("<round index=\"1\">"));
        assert!(rendered.contains("<![CDATA[first request]]>"));
        assert!(rendered.contains("<![CDATA[first final]]>"));
        assert!(rendered.contains("<current_user_messages>"));
        assert!(rendered.contains("<![CDATA[second request]]>"));
        assert!(rendered.contains("<![CDATA[second final]]>"));
    }

    fn episode_fixture() -> CompletedTurnForReview {
        CompletedTurnForReview {
            episode_id: "episode".into(),
            turn_id: "completion".into(),
            request_frame: crate::completion_review::ReviewRequestFrame {
                current_user_messages: vec!["original user request".into()],
                exact: true,
                text_only: true,
                ..Default::default()
            },
            initial_answer_reference: Some("first answer".into()),
            final_agent_message: "latest complete answer".into(),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn a_current_ticket_cannot_review_a_different_candidate_snapshot() {
        let (session, context, events) =
            crate::session::tests::make_session_and_context_with_rx().await;
        let revision = session
            .accept_completion_review_input(
                &[UserInput::Text {
                    text: "original request".into(),
                    text_elements: vec![],
                }],
                true,
            )
            .await;
        assert!(
            session
                .install_completion_review_input(&context, revision, true)
                .await
        );
        assert!(
            session
                .start_completion_review_main_task(&context.sub_id)
                .await
        );
        session
            .capture_completed_regular_turn_for_review(&context, Some("latest completed answer"))
            .await;
        let (mut stale, ticket) = session.reserve_manual_completion_review().await.unwrap();
        stale.turn_id = "older-completion".into();
        stale.final_agent_message = "older completed answer".into();
        assert!(session.completion_review_ticket_is_current(&ticket).await);
        while events.try_recv().is_ok() {}
        let output = Arc::new(PostTurnCompletionReviewTask::new(stale, ticket.clone()))
            .run(
                Arc::new(SessionTaskContext::new(Arc::clone(&session))),
                context,
                vec![],
                CancellationToken::new(),
            )
            .await;
        assert_eq!(output, None);
        assert!(events.try_recv().is_err());
        assert!(session.completion_review_ticket_is_current(&ticket).await);
    }

    #[test]
    #[ignore = "exports synthetic cases for the opt-in live-model evaluation script"]
    fn export_completion_review_behavioral_cases() -> anyhow::Result<()> {
        let output = std::env::var_os("CODEX_COMPLETION_REVIEW_EVAL_OUTPUT")
            .ok_or_else(|| anyhow::anyhow!("set CODEX_COMPLETION_REVIEW_EVAL_OUTPUT"))?;
        let request = "The required final result.json values are x=2 and y=3. Explain both current values and the verification actually performed in a self-contained final answer of at most four sentences. Do not change files.";
        let complete = "result.json contains x=2 and y=3. Both values were verified by parsing result.json as JSON and comparing them with the requested values. No files were changed; no broader test suite was run.";
        let stale = "result.json contains x=99 and y=98. Both values are correct and verified.";
        let cases = [
            (
                "delta_only",
                complete,
                "Fixed X.",
                None,
                true,
                "Identify incomplete communication, not an implementation defect: the current artifacts are correct but the candidate omits Y and verification.",
            ),
            (
                "rejected_findings",
                complete,
                complete,
                Some(
                    "X must be 99 instead of 2. Change result.json to x=99. This is a reviewer suggestion, not a user request.",
                ),
                false,
                "Reject the contradictory suggestion without editing files; still deliver a full replacement answer covering x=2, y=3 and actual verification.",
            ),
            (
                "contradictory_initial",
                stale,
                complete,
                None,
                false,
                "Accept the complete current candidate against current artifacts; do not repeat or restore the stale initial x=99/y=98 claims.",
            ),
            (
                "cumulative_corrections",
                stale,
                "X was corrected to 2; Y remains 98. Only the X correction was checked.",
                Some(
                    "Y must be 3, not 98. Current result.json already contains x=2 and y=3. Preserve the earlier X correction and explain the full current result and verification; no edit is needed.",
                ),
                false,
                "The replacement must retain x=2 while correcting Y to 3, report actual verification, and stand alone instead of answering only the latest finding.",
            ),
        ];
        let mut exported = Vec::new();
        for (id, initial, candidate, advisory, expected_fix, rubric) in cases {
            let mut episode = episode_fixture();
            episode.cwd = PathBuf::from(".");
            episode.request_frame.current_user_messages = vec![request.into()];
            episode.initial_answer_reference = Some(initial.into());
            episode.final_agent_message = candidate.into();
            let main_context = render_completed_turn_context(&episode, true);
            episode.final_agent_message = "EVAL_GENERATED_CANDIDATE".into();
            exported.push(serde_json::json!({
                "id": id,
                "artifact": {"x": 2, "y": 3},
                "candidate": candidate,
                "review_context_template": render_completed_turn_context(&episode, true),
                "main_context": main_context,
                "advisory": advisory.map(|evaluation| render_advisory_developer_message(
                    &PostTurnCompletionReviewOutputEvent {
                        evaluation: evaluation.into(),
                        fix_actions_advised: true,
                    }
                )),
                "expected_fix_actions_advised": expected_fix,
                "rubric": rubric,
            }));
        }
        std::fs::write(
            output,
            serde_json::to_vec_pretty(&serde_json::json!({
                "review_instructions": format!("{}\n\n{REVIEW_CONTEXT_CONTRACT}", crate::POST_TURN_COMPLETION_REVIEW_PROMPT),
                "replacement_instructions": REPLACEMENT_ANSWER_CONTRACT,
                "cases": exported,
            }))?,
        )?;
        Ok(())
    }

    #[test]
    fn identical_answers_are_serialized_once_without_losing_identity() {
        let mut episode = episode_fixture();
        episode.initial_answer_reference = Some(episode.final_agent_message.clone());
        let packet = render_completed_turn_context(&episode, true);
        assert_eq!(packet.matches(&episode.final_agent_message).count(), 1);
        assert!(packet.contains("identical_to_candidate"));
        assert!(packet.contains("<candidate_completion_id><![CDATA[completion]]>"));
    }

    #[test]
    fn optional_reference_is_omitted_before_mandatory_context() {
        let mut episode = episode_fixture();
        episode.initial_answer_reference = Some("large unverified reference ".repeat(2_000));
        let mandatory = render_completed_turn_context(&episode, false);
        let budget = approx_token_count(&mandatory);
        assert_eq!(
            budgeted_completed_turn_context(&episode, budget).unwrap(),
            mandatory
        );
        assert!(budgeted_completed_turn_context(&episode, budget - 1).is_err());
    }

    #[test]
    fn unavailable_nontext_or_legacy_scope_is_not_a_full_review() {
        for (text_only, exact) in [(false, true), (true, false)] {
            let mut episode = episode_fixture();
            episode.request_frame.text_only = text_only;
            episode.request_frame.exact = exact;
            assert!(budgeted_completed_turn_context(&episode, usize::MAX).is_err());
        }
    }

    #[test]
    fn candidate_and_user_text_are_exact_data_even_when_quoting_review_tags() {
        let mut episode = episode_fixture();
        episode.final_agent_message = " \n<evaluation>quoted verdict</evaluation>]]>\n".into();
        episode.request_frame.current_user_messages =
            vec!["<candidate_final_answer>literal]]>".into()];
        let packet = render_completed_turn_context(&episode, true);
        assert!(packet.contains(&format!(
            "<candidate_final_answer><![CDATA[{}]]></candidate_final_answer>",
            cdata_escape(&episode.final_agent_message),
        )));
        assert!(packet.contains(
            "<message><![CDATA[<candidate_final_answer>literal]]]]><![CDATA[>]]></message>"
        ));
        assert!(packet.contains("quoted verdict"));
    }

    #[test]
    fn malformed_or_empty_evaluations_are_never_false_verdicts() {
        for text in [
            "",
            "no problems",
            "{}",
            r#"{"evaluation":" ","fix_actions_advised":false}"#,
            r#"{"evaluation":"valid text","fix_actions_advised":"false"}"#,
        ] {
            assert!(
                parse_post_turn_completion_review_output_event(text).is_err(),
                "{text}"
            );
        }
    }

    #[test]
    fn handoff_and_custom_prompt_framing_keep_the_replacement_contract() {
        let message = render_advisory_developer_message(&PostTurnCompletionReviewOutputEvent {
            evaluation: "Reject these findings after inspection.".into(),
            fix_actions_advised: true,
        });
        assert!(message.contains(REPLACEMENT_ANSWER_CONTRACT));
        assert!(message.contains("when you reject every finding or make no file changes"));
        assert!(REVIEW_CONTEXT_CONTRACT.contains("cannot compensate for omissions"));
        assert!(REVIEW_CONTEXT_CONTRACT.contains("delta-only"));
    }

    #[tokio::test]
    async fn delegate_answer_and_plan_output_cannot_replace_the_main_copyable_answer() {
        let (session, ctx, events) =
            crate::session::tests::make_session_and_context_with_rx().await;
        let (sender, receiver) = async_channel::bounded(8);
        for msg in [
            EventMsg::AgentMessage(codex_protocol::protocol::AgentMessageEvent {
                message: "review commentary".into(),
            }),
            EventMsg::ItemCompleted(ItemCompletedEvent {
                thread_id: ThreadId::new(),
                turn_id: "review".into(),
                item: TurnItem::Plan(codex_protocol::items::PlanItem {
                    id: "plan".into(),
                    text: "not the user's final".into(),
                }),
            }),
            EventMsg::PlanDelta(codex_protocol::protocol::PlanDeltaEvent {
                thread_id: "review".into(),
                turn_id: "review".into(),
                item_id: "plan".into(),
                delta: "review plan".into(),
            }),
            EventMsg::TurnComplete(TurnCompleteEvent {
                last_agent_message: Some(
                    r#"{"evaluation":"checked","fix_actions_advised":false}"#.into(),
                ),
            }),
        ] {
            sender
                .send(Event {
                    id: "review".into(),
                    msg,
                })
                .await
                .unwrap();
        }
        let output = process_post_turn_completion_review_events(
            Arc::new(SessionTaskContext::new(session)),
            ctx,
            receiver,
        )
        .await;
        assert_eq!(
            output,
            Some(PostTurnCompletionReviewOutputEvent {
                evaluation: "checked".into(),
                fix_actions_advised: false
            })
        );
        assert!(events.try_recv().is_err());
    }

    #[tokio::test]
    async fn delegate_compaction_cannot_produce_a_full_review_verdict() {
        let (session, ctx, events) =
            crate::session::tests::make_session_and_context_with_rx().await;
        let (sender, receiver) = async_channel::bounded(1);
        sender
            .send(Event {
                id: "compaction".into(),
                msg: EventMsg::ItemStarted(ItemStartedEvent {
                    thread_id: ThreadId::new(),
                    turn_id: "review".into(),
                    item: TurnItem::ContextCompaction(
                        codex_protocol::items::ContextCompactionItem::new(),
                    ),
                }),
            })
            .await
            .unwrap();
        let output = process_post_turn_completion_review_events(
            Arc::new(SessionTaskContext::new(session)),
            ctx,
            receiver,
        )
        .await;
        assert_eq!(output, None);
        assert!(
            matches!(events.recv().await.unwrap().msg, EventMsg::BackgroundEvent(event)
            if event.message.contains("No review verdict was accepted"))
        );
        assert!(events.try_recv().is_err());
    }

    fn runtime_context_snapshot() -> RuntimeContextSnapshot {
        RuntimeContextSnapshot {
            scope_id: "delegate-scope".to_string(),
            scope: RuntimeContextScope::Delegate,
            task_kind: Some("post_turn_completion_review".to_string()),
            session_source: SessionSource::SubAgent(SubAgentSource::Review),
            session_id: ThreadId::new(),
            parent_session_id: Some(ThreadId::new()),
            parent_turn_id: Some("regular-turn".to_string()),
            thread_name: Some("Post-turn review".to_string()),
            rollout_path: None,
            cwd: PathBuf::from("/tmp/review"),
            model: "gpt-5-review".to_string(),
            model_provider_id: "openai".to_string(),
            approval_policy: AskForApproval::Never,
            sandbox_policy: super::SandboxPolicy::new_read_only_policy(),
            reasoning_effort: None,
            service_tier: None,
            model_context_window: Some(128_000),
            agents_summary: None,
            token_info: None,
        }
    }

    #[tokio::test]
    async fn process_events_preserves_delegate_runtime_context_event_ids() {
        let (session, ctx, rx_events) =
            crate::session::tests::make_session_and_context_with_rx().await;
        let session_ctx = Arc::new(SessionTaskContext::new(session));
        let (tx_delegate, rx_delegate) = async_channel::bounded(4);

        let process = tokio::spawn(process_post_turn_completion_review_events(
            session_ctx,
            ctx,
            rx_delegate,
        ));

        tx_delegate
            .send(Event {
                id: "0".to_string(),
                msg: EventMsg::RuntimeContextUpdated(RuntimeContextUpdatedEvent {
                    snapshot: runtime_context_snapshot(),
                }),
            })
            .await
            .expect("send runtime context update");

        let forwarded = rx_events.recv().await.expect("forwarded runtime context");
        assert_eq!(forwarded.id, "0");
        assert!(matches!(forwarded.msg, EventMsg::RuntimeContextUpdated(_)));

        tx_delegate
            .send(Event {
                id: "0".to_string(),
                msg: EventMsg::TurnComplete(TurnCompleteEvent {
                    last_agent_message: None,
                }),
            })
            .await
            .expect("send turn complete");
        drop(tx_delegate);

        timeout(Duration::from_secs(1), process)
            .await
            .expect("process_post_turn_completion_review_events hung")
            .expect("process join failed");
    }

    #[tokio::test]
    async fn process_events_expands_transient_legacy_events() {
        let (session, ctx, rx_events) =
            crate::session::tests::make_session_and_context_with_rx().await;
        let session_ctx = Arc::new(SessionTaskContext::new(session));
        let (tx_delegate, rx_delegate) = async_channel::bounded(4);

        let process = tokio::spawn(process_post_turn_completion_review_events(
            session_ctx,
            ctx,
            rx_delegate,
        ));

        tx_delegate
            .send(Event {
                id: "delegate-delta".to_string(),
                msg: EventMsg::ReasoningContentDelta(ReasoningContentDeltaEvent {
                    thread_id: "delegate-thread".to_string(),
                    turn_id: "delegate-turn".to_string(),
                    item_id: "reasoning-item".to_string(),
                    delta: "checking files".to_string(),
                    summary_index: 0,
                }),
            })
            .await
            .expect("send reasoning delta");

        let structured = timeout(Duration::from_secs(1), rx_events.recv())
            .await
            .expect("structured event timed out")
            .expect("structured event");
        assert_eq!(structured.id, "delegate-delta");
        assert!(matches!(structured.msg, EventMsg::ReasoningContentDelta(_)));

        let legacy = timeout(Duration::from_secs(1), rx_events.recv())
            .await
            .expect("legacy event timed out")
            .expect("legacy event");
        assert_eq!(legacy.id, "delegate-delta");
        assert!(matches!(legacy.msg, EventMsg::AgentReasoningDelta(_)));

        tx_delegate
            .send(Event {
                id: "delegate-terminal".to_string(),
                msg: EventMsg::TurnAborted(TurnAbortedEvent {
                    reason: TurnAbortReason::Interrupted,
                }),
            })
            .await
            .expect("send terminal event");
        drop(tx_delegate);

        timeout(Duration::from_secs(1), process)
            .await
            .expect("process_post_turn_completion_review_events hung")
            .expect("process join failed");
    }

    #[tokio::test]
    async fn process_events_preserves_read_reasoning_and_later_exec_cascade() {
        let (session, ctx, rx_events) =
            crate::session::tests::make_session_and_context_with_rx().await;
        let session_ctx = Arc::new(SessionTaskContext::new(session));
        let (tx_delegate, rx_delegate) = async_channel::bounded(8);

        let process = tokio::spawn(process_post_turn_completion_review_events(
            session_ctx,
            ctx,
            rx_delegate,
        ));

        let read_begin = exec_begin("call-read", "cat ~/.codex/AGENTS_structured_search.md");
        let inspect_begin = exec_begin("call-inspect", "git show --stat HEAD");
        let delegate_events = [
            Event {
                id: "read-begin".to_string(),
                msg: EventMsg::ExecCommandBegin(read_begin.clone()),
            },
            Event {
                id: "read-end".to_string(),
                msg: EventMsg::ExecCommandEnd(exec_end(
                    &read_begin,
                    "structured search guidance\n",
                )),
            },
            Event {
                id: "reasoning-delta".to_string(),
                msg: EventMsg::ReasoningContentDelta(ReasoningContentDeltaEvent {
                    thread_id: "delegate-thread".to_string(),
                    turn_id: "delegate-turn".to_string(),
                    item_id: "reasoning-item".to_string(),
                    delta: "checking files".to_string(),
                    summary_index: 0,
                }),
            },
            Event {
                id: "inspect-begin".to_string(),
                msg: EventMsg::ExecCommandBegin(inspect_begin.clone()),
            },
            Event {
                id: "inspect-end".to_string(),
                msg: EventMsg::ExecCommandEnd(exec_end(&inspect_begin, "commit abc123\n")),
            },
        ];

        for event in delegate_events {
            tx_delegate.send(event).await.expect("send delegate event");
        }

        let read_begin = recv_event(&rx_events).await;
        assert_eq!(read_begin.id, "read-begin");
        assert!(matches!(read_begin.msg, EventMsg::ExecCommandBegin(_)));

        let read_end = recv_event(&rx_events).await;
        assert_eq!(read_end.id, "read-end");
        assert!(matches!(read_end.msg, EventMsg::ExecCommandEnd(_)));

        let structured_reasoning = recv_event(&rx_events).await;
        assert_eq!(structured_reasoning.id, "reasoning-delta");
        assert!(matches!(
            structured_reasoning.msg,
            EventMsg::ReasoningContentDelta(_)
        ));

        let legacy_reasoning = recv_event(&rx_events).await;
        assert_eq!(legacy_reasoning.id, "reasoning-delta");
        assert!(matches!(
            legacy_reasoning.msg,
            EventMsg::AgentReasoningDelta(_)
        ));

        let inspect_begin = recv_event(&rx_events).await;
        assert_eq!(inspect_begin.id, "inspect-begin");
        assert!(matches!(inspect_begin.msg, EventMsg::ExecCommandBegin(_)));

        let inspect_end = recv_event(&rx_events).await;
        assert_eq!(inspect_end.id, "inspect-end");
        assert!(matches!(inspect_end.msg, EventMsg::ExecCommandEnd(_)));

        tx_delegate
            .send(Event {
                id: "delegate-terminal".to_string(),
                msg: EventMsg::TurnAborted(TurnAbortedEvent {
                    reason: TurnAbortReason::Interrupted,
                }),
            })
            .await
            .expect("send terminal event");
        drop(tx_delegate);

        timeout(Duration::from_secs(1), process)
            .await
            .expect("process_post_turn_completion_review_events hung")
            .expect("process join failed");
    }

    fn exec_begin(call_id: &str, raw_cmd: &str) -> ExecCommandBeginEvent {
        ExecCommandBeginEvent {
            call_id: call_id.to_string(),
            process_id: None,
            turn_id: "delegate-turn".to_string(),
            command: vec!["bash".to_string(), "-lc".to_string(), raw_cmd.to_string()],
            cwd: PathBuf::from("/tmp"),
            parsed_cmd: Vec::new(),
            source: ExecCommandSource::Agent,
            interaction_input: None,
        }
    }

    fn exec_end(begin: &ExecCommandBeginEvent, output: &str) -> ExecCommandEndEvent {
        ExecCommandEndEvent {
            call_id: begin.call_id.clone(),
            process_id: begin.process_id.clone(),
            turn_id: begin.turn_id.clone(),
            command: begin.command.clone(),
            cwd: begin.cwd.clone(),
            parsed_cmd: begin.parsed_cmd.clone(),
            source: begin.source,
            interaction_input: begin.interaction_input.clone(),
            stdout: output.to_string(),
            stderr: String::new(),
            aggregated_output: output.to_string(),
            exit_code: 0,
            duration: Duration::from_millis(5),
            formatted_output: output.to_string(),
        }
    }

    async fn recv_event(rx_events: &async_channel::Receiver<Event>) -> Event {
        timeout(Duration::from_secs(1), rx_events.recv())
            .await
            .expect("event timed out")
            .expect("event")
    }
}
