use super::*;

#[derive(Clone, Copy, Debug, Default)]
enum PreCompactNotesState {
    #[default]
    Idle,
    AwaitingNotes,
}

const AUTO_COMPACT_WORK_NOTES_CAPTURE_MAX_ATTEMPTS: u8 = 2;
const AUTO_COMPACT_WORK_NOTES_TOOL_REJECT_REASON: &str =
    "tool use disabled during auto-compact work-notes capture";

fn is_auto_compact_work_notes_message(message: &str) -> bool {
    message
        .trim_start()
        .starts_with(AUTO_COMPACT_WORK_NOTES_TAG)
}

async fn inject_pre_compact_work_notes_request(sess: &Session, turn_context: &TurnContext) {
    let request = format!(
        "{AUTO_COMPACT_WORK_NOTES_REQUEST_TAG}\n\
Token limit is approaching and follow-up work is still needed.\n\
\n\
Respond with exactly one assistant message and do not call tools.\n\
Produce structured SESSION WORK NOTES for the next model after compaction.\n\
\n\
Required sections:\n\
- Objective\n\
- Current status\n\
- Validated findings\n\
- Ruled-out hypotheses / dead ends\n\
- Open hypotheses / unresolved questions\n\
- Relevant files / why\n\
- Irrelevant files / why skip\n\
- Edits made\n\
- Edits in progress / intended edits\n\
- Next best step\n\
\n\
Keep it concise but loss-resistant.\n\
Begin with: {AUTO_COMPACT_WORK_NOTES_TAG}\n\
</AUTO_COMPACT_WORK_NOTES_REQUEST>"
    );
    let message: ResponseItem = DeveloperInstructions::new(request).into();
    sess.record_conversation_items(turn_context, std::slice::from_ref(&message))
        .await;
}

/// Takes a user message as input and runs a loop where, at each sampling request, the model
/// replies with either:
///
/// - requested function calls
/// - an assistant message
///
/// While it is possible for the model to return multiple of these items in a
/// single sampling request, in practice, we generally one item per sampling request:
///
/// - If the model requests a function call, we execute it and send the output
///   back to the model in the next sampling request.
/// - If the model sends only an assistant message, we record it in the
///   conversation history and consider the turn complete.
///
pub(crate) async fn run_turn(
    sess: Arc<Session>,
    turn_context: Arc<TurnContext>,
    input: Vec<UserInput>,
    cancellation_token: CancellationToken,
) -> Option<String> {
    if input.is_empty() {
        return None;
    }

    run_turn_inner(sess, turn_context, input, None, cancellation_token).await
}

pub(crate) async fn continue_turn(
    sess: Arc<Session>,
    turn_context: Arc<TurnContext>,
    checkpoint: PendingContinuation,
    cancellation_token: CancellationToken,
) -> Option<String> {
    run_turn_inner(
        sess,
        turn_context,
        Vec::new(),
        Some(checkpoint),
        cancellation_token,
    )
    .await
}

