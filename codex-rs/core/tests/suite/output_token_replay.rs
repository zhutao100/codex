//! Replay is OpenAI-only; stored ciphertext survives opt-out and provider changes.

use anyhow::Context;
use anyhow::Result;
use codex_core::config::Config;
use codex_core::features::Feature;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_function_call;
use core_test_support::responses::ev_reasoning_item;
use core_test_support::responses::ev_web_search_call_done;
use core_test_support::responses::mount_sse_sequence;
use core_test_support::responses::sse;
use core_test_support::responses::start_mock_server;
use core_test_support::responses::start_websocket_server;
use core_test_support::skip_if_no_network_unless_localhost;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use pretty_assertions::assert_eq;
use serde_json::json;

#[test_case::test_case(false, false; "disabled other provider")]
#[test_case::test_case(false, true; "disabled openai")]
#[test_case::test_case(true, false; "enabled other provider")]
#[test_case::test_case(true, true; "enabled openai")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn output_token_replay_respects_provider_and_preserves_history(
    enabled: bool,
    openai_provider: bool,
) -> Result<()> {
    skip_if_no_network_unless_localhost!(Ok(()));
    let server = start_mock_server().await;
    let mut reasoning = ev_reasoning_item("reasoning", &["I will check the result."], &[]);
    reasoning["item"]["content"] = json!(null);
    let mut commentary = ev_assistant_message("commentary", "I will record the completed step.");
    commentary["item"]["phase"] = json!("commentary");
    commentary["item"]["encrypted_content"] = json!("encrypted-commentary");
    let mut search = ev_web_search_call_done("ws-1", "completed", "example");
    search["item"]["encrypted_content"] = json!("encrypted-search");
    let mut call = ev_function_call(
        "plan-call",
        "update_plan",
        &json!({"plan": [{"step": "Check the result", "status": "completed"}]}).to_string(),
    );
    call["item"]["encrypted_content"] = json!("encrypted-call");
    call["item"]["status"] = json!("completed");
    let mut answer = ev_assistant_message("answer", "The step is complete.");
    answer["item"]["encrypted_content"] = json!("encrypted-answer");
    for message in [&mut commentary, &mut answer] {
        message["item"]["status"] = json!("completed");
        message["item"]["content"][0]["annotations"] = json!([]);
        message["item"]["content"][0]["logprobs"] = json!([]);
    }
    commentary["item"]["content"][0]["annotations"] = json!([{
        "type": "url_citation",
        "url": "https://example.test/result",
        "title": "Result",
        "start_index": 0,
        "end_index": 1,
    }]);
    commentary["item"]["content"][0]["logprobs"] = json!([{
        "token": "I", "logprob": -0.25, "bytes": [73], "top_logprobs": [],
    }]);
    let expected_items = [
        reasoning["item"].clone(),
        commentary["item"].clone(),
        search["item"].clone(),
        call["item"].clone(),
        answer["item"].clone(),
    ];
    let mock = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                reasoning,
                commentary,
                search,
                call,
                ev_completed("plan-response"),
            ]),
            sse(vec![answer, ev_completed("answer-response")]),
            sse(vec![
                ev_assistant_message("resumed", "Still complete."),
                ev_completed("resumed"),
            ]),
        ],
    )
    .await;
    let configure = move |config: &mut Config| {
        if enabled {
            config.features.enable(Feature::OutputTokenReplay);
        }
        config.model_provider.name = if openai_provider { "OpenAI" } else { "mock" }.to_string();
    };
    let mut builder = test_codex().with_config(configure);
    let test = builder.build(&server).await?;
    test.submit_turn("Record that the result was checked.")
        .await?;
    let path = test.codex.rollout_path().context("recorded thread")?;
    test.codex.submit(Op::Shutdown {}).await?;
    wait_for_event(&test.codex, |e| matches!(e, EventMsg::ShutdownComplete)).await;

    // Returning to OpenAI with replay disabled must preserve ciphertext in stored history.
    builder = builder.with_config(move |config| {
        configure(config);
        config.model_provider.name = "OpenAI".to_string();
        config.features.disable(Feature::OutputTokenReplay);
    });
    let resumed = builder.resume(&server, test.home.clone(), path).await?;
    resumed.submit_turn("Is the step still complete?").await?;

    let requests = mock.requests();
    assert_eq!(requests.len(), 3);
    let prefix = &requests[0].input()[0];
    assert_eq!(
        requests[0].instructions_text(),
        requests[2].instructions_text()
    );
    assert_eq!(&requests[1].input()[0], prefix);
    assert_eq!(&requests[2].input()[0], prefix);
    let expected_include = if enabled && openai_provider {
        json!(["reasoning.encrypted_content", "output.encrypted_content"])
    } else {
        json!(["reasoning.encrypted_content"])
    };
    assert_eq!(requests[0].body_json()["include"], expected_include);
    assert_eq!(requests[1].body_json()["include"], expected_include);
    assert_eq!(
        requests[2].body_json()["include"],
        json!(["reasoning.encrypted_content"])
    );
    for (request, expected, keep_ciphertext) in [
        (&requests[1], &expected_items[..4], openai_provider),
        (&requests[2], &expected_items[..], true),
    ] {
        let body = request.body_json();
        let replayed = body["input"]
            .as_array()
            .context("request input")?
            .iter()
            .filter(|item| {
                item["role"] == "assistant"
                    || item["type"] == "function_call"
                    || item["type"] == "reasoning"
                    || item["type"] == "web_search_call"
            })
            .cloned()
            .collect::<Vec<_>>();
        let mut expected = json!(expected);
        for item in expected.as_array_mut().context("expected items")? {
            let is_reasoning = item["type"] == "reasoning";
            let fields = item.as_object_mut().context("expected item")?;
            fields.remove("id");
            if !keep_ciphertext && !is_reasoning {
                fields.remove("encrypted_content");
            }
        }
        assert_eq!(json!(replayed), expected);
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn missing_previous_response_replays_full_hosted_search_history() -> Result<()> {
    skip_if_no_network_unless_localhost!(Ok(()));
    let mut search = ev_web_search_call_done("ws-1", "completed", "example");
    search["item"]["encrypted_content"] = json!("opaque-search-tokens");
    let reasoning = ev_reasoning_item("rs-1", &["check"], &[]);
    let mut expected_reasoning = reasoning["item"].clone();
    expected_reasoning.as_object_mut().unwrap().remove("id");
    expected_reasoning["content"] = json!(null);
    let server = start_websocket_server(vec![
        vec![
            vec![
                search,
                reasoning,
                ev_function_call("p", "update_plan", r#"{"plan":[]}"#),
                ev_completed("first"),
            ],
            vec![
                json!({"type":"error","status":400,"error":{"code":"previous_response_not_found"}}),
            ],
        ],
        vec![vec![
            ev_assistant_message("a", "done"),
            ev_completed("final"),
        ]],
    ])
    .await;
    let test = test_codex()
        .with_config(|config| {
            config.model_provider.name = "OpenAI".to_string();
            config.model_provider.supports_websockets = true;
            config.features.enable(Feature::OutputTokenReplay);
        })
        .build_with_websocket_server(&server)
        .await?;
    test.submit_turn("Search, then update the plan.").await?;
    let connections = server.connections();
    let [first, second] = connections.as_slice() else {
        panic!("expected one reconnect")
    };
    let [initial, delta] = first.as_slice() else {
        panic!("expected initial and incremental sends")
    };
    let [retry] = second.as_slice() else {
        panic!("expected one successful retry")
    };
    let (initial, delta, retry) = (initial.body_json(), delta.body_json(), retry.body_json());
    assert_eq!(delta["previous_response_id"], "first");
    assert_eq!(retry["previous_response_id"], serde_json::Value::Null);
    assert_eq!(
        initial["include"],
        json!(["reasoning.encrypted_content", "output.encrypted_content"])
    );
    let mut expected = initial["input"].as_array().unwrap().clone();
    expected.push(json!({"type":"web_search_call","status":"completed","action":{"type":"search","query":"example"},"encrypted_content":"opaque-search-tokens"}));
    expected.push(expected_reasoning);
    expected.push(json!({"type":"function_call","name":"update_plan","arguments":"{\"plan\":[]}","call_id":"p"}));
    let suffix = delta["input"].as_array().unwrap();
    assert_eq!(suffix.len(), 1);
    assert_eq!(suffix[0]["call_id"], "p");
    expected.extend_from_slice(suffix);
    assert_eq!(retry["input"], json!(expected));
    server.shutdown().await;
    Ok(())
}
