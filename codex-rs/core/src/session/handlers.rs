use super::review::spawn_post_turn_completion_review;
use super::review::spawn_review_thread;
use super::session::Session;
use super::session::SessionSettingsUpdate;
use super::*;

pub(super) async fn submission_loop(
    sess: Arc<Session>,
    config: Arc<Config>,
    rx_sub: Receiver<Submission>,
) {
    // To break out of this loop, send Op::Shutdown.
    while let Ok(sub) = rx_sub.recv().await {
        debug!(?sub, "Submission");
        match sub.op.clone() {
            Op::Interrupt => {
                interrupt(&sess).await;
            }
            Op::Pause => {
                pause(&sess).await;
            }
            Op::Continue => {
                continue_last(&sess, sub.id.clone()).await;
            }
            Op::OverrideTurnContext {
                cwd,
                approval_policy,
                sandbox_policy,
                windows_sandbox_level,
                model,
                effort,
                summary,
                collaboration_mode,
                personality,
                service_tier,
            } => {
                let collaboration_mode = if let Some(collab_mode) = collaboration_mode {
                    collab_mode
                } else {
                    let state = sess.state.lock().await;
                    state.session_configuration.collaboration_mode.with_updates(
                        model.clone(),
                        effort,
                        None,
                    )
                };
                override_turn_context(
                    &sess,
                    sub.id.clone(),
                    SessionSettingsUpdate {
                        cwd,
                        approval_policy,
                        sandbox_policy,
                        windows_sandbox_level,
                        collaboration_mode: Some(collaboration_mode),
                        reasoning_summary: summary,
                        personality,
                        service_tier,
                        ..Default::default()
                    },
                )
                .await;
            }
            Op::UserInput { .. } | Op::UserTurn { .. } => {
                user_input_or_turn(&sess, sub.id.clone(), sub.op).await;
            }
            Op::ExecApproval { id, decision } => {
                exec_approval(&sess, id, decision).await;
            }
            Op::PatchApproval { id, decision } => {
                patch_approval(&sess, id, decision).await;
            }
            Op::UserInputAnswer { id, response } => {
                request_user_input_response(&sess, id, response).await;
            }
            Op::DynamicToolResponse { id, response } => {
                dynamic_tool_response(&sess, id, response).await;
            }
            Op::AddToHistory { text } => {
                add_to_history(&sess, &config, text).await;
            }
            Op::GetHistoryEntryRequest { offset, log_id } => {
                get_history_entry_request(&sess, &config, sub.id.clone(), offset, log_id).await;
            }
            Op::ListMcpTools => {
                list_mcp_tools(&sess, &config, sub.id.clone()).await;
            }
            Op::RefreshMcpServers { config } => {
                refresh_mcp_servers(&sess, config).await;
            }
            Op::ListCustomPrompts => {
                list_custom_prompts(&sess, sub.id.clone()).await;
            }
            Op::ListSkills { cwds, force_reload } => {
                list_skills(&sess, sub.id.clone(), cwds, force_reload).await;
            }
            Op::ListRemoteSkills => {
                list_remote_skills(&sess, &config, sub.id.clone()).await;
            }
            Op::DownloadRemoteSkill {
                hazelnut_id,
                is_preload,
            } => {
                download_remote_skill(&sess, &config, sub.id.clone(), hazelnut_id, is_preload)
                    .await;
            }
            Op::Undo => {
                undo(&sess, sub.id.clone()).await;
            }
            Op::Compact => {
                compact(&sess, sub.id.clone()).await;
            }
            Op::ThreadRollback { num_turns } => {
                thread_rollback(&sess, sub.id.clone(), num_turns).await;
            }
            Op::SetThreadName { name } => {
                set_thread_name(&sess, sub.id.clone(), name).await;
            }
            Op::AutoRenameThread => {
                auto_rename_thread(&sess, sub.id.clone()).await;
            }
            Op::RunUserShellCommand { command } => {
                run_user_shell_command(&sess, sub.id.clone(), command).await;
            }
            Op::ResolveElicitation {
                server_name,
                request_id,
                decision,
            } => {
                resolve_elicitation(&sess, server_name, request_id, decision).await;
            }
            Op::Shutdown => {
                if shutdown(&sess, sub.id.clone()).await {
                    break;
                }
            }
            Op::Review { review_request } => {
                review(&sess, &config, sub.id.clone(), review_request).await;
            }
            Op::ReviewCompletedTurn => {
                review_completed_turn(&sess, sub.id.clone()).await;
            }
            _ => {} // Ignore unknown ops; enum is non_exhaustive to allow extensions.
        }
    }
    debug!("Agent loop exited");
}

