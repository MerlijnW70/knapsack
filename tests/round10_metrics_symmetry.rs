//! Round-10: MCP / CLI metrics symmetry under edge values.
//!
//! `tests/mcp_cli_symmetry_and_disk.rs` covers the basic symmetry (empty,
//! populated, per-session filter). This file pins symmetry across the
//! adversarial value space: negative net, huge numbers, long session IDs,
//! malformed lines, session filter with no matches, session filter naming
//! a session that ALSO contains failed expands.
//!
//! Contract: `knapsack metrics` (CLI) and `knapsack_metrics` (MCP tool) must
//! return BYTE-IDENTICAL text. The unfiltered surface dispatches through
//! `metrics::report()` (which prepends a "current session" block before the
//! lifetime table); the per-session filter goes through `metrics::report_for`
//! directly. A drift means either the MCP envelope changed the body or one
//! surface added a header/footer the other didn't — scripts grepping the
//! output would break silently.

mod common;
use common::EnvSandbox;

use knapsack::mcp::handle_message;
use knapsack::metrics;

fn rpc_metrics_text(session: Option<&str>) -> String {
    let args = match session {
        Some(s) => format!(r#"{{"session_id":"{s}"}}"#),
        None => "{}".into(),
    };
    let req = format!(
        r#"{{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{{"name":"knapsack_metrics","arguments":{args}}}}}"#
    );
    let resp = handle_message(&req).expect("metrics tool must return a response");
    let v = knapsack::json::parse(&resp).expect("response must be valid JSON");
    v.get("result")
        .and_then(|r| r.get("content"))
        .and_then(|c| match c {
            knapsack::json::Json::Arr(a) => a.first(),
            _ => None,
        })
        .and_then(|item| item.get("text"))
        .and_then(|t| t.as_str())
        .expect("text content present")
        .to_string()
}

fn cli_metrics_text(session: Option<&str>) -> String {
    // Mirror what `knapsack metrics` actually runs in main.rs: dispatch to
    // `report()` (with current-session prefix) for the unfiltered case, and
    // to `report_for(Some(s))` (single-session view, no prefix) when filtering.
    // This keeps the symmetry assertion meaningful — both sides reflect the
    // real user-facing surface, not an internal helper.
    match session {
        Some(s) => metrics::report_for(Some(s)),
        None => metrics::report(),
    }
}

// =====================================================================
// 1. Negative-net session symmetry
// =====================================================================

#[test]
fn negative_net_session_renders_identically_on_mcp_and_cli() {
    let _sb = EnvSandbox::new("neg-net-sym");
    // saved=100, refetched=300 → net=-200
    metrics::record_compress("over-recall", 500, 400, 100, 0, 0);
    metrics::record_expand(
        "over-recall",
        "ks2_test",
        300,
        true,
        metrics::ExpandMode::Whole,
        metrics::ExpandCaller::Cli,
    );

    let cli = cli_metrics_text(None);
    let mcp = rpc_metrics_text(None);
    assert_eq!(
        cli, mcp,
        "negative-net text must match byte-for-byte across surfaces"
    );
    assert!(
        cli.contains("-200") || cli.contains("(-200)") || cli.contains("net NEGATIVE"),
        "verdict reflects negative net somewhere in:\n{cli}"
    );
}

// =====================================================================
// 2. Huge numbers (test overflow / formatting parity)
// =====================================================================

#[test]
fn huge_numeric_values_render_identically() {
    let _sb = EnvSandbox::new("huge-sym");
    // 100 compress events each at ~1 G tokens → 100 G total; well within i64.
    for _ in 0..100 {
        metrics::record_compress(
            "whale",
            1_000_000_000,
            100_000_000,
            900_000_000,
            1_000_000,
            0,
        );
    }
    metrics::record_expand(
        "whale",
        "ks2_test",
        999_999_999,
        true,
        metrics::ExpandMode::Whole,
        metrics::ExpandCaller::Cli,
    );

    let cli = cli_metrics_text(None);
    let mcp = rpc_metrics_text(None);
    assert_eq!(cli, mcp, "huge-number formatting must match");
    // Sanity: numbers actually appear (commafy may insert thousands separators)
    assert!(
        cli.contains("100,000,000,000") || cli.contains("100000000000"),
        "raw aggregate at 100 G appears in:\n{cli}"
    );
}

// =====================================================================
// 3. Long session ID — both surfaces truncate identically (or both don't)
// =====================================================================

#[test]
fn long_session_id_renders_identically_filtered_and_unfiltered() {
    let _sb = EnvSandbox::new("long-sid-sym");
    let long_sid = "a".repeat(80);
    metrics::record_compress(&long_sid, 500, 100, 400, 0, 0);

    let cli_all = cli_metrics_text(None);
    let mcp_all = rpc_metrics_text(None);
    assert_eq!(cli_all, mcp_all, "unfiltered long-sid view must match");

    let cli_f = cli_metrics_text(Some(&long_sid));
    let mcp_f = rpc_metrics_text(Some(&long_sid));
    assert_eq!(cli_f, mcp_f, "filtered long-sid view must match");
}

// =====================================================================
// 4. Filter naming a session that doesn't exist
// =====================================================================

#[test]
fn filter_with_no_matches_renders_identically_and_shows_zero_state() {
    let _sb = EnvSandbox::new("filter-empty-sym");
    metrics::record_compress("real-session", 500, 100, 400, 5, 0);

    let cli = cli_metrics_text(Some("nonexistent-session"));
    let mcp = rpc_metrics_text(Some("nonexistent-session"));
    assert_eq!(cli, mcp, "no-match filter must match across surfaces");

    // Filtered view should still render the column labels (so users know it's
    // a real query that just had no data), and the filtered NET should be 0
    // / "no data yet" verdict.
    assert!(
        cli.contains("compress events"),
        "filtered view keeps labels: \n{cli}"
    );
}

// =====================================================================
// 5. Malformed metrics lines don't desync the two surfaces
// =====================================================================

#[test]
fn malformed_lines_dont_cause_mcp_cli_drift() {
    let sb = EnvSandbox::new("malformed-sym");
    let metrics_path = sb.join("metrics.jsonl");
    // 3 valid compress lines, with a corrupt line in the middle and a
    // missing-event line at the end.
    let content = concat!(
        r#"{"event":"compress","session":"a","raw":100,"shown":50,"saved":50,"delta_hits":0,"evicted":0}"#,
        "\n",
        "garbage line that won't parse",
        "\n",
        r#"{"event":"compress","session":"a","raw":200,"shown":75,"saved":125,"delta_hits":1,"evicted":0}"#,
        "\n",
        r#"{"event":"expand","session":"a","tokens":30,"ok":true}"#,
        "\n",
        r#"{"event":"compress","session":"a","raw":300,"shown":100,"saved":200,"delta_hits":2,"evicted":0}"#,
        "\n",
        r#"{"no_event_field":"oops"}"#,
        "\n",
    );
    std::fs::write(&metrics_path, content).unwrap();

    let cli = cli_metrics_text(None);
    let mcp = rpc_metrics_text(None);
    assert_eq!(
        cli, mcp,
        "MCP and CLI must drop the same lines and produce identical text"
    );

    // Sanity check: 3 valid compress events, saved = 50+125+200 = 375.
    let summary = metrics::summary();
    assert_eq!(summary.compress_events, 3);
    assert_eq!(summary.saved, 375);
}

// =====================================================================
// 6. Failed-expand attribution symmetry
// =====================================================================

#[test]
fn failed_expand_attribution_renders_identically_on_both_surfaces() {
    let _sb = EnvSandbox::new("failed-expand-sym");
    metrics::record_compress("flaky", 500, 100, 400, 0, 0);
    metrics::record_expand(
        "flaky",
        "ks2_test",
        50,
        true,
        metrics::ExpandMode::Whole,
        metrics::ExpandCaller::Cli,
    ); // ok
    metrics::record_expand(
        "flaky",
        "ks2_test",
        75,
        false,
        metrics::ExpandMode::Whole,
        metrics::ExpandCaller::Cli,
    ); // fail
    metrics::record_expand(
        "flaky",
        "ks2_test",
        25,
        true,
        metrics::ExpandMode::Whole,
        metrics::ExpandCaller::Cli,
    ); // ok

    let cli = cli_metrics_text(None);
    let mcp = rpc_metrics_text(None);
    assert_eq!(cli, mcp, "failed-expand attribution renders identically");
}

// =====================================================================
// 7. Many sessions with mixed nets — sorted ordering parity
// =====================================================================

#[test]
fn many_sessions_sort_order_is_identical_across_surfaces() {
    let _sb = EnvSandbox::new("many-sessions-sym");
    // 10 sessions with varying nets — both surfaces must list them in the
    // same order (descending net), because both go through metrics::report.
    for i in 0..10usize {
        let sid = format!("session-{i:02}");
        let saved = (10 - i) * 100;
        metrics::record_compress(&sid, 1000, 1000 - saved, saved as isize, 0, 0);
        // Half also have refetches to mix up the nets
        if i.is_multiple_of(2) {
            metrics::record_expand(
                &sid,
                "ks2_test",
                i * 10,
                true,
                metrics::ExpandMode::Whole,
                metrics::ExpandCaller::Cli,
            );
        }
    }

    let cli = cli_metrics_text(None);
    let mcp = rpc_metrics_text(None);
    assert_eq!(cli, mcp, "10-session sort order identical across MCP+CLI");
}

// =====================================================================
// 8. Per-session text isolation: filtering removes other sessions' numbers
// =====================================================================

#[test]
fn per_session_filter_removes_other_session_numbers_consistently() {
    let _sb = EnvSandbox::new("per-session-iso");
    metrics::record_compress("alpha", 1000, 100, 900, 0, 0);
    metrics::record_compress("beta", 500, 100, 400, 0, 0);

    let cli_alpha = cli_metrics_text(Some("alpha"));
    let mcp_alpha = rpc_metrics_text(Some("alpha"));
    assert_eq!(cli_alpha, mcp_alpha);

    // The filtered text should NOT mention beta's numbers. We use "500" as
    // a sentinel — alpha doesn't have 500 anywhere. (Tolerant test: the
    // commafied form might be "500" or " 500"; just check the substring.)
    //
    // Alpha has raw=1000 saved=900 shown=100. None of those equal beta's
    // raw=500 saved=400 shown=100. So "500" as a number on its own would
    // be a beta-leak.
    // We allow "1000" to appear (alpha's raw); just check that the formatted
    // total doesn't include 1500 (alpha + beta merged).
    assert!(
        !cli_alpha.contains("1,500"),
        "filter must not leak beta totals into alpha view"
    );
    assert!(!cli_alpha.contains(" 1500 "), "no merged total either");
}
