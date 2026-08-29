use std::{
    collections::HashMap,
    process::Stdio,
    time::{Duration, Instant},
};

use anyhow::Context;
use chrono::Utc;
use sysinfo::{Components, Disks, System};
use tokio::{process::Command, time::timeout};

use crate::models::{
    CpuMetrics, DiskMetrics, GpuMetrics, MemoryMetrics, MetricsSnapshot, NetworkMetrics,
    TemperatureMetric,
};

#[derive(Debug)]
pub struct MetricsCollector {
    system: System,
    previous_networks: HashMap<String, NetworkSample>,
}

#[derive(Debug, Clone)]
struct NetworkSample {
    received: u64,
    transmitted: u64,
    at: Instant,
}

#[cfg(windows)]
#[derive(Debug, serde::Deserialize)]
struct CounterSample {
    #[serde(rename = "Path")]
    path: String,
    #[serde(rename = "CookedValue")]
    cooked_value: f64,
}

impl MetricsCollector {
    pub fn new() -> Self {
        Self {
            system: System::new_all(),
            previous_networks: HashMap::new(),
        }
    }

    pub async fn collect(&mut self, node_id: &str) -> anyhow::Result<MetricsSnapshot> {
        self.system.refresh_all();

        let cpu = self.collect_cpu();
        let memory = self.collect_memory();
        let disks = collect_disks();
        let networks = self.collect_networks().await;
        let temperatures = collect_temperatures();
        let gpus = collect_gpus().await;

        Ok(MetricsSnapshot {
            node_id: node_id.to_string(),
            collected_at: Utc::now(),
            uptime_seconds: System::uptime(),
            cpu,
            memory,
            disks,
            networks,
            temperatures,
            gpus,
        })
    }

    fn collect_cpu(&self) -> CpuMetrics {
        let cpus = self.system.cpus();
        let usage_percent = self.system.global_cpu_usage();
        let brand = cpus
            .first()
            .map(|cpu| cpu.brand().trim().to_string())
            .filter(|brand| !brand.is_empty());

        CpuMetrics {
            usage_percent,
            logical_cores: cpus.len(),
            brand,
        }
    }

    fn collect_memory(&self) -> MemoryMetrics {
        let total_bytes = self.system.total_memory();
        let used_bytes = self.system.used_memory();
        let usage_percent = percent(used_bytes, total_bytes);

        MemoryMetrics {
            total_bytes,
            used_bytes,
            usage_percent,
        }
    }

    async fn collect_networks(&mut self) -> Vec<NetworkMetrics> {
        let samples = platform_network_samples().await.unwrap_or_default();
        let now = Instant::now();
        let mut output = Vec::with_capacity(samples.len());

        for (name, received, transmitted) in samples {
            let previous = self.previous_networks.get(&name);
            let (rx_per_second, tx_per_second) = previous
                .and_then(|sample| {
                    let elapsed = now.duration_since(sample.at).as_secs_f64();
                    if elapsed <= 0.0 {
                        return None;
                    }
                    Some((
                        received.saturating_sub(sample.received) as f64 / elapsed,
                        transmitted.saturating_sub(sample.transmitted) as f64 / elapsed,
                    ))
                })
                .map(|(rx, tx)| (Some(rx), Some(tx)))
                .unwrap_or((None, None));

            self.previous_networks.insert(
                name.clone(),
                NetworkSample {
                    received,
                    transmitted,
                    at: now,
                },
            );

            output.push(NetworkMetrics {
                name,
                received_total_bytes: received,
                transmitted_total_bytes: transmitted,
                received_bytes_per_second: rx_per_second,
                transmitted_bytes_per_second: tx_per_second,
            });
        }

        output
    }
}

fn collect_disks() -> Vec<DiskMetrics> {
    let disks = Disks::new_with_refreshed_list();
    disks
        .iter()
        .map(|disk| {
            let total_bytes = disk.total_space();
            let available_bytes = disk.available_space();
            let used_bytes = total_bytes.saturating_sub(available_bytes);
            DiskMetrics {
                name: disk.name().to_string_lossy().to_string(),
                mount_point: disk.mount_point().to_string_lossy().to_string(),
                total_bytes,
                available_bytes,
                usage_percent: percent(used_bytes, total_bytes),
            }
        })
        .collect()
}

fn collect_temperatures() -> Vec<TemperatureMetric> {
    let components = Components::new_with_refreshed_list();
    components
        .iter()
        .filter_map(|component| {
            let temperature = component.temperature()?;
            Some(TemperatureMetric {
                label: component.label().to_string(),
                temperature_celsius: temperature,
            })
        })
        .collect()
}

async fn collect_gpus() -> Vec<GpuMetrics> {
    if let Ok(gpus) = collect_nvidia_gpus().await
        && !gpus.is_empty()
    {
        return gpus;
    }

    #[cfg(windows)]
    if let Ok(gpus) = collect_windows_video_controllers().await {
        return gpus;
    }

    Vec::new()
}

