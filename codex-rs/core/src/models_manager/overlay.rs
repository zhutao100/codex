use std::io;

use codex_protocol::config_types::Verbosity;
use codex_protocol::openai_models::ApplyPatchToolType;
use codex_protocol::openai_models::ConfigShellToolType;
use codex_protocol::openai_models::InputModality;
use codex_protocol::openai_models::ModelInfo;
use codex_protocol::openai_models::ModelInfoUpgrade;
use codex_protocol::openai_models::ModelInstructionsVariables;
use codex_protocol::openai_models::ModelMessages;
use codex_protocol::openai_models::ModelVisibility;
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::openai_models::ReasoningEffortPreset;
use codex_protocol::openai_models::TruncationPolicyConfig;
use codex_utils_absolute_path::AbsolutePathBuf;
use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;
use tracing::warn;

use crate::models_manager::model_info;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModelOverlay {
    pub cross_model: ModelInfoPatch,
    pub final_instruction_override: Option<String>,
    pub models: Vec<ModelOverlayEntry>,
}

impl ModelOverlay {
    pub(crate) fn apply_to_candidates(&self, candidates: &[ModelInfo]) -> Vec<ModelInfo> {
        let mut overlaid: Vec<ModelInfo> = candidates
            .iter()
            .cloned()
            .map(|mut model| {
                self.cross_model
                    .apply_to(&mut model, self.final_instruction_override.is_some());
                model
            })
            .collect();

        for entry in &self.models {
            if let Some(model) = overlaid.iter_mut().find(|model| model.slug == entry.slug) {
                entry
                    .patch
                    .apply_to(model, entry.final_instruction_override.is_some());
            } else {
                let mut model = model_info::model_info_from_slug(&entry.slug);
                self.cross_model
                    .apply_to(&mut model, self.final_instruction_override.is_some());
                entry
                    .patch
                    .apply_to(&mut model, entry.final_instruction_override.is_some());
                overlaid.push(model);
            }
        }

        overlaid
    }

    pub(crate) fn apply_mentioned_to_candidates(&self, candidates: &[ModelInfo]) -> Vec<ModelInfo> {
        self.models
            .iter()
            .map(|entry| {
                let mut model = candidates
                    .iter()
                    .find(|candidate| candidate.slug == entry.slug)
                    .cloned()
                    .unwrap_or_else(|| model_info::model_info_from_slug(&entry.slug));
                self.cross_model
                    .apply_to(&mut model, self.final_instruction_override.is_some());
                entry
                    .patch
                    .apply_to(&mut model, entry.final_instruction_override.is_some());
                model
            })
            .collect()
    }

