//! Remote compaction over the ordinary Responses transport.

use std::sync::Arc;
use std::time::Instant;

use crate::Prompt;
use crate::client::ModelClientSession;
use crate::client_common::ResponseEvent;
use crate::client_common::ResponseStream;
use crate::compact::CompactionContext;
use crate::compact::InitialContextInjection;
use crate::compact::SentCompactInput;
use crate::compact::insert_initial_context_before_last_real_user_message;
use crate::compact::preserved_work_notes_message;
use crate::context_manager::estimate_item_token_count;
use crate::error::CodexErr;
use crate::error::Result as CodexResult;
use crate::protocol::EventMsg;
use crate::protocol::TurnStartedEvent;
use crate::session::session::Session;
use crate::session::turn::compaction_tool_specs;
use crate::session::turn_context::TurnContext;
use crate::truncate::approx_token_count;
use crate::util::backoff;
use codex_protocol::items::ContextCompactionItem;
use codex_protocol::items::TurnItem;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use futures::StreamExt;
use serde_json::json;
use tokio_util::sync::CancellationToken;
use tracing::info;

#[path = "compact_remote_v2_history.rs"]
mod history;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CompactionReason {
    UserRequested,
    TokenLimit,
    ModelDownshift,
}

pub(crate) fn source_token_limit(context: &TurnContext) -> Option<i64> {
    context.model_context_window().map(|window| {
        window.saturating_sub(estimate_item_token_count(
            &ResponseItem::CompactionTrigger {},
        ))
    })
}

pub(crate) async fn run_remote_compact_task(
    sess: &Arc<Session>,
    context: &CompactionContext<'_>,
    client_session: Option<&mut ModelClientSession>,
    notes: Option<String>,
    injection: InitialContextInjection,
    reason: CompactionReason,
    cancellation_token: &CancellationToken,
) -> CodexResult<bool> {
    let turn_context = context.request;
    if reason == CompactionReason::UserRequested {
        sess.send_event(
            turn_context,
            EventMsg::TurnStarted(TurnStartedEvent {
                model_context_window: turn_context.model_context_window(),
                collaboration_mode_kind: turn_context.collaboration_mode.mode,
            }),
        )
        .await;
    }
    let mut owned_session;
    let client_session = match client_session {
        Some(session) => session,
        None => {
            owned_session = sess
                .services
                .model_client
                .new_session_with_provider(turn_context.provider.clone());
            &mut owned_session
        }
    };
    let result = run_compact(
        sess,
        context,
        client_session,
        notes,
        injection,
        reason,
        cancellation_token,
    )
    .await;
    if let Err(err) = &result
        && !matches!(err, CodexErr::TurnAborted | CodexErr::Interrupted)
    {
        sess.send_event(
            turn_context,
            EventMsg::Error(err.to_error_event(Some("Error running remote compact task".into()))),
        )
        .await;
    }
    result.map(|()| true)
}

