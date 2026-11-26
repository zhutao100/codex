use super::live_helper_route_tests::routed_client;
use super::live_helper_route_tests::sample_work;
use super::live_helper_route_tests::save_chatgpt_login;
use super::*;
use crate::auth::AuthCredentialsStoreMode;
use crate::auth::AuthMode;
use crate::session::tests::attach_recorder;
use crate::session::tests::make_session_and_context;
use core_test_support::responses;
use pretty_assertions::assert_eq;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::ResponseTemplate;
use wiremock::matchers::method;
use wiremock::matchers::path_regex;

const INITIAL: &str = "Initial reference: check the buffered evidence.\n";
const LATEST: &str = "Distinct latest candidate.";
const ADVICE: &str = "Inspect the second handoff.";

#[derive(Clone, Copy)]
enum PreviousSample {
    Measured(i64),
    SwitchedWithoutUsage,
}

fn answer(id: &str, text: &str) -> String {
    responses::sse(vec![
        responses::ev_assistant_message(id, text),
        responses::ev_completed_with_tokens(id, 5_000),
    ])
}

async fn claim(session: &Arc<Session>, context: &TurnContext, advice: &str) -> PendingContinuation {
    let ticket = session.begin_completion_review(false).await.unwrap();
    assert!(
        session
            .prepare_completion_review_handoff(
                &ticket,
                advice.into(),
                context.model_info.slug.clone()
            )
            .await
    );
    session
        .claim_completion_review_handoff(&ticket.attempt_id, &context.sub_id, false)
        .await
        .unwrap()
}

struct ReviewFixture {
    session: Arc<Session>,
    context: Arc<TurnContext>,
    continuation: PendingContinuation,
    auth: Arc<AuthManager>,
    _home: tempfile::TempDir,
    _recording: tempfile::TempDir,
    rollout: PathBuf,
    chat: MockServer,
    api: MockServer,
    events: async_channel::Receiver<Event>,
    previous: responses::ResponsesRequest,
}

impl ReviewFixture {
    async fn new(installed: bool) -> anyhow::Result<Self> {
        Self::with_sample(installed, PreviousSample::Measured(5_000)).await
    }

    async fn with_sample(installed: bool, previous: PreviousSample) -> anyhow::Result<Self> {
        let chat = responses::start_mock_server().await;
        let api = responses::start_mock_server().await;
        let home = tempfile::tempdir()?;
        let auth = AuthManager::from_auth_for_testing_with_home(
            CodexAuth::create_dummy_chatgpt_auth_for_testing(),
            home.path().into(),
        );
        crate::auth::login_with_api_key(
            home.path(),
            "test-api-key",
            AuthCredentialsStoreMode::File,
        )?;
        let (mut session, mut context) = make_session_and_context().await;
        let (tx, events) = async_channel::unbounded();
        session.tx_event = tx;
        context.provider = crate::ModelProviderInfo::create_openai_provider();
        context.provider.base_url = None;
        context.provider.supports_websockets = false;
        context.auth_manager = Some(auth.clone());
        context.model_info.context_window = Some(128_000);
        context.model_info.auto_compact_token_limit = Some(110_000);
        context.model_info.effective_context_window_percent = 100;
        context.verbosity = SampledVerbosity::Level(codex_protocol::config_types::Verbosity::High);
        context.user_instructions =
            Some("Retain these canonical project instructions.\n".repeat(1_200));
        Arc::make_mut(&mut context.config).compaction_mode =
            crate::config::types::CompactionMode::Auto;
        session.services.auth_manager = auth.clone();
        session.services.model_client =
            routed_client(&session, &context, auth.clone(), chat.uri(), api.uri())?;
        let session = Arc::new(session);
        let mut context = Arc::new(context);
        let (recording, rollout) = attach_recorder(&session, &context).await;
        let input = vec![UserInput::Text {
            text: "Verify the evidence.".into(),
            text_elements: vec![],
        }];
        let revision = session
            .accept_completion_review_input(&input, true, None)
            .await
            .unwrap();
        session.seed_initial_context_if_needed(&context).await?;
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
        let initial = INITIAL.repeat(630);
        let first_usage = match previous {
            PreviousSample::Measured(_) => 5_000,
            PreviousSample::SwitchedWithoutUsage => 45_000,
        };
        responses::mount_sse_once(
            &chat,
            responses::sse(vec![
                responses::ev_assistant_message("first", &initial),
                responses::ev_completed_with_tokens("first", first_usage),
            ]),
        )
        .await;
        let mut client = session
            .services
            .model_client
            .new_session_with_provider(context.provider.clone());
        sample_work(&session, &context, &mut client).await?;
        session
            .capture_completed_regular_turn_for_review(&context, Some(&initial))
            .await;
        let first = claim(&session, &context, "Produce the second candidate.").await;
        let (server, completion) = match previous {
            PreviousSample::Measured(tokens) => {
                (&chat, responses::ev_completed_with_tokens("second", tokens))
            }
            PreviousSample::SwitchedWithoutUsage => {
                assert!(auth.reload());
                (
                    &api,
                    serde_json::json!({"type":"response.completed","response":{"id":"second"}}),
                )
            }
        };
        let work = responses::mount_sse_once(
            server,
            responses::sse(vec![
                responses::ev_assistant_message("second", LATEST),
                completion,
            ]),
        )
        .await;
        assert_eq!(
            continue_turn(
                session.clone(),
                context.clone(),
                first,
                CancellationToken::new()
            )
            .await
            .as_deref(),
            Some(LATEST)
        );
        session
            .capture_completed_regular_turn_for_review(&context, Some(LATEST))
            .await;
        let continuation = claim(&session, &context, ADVICE).await;
        if installed {
            let mut replacement = session.build_initial_context(&context).await;
            replacement.push(ResponseItem::Compaction {
                encrypted_content: "summary".into(),
            });
            session
                .replace_compacted_history(
                    &context,
                    replacement,
                    Some(context.to_turn_context_item()),
                    String::new(),
                )
                .await;
        }
        Arc::get_mut(&mut context)
            .unwrap()
            .provider
            .supports_websockets = true;
        let previous = work.single_request();
        Ok(Self {
            session,
            context,
            auth,
            _home: home,
            _recording: recording,
            rollout,
            chat,
            api,
            events,
            continuation,
            previous,
        })
    }

