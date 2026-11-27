#![allow(clippy::expect_used, clippy::unwrap_used)]

#[path = "compact_remote_v2_recovery.rs"]
mod recovery;

use anyhow::Result;
use codex_core::CodexAuth;
use codex_core::protocol::EventMsg;
use codex_core::protocol::Op;
use codex_core::protocol::RolloutItem;
use codex_core::protocol::RolloutLine;
use codex_protocol::items::TurnItem;
use codex_protocol::models::ContentItem;
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
        assert_eq!(
            requests[1].instructions_text(),
            requests[0].instructions_text()
        );
        assert_eq!(compact["input"][0], first["input"][0]);
        for field in [
            "tools",
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
    assert_eq!(input[0], compact["input"][0]);
    assert_eq!(serde_json::to_value(&replacements[0])?, json!(&input[1..]));
    Ok(())
}

#[test_case::test_case(false; "stream failure")]
#[test_case::test_case(true; "HTTP 400 before the notes stream")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rejected_notes_preserve_work_and_billed_usage_through_install_and_replay(
    http: bool,
) -> Result<()> {
    skip_if_no_network_unless_localhost!(Ok(()));
    let server = responses::start_mock_server().await;
    let receipt = |id, tokens, cached| {
        let mut event = responses::ev_completed_with_tokens(id, tokens);
        event["response"]["usage"]["input_tokens_details"] = json!({"cached_tokens":cached});
        event
    };
    let followup = || {
        responses::sse(vec![
            responses::ev_assistant_message("done", "Done."),
            receipt("done", 3000, 128),
        ])
    };
    let refusal = if http {
        ResponseTemplate::new(400).set_body_json(
            json!({"error":{"code":"context_length_exceeded","message":"notes do not fit"}}),
        )
    } else {
        responses::sse_response(responses::sse_failed(
            "notes",
            "context_length_exceeded",
            "notes do not fit",
        ))
    };
    let wire = responses::mount_response_sequence(
        &server,
        vec![
            responses::sse_response(responses::sse(vec![
                responses::ev_shell_command_call("work", "printf evidence"),
                receipt("work", 28_000, 25_600),
            ])),
            refusal,
            responses::sse_response(responses::sse(vec![
                ev_compaction("summary"),
                receipt("compact", 1000, 512),
            ])),
            responses::sse_response(followup()),
            responses::sse_response(followup()),
        ],
    )
    .await;
    let build = || {
        test_codex().with_config(|cfg| {
            cfg.model_context_window = Some(32_000);
            cfg.model_auto_compact_token_limit = Some(27_000);
        })
    };
    let original = build().build(&server).await?;
    original.codex.submit(user_input("Read evidence.")).await?;
    complete(&original.codex).await;
    let path = original.codex.rollout_path().unwrap();
    original.codex.submit(Op::Shutdown).await?;
    wait_for_event(&original.codex, |e| matches!(e, EventMsg::ShutdownComplete)).await;
    let raw = std::fs::read_to_string(&path)?;
    let mut cut = 0;
    let mut bills = Vec::new();
    for line in raw.split_inclusive('\n') {
        cut += line.len();
        match serde_json::from_str::<RolloutLine>(line)?.item {
            RolloutItem::TokenUsageContext {
                info,
                helper,
                measured_surface,
                rejected_context,
                ..
            } => {
                assert_eq!(rejected_context, None);
                bills.push((
                    info.total_token_usage.input_tokens,
                    info.total_token_usage.cached_input_tokens,
                ));
                if info.last_token_usage.input_tokens == 1000 {
                    assert_eq!(helper.unwrap().work_usage.input_tokens, 28_000);
                    assert!(measured_surface.unwrap().endpoint_sha256.is_some());
                }
            }
            RolloutItem::Compacted(c) => {
                let billed = c.checkpoint.unwrap().usage.total_token_usage;
                assert_eq!(
                    (billed.input_tokens, billed.cached_input_tokens),
                    (29_000, 26_112)
                );
                break;
            }
            _ => {}
        }
    }
    assert_eq!(bills, [(28_000, 25_600), (29_000, 26_112)]);
    std::fs::write(&path, &raw[..cut])?;
    let resumed = build()
        .resume(&server, original.home.clone(), path.clone())
        .await?;
    resumed.codex.submit(Op::Continue).await?;
    complete(&resumed.codex).await;
    resumed.codex.submit(Op::Shutdown).await?;
    wait_for_event(&resumed.codex, |e| matches!(e, EventMsg::ShutdownComplete)).await;
    let total = std::fs::read_to_string(&path)?
        .lines()
        .map(serde_json::from_str::<RolloutLine>)
        .collect::<serde_json::Result<Vec<_>>>()?
        .into_iter()
        .rev()
        .find_map(|l| match l.item {
            RolloutItem::TokenUsageContext { info, .. } => Some(info.total_token_usage),
            _ => None,
        })
        .unwrap();
    assert_eq!(
        (total.input_tokens, total.cached_input_tokens),
        (32_000, 26_240)
    );
    let sent = wire.requests();
    assert_eq!(sent.len(), 5);
    let capture = sent[1].input();
    let (marker, notes_prefix) = capture.split_last().unwrap();
    assert!(
        marker
            .to_string()
            .contains("AUTO_COMPACT_WORK_NOTES_REQUEST")
    );
    assert_compact_request(&sent[2].body_json());
    assert_eq!(notes_prefix, sent[2].input().split_last().unwrap().1);
    assert_eq!(sent[3].input(), sent[4].input());
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn measured_second_user_turn_preserves_helper_prefix_without_reasoning_header() -> Result<()>
{
    skip_if_no_network_unless_localhost!(Ok(()));
    let server = responses::start_mock_server().await;
    let builder = || {
        test_codex().with_config(|cfg| {
            cfg.model_context_window = Some(32_000);
            cfg.model_auto_compact_token_limit = Some(27_000);
            cfg.model_overlay
                .get_or_insert_with(Default::default)
                .cross_model
                .experimental_supported_tools = Some(vec!["read_file".into()]);
        })
    };
    let test = builder().build(&server).await?;
    let path = test.workspace_path("evidence.txt");
    std::fs::write(&path, "original evidence\n".repeat(120))?;
    let args = json!({"file_path": path}).to_string();
    let mut reasoning = responses::ev_reasoning_item("past", &[], &[]);
    reasoning["item"]["encrypted_content"] = json!("A".repeat(240_000));
    let reply = |id, text, tokens| {
        responses::sse(vec![
            responses::ev_assistant_message(id, text),
            responses::ev_completed_with_tokens(id, tokens),
        ])
    };
    let helpers = || {
        [
            reply("notes", NOTES, 14_000),
            compact_response("summary"),
            reply("done", "Finished.", 3_000),
        ]
    };
    let streams = [
        responses::sse(vec![
            reasoning,
            responses::ev_function_call("early", "read_file", &args),
            responses::ev_completed_with_tokens("work", 8_000),
        ]),
        reply("first", "Initial task done.", 10_000),
        responses::sse(vec![
            responses::ev_function_call("tail", "read_file", &args),
            responses::ev_completed_with_tokens("second", 14_000),
        ]),
    ]
    .into_iter()
    .chain(helpers())
    .collect();
    let mock = responses::mount_sse_sequence(&server, streams).await;
    test.submit_turn("Read the evidence.").await?;
    test.submit_turn("Check the evidence again.").await?;
    let captured = mock.requests();
    assert_eq!(captured.len(), 6);
    let work = captured[2].input();
    let notes = captured[3].input();
    let (marker, prefix) = notes.split_last().unwrap();
    assert!(
        marker["content"][0]["text"]
            .as_str()
            .unwrap()
            .starts_with("<AUTO_COMPACT_WORK_NOTES_REQUEST>")
    );
    assert!(prefix.starts_with(&work), "notes changed measured work");
    assert_eq!(prefix.len(), work.len() + 2);
    for req in &captured[3..5] {
        for call in ["early", "tail"] {
            let (text, _) = req.function_call_output_content_and_success(call).unwrap();
            assert_eq!(text.unwrap().matches("original evidence").count(), 120);
        }
    }
    let compact = captured[4].input();
    let (trigger, source) = compact.split_last().unwrap();
    assert_eq!(trigger, &json!({"type":"compaction_trigger"}));
    assert!(
        source == prefix,
        "notes and V2 did not share the measured work"
    );
    test.codex.submit(Op::Shutdown).await?;
    wait_for_event(&test.codex, |e| matches!(e, EventMsg::ShutdownComplete)).await;

    // Lose the process after the marker, before notes usage can replace measured work.
    let path = test.session_configured.rollout_path.as_ref().unwrap();
    let rollout = std::fs::read_to_string(path)?;
    let mut cutoff = 0;
    for line in rollout.split_inclusive('\n') {
        cutoff += line.len();
        let record: RolloutLine = serde_json::from_str(line)?;
        if let RolloutItem::ResponseItem(ResponseItem::Message { role, content, .. }) = record.item
            && role == "developer"
            && content.iter().any(|part| matches!(part, ContentItem::InputText { text } if text.starts_with("<AUTO_COMPACT_WORK_NOTES_REQUEST>")))
        {
            break;
        }
    }
    assert!(cutoff < rollout.len());
    for measured in [true, false] {
        let prefix = &rollout[..cutoff];
        if measured {
            std::fs::write(path, prefix)?;
        } else {
            let mut legacy = Vec::new();
            for line in prefix.lines() {
                let mut record: RolloutLine = serde_json::from_str(line)?;
                if let RolloutItem::TokenUsageContext { source_model, .. } = &mut record.item {
                    *source_model = None;
                } else if let RolloutItem::PostTurnCompletionReviewState(s) = &mut record.item {
                    // Legacy writers recorded neither measurement ownership nor a frozen fit decision.
                    s.as_object_mut().unwrap().remove("pending_compaction");
                }
                serde_json::to_writer(&mut legacy, &record)?;
                legacy.push(b'\n');
            }
            std::fs::write(path, legacy)?;
        }
        let replay = responses::mount_sse_sequence(&server, helpers().into()).await;
        let resumed = builder()
            .resume(&server, test.home.clone(), path.clone())
            .await?;
        resumed.codex.submit(Op::Continue).await?;
        complete(&resumed.codex).await;
        let replayed = replay.requests();
        assert_eq!(replayed.len(), 3);
        if measured {
            assert!(
                replayed[0].input() == notes,
                "notes prefix changed on replay"
            );
            assert!(
                replayed[1].input() == compact,
                "compact prefix changed on replay"
            );
        } else {
            // The legacy user-turn hint matches the requested model, but this alone
            // cannot discount the oversized reasoning from the local input estimate.
            let (text, _) = replayed[0]
                .function_call_output_content_and_success("early")
                .unwrap();
            assert!(text.unwrap().matches("original evidence").count() < 120);
        }
        resumed.codex.submit(Op::Shutdown).await?;
        wait_for_event(&resumed.codex, |e| matches!(e, EventMsg::ShutdownComplete)).await;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn repeated_auto_compaction_preserves_the_full_work_prefix_and_installs_only_latest_notes()
-> Result<()> {
    skip_if_no_network_unless_localhost!(Ok(()));
    let server = responses::start_mock_server().await;
    let test = test_codex()
        .with_config(|config| {
            config.model_context_window = Some(32_000);
            config.model_auto_compact_token_limit = Some(27_000);
            let overlay = config.model_overlay.get_or_insert_with(Default::default);
            overlay.cross_model.experimental_supported_tools = Some(vec!["read_file".into()]);
        })
        .build(&server)
        .await?;
    let path = test.workspace_path("evidence.txt");
    std::fs::write(&path, "observed evidence\n".repeat(120))?;
    let arguments = json!({"file_path":path}).to_string();
    let mut streams = Vec::new();
    let mut starts = Vec::new();
    let extra_candidates = [
        "Second same-scope candidate.",
        "Third same-scope candidate.",
    ];
    for epoch in 1..=5 {
        if epoch == 3 {
            for candidate in extra_candidates {
                streams.extend([
                    responses::sse(vec![responses::ev_assistant_message("candidate", candidate), responses::ev_completed_with_tokens("candidate", 14_000)]),
                    responses::sse(vec![responses::ev_assistant_message("review", r#"{"evaluation":"Check the next candidate.","fix_actions_advised":true}"#), responses::ev_completed("review")]),
                ]);
            }
        }
        starts.push(streams.len());
        let mut reasoning = responses::ev_reasoning_item("reasoning", &[], &[]);
        // The opaque wire representation is large; the completed work request reports that it fits.
        reasoning["item"]["encrypted_content"] = json!("A".repeat(240_000));
        streams.extend([
            responses::sse(vec![
                reasoning,
                responses::ev_function_call("early", "read_file", &arguments),
                responses::ev_completed_with_tokens("work", 14_000),
            ]),
            responses::sse(vec![
                responses::ev_function_call("last", "read_file", &arguments),
                // Include a review-budget crossing with three anchored frames, below the automatic threshold.
                responses::ev_completed_with_tokens(
                    "near-limit",
                    if matches!(epoch, 2..=4) {
                        24_000
                    } else {
                        28_000
                    },
                ),
            ]),
            responses::sse(vec![
                responses::ev_assistant_message("notes", &format!("{NOTES} {epoch}")),
                responses::ev_completed_with_tokens("notes", 31_999),
            ]),
            compact_response(&format!("summary-{epoch}")),
        ]);
        if epoch == 1 {
            streams.extend([
                responses::sse(vec![
                    responses::ev_assistant_message("candidate", "Initial complete answer."),
                    responses::ev_completed("candidate"),
                ]),
                responses::sse(vec![
                    responses::ev_assistant_message(
                        "review",
                        r#"{"evaluation":"Verify the remaining work.","fix_actions_advised":true}"#,
                    ),
                    responses::ev_completed("review"),
                ]),
            ]);
        }
    }
    streams.push(responses::sse(vec![
        responses::ev_assistant_message("done", "Finished all work."),
        responses::ev_completed("done"),
    ]));
    let mock = responses::mount_sse_sequence(&server, streams).await;
    test.codex
        .submit(user_input("continue the same work"))
        .await?;
    complete(&test.codex).await;
    for expected in extra_candidates.into_iter().chain(["Finished all work."]) {
        test.codex.submit(Op::ReviewCompletedTurn).await?;
        wait_for_event(&test.codex, |event| match event {
            EventMsg::Error(error) => panic!("unexpected error: {}", error.message),
            EventMsg::TurnComplete(turn) => turn.last_agent_message.as_deref() == Some(expected),
            _ => false,
        })
        .await;
    }
    test.codex.submit(Op::Shutdown).await?;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::ShutdownComplete)
    })
    .await;
    let installed = replacement_history(test.session_configured.rollout_path.as_ref().unwrap());
    let requests = mock.requests();
    assert_eq!(requests.len(), 27);
    assert_eq!(installed.len(), starts.len());
    let mut previous_wrapper = None;
    for (epoch, start) in starts.into_iter().enumerate() {
        let [work, notes, compact, followup] = &requests[start + 1..start + 5] else {
            unreachable!()
        };
        let hot = work.input();
        let capture = notes.input();
        let (marker, source) = capture.split_last().unwrap();
        assert!(
            marker["content"][0]["text"]
                .as_str()
                .unwrap()
                .starts_with("<AUTO_COMPACT_WORK_NOTES_REQUEST>")
        );
        assert!(
            source.starts_with(&hot),
            "capture rewrote the sampled prefix"
        );
        let ordinary = work.body_json();
        for request in [notes, compact] {
            let body = request.body_json();
            for key in [
                "model",
                "instructions",
                "tools",
                "parallel_tool_calls",
                "reasoning",
                "prompt_cache_key",
            ] {
                assert_eq!(body[key], ordinary[key], "changed request field {key}");
            }
        }
        let is_review_frame = |item: &Value| {
            item["role"] == "developer"
                && item["content"][0]["text"]
                    .as_str()
                    .is_some_and(|text| text.contains("<completed_turn_review_context>"))
        };
        assert_eq!(
            hot.iter().filter(|item| is_review_frame(item)).count(),
            if epoch == 2 {
                3
            } else {
                usize::from(epoch > 0)
            }
        );
        if epoch == 2 {
            for (n, candidate) in extra_candidates.iter().enumerate() {
                let previous = requests[start - 4 + 2 * n].input();
                let next = requests[start - 2 + 2 * n].input();
                assert!(next.starts_with(&previous));
                assert_eq!(next[previous.len()]["content"][0]["text"], *candidate);
                assert_eq!(
                    next.iter().filter(|item| is_review_frame(item)).count(),
                    n + 2
                );
            }
        }
        for (request, call) in [(work, "early"), (notes, "last")] {
            let (text, _) = request
                .function_call_output_content_and_success(call)
                .unwrap();
            assert_eq!(text.unwrap().matches("observed evidence").count(), 120);
        }

        let compact_input = compact.input();
        let (trigger, compact_source) = compact_input.split_last().unwrap();
        assert_eq!(trigger, &json!({"type":"compaction_trigger"}));
        assert!(
            compact_source == source,
            "notes and compact diverged before their last item in cycle {}",
            epoch + 1
        );
        let replacement = followup.input();
        if let Some(previous) = &previous_wrapper {
            assert!(hot.contains(previous));
            assert!(!replacement.contains(previous));
        }
        let persisted = serde_json::to_value(&installed[epoch])?;
        let persisted = persisted.as_array().unwrap();
        assert!(!persisted.iter().any(is_review_frame));
        let wrappers = persisted
            .iter()
            .filter(|item| {
                item["role"] == "user"
                    && item["content"][0]["text"]
                        .as_str()
                        .is_some_and(|text| text.starts_with("Immediately before compaction,"))
            })
            .collect::<Vec<_>>();
        let [wrapper] = wrappers.as_slice() else {
            panic!("expected one installed notes wrapper")
        };
        assert!(replacement.contains(wrapper));
        assert!(
            wrapper["content"][0]["text"]
                .as_str()
                .unwrap()
                .ends_with(&format!("{NOTES} {}", epoch + 1))
        );
        previous_wrapper = Some((*wrapper).clone());
    }
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
            responses::ev_completed_with_tokens("work", 28_000),
        ],
        vec![
            responses::ev_assistant_message("notes", NOTES),
            responses::ev_completed_with_tokens("notes", 4_000),
        ],
        vec![
            ev_compaction("summary"),
            responses::ev_completed_with_tokens("compact", 4_001),
        ],
        vec![
            responses::ev_assistant_message("done", "done"),
            responses::ev_completed_with_tokens("done", 4_002),
        ],
    ]])
    .await;
    let test = test_codex()
        .with_config(|config| {
            config.model_context_window = Some(32_000);
            config.model_auto_compact_token_limit = Some(27_000);
        })
        .build_with_websocket_server(&server)
        .await?;
    test.codex.submit(user_input("do work")).await?;
    complete(&test.codex).await;
    assert_eq!(server.handshakes().len(), 1);
    let requests = server.single_connection();
    assert_eq!(requests.len(), 4);
    let notes = requests[1].body_json();
    assert_eq!(notes["previous_response_id"], "work");
    assert_eq!(notes["input"].as_array().unwrap().len(), 2);
    assert_eq!(notes["input"][0]["call_id"], "work");
    assert!(
        notes["input"][1]["content"][0]["text"]
            .as_str()
            .unwrap()
            .starts_with("<AUTO_COMPACT_WORK_NOTES_REQUEST>")
    );
    let compact = requests[2].body_json();
    assert_compact_request(&compact);
    assert!(compact.get("previous_response_id").is_none());
    assert!(!compact.to_string().contains("AUTO_COMPACT_WORK_NOTES"));
    let followup = requests[3].body_json();
    assert!(followup.get("previous_response_id").is_none());
    assert!(followup.to_string().contains("summary"));
    assert!(!followup.to_string().contains("compaction_trigger"));
    test.codex.submit(Op::Shutdown).await?;
    wait_for_event(&test.codex, |e| matches!(e, EventMsg::ShutdownComplete)).await;
    let mut owners = Vec::new();
    for line in std::fs::read_to_string(test.codex.rollout_path().unwrap())?.lines() {
        if let RolloutItem::TokenUsageContext {
            info,
            measured_surface: Some(surface),
            ..
        } = serde_json::from_str::<RolloutLine>(line)?.item
            && info.last_token_usage.input_tokens > 0
        {
            owners.push((info.last_token_usage.input_tokens, surface.endpoint_sha256));
        }
    }
    assert_eq!(
        owners.iter().map(|v| v.0).collect::<Vec<_>>(),
        [28_000, 4_000, 4_001, 4_002]
    );
    assert!(owners[0].1.is_some() && owners.iter().all(|v| v.1 == owners[0].1));
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
    let configure = |config: &mut codex_core::config::Config| {
        config.model = Some("calibration-a".into());
        config.model_provider.request_max_retries = Some(0);
        config.model_provider.stream_max_retries = Some(0);
        config.model_context_window = Some(32_000);
        config.model_auto_compact_token_limit = Some(27_000);
        let overlay = config.model_overlay.get_or_insert_with(Default::default);
        overlay.cross_model.experimental_supported_tools = Some(vec!["read_file".into()]);
        overlay.cross_model.truncation_policy =
            Some(codex_protocol::openai_models::TruncationPolicyConfig::bytes(600));
        overlay
            .models
            .push(codex_core::models_manager::overlay::ModelOverlayEntry {
                slug: "calibration-b".into(),
                model_provider: None,
                final_instruction_override: None,
                patch: codex_core::models_manager::overlay::ModelInfoPatch {
                    truncation_policy: Some(
                        codex_protocol::openai_models::TruncationPolicyConfig::bytes(5_000),
                    ),
                    ..Default::default()
                },
            });
    };
    let test = test_codex().with_config(configure).build(&server).await?;
    let warm = responses::mount_response_sequence(
        &server,
        vec![
            responses::sse_response(responses::sse(vec![
                responses::ev_function_call(
                    "plan",
                    "update_plan",
                    r#"{"plan":[{"step":"Read evidence","status":"pending"}]}"#,
                ),
                responses::ev_completed_with_tokens("a", 4_000),
            ])),
            responses::sse_response(responses::sse(vec![responses::ev_completed("unused")]))
                .set_delay(Duration::from_secs(10)),
        ],
    )
    .await;
    test.codex.submit(user_input("unfinished task")).await?;
    tokio::time::timeout(Duration::from_secs(5), async {
        while warm.requests().len() < 2 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await?;
    assert_eq!(warm.requests()[1].body_json()["model"], "calibration-a");
    test.codex.submit(Op::Pause).await?;
    wait_for_event(&test.codex, |e| matches!(e, EventMsg::TurnPaused(_))).await;
    test.codex
        .submit(Op::OverrideTurnContext {
            model: Some("calibration-b".into()),
            cwd: None,
            approval_policy: None,
            sandbox_policy: None,
            windows_sandbox_level: None,
            effort: None,
            summary: None,
            collaboration_mode: None,
            personality: None,
            service_tier: None,
        })
        .await?;
    let path = test.workspace_path("evidence.txt");
    std::fs::write(&path, "unmodified trailing evidence\n".repeat(320))?;
    let mut reasoning = responses::ev_reasoning_item("reasoning", &[], &[]);
    reasoning["item"]["encrypted_content"] = json!("A".repeat(240_000));
    let initial = responses::mount_sse_sequence(
        &server,
        vec![
            responses::sse(vec![
                reasoning,
                responses::ev_function_call(
                    "work",
                    "read_file",
                    &json!({"file_path":path}).to_string(),
                ),
                responses::ev_completed_with_tokens("work", 27_100),
            ]),
            responses::sse(vec![
                responses::ev_assistant_message("notes", NOTES),
                responses::ev_completed_with_tokens("notes", 31_900),
            ]),
            responses::sse(vec![ev_compaction("discarded")]),
        ],
    )
    .await;
    test.codex.submit(Op::Continue).await?;
    wait_for_event(&test.codex, |event| matches!(event, EventMsg::Error(_))).await;
    complete(&test.codex).await;
    let sent = initial.requests();
    assert_eq!(sent.len(), 3);
    assert!(
        sent.iter()
            .all(|r| r.body_json()["model"] == "calibration-b")
    );
    let before_restart = sent[2].input();
    let admitted = before_restart
        .iter()
        .find(|i| i["type"] == "function_call_output" && i["call_id"] == "work")
        .unwrap()["output"]
        .as_str()
        .unwrap();
    assert!(admitted.contains("unmodified trailing evidence"));
    assert!(
        (1_000..7_000).contains(&admitted.len()),
        "admitted {} bytes",
        admitted.len()
    );
    let notes_input = sent[1].input();
    assert_eq!(
        &notes_input[..notes_input.len() - 1],
        &before_restart[..before_restart.len() - 1]
    );
    test.codex.submit(Op::Shutdown).await?;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::ShutdownComplete)
    })
    .await;
    let rollout = test.session_configured.rollout_path.as_ref().unwrap();
    let mut measured = Vec::new();
    for line in std::fs::read_to_string(rollout)?.lines() {
        if let RolloutItem::TokenUsageContext {
            source_model, info, ..
        } = serde_json::from_str::<RolloutLine>(line)?.item
            && info.last_token_usage.input_tokens > 0
        {
            measured.push((source_model, info.last_token_usage.input_tokens));
        }
    }
    assert_eq!(
        measured,
        vec![
            (Some("calibration-a".into()), 4_000),
            (Some("calibration-b".into()), 27_100),
            (Some("calibration-b".into()), 31_900)
        ]
    );
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
        .with_config(configure)
        .resume(&server, test.home.clone(), rollout.clone())
        .await?;
    resumed.codex.submit(Op::Continue).await?;
    complete(&resumed.codex).await;
    let requests = followup.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].body_json()["model"], "calibration-b");
    assert_eq!(requests[0].input(), before_restart);
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
            config.compaction_mode = codex_core::config::types::CompactionMode::Local;
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
