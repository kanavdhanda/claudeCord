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
    let device = Slug.safeParse(rawDevice).success ? rawDevice : "device";
    // A second machine with the same name gets its own entry instead of replacing the first one's token.
    if (this.db.deviceExists(device)) {
      for (let i = 2; i < 100; i++) {
        if (!this.db.deviceExists(`${device.slice(0, 58)}-${i}`)) {
          device = `${device.slice(0, 58)}-${i}`;
          break;
        }
      }
    }
    return { token: this.db.createToken(device), device };
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
