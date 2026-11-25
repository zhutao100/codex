//! Live process work, including accepted operations without a visible turn yet.
use std::collections::BTreeSet;
use std::sync::LazyLock;
use tokio::sync::watch;

static WORK: LazyLock<watch::Sender<BTreeSet<String>>> =
    LazyLock::new(|| watch::channel(BTreeSet::new()).0);

pub fn subscribe() -> watch::Receiver<BTreeSet<String>> {
    WORK.subscribe()
}

pub(crate) struct WorkGuard(String);

impl WorkGuard {
    pub(crate) fn new(thread: codex_protocol::ThreadId, id: &str, stage: &str) -> Self {
        let key = format!("{thread}:{id}:{stage}:{}", uuid::Uuid::new_v4());
        WORK.send_modify(|items| {
            items.insert(key.clone());
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
