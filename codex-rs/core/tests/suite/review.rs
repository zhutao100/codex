use codex_core::CodexAuth;
use codex_core::CodexThread;
use codex_core::ContentItem;
use codex_core::ModelProviderInfo;
use codex_core::REVIEW_PROMPT;
use codex_core::ResponseItem;
use codex_core::WireApi;
use codex_core::config::Config;
use codex_core::models_manager::overlay::ModelInfoPatch;
use codex_core::models_manager::overlay::ModelOverlay;
use codex_core::models_manager::overlay::ModelOverlayEntry;
use codex_core::protocol::ENVIRONMENT_CONTEXT_OPEN_TAG;
use codex_core::protocol::EventMsg;
use codex_core::protocol::ExitedReviewModeEvent;
use codex_core::protocol::Op;
use codex_core::protocol::ReviewCodeLocation;
use codex_core::protocol::ReviewFinding;
use codex_core::protocol::ReviewLineRange;
use codex_core::protocol::ReviewOutputEvent;
use codex_core::protocol::ReviewRequest;
use codex_core::protocol::ReviewTarget;
use codex_core::protocol::RolloutItem;
use codex_core::protocol::RolloutLine;
use codex_core::protocol::TurnContinuationSource;
use codex_core::review_format::render_review_output_text;
use codex_protocol::user_input::UserInput;
use core_test_support::load_sse_fixture_with_id_from_str;
use core_test_support::responses::ResponseMock;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_reasoning_item;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_sse_sequence;
use core_test_support::responses::sse;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use pretty_assertions::assert_eq;
use serial_test::serial;
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::Arc;
use tempfile::TempDir;
use tokio::io::AsyncWriteExt as _;
use uuid::Uuid;
use wiremock::MockServer;

const REVIEW_PROVIDER_API_KEY_ENV: &str = "CODEX_REVIEW_TEST_PROVIDER_API_KEY";

/// Verify that submitting `Op::Review` spawns a child task and emits
/// EnteredReviewMode -> ExitedReviewMode(None) -> TurnComplete
/// in that order when the model returns a structured review JSON payload.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn review_op_emits_lifecycle_and_review_output() {
    // Skip under Codex sandbox network restrictions.
    skip_if_no_network!();

    // Start mock Responses API server. Return a single assistant message whose
    // text is a JSON-encoded ReviewOutputEvent.
    let review_json = serde_json::json!({
        "findings": [
            {
                "title": "Prefer Stylize helpers",
                "body": "Use .dim()/.bold() chaining instead of manual Style where possible.",
                "confidence_score": 0.9,
                "priority": 1,
                "code_location": {
                    "absolute_file_path": "/tmp/file.rs",
                    "line_range": {"start": 10, "end": 20}
                }
            }
        ],
        "overall_correctness": "good",
        "overall_explanation": "All good with some improvements suggested.",
        "overall_confidence_score": 0.8
    })
    .to_string();
    let sse_template = r#"[
            {"type":"response.output_item.done", "item":{
                "type":"message", "role":"assistant",
                "content":[{"type":"output_text","text":__REVIEW__}]
            }},
            {"type":"response.completed", "response": {"id": "__ID__"}}
        ]"#;
    let review_json_escaped = serde_json::to_string(&review_json).unwrap();
    let sse_raw = sse_template.replace("__REVIEW__", &review_json_escaped);
    let (server, _request_log) = start_responses_server_with_sse(&sse_raw, 1).await;
    let codex_home = Arc::new(TempDir::new().unwrap());
    let codex = new_conversation_for_server(&server, codex_home.clone(), |_| {}).await;

    // Submit review request.
    codex
        .submit(Op::Review {
            review_request: ReviewRequest {
                target: ReviewTarget::Custom {
                    instructions: "Please review my changes".to_string(),
                },
                user_facing_hint: None,
            },
        })
        .await
        .unwrap();

    // Verify lifecycle: Entered -> Exited(Some(review)) -> TurnComplete.
    let _entered = wait_for_event(&codex, |ev| matches!(ev, EventMsg::EnteredReviewMode(_))).await;
    let closed = wait_for_event(&codex, |ev| matches!(ev, EventMsg::ExitedReviewMode(_))).await;
    let review = match closed {
        EventMsg::ExitedReviewMode(ev) => ev
            .review_output
            .expect("expected ExitedReviewMode with Some(review_output)"),
        other => panic!("expected ExitedReviewMode(..), got {other:?}"),
    };

    // Deep compare full structure using PartialEq (floats are f32 on both sides).
    let expected = ReviewOutputEvent {
        findings: vec![ReviewFinding {
            title: "Prefer Stylize helpers".to_string(),
            body: "Use .dim()/.bold() chaining instead of manual Style where possible.".to_string(),
            confidence_score: 0.9,
            priority: 1,
            code_location: ReviewCodeLocation {
                absolute_file_path: PathBuf::from("/tmp/file.rs"),
                line_range: ReviewLineRange { start: 10, end: 20 },
            },
        }],
        overall_correctness: "good".to_string(),
        overall_explanation: "All good with some improvements suggested.".to_string(),
        overall_confidence_score: 0.8,
    };
    assert_eq!(expected, review);
    let _complete = wait_for_event(&codex, |ev| matches!(ev, EventMsg::TurnComplete(_))).await;

    // Also verify that a user message with the header and a formatted finding
    // was recorded back in the parent session's rollout.
    let path = codex.rollout_path().expect("rollout path");
    let text = std::fs::read_to_string(&path).expect("read rollout file");

    let mut saw_header = false;
    let mut saw_finding_line = false;
    let expected_assistant_text = render_review_output_text(&expected);
    let mut saw_assistant_plain = false;
    let mut saw_assistant_xml = false;
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let v: serde_json::Value = serde_json::from_str(line).expect("jsonl line");
        let rl: RolloutLine = serde_json::from_value(v).expect("rollout line");
        if let RolloutItem::ResponseItem(ResponseItem::Message { role, content, .. }) = rl.item {
            if role == "user" {
                for c in content {
                    if let ContentItem::InputText { text } = c {
                        if text.contains("full review output from reviewer model") {
                            saw_header = true;
                        }
                        if text.contains("- Prefer Stylize helpers — /tmp/file.rs:10-20") {
                            saw_finding_line = true;
                        }
                    }
                }
            } else if role == "assistant" {
                for c in content {
                    if let ContentItem::OutputText { text } = c {
                        if text.contains("<user_action>") {
                            saw_assistant_xml = true;
                        }
                        if text == expected_assistant_text {
                            saw_assistant_plain = true;
                        }
                    }
                }
            }
        }
    }
    assert!(saw_header, "user header missing from rollout");
    assert!(
        saw_finding_line,
        "formatted finding line missing from rollout"
    );
    assert!(
        saw_assistant_plain,
        "assistant review output missing from rollout"
    );
    assert!(
        !saw_assistant_xml,
        "assistant review output contains user_action markup"
    );

    let _codex_home_guard = codex_home;
    server.verify().await;
}

