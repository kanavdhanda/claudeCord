import fs from "node:fs";
import path from "node:path";
import type { Project } from "../db/types.js";
import { holdIsLive } from "./identity.js";
import { appRoot } from "../utils/paths.js";

function pidAlive(pid: number): boolean {
  try {
    process.kill(pid, 0);
    return true;
  } catch (e) {
    return (e as NodeJS.ErrnoException).code === "EPERM";
  }
}

function relayPath(agentName: string): string {
  return path.join(appRoot(), `.relay-${agentName}`);
}

/** Append a message to the relay file so an attached terminal can pick it up. */
export function writeRelay(agentName: string, text: string): void {
  fs.appendFileSync(relayPath(agentName), JSON.stringify({ ts: Date.now(), text }) + "\n", "utf8");
}

/** Read all pending relay messages and clear the file. Returns texts in order. */
export function readAndClearRelay(agentName: string): string[] {
  const p = relayPath(agentName);
  if (!fs.existsSync(p)) return [];
  const raw = fs.readFileSync(p, "utf8").trim();
  fs.writeFileSync(p, "", "utf8");
  if (!raw) return [];
  return raw
    .split("\n")
    .filter(Boolean)
    .map((line) => {
      try {
        return (JSON.parse(line) as { text: string }).text;
      } catch {
        return line;
      }
    });
}

/** True while a terminal session (`claudecord attach`) has taken over this agent. */
export function isTerminalHeld(project: Project): boolean {
  return holdIsLive(
    project.terminal_pid !== null && project.terminal_since !== null ? { pid: project.terminal_pid, since: project.terminal_since } : null,
    pidAlive,
  );
}
