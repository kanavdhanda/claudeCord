//! Checks the code against golden vectors in `testdata/conformance`: for each case, the exact output the program must give for a
//! given input (frames, redaction, terminal screens, rate limits, metrics and more). A change that alters any of them is a change to
//! the wire format or to a safety rule, and needs the vector updated on purpose.

use claudecord::adapters::{LaunchCtx, Policy, detect_limit, parse_menu};
use claudecord::codes::{format_code, normalize_code};
use claudecord::env::secret_env_names;
use claudecord::limits::{Bucket, FailureLimiter};
use claudecord::metrics::Metrics;
use claudecord::perms::{app_id_from_token, permissions_integer};
use claudecord::protocol::{AdapterId, AgentSpec, HubFrame, NodeFrame, is_slug};
use claudecord::redact::{find_secrets_in_file, looks_like_env_dump, redact};
use claudecord::text::{
    Delivery, format_deliveries, is_sensitive_path, project_slug, quote_body, safe_name,
    strip_control,
};
use serde_json::{Value, json};

/// A long run of one character is stored as {"$repeat": c, "$n": length}. Puts the real string back.
fn expand(v: Value) -> Value {
    match v {
        Value::Object(m) if m.len() == 2 && m.contains_key("$repeat") && m.contains_key("$n") => {
            let c = m["$repeat"].as_str().unwrap();
            Value::String(c.repeat(m["$n"].as_u64().unwrap() as usize))
        }
        Value::Object(m) => Value::Object(m.into_iter().map(|(k, v)| (k, expand(v))).collect()),
        Value::Array(a) => Value::Array(a.into_iter().map(expand).collect()),
        other => other,
    }
}

fn load(name: &str) -> Value {
    let path = format!(
        "{}/testdata/conformance/{name}.json",
        env!("CARGO_MANIFEST_DIR")
    );
    expand(
        serde_json::from_str(
            &std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}")),
        )
        .unwrap(),
    )
}

/// JSON has one number type but serde_json keeps integers and floats apart, so compare numbers as floats.
fn norm(v: &Value) -> Value {
    match v {
        Value::Number(n) => json!(n.as_f64().unwrap()),
        Value::Array(a) => Value::Array(a.iter().map(norm).collect()),
        Value::Object(m) => Value::Object(m.iter().map(|(k, v)| (k.clone(), norm(v))).collect()),
        other => other.clone(),
    }
}

fn s(v: &Value) -> &str {
    v.as_str().unwrap()
}

fn rows(name: &str) -> Vec<Value> {
    load(name).as_array().unwrap().clone()
}

#[test]
fn slugs() {
    for r in rows("slug") {
        assert_eq!(
            is_slug(s(&r["input"])),
            r["valid"].as_bool().unwrap(),
            "{r}"
        );
    }
}

#[test]
fn agent_specs() {
    for r in rows("agent_spec") {
        let ok =
            serde_json::from_value::<AgentSpec>(r["input"].clone()).is_ok_and(|a| a.is_valid());
        assert_eq!(ok, r["valid"].as_bool().unwrap(), "{}", r["input"]);
    }
}

#[test]
fn frames_from_a_device() {
    for r in rows("node_frames") {
        let raw = serde_json::to_string(&r["input"]).unwrap();
        let shown: String = raw.chars().take(200).collect();
        assert_eq!(
            NodeFrame::parse(&raw).is_some(),
            r["valid"].as_bool().unwrap(),
            "{shown}"
        );
    }
}

#[test]
fn frames_from_the_hub() {
    for r in rows("hub_frames") {
        let raw = serde_json::to_string(&r["input"]).unwrap();
        assert_eq!(
            HubFrame::parse(&raw).is_some(),
            r["valid"].as_bool().unwrap(),
            "{raw}"
        );
    }
}

#[test]
fn the_hub_cannot_choose_a_spawn_directory() {
    let v = load("hub_frame_spawn_strips_cwd");
    let frame = HubFrame::parse(&serde_json::to_string(&v["input"]).unwrap()).expect("valid");
    assert_eq!(serde_json::to_value(&frame).unwrap(), v["output"]);
}

#[test]
fn junk_is_dropped() {
    for r in rows("junk_frames") {
        assert_eq!(
            NodeFrame::parse(s(&r["raw"])).is_some(),
            r["node"].as_bool().unwrap(),
            "{r}"
        );
        assert_eq!(
            HubFrame::parse(s(&r["raw"])).is_some(),
            r["hub"].as_bool().unwrap(),
            "{r}"
        );
    }
}

