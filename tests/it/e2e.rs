//! Everything together, on one machine: a real hub, a real device daemon, and a stand-in agent (a shell script named
//! `claude` that prints the ready line and echoes each line it is given). A person's message goes in through the hub and
//! must come out in the agent's terminal; the agent's own commands must come out in the hub.
#![cfg(unix)]

use claudecord::device::config::Config;
use claudecord::device::daemon::{self, Options};
use claudecord::device::ipc::{self, Req, UpOpts};
use claudecord::device::link::LinkOpts;
use claudecord::device::terminal::Backend;
use claudecord::hub::*;
use claudecord::protocol::AgentStatus;
use claudecord::server::{self, Config as ServerConfig};
use claudecord::store::Store;
use std::path::{Path, PathBuf};
use std::time::Duration;

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("cc-e2e-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// A stand-in for the `claude` program: prints the ready line, then echoes each line it is given into ./fake.log.
const NORMAL: &str = "#!/bin/sh\necho 'fake claude'\nprintf '? for shortcuts\\n'\nwhile IFS= read -r line; do\n  printf '%s\\n' \"$line\" >> fake.log\n  echo \"ack: $line\"\n  printf '? for shortcuts\\n'\ndone\n";

/// Dies the first time it is started (leaving a marker file), then behaves normally.
const DIES_ONCE: &str = "#!/bin/sh\necho run >> starts.log\nif [ ! -f started ]; then touch started; exit 1; fi\necho 'fake claude'\nprintf '? for shortcuts\\n'\nwhile IFS= read -r line; do\n  printf '%s\\n' \"$line\" >> fake.log\n  echo \"ack: $line\"\n  printf '? for shortcuts\\n'\ndone\n";

/// Floods its terminal with 3 MB of random bytes (invalid text, stray escape codes, a terminal reset) before behaving normally.
const GARBAGE: &str = "#!/bin/sh\nhead -c 3000000 /dev/urandom\nprintf '\\033[2J\\033[999;999H\\033]0;title\\007\\033[?1049h'\necho 'fake claude'\nprintf '? for shortcuts\\n'\nwhile IFS= read -r line; do\n  printf '%s\\n' \"$line\" >> fake.log\n  echo \"ack: $line\"\n  printf '? for shortcuts\\n'\ndone\n";

/// Lives for a second, then dies: long enough to be seen starting, so a test can watch what happens when it ends.
const DIES_SOON: &str = "#!/bin/sh\necho run >> starts.log\nsleep 1\nexit 1\n";

/// Dies every time, at once.
const ALWAYS_DIES: &str = "#!/bin/sh\nexit 1\n";

/// Writes the fake program into `root/bin` and returns that folder, to be put first on the agent's PATH. Each test has
/// its own folder, so tests running side by side do not see each other's fake.
fn fake_claude(root: &Path, script: &str) -> PathBuf {
    let bin = root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let path = bin.join("claude");
    std::fs::write(&path, script).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    bin
}

fn kd() -> Human {
    Human {
        id: "1".into(),
        name: "kd".into(),
    }
}

fn fast(extra: Vec<PathBuf>, max_agents: usize, labels: Vec<String>) -> Options {
    Options {
        dev_spawn: true,
        link: LinkOpts {
            ping_every: Duration::from_millis(200),
            connect_timeout: Duration::from_millis(500),
            backoff_min: Duration::from_millis(30),
            backoff_max: Duration::from_millis(300),
            proxy: None,
        },
        tick: Duration::from_millis(40),
        accept_after: Duration::from_millis(400),
        quiet: (100, 200),
        extra_path: extra,
        max_agents,
        labels,
        backend: Backend::Pty,
        auto_startup: false,
        idle_exit: None,
        idle_agents_exit: None,
    }
}

async fn eventually(what: &str, mut f: impl AsyncFnMut() -> bool) {
    // Generous: shared CI machines are slow, and a wait only lasts as long as the thing it waits for.
    for _ in 0..800 {
        if f().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    panic!("timed out waiting for {what}");
}

struct Rig {
    hub: server::Hub,
    dir: PathBuf,
    project: PathBuf,
}

async fn rig(name: &str) -> Rig {
    rig_with(name, NORMAL).await
}

async fn rig_with(name: &str, script: &str) -> Rig {
    rig_full(name, script, 8, vec![]).await
}

async fn rig_full(name: &str, script: &str, max_agents: usize, labels: Vec<String>) -> Rig {
    rig_backend(name, script, max_agents, labels, Backend::Pty).await
}

async fn rig_backend(
    name: &str,
    script: &str,
    max_agents: usize,
    labels: Vec<String>,
    backend: Backend,
) -> Rig {
    rig_tuned(name, script, max_agents, labels, backend, |_| {}).await
}

/// A rig whose daemon's options a test may change before it runs (the dev switch, the idle exit).
async fn rig_tuned(
    name: &str,
    script: &str,
    max_agents: usize,
    labels: Vec<String>,
    backend: Backend,
    tweak: impl FnOnce(&mut Options) + Send + 'static,
) -> Rig {
    let root = tmp(name);
    let bin = fake_claude(&root, script);
    let mut store = Store::open(&root.join("hub.db"), None).unwrap();
    let token = store.create_token("mac", 0).unwrap();
    let mut core = HubCore::default();
    core.add_owner("1");
    // The daemon's options are made first: the hub's heartbeat follows the daemon's, so a test that changes one changes both.
    let mut opts = fast(vec![bin], max_agents, labels);
    opts.backend = backend;
    tweak(&mut opts);
    let hub = server::start(
        ServerConfig {
            bind: "127.0.0.1:0".parse().unwrap(),
            ping_every: opts.link.ping_every,
            tick_every: Duration::from_millis(50),
            ..ServerConfig::default()
        },
        core,
        store,
    )
    .await
    .unwrap();
    let dir = root.join("home");
    let cfg = Config {
        hub_url: format!("ws://{}", hub.addr),
        token,
        node_name: "mac".into(),
    };
    let d = dir.clone();
    tokio::spawn(async move { daemon::run(cfg, d, opts).await.unwrap() });
    eventually("daemon socket", async || {
        ipc::call(&dir, &Req::Ping).await.is_ok()
    })
    .await;
    let project = root.join("demo");
    std::fs::create_dir_all(&project).unwrap();
    Rig { hub, dir, project }
}

/// Starts an agent with the given options and returns the answer.
async fn up_with(r: &Rig, name: &str, opts: UpOpts) -> claudecord::device::ipc::Resp {
    ipc::call(
        &r.dir,
        &Req::Up {
            project: "demo".into(),
            name: Some(name.into()),
            adapter: "claude".into(),
            model: None,
            role: None,
            cwd: r.project.to_string_lossy().into(),
            policy: "autonomous".into(),
            rows: 24,
            cols: 80,
            opts,
        },
    )
    .await
    .unwrap()
}

/// Starts an agent the plain way and returns its secret key.
async fn up(r: &Rig, name: &str) -> String {
    let resp = up_with(r, name, UpOpts::default()).await;
    assert!(resp.ok, "{}", resp.msg);
    resp.data.unwrap()["key"].as_str().unwrap().to_string()
}

#[tokio::test]
async fn a_message_waiting_on_a_busy_agent_goes_straight_through_when_the_person_asks() {
    // The agent never goes quiet for 60 seconds, as if it were in the middle of a long run.
    let r = rig_tuned("hurry", NORMAL, 8, vec![], Backend::Pty, |o| {
        o.quiet = (0, 60_000)
    })
    .await;
    up(&r, "otter").await;
    eventually("registered", async || {
        r.hub
            .call(|c, _| (c.agent("demo/otter").is_some(), vec![]))
            .await
            .unwrap()
    })
    .await;
    r.hub
        .call(|c, now| {
            let r = c
                .human_message(
                    &kd(),
                    "demo",
                    "look at this now",
                    &MessageOpts {
                        reference: Some("c:7"),
                        ..Default::default()
                    },
                    now,
                )
                .unwrap();
            ((), r.1)
        })
        .await;
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert!(
        std::fs::read_to_string(r.project.join("fake.log")).is_err(),
        "queued: the agent is busy, so nothing was pasted"
    );
    r.hub
        .call(|c, _| {
            let mut fx = Vec::new();
            let sent = c.prioritise(&kd(), "demo", "c:7", "now", &mut fx);
            (sent, fx)
        })
        .await;
    eventually("the message goes straight through", async || {
        std::fs::read_to_string(r.project.join("fake.log"))
            .is_ok_and(|s| s.contains("look at this now"))
    })
    .await;
}

#[tokio::test]
async fn a_persons_message_reaches_the_agents_terminal_and_the_agent_speaks_back() {
    let r = rig("flow").await;
    let mut chat = r.hub.chat();
    let key = up(&r, "otter").await;
    eventually("registered", async || {
        r.hub
            .call(|c, _| (c.agent("demo/otter").is_some(), vec![]))
            .await
            .unwrap()
    })
    .await;
    eventually("agent idle", async || {
        r.hub
            .call(|c, _| (c.status_of("demo/otter") == AgentStatus::Idle, vec![]))
            .await
            .unwrap()
    })
    .await;

    r.hub
        .call(|c, now| {
            let r = c
                .human_message(
                    &kd(),
                    "demo",
                    "please add retries",
                    &MessageOpts {
                        reference: Some("c:1"),
                        ..Default::default()
                    },
                    now,
                )
                .unwrap();
            ((), r.1)
        })
        .await;
    // It arrives in the agent's terminal, as a pasted line.
    eventually("message in the terminal", async || {
        std::fs::read_to_string(r.project.join("fake.log"))
            .is_ok_and(|s| s.contains("please add retries"))
    })
    .await;
    // The hub is told the agent took it up, and the person's message gets its confirmation.
    let confirmed = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(Chat::Confirm { reference, .. }) = chat.recv().await
                && reference == "c:1"
            {
                return true;
            }
        }
    })
    .await;
    assert_eq!(confirmed, Ok(true));

    // The agent runs a command in its shell, and it shows up in the chat.
    let resp = ipc::call_as(
        &r.dir,
        Some(&key),
        &Req::Say {
            agent: "demo/otter".into(),
            text: "added retries".into(),
            thread: None,
        },
    )
    .await
    .unwrap();
    assert!(resp.ok);
    let spoke = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(Chat::Post { text, .. }) = chat.recv().await
                && text == "added retries"
            {
                return true;
            }
        }
    })
    .await;
    assert_eq!(spoke, Ok(true));

    // An agent that does not exist here is refused.
    assert!(
        !ipc::call(
            &r.dir,
            &Req::Say {
                agent: "demo/ghost".into(),
                text: "x".into(),
                thread: None
            }
        )
        .await
        .unwrap()
        .ok
    );
    r.hub.shutdown().await;
}

