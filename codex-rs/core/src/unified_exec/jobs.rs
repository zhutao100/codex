//! Completion ownership for explicitly awaited unified-exec jobs. The
//! originating turn stays alive; an exit watcher cannot start a new turn.

use std::io;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use codex_protocol::models::ContentItem;
use codex_protocol::models::FunctionCallOutputBody;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::ProgressTraceCategory;
use codex_protocol::protocol::ProgressTraceState;
use serde::Deserialize;
use serde::Serialize;
use tokio::io::AsyncReadExt;
use tokio::sync::Mutex;
use tokio::sync::Notify;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::UnifiedExecContext;
use super::UnifiedExecProcess;
use crate::session::session::Session;
use crate::session::turn_context::TurnContext;
use crate::state::TurnState;

const MAX_JOBS_PER_TURN: usize = 4;
const MAX_JOBS_PER_SESSION: usize = 64;
const CHECKPOINT_TIMEOUT: Duration = Duration::from_secs(1);
const WAIT_LIMIT: Duration = Duration::from_secs(300);
const REGISTRATION: &str = "Awaiting background job ";
const DELIVERY: &str = "Background job outcome ";
const INCOMPLETE: &str = "Background job not delivered ";

#[derive(Debug, Deserialize, Serialize)]
struct JobRecord {
    id: Uuid,
    owner: String,
    call_id: String,
    outcome: Option<JobOutcome>,
    #[serde(default)]
    admitted: bool,
    #[serde(default)]
    superseded: bool,
    #[serde(skip)]
    discarded: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct JobOutcome {
    exit_code: Option<i32>,
    artifact: Option<String>,
    #[serde(default)]
    stop_requested: bool,
}

#[derive(Debug)]
pub(crate) struct BackgroundJob {
    path: PathBuf,
    record: Mutex<JobRecord>,
    changed: Notify,
}

pub(crate) async fn register(ctx: &UnifiedExecContext) -> io::Result<Arc<BackgroundJob>> {
    tokio::time::timeout(CHECKPOINT_TIMEOUT, async {
        if !ctx.session.flush_job_ledger().await {
            return Err(io::Error::other("rollout unavailable for job ownership"));
        }
        let mut jobs = ctx.turn.background_jobs.lock().await;
        if jobs.len() == MAX_JOBS_PER_TURN {
            return Err(io::Error::other("too many awaited jobs in this turn"));
        }
        let dir = catalog(ctx.session.as_ref(), ctx.turn.as_ref());
        tokio::fs::create_dir_all(&dir).await?;
        let mut entries = tokio::fs::read_dir(&dir).await?;
        for _ in 0..MAX_JOBS_PER_SESSION {
            if entries.next_entry().await?.is_none() {
                let id = Uuid::new_v4();
                let path = dir.join(format!("{id}.json"));
                let record = JobRecord {
                    id,
                    owner: ctx.turn.sub_id.clone(),
                    call_id: ctx.call_id.clone(),
                    outcome: None,
                    admitted: false,
                    superseded: false,
                    discarded: false,
                };
                publish(&path, &record).await?;
                let job = Arc::new(BackgroundJob {
                    path,
                    record: Mutex::new(record),
                    changed: Notify::new(),
                });
                jobs.push(Arc::clone(&job));
                return Ok(job);
            }
        }
        Err(io::Error::other("session job catalog is full"))
    })
    .await
    .unwrap_or_else(|_| {
        Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "registering job timed out",
        ))
    })
}

fn catalog(session: &Session, turn: &TurnContext) -> PathBuf {
    turn.config
        .codex_home
        .join("exec-jobs")
        .join(session.conversation_id.to_string())
}

async fn publish(path: &Path, record: &JobRecord) -> io::Result<()> {
    let temp = path.with_extension("tmp");
    tokio::fs::write(&temp, serde_json::to_vec(record)?).await?;
    tokio::fs::rename(&temp, path).await
}

async fn load(path: &Path) -> io::Result<JobRecord> {
    let mut data = Vec::new();
    tokio::fs::File::open(path)
        .await?
        .take(8192)
        .read_to_end(&mut data)
        .await?;
    Ok(serde_json::from_slice(&data)?)
}

