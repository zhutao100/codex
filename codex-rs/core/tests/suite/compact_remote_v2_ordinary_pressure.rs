use super::*;
use pretty_assertions::assert_eq;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn post_install_capacity_prices_the_paired_shell_result_live_and_on_resume() -> Result<()> {
    skip_if_no_network_unless_localhost!(Ok(()));
    use codex_protocol::openai_models::ApplyPatchToolType;
    let cwd = tempfile::tempdir()?;
    let args = evidence(cwd.path())?;
    let cfg = |limit| {
        builder(cwd.path(), 120_000, TruncationPolicyConfig::bytes(32_000)).with_config(move |c| {
            c.model_auto_compact_token_limit = Some(limit);
            let m = &mut c.model_overlay.as_mut().unwrap().cross_model;
            m.apply_patch_tool_type = Some(Some(ApplyPatchToolType::Freeform));
        })
    };
    let server = responses::start_mock_server().await;
    let call = responses::sse(vec![
        responses::ev_function_call("read", "read_file", &args),
        json!({"type":"response.completed","response":{"id":"unmeasured"}}),
    ]);
    let done = text_reply("after", "{}", 1200);
    let original = cfg(70_000).build(&server).await?;
    let patch = responses::sse(vec![
        responses::ev_apply_patch_custom_tool_call("tail", "*** Begin Patch\n*** End Patch"),
        json!({"type":"response.completed","response":{"id":"patch"}}),
    ]);
    let seed = responses::mount_sse_sequence(
        &server,
        vec![
            text_reply("original", "Done.", 75_000),
            compact_response("summary"),
            patch,
            done.clone(),
        ],
    )
    .await;
    original
        .codex
        .submit(user_input("Keep the evidence."))
        .await?;
    complete(&original.codex).await;
    original
        .codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "Read the evidence.".into(),
                text_elements: vec![],
            }],
            final_output_json_schema: Some(json!({"type":"object"})),
        })
        .await?;
    complete(&original.codex).await;
    shutdown(&original.codex).await?;
    let path = original.codex.rollout_path().unwrap();
    let raw = std::fs::read_to_string(&path)?;
    let mut prefix = String::new();
    let evidence = "changed \"filename\"\n".repeat(160);
    let envelope =
        json!({"output":evidence,"metadata":{"exit_code":0,"duration_seconds":1}}).to_string();
    let mut stored = None;
    for line in raw.split_inclusive('\n') {
        let mut v: Value = serde_json::from_str(line)?;
        // Replay a known structured patch result; the budget test does not run a host shell.
        if v["type"] == "response_item"
            && v["payload"]["type"] == "custom_tool_call_output"
            && v["payload"]["call_id"] == "tail"
        {
            v["payload"]["output"] = json!(envelope);
            stored = Some(v["payload"].clone());
            prefix += &serde_json::to_string(&v)?;
            prefix.push('\n');
            break;
        }
        prefix += line;
    }
    assert_eq!(seed.requests().len(), 4);
    std::fs::write(&path, &prefix)?;
    let probe = responses::mount_sse_sequence(&server, vec![call.clone(), done.clone()]).await;
    let work = cfg(70_000)
        .resume(&server, original.home.clone(), path.clone())
        .await?;
    work.codex.submit(Op::Continue).await?;
    complete(&work.codex).await;
    shutdown(&work.codex).await?;
    let raw = std::fs::read_to_string(&path)?;
    let read_cut = cut_after_output(&raw, "read")?;
    let sent = probe.requests();
    assert_eq!(sent.len(), 2);
    let ordinary = &sent[1];
    let wire = ordinary.custom_tool_call_output("tail");
    let stored = stored.context("recorded shell result")?;
    let delta = json_tokens(&stored)? - json_tokens(&wire)?;
    assert_eq!((json_tokens(&stored)?, json_tokens(&wire)?), (1114, 907));
    let actual = request_cost(ordinary)?
        + json_tokens(&ordinary.body_json()["tools"])?
        + json_tokens(&ordinary.body_json()["text"]["format"])?;
    for (limit, compact) in [(actual + delta / 2, false), (actual - 5, true)] {
        for (cut, run_tool) in [(prefix.as_str(), true), (&raw[..read_cut], false)] {
            std::fs::write(&path, cut)?;
            let mut replies = vec![];
            if run_tool {
                replies.push(call.clone());
            }
            if compact {
                replies.push(compact_response("summary"));
            }
            replies.push(done.clone());
            let next = responses::mount_sse_sequence(&server, replies).await;
            let work = cfg(limit)
                .resume(&server, original.home.clone(), path.clone())
                .await?;
            work.codex.submit(Op::Continue).await?;
            complete(&work.codex).await;
            shutdown(&work.codex).await?;
            let requests = next.requests();
            let decision = usize::from(run_tool);
            assert_eq!(
                requests.len(),
                decision + 1 + usize::from(compact),
                "cost {actual}, raw excess {delta}, limit {limit}"
            );
            if compact {
                assert_compact_request(&requests[decision].body_json());
            }
            let tail = requests[decision].custom_tool_call_output("tail");
            assert!(
                tail["output"]
                    .as_str()
                    .unwrap()
                    .contains("changed \"filename\"")
            );
            assert_eq!(
                requests.last().unwrap().body_json()["text"],
                ordinary.body_json()["text"]
            );
        }
    }
    Ok(())
}

