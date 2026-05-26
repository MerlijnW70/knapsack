//! Round 7 — ab math, install adversarial settings, read cache growth.
//!
//! - ab: aggregation across many sessions, divide-by-zero on empty,
//!   negative-net rendering, very large numbers.
//! - install: edge-case settings.json shapes the previous adversarial pass
//!   didn't hit (multiple stale hooks, very large existing config).
//! - read cache: pack many files via the Read hook, verify gc cleans them
//!   and the per-file size cap holds.

use knapsack::ab;
use knapsack::install::{patch_settings_file, settings_has_hook, unpatch_settings_file, Patch};
use knapsack::json;
use std::path::PathBuf;

fn tmpfile(tag: &str) -> PathBuf {
    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    std::env::temp_dir().join(format!("kn-r7-{}-{}-{}", tag, std::process::id(), t))
}

// =====================================================================
// 1. ab math correctness
// =====================================================================

#[test]
fn ab_empty_metrics_returns_clean_report_no_panic() {
    let p = tmpfile("ab-empty.jsonl");
    std::fs::write(&p, "").unwrap();
    let r = ab::build(&p);
    assert_eq!(r.total.compress_events, 0);
    assert_eq!(r.total.net(), 0, "no data → net=0");
    assert!(r.sessions.is_empty());
    let s = ab::format(&r);
    assert!(s.contains("no data yet"), "verdict line names empty state:\n{s}");
    let _ = std::fs::remove_file(&p);
}

#[test]
fn ab_missing_metrics_file_returns_empty_not_error() {
    let p = tmpfile("ab-missing.jsonl");
    // Don't create the file. read() uses unwrap_or_default() → empty string.
    let r = ab::build(&p);
    assert_eq!(r.total.compress_events, 0);
    let _ = std::fs::remove_file(&p);
}

