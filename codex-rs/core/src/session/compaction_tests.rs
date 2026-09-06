use super::*;
use crate::compact::CompactionImplementation;
use crate::compact::compaction_implementation;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn stable_compaction_feature_is_advertised_without_displacing_other_flags() {
    let home = tempfile::tempdir().unwrap();
    let mut config = build_test_config(home.path()).await;
    let header = Session::build_model_client_beta_features_header(&config).unwrap();
    assert!(header.split(',').any(|key| key == "remote_compaction_v2"));
    config.features.disable(Feature::RemoteCompactionV2);
    let disabled = Session::build_model_client_beta_features_header(&config).unwrap_or_default();
    assert_eq!(
        header
            .split(',')
            .filter(|key| *key != "remote_compaction_v2")
            .collect::<Vec<_>>()
            .join(","),
        disabled,
    );
}

#[tokio::test]
async fn encrypted_only_replay_uses_inline_provenance_and_completion_tail() {
    let (session, context) = make_session_and_context().await;
    let baseline = context.to_turn_context_item();
    for inline in [false, true] {
        for notes in [false, true] {
            for completed in [false, true] {
                let mut history = vec![ResponseItem::Compaction {
                    encrypted_content: "encrypted".into(),
                }];
                if notes {
                    history.push(compact::preserved_work_notes_message("exact notes"));
                }
                let mut rollout = vec![
                    RolloutItem::TurnContext(baseline.clone()),
                    RolloutItem::ResponseItem(user_message("original task")),
                    RolloutItem::Compacted(CompactedItem {
                        message: String::new(),
                        replacement_history: Some(history.clone()),
                    }),
                ];
                if inline {
                    rollout.push(RolloutItem::TurnContext(baseline.clone()));
                }
                if completed {
                    let answer = assistant_message("finished");
                    history.push(answer.clone());
                    rollout.push(RolloutItem::ResponseItem(answer));
                }
                let reconstructed = session
                    .reconstruct_history_from_rollout(&context, &rollout)
                    .await;
                assert_eq!(
                    reconstructed,
                    ReconstructedRollout {
                        history,
                        reference_context_item: inline.then(|| baseline.clone()),
                        previous_turn_settings: Some(PreviousTurnSettings {
                            model: baseline.model.clone()
                        }),
                        pending_continuation: (inline && !completed).then(|| PendingContinuation {
                            source: TurnContinuationSource::Interrupted,
                            continued_from_turn_id: None,
                            model: Some(baseline.model.clone()),
                            pause_reason: None,
                            target: PendingContinuationTarget::Regular,
                        }),
                    }
                );
            }
        }
    }
}

#[tokio::test]
async fn encrypted_only_replay_retains_incomplete_tool_rounds_and_respects_rollback() {
    let (session, context) = make_session_and_context().await;
    let mut rollout = vec![
        RolloutItem::Compacted(CompactedItem {
            message: String::new(),
            replacement_history: Some(vec![ResponseItem::Compaction {
                encrypted_content: "encrypted".into(),
            }]),
        }),
        RolloutItem::TurnContext(context.to_turn_context_item()),
        RolloutItem::ResponseItem(assistant_message("intermediate")),
        RolloutItem::ResponseItem(ResponseItem::FunctionCall {
            id: None,
            call_id: "pending".into(),
            name: "shell".into(),
            arguments: "{}".into(),
        }),
    ];
    assert!(
        session
            .reconstruct_history_from_rollout(&context, &rollout)
            .await
            .pending_continuation
            .is_some()
    );
    rollout.push(RolloutItem::EventMsg(EventMsg::ThreadRolledBack(
        crate::protocol::ThreadRolledBackEvent { num_turns: 1 },
    )));
    assert!(
        session
            .reconstruct_history_from_rollout(&context, &rollout)
            .await
            .pending_continuation
            .is_none()
    );
}

