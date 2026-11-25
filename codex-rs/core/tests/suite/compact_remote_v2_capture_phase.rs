use super::*;
use anyhow::Context;
use codex_protocol::protocol::SandboxPolicy;
use pretty_assertions::assert_eq;
use std::path::Path;
use wiremock::MockServer;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn admitted_prompt_crossing_the_trigger_is_not_compacted_on_restart() -> Result<()> {
    skip_if_no_network_unless_localhost!(Ok(()));
    let cwd = tempfile::tempdir()?;
    let home = std::sync::Arc::new(tempfile::tempdir()?);
    let skill_dir = home.path().join("skills").join("inspect");
    std::fs::create_dir_all(&skill_dir)?;
    let skill = skill_dir.join("SKILL.md");
    std::fs::write(
        &skill,
        "---\nname: inspect\ndescription: Inspect input.\n---\n\nKeep the exact selected skill.\n",
    )?;
    let builder = || {
        let cwd = cwd.path().to_owned();
        test_codex().with_model("gpt-5.1").with_config(move |cfg| {
            cfg.cwd = cwd;
            cfg.model_context_window = Some(100_000);
            cfg.model_auto_compact_token_limit = Some(40_000);
        })
    };
    let server = responses::start_mock_server().await;
    let final_reply = text_reply("new", "New task done.", 41_500);
    let live = responses::mount_sse_sequence(
        &server,
        vec![
            text_reply("old", "Old task done.", 39_900),
            final_reply.clone(),
        ],
    )
    .await;
    let original = builder().with_home(home.clone()).build(&server).await?;
    original
        .codex
        .submit(user_input("Complete the old task."))
        .await?;
    complete(&original.codex).await;
    let fresh = format!("Use $inspect. {}", "Fresh evidence. ".repeat(400));
    original
        .codex
        .submit(Op::UserInput {
            items: vec![
                UserInput::Text {
                    text: fresh.clone(),
                    text_elements: vec![],
                },
                UserInput::Skill {
                    name: "inspect".into(),
                    path: skill.canonicalize()?,
                },
            ],
            final_output_json_schema: None,
        })
        .await?;
    complete(&original.codex).await;
    shutdown(&original.codex).await?;
    let sent = live.requests();
    assert_eq!(sent.len(), 2);
    let golden = sent[1].body_json();
    let work = sent[0].input();
    let expanded = sent[1].input();
    assert!(expanded.starts_with(&work));
    assert!(
        expanded[work.len()..]
            .iter()
            .any(|i| i["content"][0]["text"] == fresh)
    );
    assert_eq!(
        sent[1]
            .message_input_texts("user")
            .iter()
            .filter(|s| s.starts_with("<skill>"))
            .count(),
        1
    );
    // The new user text alone crosses 40k; even the entire appended JSON fits well below 100k.
    assert!(39_900 + fresh.len() / 4 > 40_000);
    assert!(39_900 + serde_json::to_vec(&expanded[work.len()..])?.len() < 90_000);
    let path = original.codex.rollout_path().unwrap();
    let raw = std::fs::read_to_string(&path)?;
    let mut cuts = Vec::new();
    let mut prepared = false;
    let mut offset = 0;
    for line in raw.split_inclusive('\n') {
        offset += line.len();
        match serde_json::from_str::<RolloutLine>(line)?.item {
            RolloutItem::PostTurnCompletionReviewState(s)
                if s["unsampled_fresh_model"] == "gpt-5.1" =>
            {
                if !prepared && s["pending_inputs"][0]["content"][0]["text"] == fresh {
                    cuts.push(("prepared", offset));
                    prepared = true;
                } else if prepared && s["pending_inputs"].as_array().is_some_and(Vec::is_empty) {
                    cuts.push(("consumed", offset));
                    break;
                }
            }
            RolloutItem::ResponseItem(ResponseItem::Message { content, .. }) if prepared => {
                if let [ContentItem::InputText { text }] = &content[..] {
                    if *text == fresh {
                        cuts.push(("body", offset));
                    }
                    if text.starts_with("<skill>") {
                        cuts.push(("skill", offset));
                    }
                }
            }
            _ => {}
        }
    }
    assert_eq!(
        cuts.iter().map(|(label, _)| *label).collect::<Vec<_>>(),
        ["prepared", "body", "skill", "consumed"]
    );
    for (label, offset) in cuts {
        std::fs::write(&path, &raw[..offset])?;
        let server = responses::start_mock_server().await;
        let replay = responses::mount_sse_once(&server, final_reply.clone()).await;
        let test = builder()
            .resume(&server, home.clone(), path.clone())
            .await?;
        test.codex.submit(Op::Continue).await?;
        complete(&test.codex).await;
        shutdown(&test.codex).await?;
        assert!(
            replay.single_request().body_json() == golden,
            "ordinary request after {label}"
        );
    }
    Ok(())
}

