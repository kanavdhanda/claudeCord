import { chmodSync, existsSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { meshDir } from "@claudecord/agent-tools";
import type { AdapterId } from "@claudecord/protocol";

export type Policy = "autonomous" | "plan" | "ask";

export interface NodeConfig {
  hubUrl: string;
  token: string;
  nodeName: string;
  adapter: AdapterId;
  model?: string;
  role?: string;
  policy: Policy;
  /** scrub (default) removes secret-looking variables from agents. inherit passes everything through. */
  envPolicy?: "scrub" | "inherit";
}

export interface ProjectConfig {
  project: string;
  adapter?: AdapterId;
  model?: string;
  role?: string;
  policy?: Policy;
}

const PROJECT_FILE = ".claudecord.json";

export function configPath(): string {
  return join(meshDir(), "config.json");
}

export function loadNodeConfig(): NodeConfig {
  if (!existsSync(configPath())) throw new Error("Not set up yet. Run: npx claudecord init");
  return JSON.parse(readFileSync(configPath(), "utf8")) as NodeConfig;
}

/** The config holds the device token, so the directory and file are private to this user. */
export function ensureMeshDir(): void {
  mkdirSync(meshDir(), { recursive: true, mode: 0o700 });
  if (process.platform !== "win32") chmodSync(meshDir(), 0o700);
}

export function saveNodeConfig(c: NodeConfig): void {
  ensureMeshDir();
  writeFileSync(configPath(), JSON.stringify(c, null, 2), { mode: 0o600 });
  if (process.platform !== "win32") chmodSync(configPath(), 0o600);
}

/** Turns a folder name into a valid project name. */
export function projectSlug(name: string): string {
  const s = name.replace(/[^A-Za-z0-9._-]+/g, "-").replace(/^[^A-Za-z0-9]+/, "").replace(/-+$/, "").slice(0, 64);
  return s || "project";
}

export function loadProjectConfig(dir: string): ProjectConfig | undefined {
  const p = join(dir, PROJECT_FILE);
  return existsSync(p) ? (JSON.parse(readFileSync(p, "utf8")) as ProjectConfig) : undefined;
}

export function saveProjectConfig(dir: string, c: ProjectConfig): void {
  writeFileSync(join(dir, PROJECT_FILE), JSON.stringify(c, null, 2) + "\n");
}

/** Remembers where each project lives on this device so the hub can spawn into it. */
export function loadProjectDirs(): Record<string, string> {
  const p = join(meshDir(), "projects.json");
  return existsSync(p) ? (JSON.parse(readFileSync(p, "utf8")) as Record<string, string>) : {};
}

export function saveProjectDir(project: string, dir: string): void {
  ensureMeshDir();
  writeFileSync(join(meshDir(), "projects.json"), JSON.stringify({ ...loadProjectDirs(), [project]: dir }, null, 2));
}
