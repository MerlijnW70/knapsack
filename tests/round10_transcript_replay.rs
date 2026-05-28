//! Round-10: realistic transcript replay scenarios.
//!
//! tests/transcript_residency.rs already covers scanner mechanics and the basic
//! pack-with-transcript contract. This file adds the *scenario* tests the brief
//! called for:
//!
//!   1. Multi-boundary cascade: /clear → compaction → restart. Only the most
//!      recent (restart) boundary matters; handles before it are forgotten.
//!   2. Realistic Claude Code message-shape with tool_use entries containing
//!      ks2_ handles in the visible text — the collector picks them up out of
//!      the JSON string content.
//!   3. Long transcript (2000+ lines) finishes in well under a second and
//!      finds all handles correctly.
//!   4. End-to-end: pack → /clear → repack. The second pack MUST NOT emit
//!      backrefs to handles that only appeared before the /clear (this is
//!      the bug the transcript gate exists to prevent — dangling backrefs).
//!   5. `pack_with_transcript` with Some(empty) suppresses ALL backrefs even
//!      when ledger says everything is resident — the transcript-AND-ledger
//!      gate semantics.

mod common;
use common::EnvSandbox;

use knapsack::content_type::ContentType;
use knapsack::ledger::Ledger;
use knapsack::pack::{pack, pack_with_transcript};
use knapsack::store::Store;
use knapsack::transcript;
use std::collections::HashSet;
use std::io::Write;
use std::path::Path;

fn write_transcript(p: &Path, lines: &[&str]) {
    let mut f = std::fs::File::create(p).unwrap();
    for l in lines {
        f.write_all(l.as_bytes()).unwrap();
        f.write_all(b"\n").unwrap();
    }
}

// =====================================================================
// 1. Multi-boundary cascade — only the most recent wins
// =====================================================================

#[test]
fn scanner_cascade_clear_then_compaction_then_restart_only_latest_wins() {
    let sb = EnvSandbox::new("tx-cascade");
    let p = sb.join("cascade.jsonl");
    write_transcript(
        &p,
        &[
            // Pre-clear noise: handles that MUST NOT end up resident.
            r#"{"role":"user","content":"first message"}"#,
            r#"{"role":"assistant","content":"see [Knapsack: 10 lines · recall ks2_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa]"}"#,
            // BOUNDARY 1: /clear
            r#"{"role":"user","content":"/clear"}"#,
            // Post-clear, pre-compaction: also MUST NOT be resident (cleared again later).
            r#"{"role":"assistant","content":"after clear, see ks2_bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"}"#,
            // BOUNDARY 2: compaction
            r#"{"type":"compact","when":1234567890}"#,
            // Post-compaction, pre-restart: also MUST NOT be resident.
            r#"{"role":"assistant","content":"after compact, see ks2_cccccccccccccccccccccccccccccccc"}"#,
            // BOUNDARY 3: session restart (the latest boundary)
            r#"{"type":"session_restart","when":1234567899}"#,
            // Post-restart: THESE are the only handles that should be resident.
            r#"{"role":"assistant","content":"final state, ks2_dddddddddddddddddddddddddddddddd is alive"}"#,
            r#"{"role":"assistant","content":"and so is ks2_eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee"}"#,
        ],
    );

    let r = transcript::scan(&p);
    assert!(r.ok, "scanner runs to completion");
    assert_eq!(
        r.last_boundary.map(|(b, _)| b),
        Some(transcript::Boundary::Restart),
        "the LATEST boundary (restart) is reported, not /clear or compaction"
    );

    // Resident set is exactly {d…, e…}. The a/b/c handles were all wiped.
    assert!(r.resident.contains("ks2_dddddddddddddddddddddddddddddddd"));
    assert!(r.resident.contains("ks2_eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee"));
    assert!(
        !r.resident.contains("ks2_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
        "pre-clear handle MUST NOT be resident"
    );
    assert!(
        !r.resident.contains("ks2_bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"),
        "pre-compaction handle MUST NOT be resident"
    );
    assert!(
        !r.resident.contains("ks2_cccccccccccccccccccccccccccccccc"),
        "pre-restart handle MUST NOT be resident"
    );
    assert_eq!(r.resident.len(), 2);
}

// =====================================================================
// 2. Realistic Claude Code tool-use envelope shape
// =====================================================================

#[test]
fn scanner_picks_up_handles_buried_in_realistic_tool_use_envelope() {
    // Mimic the structure Claude Code uses for assistant messages: tool_use
    // blocks containing the compressed view text. Handles are embedded in
    // escape-encoded JSON strings; the collector scans raw line text so they
    // survive any reasonable JSON encoding.
    let sb = EnvSandbox::new("tx-realistic-shape");
    let p = sb.join("realistic.jsonl");
    write_transcript(
        &p,
        &[
            r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"run the tests"}]}}"#,
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"toolu_01","name":"Bash","input":{"command":"cargo test"}}]}}"#,
            // tool_result with a knapsack view in the text — handle embedded in inner JSON
            r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_01","content":"[knapsack 5240->420 tok · 14 blocks · 12 unchanged]\n[unchanged · recall ks2_11112222333344445555666677778888]\nfailing test details here\n[unchanged · recall ks2_aaaabbbbccccddddeeeeffff00001111]"}]}}"#,
        ],
    );
    let r = transcript::scan(&p);
    assert!(r.ok);
    assert!(r.last_boundary.is_none(), "no boundary in this transcript");
    assert!(
        r.resident.contains("ks2_11112222333344445555666677778888"),
        "handle inside tool_result text is detected"
    );
    assert!(r.resident.contains("ks2_aaaabbbbccccddddeeeeffff00001111"));
    assert_eq!(r.resident.len(), 2);
}