/// When the model returns plain text that is not JSON, ensure the child
/// lifecycle still occurs and the plain text is surfaced via
/// ExitedReviewMode(Some(..)) as the overall_explanation.
// Windows CI only: bump to 4 workers to prevent SSE/event starvation and test timeouts.
#[cfg_attr(windows, tokio::test(flavor = "multi_thread", worker_threads = 4))]
#[cfg_attr(not(windows), tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn review_op_with_plain_text_emits_review_fallback() {
    skip_if_no_network!();

    let sse_raw = r#"[
        {"type":"response.output_item.done", "item":{
            "type":"message", "role":"assistant",
            "content":[{"type":"output_text","text":"just plain text"}]
        }},
        {"type":"response.completed", "response": {"id": "__ID__"}}
    ]"#;
    let (server, _request_log) = start_responses_server_with_sse(sse_raw, 1).await;
    let codex_home = Arc::new(TempDir::new().unwrap());
    let codex = new_conversation_for_server(&server, codex_home.clone(), |_| {}).await;

    codex
        .submit(Op::Review {
            review_request: ReviewRequest {
                target: ReviewTarget::Custom {
                    instructions: "Plain text review".to_string(),
                },
                user_facing_hint: None,
            },
        })
        .await
        .unwrap();

    let _entered = wait_for_event(&codex, |ev| matches!(ev, EventMsg::EnteredReviewMode(_))).await;
    let closed = wait_for_event(&codex, |ev| matches!(ev, EventMsg::ExitedReviewMode(_))).await;
    let review = match closed {
        EventMsg::ExitedReviewMode(ev) => ev
            .review_output
            .expect("expected ExitedReviewMode with Some(review_output)"),
        other => panic!("expected ExitedReviewMode(..), got {other:?}"),
    };

    // Expect a structured fallback carrying the plain text.
    let expected = ReviewOutputEvent {
        overall_explanation: "just plain text".to_string(),
        ..Default::default()
    };
    assert_eq!(expected, review);
    let _complete = wait_for_event(&codex, |ev| matches!(ev, EventMsg::TurnComplete(_))).await;

    let _codex_home_guard = codex_home;
    server.verify().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn review_completed_turn_positive_output_records_developer_advisory_and_continues() {
    skip_if_no_network!();

    let review_json = serde_json::json!({
        "evaluation": "A concrete follow-up is needed.",
        "fix_actions_advised": true
    })
    .to_string();
    let review_json_escaped = serde_json::to_string(&review_json).unwrap();
    let sse_parent = r#"[
        {"type":"response.output_item.done", "item":{
            "type":"message", "role":"assistant",
            "content":[{"type":"output_text","text":"initial done"}]
        }},
        {"type":"response.completed", "response": {"id": "__ID__"}}
    ]"#;
    let sse_review = format!(
        r#"[
            {{"type":"response.output_item.done", "item":{{
                "type":"message", "role":"assistant",
                "content":[{{"type":"output_text","text":{review_json_escaped}}}]
            }}}},
            {{"type":"response.completed", "response": {{"id": "__ID__"}}}}
        ]"#
    );
    let sse_continue = r#"[
        {"type":"response.output_item.done", "item":{
            "type":"message", "role":"assistant",
            "content":[{"type":"output_text","text":"follow-up done"}]
        }},
        {"type":"response.completed", "response": {"id": "__ID__"}}
    ]"#;
    let server = MockServer::start().await;
    let request_log = mount_sse_sequence(
        &server,
        vec![
            load_sse_fixture_with_id_from_str(sse_parent, &Uuid::new_v4().to_string()),
            load_sse_fixture_with_id_from_str(&sse_review, &Uuid::new_v4().to_string()),
            load_sse_fixture_with_id_from_str(sse_continue, &Uuid::new_v4().to_string()),
        ],
    )
    .await;
    let codex_home = Arc::new(TempDir::new().unwrap());
    let codex = new_conversation_for_server(&server, codex_home.clone(), |_| {}).await;

    codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "finish the change".to_string(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
        })
        .await
        .unwrap();
    let _parent_complete =
        wait_for_event(&codex, |ev| matches!(ev, EventMsg::TurnComplete(_))).await;

    codex.submit(Op::ReviewCompletedTurn).await.unwrap();

    let entered = wait_for_event(&codex, |ev| matches!(ev, EventMsg::EnteredReviewMode(_))).await;
    match entered {
        EventMsg::EnteredReviewMode(request) => {
            assert_eq!(request.user_facing_hint.as_deref(), Some("completed turn"));
        }
        other => panic!("expected EnteredReviewMode, got {other:?}"),
    }
    let exited = wait_for_event(&codex, |ev| matches!(ev, EventMsg::ExitedReviewMode(_))).await;
    match exited {
        EventMsg::ExitedReviewMode(event) => {
            let output = event
                .post_turn_completion_review_output
                .expect("post-turn review output");
            assert_eq!(output.evaluation, "A concrete follow-up is needed.");
            assert!(output.fix_actions_advised);
        }
        other => panic!("expected ExitedReviewMode, got {other:?}"),
    }
    let _review_complete =
        wait_for_event(&codex, |ev| matches!(ev, EventMsg::TurnComplete(_))).await;
    let continued = wait_for_event(&codex, |ev| matches!(ev, EventMsg::TurnContinued(_))).await;
    match continued {
        EventMsg::TurnContinued(event) => {
            assert_eq!(
                event.source,
                TurnContinuationSource::PostTurnCompletionReview
            );
        }
        other => panic!("expected TurnContinued, got {other:?}"),
    }
    let _continuation_complete =
        wait_for_event(&codex, |ev| matches!(ev, EventMsg::TurnComplete(_))).await;

    let requests = request_log.requests();
    assert_eq!(requests.len(), 3);
    let delegate_input = serde_json::to_string(&requests[1].input()).expect("serialize input");
    assert!(
        delegate_input.contains("<completed_turn_review_context>"),
        "delegate input should contain completed-turn context: {delegate_input}"
    );
    assert!(delegate_input.contains("finish the change"));
    assert!(delegate_input.contains("initial done"));

    let continuation_body = requests[2].body_json();
    let continuation_input = continuation_body["input"].as_array().expect("input array");
    let saw_advisory = continuation_input.iter().any(|item| {
        item["role"].as_str() == Some("developer")
            && item["content"][0]["text"]
                .as_str()
                .unwrap_or_default()
                .contains("<post_turn_completion_review>")
    });
    assert!(
        saw_advisory,
        "developer advisory missing from continuation request"
    );

    let _codex_home_guard = codex_home;
    server.verify().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn review_completed_turn_passes_multi_round_interaction_history() {
    skip_if_no_network!();

    let review_json = serde_json::json!({
        "evaluation": "Inspection coverage: checked request-fulfillment checklist.\n\nFindings:\n- None.\n\nFix actions advised: no",
        "fix_actions_advised": false
    })
    .to_string();
    let review_json_escaped = serde_json::to_string(&review_json).unwrap();
    let sse_round_one = r#"[
        {"type":"response.output_item.done", "item":{
            "type":"message", "role":"assistant",
            "content":[{"type":"output_text","text":"round one final"}]
        }},
        {"type":"response.completed", "response": {"id": "__ID__"}}
    ]"#;
    let sse_round_two = r#"[
        {"type":"response.output_item.done", "item":{
            "type":"message", "role":"assistant",
            "content":[{"type":"output_text","text":"round two final"}]
        }},
        {"type":"response.completed", "response": {"id": "__ID__"}}
    ]"#;
    let sse_review = format!(
        r#"[
            {{"type":"response.output_item.done", "item":{{
                "type":"message", "role":"assistant",
                "content":[{{"type":"output_text","text":{review_json_escaped}}}]
            }}}},
            {{"type":"response.completed", "response": {{"id": "__ID__"}}}}
        ]"#
    );
    let server = MockServer::start().await;
    let request_log = mount_sse_sequence(
        &server,
        vec![
            load_sse_fixture_with_id_from_str(sse_round_one, &Uuid::new_v4().to_string()),
            load_sse_fixture_with_id_from_str(sse_round_two, &Uuid::new_v4().to_string()),
            load_sse_fixture_with_id_from_str(&sse_review, &Uuid::new_v4().to_string()),
        ],
    )
    .await;
    let codex_home = Arc::new(TempDir::new().unwrap());
    let codex = new_conversation_for_server(&server, codex_home.clone(), |_| {}).await;

    codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "round one request".to_string(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
        })
        .await
        .unwrap();
    let _round_one_complete =
        wait_for_event(&codex, |ev| matches!(ev, EventMsg::TurnComplete(_))).await;

    codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "round two request".to_string(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
        })
        .await
        .unwrap();
    let _round_two_complete =
        wait_for_event(&codex, |ev| matches!(ev, EventMsg::TurnComplete(_))).await;

    codex.submit(Op::ReviewCompletedTurn).await.unwrap();
    let _entered = wait_for_event(&codex, |ev| matches!(ev, EventMsg::EnteredReviewMode(_))).await;
    let _exited = wait_for_event(&codex, |ev| matches!(ev, EventMsg::ExitedReviewMode(_))).await;
    let _review_complete =
        wait_for_event(&codex, |ev| matches!(ev, EventMsg::TurnComplete(_))).await;

    let requests = request_log.requests();
    assert_eq!(requests.len(), 3);
    let delegate_input = serde_json::to_string(&requests[2].input()).expect("serialize input");
    assert!(
        delegate_input.contains("<session_interaction_history"),
        "delegate input should contain session interaction history: {delegate_input}"
    );
    assert!(delegate_input.contains("<round index=\\\"1\\\">"));
    assert!(delegate_input.contains("round one request"));
    assert!(delegate_input.contains("round one final"));
    assert!(delegate_input.contains("<round index=\\\"2\\\">"));
    assert!(delegate_input.contains("round two request"));
    assert!(delegate_input.contains("round two final"));

    let _codex_home_guard = codex_home;
    server.verify().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn review_completed_turn_false_output_does_not_continue() {
    skip_if_no_network!();

    let review_json = serde_json::json!({
        "evaluation": "Inspection coverage: checked changed files and tests.\n\nFindings:\n- None.\n\nFix actions advised: no",
        "fix_actions_advised": false
    })
    .to_string();
    let review_json_escaped = serde_json::to_string(&review_json).unwrap();
    let sse_parent = r#"[
        {"type":"response.output_item.done", "item":{
            "type":"message", "role":"assistant",
            "content":[{"type":"output_text","text":"initial done"}]
        }},
        {"type":"response.completed", "response": {"id": "__ID__"}}
    ]"#;
    let sse_review = format!(
        r#"[
            {{"type":"response.output_item.done", "item":{{
                "type":"message", "role":"assistant",
                "content":[{{"type":"output_text","text":{review_json_escaped}}}]
            }}}},
            {{"type":"response.completed", "response": {{"id": "__ID__"}}}}
        ]"#
    );
    let server = MockServer::start().await;
    let request_log = mount_sse_sequence(
        &server,
        vec![
            load_sse_fixture_with_id_from_str(sse_parent, &Uuid::new_v4().to_string()),
            load_sse_fixture_with_id_from_str(&sse_review, &Uuid::new_v4().to_string()),
        ],
    )
    .await;
    let codex_home = Arc::new(TempDir::new().unwrap());
    let codex = new_conversation_for_server(&server, codex_home.clone(), |_| {}).await;

    codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "finish the change".to_string(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
        })
        .await
        .unwrap();
    let _parent_complete =
        wait_for_event(&codex, |ev| matches!(ev, EventMsg::TurnComplete(_))).await;

    codex.submit(Op::ReviewCompletedTurn).await.unwrap();
    let _entered = wait_for_event(&codex, |ev| matches!(ev, EventMsg::EnteredReviewMode(_))).await;
    let exited = wait_for_event(&codex, |ev| matches!(ev, EventMsg::ExitedReviewMode(_))).await;
    match exited {
        EventMsg::ExitedReviewMode(event) => {
            let output = event
                .post_turn_completion_review_output
                .expect("post-turn review output");
            assert!(output.evaluation.starts_with("Inspection coverage:"));
            assert!(!output.fix_actions_advised);
        }
        other => panic!("expected ExitedReviewMode, got {other:?}"),
    }
    let _review_complete =
        wait_for_event(&codex, |ev| matches!(ev, EventMsg::TurnComplete(_))).await;

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert_eq!(
        request_log.requests().len(),
        2,
        "false fix_actions_advised should not trigger a continuation request"
    );

    let _codex_home_guard = codex_home;
    server.verify().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn post_turn_review_delegate_keeps_project_docs_and_uses_review_host_instructions() {
    skip_if_no_network!();

    let review_json = serde_json::json!({
        "evaluation": "Inspection coverage: checked project docs.\n\nFindings:\n- None.\n\nFix actions advised: no",
        "fix_actions_advised": false
    })
    .to_string();
    let review_json_escaped = serde_json::to_string(&review_json).unwrap();
    let sse_parent = r#"[
        {"type":"response.output_item.done", "item":{
            "type":"message", "role":"assistant",
            "content":[{"type":"output_text","text":"initial done"}]
        }},
        {"type":"response.completed", "response": {"id": "__ID__"}}
    ]"#;
    let sse_review = format!(
        r#"[
            {{"type":"response.output_item.done", "item":{{
                "type":"message", "role":"assistant",
                "content":[{{"type":"output_text","text":{review_json_escaped}}}]
            }}}},
            {{"type":"response.completed", "response": {{"id": "__ID__"}}}}
        ]"#
    );
    let server = MockServer::start().await;
    let request_log = mount_sse_sequence(
        &server,
        vec![
            load_sse_fixture_with_id_from_str(sse_parent, &Uuid::new_v4().to_string()),
            load_sse_fixture_with_id_from_str(&sse_review, &Uuid::new_v4().to_string()),
        ],
    )
    .await;
    let codex_home = Arc::new(TempDir::new().unwrap());
    std::fs::write(
        codex_home.path().join("AGENTS.md"),
        "main-session-only editing workflow",
    )
    .unwrap();
    std::fs::write(
        codex_home.path().join("AGENTS.post-turn-review.md"),
        "review-safe machine resource notes",
    )
    .unwrap();
    let codex = new_conversation_for_server(&server, codex_home.clone(), |config| {
        std::fs::write(
            config.cwd.join("AGENTS.md"),
            "project build and test guidance",
        )
        .unwrap();
    })
    .await;

    codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "finish the change".to_string(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
        })
        .await
        .unwrap();
    let _parent_complete =
        wait_for_event(&codex, |ev| matches!(ev, EventMsg::TurnComplete(_))).await;

    codex.submit(Op::ReviewCompletedTurn).await.unwrap();
    let _entered = wait_for_event(&codex, |ev| matches!(ev, EventMsg::EnteredReviewMode(_))).await;
    let _exited = wait_for_event(&codex, |ev| matches!(ev, EventMsg::ExitedReviewMode(_))).await;
    let _review_complete =
        wait_for_event(&codex, |ev| matches!(ev, EventMsg::TurnComplete(_))).await;

    let requests = request_log.requests();
    assert_eq!(requests.len(), 2);
    let delegate_body = requests[1].body_json();
    let delegate_input = delegate_body["input"].as_array().expect("input array");
    let delegate_text = delegate_input
        .iter()
        .filter_map(|item| item["content"][0]["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n");

    assert!(
        delegate_text.contains("review-safe machine resource notes"),
        "post-turn delegate should use review host instructions: {delegate_text}"
    );
    assert!(
        delegate_text.contains("project build and test guidance"),
        "post-turn delegate should keep project AGENTS.md docs: {delegate_text}"
    );
    assert!(
        !delegate_text.contains("main-session-only editing workflow"),
        "post-turn delegate should not inherit main-session host instructions: {delegate_text}"
    );

    let _codex_home_guard = codex_home;
    server.verify().await;
}