#[tokio::test]
async fn an_ask_returns_at_once_and_the_answer_comes_back_as_input() {
    let r = rig("ask").await;
    let key = up(&r, "otter").await;
    eventually("registered", async || {
        r.hub
            .call(|c, _| (c.agent("demo/otter").is_some(), vec![]))
            .await
            .unwrap()
    })
    .await;
    let resp = ipc::call_as(
        &r.dir,
        Some(&key),
        &Req::Ask {
            agent: "demo/otter".into(),
            question: "which db?".into(),
            options: None,
            thread: None,
        },
    )
    .await
    .unwrap();
    assert!(
        resp.ok && resp.msg.contains("End your turn"),
        "{}",
        resp.msg
    );
    eventually("ask open", async || {
        r.hub
            .call(|c, _| (c.asks_of("demo").len() == 1, vec![]))
            .await
            .unwrap()
    })
    .await;
    r.hub
        .call(|c, now| {
            let o = c
                .answer_ask(&Answerer::Human(kd()), "demo", "Q1", "postgres", now)
                .unwrap();
            ((), o.effects)
        })
        .await;
    eventually("answer in the terminal", async || {
        std::fs::read_to_string(r.project.join("fake.log")).is_ok_and(|s| s.contains("postgres"))
    })
    .await;
    r.hub.shutdown().await;
}

#[tokio::test]
async fn stopping_an_agent_from_the_hub_ends_it_and_removes_it() {
    let r = rig("stop").await;
    up(&r, "otter").await;
    eventually("registered", async || {
        r.hub
            .call(|c, _| (c.agent("demo/otter").is_some(), vec![]))
            .await
            .unwrap()
    })
    .await;
    r.hub
        .call(|c, now| {
            let (_, fx) = c.stop(&kd(), "demo", "otter", now).unwrap();
            ((), fx)
        })
        .await;
    eventually("gone from the hub", async || {
        r.hub
            .call(|c, _| (c.agent("demo/otter").is_none(), vec![]))
            .await
            .unwrap()
    })
    .await;
    let list = ipc::call(&r.dir, &Req::List).await.unwrap();
    assert_eq!(list.data.unwrap().as_array().unwrap().len(), 0);
    r.hub.shutdown().await;
}

#[tokio::test]
async fn a_file_sent_from_chat_lands_in_the_agents_inbox_and_the_agent_is_told_where() {
    let r = rig("file").await;
    up(&r, "otter").await;
    eventually("registered", async || {
        r.hub
            .call(|c, _| (c.agent("demo/otter").is_some(), vec![]))
            .await
            .unwrap()
    })
    .await;
    r.hub
        .call(|c, _| {
            let (_, fx) = c
                .send_file(
                    &kd(),
                    "demo",
                    "see this",
                    "plan.txt",
                    b"the plan",
                    None,
                    "t1",
                )
                .unwrap();
            ((), fx)
        })
        .await;
    eventually("file saved", async || {
        std::fs::read(r.project.join(".claudecord/files/demo/plan.txt"))
            .is_ok_and(|b| b == b"the plan")
    })
    .await;
    eventually("agent told", async || {
        std::fs::read_to_string(r.project.join("fake.log"))
            .is_ok_and(|s| s.contains(".claudecord/files/demo/plan.txt"))
    })
    .await;
    r.hub.shutdown().await;
}

#[tokio::test]
async fn an_agent_that_dies_is_started_again_and_keeps_working() {
    let r = rig_with("revive", DIES_ONCE).await;
    let resp = up_with(
        &r,
        "otter",
        UpOpts {
            restart: 3,
            ..Default::default()
        },
    )
    .await;
    assert!(resp.ok);
    eventually("registered", async || {
        r.hub
            .call(|c, _| (c.agent("demo/otter").is_some(), vec![]))
            .await
            .unwrap()
    })
    .await;
    // The first start dies at once. The daemon starts it again, which shows as a second line in the start log.
    eventually("started a second time", async || {
        std::fs::read_to_string(r.project.join("starts.log")).is_ok_and(|s| s.lines().count() >= 2)
    })
    .await;
    r.hub
        .call(|c, now| {
            let r = c
                .human_message(
                    &kd(),
                    "demo",
                    "after the restart",
                    &MessageOpts::default(),
                    now,
                )
                .unwrap();
            ((), r.1)
        })
        .await;
    eventually("message reaches the restarted agent", async || {
        std::fs::read_to_string(r.project.join("fake.log"))
            .is_ok_and(|s| s.contains("after the restart"))
    })
    .await;
    r.hub.shutdown().await;
}

#[tokio::test]
async fn an_agent_that_keeps_dying_is_given_up_on_instead_of_restarted_forever() {
    let r = rig_with("giveup", ALWAYS_DIES).await;
    let resp = up_with(
        &r,
        "otter",
        UpOpts {
            restart: 3,
            ..Default::default()
        },
    )
    .await;
    assert!(resp.ok);
    eventually("registered", async || {
        r.hub
            .call(|c, _| (c.agent("demo/otter").is_some(), vec![]))
            .await
            .unwrap()
    })
    .await;
    eventually("given up and removed from the hub", async || {
        r.hub
            .call(|c, _| (c.agent("demo/otter").is_none(), vec![]))
            .await
            .unwrap()
    })
    .await;
    let list = ipc::call(&r.dir, &Req::List).await.unwrap();
    assert_eq!(list.data.unwrap().as_array().unwrap().len(), 0);
    r.hub.shutdown().await;
}

#[tokio::test]
async fn a_machine_at_its_limit_refuses_more_agents_and_tells_the_hub_what_it_can_take() {
    let r = rig_full("cap", NORMAL, 1, vec!["gpu".into(), "h100".into()]).await;
    up(&r, "otter").await;
    let resp = ipc::call(
        &r.dir,
        &Req::Up {
            project: "demo".into(),
            name: Some("heron".into()),
            adapter: "claude".into(),
            model: None,
            role: None,
            cwd: r.project.to_string_lossy().into(),
            policy: "autonomous".into(),
            rows: 24,
            cols: 80,
            opts: UpOpts::default(),
        },
    )
    .await
    .unwrap();
    assert!(!resp.ok && resp.msg.contains("limit"), "{}", resp.msg);
    eventually("capacity reported", async || {
        r.hub
            .devices()
            .await
            .first()
            .is_some_and(|d| d.max_agents == Some(1) && d.labels == vec!["gpu", "h100"])
    })
    .await;
    r.hub.shutdown().await;
}

#[tokio::test]
async fn a_second_agent_in_the_same_git_folder_gets_its_own_worktree() {
    let r = rig("worktree").await;
    let git = |args: &[&str]| {
        std::process::Command::new("git")
            .args(["-c", "user.name=t", "-c", "user.email=t@t"])
            .arg("-C")
            .arg(&r.project)
            .args(args)
            .output()
            .unwrap()
    };
    git(&["init", "-q"]);
    std::fs::write(r.project.join("a.txt"), "x").unwrap();
    git(&["add", "."]);
    git(&["commit", "-q", "-m", "init"]);
    up(&r, "otter").await;
    // No flag needed: another agent already works here, so the second one is given its own worktree, and the answer says so.
    let resp = up_with(&r, "heron", UpOpts::default()).await;
    assert!(resp.ok, "{}", resp.msg);
    assert!(
        resp.data.as_ref().unwrap()["worktree"]
            .as_str()
            .is_some_and(|t| t.contains("demo-heron")),
        "{:?}",
        resp.data
    );
    // Agents a person started are kept to offer again.
    let saved = std::fs::read_to_string(r.dir.join("agents.json")).unwrap_or_default();
    assert!(
        saved.contains("otter") && saved.contains("heron"),
        "{saved}"
    );
    let tree = r.project.join(".claudecord/worktrees/demo-heron");
    assert!(
        tree.join("a.txt").exists(),
        "the second agent works in its own checkout"
    );
    eventually("both registered", async || {
        r.hub
            .call(|c, _| (c.agents_of_project("demo").len() == 2, vec![]))
            .await
            .unwrap()
    })
    .await;
    r.hub
        .call(|c, now| {
            let r = c
                .human_message(&kd(), "demo", "@heron hello", &MessageOpts::default(), now)
                .unwrap();
            ((), r.1)
        })
        .await;
    eventually(
        "heron's message lands in its own folder, not the shared one",
        async || std::fs::read_to_string(tree.join("fake.log")).is_ok_and(|s| s.contains("hello")),
    )
    .await;
    assert!(
        std::fs::read_to_string(r.project.join("fake.log")).is_ok_and(|s| !s.contains("hello")),
        "the first agent's folder did not get the second agent's message"
    );
    r.hub.shutdown().await;
}

