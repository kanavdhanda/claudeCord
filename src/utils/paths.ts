import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

/** Folder containing package.json, found by walking up from this file (works from src/ and dist/). */
export function appRoot(): string {
  if (process.env.CLAUDECORD_HOME) return process.env.CLAUDECORD_HOME;
  let dir = path.dirname(fileURLToPath(import.meta.url));
  for (let i = 0; i < 6; i++) {
    if (fs.existsSync(path.join(dir, "package.json"))) return dir;
    dir = path.dirname(dir);
  }
  return process.cwd();
}

/** Database location: override with CLAUDECORD_DB; otherwise data.db in the app folder. */
export function dbPath(): string {
  return process.env.CLAUDECORD_DB ?? path.join(appRoot(), "data.db");
}
