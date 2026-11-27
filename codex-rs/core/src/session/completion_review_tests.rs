use super::*;
use crate::completion_review::CompletionReviewReservationError;
use crate::completion_review::CompletionReviewState;
use crate::completion_review::ReviewAdvice;
use crate::completion_review::ReviewHandoffDisposition;
use crate::completion_review::ReviewInputInstallation;
use crate::completion_review::ReviewPromptFrame;
use crate::completion_review::ReviewPromptFrames;
use crate::completion_review::ReviewRequestFrame;
use crate::completion_review::ReviewSampling;
use crate::completion_review::ReviewSamplingInput;
use codex_protocol::models::ResponseInputItem;
use pretty_assertions::assert_eq;
use tokio::sync::Notify;

#[test_case::test_case(vec![], 0; "empty history")]
#[test_case::test_case(vec![assistant_message("local compacted summary")], 0; "assistant only history")]
#[test_case::test_case(
    vec![
        user_message("request"),
        assistant_message("initial answer"),
        DeveloperInstructions::new("review advisory").into(),
    ],
    3;
    "recorded advisory"
)]
#[test_case::test_case(
    vec![
        DeveloperInstructions::new("previous advisory").into(),
        assistant_message("previous work"),
        user_message("amended request"),
        assistant_message("work after steering"),
    ],
    3;
    "steered scope without handoff"
)]
#[test_case::test_case(
    vec![
        user_message("retained request"),
        ResponseItem::Compaction { encrypted_content: "opaque context".into() },
        assistant_message("exact work notes"),
    ],
    2;
    "encrypted compaction with notes"
)]
fn completion_review_request_frame_keeps_a_stable_prefix(
    mut history: Vec<ResponseItem>,
    expected_index: usize,
) {
    let frames = ReviewPromptFrames(vec![ReviewPromptFrame::new(
        "exact review frame".into(),
        ReviewPromptFrame::anchor(&history),
        None,
    )]);
    let mut request = history.clone();
    frames.insert(&mut request, None).unwrap();
    let mut expected = history[..expected_index].to_vec();
    expected.push(DeveloperInstructions::new("exact review frame").into());
    expected.extend_from_slice(&history[expected_index..]);
    assert_eq!(request, expected);

    let output = assistant_message("new follow-up output");
    history.push(output.clone());
    frames.insert(&mut history, None).unwrap();
    expected.push(output);
    assert_eq!(history, expected);
}

async fn completed_session() -> (Arc<Session>, Arc<TurnContext>) {
    let (session, context, _) = make_session_and_context_with_rx().await;
    complete_new_request(
        &session,
        &context,
        "original request",
        " initial complete answer\n",
    )
    .await;
    (session, context)
}

async fn propose_first_review(
    session: &Arc<Session>,
    context: &Arc<TurnContext>,
) -> (
    Arc<ReviewFrameProposal>,
    ReviewPromptFrames,
    Vec<ResponseItem>,
    ToolSurface,
) {
    let cancel = CancellationToken::new();
    let ReviewAdvice::FirstSend(advice) = session
        .admit_completion_review_advisory(context)
        .await
        .unwrap()
    else {
        panic!("expected pending advice")
    };
    let mut input = session.prompt_history(context).await;
    input.push(advice.item.clone());
    let tools = super::turn::compaction_tool_specs(session, context, &input, &cancel)
        .await
        .unwrap();
    let ReviewSampling::FirstSend {
        frames,
        mandatory_alternative,
    } = session
        .review_sampling(ReviewSamplingInput {
            items: &input,
            advice: Some(&advice),
        })
        .await
        .unwrap()
    else {
        panic!("expected proposed frame")
    };
    let mut projected = input.clone();
    let index = frames.insert(&mut projected, None).unwrap();
    let proposal = ReviewFrameProposal::new(
        session,
        context,
        frames.clone(),
        Some(advice.item),
        index.zip(mandatory_alternative),
    );
    (proposal, frames, input, tools)
}

async fn commit_first_review(
    session: &Arc<Session>,
    context: &Arc<TurnContext>,
) -> (ReviewPromptFrames, Vec<ResponseItem>, ToolSurface) {
    let (proposal, frames, input, tools) = propose_first_review(session, context).await;
    proposal.commit(0, i64::MAX, None).await.unwrap();
    proposal.commit(0, i64::MAX, None).await.unwrap();
    (frames, input, tools)
}

#[tokio::test]
async fn no_input_replacement_seed_replays_at_one_rollout_boundary() {
    let (session, context) = completed_session().await;
    let (_home, path) = attach_recorder(&session, &context).await;
    let summary = ResponseItem::Compaction {
        encrypted_content: "summary".into(),
    };
    session
        .replace_compacted_history(&context, vec![summary.clone()], None, String::new())
        .await;
    session
        .seed_initial_context_if_needed(&context)
        .await
        .unwrap();
    session
        .seed_initial_context_if_needed(&context)
        .await
        .unwrap();
    let (items, _, errors) = RolloutRecorder::load_rollout_items(&path).await.unwrap();
    assert_eq!(errors, 0);
    let seeds = items
        .iter()
        .enumerate()
        .filter(|(_, i)| matches!(i, RolloutItem::ContextSeed { .. }))
        .map(|(i, _)| i)
        .collect::<Vec<_>>();
    let [seed] = seeds[..] else {
        panic!("expected exactly one canonical seed")
    };
    let mut expected = vec![summary];
    let before = session
        .reconstruct_history_from_rollout(&context, &items[..seed])
        .await;
    assert_eq!(
        (before.history, before.reference_context_item),
        (expected.clone(), None)
    );
    expected.extend(session.build_initial_context(&context).await);
    let after = session
        .reconstruct_history_from_rollout(&context, &items[..=seed])
        .await;
    assert_eq!(
        (after.history, after.reference_context_item),
        (expected, Some(context.to_turn_context_item()))
    );
    assert_eq!(after.previous_turn_settings, before.previous_turn_settings);
}

#[test_case::test_case(None; "refused first send leaves only outgoing work")]
#[test_case::test_case(Some(false); "refused retry cannot shorten a dispatched full handoff")]
#[test_case::test_case(Some(true); "fitting retry cannot expand a dispatched mandatory handoff")]
#[tokio::test]
async fn review_refusal_replays_the_correct_side_of_dispatch(mandatory: Option<bool>) {
    let (session, context, _) = make_session_and_context_with_rx().await;
    complete_new_request(
        &session,
        &context,
        "Verify.",
        &"Distinct initial reference.\n".repeat(130),
    )
    .await;
    let (_home, path) = attach_recorder(&session, &context).await;
    let claim = |advice: &'static str| async {
        let ticket = session.begin_completion_review(false).await.unwrap();
        assert!(
            session
                .prepare_completion_review_handoff(
                    &ticket,
                    advice.into(),
                    context.model_info.slug.clone()
                )
                .await
        );
        session
            .claim_completion_review_handoff(&ticket.attempt_id, &context.sub_id, false)
            .await
            .unwrap();
    };
    claim("First advice.").await;
    let (outgoing, _, _) = commit_first_review(&session, &context).await;
    assert!(
        session
            .start_completion_review_main_task(&context.sub_id, &context)
            .await
    );
    session
        .record_conversation_items(&context, &[assistant_message("Another candidate.")])
        .await;
    session
        .capture_completed_regular_turn_for_review(&context, Some("Another candidate."))
        .await;
    claim("Second advice.").await;
    let advice = session
        .state
        .lock()
        .await
        .completion_review
        .handoff
        .as_ref()
        .unwrap()
        .advisory_item();
    let (proposal, frames, input, _) = propose_first_review(&session, &context).await;
    assert_eq!(frames.0[..frames.0.len() - 1], outgoing.0);
    let mut expected = input.clone();
    if let Some(short) = mandatory {
        let mut preferred = input.clone();
        let index = frames.insert(&mut preferred, None).unwrap().unwrap();
        expected = preferred.clone();
        let cost = 1_000 + frames.tokens();
        proposal
            .admit(cost, cost - i64::from(short), &mut expected, None)
            .await
            .unwrap();
        assert_eq!(expected[index] != preferred[index], short);
        let mut retry = preferred;
        let result = proposal
            .admit(
                cost,
                if short { cost + 1 } else { cost - 1 },
                &mut retry,
                None,
            )
            .await;
        if short {
            result.unwrap();
            assert_eq!(retry, expected);
        } else {
            assert!(matches!(
                result,
                Err(CodexErr::ReviewPayloadExceeded { .. })
            ));
        }
    } else {
        outgoing.insert(&mut expected, None).unwrap();
    }
    proposal
        .defer_refused(&context, None, &CancellationToken::new())
        .await
        .unwrap();
    if mandatory.is_none() {
        let saved = session
            .state
            .lock()
            .await
            .completion_review
            .deferred_capture_source
            .clone();
        let rebound = session
            .turn_context_with_model(&context, "other-work-model")
            .await
            .unwrap();
        proposal
            .defer_refused(&rebound, None, &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(
            session
                .state
                .lock()
                .await
                .completion_review
                .deferred_capture_source,
            saved
        );
    }
    let (items, _, errors) = RolloutRecorder::load_rollout_items(&path).await.unwrap();
    assert_eq!(errors, 0);
    let replay = session
        .reconstruct_history_from_rollout(&context, &items)
        .await;
    let review = replay.completion_review.unwrap();
    let dispatched = mandatory.is_some();
    assert_eq!(review.defer_new_prompt_frame, !dispatched);
    let mut recovered = input;
    review.prompt_frames.insert(&mut recovered, None).unwrap();
    assert_eq!(recovered, expected);
    assert_eq!(review.handoff.unwrap().advisory_recorded, dispatched);
    assert_eq!(
        replay.history.iter().filter(|i| **i == advice).count(),
        usize::from(dispatched)
    );
}

