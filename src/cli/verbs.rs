//! The commands an agent runs in its own shell. Each one finds out which agent it is from the environment the daemon
//! gave it (`CLAUDECORD_AGENT`), sends one request to the daemon on this machine, and prints the answer. They cost the
//! agent one short tool call and no tool definitions in its context.

use crate::device::config::home_dir;
use crate::device::ipc::{self, Req};

/// Which agent this shell belongs to.
fn me() -> Result<String, String> {
    std::env::var("CLAUDECORD_AGENT").map_err(|_| {
        "this command is for agents started by claudecord (CLAUDECORD_AGENT is not set)".to_string()
    })
}

/// Sends a request and prints the answer, failing if it was refused.
async fn send(req: Req) -> Result<(), String> {
    let r = ipc::call(&home_dir(), &req)
        .await
        .map_err(|_| "the claudecord daemon is not running".to_string())?;
    if r.ok {
        println!("{}", r.msg);
        Ok(())
    } else {
        Err(r.msg)
    }
}

/// `claudecord say`: a message to the team.
pub async fn say(text: String, thread: Option<String>) -> Result<(), String> {
    send(Req::Say {
        agent: me()?,
        text,
        thread,
    })
    .await
}

/// `claudecord ask`: a question for the people. Returns at once.
pub async fn ask(question: String) -> Result<(), String> {
    send(Req::Ask {
        agent: me()?,
        question,
        options: None,
        thread: None,
    })
    .await
}

/// `claudecord assign`: a task for a peer (the lead only).
pub async fn assign(to: String, task: String) -> Result<(), String> {
    send(Req::Assign {
        agent: me()?,
        to,
        task,
        thread: None,
    })
    .await
}

/// `claudecord done`: a task is finished.
pub async fn done(task: String, summary: String) -> Result<(), String> {
    send(Req::Done {
        agent: me()?,
        task,
        summary,
    })
    .await
}

/// `claudecord report`: a report for the people.
pub async fn report(title: String, summary: String) -> Result<(), String> {
    send(Req::Report {
        agent: me()?,
        title,
        summary,
        artifacts: None,
    })
    .await
}

/// `claudecord dump`: save this session's state for a fresh one.
pub async fn dump(text: String) -> Result<(), String> {
    send(Req::Dump { agent: me()?, text }).await
}

/// `claudecord team`: who else is in this project, what they do and whether they can be reached. The answer arrives as your next input.
pub async fn team() -> Result<(), String> {
    send(Req::Team { agent: me()? }).await
}

/// `claudecord pickup`: ask what to carry on from.
pub async fn pickup() -> Result<(), String> {
    send(Req::Pickup { agent: me()? }).await
}

/// `claudecord send`: a file to the chat or a peer.
pub async fn send_file(
    path: String,
    to: Option<String>,
    caption: Option<String>,
) -> Result<(), String> {
    send(Req::Send {
        agent: me()?,
        path,
        to,
        caption,
    })
    .await
}

/// `claudecord answer`: answer another agent's question.
pub async fn answer(ask: String, text: String) -> Result<(), String> {
    send(Req::Answer {
        agent: me()?,
        ask,
        text,
    })
    .await
}
