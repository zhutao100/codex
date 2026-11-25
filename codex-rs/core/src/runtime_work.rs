//! Live process work, including accepted operations without a visible turn yet.
use std::collections::BTreeMap;
use std::sync::LazyLock;
use tokio::sync::watch;

/// One entry per guard. Only executing tasks own a visible turn; queued successors do not.
type WorkOwners = BTreeMap<String, Option<String>>;
static WORK: LazyLock<watch::Sender<WorkOwners>> =
    LazyLock::new(|| watch::channel(WorkOwners::new()).0);

pub fn subscribe() -> watch::Receiver<WorkOwners> {
    WORK.subscribe()
}

pub(crate) struct WorkGuard(String);

impl WorkGuard {
    pub(crate) fn new(thread: codex_protocol::ThreadId, id: &str, stage: &str) -> Self {
        let turn_key = format!("{thread}:{id}");
        let key = format!("{turn_key}:{stage}:{}", uuid::Uuid::new_v4());
        WORK.send_modify(|items| {
            items.insert(key.clone(), (stage == "task").then_some(turn_key));
        });
        Self(key)
    }
}

impl Drop for WorkGuard {
    fn drop(&mut self) {
        WORK.send_modify(|items| {
            items.remove(&self.0);
        });
    }
}