#[tokio::test]
async fn legacy_prepared_prompt_owns_its_reference_but_not_a_user_boundary() {
    let (session, context) = make_session_and_context().await;
    let reference = context.to_turn_context_item();
    for fresh in [true, false] {
        let mut review = CompletionReviewState::default();
        review.accept_input(
            fresh,
            vec![UserInput::Text {
                text: "Accepted input".into(),
                text_elements: vec![],
            }],
            None,
        );
        review.set_main_task("original", &reference.model);
        let pending = &mut review.pending_inputs[0];
        pending.installation = ReviewInputInstallation::Installing;
        pending.remaining_skill_items = Some(vec![]);
        let saved = serde_json::to_value(review).unwrap();
        assert!(
            saved["pending_inputs"][0]
                .get("remaining_context")
                .is_none()
        );
        let replay = session
            .reconstruct_history_from_rollout(
                &context,
                &[
                    RolloutItem::TurnContext(reference.clone()),
                    RolloutItem::PostTurnCompletionReviewState(saved),
                ],
            )
            .await;
        assert_eq!(
            replay.reference_context_item,
            fresh.then(|| reference.clone())
        );
        assert_eq!(replay.previous_turn_settings, None);
        assert!(replay.history.is_empty());
    }
}

async fn complete_new_request(
    session: &Arc<Session>,
    context: &Arc<TurnContext>,
    request: &str,
    answer: &str,
) {
    let revision = session
        .accept_completion_review_input(&steer_text_input(request), true, None)
        .await
        .unwrap();
    assert!(
        session
            .install_completion_review_input(context, revision, true)
            .await
    );
    assert!(
        session
            .start_completion_review_main_task(&context.sub_id, context)
            .await
    );
    session
        .capture_completed_regular_turn_for_review(context, Some(answer))
        .await;
}

fn metadata(state: &CompletionReviewState) -> RolloutItem {
    RolloutItem::PostTurnCompletionReviewState(serde_json::to_value(state).unwrap())
}

pub(in crate::session) async fn attach_recorder(
    session: &Arc<Session>,
    context: &TurnContext,
) -> (tempfile::TempDir, PathBuf) {
    let home = tempfile::tempdir().unwrap();
    let mut config = context.config.as_ref().clone();
    config.codex_home = home.path().to_path_buf();
    let recorder = RolloutRecorder::new(
        &config,
        crate::rollout::recorder::RolloutRecorderParams::new(
            session.conversation_id,
            None,
            context.session_source.clone(),
            "openai".into(),
            BaseInstructions {
                text: "test".into(),
            },
            vec![],
        ),
        None,
        None,
    )
    .await
    .unwrap();
    let path = recorder.rollout_path().to_path_buf();
    *session.services.rollout.lock().await = Some(recorder);
    (home, path)
}

#[tokio::test]
async fn replay_attributes_identical_advice_to_its_own_append() {
    let (session, context) = completed_session().await;
    let (_home, path) = attach_recorder(&session, &context).await;
    let ticket = session.begin_completion_review(false).await.unwrap();
    let text = "Check the evidence again.";
    assert!(
        session
            .prepare_completion_review_handoff(
                &ticket,
                text.into(),
                context.model_info.slug.clone()
            )
            .await
    );
    let identical = ResponseItem::from(DeveloperInstructions::new(text));
    session
        .record_conversation_items(&context, std::slice::from_ref(&identical))
        .await;
    session
        .claim_completion_review_handoff(&ticket.attempt_id, &context.sub_id, false)
        .await
        .unwrap();
    session.flush_rollout().await;
    let (items, _, errors) = RolloutRecorder::load_rollout_items(&path).await.unwrap();
    assert_eq!(errors, 0);
    let replay = session
        .reconstruct_history_from_rollout(&context, &items)
        .await;
    assert_eq!(replay.history.last(), Some(&identical));
    assert!(
        !replay
            .completion_review
            .as_ref()
            .unwrap()
            .handoff
            .as_ref()
            .unwrap()
            .advisory_recorded
    );
    session.replace_with_reconstructed_rollout(replay).await;
    commit_first_review(&session, &context).await;
    assert_eq!(
        session
            .prompt_history(&context)
            .await
            .iter()
            .filter(|item| **item == identical)
            .count(),
        2
    );
    // A later interrupt and repeated stale metadata cannot turn the owned write
    // back into a pending advisory, or confer ownership on the earlier copy.
    session.flush_rollout().await;
    let (mut written, _, errors) = RolloutRecorder::load_rollout_items(&path).await.unwrap();
    assert_eq!(errors, 0);
    let append = written
        .iter()
        .rposition(|i| matches!(i, RolloutItem::ResponseItem(item) if item == &identical))
        .unwrap();
    written.truncate(append + 1);
    written.push(RolloutItem::ResponseItem(user_message(
        "<turn_aborted>\nInterrupted.\n</turn_aborted>",
    )));
    let stale = items
        .iter()
        .rev()
        .find(|i| matches!(i, RolloutItem::PostTurnCompletionReviewState(_)))
        .unwrap();
    written.push(stale.clone());
    let replay = session
        .reconstruct_history_from_rollout(&context, &written)
        .await;
    assert!(
        replay
            .completion_review
            .unwrap()
            .handoff
            .unwrap()
            .advisory_recorded
    );
}

#[tokio::test]
async fn completion_review_manual_reservation_distinguishes_missing_and_pending_work() {
    let (session, _, _) = make_session_and_context_with_rx().await;
    assert_eq!(
        session.reserve_manual_completion_review().await,
        Err(CompletionReviewReservationError::NoCompletedTurn)
    );
    session
        .accept_completion_review_input(&steer_text_input("accepted request"), true, None)
        .await
        .unwrap();
    assert_eq!(
        session.reserve_manual_completion_review().await,
        Err(CompletionReviewReservationError::PendingInput)
    );
}

