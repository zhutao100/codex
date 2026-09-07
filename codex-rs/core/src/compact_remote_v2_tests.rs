use super::*;
use crate::context_manager::ContextManager;
use crate::rollout::policy::should_persist_response_item;
use crate::session::tests::make_session_and_context;
use crate::truncate::TruncationPolicy;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::protocol::TokenUsage;
use pretty_assertions::assert_eq;
use tokio::sync::mpsc;

fn compaction(value: &str) -> ResponseItem {
    ResponseItem::Compaction {
        encrypted_content: value.into(),
    }
}

fn stream(events: Vec<CodexResult<ResponseEvent>>) -> ResponseStream {
    let (tx, rx_event) = mpsc::channel(events.len().max(1));
    for event in events {
        tx.try_send(event).unwrap();
    }
    ResponseStream {
        rx_event,
        consumer_dropped: CancellationToken::new(),
    }
}

fn completed() -> CodexResult<ResponseEvent> {
    Ok(ResponseEvent::Completed {
        response_id: "complete".into(),
        token_usage: Some(TokenUsage {
            input_tokens: 100,
            output_tokens: 20,
            total_tokens: 120,
            ..Default::default()
        }),
        end_turn: None,
    })
}

#[tokio::test]
async fn collector_accepts_only_completed_compaction_and_never_records_extra_output() {
    let (session, context) = make_session_and_context().await;
    let extra = ResponseItem::FunctionCall {
        id: None,
        call_id: "ignored".into(),
        name: "shell".into(),
        arguments: "{}".into(),
    };
    let output = collect_compaction_output(
        &session,
        &context,
        stream(vec![
            Ok(ResponseEvent::OutputItemDone(extra)),
            Ok(ResponseEvent::OutputItemDone(compaction("summary"))),
            completed(),
        ]),
    )
    .await
    .unwrap();
    assert_eq!(
        output,
        CompactionOutput {
            item: compaction("summary"),
            response_id: "complete".into()
        }
    );
    assert_eq!(session.clone_history().await.raw_items(), &[]);
    assert_eq!(
        session
            .clone_history()
            .await
            .token_info()
            .unwrap()
            .total_token_usage
            .total_tokens,
        120
    );
}

#[tokio::test]
async fn collector_rejects_invalid_completed_cardinality_but_accounts_for_usage() {
    for count in [0, 2] {
        let (session, context) = make_session_and_context().await;
        let mut events: Vec<_> = (0..count)
            .map(|_| Ok(ResponseEvent::OutputItemDone(compaction("invalid"))))
            .collect();
        events.push(completed());
        assert!(matches!(
            collect_compaction_output(&session, &context, stream(events)).await,
            Err(CodexErr::Fatal(_))
        ));
        assert_eq!(session.clone_history().await.raw_items(), &[]);
        assert_eq!(
            session
                .clone_history()
                .await
                .token_info()
                .unwrap()
                .total_token_usage
                .total_tokens,
            120
        );
    }
}

#[tokio::test]
async fn collector_discards_partial_summary_on_eof_or_failure() {
    let (session, context) = make_session_and_context().await;
    for error in [None, Some(CodexErr::Stream("failure".into(), None))] {
        let mut events = vec![Ok(ResponseEvent::OutputItemDone(compaction("failed")))];
        if let Some(error) = error {
            events.push(Err(error));
        }
        assert!(matches!(
            collect_compaction_output(&session, &context, stream(events)).await,
            Err(CodexErr::Stream(..))
        ));
        assert_eq!(session.clone_history().await.raw_items(), &[]);
    }
    let output = collect_compaction_output(
        &session,
        &context,
        stream(vec![
            Ok(ResponseEvent::OutputItemDone(compaction("fresh"))),
            completed(),
        ]),
    )
    .await
    .unwrap();
    assert_eq!(output.item, compaction("fresh"));
}

#[test]
fn trigger_is_request_only_even_for_projected_replacement_history() {
    let trigger = ResponseItem::CompactionTrigger {};
    let mut history = ContextManager::new();
    history.record_items(
        std::slice::from_ref(&trigger),
        TruncationPolicy::Tokens(1000),
    );
    assert_eq!(history.raw_items(), &[]);
    history.replace(vec![trigger.clone()]);
    assert_eq!(history.for_prompt(), vec![]);
    assert!(!should_persist_response_item(&trigger));
    assert!(crate::event_mapping::parse_turn_item(&trigger).is_none());
}

