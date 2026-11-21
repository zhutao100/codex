use rand::Rng;
use std::cmp::Reverse;
use std::collections::HashMap;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Notify;
use tokio::sync::mpsc;
use tokio::time::Duration;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::exec_env::create_env;
use crate::exec_policy::ExecApprovalRequest;
use crate::protocol::ExecCommandSource;
use crate::sandboxing::ExecEnv;
use crate::tools::events::ToolEmitter;
use crate::tools::events::ToolEventCtx;
use crate::tools::events::ToolEventStage;
use crate::tools::orchestrator::ToolOrchestrator;
use crate::tools::runtimes::unified_exec::UnifiedExecRequest as UnifiedExecToolRequest;
use crate::tools::runtimes::unified_exec::UnifiedExecRuntime;
use crate::tools::sandboxing::ToolCtx;
use crate::truncate::TruncationPolicy;
use crate::truncate::approx_token_count;
use crate::truncate::formatted_truncate_text;
use crate::unified_exec::ExecCommandRequest;
use crate::unified_exec::MAX_UNIFIED_EXEC_PROCESSES;
use crate::unified_exec::ProcessEntry;
use crate::unified_exec::ProcessStore;
use crate::unified_exec::UnifiedExecContext;
use crate::unified_exec::UnifiedExecError;
use crate::unified_exec::UnifiedExecProcessManager;
use crate::unified_exec::UnifiedExecResponse;
use crate::unified_exec::WARNING_UNIFIED_EXEC_PROCESSES;
use crate::unified_exec::WriteStdinRequest;
use crate::unified_exec::async_watcher::emit_exec_end_for_unified_exec;
use crate::unified_exec::async_watcher::spawn_exit_watcher;
use crate::unified_exec::async_watcher::start_streaming_output;
use crate::unified_exec::clamp_stdin_yield_time;
use crate::unified_exec::clamp_yield_time;
use crate::unified_exec::generate_chunk_id;
use crate::unified_exec::head_tail_buffer::HeadTailBuffer;
use crate::unified_exec::process::OutputBuffer;
use crate::unified_exec::process::OutputHandles;
use crate::unified_exec::process::UnifiedExecProcess;
use crate::unified_exec::resolve_max_tokens;

const UNIFIED_EXEC_ENV: [(&str, &str); 10] = [
    ("NO_COLOR", "1"),
    ("TERM", "dumb"),
    ("LANG", "C.UTF-8"),
    ("LC_CTYPE", "C.UTF-8"),
    ("LC_ALL", "C.UTF-8"),
    ("COLORTERM", ""),
    ("PAGER", "cat"),
    ("GIT_PAGER", "cat"),
    ("GH_PAGER", "cat"),
    ("CODEX_CI", "1"),
];

fn apply_unified_exec_env(mut env: HashMap<String, String>) -> HashMap<String, String> {
    for (key, value) in UNIFIED_EXEC_ENV {
        env.insert(key.to_string(), value.to_string());
    }
    env
}

struct PreparedProcessHandles {
    writer_tx: mpsc::Sender<Vec<u8>>,
    output_buffer: OutputBuffer,
    output_notify: Arc<Notify>,
    cancellation_token: CancellationToken,
    command: Vec<String>,
    process_id: String,
    tty: bool,
    capture: crate::exec_output::OutputCapture,
}

impl UnifiedExecProcessManager {
    pub(crate) async fn allocate_process_id(&self) -> String {
        loop {
            let mut store = self.process_store.lock().await;

            let process_id = if !cfg!(test) && !cfg!(feature = "deterministic_process_ids") {
                // production mode → random
                rand::rng().random_range(1_000..100_000).to_string()
            } else {
                // test or deterministic mode
                let next = store
                    .reserved_process_ids
                    .iter()
                    .filter_map(|s| s.parse::<i32>().ok())
                    .max()
                    .map(|m| std::cmp::max(m, 999) + 1)
                    .unwrap_or(1000);

                next.to_string()
            };

            if store.reserved_process_ids.contains(&process_id) {
                continue;
            }

            store.reserved_process_ids.insert(process_id.clone());
            return process_id;
        }
    }

