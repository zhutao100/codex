//! Isolate the process-wide tracing subscriber so parallel suites cannot change callsite interest.
use anyhow::Context;
use anyhow::Result;
use codex_core::CodexAuth;
use core_test_support::responses;
use core_test_support::skip_if_no_network_unless_localhost;
use core_test_support::test_codex::test_codex;
use pretty_assertions::assert_eq;
use serde_json::json;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn compare_completed_work_and_sibling_compact_on_http_and_websocket() -> Result<()> {
    skip_if_no_network_unless_localhost!(Ok(()));
    let gate = codex_api::telemetry::PROMPT_CACHE_TRACE_ENV;
    if std::env::var_os(gate).as_deref() != Some(std::ffi::OsStr::new("1")) {
        tracing::subscriber::with_default(
            tracing_subscriber::fmt()
                .with_max_level(tracing::Level::TRACE)
                .finish(),
            || {
                assert!(!codex_api::telemetry::prompt_cache_trace_enabled());
            },
        );
        let run = std::process::Command::new(std::env::current_exe()?)
            .args([
                "--exact",
                stringify!(compare_completed_work_and_sibling_compact_on_http_and_websocket),
            ])
            .env(gate, "1")
            .output()?;
        anyhow::ensure!(
            run.status.success(),
            "opted-in fixture: {}",
            String::from_utf8_lossy(&run.stdout)
        );
        return Ok(());
    }
    let log = tempfile::NamedTempFile::new()?;
    tracing::subscriber::set_global_default(
        tracing_subscriber::fmt()
            .with_env_filter("off,codex_core::cache_trace=debug")
            .with_ansi(false)
            .with_writer(log.reopen()?)
            .finish(),
    )?;
    let files = tempfile::tempdir()?;
    let private_path = files.path().join("private-argument-sentinel");
    std::fs::write(&private_path, "private-output-sentinel")?;
    for (websocket, recover) in [(false, false), (true, false), (false, true)] {
        let mut matched = responses::ev_completed_with_tokens("notes", 28_020);
        matched["response"]["usage"]["input_tokens_details"] = json!({"cached_tokens": 3_840});
        let mut cold = responses::ev_completed_with_tokens("compact", 27_900);
        cold["response"]["usage"]["input_tokens_details"] = json!({"cached_tokens": 0});
        cold["response"]["input_transformations"] = json!([]);
        cold["response"]["prompt_cache_diagnostics"] =
            json!({"type":"cache_miss","reason":"model_changed","cache_missed_tokens":27_000});
        let streams = vec![
            vec![
                responses::ev_function_call(
                    "work",
                    "read_file",
                    &json!({"file_path":private_path}).to_string(),
                ),
                responses::ev_completed_with_tokens("work", 28_000),
            ],
            vec![
                responses::ev_assistant_message("notes-first", "Not a valid checkpoint."),
                responses::ev_completed_with_tokens("notes-first", 28_010),
            ],
            vec![
                responses::ev_assistant_message(
                    "notes",
                    "<AUTO_COMPACT_WORK_NOTES>\nprivate note text\n</AUTO_COMPACT_WORK_NOTES>",
                ),
                matched,
            ],
            vec![
                json!({"type":"response.output_item.done","item":{"type":"compaction","encrypted_content":"private ciphertext"}}),
                cold,
            ],
            vec![
                responses::ev_assistant_message("done", "done"),
                responses::ev_completed("done"),
            ],
        ];
        let offset = std::fs::metadata(log.path())?.len() as usize;
        let mut builder = test_codex()
            .with_auth(CodexAuth::from_api_key("sk-test"))
            .with_config(move |config| {
                config.model_context_window = Some(32_000);
                config.model_auto_compact_token_limit = Some(27_000);
                config.responses_transport = if websocket || recover {
                    codex_core::config::types::ResponsesTransport::Auto
                } else {
                    codex_core::config::types::ResponsesTransport::Http
                };
                config.model_provider.request_max_retries = Some(1);
                config
                    .model_overlay
                    .get_or_insert_with(Default::default)
                    .cross_model
                    .experimental_supported_tools = Some(vec!["read_file".into()]);
            });
        if websocket {
            let server = responses::start_websocket_server(vec![streams]).await;
            let test = builder.build_with_websocket_server(&server).await?;
            test.submit_turn("private task text").await?;
            let requests = server.single_connection();
            assert_eq!(requests.len(), 5);
            assert!(
                requests[1].body_json()["input"]
                    .to_string()
                    .contains("private-output-sentinel")
            );
            server.shutdown().await;
        } else {
            let server = responses::start_mock_server().await;
            let mut replies: Vec<_> = streams
                .into_iter()
                .map(responses::sse)
                .map(responses::sse_response)
                .collect();
            if recover {
                wiremock::Mock::given(wiremock::matchers::method("GET"))
                    .and(wiremock::matchers::path_regex(".*/responses$"))
                    .respond_with(wiremock::ResponseTemplate::new(426))
                    .expect(1)
                    .mount(&server)
                    .await;
                replies.insert(0, wiremock::ResponseTemplate::new(503));
            }
            let mock = responses::mount_response_sequence(&server, replies).await;
            let test = builder.build(&server).await?;
            test.submit_turn("private task text").await?;
            let requests = mock.requests();
            assert_eq!(requests.len(), 5 + usize::from(recover));
            if recover {
                assert_eq!(requests[0].body_json(), requests[1].body_json());
            }
            assert!(
                requests[1 + usize::from(recover)].body_json()["input"]
                    .to_string()
                    .contains("private-output-sentinel")
            );
        }
        let data = std::fs::read_to_string(log.path())?;
        let lines = &data[offset..];
        let comparison = |id: &str, baseline: &str| {
            lines
                .lines()
                .find(|line| {
                    line.contains("completed logical cache comparison")
                        && line.contains(&format!("response_id=\"{id}\""))
                        && line.contains(&format!("baseline=\"{baseline}\""))
                })
                .context("missing completed cache comparison")
        };
        let notes = comparison("notes", "ordinary")?;
        let work = comparison("work", "previous")?;
        assert!(work.contains("attempt=1"));
        if recover {
            assert!(work.contains("transport_reason=\"websocket_upgrade_required\""));
            assert!(work.contains("http_fallback_cause=\"upgrade_required\""));
            assert!(work.contains("http_retries=1"));
            assert!(notes.contains("transport_reason=\"sticky_websocket_fallback\""));
            assert!(notes.contains("http_retries=0"));
        } else {
            assert!(!work.contains("http_fallback_cause="));
            assert_eq!(work.contains("http_retries=0"), !websocket);
        }
        assert!(notes.contains(if websocket {
            "transport=\"incremental\""
        } else {
            "transport=\"http\""
        }));
        assert!(notes.contains("shape=\"extends_completed_work\""));
        assert!(notes.contains("accounted_cached_tokens=3840"));
        let compact = comparison("compact", "previous")?;
        assert!(compact.contains("operation=Compact"));
        assert!(compact.contains("baseline_id=\"notes\""));
        assert!(compact.contains("shape=\"sibling_suffix\""));
        assert!(compact.contains("accounted_cached_tokens=0"));
        assert!(comparison("compact", "ordinary")?.contains("baseline_id=\"work\""));
        assert!(comparison("done", "previous")?.contains("shape=\"new_history\""));
        assert!(lines.lines().any(|l| l.contains("provider cache receipt")
            && l.contains("response_id=\"compact\"")
            && l.contains("cached_tokens_reported=true")));
        assert!(
            lines
                .lines()
                .any(|l| l.contains("comparison_reason=\"model_changed\"")
                    && l.contains("cache_missed_tokens=27000"))
        );
        assert!(lines.lines().any(|l| l.contains("provider cache receipt")
            && l.contains("response_id=\"work\"")
            && l.contains("cached_tokens_reported=false")));
        assert!(lines.lines().any(|l| l.contains("reasoning replay report")
            && l.contains("response_id=\"compact\"")
            && l.contains("no_removals_reported")));
        assert!(
            !lines.contains("private task text")
                && !lines.contains("private note text")
                && !lines.contains("private ciphertext")
                && !lines.contains("private-argument-sentinel")
                && !lines.contains("private-output-sentinel")
                && !lines.contains("codex.tool_result")
        );
    }
    Ok(())
}