#[tokio::test]
async fn the_hub_can_start_an_agent_in_a_folder_the_machine_already_knows() {
    let r = rig("spawn").await;
    up(&r, "otter").await;
    eventually("registered", async || {
        r.hub
            .call(|c, _| (c.agent("demo/otter").is_some(), vec![]))
            .await
            .unwrap()
    })
    .await;
    let spec = |project: &str, name: &str| claudecord::protocol::AgentSpec {
        agent_id: format!("{project}/{name}"),
        name: name.into(),
        project: project.into(),
        adapter: claudecord::protocol::AdapterId::Claude,
        model: None,
        role: None,
    };
    let node = r
        .hub
        .call(move |c, _| {
            let (node, fx) = c
                .spawn_auto(&kd(), "demo", spec("demo", "wren"), None)
                .unwrap();
            (node, fx)
        })
        .await
        .unwrap();
    assert_eq!(node.as_deref(), Some("mac"), "the only machine was chosen");
    // otter still works in that folder, so the daemon refuses to put a second agent there.
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        r.hub
            .call(|c, _| (c.agent("demo/wren").is_none(), vec![]))
            .await
            .unwrap()
    );
    // Once the folder is free, the same request works.
    assert!(
        ipc::call(
            &r.dir,
            &Req::Stop {
                agent: "demo/otter".into()
            }
        )
        .await
        .unwrap()
        .ok
    );
    eventually("otter is gone", async || {
        r.hub
            .call(|c, _| (c.agent("demo/otter").is_none(), vec![]))
            .await
            .unwrap()
    })
    .await;
    r.hub
        .call(move |c, _| {
            let (_, fx) = c
                .spawn_auto(&kd(), "demo", spec("demo", "wren"), None)
                .unwrap();
            ((), fx)
        })
        .await;
    eventually("the new agent registered", async || {
        r.hub
            .call(|c, _| (c.agent("demo/wren").is_some(), vec![]))
            .await
            .unwrap()
    })
    .await;
    // A project this machine has never been given a folder for is not started: the hub never chooses a folder.
    r.hub
        .call(move |c, _| {
            let (_, fx) = c
                .spawn_auto(&kd(), "demo", spec("elsewhere", "x"), None)
                .unwrap();
            ((), fx)
        })
        .await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(
        r.hub
            .call(|c, _| (c.agent("elsewhere/x").is_none(), vec![]))
            .await
            .unwrap()
    );
    r.hub.shutdown().await;
}

#[tokio::test]
async fn a_raw_command_reaches_the_terminal_exactly_as_typed_without_a_header() {
    let r = rig("raw").await;
    up(&r, "otter").await;
    eventually("registered", async || {
        r.hub
            .call(|c, _| (c.agent("demo/otter").is_some(), vec![]))
            .await
            .unwrap()
    })
    .await;
    r.hub
        .call(|c, now| {
            let (_, fx) = c
                .raw_input(&kd(), "demo", "otter", "/compact keep the plan", now)
                .unwrap();
            ((), fx)
        })
        .await;
    eventually("raw text in the terminal", async || {
        std::fs::read_to_string(r.project.join("fake.log"))
            .is_ok_and(|s| s.contains("/compact keep the plan"))
    })
    .await;
    let log = std::fs::read_to_string(r.project.join("fake.log")).unwrap();
    assert!(!log.contains("[kd"), "no sender header was added: {log}");
    r.hub.shutdown().await;
}

// Nothing happens by itself, and agents cannot act as each other.

/// Shows a start-up trust dialog and waits for Enter before going on.
/// The same question as the real Claude Code shows it: options without numbers, blank lines between the parts and before the footer.
const TRUST_DIALOG_PLAIN: &str = "#!/bin/sh\nprintf '\\n Accessing workspace:\\n /work/eeg\\n Quick safety check: Is this a project you created or one you trust?\\n\\n Security guide\\n\\n ❯ No, exit\\n   Yes, I trust this folder\\n\\n Enter to confirm · Esc to cancel\\n'\nIFS= read -r line\necho \"got:[$line]\" >> trust.log\nsleep 30\n";

const TRUST_DIALOG: &str = "#!/bin/sh\necho 'Do you trust the files in this folder?'\necho '> 1. Yes, proceed'\necho '  2. No, exit'\nIFS= read -r line\necho \"got:[$line]\" >> trust.log\necho 'fake claude'\nprintf '? for shortcuts\\n'\nsleep 30\n";

async fn second_agent_elsewhere(r: &Rig, name: &str) -> (String, std::path::PathBuf) {
    let other = r.project.parent().unwrap().join(format!("other-{name}"));
    std::fs::create_dir_all(&other).unwrap();
    let resp = ipc::call(
        &r.dir,
        &Req::Up {
            project: "demo".into(),
            name: Some(name.into()),
            adapter: "claude".into(),
            model: None,
            role: None,
            cwd: other.to_string_lossy().into(),
            policy: "autonomous".into(),
            rows: 24,
            cols: 80,
            opts: UpOpts::default(),
        },
    )
    .await
    .unwrap();
    assert!(resp.ok, "{}", resp.msg);
    (
        resp.data.unwrap()["key"].as_str().unwrap().to_string(),
        other,
    )
}

#[tokio::test]
async fn an_agents_commands_need_its_own_key_so_no_agent_can_speak_for_another() {
    let r = rig("keys").await;
    let key_otter = up(&r, "otter").await;
    let (key_heron, _) = second_agent_elsewhere(&r, "heron").await;
    eventually("both registered", async || {
        r.hub
            .call(|c, _| (c.agents_of_project("demo").len() == 2, vec![]))
            .await
            .unwrap()
    })
    .await;
    let say = |agent: &str, text: &str| Req::Say {
        agent: agent.into(),
        text: text.into(),
        thread: None,
    };
    // Nothing, a wrong key, and another agent's key are all refused.
    for key in [None, Some("nonsense"), Some(key_heron.as_str())] {
        let resp = ipc::call_as(&r.dir, key, &say("demo/otter", "I am otter"))
            .await
            .unwrap();
        assert!(
            !resp.ok && resp.msg.contains("by the agent itself"),
            "{key:?}: {}",
            resp.msg
        );
    }
    // The right key works.
    assert!(
        ipc::call_as(&r.dir, Some(&key_otter), &say("demo/otter", "really otter"))
            .await
            .unwrap()
            .ok
    );
    // Other agents' keys never appear in what a person can list.
    let list = ipc::call(&r.dir, &Req::List).await.unwrap();
    assert!(!list.data.unwrap().to_string().contains(&key_otter));
    r.hub.shutdown().await;
}

#[tokio::test]
async fn nothing_is_handed_over_unless_asked_for() {
    let r = rig("pickup").await;
    // A saved handoff exists for the agent from an earlier session.
    r.hub
        .call(|c, now| {
            let mut fx = c.on_node_frame(
                "mac",
                claudecord::protocol::NodeFrame::AgentRegister {
                    agent: claudecord::protocol::AgentSpec {
                        agent_id: "demo/otter".into(),
                        name: "otter".into(),
                        project: "demo".into(),
                        adapter: claudecord::protocol::AdapterId::Claude,
                        model: None,
                        role: None,
                    },
                    cwd: "/x".into(),
                },
                now,
            );
            fx.extend(c.on_node_frame(
                "mac",
                claudecord::protocol::NodeFrame::AgentHandoff {
                    agent_id: "demo/otter".into(),
                    text: "goal: finish the parser".into(),
                },
                now,
            ));
            ((), fx)
        })
        .await;
    up(&r, "otter").await;
    tokio::time::sleep(Duration::from_millis(900)).await;
    let log = std::fs::read_to_string(r.project.join("fake.log")).unwrap_or_default();
    assert!(
        !log.contains("finish the parser"),
        "a plain start does not pick anything up: {log}"
    );
    r.hub.shutdown().await;
    // Asked for, it is handed over.
    let r = rig("pickup2").await;
    r.hub
        .call(|c, now| {
            let mut fx = c.on_node_frame(
                "mac",
                claudecord::protocol::NodeFrame::AgentRegister {
                    agent: claudecord::protocol::AgentSpec {
                        agent_id: "demo/otter".into(),
                        name: "otter".into(),
                        project: "demo".into(),
                        adapter: claudecord::protocol::AdapterId::Claude,
                        model: None,
                        role: None,
                    },
                    cwd: "/x".into(),
                },
                now,
            );
            fx.extend(c.on_node_frame(
                "mac",
                claudecord::protocol::NodeFrame::AgentHandoff {
                    agent_id: "demo/otter".into(),
                    text: "goal: finish the parser".into(),
                },
                now,
            ));
            ((), fx)
        })
        .await;
    let resp = up_with(
        &r,
        "otter",
        UpOpts {
            pickup: true,
            ..Default::default()
        },
    )
    .await;
    assert!(resp.ok);
    eventually("the handoff reaches the new session", async || {
        std::fs::read_to_string(r.project.join("fake.log"))
            .is_ok_and(|s| s.contains("finish the parser"))
    })
    .await;
    r.hub.shutdown().await;
}

#[tokio::test]
async fn a_crashed_agent_is_not_started_again_unless_asked() {
    let r = rig_with("norestart", DIES_SOON).await;
    let resp = up_with(&r, "otter", UpOpts::default()).await;
    assert!(resp.ok);
    eventually("registered", async || {
        r.hub
            .call(|c, _| (c.agent("demo/otter").is_some(), vec![]))
            .await
            .unwrap()
    })
    .await;
    eventually("removed once it ended", async || {
        r.hub
            .call(|c, _| (c.agent("demo/otter").is_none(), vec![]))
            .await
            .unwrap()
    })
    .await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let starts = std::fs::read_to_string(r.project.join("starts.log")).unwrap();
    assert_eq!(
        starts.lines().count(),
        1,
        "it was started exactly once: {starts}"
    );
    r.hub.shutdown().await;
}

#[tokio::test]
async fn a_start_up_dialog_goes_to_a_person_by_default_and_is_only_answered_for_them_when_told_to()
{
    // By default: a permission request reaches the hub, and nothing is typed into the dialog.
    let r = rig_with("trust", TRUST_DIALOG).await;
    let mut chat = r.hub.chat();
    up(&r, "otter").await;
    let asked = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(Chat::Permission { perm, .. }) = chat.recv().await {
                return perm.action.contains("trust");
            }
        }
    })
    .await;
    assert_eq!(asked, Ok(true), "the dialog was passed to a person");
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        !r.project.join("trust.log").exists(),
        "nothing was typed into the dialog"
    );
    r.hub.shutdown().await;
}

