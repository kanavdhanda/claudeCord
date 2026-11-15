//! Everything about the coding agents themselves: how each harness is launched and read, and how text is made safe
//! before it reaches one.

pub mod adapters;
pub mod text;

pub use adapters::AdapterExt;

/// What `claudecord init` puts in a project's AGENTS.md, so any agent that reads that file knows the team commands. Kept very short: it is read at the
/// start of every session, and so paid for in tokens every time.
pub const AGENTS_GUIDE: &str = "\
## claudeCord team chat
Team messages arrive as `[name] text`; reply to them only with the shell command `claudecord say \"text\"`, never also as terminal text. Input with no `[name]` was typed at the terminal: answer there, no say. Run `claudecord guide` once for the rules.
";
