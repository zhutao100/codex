use super::*;
use core_test_support::responses::ResponsesRequest;
use pretty_assertions::assert_eq;

fn json_tokens(value: &Value) -> Result<i64> {
    Ok(serde_json::to_vec(value)?.len().div_ceil(4) as i64)
}

// All fixture items are text, without images, ciphertext or provider-side tools.
fn request_cost(request: &ResponsesRequest) -> Result<i64> {
    let mut count = request.instructions_text().len().div_ceil(4) as i64;
    for item in request.input().iter().skip(1) {
        count += match item["type"].as_str() {
            Some("function_call") => (item["name"].as_str().context("name")?.len()
                + item["arguments"].as_str().context("args")?.len())
            .div_ceil(4) as i64,
            Some("message") if item["role"] == "assistant" => item["content"]
                .as_array()
                .context("text")?
                .iter()
                .filter_map(|c| c["text"].as_str())
                .map(str::len)
                .sum::<usize>()
                .div_ceil(4) as i64,
            _ => json_tokens(item)?,
        };
    }
    Ok(count)
}

#[test_case::test_case(false; "catalog enables parallel tool calls")]
#[test_case::test_case(true; "catalog withdraws parallel tool calls")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn same_slug_provider_rebind_keeps_the_receipt_without_applying_it(
    original_parallel: bool,
) -> Result<()> {
    skip_if_no_network_unless_localhost!(Ok(()));
    const MODEL: &str = "gpt-5.1";
    let cwd = tempfile::tempdir()?;
    let args = evidence(cwd.path())?;
    let build = |window, selected: &'static str, first: String, second: String, supports: bool| {
        builder(cwd.path(), window, TruncationPolicyConfig::bytes(32_000)).with_config(move |cfg| {
            for (key, url) in [("first", first), ("second", second)] {
                let mut provider = cfg.model_provider.clone();
                provider.base_url = Some(url);
                cfg.model_providers.insert(key.into(), provider);
            }
            let overlay = cfg.model_overlay.as_mut().unwrap();
            overlay.cross_model.effective_context_window_percent = Some(100);
            overlay.cross_model.supports_reasoning_summaries = Some(supports);
            overlay.cross_model.default_reasoning_level =
                Some(Some(codex_protocol::openai_models::ReasoningEffort::Low));
            overlay.cross_model.support_verbosity = Some(supports);
            overlay.cross_model.supports_parallel_tool_calls = Some(if supports {
                original_parallel
            } else {
                !original_parallel
            });
            cfg.model_verbosity = Some(codex_protocol::config_types::Verbosity::High);
            overlay.models.push(ModelOverlayEntry {
                slug: MODEL.into(),
                model_provider: Some(selected.into()),
                patch: Default::default(),
                final_instruction_override: None,
            });
        })
    };
    let replies = [
        responses::sse(vec![
            responses::ev_function_call("doc", "read_file", &args),
            responses::ev_completed_with_tokens("work", 31_800),
        ]),
        text_reply("notes", NOTES, 5_000),
        compact_response("summary"),
        text_reply("done", "Done.", 2_000),
    ];
    let original = responses::start_mock_server().await;
    let other = responses::start_mock_server().await;
    let capture = responses::mount_sse_sequence(&original, replies.to_vec()).await;
    let live = build(120_000, "first", original.uri(), other.uri(), true)
        .build(&original)
        .await?;
    live.codex
        .submit(user_input("Inspect the evidence."))
        .await?;
    complete(&live.codex).await;
    shutdown(&live.codex).await?;
    let captured = capture.requests();
    assert_eq!(captured.len(), 4);
    let full = captured[2].function_call_output("doc");
    assert_eq!(captured[0].body_json()["reasoning"]["effort"], "low");
    assert_eq!(captured[0].body_json()["text"]["verbosity"], "high");
    assert_eq!(
        captured[0].body_json()["parallel_tool_calls"],
        original_parallel
    );
    assert!(full.to_string().contains("sampled evidence"));
    let path = live.codex.rollout_path().context("rollout")?;
    let raw = std::fs::read_to_string(&path)?;
    let before = cut_after_output(&raw, "doc")?;
    let mut after = 0;
    let mut sampled_controls = None;
    for line in raw.split_inclusive('\n') {
        after += line.len();
        let v: Value = serde_json::from_str(line)?;
        let p = &v["payload"];
        if v["type"] == "token_usage_context" {
            assert_eq!(p["source_model"], MODEL);
            assert_eq!(p["measured_surface"]["provider"], "first");
            if p["info"]["last_token_usage"]["input_tokens"] == 5_000 {
                break;
            }
        }
        if let Some(decision) = p["pending_compaction"].as_object() {
            assert_eq!(decision["fit_route"]["provider"], "first");
            assert_eq!(
                decision["fit_route"]["endpoint_sha256"]
                    .as_str()
                    .map(str::len),
                Some(64)
            );
            assert_eq!(
                decision["source"]["sampled_controls"]["route"],
                decision["fit_route"]
            );
            assert!(decision["source_fit"]["measured"].is_object());
            sampled_controls = Some(decision["source"]["sampled_controls"].clone());
        }
    }
    assert!(before < after && after < raw.len());
    let sampled_controls = sampled_controls.context("initial sampled controls")?;
    assert_eq!(sampled_controls["parallel_tool_calls"], original_parallel);
    for (cut, next) in [(before, 1), (after, 2)] {
        for (new_key, new_endpoint, legacy) in [
            (false, false, false),
            (true, false, false),
            (false, true, false),
            (true, true, false),
            (false, false, true),
        ] {
            let local = new_key || new_endpoint || legacy;
            if legacy {
                let mut unowned = String::new();
                for line in raw[..cut].lines() {
                    let mut v: Value = serde_json::from_str(line)?;
                    if let Some(p) = v["payload"].as_object_mut() {
                        if let Some(surface) =
                            p.get_mut("measured_surface").and_then(Value::as_object_mut)
                        {
                            surface.remove("endpoint_sha256"); // The older record still identifies the key.
                        }
                        if let Some(s) = p
                            .get_mut("last_work_settings")
                            .and_then(Value::as_object_mut)
                        {
                            s.remove("sampled_controls");
                        }
                        if let Some(d) = p
                            .get_mut("pending_compaction")
                            .and_then(Value::as_object_mut)
                        {
                            d.remove("fit_route");
                            d.insert("fit_provider".into(), json!("first"));
                            if let Some(s) = d.get_mut("source").and_then(Value::as_object_mut) {
                                s.remove("sampled_controls");
                            }
                        }
                    }
                    unowned.push_str(&serde_json::to_string(&v)?);
                    unowned.push('\n');
                }
                std::fs::write(&path, unowned)?;
            } else {
                std::fs::write(&path, &raw[..cut])?;
            }
            let (route, unused) = if new_endpoint {
                (&other, &original)
            } else {
                (&original, &other)
            };
            let unrelated_posts = responses::model_post_count(unused).await?;
            let wire = responses::mount_sse_sequence(route, replies[next..].to_vec()).await;
            let active = if new_key {
                build(32_000, "second", original.uri(), route.uri(), false)
            } else {
                build(32_000, "first", route.uri(), other.uri(), false)
            }
            .resume(&original, live.home.clone(), path.clone())
            .await?;
            active.codex.submit(Op::Continue).await?;
            complete(&active.codex).await;
            shutdown(&active.codex).await?;
            let requests = wire.requests();
            assert_eq!(requests.len(), 4 - next);
            assert_eq!(responses::model_post_count(unused).await?, unrelated_posts);
            for helper in &requests[..requests.len() - 1] {
                assert_eq!(helper.body_json()["model"], MODEL);
                assert_eq!(
                    helper.body_json()["reasoning"],
                    if local {
                        Value::Null
                    } else {
                        captured[0].body_json()["reasoning"].clone()
                    }
                );
                assert_eq!(
                    helper.body_json()["text"]["verbosity"],
                    if local { Value::Null } else { json!("high") }
                );
                assert_eq!(
                    helper.body_json()["parallel_tool_calls"],
                    local && !original_parallel,
                    "key={new_key}, endpoint={new_endpoint}, legacy={legacy}, cut={cut}"
                );
                assert_eq!(
                    helper.function_call_output("doc") == full,
                    local,
                    "key={new_key}, endpoint={new_endpoint}, legacy={legacy}, cut={cut}"
                );
            }
            let destination = requests.last().context("destination")?.body_json();
            assert_eq!(destination["parallel_tool_calls"], !original_parallel);
            assert!(
                destination["reasoning"].is_null() && destination["text"]["verbosity"].is_null()
            );
            let saved = std::fs::read_to_string(&path)?;
            let decision = saved
                .lines()
                .filter_map(|s| serde_json::from_str::<Value>(s).ok())
                .filter_map(|r| r["payload"]["pending_compaction"].as_object().cloned())
                .next_back()
                .context("recovered fit")?;
            // A different current route uses local sizing, but cannot erase the
            // original endpoint's qualified correction from the recovery journal.
            assert_eq!(
                decision["source_fit"] == json!("local"),
                legacy,
                "key={new_key}, endpoint={new_endpoint}, legacy={legacy}, cut={cut}"
            );
            assert_eq!(
                decision
                    .get("fit_route")
                    .and_then(|r| r["provider"].as_str()),
                (!legacy).then_some("first")
            );
            if !legacy {
                assert_eq!(
                    decision["source"]["sampled_controls"], sampled_controls,
                    "key={new_key}, endpoint={new_endpoint}, cut={cut}"
                );
            }
        }
    }
    assert_eq!(capture.requests().len(), 4);
    Ok(())
}

