use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthResponse {
    pub cluster_name: String,
    pub node_id: String,
    pub display_name: String,
    pub version: String,
    pub started_at: DateTime<Utc>,
    pub uptime_seconds: u64,
    pub eligible_leader: bool,
    pub priority: i64,
    pub leader_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetricsSnapshot {
    pub node_id: String,
    pub collected_at: DateTime<Utc>,
    pub uptime_seconds: u64,
    pub cpu: CpuMetrics,
    pub memory: MemoryMetrics,
    pub disks: Vec<DiskMetrics>,
    pub networks: Vec<NetworkMetrics>,
    pub temperatures: Vec<TemperatureMetric>,
    pub gpus: Vec<GpuMetrics>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CpuMetrics {
    pub usage_percent: f32,
    pub logical_cores: usize,
    pub brand: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryMetrics {
    pub total_bytes: u64,
    pub used_bytes: u64,
    pub usage_percent: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiskMetrics {
    pub name: String,
    pub mount_point: String,
    pub total_bytes: u64,
    pub available_bytes: u64,
    pub usage_percent: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkMetrics {
    pub name: String,
    pub received_total_bytes: u64,
    pub transmitted_total_bytes: u64,
    pub received_bytes_per_second: Option<f64>,
    pub transmitted_bytes_per_second: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TemperatureMetric {
    pub label: String,
    pub temperature_celsius: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GpuMetrics {
    pub index: usize,
    pub name: String,
    pub vendor: Option<String>,
    pub usage_percent: Option<f32>,
    pub memory_used_bytes: Option<u64>,
    pub memory_total_bytes: Option<u64>,
    pub temperature_celsius: Option<f32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerStatus {
    pub id: String,
    pub display_name: String,
    pub urls: Vec<String>,
    pub online: bool,
    pub eligible_leader: bool,
    pub priority: i64,
    pub ssh_fallback: bool,
    pub allow_screenshot: bool,
    pub last_seen: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
    pub health: Option<HealthResponse>,
    pub metrics: Option<MetricsSnapshot>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClusterStatus {
    pub self_node: HealthResponse,
    pub leader_id: Option<String>,
    pub peers: Vec<PeerStatus>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShutdownRequest {
    pub delay_seconds: Option<u64>,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpeedtestRequest {
    pub bytes: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerSpeedtestRequest {
    pub peer_id: String,
    pub bytes: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateRequest {
    pub cluster_name: String,
    pub target_node_id: String,
    pub leader_id: String,
    pub operation_id: String,
    pub issued_at: DateTime<Utc>,
    pub signature: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateResult {
    pub node_id: String,
    pub ok: bool,
    pub message: String,
    pub completed_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpeedtestResult {
    pub mode: String,
    pub source_node: String,
    pub target: String,
    pub bytes: u64,
    pub elapsed_millis: u64,
    pub mbps: f64,
    pub completed_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionResponse {
    pub ok: bool,
    pub message: String,
}

#[derive(Debug, Clone)]
pub enum AlertEvent {
    PeerOnline { peer_id: String },
    PeerOffline { peer_id: String, error: String },
    LeaderChanged { leader_id: Option<String> },
    Threshold { peer_id: String, message: String },
}

impl AlertEvent {
    pub fn render(&self) -> String {
        match self {
            Self::PeerOnline { peer_id } => format!("Host online\n{peer_id} is back online."),
            Self::PeerOffline { peer_id, error } => {
                format!("Host offline\nHost: {peer_id}\n{}", compact_error(error))
            }
            Self::LeaderChanged { leader_id } => {
                format!(
                    "Controller changed\nActive: {}",
                    leader_id.as_deref().unwrap_or("none")
                )
            }
            Self::Threshold { peer_id, message } => {
                format!("Host alert\nHost: {peer_id}\n{message}")
            }
        }
    }
}

fn compact_error(error: &str) -> String {
    let first_line = error.lines().next().unwrap_or(error);
    if first_line.len() > 120 {
        let mut end = 120;
        while !first_line.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}...", &first_line[..end])
    } else {
        first_line.to_string()
    }
}