pub(crate) async fn status(session: &Session, turn: &TurnContext, id: Uuid) -> io::Result<String> {
    let path = catalog(session, turn).join(format!("{id}.json"));
    let record = tokio::time::timeout(CHECKPOINT_TIMEOUT, load(&path))
        .await
        .map_err(io::Error::other)??;
    Ok(match record.outcome {
        Some(outcome) => format!("{DELIVERY}{id}: {}", outcome.report()),
        None => format!(
            "{INCOMPLETE}{id}: no durable terminal outcome yet; the process may still be running or may no longer be attached."
        ),
    })
}

impl JobOutcome {
    fn report(&self) -> String {
        let Self {
            exit_code,
            artifact,
            stop_requested,
        } = self;
        let exit = exit_code.map_or_else(
            || "exit status unavailable".to_string(),
            |code| format!("exited with code {code}"),
        );
        let exit = if *stop_requested {
            format!("stop was requested; {exit}")
        } else {
            exit
        };
        if let Some(artifact) = artifact {
            format!("{exit}.\n{artifact}")
        } else {
            format!("{exit}.")
        }
    }
}

impl BackgroundJob {
    pub(super) async fn discard_provisional(self: &Arc<Self>, turn: &TurnContext) {
        let _ = tokio::time::timeout(CHECKPOINT_TIMEOUT, self.discard()).await;
        turn.background_jobs
            .lock()
            .await
            .retain(|job| !Arc::ptr_eq(job, self));
    }

    async fn discard(&self) {
        let mut record = self.record.lock().await;
        record.discarded = true;
        if let Err(error) = tokio::fs::remove_file(&self.path).await
            && error.kind() != io::ErrorKind::NotFound
        {
            tracing::warn!(%error, "failed to reclaim a provisional job registration");
        }
    }

    pub(crate) async fn acknowledgment(&self) -> String {
        registration(self.record.lock().await.id)
    }

    pub(super) async fn complete(&self, process: &UnifiedExecProcess) {
        let mut record = self.record.lock().await;
        if record.discarded || record.outcome.is_some() {
            return;
        }
        process.capture.seal();
        let outcome = JobOutcome {
            exit_code: process.exit_code(),
            artifact: process.capture.receipt().await,
            stop_requested: process.stop_requested(),
        };
        record.outcome = Some(outcome);
        if !matches!(
            tokio::time::timeout(CHECKPOINT_TIMEOUT, publish(&self.path, &record)).await,
            Ok(Ok(()))
        ) {
            // Wake the current waiter without claiming that the terminal
            // outcome was retained; a restart sees the last acknowledged state.
            record.outcome = None;
            self.changed.notify_one();
            return;
        }
        self.changed.notify_one();
    }

    async fn mark_admitted(&self) {
        let mut record = self.record.lock().await;
        record.admitted = true;
        let _ = tokio::time::timeout(CHECKPOINT_TIMEOUT, publish(&self.path, &record)).await;
    }

    async fn supersede(&self) {
        let _ = tokio::time::timeout(CHECKPOINT_TIMEOUT, async {
            let mut record = self.record.lock().await;
            record.superseded = true;
            publish(&self.path, &record).await
        })
        .await;
    }

    async fn wait(&self, deadline: Instant) -> (Uuid, Option<JobOutcome>) {
        // There is one waiter for this job. notify_one stores a permit when
        // the process exits before the waiter is installed.
        let record = self.record.lock().await;
        if record.outcome.is_some() {
            return (record.id, record.outcome.clone());
        }
        let id = record.id;
        drop(record);
        let _ = tokio::time::timeout_at(deadline, self.changed.notified()).await;
        (id, self.record.lock().await.outcome.clone())
    }
}

fn registration(id: Uuid) -> String {
    format!("{REGISTRATION}{id}. The harness will wait for this job after the current tool batch.")
}

fn is_acknowledged(history: &[ResponseItem], record: &JobRecord) -> bool {
    let line = registration(record.id);
    let tail = history
        .iter()
        .rev()
        .take_while(|item| !crate::context_manager::is_user_turn_boundary(item));
    let paired = tail.clone().any(|item| {
        matches!(item, ResponseItem::FunctionCall { name, call_id, .. }
            if name == "exec_command" && *call_id == record.call_id)
    });
    paired
        && tail.into_iter().any(|item| {
            matches!(
                item,
                ResponseItem::FunctionCallOutput { call_id, output }
                    if *call_id == record.call_id
                        && matches!(&output.body, FunctionCallOutputBody::Text(text)
                            if text.lines().any(|text| text == line))
            )
        })
}