#[test_case::test_case(true, true; "stable large inventory")]
#[test_case::test_case(false, true; "MCP schemas added")]
#[test_case::test_case(true, false; "MCP schemas removed")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn helper_budgets_price_the_current_mcp_inventory(old: bool, new: bool) -> Result<()> {
    skip_if_no_network_unless_localhost!(Ok(()));
    let cwd = tempfile::tempdir()?;
    let args = evidence(cwd.path())?;
    let build = |window| {
        builder(cwd.path(), window, TruncationPolicyConfig::bytes(32_000)).with_config(|cfg| {
            cfg.model_overlay
                .as_mut()
                .unwrap()
                .cross_model
                .effective_context_window_percent = Some(100);
        })
    };
    let inventory = responses::start_mock_server().await;
    let names: Vec<_> = (0..100).map(|i| format!("field_{i}")).collect();
    responses::mount_app_catalog(
        &inventory,
        &names.iter().map(String::as_str).collect::<Vec<_>>(),
    )
    .await;
    let refresh =
        |on| responses::refresh_catalog_servers(&inventory, if on { &["inventory"] } else { &[] });
    let replies = [
        responses::sse(vec![
            responses::ev_function_call("doc", "read_file", &args),
            responses::ev_completed_with_tokens("work", 45_000),
        ]),
        text_reply("notes", NOTES, 5_000),
        compact_response("summary"),
        text_reply("done", "Done.", 2_000),
    ];
    let server = responses::start_mock_server().await;
    let traffic = responses::mount_sse_sequence(&server, replies.to_vec()).await;
    let live = build(120_000).build(&server).await?;
    live.codex.submit(refresh(old)).await?;
    live.codex
        .submit(user_input(&"Inspect the evidence.\n".repeat(6_000)))
        .await?;
    complete(&live.codex).await;
    shutdown(&live.codex).await?;
    let sent = traffic.requests();
    assert_eq!(sent.len(), 4);
    let old_tools = &sent[0].body_json()["tools"];
    assert_eq!(old_tools.to_string().contains("probe_field_0"), old);
    let old_tokens = json_tokens(old_tools)?;
    let path = live.codex.rollout_path().unwrap();
    let raw = std::fs::read_to_string(&path)?;
    let cut = &raw[..cut_after_output(&raw, "doc")?];
    std::fs::write(&path, cut)?;
    let probe = responses::mount_sse_sequence(&server, replies[1..].to_vec()).await;
    let active = build(120_000)
        .resume(&server, live.home.clone(), path.clone())
        .await?;
    active.codex.submit(refresh(new)).await?;
    active.codex.submit(Op::Continue).await?;
    complete(&active.codex).await;
    shutdown(&active.codex).await?;
    let tools = probe.requests()[0].body_json()["tools"].clone();
    assert_eq!(tools.to_string().contains("probe_field_0"), new);
    let tool_tokens = json_tokens(&tools)?;
    if old != new {
        assert!(tool_tokens.abs_diff(old_tokens) > 2_000);
    }
    let tail = sent[2].function_call_output("doc");
    let covered = request_cost(&sent[2])?
        - json_tokens(&json!({"type":"compaction_trigger"}))?
        - json_tokens(&tail)?;
    assert!(covered > 27_000);
    for measured in [false, true] {
        let mut prefix = String::new();
        for line in cut.lines() {
            let mut v: Value = serde_json::from_str(line)?;
            let p = &mut v["payload"];
            if p["info"]["last_token_usage"]["input_tokens"] == 45_000 {
                if p.get("measured_tool_tokens").is_some() {
                    assert_eq!(p["measured_tool_tokens"], old_tokens);
                }
                if measured {
                    // A real zero correction: receipt equals the locally sized covered work.
                    for field in ["input_tokens", "total_tokens"] {
                        p["info"]["last_token_usage"][field] = json!(covered);
                        let before = p["info"]["total_token_usage"][field]
                            .as_i64()
                            .context("total")?;
                        p["info"]["total_token_usage"][field] = json!(before + covered - 45_000);
                    }
                } else {
                    // An older record may know the model but not the original tools.
                    p.as_object_mut().context("record")?.remove(if old == new {
                        "source_model"
                    } else {
                        "measured_tool_tokens"
                    });
                }
            }
            prefix.push_str(&serde_json::to_string(&v)?);
            prefix.push('\n');
        }
        for (index, notes) in [(0, true), (1, false)] {
            let full = request_cost(&sent[index + 1])? + tool_tokens
                - if measured { old_tokens } else { 0 };
            let exact = if notes {
                (full..=full + 4_096)
                    .find(|w| w - (w / 20).clamp(256, 4_096) >= full)
                    .context("notes window")?
            } else {
                full
            };
            for overflow in [0, 1] {
                std::fs::write(&path, &prefix)?;
                let wire = responses::mount_sse_sequence(&server, replies[1..].to_vec()).await;
                let active = build(exact - overflow)
                    .resume(&server, live.home.clone(), path.clone())
                    .await?;
                active.codex.submit(refresh(new)).await?;
                active.codex.submit(Op::Continue).await?;
                complete(&active.codex).await;
                shutdown(&active.codex).await?;
                let reqs = wire.requests();
                assert_eq!(reqs.len(), 3);
                let helper = &reqs[index];
                assert_eq!(helper.body_json()["tools"], tools);
                let actual = helper.function_call_output("doc");
                assert_eq!(
                    actual == tail,
                    overflow == 0,
                    "measured={measured}, notes={notes}, window={}, full={full}",
                    exact - overflow
                );
                let saved = std::fs::read_to_string(&path)?;
                let decision = saved
                    .lines()
                    .filter_map(|s| serde_json::from_str::<Value>(s).ok())
                    .find_map(|v| {
                        v["payload"]["pending_compaction"]
                            .get("source_fit")
                            .cloned()
                    })
                    .context("fit owner")?;
                assert_eq!(
                    decision,
                    if measured {
                        json!({"measured":{"transcript_adjustment": -old_tokens}})
                    } else {
                        json!("local")
                    }
                );
                if !notes && overflow == 1 {
                    // The notes receipt is not a new measurement of the original work. Older
                    // journals wrote 0 for an unmeasured source as well as for a measured zero.
                    let mut after_notes = String::new();
                    let mut saw_receipt = false;
                    for line in saved.lines() {
                        let mut record: Value = serde_json::from_str(line)?;
                        if let Some(d) = record["payload"]
                            .get_mut("pending_compaction")
                            .and_then(Value::as_object_mut)
                        {
                            d.remove("source_fit");
                            d.insert("source_token_adjustment".into(), json!(0));
                        }
                        after_notes.push_str(&serde_json::to_string(&record)?);
                        after_notes.push('\n');
                        if record["type"] == "token_usage_context"
                            && record["payload"]["info"]["last_token_usage"]["input_tokens"]
                                == 5_000
                        {
                            saw_receipt = true;
                            break;
                        }
                    }
                    assert!(saw_receipt);
                    std::fs::write(&path, after_notes)?;
                    let resumed =
                        responses::mount_sse_sequence(&server, replies[2..].to_vec()).await;
                    let active = build(exact - overflow)
                        .resume(&server, live.home.clone(), path.clone())
                        .await?;
                    active.codex.submit(refresh(new)).await?;
                    active.codex.submit(Op::Continue).await?;
                    complete(&active.codex).await;
                    shutdown(&active.codex).await?;
                    let resumed = resumed.requests();
                    assert_eq!(resumed.len(), 2);
                    assert!(
                        resumed[0].body_json() == helper.body_json(),
                        "changed compact after notes, measured={measured}"
                    );
                }
            }
        }
    }
    Ok(())
}
