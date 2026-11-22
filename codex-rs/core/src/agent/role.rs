use crate::config::Config;
use crate::features::Feature;
use crate::protocol::AskForApproval;
use crate::protocol::SandboxPolicy;
use codex_protocol::openai_models::ReasoningEffort;
use serde::Deserialize;
use serde::Serialize;
use std::sync::Mutex;
use std::sync::PoisonError;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering::Relaxed;
use std::time::Duration;

/// Base instructions for the orchestrator role.
const ORCHESTRATOR_PROMPT: &str = include_str!("../../templates/agents/orchestrator.md");
/// Default model override used.
// TODO(jif) update when we have something smarter.
const EXPLORER_MODEL: &str = "gpt-6-luna";
pub(crate) const EXPLORER_MAX_ACQUISITIONS: usize = 12;
pub(crate) const EXPLORER_RESULT_BYTES: usize = 8 * 1024;
pub(crate) const EXPLORER_DEADLINE: Duration = Duration::from_secs(120);
pub(crate) const EXPLORER_REPORT_RESERVE: Duration = Duration::from_secs(20);
const EXPLORER_GUIDANCE: &str = "Research only the assigned question and scope. Prefer direct paths and targeted ranges; narrow partial searches and recheck changed evidence. Report findings with file/range and revision, counterevidence or false premises, and what you did not examine. Keep the report concise and label partial coverage.";

/// Enumerated list of all supported agent roles.
const ALL_ROLES: [AgentRole; 3] = [
    AgentRole::Default,
    AgentRole::Explorer,
    AgentRole::Worker,
    // TODO(jif) add when we have stable prompts + models
    // AgentRole::Orchestrator,
];

/// Hard-coded agent role selection used when spawning sub-agents.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentRole {
    /// Inherit the parent agent's configuration unchanged.
    Default,
    /// Coordination-only agent that delegates to workers.
    Orchestrator,
    /// Task-executing agent with a fixed model override.
    Worker,
    /// Read-only specialist for independent evidence gathering.
    Explorer,
}

/// Immutable profile data that drives per-agent configuration overrides.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AgentProfile {
    /// Optional base instructions override.
    pub base_instructions: Option<&'static str>,
    /// Optional model override.
    pub model: Option<&'static str>,
    /// Optional reasoning effort override.
    pub reasoning_effort: Option<ReasoningEffort>,
    /// Whether to force a read-only sandbox policy.
    pub read_only: bool,
    /// Description to include in the tool specs.
    pub description: &'static str,
}

