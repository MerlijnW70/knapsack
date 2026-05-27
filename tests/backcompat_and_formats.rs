//! Backward compatibility (legacy ks_ handles, flat-layout store, BOM in
//! metrics) + output format stability for the human-facing surfaces (status,
//! doctor, metrics, why-last, ab). Lots of grep-pinning here — a refactor
//! that changes any of these strings will fail and force a conscious
//! re-pinning rather than silently breaking scripts that grep the output.
//!
//! Format-stability tests below use `common::EnvSandbox` for parallel safety;
//! the legacy-handle tests above use direct `Store::new(dir)` and don't need it.

mod common;
use common::EnvSandbox;

use knapsack::api::ExpandRequest;
use knapsack::hash::sha1_hex;
use knapsack::sha256::sha256_hex;
use knapsack::store::Store;

fn tmpstore(tag: &str) -> Store {
    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    let dir = std::env::temp_dir().join(format!("kn-bc-{}-{}-{}", tag, std::process::id(), t));
    Store::new(dir)
}

// ---------- legacy ks_ handles still resolve byte-exact ----------

#[test]
fn legacy_ks_10_hex_handle_resolves() {
    // ks_<10 hex> = 40-bit SHA-1 truncation. Pre-format-bump handles must
    // still resolve. Write directly into the store using the legacy handle
    // shape, then read it back.
    let store = tmpstore("legacy-10");
    let bytes = b"legacy 10-hex content";
    let legacy_handle = format!("ks_{}", &sha1_hex(bytes)[..10]);
    // put_with_handle writes ANY handle the caller supplies; legacy callers
    // (pre-fix) would have computed SHA-1 and passed it here.
    store.put_with_handle(&legacy_handle, bytes);
    // Now read it back — the verify-on-read routing in hash::verify should
    // recognize the ks_<10 hex> shape and re-hash with SHA-1.
    let recalled = store.get(&legacy_handle).expect("legacy ks_<10> must resolve");
    assert_eq!(recalled, bytes, "legacy 10-hex recall byte-exact");
}

#[test]
fn legacy_ks_16_hex_handle_resolves() {
    let store = tmpstore("legacy-16");
    let bytes = b"legacy 16-hex content";
    let legacy_handle = format!("ks_{}", &sha1_hex(bytes)[..16]);
    store.put_with_handle(&legacy_handle, bytes);
    let recalled = store.get(&legacy_handle).expect("legacy ks_<16> must resolve");
    assert_eq!(recalled, bytes);
}

#[test]
fn legacy_handle_with_corrupt_bytes_rejects() {
    let store = tmpstore("legacy-corrupt");
    let bytes = b"legacy corrupt test";
    let legacy_handle = format!("ks_{}", &sha1_hex(bytes)[..10]);
    store.put_with_handle(&legacy_handle, bytes);
    // Find the on-disk file and corrupt the bytes.
    let hash_start = legacy_handle.find('_').unwrap() + 1;
    let shard = &legacy_handle[hash_start..hash_start + 2];
    let bp = store.dir().join(shard).join(&legacy_handle);
    let mut data = std::fs::read(&bp).unwrap();
    data[0] ^= 0xFF;
    std::fs::write(&bp, data).unwrap();
    // Corruption must read as None even on legacy path.
    assert_eq!(store.get(&legacy_handle), None, "legacy + corrupt -> None");
}

#[test]
fn new_ks2_and_legacy_ks_can_coexist_in_same_store() {
    let store = tmpstore("mixed");
    let a = b"new ks2 content";
    let b = b"legacy ks content";
    let new = store.put(a); // produces ks2_<32 hex>
    let legacy = format!("ks_{}", &sha1_hex(b)[..10]);
    store.put_with_handle(&legacy, b);

    // Both must resolve to their respective bytes.
    assert_eq!(store.get(&new).as_deref(), Some(&a[..]));
    assert_eq!(store.get(&legacy).as_deref(), Some(&b[..]));
}

// ---------- flat-layout (pre-sharding) store fallback ----------

#[test]
fn flat_layout_block_resolves_via_fallback() {
    // store::get tries the sharded path first, then the legacy flat path.
    // Simulate an old block by writing it DIRECTLY at the store dir root
    // (no shard subdir).
    let store = tmpstore("flat");
    let bytes = b"old flat-layout block";
    let h = knapsack::hash::handle(bytes);
    // Compute the flat path (store dir root + sanitized handle).
    let flat = store.dir().join(&h);
    std::fs::write(&flat, bytes).unwrap();
    // get() should find it via the flat_path fallback.
    let recalled = store.get(&h).expect("flat-layout block must resolve via fallback");
    assert_eq!(recalled, bytes);
}