use crate::config::Config;

use crate::mcp::auth::compute_auth_statuses;
use crate::mcp::collect_mcp_snapshot_from_manager;
use crate::mcp::effective_mcp_servers;
use crate::review_prompts::resolve_review_request;
use crate::tasks::CompactTask;
use crate::tasks::ContinueTask;
use crate::tasks::RegularTask;
use crate::tasks::UndoTask;
use crate::tasks::UserShellCommandMode;
use crate::tasks::UserShellCommandTask;
use crate::tasks::execute_user_shell_command;
use codex_protocol::custom_prompts::CustomPrompt;
use codex_protocol::protocol::CodexErrorInfo;
use codex_protocol::protocol::ErrorEvent;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ListCustomPromptsResponseEvent;
use codex_protocol::protocol::ListRemoteSkillsResponseEvent;
use codex_protocol::protocol::ListSkillsResponseEvent;
use codex_protocol::protocol::McpServerRefreshConfig;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::RemoteSkillDownloadedEvent;
use codex_protocol::protocol::RemoteSkillSummary;
use codex_protocol::protocol::ReviewDecision;
use codex_protocol::protocol::ReviewRequest;
use codex_protocol::protocol::SkillsListEntry;
use codex_protocol::protocol::ThreadNameUpdatedEvent;
use codex_protocol::protocol::ThreadRolledBackEvent;
use codex_protocol::protocol::TurnAbortReason;
use codex_protocol::protocol::WarningEvent;
use codex_protocol::request_user_input::RequestUserInputResponse;

use crate::context_manager::is_user_turn_boundary;
use codex_protocol::config_types::CollaborationMode;
use codex_protocol::config_types::ModeKind;
use codex_protocol::config_types::Settings;
use codex_protocol::dynamic_tools::DynamicToolResponse;
use codex_protocol::mcp::RequestId as ProtocolRequestId;
use codex_protocol::user_input::UserInput;
use codex_rmcp_client::ElicitationAction;
use codex_rmcp_client::ElicitationResponse;
use std::path::PathBuf;
use std::sync::Arc;
use tracing::info;
use tracing::warn;

pub async fn interrupt(sess: &Arc<Session>) {
    sess.interrupt_task().await;
}

pub async fn pause(sess: &Arc<Session>) {
    sess.pause_task().await;
}

pub async fn continue_last(sess: &Arc<Session>, sub_id: String) {
    if sess.active_turn.lock().await.is_some() {
        sess.send_event_raw(Event {
            id: sub_id,
            msg: EventMsg::Error(ErrorEvent {
                message: "Cannot continue while a task is already running.".to_string(),
                codex_error_info: Some(CodexErrorInfo::BadRequest),
            }),
        })
        .await;
        return;
    }

    let Some(checkpoint) = sess.take_pending_continuation().await else {
        sess.send_event_raw(Event {
            id: sub_id,
            msg: EventMsg::Error(ErrorEvent {
                message: "There is no paused or interrupted turn to continue.".to_string(),
                codex_error_info: Some(CodexErrorInfo::BadRequest),
            }),
        })
        .await;
        return;
    };

    let turn_context = sess.new_default_turn_with_sub_id(sub_id).await;
    sess.spawn_task(
        Arc::clone(&turn_context),
        Vec::new(),
        ContinueTask::new(checkpoint),
    )
    .await;
}

pub async fn override_turn_context(sess: &Session, sub_id: String, updates: SessionSettingsUpdate) {
    if let Err(err) = sess.update_settings(updates).await {
        sess.send_event_raw(Event {
            id: sub_id,
            msg: EventMsg::Error(ErrorEvent {
                message: err.to_string(),
                codex_error_info: Some(CodexErrorInfo::BadRequest),
            }),
        })
        .await;
    }
}

