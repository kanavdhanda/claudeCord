#!/usr/bin/env bash
# A small, cheap run of the k6 load test, fit for every push: a few hundred users with three bots each for under a minute. It
# fails when connects fail, the hub is slow to answer or messages are lost (the thresholds in hub.js). The big runs are load.yml.
#   scripts/load/smoke.sh [USERS] [PATH_TO_CLAUDECORD]
set -euo pipefail
users=${1:-200}; bin=${2:-target/release/claudecord}
work=$(mktemp -d); trap 'kill $hub 2>/dev/null || true; rm -rf "$work"' EXIT
"$bin" load-tokens --count "$users" --out "$work/tokens.json" --data "$work/hub" 2>/dev/null
port=$((20000 + RANDOM % 20000))
"$bin" hub --data "$work/hub" --bind "127.0.0.1:$port" >"$work/hub.log" 2>&1 & hub=$!
for _ in $(seq 30); do curl -fs "http://127.0.0.1:$port/readyz" >/dev/null && break; sleep 0.5; done
ulimit -n 8192 2>/dev/null || true
k6 run -q -e HUB="ws://127.0.0.1:$port" -e USERS="$users" -e RAMP=10 -e HOLD=20 -e TOKENS="$work/tokens.json" scripts/load/hub.js