async fn mount_policy_probe(server: &MockServer) {
    responses::mount_app_catalog_with_capabilities(
        server,
        &["Probe"],
        json!({"tools":{}, "experimental":{codex_core::MCP_SANDBOX_STATE_CAPABILITY:{}}}),
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn completed_notes_item_goes_directly_to_compact_after_disconnect() -> Result<()> {
    skip_if_no_network_unless_localhost!(Ok(()));
    let cwd = tempfile::tempdir()?;
    let builder = || {
        let cwd = cwd.path().to_owned();
        test_codex().with_model("gpt-5.1").with_config(move |cfg| {
            cfg.cwd = cwd;
            cfg.model_context_window = Some(100_000);
            cfg.model_auto_compact_token_limit = Some(40_000);
            cfg.model_provider.stream_max_retries = Some(1);
        })
    };
    let replies = [
        responses::sse(vec![
            responses::ev_function_call("work", "update_plan", r#"{"plan":[]}"#),
            responses::ev_completed_with_tokens("work", 41_000),
        ]),
        responses::sse(vec![
            responses::ev_function_call("capture", "update_plan", r#"{"plan":[]}"#),
            responses::ev_assistant_message("tagged", NOTES),
        ]),
        compact_response("from-original-work"),
        text_reply("final", "Done.", 5_000),
    ];
    let server = responses::start_mock_server().await;
    let live = responses::mount_sse_sequence(&server, replies.to_vec()).await;
    let original = builder().build(&server).await?;
    original
        .codex
        .submit(user_input("Continue the old task."))
        .await?;
    complete(&original.codex).await;
    shutdown(&original.codex).await?;
    let sent = live.requests();
    assert_eq!(sent.len(), replies.len());
    assert_compact_request(&sent[2].body_json());
    assert!(!sent[2].input().iter().any(|i| i["call_id"] == "capture"));
    assert_eq!(
        sent[3]
            .message_input_texts("user")
            .iter()
            .filter(|s| s.ends_with(NOTES))
            .count(),
        1
    );
    let path = original.codex.rollout_path().unwrap();
    let raw = std::fs::read_to_string(&path)?;
    let mut cut = 0;
    for line in raw.split_inclusive('\n') {
        cut += line.len();
        if matches!(serde_json::from_str::<RolloutLine>(line)?.item, RolloutItem::ResponseItem(ResponseItem::Message {content,..}) if matches!(&content[..], [ContentItem::OutputText{text,..}] if text == NOTES))
        {
            break;
        }
    }
    assert!(cut < raw.len(), "tagged item before its response receipt");
    std::fs::write(&path, &raw[..cut])?;
    let server = responses::start_mock_server().await;
    let traffic = responses::mount_sse_sequence(&server, replies[2..].to_vec()).await;
    let replay = builder()
        .resume(&server, original.home.clone(), path)
        .await?;
    replay.codex.submit(Op::Continue).await?;
    complete(&replay.codex).await;
    shutdown(&replay.codex).await?;
    let requests = traffic.requests();
    assert_eq!(requests.len(), 2);
    for (actual, expected) in requests.iter().zip(&sent[2..]) {
        assert!(actual.body_json() == expected.body_json());
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum CaptureRetry {
    Exhausted,
    MultipleMessages,
    Disconnect,
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tagged_notes_own_only_same_response_calls_and_replay_legacy_shell_pairing() -> Result<()> {
    skip_if_no_network_unless_localhost!(Ok(()));
    let cwd = tempfile::tempdir()?;
    let sentinel = cwd.path().join("capture-tool-ran");
    let build = |server: &MockServer| {
        let cwd = cwd.path().to_owned();
        let catalog = server.uri();
        test_codex()
            .with_auth(CodexAuth::create_dummy_chatgpt_auth_for_testing())
            .with_model("gpt-5.1")
            .with_config(move |cfg| {
                cfg.cwd = cwd;
                cfg.features.enable(codex_core::features::Feature::Apps);
                cfg.chatgpt_base_url = catalog;
                cfg.model_context_window = Some(100_000);
                cfg.model_auto_compact_token_limit = Some(40_000);
            })
    };
    let legacy_shell = |id: &str, command| {
        let mut ev = responses::ev_local_shell_call(id, "completed", command);
        ev["item"].as_object_mut().unwrap().remove("call_id");
        ev["item"]["id"] = json!(id);
        ev
    };
    let mut tag = responses::ev_assistant_message("notes", NOTES);
    tag["item"]["phase"] = json!("commentary");
    let replies = [
        responses::sse(vec![
            // Exercise the actual ordinary dispatcher without spawning a host shell.
            legacy_shell("ordinary", vec![]),
            responses::ev_completed_with_tokens("work", 41_000),
        ]),
        responses::sse(vec![
            tag,
            legacy_shell("denied", vec!["/usr/bin/touch", sentinel.to_str().unwrap()]),
            responses::ev_function_call("also-denied", "update_plan", r#"{"plan":[]}"#),
            responses::ev_completed_with_tokens("captured", 36_000),
        ]),
        compact_response("tagged-summary"),
        text_reply("final", "Done.", 5_000),
    ];
    let server = responses::start_mock_server().await;
    responses::mount_app_catalog(&server, &["Calendar"]).await;
    let live = responses::mount_sse_sequence(&server, replies.to_vec()).await;
    let original = build(&server).build(&server).await?;
    original
        .codex
        .submit(Op::UserInput {
            items: vec![
                UserInput::Text {
                    text: "Run the ordinary shell.".into(),
                    text_elements: vec![],
                },
                UserInput::Mention {
                    name: "Calendar".into(),
                    path: "app://Calendar".into(),
                },
            ],
            final_output_json_schema: None,
        })
        .await?;
    complete(&original.codex).await;
    shutdown(&original.codex).await?;
    let sent = live.requests();
    assert_eq!(sent.len(), 4);
    assert!(!sentinel.exists());
    let result = sent[1].function_call_output("ordinary");
    assert!(
        sent[0].body_json()["tools"]
            .to_string()
            .contains("probe_Calendar")
    );
    assert!(
        !sent[0]
            .message_input_texts("user")
            .iter()
            .any(|s| s.contains("Calendar"))
    );
    assert_eq!(sent[0].body_json()["tools"], sent[2].body_json()["tools"]);
    assert!(
        result["output"]
            .as_str()
            .unwrap()
            .contains("command args are empty"),
        "actual ordinary result: {result}"
    );
    assert_eq!(sent[2].function_call_output("ordinary"), result);
    assert_compact_request(&sent[2].body_json());
    assert_eq!(
        sent[3]
            .message_input_texts("user")
            .iter()
            .filter(|s| s.ends_with(NOTES))
            .count(),
        1
    );
    let path = original.codex.rollout_path().unwrap();
    let raw = std::fs::read_to_string(&path)?;
    let mut cuts = Vec::new();
    let mut offset = 0;
    let mut claims = 0;
    let mut tagged_call_cut = None;
    for line in raw.split_inclusive('\n') {
        offset += line.len();
        let v: Value = serde_json::from_str(line)?;
        let p = &v["payload"];
        match (v["type"].as_str(), p["type"].as_str()) {
            (Some("response_item"), Some("local_shell_call")) => {
                assert!(p.get("id").is_none());
                if p["call_id"] == "denied" {
                    tagged_call_cut = Some(offset);
                    cuts.push((offset, 2, 1));
                } else {
                    assert_eq!(p["call_id"], "ordinary");
                }
            }
            (Some("response_item"), Some("function_call_output")) if p["call_id"] == "ordinary" => {
                cuts.push((offset, 1, 2))
            }
            (Some("response_item"), Some("message")) if p["content"][0]["text"] == NOTES => {
                cuts.push((offset, 2, 0))
            }
            (Some("post_turn_completion_review_state"), _) => {
                if let Some(entries) = p
                    .pointer("/pending_compaction/tagged_calls")
                    .and_then(Value::as_array)
                    && entries.len() > claims
                {
                    claims = entries.len();
                    cuts.push((offset, 2, claims - 1));
                }
            }
            (Some("response_item"), Some("function_call")) if p["call_id"] == "also-denied" => {
                cuts.push((offset, 2, 2))
            }
            (Some("response_item"), Some("function_call_output"))
                if p["call_id"] == "denied" || p["call_id"] == "also-denied" =>
            {
                assert_eq!(
                    p["output"],
                    "tool use disabled during auto-compact work-notes capture"
                );
                cuts.push((offset, 2, 2));
            }
            (Some("compacted"), _) => break,
            _ => {}
        }
    }
    assert_eq!((claims, cuts.len()), (2, 8));
    for (cut, next, expected_denials) in cuts {
        std::fs::write(&path, &raw[..cut])?;
        let server = responses::start_mock_server().await;
        responses::mount_app_catalog(&server, &["Calendar"]).await;
        let wire = responses::mount_sse_sequence(&server, replies[next..].to_vec()).await;
        let active = build(&server)
            .resume(&server, original.home.clone(), path.clone())
            .await?;
        active.codex.submit(Op::Continue).await?;
        wait_for_event(&active.codex, |event| match event {
            EventMsg::Error(error) => panic!(
                "cut {cut}, expecting {expected_denials} denials: {}",
                error.message
            ),
            EventMsg::TurnComplete(_) => true,
            _ => false,
        })
        .await;
        shutdown(&active.codex).await?;
        let again = wire.requests();
        assert_eq!(again.len(), replies.len() - next);
        for (a, b) in again.iter().zip(&sent[next..]) {
            assert!(
                a.body_json() == b.body_json(),
                "shell work or tagged replacement changed at cut {cut}"
            );
        }
        let saved = std::fs::read_to_string(&path)?;
        let denials = saved
            .lines()
            .map(serde_json::from_str::<Value>)
            .collect::<serde_json::Result<Vec<_>>>()?
            .into_iter()
            .filter(|v| {
                v["type"] == "response_item"
                    && v["payload"]["type"] == "function_call_output"
                    && v["payload"]["output"]
                        == "tool use disabled during auto-compact work-notes capture"
            })
            .count();
        assert_eq!(denials, expected_denials);
        assert!(!sentinel.exists());
    }
    // The source's tagged call has no denial or completion yet. A full fork must
    // resolve that copied transaction, without taking over the parent's turn.
    let copy = &raw[..tagged_call_cut.context("tagged shell call")?];
    std::fs::write(&path, copy)?;
    for restart in [false, true] {
        server.reset().await;
        responses::mount_app_catalog(&server, &["Calendar"]).await;
        let wire = responses::mount_sse_sequence(&server, replies[2..].to_vec()).await;
        let fork = original
            .thread_manager
            .fork_thread(usize::MAX, original.config.clone(), path.clone())
            .await?
            .thread;
        let child_path = fork.rollout_path().context("fork rollout")?;
        let child = if restart {
            shutdown(&fork).await?;
            build(&server)
                .resume(&server, original.home.clone(), child_path.clone())
                .await?
                .codex
        } else {
            fork
        };
        child
            .submit(user_input("Finish the new fork task."))
            .await?;
        complete(&child).await;
        shutdown(&child).await?;
        let requests = wire.requests();
        assert_eq!(requests.len(), 2);
        let trigger = requests[0].body_json();
        assert_compact_request(&trigger);
        let mut expected = sent[2].body_json();
        // The stable base-instruction ID is scoped to the child thread.
        assert_ne!(trigger["input"][0]["id"], expected["input"][0]["id"]);
        expected["input"][0]["id"] = trigger["input"][0]["id"].clone();
        assert!(trigger["input"] == expected["input"]);
        assert_eq!(trigger["tools"], expected["tools"]);
        assert!(
            !requests[1].body_json()["tools"]
                .to_string()
                .contains("probe_Calendar")
        );
        assert!(!trigger["input"].to_string().contains("new fork task"));
        let users = requests[1].message_input_texts("user");
        assert_eq!(users.iter().filter(|s| s.ends_with(NOTES)).count(), 1);
        assert_eq!(
            users
                .iter()
                .filter(|s| *s == "Finish the new fork task.")
                .count(),
            1
        );
        let child_log = std::fs::read_to_string(child_path)?;
        let denials = child_log
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .filter(|v| {
                v["type"] == "response_item"
                    && v["payload"]["call_id"] == "denied"
                    && v["payload"]["output"]
                        == "tool use disabled during auto-compact work-notes capture"
            })
            .count();
        assert_eq!(denials, 1);
        assert_eq!(std::fs::read_to_string(&path)?, copy);
        assert!(!sentinel.exists());
    }
    Ok(())
}

#[test_case::test_case(CaptureRetry::Exhausted; "two non-message completions")]
#[test_case::test_case(CaptureRetry::MultipleMessages; "commentary and final in one attempt")]
#[test_case::test_case(CaptureRetry::Disconnect; "stream ends after calls before completion")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recovered_helpers_use_source_policy_and_only_unspent_notes_responses(
    case: CaptureRetry,
) -> Result<()> {
    skip_if_no_network_unless_localhost!(Ok(()));
    const SOURCE: &str = "gpt-5.1";
    const DEST: &str = "gpt-5.2";
    let work = tempfile::tempdir()?;
    let new = work.path().join("destination");
    std::fs::create_dir(&new)?;
    let builder = |server: &MockServer, cwd: &Path, policy| {
        let base = server.uri();
        let cwd = cwd.to_owned();
        test_codex()
            .with_model(SOURCE)
            .with_auth(CodexAuth::create_dummy_chatgpt_auth_for_testing())
            .with_config(move |cfg| {
                cfg.cwd = cwd;
                cfg.sandbox_policy = codex_core::config::Constrained::allow_any(policy);
                cfg.model_context_window = Some(200_000);
                cfg.model_auto_compact_token_limit = Some(90_000);
                cfg.model_provider.stream_max_retries = Some(1);
                cfg.features.enable(codex_core::features::Feature::Apps);
                cfg.features
                    .disable(codex_core::features::Feature::EnableRequestCompression);
                cfg.chatgpt_base_url = base;
            })
    };
    let mut events = vec![
        responses::ev_function_call("notes-only-call", "update_plan", r#"{"plan":[]}"#),
        responses::ev_custom_tool_call("notes-custom", "apply_patch", "not a patch"),
    ];
    if matches!(case, CaptureRetry::MultipleMessages) {
        let mut commentary = responses::ev_assistant_message("progress", "Checking the notes.");
        commentary["item"]["phase"] = json!("commentary");
        events.push(commentary);
        events.push(responses::ev_assistant_message(
            "final",
            "No tagged notes yet.",
        ));
    }
    if !matches!(case, CaptureRetry::Disconnect) {
        events.push(responses::ev_completed_with_tokens("first", 91_000));
    }
    let replies = [
        text_reply("old", "Original task complete.", 100_000),
        responses::sse(events),
        responses::sse(vec![
            if matches!(case, CaptureRetry::Exhausted) {
                responses::ev_reasoning_item("notes-only-reasoning", &["capture only"], &[])
            } else {
                responses::ev_assistant_message("valid", NOTES)
            },
            responses::ev_completed("retry"),
        ]),
        responses::sse(vec![
            ev_compaction("replacement"),
            responses::ev_completed_with_tokens("compact", 1_234),
        ]),
        text_reply("done", "Destination task done.", 5_000),
    ];
    let server = responses::start_mock_server().await;
    mount_policy_probe(&server).await;
    let live = responses::mount_sse_sequence(&server, replies.to_vec()).await;
    let original = builder(&server, work.path(), SandboxPolicy::new_read_only_policy())
        .build(&server)
        .await?;
    original
        .codex
        .submit(user_input("Complete the old task."))
        .await?;
    complete(&original.codex).await;
    original
        .codex
        .submit(user_input("Begin a new task."))
        .await?;
    complete(&original.codex).await;
    shutdown(&original.codex).await?;
    let sent = live.requests();
    assert_eq!(sent.len(), replies.len());
    let trigger = sent[3].body_json();
    assert_compact_request(&trigger);
    assert!(sent[3].input().starts_with(&sent[0].input()));
    assert!(!trigger["input"].to_string().contains("notes-only-call"));
    assert!(!trigger["input"].to_string().contains("capture only"));
    let retry = sent[2].input();
    let paired: Vec<_> = retry
        .iter()
        .filter(|i| i["type"].as_str().is_some_and(|k| k.ends_with("_output")))
        .collect();
    assert_eq!(paired.len(), 2);
    for (i, id) in paired.iter().zip(["notes-only-call", "notes-custom"]) {
        assert_eq!(i["call_id"], id);
        assert_eq!(
            i["output"],
            "tool use disabled during auto-compact work-notes capture"
        );
    }
    assert_eq!(
        sent[4]
            .message_input_texts("user")
            .iter()
            .filter(|s| s.ends_with(NOTES))
            .count(),
        usize::from(!matches!(case, CaptureRetry::Exhausted))
    );
    let path = original.codex.rollout_path().unwrap();
    let raw = std::fs::read_to_string(&path)?;
    let mut offset = 0;
    let (mut marker, mut first, mut rejected, mut exhausted, mut receipt) =
        (None, None, None, None, None);
    let (mut custom_call, mut custom_rejected) = (None, None);
    for line in raw.split_inclusive('\n') {
        offset += line.len();
        match serde_json::from_str::<RolloutLine>(line)?.item {
            RolloutItem::ResponseItem(ResponseItem::Message { content, .. }) if matches!(&content[..], [ContentItem::InputText {text}] if text.starts_with("<AUTO_COMPACT_WORK_NOTES_REQUEST>")) => {
                marker = Some(offset)
            }
            RolloutItem::ResponseItem(ResponseItem::FunctionCallOutput { call_id, .. })
                if call_id == "notes-only-call" =>
            {
                rejected = Some(offset)
            }
            RolloutItem::ResponseItem(ResponseItem::CustomToolCall { call_id, .. })
                if call_id == "notes-custom" =>
            {
                custom_call = Some(offset)
            }
            RolloutItem::ResponseItem(ResponseItem::CustomToolCallOutput { call_id, .. })
                if call_id == "notes-custom" =>
            {
                custom_rejected = Some(offset)
            }
            RolloutItem::PostTurnCompletionReviewState(s) => {
                match s["pending_compaction"]["notes_remaining"].as_u64() {
                    Some(1) => {
                        first.get_or_insert(offset);
                    }
                    Some(0) => {
                        exhausted.get_or_insert(offset);
                    }
                    _ => {}
                }
            }
            RolloutItem::TokenUsageContext { info, .. }
                if info.last_token_usage.input_tokens == 1_234 =>
            {
                receipt = Some(offset)
            }
            RolloutItem::Compacted(_) => break,
            _ => {}
        }
    }
    let first = if matches!(case, CaptureRetry::Disconnect) {
        custom_call.context("stream stopped with two calls")?
    } else {
        first.context("first response completed")?
    };
    let cuts = [
        ("marker", marker.context("notes marker")?, 1),
        ("before the first rejection", first, 2),
        ("one rejection", rejected.context("function rejection")?, 2),
        (
            "all rejections",
            custom_rejected.context("custom rejection")?,
            2,
        ),
        ("capture closed", exhausted.context("capture closed")?, 3),
        (
            "positive compact receipt before installation",
            receipt.context("measured compact")?,
            3,
        ),
    ];
    for (label, offset, start) in cuts {
        std::fs::write(&path, &raw[..offset])?;
        let server = responses::start_mock_server().await;
        mount_policy_probe(&server).await;
        let replay = responses::mount_sse_sequence(&server, replies[start..].to_vec()).await;
        let test = builder(&server, &new, SandboxPolicy::new_workspace_write_policy())
            .resume(&server, original.home.clone(), path.clone())
            .await?;
        let mut settings = model_override(DEST);
        let Op::OverrideTurnContext {
            cwd,
            sandbox_policy,
            ..
        } = &mut settings
        else {
            unreachable!()
        };
        *cwd = Some(new.clone());
        *sandbox_policy = Some(SandboxPolicy::new_workspace_write_policy());
        test.codex.submit(settings).await?;
        test.codex.submit(Op::Continue).await?;
        complete(&test.codex).await;
        shutdown(&test.codex).await?;
        let requests = replay.requests();
        assert_eq!(requests.len(), replies.len() - start, "{label}");
        for (index, (actual, expected)) in requests.iter().zip(&sent[start..4]).enumerate() {
            assert!(
                actual.body_json() == expected.body_json(),
                "helper {index} after {label}"
            );
        }
        // One recorder owns both protocols: observe the acknowledged cwd/policy at every model POST.
        let mut selected = None;
        let mut sampled = Vec::new();
        for req in server.received_requests().await.context("both protocols")? {
            if req.method.as_str() != "POST" {
                continue;
            }
            let body: Value = req.body_json()?;
            if body["method"] == codex_core::MCP_SANDBOX_STATE_METHOD {
                selected = Some((
                    body["params"]["sandboxPolicy"]["type"].clone(),
                    body["params"]["sandboxCwd"].clone(),
                ));
            } else if req.url.path() == "/v1/responses" {
                let input = body["input"].as_array().context("model input")?;
                let kind = if input
                    .last()
                    .is_some_and(|i| i["type"] == "compaction_trigger")
                {
                    "compact"
                } else if input.iter().any(|i| {
                    i["content"][0]["text"]
                        .as_str()
                        .is_some_and(|s| s.starts_with("<AUTO_COMPACT_WORK_NOTES_REQUEST>"))
                }) {
                    "notes"
                } else {
                    "work"
                };
                sampled.push((kind, body["model"].clone(), selected.clone()));
            }
        }
        let source = Some((json!("read-only"), json!(work.path())));
        let destination = Some((json!("workspace-write"), json!(new)));
        let mut expected = vec![("notes", json!(SOURCE), source.clone()); 3 - start];
        expected.extend([
            ("compact", json!(SOURCE), source),
            ("work", json!(DEST), destination),
        ]);
        assert_eq!(sampled, expected, "{label}");
    }
    Ok(())
}
