#!/usr/bin/env sh
set -eu

install_dir="${INSTALL_DIR:-$PWD}"
binary_path="${BINARY_PATH:-$install_dir/distributed-watchdog}"
config_path="${CONFIG_PATH:-$install_dir/config.toml}"
pid_path="${PID_PATH:-$install_dir/distributed-watchdog.pid}"
log_path="${LOG_PATH:-$install_dir/distributed-watchdog.log}"

if [ ! -x "$binary_path" ]; then
  echo "missing executable $binary_path" >&2
  exit 1
fi

if [ ! -f "$config_path" ]; then
  echo "missing config $config_path" >&2
  exit 1
fi

if [ -f "$pid_path" ]; then
  old_pid="$(cat "$pid_path" 2>/dev/null || true)"
  if [ -n "$old_pid" ] && kill -0 "$old_pid" 2>/dev/null; then
    expected_exe="$(readlink -f "$binary_path")"
    actual_exe="$(readlink "/proc/$old_pid/exe" 2>/dev/null || true)"
    if [ "$actual_exe" = "$expected_exe" ] || [ "$actual_exe" = "$expected_exe (deleted)" ]; then
      kill "$old_pid" 2>/dev/null || true
      sleep 1
    else
      echo "refusing to kill pid $old_pid because it is not $expected_exe" >&2
      exit 1
    fi
  fi
fi

cd "$install_dir"
nohup "$binary_path" --config "$config_path" serve >>"$log_path" 2>&1 </dev/null &
echo "$!" >"$pid_path"
echo "started distributed-watchdog pid $(cat "$pid_path")"
