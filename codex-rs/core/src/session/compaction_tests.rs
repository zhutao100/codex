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
            annotations: None,
            logprobs: None,
            text: "<AUTO_COMPACT_WORK_".into(),
        },
        ContentItem::InputText {
            text: "not assistant output".into(),
        },
        ContentItem::OutputText {
            annotations: None,
            logprobs: None,
            text: "NOTES>\n`let val".into(),
        },
        ContentItem::OutputText {
            annotations: None,
            logprobs: None,
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
        legacy_attempts: 1,
        missing_rejections: vec![],
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
        compact::pending_work_notes(session.clone_history().await.raw_items(), None).unwrap(),
        Some(expected)
    );
    let prepared =
        compact::prepare_history_for_compaction(session.clone_history().await, None, None, None)
            .unwrap();
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
            compact::pending_work_notes(&[request], None),
            Err(CodexErr::Fatal(_))
        ));
    }
}

#[test_case::test_case(compact::WORK_NOTES_TOOL_REJECT_REASON; "capture denial")]
#[test_case::test_case(crate::tools::parallel::INCOMPLETE_TOOL_BATCH_REASON; "legacy stream failure")]
fn compaction_notes_accept_only_matching_rejected_capture_outputs(reason: &str) {
    let notes = "<AUTO_COMPACT_WORK_NOTES>\nexact notes";
    let request: ResponseItem =
        DeveloperInstructions::new(compact::AUTO_COMPACT_WORK_NOTES_REQUEST_TAG).into();
    for (call, output) in [
        (
            ResponseItem::FunctionCall {
                encrypted_content: None,
                status: None,
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
                encrypted_content: None,
                status: None,
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
                encrypted_content: None,
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
            let prepared =
                compact::prepare_history_for_compaction(history, None, None, None).unwrap();
            assert_eq!(prepared.source_history.raw_items(), &[user_message("task")]);
            assert_eq!(prepared.preserved_work_notes, Some(notes.into()));
            // The same rejection without its matching capture call is
            // unrecognized work and must not be silently discarded.
            items.remove(2);
            assert!(compact::pending_work_notes(&items, None).is_err());
        }
    }
}

#[test]
fn journaled_notes_count_responses_without_widening_legacy_capture_boundaries() {
    let notes = "<AUTO_COMPACT_WORK_NOTES>\nexact";
    let items = [
        DeveloperInstructions::new(compact::AUTO_COMPACT_WORK_NOTES_REQUEST_TAG).into(),
        assistant_message("commentary"),
        assistant_message("final without a tag"),
        assistant_message(notes),
    ];
    assert!(compact::pending_work_notes(&items, None).is_err());
    assert_eq!(
        compact::pending_work_notes(
            &items,
            Some(&crate::completion_review::CaptureJournal {
                notes_remaining: Some(0),
                ..Default::default()
            })
        )
        .unwrap()
        .unwrap()
        .notes
        .as_deref(),
        Some(notes)
    );
}

#[tokio::test]
async fn another_accepted_prompt_does_not_reopen_completed_owed_work() {
    let (_, ctx) = make_session_and_context().await;
    let settings = RegularTaskSettings::from_turn(&ctx);
    let mut sampled = settings.clone();
    sampled.recorded_base = Some(ThreadId::new());
    let input = |s: &str| {
        vec![UserInput::Text {
            text: s.into(),
            text_elements: vec![],
        }]
    };
    for actual in [None, Some(&sampled)] {
        let mut review = crate::completion_review::CompletionReviewState {
            owed_settings: Some(settings.clone()),
            recovery_settings: Some(settings.clone()),
            ..Default::default()
        };
        review.accept_input(false, input("older steer"), None);
        review.release_owed_response(actual);
        review.accept_input(true, input("next task"), None);
        assert_eq!(
            (review.owed_settings, review.completed_owed_source),
            (None, Some(actual.unwrap_or(&settings).clone()))
        );
    }
}

#[tokio::test]
async fn live_continuation_discounts_discarded_sampled_calls_only_for_helper_fit() {
    let (session, context) = make_session_and_context().await;
    let call = |id: &str, n| ResponseItem::FunctionCall {
        encrypted_content: None,
        status: None,
        id: None,
        name: "read_file".into(),
        call_id: id.into(),
        arguments: " ".repeat(n),
    };
    let a = call("A", 16);
    let user = user_message("work");
    session
        .record_conversation_items(
            &context,
            &[
                user.clone(),
                a.clone(),
                call("B", 12_000),
                call("C", 12_000),
            ],
        )
        .await;
    let receipt = TokenUsage {
        input_tokens: 26_000,
        output_tokens: 5_600,
        total_tokens: 31_600,
        ..Default::default()
    };
    session
        .update_token_usage_info_with_request_context(
            &context,
            Some(&receipt),
            0,
            true,
            1,
            &crate::Prompt::default(),
        )
        .await;
    let result = ResponseItem::FunctionCallOutput {
        call_id: "A".into(),
        output: codex_protocol::models::FunctionCallOutputPayload::from_text("x".repeat(8_000)),
    };
    session
        .record_conversation_items(&context, std::slice::from_ref(&result))
        .await;
    session.prepare_history_for_continuation(false).await;
    let state = session.state.lock().await;
    assert_eq!(state.history.raw_items(), &[user, a, result.clone()]);
    assert_eq!(state.token_info().unwrap().last_token_usage, receipt);
    let new_result = crate::context_manager::estimate_item_token_count(&result);
    assert_eq!(
        state.helper_tokens_from_usage(
            &context.model_info.slug,
            crate::context_manager::HelperProjection::plain(true)
        ),
        Some(receipt.input_tokens + new_result)
    );
    assert_eq!(
        state.history.work_usage_without_frames(true),
        receipt.total_tokens + new_result
    );
}

#[test]
fn a_tagged_capture_claim_cannot_discard_a_different_call_at_the_same_position() {
    use crate::completion_review::CaptureJournal;
    let call = |value: &str| ResponseItem::CustomToolCall {
        encrypted_content: None,
        status: None,
        id: None,
        call_id: "same-id".into(),
        name: "capture".into(),
        input: value.into(),
    };
    let mut journal = CaptureJournal {
        notes_remaining: Some(1),
        ..Default::default()
    };
    journal
        .tagged_calls
        .push(crate::completion_review::TaggedCaptureCall::from_item(1, &call("owned")).unwrap());
    let mut items = vec![
        DeveloperInstructions::new(compact::AUTO_COMPACT_WORK_NOTES_REQUEST_TAG).into(),
        assistant_message("<AUTO_COMPACT_WORK_NOTES>\nexact"),
        call("owned"),
    ];
    let pending = compact::pending_work_notes(&items, Some(&journal))
        .unwrap()
        .unwrap();
    assert_eq!(
        pending.missing_rejections,
        vec![ResponseItem::CustomToolCallOutput {
            call_id: "same-id".into(),
            output: compact::WORK_NOTES_TOOL_REJECT_REASON.into()
        }]
    );
    items[2] = call("unrelated");
    assert!(compact::pending_work_notes(&items, Some(&journal)).is_err());
    items.insert(2, user_message("new work"));
    assert_eq!(
        compact::pending_work_notes(&items, Some(&journal))
            .unwrap()
            .unwrap()
            .capture_end,
        2
    );
}

#[tokio::test]
async fn continuation_cleanup_preserves_notes_before_rejected_outputs_are_recorded() {
    let call = ResponseItem::FunctionCall {
        encrypted_content: None,
        status: None,
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
                    encrypted_content: None,
                    status: None,
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
            compact::pending_work_notes(&items, None)
                .unwrap()
                .unwrap()
                .notes,
            Some("<AUTO_COMPACT_WORK_NOTES>\nexact notes".into())
        );
    }
}

