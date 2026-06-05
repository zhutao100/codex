use super::*;
use crate::CodexAuth;
use crate::config::ConfigBuilder;
use crate::config::test_config;
use crate::exec::ExecToolCallOutput;
use crate::function_tool::FunctionCallError;
use crate::shell::default_user_shell;
use crate::tools::format_exec_output_str;

use codex_protocol::ThreadId;
use codex_protocol::models::FunctionCallOutputBody;
use codex_protocol::models::FunctionCallOutputPayload;

use crate::protocol::CompactedItem;
use crate::protocol::CreditsSnapshot;
use crate::protocol::InitialHistory;
use crate::protocol::NonSteerableTurnKind;
use crate::protocol::RateLimitSnapshot;
use crate::protocol::RateLimitWindow;
use crate::protocol::ResumedHistory;
use crate::protocol::TokenCountEvent;
use crate::protocol::TokenUsage;
use crate::protocol::TokenUsageInfo;
use crate::state::PendingContinuationTarget;
use crate::state::TaskKind;
use crate::tasks::SessionTask;
use crate::tasks::SessionTaskContext;
use crate::tools::ToolRouter;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolOutput;
use crate::tools::context::ToolPayload;
use crate::tools::handlers::ShellHandler;
use crate::tools::handlers::UnifiedExecHandler;
use crate::tools::registry::ToolHandler;
use crate::turn_diff_tracker::TurnDiffTracker;
use codex_app_server_protocol::AppInfo;
use codex_otel::TelemetryAuthMode;
use codex_protocol::models::BaseInstructions;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseInputItem;
use codex_protocol::models::ResponseItem;
use std::path::Path;
use std::time::Duration;
use tokio::time::sleep;

use codex_protocol::mcp::CallToolResult as McpCallToolResult;
use pretty_assertions::assert_eq;
use serde::Deserialize;
use serde_json::json;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration as StdDuration;

struct InstructionsTestCase {
    slug: &'static str,
}

fn user_message(text: &str) -> ResponseItem {
    ResponseItem::Message {
        id: None,
        role: "user".to_string(),
        content: vec![ContentItem::InputText {
            text: text.to_string(),
        }],
        end_turn: None,
        phase: None,
    }
}

fn assistant_message(text: &str) -> ResponseItem {
    ResponseItem::Message {
        id: None,
        role: "assistant".to_string(),
        content: vec![ContentItem::OutputText {
            text: text.to_string(),
        }],
        end_turn: None,
        phase: None,
    }
}

fn developer_texts(items: &[ResponseItem]) -> Vec<&str> {
    items
        .iter()
        .filter_map(|item| match item {
            ResponseItem::Message { role, content, .. } if role == "developer" => {
                let [ContentItem::InputText { text }] = content.as_slice() else {
                    return None;
                };
                Some(text.as_str())
            }
            _ => None,
        })
        .collect()
}

fn make_connector(id: &str, name: &str) -> AppInfo {
    AppInfo {
        id: id.to_string(),
        name: name.to_string(),
        description: None,
        logo_url: None,
        logo_url_dark: None,
        distribution_channel: None,
        install_url: None,
        is_accessible: true,
    }
}

#[tokio::test]
async fn get_base_instructions_no_user_content() {
    let test_cases = vec![
        InstructionsTestCase { slug: "gpt-5.4" },
        InstructionsTestCase {
            slug: "gpt-5.4-mini",
        },
        InstructionsTestCase {
            slug: "gpt-5.3-codex",
        },
        InstructionsTestCase {
            slug: "gpt-5.3-codex-spark",
        },
        InstructionsTestCase { slug: "gpt-5.2" },
        InstructionsTestCase {
            slug: "codex-auto-review",
        },
    ];

    let (session, _turn_context) = make_session_and_context().await;

    for test_case in test_cases {
        let config = test_config();
        let model_info = ModelsManager::construct_model_info_offline(test_case.slug, &config);

        {
            let mut state = session.state.lock().await;
            state.session_configuration.base_instructions = model_info.base_instructions.clone();
        }

        let base_instructions = session.get_base_instructions().await;
        assert_eq!(base_instructions.text, model_info.base_instructions);
    }
}

#[test]
fn session_base_instructions_use_overlay_final_before_history() {
    let config = test_config();
    let model_info = ModelsManager::construct_model_info_offline("gpt-5.4", &config);
    let history = InitialHistory::Forked(vec![RolloutItem::SessionMeta(
        codex_protocol::protocol::SessionMetaLine {
            meta: codex_protocol::protocol::SessionMeta {
                base_instructions: Some(BaseInstructions {
                    text: "history instructions".to_string(),
                }),
                ..Default::default()
            },
            git: None,
        },
    )]);

    let resolved = resolve_session_base_instructions(
        &config,
        &model_info,
        &history,
        Some("overlay final instructions"),
    );

    assert_eq!(resolved, "overlay final instructions");
}

#[test]
fn session_base_instructions_keep_config_override_strongest() {
    let mut config = test_config();
    config.base_instructions = Some("config instructions".to_string());
    let model_info = ModelsManager::construct_model_info_offline("gpt-5.4", &config);

    let resolved = resolve_session_base_instructions(
        &config,
        &model_info,
        &InitialHistory::New,
        Some("overlay final instructions"),
    );

    assert_eq!(resolved, "config instructions");
}

#[test]
fn filter_connectors_for_input_skips_duplicate_slug_mentions() {
    let connectors = vec![
        make_connector("one", "Foo Bar"),
        make_connector("two", "Foo-Bar"),
    ];
    let input = vec![user_message("use $foo-bar")];
    let explicit_app_paths = Vec::new();
    let skill_name_counts_lower = HashMap::new();

    let selected = filter_connectors_for_input(
        connectors,
        &input,
        &explicit_app_paths,
        &skill_name_counts_lower,
    );

    assert_eq!(selected, Vec::new());
}

#[test]
fn filter_connectors_for_input_skips_when_skill_name_conflicts() {
    let connectors = vec![make_connector("one", "Todoist")];
    let input = vec![user_message("use $todoist")];
    let explicit_app_paths = Vec::new();
    let skill_name_counts_lower = HashMap::from([("todoist".to_string(), 1)]);

    let selected = filter_connectors_for_input(
        connectors,
        &input,
        &explicit_app_paths,
        &skill_name_counts_lower,
    );

    assert_eq!(selected, Vec::new());
}

#[tokio::test]
async fn reconstruct_history_matches_live_compactions() {
    let (session, turn_context) = make_session_and_context().await;
    let (rollout_items, expected) = sample_rollout(&session, &turn_context).await;

    let reconstructed = session
        .reconstruct_history_from_rollout(&turn_context, &rollout_items)
        .await;

    assert_eq!(expected, reconstructed.history);
}

#[tokio::test]
async fn rollout_reconstruction_restores_reference_context_item_after_regular_turn() {
    let (session, turn_context) = make_session_and_context().await;
    let context_item = turn_context.to_turn_context_item();
    let history = vec![user_message("hello"), assistant_message("hi")];
    let rollout_items = vec![
        RolloutItem::TurnContext(context_item.clone()),
        RolloutItem::ResponseItem(history[0].clone()),
        RolloutItem::ResponseItem(history[1].clone()),
    ];

    let reconstructed = session
        .reconstruct_history_from_rollout(&turn_context, &rollout_items)
        .await;

    assert_eq!(
        reconstructed,
        ReconstructedRollout {
            history,
            reference_context_item: Some(context_item.clone()),
            previous_turn_settings: Some(PreviousTurnSettings {
                model: context_item.model
            }),
            pending_continuation: None,
        }
    );
}

#[tokio::test]
async fn rollout_reconstruction_clears_reference_context_item_after_legacy_compaction() {
    let (session, turn_context) = make_session_and_context().await;
    let context_item = turn_context.to_turn_context_item();
    let rollout_items = vec![
        RolloutItem::TurnContext(context_item),
        RolloutItem::ResponseItem(user_message("hello")),
        RolloutItem::ResponseItem(assistant_message("hi")),
        RolloutItem::Compacted(CompactedItem {
            message: "summary".to_string(),
            replacement_history: None,
        }),
    ];
    let expected_history = compact::build_compacted_history(
        session.build_initial_context(&turn_context).await,
        &["hello".to_string()],
        "summary",
    );

    let reconstructed = session
        .reconstruct_history_from_rollout(&turn_context, &rollout_items)
        .await;

    assert_eq!(
        reconstructed,
        ReconstructedRollout {
            history: expected_history,
            reference_context_item: None,
            previous_turn_settings: None,
            pending_continuation: Some(PendingContinuation {
                source: TurnContinuationSource::Interrupted,
                continued_from_turn_id: None,
                model: None,
                pause_reason: None,
                target: PendingContinuationTarget::Regular,
            }),
        }
    );
}

#[tokio::test]
async fn rollout_reconstruction_restores_reference_context_item_after_replacement_history() {
    let (session, turn_context) = make_session_and_context().await;
    let context_item = turn_context.to_turn_context_item();
    let replacement_history = vec![user_message("summary")];
    let rollout_items = vec![
        RolloutItem::Compacted(CompactedItem {
            message: "summary".to_string(),
            replacement_history: Some(replacement_history.clone()),
        }),
        RolloutItem::TurnContext(context_item.clone()),
    ];

    let reconstructed = session
        .reconstruct_history_from_rollout(&turn_context, &rollout_items)
        .await;

    assert_eq!(
        reconstructed,
        ReconstructedRollout {
            history: replacement_history,
            reference_context_item: Some(context_item.clone()),
            previous_turn_settings: Some(PreviousTurnSettings {
                model: context_item.model.clone()
            }),
            pending_continuation: Some(PendingContinuation {
                source: TurnContinuationSource::Interrupted,
                continued_from_turn_id: None,
                model: Some(context_item.model.clone()),
                pause_reason: None,
                target: PendingContinuationTarget::Regular,
            }),
        }
    );
}

