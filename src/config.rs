use std::{
    collections::HashSet,
    env,
    net::SocketAddr,
    path::{Path, PathBuf},
};

use anyhow::{Context, anyhow, bail};
use reqwest::Url;
use serde::Deserialize;

const MIN_CLUSTER_SECRET_LENGTH: usize = 24;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub cluster: ClusterConfig,
    pub node: NodeConfig,
    #[serde(default)]
    pub telegram: TelegramConfig,
    #[serde(default)]
    pub peers: Vec<PeerConfig>,
    #[serde(default)]
    pub alerts: AlertConfig,
    #[serde(default)]
    pub speedtest: SpeedtestConfig,
    #[serde(default)]
    pub update: UpdateConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ClusterConfig {
    pub name: String,
    #[serde(default = "default_state_dir")]
    pub state_dir: PathBuf,
    #[serde(default = "default_secret_env")]
    pub shared_secret_env: String,
    #[serde(default)]
    pub shared_secret: Option<String>,
    #[serde(default)]
    pub allow_plaintext_peer_urls: bool,
    #[serde(default = "default_heartbeat")]
    pub heartbeat_interval_seconds: u64,
    #[serde(default = "default_lease")]
    pub leader_lease_seconds: u64,
    #[serde(default = "default_timeout")]
    pub request_timeout_millis: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct NodeConfig {
    pub id: String,
    pub display_name: String,
    #[serde(default = "default_priority")]
    pub priority: i64,
    #[serde(default = "default_bind")]
    pub bind: String,
    pub public_url: String,
    #[serde(default = "default_true")]
    pub eligible_leader: bool,
    #[serde(default)]
    pub allow_shutdown: bool,
    #[serde(default)]
    pub allow_screenshot: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TelegramConfig {
    #[serde(default = "default_telegram_env")]
    pub bot_token_env: String,
    #[serde(default = "default_chat_id_env")]
    pub chat_id_env: String,
    #[serde(default)]
    pub bot_token: Option<String>,
    #[serde(default)]
    pub authorized_chat_ids: Vec<i64>,
    #[serde(default)]
    pub authorized_user_ids: Vec<i64>,
    #[serde(default = "default_poll_timeout")]
    pub polling_timeout_seconds: u64,
    #[serde(default)]
    pub allow_group_control: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PeerConfig {
    pub id: String,
    pub display_name: String,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub urls: Vec<String>,
    #[serde(default)]
    pub lan_url: Option<String>,
    #[serde(default)]
    pub tailscale_url: Option<String>,
    #[serde(default)]
    pub priority: Option<i64>,
    #[serde(default = "default_true_opt")]
    pub eligible_leader: Option<bool>,
    #[serde(default)]
    pub wol_mac: Option<String>,
    #[serde(default)]
    pub wol_broadcast: Option<String>,
    #[serde(default)]
    pub allow_shutdown: bool,
    #[serde(default)]
    pub allow_screenshot: bool,
    #[serde(default)]
    pub ssh_fallback: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AlertConfig {
    #[serde(default = "default_missed_heartbeats")]
    pub offline_after_missed_heartbeats: u64,
    #[serde(default = "default_temp_threshold")]
    pub cpu_temp_celsius: f32,
    #[serde(default = "default_temp_threshold")]
    pub gpu_temp_celsius: f32,
    #[serde(default = "default_disk_threshold")]
    pub disk_free_percent: f32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SpeedtestConfig {
    #[serde(default = "default_internet_speedtest_bytes")]
    pub internet_bytes: u64,
    #[serde(default = "default_peer_speedtest_bytes")]
    pub peer_bytes: u64,
    #[serde(default = "default_speedtest_max_bytes")]
    pub max_bytes: u64,
    #[serde(default = "default_speedtest_timeout")]
    pub timeout_seconds: u64,
    #[serde(default = "default_speedtest_rpc_timeout")]
    pub rpc_timeout_seconds: u64,
    #[serde(default = "default_internet_download_urls")]
    pub internet_download_urls: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct UpdateConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub command: Vec<String>,
    #[serde(default = "default_update_timeout")]
    pub timeout_seconds: u64,
}

impl Default for TelegramConfig {
    fn default() -> Self {
        Self {
            bot_token_env: default_telegram_env(),
            chat_id_env: default_chat_id_env(),
            bot_token: None,
            authorized_chat_ids: Vec::new(),
            authorized_user_ids: Vec::new(),
            polling_timeout_seconds: default_poll_timeout(),
            allow_group_control: false,
        }
    }
}

impl Default for AlertConfig {
    fn default() -> Self {
        Self {
            offline_after_missed_heartbeats: default_missed_heartbeats(),
            cpu_temp_celsius: default_temp_threshold(),
            gpu_temp_celsius: default_temp_threshold(),
            disk_free_percent: default_disk_threshold(),
        }
    }
}

impl Default for SpeedtestConfig {
    fn default() -> Self {
        Self {
            internet_bytes: default_internet_speedtest_bytes(),
            peer_bytes: default_peer_speedtest_bytes(),
            max_bytes: default_speedtest_max_bytes(),
            timeout_seconds: default_speedtest_timeout(),
            rpc_timeout_seconds: default_speedtest_rpc_timeout(),
            internet_download_urls: default_internet_download_urls(),
        }
    }
}

impl Default for UpdateConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            command: Vec::new(),
            timeout_seconds: default_update_timeout(),
        }
    }
}

impl Config {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("could not read {}", path.display()))?;
        toml::from_str(&text).with_context(|| format!("invalid TOML in {}", path.display()))
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        if self.node.id.trim().is_empty() {
            bail!("node.id must not be empty");
        }
        if self.node.public_url.trim().is_empty() {
            bail!("node.public_url must not be empty");
        }
        self.cluster.validate(&self.alerts)?;
        self.telegram.validate()?;
        self.alerts.validate()?;
        self.node.bind_addr()?;
        let shared_secret = self.shared_secret()?;
        if shared_secret.len() < MIN_CLUSTER_SECRET_LENGTH {
            bail!("CLUSTER_SECRET must be at least {MIN_CLUSTER_SECRET_LENGTH} characters");
        }
        self.speedtest.validate()?;
        self.update.validate()?;

        let mut ids = HashSet::new();
        ids.insert(self.node.id.as_str());
        for peer in &self.peers {
            if peer.id.trim().is_empty() {
                bail!("peer id must not be empty");
            }
            if peer.id == self.node.id {
                bail!("peers must not include this node id ({})", self.node.id);
            }
            if !ids.insert(peer.id.as_str()) {
                bail!("duplicate peer id: {}", peer.id);
            }
            if peer.urls().is_empty() {
                bail!(
                    "peer {} must have url, urls, lan_url, or tailscale_url",
                    peer.id
                );
            }
            for url in peer.urls() {
                let parsed = Url::parse(&url)
                    .with_context(|| format!("peer {} has invalid URL {url}", peer.id))?;
                match parsed.scheme() {
                    "https" => {}
                    "http" if self.cluster.allow_plaintext_peer_urls => {}
                    "http" if is_loopback_url(&parsed) => {}
                    "http" => bail!(
                        "peer {} URL uses plaintext HTTP; set cluster.allow_plaintext_peer_urls=true only for a trusted LAN/Tailscale/firewalled network",
                        peer.id
                    ),
                    scheme => bail!("peer {} URL uses unsupported scheme {scheme}", peer.id),
                }
                if parsed.host_str().is_none() {
                    bail!("peer {} URL must include a host", peer.id);
                }
            }
        }

        Ok(())
    }

    pub fn shared_secret(&self) -> anyhow::Result<String> {
        if let Some(secret) = self
            .cluster
            .shared_secret
            .as_ref()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
        {
            return Ok(secret.to_string());
        }
        env::var(&self.cluster.shared_secret_env)
            .map(|s| s.trim().to_string())
            .ok()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                anyhow!(
                    "{} must be set or cluster.shared_secret must be configured",
                    self.cluster.shared_secret_env
                )
            })
    }

    pub fn telegram_token(&self) -> Option<String> {
        self.telegram
            .bot_token
            .as_ref()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .or_else(|| {
                env::var(&self.telegram.bot_token_env)
                    .ok()
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
            })
    }

    pub fn telegram_chat_ids(&self) -> Vec<i64> {
        let mut ids = self.telegram.authorized_chat_ids.clone();
        if let Ok(value) = env::var(&self.telegram.chat_id_env) {
            ids.extend(
                value
                    .split([',', ' ', '\n', '\r', '\t'])
                    .map(str::trim)
                    .filter(|part| !part.is_empty())
                    .filter_map(|part| part.parse::<i64>().ok()),
            );
        }
        ids.sort_unstable();
        ids.dedup();
        ids
    }
}

