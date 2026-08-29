#!/usr/bin/env sh
set -eu

install_dir="${INSTALL_DIR:-/etc/distributed-watchdog}"
binary_path="${BINARY_PATH:-/usr/local/bin/distributed-watchdog}"
service_path="${SERVICE_PATH:-/etc/systemd/system/distributed-watchdog.service}"

if ! command -v systemctl >/dev/null 2>&1; then
  echo "systemd is required" >&2
  exit 1
fi

if ! id distributed-watchdog >/dev/null 2>&1; then
  useradd --system --home-dir "$install_dir" --shell /usr/sbin/nologin distributed-watchdog
fi

mkdir -p "$install_dir"
chown root:distributed-watchdog "$install_dir"
chmod 750 "$install_dir"

if [ ! -f "$install_dir/env" ]; then
  install -m 640 -o root -g distributed-watchdog /dev/null "$install_dir/env"
else
  chown root:distributed-watchdog "$install_dir/env"
  chmod 640 "$install_dir/env"
fi

if [ ! -f "$install_dir/config.toml" ]; then
  echo "missing $install_dir/config.toml" >&2
  exit 1
fi
chown root:distributed-watchdog "$install_dir/config.toml"
chmod 640 "$install_dir/config.toml"

if [ ! -x "$binary_path" ]; then
  echo "missing executable $binary_path" >&2
  exit 1
fi

install -m 644 "$(dirname "$0")/../systemd/distributed-watchdog.service" "$service_path"
systemctl daemon-reload
systemctl enable --now distributed-watchdog.service
systemctl status distributed-watchdog.service --no-pager
