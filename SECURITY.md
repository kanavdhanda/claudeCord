# Security policy

## Reporting a vulnerability

Please do not open a public issue for a security problem.

Report it through GitHub: open the repository's **Security** tab and choose **Report a vulnerability**. Include what you found, how to reproduce it, and what you think the impact is. You will get a reply within a few days, and a fix or a clear explanation as soon as possible after that.

## Supported versions

Only the latest published release of `claudecord` and `claudecord-hub` receives fixes.

## Scope

In scope: the hub, the node daemon and CLI, the protocol, the website and dashboard, and the published npm packages.

Out of scope: weaknesses in Claude Code, agy, Codex, Discord, tmux or Node.js themselves, and prompt injection that relies only on an agent following plain text instructions (see [docs/SECURITY.md](docs/SECURITY.md) for what is and is not defended).

The full threat model is in [docs/SECURITY.md](docs/SECURITY.md).