/// Ensure review flow suppresses assistant-specific streaming/completion events:
/// - AgentMessageContentDelta
/// - AgentMessageDelta (legacy)
/// - ItemCompleted for TurnItem::AgentMessage
// Windows CI only: bump to 4 workers to prevent SSE/event starvation and test timeouts.
#[cfg_attr(windows, tokio::test(flavor = "multi_thread", worker_threads = 4))]
#[cfg_attr(not(windows), tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn review_filters_agent_message_related_events() {
    skip_if_no_network!();

    // Stream simulating a typing assistant message with deltas and finalization.
    let sse_raw = r#"[
        {"type":"response.output_item.added", "item":{
            "type":"message", "role":"assistant", "id":"msg-1",
            "content":[{"type":"output_text","text":""}]
        }},
        {"type":"response.output_text.delta", "delta":"Hi"},
        {"type":"response.output_text.delta", "delta":" there"},
        {"type":"response.output_item.done", "item":{
            "type":"message", "role":"assistant", "id":"msg-1",
            "content":[{"type":"output_text","text":"Hi there"}]
        }},
        {"type":"response.completed", "response": {"id": "__ID__"}}
    ]"#;
    let (server, _request_log) = start_responses_server_with_sse(sse_raw, 1).await;
    let codex_home = Arc::new(TempDir::new().unwrap());
    let codex = new_conversation_for_server(&server, codex_home.clone(), |_| {}).await;

    codex
        .submit(Op::Review {
            review_request: ReviewRequest {
                target: ReviewTarget::Custom {
                    instructions: "Filter streaming events".to_string(),
                },
                user_facing_hint: None,
            },
        })
        .await
        .unwrap();

    let mut saw_entered = false;
    let mut saw_exited = false;

    // Drain until TurnComplete; assert streaming-related events never surface.
    wait_for_event(&codex, |event| match event {
        EventMsg::TurnComplete(_) => true,
        EventMsg::EnteredReviewMode(_) => {
            saw_entered = true;
            false
        }
        EventMsg::ExitedReviewMode(_) => {
            saw_exited = true;
            false
        }
        // The following must be filtered by review flow
        EventMsg::AgentMessageContentDelta(_) => {
            panic!("unexpected AgentMessageContentDelta surfaced during review")
        }
        EventMsg::AgentMessageDelta(_) => {
            panic!("unexpected AgentMessageDelta surfaced during review")
        }
        _ => false,
    })
    .await;
    assert!(saw_entered && saw_exited, "missing review lifecycle events");

    let _codex_home_guard = codex_home;
    server.verify().await;
}

