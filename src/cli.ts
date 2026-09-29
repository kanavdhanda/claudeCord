#!/usr/bin/env node
import { spawn } from "node:child_process";
import * as nodePty from "node-pty";
import fs from "node:fs";
import readline from "node:readline";
import {
  getAgentByPersonaAnywhere,
  getEveryProject,
  getPrimaryAgent,
  getSession,
  initDatabase,
  recordAudit,
  setTerminalHold,
  upsertSession,
} from "./db/database.js";
import type { Project } from "./db/types.js";
import { isTerminalHeld, readAndClearRelay } from "./agents/terminal.js";
import { latestSessionId } from "./agents/session-files.js";
import { randomUUID } from "node:crypto";
import os from "node:os";
import path from "node:path";
import { execFile } from "node:child_process";
import { getConfig, loadConfig } from "./utils/config.js";
import { appRoot } from "./utils/paths.js";
import { decodeToken, encodeToken } from "./workspace/token.js";
import { readEnvValue, upsertEnv } from "./workspace/env-file.js";
import { daemonPid, ensureDaemon } from "./workspace/daemon.js";
import { addAgentToChannel, createWorkspaceChannel, pickGuild, withClient } from "./workspace/ops.js";
import { skillMarkdown } from "./workspace/skill.js";
import { setSetting } from "./db/database.js";
import { discordChannelOf } from "./agents/identity.js";
import type { TextChannel } from "discord.js";

const HELP = `claudecord - manage agents from the terminal

  claudecord workspace create --name <n> [--dir <folder>] [--agent <name>] [--guild <server>]
                                   Make a shared Discord channel with this folder as its first agent
                                   and print a join token for other Claude chats
  claudecord join <token> [--dir <folder>] [--name <agent>] [--force]
                                   Join a workspace with a token
  claudecord workspace token [agent]   Print the join token again
  claudecord daemon status|start   Is the background service running?
  claudecord install-skill         Teach Claude Code chats to use these commands
  claudecord list                  Show the agents on this machine
  claudecord attach [name] [--force]   Attach to an agent (omit name → pick from list or use primary)
                                       If the name is new, auto-creates an agent in the workspace
  claudecord detach <name>         Release a stuck terminal hold

While attached, Discord messages inject directly into Claude's input. Type normally
or message from your phone — both reach the same session.`;

function findExisting(name: string): Project | undefined {
  const matches = getAgentByPersonaAnywhere(name);
  if (matches.length > 1) {
    console.error(`"${name}" exists in ${matches.length} servers; rename one, or detach via the Discord bot.`);
    process.exit(1);
  }
  return matches[0];
}

function findRequired(name: string | undefined): Project {
  if (!name) {
    console.error("Give an agent name. See: claudecord list");
    process.exit(1);
  }
  const p = findExisting(name);
  if (!p) {
    console.error(`No agent named "${name}" on this machine. See: claudecord list`);
    process.exit(1);
  }
  return p;
}

/** Prompt the user to pick from a list, returns chosen index. */
async function pickFromList(items: string[]): Promise<number> {
  items.forEach((item, i) => console.log(`  ${i + 1}) ${item}`));
  const rl = readline.createInterface({ input: process.stdin, output: process.stdout });
  return new Promise((resolve) => {
    rl.question("Select agent [1]: ", (ans) => {
      rl.close();
      const n = parseInt(ans.trim() || "1", 10);
      resolve(isNaN(n) || n < 1 || n > items.length ? 0 : n - 1);
    });
  });
}

function list(): void {
  const rows = getEveryProject();
  if (rows.length === 0) {
    console.log("No agents yet. Create one in Discord with /project new.");
    return;
  }
  for (const p of rows) {
    const s = getSession(p.channel_id);
    const state = isTerminalHeld(p) ? "terminal" : (s?.status ?? "offline");
    console.log(`${(p.persona ?? "(unnamed)").padEnd(20)} ${state.padEnd(9)} ${p.project_path}${p.repo_url ? `  <- ${p.repo_url}` : ""}`);
  }
}