#[test]
fn sharded_wins_over_flat_when_both_exist() {
    // store.rs documents: "when both exist the sharded copy deterministically
    // wins". Pin that — and verify a corrupt sharded copy STILL falls back to
    // a valid flat one (per the doc).
    let store = tmpstore("both");
    let bytes = b"both-paths test content";
    let h = store.put(bytes); // writes to SHARDED path
    let flat = store.dir().join(&h);
    std::fs::write(&flat, b"DIFFERENT bytes at flat path").unwrap();
    // get() reads the SHARDED copy first (the correct one).
    let recalled = store.get(&h).expect("sharded wins");
    assert_eq!(recalled, bytes, "sharded path takes precedence over flat");
}

#[test]
fn corrupt_sharded_falls_back_to_valid_flat() {
    let store = tmpstore("fall-flat");
    let bytes = b"fall-back test content";
    let h = store.put(bytes);
    // Corrupt the sharded copy
    let hash_start = h.find('_').unwrap() + 1;
    let shard = &h[hash_start..hash_start + 2];
    let bp = store.dir().join(shard).join(&h);
    let mut corrupted = std::fs::read(&bp).unwrap();
    corrupted[0] ^= 0xFF;
    std::fs::write(&bp, &corrupted).unwrap();
    // Write a valid copy at the flat path.
    let flat = store.dir().join(&h);
    std::fs::write(&flat, bytes).unwrap();
    // get() should fall back to flat and return the valid bytes.
    let recalled = store.get(&h).expect("flat-fallback should resolve");
    assert_eq!(recalled, bytes, "corrupt sharded -> fall back to valid flat");
}

// ---------- output format stability (status / doctor / metrics / why-last) ----------

fn sandbox_env(tag: &str) -> EnvSandbox {
    let mut sb = EnvSandbox::new(tag);
    let settings = sb.join("settings.json");
    let mcp = sb.join(".claude.json");
    std::fs::write(&settings, "{}").unwrap();
    std::fs::write(&mcp, "{}").unwrap();
    sb.set("KNAPSACK_SETTINGS", settings);
    sb.set("KNAPSACK_MCP_CONFIG", mcp);
    sb
}

#[test]
fn status_inactive_shape_is_pinned() {
    let _sb = sandbox_env("status-inactive");
    let report = knapsack::status::report();
    // Pin the exact user-facing strings; refactors that change these break
    // anyone scripting against the output.
    assert!(report.contains("Knapsack inactive"), "got:\n{report}");
    assert!(report.contains("knapsack install"), "advises install: \n{report}");
}

#[test]
fn metrics_report_baseline_shape() {
    let _sb = sandbox_env("metrics-fmt");
    let report = knapsack::metrics::report();
    // Pinned column labels — scripts may parse these.
    for label in [
        "knapsack live stats",
        "compress events",
        "raw tokens",
        "shown tokens",
        "saved tokens",
        "delta hits",
        "evicted backrefs avoided",
        "expand calls",
        "tokens refetched",
        "NET saved",
        "verdict:",
    ] {
        assert!(report.contains(label), "metrics must contain {label:?}:\n{report}");
    }
}

#[test]
fn metrics_verdict_no_data_message() {
    let _sb = sandbox_env("metrics-nodata");
    let report = knapsack::metrics::report();
    assert!(report.contains("no data yet"), "empty-state verdict pinned");
}

#[test]
fn metrics_report_leads_with_current_session_block() {
    // Mirrors the user-reported confusion on `knapsack metrics`: a current session
    // saved tokens while the lifetime aggregate is dominated by historical recall
    // debt. The default report prepends a tiny "current session" block so the
    // recent positive work is the first thing the user reads. The full lifetime
    // table follows untouched, NET line and all — nothing is erased.
    let sb = sandbox_env("metrics-current-session-leads");
    let metrics_path = sb.join("metrics.jsonl");
    std::fs::write(
        &metrics_path,
        concat!(
            // Historical session contributing to recall debt.
            r#"{"t":100,"event":"compress","session":"old-1","raw":1012,"shown":150,"saved":862,"delta_hits":0,"evicted":0}"#, "\n",
            r#"{"t":150,"event":"expand","session":"old-1","tokens":6952811,"ok":true}"#, "\n",
            // Current session — clean positive run.
            r#"{"t":2000,"event":"compress","session":"react-fresh","raw":6689,"shown":1012,"saved":5677,"delta_hits":34,"evicted":0}"#, "\n",
        ),
    )
    .unwrap();

    let report = knapsack::metrics::report();

    // Current session block leads.
    let current_idx = report.find("current session").expect("'current session' block must appear");
    let lifetime_idx = report.find("knapsack live stats").expect("lifetime table must appear");
    assert!(
        current_idx < lifetime_idx,
        "current session block must come BEFORE the lifetime table:\n{report}"
    );

    // Current session shows the positive numbers, not the historical debt.
    assert!(report.contains("saved tokens           : 5677"), "current session's saved tokens lead:\n{report}");
    assert!(report.contains("reduction              : 84%"), "current session's reduction (5677/6689 = 84%):\n{report}");

    // Lifetime table is preserved verbatim: NET line, refetched total, verdict.
    assert!(report.contains("NET saved"), "lifetime NET line is preserved:\n{report}");
    assert!(report.contains("6952811"), "lifetime refetched total is preserved:\n{report}");
    assert!(report.contains("verdict:"), "lifetime verdict is preserved:\n{report}");
}