    pub(crate) fn final_instruction_override_for_slug(&self, slug: &str) -> Option<String> {
        self.models
            .iter()
            .find(|entry| entry.slug == slug)
            .and_then(|entry| entry.final_instruction_override.clone())
            .or_else(|| self.final_instruction_override.clone())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelOverlayEntry {
    pub slug: String,
    pub patch: ModelInfoPatch,
    pub final_instruction_override: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModelInfoPatch {
    pub display_name: Option<String>,
    pub description: Option<Option<String>>,
    pub default_reasoning_level: Option<Option<ReasoningEffort>>,
    pub supported_reasoning_levels: Option<Vec<ReasoningEffortPreset>>,
    pub shell_type: Option<ConfigShellToolType>,
    pub visibility: Option<ModelVisibility>,
    pub supported_in_api: Option<bool>,
    pub priority: Option<i32>,
    pub upgrade: Option<Option<ModelInfoUpgrade>>,
    pub base_instructions: Option<String>,
    pub model_messages: Option<ModelMessagesPatch>,
    pub clear_model_messages: bool,
    pub supports_reasoning_summaries: Option<bool>,
    pub support_verbosity: Option<bool>,
    pub default_verbosity: Option<Option<Verbosity>>,
    pub apply_patch_tool_type: Option<Option<ApplyPatchToolType>>,
    pub truncation_policy: Option<TruncationPolicyConfig>,
    pub supports_parallel_tool_calls: Option<bool>,
    pub context_window: Option<Option<i64>>,
    pub auto_compact_token_limit: Option<Option<i64>>,
    pub effective_context_window_percent: Option<i64>,
    pub experimental_supported_tools: Option<Vec<String>>,
    pub input_modalities: Option<Vec<InputModality>>,
}

impl ModelInfoPatch {
    fn apply_to(&self, model: &mut ModelInfo, has_final_instruction_override: bool) {
        if let Some(display_name) = &self.display_name {
            model.display_name = display_name.clone();
        }
        if let Some(description) = &self.description {
            model.description = description.clone();
        }
        if let Some(default_reasoning_level) = self.default_reasoning_level {
            model.default_reasoning_level = default_reasoning_level;
        }
        if let Some(supported_reasoning_levels) = &self.supported_reasoning_levels {
            model.supported_reasoning_levels = supported_reasoning_levels.clone();
        }
        if let Some(shell_type) = self.shell_type {
            model.shell_type = shell_type;
        }
        if let Some(visibility) = self.visibility {
            model.visibility = visibility;
        }
        if let Some(supported_in_api) = self.supported_in_api {
            model.supported_in_api = supported_in_api;
        }
        if let Some(priority) = self.priority {
            model.priority = priority;
        }
        if let Some(upgrade) = &self.upgrade {
            model.upgrade = upgrade.clone();
        }
        if let Some(base_instructions) = &self.base_instructions {
            model.base_instructions = base_instructions.clone();
        }
        if self.clear_model_messages {
            model.model_messages = None;
        }
        if let Some(model_messages_patch) = &self.model_messages {
            let mut messages = model.model_messages.take().unwrap_or(ModelMessages {
                instructions_template: None,
                instructions_variables: None,
            });
            model_messages_patch.apply_to(&mut messages);
            model.model_messages = Some(messages);
        }
        if let Some(supports_reasoning_summaries) = self.supports_reasoning_summaries {
            model.supports_reasoning_summaries = supports_reasoning_summaries;
        }
        if let Some(support_verbosity) = self.support_verbosity {
            model.support_verbosity = support_verbosity;
        }
        if let Some(default_verbosity) = self.default_verbosity {
            model.default_verbosity = default_verbosity;
        }
        if let Some(apply_patch_tool_type) = &self.apply_patch_tool_type {
            model.apply_patch_tool_type = apply_patch_tool_type.clone();
        }
        if let Some(truncation_policy) = self.truncation_policy {
            model.truncation_policy = truncation_policy;
        }
        if let Some(supports_parallel_tool_calls) = self.supports_parallel_tool_calls {
            model.supports_parallel_tool_calls = supports_parallel_tool_calls;
        }
        if let Some(context_window) = self.context_window {
            model.context_window = context_window;
        }
        if let Some(auto_compact_token_limit) = self.auto_compact_token_limit {
            model.auto_compact_token_limit = auto_compact_token_limit;
        }
        if let Some(effective_context_window_percent) = self.effective_context_window_percent {
            model.effective_context_window_percent = effective_context_window_percent;
        }
        if let Some(experimental_supported_tools) = &self.experimental_supported_tools {
            model.experimental_supported_tools = experimental_supported_tools.clone();
        }
        if let Some(input_modalities) = &self.input_modalities {
            model.input_modalities = input_modalities.clone();
        }

        let template_still_effective = model
            .model_messages
            .as_ref()
            .and_then(|messages| messages.instructions_template.as_ref())
            .is_some();
        let template_patched = self
            .model_messages
            .as_ref()
            .and_then(|messages| messages.instructions_template.as_ref())
            .is_some();
        if self.base_instructions.is_some()
            && template_still_effective
            && !template_patched
            && !self.clear_model_messages
            && !has_final_instruction_override
        {
            warn!(
                model = %model.slug,
                "model_overlay base_instructions patch may not affect runtime instructions because instructions_template remains set"
            );
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModelMessagesPatch {
    pub instructions_template: Option<String>,
    pub instructions_variables: Option<ModelInstructionsVariablesPatch>,
}

impl ModelMessagesPatch {
    fn apply_to(&self, messages: &mut ModelMessages) {
        if let Some(instructions_template) = &self.instructions_template {
            messages.instructions_template = Some(instructions_template.clone());
        }
        if let Some(instructions_variables_patch) = &self.instructions_variables {
            let variables =
                messages
                    .instructions_variables
                    .get_or_insert(ModelInstructionsVariables {
                        personality_default: None,
                        personality_friendly: None,
                        personality_pragmatic: None,
                    });
            instructions_variables_patch.apply_to(variables);
        }
    }

    fn has_any_field(&self) -> bool {
        self.instructions_template.is_some()
            || self
                .instructions_variables
                .as_ref()
                .is_some_and(ModelInstructionsVariablesPatch::has_any_field)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModelInstructionsVariablesPatch {
    pub personality_default: Option<String>,
    pub personality_friendly: Option<String>,
    pub personality_pragmatic: Option<String>,
}

impl ModelInstructionsVariablesPatch {
    fn apply_to(&self, variables: &mut ModelInstructionsVariables) {
        if let Some(personality_default) = &self.personality_default {
            variables.personality_default = Some(personality_default.clone());
        }
        if let Some(personality_friendly) = &self.personality_friendly {
            variables.personality_friendly = Some(personality_friendly.clone());
        }
        if let Some(personality_pragmatic) = &self.personality_pragmatic {
            variables.personality_pragmatic = Some(personality_pragmatic.clone());
        }
    }

    fn has_any_field(&self) -> bool {
        self.personality_default.is_some()
            || self.personality_friendly.is_some()
            || self.personality_pragmatic.is_some()
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct ModelOverlayToml {
    #[serde(flatten)]
    pub patch: ModelInfoPatchToml,
    pub final_instruction_override: Option<String>,
    pub final_instruction_override_file: Option<AbsolutePathBuf>,
    #[serde(default)]
    pub models: Vec<ModelOverlayEntryToml>,
    #[serde(default, skip_serializing)]
    #[schemars(skip)]
    pub review_model: Option<toml::Value>,
    #[serde(default, skip_serializing)]
    #[schemars(skip)]
    pub review_model_provider: Option<toml::Value>,
}

impl ModelOverlayToml {
    pub(crate) fn resolve(self) -> io::Result<ModelOverlay> {
        reject_misplaced_review_fields(
            "model_overlay",
            self.review_model.as_ref(),
            self.review_model_provider.as_ref(),
        )?;
        let final_instruction_override = resolve_text_field(
            self.final_instruction_override,
            self.final_instruction_override_file,
            "model_overlay.final_instruction_override",
        )?;
        let cross_model = self.patch.resolve("model_overlay")?;
        let models = self
            .models
            .into_iter()
            .map(ModelOverlayEntryToml::resolve)
            .collect::<io::Result<Vec<_>>>()?;

        Ok(ModelOverlay {
            cross_model,
            final_instruction_override,
            models,
        })
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct ModelOverlayEntryToml {
    pub slug: String,
    #[serde(flatten)]
    pub patch: ModelInfoPatchToml,
    pub final_instruction_override: Option<String>,
    pub final_instruction_override_file: Option<AbsolutePathBuf>,
    #[serde(default, skip_serializing)]
    #[schemars(skip)]
    pub review_model: Option<toml::Value>,
    #[serde(default, skip_serializing)]
    #[schemars(skip)]
    pub review_model_provider: Option<toml::Value>,
}

impl ModelOverlayEntryToml {
    fn resolve(self) -> io::Result<ModelOverlayEntry> {
        if self.slug.trim().is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "model_overlay.models[].slug must not be empty",
            ));
        }

        let context = format!("model_overlay.models[slug={}]", self.slug);
        reject_misplaced_review_fields(
            &context,
            self.review_model.as_ref(),
            self.review_model_provider.as_ref(),
        )?;
        let final_instruction_override = resolve_text_field(
            self.final_instruction_override,
            self.final_instruction_override_file,
            format!("{context}.final_instruction_override"),
        )?;
        let patch = self.patch.resolve(&context)?;

        Ok(ModelOverlayEntry {
            slug: self.slug,
            patch,
            final_instruction_override,
        })
    }
}

fn reject_misplaced_review_fields(
    context: &str,
    review_model: Option<&toml::Value>,
    review_model_provider: Option<&toml::Value>,
) -> io::Result<()> {
    let mut fields = Vec::new();
    if review_model.is_some() {
        fields.push("review_model");
    }
    if review_model_provider.is_some() {
        fields.push("review_model_provider");
    }
    if fields.is_empty() {
        return Ok(());
    }

    let fields = fields.join("`, `");
    Err(io::Error::new(
        io::ErrorKind::InvalidData,
        format!(
            "`{fields}` must be set at the top level of config.toml, not under `{context}`. In TOML, keys after `[[model_overlay.models]]` belong to that model entry until the next table header."
        ),
    ))
}

#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct ModelInfoPatchToml {
    pub display_name: Option<String>,
    pub description: Option<String>,
    pub default_reasoning_level: Option<ReasoningEffort>,
    pub supported_reasoning_levels: Option<Vec<ReasoningEffortPreset>>,
    pub shell_type: Option<ConfigShellToolType>,
    pub visibility: Option<ModelVisibility>,
    pub supported_in_api: Option<bool>,
    pub priority: Option<i32>,
    pub upgrade: Option<ModelInfoUpgrade>,
    pub base_instructions: Option<String>,
    pub base_instructions_file: Option<AbsolutePathBuf>,
    pub instructions_template: Option<String>,
    pub instructions_template_file: Option<AbsolutePathBuf>,
    pub model_messages: Option<ModelMessagesPatchToml>,
    pub clear_model_messages: Option<bool>,
    pub supports_reasoning_summaries: Option<bool>,
    pub support_verbosity: Option<bool>,
    pub default_verbosity: Option<Verbosity>,
    pub apply_patch_tool_type: Option<ApplyPatchToolType>,
    pub truncation_policy: Option<TruncationPolicyConfig>,
    pub supports_parallel_tool_calls: Option<bool>,
    pub context_window: Option<i64>,
    pub auto_compact_token_limit: Option<i64>,
    pub effective_context_window_percent: Option<i64>,
    pub experimental_supported_tools: Option<Vec<String>>,
    pub input_modalities: Option<Vec<InputModality>>,
}

impl ModelInfoPatchToml {
    fn resolve(self, context: &str) -> io::Result<ModelInfoPatch> {
        let base_instructions = resolve_text_field(
            self.base_instructions,
            self.base_instructions_file,
            format!("{context}.base_instructions"),
        )?;
        let root_instructions_template = resolve_text_field(
            self.instructions_template,
            self.instructions_template_file,
            format!("{context}.instructions_template"),
        )?;
        let mut model_messages = self
            .model_messages
            .map(|model_messages| model_messages.resolve(&format!("{context}.model_messages")))
            .transpose()?;

        if let Some(instructions_template) = root_instructions_template {
            let messages = model_messages.get_or_insert_with(ModelMessagesPatch::default);
            if messages.instructions_template.is_some() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "{context}.instructions_template conflicts with {context}.model_messages.instructions_template"
                    ),
                ));
            }
            messages.instructions_template = Some(instructions_template);
        }

        let clear_model_messages = self.clear_model_messages.unwrap_or(false);
        if clear_model_messages
            && model_messages
                .as_ref()
                .is_some_and(ModelMessagesPatch::has_any_field)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "{context}.clear_model_messages cannot be combined with model_messages or instructions_template fields"
                ),
            ));
        }

        Ok(ModelInfoPatch {
            display_name: self.display_name,
            description: self.description.map(Some),
            default_reasoning_level: self.default_reasoning_level.map(Some),
            supported_reasoning_levels: self.supported_reasoning_levels,
            shell_type: self.shell_type,
            visibility: self.visibility,
            supported_in_api: self.supported_in_api,
            priority: self.priority,
            upgrade: self.upgrade.map(Some),
            base_instructions,
            model_messages,
            clear_model_messages,
            supports_reasoning_summaries: self.supports_reasoning_summaries,
            support_verbosity: self.support_verbosity,
            default_verbosity: self.default_verbosity.map(Some),
            apply_patch_tool_type: self.apply_patch_tool_type.map(Some),
            truncation_policy: self.truncation_policy,
            supports_parallel_tool_calls: self.supports_parallel_tool_calls,
            context_window: self.context_window.map(Some),
            auto_compact_token_limit: self.auto_compact_token_limit.map(Some),
            effective_context_window_percent: self.effective_context_window_percent,
            experimental_supported_tools: self.experimental_supported_tools,
            input_modalities: self.input_modalities,
        })
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct ModelMessagesPatchToml {
    pub instructions_template: Option<String>,
    pub instructions_template_file: Option<AbsolutePathBuf>,
    pub instructions_variables: Option<ModelInstructionsVariablesPatchToml>,
}

impl ModelMessagesPatchToml {
    fn resolve(self, context: &str) -> io::Result<ModelMessagesPatch> {
        Ok(ModelMessagesPatch {
            instructions_template: resolve_text_field(
                self.instructions_template,
                self.instructions_template_file,
                format!("{context}.instructions_template"),
            )?,
            instructions_variables: self
                .instructions_variables
                .map(|variables| variables.resolve(&format!("{context}.instructions_variables")))
                .transpose()?,
        })
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct ModelInstructionsVariablesPatchToml {
    pub personality_default: Option<String>,
    pub personality_default_file: Option<AbsolutePathBuf>,
    pub personality_friendly: Option<String>,
    pub personality_friendly_file: Option<AbsolutePathBuf>,
    pub personality_pragmatic: Option<String>,
    pub personality_pragmatic_file: Option<AbsolutePathBuf>,
}

impl ModelInstructionsVariablesPatchToml {
    fn resolve(self, context: &str) -> io::Result<ModelInstructionsVariablesPatch> {
        Ok(ModelInstructionsVariablesPatch {
            personality_default: resolve_text_field(
                self.personality_default,
                self.personality_default_file,
                format!("{context}.personality_default"),
            )?,
            personality_friendly: resolve_text_field(
                self.personality_friendly,
                self.personality_friendly_file,
                format!("{context}.personality_friendly"),
            )?,
            personality_pragmatic: resolve_text_field(
                self.personality_pragmatic,
                self.personality_pragmatic_file,
                format!("{context}.personality_pragmatic"),
            )?,
        })
    }
}

fn resolve_text_field(
    inline: Option<String>,
    file: Option<AbsolutePathBuf>,
    context: impl AsRef<str>,
) -> io::Result<Option<String>> {
    let context = context.as_ref();
    match (inline, file) {
        (Some(_), Some(_)) => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{context} cannot set both inline and _file variants"),
        )),
        (Some(value), None) => Ok(Some(value)),
        (None, Some(path)) => read_non_empty_file_preserve_contents(&path, context).map(Some),
        (None, None) => Ok(None),
    }
}

fn read_non_empty_file_preserve_contents(
    path: &AbsolutePathBuf,
    context: &str,
) -> io::Result<String> {
    let contents = std::fs::read_to_string(path).map_err(|err| {
        io::Error::new(
            err.kind(),
            format!("failed to read {context} file {}: {err}", path.display()),
        )
    })?;

    if contents.trim().is_empty() {
        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{context} file is empty: {}", path.display()),
        ))
    } else {
        Ok(contents)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn clear_model_messages_rejects_template_patch() {
        let patch = ModelInfoPatchToml {
            clear_model_messages: Some(true),
            instructions_template: Some("template".to_string()),
            ..Default::default()
        };

        let err = patch
            .resolve("model_overlay.models[slug=test]")
            .expect_err("clear plus template should be invalid");

        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn root_template_shorthand_conflicts_with_nested_template() {
        let patch = ModelInfoPatchToml {
            instructions_template: Some("root".to_string()),
            model_messages: Some(ModelMessagesPatchToml {
                instructions_template: Some("nested".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        };

        let err = patch
            .resolve("model_overlay.models[slug=test]")
            .expect_err("duplicate template fields should be invalid");

        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn apply_overlay_adds_custom_model_from_fallback() {
        let overlay = ModelOverlay {
            models: vec![ModelOverlayEntry {
                slug: "private-model".to_string(),
                patch: ModelInfoPatch {
                    context_window: Some(Some(1_000_000)),
                    auto_compact_token_limit: Some(Some(900_000)),
                    ..Default::default()
                },
                final_instruction_override: None,
            }],
            ..Default::default()
        };

        let overlaid = overlay.apply_to_candidates(&[]);

        assert_eq!(overlaid.len(), 1);
        assert_eq!(overlaid[0].slug, "private-model");
        assert_eq!(overlaid[0].context_window, Some(1_000_000));
        assert_eq!(overlaid[0].auto_compact_token_limit, Some(900_000));
        assert_eq!(overlaid[0].display_name, "private-model");
    }
}
