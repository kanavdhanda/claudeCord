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
    let root = tmp(name);
    let bin = fake_claude(&root, script);
    let mut store = Store::open(&root.join("hub.db"), None).unwrap();
    let token = store.create_token("mac", 0).unwrap();
    let mut core = HubCore::default();
    core.add_owner("1");
    let hub = server::start(
        ServerConfig {
            bind: "127.0.0.1:0".parse().unwrap(),
            ping_every: Duration::from_millis(200),
            save_every: Duration::from_millis(10),
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
    tokio::spawn(async move {
        let mut opts = fast(vec![bin], max_agents, labels);
        opts.backend = backend;
        daemon::run(cfg, d, opts).await.unwrap()
    });
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
        std::fs::read(r.project.join(".claudecord/inbox/t1-plan.txt"))
            .is_ok_and(|b| b == b"the plan")
    })
    .await;
    eventually("agent told", async || {
        std::fs::read_to_string(r.project.join("fake.log"))
            .is_ok_and(|s| s.contains(".claudecord/inbox/t1-plan.txt"))
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
    // Without asking for a worktree, a second agent in the same folder is refused.
    let refused = up_with(&r, "heron", UpOpts::default()).await;
    assert!(
        !refused.ok && refused.msg.contains("--worktree"),
        "{}",
        refused.msg
    );
    let resp = up_with(
        &r,
        "heron",
        UpOpts {
            worktree: true,
            ..Default::default()
        },
    )
    .await;
    assert!(resp.ok, "{}", resp.msg);
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
        !r.project.join("fake.log").exists(),
        "the first agent's folder was not touched"
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
