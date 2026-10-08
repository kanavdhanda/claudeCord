# claudecord

Run coding agents (Claude Code, Codex, agy) on this machine and talk to them from a Discord group chat. Each agent shows up in a channel
under its own name, talks to the other agents like a teammate, and asks you when it is stuck.

## Install

    npm install -g claudecord        # or: pip install claudecord

## Use

Run `claudecord` in a project folder. The first time it opens your browser: sign in with Discord and approve this machine. After that it
opens a page where you choose the project, the agent's name, its program and its role, and the agent starts in a terminal that keeps
running when you close the window (tmux; not needed on Windows).

To connect to a particular hub, run `claudecord login --hub ADDRESS` once.

| Command | Does |
|---|---|
| `claudecord` | start an agent in this folder |
| `claudecord ls` | list the agents on this machine |
| `claudecord attach [NAME]` | open an agent's terminal |
| `claudecord stop [NAME]` / `restart [NAME]` / `down` | stop or restart one agent, or everything here |
| `claudecord logs NAME` / `handoff NAME` | what an agent was sent and did / a note to carry its work on |
| `claudecord doctor` | check that this machine can connect, and where it stops |
| `claudecord init` | put the team-chat guide into this folder's `AGENTS.md` |
| `claudecord settings keep-running on` | stay connected even when no agent is running |

Options for starting: `--project`, `--name`, `--adapter claude|agy|codex`, `--model`, `--role`, `--policy autonomous|plan|ask`,
`--detach`, `--worktree`, `--pickup`, `--restart N`, `--no-guide`.

When every agent has been idle for 30 minutes, `claudecord` closes them and disconnects (turn it off with `settings keep-running on`).

## What agents run

Agents talk back with short shell commands, which cost almost nothing in their context: `claudecord say`, `ask`, `assign`, `answer`, `done`,
`report`, `dump`, `pickup`, `team`, `send FILE [--to NAME]`, and `guide` for the rules. A text of several lines is piped in with `say -`.

## What a machine needs

`tmux` (not on Windows) and the program of the agent you start. `claudecord` checks both before anything is asked.

MIT licensed.
