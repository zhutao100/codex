use super::*;
use crate::compact::CompactionImplementation;
use crate::compact::compaction_implementation;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn compaction_notes_preserve_multipart_text_and_source_model_after_replay() {
    let source_model = "source-\"model\\name";
    let request: ResponseItem = DeveloperInstructions::new(format!(
        "{}\n{}{}",
        compact::AUTO_COMPACT_WORK_NOTES_REQUEST_TAG,
        compact::WORK_NOTES_SOURCE_MODEL_PREFIX,
        serde_json::to_string(source_model).unwrap()
    ))
    .into();
    let mut message = assistant_message("");
    let ResponseItem::Message { content, .. } = &mut message else {
        unreachable!()
    };
    *content = vec![
        ContentItem::OutputText {
            text: "<AUTO_COMPACT_WORK_".into(),
        },
        ContentItem::InputText {
            text: "not assistant output".into(),
        },
        ContentItem::OutputText {
            text: "NOTES>\n`let val".into(),
        },
        ContentItem::OutputText {
            text: "ue = 1;`\n<proposed_plan>literal notes</proposed_plan>".into(),
        },
    ];
    let notes =
        "<AUTO_COMPACT_WORK_NOTES>\n`let value = 1;`\n<proposed_plan>literal notes</proposed_plan>";
    assert_eq!(
        crate::stream_events_utils::last_assistant_message_from_item(&message, false),
        Some(notes.into())
    );
    let items = vec![user_message("task"), request, message];
    let expected = compact::PendingWorkNotes {
        request_index: 1,
        capture_end: 3,
        source_model: Some(source_model.into()),
        notes: Some(notes.into()),
        attempts: 1,
    };
    let (session, _) = make_session_and_context().await;
    let rollout: Vec<_> = items
        .iter()
        .cloned()
        .map(RolloutItem::ResponseItem)
        .collect();
    session
        .record_initial_history(InitialHistory::Resumed(ResumedHistory {
            conversation_id: session.conversation_id,
            rollout_path: PathBuf::from("/tmp/multipart-compaction.jsonl"),
            history: serde_json::from_str(&serde_json::to_string(&rollout).unwrap()).unwrap(),
        }))
        .await;
    assert_eq!(session.clone_history().await.raw_items(), items);
    assert_eq!(
        compact::pending_work_notes(session.clone_history().await.raw_items()).unwrap(),
        Some(expected)
    );
    let prepared =
        compact::prepare_history_for_compaction(session.clone_history().await, None).unwrap();
    assert_eq!(prepared.source_history.raw_items(), &[user_message("task")]);
    assert_eq!(prepared.preserved_work_notes, Some(notes.into()));
}

#[test]
fn compaction_notes_reject_malformed_explicit_source_models() {
    for model in ["unquoted", "null", "42", "\"unterminated"] {
        let request = DeveloperInstructions::new(format!(
            "{}\n{}{model}",
            compact::AUTO_COMPACT_WORK_NOTES_REQUEST_TAG,
            compact::WORK_NOTES_SOURCE_MODEL_PREFIX,
        ))
        .into();
        assert!(matches!(
            compact::pending_work_notes(&[request]),
            Err(CodexErr::Fatal(_))
        ));
    }
}

#[test]
fn compaction_notes_accept_only_matching_rejected_capture_outputs() {
    let notes = "<AUTO_COMPACT_WORK_NOTES>\nexact notes";
    let request: ResponseItem =
        DeveloperInstructions::new(compact::AUTO_COMPACT_WORK_NOTES_REQUEST_TAG).into();
    let reason = compact::WORK_NOTES_TOOL_REJECT_REASON;
    for (call, output) in [
        (
            ResponseItem::FunctionCall {
                id: None,
                call_id: "capture".into(),
                name: "update_plan".into(),
                arguments: r#"{"plan":[]}"#.into(),
            },
            ResponseItem::FunctionCallOutput {
                call_id: "capture".into(),
                output: FunctionCallOutputPayload {
                    body: FunctionCallOutputBody::Text(reason.into()),
                    success: Some(false),
                },
            },
        ),
        (
            ResponseItem::FunctionCall {
                id: None,
                call_id: "capture".into(),
                name: "mcp__server__tool".into(),
                arguments: "{}".into(),
            },
            ResponseInputItem::McpToolCallOutput {
                call_id: "capture".into(),
                result: Err(reason.into()),
            }
            .into(),
        ),
        (
            ResponseItem::CustomToolCall {
                id: None,
                call_id: "capture".into(),
                name: "apply_patch".into(),
                input: "unexecuted patch".into(),
                status: None,
            },
            ResponseItem::CustomToolCallOutput {
                call_id: "capture".into(),
                output: reason.into(),
            },
        ),
    ] {
        for output_after_notes in [false, true] {
            let mut items = vec![user_message("task"), request.clone(), call.clone()];
            let tail = if output_after_notes {
                vec![assistant_message(notes), output.clone()]
            } else {
                vec![output.clone(), assistant_message(notes)]
            };
            items.extend(tail);
            let mut history = ContextManager::new();
            history.replace(items.clone());
            let prepared = compact::prepare_history_for_compaction(history, None).unwrap();
            assert_eq!(prepared.source_history.raw_items(), &[user_message("task")]);
            assert_eq!(prepared.preserved_work_notes, Some(notes.into()));
            // The same rejection without its matching capture call is
            // unrecognized work and must not be silently discarded.
            items.remove(2);
            assert!(compact::pending_work_notes(&items).is_err());
        }
    }
}