/// When the model returns structured JSON in a review, ensure only a single
/// non-streaming AgentMessage is emitted; the UI consumes the structured
/// result via ExitedReviewMode plus a final assistant message.
// Windows CI only: bump to 4 workers to prevent SSE/event starvation and test timeouts.
#[cfg_attr(windows, tokio::test(flavor = "multi_thread", worker_threads = 4))]
#[cfg_attr(not(windows), tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn review_does_not_emit_agent_message_on_structured_output() {
    skip_if_no_network!();

    let review_json = serde_json::json!({
        "findings": [
            {
                "title": "Example",
                "body": "Structured review output.",
                "confidence_score": 0.5,
                "priority": 1,
                "code_location": {
                    "absolute_file_path": "/tmp/file.rs",
                    "line_range": {"start": 1, "end": 2}
                }
            }
        ],
        "overall_correctness": "ok",
        "overall_explanation": "ok",
        "overall_confidence_score": 0.5
    })
    .to_string();
    let sse_template = r#"[
            {"type":"response.output_item.done", "item":{
                "type":"message", "role":"assistant",
                "content":[{"type":"output_text","text":__REVIEW__}]
            }},
            {"type":"response.completed", "response": {"id": "__ID__"}}
        ]"#;
    let review_json_escaped = serde_json::to_string(&review_json).unwrap();
    let sse_raw = sse_template.replace("__REVIEW__", &review_json_escaped);
    let (server, _request_log) = start_responses_server_with_sse(&sse_raw, 1).await;
    let codex_home = Arc::new(TempDir::new().unwrap());
    let codex = new_conversation_for_server(&server, codex_home.clone(), |_| {}).await;

    codex
        .submit(Op::Review {
            review_request: ReviewRequest {
                target: ReviewTarget::Custom {
                    instructions: "check structured".to_string(),
                },
                user_facing_hint: None,
            },
        })
        .await
        .unwrap();

    // Drain events until TurnComplete; ensure we only see a final
    // AgentMessage (no streaming assistant messages).
    let mut saw_entered = false;
    let mut saw_exited = false;
    let mut agent_messages = 0;
    wait_for_event(&codex, |event| match event {
        EventMsg::TurnComplete(_) => true,
        EventMsg::AgentMessage(_) => {
            agent_messages += 1;
            false
        }
        EventMsg::EnteredReviewMode(_) => {
            saw_entered = true;
            false
        }
        EventMsg::ExitedReviewMode(_) => {
            saw_exited = true;
            false
        }
        _ => false,
    })
    .await;
    assert_eq!(1, agent_messages, "expected exactly one AgentMessage event");
    assert!(saw_entered && saw_exited, "missing review lifecycle events");

    let _codex_home_guard = codex_home;
    server.verify().await;
}

/// Review delegates emit both structured items and legacy compatibility events.
/// The delegate rollout is the canonical transcript, so the parent should not
/// copy forwarded delegate events into its own rollout.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn review_rollout_omits_forwarded_delegate_transcript() {
    let intermediate_text = "intermediate delegate note";
    let review_text = "final review assistant output";
    let server = MockServer::start().await;
    mount_sse_sequence(
        &server,
        vec![sse(vec![
            ev_response_created("resp-1"),
            ev_assistant_message("msg-0", intermediate_text),
            ev_reasoning_item("reason-1", &["delegate reasoning"], &[]),
            ev_assistant_message("msg-1", review_text),
            ev_completed("resp-1"),
        ])],
    )
    .await;
    let codex_home = Arc::new(TempDir::new().unwrap());
    let codex = new_conversation_for_server(&server, codex_home.clone(), |_| {}).await;

    codex
        .submit(Op::Review {
            review_request: ReviewRequest {
                target: ReviewTarget::Custom {
                    instructions: "check rollout duplicates".to_string(),
                },
                user_facing_hint: None,
            },
        })
        .await
        .unwrap();

    let _complete = wait_for_event(&codex, |ev| matches!(ev, EventMsg::TurnComplete(_))).await;

    let path = codex.rollout_path().expect("rollout path");
    let lines = read_rollout_lines(&path);
    let user_message_count =
        rollout_event_count(&lines, |event| matches!(event, EventMsg::UserMessage(_)));
    let agent_reasoning_count =
        rollout_event_count(&lines, |event| matches!(event, EventMsg::AgentReasoning(_)));

    assert_eq!(
        0, user_message_count,
        "delegate user message should not be persisted in parent rollout"
    );
    assert_eq!(
        0, agent_reasoning_count,
        "delegate reasoning should not be persisted in parent rollout"
    );
    assert!(
        !rollout_has_agent_message(&lines, intermediate_text),
        "delegate agent message should not be persisted in parent rollout"
    );
    assert_eq!(
        0,
        adjacent_duplicate_rollout_items(&lines),
        "rollout should not contain adjacent duplicate records"
    );

    let delegate_lines = read_single_delegate_rollout(codex_home.path(), &path);
    assert_eq!(
        1,
        rollout_event_count(&delegate_lines, |event| {
            matches!(event, EventMsg::UserMessage(_))
        }),
        "delegate rollout should keep its user message"
    );
    assert_eq!(
        1,
        rollout_event_count(&delegate_lines, |event| {
            matches!(event, EventMsg::AgentReasoning(_))
        }),
        "delegate rollout should keep its reasoning"
    );
    assert!(
        rollout_has_agent_message(&delegate_lines, intermediate_text),
        "delegate rollout should keep its assistant transcript"
    );

    let _codex_home_guard = codex_home;
    server.verify().await;
}

