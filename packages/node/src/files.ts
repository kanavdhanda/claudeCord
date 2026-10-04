import { createWriteStream, mkdirSync, realpathSync, statSync, writeFileSync, existsSync, rmSync } from "node:fs";
import { open, readFile } from "node:fs/promises";
import { basename, extname, isAbsolute, join, relative, resolve } from "node:path";
import { randomUUID } from "node:crypto";
import { FILE_CHUNK_BYTES, MAX_FILE_BYTES, findSecretsInFile } from "@claudecord/protocol";

export function safeName(name: string): string {
  const base = basename(name)
    .replace(/[\u0000-\u001f<>:"/\\|?*]/g, "_")
    .replace(/^\.+/, "_");
  return (base || "file").slice(0, 120);
}

/**
 * Files an agent must never send, even from inside the project. A prompt-injected agent can be told to upload
 * credentials, so the obvious carriers are refused outright. This is a denylist and cannot be complete.
 */
const SENSITIVE_NAMES = [
  /^\.env(\..*)?$/i,
  /^\.(npmrc|netrc|pypirc|git-credentials|htpasswd|pgpass|my\.cnf)$/i,
  /^id_(rsa|dsa|ecdsa|ed25519)(\.pub)?$/i,
  /^(credentials?|secrets?)(\.[a-z0-9]+)?$/i,
  /\.(pem|key|p12|pfx|jks|keystore|kdbx|ovpn|tfstate)$/i,
  /^terraform\.tfvars$/i,
  /^service[-_]?account.*\.json$/i,
];
const SENSITIVE_DIRS = new Set([".ssh", ".aws", ".gnupg", ".kube", ".claude-mesh"]);

export function isSensitivePath(path: string): boolean {
  const parts = path.split(/[\\/]+/).filter(Boolean);
  const name = parts[parts.length - 1] ?? "";
  if (SENSITIVE_NAMES.some((re) => re.test(name))) return true;
  return parts.slice(0, -1).some((p) => SENSITIVE_DIRS.has(p));
}

/**
 * Resolves p against cwd and refuses anything that escapes it, including through symlinks, and anything sensitive.
 * The native realpath returns the true case on case-insensitive file systems (macOS, Windows), so the comparison
 * cannot be fooled by a differently cased path.
 */
export function resolveInside(cwd: string, p: string): string {
  const root = realpathSync.native(cwd);
  const real = realpathSync.native(isAbsolute(p) ? p : resolve(root, p));
  const rel = relative(root, real);
  if (rel.startsWith("..") || isAbsolute(rel)) throw new Error("path is outside the project folder");
  if (isSensitivePath(rel)) throw new Error("refusing to send what looks like a credential or key file");
  return real;
}

export interface OutChunk {
  transferId: string;
  name: string;
  seq: number;
  last: boolean;
  data: string;
}

/** Reads the file in fixed-size chunks so large files never sit in memory whole. */
export async function* readChunks(path: string, maxBytes = MAX_FILE_BYTES): AsyncGenerator<OutChunk> {
  const st = statSync(path);
  if (!st.isFile()) throw new Error("not a regular file");
  if (st.size > maxBytes) throw new Error(`file is ${st.size} bytes, limit is ${maxBytes}`);
  // Checked here, on this device, so a secret never leaves it, not even to the hub. Content is what counts, so
  // renaming .env to notes.md does not get it through.
  const secrets = findSecretsInFile(await readFile(path));
  if (secrets.length) throw new Error(`refusing to send: the file appears to contain ${secrets.join(", ")}`);
  const transferId = randomUUID();
  const name = safeName(path);
  const fh = await open(path, "r");
  try {
    const buf = Buffer.alloc(FILE_CHUNK_BYTES);
    let seq = 0;
    let sent = 0;
    do {
      const { bytesRead } = await fh.read(buf, 0, FILE_CHUNK_BYTES, sent);
      sent += bytesRead;
      yield {
        transferId,
        name,
        seq: seq++,
        last: sent >= st.size,
        data: buf.subarray(0, bytesRead).toString("base64"),
      };
    } while (sent < st.size);
  } finally {
    await fh.close();
  }
}

interface Incoming {
  name: string;
  path: string;
  out: ReturnType<typeof createWriteStream>;
  bytes: number;
  next: number;
  at: number;
}

const MAX_ACTIVE_RECEIVES = 8;
const RECEIVE_TTL_MS = 60_000;

/** Reassembles chunks into <cwd>/.claudecord/inbox, one active transfer per id. */
export class FileReceiver {
  private active = new Map<string, Incoming>();

  constructor(private maxBytes = MAX_FILE_BYTES) {}

  /** Drops transfers that stalled, so open files and partial data do not pile up. */
  private prune(now = Date.now()): void {
    for (const [id, t] of this.active) {
      if (now - t.at > RECEIVE_TTL_MS) {
        t.out.destroy();
        rmSync(t.path, { force: true });
        this.active.delete(id);
      }
    }
  }

  async receive(
    cwd: string,
    c: { transferId: string; name: string; seq: number; last: boolean; data: string },
  ): Promise<{ path: string; bytes: number } | undefined> {
    this.prune();
    let t = this.active.get(c.transferId);
    if (!t) {
      if (c.seq !== 0 || this.active.size >= MAX_ACTIVE_RECEIVES) return undefined;
      const dir = join(cwd, ".claudecord", "inbox");
      mkdirSync(dir, { recursive: true, mode: 0o700 });
      const gi = join(cwd, ".claudecord", ".gitignore");
      if (!existsSync(gi)) writeFileSync(gi, "*\n");
      const name = uniqueName(dir, safeName(c.name));
      const path = join(dir, name);
      // Not executable, not readable by other users.
      t = { name, path, out: createWriteStream(path, { mode: 0o600 }), bytes: 0, next: 0, at: Date.now() };
      this.active.set(c.transferId, t);
    }
    t.at = Date.now();
    if (c.seq !== t.next) return this.abort(c.transferId);
    const buf = Buffer.from(c.data, "base64");
    t.bytes += buf.length;
    t.next++;
    if (t.bytes > this.maxBytes) return this.abort(c.transferId);
    await new Promise<void>((res, rej) => t!.out.write(buf, (e) => (e ? rej(e) : res())));
    if (!c.last) return undefined;
    await new Promise<void>((res) => t!.out.end(res));
    this.active.delete(c.transferId);
    return { path: t.path, bytes: t.bytes };
  }

  private abort(id: string): undefined {
    const t = this.active.get(id);
    if (t) {
      t.out.destroy();
      rmSync(t.path, { force: true });
      this.active.delete(id);
    }
    return undefined;
  }
}

function uniqueName(dir: string, name: string): string {
  if (!existsSync(join(dir, name))) return name;
  const ext = extname(name);
  const stem = name.slice(0, name.length - ext.length);
  for (let i = 1; ; i++) {
    const n = `${stem}-${i}${ext}`;
    if (!existsSync(join(dir, n))) return n;
  }
}