fn is_delivered(history: &[ResponseItem], id: Uuid) -> bool {
    let prefix = format!("{DELIVERY}{id}:");
    history.iter().any(|item| {
        matches!(item, ResponseItem::Message { role, content, .. }
            if role == "developer" && matches!(content.as_slice(),
                [ContentItem::InputText { text }] if text.starts_with(&prefix)))
    })
}

fn delivery(id: Uuid, outcome: Option<&JobOutcome>) -> ResponseItem {
    let text = match outcome {
        Some(outcome) => format!("{DELIVERY}{id}: {}", outcome.report()),
        None => format!(
            "{INCOMPLETE}{id}: no durable terminal outcome was available when the wait ended. Use exec_job_status to inspect it later; do not assume success or rerun automatically."
        ),
    };
    ResponseItem::Message {
        encrypted_content: None,
        status: None,
        id: None,
        role: "developer".into(),
        content: vec![ContentItem::InputText { text }],
        end_turn: None,
        phase: None,
    }
}

/// Take the registrations belonging to this live turn after its paired tool
/// results were recorded. New input and cancellation win before admission.
pub(crate) async fn await_after_tools(
    session: &Session,
    turn: &TurnContext,
    state: &Arc<Mutex<TurnState>>,
    cancellation: &CancellationToken,
) {
    let jobs = std::mem::take(&mut *turn.background_jobs.lock().await);
    if jobs.is_empty() {
        return;
    }
    let history = session.clone_history().await;
    let mut acknowledged = Vec::new();
    for job in jobs {
        if is_acknowledged(history.raw_items(), &*job.record.lock().await) {
            acknowledged.push(job);
        } else {
            let _ = tokio::time::timeout(CHECKPOINT_TIMEOUT, job.discard()).await;
        }
    }
    if acknowledged.is_empty() {
        return;
    }

    let input_changed = {
        let state = state.lock().await;
        if state.has_pending_input() {
            drop(state);
            futures::future::join_all(acknowledged.iter().map(|job| job.supersede())).await;
            return;
        }
        state.input_changed().notified_owned()
    };
    if !session.flush_job_ledger().await {
        session
            .record_model_warning(
                "Cannot wait durably: rollout flush failed.".to_string(),
                turn,
            )
            .await;
        return;
    }
    session
        .emit_progress_trace(
            turn,
            ProgressTraceCategory::Waiting,
            ProgressTraceState::Started,
            Some("Awaiting registered shell work".into()),
            Some("exec_job_completion"),
        )
        .await;
    let deadline = Instant::now() + WAIT_LIMIT;
    let wait = futures::future::join_all(acknowledged.iter().map(|job| job.wait(deadline)));
    let outcomes = tokio::select! {
        biased;
        _ = cancellation.cancelled() => None,
        _ = input_changed => None,
        outcomes = wait => Some(outcomes),
    };
    session
        .emit_progress_trace(
            turn,
            ProgressTraceCategory::Waiting,
            ProgressTraceState::Completed,
            None,
            Some("exec_job_completion"),
        )
        .await;
    let Some(outcomes) = outcomes else {
        if !cancellation.is_cancelled() {
            futures::future::join_all(acknowledged.iter().map(|job| job.supersede())).await;
        }
        return;
    };
    let state = state.lock().await;
    if state.has_pending_input() {
        drop(state);
        futures::future::join_all(acknowledged.iter().map(|job| job.supersede())).await;
        return;
    }
    if cancellation.is_cancelled() {
        return;
    }
    let items = outcomes
        .iter()
        .map(|(id, outcome)| delivery(*id, outcome.as_ref()))
        .collect::<Vec<_>>();
    if session.record_job_outcomes(turn, &items).await.is_ok() {
        drop(state);
        let terminal = acknowledged
            .iter()
            .zip(&outcomes)
            .filter(|(_, (_, outcome))| outcome.is_some())
            .map(|(job, _)| job.mark_admitted());
        futures::future::join_all(terminal).await;
    } else {
        drop(state);
        session
            .record_model_warning(
                "Could not admit the retained job outcome to the rollout.".to_string(),
                turn,
            )
            .await;
    }
}

