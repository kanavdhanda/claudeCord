//! Agents run as the user, so by default they inherit the daemon's whole environment, including cloud credentials,
//! tokens and passwords the user exported for other things. A prompt-injected agent could read and leak them, so the
//! environment an agent starts with is scrubbed of anything that looks like a secret.

use crate::{jslen, security::redact::redact};
use regex::Regex;
use std::sync::LazyLock;

static SECRET_NAME: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(SECRET|TOKEN|PASSWORD|PASSWD|PRIVATE|CREDENTIAL|API_?KEY|ACCESS_?KEY|AUTH|COOKIE|SESSION)").unwrap()
});
static SECRET_PREFIX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)^(AWS_|AZURE_|GCP_|GCLOUD_|GOOGLE_APPLICATION_CREDENTIALS|GH_|GITHUB_|GITLAB_|NPM_|YARN_|DOCKER_|KUBE|VAULT_|DATABASE_URL|REDIS_URL|MONGO|PG(PASSWORD|USER)|SLACK_|DISCORD_|STRIPE_|TWILIO_|SENTRY_)",
    )
    .unwrap()
});

/// What each agent needs to log in to its own service. These are kept on purpose.
fn keep(adapter: &str, name: &str) -> bool {
    static CLAUDE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)^(ANTHROPIC_|CLAUDE_)").unwrap());
    static AGY: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)^(GOOGLE_API_KEY|GEMINI_|ANTIGRAVITY_|GOOGLE_CLOUD_PROJECT)").unwrap()
    });
    static CODEX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^(OPENAI_|CODEX_)").unwrap());
    match adapter {
        "claude" => CLAUDE.is_match(name),
        "agy" => AGY.is_match(name),
        "codex" => CODEX.is_match(name),
        _ => false,
    }
}

/// Names to remove from an agent's environment.
pub fn secret_env_names<'a>(
    env: impl IntoIterator<Item = (&'a str, &'a str)>,
    adapter: &str,
) -> Vec<String> {
    env.into_iter()
        .filter(|(name, value)| {
            // claudecord's own variables carry no secrets and the agent tools need them. SSH_AUTH_SOCK is a path to
            // the ssh agent, not a secret, and without it `git push` over ssh stops working.
            if name.starts_with("CLAUDECORD_") || *name == "SSH_AUTH_SOCK" || keep(adapter, name) {
                return false;
            }
            let by_name = SECRET_NAME.is_match(name) || SECRET_PREFIX.is_match(name);
            // A harmless-looking name can still hold a token, so look at the value too.
            by_name || (jslen(value) >= 12 && !redact(value).found.is_empty())
        })
        .map(|(name, _)| name.to_string())
        .collect()
}

/// Wraps a command so it starts with those variables removed. `env -u` works the same on macOS and Linux.
pub fn with_scrubbed_env(argv: Vec<String>, names: &[String]) -> Vec<String> {
    if names.is_empty() {
        return argv;
    }
    let mut out = vec!["env".to_string()];
    for n in names {
        out.push("-u".into());
        out.push(n.clone());
    }
    out.extend(argv);
    out
}
