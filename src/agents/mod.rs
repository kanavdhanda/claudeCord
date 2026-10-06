//! Everything about the coding agents themselves: how each harness is launched and read, and how text is made safe
//! before it reaches one.

pub mod adapters;
pub mod text;

/// What `claudecord init` puts in a project's AGENTS.md, so any agent that reads that file (and Claude, through CLAUDE.md) knows the team
/// commands. Short on purpose: it is read at the start of every session.
pub const AGENTS_GUIDE: &str = "\
## claudeCord team chat
You may be one of several agents in this project, working through claudeCord. Use the shell command `claudecord`:
- `claudecord team`: who else is here, what they do, who leads, and whether they can be reached. The answer arrives as your next input.
- `claudecord say \"text\"`: tell the team. Start with @name to need a reply from that agent; a plain say is FYI.
- `claudecord ask \"question\"`: ask the people. The answer arrives as your next input.
- `claudecord assign NAME \"task\"` (lead only), `claudecord done ID \"summary\"`, `claudecord report TITLE SUMMARY`.
- `claudecord send FILE`: share a file. `claudecord dump TEXT` saves your state; `claudecord pickup` resumes it.
Team messages arrive in your input as `[name] text`. Answer with `say`, not by typing a reply here. While you work on a task your `say` goes
to that task's thread; `ask`, `done` and `report` go to the main chat.
";
