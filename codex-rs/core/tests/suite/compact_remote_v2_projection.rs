use super::*;
use anyhow::Context;
use codex_protocol::openai_models::TruncationPolicyConfig;
use core_test_support::test_codex::TestCodexBuilder;
use pretty_assertions::assert_eq;
use std::path::Path;

#[path = "compact_remote_v2_formatted.rs"]
mod formatted;

#[path = "compact_remote_v2_tool_budget.rs"]
mod tool_budget;

fn builder(cwd: &Path, window: i64, policy: TruncationPolicyConfig) -> TestCodexBuilder {
    let cwd = cwd.to_owned();
    test_codex().with_model("gpt-5.1").with_config(move |cfg| {
        cfg.cwd = cwd;
        cfg.model_context_window = Some(window);
        cfg.model_auto_compact_token_limit = Some(27_000);
        cfg.model_provider.stream_max_retries = Some(1);
        cfg.web_search_mode = Some(codex_protocol::config_types::WebSearchMode::Cached);
        cfg.features
            .enable(codex_core::features::Feature::OutputTokenReplay);
        let patch = &mut cfg
            .model_overlay
            .get_or_insert_with(Default::default)
            .cross_model;
        patch.experimental_supported_tools = Some(vec!["read_file".into()]);
        patch.truncation_policy = Some(policy);
    })
}

fn evidence(cwd: &Path) -> Result<String> {
    let path = cwd.join("evidence.txt");
    std::fs::write(&path, "sampled evidence\n".repeat(400))?;
    Ok(json!({"file_path":path}).to_string())
}

fn cut_after_output(raw: &str, id: &str) -> Result<usize> {
    let mut offset = 0;
    for line in raw.split_inclusive('\n') {
        offset += line.len();
        if matches!(serde_json::from_str::<RolloutLine>(line)?.item,
            RolloutItem::ResponseItem(ResponseItem::FunctionCallOutput {call_id,..}) if call_id == id)
        {
            return Ok(offset);
        }
    }
    anyhow::bail!("missing result for {id}")
}