/// Ensure that when a custom `review_model` is set in the config, the review
/// request uses that model (and not the main chat model).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn review_uses_custom_review_model_from_config() {
    skip_if_no_network!();

    // Minimal stream: just a completed event
    let sse_raw = r#"[
        {"type":"response.completed", "response": {"id": "__ID__"}}
    ]"#;
    let (server, request_log) = start_responses_server_with_sse(sse_raw, 1).await;
    let codex_home = Arc::new(TempDir::new().unwrap());
    // Choose a review model different from the main model; ensure it is used.
    let codex = new_conversation_for_server(&server, codex_home.clone(), |cfg| {
        cfg.model = Some("gpt-4.1".to_string());
        cfg.review_model = Some("gpt-5.1".to_string());
    })
    .await;

    codex
        .submit(Op::Review {
            review_request: ReviewRequest {
                target: ReviewTarget::Custom {
                    instructions: "use custom model".to_string(),
                },
                user_facing_hint: None,
            },
        })
        .await
        .unwrap();

    // Wait for completion
    let _entered = wait_for_event(&codex, |ev| matches!(ev, EventMsg::EnteredReviewMode(_))).await;
    let _closed = wait_for_event(&codex, |ev| {
        matches!(
            ev,
            EventMsg::ExitedReviewMode(ExitedReviewModeEvent {
                review_output: None,
                ..
            })
        )
    })
    .await;
    let _complete = wait_for_event(&codex, |ev| matches!(ev, EventMsg::TurnComplete(_))).await;

    // Assert the request body model equals the configured review model
    let request = request_log.single_request();
    assert_eq!(request.path(), "/v1/responses");
    let body = request.body_json();
    assert_eq!(body["model"].as_str().unwrap(), "gpt-5.1");

    let _codex_home_guard = codex_home;
    server.verify().await;
}

#[serial(env_vars)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn review_model_provider_routes_delegate_only_to_secondary_provider() {
    let sse_raw = r#"[
        {"type":"response.completed", "response": {"id": "__ID__"}}
    ]"#;
    let (primary_server, primary_log) = start_responses_server_with_sse(sse_raw, 1).await;
    let (secondary_server, secondary_log) = start_responses_server_with_sse(sse_raw, 1).await;
    let _env_guard = EnvGuard::set(REVIEW_PROVIDER_API_KEY_ENV, "secondary-key");
    let secondary_base_url = format!("{}/v1", secondary_server.uri());

    let codex_home = Arc::new(TempDir::new().unwrap());
    let mut builder = test_codex()
        .with_auth(CodexAuth::create_dummy_chatgpt_auth_for_testing())
        .with_home(codex_home.clone())
        .with_config(move |cfg| {
            cfg.model = Some("gpt-4.1".to_string());
            cfg.review_model = Some("external-reviewer".to_string());
            cfg.review_model_provider = Some("external-review".to_string());
            cfg.model_providers.insert(
                "external-review".to_string(),
                ModelProviderInfo {
                    name: "External Review".to_string(),
                    base_url: Some(secondary_base_url),
                    env_key: Some(REVIEW_PROVIDER_API_KEY_ENV.to_string()),
                    env_key_instructions: None,
                    experimental_bearer_token: None,
                    wire_api: WireApi::Responses,
                    query_params: None,
                    http_headers: None,
                    env_http_headers: None,
                    request_max_retries: Some(0),
                    stream_max_retries: Some(0),
                    stream_idle_timeout_ms: Some(5_000),
                    requires_openai_auth: false,
                    supports_websockets: false,
                },
            );
        });
    let test = builder
        .build(&primary_server)
        .await
        .expect("create conversation");
    assert_eq!(test.config.model_provider_id, "openai");
    assert_eq!(test.session_configured.model_provider_id, "openai");
    let codex = test.codex;

    codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "parent turn".to_string(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
        })
        .await
        .unwrap();
    let _parent_complete =
        wait_for_event(&codex, |ev| matches!(ev, EventMsg::TurnComplete(_))).await;

    codex
        .submit(Op::Review {
            review_request: ReviewRequest {
                target: ReviewTarget::Custom {
                    instructions: "provider-specific review".to_string(),
                },
                user_facing_hint: None,
            },
        })
        .await
        .unwrap();

    let _entered = wait_for_event(&codex, |ev| matches!(ev, EventMsg::EnteredReviewMode(_))).await;
    let _closed = wait_for_event(&codex, |ev| {
        matches!(
            ev,
            EventMsg::ExitedReviewMode(ExitedReviewModeEvent {
                review_output: None,
                ..
            })
        )
    })
    .await;
    let _review_complete =
        wait_for_event(&codex, |ev| matches!(ev, EventMsg::TurnComplete(_))).await;

    let primary_request = primary_log.single_request();
    assert_eq!(primary_request.path(), "/v1/responses");
    assert_eq!(
        primary_request.body_json()["model"].as_str(),
        Some("gpt-4.1")
    );

    let secondary_request = secondary_log.single_request();
    assert_eq!(secondary_request.path(), "/v1/responses");
    assert_eq!(
        secondary_request.body_json()["model"].as_str(),
        Some("external-reviewer")
    );
    assert_eq!(
        secondary_request.header("authorization").as_deref(),
        Some("Bearer secondary-key")
    );
    assert_ne!(
        secondary_request.header("authorization").as_deref(),
        Some("Bearer Access Token")
    );

    let _codex_home_guard = codex_home;
    primary_server.verify().await;
    secondary_server.verify().await;
}

