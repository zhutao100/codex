use super::*;
use pretty_assertions::assert_eq;

#[path = "compact_remote_v2_refused_route.rs"]
mod refused_route;

fn structured_input(text: &str) -> Op {
    Op::UserInput {
        items: vec![UserInput::Text {
            text: text.into(),
            text_elements: vec![],
        }],
        final_output_json_schema: Some(json!({"type":"object"})),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn downshift_does_not_charge_an_earlier_models_bill_to_unmeasured_work() -> Result<()> {
    skip_if_no_network_unless_localhost!(Ok(()));
    let server = responses::start_mock_server().await;
    let build = || {
        let uri = format!("{}/v1", server.uri());
        test_codex()
            .with_model("previous-provider-model")
            .with_config(move |cfg| {
                configure_providers(cfg, uri.clone(), uri, false);
            })
    };
    for restart in [false, true] {
        let posts = responses::mount_sse_sequence(
            &server,
            vec![
                text_reply("first", "First done.", 60_000),
                responses::sse(vec![
                    responses::ev_assistant_message("middle", "Middle done."),
                    responses::ev_completed("middle"),
                ]),
                text_reply("last", "Third done.", 2_000),
            ],
        )
        .await;
        let work = build().build(&server).await?;
        work.codex.submit(structured_input("First task.")).await?;
        complete(&work.codex).await;
        work.codex
            .submit(model_override("recovery-provider-model"))
            .await?;
        work.codex.submit(structured_input("Middle task.")).await?;
        complete(&work.codex).await;
        let work = if restart {
            shutdown(&work.codex).await?;
            build()
                .resume(
                    &server,
                    work.home.clone(),
                    work.codex.rollout_path().unwrap(),
                )
                .await?
        } else {
            work
        };
        work.codex
            .submit(model_override("destination-provider-model"))
            .await?;
        work.codex.submit(structured_input("Last task.")).await?;
        complete(&work.codex).await;
        shutdown(&work.codex).await?;
        let sent: Vec<_> = posts
            .requests()
            .iter()
            .map(|r| r.body_json()["model"].clone())
            .collect();
        assert_eq!(
            sent,
            [
                "previous-provider-model",
                "recovery-provider-model",
                "destination-provider-model"
            ]
            .map(|v| json!(v))
        );
    }
    Ok(())
}

#[test_case::test_case(false, false; "live rejection after lifetime usage exceeds a window")]
#[test_case::test_case(true, false; "restart after lifetime usage exceeds a window")]
#[test_case::test_case(true, true; "a different endpoint does not inherit the refusal")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ordinary_rejection_preserves_bills_and_requires_a_checkpoint(
    restart: bool,
    rebound: bool,
) -> Result<()> {
    skip_if_no_network_unless_localhost!(Ok(()));
    let cwd = tempfile::tempdir()?;
    let build = || builder(cwd.path(), 50_000, TruncationPolicyConfig::bytes(32_000));
    let server = responses::start_mock_server().await;
    let receipt = |id, input, cached| {
        let mut event = responses::ev_completed_with_tokens(id, input);
        event["response"]["usage"]["input_tokens_details"] = json!({"cached_tokens": cached});
        event
    };
    let mut replies: Vec<_> = ["one", "two", "three"]
        .into_iter()
        .map(|id| {
            responses::sse(vec![
                responses::ev_assistant_message(id, "Done."),
                receipt(id, 18_000, 16_000),
            ])
        })
        .collect();
    replies.push(responses::sse_failed(
        "refused",
        "context_length_exceeded",
        "input too large",
    ));
    let destination = responses::start_mock_server().await;
    let route = if rebound { &destination } else { &server };
    let seed = responses::mount_sse_sequence(&server, replies).await;
    let mut recovery = Vec::new();
    if !rebound {
        recovery.push(responses::sse(vec![
            ev_compaction("summary"),
            receipt("compact", 1200, 512),
        ]));
    }
    recovery.push(responses::sse(vec![
        responses::ev_assistant_message("next", "Done."),
        receipt("next", 2000, 1024),
    ]));
    let original = build().build(&server).await?;
    for text in ["First.", "Second.", "Third."] {
        original.codex.submit(structured_input(text)).await?;
        complete(&original.codex).await;
    }
    original
        .codex
        .submit(structured_input("Rejected task."))
        .await?;
    wait_for_event(&original.codex, |e| matches!(e, EventMsg::Error(_))).await;
    complete(&original.codex).await;
    assert_eq!(seed.requests().len(), 4);
    let posts = responses::mount_sse_sequence(route, recovery).await;
    let path = original.codex.rollout_path().unwrap();
    let active = if restart {
        shutdown(&original.codex).await?;
        build()
            .resume(route, original.home.clone(), path.clone())
            .await?
    } else {
        original
    };
    let last_bill = || -> Result<_> {
        std::fs::read_to_string(&path)?
            .lines()
            .map(serde_json::from_str::<RolloutLine>)
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .filter_map(|l| match l.item {
                RolloutItem::TokenUsageContext {
                    info,
                    rejected_context,
                    ..
                } => Some((info, rejected_context)),
                _ => None,
            })
            .next_back()
            .context("durable usage")
    };
    let (before, refusal) = last_bill()?;
    let refusal = refusal.context("durable rejection separate from billing")?;
    assert_eq!(refusal.model, "gpt-5.1");
    assert!(refusal.endpoint_sha256.is_some());
    assert_eq!(
        (
            before.total_token_usage.input_tokens,
            before.total_token_usage.cached_input_tokens
        ),
        (54_000, 48_000)
    );
    assert_eq!(before.last_token_usage.input_tokens, 18_000);
    active.codex.submit(Op::Continue).await?;
    complete(&active.codex).await;
    shutdown(&active.codex).await?;
    assert_eq!(posts.requests().len(), if rebound { 1 } else { 2 });
    if !rebound {
        assert_compact_request(&posts.requests()[0].body_json());
    }
    let (after, refusal) = last_bill()?;
    assert_eq!(refusal, None);
    assert_eq!(
        (
            after.total_token_usage.input_tokens,
            after.total_token_usage.cached_input_tokens
        ),
        if rebound {
            (56_000, 49_024)
        } else {
            (57_200, 49_536)
        }
    );
    Ok(())
}

#[test_case::test_case(true; "queued inventory addition exceeds capacity")]
#[test_case::test_case(false; "queued inventory removal fits")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ordinary_refresh_prices_the_next_inventory(add: bool) -> Result<()> {
    skip_if_no_network_unless_localhost!(Ok(()));
    let cwd = tempfile::tempdir()?;
    let inventory = responses::start_mock_server().await;
    let names: Vec<_> = (0..100).map(|n| format!("field_{n}")).collect();
    responses::mount_app_catalog(
        &inventory,
        &names.iter().map(String::as_str).collect::<Vec<_>>(),
    )
    .await;
    let refresh =
        |on| responses::refresh_catalog_servers(&inventory, if on { &["inventory"] } else { &[] });
    let server = responses::start_mock_server().await;
    let mut replies = vec![text_reply(
        "first",
        "Done.",
        if add { 26_000 } else { 28_000 },
    )];
    if add {
        replies.push(compact_response("summary"));
    }
    replies.push(text_reply("next", "Done.", 1_000));
    let traffic = responses::mount_sse_sequence(&server, replies).await;
    let session = builder(cwd.path(), 120_000, TruncationPolicyConfig::bytes(32_000))
        .build(&server)
        .await?;
    session.codex.submit(refresh(!add)).await?;
    session
        .codex
        .submit(structured_input("Initial task."))
        .await?;
    complete(&session.codex).await;
    session.codex.submit(refresh(add)).await?;
    session.codex.submit(structured_input("Next task.")).await?;
    complete(&session.codex).await;
    shutdown(&session.codex).await?;
    let sent = traffic.requests();
    assert_eq!(sent.len(), 2 + usize::from(add));
    if add {
        assert_compact_request(&sent[1].body_json());
    }
    let before = &sent[0].body_json()["tools"];
    let after = &sent.last().unwrap().body_json()["tools"];
    assert_eq!(before.to_string().contains("probe_field_0"), !add);
    assert_eq!(after.to_string().contains("probe_field_0"), add);
    assert!(json_tokens(before)?.abs_diff(json_tokens(after)?) > 2_000);
    Ok(())
}