    async fn mandatory_cost(&self) -> anyhow::Result<(i64, i64)> {
        let cancel = CancellationToken::new();
        let ReviewAdvice::FirstSend(advice) = self
            .session
            .admit_completion_review_advisory(&self.context)
            .await?
        else {
            panic!("expected unsent advice")
        };
        let mut items = self.session.prompt_history(&self.context).await;
        items.push(advice.item.clone());
        let tools = compaction_tool_specs(&self.session, &self.context, &items, &cancel).await?;
        let ReviewSampling::FirstSend {
            frames,
            mandatory_alternative: Some(mandatory),
        } = self
            .session
            .review_sampling(ReviewSamplingInput {
                items: &items,
                advice: Some(&advice),
            })
            .await?
        else {
            panic!("expected a distinct optional reference")
        };
        let credit = frames.tokens() - frames.0.last().unwrap().tokens + mandatory.tokens;
        let base = self
            .session
            .get_base_instructions_for_turn(&self.context)
            .await;
        let cost = approx_token_count(&base.text) as i64
            + tools.tokens()
            + items.iter().map(estimate_item_token_count).sum::<i64>()
            + credit;
        Ok((cost, credit))
    }

    async fn switch_on_upgrade(&self) {
        let auth = self.auth.clone();
        Mock::given(method("GET"))
            .and(path_regex(".*/responses$"))
            .respond_with(move |_: &wiremock::Request| {
                assert!(auth.reload());
                ResponseTemplate::new(426)
            })
            .expect(1)
            .mount(&self.chat)
            .await;
    }

    async fn resume(&self) -> Option<String> {
        continue_turn(
            self.session.clone(),
            self.context.clone(),
            self.continuation.clone(),
            CancellationToken::new(),
        )
        .await
    }

    async fn restore(&mut self, items: &[RolloutItem]) {
        let replay = self
            .session
            .reconstruct_history_from_rollout(&self.context, items)
            .await;
        self.continuation = replay.pending_continuation.clone().unwrap();
        self.session
            .replace_with_reconstructed_rollout(replay)
            .await;
    }

    async fn post_counts(&self) -> anyhow::Result<[usize; 2]> {
        Ok([
            responses::model_post_count(&self.chat).await?,
            responses::model_post_count(&self.api).await?,
        ])
    }

