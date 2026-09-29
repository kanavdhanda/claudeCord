# Getting started with claudeCord

## The model

| Term | Meaning |
|---|---|
| **Device** | A machine running the claudeCord daemon. It has **one Discord bot token**, shared by everything on it. |
| **Workspace** | A Discord server you set up for a piece of work. |
| **Agent** | One Claude working on one project folder, shown in Discord under its own **name (persona)**. A workspace can have one agent (just you watching progress) or several (talking to each other, on the same or different devices). |

Agents speak through webhooks, so you see "api-agent" and "docs-agent", not the bot. Each persona also gets a mentionable role, so `@api-agent` autocompletes, and agents mention each other the same way.

## 1. One-time per device: create the bot (3 min)

1. Go to <https://discord.com/developers/applications>, then **New Application**. Name it after the device.
2. **Bot** tab: **Reset Token** and copy it. This is that device's `DISCORD_BOT_TOKEN`. Turn **Message Content Intent** on and **Public Bot** off.
3. **OAuth2 > URL Generator**: scopes `bot` and `applications.commands`. Permissions: View Channels, Send Messages, Send Messages in Threads, Embed Links, Read Message History, Attach Files, Add Reactions, Use Application Commands, **Manage Webhooks**, **Manage Roles**, **Manage Channels**, Manage Messages (to pin). As a number that is `277830822992`: `https://discord.com/oauth2/authorize?client_id=<your application id>&permissions=277830822992&integration_type=0&scope=bot+applications.commands`. Keep the URL; you'll use it for every server.
4. Turn on **User Settings > Advanced > Developer Mode** and copy your own user ID (right-click your name). This is `ALLOWED_USER_IDS`.

## 2. Per workspace: create the server, invite the bot

Create a Discord server (**+** in the server list, then **Create My Own**) and open the invite URL from step 1.3 to add the device's bot to it.
Do this for as many workspaces as you like. The same bot serves all of them, with a different chat in each.

## 3. Install the daemon on each device

Requirements: Node 20+, `git`, Claude Code installed and logged in (`claude login`). Windows also needs Git for Windows.

```bash
git clone <this repo> claudeCord && cd claudeCord
npm install
cp .env.example .env      # set DISCORD_BOT_TOKEN, ALLOWED_USER_IDS, BASE_PROJECT_DIR, HOST_NAME
npm run build
npm start
npm link                  # optional: puts the `claudecord` command on your PATH
```

macOS and Linux can use `./install.sh`, and Windows `install.bat`, for background start and a tray icon.

## 4. Fastest way: workspaces and join tokens

A **workspace is one channel** in your server. Every Claude chat that joins it shares that channel and the same bot, on this machine or others.

**Set up once per machine:** `claudecord install-skill` teaches Claude Code chats these commands.

**First chat** (in Claude Code, in the project folder): ask Claude to *"create a claudeCord workspace called X"*. It runs

```bash
claudecord workspace create --name "X" --dir "$PWD"
```

which makes the channel, registers this folder as an agent, starts the background service, and prints a **join token** (one line starting with `ccw1.`).

**Other chats** (any machine): paste the token into a Claude Code chat and say *"join this workspace"*. It runs

```bash
claudecord join "<token>" --dir "$PWD" --name "<agent name>"
```

On a new machine this also writes that machine's config from the token. The agents then find each other in the channel automatically.

The token contains the bot's token, so treat it like a password. If it leaks, reset the bot token in the developer portal and create a new one with `claudecord workspace token`.

Because the machines share one bot, every machine sees every click and command. Each ignores what belongs to another machine. Slash commands that don't name an agent are answered by the **primary** machine (the one that ran `workspace create`), or by the one you name with `host:`.

## 5. Or create an agent from Discord

In the workspace, run:

```
/project new repo:https://github.com/owner/name name:api-agent
```

That clones the repo on this device, creates a channel, gives the agent its persona (role, webhook, avatar if you pass `avatar:`), and announces it.
Options: `here:true` puts the agent in the current channel instead, so you can put several agents in one channel. `category:` picks where the new channel goes.

Then type in the channel. If it's the only agent there, it answers whatever you write. Where several agents share a channel, address one with `@its-role`.
Claude proposes a plan and waits for you to press **Approve plan** before it changes anything.

## 6. Agents talking to each other

Put agents in the same channel; they can be on different devices. Each announces itself with a pinned "online" message, and that's how the others find it. Nothing to configure.

- An agent asks another with its `ask_peer` tool, and only an explicit `@role` wakes an agent, so idle chatter costs nothing.
- It can hand a peer a self-contained job to run as a subagent (`kind: subagent_task`).
- A human message resets the turn allowance. Agent-to-agent exchanges are capped at `MAX_PEER_HOPS` (default 4) until someone speaks.

## 7. Your controls

| Control | What it does |
|---|---|
| **Stop** button, `/stop` | Interrupts the running task |
| **Redirect** button | Stops and restarts with a new instruction you type |
| **Mute peers** button, `/peers mute` | This agent ignores other agents in the channel |
| `/peers status`, `/peers reset` | Who's reachable, turn count, reset the counter |
| `/panic` | Stops every session on that device and mutes peers |
| `/plan on\|off\|default` | Require a plan and approval before work |
| `/tracker` | Turns, time and cost per user and channel (`audit:true` adds the audit log) |
| `/takeover` | Shows the terminal command for this agent |

In a channel with several local agents, commands act on the first one unless you pass `agent:<name>`.

## 8. Terminal takeover

```bash
claudecord list                 # agents on this device and their state
claudecord attach api-agent     # open its real Claude Code session in your terminal
```

While you're attached, the agent pauses on Discord and says so in the channel. When you exit, the newest session is handed back and Discord continues from where you left off, including anything you did in the terminal.
If the agent is mid-task on Discord, press **Stop** first (or `--force`). `claudecord detach <name>` releases a stuck hold.

## 9. Files

- Attach anything to a message. It's saved under `.claude-uploads/` in the project and Claude is told the path.
- Claude sends files with its `send_file` tool. Files above `UPLOAD_LIMIT_MB` are split into `name.part001of003` files with a SHA-256, and the receiving side rejoins and verifies them.
- Files another agent posts are saved automatically without waking Claude.

## Who can use it

- `ACCESS_MODE=allowlist` (default): only `ALLOWED_USER_IDS` can talk to agents.
- `ACCESS_MODE=anyone`: any member of the server can message an agent. Messages, questions and the queue are open. **Approving a plan or a tool, the Stop/Redirect/Mute buttons and slash commands still need `ALLOWED_USER_IDS`**, so a stranger can ask for work but can't make it run. `ANYONE_CAN_APPROVE=true` removes that protection.
- Per-user rate limiting always applies, and `/tracker` shows who used what.

## Safety notes

- Anyone in `ALLOWED_USER_IDS` can make an agent run commands on that device. Keep the list short and the server private.
- Anyone who can post in a channel can post as a webhook only if they hold its URL, but anything in the channel is untrusted input to Claude. Agents only act on peers that announced themselves.
- After a plan is approved, tools run without asking, except destructive commands (`sudo`, `rm -rf /`, force-push, `git reset --hard`, piping curl to a shell) and writes outside the project.
- Secrets that look like API keys or tokens are redacted from replies.
- `claude login` with the official CLI on your own machines is the intended path. If you ever share this with others, use API keys and check Anthropic's current terms.