#[tokio::test]
async fn the_log_records_what_happened_with_secrets_removed_and_can_be_read_for_a_handoff() {
    let r = rig("log").await;
    let key = up(&r, "otter").await;
    eventually("registered", async || {
        r.hub
            .call(|c, _| (c.agent("demo/otter").is_some(), vec![]))
            .await
            .unwrap()
    })
    .await;
    let leak = format!("my token is ghp_{}", "a".repeat(36));
    r.hub
        .call(move |c, now| {
            let fx = c
                .human_message(&kd(), "demo", &leak, &MessageOpts::default(), now)
                .unwrap()
                .1;
            ((), fx)
        })
        .await;
    ipc::call_as(
        &r.dir,
        Some(&key),
        &Req::Say {
            agent: "demo/otter".into(),
            text: "working on it".into(),
            thread: None,
        },
    )
    .await
    .unwrap();
    eventually("the message was delivered", async || {
        std::fs::read_to_string(r.project.join("fake.log")).is_ok_and(|s| s.contains("my token is"))
    })
    .await;
    let events = ipc::call(
        &r.dir,
        &Req::Logs {
            agent: "demo/otter".into(),
            lines: 50,
            terminal: false,
        },
    )
    .await
    .unwrap();
    let text = events.data.unwrap()["lines"].to_string();
    for want in ["started", "delivered", "say", "working on it"] {
        assert!(text.contains(want), "the event log lacks {want}: {text}");
    }
    assert!(
        !text.contains("ghp_"),
        "secrets are removed from the log: {text}"
    );
    // The terminal log shows what the terminal showed, as plain text.
    eventually("the terminal log has the agent's reply", async || {
        let t = ipc::call(
            &r.dir,
            &Req::Logs {
                agent: "demo/otter".into(),
                lines: 50,
                terminal: true,
            },
        )
        .await
        .unwrap();
        t.data.unwrap()["lines"].to_string().contains("ack:")
    })
    .await;
    r.hub.shutdown().await;
}

#[tokio::test]
async fn an_agent_the_hub_asks_for_runs_inside_tmux_where_a_person_can_attach() {
    if !claudecord::device::tmux::TmuxTerminal::available() {
        return;
    }
    let socket = format!("cc-spawn-{}", std::process::id());
    let r = rig_backend(
        "tmuxspawn",
        NORMAL,
        8,
        vec![],
        Backend::Tmux(socket.clone()),
    )
    .await;
    // The folder becomes known when a person starts the first agent there; then it is free again.
    up(&r, "otter").await;
    eventually("registered", async || {
        r.hub
            .call(|c, _| (c.agent("demo/otter").is_some(), vec![]))
            .await
            .unwrap()
    })
    .await;
    assert!(
        ipc::call(
            &r.dir,
            &Req::Stop {
                agent: "demo/otter".into()
            }
        )
        .await
        .unwrap()
        .ok
    );
    eventually("otter is gone", async || {
        r.hub
            .call(|c, _| (c.agent("demo/otter").is_none(), vec![]))
            .await
            .unwrap()
    })
    .await;
    // This is what /spawn does: the hub asks the machine, and the machine starts the agent.
    let spec = claudecord::protocol::AgentSpec {
        agent_id: "demo/wren".into(),
        name: "wren".into(),
        project: "demo".into(),
        adapter: claudecord::protocol::AdapterId::Claude,
        model: None,
        role: None,
    };
    r.hub
        .call(move |c, _| {
            let (_, fx) = c.spawn_auto(&kd(), "demo", spec, None).unwrap();
            ((), fx)
        })
        .await;
    eventually("the spawned agent registered", async || {
        r.hub
            .call(|c, _| (c.agent("demo/wren").is_some(), vec![]))
            .await
            .unwrap()
    })
    .await;
    let sessions = std::process::Command::new("tmux")
        .args(["-L", &socket, "list-sessions", "-F", "#{session_name}"])
        .output()
        .unwrap();
    let names = String::from_utf8_lossy(&sessions.stdout).to_string();
    assert!(
        names.contains("wren"),
        "the agent has its own tmux session: {names:?}"
    );
    claudecord::device::tmux::stop_server(&socket);
    r.hub.shutdown().await;
}

// Races at the machine's door: many callers at once.

#[tokio::test]
async fn starting_the_same_agent_twice_at_once_gives_one_agent_and_one_refusal() {
    let r = rig("dupe").await;
    let start = |r: &Rig| {
        let dir = r.dir.clone();
        let cwd = r.project.to_string_lossy().to_string();
        tokio::spawn(async move {
            ipc::call(
                &dir,
                &Req::Up {
                    project: "demo".into(),
                    name: Some("otter".into()),
                    adapter: "claude".into(),
                    model: None,
                    role: None,
                    cwd,
                    policy: "autonomous".into(),
                    rows: 24,
                    cols: 80,
                    opts: UpOpts::default(),
                },
            )
            .await
            .unwrap()
        })
    };
    let all: Vec<_> = (0..6).map(|_| start(&r)).collect();
    let mut ok = 0;
    for t in all {
        ok += usize::from(t.await.unwrap().ok);
    }
    assert_eq!(ok, 1, "exactly one start wins; the rest are told no");
    eventually("registered", async || {
        r.hub
            .call(|c, _| (c.agent("demo/otter").is_some(), vec![]))
            .await
            .unwrap()
    })
    .await;
    let list = ipc::call(&r.dir, &Req::List).await.unwrap();
    assert_eq!(
        list.data.unwrap().as_array().map_or(0, Vec::len),
        1,
        "one agent here"
    );
    r.hub.shutdown().await;
}

#[tokio::test]
async fn many_says_at_once_all_arrive_and_wrong_keys_in_the_crowd_are_all_refused() {
    let r = rig("crowd").await;
    let key = up(&r, "otter").await;
    eventually("registered", async || {
        r.hub
            .call(|c, _| (c.agent("demo/otter").is_some(), vec![]))
            .await
            .unwrap()
    })
    .await;
    let mut tasks = Vec::new();
    for i in 0..60 {
        let dir = r.dir.clone();
        let key = if i % 3 == 0 {
            "wrong-key".to_string()
        } else {
            key.clone()
        };
        tasks.push(tokio::spawn(async move {
            let resp = ipc::call_as(
                &dir,
                Some(&key),
                &Req::Say {
                    agent: "demo/otter".into(),
                    text: format!("note {i}"),
                    thread: None,
                },
            )
            .await
            .unwrap();
            (i % 3 == 0, resp.ok)
        }));
    }
    let (mut good, mut refused) = (0, 0);
    for t in tasks {
        match t.await.unwrap() {
            (false, true) => good += 1,
            (true, false) => refused += 1,
            other => panic!("a wrong key got in, or a right one was refused: {other:?}"),
        }
    }
    assert_eq!((good, refused), (40, 20));
    r.hub.shutdown().await;
}

#[tokio::test]
async fn an_agent_that_floods_its_terminal_with_garbage_does_not_hurt_the_daemon_or_its_neighbours()
{
    let r = rig_with("garbage", GARBAGE).await;
    let key = up(&r, "otter").await;
    eventually("registered", async || {
        r.hub
            .call(|c, _| (c.agent("demo/otter").is_some(), vec![]))
            .await
            .unwrap()
    })
    .await;
    // The daemon is alive, answers commands, and the noisy agent can still be spoken to and can speak.
    assert!(ipc::call(&r.dir, &Req::Ping).await.unwrap().ok);
    r.hub
        .call(|c, now| {
            let fx = c
                .human_message(
                    &kd(),
                    "demo",
                    "hello through the noise",
                    &MessageOpts::default(),
                    now,
                )
                .unwrap()
                .1;
            ((), fx)
        })
        .await;
    eventually("the message got through the garbage", async || {
        std::fs::read_to_string(r.project.join("fake.log"))
            .is_ok_and(|s| s.contains("hello through the noise"))
    })
    .await;
    let said = ipc::call_as(
        &r.dir,
        Some(&key),
        &Req::Say {
            agent: "demo/otter".into(),
            text: "still talking".into(),
            thread: None,
        },
    )
    .await
    .unwrap();
    assert!(said.ok, "{}", said.msg);
    // A second agent, started afterwards, works as normal: one agent's noise is not another's problem.
    assert!(ipc::call(&r.dir, &Req::Ping).await.unwrap().ok);
    r.hub.shutdown().await;
}

#[tokio::test]
async fn an_agent_whose_program_is_missing_or_whose_folder_is_gone_is_refused_at_once_with_the_reason()
 {
    let r = rig("preflight").await;
    let missing = up_with(
        &r,
        "otter",
        UpOpts {
            command: Some(vec!["definitely-not-installed-xyz".into()]),
            ..Default::default()
        },
    )
    .await;
    assert!(
        !missing.ok && missing.msg.contains("was not found"),
        "{}",
        missing.msg
    );
    let no_folder = ipc::call(
        &r.dir,
        &Req::Up {
            project: "demo".into(),
            name: Some("heron".into()),
            adapter: "claude".into(),
            model: None,
            role: None,
            cwd: r.project.join("no-such-folder").to_string_lossy().into(),
            policy: "autonomous".into(),
            rows: 24,
            cols: 80,
            opts: UpOpts::default(),
        },
    )
    .await
    .unwrap();
    assert!(
        !no_folder.ok && no_folder.msg.contains("is not a folder"),
        "{}",
        no_folder.msg
    );
    // Neither left anything behind: a good start still works afterwards.
    let good = up_with(&r, "wren", UpOpts::default()).await;
    assert!(good.ok, "{}", good.msg);
    r.hub.shutdown().await;
}

