use super::*;
use core_test_support::responses::base_instructions_text;
use pretty_assertions::assert_eq;

#[path = "compact_remote_v2_capture_phase.rs"]
mod capture_phase;

pub(super) fn configure_providers(
    config: &mut codex_core::config::Config,
    previous_base: String,
    destination_base: String,
    final_override: bool,
) {
    config.base_instructions = None;
    config.model_provider.request_max_retries = Some(0);
    config.model_provider.stream_max_retries = Some(0);
    let mut previous = config.model_provider.clone();
    previous.base_url = Some(previous_base);
    config.model_providers.insert("previous".into(), previous);
    let mut destination = config.model_provider.clone();
    destination.base_url = Some(destination_base);
    config
        .model_providers
        .insert("recovery".into(), destination.clone());
    config
        .model_providers
        .insert("destination".into(), destination);
    config.model_overlay = Some(ModelOverlay {
        models: [
            ("previous", 200_000, 180_000, "Previous"),
            ("destination", 100_000, 40_000, "Destination"),
            ("recovery", 160_000, 80_000, "Recovery"),
        ]
        .into_iter()
        .map(|(provider, window, limit, label)| ModelOverlayEntry {
            slug: format!("{provider}-provider-model"),
            model_provider: Some(provider.into()),
            patch: ModelInfoPatch {
                base_instructions: Some(format!("{label} model base instructions.")),
                context_window: Some(Some(window)),
                auto_compact_token_limit: Some(Some(limit)),
                ..Default::default()
            },
            final_instruction_override: final_override
                .then(|| format!("{label} model final override.")),
        })
        .collect(),
        ..Default::default()
    });
}

fn model_override(model: &str) -> Op {
    Op::OverrideTurnContext {
        cwd: None,
        approval_policy: None,
        sandbox_policy: None,
        windows_sandbox_level: None,
        model: Some(model.into()),
        effort: None,
        summary: None,
        collaboration_mode: None,
        personality: None,
        service_tier: None,
    }
}

fn without_compaction_decision(raw: &str) -> Result<String> {
    let mut legacy = String::new();
    for line in raw.split_inclusive('\n') {
        let mut record: RolloutLine = serde_json::from_str(line)?;
        if let RolloutItem::PostTurnCompletionReviewState(state) = &mut record.item
            && state
                .as_object_mut()
                .unwrap()
                .remove("pending_compaction")
                .is_some()
        {
            legacy.push_str(&serde_json::to_string(&record)?);
            legacy.push('\n');
        } else {
            legacy.push_str(line);
        }
    }
    Ok(legacy)
}

fn assert_same_input(actual: &[Value], expected: &[Value], cut: usize) {
    // Compare complete values but do not print hundreds of kilobytes on failure.
    assert_eq!(actual.len(), expected.len(), "input length at cut {cut}");
    assert_eq!(
        actual.iter().zip(expected).position(|(a, b)| a != b),
        None,
        "first input difference at cut {cut}"
    );
}

