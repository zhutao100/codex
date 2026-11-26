use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn structured_review_checkpoint_uses_outgoing_tools_live_and_after_restart()
-> anyhow::Result<()> {
    use codex_protocol::protocol::SandboxPolicy;
    skip_if_no_network_unless_localhost!(Ok(()));
    let server = MockServer::start().await;
    let home = Arc::new(TempDir::new()?);
    let configure = |cfg: &mut Config| {
        configure_recovery_windows(cfg, 200_000, 24_000)
            .cross_model
            .effective_context_window_percent = Some(100);
        cfg.web_search_mode = None;
        cfg.features.disable(Feature::WebSearchCached);
        cfg.features.disable(Feature::WebSearchRequest);
    };
    let policy = |value| {
        let mut op = context_override(None, None);
        if let Op::OverrideTurnContext { sandbox_policy, .. } = &mut op {
            *sandbox_policy = Some(value);
        }
        op
    };
    let candidate = r#"{"result":"Checked."}"#;
    let wire = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_assistant_message("candidate", candidate),
                ev_completed_with_tokens("work", 22_000),
            ]),
            verdict("Verify the evidence.", true),
            compacted_answer(),
            answer("Verified."),
            compacted_answer(),
            answer("Resumed."),
        ],
    )
    .await;
    let live = new_conversation_for_server(&server, home.clone(), configure).await;
    live.submit(policy(SandboxPolicy::new_read_only_policy()))
        .await?;
    live.submit(Op::UserInput {
        items: vec![UserInput::Text { text: "Inspect the evidence.".into(), text_elements: vec![] }],
        final_output_json_schema: Some(json!({"type":"object","properties":{"result":{"type":"string"}},"required":["result"],"additionalProperties":false})),
    }).await?;
    let finished = |event: &EventMsg, expected: &str| match event {
        EventMsg::Error(error) => panic!("{}", error.message),
        EventMsg::TurnComplete(t) => t.last_agent_message.as_deref() == Some(expected),
        _ => false,
    };
    wait_for_event(&live, |e| finished(e, candidate)).await;
    live.submit(policy(SandboxPolicy::DangerFullAccess)).await?;
    live.submit(Op::ReviewCompletedTurn).await?;
    wait_for_event(&live, |e| finished(e, "Verified.")).await;
    live.submit(Op::Shutdown).await?;
    wait_for_event(&live, |e| matches!(e, EventMsg::ShutdownComplete)).await;
    assert_eq!(responses::model_post_count(&server).await?, 4);

    // Resume at the writer's no-notes decision, not after its installed replacement.
    let path = live.rollout_path().context("rollout")?;
    let raw = std::fs::read_to_string(&path)?;
    let mut cut = 0;
    let mut checkpoint = None;
    for line in raw.split_inclusive('\n') {
        cut += line.len();
        let row: codex_protocol::protocol::RolloutLine = serde_json::from_str(line)?;
        if let RolloutItem::PostTurnCompletionReviewState(s) = row.item
            && s["pending_compaction"].is_object()
        {
            checkpoint = Some(s);
            break;
        }
    }
    let checkpoint = checkpoint.context("recorded source decision")?;
    assert_eq!(checkpoint["pending_compaction"]["notes_remaining"], 0);
    std::fs::write(&path, &raw[..cut])?;
    let restored = resume_conversation_for_server(&server, home, path, configure).await;
    restored
        .submit(policy(SandboxPolicy::DangerFullAccess))
        .await?;
    restored.submit(Op::Continue).await?;
    wait_for_event(&restored, |e| finished(e, "Resumed.")).await;
    restored.submit(Op::Shutdown).await?;
    wait_for_event(&restored, |e| matches!(e, EventMsg::ShutdownComplete)).await;
    assert_eq!(responses::model_post_count(&server).await?, 6);
    let sent = wire.requests();
    let [source, _, live_v2, live_work, recovered_v2, recovered_work] = &sent[..] else {
        anyhow::bail!("unexpected request count: {}", sent.len())
    };
    let tools = |r: &ResponsesRequest| r.body_json()["tools"].clone();
    let web = |r: &ResponsesRequest| {
        tools(r)
            .as_array()?
            .iter()
            .find(|t| t["type"] == "web_search")
            .and_then(|t| t["external_web_access"].as_bool())
    };
    assert_eq!(source.body_json()["text"]["format"]["type"], "json_schema");
    assert_eq!(
        [source, live_v2, recovered_v2, live_work, recovered_work].map(web),
        [
            Some(false),
            Some(false),
            Some(false),
            Some(true),
            Some(true)
        ]
    );
    for r in [recovered_v2, live_v2] {
        assert_eq!(r.body_json()["model"], source.body_json()["model"]);
        assert_eq!(
            r.input().last(),
            Some(&json!({"type":"compaction_trigger"}))
        );
        assert_eq!(tools(r), tools(source));
    }
    assert_eq!(tools(recovered_work), tools(live_work));
    Ok(())
}
use core_test_support::responses;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;

