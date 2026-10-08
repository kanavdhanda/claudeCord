//! The daemon: the one program that runs on a machine with agents. It keeps the link to the hub, starts each agent in a
//! terminal it owns, pastes messages from the hub into the right terminal at a safe moment, watches each terminal to tell
//! the hub what the agent is doing, and answers the `claudecord` command that agents and people run in a shell.
//!
//! It is one loop. Everything that can happen (a frame from the hub, a request from the command line, a timer) is handled
//! one at a time by `State`, so there is nothing to lock. The only other threads are the ones that read each terminal.

use super::config::Config;
use super::inject::{Urgency, key_bytes};
use super::ipc::{Envelope, Req, Resp, UpOpts, socket_path};
use super::link::{self, Link, LinkEvent, LinkOpts};
use super::logs::AgentLog;
use super::terminal::{Backend, Spawn, Terminal};
use crate::agents::adapters::{LaunchCtx, Policy, ScreenState};
use crate::agents::text::{Delivery, format_deliveries, project_slug, safe_name};
use crate::protocol::{
    AdapterId, AgentSpec, AgentStatus, FILE_CHUNK_BYTES, HubFrame, MAX_FILE_BYTES, NodeFrame,
    UsageKind, auto_name,
};
use crate::security::env::secret_env_names;
use crate::sync::Lock;
use base64::Engine;
use interprocess::local_socket::tokio::{RecvHalf, SendHalf, Stream as LocalStream};
use interprocess::local_socket::traits::tokio::{Listener as _, Stream as _};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot};

/// The short standing instruction added to every agent: the verbs it has and the one rule about replies.
/// The last lines of a terminal screen, for the chat: blank lines at the end dropped, secrets removed, at most about 1500 characters.
pub fn shot(screen: &str) -> String {
    let lines: Vec<&str> = screen.lines().map(str::trim_end).collect();
    let end = lines
        .iter()
        .rposition(|l| !l.is_empty())
        .map_or(0, |i| i + 1);
    let from = end.saturating_sub(25);
    let text = lines[from..end].join("\n");
    let clean = crate::security::redact::redact(&text);
    // A credential that wraps over the end of a line is two halves, and neither is recognised alone. If putting the lines together shows
    // more of them than each line did, show nothing of the screen rather than a half.
    let together = crate::security::redact::redact(&lines[from..end].concat());
    if together.found.len() > clean.found.len() {
        return "(the screen shows something that looks like a credential running over a line, so it is not shown)".into();
    }
    let text = clean.text;
    let skip = text.chars().count().saturating_sub(1500);
    text.chars().skip(skip).collect()
}

/// Received files are kept this long, and this much of them at most (the oldest go first).
const FILES_KEEP_MS: i64 = 3 * 24 * 3_600_000;
/// How long `claudecord say` waits for the hub to confirm that the agents it named have the message.
const SAY_WAIT: Duration = Duration::from_secs(5);

/// What `claudecord say` prints, from what the hub said (or did not say in time).
fn say_answer(
    got: Result<
        Result<(String, Option<String>), oneshot::error::RecvError>,
        tokio::time::error::Elapsed,
    >,
) -> Resp {
    match got {
        Ok(Ok((state, detail))) => match state.as_str() {
            "delivered" => Resp::ok("delivered: they have it"),
            "pending" => Resp::ok(format!(
                "pending: {} cannot take it yet (offline or on hold); it will be delivered as soon as it can",
                detail.unwrap_or_else(|| "the agent".into())
            )),
            "failed" => Resp::err(format!(
                "message could not be sent: {}",
                detail.unwrap_or_else(|| "the hub refused it".into())
            )),
            _ => Resp::ok("sent"),
        },
        _ => Resp::ok(
            "pending: sent, but not yet confirmed as received; it will be delivered when the agent is ready",
        ),
    }
}

/// How long after Escape a steering message waits before it is pasted.
const STEER_SETTLE_MS: i64 = 300;
const FILES_KEEP_BYTES: u64 = 200 * 1024 * 1024;

/// Removes the files in `dir` older than `max_age_ms`, then the oldest ones until what is left fits in `max_bytes`.
fn tidy_files(dir: &std::path::Path, max_age_ms: i64, max_bytes: u64) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    let now = std::time::SystemTime::now();
    let mut files: Vec<(std::time::SystemTime, u64, PathBuf)> = rd
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let m = e.metadata().ok()?;
            m.is_file()
                .then(|| (m.modified().unwrap_or(now), m.len(), e.path()))
        })
        .collect();
    files.sort_by_key(|f| f.0);
    let mut total: u64 = files.iter().map(|f| f.1).sum();
    for (at, len, path) in files {
        let age = now.duration_since(at).map_or(0, |d| d.as_millis() as i64);
        // A piece of a transfer still arriving is not tidied away.
        if path.extension().is_some_and(|x| x == "part") && age < 15 * 60_000 {
            continue;
        }
        let old = age > max_age_ms;
        if (old || total > max_bytes) && std::fs::remove_file(&path).is_ok() {
            total -= len;
        }
    }
}

/// The first input of every agent, whatever its program: the team rules, once, as a message from the system, so nothing depends on a program's
/// own way of taking instructions. It asks for no reply, so it costs one short turn.
fn intro(spec: &AgentSpec) -> Option<Delivery> {
    Some(Delivery {
        from: "claudecord".into(),
        text: format!(
            "You are {} in project {}. {RULES} Do not answer this message; wait for the next one.",
            spec.name, spec.project
        ),
        thread: None,
        msg_id: None,
    })
}

pub const RULES: &str = "Team chat is the shell command claudecord: say <text>, ask <question>, assign <agent> <task>, done <id> <summary>, dump (save state), team (who else is here), send <file> (to the chat) or send <file> --to <agent> (to another agent's .claudecord inbox). Answer team messages with say, not also in the terminal; answer a person typing at your terminal there. For several lines pipe them in: say - <<'EOF'. Put @theirname before a person's name only when you need their attention; never @ yourself. @agentname addresses ANOTHER agent, in a thread of its own, so keep it short and only when you need them. Ask a person only what you cannot decide, once. Your say on a task goes to its thread; ask, done and report go to the main chat.";

/// Choices that tests change.
#[derive(Clone)]
pub struct Options {
    pub link: LinkOpts,
    /// Development and tests only (`CLAUDECORD_DEV_SPAWN=1`): lets the local socket start agents, which otherwise only the hub may ask for.
    pub dev_spawn: bool,
    /// How often terminals are looked at.
    pub tick: Duration,
    /// How long after pasting a message to wait for a sign the agent started on it, before saying it did.
    pub accept_after: Duration,
    /// Quiet time after the person types, and after the agent prints, before a message is pasted (milliseconds).
    pub quiet: (i64, i64),
    /// Folders searched first for the agent programs, ahead of this program's own folder and the usual PATH.
    pub extra_path: Vec<PathBuf>,
    /// Most agents this machine runs at once. More are refused, and the hub places new agents elsewhere.
    pub max_agents: usize,
    /// What kind of machine this is (`gpu`, `h100`...), told to the hub so a request can ask for it.
    pub labels: Vec<String>,
    /// How agents' terminals are provided: tmux where it is installed, otherwise the built-in terminal.
    pub backend: Backend,
    /// Answer start-up dialogs (such as "trust this folder") on its own. Off by default: a person decides.
    pub auto_startup: bool,
    /// Exit this long after the last agent ends, so the machine is only connected while something is running. None keeps it running.
    pub idle_exit: Option<Duration>,
    /// How long every agent may sit idle (nothing queued for it, nobody typing, not working or waiting) before the daemon ends them and goes
    /// away too. `None` keeps agents for ever. Not applied while keep-running is on.
    pub idle_agents_exit: Option<Duration>,
}

impl Default for Options {
    /// Look at terminals four times a second; assume a message was taken up after three seconds.
    fn default() -> Self {
        Self {
            link: LinkOpts::default(),
            dev_spawn: false,
            tick: Duration::from_millis(250),
            accept_after: Duration::from_secs(3),
            quiet: (
                super::inject::QUIET_INPUT_MS,
                super::inject::QUIET_OUTPUT_MS,
            ),
            extra_path: Vec::new(),
            backend: Backend::from_env(),
            auto_startup: false,
            idle_exit: None,
            idle_agents_exit: None,
            max_agents: std::env::var("CLAUDECORD_MAX_AGENTS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or_else(|| cores().max(2)),
            labels: std::env::var("CLAUDECORD_LABELS")
                .map(|v| {
                    v.split(',')
                        .map(|l| l.trim().to_string())
                        .filter(|l| !l.is_empty())
                        .collect()
                })
                .unwrap_or_default(),
        }
    }
}

/// How many processor cores this machine has.
fn cores() -> usize {
    std::thread::available_parallelism().map_or(1, |n| n.get())
}

/// Memory in megabytes, as far as it can be found cheaply (0 if not).
fn mem_mb() -> u64 {
    if let Ok(t) = std::fs::read_to_string("/proc/meminfo") {
        return t
            .lines()
            .find(|l| l.starts_with("MemTotal:"))
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|k| k.parse::<u64>().ok())
            .map_or(0, |k| k / 1024);
    }
    std::process::Command::new("sysctl")
        .args(["-n", "hw.memsize"])
        .output()
        .ok()
        .and_then(|o| {
            String::from_utf8_lossy(&o.stdout)
                .trim()
                .parse::<u64>()
                .ok()
        })
        .map_or(0, |b| b / 1_048_576)
}

/// A message pasted into a terminal that the hub has not yet been told was accepted.
struct Inflight {
    ids: Vec<String>,
    since: i64,
    screen_before: u64,
}

/// A permission prompt on screen that is waiting for the hub's decision.
struct AwaitingDecision {
    perm_id: String,
    prompt: crate::agents::adapters::PromptInfo,
}

/// Everything needed to start an agent's terminal again after it dies.
#[derive(Clone)]
struct Launch {
    argv: Vec<String>,
    secrets: Vec<String>,
    env: Vec<(String, String)>,
    rows: u16,
    cols: u16,
}

