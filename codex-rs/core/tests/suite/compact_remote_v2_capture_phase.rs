use super::*;
use anyhow::Context;
use codex_protocol::protocol::SandboxPolicy;
use pretty_assertions::assert_eq;
use std::path::Path;
use wiremock::MockServer;

fn text_reply(id: &str, text: &str, usage: i64) -> String {
    responses::sse(vec![
        responses::ev_assistant_message(id, text),
        responses::ev_completed_with_tokens(id, usage),
    ])
}

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
async fn recovered_helpers_use_source_policy_and_only_unspent_notes_responses() -> Result<()> {
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
                cfg.features.enable(codex_core::features::Feature::Apps);
                cfg.features
                    .disable(codex_core::features::Feature::EnableRequestCompression);
                cfg.chatgpt_base_url = base;
            })
    };
    let replies = [
        text_reply("old", "Original task complete.", 100_000),
        responses::sse(vec![
            responses::ev_function_call("notes-only-call", "update_plan", r#"{"plan":[]}"#),
            responses::ev_completed_with_tokens("tool-only", 91_000),
        ]),
        responses::sse(vec![
            responses::ev_reasoning_item("notes-only-reasoning", &["capture only"], &[]),
            responses::ev_completed("reasoning-only"),
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
    let path = original.codex.rollout_path().unwrap();
    let raw = std::fs::read_to_string(&path)?;
    let mut offset = 0;
    let (mut marker, mut first, mut rejected, mut exhausted, mut receipt) =
        (None, None, None, None, None);
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
    let first = first.context("tool-only response completed")?;
    let rejected = rejected.context("capture tool was rejected")?;
    let cuts = [
        ("marker", marker.context("notes marker")?, 1),
        ("tool-only completion", first, 2),
        ("rejected tool", first.max(rejected), 2),
        (
            "zero-usage reasoning completion",
            exhausted.context("capture closed")?,
            3,
        ),
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
        assert!(
            requests[3 - start].body_json() == trigger,
            "compact request after {label}"
        );
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
