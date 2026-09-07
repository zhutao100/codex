use super::*;
use codex_core::models_manager::overlay::ModelInfoPatch;
use codex_core::models_manager::overlay::ModelOverlay;
use codex_core::models_manager::overlay::ModelOverlayEntry;
use core_test_support::streaming_sse::StreamingSseChunk;
use core_test_support::streaming_sse::StreamingSseServer;
use core_test_support::streaming_sse::start_streaming_sse_server;
use core_test_support::test_codex::TestCodex;
use pretty_assertions::assert_eq;
use tokio::sync::oneshot;

fn chunk(events: Vec<Value>) -> StreamingSseChunk {
    StreamingSseChunk {
        gate: None,
        body: responses::sse(events),
    }
}

fn work_and_notes() -> Vec<Vec<StreamingSseChunk>> {
    vec![
        vec![chunk(vec![
            responses::ev_function_call("work", "unsupported_tool", "{}"),
            responses::ev_completed_with_tokens("work", 1_000_000),
        ])],
        vec![chunk(vec![
            responses::ev_function_call("notes-tool", "update_plan", r#"{"plan":[]}"#),
            responses::ev_assistant_message("notes", NOTES),
            responses::ev_completed("notes"),
        ])],
    ]
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn notes_with_rejected_tools_complete_compaction_without_retry() -> Result<()> {
    skip_if_no_network_unless_localhost!(Ok(()));
    let mut streams = work_and_notes();
    streams.extend(successful_recovery());
    let (server, _) = start_streaming_sse_server(streams).await;
    let test = test_codex().build_with_streaming_server(&server).await?;
    test.codex.submit(user_input("task with notes")).await?;
    complete(&test.codex).await;
    let requests = server.requests().await;
    assert_eq!(requests.len(), 4);
    let compact: Value = serde_json::from_slice(&requests[2])?;
    assert_compact_request(&compact);
    assert!(!compact.to_string().contains("notes-tool"));
    assert!(!compact.to_string().contains("AUTO_COMPACT_WORK_NOTES"));
    let followup: Value = serde_json::from_slice(&requests[3])?;
    assert_eq!(
        user_messages(&followup)
            .iter()
            .filter(|text| text.ends_with(NOTES))
            .count(),
        1
    );
    server.shutdown().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn long_notes_do_not_trigger_another_pre_turn_compaction_after_restart() -> Result<()> {
    skip_if_no_network_unless_localhost!(Ok(()));
    let notes = format!("{NOTES}\n{}", "n ".repeat(40_000));
    let (_release, gate) = oneshot::channel();
    let mut streams = work_and_notes();
    streams[1] = vec![chunk(vec![
        responses::ev_assistant_message("long-notes", &notes),
        responses::ev_completed("notes"),
    ])];
    streams.push(vec![chunk(vec![
        ev_compaction("summary"),
        responses::ev_completed("compact"),
    ])]);
    streams.push(vec![
        chunk(vec![responses::ev_response_created("blocked-followup")]),
        StreamingSseChunk {
            gate: Some(gate),
            body: responses::sse(vec![responses::ev_completed("blocked-followup")]),
        },
    ]);
    let (server, _) = start_streaming_sse_server(streams).await;
    let configure = |config: &mut codex_core::config::Config| {
        config.base_instructions = Some("Answer concisely.".into());
        config.model_context_window = Some(100_000);
        config.model_auto_compact_token_limit = Some(35_000);
    };
    let test = test_codex()
        .with_config(configure)
        .build_with_streaming_server(&server)
        .await?;
    test.codex.submit(user_input("original task")).await?;
    wait_for_requests(&server, 4).await;
    test.codex.submit(Op::Interrupt).await?;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnAborted(_))
    })
    .await;
    test.codex.submit(Op::Shutdown).await?;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::ShutdownComplete)
    })
    .await;

    let resumed_server = responses::start_mock_server().await;
    let mock = responses::mount_sse_once(
        &resumed_server,
        responses::sse(vec![
            responses::ev_assistant_message("done", "finished"),
            responses::ev_completed("done"),
        ]),
    )
    .await;
    let resumed = test_codex()
        .with_config(configure)
        .resume(
            &resumed_server,
            test.home.clone(),
            test.session_configured
                .rollout_path
                .as_ref()
                .unwrap()
                .clone(),
        )
        .await?;
    resumed
        .codex
        .submit(user_input("fresh input after restart"))
        .await?;
    complete(&resumed.codex).await;
    let body = mock.single_request().body_json();
    assert!(!body.to_string().contains("AUTO_COMPACT_WORK_NOTES_REQUEST"));
    assert!(!body.to_string().contains("compaction_trigger"));
    assert_eq!(
        user_messages(&body)
            .iter()
            .filter(|text| text.ends_with(&notes))
            .count(),
        1
    );
    server.shutdown().await;
    Ok(())
}