async function resolveAgent(name: string | undefined, force: boolean): Promise<Project> {
  // Named agent that doesn't exist yet → auto-create in the workspace
  if (name) {
    const existing = findExisting(name);
    if (existing) return existing;

    // Auto-create: register current directory as a new agent
    const dir = process.cwd();
    const allProjects = getEveryProject();
    if (allProjects.length === 0) {
      console.error(`No workspace found. Run "claudecord workspace create" first.`);
      process.exit(1);
    }
    // Use the first project's guild to derive guild/channel — create via Discord
    const sample = allProjects[0];
    console.log(`Agent "${name}" not found. Creating it in the workspace (dir: ${dir})…`);
    const repoUrl = await gitOrigin(dir);
    await withClient(async (client) => {
      const guild = await client.guilds.fetch(sample.guild_id);
      // Find the workspace channel — use the Discord channel of the sample agent
      const { discordChannelId } = sample;
      if (!discordChannelId) throw new Error("Could not determine workspace channel.");
      const channel = await client.channels.fetch(discordChannelId);
      if (!channel || !("send" in channel)) throw new Error("Workspace channel not accessible.");
      await addAgentToChannel(client, channel as Parameters<typeof addAgentToChannel>[1], name, dir, repoUrl);
      void guild; // used implicitly
    });
    await ensureDaemon();
    const created = findExisting(name);
    if (!created) { console.error("Auto-create failed."); process.exit(1); }
    return created;
  }

  // No name → try primary, then single agent, then interactive list
  const allProjects = getEveryProject();
  if (allProjects.length === 0) {
    console.error("No agents on this machine. Run \"claudecord workspace create\" first.");
    process.exit(1);
  }

  // Primary agent
  for (const p of allProjects) {
    if (p.is_primary) return p;
  }
  // Check guild-level primary
  const guilds = [...new Set(allProjects.map((p) => p.guild_id))];
  for (const gid of guilds) {
    const primary = getPrimaryAgent(gid);
    if (primary) return primary;
  }

  // Only one agent → auto-select
  if (allProjects.length === 1) return allProjects[0];

  // Multiple → show list
  console.log("\nWhich agent do you want to attach to?\n");
  const labels = allProjects.map((p) => {
    const s = getSession(p.channel_id);
    const state = isTerminalHeld(p) ? "terminal" : (s?.status ?? "offline");
    return `${(p.persona ?? "(unnamed)").padEnd(20)} [${state}]  ${p.project_path}`;
  });
  const idx = await pickFromList(labels);
  return allProjects[idx];
}

async function attach(name: string | undefined, force: boolean): Promise<void> {
  const p = await resolveAgent(name, force);

  if (isTerminalHeld(p)) {
    console.error("Another terminal already holds this agent. Use: claudecord detach " + p.persona);
    process.exit(1);
  }
  const session = getSession(p.channel_id);
  if (!force && (session?.status === "online" || session?.status === "waiting")) {
    console.error(`${p.persona} is working on Discord right now. Stop it there first (Stop button or /stop), or use --force.`);
    process.exit(1);
  }
  if (!fs.existsSync(p.project_path)) {
    console.error(`Project folder is missing: ${p.project_path}`);
    process.exit(1);
  }

  const resumeId = session?.session_id ?? latestSessionId(p.project_path);
  const args = resumeId ? ["--resume", resumeId] : [];
  setTerminalHold(p.channel_id, process.pid);
  recordAudit({ guildId: p.guild_id, channelId: p.channel_id, action: "terminal-attach", detail: resumeId ?? "new session" });
  console.log(`Attached to ${p.persona}${resumeId ? ` (resuming ${resumeId.slice(0, 8)})` : " (new session)"}. Type normally or send messages from Discord.\n`);

  let released = false;
  const release = () => {
    if (released) return;
    released = true;
    const latest = latestSessionId(p.project_path);
    if (latest) upsertSession(session?.id ?? randomUUID(), p.channel_id, latest, "idle");
    setTerminalHold(p.channel_id, null);
    recordAudit({ guildId: p.guild_id, channelId: p.channel_id, action: "terminal-detach", detail: latest ?? "" });
  };

  // Spawn claude in a PTY so it gets a real TTY while we can still inject text
  const ptyProcess = nodePty.spawn("claude", args, {
    name: process.env.TERM ?? "xterm-256color",
    cols: process.stdout.columns ?? 80,
    rows: process.stdout.rows ?? 24,
    cwd: p.project_path,
    env: process.env as Record<string, string>,
  });

  // PTY output → our terminal
  ptyProcess.onData((data) => process.stdout.write(data));

  // Keyboard → PTY
  if (process.stdin.isTTY) process.stdin.setRawMode(true);
  process.stdin.resume();
  process.stdin.on("data", (data: Buffer) => ptyProcess.write(data.toString()));

  // Resize terminal → PTY
  process.stdout.on("resize", () => {
    ptyProcess.resize(process.stdout.columns ?? 80, process.stdout.rows ?? 24);
  });

  // Discord relay → PTY stdin (real injection)
  const relayTimer = setInterval(() => {
    const messages = readAndClearRelay(p.persona ?? p.channel_id);
    for (const text of messages) {
      // Show a visual banner, then inject the text as actual input
      ptyProcess.write(`\r\n\x1b[33m📱 [Discord]\x1b[0m ${text}\r\n`);
    }
  }, 500);

  process.on("exit", release);
  process.on("SIGTERM", () => { release(); process.exit(143); });

  ptyProcess.onExit(({ exitCode }) => {
    clearInterval(relayTimer);
    if (process.stdin.isTTY) process.stdin.setRawMode(false);
    release();
    console.log("\nDetached. The agent is available on Discord again.");
    process.exit(exitCode ?? 0);
  });
}