    pub(crate) async fn release_process_id(&self, process_id: &str) {
        let mut store = self.process_store.lock().await;
        store.remove(process_id);
    }

    pub(crate) async fn exec_command(
        &self,
        request: ExecCommandRequest,
        context: &UnifiedExecContext,
    ) -> Result<UnifiedExecResponse, UnifiedExecError> {
        let cwd = request
            .workdir
            .clone()
            .unwrap_or_else(|| context.turn.cwd.clone());

        let process = self
            .open_session_with_sandbox(&request, cwd.clone(), context)
            .await;

        let process = match process {
            Ok(process) => Arc::new(process),
            Err(err) => {
                if let Some(job) = &request.background_job {
                    job.discard_provisional(&context.turn).await;
                }
                self.release_process_id(&request.process_id).await;
                return Err(err);
            }
        };

        let transcript = Arc::new(tokio::sync::Mutex::new(HeadTailBuffer::default()));
        let event_ctx = ToolEventCtx::new(
            context.session.as_ref(),
            context.turn.as_ref(),
            &context.call_id,
            None,
        );
        let emitter = ToolEmitter::unified_exec(
            &request.command,
            cwd.clone(),
            ExecCommandSource::UnifiedExecStartup,
            Some(request.process_id.clone()),
        );
        emitter.emit(event_ctx, ToolEventStage::Begin).await;

        start_streaming_output(&process, context, Arc::clone(&transcript));

        let max_tokens = resolve_max_tokens(request.max_output_tokens);
        let yield_time_ms = clamp_yield_time(request.yield_time_ms);

        let start = Instant::now();
        // For the initial exec_command call, we both stream output to events
        // (via start_streaming_output above) and collect a snapshot here for
        // the tool response body.
        let OutputHandles {
            output_buffer,
            output_notify,
            cancellation_token,
        } = process.output_handles();
        let deadline = start + Duration::from_millis(yield_time_ms);
        let collected = Self::collect_output_until_deadline(
            &output_buffer,
            &output_notify,
            &cancellation_token,
            deadline,
        )
        .await;
        let wall_time = Instant::now().saturating_duration_since(start);

        let omitted_preview_bytes = collected.omitted_bytes();
        let raw_output = collected.into_preview();
        let text = String::from_utf8_lossy(&raw_output);
        let output = formatted_truncate_text(&text, TruncationPolicy::Tokens(max_tokens));
        let exit_code = process.exit_code();
        let has_exited = process.has_exited() || exit_code.is_some();
        if has_exited {
            if let Some(job) = &request.background_job {
                job.discard_provisional(&context.turn).await;
            }
            process.capture.seal();
        }
        let capture_receipt = process.capture.receipt().await;
        let chunk_id = generate_chunk_id();
        let process_id = request.process_id.clone();
        if has_exited {
            // Short‑lived command: emit ExecCommandEnd immediately using the
            // same helper as the background watcher, so all end events share
            // one implementation.
            let exit = exit_code.unwrap_or(-1);
            emit_exec_end_for_unified_exec(
                Arc::clone(&context.session),
                Arc::clone(&context.turn),
                context.call_id.clone(),
                request.command.clone(),
                cwd,
                Some(process_id),
                Arc::clone(&transcript),
                output.clone(),
                exit,
                wall_time,
            )
            .await;

            self.release_process_id(&request.process_id).await;
            if let Err(err) = process.check_for_sandbox_denial_with_text(&text).await {
                if let Some(receipt) = capture_receipt.as_ref() {
                    let message = crate::truncate::fit_result(
                        receipt,
                        &err.to_string(),
                        context
                            .turn
                            .truncation_policy
                            .byte_budget()
                            .saturating_sub(96),
                    );
                    return Err(UnifiedExecError::create_process(message));
                }
                return Err(err);
            }
        } else {
            // Long‑lived command: persist the process so write_stdin can reuse
            // it, and register a background watcher that will emit
            // ExecCommandEnd when the PTY eventually exits (even if no further
            // tool calls are made).
            self.store_process(
                Arc::clone(&process),
                context,
                &request.command,
                cwd.clone(),
                start,
                process_id,
                request.tty,
                Arc::clone(&transcript),
                request.background_job.clone(),
            )
            .await;
        };

        let original_token_count = (omitted_preview_bytes == 0).then(|| approx_token_count(&text));
        let job_acknowledgment = match (has_exited, &request.background_job) {
            (false, Some(job)) => Some(job.acknowledgment().await),
            _ => None,
        };
        let response = UnifiedExecResponse {
            event_call_id: context.call_id.clone(),
            chunk_id,
            wall_time,
            output,
            raw_output,
            omitted_preview_bytes,
            capture_receipt,
            job_acknowledgment,
            process_id: if has_exited {
                None
            } else {
                Some(request.process_id.clone())
            },
            exit_code,
            original_token_count,
            session_command: Some(request.command.clone()),
        };

        Ok(response)
    }