async fn run_turn_inner(
    sess: Arc<Session>,
    turn_context: Arc<TurnContext>,
    input: Vec<UserInput>,
    continuation: Option<PendingContinuation>,
    cancellation_token: CancellationToken,
) -> Option<String> {
    let model_info = turn_context.model_info.clone();
    let auto_compact_limit = model_info.auto_compact_token_limit().unwrap_or(i64::MAX);
    let total_usage_tokens = sess.get_total_token_usage().await;

    let event = EventMsg::TurnStarted(TurnStartedEvent {
        model_context_window: turn_context.model_context_window(),
        collaboration_mode_kind: turn_context.collaboration_mode.mode,
    });
    sess.send_event(&turn_context, event).await;

    let (explicit_app_paths, skill_name_counts_lower) = if let Some(checkpoint) = continuation {
        let remove_interrupted_abort = checkpoint.source == TurnContinuationSource::Interrupted;
        sess.prepare_history_for_continuation(remove_interrupted_abort)
            .await;
        sess.clean_rollout_for_continuation(remove_interrupted_abort)
            .await;
        sess.send_event_raw_flushed(Event {
            id: turn_context.sub_id.clone(),
            msg: EventMsg::TurnContinued(TurnContinuedEvent {
                continued_from_turn_id: checkpoint.continued_from_turn_id,
                source: checkpoint.source,
            }),
        })
        .await;
        (Vec::new(), HashMap::new())
    } else {
        if total_usage_tokens >= auto_compact_limit {
            if turn_context.final_output_json_schema.is_some() {
                run_auto_compact(&sess, &turn_context, None).await;
            } else {
                inject_pre_compact_work_notes_request(&sess, &turn_context).await;

                let mut attempts: u8 = 0;
                let explicit_app_paths: Vec<String> = Vec::new();
                let skill_name_counts_lower: HashMap<String, usize> = HashMap::new();
                let turn_diff_tracker = Arc::new(tokio::sync::Mutex::new(TurnDiffTracker::new()));
                let turn_metadata_header = turn_context.resolve_turn_metadata_header().await;
                let mut client_session = sess.services.model_client.new_session();

                loop {
                    let sampling_request_input: Vec<ResponseItem> = sess.prompt_history().await;
                    let tool_selection = SamplingRequestToolSelection {
                        explicit_app_paths: &explicit_app_paths,
                        skill_name_counts_lower: &skill_name_counts_lower,
                    };

                    match run_sampling_request(
                        Arc::clone(&sess),
                        Arc::clone(&turn_context),
                        Arc::clone(&turn_diff_tracker),
                        &mut client_session,
                        turn_metadata_header.as_deref(),
                        sampling_request_input,
                        tool_selection,
                        ToolCallExecutionMode::RejectAll {
                            reason: AUTO_COMPACT_WORK_NOTES_TOOL_REJECT_REASON,
                        },
                        cancellation_token.child_token(),
                    )
                    .await
                    {
                        Ok(output) => {
                            if let Some(notes) = output.last_agent_message
                                && is_auto_compact_work_notes_message(&notes)
                            {
                                run_auto_compact(&sess, &turn_context, Some(notes)).await;
                                break;
                            }

                            attempts += 1;
                            if attempts >= AUTO_COMPACT_WORK_NOTES_CAPTURE_MAX_ATTEMPTS {
                                info!(
                                    "Work-notes capture yielded no notes; compacting without notes after {attempts} attempts"
                                );
                                run_auto_compact(&sess, &turn_context, None).await;
                                break;
                            }

                            info!(
                                "Work-notes capture yielded no notes; retrying capture ({attempts}/{AUTO_COMPACT_WORK_NOTES_CAPTURE_MAX_ATTEMPTS})"
                            );
                        }
                        Err(e) => {
                            info!(
                                "Work-notes capture failed during pre-turn compaction; compacting without notes: {e:#}"
                            );
                            run_auto_compact(&sess, &turn_context, None).await;
                            break;
                        }
                    }
                }
            }
        }

        let skills_outcome = Some(
            sess.services
                .skills_manager
                .skills_for_cwd(&turn_context.cwd, false)
                .await,
        );

        let (skill_name_counts, skill_name_counts_lower) = skills_outcome.as_ref().map_or_else(
            || (HashMap::new(), HashMap::new()),
            |outcome| build_skill_name_counts(&outcome.skills, &outcome.disabled_paths),
        );
        let connector_slug_counts = if turn_context.config.features.enabled(Feature::Apps) {
            let mcp_tools = match sess
                .services
                .mcp_connection_manager
                .read()
                .await
                .list_all_tools()
                .or_cancel(&cancellation_token)
                .await
            {
                Ok(mcp_tools) => mcp_tools,
                Err(_) => return None,
            };
            let connectors = connectors::accessible_connectors_from_mcp_tools(&mcp_tools);
            build_connector_slug_counts(&connectors)
        } else {
            HashMap::new()
        };
        let mentioned_skills = skills_outcome.as_ref().map_or_else(Vec::new, |outcome| {
            collect_explicit_skill_mentions(
                &input,
                &outcome.skills,
                &outcome.disabled_paths,
                &skill_name_counts,
                &connector_slug_counts,
            )
        });
        let explicit_app_paths = collect_explicit_app_paths(&input);

        let config = turn_context.config.clone();
        if config
            .features
            .enabled(Feature::SkillEnvVarDependencyPrompt)
        {
            let env_var_dependencies = collect_env_var_dependencies(&mentioned_skills);
            resolve_skill_dependencies_for_turn(&sess, &turn_context, &env_var_dependencies).await;
        }

        maybe_prompt_and_install_mcp_dependencies(
            sess.as_ref(),
            turn_context.as_ref(),
            &cancellation_token,
            &mentioned_skills,
        )
        .await;

        let otel_manager = turn_context.otel_manager.clone();
        let thread_id = sess.conversation_id.to_string();
        let tracking = build_track_events_context(turn_context.model_info.slug.clone(), thread_id);
        let SkillInjections {
            items: skill_items,
            warnings: skill_warnings,
        } = build_skill_injections(
            &mentioned_skills,
            Some(&otel_manager),
            &sess.services.analytics_events_client,
            tracking.clone(),
        )
        .await;

        for message in skill_warnings {
            sess.send_event(&turn_context, EventMsg::Warning(WarningEvent { message }))
                .await;
        }

        let initial_input_for_turn: ResponseInputItem = ResponseInputItem::from(input.clone());
        let response_item: ResponseItem = initial_input_for_turn.clone().into();
        sess.record_user_prompt_and_emit_turn_item(turn_context.as_ref(), &input, response_item)
            .await;

        if !skill_items.is_empty() {
            sess.record_conversation_items(&turn_context, &skill_items)
                .await;
        }

        (explicit_app_paths, skill_name_counts_lower)
    };

    sess.maybe_start_ghost_snapshot(Arc::clone(&turn_context), cancellation_token.child_token())
        .await;
    let mut last_agent_message: Option<String> = None;
    // Although from the perspective of codex.rs, TurnDiffTracker has the lifecycle of a Task which contains
    // many turns, from the perspective of the user, it is a single turn.
    let turn_diff_tracker = Arc::new(tokio::sync::Mutex::new(TurnDiffTracker::new()));

    let turn_metadata_header = turn_context.resolve_turn_metadata_header().await;
    // `ModelClientSession` is turn-scoped and caches WebSocket + sticky routing state, so we reuse
    // one instance across retries within this turn.
    let mut client_session = sess.services.model_client.new_session();

    let mut pre_compact_notes_state = PreCompactNotesState::Idle;
    let mut pre_compact_notes_attempts: u8 = 0;

    loop {
        // Note that pending_input would be something like a message the user
        // submitted through the UI while the model was running. Though the UI
        // may support this, the model might not.
        //
        // During pre-compact work-notes capture, defer pending input until after compaction so
        // the notes reflect the pre-interruption history and ordering is preserved.
        let pending_input = if matches!(pre_compact_notes_state, PreCompactNotesState::Idle) {
            sess.get_pending_input().await
        } else {
            Vec::new()
        };
        let pending_response_items = pending_input
            .into_iter()
            .map(ResponseItem::from)
            .collect::<Vec<ResponseItem>>();

        if !pending_response_items.is_empty() {
            for response_item in pending_response_items {
                if let Some(TurnItem::UserMessage(user_message)) = parse_turn_item(&response_item) {
                    // todo(aibrahim): move pending input to be UserInput only to keep TextElements. context: https://github.com/openai/codex/pull/10656#discussion_r2765522480
                    sess.record_user_prompt_and_emit_turn_item(
                        turn_context.as_ref(),
                        &user_message.content,
                        response_item,
                    )
                    .await;
                } else {
                    sess.record_conversation_items(
                        &turn_context,
                        std::slice::from_ref(&response_item),
                    )
                    .await;
                }
            }
        }

        // Construct the input that we will send to the model.
        let sampling_request_input: Vec<ResponseItem> = sess.prompt_history().await;

        let sampling_request_input_messages = sampling_request_input
            .iter()
            .filter_map(|item| match parse_turn_item(item) {
                Some(TurnItem::UserMessage(user_message)) => Some(user_message),
                _ => None,
            })
            .map(|user_message| user_message.message())
            .collect::<Vec<String>>();
        let tool_selection = SamplingRequestToolSelection {
            explicit_app_paths: &explicit_app_paths,
            skill_name_counts_lower: &skill_name_counts_lower,
        };
        let tool_execution_mode = match pre_compact_notes_state {
            PreCompactNotesState::Idle => ToolCallExecutionMode::Normal,
            PreCompactNotesState::AwaitingNotes => ToolCallExecutionMode::RejectAll {
                reason: AUTO_COMPACT_WORK_NOTES_TOOL_REJECT_REASON,
            },
        };
        match run_sampling_request(
            Arc::clone(&sess),
            Arc::clone(&turn_context),
            Arc::clone(&turn_diff_tracker),
            &mut client_session,
            turn_metadata_header.as_deref(),
            sampling_request_input,
            tool_selection,
            tool_execution_mode,
            cancellation_token.child_token(),
        )
        .await
        {
            Ok(sampling_request_output) => {
                let SamplingRequestResult {
                    needs_follow_up,
                    last_agent_message: sampling_request_last_agent_message,
                } = sampling_request_output;
                let total_usage_tokens = sess.get_total_token_usage().await;
                let token_limit_reached = total_usage_tokens >= auto_compact_limit;

                let estimated_token_count =
                    sess.get_estimated_token_count(turn_context.as_ref()).await;

                info!(
                    turn_id = %turn_context.sub_id,
                    total_usage_tokens,
                    estimated_token_count = ?estimated_token_count,
                    auto_compact_limit,
                    token_limit_reached,
                    needs_follow_up,
                    "post sampling token usage"
                );

                if matches!(pre_compact_notes_state, PreCompactNotesState::AwaitingNotes) {
                    if let Some(notes) = sampling_request_last_agent_message
                        && is_auto_compact_work_notes_message(&notes)
                    {
                        run_auto_compact(&sess, &turn_context, Some(notes)).await;
                        pre_compact_notes_state = PreCompactNotesState::Idle;
                        pre_compact_notes_attempts = 0;
                        continue;
                    }

                    pre_compact_notes_attempts += 1;
                    if pre_compact_notes_attempts >= AUTO_COMPACT_WORK_NOTES_CAPTURE_MAX_ATTEMPTS {
                        info!(
                            "Work-notes capture yielded no notes; compacting without notes after {pre_compact_notes_attempts} attempts"
                        );
                        run_auto_compact(&sess, &turn_context, None).await;
                        pre_compact_notes_state = PreCompactNotesState::Idle;
                        pre_compact_notes_attempts = 0;
                        continue;
                    }

                    info!(
                        "Work-notes capture yielded no notes; retrying capture ({pre_compact_notes_attempts}/{AUTO_COMPACT_WORK_NOTES_CAPTURE_MAX_ATTEMPTS})"
                    );
                    continue;
                }

                // as long as compaction works well in getting us way below the token limit, we
                // shouldn't worry about being in an infinite loop.
                if token_limit_reached && needs_follow_up {
                    if turn_context.final_output_json_schema.is_some() {
                        run_auto_compact(&sess, &turn_context, None).await;
                        continue;
                    }

                    inject_pre_compact_work_notes_request(&sess, &turn_context).await;
                    pre_compact_notes_state = PreCompactNotesState::AwaitingNotes;
                    pre_compact_notes_attempts = 0;
                    continue;
                }

                if !needs_follow_up {
                    last_agent_message = sampling_request_last_agent_message;
                    sess.hooks()
                        .dispatch(crate::hooks::HookPayload {
                            session_id: sess.conversation_id,
                            cwd: turn_context.cwd.clone(),
                            triggered_at: chrono::Utc::now(),
                            hook_event: HookEvent::AfterAgent {
                                event: HookEventAfterAgent {
                                    thread_id: sess.conversation_id,
                                    turn_id: turn_context.sub_id.clone(),
                                    input_messages: sampling_request_input_messages,
                                    last_assistant_message: last_agent_message.clone(),
                                },
                            },
                        })
                        .await;
                    break;
                }
                continue;
            }
            Err(CodexErr::TurnAborted) => {
                // Aborted turn is reported via a different event.
                break;
            }
            Err(e) if matches!(pre_compact_notes_state, PreCompactNotesState::AwaitingNotes) => {
                info!("Work-notes capture failed; compacting without notes: {e:#}");
                run_auto_compact(&sess, &turn_context, None).await;
                pre_compact_notes_state = PreCompactNotesState::Idle;
                pre_compact_notes_attempts = 0;
                continue;
            }
            Err(CodexErr::InvalidImageRequest()) => {
                let mut state = sess.state.lock().await;
                error_or_panic(
                    "Invalid image detected; sanitizing tool output to prevent poisoning",
                );
                if state.history.replace_last_turn_images("Invalid image") {
                    continue;
                }
                let event = EventMsg::Error(ErrorEvent {
                    message: "Invalid image in your last message. Please remove it and try again."
                        .to_string(),
                    codex_error_info: Some(CodexErrorInfo::BadRequest),
                });
                sess.send_event(&turn_context, event).await;
                break;
            }
            Err(e) => {
                info!("Turn error: {e:#}");
                let event = EventMsg::Error(e.to_error_event(None));
                sess.send_event(&turn_context, event).await;
                // let the user continue the conversation
                break;
            }
        }
    }

    last_agent_message
}

