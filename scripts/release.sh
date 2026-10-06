#!/usr/bin/env bash
# Ships a version: bumps it in Cargo.toml (the one place pip and npm read it from), commits, tags and pushes. The tag starts
# .github/workflows/release.yml, which builds every platform and publishes to npm (and PyPI when set up) and makes the GitHub release.
#   scripts/release.sh 0.2.1
# It refuses unless: you are on main with nothing uncommitted, main is the same as GitHub's, the version is newer than the current one and
# not tagged, and the newest tests on main passed in GitHub Actions and nothing but documentation changed since.
set -euo pipefail
cd "$(dirname "$0")/.."
v=${1:?usage: scripts/release.sh X.Y.Z}
[[ $v =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || { echo "the version looks like 0.2.1, not '$v'" >&2; exit 2; }
cur=$(sed -n 's/^version *= *"\(.*\)"/\1/p' Cargo.toml | head -1)
[ "$(printf '%s\n%s\n' "$cur" "$v" | sort -V | tail -1)" = "$v" ] && [ "$cur" != "$v" ] || { echo "$v is not newer than the current version $cur" >&2; exit 1; }
[ "$(git branch --show-current)" = main ] || { echo "release from main" >&2; exit 1; }
[ -z "$(git status --porcelain)" ] || { echo "commit or stash your changes first" >&2; exit 1; }
git fetch -q origin main --tags
[ "$(git rev-parse HEAD)" = "$(git rev-parse origin/main)" ] || { echo "main is not the same as GitHub's: pull or push first" >&2; exit 1; }
! git rev-parse -q --verify "refs/tags/v$v" >/dev/null || { echo "v$v already exists" >&2; exit 1; }
# The tests skip changes to documentation only, so the check is the newest test run on main, plus: nothing but documentation changed since it.
read -r state sha < <(gh run list --workflow rust --branch main --limit 1 --json status,conclusion,headSha --jq '.[0] | "\(.status)-\(.conclusion) \(.headSha)"' 2>/dev/null || true)
[ "${state:-}" = "completed-success" ] || { echo "the newest test run on main is '${state:-not found}', not passing: wait for it (gh run watch)" >&2; exit 1; }
git diff --quiet "$sha" HEAD -- . ':(exclude)*.md' ':(exclude)site' ':(exclude)learnings' ':(exclude)deploy' || { echo "code changed after the last passing test run ($sha): wait for the tests on the latest commit" >&2; exit 1; }
perl -pi -e 'if (!$d && s/^version = ".*"/version = "'"$v"'"/) { $d = 1 }' Cargo.toml
cargo check -q   # brings Cargo.lock to the new version
git add Cargo.toml Cargo.lock
git commit -q -m "Release v$v"
git tag "v$v"
git push origin main "v$v"
echo "pushed v$v. Watch it: gh run watch \$(gh run list --workflow release --limit 1 --json databaseId --jq '.[0].databaseId')"
