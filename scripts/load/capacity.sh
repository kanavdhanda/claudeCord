#!/usr/bin/env bash
# Capacity, leak and chaos runs against ONE hub process on this machine: starts a hub on a random port with a throw-away data folder, drives it
# with simulated machines (machines.mjs, Node 22+), and samples the hub's memory, open files and threads every few seconds.
#   scripts/load/capacity.sh MODE MACHINES [AGENTS_PER_MACHINE] [SECONDS] [SAMPLE_EVERY]
# MODE is one of machines.mjs's modes (hold, churn, agents, say, files), or:
#   storm    the hub is stopped (SIGTERM) and started again a third of the way in; every machine reconnects at once, with no pause
#   durable  machines send numbered messages; half way the hub gets $SIGNAL (default KILL, or TERM) and is started again. Every message the hub
#            had acknowledged must be in its database afterwards (the hub acks only after the write is on disk)
# Env: RATE (new connections per second), EXTRA (more machines.mjs flags, e.g. "--per-sec 20"), NOFILE, CLAUDECORD_BIN.
# Prints the driver's result line, then a table of seconds / RSS MB / footprint MB / open files / threads. A leak is RSS or files that keep
# climbing after warm-up and do not come back. Everything runs under /tmp/ccl.*; only the processes started here are signalled.
# Needs a release build (cargo build --release). On one address a machine runs out of local ports at about 16,000 connections.
set -euo pipefail
mode=${1:?mode}; n=${2:?machines}; agents=${3:-3}; secs=${4:-60}; every=${5:-5}
bin=${CLAUDECORD_HUB_BIN:-target/release/claudecord-hub}
here=$(cd "$(dirname "$0")" && pwd)
work=$(mktemp -d /tmp/ccl.XXXX); hub=""; sampler=""
cleanup() { [ -n "$sampler" ] && kill "$sampler" 2>/dev/null || true; [ -n "$hub" ] && kill "$hub" 2>/dev/null || true; wait 2>/dev/null || true; rm -rf "$work"; }
trap cleanup EXIT
ulimit -n "${NOFILE:-60000}" 2>/dev/null || ulimit -n 10240 || true
"$bin" load-tokens --count "$n" --out "$work/tokens.json" --data "$work/hub" 2>/dev/null
port=$((20000 + RANDOM % 20000))
start_hub() {
  "$bin" hub --data "$work/hub" --bind "127.0.0.1:$port" >>"$work/hub.log" 2>&1 & hub=$!
  for _ in $(seq 40); do curl -fs "http://127.0.0.1:$port/readyz" >/dev/null && break; sleep 0.25; done
}
start_hub
start=$(date +%s)
# footprint (macOS) counts pages the system compressed too, which RSS hides; it stays 0 elsewhere.
fp() { footprint -p "$hub" 2>/dev/null | awk '/Footprint:/ { for (i = 1; i <= NF; i++) if ($i == "Footprint:") { v = $(i+1); u = $(i+2) } if (u=="KB") v/=1024; if (u=="GB") v*=1024; printf "%d", v; f=1 } END { if (!f) printf 0 }'; }
sample() { echo "$(( $(date +%s) - start ))	$(( $(ps -o rss= -p "$hub" | tr -d ' ') / 1024 ))	$(fp)	$(lsof -p "$hub" 2>/dev/null | wc -l | tr -d ' ')	$(ps -M "$hub" | tail -n +2 | wc -l | tr -d ' ')"; }
( while true; do kill -0 "$hub" 2>/dev/null && sample >>"$work/samples.tsv"; sleep "$every"; done ) & sampler=$!
drive=$mode; [ "$mode" = durable ] && drive=say
node "$here/machines.mjs" ${EXTRA:-} --hub "ws://127.0.0.1:$port" --tokens "$work/tokens.json" --machines "$n" --agents "$agents" --secs "$secs" \
  --mode "$drive" --acked "$work/acked" ${RATE:+--rate $RATE} & driver=$!
case $mode in
  storm|durable)
    if [ "$mode" = storm ]; then sleep $(( secs / 3 )); sig=TERM; else sleep $(( secs / 2 )); sig=${SIGNAL:-KILL}; fi
    kill -"$sig" "$hub"; while kill -0 "$hub" 2>/dev/null; do sleep 0.1; done
    echo "hub stopped with SIG$sig"; start_hub ;;
esac
wait "$driver"
sample >>"$work/samples.tsv"
printf 'seconds\trss_mb\tfootprint_mb\topen_files\tthreads\n'; cat "$work/samples.tsv"
echo "hub ERROR/WARN/panic log lines: $(grep -cE ' (ERROR|WARN) |panicked' "$work/hub.log" || true)"
grep -E ' (ERROR|WARN) |panicked' "$work/hub.log" | cut -c1-160 | sed -E 's/[0-9]+/N/g' | sort | uniq -c | sort -rn | head -5 || true
if [ "$mode" = durable ]; then
  kill "$hub"; wait "$hub" 2>/dev/null || true
  acked=$(cat "$work/acked"); rows=$(sqlite3 "$work/hub/hub.db" "select count(*) from history where body like 'status %'")
  echo "acknowledged by the hub: $acked, in its database after the restart: $rows"
  [ "$rows" -ge "$acked" ] || { echo "LOST $((acked - rows)) ACKNOWLEDGED MESSAGES"; exit 1; }
fi
