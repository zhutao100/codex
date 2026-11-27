use super::*;
use crate::auth::AuthCredentialsStoreMode;
use crate::auth::AuthMode;
use crate::client_common::SampledControls;
use crate::model_provider_info::CheckpointRoute;
use crate::session::tests::make_session_and_context;
use codex_protocol::config_types::Verbosity;
use core_test_support::responses;
use pretty_assertions::assert_eq;
use serde_json::json;
use wiremock::Mock;
use wiremock::matchers::method;
use wiremock::matchers::path_regex;

#[derive(Clone, Copy, PartialEq, Eq)]
enum LoginChange {
    None,
    DuringNotes,
    DuringUpgrade,
    ReturnDuringRecovery { decision: bool },
}

pub(super) fn save_chatgpt_login(home: &std::path::Path) -> anyhow::Result<()> {
    let dummy = CodexAuth::create_dummy_chatgpt_auth_for_testing();
    let mut tokens = dummy.get_token_data()?;
    tokens.id_token = crate::token_data::parse_id_token("e30.e30.test")?;
    crate::auth::save_auth(
        home,
        &crate::auth::AuthDotJson {
            auth_mode: Some(dummy.api_auth_mode()),
            openai_api_key: None,
            tokens: Some(tokens),
            last_refresh: Some(chrono::Utc::now()),
        },
        AuthCredentialsStoreMode::File,
    )?;
    Ok(())
}

