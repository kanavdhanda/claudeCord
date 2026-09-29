// Scrubs secrets from text before it is posted to Discord.

const PATTERNS: RegExp[] = [
  /sk-ant-[A-Za-z0-9_-]{20,}/g, // Anthropic keys
  /sk-[A-Za-z0-9]{32,}/g, // OpenAI-style keys
  /gh[pousr]_[A-Za-z0-9]{30,}/g, // GitHub tokens
  /github_pat_[A-Za-z0-9_]{40,}/g,
  /AKIA[0-9A-Z]{16}/g, // AWS access key ids
  /[MNO][A-Za-z\d_-]{23,25}\.[\w-]{6}\.[\w-]{27,}/g, // Discord bot tokens
  /-----BEGIN [A-Z ]*PRIVATE KEY-----[\s\S]*?-----END [A-Z ]*PRIVATE KEY-----/g,
];

export function redactSecrets(text: string, extraSecrets: string[] = []): string {
  let out = text;
  for (const secret of extraSecrets) {
    if (secret.length >= 8) out = out.split(secret).join("[redacted]");
  }
  for (const re of PATTERNS) out = out.replace(re, "[redacted]");
  return out;
}