#[tokio::test]
async fn completed_work_notes_remain_pending_work_after_restart() {
    let (session, context) = make_session_and_context().await;
    let request: ResponseItem = DeveloperInstructions::new(format!(
        "{}\nnotes",
        compact::AUTO_COMPACT_WORK_NOTES_REQUEST_TAG
    ))
    .into();
    let notes = "<AUTO_COMPACT_WORK_NOTES>\n  Unicode ✅\nexact\n";
    let history = vec![user_message("task"), request, assistant_message(notes)];
    let rollout: Vec<_> = history
        .iter()
        .cloned()
        .map(RolloutItem::ResponseItem)
        .collect();
    let reconstructed = session
        .reconstruct_history_from_rollout(&context, &rollout)
        .await;
    assert_eq!(reconstructed.history, history);
    assert!(reconstructed.pending_continuation.is_some());
    let pending = compact::pending_work_notes(&history).unwrap().unwrap();
    assert_eq!(
        pending,
        compact::PendingWorkNotes {
            request_index: 1,
            notes: Some(notes.into()),
            attempts: 1
        }
    );
    let mut source = ContextManager::new();
    source.replace(history.clone());
    let prepared = compact::prepare_history_for_compaction(source, None).unwrap();
    assert_eq!(prepared.source_history.raw_items(), &history[..1]);
    assert_eq!(prepared.preserved_work_notes, Some(notes.into()));
    let mut abandoned = history;
    abandoned.push(user_message("later genuine work"));
    let mut source = ContextManager::new();
    source.replace(abandoned.clone());
    assert!(compact::prepare_history_for_compaction(source.clone(), None).is_err());
    assert_eq!(source.raw_items(), abandoned);
}

#[test]
fn canonical_context_anchors_on_images_then_summaries_then_encrypted_state() {
    let context: ResponseItem = DeveloperInstructions::new("canonical").into();
    let image = ResponseItem::Message {
        id: None,
        role: "user".into(),
        content: vec![ContentItem::InputImage {
            image_url: "remote".into(),
        }],
        end_turn: None,
        phase: None,
    };
    let summary = user_message(&format!("{}\nsummary", compact::SUMMARY_PREFIX));
    let encrypted = ResponseItem::Compaction {
        encrypted_content: "encrypted".into(),
    };
    for (mut history, expected) in [
        (
            vec![image.clone(), summary.clone(), encrypted.clone()],
            vec![context.clone(), image, summary.clone(), encrypted.clone()],
        ),
        (
            vec![summary.clone(), summary.clone(), encrypted.clone()],
            vec![summary.clone(), context.clone(), summary, encrypted.clone()],
        ),
        (vec![encrypted.clone()], vec![context.clone(), encrypted]),
        (
            vec![assistant_message("unrelated")],
            vec![assistant_message("unrelated"), context.clone()],
        ),
    ] {
        compact::insert_initial_context_before_last_real_user_message(
            &mut history,
            vec![context.clone()],
        );
        assert_eq!(history, expected);
    }
}

#[test]
fn continuation_trims_dangling_work_after_encrypted_compaction_and_keeps_undo() {
    let summary = ResponseItem::Compaction {
        encrypted_content: "summary".into(),
    };
    let snapshot = ResponseItem::GhostSnapshot {
        ghost_commit: codex_git::GhostCommit::new("snapshot".into(), None, vec![], vec![]),
    };
    let mut history = vec![
        summary.clone(),
        ResponseItem::FunctionCall {
            id: None,
            call_id: "unfinished".into(),
            name: "shell".into(),
            arguments: "{}".into(),
        },
        snapshot.clone(),
    ];
    assert!(trim_incomplete_continuation_tail(&mut history));
    assert_eq!(history, vec![summary, snapshot]);
}

#[tokio::test]
async fn selection_preserves_master_override_and_provider_boundary() {
    let (mut session, mut context) = make_session_and_context().await;
    assert!(session.enabled(Feature::RemoteCompactionV2));
    for eligible in [false, true] {
        context.provider.name = if eligible { "OpenAI" } else { "Other" }.into();
        context.provider.base_url = Some("http://127.0.0.1/custom/backend-api/codex".into());
        for master in [false, true] {
            for v2 in [false, true] {
                if master {
                    session.features.enable(Feature::RemoteCompaction);
                } else {
                    session.features.disable(Feature::RemoteCompaction);
                }
                if v2 {
                    session.features.enable(Feature::RemoteCompactionV2);
                } else {
                    session.features.disable(Feature::RemoteCompactionV2);
                }
                let expected = if !eligible || !master {
                    CompactionImplementation::Local
                } else if v2 {
                    CompactionImplementation::RemoteV2
                } else {
                    CompactionImplementation::Remote
                };
                assert_eq!(
                    compaction_implementation(&session, &context.provider),
                    expected
                );
            }
        }
    }
}
