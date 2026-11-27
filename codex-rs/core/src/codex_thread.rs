use crate::agent::AgentStatus;
use crate::error::Result as CodexResult;
use crate::protocol::Event;
use crate::protocol::Op;
use crate::protocol::Submission;
use crate::session::Codex;
use codex_protocol::config_types::Personality;
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::SandboxPolicy;
use codex_protocol::protocol::SessionSource;
use std::path::PathBuf;
use tokio::sync::watch;

use crate::state_db::StateDbHandle;

#[derive(Clone, Debug)]
pub struct ThreadConfigSnapshot {
    pub model: String,
    pub model_provider_id: String,
    pub approval_policy: AskForApproval,
    pub sandbox_policy: SandboxPolicy,
    pub cwd: PathBuf,
    pub reasoning_effort: Option<ReasoningEffort>,
    pub service_tier: Option<String>,
    pub personality: Option<Personality>,
    pub session_source: SessionSource,
}

pub struct CodexThread {
    codex: Codex,
    rollout_path: Option<PathBuf>,
}

/// Conduit for the bidirectional stream of messages that compose a thread
/// (formerly called a conversation) in Codex.
impl CodexThread {
    pub(crate) async fn mcp_config_for_reload(
        &self,
        cloud: crate::config_loader::CloudRequirementsLoader,
    ) -> std::io::Result<crate::config::Config> {
        self.codex.session.mcp_config_for_reload(cloud).await
    }
    pub(crate) async fn queue_trusted_mcp_refresh(
        &self,
        refresh: codex_protocol::protocol::McpServerRefreshConfig,
        constraints: crate::mcp::ServerConstraints,
    ) {
        self.codex
            .session
            .queue_trusted_mcp_refresh(refresh, constraints)
            .await;
    }
    pub(crate) fn new(codex: Codex, rollout_path: Option<PathBuf>) -> Self {
        Self {
            codex,
            rollout_path,
        }
    }

    pub async fn submit(&self, op: Op) -> CodexResult<String> {
        self.codex.submit(op).await
    }

    /// Wait for ordered idle-thread admission, not for inference or installation.
    pub async fn start_compaction(&self) -> CodexResult<()> {
        self.codex.start_compaction().await
    }

    /// Use sparingly: this is intended to be removed soon.
    pub async fn submit_with_id(&self, sub: Submission) -> CodexResult<()> {
        self.codex.submit_with_id(sub).await
    }

    pub async fn next_event(&self) -> CodexResult<Event> {
        self.codex.next_event().await
    }

    pub async fn agent_status(&self) -> AgentStatus {
        self.codex.agent_status().await
    }

    pub async fn active_turn_id(&self) -> Option<String> {
        self.codex.active_turn_id().await
    }

    pub(crate) fn subscribe_status(&self) -> watch::Receiver<AgentStatus> {
        self.codex.agent_status.clone()
    }

    pub fn rollout_path(&self) -> Option<PathBuf> {
        self.rollout_path.clone()
    }

    pub fn state_db(&self) -> Option<StateDbHandle> {
        self.codex.state_db()
    }

    pub async fn config_snapshot(&self) -> ThreadConfigSnapshot {
        self.codex.thread_config_snapshot().await
    }

    pub async fn collaboration_mode(&self) -> codex_protocol::config_types::CollaborationMode {
        self.codex.session.collaboration_mode().await
    }

    pub async fn effective_fork_config(&self) -> crate::config::Config {
        self.codex.session.effective_fork_config().await
    }