impl AgentRole {
    /// Returns the string values used by JSON schema enums.
    pub fn enum_values() -> Vec<String> {
        ALL_ROLES
            .iter()
            .filter_map(|role| {
                let description = role.profile().description;
                serde_json::to_string(role)
                    .map(|role| {
                        let description = if !description.is_empty() {
                            format!(r#", "description": {description}"#)
                        } else {
                            String::new()
                        };
                        format!(r#"{{ "name": {role}{description}}}"#)
                    })
                    .ok()
            })
            .collect()
    }

    /// Returns the hard-coded profile for this role.
    pub fn profile(self) -> AgentProfile {
        match self {
            AgentRole::Default => AgentProfile::default(),
            AgentRole::Orchestrator => AgentProfile {
                base_instructions: Some(ORCHESTRATOR_PROMPT),
                ..Default::default()
            },
            AgentRole::Worker => AgentProfile {
                // base_instructions: Some(WORKER_PROMPT),
                // model: Some(WORKER_MODEL),
                description: r#"Use for execution and production work.
Typical tasks:
- Implement part of a feature
- Fix tests or bugs
- Split large refactors into independent chunks
Rules:
- Explicitly assign **ownership** of the task (files / responsibility).
- Always tell workers they are **not alone in the codebase**, and they should ignore edits made by others without touching them"#,
                ..Default::default()
            },
            AgentRole::Explorer => AgentProfile {
                model: Some(EXPLORER_MODEL),
                reasoning_effort: Some(ReasoningEffort::Medium),
                read_only: true,
                description: "Read-only independent research when the raw evidence is much larger than the report. Use direct lookup for narrow questions. Provide scope, revision, premises to check, and expected evidence. Verify consequential claims selectively.",
                ..Default::default()
            },
        }
    }

    /// Applies this role's profile onto the provided config.
    pub fn apply_to_config(self, config: &mut Config) -> Result<(), String> {
        let profile = self.profile();
        if let Some(base_instructions) = profile.base_instructions {
            config.base_instructions = Some(base_instructions.to_string());
        }
        if let Some(model) = profile.model {
            config.model = Some(model.to_string());
        }
        if let Some(reasoning_effort) = profile.reasoning_effort {
            config.model_reasoning_effort = Some(reasoning_effort)
        }
        if profile.read_only {
            config
                .sandbox_policy
                .set(SandboxPolicy::new_read_only_policy())
                .map_err(|err| format!("sandbox_policy is invalid: {err}"))?;
        }
        if self == Self::Explorer {
            config.bounded_explorer = true;
            config
                .approval_policy
                .set(AskForApproval::Never)
                .map_err(|err| format!("approval_policy is invalid: {err}"))?;
            config
                .mcp_servers
                .set(Default::default())
                .map_err(|err| format!("mcp_servers are invalid: {err}"))?;
            config.features.disable(Feature::Collab);
            config.features.disable(Feature::Apps);
            config.features.disable(Feature::SkillMcpDependencyInstall);
            config
                .features
                .disable(Feature::AutoPostTurnCompletionReview);
            let instructions = config
                .developer_instructions
                .get_or_insert_with(String::new);
            if !instructions.is_empty() {
                instructions.push_str("\n\n");
            }
            instructions.push_str(EXPLORER_GUIDANCE);
        }
        Ok(())
    }
}

pub(crate) fn explorer_tool_allowed(name: &str) -> bool {
    matches!(name, "grep_files" | "read_file")
}

#[derive(Debug, Default)]
pub(crate) struct ExplorationBudget {
    admitted: AtomicUsize,
    partial: AtomicUsize,
    revisions: Mutex<Vec<String>>,
}

impl ExplorationBudget {
    pub(crate) fn admit(&self) -> bool {
        self.admitted
            .fetch_update(Relaxed, Relaxed, |used| {
                (used < EXPLORER_MAX_ACQUISITIONS).then_some(used + 1)
            })
            .is_ok()
    }

    pub(crate) fn exhausted(&self) -> bool {
        self.admitted.load(Relaxed) >= EXPLORER_MAX_ACQUISITIONS
    }

    pub(crate) fn observe(&self, partial: bool, revision: Option<&str>) {
        if partial {
            self.partial.fetch_add(1, Relaxed);
        }
        if let Some(revision) = revision {
            let mut revisions = self
                .revisions
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if revisions.len() < 4 && !revisions.iter().any(|seen| seen == revision) {
                revisions.push(revision.to_string());
            }
        }
    }

    pub(crate) fn report(&self, report: Option<String>, limited: bool) -> String {
        let admitted = self.admitted.load(Relaxed);
        let partial = self.partial.load(Relaxed);
        let revisions = self
            .revisions
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let revisions = if revisions.is_empty() {
            String::new()
        } else {
            format!(" Read revisions (up to four): {}.", revisions.join(", "))
        };
        let status = if limited
            || partial > 0
            || report
                .as_ref()
                .is_none_or(|text| text.len() > EXPLORER_RESULT_BYTES)
        {
            "partial"
        } else {
            "reported"
        };
        let coverage = format!(
            "Exploration {status}; {admitted} evidence calls admitted, {partial} partial results.{revisions}"
        );
        match report {
            Some(report) if report.len() <= EXPLORER_RESULT_BYTES => {
                format!("{coverage}\n{report}")
            }
            Some(_) => format!(
                "{coverage} Report exceeded the inline limit and remains in the child history. Request a focused replacement."
            ),
            None => {
                format!("{coverage} No final report was available; remaining scope unverified.")
            }
        }
    }
}
