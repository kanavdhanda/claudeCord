# Contributing

    scripts/check.sh           # format, lints, every test, a live check of every feature, then the shipped binary
    scripts/check.sh --fix     # repairs formatting, simple lints and the code map first

`CODEMAP.md` says what every file is. Each file begins with a comment saying what it is, and each function begins with a
comment saying what it does; keep it that way, and run `scripts/check.sh --fix` after changing a header (the map is generated).

Rules that keep it safe to change:

- The hub core (`src/hub`) does no I/O and reads no clock. Time is an argument, effects are returned. Keep it that way so it
  stays testable.
- Check every limit before allocating anything. Never trust a length that came off the wire.
- A new frame, limit or safety rule gets a golden vector in `testdata/conformance` and a test.
- Every feature has a probe in `src/health`, and `tests/it/health.rs` fails if a source file is not claimed by one. New module, new probe.
- A security branch gets a test that fails when the branch is removed.
- Cost: `scripts/check.sh --cost` runs the real-model benchmarks (they spend tokens) and fails if turns or tokens rise.

Commit messages say what changed and why, in the imperative.

## Cost and scale impact (every pull request)

Every pull request description says what the change does to **cost** and to **scale**, even when the answer is "none, because ...".
The pull request template (`.github/pull_request_template.md`) fills the description in for you with both headings. Nothing in CI
measures or enforces them; a reviewer reads them and can ask for numbers.

- **Cost impact**: does it change how many agent turns or tokens a message costs? Anything an agent reads (briefs, delivery, headers)
  or any change to batching, wake-ups or ride-along counts. How to check: `python3 scripts/cost/benchmark.py --model sonnet --check`
  and `scripts/cost/team_benchmark.py` against real Claude Code (they spend real tokens, so run them yourself, not CI), and compare
  with `scripts/cost/baseline.json`. A change that touches none of that writes "none" and says why.
- **Scale impact**: does it change how many users, machines or agents one hub can carry? Anything that adds work or memory per
  connection, per message or per agent (timers, database writes, queues held in memory). How to check: `scripts/load/smoke.sh`
  (200 users with three bots each, about a minute, locally), or the manual `load` workflow for the large runs at free-server sizes.
  Quote the numbers before and after.

Example of a filled description:

    Cost impact: none. Only the dashboard page changed; no agent reads it.
    Scale impact: adds one timer per connected machine (about 100 bytes each). scripts/load/smoke.sh p95 delivery 17 ms before, 18 ms after.