pub async fn user_input_or_turn(sess: &Arc<Session>, sub_id: String, op: Op) {
    let (items, updates) = match op {
        Op::UserTurn {
            cwd,
            approval_policy,
            sandbox_policy,
            model,
            effort,
            summary,
            final_output_json_schema,
            items,
            collaboration_mode,
            personality,
            service_tier,
        } => {
            let collaboration_mode = collaboration_mode.or_else(|| {
                Some(CollaborationMode {
                    mode: ModeKind::Default,
                    settings: Settings {
                        model: model.clone(),
                        reasoning_effort: effort,
                        developer_instructions: None,
                    },
                })
            });
            (
                items,
                SessionSettingsUpdate {
                    cwd: Some(cwd),
                    approval_policy: Some(approval_policy),
                    sandbox_policy: Some(sandbox_policy),
                    windows_sandbox_level: None,
                    collaboration_mode,
                    reasoning_summary: Some(summary),
                    final_output_json_schema: Some(final_output_json_schema),
                    personality,
                    service_tier,
                },
            )
        }
        Op::UserInput {
            items,
            final_output_json_schema,
        } => (
            items,
            SessionSettingsUpdate {
                final_output_json_schema: Some(final_output_json_schema),
                ..Default::default()
            },
        ),
        _ => unreachable!(),
    };

    let Ok(current_context) = sess.new_turn_with_sub_id(sub_id, updates).await else {
        // new_turn_with_sub_id already emits the error event.
        return;
    };
    current_context.otel_manager.user_prompt(&items);

    // Attempt to inject input into current task
    if let Err(items) = sess.inject_input(items).await {
        sess.clear_pending_continuation().await;
        sess.refresh_mcp_servers_if_requested(&current_context)
            .await;
        sess.spawn_task(Arc::clone(&current_context), items, RegularTask)
            .await;
    }
}

pub async fn run_user_shell_command(sess: &Arc<Session>, sub_id: String, command: String) {
    if let Some((turn_context, cancellation_token)) =
        sess.active_turn_context_and_cancellation_token().await
    {
        let session = Arc::clone(sess);
        tokio::spawn(async move {
            execute_user_shell_command(
                session,
                turn_context,
                command,
                cancellation_token,
                UserShellCommandMode::ActiveTurnAuxiliary,
            )
            .await;
        });
        return;
    }

    let turn_context = sess.new_default_turn_with_sub_id(sub_id).await;
    sess.spawn_task(
        Arc::clone(&turn_context),
        Vec::new(),
        UserShellCommandTask::new(command),
    )
    .await;
}

pub async fn resolve_elicitation(
    sess: &Arc<Session>,
    server_name: String,
    request_id: ProtocolRequestId,
    decision: codex_protocol::approvals::ElicitationAction,
) {
    let action = match decision {
        codex_protocol::approvals::ElicitationAction::Accept => ElicitationAction::Accept,
        codex_protocol::approvals::ElicitationAction::Decline => ElicitationAction::Decline,
        codex_protocol::approvals::ElicitationAction::Cancel => ElicitationAction::Cancel,
    };
    // When accepting, send an empty object as content to satisfy MCP servers
    // that expect non-null content on Accept. For Decline/Cancel, content is None.
    let content = match action {
        ElicitationAction::Accept => Some(serde_json::json!({})),
        ElicitationAction::Decline | ElicitationAction::Cancel => None,
    };
    let response = ElicitationResponse { action, content };
    let request_id = match request_id {
        ProtocolRequestId::String(value) => {
            rmcp::model::NumberOrString::String(std::sync::Arc::from(value))
        }
        ProtocolRequestId::Integer(value) => rmcp::model::NumberOrString::Number(value),
    };
    if let Err(err) = sess
        .resolve_elicitation(server_name, request_id, response)
        .await
    {
        warn!(
            error = %err,
            "failed to resolve elicitation request in session"
        );
    }
}

