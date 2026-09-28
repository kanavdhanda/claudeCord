import { execFile } from "node:child_process";
import fs from "node:fs";
import path from "node:path";
import type { RepoRef } from "../utils/repo.js";

export interface CloneResult {
  ok: boolean;
  path: string;
  message: string;
}

/** Clone a public GitHub repo into baseDir/<repo>. Uses argv, never a shell. */
export function cloneRepo(repo: RepoRef, baseDir: string): Promise<CloneResult> {
  const dest = path.join(baseDir, repo.dirName);
  if (fs.existsSync(dest)) {
    return Promise.resolve({ ok: true, path: dest, message: "Folder already exists; using it as-is." });
  }
  fs.mkdirSync(baseDir, { recursive: true });
  return new Promise((resolve) => {
    execFile(
      "git",
      ["clone", "--depth", "50", "--", repo.cloneUrl, dest],
      // Public repos only: never prompt for credentials
      { timeout: 5 * 60_000, env: { ...process.env, GIT_TERMINAL_PROMPT: "0" } },
      (err, _stdout, stderr) => {
        if (err) {
          fs.rmSync(dest, { recursive: true, force: true });
          const detail = String(stderr || err.message).trim().split("\n").slice(-2).join(" ");
          resolve({ ok: false, path: dest, message: `git clone failed: ${detail}` });
        } else {
          resolve({ ok: true, path: dest, message: "Cloned." });
        }
      },
    );
  });
}