    async fn resume_after_advisory_cut(
        &mut self,
        before_selection: &[RolloutItem],
        advisory: &ResponseItem,
        limit: i64,
    ) -> anyhow::Result<()> {
        let index = before_selection
            .iter()
            .position(|i| matches!(i, RolloutItem::ResponseItem(item) if item == advisory))
            .unwrap();
        let prior = before_selection[..index]
            .iter()
            .rev()
            .find_map(|i| match i {
                RolloutItem::PostTurnCompletionReviewState(state) => Some(state),
                _ => None,
            })
            .unwrap();
        assert_eq!(prior["handoff"]["advisory_recorded"], false);
        assert_ne!(prior["defer_new_prompt_frame"], true);
        assert!(!prior["pending_compaction"].is_object());
        self.restore(&before_selection[..index]).await;
        let before = self.session.get_total_token_usage().await;

        // Cut the disposable writer at the actual item, irrespective of filtered UI events.
        self.session.flush_rollout().await;
        let file = tokio::fs::read(&self.rollout).await?;
        let mut end = 0;
        for line in file.split_inclusive(|b| *b == b'\n') {
            end += line.len();
            let entry: codex_protocol::protocol::RolloutLine = serde_json::from_slice(line)?;
            if matches!(entry.item, RolloutItem::ResponseItem(item) if &item == advisory) {
                break;
            }
        }
        tokio::fs::OpenOptions::new()
            .write(true)
            .open(&self.rollout)
            .await?
            .set_len(end as u64)
            .await?;
        let (torn, _, errors) = RolloutRecorder::load_rollout_items(&self.rollout).await?;
        assert_eq!(errors, 0);
        assert_eq!(torn.len(), index + 1);
        assert!(matches!(torn.last(), Some(RolloutItem::ResponseItem(item)) if item == advisory));
        self.restore(&torn).await;
        let after = self.session.get_total_token_usage().await;
        assert!(
            before < after && limit < before,
            "before={before} after={after} target={limit}"
        );
        assert!(history_needs_continuation(
            self.session.state.lock().await.history.raw_items()
        ));
        Arc::get_mut(&mut self.context)
            .unwrap()
            .model_info
            .auto_compact_token_limit = Some(after);
        assert_eq!(self.resume().await.as_deref(), Some("Verified."));
        self.session.flush_rollout().await;
        let (recovered, _, errors) = RolloutRecorder::load_rollout_items(&self.rollout).await?;
        assert_eq!(errors, 0);
        assert_eq!(
            recovered
                .iter()
                .filter(|i| matches!(i, RolloutItem::ResponseItem(item) if item == advisory))
                .count(),
            1
        );
        Ok(())
    }
}