#[tokio::test]
async fn a_misspelt_policy_is_refused_not_turned_into_autonomous() {
    let r = rig("badpolicy").await;
    let bad = ipc::call(
        &r.dir,
        &Req::Up {
            project: "demo".into(),
            name: Some("heron".into()),
            adapter: "claude".into(),
            model: None,
            role: None,
            cwd: r.project.to_string_lossy().into(),
            policy: "plam".into(),
            rows: 24,
            cols: 80,
            opts: UpOpts {
                command: Some(vec!["cat".into()]),
                ..Default::default()
            },
        },
    )
    .await
    .unwrap();
    assert!(!bad.ok && bad.msg.contains("unknown policy"), "{}", bad.msg);
    r.hub.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_start_up_question_without_numbers_reaches_a_person_too_and_nothing_is_typed_for_them() {
    let r = rig_with("trustplain", TRUST_DIALOG_PLAIN).await;
    let mut chat = r.hub.chat();
    up(&r, "otter").await;
    let asked = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(Chat::Permission { perm, .. }) = chat.recv().await {
                return perm.action.contains("trust");
            }
        }
    })
    .await;
    assert_eq!(asked, Ok(true), "the question was passed to a person");
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        !r.project.join("trust.log").exists(),
        "nothing was typed into the dialog"
    );
    r.hub.shutdown().await;
}

/// Like NORMAL, and every start is counted as a line in ./starts.log.
const COUNTING: &str = "#!/bin/sh\necho run >> starts.log\necho 'fake claude'\nprintf '? for shortcuts\\n'\nwhile IFS= read -r line; do\n  printf '%s\\n' \"$line\" >> fake.log\n  echo \"ack: $line\"\n  printf '? for shortcuts\\n'\ndone\n";

#[tokio::test]
async fn restart_starts_the_same_agent_again_and_it_keeps_its_place_and_keeps_working() {
    let r = rig_with("restart", COUNTING).await;
    assert!(up_with(&r, "otter", UpOpts::default()).await.ok);
    let starts =
        || std::fs::read_to_string(r.project.join("starts.log")).map_or(0, |s| s.lines().count());
    eventually("started once", async || starts() == 1).await;
    // Asking for one that is not here says so and changes nothing.
    let bad = ipc::call(
        &r.dir,
        &Req::Restart {
            agent: Some("demo/nobody".into()),
        },
    )
    .await
    .unwrap();
    assert!(!bad.ok && bad.msg.contains("nobody"), "{}", bad.msg);
    assert_eq!(starts(), 1);
    // Restart by name, then everything: each is one more start, and the agent is still the same one in the team.
    let one = ipc::call(
        &r.dir,
        &Req::Restart {
            agent: Some("demo/otter".into()),
        },
    )
    .await
    .unwrap();
    assert!(one.ok, "{}", one.msg);
    eventually("started a second time", async || starts() == 2).await;
    let all = ipc::call(&r.dir, &Req::Restart { agent: None })
        .await
        .unwrap();
    assert!(all.ok && all.msg.contains('1'), "{}", all.msg);
    eventually("started a third time", async || starts() == 3).await;
    assert!(
        r.hub
            .call(|c, _| (c.agent("demo/otter").is_some(), vec![]))
            .await
            .unwrap(),
        "still registered"
    );
    r.hub
        .call(|c, now| {
            let r = c
                .human_message(
                    &kd(),
                    "demo",
                    "after the restart",
                    &MessageOpts::default(),
                    now,
                )
                .unwrap();
            ((), r.1)
        })
        .await;
    eventually("the restarted agent hears it", async || {
        std::fs::read_to_string(r.project.join("fake.log"))
            .is_ok_and(|s| s.contains("after the restart"))
    })
    .await;
    r.hub.shutdown().await;
}

#[tokio::test]
async fn an_agents_own_shell_command_can_speak_to_the_team_and_only_with_its_own_key() {
    let r = rig("shellsay").await;
    let mut chat = r.hub.chat();
    let key = up(&r, "otter").await;
    eventually("registered", async || {
        r.hub
            .call(|c, _| (c.agent("demo/otter").is_some(), vec![]))
            .await
            .unwrap()
    })
    .await;
    // The real program, run the way an agent's shell runs it: its environment holds who it is and its secret key, nothing else is passed.
    let run = |agent: &str, key: &str| {
        let (home, agent, key) = (r.dir.clone(), agent.to_string(), key.to_string());
        tokio::task::spawn_blocking(move || {
            std::process::Command::new(env!("CARGO_BIN_EXE_claudecord"))
                .args(["say", "hello from the shell"])
                .env("CLAUDECORD_HOME", home)
                .env("CLAUDECORD_AGENT", agent)
                .env("CLAUDECORD_AGENT_KEY", key)
                .output()
                .unwrap()
        })
    };
    let wrong = run("demo/otter", "not-its-key").await.unwrap();
    assert!(!wrong.status.success(), "a wrong key is refused");
    let right = run("demo/otter", &key).await.unwrap();
    assert!(
        right.status.success(),
        "{}",
        String::from_utf8_lossy(&right.stderr)
    );
    let said = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(Chat::Post { text, .. }) = chat.recv().await
                && text == "hello from the shell"
            {
                return true;
            }
        }
    })
    .await;
    assert_eq!(said, Ok(true), "what the agent said reached the chat");
    // Several lines are piped in (`say -`), so they arrive as real lines and not as one line with backslash-n in it.
    let (home, k) = (r.dir.clone(), key.clone());
    let piped = tokio::task::spawn_blocking(move || {
        use std::io::Write;
        let mut c = std::process::Command::new(env!("CARGO_BIN_EXE_claudecord"))
            .args(["say", "-"])
            .env("CLAUDECORD_HOME", home)
            .env("CLAUDECORD_AGENT", "demo/otter")
            .env("CLAUDECORD_AGENT_KEY", k)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        c.stdin
            .take()
            .unwrap()
            .write_all(b"first line\n- second line\n")
            .unwrap();
        c.wait().unwrap().success()
    })
    .await
    .unwrap();
    assert!(piped);
    let lines = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(Chat::Post { text, .. }) = chat.recv().await
                && text.starts_with("first line")
            {
                return text;
            }
        }
    })
    .await;
    assert_eq!(lines.as_deref(), Ok("first line\n- second line"));
    r.hub.shutdown().await;
}

#[tokio::test]
async fn a_second_agent_in_a_folder_that_is_not_a_git_repository_is_refused_and_told_why() {
    let r = rig("nogit").await;
    up(&r, "otter").await;
    let refused = up_with(&r, "heron", UpOpts::default()).await;
    assert!(
        !refused.ok
            && refused.msg.contains("not a git repository")
            && refused.msg.contains("different folder"),
        "{}",
        refused.msg
    );
    r.hub.shutdown().await;
}

#[tokio::test]
async fn a_file_that_loses_a_piece_on_the_way_is_not_kept_and_the_agent_is_told() {
    let r = rig("lostpiece").await;
    up(&r, "otter").await;
    eventually("registered", async || {
        r.hub
            .call(|c, _| (c.agent("demo/otter").is_some(), vec![]))
            .await
            .unwrap()
    })
    .await;
    let data = vec![b'x'; 450 * 1024]; // three pieces
    r.hub
        .call(move |c, _| {
            let (_, fx) = c
                .send_file(&kd(), "demo", "see this", "big.bin", &data, None, "t2")
                .unwrap();
            // The second piece is lost.
            let fx = fx
                .into_iter()
                .filter(|e| {
                    !matches!(
                        e,
                        Effect::Send {
                            frame: claudecord::protocol::HubFrame::FileChunk { seq: 1, .. },
                            ..
                        }
                    )
                })
                .collect();
            ((), fx)
        })
        .await;
    eventually("agent told it did not arrive", async || {
        std::fs::read_to_string(r.project.join("fake.log"))
            .is_ok_and(|s| s.contains("big.bin did not arrive complete"))
    })
    .await;
    let dir = r.project.join(".claudecord/files/demo");
    let left: Vec<_> = std::fs::read_dir(&dir)
        .map(|d| d.filter_map(|e| e.ok()).map(|e| e.file_name()).collect())
        .unwrap_or_default();
    assert!(left.is_empty(), "nothing partial is kept: {left:?}");
    r.hub.shutdown().await;
}

#[tokio::test]
async fn a_file_changed_on_the_way_fails_its_checksum_and_is_not_kept() {
    let r = rig("badsum").await;
    up(&r, "otter").await;
    eventually("registered", async || {
        r.hub
            .call(|c, _| (c.agent("demo/otter").is_some(), vec![]))
            .await
            .unwrap()
    })
    .await;
    let data = vec![b'x'; 300 * 1024]; // two pieces
    r.hub
        .call(move |c, _| {
            let (_, mut fx) = c
                .send_file(&kd(), "demo", "see this", "big.bin", &data, None, "t3")
                .unwrap();
            // One piece arrives with different bytes (same length, so the pieces still add up).
            for e in &mut fx {
                if let Effect::Send {
                    frame: claudecord::protocol::HubFrame::FileChunk { seq: 0, data, .. },
                    ..
                } = e
                {
                    *data = data.replacen('e', "f", 1);
                    *data = data.replacen('e', "f", 1);
                }
            }
            ((), fx)
        })
        .await;
    eventually("agent told it was damaged", async || {
        std::fs::read_to_string(r.project.join("fake.log"))
            .is_ok_and(|s| s.contains("arrived damaged"))
    })
    .await;
    let left: Vec<_> = std::fs::read_dir(r.project.join(".claudecord/files/demo"))
        .map(|d| d.filter_map(|e| e.ok()).map(|e| e.file_name()).collect())
        .unwrap_or_default();
    assert!(left.is_empty(), "nothing damaged is kept: {left:?}");
    r.hub.shutdown().await;
}

#[tokio::test]
async fn a_second_daemon_does_not_take_over_a_running_ones_socket() {
    let r = rig("second").await;
    let cfg = Config {
        hub_url: "ws://127.0.0.1:1".into(),
        token: "x".into(),
        node_name: "mac2".into(),
    };
    let err = daemon::run(cfg, r.dir.clone(), fast(vec![], 8, vec![]))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("already running"), "{err}");
    assert!(
        ipc::call(&r.dir, &Req::Ping).await.unwrap().ok,
        "the first one still answers"
    );
    r.hub.shutdown().await;
}

