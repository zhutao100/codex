use super::*;

impl From<&TurnContext> for SandboxState {
    fn from(context: &TurnContext) -> Self {
        Self {
            sandbox_policy: context.sandbox_policy.clone(),
            sandbox_cwd: context.cwd.clone(),
            codex_linux_sandbox_exe: context.codex_linux_sandbox_exe.clone(),
        }
    }
}

impl Session {
    pub(super) async fn activate_mcp_context(&self, context: &TurnContext) {
        self.refresh_mcp_servers_if_requested(context).await;
        self.current_mcp_manager()
            .await
            .activate_sandbox(context.into())
            .await;
    }

    /// Keep workspace connections alive across login/logout, but never send a new request
    /// through a hosted connection established with different account credentials.
    pub(crate) async fn current_mcp_manager(&self) -> Arc<McpConnectionManager> {
        let clients = &self.services.mcp_connection_manager;
        if !self.features.enabled(Feature::Apps) {
            return clients.read().await.clone();
        }
        let config = self.get_config().await;
        let desired = || {
            crate::mcp::hosted_apps_server(
                true,
                self.services.auth_manager.auth_cached().as_ref(),
                &config,
            )
        };
        let guard = clients.read().await;
        if guard
            .configured_servers
            .get(crate::mcp::CODEX_APPS_MCP_SERVER_NAME)
            == desired().as_ref()
        {
            return guard.clone();
        }
        drop(guard);
        let mut guard = clients.write().await;
        Arc::make_mut(&mut guard)
            .rebind_hosted_apps(
                desired(),
                config.mcp_oauth_credentials_store_mode,
                self.mcp_startup_cancellation_token().await,
                self.get_tx_event(),
            )
            .await;
        guard.clone()
    }

    pub(crate) async fn mcp_config_for_reload(
        &self,
        cloud: crate::config_loader::CloudRequirementsLoader,
    ) -> std::io::Result<Config> {
        let (startup, cwd) = {
            let state = self.state.lock().await;
            let session = &state.session_configuration;
            (
                session.original_config_do_not_use.clone(),
                session.cwd.clone(),
            )
        };
        if startup.bounded_explorer {
            return Ok((*startup).clone());
        }
        let flags = startup
            .config_layer_stack
            .layers_high_to_low()
            .into_iter()
            .find(|layer| {
                matches!(
                    layer.name,
                    codex_app_server_protocol::ConfigLayerSource::SessionFlags
                )
            })
            .and_then(|layer| layer.config.as_table())
            .map(|flags| flags.clone().into_iter().collect())
            .unwrap_or_default();
        crate::config::ConfigBuilder::default()
            .codex_home(startup.codex_home.clone())
            .harness_overrides(crate::config::ConfigOverrides {
                cwd: Some(cwd),
                config_profile: startup.active_profile.clone(),
                ..Default::default()
            })
            .cli_overrides(flags)
            .cloud_requirements(cloud)
            .build()
            .await
    }

    pub(crate) async fn queue_trusted_mcp_refresh(
        &self,
        refresh: McpServerRefreshConfig,
        constraints: crate::mcp::ServerConstraints,
    ) {
        // Only the host config loader supplies constraints; a client-supplied map cannot relax them.
        let mut latest = self.refreshed_mcp_constraints.write().await;
        *latest = Some(constraints);
        *self.pending_mcp_server_refresh_config.lock().await = Some(refresh);
    }
    pub async fn resolve_elicitation(
        &self,
        server_name: String,
        id: RequestId,
        response: ElicitationResponse,
    ) -> anyhow::Result<()> {
        self.current_mcp_manager()
            .await
            .resolve_elicitation(server_name, id, response)
            .await
    }

    pub async fn list_resources(
        &self,
        server: &str,
        params: Option<PaginatedRequestParam>,
    ) -> anyhow::Result<ListResourcesResult> {
        self.current_mcp_manager()
            .await
            .list_resources(server, params)
            .await
    }

    pub async fn list_resource_templates(
        &self,
        server: &str,
        params: Option<PaginatedRequestParam>,
    ) -> anyhow::Result<ListResourceTemplatesResult> {
        self.current_mcp_manager()
            .await
            .list_resource_templates(server, params)
            .await
    }

    pub async fn read_resource(
        &self,
        server: &str,
        params: ReadResourceRequestParam,
    ) -> anyhow::Result<ReadResourceResult> {
        self.current_mcp_manager()
            .await
            .read_resource(server, params)
            .await
    }