#[tokio::test]
async fn rollout_reconstruction_thread_rollback_recomputes_reference_context_item() {
    let (session, turn_context) = make_session_and_context().await;
    let first_context = turn_context.to_turn_context_item();
    let mut second_context = first_context.clone();
    second_context.model = "next-model".to_string();
    let surviving_history = vec![user_message("first"), assistant_message("first reply")];
    let rollout_items = vec![
        RolloutItem::TurnContext(first_context.clone()),
        RolloutItem::ResponseItem(surviving_history[0].clone()),
        RolloutItem::ResponseItem(surviving_history[1].clone()),
        RolloutItem::TurnContext(second_context),
        RolloutItem::ResponseItem(user_message("second")),
        RolloutItem::ResponseItem(assistant_message("second reply")),
        RolloutItem::EventMsg(EventMsg::ThreadRolledBack(ThreadRolledBackEvent {
            num_turns: 1,
        })),
    ];

    let reconstructed = session
        .reconstruct_history_from_rollout(&turn_context, &rollout_items)
        .await;

    assert_eq!(
        reconstructed,
        ReconstructedRollout {
            history: surviving_history,
            reference_context_item: Some(first_context.clone()),
            previous_turn_settings: Some(PreviousTurnSettings {
                model: first_context.model
            }),
            pending_continuation: None,
        }
    );
}

#[tokio::test]
async fn record_initial_history_reconstructs_resumed_transcript() {
    let (session, turn_context) = make_session_and_context().await;
    let (rollout_items, expected) = sample_rollout(&session, &turn_context).await;

    session
        .record_initial_history(InitialHistory::Resumed(ResumedHistory {
            conversation_id: ThreadId::default(),
            history: rollout_items,
            rollout_path: PathBuf::from("/tmp/resume.jsonl"),
        }))
        .await;

    let history = session.state.lock().await.clone_history();
    assert_eq!(expected, history.raw_items());
}

#[tokio::test]
async fn record_initial_history_restores_resumed_reference_context_item() {
    let (session, turn_context) = make_session_and_context().await;
    let context_item = turn_context.to_turn_context_item();
    let rollout_items = vec![
        RolloutItem::TurnContext(context_item.clone()),
        RolloutItem::ResponseItem(user_message("hello")),
        RolloutItem::ResponseItem(assistant_message("hi")),
    ];

    session
        .record_initial_history(InitialHistory::Resumed(ResumedHistory {
            conversation_id: ThreadId::default(),
            history: rollout_items,
            rollout_path: PathBuf::from("/tmp/resume.jsonl"),
        }))
        .await;

    assert_eq!(
        session.reference_context_item().await,
        Some(context_item.clone())
    );
    assert_eq!(
        session.previous_turn_settings().await,
        Some(PreviousTurnSettings {
            model: context_item.model
        })
    );
}

#[test]
fn pending_continuation_from_rollout_uses_incomplete_history_without_pause_event() {
    let original_user = user_message("original request");
    let rollout_items = vec![RolloutItem::ResponseItem(original_user.clone())];
    let reconstructed_history = vec![original_user];

    assert_eq!(
        Some(PendingContinuation {
            source: TurnContinuationSource::Interrupted,
            continued_from_turn_id: None,
            model: None,
            pause_reason: None,
            target: PendingContinuationTarget::Regular,
        }),
        Session::pending_continuation_from_rollout(&rollout_items, &reconstructed_history)
    );
}

#[test]
fn pending_continuation_from_rollout_ignores_custom_pause_event() {
    let original_user = user_message("original request");
    let later_user = user_message("new request");
    let later_assistant = assistant_message("new response");
    let rollout_items = vec![
        RolloutItem::ResponseItem(original_user.clone()),
        RolloutItem::EventMsg(EventMsg::TurnPaused(crate::protocol::TurnPausedEvent {
            turn_id: "turn-1".to_string(),
            reason: crate::protocol::TurnPauseReason::UserRequested,
        })),
        RolloutItem::ResponseItem(later_user.clone()),
        RolloutItem::ResponseItem(later_assistant.clone()),
    ];
    let reconstructed_history = vec![original_user, later_user, later_assistant];

    assert_eq!(
        None,
        Session::pending_continuation_from_rollout(&rollout_items, &reconstructed_history)
    );
}

#[test]
fn history_needs_continuation_after_tool_output_without_final_message() {
    let history = vec![
        user_message("run a tool"),
        assistant_message("I will check."),
        ResponseItem::FunctionCallOutput {
            call_id: "call-1".to_string(),
            output: codex_protocol::models::FunctionCallOutputPayload::from_text(
                "tool result".to_string(),
            ),
        },
    ];

    assert!(history_needs_continuation(&history));
}

#[test]
fn history_needs_continuation_ignores_preserved_work_notes() {
    let history = vec![
        user_message("complete request"),
        assistant_message("done"),
        crate::compact::preserved_work_notes_message("notes"),
    ];

    assert!(!history_needs_continuation(&history));
}

#[test]
fn preserved_work_notes_are_not_user_turn_boundaries() {
    let work_notes = crate::compact::preserved_work_notes_message("notes");

    assert!(!turn::is_user_turn_boundary_response_item(&work_notes));
}

#[test]
fn trims_dangling_tool_call_before_continuation() {
    let mut history = vec![
        user_message("run a tool"),
        assistant_message("I will check."),
        ResponseItem::FunctionCall {
            id: None,
            name: "shell".to_string(),
            arguments: "{}".to_string(),
            call_id: "call-1".to_string(),
        },
    ];

    assert!(trim_incomplete_continuation_tail(&mut history));
    assert_eq!(
        history,
        vec![
            user_message("run a tool"),
            assistant_message("I will check.")
        ]
    );
}

#[tokio::test]
async fn resumed_history_seeds_initial_context_on_first_turn_only() {
    let (session, turn_context) = make_session_and_context().await;
    let (rollout_items, mut expected) = sample_rollout(&session, &turn_context).await;

    session
        .record_initial_history(InitialHistory::Resumed(ResumedHistory {
            conversation_id: ThreadId::default(),
            history: rollout_items,
            rollout_path: PathBuf::from("/tmp/resume.jsonl"),
        }))
        .await;

    let history_before_seed = session.state.lock().await.clone_history();
    assert_eq!(expected, history_before_seed.raw_items());

    session.seed_initial_context_if_needed(&turn_context).await;
    expected.extend(session.build_initial_context(&turn_context).await);
    let history_after_seed = session.clone_history().await;
    assert_eq!(expected, history_after_seed.raw_items());

    session.seed_initial_context_if_needed(&turn_context).await;
    let history_after_second_seed = session.clone_history().await;
    assert_eq!(expected, history_after_second_seed.raw_items());
}

#[tokio::test]
async fn record_initial_history_seeds_token_info_from_rollout() {
    let (session, turn_context) = make_session_and_context().await;
    let (mut rollout_items, _expected) = sample_rollout(&session, &turn_context).await;

    let info1 = TokenUsageInfo {
        total_token_usage: TokenUsage {
            input_tokens: 10,
            cached_input_tokens: 0,
            output_tokens: 20,
            reasoning_output_tokens: 0,
            total_tokens: 30,
        },
        last_token_usage: TokenUsage {
            input_tokens: 3,
            cached_input_tokens: 0,
            output_tokens: 4,
            reasoning_output_tokens: 0,
            total_tokens: 7,
        },
        model_context_window: Some(1_000),
    };
    let info2 = TokenUsageInfo {
        total_token_usage: TokenUsage {
            input_tokens: 100,
            cached_input_tokens: 50,
            output_tokens: 200,
            reasoning_output_tokens: 25,
            total_tokens: 375,
        },
        last_token_usage: TokenUsage {
            input_tokens: 10,
            cached_input_tokens: 0,
            output_tokens: 20,
            reasoning_output_tokens: 5,
            total_tokens: 35,
        },
        model_context_window: Some(2_000),
    };

    rollout_items.push(RolloutItem::EventMsg(EventMsg::TokenCount(
        TokenCountEvent {
            info: Some(info1),
            rate_limits: None,
        },
    )));
    rollout_items.push(RolloutItem::EventMsg(EventMsg::TokenCount(
        TokenCountEvent {
            info: None,
            rate_limits: None,
        },
    )));
    rollout_items.push(RolloutItem::EventMsg(EventMsg::TokenCount(
        TokenCountEvent {
            info: Some(info2.clone()),
            rate_limits: None,
        },
    )));
    rollout_items.push(RolloutItem::EventMsg(EventMsg::TokenCount(
        TokenCountEvent {
            info: None,
            rate_limits: None,
        },
    )));

    session
        .record_initial_history(InitialHistory::Resumed(ResumedHistory {
            conversation_id: ThreadId::default(),
            history: rollout_items,
            rollout_path: PathBuf::from("/tmp/resume.jsonl"),
        }))
        .await;

    let actual = session.state.lock().await.token_info();
    assert_eq!(actual, Some(info2));
}

#[tokio::test]
async fn recompute_token_usage_uses_session_base_instructions() {
    let (session, turn_context) = make_session_and_context().await;

    let override_instructions = "SESSION_OVERRIDE_INSTRUCTIONS_ONLY".repeat(120);
    {
        let mut state = session.state.lock().await;
        state.session_configuration.base_instructions = override_instructions.clone();
    }

    let item = user_message("hello");
    session
        .record_into_history(std::slice::from_ref(&item), &turn_context)
        .await;

    let history = session.clone_history().await;
    let session_base_instructions = BaseInstructions {
        text: override_instructions,
    };
    let expected_tokens = history
        .estimate_token_count_with_base_instructions(&session_base_instructions)
        .expect("estimate with session base instructions");
    let model_estimated_tokens = history
        .estimate_token_count(&turn_context)
        .expect("estimate with model instructions");
    assert_ne!(expected_tokens, model_estimated_tokens);

    session.recompute_token_usage(&turn_context).await;

    let actual_tokens = session
        .state
        .lock()
        .await
        .token_info()
        .expect("token info")
        .last_token_usage
        .total_tokens;
    assert_eq!(actual_tokens, expected_tokens.max(0));
}

