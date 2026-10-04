//! Uptime: the arithmetic, the log, crash recovery, the debounce a prober uses, and the hub's own endpoints.

use claudecord::server::{self, Config};
use claudecord::store::Store;
use claudecord::uptime::{self, Change, Debounce, State::*, budget_left, outages, report};
use std::time::Duration;

const MIN: i64 = 60_000;

fn c(at: i64, state: claudecord::uptime::State) -> Change {
    Change { at, state }
}

#[test]
fn availability_is_up_time_over_known_time_and_unknown_time_is_left_out() {
    // Known from minute 10: up for 30, down for 10, up for the last 10 of a 60 minute window.
    let log = [c(10 * MIN, Up), c(40 * MIN, Down), c(50 * MIN, Up)];
    let r = report(&log, 0, 60 * MIN);
    assert_eq!(
        (r.unknown_ms, r.up_ms, r.down_ms),
        (10 * MIN, 40 * MIN, 10 * MIN)
    );
    assert_eq!(r.availability(), Some(0.8));
    assert_eq!(
        report(&[], 0, MIN).availability(),
        None,
        "nothing known is not 100%"
    );
}

#[test]
fn a_window_starts_in_the_state_the_last_earlier_change_left() {
    let log = [c(0, Up), c(100 * MIN, Down)];
    let r = report(&log, 90 * MIN, 120 * MIN);
    assert_eq!((r.up_ms, r.down_ms, r.unknown_ms), (10 * MIN, 20 * MIN, 0));
}

#[test]
fn the_error_budget_is_what_the_target_allows_minus_what_was_used() {
    let day = 24 * 60 * MIN;
    let log = [c(0, Up), c(MIN, Down), c(11 * MIN, Up)];
    let r = report(&log, 0, 30 * day);
    // 99.9% of 30 days allows 43.2 minutes; 10 were used.
    assert_eq!(budget_left(&r, 0.999) / MIN, 33);
    let spent = report(&[c(0, Down)], 0, 30 * day);
    assert!(
        budget_left(&spent, 0.999) < 0,
        "an all-down month overspends"
    );
}

#[test]
fn outages_are_listed_with_their_start_and_end() {
    let log = [c(0, Up), c(10, Down), c(20, Up), c(30, Down)];
    assert_eq!(
        outages(&log, 0, 50),
        vec![(10, 20), (30, 50)],
        "an open outage runs to now"
    );
    assert_eq!(
        outages(&[c(0, Down), c(5, Up)], 2, 9),
        vec![(2, 5)],
        "an outage already under way when the window opens"
    );
}

#[test]
fn the_log_records_only_real_changes_and_reads_back_a_window() {
    let db = Store::open_memory().unwrap();
    assert!(db.uptime_set("hub", Up, 100).unwrap());
    assert!(
        !db.uptime_set("hub", Up, 200).unwrap(),
        "no change, nothing written"
    );
    assert!(db.uptime_set("hub", Down, 300).unwrap());
    assert!(db.uptime_set("discord", Up, 150).unwrap());
    assert_eq!(db.uptime_components().unwrap(), ["discord", "hub"]);
    assert_eq!(db.uptime_last("hub").unwrap(), Some(Down));
    // A window starting at 250 still starts from the change at 100.
    assert_eq!(
        db.uptime_changes("hub", 250).unwrap(),
        vec![c(100, Up), c(300, Down)]
    );
    assert_eq!(db.uptime_changes("hub", 0).unwrap().len(), 2);
}

#[test]
fn a_crash_counts_as_down_from_the_last_heartbeat_and_a_clean_stop_from_the_stop() {
    let db = Store::open_memory().unwrap();
    db.uptime_set("hub", Up, 1_000).unwrap();
    db.uptime_set("discord", Up, 1_500).unwrap();
    db.uptime_set("external:eu", Up, 1_000).unwrap();
    db.kv_set("hub_alive_at", "5000").unwrap();
    uptime::recover(&db, 20_000);
    assert_eq!(
        db.uptime_changes("hub", 0).unwrap(),
        vec![c(1_000, Up), c(5_000, Down), c(20_000, Up)]
    );
    assert_eq!(
        db.uptime_last("discord").unwrap(),
        Some(Down),
        "the bridge died with the hub"
    );
    assert_eq!(
        db.uptime_last("external:eu").unwrap(),
        Some(Up),
        "an outside prober is not the hub's to close"
    );
    uptime::stopped(&db, 30_000);
    assert_eq!(db.uptime_last("hub").unwrap(), Some(Down));
    assert_eq!(
        db.uptime_changes("hub", 0).unwrap().last(),
        Some(&c(30_000, Down))
    );
}