/// Propagate a user's exec approval decision to the session.
/// Also optionally applies an execpolicy amendment.
pub async fn exec_approval(sess: &Arc<Session>, id: String, decision: ReviewDecision) {
    if let ReviewDecision::ApprovedExecpolicyAmendment {
        proposed_execpolicy_amendment,
    } = &decision
    {
        match sess
            .persist_execpolicy_amendment(proposed_execpolicy_amendment)
            .await
        {
            Ok(()) => {
                sess.record_execpolicy_amendment_message(&id, proposed_execpolicy_amendment)
                    .await;
            }
            Err(err) => {
                let message = format!("Failed to apply execpolicy amendment: {err}");
                tracing::warn!("{message}");
                let warning = EventMsg::Warning(WarningEvent { message });
                sess.send_event_raw(Event {
                    id: id.clone(),
                    msg: warning,
                })
                .await;
            }
        }
    }
    match decision {
        ReviewDecision::Abort => {
            sess.interrupt_task().await;
        }
        other => sess.notify_approval(&id, other).await,
    }
}

pub async fn patch_approval(sess: &Arc<Session>, id: String, decision: ReviewDecision) {
    match decision {
        ReviewDecision::Abort => {
            sess.interrupt_task().await;
        }
        other => sess.notify_approval(&id, other).await,
    }
}

pub async fn request_user_input_response(
    sess: &Arc<Session>,
    id: String,
    response: RequestUserInputResponse,
) {
    sess.notify_user_input_response(&id, response).await;
}

pub async fn dynamic_tool_response(sess: &Arc<Session>, id: String, response: DynamicToolResponse) {
    sess.notify_dynamic_tool_response(&id, response).await;
}

pub async fn add_to_history(sess: &Arc<Session>, config: &Arc<Config>, text: String) {
    let id = sess.conversation_id;
    let config = Arc::clone(config);
    tokio::spawn(async move {
        if let Err(e) = crate::message_history::append_entry(&text, &id, &config).await {
            warn!("failed to append to message history: {e}");
        }
    });
}

pub async fn get_history_entry_request(
    sess: &Arc<Session>,
    config: &Arc<Config>,
    sub_id: String,
    offset: usize,
    log_id: u64,
) {
    let config = Arc::clone(config);
    let sess_clone = Arc::clone(sess);

    tokio::spawn(async move {
        // Run lookup in blocking thread because it does file IO + locking.
        let entry_opt = tokio::task::spawn_blocking(move || {
            crate::message_history::lookup(log_id, offset, &config)
        })
        .await
        .unwrap_or(None);

        let event = Event {
            id: sub_id,
            msg: EventMsg::GetHistoryEntryResponse(crate::protocol::GetHistoryEntryResponseEvent {
                offset,
                log_id,
                entry: entry_opt.map(|e| codex_protocol::message_history::HistoryEntry {
                    conversation_id: e.session_id,
                    ts: e.ts,
                    text: e.text,
                }),
            }),
        };

        sess_clone.send_event_raw(event).await;
    });
}

pub async fn refresh_mcp_servers(sess: &Arc<Session>, refresh_config: McpServerRefreshConfig) {
    let mut guard = sess.pending_mcp_server_refresh_config.lock().await;
    *guard = Some(refresh_config);
}

pub async fn list_mcp_tools(sess: &Session, config: &Arc<Config>, sub_id: String) {
    let mcp_connection_manager = sess.services.mcp_connection_manager.read().await;
    let auth = sess.services.auth_manager.auth().await;
    let mcp_servers = effective_mcp_servers(config, auth.as_ref());
    let snapshot = collect_mcp_snapshot_from_manager(
        &mcp_connection_manager,
        compute_auth_statuses(mcp_servers.iter(), config.mcp_oauth_credentials_store_mode).await,
    )
    .await;
    let event = Event {
        id: sub_id,
        msg: EventMsg::McpListToolsResponse(snapshot),
    };
    sess.send_event_raw(event).await;
}