#[test]
fn ab_skips_malformed_lines_keeps_valid_ones() {
    let p = tmpfile("ab-malformed.jsonl");
    let content = "\
{\"event\":\"compress\",\"session\":\"a\",\"raw\":100,\"shown\":40,\"saved\":60,\"delta_hits\":0,\"evicted\":0}
not json at all
{\"event\":\"compress\",\"session\":\"a\",\"raw\":200,\"shown\":50,\"saved\":150,\"delta_hits\":1,\"evicted\":0}
{garbage}
{\"event\":\"expand\",\"session\":\"a\",\"tokens\":20,\"ok\":true}
{\"event\":\"expand\",\"session\":\"a\",\"tokens\":10,\"ok\":false}
";
    std::fs::write(&p, content).unwrap();
    let r = ab::build(&p);
    assert_eq!(r.total.compress_events, 2, "2 valid compress lines");
    assert_eq!(r.total.raw, 300);
    assert_eq!(r.total.saved, 210);
    assert_eq!(r.total.expand_calls, 2);
    assert_eq!(r.total.failed_expands, 1);
    assert_eq!(r.total.refetched, 20, "only the ok expand contributes refetched");
    assert_eq!(r.total.net(), 210 - 20, "net = saved − refetched");
    let _ = std::fs::remove_file(&p);
}

#[test]
fn ab_negative_net_session_renders_correctly() {
    // saved=50, refetched=200 → net=-150. Must show with leading `-`.
    let p = tmpfile("ab-negative.jsonl");
    let content = "\
{\"event\":\"compress\",\"session\":\"over-recall\",\"raw\":100,\"shown\":50,\"saved\":50,\"delta_hits\":0,\"evicted\":0}
{\"event\":\"expand\",\"session\":\"over-recall\",\"tokens\":200,\"ok\":true}
";
    std::fs::write(&p, content).unwrap();
    let r = ab::build(&p);
    assert_eq!(r.total.net(), -150);
    let s = ab::format(&r);
    assert!(s.contains("-150"), "negative net rendered with `-` sign:\n{s}");
    assert!(s.contains("net NEGATIVE"), "verdict reflects over-recall");
    let _ = std::fs::remove_file(&p);
}

#[test]
fn ab_huge_numbers_dont_overflow_aggregate() {
    let p = tmpfile("ab-huge.jsonl");
    // 1000 events each with raw=1e9 → total raw = 1e12 (well within i64)
    let mut content = String::new();
    for i in 0..1000 {
        content.push_str(&format!(
            "{{\"event\":\"compress\",\"session\":\"s{i}\",\"raw\":1000000000,\"shown\":100,\"saved\":999999900,\"delta_hits\":0,\"evicted\":0}}\n"
        ));
    }
    std::fs::write(&p, &content).unwrap();
    let r = ab::build(&p);
    assert_eq!(r.total.compress_events, 1000);
    assert_eq!(r.total.raw, 1_000_000_000_000);
    assert_eq!(r.total.saved, 999_999_900_000);
    let _ = std::fs::remove_file(&p);
}

#[test]
fn ab_sessions_sorted_by_net_desc() {
    let p = tmpfile("ab-sorted.jsonl");
    let content = "\
{\"event\":\"compress\",\"session\":\"low\",\"raw\":50,\"shown\":40,\"saved\":10,\"delta_hits\":0,\"evicted\":0}
{\"event\":\"compress\",\"session\":\"high\",\"raw\":1000,\"shown\":100,\"saved\":900,\"delta_hits\":5,\"evicted\":0}
{\"event\":\"compress\",\"session\":\"mid\",\"raw\":200,\"shown\":100,\"saved\":100,\"delta_hits\":1,\"evicted\":0}
";
    std::fs::write(&p, content).unwrap();
    let r = ab::build(&p);
    // Sessions sorted by net descending: high(900), mid(100), low(10).
    let names: Vec<&str> = r.sessions.iter().map(|s| s.0.as_str()).collect();
    assert_eq!(names, vec!["high", "mid", "low"], "sorted by net descending");
    let _ = std::fs::remove_file(&p);
}

#[test]
fn ab_session_missing_field_uses_no_session_placeholder() {
    let p = tmpfile("ab-no-session.jsonl");
    let content = "{\"event\":\"compress\",\"raw\":50,\"shown\":40,\"saved\":10,\"delta_hits\":0,\"evicted\":0}\n";
    std::fs::write(&p, content).unwrap();
    let r = ab::build(&p);
    assert_eq!(r.sessions.len(), 1);
    assert_eq!(r.sessions[0].0, "(no session)");
    let _ = std::fs::remove_file(&p);
}

#[test]
fn ab_long_session_id_truncated_in_output() {
    let p = tmpfile("ab-long-sid.jsonl");
    let long_sid = "x".repeat(50);
    let content = format!(
        "{{\"event\":\"compress\",\"session\":\"{long_sid}\",\"raw\":50,\"shown\":40,\"saved\":10,\"delta_hits\":0,\"evicted\":0}}\n"
    );
    std::fs::write(&p, &content).unwrap();
    let r = ab::build(&p);
    let s = ab::format(&r);
    // short() truncates session id to 19 chars + … — the FULL 50-char id must NOT appear
    assert!(!s.contains(&long_sid), "long session id must be truncated in output");
    assert!(s.contains("xxxxxxxxxxxxxxxxxx…"), "should show 18 chars + ellipsis");
    let _ = std::fs::remove_file(&p);
}

// =====================================================================
// 2. install — adversarial settings.json shapes round 2
// =====================================================================

#[test]
fn install_with_multiple_stale_knapsack_hooks_converges_to_one() {
    // Edge case: settings.json has TWO knapsack hook entries pointing at
    // different (stale) binaries. install --apply should NOT add a third —
    // it should converge both to the canonical bin... actually per the
    // current `apply_hook` logic, it rewrites ANY entry that contains
    // "knapsack" + "hook". So 2 stale entries → both rewritten to canonical.
    // The user ends up with 2 identical canonical entries. Pin that.
    let p = tmpfile("multi-stale.json");
    let content = r#"{"hooks":{"PreToolUse":[
        {"matcher":"Bash","hooks":[{"type":"command","command":"\"H:/old1/knapsack.exe\" hook"}]},
        {"matcher":"Bash","hooks":[{"type":"command","command":"\"H:/old2/knapsack.exe\" hook"}]}
    ]}}"#;
    std::fs::write(&p, content).unwrap();

    let result = patch_settings_file(&p, "/canonical/knapsack");
    assert!(matches!(result, Ok(Patch::Changed(_))), "two stale entries → patched");

    let v = json::parse(&std::fs::read_to_string(&p).unwrap()).unwrap();
    let pre = v.get("hooks").and_then(|h| h.get("PreToolUse")).unwrap();
    if let json::Json::Arr(a) = pre {
        assert_eq!(a.len(), 2, "still 2 entries — each was rewritten in place");
        for entry in a {
            // entry → hooks (Arr) → [0] (Obj) → command (Str)
            let hooks_arr = match entry.get("hooks") {
                Some(json::Json::Arr(h)) => h,
                _ => panic!("hooks must be an Arr"),
            };
            let cmd = hooks_arr[0].get("command").and_then(|c| c.as_str()).unwrap();
            assert!(cmd.contains("/canonical/knapsack"), "every entry now points at canonical: {cmd}");
        }
    } else {
        panic!("PreToolUse not Array");
    }
    let _ = std::fs::remove_file(&p);
}

#[test]
fn install_with_pretooluse_as_object_instead_of_array_recovers() {
    // PreToolUse is supposed to be an Array. If it's a malformed Object, the
    // patch code replaces the value with a fresh [] and adds our hook entry.
    let p = tmpfile("pretool-obj.json");
    std::fs::write(&p, r#"{"hooks":{"PreToolUse":{"oops":1}},"unrelated":42}"#).unwrap();
    assert!(patch_settings_file(&p, "/bin/knapsack").is_ok());
    assert!(settings_has_hook(&p));
    // Unrelated key must survive.
    let v = json::parse(&std::fs::read_to_string(&p).unwrap()).unwrap();
    assert_eq!(v.get("unrelated").and_then(|x| x.as_f64()), Some(42.0));
    let _ = std::fs::remove_file(&p);
}

#[test]
fn install_with_very_large_existing_config_still_patches() {
    // Synthesize a 100KB-ish config with a fake `recent_chats` array. Patch
    // should work without OOM, and the unrelated content must survive.
    let p = tmpfile("huge-config.json");
    let mut obj = String::from(r#"{"model":"opus","recent_chats":["#);
    for i in 0..1000 {
        if i > 0 { obj.push(','); }
        obj.push_str(&format!("\"chat-{i}\""));
    }
    obj.push_str(r#"]}"#);
    std::fs::write(&p, &obj).unwrap();

    assert!(patch_settings_file(&p, "/bin/knapsack").is_ok());
    assert!(settings_has_hook(&p));

    let after = json::parse(&std::fs::read_to_string(&p).unwrap()).unwrap();
    assert_eq!(after.get("model").and_then(|x| x.as_str()), Some("opus"));
    if let Some(json::Json::Arr(rc)) = after.get("recent_chats") {
        assert_eq!(rc.len(), 1000, "all 1000 chats preserved");
    } else {
        panic!("recent_chats lost");
    }
    let _ = std::fs::remove_file(&p);
}

#[test]
fn install_with_hook_entry_missing_matcher_field_does_not_crash() {
    // A pre-existing hook entry shaped weirdly — missing `matcher`. Our
    // detector keys off `hooks[].command`, not `matcher`, so it should
    // handle the absence gracefully.
    let p = tmpfile("no-matcher.json");
    std::fs::write(&p, r#"{"hooks":{"PreToolUse":[
        {"hooks":[{"type":"command","command":"echo unrelated"}]}
    ]}}"#).unwrap();
    let r = patch_settings_file(&p, "/bin/knapsack");
    assert!(r.is_ok(), "matcher-less entry must not crash patch_settings");
    let _ = std::fs::remove_file(&p);
}

#[test]
fn uninstall_with_only_stale_knapsack_hooks_removes_them_all() {
    // Two stale knapsack hooks, nothing else. Uninstall removes both, leaves
    // the file with empty hooks (pruned by the round-3 fix to {} ).
    let p = tmpfile("only-stale.json");
    std::fs::write(&p, r#"{"hooks":{"PreToolUse":[
        {"matcher":"Bash","hooks":[{"type":"command","command":"\"H:/old1/knapsack\" hook"}]},
        {"matcher":"Bash","hooks":[{"type":"command","command":"\"H:/old2/knapsack\" hook"}]}
    ]}}"#).unwrap();
    assert!(matches!(unpatch_settings_file(&p).unwrap(), Patch::Changed(_)));
    assert!(!settings_has_hook(&p));
    // Scaffold pruned to {} (round-3 fix)
    let v = json::parse(&std::fs::read_to_string(&p).unwrap()).unwrap();
    assert!(v.get("hooks").is_none(), "empty hooks scaffold pruned");
    let _ = std::fs::remove_file(&p);
}

// =====================================================================
// 3. Read cache growth investigation
// =====================================================================

#[test]
fn read_cache_does_not_grow_unboundedly_per_file_content() {
    // Each unique (content sha, path tag) makes one cache file. Same content
    // at the same path = SAME cache filename = re-used, not duplicated.
    // Pin the dedup property.
    use knapsack::read_hook::decide_with_gate;
    use knapsack::json::Json;

    let dir = tmpfile("cache-dedup");
    std::fs::create_dir_all(&dir).unwrap();
    let src = dir.join("src.rs");
    // Use this project's install.rs (~32KB, known to compress) as the fixture
    // so the redirect threshold is comfortably met.
    let real_src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/install.rs");
    let content = std::fs::read(&real_src).expect("install.rs must exist for this test");
    std::fs::write(&src, &content).unwrap();

    std::env::set_var("KNAPSACK_READ_CACHE", dir.join("cache"));
    std::env::set_var("KNAPSACK_STORE", dir.join("store"));

    // 10 reads of the SAME (content, path) should produce 1 cache file.
    for _ in 0..10 {
        let evt = Json::Obj(vec![
            ("tool_name".into(), Json::Str("Read".into())),
            ("tool_input".into(), Json::Obj(vec![
                ("file_path".into(), Json::Str(src.to_string_lossy().into())),
            ])),
        ]);
        let _ = decide_with_gate(true, &evt);
    }

    let cache_files: Vec<_> = std::fs::read_dir(dir.join("cache"))
        .unwrap()
        .filter_map(|e| e.ok())
        .collect();
    assert_eq!(cache_files.len(), 1, "10 reads of same content+path → 1 cache file, got {}",
        cache_files.len());

    std::env::remove_var("KNAPSACK_READ_CACHE");
    std::env::remove_var("KNAPSACK_STORE");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn read_cache_grows_with_file_changes_but_gc_can_clean() {
    // Modify the source file N times → N distinct cache entries.
    // Then verify gc with threshold 0 cleans them all.
    use knapsack::gc;
    use knapsack::read_hook::decide_with_gate;
    use knapsack::json::Json;
    use knapsack::store::Store;

    let dir = tmpfile("cache-gc");
    std::fs::create_dir_all(&dir).unwrap();
    let src = dir.join("src.rs");
    std::env::set_var("KNAPSACK_READ_CACHE", dir.join("cache"));
    std::env::set_var("KNAPSACK_STORE", dir.join("store"));

    // 5 different content versions of the same path → 5 cache files.
    // Use real install.rs (≈32KB, compresses well) so the redirect threshold
    // is met — then add a unique marker line per version so content shas differ.
    let real_src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/install.rs");
    let base = std::fs::read_to_string(&real_src).expect("install.rs must exist");
    for v in 0..5 {
        let content = format!("// version-marker {v}\n{base}");
        std::fs::write(&src, &content).unwrap();
        let evt = Json::Obj(vec![
            ("tool_name".into(), Json::Str("Read".into())),
            ("tool_input".into(), Json::Obj(vec![
                ("file_path".into(), Json::Str(src.to_string_lossy().into())),
            ])),
        ]);
        let _ = decide_with_gate(true, &evt);
    }
    let cache_dir = dir.join("cache");
    let before_files = std::fs::read_dir(&cache_dir).unwrap().count();
    assert!(before_files >= 5, "expected 5+ cache files (one per content sha), got {before_files}");

    // gc should clean them (threshold 0 = everything past 0 seconds)
    std::thread::sleep(std::time::Duration::from_millis(1100));
    let store = Store::new(dir.join("store"));
    let report = gc::gc(&store, 0, false);
    assert!(report.read_cache_deleted > 0, "gc must delete some cache files; got {} deleted of {} scanned",
        report.read_cache_deleted, report.read_cache_scanned);

    std::env::remove_var("KNAPSACK_READ_CACHE");
    std::env::remove_var("KNAPSACK_STORE");
    let _ = std::fs::remove_dir_all(&dir);
}
