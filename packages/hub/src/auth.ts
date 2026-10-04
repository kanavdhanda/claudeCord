/**
 * Pairing and sign-in, with Discord as the identity. Only the configured owner can mint a pairing code or a dashboard
 * link (through slash commands), so there are no passwords and no accounts to manage. Everything here is a short lived,
 * single use secret stored only as a hash.
 */
import { randomBytes } from "node:crypto";
import { Slug } from "@claudecord/protocol";
import type { Db } from "./db.js";

// No 0, O, 1, I or L, so a code read aloud or from a small screen is hard to get wrong.
const ALPHABET = "ABCDEFGHJKMNPQRSTUVWXYZ23456789";
const PAIR_TTL_MS = 10 * 60_000;
const LOGIN_TTL_MS = 10 * 60_000;
export const SESSION_TTL_MS = 12 * 60 * 60_000;

function randomCode(len: number): string {
  // Rejection sampling: bytes at or above the largest multiple of the alphabet size are discarded, so every symbol is
  // exactly equally likely.
  const limit = 256 - (256 % ALPHABET.length);
  let out = "";
  while (out.length < len) {
    for (const b of randomBytes(len * 2)) {
      if (b < limit && out.length < len) out += ALPHABET[b % ALPHABET.length];
    }
  }
  return out;
}

/** Codes are typed by people, so accept any case and a dash or spaces. */
export function normalizeCode(input: string): string {
  return input.toUpperCase().replace(/[^A-Z0-9]/g, "");
}

export function formatCode(code: string): string {
  return `${code.slice(0, 4)}-${code.slice(4)}`;
}

/** What the owner chose in the browser for the folder that asked to sign in. */
export interface LoginDecision {
  /** new makes a project, existing joins one, none only connects the machine. */
  mode: "new" | "existing" | "none";
  project?: string;
  agentName?: string;
  adapter?: string;
}

export class Auth {
  constructor(private db: Db) {}

  // Device pairing

  /** A code the owner types into `claudecord login`. Valid for 10 minutes, once. */
  createPairCode(): { code: string; expiresInMs: number } {
    const code = randomCode(8);
    this.db.putSecret("pair_codes", code, PAIR_TTL_MS);
    return { code: formatCode(code), expiresInMs: PAIR_TTL_MS };
  }

  /** Exchanges a pairing code for a device token. Returns null for a wrong, used or expired code. */
  redeemPairCode(rawCode: string, rawDevice: string): { token: string; device: string } | null {
    const code = normalizeCode(rawCode);
    if (code.length !== 8 || !this.db.takeSecret("pair_codes", code)) return null;
    const device = this.uniqueDevice(rawDevice);
    return { token: this.db.createToken(device), device };
  }

  /** A second machine with the same name gets its own entry instead of replacing the first one's token. */
  private uniqueDevice(raw: string): string {
    const device = Slug.safeParse(raw).success ? raw : "device";
    if (!this.db.deviceExists(device)) return device;
    for (let i = 2; i < 100; i++) {
      const candidate = `${device.slice(0, 58)}-${i}`;
      if (!this.db.deviceExists(candidate)) return candidate;
    }
    return `${device.slice(0, 50)}-${randomBytes(4).toString("hex")}`;
  }

  // Browser login for a command line, like `claude login`

  /**
   * The command line asks to be signed in. It keeps `pollToken` secret and shows `code` in a link for the browser.
   * `trusted` means the request came from the same machine as the hub, so opening the link may sign the browser in
   * without anyone approving it. A request from anywhere else always needs the owner to approve.
   */
  startLogin(o: { device: string; folder?: string; trusted: boolean }): {
    code: string;
    pollToken: string;
    expiresInMs: number;
  } {
    const code = randomCode(8);
    const pollToken = randomBytes(24).toString("base64url");
    const folder = o.folder && Slug.safeParse(o.folder).success ? o.folder : undefined;
    this.db.createLoginSession(code, pollToken, {
      device: Slug.safeParse(o.device).success ? o.device : "device",
      folder,
      trusted: o.trusted,
      ttlMs: LOGIN_TTL_MS,
    });
    return { code: formatCode(code), pollToken, expiresInMs: LOGIN_TTL_MS };
  }

  describeLogin(rawCode: string) {
    const row = this.db.loginByCode(normalizeCode(rawCode));
    if (!row) return null;
    return {
      device: row.device,
      folder: row.folder ?? undefined,
      trusted: !!row.trusted,
      state: row.state,
      claimed: !!row.claimed,
    };
  }

  /** True the first time only. A trusted link signs the browser in once, so a copy of it later is useless. */
  claimBrowser(rawCode: string): boolean {
    const code = normalizeCode(rawCode);
    const row = this.db.loginByCode(code);
    return !!row && !!row.trusted && row.state === "pending" && this.db.claimLogin(code);
  }

  approveLogin(rawCode: string, decision: LoginDecision): boolean {
    return this.db.decideLogin(normalizeCode(rawCode), "approved", decision);
  }

  denyLogin(rawCode: string): boolean {
    return this.db.decideLogin(normalizeCode(rawCode), "denied", null);
  }

  /**
   * What the command line asks while it waits. The device token is made here, at the moment it is collected, and the
   * session is deleted, so a token is never stored waiting to be picked up and can only be collected once.
   */
  pollLogin(
    pollToken: string,
  ):
    | { state: "pending" | "denied" | "expired" }
    | { state: "approved"; token: string; device: string; decision: LoginDecision } {
    const row = this.db.loginByPoll(pollToken);
    if (!row) return { state: "expired" };
    if (row.state === "pending") return { state: "pending" };
    this.db.deleteLoginByPoll(pollToken);
    if (row.state === "denied") return { state: "denied" };
    const device = this.uniqueDevice(row.device);
    const decision = (row.decision ? JSON.parse(row.decision) : { mode: "none" }) as LoginDecision;
    return { state: "approved", token: this.db.createToken(device), device, decision };
  }

  // Dashboard sign in

  /** A one-time link token. Opening the link signs the owner in. */
  createLoginToken(): { token: string; expiresInMs: number } {
    const token = randomBytes(24).toString("base64url");
    this.db.putSecret("login_tokens", token, LOGIN_TTL_MS);
    return { token, expiresInMs: LOGIN_TTL_MS };
  }

  /** Spends a login token and starts a session. Returns the session secret for the cookie, or null. */
  redeemLoginToken(token: string): string | null {
    if (!this.db.takeSecret("login_tokens", token)) return null;
    const session = randomBytes(32).toString("base64url");
    this.db.putSecret("sessions", session, SESSION_TTL_MS);
    return session;
  }

  verifySession(session: string | undefined): boolean {
    return !!session && this.db.hasSecret("sessions", session);
  }

  endSession(session: string | undefined): void {
    if (session) this.db.deleteSecret("sessions", session);
  }
}
