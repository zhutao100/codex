use super::*;

/// The context needed for a single turn of the thread.
#[derive(Debug)]
pub(crate) struct TurnContext {
    pub(crate) sub_id: String,
    pub(crate) config: Arc<Config>,
    pub(crate) auth_manager: Option<Arc<AuthManager>>,
    pub(crate) model_info: ModelInfo,
    pub(crate) otel_manager: OtelManager,
    pub(crate) provider: ModelProviderInfo,
    pub(crate) reasoning_effort: Option<ReasoningEffortConfig>,
    pub(crate) reasoning_summary: ReasoningSummaryConfig,
    pub(crate) service_tier: Option<ServiceTier>,
    pub(crate) session_source: SessionSource,
    /// The session's current working directory. All relative paths provided by
    /// the model as well as sandbox policies are resolved against this path
    /// instead of `std::env::current_dir()`.
    pub(crate) cwd: PathBuf,
    pub(crate) developer_instructions: Option<String>,
    pub(crate) compact_prompt: Option<String>,
    pub(crate) user_instructions: Option<String>,
    pub(crate) collaboration_mode: CollaborationMode,
    pub(crate) personality: Option<Personality>,
    pub(crate) final_instruction_override: Option<String>,
    pub(crate) approval_policy: AskForApproval,
    pub(crate) sandbox_policy: SandboxPolicy,
    pub(crate) windows_sandbox_level: WindowsSandboxLevel,
    pub(crate) shell_environment_policy: ShellEnvironmentPolicy,
    pub(crate) tools_config: ToolsConfig,
    pub(crate) features: Features,
    pub(crate) ghost_snapshot: GhostSnapshotConfig,
    pub(crate) final_output_json_schema: Option<Value>,
    pub(crate) codex_linux_sandbox_exe: Option<PathBuf>,
    pub(crate) tool_call_gate: Arc<ReadinessFlag>,
    pub(crate) truncation_policy: TruncationPolicy,
    pub(crate) dynamic_tools: Vec<DynamicToolSpec>,
    pub(super) turn_metadata_header: OnceCell<Option<String>>,
}
impl TurnContext {
    pub(crate) fn model_context_window(&self) -> Option<i64> {
        let effective_context_window_percent = self.model_info.effective_context_window_percent;
        self.model_info.context_window.map(|context_window| {
            context_window.saturating_mul(effective_context_window_percent) / 100
        })
    }

    pub(crate) fn resolve_path(&self, path: Option<String>) -> PathBuf {
        path.as_ref()
            .map(PathBuf::from)
            .map_or_else(|| self.cwd.clone(), |p| self.cwd.join(p))
    }

    pub(crate) fn compact_prompt(&self) -> &str {
        self.compact_prompt
            .as_deref()
            .unwrap_or(compact::SUMMARIZATION_PROMPT)
    }

    pub(crate) fn effective_model_instructions(&self) -> String {
        ModelsManager::effective_model_instructions(
            &self.model_info,
            self.personality,
            self.final_instruction_override.as_deref(),
        )
    }

    async fn build_turn_metadata_header(&self) -> Option<String> {
        self.turn_metadata_header
            .get_or_init(|| async { build_turn_metadata_header(self.cwd.as_path()).await })
            .await
            .clone()
    }

    pub async fn resolve_turn_metadata_header(&self) -> Option<String> {
        const TURN_METADATA_HEADER_TIMEOUT_MS: u64 = 250;
        match tokio::time::timeout(
            std::time::Duration::from_millis(TURN_METADATA_HEADER_TIMEOUT_MS),
            self.build_turn_metadata_header(),
        )
        .await
        {
            Ok(header) => header,
            Err(_) => {
                warn!("timed out after 250ms while building turn metadata header");
                self.turn_metadata_header.get().cloned().flatten()
            }
        }
    }

    pub fn spawn_turn_metadata_header_task(self: &Arc<Self>) {
        let context = Arc::clone(self);
        tokio::spawn(async move {
            trace!("Spawning turn metadata calculation task");
            context.build_turn_metadata_header().await;
            trace!("Turn metadata calculation task completed");
        });
    }
}