/// What became of a `claudecord` that is waiting for the hub to start an agent.
enum PendingState {
    Waiting,
    Started {
        agent: String,
        worktree: Option<String>,
    },
    Failed(String),
}

/// A `claudecord` waiting on the dashboard: where it was run, and the choices that belong to this machine (see `ExpectOpts`).
struct PendingStart {
    cwd: String,
    rows: u16,
    cols: u16,
    opts: super::ipc::ExpectOpts,
    at: i64,
    state: PendingState,
}

/// How long a waiting `claudecord` is remembered (the page's own code expires after the same time).
const PENDING_TTL_MS: i64 = 60 * 60_000;

/// Restarts (when asked for with `--restart N`) are counted over this long. Past the count the agent is left stopped, because
/// an agent that dies at once every time will not be fixed by trying faster.
const RESTART_WINDOW_MS: i64 = 10 * 60_000;

struct Agent {
    /// The secret this agent's own commands must carry, so no other agent can speak for it.
    key: String,
    log: AgentLog,
    /// How many times it may be started again if its program ends (zero unless asked for).
    restart_budget: usize,
    /// The folder the agent was asked to start in. Its own folder differs when it was given a worktree.
    origin: PathBuf,
    launch: Launch,
    restarts: VecDeque<i64>,
    spec: AgentSpec,
    cwd: PathBuf,
    proc: Terminal,
    queue: Vec<Delivery>,
    /// Text to type exactly as given (harness commands), oldest first. Sent one at a time, before ordinary messages.
    raw: VecDeque<String>,
    inflight: Option<Inflight>,
    /// How urgently the waiting queue wants to go in (raised by a `priority` frame, back to `Queue` once pasted).
    urgency: Urgency,
    /// The delivery that asked for it: once it has been pasted nothing is urgent any more. What waits ahead of it goes in with the same urgency.
    urgent_id: Option<String>,
    /// When Escape was pressed for a steering message, so the paste follows after the agent has had a moment to stop.
    steered_at: Option<i64>,
    status: AgentStatus,
    held: bool,
    asks: u32,
    shown_prompt: Option<String>,
    /// A prompt seen on the last tick but not yet reported: text scrolling past must stay on screen for two ticks to count.
    candidate_prompt: Option<String>,
    /// When a message that wants an answer was handed over, until the agent answers with any command of its own.
    awaiting: Option<i64>,
    /// When a picture of this terminal was last sent by itself (not asked for): at most one every five minutes.
    shot_at: i64,
    /// The screen as of the last change and since when: an agent that looks the same for minutes while "working" is stuck.
    still: (u64, i64),
    deciding: Option<AwaitingDecision>,
    limit_reported: bool,
    /// Internal errors in a row while looking at this agent (see `agent_faulted`).
    faults: u32,
    /// Files being received: where they are being written, and the chunk number expected next.
    files: HashMap<String, (PathBuf, u64)>,
}

/// A request from the command line, with where to send the answer.
type Call = (Envelope, oneshot::Sender<Resp>);

struct State {
    dir: PathBuf,
    /// Folders this machine has started agents in, by project, so the hub can start more there later. The hub never
    /// chooses a folder: only one the person already used on this machine.
    folders: HashMap<String, Vec<PathBuf>>,
    /// For each folder, the projects it was started for, the most recent first.
    last: BTreeMap<String, Vec<String>>,
    /// The `claudecord`s waiting for the hub to start an agent for them, by the code of the page they opened.
    pending: HashMap<String, PendingStart>,
    link: Link,
    /// Since when no agent has been running, so the daemon can go away by itself (see `Options::idle_exit`).
    idle_since: Option<i64>,
    /// When the received files were last tidied.
    tidied: i64,
    agents: HashMap<String, Agent>,
    /// Recently seen delivery ids, so a message the hub sent twice is pasted once.
    seen: VecDeque<String>,
    up: bool,
    opts: Options,
    quit: bool,
    /// The `say`s waiting for the hub's word on whether the agents they named have the message, by `say_id`.
    says: HashMap<String, oneshot::Sender<(String, Option<String>)>>,
    say_seq: u64,
    /// Set by `handle` for a `say`: the answer to the caller comes from this, later, and the daemon's loop carries on meanwhile.
    defer: Option<oneshot::Receiver<(String, Option<String>)>>,
}

/// Runs the daemon until told to shut down. Listens on the socket in `dir`, connects to the hub from `cfg`.
pub async fn run(cfg: Config, dir: PathBuf, opts: Options) -> std::io::Result<()> {
    std::fs::create_dir_all(&dir)?;
    let sock = socket_path(&dir);
    // Listening replaces whatever socket file is there (a leftover from a daemon that crashed), so a second daemon would quietly take the
    // first one's place and leave its agents unreachable. One that answers is not a leftover.
    if super::ipc::call(&dir, &Req::Ping).await.is_ok() {
        return Err(std::io::Error::other(
            "a daemon is already running here (claudecord stop ends it)",
        ));
    }
    let listener = super::ipc::listen(&dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(0o600))?;
    }
    let (ev_tx, mut events) = mpsc::channel(256);
    let link = link::spawn_with(
        link::Auth::new(&cfg.hub_url, cfg.token.clone(), Some(dir.clone())),
        cfg.connect_url(),
        cfg.node_name.clone(),
        ev_tx,
        opts.link.clone(),
    );
    let (call_tx, mut calls) = mpsc::channel::<Call>(64);
    let attach_calls = call_tx.clone();
    tokio::spawn(async move {
        loop {
            let Ok(stream) = listener.accept().await else {
                break;
            };
            tokio::spawn(serve(stream, call_tx.clone(), attach_calls.clone()));
        }
    });
    let folders = super::config::read_projects(&dir).into_iter().collect();
    let last = super::config::read_last(&dir);
    let mut st = State {
        folders,
        last,
        pending: HashMap::new(),
        idle_since: None,
        tidied: 0,
        dir,
        link,
        agents: HashMap::new(),
        seen: VecDeque::new(),
        up: false,
        opts: opts.clone(),
        quit: false,
        says: HashMap::new(),
        say_seq: 0,
        defer: None,
    };
    let mut tick = tokio::time::interval(opts.tick);
    let mut stop = Box::pin(crate::task::shutdown_signal());
    crate::info!(
        "daemon",
        "running as {} (terminals: {:?})",
        cfg.node_name,
        st.opts.backend.resolved()
    );
    loop {
        // Each step is guarded: a bug while handling one frame, request or tick costs that step, never the daemon and with it every
        // agent it supervises.
        tokio::select! {
            ev = events.recv() => match ev {
                None => break,
                Some(LinkEvent::Up) => {
                    crate::info!("daemon", "connected to the hub");
                    crate::task::guarded("handling the hub connecting", st.on_up()).await;
                }
                Some(LinkEvent::Down) => {
                    if st.up { crate::warn!("daemon", "lost the connection to the hub; reconnecting"); }
                    st.up = false;
                }
                Some(LinkEvent::Frame(f)) => { crate::task::guarded("handling a frame from the hub", st.on_frame(f)).await; }
            },
            call = calls.recv() => {
                let Some((env, reply)) = call else { break };
                let resp = crate::task::guarded("handling a command", st.handle(env.req, env.key.as_deref())).await
                    .unwrap_or_else(|| Resp::err("the daemon hit an internal error on that command; see its log"));
                match st.defer.take() {
                    // A `say` that named agents: answered when the hub says they have it, or after a few seconds, without holding the loop.
                    Some(receipt) if resp.ok => {
                        tokio::spawn(async move {
                            let _ = reply.send(say_answer(tokio::time::timeout(SAY_WAIT, receipt).await));
                        });
                    }
                    _ => { let _ = reply.send(resp); }
                }
            }
            _ = tick.tick() => { crate::task::guarded("looking at the terminals", st.on_tick(crate::now_ms())).await; }
            _ = &mut stop => {
                crate::info!("daemon", "told to stop; closing the agents");
                break;
            }
        }
        if st.quit {
            break;
        }
    }
    for a in st.agents.values() {
        a.proc.kill();
    }
    let _ = std::fs::remove_file(&sock);
    Ok(())
}

/// Whether `program` can be run: a path that exists, or a name found in one of the folders of `path`.
fn find_program(program: &str, path: &str) -> bool {
    let p = std::path::Path::new(program);
    if p.components().count() > 1 {
        return p.is_file();
    }
    let exts: &[&str] = if cfg!(windows) {
        &["", ".exe", ".cmd", ".bat"]
    } else {
        &[""]
    };
    std::env::split_paths(path).any(|dir| {
        exts.iter()
            .any(|e| dir.join(format!("{program}{e}")).is_file())
    })
}

/// One local connection: reads a request line, asks the main loop, writes the answer. An attach request turns the
/// connection into a live terminal view instead.
async fn serve(stream: LocalStream, calls: mpsc::Sender<Call>, attach: mpsc::Sender<Call>) {
    let (rd, mut wr) = stream.split();
    let mut rd = BufReader::new(rd);
    let mut line = String::new();
    if rd.read_line(&mut line).await.unwrap_or(0) == 0 {
        return;
    }
    let Ok(env) = serde_json::from_str::<Envelope>(&line) else {
        let _ = wr
            .write_all(b"{\"ok\":false,\"msg\":\"bad request\"}\n")
            .await;
        return;
    };
    if let Req::Attach { agent } = &env.req {
        // Ask for the terminal by sending the attach request on; the answer carries nothing, the proc comes via a side map.
        let (tx, rx) = oneshot::channel();
        let _ = attach
            .send((
                Envelope {
                    key: None,
                    req: Req::Attach {
                        agent: agent.clone(),
                    },
                },
                tx,
            ))
            .await;
        let resp = rx.await.unwrap_or_else(|_| Resp::err("daemon busy"));
        let mut out = serde_json::to_string(&resp).expect("plain data");
        out.push('\n');
        let _ = wr.write_all(out.as_bytes()).await;
        if resp.ok
            && let Some(proc) = take_proc(agent)
        {
            bridge(proc, rd, wr).await;
        }
        return;
    }
    let (tx, rx) = oneshot::channel();
    if calls.send((env, tx)).await.is_err() {
        return;
    }
    let resp = rx.await.unwrap_or_else(|_| Resp::err("daemon stopped"));
    let mut out = serde_json::to_string(&resp).expect("plain data");
    out.push('\n');
    let _ = wr.write_all(out.as_bytes()).await;
}