#[tokio::test]
async fn record_context_updates_and_set_reference_context_item_injects_full_context_when_baseline_missing()
 {
    let (session, turn_context) = make_session_and_context().await;

    session
        .record_context_updates_and_set_reference_context_item(&turn_context)
        .await;

    let expected_history = session.build_initial_context(&turn_context).await;
    let history = session.clone_history().await;
    assert_eq!(expected_history, history.raw_items());
    assert_eq!(
        session.reference_context_item().await,
        Some(turn_context.to_turn_context_item())
    );
    assert_eq!(
        session.previous_turn_settings().await,
        Some(PreviousTurnSettings {
            model: turn_context.model_info.slug.clone()
        })
    );
}

#[tokio::test]
async fn record_context_updates_and_set_reference_context_item_persists_baseline_without_diff_items()
 {
    let (session, turn_context) = make_session_and_context().await;
    session
        .record_context_updates_and_set_reference_context_item(&turn_context)
        .await;
    let history_after_first = session.clone_history().await;

    session
        .record_context_updates_and_set_reference_context_item(&turn_context)
        .await;

    let history_after_second = session.clone_history().await;
    assert_eq!(
        history_after_first.raw_items(),
        history_after_second.raw_items()
    );
    assert_eq!(
        session.reference_context_item().await,
        Some(turn_context.to_turn_context_item())
    );
}

#[tokio::test]
async fn record_context_updates_keeps_model_switch_when_legacy_baseline_needs_full_context() {
    let (session, turn_context) = make_session_and_context().await;
    let mut previous = turn_context.to_turn_context_item();
    previous.model = "previous-model".to_string();
    previous.collaboration_mode = None;

    {
        let mut state = session.state.lock().await;
        state
            .history
            .set_reference_context_item(Some(previous.clone()));
        state.previous_turn_settings = Some(PreviousTurnSettings {
            model: previous.model.clone(),
        });
        state.initial_context_seeded = true;
    }

    session
        .record_context_updates_and_set_reference_context_item(&turn_context)
        .await;

    let history = session.clone_history().await;
    let developer_texts = developer_texts(history.raw_items());
    let model_switch_count = developer_texts
        .iter()
        .filter(|text| text.contains("<model_switch>"))
        .count();
    assert_eq!(model_switch_count, 1);
    assert!(developer_texts[0].starts_with("<model_switch>"));
    assert!(
        developer_texts
            .iter()
            .any(|text| text.starts_with("<permissions instructions>")),
        "expected full context permissions message, got {developer_texts:?}"
    );
}

#[tokio::test]
async fn build_settings_update_items_emits_model_switch_before_other_developer_diffs() {
    use crate::protocol::AskForApproval;

    let (session, turn_context) = make_session_and_context().await;
    let mut previous = turn_context.to_turn_context_item();
    previous.model = "gpt-5.4-mini".to_string();
    previous.approval_policy = match turn_context.approval_policy {
        AskForApproval::Never => AskForApproval::OnRequest,
        AskForApproval::UnlessTrusted | AskForApproval::OnFailure | AskForApproval::OnRequest => {
            AskForApproval::Never
        }
    };
    let previous_settings = PreviousTurnSettings {
        model: previous.model.clone(),
    };

    let update_items =
        session.build_settings_update_items(&previous, Some(&previous_settings), &turn_context);
    let developer_texts = developer_texts(&update_items);

    assert!(
        developer_texts.len() >= 2,
        "expected model-switch and permissions developer updates"
    );
    assert!(developer_texts[0].starts_with("<model_switch>"));
    assert!(developer_texts[1].starts_with("<permissions instructions>"));
}

#[tokio::test]
async fn record_initial_history_reconstructs_forked_transcript() {
    let (session, turn_context) = make_session_and_context().await;
    let (rollout_items, expected) = sample_rollout(&session, &turn_context).await;

    session
        .record_initial_history(InitialHistory::Forked(rollout_items))
        .await;

    let history = session.state.lock().await.clone_history();
    assert_eq!(expected, history.raw_items());
}

#[tokio::test]
async fn replace_compacted_history_sets_reference_context_item() {
    let (session, turn_context) = make_session_and_context().await;
    let reference_context_item = turn_context.to_turn_context_item();
    let replacement_history = vec![user_message("summary")];
    let compacted_item = CompactedItem {
        message: "summary".to_string(),
        replacement_history: Some(replacement_history.clone()),
    };

    session
        .replace_compacted_history(
            &turn_context,
            replacement_history.clone(),
            Some(reference_context_item.clone()),
            compacted_item,
        )
        .await;

    let history = session.clone_history().await;
    assert_eq!(replacement_history.as_slice(), history.raw_items());
    assert_eq!(
        session.reference_context_item().await,
        Some(reference_context_item.clone())
    );
    assert_eq!(
        session.previous_turn_settings().await,
        Some(PreviousTurnSettings {
            model: reference_context_item.model
        })
    );
}

#[tokio::test]
async fn replace_compacted_history_clears_reference_context_item() {
    let (session, turn_context) = make_session_and_context().await;
    session
        .record_context_updates_and_set_reference_context_item(&turn_context)
        .await;
    let replacement_history = vec![user_message("summary")];
    let compacted_item = CompactedItem {
        message: "summary".to_string(),
        replacement_history: Some(replacement_history.clone()),
    };

    session
        .replace_compacted_history(&turn_context, replacement_history, None, compacted_item)
        .await;

    assert_eq!(session.reference_context_item().await, None);
    assert_eq!(session.previous_turn_settings().await, None);
}

#[tokio::test]
async fn thread_rollback_drops_last_turn_from_history() {
    let (sess, tc, rx) = make_session_and_context_with_rx().await;

    let initial_context = sess.build_initial_context(tc.as_ref()).await;
    sess.record_into_history(&initial_context, tc.as_ref())
        .await;

    let turn_1 = vec![
        ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: "turn 1 user".to_string(),
            }],
            end_turn: None,
            phase: None,
        },
        ResponseItem::Message {
            id: None,
            role: "assistant".to_string(),
            content: vec![ContentItem::OutputText {
                text: "turn 1 assistant".to_string(),
            }],
            end_turn: None,
            phase: None,
        },
    ];
    sess.record_into_history(&turn_1, tc.as_ref()).await;

    let turn_2 = vec![
        ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: "turn 2 user".to_string(),
            }],
            end_turn: None,
            phase: None,
        },
        ResponseItem::Message {
            id: None,
            role: "assistant".to_string(),
            content: vec![ContentItem::OutputText {
                text: "turn 2 assistant".to_string(),
            }],
            end_turn: None,
            phase: None,
        },
    ];
    sess.record_into_history(&turn_2, tc.as_ref()).await;

    handlers::thread_rollback(&sess, "sub-1".to_string(), 1).await;

    let rollback_event = wait_for_thread_rolled_back(&rx).await;
    assert_eq!(rollback_event.num_turns, 1);

    let mut expected = Vec::new();
    expected.extend(initial_context);
    expected.extend(turn_1);

    let history = sess.clone_history().await;
    assert_eq!(expected, history.raw_items());
}

#[tokio::test]
async fn thread_rollback_clears_reference_context_item_when_rollback_crosses_baseline() {
    let (sess, tc, rx) = make_session_and_context_with_rx().await;
    let context_item = tc.to_turn_context_item();

    {
        let mut state = sess.state.lock().await;
        state
            .history
            .set_reference_context_item(Some(context_item.clone()));
        state.previous_turn_settings = Some(PreviousTurnSettings {
            model: context_item.model,
        });
        state.initial_context_seeded = true;
    }

    let turn = vec![
        user_message("turn user"),
        assistant_message("turn assistant"),
    ];
    sess.record_into_history(&turn, tc.as_ref()).await;

    handlers::thread_rollback(&sess, "sub-1".to_string(), 1).await;

    let rollback_event = wait_for_thread_rolled_back(&rx).await;
    assert_eq!(rollback_event.num_turns, 1);
    assert_eq!(sess.reference_context_item().await, None);
    assert_eq!(sess.previous_turn_settings().await, None);
}

#[tokio::test]
async fn thread_rollback_clears_history_when_num_turns_exceeds_existing_turns() {
    let (sess, tc, rx) = make_session_and_context_with_rx().await;

    let initial_context = sess.build_initial_context(tc.as_ref()).await;
    sess.record_into_history(&initial_context, tc.as_ref())
        .await;

    let turn_1 = vec![ResponseItem::Message {
        id: None,
        role: "user".to_string(),
        content: vec![ContentItem::InputText {
            text: "turn 1 user".to_string(),
        }],
        end_turn: None,
        phase: None,
    }];
    sess.record_into_history(&turn_1, tc.as_ref()).await;

    handlers::thread_rollback(&sess, "sub-1".to_string(), 99).await;

    let rollback_event = wait_for_thread_rolled_back(&rx).await;
    assert_eq!(rollback_event.num_turns, 99);

    let history = sess.clone_history().await;
    assert_eq!(initial_context, history.raw_items());
}