pub(super) async fn sample_work(
    session: &Arc<Session>,
    context: &Arc<TurnContext>,
    client: &mut ModelClientSession,
) -> anyhow::Result<()> {
    run_sampling_request(
        session.clone(),
        context.clone(),
        None,
        Arc::new(Mutex::new(TurnDiffTracker::new())),
        client,
        None,
        session.prompt_history(context).await,
        0,
        &HashMap::new(),
        None,
        None,
        SamplingPurpose::Work,
        CancellationToken::new(),
    )
    .await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn opening_refusal_keeps_the_attempted_endpoint_across_login_change() -> anyhow::Result<()> {
    core_test_support::skip_if_no_network_unless_localhost!(Ok(()));
    let chat = responses::start_mock_server().await;
    let api = responses::start_mock_server().await;
    let home = tempfile::tempdir()?;
    let auth = AuthManager::from_auth_for_testing_with_home(
        CodexAuth::create_dummy_chatgpt_auth_for_testing(),
        home.path().into(),
    );
    crate::auth::login_with_api_key(home.path(), "test-api-key", AuthCredentialsStoreMode::File)?;
    let (mut session, mut context) = make_session_and_context().await;
    context.provider = crate::ModelProviderInfo::create_openai_provider();
    context.provider.base_url = None;
    context.provider.supports_websockets = false;
    context.auth_manager = Some(auth.clone());
    context.model_info.context_window = Some(32_000);
    let attempted = context.current_endpoint();
    assert!(attempted.is_some());
    session.services.auth_manager = auth.clone();
    session.services.model_client =
        routed_client(&session, &context, auth.clone(), chat.uri(), api.uri())?;
    Mock::given(method("POST"))
        .and(path_regex("/responses$"))
        .respond_with(move |_: &wiremock::Request| {
            assert!(auth.reload());
            wiremock::ResponseTemplate::new(400)
                .set_body_json(json!({"error":{"code":"context_length_exceeded"}}))
        })
        .expect(1)
        .mount(&chat)
        .await;
    let (session, context) = (Arc::new(session), Arc::new(context));
    let mut client = session.services.model_client.new_session();
    let error = sample_work(&session, &context, &mut client)
        .await
        .err()
        .ok_or_else(|| anyhow::anyhow!("expected context refusal"))?;
    assert!(matches!(
        error.downcast_ref::<CodexErr>(),
        Some(CodexErr::ContextWindowExceeded)
    ));
    let current = context.current_endpoint();
    assert_ne!(current, attempted);
    assert_eq!(responses::model_post_count(&api).await?, 0);
    let state = session.state.lock().await;
    let refused = |route| {
        state
            .history
            .rejected_on(context.usage_source(route), context.model_context_window())
    };
    assert!(refused(attempted.as_deref()));
    assert!(!refused(current.as_deref()));
    Ok(())
}

#[test_case::test_case(false, true; "unchanged endpoint uses the measured review size")]
#[test_case::test_case(true, false; "new endpoint admits locally fitting review directly")]
#[test_case::test_case(true, true; "new endpoint checkpoints instead of sending oversized review")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ordinary_review_after_upgrade_uses_sending_endpoint(
    switch: bool,
    large: bool,
) -> anyhow::Result<()> {
    core_test_support::skip_if_no_network_unless_localhost!(Ok(()));
    let chat = responses::start_mock_server().await;
    let api = responses::start_mock_server().await;
    let home = tempfile::tempdir()?;
    let auth = AuthManager::from_auth_for_testing_with_home(
        CodexAuth::create_dummy_chatgpt_auth_for_testing(),
        home.path().into(),
    );
    crate::auth::login_with_api_key(home.path(), "test-api-key", AuthCredentialsStoreMode::File)?;
    let (mut session, mut context) = make_session_and_context().await;
    context.provider = crate::ModelProviderInfo::create_openai_provider();
    context.provider.base_url = None;
    context.provider.supports_websockets = true;
    context.auth_manager = Some(auth.clone());
    context.model_info.context_window = Some(32_000);
    context.model_info.auto_compact_token_limit = Some(32_000);
    context.model_info.effective_context_window_percent = 100;
    Arc::make_mut(&mut context.config).compaction_mode = crate::config::types::CompactionMode::Auto;
    session.services.auth_manager = auth.clone();
    session.services.model_client =
        routed_client(&session, &context, auth.clone(), chat.uri(), api.uri())?;
    let session = Arc::new(session);
    let mut context = Arc::new(context);
    let input = vec![UserInput::Text {
        text: "Verify the evidence.".into(),
        text_elements: vec![],
    }];
    let revision = session
        .accept_completion_review_input(&input, true, None)
        .await
        .unwrap();
    assert!(
        session
            .install_completion_review_input(&context, revision, true)
            .await
    );
    assert!(
        session
            .start_completion_review_main_task(&context.sub_id, &context)
            .await
    );
    let earlier: ResponseItem = serde_json::from_value(
        json!({"type":"message","role":"assistant","content":[{"type":"output_text","text":"Prior evidence row.\n".repeat(if large {5_000} else {1})}]}),
    )?;
    session
        .record_conversation_items(&context, &[earlier])
        .await;
    let call = serde_json::from_value(
        json!({"type":"function_call","name":"read_file","arguments":"{}","call_id":"evidence"}),
    )?;
    let result = serde_json::from_value(
        json!({"type":"function_call_output","call_id":"evidence","output":"Keep this evidence row.\n".repeat(160)}),
    )?;
    session
        .record_conversation_items(&context, &[call, result])
        .await;
    let answer = |id, text| {
        responses::sse(vec![
            responses::ev_assistant_message(id, text),
            responses::ev_completed_with_tokens(id, 12_000),
        ])
    };
    let candidate = format!(
        "Original answer. {}",
        "Check evidence against the source.\n".repeat(if large { 700 } else { 0 })
    );
    let ordinary = responses::mount_sse_once(&chat, answer("candidate", &candidate)).await;
    let mut http = context.provider.clone();
    http.supports_websockets = false;
    let mut work = session
        .services
        .model_client
        .new_session_with_provider(http);
    sample_work(&session, &context, &mut work).await?;
    assert_eq!(ordinary.requests().len(), 1);
    session
        .capture_completed_regular_turn_for_review(&context, Some(&candidate))
        .await;
    if large {
        let work = session.prompt_history(&context).await;
        let tools =
            compaction_tool_specs(&session, &context, &work, &CancellationToken::new()).await?;
        let base = session.get_base_instructions_for_turn(&context).await;
        let outgoing = work.iter().map(estimate_item_token_count).sum::<i64>()
            + tools.tokens()
            + approx_token_count(&base.text) as i64;
        let window = outgoing + outgoing / 18 + 650;
        let ctx = Arc::get_mut(&mut context).unwrap();
        ctx.model_info.context_window = Some(window);
        ctx.model_info.auto_compact_token_limit = Some(window);
        assert!(outgoing <= unsampled_review_token_limit(ctx));
        assert!(outgoing + approx_token_count(&candidate) as i64 > window);
    }
    let ticket = session.begin_completion_review(false).await.unwrap();
    assert!(
        session
            .prepare_completion_review_handoff(
                &ticket,
                "Check all evidence.".into(),
                context.model_info.slug.clone()
            )
            .await
    );
    let continuation = session
        .claim_completion_review_handoff(&ticket.attempt_id, &context.sub_id, false)
        .await
        .unwrap();
    let login = auth.clone();
    Mock::given(method("GET"))
        .and(path_regex(".*/responses$"))
        .respond_with(move |_: &wiremock::Request| {
            if switch {
                assert!(login.reload());
            }
            wiremock::ResponseTemplate::new(426)
        })
        .expect(1)
        .mount(&chat)
        .await;
    let checkpoint = switch && large;
    let mut replies = Vec::new();
    if checkpoint {
        replies.extend([answer("notes", "<AUTO_COMPACT_WORK_NOTES>\nKeep the evidence.\n</AUTO_COMPACT_WORK_NOTES>"),
            responses::sse(vec![json!({"type":"response.output_item.done","item":{"type":"compaction","encrypted_content":"small summary"}}), responses::ev_completed("compact")])]);
    }
    replies.push(answer("final", "Verified."));
    let wire = responses::mount_sse_sequence(if switch { &api } else { &chat }, replies).await;
    let result = continue_turn(session, context, continuation, CancellationToken::new()).await;
    let sent = wire.requests();
    assert_eq!(
        responses::model_post_count(&chat).await?,
        if switch { 1 } else { 2 }
    );
    assert_eq!(
        responses::model_post_count(&api).await?,
        if checkpoint { 3 } else { usize::from(switch) }
    );
    assert_eq!(result.as_deref(), Some("Verified."));
    if checkpoint {
        let notes = sent[0].input();
        let compact = sent[1].input();
        assert!(
            notes.last().unwrap()["content"][0]["text"]
                .as_str()
                .unwrap()
                .starts_with("<AUTO_COMPACT_WORK_NOTES_REQUEST>")
        );
        assert_eq!(compact.last(), Some(&json!({"type":"compaction_trigger"})));
        let hot = ordinary.single_request().input();
        for input in [&notes, &compact] {
            let work = &input[..input.len() - 1];
            assert_eq!(work.len(), hot.len() + 1);
            assert_eq!(work[..hot.len()], hot);
            assert_eq!(work[hot.len()]["content"][0]["text"], candidate);
        }
        assert_eq!(
            sent[0].function_call_output("evidence"),
            ordinary.single_request().function_call_output("evidence")
        );
        assert!(
            !sent[2]
                .body_bytes()
                .windows(b"Prior evidence row.".len())
                .any(|w| w == b"Prior evidence row.")
        );
    }
    let last = sent
        .last()
        .unwrap()
        .message_input_texts("developer")
        .join("\n");
    assert!(
        last.contains("Check all evidence.")
            && last.contains("Verify the evidence.")
            && last.contains("Original answer.")
    );
    Ok(())
}

