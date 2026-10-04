# Contributing

Thanks for helping. This guide covers how to run the project, what a good change looks like, and what is checked before it merges.

## Set up

You need Node 22.13 or newer, pnpm, and tmux (the end-to-end tests drive real tmux panes).

```
git clone https://github.com/kanavdhanda/claudeCord
cd claudeCord
pnpm install
pnpm build
pnpm test            # unit tests, about a second
pnpm test:e2e        # real tmux, about a minute
```

On Windows, run everything inside WSL2. The hub, the protocol and the unit tests also run natively, but running agents needs tmux.

## Layout

```
packages/protocol     wire types, validation, secret redaction, prompts
packages/hub          Discord bot, node gateway, router, web server, auth, benchmarks
packages/node         the claudecord CLI and node daemon (published to npm)
packages/agent-tools  MCP server and IPC client the agents use
docs/                 setup, capabilities, benchmarks, security model
```

## Scripts

| Command | What it does |
|---------|--------------|
| `pnpm build` | Typecheck and build every package. The CLI becomes a single bundle |
| `pnpm test` | Unit tests |
| `pnpm test:e2e` | End-to-end tests with real tmux and the packaged CLI |
| `pnpm test:all` | Both |
| `pnpm test:coverage` | Everything, with a coverage report in `coverage/` |
| `pnpm typecheck` | Types only, never touches the build output |
| `pnpm lint` and `pnpm format` | ESLint, and Prettier (`pnpm format:check` only checks) |
| `pnpm ci:local` | Runs what CI runs, in the same order, on your machine. Add `--fast` to skip the slow steps |
| `pnpm changeset` | Describe a change for the release notes |
| `pnpm bench` | Load test against an in-process hub. See `docs/BENCHMARKS.md` |
| `pnpm bench:serve` and `pnpm bench:k6` | The k6 scenario against a standalone hub |

## What a good change looks like

1. **It has tests.** A behaviour change comes with a test that fails without it. A bug fix starts with a test that reproduces the bug. The end-to-end suites are the place for anything that crosses the hub, the daemon and a pane.
2. **It keeps the security properties.** If you touch messages that reach a terminal, files, the environment an agent starts with, pairing, or the web server, read [docs/SECURITY.md](docs/SECURITY.md) and add a test for the property you are relying on. Never read configuration from environment variables, and never write a secret anywhere that is not mode 0600.
3. **It is small and says why.** One concern per change. Comments explain why, not what.
4. **It matches the surrounding code.** TypeScript strict mode, ES modules, no default exports for new modules, no emojis in code, docs or messages, short plain sentences.

## What CI checks

Every pull request must pass two checks, `ci passed` and `pull request checks passed`. Run `pnpm ci:local` first and you will see the same results.

| Check | What it needs |
|-------|---------------|
| Lint, format, types | `pnpm lint`, `pnpm format:check`, `pnpm typecheck` clean |
| Tests on Linux x64 and arm64, macOS Apple silicon and Intel, Node 22 and 24 | Unit and end-to-end tests pass. Native Windows runs the portable tests and is allowed to fail until it has passed once |
| Packaged CLI smoke test | The npm tarball installs into an empty folder and runs, on every operating system |
| Coverage | Statements 88%, branches 78%, functions 86%, lines 90% across the whole suite. Raise these as coverage improves, never lower them |
| Coverage of new code | At least 80% of the executable lines you added are run by a test |
| Performance budget | 2,000 simulated agents deliver at least 99.5% of messages with p99 under 100 ms and under 400 MB. Plus a k6 run at 400 agents |
| Security | CodeQL, a dependency audit (production dependencies must be clean), dependency review on pull requests, and a secret scan |
| Title | A Conventional Commit, such as `fix(node): stop reading stale prompts` |
| Checklist | Every box in the pull request template is ticked, or the line removed with a reason |
| Tests | A change under `packages/*/src` includes a change under `packages/*/test`, or carries the `no-tests` label |
| Changeset | A change to code that ships to npm adds a `.changeset/*.md`, or carries the `no-changeset` label |

The labels are for changes that truly cannot be tested or do not affect users, such as a pure rename. Say why in the description.

## Adding an agent adapter

An adapter teaches claudeCord to launch one CLI and to read its terminal. It lives in `packages/node/src/adapters/` and implements `Adapter`:

- `argv`: how to start it, including the model and permission policy flags.
- `detect`: from a screenshot of the visible pane, is it busy, ready, showing a prompt, or at a usage limit.
- `startupChoice`, `selectKeys`, `otherKeys`: how to answer its menus.

Test it against captured screens, like `packages/node/test/adapters.test.ts`, and run it for real before claiming it works. Add its credential variables to the allow-list in `packages/node/src/env.ts`.

## Commits and pull requests

- Use [Conventional Commits](https://www.conventionalcommits.org): `feat:`, `fix:`, `docs:`, `test:`, `refactor:`, `perf:`, `chore:`. Say what changed and why in the body when it is not obvious.
- Describe what you changed, how you tested it, and anything you could not test.
- Keep the diff reviewable. A large refactor and a feature do not belong in one pull request.

## For maintainers

Settings that live in GitHub, not in the repository:

- Branch protection on `main`: require `ci passed` and `pull request checks passed`, require branches to be up to date, require review, and block force pushes.
- Settings, Actions, General: allow GitHub Actions to create and approve pull requests, so the release workflow can open the "version packages" pull request.
- Secrets: `NPM_TOKEN`, an npm automation token with publish rights.
- Turn on private vulnerability reporting, secret scanning and push protection, and Dependabot alerts.
- Native Windows is marked `experimental` in `.github/workflows/ci.yml`. Once it has passed, remove that flag so it blocks merges like the others.

A release is a pull request, not a command. Merging a pull request with a changeset makes the release workflow open "chore: version packages". Merging that one publishes to npm with provenance.

## Reporting bugs and security problems

Open an issue with steps to reproduce, what you expected and what happened. Report security problems privately, as described in [SECURITY.md](SECURITY.md).
