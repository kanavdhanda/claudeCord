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
| `pnpm bench` | Load test against an in-process hub. See `docs/BENCHMARKS.md` |
| `pnpm bench:serve` and `pnpm bench:k6` | The k6 scenario against a standalone hub |

## What a good change looks like

1. **It has tests.** A behaviour change comes with a test that fails without it. A bug fix starts with a test that reproduces the bug. The end-to-end suites are the place for anything that crosses the hub, the daemon and a pane.
2. **It keeps the security properties.** If you touch messages that reach a terminal, files, the environment an agent starts with, pairing, or the web server, read [docs/SECURITY.md](docs/SECURITY.md) and add a test for the property you are relying on. Never read configuration from environment variables, and never write a secret anywhere that is not mode 0600.
3. **It is small and says why.** One concern per change. Comments explain why, not what.
4. **It matches the surrounding code.** TypeScript strict mode, ES modules, no default exports for new modules, no emojis in code, docs or messages, short plain sentences.

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

## Reporting bugs and security problems

Open an issue with steps to reproduce, what you expected and what happened. Report security problems privately, as described in [SECURITY.md](SECURITY.md).
