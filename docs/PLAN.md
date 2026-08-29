# distributed-watchdog Plan

Last updated: 2026-08-29

## Decision

Use Rust for the first implementation.

Rust is the better fit for a long-running, low-resource, cross-platform service that needs to run as a Linux systemd service, Windows service, Docker container, and later macOS launchd agent. Node remains a fallback only if Telegram or service packaging becomes unnecessarily slow in Rust.

## Network Model

Every machine runs the same watchdog agent. The agent has three responsibilities:

1. Serve a small authenticated local API for health, metrics, and power actions.
2. Track peer heartbeats and participate in leader election.
3. If elected leader, handle Telegram messages and fleet-wide notifications.

SSH should be optional, not required for normal operation. It is useful for bootstrap and emergency administration, but it should not be the primary protocol. This matters because Unraid may join later through Docker, where passwordless SSH is awkward.

## Supported Node Types

| Platform | Install target | Notes |
| --- | --- | --- |
| Linux desktop/server | systemd service | Metrics from procfs/sysfs, `sysinfo`, lm-sensors, NVIDIA tools when available. |
| Windows desktop/handheld | Windows service | Metrics from WMI/CIM/performance counters and vendor GPU tools where available. |
| Unraid | Docker container | Host networking preferred for Wake-on-LAN and peer discovery. Config mounted from host. |
| macOS | launchd agent | Later phase. Use sysctl/iostat and permission-gated tools such as powermetrics where possible. |

## Leader Election

The cluster should use a lease-based election rather than a permanent master.

- Each node has a stable `node_id`, priority, and shared cluster secret.
- Nodes exchange heartbeat records with start time, health, and metrics.
- A leader is considered valid while it is seen inside the configured lease window.
- If the leader lease expires, eligible online nodes elect a replacement.
- Election is deterministic: highest priority wins, then oldest live eligible start time breaks ties, then lowest stable node ID.
- Only the active leader processes Telegram updates.
- Non-leaders still expose local health and metrics APIs.

This is intentionally simple because the home network is small. If the cluster grows or split-brain becomes likely, the election layer can later be swapped for a small Raft implementation.

## Transport

Initial transport:

- HTTP on the trusted LAN or Tailscale interface, enabled explicitly in private config.
- Each peer can define both `lan_url` and `tailscale_url`; the agent tries all configured URLs.
- Shared bearer token for read/control endpoints and HMAC-signed update requests.
- HTTPS or mTLS should replace plaintext HTTP for any untrusted route.

Why not SSH as the core transport:

- Harder to make uniform across Linux, Windows, Unraid, and macOS.
- Awkward for containers and passwordless setup.
- Better suited as an optional admin backend than a peer protocol.

## Telegram Behavior

The leader should poll Telegram using the bot token. Webhooks are not ideal unless a stable public endpoint exists.

Alerts:

- Machine online.
- Machine offline after configurable missed heartbeats.
- Leader changed.
- High temperature, high load, low disk, or other configured thresholds.

Commands:

- `/status`
- `/leader`
- `/on <host>`
- `/off <host>`
- `/monitor <hosts...>` with one-second in-place updates, text bars, an inline Stop button, and a 10-minute cap.
- `/speedtest <host>` to test internet download throughput from one node.
- `/speedtest <source> <target>` to test node-to-node throughput; the source downloads from the target.
- `/screenshot <host>` for allowlisted interactive nodes.
- `/update [all|host ...]` for opt-in pull/build/install/restart on configured nodes, using signed short-lived operation requests and per-node update locks.
- `/userinfo`
- `/help`

Command authorization requires configured Telegram chat IDs. Sensitive commands require a private chat by default; group control is an explicit opt-in.

## Metrics

Common metrics:

- Uptime.
- CPU usage.
- CPU temperature when available.
- RAM usage.
- Disk usage.
- Network throughput.
- GPU usage, memory, and temperature when available.

Network speed tests:

- Internet tests download a bounded byte count from configured external test URLs.
- Peer tests use the authenticated watchdog HTTP API, not SSH, and transfer bounded byte streams between two configured nodes.
- LAN URLs are tried before Tailscale URLs so degraded local link speed is visible when the LAN route is healthy.

Linux collectors:

- `sysinfo` or procfs/sysfs for base metrics.
- lm-sensors where available.
- `nvidia-smi` for NVIDIA GPUs.

Windows collectors:

- WMI/CIM and performance counters for base metrics.
- Vendor-specific GPU support as a progressive enhancement.

Docker/Unraid collectors:

- Container can report its own view first.
- Optional host mounts can expose richer host metrics.
- Host shutdown support should be opt-in and explicitly configured.

## Power Control

Wake:

- Wake-on-LAN magic packet sent by the current leader.
- Each host needs configured MAC address and broadcast target.
- MAC addresses stay in private config, not the public repo.

Shutdown:

- Prefer local agent API on the target host.
- Each host must explicitly allow shutdown.
- Platform-specific shutdown command runs locally on the target node.
- SSH shutdown fallback can exist only for configured hosts.

Safety rules:

- No shutdown action unless the host is allowlisted.
- Confirmation can be required for selected hosts.
- The leader should refuse to shut itself down unless another eligible node is online or the command explicitly allows it.

## Config

Tracked files contain examples only. Real config should be ignored by git.

Suggested local files:

- `config.toml`
- `.env`
- `hosts.local.toml`

Config should include:

- Node identity.
- Peer list.
- LAN and Tailscale URLs for each peer.
- Telegram bot token and authorized chat ID from `TELEGRAM_TOKEN` and `TELEGRAM_CHAT_ID`.
- Cluster authentication secret.
- Local state directory for Telegram update offsets and other small runtime files.
- Host MAC addresses and Wake-on-LAN settings.
- Per-host feature flags for shutdown, metrics, SSH fallback, and Docker behavior.
- Optional fixed local updater command for nodes that allow `/update`.
- Default localhost bind with explicit private-config exposure for LAN or Tailscale addresses.

## Repository Layout

Planned layout:

```text
distributed-watchdog/
  crates/
    agent/
    config/
    election/
    metrics/
    net/
    power/
    telegram/
  deploy/
    systemd/
    windows-service/
    docker/
    macos-launchd/
  docs/
  config.example.toml
```

## Implementation Phases

1. Scaffold the Rust workspace and shared config model.
2. Implement a local node API with `/health`, `/metrics`, `/peers`, and `/power/*`.
3. Add Linux and Windows metrics collectors.
4. Add peer heartbeat and lease-based leader election.
5. Add Telegram command handling and alert formatting.
6. Add Wake-on-LAN.
7. Add safe shutdown handlers with per-host allowlists.
8. Add `/screenshot` and `/speedtest`.
9. Add packaging for systemd, Windows scheduled task, Docker, and macOS launchd.
10. Test first on fully available Linux machines, then expand to Windows endpoints.
11. Add Unraid Docker docs and macOS launchd support.

## Rollout Notes

Keep private fleet inventory, addresses, MAC addresses, and tokens out of this public repo. Put those details only in ignored per-node config files.

Future node types should be added through the same per-node config model. Docker/Unraid and macOS support should stay opt-in until the host permissions, screenshots, sensors, and shutdown behavior are tested on those platforms.
