use super::*;
use pretty_assertions::assert_eq;

fn text_input(text: &str) -> Vec<UserInput> {
    vec![UserInput::Text {
        text: text.into(),
        text_elements: vec![],
    }]
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
    state.replacement_answer_required = true;
    let revision = state.accept_input(false, text_input("use option two from that answer"), None);
    consume(&mut state, revision);
    assert!(state.replacement_answer_required);
    state.capture(
        "amended".into(),
        PathBuf::new(),
        "complete amended result".into(),
    );
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
    let ticket = state.begin_review(false).unwrap();
    state.accept_input(true, text_input("implement X and explain Y"), None);
    assert!(!state.ticket_is_current(&ticket));
}

#[test]
fn automatic_allowance_survives_replacement_manual_review_and_replay() {
    let mut state = completed_state();
    for index in 1..=AUTOMATIC_REVIEW_LIMIT {
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
    assert!(state.begin_review(true).is_none());
    assert!(state.begin_review(false).is_some());
    assert_eq!(
        state.episode.as_ref().unwrap().automatic_reviews_started,
        AUTOMATIC_REVIEW_LIMIT
    );
    state.capture(
        "manual-followup".into(),
        PathBuf::new(),
        "manual replacement".into(),
    );
    assert!(state.begin_review(true).is_none());
}

#[test]
fn new_request_gets_a_new_allowance() {
    let mut state = completed_state();
    state.episode.as_mut().unwrap().automatic_reviews_started = AUTOMATIC_REVIEW_LIMIT;
    let revision = state.accept_input(true, text_input("next request"), None);
    consume(&mut state, revision);
    state.capture("next".into(), PathBuf::new(), "next answer".into());
    assert!(state.begin_review(true).is_some());
}

#[test]
fn branch_generation_invalidates_old_work_without_changing_candidate() {
    let mut state = completed_state();
    let ticket = state.begin_review(false).unwrap();
    let text = state.episode.as_ref().unwrap().final_agent_message.clone();
    state.rebranch();
    assert!(!state.ticket_is_current(&ticket));
    assert_eq!(state.eligible_episode().unwrap().final_agent_message, text);
    assert!(state.attempt.is_none());
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
