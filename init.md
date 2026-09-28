# Multi-Agent Claude Code Orchestrator with Discord Mesh & Tmux

## 1. Goal

Build a cross-device, multi-agent orchestration daemon for Claude Code CLI instances that:

1. Authenticates remote machines using a single Machine/Node Token per device.
2. Automatically routes agents into Discord channels named after their current project/working directory (e.g., directory `my-project` binds to `#my-project`).
3. Renders all active agents side-by-side in tiled/split tmux windows on the host machine.
4. Provides a `claudeTalk` dialogue mode: agents converse like engineers in Discord, keeping terminal tool executions silent, and only output a final consolidated report once or when an engineer asks for input.
5. Supports autonomous execution, plan mode, and human-in-the-loop escalation.

---

## 2. Core Architecture

### A. Central Hub (Cloud or Primary Machine)

- **Discord Bot Daemon:** Connects via Discord WebSockets.
- **Node Gateway:** Exposes an authenticated WebSocket or HTTP server (`/api/v1/node/connect`).
- **Channel Mapping Engine:**
  - When an agent registers with `project_name: "alpha"`, check if `#alpha` exists.
  - If not, create `#alpha` under the configured category.
  - Generates Discord Webhooks for each agent dynamically to preserve agent identities and avatars.

### B. Machine Node Daemon (`node-runner`)

- Runs on any developer machine (macOS / Linux).
- Authenticates using `NODE_SECRET` in `~/.claude-mesh/config.json`.
- Manages local `tmux` sessions:
  - Tmux Session Name: `claude-<project_name>`
  - Layout: `tiled` (e.g., `tmux select-layout -t <session> tiled`).
  - Automatically splits a new pane horizontally/vertically whenever a new agent is spawned.
- Intercepts input/output to Claude via tmux `send-keys` and buffer capturing.

---

## 3. Communication & `claudeTalk` Mode

### A. The Noise Filter (No Terminal Dumps)

- Claude Code bash outputs, diffs, tool executions, and file reads MUST NOT be dumped into Discord.
- Agents interact with an MCP server or side-channel tool: `agent_chat(message, target_agent, visibility)`.
- If `visibility == "chat"`, the message is posted to Discord as natural conversation.
- If `visibility == "report"`, it posts a formatted Discord Embed with the deliverable.

### B. `claudeTalk` Trigger Flow

1. User types in `#project-channel`: `claudeTalk: Plan and implement database migration`
2. **Phase 1: Agent Discussion (Silent Execution)**
   - Agent 1 (Device 1) and Agent 2 (Device 2) discuss schema design, trade-offs, and file changes in the channel like two peer engineers (2-3 sentences per turn).
   - If an agent reaches a roadblock or decision point, it tags `@engineer` (the human user) and enters `WAITING_INPUT` state.
3. **Phase 2: Execution & Single Consolidated Report**
   - The agents switch to code execution in their respective tmux panes locally.
   - When finished, one agent posts: `[STATUS: COMPLETE]` followed by a single report embed containing the pull request / diff summary.

---

## 4. Implementation Checklist for Claude

1. **`hub/` (Discord + Mesh Gateway)**
   - Setup `discord.js` or `discord.py` bot with Channel Management permissions.
   - Implement node authentication middleware (validates `Bearer <NODE_SECRET>`).
   - Implement WebSocket dispatcher for multiplexing messages between Discord channels and node daemons.

2. **`client/` (Tmux + Local Agent Daemon)**
   - Tmux manager script:
     - `create_or_join(project_dir, agent_name)`: Creates session `claude-<dir>` or adds pane.
     - `send_input(pane_id, text)`: Uses `tmux load-buffer` + `paste-buffer` + `send-keys Enter` to avoid race conditions.
   - Spawns Claude Code CLI inside each pane with custom system rules (via `.claude.json` or custom system prompt).

3. **`mcp-server/` (Inter-Agent Communication Server)**
   - Build a lightweight local MCP server using TypeScript or Python.
   - Tools to expose to Claude:
     - `send_peer_message(peer_name, message)`: Sends chat message visible in Discord.
     - `ask_human(question)`: Flags Discord with a mention and suspends agent until reply.
     - `publish_final_report(summary, artifacts)`: Emits the single final report.