async fn run_auto_compact(
    sess: &Arc<Session>,
    turn_context: &Arc<TurnContext>,
    preserved_work_notes: Option<String>,
) {
    if should_use_remote_compact_task(sess.as_ref(), &turn_context.provider) {
        run_inline_remote_auto_compact_task(
            Arc::clone(sess),
            Arc::clone(turn_context),
            preserved_work_notes,
        )
        .await;
    } else {
        run_inline_auto_compact_task(
            Arc::clone(sess),
            Arc::clone(turn_context),
            preserved_work_notes,
        )
        .await;
    }
}

pub(super) fn history_needs_continuation(history: &[ResponseItem]) -> bool {
    let Some(last_user_index) = history
        .iter()
        .rposition(is_user_turn_boundary_response_item)
    else {
        return false;
    };

    let tail = &history[last_user_index + 1..];
    if tail.is_empty() || first_dangling_tool_call_index(tail).is_some() {
        return true;
    }

    match tail.last() {
        Some(ResponseItem::Message { role, .. }) if role == "assistant" => false,
        Some(ResponseItem::Message { .. }) => true,
        Some(
            ResponseItem::Reasoning { .. }
            | ResponseItem::LocalShellCall { .. }
            | ResponseItem::FunctionCall { .. }
            | ResponseItem::FunctionCallOutput { .. }
            | ResponseItem::CustomToolCall { .. }
            | ResponseItem::CustomToolCallOutput { .. }
            | ResponseItem::WebSearchCall { .. },
        ) => true,
        Some(_) => !tail.iter().any(|item| {
            matches!(
                item,
                ResponseItem::Message { role, .. } if role == "assistant"
            )
        }),
        None => true,
    }
}

pub(super) fn completed_turn_for_review_from_history(
    items: &[ResponseItem],
    cwd: PathBuf,
) -> Option<CompletedTurnForReview> {
    let mut search_end = items.len();
    while let Some(assistant_index) = items[..search_end]
        .iter()
        .rposition(|item| assistant_message_text(item).is_some_and(|text| !text.trim().is_empty()))
    {
        let final_agent_message = assistant_message_text(&items[assistant_index])?
            .trim()
            .to_string();
        let Some((user_index, user_messages)) = items[..assistant_index]
            .iter()
            .enumerate()
            .rev()
            .find_map(|(index, item)| {
                user_message_texts_for_review(item).map(|texts| (index, texts))
            })
        else {
            search_end = assistant_index;
            continue;
        };

        let has_review_synthetic_between = items[user_index + 1..assistant_index]
            .iter()
            .any(is_review_rollout_user_message);
        if has_review_synthetic_between {
            search_end = assistant_index;
            continue;
        }

        return Some(CompletedTurnForReview {
            turn_id: format!("reconstructed-{user_index}-{assistant_index}"),
            cwd,
            user_messages,
            final_agent_message,
        });
    }

    None
}