#[tokio::test]
async fn continuation_cleanup_preserves_notes_before_rejected_outputs_are_recorded() {
    let call = ResponseItem::FunctionCall {
        id: None,
        call_id: "capture".into(),
        name: "update_plan".into(),
        arguments: "{}".into(),
    };
    let capture = vec![
        user_message("task"),
        DeveloperInstructions::new(compact::AUTO_COMPACT_WORK_NOTES_REQUEST_TAG).into(),
        call,
        assistant_message("<AUTO_COMPACT_WORK_NOTES>\nexact notes"),
    ];
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("rollout.jsonl");
    for later_work in [false, true] {
        let mut items = capture.clone();
        if later_work {
            items.extend([
                user_message("later task"),
                ResponseItem::FunctionCall {
                    id: None,
                    call_id: "actual-work".into(),
                    name: "shell".into(),
                    arguments: "{}".into(),
                },
            ]);
        }
        let lines = items
            .iter()
            .map(|item| {
                serde_json::to_string(&codex_protocol::protocol::RolloutLine {
                    timestamp: "2026-09-07T00:00:00Z".into(),
                    item: RolloutItem::ResponseItem(item.clone()),
                })
                .unwrap()
            })
            .collect::<Vec<_>>();
        tokio::fs::write(&path, format!("{}\n", lines.join("\n")))
            .await
            .unwrap();
        assert_eq!(trim_incomplete_continuation_tail(&mut items), later_work);
        let mut expected = capture.clone();
        if later_work {
            expected.push(user_message("later task"));
        }
        assert_eq!(items, expected);
        assert_eq!(
            RolloutRecorder::clean_for_continue(&path, false)
                .await
                .unwrap(),
            later_work
        );
        let remaining = tokio::fs::read_to_string(&path).await.unwrap();
        assert_eq!(remaining, format!("{}\n", lines[..items.len()].join("\n")));
        assert_eq!(
            compact::pending_work_notes(&items).unwrap().unwrap().notes,
            Some("<AUTO_COMPACT_WORK_NOTES>\nexact notes".into())
        );
    }
}

#[tokio::test]
async fn compaction_usage_counts_preserved_notes_once_after_installation_and_restart() {
    let (session, context) = make_session_and_context().await;
    let notes = compact::preserved_work_notes_message(&"long exact notes\n".repeat(2_000));
    let replacement = vec![
        user_message("original task"),
        ResponseItem::Compaction {
            encrypted_content: "summary".into(),
        },
        notes,
    ];
    let compacted = CompactedItem {
        message: String::new(),
        replacement_history: Some(replacement.clone()),
    };
    let reference = context.to_turn_context_item();
    session
        .replace_compacted_history(
            &context,
            replacement.clone(),
            Some(reference.clone()),
            compacted.clone(),
        )
        .await;
    let expected = session
        .clone_history()
        .await
        .estimate_token_count_with_base_instructions(&session.get_base_instructions().await)
        .unwrap();
    assert_eq!(session.get_total_token_usage().await, expected);
    let info = session.state.lock().await.token_info();
    let (resumed, resumed_context) = make_session_and_context().await;
    resumed
        .record_initial_history(InitialHistory::Resumed(ResumedHistory {
            conversation_id: ThreadId::default(),
            rollout_path: PathBuf::from("/tmp/compaction-usage.jsonl"),
            history: vec![
                RolloutItem::Compacted(compacted),
                RolloutItem::TurnContext(reference),
                RolloutItem::EventMsg(EventMsg::TokenCount(TokenCountEvent {
                    info,
                    rate_limits: None,
                })),
            ],
        }))
        .await;
    assert_eq!(resumed.clone_history().await.raw_items(), replacement);
    assert_eq!(resumed.get_total_token_usage().await, expected);
    let next = user_message("new input");
    resumed
        .record_into_history(std::slice::from_ref(&next), &resumed_context)
        .await;
    assert_eq!(
        resumed.get_total_token_usage().await,
        expected + crate::context_manager::estimate_item_token_count(&next)
    );
}

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
    let mut source = baseline.clone();
    source.model = "larger-source-model".into();
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
                    RolloutItem::TurnContext(source.clone()),
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
                            model: source.model.clone()
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
            capture_end: 3,
            source_model: None,
            notes: Some(notes.into()),
            attempts: 1
        }
    );
    let mut source = ContextManager::new();
    source.replace(history.clone());
    let prepared = compact::prepare_history_for_compaction(source, None).unwrap();
    assert_eq!(prepared.source_history.raw_items(), &history[..1]);
    assert_eq!(prepared.preserved_work_notes, Some(notes.into()));
    let mut abandoned = history.clone();
    let later = user_message("later genuine work");
    abandoned.push(later.clone());
    let mut source = ContextManager::new();
    source.replace(abandoned.clone());
    let prepared = compact::prepare_history_for_compaction(source.clone(), None).unwrap();
    assert_eq!(
        prepared.source_history.raw_items(),
        &[history[0].clone(), later]
    );
    assert_eq!(prepared.preserved_work_notes, Some(notes.into()));
    assert_eq!(source.raw_items(), abandoned);

    let mut ambiguous = ContextManager::new();
    let mut items = history;
    items.push(assistant_message("unidentified later work"));
    ambiguous.replace(items);
    assert!(compact::prepare_history_for_compaction(ambiguous, None).is_err());
}