#[tokio::test]
async fn two_agents_send_each_other_files_in_the_thread_of_the_pair() {
    use claudecord::hub::effects::Chat;
    let r = rig("peerfile").await;
    let mut chat = r.hub.chat();
    // Two agents on one machine, each in a folder of its own (two agents never share a folder).
    let other = r.project.parent().unwrap().join("demo2");
    std::fs::create_dir_all(&other).unwrap();
    let otter_key = up(&r, "otter").await;
    let resp = ipc::call(
        &r.dir,
        &Req::Up {
            project: "demo".into(),
            name: Some("fox".into()),
            adapter: "claude".into(),
            model: None,
            role: None,
            cwd: other.to_string_lossy().into(),
            policy: "autonomous".into(),
            rows: 24,
            cols: 80,
            opts: UpOpts::default(),
        },
    )
    .await
    .unwrap();
    assert!(resp.ok, "{}", resp.msg);
    let fox_key = resp.data.unwrap()["key"].as_str().unwrap().to_string();
    eventually("both registered", async || {
        r.hub
            .call(|c, _| {
                (
                    c.agent("demo/otter").is_some() && c.agent("demo/fox").is_some(),
                    vec![],
                )
            })
            .await
            .unwrap()
    })
    .await;
    // otter -> fox, then fox -> otter.
    std::fs::write(r.project.join("from-otter.txt"), b"hello fox").unwrap();
    std::fs::write(other.join("from-fox.txt"), b"hello otter").unwrap();
    for (agent, key, path, to, caption) in [
        (
            "demo/otter",
            &otter_key,
            "from-otter.txt",
            "fox",
            "the data",
        ),
        ("demo/fox", &fox_key, "from-fox.txt", "otter", "thanks"),
    ] {
        let sent = ipc::call_as(
            &r.dir,
            Some(key),
            &Req::Send {
                agent: agent.into(),
                path: path.into(),
                to: Some(to.into()),
                caption: Some(caption.into()),
            },
        )
        .await
        .unwrap();
        assert!(sent.ok, "{}", sent.msg);
    }
    let landed = |dir: &std::path::Path, name: &str, body: &[u8]| {
        std::fs::read_dir(dir.join(".claudecord/files/demo"))
            .ok()
            .into_iter()
            .flatten()
            .flatten()
            .any(|e| {
                e.file_name().to_string_lossy().ends_with(name)
                    && std::fs::read(e.path()).is_ok_and(|b| b == body)
            })
    };
    eventually("fox has otter's file", async || {
        landed(&other, "from-otter.txt", b"hello fox")
    })
    .await;
    eventually("otter has fox's file", async || {
        landed(&r.project, "from-fox.txt", b"hello otter")
    })
    .await;
    // The chat shows both transfers in the pair's thread, not in the main channel.
    let mut shown = Vec::new();
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while shown.len() < 2 && std::time::Instant::now() < deadline {
        if let Ok(Ok(Chat::Post { text, thread, .. })) =
            tokio::time::timeout(Duration::from_millis(200), chat.recv()).await
            && text.starts_with("Sent ")
        {
            shown.push((text, thread));
        }
    }
    assert_eq!(shown.len(), 2, "{shown:?}");
    for (text, thread) in &shown {
        assert_eq!(thread.as_deref(), Some("fox & otter"), "{text}");
    }
    r.hub.shutdown().await;
}

#[tokio::test]
async fn one_folder_serves_several_projects_and_offers_the_one_used_last() {
    let r = rig("manyprojects").await;
    let up_in = async |project: &str, name: &str| {
        ipc::call(
            &r.dir,
            &Req::Up {
                project: project.into(),
                name: Some(name.into()),
                adapter: "claude".into(),
                model: None,
                role: None,
                cwd: r.project.to_string_lossy().into(),
                policy: "autonomous".into(),
                rows: 24,
                cols: 80,
                opts: UpOpts::default(),
            },
        )
        .await
        .unwrap()
    };
    // The same folder, started for "demo", then (after that agent ended) for "other", then for "demo" again.
    assert!(up_in("demo", "otter").await.ok);
    let cfg = claudecord::device::config::project_of_folder;
    assert_eq!(cfg(&r.dir, &r.project).as_deref(), Some("demo"));
    assert!(
        ipc::call(
            &r.dir,
            &Req::Stop {
                agent: "demo/otter".into()
            }
        )
        .await
        .unwrap()
        .ok
    );
    assert!(up_in("other", "fox").await.ok);
    // Both projects keep the folder; "demo" sorts first, but "other" was used last and is what is offered.
    let map = claudecord::device::config::read_projects(&r.dir);
    assert!(
        map["demo"].contains(&r.project) && map["other"].contains(&r.project),
        "{map:?}"
    );
    assert_eq!(cfg(&r.dir, &r.project).as_deref(), Some("other"));
    assert!(
        ipc::call(
            &r.dir,
            &Req::Stop {
                agent: "other/fox".into()
            }
        )
        .await
        .unwrap()
        .ok
    );
    assert!(up_in("demo", "heron").await.ok);
    assert_eq!(cfg(&r.dir, &r.project).as_deref(), Some("demo"));
    // The bookkeeping key is never taken for a project.
    assert!(!claudecord::device::config::read_projects(&r.dir).contains_key("_last"));
    r.hub.shutdown().await;
}

#[tokio::test]
async fn files_sent_to_an_agent_land_in_the_inbox_of_its_project_only() {
    let r = rig("projectinbox").await;
    up(&r, "otter").await;
    eventually("registered", async || {
        r.hub
            .call(|c, _| (c.agent("demo/otter").is_some(), vec![]))
            .await
            .unwrap()
    })
    .await;
    r.hub
        .call(|c, _| {
            let (_, fx) = c
                .send_file(&kd(), "demo", "x", "a.txt", b"mine", None, "t9")
                .unwrap();
            ((), fx)
        })
        .await;
    eventually("saved in the project's own inbox", async || {
        std::fs::read(r.project.join(".claudecord/files/demo/a.txt")).is_ok_and(|b| b == b"mine")
    })
    .await;
    // Nothing is put loose in the shared folder, where another project's agent would find it.
    assert!(!r.project.join(".claudecord/files/a.txt").exists());
    r.hub.shutdown().await;
}

#[tokio::test]
async fn a_file_sent_again_under_the_same_name_replaces_the_old_one() {
    let r = rig("resend").await;
    up(&r, "otter").await;
    eventually("registered", async || {
        r.hub
            .call(|c, _| (c.agent("demo/otter").is_some(), vec![]))
            .await
            .unwrap()
    })
    .await;
    let path = r.project.join(".claudecord/files/demo/plan.md");
    for (id, body) in [("t1", &b"first"[..]), ("t2", &b"second"[..])] {
        r.hub
            .call(move |c, _| {
                let (_, fx) = c
                    .send_file(&kd(), "demo", "x", "plan.md", body, None, id)
                    .unwrap();
                ((), fx)
            })
            .await;
        eventually("saved", async || {
            std::fs::read(&path).is_ok_and(|b| b == body)
        })
        .await;
    }
    let left: Vec<_> = std::fs::read_dir(path.parent().unwrap())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        left,
        vec!["plan.md"],
        "one canonical file, no copies or leftovers"
    );
    r.hub.shutdown().await;
}

#[tokio::test]
async fn a_moved_agent_is_filed_under_its_new_project_and_still_speaks_with_its_old_environment() {
    use claudecord::hub::effects::Chat;
    let r = rig("moveagent").await;
    let mut chat = r.hub.chat();
    let key = up(&r, "otter").await;
    eventually("registered", async || {
        r.hub
            .call(|c, _| (c.agent("demo/otter").is_some(), vec![]))
            .await
            .unwrap()
    })
    .await;
    // Moved by the owner from "demo" to "other".
    let moved = r
        .hub
        .call(
            |c, now| match c.move_agent(&kd(), "demo", "otter", "other", now) {
                Ok((_, fx)) => (true, fx),
                Err(e) => panic!("{e:?}"),
            },
        )
        .await
        .unwrap();
    assert!(moved);
    // The machine keeps the folder under the new project, offers it first, and saves files for the agent in that project's inbox.
    eventually("the machine knows", async || {
        claudecord::device::config::project_of_folder(&r.dir, &r.project).as_deref()
            == Some("other")
    })
    .await;
    r.hub
        .call(|c, _| {
            let (_, fx) = c
                .send_file(&kd(), "other", "x", "n.txt", b"new home", None, "t7")
                .unwrap();
            ((), fx)
        })
        .await;
    eventually("file in the new project's inbox", async || {
        std::fs::read(r.project.join(".claudecord/files/other/n.txt"))
            .is_ok_and(|b| b == b"new home")
    })
    .await;
    // The agent's program still carries its old id in its environment. What it says is filed under the new project, and only there.
    let said = ipc::call_as(
        &r.dir,
        Some(&key),
        &Req::Say {
            agent: "demo/otter".into(),
            text: "settled in".into(),
            thread: None,
        },
    )
    .await
    .unwrap();
    assert!(said.ok, "{}", said.msg);
    let mut seen = None;
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while seen.is_none() && std::time::Instant::now() < deadline {
        if let Ok(Ok(Chat::Post { project, text, .. })) =
            tokio::time::timeout(Duration::from_millis(200), chat.recv()).await
            && text == "settled in"
        {
            seen = Some(project);
        }
    }
    assert_eq!(seen.as_deref(), Some("other"));
    r.hub.shutdown().await;
}

fn spec_of(project: &str, name: &str) -> claudecord::protocol::AgentSpec {
    claudecord::protocol::AgentSpec {
        agent_id: format!("{project}/{name}"),
        name: name.into(),
        project: project.into(),
        adapter: claudecord::protocol::AdapterId::Claude,
        model: None,
        role: None,
    }
}