pub async fn list_custom_prompts(sess: &Session, sub_id: String) {
    let custom_prompts: Vec<CustomPrompt> =
        if let Some(dir) = crate::custom_prompts::default_prompts_dir() {
            crate::custom_prompts::discover_prompts_in(&dir).await
        } else {
            Vec::new()
        };

    let event = Event {
        id: sub_id,
        msg: EventMsg::ListCustomPromptsResponse(ListCustomPromptsResponseEvent { custom_prompts }),
    };
    sess.send_event_raw(event).await;
}

pub async fn list_skills(sess: &Session, sub_id: String, cwds: Vec<PathBuf>, force_reload: bool) {
    let cwds = if cwds.is_empty() {
        let state = sess.state.lock().await;
        vec![state.session_configuration.cwd.clone()]
    } else {
        cwds
    };

    let skills_manager = &sess.services.skills_manager;
    let mut skills = Vec::new();
    for cwd in cwds {
        let outcome = skills_manager.skills_for_cwd(&cwd, force_reload).await;
        let errors = super::errors_to_info(&outcome.errors);
        let skills_metadata = super::skills_to_info(&outcome.skills, &outcome.disabled_paths);
        skills.push(SkillsListEntry {
            cwd,
            skills: skills_metadata,
            errors,
        });
    }

    let event = Event {
        id: sub_id,
        msg: EventMsg::ListSkillsResponse(ListSkillsResponseEvent { skills }),
    };
    sess.send_event_raw(event).await;
}

pub async fn list_remote_skills(sess: &Session, config: &Arc<Config>, sub_id: String) {
    let response = crate::skills::remote::list_remote_skills(config)
        .await
        .map(|skills| {
            skills
                .into_iter()
                .map(|skill| RemoteSkillSummary {
                    id: skill.id,
                    name: skill.name,
                    description: skill.description,
                })
                .collect::<Vec<_>>()
        });

    match response {
        Ok(skills) => {
            let event = Event {
                id: sub_id,
                msg: EventMsg::ListRemoteSkillsResponse(ListRemoteSkillsResponseEvent { skills }),
            };
            sess.send_event_raw(event).await;
        }
        Err(err) => {
            let event = Event {
                id: sub_id,
                msg: EventMsg::Error(ErrorEvent {
                    message: format!("failed to list remote skills: {err}"),
                    codex_error_info: Some(CodexErrorInfo::Other),
                }),
            };
            sess.send_event_raw(event).await;
        }
    }
}

pub async fn download_remote_skill(
    sess: &Session,
    config: &Arc<Config>,
    sub_id: String,
    hazelnut_id: String,
    is_preload: bool,
) {
    match crate::skills::remote::download_remote_skill(config, hazelnut_id.as_str(), is_preload)
        .await
    {
        Ok(result) => {
            let event = Event {
                id: sub_id,
                msg: EventMsg::RemoteSkillDownloaded(RemoteSkillDownloadedEvent {
                    id: result.id,
                    name: result.name,
                    path: result.path,
                }),
            };
            sess.send_event_raw(event).await;
        }
        Err(err) => {
            let event = Event {
                id: sub_id,
                msg: EventMsg::Error(ErrorEvent {
                    message: format!("failed to download remote skill {hazelnut_id}: {err}"),
                    codex_error_info: Some(CodexErrorInfo::Other),
                }),
            };
            sess.send_event_raw(event).await;
        }
    }
}

pub async fn undo(sess: &Arc<Session>, sub_id: String) {
    let turn_context = sess.new_default_turn_with_sub_id(sub_id).await;
    sess.spawn_task(turn_context, Vec::new(), UndoTask::new())
        .await;
}

pub async fn compact(sess: &Arc<Session>, sub_id: String) {
    let turn_context = sess.new_default_turn_with_sub_id(sub_id).await;

    sess.spawn_task(
        Arc::clone(&turn_context),
        vec![UserInput::Text {
            text: turn_context.compact_prompt().to_string(),
            // Compaction prompt is synthesized; no UI element ranges to preserve.
            text_elements: Vec::new(),
        }],
        CompactTask,
    )
    .await;
}

