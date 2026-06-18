use codex_core::compact::SUMMARIZATION_PROMPT;
use codex_core::features::Feature;
use codex_core::protocol::EventMsg;
use codex_core::protocol::Op;
use codex_protocol::items::TurnItem;
use codex_protocol::user_input::UserInput;
use core_test_support::responses;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_completed_with_tokens;
use core_test_support::responses::ev_function_call;
use core_test_support::responses::ev_message_item_added;
use core_test_support::responses::ev_output_text_delta;
use core_test_support::responses::ev_reasoning_item;
use core_test_support::responses::ev_reasoning_item_added;
use core_test_support::responses::ev_response_created;
use core_test_support::streaming_sse::StreamingSseChunk;
use core_test_support::streaming_sse::start_streaming_sse_server;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use pretty_assertions::assert_eq;
use serde_json::Value;
use tokio::sync::oneshot;

fn ev_message_item_done(id: &str, text: &str) -> Value {
    serde_json::json!({
        "type": "response.output_item.done",
        "item": {
            "type": "message",
            "role": "assistant",
            "id": id,
            "content": [{"type": "output_text", "text": text}]
        }
    })
}

fn sse_event(event: Value) -> String {
    responses::sse(vec![event])
}

fn message_input_texts(body: &Value, role: &str) -> Vec<String> {
    body.get("input")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|item| item.get("type").and_then(Value::as_str) == Some("message"))
        .filter(|item| item.get("role").and_then(Value::as_str) == Some(role))
        .filter_map(|item| item.get("content").and_then(Value::as_array))
        .flatten()
        .filter(|span| span.get("type").and_then(Value::as_str) == Some("input_text"))
        .filter_map(|span| span.get("text").and_then(Value::as_str).map(str::to_owned))
        .collect()
}

fn request_body(requests: &[Vec<u8>], index: usize) -> Value {
    serde_json::from_slice(&requests[index])
        .unwrap_or_else(|err| panic!("parse request body: {err}"))
}

fn request_input_items(body: &Value) -> &[Value] {
    body.get("input")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .expect("request input array")
}