/// A rig whose daemon does what a real one does: it starts no agent on its own, only when the hub asks.
async fn rig_hub_only(name: &str) -> Rig {
    rig_tuned(name, NORMAL, 8, vec![], Backend::Pty, |o| {
        o.dev_spawn = false
    })
    .await
}

/// What a waiting `claudecord` tells the daemon about where the agent goes.
fn expect(code: &str, cwd: &Path) -> Req {
    Req::Expect {
        code: code.into(),
        cwd: cwd.to_string_lossy().into(),
        rows: 24,
        cols: 80,
        opts: claudecord::device::ipc::ExpectOpts {
            policy: "autonomous".into(),
            ..Default::default()
        },
    }
}

async fn pending(r: &Rig, code: &str) -> serde_json::Value {
    let resp = ipc::call(&r.dir, &Req::Pending { code: code.into() })
        .await
        .unwrap();
    assert!(resp.ok, "{}", resp.msg);
    resp.data.unwrap()
}

#[tokio::test]
async fn nothing_starts_an_agent_on_a_machine_but_the_hub() {
    let r = rig_hub_only("hubonly").await;
    // The local socket refuses, whoever asks and however it asks.
    let refused = ipc::call(
        &r.dir,
        &Req::Up {
            project: "demo".into(),
            name: Some("otter".into()),
            adapter: "claude".into(),
            model: None,
            role: None,
            cwd: r.project.to_string_lossy().into(),
            policy: "autonomous".into(),
            rows: 24,
            cols: 80,
            opts: UpOpts {
                command: Some(vec!["cat".into()]),
                ..Default::default()
            },
        },
    )
    .await
    .unwrap();
    assert!(
        !refused.ok && refused.msg.contains("started by the hub"),
        "{}",
        refused.msg
    );
    let list = ipc::call(&r.dir, &Req::List).await.unwrap().data.unwrap();
    assert_eq!(list.as_array().unwrap().len(), 0);
    // The hub can: with a waiting `claudecord`, in its folder, and the start is told.
    assert!(
        ipc::call(&r.dir, &expect("abc123", &r.project))
            .await
            .unwrap()
            .ok
    );
    assert_eq!(pending(&r, "abc123").await["state"], "waiting");
    eventually("the machine is connected", async || {
        r.hub
            .call(|c, _| (c.is_connected("mac"), vec![]))
            .await
            .unwrap()
    })
    .await;
    r.hub
        .call(|c, _| {
            let mut spec = spec_of("demo", "fox");
            spec.role = Some("lead".into());
            let (sent, fx) = c
                .spawn_with(
                    &kd(),
                    "demo",
                    "mac",
                    spec,
                    claudecord::hub::controls::SpawnExtra {
                        command: None,
                        pick: Some("abc123".into()),
                    },
                )
                .unwrap();
            assert!(sent);
            ((), fx)
        })
        .await;
    eventually("started", async || {
        pending(&r, "abc123").await["state"] == "started"
    })
    .await;
    assert_eq!(pending(&r, "abc123").await["agent"], "demo/fox");
    eventually("registered with the hub", async || {
        r.hub
            .call(|c, _| {
                (
                    c.agent("demo/fox")
                        .is_some_and(|a| a.role.as_deref() == Some("lead")),
                    vec![],
                )
            })
            .await
            .unwrap()
    })
    .await;
    // A code nobody is waiting under starts nothing in a folder it names itself: the hub never chooses a folder.
    r.hub
        .call(|c, _| {
            let (_, fx) = c
                .spawn_with(
                    &kd(),
                    "demo",
                    "mac",
                    spec_of("elsewhere", "ghost"),
                    claudecord::hub::controls::SpawnExtra {
                        command: None,
                        pick: Some("nobody".into()),
                    },
                )
                .unwrap();
            ((), fx)
        })
        .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let list = ipc::call(&r.dir, &Req::List).await.unwrap().data.unwrap();
    assert_eq!(list.as_array().unwrap().len(), 1, "only fox");
    r.hub.shutdown().await;
}

