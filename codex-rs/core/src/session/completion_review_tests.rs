use super::*;
use crate::completion_review::CompletionReviewState;
use crate::completion_review::ReviewHandoffDisposition;
use crate::completion_review::ReviewInputInstallation;
use crate::completion_review::ReviewRequestFrame;
use pretty_assertions::assert_eq;
use tokio::sync::Notify;

async fn completed_session() -> (Arc<Session>, Arc<TurnContext>) {
    let (session, context, _) = make_session_and_context_with_rx().await;
    let revision = session
        .accept_completion_review_input(&steer_text_input("original request"), true)
        .await;
    assert!(
        session
            .install_completion_review_input(&context, revision, true)
            .await
    );
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
    assert!(
        session
            .install_completion_review_input(&context, first, true)
            .await
    );
    session
        .record_conversation_items(&context, &[assistant_message("progress")])
        .await;
    let second = session
        .accept_completion_review_input(&steer_text_input("steering"), false)
        .await;
    assert!(
        session
            .install_completion_review_input(&context, second, true)
            .await
    );
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
    assert!(
        session
            .install_completion_review_input(&context, first, true)
            .await
    );
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
    for revision in session.completion_review_input_recovery_revisions().await {
        assert!(
            session
                .install_completion_review_input(&context, revision, true)
                .await
        );
    }
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
    assert!(
        session
            .install_completion_review_input(&context, revision, true)
            .await
    );
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
        let first = session.accept_completion_review_input(&content, true).await;
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
            )
            .await;
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
        resumed
            .start_completion_review_main_task(&resumed_context.sub_id)
            .await;
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
        .accept_completion_review_input(&steer_text_input("accepted before task startup"), true)
        .await;
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
        session
            .clone_session()
            .start_completion_review_main_task(&context.sub_id)
            .await;
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
        assert!(session.take_pending_continuation().await.is_none());
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
        session
            .start_completion_review_main_task(&context.sub_id)
            .await;
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
        .accept_completion_review_input(&steer_text_input("acknowledged amendment"), true)
        .await;
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
