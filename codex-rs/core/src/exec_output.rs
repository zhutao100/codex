//! Reader-observed command output, independent of the preview and rollout.

use std::collections::HashMap;
use std::fs;
use std::io;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::PoisonError;
use std::sync::Weak;
use std::time::Duration;

use codex_protocol::ThreadId;
use codex_utils_pty::OutputEnd;
use codex_utils_pty::OutputSink;
use codex_utils_pty::OutputStream;
use serde::Deserialize;
use serde::Serialize;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncSeekExt;
use tokio::sync::OwnedSemaphorePermit;
use tokio::sync::Semaphore;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use uuid::Uuid;

use OutputStream::Stderr;
use OutputStream::Stdout;
use OutputStream::Terminal;

const ARTIFACT_BYTES: u64 = 8 * 1024 * 1024;
const SESSION_ARTIFACTS: usize = 64;
const SESSION_QUEUE_BYTES: usize = 4 * 1024 * 1024;
const OPERATIONS_TIMEOUT: Duration = Duration::from_millis(500);
const STREAMS: [OutputStream; 3] = [Stdout, Stderr, Terminal];

#[derive(Clone, Debug, Default)]
pub(crate) enum OutputCapture {
    #[default]
    Disabled,
    Open(Arc<Capture>),
    Unavailable(&'static str),
}

impl OutputCapture {
    pub(crate) async fn open(ctx: &crate::tools::sandboxing::ToolCtx<'_>, tty: bool) -> Self {
        if !ctx.turn.tools_config.recoverable_exec_output {
            return Self::Disabled;
        }
        match ctx
            .session
            .services
            .output_store
            .open(
                &ctx.turn.config.codex_home,
                ctx.session.conversation_id,
                tty,
            )
            .await
        {
            Ok(capture) => {
                // Publish the locator to dispatch before spawning the command.
                // Cancellation can drop the tool handler before it formats a receipt.
                ctx.turn
                    .exec_capture_locators
                    .lock()
                    .await
                    .insert(ctx.call_id.clone(), capture.id.clone());
                Self::Open(capture)
            }
            Err(reason) => Self::Unavailable(reason),
        }
    }

    pub(crate) fn sink(&self) -> Option<Arc<dyn OutputSink>> {
        match self {
            Self::Open(capture) => Some(Arc::clone(capture) as Arc<dyn OutputSink>),
            Self::Disabled | Self::Unavailable(_) => None,
        }
    }

    pub(crate) fn seal(&self) {
        if let Self::Open(capture) = self {
            capture.seal();
        }
    }

    /// Only the spawn boundary knows that no reader could have observed bytes.
    pub(crate) async fn after_spawn<T, E>(&mut self, result: Result<T, E>) -> Result<T, E> {
        if result.is_err()
            && let Self::Open(capture) = std::mem::take(self)
            && let Err(error) = tokio::fs::remove_dir_all(&capture.path).await
        {
            tracing::warn!(%error, "failed to reclaim an unstarted output artifact");
        }
        result
    }