#[tokio::test]
async fn review_delegate_keeps_transcript_without_completion_review_state() {
    let (session, mut context, _events) = make_session_and_context_with_rx().await;
    let source = SessionSource::SubAgent(SubAgentSource::Review);
    session
        .state
        .lock()
        .await
        .session_configuration
        .session_source = source.clone();
    Arc::get_mut(&mut context).unwrap().session_source = source;
    let initial_state = session.state.lock().await.completion_review.clone();
    let (_home, path) = attach_recorder(&session, &context).await;
    let content = steer_text_input("inspect the completed answer");
    let review_revision = session
        .accept_completion_review_input_with_client_id(
            &content,
            true,
            Some("review-request".into()),
            None,
        )
        .await;
    assert_eq!(review_revision, None);
    assert!(
        session
            .install_pending_completion_review_input(
                &context,
                TurnInput::UserInput {
                    content: content.clone(),
                    client_id: Some("review-request".into()),
                    review_revision,
                },
                true,
            )
            .await
    );
    assert!(
        session
            .start_completion_review_main_task(&context.sub_id, &context)
            .await
    );
    let answer = assistant_message("review evaluation");
    session
        .record_conversation_items(&context, std::slice::from_ref(&answer))
        .await;
    session
        .capture_completed_regular_turn_for_review(&context, Some("review evaluation"))
        .await;
    assert_eq!(session.completed_turn_for_review().await, None);
    assert_eq!(session.begin_completion_review(false).await, None);
    assert_eq!(session.state.lock().await.completion_review, initial_state);

    assert!(super::super::handlers::shutdown(&session, "shutdown".into()).await);
    // Shutdown, not a test-side flush, must preserve the ordinary transcript.
    let (items, _, errors) = RolloutRecorder::load_rollout_items(&path).await.unwrap();
    assert_eq!(errors, 0);
    assert!(
        !items
            .iter()
            .any(|item| matches!(item, RolloutItem::PostTurnCompletionReviewState(_)))
    );
    let messages = items
        .iter()
        .filter_map(|item| match item {
            RolloutItem::ResponseItem(message) => Some(message.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        messages,
        vec![ResponseInputItem::from(content).into(), answer]
    );
    let replay = session
        .reconstruct_history_from_rollout(&context, &items)
        .await;
    assert_eq!(replay.completion_review, None);
}

#[tokio::test]
async fn completion_review_capture_uses_task_result_not_assistant_role() {
    let (session, context) = completed_session().await;
    session
        .record_conversation_items(&context, &[assistant_message("commentary, not the final")])
        .await;
    assert!(
        session
            .start_completion_review_main_task(&context.sub_id, &context)
            .await
    );
    session
        .capture_completed_regular_turn_for_review(&context, Some(" exact final\n"))
        .await;
    let episode = session.completed_turn_for_review().await.unwrap();
    assert_eq!(
        (
            episode.initial_answer_reference,
            episode.final_agent_message
        ),
        (
            Some(" initial complete answer\n".into()),
            " exact final\n".into()
        )
    );
}

#[tokio::test]
async fn completion_review_captures_consumed_steering_without_progress_round() {
    let (session, context, _) = make_session_and_context_with_rx().await;
    let first = session
        .accept_completion_review_input(&steer_text_input("original"), true, None)
        .await
        .unwrap();
    assert!(
        session
            .install_completion_review_input(&context, first, true)
            .await
    );
    session
        .record_conversation_items(&context, &[assistant_message("progress")])
        .await;
    let second = session
        .accept_completion_review_input(&steer_text_input("steering"), false, None)
        .await
        .unwrap();
    assert!(
        session
            .install_completion_review_input(&context, second, true)
            .await
    );
    assert!(
        session
            .start_completion_review_main_task(&context.sub_id, &context)
            .await
    );
    session
        .capture_completed_regular_turn_for_review(&context, Some("final"))
        .await;
    assert_eq!(
        session
            .completed_turn_for_review()
            .await
            .unwrap()
            .request_frame,
        ReviewRequestFrame {
            prior_user_rounds: vec![],
            current_user_messages: vec!["original".into(), "steering".into()],
            text_only: true,
            exact: true,
        }
    );
}

#[tokio::test]
async fn completion_review_late_input_remains_unreviewed_until_consumed() {
    let (session, context, _) = make_session_and_context_with_rx().await;
    let first = session
        .accept_completion_review_input(&steer_text_input("original request"), true, None)
        .await
        .unwrap();
    assert!(
        session
            .install_completion_review_input(&context, first, true)
            .await
    );
    let revision = session
        .accept_completion_review_input(&steer_text_input("late amendment"), false, None)
        .await
        .unwrap();
    let input = TurnInput::UserInput {
        content: steer_text_input("late amendment"),
        client_id: None,
        review_revision: Some(revision),
    };
    session
        .retain_completion_review_input(&context, input)
        .await;
    assert!(
        session
            .start_completion_review_main_task(&context.sub_id, &context)
            .await
    );
    session
        .capture_completed_regular_turn_for_review(&context, Some("old sampling result"))
        .await;
    assert!(session.completed_turn_for_review().await.is_none());
    for revision in session.completion_review_input_recovery_revisions().await {
        assert!(
            session
                .install_completion_review_input(&context, revision, true)
                .await
        );
    }
    assert!(
        session
            .start_completion_review_main_task(&context.sub_id, &context)
            .await
    );
    session
        .capture_completed_regular_turn_for_review(&context, Some("answer including amendment"))
        .await;
    let episode = session.completed_turn_for_review().await.unwrap();
    assert_eq!(
        episode.request_frame.current_user_messages,
        vec!["original request", "late amendment"]
    );
    assert_eq!(episode.final_agent_message, "answer including amendment");
}

#[tokio::test]
async fn only_unavoidable_previous_notes_count_in_the_checkpoint_floor() {
    let (session, mut context) = completed_session().await;
    let ctx = Arc::get_mut(&mut context).unwrap();
    ctx.provider = crate::ModelProviderInfo::create_openai_provider();
    ctx.model_info.context_window = Some(64_000);
    ctx.model_info.auto_compact_token_limit = Some(20_000);
    ctx.model_info.effective_context_window_percent = 100;
    Arc::make_mut(&mut ctx.config).compaction_mode = crate::config::types::CompactionMode::Auto;
    let ticket = session.begin_completion_review(false).await.unwrap();
    assert!(
        session
            .prepare_completion_review_handoff(
                &ticket,
                "Verify.".into(),
                ctx.model_info.slug.clone()
            )
            .await
    );
    session
        .claim_completion_review_handoff(&ticket.attempt_id, "main", false)
        .await
        .unwrap();
    session
        .record_conversation_items(
            ctx,
            &[compact::preserved_work_notes_message(
                &"Recorded note.\n".repeat(6_000),
            )],
        )
        .await;
    let cancel = CancellationToken::new();
    session
        .check_review_handoff_floor(ctx, ctx, &cancel)
        .await
        .unwrap();
    ctx.final_output_json_schema = Some(serde_json::json!({"type":"object"}));
    let error = session
        .check_review_handoff_floor(ctx, ctx, &cancel)
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("19999-token context target"),
        "{error}"
    );
}

#[test_case::test_case(0, None, None, true; "unmeasured exact advice")]
#[test_case::test_case(1, None, None, true; "unmeasured advice one over")]
#[test_case::test_case(0, None, None, false; "login alone cannot place unmeasured advice")]
#[test_case::test_case(0, Some(true), None, true; "schemas shrink to exact fit")]
#[test_case::test_case(1, Some(false), None, true; "schemas grow one over")]
#[test_case::test_case(1, Some(false), Some(false), true; "one token of measured credit admits advice on a fixed route")]
#[test_case::test_case(1, Some(false), Some(false), false; "positive receipt alone cannot place advice")]
#[test_case::test_case(1, Some(false), Some(true), true; "the same credit cannot commit advice across a login switch")]
#[tokio::test]
async fn review_admission_charges_resolved_tool_schemas(
    overflow: i64,
    shrink: Option<bool>,
    variable_route: Option<bool>,
    sampled_route_known: bool,
) {
    let (session, mut context) = completed_session().await;
    let ctx = Arc::get_mut(&mut context).unwrap();
    if let Some(variable) = variable_route {
        ctx.provider = crate::ModelProviderInfo::create_openai_provider();
        ctx.provider.base_url = (!variable).then(|| "http://localhost/fixed".to_string());
    }
    ctx.model_info.context_window = Some(40_000);
    ctx.model_info.effective_context_window_percent = 100;
    // The boundary under test is recoverable: the schemas fit once old work is gone.
    session
        .record_conversation_items(ctx, &[assistant_message(&"older work ".repeat(4_000))])
        .await;
    ctx.dynamic_tools.push(DynamicToolSpec {
        name: "schema_probe".into(),
        description: String::new(),
        input_schema: serde_json::json!({"type":"object","properties":{}}),
    });
    let ticket = session.begin_completion_review(false).await.unwrap();
    assert!(
        session
            .prepare_completion_review_handoff(
                &ticket,
                "Verify the answer.".into(),
                ctx.model_info.slug.clone()
            )
            .await
    );
    session
        .claim_completion_review_handoff(&ticket.attempt_id, "main", false)
        .await
        .unwrap();
    let cancel = CancellationToken::new();
    let items = session.prompt_history(ctx).await;
    let specs = super::turn::compaction_tool_specs(&session, ctx, &items, &cancel)
        .await
        .unwrap();
    assert!(specs.wire().iter().any(|t| t["name"] == "schema_probe"));
    let fixed = specs.tokens();
    let state = session.state.lock().await;
    let base = state.base_instructions_for_turn(ctx);
    let work = state
        .history
        .projected_tokens(&base, ctx.helper_projection());
    let advice = state
        .completion_review
        .handoff
        .as_ref()
        .unwrap()
        .advisory_item();
    drop(state);
    let extra = super::turn::unsampled_review_token_limit(ctx)
        - work
        - fixed
        - crate::context_manager::estimate_item_token_count(&advice)
        + overflow;
    assert!(extra > 1_000);
    if shrink == Some(true) {
        ctx.dynamic_tools.last_mut().unwrap().description = "a".repeat(4 * (extra as usize + 2000));
    }
    let tools = super::turn::compaction_tool_specs(&session, ctx, &items, &cancel)
        .await
        .unwrap();
    let total = work + tools.tokens() - i64::from(variable_route.is_some());
    let prompt = Prompt {
        input: items.clone(),
        base_instructions: base.clone(),
        tools,
        ..Default::default()
    };
    let sampled_route = ctx.current_endpoint().filter(|_| sampled_route_known);
    assert!(
        session
            .finish_ordinary_input(ctx, &prompt, sampled_route.as_deref())
            .await
    );
    if shrink.is_some() {
        session
            .update_token_usage_info_with_request_context(
                ctx,
                ResponseUsage {
                    tokens: Some(&TokenUsage {
                        input_tokens: total,
                        total_tokens: total,
                        ..Default::default()
                    }),
                    endpoint: ctx.current_endpoint().as_deref(),
                },
                0,
                true,
                items.len(),
                &prompt,
            )
            .await;
    }
    ctx.dynamic_tools.last_mut().unwrap().description = "a".repeat(4 * extra as usize);
    let (proposal, frames, mut input, tools) = propose_first_review(&session, &context).await;
    frames.insert(&mut input, None).unwrap();
    let prompt = Prompt {
        input: input.clone(),
        tools,
        base_instructions: base,
        ..Default::default()
    };
    let admit = session
        .ordinary_review_admission(&context, &prompt, frames.tokens(), Some(proposal.clone()))
        .await;
    let Err(CodexErr::ReviewPayloadExceeded { endpoint, .. }) = admit
        .admit(&prompt, &mut input, context.current_endpoint().as_deref())
        .await
    else {
        panic!("ordinary request must be refused at dispatch")
    };
    proposal
        .defer_refused(&context, endpoint, &cancel)
        .await
        .unwrap();
    assert_eq!(
        session.prompt_history(&context).await.contains(&advice),
        sampled_route_known && (overflow == 0 || variable_route == Some(false))
    );
    let state = session.state.lock().await;
    assert!(state.completion_review.prompt_frames.is_empty());
    assert!(state.completion_review.defer_new_prompt_frame);
}

#[test_case::test_case(true, false; "growing base cannot use pinned fast path")]
#[test_case::test_case(false, false; "shrinking base avoids compaction")]
#[test_case::test_case(true, true; "adding a response format charges its wire cost")]
#[test_case::test_case(false, true; "dropping a response format credits its wire cost")]
#[tokio::test]
async fn pinned_review_accounts_for_request_surface_changes(grow: bool, change_format: bool) {
    let (session, mut context) = completed_session().await;
    let ctx = Arc::get_mut(&mut context).unwrap();
    ctx.model_info.context_window = Some(40_000);
    ctx.model_info.effective_context_window_percent = 100;
    let base = |long| {
        if long && !change_format {
            "Sampled style. ".repeat(300)
        } else {
            "Default.".into()
        }
    };
    let schema = |long| {
        (change_format && long)
            .then(|| serde_json::json!({"type":"object","description":"Evidence. ".repeat(500)}))
    };
    Arc::make_mut(&mut ctx.config).base_instructions = Some(base(!grow));
    let ticket = session.begin_completion_review(false).await.unwrap();
    assert!(
        session
            .prepare_completion_review_handoff(
                &ticket,
                "Verify the answer.".into(),
                ctx.model_info.slug.clone()
            )
            .await
    );
    session
        .claim_completion_review_handoff(&ticket.attempt_id, "main", false)
        .await
        .unwrap();
    let (frames, input, tools) = commit_first_review(&session, &context).await;
    let tokens = 40_000 - 8_192 + if grow { -1 } else { 1 };
    let prompt = Prompt {
        base_instructions: session.get_base_instructions_for_turn(&context).await,
        tools: tools.clone(),
        output_schema: schema(!grow),
        ..Default::default()
    };
    session
        .update_token_usage_info_with_request_context(
            &context,
            ResponseUsage {
                tokens: Some(&TokenUsage {
                    input_tokens: tokens,
                    total_tokens: tokens,
                    ..Default::default()
                }),
                endpoint: context.current_endpoint().as_deref(),
            },
            frames.tokens(),
            true,
            input.len(),
            &prompt,
        )
        .await;
    Arc::make_mut(&mut Arc::get_mut(&mut context).unwrap().config).base_instructions =
        Some(base(grow));
    let ReviewSampling::Ready(ready) = session
        .review_sampling(ReviewSamplingInput {
            items: &input,
            advice: None,
        })
        .await
        .unwrap()
    else {
        panic!("already sent")
    };
    assert_eq!(ready, frames);
    let mut items = input;
    frames.insert(&mut items, None).unwrap();
    let prompt = Prompt {
        input: items.clone(),
        tools,
        base_instructions: session.get_base_instructions_for_turn(&context).await,
        output_schema: schema(grow),
        ..Default::default()
    };
    let admit = session
        .ordinary_review_admission(&context, &prompt, frames.tokens(), None)
        .await;
    assert_eq!(
        admit
            .admit(&prompt, &mut items, context.current_endpoint().as_deref())
            .await
            .is_ok(),
        !grow
    );
    if change_format {
        // Without a recorded format cost, keep the work correction but grant
        // no credit for a schema the old writer might not have sent.
        session
            .state
            .lock()
            .await
            .history
            .measured_surface
            .as_mut()
            .unwrap()
            .response_format_tokens = None;
        let legacy = session
            .ordinary_review_admission(&context, &prompt, frames.tokens(), None)
            .await;
        assert!(legacy.measured.is_some());
        assert!(
            legacy
                .admit(&prompt, &mut items, context.current_endpoint().as_deref())
                .await
                .is_err()
        );
    }
}

#[tokio::test]
async fn completion_review_handoff_is_claimed_once_and_survives_compaction() {
    let (session, context) = completed_session().await;
    let ticket = session.begin_completion_review(false).await.unwrap();
    assert!(
        session
            .prepare_completion_review_handoff(
                &ticket,
                "advisory plus replacement contract".into(),
                context.model_info.slug.clone()
            )
            .await
    );
    let prepared = session.state.lock().await.completion_review.clone();
    assert_eq!(
        prepared.handoff.as_ref().unwrap().disposition,
        ReviewHandoffDisposition::Prepared
    );
    assert!(
        session
            .claim_completion_review_handoff(&ticket.attempt_id, "main-followup", false)
            .await
            .is_some()
    );
    assert!(
        session
            .claim_completion_review_handoff(&ticket.attempt_id, "duplicate", true)
            .await
            .is_none()
    );
    let snapshot = session.state.lock().await.completion_review.clone();
    session
        .replace_compacted_history(
            &context,
            vec![assistant_message("compacted")],
            None,
            "compacted".into(),
        )
        .await;
    assert_eq!(
        session.state.lock().await.completion_review,
        crate::completion_review::CompletionReviewState {
            awaiting_compact_followup: true,
            ..snapshot
        }
    );
    let prompt = session
        .completion_review_main_instructions(true)
        .await
        .unwrap()
        .unwrap();
    assert!(prompt.contains("original request"));
    assert!(prompt.contains(" initial complete answer\n"));
    assert!(prompt.contains("advisory plus replacement contract"));
    commit_first_review(&session, &context).await;
    assert_eq!(
        session
            .admit_completion_review_advisory(&context)
            .await
            .expect("admit advice"),
        ReviewAdvice::Recorded
    );
    assert_eq!(
        session
            .clone_history()
            .await
            .raw_items()
            .iter()
            .filter(|item| {
                matches!(item, ResponseItem::Message { role, .. } if role == "developer")
            })
            .count(),
        1
    );
}

#[tokio::test]
async fn stopped_review_advice_survives_replay_but_cannot_take_a_paused_checkpoint() {
    let (session, context, events) = make_session_and_context_with_rx().await;
    let (_home, path) = attach_recorder(&session, &context).await;
    complete_new_request(&session, &context, "original request", "completed answer").await;
    session
        .record_conversation_items(&context, &[assistant_message("completed answer")])
        .await;
    let ticket = session.begin_completion_review(true).await.unwrap();
    assert!(
        session
            .prepare_completion_review_handoff(
                &ticket,
                "saved advice".into(),
                context.model_info.slug.clone(),
            )
            .await
    );
    super::super::handlers::pause(&session).await;
    let (items, _, errors) = RolloutRecorder::load_rollout_items(&path).await.unwrap();
    assert_eq!(errors, 0);
    let replay = session
        .reconstruct_history_from_rollout(&context, &items)
        .await;
    assert!(replay.pending_continuation.is_none());
    let resumed = replay.completion_review.unwrap();
    assert_eq!(
        resumed.unstarted_handoff().unwrap().disposition,
        ReviewHandoffDisposition::Deferred
    );
    assert_eq!(resumed.unstarted_handoff().unwrap().ticket, ticket);
    assert!(
        session
            .claim_completion_review_handoff(&ticket.attempt_id, "queued-automatic", false)
            .await
            .is_none()
    );
    let checkpoint = PendingContinuation {
        source: TurnContinuationSource::Paused,
        continued_from_turn_id: Some("existing-work".into()),
        model: Some(context.model_info.slug.clone()),
        pause_reason: Some(TurnPauseReason::UserRequested),
        target: PendingContinuationTarget::Regular,
    };
    session
        .set_pending_continuation(Some(checkpoint.clone()))
        .await;
    assert!(
        session
            .claim_completion_review_handoff(&ticket.attempt_id, "confirmed", true)
            .await
            .is_none()
    );
    assert_eq!(
        session.take_pending_continuation().await.unwrap(),
        Some(checkpoint)
    );
    super::super::handlers::continue_last(&session, "offer".into()).await;
    let offer = std::iter::from_fn(|| events.try_recv().ok())
        .find_map(|event| match event.msg {
            EventMsg::CompletionReviewAdviceAvailable(offer) => Some(offer),
            _ => None,
        })
        .unwrap();
    assert_eq!(offer.attempt_id, ticket.attempt_id);
}

#[tokio::test]
async fn completion_review_persistence_failure_prevents_reviewer_and_main_startup() {
    for followup_main in [false, true] {
        let (session, context, events) = make_session_and_context_with_rx().await;
        let (_home, path) = attach_recorder(&session, &context).await;
        complete_new_request(&session, &context, "original request", "complete answer").await;
        if followup_main {
            let ticket = session.begin_completion_review(false).await.unwrap();
            assert!(
                session
                    .prepare_completion_review_handoff(
                        &ticket,
                        "advisory and replacement contract".into(),
                        context.model_info.slug.clone(),
                    )
                    .await
            );
        }
        let before = tokio::fs::read(&path).await.unwrap();
        *session.services.rollout.lock().await =
            Some(RolloutRecorder::read_only_for_test(&path).await.unwrap());
        while events.try_recv().is_ok() {}
        if followup_main {
            let ticket = session.current_completion_review_ticket().await.unwrap();
            assert!(
                session
                    .claim_completion_review_handoff(&ticket.attempt_id, "failed-main", false)
                    .await
                    .is_none()
            );
        } else {
            super::super::handlers::review_completed_turn(&session, "failed-review".into()).await;
        }
        let started = session.active_turn.lock().await.is_some();
        session.abort_all_tasks(TurnAbortReason::Replaced).await;
        assert!(!started);
        assert_eq!(tokio::fs::read(&path).await.unwrap(), before);
        assert!(session.completed_turn_for_review().await.is_none());
        let messages: Vec<_> = std::iter::from_fn(|| events.try_recv().ok())
            .map(|event| event.msg)
            .collect();
        assert!(messages.iter().any(|event| {
            matches!(event, EventMsg::Warning(warning)
                if warning.message.contains("could not be persisted"))
        }));
        if !followup_main {
            assert!(messages.iter().any(|event| {
                matches!(event, EventMsg::Error(error)
                    if error.message == CompletionReviewReservationError::Persistence.to_string()
                        && error.codex_error_info == Some(CodexErrorInfo::Other))
            }));
        }
        assert!(!messages.iter().any(|event| {
            matches!(
                event,
                EventMsg::TurnStarted(_) | EventMsg::EnteredReviewMode(_)
            )
        }));
    }
}

#[tokio::test]
async fn completion_review_automatic_capture_failure_cannot_authorize_review() {
    let (session, mut context, events) = make_session_and_context_with_rx().await;
    Arc::get_mut(&mut context)
        .unwrap()
        .features
        .enable(Feature::AutoPostTurnCompletionReview);
    let (_home, path) = attach_recorder(&session, &context).await;
    let revision = session
        .accept_completion_review_input(&steer_text_input("complete request"), true, Some(&context))
        .await
        .unwrap();
    assert!(
        session
            .install_completion_review_input(&context, revision, true)
            .await
    );
    let before = tokio::fs::read(&path).await.unwrap();
    *session.services.rollout.lock().await =
        Some(RolloutRecorder::read_only_for_test(&path).await.unwrap());
    while events.try_recv().is_ok() {}
    session
        .capture_completed_regular_turn_for_review(&context, Some("complete answer"))
        .await;
    let state = session.state.lock().await.completion_review.clone();
    assert!(state.attempt.as_ref().unwrap().automatic);
    assert!(!state.request_frame.exact);
    assert!(session.current_completion_review_ticket().await.is_none());
    assert!(session.completed_turn_for_review().await.is_none());
    assert_eq!(tokio::fs::read(&path).await.unwrap(), before);
    assert!(std::iter::from_fn(|| events.try_recv().ok()).any(|event| {
        matches!(event.msg, EventMsg::Warning(warning)
            if warning.message.contains("could not be persisted"))
    }));
}

#[tokio::test]
async fn completion_review_started_resumes_stop_before_sampling_on_failed_owner_persistence() {
    for compacted in [false, true] {
        let (session, mut context, events) = make_session_and_context_with_rx().await;
        let (_home, path) = attach_recorder(&session, &context).await;
        complete_new_request(&session, &context, "original request", "complete answer").await;
        Arc::get_mut(&mut context).unwrap().sub_id = "started-main".into();
        let ticket = session.begin_completion_review(false).await.unwrap();
        assert!(
            session
                .prepare_completion_review_handoff(
                    &ticket,
                    "advisory and replacement contract".into(),
                    context.model_info.slug.clone(),
                )
                .await
        );
        let checkpoint = session
            .claim_completion_review_handoff(&ticket.attempt_id, &context.sub_id, false)
            .await
            .unwrap();
        session.set_pending_continuation(Some(checkpoint)).await;
        let ready = Arc::new(Notify::new());
        session
            .spawn_task(
                Arc::clone(&context),
                vec![],
                CompletingReviewTask {
                    ready: Arc::clone(&ready),
                    finish: Arc::new(Notify::new()),
                },
            )
            .await;
        ready.notified().await;
        super::super::handlers::pause(&session).await;
        if compacted {
            let replacement = vec![ResponseItem::Compaction {
                encrypted_content: "compacted paused main work".into(),
            }];
            session
                .replace_compacted_history(&context, replacement, None, String::new())
                .await;
            session.persist_completion_review().await;
        }
        let before = tokio::fs::read(&path).await.unwrap();
        let snapshot = session.state.lock().await.completion_review.clone();
        let history = session.clone_history().await.raw_items().to_vec();
        *session.services.rollout.lock().await =
            Some(RolloutRecorder::read_only_for_test(&path).await.unwrap());
        while events.try_recv().is_ok() {}
        super::super::handlers::continue_last(&session, "failed-resume".into()).await;
        let event = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let event = events.recv().await.unwrap().msg;
                if matches!(event, EventMsg::TurnStarted(_) | EventMsg::TurnComplete(_)) {
                    return event;
                }
            }
        })
        .await
        .unwrap();
        session.abort_all_tasks(TurnAbortReason::Replaced).await;
        assert!(
            matches!(event, EventMsg::TurnComplete(event) if event.last_agent_message.is_none()),
            "failed Started resume must stop before sampling; compacted={compacted}"
        );
        assert_eq!(tokio::fs::read(&path).await.unwrap(), before);
        assert_eq!(
            session.clone_history().await.raw_items(),
            history.as_slice()
        );
        {
            let state = session.state.lock().await;
            assert_eq!(state.completion_review.episode, snapshot.episode);
            assert_eq!(state.completion_review.handoff, snapshot.handoff);
            assert!(state.completion_review.replacement_answer_required);
            let pending = state.pending_continuation.as_ref().unwrap();
            assert_eq!(pending.target, PendingContinuationTarget::Regular);
            assert_eq!(
                pending.model.as_deref(),
                Some(context.model_info.slug.as_str())
            );
        }
        assert!(
            session
                .completion_review_main_instructions(true)
                .await
                .is_err()
        );
        let pending = session.state.lock().await.pending_continuation.clone();
        *session.services.rollout.lock().await = Some(
            RolloutRecorder::new(
                context.config.as_ref(),
                crate::rollout::recorder::RolloutRecorderParams::resume(path.clone()),
                None,
                None,
            )
            .await
            .unwrap(),
        );
        super::super::handlers::continue_last(&session, "missing-snapshot".into()).await;
        assert!(session.active_turn.lock().await.is_none());
        assert_eq!(session.state.lock().await.pending_continuation, pending);
        assert_eq!(tokio::fs::read(&path).await.unwrap(), before);
        let (items, _, errors) = RolloutRecorder::load_rollout_items(&path).await.unwrap();
        assert_eq!(errors, 0);
        let recovered = session
            .reconstruct_history_from_rollout(&context, &items)
            .await;
        assert_eq!(recovered.completion_review, Some(snapshot));
        assert_eq!(
            recovered.pending_continuation.unwrap().target,
            PendingContinuationTarget::Regular
        );
    }
}

