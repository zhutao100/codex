//! VM verification peer: publish one visible reviewer, then lose detail on the real producer queue.
use codex_codexd::SchedulerWork;
use codex_codexd::producer::CodexdProducerClient;
use codex_codexd::producer::RuntimeMetadata;
use codex_codexd::protocol::HubNotification;
use serde_json::json;
use std::io;
use std::path::PathBuf;
use tokio::sync::watch;

async fn command(expected: &str) -> io::Result<()> {
    let line = tokio::task::spawn_blocking(|| {
        let mut line = String::new();
        io::stdin().read_line(&mut line)?;
        Ok::<_, io::Error>(line)
    })
    .await??;
    if line.trim() != expected {
        return Err(io::Error::other(format!("expected {expected}")));
    }
    Ok(())
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> io::Result<()> {
    let path = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .ok_or_else(|| io::Error::other("expected fixture daemon socket path"))?;
    let (work, owners) = watch::channel(SchedulerWork::from([(
        "review-owner".into(),
        Some("review:0".into()),
    )]));
    let client = CodexdProducerClient::spawn_with_socket_path(
        path,
        RuntimeMetadata {
            runtime_id: "overflow-fixture".into(),
            pid: Some(std::process::id()),
            session_source: None,
            cwd: None,
            display_name: None,
        },
        owners,
    );
    client.try_publish_hub_notification(HubNotification { method:"turn/started".into(),
        params:Some(json!({"threadId":"review","turn":{"id":"0","key":"review:0","taskKind":"post_turn_completion_review"}}))});
    command("overflow").await?;
    // No await between the terminal and the burst: on this executor the socket writer cannot drain.
    work.send_replace(SchedulerWork::new());
    client.try_publish_hub_notification(HubNotification {
        method: "turn/completed".into(),
        params: Some(
            json!({"threadId":"review","turn":{"id":"0","key":"review:0","status":"failed"}}),
        ),
    });
    for _ in 0..4096 {
        client.try_publish_hub_notification(HubNotification {
            method: "fixture/detail".into(),
            params: None,
        });
    }
    command("quit").await?;
    client.shutdown().await;
    Ok(())
}
