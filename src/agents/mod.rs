//! Everything about the coding agents themselves: how each harness is launched and read, and how text is made safe
//! before it reaches one.

pub mod adapters;
pub mod text;

/// What `claudecord init` puts in a project's AGENTS.md, so any agent that reads that file knows the team commands. Kept very short: it is read at the
/// start of every session, and so paid for in tokens every time.
pub const AGENTS_GUIDE: &str = "\
## claudeCord team chat
Team messages arrive as `[name] text`; answer with the shell command `claudecord say \"text\"`. Run `claudecord guide` once for the rules.
";
