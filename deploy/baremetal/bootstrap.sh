#!/usr/bin/env bash
# Sets up a claudeCord hub on a fresh Linux machine that uses systemd (Ubuntu or Debian, such as an Oracle Cloud Always Free VM), in one go:
# the program, a service that restarts it (and restarts a hung one), TLS from Let's Encrypt through Caddy, and the firewall openings.
#
#   sudo ./bootstrap.sh --domain claudecord.example.com --client-id DISCORD_APP_ID --secret-file secret.txt [--binary ./claudecord | --version v0.2.0]
#
# This sets up the hosted service: people sign in with Discord, add their own bots on the dashboard, and join machines by a code. The
# Discord application named by --client-id is only for signing people in (create it in the Discord developer portal and add
# https://YOUR-DOMAIN/auth/callback under OAuth2 > Redirects). Their bots are separate and are saved on the dashboard, encrypted with a
# key the program makes on first start in /var/lib/claudecord/kek. BACK THAT FILE UP: without it every saved bot token is lost.
#
# Before running it: the domain's A record must already point at this machine (Caddy asks Let's Encrypt for a certificate as soon as it starts),
# and ports 80 and 443 must be open in the provider's network settings (on Oracle: the VCN security list). It can be run again safely.
# Without --binary it downloads the release file for this CPU from GitHub (so a release must exist); with --binary it uses the file you give.
#
# NOTE: written carefully but not yet run on a real server. Run it on a throwaway machine first, or read it line by line.
set -euo pipefail

domain=""; client=""; secret=""; binary=""; version="latest"
while [ $# -gt 0 ]; do
  case "$1" in
    --domain) domain=${2:?}; shift 2 ;;
    --client-id) client=${2:?}; shift 2 ;;
    --secret-file) secret=${2:?}; shift 2 ;;
    --binary) binary=${2:?}; shift 2 ;;
    --version) version=${2:?}; shift 2 ;;
    *) echo "unknown option $1" >&2; exit 2 ;;
  esac
done
[ -n "$domain" ] && [ -n "$client" ] && [ -n "$secret" ] || { echo "usage: $0 --domain NAME --client-id DISCORD_APP_ID --secret-file FILE [--binary FILE | --version TAG]" >&2; exit 2; }
[ -f "$secret" ] || { echo "$secret is not a file" >&2; exit 2; }
[ "$(id -u)" = 0 ] || { echo "run this as root (sudo)" >&2; exit 1; }
command -v systemctl >/dev/null || { echo "this needs a machine that uses systemd" >&2; exit 1; }
# The application id ends up in a service file and a command line, so only digits are accepted.
case "$client" in *[!0-9]*|"") echo "--client-id is the Discord application id: digits only" >&2; exit 2 ;; esac
case "$domain" in *[!A-Za-z0-9.-]*|"") echo "--domain looks wrong: $domain" >&2; exit 2 ;; esac

# 1. The program.
if [ -z "$binary" ]; then
  case "$(uname -m)" in
    x86_64) arch=x64 ;;
    aarch64|arm64) arch=arm64 ;;
    *) echo "no build for $(uname -m)" >&2; exit 1 ;;
  esac
  url="https://github.com/kanavdhanda/claudeCord/releases/latest/download/claudecord-linux-$arch"
  [ "$version" = latest ] || url="https://github.com/kanavdhanda/claudeCord/releases/download/$version/claudecord-linux-$arch"
  echo "downloading $url"
  curl -fsSL -o /tmp/claudecord.new "$url" || { echo "could not download it: is there a published release? Use --binary FILE instead." >&2; exit 1; }
  binary=/tmp/claudecord.new
fi
[ -f "$binary" ] || { echo "$binary is not a file" >&2; exit 1; }
[ -f /usr/local/bin/claudecord ] && cp -f /usr/local/bin/claudecord /usr/local/bin/claudecord.previous
install -m 755 "$binary" /usr/local/bin/claudecord
/usr/local/bin/claudecord --help >/dev/null || { echo "the program does not run on this machine" >&2; exit 1; }

# 2. A user of its own, and a folder only it can use.
id claudecord >/dev/null 2>&1 || useradd --system --create-home --shell /usr/sbin/nologin claudecord
install -d -m 750 -o claudecord -g claudecord /var/lib/claudecord
install -m 600 -o claudecord -g claudecord "$secret" /var/lib/claudecord/oauth-secret

# 3. The service: starts at boot, restarts if it dies, and restarts if its core hangs (it tells systemd it is alive every few seconds).
cat > /etc/systemd/system/claudecord-hub.service <<UNIT
[Unit]
Description=claudeCord hub
After=network-online.target
Wants=network-online.target
StartLimitIntervalSec=0

[Service]
Type=notify
NotifyAccess=main
WatchdogSec=30
TimeoutStopSec=30
User=claudecord
ExecStart=/usr/local/bin/claudecord serve --data /var/lib/claudecord --bind 127.0.0.1:8787 --public-url https://$domain --client-id $client --secret-file /var/lib/claudecord/oauth-secret
Restart=always
RestartSec=2
NoNewPrivileges=true
ProtectSystem=strict
ReadWritePaths=/var/lib/claudecord
PrivateTmp=true
LimitNOFILE=65535

[Install]
WantedBy=multi-user.target
UNIT
systemctl daemon-reload
systemctl enable claudecord-hub >/dev/null
systemctl restart claudecord-hub

# 4. Oracle's Ubuntu images block ports in the machine's own firewall as well as in the network settings.
if command -v iptables >/dev/null; then
  for port in 80 443; do
    iptables -C INPUT -p tcp --dport "$port" -j ACCEPT 2>/dev/null || iptables -I INPUT -p tcp --dport "$port" -j ACCEPT
  done
  command -v netfilter-persistent >/dev/null && netfilter-persistent save >/dev/null || true
fi

# 5. TLS and the public address: Caddy gets and renews a certificate by itself and passes everything on to the hub.
if ! command -v caddy >/dev/null; then
  apt-get update -qq
  apt-get install -y -qq debian-keyring debian-archive-keyring apt-transport-https curl gpg
  curl -fsSL https://dl.cloudsmith.io/public/caddy/stable/gpg.key | gpg --dearmor -o /usr/share/keyrings/caddy-stable-archive-keyring.gpg
  curl -fsSL https://dl.cloudsmith.io/public/caddy/stable/debian.deb.txt > /etc/apt/sources.list.d/caddy-stable.list
  apt-get update -qq
  apt-get install -y -qq caddy
fi
cat > /etc/caddy/Caddyfile <<CADDY
$domain {
	reverse_proxy 127.0.0.1:8787
}
CADDY
systemctl enable caddy >/dev/null
systemctl reload caddy 2>/dev/null || systemctl restart caddy

# 6. Is it up?
for _ in $(seq 1 30); do
  code=$(curl -s -o /dev/null -w '%{http_code}' --max-time 5 http://127.0.0.1:8787/healthz || true)
  [ "$code" = 200 ] && break
  sleep 1
done
[ "${code:-}" = 200 ] || { echo "the hub did not come up; see: journalctl -u claudecord-hub -n 50" >&2; exit 1; }
echo
echo "The service is running. Check from outside in a minute (the certificate is being fetched):  curl -s https://$domain/readyz"
echo "Open https://$domain, sign in with Discord, and the dashboard walks you through adding a bot and a project."
echo "BACK UP /var/lib/claudecord/kek (it seals every saved bot token) and /var/lib/claudecord/control.db."
echo "On a machine: npx claudecord  (it opens a browser to approve the machine)."