#[test]
fn a_frame_survives_a_round_trip() {
    for r in rows("node_frames")
        .into_iter()
        .filter(|r| r["valid"].as_bool().unwrap())
    {
        let frame = NodeFrame::parse(&serde_json::to_string(&r["input"]).unwrap()).unwrap();
        let again = NodeFrame::parse(&serde_json::to_string(&frame).unwrap()).unwrap();
        assert_eq!(frame, again);
    }
}

#[test]
fn redaction() {
    for r in rows("redact") {
        let got = redact(s(&r["input"]));
        assert_eq!(got.text, s(&r["text"]), "input {}", r["input"]);
        let want: Vec<&str> = r["found"].as_array().unwrap().iter().map(s).collect();
        assert_eq!(got.found, want, "input {}", r["input"]);
        // Scrubbing twice must change nothing, or the device and the hub would mangle each other's markers.
        let twice = redact(&got.text);
        assert_eq!(twice.text, got.text);
        assert!(twice.found.is_empty());
    }
}

#[test]
fn secrets_in_files() {
    for r in rows("file_secrets") {
        let input = s(&r["input"]);
        let mut got = find_secrets_in_file(input.as_bytes());
        got.sort();
        let want: Vec<&str> = r["kinds"].as_array().unwrap().iter().map(s).collect();
        assert_eq!(got, want, "input {input}");
        assert_eq!(
            looks_like_env_dump(input),
            r["envDump"].as_bool().unwrap(),
            "input {input}"
        );
    }
}

#[test]
fn terminal_text() {
    for r in rows("strip_control") {
        assert_eq!(strip_control(s(&r["input"])), s(&r["output"]), "{r}");
    }
    for r in rows("quote_body") {
        assert_eq!(quote_body(s(&r["input"])), s(&r["output"]), "{r}");
    }
    for r in rows("format_deliveries") {
        let items: Vec<Delivery> = r["input"]
            .as_array()
            .unwrap()
            .iter()
            .map(|d| Delivery {
                from: s(&d["from"]).into(),
                text: s(&d["text"]).into(),
                thread: d.get("thread").map(|t| s(t).into()),
                msg_id: None,
            })
            .collect();
        assert_eq!(format_deliveries(&items), s(&r["output"]), "{r}");
    }
}

#[test]
fn names_and_paths() {
    for r in rows("project_slug") {
        assert_eq!(project_slug(s(&r["input"])), s(&r["output"]), "{r}");
    }
    for r in rows("safe_name") {
        assert_eq!(safe_name(s(&r["input"])), s(&r["output"]), "{r}");
    }
    for r in rows("sensitive_path") {
        assert_eq!(
            is_sensitive_path(s(&r["input"])),
            r["sensitive"].as_bool().unwrap(),
            "{r}"
        );
    }
}

#[test]
fn environment_scrubbing() {
    for r in rows("env_scrub") {
        let env: Vec<(&str, &str)> = r["env"]
            .as_object()
            .unwrap()
            .iter()
            .map(|(k, v)| (k.as_str(), s(v)))
            .collect();
        let mut got = secret_env_names(env, s(&r["adapter"]));
        got.sort();
        let want: Vec<&str> = r["unset"].as_array().unwrap().iter().map(s).collect();
        assert_eq!(got, want, "{}", r["adapter"]);
    }
}

fn adapter(name: &str) -> AdapterId {
    serde_json::from_value(json!(name)).unwrap()
}

#[test]
fn reading_screens() {
    for r in rows("adapter_detect") {
        for name in ["claude", "agy", "codex"] {
            let got = serde_json::to_value(adapter(name).detect(s(&r["screen"]))).unwrap();
            assert_eq!(got, r[name], "{name} on {}", r["screen"]);
        }
    }
    for r in rows("parse_menu") {
        assert_eq!(
            serde_json::to_value(parse_menu(s(&r["screen"]))).unwrap(),
            r["menu"],
            "{}",
            r["screen"]
        );
    }
    for r in rows("detect_limit") {
        assert_eq!(
            serde_json::to_value(detect_limit(s(&r["screen"]))).unwrap(),
            r["limit"],
            "{}",
            r["screen"]
        );
    }
}

#[test]
fn startup_dialogs() {
    for r in rows("startup_choice") {
        let claude = adapter("claude");
        let prompt = claude.detect(s(&r["screen"])).prompt;
        for name in ["claude", "agy", "codex"] {
            let got = prompt
                .as_ref()
                .and_then(|p| adapter(name).startup_choice(p));
            assert_eq!(json!(got), r[name], "{name} on {}", r["screen"]);
        }
        let keys = prompt.as_ref().map(|p| claude.select_keys(p, 1));
        assert_eq!(json!(keys), r["selectKeys"]);
    }
}