#[tokio::test]
async fn completion_review_directory_rejection_preserves_the_pending_checkpoint() {
    for review_derived in [false, true] {
        let (session, context) = completed_session().await;
        let checkpoint = if review_derived {
            let ticket = session.begin_completion_review(false).await.unwrap();
            assert!(
                session
                    .prepare_completion_review_handoff(
                        &ticket,
                        "advisory and replacement contract".into(),
                        context.model_info.slug.clone(),
                    )
                    .await
            );
            session
                .claim_completion_review_handoff(&ticket.attempt_id, "main-followup", false)
                .await
                .unwrap()
        } else {
            PendingContinuation {
                source: TurnContinuationSource::Interrupted,
                continued_from_turn_id: Some(context.sub_id.clone()),
                model: Some(context.model_info.slug.clone()),
                pause_reason: None,
                target: PendingContinuationTarget::Regular,
            }
        };
        session
            .set_pending_continuation(Some(checkpoint.clone()))
            .await;
        let before = session.state.lock().await.completion_review.clone();
        session
            .update_settings(SessionSettingsUpdate {
                cwd: Some(context.cwd.join("other-project")),
                ..Default::default()
            })
            .await
            .unwrap();
        if review_derived {
            assert!(matches!(
                session.take_pending_continuation().await,
                Err(CodexErr::UnsupportedOperation(_))
            ));
            {
                let state = session.state.lock().await;
                assert_eq!(state.completion_review, before);
                assert_eq!(state.pending_continuation.as_ref(), Some(&checkpoint));
            }
            session
                .update_settings(SessionSettingsUpdate {
                    cwd: Some(context.cwd.clone()),
                    ..Default::default()
                })
                .await
                .unwrap();
        }
        assert_eq!(
            session.take_pending_continuation().await.unwrap(),
            Some(checkpoint)
        );
    }
}