pub(super) fn routed_client(
    session: &Session,
    context: &TurnContext,
    auth: Arc<AuthManager>,
    chat: String,
    api: String,
) -> anyhow::Result<crate::client::ModelClient> {
    let mut client = crate::client::ModelClient::new(
        Some(auth),
        session.conversation_id,
        context.provider.clone(),
        context.session_source.clone(),
        None,
        crate::config::types::ResponsesTransport::Auto,
        false,
        false,
        None,
        false,
    );
    client.loopback_routes = Some(Arc::new(HashMap::from([
        (
            context
                .provider
                .to_api_provider(Some(AuthMode::Chatgpt))?
                .base_url,
            chat,
        ),
        (
            context
                .provider
                .to_api_provider(Some(AuthMode::ApiKey))?
                .base_url,
            api,
        ),
    ])));
    Ok(client)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn manual_recovery_counts_the_compaction_attempt_once() -> anyhow::Result<()> {
    core_test_support::skip_if_no_network_unless_localhost!(Ok(()));
    use codex_otel::metrics::MetricsConfig;
    use opentelemetry_sdk::metrics::InMemoryMetricExporter;
    use opentelemetry_sdk::metrics::data::AggregatedMetrics;
    use opentelemetry_sdk::metrics::data::MetricData;
    let server = responses::start_mock_server().await;
    Mock::given(method("GET"))
        .and(path_regex("/responses$"))
        .respond_with(wiremock::ResponseTemplate::new(426))
        .mount(&server)
        .await;
    let request = responses::mount_sse_once(&server, responses::sse(vec![
        json!({"type":"response.output_item.done","item":{"type":"compaction","encrypted_content":"summary"}}),
        responses::ev_completed("compact"),
    ])).await;
    let (mut session, mut context) = make_session_and_context().await;
    let (tx, events) = async_channel::unbounded();
    session.tx_event = tx;
    let meter = context.otel_manager.clone().with_metrics_config(
        MetricsConfig::in_memory(
            "test",
            "codex-core",
            crate::CODEX_VERSION,
            InMemoryMetricExporter::default(),
        )
        .with_runtime_reader(),
    )?;
    context.otel_manager = meter.clone();
    session.services.otel_manager = meter.clone();
    session.services.model_client = routed_client(
        &session,
        &context,
        session.services.auth_manager.clone(),
        server.uri(),
        server.uri(),
    )?;
    let session = Arc::new(session);
    session
        .checkpoint_auto_compaction(
            &context,
            crate::compact::InitialContextInjection::DoNotInject,
            &crate::compact::WorkFit::local(),
            0,
            false,
        )
        .await?;
    crate::tasks::SessionTask::run(
        Arc::new(crate::tasks::CompactTask),
        Arc::new(crate::tasks::SessionTaskContext::new(session)),
        Arc::new(context),
        vec![],
        CancellationToken::new(),
    )
    .await;
    while let Ok(event) = events.try_recv() {
        if let EventMsg::Error(error) = event.msg {
            anyhow::bail!("{}", error.message);
        }
    }
    assert_eq!(
        request.single_request().input().last().unwrap()["type"],
        "compaction_trigger"
    );
    let snapshot = meter.snapshot_metrics()?;
    let counter = snapshot
        .scope_metrics()
        .flat_map(opentelemetry_sdk::metrics::data::ScopeMetrics::metrics)
        .find(|m| m.name() == "codex.task.compact")
        .ok_or_else(|| anyhow::anyhow!("missing compaction counter"))?;
    let AggregatedMetrics::U64(MetricData::Sum(sum)) = counter.data() else {
        anyhow::bail!("expected an unsigned counter")
    };
    assert_eq!(
        sum.data_points()
            .map(opentelemetry_sdk::metrics::data::SumDataPoint::value)
            .sum::<u64>(),
        1
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn accepted_socket_checks_review_before_response_create() -> anyhow::Result<()> {
    core_test_support::skip_if_no_network_unless_localhost!(Ok(()));
    let server =
        responses::start_websocket_server(vec![vec![vec![responses::ev_completed("fits")]]]).await;
    let home = tempfile::tempdir()?;
    let auth = AuthManager::from_auth_for_testing_with_home(
        CodexAuth::create_dummy_chatgpt_auth_for_testing(),
        home.path().into(),
    );
    let (session, mut context) = make_session_and_context().await;
    context.auth_manager = Some(auth.clone());
    context.provider = crate::ModelProviderInfo::create_openai_provider();
    context.provider.base_url = None;
    context.provider.supports_websockets = true;
    let measured = context.current_endpoint().unwrap();
    let model_client = routed_client(
        &session,
        &context,
        auth.clone(),
        server.uri().into(),
        server.uri().into(),
    )?;
    let mut client = model_client.new_session_with_provider(context.provider.clone());
    crate::auth::login_with_api_key(home.path(), "test-api-key", AuthCredentialsStoreMode::File)?;
    assert!(auth.reload());
    assert_ne!(context.current_endpoint().as_ref(), Some(&measured));
    for (size, send) in [(1_001, false), (1_000, true)] {
        let prompt = Prompt {
            base_instructions: BaseInstructions {
                text: "a".repeat(size * 4),
            },
            review_admission: Some(crate::client_common::ReviewAdmission {
                proposal: None,
                target: 1_000,
                measured: Some((measured.clone(), 100)),
            }),
            ..Default::default()
        };
        let result = client
            .stream(
                &prompt,
                &context.model_info,
                &context.otel_manager,
                context.reasoning_effort,
                context.reasoning_summary,
                None,
                None,
            )
            .await;
        if send {
            let mut stream = result?;
            while let Some(event) = stream.next().await {
                event?;
            }
        } else {
            assert!(matches!(
                result,
                Err(CodexErr::ReviewPayloadExceeded {
                    estimated: 1_001,
                    target: 1_000,
                    ..
                })
            ));
        }
    }
    assert_eq!(server.single_connection().len(), 1);
    server.shutdown().await;
    Ok(())
}

#[test_case::test_case(LoginChange::None, false; "same route retains its positive correction")]
#[test_case::test_case(LoginChange::DuringNotes, false; "new route restores locally fitting work")]
#[test_case::test_case(LoginChange::None, true; "same route retains its negative correction")]
#[test_case::test_case(LoginChange::DuringNotes, true; "new route shortens locally overflowing work")]
#[test_case::test_case(LoginChange::DuringUpgrade, true; "HTTP fallback refits notes after login during WebSocket upgrade")]
#[test_case::test_case(LoginChange::ReturnDuringRecovery { decision: false }, true; "recovery before the decision on B restores measured V2 on A")]
#[test_case::test_case(LoginChange::ReturnDuringRecovery { decision: true }, true; "recovery with the saved decision on B restores measured V2 on A")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn live_login_between_notes_and_compact(
    change: LoginChange,
    local_over: bool,
) -> anyhow::Result<()> {
    core_test_support::skip_if_no_network_unless_localhost!(Ok(()));
    const NOTES: &str =
        "<AUTO_COMPACT_WORK_NOTES>\nFinish the verification.\n</AUTO_COMPACT_WORK_NOTES>";
    let chat = responses::start_mock_server().await;
    let api = responses::start_mock_server().await;
    let home = tempfile::tempdir()?;
    let evidence = home.path().join("evidence.txt");
    std::fs::write(&evidence, "Complete evidence row\n".repeat(400))?;
    let auth = AuthManager::from_auth_for_testing_with_home(
        CodexAuth::create_dummy_chatgpt_auth_for_testing(),
        home.path().into(),
    );
    // Only the disposable file changes. The running manager retains ChatGPT auth until reload.
    crate::auth::login_with_api_key(home.path(), "test-api-key", AuthCredentialsStoreMode::File)?;
    let (mut session, mut context) = make_session_and_context().await;
    context.provider = crate::ModelProviderInfo::create_openai_provider();
    context.provider.base_url = None;
    let upgrade = change == LoginChange::DuringUpgrade;
    let returning = matches!(change, LoginChange::ReturnDuringRecovery { .. });
    let notes_on_api = upgrade || returning;
    let compact_on_api = matches!(
        change,
        LoginChange::DuringUpgrade | LoginChange::DuringNotes
    );
    context.provider.supports_websockets = upgrade;
    context.auth_manager = Some(auth.clone());
    context.model_info.context_window = Some(32_000);
    context.model_info.auto_compact_token_limit = Some(27_000);
    context.model_info.effective_context_window_percent = 100;
    context.model_info.supports_reasoning_summaries = false;
    context.model_info.supports_parallel_tool_calls = true;
    context.verbosity = SampledVerbosity::Omitted;
    context.sampled_controls = Some(SampledControls {
        route: CheckpointRoute::new(
            &context.model_provider_id,
            context.current_endpoint().as_deref(),
        )
        .unwrap(),
        reasoning: SampledReasoning::Enabled(Some(
            codex_protocol::openai_models::ReasoningEffort::Low,
        )),
        verbosity: SampledVerbosity::Level(Verbosity::High),
        parallel_tool_calls: Some(false),
    });
    context.cwd = home.path().into();
    context.tools_config.experimental_supported_tools = vec!["read_file".into()];
    context.truncation_policy = crate::truncate::TruncationPolicy::Tokens(20_000);
    Arc::make_mut(&mut context.config).compaction_mode = crate::config::types::CompactionMode::Auto;
    session.services.auth_manager = auth.clone();
    session.services.model_client =
        routed_client(&session, &context, auth.clone(), chat.uri(), api.uri())?;
    let session = Arc::new(session);
    let context = Arc::new(context);
    let recording = if returning {
        Some(crate::session::tests::attach_recorder(&session, &context).await)
    } else {
        None
    };
    let ordinary = responses::mount_sse_once(
        &chat,
        responses::sse(vec![
            responses::ev_function_call(
                "read",
                "read_file",
                &json!({"file_path":evidence}).to_string(),
            ),
            responses::ev_completed_with_tokens("work", if local_over { 20_000 } else { 31_800 }),
        ]),
    )
    .await;
    let user: ResponseItem = serde_json::from_value(
        json!({"type":"message","role":"user","content":[{"type":"input_text","text":"Project working transcript.\n".repeat(if local_over {5_100} else {1})}]}),
    )?;
    session.record_conversation_items(&context, &[user]).await;
    // The original work uses HTTP; the capture can independently attempt a socket upgrade.
    let mut work_provider = context.provider.clone();
    work_provider.supports_websockets = false;
    let mut work_client = session
        .services
        .model_client
        .new_session_with_provider(work_provider);
    sample_work(&session, &context, &mut work_client).await?;
    assert_eq!(
        ordinary.single_request().body_json()["parallel_tool_calls"],
        false
    );
    assert_eq!(
        ordinary.single_request().body_json()["text"]["verbosity"],
        "high"
    );
    let original = session
        .prompt_history(&context)
        .await
        .into_iter()
        .find(
            |item| matches!(item, ResponseItem::FunctionCallOutput {call_id,..} if call_id=="read"),
        )
        .unwrap();
    let original = serde_json::to_value(original)?;
    assert!(original.to_string().contains("Complete evidence row"));

    if let LoginChange::ReturnDuringRecovery { decision } = change {
        let fit = compaction_source_fit(&session, &context).await;
        assert!(matches!(
            fit.source_fit,
            Some(compact::SourceFit::Measured { .. })
        ));
        if decision {
            session
                .checkpoint_auto_compaction(
                    &context,
                    InitialContextInjection::BeforeLastUserMessage,
                    &fit,
                    0,
                    true,
                )
                .await?;
        }
        session.flush_rollout().await;
        assert!(auth.reload());
        let (items, _, errors) =
            RolloutRecorder::load_rollout_items(&recording.as_ref().unwrap().1).await?;
        assert_eq!(errors, 0);
        let replay = session
            .reconstruct_history_from_rollout(&context, &items)
            .await;
        assert_eq!(
            replay
                .completion_review
                .as_ref()
                .and_then(|review| review.pending_compaction.as_ref())
                .map(|d| &d.fit),
            decision.then_some(&fit)
        );
        session.replace_with_reconstructed_rollout(replay).await;
        assert_eq!(compaction_source_fit(&session, &context).await, fit);
        // The saved receipt belongs to ChatGPT, but recovery now starts with API-key auth.
        // The disposable login changes back only after the notes request reaches its endpoint.
        save_chatgpt_login(home.path())?;
    }
    if upgrade {
        let manager = auth.clone();
        Mock::given(method("GET"))
            .and(path_regex(".*/responses$"))
            .respond_with(move |_: &wiremock::Request| {
                assert!(manager.reload());
                wiremock::ResponseTemplate::new(426)
            })
            .expect(1)
            .mount(&chat)
            .await;
    }
    let manager = auth.clone();
    Mock::given(method("POST"))
        .and(path_regex(".*/responses$"))
        .respond_with(move |req: &wiremock::Request| {
            let body: serde_json::Value = req.body_json().unwrap();
            assert!(
                body["input"].as_array().unwrap().last().unwrap()["content"][0]["text"]
                    .as_str()
                    .unwrap()
                    .starts_with("<AUTO_COMPACT_WORK_NOTES_REQUEST>")
            );
            assert_eq!(
                body["text"]["verbosity"],
                if notes_on_api {
                    serde_json::Value::Null
                } else {
                    json!("high")
                }
            );
            assert_eq!(body["parallel_tool_calls"], notes_on_api);
            let result = body["input"]
                .as_array()
                .unwrap()
                .iter()
                .find(|v| v["type"] == "function_call_output" && v["call_id"] == "read")
                .unwrap();
            assert_eq!(
                result.to_string().contains("Complete evidence row"),
                local_over != notes_on_api
            );
            if change == LoginChange::DuringNotes || returning {
                assert!(manager.reload());
            }
            responses::sse_response(responses::sse(vec![
                responses::ev_assistant_message("notes", NOTES),
                responses::ev_completed_with_tokens("notes", 3_000),
            ]))
        })
        .up_to_n_times(1)
        .expect(1)
        .mount(if notes_on_api { &api } else { &chat })
        .await;
    let actual = responses::mount_sse_once(if compact_on_api {&api} else {&chat}, responses::sse(vec![
        json!({"type":"response.output_item.done","item":{"type":"compaction","encrypted_content":"summary"}}),
        responses::ev_completed_with_tokens("compact",3_100),
    ])).await;
    run_pre_turn_compact(
        &session,
        &context,
        &context,
        InitialContextInjection::BeforeLastUserMessage,
        &CancellationToken::new(),
        None,
    )
    .await?;
    let compact = actual.single_request();
    let body = compact.body_json();
    assert_eq!(body["parallel_tool_calls"], compact_on_api);
    assert_eq!(
        compact.input().last(),
        Some(&json!({"type":"compaction_trigger"}))
    );
    assert_eq!(
        compact.function_call_output("read") == original,
        local_over != compact_on_api
    );
    assert_eq!(
        body["reasoning"]["effort"],
        if compact_on_api {
            serde_json::Value::Null
        } else {
            json!("low")
        }
    );
    assert_eq!(
        body["text"]["verbosity"],
        if compact_on_api {
            serde_json::Value::Null
        } else {
            json!("high")
        }
    );
    assert_eq!(
        responses::model_post_count(&api).await?,
        usize::from(compact_on_api) + usize::from(notes_on_api)
    );
    assert_eq!(
        responses::model_post_count(&chat).await?,
        3 - usize::from(compact_on_api) - usize::from(notes_on_api)
    );
    let installed = session.clone_history().await;
    assert!(installed.raw_items().iter().any(|v| matches!(v,ResponseItem::Compaction{encrypted_content} if encrypted_content == "summary")));
    assert_eq!(
        installed
            .raw_items()
            .iter()
            .filter(|item| **item == compact::preserved_work_notes_message(NOTES))
            .count(),
        1
    );
    if compact_on_api {
        let followup = responses::mount_sse_once(
            &api,
            responses::sse(vec![
                responses::ev_assistant_message("done", "Verified."),
                responses::ev_completed_with_tokens("done", 2_100),
            ]),
        )
        .await;
        sample_work(&session, &context, &mut work_client).await?;
        assert!(followup.single_request().body_json()["text"]["verbosity"].is_null());
        let next = compaction_source_fit(&session, &context).await;
        assert!(matches!(
            next.source_fit,
            Some(compact::SourceFit::Measured { .. })
        ));
        assert_eq!(
            next.fit_route,
            CheckpointRoute::new(
                &context.model_provider_id,
                context.current_endpoint().as_deref()
            )
        );
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn live_login_between_accepted_websocket_helpers_refits_the_full_send() -> anyhow::Result<()>
{
    core_test_support::skip_if_no_network_unless_localhost!(Ok(()));
    let chat =
        responses::start_websocket_server(vec![vec![vec![responses::ev_completed_with_tokens(
            "notes", 700,
        )]]])
        .await;
    let api =
        responses::start_websocket_server(vec![vec![vec![responses::ev_completed_with_tokens(
            "compact", 710,
        )]]])
        .await;
    let home = tempfile::tempdir()?;
    let auth = AuthManager::from_auth_for_testing_with_home(
        CodexAuth::create_dummy_chatgpt_auth_for_testing(),
        home.path().into(),
    );
    let (session, mut context) = make_session_and_context().await;
    context.auth_manager = Some(auth.clone());
    context.provider = crate::ModelProviderInfo::create_openai_provider();
    context.provider.base_url = None;
    context.provider.supports_websockets = true;
    context.model_info.context_window = Some(16_000);
    context.model_info.effective_context_window_percent = 100;
    let context = Arc::new(context);
    let model_client = routed_client(
        &session,
        &context,
        auth.clone(),
        chat.uri().into(),
        api.uri().into(),
    )?;
    let mut client = model_client.new_session_with_provider(context.provider.clone());
    let mut checkpoint = CompactionContext::new(&context);
    checkpoint.source_fit = compact::WorkFit {
        source_fit: Some(compact::SourceFit::Measured {
            transcript_adjustment: 24_000,
        }),
        fit_route: CheckpointRoute::new(
            &context.model_provider_id,
            context.current_endpoint().as_deref(),
        ),
    };
    let call: ResponseItem = serde_json::from_value(
        json!({"type":"function_call","name":"read_file","arguments":"{}","call_id":"tail"}),
    )?;
    let tail: ResponseItem = serde_json::from_value(
        json!({"type":"function_call_output","call_id":"tail","output":"wire evidence\n".repeat(800)}),
    )?;
    let work = vec![call, tail.clone()];
    for notes in [true, false] {
        let mut input = work.clone();
        let fit = if notes {
            input.push(work_notes_request_item(&context));
            checkpoint.notes_fit()
        } else {
            crate::auth::login_with_api_key(
                home.path(),
                "test-api-key",
                AuthCredentialsStoreMode::File,
            )?;
            assert!(auth.reload());
            checkpoint.v2_fit()
        };
        let prompt = Prompt {
            input,
            helper_fit: Some(fit),
            ..Default::default()
        };
        let mut stream = client
            .stream(
                &prompt,
                &context.model_info,
                &context.otel_manager,
                context.reasoning_effort,
                context.reasoning_summary,
                None,
                None,
            )
            .await?;
        assert_eq!(stream.endpoint_sha256, context.current_endpoint());
        if !notes {
            assert_eq!(stream.compact_input.take().unwrap().items, work);
        }
        while let Some(event) = stream.next().await {
            event?;
        }
    }
    let destination = api.single_connection();
    let source = chat.single_connection();
    assert_eq!((source.len(), destination.len()), (1, 1));
    let notes = source[0].body_json();
    let compact = destination[0].body_json();
    let result = |request: &serde_json::Value| {
        request["input"]
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["type"] == "function_call_output" && i["call_id"] == "tail")
            .unwrap()
            .clone()
    };
    assert_ne!(result(&notes), serde_json::to_value(&tail)?);
    assert_eq!(result(&compact), serde_json::to_value(tail)?);
    assert_eq!(
        compact["input"].as_array().unwrap().last(),
        Some(&json!({"type":"compaction_trigger"}))
    );
    assert!(compact.get("previous_response_id").is_none());
    chat.shutdown().await;
    api.shutdown().await;
    Ok(())
}