/// Terminals handed from the main loop to an attach connection. Keyed by agent id, taken once.
static ATTACH: std::sync::LazyLock<
    std::sync::Mutex<HashMap<String, Arc<super::pty::PtyTerminal>>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(HashMap::new()));

/// Takes the terminal the main loop set aside for this attach.
fn take_proc(agent: &str) -> Option<Arc<super::pty::PtyTerminal>> {
    ATTACH.locked().remove(agent)
}

/// Connects a window to a terminal. Screen contents are sent first so the window shows where the agent is, then output
/// streams out. Input comes in as small frames: type 0 is keys, type 1 is a window size (rows, cols).
async fn bridge(proc: Arc<super::pty::PtyTerminal>, mut rd: BufReader<RecvHalf>, mut wr: SendHalf) {
    let mut out = proc.output.subscribe();
    let _ = wr.write_all(&proc.redraw_bytes()).await;
    let p2 = proc.clone();
    let reader = tokio::spawn(async move {
        while let Ok(kind) = rd.read_u8().await {
            let Ok(len) = rd.read_u16().await else { break };
            let mut buf = vec![0u8; len as usize];
            if rd.read_exact(&mut buf).await.is_err() {
                break;
            }
            match kind {
                0 => {
                    let _ = p2.type_input(&buf, crate::now_ms());
                }
                1 if buf.len() == 4 => p2.resize(
                    u16::from_be_bytes([buf[0], buf[1]]),
                    u16::from_be_bytes([buf[2], buf[3]]),
                ),
                _ => {}
            }
        }
    });
    while let Ok(bytes) = out.recv().await {
        if wr.write_all(&bytes).await.is_err() {
            break;
        }
    }
    reader.abort();
}

impl State {
    /// The hub link came up: tell the hub about every agent here, and remember it is connected.
    async fn on_up(&mut self) {
        self.up = true;
        self.link
            .send(NodeFrame::NodeInfo {
                cores: cores() as u64,
                mem_mb: mem_mb(),
                max_agents: self.opts.max_agents as u64,
                labels: self.opts.labels.clone(),
            })
            .await;
        for a in self.agents.values() {
            self.link
                .send(NodeFrame::AgentRegister {
                    agent: a.spec.clone(),
                    cwd: a.cwd.to_string_lossy().into(),
                })
                .await;
        }
        // The full list last: whatever else the hub remembers of this machine is not running.
        self.link
            .send(NodeFrame::AgentsHere {
                agent_ids: self.agents.keys().cloned().collect(),
            })
            .await;
    }

    /// Something from the hub.
    async fn on_frame(&mut self, f: HubFrame) {
        match f {
            HubFrame::Deliver {
                agent_id,
                from,
                text,
                thread,
                msg_id,
            } => {
                if let Some(id) = &msg_id {
                    if self.seen.contains(id) {
                        return;
                    }
                    self.seen.push_back(id.clone());
                    if self.seen.len() > 512 {
                        self.seen.pop_front();
                    }
                }
                if let Some(a) = self.agents.get_mut(&agent_id) {
                    if from != "system" {
                        a.awaiting = Some(crate::now_ms());
                    }
                    a.queue.push(Delivery {
                        from,
                        text,
                        thread,
                        msg_id,
                    });
                }
            }
            HubFrame::Priority {
                agent_id,
                msg_id,
                mode,
            } => {
                if let (Some(a), Some(u)) = (self.agents.get_mut(&agent_id), Urgency::parse(&mode))
                    && a.queue.iter().any(|d| d.msg_id.as_ref() == Some(&msg_id))
                {
                    a.urgency = a.urgency.max(u);
                    a.urgent_id = Some(msg_id);
                }
            }
            HubFrame::SayReceipt {
                say_id,
                state,
                detail,
                ..
            } => {
                if let Some(tx) = self.says.remove(&say_id) {
                    let _ = tx.send((state, detail));
                }
            }
            HubFrame::Answer { agent_id, text, .. } => {
                if let Some(a) = self.agents.get_mut(&agent_id) {
                    a.queue.push(Delivery {
                        from: "answer".into(),
                        text,
                        thread: None,
                        msg_id: None,
                    });
                }
            }
            HubFrame::Raw { agent_id, text } => {
                if let Some(a) = self.agents.get_mut(&agent_id) {
                    a.raw.push_back(text);
                }
            }
            HubFrame::Decision {
                agent_id,
                perm_id,
                allow,
            } => self.decide(&agent_id, &perm_id, allow),
            HubFrame::Stop { agent_id } => self.stop(&agent_id).await,
            HubFrame::Screen { agent_id } => self.send_screen(&agent_id, "asked for").await,
            HubFrame::Restart { agent_id } => {
                let _ = self
                    .respawn(&agent_id, crate::now_ms(), "the chat was cleared")
                    .await;
            }
            HubFrame::Killall { project } => {
                let ids: Vec<String> = self
                    .agents
                    .values()
                    .filter(|a| project.as_ref().is_none_or(|p| &a.spec.project == p))
                    .map(|a| a.spec.agent_id.clone())
                    .collect();
                for id in ids {
                    self.stop(&id).await;
                }
            }
            HubFrame::Hold {
                on,
                agent_id,
                project,
            } => {
                for a in self.agents.values_mut() {
                    if agent_id.as_ref().is_none_or(|i| *i == a.spec.agent_id)
                        && project.as_ref().is_none_or(|p| *p == a.spec.project)
                    {
                        a.held = on;
                    }
                }
            }
            HubFrame::FileChunk {
                transfer_id,
                agent_id,
                from,
                name,
                seq,
                last,
                data,
                sha256,
                ..
            } => self.on_file(
                &agent_id,
                &transfer_id,
                &from,
                &name,
                seq,
                last,
                &data,
                sha256,
            ),
            HubFrame::Spawn {
                agent,
                command,
                pick,
            } => {
                // The agent goes in the folder of the `claudecord` that is waiting for it (the hub sent its code). Otherwise only in a
                // folder this machine already knows for the project (the hub never chooses a folder): one with no agent in it first, and if
                // every known folder is busy, the first one with its own git worktree.
                let waiting = pick
                    .as_deref()
                    .and_then(|c| self.pending.get(c))
                    .filter(|p| matches!(p.state, PendingState::Waiting))
                    .map(|p| (p.cwd.clone(), p.rows, p.cols, p.opts.clone()));
                let folders = self
                    .folders
                    .get(&agent.project)
                    .cloned()
                    .unwrap_or_default();
                let free = folders
                    .iter()
                    .find(|f| !self.agents.values().any(|a| &a.origin == *f))
                    .cloned();
                let known = free
                    .map(|f| (f, false))
                    .or_else(|| folders.first().cloned().map(|f| (f, true)));
                // A saved startup command is a line of shell sent by the hub: it runs only on a machine whose owner turned that on.
                let line = match command.as_deref() {
                    None => Ok(None),
                    Some(_) if !super::config::custom_commands_allowed(&self.dir) => Err(
                        "this machine does not run startup commands sent by the hub. Allow them with `claudecord settings custom-commands on`"
                            .to_string(),
                    ),
                    Some(c) => super::config::shell_argv(c).map(Some),
                };
                let result = match (line, waiting, known) {
                    (Err(why), ..) => Err(why),
                    (Ok(command), Some((cwd, rows, cols, ex)), _) => {
                        let folder = cwd.clone();
                        let r = self
                            .up_agent(
                                agent.project.clone(),
                                Some(agent.name.clone()),
                                agent.adapter.as_str().into(),
                                ex.model.clone().or(agent.model.clone()),
                                agent.role.clone(),
                                cwd,
                                if ex.policy.is_empty() {
                                    "ask".into()
                                } else {
                                    ex.policy
                                },
                                rows,
                                cols,
                                UpOpts {
                                    command,
                                    worktree: ex.worktree,
                                    pickup: ex.pickup,
                                    restart: ex.restart,
                                },
                            )
                            .await;
                        if r.ok {
                            if let Some(p) = pick.as_deref().and_then(|c| self.pending.get_mut(c)) {
                                p.state = PendingState::Started {
                                    agent: r.msg.clone(),
                                    worktree: r
                                        .data
                                        .as_ref()
                                        .and_then(|d| d["worktree"].as_str().map(String::from)),
                                };
                            }
                            self.remember_agent(
                                &agent.name,
                                (
                                    agent.project.clone(),
                                    agent.adapter.as_str().into(),
                                    agent.model.clone(),
                                    agent.role.clone(),
                                    folder,
                                ),
                            );
                            Ok(())
                        } else {
                            Err(r.msg)
                        }
                    }
                    (Ok(_), None, _) if pick.is_some() => Err(
                        "the `claudecord` this was for is not waiting here any more. Run it again"
                            .to_string(),
                    ),
                    (Ok(command), None, None) => {
                        let _ = command;
                        Err(format!(
                            "this machine has no folder for project {} yet. Run `claudecord` in one",
                            agent.project
                        ))
                    }
                    (Ok(command), None, Some((cwd, worktree))) => {
                        let r = self
                            .up_agent(
                                agent.project.clone(),
                                Some(agent.name.clone()),
                                agent.adapter.as_str().into(),
                                agent.model.clone(),
                                agent.role.clone(),
                                cwd.to_string_lossy().into(),
                                "ask".into(),
                                30,
                                100,
                                UpOpts {
                                    command,
                                    worktree,
                                    ..UpOpts::default()
                                },
                            )
                            .await;
                        if r.ok { Ok(()) } else { Err(r.msg) }
                    }
                };
                if let Err(reason) = result {
                    crate::warn!(
                        "daemon",
                        "{}: spawn from the hub failed: {reason}",
                        agent.agent_id
                    );
                    if let Some(p) = pick.as_deref().and_then(|c| self.pending.get_mut(c)) {
                        p.state = PendingState::Failed(reason.clone());
                    }
                    self.link
                        .send(NodeFrame::SpawnFailed {
                            project: agent.project.clone(),
                            name: agent.name.clone(),
                            reason: reason.chars().take(300).collect(),
                        })
                        .await;
                }
            }
            HubFrame::Moved { agent_id, project } => self.on_moved(&agent_id, &project),
            HubFrame::Update { latest } => {
                // Kept for the command line to mention the next time it runs (the daemon has no terminal to say it in).
                if crate::protocol::version_older(env!("CARGO_PKG_VERSION"), &latest) {
                    crate::info!(
                        "daemon",
                        "claudecord {latest} is available (this is {})",
                        env!("CARGO_PKG_VERSION")
                    );
                    let _ = std::fs::write(
                        self.dir.join("update.json"),
                        serde_json::json!({ "latest": latest }).to_string(),
                    );
                }
            }
            HubFrame::Welcome { .. } | HubFrame::Error { .. } | HubFrame::Ack { .. } => {}
        }
    }

