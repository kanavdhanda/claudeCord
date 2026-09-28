/** Minimal .env editing that keeps comments and unrelated lines intact. */

export function readEnvValue(text: string, key: string): string | undefined {
  const m = text.match(new RegExp(`^${key}=(.*)$`, "m"));
  return m ? m[1].trim() : undefined;
}

export function upsertEnv(text: string, updates: Record<string, string>): string {
  let out = text;
  for (const [key, value] of Object.entries(updates)) {
    if (/[\r\n]/.test(value)) throw new Error(`Refusing to write a multi-line value for ${key}`);
    const line = `${key}=${value}`;
    const re = new RegExp(`^${key}=.*$`, "m");
    if (re.test(out)) out = out.replace(re, () => line);
    else out = out + (out === "" || out.endsWith("\n") ? "" : "\n") + line + "\n";
  }
  return out;
}
