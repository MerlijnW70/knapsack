//! Round-10: AB / bench math honesty.
//!
//! The AB report and the bench harness are advertising surfaces — they tell
//! the user how much knapsack saved them. If the math is dishonest (numbers
//! tuned to look good), the user gets burned. These tests pin every accounting
//! invariant we claim:
//!
//!   - `net()` is STRICTLY `saved - refetched`. Never delta_hits, never evicted.
//!   - Failed expands count toward `expand_calls` + `failed_expands` but NOT
//!     toward `refetched` (because no bytes came back).
//!   - Compress event with `saved` field missing falls back to `raw - shown`
//!     (per ab.rs:78). Tested explicitly because the fallback is silent.
//!   - Sessions sort by net DESCENDING (best first). Stable order.
//!   - Aggregate total = sum of per-session aggs (no double-counting).
//!   - "no data yet" verdict iff `compress_events == 0`.
//!   - Bench A/B/C math: A is always ≥ B is always ≥ C for non-trivial inputs;
//!     C delta_hits > 0 from iteration 2+ (the conditional layer DOES kick in).

mod common;
use common::EnvSandbox;

use knapsack::ab;
use knapsack::bench;
use knapsack::content_type::ContentType;
use knapsack::ledger::Ledger;
use knapsack::pack::pack;
use knapsack::store::Store;
use knapsack::structural;
use knapsack::token_estimate::tokens;
use std::path::PathBuf;

// =====================================================================
// 1. net() identity — saved − refetched, nothing else
// =====================================================================

fn write_metrics(sb: &EnvSandbox, lines: &str) -> PathBuf {
    let p = sb.join("metrics.jsonl");
    std::fs::write(&p, lines).unwrap();
    p
}

#[test]
fn ab_net_is_strictly_saved_minus_refetched_delta_hits_doesnt_pollute() {
    let sb = EnvSandbox::new("ab-net-identity");
    // saved=100, refetched=30, delta_hits=999, evicted=999.
    // net MUST be 70 — delta_hits & evicted are reported but never folded in.
    let p = write_metrics(
        &sb,
        concat!(
            r#"{"event":"compress","session":"a","raw":1000,"shown":900,"saved":100,"delta_hits":999,"evicted":999}"#,
            "\n",
            r#"{"event":"expand","session":"a","tokens":30,"ok":true}"#,
            "\n",
        ),
    );
    let r = ab::build(&p);
    assert_eq!(r.total.saved, 100);
    assert_eq!(r.total.refetched, 30);
    assert_eq!(r.total.delta_hits, 999);
    assert_eq!(r.total.evicted, 999);
    assert_eq!(
        r.total.net(),
        70,
        "net = 100 - 30, NOT touched by delta_hits/evicted"
    );
}

#[test]
fn ab_failed_expand_does_not_count_as_refetched_but_does_count_as_expand_call() {
    let sb = EnvSandbox::new("ab-failed-expand");
    // saved=100, two expand calls: one ok=true tokens=20, one ok=false tokens=50.
    // Failed expand contributes to expand_calls + failed_expands but NOT refetched.
    let p = write_metrics(
        &sb,
        concat!(
            r#"{"event":"compress","session":"a","raw":500,"shown":400,"saved":100,"delta_hits":0,"evicted":0}"#,
            "\n",
            r#"{"event":"expand","session":"a","tokens":20,"ok":true}"#,
            "\n",
            r#"{"event":"expand","session":"a","tokens":50,"ok":false}"#,
            "\n",
        ),
    );
    let r = ab::build(&p);
    assert_eq!(r.total.expand_calls, 2, "both expand calls counted");
    assert_eq!(r.total.failed_expands, 1, "one failure counted");
    assert_eq!(
        r.total.refetched, 20,
        "only OK expand contributes refetched (the 50 is dropped)"
    );
    assert_eq!(r.total.net(), 100 - 20);
}

#[test]
fn ab_compress_with_saved_field_missing_falls_back_to_raw_minus_shown() {
    // ab.rs:78: `if v.get("saved").is_some() { numk(&v, &["saved"]) } else { raw - shown }`.
    // Pre-fix metrics may have lacked the saved field. Pin the fallback.
    let sb = EnvSandbox::new("ab-saved-missing");
    let p = write_metrics(
        &sb,
        concat!(
            r#"{"event":"compress","session":"a","raw":1000,"shown":300,"delta_hits":0,"evicted":0}"#,
            "\n",
        ),
    );
    let r = ab::build(&p);
    assert_eq!(r.total.raw, 1000);
    assert_eq!(r.total.shown, 300);
    assert_eq!(
        r.total.saved, 700,
        "fallback: saved = raw - shown when field absent"
    );
    assert_eq!(r.total.net(), 700, "and net follows the fallback");
}

