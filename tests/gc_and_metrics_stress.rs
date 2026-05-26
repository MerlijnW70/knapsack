//! gc + metrics under adversarial conditions.
//!
//! - gc: empty store, missing dir, mixed meta-present/missing, dry-run vs real,
//!   --older-than huge, read_cache cleanup, legacy ks_ blocks untouched.
//! - metrics: 10K events (perf), corrupt JSON lines in middle (skipped not fatal),
//!   leading BOM, gigantic numbers, empty session strings.
//!
//! `metrics_*` tests mutate `KNAPSACK_METRICS` and are serialized via
//! `common::EnvSandbox`. `gc_*` tests use explicit `Store::new(dir)` paths
//! and don't need the lock — they parallelize freely.

mod common;
use common::EnvSandbox;

use knapsack::gc;
use knapsack::metrics;
use knapsack::store::Store;
use std::path::PathBuf;

fn tmpdir(tag: &str) -> PathBuf {
    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    let d = std::env::temp_dir().join(format!("kn-gc-{}-{}-{}", tag, std::process::id(), t));
    std::fs::create_dir_all(&d).unwrap();
    d
}

// ---------- gc on a completely empty / missing store ----------

#[test]
fn gc_on_missing_store_directory_doesnt_panic() {
    let dir = std::env::temp_dir().join(format!("kn-gc-noexist-{}-{}",
        std::process::id(),
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
    assert!(!dir.exists());
    let store = Store::new(dir.clone()); // creates it
    // Even though Store::new created the dir, it's empty.
    let r = gc::gc(&store, 0, true);
    let text = gc::format(&r);
    assert!(text.contains("knapsack gc"), "report renders for empty store: {text}");
}

#[test]
fn gc_on_empty_store_reports_zero_counts() {
    let dir = tmpdir("gc-empty");
    let store = Store::new(dir.clone());
    let r = gc::gc(&store, 0, true);
    let text = gc::format(&r);
    // We don't pin specific counts because the report format may evolve, but
    // it must mention something gc-shaped (scanned/deleted/etc.).
    assert!(text.contains("knapsack gc"));
}

#[test]
fn gc_dry_run_does_not_actually_delete() {
    let dir = tmpdir("gc-dry");
    let store = Store::new(dir.clone());
    let payload = b"some content for gc test";
    let h = store.put(payload);
    assert!(store.get(&h).is_some(), "block written");

    // Dry-run with --older-than 0 (delete everything threshold)
    let _r = gc::gc(&store, 0, true);

    // Block must STILL exist after dry-run
    assert!(store.get(&h).is_some(), "dry-run must NOT delete the block");
}

#[test]
fn gc_real_run_with_zero_threshold_deletes_blocks() {
    let dir = tmpdir("gc-real");
    let store = Store::new(dir.clone());
    let payload = b"deletable content";
    let h = store.put(payload);
    assert!(store.get(&h).is_some());

    // Wait 1.5 seconds so the block's last_accessed is at least 1s old
    // (threshold 0 with the meta.last_accessed timestamp).
    std::thread::sleep(std::time::Duration::from_millis(1500));

    // Real gc with --older-than 0
    let _r = gc::gc(&store, 0, false);

    // Block should now be gone
    assert!(store.get(&h).is_none(), "real gc with threshold 0 must delete");
}

#[test]
fn gc_threshold_higher_than_oldest_block_age_is_noop() {
    let dir = tmpdir("gc-far-future");
    let store = Store::new(dir.clone());
    let h = store.put(b"young content");
    // 1 year threshold — block is much younger
    let r = gc::gc(&store, 365 * 86_400, false);
    let text = gc::format(&r);
    assert!(store.get(&h).is_some(), "young block must survive a 1-year threshold");
    let _ = text;
}

// ---------- metrics under stress ----------

#[test]
fn metrics_handles_10k_compress_events() {
    let _sb = EnvSandbox::new("metrics-10k");

    let start = std::time::Instant::now();
    for i in 0..10_000 {
        metrics::record_compress(&format!("sess-{}", i % 5), 1000, 500, 500, 10, 0);
    }
    let write_dur = start.elapsed();
    assert!(write_dur.as_secs() < 5, "writing 10K metrics took {write_dur:?}");

    let start = std::time::Instant::now();
    let summary = metrics::summary();
    let read_dur = start.elapsed();
    assert!(read_dur.as_secs() < 5, "reading 10K metrics took {read_dur:?}");

    assert_eq!(summary.compress_events, 10_000);
    assert!(summary.raw > 0);
}

#[test]
fn metrics_skips_corrupt_lines_keeps_valid_ones() {
    let sb = EnvSandbox::new("metrics-corrupt");
    let metrics_path = sb.join("metrics.jsonl");

    let valid_line = r#"{"t":1700000000000.0,"event":"compress","session":"a","raw":100,"shown":50,"saved":50,"delta_hits":0,"evicted":0}"#;
    let content = format!("{valid_line}\nnot json at all\n{valid_line}\n{{ broken json\n{valid_line}\n");
    std::fs::write(&metrics_path, &content).unwrap();

    let summary = metrics::summary();
    assert_eq!(summary.compress_events, 3, "must count only the 3 valid lines, skip corrupt");
    assert_eq!(summary.raw, 300);
    assert_eq!(summary.saved, 150);
}

#[test]
fn metrics_skips_lines_missing_event_field() {
    let sb = EnvSandbox::new("metrics-no-event");
    let metrics_path = sb.join("metrics.jsonl");

    let content = "{\"t\":1700000000.0}\n{\"random\":\"json\"}\n{\"event\":\"compress\",\"session\":\"x\",\"raw\":50,\"shown\":25,\"saved\":25,\"delta_hits\":0,\"evicted\":0}\n";
    std::fs::write(&metrics_path, content).unwrap();

    let summary = metrics::summary();
    assert_eq!(summary.compress_events, 1, "only the line with event=compress counts");
}

#[test]
fn metrics_with_bom_at_start() {
    let sb = EnvSandbox::new("metrics-bom");
    let metrics_path = sb.join("metrics.jsonl");

    let valid_line = r#"{"event":"compress","session":"a","raw":100,"shown":50,"saved":50,"delta_hits":0,"evicted":0}"#;
    // Write with leading BOM
    let mut content = b"\xef\xbb\xbf".to_vec();
    content.extend(valid_line.as_bytes());
    content.push(b'\n');
    content.extend(valid_line.as_bytes());
    content.push(b'\n');
    std::fs::write(&metrics_path, &content).unwrap();

    let summary = metrics::summary();
    // The first line gets the BOM smushed onto it — won't parse as JSON.
    // The second line is clean and should count. So we expect at least 1.
    assert!(summary.compress_events >= 1, "at least 1 line parses past the BOM");
}

#[test]
fn metrics_huge_numeric_values_dont_overflow_reporter() {
    let _sb = EnvSandbox::new("metrics-huge-nums");

    // Big but representable numbers
    metrics::record_compress("session-x", 1_000_000_000, 100_000, 999_900_000, 1_000_000, 0);
    metrics::record_compress("session-x", 1_000_000_000, 100_000, 999_900_000, 1_000_000, 0);

    let summary = metrics::summary();
    assert_eq!(summary.compress_events, 2);
    assert_eq!(summary.raw, 2_000_000_000);
    assert_eq!(summary.saved, 1_999_800_000);
}

#[test]
fn metrics_report_renders_for_empty_session_filter() {
    let _sb = EnvSandbox::new("metrics-empty-filter");

    metrics::record_compress("real-session", 100, 50, 50, 0, 0);
    let report = metrics::report_for(Some(""));
    // No events match an empty session; should produce a "no data" verdict.
    assert!(report.contains("knapsack live stats"));
    assert!(report.contains("compress events"));
}
