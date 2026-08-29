# distributed-watchdog

Peer-elected Telegram watchdog for monitoring and power control across machines on a home network.

The service is intended to run on every machine. There is no fixed master: whichever eligible node wins leadership handles Telegram polling, fleet monitoring, alerts, and commands. If that node goes offline, another online node takes over.

## Goals

- Notify Telegram when machines go online or offline.
- Let Telegram commands wake, shut down, and inspect machines.
- Keep working when any single machine disappears.
- Support Linux, Windows, Docker/Unraid, and eventually macOS.
- Avoid committing private network details or secrets to the public repo.

## Planned Commands

- `/status` - fleet status summary.
- `/leader` - current leader and peer election state.
- `/on <host>` - wake a host with Wake-on-LAN.
- `/off <host>` - request graceful shutdown for an allowlisted host.
- `/monitor <hosts...>` - report CPU, temperature, GPU, RAM, disk, and network metrics.
- `/silence` - temporarily suppress alerts.
- `/resume` - re-enable alerts.
- `/help` - show available commands.

## Current Status

Planning phase. See [docs/PLAN.md](docs/PLAN.md) for the architecture and rollout plan.