#[tokio::test]
async fn compaction_usage_counts_preserved_notes_once_after_installation_and_restart() {
    let (session, context) = make_session_and_context().await;
    session
        .update_token_usage_info_with_request_context(
            &context,
            Some(&TokenUsage {
                input_tokens: 11_200,
                total_tokens: 11_200,
                ..Default::default()
            }),
            5_000,
            false,
            0,
            &crate::Prompt::default(),
        )
        .await;
    let notes = compact::preserved_work_notes_message(&"long exact notes\n".repeat(2_000));
    let replacement = vec![
        user_message("original task"),
        ResponseItem::Compaction {
            encrypted_content: "summary".into(),
        },
        notes,
    ];
    let compacted = CompactedItem {
        checkpoint: None,
        message: String::new(),
        replacement_history: Some(replacement.clone()),
    };
    let reference = context.to_turn_context_item();
    session
        .replace_compacted_history(
            &context,
            replacement.clone(),
            Some(reference.clone()),
            String::new(),
        )
        .await;
    let expected = session
        .clone_history()
        .await
        .estimate_token_count_with_base_instructions(&session.get_base_instructions().await)
        .unwrap();
    assert_eq!(session.get_total_token_usage().await, expected);
    assert_eq!(session.clone_history().await.request_context_tokens(), 0);
    let info = session.state.lock().await.token_info().unwrap();
    let (resumed, resumed_context) = make_session_and_context().await;
    resumed
        .record_initial_history(InitialHistory::Resumed(ResumedHistory {
            conversation_id: ThreadId::default(),
            rollout_path: PathBuf::from("/tmp/compaction-usage.jsonl"),
            history: vec![
                RolloutItem::Compacted(compacted),
                RolloutItem::TurnContext(reference),
                RolloutItem::TokenUsageContext {
                    info: info.clone(),
                    source_model: None,
                    request_context_tokens: 0,
                    server_reasoning_included: true,
                    last_usage_item_count: Some(replacement.len()),
                    measured_item_tokens: None,
                    measured_format_delta: None,
                    measured_base_tokens: None,
                    measured_tool_tokens: None,
                },
                RolloutItem::EventMsg(EventMsg::TokenCount(TokenCountEvent {
                    info: Some(info),
                    rate_limits: None,
                })),
            ],
        }))
        .await;
    assert_eq!(resumed.clone_history().await.raw_items(), replacement);
    assert_eq!(resumed.get_total_token_usage().await, expected);
    assert_eq!(resumed.clone_history().await.request_context_tokens(), 0);
    assert!(resumed.state.lock().await.server_reasoning_included());
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
async fn continued_work_measurement_is_not_owned_by_the_previous_user_turn() {
    for owner in [None, Some("model-b")] {
        let (session, context) = make_session_and_context().await;
        let a = context.to_turn_context_item();
        let mut b = a.clone();
        b.model = "model-b".into();
        let usage = TokenUsage {
            input_tokens: 600,
            total_tokens: 600,
            ..Default::default()
        };
        let history = vec![
            RolloutItem::TurnContext(a.clone()),
            RolloutItem::ResponseItem(user_message("start on A")),
            RolloutItem::EventMsg(EventMsg::TurnContinued(TurnContinuedEvent {
                continued_from_turn_id: Some("A".into()),
                source: TurnContinuationSource::Paused,
            })),
            RolloutItem::TurnContext(b.clone()),
            RolloutItem::ResponseItem(assistant_message("B continued the work")),
            RolloutItem::TokenUsageContext {
                info: TokenUsageInfo::new_or_append(&None, &Some(usage), None).unwrap(),
                source_model: owner.map(str::to_owned),
                request_context_tokens: 50,
                server_reasoning_included: false,
                last_usage_item_count: Some(2),
                measured_item_tokens: None,
                measured_format_delta: None,
                measured_base_tokens: None,
                measured_tool_tokens: None,
            },
            RolloutItem::ResponseItem(
                DeveloperInstructions::new(compact::AUTO_COMPACT_WORK_NOTES_REQUEST_TAG).into(),
            ),
        ];
        session
            .record_initial_history(InitialHistory::Resumed(ResumedHistory {
                conversation_id: ThreadId::default(),
                rollout_path: PathBuf::from("continued-work.jsonl"),
                history,
            }))
            .await;
        let state = session.state.lock().await;
        assert_eq!(state.work_source_model(), Some(owner.unwrap_or(&a.model)));
        assert_eq!(
            state.helper_tokens_from_usage(
                &a.model,
                crate::context_manager::HelperProjection::plain(true)
            ),
            None
        );
        assert_eq!(state.history.work_notes_baseline_at(2, &a.model), None);
        assert_eq!(
            state
                .history
                .work_notes_baseline_at(2, &b.model)
                .map(|s| s.work_tokens(
                    &BaseInstructions::default(),
                    crate::context_manager::HelperProjection::plain(true)
                )),
            owner.map(|_| 550)
        );
    }
}

#[tokio::test]
async fn stable_compaction_feature_is_advertised_without_displacing_other_flags() {
    let home = tempfile::tempdir().unwrap();
    let mut config = build_test_config(home.path()).await;
    config.features.enable(Feature::Apps);
    config.features.enable(Feature::Collab);
    let header = Session::build_model_client_beta_features_header(&config).unwrap();
    assert!(header.split(',').any(|key| key == "remote_compaction_v2"));
    assert!(header.split(',').any(|key| key == "apps"));
    assert!(header.split(',').any(|key| key == "collab"));
    config.compaction_mode = crate::config::types::CompactionMode::Unary;
    assert_eq!(
        Session::build_model_client_beta_features_header(&config),
        Some(header)
    );
}

#[tokio::test]
async fn inline_destination_survives_ordinary_task_ownership_without_pending_input() {
    let (session, context) = make_session_and_context().await;
    let mut source = context.to_turn_context_item();
    source.model = "larger-source-model".into();
    let destination = context.to_turn_context_item();
    for selected in [None, Some("explicit-recovery-model")] {
        let mut owner = crate::completion_review::CompletionReviewState {
            main_task_id: Some("original-task".into()),
            recovery_model: selected.map(str::to_owned),
            ..Default::default()
        };
        owner
            .request_frame
            .current_user_messages
            .push("original task".into());
        let rollout = [
            RolloutItem::TurnContext(source.clone()),
            RolloutItem::ResponseItem(user_message("original task")),
            RolloutItem::PostTurnCompletionReviewState(serde_json::to_value(owner).unwrap()),
            RolloutItem::Compacted(CompactedItem {
                checkpoint: None,
                message: String::new(),
                replacement_history: Some(vec![ResponseItem::Compaction {
                    encrypted_content: "installed".into(),
                }]),
            }),
            RolloutItem::TurnContext(destination.clone()),
        ];
        assert_eq!(
            session
                .reconstruct_history_from_rollout(&context, &rollout)
                .await
                .pending_continuation,
            Some(PendingContinuation {
                source: TurnContinuationSource::Interrupted,
                continued_from_turn_id: Some("original-task".into()),
                model: Some(selected.unwrap_or(&destination.model).into()),
                pause_reason: None,
                target: PendingContinuationTarget::Regular,
            })
        );
    }
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
                        checkpoint: None,
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
                        token_usage: None,
                        completion_review: None,
                        history,
                        reference_context_item: inline.then(|| baseline.clone()),
                        previous_turn_settings: Some(PreviousTurnSettings {
                            collaboration_mode: source.collaboration_mode.clone(),
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
            checkpoint: None,
            message: String::new(),
            replacement_history: Some(vec![ResponseItem::Compaction {
                encrypted_content: "encrypted".into(),
            }]),
        }),
        RolloutItem::TurnContext(context.to_turn_context_item()),
        RolloutItem::ResponseItem(assistant_message("intermediate")),
        RolloutItem::ResponseItem(ResponseItem::FunctionCall {
            encrypted_content: None,
            status: None,
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
async fn completed_work_notes_with_response_tail_remain_pending_work_after_restart() {
    let (session, context) = make_session_and_context().await;
    let request: ResponseItem = DeveloperInstructions::new(format!(
        "{}\nnotes",
        compact::AUTO_COMPACT_WORK_NOTES_REQUEST_TAG
    ))
    .into();
    let notes = "<AUTO_COMPACT_WORK_NOTES>\n  Unicode ✅\nexact\n</AUTO_COMPACT_WORK_NOTES> trailing self-commentary";
    let history = vec![
        user_message("task"),
        request,
        assistant_message(notes),
        ResponseItem::Reasoning {
            id: String::new(),
            summary: Vec::new(),
            content: None,
            encrypted_content: Some("capture reasoning".into()),
        },
        assistant_message(""),
        assistant_message(" \n\t"),
    ];
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
    let pending = compact::pending_work_notes(&history, None)
        .unwrap()
        .unwrap();
    assert_eq!(
        pending,
        compact::PendingWorkNotes {
            request_index: 1,
            capture_end: history.len(),
            source_model: None,
            notes: Some(notes.into()),
            legacy_attempts: 1,
            missing_rejections: vec![],
        }
    );
    let mut source = ContextManager::new();
    source.replace(history.clone());
    let prepared = compact::prepare_history_for_compaction(source, None, None, None).unwrap();
    assert_eq!(prepared.source_history.raw_items(), &history[..1]);
    assert_eq!(prepared.preserved_work_notes, Some(notes.into()));
    let mut abandoned = history.clone();
    let later = user_message("later genuine work");
    abandoned.push(later.clone());
    let mut source = ContextManager::new();
    source.replace(abandoned.clone());
    let frame = crate::completion_review::ReviewPromptFrames(vec![
        crate::completion_review::ReviewPromptFrame::new("work frame".into(), 1, None),
    ]);
    let prepared =
        compact::prepare_history_for_compaction(source.clone(), None, Some(&frame), None).unwrap();
    assert_eq!(
        prepared.source_history.raw_items(),
        &[
            history[0].clone(),
            DeveloperInstructions::new("work frame").into(),
            later
        ]
    );
    assert_eq!(prepared.preserved_work_notes, Some(notes.into()));
    assert_eq!(source.raw_items(), abandoned);

    for later_work in [
        assistant_message("unidentified later work"),
        ResponseItem::Message {
            encrypted_content: None,
            status: None,
            id: None,
            role: "assistant".into(),
            content: vec![ContentItem::InputImage {
                image_url: "data:image/png;base64,image".into(),
            }],
            end_turn: None,
            phase: None,
        },
        ResponseItem::Message {
            encrypted_content: None,
            status: None,
            id: None,
            role: "assistant".into(),
            content: vec![ContentItem::InputText {
                text: String::new(),
            }],
            end_turn: None,
            phase: None,
        },
        ResponseItem::FunctionCall {
            encrypted_content: None,
            status: None,
            id: None,
            call_id: "later-work".into(),
            name: "update_plan".into(),
            arguments: "{}".into(),
        },
    ] {
        let mut ambiguous = ContextManager::new();
        let mut items = history.clone();
        items.push(later_work);
        ambiguous.replace(items.clone());
        for journal in [
            None,
            Some(crate::completion_review::CaptureJournal {
                notes_remaining: Some(1),
                ..Default::default()
            }),
        ] {
            assert!(
                compact::prepare_history_for_compaction(
                    ambiguous.clone(),
                    None,
                    None,
                    journal.as_ref()
                )
                .is_err()
            );
        }
        assert_eq!(ambiguous.raw_items(), items);
    }
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
        let prepared = compact::prepare_history_for_compaction(source, None, None, None).unwrap();
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
            checkpoint: None,
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
    assert!(compact::pending_work_notes(&reconstructed.history, None).is_err());
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
            collaboration_mode: None,
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
        encrypted_content: None,
        status: None,
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
            encrypted_content: None,
            status: None,
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

#[test]
fn continuation_keeps_completed_pairs_on_either_side_of_an_unmatched_ordinary_call() {
    use serde_json::json;
    let item = |v| serde_json::from_value::<ResponseItem>(v).unwrap();
    let user = user_message("run the batch");
    let first = item(json!({"type":"function_call","name":"a","call_id":"first","arguments":"{}"}));
    let dangling =
        item(json!({"type":"function_call","name":"b","call_id":"shared","arguments":"{}"}));
    let custom =
        item(json!({"type":"custom_tool_call","name":"c","call_id":"shared","input":"test"}));
    let shell = item(
        json!({"type":"local_shell_call","call_id":"local","status":"completed","action":{"type":"exec","command":["pwd"]}}),
    );
    let result = |id: &str, text: &str| {
        item(json!({"type":"function_call_output","call_id":id,"output":text}))
    };
    let custom_out = item(
        json!({"type":"custom_tool_call_output","call_id":"shared","output":"actual custom result"}),
    );
    let shell_out = result("local", "actual directory");
    let first_out = result("first", "actual first result");
    let mut history = vec![
        user.clone(),
        first.clone(),
        dangling,
        custom.clone(),
        shell.clone(),
        custom_out.clone(),
        shell_out.clone(),
        first_out.clone(),
    ];
    assert!(trim_incomplete_continuation_tail(&mut history));
    assert_eq!(
        history,
        vec![user, first, custom, shell, custom_out, shell_out, first_out]
    );
    assert!(!trim_incomplete_continuation_tail(&mut history));
}

#[tokio::test]
async fn selection_respects_provider_and_explicit_compaction_mode() {
    use crate::config::types::CompactionMode;
    let (_, mut context) = make_session_and_context().await;
    for eligible in [false, true] {
        context.provider.name = if eligible { "OpenAI" } else { "Other" }.into();
        for (mode, supported) in [
            (CompactionMode::Auto, CompactionImplementation::RemoteV2),
            (CompactionMode::Unary, CompactionImplementation::Remote),
            (CompactionMode::Local, CompactionImplementation::Local),
        ] {
            Arc::make_mut(&mut context.config).compaction_mode = mode;
            assert_eq!(
                compaction_implementation(&context),
                if eligible {
                    supported
                } else {
                    CompactionImplementation::Local
                }
            );
        }
    }
}