impl ClusterConfig {
    pub fn validate(&self, alerts: &AlertConfig) -> anyhow::Result<()> {
        if self.name.trim().is_empty() {
            bail!("cluster.name must not be empty");
        }
        if self.state_dir.as_os_str().is_empty() {
            bail!("cluster.state_dir must not be empty");
        }
        if self.shared_secret_env.trim().is_empty() {
            bail!("cluster.shared_secret_env must not be empty");
        }
        if self.heartbeat_interval_seconds == 0 {
            bail!("cluster.heartbeat_interval_seconds must be greater than zero");
        }
        if self.leader_lease_seconds == 0 {
            bail!("cluster.leader_lease_seconds must be greater than zero");
        }
        if self.request_timeout_millis == 0 {
            bail!("cluster.request_timeout_millis must be greater than zero");
        }

        let minimum_lease = self
            .heartbeat_interval_seconds
            .saturating_mul(alerts.offline_after_missed_heartbeats.max(1));
        if self.leader_lease_seconds < minimum_lease {
            bail!(
                "cluster.leader_lease_seconds must be at least heartbeat_interval_seconds * alerts.offline_after_missed_heartbeats ({minimum_lease}s)"
            );
        }

        Ok(())
    }
}

impl TelegramConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.bot_token_env.trim().is_empty() {
            bail!("telegram.bot_token_env must not be empty");
        }
        if self.chat_id_env.trim().is_empty() {
            bail!("telegram.chat_id_env must not be empty");
        }
        if self.polling_timeout_seconds == 0 {
            bail!("telegram.polling_timeout_seconds must be greater than zero");
        }
        if self.polling_timeout_seconds > 300 {
            bail!("telegram.polling_timeout_seconds must not exceed 300");
        }
        if self.allow_group_control && self.authorized_user_ids.is_empty() {
            bail!("telegram.authorized_user_ids must be set when allow_group_control is true");
        }
        Ok(())
    }
}

