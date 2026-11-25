use super::*;
use pretty_assertions::assert_eq;

fn text_input(text: &str) -> Vec<UserInput> {
    vec![UserInput::Text {
        text: text.into(),
        text_elements: vec![],
    }]
}

#[test]
fn earlier_review_deferral_restores_only_its_recorded_model_hint() {
    let mut saved = serde_json::to_value(CompletionReviewState::default()).unwrap();
    saved["defer_new_prompt_frame"] = true.into();
    saved["deferred_capture_model"] = "previous".into();
    let mut state = CompletionReviewState::from_rollout(&saved).unwrap();
    assert_eq!(
        state.recovery_capture_source("destination"),
        Some(DeferredCaptureSource::LegacyModel("previous".into()))
    );
    state.clear_prompt_frames();
    assert_eq!(state.recovery_capture_source("destination"), None);
}

fn consume(state: &mut CompletionReviewState, revision: u64) {
    state
        .pending_inputs
        .iter_mut()
        .find(|input| input.revision == revision)
        .unwrap()
        .installation = ReviewInputInstallation::Recorded;
    state.consume_input(revision);
}

#[test]
fn accepted_input_cannot_be_consumed_before_its_history_installation() {
    let mut state = CompletionReviewState::default();
    let revision = state.accept_input(true, text_input("amendment"), None);
    let accepted = state.clone();
    state.consume_input(revision);
    assert_eq!(state, accepted);
    let expected = state.pending_inputs[0].response_item.clone();
    assert!(!state.observe_input_installation(&expected));
    state.pending_inputs[0].installation = ReviewInputInstallation::Installing;
    assert!(state.observe_input_installation(&expected));
    state.consume_input(revision);
    assert!(state.pending_inputs.is_empty());
    assert_eq!(state.request_frame.current_user_messages, vec!["amendment"]);
}

fn completed_state() -> CompletionReviewState {
    let mut state = CompletionReviewState::default();
    let revision = state.accept_input(true, text_input("implement X and explain Y"), None);
    consume(&mut state, revision);
    state.capture(
        "main-0".into(),
        PathBuf::from("/tmp/project"),
        "  initial X and Y\n".into(),
    );
    state
}

#[test]
fn frame_anchors_survive_pending_input_but_not_consumption_or_fork() {
    let mut state = completed_state();
    let frames = ReviewPromptFrames(vec![ReviewPromptFrame::new(
        "outgoing frame".into(),
        1,
        Some("review-1".into()),
    )]);
    state.prompt_frames = frames.clone();
    let revision = state.accept_input(false, text_input("new scope"), None);
    assert_eq!(state.prompt_frames, frames);
    let mut forked = state.clone();
    forked.rebranch();
    assert!(forked.prompt_frames.is_empty());
    consume(&mut state, revision);
    assert!(state.prompt_frames.is_empty());

    let marker =
        DeveloperInstructions::new(crate::compact::AUTO_COMPACT_WORK_NOTES_REQUEST_TAG).into();
    let mut orphan = vec![marker];
    assert!(frames.insert(&mut orphan, Some(0)).is_err());
}

#[test]
fn ten_followups_keep_frozen_request_and_initial_reference() {
    let mut state = completed_state();
    let original = state.episode.clone().unwrap();
    for index in 1..=10 {
        let ticket = state.begin_review(false).unwrap();
        assert!(state.ticket_is_current(&ticket));
        state.capture(
            format!("main-{index}"),
            original.cwd.clone(),
            format!("replacement {index}: current X and Y"),
        );
        let mut expected = original.clone();
        expected.turn_id = format!("main-{index}");
        expected.final_agent_message = format!("replacement {index}: current X and Y");
        assert_eq!(state.episode, Some(expected));
        assert!(!state.ticket_is_current(&ticket));
    }
}

#[test]
fn replacement_candidate_tracks_the_actual_completion_directory() {
    let mut state = completed_state();
    state.begin_review(true).unwrap();
    let mut expected = state.episode.clone().unwrap();
    expected.turn_id = "replacement".into();
    expected.cwd = PathBuf::from("/tmp/other-project");
    expected.final_agent_message = "replacement from the actual task directory".into();
    state.capture(
        expected.turn_id.clone(),
        expected.cwd.clone(),
        expected.final_agent_message.clone(),
    );
    assert_eq!(state.eligible_episode(), Some(&expected));
    let restored =
        CompletionReviewState::from_rollout(&serde_json::to_value(&state).unwrap()).unwrap();
    assert_eq!(restored, state);
}

#[test]
fn steering_is_owned_by_current_request_not_a_fake_completed_round() {
    let mut state = CompletionReviewState::default();
    let first = state.accept_input(true, text_input("original"), None);
    consume(&mut state, first);
    let second = state.accept_input(false, text_input("steering"), None);
    consume(&mut state, second);
    state.capture("main".into(), PathBuf::new(), "final".into());
    assert_eq!(
        state.eligible_episode().unwrap().request_frame,
        ReviewRequestFrame {
            prior_user_rounds: vec![],
            current_user_messages: vec!["original".into(), "steering".into()],
            text_only: true,
            exact: true,
        }
    );
}