#[tokio::test]
async fn thread_rollback_fails_when_turn_in_progress() {
    let (sess, tc, rx) = make_session_and_context_with_rx().await;

    let initial_context = sess.build_initial_context(tc.as_ref()).await;
    sess.record_into_history(&initial_context, tc.as_ref())
        .await;

    *sess.active_turn.lock().await = Some(crate::state::ActiveTurn::default());
    handlers::thread_rollback(&sess, "sub-1".to_string(), 1).await;

    let error_event = wait_for_thread_rollback_failed(&rx).await;
    assert_eq!(
        error_event.codex_error_info,
        Some(CodexErrorInfo::ThreadRollbackFailed)
    );

    let history = sess.clone_history().await;
    assert_eq!(initial_context, history.raw_items());
}

#[tokio::test]
async fn thread_rollback_fails_when_num_turns_is_zero() {
    let (sess, tc, rx) = make_session_and_context_with_rx().await;

    let initial_context = sess.build_initial_context(tc.as_ref()).await;
    sess.record_into_history(&initial_context, tc.as_ref())
        .await;

    handlers::thread_rollback(&sess, "sub-1".to_string(), 0).await;

    let error_event = wait_for_thread_rollback_failed(&rx).await;
    assert_eq!(error_event.message, "num_turns must be >= 1");
    assert_eq!(
        error_event.codex_error_info,
        Some(CodexErrorInfo::ThreadRollbackFailed)
    );

    let history = sess.clone_history().await;
    assert_eq!(initial_context, history.raw_items());
}

#[tokio::test]
async fn set_rate_limits_retains_previous_credits() {
    let codex_home = tempfile::tempdir().expect("create temp dir");
    let config = build_test_config(codex_home.path()).await;
    let config = Arc::new(config);
    let model = ModelsManager::get_model_offline(config.model.as_deref());
    let model_info = ModelsManager::construct_model_info_offline(model.as_str(), &config);
    let reasoning_effort = config.model_reasoning_effort;
    let collaboration_mode = CollaborationMode {
        mode: ModeKind::Default,
        settings: Settings {
            model,
            reasoning_effort,
            developer_instructions: None,
        },
    };
    let session_configuration = SessionConfiguration {
        provider_id: config.model_provider_id.clone(),
        provider: config.model_provider.clone(),
        collaboration_mode,
        model_reasoning_summary: config.model_reasoning_summary,
        service_tier: config.service_tier.clone(),
        developer_instructions: config.developer_instructions.clone(),
        user_instructions: config.user_instructions.clone(),
        personality: config.personality,
        base_instructions: config
            .base_instructions
            .clone()
            .unwrap_or_else(|| model_info.get_model_instructions(config.personality)),
        compact_prompt: config.compact_prompt.clone(),
        approval_policy: config.approval_policy.clone(),
        sandbox_policy: config.sandbox_policy.clone(),
        windows_sandbox_level: WindowsSandboxLevel::from_config(&config),
        cwd: config.cwd.clone(),
        codex_home: config.codex_home.clone(),
        thread_name: None,
        original_config_do_not_use: Arc::clone(&config),
        session_source: SessionSource::Exec,
        dynamic_tools: Vec::new(),
    };

    let mut state = SessionState::new(session_configuration);
    let initial = RateLimitSnapshot {
        primary: Some(RateLimitWindow {
            used_percent: 10.0,
            window_minutes: Some(15),
            resets_at: Some(1_700),
        }),
        secondary: None,
        credits: Some(CreditsSnapshot {
            has_credits: true,
            unlimited: false,
            balance: Some("10.00".to_string()),
        }),
        plan_type: Some(codex_protocol::account::PlanType::Plus),
    };
    state.set_rate_limits(initial.clone());

    let update = RateLimitSnapshot {
        primary: Some(RateLimitWindow {
            used_percent: 40.0,
            window_minutes: Some(30),
            resets_at: Some(1_800),
        }),
        secondary: Some(RateLimitWindow {
            used_percent: 5.0,
            window_minutes: Some(60),
            resets_at: Some(1_900),
        }),
        credits: None,
        plan_type: None,
    };
    state.set_rate_limits(update.clone());

    assert_eq!(
        state.latest_rate_limits,
        Some(RateLimitSnapshot {
            primary: update.primary.clone(),
            secondary: update.secondary,
            credits: initial.credits,
            plan_type: initial.plan_type,
        })
    );
}

#[tokio::test]
async fn set_rate_limits_updates_plan_type_when_present() {
    let codex_home = tempfile::tempdir().expect("create temp dir");
    let config = build_test_config(codex_home.path()).await;
    let config = Arc::new(config);
    let model = ModelsManager::get_model_offline(config.model.as_deref());
    let model_info = ModelsManager::construct_model_info_offline(model.as_str(), &config);
    let reasoning_effort = config.model_reasoning_effort;
    let collaboration_mode = CollaborationMode {
        mode: ModeKind::Default,
        settings: Settings {
            model,
            reasoning_effort,
            developer_instructions: None,
        },
    };
    let session_configuration = SessionConfiguration {
        provider_id: config.model_provider_id.clone(),
        provider: config.model_provider.clone(),
        collaboration_mode,
        model_reasoning_summary: config.model_reasoning_summary,
        service_tier: config.service_tier.clone(),
        developer_instructions: config.developer_instructions.clone(),
        user_instructions: config.user_instructions.clone(),
        personality: config.personality,
        base_instructions: config
            .base_instructions
            .clone()
            .unwrap_or_else(|| model_info.get_model_instructions(config.personality)),
        compact_prompt: config.compact_prompt.clone(),
        approval_policy: config.approval_policy.clone(),
        sandbox_policy: config.sandbox_policy.clone(),
        windows_sandbox_level: WindowsSandboxLevel::from_config(&config),
        cwd: config.cwd.clone(),
        codex_home: config.codex_home.clone(),
        thread_name: None,
        original_config_do_not_use: Arc::clone(&config),
        session_source: SessionSource::Exec,
        dynamic_tools: Vec::new(),
    };

    let mut state = SessionState::new(session_configuration);
    let initial = RateLimitSnapshot {
        primary: Some(RateLimitWindow {
            used_percent: 15.0,
            window_minutes: Some(20),
            resets_at: Some(1_600),
        }),
        secondary: Some(RateLimitWindow {
            used_percent: 5.0,
            window_minutes: Some(45),
            resets_at: Some(1_650),
        }),
        credits: Some(CreditsSnapshot {
            has_credits: true,
            unlimited: false,
            balance: Some("15.00".to_string()),
        }),
        plan_type: Some(codex_protocol::account::PlanType::Plus),
    };
    state.set_rate_limits(initial.clone());

    let update = RateLimitSnapshot {
        primary: Some(RateLimitWindow {
            used_percent: 35.0,
            window_minutes: Some(25),
            resets_at: Some(1_700),
        }),
        secondary: None,
        credits: None,
        plan_type: Some(codex_protocol::account::PlanType::Pro),
    };
    state.set_rate_limits(update.clone());

    assert_eq!(
        state.latest_rate_limits,
        Some(RateLimitSnapshot {
            primary: update.primary,
            secondary: update.secondary,
            credits: initial.credits,
            plan_type: update.plan_type,
        })
    );
}

#[test]
fn prefers_structured_content_when_present() {
    let ctr = McpCallToolResult {
        // Content present but should be ignored because structured_content is set.
        content: vec![text_block("ignored")],
        is_error: None,
        structured_content: Some(json!({
            "ok": true,
            "value": 42
        })),
        meta: None,
    };

    let got = FunctionCallOutputPayload::from(&ctr);
    let expected = FunctionCallOutputPayload {
        body: FunctionCallOutputBody::Text(
            serde_json::to_string(&json!({
                "ok": true,
                "value": 42
            }))
            .unwrap(),
        ),
        success: Some(true),
    };

    assert_eq!(expected, got);
}

#[tokio::test]
async fn includes_timed_out_message() {
    let exec = ExecToolCallOutput {
        exit_code: 0,
        stdout: StreamOutput::new(String::new()),
        stderr: StreamOutput::new(String::new()),
        aggregated_output: StreamOutput::new("Command output".to_string()),
        duration: StdDuration::from_secs(1),
        timed_out: true,
    };
    let (_, turn_context) = make_session_and_context().await;

    let out = format_exec_output_str(&exec, turn_context.truncation_policy);

    assert_eq!(
        out,
        "command timed out after 1000 milliseconds\nCommand output"
    );
}

#[test]
fn falls_back_to_content_when_structured_is_null() {
    let ctr = McpCallToolResult {
        content: vec![text_block("hello"), text_block("world")],
        is_error: None,
        structured_content: Some(serde_json::Value::Null),
        meta: None,
    };

    let got = FunctionCallOutputPayload::from(&ctr);
    let expected = FunctionCallOutputPayload {
        body: FunctionCallOutputBody::Text(
            serde_json::to_string(&vec![text_block("hello"), text_block("world")]).unwrap(),
        ),
        success: Some(true),
    };

    assert_eq!(expected, got);
}

#[test]
fn success_flag_reflects_is_error_true() {
    let ctr = McpCallToolResult {
        content: vec![text_block("unused")],
        is_error: Some(true),
        structured_content: Some(json!({ "message": "bad" })),
        meta: None,
    };

    let got = FunctionCallOutputPayload::from(&ctr);
    let expected = FunctionCallOutputPayload {
        body: FunctionCallOutputBody::Text(
            serde_json::to_string(&json!({ "message": "bad" })).unwrap(),
        ),
        success: Some(false),
    };

    assert_eq!(expected, got);
}

#[test]
fn success_flag_true_with_no_error_and_content_used() {
    let ctr = McpCallToolResult {
        content: vec![text_block("alpha")],
        is_error: Some(false),
        structured_content: None,
        meta: None,
    };

    let got = FunctionCallOutputPayload::from(&ctr);
    let expected = FunctionCallOutputPayload {
        body: FunctionCallOutputBody::Text(
            serde_json::to_string(&vec![text_block("alpha")]).unwrap(),
        ),
        success: Some(true),
    };

    assert_eq!(expected, got);
}