    pub async fn active_turn_ids(&self) -> Vec<String> {
        let active = self.codex.session.active_turn.lock().await;
        active
            .as_ref()
            .map(|turn| turn.tasks.keys().cloned().collect())
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AuthManager;
    use crate::CodexAuth;
    use crate::ThreadManager;
    use crate::config::ConfigBuilder;
    use crate::config::ConfigOverrides;
    use crate::config::set_project_trust_level;
    use codex_protocol::config_types::TrustLevel;
    use pretty_assertions::assert_eq;
    use std::collections::BTreeSet;

    #[test_case::test_case(false, false; "queued reviewer")]
    #[test_case::test_case(true, false; "queued ordinary continuation")]
    #[test_case::test_case(true, true; "canceled continuation does not own the thread")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn compact_cannot_overtake_reserved_review_work(
        continuation: bool,
        cancel: bool,
    ) -> anyhow::Result<()> {
        core_test_support::skip_if_no_network_unless_localhost!(Ok(()));
        use crate::completion_review::InternalReviewOperation;
        use codex_protocol::protocol::EventMsg;
        use codex_protocol::user_input::UserInput;
        use core_test_support::responses;
        use serde_json::json;
        let server = responses::start_mock_server().await;
        let home = tempfile::tempdir()?;
        let mut config = ConfigBuilder::default()
            .codex_home(home.path().into())
            .build()
            .await?;
        config.model = Some("gpt-5.1".into());
        config.review_model = config.model.clone();
        config.cwd = home.path().into();
        config.responses_transport = crate::config::types::ResponsesTransport::Http;
        config.model_provider.base_url = Some(format!("{}/v1", server.uri()));
        config.model_providers.insert(
            config.model_provider_id.clone(),
            config.model_provider.clone(),
        );
        let manager = ThreadManager::with_models_provider_and_home(
            CodexAuth::from_api_key("test"),
            config.model_provider.clone(),
            home.path().into(),
        );
        let thread = manager.start_thread(config).await?.thread;
        let end = if cancel {
            json!({"type":"response.output_item.done","item":{"type":"compaction","encrypted_content":"summary"}})
        } else if continuation {
            responses::ev_assistant_message("last", "Revised.")
        } else {
            responses::ev_assistant_message(
                "verdict",
                r#"{"evaluation":"Complete.","fix_actions_advised":false}"#,
            )
        };
        let wire = responses::mount_sse_sequence(
            &server,
            vec![
                responses::sse(vec![
                    responses::ev_assistant_message("first", "Candidate."),
                    responses::ev_completed("first"),
                ]),
                responses::sse(vec![end, responses::ev_completed("last")]),
            ],
        )
        .await;
        let complete = || async {
            tokio::time::timeout(std::time::Duration::from_secs(10), async {
                let mut verdict = None;
                loop {
                    match thread.next_event().await?.msg {
                        EventMsg::ExitedReviewMode(exit) => {
                            verdict = exit
                                .post_turn_completion_review_output
                                .map(|v| v.evaluation);
                        }
                        EventMsg::TurnComplete(end) => {
                            return anyhow::Ok((verdict, end.last_agent_message));
                        }
                        EventMsg::Error(e) => anyhow::bail!("{}", e.message),
                        _ => {}
                    }
                }
            })
            .await?
        };
        thread
            .submit(Op::UserInput {
                items: vec![UserInput::Text {
                    text: "Check the work.".into(),
                    text_elements: vec![],
                }],
                final_output_json_schema: None,
            })
            .await?;
        assert_eq!(complete().await?.1.as_deref(), Some("Candidate."));
        let session = &thread.codex.session;
        // Put the two submissions in the exact order possible under the finalizer lock:
        // maintenance is already in the FIFO when the terminal event's successor is enqueued.
        let lifecycle = session.task_lifecycle.lock().await;
        let ticket = session.begin_completion_review(false).await.unwrap();
        let operation = if continuation {
            assert!(
                session
                    .prepare_completion_review_handoff(
                        &ticket,
                        "Verify the evidence.".into(),
                        "gpt-5.1".into()
                    )
                    .await
            );
            session.prepared_completion_review_handoff().await.unwrap()
        } else {
            InternalReviewOperation::Review(ticket)
        };
        let reserved = session.reserve_review_work(operation);
        let maintenance = thread.start_compaction();
        tokio::pin!(maintenance);
        assert!(futures::poll!(&mut maintenance).is_pending());
        session.queue_completion_review_operation(reserved).await;
        if cancel {
            session.stop_completion_review_operations().await;
        }
        drop(lifecycle);
        let result = maintenance.await;
        if cancel {
            result?;
        } else {
            assert!(matches!(
                result,
                Err(crate::error::CodexErr::UnsupportedOperation(_))
            ));
        }
        let (verdict, answer) = complete().await?;
        assert_eq!(
            verdict.as_deref(),
            (!cancel && !continuation).then_some("Complete.")
        );
        if !cancel && continuation {
            assert_eq!(answer.as_deref(), Some("Revised."));
        }
        thread.submit(Op::Shutdown).await?;
        while !matches!(thread.next_event().await?.msg, EventMsg::ShutdownComplete) {}
        let sent = wire.requests();
        assert_eq!(sent.len(), 2);
        let input = sent[1].input();
        assert_eq!(
            input.last().unwrap()["type"] == "compaction_trigger",
            cancel
        );
        if !cancel {
            let main = input.iter().any(|v| {
                v["role"] == "developer" && v.to_string().contains("Verify the evidence.")
            });
            assert_eq!(main, continuation);
        }
        Ok(())
    }

