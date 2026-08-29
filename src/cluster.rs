use std::{
    collections::{HashMap, HashSet, VecDeque},
    io::ErrorKind,
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, anyhow, bail};
use chrono::{DateTime, Utc};
use futures_util::StreamExt;
use hmac::{Hmac, Mac};
use reqwest::header::{HeaderMap, HeaderValue};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use subtle::ConstantTimeEq;
use tokio::sync::{Mutex, RwLock, Semaphore};
use tracing::{debug, warn};
use uuid::Uuid;

use crate::{
    config::{Config, PeerConfig},
    metrics::MetricsCollector,
    models::{
        AlertEvent, ClusterStatus, HealthResponse, MetricsSnapshot, PeerSpeedtestRequest,
        PeerStatus, SpeedtestRequest, SpeedtestResult, UpdateRequest, UpdateResult,
    },
    screenshot::{self, Screenshot},
    speedtest, updater,
};

#[derive(Debug)]
pub struct AppState {
    pub config: Arc<Config>,
    pub started_at: DateTime<Utc>,
    pub metrics: Mutex<MetricsCollector>,
    pub cluster: RwLock<ClusterRuntime>,
    pub alerts: Mutex<VecDeque<AlertEvent>>,
    pub threshold_alerts: Mutex<HashMap<String, DateTime<Utc>>>,
    pub http: reqwest::Client,
    pub speedtest_http: reqwest::Client,
    pub speedtest_rpc_http: reqwest::Client,
    pub speedtest_limiter: Arc<Semaphore>,
    pub screenshot_limiter: Arc<Semaphore>,
    pub update_runtime: Mutex<UpdateRuntime>,
    pub shared_secret: String,
}

const MAX_SCREENSHOT_BYTES: u64 = 25 * 1024 * 1024;
const UPDATE_SIGNATURE_TTL_SECONDS: i64 = 300;
const UPDATE_OPERATION_HISTORY_SECONDS: i64 = 900;
const UPDATE_OPERATION_HISTORY_LIMIT: usize = 256;
const UPDATE_SCHEDULE_COOLDOWN_SECONDS: i64 = 60;
const UPDATE_OPERATIONS_FILE: &str = "update-operations.json";

type HmacSha256 = Hmac<Sha256>;

#[derive(Debug, Default)]
pub struct ClusterRuntime {
    pub self_metrics: Option<MetricsSnapshot>,
    pub peers: HashMap<String, PeerRuntime>,
    pub leader_id: Option<String>,
}

#[derive(Debug, Default)]
pub struct UpdateRuntime {
    seen_operations: HashMap<String, DateTime<Utc>>,
    busy_until: Option<DateTime<Utc>>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct StoredUpdateState {
    #[serde(default)]
    operations: Vec<StoredUpdateOperation>,
    #[serde(default)]
    busy_until: Option<DateTime<Utc>>,
}

#[derive(Debug, Serialize, Deserialize)]
struct StoredUpdateOperation {
    operation_id: String,
    seen_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct PeerRuntime {
    pub config: PeerConfig,
    pub online: bool,
    pub last_seen: Option<DateTime<Utc>>,
    pub first_seen: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
    pub missed_heartbeats: u64,
    pub health: Option<HealthResponse>,
    pub metrics: Option<MetricsSnapshot>,
}

impl AppState {
    pub fn new(config: Config) -> anyhow::Result<Self> {
        let shared_secret = config.shared_secret()?;
        let timeout = Duration::from_millis(config.cluster.request_timeout_millis);
        let http = reqwest::Client::builder()
            .timeout(timeout)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .context("failed to create HTTP client")?;
        let speedtest_http = reqwest::Client::builder()
            .timeout(Duration::from_secs(config.speedtest.timeout_seconds))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .context("failed to create speedtest HTTP client")?;
        let speedtest_rpc_http = reqwest::Client::builder()
            .timeout(Duration::from_secs(config.speedtest.rpc_timeout_seconds))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .context("failed to create speedtest RPC HTTP client")?;

        let peers = config
            .peers
            .iter()
            .cloned()
            .map(|peer| {
                (
                    peer.id.clone(),
                    PeerRuntime {
                        config: peer,
                        online: false,
                        last_seen: None,
                        first_seen: None,
                        last_error: None,
                        missed_heartbeats: 0,
                        health: None,
                        metrics: None,
                    },
                )
            })
            .collect();

        Ok(Self {
            config: Arc::new(config),
            started_at: Utc::now(),
            metrics: Mutex::new(MetricsCollector::new()),
            cluster: RwLock::new(ClusterRuntime {
                peers,
                ..Default::default()
            }),
            alerts: Mutex::new(VecDeque::new()),
            threshold_alerts: Mutex::new(HashMap::new()),
            http,
            speedtest_http,
            speedtest_rpc_http,
            speedtest_limiter: Arc::new(Semaphore::new(1)),
            screenshot_limiter: Arc::new(Semaphore::new(1)),
            update_runtime: Mutex::new(UpdateRuntime::default()),
            shared_secret,
        })
    }

    pub async fn refresh_self(&self) -> anyhow::Result<()> {
        let metrics = self.collect_fresh_local_metrics().await?;
        self.enqueue_metric_alerts(&self.config.node.id, &metrics)
            .await;
        Ok(())
    }