async fn wait_for_thread_rolled_back(
    rx: &async_channel::Receiver<Event>,
) -> crate::protocol::ThreadRolledBackEvent {
    let deadline = StdDuration::from_secs(2);
    let start = std::time::Instant::now();
    loop {
        let remaining = deadline.saturating_sub(start.elapsed());
        let evt = tokio::time::timeout(remaining, rx.recv())
            .await
            .expect("timeout waiting for event")
            .expect("event");
        match evt.msg {
            EventMsg::ThreadRolledBack(payload) => return payload,
            _ => continue,
        }
    }
}

async fn wait_for_thread_rollback_failed(rx: &async_channel::Receiver<Event>) -> ErrorEvent {
    let deadline = StdDuration::from_secs(2);
    let start = std::time::Instant::now();
    loop {
        let remaining = deadline.saturating_sub(start.elapsed());
        let evt = tokio::time::timeout(remaining, rx.recv())
            .await
            .expect("timeout waiting for event")
            .expect("event");
        match evt.msg {
            EventMsg::Error(payload)
                if payload.codex_error_info == Some(CodexErrorInfo::ThreadRollbackFailed) =>
            {
                return payload;
            }
            _ => continue,
        }
    }
}

fn text_block(s: &str) -> serde_json::Value {
    json!({
        "type": "text",
        "text": s,
    })
}

async fn build_test_config(codex_home: &Path) -> Config {
    ConfigBuilder::default()
        .codex_home(codex_home.to_path_buf())
        .build()
        .await
        .expect("load default test config")
}

fn otel_manager(
    conversation_id: ThreadId,
    config: &Config,
    model_info: &ModelInfo,
    session_source: SessionSource,
) -> OtelManager {
    OtelManager::new(
        conversation_id,
        ModelsManager::get_model_offline(config.model.as_deref()).as_str(),
        model_info.slug.as_str(),
        None,
        Some("test@test.com".to_string()),
        Some(TelemetryAuthMode::Chatgpt),
        false,
        "test".to_string(),
        session_source,
    )
}

pub(crate) async fn make_session_and_context() -> (Session, TurnContext) {
    let (tx_event, _rx_event) = async_channel::unbounded();
    let codex_home = tempfile::tempdir().expect("create temp dir");
    let config = build_test_config(codex_home.path()).await;
    let config = Arc::new(config);
    let conversation_id = ThreadId::default();
    let auth_manager = AuthManager::from_auth_for_testing(CodexAuth::from_api_key("Test API Key"));
    let models_manager = Arc::new(ModelsManager::new(
        config.codex_home.clone(),
        auth_manager.clone(),
    ));
    let agent_control = AgentControl::default();
    let exec_policy = ExecPolicyManager::default();
    let (agent_status_tx, _agent_status_rx) = watch::channel(AgentStatus::PendingInit);
    let (tx_sub, _rx_sub) = async_channel::bounded(SUBMISSION_CHANNEL_CAPACITY);
    let model = ModelsManager::get_model_offline(config.model.as_deref());
    let model_info = models_manager
        .get_model_info(model.as_str(), config.as_ref())
        .await;
    let reasoning_effort = config.model_reasoning_effort;
    let collaboration_mode = CollaborationMode {
        mode: ModeKind::Default,
        settings: Settings {
            model,
            reasoning_effort,
            developer_instructions: None,
        },
    };
    let session_configuration = SessionConfiguration {
        provider_id: config.model_provider_id.clone(),
        provider: config.model_provider.clone(),
        collaboration_mode,
        model_reasoning_summary: config.model_reasoning_summary,
        service_tier: config.service_tier.clone(),
        developer_instructions: config.developer_instructions.clone(),
        user_instructions: config.user_instructions.clone(),
        personality: config.personality,
        base_instructions: config
            .base_instructions
            .clone()
            .unwrap_or_else(|| model_info.get_model_instructions(config.personality)),
        compact_prompt: config.compact_prompt.clone(),
        approval_policy: config.approval_policy.clone(),
        sandbox_policy: config.sandbox_policy.clone(),
        windows_sandbox_level: WindowsSandboxLevel::from_config(&config),
        cwd: config.cwd.clone(),
        codex_home: config.codex_home.clone(),
        thread_name: None,
        original_config_do_not_use: Arc::clone(&config),
        session_source: SessionSource::Exec,
        dynamic_tools: Vec::new(),
    };
    let per_turn_config = Session::build_per_turn_config(&session_configuration);
    let model_info = models_manager
        .get_model_info(
            session_configuration.collaboration_mode.model(),
            &per_turn_config,
        )
        .await;
    let otel_manager = otel_manager(
        conversation_id,
        config.as_ref(),
        &model_info,
        session_configuration.session_source.clone(),
    );

    let mut state = SessionState::new(session_configuration.clone());
    mark_state_initial_context_seeded(&mut state);
    let skills_manager = Arc::new(SkillsManager::new(config.codex_home.clone()));

    let file_watcher = Arc::new(FileWatcher::noop());
    let services = SessionServices {
        mcp_connection_manager: Arc::new(RwLock::new(McpConnectionManager::default())),
        mcp_startup_cancellation_token: Mutex::new(CancellationToken::new()),
        unified_exec_manager: UnifiedExecProcessManager::default(),
        analytics_events_client: AnalyticsEventsClient::new(
            Arc::clone(&config),
            Arc::clone(&auth_manager),
        ),
        hooks: Hooks::new(&config),
        rollout: Mutex::new(None),
        user_shell: Arc::new(default_user_shell()),
        show_raw_agent_reasoning: config.show_raw_agent_reasoning,
        exec_policy,
        auth_manager: auth_manager.clone(),
        otel_manager: otel_manager.clone(),
        models_manager: Arc::clone(&models_manager),
        tool_approvals: Mutex::new(ApprovalStore::default()),
        skills_manager,
        file_watcher,
        agent_control,
        state_db: None,
        model_client: ModelClient::new(
            Some(auth_manager.clone()),
            conversation_id,
            session_configuration.provider.clone(),
            session_configuration.session_source.clone(),
            config.model_verbosity,
            config.features.enabled(Feature::ResponsesWebsockets),
            config.features.enabled(Feature::ResponsesWebsocketsV2),
            config.features.enabled(Feature::EnableRequestCompression),
            config.features.enabled(Feature::RuntimeMetrics),
            Session::build_model_client_beta_features_header(config.as_ref()),
        ),
    };

    let turn_context = Session::make_turn_context(
        Some(Arc::clone(&auth_manager)),
        &otel_manager,
        session_configuration.provider.clone(),
        &session_configuration,
        per_turn_config,
        model_info,
        None,
        "turn_id".to_string(),
    );

    let session = Session {
        conversation_id,
        tx_sub,
        tx_event,
        agent_status: agent_status_tx,
        state: Mutex::new(state),
        features: config.features.clone(),
        pending_mcp_server_refresh_config: Mutex::new(None),
        active_turn: Mutex::new(None),
        services,
        next_internal_sub_id: AtomicU64::new(0),
    };

    (session, turn_context)
}

