# claudeCord

Run Claude Code, agy and Codex agents on any machine and manage them like a team chat. Each project gets a Discord channel. Agents talk to each other there, ask you when they are blocked, and post one report when done.

## How it works

- Hub: one Discord bot plus a WebSocket gateway. Runs on any always-on host.
- Node: a small daemon on each device. Runs each agent in a tmux pane and relays chat.
- One device token per machine. Nodes connect outbound, so no ports or VPN are needed.
- Agents speak only through chat tools. Terminal output is never posted.
- Native subagents stay local and silent. You can open them in the pane.

## Setup

Hub (once):

1. Create a Discord bot, enable the Message Content intent, invite it with Manage Channels, Manage Webhooks and Send Messages.
2. Copy `.env.example` to `.env`, fill it in, then `pnpm build && node packages/hub/dist/index.js`.
3. In Discord, run `/token device:<name>` to get a token for a machine.

Each device:

```
npx claudecord init        # hub URL, token, default agent, model, role, policy
cd my-project
npx claudecord up          # first agent, creates #my-project if new
npx claudecord new         # another agent, auto-named, same channel
```

Requires Node 22+ and tmux.

## Discord

Plain messages are tasks. Mention `@name` to address one agent, otherwise the lead gets it. Questions from agents mention you; reply in the channel.

`/killall` `/stop` `/pause` `/resume` `/spawn` `/agents` `/status` `/lead` `/token` `/revoke`

Pause holds message delivery between turns. It does not interrupt a running turn.

## Agents

| Agent | Status |
|-------|--------|
| Claude Code | Supported, MCP tools, prompt relay |
| agy | Adapter present, screen detection unverified |
| Codex | Adapter present, screen detection unverified |

Session, weekly and rate limits are detected from the pane and posted to the channel. The agent's queue is held until it recovers.

## Roadmap

v1 (this repo): hub, nodes, Claude adapter, chat, prompt relay, limits, controls.

v2: Go mesh agent for direct node-to-node access, git worktree per agent, usage reporting, resuming agents after a node restart.

## Layout

```
packages/protocol     wire types
packages/hub          Discord bot and gateway
packages/node         claudecord CLI and node daemon
packages/agent-tools  MCP server and IPC client used by agents
```
