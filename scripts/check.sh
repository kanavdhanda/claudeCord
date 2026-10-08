#!/bin/sh
# One command that says whether the Rust code is healthy. With --fix it repairs what can be repaired by itself
# (formatting, simple lint fixes, the generated code map) and then checks everything. With --cost it also runs the
# real-model cost benchmarks, which spend tokens and are therefore never run by default.
#
#   scripts/check.sh            check only, change nothing
#   scripts/check.sh --fix      repair formatting, lint suggestions and the code map, then check
#   scripts/check.sh --cost     also compare cost with the saved baseline (needs the claude command)
set -e
cd "$(dirname "$0")/.."
PATH="$HOME/.cargo/bin:$PATH"
FIX=0; COST=0
for a in "$@"; do case "$a" in --fix) FIX=1;; --cost) COST=1;; esac; done

if [ "$FIX" = 1 ]; then
  echo "== fixing"
  cargo fmt
  cargo clippy --fix --allow-dirty --allow-staged --all-targets -q 2>/dev/null || true
  cargo fmt
  python3 scripts/codemap.py
fi

echo "== formatting";  cargo fmt --check
echo "== lints";       cargo clippy --all-targets -- -D warnings
echo "== tests";       cargo test
echo "== code map";    python3 scripts/codemap.py --check

# Health checks come last: they prove every feature is alive, in this process and in the shipped binary.
echo "== features alive (selftest)"; cargo run -q --bin claudecord-hub -- selftest
echo "== shipped binary (smoke)";     scripts/smoke.sh

if [ "$COST" = 1 ]; then
  echo "== cost (spends tokens)"
  python3 scripts/cost/benchmark.py --model sonnet --check
  python3 scripts/cost/team_benchmark.py --model sonnet --check
fi
echo "all good"