impl AlertConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.offline_after_missed_heartbeats == 0 {
            bail!("alerts.offline_after_missed_heartbeats must be greater than zero");
        }
        if !self.cpu_temp_celsius.is_finite() || self.cpu_temp_celsius <= 0.0 {
            bail!("alerts.cpu_temp_celsius must be a positive finite value");
        }
        if !self.gpu_temp_celsius.is_finite() || self.gpu_temp_celsius <= 0.0 {
            bail!("alerts.gpu_temp_celsius must be a positive finite value");
        }
        if !self.disk_free_percent.is_finite()
            || self.disk_free_percent <= 0.0
            || self.disk_free_percent > 100.0
        {
            bail!("alerts.disk_free_percent must be greater than 0 and at most 100");
        }
        Ok(())
    }
}

impl NodeConfig {
    pub fn bind_addr(&self) -> anyhow::Result<SocketAddr> {
        self.bind
            .parse()
            .with_context(|| format!("invalid bind address {}", self.bind))
    }
}

impl PeerConfig {
    pub fn priority(&self) -> i64 {
        self.priority.unwrap_or(default_priority())
    }

    pub fn eligible_leader(&self) -> bool {
        self.eligible_leader.unwrap_or(true)
    }

    pub fn urls(&self) -> Vec<String> {
        let mut urls = Vec::new();
        let mut seen = HashSet::new();
        let mut push_url = |url: &str| {
            let url = url.trim();
            if !url.is_empty() && seen.insert(url.to_string()) {
                urls.push(url.to_string());
            }
        };
        if let Some(url) = self
            .url
            .as_ref()
            .map(|url| url.trim())
            .filter(|url| !url.is_empty())
        {
            push_url(url);
        }
        if let Some(url) = self
            .lan_url
            .as_ref()
            .map(|url| url.trim())
            .filter(|url| !url.is_empty())
        {
            push_url(url);
        }
        if let Some(url) = self
            .tailscale_url
            .as_ref()
            .map(|url| url.trim())
            .filter(|url| !url.is_empty())
        {
            push_url(url);
        }
        for url in &self.urls {
            push_url(url);
        }
        urls
    }
}

impl SpeedtestConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.internet_bytes == 0 {
            bail!("speedtest.internet_bytes must be greater than zero");
        }
        if self.peer_bytes == 0 {
            bail!("speedtest.peer_bytes must be greater than zero");
        }
        if self.max_bytes == 0 {
            bail!("speedtest.max_bytes must be greater than zero");
        }
        if self.internet_bytes > self.max_bytes {
            bail!("speedtest.internet_bytes must not exceed speedtest.max_bytes");
        }
        if self.peer_bytes > self.max_bytes {
            bail!("speedtest.peer_bytes must not exceed speedtest.max_bytes");
        }
        if self.timeout_seconds == 0 {
            bail!("speedtest.timeout_seconds must be greater than zero");
        }
        if self.rpc_timeout_seconds < self.timeout_seconds {
            bail!("speedtest.rpc_timeout_seconds must be at least speedtest.timeout_seconds");
        }
        if self
            .internet_download_urls
            .iter()
            .all(|url| url.trim().is_empty())
        {
            bail!("speedtest.internet_download_urls must contain at least one URL");
        }
        for template in self
            .internet_download_urls
            .iter()
            .map(|url| url.trim())
            .filter(|url| !url.is_empty())
        {
            let url = template.replace("{bytes}", "1");
            let parsed = Url::parse(&url)
                .with_context(|| format!("invalid speedtest internet URL {template}"))?;
            match parsed.scheme() {
                "http" | "https" => {}
                scheme => bail!("speedtest internet URL uses unsupported scheme {scheme}"),
            }
            if parsed.host_str().is_none() {
                bail!("speedtest internet URL must include a host");
            }
        }
        Ok(())
    }
}

