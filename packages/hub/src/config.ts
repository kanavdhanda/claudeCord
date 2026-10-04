/**
 * Hub configuration lives in one private file, never in environment variables. Environment variables leak into child
 * processes, crash dumps, process listings, container inspect output and CI logs. A file with mode 0600 does not, and
 * the hub refuses to read one that other users can read.
 */
import { chmodSync, existsSync, mkdirSync, readFileSync, statSync, writeFileSync } from "node:fs";
import { homedir } from "node:os";
import { dirname, join } from "node:path";
import { z } from "zod";

const Snowflake = z.string().regex(/^\d{17,20}$/, "must be a Discord ID: 17 to 20 digits");

const isLocal = (u: URL) => ["localhost", "127.0.0.1", "[::1]"].includes(u.hostname);

export const HubConfigSchema = z.object({
  discordToken: z.string().min(50, "this does not look like a Discord bot token"),
  guildId: Snowflake,
  ownerId: Snowflake,
  /** Where devices and the dashboard reach this hub. Must be https unless it is localhost. */
  publicUrl: z
    .string()
    .url()
    .refine((u) => {
      // Zod runs this even when the string is not a URL at all, so it must not throw.
      try {
        const url = new URL(u);
        return url.protocol === "https:" || (url.protocol === "http:" && isLocal(url));
      } catch {
        return false;
      }
    }, "must be an https:// URL, or http://localhost for local use"),
  categoryName: z.string().min(1).max(90).default("claudecord"),
  port: z.number().int().min(0).max(65535).default(8787),
  dbPath: z.string().min(1),
  /** Optional avatar URL template with {name}. Off by default so no third party sees agent names. */
  avatarTemplate: z.string().url().optional(),
});

export type HubConfig = z.infer<typeof HubConfigSchema>;

export function defaultDir(): string {
  return join(homedir(), ".claudecord");
}

export function defaultConfigPath(): string {
  return join(defaultDir(), "hub.json");
}

export function defaultDbPath(): string {
  return join(defaultDir(), "hub.db");
}

/** Throws with the command that fixes it when the file is readable by anyone but its owner. */
export function assertPrivate(path: string): void {
  if (process.platform === "win32") return;
  const mode = statSync(path).mode & 0o777;
  if (mode & 0o077) {
    throw new Error(`${path} is readable by other users (mode ${mode.toString(8)}). Fix it with: chmod 600 ${path}`);
  }
}

export function loadConfig(path = defaultConfigPath()): HubConfig {
  if (!existsSync(path)) throw new Error(`No hub config at ${path}. Create it with: claudecord-hub setup`);
  assertPrivate(path);
  let raw: unknown;
  try {
    raw = JSON.parse(readFileSync(path, "utf8"));
  } catch {
    throw new Error(`${path} is not valid JSON. Re-create it with: claudecord-hub setup`);
  }
  const r = HubConfigSchema.safeParse(raw);
  if (!r.success) {
    const problems = r.error.issues.map((i) => `  ${i.path.join(".") || "config"}: ${i.message}`).join("\n");
    throw new Error(`${path} has problems:\n${problems}\nFix it, or re-create it with: claudecord-hub setup`);
  }
  return r.data;
}

export function saveConfig(path: string, cfg: HubConfig): void {
  mkdirSync(dirname(path), { recursive: true, mode: 0o700 });
  if (process.platform !== "win32") chmodSync(dirname(path), 0o700);
  // Create it private from the start, so there is no window where it is world readable.
  writeFileSync(path, JSON.stringify(cfg, null, 2) + "\n", { mode: 0o600 });
  if (process.platform !== "win32") chmodSync(path, 0o600);
}

/** The token with everything but its first and last characters hidden, for display. */
export function maskToken(t: string): string {
  return t.length <= 8 ? "****" : `${t.slice(0, 4)}...${t.slice(-4)}`;
}