fn find_item_position(items: &[Value], label: &str, predicate: impl Fn(&Value) -> bool) -> usize {
    items
        .iter()
        .position(predicate)
        .unwrap_or_else(|| panic!("expected {label} in request suffix: {items:#?}"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn injected_user_input_triggers_follow_up_request_with_deltas() {
    let (gate_completed_tx, gate_completed_rx) = oneshot::channel();

    let first_chunks = vec![
        StreamingSseChunk {
            gate: None,
            body: sse_event(ev_response_created("resp-1")),
        },
        StreamingSseChunk {
            gate: None,
            body: sse_event(ev_message_item_added("msg-1", "")),
        },
        StreamingSseChunk {
            gate: None,
            body: sse_event(ev_output_text_delta("first ")),
        },
        StreamingSseChunk {
            gate: None,
            body: sse_event(ev_output_text_delta("turn")),
        },
        StreamingSseChunk {
            gate: None,
            body: sse_event(ev_message_item_done("msg-1", "first turn")),
        },
        StreamingSseChunk {
            gate: Some(gate_completed_rx),
            body: sse_event(ev_completed("resp-1")),
        },
    ];

    let second_chunks = vec![
        StreamingSseChunk {
            gate: None,
            body: sse_event(ev_response_created("resp-2")),
        },
        StreamingSseChunk {
            gate: None,
            body: sse_event(ev_completed("resp-2")),
        },
    ];

    let (server, _completions) =
        start_streaming_sse_server(vec![first_chunks, second_chunks]).await;

    let codex = test_codex()
        .with_model("gpt-5.1")
        .build_with_streaming_server(&server)
        .await
        .unwrap()
        .codex;

    codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "first prompt".into(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
        })
        .await
        .unwrap();

    wait_for_event(&codex, |event| {
        matches!(event, EventMsg::AgentMessageContentDelta(_))
    })
    .await;

    codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "second prompt".into(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
        })
        .await
        .unwrap();

    let _ = gate_completed_tx.send(());

    let _ = wait_for_event(&codex, |event| {
        matches!(event, EventMsg::UserMessage(message) if message.message == "second prompt")
    })
    .await;

    wait_for_event(&codex, |event| matches!(event, EventMsg::TurnComplete(_))).await;

    let requests = server.requests().await;
    assert_eq!(requests.len(), 2);

    let first_body: Value = serde_json::from_slice(&requests[0]).expect("parse first request");
    let second_body: Value = serde_json::from_slice(&requests[1]).expect("parse second request");

    let first_texts = message_input_texts(&first_body, "user");
    assert!(first_texts.iter().any(|text| text == "first prompt"));
    assert!(!first_texts.iter().any(|text| text == "second prompt"));

    let second_texts = message_input_texts(&second_body, "user");
    assert!(second_texts.iter().any(|text| text == "first prompt"));
    assert!(second_texts.iter().any(|text| text == "second prompt"));

    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn explicit_prompt_samples_before_pending_input() {
    let (first_completed_tx, first_completed_rx) = oneshot::channel();

    let first_chunks = vec![
        StreamingSseChunk {
            gate: None,
            body: sse_event(ev_response_created("resp-1")),
        },
        StreamingSseChunk {
            gate: Some(first_completed_rx),
            body: sse_event(ev_completed("resp-1")),
        },
    ];
    let second_chunks = vec![
        StreamingSseChunk {
            gate: None,
            body: sse_event(ev_response_created("resp-2")),
        },
        StreamingSseChunk {
            gate: None,
            body: sse_event(ev_completed("resp-2")),
        },
    ];
    let (server, _completions) =
        start_streaming_sse_server(vec![first_chunks, second_chunks]).await;

    let codex = test_codex()
        .with_model("gpt-5.1")
        .build_with_streaming_server(&server)
        .await
        .unwrap()
        .codex;

    codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "fresh prompt".into(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
        })
        .await
        .unwrap();

    wait_for_event(&codex, |event| matches!(event, EventMsg::TurnStarted(_))).await;

    codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "queued pending input".into(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
        })
        .await
        .unwrap();

    let _ = first_completed_tx.send(());
    wait_for_event(&codex, |event| matches!(event, EventMsg::TurnComplete(_))).await;

    let requests = server.requests().await;
    assert_eq!(requests.len(), 2);

    let first_body = request_body(&requests, 0);
    let first_texts = message_input_texts(&first_body, "user");
    assert!(first_texts.iter().any(|text| text == "fresh prompt"));
    assert!(
        !first_texts
            .iter()
            .any(|text| text == "queued pending input")
    );

    let second_body = request_body(&requests, 1);
    let second_texts = message_input_texts(&second_body, "user");
    assert!(second_texts.iter().any(|text| text == "fresh prompt"));
    assert!(
        second_texts
            .iter()
            .any(|text| text == "queued pending input")
    );

    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn steered_user_input_preserves_previous_request_prefix_after_reasoning_item() {
    let (gate_reasoning_done_tx, gate_reasoning_done_rx) = oneshot::channel();
    let call_id = "call-preserved-prefix";

    let first_chunks = vec![
        StreamingSseChunk {
            gate: None,
            body: sse_event(ev_response_created("resp-1")),
        },
        StreamingSseChunk {
            gate: None,
            body: sse_event(ev_reasoning_item_added("reason-1", &["thinking"])),
        },
        StreamingSseChunk {
            gate: Some(gate_reasoning_done_rx),
            body: responses::sse(vec![
                ev_reasoning_item("reason-1", &["thinking"], &[]),
                ev_function_call(call_id, "unsupported_tool", "{}"),
                ev_message_item_added("msg-1", ""),
                ev_output_text_delta("first answer"),
                ev_message_item_done("msg-1", "first answer"),
                ev_completed("resp-1"),
            ]),
        },
    ];
    let second_chunks = vec![
        StreamingSseChunk {
            gate: None,
            body: sse_event(ev_response_created("resp-2")),
        },
        StreamingSseChunk {
            gate: None,
            body: sse_event(ev_completed("resp-2")),
        },
    ];
    let (server, _completions) =
        start_streaming_sse_server(vec![first_chunks, second_chunks]).await;

    let codex = test_codex()
        .with_model("gpt-5.1")
        .build_with_streaming_server(&server)
        .await
        .unwrap()
        .codex;

    codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "first prompt".into(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
        })
        .await
        .unwrap();

    wait_for_event(&codex, |event| {
        matches!(event, EventMsg::ItemStarted(item) if matches!(&item.item, TurnItem::Reasoning(_)))
    })
    .await;

    codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "second prompt".into(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
        })
        .await
        .unwrap();

    let _ = gate_reasoning_done_tx.send(());
    wait_for_event(&codex, |event| {
        matches!(event, EventMsg::AgentMessage(message) if message.message == "first answer")
    })
    .await;
    wait_for_event(&codex, |event| matches!(event, EventMsg::TurnComplete(_))).await;

    let requests = server.requests().await;
    assert_eq!(requests.len(), 2);

    let first_body = request_body(&requests, 0);
    let second_body = request_body(&requests, 1);

    assert_eq!(first_body.get("model"), second_body.get("model"));
    assert_eq!(first_body.get("tools"), second_body.get("tools"));
    assert_eq!(
        first_body.get("tool_choice"),
        second_body.get("tool_choice")
    );

    let first_input = request_input_items(&first_body);
    let second_input = request_input_items(&second_body);
    assert!(
        second_input.len() > first_input.len(),
        "follow-up request should append response/tool/steer items"
    );
    assert_eq!(&second_input[..first_input.len()], first_input);

    let suffix = &second_input[first_input.len()..];
    let reasoning_pos = find_item_position(suffix, "reasoning item", |item| {
        item.get("type").and_then(Value::as_str) == Some("reasoning")
    });
    let function_call_pos = find_item_position(suffix, "function call", |item| {
        item.get("type").and_then(Value::as_str) == Some("function_call")
            && item.get("call_id").and_then(Value::as_str) == Some(call_id)
    });
    let assistant_pos = find_item_position(suffix, "assistant message", |item| {
        item.get("type").and_then(Value::as_str) == Some("message")
            && item.get("role").and_then(Value::as_str) == Some("assistant")
    });
    let function_output_pos = find_item_position(suffix, "function call output", |item| {
        item.get("type").and_then(Value::as_str) == Some("function_call_output")
            && item.get("call_id").and_then(Value::as_str) == Some(call_id)
    });
    let steer_pos = find_item_position(suffix, "steered user message", |item| {
        item.get("type").and_then(Value::as_str) == Some("message")
            && item.get("role").and_then(Value::as_str) == Some("user")
            && message_input_texts(&serde_json::json!({ "input": [item] }), "user")
                .iter()
                .any(|text| text == "second prompt")
    });
    assert!(
        reasoning_pos < function_call_pos
            && function_call_pos < assistant_pos
            && assistant_pos < function_output_pos
            && function_output_pos < steer_pos,
        "follow-up suffix should preserve response items before the steered user input"
    );

    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mid_turn_compaction_defers_pending_input_until_model_follow_up_finishes() {
    let (work_notes_completed_tx, work_notes_completed_rx) = oneshot::channel();
    let pending_text = "queued during work-notes capture";
    let call_id = "call-pending-compact";
    let function_name = "unsupported_tool";
    let work_notes_text =
        "<AUTO_COMPACT_WORK_NOTES>\nObjective: test pending input ordering.\nStatus: ready.";

    let first_chunks = vec![
        StreamingSseChunk {
            gate: None,
            body: sse_event(ev_response_created("resp-1")),
        },
        StreamingSseChunk {
            gate: None,
            body: sse_event(ev_function_call(call_id, function_name, "{}")),
        },
        StreamingSseChunk {
            gate: None,
            body: sse_event(ev_completed_with_tokens("resp-1", 96)),
        },
    ];
    let work_notes_chunks = vec![
        StreamingSseChunk {
            gate: None,
            body: sse_event(ev_response_created("resp-2")),
        },
        StreamingSseChunk {
            gate: None,
            body: sse_event(ev_assistant_message("msg-work-notes", work_notes_text)),
        },
        StreamingSseChunk {
            gate: Some(work_notes_completed_rx),
            body: sse_event(ev_completed_with_tokens("resp-2", 10)),
        },
    ];
    let compact_chunks = vec![
        StreamingSseChunk {
            gate: None,
            body: sse_event(ev_response_created("resp-3")),
        },
        StreamingSseChunk {
            gate: None,
            body: sse_event(ev_assistant_message("msg-summary", "COMPACTED SUMMARY")),
        },
        StreamingSseChunk {
            gate: None,
            body: sse_event(ev_completed_with_tokens("resp-3", 10)),
        },
    ];
    let model_follow_up_chunks = vec![
        StreamingSseChunk {
            gate: None,
            body: sse_event(ev_response_created("resp-4")),
        },
        StreamingSseChunk {
            gate: None,
            body: sse_event(ev_assistant_message("msg-follow-up", "follow-up done")),
        },
        StreamingSseChunk {
            gate: None,
            body: sse_event(ev_completed_with_tokens("resp-4", 10)),
        },
    ];
    let pending_chunks = vec![
        StreamingSseChunk {
            gate: None,
            body: sse_event(ev_response_created("resp-5")),
        },
        StreamingSseChunk {
            gate: None,
            body: sse_event(ev_completed("resp-5")),
        },
    ];
    let (server, _completions) = start_streaming_sse_server(vec![
        first_chunks,
        work_notes_chunks,
        compact_chunks,
        model_follow_up_chunks,
        pending_chunks,
    ])
    .await;

    let codex = test_codex()
        .with_model("gpt-5.1")
        .with_config(|config| {
            config.compact_prompt = Some(SUMMARIZATION_PROMPT.to_string());
            config.model_context_window = Some(100);
            config.model_auto_compact_token_limit = Some(90);
            config.features.disable(Feature::RemoteCompaction);
        })
        .build_with_streaming_server(&server)
        .await
        .unwrap()
        .codex;

    codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "trigger tool and compaction".into(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
        })
        .await
        .unwrap();

    wait_for_event(&codex, |event| {
        matches!(event, EventMsg::AgentMessage(message) if message.message == work_notes_text)
    })
    .await;

    codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: pending_text.into(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
        })
        .await
        .unwrap();

    let _ = work_notes_completed_tx.send(());
    wait_for_event(&codex, |event| matches!(event, EventMsg::TurnComplete(_))).await;

    let requests = server.requests().await;
    assert_eq!(requests.len(), 5);

    let bodies = (0..requests.len())
        .map(|index| request_body(&requests, index))
        .collect::<Vec<_>>();
    assert!(
        message_input_texts(&bodies[2], "user")
            .iter()
            .any(|text| text == SUMMARIZATION_PROMPT),
        "expected third request to be compaction"
    );

    for (index, body) in bodies.iter().take(4).enumerate() {
        let user_texts = message_input_texts(body, "user");
        assert!(
            !user_texts.iter().any(|text| text == pending_text),
            "request {index} should not include pending input before model follow-up finishes"
        );
    }

    let pending_request_texts = message_input_texts(&bodies[4], "user");
    assert!(
        pending_request_texts
            .iter()
            .any(|text| text == pending_text),
        "pending input should be sampled after the model follow-up"
    );

    server.shutdown().await;
}