fn assistant_message_text(item: &ResponseItem) -> Option<String> {
    let ResponseItem::Message { role, content, .. } = item else {
        return None;
    };
    if role != "assistant" {
        return None;
    }
    let text = content
        .iter()
        .filter_map(|item| match item {
            ContentItem::OutputText { text } | ContentItem::InputText { text } => {
                Some(text.as_str())
            }
            ContentItem::InputImage { .. } => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    (!text.trim().is_empty()).then_some(text)
}

fn user_message_texts_for_review(item: &ResponseItem) -> Option<Vec<String>> {
    if !is_user_turn_boundary_response_item(item) || is_review_rollout_user_message(item) {
        return None;
    }
    let ResponseItem::Message { content, .. } = item else {
        return None;
    };
    let texts = content
        .iter()
        .filter_map(|item| match item {
            ContentItem::InputText { text } if !text.trim().is_empty() => Some(text.clone()),
            ContentItem::OutputText { text } if !text.trim().is_empty() => Some(text.clone()),
            ContentItem::InputText { .. }
            | ContentItem::OutputText { .. }
            | ContentItem::InputImage { .. } => None,
        })
        .collect::<Vec<_>>();
    (!texts.is_empty()).then_some(texts)
}

fn is_review_rollout_user_message(item: &ResponseItem) -> bool {
    let ResponseItem::Message { role, content, .. } = item else {
        return false;
    };
    role == "user"
        && content.iter().any(|item| match item {
            ContentItem::InputText { text } | ContentItem::OutputText { text } => {
                text.contains("User initiated a review task.")
            }
            ContentItem::InputImage { .. } => false,
        })
}

pub(super) fn is_user_turn_boundary_response_item(item: &ResponseItem) -> bool {
    crate::context_manager::is_user_turn_boundary(item)
}

pub(super) fn remove_trailing_turn_aborted_marker(items: &mut Vec<ResponseItem>) -> bool {
    if items.last().is_some_and(is_turn_aborted_interrupted_marker) {
        items.pop();
        true
    } else {
        false
    }
}

pub(super) fn trim_incomplete_continuation_tail(items: &mut Vec<ResponseItem>) -> bool {
    let Some(last_user_index) = items.iter().rposition(is_user_turn_boundary_response_item) else {
        return false;
    };

    let tail = &items[last_user_index + 1..];
    let Some(relative_index) = first_dangling_tool_call_index(tail) else {
        return false;
    };

    items.truncate(last_user_index + 1 + relative_index);
    true
}

fn first_dangling_tool_call_index(items: &[ResponseItem]) -> Option<usize> {
    let mut open_calls: HashMap<String, usize> = HashMap::new();
    let mut first_incomplete_local_shell: Option<usize> = None;

    for (index, item) in items.iter().enumerate() {
        match item {
            ResponseItem::FunctionCall { call_id, .. }
            | ResponseItem::CustomToolCall { call_id, .. } => {
                open_calls.entry(call_id.clone()).or_insert(index);
            }
            ResponseItem::FunctionCallOutput { call_id, .. }
            | ResponseItem::CustomToolCallOutput { call_id, .. } => {
                open_calls.remove(call_id);
            }
            ResponseItem::LocalShellCall { status, .. }
                if !matches!(status, codex_protocol::models::LocalShellStatus::Completed) =>
            {
                first_incomplete_local_shell =
                    Some(first_incomplete_local_shell.map_or(index, |current| current.min(index)));
            }
            _ => {}
        }
    }

    open_calls
        .values()
        .copied()
        .chain(first_incomplete_local_shell)
        .min()
}

fn is_turn_aborted_interrupted_marker(item: &ResponseItem) -> bool {
    let ResponseItem::Message { role, content, .. } = item else {
        return false;
    };

    role == "user"
        && content.iter().any(|content_item| {
            matches!(
                content_item,
                ContentItem::InputText { text } if text.starts_with(TURN_ABORTED_OPEN_TAG)
            )
        })
}

pub(super) fn filter_connectors_for_input(
    connectors: Vec<connectors::AppInfo>,
    input: &[ResponseItem],
    explicit_app_paths: &[String],
    skill_name_counts_lower: &HashMap<String, usize>,
) -> Vec<connectors::AppInfo> {
    let user_messages = collect_user_messages(input);
    if user_messages.is_empty() && explicit_app_paths.is_empty() {
        return Vec::new();
    }

    let mentions = collect_tool_mentions_from_messages(&user_messages);
    let mention_names_lower = mentions
        .plain_names
        .iter()
        .map(|name| name.to_ascii_lowercase())
        .collect::<HashSet<String>>();

    let connector_slug_counts = build_connector_slug_counts(&connectors);
    let mut allowed_connector_ids: HashSet<String> = HashSet::new();
    for path in explicit_app_paths
        .iter()
        .chain(mentions.paths.iter())
        .filter(|path| tool_kind_for_path(path) == ToolMentionKind::App)
    {
        if let Some(connector_id) = app_id_from_path(path) {
            allowed_connector_ids.insert(connector_id.to_string());
        }
    }

    connectors
        .into_iter()
        .filter(|connector| {
            connector_inserted_in_messages(
                connector,
                &mention_names_lower,
                &allowed_connector_ids,
                &connector_slug_counts,
                skill_name_counts_lower,
            )
        })
        .collect()
}

fn connector_inserted_in_messages(
    connector: &connectors::AppInfo,
    mention_names_lower: &HashSet<String>,
    allowed_connector_ids: &HashSet<String>,
    connector_slug_counts: &HashMap<String, usize>,
    skill_name_counts_lower: &HashMap<String, usize>,
) -> bool {
    if allowed_connector_ids.contains(&connector.id) {
        return true;
    }

    let mention_slug = connectors::connector_mention_slug(connector);
    let connector_count = connector_slug_counts
        .get(&mention_slug)
        .copied()
        .unwrap_or(0);
    let skill_count = skill_name_counts_lower
        .get(&mention_slug)
        .copied()
        .unwrap_or(0);
    connector_count == 1 && skill_count == 0 && mention_names_lower.contains(&mention_slug)
}

fn filter_codex_apps_mcp_tools(
    mut mcp_tools: HashMap<String, crate::mcp_connection_manager::ToolInfo>,
    connectors: &[connectors::AppInfo],
) -> HashMap<String, crate::mcp_connection_manager::ToolInfo> {
    let allowed: HashSet<&str> = connectors
        .iter()
        .map(|connector| connector.id.as_str())
        .collect();

    mcp_tools.retain(|_, tool| {
        if tool.server_name != CODEX_APPS_MCP_SERVER_NAME {
            return true;
        }
        let Some(connector_id) = codex_apps_connector_id(tool) else {
            return false;
        };
        allowed.contains(connector_id)
    });

    mcp_tools
}

fn codex_apps_connector_id(tool: &crate::mcp_connection_manager::ToolInfo) -> Option<&str> {
    tool.connector_id.as_deref()
}

struct SamplingRequestToolSelection<'a> {
    explicit_app_paths: &'a [String],
    skill_name_counts_lower: &'a HashMap<String, usize>,
}

#[allow(clippy::too_many_arguments)]
#[instrument(level = "trace",
    skip_all,
    fields(
        turn_id = %turn_context.sub_id,
        model = %turn_context.model_info.slug,
        cwd = %turn_context.cwd.display()
    )
)]
async fn run_sampling_request(
    sess: Arc<Session>,
    turn_context: Arc<TurnContext>,
    turn_diff_tracker: SharedTurnDiffTracker,
    client_session: &mut ModelClientSession,
    turn_metadata_header: Option<&str>,
    input: Vec<ResponseItem>,
    tool_selection: SamplingRequestToolSelection<'_>,
    tool_execution_mode: ToolCallExecutionMode,
    cancellation_token: CancellationToken,
) -> CodexResult<SamplingRequestResult> {
    let mcp_connection_manager = sess.services.mcp_connection_manager.read().await;
    let has_mcp_servers = mcp_connection_manager.has_servers();
    let mut mcp_tools = mcp_connection_manager
        .list_all_tools()
        .or_cancel(&cancellation_token)
        .await?;
    drop(mcp_connection_manager);
    let connectors_for_tools = if turn_context.config.features.enabled(Feature::Apps) {
        let connectors = connectors::accessible_connectors_from_mcp_tools(&mcp_tools);
        Some(filter_connectors_for_input(
            connectors,
            &input,
            tool_selection.explicit_app_paths,
            tool_selection.skill_name_counts_lower,
        ))
    } else {
        None
    };
    if let Some(connectors) = connectors_for_tools.as_ref() {
        mcp_tools = filter_codex_apps_mcp_tools(mcp_tools, connectors);
    }
    let router = Arc::new(ToolRouter::from_config(
        &turn_context.tools_config,
        has_mcp_servers.then(|| {
            mcp_tools
                .into_iter()
                .map(|(name, tool)| (name, tool.tool))
                .collect()
        }),
        turn_context.dynamic_tools.as_slice(),
    ));

    let model_supports_parallel = turn_context.model_info.supports_parallel_tool_calls;

    let base_instructions = sess.get_base_instructions().await;

    let prompt = Prompt {
        input,
        tools: router.specs(),
        parallel_tool_calls: model_supports_parallel,
        base_instructions,
        personality: turn_context.personality,
        output_schema: turn_context.final_output_json_schema.clone(),
    };

    let mut retries = 0;
    loop {
        let err = match try_run_sampling_request(
            Arc::clone(&router),
            Arc::clone(&sess),
            Arc::clone(&turn_context),
            client_session,
            turn_metadata_header,
            Arc::clone(&turn_diff_tracker),
            &prompt,
            tool_execution_mode,
            cancellation_token.child_token(),
        )
        .await
        {
            Ok(output) => {
                return Ok(output);
            }
            Err(CodexErr::ContextWindowExceeded) => {
                sess.set_total_tokens_full(&turn_context).await;
                return Err(CodexErr::ContextWindowExceeded);
            }
            Err(CodexErr::UsageLimitReached(e)) => {
                let rate_limits = e.rate_limits.clone();
                if let Some(rate_limits) = rate_limits {
                    sess.update_rate_limits(&turn_context, rate_limits).await;
                }
                return Err(CodexErr::UsageLimitReached(e));
            }
            Err(err) => err,
        };

        if !err.is_retryable() {
            return Err(err);
        }

        // Use the configured provider-specific stream retry budget.
        let max_retries = turn_context.provider.stream_max_retries();
        if retries >= max_retries
            && client_session.try_switch_fallback_transport(&turn_context.otel_manager)
        {
            sess.send_event(
                &turn_context,
                EventMsg::Warning(WarningEvent {
                    message: format!("Falling back from WebSockets to HTTPS transport. {err:#}"),
                }),
            )
            .await;
            retries = 0;
            continue;
        }
        if retries < max_retries {
            retries += 1;
            let delay = match &err {
                CodexErr::Stream(_, requested_delay) => {
                    requested_delay.unwrap_or_else(|| backoff(retries))
                }
                _ => backoff(retries),
            };
            warn!(
                "stream disconnected - retrying sampling request ({retries}/{max_retries} in {delay:?})...",
            );

            // Surface retry information to any UI/front‑end so the
            // user understands what is happening instead of staring
            // at a seemingly frozen screen.
            sess.notify_stream_error(
                &turn_context,
                format!("Reconnecting... {retries}/{max_retries}"),
                err,
            )
            .await;

            tokio::time::sleep(delay).await;
        } else {
            return Err(err);
        }
    }
}