#[test]
fn a_prober_needs_several_failures_in_a_row_and_dates_the_outage_from_the_first() {
    let mut d = Debounce::new(3);
    assert_eq!(d.check(true, 0), Some(c(0, Up)));
    assert_eq!(d.check(true, 30), None);
    assert_eq!(d.check(false, 60), None);
    assert_eq!(d.check(true, 90), None, "one blip is forgotten");
    assert_eq!(d.check(false, 120), None);
    assert_eq!(d.check(false, 150), None);
    assert_eq!(
        d.check(false, 180),
        Some(c(120, Down)),
        "confirmed on the third, dated from the first"
    );
    assert_eq!(d.check(false, 210), None, "already down");
    assert_eq!(d.check(true, 240), Some(c(240, Up)));
}

#[test]
fn times_are_written_in_utc_without_a_calendar_library() {
    assert_eq!(uptime::iso(0), "1970-01-01 00:00:00 UTC");
    assert_eq!(uptime::iso(1_800_000_000_000), "2027-01-15 08:00:00 UTC");
    assert_eq!(
        uptime::iso(951_782_400_000),
        "2000-02-29 00:00:00 UTC",
        "a leap day"
    );
}

fn dir(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("cc-uptime-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

async fn hub_with(dir: &std::path::Path, discord_expected: bool) -> (server::Hub, String, String) {
    let mut store = Store::open(&dir.join("hub.db"), None).unwrap();
    let token = store.create_token("web:scraper", 0).unwrap();
    let cfg = Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        discord_expected,
        ..Config::default()
    };
    let hub = server::start(cfg, claudecord::hub::HubCore::default(), store)
        .await
        .unwrap();
    let base = format!("http://{}", hub.addr);
    (hub, base, token)
}

#[tokio::test]
async fn the_hub_records_itself_answers_readiness_and_serves_metrics_to_a_scraper() {
    let d = dir("hub");
    let (hub, base, token) = hub_with(&d, false).await;
    let http = reqwest::Client::new();
    assert_eq!(
        http.get(format!("{base}/readyz"))
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    assert!(
        uptime::probe_once(&base, Duration::from_secs(2)).await,
        "a prober sees it up"
    );
    // Metrics need a dashboard token, and show the hub as up.
    assert_eq!(
        http.get(format!("{base}/metrics"))
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    let text = http
        .get(format!("{base}/metrics"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        text.contains("claudecord_component_up{component=\"hub\"} 1"),
        "{text}"
    );
    assert!(
        text.contains("claudecord_availability_ratio{component=\"hub\",window=\"1h\"} 1.0"),
        "{text}"
    );
    let up: serde_json::Value = http
        .get(format!("{base}/api/v1/uptime"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(up["components"]["hub"]["state"], "up");
    hub.shutdown().await;
    assert!(
        !uptime::probe_once(&base, Duration::from_millis(500)).await,
        "a prober sees it down once it stopped"
    );
    let log = Store::open(&d.join("hub.db"), None).unwrap();
    assert_eq!(
        log.uptime_last("hub").unwrap(),
        Some(Down),
        "a clean stop is recorded"
    );
}

#[tokio::test]
async fn readiness_fails_and_names_discord_when_it_is_part_of_the_hub_but_not_connected() {
    let (hub, base, _) = hub_with(&dir("ready"), true).await;
    let r = reqwest::get(format!("{base}/readyz")).await.unwrap();
    assert_eq!(r.status(), 503);
    let body: serde_json::Value = r.json().await.unwrap();
    assert_eq!(body["checks"]["discord"], false);
    assert_eq!(body["checks"]["core"], true);
    assert_eq!(
        reqwest::get(format!("{base}/healthz"))
            .await
            .unwrap()
            .status(),
        200,
        "the shallow check still passes"
    );
    hub.shutdown().await;
}