// Like make_session_and_context, but returns Arc<Session> and the event receiver
// so tests can assert on emitted events.
pub(crate) async fn make_session_and_context_with_rx() -> (
    Arc<Session>,
    Arc<TurnContext>,
    async_channel::Receiver<Event>,
) {
    let (tx_event, rx_event) = async_channel::unbounded();
    let codex_home = tempfile::tempdir().expect("create temp dir");
    let config = build_test_config(codex_home.path()).await;
    let config = Arc::new(config);
    let conversation_id = ThreadId::default();
    let auth_manager = AuthManager::from_auth_for_testing(CodexAuth::from_api_key("Test API Key"));
    let models_manager = Arc::new(ModelsManager::new(
        config.codex_home.clone(),
        auth_manager.clone(),
    ));
    let agent_control = AgentControl::default();
    let exec_policy = ExecPolicyManager::default();
    let (agent_status_tx, _agent_status_rx) = watch::channel(AgentStatus::PendingInit);
    let (tx_sub, _rx_sub) = async_channel::bounded(SUBMISSION_CHANNEL_CAPACITY);
    let model = ModelsManager::get_model_offline(config.model.as_deref());
    let model_info = models_manager
        .get_model_info(model.as_str(), config.as_ref())
        .await;
    let reasoning_effort = config.model_reasoning_effort;
    let collaboration_mode = CollaborationMode {
        mode: ModeKind::Default,
        settings: Settings {
            model,
            reasoning_effort,
            developer_instructions: None,
        },
    };
    let session_configuration = SessionConfiguration {
        provider_id: config.model_provider_id.clone(),
        provider: config.model_provider.clone(),
        collaboration_mode,
        model_reasoning_summary: config.model_reasoning_summary,
        service_tier: config.service_tier.clone(),
        developer_instructions: config.developer_instructions.clone(),
        user_instructions: config.user_instructions.clone(),
        personality: config.personality,
        base_instructions: config
            .base_instructions
            .clone()
            .unwrap_or_else(|| model_info.get_model_instructions(config.personality)),
        compact_prompt: config.compact_prompt.clone(),
        approval_policy: config.approval_policy.clone(),
        sandbox_policy: config.sandbox_policy.clone(),
        windows_sandbox_level: WindowsSandboxLevel::from_config(&config),
        cwd: config.cwd.clone(),
        codex_home: config.codex_home.clone(),
        thread_name: None,
        original_config_do_not_use: Arc::clone(&config),
        session_source: SessionSource::Exec,
        dynamic_tools: Vec::new(),
    };
    let per_turn_config = Session::build_per_turn_config(&session_configuration);
    let model_info = models_manager
        .get_model_info(
            session_configuration.collaboration_mode.model(),
            &per_turn_config,
        )
        .await;
    let otel_manager = otel_manager(
        conversation_id,
        config.as_ref(),
        &model_info,
        session_configuration.session_source.clone(),
    );

    let mut state = SessionState::new(session_configuration.clone());
    mark_state_initial_context_seeded(&mut state);
    let skills_manager = Arc::new(SkillsManager::new(config.codex_home.clone()));

    let file_watcher = Arc::new(FileWatcher::noop());
    let services = SessionServices {
        mcp_connection_manager: Arc::new(RwLock::new(McpConnectionManager::default())),
        mcp_startup_cancellation_token: Mutex::new(CancellationToken::new()),
        unified_exec_manager: UnifiedExecProcessManager::default(),
        analytics_events_client: AnalyticsEventsClient::new(
            Arc::clone(&config),
            Arc::clone(&auth_manager),
        ),
        hooks: Hooks::new(&config),
        rollout: Mutex::new(None),
        user_shell: Arc::new(default_user_shell()),
        show_raw_agent_reasoning: config.show_raw_agent_reasoning,
        exec_policy,
        auth_manager: Arc::clone(&auth_manager),
        otel_manager: otel_manager.clone(),
        models_manager: Arc::clone(&models_manager),
        tool_approvals: Mutex::new(ApprovalStore::default()),
        skills_manager,
        file_watcher,
        agent_control,
        state_db: None,
        model_client: ModelClient::new(
            Some(Arc::clone(&auth_manager)),
            conversation_id,
            session_configuration.provider.clone(),
            session_configuration.session_source.clone(),
            config.model_verbosity,
            config.features.enabled(Feature::ResponsesWebsockets),
            config.features.enabled(Feature::ResponsesWebsocketsV2),
            config.features.enabled(Feature::EnableRequestCompression),
            config.features.enabled(Feature::RuntimeMetrics),
            Session::build_model_client_beta_features_header(config.as_ref()),
        ),
    };

    let turn_context = Arc::new(Session::make_turn_context(
        Some(Arc::clone(&auth_manager)),
        &otel_manager,
        session_configuration.provider.clone(),
        &session_configuration,
        per_turn_config,
        model_info,
        None,
        "turn_id".to_string(),
    ));

    let session = Arc::new(Session {
        conversation_id,
        tx_sub,
        tx_event,
        agent_status: agent_status_tx,
        state: Mutex::new(state),
        features: config.features.clone(),
        pending_mcp_server_refresh_config: Mutex::new(None),
        active_turn: Mutex::new(None),
        services,
        next_internal_sub_id: AtomicU64::new(0),
    });

    (session, turn_context, rx_event)
}

fn mark_state_initial_context_seeded(state: &mut SessionState) {
    state.initial_context_seeded = true;
}

#[tokio::test]
async fn refresh_mcp_servers_is_deferred_until_next_turn() {
    let (session, turn_context) = make_session_and_context().await;
    let old_token = session.mcp_startup_cancellation_token().await;
    assert!(!old_token.is_cancelled());

    let mcp_oauth_credentials_store_mode =
        serde_json::to_value(OAuthCredentialsStoreMode::Auto).expect("serialize store mode");
    let refresh_config = McpServerRefreshConfig {
        mcp_servers: json!({}),
        mcp_oauth_credentials_store_mode,
    };
    {
        let mut guard = session.pending_mcp_server_refresh_config.lock().await;
        *guard = Some(refresh_config);
    }

    assert!(!old_token.is_cancelled());
    assert!(
        session
            .pending_mcp_server_refresh_config
            .lock()
            .await
            .is_some()
    );

    session
        .refresh_mcp_servers_if_requested(&turn_context)
        .await;

    assert!(old_token.is_cancelled());
    assert!(
        session
            .pending_mcp_server_refresh_config
            .lock()
            .await
            .is_none()
    );
    let new_token = session.mcp_startup_cancellation_token().await;
    assert!(!new_token.is_cancelled());
}

#[tokio::test]
async fn record_model_warning_appends_user_message() {
    let (mut session, turn_context) = make_session_and_context().await;
    let features = Features::with_defaults();
    session.features = features;

    session
        .record_model_warning("too many unified exec processes", &turn_context)
        .await;

    let history = session.clone_history().await;
    let history_items = history.raw_items();
    let last = history_items.last().expect("warning recorded");

    match last {
        ResponseItem::Message { role, content, .. } => {
            assert_eq!(role, "user");
            assert_eq!(
                content,
                &vec![ContentItem::InputText {
                    text: "Warning: too many unified exec processes".to_string(),
                }]
            );
        }
        other => panic!("expected user message, got {other:?}"),
    }
}

#[derive(Clone, Copy)]
struct NeverEndingTask {
    kind: TaskKind,
    listen_to_cancellation_token: bool,
}
impl SessionTask for NeverEndingTask {
    fn kind(&self) -> TaskKind {
        self.kind
    }

    async fn run(
        self: Arc<Self>,
        _session: Arc<SessionTaskContext>,
        _ctx: Arc<TurnContext>,
        _input: Vec<UserInput>,
        cancellation_token: CancellationToken,
    ) -> Option<String> {
        if self.listen_to_cancellation_token {
            cancellation_token.cancelled().await;
            return None;
        }
        loop {
            sleep(Duration::from_secs(60)).await;
        }
    }
}

fn steer_text_input(text: &str) -> Vec<UserInput> {
    vec![UserInput::Text {
        text: text.to_string(),
        text_elements: Vec::new(),
    }]
}

