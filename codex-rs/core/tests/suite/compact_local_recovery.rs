use super::*;
use codex_core::config::Config;
use pretty_assertions::assert_eq;

fn local_compaction(config: &mut Config) {
    config.features.disable(Feature::RemoteCompaction);
    config.model_provider.request_max_retries = Some(0);
    config.model_provider.stream_max_retries = Some(1);
}

fn recovered_summary() -> Vec<Value> {
    vec![
        responses::ev_assistant_message("summary", "Recovered local summary."),
        responses::ev_completed("summary"),
    ]
}

fn recovered_answer() -> Vec<Value> {
    vec![
        responses::ev_assistant_message("done", "finished"),
        responses::ev_completed("done"),
    ]
}

#[test_case::test_case(false, false; "live continue")]
#[test_case::test_case(false, true; "live new input")]
#[test_case::test_case(true, false; "restart continue")]
#[test_case::test_case(true, true; "restart new input")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn local_compaction_recovers_after_uncommitted_summary_output(
    restart: bool,
    new_input: bool,
) -> Result<()> {
    skip_if_no_network_unless_localhost!(Ok(()));
    let mut streams = work_and_notes();
    for attempt in 1..=2 {
        streams.push(vec![chunk(vec![responses::ev_assistant_message(
            &format!("failed-{attempt}"),
            &format!("Discarded local summary attempt {attempt}."),
        )])]);
    }
    if !restart {
        streams.extend([
            vec![chunk(recovered_summary())],
            vec![chunk(recovered_answer())],
        ]);
    }
    let (server, _) = start_streaming_sse_server(streams).await;
    let test = test_codex()
        .with_config(local_compaction)
        .build_with_streaming_server(&server)
        .await?;
    test.codex
        .submit(user_input("unfinished local task"))
        .await?;
    wait_for_event(&test.codex, |event| matches!(event, EventMsg::Error(_))).await;
    complete(&test.codex).await;
    assert_eq!(server.requests().await.len(), 4);
    let path = test.session_configured.rollout_path.as_ref().unwrap();
    assert!(replacement_history(path).is_empty());
    let operation = || {
        if new_input {
            user_input("new input after local failure")
        } else {
            Op::Continue
        }
    };
    let bodies: Vec<Value> = if restart {
        test.codex.submit(Op::Shutdown).await?;
        wait_for_event(&test.codex, |event| {
            matches!(event, EventMsg::ShutdownComplete)
        })
        .await;
        let replay_server = responses::start_mock_server().await;
        let replay = responses::mount_sse_sequence(
            &replay_server,
            vec![
                responses::sse(recovered_summary()),
                responses::sse(recovered_answer()),
            ],
        )
        .await;
        let resumed = test_codex()
            .with_config(local_compaction)
            .resume(&replay_server, test.home.clone(), path.clone())
            .await?;
        resumed.codex.submit(operation()).await?;
        complete(&resumed.codex).await;
        replay
            .requests()
            .iter()
            .map(responses::ResponsesRequest::body_json)
            .collect()
    } else {
        test.codex.submit(operation()).await?;
        complete(&test.codex).await;
        let requests = server.requests().await;
        assert_eq!(requests.len(), 6);
        requests[4..]
            .iter()
            .map(|body| serde_json::from_slice(body).unwrap())
            .collect()
    };
    assert_eq!(bodies.len(), 2);
    assert!(bodies[0]["tools"].as_array().is_none_or(Vec::is_empty));
    assert!(!bodies[0].to_string().contains("AUTO_COMPACT_WORK_NOTES"));
    for body in &bodies {
        assert!(!body.to_string().contains("compaction_trigger"));
        assert!(!body.to_string().contains("Discarded local summary"));
    }
    let messages = user_messages(&bodies[1]);
    assert_eq!(
        messages.iter().filter(|text| text.ends_with(NOTES)).count(),
        1
    );
    assert_eq!(
        messages
            .iter()
            .filter(|text| *text == "new input after local failure")
            .count(),
        usize::from(new_input),
    );
    assert!(
        messages
            .iter()
            .any(|text| text.contains("Recovered local summary."))
    );
    let replacements = replacement_history(path);
    assert_eq!(replacements.len(), 1);
    let persisted = serde_json::to_string(&replacements)?;
    assert!(!persisted.contains("Discarded local summary"));
    assert_eq!(persisted.matches("AUTO_COMPACT_WORK_NOTES").count(), 1);
    server.shutdown().await;
    Ok(())
}
