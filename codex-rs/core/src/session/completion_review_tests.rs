use super::*;
use crate::completion_review::CompletionReviewState;
use crate::completion_review::ReviewHandoffDisposition;
use crate::completion_review::ReviewRequestFrame;
use pretty_assertions::assert_eq;

async fn completed_session() -> (Arc<Session>, Arc<TurnContext>) {
    let (session, context, _) = make_session_and_context_with_rx().await;
    let revision = session
        .accept_completion_review_input(&steer_text_input("original request"), true)
        .await;
    session.consume_completion_review_input(revision).await;
    session
        .start_completion_review_main_task(&context.sub_id)
        .await;
    session
        .capture_completed_regular_turn_for_review(&context, Some(" initial complete answer\n"))
        .await;
    (session, context)
}

fn metadata(state: &CompletionReviewState) -> RolloutItem {
    RolloutItem::PostTurnCompletionReviewState(serde_json::to_value(state).unwrap())
}

async fn attach_recorder(
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
            SessionSource::Cli,
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
async fn completion_review_capture_uses_task_result_not_assistant_role() {
    let (session, context) = completed_session().await;
    session
        .record_conversation_items(&context, &[assistant_message("commentary, not the final")])
        .await;
    session
        .start_completion_review_main_task(&context.sub_id)
        .await;
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
        .accept_completion_review_input(&steer_text_input("original"), true)
        .await;
    session.consume_completion_review_input(first).await;
    session
        .record_conversation_items(
            &context,
            &[user_message("original"), assistant_message("progress")],
        )
        .await;
    let second = session
        .accept_completion_review_input(&steer_text_input("steering"), false)
        .await;
    session.consume_completion_review_input(second).await;
    session
        .start_completion_review_main_task(&context.sub_id)
        .await;
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
        .accept_completion_review_input(&steer_text_input("original request"), true)
        .await;
    session.consume_completion_review_input(first).await;
    let revision = session
        .accept_completion_review_input(&steer_text_input("late amendment"), false)
        .await;
    let input = TurnInput::UserInput {
        content: steer_text_input("late amendment"),
        client_id: None,
        review_revision: Some(revision),
    };
    session
        .retain_completion_review_input(&context, input)
        .await;
    session
        .start_completion_review_main_task(&context.sub_id)
        .await;
    session
        .capture_completed_regular_turn_for_review(&context, Some("old sampling result"))
        .await;
    assert!(session.completed_turn_for_review().await.is_none());
    session.consume_recorded_completion_review_inputs().await;
    session
        .start_completion_review_main_task(&context.sub_id)
        .await;
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
            .claim_completion_review_handoff(&ticket, "main-followup")
            .await
            .is_some()
    );
    assert!(
        session
            .claim_completion_review_handoff(&ticket, "duplicate")
            .await
            .is_none()
    );
    let snapshot = session.state.lock().await.completion_review.clone();
    session
        .replace_compacted_history(
            &context,
            vec![assistant_message("compacted")],
            None,
            CompactedItem {
                message: "compacted".into(),
                replacement_history: Some(vec![assistant_message("compacted")]),
            },
        )
        .await;
    assert_eq!(session.state.lock().await.completion_review, snapshot);
    let prompt = session
        .completion_review_main_instructions(true)
        .await
        .unwrap();
    assert!(prompt.contains("original request"));
    assert!(prompt.contains(" initial complete answer\n"));
    assert!(prompt.contains("advisory plus replacement contract"));
    session.record_completion_review_advisory(&context).await;
    session.record_completion_review_advisory(&context).await;
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
        .claim_completion_review_handoff(&ticket, "started-main")
        .await
        .unwrap();
    let started = session.state.lock().await.completion_review.clone();
    let replay = session
        .reconstruct_history_from_rollout(&context, &[metadata(&started)])
        .await;
    assert_eq!(replay.completion_review, Some(started));
    let checkpoint = replay.pending_continuation.unwrap();
    assert_eq!(checkpoint.target, PendingContinuationTarget::Regular);
    assert_eq!(
        checkpoint.continued_from_turn_id.as_deref(),
        Some("started-main")
    );
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
async fn completion_review_stale_tickets_cannot_prepare_or_claim_advice() {
    let (session, context) = completed_session().await;
    let old = session.begin_completion_review(false).await.unwrap();
    let revision = session
        .accept_completion_review_input(&steer_text_input("new user request"), true)
        .await;
    session.consume_completion_review_input(revision).await;
    session
        .start_completion_review_main_task(&context.sub_id)
        .await;
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
            .claim_completion_review_handoff(&old, "stale-main")
            .await
            .is_none()
    );
    session.finish_completion_review(&old).await;
    assert!(session.completion_review_ticket_is_current(&current).await);
}

#[tokio::test]
async fn completion_review_old_task_cannot_capture_or_schedule_a_newer_candidate() {
    let (session, context) = completed_session().await;
    let original = session.completed_turn_for_review().await;
    session
        .start_completion_review_main_task("newer-main")
        .await;
    session
        .capture_completed_regular_turn_for_review(&context, Some("stale old result"))
        .await;
    assert_eq!(session.completed_turn_for_review().await, original);
    assert!(
        session
            .begin_completion_review_for_candidate(true, Some("not-the-candidate"))
            .await
            .is_none()
    );
}

#[tokio::test]
async fn completion_review_fork_excludes_metadata_for_the_next_unconsumed_user_boundary() {
    let (session, context) = completed_session().await;
    let original = session.state.lock().await.completion_review.clone();
    session
        .accept_completion_review_input(&steer_text_input("excluded future request"), true)
        .await;
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
    for marker in [
        RolloutItem::PostTurnCompletionReviewState(serde_json::json!({"version": 99})),
        metadata(&CompletionReviewState::default()),
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
    assert!(session.take_pending_continuation().await.is_none());
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
                .claim_completion_review_handoff(&ticket, &context.sub_id)
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
        self.ready.notify_one();
        self.enter.notified().await;
        let session = session.clone_session();
        if self.main {
            session.record_completion_review_advisory(&context).await;
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
                .claim_completion_review_handoff(&ticket, &context.sub_id)
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
