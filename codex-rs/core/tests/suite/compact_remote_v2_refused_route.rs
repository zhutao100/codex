use super::*;
use codex_protocol::protocol::CodexErrorInfo;
use pretty_assertions::assert_eq;
use sha2::Digest;
use sha2::Sha256;

#[derive(Clone, Copy)]
enum Recovery {
    Live,
    Restart,
    Manual,
    ManualAtAdmission,
}

#[test_case::test_case(false, Recovery::Live; "stream refusal live")]
#[test_case::test_case(false, Recovery::Restart; "stream refusal after restart")]
#[test_case::test_case(true, Recovery::Live; "HTTP refusal live")]
#[test_case::test_case(true, Recovery::Restart; "HTTP refusal after restart")]
#[test_case::test_case(true, Recovery::Manual; "manual compact before continue after restart")]
#[test_case::test_case(true, Recovery::ManualAtAdmission; "manual compact at the admitted prompt before the refusal ledger")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recovery_stays_on_the_provider_that_received_the_prompt(
    http: bool,
    recovery: Recovery,
) -> Result<()> {
    skip_if_no_network_unless_localhost!(Ok(()));
    const A: &str = "previous-provider-model";
    const B: &str = "destination-provider-model";
    const PROMPT: &str = "Only the selected second provider may receive this task.";
    for structured in [false, true] {
        let a = responses::start_mock_server().await;
        let b = responses::start_mock_server().await;
        let build = || {
            let first = format!("{}/v1", a.uri());
            let second = format!("{}/v1", b.uri());
            test_codex()
                .with_model(A)
                .with_config(move |cfg| configure_providers(cfg, first, second, false))
        };
        responses::mount_sse_once(&a, text_reply("a", "First task complete.", 3_000)).await;
        let refused = if http {
            responses::mount_response_once(&b, ResponseTemplate::new(400).set_body_json(json!({
                "error":{"code":"context_length_exceeded","type":"invalid_request_error","message":"Too many tokens."}
            }))).await
        } else {
            responses::mount_sse_once(
                &b,
                responses::sse_failed("b", "context_length_exceeded", "Too many tokens."),
            )
            .await
        };
        let work = build().build(&a).await?;
        let input = |text, schema| {
            if schema {
                structured_input(text)
            } else {
                user_input(text)
            }
        };
        work.codex.submit(input("First task.", !structured)).await?;
        complete(&work.codex).await;
        work.codex.submit(model_override(B)).await?;
        work.codex.submit(input(PROMPT, structured)).await?;
        let error = wait_for_event(&work.codex, |ev| matches!(ev, EventMsg::Error(_))).await;
        assert!(
            matches!(error, EventMsg::Error(e) if e.codex_error_info == Some(CodexErrorInfo::ContextWindowExceeded))
        );
        complete(&work.codex).await;
        let path = work.codex.rollout_path().context("rollout")?;
        let restart = !matches!(recovery, Recovery::Live);
        if restart {
            shutdown(&work.codex).await?;
        }
        let ledger = std::fs::read_to_string(&path)?;
        let mut cut = 0;
        let mut refusal = None;
        for line in ledger.split_inclusive('\n') {
            if let RolloutItem::TokenUsageContext {
                info,
                source_model,
                rejected_context: Some(owner),
                ..
            } = serde_json::from_str::<RolloutLine>(line)?.item
            {
                refusal = Some((info, source_model, owner));
                break;
            }
            cut += line.len();
        }
        let (usage, measured, owner) = refusal.context("durable refusal")?;
        assert_eq!(usage.total_token_usage.input_tokens, 3_000);
        assert_eq!(measured.as_deref(), Some(A));
        assert_eq!(
            (owner.model.as_str(), owner.provider.as_str()),
            (B, "destination")
        );
        assert_eq!(
            owner.endpoint_sha256,
            Some(format!("{:x}", Sha256::digest(format!("{}/v1", b.uri()))))
        );
        if matches!(recovery, Recovery::ManualAtAdmission) {
            std::fs::write(&path, &ledger[..cut])?;
        }
        let active = if restart {
            build().resume(&a, work.home.clone(), path).await?
        } else {
            work
        };
        let manual = matches!(recovery, Recovery::Manual | Recovery::ManualAtAdmission);
        let mut replies = Vec::new();
        let notes = !structured && !manual;
        if notes {
            replies.push(text_reply("notes", NOTES, 1_000));
        }
        replies.extend([
            compact_response("b-summary"),
            text_reply("done", "Done.", 2_000),
        ]);
        let recovered = responses::mount_sse_sequence(&b, replies).await;
        if manual {
            active.codex.start_compaction().await?;
            wait_for_event(&active.codex, |ev| matches!(ev, EventMsg::TurnComplete(_))).await;
            assert_eq!(responses::model_post_count(&a).await?, 1);
            assert_eq!(recovered.requests().len(), 1);
        }
        active.codex.submit(Op::Continue).await?;
        wait_for_event(&active.codex, |ev| matches!(ev, EventMsg::TurnComplete(_))).await;
        shutdown(&active.codex).await?;

        let first = a
            .received_requests()
            .await
            .context("first provider requests")?;
        assert!(!first.iter().any(|r| {
            r.body_json::<Value>()
                .ok()
                .is_some_and(|v| v["input"].to_string().contains(PROMPT))
        }));
        assert_eq!(responses::model_post_count(&a).await?, 1);
        let sent = recovered.requests();
        let helpers = 1 + usize::from(notes);
        assert_eq!(sent.len(), helpers + 1);
        let original = refused.single_request();
        for helper in &sent[..helpers] {
            let items = helper.input();
            assert_eq!(&items[..items.len() - 1], original.input().as_slice());
            assert_eq!(helper.body_json()["tools"], original.body_json()["tools"]);
            assert_eq!(helper.body_json()["model"], B);
        }
        if notes {
            assert!(
                sent[0]
                    .input()
                    .last()
                    .unwrap()
                    .to_string()
                    .contains("<AUTO_COMPACT_WORK_NOTES_REQUEST>")
            );
        }
        assert_compact_request(&sent[helpers - 1].body_json());
        let ordinary = &sent[helpers];
        assert_eq!(ordinary.body_json()["model"], B);
        assert_eq!(
            ordinary.body_json()["text"]["format"],
            original.body_json()["text"]["format"]
        );
        assert!(
            ordinary
                .input()
                .iter()
                .any(|item| item["encrypted_content"] == "b-summary")
        );
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unrelated_bad_request_does_not_create_a_capacity_barrier() -> Result<()> {
    skip_if_no_network_unless_localhost!(Ok(()));
    let server = responses::start_mock_server().await;
    let sent = responses::mount_response_sequence(
        &server,
        vec![
            ResponseTemplate::new(400).set_body_json(json!({"error":{
                "code":"invalid_value","message":"Example of another error: context_length_exceeded"
            }})),
            responses::sse_response(text_reply("ok", "Done.", 1200)),
        ],
    )
    .await;
    let active = test_codex().build(&server).await?;
    active
        .codex
        .submit(structured_input("Small ordinary task."))
        .await?;
    let error = wait_for_event(&active.codex, |ev| matches!(ev, EventMsg::Error(_))).await;
    assert!(
        matches!(error, EventMsg::Error(e) if e.codex_error_info != Some(CodexErrorInfo::ContextWindowExceeded))
    );
    complete(&active.codex).await;
    active.codex.submit(Op::Continue).await?;
    complete(&active.codex).await;
    shutdown(&active.codex).await?;
    let requests = sent.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].body_json(), requests[1].body_json());
    Ok(())
}
