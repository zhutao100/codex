//! Opt-in comparisons of completed, provider-projected logical requests. Never used to select a request.
use super::*;
use codex_protocol::protocol::TokenUsage;
use serde::Serialize;
use std::hash::BuildHasher;
use std::hash::Hasher;
use std::hash::RandomState;
use std::io::Write;
use std::time::Instant;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum CacheOperation {
    Work,
    Notes,
    Compact,
    #[default]
    Other,
}

type Stamp = (&'static str, u64);

struct Completed {
    id: String,
    when: Instant,
    epoch: u64,
    operation: CacheOperation,
    capture_start: Option<usize>,
    input: Vec<Stamp>,
    output: Vec<Stamp>,
    surface: [u64; 5],
}

#[derive(Default)]
pub(super) struct CacheTrace {
    key: RandomState,
    history: StdMutex<History>,
}

impl std::fmt::Debug for CacheTrace {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CacheTrace").finish_non_exhaustive()
    }
}

#[derive(Default)]
struct History {
    previous: Option<Arc<Completed>>,
    work: Option<Arc<Completed>>,
    epoch: u64,
}

struct HashWriter<H>(H);

impl<H: Hasher> Write for HashWriter<H> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.write(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl CacheTrace {
    fn hash(&self, value: &impl Serialize) -> Option<u64> {
        let mut writer = HashWriter(self.key.build_hasher());
        serde_json::to_writer(&mut writer, value).ok()?;
        Some(writer.0.finish())
    }

    fn stamp(&self, item: &ResponseItem) -> Option<Stamp> {
        Some((item.item_type(), self.hash(item)?))
    }

    pub(super) fn installed(&self) {
        if codex_api::telemetry::prompt_cache_trace_enabled() {
            self.history
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .epoch += 1;
        }
    }
}

pub(super) struct Pending {
    trace: Arc<CacheTrace>,
    thread: ThreadId,
    operation: CacheOperation,
    capture_start: Option<usize>,
    attempt: usize,
    transport: &'static str,
    reason: &'static str,
    fallback_cause: Option<&'static str>,
    pub(super) http_retries: Option<u64>,
    sent: Instant,
    epoch: u64,
    surface: [u64; 5],
    input: Vec<Stamp>,
    previous: Option<Arc<Completed>>,
    work: Option<Arc<Completed>>,
}

impl ModelClientSession {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn trace_cache_request(
        &self,
        endpoint: &str,
        model: &str,
        prompt: &ApiPrompt,
        options: &ApiResponsesOptions,
        transport: &'static str,
        reason: &'static str,
        validated_parent: Option<&str>,
    ) -> Option<Pending> {
        if !codex_api::telemetry::prompt_cache_trace_enabled() {
            return None;
        }
        let trace = &self.client.state.cache_trace;
        let (epoch, previous, work) = {
            let h = trace
                .history
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            (h.epoch, h.previous.clone(), h.work.clone())
        };
        let surface = [
            trace.hash(&endpoint)?,
            trace.hash(&model)?,
            trace.hash(&prompt.input.base)?,
            trace.hash(&prompt.tools)?,
            trace.hash(&(
                prompt.parallel_tool_calls,
                &options.reasoning,
                &options.text,
                &options.include,
                &options.service_tier,
                &options.prompt_cache_key,
                options.store_override,
            ))?,
        ];
        let mut input = Vec::with_capacity(prompt.input.items.len());
        // Incremental transport already compared these exact input/output items. Full HTTP/WS
        // sends need independent signatures; hash one item at a time, never retain its contents.
        if let Some(parent) = previous
            .as_ref()
            .filter(|p| Some(p.id.as_str()) == validated_parent)
        {
            input.extend_from_slice(&parent.input);
            input.extend_from_slice(&parent.output);
        }
        for item in prompt.input.items.get(input.len()..)? {
            input.push(trace.stamp(item)?);
        }
        Some(Pending {
            trace: Arc::clone(trace),
            thread: self.client.state.conversation_id,
            operation: self.cache_operation,
            capture_start: (self.cache_operation == CacheOperation::Notes)
                .then(|| {
                    prompt
                        .input
                        .items
                        .iter()
                        .rposition(crate::compact::is_work_notes_request_item)
                })
                .flatten(),
            attempt: self.cache_attempt,
            transport,
            reason,
            fallback_cause: if transport == "http" && reason != "full" {
                self.client.state.http_fallback_cause.get().copied()
            } else {
                None
            },
            http_retries: None,
            sent: Instant::now(),
            epoch,
            surface,
            input,
            previous,
            work,
        })
    }
}

#[derive(Debug, PartialEq)]
struct Comparison {
    shared: usize,
    previous_type: Option<&'static str>,
    current_type: Option<&'static str>,
    changed_surface: Option<&'static str>,
    shape: &'static str,
}

fn compare(
    current: &[Stamp],
    previous: &Completed,
    surface: [u64; 5],
    epoch: u64,
    operation: CacheOperation,
) -> Comparison {
    let mut expected = previous.input.iter().chain(&previous.output);
    let shared = current
        .iter()
        .zip(expected.clone())
        .take_while(|(a, b)| a == b)
        .count();
    let previous_type = expected.nth(shared).map(|(kind, _)| *kind);
    let current_type = current.get(shared).map(|(kind, _)| *kind);
    let changed = ["endpoint", "model", "base", "tools", "settings"]
        .into_iter()
        .zip(previous.surface.iter().zip(surface))
        .find_map(|(field, (a, b))| (*a != b).then_some(field));
    let shape = if epoch != previous.epoch {
        "new_history"
    } else if changed.is_some() {
        "settings_changed"
    } else if previous_type.is_none() {
        "extends_completed_work"
    } else if previous.operation == CacheOperation::Notes
        && operation == CacheOperation::Compact
        && previous.capture_start == Some(shared)
        && current
            .last()
            .is_some_and(|(kind, _)| *kind == "compaction_trigger")
    {
        "sibling_suffix"
    } else if shared < previous.input.len() {
        "previous_input_changed"
    } else {
        "previous_output_changed"
    };
    Comparison {
        shared,
        previous_type,
        current_type,
        changed_surface: changed,
        shape,
    }
}

impl Pending {
    pub(super) fn complete(self, id: &str, usage: Option<&TokenUsage>, output: &[ResponseItem]) {
        let Some(stamps) = output
            .iter()
            .map(|item| self.trace.stamp(item))
            .collect::<Option<Vec<_>>>()
        else {
            return;
        };
        let (sampled, returned) = output
            .iter()
            .filter_map(|item| match item {
                ResponseItem::Message {
                    role,
                    encrypted_content,
                    ..
                } if role == "assistant" => Some(encrypted_content.is_some()),
                ResponseItem::FunctionCall {
                    encrypted_content, ..
                }
                | ResponseItem::CustomToolCall {
                    encrypted_content, ..
                }
                | ResponseItem::LocalShellCall {
                    encrypted_content, ..
                }
                | ResponseItem::WebSearchCall {
                    encrypted_content, ..
                } => Some(encrypted_content.is_some()),
                _ => None,
            })
            .fold((0_usize, 0_usize), |(count, returned), present| {
                (count + 1, returned + usize::from(present))
            });
        for (baseline, prior) in [("previous", &self.previous), ("ordinary", &self.work)] {
            let comparison = prior
                .as_ref()
                .map(|p| compare(&self.input, p, self.surface, self.epoch, self.operation));
            tracing::debug!(target: "codex_core::cache_trace",
                thread_id = %self.thread, response_id = id, operation = ?self.operation,
                transport = self.transport, transport_reason = self.reason, http_fallback_cause = self.fallback_cause,
                http_retries = self.http_retries, attempt = self.attempt, epoch = self.epoch,
                baseline, baseline_id = prior.as_ref().map(|p| p.id.as_str()),
                baseline_epoch = prior.as_ref().map(|p| p.epoch),
                idle_ms = prior.as_ref().map(|p| self.sent.saturating_duration_since(p.when).as_millis() as u64),
                items = self.input.len(), baseline_input_items = prior.as_ref().map(|p| p.input.len()),
                baseline_output_items = prior.as_ref().map(|p| p.output.len()),
                shape = comparison.as_ref().map_or("no_baseline", |c| c.shape),
                shared_items = comparison.as_ref().map(|c| c.shared), previous_type = comparison.as_ref().and_then(|c| c.previous_type),
                current_type = comparison.as_ref().and_then(|c| c.current_type), changed_surface = comparison.as_ref().and_then(|c| c.changed_surface),
                input_tokens = usage.map(|u| u.input_tokens), accounted_cached_tokens = usage.map(|u| u.cached_input_tokens),
                accounted_uncached_tokens = usage.map(|u| u.input_tokens.saturating_sub(u.cached_input_tokens)),
                sampled_outputs = sampled, outputs_with_replay = returned,
                "completed logical cache comparison");
        }
        let completed = Arc::new(Completed {
            id: id.to_owned(),
            when: Instant::now(),
            epoch: self.epoch,
            operation: self.operation,
            capture_start: self.capture_start,
            input: self.input,
            output: stamps,
            surface: self.surface,
        });
        let mut h = self
            .trace
            .history
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if self.operation == CacheOperation::Work {
            h.work = Some(Arc::clone(&completed));
        }
        h.previous = Some(completed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn retried_capture_is_a_sibling_only_when_all_work_matches() {
        let work = ("message", 1);
        let later = ("message", 2);
        let trigger = ("compaction_trigger", 5);
        let baseline = Completed {
            id: "retry".into(),
            when: Instant::now(),
            epoch: 0,
            operation: CacheOperation::Notes,
            capture_start: Some(1),
            input: vec![work, ("message", 3), ("message", 4)],
            output: vec![],
            surface: [0; 5],
        };
        for (input, expected) in [
            (vec![work, trigger], "sibling_suffix"),
            (vec![work, later, trigger], "sibling_suffix"),
            (vec![later, trigger], "previous_input_changed"),
        ] {
            assert_eq!(
                compare(
                    &input,
                    &baseline,
                    baseline.surface,
                    0,
                    CacheOperation::Compact
                )
                .shape,
                expected
            );
        }
    }

    #[test]
    fn distinguish_settings_from_changed_history_and_unreplayed_output() {
        let prompt = ("message", 1);
        let output = ("function_call", 2);
        let edited = ("message", 3);
        let settings = [0; 5];
        let previous = Completed {
            id: "r".into(),
            when: Instant::now(),
            epoch: 0,
            operation: CacheOperation::Work,
            capture_start: None,
            input: vec![prompt],
            output: vec![output],
            surface: settings,
        };
        for (items, surface, shape, shared, changed) in [
            (
                vec![prompt, output],
                [0, 0, 0, 1, 0],
                "settings_changed",
                2,
                Some("tools"),
            ),
            (
                vec![edited, output],
                settings,
                "previous_input_changed",
                0,
                None,
            ),
            (vec![prompt], settings, "previous_output_changed", 1, None),
            (
                vec![prompt, output, edited],
                settings,
                "extends_completed_work",
                2,
                None,
            ),
        ] {
            let result = compare(&items, &previous, surface, 0, CacheOperation::Work);
            assert_eq!(
                (result.shape, result.shared, result.changed_surface),
                (shape, shared, changed)
            );
        }
    }
}
