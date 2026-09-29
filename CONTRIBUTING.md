# Contributing

Thanks for your interest in contributing! Here's how to get started.

## Development Setup

```bash
git clone https://github.com/chadingTV/claudecode-discord.git
cd claudecode-discord
npm install
cp .env.example .env   # Fill in your Discord bot token and settings
npm run dev             # Start in dev mode (tsx, auto-reload)
```

### Prerequisites

- Node.js 20+
- A Discord bot token ([create one here](https://discord.com/developers/applications))
- Claude Code installed and authenticated (`claude` CLI working)

## Project Structure

```
src/
├── index.ts                # Entry point
├── bot/
│   ├── client.ts           # Discord bot setup
│   ├── commands/           # Slash commands
│   └── handlers/           # Message & interaction handlers
├── claude/
│   ├── session-manager.ts  # Core: Agent SDK session lifecycle
│   └── output-formatter.ts # Discord embed/button formatting
├── db/                     # SQLite persistence
├── security/               # Whitelist, rate limit, path validation
└── utils/                  # Config (zod schema)
```

## Code Style

- **TypeScript strict mode** — no `any` unless absolutely necessary
- **ESM** — `import/export`, no `require()`
- **Zod** for all runtime validation (env vars, configs)
- **Error handling** — every async operation needs try-catch with meaningful error messages

## Making Changes

1. Fork the repo and create a branch from `main`
2. Make your changes
3. Run tests: `npm test`
4. Run build: `npm run build`
5. Submit a PR with a clear description of what and why

### Commit Messages

Use conventional format:

```
feat: add /cost command for session billing
fix: session resume failing after bot restart
docs: update Windows setup guide
```

### Pull Requests

- Keep PRs focused — one feature or fix per PR
- Update relevant docs if behavior changes
- Add tests for new features when possible

## Good First Issues

Look for issues labeled [`good first issue`](https://github.com/chadingTV/claudecode-discord/labels/good%20first%20issue). These are scoped tasks suitable for first-time contributors.

## Reporting Bugs

Use the [Bug Report](https://github.com/chadingTV/claudecode-discord/issues/new?template=bug_report.md) template. Include:
- Steps to reproduce
- Expected vs actual behavior
- Your OS and Node.js version

## Requesting Features

Use the [Feature Request](https://github.com/chadingTV/claudecode-discord/issues/new?template=feature_request.md) template.

## License

By contributing, you agree that your contributions will be licensed under the [MIT License](LICENSE).
