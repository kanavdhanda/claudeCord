//! Everything together, on one machine: a real hub, a real device daemon, and a stand-in agent (a shell script named
//! `claude` that prints the ready line and echoes each line it is given). A person's message goes in through the hub and
//! must come out in the agent's terminal; the agent's own commands must come out in the hub.

use claudecord::device::config::Config;
use claudecord::device::daemon::{self, Options};
use claudecord::device::ipc::{self, Req};
use claudecord::device::link::LinkOpts;
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
    }
}

async fn eventually(what: &str, mut f: impl AsyncFnMut() -> bool) {
    for _ in 0..300 {
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
        daemon::run(cfg, d, fast(vec![bin], max_agents, labels))
            .await
            .unwrap()
    });
    eventually("daemon socket", async || {
        ipc::call(&dir, &Req::Ping).await.is_ok()
    })
    .await;
    let project = root.join("demo");
    std::fs::create_dir_all(&project).unwrap();
    Rig { hub, dir, project }
}

async fn up(r: &Rig, name: &str) {
    let resp = ipc::call(
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
        },
    )
    .await
    .unwrap();
    assert!(resp.ok, "{}", resp.msg);
}

#[tokio::test]
async fn a_persons_message_reaches_the_agents_terminal_and_the_agent_speaks_back() {
    let r = rig("flow").await;
    let mut chat = r.hub.chat();
    up(&r, "otter").await;
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
    let resp = ipc::call(
        &r.dir,
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
    up(&r, "otter").await;
    eventually("registered", async || {
        r.hub
            .call(|c, _| (c.agent("demo/otter").is_some(), vec![]))
            .await
            .unwrap()
    })
    .await;
    let resp = ipc::call(
        &r.dir,
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
    up(&r, "otter").await;
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
    up(&r, "otter").await;
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
    up(&r, "heron").await;
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
