/**
 * Agents run as the user, so by default they inherit the daemon's whole environment, including cloud credentials,
 * tokens and passwords the user exported for other things. A prompt-injected agent could read and leak them, so the
 * environment an agent starts with is scrubbed of anything that looks like a secret.
 */
import { redact } from "@claudecord/protocol";

const SECRET_NAME = /(SECRET|TOKEN|PASSWORD|PASSWD|PRIVATE|CREDENTIAL|API_?KEY|ACCESS_?KEY|AUTH|COOKIE|SESSION)/i;
const SECRET_PREFIX =
  /^(AWS_|AZURE_|GCP_|GCLOUD_|GOOGLE_APPLICATION_CREDENTIALS|GH_|GITHUB_|GITLAB_|NPM_|YARN_|DOCKER_|KUBE|VAULT_|DATABASE_URL|REDIS_URL|MONGO|PG(PASSWORD|USER)|SLACK_|DISCORD_|STRIPE_|TWILIO_|SENTRY_)/i;

/** What each agent needs to log in to its own service. These are kept on purpose. */
const KEEP: Record<string, RegExp> = {
  claude: /^(ANTHROPIC_|CLAUDE_)/i,
  agy: /^(GOOGLE_API_KEY|GEMINI_|ANTIGRAVITY_|GOOGLE_CLOUD_PROJECT)/i,
  codex: /^(OPENAI_|CODEX_)/i,
};

/** claudecord's own variables carry no secrets and the agent tools need them. */
const OWN = /^CLAUDECORD_/;

/**
 * Kept on purpose even though the name contains AUTH. It is a path to the ssh agent's socket, not a secret, and
 * without it `git push` over ssh stops working. It does let an agent use the keys in your ssh agent, which
 * docs/SECURITY.md says plainly.
 */
const KEEP_ALWAYS = new Set(["SSH_AUTH_SOCK"]);

export type EnvPolicy = "scrub" | "inherit";

/** Names to remove from an agent's environment. */
export function secretEnvNames(env: NodeJS.ProcessEnv, adapter: string): string[] {
  const keep = KEEP[adapter];
  const out: string[] = [];
  for (const [name, value] of Object.entries(env)) {
    if (value === undefined || OWN.test(name) || KEEP_ALWAYS.has(name) || keep?.test(name)) continue;
    const bySecretName = SECRET_NAME.test(name) || SECRET_PREFIX.test(name);
    // A harmless-looking name can still hold a token, so look at the value too.
    const byValue = value.length >= 12 && redact(value).found.length > 0;
    if (bySecretName || byValue) out.push(name);
  }
  return out;
}

/** Wraps a command so it starts with those variables removed. `env -u` works the same on macOS and Linux. */
export function withScrubbedEnv(argv: string[], names: string[]): string[] {
  if (!names.length) return argv;
  return ["env", ...names.flatMap((n) => ["-u", n]), ...argv];
}