#[test]
fn separate_user_task_keeps_only_previous_terminal_answer() {
    let mut state = completed_state();
    state.capture(
        "replacement".into(),
        PathBuf::new(),
        "corrected X and Y".into(),
    );
    let revision = state.accept_input(true, text_input("use option two"), None);
    consume(&mut state, revision);
    state.capture("second".into(), PathBuf::new(), "second answer".into());
    assert_eq!(
        state.episode.unwrap().request_frame,
        ReviewRequestFrame {
            prior_user_rounds: vec![CompletedTurnReviewRound {
                user_messages: vec!["implement X and explain Y".into()],
                final_agent_message: "corrected X and Y".into(),
            }],
            current_user_messages: vec!["use option two".into()],
            text_only: true,
            exact: true,
        }
    );
}

#[test]
fn steering_a_review_followup_retains_the_known_previous_answer_as_context() {
    let mut state = completed_state();
    state.main_task_id = Some("review-followup".into());
    state.recovery_model = Some("recovery-model".into());
    state.replacement_answer_required = true;
    let revision = state.accept_input(false, text_input("use option two from that answer"), None);
    consume(&mut state, revision);
    assert!(state.replacement_answer_required);
    assert_eq!(state.recovery_model.as_deref(), Some("recovery-model"));
    state.capture(
        "amended".into(),
        PathBuf::new(),
        "complete amended result".into(),
    );
    assert!(state.recovery_model.is_none());
    let episode = state.episode.unwrap();
    assert_eq!(
        episode.request_frame.prior_user_rounds,
        vec![CompletedTurnReviewRound {
            user_messages: vec!["implement X and explain Y".into()],
            final_agent_message: "  initial X and Y\n".into(),
        }]
    );
    assert_eq!(
        episode.request_frame.current_user_messages,
        vec!["use option two from that answer"]
    );
}

#[test]
fn accepted_but_unconsumed_input_invalidates_ticket_and_capture() {
    let mut state = completed_state();
    let ticket = state.begin_review(true).unwrap();
    let original = state.episode.clone();
    state.accept_input(false, text_input("late amendment"), None);
    assert!(!state.ticket_is_current(&ticket));
    state.capture(
        "late-final".into(),
        PathBuf::new(),
        "not an answer to the amendment".into(),
    );
    assert_eq!(state.episode, original);
    assert!(state.eligible_episode().is_none());
    assert!(state.begin_review(false).is_none());
}

#[test]
fn repeated_identical_user_text_still_invalidates_ticket() {
    let mut state = completed_state();
    state.recovery_model = Some("previous-recovery-model".into());
    let ticket = state.begin_review(false).unwrap();
    state.accept_input(true, text_input("implement X and explain Y"), None);
    assert!(!state.ticket_is_current(&ticket));
    assert!(state.recovery_model.is_none());
}

#[test]
fn automatic_reviews_remain_eligible_across_replacements_and_replay() {
    let mut state = completed_state();
    let initial = state
        .eligible_episode()
        .unwrap()
        .initial_answer_reference
        .clone();
    for index in 1..=5 {
        let ticket = state.begin_review(true).unwrap();
        let encoded = serde_json::to_value(&state).unwrap();
        state = CompletionReviewState::from_rollout(&encoded).unwrap();
        assert!(state.ticket_is_current(&ticket));
        state.capture(
            format!("main-{index}"),
            PathBuf::new(),
            "replacement".into(),
        );
    }
    let episode = state.eligible_episode().unwrap();
    assert_eq!(episode.turn_id, "main-5");
    assert_eq!(episode.initial_answer_reference, initial);
}

#[test]
fn branch_generation_invalidates_old_work_without_changing_candidate() {
    let mut state = completed_state();
    state.recovery_model = Some("previous-recovery-model".into());
    let ticket = state.begin_review(false).unwrap();
    let text = state.episode.as_ref().unwrap().final_agent_message.clone();
    state.rebranch();
    assert!(!state.ticket_is_current(&ticket));
    assert_eq!(state.eligible_episode().unwrap().final_agent_message, text);
    assert!(state.attempt.is_none());
    assert!(state.recovery_model.is_none());
}

#[test]
fn incomplete_or_future_metadata_is_not_reconstructed_as_exact() {
    assert!(CompletionReviewState::from_rollout(&serde_json::json!({"version": 1})).is_none());
    let mut value = serde_json::to_value(completed_state()).unwrap();
    value["version"] = serde_json::json!(99);
    assert!(CompletionReviewState::from_rollout(&value).is_none());
}

#[test]
fn blank_or_partial_completion_never_replaces_candidate() {
    let mut state = completed_state();
    let original = state.clone();
    state.capture("partial".into(), PathBuf::new(), " \n".into());
    assert_eq!(state, original);
}