    pub async fn poll_peers(&self) -> anyhow::Result<()> {
        let peers: Vec<PeerConfig> = self
            .config
            .peers
            .iter()
            .filter(|peer| peer.id != self.config.node.id)
            .cloned()
            .collect();

        let mut handles = Vec::with_capacity(peers.len());
        for peer in peers {
            let client = self.http.clone();
            let secret = self.shared_secret.clone();
            let cluster_name = self.config.cluster.name.clone();
            handles.push(tokio::spawn(async move {
                let result = poll_peer(&client, &secret, &cluster_name, &peer).await;
                (peer, result)
            }));
        }

        let mut results = Vec::with_capacity(handles.len());
        for handle in handles {
            results.push(handle.await.context("peer polling task failed")?);
        }

        let mut alerts = Vec::new();
        let mut threshold_checks = Vec::new();
        let now = Utc::now();
        let mut cluster = self.cluster.write().await;
        for (peer, result) in results {
            let runtime = cluster
                .peers
                .entry(peer.id.clone())
                .or_insert_with(|| PeerRuntime {
                    config: peer.clone(),
                    online: false,
                    last_seen: None,
                    first_seen: None,
                    last_error: None,
                    missed_heartbeats: 0,
                    health: None,
                    metrics: None,
                });
            runtime.config = peer;
            match result {
                Ok((health, metrics)) => {
                    let was_online = runtime.online;
                    let had_seen = runtime.last_seen.is_some();
                    runtime.online = true;
                    runtime.last_seen = Some(now);
                    if runtime.first_seen.is_none() {
                        runtime.first_seen = Some(now);
                    }
                    runtime.last_error = None;
                    runtime.missed_heartbeats = 0;
                    runtime.health = Some(health);
                    runtime.metrics = Some(metrics.clone());
                    threshold_checks.push((runtime.config.id.clone(), metrics));
                    if !was_online && had_seen {
                        alerts.push(AlertEvent::PeerOnline {
                            peer_id: runtime.config.id.clone(),
                        });
                    }
                }
                Err(err) => {
                    let error = err.to_string();
                    runtime.missed_heartbeats = runtime.missed_heartbeats.saturating_add(1);
                    runtime.last_error = Some(error.clone());
                    if runtime.missed_heartbeats
                        >= self.config.alerts.offline_after_missed_heartbeats
                    {
                        let was_online = runtime.online;
                        runtime.online = false;
                        runtime.first_seen = None;
                        if was_online {
                            alerts.push(AlertEvent::PeerOffline {
                                peer_id: runtime.config.id.clone(),
                                error,
                            });
                        }
                    }
                }
            }
        }
        drop(cluster);
        self.enqueue_alerts(alerts).await;
        for (peer_id, metrics) in threshold_checks {
            self.enqueue_metric_alerts(&peer_id, &metrics).await;
        }

        Ok(())
    }

    pub async fn recompute_leader(&self) {
        let mut cluster = self.cluster.write().await;
        let leader_id = self.compute_leader_locked(&cluster);
        if cluster.leader_id != leader_id {
            debug!(?leader_id, "leader changed");
            let alert = AlertEvent::LeaderChanged {
                leader_id: leader_id.clone(),
            };
            cluster.leader_id = leader_id;
            drop(cluster);
            self.enqueue_alert(alert).await;
            return;
        }
        cluster.leader_id = leader_id;
    }

    pub async fn is_leader(&self) -> bool {
        self.cluster.read().await.leader_id.as_deref() == Some(self.config.node.id.as_str())
    }

    pub async fn leader_id(&self) -> Option<String> {
        self.cluster.read().await.leader_id.clone()
    }

    pub async fn has_viable_successor(&self) -> bool {
        let cluster = self.cluster.read().await;
        let now = Utc::now();
        let max_age = chrono::Duration::seconds(self.config.cluster.leader_lease_seconds as i64);

        cluster.peers.values().any(|runtime| {
            runtime.online
                && runtime.config.eligible_leader()
                && runtime
                    .last_seen
                    .map(|last_seen| now - last_seen <= max_age)
                    .unwrap_or(false)
        })
    }