#[test_case::test_case(false, 5_000; "second review retains the preceding dispatched frame")]
#[test_case::test_case(false, 85_000; "measured mandatory overflow still sends ordinary on the fitting fallback")]
#[test_case::test_case(true, 5_000; "first ordinary review after installation needs no further checkpoint")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn actual_endpoint_accepts_mandatory_review_at_exact_fit(
    installed: bool,
    previous_usage: i64,
) -> anyhow::Result<()> {
    core_test_support::skip_if_no_network_unless_localhost!(Ok(()));
    let mut f =
        ReviewFixture::with_sample(installed, PreviousSample::Measured(previous_usage)).await?;
    let (cost, credit) = f.mandatory_cost().await?;
    Arc::get_mut(&mut f.context)
        .unwrap()
        .model_info
        .context_window = Some(cost + 8_192);
    if previous_usage == 85_000 {
        let previous_credit = f
            .session
            .state
            .lock()
            .await
            .history
            .request_context_tokens();
        assert!(previous_usage - previous_credit > cost);
        assert!(
            f.session.get_total_token_usage().await
                < f.context.model_info.auto_compact_token_limit().unwrap()
        );
    }
    f.switch_on_upgrade().await;
    let followup = responses::mount_sse_once(&f.api, answer("final", "Verified.")).await;
    assert_eq!(f.resume().await.as_deref(), Some("Verified."));
    assert_eq!(responses::model_post_count(&f.chat).await?, 2);
    assert_eq!(responses::model_post_count(&f.api).await?, 1);
    let sent = followup.single_request();
    let frames = sent
        .message_input_texts("developer")
        .into_iter()
        .filter(|t| t.contains("<completed_turn_review_context>"))
        .collect::<Vec<_>>();
    assert_eq!(frames.len(), if installed { 1 } else { 2 });
    if !installed {
        assert!(sent.input().starts_with(&f.previous.input()));
    }
    let latest = frames.last().unwrap();
    assert!(latest.contains(LATEST) && !latest.contains(INITIAL));
    let state = f.session.state.lock().await;
    assert!(
        state.completion_review.pending_compaction.is_none()
            && !state.completion_review.defer_new_prompt_frame
    );
    assert_eq!(state.completion_review.prompt_frames.tokens(), credit);
    assert_eq!(state.history.request_context_tokens(), credit);
    drop(state);
    f.session.flush_rollout().await;
    let (items, _, errors) = RolloutRecorder::load_rollout_items(&f.rollout).await?;
    assert_eq!(errors, 0);
    if !installed {
        assert!(!items.iter().any(|item| matches!(item, RolloutItem::PostTurnCompletionReviewState(s) if s["pending_compaction"].is_object() || s["defer_new_prompt_frame"] == serde_json::json!(true))));
    }
    let replay = f
        .session
        .reconstruct_history_from_rollout(&f.context, &items)
        .await;
    assert_eq!(replay.token_usage.unwrap().request_context_tokens, credit);
    assert_eq!(
        replay
            .completion_review
            .unwrap()
            .prompt_frames
            .0
            .last()
            .unwrap()
            .item,
        f.session
            .state
            .lock()
            .await
            .completion_review
            .prompt_frames
            .0
            .last()
            .unwrap()
            .item
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn review_loop_keeps_outgoing_controls_when_compact_returns_to_the_work_route()
-> anyhow::Result<()> {
    core_test_support::skip_if_no_network_unless_localhost!(Ok(()));
    let mut f = ReviewFixture::new(false).await?;
    let (mandatory, _) = f.mandatory_cost().await?;
    let work_controls = &f.previous.body_json()["text"]["verbosity"];
    assert_eq!(work_controls, "high");
    assert!(f.auth.reload());
    save_chatgpt_login(f._home.path())?;
    // Recover both the work receipt and its controls from the writer, not from a hand-built helper context.
    f.session.flush_rollout().await;
    let (items, _, errors) = RolloutRecorder::load_rollout_items(&f.rollout).await?;
    assert_eq!(errors, 0);
    let replay = f
        .session
        .reconstruct_history_from_rollout(&f.context, &items)
        .await;
    f.continuation = replay.pending_continuation.clone().unwrap();
    f.session.replace_with_reconstructed_rollout(replay).await;
    let settings = f
        .session
        .state
        .lock()
        .await
        .completion_review
        .last_work_settings
        .clone()
        .unwrap();
    {
        let mut state = f.session.state.lock().await;
        Arc::make_mut(&mut state.session_configuration.original_config_do_not_use)
            .model_verbosity = Some(codex_protocol::config_types::Verbosity::Low);
    }
    let outgoing = f
        .session
        .turn_context_for_task_settings(&f.context, &settings)
        .await?;
    let endpoint = outgoing.current_endpoint();
    let ctx = Arc::get_mut(&mut f.context).unwrap();
    ctx.model_info.context_window = Some(mandatory - 1 + 8_192);
    ctx.provider.supports_websockets = false;
    ctx.verbosity = outgoing.verbosity;
    assert_ne!(
        ctx.verbosity,
        SampledVerbosity::Level(codex_protocol::config_types::Verbosity::High)
    );
    assert_eq!(
        outgoing.controls_at(endpoint.as_deref()),
        ctx.controls_at(endpoint.as_deref())
    );
    assert_eq!(
        RegularTaskSettings::from_turn(&outgoing),
        RegularTaskSettings::from_turn(ctx)
    );
    assert_eq!(
        f.session.get_base_instructions_for_turn(&outgoing).await,
        f.session.get_base_instructions_for_turn(ctx).await
    );
    let auth = f.auth.clone();
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
            assert_ne!(body["text"]["verbosity"], "high");
            assert!(auth.reload());
            responses::sse_response(answer(
                "notes",
                "<AUTO_COMPACT_WORK_NOTES>\nKeep the recorded work.\n</AUTO_COMPACT_WORK_NOTES>",
            ))
        })
        .expect(1)
        .mount(&f.api)
        .await;
    for server in [&f.api, &f.chat] {
        Mock::given(method("GET"))
            .and(path_regex(".*/responses$"))
            .respond_with(ResponseTemplate::new(426))
            .mount(server)
            .await;
    }
    let on_work_route = responses::mount_sse_sequence(&f.chat, vec![
        responses::sse(vec![serde_json::json!({"type":"response.output_item.done","item":{"type":"compaction","encrypted_content":"summary"}}), responses::ev_completed_with_tokens("compact", 2000)]),
        answer("final", "Verified."),
    ]).await;
    assert_eq!(f.resume().await.as_deref(), Some("Verified."));
    let sent = on_work_route.requests();
    assert_eq!(sent.len(), 2);
    assert_eq!(
        sent[0].input().last(),
        Some(&serde_json::json!({"type":"compaction_trigger"}))
    );
    assert_eq!(sent[0].body_json()["text"]["verbosity"], *work_controls);
    assert_ne!(sent[1].body_json()["text"]["verbosity"], *work_controls);
    assert_eq!(responses::model_post_count(&f.api).await?, 1);
    assert_eq!(responses::model_post_count(&f.chat).await?, 4);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refused_impossible_install_does_not_arm_continue() -> anyhow::Result<()> {
    core_test_support::skip_if_no_network_unless_localhost!(Ok(()));
    let mut f = ReviewFixture::new(false).await?;
    let (mandatory, _) = f.mandatory_cost().await?;
    let ctx = Arc::get_mut(&mut f.context).unwrap();
    ctx.model_info.context_window = Some(mandatory - 1 + 8_192);
    ctx.model_info.auto_compact_token_limit = Some(10_000);
    f.switch_on_upgrade().await;
    for round in 0..3 {
        assert_eq!(f.resume().await, None);
        let mut errors = Vec::new();
        while let Ok(event) = f.events.try_recv() {
            if let EventMsg::Error(e) = event.msg {
                errors.push(e.message);
            }
        }
        assert!(
            errors
                .iter()
                .any(|e| e.contains("9999-token context target")),
            "{errors:?}"
        );
        assert_eq!(responses::model_post_count(&f.api).await?, 0);
        assert_eq!(responses::model_post_count(&f.chat).await?, 2);
        let (mut items, _, errors) = RolloutRecorder::load_rollout_items(&f.rollout).await?;
        assert_eq!(errors, 0);
        if round == 1 {
            // Replay the pre-fix cut: a frame was deferred, but no capture was started.
            let state = items
                .iter_mut()
                .rev()
                .find_map(|i| match i {
                    RolloutItem::PostTurnCompletionReviewState(s) => Some(s),
                    _ => None,
                })
                .unwrap();
            assert_ne!(state["defer_new_prompt_frame"], serde_json::json!(true));
            state["defer_new_prompt_frame"] = serde_json::json!(true);
        }
        let replay = f
            .session
            .reconstruct_history_from_rollout(&f.context, &items)
            .await;
        assert_eq!(
            replay
                .completion_review
                .as_ref()
                .unwrap()
                .recovery_capture_source(&f.context.model_info.slug)
                .is_some(),
            round != 0
        );
        f.continuation = replay.pending_continuation.clone().unwrap();
        f.session.replace_with_reconstructed_rollout(replay).await;
    }
    Ok(())
}

#[test_case::test_case(PreviousSample::Measured(85_000), true, false; "outgoing advice survives an item only recorder cut")]
#[test_case::test_case(PreviousSample::Measured(85_000), true, true; "one below the embedded floor publishes nothing")]
#[test_case::test_case(PreviousSample::Measured(85_000), false, false; "a different route cannot borrow the embedded floor")]
#[test_case::test_case(PreviousSample::SwitchedWithoutUsage, true, false; "an unmeasured last sampler can carry the advice")]
#[test_case::test_case(PreviousSample::SwitchedWithoutUsage, false, false; "an older measured sampler cannot carry the advice")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refused_review_prices_the_advice_that_will_survive_installation(
    previous: PreviousSample,
    same_route: bool,
    below_embedded: bool,
) -> anyhow::Result<()> {
    core_test_support::skip_if_no_network_unless_localhost!(Ok(()));
    let mut f = ReviewFixture::with_sample(false, previous).await?;
    let switched = matches!(previous, PreviousSample::SwitchedWithoutUsage);
    if switched {
        if !same_route {
            f.session.flush_rollout().await;
            let (items, _, errors) = RolloutRecorder::load_rollout_items(&f.rollout).await?;
            assert_eq!(errors, 0);
            f.restore(&items).await;
        }
        let state = f.session.state.lock().await;
        let receipt = state.history.work_receipt_source(None).unwrap();
        assert_eq!(
            receipt.endpoint,
            f.context
                .provider
                .configured_endpoint(Some(AuthMode::Chatgpt))
                .as_deref()
        );
        let sampled = state
            .completion_review
            .last_work_settings
            .as_ref()
            .unwrap()
            .sampled_controls
            .as_ref()
            .unwrap();
        assert!(sampled.owns_endpoint(f.context.current_endpoint().as_deref()));
        assert_ne!(
            receipt.endpoint,
            Some(sampled.route.endpoint_sha256.as_str())
        );
    }
    let (embedded, separate, advisory) = {
        let state = f.session.state.lock().await;
        let review = &state.completion_review;
        let handoff = review.handoff.as_ref().unwrap();
        let packet = crate::tasks::post_turn_completion_review::render_completed_turn_context(
            review.eligible_episode().unwrap(),
            false,
        );
        let frame = |prefix: &str| {
            estimate_item_token_count(&ResponseItem::from(
                codex_protocol::models::DeveloperInstructions::new(format!("{prefix}\n{packet}")),
            ))
        };
        (
            frame(&handoff.advisory),
            frame(crate::tasks::post_turn_completion_review::REPLACEMENT_ANSWER_CONTRACT)
                + estimate_item_token_count(&handoff.advisory_item()),
            handoff.advisory_item(),
        )
    };
    assert!(embedded < separate);
    let canonical = f.session.build_initial_context(&f.context).await;
    let base = f.session.get_base_instructions_for_turn(&f.context).await;
    let tools = compaction_tool_specs(
        &f.session,
        &f.context,
        &canonical,
        &CancellationToken::new(),
    )
    .await?;
    let fixed = crate::compact_remote_v2::fixed_context_tokens(&canonical, &base.text) as i64
        + tools.tokens();
    let limit = fixed + if below_embedded { embedded } else { separate } - 1;
    let ctx = Arc::get_mut(&mut f.context).unwrap();
    ctx.model_info.context_window = Some(limit + 8_192);
    ctx.provider.supports_websockets = !same_route && !switched;
    if !same_route {
        if switched {
            save_chatgpt_login(f._home.path())?;
            assert!(f.auth.reload());
        } else {
            f.switch_on_upgrade().await;
        }
    }
    let fits = same_route && !below_embedded;
    let notes = "<AUTO_COMPACT_WORK_NOTES>\nOK.\n</AUTO_COMPACT_WORK_NOTES>";
    let cycle = || {
        vec![
            answer("notes", notes),
            responses::sse(vec![
                serde_json::json!({"type":"response.output_item.done","item":{"type":"compaction","encrypted_content":"S"}}),
                responses::ev_completed_with_tokens("compact", 100),
            ]),
            answer("final", "Verified."),
        ]
    };
    let wire = if fits {
        let server = if switched { &f.api } else { &f.chat };
        Some(responses::mount_sse_sequence(server, [cycle(), cycle(), cycle()].concat()).await)
    } else {
        None
    };
    let result = f.resume().await;
    let mut errors = Vec::new();
    while let Ok(event) = f.events.try_recv() {
        if let EventMsg::Error(e) = event.msg {
            errors.push(e.message);
        }
    }
    assert_eq!(
        result.as_deref(),
        fits.then_some("Verified."),
        "{errors:?}; embedded={}, separate={}, target={limit}",
        fixed + embedded,
        fixed + separate
    );
    let posts = |new| if switched { [1, 1 + new] } else { [2 + new, 0] };
    assert_eq!(f.post_counts().await?, posts(if fits { 3 } else { 0 }));
    f.session.flush_rollout().await;
    let (items, _, parse_errors) = RolloutRecorder::load_rollout_items(&f.rollout).await?;
    assert_eq!(parse_errors, 0);
    let cut = items.iter().position(|i| matches!(i, RolloutItem::PostTurnCompletionReviewState(s) if s["defer_new_prompt_frame"] == true || s["pending_compaction"].is_object()));
    let Some(wire) = wire else {
        assert!(
            errors.iter().any(|e| e.contains("even without history")),
            "{errors:?}"
        );
        assert_eq!(cut, None);
        assert!(
            !items
                .iter()
                .any(|i| matches!(i, RolloutItem::ResponseItem(item) if item == &advisory))
        );
        return Ok(());
    };
    let cut = cut.unwrap();
    f.restore(&items[..=cut]).await;
    assert_eq!(f.resume().await.as_deref(), Some("Verified."));
    f.resume_after_advisory_cut(&items[..cut], &advisory, limit)
        .await?;
    let sent = wire.requests();
    assert_eq!(sent.len(), 9);
    assert_eq!(f.post_counts().await?, posts(sent.len()));
    for cycle in sent.chunks_exact(3) {
        let [notes, compact, ordinary] = cycle else {
            unreachable!()
        };
        let work = notes.input();
        let source = compact.input();
        assert_eq!(work[..work.len() - 1], source[..source.len() - 1]);
        assert!(work.starts_with(&f.previous.input()));
        let frames = notes
            .message_input_texts("developer")
            .into_iter()
            .filter(|s| s.contains("<completed_turn_review_context>"))
            .collect::<Vec<_>>();
        assert_eq!(frames.len(), 2);
        assert!(frames[1].contains(LATEST));
        assert_eq!(
            source.last(),
            Some(&serde_json::json!({"type":"compaction_trigger"}))
        );
        assert_eq!(
            notes
                .message_input_texts("developer")
                .iter()
                .filter(|s| s.contains(ADVICE))
                .count(),
            1
        );
        let carried = ordinary
            .message_input_texts("developer")
            .into_iter()
            .filter(|s| s.contains(ADVICE))
            .collect::<Vec<_>>();
        assert_eq!(carried.len(), 1);
        assert!(
            carried[0].contains("<completed_turn_review_context>") && carried[0].contains(LATEST)
        );
    }
    Ok(())
}

