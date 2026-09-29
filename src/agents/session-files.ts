import fs from "node:fs";
import os from "node:os";
import path from "node:path";

/** Claude Code stores transcripts under ~/.claude/projects/<path with non-alphanumerics as dashes>/<id>.jsonl */
export function transcriptDirFor(projectPath: string, home = os.homedir()): string {
  return path.join(home, ".claude", "projects", path.resolve(projectPath).replace(/[^a-zA-Z0-9]/g, "-"));
}

/** Id of the most recently written session for a project, or null if there is none. */
export function latestSessionId(projectPath: string, home = os.homedir()): string | null {
  const dir = transcriptDirFor(projectPath, home);
  try {
    const files = fs
      .readdirSync(dir)
      .filter((f) => f.endsWith(".jsonl"))
      .map((f) => ({ id: f.slice(0, -".jsonl".length), mtime: fs.statSync(path.join(dir, f)).mtimeMs }))
      .sort((a, b) => b.mtime - a.mtime);
    return files[0]?.id ?? null;
  } catch {
    return null;
  }
}
