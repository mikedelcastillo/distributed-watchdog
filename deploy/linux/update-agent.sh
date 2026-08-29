#!/usr/bin/env sh
set -eu

repo_url="${1:-${REPO_URL:-}}"
branch="${2:-${BRANCH:-main}}"
install_dir="${INSTALL_DIR:-$PWD}"
source_dir="${SOURCE_DIR:-$install_dir/source}"
binary_path="${BINARY_PATH:-$install_dir/distributed-watchdog}"
log_path="${UPDATE_LOG_PATH:-$install_dir/update.log}"
lock_dir="${UPDATE_LOCK_DIR:-$install_dir/.update.lock}"
stale_lock_seconds="${UPDATE_STALE_LOCK_SECONDS:-7200}"

if [ -z "$repo_url" ]; then
  echo "repo URL is required" >&2
  exit 1
fi

install_real="$(readlink -f "$install_dir")"
source_parent="$(dirname "$source_dir")"
mkdir -p "$source_parent"
source_parent_real="$(readlink -f "$source_parent")"
case "$source_parent_real" in
  "$install_real"|"$install_real"/*) ;;
  *)
    echo "SOURCE_DIR must be inside INSTALL_DIR" >&2
    exit 1
    ;;
esac

acquire_update_lock() {
  if mkdir "$lock_dir" 2>/dev/null; then
    echo "$$" >"$lock_dir/pid"
    return 0
  fi

  pid="$(cat "$lock_dir/pid" 2>/dev/null || true)"
  if [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null; then
    return 1
  fi

  now="$(date +%s)"
  lock_mtime="$(date -r "$lock_dir" +%s 2>/dev/null || echo 0)"
  age=$((now - lock_mtime))
  if [ "$age" -lt "$stale_lock_seconds" ]; then
    return 1
  fi

  rm -rf "$lock_dir"
  mkdir "$lock_dir"
  echo "$$" >"$lock_dir/pid"
}

if [ "${DW_UPDATE_CHILD:-0}" != "1" ]; then
  if ! acquire_update_lock; then
    echo "update already running"
    exit 0
  fi
  DW_UPDATE_CHILD=1 DW_UPDATE_LOCK_HELD=1 nohup "$0" "$repo_url" "$branch" >"$log_path" 2>&1 </dev/null &
  echo "update scheduled"
  exit 0
fi

if [ "${DW_UPDATE_LOCK_HELD:-0}" = "1" ]; then
  echo "$$" >"$lock_dir/pid"
else
  if ! acquire_update_lock; then
    echo "update already running"
    exit 0
  fi
fi
trap 'rm -rf "$lock_dir"' EXIT INT TERM

sleep "${DW_UPDATE_DELAY_SECONDS:-2}"

if [ -d "$source_dir/.git" ]; then
  actual_url="$(git -C "$source_dir" remote get-url origin)"
  if [ "$actual_url" != "$repo_url" ]; then
    echo "git remote URL mismatch" >&2
    exit 1
  fi
  git -C "$source_dir" fetch origin "$branch"
  local_head="$(git -C "$source_dir" rev-parse HEAD)"
  remote_head="$(git -C "$source_dir" rev-parse "origin/$branch")"
  if [ "$local_head" = "$remote_head" ]; then
    echo "already up to date"
    exit 0
  fi
  git -C "$source_dir" reset --hard "origin/$branch"
else
  rm -rf "$source_dir"
  git clone --branch "$branch" --single-branch "$repo_url" "$source_dir"
fi

if [ "${VERIFY_GIT_SIGNATURES:-0}" = "1" ]; then
  git -C "$source_dir" verify-commit HEAD
fi

cd "$source_dir"
if command -v cargo >/dev/null 2>&1; then
  cargo build --release
elif command -v docker >/dev/null 2>&1; then
  docker run --rm \
    -u "$(id -u):$(id -g)" \
    -e CARGO_HOME=/tmp/cargo \
    -v "$PWD":/src \
    -w /src \
    rust:1.98-bookworm cargo build --release
else
  echo "cargo or docker is required to build" >&2
  exit 1
fi

cp target/release/distributed-watchdog "$binary_path.new"
chmod +x "$binary_path.new"
mv "$binary_path.new" "$binary_path"

if [ -n "${SERVICE_NAME:-}" ] && command -v systemctl >/dev/null 2>&1; then
  systemctl restart "$SERVICE_NAME"
else
  cd "$install_dir"
  exec ./start-agent.sh
fi
