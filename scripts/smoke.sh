#!/bin/sh
# End-to-end smoke test of the real binary, the way a person would use it: make a token, start the hub, log a machine in,
# and run the connection check. Proves the pieces are wired together in the shipped program, not only in the tests.
# Used by CI after the unit tests, and by scripts/check.sh.
set -e
cd "$(dirname "$0")/.."
PATH="$HOME/.cargo/bin:$PATH"
cargo build -q
BIN="$PWD/target/debug/claudecord"          # the machine side
HUB="$PWD/target/debug/claudecord-hub"      # the hub side
WORK=$(mktemp -d)
trap '{ kill $HUB_PID; wait $HUB_PID; } 2>/dev/null || true; rm -rf "$WORK"' EXIT
export CLAUDECORD_HOME="$WORK/home"

TOKEN=$("$HUB" token mac --data "$WORK/hub" 2>/dev/null)
[ -n "$TOKEN" ] || { echo "no token made"; exit 1; }

# Pick a free port.
PORT=$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1])')
"$HUB" hub --data "$WORK/hub" --bind "127.0.0.1:$PORT" --owner 1 >"$WORK/hub.log" 2>&1 &
HUB_PID=$!
for i in 1 2 3 4 5 6 7 8 9 10; do
  curl -fs "http://127.0.0.1:$PORT/healthz" >/dev/null 2>&1 && break
  sleep 0.3
done
[ "$(curl -fs "http://127.0.0.1:$PORT/healthz")" = "ok" ] || { echo "hub did not come up"; cat "$WORK/hub.log"; exit 1; }
echo "hub is up on port $PORT"

# Refuses a public bind without TLS.
if "$HUB" hub --data "$WORK/x" --bind "0.0.0.0:0" 2>/dev/null; then echo "hub accepted a public bind without TLS"; exit 1; fi
echo "refuses a public address without TLS"

"$BIN" login --hub "ws://127.0.0.1:$PORT" --token "$TOKEN" --name mac
"$BIN" doctor
echo "doctor passed"

# A wrong token is refused and named.
"$BIN" login --hub "ws://127.0.0.1:$PORT" --token wrong --name mac >/dev/null
if "$BIN" doctor >"$WORK/bad.txt" 2>&1; then echo "doctor passed with a wrong token"; exit 1; fi
grep -q "401" "$WORK/bad.txt" || { echo "doctor did not name the refusal"; cat "$WORK/bad.txt"; exit 1; }
echo "wrong token refused and named"
echo "smoke test passed"