async fn run_compact(
    sess: &Arc<Session>,
    context: &CompactionContext<'_>,
    client_session: &mut ModelClientSession,
    notes: Option<String>,
    injection: InitialContextInjection,
    reason: CompactionReason,
    cancellation_token: &CancellationToken,
) -> CodexResult<()> {
    let turn_context = context.request;
    let replacement_context = context.replacement;
    check_cancelled(cancellation_token)?;
    let started = Instant::now();
    let lifecycle_item = TurnItem::ContextCompaction(ContextCompactionItem::new());
    sess.emit_turn_item_started(turn_context, &lifecycle_item)
        .await;
    let prepared = sess
        .prepare_history_for_compaction(notes, &context.review_frames)
        .await?;
    let input = prepared
        .source_history
        .for_prompt_with_modalities(&turn_context.model_info.input_modalities);
    let base_instructions = sess.get_base_instructions_for_turn(turn_context).await;
    let tools = match &context.notes_tools {
        Some(tools) => tools.clone(),
        None => compaction_tool_specs(sess, turn_context, &input, cancellation_token).await?,
    };
    let prompt = Prompt {
        input,
        verbosity: Some(turn_context.verbosity),
        sampled_controls: turn_context.sampled_controls.clone(),
        helper_fit: Some(context.v2_fit()),
        parallel_tool_calls: turn_context.model_info.supports_parallel_tool_calls,
        base_instructions,
        personality: turn_context.personality,
        output_schema: None,
        ..Default::default()
    }
    .with_tools(tools);
    let metadata = compaction_metadata(
        turn_context.resolve_turn_metadata_header().await.as_deref(),
        reason,
        injection,
    );
    let (output, sent) = request_compaction(
        sess,
        context,
        client_session,
        &prompt,
        &metadata,
        cancellation_token,
    )
    .await?;
    let SentCompactInput {
        items,
        rewritten_outputs,
        estimated_deleted_tokens,
    } = sent;

    // Reserve canonical context even for standalone/pre-turn compaction: the
    // next normal request seeds it. Count notes and tools outside retention.
    let initial_context = sess.build_initial_context(replacement_context).await;
    let notes_item = prepared
        .preserved_work_notes
        .as_deref()
        .map(preserved_work_notes_message);
    // Previous-model requests use that model's instructions, while the
    // replacement must leave room for the selected destination model.
    let replacement_instructions = sess
        .get_base_instructions_for_turn(replacement_context)
        .await;
    let fixed = fixed_context_tokens(&initial_context, &replacement_instructions.text)
        .saturating_add(history::retained_tokens(&output.item))
        .saturating_add(notes_item.as_ref().map_or(0, history::retained_tokens));
    let review_limit = sess.replacement_review_limit(replacement_context).await?;
    let budget_for = |fixed| -> CodexResult<usize> {
        let install = retention_budget(
            fixed,
            replacement_context.model_context_window(),
            replacement_context.model_info.auto_compact_token_limit(),
        )?;
        Ok(review_limit.map_or(install, |limit| install.min(limit.saturating_sub(fixed))))
    };
    // Only retained user messages can add transcript-selected connectors. Start with
    // the mandatory surface, not the (possibly larger) outgoing helper's schemas.
    let minimum = fixed.saturating_add(
        compaction_tool_specs(
            sess,
            replacement_context,
            &initial_context,
            cancellation_token,
        )
        .await?
        .tokens() as usize,
    );
    let mut fixed_tokens = minimum;
    let mut replacement = history::retain_messages(&items, budget_for(minimum)?);
    if replacement_context
        .features
        .enabled(crate::features::Feature::Apps)
    {
        // Recheck once if truncation changes a textual mention. The empty selection
        // is always feasible when an optional connector costs more than all headroom.
        for resized in [false, true] {
            if replacement.is_empty() {
                break;
            }
            let cost = fixed.saturating_add(
                compaction_tool_specs(sess, replacement_context, &replacement, cancellation_token)
                    .await?
                    .tokens() as usize,
            );
            let available = budget_for(cost).unwrap_or(0);
            if replacement
                .iter()
                .map(history::retained_tokens)
                .sum::<usize>()
                <= available
            {
                fixed_tokens = cost;
                break;
            }
            replacement = if resized {
                Vec::new()
            } else {
                history::retain_messages(&items, available)
            };
        }
    }
    let retained_tokens = replacement
        .iter()
        .map(history::retained_tokens)
        .sum::<usize>();
    let retained_images = replacement
        .iter()
        .map(|item| match item {
            ResponseItem::Message { content, .. } => content
                .iter()
                .filter(|item| matches!(item, ContentItem::InputImage { .. }))
                .count(),
            _ => 0,
        })
        .sum::<usize>();
    replacement.push(output.item);
    let reference = match injection {
        InitialContextInjection::DoNotInject => None,
        InitialContextInjection::BeforeLastUserMessage => {
            insert_initial_context_before_last_real_user_message(&mut replacement, initial_context);
            Some(replacement_context.to_turn_context_item())
        }
    };
    replacement.extend(notes_item);
    check_cancelled(cancellation_token)?;

    install_compacted_history(
        sess,
        replacement_context,
        replacement,
        reference,
        lifecycle_item,
        cancellation_token,
    )
    .await?;
    client_session.reset_after_compaction();
    info!(
        turn_id = %turn_context.sub_id,
        response_id = %output.response_id,
        retained_tokens,
        retained_images,
        full_context_tokens = fixed_tokens.saturating_add(retained_tokens),
        rewritten_outputs,
        estimated_deleted_tokens,
        elapsed_ms = started.elapsed().as_millis(),
        "installed remote compaction v2"
    );
    Ok(())
}