    #[tokio::test]
    async fn workspace_mcp_reload_preserves_session_flags_and_pending_installs()
    -> anyhow::Result<()> {
        let home = tempfile::tempdir()?;
        let projects = tempfile::tempdir()?;
        let auth = AuthManager::from_auth_for_testing(CodexAuth::from_api_key("test"));
        let manager = ThreadManager::new(home.path().into(), auth, SessionSource::Exec);
        let mut threads = Vec::new();
        let server = "command = 'unused'\nenabled = false\n";
        for name in ["first", "second"] {
            let project = projects.path().join(name);
            std::fs::create_dir_all(project.join(".git"))?;
            std::fs::create_dir_all(project.join(".codex"))?;
            let project = project.canonicalize()?;
            let file = project.join(".codex/config.toml");
            std::fs::write(&file, format!("[mcp_servers.removed]\n{server}"))?;
            set_project_trust_level(home.path(), &project, TrustLevel::Trusted)?;
            let config = ConfigBuilder::default()
                .codex_home(home.path().into())
                .harness_overrides(ConfigOverrides {
                    cwd: Some(project),
                    ..Default::default()
                })
                .cli_overrides(vec![(
                    "mcp_servers.flag".into(),
                    server.parse::<toml::Table>()?.into(),
                )])
                .build()
                .await?;
            assert!(config.mcp_servers.get().contains_key("removed"));
            let mode = config.mcp_oauth_credentials_store_mode;
            let thread = manager.start_thread(config).await?.thread;
            std::fs::write(file, format!("[mcp_servers.{name}]\n{server}"))?;
            threads.push((name, thread, mode));
        }
        manager.refresh_mcp_servers(Default::default()).await?;
        for (name, thread, mode) in threads {
            let session = &thread.codex.session;
            let mut expected = session.configured_mcp_servers().await;
            assert_eq!(
                expected.keys().map(String::as_str).collect::<BTreeSet<_>>(),
                [name, "flag"].into_iter().collect()
            );
            let new_server = expected[name].clone();
            expected.insert("installed".into(), new_server.clone());
            let turn = session.new_default_turn().await;
            session
                .refresh_mcp_servers_after_install(
                    &turn,
                    vec![("installed".into(), new_server)],
                    mode,
                )
                .await;
            assert_eq!(session.configured_mcp_servers().await, expected);
            assert_eq!(
                session
                    .services
                    .mcp_connection_manager
                    .read()
                    .await
                    .configured_servers,
                expected
            );
            thread.submit(Op::Shutdown).await?;
            while !matches!(
                thread.next_event().await?.msg,
                crate::protocol::EventMsg::ShutdownComplete
            ) {}
        }
        Ok(())
    }
}
