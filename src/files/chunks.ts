import crypto from "node:crypto";
import fs from "node:fs";
import path from "node:path";

// Splitting and reassembling files that exceed Discord's per-file upload limit.
// Part names look like `report.zip.part002of005`.

export const PART_RE = /^(.*)\.part(\d{3,})of(\d{3,})$/;
export const MAX_TOTAL_BYTES = 200 * 1024 * 1024;

export interface FilePart {
  name: string;
  data: Buffer;
}

export function sha256File(filePath: string): string {
  const hash = crypto.createHash("sha256");
  const fd = fs.openSync(filePath, "r");
  try {
    const buf = Buffer.alloc(1024 * 1024);
    let n: number;
    while ((n = fs.readSync(fd, buf, 0, buf.length, null)) > 0) hash.update(buf.subarray(0, n));
  } finally {
    fs.closeSync(fd);
  }
  return hash.digest("hex");
}

/** Returns one part for small files, or several numbered parts for large ones. */
export function splitFile(filePath: string, maxBytes: number): FilePart[] {
  const size = fs.statSync(filePath).size;
  if (size > MAX_TOTAL_BYTES) throw new Error(`File is ${(size / 1048576).toFixed(0)} MB; the limit is ${MAX_TOTAL_BYTES / 1048576} MB`);
  const name = path.basename(filePath);
  const data = fs.readFileSync(filePath);
  if (size <= maxBytes) return [{ name, data }];
  const total = Math.ceil(size / maxBytes);
  const pad = Math.max(3, String(total).length);
  const parts: FilePart[] = [];
  for (let i = 0; i < total; i++) {
    parts.push({
      name: `${name}.part${String(i + 1).padStart(pad, "0")}of${String(total).padStart(pad, "0")}`,
      data: data.subarray(i * maxBytes, (i + 1) * maxBytes),
    });
  }
  return parts;
}

export function parsePartName(fileName: string): { base: string; index: number; total: number } | null {
  const m = fileName.match(PART_RE);
  if (!m) return null;
  const index = Number(m[2]);
  const total = Number(m[3]);
  if (index < 1 || index > total || total > 5000) return null;
  return { base: path.basename(m[1]), index, total };
}

/**
 * Store a received part; when every part is present, join them into `<destDir>/<base>`.
 * Returns the joined file path once complete, otherwise null.
 */
export function storePart(
  destDir: string,
  fileName: string,
  data: Buffer,
  expectedSha256?: string,
): { complete: boolean; joinedPath?: string; received: number; total: number; hashOk?: boolean } | null {
  const info = parsePartName(fileName);
  if (!info) return null;
  const partsDir = path.join(destDir, ".parts", info.base);
  fs.mkdirSync(partsDir, { recursive: true });
  // The hash may arrive with an earlier message than the last part; remember it
  const shaFile = path.join(destDir, ".parts", `${info.base}.sha256`);
  if (expectedSha256) fs.writeFileSync(shaFile, expectedSha256);
  else if (fs.existsSync(shaFile)) expectedSha256 = fs.readFileSync(shaFile, "utf-8").trim();
  fs.writeFileSync(path.join(partsDir, String(info.index).padStart(6, "0")), data);
  const have = fs.readdirSync(partsDir).length;
  if (have < info.total) return { complete: false, received: have, total: info.total };

  const joinedPath = path.join(destDir, info.base);
  const chunks: Buffer[] = [];
  for (let i = 1; i <= info.total; i++) chunks.push(fs.readFileSync(path.join(partsDir, String(i).padStart(6, "0"))));
  fs.writeFileSync(joinedPath, Buffer.concat(chunks));
  fs.rmSync(partsDir, { recursive: true, force: true });
  fs.rmSync(shaFile, { force: true });
  const hashOk = expectedSha256 ? sha256File(joinedPath) === expectedSha256.toLowerCase() : undefined;
  return { complete: true, joinedPath, received: info.total, total: info.total, hashOk };
}