async fn install_compacted_history(
    sess: &Arc<Session>,
    turn_context: &Arc<TurnContext>,
    replacement: Vec<ResponseItem>,
    reference: Option<crate::protocol::TurnContextItem>,
    lifecycle_item: TurnItem,
    cancellation_token: &CancellationToken,
) -> CodexResult<()> {
    // Acquire before spawning so task-stop joins this boundary even if its
    // grace timeout aborts the parent task while rollout persistence is blocked.
    let guard = Arc::clone(&sess.compaction_installation).lock_owned().await;
    check_cancelled(cancellation_token)?;
    let install_session = Arc::clone(sess);
    let install_context = Arc::clone(turn_context);
    let install = tokio::spawn(async move {
        let _guard = guard;
        install_session
            .replace_compacted_history(&install_context, replacement, reference, String::new())
            .await;
        install_session
            .emit_turn_item_completed(&install_context, lifecycle_item)
            .await;
    });
    install.await?;
    Ok(())
}

pub(crate) fn fixed_context_tokens(initial_context: &[ResponseItem], base: &str) -> usize {
    initial_context
        .iter()
        .map(history::retained_tokens)
        .sum::<usize>()
        .saturating_add(approx_token_count(base))
}

pub(crate) fn installation_target(window: Option<i64>, auto_limit: Option<i64>) -> Option<usize> {
    window
        .filter(|limit| *limit > 0)
        .into_iter()
        .chain(auto_limit.filter(|limit| *limit > 0).map(|limit| limit - 1))
        .min()
        .map(|limit| usize::try_from(limit).unwrap_or(usize::MAX))
}

fn retention_budget(
    fixed_tokens: usize,
    window: Option<i64>,
    auto_limit: Option<i64>,
) -> CodexResult<usize> {
    if let Some(target) = installation_target(window, auto_limit) {
        if fixed_tokens > target {
            return Err(CodexErr::Fatal(format!(
                "Compaction summary, notes, instructions and tools need about {fixed_tokens} tokens, exceeding the {target}-token context target; increase the limiting context window or automatic compaction threshold"
            )));
        }
        return Ok(history::RETAINED_MESSAGE_TOKEN_BUDGET.min(target - fixed_tokens));
    }
    Ok(history::RETAINED_MESSAGE_TOKEN_BUDGET)
}

fn check_cancelled(token: &CancellationToken) -> CodexResult<()> {
    if token.is_cancelled() {
        Err(CodexErr::TurnAborted)
    } else {
        Ok(())
    }
}

fn compaction_metadata(
    base: Option<&str>,
    reason: CompactionReason,
    injection: InitialContextInjection,
) -> String {
    let mut metadata = base
        .and_then(|base| {
            serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(base).ok()
        })
        .unwrap_or_default();
    let manual = reason == CompactionReason::UserRequested;
    metadata.insert("request_kind".into(), json!("compaction"));
    metadata.insert(
        "compaction".into(),
        json!({
            "trigger": if manual { "manual" } else { "auto" },
            "reason": match reason {
                CompactionReason::UserRequested => "user_requested",
                CompactionReason::TokenLimit => "context_limit",
                CompactionReason::ModelDownshift => "model_downshift",
            },
            "implementation": "responses_compaction_v2",
            "phase": if manual { "standalone_turn" } else {
                match injection {
                    InitialContextInjection::DoNotInject => "pre_turn",
                    InitialContextInjection::BeforeLastUserMessage => "mid_turn",
                }
            },
            "strategy": "memento"
        }),
    );
    serde_json::Value::Object(metadata).to_string()
}

fn retryable(error: &CodexErr) -> bool {
    !error
        .http_status_code_value()
        .is_some_and(|status| (400..500).contains(&status) && !matches!(status, 408 | 429))
        && error.is_retryable()
}

