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
- Every feature has a probe in `src/health`, and `tests/health.rs` fails if a source file is not claimed by one. New module, new probe.
- A security branch gets a test that fails when the branch is removed.
- Cost: `scripts/check.sh --cost` runs the real-model benchmarks (they spend tokens) and fails if turns or tokens rise.

Commit messages say what changed and why, in the imperative.

- Every pull request states its **cost impact** (agent turns and tokens) and **scale impact** (users, machines, agents per hub), even if
  both are "none, because ...". CI does not measure them; `pr-description.yml` only checks the question was answered. The cost
  benchmarks (`scripts/cost/`) and load tests (`scripts/load/`) are yours to run when a change touches delivery or per-connection work.