async fn spawn_never_ending_task(sess: &Arc<Session>, tc: &Arc<TurnContext>, kind: TaskKind) {
    sess.spawn_task(
        Arc::clone(tc),
        steer_text_input("active task"),
        NeverEndingTask {
            kind,
            listen_to_cancellation_token: true,
        },
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn steer_input_returns_no_active_turn_when_idle() {
    let (sess, _tc, _rx) = make_session_and_context_with_rx().await;
    let input = steer_text_input("steer");

    assert_eq!(
        sess.steer_input(input.clone()).await,
        Err(SteerInputError::NoActiveTurn(input))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn steer_input_rejects_empty_input() {
    let (sess, _tc, _rx) = make_session_and_context_with_rx().await;

    assert_eq!(
        sess.steer_input(Vec::new()).await,
        Err(SteerInputError::EmptyInput)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn steer_input_accepts_regular_active_turn() {
    let (sess, tc, _rx) = make_session_and_context_with_rx().await;
    spawn_never_ending_task(&sess, &tc, TaskKind::Regular).await;

    let input = steer_text_input("steer");
    sess.steer_input(input.clone())
        .await
        .expect("regular turn should accept steer input");

    assert_eq!(
        sess.get_pending_input().await,
        vec![ResponseInputItem::from(input)]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn steer_input_rejects_non_steerable_active_turns() {
    for (task_kind, turn_kind) in [
        (TaskKind::Review, NonSteerableTurnKind::Review),
        (
            TaskKind::PostTurnCompletionReview,
            NonSteerableTurnKind::Review,
        ),
        (TaskKind::Compact, NonSteerableTurnKind::Compact),
        (TaskKind::UserShell, NonSteerableTurnKind::UserShell),
    ] {
        let (sess, tc, _rx) = make_session_and_context_with_rx().await;
        spawn_never_ending_task(&sess, &tc, task_kind).await;

        assert_eq!(
            sess.steer_input(steer_text_input("steer")).await,
            Err(SteerInputError::ActiveTurnNotSteerable { turn_kind })
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[test_log::test]
async fn abort_regular_task_emits_turn_aborted_only() {
    let (sess, tc, rx) = make_session_and_context_with_rx().await;
    let input = vec![UserInput::Text {
        text: "hello".to_string(),
        text_elements: Vec::new(),
    }];
    sess.spawn_task(
        Arc::clone(&tc),
        input,
        NeverEndingTask {
            kind: TaskKind::Regular,
            listen_to_cancellation_token: false,
        },
    )
    .await;

    sess.abort_all_tasks(TurnAbortReason::Interrupted).await;

    // Interrupts persist a model-visible `<turn_aborted>` marker into history, but there is no
    // separate client-visible event for that marker (only `EventMsg::TurnAborted`).
    let evt = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
        .await
        .expect("timeout waiting for event")
        .expect("event");
    match evt.msg {
        EventMsg::TurnAborted(e) => assert_eq!(TurnAbortReason::Interrupted, e.reason),
        other => panic!("unexpected event: {other:?}"),
    }
    // No extra events should be emitted after an abort.
    assert!(rx.try_recv().is_err());
}

#[tokio::test]
async fn abort_gracefully_emits_turn_aborted_only() {
    let (sess, tc, rx) = make_session_and_context_with_rx().await;
    let input = vec![UserInput::Text {
        text: "hello".to_string(),
        text_elements: Vec::new(),
    }];
    sess.spawn_task(
        Arc::clone(&tc),
        input,
        NeverEndingTask {
            kind: TaskKind::Regular,
            listen_to_cancellation_token: true,
        },
    )
    .await;

    sess.abort_all_tasks(TurnAbortReason::Interrupted).await;

    // Even if tasks handle cancellation gracefully, interrupts still result in `TurnAborted`
    // being the only client-visible signal.
    let evt = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
        .await
        .expect("timeout waiting for event")
        .expect("event");
    match evt.msg {
        EventMsg::TurnAborted(e) => assert_eq!(TurnAbortReason::Interrupted, e.reason),
        other => panic!("unexpected event: {other:?}"),
    }
    // No extra events should be emitted after an abort.
    assert!(rx.try_recv().is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn task_finish_persists_leftover_pending_input() {
    let (sess, tc, _rx) = make_session_and_context_with_rx().await;
    let input = vec![UserInput::Text {
        text: "hello".to_string(),
        text_elements: Vec::new(),
    }];
    sess.spawn_task(
        Arc::clone(&tc),
        input,
        NeverEndingTask {
            kind: TaskKind::Regular,
            listen_to_cancellation_token: false,
        },
    )
    .await;

    sess.inject_response_items(vec![ResponseInputItem::Message {
        role: "user".to_string(),
        content: vec![ContentItem::InputText {
            text: "late pending input".to_string(),
        }],
    }])
    .await
    .expect("inject pending input into active turn");

    sess.on_task_finished(Arc::clone(&tc), None, TaskKind::Regular)
        .await;

    let history = sess.clone_history().await;
    let expected = ResponseItem::Message {
        id: None,
        role: "user".to_string(),
        content: vec![ContentItem::InputText {
            text: "late pending input".to_string(),
        }],
        end_turn: None,
        phase: None,
    };
    assert!(
        history.raw_items().iter().any(|item| item == &expected),
        "expected pending input to be persisted into history on turn completion"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn regular_task_completion_stores_completed_turn_for_review() {
    let (sess, tc, _rx) = make_session_and_context_with_rx().await;
    let input = vec![
        UserInput::Text {
            text: "first user message".to_string(),
            text_elements: Vec::new(),
        },
        UserInput::Image {
            image_url: "data:image/png;base64,aaa".to_string(),
        },
        UserInput::Text {
            text: "second user message".to_string(),
            text_elements: Vec::new(),
        },
    ];
    sess.spawn_task(
        Arc::clone(&tc),
        input,
        NeverEndingTask {
            kind: TaskKind::Regular,
            listen_to_cancellation_token: false,
        },
    )
    .await;

    sess.on_task_finished(
        Arc::clone(&tc),
        Some("final assistant message".to_string()),
        TaskKind::Regular,
    )
    .await;

    assert_eq!(
        sess.last_completed_regular_turn_for_review().await,
        Some(CompletedTurnForReview {
            turn_id: tc.sub_id.clone(),
            cwd: tc.cwd.clone(),
            interaction_history: vec![CompletedTurnReviewRound {
                user_messages: vec![
                    "first user message".to_string(),
                    "second user message".to_string()
                ],
                final_agent_message: "final assistant message".to_string(),
            }],
            user_messages: vec![
                "first user message".to_string(),
                "second user message".to_string()
            ],
            final_agent_message: "final assistant message".to_string(),
        })
    );
}

#[test]
fn completed_turn_reconstruction_uses_user_and_final_assistant_text() {
    let cwd = PathBuf::from("/tmp/project");
    let history = vec![
        DeveloperInstructions::new("developer note").into(),
        user_message("implement the feature"),
        ResponseItem::FunctionCall {
            id: None,
            name: "shell".to_string(),
            arguments: "{}".to_string(),
            call_id: "call-1".to_string(),
        },
        ResponseItem::FunctionCallOutput {
            call_id: "call-1".to_string(),
            output: FunctionCallOutputPayload::from_text("tool output".to_string()),
        },
        assistant_message("done"),
    ];

    assert_eq!(
        completed_turn_for_review_from_history(&history, cwd.clone()),
        Some(CompletedTurnForReview {
            turn_id: "reconstructed-1-4".to_string(),
            cwd,
            interaction_history: vec![CompletedTurnReviewRound {
                user_messages: vec!["implement the feature".to_string()],
                final_agent_message: "done".to_string(),
            }],
            user_messages: vec!["implement the feature".to_string()],
            final_agent_message: "done".to_string(),
        })
    );
}

#[test]
fn completed_turn_reconstruction_skips_review_synthetic_turn() {
    let cwd = PathBuf::from("/tmp/project");
    let history = vec![
        user_message("real request"),
        assistant_message("real response"),
        user_message(
            "<user_action>\n  <context>User initiated a review task.</context>\n</user_action>",
        ),
        assistant_message("review output"),
    ];

    assert_eq!(
        completed_turn_for_review_from_history(&history, cwd.clone()),
        Some(CompletedTurnForReview {
            turn_id: "reconstructed-0-1".to_string(),
            cwd,
            interaction_history: vec![CompletedTurnReviewRound {
                user_messages: vec!["real request".to_string()],
                final_agent_message: "real response".to_string(),
            }],
            user_messages: vec!["real request".to_string()],
            final_agent_message: "real response".to_string(),
        })
    );
}

#[test]
fn completed_turn_reconstruction_keeps_multi_round_interaction_history() {
    let cwd = PathBuf::from("/tmp/project");
    let history = vec![
        user_message("round one request"),
        ResponseItem::FunctionCall {
            id: None,
            name: "shell".to_string(),
            arguments: "{}".to_string(),
            call_id: "call-1".to_string(),
        },
        ResponseItem::FunctionCallOutput {
            call_id: "call-1".to_string(),
            output: FunctionCallOutputPayload::from_text("tool output".to_string()),
        },
        assistant_message("round one final"),
        user_message("round two request"),
        assistant_message("round two interim"),
        assistant_message("round two final"),
    ];

    assert_eq!(
        completed_turn_for_review_from_history(&history, cwd.clone()),
        Some(CompletedTurnForReview {
            turn_id: "reconstructed-4-6".to_string(),
            cwd,
            interaction_history: vec![
                CompletedTurnReviewRound {
                    user_messages: vec!["round one request".to_string()],
                    final_agent_message: "round one final".to_string(),
                },
                CompletedTurnReviewRound {
                    user_messages: vec!["round two request".to_string()],
                    final_agent_message: "round two final".to_string(),
                },
            ],
            user_messages: vec!["round two request".to_string()],
            final_agent_message: "round two final".to_string(),
        })
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn abort_review_task_emits_exited_then_aborted_and_records_history() {
    let (sess, tc, rx) = make_session_and_context_with_rx().await;
    let input = vec![UserInput::Text {
        text: "start review".to_string(),
        text_elements: Vec::new(),
    }];
    sess.spawn_task(Arc::clone(&tc), input, ReviewTask::new())
        .await;

    sess.abort_all_tasks(TurnAbortReason::Interrupted).await;

    // Aborting a review task should exit review mode before surfacing the abort to the client.
    // We scan for these events (rather than relying on fixed ordering) since unrelated events
    // may interleave.
    let mut exited_review_mode_idx = None;
    let mut turn_aborted_idx = None;
    let mut idx = 0usize;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(3);
    while tokio::time::Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let evt = tokio::time::timeout(remaining, rx.recv())
            .await
            .expect("timeout waiting for event")
            .expect("event");
        let event_idx = idx;
        idx = idx.saturating_add(1);
        match evt.msg {
            EventMsg::ExitedReviewMode(ev) => {
                assert!(ev.review_output.is_none());
                exited_review_mode_idx = Some(event_idx);
            }
            EventMsg::TurnAborted(ev) => {
                assert_eq!(TurnAbortReason::Interrupted, ev.reason);
                turn_aborted_idx = Some(event_idx);
                break;
            }
            _ => {}
        }
    }
    assert!(
        exited_review_mode_idx.is_some(),
        "expected ExitedReviewMode after abort"
    );
    assert!(
        turn_aborted_idx.is_some(),
        "expected TurnAborted after abort"
    );
    assert!(
        exited_review_mode_idx.unwrap() < turn_aborted_idx.unwrap(),
        "expected ExitedReviewMode before TurnAborted"
    );

    let history = sess.clone_history().await;
    // The `<turn_aborted>` marker is silent in the event stream, so verify it is still
    // recorded in history for the model.
    assert!(
        history.raw_items().iter().any(|item| {
            let ResponseItem::Message { role, content, .. } = item else {
                return false;
            };
            if role != "user" {
                return false;
            }
            content.iter().any(|content_item| {
                let ContentItem::InputText { text } = content_item else {
                    return false;
                };
                text.contains(crate::session_prefix::TURN_ABORTED_OPEN_TAG)
            })
        }),
        "expected a model-visible turn aborted marker in history after interrupt"
    );
}

#[tokio::test]
async fn fatal_tool_error_stops_turn_and_reports_error() {
    let (session, turn_context, _rx) = make_session_and_context_with_rx().await;
    let tools = {
        session
            .services
            .mcp_connection_manager
            .read()
            .await
            .list_all_tools()
            .await
    };
    let router = ToolRouter::from_config(
        &turn_context.tools_config,
        Some(
            tools
                .into_iter()
                .map(|(name, tool)| (name, tool.tool))
                .collect(),
        ),
        turn_context.dynamic_tools.as_slice(),
    );
    let item = ResponseItem::CustomToolCall {
        id: None,
        status: None,
        call_id: "call-1".to_string(),
        name: "shell".to_string(),
        input: "{}".to_string(),
    };

    let call = ToolRouter::build_tool_call(session.as_ref(), item.clone())
        .await
        .expect("build tool call")
        .expect("tool call present");
    let tracker = Arc::new(tokio::sync::Mutex::new(TurnDiffTracker::new()));
    let err = router
        .dispatch_tool_call(
            Arc::clone(&session),
            Arc::clone(&turn_context),
            tracker,
            call,
        )
        .await
        .expect_err("expected fatal error");

    match err {
        FunctionCallError::Fatal(message) => {
            assert_eq!(message, "tool shell invoked with incompatible payload");
        }
        other => panic!("expected FunctionCallError::Fatal, got {other:?}"),
    }
}

async fn sample_rollout(
    session: &Session,
    turn_context: &TurnContext,
) -> (Vec<RolloutItem>, Vec<ResponseItem>) {
    let mut rollout_items = Vec::new();
    let mut live_history = ContextManager::new();

    let initial_context = session.build_initial_context(turn_context).await;
    for item in &initial_context {
        rollout_items.push(RolloutItem::ResponseItem(item.clone()));
    }
    live_history.record_items(initial_context.iter(), turn_context.truncation_policy);

    let user1 = ResponseItem::Message {
        id: None,
        role: "user".to_string(),
        content: vec![ContentItem::InputText {
            text: "first user".to_string(),
        }],
        end_turn: None,
        phase: None,
    };
    live_history.record_items(std::iter::once(&user1), turn_context.truncation_policy);
    rollout_items.push(RolloutItem::ResponseItem(user1.clone()));

    let assistant1 = ResponseItem::Message {
        id: None,
        role: "assistant".to_string(),
        content: vec![ContentItem::OutputText {
            text: "assistant reply one".to_string(),
        }],
        end_turn: None,
        phase: None,
    };
    live_history.record_items(std::iter::once(&assistant1), turn_context.truncation_policy);
    rollout_items.push(RolloutItem::ResponseItem(assistant1.clone()));

    let summary1 = "summary one";
    let snapshot1 = live_history.clone().for_prompt();
    let user_messages1 = collect_user_messages(&snapshot1);
    let rebuilt1 = compact::build_compacted_history(
        session.build_initial_context(turn_context).await,
        &user_messages1,
        summary1,
    );
    live_history.replace(rebuilt1);
    rollout_items.push(RolloutItem::Compacted(CompactedItem {
        message: summary1.to_string(),
        replacement_history: None,
    }));

    let user2 = ResponseItem::Message {
        id: None,
        role: "user".to_string(),
        content: vec![ContentItem::InputText {
            text: "second user".to_string(),
        }],
        end_turn: None,
        phase: None,
    };
    live_history.record_items(std::iter::once(&user2), turn_context.truncation_policy);
    rollout_items.push(RolloutItem::ResponseItem(user2.clone()));

    let assistant2 = ResponseItem::Message {
        id: None,
        role: "assistant".to_string(),
        content: vec![ContentItem::OutputText {
            text: "assistant reply two".to_string(),
        }],
        end_turn: None,
        phase: None,
    };
    live_history.record_items(std::iter::once(&assistant2), turn_context.truncation_policy);
    rollout_items.push(RolloutItem::ResponseItem(assistant2.clone()));

    let summary2 = "summary two";
    let snapshot2 = live_history.clone().for_prompt();
    let user_messages2 = collect_user_messages(&snapshot2);
    let rebuilt2 = compact::build_compacted_history(
        session.build_initial_context(turn_context).await,
        &user_messages2,
        summary2,
    );
    live_history.replace(rebuilt2);
    rollout_items.push(RolloutItem::Compacted(CompactedItem {
        message: summary2.to_string(),
        replacement_history: None,
    }));

    let user3 = ResponseItem::Message {
        id: None,
        role: "user".to_string(),
        content: vec![ContentItem::InputText {
            text: "third user".to_string(),
        }],
        end_turn: None,
        phase: None,
    };
    live_history.record_items(std::iter::once(&user3), turn_context.truncation_policy);
    rollout_items.push(RolloutItem::ResponseItem(user3));

    let assistant3 = ResponseItem::Message {
        id: None,
        role: "assistant".to_string(),
        content: vec![ContentItem::OutputText {
            text: "assistant reply three".to_string(),
        }],
        end_turn: None,
        phase: None,
    };
    live_history.record_items(std::iter::once(&assistant3), turn_context.truncation_policy);
    rollout_items.push(RolloutItem::ResponseItem(assistant3));

    (rollout_items, live_history.for_prompt())
}

#[tokio::test]
async fn rejects_escalated_permissions_when_policy_not_on_request() {
    use crate::exec::ExecParams;
    use crate::protocol::AskForApproval;
    use crate::protocol::SandboxPolicy;
    use crate::sandboxing::SandboxPermissions;
    use crate::turn_diff_tracker::TurnDiffTracker;
    use std::collections::HashMap;

    let (session, mut turn_context_raw) = make_session_and_context().await;
    // Ensure policy is NOT OnRequest so the early rejection path triggers
    turn_context_raw.approval_policy = AskForApproval::OnFailure;
    let session = Arc::new(session);
    let mut turn_context = Arc::new(turn_context_raw);

    let timeout_ms = 1000;
    let sandbox_permissions = SandboxPermissions::RequireEscalated;
    let params = ExecParams {
        command: if cfg!(windows) {
            vec![
                "cmd.exe".to_string(),
                "/C".to_string(),
                "echo hi".to_string(),
            ]
        } else {
            vec![
                "/bin/sh".to_string(),
                "-c".to_string(),
                "echo hi".to_string(),
            ]
        },
        cwd: turn_context.cwd.clone(),
        expiration: timeout_ms.into(),
        env: HashMap::new(),
        sandbox_permissions,
        windows_sandbox_level: turn_context.windows_sandbox_level,
        justification: Some("test".to_string()),
        arg0: None,
    };

    let params2 = ExecParams {
        sandbox_permissions: SandboxPermissions::UseDefault,
        command: params.command.clone(),
        cwd: params.cwd.clone(),
        expiration: timeout_ms.into(),
        env: HashMap::new(),
        windows_sandbox_level: turn_context.windows_sandbox_level,
        justification: params.justification.clone(),
        arg0: None,
    };

    let turn_diff_tracker = Arc::new(tokio::sync::Mutex::new(TurnDiffTracker::new()));

    let tool_name = "shell";
    let call_id = "test-call".to_string();

    let handler = ShellHandler;
    let resp = handler
        .handle(ToolInvocation {
            session: Arc::clone(&session),
            turn: Arc::clone(&turn_context),
            tracker: Arc::clone(&turn_diff_tracker),
            call_id,
            tool_name: tool_name.to_string(),
            payload: ToolPayload::Function {
                arguments: serde_json::json!({
                    "command": params.command.clone(),
                    "workdir": Some(turn_context.cwd.to_string_lossy().to_string()),
                    "timeout_ms": params.expiration.timeout_ms(),
                    "sandbox_permissions": params.sandbox_permissions,
                    "justification": params.justification.clone(),
                })
                .to_string(),
            },
        })
        .await;

    let Err(FunctionCallError::RespondToModel(output)) = resp else {
        panic!("expected error result");
    };

    let expected = format!(
        "approval policy is {policy:?}; reject command — you should not ask for escalated permissions if the approval policy is {policy:?}",
        policy = turn_context.approval_policy
    );

    pretty_assertions::assert_eq!(output, expected);

    // Now retry the same command WITHOUT escalated permissions; should succeed.
    // Force DangerFullAccess to avoid platform sandbox dependencies in tests.
    Arc::get_mut(&mut turn_context)
        .expect("unique turn context Arc")
        .sandbox_policy = SandboxPolicy::DangerFullAccess;

    let resp2 = handler
        .handle(ToolInvocation {
            session: Arc::clone(&session),
            turn: Arc::clone(&turn_context),
            tracker: Arc::clone(&turn_diff_tracker),
            call_id: "test-call-2".to_string(),
            tool_name: tool_name.to_string(),
            payload: ToolPayload::Function {
                arguments: serde_json::json!({
                    "command": params2.command.clone(),
                    "workdir": Some(turn_context.cwd.to_string_lossy().to_string()),
                    "timeout_ms": params2.expiration.timeout_ms(),
                    "sandbox_permissions": params2.sandbox_permissions,
                    "justification": params2.justification.clone(),
                })
                .to_string(),
            },
        })
        .await;

    let output = match resp2.expect("expected Ok result") {
        ToolOutput::Function {
            body: FunctionCallOutputBody::Text(content),
            ..
        } => content,
        _ => panic!("unexpected tool output"),
    };

    #[derive(Deserialize, PartialEq, Eq, Debug)]
    struct ResponseExecMetadata {
        exit_code: i32,
    }

    #[derive(Deserialize)]
    struct ResponseExecOutput {
        output: String,
        metadata: ResponseExecMetadata,
    }

    let exec_output: ResponseExecOutput =
        serde_json::from_str(&output).expect("valid exec output json");

    pretty_assertions::assert_eq!(exec_output.metadata, ResponseExecMetadata { exit_code: 0 });
    assert!(exec_output.output.contains("hi"));
}
#[tokio::test]
async fn unified_exec_rejects_escalated_permissions_when_policy_not_on_request() {
    use crate::protocol::AskForApproval;
    use crate::sandboxing::SandboxPermissions;
    use crate::turn_diff_tracker::TurnDiffTracker;

    let (session, mut turn_context_raw) = make_session_and_context().await;
    turn_context_raw.approval_policy = AskForApproval::OnFailure;
    let session = Arc::new(session);
    let turn_context = Arc::new(turn_context_raw);
    let tracker = Arc::new(tokio::sync::Mutex::new(TurnDiffTracker::new()));

    let handler = UnifiedExecHandler;
    let resp = handler
        .handle(ToolInvocation {
            session: Arc::clone(&session),
            turn: Arc::clone(&turn_context),
            tracker: Arc::clone(&tracker),
            call_id: "exec-call".to_string(),
            tool_name: "exec_command".to_string(),
            payload: ToolPayload::Function {
                arguments: serde_json::json!({
                    "cmd": "echo hi",
                    "sandbox_permissions": SandboxPermissions::RequireEscalated,
                    "justification": "need unsandboxed execution",
                })
                .to_string(),
            },
        })
        .await;

    let Err(FunctionCallError::RespondToModel(output)) = resp else {
        panic!("expected error result");
    };

    let expected = format!(
        "approval policy is {policy:?}; reject command — you cannot ask for escalated permissions if the approval policy is {policy:?}",
        policy = turn_context.approval_policy
    );

    pretty_assertions::assert_eq!(output, expected);
}