function detach(name: string | undefined): void {
  const p = findRequired(name);
  setTerminalHold(p.channel_id, null);
  console.log(`Released ${p.persona}.`);
}


function flag(rest: string[], name: string): string | undefined {
  const i = rest.indexOf(`--${name}`);
  return i >= 0 && rest[i + 1] && !rest[i + 1].startsWith("--") ? rest[i + 1] : undefined;
}

function gitOrigin(dir: string): Promise<string | null> {
  return new Promise((resolve) =>
    execFile("git", ["remote", "get-url", "origin"], { cwd: dir, timeout: 3000 }, (err, out) => resolve(err ? null : out.trim() || null)),
  );
}

function projectDir(rest: string[]): string {
  const dir = path.resolve(flag(rest, "dir") ?? process.cwd());
  if (!fs.existsSync(dir) || !fs.statSync(dir).isDirectory()) {
    console.error(`Folder not found: ${dir}`);
    process.exit(1);
  }
  return dir;
}

function tokenFor(channelId: string, guildId: string, name: string): string {
  const c = getConfig();
  return encodeToken({
    v: 1,
    botToken: c.DISCORD_BOT_TOKEN,
    guildId,
    channelId,
    name,
    admins: c.ALLOWED_USER_IDS,
    access: c.ACCESS_MODE,
    anyoneApprove: c.ANYONE_CAN_APPROVE,
  });
}

function printToken(token: string): void {
  console.log("\nJoin token (paste it into another Claude chat, it contains the bot's token, so keep it secret):\n");
  console.log(token);
}

async function workspaceCreate(rest: string[]): Promise<void> {
  loadConfig();
  const config = getConfig();
  const dir = projectDir(rest);
  const name = flag(rest, "name") ?? path.basename(dir);
  const persona = flag(rest, "agent") ?? path.basename(dir);
  const guildHint = flag(rest, "guild") ?? (config.DISCORD_GUILD_IDS[0] || config.DISCORD_GUILD_ID);
  const repo = await gitOrigin(dir);

  const made = await withClient(config.DISCORD_BOT_TOKEN, async (client) => {
    const guild = pickGuild(client, guildHint);
    const channel = await createWorkspaceChannel(guild, name);
    const { roleId } = await addAgentToChannel(client, channel, persona, dir, repo);
    return { guildId: guild.id, guildName: guild.name, channelId: channel.id, channelName: channel.name, roleId };
  });
  setSetting("primary", "1");
  const started = ensureDaemon();

  console.log(`Workspace created: #${made.channelName} in ${made.guildName}`);
  console.log(`Agent "${persona}" is working in ${dir}${made.roleId ? ` (mention it with @claude · ${persona})` : ""}.`);
  console.log(started ? "Background service started." : "Background service already running.");
  printToken(tokenFor(made.channelId, made.guildId, name));
}

async function workspaceToken(agentName: string | undefined): Promise<void> {
  loadConfig();
  const all = getEveryProject().filter((p) => p.persona && (!agentName || p.persona.toLowerCase() === agentName.toLowerCase()));
  if (all.length === 0) {
    console.error(agentName ? `No agent named "${agentName}".` : "No agents on this machine yet.");
    process.exit(1);
  }
  if (all.length > 1) {
    console.error(`Several agents here; say which: ${all.map((p) => p.persona).join(", ")}`);
    process.exit(1);
  }
  const p = all[0];
  printToken(tokenFor(discordChannelOf(p.discord_channel_id ?? p.channel_id), p.guild_id, p.persona ?? "workspace"));
}