#[test_case::test_case(false, 40_500, false; "fresh turn above destination threshold")]
#[test_case::test_case(true, 40_000, false; "fresh turn after restart at destination threshold")]
#[test_case::test_case(false, 120_000, true; "continue work larger than destination window")]
#[test_case::test_case(true, 40_000, true; "continue after restart at destination threshold")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn preturn_downshift_uses_work_measured_by_continued_model(
    restart: bool,
    measured: i64,
    continue_on_small: bool,
) -> Result<()> {
    skip_if_no_network_unless_localhost!(Ok(()));
    let small = responses::start_mock_server().await;
    let large = responses::start_mock_server().await;
    let builder = || {
        let big_url = format!("{}/v1", large.uri());
        let small_url = format!("{}/v1", small.uri());
        test_codex()
            .with_model("destination-provider-model")
            .with_config(move |cfg| {
                configure_providers(cfg, big_url, small_url, false);
            })
    };
    let test = builder().build(&small).await?;
    let initial = responses::mount_response_sequence(
        &small,
        vec![
            responses::sse_response(responses::sse(vec![
                responses::ev_function_call(
                    "plan",
                    "update_plan",
                    r#"{"plan":[{"step":"Collect evidence","status":"pending"}]}"#,
                ),
                responses::ev_completed_with_tokens("small", 3_000),
            ])),
            responses::sse_response(responses::sse_completed("unused"))
                .set_delay(Duration::from_secs(10)),
        ],
    )
    .await;
    test.codex
        .submit(user_input("First task on small."))
        .await?;
    tokio::time::timeout(Duration::from_secs(5), async {
        while initial.requests().len() < 2 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await?;
    test.codex.submit(Op::Pause).await?;
    wait_for_event(&test.codex, |e| matches!(e, EventMsg::TurnPaused(_))).await;
    test.codex
        .submit(model_override("previous-provider-model"))
        .await?;
    let large_work = if continue_on_small {
        responses::ev_function_call(
            "large-plan",
            "update_plan",
            r#"{"plan":[{"step":"Continue the original task","status":"in_progress"}]}"#,
        )
    } else {
        responses::ev_assistant_message("finished", "Large model completed the original task.")
    };
    let mut replies = vec![responses::sse_response(responses::sse(vec![
        large_work,
        responses::ev_completed_with_tokens("large", measured),
    ]))];
    if continue_on_small {
        replies.push(
            responses::sse_response(responses::sse_completed("unused-large"))
                .set_delay(Duration::from_secs(10)),
        );
    }
    replies.extend([
        responses::sse_response(responses::sse(vec![
            responses::ev_assistant_message("notes", NOTES),
            responses::ev_completed("notes"),
        ])),
        responses::sse_response(compact_response("from-large")),
    ]);
    let source = responses::mount_response_sequence(&large, replies).await;
    test.codex.submit(Op::Continue).await?;
    if continue_on_small {
        tokio::time::timeout(Duration::from_secs(5), async {
            while source.requests().len() < 2 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await?;
        test.codex.submit(Op::Pause).await?;
        wait_for_event(&test.codex, |e| matches!(e, EventMsg::TurnPaused(_))).await;
    } else {
        complete(&test.codex).await;
    }
    let sent = 1 + usize::from(continue_on_small);
    assert_eq!(source.requests().len(), sent);
    let resumed;
    let active = if restart {
        let path = test.codex.rollout_path().unwrap();
        test.codex.submit(Op::Shutdown).await?;
        wait_for_event(&test.codex, |e| matches!(e, EventMsg::ShutdownComplete)).await;
        resumed = builder().resume(&small, test.home.clone(), path).await?;
        &resumed.codex
    } else {
        &test.codex
    };
    let ordinary = responses::mount_sse_once(
        &small,
        responses::sse(vec![
            responses::ev_assistant_message("next", "New task on small."),
            responses::ev_completed("next"),
        ]),
    )
    .await;
    active
        .submit(model_override("destination-provider-model"))
        .await?;
    active
        .submit(if continue_on_small {
            Op::Continue
        } else {
            user_input("New task after downshift.")
        })
        .await?;
    wait_for_event(active, |e| match e {
        EventMsg::Error(err) => panic!("{}", err.message),
        EventMsg::TurnComplete(_) => true,
        _ => false,
    })
    .await;
    let requests = source.requests();
    assert_eq!(requests.len(), sent + 2);
    let hot = requests[sent - 1].input();
    for helper in &requests[sent..] {
        assert_eq!(helper.body_json()["model"], "previous-provider-model");
        assert_eq!(
            helper.body_json()["tools"],
            requests[0].body_json()["tools"]
        );
        let input = helper.input();
        assert!(input.starts_with(&hot));
        if continue_on_small {
            assert!(
                hot.iter()
                    .any(|i| i["call_id"] == "large-plan" && i["type"] == "function_call_output")
            );
            assert_eq!(input.len(), hot.len() + 1);
        } else {
            assert_eq!(
                input[hot.len()]["content"][0]["text"],
                "Large model completed the original task."
            );
            assert_eq!(input.len(), hot.len() + 2);
        }
    }
    assert_compact_request(&requests[sent + 1].body_json());
    let next = ordinary.single_request();
    assert_eq!(next.body_json()["model"], "destination-provider-model");
    assert!(
        next.input()
            .iter()
            .any(|item| item["encrypted_content"] == "from-large")
    );
    Ok(())
}

async fn shutdown(codex: &codex_core::CodexThread) -> Result<()> {
    codex.submit(Op::Shutdown).await?;
    wait_for_event(codex, |event| matches!(event, EventMsg::ShutdownComplete)).await;
    Ok(())
}

fn preturn_replay_cuts(
    raw: &str,
    prompt: &str,
    dest: &str,
    last: usize,
) -> Result<Vec<(&'static str, usize, usize)>> {
    let mut cuts = Vec::new();
    let mut offset = 0;
    let mut installed = None;
    let mut context_items = 0;
    let mut saw_usage = false;
    for line in raw.split_inclusive('\n') {
        offset += line.len();
        match serde_json::from_str::<RolloutLine>(line)?.item {
            RolloutItem::PostTurnCompletionReviewState(s)
                if s["pending_inputs"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|p| p["new_task"] == true && p["content"][0]["text"] == prompt) =>
            {
                if cuts.is_empty() {
                    cuts.push(("accepted before capture", offset, 1));
                } else if cuts.len() == 1 && s["pending_compaction"].is_object() {
                    cuts.push(("selected before first helper", offset, 1));
                } else if installed.is_some()
                    && s["pending_inputs"][0]["remaining_context"].is_object()
                {
                    cuts.push(("prepared before first context item", offset, last));
                }
            }
            RolloutItem::ResponseItem(ResponseItem::Message { content, .. })
                if installed.is_none() =>
            {
                match &content[..] {
                    [ContentItem::InputText { text }]
                        if text.starts_with("<AUTO_COMPACT_WORK_NOTES_REQUEST>") =>
                    {
                        cuts.push(("notes marker", offset, 1))
                    }
                    [ContentItem::OutputText { text, .. }] if text == NOTES => {
                        cuts.push(("notes answer", offset, last - 1))
                    }
                    _ => {}
                }
            }
            RolloutItem::TokenUsageContext { info, .. }
                if installed.is_none() && info.last_token_usage.input_tokens == 1_234 =>
            {
                cuts.push(("compact receipt before install", offset, last - 1));
            }
            RolloutItem::Compacted(item) => {
                let checkpoint = item.checkpoint.expect("self-contained marker");
                assert!(checkpoint.injected_context.is_none());
                assert!(checkpoint.usage.last_token_usage.total_tokens < 40_000);
                assert_eq!(checkpoint.usage.last_token_usage.input_tokens, 0);
                let history = serde_json::to_value(item.replacement_history.unwrap())?;
                assert!(!history.to_string().contains("<environment_context>"));
                installed = Some(checkpoint.usage);
                cuts.push(("replacement before usage", offset, last));
            }
            RolloutItem::TokenUsageContext {
                info, source_model, ..
            } if installed.is_some() && !saw_usage => {
                assert_eq!(Some(info), installed);
                assert!(source_model.is_none());
                saw_usage = true;
            }
            RolloutItem::ResponseItem(ResponseItem::Message { content, .. })
                if installed.is_some() =>
            {
                if matches!(&content[..], [ContentItem::InputText { text }] if text == prompt) {
                    cuts.push(("prompt before consumption", offset, last));
                    break;
                }
                context_items += 1;
                cuts.push(("partial context", offset, last));
            }
            RolloutItem::TurnContext(ctx) if installed.is_some() => {
                assert_eq!(ctx.model, dest);
                cuts.push(("reference before prompt", offset, last));
            }
            _ => {}
        }
    }
    assert!(saw_usage && context_items >= 2);
    for name in [
        "accepted before capture",
        "selected before first helper",
        "compact receipt before install",
        "replacement before usage",
        "prepared before first context item",
        "reference before prompt",
        "prompt before consumption",
    ] {
        assert!(cuts.iter().any(|&(label, _, _)| label == name), "{name}");
    }
    for name in ["notes marker", "notes answer"] {
        assert_eq!(
            cuts.iter().any(|&(label, _, _)| label == name),
            last == 3,
            "{name}"
        );
    }
    Ok(cuts)
}

#[test_case::test_case(false, false; "same model after an over-limit final answer")]
#[test_case::test_case(true, false; "downshift with work notes")]
#[test_case::test_case(true, true; "downshift with a final output schema")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fresh_preturn_replays_the_same_requests_at_every_installation_cut(
    downshift: bool,
    structured: bool,
) -> Result<()> {
    skip_if_no_network_unless_localhost!(Ok(()));
    const SOURCE: &str = "previous-provider-model";
    const DEST: &str = "destination-provider-model";
    const RETARGET: &str = "recovery-provider-model";
    const FRESH: &str = "Start the new task.";
    let dest = if downshift { DEST } else { SOURCE };
    let schema = structured.then(|| {
        json!({
            "type": "object", "properties": {"answer": {"type":"string"}},
            "required": ["answer"], "additionalProperties": false
        })
    });
    let cwd = tempfile::tempdir()?;
    let builder = |server: &wiremock::MockServer| {
        let url = format!("{}/v1", server.uri());
        let cwd = cwd.path().to_owned();
        test_codex().with_model(SOURCE).with_config(move |cfg| {
            configure_providers(cfg, url.clone(), url, true);
            cfg.cwd = cwd;
            cfg.user_instructions = Some("Use the canonical project instructions.".into());
            if downshift {
                // A replacement reestimated against startup would exceed the 40k destination trigger.
                cfg.model_overlay.as_mut().unwrap().models[0].final_instruction_override =
                    Some("source-only instructions ".repeat(9_000));
            }
        })
    };
    let mut replies = vec![responses::sse(vec![
        responses::ev_assistant_message("old", "Old task done."),
        responses::ev_completed_with_tokens("old", if downshift { 120_000 } else { 185_000 }),
    ])];
    if !structured {
        replies.push(responses::sse(vec![
            responses::ev_assistant_message("notes", NOTES),
            responses::ev_completed("notes"),
        ]));
    }
    replies.push(responses::sse(vec![
        ev_compaction("original-pre-turn"),
        responses::ev_completed_with_tokens("compact", 1_234),
    ]));
    replies.push(responses::sse(vec![
        responses::ev_assistant_message("done", r#"{"answer":"Finished."}"#),
        responses::ev_completed("done"),
    ]));
    let last = replies.len() - 1;
    let server = responses::start_mock_server().await;
    let original_posts = responses::mount_sse_sequence(&server, replies.clone()).await;
    let original = builder(&server).build(&server).await?;
    original
        .codex
        .submit(user_input("Complete the original task."))
        .await?;
    complete(&original.codex).await;
    let Op::UserInput { items, .. } = user_input(FRESH) else {
        unreachable!()
    };
    let next_cwd = cwd.path().join("new-task");
    std::fs::create_dir(&next_cwd)?;
    let mode = codex_protocol::config_types::CollaborationMode {
        mode: codex_protocol::config_types::ModeKind::Plan,
        settings: codex_protocol::config_types::Settings {
            model: dest.into(),
            reasoning_effort: None,
            developer_instructions: Some("Make a plan for the accepted task.".into()),
        },
    };
    original
        .codex
        .submit(Op::UserTurn {
            items,
            cwd: next_cwd.clone(),
            model: dest.into(),
            collaboration_mode: Some(mode),
            approval_policy: original.config.approval_policy.value(),
            sandbox_policy: original.config.sandbox_policy.get().clone(),
            summary: original.config.model_reasoning_summary,
            effort: None,
            service_tier: Some("priority".into()),
            personality: None,
            final_output_json_schema: schema.clone(),
        })
        .await?;
    complete(&original.codex).await;
    shutdown(&original.codex).await?;
    let expected: Vec<_> = original_posts
        .requests()
        .iter()
        .map(responses::ResponsesRequest::body_json)
        .collect();
    assert_eq!(expected.len(), last + 1);
    let work = expected[0]["input"].as_array().unwrap();
    for helper in &expected[1..last] {
        let input = helper["input"].as_array().unwrap();
        assert!(input.starts_with(work));
        assert_eq!(input.len(), work.len() + 2);
        assert_eq!(input[work.len()]["content"][0]["text"], "Old task done.");
        assert_eq!(helper["model"], SOURCE);
    }
    assert_compact_request(&expected[last - 1]);
    assert_eq!(expected[last]["model"], dest);
    let input = expected[last]["input"].to_string();
    assert!(input.contains("Make a plan for the accepted task."));
    assert!(input.contains(next_cwd.to_str().unwrap()));
    assert_eq!(
        expected[last].pointer("/text/format/schema"),
        schema.as_ref()
    );

    let path = original.codex.rollout_path().unwrap();
    let raw = std::fs::read_to_string(&path)?;
    let cuts = preturn_replay_cuts(&raw, FRESH, dest, last)?;
    for &(label, cut, start) in &cuts {
        std::fs::write(&path, &raw[..cut])?;
        let server = responses::start_mock_server().await;
        let posts = responses::mount_sse_sequence(&server, replies[start..].to_vec()).await;
        let resumed = builder(&server)
            .resume(&server, original.home.clone(), path.clone())
            .await?;
        resumed.codex.submit(Op::Continue).await?;
        complete(&resumed.codex).await;
        shutdown(&resumed.codex).await?;
        let requests = posts.requests();
        assert_eq!(requests.len(), expected.len() - start, "{label}");
        for (req, original) in requests.iter().zip(&expected[start..]) {
            let body = req.body_json();
            assert_same_input(
                body["input"].as_array().unwrap(),
                original["input"].as_array().unwrap(),
                cut,
            );
            for key in [
                "model",
                "tools",
                "tool_choice",
                "parallel_tool_calls",
                "include",
                "store",
                "text",
                "reasoning",
                "service_tier",
            ] {
                assert!(
                    body[key] == original[key],
                    "{label} at {cut}: different {key}"
                );
            }
        }
    }
    if downshift {
        let cut = cuts
            .iter()
            .find(|(label, ..)| *label == "partial context")
            .unwrap()
            .1;
        std::fs::write(&path, &raw[..cut])?;
        let server = responses::start_mock_server().await;
        let paused = builder(&server)
            .resume(&server, original.home.clone(), path.clone())
            .await?;
        paused.codex.submit(model_override(RETARGET)).await?;
        shutdown(&paused.codex).await?;
        let retarget = responses::mount_sse_sequence(&server, vec![replies[last].clone(); 2]).await;
        let resumed = builder(&server)
            .resume(&server, original.home.clone(), path)
            .await?;
        resumed.codex.submit(Op::Continue).await?;
        complete(&resumed.codex).await;
        resumed
            .codex
            .submit(user_input("A later unstructured request."))
            .await?;
        complete(&resumed.codex).await;
        shutdown(&resumed.codex).await?;
        let reqs = retarget.requests();
        assert_eq!(reqs.len(), 2);
        let first = reqs[0].body_json();
        assert_eq!(first["model"], RETARGET);
        assert_eq!(
            reqs[0].instructions_text(),
            "Recovery model final override."
        );
        assert_eq!(first.pointer("/text/format/schema"), schema.as_ref());
        assert_eq!(
            reqs[0]
                .message_input_texts("user")
                .iter()
                .filter(|s| s.as_str() == FRESH)
                .count(),
            1
        );
        assert!(reqs[1].body_json().pointer("/text/format/schema").is_none());
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn legacy_capture_recovers_inline_and_installs_prepared_skills_once() -> Result<()> {
    skip_if_no_network_unless_localhost!(Ok(()));
    const SOURCE: &str = "previous-provider-model";
    const DEST: &str = "destination-provider-model";
    const PROMPT: &str = "Use $first-skill and $second-skill on the new task.";
    let large = responses::start_mock_server().await;
    let small = responses::start_mock_server().await;
    let cwd = tempfile::tempdir()?;
    let builder = |model: &str, destination: &wiremock::MockServer| {
        let from = format!("{}/v1", large.uri());
        let to = format!("{}/v1", destination.uri());
        let cwd = cwd.path().to_owned();
        test_codex().with_model(model).with_config(move |cfg| {
            configure_providers(cfg, from, to, true);
            cfg.cwd = cwd;
        })
    };
    let home = std::sync::Arc::new(tempfile::tempdir()?);
    let mut items = vec![UserInput::Text {
        text: PROMPT.into(),
        text_elements: vec![],
    }];
    let mut skill_paths = Vec::new();
    for name in ["first-skill", "second-skill"] {
        let dir = home.path().join("skills").join(name);
        std::fs::create_dir_all(&dir)?;
        let path = dir.join("SKILL.md");
        std::fs::write(
            &path,
            format!("---\nname: {name}\ndescription: Test.\n---\n\nOriginal body for {name}.\n"),
        )?;
        let path = path.canonicalize()?;
        items.push(UserInput::Skill {
            name: name.into(),
            path: path.clone(),
        });
        skill_paths.push(path);
    }
    let initial = builder(SOURCE, &small)
        .with_home(home.clone())
        .build(&large)
        .await?;
    let sources = responses::mount_response_sequence(
        &large,
        vec![
            responses::sse_response(responses::sse(vec![
                responses::ev_assistant_message("old", "Old task complete."),
                responses::ev_completed_with_tokens("source", 120_000),
            ])),
            responses::sse_response(responses::sse_completed("unused"))
                .set_delay(Duration::from_secs(10)),
            responses::sse_response(responses::sse(vec![
                responses::ev_assistant_message("notes", NOTES),
                responses::ev_completed("notes"),
            ])),
            responses::sse_response(compact_response("installed-for-small")),
        ],
    )
    .await;
    initial
        .codex
        .submit(user_input("Work on the source model."))
        .await?;
    complete(&initial.codex).await;
    initial.codex.submit(model_override(DEST)).await?;
    initial
        .codex
        .submit(Op::UserInput {
            items,
            final_output_json_schema: None,
        })
        .await?;
    tokio::time::timeout(Duration::from_secs(5), async {
        while sources.requests().len() < 2 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await?;
    shutdown(&initial.codex).await?;
    let path = initial.codex.rollout_path().unwrap();
    let raw = std::fs::read_to_string(&path)?;
    let mut cutoff = 0;
    let mut accepted = false;
    for line in raw.split_inclusive('\n') {
        cutoff += line.len();
        match serde_json::from_str::<RolloutLine>(line)?.item {
            RolloutItem::PostTurnCompletionReviewState(s) => {
                accepted |= s["pending_inputs"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|p| p["new_task"] == true && p["content"][0]["text"] == PROMPT);
            }
            RolloutItem::ResponseItem(ResponseItem::Message { role, content, .. })
                if role == "developer"
                    && matches!(&content[..], [ContentItem::InputText { text }] if text.starts_with("<AUTO_COMPACT_WORK_NOTES_REQUEST>")) =>
            {
                break;
            }
            _ => {}
        }
    }
    assert!(accepted && cutoff < raw.len());
    std::fs::write(&path, without_compaction_decision(&raw[..cutoff])?)?;
    let finish = || {
        responses::sse(vec![
            responses::ev_assistant_message("answer", "Finished."),
            responses::ev_completed("done"),
        ])
    };
    let first = responses::mount_sse_once(&small, finish()).await;
    let recovered = builder(DEST, &small)
        .resume(&small, home.clone(), path.clone())
        .await?;
    recovered.codex.submit(Op::Continue).await?;
    complete(&recovered.codex).await;
    let expected_input = first.single_request();
    let ordered = |req: &responses::ResponsesRequest| {
        req.message_input_texts("user")
            .into_iter()
            .filter(|s| s == PROMPT || s.starts_with("<skill>"))
            .collect::<Vec<_>>()
    };
    let expected = ordered(&expected_input);
    assert_eq!(expected_input.body_json()["model"], DEST);
    assert_eq!(expected.len(), 3);
    assert_eq!(expected[0], PROMPT);
    assert!(expected[1].contains("Original body for first-skill."));
    assert!(expected[2].contains("Original body for second-skill."));
    for helper in &sources.requests()[1..] {
        assert!(ordered(helper).is_empty());
    }
    shutdown(&recovered.codex).await?;

    let raw = std::fs::read_to_string(&path)?;
    let mut cuts = Vec::new();
    let mut end = 0;
    let mut installed = false;
    for line in raw.split_inclusive('\n') {
        end += line.len();
        match serde_json::from_str::<RolloutLine>(line)?.item {
            RolloutItem::Compacted(saved) => {
                let reference = saved.checkpoint.unwrap().injected_context.unwrap();
                assert_eq!(reference.model, DEST);
                installed = true;
                cuts.push(end);
            }
            RolloutItem::TurnContext(ctx) if installed && cuts.len() == 1 => {
                assert_eq!(ctx.model, DEST);
                cuts.push(end);
            }
            RolloutItem::PostTurnCompletionReviewState(s)
                if installed
                    && cuts.len() == 2
                    && s["pending_inputs"].as_array().unwrap().iter().any(|p| {
                        p["installation"] == "Installing"
                            && p["remaining_skill_items"]
                                .as_array()
                                .is_some_and(|s| s.len() == 2)
                    }) =>
            {
                cuts.push(end)
            }
            RolloutItem::ResponseItem(ResponseItem::Message { role, content, .. })
                if installed
                    && role == "user"
                    && matches!(&content[..], [ContentItem::InputText { text }] if text == PROMPT || text.starts_with("<skill>")) =>
            {
                cuts.push(end)
            }
            _ => {}
        }
    }
    assert_eq!(
        cuts.len(),
        6,
        "inline marker, reference, prepared projection, prompt and each skill"
    );
    for (index, cut) in cuts.into_iter().enumerate() {
        // The installed destination wins over both the old user-turn model and the
        // configured startup model. Once prepared, skill bytes no longer come from disk.
        if index == 2 {
            for skill in &skill_paths {
                std::fs::remove_file(skill)?;
            }
        }
        std::fs::write(&path, &raw[..cut])?;
        let server = responses::start_mock_server().await;
        let ordinary = responses::mount_sse_once(&server, finish()).await;
        let resumed = builder(SOURCE, &server)
            .resume(&server, home.clone(), path.clone())
            .await?;
        resumed.codex.submit(Op::Continue).await?;
        complete(&resumed.codex).await;
        let req = ordinary.single_request();
        assert_eq!(req.body_json()["model"], DEST);
        assert_same_input(&req.input(), &expected_input.input(), index);
        assert_eq!(
            base_instructions_text(&req.body_json()),
            "Destination model final override."
        );
        assert!(
            req.input()
                .iter()
                .any(|i| i["encrypted_content"] == "installed-for-small")
        );
        assert_eq!(ordered(&req), expected);
        shutdown(&resumed.codex).await?;
        let scope = std::fs::read_to_string(&path)?
            .lines()
            .rev()
            .map(serde_json::from_str::<RolloutLine>)
            .find_map(|l| match l.unwrap().item {
                RolloutItem::PostTurnCompletionReviewState(s) => Some(s),
                _ => None,
            })
            .unwrap();
        let selected: Vec<_> = std::iter::once(PROMPT.to_owned())
            .chain(
                ["first-skill", "second-skill"]
                    .into_iter()
                    .zip(&skill_paths)
                    .map(|(name, path)| format!("Selected skill: {name} ({})", path.display())),
            )
            .collect();
        assert_eq!(
            scope["request_frame"]["current_user_messages"],
            json!(selected)
        );
        assert_eq!(scope["pending_inputs"], json!([]));
    }
    assert_eq!(sources.requests().len(), 4);
    Ok(())
}

#[test_case::test_case(false, false, false, false, false; "failed continue")]
#[test_case::test_case(false, true, false, false, false; "failed new input")]
#[test_case::test_case(true, false, false, false, false; "paused continue")]
#[test_case::test_case(true, true, false, false, false; "paused new input")]
#[test_case::test_case(false, false, true, false, false; "failed restart continue")]
#[test_case::test_case(false, true, true, false, false; "failed restart new input")]
#[test_case::test_case(true, false, true, false, false; "paused restart continue")]
#[test_case::test_case(true, true, true, false, false; "paused restart new input")]
#[test_case::test_case(false, false, true, true, false; "legacy capture restart continue")]
#[test_case::test_case(false, false, false, false, true; "failed continue with model override")]
#[test_case::test_case(true, false, true, false, true; "paused restart with model override")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn model_downshift_recovery_preserves_source_and_destination(
    pause: bool,
    new_input: bool,
    restart: bool,
    legacy_capture: bool,
    recovery_override: bool,
) -> Result<()> {
    skip_if_no_network_unless_localhost!(Ok(()));
    let (release, gate) = oneshot::channel();
    let (old_server, _) = start_streaming_sse_server(vec![
        vec![chunk(vec![
            responses::ev_assistant_message("old", "old model work"),
            responses::ev_completed_with_tokens("old", 160_000),
        ])],
        vec![chunk(vec![
            multipart_notes(),
            responses::ev_completed_with_tokens("notes", 10),
        ])],
        vec![
            chunk(vec![responses::ev_response_created("failed")]),
            StreamingSseChunk {
                gate: Some(gate),
                body: responses::sse(vec![ev_compaction("discarded")]),
            },
        ],
        vec![chunk(vec![
            ev_compaction("recovered"),
            responses::ev_completed("compact"),
        ])],
    ])
    .await;
    let new_server = responses::start_mock_server().await;
    let followup = responses::mount_sse_once(
        &new_server,
        responses::sse(vec![
            responses::ev_assistant_message("new", "destination work"),
            responses::ev_completed("new"),
        ]),
    )
    .await;
    let old_base = format!("{}/v1", old_server.uri());
    let new_base = format!("{}/v1", new_server.uri());
    let configure = move |config: &mut codex_core::config::Config| {
        configure_providers(config, old_base.clone(), new_base.clone(), true);
    };
    let test = test_codex()
        .with_model("previous-provider-model")
        .with_config(configure.clone())
        .build_with_streaming_server(&old_server)
        .await?;
    // The real payload fits the source window but exceeds the destination.
    // Tiny reported notes usage must not erase the pending downshift.
    test.codex
        .submit(user_input(&"older source data\n".repeat(35_000)))
        .await?;
    complete(&test.codex).await;
    test.codex
        .submit(model_override("destination-provider-model"))
        .await?;
    test.codex
        .submit(user_input("start destination work"))
        .await?;
    wait_for_compaction(&test.codex).await;
    wait_for_requests(&old_server, 3).await;
    if pause {
        test.codex.submit(Op::Pause).await?;
        wait_for_event(&test.codex, |event| {
            matches!(event, EventMsg::TurnPaused(_))
        })
        .await;
    } else {
        release.send(()).unwrap();
        wait_for_event(&test.codex, |event| matches!(event, EventMsg::Error(_))).await;
        complete(&test.codex).await;
    }
    let path = test.session_configured.rollout_path.as_ref().unwrap();
    assert!(replacement_history(path).is_empty());
    let resumed;
    let active = if restart {
        test.codex.submit(Op::Shutdown).await?;
        wait_for_event(&test.codex, |event| {
            matches!(event, EventMsg::ShutdownComplete)
        })
        .await;
        if legacy_capture {
            // Exercise rollouts from before source-model metadata was added.
            let raw = std::fs::read_to_string(path)?;
            let source_line = r#"Compaction source model: \"previous-provider-model\"\n"#;
            assert!(raw.contains(source_line));
            std::fs::write(
                path,
                without_compaction_decision(&raw.replace(source_line, ""))?,
            )?;
        }
        resumed = test_codex()
            .with_model("destination-provider-model")
            .with_config(configure)
            .resume(&new_server, test.home.clone(), path.clone())
            .await?;
        &resumed
    } else {
        &test
    };
    let (destination, instructions, limit) = if recovery_override {
        active
            .codex
            .submit(model_override("recovery-provider-model"))
            .await?;
        (
            "recovery-provider-model",
            "Recovery model final override.",
            80_000,
        )
    } else {
        (
            "destination-provider-model",
            "Destination model final override.",
            40_000,
        )
    };
    active
        .codex
        .submit(if new_input {
            user_input("fresh destination input")
        } else {
            Op::Continue
        })
        .await?;
    complete(&active.codex).await;

    let requests = old_server.requests().await;
    assert_eq!(requests.len(), 4);
    let bodies: Vec<Value> = requests
        .iter()
        .map(|body| serde_json::from_slice(body).unwrap())
        .collect();
    for body in &bodies {
        assert_eq!(body["model"], "previous-provider-model");
        assert_eq!(
            base_instructions_text(body),
            "Previous model final override."
        );
    }
    assert!(
        bodies[1]
            .to_string()
            .contains(r#"Compaction source model: \"previous-provider-model\""#)
    );
    for body in &bodies[2..] {
        assert_compact_request(body);
        assert!(!body.to_string().contains("AUTO_COMPACT_WORK_NOTES"));
        assert!(!body.to_string().contains("Compaction source model:"));
    }
    let body = followup.single_request().body_json();
    assert_eq!(body["model"], destination);
    assert_eq!(base_instructions_text(&body), instructions);
    assert!(!body.to_string().contains("discarded"));
    assert!(!body.to_string().contains("compaction_trigger"));
    assert_eq!(
        user_messages(&body)
            .iter()
            .filter(|text| text.ends_with(NOTES))
            .count(),
        1,
    );
    assert_eq!(
        user_messages(&body)
            .iter()
            .filter(|text| *text == "fresh destination input")
            .count(),
        usize::from(new_input),
    );
    let replacement = replacement_history(active.session_configured.rollout_path.as_ref().unwrap());
    assert_eq!(replacement.len(), 1);
    // This also catches accidentally applying the source model's 64K retention
    // budget instead of the selected destination's auto-compaction ceiling.
    let request_tokens: usize = ["input", "tools"]
        .iter()
        .map(|key| body[key].to_string().chars().count().div_ceil(4))
        .sum();
    assert!(
        request_tokens < limit,
        "destination request needs {request_tokens} tokens"
    );
    let lines: Vec<_> =
        std::fs::read_to_string(active.session_configured.rollout_path.as_ref().unwrap())?
            .lines()
            .map(|line| serde_json::from_str::<RolloutLine>(line).unwrap().item)
            .collect();
    let index = lines
        .iter()
        .position(|item| matches!(item, RolloutItem::Compacted(_)))
        .unwrap();
    let reference = if !legacy_capture || new_input {
        assert!(
            !matches!(&lines[index + 1], RolloutItem::TurnContext(_)),
            "a fresh pre-turn capture keeps its no-injection placement"
        );
        lines[index + 1..]
            .iter()
            .find_map(|item| {
                if let RolloutItem::TurnContext(reference) = item {
                    Some(reference)
                } else {
                    None
                }
            })
            .expect("new input must seed destination context")
    } else if let RolloutItem::TurnContext(reference) = &lines[index + 1] {
        reference
    } else {
        panic!("continuation must persist an adjacent destination reference context");
    };
    assert_eq!(reference.model, destination);
    old_server.shutdown().await;
    Ok(())
}
