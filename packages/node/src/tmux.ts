import { execFile, spawn } from "node:child_process";
import { withScrubbedEnv } from "./env.js";
import { stripControl } from "./text.js";

/** Set CLAUDECORD_TMUX_SOCKET to keep agents in their own tmux server, apart from your normal sessions. */
function socketArgs(): string[] {
  const sock = process.env.CLAUDECORD_TMUX_SOCKET;
  return sock ? ["-L", sock] : [];
}

function run(args: string[], input?: string): Promise<string> {
  return new Promise((resolve, reject) => {
    const p = execFile("tmux", [...socketArgs(), ...args], { maxBuffer: 8 * 1024 * 1024 }, (err, stdout, stderr) => {
      if (err) reject(new Error(`tmux ${args[0]}: ${stderr.trim() || err.message}`));
      else resolve(stdout);
    });
    if (input !== undefined) p.stdin?.end(input);
  });
}

export interface PaneOpts {
  session: string;
  cwd: string;
  title: string;
  argv: string[];
  env: Record<string, string>;
  /** Environment variables to remove from the agent's process. */
  unset?: string[];
}

/** Names of variables in the tmux server's own environment, which a new pane also inherits. */
export async function serverEnvNames(): Promise<string[]> {
  try {
    const out = await run(["show-environment", "-g"]);
    return out.split("\n").map((l) => l.replace(/^-/, "").split("=")[0]!).filter(Boolean);
  } catch {
    return [];
  }
}

export async function hasTmux(): Promise<boolean> {
  try {
    await run(["-V"]);
    return true;
  } catch {
    return false;
  }
}

async function hasSession(session: string): Promise<boolean> {
  try {
    await run(["has-session", "-t", `=${session}`]);
    return true;
  } catch {
    return false;
  }
}

function envArgs(env: Record<string, string>): string[] {
  return Object.entries(env).flatMap(([k, v]) => ["-e", `${k}=${v}`]);
}

/** Creates the session if needed, otherwise adds a tiled pane. Returns the pane id. */
export async function openPane(o: PaneOpts): Promise<string> {
  let paneId: string;
  if (await hasSession(o.session)) {
    paneId = (
      await run(["split-window", "-t", `=${o.session}:`, "-c", o.cwd, "-P", "-F", "#{pane_id}", ...envArgs(o.env), ...withScrubbedEnv(o.argv, o.unset ?? [])])
    ).trim();
    await run(["select-layout", "-t", `=${o.session}:`, "tiled"]);
  } else {
    paneId = (
      await run(["new-session", "-d", "-s", o.session, "-c", o.cwd, "-x", "220", "-y", "55", "-P", "-F", "#{pane_id}", ...envArgs(o.env), ...withScrubbedEnv(o.argv, o.unset ?? [])])
    ).trim();
    await run(["set-option", "-t", `=${o.session}`, "pane-border-status", "top"]).catch(() => {});
    await run(["set-option", "-t", `=${o.session}`, "pane-border-format", " #{pane_title} "]).catch(() => {});
  }
  await run(["select-pane", "-t", paneId, "-T", o.title]).catch(() => {});
  return paneId;
}

/**
 * Captures the visible pane only. Scrollback would include prompts that were already answered and screens the
 * agent has since cleared, and the watcher would mistake those for the live state.
 */
export async function capture(paneId: string): Promise<string> {
  return run(["capture-pane", "-p", "-t", paneId]);
}

export async function paneAlive(paneId: string): Promise<boolean> {
  try {
    const out = await run(["display-message", "-p", "-t", paneId, "#{pane_dead}"]);
    return out.trim() === "0";
  } catch {
    return false;
  }
}

/** Bracketed paste so multi-line text lands as one input, then Enter. */
export async function pasteAndSubmit(paneId: string, text: string): Promise<void> {
  const buf = `cc-${paneId.replace("%", "")}`;
  // Last line of defence: nothing with control characters ever reaches the terminal.
  await run(["load-buffer", "-b", buf, "-"], stripControl(text));
  // -r keeps line feeds as they are. Without it tmux turns them into carriage returns, which can submit mid-message.
  await run(["paste-buffer", "-b", buf, "-t", paneId, "-d", "-p", "-r"]);
  await new Promise((r) => setTimeout(r, 200));
  await run(["send-keys", "-t", paneId, "Enter"]);
}

export async function sendKeys(paneId: string, keys: string[]): Promise<void> {
  for (const k of keys) {
    await run(["send-keys", "-t", paneId, k]);
    await new Promise((r) => setTimeout(r, 60));
  }
}

export async function killPane(paneId: string): Promise<void> {
  await run(["kill-pane", "-t", paneId]).catch(() => {});
}

export async function killSession(session: string): Promise<void> {
  await run(["kill-session", "-t", `=${session}`]).catch(() => {});
}

export function attach(session: string): void {
  spawn("tmux", [...socketArgs(), "attach", "-t", `=${session}`], { stdio: "inherit" });
}