    /// A person moved one of this machine's agents to another project. The agent keeps its id, folder, terminal and conversation; from now on
    /// what it receives and saves is kept under the new project, and the folder lists the new project too (the one used last is offered first).
    fn on_moved(&mut self, agent_id: &str, project: &str) {
        let Some(a) = self.agents.get_mut(agent_id) else {
            return;
        };
        let old = std::mem::replace(&mut a.spec.project, project.to_string());
        let (origin, name, adapter) = (
            a.origin.clone(),
            a.spec.name.clone(),
            a.spec.adapter.as_str().to_string(),
        );
        let (model, role) = (a.spec.model.clone(), a.spec.role.clone());
        let cwd = origin.to_string_lossy().into_owned();
        crate::info!(
            "daemon",
            "{agent_id}: moved from project {old} to {project}"
        );
        a.log.event(
            crate::now_ms(),
            "moved",
            &format!("from {old} to {project}"),
        );
        let list = self.folders.entry(project.to_string()).or_default();
        list.retain(|f| f != &origin);
        list.insert(0, origin.clone());
        let recent = self.last.entry(cwd.clone()).or_default();
        recent.retain(|p| p != project);
        recent.insert(0, project.to_string());
        let sorted: BTreeMap<String, Vec<PathBuf>> = self
            .folders
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let _ = super::config::write_projects(&self.dir, &sorted, &self.last);
        self.remember_agent(&name, (project.to_string(), adapter, model, role, cwd));
    }

    /// A chunk of a file from the hub. It is saved into the agent's inbox folder, and when complete the agent is told
    /// where it is in one short line.
    #[allow(clippy::too_many_arguments)]
    fn on_file(
        &mut self,
        agent_id: &str,
        transfer_id: &str,
        from: &str,
        name: &str,
        seq: u64,
        last: bool,
        data: &str,
        sha256: Option<String>,
    ) {
        let Some(a) = self.agents.get_mut(agent_id) else {
            return;
        };
        let dir = a.cwd.join(crate::protocol::inbox_dir(&a.spec.project));
        if std::fs::create_dir_all(&dir).is_err() {
            return;
        }
        // What is kept here is not for the project's history: git is told to ignore the whole `.claudecord` folder.
        if seq == 0 {
            let _ = std::fs::write(a.cwd.join(".claudecord/.gitignore"), "*\n");
        }
        // The file keeps its own name, so an agent finds the newest version where it expects it; the transfer id only tells the pieces apart.
        let done = dir.join(safe_name(name));
        let part = dir.join(format!(
            "{}-{}.part",
            safe_name(transfer_id),
            safe_name(name)
        ));
        let entry = a
            .files
            .entry(transfer_id.to_string())
            .or_insert_with(|| (part.clone(), 0));
        // Already failed: the rest of its pieces are ignored (the agent was told once).
        if entry.1 == u64::MAX {
            if last {
                a.files.remove(transfer_id);
            }
            return;
        }
        // A piece out of order (one was lost when the connection dropped, or the start never came) ruins the file: what was written is removed,
        // and the agent is told, so it can say so instead of waiting for a file that will never be whole.
        let decoded = base64::engine::general_purpose::STANDARD.decode(data);
        let in_order = seq == entry.1;
        use std::io::Write;
        let written = match (&decoded, in_order) {
            (Ok(bytes), true) => {
                let opened = if seq == 0 {
                    std::fs::File::create(&part)
                } else {
                    std::fs::OpenOptions::new().append(true).open(&part)
                };
                opened.and_then(|mut f| f.write_all(bytes)).is_ok()
            }
            _ => false,
        };
        if !written {
            let _ = std::fs::remove_file(&part);
            if last {
                a.files.remove(transfer_id);
            } else {
                a.files.insert(transfer_id.to_string(), (part, u64::MAX));
            }
            a.queue.push(Delivery {
                from: from.to_string(),
                text: format!(
                    "[file {} did not arrive complete: ask for it again]",
                    safe_name(name)
                ),
                thread: None,
                msg_id: None,
            });
            return;
        }
        entry.1 += 1;
        if last {
            a.files.remove(transfer_id);
            // The whole file is checked against the sender's SHA-256 before it is kept.
            if let Some(want) = &sha256
                && std::fs::read(&part)
                    .map(|b| crate::agents::text::sha256_hex(&b))
                    .ok()
                    .as_ref()
                    != Some(want)
            {
                let _ = std::fs::remove_file(&part);
                a.queue.push(Delivery {
                    from: from.to_string(),
                    text: format!(
                        "[file {} arrived damaged (its checksum did not match): ask for it again]",
                        safe_name(name)
                    ),
                    thread: None,
                    msg_id: None,
                });
                return;
            }
            if let Err(e) = std::fs::rename(&part, &done).or_else(|_| {
                std::fs::copy(&part, &done).map(|_| {
                    let _ = std::fs::remove_file(&part);
                })
            }) {
                let _ = std::fs::remove_file(&part);
                a.queue.push(Delivery {
                    from: from.to_string(),
                    text: format!(
                        "[file {} could not be saved ({e}): ask for it again]",
                        safe_name(name)
                    ),
                    thread: None,
                    msg_id: None,
                });
                return;
            }
            let path = done;
            let rel = path
                .strip_prefix(&a.cwd)
                .unwrap_or(&path)
                .to_string_lossy()
                .into_owned();
            a.queue.push(Delivery {
                from: from.to_string(),
                text: format!("[file {} at {rel}]", safe_name(name)),
                thread: None,
                msg_id: None,
            });
        }
    }

    /// The hub decided a permission request. Pick the matching option on the prompt that is showing.
    fn decide(&mut self, agent_id: &str, perm_id: &str, allow: bool) {
        let Some(a) = self.agents.get_mut(agent_id) else {
            return;
        };
        let Some(d) = a.deciding.take().filter(|d| d.perm_id == perm_id) else {
            return;
        };
        // ponytail: option 0 is "yes" and the last option is "no" on the harnesses seen so far. A prompt that differs
        // would need its own rule in the adapter.
        let index = if allow {
            0
        } else {
            d.prompt.options.len().saturating_sub(1) as i64
        };
        a.log.event(
            crate::now_ms(),
            "decision",
            &format!(
                "{} for: {}",
                if allow { "allowed" } else { "denied" },
                d.prompt.question
            ),
        );
        for k in a.spec.adapter.select_keys(&d.prompt, index) {
            let _ = a.proc.type_input(&key_bytes(&k), crate::now_ms());
        }
    }

    /// Quits an agent and tells the hub it is gone.
    async fn stop(&mut self, agent_id: &str) {
        if let Some(a) = self.agents.remove(agent_id) {
            a.proc.kill();
            self.link
                .send(NodeFrame::AgentGone {
                    agent_id: agent_id.to_string(),
                })
                .await;
        }
    }

    /// An agent's program ended without being told to. Start it again (a fresh session, which asks for a handoff to carry
    /// on from) unless it has already died too often lately, in which case it is stopped and the hub is told.
    async fn revive_or_stop(&mut self, id: &str, now: i64) {
        let Some(a) = self.agents.get_mut(id) else {
            return;
        };
        while a
            .restarts
            .front()
            .is_some_and(|t| now - t > RESTART_WINDOW_MS)
        {
            a.restarts.pop_front();
        }
        if a.restarts.len() >= a.restart_budget {
            a.log.event(now, "ended", "the agent's program ended");
            crate::warn!(
                "daemon",
                "{id}: the agent's program ended and is not set to restart"
            );
            self.stop(id).await;
            return;
        }
        crate::warn!(
            "daemon",
            "{id}: the agent's program ended; starting it again"
        );
        if self
            .respawn(id, now, "the agent's program ended and was started again")
            .await
        {
            if let Some(a) = self.agents.get_mut(id) {
                a.restarts.push_back(now);
            }
            self.link
                .send(NodeFrame::AgentPickup {
                    agent_id: id.to_string(),
                })
                .await;
        } else {
            self.stop(id).await;
        }
    }

