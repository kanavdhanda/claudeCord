//! The health check as a test: every feature must pass its probe, and every source file must be claimed by a probe, so a
//! new module cannot be added without a check that it is alive.

use claudecord::health;
use std::path::Path;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_feature_is_alive() {
    let outcomes = health::run_all().await;
    let failed: Vec<String> = outcomes
        .iter()
        .filter(|o| !o.ok)
        .map(|o| format!("{}: {}", o.name, o.detail))
        .collect();
    assert!(
        failed.is_empty(),
        "features not working:\n{}",
        failed.join("\n")
    );
    assert!(
        outcomes.len() >= 20,
        "expected a probe for every feature, found {}",
        outcomes.len()
    );
}

/// Every .rs file under src (as a path without the extension) must be covered by some probe's `covers` entry, either
/// exactly or because a covered path is a folder above it.
#[test]
fn every_source_file_is_claimed_by_a_probe() {
    let covers: Vec<&str> = health::features()
        .iter()
        .flat_map(|f| f.covers.iter().copied())
        .collect();
    let mut missing = Vec::new();
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut stack = vec![root.clone()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let p = entry.unwrap().path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().is_some_and(|e| e == "rs") {
                let rel = p.strip_prefix(&root).unwrap().with_extension("");
                let rel = rel.to_string_lossy().replace('\\', "/");
                let claimed = covers
                    .iter()
                    .any(|c| rel == *c || rel.starts_with(&format!("{c}/")));
                if !claimed {
                    missing.push(rel);
                }
            }
        }
    }
    missing.sort();
    assert!(
        missing.is_empty(),
        "these files are not covered by any health probe (add them to a feature's `covers`): {missing:?}"
    );
}
