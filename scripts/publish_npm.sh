#!/usr/bin/env bash
# Publishes the one package made by scripts/package_npm.py (`claudecord`, holding every platform's program). Needs npm to be logged in
# (`npm login`) or NODE_AUTH_TOKEN set. A version already on npm is skipped, so running it again after a stop is safe.
#   scripts/publish_npm.sh OUT_DIR [--dry-run]
set -euo pipefail
out=${1:?out dir}; shift || true
id=$(cd "$out/claudecord" && node -p "require('./package.json').name + '@' + require('./package.json').version")
if npm view "$id" version >/dev/null 2>&1; then
  echo "$id is already published, skipping"
else
  (cd "$out/claudecord" && npm publish --access public "$@")
fi