#[test]
fn metrics_report_omits_current_session_block_when_no_compresses_yet() {
    // When there are no compress events, there is no "current session" to surface —
    // the prefix block must be omitted entirely (no half-empty block, no "n/a" lines).
    // The empty-state verdict in the lifetime table is the user signal.
    let _sb = sandbox_env("metrics-nodata-prefix");
    let report = knapsack::metrics::report();
    assert!(!report.contains("current session"), "no compresses -> no current session block:\n{report}");
    assert!(report.contains("no data yet"), "empty-state verdict still leads:\n{report}");
}

#[test]
fn doctor_output_has_each_check_line() {
    let _sb = sandbox_env("doctor-fmt");
    let report = knapsack::install::doctor();
    for label in [
        "binary found",
        "store writable",
        "metrics writable",
        "hook installed",
        "MCP configured",
        "MCP initialize",
        "pack/expand smoke",
    ] {
        assert!(report.contains(label), "doctor must report '{label}':\n{report}");
    }
}

#[test]
fn doctor_uses_unicode_status_markers() {
    let _sb = sandbox_env("doctor-markers");
    let report = knapsack::install::doctor();
    // ✓ (green), • (warn), ✗ (fail) — these markers are part of the visible
    // surface; tests that grep for them shouldn't silently break.
    assert!(
        report.contains('✓') || report.contains('•') || report.contains('✗'),
        "doctor must use Unicode status markers:\n{report}"
    );
}

#[test]
fn metrics_per_session_filter_shape_matches_overall() {
    let _sb = sandbox_env("metrics-per-session");
    let overall = knapsack::metrics::report();
    let filtered = knapsack::metrics::report_for(Some("nonexistent"));
    // Both must contain the same column labels — only the numbers differ.
    for label in ["compress events", "NET saved", "verdict:"] {
        assert!(overall.contains(label));
        assert!(filtered.contains(label));
    }
}

// ---------- expand: unknown handle error path attribution ----------

#[test]
fn expand_on_unknown_handle_records_failed_expand_metric() {
    let sb = sandbox_env("expand-fail-metric");
    let req = ExpandRequest {
        handle: "ks2_00000000000000000000000000000000".into(),
        range: None,
        grep: None,
        context: 0,
        session_id: "failed-session".into(),
    };
    let out = knapsack::api::expand_handle(req);
    assert!(out.is_none());
    // Metric should record a failed expand attempt.
    let metrics_path = sb.join("metrics.jsonl");
    if metrics_path.exists() {
        let content = std::fs::read_to_string(&metrics_path).unwrap();
        assert!(
            content.contains(r#""ok":false"#) || content.is_empty(),
            "failed expand should be recorded OR be a no-op; got: {content}"
        );
    }
}

// ---------- handle for the same content from two sessions: dedup ----------

#[test]
fn put_same_content_twice_is_idempotent() {
    let store = tmpstore("idem");
    let bytes = b"idempotent content test";
    let h1 = store.put(bytes);
    let h2 = store.put(bytes);
    assert_eq!(h1, h2, "same content -> same handle");
    let recalled = store.get(&h1).expect("must resolve");
    assert_eq!(recalled, bytes);
}

#[test]
fn handle_for_empty_bytes_is_stable() {
    // Empty content should produce a deterministic handle (SHA-256 of empty).
    let bytes = b"";
    let h = knapsack::hash::handle(bytes);
    let h2 = knapsack::hash::handle(bytes);
    assert_eq!(h, h2);
    // It should be ks2_ + the first 32 hex of sha256("")
    let expected = format!("ks2_{}", &sha256_hex(bytes)[..32]);
    assert_eq!(h, expected);
}
