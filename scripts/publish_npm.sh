#!/usr/bin/env bash
# Publishes the packages made by scripts/package_npm.py: every platform package first, the main package last, so nobody can
# install a main package whose platform package is not there yet. Needs npm to be logged in (`npm login`) or NODE_AUTH_TOKEN set.
#   scripts/publish_npm.sh OUT_DIR [--dry-run]
set -euo pipefail
out=${1:?out dir}; shift || true
for d in "$out"/claudecord-*; do (cd "$d" && npm publish --access public "$@"); done
(cd "$out/claudecord" && npm publish --access public "$@")
