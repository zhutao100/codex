#![allow(clippy::expect_used, clippy::unwrap_used)]

#[path = "compact_remote_v2_recovery.rs"]
mod recovery;

use anyhow::Result;
use codex_core::CodexAuth;
use codex_core::features::Feature;
use codex_core::protocol::EventMsg;
use codex_core::protocol::Op;
use codex_core::protocol::RolloutItem;
use codex_core::protocol::RolloutLine;
use codex_protocol::items::TurnItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::user_input::UserInput;
use core_test_support::responses;
use core_test_support::responses::WebSocketConnectionConfig;
use core_test_support::skip_if_no_network_unless_localhost;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use std::time::Duration;
use wiremock::ResponseTemplate;

const NOTES: &str = "<AUTO_COMPACT_WORK_NOTES>\nObjective: keep ✅ exact\n  `code`\nRuled out: retrying the old route\nNext: finish";

fn ev_compaction(value: &str) -> Value {
    json!({"type":"response.output_item.done", "item": {"type":"compaction", "encrypted_content":value}})
}

fn compact_response(value: &str) -> String {
    responses::sse(vec![
        ev_compaction(value),
        responses::ev_completed("compact"),
    ])
}

fn user_input(text: &str) -> Op {
    Op::UserInput {
        items: vec![UserInput::Text {
            text: text.into(),
            text_elements: Vec::new(),
        }],
        final_output_json_schema: None,
    }
}

async fn complete(codex: &codex_core::CodexThread) {
    wait_for_event(codex, |event| match event {
        EventMsg::Error(error) => panic!("unexpected error: {}", error.message),
        EventMsg::TurnComplete(_) => true,
        _ => false,
    })
    .await;
}

fn assert_compact_request(body: &Value) {
    assert_eq!(
        body["input"].as_array().unwrap().last(),
        Some(&json!({"type":"compaction_trigger"}))
    );
    assert_eq!(
        body["input"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|item| item["type"] == "compaction_trigger")
            .count(),
        1
    );
    assert!(!body["tools"].as_array().unwrap().is_empty());
    assert!(body.pointer("/text/format").is_none());
}