    /// Starts an agent's program again in a new terminal, with the same settings, in place of the old one (which is closed first: a terminal of
    /// the same name cannot be made while the old one is still there). The agent stays registered with the hub. False if it could not be started.
    async fn respawn(&mut self, id: &str, now: i64, why: &str) -> bool {
        let Some(a) = self.agents.get_mut(id) else {
            return false;
        };
        let l = a.launch.clone();
        a.proc.kill();
        let guard = super::inject::Guard::with_times(now, self.opts.quiet.0, self.opts.quiet.1);
        let spawn = Spawn {
            name: id,
            argv: &l.argv,
            cwd: a.cwd.clone(),
            remove_env: &l.secrets,
            add_env: &l.env,
            rows: l.rows,
            cols: l.cols,
        };
        match Terminal::spawn(&self.opts.backend, &spawn, guard) {
            Ok(p) => {
                watch_terminal(&p, &a.log);
                a.log.event(now, "restarted", why);
                a.proc = p;
                a.queue.splice(0..0, intro(&a.spec));
                a.status = AgentStatus::Starting;
                a.inflight = None;
                a.shown_prompt = None;
                a.deciding = None;
                a.limit_reported = false;
                self.link
                    .send(NodeFrame::AgentStatus {
                        agent_id: id.to_string(),
                        status: AgentStatus::Starting,
                        detail: Some("restarted".into()),
                    })
                    .await;
                true
            }
            Err(e) => {
                crate::error!("daemon", "{id}: could not start it again: {e}");
                false
            }
        }
    }

    /// Looks at every terminal: reports ended agents, status changes, limits and prompts, pastes waiting messages when it
    /// is safe, and says when a pasted message seems to have been taken up.
    async fn on_tick(&mut self, now: i64) {
        // A `say` that stopped waiting leaves its slot behind: clear them.
        self.says.retain(|_, tx| !tx.is_closed());
        // Files people sent to the agents pile up in each folder's `.claudecord/files`: old ones are removed, once an hour.
        if now - self.tidied > 3_600_000 {
            self.tidied = now;
            let dirs: Vec<PathBuf> = self
                .agents
                .values()
                .map(|a| a.cwd.join(crate::protocol::inbox_dir(&a.spec.project)))
                .collect();
            let _ = tokio::task::spawn_blocking(move || {
                for d in dirs {
                    tidy_files(&d, FILES_KEEP_MS, FILES_KEEP_BYTES);
                }
            })
            .await;
        }
        // With no agent running, the daemon (which holds the connection to the hub) goes away by itself after a short while, unless the person
        // chose to keep it running (`claudecord settings keep-running on`).
        if let Some(limit) = self.opts.idle_exit {
            // A `claudecord` still waiting for the hub counts as something to wait for.
            self.pending.retain(|_, p| now - p.at < PENDING_TTL_MS);
            let waiting = self
                .pending
                .values()
                .any(|p| matches!(p.state, PendingState::Waiting));
            if self.agents.is_empty() && !waiting && !super::config::keep_running(&self.dir) {
                let since = *self.idle_since.get_or_insert(now);
                if now - since > limit.as_millis() as i64 {
                    crate::info!("daemon", "no agent is running, so this machine disconnects");
                    self.quit = true;
                }
            } else {
                self.idle_since = None;
            }
        }
        // Agents that all sit idle for a long time are not worth keeping: the daemon ends them and goes. Idle means ready for input with
        // nothing queued or in flight, and a screen that has not changed for the whole time (so nobody is typing in it either).
        if let Some(limit) = self.opts.idle_agents_exit
            && !self.agents.is_empty()
            && !super::config::keep_running(&self.dir)
            && self.agents.values().all(|a| {
                a.status == AgentStatus::Idle
                    && a.queue.is_empty()
                    && a.raw.is_empty()
                    && a.inflight.is_none()
                    && now - a.still.1 > limit.as_millis() as i64
            })
        {
            crate::info!(
                "daemon",
                "every agent has been idle for {} minutes, so they are closed and this machine disconnects (`claudecord settings keep-running on` prevents this)",
                limit.as_secs() / 60
            );
            self.quit = true;
        }
        let ids: Vec<String> = self.agents.keys().cloned().collect();
        // Looking at a tmux session runs the tmux program, which waits for the operating system. All of them are looked at at once on
        // threads meant for blocking work, so many agents never hold up the daemon's own connections.
        for a in self.agents.values().filter(|a| !a.queue.is_empty()) {
            a.proc.hurry();
        }
        let looks: Vec<_> = self
            .agents
            .values()
            .filter(|a| matches!(a.proc, Terminal::Tmux(_)))
            .map(|a| {
                let t = a.proc.clone();
                tokio::task::spawn_blocking(move || t.observe(now))
            })
            .collect();
        for look in looks {
            let _ = look.await;
        }
        for id in ids {
            // Each agent is looked at on its own, guarded: a bug while looking at one costs that agent's turn, is reported, and the
            // others are looked at as usual. An agent that keeps failing is stopped and reported rather than looped on.
            match crate::task::guarded(&format!("looking at {id}"), self.tick_agent(&id, now)).await
            {
                Some(()) => {
                    if let Some(a) = self.agents.get_mut(&id) {
                        a.faults = 0;
                    }
                }
                None => self.agent_faulted(&id).await,
            }
        }
    }

    /// An agent's look failed with a bug. Tell the hub (so the chat says so) and, after three in a row, stop the agent.
    async fn agent_faulted(&mut self, id: &str) {
        let Some(a) = self.agents.get_mut(id) else {
            return;
        };
        a.faults += 1;
        let (faults, status) = (a.faults, a.status);
        a.log.event(
            crate::now_ms(),
            "fault",
            "an internal error while looking at this agent",
        );
        let detail = if faults >= 3 {
            "stopped after repeated internal errors on this machine; see the daemon log".to_string()
        } else {
            "an internal error on this machine while looking at this agent; see the daemon log"
                .to_string()
        };
        self.link
            .send(NodeFrame::AgentStatus {
                agent_id: id.to_string(),
                status: if faults >= 3 {
                    AgentStatus::Offline
                } else {
                    status
                },
                detail: Some(detail),
            })
            .await;
        if faults >= 3 {
            crate::error!(
                "daemon",
                "{id}: stopped after {faults} internal errors in a row"
            );
            self.stop(id).await;
        }
    }

    /// One agent's turn in the tick: its end, its screen, its prompts, and delivering what waits for it.
    async fn tick_agent(&mut self, id: &str, now: i64) {
        if self.agents[id].proc.has_exited() {
            self.revive_or_stop(id, now).await;
            return;
        }
        let (screen, adapter) = {
            let a = &self.agents[id];
            (a.proc.screen_text(), a.spec.adapter)
        };
        let state = adapter.detect(&screen);
        let mut out: Vec<NodeFrame> = Vec::new();
        let mut shots: Vec<&str> = Vec::new();
        {
            let a = self.agents.get_mut(id).expect("listed above");
            if let Some(p) = &state.prompt {
                // Start-up dialogs such as "trust this folder" are only answered by the device if it was told to; by
                // default they go to a person like any other prompt.
                if let Some(i) = adapter.startup_choice(p).filter(|_| self.opts.auto_startup) {
                    if a.shown_prompt.as_deref() != Some(&p.signature) && i >= 0 {
                        a.shown_prompt = Some(p.signature.clone());
                        for k in adapter.select_keys(p, i) {
                            let _ = a.proc.type_input(&key_bytes(&k), now);
                        }
                    }
                } else if a.shown_prompt.as_deref() != Some(&p.signature)
                    && a.candidate_prompt.as_deref() != Some(&p.signature)
                {
                    a.candidate_prompt = Some(p.signature.clone());
                } else if a.shown_prompt.as_deref() != Some(&p.signature) {
                    // Anything else is a question for a person. It becomes a permission request in the chat.
                    a.shown_prompt = Some(p.signature.clone());
                    let perm_id = format!("{}-{:x}", a.spec.name, fingerprint(&p.signature));
                    a.deciding = Some(AwaitingDecision {
                        perm_id: perm_id.to_string(),
                        prompt: p.clone(),
                    });
                    shots.push("is asking for permission");
                    out.push(NodeFrame::AgentPermission {
                        agent_id: id.to_string(),
                        perm_id,
                        kind: "tool".into(),
                        action: p.question.clone(),
                        thread: None,
                    });
                }
            } else if a.shown_prompt.is_none() {
                a.candidate_prompt = None;
            } else if let Some(shown) = a.shown_prompt.take() {
                a.candidate_prompt = None;
                // The prompt is gone. If a decision was still pending, it was answered here at the terminal.
                if let Some(d) = a.deciding.take() {
                    out.push(NodeFrame::AgentPermissionDone {
                        agent_id: id.to_string(),
                        perm_id: d.perm_id,
                    });
                }
                let _ = shown;
            }
            let status = status_of(&state);
            // A turn ended: the agent is no longer owed an answer. No picture of the terminal is sent unasked, only when the agent asks a person for
            // permission or runs into a usage limit, or when a person asks for one.
            if status == AgentStatus::Idle && a.status != AgentStatus::Idle {
                a.awaiting = None;
            }
            // When the screen last changed (the idle shutdown goes by it).
            let hash = a.proc.screen_hash();
            if hash != a.still.0 {
                a.still = (hash, now);
            }
            if status != a.status {
                a.log.event(now, "status", &format!("{status:?}"));
                a.status = status;
                out.push(NodeFrame::AgentStatus {
                    agent_id: id.to_string(),
                    status,
                    detail: None,
                });
            }
            match &state.limit {
                Some(l) if !a.limit_reported => {
                    a.limit_reported = true;
                    shots.push("hit a usage limit");
                    out.push(NodeFrame::AgentLimit {
                        agent_id: id.to_string(),
                        kind: l.kind,
                        resets_at: l.resets_at.clone(),
                    });
                }
                None => a.limit_reported = false,
                _ => {}
            }
            // Harness commands typed by a person (the raw queue) go first, one at a time, exactly as written.
            if a.inflight.is_none()
                && state.prompt.is_none()
                && let Some(text) = a.raw.front().cloned()
                && a.proc.inject(&text, now).is_ok()
            {
                a.log.event(now, "raw", &text);
                a.raw.pop_front();
            }
            // Paste what is waiting, as one input, when it is safe.
            if !a.held
                && a.inflight.is_none()
                && !a.queue.is_empty()
                && state.limit.is_none()
                && state.prompt.is_none()
                // Not before the agent's input box is showing, or start-up would swallow the message.
                && (state.ready || a.status != AgentStatus::Starting)
            {
                // One thread at a time: what waited in different threads is not mixed into one prompt, so the agent answers each where it was asked.
                let n = a
                    .queue
                    .iter()
                    .take_while(|d| d.thread == a.queue[0].thread)
                    .count();
                let text = format_deliveries(&a.queue[..n]);
                let before = a.proc.screen_hash();
                // Steering stops what the agent is doing first, then gives it a moment before the paste.
                if a.urgency == Urgency::Steer && a.steered_at.is_none() {
                    let _ = a.proc.interrupt(now);
                    a.steered_at = Some(now);
                }
                let settled = a.steered_at.is_none_or(|t| now - t >= STEER_SETTLE_MS);
                if settled && a.proc.inject_as(&text, now, a.urgency).is_ok() {
                    a.log.event(now, "delivered", &text);
                    let ids: Vec<String> = a.queue.drain(..n).filter_map(|d| d.msg_id).collect();
                    if a.urgent_id.as_ref().is_none_or(|u| ids.contains(u)) {
                        a.urgency = Urgency::Queue;
                        a.urgent_id = None;
                        a.steered_at = None;
                    }
                    a.inflight = Some(Inflight {
                        ids,
                        since: now,
                        screen_before: before,
                    });
                }
            }
            // The agent counts as having taken a message up once its screen changes, or after a short wait.
            if let Some(f) = &a.inflight {
                let changed = a.proc.screen_hash() != f.screen_before;
                if changed && now - f.since > 300
                    || now - f.since > self.opts.accept_after.as_millis() as i64
                {
                    let f = a.inflight.take().expect("checked above");
                    if !f.ids.is_empty() {
                        a.log.event(now, "accepted", &f.ids.join(","));
                        out.push(NodeFrame::AgentAccepted {
                            agent_id: id.to_string(),
                            msg_ids: f.ids,
                        });
                    }
                }
            }
        }
        for f in out {
            self.link.send(f).await;
        }
        for why in shots {
            // One picture by itself per agent per five minutes, however many reasons there are.
            let Some(a) = self.agents.get_mut(id) else {
                break;
            };
            if now - a.shot_at < 300_000 {
                continue;
            }
            a.shot_at = now;
            self.send_screen(id, why).await;
        }
    }

