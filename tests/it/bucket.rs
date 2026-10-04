//! The bucket client: the signature is checked against the worked examples AWS publishes (so it is right for every
//! S3-compatible service), and the upload, download and delete path is checked against a stand-in bucket server.

use claudecord::store::bucket::{Bucket, authorization, encode_path, stamp};

const KEY: &str = "AKIAIOSFODNN7EXAMPLE";
const SECRET: &str = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY";
const EMPTY: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

#[test]
fn the_signature_matches_the_published_aws_examples() {
    // "GET Object" with a Range header, from AWS's Signature Version 4 documentation.
    let get = authorization(
        "GET",
        "examplebucket.s3.amazonaws.com",
        "/test.txt",
        &[("range", "bytes=0-9")],
        EMPTY,
        KEY,
        SECRET,
        "us-east-1",
        "20130524T000000Z",
    );
    assert!(
        get.ends_with("Signature=f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41"),
        "{get}"
    );
    assert!(get.contains("SignedHeaders=host;range;x-amz-content-sha256;x-amz-date"));
    assert!(get.contains("Credential=AKIAIOSFODNN7EXAMPLE/20130524/us-east-1/s3/aws4_request"));
    // "PUT Object" of the text "Welcome to Amazon S3.", with storage class and date headers.
    let put = authorization(
        "PUT",
        "examplebucket.s3.amazonaws.com",
        "/test%24file.text",
        &[
            ("date", "Fri, 24 May 2013 00:00:00 GMT"),
            ("x-amz-storage-class", "REDUCED_REDUNDANCY"),
        ],
        "44ce7dd67c959e0d3524ffac1771dfbba87d2b6b4b4e99e42034a8b803f8b072",
        KEY,
        SECRET,
        "us-east-1",
        "20130524T000000Z",
    );
    assert!(
        put.ends_with("Signature=98ad721746da40c64f1a55b78f14c238d841ea1380cd77a1b5971af0ece108bd"),
        "{put}"
    );
}

#[test]
fn paths_are_encoded_the_way_the_signature_needs() {
    assert_eq!(
        encode_path("/b/p-1/a b$c.jsonl.gz"),
        "/b/p-1/a%20b%24c.jsonl.gz"
    );
    assert_eq!(encode_path("/b/ü"), "/b/%C3%BC");
}

#[test]
fn the_timestamp_is_formatted_in_utc_without_a_calendar_library() {
    assert_eq!(stamp(0), "19700101T000000Z");
    assert_eq!(stamp(1_369_353_600), "20130524T000000Z");
    assert_eq!(stamp(1_709_251_199), "20240229T235959Z", "a leap day");
}

#[test]
fn presets_build_the_right_addresses() {
    let o = Bucket::oracle("mynamespace", "us-ashburn-1", "hist", "k", "s");
    assert_eq!(
        o.endpoint,
        "https://mynamespace.compat.objectstorage.us-ashburn-1.oraclecloud.com"
    );
    assert_eq!(o.region, "us-ashburn-1");
    let r = Bucket::r2("acct123", "hist", "k", "s");
    assert_eq!(r.endpoint, "https://acct123.r2.cloudflarestorage.com");
    assert_eq!(r.region, "auto");
}

// A stand-in bucket server, to test uploads, downloads and moving history between buckets without a real account.

use axum::{
    Router,
    body::Bytes,
    extract::{Path as AxPath, State},
    http::{HeaderMap, Method, StatusCode},
    routing::any,
};
use claudecord::store::{HistoryRow, Store};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

type Objects = Arc<Mutex<HashMap<String, Vec<u8>>>>;

