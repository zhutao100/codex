#![deny(clippy::print_stdout, clippy::print_stderr)]

use std::path::Path;
use std::path::PathBuf;

pub mod protocol;

#[cfg(unix)]
pub mod daemon;
#[cfg(unix)]
pub mod producer;

#[cfg(not(unix))]
pub mod daemon {
    use anyhow::Result;
    use std::path::Path;
    use std::path::PathBuf;

    pub async fn run_daemon(_codex_home: &Path, _socket_path: Option<PathBuf>) -> Result<()> {
        Ok(())
    }
}

#[cfg(not(unix))]
pub mod producer {
    use crate::protocol::HubNotification;
    use codex_app_server_protocol::ServerNotification;
    use std::path::Path;
    use std::path::PathBuf;

    #[derive(Debug, Clone)]
    pub struct RuntimeMetadata {
        pub runtime_id: String,
        pub pid: Option<u32>,
        pub session_source: Option<String>,
        pub cwd: Option<String>,
        pub display_name: Option<String>,
    }

    #[derive(Clone)]
    pub struct CodexdProducerClient;

    impl CodexdProducerClient {
        pub fn spawn(_codex_home: &Path, _metadata: RuntimeMetadata) -> Self {
            Self
        }

        pub fn spawn_with_socket_path(_socket_path: PathBuf, _metadata: RuntimeMetadata) -> Self {
            Self
        }

        pub async fn update_metadata(&self, _metadata: RuntimeMetadata) {}

        pub async fn publish_hub_notification(&self, _notification: HubNotification) {}

        pub async fn publish_server_notification(&self, _notification: &ServerNotification) {}

        pub async fn shutdown(&self) {}
    }
}

pub fn default_socket_path(codex_home: &Path) -> PathBuf {
    codex_home
        .join("runtime")
        .join("codexd")
        .join("codexd.sock")
}
