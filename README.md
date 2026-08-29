# distributed-watchdog

Peer-elected Telegram watchdog for monitoring and power control across machines on a home network.

The service is intended to run on every machine. There is no fixed master: whichever eligible node wins leadership handles Telegram polling, fleet monitoring, alerts, and commands. Higher priority nodes win first; if the leader goes offline, another online node takes over.

## Goals

- Notify Telegram when machines go online or offline.
- Let Telegram commands wake, shut down, and inspect machines.
- Keep working when any single machine disappears.
- Support Linux, Windows, Docker/Unraid, and eventually macOS.
- Avoid committing private network details or secrets to the public repo.

## Commands

- `/status` - fleet status summary.
- `/leader` - current leader and peer election state.
- `/on <host>` - wake a host with Wake-on-LAN.
- `/off <host>` - request graceful shutdown for an allowlisted host.
- `/monitor <hosts...>` - live CPU, temperature, multi-GPU, RAM, disk, and network metrics, updated in place once per second with text bars, a Stop button, and a 10-minute maximum duration.
- `/speedtest <host>` - run an internet download speed test from a host.
- `/speedtest <source> <target>` - test node-to-node throughput; the source downloads from the target.
- `/screenshot <host>` - capture a fresh screenshot from an allowlisted node and send it through Telegram.
- `/update [all|host ...]` - ask nodes with opt-in updater config to pull, build, install, and restart.
- `/userinfo` - show Telegram user and chat IDs for authorization setup.
- `/help` - show available commands.

Sensitive commands require a private authorized chat by default. Group control is opt-in with `telegram.allow_group_control = true` plus private-config `authorized_user_ids`; sensitive group commands must mention the bot explicitly. Remote update scheduling uses signed, short-lived operation requests, persisted replay protection, and per-node update locks.

Non-loopback plaintext HTTP peer URLs are rejected unless the private config sets `cluster.allow_plaintext_peer_urls = true`. Use that only on a trusted, firewalled LAN/Tailscale network; prefer HTTPS/mTLS for anything less controlled.

The bot registers these commands with Telegram on startup so the Telegram command menu and autocomplete stay populated automatically.

## Current Status

First working Rust agent. See [docs/PLAN.md](docs/PLAN.md) for the architecture and rollout plan.
See [docs/DEPLOYMENT.md](docs/DEPLOYMENT.md) for private per-machine configuration.

The public repo intentionally contains only example machine IDs and placeholder addresses. Real hostnames, IPs, Tailscale names, Wake-on-LAN MACs, Telegram tokens, chat IDs, and cluster secrets belong in ignored local files.

## Local Secrets

Copy `.env.example` to `.env` and set:

- `TELEGRAM_TOKEN`
- `TELEGRAM_CHAT_ID`
- `CLUSTER_SECRET`

Use `/userinfo` in Telegram to get the chat ID during setup.