#[test_case::test_case(true, false; "new model adds a format")]
#[test_case::test_case(false, false; "new model drops a format")]
#[test_case::test_case(true, true; "rebound endpoint adds a format")]
#[test_case::test_case(false, true; "rebound endpoint drops a format")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ordinary_capacity_is_priced_on_the_destination(grow: bool, rebound: bool) -> Result<()> {
    skip_if_no_network_unless_localhost!(Ok(()));
    let first = responses::start_mock_server().await;
    let second = responses::start_mock_server().await;
    let cwd = tempfile::tempdir()?;
    let build = |restored: bool| {
        let src = format!(
            "{}/v1",
            if restored && rebound {
                second.uri()
            } else {
                first.uri()
            }
        );
        let dest = format!("{}/v1", second.uri());
        let dir = cwd.path().to_owned();
        test_codex()
            .with_model("previous-provider-model")
            .with_config(move |c| {
                c.cwd = dir;
                configure_providers(c, src, dest, false);
                let overlay = c.model_overlay.as_mut().unwrap();
                for m in &mut overlay.models {
                    m.patch.context_window = Some(Some(120_000));
                    m.patch.auto_compact_token_limit = Some(Some(27_000));
                    m.patch.base_instructions = Some("Shared base.".into());
                }
            })
    };
    let initial = responses::mount_sse_once(
        &first,
        text_reply("work", "Done.", if grow { 5000 } else { 90_000 }),
    )
    .await;
    let schema = json!({"type":"object","description":"Destination format. ".repeat(1700)});
    let input = |text: String, structured: bool| Op::UserInput {
        items: vec![UserInput::Text {
            text,
            text_elements: vec![],
        }],
        final_output_json_schema: structured.then(|| schema.clone()),
    };
    let orig = build(false).build(&first).await?;
    orig.codex
        .submit(input("Customer evidence.\n".repeat(4400), !grow))
        .await?;
    complete(&orig.codex).await;
    let measured = initial.single_request();
    let text_cost = request_cost(&measured)? + json_tokens(&measured.body_json()["tools"])?;
    assert!(
        text_cost < 27_000 && text_cost + json_tokens(&schema)? > 27_000,
        "local transcript plus tools: {text_cost}"
    );
    let saved = orig.codex.rollout_path().unwrap();
    shutdown(&orig.codex).await?;
    let active = build(true)
        .resume(&second, orig.home.clone(), saved)
        .await?;
    let dest = if rebound {
        "previous-provider-model"
    } else {
        "destination-provider-model"
    };
    if !rebound {
        active.codex.submit(model_override(dest)).await?;
    }
    let source = if rebound {
        None
    } else {
        Some(responses::mount_sse_once(&first, compact_response("summary")).await)
    };
    let mut responses = vec![];
    if rebound && grow {
        responses.push(compact_response("summary"));
    }
    responses.push(text_reply("new", "Done.", 1500));
    let next = responses::mount_sse_sequence(&second, responses).await;
    active.codex.submit(input("Next.".into(), grow)).await?;
    complete(&active.codex).await;
    shutdown(&active.codex).await?;
    let sent = next.requests();
    assert_eq!(sent.len(), 1 + usize::from(rebound && grow));
    let compact = if let Some(source) = source {
        assert_eq!(source.requests().len(), usize::from(grow));
        source.requests().into_iter().next()
    } else {
        grow.then(|| sent[0].clone())
    };
    if let Some(c) = compact {
        assert_compact_request(&c.body_json());
        assert_eq!(c.body_json()["model"], "previous-provider-model");
        assert!(c.input().starts_with(&measured.input()));
    }
    let ordinary = sent.last().unwrap();
    assert_eq!(ordinary.body_json()["model"], dest);
    assert_eq!(
        ordinary.body_json().pointer("/text/format/schema"),
        grow.then_some(&schema)
    );
    assert_eq!(
        ordinary.input().iter().any(|v| v["type"] == "compaction"),
        grow
    );
    Ok(())
}