impl UpdateConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.timeout_seconds == 0 {
            bail!("update.timeout_seconds must be greater than zero");
        }
        if self.timeout_seconds > 300 {
            bail!("update.timeout_seconds must not exceed 300");
        }
        if self.enabled {
            let Some(program) = self.command.first() else {
                bail!("update.command must be set when update.enabled is true");
            };
            if program.trim().is_empty() {
                bail!("update.command program must not be empty");
            }
            if self.command.iter().any(|part| part.trim().is_empty()) {
                bail!("update.command must not contain empty arguments");
            }
        }
        Ok(())
    }
}

fn default_secret_env() -> String {
    "CLUSTER_SECRET".to_string()
}

fn default_state_dir() -> PathBuf {
    ".watchdog-state".into()
}

fn default_telegram_env() -> String {
    "TELEGRAM_TOKEN".to_string()
}

fn default_chat_id_env() -> String {
    "TELEGRAM_CHAT_ID".to_string()
}

fn default_heartbeat() -> u64 {
    10
}

fn default_lease() -> u64 {
    30
}

fn default_timeout() -> u64 {
    15000
}

fn default_priority() -> i64 {
    100
}

fn default_bind() -> String {
    "127.0.0.1:7373".to_string()
}

fn default_poll_timeout() -> u64 {
    30
}

fn default_true() -> bool {
    true
}

fn default_true_opt() -> Option<bool> {
    Some(true)
}

fn default_missed_heartbeats() -> u64 {
    3
}

fn default_temp_threshold() -> f32 {
    85.0
}

fn default_disk_threshold() -> f32 {
    10.0
}

fn default_internet_speedtest_bytes() -> u64 {
    25_000_000
}

fn default_peer_speedtest_bytes() -> u64 {
    64_000_000
}

fn default_speedtest_max_bytes() -> u64 {
    256_000_000
}

fn default_speedtest_timeout() -> u64 {
    30
}

fn default_speedtest_rpc_timeout() -> u64 {
    45
}

fn default_internet_download_urls() -> Vec<String> {
    vec!["https://speed.cloudflare.com/__down?bytes={bytes}".to_string()]
}

fn default_update_timeout() -> u64 {
    30
}

fn is_loopback_url(url: &Url) -> bool {
    let Some(host) = url.host_str() else {
        return false;
    };
    host.eq_ignore_ascii_case("localhost")
        || host == "127.0.0.1"
        || host == "::1"
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peer_urls_preserve_preferred_order_and_deduplicate() {
        let peer = PeerConfig {
            id: "example".to_string(),
            display_name: "Example".to_string(),
            url: Some(" http://primary:7373 ".to_string()),
            lan_url: Some("http://lan:7373".to_string()),
            tailscale_url: Some("http://tail:7373".to_string()),
            urls: vec![
                "http://tail:7373".to_string(),
                "http://extra:7373".to_string(),
                " ".to_string(),
            ],
            priority: None,
            eligible_leader: None,
            wol_mac: None,
            wol_broadcast: None,
            allow_shutdown: false,
            allow_screenshot: false,
            ssh_fallback: false,
        };

        assert_eq!(
            peer.urls(),
            vec![
                "http://primary:7373",
                "http://lan:7373",
                "http://tail:7373",
                "http://extra:7373",
            ]
        );
    }

    #[test]
    fn speedtest_config_rejects_unbounded_defaults() {
        let config = SpeedtestConfig {
            internet_bytes: 25_000_000,
            peer_bytes: 64_000_000,
            max_bytes: 10_000_000,
            timeout_seconds: 30,
            rpc_timeout_seconds: 45,
            internet_download_urls: default_internet_download_urls(),
        };

        assert!(config.validate().is_err());
    }

    #[test]
    fn plaintext_peer_urls_require_explicit_opt_in() {
        let mut config: Config = toml::from_str(
            r#"
[cluster]
name = "test"
shared_secret = "abcdefghijklmnopqrstuvwxyz123456"

[node]
id = "self"
display_name = "Self"
public_url = "http://127.0.0.1:7373"

[[peers]]
id = "peer"
display_name = "Peer"
lan_url = "http://192.168.1.10:7373"
"#,
        )
        .unwrap();

        assert!(config.validate().is_err());
        config.cluster.allow_plaintext_peer_urls = true;
        assert!(config.validate().is_ok());
    }
}
