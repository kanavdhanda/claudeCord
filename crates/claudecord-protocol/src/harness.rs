//! The agent programs claudeCord knows by name. Claude, Codex and Agy have screen readers of their own (`adapters.rs` on the machine);
//! every other program here is started by one word and read with the general screen reader, so adding one is a line in this table.

use crate::AdapterId;

/// A program that can be started by its name alone.
pub struct Harness {
    /// What a person types (`--adapter gemini`), and what the dashboard offers.
    pub name: &'static str,
    /// The command that starts it (on the PATH).
    pub program: &'static str,
    /// How to install it, for the message that says it is missing.
    pub install: &'static str,
}

/// Programs started by one word. Their start-up flags are not known to claudeCord: they run as installed, and what they ask for comes to the chat
/// as a question like any other. Not every one has been tried; if one reads badly, say so and give it a reader of its own.
pub const HARNESSES: &[Harness] = &[
    Harness {
        name: "gemini",
        program: "gemini",
        install: "npm i -g @google/gemini-cli",
    },
    Harness {
        name: "opencode",
        program: "opencode",
        install: "npm i -g opencode-ai",
    },
    Harness {
        name: "aider",
        program: "aider",
        install: "pipx install aider-chat",
    },
    Harness {
        name: "cursor-agent",
        program: "cursor-agent",
        install: "curl https://cursor.com/install -fsS | bash",
    },
    Harness {
        name: "copilot",
        program: "copilot",
        install: "npm i -g @github/copilot",
    },
    Harness {
        name: "amp",
        program: "amp",
        install: "install Amp (https://ampcode.com) and put `amp` on your PATH",
    },
    Harness {
        name: "goose",
        program: "goose",
        install: "install Goose (https://block.github.io/goose) and put `goose` on your PATH",
    },
    Harness {
        name: "qwen",
        program: "qwen",
        install: "npm i -g @qwen-code/qwen-code",
    },
    Harness {
        name: "crush",
        program: "crush",
        install: "brew install charmbracelet/tap/crush",
    },
    Harness {
        name: "droid",
        program: "droid",
        install: "install Factory's droid (https://factory.ai) and put `droid` on your PATH",
    },
];

/// The named program for `name`, if there is one.
pub fn harness(name: &str) -> Option<&'static Harness> {
    HARNESSES.iter().find(|h| h.name == name)
}

/// What an agent type name means: how it is read, and (for a named program) which one. `None` if the name is not known.
/// `generic` is the general reader with no program of its own: it needs a saved startup command to say what to run.
pub fn resolve_adapter(name: &str) -> Option<(AdapterId, Option<&'static str>)> {
    match name {
        "claude" => Some((AdapterId::Claude, None)),
        "agy" => Some((AdapterId::Agy, None)),
        "codex" => Some((AdapterId::Codex, None)),
        "generic" => Some((AdapterId::Generic, None)),
        other => harness(other).map(|h| (AdapterId::Generic, Some(h.name))),
    }
}

/// Every agent type name, for messages that list the choices.
pub fn adapter_names() -> Vec<&'static str> {
    ["claude", "codex", "agy", "generic"]
        .into_iter()
        .chain(HARNESSES.iter().map(|h| h.name))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_name_resolves_and_the_named_programs_read_as_generic() {
        assert_eq!(resolve_adapter("claude"), Some((AdapterId::Claude, None)));
        assert_eq!(
            resolve_adapter("gemini"),
            Some((AdapterId::Generic, Some("gemini")))
        );
        assert_eq!(resolve_adapter("nope"), None);
        for n in adapter_names() {
            assert!(resolve_adapter(n).is_some(), "{n}");
        }
        let mut seen = std::collections::HashSet::new();
        assert!(
            HARNESSES
                .iter()
                .all(|h| seen.insert(h.name) && crate::is_slug(h.name))
        );
    }
}
