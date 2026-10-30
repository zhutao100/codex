use super::*;
use crate::completion_review::CompletionReviewState;
use crate::completion_review::ReviewHandoffDisposition;
use crate::completion_review::ReviewInputInstallation;
use crate::completion_review::ReviewRequestFrame;
use pretty_assertions::assert_eq;
use tokio::sync::Notify;

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

async fn complete_new_request(
    session: &Arc<Session>,
    context: &Arc<TurnContext>,
    request: &str,
    answer: &str,
) {
    let revision = session
        .accept_completion_review_input(&steer_text_input(request), true)
        .await;
    assert!(
        session
            .install_completion_review_input(context, revision, true)
            .await
    );
    assert!(
        session
            .start_completion_review_main_task(&context.sub_id)
            .await
    );
    session
        .capture_completed_regular_turn_for_review(context, Some(answer))
        .await;
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
    assert!(
        session
            .start_completion_review_main_task(&context.sub_id)
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
    assert!(
        session
            .start_completion_review_main_task(&context.sub_id)
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
    assert!(
        session
            .start_completion_review_main_task(&context.sub_id)
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
            .start_completion_review_main_task(&context.sub_id)
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
        .unwrap()
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
            super::super::handlers::continue_last(&session, "failed-main".into()).await;
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
        assert!(!messages.iter().any(|event| {
            matches!(
                event,
                EventMsg::TurnStarted(_) | EventMsg::EnteredReviewMode(_)
            )
        }));
    }
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
            .claim_completion_review_handoff(&ticket, &context.sub_id)
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
                .replace_compacted_history(
                    &context,
                    replacement.clone(),
                    None,
                    CompactedItem {
                        message: String::new(),
                        replacement_history: Some(replacement),
                    },
                )
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
                .claim_completion_review_handoff(&ticket, "main-followup")
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
                .claim_completion_review_handoff(&ticket, "main-followup")
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
                    .start_completion_review_main_task("main-followup")
                    .await
            );
            let revision = session
                .accept_completion_review_input(&steer_text_input("preserve the public API"), false)
                .await;
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
        .claim_completion_review_handoff(&ticket, "started-main")
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
            assert!(session.reserve_manual_completion_review().await.is_none());
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
        assert!(session.reserve_manual_completion_review().await.is_none());
    }
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
    assert!(
        session
            .start_completion_review_main_task(&context.sub_id)
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
    assert!(
        session
            .start_completion_review_main_task("newer-main")
            .await
    );
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
        assert!(
            resumed
                .start_completion_review_main_task(&resumed_context.sub_id)
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
                .start_completion_review_main_task(&context.sub_id)
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
                .start_completion_review_main_task(&context.sub_id)
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