// =====================================================================
// 3. Long transcript performance + correctness
// =====================================================================

#[test]
fn scanner_handles_long_transcript_under_one_second() {
    let sb = EnvSandbox::new("tx-long");
    let p = sb.join("long.jsonl");
    let mut f = std::fs::File::create(&p).unwrap();
    // 2000 lines of noise, with handles sprinkled every 100 lines.
    for i in 0..2000usize {
        if i.is_multiple_of(100) {
            let h = format!("ks2_{:032x}", i);
            writeln!(
                f,
                r#"{{"role":"assistant","content":"line {i}: recall {h}"}}"#
            )
            .unwrap();
        } else {
            writeln!(f, r#"{{"role":"assistant","content":"line {i}: routine"}}"#).unwrap();
        }
    }
    drop(f);
    let start = std::time::Instant::now();
    let r = transcript::scan(&p);
    let elapsed = start.elapsed();
    assert!(
        elapsed.as_secs() < 1,
        "2000-line scan took {elapsed:?}, target < 1 s"
    );
    assert!(r.ok);
    assert_eq!(
        r.resident.len(),
        20,
        "found exactly 20 sprinkled handles (every 100/2000)"
    );
}

#[test]
fn scanner_long_transcript_with_boundary_late_drops_pre_boundary_handles() {
    // 1000 lines with handles every 50; /clear at line 800.
    // Only handles in lines 801..1000 (i.e. 4 sprinkled, at 800,850,900,950)
    // should be resident.
    let sb = EnvSandbox::new("tx-long-with-boundary");
    let p = sb.join("long-clear.jsonl");
    let mut f = std::fs::File::create(&p).unwrap();
    for i in 0..1000usize {
        if i == 800 {
            writeln!(f, r#"{{"role":"user","content":"/clear"}}"#).unwrap();
        } else if i.is_multiple_of(50) {
            let h = format!("ks2_{:032x}", i);
            writeln!(f, r#"{{"role":"assistant","content":"recall {h}"}}"#).unwrap();
        } else {
            writeln!(f, r#"{{"role":"assistant","content":"line {i}"}}"#).unwrap();
        }
    }
    drop(f);
    let r = transcript::scan(&p);
    assert!(r.ok);
    assert_eq!(
        r.last_boundary.map(|(b, _)| b),
        Some(transcript::Boundary::Clear)
    );
    // Handles at i=0,50,100,…,750 (16 handles) are pre-clear → dropped.
    // Handles at i=850,900,950 are post-clear → resident.
    assert_eq!(
        r.resident.len(),
        3,
        "exactly 3 handles after /clear: i=850, 900, 950"
    );
    assert!(r.resident.contains(&format!("ks2_{:032x}", 850)));
    assert!(r.resident.contains(&format!("ks2_{:032x}", 900)));
    assert!(r.resident.contains(&format!("ks2_{:032x}", 950)));
    assert!(
        !r.resident.contains(&format!("ks2_{:032x}", 750)),
        "pre-clear handle must be dropped"
    );
}

// =====================================================================
// 4. End-to-end: pack → /clear → repack never emits dangling backref
// =====================================================================

#[test]
fn end_to_end_pack_then_clear_then_repack_does_not_dangle_backref() {
    // Cold pack with no transcript: ledger learns the handles.
    // Then we hand the SAME bytes back via pack_with_transcript, passing a
    // transcript-resident set that DOESN'T contain those handles (mimicking
    // a transcript where /clear wiped them). The second pack must NOT emit
    // a backref to the now-non-resident handle.
    let sb = EnvSandbox::new("tx-e2e-dangle");
    let store = Store::new(sb.join("store"));
    let mut ledger = Ledger::in_memory();

    let payload = b"shared content\nthat repeats many times\n".repeat(40);

    // Cold pack: warms the ledger
    let cold = pack(&payload, ContentType::Log, &store, &mut ledger, 0);
    assert_eq!(cold.delta_hits, 0, "cold pack has no delta hits");

    // Warm pack with transcript = empty set: ledger says resident, transcript
    // says no. Effective Resident = ledger AND transcript = false. Backrefs
    // must NOT fire.
    let empty: HashSet<String> = HashSet::new();
    let warm_empty_tx = pack_with_transcript(
        &payload,
        ContentType::Log,
        &store,
        &mut ledger,
        1,
        Some(&empty),
    );
    assert_eq!(
        warm_empty_tx.delta_hits, 0,
        "transcript-says-no must override ledger-says-yes (no dangling backref)"
    );

    // Sanity: warm pack WITHOUT a transcript (ledger-only) DOES fire backrefs.
    let warm_no_tx = pack_with_transcript(&payload, ContentType::Log, &store, &mut ledger, 2, None);
    assert!(
        warm_no_tx.delta_hits > 0,
        "warm pack with no transcript falls back to ledger-only → delta hits"
    );
}

#[test]
fn end_to_end_pack_with_full_transcript_match_emits_backref() {
    // Symmetric positive case: warm pack with a transcript that DOES contain
    // the handles → backref emitted (no over-conservatism). Use PackResult
    // .handles to seed transcript_resident — those are the block-level
    // handles the residency gate actually checks (view-text handles include
    // elision sub-handles, which is a different set).
    let sb = EnvSandbox::new("tx-e2e-match");
    let store = Store::new(sb.join("store"));
    let mut ledger = Ledger::in_memory();
    let payload = b"shared content\nthat repeats many times\n".repeat(40);

    // Cold pack to populate ledger and store
    let cold = pack(&payload, ContentType::Log, &store, &mut ledger, 0);
    assert_eq!(cold.delta_hits, 0);

    // The block-level handles the residency gate keys off.
    let transcript_resident: HashSet<String> = cold.handles.iter().cloned().collect();
    assert!(
        !transcript_resident.is_empty(),
        "cold pack must produce at least one block handle"
    );

    // Warm pack with transcript naming the same block handles → delta hits fire
    let warm = pack_with_transcript(
        &payload,
        ContentType::Log,
        &store,
        &mut ledger,
        1,
        Some(&transcript_resident),
    );
    assert!(
        warm.delta_hits > 0,
        "transcript matches ledger → backrefs fire (no over-conservatism); got delta_hits={}",
        warm.delta_hits
    );
}

// =====================================================================
// 5. Pack with realistic on-disk transcript (file path, not in-memory set)
// =====================================================================

#[test]
fn pack_output_with_real_transcript_file_path_drops_post_clear_dangle() {
    // The cross-cutting end-to-end: pack_output's transcript_path arg routes
    // through transcript::scan and gates residency. This is what production
    // hooks use. Pin that the file path → scan → residency gate chain works.
    let _sb = EnvSandbox::new("tx-pack-output");

    let payload = b"shared content\nrepeats many times for blocks\n".repeat(40);

    // First pack with no transcript: cold, warms ledger.
    let cold = knapsack::api::pack_output(knapsack::api::PackRequest {
        session_id: "tx-test".into(),
        command: None,
        bytes: payload.to_vec(),
        content_hint: Some(ContentType::Log),
        step: 0,
        transcript_path: None,
    });
    assert_eq!(cold.delta_hits, 0);

    // Now write a transcript that contains /clear AFTER any handles —
    // empty post-clear means no handles are resident. Save it where
    // pack_output can find it.
    let transcript_path = _sb.join("session.jsonl");
    write_transcript(
        &transcript_path,
        &[
            r#"{"role":"assistant","content":"some early text"}"#,
            r#"{"role":"user","content":"/clear"}"#,
            r#"{"role":"assistant","content":"empty after clear"}"#,
        ],
    );

    // Warm pack WITH this transcript: ledger says resident, transcript says
    // empty post-/clear → no backrefs.
    let warm = knapsack::api::pack_output(knapsack::api::PackRequest {
        session_id: "tx-test".into(),
        command: None,
        bytes: payload.to_vec(),
        content_hint: Some(ContentType::Log),
        step: 1,
        transcript_path: Some(transcript_path),
    });
    assert_eq!(
        warm.delta_hits, 0,
        "real transcript file with /clear must suppress backrefs (no dangling)"
    );
}
