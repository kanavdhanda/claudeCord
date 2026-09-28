import { spawn } from "node:child_process";
import fs from "node:fs";
import path from "node:path";
import { appRoot } from "../utils/paths.js";

export const lockFile = () => path.join(appRoot(), ".bot.lock");

/** Pid of the running daemon on this device, or null. */
export function daemonPid(): number | null {
  try {
    const pid = parseInt(fs.readFileSync(lockFile(), "utf-8").trim(), 10);
    process.kill(pid, 0);
    return pid;
  } catch {
    return null;
  }
}

/** Start the daemon in the background if it isn't running. Returns true if it started one. */
export function ensureDaemon(): boolean {
  if (daemonPid() !== null) return false;
  const entry = path.join(appRoot(), "dist", "index.js");
  if (!fs.existsSync(entry)) throw new Error(`The daemon isn't built yet. Run \`npm run build\` in ${appRoot()}.`);
  const log = fs.openSync(path.join(appRoot(), "bot.log"), "a");
  const child = spawn(process.execPath, [entry], { cwd: appRoot(), detached: true, stdio: ["ignore", log, log] });
  child.unref();
  return true;
}
