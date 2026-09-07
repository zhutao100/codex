use super::*;
use crate::context_manager::ContextManager;
use crate::rollout::policy::should_persist_response_item;
use crate::rollout::recorder::RolloutRecorder;
use crate::rollout::recorder::RolloutRecorderParams;
use crate::session::tests::make_session_and_context;
use crate::state::TaskKind;
use crate::tasks::SessionTask;
use crate::tasks::SessionTaskContext;
use crate::truncate::TruncationPolicy;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::protocol::RolloutItem;
use codex_protocol::protocol::RolloutLine;
use codex_protocol::protocol::TokenUsage;
use codex_protocol::protocol::TurnAbortReason;
use codex_protocol::protocol::TurnPauseReason;
use codex_protocol::user_input::UserInput;
use pretty_assertions::assert_eq;
use tokio::sync::Notify;
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

struct InstallingTask {
    started: Arc<Notify>,
    install: Arc<Notify>,
    dropped: Arc<Notify>,
    replacement: Vec<ResponseItem>,
}

struct NotifyOnDrop(Arc<Notify>);

impl Drop for NotifyOnDrop {
    fn drop(&mut self) {
        self.0.notify_one();
    }
}

impl SessionTask for InstallingTask {
    fn kind(&self) -> TaskKind {
        TaskKind::Regular
    }

    async fn run(
        self: Arc<Self>,
        session: Arc<SessionTaskContext>,
        context: Arc<TurnContext>,
        _input: Vec<UserInput>,
        cancellation_token: CancellationToken,
    ) -> Option<String> {
        let _dropped = NotifyOnDrop(Arc::clone(&self.dropped));
        let session = session.clone_session();
        let item = TurnItem::ContextCompaction(ContextCompactionItem::new());
        session.emit_turn_item_started(&context, &item).await;
        self.started.notify_one();
        self.install.notified().await;
        install_compacted_history(
            &session,
            &context,
            self.replacement.clone(),
            Some(context.to_turn_context_item()),
            item,
            &cancellation_token,
        )
        .await
        .unwrap();
        std::future::pending().await
    }
}

#[test_case::test_case(true; "pause")]
#[test_case::test_case(false; "interrupt")]
#[tokio::test]
async fn installation_persists_before_task_stop_and_replays_exactly(pause: bool) {
    let (session, context, events) =
        crate::session::tests::make_session_and_context_with_rx().await;
    let home = tempfile::tempdir().unwrap();
    let mut config = context.config.as_ref().clone();
    config.codex_home = home.path().into();
    let recorder = RolloutRecorder::new(
        &config,
        RolloutRecorderParams::Create {
            conversation_id: session.conversation_id,
            forked_from_id: None,
            source: context.session_source.clone(),
            model_provider_id: context.model_provider_id.clone(),
            base_instructions: session.get_base_instructions().await,
            dynamic_tools: vec![],
        },
        None,
        None,
    )
    .await
    .unwrap();
    let path = recorder.rollout_path.clone();
    *session.services.rollout.lock().await = Some(recorder);
    let user = ResponseItem::Message {
        id: None,
        role: "user".into(),
        content: vec![ContentItem::InputText {
            text: "task".into(),
        }],
        end_turn: None,
        phase: None,
    };
    session.record_conversation_items(&context, &[user]).await;
    let snapshot = codex_git::GhostCommit::new("snapshot".into(), None, vec![], vec![]);
    session.record_ghost_snapshot(&context, &snapshot).await;
    session.flush_rollout().await;
    let replacement = vec![
        compaction("installed"),
        preserved_work_notes_message("exact notes"),
    ];
    let mut expected = replacement.clone();
    expected.push(ResponseItem::GhostSnapshot {
        ghost_commit: snapshot,
    });
    let reference = context.to_turn_context_item();
    let started = Arc::new(Notify::new());
    let install = Arc::new(Notify::new());
    let dropped = Arc::new(Notify::new());
    session
        .spawn_task(
            Arc::clone(&context),
            vec![],
            InstallingTask {
                started: Arc::clone(&started),
                install: Arc::clone(&install),
                dropped: Arc::clone(&dropped),
                replacement,
            },
        )
        .await;
    let timeout = std::time::Duration::from_secs(10);
    tokio::time::timeout(timeout, started.notified())
        .await
        .unwrap();
    // Block the real recorder at the await after memory installation.
    let recorder_gate = session.services.rollout.lock().await;
    install.notify_one();
    tokio::time::timeout(timeout, async {
        while session.clone_history().await.raw_items() != expected {
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap();
    let stop_session = Arc::clone(&session);
    let stopped = tokio::spawn(async move {
        if pause {
            stop_session
                .pause_all_tasks(TurnPauseReason::UserRequested)
                .await;
        } else {
            stop_session
                .abort_all_tasks(TurnAbortReason::Interrupted)
                .await;
        }
    });
    // The actual task-stop grace timeout aborts the parent, leaving its child
    // installer alive and holding the boundary until persistence completes.
    tokio::time::timeout(timeout, dropped.notified())
        .await
        .unwrap();
    assert!(!stopped.is_finished());
    assert!(session.compaction_installation.try_lock().is_err());
    let mut lifecycle = Vec::new();
    while let Ok(event) = events.try_recv() {
        assert!(!matches!(
            event.msg,
            EventMsg::TurnPaused(_) | EventMsg::TurnAborted(_)
        ));
        if matches!(event.msg, EventMsg::ItemStarted(_)) {
            lifecycle.push("started");
        }
    }
    drop(recorder_gate);
    tokio::time::timeout(timeout, stopped)
        .await
        .unwrap()
        .unwrap();

    // No test-side flush: task-stop must make the replacement and reference
    // visible on disk before reporting that the turn stopped.
    let rollout: Vec<_> = tokio::fs::read_to_string(&path)
        .await
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<RolloutLine>(line).unwrap().item)
        .collect();
    let checkpoints: Vec<_> = rollout
        .windows(2)
        .filter_map(|items| {
            if let [
                RolloutItem::Compacted(item),
                RolloutItem::TurnContext(reference),
            ] = items
            {
                Some((item.clone(), reference.clone()))
            } else {
                None
            }
        })
        .collect();
    assert_eq!(
        serde_json::to_value(checkpoints).unwrap(),
        serde_json::to_value(vec![(
            CompactedItem {
                message: String::new(),
                replacement_history: Some(expected.clone()),
            },
            reference.clone()
        )])
        .unwrap(),
    );
    let live = session.clone_history().await;
    assert_eq!(&live.raw_items()[..expected.len()], expected);
    assert_eq!(live.raw_items().len(), expected.len() + usize::from(!pause));
    assert_eq!(session.reference_context_item().await, Some(reference));
    while let Ok(event) = events.try_recv() {
        match event.msg {
            EventMsg::ItemCompleted(_) => lifecycle.push("completed"),
            EventMsg::TurnPaused(_) => lifecycle.push("paused"),
            EventMsg::TurnAborted(_) => lifecycle.push("interrupted"),
            _ => {}
        }
    }
    assert_eq!(
        lifecycle,
        vec![
            "started",
            "completed",
            if pause { "paused" } else { "interrupted" }
        ]
    );
    let replayed = crate::session::tests::replay_rollout_history(rollout).await;
    assert_eq!(replayed.raw_items(), live.raw_items());
    assert_eq!(
        replayed.reference_context_item(),
        live.reference_context_item()
    );
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
            CompactionContext::new(&task_context),
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