    pub(crate) async fn receipt(&self) -> Option<String> {
        match self {
            Self::Disabled => None,
            Self::Open(capture) => Some(capture.receipt().await),
            Self::Unavailable(reason) => Some(format!("Output artifact unavailable: {reason}.")),
        }
    }
}

pub(crate) struct OutputStore {
    active: tokio::sync::Mutex<HashMap<Uuid, Weak<Capture>>>,
    queue: Arc<Semaphore>,
}

impl Default for OutputStore {
    fn default() -> Self {
        Self {
            active: tokio::sync::Mutex::new(HashMap::new()),
            queue: Arc::new(Semaphore::new(SESSION_QUEUE_BYTES)),
        }
    }
}

impl OutputStore {
    pub(crate) async fn open(
        &self,
        home: &Path,
        thread: ThreadId,
        tty: bool,
    ) -> Result<Arc<Capture>, &'static str> {
        let result = tokio::time::timeout(OPERATIONS_TIMEOUT, async {
            let mut active = self.active.lock().await;
            let catalog = home.join("exec-outputs").join(thread.to_string());
            tokio::fs::create_dir_all(&catalog).await?;
            let mut entries = tokio::fs::read_dir(&catalog).await?;
            let mut count = 0;
            while entries.next_entry().await?.is_some() {
                count += 1;
                if count == SESSION_ARTIFACTS {
                    return Ok(Err("session retention limit"));
                }
            }
            let id = Uuid::new_v4();
            let path = catalog.join(id.to_string());
            let state = Metadata::new(tty);
            let initial = state.clone();
            let prepare_path = path.clone();
            let files = tokio::task::spawn_blocking(move || prepare(&prepare_path, &initial))
                .await
                .map_err(io::Error::other)??;
            let capture = Capture::start(
                format!("{thread}/{id}"),
                path,
                state,
                files,
                Arc::clone(&self.queue),
            );
            active.retain(|_, capture| capture.strong_count() > 0);
            active.insert(id, Arc::downgrade(&capture));
            Ok::<_, io::Error>(Ok(capture))
        })
        .await;
        match result {
            Ok(Ok(capture)) => capture,
            Ok(Err(_)) => Err("storage unavailable"),
            Err(_) => Err("storage allocation timed out"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[tokio::test]
    async fn short_write_then_disk_full_reopens_only_the_written_prefix() -> anyhow::Result<()> {
        struct FailingFile(fs::File, usize);
        impl Write for FailingFile {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                if self.1 == 0 {
                    return Err(io::ErrorKind::StorageFull.into());
                }
                let n = self.0.write(&bytes[..bytes.len().min(self.1)])?;
                self.1 -= n;
                Ok(n)
            }
            fn flush(&mut self) -> io::Result<()> {
                self.0.flush()
            }
        }
        let home = tempfile::tempdir()?;
        let thread = ThreadId::new();
        let id = Uuid::new_v4();
        let dir = home.path().join("exec-outputs").join(thread.to_string());
        fs::create_dir_all(&dir)?;
        let path = dir.join(id.to_string());
        let metadata = Metadata::new(false);
        let files = prepare(&path, &metadata)?.map(|file| file.map(|file| FailingFile(file, 3)));
        let locator = format!("{thread}/{id}");
        let capture = Capture::start(
            locator.clone(),
            path,
            metadata,
            files,
            Arc::new(Semaphore::new(64)),
        );
        capture.observe(Stdout, b"abcdef");
        capture.observe(Stderr, b"later");
        capture.finish(Stdout, OutputEnd::Eof);
        capture.finish(Stderr, OutputEnd::Eof);
        let receipt = capture.receipt().await;
        drop(capture);
        assert!(receipt.contains("storage write failed"), "{receipt}");
        assert!(
            receipt.contains("stdout 0..3 of 6 observed, stderr 0..0 of 5 observed"),
            "{receipt}"
        );
        let (_, bytes) = OutputStore::default()
            .retrieve(home.path(), thread, &locator, Stdout, 0, 8)
            .await
            .expect("reopen published prefix");
        assert_eq!(bytes, b"abc");
        Ok(())
    }

    #[tokio::test]
    async fn failed_catalog_publication_does_not_advertise_uncommitted_bytes() -> anyhow::Result<()>
    {
        let home = tempfile::tempdir()?;
        let thread = ThreadId::new();
        let store = OutputStore::default();
        let capture = store
            .open(home.path(), thread, true)
            .await
            .expect("open capture");
        capture.observe(Terminal, b"first");
        assert!(
            capture
                .receipt()
                .await
                .contains("terminal 0..5 of 5 observed")
        );

        let obstruction = capture.path.join("metadata.tmp");
        fs::create_dir(&obstruction)?;
        capture.observe(Terminal, b" second");
        capture.finish(Terminal, OutputEnd::Eof);
        let receipt = capture.receipt().await;
        assert!(receipt.contains("terminal 0..5 of 5 observed"), "{receipt}");
        assert!(receipt.contains("checkpoint unavailable"), "{receipt}");
        let (_, prefix) = store
            .retrieve(home.path(), thread, &capture.id, Terminal, 0, 20)
            .await
            .expect("the prior committed prefix is addressable");
        assert_eq!(prefix, b"first");

        fs::remove_dir(obstruction)?;
        let receipt = capture.receipt().await;
        assert!(
            receipt.contains("(complete; read_exec_output)"),
            "{receipt}"
        );
        let (_, data) = store
            .retrieve(home.path(), thread, &capture.id, Terminal, 0, 20)
            .await
            .expect("the next receipt can publish the acknowledged bytes");
        assert_eq!(data, b"first second");
        Ok(())
    }

    #[tokio::test]
    async fn queue_pressure_seals_both_streams_without_losing_the_committed_prefix()
    -> anyhow::Result<()> {
        let home = tempfile::tempdir()?;
        let thread = ThreadId::new();
        let store = OutputStore {
            queue: Arc::new(Semaphore::new(4)),
            ..Default::default()
        };
        let capture = store
            .open(home.path(), thread, false)
            .await
            .expect("open capture");
        capture.observe(Stdout, b"seed");
        let start = capture.receipt().await;
        assert!(start.contains("stdout 0..4 of 4 observed"), "{start}");

        // A chunk larger than the session's available queue budget always
        // stops retention even if the writer drained the earlier prefix.
        capture.observe(Stderr, b"flood");
        capture.observe(Stdout, b"after");
        capture.finish(Stdout, OutputEnd::Eof);
        capture.finish(Stderr, OutputEnd::Eof);
        let finished = capture.receipt().await;
        assert!(
            finished.contains("(failed (writer queue full);"),
            "{finished}"
        );
        assert!(
            finished.contains("stdout 0..4 of 9 observed, stderr 0..0 of 5 observed"),
            "{finished}"
        );

        let id = capture.id.clone();
        drop(capture);
        let reopened = OutputStore::default();
        let (_, bytes) = reopened
            .retrieve(home.path(), thread, &id, Stdout, 0, 8)
            .await
            .expect("recover the committed prefix");
        assert_eq!(bytes, b"seed");
        assert_eq!(
            reopened
                .retrieve(home.path(), ThreadId::new(), &id, Stdout, 0, 8)
                .await,
            Err("artifact belongs to a different session")
        );
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum StreamEnd {
    Eof,
    ReadError,
    Cutoff,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct StreamMetadata {
    observed: u64,
    committed: u64,
    end: Option<StreamEnd>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum RetentionStop {
    ArtifactLimit,
    QueueFull,
    WriteFailed,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct Metadata {
    tty: bool,
    streams: [StreamMetadata; 3],
    retention_stop: Option<RetentionStop>,
}

impl Metadata {
    fn new(tty: bool) -> Self {
        Self {
            tty,
            streams: Default::default(),
            retention_stop: None,
        }
    }

    fn includes(&self, stream: OutputStream) -> bool {
        (stream == Terminal) == self.tty
    }

    fn ended(&self) -> bool {
        STREAMS
            .into_iter()
            .filter(|&stream| self.includes(stream))
            .all(|stream| self.streams[index(stream)].end.is_some())
    }

    fn status(&self, active: bool) -> &'static str {
        match self.retention_stop {
            Some(RetentionStop::ArtifactLimit) => "capped",
            Some(RetentionStop::QueueFull | RetentionStop::WriteFailed) => "failed",
            None if !self.ended() && active => "writing",
            None if !self.ended() => "interrupted",
            None if self
                .streams
                .iter()
                .any(|s| s.end == Some(StreamEnd::ReadError)) =>
            {
                "failed"
            }
            None if self
                .streams
                .iter()
                .any(|s| s.end == Some(StreamEnd::Cutoff)) =>
            {
                "interrupted"
            }
            None => "complete",
        }
    }

    fn receipt(&self, id: &str, active: bool) -> String {
        let status = self.status(active);
        let stop = match self.retention_stop {
            Some(RetentionStop::ArtifactLimit) => " (artifact byte limit)",
            Some(RetentionStop::QueueFull) => " (writer queue full)",
            Some(RetentionStop::WriteFailed) => " (storage write failed)",
            None => "",
        };
        let streams = STREAMS
            .into_iter()
            .filter(|&stream| self.includes(stream))
            .map(|stream| {
                let StreamMetadata {
                    committed,
                    observed,
                    end,
                } = self.streams[index(stream)];
                let name = stream_name(stream);
                let end = match end {
                    Some(StreamEnd::Eof) => "",
                    Some(StreamEnd::ReadError) => "; reader error, remainder unknown",
                    Some(StreamEnd::Cutoff) => "; drain cutoff, remainder unknown",
                    None => "; remainder unknown",
                };
                format!("{name} 0..{committed} of {observed} observed{end}")
            })
            .collect::<Vec<_>>()
            .join(", ");
        format!("Output artifact {id} ({status}{stop}; read_exec_output): {streams}.")
    }
}

#[derive(Debug)]
pub(crate) struct Capture {
    id: String,
    path: PathBuf,
    state: Arc<Mutex<State>>,
    pool: Arc<Semaphore>,
    tx: mpsc::Sender<Command>,
}

#[derive(Debug)]
struct State {
    metadata: Metadata,
    enqueued: u64,
}

enum Command {
    Bytes(OutputStream, Box<[u8]>, OwnedSemaphorePermit),
    Checkpoint(Option<oneshot::Sender<io::Result<Metadata>>>),
}

impl Capture {
    fn start(
        id: String,
        path: PathBuf,
        metadata: Metadata,
        files: [Option<impl Write + Send + 'static>; 3],
        pool: Arc<Semaphore>,
    ) -> Arc<Self> {
        let (tx, rx) = mpsc::channel(256);
        let state = Arc::new(Mutex::new(State {
            metadata,
            enqueued: 0,
        }));
        tokio::task::spawn_blocking({
            let path = path.clone();
            let state = Arc::clone(&state);
            move || write_output(&path, &state, files, rx)
        });
        Arc::new(Self {
            id,
            path,
            state,
            pool,
            tx,
        })
    }

    pub(crate) fn seal(&self) {
        for stream in STREAMS {
            self.finish(stream, OutputEnd::Cutoff);
        }
    }

    pub(crate) async fn receipt(&self) -> String {
        let checkpoint = tokio::time::timeout(OPERATIONS_TIMEOUT, async {
            let (reply, ack) = oneshot::channel();
            self.tx.send(Command::Checkpoint(Some(reply))).await.ok()?;
            ack.await.ok()?.ok()
        })
        .await
        .ok()
        .flatten();
        if let Some(metadata) = checkpoint {
            return metadata.receipt(&self.id, true);
        }
        let metadata = tokio::time::timeout(OPERATIONS_TIMEOUT, load(&self.path)).await;
        match metadata {
            Ok(Ok(metadata)) if self.tx.is_closed() && metadata.ended() => {
                metadata.receipt(&self.id, false)
            }
            Ok(Ok(metadata)) => format!(
                "{} Current checkpoint unavailable.",
                metadata.receipt(&self.id, !self.tx.is_closed())
            ),
            Ok(Err(_)) | Err(_) => format!(
                "Output artifact {}: checkpoint unavailable (read_exec_output).",
                self.id
            ),
        }
    }
}

impl OutputSink for Capture {
    fn observe(&self, stream: OutputStream, bytes: &[u8]) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let entry = &mut state.metadata.streams[index(stream)];
        if entry.end.is_some() {
            return;
        }
        entry.observed = entry.observed.saturating_add(bytes.len() as u64);
        if state.metadata.retention_stop.is_some() {
            return;
        }
        let count = bytes.len().min((ARTIFACT_BYTES - state.enqueued) as usize);
        if count < bytes.len() {
            state.metadata.retention_stop = Some(RetentionStop::ArtifactLimit);
        }
        if count == 0 {
            return;
        }
        let (Ok(slot), Ok(permit)) = (
            self.tx.try_reserve(),
            Arc::clone(&self.pool).try_acquire_many_owned(count as u32),
        ) else {
            state.metadata.retention_stop = Some(RetentionStop::QueueFull);
            return;
        };
        state.enqueued += count as u64;
        slot.send(Command::Bytes(stream, bytes[..count].into(), permit));
    }

    fn finish(&self, stream: OutputStream, end: OutputEnd) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if state.metadata.includes(stream) {
            let end = match end {
                OutputEnd::Eof => StreamEnd::Eof,
                OutputEnd::ReadError => StreamEnd::ReadError,
                OutputEnd::Cutoff => StreamEnd::Cutoff,
            };
            state.metadata.streams[index(stream)].end.get_or_insert(end);
            if state.metadata.ended() {
                let _ = self.tx.try_send(Command::Checkpoint(None));
            }
        }
    }
}

fn index(stream: OutputStream) -> usize {
    match stream {
        Stdout => 0,
        Stderr => 1,
        Terminal => 2,
    }
}

pub(crate) fn stream_name(stream: OutputStream) -> &'static str {
    match stream {
        Stdout => "stdout",
        Stderr => "stderr",
        Terminal => "terminal",
    }
}

fn prepare(path: &Path, metadata: &Metadata) -> io::Result<[Option<fs::File>; 3]> {
    let mut dir = fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt as _;
        dir.mode(0o700);
    }
    dir.create(path)?;
    let result = (|| {
        let mut files = [None, None, None];
        for stream in STREAMS.into_iter().filter(|&s| metadata.includes(s)) {
            files[index(stream)] = Some(fs::File::create_new(path.join(stream_name(stream)))?);
        }
        publish(path, metadata)?;
        Ok(files)
    })();
    if result.is_err() {
        let _ = fs::remove_dir_all(path);
    }
    result
}

fn publish(path: &Path, metadata: &Metadata) -> io::Result<()> {
    fs::write(path.join("metadata.tmp"), serde_json::to_vec(metadata)?)?;
    fs::rename(path.join("metadata.tmp"), path.join("metadata.json"))
}

fn write_output(
    path: &Path,
    state: &Mutex<State>,
    mut files: [Option<impl Write>; 3],
    mut rx: mpsc::Receiver<Command>,
) {
    let mut written = [0; 3];
    let mut write_failed = false;
    while let Some(command) = rx.blocking_recv() {
        let reply = match command {
            Command::Bytes(stream, bytes, _permit) => {
                if !write_failed {
                    let i = index(stream);
                    let Some(file) = files[i].as_mut() else {
                        tracing::error!("captured stream has no output file");
                        state
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner)
                            .metadata
                            .retention_stop = Some(RetentionStop::WriteFailed);
                        write_failed = true;
                        continue;
                    };
                    let mut rest = &bytes[..];
                    while !rest.is_empty() {
                        match file.write(rest) {
                            Ok(n) if n > 0 => {
                                written[i] += n as u64;
                                rest = &rest[n..];
                            }
                            Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
                            Ok(_) | Err(_) => {
                                write_failed = true;
                                state
                                    .lock()
                                    .unwrap_or_else(PoisonError::into_inner)
                                    .metadata
                                    .retention_stop = Some(RetentionStop::WriteFailed);
                                break;
                            }
                        }
                    }
                }
                None
            }
            Command::Checkpoint(reply) => reply,
        };
        let mut snapshot = state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .metadata
            .clone();
        let finalized = snapshot.ended() && rx.is_empty();
        if finalized || reply.is_some() {
            for (entry, committed) in snapshot.streams.iter_mut().zip(written) {
                entry.committed = committed;
                if !finalized
                    && entry.end == Some(StreamEnd::Eof)
                    && entry.committed < entry.observed
                {
                    entry.end = None;
                }
            }
            let result = publish(path, &snapshot).map(|()| snapshot);
            let published = result.is_ok();
            if let Some(reply) = reply {
                let _ = reply.send(result);
            }
            if finalized && published {
                break;
            }
        }
    }
}

async fn load(path: &Path) -> io::Result<Metadata> {
    let file = tokio::fs::File::open(path.join("metadata.json")).await?;
    let mut data = Vec::new();
    file.take(8192).read_to_end(&mut data).await?;
    Ok(serde_json::from_slice(&data)?)
}

impl OutputStore {
    pub(crate) async fn retrieve(
        &self,
        home: &Path,
        thread: ThreadId,
        id: &str,
        stream: OutputStream,
        offset: u64,
        limit: usize,
    ) -> Result<(String, Vec<u8>), &'static str> {
        let (owner, id) = id.split_once('/').ok_or("invalid artifact locator")?;
        if ThreadId::from_string(owner).ok() != Some(thread) {
            return Err("artifact belongs to a different session");
        }
        let id = Uuid::parse_str(id).map_err(|_| "invalid artifact locator")?;
        let path = home
            .join("exec-outputs")
            .join(thread.to_string())
            .join(id.to_string());
        tokio::time::timeout(OPERATIONS_TIMEOUT, async {
            let metadata = load(&path)
                .await
                .map_err(|_| "artifact metadata missing or inaccessible")?;
            if !metadata.includes(stream) {
                return Err("artifact does not contain that stream");
            }
            let stream_meta = &metadata.streams[index(stream)];
            let Some(available) = stream_meta.committed.checked_sub(offset) else {
                return Err("offset is beyond the committed prefix");
            };
            let size = limit.min(available as usize);
            let mut file = tokio::fs::File::open(path.join(stream_name(stream)))
                .await
                .map_err(|_| "artifact bytes missing or inaccessible")?;
            file.seek(io::SeekFrom::Start(offset))
                .await
                .map_err(|_| "artifact seek failed")?;
            let mut bytes = vec![0; size];
            file.read_exact(&mut bytes)
                .await
                .map_err(|_| "artifact no longer contains its committed bytes")?;
            let end = offset + size as u64;
            let name = stream_name(stream);
            let active = self
                .active
                .lock()
                .await
                .get(&id)
                .and_then(Weak::upgrade)
                .is_some_and(|capture| !capture.tx.is_closed());
            let receipt = metadata.receipt(&format!("{thread}/{id}"), active);
            Ok((
                format!("{receipt}\nSource {name} bytes {offset}..{end} (exclusive end)."),
                bytes,
            ))
        })
        .await
        .map_err(|_| "artifact read timed out")?
    }
}
