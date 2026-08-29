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

    let stdout = compact_output(&output.stdout);
    let stderr = compact_output(&output.stderr);
    if !output.status.success() {
        warn!(
            status = ?output.status,
            stdout = %redact_output(&stdout),
            stderr = %redact_output(&stderr),
            "update launcher failed"
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

fn compact_output(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let trimmed = text.trim();
    if trimmed.len() <= 500 {
        return trimmed.to_string();
    }
    let mut end = 500;
    while !trimmed.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &trimmed[..end])
}

fn redact_output(text: &str) -> String {
    text.split_whitespace()
        .map(|part| {
            if part.contains("://") && part.contains('@') {
                "<redacted-url>"
            } else if part.to_ascii_uppercase().contains("TOKEN")
                || part.to_ascii_uppercase().contains("SECRET")
                || part.to_ascii_uppercase().contains("PASSWORD")
            {
                "<redacted>"
            } else {
                part
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}
