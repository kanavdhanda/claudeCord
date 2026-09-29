import { createWriteStream, mkdirSync, realpathSync, statSync, writeFileSync, existsSync } from "node:fs";
import { open } from "node:fs/promises";
import { basename, extname, isAbsolute, join, relative, resolve } from "node:path";
import { randomUUID } from "node:crypto";
import { FILE_CHUNK_BYTES, MAX_FILE_BYTES } from "@claudecord/protocol";

export function safeName(name: string): string {
  const base = basename(name).replace(/[\u0000-\u001f<>:"/\\|?*]/g, "_").replace(/^\.+/, "_");
  return (base || "file").slice(0, 120);
}

/** Resolves p against cwd and refuses anything that escapes it, including through symlinks. */
export function resolveInside(cwd: string, p: string): string {
  const root = realpathSync(cwd);
  const real = realpathSync(isAbsolute(p) ? p : resolve(root, p));
  const rel = relative(root, real);
  if (rel.startsWith("..") || isAbsolute(rel)) throw new Error("path is outside the project folder");
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
      yield { transferId, name, seq: seq++, last: sent >= st.size, data: buf.subarray(0, bytesRead).toString("base64") };
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
}

/** Reassembles chunks into <cwd>/.claudecord/inbox, one active transfer per id. */
export class FileReceiver {
  private active = new Map<string, Incoming>();

  constructor(private maxBytes = MAX_FILE_BYTES) {}

  async receive(
    cwd: string,
    c: { transferId: string; name: string; seq: number; last: boolean; data: string },
  ): Promise<{ path: string; bytes: number } | undefined> {
    let t = this.active.get(c.transferId);
    if (!t) {
      if (c.seq !== 0) return undefined;
      const dir = join(cwd, ".claudecord", "inbox");
      mkdirSync(dir, { recursive: true });
      const gi = join(cwd, ".claudecord", ".gitignore");
      if (!existsSync(gi)) writeFileSync(gi, "*\n");
      const name = uniqueName(dir, safeName(c.name));
      const path = join(dir, name);
      t = { name, path, out: createWriteStream(path), bytes: 0, next: 0 };
      this.active.set(c.transferId, t);
    }
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