#[test]
fn launching_agents() {
    for r in rows("adapter_argv") {
        let policy = match s(&r["policy"]) {
            "autonomous" => Policy::Autonomous,
            "plan" => Policy::Plan,
            _ => Policy::Ask,
        };
        let a = adapter(s(&r["adapter"]));
        let claude = a == AdapterId::Claude;
        let ctx = LaunchCtx {
            name: "a",
            model: Some(s(&r["model"])),
            policy,
            rules: if claude { "R" } else { "" },
            mcp_config: claude.then_some("/m.json"),
        };
        assert_eq!(json!(a.argv(&ctx)), r["argv"], "{r}");
    }
}

#[test]
fn discord_permissions() {
    assert_eq!(
        permissions_integer().to_string(),
        s(&load("permissions")["integer"])
    );
    for r in rows("app_id") {
        assert_eq!(json!(app_id_from_token(s(&r["input"]))), r["appId"], "{r}");
    }
    for r in rows("pair_code_format") {
        let n = normalize_code(s(&r["input"]));
        assert_eq!(n, s(&r["normalized"]), "{r}");
        assert_eq!(format_code(&n), s(&r["formatted"]), "{r}");
    }
}

#[test]
fn rate_limiters() {
    let b = load("bucket");
    let mut bucket = Bucket::new(
        b["capacity"].as_f64().unwrap(),
        b["perSecond"].as_f64().unwrap(),
        b["start"].as_f64().unwrap(),
    );
    for st in b["steps"].as_array().unwrap() {
        assert_eq!(
            bucket.take(st["n"].as_f64().unwrap(), st["now"].as_f64().unwrap()),
            st["ok"].as_bool().unwrap(),
            "{st}"
        );
    }
    let f = load("failure_limiter");
    let mut lim = FailureLimiter::new(
        f["max"].as_u64().unwrap() as u32,
        f["windowMs"].as_f64().unwrap(),
    );
    for st in f["steps"].as_array().unwrap() {
        let (key, now) = (s(&st["key"]), st["now"].as_f64().unwrap());
        match s(&st["op"]) {
            "blocked" => assert_eq!(
                lim.blocked(key, now),
                st["result"].as_bool().unwrap(),
                "{st}"
            ),
            _ => lim.fail(key, now),
        }
    }
}

#[test]
fn metrics_match() {
    const MIN: i64 = 60_000;
    let want = load("metrics");
    let mut clock = 10_000 * MIN;
    let mut m = Metrics::default();
    for i in 0..30 {
        m.inc("msg_agent", 1.0, clock);
        if i % 3 == 0 {
            m.inc("msg_human", 2.0, clock);
        }
        clock += MIN;
    }
    for ms in [
        200.0, 400.0, 900.0, 2000.0, 3000.0, 4000.0, 8000.0, 20_000.0, 90_000.0, 500.0,
    ] {
        m.accepted(ms, clock);
    }
    m.task_finished(60_000.0, clock);
    m.task_finished(120_000.0, clock);
    m.status("a", "thinking", clock);
    clock += 30_000;
    m.status("a", "idle", clock);
    m.status("b", "executing", clock);
    clock += 10_000;

    let got = |v: &claudecord::metrics::Insights| norm(&serde_json::to_value(v).unwrap());
    assert_eq!(
        got(&m.insights(60, 15, &["msg_agent", "msg_human"], clock)),
        norm(&want["insights60x15"])
    );
    assert_eq!(
        got(&m.insights(60, 1, &["msg_agent"], clock)),
        norm(&want["insights60x1"])
    );
    let busiest: Vec<Value> = m
        .busiest(5, clock)
        .into_iter()
        .map(|(id, ms)| json!({"agentId": id, "busyMs": ms}))
        .collect();
    assert_eq!(norm(&json!(busiest)), norm(&want["busiest"]));
    assert_eq!(
        norm(&serde_json::to_value(m.to_json(clock)).unwrap()),
        norm(&want["saved"])
    );
    assert_eq!(clock.div_euclid(MIN), want["nowMinute"].as_i64().unwrap());

    // Saved data restores to the same insights.
    let mut restored = Metrics::default();
    restored.load(&serde_json::to_value(m.to_json(clock)).unwrap(), clock);
    assert_eq!(
        got(&restored.insights(60, 15, &["msg_agent", "msg_human"], clock)),
        got(&m.insights(60, 15, &["msg_agent", "msg_human"], clock))
    );
}
