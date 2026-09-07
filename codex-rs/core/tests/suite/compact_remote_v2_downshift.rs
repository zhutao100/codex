use super::*;
use pretty_assertions::assert_eq;

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
        .insert("destination".into(), destination);
    config.model_overlay = Some(ModelOverlay {
        models: [
            ("previous", 200_000, 180_000, "Previous"),
            ("destination", 100_000, 40_000, "Destination"),
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

#[test_case::test_case(false, false, false, false; "failed continue")]
#[test_case::test_case(false, true, false, false; "failed new input")]
#[test_case::test_case(true, false, false, false; "paused continue")]
#[test_case::test_case(true, true, false, false; "paused new input")]
#[test_case::test_case(false, false, true, false; "failed restart continue")]
#[test_case::test_case(false, true, true, false; "failed restart new input")]
#[test_case::test_case(true, false, true, false; "paused restart continue")]
#[test_case::test_case(true, true, true, false; "paused restart new input")]
#[test_case::test_case(false, false, true, true; "legacy capture restart continue")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn model_downshift_recovery_preserves_source_and_destination(
    pause: bool,
    new_input: bool,
    restart: bool,
    legacy_capture: bool,
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
        .submit(Op::OverrideTurnContext {
            cwd: None,
            approval_policy: None,
            sandbox_policy: None,
            windows_sandbox_level: None,
            model: Some("destination-provider-model".into()),
            effort: None,
            summary: None,
            collaboration_mode: None,
            personality: None,
            service_tier: None,
        })
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
            std::fs::write(path, raw.replace(source_line, ""))?;
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
        assert_eq!(body["instructions"], "Previous model final override.");
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
    assert_eq!(body["model"], "destination-provider-model");
    assert_eq!(body["instructions"], "Destination model final override.");
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
    // budget instead of the destination's 40K auto-compaction ceiling.
    let request_tokens: usize = ["input", "instructions", "tools"]
        .iter()
        .map(|key| body[key].to_string().chars().count().div_ceil(4))
        .sum();
    assert!(
        request_tokens < 40_000,
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
    let reference = if new_input {
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
    assert_eq!(reference.model, "destination-provider-model");
    old_server.shutdown().await;
    Ok(())
}
