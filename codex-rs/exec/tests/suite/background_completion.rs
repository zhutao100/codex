#![cfg(unix)]

use anyhow::Context;
use codex_core::features::Feature;
use codex_core::protocol::EventMsg;
use codex_core::protocol::Op;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_function_call;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_function_call_agent_response;
use core_test_support::responses::mount_response_sequence;
use core_test_support::responses::mount_sse_once;
use core_test_support::responses::sse;
use core_test_support::responses::sse_response;
use core_test_support::responses::start_mock_server;
use core_test_support::skip_if_no_network_unless_localhost;
use core_test_support::test_codex::test_codex;
use core_test_support::test_codex_exec::test_codex_exec;
use core_test_support::wait_for_event;
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn headless_exec_waits_for_its_registered_job() -> anyhow::Result<()> {
    skip_if_no_network_unless_localhost!(Ok(()));
    let test = test_codex_exec();
    let server = start_mock_server().await;
    let mocks = mount_function_call_agent_response(
        &server,
        "register",
        &json!({
            "cmd": "sleep 0.6; exit 8",
            "shell": "/bin/sh",
            "login": false,
            "yield_time_ms": 250,
            "wait_for_completion": true,
        })
        .to_string(),
        "exec_command",
    )
    .await;

    test.cmd_with_server(&server)
        .args([
            "--skip-git-repo-check",
            "--sandbox",
            "danger-full-access",
            "-c",
            "features.unified_exec=true",
            "-c",
            "features.exec_job_completion=true",
            "Run the fixture and wait for its outcome.",
        ])
        .timeout(Duration::from_secs(10))
        .assert()
        .success();

    let request = mocks.completion.single_request();
    assert!(
        request
            .message_input_texts("developer")
            .iter()
            .any(|text| text.starts_with("Background job outcome ")
                && text.ends_with("exited with code 8.")),
        "headless continuation must receive the retained outcome"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn abrupt_exit_before_and_after_outcome_admission() -> anyhow::Result<()> {
    skip_if_no_network_unless_localhost!(Ok(()));
    for admitted in [false, true] {
        let home = Arc::new(tempfile::tempdir()?);
        let cwd = tempfile::tempdir()?;
        let server = start_mock_server().await;
        let mut calls = vec![ev_response_created("jobs")];
        for id in ["first", "second"] {
            // A bounded shell lifetime even if the test loses its host process.
            let cmd = format!(
                "printf x >> count-{id}; printf {id}; for i in $(seq 100); do test -e release-{id} && exit 17; sleep 0.1; done; exit 99"
            );
            calls.push(ev_function_call(id, "exec_command", &json!({
                "cmd":cmd,"shell":"/bin/sh","login":false,"yield_time_ms":250,"wait_for_completion":true
            }).to_string()));
        }
        calls.push(ev_completed("jobs"));
        let mut responses = vec![sse_response(sse(calls))];
        if admitted {
            responses.push(
                sse_response(sse(vec![ev_completed("held")])).set_delay(Duration::from_secs(30)),
            );
        }
        let mock = mount_response_sequence(&server, responses).await;
        let mut host =
            tokio::process::Command::new(codex_utils_cargo_bin::cargo_bin("codex-exec")?)
                .current_dir(cwd.path())
                .env("CODEX_HOME", home.path())
                .env("OPENAI_BASE_URL", format!("{}/v1", server.uri()))
                .env(codex_core::auth::CODEX_API_KEY_ENV_VAR, "dummy")
                .args([
                    "--skip-git-repo-check",
                    "--sandbox",
                    "danger-full-access",
                    "-c",
                    "features.unified_exec=true",
                    "-c",
                    "features.exec_job_completion=true",
                    "-c",
                    "features.recoverable_exec_output=true",
                    "Run and await both fixtures.",
                ])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .kill_on_drop(true)
                .spawn()?;
        let rollout = wait_for_cut(|| {
            walkdir::WalkDir::new(home.path().join("sessions"))
                .into_iter()
                .flatten()
                .find(|entry| {
                    entry.path().extension().is_some_and(|ext| ext == "jsonl")
                        && std::fs::read_to_string(entry.path())
                            .is_ok_and(|s| s.matches("Awaiting background job ").count() == 2)
                })
                .map(walkdir::DirEntry::into_path)
        })
        .await
        .context("paired acknowledgments were not flushed")?;
        std::fs::write(cwd.path().join("release-first"), "")?;
        let first_id = wait_for_cut(|| {
            walkdir::WalkDir::new(home.path().join("exec-jobs"))
                .into_iter()
                .flatten()
                .filter(|e| e.path().extension().is_some_and(|ext| ext == "json"))
                .find_map(|e| {
                    let record: serde_json::Value =
                        serde_json::from_slice(&std::fs::read(e.path()).ok()?).ok()?;
                    (record["call_id"] == "first" && record["outcome"]["exit_code"] == 17)
                        .then(|| record["id"].as_str().map(str::to_owned))
                        .flatten()
                })
        })
        .await
        .context("first outcome not durable")?;
        if admitted {
            std::fs::write(cwd.path().join("release-second"), "")?;
            wait_for_cut(|| (mock.requests().len() == 2).then_some(()))
                .await
                .context("model did not receive admitted outcomes")?;
        }
        host.kill().await?;
        // Do not stop the session gracefully, flush the recorder or edit its files.
        std::fs::write(cwd.path().join("release-second"), "")?;
        let marker = format!("Background job outcome {first_id}:");
        assert_eq!(
            std::fs::read_to_string(&rollout)?.matches(&marker).count(),
            usize::from(admitted)
        );
        server.reset().await;
        let finish = mount_sse_once(
            &server,
            sse(vec![
                ev_assistant_message("done", "Finished"),
                ev_completed("done"),
            ]),
        )
        .await;
        let dir = cwd.path().to_owned();
        let resumed = test_codex()
            .with_config(move |cfg| {
                cfg.cwd = dir;
                cfg.features.enable(Feature::UnifiedExec);
                cfg.features.enable(Feature::ExecJobCompletion);
                cfg.features.enable(Feature::RecoverableExecOutput);
            })
            .resume(&server, home, rollout)
            .await?;
        assert!(finish.requests().is_empty());
        resumed.codex.submit(Op::Continue).await?;
        wait_for_event(&resumed.codex, |event| {
            matches!(event, EventMsg::TurnComplete(_))
        })
        .await;
        let request = finish.single_request();
        let messages = request.message_input_texts("developer");
        assert_eq!(
            messages
                .iter()
                .filter(|text| text.starts_with(&marker) && text.contains("exited with code 17."))
                .count(),
            1
        );
        assert_eq!(
            messages
                .iter()
                .filter(|text| text.starts_with("Background job not delivered "))
                .count(),
            usize::from(!admitted)
        );
        for id in ["first", "second"] {
            assert!(
                request
                    .function_call_output_text(id)
                    .context("missing acknowledged result")?
                    .contains("Awaiting background job ")
            );
            assert_eq!(std::fs::read(cwd.path().join(format!("count-{id}")))?, b"x");
        }
        let acknowledgment = request
            .function_call_output_text("second")
            .context("second job")?;
        let locator = acknowledgment
            .lines()
            .find_map(|s| s.strip_prefix("Output artifact "))
            .and_then(|s| s.split_ascii_whitespace().next())
            .context("acknowledged output locator")?;
        let read = mount_function_call_agent_response(
            &server,
            "read",
            &json!({"artifact":locator,"stream":"stdout","offset":0,"limit":6}).to_string(),
            "read_exec_output",
        )
        .await;
        resumed
            .submit_turn("Read the committed bytes without relaunching.")
            .await?;
        let bytes = read
            .completion
            .single_request()
            .function_call_output_text("read")
            .context("read after host exit")?;
        assert!(
            bytes.contains(if admitted {
                "(complete;"
            } else {
                "(interrupted;"
            }),
            "{bytes}"
        );
        assert!(bytes.contains("\"second\""), "{bytes}");
    }
    Ok(())
}

async fn wait_for_cut<T>(mut observed: impl FnMut() -> Option<T>) -> anyhow::Result<T> {
    Ok(tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(value) = observed() {
                return value;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await?)
}
