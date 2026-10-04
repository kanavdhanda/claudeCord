//! Hot and cold history: recent messages live in the database, older ones in immutable compressed files, one per project per day,
//! joined when a day ends up with several, and read back by paging. These tests check the files hold exactly the rows (none lost, none
//! doubled), the number of files follows the number of days, and paging back through the database and the files returns every row once.

use claudecord::store::{HistoryRow, Store};

const DAY: i64 = 86_400_000;

fn dir(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("cc-tier-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn row(project: &str, at: i64, text: &str) -> HistoryRow {
    HistoryRow {
        id: 0,
        at,
        project: project.into(),
        thread: None,
        from: "kd".into(),
        kind: "say".into(),
        text: text.into(),
    }
}

fn files(seg: &std::path::Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(seg)
        .map(|d| {
            d.filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v
}

#[test]
fn old_history_moves_as_one_file_per_project_per_day_and_every_row_is_kept_once() {
    let d = dir("days");
    let seg = d.join("seg");
    let mut s = Store::open(&d.join("hub.db"), Some(&seg)).unwrap();
    let mut rows = Vec::new();
    for day in 0..3 {
        for i in 0..10 {
            rows.push(row("alpha", day * DAY + i * 1000, &format!("a-{day}-{i}")));
            rows.push(row("beta", day * DAY + i * 1000, &format!("b-{day}-{i}")));
        }
    }
    s.append(&rows).unwrap();
    // Days 0 and 1 are old, day 2 is recent.
    let moved = s.rollover(2 * DAY).unwrap();
    assert_eq!(moved, 2 * 2 * 10, "two projects, two old days");
    assert_eq!(
        files(&seg).len(),
        4,
        "one file per project per old day: {:?}",
        files(&seg)
    );
    assert_eq!(
        s.hot_rows().unwrap(),
        20,
        "only the recent day is still in the database"
    );
    // A second rollover with nothing new to move makes no files.
    assert_eq!(s.rollover(2 * DAY).unwrap(), 0);
    assert_eq!(files(&seg).len(), 4);
    // Every row is still there exactly once, in order.
    let all = s.history_all("alpha").unwrap();
    assert_eq!(all.len(), 30);
    assert!(all.windows(2).all(|w| w[0].id < w[1].id));
    let days: Vec<i64> = s.segments("alpha").unwrap().iter().map(|g| g.day).collect();
    assert_eq!(days, [0, 1], "each file says which day it holds");
}

#[test]
fn a_huge_day_is_written_in_files_of_bounded_size_so_memory_stays_small() {
    let d = dir("chunks");
    let seg = d.join("seg");
    let mut s = Store::open(&d.join("hub.db"), Some(&seg)).unwrap();
    let rows: Vec<_> = (0..250)
        .map(|i| row("p", 5 * DAY + i, &format!("m{i}")))
        .collect();
    s.append(&rows).unwrap();
    assert_eq!(s.rollover_chunked(6 * DAY, 100).unwrap(), 250);
    let segs = s.segments("p").unwrap();
    assert_eq!(
        segs.iter().map(|g| g.rows).collect::<Vec<_>>(),
        [100, 100, 50],
        "files of at most 100 rows"
    );
    assert_eq!(s.history_all("p").unwrap().len(), 250);
}

#[test]
fn many_small_files_in_a_day_are_joined_into_one_without_losing_a_row() {
    let d = dir("compact");
    let seg = d.join("seg");
    let mut s = Store::open(&d.join("hub.db"), Some(&seg)).unwrap();
    // The hub rolls over hourly: each pass finds a few more old rows of the same day.
    for hour in 0..6 {
        let rows: Vec<_> = (0..4)
            .map(|i| row("p", 3 * DAY + hour * 3_600_000 + i, &format!("h{hour}-{i}")))
            .collect();
        s.append(&rows).unwrap();
        s.rollover(3 * DAY + (hour + 1) * 3_600_000).unwrap();
    }
    assert_eq!(files(&seg).len(), 6, "six rollovers, six small files");
    let before = s.history_all("p").unwrap();
    let removed = s.compact(100_000).unwrap();
    assert_eq!(
        removed, 6,
        "the old files are gone once the joined one is written"
    );
    assert_eq!(files(&seg).len(), 1, "{:?}", files(&seg));
    let segs = s.segments("p").unwrap();
    assert_eq!((segs.len(), segs[0].rows), (1, 24));
    assert_eq!(
        s.history_all("p").unwrap(),
        before,
        "exactly the same rows, in the same order"
    );
    // Nothing more to join: running it again changes nothing.
    assert_eq!(s.compact(100_000).unwrap(), 0);
    // A day too big to join in memory is left as it is.
    let mut big = Store::open(&d.join("big.db"), Some(&d.join("seg2"))).unwrap();
    for part in 0..2 {
        big.append(
            &(0..50)
                .map(|i| row("p", DAY + part * 1000 + i, "x"))
                .collect::<Vec<_>>(),
        )
        .unwrap();
        big.rollover(DAY + (part + 1) * 1000).unwrap();
    }
    assert_eq!(
        big.compact(60).unwrap(),
        0,
        "100 rows is over the limit of 60"
    );
}

#[test]
fn paging_back_through_the_database_and_the_files_returns_every_row_once_in_order() {
    let d = dir("paging");
    let seg = d.join("seg");
    let mut s = Store::open(&d.join("hub.db"), Some(&seg)).unwrap();
    // 1200 rows over six days, in two threads; the first four days go to files, the last two stay hot.
    let rows: Vec<_> = (0..1200)
        .map(|i| {
            let mut r = row("p", (i / 200) * DAY + i, &format!("m{i:04}"));
            r.thread = Some(if i % 3 == 0 { "t1" } else { "t2" }.into());
            r
        })
        .collect();
    s.append(&rows).unwrap();
    s.rollover(4 * DAY).unwrap();
    assert!(s.hot_rows().unwrap() == 400 && !files(&seg).is_empty());
    let page = |before: Option<i64>, thread: Option<&str>| {
        s.history_before("p", thread, before, 50).unwrap()
    };
    // The newest page, then each older page by the id of the oldest row seen.
    let mut got: Vec<String> = Vec::new();
    let mut before = None;
    loop {
        let rows = page(before, None);
        if rows.is_empty() {
            break;
        }
        assert!(
            rows.windows(2).all(|w| w[0].id < w[1].id),
            "a page is oldest first"
        );
        before = Some(rows[0].id);
        got.splice(0..0, rows.into_iter().map(|r| r.text));
    }
    let want: Vec<String> = (0..1200).map(|i| format!("m{i:04}")).collect();
    assert_eq!(
        got, want,
        "paging back saw every row once and in order, across the database and the files"
    );
    // One thread only.
    let mut t1 = Vec::new();
    let mut before = None;
    loop {
        let rows = page(before, Some("t1"));
        if rows.is_empty() {
            break;
        }
        before = Some(rows[0].id);
        t1.splice(0..0, rows.into_iter().map(|r| r.text));
    }
    assert_eq!(t1.len(), 400);
    assert!(t1.iter().all(|t| t[1..].parse::<usize>().unwrap() % 3 == 0));
    // Asking for older than everything is simply empty.
    assert!(page(Some(1), None).is_empty());
}

#[test]
fn a_hub_that_still_has_old_one_per_hour_files_keeps_working_with_them() {
    // Files made before days were recorded have day -1: they are read like any other and never joined.
    let d = dir("legacy");
    let seg = d.join("seg");
    let mut s = Store::open(&d.join("hub.db"), Some(&seg)).unwrap();
    s.append(
        &(0..5)
            .map(|i| row("p", DAY + i, &format!("old{i}")))
            .collect::<Vec<_>>(),
    )
    .unwrap();
    s.rollover(2 * DAY).unwrap();
    let name = s.segments("p").unwrap()[0].file.clone();
    drop(s);
    let conn = rusqlite_path(&d.join("hub.db"));
    conn.execute("UPDATE segments SET day = -1", []).unwrap();
    drop(conn);
    let s = Store::open(&d.join("hub.db"), Some(&seg)).unwrap();
    assert_eq!(s.segments("p").unwrap()[0].day, -1);
    assert_eq!(s.read_segment(&name).unwrap().len(), 5);
    assert_eq!(s.history_before("p", None, None, 10).unwrap().len(), 5);
}

fn rusqlite_path(path: &std::path::Path) -> rusqlite::Connection {
    rusqlite::Connection::open(path).unwrap()
}