#[derive(Debug)]
struct SamplingRequestResult {
    needs_follow_up: bool,
    last_agent_message: Option<String>,
}

/// Ephemeral per-response state for streaming a single proposed plan.
/// This is intentionally not persisted or stored in session/state since it
/// only exists while a response is actively streaming. The final plan text
/// is extracted from the completed assistant message.
/// Tracks a single proposed plan item across a streaming response.
struct ProposedPlanItemState {
    item_id: String,
    started: bool,
    completed: bool,
}

/// Per-item plan parsers so we can buffer text while detecting `<proposed_plan>`
/// tags without ever mixing buffered lines across item ids.
struct PlanParsers {
    assistant: HashMap<String, ProposedPlanParser>,
}

impl PlanParsers {
    fn new() -> Self {
        Self {
            assistant: HashMap::new(),
        }
    }

    fn assistant_parser_mut(&mut self, item_id: &str) -> &mut ProposedPlanParser {
        self.assistant
            .entry(item_id.to_string())
            .or_insert_with(ProposedPlanParser::new)
    }

    fn take_assistant_parser(&mut self, item_id: &str) -> Option<ProposedPlanParser> {
        self.assistant.remove(item_id)
    }

    fn drain_assistant_parsers(&mut self) -> Vec<(String, ProposedPlanParser)> {
        self.assistant.drain().collect()
    }
}

/// Aggregated state used only while streaming a plan-mode response.
/// Includes per-item parsers, deferred agent message bookkeeping, and the plan item lifecycle.
struct PlanModeStreamState {
    /// Per-item parsers for assistant streams in plan mode.
    plan_parsers: PlanParsers,
    /// Agent message items started by the model but deferred until we see non-plan text.
    pending_agent_message_items: HashMap<String, TurnItem>,
    /// Agent message items whose start notification has been emitted.
    started_agent_message_items: HashSet<String>,
    /// Leading whitespace buffered until we see non-whitespace text for an item.
    leading_whitespace_by_item: HashMap<String, String>,
    /// Tracks plan item lifecycle while streaming plan output.
    plan_item_state: ProposedPlanItemState,
}

impl PlanModeStreamState {
    fn new(turn_id: &str) -> Self {
        Self {
            plan_parsers: PlanParsers::new(),
            pending_agent_message_items: HashMap::new(),
            started_agent_message_items: HashSet::new(),
            leading_whitespace_by_item: HashMap::new(),
            plan_item_state: ProposedPlanItemState::new(turn_id),
        }
    }
}

#[derive(Default)]
struct ProgressTraceStreamState {
    saw_work_category: bool,
    pending_prefill: bool,
    prefill_open: bool,
    reasoning_open: bool,
    gen_open: bool,
}

impl ProgressTraceStreamState {
    async fn mark_work_seen(&mut self, sess: &Session, turn_context: &TurnContext) {
        if self.saw_work_category {
            return;
        }
        self.saw_work_category = true;
        if self.pending_prefill && !self.prefill_open {
            sess.emit_progress_trace(
                turn_context,
                ProgressTraceCategory::Prefill,
                ProgressTraceState::Started,
                None,
                Some("stream"),
            )
            .await;
            self.prefill_open = true;
        }
    }

    async fn on_text_delta(&mut self, sess: &Session, turn_context: &TurnContext) {
        if self.saw_work_category {
            if !self.gen_open {
                sess.emit_progress_trace(
                    turn_context,
                    ProgressTraceCategory::Gen,
                    ProgressTraceState::Started,
                    None,
                    Some("stream"),
                )
                .await;
                self.gen_open = true;
            }
            return;
        }
        self.pending_prefill = true;
    }

    async fn on_reasoning_delta(&mut self, sess: &Session, turn_context: &TurnContext) {
        if !self.reasoning_open {
            sess.emit_progress_trace(
                turn_context,
                ProgressTraceCategory::Reasoning,
                ProgressTraceState::Started,
                None,
                Some("stream"),
            )
            .await;
            self.reasoning_open = true;
        }
    }

    async fn on_reasoning_section_break(&mut self, sess: &Session, turn_context: &TurnContext) {
        if self.reasoning_open {
            sess.emit_progress_trace(
                turn_context,
                ProgressTraceCategory::Reasoning,
                ProgressTraceState::Completed,
                None,
                Some("stream"),
            )
            .await;
            self.reasoning_open = false;
        }
    }

    async fn finalize(&mut self, sess: &Session, turn_context: &TurnContext) {
        if self.reasoning_open {
            sess.emit_progress_trace(
                turn_context,
                ProgressTraceCategory::Reasoning,
                ProgressTraceState::Completed,
                None,
                Some("stream"),
            )
            .await;
            self.reasoning_open = false;
        }
        if self.prefill_open {
            sess.emit_progress_trace(
                turn_context,
                ProgressTraceCategory::Prefill,
                ProgressTraceState::Completed,
                None,
                Some("stream"),
            )
            .await;
            self.prefill_open = false;
        }
        if self.gen_open {
            sess.emit_progress_trace(
                turn_context,
                ProgressTraceCategory::Gen,
                ProgressTraceState::Completed,
                None,
                Some("stream"),
            )
            .await;
            self.gen_open = false;
        } else if self.pending_prefill && !self.saw_work_category {
            // Conversational turns with no work are categorized as generation.
            sess.emit_progress_trace(
                turn_context,
                ProgressTraceCategory::Gen,
                ProgressTraceState::Started,
                None,
                Some("stream"),
            )
            .await;
            sess.emit_progress_trace(
                turn_context,
                ProgressTraceCategory::Gen,
                ProgressTraceState::Completed,
                None,
                Some("stream"),
            )
            .await;
        }
        self.pending_prefill = false;
    }
}