    pub(crate) async fn write_stdin(
        &self,
        request: WriteStdinRequest<'_>,
    ) -> Result<UnifiedExecResponse, UnifiedExecError> {
        let process_id = request.process_id.to_string();

        let PreparedProcessHandles {
            writer_tx,
            output_buffer,
            output_notify,
            cancellation_token,
            command: session_command,
            process_id,
            tty,
            capture,
            ..
        } = self.prepare_process_handles(process_id.as_str()).await?;

        if !request.input.is_empty() {
            if !tty {
                return Err(UnifiedExecError::StdinClosed);
            }
            Self::send_input(&writer_tx, request.input.as_bytes()).await?;
            // Give the remote process a brief window to react so that we are
            // more likely to capture its output in the poll below.
            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        let max_tokens = resolve_max_tokens(request.max_output_tokens);
        let yield_time_ms = clamp_stdin_yield_time(request.yield_time_ms, request.input.is_empty());
        let start = Instant::now();
        let deadline = start + Duration::from_millis(yield_time_ms);
        let collected = Self::collect_output_until_deadline(
            &output_buffer,
            &output_notify,
            &cancellation_token,
            deadline,
        )
        .await;
        let wall_time = Instant::now().saturating_duration_since(start);

        let omitted_preview_bytes = collected.omitted_bytes();
        let raw_output = collected.into_preview();
        let text = String::from_utf8_lossy(&raw_output);
        let output = formatted_truncate_text(&text, TruncationPolicy::Tokens(max_tokens));
        let original_token_count = (omitted_preview_bytes == 0).then(|| approx_token_count(&text));
        let chunk_id = generate_chunk_id();

        // After polling, refresh_process_state tells us whether the PTY is
        // still alive or has exited and been removed from the store; we thread
        // that through so the handler can tag TerminalInteraction with an
        // appropriate process_id and exit_code.
        let status = self.refresh_process_state(process_id.as_str()).await;
        let (process_id, exit_code, event_call_id) = match status {
            ProcessStatus::Alive {
                exit_code,
                call_id,
                process_id,
            } => (Some(process_id), exit_code, call_id),
            ProcessStatus::Exited { exit_code, entry } => {
                capture.seal();
                let call_id = entry.call_id.clone();
                (None, exit_code, call_id)
            }
            ProcessStatus::Unknown => {
                return Err(UnifiedExecError::UnknownProcessId {
                    process_id: request.process_id.to_string(),
                });
            }
        };

        let response = UnifiedExecResponse {
            event_call_id,
            chunk_id,
            wall_time,
            output,
            raw_output,
            omitted_preview_bytes,
            capture_receipt: capture.receipt().await,
            job_acknowledgment: None,
            process_id,
            exit_code,
            original_token_count,
            session_command: Some(session_command.clone()),
        };

        Ok(response)
    }

    async fn refresh_process_state(&self, process_id: &str) -> ProcessStatus {
        let mut store = self.process_store.lock().await;
        let Some(entry) = store.processes.get(process_id) else {
            return ProcessStatus::Unknown;
        };

        let exit_code = entry.process.exit_code();
        let process_id = entry.process_id.clone();

        if entry.process.has_exited() {
            let Some(entry) = store.remove(&process_id) else {
                return ProcessStatus::Unknown;
            };
            ProcessStatus::Exited {
                exit_code,
                entry: Box::new(entry),
            }
        } else {
            ProcessStatus::Alive {
                exit_code,
                call_id: entry.call_id.clone(),
                process_id,
            }
        }
    }

    async fn prepare_process_handles(
        &self,
        process_id: &str,
    ) -> Result<PreparedProcessHandles, UnifiedExecError> {
        let mut store = self.process_store.lock().await;
        let entry =
            store
                .processes
                .get_mut(process_id)
                .ok_or(UnifiedExecError::UnknownProcessId {
                    process_id: process_id.to_string(),
                })?;
        entry.last_used = Instant::now();
        if entry.background_job.is_some() {
            return Err(UnifiedExecError::RegisteredJob);
        }
        let OutputHandles {
            output_buffer,
            output_notify,
            cancellation_token,
        } = entry.process.output_handles();

        Ok(PreparedProcessHandles {
            writer_tx: entry.process.writer_sender(),
            output_buffer,
            output_notify,
            cancellation_token,
            command: entry.command.clone(),
            process_id: entry.process_id.clone(),
            tty: entry.tty,
            capture: entry.process.capture.clone(),
        })
    }

    async fn send_input(
        writer_tx: &mpsc::Sender<Vec<u8>>,
        data: &[u8],
    ) -> Result<(), UnifiedExecError> {
        writer_tx
            .send(data.to_vec())
            .await
            .map_err(|_| UnifiedExecError::WriteToStdin)
    }

    #[allow(clippy::too_many_arguments)]
    async fn store_process(
        &self,
        process: Arc<UnifiedExecProcess>,
        context: &UnifiedExecContext,
        command: &[String],
        cwd: PathBuf,
        started_at: Instant,
        process_id: String,
        tty: bool,
        transcript: Arc<tokio::sync::Mutex<HeadTailBuffer>>,
        background_job: Option<Arc<super::jobs::BackgroundJob>>,
    ) {
        let entry = ProcessEntry {
            process: Arc::clone(&process),
            call_id: context.call_id.clone(),
            process_id: process_id.clone(),
            command: command.to_vec(),
            tty,
            background_job: background_job.clone(),
            last_used: started_at,
        };
        let number_processes = {
            let mut store = self.process_store.lock().await;
            Self::prune_processes_if_needed(&mut store);
            store.processes.insert(process_id.clone(), entry);
            store.processes.len()
        };

        if number_processes >= WARNING_UNIFIED_EXEC_PROCESSES {
            context
                .session
                .record_model_warning(
                    format!("The maximum number of unified exec processes you can keep open is {WARNING_UNIFIED_EXEC_PROCESSES} and you currently have {number_processes} processes open. Reuse older processes or close them to prevent automatic pruning of old processes"),
                    &context.turn
                )
                .await;
        };

        spawn_exit_watcher(
            Arc::clone(&process),
            Arc::clone(&context.session),
            Arc::clone(&context.turn),
            context.call_id.clone(),
            command.to_vec(),
            cwd,
            process_id,
            transcript,
            started_at,
            background_job,
        );
    }

    pub(super) async fn release_finished_job(&self, id: &str, process: &Arc<UnifiedExecProcess>) {
        let mut store = self.process_store.lock().await;
        // Shutdown or pruning may already have released and reused the short ID.
        if store
            .processes
            .get(id)
            .is_some_and(|e| Arc::ptr_eq(&e.process, process))
        {
            store.remove(id);
        }
    }

    pub(crate) async fn open_session_with_exec_env(
        &self,
        env: &ExecEnv,
        tty: bool,
        capture: &mut crate::exec_output::OutputCapture,
    ) -> Result<UnifiedExecProcess, UnifiedExecError> {
        let (program, args) = env
            .command
            .split_first()
            .ok_or(UnifiedExecError::MissingCommandLine)?;

        let spawn_result = if tty {
            codex_utils_pty::pty::spawn_process_with_sink(
                program,
                args,
                env.cwd.as_path(),
                &env.env,
                &env.arg0,
                capture.sink(),
            )
            .await
        } else {
            codex_utils_pty::pipe::spawn_process_no_stdin_with_sink(
                program,
                args,
                env.cwd.as_path(),
                &env.env,
                &env.arg0,
                capture.sink(),
            )
            .await
        };
        let spawned = capture
            .after_spawn(spawn_result)
            .await
            .map_err(|err| UnifiedExecError::create_process(err.to_string()))?;
        UnifiedExecProcess::from_spawned(spawned, env.sandbox, capture.clone()).await
    }

    pub(super) async fn open_session_with_sandbox(
        &self,
        request: &ExecCommandRequest,
        cwd: PathBuf,
        context: &UnifiedExecContext,
    ) -> Result<UnifiedExecProcess, UnifiedExecError> {
        let env = apply_unified_exec_env(create_env(
            &context.turn.shell_environment_policy,
            Some(context.session.conversation_id),
        ));
        let features = context.session.features();
        let mut orchestrator = ToolOrchestrator::new();
        let mut runtime = UnifiedExecRuntime::new(self);
        let exec_approval_requirement = context
            .session
            .services
            .exec_policy
            .create_exec_approval_requirement_for_command(ExecApprovalRequest {
                features: &features,
                command: &request.command,
                approval_policy: context.turn.approval_policy,
                sandbox_policy: &context.turn.sandbox_policy,
                sandbox_permissions: request.sandbox_permissions,
                prefix_rule: request.prefix_rule.clone(),
            })
            .await;
        let req = UnifiedExecToolRequest::new(
            request.command.clone(),
            cwd,
            env,
            request.tty,
            request.sandbox_permissions,
            request.justification.clone(),
            exec_approval_requirement,
        );
        let tool_ctx = ToolCtx {
            session: context.session.as_ref(),
            turn: context.turn.as_ref(),
            call_id: context.call_id.clone(),
            tool_name: "exec_command".to_string(),
        };
        let result = orchestrator
            .run(
                &mut runtime,
                &req,
                &tool_ctx,
                context.turn.as_ref(),
                context.turn.approval_policy,
            )
            .await;
        match result {
            Ok(process) => Ok(process),
            Err(err) => {
                let mut message = format!("{err:?}");
                runtime.capture.seal();
                if let Some(receipt) = runtime.capture.receipt().await {
                    message = crate::truncate::fit_result(
                        &receipt,
                        &message,
                        context
                            .turn
                            .truncation_policy
                            .byte_budget()
                            .saturating_sub(96),
                    );
                }
                Err(UnifiedExecError::create_process(message))
            }
        }
    }

    pub(super) async fn collect_output_until_deadline(
        output_buffer: &OutputBuffer,
        output_notify: &Arc<Notify>,
        cancellation_token: &CancellationToken,
        deadline: Instant,
    ) -> HeadTailBuffer {
        const POST_EXIT_OUTPUT_GRACE: Duration = Duration::from_millis(50);

        let mut collected = HeadTailBuffer::default();
        let mut post_exit_deadline = None;
        loop {
            let now = Instant::now();
            if cancellation_token.is_cancelled() {
                post_exit_deadline
                    .get_or_insert_with(|| (now + POST_EXIT_OUTPUT_GRACE).min(deadline));
            }
            let notified = output_notify.notified();
            let drained = std::mem::take(&mut *output_buffer.lock().await);
            collected.append(drained);
            let wait_until = post_exit_deadline.unwrap_or(deadline);
            if now >= wait_until {
                break;
            }

            tokio::select! {
                _ = notified => {}
                _ = cancellation_token.cancelled(), if post_exit_deadline.is_none() => {}
                _ = tokio::time::sleep_until(wait_until) => {}
            }
        }

        collected
    }

    fn prune_processes_if_needed(store: &mut ProcessStore) -> bool {
        if store.processes.len() < MAX_UNIFIED_EXEC_PROCESSES {
            return false;
        }

        let meta: Vec<(String, Instant, bool)> = store
            .processes
            .iter()
            .map(|(id, entry)| (id.clone(), entry.last_used, entry.process.has_exited()))
            .collect();

        if let Some(process_id) = Self::process_id_to_prune_from_meta(&meta) {
            if let Some(entry) = store.remove(&process_id) {
                entry.process.terminate();
            }
            return true;
        }

        false
    }

    // Centralized pruning policy so we can easily swap strategies later.
    fn process_id_to_prune_from_meta(meta: &[(String, Instant, bool)]) -> Option<String> {
        if meta.is_empty() {
            return None;
        }

        let mut by_recency = meta.to_vec();
        by_recency.sort_by_key(|(_, last_used, _)| Reverse(*last_used));
        let protected: HashSet<String> = by_recency
            .iter()
            .take(8)
            .map(|(process_id, _, _)| process_id.clone())
            .collect();

        let mut lru = meta.to_vec();
        lru.sort_by_key(|(_, last_used, _)| *last_used);

        if let Some((process_id, _, _)) = lru
            .iter()
            .find(|(process_id, _, exited)| !protected.contains(process_id) && *exited)
        {
            return Some(process_id.clone());
        }

        lru.into_iter()
            .find(|(process_id, _, _)| !protected.contains(process_id))
            .map(|(process_id, _, _)| process_id)
    }

    pub(crate) async fn terminate_all_processes(&self) {
        let entries: Vec<ProcessEntry> = {
            let mut processes = self.process_store.lock().await;
            let entries: Vec<ProcessEntry> = processes
                .processes
                .drain()
                .map(|(_, entry)| entry)
                .collect();
            processes.reserved_process_ids.clear();
            entries
        };

        futures::future::join_all(entries.iter().map(|entry| async {
            entry.process.terminate_and_drain().await;
            if let Some(job) = &entry.background_job {
                job.complete(&entry.process).await;
            }
        }))
        .await;
    }
}

enum ProcessStatus {
    Alive {
        exit_code: Option<i32>,
        call_id: String,
        process_id: String,
    },
    Exited {
        exit_code: Option<i32>,
        entry: Box<ProcessEntry>,
    },
    Unknown,
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use tokio::time::Duration;
    use tokio::time::Instant;

