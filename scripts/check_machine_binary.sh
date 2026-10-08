#!/bin/sh
# The program people install must hold nothing of the hub: build it without the hub feature and look for what only the hub has.
set -e
cd "$(dirname "$0")/.."
cargo build -q --no-default-features --bin claudecord
for word in axum rusqlite control.db machine_tokens claudecord-hub; do
  if strings target/debug/claudecord | grep -q -- "$word"; then echo "the claudecord program contains hub code ($word)"; exit 1; fi
done
echo "claudecord holds no hub code"