#[test]
fn ab_compress_with_explicit_saved_overrides_raw_minus_shown_arithmetic() {
    // If the metric was written with an EXPLICIT saved that doesn't equal
    // raw - shown (which can happen if e.g. shown counts something different),
    // we trust the explicit value. Pin this.
    let sb = EnvSandbox::new("ab-saved-explicit");
    let p = write_metrics(
        &sb,
        concat!(
            r#"{"event":"compress","session":"a","raw":1000,"shown":300,"saved":650,"delta_hits":0,"evicted":0}"#,
            "\n",
        ),
    );
    let r = ab::build(&p);
    assert_eq!(r.total.saved, 650, "explicit saved overrides arithmetic");
    // raw-shown would be 700; we explicitly trust 650.
}

#[test]
fn ab_aggregate_equals_sum_of_session_aggregates_no_double_count() {
    let sb = EnvSandbox::new("ab-aggregate-sum");
    let p = write_metrics(
        &sb,
        concat!(
            r#"{"event":"compress","session":"a","raw":500,"shown":100,"saved":400,"delta_hits":2,"evicted":0}"#,
            "\n",
            r#"{"event":"compress","session":"b","raw":700,"shown":200,"saved":500,"delta_hits":5,"evicted":1}"#,
            "\n",
            r#"{"event":"compress","session":"a","raw":300,"shown":50,"saved":250,"delta_hits":1,"evicted":0}"#,
            "\n",
            r#"{"event":"expand","session":"a","tokens":10,"ok":true}"#,
            "\n",
            r#"{"event":"expand","session":"b","tokens":25,"ok":true}"#,
            "\n",
        ),
    );
    let r = ab::build(&p);
    // 3 compress events across 2 sessions; raw = 500 + 700 + 300 = 1500
    let sum_raw: i64 = r.sessions.iter().map(|s| s.1.raw).sum();
    let sum_saved: i64 = r.sessions.iter().map(|s| s.1.saved).sum();
    let sum_refetched: i64 = r.sessions.iter().map(|s| s.1.refetched).sum();
    let sum_compress: i64 = r.sessions.iter().map(|s| s.1.compress_events).sum();
    assert_eq!(sum_raw, r.total.raw);
    assert_eq!(sum_saved, r.total.saved);
    assert_eq!(sum_refetched, r.total.refetched);
    assert_eq!(sum_compress, r.total.compress_events);
    assert_eq!(r.total.compress_events, 3);
    assert_eq!(r.total.raw, 1500);
}

#[test]
fn ab_sessions_sorted_by_net_descending_best_first() {
    let sb = EnvSandbox::new("ab-sort-desc");
    let p = write_metrics(
        &sb,
        concat!(
            r#"{"event":"compress","session":"middle","raw":1000,"shown":500,"saved":500,"delta_hits":0,"evicted":0}"#,
            "\n",
            r#"{"event":"compress","session":"worst","raw":1000,"shown":900,"saved":100,"delta_hits":0,"evicted":0}"#,
            "\n",
            r#"{"event":"compress","session":"best","raw":1000,"shown":100,"saved":900,"delta_hits":0,"evicted":0}"#,
            "\n",
        ),
    );
    let r = ab::build(&p);
    let order: Vec<&str> = r.sessions.iter().map(|(s, _)| s.as_str()).collect();
    assert_eq!(
        order,
        vec!["best", "middle", "worst"],
        "sorted by net descending: 900, 500, 100"
    );
}

#[test]
fn ab_no_data_yet_verdict_iff_compress_events_zero() {
    let sb = EnvSandbox::new("ab-no-data-iff");

    // Case 1: empty metrics → "no data yet"
    let p = write_metrics(&sb, "");
    let r = ab::build(&p);
    let txt = ab::format(&r);
    assert!(txt.contains("no data yet"), "empty → no data yet");

    // Case 2: only expand events (no compress) → STILL "no data yet"
    let p = write_metrics(
        &sb,
        concat!(
            r#"{"event":"expand","session":"a","tokens":50,"ok":true}"#,
            "\n",
        ),
    );
    let r = ab::build(&p);
    let txt = ab::format(&r);
    assert!(
        txt.contains("no data yet"),
        "expand events without compress → still no data (compress_events==0)"
    );

    // Case 3: one compress → NOT "no data yet" (verdict shifts to POSITIVE/NEGATIVE)
    let p = write_metrics(
        &sb,
        concat!(
            r#"{"event":"compress","session":"a","raw":100,"shown":50,"saved":50,"delta_hits":0,"evicted":0}"#,
            "\n",
        ),
    );
    let r = ab::build(&p);
    let txt = ab::format(&r);
    assert!(
        !txt.contains("no data yet"),
        "one compress event triggers the real verdict, not 'no data yet'"
    );
    assert!(txt.contains("net POSITIVE"));
}