impl ProposedPlanItemState {
    fn new(turn_id: &str) -> Self {
        Self {
            item_id: format!("{turn_id}-plan"),
            started: false,
            completed: false,
        }
    }

    async fn start(&mut self, sess: &Session, turn_context: &TurnContext) {
        if self.started || self.completed {
            return;
        }
        self.started = true;
        let item = TurnItem::Plan(PlanItem {
            id: self.item_id.clone(),
            text: String::new(),
        });
        sess.emit_turn_item_started(turn_context, &item).await;
    }

    async fn push_delta(&mut self, sess: &Session, turn_context: &TurnContext, delta: &str) {
        if self.completed {
            return;
        }
        if delta.is_empty() {
            return;
        }
        let event = PlanDeltaEvent {
            thread_id: sess.conversation_id.to_string(),
            turn_id: turn_context.sub_id.clone(),
            item_id: self.item_id.clone(),
            delta: delta.to_string(),
        };
        sess.send_event(turn_context, EventMsg::PlanDelta(event))
            .await;
    }

    async fn complete_with_text(
        &mut self,
        sess: &Session,
        turn_context: &TurnContext,
        text: String,
    ) {
        if self.completed || !self.started {
            return;
        }
        self.completed = true;
        let item = TurnItem::Plan(PlanItem {
            id: self.item_id.clone(),
            text,
        });
        sess.emit_turn_item_completed(turn_context, item).await;
    }
}

/// In plan mode we defer agent message starts until the parser emits non-plan
/// text. The parser buffers each line until it can rule out a tag prefix, so
/// plan-only outputs never show up as empty assistant messages.
async fn maybe_emit_pending_agent_message_start(
    sess: &Session,
    turn_context: &TurnContext,
    state: &mut PlanModeStreamState,
    item_id: &str,
) {
    if state.started_agent_message_items.contains(item_id) {
        return;
    }
    if let Some(item) = state.pending_agent_message_items.remove(item_id) {
        sess.emit_turn_item_started(turn_context, &item).await;
        state
            .started_agent_message_items
            .insert(item_id.to_string());
    }
}

/// Agent messages are text-only today; concatenate all text entries.
fn agent_message_text(item: &codex_protocol::items::AgentMessageItem) -> String {
    item.content
        .iter()
        .map(|entry| match entry {
            codex_protocol::items::AgentMessageContent::Text { text } => text.as_str(),
        })
        .collect()
}

/// Split the stream into normal assistant text vs. proposed plan content.
/// Normal text becomes AgentMessage deltas; plan content becomes PlanDelta +
/// TurnItem::Plan.
async fn handle_plan_segments(
    sess: &Session,
    turn_context: &TurnContext,
    state: &mut PlanModeStreamState,
    item_id: &str,
    segments: Vec<ProposedPlanSegment>,
) {
    for segment in segments {
        match segment {
            ProposedPlanSegment::Normal(delta) => {
                if delta.is_empty() {
                    continue;
                }
                let has_non_whitespace = delta.chars().any(|ch| !ch.is_whitespace());
                if !has_non_whitespace && !state.started_agent_message_items.contains(item_id) {
                    let entry = state
                        .leading_whitespace_by_item
                        .entry(item_id.to_string())
                        .or_default();
                    entry.push_str(&delta);
                    continue;
                }
                let delta = if !state.started_agent_message_items.contains(item_id) {
                    if let Some(prefix) = state.leading_whitespace_by_item.remove(item_id) {
                        format!("{prefix}{delta}")
                    } else {
                        delta
                    }
                } else {
                    delta
                };
                maybe_emit_pending_agent_message_start(sess, turn_context, state, item_id).await;

                let event = AgentMessageContentDeltaEvent {
                    thread_id: sess.conversation_id.to_string(),
                    turn_id: turn_context.sub_id.clone(),
                    item_id: item_id.to_string(),
                    delta,
                };
                sess.send_event(turn_context, EventMsg::AgentMessageContentDelta(event))
                    .await;
            }
            ProposedPlanSegment::ProposedPlanStart => {
                if !state.plan_item_state.completed {
                    state.plan_item_state.start(sess, turn_context).await;
                }
            }
            ProposedPlanSegment::ProposedPlanDelta(delta) => {
                if !state.plan_item_state.completed {
                    if !state.plan_item_state.started {
                        state.plan_item_state.start(sess, turn_context).await;
                    }
                    state
                        .plan_item_state
                        .push_delta(sess, turn_context, &delta)
                        .await;
                }
            }
            ProposedPlanSegment::ProposedPlanEnd => {}
        }
    }
}

/// Flush any buffered proposed-plan segments when a specific assistant message ends.
async fn flush_proposed_plan_segments_for_item(
    sess: &Session,
    turn_context: &TurnContext,
    state: &mut PlanModeStreamState,
    item_id: &str,
) {
    let Some(mut parser) = state.plan_parsers.take_assistant_parser(item_id) else {
        return;
    };
    let segments = parser.finish();
    if segments.is_empty() {
        return;
    }
    handle_plan_segments(sess, turn_context, state, item_id, segments).await;
}

/// Flush any remaining assistant plan parsers when the response completes.
async fn flush_proposed_plan_segments_all(
    sess: &Session,
    turn_context: &TurnContext,
    state: &mut PlanModeStreamState,
) {
    for (item_id, mut parser) in state.plan_parsers.drain_assistant_parsers() {
        let segments = parser.finish();
        if segments.is_empty() {
            continue;
        }
        handle_plan_segments(sess, turn_context, state, &item_id, segments).await;
    }
}

/// Emit completion for plan items by parsing the finalized assistant message.
async fn maybe_complete_plan_item_from_message(
    sess: &Session,
    turn_context: &TurnContext,
    state: &mut PlanModeStreamState,
    item: &ResponseItem,
) {
    if let ResponseItem::Message { role, content, .. } = item
        && role == "assistant"
    {
        let mut text = String::new();
        for entry in content {
            if let ContentItem::OutputText { text: chunk } = entry {
                text.push_str(chunk);
            }
        }
        if let Some(plan_text) = extract_proposed_plan_text(&text) {
            if !state.plan_item_state.started {
                state.plan_item_state.start(sess, turn_context).await;
            }
            state
                .plan_item_state
                .complete_with_text(sess, turn_context, plan_text)
                .await;
        }
    }
}

/// Emit a completed agent message in plan mode, respecting deferred starts.
async fn emit_agent_message_in_plan_mode(
    sess: &Session,
    turn_context: &TurnContext,
    agent_message: codex_protocol::items::AgentMessageItem,
    state: &mut PlanModeStreamState,
) {
    let agent_message_id = agent_message.id.clone();
    let text = agent_message_text(&agent_message);
    if text.trim().is_empty() {
        state.pending_agent_message_items.remove(&agent_message_id);
        state.started_agent_message_items.remove(&agent_message_id);
        return;
    }

    maybe_emit_pending_agent_message_start(sess, turn_context, state, &agent_message_id).await;

    if !state
        .started_agent_message_items
        .contains(&agent_message_id)
    {
        let start_item = state
            .pending_agent_message_items
            .remove(&agent_message_id)
            .unwrap_or_else(|| {
                TurnItem::AgentMessage(codex_protocol::items::AgentMessageItem {
                    id: agent_message_id.clone(),
                    content: Vec::new(),
                })
            });
        sess.emit_turn_item_started(turn_context, &start_item).await;
        state
            .started_agent_message_items
            .insert(agent_message_id.clone());
    }

    sess.emit_turn_item_completed(turn_context, TurnItem::AgentMessage(agent_message))
        .await;
    state.started_agent_message_items.remove(&agent_message_id);
}