#[serial(env_vars)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn review_model_uses_overlay_model_provider_when_review_provider_unset() {
    let sse_raw = r#"[
        {"type":"response.completed", "response": {"id": "__ID__"}}
    ]"#;
    let (primary_server, primary_log) = start_responses_server_with_sse(sse_raw, 1).await;
    let (secondary_server, secondary_log) = start_responses_server_with_sse(sse_raw, 1).await;
    let _env_guard = EnvGuard::set(REVIEW_PROVIDER_API_KEY_ENV, "secondary-key");
    let secondary_base_url = format!("{}/v1", secondary_server.uri());

    let codex_home = Arc::new(TempDir::new().unwrap());
    let mut builder = test_codex()
        .with_auth(CodexAuth::create_dummy_chatgpt_auth_for_testing())
        .with_home(codex_home.clone())
        .with_config(move |cfg| {
            cfg.model = Some("gpt-4.1".to_string());
            cfg.review_model = Some("external-reviewer".to_string());
            cfg.review_model_provider = None;
            cfg.model_providers.insert(
                "external-review".to_string(),
                ModelProviderInfo {
                    name: "External Review".to_string(),
                    base_url: Some(secondary_base_url),
                    env_key: Some(REVIEW_PROVIDER_API_KEY_ENV.to_string()),
                    env_key_instructions: None,
                    experimental_bearer_token: None,
                    wire_api: WireApi::Responses,
                    query_params: None,
                    http_headers: None,
                    env_http_headers: None,
                    request_max_retries: Some(0),
                    stream_max_retries: Some(0),
                    stream_idle_timeout_ms: Some(5_000),
                    requires_openai_auth: false,
                    supports_websockets: false,
                },
            );
            cfg.model_overlay = Some(ModelOverlay {
                models: vec![ModelOverlayEntry {
                    slug: "external-reviewer".to_string(),
                    model_provider: Some("external-review".to_string()),
                    patch: ModelInfoPatch {
                        context_window: Some(Some(1_048_576)),
                        auto_compact_token_limit: Some(Some(960_000)),
                        ..Default::default()
                    },
                    final_instruction_override: None,
                }],
                ..Default::default()
            });
        });
    let test = builder
        .build(&primary_server)
        .await
        .expect("create conversation");
    assert_eq!(test.config.review_model_provider, None);
    assert_eq!(test.session_configured.model_provider_id, "openai");
    let codex = test.codex;

    codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "parent turn".to_string(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
        })
        .await
        .unwrap();
    let _parent_complete =
        wait_for_event(&codex, |ev| matches!(ev, EventMsg::TurnComplete(_))).await;

    codex
        .submit(Op::Review {
            review_request: ReviewRequest {
                target: ReviewTarget::Custom {
                    instructions: "provider-specific review".to_string(),
                },
                user_facing_hint: None,
            },
        })
        .await
        .unwrap();

    let _entered = wait_for_event(&codex, |ev| matches!(ev, EventMsg::EnteredReviewMode(_))).await;
    let _closed = wait_for_event(&codex, |ev| {
        matches!(
            ev,
            EventMsg::ExitedReviewMode(ExitedReviewModeEvent {
                review_output: None,
                ..
            })
        )
    })
    .await;
    let _review_complete =
        wait_for_event(&codex, |ev| matches!(ev, EventMsg::TurnComplete(_))).await;

    let primary_request = primary_log.single_request();
    assert_eq!(
        primary_request.body_json()["model"].as_str(),
        Some("gpt-4.1")
    );

    let secondary_request = secondary_log.single_request();
    assert_eq!(
        secondary_request.body_json()["model"].as_str(),
        Some("external-reviewer")
    );
    assert_eq!(
        secondary_request.header("authorization").as_deref(),
        Some("Bearer secondary-key")
    );

    let _codex_home_guard = codex_home;
    primary_server.verify().await;
    secondary_server.verify().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unknown_review_model_provider_surfaces_error_event() {
    let server = MockServer::start().await;
    let codex_home = Arc::new(TempDir::new().unwrap());
    let codex = new_conversation_for_server(&server, codex_home.clone(), |cfg| {
        cfg.review_model = Some("external-reviewer".to_string());
        cfg.review_model_provider = Some("missing-review-provider".to_string());
    })
    .await;

    codex
        .submit(Op::Review {
            review_request: ReviewRequest {
                target: ReviewTarget::Custom {
                    instructions: "provider error".to_string(),
                },
                user_facing_hint: None,
            },
        })
        .await
        .unwrap();

    let error = wait_for_event(&codex, |ev| {
        matches!(
            ev,
            EventMsg::Error(err)
                if err.message.contains("Model provider `missing-review-provider` not found")
        )
    })
    .await;
    assert!(matches!(error, EventMsg::Error(_)));
    let _closed = wait_for_event(&codex, |ev| {
        matches!(
            ev,
            EventMsg::ExitedReviewMode(ExitedReviewModeEvent {
                review_output: None,
                ..
            })
        )
    })
    .await;

    let _codex_home_guard = codex_home;
}

/// Ensure that when `review_model` is not set in the config, the review request
/// uses the session model.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn review_uses_session_model_when_review_model_unset() {
    skip_if_no_network!();

    // Minimal stream: just a completed event
    let sse_raw = r#"[
        {"type":"response.completed", "response": {"id": "__ID__"}}
    ]"#;
    let (server, request_log) = start_responses_server_with_sse(sse_raw, 1).await;
    let codex_home = Arc::new(TempDir::new().unwrap());
    let codex = new_conversation_for_server(&server, codex_home.clone(), |cfg| {
        cfg.model = Some("gpt-4.1".to_string());
        cfg.review_model = None;
    })
    .await;

    codex
        .submit(Op::Review {
            review_request: ReviewRequest {
                target: ReviewTarget::Custom {
                    instructions: "use session model".to_string(),
                },
                user_facing_hint: None,
            },
        })
        .await
        .unwrap();

    let _entered = wait_for_event(&codex, |ev| matches!(ev, EventMsg::EnteredReviewMode(_))).await;
    let _closed = wait_for_event(&codex, |ev| {
        matches!(
            ev,
            EventMsg::ExitedReviewMode(ExitedReviewModeEvent {
                review_output: None,
                ..
            })
        )
    })
    .await;
    let _complete = wait_for_event(&codex, |ev| matches!(ev, EventMsg::TurnComplete(_))).await;

    let request = request_log.single_request();
    assert_eq!(request.path(), "/v1/responses");
    let body = request.body_json();
    assert_eq!(body["model"].as_str().unwrap(), "gpt-4.1");

    let _codex_home_guard = codex_home;
    server.verify().await;
}