#[test_case::test_case(false; "superseded first review drains the accepted steer")]
#[test_case::test_case(true; "installed checkpoint samples owed work before the steer")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn steer_during_upgrade_finishes_without_manual_continuation(
    installed: bool,
) -> anyhow::Result<()> {
    core_test_support::skip_if_no_network_unless_localhost!(Ok(()));
    let f = ReviewFixture::new(installed).await?;
    let entered = Arc::new(tokio::sync::Notify::new());
    let signal = entered.clone();
    let (release, wait) = std::sync::mpsc::channel();
    let wait = std::sync::Mutex::new(wait);
    let auth = f.auth.clone();
    Mock::given(method("GET"))
        .and(path_regex(".*/responses$"))
        .respond_with(move |_: &wiremock::Request| {
            signal.notify_one();
            wait.lock()
                .unwrap()
                .recv_timeout(std::time::Duration::from_secs(5))
                .unwrap();
            assert!(auth.reload());
            ResponseTemplate::new(426)
        })
        .expect(1)
        .mount(&f.chat)
        .await;
    let mut replies = Vec::new();
    if installed {
        replies.push(answer("owed", "Finished prior work."));
    }
    replies.push(answer("amended", "Finished amended work."));
    let wire = responses::mount_sse_sequence(&f.api, replies).await;
    f.session
        .spawn_task(
            f.context.clone(),
            vec![],
            crate::tasks::ContinueTask::new(f.continuation.clone()),
        )
        .await;
    tokio::time::timeout(std::time::Duration::from_secs(5), entered.notified()).await?;
    let amendment = "Include the new measurements.";
    f.session
        .steer_input_for_turn(
            vec![UserInput::Text {
                text: amendment.into(),
                text_elements: vec![],
            }],
            Some(&f.context.sub_id),
            None,
        )
        .await
        .unwrap();
    release.send(())?;
    tokio::time::timeout(std::time::Duration::from_secs(8), async {
        loop {
            match f.events.recv().await.unwrap().msg {
                EventMsg::TurnComplete(done) => {
                    assert_eq!(
                        done.last_agent_message.as_deref(),
                        Some("Finished amended work.")
                    );
                    break;
                }
                EventMsg::Error(e) => panic!("{}", e.message),
                _ => {}
            }
        }
    })
    .await?;
    assert_eq!(
        responses::model_post_count(&f.api).await?,
        1 + usize::from(installed)
    );
    let sent = wire.requests();
    for (i, request) in sent.iter().enumerate() {
        assert!(
            !request
                .message_input_texts("developer")
                .iter()
                .any(|s| s.contains(ADVICE))
        );
        assert_eq!(
            request
                .message_input_texts("user")
                .iter()
                .filter(|s| *s == amendment)
                .count(),
            usize::from(i + 1 == sent.len())
        );
    }
    Ok(())
}
