import { describe, it, expect } from "vitest";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import crypto from "node:crypto";
import { parsePartName, sha256File, splitFile, storePart } from "./chunks.js";

const tmp = () => fs.mkdtempSync(path.join(os.tmpdir(), "chunks-"));

describe("file chunking", () => {
  it("keeps small files whole", () => {
    const d = tmp();
    const f = path.join(d, "a.txt");
    fs.writeFileSync(f, "hello");
    const parts = splitFile(f, 100);
    expect(parts).toHaveLength(1);
    expect(parts[0].name).toBe("a.txt");
  });

  it("splits, reassembles out of order, and verifies the hash", () => {
    const d = tmp();
    const f = path.join(d, "big.bin");
    const content = crypto.randomBytes(2500);
    fs.writeFileSync(f, content);
    const sha = sha256File(f);
    const parts = splitFile(f, 1000);
    expect(parts.map((p) => p.name)).toEqual(["big.bin.part001of003", "big.bin.part002of003", "big.bin.part003of003"]);

    const dest = tmp();
    expect(storePart(dest, parts[2].name, parts[2].data, sha)).toMatchObject({ complete: false, received: 1 });
    expect(storePart(dest, parts[0].name, parts[0].data, sha)).toMatchObject({ complete: false, received: 2 });
    const done = storePart(dest, parts[1].name, parts[1].data, sha)!;
    expect(done.complete).toBe(true);
    expect(done.hashOk).toBe(true);
    expect(fs.readFileSync(done.joinedPath!).equals(content)).toBe(true);
  });

  it("flags a hash mismatch", () => {
    const d = tmp();
    const f = path.join(d, "x.bin");
    fs.writeFileSync(f, Buffer.alloc(30, 1));
    const parts = splitFile(f, 20);
    const dest = tmp();
    storePart(dest, parts[0].name, parts[0].data, "deadbeef");
    expect(storePart(dest, parts[1].name, parts[1].data, "deadbeef")?.hashOk).toBe(false);
  });

  it("rejects malformed part names and path tricks", () => {
    expect(parsePartName("a.txt")).toBeNull();
    expect(parsePartName("a.part009of003")).toBeNull();
    expect(parsePartName("../../evil.part001of002")?.base).toBe("evil");
  });
});