pub async fn thread_rollback(sess: &Arc<Session>, sub_id: String, num_turns: u32) {
    if num_turns == 0 {
        sess.send_event_raw(Event {
            id: sub_id,
            msg: EventMsg::Error(ErrorEvent {
                message: "num_turns must be >= 1".to_string(),
                codex_error_info: Some(CodexErrorInfo::ThreadRollbackFailed),
            }),
        })
        .await;
        return;
    }

    let has_active_turn = { sess.active_turn.lock().await.is_some() };
    if has_active_turn {
        sess.send_event_raw(Event {
            id: sub_id,
            msg: EventMsg::Error(ErrorEvent {
                message: "Cannot rollback while a turn is in progress.".to_string(),
                codex_error_info: Some(CodexErrorInfo::ThreadRollbackFailed),
            }),
        })
        .await;
        return;
    }

    let turn_context = sess.new_default_turn_with_sub_id(sub_id).await;

    let mut history = sess.clone_history().await;
    history.drop_last_n_user_turns(num_turns);

    // Replace with the raw items. We don't want to replace with a normalized
    // version of the history.
    sess.replace_history(history.raw_items().to_vec()).await;
    sess.recompute_token_usage(turn_context.as_ref()).await;

    sess.send_event_raw_flushed(Event {
        id: turn_context.sub_id.clone(),
        msg: EventMsg::ThreadRolledBack(ThreadRolledBackEvent { num_turns }),
    })
    .await;
}

/// Persists the thread name in the session index, updates in-memory state, and emits
/// a `ThreadNameUpdated` event on success.
///
/// This appends the name to `CODEX_HOME/session_index.jsonl` via `session_index::append_thread_name` for the
/// current `thread_id`, then updates `SessionConfiguration::thread_name`.
///
/// Returns an error event if the name is empty or session persistence is disabled.
pub async fn set_thread_name(sess: &Arc<Session>, sub_id: String, name: String) {
    let Some(name) = crate::util::normalize_thread_name(&name) else {
        let event = Event {
            id: sub_id,
            msg: EventMsg::Error(ErrorEvent {
                message: "Thread name cannot be empty.".to_string(),
                codex_error_info: Some(CodexErrorInfo::BadRequest),
            }),
        };
        sess.send_event_raw(event).await;
        return;
    };

    if let Err(e) = sess.set_thread_name(name.clone()).await {
        let event = Event {
            id: sub_id,
            msg: EventMsg::Error(ErrorEvent {
                message: format!("Failed to set thread name: {e}"),
                codex_error_info: Some(CodexErrorInfo::Other),
            }),
        };
        sess.send_event_raw(event).await;
        return;
    }

    sess.send_event_raw(Event {
        id: sub_id,
        msg: EventMsg::ThreadNameUpdated(ThreadNameUpdatedEvent {
            thread_id: sess.conversation_id,
            thread_name: Some(name),
        }),
    })
    .await;
}

pub async fn auto_rename_thread(sess: &Arc<Session>, sub_id: String) {
    let turn_context = sess.new_default_turn_with_sub_id(sub_id.clone()).await;
    let thread_name = match crate::thread_name::generate_thread_name(
        sess.as_ref(),
        turn_context.as_ref(),
    )
    .await
    {
        Ok(thread_name) => thread_name,
        Err(err) => {
            sess.send_event_raw(Event {
                id: sub_id,
                msg: EventMsg::Error(ErrorEvent {
                    message: format!("Auto-rename failed: {err}"),
                    codex_error_info: Some(CodexErrorInfo::Other),
                }),
            })
            .await;
            return;
        }
    };

    let Some(thread_name) = thread_name else {
        sess.send_event_raw(Event {
            id: sub_id,
            msg: EventMsg::Error(ErrorEvent {
                message: "Auto-rename failed: empty thread name.".to_string(),
                codex_error_info: Some(CodexErrorInfo::Other),
            }),
        })
        .await;
        return;
    };

    if let Err(err) = sess.set_thread_name(thread_name.clone()).await {
        sess.send_event_raw(Event {
            id: sub_id,
            msg: EventMsg::Error(ErrorEvent {
                message: format!("Auto-rename failed: {err}"),
                codex_error_info: Some(CodexErrorInfo::Other),
            }),
        })
        .await;
        return;
    }

    sess.send_event_raw(Event {
        id: turn_context.sub_id.clone(),
        msg: EventMsg::ThreadNameUpdated(ThreadNameUpdatedEvent {
            thread_id: sess.conversation_id,
            thread_name: Some(thread_name),
        }),
    })
    .await;
}