fn replacement_history(path: &std::path::Path) -> Vec<Vec<ResponseItem>> {
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<RolloutLine>(line).unwrap())
        .filter_map(|line| match line.item {
            RolloutItem::Compacted(item) => item.replacement_history,
            _ => None,
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn manual_uses_responses_and_preserves_settings_and_exact_replay() -> Result<()> {
    skip_if_no_network_unless_localhost!(Ok(()));
    for chatgpt in [false, true] {
        let server = responses::start_mock_server().await;
        let base = format!("{}/backend-api/codex", server.uri());
        let mock = responses::mount_sse_sequence(
            &server,
            vec![
                responses::sse(vec![
                    responses::ev_assistant_message("work", "original work"),
                    responses::ev_completed("work"),
                ]),
                responses::sse(vec![
                    responses::ev_shell_command_call(
                        "never-executed",
                        "printf unexpected > unexpected.txt",
                    ),
                    ev_compaction("encrypted"),
                    responses::ev_completed("compact"),
                ]),
                responses::sse(vec![
                    responses::ev_assistant_message("followup", "done"),
                    responses::ev_completed("followup"),
                ]),
            ],
        )
        .await;
        let auth = if chatgpt {
            CodexAuth::create_dummy_chatgpt_auth_for_testing()
        } else {
            CodexAuth::from_api_key("fixture")
        };
        let test = test_codex()
            .with_auth(auth)
            .with_config(move |config| {
                config.model_provider.base_url = Some(base);
                config.service_tier = Some("priority".into());
            })
            .build(&server)
            .await?;
        test.codex.submit(user_input("original user")).await?;
        complete(&test.codex).await;
        test.codex.submit(Op::Compact).await?;
        let mut started = None;
        let mut completed = None;
        wait_for_event(&test.codex, |event| {
            match event {
                EventMsg::ItemStarted(event)
                    if matches!(event.item, TurnItem::ContextCompaction(_)) =>
                {
                    started = Some(serde_json::to_value(&event.item).unwrap())
                }
                EventMsg::ItemCompleted(event)
                    if matches!(event.item, TurnItem::ContextCompaction(_)) =>
                {
                    completed = Some(serde_json::to_value(&event.item).unwrap())
                }
                EventMsg::Error(error) => panic!("{}", error.message),
                _ => {}
            }
            matches!(event, EventMsg::TurnComplete(_))
        })
        .await;
        assert!(started.is_some());
        assert_eq!(started, completed);
        test.codex.submit(user_input("follow up")).await?;
        complete(&test.codex).await;
        assert!(!test.cwd_path().join("unexpected.txt").exists());
        let requests = mock.requests();
        assert_eq!(requests.len(), 3);
        assert_eq!(requests[1].path(), "/backend-api/codex/responses");
        let compact = requests[1].body_json();
        assert_compact_request(&compact);
        let first = requests[0].body_json();
        for field in [
            "tools",
            "instructions",
            "model",
            "reasoning",
            "parallel_tool_calls",
            "prompt_cache_key",
            "service_tier",
        ] {
            assert_eq!(&compact[field], &first[field], "{field}");
        }
        assert!(
            requests[1]
                .header("x-codex-beta-features")
                .unwrap()
                .split(',')
                .any(|flag| flag == "remote_compaction_v2")
        );
        let metadata: Value =
            serde_json::from_str(&requests[1].header("x-codex-turn-metadata").unwrap())?;
        assert_eq!(metadata["request_kind"], "compaction");
        assert_eq!(metadata["compaction"]["phase"], "standalone_turn");
        let followup = requests[2].body_json();
        assert!(followup.get("previous_response_id").is_none());
        assert!(!followup.to_string().contains("compaction_trigger"));
        assert!(!followup.to_string().contains("original work"));
        assert!(!followup.to_string().contains("never-executed"));
        assert!(followup.to_string().contains("encrypted"));
        let rollout = test
            .session_configured
            .rollout_path
            .as_ref()
            .unwrap()
            .clone();
        test.codex.submit(Op::Shutdown).await?;
        wait_for_event(&test.codex, |event| {
            matches!(event, EventMsg::ShutdownComplete)
        })
        .await;
        let replacements = replacement_history(&rollout);
        assert_eq!(replacements.len(), 1);
        assert_eq!(
            replacements[0].last(),
            Some(&ResponseItem::Compaction {
                encrypted_content: "encrypted".into()
            })
        );
        assert_eq!(
            serde_json::to_value(&replacements[0])?,
            json!([
                {"type":"message","role":"user","content":[{"type":"input_text","text":"original user"}]},
                {"type":"compaction","encrypted_content":"encrypted"}
            ])
        );
        assert!(
            server
                .received_requests()
                .await
                .unwrap()
                .iter()
                .all(|request| !request.url.path().ends_with("/compact"))
        );
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mid_turn_notes_are_carried_exactly_and_transient_suffix_is_excluded() -> Result<()> {
    skip_if_no_network_unless_localhost!(Ok(()));
    let server = responses::start_mock_server().await;
    let mock = responses::mount_sse_sequence(
        &server,
        vec![
            responses::sse(vec![
                responses::ev_shell_command_call("work", "printf work"),
                responses::ev_completed_with_tokens("work", 1_000_000),
            ]),
            responses::sse(vec![
                responses::ev_assistant_message("notes", NOTES),
                responses::ev_completed("notes"),
            ]),
            compact_response("summary"),
            responses::sse(vec![
                responses::ev_assistant_message("done", "completed work"),
                responses::ev_completed("done"),
            ]),
        ],
    )
    .await;
    let test = test_codex().build(&server).await?;
    test.codex.submit(user_input("finish the task")).await?;
    complete(&test.codex).await;
    let requests = mock.requests();
    assert_eq!(requests.len(), 4);
    let compact = requests[2].body_json();
    assert_compact_request(&compact);
    assert!(!compact.to_string().contains("AUTO_COMPACT_WORK_NOTES"));
    assert!(compact.to_string().contains("function_call_output"));
    assert_eq!(compact["tools"], requests[1].body_json()["tools"]);
    let followup = requests[3].body_json();
    let input = followup["input"].as_array().unwrap();
    let notes = input.last().unwrap()["content"][0]["text"]
        .as_str()
        .unwrap();
    assert!(notes.ends_with(NOTES));
    assert_eq!(notes.matches(NOTES).count(), 1);
    assert_eq!(
        input[input.len() - 2],
        json!({"type":"compaction","encrypted_content":"summary"})
    );
    let rollout = test
        .session_configured
        .rollout_path
        .as_ref()
        .unwrap()
        .clone();
    test.codex.submit(Op::Shutdown).await?;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::ShutdownComplete)
    })
    .await;
    let replacements = replacement_history(&rollout);
    assert_eq!(serde_json::to_value(&replacements[0])?, followup["input"]);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn permanent_404_preserves_history_without_outer_retries() -> Result<()> {
    skip_if_no_network_unless_localhost!(Ok(()));
    let server = responses::start_mock_server().await;
    let first = responses::mount_sse_once(
        &server,
        responses::sse(vec![
            responses::ev_assistant_message("first", "keep this work"),
            responses::ev_completed("first"),
        ]),
    )
    .await;
    let test = test_codex()
        .with_config(|config| {
            config.model_provider.request_max_retries = Some(0);
            config.model_provider.stream_max_retries = Some(5);
        })
        .build(&server)
        .await?;
    test.codex.submit(user_input("user task")).await?;
    complete(&test.codex).await;
    assert_eq!(first.requests().len(), 1);
    let failure = responses::mount_response_once(
        &server,
        ResponseTemplate::new(404).set_body_json(json!({"detail":"Not Found"})),
    )
    .await;
    test.codex.submit(Op::Compact).await?;
    wait_for_event(&test.codex, |event| matches!(event, EventMsg::Error(_))).await;
    complete(&test.codex).await;
    assert_eq!(failure.requests().len(), 1);
    let followup = responses::mount_sse_once(
        &server,
        responses::sse(vec![
            responses::ev_assistant_message("next", "done"),
            responses::ev_completed("next"),
        ]),
    )
    .await;
    test.codex.submit(user_input("try ordinary work")).await?;
    complete(&test.codex).await;
    assert!(
        followup
            .single_request()
            .body_json()
            .to_string()
            .contains("keep this work")
    );
    assert!(replacement_history(test.session_configured.rollout_path.as_ref().unwrap()).is_empty());
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retries_discard_partial_summary_and_honor_zero_budget() -> Result<()> {
    skip_if_no_network_unless_localhost!(Ok(()));
    for retry_budget in [0, 1] {
        let server = responses::start_mock_server().await;
        let mut bodies = vec![responses::sse(vec![ev_compaction("discarded")])];
        if retry_budget > 0 {
            bodies.push(compact_response("accepted"));
        }
        let mock = responses::mount_sse_sequence(&server, bodies).await;
        let test = test_codex()
            .with_config(move |config| {
                config.model_provider.request_max_retries = Some(0);
                config.model_provider.stream_max_retries = Some(retry_budget);
            })
            .build(&server)
            .await?;
        test.codex.submit(Op::Compact).await?;
        if retry_budget == 0 {
            wait_for_event(&test.codex, |event| matches!(event, EventMsg::Error(_))).await;
        }
        complete(&test.codex).await;
        assert_eq!(mock.requests().len(), 1 + retry_budget as usize);
        let replacements =
            replacement_history(test.session_configured.rollout_path.as_ref().unwrap());
        assert_eq!(replacements.len(), retry_budget as usize);
        assert!(!serde_json::to_string(&replacements)?.contains("discarded"));
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn websocket_notes_compact_followup_reuses_socket_with_fresh_history() -> Result<()> {
    skip_if_no_network_unless_localhost!(Ok(()));
    let server = responses::start_websocket_server(vec![vec![
        vec![
            responses::ev_shell_command_call("work", "printf work"),
            responses::ev_completed_with_tokens("work", 1_000_000),
        ],
        vec![
            responses::ev_assistant_message("notes", NOTES),
            responses::ev_completed("notes"),
        ],
        vec![ev_compaction("summary"), responses::ev_completed("compact")],
        vec![
            responses::ev_assistant_message("done", "done"),
            responses::ev_completed("done"),
        ],
    ]])
    .await;
    let test = test_codex().build_with_websocket_server(&server).await?;
    test.codex.submit(user_input("do work")).await?;
    complete(&test.codex).await;
    assert_eq!(server.handshakes().len(), 1);
    let requests = server.single_connection();
    assert_eq!(requests.len(), 4);
    let compact = requests[2].body_json();
    assert_compact_request(&compact);
    assert!(compact.get("previous_response_id").is_none());
    assert!(!compact.to_string().contains("AUTO_COMPACT_WORK_NOTES"));
    let followup = requests[3].body_json();
    assert!(followup.get("previous_response_id").is_none());
    assert!(followup.to_string().contains("summary"));
    assert!(!followup.to_string().contains("compaction_trigger"));
    server.shutdown().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pause_during_compact_recovers_completed_notes_on_a_fresh_socket() -> Result<()> {
    skip_if_no_network_unless_localhost!(Ok(()));
    let server = responses::start_websocket_server_with_headers(vec![
        WebSocketConnectionConfig {
            requests: vec![
                vec![
                    responses::ev_shell_command_call("work", "printf work"),
                    responses::ev_completed_with_tokens("work", 1_000_000),
                ],
                vec![
                    responses::ev_assistant_message("notes", NOTES),
                    responses::ev_completed("notes"),
                ],
                vec![
                    responses::ev_response_created("partial"),
                    ev_compaction("discarded"),
                ],
            ],
            response_headers: vec![],
            keep_open: true,
        },
        WebSocketConnectionConfig {
            requests: vec![
                vec![
                    ev_compaction("recovered"),
                    responses::ev_completed("compact"),
                ],
                vec![
                    responses::ev_assistant_message("done", "finished"),
                    responses::ev_completed("done"),
                ],
            ],
            response_headers: vec![],
            keep_open: true,
        },
    ])
    .await;
    let test = test_codex().build_with_websocket_server(&server).await?;
    test.codex.submit(user_input("unfinished task")).await?;
    tokio::time::timeout(Duration::from_secs(10), async {
        while server
            .connections()
            .first()
            .is_none_or(|requests| requests.len() < 3)
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await?;
    test.codex.submit(Op::Pause).await?;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnPaused(_))
    })
    .await;
    assert!(replacement_history(test.session_configured.rollout_path.as_ref().unwrap()).is_empty());
    test.codex.submit(Op::Continue).await?;
    complete(&test.codex).await;
    assert_eq!(server.handshakes().len(), 2);
    let connections = server.connections();
    assert_eq!(
        connections.iter().map(Vec::len).collect::<Vec<_>>(),
        vec![3, 2]
    );
    let retried = connections[1][0].body_json();
    assert_compact_request(&retried);
    assert!(retried.get("previous_response_id").is_none());
    assert!(!retried.to_string().contains("AUTO_COMPACT_WORK_NOTES"));
    let followup = connections[1][1].body_json();
    assert!(followup.to_string().contains("recovered"));
    assert!(!followup.to_string().contains("discarded"));
    let notes = followup["input"].as_array().unwrap().last().unwrap()["content"][0]["text"]
        .as_str()
        .unwrap();
    assert!(notes.ends_with(NOTES));
    server.shutdown().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restart_after_compact_failure_recovers_notes_before_sampling() -> Result<()> {
    skip_if_no_network_unless_localhost!(Ok(()));
    let server = responses::start_mock_server().await;
    let initial = responses::mount_sse_sequence(
        &server,
        vec![
            responses::sse(vec![
                responses::ev_shell_command_call("work", "printf work"),
                responses::ev_completed_with_tokens("work", 1_000_000),
            ]),
            responses::sse(vec![
                responses::ev_assistant_message("notes", NOTES),
                responses::ev_completed("notes"),
            ]),
            responses::sse(vec![ev_compaction("discarded")]),
        ],
    )
    .await;
    let test = test_codex()
        .with_config(|config| {
            config.model_provider.request_max_retries = Some(0);
            config.model_provider.stream_max_retries = Some(0);
        })
        .build(&server)
        .await?;
    test.codex.submit(user_input("unfinished task")).await?;
    wait_for_event(&test.codex, |event| matches!(event, EventMsg::Error(_))).await;
    complete(&test.codex).await;
    assert_eq!(initial.requests().len(), 3);
    test.codex.submit(Op::Shutdown).await?;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::ShutdownComplete)
    })
    .await;
    let followup = responses::mount_sse_sequence(
        &server,
        vec![
            compact_response("resumed"),
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
    let requests = followup.requests();
    assert_eq!(requests.len(), 2);
    assert_compact_request(&requests[0].body_json());
    assert!(
        !requests[0]
            .body_json()
            .to_string()
            .contains("AUTO_COMPACT_WORK_NOTES")
    );
    assert!(
        requests[1].body_json()["input"]
            .as_array()
            .unwrap()
            .last()
            .unwrap()["content"][0]["text"]
            .as_str()
            .unwrap()
            .ends_with(NOTES)
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pre_turn_compact_skips_notes_for_schema_and_restores_the_schema_afterward() -> Result<()> {
    skip_if_no_network_unless_localhost!(Ok(()));
    let server = responses::start_mock_server().await;
    let mock = responses::mount_sse_sequence(
        &server,
        vec![
            responses::sse(vec![
                responses::ev_assistant_message("first", r#"{"answer":"first"}"#),
                responses::ev_completed_with_tokens("first", 1_000_000),
            ]),
            compact_response("summary"),
            responses::sse(vec![
                responses::ev_assistant_message("second", r#"{"answer":"second"}"#),
                responses::ev_completed("second"),
            ]),
        ],
    )
    .await;
    let test = test_codex().build(&server).await?;
    let schema = json!({"type":"object","properties":{"answer":{"type":"string"}},"required":["answer"],"additionalProperties":false});
    for text in ["first request", "new incoming request"] {
        test.codex
            .submit(Op::UserInput {
                items: vec![UserInput::Text {
                    text: text.into(),
                    text_elements: vec![],
                }],
                final_output_json_schema: Some(schema.clone()),
            })
            .await?;
        complete(&test.codex).await;
    }
    let requests = mock.requests();
    assert_eq!(requests.len(), 3);
    let compact = requests[1].body_json();
    assert_compact_request(&compact);
    assert!(!compact.to_string().contains("new incoming request"));
    assert!(!compact.to_string().contains("AUTO_COMPACT_WORK_NOTES"));
    assert_eq!(requests[2].body_json()["text"]["format"]["schema"], schema);
    let metadata: Value =
        serde_json::from_str(&requests[1].header("x-codex-turn-metadata").unwrap())?;
    assert_eq!(metadata["compaction"]["phase"], "pre_turn");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn measures_text_and_image_compaction_roundtrips() -> Result<()> {
    skip_if_no_network_unless_localhost!(Ok(()));
    for images in [false, true] {
        let server = responses::start_mock_server().await;
        let mock = responses::mount_sse_sequence(
            &server,
            vec![
                responses::sse(vec![
                    responses::ev_assistant_message("work", "initial reply"),
                    responses::ev_completed("work"),
                ]),
                compact_response("measured_summary"),
                responses::sse(vec![
                    responses::ev_assistant_message("followup", "done"),
                    responses::ev_completed("followup"),
                ]),
            ],
        )
        .await;
        let test = test_codex().build(&server).await?;
        let items = if images {
            (0..50)
                .map(|index| UserInput::Image {
                    image_url: format!("https://example.test/fixture-{index}.png"),
                })
                .collect()
        } else {
            vec![UserInput::Text {
                text: "representative source context\n".repeat(14_000),
                text_elements: vec![],
            }]
        };
        test.codex
            .submit(Op::UserInput {
                items,
                final_output_json_schema: None,
            })
            .await?;
        complete(&test.codex).await;
        let started = std::time::Instant::now();
        test.codex.submit(Op::Compact).await?;
        complete(&test.codex).await;
        let compact_us = started.elapsed().as_micros();
        test.codex.submit(user_input("follow up")).await?;
        complete(&test.codex).await;
        let requests = mock.requests();
        assert_eq!(requests.len(), 3);
        let followup = requests[2].body_json();
        let retained_images = followup["input"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|item| item["content"].as_array())
            .flatten()
            .filter(|item| item["type"] == "input_image")
            .count();
        if images {
            assert!(retained_images > 0 && retained_images < 50);
        } else {
            assert!(
                requests[2].body_json().to_string().len()
                    < requests[0].body_json().to_string().len()
            );
        }
        println!(
            "REMOTE_V2_MEASUREMENT {}",
            json!({
                "fixture": if images { "images" } else { "text" },
                "compact_us": compact_us,
                "initial_request_bytes": requests[0].body_json().to_string().len(),
                "compact_request_bytes": requests[1].body_json().to_string().len(),
                "followup_request_bytes": followup.to_string().len(),
                "retained_images": retained_images,
                "responses_requests": requests.len(),
            })
        );
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn master_override_runs_local_compaction_without_a_trigger() -> Result<()> {
    skip_if_no_network_unless_localhost!(Ok(()));
    let server = responses::start_mock_server().await;
    let mock = responses::mount_sse_sequence(
        &server,
        vec![
            responses::sse(vec![
                responses::ev_assistant_message("first", "original response"),
                responses::ev_completed("first"),
            ]),
            responses::sse(vec![
                responses::ev_assistant_message("summary", "LOCAL FALLBACK SUMMARY"),
                responses::ev_completed("summary"),
            ]),
            responses::sse(vec![
                responses::ev_assistant_message("followup", "done"),
                responses::ev_completed("followup"),
            ]),
        ],
    )
    .await;
    let test = test_codex()
        .with_config(|config| {
            config.features.disable(Feature::RemoteCompaction);
        })
        .build(&server)
        .await?;
    test.codex.submit(user_input("original task")).await?;
    complete(&test.codex).await;
    test.codex.submit(Op::Compact).await?;
    complete(&test.codex).await;
    test.codex.submit(user_input("follow up")).await?;
    complete(&test.codex).await;
    let requests = mock.requests();
    assert_eq!(requests.len(), 3);
    assert!(requests.iter().all(|request| {
        request.path() == "/v1/responses"
            && !request
                .body_json()
                .to_string()
                .contains("compaction_trigger")
    }));
    let followup = requests[2].body_json().to_string();
    assert!(followup.contains("LOCAL FALLBACK SUMMARY"));
    assert!(!followup.contains("original response"));
    Ok(())
}
