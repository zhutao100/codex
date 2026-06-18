//! Protocol event dispatch and app request handlers for ChatWidget.

use super::*;

impl ChatWidget {
    pub(super) fn on_shutdown_complete(&mut self) {
        self.request_immediate_exit();
    }

    pub(super) fn on_turn_diff(&mut self, unified_diff: String) {
        debug!("TurnDiffEvent: {unified_diff}");
        self.refresh_status_line();
    }

    pub(super) fn on_deprecation_notice(&mut self, event: DeprecationNoticeEvent) {
        let DeprecationNoticeEvent { summary, details } = event;
        self.add_to_history(history_cell::new_deprecation_notice(summary, details));
        self.request_redraw();
    }

    pub(super) fn on_background_event(&mut self, message: String) {
        debug!("BackgroundEvent: {message}");
        self.bottom_pane.ensure_status_indicator();
        self.bottom_pane.set_interrupt_hint_visible(true);
        self.set_status_header(message);
    }

    pub(super) fn on_undo_started(&mut self, event: UndoStartedEvent) {
        self.bottom_pane.ensure_status_indicator();
        self.bottom_pane.set_interrupt_hint_visible(false);
        let message = event
            .message
            .unwrap_or_else(|| "Undo in progress...".to_string());
        self.set_status_header(message);
    }

    pub(super) fn on_undo_completed(&mut self, event: UndoCompletedEvent) {
        let UndoCompletedEvent { success, message } = event;
        self.bottom_pane.hide_status_indicator();
        let message = message.unwrap_or_else(|| {
            if success {
                "Undo completed successfully.".to_string()
            } else {
                "Undo failed.".to_string()
            }
        });
        if success {
            self.add_info_message(message, None);
        } else {
            self.add_error_message(message);
        }
    }

    pub(crate) fn handle_codex_event(&mut self, event: Event) {
        let Event { id, msg } = event;
        self.dispatch_event_msg(Some(id), msg, false);
    }