impl Session {
    /// Don't expand the number of mutated arguments on config. We are in the process of getting rid of it.
    pub(crate) fn build_per_turn_config(session_configuration: &SessionConfiguration) -> Config {
        // todo(aibrahim): store this state somewhere else so we don't need to mut config
        let config = session_configuration.original_config_do_not_use.clone();
        let mut per_turn_config = (*config).clone();
        per_turn_config.model_reasoning_effort =
            session_configuration.collaboration_mode.reasoning_effort();
        per_turn_config.model_reasoning_summary = session_configuration.model_reasoning_summary;
        per_turn_config.service_tier = session_configuration.service_tier;
        per_turn_config.personality = session_configuration.personality;
        per_turn_config.web_search_mode = Some(resolve_web_search_mode_for_turn(
            per_turn_config.web_search_mode,
            session_configuration.provider.is_azure_responses_endpoint(),
            session_configuration.sandbox_policy.get(),
        ));
        per_turn_config.features = config.features.clone();
        per_turn_config
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn make_turn_context(
        auth_manager: Option<Arc<AuthManager>>,
        otel_manager: &OtelManager,
        provider: ModelProviderInfo,
        session_configuration: &SessionConfiguration,
        per_turn_config: Config,
        model_info: ModelInfo,
        final_instruction_override: Option<String>,
        sub_id: String,
    ) -> TurnContext {
        let reasoning_effort = session_configuration.collaboration_mode.reasoning_effort();
        let reasoning_summary = session_configuration.model_reasoning_summary;
        let otel_manager = otel_manager.clone().with_model(
            session_configuration.collaboration_mode.model(),
            model_info.slug.as_str(),
        );
        let session_source = session_configuration.session_source.clone();
        let auth_manager_for_context = auth_manager;
        let provider_for_context = provider;
        let otel_manager_for_context = otel_manager;
        let per_turn_config = Arc::new(per_turn_config);

        let tools_config = ToolsConfig::new(&ToolsConfigParams {
            model_info: &model_info,
            features: &per_turn_config.features,
            web_search_mode: per_turn_config.web_search_mode,
        });

        let cwd = session_configuration.cwd.clone();
        TurnContext {
            sub_id,
            config: per_turn_config.clone(),
            auth_manager: auth_manager_for_context,
            model_info: model_info.clone(),
            otel_manager: otel_manager_for_context,
            provider: provider_for_context,
            reasoning_effort,
            reasoning_summary,
            service_tier: session_configuration.service_tier,
            session_source,
            cwd,
            developer_instructions: session_configuration.developer_instructions.clone(),
            compact_prompt: session_configuration.compact_prompt.clone(),
            user_instructions: session_configuration.user_instructions.clone(),
            collaboration_mode: session_configuration.collaboration_mode.clone(),
            personality: session_configuration.personality,
            final_instruction_override,
            approval_policy: session_configuration.approval_policy.value(),
            sandbox_policy: session_configuration.sandbox_policy.get().clone(),
            windows_sandbox_level: session_configuration.windows_sandbox_level,
            shell_environment_policy: per_turn_config.shell_environment_policy.clone(),
            tools_config,
            features: per_turn_config.features.clone(),
            ghost_snapshot: per_turn_config.ghost_snapshot.clone(),
            final_output_json_schema: None,
            codex_linux_sandbox_exe: per_turn_config.codex_linux_sandbox_exe.clone(),
            tool_call_gate: Arc::new(ReadinessFlag::new()),
            truncation_policy: model_info.truncation_policy.into(),
            dynamic_tools: session_configuration.dynamic_tools.clone(),
            turn_metadata_header: OnceCell::new(),
        }
    }

    pub(crate) async fn new_turn_with_sub_id(
        &self,
        sub_id: String,
        updates: SessionSettingsUpdate,
    ) -> ConstraintResult<Arc<TurnContext>> {
        let (session_configuration, sandbox_policy_changed) = {
            let mut state = self.state.lock().await;
            match state.session_configuration.clone().apply(&updates) {
                Ok(next) => {
                    let sandbox_policy_changed =
                        state.session_configuration.sandbox_policy != next.sandbox_policy;
                    state.session_configuration = next.clone();
                    (next, sandbox_policy_changed)
                }
                Err(err) => {
                    drop(state);
                    self.send_event_raw(Event {
                        id: sub_id.clone(),
                        msg: EventMsg::Error(ErrorEvent {
                            message: err.to_string(),
                            codex_error_info: Some(CodexErrorInfo::BadRequest),
                        }),
                    })
                    .await;
                    return Err(err);
                }
            }
        };

        Ok(self
            .new_turn_from_configuration(
                sub_id,
                session_configuration,
                updates.final_output_json_schema,
                sandbox_policy_changed,
            )
            .await)
    }

    async fn new_turn_from_configuration(
        &self,
        sub_id: String,
        session_configuration: SessionConfiguration,
        final_output_json_schema: Option<Option<Value>>,
        sandbox_policy_changed: bool,
    ) -> Arc<TurnContext> {
        let per_turn_config = Self::build_per_turn_config(&session_configuration);

        if sandbox_policy_changed {
            let sandbox_state = SandboxState {
                sandbox_policy: per_turn_config.sandbox_policy.get().clone(),
                codex_linux_sandbox_exe: per_turn_config.codex_linux_sandbox_exe.clone(),
                sandbox_cwd: per_turn_config.cwd.clone(),
                use_linux_sandbox_bwrap: per_turn_config
                    .features
                    .enabled(Feature::UseLinuxSandboxBwrap),
            };
            if let Err(e) = self
                .services
                .mcp_connection_manager
                .read()
                .await
                .notify_sandbox_state_change(&sandbox_state)
                .await
            {
                warn!("Failed to notify sandbox state change to MCP servers: {e:#}");
            }
        }

        let model_info = self
            .services
            .models_manager
            .get_model_info(
                session_configuration.collaboration_mode.model(),
                &per_turn_config,
            )
            .await;
        let final_instruction_override = self
            .services
            .models_manager
            .get_final_instruction_override(
                session_configuration.collaboration_mode.model(),
                &per_turn_config,
            )
            .await;
        let mut turn_context: TurnContext = Self::make_turn_context(
            Some(Arc::clone(&self.services.auth_manager)),
            &self.services.otel_manager,
            session_configuration.provider.clone(),
            &session_configuration,
            per_turn_config,
            model_info,
            final_instruction_override,
            sub_id,
        );

        if let Some(final_schema) = final_output_json_schema {
            turn_context.final_output_json_schema = final_schema;
        }
        let turn_context = Arc::new(turn_context);
        turn_context.spawn_turn_metadata_header_task();
        turn_context
    }

    pub(crate) async fn new_default_turn(&self) -> Arc<TurnContext> {
        self.new_default_turn_with_sub_id(self.next_internal_sub_id())
            .await
    }

    pub(crate) async fn new_default_turn_with_sub_id(&self, sub_id: String) -> Arc<TurnContext> {
        let session_configuration = {
            let state = self.state.lock().await;
            state.session_configuration.clone()
        };
        self.new_turn_from_configuration(sub_id, session_configuration, None, false)
            .await
    }
}
