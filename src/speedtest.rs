use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use chrono::Utc;
use futures_util::StreamExt;

use crate::{
    cluster::auth_headers,
    config::{PeerConfig, SpeedtestConfig},
    models::SpeedtestResult,
};

pub fn requested_bytes(
    requested: Option<u64>,
    default_bytes: u64,
    max_bytes: u64,
) -> anyhow::Result<u64> {
    let bytes = requested.unwrap_or(default_bytes);
    if bytes == 0 {
        bail!("speedtest bytes must be greater than zero");
    }
    if bytes > max_bytes {
        bail!("speedtest bytes {bytes} exceeds configured max {max_bytes}");
    }
    Ok(bytes)
}

pub async fn internet_download(
    client: &reqwest::Client,
    config: &SpeedtestConfig,
    source_node: &str,
    requested: Option<u64>,
) -> anyhow::Result<SpeedtestResult> {
    let bytes = requested_bytes(requested, config.internet_bytes, config.max_bytes)?;
    let mut errors = Vec::new();

    for template in &config.internet_download_urls {
        let template = template.trim();
        if template.is_empty() {
            continue;
        }
        let url = internet_url(template, bytes);
        match download_bytes(client, &url, None, bytes).await {
            Ok(measurement) => {
                return Ok(result(
                    "internet_download",
                    source_node,
                    "internet",
                    measurement.bytes,
                    measurement.elapsed,
                ));
            }
            Err(err) => errors.push(format!("{}: {err}", display_url(&url))),
        }
    }

    bail!(
        "{}",
        errors
            .first()
            .cloned()
            .unwrap_or_else(|| "no internet speedtest URL configured".to_string())
    )
}

pub async fn peer_download(
    client: &reqwest::Client,
    secret: &str,
    source_node: &str,
    target: &PeerConfig,
    requested: Option<u64>,
    default_bytes: u64,
    max_bytes: u64,
) -> anyhow::Result<SpeedtestResult> {
    let bytes = requested_bytes(requested, default_bytes, max_bytes)?;
    let headers = auth_headers(secret)?;
    let mut errors = Vec::new();

    for base_url in target.urls() {
        let url = format!(
            "{}/speedtest/bytes?bytes={bytes}",
            base_url.trim_end_matches('/')
        );
        match download_bytes(client, &url, Some(headers.clone()), bytes).await {
            Ok(measurement) => {
                return Ok(result(
                    "peer_download",
                    source_node,
                    &target.id,
                    measurement.bytes,
                    measurement.elapsed,
                ));
            }
            Err(err) => errors.push(format!("{}: {err}", display_url(&url))),
        }
    }

    bail!(
        "{}",
        errors
            .first()
            .cloned()
            .unwrap_or_else(|| format!("{} has no speedtest URL configured", target.id))
    )
}

struct Measurement {
    bytes: u64,
    elapsed: Duration,
}

async fn download_bytes(
    client: &reqwest::Client,
    url: &str,
    headers: Option<reqwest::header::HeaderMap>,
    expected_bytes: u64,
) -> anyhow::Result<Measurement> {
    let mut request = client.get(url);
    if let Some(headers) = headers {
        request = request.headers(headers);
    }

    let started = Instant::now();
    let response = request
        .send()
        .await
        .with_context(|| format!("request failed for {}", display_url(url)))?;
    let response = response
        .error_for_status()
        .with_context(|| format!("non-success response from {}", display_url(url)))?;
    if let Some(content_length) = response.content_length()
        && content_length != expected_bytes
    {
        bail!(
            "expected {expected_bytes} bytes from {}, got content-length {content_length}",
            display_url(url)
        );
    }

    let mut stream = response.bytes_stream();

    let mut bytes = 0u64;
    while let Some(chunk) = stream.next().await {
        bytes = bytes.saturating_add(chunk.context("failed to read response body")?.len() as u64);
        if bytes > expected_bytes {
            bail!(
                "response from {} exceeded requested {expected_bytes} bytes",
                display_url(url)
            );
        }
    }

    if bytes != expected_bytes {
        bail!(
            "response from {} ended after {bytes} bytes, expected {expected_bytes}",
            display_url(url)
        );
    }

    Ok(Measurement {
        bytes,
        elapsed: started.elapsed(),
    })
}

fn result(
    mode: &str,
    source_node: &str,
    target: &str,
    bytes: u64,
    elapsed: Duration,
) -> SpeedtestResult {
    SpeedtestResult {
        mode: mode.to_string(),
        source_node: source_node.to_string(),
        target: target.to_string(),
        bytes,
        elapsed_millis: elapsed.as_millis().min(u128::from(u64::MAX)) as u64,
        mbps: throughput_mbps(bytes, elapsed),
        completed_at: Utc::now(),
    }
}

fn internet_url(template: &str, bytes: u64) -> String {
    if template.contains("{bytes}") {
        template.replace("{bytes}", &bytes.to_string())
    } else {
        template.to_string()
    }
}

fn display_url(url: &str) -> String {
    reqwest::Url::parse(url)
        .ok()
        .and_then(|url| {
            let mut output = url.host_str()?.to_string();
            if let Some(port) = url.port() {
                output.push(':');
                output.push_str(&port.to_string());
            }
            Some(output)
        })
        .unwrap_or_else(|| "configured endpoint".to_string())
}

fn throughput_mbps(bytes: u64, elapsed: Duration) -> f64 {
    let seconds = elapsed.as_secs_f64();
    if seconds <= 0.0 {
        return 0.0;
    }
    bytes as f64 * 8.0 / seconds / 1_000_000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requested_bytes_rejects_too_large_values() {
        assert!(requested_bytes(Some(11), 10, 10).is_err());
    }

    #[test]
    fn internet_url_replaces_byte_placeholder() {
        assert_eq!(
            internet_url("https://example.test/down?bytes={bytes}", 123),
            "https://example.test/down?bytes=123"
        );
    }

    #[test]
    fn throughput_mbps_uses_decimal_megabits() {
        let mbps = throughput_mbps(125_000_000, Duration::from_secs(1));
        assert!((mbps - 1000.0).abs() < f64::EPSILON);
    }
}