#[tokio::test]
async fn completion_review_model_override_retargets_only_pending_main_work_durably() {
    for (target, previously_selected, steered) in [
        (PendingContinuationTarget::Regular, false, false),
        (PendingContinuationTarget::Regular, true, false),
        (PendingContinuationTarget::Regular, false, true),
        (PendingContinuationTarget::Regular, true, true),
        (
            PendingContinuationTarget::PostTurnCompletionReview,
            false,
            false,
        ),
    ] {
        let (session, context) = completed_session().await;
        let model = if context.model_info.slug == "gpt-5.4" {
            "gpt-5.1"
        } else {
            "gpt-5.4"
        };
        let mode = context
            .collaboration_mode
            .with_updates(Some(model.into()), None, None);
        if previously_selected {
            session
                .update_settings(SessionSettingsUpdate {
                    collaboration_mode: Some(mode.clone()),
                    ..Default::default()
                })
                .await
                .unwrap();
        }
        let ticket = session.begin_completion_review(false).await.unwrap();
        let checkpoint = if target == PendingContinuationTarget::Regular {
            assert!(
                session
                    .prepare_completion_review_handoff(
                        &ticket,
                        "advisory and replacement contract".into(),
                        context.model_info.slug.clone(),
                    )
                    .await
            );
            session
                .claim_completion_review_handoff(&ticket.attempt_id, "main-followup", false)
                .await
                .unwrap()
        } else {
            PendingContinuation {
                source: TurnContinuationSource::Paused,
                continued_from_turn_id: Some(context.sub_id.clone()),
                model: Some(context.model_info.slug.clone()),
                pause_reason: None,
                target,
            }
        };
        session
            .set_pending_continuation(Some(checkpoint.clone()))
            .await;
        if steered {
            assert!(
                session
                    .start_completion_review_main_task("main-followup", &context)
                    .await
            );
            let revision = session
                .accept_completion_review_input(
                    &steer_text_input("preserve the public API"),
                    false,
                    None,
                )
                .await
                .unwrap();
            assert!(
                session
                    .install_completion_review_input(&context, revision, true)
                    .await
            );
        }
        let (_home, path) = attach_recorder(&session, &context).await;
        let mut expected = session.state.lock().await.completion_review.clone();
        let mut expected_checkpoint = checkpoint;
        if target == PendingContinuationTarget::Regular {
            expected.recovery_model = Some(model.into());
            if let Some(settings) = &mut expected.recovery_settings {
                settings.mode = mode.clone();
            }
            if let Some(handoff) = expected.handoff.as_mut() {
                handoff.model = model.into();
            }
            expected_checkpoint.model = Some(model.into());
        }
        super::super::handlers::override_turn_context(
            &session,
            "selected-recovery-model".into(),
            SessionSettingsUpdate {
                collaboration_mode: Some(mode),
                ..Default::default()
            },
            true,
        )
        .await;
        {
            let state = session.state.lock().await;
            assert_eq!(state.completion_review, expected);
            assert_eq!(
                state.pending_continuation.as_ref(),
                Some(&expected_checkpoint)
            );
        }
        if target == PendingContinuationTarget::Regular {
            let (items, _, errors) = RolloutRecorder::load_rollout_items(&path).await.unwrap();
            assert_eq!(errors, 0);
            let replay = session
                .reconstruct_history_from_rollout(&context, &items)
                .await;
            assert_eq!(replay.completion_review, Some(expected));
            assert_eq!(
                replay.pending_continuation.unwrap().model.as_deref(),
                Some(model)
            );
        }
    }
}

#[tokio::test]
async fn completion_review_replay_distinguishes_prepared_started_and_reviewer() {
    let (session, context) = completed_session().await;
    let ticket = session.begin_completion_review(true).await.unwrap();
    let reviewer = session.state.lock().await.completion_review.clone();
    let replay = session
        .reconstruct_history_from_rollout(&context, &[metadata(&reviewer)])
        .await;
    assert_eq!(replay.completion_review, Some(reviewer.clone()));
    assert_eq!(
        replay.pending_continuation.unwrap().target,
        PendingContinuationTarget::PostTurnCompletionReview
    );
    assert!(
        session
            .prepare_completion_review_handoff(
                &ticket,
                "advice".into(),
                context.model_info.slug.clone()
            )
            .await
    );
    let prepared = session.state.lock().await.completion_review.clone();
    let replay = session
        .reconstruct_history_from_rollout(&context, &[metadata(&prepared)])
        .await;
    assert_eq!(replay.completion_review, Some(prepared));
    assert!(replay.pending_continuation.is_none());
    session
        .claim_completion_review_handoff(&ticket.attempt_id, "started-main", false)
        .await
        .unwrap();
    let started = session.state.lock().await.completion_review.clone();
    for legacy in [false, true] {
        let mut expected = started.clone();
        let mut value = serde_json::to_value(&started).unwrap();
        if legacy {
            value.as_object_mut().unwrap().remove("recovery_model");
            expected.recovery_model = None;
        }
        let replay = session
            .reconstruct_history_from_rollout(
                &context,
                &[RolloutItem::PostTurnCompletionReviewState(value)],
            )
            .await;
        assert_eq!(replay.completion_review, Some(expected));
        assert_eq!(
            replay.pending_continuation,
            Some(PendingContinuation {
                source: TurnContinuationSource::Interrupted,
                continued_from_turn_id: Some("started-main".into()),
                model: Some(context.model_info.slug.clone()),
                pause_reason: None,
                target: PendingContinuationTarget::Regular,
            })
        );
    }
}