async fn request_compaction(
    sess: &Session,
    source: &CompactionContext<'_>,
    client: &mut ModelClientSession,
    prompt: &Prompt,
    metadata: &str,
    token: &CancellationToken,
) -> CodexResult<(CompactionOutput, SentCompactInput)> {
    let context = source.request;
    client.cache_operation = crate::client::CacheOperation::Compact;
    client.cache_attempt = 0;
    let max_retries = context.provider.stream_max_retries().min(2);
    let mut retries = 0;
    loop {
        client.cache_attempt += 1;
        let attempt = async {
            let mut stream = client
                .stream(
                    prompt,
                    &context.model_info,
                    &context.otel_manager,
                    context.reasoning_effort,
                    context.reasoning_summary,
                    context.service_tier.clone(),
                    Some(metadata),
                )
                .await?;
            let sent = stream
                .compact_input
                .take()
                .ok_or_else(|| CodexErr::Fatal("Missing fitted compact input".into()))?;
            let output = collect_compaction_output(
                sess,
                context,
                stream,
                source.review_frame_tokens(),
                prompt,
            )
            .await?;
            Ok((output, sent))
        };
        let error = tokio::select! {
            biased;
            _ = token.cancelled() => return Err(CodexErr::TurnAborted),
            result = attempt => match result {
                Ok(output) => return Ok(output),
                Err(error) => error,
            }
        };
        if !retryable(&error) {
            return Err(error);
        }
        if retries >= max_retries && client.try_switch_fallback_transport(&context.otel_manager) {
            retries = 0;
            continue;
        }
        if retries >= max_retries {
            return Err(error);
        }
        retries += 1;
        let delay = match &error {
            CodexErr::Stream(_, delay) => delay.unwrap_or_else(|| backoff(retries)),
            _ => backoff(retries),
        };
        context
            .otel_manager
            .counter("codex.compaction.retry", 1, &[("type", "remote_v2")]);
        sess.notify_stream_error(
            context,
            format!("Retrying compaction... {retries}/{max_retries}"),
            error,
        )
        .await;
        tokio::select! {
            biased;
            _ = token.cancelled() => return Err(CodexErr::TurnAborted),
            _ = tokio::time::sleep(delay) => {}
        }
    }
}

#[derive(Debug, PartialEq)]
struct CompactionOutput {
    item: ResponseItem,
    response_id: String,
}

async fn collect_compaction_output(
    sess: &Session,
    context: &TurnContext,
    mut stream: ResponseStream,
    frame_tokens: i64,
    prompt: &Prompt,
) -> CodexResult<CompactionOutput> {
    let mut candidate = None;
    let mut count = 0;
    let mut server_reasoning_included = false;
    while let Some(event) = stream.next().await {
        let event = event?;
        if matches!(&event, ResponseEvent::Completed { .. }) {
            let span = tracing::trace_span!("compact_response", otel.name = tracing::field::Empty);
            context.otel_manager.record_responses(&span, &event);
        }
        match event {
            ResponseEvent::OutputItemDone(item @ ResponseItem::Compaction { .. }) => {
                count += 1;
                if candidate.is_none() {
                    candidate = Some(item);
                }
            }
            ResponseEvent::Completed {
                response_id,
                token_usage,
                ..
            } => {
                sess.update_token_usage_info_with_request_context(
                    context,
                    stream.usage(token_usage.as_ref()),
                    frame_tokens,
                    server_reasoning_included,
                    0,
                    prompt,
                )
                .await;
                if count != 1 {
                    return Err(CodexErr::Fatal(format!(
                        "Remote compaction v2 expected exactly one compaction output, got {count}"
                    )));
                }
                let Some(item) = candidate else {
                    unreachable!("exactly one completed compaction item");
                };
                return Ok(CompactionOutput { item, response_id });
            }
            ResponseEvent::RateLimits(snapshot) => sess.update_rate_limits(context, snapshot).await,
            ResponseEvent::ServerReasoningIncluded(included) => {
                server_reasoning_included = included;
            }
            _ => {}
        }
    }
    Err(CodexErr::Stream(
        "Remote compaction v2 stream closed before response.completed".into(),
        None,
    ))
}

#[cfg(test)]
#[path = "compact_remote_v2_tests.rs"]
mod tests;