    /// Dispatch a protocol `EventMsg` to the appropriate handler.
    ///
    /// `id` is `Some` for live events and `None` for replayed events from
    /// `replay_initial_messages()`. Callers should treat `None` as a "fake" id
    /// that must not be used to correlate follow-up actions.
    pub(super) fn dispatch_event_msg(
        &mut self,
        id: Option<String>,
        msg: EventMsg,
        from_replay: bool,
    ) {
        let is_stream_error = matches!(&msg, EventMsg::StreamError(_));
        if !is_stream_error {
            self.restore_retry_status_header_if_present();
        }

        match msg {
            EventMsg::AgentMessageDelta(_)
            | EventMsg::PlanDelta(_)
            | EventMsg::AgentReasoningDelta(_)
            | EventMsg::TerminalInteraction(_)
            | EventMsg::ExecCommandOutputDelta(_)
            | EventMsg::ProgressTrace(_) => {}
            _ => {
                tracing::trace!("handle_codex_event: {:?}", msg);
            }
        }

        match msg {
            EventMsg::SessionConfigured(e) => self.on_session_configured(e),
            EventMsg::ThreadNameUpdated(e) => self.on_thread_name_updated(e),
            EventMsg::RuntimeContextActivated(event) => {
                self.on_runtime_context_activated(event.snapshot)
            }
            EventMsg::RuntimeContextUpdated(event) => {
                self.on_runtime_context_updated(event.snapshot)
            }
            EventMsg::RuntimeContextDeactivated(event) => {
                self.on_runtime_context_deactivated(event)
            }
            EventMsg::AgentMessage(AgentMessageEvent { message }) => {
                self.on_agent_message(message, from_replay)
            }
            EventMsg::AgentMessageDelta(AgentMessageDeltaEvent { delta }) => {
                self.on_agent_message_delta(delta)
            }
            EventMsg::PlanDelta(event) => self.on_plan_delta(event.delta),
            EventMsg::AgentReasoningDelta(AgentReasoningDeltaEvent { delta })
            | EventMsg::AgentReasoningRawContentDelta(AgentReasoningRawContentDeltaEvent {
                delta,
            }) => self.on_agent_reasoning_delta(delta),
            EventMsg::AgentReasoning(AgentReasoningEvent { .. }) => self.on_agent_reasoning_final(),
            EventMsg::AgentReasoningRawContent(AgentReasoningRawContentEvent { text }) => {
                self.on_agent_reasoning_delta(text);
                self.on_agent_reasoning_final();
            }
            EventMsg::AgentReasoningSectionBreak(_) => self.on_reasoning_section_break(),
            EventMsg::TurnStarted(_) => {
                let live_turn_id = (!from_replay).then(|| id.clone()).flatten();
                self.on_task_started(live_turn_id);
            }
            EventMsg::TurnComplete(TurnCompleteEvent { last_agent_message }) => {
                self.on_task_complete(last_agent_message, from_replay)
            }
            EventMsg::TokenCount(ev) => {
                self.set_token_info(ev.info);
                self.on_rate_limit_snapshot(ev.rate_limits);
            }
            EventMsg::Warning(WarningEvent { message }) => self.on_warning(message),
            EventMsg::ModelReroute(_) => {}
            EventMsg::ModelVerification(event) => self.on_model_verification(&event.verifications),
            EventMsg::Error(ErrorEvent {
                message,
                codex_error_info,
            }) => {
                if codex_error_info
                    .as_ref()
                    .is_some_and(|info| self.handle_steer_rejected_error(info))
                {
                } else if codex_error_info
                    .as_ref()
                    .is_some_and(|info| matches!(info, CodexErrorInfo::CyberPolicy))
                {
                    self.on_cyber_policy_error();
                } else if let Some(kind) = codex_error_info.as_ref().and_then(rate_limit_error_kind)
                {
                    match kind {
                        RateLimitErrorKind::ModelCap {
                            model,
                            reset_after_seconds,
                        } => self.on_model_cap_error(model, reset_after_seconds),
                        RateLimitErrorKind::UsageLimit | RateLimitErrorKind::Generic => {
                            self.on_error(message)
                        }
                    }
                } else {
                    self.on_error(message);
                }
            }
            EventMsg::McpStartupUpdate(ev) => self.on_mcp_startup_update(ev),
            EventMsg::McpStartupComplete(ev) => self.on_mcp_startup_complete(ev),
            EventMsg::TurnAborted(ev) => match ev.reason {
                TurnAbortReason::Interrupted => {
                    self.on_interrupted_turn(ev.reason);
                }
                TurnAbortReason::Replaced => {
                    self.on_error("Turn aborted: replaced by a new task".to_owned())
                }
                TurnAbortReason::ReviewEnded => {
                    self.on_interrupted_turn(ev.reason);
                }
            },
            EventMsg::TurnPaused(_) => self.on_paused_turn(),
            EventMsg::TurnContinued(_) => {}
            EventMsg::PlanUpdate(update) => self.on_plan_update(update),
            EventMsg::ExecApprovalRequest(ev) => {
                // For replayed events, synthesize an empty id (these should not occur).
                self.on_exec_approval_request(id.unwrap_or_default(), ev)
            }
            EventMsg::ApplyPatchApprovalRequest(ev) => {
                self.on_apply_patch_approval_request(id.unwrap_or_default(), ev)
            }
            EventMsg::ElicitationRequest(ev) => {
                self.on_elicitation_request(ev);
            }
            EventMsg::RequestUserInput(ev) => {
                self.on_request_user_input(ev);
            }
            EventMsg::ProgressTrace(ev) => self.on_progress_trace(ev),
            EventMsg::ExecCommandBegin(ev) => self.on_exec_command_begin(ev),
            EventMsg::TerminalInteraction(delta) => self.on_terminal_interaction(delta),
            EventMsg::ExecCommandOutputDelta(delta) => self.on_exec_command_output_delta(delta),
            EventMsg::PatchApplyBegin(ev) => self.on_patch_apply_begin(ev),
            EventMsg::PatchApplyEnd(ev) => self.on_patch_apply_end(ev),
            EventMsg::ExecCommandEnd(ev) => self.on_exec_command_end(ev),
            EventMsg::ViewImageToolCall(ev) => self.on_view_image_tool_call(ev),
            EventMsg::McpToolCallBegin(ev) => self.on_mcp_tool_call_begin(ev),
            EventMsg::McpToolCallEnd(ev) => self.on_mcp_tool_call_end(ev),
            EventMsg::WebSearchBegin(ev) => self.on_web_search_begin(ev),
            EventMsg::WebSearchEnd(ev) => self.on_web_search_end(ev),
            EventMsg::GetHistoryEntryResponse(ev) => self.on_get_history_entry_response(ev),
            EventMsg::McpListToolsResponse(ev) => self.on_list_mcp_tools(ev),
            EventMsg::ListCustomPromptsResponse(ev) => self.on_list_custom_prompts(ev),
            EventMsg::ListSkillsResponse(ev) => self.on_list_skills(ev),
            EventMsg::ListRemoteSkillsResponse(_) | EventMsg::RemoteSkillDownloaded(_) => {}
            EventMsg::SkillsUpdateAvailable => {
                self.submit_op(Op::ListSkills {
                    cwds: Vec::new(),
                    force_reload: true,
                });
            }
            EventMsg::ShutdownComplete => self.on_shutdown_complete(),
            EventMsg::TurnDiff(TurnDiffEvent { unified_diff }) => self.on_turn_diff(unified_diff),
            EventMsg::DeprecationNotice(ev) => self.on_deprecation_notice(ev),
            EventMsg::BackgroundEvent(BackgroundEventEvent { message }) => {
                self.on_background_event(message)
            }
            EventMsg::UndoStarted(ev) => self.on_undo_started(ev),
            EventMsg::UndoCompleted(ev) => self.on_undo_completed(ev),
            EventMsg::StreamError(StreamErrorEvent {
                message,
                additional_details,
                ..
            }) => self.on_stream_error(message, additional_details),
            EventMsg::UserMessage(ev) => {
                self.on_committed_user_message(ev, from_replay);
            }
            EventMsg::EnteredReviewMode(review_request) => {
                self.on_entered_review_mode(review_request, from_replay)
            }
            EventMsg::ExitedReviewMode(review) => self.on_exited_review_mode(review),
            EventMsg::ContextCompacted(_) => {
                self.on_agent_message("Context compacted".to_owned(), from_replay)
            }
            EventMsg::CollabAgentSpawnBegin(_) => {}
            EventMsg::CollabAgentSpawnEnd(ev) => self.on_collab_event(collab::spawn_end(ev)),
            EventMsg::CollabAgentInteractionBegin(_) => {}
            EventMsg::CollabAgentInteractionEnd(ev) => {
                self.on_collab_event(collab::interaction_end(ev))
            }
            EventMsg::CollabWaitingBegin(ev) => self.on_collab_event(collab::waiting_begin(ev)),
            EventMsg::CollabWaitingEnd(ev) => self.on_collab_event(collab::waiting_end(ev)),
            EventMsg::CollabCloseBegin(_) => {}
            EventMsg::CollabCloseEnd(ev) => self.on_collab_event(collab::close_end(ev)),
            EventMsg::ThreadRolledBack(_) => {
                // Conservatively clear `/copy` state on rollback. The app layer trims visible
                // transcript cells, but we do not maintain rollback-aware raw-markdown history yet,
                // so keeping the previous cache can return content that was just removed.
                self.last_copyable_output = None;
            }
            EventMsg::RawResponseItem(_)
            | EventMsg::ItemStarted(_)
            | EventMsg::AgentMessageContentDelta(_)
            | EventMsg::ReasoningContentDelta(_)
            | EventMsg::ReasoningRawContentDelta(_)
            | EventMsg::DynamicToolCallRequest(_) => {}
            EventMsg::ItemCompleted(event) => {
                if let codex_protocol::items::TurnItem::Plan(plan_item) = event.item {
                    self.on_plan_item_completed(plan_item.text);
                }
            }
        }
    }

    pub(super) fn on_list_mcp_tools(&mut self, ev: McpListToolsResponseEvent) {
        self.add_to_history(history_cell::new_mcp_tools_output(
            &self.config,
            ev.tools,
            ev.resources,
            ev.resource_templates,
            &ev.auth_statuses,
        ));
    }

    pub(super) fn on_list_custom_prompts(&mut self, ev: ListCustomPromptsResponseEvent) {
        let len = ev.custom_prompts.len();
        debug!("received {len} custom prompts");
        // Forward to bottom pane so the slash popup can show them now.
        self.bottom_pane.set_custom_prompts(ev.custom_prompts);
    }

    pub(super) fn on_list_skills(&mut self, ev: ListSkillsResponseEvent) {
        self.set_skills_from_response(&ev);
    }
}
