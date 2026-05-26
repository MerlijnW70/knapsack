//! Round-8 closer:
//! - MCP `knapsack_metrics` must return the SAME report text as the CLI
//!   `knapsack metrics`. Drift between the two surfaces is a real risk —
//!   they share `metrics::report` but it's worth pinning end-to-end.
//! - Disk write failure handling: read-only metrics path, read-only
//!   sessions dir. Pack/expand/install must fail gracefully, not panic.

use knapsack::mcp::handle_message;
use knapsack::{api, metrics};
use std::path::PathBuf;

fn sandbox(tag: &str) -> PathBuf {
    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    let d = std::env::temp_dir().join(format!("kn-mcp-cli-{}-{}-{}", tag, std::process::id(), t));
    std::fs::create_dir_all(&d).unwrap();
    std::env::set_var("KNAPSACK_STORE", d.join("store"));
    std::env::set_var("KNAPSACK_SESSIONS", d.join("sessions"));
    std::env::set_var("KNAPSACK_METRICS", d.join("metrics.jsonl"));
    d
}

fn teardown(d: &PathBuf) {
    for v in ["KNAPSACK_STORE", "KNAPSACK_SESSIONS", "KNAPSACK_METRICS"] {
        std::env::remove_var(v);
    }
    let _ = std::fs::remove_dir_all(d);
}

// =====================================================================
// 1. MCP / CLI symmetry on metrics
// =====================================================================

