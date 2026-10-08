//! Demo data for `claudecord-hub serve --dev`: so the dashboard has something to show without any real machine or Discord. It makes a
//! made-up account with a bot, a placed project, a machine and a few agents, then plays a short day of work through the real hub core
//! (messages, a task handed out and finished, a question asked and answered, states changing) so every graph has data.
//! Never runs outside `--dev`, and everything it makes goes through the same paths real activity would.

use crate::control::registry::Registry;
use crate::control::{Control, Placement, seal};
use crate::discord::api::Rest;
use crate::hub::{Human, MessageOpts};
use crate::protocol::{AdapterId, AgentSpec, AgentStatus, NodeFrame};

fn spec(project: &str, name: &str) -> AgentSpec {
    AgentSpec {
        agent_id: format!("{project}/{name}"),
        name: name.into(),
        project: project.into(),
        adapter: AdapterId::Claude,
        model: None,
        role: None,
    }
}

/// Fills the dev account's hub with a believable few hours of activity.
pub async fn seed(
    control: &Control,
    registry: &Registry,
    keys: &dyn seal::KeyProvider,
) -> Result<(), String> {
    let now = crate::now_ms();
    let account = control
        .sign_in_discord("900001", "Dev", now)
        .map_err(|e| e.to_string())?;
    let tenant = registry.hub(&account).map_err(|e| e.to_string())?;
    // A bot and a channel in the stand-in Discord, and the project placed there.
    let (app_id, token) = (
        "111111111111111111",
        "MTExMTExMTExMTExMTExMTEx.AAAAAA.demodemodemodemodemo",
    );
    let sealed = seal::seal(keys, &account.id, app_id, token).ok_or("could not seal")?;
    let bot = control
        .save_bot(&account.id, app_id, "demo-bot", &sealed, now)
        .map_err(|e| e.to_string())?;
    let rest = Rest::new(&registry.discord().api_base, token);
    let channel = rest
        .create_channel("g1", "website", "claudeCord project")
        .await
        .map_err(|e| e.to_string())?;
    control
        .set_target(
            &account.id,
            &Placement {
                project: "website".into(),
                bot: bot.clone(),
                guild: "g1".into(),
                channel,
                guild_name: "Test server".into(),
                channel_name: "website".into(),
            },
        )
        .map_err(|e| e.to_string())?;
    registry.start_bridge(&tenant, &bot);

    let kd = Human {
        id: account.discord_id.clone(),
        name: "kd".into(),
    };
    let h = tenant.state.handle.clone();
    h.call(move |c, _| {
        let min = 60_000i64;
        let t0 = now - 5 * 60 * min;
        let mut fx = Vec::new();
        c.add_owner(&kd.id);
        fx.extend(c.node_connected("demo-laptop", 9000));
        c.touch("demo-laptop", t0);
        let mut at = t0;
        let mut step = |n: i64| {
            at += n * min;
            at
        };
        let say =
            |c: &mut crate::hub::HubCore, fx: &mut Vec<_>, node: &str, f: NodeFrame, t: i64| {
                fx.extend(c.on_node_frame(node, f, t));
            };
        for (p, n) in [
            ("website", "otter"),
            ("website", "mole"),
            ("website", "fox"),
            ("api", "heron"),
        ] {
            let t = step(1);
            say(
                c,
                &mut fx,
                "demo-laptop",
                NodeFrame::AgentRegister {
                    agent: spec(p, n),
                    cwd: format!("/code/{p}"),
                },
                t,
            );
        }
        let status = |c: &mut crate::hub::HubCore,
                      fx: &mut Vec<_>,
                      p: &str,
                      n: &str,
                      s: AgentStatus,
                      t: i64| {
            fx.extend(c.on_node_frame(
                "demo-laptop",
                NodeFrame::AgentStatus {
                    agent_id: format!("{p}/{n}"),
                    status: s,
                    detail: None,
                },
                t,
            ));
        };
        // The owner asks the lead for something; the lead hands tasks to two others; they work and finish.
        let t = step(10);
        if let Ok((_, f)) = c.human_message(
            &kd,
            "website",
            "Build the pricing page, then wire the checkout.",
            &MessageOpts::default(),
            t,
        ) {
            fx.extend(f);
        }
        for (n, s, gap) in [
            ("otter", AgentStatus::Thinking, 1),
            ("otter", AgentStatus::Executing, 4),
        ] {
            let t = step(gap);
            status(c, &mut fx, "website", n, s, t);
        }
        for (to, task) in [("mole", "Pricing page layout"), ("fox", "Checkout form")] {
            let t = step(2);
            say(
                c,
                &mut fx,
                "demo-laptop",
                NodeFrame::AgentAssign {
                    agent_id: "website/otter".into(),
                    to: to.into(),
                    task: task.into(),
                    thread: None,
                },
                t,
            );
        }
        let t = step(1);
        status(c, &mut fx, "website", "otter", AgentStatus::Idle, t);
        for (n, work) in [("mole", 25), ("fox", 40)] {
            let t = step(1);
            status(c, &mut fx, "website", n, AgentStatus::Thinking, t);
            let t = step(work / 2);
            status(c, &mut fx, "website", n, AgentStatus::Executing, t);
        }
        // A question that waits a while for a person.
        let t = step(3);
        say(
            c,
            &mut fx,
            "demo-laptop",
            NodeFrame::AgentAsk {
                agent_id: "website/fox".into(),
                ask_id: "q-a".into(),
                question: "Stripe or Paddle for payments?".into(),
                options: Some(vec!["Stripe".into(), "Paddle".into()]),
                thread: None,
            },
            t,
        );
        let t = step(14);
        if let Ok((_, f)) = c.human_message(
            &kd,
            "website",
            "Stripe.",
            &MessageOpts {
                answers_ask: Some("Q1"),
                ..Default::default()
            },
            t,
        ) {
            fx.extend(f);
        }
        let t = step(6);
        status(c, &mut fx, "website", "fox", AgentStatus::Executing, t);
        let t = step(10);
        say(
            c,
            &mut fx,
            "demo-laptop",
            NodeFrame::AgentTaskDone {
                agent_id: "website/mole".into(),
                task_id: "T1".into(),
                summary: "Pricing page done".into(),
            },
            t,
        );
        status(c, &mut fx, "website", "mole", AgentStatus::Idle, t);
        let t = step(12);
        say(
            c,
            &mut fx,
            "demo-laptop",
            NodeFrame::AgentTaskDone {
                agent_id: "website/fox".into(),
                task_id: "T2".into(),
                summary: "Checkout wired to Stripe".into(),
            },
            t,
        );
        status(c, &mut fx, "website", "fox", AgentStatus::Idle, t);
        // A second project with one agent, not placed in Discord yet, so the dashboard shows what an unplaced one looks like.
        let t = step(5);
        if let Ok((_, f)) = c.human_message(
            &kd,
            "api",
            "Add rate limiting to the public endpoints.",
            &MessageOpts::default(),
            t,
        ) {
            fx.extend(f);
        }
        let t = step(2);
        status(c, &mut fx, "api", "heron", AgentStatus::Thinking, t);
        ((), fx)
    })
    .await;
    Ok(())
}
