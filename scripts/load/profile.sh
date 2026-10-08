#!/usr/bin/env bash
# Runs the k6 load test against a hub limited to the size of a free server, so the result says what that server can carry.
#   scripts/load/profile.sh PROFILE USERS [HOLD_SECONDS]
# Profiles (limits are what the provider's free tier gives; k6 shares the machine, so real numbers are slightly better):
#   oracle-micro   0.25 CPU, 1 GB   Oracle Always Free AMD micro (1/8 OCPU burst)
#   small-vps      1 CPU, 512 MB    the cheapest paid VPS and most free containers
#   oracle-arm     4 CPU, 12 GB     Oracle Always Free Ampere A1 (up to 4 OCPU / 24 GB; 12 GB here so a CI runner fits)
# Needs docker, k6 and a built image: docker build -t claudecord .
set -euo pipefail
profile=${1:?profile}; users=${2:?users}; hold=${3:-60}
case $profile in
  oracle-micro) cpus=0.25; mem=1g ;;
  small-vps)    cpus=1;    mem=512m ;;
  oracle-arm)   cpus=4;    mem=12g ;;
  *) echo "unknown profile $profile" >&2; exit 2 ;;
esac
work=$(mktemp -d); trap 'docker rm -f cc-load >/dev/null 2>&1 || true; rm -rf "$work"' EXIT
mkdir -p "$work/data"; chmod 777 "$work/data"
# Tokens are made with the same image so the hub's database and the token file agree.
docker run --rm -v "$work/data:/data" --entrypoint /claudecord-hub claudecord \
  load-tokens --count "$users" --out /data/tokens.json --data /data >/dev/null
docker run -d --name cc-load --cpus "$cpus" --memory "$mem" -p 18787:8787 -v "$work/data:/data" claudecord >/dev/null
for _ in $(seq 30); do curl -fs http://127.0.0.1:18787/healthz >/dev/null && break; sleep 1; done
ulimit -n 65535 2>/dev/null || true
set +e
k6 run -q --summary-export "$work/summary.json" -e HUB=ws://127.0.0.1:18787 -e USERS="$users" \
  -e RAMP="$((users / 100 + 30))" -e HOLD="$hold" -e TOKENS="$work/data/tokens.json" scripts/load/hub.js
status=$?
echo "== hub resource use at the end ($profile, $users users x 3 bots)"
docker stats --no-stream --format 'cpu {{.CPUPerc}}  memory {{.MemUsage}}' cc-load
cp "$work/summary.json" "${SUMMARY_OUT:-/dev/null}" 2>/dev/null || true
exit $status
