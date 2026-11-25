use anyhow::Context;
use codex_core::CodexThread;
use codex_core::compact::SUMMARIZATION_PROMPT;
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
use core_test_support::skip_if_no_network_unless_localhost;
use core_test_support::streaming_sse::StreamingSseChunk;
use core_test_support::streaming_sse::start_streaming_sse_server;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use pretty_assertions::assert_eq;
use serde_json::Value;
use std::sync::Arc;
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

async fn wait_for_queued_input(codex: &CodexThread) {
    // submit() enqueues an operation. A later read-only FIFO request proves its handler
    // finished before the fixture releases the active model stream.
    codex.submit(Op::ListCustomPrompts).await.unwrap();
    wait_for_event(codex, |event| {
        matches!(event, EventMsg::ListCustomPromptsResponse(_))
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn injected_user_input_triggers_follow_up_request_with_deltas() {
    skip_if_no_network_unless_localhost!();
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

    wait_for_queued_input(&codex).await;
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
    skip_if_no_network_unless_localhost!();
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

    wait_for_queued_input(&codex).await;
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
    skip_if_no_network_unless_localhost!();
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

    wait_for_queued_input(&codex).await;
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
async fn mid_turn_compaction_defers_pending_input_until_model_follow_up_finishes()
-> anyhow::Result<()> {
    assert_pending_input_after_compaction(false, SteerAt::Notes).await
}

#[derive(Clone, Copy)]
enum SteeringCut {
    Installed,
    OverLimitReceipt,
    BelowLimitReceipt,
    OwedResponseCrossesLimit,
}

#[test_case::test_case(SteeringCut::Installed; "installed replacement")]
#[test_case::test_case(SteeringCut::OverLimitReceipt; "over-limit receipt before decision")]
#[test_case::test_case(SteeringCut::BelowLimitReceipt; "below-limit receipt without a barrier")]
#[test_case::test_case(SteeringCut::OwedResponseCrossesLimit; "same model with new directory policy and schema")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn schema_checkpoint_defers_steering_and_keeps_jointly_admitted_apps(
    cut: SteeringCut,
) -> anyhow::Result<()> {
    use codex_protocol::protocol::RolloutItem;
    use codex_protocol::protocol::RolloutLine;
    use serde_json::json;
    skip_if_no_network_unless_localhost!(Ok(()));
    let receipt = matches!(
        cut,
        SteeringCut::OverLimitReceipt | SteeringCut::BelowLimitReceipt
    );
    let below_limit = matches!(cut, SteeringCut::BelowLimitReceipt);
    let recompact = matches!(cut, SteeringCut::OwedResponseCrossesLimit);
    let target = if recompact { "gpt-5.1" } else { "gpt-5.2" };
    const STEER: &str = "Earlier amendment.";
    const FRESH: &str = "New prompt.";
    let apps = responses::start_mock_server().await;
    responses::mount_app_catalog_with_capabilities(
        &apps,
        &["Old", "Tasks", "Calendar"],
        json!({"tools":{}, "experimental":{codex_core::MCP_SANDBOX_STATE_CAPABILITY:{}}}),
    )
    .await;
    let cwd = tempfile::tempdir()?;
    let builder = |limit| {
        let base = apps.uri();
        let cwd = cwd.path().to_owned();
        test_codex()
            .with_model("gpt-5.1")
            .with_auth(codex_core::CodexAuth::create_dummy_chatgpt_auth_for_testing())
            .with_config(move |cfg| {
                cfg.model_context_window = Some(200_000);
                cfg.model_auto_compact_token_limit = Some(limit);
                cfg.cwd = cwd;
                cfg.sandbox_policy = codex_core::config::Constrained::allow_any(
                    codex_protocol::protocol::SandboxPolicy::new_read_only_policy(),
                );
                cfg.features.enable(codex_core::features::Feature::Apps);
                cfg.features
                    .disable(codex_core::features::Feature::EnableRequestCompression);
                cfg.chatgpt_base_url = base;
            })
    };
    let prompt = |text: &str, app: &str, schema| Op::UserInput {
        items: vec![
            UserInput::Text {
                text: text.into(),
                text_elements: vec![],
            },
            UserInput::Mention {
                name: app.into(),
                path: format!("app://{app}"),
            },
        ],
        final_output_json_schema: schema,
    };
    let complete = |codex: Arc<CodexThread>| async move {
        wait_for_event(&codex, |e| match e {
            EventMsg::TurnComplete(_) => true,
            EventMsg::Error(err) => panic!("{}", err.message),
            _ => false,
        })
        .await;
    };
    let stop = |codex: Arc<CodexThread>| async move {
        codex.submit(Op::Shutdown).await?;
        wait_for_event(&codex, |e| matches!(e, EventMsg::ShutdownComplete)).await;
        anyhow::Ok(())
    };
    let assert_request = |body: &Value, amendments: &[&str], apps: &[&str]| -> anyhow::Result<()> {
        let users = message_input_texts(body, "user")
            .into_iter()
            .filter(|s| [STEER, FRESH].contains(&s.as_str()))
            .collect::<Vec<_>>();
        assert_eq!(users, amendments);
        let selected = body["tools"]
            .as_array()
            .context("model tools")?
            .iter()
            .filter_map(|t| t["name"].as_str()?.split_once("probe_").map(|(_, app)| app))
            .collect::<Vec<_>>();
        assert_eq!(selected, apps);
        Ok(())
    };
    let finish = |id: &str| {
        responses::sse(vec![
            ev_assistant_message(id, r#"{"done":true}"#),
            ev_completed(id),
        ])
    };
    let chunk = |body| StreamingSseChunk { gate: None, body };
    let (release, gate) = oneshot::channel();
    let (server, _) = start_streaming_sse_server(vec![
        vec![
            chunk(responses::sse(vec![ev_response_created("work"), ev_assistant_message("first", r#"{"done":true}"#)])),
            StreamingSseChunk { gate: Some(gate), body: sse_event(ev_completed_with_tokens("work", 100_000)) },
        ],
        vec![chunk(responses::sse(vec![json!({"type":"response.output_item.done","item":{"type":"compaction","encrypted_content":"replacement"}}), ev_completed("compact")]))],
        vec![chunk(finish("continue"))],
        vec![chunk(finish("amend"))],
    ]).await;
    let test = builder(90_000).build_with_streaming_server(&server).await?;
    let schema = json!({"type":"object","properties":{"done":{"type":"boolean"}},"additionalProperties":false,"required":["done"]});
    test.codex
        .submit(prompt("Original task.", "Old", Some(schema.clone())))
        .await?;
    wait_for_event(
        &test.codex,
        |e| matches!(e, EventMsg::AgentMessage(m) if m.message == r#"{"done":true}"#),
    )
    .await;
    test.codex.submit(prompt(STEER, "Tasks", None)).await?;
    wait_for_queued_input(&test.codex).await;
    release
        .send(())
        .map_err(|()| anyhow::anyhow!("work receipt gate closed"))?;
    complete(test.codex.clone()).await;
    let sent = server.requests().await;
    assert_eq!(sent.len(), 4);
    let bodies = (0..4).map(|i| request_body(&sent, i)).collect::<Vec<_>>();
    assert_eq!(
        bodies[1]["input"]
            .as_array()
            .context("compact input")?
            .last(),
        Some(&json!({"type":"compaction_trigger"}))
    );
    assert!(bodies[1].pointer("/text/format").is_none());
    for i in [0, 2, 3] {
        assert_eq!(bodies[i]["text"]["format"]["schema"], schema);
    }
    for body in &bodies[..3] {
        assert_request(body, &[], &["Old"])?;
    }
    assert_request(&bodies[3], &[STEER], &["Old", "Tasks"])?;
    stop(test.codex.clone()).await?;
    server.shutdown().await;
    let path = test.codex.rollout_path().context("original rollout")?;
    let truncate_after = |predicate: &dyn Fn(&RolloutItem) -> bool| -> anyhow::Result<()> {
        let raw = std::fs::read_to_string(&path)?;
        let mut end = 0;
        for line in raw.split_inclusive('\n') {
            end += line.len();
            if predicate(&serde_json::from_str::<RolloutLine>(line)?.item) {
                std::fs::write(&path, &raw[..end])?;
                return Ok(());
            }
        }
        anyhow::bail!("missing recorder boundary")
    };
    truncate_after(&|i| {
        if receipt {
            matches!(i, RolloutItem::TokenUsageContext {info,..} if info.last_token_usage.input_tokens == 100_000)
        } else {
            matches!(i, RolloutItem::Compacted(_))
        }
    })?;
    let compact_expected = receipt && !below_limit;
    let limit = if below_limit { 180_000 } else { 90_000 };
    let mut replies = Vec::new();
    let compact = || {
        responses::sse(vec![
            json!({"type":"response.output_item.done","item":{"type":"compaction","encrypted_content":"replacement"}}),
            ev_completed("compact"),
        ])
    };
    if compact_expected {
        replies.push(compact());
    }
    if !below_limit {
        let old_tool = bodies[2]["tools"]
            .as_array()
            .context("owed tools")?
            .iter()
            .filter_map(|t| t["name"].as_str())
            .find(|name| name.ends_with("probe_Old"))
            .context("the original task's app probe")?;
        replies.push(responses::sse(vec![
            responses::ev_function_call("old-probe", old_tool, "{}"),
            ev_completed_with_tokens("before", if recompact { 100_000 } else { 0 }),
        ]));
    }
    if recompact {
        replies.extend([compact(), finish("after-second-compact")]);
    }
    replies.push(finish("together"));
    let start = apps
        .received_requests()
        .await
        .context("HTTP recording")?
        .len();
    let traffic = responses::mount_sse_sequence(&apps, replies.clone()).await;
    let resume = builder(limit)
        .resume(&apps, test.home.clone(), path.clone())
        .await?;
    let Op::UserInput { items, .. } = prompt(FRESH, "Calendar", None) else {
        unreachable!()
    };
    let next_cwd = cwd.path().join("new-task");
    std::fs::create_dir(&next_cwd)?;
    resume
        .codex
        .submit(Op::UserTurn {
            items,
            cwd: next_cwd.clone(),
            model: target.into(),
            approval_policy: resume.config.approval_policy.value(),
            sandbox_policy: codex_protocol::protocol::SandboxPolicy::new_workspace_write_policy(),
            effort: None,
            summary: resume.config.model_reasoning_summary,
            final_output_json_schema: None,
            collaboration_mode: None,
            personality: None,
            service_tier: None,
        })
        .await?;
    complete(resume.codex.clone()).await;
    let requests = traffic.requests();
    assert_eq!(requests.len(), replies.len());
    // One wiremock records both protocols, so this is arrival order, not a clock comparison.
    let wire_order = |posts: &[wiremock::Request]| -> anyhow::Result<Vec<String>> {
        let mut order = Vec::new();
        for post in posts {
            if post.method.as_str() != "POST" {
                continue;
            }
            let body: Value = post.body_json()?;
            if body["method"] == codex_core::MCP_SANDBOX_STATE_METHOD {
                let policy = body["params"]["sandboxPolicy"]["type"]
                    .as_str()
                    .context("published policy")?;
                let expected_cwd = if policy == "read-only" {
                    cwd.path()
                } else {
                    &next_cwd
                };
                assert_eq!(
                    body["params"]["sandboxCwd"],
                    serde_json::to_value(expected_cwd)?
                );
                order.push(policy.to_owned());
            } else if body["method"] == "tools/call" {
                order.push(
                    body["params"]["name"]
                        .as_str()
                        .context("executed MCP tool")?
                        .to_owned(),
                );
            } else if post.url.path() == "/v1/responses" {
                order.push(body["model"].as_str().context("sampled model")?.to_owned());
            }
        }
        Ok(order)
    };
    let order = wire_order(
        &apps
            .received_requests()
            .await
            .context("recorded MCP and model calls")?[start..],
    )?;
    let mut expected = Vec::new();
    if !below_limit || order.first().is_some_and(|s| s == "read-only") {
        expected.push("read-only");
    }
    if compact_expected {
        expected.push("gpt-5.1");
    }
    if !below_limit {
        expected.push("gpt-5.1");
        expected.push("probe_Old");
    }
    if recompact {
        expected.extend(["gpt-5.1", "workspace-write", target, target]);
    } else {
        expected.extend(["workspace-write", target]);
    }
    assert_eq!(order, expected);
    if recompact {
        let compact = requests[1].body_json();
        assert_eq!(
            compact["input"].as_array().and_then(|v| v.last()),
            Some(&json!({"type":"compaction_trigger"}))
        );
        assert_request(&compact, &[], &["Old"])?;
        assert_eq!(compact["tools"], bodies[2]["tools"]);
        assert_request(
            &requests[3].body_json(),
            &[STEER, FRESH],
            &["Calendar", "Tasks"],
        )?;
        stop(resume.codex.clone()).await?;
        // After the old job returned, no further response is owed to it. Its completed
        // work still owns the very next compact, despite the matching model names.
        truncate_after(&|i| {
            matches!(i,
                RolloutItem::ResponseItem(codex_protocol::models::ResponseItem::FunctionCallOutput {call_id,..}) if call_id == "old-probe"
            )
        })?;
        let start = apps.received_requests().await.context("recording")?.len();
        let recovered = responses::mount_sse_sequence(&apps, replies[1..].to_vec()).await;
        let replay = builder(limit)
            .resume(&apps, test.home.clone(), path.clone())
            .await?;
        replay.codex.submit(Op::Continue).await?;
        complete(replay.codex.clone()).await;
        stop(replay.codex.clone()).await?;
        let retried = recovered.requests();
        assert_eq!(retried.len(), 3);
        for (idx, (actual, original)) in retried.iter().zip(&requests[1..]).enumerate() {
            assert!(
                actual.body_json() == original.body_json(),
                "request {idx} after the owed tool result"
            );
        }
        let order = wire_order(&apps.received_requests().await.context("recording")?[start..])?;
        assert!(
            order.ends_with(
                &["read-only", target, "workspace-write", target, target].map(str::to_owned)
            ),
            "recovery wire order: {order:?}"
        );
        return Ok(());
    }
    if below_limit {
        let joint = requests[0].body_json();
        assert_request(&joint, &[STEER, FRESH], &["Calendar", "Tasks"])?;
        assert!(joint.pointer("/text/format").is_none());
        stop(resume.codex.clone()).await?;
        return Ok(());
    }
    let assert_compact = |req: &responses::ResponsesRequest| {
        let body = req.body_json();
        for key in [
            "input",
            "tools",
            "model",
            "text",
            "reasoning",
            "service_tier",
        ] {
            assert!(
                body[key] == bodies[1][key],
                "compact {key} differs after work receipt"
            );
        }
    };
    if compact_expected {
        assert_compact(&requests[0]);
    }
    let requests = &requests[usize::from(compact_expected)..];
    let assert_owners = |reqs: &[responses::ResponsesRequest]| {
        let prior = reqs[0].body_json();
        assert_eq!(prior["model"], "gpt-5.1");
        assert_eq!(prior.pointer("/text/format/schema"), Some(&schema));
        assert!(prior["input"] == bodies[2]["input"]);
        assert!(prior["tools"] == bodies[2]["tools"]);
        let fresh = reqs[1].body_json();
        assert_eq!(fresh["model"], "gpt-5.2");
        assert!(fresh.pointer("/text/format/schema").is_none());
    };
    assert_owners(requests);
    assert_request(&requests[0].body_json(), &[], &["Old"])?;
    assert_request(
        &requests[1].body_json(),
        &[STEER, FRESH],
        &["Calendar", "Tasks"],
    )?;
    stop(resume.codex.clone()).await?;

    let joined = std::fs::read(&path)?;
    truncate_after(&|i| {
        matches!(i, RolloutItem::PostTurnCompletionReviewState(s)
        if s["owed_settings"].is_object() && s["pending_inputs"].as_array().is_some_and(|v| v.iter().any(|p| p["content"][0]["text"] == FRESH)))
    })?;
    let mock = responses::start_mock_server().await;
    let traffic = responses::mount_sse_sequence(&mock, replies).await;
    let accepted = builder(limit)
        .resume(&mock, test.home.clone(), path.clone())
        .await?;
    accepted.codex.submit(Op::Continue).await?;
    complete(accepted.codex.clone()).await;
    let sent = traffic.requests();
    if compact_expected {
        assert_compact(&sent[0]);
    }
    assert_owners(&sent[usize::from(compact_expected)..]);
    stop(accepted.codex.clone()).await?;
    std::fs::write(&path, joined)?;

    // Lose the process between the admissions. Only the new selections, not those
    // already sampled on the old task, belong to the first joint sample.
    truncate_after(&|i| {
        matches!(i, RolloutItem::PostTurnCompletionReviewState(s)
        if s["pending_inputs"].as_array().is_some_and(|p| p.iter().any(|v| v["content"][0]["text"] == FRESH))
            && s["explicit_app_paths"].as_array().is_some_and(|p| p.iter().any(|v| v == "app://Tasks")))
    })?;
    let mock = responses::start_mock_server().await;
    let traffic =
        responses::mount_sse_sequence(&mock, vec![finish("restored"), finish("unrelated")]).await;
    let resumed = builder(limit)
        .resume(&mock, test.home.clone(), path.clone())
        .await?;
    resumed.codex.submit(Op::Continue).await?;
    complete(resumed.codex.clone()).await;
    resumed
        .submit_turn("Another task with no app selection.")
        .await?;
    let requests = traffic.requests();
    assert_eq!(requests.len(), 2);
    assert_request(
        &requests[0].body_json(),
        &[STEER, FRESH],
        &["Calendar", "Tasks"],
    )?;
    assert_request(&requests[1].body_json(), &[STEER, FRESH], &[])?;
    Ok(())
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SteerAt {
    TriggeringTool,
    Notes,
    Installed,
}

#[test_case::test_case(SteerAt::TriggeringTool; "steered before the capture marker")]
#[test_case::test_case(SteerAt::Notes; "steered during capture")]
#[test_case::test_case(SteerAt::Installed; "steered after installation before the first response")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_v2_mid_turn_compaction_defers_pending_input_until_model_follow_up_finishes(
    phase: SteerAt,
) -> anyhow::Result<()> {
    assert_pending_input_after_compaction(true, phase).await
}

async fn assert_pending_input_after_compaction(
    remote_v2: bool,
    phase: SteerAt,
) -> anyhow::Result<()> {
    skip_if_no_network_unless_localhost!(Ok(()));
    let (release_model, gate) = oneshot::channel();
    let mut gate = Some(gate);
    let after_install = phase == SteerAt::Installed;
    let pending_text = match phase {
        SteerAt::TriggeringTool => "queued during the triggering tool round",
        SteerAt::Installed => "queued after installation",
        SteerAt::Notes => "queued during work-notes capture",
    };
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
            gate: if phase == SteerAt::TriggeringTool {
                gate.take()
            } else {
                None
            },
            body: sse_event(ev_completed_with_tokens(
                "resp-1",
                if remote_v2 { 100_000 } else { 96 },
            )),
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
            gate: if after_install { None } else { gate.take() },
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
            body: sse_event(if remote_v2 {
                serde_json::json!({"type":"response.output_item.done", "item": {"type":"compaction","encrypted_content":"COMPACTED SUMMARY"}})
            } else {
                ev_assistant_message("msg-summary", "COMPACTED SUMMARY")
            }),
        },
        StreamingSseChunk {
            gate: None,
            body: sse_event(ev_completed_with_tokens("resp-3", 10)),
        },
    ];
    let model_follow_up_chunks = vec![
        StreamingSseChunk {
            gate: gate.take(),
            body: sse_event(ev_response_created("resp-4")),
        },
        StreamingSseChunk {
            gate: None,
            body: sse_event(ev_assistant_message("msg-follow-up", "follow-up done")),
        },
        StreamingSseChunk {
            gate: None,
            body: sse_event(if after_install {
                ev_completed("resp-4")
            } else {
                ev_completed_with_tokens("resp-4", 10)
            }),
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

    let configure = move |config: &mut codex_core::config::Config| {
        config.compact_prompt = Some(SUMMARIZATION_PROMPT.to_string());
        config.model_context_window = Some(if remote_v2 { 200_000 } else { 100 });
        config.model_auto_compact_token_limit = Some(if remote_v2 { 90_000 } else { 90 });
        if remote_v2 {
            config.compaction_mode = codex_core::config::types::CompactionMode::Auto;
        } else {
            config.compaction_mode = codex_core::config::types::CompactionMode::Local;
        }
    };
    let home = Arc::new(tempfile::tempdir()?);
    let skill_dir = home.path().join("skills/resumed-task");
    std::fs::create_dir_all(&skill_dir)?;
    let skill_body = "Instructions selected only by the new prompt.";
    let skill_path = skill_dir.join("SKILL.md");
    std::fs::write(
        &skill_path,
        format!("---\nname: resumed-task\ndescription: A test skill.\n---\n\n{skill_body}\n"),
    )?;
    let skill_path = skill_path.canonicalize()?;
    let selected_skill = format!("Selected skill: resumed-task ({})", skill_path.display());
    let test = test_codex()
        .with_home(home)
        .with_model("gpt-5.1")
        .with_config(configure)
        .build_with_streaming_server(&server)
        .await?;
    let codex = test.codex.clone();

    codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "trigger tool and compaction".into(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
        })
        .await?;

    if phase != SteerAt::Notes {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while server.requests().await.len() < if after_install { 4 } else { 1 } {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await?;
    } else {
        wait_for_event(&codex, |event| {
            matches!(event, EventMsg::AgentMessage(message) if message.message == work_notes_text)
        }).await;
    }

    codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: pending_text.into(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
        })
        .await?;

    wait_for_queued_input(&codex).await;
    let _ = release_model.send(());
    wait_for_event(&codex, |event| matches!(event, EventMsg::TurnComplete(_))).await;

    let requests = server.requests().await;
    assert_eq!(requests.len(), 5);

    let bodies = (0..requests.len())
        .map(|index| request_body(&requests, index))
        .collect::<Vec<_>>();
    if remote_v2 {
        assert_eq!(
            request_input_items(&bodies[2]).last(),
            Some(&serde_json::json!({"type":"compaction_trigger"}))
        );
    } else {
        assert!(
            message_input_texts(&bodies[2], "user")
                .iter()
                .any(|text| text == SUMMARIZATION_PROMPT),
            "expected third request to be compaction"
        );
    }

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

    if remote_v2 {
        use codex_protocol::protocol::RolloutItem;
        use codex_protocol::protocol::RolloutLine;
        codex.submit(Op::Shutdown).await?;
        wait_for_event(&codex, |e| matches!(e, EventMsg::ShutdownComplete)).await;
        let path = test
            .session_configured
            .rollout_path
            .as_ref()
            .context("raw rollout")?;
        let rollout = std::fs::read_to_string(path)?;
        let (mut accepted, mut installed, mut continued) = (None, None, None);
        let mut marker = None;
        let mut end = 0;
        for line in rollout.split_inclusive('\n') {
            end += line.len();
            match serde_json::from_str::<RolloutLine>(line)?.item {
                RolloutItem::PostTurnCompletionReviewState(state)
                    if accepted.is_none()
                        && state["pending_inputs"].as_array().is_some_and(|items| {
                            items.iter().any(|i| {
                                i["installation"] == "Accepted"
                                    && i["content"][0]["text"] == pending_text
                            })
                        }) =>
                {
                    accepted = Some(end)
                }
                RolloutItem::ResponseItem(codex_protocol::models::ResponseItem::Message {
                    role,
                    content,
                    ..
                }) if role == "developer"
                    && serde_json::to_string(&content)?
                        .contains("<AUTO_COMPACT_WORK_NOTES_REQUEST>") =>
                {
                    marker = Some((end - line.len(), end))
                }
                RolloutItem::Compacted(_) => installed = Some(end),
                RolloutItem::PostTurnCompletionReviewState(state)
                    if after_install
                        && accepted.is_some()
                        && continued.is_none()
                        && state["awaiting_compact_followup"] == false
                        && state["pending_inputs"]
                            .as_array()
                            .is_some_and(|p| !p.is_empty()) =>
                {
                    continued = Some(end);
                }
                RolloutItem::TokenUsageContext { info, .. }
                    if installed.is_some()
                        && continued.is_none()
                        && info.last_token_usage.input_tokens > 0 =>
                {
                    continued = Some(end)
                }
                _ => {}
            }
        }
        let accepted = accepted.context("durable queued input")?;
        let installed = installed.context("durable replacement")?;
        assert_eq!(accepted > installed, after_install);
        #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
        enum Next {
            Notes,
            Compact,
            Continue,
            Amend,
        }
        let mut cuts = match phase {
            SteerAt::TriggeringTool => {
                let (start, end) = marker.context("notes request")?;
                assert!(accepted < start);
                vec![
                    (start, Next::Notes, false),
                    (end, Next::Notes, true),
                    (installed, Next::Continue, false),
                ]
            }
            SteerAt::Installed => vec![
                (accepted, Next::Continue, false),
                (accepted, Next::Continue, true),
            ],
            SteerAt::Notes => vec![
                (accepted, Next::Compact, true),
                (installed, Next::Continue, false),
            ],
        };
        cuts.push((
            continued.context("first continuation completed")?,
            Next::Amend,
            false,
        ));
        let fresh_text = "fresh prompt after restart using $resumed-task";
        for (cut, next, legacy, fresh) in cuts
            .into_iter()
            .flat_map(|(cut, next, legacy)| [false, true].map(|fresh| (cut, next, legacy, fresh)))
        {
            let prefix = &rollout[..cut];
            if legacy {
                // Earlier hosts had no standing barrier; some wrote a false per-input
                // flag for a steer first accepted after installation.
                let mut legacy = Vec::new();
                for line in prefix.lines() {
                    let mut record: Value = serde_json::from_str(line)?;
                    if let Some(obj) = record["payload"].as_object_mut() {
                        obj.remove("awaiting_compact_followup");
                    }
                    if let Some(inputs) = record["payload"]
                        .get_mut("pending_inputs")
                        .and_then(Value::as_array_mut)
                    {
                        for input in inputs {
                            let input = input.as_object_mut().context("pending input object")?;
                            if after_install {
                                input.insert("after_compaction".into(), false.into());
                            } else {
                                input.remove("after_compaction");
                            }
                            input.remove("new_task");
                        }
                    }
                    serde_json::to_writer(&mut legacy, &record)?;
                    legacy.push(b'\n');
                }
                std::fs::write(path, legacy)?;
            } else {
                std::fs::write(path, prefix)?;
            }
            let mock = responses::start_mock_server().await;
            let mut replies = Vec::new();
            if next == Next::Notes {
                replies.push(responses::sse(vec![
                    ev_assistant_message("notes", work_notes_text),
                    ev_completed("notes"),
                ]));
            }
            if next <= Next::Compact {
                replies.push(responses::sse(vec![serde_json::json!({"type":"response.output_item.done","item":{"type":"compaction","encrypted_content":"RECOVERED"}}), ev_completed("compact")]));
            }
            replies.extend(
                (next <= Next::Continue)
                    .then_some("first model continuation")
                    .into_iter()
                    .chain(["amended answer"])
                    .map(|text| {
                        responses::sse(vec![ev_assistant_message("a", text), ev_completed("a")])
                    }),
            );
            let count = replies.len();
            let captured = responses::mount_sse_sequence(&mock, replies).await;
            let resumed = test_codex()
                .with_model("gpt-5.1")
                .with_config(configure)
                .resume(&mock, test.home.clone(), path.clone())
                .await?;
            resumed
                .codex
                .submit(if fresh {
                    Op::UserInput {
                        items: vec![
                            UserInput::Text {
                                text: fresh_text.into(),
                                text_elements: vec![],
                            },
                            UserInput::Skill {
                                name: "resumed-task".into(),
                                path: skill_path.clone(),
                            },
                        ],
                        final_output_json_schema: None,
                    }
                } else {
                    Op::Continue
                })
                .await?;
            wait_for_event(&resumed.codex, |event| match event {
                EventMsg::Error(e) => panic!("{}", e.message),
                EventMsg::TurnComplete(_) => true,
                _ => false,
            })
            .await;
            let actual = captured.requests();
            assert_eq!(actual.len(), count);
            let expected = [Some(pending_text), fresh.then_some(fresh_text)]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>();
            for (index, req) in actual.iter().enumerate() {
                let texts = message_input_texts(&req.body_json(), "user");
                assert_eq!(
                    texts.iter().filter(|t| t.contains(skill_body)).count(),
                    usize::from(fresh && index + 1 == actual.len())
                );
                let amendments = texts
                    .iter()
                    .map(String::as_str)
                    .filter(|t| *t == pending_text || *t == fresh_text)
                    .collect::<Vec<_>>();
                assert_eq!(
                    amendments.as_slice(),
                    if index + 1 == actual.len() {
                        expected.as_slice()
                    } else {
                        &[]
                    }
                );
            }
            resumed.codex.submit(Op::Shutdown).await?;
            wait_for_event(&resumed.codex, |e| matches!(e, EventMsg::ShutdownComplete)).await;
            let amended = std::fs::read_to_string(path)?;
            let snapshot = amended
                .lines()
                .rev()
                .filter_map(|l| serde_json::from_str::<RolloutLine>(l).ok())
                .find_map(|l| match l.item {
                    RolloutItem::PostTurnCompletionReviewState(s) => Some(s),
                    _ => None,
                })
                .context("consumed request snapshot")?;
            let consumed = snapshot["request_frame"]["current_user_messages"]
                .as_array()
                .context("review scope")?
                .iter()
                .filter_map(Value::as_str)
                .filter(|t| *t == pending_text || *t == fresh_text || *t == selected_skill)
                .collect::<Vec<_>>();
            let expected_scope = expected
                .into_iter()
                .chain(fresh.then_some(selected_skill.as_str()))
                .collect::<Vec<_>>();
            assert_eq!(consumed, expected_scope);
            assert_eq!(snapshot["pending_inputs"], serde_json::json!([]));
        }
    }
    server.shutdown().await;
    Ok(())
}