#[test_case::test_case(true, false, false; "canonical project instructions cannot fit")]
#[test_case::test_case(true, true, false; "downshift cannot remove project instructions")]
#[test_case::test_case(true, true, true; "review fits but fixed V2 installation exceeds the automatic limit")]
#[test_case::test_case(false, false, false; "smaller project instructions survive a useful checkpoint")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn review_checkpoint_prices_installed_project_context(
    large: bool,
    downshift: bool,
    install_only: bool,
) -> anyhow::Result<()> {
    skip_if_no_network_unless_localhost!(Ok(()));
    let project = "Verify each stored result.\n".repeat(if install_only {
        3_200
    } else if large {
        1_800
    } else {
        16
    });
    let advice = "Reconcile the evidence.\n".repeat(930);
    let server = MockServer::start().await;
    let home = Arc::new(TempDir::new()?);
    let cwd = home.path().join("workspace");
    std::fs::create_dir_all(&cwd)?;
    std::fs::write(cwd.join("AGENTS.md"), &project)?;
    let mut replies = vec![
        sse(vec![
            ev_assistant_message("candidate", "Initial."),
            ev_completed_with_tokens("work", 22_000),
        ]),
        verdict(&advice, true),
    ];
    if !large {
        replies.extend([
            answer(REVIEW_WORK_NOTES),
            compacted_answer(),
            answer("Verified."),
        ]);
    }
    let wire = mount_sse_sequence(&server, replies).await;
    let live = new_conversation_for_server(&server, home, move |cfg| {
        configure_recovery_windows(
            cfg,
            if downshift { 20_000 } else { 200_000 },
            if install_only { 64_000 } else { 24_000 },
        )
        .cross_model
        .effective_context_window_percent = Some(100);
        cfg.project_doc_max_bytes = 100_000;
        if downshift {
            cfg.model = Some("completion-recovery-large".into());
        }
    })
    .await;
    submit_request(&live, "Inspect the results.").await;
    wait_for_event(&live, |e| matches!(e, EventMsg::TurnComplete(_))).await;
    if downshift {
        live.submit(context_override(None, Some("completion-recovery-small")))
            .await?;
    }
    live.submit(Op::ReviewCompletedTurn).await?;
    wait_for_event(&live, |e| match e {
        EventMsg::Error(e) => {
            assert!(
                large
                    && e.message.contains(if install_only {
                        "19999-token context target"
                    } else {
                        "even without history"
                    }),
                "{}",
                e.message
            );
            true
        }
        EventMsg::TurnComplete(t) => t.last_agent_message.as_deref() == Some("Verified."),
        _ => false,
    })
    .await;
    live.submit(Op::Shutdown).await?;
    wait_for_event(&live, |e| matches!(e, EventMsg::ShutdownComplete)).await;
    let sent = wire.requests();
    assert_eq!(
        responses::model_post_count(&server).await?,
        if large { 2 } else { 5 }
    );
    let copies = |r: &ResponsesRequest| {
        r.input()
            .iter()
            .filter(|i| {
                i["content"][0]["text"]
                    .as_str()
                    .is_some_and(|t| t.contains(project.trim_end()))
            })
            .count()
    };
    assert_eq!(copies(&sent[0]), 1);
    if !large {
        assert_eq!(
            sent[3].input().last(),
            Some(&json!({"type":"compaction_trigger"}))
        );
        assert_eq!(copies(&sent[4]), 1);
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn checkpoint_retains_only_user_text_that_fits_beside_exact_review_scope()
-> anyhow::Result<()> {
    skip_if_no_network_unless_localhost!(Ok(()));
    let server = MockServer::start().await;
    let scope = format!("BEGIN\n{}END", "Evidence row 0123456789\n".repeat(5_400));
    let wire = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_assistant_message("candidate", "Initial."),
                ev_completed_with_tokens("work", 40_000),
            ]),
            verdict("Verify every row.", true),
            answer(REVIEW_WORK_NOTES),
            compacted_answer(),
            answer("Verified."),
        ],
    )
    .await;
    let live = new_conversation_for_server(&server, Arc::new(TempDir::new()?), |cfg| {
        configure_recovery_windows(cfg, 62_000, 64_000)
            .cross_model
            .effective_context_window_percent = Some(100);
    })
    .await;
    submit_request(&live, &scope).await;
    wait_for_event(&live, |e| matches!(e, EventMsg::TurnComplete(_))).await;
    live.submit(Op::ReviewCompletedTurn).await?;
    wait_for_event(&live, |e| match e {
        EventMsg::Error(e) => panic!("{}", e.message),
        EventMsg::TurnComplete(t) => t.last_agent_message.as_deref() == Some("Verified."),
        _ => false,
    })
    .await;
    live.submit(Op::Shutdown).await?;
    wait_for_event(&live, |e| matches!(e, EventMsg::ShutdownComplete)).await;
    let sent = wire.requests();
    assert_eq!(responses::model_post_count(&server).await?, 5);
    let user = |r: &ResponsesRequest| {
        r.input()
            .into_iter()
            .filter(|i| i["role"] == "user")
            .find_map(|i| {
                i["content"][0]["text"]
                    .as_str()
                    .filter(|t| t.starts_with("BEGIN"))
                    .map(str::to_owned)
            })
    };
    assert_eq!(user(&sent[2]).as_ref(), Some(&scope));
    assert_eq!(user(&sent[3]).as_ref(), Some(&scope));
    let kept = user(&sent[4]).context("optional retained user message")?;
    assert!(kept.ends_with("END") && kept.len() < scope.len());
    assert!(
        followup_frame(&sent[4])
            .context("review frame")?
            .contains(&scope)
    );
    assert!(sent[4].input().iter().any(|i| {
        i["content"][0]["text"]
            .as_str()
            .is_some_and(|t| t.contains(REVIEW_WORK_NOTES))
    }));
    Ok(())
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ReviewFloor {
    Fits,
    Candidate,
    Advice,
    Combined,
    Tools,
}