/// When a review session begins, it must not prepend prior chat history from
/// the parent session. The request `input` should contain only the review
/// prompt from the user.
// Windows CI only: bump to 4 workers to prevent SSE/event starvation and test timeouts.
#[cfg_attr(windows, tokio::test(flavor = "multi_thread", worker_threads = 4))]
#[cfg_attr(not(windows), tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn review_input_isolated_from_parent_history() {
    skip_if_no_network!();

    // Mock server for the single review request
    let sse_raw = r#"[
        {"type":"response.completed", "response": {"id": "__ID__"}}
    ]"#;
    let (server, request_log) = start_responses_server_with_sse(sse_raw, 1).await;

    // Seed a parent session history via resume file with both user + assistant items.
    let codex_home = Arc::new(TempDir::new().unwrap());

    let session_file = codex_home.path().join("resume.jsonl");
    {
        let mut f = tokio::fs::File::create(&session_file).await.unwrap();
        let convo_id = Uuid::new_v4();
        // Proper session_meta line (enveloped) with a conversation id
        let meta_line = serde_json::json!({
            "timestamp": "2024-01-01T00:00:00.000Z",
            "type": "session_meta",
            "payload": {
                "id": convo_id,
                "timestamp": "2024-01-01T00:00:00Z",
                "cwd": ".",
                "originator": "test_originator",
                "cli_version": "test_version",
                "model_provider": "test-provider"
            }
        });
        f.write_all(format!("{meta_line}\n").as_bytes())
            .await
            .unwrap();

        // Prior user message (enveloped response_item)
        let user = codex_protocol::models::ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![codex_protocol::models::ContentItem::InputText {
                text: "parent: earlier user message".to_string(),
            }],
            end_turn: None,
            phase: None,
        };
        let user_json = serde_json::to_value(&user).unwrap();
        let user_line = serde_json::json!({
            "timestamp": "2024-01-01T00:00:01.000Z",
            "type": "response_item",
            "payload": user_json
        });
        f.write_all(format!("{user_line}\n").as_bytes())
            .await
            .unwrap();

        // Prior assistant message (enveloped response_item)
        let assistant = codex_protocol::models::ResponseItem::Message {
            id: None,
            role: "assistant".to_string(),
            content: vec![codex_protocol::models::ContentItem::OutputText {
                text: "parent: assistant reply".to_string(),
            }],
            end_turn: None,
            phase: None,
        };
        let assistant_json = serde_json::to_value(&assistant).unwrap();
        let assistant_line = serde_json::json!({
            "timestamp": "2024-01-01T00:00:02.000Z",
            "type": "response_item",
            "payload": assistant_json
        });
        f.write_all(format!("{assistant_line}\n").as_bytes())
            .await
            .unwrap();
    }
    let codex =
        resume_conversation_for_server(&server, codex_home.clone(), session_file.clone(), |_| {})
            .await;

    // Submit review request; it must start fresh (no parent history in `input`).
    let review_prompt = "Please review only this".to_string();
    codex
        .submit(Op::Review {
            review_request: ReviewRequest {
                target: ReviewTarget::Custom {
                    instructions: review_prompt.clone(),
                },
                user_facing_hint: None,
            },
        })
        .await
        .unwrap();

    let _entered = wait_for_event(&codex, |ev| matches!(ev, EventMsg::EnteredReviewMode(_))).await;
    let _closed = wait_for_event(&codex, |ev| {
        matches!(
            ev,
            EventMsg::ExitedReviewMode(ExitedReviewModeEvent {
                review_output: None,
                ..
            })
        )
    })
    .await;
    let _complete = wait_for_event(&codex, |ev| matches!(ev, EventMsg::TurnComplete(_))).await;

    // Assert the request `input` contains the environment context followed by the user review prompt.
    let request = request_log.single_request();
    assert_eq!(request.path(), "/v1/responses");
    let body = request.body_json();
    let input = body["input"].as_array().expect("input array");
    assert!(
        input.len() >= 2,
        "expected at least environment context and review prompt"
    );

    let env_text = input
        .iter()
        .filter_map(|msg| msg["content"][0]["text"].as_str())
        .find(|text| text.starts_with(ENVIRONMENT_CONTEXT_OPEN_TAG))
        .expect("env text");
    assert!(
        env_text.contains("<cwd>"),
        "environment context should include cwd"
    );

    let review_text = input
        .iter()
        .filter_map(|msg| msg["content"][0]["text"].as_str())
        .find(|text| *text == review_prompt)
        .expect("review prompt text");
    assert_eq!(
        review_text, review_prompt,
        "user message should only contain the raw review prompt"
    );

    // Ensure the REVIEW_PROMPT rubric is sent via instructions.
    let instructions = body["instructions"].as_str().expect("instructions string");
    assert_eq!(instructions, REVIEW_PROMPT);

    // Also verify that a user interruption note was recorded in the rollout.
    let path = codex.rollout_path().expect("rollout path");
    let text = std::fs::read_to_string(&path).expect("read rollout file");
    let mut saw_interruption_message = false;
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let v: serde_json::Value = serde_json::from_str(line).expect("jsonl line");
        let rl: RolloutLine = serde_json::from_value(v).expect("rollout line");
        if let RolloutItem::ResponseItem(ResponseItem::Message { role, content, .. }) = rl.item
            && role == "user"
        {
            for c in content {
                if let ContentItem::InputText { text } = c
                    && text.contains("User initiated a review task, but was interrupted.")
                {
                    saw_interruption_message = true;
                    break;
                }
            }
        }
        if saw_interruption_message {
            break;
        }
    }
    assert!(
        saw_interruption_message,
        "expected user interruption message in rollout"
    );

    let _codex_home_guard = codex_home;
    server.verify().await;
}

/// After a review thread finishes, its conversation should be visible in the
/// parent session so later turns can reference the results.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn review_history_surfaces_in_parent_session() {
    skip_if_no_network!();

    // Respond to both the review request and the subsequent parent request.
    let sse_raw = r#"[
        {"type":"response.output_item.done", "item":{
            "type":"message", "role":"assistant",
            "content":[{"type":"output_text","text":"review assistant output"}]
        }},
        {"type":"response.completed", "response": {"id": "__ID__"}}
    ]"#;
    let (server, request_log) = start_responses_server_with_sse(sse_raw, 2).await;
    let codex_home = Arc::new(TempDir::new().unwrap());
    let codex = new_conversation_for_server(&server, codex_home.clone(), |_| {}).await;

    // 1) Run a review turn that produces an assistant message (isolated in child).
    codex
        .submit(Op::Review {
            review_request: ReviewRequest {
                target: ReviewTarget::Custom {
                    instructions: "Start a review".to_string(),
                },
                user_facing_hint: None,
            },
        })
        .await
        .unwrap();
    let _entered = wait_for_event(&codex, |ev| matches!(ev, EventMsg::EnteredReviewMode(_))).await;
    let _closed = wait_for_event(&codex, |ev| {
        matches!(
            ev,
            EventMsg::ExitedReviewMode(ExitedReviewModeEvent {
                review_output: Some(_),
                ..
            })
        )
    })
    .await;
    let _complete = wait_for_event(&codex, |ev| matches!(ev, EventMsg::TurnComplete(_))).await;

    // 2) Continue in the parent session; request input must not include any review items.
    let followup = "back to parent".to_string();
    codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: followup.clone(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
        })
        .await
        .unwrap();
    let _complete = wait_for_event(&codex, |ev| matches!(ev, EventMsg::TurnComplete(_))).await;

    // Inspect the second request (parent turn) input contents.
    // Parent turns include session initial messages (user_instructions, environment_context).
    // Critically, no messages from the review thread should appear.
    let requests = request_log.requests();
    assert_eq!(requests.len(), 2);
    for request in &requests {
        assert_eq!(request.path(), "/v1/responses");
    }
    let body = requests[1].body_json();
    let input = body["input"].as_array().expect("input array");

    // Must include the followup as the last item for this turn
    let last = input.last().expect("at least one item in input");
    assert_eq!(last["role"].as_str().unwrap(), "user");
    let last_text = last["content"][0]["text"].as_str().unwrap();
    assert_eq!(last_text, followup);

    // Ensure review-thread content is present for downstream turns.
    let contains_review_rollout_user = input.iter().any(|msg| {
        msg["content"][0]["text"]
            .as_str()
            .unwrap_or_default()
            .contains("User initiated a review task.")
    });
    let contains_review_assistant = input.iter().any(|msg| {
        msg["content"][0]["text"]
            .as_str()
            .unwrap_or_default()
            .contains("review assistant output")
    });
    assert!(
        contains_review_rollout_user,
        "review rollout user message missing from parent turn input"
    );
    assert!(
        contains_review_assistant,
        "review assistant output missing from parent turn input"
    );

    let _codex_home_guard = codex_home;
    server.verify().await;
}