    #[tokio::test]
    async fn wait_collects_a_bounded_preview_across_drains() {
        let output = Arc::new(tokio::sync::Mutex::new(HeadTailBuffer::default()));
        let notify = Arc::new(Notify::new());
        let exit = CancellationToken::new();
        let deadline =
            Instant::now() + Duration::from_millis(clamp_stdin_yield_time(u64::MAX, true));
        let collect = UnifiedExecProcessManager::collect_output_until_deadline(
            &output, &notify, &exit, deadline,
        );
        let send = async {
            for byte in b"ABC" {
                let cap = crate::unified_exec::UNIFIED_EXEC_OUTPUT_MAX_BYTES;
                output.lock().await.push_chunk(vec![*byte; cap]);
                notify.notify_waiters();
                while output.lock().await.retained_bytes() != 0 {
                    tokio::task::yield_now().await;
                }
            }
            exit.cancel();
        };
        let (buffer, ()) = tokio::time::timeout(Duration::from_secs(2), async {
            tokio::join!(collect, send)
        })
        .await
        .expect("collector returns promptly after exit");
        let cap = crate::unified_exec::UNIFIED_EXEC_OUTPUT_MAX_BYTES;
        let expected = [vec![b'A'; cap / 2], vec![b'C'; cap / 2]].concat();
        assert_eq!(
            (buffer.to_bytes(), buffer.omitted_bytes()),
            (expected, 2 * cap)
        );
    }