#[tokio::test]
async fn interrupted_notes_replay_and_preparation_preserve_the_known_abort_marker() {
    let (session, context) = make_session_and_context().await;
    let request: ResponseItem =
        DeveloperInstructions::new(compact::AUTO_COMPACT_WORK_NOTES_REQUEST_TAG).into();
    let notes = "<AUTO_COMPACT_WORK_NOTES>\nExact captured notes";
    let marker = user_message("<turn_aborted>\ninterrupted\n</turn_aborted>");
    for completed_notes in [false, true] {
        let mut history = vec![user_message("original task"), request.clone()];
        if completed_notes {
            history.push(assistant_message(notes));
        }
        history.push(marker.clone());
        let rollout: Vec<_> = history
            .iter()
            .cloned()
            .map(RolloutItem::ResponseItem)
            .collect();
        let reconstructed = session
            .reconstruct_history_from_rollout(&context, &rollout)
            .await;
        assert!(reconstructed.pending_continuation.is_some());
        assert_eq!(reconstructed.history, history);
        let mut source = ContextManager::new();
        source.replace(history);
        let prepared = compact::prepare_history_for_compaction(source, None).unwrap();
        assert_eq!(
            prepared.source_history.raw_items(),
            &[user_message("original task"), marker.clone()]
        );
        assert_eq!(
            prepared.preserved_work_notes,
            completed_notes.then(|| notes.to_string())
        );
    }
}

#[tokio::test]
async fn ambiguous_compaction_notes_remain_recoverable_after_standalone_replay() {
    let (session, context) = make_session_and_context().await;
    let history = vec![
        ResponseItem::Compaction {
            encrypted_content: "earlier summary".into(),
        },
        DeveloperInstructions::new(compact::AUTO_COMPACT_WORK_NOTES_REQUEST_TAG).into(),
        assistant_message("<AUTO_COMPACT_WORK_NOTES>\nnotes"),
        assistant_message("unidentified later work"),
    ];
    let rollout = vec![
        RolloutItem::Compacted(CompactedItem {
            message: String::new(),
            replacement_history: Some(history[..1].to_vec()),
        }),
        RolloutItem::ResponseItem(history[1].clone()),
        RolloutItem::ResponseItem(history[2].clone()),
        RolloutItem::ResponseItem(history[3].clone()),
    ];
    let reconstructed = session
        .reconstruct_history_from_rollout(&context, &rollout)
        .await;
    assert_eq!(reconstructed.history, history);
    assert!(reconstructed.pending_continuation.is_some());
    assert!(compact::pending_work_notes(&reconstructed.history).is_err());
}

#[tokio::test]
async fn previous_model_compaction_reports_a_missing_provider() {
    use crate::models_manager::overlay::ModelInfoPatch;
    use crate::models_manager::overlay::ModelOverlay;
    use crate::models_manager::overlay::ModelOverlayEntry;

    let (session, mut context, events) = make_session_and_context_with_rx().await;
    Arc::get_mut(&mut context)
        .unwrap()
        .model_info
        .auto_compact_token_limit = Some(100);
    {
        let mut state = session.state.lock().await;
        let config = Arc::make_mut(&mut state.session_configuration.original_config_do_not_use);
        config.model_overlay = Some(ModelOverlay {
            models: vec![ModelOverlayEntry {
                slug: "previous-model".into(),
                model_provider: Some("missing-provider".into()),
                patch: ModelInfoPatch::default(),
                final_instruction_override: None,
            }],
            ..Default::default()
        });
        state.set_token_usage_full(1_000);
    }
    session
        .set_previous_turn_settings(Some(PreviousTurnSettings {
            model: "previous-model".into(),
        }))
        .await;
    assert_eq!(
        turn::run_turn(
            session,
            context,
            vec![UserInput::Text {
                text: "next task".into(),
                text_elements: vec![],
            }],
            CancellationToken::new(),
        )
        .await,
        None
    );
    let mut errors = Vec::new();
    while let Ok(event) = events.try_recv() {
        if let EventMsg::Error(error) = event.msg {
            errors.push(error);
        }
    }
    assert_eq!(
        serde_json::to_value(errors).unwrap(),
        serde_json::to_value(vec![
            CodexErr::Fatal("Model provider `missing-provider` not found".into())
                .to_error_event(None)
        ])
        .unwrap()
    );
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
