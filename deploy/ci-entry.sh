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
#
# A deploy is staged: the new version is built and tested as a candidate (claudecord:staging) while the running hub is left alone, and
# only a candidate that passes replaces it. Exit codes tell the workflow what happened: 10 = the candidate failed and the running hub was
# never touched (nothing to roll back); any other failure came after the swap began, so the workflow runs `rollback`.
set -euo pipefail
DIR=/opt/claudeCord
cd "$DIR"

read -r -a req <<< "${SSH_ORIGINAL_COMMAND:-}"
action=${req[0]:-}
arg=${req[1]:-}

staging_failed() { echo "ERROR: $1. The running hub was not touched." >&2; exit 10; }

case "$action" in
  head)       # the commit now running
    git rev-parse HEAD ;;

  deploy)     # deploy SHA
    [[ $arg =~ ^[0-9a-f]{40}$ ]] || { echo "deploy needs a full 40-character commit id" >&2; exit 2; }

    echo "==> 1. Build the candidate in a separate worktree"
    git fetch --tags origin
    git worktree prune 2>/dev/null || true
    rm -rf .staging_build
    git worktree add --detach .staging_build "$arg"
    trap 'cd "$DIR"; git worktree remove --force .staging_build 2>/dev/null || rm -rf .staging_build; git worktree prune 2>/dev/null || true; docker rm -f claudecord-staging >/dev/null 2>&1 || true' EXIT
    (cd .staging_build && docker build -t claudecord:staging -f Dockerfile .) || staging_failed "the candidate did not build"

    echo "==> 2. Test the candidate"
    docker run --rm --tmpfs /tmp claudecord:staging selftest || staging_failed "the candidate's selftest failed"
    docker rm -f claudecord-staging >/dev/null 2>&1 || true
    docker run -d --name claudecord-staging --read-only --tmpfs /tmp --tmpfs /data claudecord:staging \
      serve --data=/data --bind=0.0.0.0:8787 --allow-plain --public-url=http://127.0.0.1:8787 \
            --client-id=staging-verify --secret-file=/etc/hosts >/dev/null || staging_failed "the candidate would not start"
    ready=false
    for i in $(seq 1 15); do
      if docker exec claudecord-staging /claudecord-hub probe http://127.0.0.1:8787 --once --data /tmp/probe >/dev/null 2>&1; then
        ready=true; echo "candidate ready after $i checks"; break
      fi
      sleep 1
    done
    if [ "$ready" != true ]; then
      docker logs --tail 50 claudecord-staging 2>&1 || true
      staging_failed "the candidate never became ready"
    fi
    docker rm -f claudecord-staging >/dev/null 2>&1 || true

    echo "==> 3. Swap it in (the previous image is kept as claudecord:previous, its commit in .previous)"
    git rev-parse HEAD > .previous
    docker tag claudecord:local claudecord:previous 2>/dev/null || true
    docker tag claudecord:staging claudecord:local
    git checkout --detach "$arg"
    cd deploy && docker compose up -d --no-build ;;

  logs)       # why a new version did not become ready
    cd deploy && docker compose logs --tail 40 hub 2>&1 ;;

  rollback)
    [ -f .previous ] || { echo "nothing to roll back to" >&2; exit 1; }
    git checkout --detach "$(cat .previous)"
    docker image inspect claudecord:previous >/dev/null 2>&1 && docker tag claudecord:previous claudecord:local
    cd deploy && docker compose up -d --no-build ;;

  *)
    echo "not allowed: '${action}'" >&2; exit 2 ;;
esac