/// Emit completion for a plan-mode turn item, handling agent messages specially.
async fn emit_turn_item_in_plan_mode(
    sess: &Session,
    turn_context: &TurnContext,
    turn_item: TurnItem,
    previously_active_item: Option<&TurnItem>,
    state: &mut PlanModeStreamState,
) {
    match turn_item {
        TurnItem::AgentMessage(agent_message) => {
            emit_agent_message_in_plan_mode(sess, turn_context, agent_message, state).await;
        }
        _ => {
            if previously_active_item.is_none() {
                sess.emit_turn_item_started(turn_context, &turn_item).await;
            }
            sess.emit_turn_item_completed(turn_context, turn_item).await;
        }
    }
}

/// Handle a completed assistant response item in plan mode, returning true if handled.
async fn handle_assistant_item_done_in_plan_mode(
    sess: &Session,
    turn_context: &TurnContext,
    item: &ResponseItem,
    state: &mut PlanModeStreamState,
    previously_active_item: Option<&TurnItem>,
    last_agent_message: &mut Option<String>,
) -> bool {
    if let ResponseItem::Message { role, .. } = item
        && role == "assistant"
    {
        maybe_complete_plan_item_from_message(sess, turn_context, state, item).await;

        if let Some(turn_item) = handle_non_tool_response_item(item, true).await {
            emit_turn_item_in_plan_mode(
                sess,
                turn_context,
                turn_item,
                previously_active_item,
                state,
            )
            .await;
        }

        sess.record_conversation_items(turn_context, std::slice::from_ref(item))
            .await;
        if let Some(agent_message) = last_assistant_message_from_item(item, true) {
            *last_agent_message = Some(agent_message);
        }
        return true;
    }
    false
}