async function join(tokenText: string | undefined, rest: string[]): Promise<void> {
  if (!tokenText) {
    console.error("Give the workspace token: claudecord join <token>");
    process.exit(1);
  }
  const t = decodeToken(tokenText);
  const dir = projectDir(rest);
  const persona = flag(rest, "name") ?? path.basename(dir);

  const envPath = path.join(appRoot(), ".env");
  let env = fs.existsSync(envPath) ? fs.readFileSync(envPath, "utf-8") : "";
  const existing = readEnvValue(env, "DISCORD_BOT_TOKEN");
  const newDevice = !existing;
  if (existing && existing !== t.botToken && !rest.includes("--force")) {
    console.error("This machine already runs a different bot. Joining would replace it for all its agents. Use --force if that's what you want.");
    process.exit(1);
  }
  const admins = new Set([...(readEnvValue(env, "ALLOWED_USER_IDS")?.split(",").map((x) => x.trim()) ?? []), ...t.admins].filter(Boolean));
  const guilds = new Set([...(readEnvValue(env, "DISCORD_GUILD_IDS")?.split(",").map((x) => x.trim()) ?? []), t.guildId].filter(Boolean));
  const updates = {
    DISCORD_BOT_TOKEN: t.botToken,
    ALLOWED_USER_IDS: [...admins].join(","),
    ACCESS_MODE: t.access,
    ANYONE_CAN_APPROVE: String(t.anyoneApprove),
    DISCORD_GUILD_IDS: [...guilds].join(","),
    BASE_PROJECT_DIR: readEnvValue(env, "BASE_PROJECT_DIR") ?? path.dirname(dir),
    HOST_NAME: readEnvValue(env, "HOST_NAME") || os.hostname(),
  };
  // Use the settings in memory first; the .env file is only written once Discord accepts them
  Object.assign(process.env, updates);
  loadConfig();
  const repo = await gitOrigin(dir);

  const joined = await withClient(t.botToken, async (client) => {
    const channel = (await client.channels.fetch(t.channelId).catch(() => null)) as TextChannel | null;
    if (!channel || !("send" in channel) || channel.guild.id !== t.guildId) {
      throw new Error("The bot can't see that channel. Is it in the server, and does it have access to the channel?");
    }
    const { roleId } = await addAgentToChannel(client, channel, persona, dir, repo);
    return { channelName: channel.name, guildName: channel.guild.name, roleId };
  });
  fs.writeFileSync(envPath, upsertEnv(env, updates), { mode: 0o600 });
  if (newDevice) setSetting("primary", "0");
  const started = ensureDaemon();

  console.log(`Joined workspace "${t.name}": #${joined.channelName} in ${joined.guildName}`);
  console.log(`Agent "${persona}" is working in ${dir}${joined.roleId ? ` (mention it with @claude · ${persona})` : ""}.`);
  console.log(started ? "Background service started." : "Background service already running.");
}

function installSkill(): void {
  const cli = `node "${path.join(appRoot(), "dist", "cli.js")}"`;
  const dir = path.join(os.homedir(), ".claude", "skills", "claudecord");
  fs.mkdirSync(dir, { recursive: true });
  fs.writeFileSync(path.join(dir, "SKILL.md"), skillMarkdown(cli));
  console.log(`Installed the claudecord skill in ${dir}. New Claude Code chats can now create or join workspaces.`);
}

function daemon(sub: string | undefined): void {
  if (sub === "start") {
    loadConfig();
    console.log(ensureDaemon() ? "Started." : "Already running.");
    return;
  }
  const pid = daemonPid();
  console.log(pid ? `Running (pid ${pid}).` : "Not running. Start it with: claudecord daemon start");
}

async function main(): Promise<void> {
  const [cmd, ...rest] = process.argv.slice(2);
  if (!cmd || cmd === "help" || cmd === "--help" || cmd === "-h") {
    console.log(HELP);
    return;
  }
  initDatabase();
  // Positional arguments are what's left after flags and the values of flags that take one
  const VALUE_FLAGS = new Set(["--name", "--dir", "--agent", "--guild"]);
  const positional = rest.filter((a, i) => !a.startsWith("--") && !VALUE_FLAGS.has(rest[i - 1] ?? ""));
  switch (cmd) {
    case "workspace":
      if (positional[0] === "create") return workspaceCreate(rest);
      if (positional[0] === "token") return workspaceToken(positional[1]);
      break;
    case "join":
      return join(positional[0], rest);
    case "daemon":
      return daemon(positional[0]);
    case "install-skill":
      return installSkill();
    case "list":
      return list();
    case "attach":
      return attach(positional[0], rest.includes("--force"));
    case "detach":
      return detach(positional[0]);
  }
  console.error(`Unknown command: ${cmd}\n\n${HELP}`);
  process.exit(1);
}

main().catch((e) => {
  console.error(e instanceof Error ? e.message : e);
  process.exit(1);
});