/// Starts a fake bucket service that checks every request's signature the way a real one does, and keeps objects in memory.
async fn fake_bucket(secret: &'static str) -> (String, Objects) {
    let objects: Objects = Arc::new(Mutex::new(HashMap::new()));
    async fn handle(
        State((objects, secret)): State<(Objects, &'static str)>,
        method: Method,
        AxPath(path): AxPath<String>,
        headers: HeaderMap,
        body: Bytes,
    ) -> (StatusCode, Vec<u8>) {
        use sha2::{Digest, Sha256};
        let h = |n: &str| {
            headers
                .get(n)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_string()
        };
        let payload: String = Sha256::digest(&body)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        if h("x-amz-content-sha256") != payload {
            return (StatusCode::BAD_REQUEST, b"payload hash mismatch".to_vec());
        }
        let full = format!("/{}", path);
        let expected = authorization(
            method.as_str(),
            &h("host"),
            &encode_path(&full),
            &[],
            &payload,
            "KEYID",
            secret,
            "auto",
            &h("x-amz-date"),
        );
        if h("authorization") != expected {
            return (StatusCode::FORBIDDEN, b"signature mismatch".to_vec());
        }
        let mut o = objects.lock().unwrap();
        match method {
            Method::PUT => {
                o.insert(path, body.to_vec());
                (StatusCode::OK, vec![])
            }
            Method::GET => o.get(&path).map_or((StatusCode::NOT_FOUND, vec![]), |b| {
                (StatusCode::OK, b.clone())
            }),
            Method::DELETE => {
                o.remove(&path);
                (StatusCode::NO_CONTENT, vec![])
            }
            _ => (StatusCode::METHOD_NOT_ALLOWED, vec![]),
        }
    }
    let app = Router::new()
        .route("/{*path}", any(handle))
        .with_state((objects.clone(), secret));
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    (format!("http://{addr}"), objects)
}

fn bucket(endpoint: &str, secret: &str) -> Bucket {
    Bucket {
        endpoint: endpoint.into(),
        region: "auto".into(),
        bucket: "hist".into(),
        key_id: "KEYID".into(),
        secret: secret.into(),
        prefix: "t1/".into(),
    }
}

fn row(at: i64, text: &str) -> HistoryRow {
    HistoryRow {
        id: 0,
        at,
        project: "p".into(),
        thread: None,
        from: "kd".into(),
        kind: "human".into(),
        text: text.into(),
    }
}

fn tmp(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("cc-bkt-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

#[tokio::test(flavor = "multi_thread")]
async fn objects_can_be_put_fetched_and_deleted_and_a_wrong_secret_is_refused() {
    let (endpoint, objects) = fake_bucket("sekrit").await;
    let b = bucket(&endpoint, "sekrit");
    let bad = bucket(&endpoint, "wrong");
    tokio::task::spawn_blocking(move || {
        b.put("a b.txt", b"hello").unwrap();
        assert_eq!(b.get("a b.txt").unwrap(), b"hello");
        assert!(bad.put("x", b"nope").is_err(), "a wrong secret is refused");
        b.delete("a b.txt").unwrap();
        assert!(b.get("a b.txt").is_err(), "gone after delete");
        b.delete("a b.txt").unwrap();
    })
    .await
    .unwrap();
    assert!(objects.lock().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn rolled_over_history_leaves_local_disk_for_the_bucket_and_comes_back_when_read() {
    let (endpoint, objects) = fake_bucket("sekrit").await;
    let dir = tmp("roll");
    tokio::task::spawn_blocking(move || {
        let mut s = Store::open(&dir.join("t.db"), Some(&dir.join("seg"))).unwrap();
        s.set_bucket(Some(bucket(&endpoint, "sekrit")));
        let rows: Vec<HistoryRow> = (0..50)
            .map(|i| row(i, &format!("line {i}")))
            .chain([row(1000, "recent")])
            .collect();
        s.append(&rows).unwrap();
        assert_eq!(s.rollover(500).unwrap(), 50);
        let seg = &s.segments("p").unwrap()[0];
        assert!(
            !dir.join("seg").join(&seg.file).exists(),
            "the file left the local disk"
        );
        assert!(
            objects
                .lock()
                .unwrap()
                .keys()
                .any(|k| k.contains(&seg.file)),
            "and is in the bucket"
        );
        let back = s.read_segment(&seg.file).unwrap();
        assert_eq!(back.len(), 50);
        assert_eq!(back[3].text, "line 3");
        assert_eq!(s.hot_rows().unwrap(), 1);
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failed_upload_loses_nothing() {
    let dir = tmp("down");
    tokio::task::spawn_blocking(move || {
        let mut s = Store::open(&dir.join("t.db"), Some(&dir.join("seg"))).unwrap();
        // Nothing is listening on this address, as when the network or the bucket is down.
        s.set_bucket(Some(bucket("http://127.0.0.1:1", "x")));
        s.append(&[row(1, "old one"), row(2, "old two")]).unwrap();
        assert!(s.rollover(500).is_err(), "the failure is reported");
        assert_eq!(
            s.hot_rows().unwrap(),
            2,
            "the history is still in the database"
        );
        assert!(
            s.segments("p").unwrap().is_empty(),
            "no segment was recorded for a file that never arrived"
        );
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn history_can_move_from_one_bucket_to_another_for_example_oracle_to_r2() {
    let (e1, _) = fake_bucket("one").await;
    let (e2, second) = fake_bucket("two").await;
    let dir = tmp("migrate");
    tokio::task::spawn_blocking(move || {
        let (from, to) = (bucket(&e1, "one"), bucket(&e2, "two"));
        let mut s = Store::open(&dir.join("t.db"), Some(&dir.join("seg"))).unwrap();
        s.set_bucket(Some(from.clone()));
        s.append(&[row(1, "a"), row(2, "b")]).unwrap();
        s.rollover(500).unwrap();
        assert_eq!(s.migrate_segments(&from, &to).unwrap(), 1);
        assert_eq!(
            second.lock().unwrap().len(),
            1,
            "the file is now in the second bucket"
        );
        // Switch over and read from the new place.
        s.set_bucket(Some(to));
        let file = s.segments("p").unwrap()[0].file.clone();
        assert_eq!(s.read_segment(&file).unwrap().len(), 2);
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn the_live_database_can_be_backed_up_and_brought_back_whole() {
    let (endpoint, objects) = fake_bucket("sekrit").await;
    let dir = tmp("backup");
    tokio::task::spawn_blocking(move || {
        let b = bucket(&endpoint, "sekrit");
        let mut s = Store::open(&dir.join("hub.db"), Some(&dir.join("seg"))).unwrap();
        s.set_bucket(Some(b.clone()));
        s.append(&[row(1, "one"), row(2, "two")]).unwrap();
        s.save_snapshot("{\"state\":1}", 5).unwrap();
        let token = s.create_token("mac", 0).unwrap();
        let key = s.backup_to_bucket(86_400 * 3).unwrap();
        assert_eq!(
            key, "backup/hub-3.db.gz",
            "one of seven rotating daily copies"
        );
        assert!(
            objects
                .lock()
                .unwrap()
                .keys()
                .any(|k| k.ends_with("backup/hub-latest.db.gz")),
            "and the newest copy"
        );
        // The disk is lost. Restore into a fresh place and check everything is there.
        let fresh = dir.join("fresh.db");
        Store::restore_from_bucket(&b, "backup/hub-latest.db.gz", &fresh, false).unwrap();
        let r = Store::open(&fresh, None).unwrap();
        assert_eq!(
            r.history("p", None, 0, 10).unwrap().len(),
            2,
            "history is back"
        );
        assert_eq!(
            r.load_snapshot().unwrap().as_deref(),
            Some("{\"state\":1}"),
            "saved state is back"
        );
        assert_eq!(
            r.node_for_token(&token).unwrap().as_deref(),
            Some("mac"),
            "devices can still sign in"
        );
        // Windows will not replace a file that is still open, so the restored database is closed first (stop the hub before restoring).
        drop(r);
        // A database that is already there is never replaced by accident.
        assert!(Store::restore_from_bucket(&b, "backup/hub-latest.db.gz", &fresh, false).is_err());
        assert!(Store::restore_from_bucket(&b, "backup/hub-latest.db.gz", &fresh, true).is_ok());
        // Something that is not a database is refused before anything is replaced.
        b.put("backup/bad.db.gz", &{
            use std::io::Write;
            let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
            e.write_all(b"not a database at all, just text").unwrap();
            e.finish().unwrap()
        })
        .unwrap();
        assert!(
            Store::restore_from_bucket(&b, "backup/bad.db.gz", &dir.join("other.db"), false)
                .is_err()
        );
        assert!(!dir.join("other.db").exists());
    })
    .await
    .unwrap();
}