#[tokio::test]
async fn a_saved_command_runs_only_on_a_machine_that_allowed_hub_commands() {
    let r = rig_hub_only("savedcmd").await;
    eventually("the machine is connected", async || {
        r.hub
            .call(|c, _| (c.is_connected("mac"), vec![]))
            .await
            .unwrap()
    })
    .await;
    let spawn_with_command = async |code: &str, name: &str| {
        assert!(
            ipc::call(&r.dir, &expect(code, &r.project))
                .await
                .unwrap()
                .ok
        );
        let (code, name) = (code.to_string(), name.to_string());
        r.hub
            .call(move |c, _| {
                let (_, fx) = c
                    .spawn_with(
                        &kd(),
                        "demo",
                        "mac",
                        spec_of("demo", &name),
                        claudecord::hub::controls::SpawnExtra {
                            command: Some("echo SAVED-COMMAND-RAN-$((6*7)); exec cat".into()),
                            pick: Some(code),
                        },
                    )
                    .unwrap();
                ((), fx)
            })
            .await;
    };
    // Not allowed (the default): refused, with the way to allow it, and the hub's chat is told too.
    let mut chat = r.hub.chat();
    spawn_with_command("c1", "first").await;
    eventually("refused", async || {
        pending(&r, "c1").await["state"] == "failed"
    })
    .await;
    let why = pending(&r, "c1").await["error"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(why.contains("custom-commands on"), "{why}");
    let mut told = false;
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    while !told && std::time::Instant::now() < deadline {
        if let Ok(Ok(claudecord::hub::effects::Chat::Notice { text, .. })) =
            tokio::time::timeout(Duration::from_millis(200), chat.recv()).await
        {
            told = text.contains("custom-commands on");
        }
    }
    assert!(told, "the chat never said why");
    assert_eq!(
        ipc::call(&r.dir, &Req::List)
            .await
            .unwrap()
            .data
            .unwrap()
            .as_array()
            .unwrap()
            .len(),
        0
    );
    // Allowed on this machine: it runs, in a login shell, as the line says.
    claudecord::device::config::set_custom_commands(&r.dir, true).unwrap();
    spawn_with_command("c2", "second").await;
    eventually("started", async || {
        pending(&r, "c2").await["state"] == "started"
    })
    .await;
    eventually("the line ran", async || {
        std::fs::read_dir(r.dir.join("logs"))
            .ok()
            .into_iter()
            .flatten()
            .flatten()
            .any(|d| {
                std::fs::read_to_string(d.path().join("terminal.log"))
                    .is_ok_and(|t| t.contains("SAVED-COMMAND-RAN-42"))
            })
    })
    .await;
    r.hub.shutdown().await;
}

#[tokio::test]
async fn a_start_that_is_waiting_for_the_hub_keeps_the_daemon_from_leaving() {
    let r = rig_tuned("waitkeeps", NORMAL, 8, vec![], Backend::Pty, |o| {
        o.dev_spawn = false;
        o.idle_exit = Some(Duration::from_millis(600));
    })
    .await;
    assert!(
        ipc::call(&r.dir, &expect("held", &r.project))
            .await
            .unwrap()
            .ok
    );
    // With no agent and a start waiting, well past the idle limit, it is still there.
    tokio::time::sleep(Duration::from_millis(2000)).await;
    assert!(
        ipc::call(&r.dir, &Req::Ping).await.is_ok(),
        "the daemon left while a start was waiting"
    );
    r.hub.shutdown().await;
}

#[tokio::test]
async fn a_daemon_whose_agents_all_sit_idle_closes_them_and_goes_away() {
    let r = rig_tuned("idleagents", NORMAL, 8, vec![], Backend::Pty, |o| {
        o.idle_exit = Some(Duration::from_millis(600));
        o.idle_agents_exit = Some(Duration::from_millis(1500));
    })
    .await;
    up(&r, "otter").await;
    eventually("idle", async || {
        r.hub
            .call(|c, _| (c.status_of("demo/otter") == AgentStatus::Idle, vec![]))
            .await
            .unwrap()
    })
    .await;
    // Nothing is sent to it, and nobody touches its terminal: it is given up on.
    eventually("the daemon to leave", async || {
        ipc::call(&r.dir, &Req::Ping).await.is_err()
    })
    .await;
    r.hub.shutdown().await;
}

#[tokio::test]
async fn a_daemon_with_an_agent_that_keeps_getting_messages_stays() {
    let r = rig_tuned("busyagents", NORMAL, 8, vec![], Backend::Pty, |o| {
        o.idle_exit = Some(Duration::from_millis(600));
        o.idle_agents_exit = Some(Duration::from_millis(2500));
    })
    .await;
    up(&r, "otter").await;
    eventually("idle", async || {
        r.hub
            .call(|c, _| (c.status_of("demo/otter") == AgentStatus::Idle, vec![]))
            .await
            .unwrap()
    })
    .await;
    // A message every second changes its screen, so it is never idle for the whole time.
    for n in 0..5 {
        r.hub
            .call(move |c, now| {
                let r = c
                    .human_message(
                        &kd(),
                        "demo",
                        &format!("ping {n}"),
                        &MessageOpts::default(),
                        now,
                    )
                    .unwrap();
                ((), r.1)
            })
            .await;
        tokio::time::sleep(Duration::from_millis(1000)).await;
        assert!(
            ipc::call(&r.dir, &Req::Ping).await.is_ok(),
            "the daemon left while the agent was in use"
        );
    }
    r.hub.shutdown().await;
}

#[tokio::test]
async fn messages_waiting_in_different_threads_are_pasted_one_thread_at_a_time() {
    let r = rig_tuned("threads", NORMAL, 8, vec![], Backend::Pty, |o| {
        o.quiet = (0, 60_000)
    })
    .await;
    up(&r, "otter").await;
    eventually("registered", async || {
        r.hub
            .call(|c, _| (c.agent("demo/otter").is_some(), vec![]))
            .await
            .unwrap()
    })
    .await;
    for (text, thread, reference) in [("alpha", "T1 one", "c:1"), ("beta", "T2 two", "c:2")] {
        r.hub
            .call(move |c, now| {
                let r = c
                    .human_message(
                        &kd(),
                        "demo",
                        text,
                        &MessageOpts {
                            thread: Some(thread),
                            reference: Some(reference),
                            ..Default::default()
                        },
                        now,
                    )
                    .unwrap();
                ((), r.1)
            })
            .await;
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    r.hub
        .call(|c, _| {
            let mut fx = Vec::new();
            let _ = c.prioritise(&kd(), "demo", "c:1", "now", &mut fx);
            ((), fx)
        })
        .await;
    eventually("the first thread", async || {
        std::fs::read_to_string(r.project.join("fake.log")).is_ok_and(|s| s.contains("alpha"))
    })
    .await;
    tokio::time::sleep(Duration::from_millis(800)).await;
    let log = std::fs::read_to_string(r.project.join("fake.log")).unwrap();
    assert!(
        !log.contains("beta"),
        "the second thread was mixed in: {log}"
    );
}

#[tokio::test]
async fn a_say_to_a_peer_returns_delivered_once_the_peer_has_it_and_a_plain_say_returns_at_once() {
    let r = rig("sayreceipt").await;
    let key = up(&r, "otter").await;
    // The peer works in a folder of its own: two agents never share one.
    let other = r.project.parent().unwrap().join("demo2");
    std::fs::create_dir_all(&other).unwrap();
    let resp = ipc::call(
        &r.dir,
        &Req::Up {
            project: "demo".into(),
            name: Some("heron".into()),
            adapter: "claude".into(),
            model: None,
            role: None,
            cwd: other.to_string_lossy().into(),
            policy: "autonomous".into(),
            rows: 24,
            cols: 80,
            opts: UpOpts::default(),
        },
    )
    .await
    .unwrap();
    assert!(resp.ok, "{}", resp.msg);
    eventually("both registered", async || {
        r.hub
            .call(|c, _| {
                (
                    c.agent("demo/otter").is_some() && c.agent("demo/heron").is_some(),
                    vec![],
                )
            })
            .await
            .unwrap()
    })
    .await;
    let say = |text: &str| {
        let (dir, key, text) = (r.dir.clone(), key.clone(), text.to_string());
        async move {
            let started = std::time::Instant::now();
            let resp = ipc::call_as(
                &dir,
                Some(&key),
                &Req::Say {
                    agent: "demo/otter".into(),
                    text,
                    thread: None,
                },
            )
            .await
            .unwrap();
            (resp, started.elapsed())
        }
    };
    let (plain, took) = say("thinking aloud").await;
    assert!(plain.ok && plain.msg == "sent", "{}", plain.msg);
    assert!(took < Duration::from_secs(3), "a plain say waited {took:?}");
    let (named, _) = say("@heron please look at this").await;
    assert!(
        named.ok && named.msg.starts_with("delivered"),
        "{}",
        named.msg
    );
    // It really is in the peer's terminal by then.
    assert!(
        std::fs::read_to_string(other.join("fake.log"))
            .unwrap_or_default()
            .contains("please look at this")
    );
    r.hub.shutdown().await;
}

/// Never shows its input box, so it stays "starting" and nothing can be pasted into it.
const NEVER_READY: &str = "#!/bin/sh\nsleep 60\n";

#[tokio::test]
async fn a_say_to_a_peer_whose_terminal_cannot_take_it_fails_with_the_reason_and_is_not_a_success()
{
    let r = rig_with("sayfails", NEVER_READY).await;
    let key = up(&r, "otter").await;
    let other = r.project.parent().unwrap().join("demo2");
    std::fs::create_dir_all(&other).unwrap();
    let resp = ipc::call(
        &r.dir,
        &Req::Up {
            project: "demo".into(),
            name: Some("heron".into()),
            adapter: "claude".into(),
            model: None,
            role: None,
            cwd: other.to_string_lossy().into(),
            policy: "autonomous".into(),
            rows: 24,
            cols: 80,
            opts: UpOpts::default(),
        },
    )
    .await
    .unwrap();
    assert!(resp.ok, "{}", resp.msg);
    eventually("both registered", async || {
        r.hub
            .call(|c, _| {
                (
                    c.agent("demo/otter").is_some() && c.agent("demo/heron").is_some(),
                    vec![],
                )
            })
            .await
            .unwrap()
    })
    .await;
    let resp = ipc::call_as(
        &r.dir,
        Some(&key),
        &Req::Say {
            agent: "demo/otter".into(),
            text: "@heron are you there".into(),
            thread: None,
        },
    )
    .await
    .unwrap();
    assert!(
        !resp.ok,
        "a failed say must not look like a success: {}",
        resp.msg
    );
    assert!(
        resp.msg.starts_with("NOT delivered") && resp.msg.contains("not ready for input"),
        "{}",
        resp.msg
    );
    r.hub.shutdown().await;
}

// On its own threads, as the hub and a machine are in real life: on one thread a debug build's ten megabytes of encoding and hashing would stop
// every heartbeat for most of a second, which says nothing about the program.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_large_file_in_many_pieces_arrives_byte_for_byte_and_leaves_no_part_file() {
    let _heavy = super::procs::BIG_BUFFERS.read().await;
    // A heartbeat like a real one: this test is about the file, and a debug build's encoding and hashing on a busy runner can take longer than the
    // rig's usual 200 ms.
    let r = rig_tuned("bigfile", NORMAL, 8, vec![], Backend::Pty, |o| {
        o.link.ping_every = Duration::from_secs(5)
    })
    .await;
    up(&r, "otter").await;
    eventually("registered", async || {
        r.hub
            .call(|c, _| (c.agent("demo/otter").is_some(), vec![]))
            .await
            .unwrap()
    })
    .await;
    // Four megabytes, past what the hub once allowed a device to have waiting, of a pattern that shows if a piece is lost, doubled or out of place.
    let body: Vec<u8> = (0..4_000_000u32).map(|i| (i % 251) as u8).collect();
    let sent = body.clone();
    r.hub
        .call(move |c, _| {
            let (_, fx) = c
                .send_file(&kd(), "demo", "x", "model.bin", &sent, None, "t1")
                .unwrap();
            ((), fx)
        })
        .await;
    let path = r.project.join(".claudecord/files/demo/model.bin");
    eventually("the whole file", async || {
        std::fs::read(&path).is_ok_and(|b| b == body)
    })
    .await;
    let left: Vec<_> = std::fs::read_dir(path.parent().unwrap())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(left, vec!["model.bin"], "no .part file is left behind");
    r.hub.shutdown().await;
}

#[tokio::test]
async fn a_file_over_the_limit_is_refused_by_its_size_before_it_is_read() {
    let r = rig("toobig").await;
    let key = up(&r, "otter").await;
    let big = r.project.join("big.bin");
    // A sparse file: the size is there, the bytes are not, so reading it would be slow and large.
    let f = std::fs::File::create(&big).unwrap();
    f.set_len(1_000_000_000).unwrap();
    let started = std::time::Instant::now();
    let resp = ipc::call_as(
        &r.dir,
        Some(&key),
        &Req::Send {
            agent: "demo/otter".into(),
            path: "big.bin".into(),
            to: None,
            caption: None,
        },
    )
    .await
    .unwrap();
    assert!(!resp.ok && resp.msg.contains("limit"), "{}", resp.msg);
    assert!(started.elapsed() < Duration::from_secs(2));
    r.hub.shutdown().await;
}

#[tokio::test]
async fn threads_answers_in_the_same_call_with_where_the_agent_can_post() {
    let r = rig("threadlist").await;
    let key = up(&r, "otter").await;
    eventually("registered", async || {
        r.hub
            .call(|c, _| (c.agent("demo/otter").is_some(), vec![]))
            .await
            .unwrap()
    })
    .await;
    let started = std::time::Instant::now();
    let resp = ipc::call_as(
        &r.dir,
        Some(&key),
        &Req::Threads {
            agent: "demo/otter".into(),
        },
    )
    .await
    .unwrap();
    assert!(resp.ok && resp.msg.contains("main channel"), "{}", resp.msg);
    assert!(started.elapsed() < Duration::from_secs(3));
    r.hub.shutdown().await;
}

#[tokio::test]
async fn a_steering_press_stops_the_agent_with_escape_before_the_message_goes_in() {
    // Busy for a minute, so only a steering order gets a message in.
    let r = rig_tuned("steer", NORMAL, 8, vec![], Backend::Pty, |o| {
        o.quiet = (0, 60_000)
    })
    .await;
    up(&r, "otter").await;
    eventually("registered", async || {
        r.hub
            .call(|c, _| (c.agent("demo/otter").is_some(), vec![]))
            .await
            .unwrap()
    })
    .await;
    r.hub
        .call(|c, now| {
            let r = c
                .human_message(
                    &kd(),
                    "demo",
                    "stop and look",
                    &MessageOpts {
                        reference: Some("c:3"),
                        ..Default::default()
                    },
                    now,
                )
                .unwrap();
            ((), r.1)
        })
        .await;
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert!(
        std::fs::read_to_string(r.project.join("fake.log")).is_err(),
        "nothing goes in while it is busy"
    );
    r.hub
        .call(|c, _| {
            let mut fx = Vec::new();
            let _ = c.prioritise(&kd(), "demo", "c:3", "steer", &mut fx);
            ((), fx)
        })
        .await;
    eventually("the message", async || {
        std::fs::read(r.project.join("fake.log"))
            .is_ok_and(|b| String::from_utf8_lossy(&b).contains("stop and look"))
    })
    .await;
    let log = std::fs::read(r.project.join("fake.log")).unwrap();
    let text = String::from_utf8_lossy(&log);
    let esc = text.find('\u{1b}').expect("Escape reached the terminal");
    assert!(
        esc < text.find("stop and look").unwrap(),
        "Escape came first: {text:?}"
    );
    r.hub.shutdown().await;
}
