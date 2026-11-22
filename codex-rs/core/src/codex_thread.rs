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
