//! Round 8 — bench internals, gc::coverage at scale, hash collision handling,
//! weird sources, real transcript shapes, store::with_session attribution chain.
//!
//! Bench is the proof artifact (`knapsack bench`). If its A/B/C math is wrong,
//! everything else is suspect. We pin the generator determinism + that
//! C(=knapsack) <= B(=stateless) <= A(=raw) holds for the synthetic workload
//! it's designed for.

use knapsack::api::{ExpandCaller, expand_handle, pack_output, ExpandRequest, PackRequest};
use knapsack::bench;
use knapsack::content_type::ContentType;
use knapsack::gc;
use knapsack::hash::handle;
use knapsack::ledger::Ledger;
use knapsack::pack::{pack, reconstruct};
use knapsack::store::Store;
use knapsack::structural;
use knapsack::token_estimate::tokens;
use std::path::PathBuf;

fn tmp(tag: &str) -> PathBuf {
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let d = std::env::temp_dir().join(format!("kn-r8-{}-{}-{}", tag, std::process::id(), t));
    std::fs::create_dir_all(&d).unwrap();
    d
}

// =====================================================================
// 1. bench A/B/C internals
// =====================================================================

#[test]
fn bench_gen_file_is_deterministic() {
    // Same `edited` arg → byte-identical file. Determinism is what makes
    // the bench reproducible across runs.
    for k in 0..6 {
        assert_eq!(
            bench::gen_file(k),
            bench::gen_file(k),
            "gen_file({k}) must be deterministic"
        );
    }
}

#[test]
fn bench_gen_file_changes_with_edited_count() {
    // Iteration k has the first `k` functions tweaked. k=0 vs k=1 differ
    // in exactly the handler0 block.
    let f0 = bench::gen_file(0);
    let f1 = bench::gen_file(1);
    assert_ne!(f0, f1, "edited count must change the output");
    // Handler 0 in f0 has `* 0 *`; in f1 it has `* 100 *` (bump applied)
    assert!(f0.contains("* 0 *"), "f0 handler0 uses i=0");
    assert!(f1.contains("* 100 *"), "f1 handler0 uses i+bump=100");
}

#[test]
fn bench_gen_log_passes_fail_counts_track_fixed() {
    let l0 = bench::gen_log(0); // 4 failing
    let l4 = bench::gen_log(4); // 0 failing
    assert!(l0.contains("4 failed"));
    assert!(l4.contains("0 failed"));
}