    /// One request from the command line or an agent's shell.
    async fn handle(&mut self, req: Req, key: Option<&str>) -> Resp {
        // Someone or something is using this machine, so if the hub connection is resting, bring it back now.
        self.link.nudge();
        // An agent's own commands must carry that agent's secret key, so no agent can speak, ask or finish work for another.
        if let Some((agent, kind, text)) = describe(&req) {
            let Some(a) = self.agents.get(agent) else {
                return Resp::err("no such agent here");
            };
            if key != Some(a.key.as_str()) {
                return Resp::err(
                    "agent commands must be run by the agent itself (its key was missing or wrong)",
                );
            }
            a.log.event(crate::now_ms(), kind, &text);
        }
        match req {
            Req::Logs {
                agent,
                lines,
                terminal,
            } => match self.agents.get(&agent) {
                Some(a) => {
                    let rows = if terminal {
                        a.log.tail_terminal(lines)
                    } else {
                        a.log.tail_events(lines)
                    };
                    Resp {
                        ok: true,
                        msg: format!("{} line(s)", rows.len()),
                        data: Some(serde_json::json!({ "lines": rows })),
                    }
                }
                None => Resp::err("no such agent here"),
            },
            Req::Ping => Resp::ok("pong"),
            Req::Shutdown => {
                self.quit = true;
                Resp::ok("bye")
            }
            Req::List => {
                let list: Vec<serde_json::Value> = self.agents.values().map(|a| serde_json::json!({"agent": a.spec.agent_id, "status": a.status, "cwd": a.cwd})).collect();
                Resp {
                    ok: true,
                    msg: format!("{} agent(s)", list.len()),
                    data: Some(serde_json::Value::Array(list)),
                }
            }
            Req::Expect {
                code,
                cwd,
                rows,
                cols,
                opts,
            } => {
                self.pending.insert(
                    code,
                    PendingStart {
                        cwd,
                        rows,
                        cols,
                        opts,
                        at: crate::now_ms(),
                        state: PendingState::Waiting,
                    },
                );
                Resp::ok("waiting for the hub")
            }
            Req::Pending { code } => match self.pending.get(&code) {
                None => Resp::err("nothing is waiting under that code"),
                Some(p) => {
                    let data = match &p.state {
                        PendingState::Waiting => serde_json::json!({ "state": "waiting" }),
                        PendingState::Started { agent, worktree } => {
                            serde_json::json!({ "state": "started", "agent": agent, "worktree": worktree })
                        }
                        PendingState::Failed(why) => {
                            serde_json::json!({ "state": "failed", "error": why })
                        }
                    };
                    Resp {
                        ok: true,
                        msg: String::new(),
                        data: Some(data),
                    }
                }
            },
            Req::Up { .. } if !self.opts.dev_spawn => Resp::err(
                "agents are started by the hub, never from this machine: run `claudecord` and press Start on the dashboard page it opens",
            ),
            Req::Up {
                project,
                name,
                adapter,
                model,
                role,
                cwd,
                policy,
                rows,
                cols,
                opts,
            } => {
                let asked = (
                    project.clone(),
                    adapter.clone(),
                    model.clone(),
                    role.clone(),
                    cwd.clone(),
                );
                let resp = self
                    .up_agent(
                        project, name, adapter, model, role, cwd, policy, rows, cols, opts,
                    )
                    .await;
                // Only agents a person started here (not the ones the hub asked for) are kept, to be started again later.
                if resp.ok {
                    self.remember_agent(resp.msg.rsplit('/').next().unwrap_or(&resp.msg), asked);
                }
                resp
            }
            Req::Restart { agent } => {
                let ids: Vec<String> = match agent {
                    Some(a) => vec![a],
                    None => self.agents.keys().cloned().collect(),
                };
                if let Some(missing) = ids.iter().find(|i| !self.agents.contains_key(*i)) {
                    return Resp::err(format!("no agent called {missing} here"));
                }
                let now = crate::now_ms();
                let mut done = 0;
                for id in &ids {
                    if self.respawn(id, now, "restarted on request").await {
                        done += 1;
                    }
                }
                if done == ids.len() {
                    Resp::ok(format!("restarted {done} agent(s)"))
                } else {
                    Resp::err(format!(
                        "restarted {done} of {} agents; see the daemon log",
                        ids.len()
                    ))
                }
            }
            Req::Stop { agent } => {
                if self.agents.contains_key(&agent) {
                    self.stop(&agent).await;
                    Resp::ok("stopped")
                } else {
                    Resp::err("no such agent here")
                }
            }
            Req::Attach { agent } => match self.agents.get(&agent) {
                Some(a) => match (a.proc.attach_command(), a.proc.pty()) {
                    // tmux: the command line runs `tmux attach` itself.
                    (Some(cmd), _) => Resp {
                        ok: true,
                        msg: "attach with tmux".into(),
                        data: Some(serde_json::json!({"exec": cmd})),
                    },
                    (None, Some(pty)) => {
                        ATTACH.locked().insert(agent, pty);
                        Resp::ok("attached")
                    }
                    _ => Resp::err("this agent has no terminal to attach to"),
                },
                None => Resp::err("no such agent here"),
            },
            Req::Say {
                agent,
                text,
                thread,
            } => {
                self.say_seq += 1;
                let say_id = format!("s{}-{:x}", self.say_seq, fingerprint(&text));
                let (tx, rx) = oneshot::channel();
                self.says.insert(say_id.clone(), tx);
                let resp = self
                    .verb(&agent, |id| NodeFrame::AgentSay {
                        agent_id: id,
                        text,
                        thread,
                        say_id: Some(say_id.clone()),
                    })
                    .await;
                if resp.ok {
                    self.defer = Some(rx);
                } else {
                    self.says.remove(&say_id);
                }
                resp
            }
            Req::Assign {
                agent,
                to,
                task,
                thread,
            } => {
                self.verb(&agent, |id| NodeFrame::AgentAssign {
                    agent_id: id,
                    to,
                    task,
                    thread,
                })
                .await
            }
            Req::Done {
                agent,
                task,
                summary,
            } => {
                self.verb(&agent, |id| NodeFrame::AgentTaskDone {
                    agent_id: id,
                    task_id: task,
                    summary,
                })
                .await
            }
            Req::Report {
                agent,
                title,
                summary,
                artifacts,
            } => {
                self.verb(&agent, |id| NodeFrame::AgentReport {
                    agent_id: id,
                    title,
                    summary,
                    artifacts,
                })
                .await
            }
            Req::Answer { agent, ask, text } => {
                self.verb(&agent, |id| NodeFrame::AgentAnswer {
                    agent_id: id,
                    ask,
                    text,
                })
                .await
            }
            Req::Dump { agent, text } => {
                self.verb(&agent, |id| NodeFrame::AgentHandoff { agent_id: id, text })
                    .await
            }
            Req::Pickup { agent } => {
                self.verb(&agent, |id| NodeFrame::AgentPickup { agent_id: id })
                    .await
            }
            Req::Team { agent } => {
                self.verb(&agent, |id| NodeFrame::AgentTeam { agent_id: id })
                    .await
            }
            Req::Permission {
                agent,
                kind,
                action,
            } => {
                let n = self.agents.get_mut(&agent).map(|a| {
                    a.asks += 1;
                    a.asks
                });
                match n {
                    Some(n) => {
                        let perm_id = format!("{agent}-p{n}");
                        self.link
                            .send(NodeFrame::AgentPermission {
                                agent_id: agent,
                                perm_id: perm_id.clone(),
                                kind,
                                action,
                                thread: None,
                            })
                            .await;
                        Resp::ok(perm_id)
                    }
                    None => Resp::err("no such agent here"),
                }
            }
            Req::Usage { agent, kind, pct } => {
                let kind = if kind == "session" {
                    UsageKind::Session
                } else {
                    UsageKind::Context
                };
                self.verb(&agent, |id| NodeFrame::AgentUsage {
                    agent_id: id,
                    kind,
                    pct,
                })
                .await
            }
            Req::Ask {
                agent,
                question,
                options,
                thread,
            } => {
                let n = self.agents.get_mut(&agent).map(|a| {
                    a.asks += 1;
                    a.asks
                });
                match n {
                    Some(n) => {
                        let ask_id = format!("{agent}-{n}");
                        self.link
                            .send(NodeFrame::AgentAsk {
                                agent_id: agent,
                                ask_id: ask_id.clone(),
                                question,
                                options,
                                thread,
                            })
                            .await;
                        // The agent does not wait: it ends its turn, and the answer arrives as its next input.
                        Resp::ok(format!(
                            "queued as {ask_id}. End your turn: the answer will arrive as your next message."
                        ))
                    }
                    None => Resp::err("no such agent here"),
                }
            }
            Req::Send {
                agent,
                path,
                to,
                caption,
            } => self.send_file(&agent, &path, to, caption).await,
        }
    }

