//! The daemon: the one program that runs on a machine with agents. It keeps the link to the hub, starts each agent in a
//! terminal it owns, pastes messages from the hub into the right terminal at a safe moment, watches each terminal to tell
//! the hub what the agent is doing, and answers the `claudecord` command that agents and people run in a shell.
//!
//! It is one loop. Everything that can happen (a frame from the hub, a request from the command line, a timer) is handled
//! one at a time by `State`, so there is nothing to lock. The only other threads are the ones that read each terminal.

use super::config::Config;
use super::inject::key_bytes;
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
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot};

/// The short standing instruction added to every agent: the verbs it has and the one rule about replies.
pub const RULES: &str = "Team chat is the shell command claudecord: say <text> (FYI), ask <question>, assign <agent> <task>, done <id> <summary>, dump (save state), send <file>. Your say while you work on a task goes to that task's thread by itself; ask, done and report go to the main chat. Plain say is FYI: @name someone to need a reply.";

/// Choices that tests change.
#[derive(Clone)]
pub struct Options {
    pub link: LinkOpts,
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
}

impl Default for Options {
    /// Look at terminals four times a second; assume a message was taken up after three seconds.
    fn default() -> Self {
        Self {
            link: LinkOpts::default(),
            tick: Duration::from_millis(250),
            accept_after: Duration::from_secs(3),
            quiet: (
                super::inject::QUIET_INPUT_MS,
                super::inject::QUIET_OUTPUT_MS,
            ),
            extra_path: Vec::new(),
            backend: Backend::from_env(),
            auto_startup: false,
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
    status: AgentStatus,
    held: bool,
    asks: u32,
    shown_prompt: Option<String>,
    deciding: Option<AwaitingDecision>,
    limit_reported: bool,
    /// Internal errors in a row while looking at this agent (see `agent_faulted`).
    faults: u32,
    files: HashMap<String, PathBuf>,
}

/// A request from the command line, with where to send the answer.
type Call = (Envelope, oneshot::Sender<Resp>);

struct State {
    dir: PathBuf,
    /// Folders this machine has started agents in, by project, so the hub can start more there later. The hub never
    /// chooses a folder: only one the person already used on this machine.
    folders: HashMap<String, PathBuf>,
    link: Link,
    agents: HashMap<String, Agent>,
    /// Recently seen delivery ids, so a message the hub sent twice is pasted once.
    seen: VecDeque<String>,
    up: bool,
    opts: Options,
    quit: bool,
}

/// Runs the daemon until told to shut down. Listens on the socket in `dir`, connects to the hub from `cfg`.
pub async fn run(cfg: Config, dir: PathBuf, opts: Options) -> std::io::Result<()> {
    std::fs::create_dir_all(&dir)?;
    let sock = socket_path(&dir);
    let listener = super::ipc::listen(&dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(0o600))?;
    }
    let (ev_tx, mut events) = mpsc::channel(256);
    let link = link::spawn(
        cfg.connect_url(),
        cfg.token.clone(),
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
    let folders = std::fs::read_to_string(dir.join("projects.json"))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default();
    let mut st = State {
        folders,
        dir,
        link,
        agents: HashMap::new(),
        seen: VecDeque::new(),
        up: false,
        opts: opts.clone(),
        quit: false,
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
                let _ = reply.send(resp);
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
                    a.queue.push(Delivery {
                        from,
                        text,
                        thread,
                        msg_id,
                    });
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
                ..
            } => self.on_file(&agent_id, &transfer_id, &from, &name, seq, last, &data),
            HubFrame::Spawn { agent } => {
                // Only in a folder this machine already knows for the project. Otherwise there is nothing safe to start.
                if let Some(cwd) = self.folders.get(&agent.project).cloned() {
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
                            UpOpts::default(),
                        )
                        .await;
                    if !r.ok {
                        crate::warn!(
                            "daemon",
                            "{}: spawn from the hub failed: {}",
                            agent.agent_id,
                            r.msg
                        );
                    }
                }
            }
            HubFrame::Welcome { .. } | HubFrame::Error { .. } | HubFrame::Ack { .. } => {}
        }
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
    ) {
        let Some(a) = self.agents.get_mut(agent_id) else {
            return;
        };
        let dir = a.cwd.join(crate::hub::routing::INBOX_DIR);
        if std::fs::create_dir_all(&dir).is_err() {
            return;
        }
        let path = a
            .files
            .entry(transfer_id.to_string())
            .or_insert_with(|| dir.join(format!("{}-{}", safe_name(transfer_id), safe_name(name))))
            .clone();
        let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(data) else {
            return;
        };
        use std::io::Write;
        let opened = if seq == 0 {
            std::fs::File::create(&path)
        } else {
            std::fs::OpenOptions::new().append(true).open(&path)
        };
        if opened.and_then(|mut f| f.write_all(&bytes)).is_err() {
            return;
        }
        if last {
            a.files.remove(transfer_id);
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
        let l = a.launch.clone();
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
                a.log
                    .event(now, "restarted", "the agent's program was started again");
                crate::warn!(
                    "daemon",
                    "{id}: the agent's program ended; started it again"
                );
                a.proc = p;
                a.restarts.push_back(now);
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
                self.link
                    .send(NodeFrame::AgentPickup {
                        agent_id: id.to_string(),
                    })
                    .await;
            }
            Err(_) => self.stop(id).await,
        }
    }

    /// Looks at every terminal: reports ended agents, status changes, limits and prompts, pastes waiting messages when it
    /// is safe, and says when a pasted message seems to have been taken up.
    async fn on_tick(&mut self, now: i64) {
        let ids: Vec<String> = self.agents.keys().cloned().collect();
        // Looking at a tmux session runs the tmux program, which waits for the operating system. All of them are looked at at once on
        // threads meant for blocking work, so many agents never hold up the daemon's own connections.
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
                } else if a.shown_prompt.as_deref() != Some(&p.signature) {
                    // Anything else is a question for a person. It becomes a permission request in the chat.
                    a.shown_prompt = Some(p.signature.clone());
                    let perm_id = format!("{}-{:x}", a.spec.name, fingerprint(&p.signature));
                    a.deciding = Some(AwaitingDecision {
                        perm_id: perm_id.to_string(),
                        prompt: p.clone(),
                    });
                    out.push(NodeFrame::AgentPermission {
                        agent_id: id.to_string(),
                        perm_id,
                        kind: "tool".into(),
                        action: p.question.clone(),
                        thread: None,
                    });
                }
            } else if let Some(shown) = a.shown_prompt.take() {
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
                let text = format_deliveries(&a.queue);
                let before = a.proc.screen_hash();
                if a.proc.inject(&text, now).is_ok() {
                    a.log.event(now, "delivered", &text);
                    let ids: Vec<String> = a.queue.drain(..).filter_map(|d| d.msg_id).collect();
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
                self.up_agent(
                    project, name, adapter, model, role, cwd, policy, rows, cols, opts,
                )
                .await
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
                self.verb(&agent, |id| NodeFrame::AgentSay {
                    agent_id: id,
                    text,
                    thread,
                })
                .await
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
        let transfer_id = format!(
            "{agent}-{:x}",
            fingerprint(&format!("{path}{}", crate::now_ms()))
        );
        let total = data.len().div_ceil(FILE_CHUNK_BYTES).max(1);
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
                    to: to.clone(),
                    caption: caption.clone(),
                    thread: None,
                })
                .await;
        }
        Resp::ok(format!("sent {name} ({} KB)", data.len().div_ceil(1024)))
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
        if !worktree {
            return Err("another agent already works in this folder, and two agents must not share one. Start this one in a different folder, or add --worktree to give it its own git worktree".into());
        }
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .arg("-C")
                .arg(origin)
                .args(args)
                .output()
        };
        if !git(&["rev-parse", "--is-inside-work-tree"]).is_ok_and(|o| o.status.success()) {
            return Err("--worktree needs a git repository, and this folder is not one".into());
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
            Some(n) if crate::protocol::is_slug(&n) => n,
            Some(_) => return Resp::err("names use letters, digits, dots, dashes and underscores"),
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
        // Remember the folder, so the hub can start more agents for this project here later.
        let origin = PathBuf::from(&cwd);
        self.folders.insert(project.clone(), origin.clone());
        let _ = std::fs::write(
            self.dir.join("projects.json"),
            serde_json::to_string(&self.folders).expect("plain data"),
        );
        // Two agents in one folder would trample each other's files and see each other's work. That is refused unless the
        // person asked for a separate git worktree for this one.
        let cwd = match self.isolate(&origin, &project, &name, opts.worktree) {
            Ok(c) => c,
            Err(e) => return Resp::err(e),
        };
        let agent_id = format!("{project}/{name}");
        let pol = match policy.as_str() {
            "plan" => Policy::Plan,
            "ask" => Policy::Ask,
            _ => Policy::Autonomous,
        };
        let argv = match opts.command.clone().filter(|c| !c.is_empty()) {
            Some(c) => c,
            None => adapter.argv(&LaunchCtx {
                name: &name,
                model: model.as_deref(),
                policy: pol,
                rules: RULES,
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
            return Resp::err(format!(
                "{} was not found on this machine. Install it, or put its folder on the PATH of the machine running claudecord",
                argv[0]
            ));
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
                queue: Vec::new(),
                raw: VecDeque::new(),
                inflight: None,
                status: AgentStatus::Starting,
                held: false,
                asks: 0,
                shown_prompt: None,
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
            data: Some(serde_json::json!({ "key": key })),
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
