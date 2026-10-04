import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { Auth, formatCode, normalizeCode } from "../src/auth.js";
import { Db } from "../src/db.js";

let db: Db;
let auth: Auth;
beforeEach(() => {
  db = new Db(":memory:");
  auth = new Auth(db);
});
afterEach(() => vi.useRealTimers());

describe("pairing codes", () => {
  it("makes a short code from an unambiguous alphabet", () => {
    for (let i = 0; i < 200; i++) {
      const { code } = auth.createPairCode();
      expect(code).toMatch(/^[A-HJKMNP-Z2-9]{4}-[A-HJKMNP-Z2-9]{4}$/);
    }
  });

  it("does not repeat codes", () => {
    const seen = new Set(Array.from({ length: 500 }, () => auth.createPairCode().code));
    expect(seen.size).toBe(500);
  });

  it("uses every symbol about equally, so the code has its full strength", () => {
    const counts = new Map<string, number>();
    for (let i = 0; i < 4000; i++)
      for (const ch of auth.createPairCode().code.replace("-", "")) counts.set(ch, (counts.get(ch) ?? 0) + 1);
    const values = [...counts.values()];
    expect(counts.size).toBe(31);
    // 32000 symbols over 31 values is about 1032 each. Modulo bias would put the first few near 1100.
    expect(Math.max(...values) / Math.min(...values)).toBeLessThan(1.25);
  });

  it("is redeemed once, for a device token that then authenticates", () => {
    const { code } = auth.createPairCode();
    const out = auth.redeemPairCode(code, "laptop")!;
    expect(out.device).toBe("laptop");
    expect(db.nodeForToken(out.token)).toBe("laptop");
    expect(auth.redeemPairCode(code, "laptop")).toBeNull();
  });

  it("accepts the code typed in any case, with spaces or no dash", () => {
    const { code } = auth.createPairCode();
    expect(auth.redeemPairCode(` ${code.toLowerCase().replace("-", "  ")} `, "a")).not.toBeNull();
    expect(normalizeCode("abcd-2345")).toBe("ABCD2345");
    expect(formatCode("ABCD2345")).toBe("ABCD-2345");
  });

  it("rejects wrong, short and empty codes without spending a real one", () => {
    const { code } = auth.createPairCode();
    for (const bad of ["", "ABCD", "AAAA-AAAA", "0000-0000", code.slice(0, 5)])
      expect(auth.redeemPairCode(bad, "a")).toBeNull();
    expect(auth.redeemPairCode(code, "a")).not.toBeNull();
  });

  it("expires after ten minutes", () => {
    vi.useFakeTimers();
    const { code } = auth.createPairCode();
    vi.advanceTimersByTime(10 * 60_000 + 1);
    expect(auth.redeemPairCode(code, "a")).toBeNull();
  });

  it("is still valid just before it expires", () => {
    vi.useFakeTimers();
    const { code } = auth.createPairCode();
    vi.advanceTimersByTime(9 * 60_000);
    expect(auth.redeemPairCode(code, "a")).not.toBeNull();
  });

  it("only keeps a hash, so the database never holds a usable code", () => {
    const { code } = auth.createPairCode();
    const stored = JSON.stringify(
      (db as unknown as { db: { prepare: (q: string) => { all: () => unknown[] } } }).db
        .prepare("SELECT * FROM pair_codes")
        .all(),
    );
    expect(stored).not.toContain(code.replace("-", ""));
  });

  it("falls back to a safe device name and never replaces another device's token", () => {
    const a = auth.redeemPairCode(auth.createPairCode().code, "Kanavs MacBook!!")!;
    expect(a.device).toBe("device");
    const b = auth.redeemPairCode(auth.createPairCode().code, "device")!;
    expect(b.device).toBe("device-2");
    expect(db.nodeForToken(a.token)).toBe("device");
    expect(db.nodeForToken(b.token)).toBe("device-2");
  });
});

describe("dashboard sign in", () => {
  it("turns a one-time link token into a session, once", () => {
    const { token } = auth.createLoginToken();
    const session = auth.redeemLoginToken(token)!;
    expect(auth.verifySession(session)).toBe(true);
    expect(auth.redeemLoginToken(token)).toBeNull();
  });

  it("rejects unknown tokens and unknown sessions", () => {
    expect(auth.redeemLoginToken("nope")).toBeNull();
    expect(auth.verifySession("nope")).toBe(false);
    expect(auth.verifySession(undefined)).toBe(false);
  });

  it("ends a session on sign out", () => {
    const session = auth.redeemLoginToken(auth.createLoginToken().token)!;
    auth.endSession(session);
    expect(auth.verifySession(session)).toBe(false);
  });

  it("expires links after ten minutes and sessions after twelve hours", () => {
    vi.useFakeTimers();
    const stale = auth.createLoginToken().token;
    const session = auth.redeemLoginToken(auth.createLoginToken().token)!;
    vi.advanceTimersByTime(10 * 60_000 + 1);
    expect(auth.redeemLoginToken(stale)).toBeNull();
    expect(auth.verifySession(session)).toBe(true);
    vi.advanceTimersByTime(12 * 60 * 60_000);
    expect(auth.verifySession(session)).toBe(false);
  });

  it("prunes what has expired", () => {
    vi.useFakeTimers();
    auth.createPairCode();
    auth.createLoginToken();
    vi.advanceTimersByTime(13 * 60 * 60_000);
    db.pruneExpired();
    const rows = (t: string) =>
      (db as unknown as { db: { prepare: (q: string) => { all: () => unknown[] } } }).db
        .prepare(`SELECT * FROM ${t}`)
        .all();
    expect(rows("pair_codes")).toHaveLength(0);
    expect(rows("login_tokens")).toHaveLength(0);
  });
});
