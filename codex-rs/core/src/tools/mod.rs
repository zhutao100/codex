pub mod context;
pub mod events;
pub(crate) mod handlers;
pub mod orchestrator;
pub mod parallel;
pub mod registry;
pub mod router;
pub mod runtimes;
pub mod sandboxing;
pub mod spec;

use crate::exec::ExecToolCallOutput;
use crate::truncate::TruncationPolicy;
use crate::truncate::fit_result;
use crate::truncate::formatted_truncate_text;
use crate::truncate::truncate_text;
pub use router::ToolRouter;
use serde::Serialize;

// Telemetry preview limits: keep log events smaller than model budgets.
pub(crate) const TELEMETRY_PREVIEW_MAX_BYTES: usize = 2 * 1024; // 2 KiB
pub(crate) const TELEMETRY_PREVIEW_MAX_LINES: usize = 64; // lines
pub(crate) const TELEMETRY_PREVIEW_TRUNCATION_NOTICE: &str =
    "[... telemetry preview truncated ...]";

/// Format the combined exec output for sending back to the model.
/// Includes exit code and duration metadata; truncates large bodies safely.
pub fn format_exec_output_for_model_structured(
    exec_output: &ExecToolCallOutput,
    truncation_policy: TruncationPolicy,
    receipt: Option<&str>,
) -> String {
    let ExecToolCallOutput {
        exit_code,
        duration,
        ..
    } = exec_output;

    #[derive(Serialize)]
    struct ExecMetadata<'a> {
        exit_code: i32,
        duration_seconds: f32,
        #[serde(skip_serializing_if = "Option::is_none")]
        output_artifact: Option<&'a str>,
    }

    #[derive(Serialize)]
    struct ExecOutput<'a> {
        output: &'a str,
        metadata: ExecMetadata<'a>,
    }

    // round to 1 decimal place
    let duration_seconds = ((duration.as_secs_f32()) * 10.0).round() / 10.0;

    let formatted_output = format_exec_output_str(exec_output, truncation_policy);

    let mut payload = ExecOutput {
        output: &formatted_output,
        metadata: ExecMetadata {
            exit_code: *exit_code,
            duration_seconds,
            output_artifact: receipt,
        },
    };

    #[expect(clippy::expect_used)]
    let serialize =
        |payload: &ExecOutput<'_>| serde_json::to_string(payload).expect("serialize ExecOutput");
    let result = serialize(&payload);
    let budget = truncation_policy.byte_budget();
    // Legacy history reserves additional serialization overhead for uncaptured output.
    let budget = if receipt.is_none() {
        (truncation_policy * 1.2).byte_budget()
    } else {
        budget
    };
    if result.len() <= budget {
        return result;
    }

    // Fit the preview inside the serialized envelope, not around it.
    payload.output = "";
    let mut base = serialize(&payload);
    if base.len() > budget {
        payload.metadata.output_artifact = None;
        base = serialize(&payload);
    }
    // Control characters can take six bytes each after JSON escaping.
    let preview_budget = budget.saturating_sub(base.len()) / 6;
    let preview = fit_result("", &formatted_output, preview_budget);
    payload.output = preview.strip_prefix('\n').unwrap_or(&preview);
    serialize(&payload)
}

pub fn format_exec_output_for_model_freeform(
    exec_output: &ExecToolCallOutput,
    truncation_policy: TruncationPolicy,
    receipt: Option<&str>,
) -> String {
    // round to 1 decimal place
    let duration_seconds = ((exec_output.duration.as_secs_f32()) * 10.0).round() / 10.0;

    let content = build_content_with_timeout(exec_output);

    let total_lines = content.lines().count();

    let formatted_output = truncate_text(&content, truncation_policy);

    let mut sections = Vec::new();

    sections.push(format!("Exit code: {}", exec_output.exit_code));
    sections.push(format!("Wall time: {duration_seconds} seconds"));
    if total_lines != formatted_output.lines().count() {
        sections.push(format!("Total output lines: {total_lines}"));
    }

    let budget = truncation_policy.byte_budget();
    let without_artifact = format!("{}\nOutput:", sections.join("\n"));
    if without_artifact.len() > budget {
        return fit_result(&sections[0], &formatted_output, budget);
    }
    if let Some(receipt) = receipt {
        let with_artifact = format!("{}\n{receipt}\nOutput:", sections.join("\n"));
        let header = if with_artifact.len() <= budget {
            &with_artifact
        } else {
            &without_artifact
        };
        return fit_result(header, &formatted_output, budget);
    }
    sections.push("Output:".to_string());
    sections.push(formatted_output);

    sections.join("\n")
}

pub fn format_exec_output_str(
    exec_output: &ExecToolCallOutput,
    truncation_policy: TruncationPolicy,
) -> String {
    let content = build_content_with_timeout(exec_output);

    // Truncate for model consumption before serialization.
    formatted_truncate_text(&content, truncation_policy)
}

/// Extracts exec output content and prepends a timeout message if the command timed out.
fn build_content_with_timeout(exec_output: &ExecToolCallOutput) -> String {
    if exec_output.timed_out {
        format!(
            "command timed out after {} milliseconds\n{}",
            exec_output.duration.as_millis(),
            exec_output.aggregated_output.text
        )
    } else {
        exec_output.aggregated_output.text.clone()
    }
}
