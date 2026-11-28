#!/usr/bin/env bash
set -euo pipefail

web_dir=$(cd "$(dirname "$0")/../web" && pwd)
if [[ ! -f "$web_dir/dist/index.html" ]]; then
  echo 'Build the console first with npm --prefix web run build.' >&2
  exit 1
fi

scratch=$(mktemp -d)
server_pid=''
cleanup() {
  if [[ -n "$server_pid" ]]; then
    kill "$server_pid" 2>/dev/null || true
    wait "$server_pid" 2>/dev/null || true
  fi
  rm -rf -- "$scratch"
}
trap cleanup EXIT

ln -s "$web_dir/dist" "$scratch/console"
python3 -u -m http.server 0 --bind 127.0.0.1 --directory "$scratch" > "$scratch/server.log" 2>&1 &
server_pid=$!

for _ in {1..50}; do
  port=$(sed -nE 's/^Serving HTTP on 127\.0\.0\.1 port ([0-9]+) .*/\1/p' "$scratch/server.log")
  if [[ -n "$port" ]]; then
    break
  fi
  kill -0 "$server_pid" 2>/dev/null || break
  sleep 0.1
done
if [[ -z "$port" ]]; then
  echo 'The console test server failed to start.' >&2
  cat "$scratch/server.log" >&2
  exit 1
fi

RBH_CONSOLE_TEST_URL="http://127.0.0.1:$port" \
  npm --prefix "$web_dir" run test:e2e -- --grep-invert '@daemon' --workers=2 "$@"
