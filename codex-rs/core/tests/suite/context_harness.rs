#![cfg(unix)]

use anyhow::Context;
use anyhow::Result;
use codex_core::features::Feature;
use codex_core::protocol::AskForApproval;
use codex_core::protocol::EventMsg;
use codex_core::protocol::Op;
use codex_core::protocol::SandboxPolicy;
use codex_protocol::ThreadId;
use codex_protocol::protocol::ProgressTraceState;
use codex_protocol::user_input::UserInput;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_completed_with_tokens;
use core_test_support::responses::ev_function_call;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_function_call_agent_response;
use core_test_support::responses::mount_sse_once;
use core_test_support::responses::mount_sse_once_match;
use core_test_support::responses::mount_sse_sequence;
use core_test_support::responses::sse;
use core_test_support::responses::sse_response;
use core_test_support::responses::start_mock_server;
use core_test_support::skip_if_no_network_unless_localhost;
use core_test_support::streaming_sse::StreamingSseChunk;
use core_test_support::streaming_sse::start_streaming_sse_server;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use core_test_support::wait_for_event_with_timeout;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::time::Duration;
use test_case::test_case;
use wiremock::Request;
use wiremock::matchers::method;
use wiremock::matchers::path_regex;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stream_disconnect_rejects_undispatched_call_before_retry() -> Result<()> {
    skip_if_no_network_unless_localhost!(Ok(()));
    let args = json!({"cmd":"printf x >> invocation-counter", "shell":"/bin/sh", "login":false});
    let (server, _) = start_streaming_sse_server(vec![
        vec![StreamingSseChunk {
            gate: None,
            body: sse(vec![
                ev_response_created("failed-attempt"),
                ev_function_call("unaccepted", "exec_command", &args.to_string()),
            ]),
        }],
        vec![StreamingSseChunk {
            gate: None,
            body: sse(vec![
                ev_response_created("retry"),
                ev_function_call("accepted", "exec_command", &args.to_string()),
                ev_completed("retry"),
            ]),
        }],
        vec![StreamingSseChunk {
            gate: None,
            body: sse(vec![
                ev_assistant_message("finish", "Done."),
                ev_completed("finish"),
            ]),
        }],
    ])
    .await;
    let test = test_codex()
        .with_config(|config| {
            config.features.enable(Feature::UnifiedExec);
            config.features.disable(Feature::ResponsesWebsockets);
            config.features.disable(Feature::EnableRequestCompression);
            config.features.disable(Feature::RecoverableExecOutput);
            config.features.disable(Feature::ExecJobCompletion);
        })
        .build_with_streaming_server(&server)
        .await?;
    test.submit_turn("Run the command once.").await?;
    let requests = server
        .requests()
        .await
        .into_iter()
        .map(|body| serde_json::from_slice::<serde_json::Value>(&body))
        .collect::<serde_json::Result<Vec<_>>>()?;
    assert_eq!(requests.len(), 3);
    let original = requests[0]["input"]
        .as_array()
        .context("missing original input")?;
    let retry = requests[1]["input"]
        .as_array()
        .context("missing retry input")?;
    assert!(retry.starts_with(original));
    let pair = &retry[original.len()..];
    assert_eq!(pair.len(), 2);
    assert_eq!(pair[0]["type"], "function_call");
    assert_eq!(pair[1]["type"], "function_call_output");
    assert_eq!(pair[0]["call_id"], "unaccepted");
    assert_eq!(pair[1]["call_id"], "unaccepted");
    assert!(
        pair[1]["output"]
            .as_str()
            .is_some_and(|output| output.contains("not executed"))
    );
    let final_input = requests[2]["input"]
        .as_array()
        .context("missing final input")?;
    assert!(
        final_input
            .iter()
            .any(|item| item["type"] == "function_call_output" && item["call_id"] == "accepted")
    );
    assert_eq!(
        std::fs::read(test.cwd.path().join("invocation-counter"))?,
        b"x"
    );
    server.shutdown().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recover_command_output_from_replayed_receipt() -> Result<()> {
    skip_if_no_network_unless_localhost!(Ok(()));
    let server = start_mock_server().await;
    let enable_capture = |config: &mut codex_core::config::Config| {
        config.features.enable(Feature::UnifiedExec);
        config.features.enable(Feature::RecoverableExecOutput);
        config.features.disable(Feature::RemoteCompaction);
    };
    let test = test_codex()
        .with_config(enable_capture)
        .build(&server)
        .await?;
    let run = mount_function_call_agent_response(
        &server,
        "run",
        &json!({
            "cmd": "printf x >> once; dd if=/dev/zero bs=131072 count=9 2>/dev/null | tr '\\000' A; printf 'MIDDLE\\377'; dd if=/dev/zero bs=131072 count=9 2>/dev/null | tr '\\000' Z; printf diagnostic >&2; exit 7",
            "shell": "/bin/sh",
            "login": false,
            "yield_time_ms": 1000,
            "max_output_tokens": 20,
        }).to_string(),
        "exec_command",
    )
    .await;
    test.submit_turn("Run the one-shot fixture.").await?;
    let request = run.completion.single_request();
    let output = request
        .function_call_output_text("run")
        .context("missing run result")?;
    assert!(output.contains("Process exited with code 7"), "{output}");
    assert!(output.contains("(complete; read_exec_output)"), "{output}");
    assert!(!output.contains("MIDDLE"), "{output}");
    let locator = output
        .lines()
        .find_map(|line| line.strip_prefix("Output artifact "))
        .and_then(|line| line.split_whitespace().next())
        .context("missing artifact locator")?;
    let rollout_path = test.codex.rollout_path().context("no rollout")?;
    test.codex.submit(Op::Shutdown {}).await?;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::ShutdownComplete)
    })
    .await;

    let resumed = test_codex()
        .with_config(enable_capture)
        .resume(&server, test.home.clone(), rollout_path)
        .await?;
    let read = mount_function_call_agent_response(
        &server,
        "read",
        &json!({
            "artifact": locator,
            "stream": "stdout",
            "offset": 9 * 131072,
            "limit": 7,
        })
        .to_string(),
        "read_exec_output",
    )
    .await;
    resumed
        .submit_turn("Read the original middle marker.")
        .await?;
    let request = read.completion.single_request();
    assert_eq!(
        request.function_call_output_text("run").as_deref(),
        Some(output.as_str())
    );
    let recovered = request
        .function_call_output_text("read")
        .context("missing range result")?;
    assert_eq!(
        recovered.lines().last(),
        Some(r#"{"base64":"TUlERExF/w=="}"#)
    );
    let compact = mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("compact"),
            ev_assistant_message("summary", "The original output locator is available."),
            ev_completed("compact"),
        ]),
    )
    .await;
    resumed.codex.submit(Op::Compact).await?;
    wait_for_event(&resumed.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    assert!(compact.single_request().has_function_call("run"));
    let after_compaction = mount_function_call_agent_response(
        &server,
        "read_again",
        &json!({"artifact": locator, "stream": "stdout", "offset": 9 * 131072, "limit": 7})
            .to_string(),
        "read_exec_output",
    )
    .await;
    resumed
        .submit_turn("Recover the known range after compaction.")
        .await?;
    let request = after_compaction.completion.single_request();
    assert!(!request.has_function_call("run"));
    assert_eq!(
        request.function_call_output_text("read_again").as_deref(),
        Some(recovered.as_str())
    );
    let path = resumed.codex.rollout_path().context("source rollout")?;
    let fork = resumed
        .thread_manager
        .fork_thread(usize::MAX, resumed.config.clone(), path.clone())
        .await?;
    let args = json!({"artifact":locator,"stream":"stdout","offset":0,"limit":1}).to_string();
    let denied =
        mount_function_call_agent_response(&server, "fork-read", &args, "read_exec_output").await;
    fork.thread
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "Try the inherited locator.".into(),
                text_elements: vec![],
            }],
            final_output_json_schema: None,
        })
        .await?;
    wait_for_event(&fork.thread, |e| matches!(e, EventMsg::TurnComplete(_))).await;
    assert!(
        denied
            .completion
            .single_request()
            .function_call_output_text("fork-read")
            .context("fork result")?
            .contains("belongs to a different session")
    );
    resumed.codex.submit(Op::Shutdown {}).await?;
    wait_for_event(&resumed.codex, |e| matches!(e, EventMsg::ShutdownComplete)).await;
    std::fs::remove_dir_all(test.home.path().join("exec-outputs").join(locator))?;
    let reopened = test_codex()
        .with_config(enable_capture)
        .resume(&server, test.home.clone(), path)
        .await?;
    let missing =
        mount_function_call_agent_response(&server, "missing", &args, "read_exec_output").await;
    reopened.submit_turn("Read the removed artifact.").await?;
    assert!(
        missing
            .completion
            .single_request()
            .function_call_output_text("missing")
            .context("missing artifact result")?
            .contains("missing or inaccessible")
    );
    assert_eq!(std::fs::read(test.cwd.path().join("once"))?, b"x");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interrupted_initial_exec_retains_a_replayable_artifact_locator() -> Result<()> {
    skip_if_no_network_unless_localhost!(Ok(()));
    let server = start_mock_server().await;
    let config = |config: &mut codex_core::config::Config| {
        config.features.enable(Feature::UnifiedExec);
        config.features.enable(Feature::RecoverableExecOutput);
    };
    let mut builder = test_codex().with_config(config);
    let test = builder.build(&server).await?;
    let run = mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("run"),
            ev_function_call(
                "interrupted",
                "exec_command",
                &json!({
                    "cmd": "sleep 0.5; printf x >> once; printf observed; sleep 5",
                    "shell": "/bin/sh", "login": false, "yield_time_ms": 5000,
                })
                .to_string(),
            ),
            ev_completed("run"),
        ]),
    )
    .await;
    test.codex
        .submit(Op::UserTurn {
            items: vec![UserInput::Text {
                text: "Start the command.".into(),
                text_elements: vec![],
            }],
            final_output_json_schema: None,
            cwd: test.cwd.path().to_path_buf(),
            approval_policy: AskForApproval::Never,
            sandbox_policy: SandboxPolicy::DangerFullAccess,
            model: test.session_configured.model.clone(),
            effort: None,
            summary: Default::default(),
            collaboration_mode: None,
            personality: None,
            service_tier: None,
        })
        .await?;
    wait_for_event(&test.codex, |event| {
        matches!(event,
            EventMsg::ExecCommandOutputDelta(delta) if delta.call_id == "interrupted" &&
                delta.chunk.windows(b"observed".len()).any(|window| window == b"observed")
        )
    })
    .await;
    test.codex.submit(Op::Pause).await?;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnPaused(_))
    })
    .await;
    let _ = run.single_request();
    let rollout_path = test.codex.rollout_path().context("no rollout")?;
    test.codex.submit(Op::Shutdown {}).await?;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::ShutdownComplete)
    })
    .await;
    let resumed = test_codex()
        .with_config(config)
        .resume(&server, test.home.clone(), rollout_path)
        .await?;
    let inspect = mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("inspect"),
            ev_assistant_message("before-read", "output located"),
            ev_completed("inspect"),
        ]),
    )
    .await;
    resumed
        .submit_turn("Find the interrupted command output.")
        .await?;
    let request = inspect.single_request();
    assert!(request.has_function_call("interrupted"));
    let result = request
        .function_call_output_text("interrupted")
        .context("no aborted result")?;
    let locator = result
        .lines()
        .next()
        .and_then(|line| line.strip_prefix("Output artifact "))
        .and_then(|line| line.split_once(':').map(|(id, _)| id))
        .context("no artifact locator on canceled tool result")?;
    let read = mount_function_call_agent_response(
        &server,
        "read-canceled",
        &json!({"artifact": locator, "stream": "stdout", "limit": 8}).to_string(),
        "read_exec_output",
    )
    .await;
    resumed.submit_turn("Read the original output.").await?;
    let bytes = read
        .completion
        .single_request()
        .function_call_output_text("read-canceled")
        .context("no recovered bytes")?;
    assert_eq!(bytes.lines().last(), Some(r#""observed""#));
    assert_eq!(std::fs::read(test.cwd.path().join("once"))?, b"x");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn model_makes_one_empty_poll_for_a_job_past_the_old_wait_limit() -> Result<()> {
    skip_if_no_network_unless_localhost!(Ok(()));
    let server = start_mock_server().await;
    let test = test_codex()
        .with_config(|config| {
            config.features.enable(Feature::UnifiedExec);
            config.features.disable(Feature::EnableRequestCompression);
        })
        .build(&server)
        .await?;
    let _run = mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("initial"),
            ev_function_call(
                "run",
                "exec_command",
                &json!({"cmd": "sleep 32; printf ready", "yield_time_ms": 250, "shell": "/bin/sh", "login": false}).to_string(),
            ),
            ev_completed("initial"),
        ]),
    )
    .await;
    mount_empty_poll(&server, "run").await;
    let finish = mount_sse_once_match(
        &server,
        |request: &Request| output_text(request, "poll").is_some(),
        sse(vec![
            ev_response_created("finished"),
            ev_assistant_message("done", "ready"),
            ev_completed("finished"),
        ]),
    )
    .await;
    test.codex
        .submit(Op::UserTurn {
            items: vec![UserInput::Text {
                text: "Run and wait for the silent job.".into(),
                text_elements: vec![],
            }],
            final_output_json_schema: None,
            cwd: test.cwd.path().to_path_buf(),
            approval_policy: AskForApproval::Never,
            sandbox_policy: SandboxPolicy::DangerFullAccess,
            model: test.session_configured.model.clone(),
            effort: None,
            summary: Default::default(),
            collaboration_mode: None,
            personality: None,
            service_tier: None,
        })
        .await?;
    wait_for_event_with_timeout(
        &test.codex,
        |event| matches!(event, EventMsg::TurnComplete(_)),
        Duration::from_secs(45),
    )
    .await;
    let poll = finish
        .single_request()
        .function_call_output_text("poll")
        .context("no poll result")?;
    assert!(
        poll.contains("Process exited with code 0") && poll.ends_with("ready"),
        "{poll}"
    );
    assert_eq!(
        server
            .received_requests()
            .await
            .context("mock requests missing")?
            .iter()
            .filter(|request| request.url.path().ends_with("/responses"))
            .count(),
        3
    );
    Ok(())
}