#[tokio::test]
async fn completion_review_rollback_and_legacy_history_cannot_reuse_future_snapshot() {
    let (session, context) = completed_session().await;
    let snapshot = session.state.lock().await.completion_review.clone();
    let rollout = vec![
        RolloutItem::ResponseItem(user_message("original request")),
        RolloutItem::ResponseItem(assistant_message(" initial complete answer\n")),
        metadata(&snapshot),
        RolloutItem::EventMsg(EventMsg::ThreadRolledBack(ThreadRolledBackEvent {
            num_turns: 1,
        })),
    ];
    let replay = session
        .reconstruct_history_from_rollout(&context, &rollout)
        .await;
    assert!(replay.completion_review.is_none());
    session.replace_with_reconstructed_rollout(replay).await;
    assert!(session.completed_turn_for_review().await.is_none());
    session
        .replace_history(vec![
            user_message("legacy"),
            assistant_message("role is not completion provenance"),
        ])
        .await;
    assert!(session.completed_turn_for_review().await.is_none());
}

#[tokio::test]
async fn completion_review_compacted_legacy_resume_and_fork_keep_unknown_request_provenance() {
    for version in [None, Some(1), Some(99)] {
        for fork in [false, true] {
            let (session, context) = completed_session().await;
            let mut rollout = vec![
                RolloutItem::ResponseItem(user_message("original request")),
                RolloutItem::ResponseItem(assistant_message("option one: scan; option two: index")),
                RolloutItem::Compacted(CompactedItem {
                    checkpoint: None,
                    message: String::new(),
                    replacement_history: Some(vec![ResponseItem::Compaction {
                        encrypted_content: "opaque prior user requirements".into(),
                    }]),
                }),
            ];
            if let Some(version) = version {
                let mut value =
                    serde_json::to_value(&session.state.lock().await.completion_review).unwrap();
                value["version"] = serde_json::json!(version);
                rollout.push(RolloutItem::PostTurnCompletionReviewState(value));
            }
            let initial = if fork {
                InitialHistory::Forked(rollout)
            } else {
                InitialHistory::Resumed(ResumedHistory {
                    conversation_id: session.conversation_id,
                    history: rollout,
                    rollout_path: PathBuf::from("unused-rollout"),
                })
            };
            session.record_initial_history(initial).await;
            assert!(
                !session
                    .clone_history()
                    .await
                    .raw_items()
                    .iter()
                    .any(is_user_turn_boundary_response_item)
            );
            complete_new_request(&session, &context, "use option two", "used option two").await;
            assert_eq!(
                session
                    .state
                    .lock()
                    .await
                    .completion_review
                    .episode
                    .as_ref()
                    .unwrap()
                    .request_frame,
                ReviewRequestFrame {
                    current_user_messages: vec!["use option two".into()],
                    prior_user_rounds: vec![],
                    text_only: true,
                    exact: false,
                }
            );
            assert_eq!(
                session.reserve_manual_completion_review().await,
                Err(CompletionReviewReservationError::InexactRequest)
            );
            assert!(session.begin_completion_review(true).await.is_none());
        }
    }
}

#[tokio::test]
async fn completion_review_trusted_compacted_resume_and_fork_preserve_referential_scope() {
    for fork in [false, true] {
        let (session, context) = completed_session().await;
        let original = session.completed_turn_for_review().await.unwrap();
        let state = session.state.lock().await.completion_review.clone();
        let rollout = vec![
            RolloutItem::Compacted(CompactedItem {
                checkpoint: None,
                message: String::new(),
                replacement_history: Some(vec![ResponseItem::Compaction {
                    encrypted_content: "opaque prior user requirements".into(),
                }]),
            }),
            metadata(&state),
        ];
        let initial = if fork {
            InitialHistory::Forked(rollout)
        } else {
            InitialHistory::Resumed(ResumedHistory {
                conversation_id: session.conversation_id,
                history: rollout,
                rollout_path: PathBuf::from("unused-rollout"),
            })
        };
        session.record_initial_history(initial).await;
        complete_new_request(&session, &context, "use option two", "used option two").await;
        assert_eq!(
            session
                .completed_turn_for_review()
                .await
                .unwrap()
                .request_frame,
            ReviewRequestFrame {
                prior_user_rounds: vec![crate::completion_review::CompletedTurnReviewRound {
                    user_messages: original.request_frame.current_user_messages,
                    final_agent_message: original.final_agent_message,
                }],
                current_user_messages: vec!["use option two".into()],
                text_only: true,
                exact: true,
            }
        );
    }
}

#[tokio::test]
async fn completion_review_replaced_history_cannot_regain_provenance_without_user_boundaries() {
    for replacement in [
        vec![],
        vec![ResponseItem::Compaction {
            encrypted_content: "opaque prior context".into(),
        }],
        vec![assistant_message(
            "plaintext summary without visible user messages",
        )],
    ] {
        let (session, context) = completed_session().await;
        session.replace_history(replacement).await;
        complete_new_request(&session, &context, "use option two", "used option two").await;
        assert!(session.completed_turn_for_review().await.is_none());
        assert_eq!(
            session.reserve_manual_completion_review().await,
            Err(CompletionReviewReservationError::InexactRequest)
        );
    }
}

#[tokio::test]
async fn completion_review_stale_tickets_cannot_prepare_or_claim_advice() {
    let (session, context) = completed_session().await;
    let old = session.begin_completion_review(false).await.unwrap();
    let revision = session
        .accept_completion_review_input(&steer_text_input("new user request"), true, None)
        .await
        .unwrap();
    assert!(
        session
            .install_completion_review_input(&context, revision, true)
            .await
    );
    assert!(
        session
            .start_completion_review_main_task(&context.sub_id, &context)
            .await
    );
    session
        .capture_completed_regular_turn_for_review(&context, Some("new candidate"))
        .await;
    let current = session.begin_completion_review(false).await.unwrap();
    assert!(
        !session
            .prepare_completion_review_handoff(&old, "stale advice".into(), "model".into())
            .await
    );
    assert!(
        session
            .claim_completion_review_handoff(&old.attempt_id, "stale-main", true)
            .await
            .is_none()
    );
    session.finish_completion_review(&old).await;
    assert!(session.completion_review_ticket_is_current(&current).await);
}

#[tokio::test]
async fn completion_review_old_task_cannot_capture_or_schedule_a_newer_candidate() {
    let (session, mut context) = completed_session().await;
    Arc::get_mut(&mut context)
        .unwrap()
        .features
        .enable(Feature::AutoPostTurnCompletionReview);
    let original = session.completed_turn_for_review().await;
    assert!(
        session
            .start_completion_review_main_task("newer-main", &context)
            .await
    );
    session
        .capture_completed_regular_turn_for_review(&context, Some("stale old result"))
        .await;
    assert_eq!(session.completed_turn_for_review().await, original);
    assert!(session.current_completion_review_ticket().await.is_none());
}

#[tokio::test]
async fn completion_review_fork_excludes_metadata_for_the_next_unconsumed_user_boundary() {
    let (session, context) = completed_session().await;
    let original = session.state.lock().await.completion_review.clone();
    session
        .accept_completion_review_input(&steer_text_input("excluded future request"), true, None)
        .await
        .unwrap();
    let next = session.state.lock().await.completion_review.clone();
    let prefix = vec![
        RolloutItem::ResponseItem(user_message("original request")),
        RolloutItem::ResponseItem(assistant_message(" initial complete answer\n")),
        metadata(&original),
        metadata(&next),
    ];
    assert_eq!(
        super::super::completion_review::completion_review_for_fork(&prefix),
        Some(original.clone())
    );
    let mut unavailable = next.clone();
    unavailable.request_frame.exact = false;
    for marker in [
        RolloutItem::PostTurnCompletionReviewState(serde_json::json!({"version": 99})),
        metadata(&CompletionReviewState::default()),
        metadata(&unavailable),
    ] {
        let mut invalidated = prefix.clone();
        invalidated.push(marker);
        assert!(
            super::super::completion_review::completion_review_for_fork(&invalidated).is_none()
        );
    }
    session
        .record_initial_history(InitialHistory::Forked(prefix))
        .await;
    let fork = session.completed_turn_for_review().await.unwrap();
    assert_eq!(
        fork.final_agent_message,
        original.episode.unwrap().final_agent_message
    );
    assert_eq!(
        fork.request_frame.current_user_messages,
        vec!["original request"]
    );
    assert_ne!(fork.history_generation, next.history_generation);
    assert!(session.take_pending_continuation().await.unwrap().is_none());
    let _ = context;
}

#[tokio::test]
async fn completion_review_real_pause_persists_the_correct_review_or_main_target() {
    for followup_main in [false, true] {
        let (session, context) = completed_session().await;
        let (_home, path) = attach_recorder(&session, &context).await;
        let ticket = session.begin_completion_review(true).await.unwrap();
        if followup_main {
            assert!(
                session
                    .prepare_completion_review_handoff(
                        &ticket,
                        "replacement answer required".into(),
                        context.model_info.slug.clone()
                    )
                    .await
            );
            session
                .claim_completion_review_handoff(&ticket.attempt_id, &context.sub_id, false)
                .await
                .unwrap();
        }
        let kind = if followup_main {
            TaskKind::Regular
        } else {
            TaskKind::PostTurnCompletionReview
        };
        session
            .spawn_task(
                Arc::clone(&context),
                vec![],
                NeverEndingTask {
                    kind,
                    listen_to_cancellation_token: true,
                },
            )
            .await;
        super::super::handlers::pause(&session).await;
        // No test-side flush: the real task-stop boundary must make this durable.
        let (items, _, errors) = RolloutRecorder::load_rollout_items(&path).await.unwrap();
        assert_eq!(errors, 0);
        let replay = session
            .reconstruct_history_from_rollout(&context, &items)
            .await;
        assert_eq!(
            replay.completion_review,
            Some(session.state.lock().await.completion_review.clone())
        );
        assert_eq!(
            replay.pending_continuation.unwrap().target,
            if followup_main {
                PendingContinuationTarget::Regular
            } else {
                PendingContinuationTarget::PostTurnCompletionReview
            }
        );
    }
}