    /// Sends a frame for an agent that lives on this machine.
    async fn verb(&mut self, agent: &str, make: impl FnOnce(String) -> NodeFrame) -> Resp {
        if !self.agents.contains_key(agent) {
            return Resp::err("no such agent here");
        }
        if let Some(a) = self.agents.get_mut(agent) {
            a.awaiting = None;
        }
        self.link.send(make(agent.to_string())).await;
        Resp::ok("sent")
    }

    /// Reads a file from disk and sends it in chunks. Over the size limit is refused here, before anything is sent.
    async fn send_file(
        &mut self,
        agent: &str,
        path: &str,
        to: Option<String>,
        caption: Option<String>,
    ) -> Resp {
        let Some(a) = self.agents.get(agent) else {
            return Resp::err("no such agent here");
        };
        let full = a.cwd.join(path);
        let Ok(data) = std::fs::read(&full) else {
            return Resp::err("cannot read that file");
        };
        if data.len() > MAX_FILE_BYTES {
            return Resp::err(format!("over the {} MB limit", MAX_FILE_BYTES / 1_048_576));
        }
        let name = full
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "file".into());
        let n = data.len();
        self.send_bytes(agent, name.clone(), data, to, caption)
            .await;
        Resp::ok(format!("sent {name} ({} KB)", n.div_ceil(1024)))
    }

    /// Sends a file's bytes to the chat (or to a peer) in chunks.
    async fn send_bytes(
        &mut self,
        agent: &str,
        name: String,
        data: Vec<u8>,
        to: Option<String>,
        caption: Option<String>,
    ) {
        let transfer_id = format!(
            "{agent}-{:x}",
            fingerprint(&format!("{name}{}", crate::now_ms()))
        );
        let total = data.len().div_ceil(FILE_CHUNK_BYTES).max(1);
        let sum = crate::agents::text::sha256_hex(&data);
        for seq in 0..total {
            let slice = &data[(seq * FILE_CHUNK_BYTES).min(data.len())
                ..((seq + 1) * FILE_CHUNK_BYTES).min(data.len())];
            self.link
                .send(NodeFrame::FileChunk {
                    transfer_id: transfer_id.clone(),
                    agent_id: agent.to_string(),
                    name: name.clone(),
                    seq: seq as u64,
                    last: seq == total - 1,
                    data: base64::engine::general_purpose::STANDARD.encode(slice),
                    sha256: (seq == total - 1).then(|| sum.clone()),
                    to: to.clone(),
                    caption: caption.clone(),
                    thread: None,
                })
                .await;
        }
    }

    /// Where an agent should work. Normally the folder it was asked for. If another agent here already works in that folder, two
    /// agents would see and change each other's files, so this is refused unless the person asked (`worktree`) for a separate git
    /// worktree on its own branch, which is then made.
    fn isolate(
        &self,
        origin: &std::path::Path,
        project: &str,
        name: &str,
        worktree: bool,
    ) -> Result<String, String> {
        let shared = self.agents.values().any(|a| a.origin == origin);
        if !shared {
            return Ok(origin.to_string_lossy().into_owned());
        }
        // Another agent works here and two must not share a folder, so this one gets its own git worktree, as a spawn does. (`worktree` asks for
        // one even when the folder is free; here it is made either way.)
        let _ = worktree;
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .arg("-C")
                .arg(origin)
                .args(args)
                .output()
        };
        if !git(&["rev-parse", "--is-inside-work-tree"]).is_ok_and(|o| o.status.success()) {
            return Err("another agent already works in this folder, and two agents must not share one. This folder is not a git repository, so this one cannot get its own copy: start it in a different folder".into());
        }
        let root = origin.join(".claudecord");
        let _ = std::fs::create_dir_all(&root);
        // Keep claudeCord's own files out of the project's git status.
        let _ = std::fs::write(root.join(".gitignore"), "*\n");
        let tree =
            root.join("worktrees")
                .join(format!("{}-{}", safe_name(project), safe_name(name)));
        let branch = format!("cc/{}-{}", safe_name(project), safe_name(name));
        match git(&["worktree", "add", "-b", &branch, &tree.to_string_lossy()]) {
            Ok(o) if o.status.success() => Ok(tree.to_string_lossy().into_owned()),
            Ok(o) => Err(format!(
                "git could not make the worktree: {}",
                String::from_utf8_lossy(&o.stderr).trim()
            )),
            Err(e) => Err(format!("could not run git: {e}")),
        }
    }

    /// Posts what an agent's terminal shows (the last lines, secrets removed) to the chat, with the reason.
    async fn send_screen(&mut self, id: &str, why: &str) {
        let Some(a) = self.agents.get(id) else {
            return;
        };
        // A picture of the terminal when tmux can give one (taken off the main loop: it waits on tmux), otherwise the text.
        let term = a.proc.clone();
        if let Ok(Some(png)) = tokio::task::spawn_blocking(move || term.picture()).await {
            let name = a.spec.name.clone();
            self.send_bytes(
                id,
                format!("screen-{name}.png"),
                png,
                None,
                Some(why.to_string()),
            )
            .await;
            return;
        }
        let Some(a) = self.agents.get(id) else {
            return;
        };
        let text = shot(&a.proc.screen_text());
        self.link
            .send(NodeFrame::AgentScreen {
                agent_id: id.to_string(),
                why: why.to_string(),
                text,
            })
            .await;
    }

    /// Keeps an agent a person started in agents.json (one per project and name), so it can be started again later.
    fn remember_agent(
        &self,
        name: &str,
        (project, adapter, model, role, cwd): (
            String,
            String,
            Option<String>,
            Option<String>,
            String,
        ),
    ) {
        let file = self.dir.join("agents.json");
        let mut saved: Vec<serde_json::Value> = std::fs::read_to_string(&file)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default();
        let project = project_slug(&project);
        saved.retain(|a| !(a["project"] == project.as_str() && a["name"] == name));
        saved.push(serde_json::json!({"project": project, "name": name, "adapter": adapter, "model": model, "role": role, "cwd": cwd}));
        let _ = std::fs::write(&file, serde_json::to_string(&saved).expect("plain data"));
    }

    /// Starts an agent in a folder and registers it with the hub. Nothing else happens by itself: no handoff is asked for, no start-up
    /// dialog is answered, no restart is made, unless the person said so in the options.
    #[allow(clippy::too_many_arguments)]
    async fn up_agent(
        &mut self,
        project: String,
        name: Option<String>,
        adapter: String,
        model: Option<String>,
        role: Option<String>,
        cwd: String,
        policy: String,
        rows: u16,
        cols: u16,
        opts: UpOpts,
    ) -> Resp {
        let adapter = match adapter.as_str() {
            "claude" => AdapterId::Claude,
            "agy" => AdapterId::Agy,
            "codex" => AdapterId::Codex,
            other => return Resp::err(format!("unknown agent type {other}")),
        };
        let project = project_slug(&project);
        let taken: HashSet<String> = self
            .agents
            .values()
            .filter(|a| a.spec.project == project)
            .map(|a| a.spec.name.clone())
            .collect();
        let name = match name {
            Some(n) => match crate::protocol::agent_name_problem(&n) {
                None => n.to_ascii_lowercase(),
                Some(why) => return Resp::err(why),
            },
            None => auto_name(&taken, |n| {
                let mut r = [0u8; 4];
                let _ = getrandom::fill(&mut r);
                u32::from_le_bytes(r) as usize % n
            }),
        };
        if taken.contains(&name) {
            return Resp::err(format!("{name} is already running in {project}"));
        }
        if self.agents.len() >= self.opts.max_agents {
            return Resp::err(format!(
                "this machine already runs {} agents, which is its limit. Stop one, or set CLAUDECORD_MAX_AGENTS",
                self.agents.len()
            ));
        }
        if !std::path::Path::new(&cwd).is_dir() {
            return Resp::err(format!("{cwd} is not a folder on this machine"));
        }
        let origin = PathBuf::from(&cwd);
        // Two agents in one folder would trample each other's files and see each other's work. That is refused unless the
        // person asked for a separate git worktree for this one.
        let cwd = match self.isolate(&origin, &project, &name, opts.worktree) {
            Ok(c) => c,
            Err(e) => return Resp::err(e),
        };
        // Set when this agent was given a worktree of its own because another one works in the folder it was started in.
        let own_tree = (std::path::Path::new(&cwd) != origin).then(|| cwd.clone());
        let agent_id = format!("{project}/{name}");
        let pol = match policy.as_str() {
            "plan" => Policy::Plan,
            "ask" => Policy::Ask,
            "autonomous" => Policy::Autonomous,
            // A typo must not quietly give the agent the most freedom.
            other => return Resp::err(format!("unknown policy {other}: autonomous, plan or ask")),
        };
        let argv = match opts.command.clone().filter(|c| !c.is_empty()) {
            Some(c) => c,
            None => adapter.argv(&LaunchCtx {
                name: &name,
                model: model.as_deref(),
                policy: pol,
                mcp_config: None,
            }),
        };
        let secrets = secret_env_names(
            std::env::vars()
                .collect::<Vec<_>>()
                .iter()
                .map(|(k, v)| (k.as_str(), v.as_str())),
            adapter.as_str(),
        );
        let exe_dir = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(PathBuf::from));
        // The folders searched for programs: the ones asked for, claudeCord's own folder, then the usual path. Joined the
        // way this system joins them (a colon, or a semicolon on Windows).
        let mut path_dirs: Vec<PathBuf> = self.opts.extra_path.clone();
        path_dirs.extend(exe_dir);
        path_dirs.extend(std::env::split_paths(
            &std::env::var_os("PATH").unwrap_or_default(),
        ));
        let path = std::env::join_paths(path_dirs)
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        // The secret this agent's own commands carry. Only this agent's environment holds it.
        let mut key_bytes = [0u8; 16];
        let _ = getrandom::fill(&mut key_bytes);
        let key: String = key_bytes.iter().map(|b| format!("{b:02x}")).collect();
        let env = vec![
            ("CLAUDECORD_AGENT".to_string(), agent_id.clone()),
            ("CLAUDECORD_AGENT_KEY".to_string(), key.clone()),
            (
                "CLAUDECORD_HOME".to_string(),
                self.dir.to_string_lossy().into_owned(),
            ),
            ("PATH".to_string(), path.clone()),
        ];
        // Better to say so now than to start a terminal that shows "command not found" and ends.
        if !find_program(&argv[0], &path) {
            return Resp::err(super::doctor::program_missing(&argv[0]));
        }
        let spawn = Spawn {
            name: &agent_id,
            argv: &argv,
            cwd: PathBuf::from(&cwd),
            remove_env: &secrets,
            add_env: &env,
            rows: rows.max(5),
            cols: cols.max(20),
        };
        let guard =
            super::inject::Guard::with_times(crate::now_ms(), self.opts.quiet.0, self.opts.quiet.1);
        let proc = match Terminal::spawn(&self.opts.backend, &spawn, guard) {
            Ok(p) => p,
            Err(e) => {
                crate::error!("daemon", "{agent_id}: could not start {}: {e}", argv[0]);
                return Resp::err(format!("could not start {}: {e}", argv[0]));
            }
        };
        crate::info!("daemon", "{agent_id}: started {} in {cwd}", argv[0]);
        // Remember the folder, so the hub can start more agents for this project here later. Only now that the agent is running: a start that was
        // refused or failed must not leave the folder attached to the project.
        // A folder may serve several projects; each keeps the folder in its own list, and the folder remembers which project came last (what
        // the menu offers first). Nothing of one project is shared with another's: messages, files and notes are all kept per project.
        let list = self.folders.entry(project.clone()).or_default();
        list.retain(|f| f != &origin);
        list.insert(0, origin.clone());
        let recent = self
            .last
            .entry(origin.to_string_lossy().into_owned())
            .or_default();
        recent.retain(|p| p != &project);
        recent.insert(0, project.clone());
        let sorted: BTreeMap<String, Vec<PathBuf>> = self
            .folders
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let _ = super::config::write_projects(&self.dir, &sorted, &self.last);
        let log = AgentLog::new(&self.dir, &agent_id);
        log.prepare();
        watch_terminal(&proc, &log);
        log.event(
            crate::now_ms(),
            "started",
            &format!("{} in {cwd}", argv.join(" ")),
        );
        let spec = AgentSpec {
            agent_id: agent_id.clone(),
            name,
            project,
            adapter,
            model,
            role,
        };
        self.agents.insert(
            agent_id.clone(),
            Agent {
                key: key.clone(),
                log,
                restart_budget: opts.restart as usize,
                launch: Launch {
                    argv,
                    secrets,
                    env,
                    rows: rows.max(5),
                    cols: cols.max(20),
                },
                restarts: VecDeque::new(),
                spec: spec.clone(),
                cwd: PathBuf::from(&cwd),
                origin,
                proc,
                queue: intro(&spec).into_iter().collect(),
                raw: VecDeque::new(),
                inflight: None,
                urgency: Urgency::Queue,
                urgent_id: None,
                steered_at: None,
                status: AgentStatus::Starting,
                held: false,
                asks: 0,
                shown_prompt: None,
                candidate_prompt: None,
                awaiting: None,
                shot_at: 0,
                still: (0, 0),
                deciding: None,
                limit_reported: false,
                faults: 0,
                files: HashMap::new(),
            },
        );
        self.link
            .send(NodeFrame::AgentRegister { agent: spec, cwd })
            .await;
        // A saved handoff is only asked for when the person said so. Otherwise the agent starts clean.
        if opts.pickup {
            self.link
                .send(NodeFrame::AgentPickup {
                    agent_id: agent_id.clone(),
                })
                .await;
        }
        Resp {
            ok: true,
            msg: agent_id,
            data: Some(serde_json::json!({ "key": key, "worktree": own_tree })),
        }
    }
}