fn successful_recovery() -> Vec<Vec<StreamingSseChunk>> {
    vec![
        vec![chunk(vec![
            ev_compaction("recovered"),
            responses::ev_completed("compact"),
        ])],
        vec![chunk(vec![
            responses::ev_assistant_message("done", "finished"),
            responses::ev_completed("done"),
        ])],
    ]
}

async fn wait_for_compaction(codex: &codex_core::CodexThread) {
    wait_for_event(codex, |event| {
        matches!(event, EventMsg::ItemStarted(event) if matches!(event.item, TurnItem::ContextCompaction(_)))
    }).await;
}

async fn wait_for_requests(server: &StreamingSseServer, count: usize) {
    tokio::time::timeout(Duration::from_secs(10), async {
        while server.requests().await.len() < count {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap();
}

fn user_messages(body: &Value) -> Vec<String> {
    body["input"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|item| item["role"] == "user")
        .flat_map(|item| item["content"].as_array().unwrap())
        .filter_map(|item| item["text"].as_str().map(str::to_string))
        .collect()
}

async fn restart_and_continue(test: &TestCodex) -> Result<Vec<Value>> {
    test.codex.submit(Op::Shutdown).await?;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::ShutdownComplete)
    })
    .await;
    let server = responses::start_mock_server().await;
    let mock = responses::mount_sse_sequence(
        &server,
        vec![
            compact_response("recovered"),
            responses::sse(vec![
                responses::ev_assistant_message("done", "finished"),
                responses::ev_completed("done"),
            ]),
        ],
    )
    .await;
    let resumed = test_codex()
        .resume(
            &server,
            test.home.clone(),
            test.session_configured
                .rollout_path
                .as_ref()
                .unwrap()
                .clone(),
        )
        .await?;
    resumed.codex.submit(Op::Continue).await?;
    complete(&resumed.codex).await;
    Ok(mock
        .requests()
        .iter()
        .map(responses::ResponsesRequest::body_json)
        .collect())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_compaction_can_continue_immediately() -> Result<()> {
    assert_failed_compaction_recovers(false, false).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_compaction_preserves_acknowledged_steering_for_continue() -> Result<()> {
    assert_failed_compaction_recovers(false, true).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_compaction_preserves_acknowledged_steering_across_restart() -> Result<()> {
    assert_failed_compaction_recovers(true, true).await
}

async fn assert_failed_compaction_recovers(restart: bool, steer: bool) -> Result<()> {
    skip_if_no_network_unless_localhost!(Ok(()));
    let (release, gate) = oneshot::channel();
    let mut streams = work_and_notes();
    streams.push(vec![StreamingSseChunk {
        gate: Some(gate),
        body: responses::sse(vec![ev_compaction("discarded")]),
    }]);
    if !restart {
        streams.extend(successful_recovery());
    }
    let (server, _) = start_streaming_sse_server(streams).await;
    let test = test_codex()
        .with_config(|config| {
            config.model_provider.request_max_retries = Some(0);
            config.model_provider.stream_max_retries = Some(0);
        })
        .build_with_streaming_server(&server)
        .await?;
    let turn_id = test.codex.submit(user_input("unfinished task")).await?;
    wait_for_compaction(&test.codex).await;
    wait_for_requests(&server, 3).await;
    if steer {
        test.codex
            .submit(Op::SteerInput {
                expected_turn_id: turn_id,
                items: vec![UserInput::Text {
                    text: "accepted steering after failure".into(),
                    text_elements: vec![],
                }],
                client_user_message_id: Some("queued-steer".into()),
            })
            .await?;
        // This read-only operation follows steering in the submission queue.
        test.codex
            .submit(Op::GetHistoryEntryRequest {
                offset: 0,
                log_id: 0,
            })
            .await?;
        wait_for_event(&test.codex, |event| {
            if let EventMsg::Error(error) = event {
                panic!("{}", error.message);
            }
            matches!(event, EventMsg::GetHistoryEntryResponse(_))
        })
        .await;
    }
    release.send(()).unwrap();
    wait_for_event(&test.codex, |event| matches!(event, EventMsg::Error(_))).await;
    if steer {
        wait_for_event(&test.codex, |event| {
            matches!(event, EventMsg::UserMessage(event)
                if event.message == "accepted steering after failure"
                    && event.client_user_message_id.as_deref() == Some("queued-steer"))
        })
        .await;
    }
    complete(&test.codex).await;

    let bodies: Vec<Value> = if restart {
        restart_and_continue(&test).await?
    } else {
        test.codex.submit(Op::Continue).await?;
        complete(&test.codex).await;
        let requests = server.requests().await;
        assert_eq!(requests.len(), 5);
        requests[3..]
            .iter()
            .map(|body| serde_json::from_slice(body).unwrap())
            .collect()
    };
    assert_eq!(bodies.len(), 2);
    assert_compact_request(&bodies[0]);
    assert!(!bodies[0].to_string().contains("AUTO_COMPACT_WORK_NOTES"));
    assert!(!bodies[1].to_string().contains("discarded"));
    let texts = user_messages(&bodies[1]);
    assert_eq!(
        texts
            .iter()
            .filter(|text| *text == "accepted steering after failure")
            .count(),
        usize::from(steer)
    );
    assert_eq!(texts.iter().filter(|text| text.ends_with(NOTES)).count(), 1);
    server.shutdown().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interrupt_after_notes_allows_new_user_input() -> Result<()> {
    assert_interrupted_notes_recover(false).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interrupt_after_notes_can_restart_and_continue() -> Result<()> {
    assert_interrupted_notes_recover(true).await
}

async fn assert_interrupted_notes_recover(restart: bool) -> Result<()> {
    skip_if_no_network_unless_localhost!(Ok(()));
    let (_release, gate) = oneshot::channel();
    let mut streams = work_and_notes();
    streams.push(vec![
        chunk(vec![responses::ev_response_created("interrupted")]),
        StreamingSseChunk {
            gate: Some(gate),
            body: compact_response("discarded"),
        },
    ]);
    if !restart {
        streams.extend(successful_recovery());
    }
    let (server, _) = start_streaming_sse_server(streams).await;
    let test = test_codex().build_with_streaming_server(&server).await?;
    test.codex.submit(user_input("unfinished task")).await?;
    wait_for_compaction(&test.codex).await;
    wait_for_requests(&server, 3).await;
    test.codex.submit(Op::Interrupt).await?;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnAborted(_))
    })
    .await;
    assert!(replacement_history(test.session_configured.rollout_path.as_ref().unwrap()).is_empty());
    let bodies: Vec<Value> = if restart {
        restart_and_continue(&test).await?
    } else {
        test.codex
            .submit(user_input("new request after interruption"))
            .await?;
        complete(&test.codex).await;
        let requests = server.requests().await;
        assert_eq!(requests.len(), 5);
        requests[3..]
            .iter()
            .map(|body| serde_json::from_slice(body).unwrap())
            .collect()
    };
    assert_eq!(bodies.len(), 2);
    assert_compact_request(&bodies[0]);
    assert!(!bodies[0].to_string().contains("AUTO_COMPACT_WORK_NOTES"));
    assert!(!bodies[1].to_string().contains("discarded"));
    assert_eq!(
        user_messages(&bodies[1])
            .iter()
            .filter(|text| text.ends_with(NOTES))
            .count(),
        1
    );
    if !restart {
        assert_eq!(
            user_messages(&bodies[1])
                .iter()
                .filter(|text| *text == "new request after interruption")
                .count(),
            1
        );
    }
    server.shutdown().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn model_downshift_v2_uses_the_previous_provider_for_notes_and_compaction() -> Result<()> {
    assert_previous_model_instructions(false).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn model_downshift_v2_respects_each_models_final_instruction_override() -> Result<()> {
    assert_previous_model_instructions(true).await
}

async fn assert_previous_model_instructions(final_override: bool) -> Result<()> {
    skip_if_no_network_unless_localhost!(Ok(()));
    let old_server = responses::start_mock_server().await;
    let new_server = responses::start_mock_server().await;
    let old_requests = responses::mount_sse_sequence(
        &old_server,
        vec![
            responses::sse(vec![
                responses::ev_assistant_message("old", "old model work"),
                responses::ev_completed_with_tokens("old", 50_000),
            ]),
            responses::sse(vec![
                responses::ev_assistant_message("notes", NOTES),
                responses::ev_completed("notes"),
            ]),
            compact_response("previous-provider-summary"),
        ],
    )
    .await;
    let new_request = responses::mount_sse_once(
        &new_server,
        responses::sse(vec![
            responses::ev_assistant_message("new", "new model work"),
            responses::ev_completed("new"),
        ]),
    )
    .await;
    let new_base = format!("{}/v1", new_server.uri());
    let test = test_codex()
        .with_model("previous-provider-model")
        .with_config(move |config| {
            config.base_instructions = None;
            config
                .model_providers
                .insert("previous".into(), config.model_provider.clone());
            let mut new_provider = config.model_provider.clone();
            new_provider.base_url = Some(new_base);
            config
                .model_providers
                .insert("destination".into(), new_provider);
            config.model_overlay = Some(ModelOverlay {
                models: vec![
                    ModelOverlayEntry {
                        slug: "previous-provider-model".into(),
                        model_provider: Some("previous".into()),
                        patch: ModelInfoPatch {
                            base_instructions: Some("Previous model base instructions.".into()),
                            context_window: Some(Some(200_000)),
                            auto_compact_token_limit: Some(Some(180_000)),
                            ..Default::default()
                        },
                        final_instruction_override: final_override
                            .then(|| "Previous model final override.".into()),
                    },
                    ModelOverlayEntry {
                        slug: "destination-provider-model".into(),
                        model_provider: Some("destination".into()),
                        patch: ModelInfoPatch {
                            base_instructions: Some("Destination model base instructions.".into()),
                            context_window: Some(Some(100_000)),
                            auto_compact_token_limit: Some(Some(40_000)),
                            ..Default::default()
                        },
                        final_instruction_override: final_override
                            .then(|| "Destination model final override.".into()),
                    },
                ],
                ..Default::default()
            });
        })
        .build(&old_server)
        .await?;
    test.codex.submit(user_input("old provider task")).await?;
    complete(&test.codex).await;
    test.codex
        .submit(Op::OverrideTurnContext {
            cwd: None,
            approval_policy: None,
            sandbox_policy: None,
            windows_sandbox_level: None,
            model: Some("destination-provider-model".into()),
            effort: None,
            summary: None,
            collaboration_mode: None,
            personality: None,
            service_tier: None,
        })
        .await?;
    test.codex.submit(user_input("new provider task")).await?;
    complete(&test.codex).await;
    let old = old_requests.requests();
    assert_eq!(old.len(), 3);
    for request in &old {
        assert_eq!(request.body_json()["model"], "previous-provider-model");
        assert_eq!(
            request.body_json()["instructions"],
            if final_override {
                "Previous model final override."
            } else {
                "Previous model base instructions."
            }
        );
    }
    assert_compact_request(&old[2].body_json());
    assert!(
        !old[2]
            .body_json()
            .to_string()
            .contains("AUTO_COMPACT_WORK_NOTES")
    );
    assert!(
        !user_messages(&old[2].body_json())
            .iter()
            .any(|text| text == "new provider task")
    );
    let metadata: Value = serde_json::from_str(&old[2].header("x-codex-turn-metadata").unwrap())?;
    assert_eq!(metadata["compaction"]["reason"], "model_downshift");
    let new = new_request.single_request().body_json();
    assert_eq!(new["model"], "destination-provider-model");
    assert_eq!(
        new["instructions"],
        if final_override {
            "Destination model final override."
        } else {
            "Destination model base instructions."
        }
    );
    assert!(new.to_string().contains("previous-provider-summary"));
    assert_eq!(
        user_messages(&new)
            .iter()
            .filter(|text| *text == "new provider task")
            .count(),
        1
    );
    Ok(())
}