async fn collect_nvidia_gpus() -> anyhow::Result<Vec<GpuMetrics>> {
    let output = timeout(
        Duration::from_secs(3),
        Command::new("nvidia-smi")
            .args([
                "--query-gpu=index,name,temperature.gpu,utilization.gpu,memory.used,memory.total",
                "--format=csv,noheader,nounits",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output(),
    )
    .await
    .context("nvidia-smi timed out")??;

    if !output.status.success() {
        return Ok(Vec::new());
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut gpus = Vec::new();
    for line in stdout.lines().filter(|line| !line.trim().is_empty()) {
        let fields: Vec<_> = line.split(',').map(|field| field.trim()).collect();
        if fields.len() < 6 {
            continue;
        }
        let index = fields[0].parse::<usize>().unwrap_or(gpus.len());
        gpus.push(GpuMetrics {
            index,
            name: fields[1].to_string(),
            vendor: Some("nvidia".to_string()),
            temperature_celsius: parse_optional_f32(fields[2]),
            usage_percent: parse_optional_f32(fields[3]),
            memory_used_bytes: parse_mib(fields[4]),
            memory_total_bytes: parse_mib(fields[5]),
        });
    }

    Ok(gpus)
}

#[cfg(windows)]
async fn collect_windows_video_controllers() -> anyhow::Result<Vec<GpuMetrics>> {
    use std::collections::HashMap;

    use serde::Deserialize;

    #[derive(Debug, Deserialize)]
    struct Controller {
        #[serde(rename = "Name")]
        name: Option<String>,
        #[serde(rename = "AdapterCompatibility")]
        adapter_compatibility: Option<String>,
        #[serde(rename = "PNPDeviceID")]
        pnp_device_id: Option<String>,
    }

    let script = "Get-CimInstance Win32_VideoController | Select-Object Name,AdapterCompatibility,PNPDeviceID | ConvertTo-Json -Compress";
    let output = timeout(
        Duration::from_secs(4),
        Command::new("powershell.exe")
            .args(["-NoProfile", "-Command", script])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output(),
    )
    .await
    .context("PowerShell GPU query timed out")??;

    if !output.status.success() {
        return Ok(Vec::new());
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }

    let controllers = if trimmed.starts_with('[') {
        serde_json::from_str::<Vec<Controller>>(trimmed)?
    } else {
        vec![serde_json::from_str::<Controller>(trimmed)?]
    };

    let mut gpu_usage_by_key = HashMap::<String, f32>::new();
    if let Ok(samples) = windows_counter_samples("'\\GPU Engine(*)\\Utilization Percentage'").await
    {
        for sample in samples {
            let Some(key) = gpu_counter_key(&sample.path) else {
                continue;
            };
            let value = sample.cooked_value.max(0.0) as f32;
            let current = gpu_usage_by_key.entry(key).or_insert(0.0);
            *current = (*current + value).min(100.0);
        }
    }

    let mut gpu_memory_by_key = HashMap::<String, u64>::new();
    if let Ok(samples) = windows_counter_samples("'\\GPU Adapter Memory(*)\\Dedicated Usage'").await
    {
        for sample in samples {
            let Some(key) = gpu_counter_key(&sample.path) else {
                continue;
            };
            gpu_memory_by_key.insert(key, sample.cooked_value.max(0.0) as u64);
        }
    }

    let mut perf_keys: Vec<_> = gpu_usage_by_key
        .keys()
        .chain(gpu_memory_by_key.keys())
        .cloned()
        .collect();
    perf_keys.sort();
    perf_keys.dedup();

    let physical_controllers: Vec<_> = controllers
        .into_iter()
        .filter(|controller| {
            controller
                .pnp_device_id
                .as_deref()
                .map(|id| id.starts_with("PCI\\"))
                .unwrap_or(false)
        })
        .filter_map(|controller| {
            let name = controller.name?.trim().to_string();
            if name.is_empty() || looks_virtual_gpu(&name) {
                return None;
            }
            Some((name, controller.adapter_compatibility))
        })
        .collect();

    Ok(physical_controllers
        .into_iter()
        .enumerate()
        .map(|(index, (name, compatibility))| {
            let key = perf_keys.get(index);
            GpuMetrics {
                index,
                name,
                vendor: compatibility.and_then(|value| {
                    let value = value.trim().to_string();
                    if value.is_empty() { None } else { Some(value) }
                }),
                usage_percent: key.and_then(|key| gpu_usage_by_key.get(key)).copied(),
                memory_used_bytes: key.and_then(|key| gpu_memory_by_key.get(key)).copied(),
                memory_total_bytes: None,
                temperature_celsius: None,
            }
        })
        .collect())
}

#[cfg(windows)]
async fn windows_counter_samples(counter: &str) -> anyhow::Result<Vec<CounterSample>> {
    let script = format!(
        "Get-Counter {counter} -ErrorAction SilentlyContinue | Select-Object -ExpandProperty CounterSamples | Select-Object Path,CookedValue | ConvertTo-Json -Compress"
    );
    let output = timeout(
        Duration::from_secs(4),
        Command::new("powershell.exe")
            .args(["-NoProfile", "-Command", &script])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output(),
    )
    .await
    .context("PowerShell GPU counter query timed out")??;

    if !output.status.success() {
        return Ok(Vec::new());
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }

    if trimmed.starts_with('[') {
        Ok(serde_json::from_str::<Vec<CounterSample>>(trimmed)?)
    } else {
        Ok(vec![serde_json::from_str::<CounterSample>(trimmed)?])
    }
}

#[cfg(windows)]
fn gpu_counter_key(path: &str) -> Option<String> {
    let lower = path.to_ascii_lowercase();
    let start = lower.find("luid_")? + "luid_".len();
    let rest = &lower[start..];
    let end = rest
        .find("_eng_")
        .or_else(|| rest.find(')'))
        .unwrap_or(rest.len());
    Some(rest[..end].to_string())
}

#[cfg(windows)]
fn looks_virtual_gpu(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    [
        "virtual",
        "remote display",
        "basic display",
        "mirror",
        "indirect display",
        "meta",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

#[cfg(unix)]
async fn platform_network_samples() -> anyhow::Result<Vec<(String, u64, u64)>> {
    let text = tokio::fs::read_to_string("/proc/net/dev").await?;
    let mut samples = Vec::new();
    for line in text.lines().skip(2) {
        let Some((name, rest)) = line.split_once(':') else {
            continue;
        };
        let values: Vec<_> = rest.split_whitespace().collect();
        if values.len() < 16 {
            continue;
        }
        let received = values[0].parse::<u64>().unwrap_or(0);
        let transmitted = values[8].parse::<u64>().unwrap_or(0);
        samples.push((name.trim().to_string(), received, transmitted));
    }
    Ok(samples)
}

#[cfg(windows)]
async fn platform_network_samples() -> anyhow::Result<Vec<(String, u64, u64)>> {
    use serde::Deserialize;

    #[derive(Debug, Deserialize)]
    struct AdapterStats {
        #[serde(rename = "Name")]
        name: String,
        #[serde(rename = "ReceivedBytes")]
        received_bytes: u64,
        #[serde(rename = "SentBytes")]
        sent_bytes: u64,
    }

    let script = "Get-NetAdapterStatistics | Select-Object Name,ReceivedBytes,SentBytes | ConvertTo-Json -Compress";
    let output = timeout(
        Duration::from_secs(4),
        Command::new("powershell.exe")
            .args(["-NoProfile", "-Command", script])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output(),
    )
    .await
    .context("PowerShell network query timed out")??;

    if !output.status.success() {
        return Ok(Vec::new());
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }

    let adapters = if trimmed.starts_with('[') {
        serde_json::from_str::<Vec<AdapterStats>>(trimmed)?
    } else {
        vec![serde_json::from_str::<AdapterStats>(trimmed)?]
    };

    Ok(adapters
        .into_iter()
        .map(|adapter| (adapter.name, adapter.received_bytes, adapter.sent_bytes))
        .collect())
}

#[cfg(not(any(unix, windows)))]
async fn platform_network_samples() -> anyhow::Result<Vec<(String, u64, u64)>> {
    Ok(Vec::new())
}

fn parse_optional_f32(value: &str) -> Option<f32> {
    let trimmed = value.trim();
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("[not supported]") {
        return None;
    }
    trimmed.parse::<f32>().ok()
}

fn parse_mib(value: &str) -> Option<u64> {
    value
        .trim()
        .parse::<u64>()
        .ok()
        .map(|mib| mib.saturating_mul(1024 * 1024))
}

fn percent(used: u64, total: u64) -> f32 {
    if total == 0 {
        0.0
    } else {
        (used as f64 * 100.0 / total as f64) as f32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    fn gpu_counter_key_extracts_luid() {
        assert_eq!(
            gpu_counter_key(
                r"\\host\gpu engine(luid_0x00000000_0x0000abcd_eng_3d_0)\utilization percentage"
            ),
            Some("0x00000000_0x0000abcd".to_string())
        );
    }

    #[cfg(windows)]
    #[test]
    fn virtual_gpu_filter_rejects_remote_adapters() {
        assert!(looks_virtual_gpu("Microsoft Remote Display Adapter"));
        assert!(!looks_virtual_gpu("NVIDIA GeForce RTX"));
    }

    #[test]
    fn percent_handles_zero_total() {
        assert_eq!(percent(50, 0), 0.0);
    }
}