async fn drain_in_flight(
    in_flight: &mut FuturesOrdered<BoxFuture<'static, CodexResult<ResponseInputItem>>>,
    sess: Arc<Session>,
    turn_context: Arc<TurnContext>,
) -> CodexResult<()> {
    while let Some(res) = in_flight.next().await {
        match res {
            Ok(response_input) => {
                sess.record_conversation_items(&turn_context, &[response_input.into()])
                    .await;
            }
            Err(err) => {
                error_or_panic(format!("in-flight tool future failed during drain: {err}"));
            }
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
#[instrument(level = "trace",
    skip_all,
    fields(
        turn_id = %turn_context.sub_id,
        model = %turn_context.model_info.slug
    )
)]
async fn try_run_sampling_request(
    router: Arc<ToolRouter>,
    sess: Arc<Session>,
    turn_context: Arc<TurnContext>,
    client_session: &mut ModelClientSession,
    turn_metadata_header: Option<&str>,
    turn_diff_tracker: SharedTurnDiffTracker,
    prompt: &Prompt,
    tool_execution_mode: ToolCallExecutionMode,
    cancellation_token: CancellationToken,
) -> CodexResult<SamplingRequestResult> {
    let collaboration_mode = sess.current_collaboration_mode().await;
    let rollout_item = RolloutItem::TurnContext(TurnContextItem {
        cwd: turn_context.cwd.clone(),
        approval_policy: turn_context.approval_policy,
        sandbox_policy: turn_context.sandbox_policy.clone(),
        model: turn_context.model_info.slug.clone(),
        personality: turn_context.personality,
        collaboration_mode: Some(collaboration_mode),
        effort: turn_context.reasoning_effort,
        summary: turn_context.reasoning_summary,
        user_instructions: turn_context.user_instructions.clone(),
        developer_instructions: turn_context.developer_instructions.clone(),
        final_output_json_schema: turn_context.final_output_json_schema.clone(),
        truncation_policy: Some(turn_context.truncation_policy.into()),
    });

    feedback_tags!(
        model = turn_context.model_info.slug.clone(),
        approval_policy = turn_context.approval_policy,
        sandbox_policy = turn_context.sandbox_policy,
        effort = turn_context.reasoning_effort,
        auth_mode = sess.services.auth_manager.auth_mode(),
        features = sess.features.enabled_features(),
    );

    sess.persist_rollout_items(&[rollout_item]).await;
    let mut stream = client_session
        .stream(
            prompt,
            &turn_context.model_info,
            &turn_context.otel_manager,
            turn_context.reasoning_effort,
            turn_context.reasoning_summary,
            turn_context.service_tier.clone(),
            turn_metadata_header,
        )
        .instrument(trace_span!("stream_request"))
        .or_cancel(&cancellation_token)
        .await??;

    let tool_runtime = ToolCallRuntime::new(
        Arc::clone(&router),
        Arc::clone(&sess),
        Arc::clone(&turn_context),
        Arc::clone(&turn_diff_tracker),
    )
    .with_execution_mode(tool_execution_mode);
    let mut in_flight: FuturesOrdered<BoxFuture<'static, CodexResult<ResponseInputItem>>> =
        FuturesOrdered::new();
    let mut needs_follow_up = false;
    let mut last_agent_message: Option<String> = None;
    let mut active_item: Option<TurnItem> = None;
    let mut should_emit_turn_diff = false;
    let plan_mode = turn_context.collaboration_mode.mode == ModeKind::Plan;
    let mut plan_mode_state = plan_mode.then(|| PlanModeStreamState::new(&turn_context.sub_id));
    let mut progress_trace_state = ProgressTraceStreamState::default();
    let receiving_span = trace_span!("receiving_stream");
    let outcome: CodexResult<SamplingRequestResult> = loop {
        let handle_responses = trace_span!(
            parent: &receiving_span,
            "handle_responses",
            otel.name = field::Empty,
            tool_name = field::Empty,
            from = field::Empty,
        );

        let event = match stream
            .next()
            .instrument(trace_span!(parent: &handle_responses, "receiving"))
            .or_cancel(&cancellation_token)
            .await
        {
            Ok(event) => event,
            Err(codex_async_utils::CancelErr::Cancelled) => break Err(CodexErr::TurnAborted),
        };

        let event = match event {
            Some(res) => res?,
            None => {
                break Err(CodexErr::Stream(
                    "stream closed before response.completed".into(),
                    None,
                ));
            }
        };

        sess.services
            .otel_manager
            .record_responses(&handle_responses, &event);

        match event {
            ResponseEvent::Created => {}
            ResponseEvent::OutputItemDone(item) => {
                let previously_active_item = active_item.take();
                if matches!(previously_active_item, Some(TurnItem::WebSearch(_))) {
                    progress_trace_state
                        .mark_work_seen(&sess, &turn_context)
                        .await;
                    sess.emit_progress_trace(
                        &turn_context,
                        ProgressTraceCategory::Network,
                        ProgressTraceState::Completed,
                        None,
                        Some("web_search"),
                    )
                    .await;
                }
                if let Some(state) = plan_mode_state.as_mut() {
                    if let Some(previous) = previously_active_item.as_ref() {
                        let item_id = previous.id();
                        if matches!(previous, TurnItem::AgentMessage(_)) {
                            flush_proposed_plan_segments_for_item(
                                &sess,
                                &turn_context,
                                state,
                                &item_id,
                            )
                            .await;
                        }
                    }
                    if handle_assistant_item_done_in_plan_mode(
                        &sess,
                        &turn_context,
                        &item,
                        state,
                        previously_active_item.as_ref(),
                        &mut last_agent_message,
                    )
                    .await
                    {
                        continue;
                    }
                }

                let mut ctx = HandleOutputCtx {
                    sess: sess.clone(),
                    turn_context: turn_context.clone(),
                    tool_runtime: tool_runtime.clone(),
                    cancellation_token: cancellation_token.child_token(),
                };

                let output_result = handle_output_item_done(&mut ctx, item, previously_active_item)
                    .instrument(handle_responses)
                    .await?;
                if let Some(tool_future) = output_result.tool_future {
                    progress_trace_state
                        .mark_work_seen(&sess, &turn_context)
                        .await;
                    in_flight.push_back(tool_future);
                }
                if let Some(agent_message) = output_result.last_agent_message {
                    last_agent_message = Some(agent_message);
                }
                needs_follow_up |= output_result.needs_follow_up;
            }
            ResponseEvent::OutputItemAdded(item) => {
                if let Some(turn_item) = handle_non_tool_response_item(&item, plan_mode).await {
                    if matches!(turn_item, TurnItem::WebSearch(_)) {
                        progress_trace_state
                            .mark_work_seen(&sess, &turn_context)
                            .await;
                        sess.emit_progress_trace(
                            &turn_context,
                            ProgressTraceCategory::Network,
                            ProgressTraceState::Started,
                            None,
                            Some("web_search"),
                        )
                        .await;
                    }
                    if let Some(state) = plan_mode_state.as_mut()
                        && matches!(turn_item, TurnItem::AgentMessage(_))
                    {
                        let item_id = turn_item.id();
                        state
                            .pending_agent_message_items
                            .insert(item_id, turn_item.clone());
                    } else {
                        sess.emit_turn_item_started(&turn_context, &turn_item).await;
                    }
                    active_item = Some(turn_item);
                }
            }
            ResponseEvent::ServerReasoningIncluded(included) => {
                sess.set_server_reasoning_included(included).await;
            }
            ResponseEvent::RateLimits(snapshot) => {
                // Update internal state with latest rate limits, but defer sending until
                // token usage is available to avoid duplicate TokenCount events.
                sess.update_rate_limits(&turn_context, snapshot).await;
            }
            ResponseEvent::ModelsEtag(etag) => {
                // Update internal state with latest models etag
                let config = sess.get_config().await;
                sess.services
                    .models_manager
                    .refresh_if_new_etag(etag, &config)
                    .await;
            }
            ResponseEvent::Completed {
                response_id: _,
                token_usage,
                end_turn,
            } => {
                progress_trace_state.finalize(&sess, &turn_context).await;
                if let Some(state) = plan_mode_state.as_mut() {
                    flush_proposed_plan_segments_all(&sess, &turn_context, state).await;
                }
                sess.update_token_usage_info(&turn_context, token_usage.as_ref())
                    .await;
                should_emit_turn_diff = true;

                if let Some(false) = end_turn {
                    needs_follow_up = true;
                }
                needs_follow_up |= sess.has_pending_input().await;

                break Ok(SamplingRequestResult {
                    needs_follow_up,
                    last_agent_message,
                });
            }
            ResponseEvent::OutputTextDelta(delta) => {
                progress_trace_state
                    .on_text_delta(&sess, &turn_context)
                    .await;
                // In review child threads, suppress assistant text deltas; the
                // UI will show a selection popup from the final ReviewOutput.
                if let Some(active) = active_item.as_ref() {
                    let item_id = active.id();
                    if let Some(state) = plan_mode_state.as_mut()
                        && matches!(active, TurnItem::AgentMessage(_))
                    {
                        let segments = state
                            .plan_parsers
                            .assistant_parser_mut(&item_id)
                            .parse(&delta);
                        handle_plan_segments(&sess, &turn_context, state, &item_id, segments).await;
                    } else {
                        let event = AgentMessageContentDeltaEvent {
                            thread_id: sess.conversation_id.to_string(),
                            turn_id: turn_context.sub_id.clone(),
                            item_id,
                            delta,
                        };
                        sess.send_event(&turn_context, EventMsg::AgentMessageContentDelta(event))
                            .await;
                    }
                } else {
                    error_or_panic("OutputTextDelta without active item".to_string());
                }
            }
            ResponseEvent::ReasoningSummaryDelta {
                delta,
                summary_index,
            } => {
                progress_trace_state
                    .on_reasoning_delta(&sess, &turn_context)
                    .await;
                if let Some(active) = active_item.as_ref() {
                    let event = ReasoningContentDeltaEvent {
                        thread_id: sess.conversation_id.to_string(),
                        turn_id: turn_context.sub_id.clone(),
                        item_id: active.id(),
                        delta,
                        summary_index,
                    };
                    sess.send_event(&turn_context, EventMsg::ReasoningContentDelta(event))
                        .await;
                } else {
                    error_or_panic("ReasoningSummaryDelta without active item".to_string());
                }
            }
            ResponseEvent::ReasoningSummaryPartAdded { summary_index } => {
                progress_trace_state
                    .on_reasoning_section_break(&sess, &turn_context)
                    .await;
                if let Some(active) = active_item.as_ref() {
                    let event =
                        EventMsg::AgentReasoningSectionBreak(AgentReasoningSectionBreakEvent {
                            item_id: active.id(),
                            summary_index,
                        });
                    sess.send_event(&turn_context, event).await;
                } else {
                    error_or_panic("ReasoningSummaryPartAdded without active item".to_string());
                }
            }
            ResponseEvent::ReasoningContentDelta {
                delta,
                content_index,
            } => {
                progress_trace_state
                    .on_reasoning_delta(&sess, &turn_context)
                    .await;
                if let Some(active) = active_item.as_ref() {
                    let event = ReasoningRawContentDeltaEvent {
                        thread_id: sess.conversation_id.to_string(),
                        turn_id: turn_context.sub_id.clone(),
                        item_id: active.id(),
                        delta,
                        content_index,
                    };
                    sess.send_event(&turn_context, EventMsg::ReasoningRawContentDelta(event))
                        .await;
                } else {
                    error_or_panic("ReasoningRawContentDelta without active item".to_string());
                }
            }
        }
    };

    progress_trace_state.finalize(&sess, &turn_context).await;
    drain_in_flight(&mut in_flight, sess.clone(), turn_context.clone()).await?;

    if should_emit_turn_diff {
        let unified_diff = {
            let mut tracker = turn_diff_tracker.lock().await;
            tracker.get_unified_diff()
        };
        if let Ok(Some(unified_diff)) = unified_diff {
            let msg = EventMsg::TurnDiff(TurnDiffEvent { unified_diff });
            sess.clone().send_event(&turn_context, msg).await;
        }
    }

    outcome
}

pub(crate) fn get_last_assistant_message_from_turn(responses: &[ResponseItem]) -> Option<String> {
    responses.iter().rev().find_map(|item| {
        if let ResponseItem::Message { role, content, .. } = item {
            if role == "assistant" {
                content.iter().rev().find_map(|ci| {
                    if let ContentItem::OutputText { text } = ci {
                        Some(text.clone())
                    } else {
                        None
                    }
                })
            } else {
                None
            }
        } else {
            None
        }
    })
}