async fn mount_empty_poll(server: &wiremock::MockServer, launch: &'static str) {
    wiremock::Mock::given(method("POST"))
        .and(path_regex(".*/responses$"))
        .and(move |request: &Request| {
            output_text(request, launch).is_some() && output_text(request, "poll").is_none()
        })
        .respond_with(move |request: &Request| {
            let session_id = output_text(request, launch)
                .and_then(|output| {
                    output.lines().find_map(|line| {
                        line.strip_prefix("Process running with session ID ")
                            .and_then(|id| id.parse::<u32>().ok())
                    })
                })
                .expect("run returns a live process ID");
            sse_response(sse(vec![
                ev_response_created("running"),
                ev_function_call(
                    "poll",
                    "write_stdin",
                    &json!({"session_id": session_id, "chars": "", "yield_time_ms": 60000})
                        .to_string(),
                ),
                ev_completed("running"),
            ]))
        })
        .up_to_n_times(1)
        .expect(1)
        .mount(server)
        .await;
}

fn output_text(request: &Request, call_id: &str) -> Option<String> {
    let body = request.body_json::<serde_json::Value>().ok()?;
    body.get("input")?.as_array()?.iter().find_map(|item| {
        if item["type"] == "function_call_output" && item["call_id"] == call_id {
            item["output"].as_str().map(str::to_owned)
        } else {
            None
        }
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn registered_job_delivers_once_in_the_originating_turn() -> Result<()> {
    skip_if_no_network_unless_localhost!(Ok(()));
    let server = start_mock_server().await;
    let test = test_codex()
        .with_config(|config| {
            config.features.enable(Feature::UnifiedExec);
            config.features.enable(Feature::ExecJobCompletion);
            config.features.enable(Feature::RecoverableExecOutput);
        })
        .build(&server)
        .await?;
    let mocks = mount_function_call_agent_response(
        &server,
        "background",
        &json!({
            "cmd": "sleep 0.5; printf done; exit 9",
            "shell": "/bin/sh",
            "login": false,
            "yield_time_ms": 250,
            "wait_for_completion": true,
        })
        .to_string(),
        "exec_command",
    )
    .await;
    test.submit_turn("Start and await this job.").await?;
    let request = mocks.completion.single_request();
    let acknowledgment = request
        .function_call_output_text("background")
        .context("missing call result")?;
    let id = acknowledgment
        .lines()
        .find_map(|line| line.strip_prefix("Awaiting background job "))
        .and_then(|line| line.split_once('.').map(|(id, _)| id))
        .context("missing durable job ID")?;
    let messages = request.message_input_texts("developer");
    let outcomes = messages
        .iter()
        .filter(|message| message.starts_with("Background job outcome "))
        .collect::<Vec<_>>();
    assert_eq!(outcomes.len(), 1);
    assert!(
        outcomes[0].starts_with(&format!(
            "Background job outcome {id}: exited with code 9.\nOutput artifact "
        )),
        "{outcomes:?}"
    );
    assert!(!request.has_function_call("poll"));
    assert!(
        request
            .message_input_texts("user")
            .iter()
            .all(|message| !message.contains("Background job outcome"))
    );
    let records = std::fs::read(test.codex.rollout_path().context("missing rollout path")?)?;
    let contents = String::from_utf8(records)?;
    assert_eq!(
        contents
            .matches(&format!("Background job outcome {id}:"))
            .count(),
        1
    );
    let status = mount_function_call_agent_response(
        &server,
        "inspect-job",
        &json!({"job_id": id}).to_string(),
        "exec_job_status",
    )
    .await;
    test.submit_turn("Inspect the registered job explicitly.")
        .await?;
    let status_output = status
        .completion
        .single_request()
        .function_call_output_text("inspect-job");
    assert_eq!(status_output.as_deref(), Some(outcomes[0].as_str()));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unacknowledged_registrations_do_not_exhaust_the_catalog_after_resume() -> Result<()> {
    skip_if_no_network_unless_localhost!(Ok(()));
    let server = start_mock_server().await;
    let enable = |config: &mut codex_core::config::Config| {
        config.features.enable(Feature::UnifiedExec);
        config.features.enable(Feature::ExecJobCompletion);
        config.features.enable(Feature::RecoverableExecOutput);
    };
    let test = test_codex().with_config(enable).build(&server).await?;
    let mut batches = Vec::new();
    for batch in 0..16 {
        let id = format!("batch-{batch}");
        let mut events = vec![ev_response_created(&id)];
        for slot in 0..4 {
            let call = format!("call-{batch}-{slot}");
            let mut args = json!({
                "cmd": "exit 0", "shell": "/bin/sh", "login": false,
                "yield_time_ms": 250, "wait_for_completion": true,
            });
            if slot != 0 {
                args["workdir"] = json!("nonexistent-workdir");
            }
            events.push(ev_function_call(&call, "exec_command", &args.to_string()));
        }
        events.push(ev_completed(&id));
        batches.push(sse(events));
    }
    batches.push(sse(vec![
        ev_assistant_message("done", "Done."),
        ev_completed("done"),
    ]));
    let initial = mount_sse_sequence(&server, batches).await;
    test.codex
        .submit(Op::UserTurn {
            items: vec![UserInput::Text {
                text: "Try fixture commands.".into(),
                text_elements: vec![],
            }],
            final_output_json_schema: None,
            cwd: test.cwd.path().to_path_buf(),
            approval_policy: AskForApproval::Never,
            sandbox_policy: SandboxPolicy::DangerFullAccess,
            model: test.session_configured.model.clone(),
            effort: None,
            summary: Default::default(),
            collaboration_mode: None,
            personality: None,
            service_tier: None,
        })
        .await?;
    wait_for_event_with_timeout(
        &test.codex,
        |event| matches!(event, EventMsg::TurnComplete(_)),
        Duration::from_secs(15),
    )
    .await;
    let last = initial.last_request().context("missing final request")?;
    for batch in 0..16 {
        assert!(
            last.function_call_output_text(&format!("call-{batch}-0"))
                .is_some_and(|text| text.contains("Process exited with code 0"))
        );
        for slot in 1..4 {
            let output = last
                .function_call_output_text(&format!("call-{batch}-{slot}"))
                .context("missing pre-spawn failure")?;
            assert!(
                output.contains("exec_command failed:")
                    && !output.contains("catalog is full")
                    && !output.contains("Output artifact"),
                "{output}"
            );
        }
    }
    let catalog = test
        .home
        .path()
        .join("exec-outputs")
        .join(test.session_configured.session_id.to_string());
    assert_eq!(
        std::fs::read_dir(catalog)?.count(),
        16,
        "only started commands retain artifacts"
    );
    let path = test.codex.rollout_path().context("missing rollout path")?;
    test.codex.submit(Op::Shutdown {}).await?;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::ShutdownComplete)
    })
    .await;
    let resumed = test_codex()
        .with_config(enable)
        .resume(&server, test.home.clone(), path)
        .await?;
    let registered = mount_function_call_agent_response(
        &server,
        "next-job",
        &json!({
            "cmd": "sleep 0.6; exit 13", "shell": "/bin/sh", "login": false,
            "yield_time_ms": 250, "wait_for_completion": true,
        })
        .to_string(),
        "exec_command",
    )
    .await;
    resumed.submit_turn("Wait for the next job.").await?;
    let request = registered.completion.single_request();
    assert!(
        request
            .function_call_output_text("next-job")
            .is_some_and(|text| text.contains("Awaiting background job"))
    );
    assert!(
        request
            .message_input_texts("developer")
            .iter()
            .any(|text| text.starts_with("Background job outcome ")
                && text.contains("exited with code 13"))
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[test_case(false, false; "active")]
#[test_case(true, false; "continued")]
#[test_case(true, true; "replayed")]
async fn registered_wait_preserves_the_ordinary_compaction_boundary(
    pause: bool,
    reopen: bool,
) -> Result<()> {
    skip_if_no_network_unless_localhost!(Ok(()));
    let server = start_mock_server().await;
    let enable_jobs = |config: &mut codex_core::config::Config| {
        config.features.enable(Feature::UnifiedExec);
        config.features.enable(Feature::ExecJobCompletion);
    };
    let test = test_codex().with_config(enable_jobs).build(&server).await?;
    let mock = mount_sse_sequence(&server, vec![
        sse(vec![
            ev_response_created("register"),
            ev_function_call(
                "registered",
                "exec_command",
                &json!({
                    "cmd": if pause { "sleep 5" } else { "sleep 0.6; exit 12" },
                    "shell": "/bin/sh", "login": false,
                    "yield_time_ms": 250, "wait_for_completion": true,
                }).to_string(),
            ),
            ev_completed_with_tokens("register", 1_000_000),
        ]),
        sse(vec![
            ev_assistant_message("notes", "<AUTO_COMPACT_WORK_NOTES>\nObjective: report the job\nValidated findings: exited with code 12\nNext: finish"),
            ev_completed("notes"),
        ]),
        sse(vec![
            json!({"type":"response.output_item.done","item":{"type":"compaction","encrypted_content":"retained"}}),
            ev_completed("compact"),
        ]),
        sse(vec![ev_assistant_message("final", "The job exited with code 12."), ev_completed("final")]),
    ]).await;
    test.codex
        .submit(Op::UserTurn {
            items: vec![UserInput::Text {
                text: "Start the job and report its outcome.".into(),
                text_elements: vec![],
            }],
            final_output_json_schema: None,
            cwd: test.cwd.path().to_path_buf(),
            approval_policy: AskForApproval::Never,
            sandbox_policy: SandboxPolicy::DangerFullAccess,
            model: test.session_configured.model.clone(),
            effort: None,
            summary: Default::default(),
            collaboration_mode: None,
            personality: None,
            service_tier: None,
        })
        .await?;
    if pause {
        wait_for_event(&test.codex, |event| {
            matches!(event,
                EventMsg::ProgressTrace(trace)
                    if trace.source.as_deref() == Some("exec_job_completion")
                        && trace.state == ProgressTraceState::Started
            )
        })
        .await;
        test.codex.submit(Op::Pause).await?;
        wait_for_event(&test.codex, |event| {
            matches!(event, EventMsg::TurnPaused(_))
        })
        .await;
    }
    let resumed;
    let codex = if reopen {
        let rollout_path = test.codex.rollout_path().context("missing rollout")?;
        test.codex.submit(Op::Shutdown {}).await?;
        wait_for_event(&test.codex, |event| {
            matches!(event, EventMsg::ShutdownComplete)
        })
        .await;
        resumed = test_codex()
            .with_config(enable_jobs)
            .resume(&server, test.home.clone(), rollout_path)
            .await?;
        &resumed.codex
    } else {
        &test.codex
    };
    if pause {
        codex.submit(Op::Continue).await?;
    }
    wait_for_event_with_timeout(
        codex,
        |event| matches!(event, EventMsg::TurnComplete(_)),
        Duration::from_secs(5),
    )
    .await;
    let requests = mock.requests();
    assert_eq!(requests.len(), 4);
    let capture = requests[1].message_input_texts("developer");
    let status = if pause {
        "stop was requested"
    } else {
        "exited with code 12"
    };
    assert!(
        capture
            .iter()
            .any(|text| text.contains("Background job outcome ") && text.contains(status)),
        "{capture:?}"
    );
    assert!(
        capture
            .iter()
            .any(|text| text.contains("AUTO_COMPACT_WORK_NOTES"))
    );
    assert!(
        requests[2]
            .input()
            .iter()
            .any(|item| item["type"] == "compaction_trigger")
    );
    assert!(
        requests[3]
            .input()
            .contains(&json!({"type":"compaction","encrypted_content":"retained"}))
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn paused_job_outcome_survives_interruption_of_an_explicit_continuation() -> Result<()> {
    skip_if_no_network_unless_localhost!(Ok(()));
    let server = start_mock_server().await;
    let enable_jobs = |config: &mut codex_core::config::Config| {
        config.features.enable(Feature::UnifiedExec);
        config.features.enable(Feature::ExecJobCompletion);
    };
    let test = test_codex().with_config(enable_jobs).build(&server).await?;
    let run = mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("initial"),
            ev_function_call(
                "registered",
                "exec_command",
                &json!({"cmd": "sleep 5", "shell": "/bin/sh", "login": false, "yield_time_ms": 250, "wait_for_completion": true}).to_string(),
            ),
            ev_completed("initial"),
        ]),
    ).await;
    test.codex
        .submit(Op::UserTurn {
            items: vec![UserInput::Text {
                text: "Wait until I pause.".into(),
                text_elements: vec![],
            }],
            final_output_json_schema: None,
            cwd: test.cwd.path().to_path_buf(),
            approval_policy: AskForApproval::Never,
            sandbox_policy: SandboxPolicy::DangerFullAccess,
            model: test.session_configured.model.clone(),
            effort: None,
            summary: Default::default(),
            collaboration_mode: None,
            personality: None,
            service_tier: None,
        })
        .await?;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::ProgressTrace(trace) if
            trace.source.as_deref() == Some("exec_job_completion")
                && trace.state == ProgressTraceState::Started)
    })
    .await;
    test.codex.submit(Op::Pause).await?;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnPaused(_))
    })
    .await;
    let _ = run.single_request();
    let job_directory = test
        .home
        .path()
        .join("exec-jobs")
        .join(test.session_configured.session_id.to_string());
    let job_path = std::fs::read_dir(&job_directory)?
        .next()
        .context("missing job record")??
        .path();
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let record: serde_json::Value =
                serde_json::from_slice(&tokio::fs::read(&job_path).await?)?;
            if !record["outcome"].is_null() {
                return Ok::<_, anyhow::Error>(());
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .context("stopped job outcome was not recorded")??;
    let held = job_directory.with_extension("hold");
    std::fs::rename(&job_path, &held)?;

    // The new continuation ID is already recorded, but the ledger cannot be
    // read in this attempt. Pause again before it can finish the request.
    let _held_model = core_test_support::responses::mount_response_once(
        &server,
        sse_response(sse(vec![ev_completed("not-delivered")])).set_delay(Duration::from_secs(2)),
    )
    .await;
    test.codex.submit(Op::Continue).await?;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnContinued(_))
    })
    .await;
    test.codex.submit(Op::Pause).await?;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnPaused(_))
    })
    .await;
    std::fs::rename(&held, job_path)?;
    let rollout_path = test.codex.rollout_path().context("missing rollout")?;
    test.codex.submit(Op::Shutdown {}).await?;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::ShutdownComplete)
    })
    .await;
    server.reset().await;
    let resumed = test_codex()
        .with_config(enable_jobs)
        .resume(&server, test.home.clone(), rollout_path)
        .await?;
    let finish = mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("continued"),
            ev_assistant_message("done", "stopped"),
            ev_completed("continued"),
        ]),
    )
    .await;
    resumed.codex.submit(Op::Continue).await?;
    wait_for_event(&resumed.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    let request = finish.single_request();
    let messages = request.message_input_texts("developer");
    let results = messages
        .iter()
        .filter(|m| m.starts_with("Background job outcome "))
        .collect::<Vec<_>>();
    assert_eq!(results.len(), 1);
    assert!(results[0].contains("stop was requested"), "{results:?}");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn explorer_is_fresh_and_rejects_unadvertised_side_effects() -> Result<()> {
    skip_if_no_network_unless_localhost!(Ok(()));
    let server = start_mock_server().await;
    let test = test_codex()
        .with_config(|config| {
            config.features.enable(Feature::Collab);
            config.features.disable(Feature::EnableRequestCompression);
            config.developer_instructions = Some("APPLICABLE INSTRUCTION".into());
        })
        .build(&server)
        .await?;
    let parent = |request: &Request| request_contains(request, "PARENT_ONLY");
    let child =
        move |request: &Request| !parent(request) && request_contains(request, "CHILD_ASSIGNMENT");
    let spawn = mount_sse_once_match(
        &server,
        move |request: &Request| parent(request) && output_text(request, "spawn").is_none(),
        sse(vec![
            ev_response_created("parent"),
            ev_function_call(
                "spawn",
                "spawn_agent",
                &json!({"agent_type": "explorer", "message": "CHILD_ASSIGNMENT: investigate a named file."}).to_string(),
            ),
            ev_completed("parent"),
        ]),
    ).await;
    let probe = mount_sse_once_match(
        &server,
        move |request: &Request| child(request) && output_text(request, "forged").is_none(),
        sse(vec![
            ev_response_created("explorer"),
            ev_function_call(
                "forged",
                "shell_command",
                r#"{"command":"printf no > unexpected"}"#,
            ),
            ev_completed("explorer"),
        ]),
    )
    .await;
    let child_done = mount_sse_once_match(
        &server,
        move |request: &Request| child(request) && output_text(request, "forged").is_some(),
        sse(vec![
            ev_response_created("report"),
            ev_assistant_message("report-message", "The source was not examined."),
            ev_completed("report"),
        ]),
    )
    .await;
    let parent_done = mount_sse_once_match(
        &server,
        move |request: &Request| parent(request) && output_text(request, "spawn").is_some(),
        sse(vec![
            ev_response_created("parent-completed"),
            ev_assistant_message("parent-message", "delegated"),
            ev_completed("parent-completed"),
        ]),
    )
    .await;
    test.submit_turn("PARENT_ONLY: distribute the independent research.")
        .await?;
    let spawn_output = parent_done
        .function_call_output_text("spawn")
        .context("no spawn output")?;
    let spawned: serde_json::Value = serde_json::from_str(&spawn_output)?;
    let id = ThreadId::from_string(spawned["agent_id"].as_str().context("no child ID")?)?;
    let child_thread = test.thread_manager.get_thread(id).await?;
    let done = wait_for_event(&child_thread, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    let EventMsg::TurnComplete(done) = done else {
        unreachable!()
    };
    assert_eq!(
        done.last_agent_message.as_deref(),
        Some(
            "Exploration reported; 0 evidence calls admitted, 0 partial results.\nThe source was not examined."
        )
    );
    let first = probe
        .requests()
        .into_iter()
        .find(|request| {
            request
                .message_input_texts("user")
                .iter()
                .any(|message| message.contains("CHILD_ASSIGNMENT"))
                && request.function_call_output_text("forged").is_none()
        })
        .context("missing first child request")?;
    let tools = first.body_json()["tools"]
        .as_array()
        .context("child tools missing")?
        .iter()
        .map(|tool| tool["name"].as_str().unwrap_or_default().to_string())
        .collect::<Vec<_>>();
    assert_eq!(tools, vec!["grep_files", "read_file"]);
    assert_eq!(first.body_json()["model"], "gpt-6-luna");
    let instructions = first.message_input_texts("developer").join("\n");
    assert!(
        instructions.contains("APPLICABLE INSTRUCTION")
            && instructions.contains("Research only the assigned question"),
        "{instructions}"
    );
    let rejection = child_done
        .function_call_output_text("forged")
        .context("unanswered forged call")?;
    assert!(
        rejection.contains("outside the explorer capability profile"),
        "{rejection}"
    );
    assert!(!test.cwd.path().join("unexpected").exists());
    assert!(spawn.requests().iter().any(|request| {
        let messages = request.message_input_texts("user").join("\n");
        messages.contains("PARENT_ONLY") && !messages.contains("CHILD_ASSIGNMENT")
    }));
    Ok(())
}

fn request_contains(request: &Request, marker: &str) -> bool {
    request
        .body_json::<serde_json::Value>()
        .is_ok_and(|body| body["input"].to_string().contains(marker))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rolling_back_the_next_user_keeps_the_admitted_outcome_on_replay() -> Result<()> {
    skip_if_no_network_unless_localhost!(Ok(()));
    let server = start_mock_server().await;
    let jobs = |cfg: &mut codex_core::config::Config| {
        cfg.features.enable(Feature::UnifiedExec);
        cfg.features.enable(Feature::ExecJobCompletion);
    };
    let test = test_codex().with_config(jobs).build(&server).await?;
    let mock = mount_sse_sequence(&server, vec![
        sse(vec![ev_function_call("job", "exec_command", &json!({
            "cmd":"sleep 0.6; exit 9","shell":"/bin/sh","login":false,"yield_time_ms":250,"wait_for_completion":true
        }).to_string()), ev_completed("launched")]),
        sse(vec![ev_completed("empty-answer")]),
        sse(vec![ev_assistant_message("removed","Removed."), ev_completed("newer")]),
    ]).await;
    test.submit_turn("Keep the outcome.").await?;
    test.submit_turn("Remove this user request.").await?;
    let request = mock.last_request().context("request before rollback")?;
    let input = request.input();
    assert!(
        input.windows(2).any(|pair| pair[0]["role"] == "developer"
            && pair[0]["content"]
                .to_string()
                .contains("Background job outcome ")
            && pair[1]["role"] == "user"
            && pair[1]["content"]
                .to_string()
                .contains("Remove this user request.")),
        "fixture must put the new request immediately after the outcome"
    );
    test.codex
        .submit(Op::ThreadRollback { num_turns: 1 })
        .await?;
    wait_for_event(&test.codex, |e| matches!(e, EventMsg::ThreadRolledBack(_))).await;
    let path = test.codex.rollout_path().context("rollout")?;
    test.codex.submit(Op::Shutdown {}).await?;
    wait_for_event(&test.codex, |e| matches!(e, EventMsg::ShutdownComplete)).await;
    let resumed = test_codex()
        .with_config(jobs)
        .resume(&server, test.home.clone(), path)
        .await?;
    let finish = mount_sse_once(
        &server,
        sse(vec![
            ev_assistant_message("done", "Done."),
            ev_completed("done"),
        ]),
    )
    .await;
    resumed.codex.submit(Op::Continue).await?;
    wait_for_event(&resumed.codex, |e| matches!(e, EventMsg::TurnComplete(_))).await;
    let request = finish.single_request();
    assert!(
        request
            .function_call_output_text("job")
            .context("surviving acknowledgment")?
            .contains("Awaiting background job ")
    );
    assert_eq!(
        request
            .message_input_texts("developer")
            .iter()
            .filter(
                |s| s.starts_with("Background job outcome ") && s.ends_with("exited with code 9.")
            )
            .count(),
        1
    );
    assert!(
        !request
            .message_input_texts("user")
            .iter()
            .any(|s| s.contains("Remove this user request."))
    );
    Ok(())
}

#[test_case(1; "reject_before_launch")]
#[test_case(10; "keep_session_and_exit")]
#[test_case(1024; "registered_slot_released_before_turn_ends")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tight_unified_budget_preserves_command_control(tokens: usize) -> Result<()> {
    skip_if_no_network_unless_localhost!(Ok(()));
    let server = start_mock_server().await;
    let test = test_codex()
        .with_config(move |cfg| {
            cfg.features.enable(Feature::UnifiedExec);
            cfg.features.enable(Feature::RecoverableExecOutput);
            cfg.features.enable(Feature::ExecJobCompletion);
            cfg.features.disable(Feature::EnableRequestCompression);
            cfg.tool_output_token_limit = Some(tokens);
        })
        .build(&server)
        .await?;
    let registered = tokens == 1024;
    mount_sse_once(&server, sse(vec![ev_function_call("launch","exec_command",&json!({
        "cmd":"touch launched; sleep 0.6; exit 23","shell":"/bin/sh","login":false,"yield_time_ms":250,"wait_for_completion":registered
    }).to_string()),ev_completed("launch")])).await;
    if tokens != 1 {
        mount_empty_poll(&server, "launch").await;
    }
    let finish = mount_sse_once_match(
        &server,
        move |r: &Request| output_text(r, if tokens == 1 { "launch" } else { "poll" }).is_some(),
        sse(vec![
            ev_assistant_message("done", "Done."),
            ev_completed("done"),
        ]),
    )
    .await;
    test.submit_turn("Run the command once.").await?;
    let request = finish.single_request();
    if tokens == 1 {
        assert!(!test.cwd.path().join("launched").exists());
        assert!(!test.home.path().join("exec-outputs").exists());
    } else if registered {
        assert!(
            request
                .function_call_output_text("poll")
                .context("retired slot")?
                .starts_with("write_stdin failed: Unknown process id ")
        );
        assert_eq!(
            request
                .message_input_texts("developer")
                .iter()
                .filter(|s| s.starts_with("Background job outcome "))
                .count(),
            1
        );
    } else {
        assert_eq!(
            request.function_call_output_text("poll").as_deref(),
            Some("Process exited with code 23")
        );
    }
    Ok(())
}

#[test_case(false; "steering")]
#[test_case(true; "review")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn parked_owner_cannot_sample_after_replacement(review: bool) -> Result<()> {
    skip_if_no_network_unless_localhost!(Ok(()));
    use codex_core::protocol::ReviewRequest;
    use codex_core::protocol::ReviewTarget;
    let server = start_mock_server().await;
    let test = test_codex()
        .with_config(|cfg| {
            cfg.features.enable(Feature::UnifiedExec);
            cfg.features.enable(Feature::ExecJobCompletion);
            cfg.sandbox_policy =
                codex_core::config::Constrained::allow_any(SandboxPolicy::DangerFullAccess);
        })
        .build(&server)
        .await?;
    let input = |s: &str| Op::UserInput {
        items: vec![UserInput::Text {
            text: s.into(),
            text_elements: vec![],
        }],
        final_output_json_schema: None,
    };
    let _launch = mount_sse_once(&server, sse(vec![ev_function_call("park", "exec_command", &json!({
        "cmd":"for i in $(seq 100); do test -e release && exit 7; sleep 0.1; done; exit 99","shell":"/bin/sh","login":false,"yield_time_ms":250,"wait_for_completion":true
    }).to_string()), ev_completed("parked")])).await;
    test.codex.submit(input("Await the fixture.")).await?;
    wait_for_event(&test.codex, |e| matches!(e, EventMsg::ProgressTrace(t) if t.source.as_deref() == Some("exec_job_completion") && t.state == ProgressTraceState::Started)).await;
    test.codex
        .submit(Op::ThreadRollback { num_turns: 1 })
        .await?;
    wait_for_event(&test.codex, |e| matches!(e, EventMsg::Error(err) if err.message.contains("Cannot rollback while a turn is in progress"))).await;
    let replacement = mount_sse_once(&server, sse(vec![ev_assistant_message("answer", if review {
        r#"{"findings":[],"overall_correctness":"patch is correct","overall_explanation":"No findings.","overall_confidence_score":1}"#
    } else { "New request complete." }), ev_completed("replacement")])).await;
    if review {
        test.codex
            .submit(Op::Review {
                review_request: ReviewRequest {
                    target: ReviewTarget::Custom {
                        instructions: "Independently review the fixture.".into(),
                    },
                    user_facing_hint: None,
                },
            })
            .await?;
        let end = wait_for_event(&test.codex, |e| {
            matches!(e, EventMsg::ExitedReviewMode(_) | EventMsg::Error(_))
        })
        .await;
        assert!(
            matches!(&end, EventMsg::ExitedReviewMode(e) if e.review_output.is_some()),
            "{end:?}"
        );
        wait_for_event(&test.codex, |e| matches!(e, EventMsg::TurnComplete(_))).await;
    } else {
        test.codex
            .submit(input("New request; do not wait for the job."))
            .await?;
        wait_for_event(&test.codex, |e| matches!(e, EventMsg::TurnComplete(_))).await;
    }
    let request = replacement.single_request();
    assert!(
        request
            .message_input_texts("developer")
            .iter()
            .all(|s| !s.starts_with("Background job outcome "))
    );
    let messages = request.message_input_texts("user").join("\n");
    assert!(
        messages.contains(if review {
            "Independently review the fixture."
        } else {
            "New request; do not wait for the job."
        }),
        "{messages}"
    );
    std::fs::write(test.cwd.path().join("release"), "")?;
    let dir = test
        .home
        .path()
        .join("exec-jobs")
        .join(test.session_configured.session_id.to_string());
    let path = std::fs::read_dir(dir)?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|ext| ext == "json"))
        .context("registered job")?;
    let record = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let record: serde_json::Value = serde_json::from_slice(&tokio::fs::read(&path).await?)?;
            if !record["outcome"].is_null() {
                return Ok::<_, anyhow::Error>(record);
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await??;
    assert_ne!(
        record["outcome"]["exit_code"], 99,
        "replacement must not wait for the fixture deadline"
    );
    let status = mount_function_call_agent_response(
        &server,
        "status",
        &json!({"job_id":record["id"]}).to_string(),
        "exec_job_status",
    )
    .await;
    test.submit_turn("Inspect the old job explicitly.").await?;
    let request = status.completion.single_request();
    assert!(
        request
            .function_call_output_text("status")
            .context("terminal status")?
            .starts_with("Background job outcome ")
    );
    assert!(
        request
            .message_input_texts("developer")
            .iter()
            .all(|s| !s.starts_with("Background job outcome "))
    );
    assert_eq!(
        server
            .received_requests()
            .await
            .context("mock did not record requests")?
            .iter()
            .filter(|r| r.url.path().ends_with("/responses"))
            .count(),
        4
    );
    Ok(())
}
