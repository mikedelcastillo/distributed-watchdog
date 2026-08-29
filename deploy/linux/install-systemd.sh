#!/usr/bin/env sh
set -eu

install_dir="${INSTALL_DIR:-/etc/distributed-watchdog}"
binary_path="${BINARY_PATH:-/usr/local/bin/distributed-watchdog}"
service_path="${SERVICE_PATH:-/etc/systemd/system/distributed-watchdog.service}"
override_dir="${OVERRIDE_DIR:-/etc/systemd/system/distributed-watchdog.service.d}"
override_path="$override_dir/elevated.conf"

if [ "$(id -u)" -ne 0 ]; then
  echo "this installer must be run as root (for example: sudo $0)" >&2
  exit 1
fi

if ! command -v systemctl >/dev/null 2>&1; then
  echo "systemd is required" >&2
  exit 1
fi

mkdir -p "$install_dir"
chown root:root "$install_dir"
chmod 750 "$install_dir"

if [ ! -f "$install_dir/env" ]; then
  install -m 600 -o root -g root /dev/null "$install_dir/env"
else
  chown root:root "$install_dir/env"
  chmod 600 "$install_dir/env"
fi

if [ ! -f "$install_dir/config.toml" ]; then
  echo "missing $install_dir/config.toml" >&2
  exit 1
fi
chown root:root "$install_dir/config.toml"
chmod 600 "$install_dir/config.toml"

if [ ! -x "$binary_path" ]; then
  echo "missing executable $binary_path" >&2
  exit 1
fi

install -m 644 "$(dirname "$0")/../systemd/distributed-watchdog.service" "$service_path"
mkdir -p "$override_dir"
cat >"$override_path" <<'EOF'
[Service]
# Run as root so the watchdog can perform explicitly enabled power operations.
User=root
Group=root
NoNewPrivileges=false
EOF
chmod 644 "$override_path"
systemctl daemon-reload
systemctl enable --now distributed-watchdog.service
systemctl status distributed-watchdog.service --no-pager
