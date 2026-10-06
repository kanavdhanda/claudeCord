#!/usr/bin/env bash
# Publishes the packages made by scripts/package_npm.py: every platform package first, the main package last, so nobody can install a main package whose platform package is not there yet. Needs npm to be logged in (`npm login`) or NODE_AUTH_TOKEN set.
# A package that is already on npm at this version is skipped, so a run that stopped half way can simply be run again.
#   scripts/publish_npm.sh OUT_DIR [--dry-run]
set -euo pipefail
out=${1:?out dir}; shift || true
publish() {
  local id
  id=$(cd "$1" && node -p "require('./package.json').name + '@' + require('./package.json').version")
  if npm view "$id" version >/dev/null 2>&1; then
    echo "$id is already published, skipping"
  else
    (cd "$1" && npm publish --access public "${@:2}")
  fi
}
for d in "$out"/claudecord-*; do publish "$d" "$@"; done
publish "$out/claudecord" "$@"
