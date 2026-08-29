# Deployment

This repo is public. Do not commit real hostnames, IPs, Tailscale names, MAC addresses, Telegram tokens, chat IDs, or cluster secrets.

Use private files on each node:

- `.env`
- `config.toml`

Required `.env` keys:

```text
TELEGRAM_TOKEN=<bot token>
TELEGRAM_CHAT_ID=<authorized Telegram chat id>
CLUSTER_SECRET=<shared cluster secret>
```

Use `/userinfo` before setting `TELEGRAM_CHAT_ID`. That command is rate limited only for unauthorized chats and is the only command accepted before the chat ID is configured.

Use a random `CLUSTER_SECRET`; 32+ characters is recommended. The agent rejects very short values.

Sensitive commands (`/monitor`, `/on`, `/off`, `/screenshot`, `/speedtest`, and `/update`) require a private Telegram chat by default. Group control is opt-in and also requires private-config `telegram.authorized_user_ids`, so an authorized group chat does not automatically authorize every group member. In groups, sensitive commands must mention the bot explicitly, such as `/update@example_bot`.

The example config binds to localhost. Set `node.bind` to a LAN or Tailscale address only in private config, and firewall TCP 7373 to trusted peer addresses. Non-loopback `http://` peer URLs require `cluster.allow_plaintext_peer_urls = true` in private config. Use that only on trusted LAN/Tailscale networks; use HTTPS or a private tunnel if the path crosses anything untrusted.

Node `priority` affects leader election after uptime/first-seen ordering. Higher values win ties among eligible live nodes.

## Peer URLs

Each peer can define one or more URLs:

```toml
lan_url = "http://192.168.1.10:7373"
tailscale_url = "http://100.64.0.10:7373"
urls = ["http://extra-address.example:7373"]
```

The agent tries configured URLs in this order:

1. `url`
2. `lan_url`
3. `tailscale_url`
4. `urls`

Use LAN first for lowest latency and Tailscale as fallback when both networks are trusted and firewall restricted.

On Windows, open the watchdog port only to explicit LAN and Tailscale ranges or peer `/32` addresses:

```powershell
.\deploy\windows\allow-firewall.ps1 -LanCidr 192.168.1.0/24 -TailscaleCidr 100.64.0.0/10
```

The helper applies only to Private/Domain firewall profiles. Prefer per-peer `/32` addresses when a laptop may move between networks.

## Speed Tests

`/speedtest <host>` runs a download test from that host to a configured internet endpoint. The default endpoint is Cloudflare's speed test download URL, and it can be replaced in private config:

```toml
[speedtest]
internet_bytes = 25000000
peer_bytes = 64000000
max_bytes = 256000000
timeout_seconds = 30
rpc_timeout_seconds = 45
internet_download_urls = ["https://speed.cloudflare.com/__down?bytes={bytes}"]
```

`/speedtest <source> <target>` asks the source node to download a bounded byte stream from the target node over the same authenticated watchdog HTTP channel. The agent tries the target's configured URLs in normal order, so LAN is preferred and Tailscale is fallback if both are present.

Only one expensive speed test runs per node at a time. Internet and peer transfers must deliver exactly the configured byte count so short or oversized responses do not produce misleading throughput numbers.

## Updates

`/update` and `/update all` ask every configured node to schedule its local updater. `/update <host ...>` targets specific nodes. Peers are updated before the current leader so the leader does not restart before sending requests to the rest of the cluster.

Remote update scheduling uses a signed operation id with a short timestamp window. Nodes reject stale, duplicate, mistargeted, or non-leader update requests. Recent operation IDs and update cooldown state are persisted under `cluster.state_dir`, so replay protection survives service restarts. Each node also has an in-process cooldown and the packaged updater scripts use lock files so repeated requests cannot race multiple builds.

Updates are disabled unless each node opts in with a fixed local command in private `config.toml`:

```toml
[update]
enabled = true
timeout_seconds = 30
command = ["/opt/distributed-watchdog/update-agent.sh", "https://github.com/example/distributed-watchdog.git", "main"]
```

Windows example:

```toml
[update]
enabled = true
timeout_seconds = 30
command = ["C:\\Windows\\System32\\WindowsPowerShell\\v1.0\\powershell.exe", "-NoProfile", "-ExecutionPolicy", "Bypass", "-File", "C:\\ProgramData\\distributed-watchdog\\update-agent.ps1", "-RepoUrl", "https://github.com/example/distributed-watchdog.git", "-Branch", "main", "-InstallDir", "C:\\ProgramData\\distributed-watchdog"]
```

The updater command is not taken from Telegram. It is fixed in private node config, launches a detached update job, and should return quickly with `update scheduled`. Detailed stdout/stderr stays in local `update.log` and is not returned to Telegram/API callers. The detached script checks `origin/<branch>` before building; if no commit is pending, it logs `already up to date`.

For stronger provenance, set `VERIFY_GIT_SIGNATURES=1` in the updater environment and publish signed commits or tags. The scripts also refuse to reuse an existing source checkout if its `origin` remote does not match the configured repository URL.

## Linux

Build:

```sh
cargo build --release
```

Install the binary at:

```text
/usr/local/bin/distributed-watchdog
```

Install private config at:

```text
/etc/distributed-watchdog/config.toml
/etc/distributed-watchdog/env
```

Install and start:

```sh
sudo deploy/linux/install-systemd.sh
```

For a user-local test start without systemd:

```sh
deploy/linux/start-agent.sh
```

## Windows

Build:

```powershell
cargo build --release
```

Place files under:

```text
C:\ProgramData\distributed-watchdog\
```

Expected files:

- `distributed-watchdog.exe`
- `config.toml`
- `.env`

Install startup task from an elevated PowerShell:

```powershell
.\deploy\windows\install-scheduled-task.ps1
```

By default this installs a normal interactive user logon task so `/screenshot` can access the desktop. Use `-RunElevated` only if Windows shutdown support requires it. For a headless node that should run before login, use `-RunAsSystem`; screenshot capture will usually be unavailable in that mode.

For a one-shot detached start from the install directory:

```powershell
.\deploy\windows\start-agent.ps1
```

The helper stops an existing `distributed-watchdog` process and starts the local `distributed-watchdog.exe` with `config.toml`.

The scheduled-task installer hardens the install directory so the interactive account gets read/execute access to program files and modify access only to `.watchdog-state` and `logs`.

## Unraid

Use the Docker deployment with host networking so Wake-on-LAN and peer discovery can work.

```sh
docker compose -f deploy/docker/compose.unraid.example.yml up -d
```

Host shutdown should stay disabled unless explicitly configured and tested.

Telegram `/off <host>` and the HTTP shutdown endpoint request shutdown immediately by default, with no countdown delay.

The example container mounts config read-only and stores watchdog state in a separate writable state directory. Basic host CPU and memory visibility depends on Docker/Unraid runtime behavior; deeper sensors may need a future host helper or explicit device/sysfs mounts.

Use host firewall rules on Unraid so TCP 7373 is reachable only from trusted LAN/Tailscale peers. Host networking exposes the service on all host interfaces unless the node config binds to a specific address.

For this compose file, set the private Unraid config to use the writable state mount:

```toml
[cluster]
state_dir = "/state"
```

## macOS

Use the launchd plist in `deploy/macos` after placing the binary and private config under `/usr/local`.

Screenshots require Screen Recording permissions for the running service user.

Do not use shell-sourced env files for launchd. Put private values into a local copy such as `com.local.distributed-watchdog.plist` with `EnvironmentVariables`, keep the plist/config root-owned, and use permissions no broader than `640` for secrets and config. Local plist names matching `*.local.plist` and `*.private.plist` are ignored by git.

## Screenshots

`/screenshot <host>` asks the target node for a fresh screenshot and sends it through Telegram.

Platform notes:

- Windows: uses the interactive desktop APIs. It works only when the service has access to the user session.
- Linux: tries common screenshot tools such as `grim`, `gnome-screenshot`, `spectacle`, `scrot`, `maim`, or ImageMagick `import`.
- macOS: uses `screencapture`; Screen Recording permission is required.
- Docker/Unraid: usually headless, so screenshots are normally unavailable.
