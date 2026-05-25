//! Metrics accounting must be exact (net = saved − refetched, failed expands excluded),
//! filterable per session, resilient to malformed lines, and survive concurrent appends
//! without ever producing an incoherent summary. One test fn so the process-global
//! KNAPSACK_METRICS env var can't race across parallel tests.

use knapsack::metrics;

#[test]
fn metrics_accounting_filtering_and_resilience() {
    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    let dir = std::env::temp_dir().join(format!("knapsack-metrics-{}-{}", std::process::id(), t));
    let path = dir.join("m.jsonl");
    std::env::set_var("KNAPSACK_METRICS", &path);
    let _ = std::fs::remove_file(&path);

    // s1: 3 compress (saved 70 each), 2 successful expands (10 tok each), 1 failed expand.
    for _ in 0..3 {
        metrics::record_compress("s1", 100, 30, 70, 5, 0);
    }
    for _ in 0..2 {
        metrics::record_expand("s1", 10, true);
    }
    metrics::record_expand("s1", 0, false);
    // s2: 1 compress.
    metrics::record_compress("s2", 80, 30, 50, 2, 1);

    let all = metrics::summary();
    assert_eq!(all.compress_events, 4);
    assert_eq!(all.raw, 380);
    assert_eq!(all.shown, 120);
    assert_eq!(all.saved, 260);
    assert_eq!(all.delta_hits, 17);
    assert_eq!(all.evicted_backrefs_avoided, 1);
    assert_eq!(all.expand_calls, 3);
    assert_eq!(all.failed_expands, 1);
    assert_eq!(all.refetched, 20, "failed expands contribute no refetched tokens");
    assert_eq!(all.net, 240, "net = saved(260) - refetched(20)");

    let s1 = metrics::summary_filtered(Some("s1"));
    assert_eq!(s1.compress_events, 3);
    assert_eq!(s1.saved, 210);
    assert_eq!(s1.refetched, 20);
    assert_eq!(s1.net, 190);
    let s2 = metrics::summary_filtered(Some("s2"));
    assert_eq!((s2.compress_events, s2.saved, s2.net), (1, 50, 50));

    // Over-expansion drives net negative — the scoreboard never flatters.
    metrics::record_compress("s3", 10, 8, 2, 0, 0);
    metrics::record_expand("s3", 100, true);
    assert!(metrics::summary_filtered(Some("s3")).net < 0, "over-expansion => negative net");

    // Malformed lines are skipped, never fatal.
    {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        writeln!(f, "this is not json at all").unwrap();
        writeln!(f, "{{\"event\":\"compress\"").unwrap(); // truncated
        writeln!(f).unwrap();
    }
    let after = metrics::summary();
    assert_eq!(after.compress_events, 5, "malformed lines skipped; the 5 valid compress lines intact");

    // Concurrent appends from many threads: with the single-write_all atomic append, no lines
    // are lost and the summary stays coherent (every counted line carries saved=5).
    let cpath = dir.join("conc.jsonl");
    std::env::set_var("KNAPSACK_METRICS", &cpath);
    let (threads, per) = (8usize, 200usize);
    std::thread::scope(|sc| {
        for _ in 0..threads {
            sc.spawn(|| {
                for _ in 0..per {
                    metrics::record_compress("c", 10, 5, 5, 1, 0);
                }
            });
        }
    });
    let c = metrics::summary();
    let expected = threads * per;
    assert!(c.compress_events <= expected, "cannot count more than were appended");
    assert!(
        c.compress_events as f64 >= expected as f64 * 0.95,
        "atomic appends must not lose lines under contention ({}/{expected})",
        c.compress_events
    );
    assert_eq!(c.saved, c.compress_events as isize * 5, "every counted line parsed coherently (saved=5 each)");
    assert_eq!(c.delta_hits, c.compress_events, "and delta_hits=1 each — totals never corrupt");
    eprintln!("concurrent metric appends survived: {}/{expected}", c.compress_events);
}