    pub fn self_health_with_leader(&self, leader_id: Option<String>) -> HealthResponse {
        let uptime_seconds = (Utc::now() - self.started_at).num_seconds().max(0) as u64;
        HealthResponse {
            cluster_name: self.config.cluster.name.clone(),
            node_id: self.config.node.id.clone(),
            display_name: self.config.node.display_name.clone(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            started_at: self.started_at,
            uptime_seconds,
            eligible_leader: self.config.node.eligible_leader,
            priority: self.config.node.priority,
            leader_id,
        }
    }

    pub async fn health(&self) -> HealthResponse {
        self.self_health_with_leader(self.leader_id().await)
    }

    pub async fn local_metrics(&self) -> anyhow::Result<MetricsSnapshot> {
        if let Some(metrics) = self.cluster.read().await.self_metrics.clone() {
            return Ok(metrics);
        }
        self.collect_fresh_local_metrics().await
    }

    async fn collect_fresh_local_metrics(&self) -> anyhow::Result<MetricsSnapshot> {
        let mut collector = self.metrics.lock().await;
        let metrics = collector.collect(&self.config.node.id).await?;
        drop(collector);

        let mut cluster = self.cluster.write().await;
        cluster.self_metrics = Some(metrics.clone());
        Ok(metrics)
    }

    pub async fn cluster_status(&self) -> ClusterStatus {
        let cluster = self.cluster.read().await;
        let leader_id = cluster.leader_id.clone();
        let self_node = self.self_health_with_leader(leader_id.clone());
        let peers = cluster
            .peers
            .values()
            .map(|runtime| PeerStatus {
                id: runtime.config.id.clone(),
                display_name: runtime.config.display_name.clone(),
                urls: runtime.config.urls(),
                online: runtime.online,
                eligible_leader: runtime.config.eligible_leader(),
                priority: runtime.config.priority(),
                ssh_fallback: runtime.config.ssh_fallback,
                allow_screenshot: runtime.config.allow_screenshot,
                last_seen: runtime.last_seen,
                last_error: runtime.last_error.clone(),
                health: runtime.health.clone(),
                metrics: runtime.metrics.clone(),
            })
            .collect();
        ClusterStatus {
            self_node,
            leader_id,
            peers,
        }
    }

    pub async fn collect_metrics_for_host(&self, host: &str) -> Option<MetricsSnapshot> {
        if host.eq_ignore_ascii_case(&self.config.node.id)
            || host.eq_ignore_ascii_case(&self.config.node.display_name)
        {
            return self.local_metrics().await.ok();
        }

        let peer = self.peer_config(host).await?;
        match fetch_peer_metrics(&self.http, &self.shared_secret, &peer).await {
            Ok(metrics) => {
                let mut cluster = self.cluster.write().await;
                if let Some(runtime) = cluster.peers.get_mut(&peer.id) {
                    runtime.online = true;
                    runtime.last_seen = Some(Utc::now());
                    if runtime.first_seen.is_none() {
                        runtime.first_seen = runtime.last_seen;
                    }
                    runtime.last_error = None;
                    runtime.missed_heartbeats = 0;
                    runtime.metrics = Some(metrics.clone());
                }
                Some(metrics)
            }
            Err(err) => {
                let mut cluster = self.cluster.write().await;
                if let Some(runtime) = cluster.peers.get_mut(&peer.id) {
                    runtime.last_error = Some(err.to_string());
                    runtime.missed_heartbeats = runtime.missed_heartbeats.saturating_add(1);
                    if runtime.missed_heartbeats
                        >= self.config.alerts.offline_after_missed_heartbeats
                    {
                        runtime.online = false;
                        runtime.first_seen = None;
                    }
                }
                cluster
                    .peers
                    .get(&peer.id)
                    .and_then(|runtime| runtime.metrics.clone())
            }
        }
    }

    pub async fn screenshot_for_host(&self, host: &str) -> anyhow::Result<Screenshot> {
        if host.eq_ignore_ascii_case(&self.config.node.id)
            || host.eq_ignore_ascii_case(&self.config.node.display_name)
        {
            if !self.config.node.allow_screenshot {
                anyhow::bail!("screenshots are not allowed on this node");
            }
            let _permit = self
                .screenshot_limiter
                .try_acquire()
                .map_err(|_| anyhow!("screenshot already running on this node"))?;
            return screenshot::capture().await;
        }

        let peer = self
            .peer_config(host)
            .await
            .ok_or_else(|| anyhow::anyhow!("unknown host {host}"))?;
        if !peer.allow_screenshot {
            anyhow::bail!("screenshots are not allowed for {}", peer.id);
        }
        fetch_peer_screenshot(&self.http, &self.shared_secret, &peer).await
    }

    pub async fn speedtest_internet_for_host(
        &self,
        host: &str,
        bytes: Option<u64>,
    ) -> anyhow::Result<SpeedtestResult> {
        if self.matches_self(host) {
            let _permit = self
                .speedtest_limiter
                .try_acquire()
                .map_err(|_| anyhow!("speedtest already running on this node"))?;
            return speedtest::internet_download(
                &self.speedtest_http,
                &self.config.speedtest,
                &self.config.node.id,
                bytes,
            )
            .await;
        }

        let peer = self
            .peer_config(host)
            .await
            .ok_or_else(|| anyhow::anyhow!("unknown host {host}"))?;
        fetch_peer_internet_speedtest(&self.speedtest_rpc_http, &self.shared_secret, &peer, bytes)
            .await
    }

    pub async fn speedtest_between_hosts(
        &self,
        source: &str,
        target: &str,
        bytes: Option<u64>,
    ) -> anyhow::Result<SpeedtestResult> {
        let source_id = self
            .resolve_host_id(source)
            .await
            .ok_or_else(|| anyhow::anyhow!("unknown source host {source}"))?;
        let target_id = self
            .resolve_host_id(target)
            .await
            .ok_or_else(|| anyhow::anyhow!("unknown target host {target}"))?;
        if source_id == target_id {
            anyhow::bail!("source and target must be different hosts");
        }

        if source_id == self.config.node.id {
            let _permit = self
                .speedtest_limiter
                .try_acquire()
                .map_err(|_| anyhow!("speedtest already running on this node"))?;
            let target = self
                .peer_config(&target_id)
                .await
                .ok_or_else(|| anyhow::anyhow!("unknown target host {target}"))?;
            return speedtest::peer_download(
                &self.speedtest_http,
                &self.shared_secret,
                &self.config.node.id,
                &target,
                bytes,
                self.config.speedtest.peer_bytes,
                self.config.speedtest.max_bytes,
            )
            .await;
        }

        let source = self
            .peer_config(&source_id)
            .await
            .ok_or_else(|| anyhow::anyhow!("unknown source host {source}"))?;
        fetch_peer_to_peer_speedtest(
            &self.speedtest_rpc_http,
            &self.shared_secret,
            &source,
            &target_id,
            bytes,
        )
        .await
    }

    pub async fn update_for_host(
        &self,
        host: &str,
        operation_id: &str,
    ) -> anyhow::Result<UpdateResult> {
        let target_id = self
            .resolve_host_id(host)
            .await
            .ok_or_else(|| anyhow!("unknown host {host}"))?;
        let request = self.signed_update_request(&target_id, operation_id).await?;

        if target_id == self.config.node.id {
            return self.schedule_update_request(&request).await;
        }

        let peer = self
            .peer_config(&target_id)
            .await
            .ok_or_else(|| anyhow!("unknown host {host}"))?;
        fetch_peer_update(&self.http, &peer, request).await
    }

    pub fn new_update_operation_id(&self) -> String {
        Uuid::new_v4().to_string()
    }

    pub async fn resolve_update_targets(&self, hosts: &[String]) -> anyhow::Result<Vec<String>> {
        if hosts.is_empty() || (hosts.len() == 1 && hosts[0].eq_ignore_ascii_case("all")) {
            let mut targets: Vec<_> = self
                .config
                .peers
                .iter()
                .map(|peer| peer.id.clone())
                .collect();
            targets.sort();
            targets.push(self.config.node.id.clone());
            return Ok(targets);
        }
        if hosts.iter().any(|host| host.eq_ignore_ascii_case("all")) {
            bail!("use /update all by itself");
        }

        let mut targets = Vec::new();
        let mut seen = HashSet::new();
        for host in hosts {
            let Some(target_id) = self.resolve_host_id(host).await else {
                bail!("unknown host {host}");
            };
            if seen.insert(target_id.clone()) {
                targets.push(target_id);
            }
        }
        Ok(targets)
    }

    pub async fn schedule_update_request(
        &self,
        request: &UpdateRequest,
    ) -> anyhow::Result<UpdateResult> {
        self.validate_update_request(request).await?;
        self.ensure_restart_allowed().await?;
        self.schedule_local_update(&request.operation_id).await
    }

    pub async fn ensure_restart_allowed(&self) -> anyhow::Result<()> {
        let leader_id = self.leader_id().await;
        if self.config.node.eligible_leader && leader_id.is_none() {
            bail!("leadership state is not initialized");
        }
        if leader_id.as_deref() == Some(self.config.node.id.as_str())
            && !self.has_viable_successor().await
        {
            bail!("refusing to restart active leader without a viable successor");
        }
        Ok(())
    }

    async fn signed_update_request(
        &self,
        target_node_id: &str,
        operation_id: &str,
    ) -> anyhow::Result<UpdateRequest> {
        if !self.is_leader().await {
            bail!("this node is not the active leader");
        }
        validate_operation_id(operation_id)?;
        let issued_at = Utc::now();
        let mut request = UpdateRequest {
            cluster_name: self.config.cluster.name.clone(),
            target_node_id: target_node_id.to_string(),
            leader_id: self.config.node.id.clone(),
            operation_id: operation_id.to_string(),
            issued_at,
            signature: String::new(),
        };
        request.signature = sign_update_request(&self.shared_secret, &request)?;
        Ok(request)
    }

    async fn validate_update_request(&self, request: &UpdateRequest) -> anyhow::Result<()> {
        if request.cluster_name != self.config.cluster.name {
            bail!("update cluster mismatch");
        }
        if request.target_node_id != self.config.node.id {
            bail!("update target mismatch");
        }
        validate_operation_id(&request.operation_id)?;

        let now = Utc::now();
        let age = now - request.issued_at;
        if age.num_seconds().abs() > UPDATE_SIGNATURE_TTL_SECONDS {
            bail!("stale update request");
        }

        let Some(leader_id) = self.leader_id().await else {
            bail!("leadership state is not initialized");
        };
        if leader_id != request.leader_id {
            bail!("update request was not issued by the current leader");
        }

        let expected = sign_update_request(&self.shared_secret, request)?;
        if expected
            .as_bytes()
            .ct_eq(request.signature.as_bytes())
            .unwrap_u8()
            != 1
        {
            bail!("invalid update signature");
        }
        Ok(())
    }

    async fn schedule_local_update(&self, operation_id: &str) -> anyhow::Result<UpdateResult> {
        self.reserve_update_operation(operation_id).await?;

        match updater::schedule(&self.config.update, &self.config.node.id).await {
            Ok(result) => Ok(result),
            Err(err) => {
                let mut runtime = self.update_runtime.lock().await;
                runtime.busy_until = None;
                let stored = runtime.to_stored_state();
                if let Err(save_err) = self.save_update_state(&stored).await {
                    warn!(error = %save_err, "failed to clear update busy state");
                }
                Err(err)
            }
        }
    }

    async fn reserve_update_operation(&self, operation_id: &str) -> anyhow::Result<()> {
        let now = Utc::now();
        {
            let mut runtime = self.update_runtime.lock().await;
            let stored = self.load_update_state(now).await?;
            runtime.merge_stored(stored);
            runtime.prune(now);
            if runtime.seen_operations.contains_key(operation_id) {
                bail!("duplicate update operation");
            }
            if runtime.busy_until.map(|until| until > now).unwrap_or(false) {
                bail!("update already running");
            }
            runtime
                .seen_operations
                .insert(operation_id.to_string(), now);
            let cooldown =
                UPDATE_SCHEDULE_COOLDOWN_SECONDS.max(self.config.update.timeout_seconds as i64);
            runtime.busy_until = Some(now + chrono::Duration::seconds(cooldown));
            let stored = runtime.to_stored_state();
            self.save_update_state(&stored).await?;
        }
        Ok(())
    }

    async fn load_update_state(&self, now: DateTime<Utc>) -> anyhow::Result<StoredUpdateState> {
        let path = self.update_operations_path();
        let text = match tokio::fs::read_to_string(&path).await {
            Ok(text) => text,
            Err(err) if err.kind() == ErrorKind::NotFound => {
                return Ok(StoredUpdateState::default());
            }
            Err(err) => {
                return Err(err).with_context(|| format!("failed to read {}", path.display()));
            }
        };

        let mut stored = serde_json::from_str::<StoredUpdateState>(&text)
            .or_else(|_| {
                serde_json::from_str::<Vec<StoredUpdateOperation>>(&text).map(|operations| {
                    StoredUpdateState {
                        operations,
                        busy_until: None,
                    }
                })
            })
            .with_context(|| format!("invalid update operation state in {}", path.display()))?;
        let max_age = chrono::Duration::seconds(UPDATE_OPERATION_HISTORY_SECONDS);
        stored.operations.retain(|operation| {
            validate_operation_id(&operation.operation_id).is_ok()
                && now - operation.seen_at <= max_age
        });
        if stored
            .busy_until
            .map(|busy_until| busy_until <= now)
            .unwrap_or(false)
        {
            stored.busy_until = None;
        }
        Ok(stored)
    }

    async fn save_update_state(&self, state: &StoredUpdateState) -> anyhow::Result<()> {
        let path = self.update_operations_path();
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        let text =
            serde_json::to_string_pretty(state).context("failed to serialize update state")?;
        tokio::fs::write(&path, text)
            .await
            .with_context(|| format!("failed to write {}", path.display()))
    }

    fn update_operations_path(&self) -> std::path::PathBuf {
        self.config.cluster.state_dir.join(UPDATE_OPERATIONS_FILE)
    }

    pub async fn peer_config(&self, host: &str) -> Option<PeerConfig> {
        self.config
            .peers
            .iter()
            .find(|peer| {
                peer.id.eq_ignore_ascii_case(host) || peer.display_name.eq_ignore_ascii_case(host)
            })
            .cloned()
    }

    pub async fn resolve_host_id(&self, host: &str) -> Option<String> {
        if self.matches_self(host) {
            return Some(self.config.node.id.clone());
        }
        self.peer_config(host).await.map(|peer| peer.id)
    }

    pub async fn drain_alerts(&self) -> Vec<AlertEvent> {
        self.alerts.lock().await.drain(..).collect()
    }

    pub async fn requeue_alerts(&self, alerts: Vec<AlertEvent>) {
        let mut queue = self.alerts.lock().await;
        for alert in alerts.into_iter().rev() {
            queue.push_front(alert);
        }
    }

    async fn enqueue_alert(&self, alert: AlertEvent) {
        self.alerts.lock().await.push_back(alert);
    }

    async fn enqueue_alerts(&self, alerts: Vec<AlertEvent>) {
        if alerts.is_empty() {
            return;
        }
        self.alerts.lock().await.extend(alerts);
    }

    async fn enqueue_metric_alerts(&self, peer_id: &str, metrics: &MetricsSnapshot) {
        let mut alerts = Vec::new();

        if let Some(sensor) = metrics.temperatures.iter().max_by(|left, right| {
            left.temperature_celsius
                .total_cmp(&right.temperature_celsius)
        }) && sensor.temperature_celsius >= self.config.alerts.cpu_temp_celsius
        {
            alerts.push((
                format!("{peer_id}:sensor:{}", sensor.label),
                AlertEvent::Threshold {
                    peer_id: peer_id.to_string(),
                    message: format!(
                        "high sensor temperature: {} {:.1}C",
                        sensor.label, sensor.temperature_celsius
                    ),
                },
            ));
        }

        for gpu in &metrics.gpus {
            let Some(temp) = gpu.temperature_celsius else {
                continue;
            };
            if temp >= self.config.alerts.gpu_temp_celsius {
                alerts.push((
                    format!("{peer_id}:gpu:{}:temp", gpu.index),
                    AlertEvent::Threshold {
                        peer_id: peer_id.to_string(),
                        message: format!("GPU #{} high temperature: {:.1}C", gpu.index, temp),
                    },
                ));
            }
        }

        for disk in &metrics.disks {
            if disk.total_bytes == 0 {
                continue;
            }
            let free_percent = disk.available_bytes as f64 * 100.0 / disk.total_bytes as f64;
            if free_percent <= self.config.alerts.disk_free_percent as f64 {
                alerts.push((
                    format!("{peer_id}:disk:{}:free", disk.mount_point),
                    AlertEvent::Threshold {
                        peer_id: peer_id.to_string(),
                        message: format!(
                            "low disk space on {}: {:.1}% free",
                            disk.mount_point, free_percent
                        ),
                    },
                ));
            }
        }

        if alerts.is_empty() {
            return;
        }

        let now = Utc::now();
        let throttle_window = chrono::Duration::minutes(10);
        let mut last_sent = self.threshold_alerts.lock().await;
        let mut ready = Vec::new();
        for (key, alert) in alerts {
            let should_send = last_sent
                .get(&key)
                .map(|last| now - *last >= throttle_window)
                .unwrap_or(true);
            if should_send {
                last_sent.insert(key, now);
                ready.push(alert);
            }
        }
        drop(last_sent);
        self.enqueue_alerts(ready).await;
    }

    fn compute_leader_locked(&self, cluster: &ClusterRuntime) -> Option<String> {
        let mut candidates = Vec::new();
        if self.config.node.eligible_leader {
            candidates.push((
                self.config.node.priority,
                self.started_at,
                self.config.node.id.clone(),
            ));
        }

        let now = Utc::now();
        let max_age = chrono::Duration::seconds(self.config.cluster.leader_lease_seconds as i64);
        for runtime in cluster.peers.values() {
            if !runtime.online || !runtime.config.eligible_leader() {
                continue;
            }
            let Some(last_seen) = runtime.last_seen else {
                continue;
            };
            if now - last_seen > max_age {
                continue;
            }
            let Some(health) = runtime.health.as_ref() else {
                continue;
            };
            if !health.eligible_leader {
                continue;
            }
            candidates.push((
                runtime.config.priority(),
                health.started_at,
                runtime.config.id.clone(),
            ));
        }

        candidates.sort_by(|left, right| {
            right
                .0
                .cmp(&left.0)
                .then_with(|| left.1.cmp(&right.1))
                .then_with(|| left.2.cmp(&right.2))
        });
        candidates.into_iter().next().map(|(_, _, id)| id)
    }

    fn matches_self(&self, host: &str) -> bool {
        host.eq_ignore_ascii_case(&self.config.node.id)
            || host.eq_ignore_ascii_case(&self.config.node.display_name)
    }
}

impl UpdateRuntime {
    fn merge_stored(&mut self, stored: StoredUpdateState) {
        for operation in stored.operations {
            match self.seen_operations.get(&operation.operation_id) {
                Some(existing) if *existing >= operation.seen_at => {}
                _ => {
                    self.seen_operations
                        .insert(operation.operation_id, operation.seen_at);
                }
            }
        }
        if let Some(stored_busy_until) = stored.busy_until
            && self
                .busy_until
                .map(|busy_until| stored_busy_until > busy_until)
                .unwrap_or(true)
        {
            self.busy_until = Some(stored_busy_until);
        }
    }