pub async fn shutdown(sess: &Arc<Session>, sub_id: String) -> bool {
    sess.abort_all_tasks(TurnAbortReason::Interrupted).await;
    sess.services
        .unified_exec_manager
        .terminate_all_processes()
        .await;
    info!("Shutting down Codex instance");
    let history = sess.clone_history().await;
    let turn_count = history
        .raw_items()
        .iter()
        .filter(|item| is_user_turn_boundary(item))
        .count();
    sess.services.otel_manager.counter(
        "codex.conversation.turn.count",
        i64::try_from(turn_count).unwrap_or(0),
        &[],
    );

    // Gracefully flush and shutdown rollout recorder on session end so tests
    // that inspect the rollout file do not race with the background writer.
    let recorder_opt = {
        let mut guard = sess.services.rollout.lock().await;
        guard.take()
    };
    if let Some(rec) = recorder_opt
        && let Err(e) = rec.shutdown().await
    {
        warn!("failed to shutdown rollout recorder: {e}");
        let event = Event {
            id: sub_id.clone(),
            msg: EventMsg::Error(ErrorEvent {
                message: "Failed to shutdown rollout recorder".to_string(),
                codex_error_info: Some(CodexErrorInfo::Other),
            }),
        };
        sess.send_event_raw(event).await;
    }

    let event = Event {
        id: sub_id,
        msg: EventMsg::ShutdownComplete,
    };
    sess.send_event_raw(event).await;
    true
}

pub async fn review(
    sess: &Arc<Session>,
    config: &Arc<Config>,
    sub_id: String,
    review_request: ReviewRequest,
) {
    let turn_context = sess.new_default_turn_with_sub_id(sub_id.clone()).await;
    sess.refresh_mcp_servers_if_requested(&turn_context).await;
    match resolve_review_request(review_request, turn_context.cwd.as_path()) {
        Ok(resolved) => {
            spawn_review_thread(
                Arc::clone(sess),
                Arc::clone(config),
                turn_context.clone(),
                sub_id,
                resolved,
            )
            .await;
        }
        Err(err) => {
            let event = Event {
                id: sub_id,
                msg: EventMsg::Error(ErrorEvent {
                    message: err.to_string(),
                    codex_error_info: Some(CodexErrorInfo::Other),
                }),
            };
            sess.send_event(&turn_context, event.msg).await;
        }
    }
}

pub async fn review_completed_turn(sess: &Arc<Session>, sub_id: String) {
    if sess.active_turn.lock().await.is_some() {
        sess.send_event_raw(Event {
            id: sub_id,
            msg: EventMsg::Error(ErrorEvent {
                message: "Cannot review a completed turn while another task is running."
                    .to_string(),
                codex_error_info: Some(CodexErrorInfo::BadRequest),
            }),
        })
        .await;
        return;
    }

    let Some(completed_turn) = sess.completed_turn_for_review().await else {
        sess.send_event_raw(Event {
            id: sub_id,
            msg: EventMsg::Error(ErrorEvent {
                message: "No completed Codex turn is available to review yet. Run a normal prompt first, wait for Codex to finish, then use /review-completed-turn."
                    .to_string(),
                codex_error_info: Some(CodexErrorInfo::BadRequest),
            }),
        })
        .await;
        return;
    };

    if completed_turn.final_agent_message.trim().is_empty() {
        sess.send_event_raw(Event {
            id: sub_id,
            msg: EventMsg::Error(ErrorEvent {
                message: "The last completed turn has no final assistant message to review. Run another prompt or retry after a completed response."
                    .to_string(),
                codex_error_info: Some(CodexErrorInfo::BadRequest),
            }),
        })
        .await;
        return;
    }

    let turn_context = sess.new_default_turn_with_sub_id(sub_id).await;
    spawn_post_turn_completion_review(Arc::clone(sess), turn_context, completed_turn).await;
}
