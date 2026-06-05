use super::*;
use std::sync::atomic::AtomicBool;

/// Spawn a review thread using the given prompt.
pub(super) async fn spawn_review_thread(
    sess: Arc<Session>,
    config: Arc<Config>,
    parent_turn_context: Arc<TurnContext>,
    sub_id: String,
    resolved: crate::review_prompts::ResolvedReviewRequest,
) {
    let per_turn_config = configure_review_delegate_config(
        config.as_ref(),
        parent_turn_context.model_info.slug.as_str(),
        ReviewDelegateConfigParams {
            base_instructions: config.review_prompt(),
            sandbox_policy: parent_turn_context.sandbox_policy.clone(),
            disable_collab: true,
            instruction_profile: ReviewDelegateInstructionProfile::Review,
        },
    )
    .unwrap_or_else(|_| {
        let mut fallback = config.as_ref().clone();
        fallback.model = config
            .review_model
            .clone()
            .or_else(|| Some(parent_turn_context.model_info.slug.clone()));
        fallback.web_search_mode = Some(WebSearchMode::Disabled);
        fallback
            .features
            .disable(Feature::WebSearchRequest)
            .disable(Feature::WebSearchCached)
            .disable(Feature::Collab);
        fallback
    });
    let model = per_turn_config
        .model
        .clone()
        .unwrap_or_else(|| parent_turn_context.model_info.slug.clone());
    let review_features = per_turn_config.features.clone();
    let review_web_search_mode = WebSearchMode::Disabled;

    let review_model_info = sess
        .services
        .models_manager
        .get_model_info(&model, &per_turn_config)
        .await;
    let tools_config = ToolsConfig::new(&ToolsConfigParams {
        model_info: &review_model_info,
        features: &review_features,
        web_search_mode: Some(review_web_search_mode),
    });

    let review_prompt = resolved.prompt.clone();
    let model_provider_id = per_turn_config.model_provider_id.clone();
    let provider = per_turn_config.model_provider.clone();
    let auth_manager = parent_turn_context.auth_manager.clone();
    let model_info = review_model_info.clone();

    let otel_manager = parent_turn_context
        .otel_manager
        .clone()
        .with_model(model.as_str(), review_model_info.slug.as_str());
    let auth_manager_for_context = auth_manager.clone();
    let provider_for_context = provider.clone();
    let otel_manager_for_context = otel_manager.clone();
    let reasoning_effort = per_turn_config.model_reasoning_effort;
    let reasoning_summary = per_turn_config.model_reasoning_summary;
    let service_tier = per_turn_config.service_tier.clone();
    let session_source = parent_turn_context.session_source.clone();

    let per_turn_config = Arc::new(per_turn_config);

    let review_turn_context = TurnContext {
        sub_id: sub_id.to_string(),
        config: Arc::clone(&per_turn_config),
        auth_manager: auth_manager_for_context,
        model_info: model_info.clone(),
        otel_manager: otel_manager_for_context,
        model_provider_id,
        provider: provider_for_context,
        reasoning_effort,
        reasoning_summary,
        service_tier,
        session_source,
        tools_config,
        features: review_features,
        ghost_snapshot: parent_turn_context.ghost_snapshot.clone(),
        developer_instructions: None,
        user_instructions: None,
        compact_prompt: parent_turn_context.compact_prompt.clone(),
        collaboration_mode: parent_turn_context.collaboration_mode.clone(),
        personality: parent_turn_context.personality,
        final_instruction_override: None,
        approval_policy: *per_turn_config.approval_policy.get(),
        sandbox_policy: per_turn_config.sandbox_policy.get().clone(),
        windows_sandbox_level: parent_turn_context.windows_sandbox_level,
        shell_environment_policy: parent_turn_context.shell_environment_policy.clone(),
        cwd: parent_turn_context.cwd.clone(),
        final_output_json_schema: None,
        codex_linux_sandbox_exe: parent_turn_context.codex_linux_sandbox_exe.clone(),
        tool_call_gate: Arc::new(ReadinessFlag::new()),
        dynamic_tools: parent_turn_context.dynamic_tools.clone(),
        truncation_policy: model_info.truncation_policy.into(),
        turn_metadata_header: parent_turn_context.turn_metadata_header.clone(),
        server_model_warning_emitted: AtomicBool::new(false),
        model_verification_emitted: AtomicBool::new(false),
    };

    // Seed the child task with the review prompt as the initial user message.
    let input: Vec<UserInput> = vec![UserInput::Text {
        text: review_prompt,
        // Review prompt is synthesized; no UI element ranges to preserve.
        text_elements: Vec::new(),
    }];
    let tc = Arc::new(review_turn_context);
    sess.spawn_task(tc.clone(), input, ReviewTask::new()).await;

    // Announce entering review mode so UIs can switch modes.
    let review_request = ReviewRequest {
        target: resolved.target,
        user_facing_hint: Some(resolved.user_facing_hint),
    };
    sess.send_event(&tc, EventMsg::EnteredReviewMode(review_request))
        .await;
}

pub(crate) async fn spawn_post_turn_completion_review(
    sess: Arc<Session>,
    turn_context: Arc<TurnContext>,
    completed_turn: CompletedTurnForReview,
) {
    spawn_post_turn_completion_review_inner(sess, turn_context, completed_turn, None).await;
}

pub(crate) async fn spawn_paused_post_turn_completion_review(
    sess: Arc<Session>,
    turn_context: Arc<TurnContext>,
    completed_turn: CompletedTurnForReview,
    checkpoint: PendingContinuation,
) {
    spawn_post_turn_completion_review_inner(sess, turn_context, completed_turn, Some(checkpoint))
        .await;
}

async fn spawn_post_turn_completion_review_inner(
    sess: Arc<Session>,
    turn_context: Arc<TurnContext>,
    completed_turn: CompletedTurnForReview,
    checkpoint: Option<PendingContinuation>,
) {
    let review_request = ReviewRequest {
        target: codex_protocol::protocol::ReviewTarget::Custom {
            instructions: "Review the last completed Codex turn.".to_string(),
        },
        user_facing_hint: Some("completed turn".to_string()),
    };
    sess.send_event(&turn_context, EventMsg::EnteredReviewMode(review_request))
        .await;
    let task = if let Some(checkpoint) = checkpoint {
        PostTurnCompletionReviewTask::resumed(completed_turn, checkpoint)
    } else {
        PostTurnCompletionReviewTask::new(completed_turn)
    };
    sess.spawn_task(Arc::clone(&turn_context), Vec::new(), task)
        .await;
}