    #[test]
    fn only_empty_write_stdin_can_wait_past_the_interactive_cap() {
        assert_eq!(
            (
                clamp_yield_time(u64::MAX),
                clamp_stdin_yield_time(u64::MAX, false),
                clamp_stdin_yield_time(u64::MAX, true),
                clamp_stdin_yield_time(0, true),
            ),
            (30_000, 30_000, 300_000, 5_000)
        );
    }

    #[test]
    fn unified_exec_env_injects_defaults() {
        let env = apply_unified_exec_env(HashMap::new());
        let expected = HashMap::from([
            ("NO_COLOR".to_string(), "1".to_string()),
            ("TERM".to_string(), "dumb".to_string()),
            ("LANG".to_string(), "C.UTF-8".to_string()),
            ("LC_CTYPE".to_string(), "C.UTF-8".to_string()),
            ("LC_ALL".to_string(), "C.UTF-8".to_string()),
            ("COLORTERM".to_string(), String::new()),
            ("PAGER".to_string(), "cat".to_string()),
            ("GIT_PAGER".to_string(), "cat".to_string()),
            ("GH_PAGER".to_string(), "cat".to_string()),
            ("CODEX_CI".to_string(), "1".to_string()),
        ]);

        assert_eq!(env, expected);
    }

    #[test]
    fn unified_exec_env_overrides_existing_values() {
        let mut base = HashMap::new();
        base.insert("NO_COLOR".to_string(), "0".to_string());
        base.insert("PATH".to_string(), "/usr/bin".to_string());

        let env = apply_unified_exec_env(base);

        assert_eq!(env.get("NO_COLOR"), Some(&"1".to_string()));
        assert_eq!(env.get("PATH"), Some(&"/usr/bin".to_string()));
    }