#[test_case::test_case(26_900, true; "uncovered result crosses the source threshold")]
#[test_case::test_case(21_000, false; "unfinished source below threshold")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rebound_prices_the_interrupted_source_tail(usage: i64, compact: bool) -> Result<()> {
    skip_if_no_network_unless_localhost!(Ok(()));
    let cwd = tempfile::tempdir()?;
    let args = evidence(cwd.path())?;
    let build = || builder(cwd.path(), 120_000, TruncationPolicyConfig::bytes(32_000));
    let original = responses::start_mock_server().await;
    let mut rest = vec![];
    if compact {
        rest.push(compact_response("summary"));
    }
    rest.push(text_reply("done", "Done.", 1_000));
    let call = responses::sse(vec![
        responses::ev_function_call("tool", "read_file", &args),
        responses::ev_completed_with_tokens("source", usage),
    ]);
    let traffic =
        responses::mount_sse_sequence(&original, [vec![call], rest.clone()].concat()).await;
    let live = build().build(&original).await?;
    live.codex
        .submit(structured_input("Read the evidence."))
        .await?;
    complete(&live.codex).await;
    shutdown(&live.codex).await?;
    let path = live.codex.rollout_path().unwrap();
    let raw = std::fs::read_to_string(&path)?;
    std::fs::write(&path, &raw[..cut_after_output(&raw, "tool")?])?;
    let rebound = responses::start_mock_server().await;
    let posts = responses::mount_sse_sequence(&rebound, rest).await;
    let replay = build().resume(&rebound, live.home.clone(), path).await?;
    replay.codex.submit(Op::Continue).await?;
    complete(&replay.codex).await;
    shutdown(&replay.codex).await?;
    let sent = posts.requests();
    assert_eq!(sent.len(), 1 + usize::from(compact));
    assert_eq!(traffic.requests().len(), sent.len() + 1);
    if compact {
        assert_compact_request(&sent[0].body_json());
        assert_eq!(sent[0].input(), traffic.requests()[1].input());
    }
    assert_eq!(
        sent.last().unwrap().body_json()["text"]["format"]["schema"],
        json!({"type":"object"})
    );
    Ok(())
}