struct ReviewInstallationTask {
    ticket: crate::completion_review::ReviewTicket,
    main: bool,
    ready: Arc<tokio::sync::Notify>,
    enter: Arc<tokio::sync::Notify>,
}

impl SessionTask for ReviewInstallationTask {
    fn kind(&self) -> TaskKind {
        if self.main {
            TaskKind::Regular
        } else {
            TaskKind::PostTurnCompletionReview
        }
    }

    async fn run(
        self: Arc<Self>,
        session: Arc<SessionTaskContext>,
        context: Arc<TurnContext>,
        _input: Vec<UserInput>,
        _cancellation: CancellationToken,
    ) -> Option<String> {
        let session = session.clone_session();
        let proposal = if self.main {
            Some(propose_first_review(&session, &context).await.0)
        } else {
            None
        };
        self.ready.notify_one();
        self.enter.notified().await;
        if let Some(proposal) = proposal {
            proposal.commit(0, i64::MAX, None).await.unwrap();
        } else {
            session
                .prepare_completion_review_handoff(
                    &self.ticket,
                    "exact advisory".into(),
                    context.model_info.slug.clone(),
                )
                .await;
        }
        None
    }
}

#[tokio::test]
async fn completion_review_pause_joins_inflight_installation_before_durable_stop() {
    for main in [false, true] {
        let (session, context) = completed_session().await;
        let (_home, path) = attach_recorder(&session, &context).await;
        let ticket = session.begin_completion_review(true).await.unwrap();
        if main {
            assert!(
                session
                    .prepare_completion_review_handoff(
                        &ticket,
                        "exact advisory".into(),
                        context.model_info.slug.clone()
                    )
                    .await
            );
            session
                .claim_completion_review_handoff(&ticket.attempt_id, &context.sub_id, false)
                .await
                .unwrap();
        }
        let ready = Arc::new(tokio::sync::Notify::new());
        let enter = Arc::new(tokio::sync::Notify::new());
        session
            .spawn_task(
                Arc::clone(&context),
                vec![],
                ReviewInstallationTask {
                    ticket,
                    main,
                    ready: Arc::clone(&ready),
                    enter: Arc::clone(&enter),
                },
            )
            .await;
        ready.notified().await;
        let (cancel, handle) = {
            let active = session.active_turn.lock().await;
            let task = active.as_ref().unwrap().tasks.get(&context.sub_id).unwrap();
            (task.cancellation_token.clone(), Arc::clone(&task.handle))
        };
        let state = session.state.lock().await;
        enter.notify_one();
        tokio::time::timeout(Duration::from_secs(2), async {
            while session.completion_review_installation.try_lock().is_ok() {
                sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .unwrap();
        let pause_session = Arc::clone(&session);
        let paused =
            tokio::spawn(async move { super::super::handlers::pause(&pause_session).await });
        cancel.cancelled().await;
        tokio::time::timeout(Duration::from_secs(2), async {
            while !handle.is_finished() {
                sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .unwrap();
        drop(state);
        tokio::time::timeout(Duration::from_secs(2), paused)
            .await
            .unwrap()
            .unwrap();
        // The stop handler must have joined and persisted the detached install.
        let (items, _, errors) = RolloutRecorder::load_rollout_items(&path).await.unwrap();
        assert_eq!(errors, 0);
        let replay = session
            .reconstruct_history_from_rollout(&context, &items)
            .await;
        let expected = session.state.lock().await.completion_review.clone();
        assert_eq!(replay.completion_review, Some(expected.clone()));
        assert_eq!(
            replay.pending_continuation.unwrap().target,
            if main {
                PendingContinuationTarget::Regular
            } else {
                PendingContinuationTarget::PostTurnCompletionReview
            }
        );
        if main {
            assert!(expected.handoff.unwrap().advisory_recorded);
            assert_eq!(items.iter().filter(|item| matches!(item,
                RolloutItem::ResponseItem(ResponseItem::Message { role, content, .. })
                    if role == "developer" && content == &vec![ContentItem::InputText { text: "exact advisory".into() }]
            )).count(), 1);
        } else {
            assert!(expected.handoff.is_none());
        }
    }
}

#[tokio::test]
async fn completion_review_coalesced_input_replays_each_emitted_prefix() {
    let (session, context, _) = make_session_and_context_with_rx().await;
    let (_home, path) = attach_recorder(&session, &context).await;
    let revision = session
        .accept_completion_review_input(&steer_text_input("complete request"), true, Some(&context))
        .await
        .unwrap();
    let accepted = session.state.lock().await.completion_review.clone();
    let body = accepted.pending_inputs[0].response_item.clone();
    assert!(
        session
            .start_completion_review_main_task(&context.sub_id, &context)
            .await
    );
    assert!(
        session
            .install_completion_review_input(&context, revision, true)
            .await
    );
    let expected = session.state.lock().await.completion_review.clone();
    let (items, _, errors) = RolloutRecorder::load_rollout_items(&path).await.unwrap();
    assert_eq!(errors, 0);
    let snapshots: Vec<_> = items
        .iter()
        .enumerate()
        .filter_map(|(index, item)| match item {
            RolloutItem::PostTurnCompletionReviewState(state) => Some((
                index,
                serde_json::from_value::<CompletionReviewState>(state.clone()).unwrap(),
            )),
            _ => None,
        })
        .collect();
    let mut installing = accepted.clone();
    installing.pending_inputs[0].installation = ReviewInputInstallation::Installing;
    assert_eq!(
        snapshots.iter().map(|(_, state)| state).collect::<Vec<_>>(),
        [&accepted, &installing, &expected]
    );
    let body_index = items
        .iter()
        .position(|item| matches!(item, RolloutItem::ResponseItem(item) if item == &body))
        .unwrap();
    for last in [snapshots[0].0, snapshots[1].0, body_index, snapshots[2].0] {
        let (resumed, resumed_context, _) = make_session_and_context_with_rx().await;
        let replay = resumed
            .reconstruct_history_from_rollout(&resumed_context, &items[..=last])
            .await;
        resumed.replace_with_reconstructed_rollout(replay).await;
        for revision in resumed.completion_review_input_recovery_revisions().await {
            assert!(
                resumed
                    .install_completion_review_input(&resumed_context, revision, true)
                    .await
            );
        }
        assert_eq!(
            resumed.state.lock().await.completion_review,
            expected,
            "prefix through item {last}"
        );
        assert_eq!(
            resumed
                .clone_history()
                .await
                .raw_items()
                .iter()
                .filter(|item| *item == &body)
                .count(),
            1,
            "prefix through item {last}"
        );
    }
}

#[tokio::test]
async fn completion_review_input_installation_replays_every_durable_boundary() {
    for (installation, body_written, consumed) in [
        (ReviewInputInstallation::Accepted, false, false),
        (ReviewInputInstallation::Installing, false, false),
        (ReviewInputInstallation::Installing, true, false),
        (ReviewInputInstallation::Recorded, true, false),
        (ReviewInputInstallation::Recorded, true, true),
    ] {
        let (session, context, _) = make_session_and_context_with_rx().await;
        let (_home, path) = attach_recorder(&session, &context).await;
        let content = steer_text_input("identical amendment");
        let first = session
            .accept_completion_review_input(&content, true, None)
            .await
            .unwrap();
        assert!(
            session
                .install_completion_review_input(&context, first, true)
                .await
        );
        let second = session
            .accept_completion_review_input_with_client_id(
                &content,
                false,
                Some("amendment-client".into()),
                None,
            )
            .await
            .unwrap();
        let input = session.state.lock().await.completion_review.pending_inputs[0].clone();
        if body_written {
            session.state.lock().await.completion_review.pending_inputs[0].installation =
                ReviewInputInstallation::Installing;
            session.persist_completion_review().await;
            session
                .record_user_prompt_and_emit_turn_item(
                    &context,
                    &input.content,
                    input.response_item.clone(),
                    input.client_id,
                )
                .await;
        }
        session.state.lock().await.completion_review.pending_inputs[0].installation =
            installation.clone();
        if consumed {
            session
                .state
                .lock()
                .await
                .completion_review
                .consume_input(second);
        }
        // Leave the Installing/body-written fixture at its actual crash gap.
        if installation != ReviewInputInstallation::Installing || !body_written {
            session.persist_completion_review().await;
        } else {
            session.flush_rollout().await;
        }
        let (items, _, errors) = RolloutRecorder::load_rollout_items(&path).await.unwrap();
        assert_eq!(errors, 0);
        let (resumed, resumed_context, _) = make_session_and_context_with_rx().await;
        let replay = resumed
            .reconstruct_history_from_rollout(&resumed_context, &items)
            .await;
        if !consumed {
            assert_eq!(
                replay.completion_review.as_ref().unwrap().pending_inputs[0].installation,
                if body_written {
                    ReviewInputInstallation::Recorded
                } else {
                    installation
                }
            );
            assert_eq!(
                replay.pending_continuation.as_ref().unwrap().target,
                PendingContinuationTarget::Regular
            );
        }
        resumed.replace_with_reconstructed_rollout(replay).await;
        for revision in resumed.completion_review_input_recovery_revisions().await {
            assert!(
                resumed
                    .install_completion_review_input(&resumed_context, revision, true)
                    .await
            );
        }
        assert_eq!(
            resumed
                .clone_history()
                .await
                .raw_items()
                .iter()
                .filter(|item| *item == &input.response_item)
                .count(),
            2
        );
        assert!(
            resumed
                .start_completion_review_main_task(&resumed_context.sub_id, &resumed_context)
                .await
        );
        resumed
            .capture_completed_regular_turn_for_review(&resumed_context, Some("amended answer"))
            .await;
        assert_eq!(
            resumed
                .completed_turn_for_review()
                .await
                .unwrap()
                .request_frame,
            ReviewRequestFrame {
                prior_user_rounds: vec![],
                current_user_messages: vec![
                    "identical amendment".into(),
                    "identical amendment".into(),
                ],
                text_only: true,
                exact: true,
            }
        );
    }
}

#[tokio::test]
async fn completion_review_initial_acknowledgement_recovers_without_any_history_or_main_owner() {
    let (session, context, _) = make_session_and_context_with_rx().await;
    let (_home, path) = attach_recorder(&session, &context).await;
    session
        .accept_completion_review_input(
            &steer_text_input("accepted before task startup"),
            true,
            None,
        )
        .await
        .unwrap();
    let (items, _, errors) = RolloutRecorder::load_rollout_items(&path).await.unwrap();
    assert_eq!(errors, 0);
    let replay = session
        .reconstruct_history_from_rollout(&context, &items)
        .await;
    assert!(replay.history.is_empty());
    assert_eq!(
        replay.pending_continuation.as_ref().unwrap().target,
        PendingContinuationTarget::Regular
    );
    assert!(
        replay
            .completion_review
            .as_ref()
            .unwrap()
            .main_task_id
            .is_none()
    );
    session.replace_with_reconstructed_rollout(replay).await;
    for revision in session.completion_review_input_recovery_revisions().await {
        assert!(
            session
                .install_completion_review_input(&context, revision, true)
                .await
        );
    }
    assert_eq!(
        session.clone_history().await.raw_items(),
        &[user_message("accepted before task startup")]
    );
    assert!(
        session
            .state
            .lock()
            .await
            .completion_review
            .pending_inputs
            .is_empty()
    );
}

struct CompletingReviewTask {
    ready: Arc<Notify>,
    finish: Arc<Notify>,
}

#[tokio::test]
async fn completion_review_manual_reservation_joins_blocked_finalization() {
    let (session, mut context) = completed_session().await;
    let mut expected = session.completed_turn_for_review().await.unwrap();
    Arc::get_mut(&mut context).unwrap().sub_id = "main-replacement".into();
    expected.turn_id.clone_from(&context.sub_id);
    expected.final_agent_message = "durable terminal replacement".into();
    let (_home, path) = attach_recorder(&session, &context).await;
    let ready = Arc::new(Notify::new());
    let finish = Arc::new(Notify::new());
    session
        .spawn_task(
            Arc::clone(&context),
            vec![],
            CompletingReviewTask {
                ready: Arc::clone(&ready),
                finish: Arc::clone(&finish),
            },
        )
        .await;
    ready.notified().await;
    assert_eq!(
        session.reserve_manual_completion_review().await,
        Err(CompletionReviewReservationError::TaskRunning)
    );
    let installation = session.completion_review_installation.lock().await;
    finish.notify_one();
    tokio::time::timeout(Duration::from_secs(2), async {
        while session.active_turn.lock().await.is_some() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let reviewing_session = Arc::clone(&session);
    let mut reviewing =
        tokio::spawn(async move { reviewing_session.reserve_manual_completion_review().await });
    assert!(
        tokio::time::timeout(Duration::from_millis(25), &mut reviewing)
            .await
            .is_err()
    );
    drop(installation);
    let (candidate, ticket) = tokio::time::timeout(Duration::from_secs(2), reviewing)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(candidate, expected);
    assert!(ticket.matches(&candidate));
    assert!(session.completion_review_ticket_is_current(&ticket).await);
    // The real reservation must flush both the replacement and its matching ticket.
    let (items, _, errors) = RolloutRecorder::load_rollout_items(&path).await.unwrap();
    assert_eq!(errors, 0);
    let replay = session
        .reconstruct_history_from_rollout(&context, &items)
        .await;
    let review = replay.completion_review.unwrap();
    assert_eq!(review.eligible_episode(), Some(&candidate));
    assert_eq!(review.attempt, Some(ticket));
}

impl SessionTask for CompletingReviewTask {
    fn kind(&self) -> TaskKind {
        TaskKind::Regular
    }

    async fn run(
        self: Arc<Self>,
        session: Arc<SessionTaskContext>,
        context: Arc<TurnContext>,
        _input: Vec<UserInput>,
        _cancellation: CancellationToken,
    ) -> Option<String> {
        assert!(
            session
                .clone_session()
                .start_completion_review_main_task(&context.sub_id, &context)
                .await
        );
        self.ready.notify_one();
        self.finish.notified().await;
        Some("durable terminal replacement".into())
    }
}

#[derive(Clone, Copy)]
enum CompletionStop {
    Pause,
    Interrupt,
    Shutdown,
}

#[tokio::test]
async fn completion_review_stop_joins_finalization_after_active_entry_is_removed() {
    for stop in [
        CompletionStop::Pause,
        CompletionStop::Interrupt,
        CompletionStop::Shutdown,
    ] {
        let (session, mut context) = completed_session().await;
        Arc::get_mut(&mut context)
            .unwrap()
            .features
            .enable(Feature::AutoPostTurnCompletionReview);
        let (_home, path) = attach_recorder(&session, &context).await;
        let ready = Arc::new(Notify::new());
        let finish = Arc::new(Notify::new());
        session
            .spawn_task(
                Arc::clone(&context),
                vec![],
                CompletingReviewTask {
                    ready: Arc::clone(&ready),
                    finish: Arc::clone(&finish),
                },
            )
            .await;
        ready.notified().await;
        let installation = session.completion_review_installation.lock().await;
        finish.notify_one();
        tokio::time::timeout(Duration::from_secs(2), async {
            while session.active_turn.lock().await.is_some() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(session.task_lifecycle.try_lock().is_err());
        let stopping_session = Arc::clone(&session);
        let mut stopping = tokio::spawn(async move {
            match stop {
                CompletionStop::Pause => super::super::handlers::pause(&stopping_session).await,
                CompletionStop::Interrupt => {
                    super::super::handlers::interrupt(&stopping_session).await;
                }
                CompletionStop::Shutdown => {
                    assert!(
                        super::super::handlers::shutdown(&stopping_session, "shutdown".into())
                            .await
                    );
                }
            }
        });
        assert!(
            tokio::time::timeout(Duration::from_millis(25), &mut stopping)
                .await
                .is_err()
        );
        assert!(session.services.rollout.lock().await.is_some());
        drop(installation);
        tokio::time::timeout(Duration::from_secs(2), stopping)
            .await
            .unwrap()
            .unwrap();
        // No test-side flush, including when shutdown has removed the recorder.
        let (items, _, errors) = RolloutRecorder::load_rollout_items(&path).await.unwrap();
        assert_eq!(errors, 0);
        let replay = session
            .reconstruct_history_from_rollout(&context, &items)
            .await;
        let review = replay.completion_review.unwrap();
        assert_eq!(
            review.eligible_episode().unwrap().final_agent_message,
            "durable terminal replacement"
        );
        assert!(review.attempt.is_none());
        assert!(review.handoff.is_none());
        assert!(session.take_pending_continuation().await.unwrap().is_none());
    }
}

struct InputInstallationTask {
    revision: u64,
    ready: Arc<Notify>,
    enter: Arc<Notify>,
}

impl SessionTask for InputInstallationTask {
    fn kind(&self) -> TaskKind {
        TaskKind::Regular
    }

    async fn run(
        self: Arc<Self>,
        session: Arc<SessionTaskContext>,
        context: Arc<TurnContext>,
        _input: Vec<UserInput>,
        _cancellation: CancellationToken,
    ) -> Option<String> {
        let session = session.clone_session();
        assert!(
            session
                .start_completion_review_main_task(&context.sub_id, &context)
                .await
        );
        self.ready.notify_one();
        self.enter.notified().await;
        assert!(
            session
                .install_completion_review_input(&context, self.revision, true)
                .await
        );
        None
    }
}

#[tokio::test]
async fn completion_review_pause_joins_cancelled_input_installation_and_replays_it() {
    let (session, context, _) = make_session_and_context_with_rx().await;
    let (_home, path) = attach_recorder(&session, &context).await;
    let revision = session
        .accept_completion_review_input(&steer_text_input("acknowledged amendment"), true, None)
        .await
        .unwrap();
    let ready = Arc::new(Notify::new());
    let enter = Arc::new(Notify::new());
    session
        .spawn_task(
            Arc::clone(&context),
            vec![],
            InputInstallationTask {
                revision,
                ready: Arc::clone(&ready),
                enter: Arc::clone(&enter),
            },
        )
        .await;
    ready.notified().await;
    let (cancel, handle) = {
        let active = session.active_turn.lock().await;
        let task = active.as_ref().unwrap().tasks.get(&context.sub_id).unwrap();
        (task.cancellation_token.clone(), Arc::clone(&task.handle))
    };
    let state = session.state.lock().await;
    enter.notify_one();
    tokio::time::timeout(Duration::from_secs(2), async {
        while session.completion_review_installation.try_lock().is_ok() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let pausing_session = Arc::clone(&session);
    let paused = tokio::spawn(async move { super::super::handlers::pause(&pausing_session).await });
    cancel.cancelled().await;
    tokio::time::timeout(Duration::from_secs(2), async {
        while !handle.is_finished() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    drop(state);
    tokio::time::timeout(Duration::from_secs(2), paused)
        .await
        .unwrap()
        .unwrap();
    // Read immediately after the real stop handler, without a test-side flush.
    let (items, _, errors) = RolloutRecorder::load_rollout_items(&path).await.unwrap();
    assert_eq!(errors, 0);
    let replay = session
        .reconstruct_history_from_rollout(&context, &items)
        .await;
    let review = replay.completion_review.unwrap();
    assert_eq!(
        review.request_frame.current_user_messages,
        vec!["acknowledged amendment"]
    );
    assert!(review.pending_inputs.is_empty());
    assert_eq!(
        replay
            .history
            .iter()
            .filter(|item| *item == &user_message("acknowledged amendment"))
            .count(),
        1
    );
    assert_eq!(
        replay.pending_continuation.unwrap().target,
        PendingContinuationTarget::Regular
    );
}
