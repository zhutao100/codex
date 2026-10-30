use super::*;
use codex_core::features::Feature;
use core_test_support::responses::ResponsesRequest;
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
        let record = RolloutLine {
            timestamp: "2026-01-01T00:00:00.000Z".into(),
            item: RolloutItem::PostTurnCompletionReviewState(state),
        };
        let mut file = tokio::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .await
            .unwrap();
        if advisory_already_written {
            let item: ResponseItem =
                codex_protocol::models::DeveloperInstructions::new(advisory).into();
            let prior = RolloutLine {
                timestamp: record.timestamp.clone(),
                item: RolloutItem::ResponseItem(item),
            };
            file.write_all(format!("{}\n", serde_json::to_string(&prior).unwrap()).as_bytes())
                .await
                .unwrap();
        }
        file.write_all(format!("{}\n", serde_json::to_string(&record).unwrap()).as_bytes())
            .await
            .unwrap();
        file.sync_all().await.unwrap();
        drop(file);
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
