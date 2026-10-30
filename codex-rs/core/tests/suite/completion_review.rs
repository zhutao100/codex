use super::*;
use anyhow::Context;
use codex_core::features::Feature;
use codex_protocol::config_types::CollaborationMode;
use codex_protocol::config_types::ModeKind;
use codex_protocol::config_types::Settings;
use codex_protocol::protocol::CompactedItem;
use codex_protocol::request_user_input::RequestUserInputAnswer;
use codex_protocol::request_user_input::RequestUserInputResponse;
use core_test_support::responses::ResponsesRequest;
use core_test_support::responses::ev_function_call;
use pretty_assertions::assert_eq;

fn answer(text: &str) -> String {
    sse(vec![
        ev_response_created(&Uuid::new_v4().to_string()),
        ev_assistant_message("answer", text),
        ev_completed(&Uuid::new_v4().to_string()),
    ])
}

fn verdict(evaluation: &str, followup: bool) -> String {
    answer(
        &serde_json::json!({
            "evaluation": evaluation,
            "fix_actions_advised": followup,
        })
        .to_string(),
    )
}

fn configure_recovery_models(config: &mut Config) {
    config.model = Some("completion-recovery-small".into());
    config.review_model = Some("completion-recovery-large".into());
    config.features.enable(Feature::RemoteCompaction);
    config.features.enable(Feature::RemoteCompactionV2);
    config.model_overlay = Some(ModelOverlay {
        models: [
            ("completion-recovery-small", 24_000),
            ("completion-recovery-large", 128_000),
        ]
        .into_iter()
        .map(|(model, window)| ModelOverlayEntry {
            slug: model.into(),
            model_provider: None,
            patch: ModelInfoPatch {
                base_instructions: Some("Complete the requested work.".into()),
                context_window: Some(Some(window)),
                auto_compact_token_limit: Some(Some(200_000)),
                ..Default::default()
            },
            final_instruction_override: None,
        })
        .collect(),
        ..Default::default()
    });
}

fn compacted_answer() -> String {
    sse(vec![
        serde_json::json!({
            "type": "response.output_item.done",
            "item": {"type": "compaction", "encrypted_content": "compacted review follow-up"},
        }),
        ev_completed("compact"),
    ])
}

fn context_override(cwd: Option<PathBuf>, model: Option<&str>) -> Op {
    Op::OverrideTurnContext {
        cwd,
        approval_policy: None,
        sandbox_policy: None,
        windows_sandbox_level: None,
        model: model.map(str::to_owned),
        effort: None,
        summary: None,
        collaboration_mode: None,
        personality: None,
        service_tier: None,
    }
}

fn completion_state(path: &std::path::Path) -> anyhow::Result<serde_json::Value> {
    read_rollout_lines(path)
        .into_iter()
        .rev()
        .find_map(|line| match line.item {
            RolloutItem::PostTurnCompletionReviewState(state) => Some(state),
            _ => None,
        })
        .context("expected completion-review state")
}

#[expect(clippy::expect_used)]
fn packet(request: &ResponsesRequest) -> String {
    request
        .input()
        .into_iter()
        .find(|item| {
            item["role"] == "user"
                && item["content"][0]["text"].as_str().is_some_and(|text| {
                    text.contains("<completed_turn_review_context version=\"2\">")
                })
        })
        .and_then(|item| item["content"][0]["text"].as_str().map(str::to_string))
        .expect("review packet")
}

#[expect(clippy::unwrap_used)]
async fn submit_request(codex: &CodexThread, text: &str) {
    codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: text.into(),
                text_elements: vec![],
            }],
            final_output_json_schema: None,
        })
        .await
        .unwrap();
}

