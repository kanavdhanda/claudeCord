import { execFile, spawn } from "node:child_process";

function run(args: string[], input?: string): Promise<string> {
  return new Promise((resolve, reject) => {
    const p = execFile("tmux", args, { maxBuffer: 8 * 1024 * 1024 }, (err, stdout, stderr) => {
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
      await run(["split-window", "-t", `=${o.session}`, "-c", o.cwd, "-P", "-F", "#{pane_id}", ...envArgs(o.env), ...o.argv])
    ).trim();
    await run(["select-layout", "-t", `=${o.session}`, "tiled"]);
  } else {
    paneId = (
      await run(["new-session", "-d", "-s", o.session, "-c", o.cwd, "-x", "220", "-y", "55", "-P", "-F", "#{pane_id}", ...envArgs(o.env), ...o.argv])
    ).trim();
    await run(["set-option", "-t", `=${o.session}`, "pane-border-status", "top"]).catch(() => {});
    await run(["set-option", "-t", `=${o.session}`, "pane-border-format", " #{pane_title} "]).catch(() => {});
  }
  await run(["select-pane", "-t", paneId, "-T", o.title]).catch(() => {});
  return paneId;
}

export async function capture(paneId: string, lines = 80): Promise<string> {
  return run(["capture-pane", "-p", "-t", paneId, "-S", `-${lines}`]);
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
  await run(["load-buffer", "-b", buf, "-"], text);
  await run(["paste-buffer", "-b", buf, "-t", paneId, "-d", "-p"]);
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
  spawn("tmux", ["attach", "-t", `=${session}`], { stdio: "inherit" });
}
