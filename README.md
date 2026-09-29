<p align="center">
  <img src="docs/icon-rounded.png" alt="Claude Code Discord Controller" width="120">
</p>

# claudeCord

> A fork of [chadingTV/claudecode-discord](https://github.com/chadingTV/claudecode-discord) (MIT) extended for **multiple machines, multiple Discord servers and agents that talk to each other**.
> **Start here: [docs/GETTING-STARTED.md](docs/GETTING-STARTED.md).**

What's added on top of the original:

- **One bot per device, many named agents:** each workspace (Discord server) has agents with their own name, avatar and @role, on one device or several
- **Agents talk to each other** in a shared channel (`ask_peer`), can ask a peer to run a subagent task, with a turn cap and a **Mute peers** switch; they discover each other automatically
- **Workspaces and join tokens:** the first Claude chat runs `claudecord workspace create` and gets a text token; other chats on any machine run `claudecord join <token>` to share the same channel
- **Open access option:** `ACCESS_MODE=anyone` lets any server member message the agents while approvals stay with admins
- **Terminal takeover:** `claudecord attach <agent>` continues an agent's session in your terminal and hands it back on exit
- **Plan, approve, then auto:** Claude proposes a plan, you approve, then it works without per-step prompts (destructive actions still ask)
- **Stop / Redirect / Mute / `/panic`** controls
- **Multiple Discord servers** from one bot, with a different chat per server
- **`/project new`** clones a public GitHub repo and creates its channel
- **Context prompt:** Claude is told its host, folder, repo, channel and peers
- **Files both ways** (`send_file`), with automatic splitting and checksummed reassembly above Discord's limit
- **`/tracker`** usage per user and channel, an audit log, and secret redaction
- Fix: live streaming of replies now works with the current Agent SDK

The rest of this README is the original project's documentation.

---

# Claude Code Discord Controller

[![CI](https://github.com/chadingTV/claudecode-discord/actions/workflows/ci.yml/badge.svg)](https://github.com/chadingTV/claudecode-discord/actions)

Control Claude Code from your phone — a multi-machine agent hub via Discord.
**No API key needed — works with your existing Claude Pro or Max subscription.**

<p align="center">
  <img src="docs/demo.gif" alt="Demo — register a project and code with Claude from Discord" width="300">
</p>

> **[Korean documentation (한국어)](docs/README.kr.md)**

## Why This Bot? — vs Official Remote Control

Anthropic's [Remote Control](https://code.claude.com/docs/en/remote-control) lets you view a running local session from your phone. This bot goes further — it's a **multi-machine agent hub** that runs as a daemon, creates new sessions on demand, and supports team collaboration.

|                              | This Bot | Official Remote |
|------------------------------|:--------:|:---------------:|
| Start new session from phone | ✅       | ❌              |
| Daemon (survives terminal close) | ✅   | ❌              |
| Multi-machine hub            | ✅       | ❌              |
| Concurrent sessions per machine | ✅    | ❌              |
| Push notifications           | ✅       | ❌              |
| Team collaboration           | ✅       | ❌              |
| Native tray app (3 OS)       | ✅       | ❌              |
| Zero open ports              | ✅       | ✅              |

### Multi-PC Hub

Create a separate Discord bot per machine, invite them all to the same server, and assign channels:

```
Your Discord Server
├── #work-mac-frontend     ← Bot on work Mac
├── #work-mac-backend      ← Bot on work Mac
├── #home-pc-sideproject   ← Bot on home PC
├── #cloud-server-infra    ← Bot on cloud server
```

**Control every machine's Claude Code from a single phone.** The channel list itself becomes your real-time status dashboard across all machines and projects.

## Why Discord?

Discord isn't just a chat app — it's a surprisingly perfect fit for controlling AI agents:

- **Already on your phone.** No new app to install, no web UI to bookmark. Open Discord and go.
- **Push notifications for free.** Get alerted instantly when Claude needs approval or finishes a task — even with the phone locked.
- **Channels = workspaces.** Each channel maps to a project directory. The sidebar becomes a real-time dashboard of all your projects.
- **Rich UI out of the box.** Buttons, select menus, embeds, file uploads — Discord provides the interactive components, so the bot doesn't need its own frontend.
- **Team-ready by default.** Invite teammates to your server. They can watch Claude work, approve tool calls, or queue tasks — no extra auth layer needed.
- **Cross-platform.** Windows, macOS, Linux, iOS, Android, web browser — Discord runs everywhere.

## Features

- 💰 **No API key** — runs on Claude Code CLI with your Pro or Max subscription
- 📱 Remote control Claude Code from Discord (desktop/web/mobile)
- 🔀 Independent sessions per channel (project directory mapping)
- ✅ Tool use approve/deny via Discord button UI
- ❓ Interactive question UI (selectable options + custom text input)
- ⏹️ Stop button for instant cancellation during progress, message queue for sequential tasks
- 📎 File attachments support (images, documents, code files)
- 🔄 Session resume/delete/new (persist across bot restarts, last conversation preview)
- ⏱️ Real-time progress display (tool usage, elapsed time)
- 🔒 User whitelist, rate limiting, path security, duplicate instance prevention
- 📊 **Claude Code usage dashboard** in control panel — Session (5hr), Weekly (7day), Weekly Sonnet with progress bars, auto-refresh, click to open usage page

## Tech Stack

| Category | Technology |
|----------|------------|
| Runtime | Node.js 20+, TypeScript |
| Discord | discord.js v14 |
| AI | @anthropic-ai/claude-agent-sdk |
| DB | better-sqlite3 (SQLite) |
| Validation | zod v4 |
| Build | tsup (ESM) |
| Test | vitest |

## Installation

```bash
git clone https://github.com/chadingTV/claudecode-discord.git
cd claudecode-discord

# macOS / Linux
./install.sh

# Windows
./install.bat
```

### Setup Guides

| Platform | Guide |
|----------|-------|
| **macOS / Linux** | **[SETUP.md](SETUP.md)** — terminal-based setup, menu bar / tray app |
|  **Windows** | **[SETUP-WINDOWS.md](docs/SETUP-WINDOWS.md)** — GUI installer, system tray app with control panel, desktop shortcut |

Windows users: `install.bat` handles everything automatically — installs dependencies, builds, creates a desktop shortcut, and launches the bot with a system tray GUI.

<details>
<summary><strong>Project Structure</strong></summary>

```
claudecode-discord/
├── install.sh / install.bat    # Auto-installers
├── mac-start.sh                # macOS background launcher + menu bar
├── linux-start.sh              # Linux background launcher + system tray
├── win-start.bat               # Windows background launcher + system tray
├── menubar/                    # macOS menu bar app (Swift)
├── tray/                       # System tray app (Linux: Python, Windows: C#)
├── src/
│   ├── index.ts                # Entry point
│   ├── bot/
│   │   ├── client.ts           # Discord bot init & events
│   │   ├── commands/           # Slash commands (10)
│   │   └── handlers/           # Message & interaction handlers
│   ├── claude/
│   │   ├── session-manager.ts  # Session lifecycle
│   │   └── output-formatter.ts # Discord output formatting
│   ├── db/                     # SQLite (better-sqlite3)
│   ├── security/               # Auth, rate limit, path validation
│   └── utils/                  # Config (zod)
├── SETUP.md                    # macOS/Linux setup guide
├── docs/                       # Translations, screenshots
└── package.json
```

</details>

## Usage

| Command | Description | Example |
|---------|-------------|---------|
| `/register <folder>` | Link current channel to a project | `/register my-project` |
| `/unregister` | Unlink channel | |
| `/status` | Check all session statuses | |
| `/stop` | Stop current channel's session | |
| `/auto-approve on\|off` | Toggle auto-approval | `/auto-approve on` |
| `/sessions` | List sessions to resume or delete | |
| `/last` | Show the last Claude response from current session | |
| `/usage` | Show Claude Code usage (Session 5hr / Weekly / Sonnet) | |
| `/queue list` | View queued messages (cancel individually or all) | |
| `/queue clear` | Cancel all queued messages | |
| `/clear-sessions` | Delete all session files for the project | |

The `/register` command shows an **autocomplete dropdown** listing subdirectories under `BASE_PROJECT_DIR` — just start typing to filter and select.
The first option `.` registers the base directory itself. You can also type a custom path; absolute paths work too.

> **Why per-directory?** Claude Code manages sessions per project directory — each directory has its own conversation history, `CLAUDE.md` context, and tool permissions. By mapping one Discord channel to one directory, each channel gets an independent Claude workspace.

Send a **regular message** in a registered channel and Claude will respond.
Attach images, documents, or code files and Claude can read and analyze them.

### In-Progress Controls

- **⏹️ Stop** button on progress messages for instant cancellation
- Sending a new message while busy offers **message queue** — auto-processes after current task completes
- `/queue list` to view queued messages — cancel individually (❌) or all at once
- `/stop` slash command also available

<details>
<summary><strong>Architecture</strong></summary>

```
[Mobile Discord] ←→ [Discord Bot] ←→ [Session Manager] ←→ [Claude Agent SDK]
                          ↕
                     [SQLite DB]
```

- Independent sessions per channel (project directory mapping)
- Claude Agent SDK runs Claude Code as subprocess (shares existing auth)
- Tool use approval via Discord buttons (auto-approve mode supported)
- Streaming responses edited every 1.5s into Discord messages
- Heartbeat progress display every 15s until text output begins
- Markdown code blocks preserved across message splits

**Session States:** 🟢 working · 🟡 waiting for approval · ⚪ idle · 🔴 offline

</details>

## Security

### Zero External Attack Surface

This bot **does not open any HTTP servers, ports, or API endpoints.** It connects to Discord via an outbound WebSocket — there is no inbound listener, so there is no network path for external attackers to reach this bot.

```
Typical web server:  External → [Port open, waiting] → Receives requests  (inbound)
This bot:            Bot → [Connects to Discord] → Receives events         (outbound only)
```

### Self-Hosted Architecture

The bot runs entirely on your own PC/server. No external servers involved, and no data leaves your machine except through Discord and the Anthropic API (which uses your own Claude Code login session).

### Access Control

- `ALLOWED_USER_IDS` whitelist-based authentication — all messages and commands from unregistered users are ignored
- Discord servers are private by default (no access without invite link)
- Per-minute request rate limiting

### Execution Protection

- Tool use default: file modifications, command execution, etc. **require user approval each time** (Discord buttons)
- Path traversal (`..`) blocked
- File attachments: executable files (.exe, .bat, etc.) blocked, 25MB size limit

### Precautions

- The `.env` file contains your bot token — **never share it publicly.** If compromised, immediately Reset Token in Discord Developer Portal
- `auto-approve` mode is convenient but may allow Claude to perform unintended actions — use only on trusted projects

## Quick Start by Platform

Each platform runs the bot as a background service with a native GUI for control — no terminal babysitting needed.

### macOS — Menu Bar App

<p align="center">
  <img src="docs/mac-tray.png" alt="macOS Control Panel" width="400">
</p>

```bash
./mac-start.sh          # Start (background + menu bar icon)
./mac-start.sh --stop   # Stop
```

Control panel GUI (left-click icon), **Claude Code usage dashboard** (Session 5hr / Weekly / Sonnet, click to open usage page), settings dialog, auto-update, auto-restart on crash, auto-start on boot (launchd). → **[Full guide](SETUP.md)**

### Linux — System Tray + Control Panel

<p align="center">
  <img src="docs/linux-tray.png" alt="Linux System Tray" width="350">
</p>

```bash
./linux-start.sh          # Start (systemd + tray icon)
./linux-start.sh --stop   # Stop
```

GTK3 **control panel** (left-click tray icon), **Claude Code usage dashboard**, settings dialog, auto-restart, auto-start on boot (systemd). Works headless too. → **[Full guide](SETUP.md)**

### Windows — System Tray + Control Panel

<p align="center">
  <img src="docs/windows-tray.png" alt="Windows Control Panel" width="400">
</p>

```batch
win-start.bat          &:: Start (background + tray + control panel)
win-start.bat --stop   &:: Stop
```

Desktop shortcut, control panel GUI, **Claude Code usage dashboard**, settings dialog, auto-update, auto-start on logon (Registry). → **[Full guide](docs/SETUP-WINDOWS.md)**

## Development

```bash
npm run dev          # Dev mode (tsx)
npm run build        # Production build (tsup)
npm start            # Run built files
npm test             # Tests (vitest)
npm run test:watch   # Test watch mode
```

## License

[MIT License](LICENSE) - Free to use, modify, and distribute commercially. Attribution required: include the original copyright notice and link to [this repository](https://github.com/chadingTV/claudecode-discord).

---

If you find this project useful, please consider giving it a ⭐ — it helps others discover it!