#[expect(clippy::unwrap_used)]
async fn append_crash_fixture(path: &std::path::Path, items: Vec<RolloutItem>) {
    let mut file = tokio::fs::OpenOptions::new()
        .append(true)
        .open(path)
        .await
        .unwrap();
    for item in items {
        let line = RolloutLine {
            timestamp: "2026-01-01T00:00:00.000Z".into(),
            item,
        };
        file.write_all(format!("{}\n", serde_json::to_string(&line).unwrap()).as_bytes())
            .await
            .unwrap();
    }
    file.sync_all().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn same_request_three_reviews_keep_initial_and_exact_latest_without_prior_verdicts() {
    skip_if_no_network_unless_localhost!();
    let server = MockServer::start().await;
    let requests = mount_sse_sequence(
        &server,
        vec![
            answer("A0: initial comprehensive X and Y"),
            verdict("R1: correct X", true),
            answer("A1: current corrected X and original Y"),
            verdict("R2: correct Y", true),
            answer("A2: current corrected X and corrected Y; verification complete"),
            verdict("R3: no actionable follow-up advised", false),
        ],
    )
    .await;
    let home = Arc::new(TempDir::new().unwrap());
    let codex = new_conversation_for_server(&server, home.clone(), |config| {
        config
            .features
            .enable(Feature::AutoPostTurnCompletionReview);
        config.post_turn_completion_review_prompt = Some("custom reviewer prompt".into());
    })
    .await;
    submit_request(&codex, "U1: implement X and explain Y").await;
    wait_for_event(&codex, |event| matches!(event, EventMsg::ExitedReviewMode(exit)
        if exit.post_turn_completion_review_output.as_ref().is_some_and(|output| !output.fix_actions_advised))).await;
    let terminal = wait_for_event(&codex, |event| matches!(event, EventMsg::TurnComplete(_))).await;
    assert!(
        matches!(terminal, EventMsg::TurnComplete(event) if event.last_agent_message.is_none())
    );
    let requests = requests.requests();
    assert_eq!(requests.len(), 6);
    let first = packet(&requests[1]);
    assert_eq!(
        first.matches("A0: initial comprehensive X and Y").count(),
        1
    );
    for (index, current, absent) in [
        (3, "A1: current", "A2: current"),
        (5, "A2: current", "A1: current"),
    ] {
        let context = packet(&requests[index]);
        assert!(context.contains("U1: implement X and explain Y"));
        assert!(context.contains("A0: initial comprehensive X and Y"));
        assert!(context.contains(current));
        assert!(!context.contains(absent));
        assert!(!context.contains("R1:"));
        assert!(!context.contains("R2:"));
        assert!(context.contains("historical_unverified"));
        let instructions = requests[index].body_json()["instructions"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(instructions.contains("custom reviewer prompt"));
        assert!(instructions.contains("cannot compensate for omissions"));
    }
    for index in [2, 4] {
        let input = serde_json::to_string(&requests[index].input()).unwrap();
        assert!(input.contains("self-contained final answer"));
        assert!(input.contains("when you reject every finding or make no file changes"));
        assert!(input.contains("U1: implement X and explain Y"));
    }
    let rollout = read_rollout_lines(&codex.rollout_path().unwrap());
    let state = rollout
        .iter()
        .rev()
        .find_map(|line| match &line.item {
            RolloutItem::PostTurnCompletionReviewState(state) => Some(state),
            _ => None,
        })
        .unwrap();
    assert_eq!(
        state["episode"]["initial_answer_reference"],
        "A0: initial comprehensive X and Y"
    );
    assert_eq!(
        state["episode"]["final_agent_message"],
        "A2: current corrected X and corrected Y; verification complete"
    );
    assert_eq!(state["episode"]["automatic_reviews_started"], 3);
    assert!(state["attempt"].is_null());
    assert!(
        !rollout
            .iter()
            .any(|line| matches!(line.item, RolloutItem::EventMsg(EventMsg::TurnComplete(_))))
    );
    server.verify().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn automatic_cap_keeps_unresolved_findings_and_manual_handoff_does_not_reset_it() {
    skip_if_no_network_unless_localhost!();
    let server = MockServer::start().await;
    let requests = mount_sse_sequence(
        &server,
        vec![
            answer("initial answer"),
            verdict("first correction", true),
            answer("replacement one"),
            verdict("second correction", true),
            answer("replacement two"),
            verdict("unresolved third correction", true),
            verdict("manual follow-up requested", true),
            answer("manual replacement"),
        ],
    )
    .await;
    let home = Arc::new(TempDir::new().unwrap());
    let codex = new_conversation_for_server(&server, home.clone(), |config| {
        config
            .features
            .enable(Feature::AutoPostTurnCompletionReview);
    })
    .await;
    submit_request(&codex, "finish the complete request").await;
    wait_for_event(&codex, |event| {
        matches!(event, EventMsg::BackgroundEvent(event)
        if event.message.contains("stopped with unresolved advice"))
    })
    .await;
    wait_for_event(&codex, |event| matches!(event, EventMsg::TurnComplete(_))).await;
    assert_eq!(requests.requests().len(), 6);
    codex.submit(Op::ReviewCompletedTurn).await.unwrap();
    wait_for_event(&codex, |event| {
        matches!(event, EventMsg::TurnComplete(event)
        if event.last_agent_message.as_deref() == Some("manual replacement"))
    })
    .await;
    assert_eq!(requests.requests().len(), 8);
    let lines = read_rollout_lines(&codex.rollout_path().unwrap());
    let state = lines
        .iter()
        .rev()
        .find_map(|line| match &line.item {
            RolloutItem::PostTurnCompletionReviewState(state) => Some(state),
            _ => None,
        })
        .unwrap();
    assert_eq!(state["episode"]["automatic_reviews_started"], 3);
    assert_eq!(
        state["episode"]["final_agent_message"],
        "manual replacement"
    );
    server.verify().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invalid_reviewer_output_exits_without_a_clean_verdict_or_continuation() {
    skip_if_no_network_unless_localhost!();
    let server = MockServer::start().await;
    let requests = mount_sse_sequence(
        &server,
        vec![answer("user-facing result"), answer("not valid JSON")],
    )
    .await;
    let home = Arc::new(TempDir::new().unwrap());
    let codex = new_conversation_for_server(&server, home.clone(), |_| {}).await;
    submit_request(&codex, "request").await;
    wait_for_event(&codex, |event| matches!(event, EventMsg::TurnComplete(_))).await;
    codex.submit(Op::ReviewCompletedTurn).await.unwrap();
    wait_for_event(&codex, |event| {
        matches!(event, EventMsg::BackgroundEvent(event)
        if event.message.contains("No review verdict was established"))
    })
    .await;
    let exit = wait_for_event(&codex, |event| {
        matches!(event, EventMsg::ExitedReviewMode(_))
    })
    .await;
    assert!(
        matches!(exit, EventMsg::ExitedReviewMode(event) if event.post_turn_completion_review_output.is_none())
    );
    wait_for_event(&codex, |event| matches!(event, EventMsg::TurnComplete(_))).await;
    assert_eq!(requests.requests().len(), 2);
    server.verify().await;
}

#[test_case::test_case(false; "initial task")]
#[test_case::test_case(true; "review followup")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn human_clarification_prevents_an_incomplete_request_from_being_declared_reviewed(
    review_followup: bool,
) -> anyhow::Result<()> {
    skip_if_no_network_unless_localhost!(Ok(()));
    let server = MockServer::start().await;
    let call_id = "scope-clarification";
    let questions = serde_json::json!({
        "questions": [{
            "id": "target",
            "header": "Target",
            "question": "Which interface should be changed?",
            "options": [
                {"label": "CLI only", "description": "Leave the API unchanged."},
                {"label": "API only", "description": "Leave the CLI unchanged."}
            ]
        }]
    });
    let mut responses = Vec::new();
    if review_followup {
        responses.extend([
            answer("Initial answer before clarification."),
            verdict("Clarify the interface before completing the change.", true),
        ]);
    }
    responses.extend([
        sse(vec![
            ev_response_created("clarification"),
            ev_function_call(call_id, "request_user_input", &questions.to_string()),
            ev_completed("clarification"),
        ]),
        answer("Complete result for the selected CLI-only scope."),
    ]);
    let requests = mount_sse_sequence(&server, responses).await;
    let home = Arc::new(TempDir::new()?);
    let mut builder = test_codex().with_home(home).with_config(|config| {
        config.features.enable(Feature::CollaborationModes);
    });
    let test = builder.build(&server).await?;
    let codex = Arc::clone(&test.codex);
    codex
        .submit(Op::OverrideTurnContext {
            cwd: None,
            approval_policy: None,
            sandbox_policy: None,
            windows_sandbox_level: None,
            model: None,
            effort: None,
            summary: None,
            collaboration_mode: Some(CollaborationMode {
                mode: ModeKind::Plan,
                settings: Settings {
                    model: "gpt-5.4".into(),
                    reasoning_effort: None,
                    developer_instructions: None,
                },
            }),
            personality: None,
            service_tier: None,
        })
        .await?;
    submit_request(&codex, "Plan the interface change; ask me which interface.").await;
    if review_followup {
        wait_for_event(&codex, |event| matches!(event, EventMsg::TurnComplete(_))).await;
        codex.submit(Op::ReviewCompletedTurn).await?;
    }
    let event = wait_for_event(&codex, |event| {
        matches!(event, EventMsg::RequestUserInput(_))
    })
    .await;
    let EventMsg::RequestUserInput(question) = event else {
        panic!("expected clarification");
    };
    codex
        .submit(Op::UserInputAnswer {
            id: question.turn_id,
            response: RequestUserInputResponse {
                answers: std::collections::HashMap::from([(
                    "target".into(),
                    RequestUserInputAnswer {
                        answers: vec!["CLI only".into()],
                    },
                )]),
            },
        })
        .await?;
    wait_for_event(&codex, |event| {
        matches!(event, EventMsg::Warning(event)
        if event.message.contains("human clarification answers"))
    })
    .await;
    wait_for_event(&codex, |event| matches!(event, EventMsg::TurnComplete(_))).await;
    codex.submit(Op::ReviewCompletedTurn).await?;
    wait_for_event(&codex, |event| {
        matches!(event, EventMsg::Error(event)
        if event.message.contains("exact consumed-request provenance"))
    })
    .await;
    let captured = requests.requests();
    assert_eq!(captured.len(), if review_followup { 4 } else { 2 });
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(
            &captured[captured.len() - 1]
                .function_call_output_content_and_success(call_id)
                .context("expected clarification output")?
                .0
                .context("expected clarification output text")?
        )?,
        serde_json::json!({"answers": {"target": {"answers": ["CLI only"]}}})
    );
    let rollout_path = codex.rollout_path().context("expected parent rollout")?;
    let state = completion_state(&rollout_path)?;
    assert_eq!(state["request_frame"]["exact"], false);
    assert!(state["attempt"].is_null());
    let fork = test
        .thread_manager
        .fork_thread(usize::MAX, test.config.clone(), rollout_path)
        .await?
        .thread;
    let fork_state = completion_state(&fork.rollout_path().context("expected fork rollout")?)?;
    assert_eq!(fork_state["request_frame"]["exact"], false);
    assert!(fork_state["attempt"].is_null());
    fork.submit(Op::ReviewCompletedTurn).await?;
    wait_for_event(&fork, |event| {
        matches!(event, EventMsg::Error(event)
        if event.message.contains("exact consumed-request provenance"))
    })
    .await;
    assert_eq!(requests.requests().len(), captured.len());
    server.verify().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn completed_snapshot_resumes_without_persisted_turn_complete_events() {
    skip_if_no_network_unless_localhost!();
    let server = MockServer::start().await;
    let requests = mount_sse_sequence(
        &server,
        vec![
            answer(" exact durable answer\n"),
            verdict("checked after restart", false),
        ],
    )
    .await;
    let home = Arc::new(TempDir::new().unwrap());
    let codex = new_conversation_for_server(&server, home.clone(), |_| {}).await;
    submit_request(&codex, "durable request").await;
    wait_for_event(&codex, |event| matches!(event, EventMsg::TurnComplete(_))).await;
    let path = codex.rollout_path().unwrap();
    codex.submit(Op::Shutdown).await.unwrap();
    wait_for_event(&codex, |event| matches!(event, EventMsg::ShutdownComplete)).await;
    let resumed = resume_conversation_for_server(&server, home.clone(), path, |_| {}).await;
    resumed.submit(Op::ReviewCompletedTurn).await.unwrap();
    wait_for_event(&resumed, |event| {
        matches!(event, EventMsg::ExitedReviewMode(_))
    })
    .await;
    wait_for_event(&resumed, |event| matches!(event, EventMsg::TurnComplete(_))).await;
    let requests = requests.requests();
    assert_eq!(requests.len(), 2);
    let context = packet(&requests[1]);
    assert!(context.contains("durable request"));
    assert!(context.contains("<candidate_final_answer><![CDATA[ exact durable answer\n]]>"));
    server.verify().await;
}

#[derive(Clone, Copy, Debug)]
enum NonModelRecoveryOverride {
    Cwd,
    Approval,
    Effort,
}

#[test_case::test_case(false, false, None; "compact")]
#[test_case::test_case(true, false, None; "larger model")]
#[test_case::test_case(true, true, None; "larger model after restart")]
#[test_case::test_case(true, true, Some(NonModelRecoveryOverride::Cwd); "cwd override after restart")]
#[test_case::test_case(true, true, Some(NonModelRecoveryOverride::Approval); "approval override after restart")]
#[test_case::test_case(true, true, Some(NonModelRecoveryOverride::Effort); "effort override after restart")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn review_followup_budget_recovery_preserves_scope(
    select_larger_model: bool,
    restart: bool,
    non_model_override: Option<NonModelRecoveryOverride>,
) -> anyhow::Result<()> {
    skip_if_no_network_unless_localhost!(Ok(()));
    let server = MockServer::start().await;
    let initial = "Exact candidate details.\n".repeat(1_500);
    let mut replies = vec![
        answer(&initial),
        verdict("Verify the work and replace the complete answer.", true),
    ];
    if !select_larger_model {
        replies.push(compacted_answer());
    }
    replies.push(answer("Recovered complete replacement."));
    let requests = mount_sse_sequence(&server, replies).await;
    let home = Arc::new(TempDir::new()?);
    let codex = new_conversation_for_server(&server, home.clone(), configure_recovery_models).await;
    submit_request(&codex, "Preserve every result and report verification.").await;
    wait_for_event(&codex, |event| matches!(event, EventMsg::TurnComplete(_))).await;
    codex.submit(Op::ReviewCompletedTurn).await?;
    wait_for_event(&codex, |event| match event {
        EventMsg::Error(event) => {
            assert!(event.message.contains("Review follow-up context exceeds"));
            true
        }
        EventMsg::TurnComplete(event) if event.last_agent_message.is_some() => {
            panic!("follow-up must stop before sending an over-budget request")
        }
        _ => false,
    })
    .await;
    wait_for_event(&codex, |event| matches!(event, EventMsg::TurnComplete(_))).await;
    assert_eq!(requests.requests().len(), 2);
    if select_larger_model {
        codex
            .submit(context_override(None, Some("completion-recovery-large")))
            .await?;
    } else {
        codex.submit(Op::Compact).await?;
        wait_for_event(&codex, |event| match event {
            EventMsg::Error(event) => panic!("compaction failed: {}", event.message),
            EventMsg::TurnComplete(_) => true,
            _ => false,
        })
        .await;
    }
    let active = if restart {
        let path = codex.rollout_path().context("expected rollout path")?;
        codex.submit(Op::Shutdown).await?;
        wait_for_event(&codex, |event| matches!(event, EventMsg::ShutdownComplete)).await;
        resume_conversation_for_server(&server, home.clone(), path, configure_recovery_models).await
    } else {
        codex
    };
    if let Some(non_model_override) = non_model_override {
        let (cwd, approval_policy, effort) = match non_model_override {
            NonModelRecoveryOverride::Cwd => (Some(home.path().join("workspace")), None, None),
            NonModelRecoveryOverride::Approval => (
                None,
                Some(codex_protocol::protocol::AskForApproval::Never),
                None,
            ),
            NonModelRecoveryOverride::Effort => (
                None,
                None,
                Some(Some(codex_protocol::openai_models::ReasoningEffort::High)),
            ),
        };
        active
            .submit(Op::OverrideTurnContext {
                cwd,
                approval_policy,
                sandbox_policy: None,
                windows_sandbox_level: None,
                model: None,
                effort,
                summary: None,
                collaboration_mode: None,
                personality: None,
                service_tier: None,
            })
            .await?;
    }
    active.submit(Op::Continue).await?;
    wait_for_event(&active, |event| match event {
        EventMsg::Error(event) => panic!("recovery failed: {}", event.message),
        EventMsg::TurnComplete(event) => {
            assert_eq!(
                event.last_agent_message.as_deref(),
                Some("Recovered complete replacement.")
            );
            true
        }
        _ => false,
    })
    .await;
    let captured = requests.requests();
    assert_eq!(captured.len(), if select_larger_model { 3 } else { 4 });
    let request = captured.last().context("expected recovery request")?;
    assert_eq!(
        request.body_json()["model"],
        if select_larger_model {
            "completion-recovery-large"
        } else {
            "completion-recovery-small"
        }
    );
    let input = request.input();
    assert!(input.iter().any(|item| {
        item["role"] == "developer"
            && item["content"][0]["text"].as_str().is_some_and(|text| {
                text.contains(&initial)
                    && text.contains("Preserve every result and report verification.")
                    && text.contains("self-contained final answer")
            })
    }));
    if !select_larger_model {
        let compact_input = captured[2].input();
        assert_eq!(
            compact_input.last().context("expected compaction input")?["type"],
            "compaction_trigger"
        );
        assert!(!serde_json::to_string(&input)?.contains("compaction_trigger"));
    }
    server.verify().await;
    Ok(())
}

#[test_case::test_case(false; "pause")]
#[test_case::test_case(true; "interrupt")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stopped_review_followup_survives_standalone_compaction_and_restart(
    interrupt: bool,
) -> anyhow::Result<()> {
    skip_if_no_network_unless_localhost!(Ok(()));
    for restart in [false, true] {
        let server = MockServer::start().await;
        let requests = mount_response_sequence(
            &server,
            vec![
                sse_response(answer("Original complete answer.")),
                sse_response(verdict("Verify and replace the complete answer.", true)),
                sse_response(answer("Cancelled output must not be installed."))
                    .set_delay(std::time::Duration::from_secs(5)),
                sse_response(compacted_answer()),
                sse_response(answer("Complete replacement after compaction.")),
            ],
        )
        .await;
        let home = Arc::new(TempDir::new()?);
        let codex =
            new_conversation_for_server(&server, home.clone(), configure_recovery_models).await;
        submit_request(&codex, "Complete the original user scope.").await;
        wait_for_event(&codex, |event| matches!(event, EventMsg::TurnComplete(_))).await;
        codex.submit(Op::ReviewCompletedTurn).await?;
        wait_for_captured_requests(&requests, 3).await;
        codex
            .submit(if interrupt { Op::Interrupt } else { Op::Pause })
            .await?;
        let stopped = wait_for_event(&codex, |event| {
            matches!(event, EventMsg::TurnPaused(_) | EventMsg::TurnAborted(_))
        })
        .await;
        assert!(if interrupt {
            matches!(stopped, EventMsg::TurnAborted(_))
        } else {
            matches!(stopped, EventMsg::TurnPaused(_))
        });
        codex.submit(Op::Compact).await?;
        wait_for_event(&codex, |event| match event {
            EventMsg::Error(event) => panic!("compaction failed: {}", event.message),
            EventMsg::TurnComplete(_) => true,
            _ => false,
        })
        .await;
        let path = codex.rollout_path().context("expected rollout path")?;
        let active = if restart {
            codex.submit(Op::Shutdown).await?;
            wait_for_event(&codex, |event| matches!(event, EventMsg::ShutdownComplete)).await;
            resume_conversation_for_server(
                &server,
                home.clone(),
                path.clone(),
                configure_recovery_models,
            )
            .await
        } else {
            codex
        };
        active.submit(Op::Continue).await?;
        let continued = wait_for_event(&active, |event| match event {
            EventMsg::Error(event) => panic!("continuation failed: {}", event.message),
            EventMsg::TurnContinued(_) => true,
            _ => false,
        })
        .await;
        assert!(
            matches!(continued, EventMsg::TurnContinued(event) if event.source == if restart || interrupt {
                TurnContinuationSource::Interrupted
            } else {
                TurnContinuationSource::Paused
            })
        );
        wait_for_event(&active, |event| match event {
            EventMsg::Error(event) => panic!("continued work failed: {}", event.message),
            EventMsg::TurnComplete(event) => {
                assert_eq!(
                    event.last_agent_message.as_deref(),
                    Some("Complete replacement after compaction.")
                );
                true
            }
            _ => false,
        })
        .await;
        let captured = requests.requests();
        assert_eq!(captured.len(), 5);
        let input = serde_json::to_string(&captured[4].input())?;
        for text in [
            "Complete the original user scope.",
            "Original complete answer.",
            "Verify and replace the complete answer.",
            "self-contained final answer",
        ] {
            assert!(input.contains(text), "missing {text:?}; restart={restart}");
        }
        assert!(!input.contains("Cancelled output"));
        server.verify().await;
    }
    Ok(())
}

#[test_case::test_case(false; "paused")]
#[test_case::test_case(true; "budget stopped")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn review_followup_directory_ownership_survives_pause_and_restart(
    budget_stopped: bool,
) -> anyhow::Result<()> {
    skip_if_no_network_unless_localhost!(Ok(()));
    for restart in [false, true] {
        let server = MockServer::start().await;
        let initial = if budget_stopped {
            "Exact candidate details.\n".repeat(1_500)
        } else {
            "Original complete answer.".into()
        };
        let mut replies = vec![
            sse_response(answer(&initial)),
            sse_response(verdict("Verify and replace the complete answer.", true)),
        ];
        if !budget_stopped {
            replies.push(
                sse_response(answer("Paused output must not be installed."))
                    .set_delay(std::time::Duration::from_secs(5)),
            );
        }
        replies.push(sse_response(answer(
            "Complete replacement in the original project.",
        )));
        replies.push(sse_response(verdict(
            "Checked the original project.",
            false,
        )));
        let requests = mount_response_sequence(&server, replies).await;
        let home = Arc::new(TempDir::new()?);
        let original_cwd = home.path().join("workspace");
        let other_cwd = home.path().join("other-project");
        for (cwd, instructions) in [
            (&original_cwd, "ORIGINAL_PROJECT_OWNERSHIP"),
            (&other_cwd, "OTHER_PROJECT_OWNERSHIP"),
        ] {
            tokio::fs::create_dir_all(cwd).await?;
            tokio::fs::write(cwd.join("AGENTS.md"), instructions).await?;
        }
        let codex =
            new_conversation_for_server(&server, home.clone(), configure_recovery_models).await;
        submit_request(&codex, "Finish the original project request.").await;
        wait_for_event(&codex, |event| matches!(event, EventMsg::TurnComplete(_))).await;
        codex.submit(Op::ReviewCompletedTurn).await?;
        let request_count = if budget_stopped {
            wait_for_event(&codex, |event| {
                matches!(event, EventMsg::Error(error)
                    if error.message.contains("Review follow-up context exceeds"))
            })
            .await;
            wait_for_event(&codex, |event| matches!(event, EventMsg::TurnComplete(_))).await;
            2
        } else {
            wait_for_captured_requests(&requests, 3).await;
            codex.submit(Op::Pause).await?;
            wait_for_event(&codex, |event| matches!(event, EventMsg::TurnPaused(_))).await;
            3
        };
        let path = codex.rollout_path().context("expected rollout path")?;
        let before = completion_state(&path)?;
        codex
            .submit(context_override(Some(other_cwd.clone()), None))
            .await?;
        let active = if restart {
            codex.submit(Op::Shutdown).await?;
            wait_for_event(&codex, |event| matches!(event, EventMsg::ShutdownComplete)).await;
            resume_conversation_for_server(&server, home.clone(), path.clone(), move |config| {
                configure_recovery_models(config);
                config.cwd = other_cwd;
            })
            .await
        } else {
            codex
        };
        active.submit(Op::Continue).await?;
        let event = wait_for_event(&active, |event| {
            matches!(
                event,
                EventMsg::Error(_) | EventMsg::TurnStarted(_) | EventMsg::TurnContinued(_)
            )
        })
        .await;
        assert!(
            matches!(event, EventMsg::Error(error)
                if error.message.contains("working directory")
                    && error.codex_error_info == Some(codex_protocol::protocol::CodexErrorInfo::BadRequest)),
            "cross-directory work must be rejected before task startup; restart={restart}"
        );
        assert_eq!(requests.requests().len(), request_count);
        assert_eq!(completion_state(&path)?, before);
        active
            .submit(context_override(
                Some(original_cwd.clone()),
                budget_stopped.then_some("completion-recovery-large"),
            ))
            .await?;
        active.submit(Op::Continue).await?;
        wait_for_event(&active, |event| match event {
            EventMsg::Error(error) => {
                panic!("original-directory recovery failed: {}", error.message)
            }
            EventMsg::TurnComplete(event) => {
                assert_eq!(
                    event.last_agent_message.as_deref(),
                    Some("Complete replacement in the original project.")
                );
                true
            }
            _ => false,
        })
        .await;
        assert_eq!(
            completion_state(&path)?["episode"]["cwd"],
            serde_json::to_value(&original_cwd)?
        );
        active.submit(Op::ReviewCompletedTurn).await?;
        wait_for_event(&active, |event| {
            matches!(event, EventMsg::ExitedReviewMode(_))
        })
        .await;
        wait_for_event(&active, |event| matches!(event, EventMsg::TurnComplete(_))).await;
        let captured = requests.requests();
        assert_eq!(captured.len(), request_count + 2);
        let review = captured.last().context("expected final review")?;
        let input = serde_json::to_string(&review.input())?;
        assert!(input.contains("ORIGINAL_PROJECT_OWNERSHIP"));
        assert!(!input.contains("OTHER_PROJECT_OWNERSHIP"));
        assert!(packet(review).contains("Complete replacement in the original project."));
        server.verify().await;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn compacted_legacy_resume_cannot_review_a_referential_request_as_complete_scope() {
    skip_if_no_network_unless_localhost!();
    for version in [None, Some(1)] {
        let server = MockServer::start().await;
        let requests = mount_sse_sequence(
            &server,
            vec![
                answer("option one: scan; option two: index"),
                answer("used option two"),
            ],
        )
        .await;
        let home = Arc::new(TempDir::new().unwrap());
        let codex = new_conversation_for_server(&server, home.clone(), |_| {}).await;
        submit_request(
            &codex,
            "choose a method that preserves the original requirements",
        )
        .await;
        wait_for_event(&codex, |event| matches!(event, EventMsg::TurnComplete(_))).await;
        let path = codex.rollout_path().unwrap();
        codex.submit(Op::Shutdown).await.unwrap();
        wait_for_event(&codex, |event| matches!(event, EventMsg::ShutdownComplete)).await;
        let lines = read_rollout_lines(&path);
        let session_meta = lines
            .iter()
            .find(|line| matches!(line.item, RolloutItem::SessionMeta(_)))
            .unwrap()
            .item
            .clone();
        let mut legacy = vec![
            session_meta,
            RolloutItem::Compacted(CompactedItem {
                message: String::new(),
                replacement_history: Some(vec![ResponseItem::Compaction {
                    encrypted_content: "opaque original user requirements".into(),
                }]),
            }),
        ];
        if let Some(version) = version {
            let mut state = lines
                .into_iter()
                .rev()
                .find_map(|line| match line.item {
                    RolloutItem::PostTurnCompletionReviewState(state) => Some(state),
                    _ => None,
                })
                .unwrap();
            state["version"] = serde_json::json!(version);
            legacy.push(RolloutItem::PostTurnCompletionReviewState(state));
        }
        tokio::fs::write(&path, b"").await.unwrap();
        append_crash_fixture(&path, legacy).await;
        let resumed =
            resume_conversation_for_server(&server, home.clone(), path.clone(), |_| {}).await;
        submit_request(&resumed, "use option two").await;
        wait_for_event(&resumed, |event| matches!(event, EventMsg::TurnComplete(_))).await;
        resumed.submit(Op::ReviewCompletedTurn).await.unwrap();
        let event = wait_for_event(&resumed, |event| {
            matches!(event, EventMsg::Error(_) | EventMsg::EnteredReviewMode(_))
        })
        .await;
        assert!(matches!(event, EventMsg::Error(event)
                if event.message.contains("exact consumed-request provenance")));
        assert_eq!(requests.requests().len(), 2);
        let state = read_rollout_lines(&path)
            .into_iter()
            .rev()
            .find_map(|line| match line.item {
                RolloutItem::PostTurnCompletionReviewState(state) => Some(state),
                _ => None,
            })
            .unwrap();
        assert_eq!(state["request_frame"]["exact"], false);
        assert!(state["attempt"].is_null());
        server.verify().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn started_followup_amendment_recovers_after_body_written_without_recorded_snapshot()
-> anyhow::Result<()> {
    skip_if_no_network_unless_localhost!(Ok(()));
    let server = MockServer::start().await;
    let requests = mount_response_sequence(
        &server,
        vec![
            sse_response(answer("Original complete answer.")),
            sse_response(verdict("Verify and replace the complete answer.", true)),
            sse_response(answer("Interrupted follow-up."))
                .set_delay(std::time::Duration::from_secs(5)),
            sse_response(answer("Complete amended replacement.")),
            sse_response(verdict("Checked the complete amended request.", false)),
        ],
    )
    .await;
    let home = Arc::new(TempDir::new()?);
    let original_cwd = home.path().join("workspace");
    let other_cwd = home.path().join("other-project");
    tokio::fs::create_dir_all(&other_cwd).await?;
    let codex = new_conversation_for_server(&server, home.clone(), configure_recovery_models).await;
    submit_request(&codex, "Finish the original project request.").await;
    wait_for_event(&codex, |event| matches!(event, EventMsg::TurnComplete(_))).await;
    codex.submit(Op::ReviewCompletedTurn).await?;
    wait_for_captured_requests(&requests, 3).await;
    let path = codex.rollout_path().context("expected rollout path")?;
    let started = completion_state(&path)?;
    assert!(started["handoff"]["disposition"]["Started"].is_object());
    let amendment = "Also preserve the public API.";
    let content = vec![UserInput::Text {
        text: amendment.into(),
        text_elements: vec![],
    }];
    let body: ResponseItem =
        codex_protocol::models::ResponseInputItem::from(content.clone()).into();
    codex
        .submit(Op::SteerInput {
            expected_turn_id: started["main_task_id"]
                .as_str()
                .context("expected main owner")?
                .into(),
            items: content,
            client_user_message_id: Some("review-followup-amendment".into()),
        })
        .await?;
    codex.submit(Op::Pause).await?;
    wait_for_event(&codex, |event| match event {
        EventMsg::Error(error) => panic!("steering failed: {}", error.message),
        EventMsg::TurnPaused(_) => true,
        _ => false,
    })
    .await;
    codex.submit(Op::Shutdown).await?;
    wait_for_event(&codex, |event| matches!(event, EventMsg::ShutdownComplete)).await;
    let lines = read_rollout_lines(&path);
    let body_index = lines
        .iter()
        .position(|line| matches!(&line.item, RolloutItem::ResponseItem(item) if item == &body))
        .context("expected installed amendment body")?;
    let installing = lines[..body_index]
        .iter()
        .rev()
        .find_map(|line| match &line.item {
            RolloutItem::PostTurnCompletionReviewState(state) => Some(state),
            _ => None,
        })
        .context("expected installing marker")?;
    assert_eq!(installing["replacement_answer_required"], true);
    assert_eq!(
        installing["pending_inputs"][0]["installation"],
        "Installing"
    );
    assert_eq!(
        installing["episode"]["cwd"],
        serde_json::to_value(&original_cwd)?
    );
    // Keep the real accepted/Installing/body prefix, but omit the Recorded snapshot.
    let prefix = lines[..=body_index]
        .iter()
        .map(serde_json::to_string)
        .collect::<Result<Vec<_>, _>>()?
        .join("\n")
        + "\n";
    tokio::fs::write(&path, prefix).await?;
    let resumed =
        resume_conversation_for_server(&server, home.clone(), path, configure_recovery_models)
            .await;
    resumed
        .submit(context_override(Some(other_cwd), None))
        .await?;
    resumed.submit(Op::Continue).await?;
    let rejected = wait_for_event(&resumed, |event| {
        matches!(event, EventMsg::Error(_) | EventMsg::TurnStarted(_))
    })
    .await;
    assert!(
        matches!(rejected, EventMsg::Error(error) if error.message.contains("working directory"))
    );
    assert_eq!(requests.requests().len(), 3);
    resumed
        .submit(context_override(Some(original_cwd), None))
        .await?;
    resumed.submit(Op::Continue).await?;
    wait_for_event(&resumed, |event| match event {
        EventMsg::Error(error) => panic!("amendment recovery failed: {}", error.message),
        EventMsg::TurnComplete(event) => {
            assert_eq!(
                event.last_agent_message.as_deref(),
                Some("Complete amended replacement.")
            );
            true
        }
        _ => false,
    })
    .await;
    resumed.submit(Op::ReviewCompletedTurn).await?;
    wait_for_event(&resumed, |event| {
        matches!(event, EventMsg::ExitedReviewMode(_))
    })
    .await;
    wait_for_event(&resumed, |event| matches!(event, EventMsg::TurnComplete(_))).await;
    let captured = requests.requests();
    assert_eq!(captured.len(), 5);
    assert_eq!(
        captured[3]
            .input()
            .iter()
            .filter(|item| item["role"] == "user" && item["content"][0]["text"] == amendment)
            .count(),
        1
    );
    let main_input = serde_json::to_string(&captured[3].input())?;
    assert!(main_input.contains("self-contained final answer"));
    let review = packet(&captured[4]);
    for required in [
        "Finish the original project request.",
        amendment,
        "Complete amended replacement.",
    ] {
        assert!(review.contains(required), "missing {required:?}");
    }
    server.verify().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn accepted_and_partially_installed_amendments_reach_model_and_next_review_after_restart() {
    skip_if_no_network_unless_localhost!();
    for body_written in [false, true] {
        let server = MockServer::start().await;
        let requests = mount_sse_sequence(
            &server,
            vec![
                answer("previous complete answer"),
                answer("complete answer incorporating the accepted amendment"),
                verdict("checked the amended request", false),
            ],
        )
        .await;
        let home = Arc::new(TempDir::new().unwrap());
        let codex = new_conversation_for_server(&server, home.clone(), |_| {}).await;
        submit_request(&codex, "original complete scope").await;
        wait_for_event(&codex, |event| matches!(event, EventMsg::TurnComplete(_))).await;
        let path = codex.rollout_path().unwrap();
        codex.submit(Op::Shutdown).await.unwrap();
        wait_for_event(&codex, |event| matches!(event, EventMsg::ShutdownComplete)).await;
        let mut state = read_rollout_lines(&path)
            .into_iter()
            .rev()
            .find_map(|line| match line.item {
                RolloutItem::PostTurnCompletionReviewState(state) => Some(state),
                _ => None,
            })
            .unwrap();
        let revision = state["input_revision"].as_u64().unwrap() + 1;
        let content = vec![UserInput::Text {
            text: "accepted amendment: preserve the public API".into(),
            text_elements: vec![],
        }];
        let item: ResponseItem =
            codex_protocol::models::ResponseInputItem::from(content.clone()).into();
        let episode = &state["episode"];
        state["request_frame"]["prior_user_rounds"] = serde_json::json!([{
            "user_messages": episode["request_frame"]["current_user_messages"],
            "final_agent_message": episode["final_agent_message"],
        }]);
        state["request_frame"]["current_user_messages"] = serde_json::json!([]);
        state["input_revision"] = serde_json::json!(revision);
        state["pending_inputs"] = serde_json::json!([{
            "revision": revision,
            "content": content,
            "client_id": "accepted-amendment",
            "response_item": item,
            "installation": if body_written { "Installing" } else { "Accepted" },
        }]);
        state["main_task_id"] = serde_json::json!("interrupted-main");
        let mut fixture = vec![RolloutItem::PostTurnCompletionReviewState(state)];
        if body_written {
            fixture.push(RolloutItem::ResponseItem(item));
        }
        append_crash_fixture(&path, fixture).await;
        let resumed = resume_conversation_for_server(&server, home.clone(), path, |_| {}).await;
        resumed.submit(Op::Continue).await.unwrap();
        wait_for_event(&resumed, |event| matches!(event, EventMsg::TurnComplete(event)
            if event.last_agent_message.as_deref() == Some("complete answer incorporating the accepted amendment"))).await;
        resumed.submit(Op::ReviewCompletedTurn).await.unwrap();
        wait_for_event(&resumed, |event| {
            matches!(event, EventMsg::ExitedReviewMode(_))
        })
        .await;
        wait_for_event(&resumed, |event| matches!(event, EventMsg::TurnComplete(_))).await;
        let requests = requests.requests();
        assert_eq!(requests.len(), 3);
        assert_eq!(
            requests[1]
                .input()
                .iter()
                .filter(|input| {
                    input["role"] == "user"
                        && input["content"][0]["text"]
                            == "accepted amendment: preserve the public API"
                })
                .count(),
            1
        );
        let review = packet(&requests[2]);
        assert!(review.contains("original complete scope"));
        assert!(review.contains("accepted amendment: preserve the public API"));
        assert!(review.contains("complete answer incorporating the accepted amendment"));
        server.verify().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn new_user_input_replaces_running_reviewer_without_stale_advice() {
    skip_if_no_network_unless_localhost!();
    let server = MockServer::start().await;
    let requests = mount_response_sequence(
        &server,
        vec![
            sse_response(answer("old answer")),
            sse_response(verdict("stale correction", true))
                .set_delay(std::time::Duration::from_secs(5)),
            sse_response(answer("new request result")),
        ],
    )
    .await;
    let home = Arc::new(TempDir::new().unwrap());
    let codex = new_conversation_for_server(&server, home.clone(), |_| {}).await;
    submit_request(&codex, "old request").await;
    wait_for_event(&codex, |event| matches!(event, EventMsg::TurnComplete(_))).await;
    codex.submit(Op::ReviewCompletedTurn).await.unwrap();
    wait_for_captured_requests(&requests, 2).await;
    submit_request(&codex, "new request supersedes the old one").await;
    wait_for_event(&codex, |event| {
        matches!(event, EventMsg::TurnComplete(event)
        if event.last_agent_message.as_deref() == Some("new request result"))
    })
    .await;
    let requests = requests.requests();
    assert_eq!(requests.len(), 3);
    assert!(
        !serde_json::to_string(&requests[2].input())
            .unwrap()
            .contains("stale correction")
    );
    server.verify().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn durable_handoffs_recover_prepared_and_started_work_without_duplicate_advice() {
    skip_if_no_network_unless_localhost!();
    for (started, advisory_already_written) in [(false, false), (true, false), (true, true)] {
        let server = MockServer::start().await;
        let requests = mount_sse_sequence(
            &server,
            vec![
                answer("original complete answer"),
                answer("recovered complete answer"),
            ],
        )
        .await;
        let home = Arc::new(TempDir::new().unwrap());
        let codex = new_conversation_for_server(&server, home.clone(), |_| {}).await;
        submit_request(&codex, "original complete user scope").await;
        wait_for_event(&codex, |event| matches!(event, EventMsg::TurnComplete(_))).await;
        let path = codex.rollout_path().unwrap();
        codex.submit(Op::Shutdown).await.unwrap();
        wait_for_event(&codex, |event| matches!(event, EventMsg::ShutdownComplete)).await;
        let mut state = read_rollout_lines(&path)
            .into_iter()
            .rev()
            .find_map(|line| match line.item {
                RolloutItem::PostTurnCompletionReviewState(state) => Some(state),
                _ => None,
            })
            .unwrap();
        let episode = &state["episode"];
        let ticket = serde_json::json!({
            "episode_id": episode["episode_id"],
            "request_revision": episode["request_revision"],
            "history_generation": episode["history_generation"],
            "candidate_completion_id": episode["turn_id"],
            "attempt_id": "crash-fixture-attempt",
            "automatic": false,
        });
        state["attempt"] = ticket.clone();
        let advisory = "Verify this advisory and write a self-contained final answer replacing the earlier answer, even when rejecting every finding.";
        state["handoff"] = serde_json::json!({
            "ticket": ticket,
            "advisory": advisory,
            "model": "gpt-5.4",
            "disposition": if started { serde_json::json!({"Started": {"turn_id": "interrupted-main"}}) } else { serde_json::json!("Prepared") },
            "advisory_recorded": false,
        });
        state["replacement_answer_required"] = serde_json::json!(started);
        // This is the durable boundary left by a process stopping before enqueue
        // or immediately after the main task claimed the handoff.
        let mut fixture = Vec::new();
        if advisory_already_written {
            let item: ResponseItem =
                codex_protocol::models::DeveloperInstructions::new(advisory).into();
            fixture.push(RolloutItem::ResponseItem(item));
        }
        fixture.push(RolloutItem::PostTurnCompletionReviewState(state));
        append_crash_fixture(&path, fixture).await;
        let resumed =
            resume_conversation_for_server(&server, home.clone(), path.clone(), |_| {}).await;
        if started {
            resumed.submit(Op::Continue).await.unwrap();
        }
        let continued = wait_for_event(&resumed, |event| {
            matches!(event, EventMsg::TurnContinued(_))
        })
        .await;
        assert!(
            matches!(continued, EventMsg::TurnContinued(event) if event.source == if started {
                TurnContinuationSource::Interrupted
            } else {
                TurnContinuationSource::PostTurnCompletionReview
            })
        );
        wait_for_event(&resumed, |event| {
            matches!(event, EventMsg::TurnComplete(event)
            if event.last_agent_message.as_deref() == Some("recovered complete answer"))
        })
        .await;
        let requests = requests.requests();
        assert_eq!(requests.len(), 2);
        let input = serde_json::to_string(&requests[1].input()).unwrap();
        assert!(input.contains("original complete user scope"));
        assert!(input.contains("self-contained final answer"));
        let lines = read_rollout_lines(&path);
        assert_eq!(
            lines
                .iter()
                .filter(|line| matches!(&line.item,
                    RolloutItem::ResponseItem(ResponseItem::Message { role, content, .. })
                        if role == "developer" && content == &vec![ContentItem::InputText { text: advisory.into() }]
                ))
                .count(),
            1
        );
        server.verify().await;
    }
}