fn rpc_metrics_text(session: Option<&str>) -> String {
    let args = match session {
        Some(s) => format!(r#"{{"session_id":"{s}"}}"#),
        None => "{}".into(),
    };
    let req = format!(
        r#"{{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{{"name":"knapsack_metrics","arguments":{args}}}}}"#
    );
    let resp = handle_message(&req).expect("metrics tool must return a response");
    // Extract the text from response.result.content[0].text
    let v = knapsack::json::parse(&resp).expect("response must be valid JSON");
    let text = v
        .get("result")
        .and_then(|r| r.get("content"))
        .and_then(|c| match c {
            knapsack::json::Json::Arr(a) => a.first(),
            _ => None,
        })
        .and_then(|item| item.get("text"))
        .and_then(|t| t.as_str())
        .expect("text content present")
        .to_string();
    text
}

#[test]
fn mcp_metrics_text_equals_cli_metrics_text_when_no_data() {
    let dir = sandbox("mcp-sym-empty");
    let cli_text = metrics::report();
    let mcp_text = rpc_metrics_text(None);
    assert_eq!(cli_text, mcp_text,
        "MCP and CLI must return identical text for empty metrics state");
    teardown(&dir);
}

#[test]
fn mcp_metrics_text_equals_cli_metrics_text_with_real_data() {
    let dir = sandbox("mcp-sym-data");
    // Seed some metrics
    metrics::record_compress("sess-a", 1000, 300, 700, 5, 0);
    metrics::record_compress("sess-a", 800, 200, 600, 3, 0);
    metrics::record_compress("sess-b", 500, 100, 400, 2, 0);
    metrics::record_expand("sess-a", 100, true);

    let cli_text = metrics::report();
    let mcp_text = rpc_metrics_text(None);
    assert_eq!(cli_text, mcp_text,
        "MCP and CLI must return identical text for populated metrics state");
    teardown(&dir);
}

#[test]
fn mcp_metrics_text_matches_cli_per_session_filter() {
    let dir = sandbox("mcp-sym-filter");
    metrics::record_compress("filter-target", 1000, 200, 800, 5, 0);
    metrics::record_compress("other-session", 500, 100, 400, 2, 0);

    let cli_filtered = metrics::report_for(Some("filter-target"));
    let mcp_filtered = rpc_metrics_text(Some("filter-target"));
    assert_eq!(cli_filtered, mcp_filtered, "per-session filter symmetry");

    // The filtered report should NOT mention the OTHER session's numbers
    let unfiltered = metrics::report();
    assert_ne!(cli_filtered, unfiltered, "filter must actually change the output");
    teardown(&dir);
}

#[test]
fn mcp_metrics_response_envelope_shape_is_stable() {
    let dir = sandbox("mcp-env-shape");
    let req = r#"{"jsonrpc":"2.0","id":42,"method":"tools/call","params":{"name":"knapsack_metrics","arguments":{}}}"#;
    let resp = handle_message(req).expect("must respond");

    // Pinned envelope: jsonrpc + id (echoed) + result.content[0].{type=text,text=...} + result.isError=false
    assert!(resp.contains(r#""jsonrpc":"2.0""#));
    assert!(resp.contains(r#""id":42"#), "id echoed back");
    assert!(resp.contains(r#""type":"text""#));
    assert!(resp.contains(r#""isError":false"#), "metrics is informational, not an error");
    teardown(&dir);
}

#[test]
fn mcp_inspect_response_envelope_shape() {
    // Same envelope check for knapsack_inspect on a fresh handle.
    let dir = sandbox("mcp-insp-shape");
    let bytes = b"inspect test content";
    let store = knapsack::store::Store::new(dir.join("store"));
    let h = store.put(bytes);
    let req = format!(
        r#"{{"jsonrpc":"2.0","id":99,"method":"tools/call","params":{{"name":"knapsack_inspect","arguments":{{"handle":"{h}"}}}}}}"#
    );
    let resp = handle_message(&req).expect("must respond");
    assert!(resp.contains(r#""id":99"#));
    assert!(resp.contains(r#""type":"text""#));
    assert!(resp.contains(r#""isError":false"#));
    assert!(resp.contains("bytes"), "inspect text contains 'bytes'");
    teardown(&dir);
}

#[test]
fn mcp_expand_response_envelope_shape() {
    let dir = sandbox("mcp-exp-shape");
    let bytes = b"expand test content\nline two\n";
    let store = knapsack::store::Store::new(dir.join("store"));
    let h = store.put(bytes);
    let req = format!(
        r#"{{"jsonrpc":"2.0","id":17,"method":"tools/call","params":{{"name":"knapsack_expand","arguments":{{"handle":"{h}"}}}}}}"#
    );
    let resp = handle_message(&req).expect("must respond");
    assert!(resp.contains(r#""id":17"#));
    assert!(resp.contains(r#""type":"text""#));
    assert!(resp.contains(r#""isError":false"#));
    teardown(&dir);
}

// =====================================================================
// 2. Disk write failure handling
// =====================================================================

#[test]
fn pack_with_unwriteable_metrics_path_does_not_crash() {
    // Point KNAPSACK_METRICS at an unwriteable location (a directory, so the
    // open-as-file fails). Pack must still succeed in compression — the
    // metrics-write failure is silently swallowed (`let _ = ...` in metrics.rs).
    let dir = sandbox("disk-fail-metrics");
    // Replace metrics with a path that's actually a directory (write fails)
    let bad_metrics_dir = dir.join("metrics-as-dir");
    std::fs::create_dir_all(&bad_metrics_dir).unwrap();
    std::env::set_var("KNAPSACK_METRICS", &bad_metrics_dir);

    let r = api::pack_output(api::PackRequest {
        session_id: "disk-fail".into(),
        command: None,
        bytes: b"some output\nline two\nline three\n".repeat(20),
        content_hint: Some(knapsack::content_type::ContentType::Log),
        step: 0,
        transcript_path: None,
    });
    // Pack succeeded — compression worked
    assert!(r.raw_tokens_est > 0);
    // Metrics is a directory; record_compress silently failed, no panic.
    teardown(&dir);
}

#[test]
fn expand_on_handle_from_unwriteable_store_returns_none() {
    // Point store at a directory we can't extend (simulate by making the
    // store dir a regular FILE). store::put fails to write; get returns None.
    let dir = sandbox("disk-fail-store");
    let fake_store_dir = dir.join("store-as-file");
    // Create as a FILE not a dir
    std::fs::write(&fake_store_dir, b"i am a regular file pretending to be the store dir").unwrap();
    std::env::set_var("KNAPSACK_STORE", &fake_store_dir);

    let r = api::pack_output(api::PackRequest {
        session_id: "disk-fail-2".into(),
        command: None,
        bytes: b"some output\nline two\n".to_vec(),
        content_hint: Some(knapsack::content_type::ContentType::Log),
        step: 0,
        transcript_path: None,
    });
    // Pack still returns a result (in-memory work)
    assert!(r.raw_tokens_est > 0);
    // But expand of any handle should return None (store is broken).
    let out = api::expand_handle(api::ExpandRequest {
        handle: "ks2_00000000000000000000000000000000".into(),
        range: None,
        grep: None,
        context: 0,
        session_id: "x".into(),
    });
    assert!(out.is_none(), "expand on broken store returns None, not crash");
    teardown(&dir);
}

#[test]
fn doctor_on_unwriteable_store_reports_fail() {
    let dir = sandbox("doctor-rw-store");
    let fake_store = dir.join("store-as-file");
    std::fs::write(&fake_store, b"file not dir").unwrap();
    std::env::set_var("KNAPSACK_STORE", &fake_store);

    let report = knapsack::install::doctor();
    // The "store writable" check probes by writing a test file. With store dir
    // being a regular file, writing inside it fails. Doctor must report ✗.
    assert!(report.contains("store writable"));
    // Either "✗" or "Unhealthy" or both should appear.
    assert!(
        report.contains('✗') || report.contains("Unhealthy"),
        "doctor must surface the disk problem; got:\n{report}"
    );
    teardown(&dir);
}

#[test]
fn metrics_in_memory_fallback_when_file_missing() {
    // If the metrics file simply doesn't exist (fresh install), summary
    // returns zeros — no panic. Already pinned elsewhere but doubly assert
    // here because of the disk-failure theme.
    let dir = sandbox("metrics-missing");
    let report = metrics::report();
    assert!(report.contains("compress events"));
    assert!(report.contains("no data yet"));
    teardown(&dir);
}

// =====================================================================
// 3. metrics::report stability under concurrent writes
// =====================================================================

#[test]
fn metrics_concurrent_writes_dont_corrupt_summary() {
    let dir = sandbox("metrics-concur");
    // 10 threads each writing 50 compress events. The single-write_all in
    // metrics::append should keep lines intact under concurrency.
    let mut handles = vec![];
    for t in 0..10 {
        handles.push(std::thread::spawn(move || {
            for i in 0..50 {
                metrics::record_compress(
                    &format!("thread-{t}"), 100, 50, 50, i, 0);
            }
        }));
    }
    for h in handles {
        h.join().unwrap();
    }
    let s = metrics::summary();
    // 10 threads × 50 events = 500 compress events. No corruption.
    assert_eq!(s.compress_events, 500,
        "concurrent writes must not corrupt or drop lines; got {} events",
        s.compress_events);
    assert_eq!(s.raw, 500 * 100);
    assert_eq!(s.saved, 500 * 50);
    teardown(&dir);
}