#[test_case::test_case(100; "compact fits")]
#[test_case::test_case(95; "compact must shorten its tail")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn measured_text_only_image_reserves_notes_headroom_on_the_actual_projection(
    percent: i64,
) -> Result<()> {
    skip_if_no_network_unless_localhost!(Ok(()));
    let cwd = tempfile::tempdir()?;
    let args = evidence(cwd.path())?;
    let build = || {
        builder(cwd.path(), 32_000, TruncationPolicyConfig::bytes(32_000)).with_config(move |cfg| {
            let patch = &mut cfg.model_overlay.as_mut().unwrap().cross_model;
            patch.input_modalities = Some(vec![codex_protocol::openai_models::InputModality::Text]);
            patch.effective_context_window_percent = Some(percent);
        })
    };
    let replies = [
        responses::sse(vec![
            responses::ev_function_call("doc", "read_file", &args),
            responses::ev_completed_with_tokens("with-image", 28_900),
        ]),
        text_reply("notes", NOTES, 25_000),
        compact_response("image-summary"),
        text_reply("final", "Done.", 5_000),
    ];
    let server = responses::start_mock_server().await;
    let live = responses::mount_sse_sequence(&server, replies.to_vec()).await;
    let original = build().build(&server).await?;
    let image_url = "data:image/png;base64,YWJjZA==";
    original
        .codex
        .submit(Op::UserInput {
            items: vec![
                UserInput::Text {
                    text: "Read the evidence.".into(),
                    text_elements: vec![],
                },
                UserInput::Image {
                    image_url: image_url.into(),
                },
            ],
            final_output_json_schema: None,
        })
        .await?;
    complete(&original.codex).await;
    shutdown(&original.codex).await?;
    let sent = live.requests();
    assert_eq!(sent.len(), 4);
    let path = original.codex.rollout_path().unwrap();
    let raw = std::fs::read_to_string(&path)?;
    let original_output = raw
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .find(|line| {
            line["type"] == "response_item"
                && line["payload"]["type"] == "function_call_output"
                && line["payload"]["call_id"] == "doc"
        })
        .context("admitted work result")?["payload"]["output"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(raw.contains(image_url));
    for request in &sent[..3] {
        let input = serde_json::to_string(&request.input())?;
        assert!(input.contains("image content omitted") && !input.contains(image_url));
    }
    let notes = sent[1].function_call_output("doc");
    let fitted = notes["output"].as_str().context("fitted text result")?;
    assert!(
        fitted.len() > 500 && fitted.len() + 500 < original_output.len(),
        "fitted {} of {}",
        fitted.len(),
        original_output.len()
    );
    assert_compact_request(&sent[2].body_json());
    let compact = sent[2].function_call_output("doc");
    if percent == 100 {
        assert_eq!(compact["output"], original_output);
    } else {
        assert_eq!(
            compact["output"],
            "Output exceeded the available model context and was truncated"
        );
    }
    let after_output = cut_after_output(&raw, "doc")?;
    if percent == 100 {
        // The receipt covered the call, not its result. Use the captured wire items to
        // set the effective window exactly at (and one token below) the full V2 request.
        let wire_tokens = |item: &Value| -> Result<i64> {
            Ok(i64::try_from(serde_json::to_vec(item)?.len().div_ceil(4))?)
        };
        let full_window =
            28_900 + wire_tokens(&compact)? + wire_tokens(&json!({"type":"compaction_trigger"}))?;
        for (window, fits) in [(full_window, true), (full_window - 1, false)] {
            std::fs::write(&path, &raw[..after_output])?;
            let wire = responses::mount_sse_sequence(&server, replies[1..].to_vec()).await;
            let active = build()
                .with_config(move |cfg| cfg.model_context_window = Some(window))
                .resume(&server, original.home.clone(), path.clone())
                .await?;
            active.codex.submit(Op::Continue).await?;
            complete(&active.codex).await;
            shutdown(&active.codex).await?;
            let requests = wire.requests();
            assert_eq!(requests.len(), 3);
            let mut expected = sent[2].body_json();
            if !fits {
                let tail = expected["input"]
                    .as_array_mut()
                    .unwrap()
                    .iter_mut()
                    .find(|i| i["type"] == "function_call_output" && i["call_id"] == "doc")
                    .unwrap();
                tail["output"] =
                    json!("Output exceeded the available model context and was truncated");
            }
            assert!(
                requests[1].body_json() == expected,
                "complete V2 request, window {window}, fits {fits}"
            );
        }
    }
    let mut marker = after_output;
    for line in raw[after_output..].split_inclusive('\n') {
        marker += line.len();
        if line.contains("AUTO_COMPACT_WORK_NOTES_REQUEST")
            && line.contains("\"type\":\"response_item\"")
        {
            break;
        }
    }
    assert!(marker > after_output && marker < raw.len());
    for cut in [after_output, marker] {
        let mut prefix = String::new();
        for line in raw[..cut].lines() {
            let mut v: Value = serde_json::from_str(line)?;
            if cut == marker
                && let Some(decision) = v["payload"]["pending_compaction"].as_object_mut()
            {
                // Saved by a client measuring the bytes before substituting the image.
                let fit = decision.remove("source_fit").context("fit")?;
                let tools = i64::try_from(
                    serde_json::to_vec(&sent[0].body_json()["tools"])?
                        .len()
                        .div_ceil(4),
                )?;
                let offset = fit["measured"]["transcript_adjustment"]
                    .as_i64()
                    .context("work offset")?
                    + tools;
                decision.insert("source_token_adjustment".into(), json!(offset - 1_840));
                prefix.push_str(&serde_json::to_string(&v)?);
            } else {
                prefix.push_str(line);
            }
            prefix.push('\n');
        }
        std::fs::write(&path, prefix)?;
        let wire = responses::mount_sse_sequence(&server, replies[1..].to_vec()).await;
        let active = build()
            .resume(&server, original.home.clone(), path.clone())
            .await?;
        active.codex.submit(Op::Continue).await?;
        complete(&active.codex).await;
        shutdown(&active.codex).await?;
        let restarted = wire.requests();
        assert_eq!(restarted.len(), 3);
        for (a, b) in restarted.iter().zip(&sent[1..]) {
            assert!(
                a.body_json() == b.body_json(),
                "image projection changed after restart"
            );
        }
    }
    Ok(())
}

#[test_case::test_case(false; "pre-turn")]
#[test_case::test_case(true; "inline")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn notes_retry_refits_recorded_hosted_output_at_the_headroom_boundary(
    inline: bool,
) -> Result<()> {
    skip_if_no_network_unless_localhost!(Ok(()));
    let cwd = tempfile::tempdir()?;
    let args = evidence(cwd.path())?;
    let build = || builder(cwd.path(), 32_000, TruncationPolicyConfig::bytes(32_000));
    let server = responses::start_mock_server().await;
    let mut search =
        responses::ev_web_search_call_done("s1", "completed", &"query term ".repeat(620));
    search["item"]["encrypted_content"] = json!("opaque-in-progress-search");
    let late_search =
        responses::ev_web_search_call_done("s2", "completed", "trailing hosted output");
    let replies = [
        responses::sse(vec![
            responses::ev_function_call("doc", "read_file", &args),
            responses::ev_completed_with_tokens("doc-read", 6_000),
        ]),
        responses::sse(vec![
            if inline {
                responses::ev_function_call("work-plan", "update_plan", r#"{"plan":[]}"#)
            } else {
                responses::ev_assistant_message("old", "Original work done.")
            },
            responses::ev_completed_with_tokens("measured", 29_850),
        ]),
        // EOF after service output and a local call, without a completed response.
        responses::sse(vec![
            search,
            responses::ev_function_call("capture", "update_plan", r#"{"plan":[]}"#),
        ]),
        responses::sse(vec![
            responses::ev_assistant_message("notes", NOTES),
            late_search,
            responses::ev_completed("retry"),
        ]),
        compact_response("summary"),
        text_reply("final", "Finished.", 4_000),
    ];
    let live = responses::mount_sse_sequence(&server, replies.to_vec()).await;
    let original = build().build(&server).await?;
    original
        .codex
        .submit(user_input("Read the evidence."))
        .await?;
    complete(&original.codex).await;
    if !inline {
        original.codex.submit(user_input("Next task.")).await?;
        complete(&original.codex).await;
    }
    shutdown(&original.codex).await?;
    let sent = live.requests();
    assert_eq!(sent.len(), replies.len());
    let doc = sent[1].function_call_output("doc");
    assert_eq!(sent[2].function_call_output("doc"), doc);
    let retry = sent[3].body_json();
    assert!(
        retry["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["type"] == "web_search")
    );
    let full = doc["output"].as_str().context("read result")?;
    let fitted = sent[3].function_call_output("doc");
    let fitted = fitted["output"].as_str().context("fitted read result")?;
    assert!(
        full.len() > 4_000 && fitted.len() + 2_000 < full.len(),
        "before {}, after {}",
        full.len(),
        fitted.len()
    );
    let hosted = sent[3]
        .input()
        .into_iter()
        .filter(|i| i["type"] == "web_search_call")
        .collect::<Vec<_>>();
    assert_eq!(hosted.len(), 1);
    assert_eq!(hosted[0]["encrypted_content"], "opaque-in-progress-search");
    assert_eq!(
        sent[3].function_call_output("capture")["output"],
        "tool use disabled during auto-compact work-notes capture"
    );
    assert_compact_request(&sent[4].body_json());
    assert_eq!(sent[4].function_call_output("doc"), doc);
    assert!(
        !sent[4]
            .input()
            .iter()
            .any(|i| i["type"] == "web_search_call" || i["call_id"] == "capture")
    );
    assert_eq!(
        sent[5]
            .message_input_texts("user")
            .iter()
            .filter(|s| s.ends_with(NOTES))
            .count(),
        1
    );

    let path = original.codex.rollout_path().unwrap();
    let raw = std::fs::read_to_string(&path)?;
    std::fs::write(&path, &raw[..cut_after_output(&raw, "capture")?])?;
    let resumed = responses::mount_sse_sequence(&server, replies[3..].to_vec()).await;
    let active = build().resume(&server, original.home.clone(), path).await?;
    active.codex.submit(Op::Continue).await?;
    complete(&active.codex).await;
    shutdown(&active.codex).await?;
    let again = resumed.requests();
    assert_eq!(again.len(), 3);
    assert!(
        again[0].body_json() == retry,
        "near-limit notes retry changed on restart"
    );
    for (a, b) in again[1..].iter().zip(&sent[4..]) {
        assert!(
            a.body_json() == b.body_json(),
            "post-retry projection changed"
        );
    }
    Ok(())
}

#[test_case::test_case(false; "recorded output policy")]
#[test_case::test_case(true; "near limit including discarded sampled arguments")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn partial_ordinary_batch_keeps_known_results_and_their_recorded_policy_on_both_helpers(
    near_limit: bool,
) -> Result<()> {
    skip_if_no_network_unless_localhost!(Ok(()));
    let cwd = tempfile::tempdir()?;
    let args = evidence(cwd.path())?;
    let ids = ["A", "B", "C"];
    let window = if near_limit { 32_000 } else { 100_000 };
    let policy = if near_limit { 32_000 } else { 8_000 };
    let mut batch = vec![responses::ev_function_call("A", "read_file", &args)];
    for id in &ids[1..] {
        batch.push(if near_limit {
            let plan =
                json!({"plan":[{"step":"inspect evidence ".repeat(720),"status":"pending"}]});
            responses::ev_function_call(id, "update_plan", &plan.to_string())
        } else {
            responses::ev_function_call(id, "read_file", &args)
        });
    }
    let mut usage = responses::ev_completed_with_tokens("work", 27_100);
    if near_limit {
        usage["response"]["usage"] =
            json!({"input_tokens":26_000,"output_tokens":5_600,"total_tokens":31_600});
    }
    batch.push(usage);
    let replies = [
        responses::sse(batch),
        text_reply("notes", NOTES, 15_000),
        compact_response("batch-summary"),
        text_reply("final", "Done.", 5_000),
    ];
    let server = responses::start_mock_server().await;
    let live = responses::mount_sse_sequence(&server, replies.to_vec()).await;
    let original = builder(cwd.path(), window, TruncationPolicyConfig::bytes(policy))
        .build(&server)
        .await?;
    original
        .codex
        .submit(user_input("Read all three results."))
        .await?;
    complete(&original.codex).await;
    shutdown(&original.codex).await?;
    let sent = live.requests();
    assert_eq!(sent.len(), replies.len());
    let path = original.codex.rollout_path().unwrap();
    let raw = std::fs::read_to_string(&path)?;
    let result_a = raw.lines().filter_map(|line| serde_json::from_str::<RolloutLine>(line).ok()).find_map(|record| match record.item {
        RolloutItem::ResponseItem(item @ ResponseItem::FunctionCallOutput { .. }) if matches!(&item, ResponseItem::FunctionCallOutput {call_id,..} if call_id == "A") => Some(serde_json::to_value(item).unwrap()),
        _ => None,
    }).context("original sampled result A")?;
    let receipt = |records: &str| -> Result<Value> {
        for line in records.lines() {
            let record = serde_json::from_str::<Value>(line)?;
            if record["type"] == "token_usage_context"
                && record["payload"]["info"]["last_token_usage"]["total_tokens"] == 31_600
            {
                return Ok(record["payload"].clone());
            }
        }
        anyhow::bail!("missing positive work receipt")
    };
    let mut prefix = raw.clone();
    // The second near-limit pass starts from a file already cleaned by the first /continue.
    let cuts = if near_limit { ["A", "A"] } else { ["A", "B"] };
    for id in cuts {
        let index = ids.iter().position(|&i| i == id).unwrap();
        let kept = &ids[..=index];
        let missing = &ids[index + 1..];
        std::fs::write(&path, &prefix[..cut_after_output(&prefix, id)?])?;
        let resumed = responses::mount_sse_sequence(&server, replies[1..].to_vec()).await;
        // Missing admission metadata would shorten known results on the following restart.
        let active = builder(cwd.path(), window, TruncationPolicyConfig::bytes(320))
            .resume(&server, original.home.clone(), path.clone())
            .await?;
        active.codex.submit(Op::Continue).await?;
        complete(&active.codex).await;
        shutdown(&active.codex).await?;
        let again = resumed.requests();
        assert_eq!(again.len(), 3);
        for (a, b) in again[..2].iter().zip(&sent[1..3]) {
            let mut golden = b.body_json();
            golden["input"].as_array_mut().unwrap().retain(|i| {
                !i["call_id"]
                    .as_str()
                    .is_some_and(|id| missing.contains(&id))
            });
            if near_limit {
                let projected = golden["input"]
                    .as_array_mut()
                    .unwrap()
                    .iter_mut()
                    .find(|i| i["type"] == "function_call_output" && i["call_id"] == "A")
                    .unwrap();
                *projected = result_a.clone();
            }
            assert!(
                a.body_json() == golden,
                "helper after {id} lost completed work"
            );
            for &id in kept {
                assert!(a.function_call_output(id)["output"].as_str().unwrap().len() > 1_000);
            }
        }
        let clean = std::fs::read_to_string(&path)?;
        let mut policies = Vec::new();
        let mut results = Vec::new();
        for line in clean.lines() {
            match serde_json::from_str::<RolloutLine>(line)?.item {
                RolloutItem::ToolOutputPolicy {
                    call_id,
                    policy: saved,
                } => {
                    assert_eq!(
                        saved,
                        codex_protocol::protocol::TruncationPolicy::Bytes(policy as usize)
                    );
                    policies.push(call_id);
                }
                RolloutItem::ResponseItem(ResponseItem::FunctionCallOutput { call_id, .. }) => {
                    results.push(call_id)
                }
                RolloutItem::Compacted(_) => break,
                _ => {}
            }
        }
        assert_eq!(policies, kept);
        assert_eq!(results, kept);
        if near_limit {
            let original_receipt = receipt(&raw)?;
            assert_eq!(
                original_receipt["info"]["last_token_usage"]["output_tokens"],
                5_600
            );
            assert!(
                original_receipt["measured_item_tokens"]
                    .as_i64()
                    .is_some_and(|cost| cost > 5_600)
            );
            assert_eq!(receipt(&clean)?, original_receipt);
            prefix = clean;
        }
    }
    Ok(())
}
