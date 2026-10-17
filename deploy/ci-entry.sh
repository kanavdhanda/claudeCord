#!/usr/bin/env bash
# The ONLY thing the CI deploy key may run on the server. It is the forced command in the deploy user's ~/.ssh/authorized_keys:
#
#   restrict,command="/usr/local/bin/claudecord-ci" ssh-ed25519 AAAA... gh-deploy
#
# `restrict` turns off shells, port forwarding, agent forwarding and TTYs, and `command=` ignores whatever the client asked for, except that
# the request is passed in $SSH_ORIGINAL_COMMAND, where this script accepts only the four actions below. So a stolen key can deploy a commit
# that is already on GitHub, and nothing else: no shell, no arbitrary command.
#
# INSTALL it where the deploy user cannot change it (a copy owned by root, not the one in the clone, which deploy can write):
#   sudo install -m 755 -o root -g root /opt/claudeCord/deploy/ci-entry.sh /usr/local/bin/claudecord-ci
# and install it again after changing it here.
set -euo pipefail
DIR=/opt/claudeCord
cd "$DIR"

read -r -a req <<< "${SSH_ORIGINAL_COMMAND:-}"
action=${req[0]:-}
arg=${req[1]:-}

case "$action" in
  head)       # the commit now running
    git rev-parse HEAD ;;
  deploy)     # deploy SHA: the running image is kept as claudecord:previous and its commit in .previous, so `rollback` can put it back
    [[ $arg =~ ^[0-9a-f]{40}$ ]] || { echo "deploy needs a full 40-character commit id" >&2; exit 2; }
    git rev-parse HEAD > .previous
    docker tag claudecord:local claudecord:previous 2>/dev/null || true
    git fetch --tags origin
    git checkout --detach "$arg"
    cd deploy && docker compose up -d --build ;;
  logs)       # why a new version did not become ready
    cd deploy && docker compose logs --tail 40 hub 2>&1 ;;
  rollback)
    git checkout --detach "$(cat .previous)"
    docker tag claudecord:previous claudecord:local
    cd deploy && docker compose up -d --no-build ;;
  *)
    echo "not allowed: '${action}'" >&2; exit 2 ;;
esac
