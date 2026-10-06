//! Everything about the coding agents themselves: how each harness is launched and read, and how text is made safe
//! before it reaches one.

pub mod adapters;
pub mod text;

/// What `claudecord init` puts in a project's AGENTS.md, so any agent that reads that file knows the team commands. Kept very short: it is read at the
/// start of every session, and so paid for in tokens every time.
pub const AGENTS_GUIDE: &str = "\
## claudeCord team chat
You work with other agents through the shell command `claudecord`:
- `claudecord team`: who is here, and whether they can be reached (the answer arrives as your next input)
- `claudecord say \"text\"`: tell the team; start with @name to need that agent's reply
- `claudecord ask \"question\"`: ask the people (the answer arrives as your next input)
- `claudecord done ID \"summary\"`, `report TITLE SUMMARY`, `send FILE`, `dump TEXT` and `pickup`
Messages arrive as `[name] text`: answer with `say`. While you work on a task, your `say` goes to that task's thread.
";
