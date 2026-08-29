use std::{process::Stdio, time::Duration};

use anyhow::{anyhow, bail};
use tokio::{process::Command, time::timeout};
use tracing::warn;

use crate::{config::UpdateConfig, models::UpdateResult};

pub async fn schedule(config: &UpdateConfig, node_id: &str) -> anyhow::Result<UpdateResult> {
    if !config.enabled {
        bail!("updates are disabled on this node");
    }
    let Some(program) = config.command.first() else {
        bail!("update.command is not configured");
    };
    if program.trim().is_empty() {
        bail!("update.command program is empty");
    }

    let mut command = Command::new(program);
    command
        .args(config.command.iter().skip(1))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command.kill_on_drop(true);

    let output = timeout(
        Duration::from_secs(config.timeout_seconds),
        command.output(),
    )
    .await
    .map_err(|_| anyhow!("update launcher timed out"))?
    .map_err(|err| {
        warn!(error = %err, "failed to start update launcher");
        anyhow!("update launcher could not start")
    })?;

    if !output.status.success() {
        warn!(
            status = ?output.status,
            "update launcher failed; inspect the protected local update log"
        );
        bail!("update launcher failed");
    }

    let message = "scheduled".to_string();

    Ok(UpdateResult {
        node_id: node_id.to_string(),
        ok: true,
        message,
        completed_at: chrono::Utc::now(),
    })
}
