use super::*;
use core_test_support::responses;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pinned_review_prices_the_formatted_result_and_refreshed_mcp_tools() -> anyhow::Result<()> {
    skip_if_no_network_unless_localhost!(Ok(()));
    const TARGET: i64 = 40_000 - 8_192;
    let config = |cfg: &mut Config| {
        let overlay = configure_recovery_windows(cfg, 200_000, 40_000);
        overlay.cross_model.effective_context_window_percent = Some(100);
        overlay.cross_model.apply_patch_tool_type = Some(Some(
            codex_protocol::openai_models::ApplyPatchToolType::Freeform,
        ));
    };
    let inventory = MockServer::start().await;
    let names: Vec<_> = (0..50).map(|i| format!("field_{i}")).collect();
    responses::mount_app_catalog(
        &inventory,
        &names.iter().map(String::as_str).collect::<Vec<_>>(),
    )
    .await;
    let server = MockServer::start().await;
    let captures = mount_sse_sequence(
        &server,
        vec![
            answer("Initial."),
            verdict("Verify the work.", true),
            sse(vec![
                responses::ev_apply_patch_custom_tool_call(
                    "tail",
                    "*** Begin Patch\n*** End Patch",
                ),
                ev_completed_with_tokens("pinned", TARGET - 1000),
            ]),
            answer("Verified."),
        ],
    )
    .await;
    let home = Arc::new(TempDir::new()?);
    let live = new_conversation_for_server(&server, home.clone(), config).await;
    live.submit(responses::refresh_catalog_servers(
        &inventory,
        &["inventory"],
    ))
    .await?;
    live.submit(Op::UserInput {
        items: vec![UserInput::Text {
            text: "Check the patch.".into(),
            text_elements: vec![],
        }],
        final_output_json_schema: None,
    })
    .await?;
    wait_for_event(&live, |e| matches!(e, EventMsg::TurnComplete(_))).await;
    live.submit(Op::ReviewCompletedTurn).await?;
    wait_for_event(&live, |e| matches!(e, EventMsg::TurnComplete(t) if t.last_agent_message.as_deref()==Some("Verified."))).await;
    let path = live.rollout_path().context("rollout")?;
    live.submit(Op::Shutdown).await?;
    wait_for_event(&live, |e| matches!(e, EventMsg::ShutdownComplete)).await;
    let requests = captures.requests();
    assert_eq!(requests.len(), 4);
    let pinned = &requests[2];
    let old_tools = &pinned.body_json()["tools"];
    assert!(
        old_tools
            .to_string()
            .contains("mcp__inventory__probe_field_0")
    );

    let raw = std::fs::read_to_string(&path)?;
    let evidence = "changed \"filename\"\n".repeat(160);
    let stored = json!({"type":"custom_tool_call_output","call_id":"tail","output":json!({"output":evidence,"metadata":{"exit_code":0,"duration_seconds":1}}).to_string()});
    let wire = json!({"type":"custom_tool_call_output","call_id":"tail","output":format!("Exit code: 0\nWall time: 1 seconds\nOutput:\n{evidence}")});
    let tokens =
        |v: &Value| -> anyhow::Result<i64> { Ok(serde_json::to_vec(v)?.len().div_ceil(4) as i64) };
    assert_eq!((tokens(&stored)?, tokens(&wire)?), (1114, 907));
    for (servers, gap, compact) in [
        (&["inventory"][..], 1000, false),
        (&["inventory"][..], 906, true),
        (&["inventory", "extra"][..], 1000, true),
        (&[][..], 906, false),
    ] {
        let mut cut = String::new();
        let mut found = false;
        for line in raw.lines() {
            let mut record: Value = serde_json::from_str(line)?;
            let p = &mut record["payload"];
            if p["info"]["last_token_usage"]["input_tokens"] == TARGET - 1000 {
                if let Some(measured) = p.get("measured_tool_tokens") {
                    assert_eq!(measured, &json!(tokens(old_tools)?));
                }
                for key in ["input_tokens", "total_tokens"] {
                    p["info"]["last_token_usage"][key] = json!(TARGET - gap);
                    let total = p["info"]["total_token_usage"][key]
                        .as_i64()
                        .context("total")?;
                    p["info"]["total_token_usage"][key] = json!(total + 1000 - gap);
                }
            }
            if p["type"] == "custom_tool_call_output" && p["call_id"] == "tail" {
                *p = stored.clone();
                found = true;
            }
            cut.push_str(&serde_json::to_string(&record)?);
            cut.push('\n');
            if found {
                break;
            }
        }
        assert!(found);
        std::fs::write(&path, cut)?;
        let server = MockServer::start().await;
        let mut replies = Vec::new();
        if compact {
            replies.extend([answer(REVIEW_WORK_NOTES), compacted_answer_with("summary")]);
        }
        replies.push(answer("Restored."));
        let traffic = mount_sse_sequence(&server, replies).await;
        let active =
            resume_conversation_for_server(&server, home.clone(), path.clone(), config).await;
        active
            .submit(responses::refresh_catalog_servers(&inventory, servers))
            .await?;
        active.submit(Op::Continue).await?;
        wait_for_event(&active, |e| match e {
            EventMsg::Error(err) => panic!("review failed ({servers:?}, {gap}): {}", err.message),
            EventMsg::TurnComplete(t) => t.last_agent_message.as_deref() == Some("Restored."),
            _ => false,
        })
        .await;
        active.submit(Op::Shutdown).await?;
        wait_for_event(&active, |e| matches!(e, EventMsg::ShutdownComplete)).await;
        let sent = traffic.requests();
        assert_eq!(sent.len(), if compact { 3 } else { 1 });
        let next = &sent[0];
        assert!(next.input().starts_with(&pinned.input()));
        assert_eq!(next.custom_tool_call_output("tail"), wire);
        let new_tools = &next.body_json()["tools"];
        let change = tokens(new_tools)? - tokens(old_tools)?;
        assert_eq!(tokens(&wire)? + change > gap, compact);
        assert_eq!(
            new_tools.to_string().contains("mcp__extra__probe_field_0"),
            servers.contains(&"extra")
        );
        if compact {
            assert_review_compaction_cycle(&sent);
        } else {
            assert_eq!(followup_frame(next), followup_frame(pinned));
        }
    }
    Ok(())
}