/// `/review` should use the session's current cwd (including runtime overrides)
/// when resolving base-branch review prompts (merge-base computation).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn review_uses_overridden_cwd_for_base_branch_merge_base() {
    skip_if_no_network!();

    let sse_raw = r#"[{"type":"response.completed", "response": {"id": "__ID__"}}]"#;
    let (server, request_log) = start_responses_server_with_sse(sse_raw, 1).await;

    let initial_cwd = TempDir::new().unwrap();

    let repo_dir = TempDir::new().unwrap();
    let repo_path = repo_dir.path();

    fn run_git(repo_path: &std::path::Path, args: &[&str]) {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(repo_path)
            .args(args)
            .output()
            .expect("spawn git");
        assert!(
            output.status.success(),
            "git {:?} failed: stdout={:?} stderr={:?}",
            args,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    run_git(repo_path, &["init", "-b", "main"]);
    run_git(repo_path, &["config", "user.email", "test@example.com"]);
    run_git(repo_path, &["config", "user.name", "Test User"]);
    std::fs::write(repo_path.join("file.txt"), "hello\n").unwrap();
    run_git(repo_path, &["add", "."]);
    run_git(repo_path, &["commit", "-m", "initial"]);

    let head_sha = std::process::Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .args(["rev-parse", "HEAD"])
        .output()
        .expect("rev-parse HEAD");
    assert!(head_sha.status.success());
    let head_sha = String::from_utf8(head_sha.stdout)
        .expect("utf8 sha")
        .trim()
        .to_string();

    let codex_home = Arc::new(TempDir::new().unwrap());
    let initial_cwd_path = initial_cwd.path().to_path_buf();
    let codex = new_conversation_for_server(&server, codex_home.clone(), move |config| {
        config.cwd = initial_cwd_path;
    })
    .await;

    codex
        .submit(Op::OverrideTurnContext {
            cwd: Some(repo_path.to_path_buf()),
            approval_policy: None,
            sandbox_policy: None,
            windows_sandbox_level: None,
            model: None,
            effort: None,
            summary: None,
            collaboration_mode: None,
            personality: None,
            service_tier: None,
        })
        .await
        .unwrap();

    codex
        .submit(Op::Review {
            review_request: ReviewRequest {
                target: ReviewTarget::BaseBranch {
                    branch: "main".to_string(),
                },
                user_facing_hint: None,
            },
        })
        .await
        .unwrap();

    let _entered = wait_for_event(&codex, |ev| matches!(ev, EventMsg::EnteredReviewMode(_))).await;
    let _complete = wait_for_event(&codex, |ev| matches!(ev, EventMsg::TurnComplete(_))).await;

    let requests = request_log.requests();
    assert_eq!(requests.len(), 1);
    for request in &requests {
        assert_eq!(request.path(), "/v1/responses");
    }
    let body = requests[0].body_json();
    let input = body["input"].as_array().expect("input array");

    let saw_merge_base_sha = input
        .iter()
        .filter_map(|msg| msg["content"][0]["text"].as_str())
        .any(|text| text.contains(&head_sha));
    assert!(
        saw_merge_base_sha,
        "expected review prompt to include merge-base sha {head_sha}"
    );

    let _codex_home_guard = codex_home;
    server.verify().await;
}

/// Start a mock Responses API server and mount the given SSE stream body.
async fn start_responses_server_with_sse(
    sse_raw: &str,
    expected_requests: usize,
) -> (MockServer, ResponseMock) {
    let server = MockServer::start().await;
    let sse = load_sse_fixture_with_id_from_str(sse_raw, &Uuid::new_v4().to_string());
    let responses = vec![sse; expected_requests];
    let request_log = mount_sse_sequence(&server, responses).await;
    (server, request_log)
}

/// Create a conversation configured to talk to the provided mock server.
#[expect(clippy::expect_used)]
async fn new_conversation_for_server<F>(
    server: &MockServer,
    codex_home: Arc<TempDir>,
    mutator: F,
) -> Arc<CodexThread>
where
    F: FnOnce(&mut Config) + Send + 'static,
{
    let base_url = format!("{}/v1", server.uri());
    let mut builder = test_codex()
        .with_home(codex_home)
        .with_config(move |config| {
            config.model_provider.base_url = Some(base_url.clone());
            mutator(config);
        });
    builder
        .build(server)
        .await
        .expect("create conversation")
        .codex
}

/// Create a conversation resuming from a rollout file, configured to talk to the provided mock server.
#[expect(clippy::expect_used)]
async fn resume_conversation_for_server<F>(
    server: &MockServer,
    codex_home: Arc<TempDir>,
    resume_path: std::path::PathBuf,
    mutator: F,
) -> Arc<CodexThread>
where
    F: FnOnce(&mut Config) + Send + 'static,
{
    let base_url = format!("{}/v1", server.uri());
    let mut builder = test_codex()
        .with_home(codex_home.clone())
        .with_config(move |config| {
            config.model_provider.base_url = Some(base_url.clone());
            mutator(config);
        });
    builder
        .resume(server, codex_home, resume_path)
        .await
        .expect("resume conversation")
        .codex
}

#[expect(clippy::expect_used)]
fn read_rollout_lines(path: &std::path::Path) -> Vec<RolloutLine> {
    std::fs::read_to_string(path)
        .expect("read rollout file")
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).expect("rollout line"))
        .collect()
}

#[expect(clippy::expect_used)]
fn read_single_delegate_rollout(
    codex_home: &std::path::Path,
    parent_path: &std::path::Path,
) -> Vec<RolloutLine> {
    let mut paths = Vec::new();
    collect_rollout_paths(&codex_home.join("sessions"), &mut paths);
    paths.retain(|path| path != parent_path);
    assert_eq!(1, paths.len(), "expected one delegate rollout");
    read_rollout_lines(paths.first().expect("delegate rollout path"))
}

#[expect(clippy::expect_used)]
fn collect_rollout_paths(dir: &std::path::Path, paths: &mut Vec<PathBuf>) {
    if !dir.exists() {
        return;
    }
    for entry in std::fs::read_dir(dir).expect("read rollout directory") {
        let entry = entry.expect("rollout directory entry");
        let path = entry.path();
        if path.is_dir() {
            collect_rollout_paths(&path, paths);
        } else if path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("rollout-") && name.ends_with(".jsonl"))
        {
            paths.push(path);
        }
    }
}

fn rollout_event_count(lines: &[RolloutLine], predicate: impl Fn(&EventMsg) -> bool) -> usize {
    lines
        .iter()
        .filter(|line| match &line.item {
            RolloutItem::EventMsg(event) => predicate(event),
            RolloutItem::SessionMeta(_)
            | RolloutItem::ResponseItem(_)
            | RolloutItem::Compacted(_)
            | RolloutItem::TurnContext(_) => false,
        })
        .count()
}

fn rollout_has_agent_message(lines: &[RolloutLine], text: &str) -> bool {
    lines.iter().any(|line| {
        matches!(
            &line.item,
            RolloutItem::EventMsg(EventMsg::AgentMessage(event)) if event.message == text
        )
    })
}

#[expect(clippy::expect_used)]
fn adjacent_duplicate_rollout_items(lines: &[RolloutLine]) -> usize {
    lines
        .windows(2)
        .filter(|pair| {
            let previous = serde_json::to_value(&pair[0].item).expect("serialize rollout item");
            let current = serde_json::to_value(&pair[1].item).expect("serialize rollout item");
            previous == current
        })
        .count()
}

struct EnvGuard {
    key: &'static str,
    original: Option<OsString>,
}

impl EnvGuard {
    fn set(key: &'static str, value: &str) -> Self {
        let original = std::env::var_os(key);
        // SAFETY: tests that use this guard run under the shared `env_vars` serial group.
        unsafe {
            std::env::set_var(key, value);
        }
        Self { key, original }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        // SAFETY: the guard restores the original value before the serial test exits.
        unsafe {
            match &self.original {
                Some(value) => std::env::set_var(self.key, value),
                None => std::env::remove_var(self.key),
            }
        }
    }
}