    #[test]
    fn pruning_prefers_exited_processes_outside_recently_used() {
        let now = Instant::now();
        let id = |n: i32| n.to_string();
        let meta = vec![
            (id(1), now - Duration::from_secs(40), false),
            (id(2), now - Duration::from_secs(30), true),
            (id(3), now - Duration::from_secs(20), false),
            (id(4), now - Duration::from_secs(19), false),
            (id(5), now - Duration::from_secs(18), false),
            (id(6), now - Duration::from_secs(17), false),
            (id(7), now - Duration::from_secs(16), false),
            (id(8), now - Duration::from_secs(15), false),
            (id(9), now - Duration::from_secs(14), false),
            (id(10), now - Duration::from_secs(13), false),
        ];

        let candidate = UnifiedExecProcessManager::process_id_to_prune_from_meta(&meta);

        assert_eq!(candidate, Some(id(2)));
    }

    #[test]
    fn pruning_falls_back_to_lru_when_no_exited() {
        let now = Instant::now();
        let id = |n: i32| n.to_string();
        let meta = vec![
            (id(1), now - Duration::from_secs(40), false),
            (id(2), now - Duration::from_secs(30), false),
            (id(3), now - Duration::from_secs(20), false),
            (id(4), now - Duration::from_secs(19), false),
            (id(5), now - Duration::from_secs(18), false),
            (id(6), now - Duration::from_secs(17), false),
            (id(7), now - Duration::from_secs(16), false),
            (id(8), now - Duration::from_secs(15), false),
            (id(9), now - Duration::from_secs(14), false),
            (id(10), now - Duration::from_secs(13), false),
        ];

        let candidate = UnifiedExecProcessManager::process_id_to_prune_from_meta(&meta);

        assert_eq!(candidate, Some(id(1)));
    }

    #[test]
    fn pruning_protects_recent_processes_even_if_exited() {
        let now = Instant::now();
        let id = |n: i32| n.to_string();
        let meta = vec![
            (id(1), now - Duration::from_secs(40), false),
            (id(2), now - Duration::from_secs(30), false),
            (id(3), now - Duration::from_secs(20), true),
            (id(4), now - Duration::from_secs(19), false),
            (id(5), now - Duration::from_secs(18), false),
            (id(6), now - Duration::from_secs(17), false),
            (id(7), now - Duration::from_secs(16), false),
            (id(8), now - Duration::from_secs(15), false),
            (id(9), now - Duration::from_secs(14), false),
            (id(10), now - Duration::from_secs(13), true),
        ];

        let candidate = UnifiedExecProcessManager::process_id_to_prune_from_meta(&meta);

        // (10) is exited but among the last 8; we should drop the LRU outside that set.
        assert_eq!(candidate, Some(id(1)));
    }
}