    fn prune(&mut self, now: DateTime<Utc>) {
        let max_age = chrono::Duration::seconds(UPDATE_OPERATION_HISTORY_SECONDS);
        self.seen_operations
            .retain(|_, seen_at| now - *seen_at <= max_age);
        if self
            .busy_until
            .map(|busy_until| busy_until <= now)
            .unwrap_or(false)
        {
            self.busy_until = None;
        }
        if self.seen_operations.len() <= UPDATE_OPERATION_HISTORY_LIMIT {
            return;
        }

        let mut entries: Vec<_> = self
            .seen_operations
            .iter()
            .map(|(operation_id, seen_at)| (operation_id.clone(), *seen_at))
            .collect();
        entries.sort_by_key(|(_, seen_at)| *seen_at);
        let remove_count = entries.len() - UPDATE_OPERATION_HISTORY_LIMIT;
        for (operation_id, _) in entries.into_iter().take(remove_count) {
            self.seen_operations.remove(&operation_id);
        }
    }

    fn to_stored_state(&self) -> StoredUpdateState {
        let mut operations: Vec<_> = self
            .seen_operations
            .iter()
            .map(|(operation_id, seen_at)| StoredUpdateOperation {
                operation_id: operation_id.clone(),
                seen_at: *seen_at,
            })
            .collect();
        operations.sort_by_key(|operation| operation.seen_at);
        StoredUpdateState {
            operations,
            busy_until: self.busy_until,
        }
    }
}

fn validate_operation_id(operation_id: &str) -> anyhow::Result<()> {
    if operation_id.is_empty() || operation_id.len() > 80 {
        bail!("invalid update operation id");
    }
    if operation_id
        .bytes()
        .any(|byte| !byte.is_ascii_alphanumeric() && byte != b'-' && byte != b'_')
    {
        bail!("invalid update operation id");
    }
    Ok(())
}

fn sign_update_request(secret: &str, request: &UpdateRequest) -> anyhow::Result<String> {
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes())
        .context("failed to initialize update signature")?;
    mac.update(update_signature_payload(request).as_bytes());
    let bytes = mac.finalize().into_bytes();
    Ok(hex_encode(bytes.as_slice()))
}

fn update_signature_payload(request: &UpdateRequest) -> String {
    format!(
        "v1\n{}\n{}\n{}\n{}\n{}",
        request.cluster_name,
        request.target_node_id,
        request.leader_id,
        request.operation_id,
        request.issued_at.to_rfc3339()
    )
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }
    output
}

async fn poll_peer(
    client: &reqwest::Client,
    secret: &str,
    cluster_name: &str,
    peer: &PeerConfig,
) -> anyhow::Result<(HealthResponse, MetricsSnapshot)> {
    let mut errors = Vec::new();
    for url in peer.urls() {
        match poll_peer_url(client, secret, cluster_name, peer, &url).await {
            Ok(result) => return Ok(result),
            Err(err) => errors.push(format!("{url}: {err}")),
        }
    }

    anyhow::bail!(
        "{}",
        errors
            .first()
            .cloned()
            .unwrap_or_else(|| "no peer URL configured".to_string())
    )
}

async fn poll_peer_url(
    client: &reqwest::Client,
    secret: &str,
    cluster_name: &str,
    peer: &PeerConfig,
    url: &str,
) -> anyhow::Result<(HealthResponse, MetricsSnapshot)> {
    let base = url.trim_end_matches('/');
    let health = client
        .get(format!("{base}/health"))
        .headers(auth_headers(secret)?)
        .send()
        .await?
        .error_for_status()?
        .json::<HealthResponse>()
        .await
        .context("invalid health response")?;
    validate_peer_health(cluster_name, peer, &health)?;

    let metrics = client
        .get(format!("{base}/metrics"))
        .headers(auth_headers(secret)?)
        .send()
        .await?
        .error_for_status()?
        .json::<MetricsSnapshot>()
        .await
        .context("invalid metrics response")?;
    validate_peer_metrics(peer, &metrics)?;

    Ok((health, metrics))
}

async fn fetch_peer_metrics(
    client: &reqwest::Client,
    secret: &str,
    peer: &PeerConfig,
) -> anyhow::Result<MetricsSnapshot> {
    let mut errors = Vec::new();
    for url in peer.urls() {
        let base = url.trim_end_matches('/').to_string();
        match client
            .get(format!("{base}/metrics"))
            .headers(auth_headers(secret)?)
            .send()
            .await
        {
            Ok(response) => match response.error_for_status() {
                Ok(response) => match response.json::<MetricsSnapshot>().await {
                    Ok(metrics) => {
                        if let Err(err) = validate_peer_metrics(peer, &metrics) {
                            errors.push(format!("{url}: {err}"));
                        } else {
                            return Ok(metrics);
                        }
                    }
                    Err(err) => errors.push(format!("{url}: invalid metrics response: {err}")),
                },
                Err(err) => errors.push(format!("{url}: {err}")),
            },
            Err(err) => errors.push(format!("{url}: {err}")),
        }
    }

    anyhow::bail!(
        "{}",
        errors
            .first()
            .cloned()
            .unwrap_or_else(|| "no peer URL configured".to_string())
    )
}

async fn fetch_peer_screenshot(
    client: &reqwest::Client,
    secret: &str,
    peer: &PeerConfig,
) -> anyhow::Result<Screenshot> {
    let mut errors = Vec::new();
    for url in peer.urls() {
        let base = url.trim_end_matches('/').to_string();
        match client
            .get(format!("{base}/screenshot"))
            .headers(auth_headers(secret)?)
            .send()
            .await
        {
            Ok(response) => match response.error_for_status() {
                Ok(response) => {
                    let content_type = response
                        .headers()
                        .get(reqwest::header::CONTENT_TYPE)
                        .and_then(|value| value.to_str().ok())
                        .unwrap_or("");
                    if !content_type.starts_with("image/png") {
                        errors.push(format!("{url}: screenshot response was not image/png"));
                        continue;
                    }
                    if let Some(length) = response.content_length()
                        && length > MAX_SCREENSHOT_BYTES
                    {
                        errors.push(format!("{url}: screenshot response exceeded size cap"));
                        continue;
                    }

                    match read_limited_body(response, MAX_SCREENSHOT_BYTES).await {
                        Ok(bytes) => {
                            return Ok(Screenshot {
                                bytes: bytes.to_vec(),
                                filename: format!(
                                    "{}-screenshot-{}.png",
                                    peer.id,
                                    Utc::now().format("%Y%m%d-%H%M%S")
                                ),
                                content_type: "image/png",
                            });
                        }
                        Err(err) => {
                            errors.push(format!("{url}: invalid screenshot response: {err}"))
                        }
                    }
                }
                Err(err) => errors.push(format!("{url}: {err}")),
            },
            Err(err) => errors.push(format!("{url}: {err}")),
        }
    }

    anyhow::bail!(
        "{}",
        errors
            .first()
            .cloned()
            .unwrap_or_else(|| "no peer URL configured".to_string())
    )
}

async fn read_limited_body(response: reqwest::Response, max_bytes: u64) -> anyhow::Result<Vec<u8>> {
    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("failed to read response body")?;
        if body.len() as u64 + chunk.len() as u64 > max_bytes {
            anyhow::bail!("response exceeded {max_bytes} byte limit");
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

async fn fetch_peer_internet_speedtest(
    client: &reqwest::Client,
    secret: &str,
    peer: &PeerConfig,
    bytes: Option<u64>,
) -> anyhow::Result<SpeedtestResult> {
    let mut errors = Vec::new();
    for url in peer.urls() {
        let base = url.trim_end_matches('/').to_string();
        match client
            .post(format!("{base}/speedtest/internet"))
            .headers(auth_headers(secret)?)
            .json(&SpeedtestRequest { bytes })
            .send()
            .await
        {
            Ok(response) => match response.error_for_status() {
                Ok(response) => match response.json::<SpeedtestResult>().await {
                    Ok(result) => {
                        if let Err(err) = validate_speedtest_source(peer, &result) {
                            errors.push(format!("{url}: {err}"));
                        } else {
                            return Ok(result);
                        }
                    }
                    Err(err) => errors.push(format!("{url}: invalid speedtest response: {err}")),
                },
                Err(err) => errors.push(format!("{url}: {err}")),
            },
            Err(err) => errors.push(format!("{url}: {err}")),
        }
    }

    anyhow::bail!(
        "{}",
        errors
            .first()
            .cloned()
            .unwrap_or_else(|| "no peer URL configured".to_string())
    )
}

async fn fetch_peer_to_peer_speedtest(
    client: &reqwest::Client,
    secret: &str,
    source: &PeerConfig,
    target_id: &str,
    bytes: Option<u64>,
) -> anyhow::Result<SpeedtestResult> {
    let mut errors = Vec::new();
    for url in source.urls() {
        let base = url.trim_end_matches('/').to_string();
        match client
            .post(format!("{base}/speedtest/peer"))
            .headers(auth_headers(secret)?)
            .json(&PeerSpeedtestRequest {
                peer_id: target_id.to_string(),
                bytes,
            })
            .send()
            .await
        {
            Ok(response) => match response.error_for_status() {
                Ok(response) => match response.json::<SpeedtestResult>().await {
                    Ok(result) => {
                        if let Err(err) = validate_speedtest_pair(source, target_id, &result) {
                            errors.push(format!("{url}: {err}"));
                        } else {
                            return Ok(result);
                        }
                    }
                    Err(err) => errors.push(format!("{url}: invalid speedtest response: {err}")),
                },
                Err(err) => errors.push(format!("{url}: {err}")),
            },
            Err(err) => errors.push(format!("{url}: {err}")),
        }
    }

    anyhow::bail!(
        "{}",
        errors
            .first()
            .cloned()
            .unwrap_or_else(|| "no peer URL configured".to_string())
    )
}

async fn fetch_peer_update(
    client: &reqwest::Client,
    peer: &PeerConfig,
    request: UpdateRequest,
) -> anyhow::Result<UpdateResult> {
    let mut errors = Vec::new();
    for url in peer.urls() {
        let base = url.trim_end_matches('/').to_string();
        match client
            .post(format!("{base}/update"))
            .json(&request)
            .send()
            .await
        {
            Ok(response) => match response.error_for_status() {
                Ok(response) => match response.json::<UpdateResult>().await {
                    Ok(result) => {
                        if result.node_id != peer.id {
                            errors.push(format!(
                                "{url}: update identity mismatch: expected {}, got {}",
                                peer.id, result.node_id
                            ));
                        } else {
                            return Ok(result);
                        }
                    }
                    Err(err) => errors.push(format!("{url}: invalid update response: {err}")),
                },
                Err(err) => errors.push(format!("{url}: {err}")),
            },
            Err(err) => errors.push(format!("{url}: {err}")),
        }
    }

    anyhow::bail!(
        "{}",
        errors
            .first()
            .cloned()
            .unwrap_or_else(|| "no peer URL configured".to_string())
    )
}

pub fn auth_headers(secret: &str) -> anyhow::Result<HeaderMap> {
    let mut headers = HeaderMap::new();
    headers.insert("x-watchdog-secret", HeaderValue::from_str(secret)?);
    Ok(headers)
}

fn validate_peer_health(
    cluster_name: &str,
    peer: &PeerConfig,
    health: &HealthResponse,
) -> anyhow::Result<()> {
    if health.cluster_name != cluster_name {
        anyhow::bail!(
            "cluster mismatch for {}: expected {}, got {}",
            peer.id,
            cluster_name,
            health.cluster_name
        );
    }
    if health.node_id != peer.id {
        anyhow::bail!(
            "peer identity mismatch: expected {}, got {}",
            peer.id,
            health.node_id
        );
    }
    Ok(())
}

fn validate_peer_metrics(peer: &PeerConfig, metrics: &MetricsSnapshot) -> anyhow::Result<()> {
    if metrics.node_id != peer.id {
        anyhow::bail!(
            "metrics identity mismatch: expected {}, got {}",
            peer.id,
            metrics.node_id
        );
    }
    Ok(())
}

fn validate_speedtest_source(peer: &PeerConfig, result: &SpeedtestResult) -> anyhow::Result<()> {
    if result.source_node != peer.id {
        anyhow::bail!(
            "speedtest source mismatch: expected {}, got {}",
            peer.id,
            result.source_node
        );
    }
    Ok(())
}

fn validate_speedtest_pair(
    source: &PeerConfig,
    target_id: &str,
    result: &SpeedtestResult,
) -> anyhow::Result<()> {
    validate_speedtest_source(source, result)?;
    if result.target != target_id {
        anyhow::bail!(
            "speedtest target mismatch: expected {}, got {}",
            target_id,
            result.target
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AlertConfig, ClusterConfig, NodeConfig, SpeedtestConfig, TelegramConfig};

    fn test_peer(id: &str) -> PeerConfig {
        PeerConfig {
            id: id.to_string(),
            display_name: id.to_string(),
            url: Some(format!("http://{id}.example:7373")),
            urls: Vec::new(),
            lan_url: None,
            tailscale_url: None,
            priority: Some(100),
            eligible_leader: Some(true),
            wol_mac: None,
            wol_broadcast: None,
            allow_shutdown: false,
            allow_screenshot: false,
            ssh_fallback: false,
        }
    }

    fn test_state_with_self_eligible(peers: Vec<PeerConfig>, self_eligible: bool) -> AppState {
        AppState::new(Config {
            cluster: ClusterConfig {
                name: "home".to_string(),
                state_dir: ".watchdog-state-test".into(),
                shared_secret_env: "CLUSTER_SECRET".to_string(),
                shared_secret: Some("test-secret".to_string()),
                allow_plaintext_peer_urls: true,
                heartbeat_interval_seconds: 10,
                leader_lease_seconds: 30,
                request_timeout_millis: 1000,
            },
            node: NodeConfig {
                id: "self".to_string(),
                display_name: "Self".to_string(),
                priority: 100,
                bind: "127.0.0.1:0".to_string(),
                public_url: "http://self.example:7373".to_string(),
                eligible_leader: self_eligible,
                allow_shutdown: false,
                allow_screenshot: false,
            },
            telegram: TelegramConfig::default(),
            peers,
            alerts: AlertConfig::default(),
            speedtest: SpeedtestConfig::default(),
            update: crate::config::UpdateConfig::default(),
        })
        .unwrap()
    }

    fn set_state_dir(state: &mut AppState, state_dir: std::path::PathBuf) {
        let mut config = state.config.as_ref().clone();
        config.cluster.state_dir = state_dir;
        state.config = Arc::new(config);
    }

    fn health(
        cluster_name: &str,
        node_id: &str,
        started_at: DateTime<Utc>,
        eligible_leader: bool,
    ) -> HealthResponse {
        HealthResponse {
            cluster_name: cluster_name.to_string(),
            node_id: node_id.to_string(),
            display_name: node_id.to_string(),
            version: "test".to_string(),
            started_at,
            uptime_seconds: 1,
            eligible_leader,
            priority: 100,
            leader_id: None,
        }
    }

    #[test]
    fn leader_election_prefers_highest_priority_then_oldest_started_eligible_node() {
        let older = PeerConfig {
            priority: Some(80),
            ..test_peer("older")
        };
        let newer = PeerConfig {
            priority: Some(100),
            ..test_peer("newer")
        };
        let state = test_state_with_self_eligible(vec![older.clone(), newer.clone()], false);
        let mut cluster = ClusterRuntime::default();
        let now = Utc::now();
        cluster.peers.insert(
            newer.id.clone(),
            PeerRuntime {
                config: newer,
                online: true,
                last_seen: Some(now),
                first_seen: Some(now + chrono::Duration::seconds(10)),
                last_error: None,
                missed_heartbeats: 0,
                health: Some(health(
                    "home",
                    "newer",
                    state.started_at + chrono::Duration::seconds(30),
                    true,
                )),
                metrics: None,
            },
        );
        cluster.peers.insert(
            older.id.clone(),
            PeerRuntime {
                config: older,
                online: true,
                last_seen: Some(now),
                first_seen: Some(now),
                last_error: None,
                missed_heartbeats: 0,
                health: Some(health(
                    "home",
                    "older",
                    state.started_at - chrono::Duration::seconds(30),
                    true,
                )),
                metrics: None,
            },
        );

        assert_eq!(
            state.compute_leader_locked(&cluster),
            Some("newer".to_string())
        );
    }

    #[test]
    fn leader_election_uses_reported_start_time_not_local_first_seen() {
        let alpha = PeerConfig {
            priority: Some(100),
            ..test_peer("alpha")
        };
        let beta = PeerConfig {
            priority: Some(100),
            ..test_peer("beta")
        };
        let state = test_state_with_self_eligible(vec![alpha.clone(), beta.clone()], false);
        let mut cluster = ClusterRuntime::default();
        let now = Utc::now();
        cluster.peers.insert(
            alpha.id.clone(),
            PeerRuntime {
                config: alpha,
                online: true,
                last_seen: Some(now),
                first_seen: Some(now + chrono::Duration::seconds(20)),
                last_error: None,
                missed_heartbeats: 0,
                health: Some(health(
                    "home",
                    "alpha",
                    state.started_at - chrono::Duration::seconds(20),
                    true,
                )),
                metrics: None,
            },
        );
        cluster.peers.insert(
            beta.id.clone(),
            PeerRuntime {
                config: beta,
                online: true,
                last_seen: Some(now),
                first_seen: Some(now),
                last_error: None,
                missed_heartbeats: 0,
                health: Some(health(
                    "home",
                    "beta",
                    state.started_at + chrono::Duration::seconds(20),
                    true,
                )),
                metrics: None,
            },
        );

        assert_eq!(
            state.compute_leader_locked(&cluster),
            Some("alpha".to_string())
        );
    }

    #[tokio::test]
    async fn update_targets_resolve_to_canonical_ids_and_deduplicate() {
        let alpha = test_peer("alpha");
        let beta = test_peer("beta");
        let state = test_state_with_self_eligible(vec![alpha, beta], true);

        let targets = state
            .resolve_update_targets(&[
                "alpha".to_string(),
                "alpha".to_string(),
                "Self".to_string(),
                "beta".to_string(),
            ])
            .await
            .unwrap();

        assert_eq!(targets, vec!["alpha", "self", "beta"]);
        assert!(
            state
                .resolve_update_targets(&["missing".to_string()])
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn update_signature_binds_cluster_leader_operation_and_target() {
        let state = test_state_with_self_eligible(Vec::new(), true);
        state.cluster.write().await.leader_id = Some("self".to_string());

        let request = state
            .signed_update_request("self", "test_operation-1")
            .await
            .unwrap();

        assert!(state.validate_update_request(&request).await.is_ok());

        let mut wrong_target = request.clone();
        wrong_target.target_node_id = "other".to_string();
        assert!(state.validate_update_request(&wrong_target).await.is_err());

        let mut wrong_signature = request;
        wrong_signature.signature = "bad".to_string();
        assert!(
            state
                .validate_update_request(&wrong_signature)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn update_operation_replay_protection_persists_to_state_dir() {
        let mut state = test_state_with_self_eligible(Vec::new(), true);
        let state_dir = std::env::current_dir()
            .unwrap()
            .join(format!(".watchdog-state-test-{}", Uuid::new_v4()));
        set_state_dir(&mut state, state_dir.clone());

        state
            .reserve_update_operation("persisted-op-1")
            .await
            .unwrap();
        assert!(
            state
                .reserve_update_operation("persisted-op-1")
                .await
                .is_err()
        );

        let mut restarted = test_state_with_self_eligible(Vec::new(), true);
        set_state_dir(&mut restarted, state_dir.clone());

        assert!(
            restarted
                .reserve_update_operation("persisted-op-1")
                .await
                .is_err()
        );
        let _ = std::fs::remove_dir_all(state_dir);
    }

    #[test]
    fn peer_health_validation_rejects_wrong_cluster_or_node() {
        let peer = test_peer("peer-a");
        let good = health("home", "peer-a", Utc::now(), true);
        assert!(validate_peer_health("home", &peer, &good).is_ok());

        let wrong_cluster = health("other", "peer-a", Utc::now(), true);
        assert!(validate_peer_health("home", &peer, &wrong_cluster).is_err());

        let wrong_node = health("home", "peer-b", Utc::now(), true);
        assert!(validate_peer_health("home", &peer, &wrong_node).is_err());
    }
}