/// An explicit continuation can reconcile durable outcomes of its own
/// interrupted turn. Opening a rollout or finding a running record alone
/// never creates an inference owner.
pub(crate) async fn reconcile(
    session: &Session,
    turn: &TurnContext,
    state: &Arc<Mutex<TurnState>>,
    cancellation: &CancellationToken,
) -> bool {
    let history = session.clone_history().await;
    let result = tokio::time::timeout(CHECKPOINT_TIMEOUT, async {
        let mut entries = tokio::fs::read_dir(catalog(session, turn)).await?;
        let mut items = Vec::new();
        let mut resumed_job = false;
        for _ in 0..MAX_JOBS_PER_SESSION {
            let Some(entry) = entries.next_entry().await? else {
                break;
            };
            if entry.path().extension().and_then(std::ffi::OsStr::to_str) != Some("json") {
                continue;
            }
            let Ok(record) = load(&entry.path()).await else {
                continue;
            };
            // The stable job acknowledgment in the surviving request tail
            // spans continuation IDs. The latest turn ID alone can change
            // before an interrupted continuation admits the outcome.
            if !record.superseded && is_acknowledged(history.raw_items(), &record) {
                resumed_job = true;
                if !record.admitted && !is_delivered(history.raw_items(), record.id) {
                    items.push((record.id, delivery(record.id, record.outcome.as_ref())));
                }
            }
        }
        items.sort_by_key(|(id, _)| *id);
        let items = items.into_iter().map(|(_, item)| item).collect::<Vec<_>>();
        Ok::<_, io::Error>((resumed_job, items))
    })
    .await;
    let Ok(Ok((resumed_job, items))) = result else {
        return false;
    };
    if !items.is_empty() {
        let state = state.lock().await;
        if !cancellation.is_cancelled() && !state.has_pending_input() {
            let _ = session.record_job_outcomes(turn, &items).await;
        }
    }
    resumed_job
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_protocol::models::FunctionCallOutputPayload;
    use pretty_assertions::assert_eq;

    fn record(id: Uuid) -> JobRecord {
        JobRecord {
            id,
            owner: "original-turn".into(),
            call_id: "exec-call".into(),
            outcome: None,
            admitted: false,
            superseded: false,
            discarded: false,
        }
    }

    #[tokio::test]
    async fn warning_between_call_and_result_keeps_the_acknowledgment_in_scope() {
        let (session, turn) = crate::session::tests::make_session_and_context().await;
        let id = Uuid::new_v4();
        let record = record(id);
        session
            .record_into_history(
                &[ResponseItem::FunctionCall {
                    encrypted_content: None,
                    status: None,
                    id: None,
                    name: "exec_command".into(),
                    arguments: "{}".into(),
                    call_id: record.call_id.clone(),
                }],
                &turn,
            )
            .await;
        session
            .record_model_warning("too many unified exec processes", &turn)
            .await;
        session
            .record_into_history(
                &[ResponseItem::FunctionCallOutput {
                    call_id: record.call_id.clone(),
                    output: FunctionCallOutputPayload {
                        body: FunctionCallOutputBody::Text(registration(id)),
                        success: Some(true),
                    },
                }],
                &turn,
            )
            .await;
        let history = session.clone_history().await;
        assert!(is_acknowledged(history.raw_items(), &record));
    }

    #[tokio::test]
    async fn wait_expiry_is_not_terminal_delivery() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let id = Uuid::new_v4();
        let job = BackgroundJob {
            path: dir.path().join(format!("{id}.json")),
            record: Mutex::new(record(id)),
            changed: Notify::new(),
        };
        let (_, outcome) = job.wait(Instant::now()).await;
        assert!(outcome.is_none());
        let notice = delivery(id, outcome.as_ref());
        assert!(!is_delivered(&[notice], id));

        let mut record = job.record.lock().await;
        record.outcome = Some(JobOutcome {
            exit_code: Some(23),
            artifact: None,
            stop_requested: false,
        });
        publish(&job.path, &record).await?;
        assert!(!load(&job.path).await?.admitted);
        drop(record);
        let (job_id, outcome) = job.wait(Instant::now()).await;
        assert_eq!(
            (job_id, outcome.and_then(|outcome| outcome.exit_code)),
            (id, Some(23))
        );
        Ok(())
    }
}