#[test]
fn ab_zero_net_session_is_neither_positive_nor_negative_uses_negative_branch() {
    // ab.rs:181-188: verdict is "net POSITIVE" iff net > 0, else "net NEGATIVE".
    // Pin: net == 0 produces NEGATIVE wording (saved exactly cancels refetched).
    // Worth pinning explicitly because users reading the verdict need to know
    // exactly when wording flips.
    let sb = EnvSandbox::new("ab-net-zero");
    let p = write_metrics(
        &sb,
        concat!(
            r#"{"event":"compress","session":"a","raw":200,"shown":100,"saved":100,"delta_hits":0,"evicted":0}"#,
            "\n",
            r#"{"event":"expand","session":"a","tokens":100,"ok":true}"#,
            "\n",
        ),
    );
    let r = ab::build(&p);
    assert_eq!(r.total.net(), 0);
    let txt = ab::format(&r);
    assert!(
        txt.contains("net NEGATIVE"),
        "verdict for net==0 uses NEGATIVE wording (`net > 0` is the trigger for POSITIVE)"
    );
}

// =====================================================================
// 2. Bench A/B/C: math is internally consistent
// =====================================================================
//
// bench::run() only prints; we don't have a "give me back the numbers" API.
// Instead, replicate the math out-of-line using the public bench::gen_file +
// gen_log fixtures and the pack/structural/tokens APIs, and pin the invariants.

fn compute_abc(k: usize, store: &Store, ledger: &mut Ledger) -> (usize, usize, usize, usize) {
    let file = bench::gen_file(k);
    let log = bench::gen_log(k);
    let a = tokens(&file) + tokens(&log);
    let b_file = structural::compress(file.as_bytes(), 0, file.len(), ContentType::Code).0;
    let b_log = structural::compress(log.as_bytes(), 0, log.len(), ContentType::Log).0;
    let b = tokens(&b_file) + tokens(&b_log);
    let c_file = pack(file.as_bytes(), ContentType::Code, store, ledger, k as u64);
    let c_log = pack(log.as_bytes(), ContentType::Log, store, ledger, k as u64);
    let c = c_file.shown_tokens_est + c_log.shown_tokens_est;
    let unchanged = c_file.delta_hits + c_log.delta_hits;
    (a, b, c, unchanged)
}

#[test]
fn bench_abc_invariant_a_ge_b_ge_c_for_each_iteration() {
    // A (raw) is the upper bound. B (stateless structural) ≤ A.
    // C (conditional) ≤ B for any iteration ≥ 1 where the ledger has
    // accumulated state. For iteration 0 (cold), C ≈ B (no delta benefit
    // yet); allow equality.
    let sb = EnvSandbox::new("bench-abc-ordering");
    let store = Store::new(sb.join("store"));
    let mut ledger = Ledger::in_memory();
    let mut last_unchanged = 0usize;
    for k in 0..6 {
        let (a, b, c, unchanged) = compute_abc(k, &store, &mut ledger);
        assert!(a > 0, "iter {k}: A > 0");
        assert!(
            b <= a,
            "iter {k}: B ({b}) must be ≤ A ({a}) — Rucksack never inflates"
        );
        assert!(
            c <= b,
            "iter {k}: C ({c}) must be ≤ B ({b}) — conditional never worse than stateless"
        );
        if k >= 1 {
            assert!(
                unchanged > 0,
                "iter {k} (warm): conditional MUST find some unchanged blocks, got 0",
            );
        }
        last_unchanged = unchanged;
    }
    assert!(
        last_unchanged > 0,
        "final iteration has accumulated delta hits"
    );
}

#[test]
fn bench_abc_cold_iteration_has_zero_delta_hits() {
    // Iteration 0 with a fresh ledger: nothing is resident yet.
    // delta_hits MUST be 0 — we're seeing the bytes for the first time.
    let sb = EnvSandbox::new("bench-abc-cold");
    let store = Store::new(sb.join("store"));
    let mut ledger = Ledger::in_memory();
    let (_, _, _, unchanged) = compute_abc(0, &store, &mut ledger);
    assert_eq!(
        unchanged, 0,
        "first pack with empty ledger has zero delta hits"
    );
}

#[test]
fn bench_gen_file_changes_only_in_edited_handler_count() {
    // The fixture's edit model: each iteration k changes exactly handlers
    // 0..k. So gen_file(3) differs from gen_file(2) only in handler 2's
    // constant. Pin that the fixtures actually realize this — if they
    // changed too much per iteration, the bench numbers would lie about
    // the realism of the workload.
    let f0 = bench::gen_file(0);
    let f1 = bench::gen_file(1);
    let f2 = bench::gen_file(2);
    assert_ne!(f0, f1);
    assert_ne!(f1, f2);
    // Per-line diff: f1 vs f0 should differ in exactly 1 line; f2 vs f1 in
    // exactly 1 line.
    let diff_count =
        |a: &str, b: &str| -> usize { a.lines().zip(b.lines()).filter(|(x, y)| x != y).count() };
    assert_eq!(
        diff_count(&f0, &f1),
        1,
        "iter 0→1: one handler line differs"
    );
    assert_eq!(
        diff_count(&f1, &f2),
        1,
        "iter 1→2: one handler line differs"
    );
}
