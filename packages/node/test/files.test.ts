import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { mkdirSync, mkdtempSync, readFileSync, rmSync, symlinkSync, writeFileSync, existsSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { FileReceiver, readChunks, resolveInside, safeName } from "../src/files.js";

let root: string;
let outside: string;

beforeEach(() => {
  root = mkdtempSync(join(tmpdir(), "cc-proj-"));
  outside = mkdtempSync(join(tmpdir(), "cc-out-"));
});
afterEach(() => {
  rmSync(root, { recursive: true, force: true });
  rmSync(outside, { recursive: true, force: true });
});

describe("path safety", () => {
  it("allows files inside the project and refuses escapes", () => {
    mkdirSync(join(root, "src"));
    writeFileSync(join(root, "src", "a.txt"), "x");
    writeFileSync(join(outside, "secret"), "s");
    expect(resolveInside(root, "src/a.txt")).toContain("a.txt");
    expect(() => resolveInside(root, "../" + outside.split("/").pop() + "/secret")).toThrow(/outside/);
    expect(() => resolveInside(root, join(outside, "secret"))).toThrow(/outside/);
  });

  it("refuses a symlink that points out of the project", () => {
    writeFileSync(join(outside, "secret"), "s");
    symlinkSync(join(outside, "secret"), join(root, "link"));
    expect(() => resolveInside(root, "link")).toThrow(/outside/);
  });

  it("sanitises incoming names", () => {
    expect(safeName("../../etc/passwd")).toBe("passwd");
    expect(safeName(".bashrc")).toBe("_bashrc");
    expect(safeName("a:b*c.txt")).toBe("a_b_c.txt");
  });
});

describe("chunked round trip", () => {
  it("reassembles a multi-chunk file byte for byte into the inbox", async () => {
    const data = Buffer.from(Array.from({ length: 450 * 1024 }, (_, i) => i % 251));
    const src = join(root, "big.bin");
    writeFileSync(src, data);
    const recv = new FileReceiver();
    const dest = mkdtempSync(join(tmpdir(), "cc-dest-"));
    let result;
    let n = 0;
    for await (const c of readChunks(src)) {
      n++;
      result = (await recv.receive(dest, c)) ?? result;
    }
    expect(n).toBe(3);
    expect(result!.bytes).toBe(data.length);
    expect(readFileSync(result!.path).equals(data)).toBe(true);
    expect(result!.path).toContain(join(".claudecord", "inbox"));
    expect(existsSync(join(dest, ".claudecord", ".gitignore"))).toBe(true);
    rmSync(dest, { recursive: true, force: true });
  });

  it("handles an empty file", async () => {
    const src = join(root, "empty");
    writeFileSync(src, "");
    const recv = new FileReceiver();
    let result;
    for await (const c of readChunks(src)) result = await recv.receive(root, c);
    expect(result!.bytes).toBe(0);
  });

  it("does not overwrite an existing inbox file", async () => {
    const src = join(root, "a.txt");
    writeFileSync(src, "one");
    const recv = new FileReceiver();
    const paths: string[] = [];
    for (let i = 0; i < 2; i++) for await (const c of readChunks(src)) paths.push((await recv.receive(root, c))?.path ?? "");
    const done = paths.filter(Boolean);
    expect(done[0]).not.toBe(done[1]);
    expect(done[1]).toContain("a-1.txt");
  });

  it("drops out-of-order chunks and oversize transfers", async () => {
    const recv = new FileReceiver(10);
    const ok = (seq: number, last: boolean, d = "aaaa") =>
      recv.receive(root, { transferId: "x", name: "f", seq, last, data: Buffer.from(d).toString("base64") });
    expect(await ok(0, false)).toBeUndefined();
    expect(await ok(2, true)).toBeUndefined(); // gap, aborted
    expect(await ok(1, true)).toBeUndefined(); // transfer is gone
    const big = (seq: number, last: boolean) =>
      recv.receive(root, { transferId: "y", name: "g", seq, last, data: Buffer.alloc(8).toString("base64") });
    await big(0, false);
    expect(await big(1, true)).toBeUndefined();
  });

  it("refuses to send files over the limit", async () => {
    const src = join(root, "big");
    writeFileSync(src, Buffer.alloc(100));
    await expect(async () => {
      for await (const _ of readChunks(src, 50)) void _;
    }).rejects.toThrow(/limit/);
  });
});
