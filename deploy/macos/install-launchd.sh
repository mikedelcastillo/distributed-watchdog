#!/usr/bin/env sh
set -eu

install_dir="${INSTALL_DIR:-/usr/local/etc/distributed-watchdog}"
binary_path="${BINARY_PATH:-/usr/local/bin/distributed-watchdog}"
plist_path="${PLIST_PATH:-/Library/LaunchDaemons/com.example.distributed-watchdog.plist}"
label="com.example.distributed-watchdog"
script_dir=$(CDPATH= cd "$(dirname "$0")" && pwd)
template_path="$script_dir/com.example.distributed-watchdog.system.plist"

if [ "$(id -u)" -ne 0 ]; then
  echo "this installer must be run as root (for example: sudo $0)" >&2
  exit 1
fi

if [ "$(uname -s)" != "Darwin" ]; then
  echo "this installer requires macOS launchd" >&2
  exit 1
fi

if [ "$install_dir" != "/usr/local/etc/distributed-watchdog" ] || \
  [ "$binary_path" != "/usr/local/bin/distributed-watchdog" ] || \
  [ "$plist_path" != "/Library/LaunchDaemons/com.example.distributed-watchdog.plist" ]; then
  echo "custom install paths are not supported by the bundled LaunchDaemon plist" >&2
  exit 1
fi

if [ ! -x "$binary_path" ]; then
  echo "missing executable $binary_path" >&2
  exit 1
fi

if [ ! -f "$install_dir/config.toml" ]; then
  echo "missing $install_dir/config.toml" >&2
  exit 1
fi

mkdir -p "$install_dir" /usr/local/var/log
chown root:wheel "$install_dir" /usr/local/var/log
chmod 750 "$install_dir"
install -m 600 -o root -g wheel /dev/null /usr/local/var/log/distributed-watchdog.log
install -m 600 -o root -g wheel /dev/null /usr/local/var/log/distributed-watchdog.err.log

if [ ! -f "$install_dir/.env" ]; then
  install -m 600 -o root -g wheel /dev/null "$install_dir/.env"
else
  chown root:wheel "$install_dir/.env"
  chmod 600 "$install_dir/.env"
fi

chown root:wheel "$install_dir/config.toml"
chmod 600 "$install_dir/config.toml"

plutil -lint "$template_path" >/dev/null
if launchctl print "system/$label" >/dev/null 2>&1; then
  launchctl bootout "system/$label"
fi
install -m 600 -o root -g wheel "$template_path" "$plist_path"
launchctl bootstrap system "$plist_path"
launchctl enable "system/$label"
launchctl kickstart -k "system/$label"
launchctl print "system/$label"