#[test_case::test_case(ReviewFloor::Fits, false; "larger destination fits the complete follow-up")]
#[test_case::test_case(ReviewFloor::Candidate, false; "mandatory candidate cannot fit the destination")]
#[test_case::test_case(ReviewFloor::Candidate, true; "impossible candidate at a preturn downshift")]
#[test_case::test_case(ReviewFloor::Advice, true; "impossible advice at a preturn downshift")]
#[test_case::test_case(ReviewFloor::Combined, false; "scope and advice fit separately but not together")]
#[test_case::test_case(ReviewFloor::Combined, true; "scope and advice cannot fit together after downshift")]
#[test_case::test_case(ReviewFloor::Tools, false; "tools make the retained handoff impossible")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn review_handoff_skips_both_helpers_when_compaction_cannot_help(
    floor: ReviewFloor,
    downshift: bool,
) -> anyhow::Result<()> {
    skip_if_no_network_unless_localhost!(Ok(()));
    let server = MockServer::start().await;
    let larger = floor == ReviewFloor::Fits;
    let candidate = match floor {
        ReviewFloor::Fits | ReviewFloor::Advice => "Initial candidate.".to_string(),
        ReviewFloor::Candidate => "Check the exact candidate.\n".repeat(3_000),
        ReviewFloor::Combined => "Check the exact candidate.\n".repeat(1_300),
        ReviewFloor::Tools => "Check the exact candidate.\n".repeat(900),
    };
    let advice = "Verify the evidence before answering.\n".repeat(match floor {
        ReviewFloor::Fits | ReviewFloor::Advice => 1_900,
        ReviewFloor::Candidate => 400,
        ReviewFloor::Combined => 1_000,
        ReviewFloor::Tools => 700,
    });
    if floor == ReviewFloor::Combined {
        let target = 24_000 - 8_192;
        assert!(advice.len().div_ceil(4) < target && candidate.len().div_ceil(4) < target);
        assert!((advice.len() + candidate.len()).div_ceil(4) > target);
    }
    // On the smaller source the work plus advice exceeds the notes budget.
    let mut replies = vec![
        sse(vec![
            ev_assistant_message("initial", &candidate),
            ev_completed_with_tokens("initial", 22_000),
        ]),
        verdict(&advice, true),
    ];
    if larger {
        replies.push(answer("Verified."));
    }
    let traffic = mount_sse_sequence(&server, replies).await;
    let home = Arc::new(TempDir::new()?);
    let live = new_conversation_for_server(&server, home, move |cfg| {
        let overlay =
            configure_recovery_windows(cfg, if downshift { 20_000 } else { 200_000 }, 24_000);
        overlay.cross_model.effective_context_window_percent = Some(100);
        if larger {
            // Advice fits once on this destination; embedding it again in the frame cannot.
            overlay.models[1].patch.context_window = Some(Some(40_000));
        }
        if downshift {
            cfg.model = Some("completion-recovery-large".into());
        }
    })
    .await;
    let inventory = MockServer::start().await;
    if floor == ReviewFloor::Tools {
        let names: Vec<_> = (0..100).map(|i| format!("required_field_{i}")).collect();
        responses::mount_app_catalog(
            &inventory,
            &names.iter().map(String::as_str).collect::<Vec<_>>(),
        )
        .await;
        live.submit(responses::refresh_catalog_servers(
            &inventory,
            &["inventory"],
        ))
        .await?;
    }
    submit_request(&live, "Check the work.").await;
    wait_for_event(&live, |e| matches!(e, EventMsg::TurnComplete(_))).await;
    if larger || downshift {
        live.submit(context_override(
            None,
            Some(if larger {
                "completion-recovery-large"
            } else {
                "completion-recovery-small"
            }),
        ))
        .await?;
    }
    live.submit(Op::ReviewCompletedTurn).await?;
    wait_for_event(&live, |e| match e {
        EventMsg::Error(e) => {
            assert!(
                !larger
                    && e.message.contains(if floor == ReviewFloor::Candidate {
                        "mandatory review frame alone"
                    } else {
                        "even without history"
                    }),
                "{}",
                e.message
            );
            true
        }
        EventMsg::TurnComplete(t) if t.last_agent_message.as_deref() == Some("Verified.") => true,
        _ => false,
    })
    .await;
    live.submit(Op::Shutdown).await?;
    wait_for_event(&live, |e| matches!(e, EventMsg::ShutdownComplete)).await;
    let requests = traffic.requests();
    assert_eq!(requests.len(), if larger { 3 } else { 2 });
    assert_eq!(responses::model_post_count(&server).await?, requests.len());
    assert_eq!(
        requests[0].body_json()["model"],
        if downshift {
            "completion-recovery-large"
        } else {
            "completion-recovery-small"
        }
    );
    assert!(packet(&requests[1]).contains(&candidate));
    if floor == ReviewFloor::Tools {
        let text = (advice.len() + candidate.len()).div_ceil(4);
        let tools = serde_json::to_vec(&requests[0].body_json()["tools"])?
            .len()
            .div_ceil(4);
        assert!(text < 24_000 - 8_192 && text + tools > 24_000 - 8_192);
    }
    if larger {
        assert!(2 * advice.len().div_ceil(4) > 40_000 - 8_192);
        assert_eq!(
            requests[2].body_json()["model"],
            "completion-recovery-large"
        );
        assert!(
            followup_frame(&requests[2])
                .context("mandatory frame")?
                .contains(&candidate)
        );
        assert!(requests[2].input().iter().any(|i| {
            i["content"][0]["text"]
                .as_str()
                .is_some_and(|s| s.contains(&advice))
        }));
    }
    Ok(())
}

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