    async fn refresh_mcp_servers_inner(
        &self,
        sandbox: SandboxState,
        mcp_servers: HashMap<String, McpServerConfig>,
        store_mode: OAuthCredentialsStoreMode,
    ) {
        let auth = self.services.auth_manager.auth().await;
        let config = self.get_config().await;
        // A bounded explorer has no MCP transports, including after a direct refresh request.
        if config.bounded_explorer {
            return;
        }
        let constrained = match crate::mcp::constrain_servers(
            &config.mcp_servers,
            self.refreshed_mcp_constraints.read().await.as_ref(),
            mcp_servers,
        ) {
            Ok(servers) => servers,
            Err(err) => {
                warn!("failed to validate refreshed MCP servers: {err}");
                return;
            }
        };
        let mcp_servers = with_codex_apps_mcp(
            constrained.get().clone(),
            self.features.enabled(Feature::Apps),
            auth.as_ref(),
            config.as_ref(),
        );
        let auth_statuses = compute_auth_statuses(mcp_servers.iter(), store_mode).await;
        let cancel_token = self.reset_mcp_startup_cancellation_token().await;

        let mut refreshed_manager = McpConnectionManager::default();
        refreshed_manager
            .initialize(
                &mcp_servers,
                store_mode,
                auth_statuses,
                self.get_tx_event(),
                cancel_token,
                sandbox,
            )
            .await;

        let mut manager = self.services.mcp_connection_manager.write().await;
        manager.retire_hosted_apps();
        *manager = Arc::new(refreshed_manager);
    }

    pub(super) async fn refresh_mcp_servers_if_requested(&self, turn_context: &TurnContext) {
        self.apply_pending_mcp_refresh(turn_context, false).await;
    }

    /// A quote may be for a deferred task. Update inventory without transferring
    /// sandbox ownership to that task before its first admitted request.
    pub(super) async fn refresh_mcp_inventory_for_quote(&self, turn_context: &TurnContext) {
        self.apply_pending_mcp_refresh(turn_context, true).await;
    }

    async fn apply_pending_mcp_refresh(&self, turn_context: &TurnContext, preserve_owner: bool) {
        let refresh_config = { self.pending_mcp_server_refresh_config.lock().await.take() };
        let Some(refresh_config) = refresh_config else {
            return;
        };

        let McpServerRefreshConfig {
            mcp_servers,
            mcp_oauth_credentials_store_mode,
        } = refresh_config;

        let mcp_servers =
            match serde_json::from_value::<HashMap<String, McpServerConfig>>(mcp_servers) {
                Ok(servers) => servers,
                Err(err) => {
                    warn!("failed to parse MCP server refresh config: {err}");
                    return;
                }
            };
        let store_mode = match serde_json::from_value::<OAuthCredentialsStoreMode>(
            mcp_oauth_credentials_store_mode,
        ) {
            Ok(mode) => mode,
            Err(err) => {
                warn!("failed to parse MCP OAuth refresh config: {err}");
                return;
            }
        };

        let sandbox = if preserve_owner {
            self.current_mcp_manager().await.selected_sandbox().await
        } else {
            None
        };
        self.refresh_mcp_servers_inner(
            sandbox.unwrap_or_else(|| turn_context.into()),
            mcp_servers,
            store_mode,
        )
        .await;
    }

    pub(crate) async fn refresh_mcp_servers_after_install(
        &self,
        turn_context: &TurnContext,
        added: Vec<(String, McpServerConfig)>,
        store_mode: OAuthCredentialsStoreMode,
    ) {
        let running = self.current_mcp_manager().await.configured_servers.clone();
        let mut pending = self.pending_mcp_server_refresh_config.lock().await;
        let mut servers: HashMap<String, McpServerConfig> = pending
            .as_ref()
            .and_then(|next| serde_json::from_value(next.mcp_servers.clone()).ok())
            .unwrap_or(running);
        for (name, server) in added {
            servers.entry(name).or_insert(server);
        }
        // The next turn must not replay an older map over this installation.
        if let Some(next) = pending.as_mut()
            && let Ok(merged) = serde_json::to_value(&servers)
        {
            next.mcp_servers = merged;
        }
        drop(pending);
        self.refresh_mcp_servers_inner(turn_context.into(), servers, store_mode)
            .await;
    }

    pub(crate) async fn configured_mcp_servers(&self) -> HashMap<String, McpServerConfig> {
        let pending = self.pending_mcp_server_refresh_config.lock().await;
        if let Some(refresh) = pending.as_ref()
            && let Ok(servers) = serde_json::from_value(refresh.mcp_servers.clone())
        {
            return servers;
        }
        drop(pending);
        self.current_mcp_manager().await.configured_servers.clone()
    }

    pub(super) async fn mcp_startup_cancellation_token(&self) -> CancellationToken {
        self.services
            .mcp_startup_cancellation_token
            .lock()
            .await
            .clone()
    }

    async fn reset_mcp_startup_cancellation_token(&self) -> CancellationToken {
        let mut guard = self.services.mcp_startup_cancellation_token.lock().await;
        guard.cancel();
        let cancel_token = CancellationToken::new();
        *guard = cancel_token.clone();
        cancel_token
    }

    pub(crate) async fn cancel_mcp_startup(&self) {
        self.services
            .mcp_startup_cancellation_token
            .lock()
            .await
            .cancel();
    }
}
