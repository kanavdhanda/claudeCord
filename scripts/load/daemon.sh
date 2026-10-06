#!/usr/bin/env bash
# What one machine's daemon costs with many agents: starts a hub and ONE daemon (private tmux socket, private CLAUDECORD_HOME) and adds
# agents running `cat` in steps, then samples the daemon's CPU, memory, threads and the tmux server's CPU at each step.
#   scripts/load/daemon.sh [STEP] [STEPS] [SETTLE_SECONDS]       e.g. 10 6 20 for 10, 20, ... 60 agents
# Every process and folder it makes is under /tmp/ccl.* and is removed at the end (the tmux server is stopped through its own socket).
set -euo pipefail
step=${1:-10}; steps=${2:-6}; settle=${3:-20}
bin=$(cd "$(dirname "${CLAUDECORD_BIN:-target/release/claudecord}")" && pwd)/$(basename "${CLAUDECORD_BIN:-target/release/claudecord}")
work=$(mktemp -d /tmp/ccl.XXXX); hub=""; sock="ccl-$$-$RANDOM"
export CLAUDECORD_HOME="$work/h" CLAUDECORD_TMUX_SOCKET="$sock" CLAUDECORD_MAX_AGENTS=1000
cleanup() {
  "$bin" down >/dev/null 2>&1 || true
  tmux -L "$sock" kill-server >/dev/null 2>&1 || true
  [ -n "$hub" ] && kill "$hub" 2>/dev/null || true
  wait 2>/dev/null || true; rm -rf "$work"
}
trap cleanup EXIT
mkdir -p "$work/h"
port=$((20000 + RANDOM % 20000))
"$bin" hub --data "$work/hub" --bind "127.0.0.1:$port" >"$work/hub.log" 2>&1 & hub=$!
for _ in $(seq 40); do curl -fs "http://127.0.0.1:$port/readyz" >/dev/null && break; sleep 0.25; done
token=$("$bin" token m1 --data "$work/hub" 2>/dev/null | tail -1)
"$bin" login --hub "http://127.0.0.1:$port" --token "$token" --name m1 >/dev/null
printf 'agents\tdaemon_cpu%%\tdaemon_rss_mb\tdaemon_threads\ttmux_cpu%%\n'
total=0
for s in $(seq "$steps"); do
  for _ in $(seq "$step"); do
    total=$((total + 1)); mkdir -p "$work/f$total"
    (cd "$work/f$total" && "$bin" start --project load --name "a$total" --detach --no-guide -- cat >/dev/null 2>&1) || { echo "start $total failed"; break 2; }
  done
  sleep "$settle"
  # the daemon of THIS run is whoever holds this run's socket, and its tmux server answers on this run's own socket: never found by name
  dpid=$(lsof -t "$work/h/daemon.sock" 2>/dev/null | head -1)
  tpid=$(tmux -L "$sock" display-message -p '#{pid}' 2>/dev/null || true)
  a=$(ps -o %cpu=,rss= -p "$dpid" 2>/dev/null | awk '{printf "%s\t%d", $1, $2/1024}')
  t=$(ps -o %cpu= -p "$tpid" 2>/dev/null | tr -d ' ')
  th=$(ps -M "$dpid" 2>/dev/null | tail -n +2 | wc -l | tr -d ' ')
  echo "$total	$a	$th	${t:-0}"
done
