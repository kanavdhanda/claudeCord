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

describe("browser login for a command line", () => {
  const start = (o: { device?: string; folder?: string; trusted?: boolean } = {}) =>
    auth.startLogin({ device: o.device ?? "laptop", folder: o.folder, trusted: o.trusted ?? true });

  it("gives the command line a secret to poll with and the browser a short code", () => {
    const s = start({ folder: "my-app" });
    expect(s.code).toMatch(/^[A-HJKMNP-Z2-9]{4}-[A-HJKMNP-Z2-9]{4}$/);
    expect(s.pollToken.length).toBeGreaterThanOrEqual(30);
    expect(s.pollToken).not.toContain(s.code.replace("-", ""));
    expect(auth.describeLogin(s.code)).toEqual({
      device: "laptop",
      folder: "my-app",
      trusted: true,
      state: "pending",
      claimed: false,
    });
  });

  it("finds a session by the code typed any way", () => {
    const s = start();
    expect(auth.describeLogin(s.code.toLowerCase().replace("-", " "))).not.toBeNull();
    expect(auth.describeLogin("AAAA-AAAA")).toBeNull();
  });

  it("only keeps names that are safe to show and to store", () => {
    const s = start({ device: "Kanavs MacBook!", folder: "../../etc" });
    expect(auth.describeLogin(s.code)).toMatchObject({ device: "device", folder: undefined });
  });

  it("signs a trusted link in once", () => {
    const s = start();
    expect(auth.claimBrowser(s.code)).toBe(true);
    expect(auth.claimBrowser(s.code)).toBe(false);
    expect(auth.describeLogin(s.code)?.claimed).toBe(true);
  });

  it("never signs the browser in from a session that is not trusted", () => {
    const s = start({ trusted: false });
    expect(auth.claimBrowser(s.code)).toBe(false);
    expect(auth.describeLogin(s.code)?.claimed).toBe(false);
  });

  it("will not sign in from an unknown, approved or expired session", () => {
    expect(auth.claimBrowser("AAAA-AAAA")).toBe(false);
    const s = start();
    auth.approveLogin(s.code, { mode: "none" });
    expect(auth.claimBrowser(s.code)).toBe(false);
    vi.useFakeTimers();
    const t = start();
    vi.advanceTimersByTime(10 * 60_000 + 1);
    expect(auth.claimBrowser(t.code)).toBe(false);
  });

  it("stays pending until the owner decides", () => {
    const s = start();
    expect(auth.pollLogin(s.pollToken)).toEqual({ state: "pending" });
    expect(auth.pollLogin(s.pollToken)).toEqual({ state: "pending" });
  });

  it("hands over a working device token and the owner's choice, exactly once", () => {
    const s = start({ device: "laptop", folder: "my-app" });
    expect(auth.approveLogin(s.code, { mode: "new", project: "my-app", agentName: "lead1", adapter: "claude" })).toBe(
      true,
    );
    const got = auth.pollLogin(s.pollToken);
    if (got.state !== "approved") throw new Error("expected approval");
    expect(got.device).toBe("laptop");
    expect(got.decision).toEqual({ mode: "new", project: "my-app", agentName: "lead1", adapter: "claude" });
    expect(db.nodeForToken(got.token)).toBe("laptop");
    expect(auth.pollLogin(s.pollToken)).toEqual({ state: "expired" });
  });

  it("does not store the token while it waits to be collected", () => {
    const s = start();
    auth.approveLogin(s.code, { mode: "none" });
    const rows = JSON.stringify(
      (db as unknown as { db: { prepare: (q: string) => { all: () => unknown[] } } }).db
        .prepare("SELECT * FROM login_sessions")
        .all(),
    );
    expect(rows).not.toContain("ccn1.");
    const got = auth.pollLogin(s.pollToken);
    if (got.state !== "approved") throw new Error("expected approval");
    expect(
      JSON.stringify(
        (db as unknown as { db: { prepare: (q: string) => { all: () => unknown[] } } }).db
          .prepare("SELECT * FROM tokens")
          .all(),
      ),
    ).not.toContain(got.token);
  });

  it("gives a second machine with the same name its own entry", () => {
    const a = start({ device: "laptop" });
    const b = start({ device: "laptop" });
    auth.approveLogin(a.code, { mode: "none" });
    auth.approveLogin(b.code, { mode: "none" });
    const ga = auth.pollLogin(a.pollToken);
    const gb = auth.pollLogin(b.pollToken);
    if (ga.state !== "approved" || gb.state !== "approved") throw new Error("expected approval");
    expect(new Set([ga.device, gb.device]).size).toBe(2);
    expect(db.nodeForToken(ga.token)).toBe(ga.device);
    expect(db.nodeForToken(gb.token)).toBe(gb.device);
  });

  it("tells the command line when the owner said no, and cleans up", () => {
    const s = start();
    expect(auth.denyLogin(s.code)).toBe(true);
    expect(auth.pollLogin(s.pollToken)).toEqual({ state: "denied" });
    expect(auth.pollLogin(s.pollToken)).toEqual({ state: "expired" });
  });

  it("cannot be decided twice, or after it has expired", () => {
    const s = start();
    expect(auth.approveLogin(s.code, { mode: "none" })).toBe(true);
    expect(auth.approveLogin(s.code, { mode: "new", project: "other" })).toBe(false);
    expect(auth.denyLogin(s.code)).toBe(false);
    vi.useFakeTimers();
    const t = start();
    vi.advanceTimersByTime(10 * 60_000 + 1);
    expect(auth.approveLogin(t.code, { mode: "none" })).toBe(false);
    expect(auth.pollLogin(t.pollToken)).toEqual({ state: "expired" });
  });

  it("rejects a poll token it does not know", () => {
    expect(auth.pollLogin("nope")).toEqual({ state: "expired" });
  });
});