#[test]
fn budget_reserves_all_fixed_context_and_rejects_unfittable_notes() {
    assert_eq!(retention_budget(1000, None, None).unwrap(), 64_000);
    assert_eq!(
        retention_budget(1000, Some(5000), Some(4000)).unwrap(),
        2999
    );
    assert_eq!(retention_budget(1000, Some(2000), None).unwrap(), 1000);
    assert_eq!(retention_budget(1000, Some(1000), None).unwrap(), 0);
    assert!(matches!(
        retention_budget(1000, Some(999), None),
        Err(CodexErr::Fatal(_))
    ));
}

#[test]
fn metadata_is_request_local_and_works_without_workspace_metadata() {
    for base in [None, Some(r#"{"workspace":{"head":"abc"}}"#)] {
        let metadata: serde_json::Value = serde_json::from_str(&compaction_metadata(
            base,
            CompactionReason::TokenLimit,
            InitialContextInjection::BeforeLastUserMessage,
        ))
        .unwrap();
        let mut expected = json!({
            "request_kind": "compaction",
            "compaction": {
                "trigger": "auto", "reason": "context_limit",
                "implementation": "responses_compaction_v2", "phase": "mid_turn", "strategy": "memento"
            }
        });
        if base.is_some() {
            expected["workspace"] = json!({"head":"abc"});
        }
        assert_eq!(metadata, expected);
    }
}

#[tokio::test]
async fn tail_rewrite_preserves_calls_status_and_live_source() {
    let (_, mut context) = make_session_and_context().await;
    context.model_info.context_window = Some(1000);
    let call = ResponseItem::FunctionCall {
        id: None,
        call_id: "call".into(),
        name: "shell".into(),
        arguments: "{}".into(),
    };
    let output = ResponseItem::FunctionCallOutput {
        call_id: "call".into(),
        output: FunctionCallOutputPayload {
            body: codex_protocol::models::FunctionCallOutputBody::Text("x".repeat(10_000)),
            success: Some(false),
        },
    };
    let mut history = ContextManager::new();
    history.replace(vec![call.clone(), output]);
    let source = history.raw_items().to_vec();
    let mut working = history.clone();
    let base = codex_protocol::models::BaseInstructions {
        text: String::new(),
    };
    let (count, savings) =
        trim_function_call_history_to_fit_context_window(&mut working, &context, &base);
    assert_eq!(count, 1);
    assert!(savings > 1000);
    assert_eq!(history.raw_items(), source);
    assert_eq!(
        working.raw_items(),
        &[
            call,
            ResponseItem::FunctionCallOutput {
                call_id: "call".into(),
                output: FunctionCallOutputPayload {
                    body: codex_protocol::models::FunctionCallOutputBody::Text(
                        crate::compact_remote::TRUNCATED_OUTPUT.into()
                    ),
                    success: Some(false),
                },
            },
        ]
    );
}

#[tokio::test]
async fn installation_finishes_coherently_when_the_parent_is_aborted_during_persistence() {
    let (session, context, events) =
        crate::session::tests::make_session_and_context_with_rx().await;
    let replacement = vec![
        compaction("installed"),
        preserved_work_notes_message("exact notes"),
    ];
    let expected = replacement.clone();
    let reference = context.to_turn_context_item();
    let expected_reference = reference.clone();
    // Hold the recorder lock at the real await between memory installation and
    // persistence, then simulate task-stop's forced abort of the parent.
    let recorder = session.services.rollout.lock().await;
    let install_session = Arc::clone(&session);
    let install_context = Arc::clone(&context);
    let parent = tokio::spawn(async move {
        install_compacted_history(
            &install_session,
            &install_context,
            replacement,
            Some(reference),
            TurnItem::ContextCompaction(ContextCompactionItem::new()),
            &CancellationToken::new(),
        )
        .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while session.clone_history().await.raw_items() != expected {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    parent.abort();
    assert!(parent.await.unwrap_err().is_cancelled());
    assert!(session.compaction_installation.try_lock().is_err());
    drop(recorder);
    let _installed = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        session.compaction_installation.lock(),
    )
    .await
    .unwrap();
    assert_eq!(session.clone_history().await.raw_items(), expected);
    assert_eq!(
        session.reference_context_item().await,
        Some(expected_reference)
    );
    let mut completed = 0;
    while let Ok(event) = events.try_recv() {
        if matches!(event.msg, EventMsg::ItemCompleted(_)) {
            completed += 1;
        }
    }
    assert_eq!(completed, 1);
}

#[tokio::test]
async fn cancellation_before_installation_preserves_the_source() {
    let (session, context, _) = crate::session::tests::make_session_and_context_with_rx().await;
    let token = CancellationToken::new();
    token.cancel();
    assert!(matches!(
        install_compacted_history(
            &session,
            &context,
            vec![compaction("discarded")],
            None,
            TurnItem::ContextCompaction(ContextCompactionItem::new()),
            &token,
        )
        .await,
        Err(CodexErr::TurnAborted),
    ));
    assert_eq!(session.clone_history().await.raw_items(), &[]);
}

#[test]
fn work_notes_preparation_keeps_snapshots_that_arrive_during_capture() {
    let snapshot = ResponseItem::GhostSnapshot {
        ghost_commit: codex_git::GhostCommit::new("snapshot".into(), None, vec![], vec![]),
    };
    let request: ResponseItem = codex_protocol::models::DeveloperInstructions::new(
        crate::compact::AUTO_COMPACT_WORK_NOTES_REQUEST_TAG,
    )
    .into();
    let mut history = ContextManager::new();
    history.replace(vec![request, snapshot.clone()]);
    let prepared = prepare_history_for_compaction(history, Some("notes".into())).unwrap();
    assert_eq!(prepared.source_history.raw_items(), &[snapshot]);
    assert_eq!(prepared.source_history.for_prompt(), vec![]);
    assert_eq!(prepared.preserved_work_notes, Some("notes".into()));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn snapshot_completed_during_the_compaction_stream_survives_installation() {
    core_test_support::skip_if_no_network_unless_localhost!();
    use core_test_support::responses;
    use core_test_support::streaming_sse::StreamingSseChunk;
    use core_test_support::streaming_sse::start_streaming_sse_server;
    let (release, gate) = tokio::sync::oneshot::channel();
    let (server, _) = start_streaming_sse_server(vec![vec![StreamingSseChunk {
        gate: Some(gate),
        body: responses::sse(vec![
            json!({"type":"response.output_item.done", "item":{"type":"compaction","encrypted_content":"summary"}}),
            responses::ev_completed("complete"),
        ]),
    }]]).await;
    let (session, mut context) = make_session_and_context().await;
    context.provider.base_url = Some(format!("{}/v1", server.uri()));
    let session = Arc::new(session);
    let context = Arc::new(context);
    let user = ResponseItem::Message {
        id: None,
        role: "user".into(),
        content: vec![ContentItem::InputText {
            text: "task".into(),
        }],
        end_turn: None,
        phase: None,
    };
    session
        .record_conversation_items(&context, std::slice::from_ref(&user))
        .await;
    let before = codex_git::GhostCommit::new("before-request".into(), None, vec![], vec![]);
    let during = codex_git::GhostCommit::new("during-request".into(), None, vec![], vec![]);
    session.record_ghost_snapshot(&context, &before).await;
    let task_session = Arc::clone(&session);
    let task_context = Arc::clone(&context);
    let task = tokio::spawn(async move {
        run_remote_compact_task(
            &task_session,
            &task_context,
            None,
            None,
            InitialContextInjection::DoNotInject,
            CompactionReason::TokenLimit,
            &CancellationToken::new(),
        )
        .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while server.requests().await.is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    session.record_ghost_snapshot(&context, &during).await;
    release.send(()).unwrap();
    assert!(task.await.unwrap().unwrap());
    assert_eq!(
        session.clone_history().await.raw_items(),
        &[
            user.clone(),
            compaction("summary"),
            ResponseItem::GhostSnapshot {
                ghost_commit: before
            },
            ResponseItem::GhostSnapshot {
                ghost_commit: during
            },
        ]
    );
    assert_eq!(
        session.clone_history().await.for_prompt(),
        vec![user, compaction("summary")]
    );
    server.shutdown().await;
}
