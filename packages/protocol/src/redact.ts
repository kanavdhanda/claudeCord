/**
 * Best-effort secret scrubbing for text and files that leave a machine through an agent. A prompt-injected agent can
 * be told to paste credentials into chat, so everything that reaches Discord or a peer passes through here first.
 * This catches well-known token shapes. It is a safety net, not a guarantee.
 */

interface Rule {
  kind: string;
  re: RegExp;
  /** High confidence rules are also used to block file uploads. */
  strong: boolean;
  /** Keep a capture group (for example the variable name) and replace only the value. */
  keep?: number;
}

const RULES: Rule[] = [
  {
    kind: "private key",
    re: /-----BEGIN (?:[A-Z0-9]+ )*PRIVATE KEY-----[\s\S]*?(?:-----END (?:[A-Z0-9]+ )*PRIVATE KEY-----|$)/g,
    strong: true,
  },
  { kind: "aws key", re: /\b(?:AKIA|ASIA)[0-9A-Z]{16}\b/g, strong: true },
  { kind: "github token", re: /\b(?:gh[pousr]_[A-Za-z0-9]{36,}|github_pat_[A-Za-z0-9_]{50,})\b/g, strong: true },
  { kind: "api key", re: /\bsk-(?:ant-|proj-)?[A-Za-z0-9_-]{20,}\b/g, strong: true },
  { kind: "slack token", re: /\bxox[abprs]-[A-Za-z0-9-]{10,}\b/g, strong: true },
  { kind: "discord token", re: /\b[MNO][A-Za-z\d_-]{23,25}\.[\w-]{6}\.[\w-]{27,}\b/g, strong: true },
  { kind: "claudecord token", re: /\bccn1\.[A-Za-z0-9_-]{20,}\b/g, strong: true },
  { kind: "google key", re: /\bAIza[0-9A-Za-z_-]{35}\b/g, strong: true },
  { kind: "jwt", re: /\beyJ[A-Za-z0-9_-]{8,}\.eyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}\b/g, strong: true },
  { kind: "bearer token", re: /\b(Bearer\s+)(?!\[redacted)[A-Za-z0-9._~+/-]{20,}=*/g, strong: false, keep: 1 },
  {
    kind: "secret assignment",
    // The lookahead keeps redaction idempotent: a value that is already a marker is left alone, so scrubbing on the
    // device and again on the hub does not mangle it.
    re: /\b([A-Za-z0-9_]*(?:SECRET|TOKEN|PASSWORD|PASSWD|API_?KEY|PRIVATE_?KEY|CREDENTIALS?)[A-Za-z0-9_]*\s*[=:]\s*['"]?)(?!\[redacted)[^\s'",;]{8,}/gi,
    strong: false,
    keep: 1,
  },
];

export interface Redaction {
  text: string;
  /** Kinds of secret that were removed, one entry per match. */
  found: string[];
}

export function redact(text: string): Redaction {
  const found: string[] = [];
  let out = text;
  for (const r of RULES) {
    out = out.replace(r.re, (...m: unknown[]) => {
      found.push(r.kind);
      const kept = r.keep ? (m[r.keep] as string) : "";
      return `${kept}[redacted ${r.kind}]`;
    });
  }
  return { text: out, found };
}

/** A line that assigns a value to a secret-looking name, such as `export DB_PASSWORD=...`. */
const ENV_LINE =
  /^\s*(?:export\s+)?[A-Za-z0-9_]*(?:SECRET|TOKEN|PASSWORD|PASSWD|API_?KEY|PRIVATE_?KEY|CREDENTIALS?|ACCESS_?KEY)[A-Za-z0-9_]*\s*[=:]\s*['"]?[^\s'"]{8,}/i;

/**
 * Detects a dump of environment variables or credentials, however the file is named. Renaming `.env` to `notes.md`
 * must not get it past the filename check, so the content is what counts.
 */
export function looksLikeEnvDump(text: string): boolean {
  let n = 0;
  for (const line of text.split("\n", 5000)) if (ENV_LINE.test(line) && ++n >= 2) return true;
  return false;
}

/**
 * Kinds of secret in a file, so it can be blocked. High confidence token shapes, private keys, and files that look
 * like a dump of environment variables. Binary files are not scanned.
 */
export function findSecretsInFile(data: Buffer): string[] {
  if (data.subarray(0, 8192).includes(0)) return [];
  const text = data.toString("utf8");
  const kinds = new Set<string>();
  if (looksLikeEnvDump(text)) kinds.add("environment variable dump");
  for (const r of RULES) {
    if (!r.strong) continue;
    r.re.lastIndex = 0;
    if (r.re.test(text)) kinds.add(r.kind);
    r.re.lastIndex = 0;
  }
  return [...kinds];
}