/// Starts writing what an agent's terminal shows into its log: tmux copies it out itself, the built-in terminal is listened to.
fn watch_terminal(proc: &Terminal, log: &AgentLog) {
    match proc {
        Terminal::Tmux(t) => t.pipe_to(&log.terminal_path()),
        Terminal::Pty(p) => {
            let mut rx = p.output.subscribe();
            let log = log.clone();
            tokio::spawn(async move {
                while let Ok(bytes) = rx.recv().await {
                    log.terminal(&bytes);
                }
            });
        }
    }
}

/// For an agent's own commands: which agent is speaking, what to call it in the log, and what to write there. None for
/// commands a person runs.
fn describe(req: &Req) -> Option<(&str, &'static str, String)> {
    Some(match req {
        Req::Say { agent, text, .. } => (agent, "say", text.clone()),
        Req::Ask {
            agent, question, ..
        } => (agent, "ask", question.clone()),
        Req::Assign {
            agent, to, task, ..
        } => (agent, "assign", format!("{to}: {task}")),
        Req::Done {
            agent,
            task,
            summary,
        } => (agent, "done", format!("{task}: {summary}")),
        Req::Report {
            agent,
            title,
            summary,
            ..
        } => (agent, "report", format!("{title}: {summary}")),
        Req::Dump { agent, text } => (agent, "dump", text.clone()),
        Req::Pickup { agent } => (agent, "pickup", "asked for a handoff".into()),
        Req::Team { agent } => (agent, "team", "asked who is in the project".into()),
        Req::Answer { agent, ask, text } => (agent, "answer", format!("{ask}: {text}")),
        Req::Usage { agent, kind, pct } => (agent, "usage", format!("{kind} {pct}%")),
        Req::Permission {
            agent,
            kind,
            action,
        } => (agent, "permission", format!("{kind}: {action}")),
        Req::Send { agent, path, .. } => (agent, "send", path.clone()),
        _ => return None,
    })
}

/// What a screen reading means for the status shown to people.
fn status_of(s: &ScreenState) -> AgentStatus {
    if s.limit.is_some() {
        AgentStatus::Limited
    } else if s.prompt.is_some() {
        AgentStatus::WaitingInput
    } else if s.executing {
        AgentStatus::Executing
    } else if s.busy {
        AgentStatus::Thinking
    } else if s.ready {
        AgentStatus::Idle
    } else {
        AgentStatus::Starting
    }
}

/// A short stable number for a piece of text, used to make ids.
fn fingerprint(s: &str) -> u32 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut h);
    h.finish() as u32
}

#[cfg(test)]
mod intro_tests {
    use super::*;

    #[test]
    fn every_agent_gets_the_rules_as_its_first_message_whatever_its_program() {
        for adapter in [AdapterId::Claude, AdapterId::Agy, AdapterId::Codex] {
            let spec = AgentSpec {
                agent_id: "p/otter".into(),
                name: "otter".into(),
                project: "p".into(),
                adapter,
                model: None,
                role: None,
            };
            let d = intro(&spec).expect("every agent gets one");
            assert!(d.text.contains("You are otter in project p") && d.text.contains("claudecord"));
        }
    }

    #[test]
    fn a_screen_is_shortened_cleaned_and_has_its_secrets_removed() {
        let long: String = (0..60).map(|i| format!("line {i}\n")).collect();
        let t = shot(&format!("{long}\n\n\n"));
        assert!(t.ends_with("line 59") && !t.contains("line 10\n"), "{t}");
        assert!(shot("export API_KEY=abcdefghijklmnop1234").contains("[redacted"));
        assert!(shot("").is_empty());
        // A token cut in two by the width of the terminal is not shown in halves.
        let half = shot("see ghp_abcdefghijklmnopqrstuvwxy\nz0123456789ABCD here");
        assert!(
            !half.contains("ghp_") && !half.contains("z0123456789"),
            "{half}"
        );
    }

    #[test]
    fn received_files_are_removed_when_old_and_the_oldest_go_first_when_there_are_too_many() {
        let d = std::env::temp_dir().join(format!("cc-tidy-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let put = |n: &str, kb: usize, age_s: u64| {
            let p = d.join(n);
            std::fs::write(&p, vec![0u8; kb * 1024]).unwrap();
            let t = std::time::SystemTime::now() - std::time::Duration::from_secs(age_s);
            std::fs::File::options()
                .write(true)
                .open(&p)
                .unwrap()
                .set_modified(t)
                .unwrap();
        };
        put("ancient", 1, 10 * 24 * 3600);
        put("old", 100, 3600);
        put("middle", 100, 1800);
        put("new", 100, 60);
        // Older than three days goes; then, with room for 250 KB, the oldest of the rest.
        tidy_files(&d, 3 * 24 * 3_600_000, 250 * 1024);
        let mut left: Vec<String> = std::fs::read_dir(&d)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        assert_eq!(left, vec!["middle", "new"]);
        let _ = std::fs::remove_dir_all(&d);
    }
}