#[test]
fn bench_a_b_c_ordering_holds_synthetic_workload() {
    // For the bench workload — a 40-handler module + jest log, where each
    // iteration changes ONE handler and the test summary line — the
    // following inequalities MUST hold over the session total:
    //   A (raw)        >  B (stateless)  >=  C (knapsack)
    // Anything else means a regression in either the structural compressor
    // (B) or the conditional layer (C).
    let dir = tmp("bench-abc-pinned");
    let store = Store::new(dir.clone());
    let mut ledger = Ledger::in_memory();

    let (mut a_tot, mut b_tot, mut c_tot) = (0usize, 0usize, 0usize);

    for k in 0..6 {
        let file = bench::gen_file(k);
        let log = bench::gen_log(k);

        let a = tokens(&file) + tokens(&log);

        let b_file = structural::compress(file.as_bytes(), 0, file.len(), ContentType::Code).0;
        let b_log = structural::compress(log.as_bytes(), 0, log.len(), ContentType::Log).0;
        let b = tokens(&b_file) + tokens(&b_log);

        let c_file = pack(
            file.as_bytes(),
            ContentType::Code,
            &store,
            &mut ledger,
            k as u64,
        );
        let c_log = pack(
            log.as_bytes(),
            ContentType::Log,
            &store,
            &mut ledger,
            k as u64,
        );
        let c = c_file.shown_tokens_est + c_log.shown_tokens_est;

        a_tot += a;
        b_tot += b;
        c_tot += c;
    }

    // Stateless compression must save against raw on the synthetic workload.
    assert!(
        b_tot < a_tot,
        "B (stateless) must save vs A (raw): A={a_tot} B={b_tot}"
    );
    // Conditional must be at least as good as stateless on a delta-friendly
    // workload (the bench is designed to be delta-friendly).
    assert!(
        c_tot < b_tot,
        "C (knapsack) must beat B (stateless) on delta-friendly: B={b_tot} C={c_tot}"
    );
    // Rough sanity: total tokens are in the right order of magnitude.
    assert!(a_tot > 1000, "A total nonzero ({a_tot})");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn bench_first_iteration_is_cold_for_c() {
    // First iteration (k=0) has nothing in the ledger yet, so C should
    // approximately equal B (no delta benefit on a cold session).
    let dir = tmp("bench-cold-c");
    let store = Store::new(dir.clone());
    let mut ledger = Ledger::in_memory();

    let file = bench::gen_file(0);
    let log = bench::gen_log(0);

    let b_file = structural::compress(file.as_bytes(), 0, file.len(), ContentType::Code).0;
    let b_log = structural::compress(log.as_bytes(), 0, log.len(), ContentType::Log).0;
    let b = tokens(&b_file) + tokens(&b_log);

    let c_file = pack(file.as_bytes(), ContentType::Code, &store, &mut ledger, 0);
    let c_log = pack(log.as_bytes(), ContentType::Log, &store, &mut ledger, 0);
    let c = c_file.shown_tokens_est + c_log.shown_tokens_est;

    // First iteration: cold session, delta_hits should be 0.
    assert_eq!(c_file.delta_hits, 0, "cold pack: no delta hits");
    assert_eq!(c_log.delta_hits, 0);
    // C is allowed a tiny markup over B from per-block bookkeeping but
    // should be within ~30%.
    let ratio = (c as f64) / (b as f64);
    assert!(
        ratio < 1.3,
        "cold C ({c}) should not be wildly worse than B ({b}); ratio={ratio:.2}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn bench_second_iteration_delta_hits_substantial() {
    // After iteration 1 warms up, iteration 2 (which only changes handler0)
    // should produce LOTS of delta hits — most blocks unchanged.
    let dir = tmp("bench-warm-c");
    let store = Store::new(dir.clone());
    let mut ledger = Ledger::in_memory();

    // Warm up with iteration 0.
    let _ = pack(
        bench::gen_file(0).as_bytes(),
        ContentType::Code,
        &store,
        &mut ledger,
        0,
    );
    let _ = pack(
        bench::gen_log(0).as_bytes(),
        ContentType::Log,
        &store,
        &mut ledger,
        0,
    );

    // Now iteration 1 — only the first handler bumped, rest unchanged.
    let c_file_warm = pack(
        bench::gen_file(1).as_bytes(),
        ContentType::Code,
        &store,
        &mut ledger,
        1,
    );
    // Substantial delta hits: most of the 40 handlers are unchanged.
    assert!(
        c_file_warm.delta_hits >= 10,
        "warm pack must score many delta hits, got {}",
        c_file_warm.delta_hits
    );

    let _ = std::fs::remove_dir_all(&dir);
}

// =====================================================================
// 2. gc::coverage at scale
// =====================================================================

#[test]
fn gc_coverage_counts_correctly_at_1000_blocks() {
    let dir = tmp("gc-coverage-scale");
    let store = Store::new(dir.clone());

    // Put 1000 distinct ks2_ blocks (each gets a .meta sidecar)
    for i in 0..1000usize {
        let bytes = format!("block content number {i}");
        let _ = store.put(bytes.as_bytes());
    }
    let (total, with_meta) = gc::coverage(&store);
    assert_eq!(total, 1000, "1000 blocks placed");
    assert_eq!(with_meta, 1000, "every ks2_ block gets a meta sidecar");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn gc_coverage_handles_legacy_blocks_correctly() {
    let dir = tmp("gc-coverage-legacy");
    let store = Store::new(dir.clone());

    // 50 modern ks2_ blocks (with meta)
    for i in 0..50usize {
        let _ = store.put(format!("modern {i}").as_bytes());
    }
    // 30 legacy ks_ blocks (no meta — legacy didn't have sidecars)
    for i in 0..30usize {
        let bytes = format!("legacy {i}");
        let h = format!("ks_{}", &knapsack::hash::sha1_hex(bytes.as_bytes())[..10]);
        store.put_with_handle(&h, bytes.as_bytes());
    }
    let (total, with_meta) = gc::coverage(&store);
    assert_eq!(total, 80);
    assert_eq!(with_meta, 50, "only ks2_ blocks have meta sidecars");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn gc_coverage_at_5000_blocks_is_fast() {
    // O(n) walk over 5K files should take well under 5 seconds. A regression
    // to O(n^2) would blow this cap.
    let dir = tmp("gc-coverage-perf");
    let store = Store::new(dir.clone());
    for i in 0..5000usize {
        let _ = store.put(format!("perf block {i}").as_bytes());
    }
    let start = std::time::Instant::now();
    let (total, _) = gc::coverage(&store);
    let dur = start.elapsed();
    assert_eq!(total, 5000);
    assert!(dur.as_secs() < 5, "coverage scan of 5K blocks took {dur:?}");

    let _ = std::fs::remove_dir_all(&dir);
}

// =====================================================================
// 3. Hash-collision-style abuse: put_with_handle with mismatched bytes
// =====================================================================

#[test]
fn put_with_handle_mismatched_bytes_get_returns_none() {
    // put_with_handle accepts (handle, bytes) WITHOUT validating they match.
    // If a buggy caller passes wrong (h, b), the store writes them but
    // verify-on-read catches the mismatch and returns None — the byte-exact
    // contract holds at the read boundary.
    let dir = tmp("collision");
    let store = Store::new(dir.clone());
    let real_bytes = b"the real content";
    let real_handle = handle(real_bytes);

    // Now store DIFFERENT bytes under the SAME handle (simulates caller bug)
    let fake_bytes = b"DIFFERENT bytes pretending to be the real ones";
    store.put_with_handle(&real_handle, fake_bytes);

    // verify(real_handle, fake_bytes) → false → get returns None.
    let got = store.get(&real_handle);
    assert!(
        got.is_none(),
        "put_with_handle abuse must be caught by verify-on-read; got: {got:?}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn put_with_correct_handle_then_overwrite_with_wrong_bytes_rejects() {
    // Same scenario but the file already exists with correct bytes; then
    // a second put_with_handle is called with the same handle + different
    // bytes. put_with_handle skips write if file exists, so the original
    // bytes survive — and get() returns them.
    let dir = tmp("collision-overwrite");
    let store = Store::new(dir.clone());
    let real_bytes = b"original content";
    let h = handle(real_bytes);
    store.put_with_handle(&h, real_bytes);

    // Try to overwrite under the SAME handle with WRONG bytes
    store.put_with_handle(&h, b"different bytes that wont actually be written");

    // The block file should still contain the original (put_with_handle is
    // idempotent — skips write if file exists)
    let got = store.get(&h);
    assert_eq!(
        got.as_deref(),
        Some(&real_bytes[..]),
        "idempotent put preserves the original content under handle conflict"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

// =====================================================================
// 4. Real Claude Code transcript shapes
// =====================================================================

#[test]
fn transcript_with_realistic_claude_code_shape() {
    let dir = tmp("transcript-real");
    let p = dir.join("transcript.jsonl");

    // Approximation of what a Claude Code JSONL transcript looks like:
    // a mix of system messages, user messages (string content), assistant
    // messages (string OR array-of-blocks content), tool_use, tool_result.
    let content = r#"{"type":"system","message":"Session started"}
{"type":"user","message":{"role":"user","content":"hi"}}
{"type":"assistant","message":{"role":"assistant","content":"hello! I see [Knapsack: 50 lines unchanged · recall ks2_1111111111111111111111111111111111111]"}}
{"type":"tool_use","name":"Bash","input":{"command":"cargo test"}}
{"type":"tool_result","content":"output containing recall handle ks2_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}
{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"more text with ks2_bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"}]}}
{"role":"user","content":"/clear"}
{"type":"assistant","content":"new conversation with ks2_cccccccccccccccccccccccccccccccc"}
"#;
    std::fs::write(&p, content).unwrap();

    let scan = knapsack::transcript::scan(&p);
    assert!(scan.ok, "transcript must parse");
    assert!(scan.lines_scanned >= 8, "all 8 lines scanned");
    // /clear is at line index 6 (0-based) → boundary detected
    assert!(
        scan.last_boundary.is_some(),
        "must detect the /clear boundary"
    );
    // Resident handles = only those AFTER /clear
    assert!(
        scan.resident
            .contains("ks2_cccccccccccccccccccccccccccccccc"),
        "post-/clear handle present"
    );
    // Pre-/clear handles must be excluded
    for pre in [
        "ks2_1111111111111111111111111111111111111",
        "ks2_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "ks2_bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
    ] {
        assert!(
            !scan.resident.contains(pre),
            "handle {pre} appeared BEFORE /clear and must be excluded"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn transcript_with_compaction_event_resets_resident() {
    let dir = tmp("transcript-compact");
    let p = dir.join("transcript.jsonl");
    let content = r#"{"type":"assistant","content":"pre-compact: ks2_1111111111111111111111111111111111111"}
{"type":"compact","when":1234567}
{"type":"assistant","content":"post-compact: ks2_dddddddddddddddddddddddddddddddd"}
"#;
    std::fs::write(&p, content).unwrap();
    let scan = knapsack::transcript::scan(&p);
    assert!(scan.ok);
    assert!(scan
        .resident
        .contains("ks2_dddddddddddddddddddddddddddddddd"));
    assert!(
        !scan
            .resident
            .contains("ks2_1111111111111111111111111111111111111"),
        "pre-compact handle excluded"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn transcript_with_no_boundary_collects_every_handle() {
    let dir = tmp("transcript-no-boundary");
    let p = dir.join("transcript.jsonl");
    let content = r#"{"type":"assistant","content":"ks2_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}
{"type":"assistant","content":"ks2_bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"}
{"type":"assistant","content":"ks2_cccccccccccccccccccccccccccccccc"}
"#;
    std::fs::write(&p, content).unwrap();
    let scan = knapsack::transcript::scan(&p);
    assert!(scan.last_boundary.is_none(), "no boundary detected");
    assert_eq!(scan.resident.len(), 3, "all 3 handles resident");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn transcript_handles_appearing_multiple_times_dedup() {
    let dir = tmp("transcript-dedup");
    let p = dir.join("transcript.jsonl");
    let h = "ks2_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let content = format!(
        "{{\"type\":\"assistant\",\"content\":\"first {h}\"}}\n{{\"type\":\"assistant\",\"content\":\"second {h}\"}}\n{{\"type\":\"assistant\",\"content\":\"third {h}\"}}\n"
    );
    std::fs::write(&p, &content).unwrap();
    let scan = knapsack::transcript::scan(&p);
    assert_eq!(
        scan.resident.len(),
        1,
        "handle appearing N times still dedups to 1"
    );
    assert!(scan.resident.contains(h));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn transcript_collects_legacy_ks_handles_too() {
    let dir = tmp("transcript-legacy");
    let p = dir.join("transcript.jsonl");
    let content = r#"{"type":"assistant","content":"legacy ks_0123456789 plus ks_0123456789abcdef and modern ks2_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}
"#;
    std::fs::write(&p, content).unwrap();
    let scan = knapsack::transcript::scan(&p);
    assert!(
        scan.resident.contains("ks_0123456789"),
        "legacy 10-hex collected"
    );
    assert!(
        scan.resident.contains("ks_0123456789abcdef"),
        "legacy 16-hex collected"
    );
    assert!(scan
        .resident
        .contains("ks2_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"));
    let _ = std::fs::remove_dir_all(&dir);
}

// =====================================================================
// 5. Pack on structurally weird sources
// =====================================================================

fn rt(bytes: &[u8], ct: ContentType, tag: &str) {
    let dir = tmp(&format!("weird-{tag}"));
    let store = Store::new(dir.clone());
    let mut ledger = Ledger::in_memory();
    let _ = pack(bytes, ct, &store, &mut ledger, 0);
    if !bytes.is_empty() {
        let back =
            reconstruct(bytes, ct, &store).expect(&format!("{tag}: reconstruct must return Some"));
        assert_eq!(back, bytes, "{tag}: byte-exact recall");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn shebang_only_file() {
    rt(b"#!/bin/sh\n", ContentType::Code, "shebang-only");
}

#[test]
fn license_header_only_file() {
    let bytes = b"// Copyright 2026 example.com\n// SPDX-License-Identifier: MIT\n// All rights reserved.\n";
    rt(bytes, ContentType::Code, "license-header-only");
}

#[test]
fn only_imports_no_definitions() {
    // Many `use` lines, no fn/struct/etc — splitter falls back to blank-line
    let bytes = b"use std::fs;\nuse std::path::Path;\nuse std::collections::HashMap;\nuse std::sync::Arc;\n";
    rt(bytes, ContentType::Code, "imports-only");
}

#[test]
fn only_comments_no_code() {
    let bytes = b"// just a comment\n// another comment\n// yet another\n";
    rt(bytes, ContentType::Code, "comments-only");
}

#[test]
fn one_function_no_body() {
    rt(b"fn empty() {}\n", ContentType::Code, "empty-fn");
}

#[test]
fn one_line_no_newline() {
    rt(
        b"some content without a trailing newline",
        ContentType::Code,
        "no-newline",
    );
    rt(
        b"some content without a trailing newline",
        ContentType::Log,
        "no-newline-log",
    );
}

#[test]
fn ten_thousand_blank_lines() {
    let bytes = "\n".repeat(10_000).into_bytes();
    rt(&bytes, ContentType::Code, "10k-blank");
}

#[test]
fn shebang_then_giant_function() {
    let mut s = String::from("#!/usr/bin/env rust-script\n\nfn main() {\n");
    for i in 0..500 {
        s.push_str(&format!("    let x{i} = {i};\n"));
    }
    s.push_str("}\n");
    rt(s.as_bytes(), ContentType::Code, "shebang-giant-fn");
}

#[test]
fn nested_json_arrays_pack_and_recall() {
    let bytes = br#"{"data":[[[1,2,3],[4,5,6]],[[7,8,9],[10,11,12]]],"meta":{"v":1}}"#;
    rt(bytes, ContentType::Json, "nested-arrays");
}

#[test]
fn json_with_only_one_top_level_member() {
    let bytes = br#"{"single":"value"}"#;
    rt(bytes, ContentType::Json, "single-member");
}

#[test]
fn json_with_unicode_escapes_in_values() {
    let bytes = b"{\"caf\\u00e9\":\"\\u4e2d\\u6587\\u4e16\\u754c\",\"emoji\":\"\\ud83c\\udf0d\"}";
    rt(bytes, ContentType::Json, "unicode-escapes");
}

// =====================================================================
// 6. api::pack_output + expand_handle cross-session attribution chain
// =====================================================================

#[test]
fn cross_session_attribution_chain_via_api() {
    // Pack in session A → blocks stamped with session=A in their .meta
    // Expand from session B → metric attributed to A (originator), not B.
    // This is the "cross-process billing coherence" property documented in
    // store.rs::with_session.
    let dir = tmp("xsess-api");
    std::env::set_var("KNAPSACK_STORE", dir.join("store"));
    std::env::set_var("KNAPSACK_SESSIONS", dir.join("sessions"));
    std::env::set_var("KNAPSACK_METRICS", dir.join("metrics.jsonl"));

    let payload = b"cross-session test content\nlinetwo\nlinethree\n".repeat(20);
    // Warm pack so the whole-buffer handle is stored.
    pack_output(PackRequest {
        session_id: "producer".into(),
        command: None,
        bytes: payload.to_vec(),
        content_hint: Some(ContentType::Log),
        step: 0,
        transcript_path: None,
    });
    pack_output(PackRequest {
        session_id: "producer".into(),
        command: None,
        bytes: payload.to_vec(),
        content_hint: Some(ContentType::Log),
        step: 1,
        transcript_path: None,
    });

    let h = format!("ks2_{}", &knapsack::sha256::sha256_hex(&payload)[..32]);
    let _ = expand_handle(ExpandRequest {
        handle: h,
        range: None,
        grep: None,
        context: 0,
        session_id: "consumer".into(),
        caller: ExpandCaller::Cli,
    });

    // Last expand event in metrics MUST be attributed to "producer", not "consumer".
    let metrics = std::fs::read_to_string(dir.join("metrics.jsonl")).unwrap();
    let last_expand_line = metrics
        .lines()
        .filter(|l| l.contains("\"event\":\"expand\""))
        .last()
        .unwrap();
    assert!(last_expand_line.contains(r#""session":"producer""#),
        "refetch attributed to originating session 'producer', not caller 'consumer'; line:\n{last_expand_line}");
    assert!(
        !last_expand_line.contains(r#""session":"consumer""#),
        "refetch must NOT be attributed to the caller session"
    );

    std::env::remove_var("KNAPSACK_STORE");
    std::env::remove_var("KNAPSACK_SESSIONS");
    std::env::remove_var("KNAPSACK_METRICS");
    let _ = std::fs::remove_dir_all(&dir);
}
