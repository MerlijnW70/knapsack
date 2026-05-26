//! Transcript-driven residency: the safety net that prevents dangling backrefs
//! when Claude Code resets the context (/clear, compaction, session restart).
//!
//! Fixtures are inline JSONL — defensive on purpose because the real Claude Code
//! transcript schema may shift across versions. We test the contract end-to-end
//! through `pack_with_transcript` (the in-memory entry point that bypasses fs/env
//! plumbing), so failures point at the residency-gate logic itself, not at the
//! transport layer.

use knapsack::content_type::ContentType;
use knapsack::ledger::Ledger;
use knapsack::pack::{pack, pack_with_transcript};
use knapsack::store::Store;
use knapsack::transcript;
use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};

fn tmp_dir(tag: &str) -> PathBuf {
    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    let d = std::env::temp_dir().join(format!("knapsack-transcript-{}-{}-{}", tag, std::process::id(), t));
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn write_transcript(dir: &Path, name: &str, lines: &[&str]) -> PathBuf {
    let p = dir.join(name);
    let mut f = std::fs::File::create(&p).unwrap();
    for l in lines {
        f.write_all(l.as_bytes()).unwrap();
        f.write_all(b"\n").unwrap();
    }
    p
}

/// Make a content blob of N distinct lines — enough that pack will produce multiple
/// blocks and the ledger has handles to gate.
fn many_lines(n: usize) -> Vec<u8> {
    let mut s = String::new();
    for i in 0..n {
        s.push_str(&format!("plain log line number {i}, some routine content\n"));
    }
    s.into_bytes()
}

// ---- Unit-level: the scanner ----

#[test]
fn scanner_returns_ok_false_for_missing_file() {
    let r = transcript::scan(&PathBuf::from("/does/not/exist/abc.jsonl"));
    assert!(!r.ok, "missing transcript -> ok=false -> caller falls back to ledger");
    assert!(r.resident.is_empty());
    assert!(r.last_boundary.is_none());
}

#[test]
fn scanner_returns_ok_false_for_empty_file() {
    let dir = tmp_dir("empty");
    let p = write_transcript(&dir, "empty.jsonl", &[]);
    let r = transcript::scan(&p);
    assert!(!r.ok, "empty transcript can't prove anything -> fall back");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn scanner_tolerates_corrupt_lines_returns_ok_true() {
    // Real transcripts can have torn writes / forward-compat fields. As long as SOME
    // line is parseable, the scan is usable. ok=true even with garbage lines mixed in.
    let dir = tmp_dir("corrupt");
    let p = write_transcript(
        &dir,
        "corrupt.jsonl",
        &[
            r#"this line is not json at all"#,
            r#"{"role":"assistant","content":"reference to ks2_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa here"}"#,
            r#"{ unterminated json"#,
        ],
    );
    let r = transcript::scan(&p);
    assert!(r.ok, "at least one parseable line -> ok=true");
    assert!(r.resident.contains("ks2_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"), "valid line still contributed");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn scanner_drops_handles_before_clear_keeps_handles_after() {
    let dir = tmp_dir("clear");
    let p = write_transcript(
        &dir,
        "clear.jsonl",
        &[
            r#"{"role":"assistant","content":"ran tests, see ks2_11111111111111111111111111111111"}"#,
            r#"{"role":"user","content":"/clear"}"#,
            r#"{"role":"assistant","content":"fresh start with ks2_22222222222222222222222222222222"}"#,
        ],
    );
    let r = transcript::scan(&p);
    assert!(r.ok);
    assert_eq!(r.last_boundary.map(|(b, _)| b), Some(transcript::Boundary::Clear));
    assert!(!r.resident.contains("ks2_11111111111111111111111111111111"), "pre-/clear handle not resident");
    assert!(r.resident.contains("ks2_22222222222222222222222222222222"), "post-/clear handle resident");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn scanner_detects_compaction_boundary() {
    let dir = tmp_dir("compact");
    let p = write_transcript(
        &dir,
        "compact.jsonl",
        &[
            r#"{"role":"assistant","content":"earlier ks2_33333333333333333333333333333333"}"#,
            r#"{"type":"compact","at":12345}"#,
            r#"{"role":"assistant","content":"after compaction ks2_44444444444444444444444444444444"}"#,
        ],
    );
    let r = transcript::scan(&p);
    assert_eq!(r.last_boundary.map(|(b, _)| b), Some(transcript::Boundary::Compaction));
    assert!(!r.resident.contains("ks2_33333333333333333333333333333333"));
    assert!(r.resident.contains("ks2_44444444444444444444444444444444"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn scanner_collects_handles_when_no_boundary() {
    // Common case: short session, no /clear, no compaction. Everything is resident.
    let dir = tmp_dir("noboundary");
    let p = write_transcript(
        &dir,
        "noboundary.jsonl",
        &[
            r#"{"role":"assistant","content":"first ks2_55555555555555555555555555555555 and"}"#,
            r#"{"role":"assistant","content":"second ks2_66666666666666666666666666666666 too"}"#,
        ],
    );
    let r = transcript::scan(&p);
    assert!(r.ok);
    assert!(r.last_boundary.is_none(), "no boundary detected");
    assert_eq!(r.resident.len(), 2);
    let _ = std::fs::remove_dir_all(&dir);
}

// ---- End-to-end: pack() honours the transcript gate ----

#[test]
fn pack_emits_backref_when_no_transcript_provided() {
    // Baseline: with the gate disabled (None), the original ledger-only flow holds —
    // a second pack call against unchanged bytes back-refs heavily.
    let dir = tmp_dir("pack-nognoring");
    let store = Store::new(dir.join("store"));
    let mut ledger = Ledger::in_memory();
    let bytes = many_lines(80);

    let r1 = pack(&bytes, ContentType::Log, &store, &mut ledger, 0);
    let r2 = pack(&bytes, ContentType::Log, &store, &mut ledger, 1);
    assert_eq!(r1.delta_hits, 0, "cold");
    assert!(r2.delta_hits > 0, "unchanged re-read should backref without transcript gating");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn pack_drops_backrefs_when_transcript_says_handle_is_gone() {
    // Heart of the feature. After pack #1, the ledger holds a bunch of resident
    // handles. We hand pack #2 a transcript_resident set that contains NONE of them
    // (simulating "/clear happened, nothing from before is still in context"). Every
    // block must re-send as new — no backrefs.
    let dir = tmp_dir("pack-clear");
    let store = Store::new(dir.join("store"));
    let mut ledger = Ledger::in_memory();
    let bytes = many_lines(80);

    let _r1 = pack(&bytes, ContentType::Log, &store, &mut ledger, 0);
    let empty: HashSet<String> = HashSet::new(); // transcript says: nothing resident
    let r2 = pack_with_transcript(&bytes, ContentType::Log, &store, &mut ledger, 1, Some(&empty));
    assert_eq!(
        r2.delta_hits, 0,
        "post-/clear: ledger thinks resident, transcript says no -> NO backrefs (correctness > reduction)"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn pack_keeps_backrefs_when_transcript_confirms_residency() {
    // Sanity: when the transcript DOES name the handles, the gate is satisfied and
    // backrefs flow as usual. Otherwise the feature would always cost compression.
    let dir = tmp_dir("pack-confirm");
    let store = Store::new(dir.join("store"));
    let mut ledger = Ledger::in_memory();
    let bytes = many_lines(80);

    let r1 = pack(&bytes, ContentType::Log, &store, &mut ledger, 0);
    // Collect every handle from r1.handles into the resident set so the gate passes
    // for ALL blocks; backref behaviour should now match the no-gate baseline.
    let resident: HashSet<String> = r1.handles.iter().cloned().collect();
    let r2 = pack_with_transcript(&bytes, ContentType::Log, &store, &mut ledger, 1, Some(&resident));
    assert!(
        r2.delta_hits > 0,
        "transcript confirms residency -> backrefs preserved; got delta_hits={}",
        r2.delta_hits
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn pack_view_never_names_a_non_resident_handle() {
    // The actual safety property the user cares about: a backref marker
    // `[Knapsack: ... · recall ks2_…]` in the emitted view MUST point at a handle the
    // transcript confirmed as resident. We don't pin `delta_hits` here because the
    // never-worse-than-stateless guard in pack.rs can legitimately choose to emit
    // zero backrefs (using the stateless view) when partial residency would fragment
    // the conditional view — that's *correct* behaviour, just less compressed. What
    // must never happen is naming a handle the transcript doesn't include.
    let dir = tmp_dir("pack-partial");
    let store = Store::new(dir.join("store"));
    let mut ledger = Ledger::in_memory();
    let bytes = many_lines(80);

    let _r1 = pack(&bytes, ContentType::Log, &store, &mut ledger, 0);

    // Compute the *block-level* handles (the ones residency() actually checks). Put
    // only the first half into the resident set — the other half is "post-/clear".
    let block_handles: Vec<String> = knapsack::block::split_blocks(&bytes, ContentType::Log)
        .iter()
        .map(|&(s, e)| knapsack::hash::handle(&bytes[s..e]))
        .collect();
    let half: HashSet<String> = block_handles.iter().take(block_handles.len() / 2).cloned().collect();

    let r2 = pack_with_transcript(&bytes, ContentType::Log, &store, &mut ledger, 1, Some(&half));

    // Scan the emitted view for every BACKREF marker (the "already in context"
    // residency claim that the transcript gate exists to police). NOT every `recall`
    // mention — structural compressors emit `[Knapsack: N lines elided ... recall …]`
    // markers that just say "the original is recoverable", which is true regardless
    // of residency. Only `[Knapsack: N lines unchanged ... recall <h>]` is the
    // residency assertion we're gating.
    let mut backref_handles = 0usize;
    for line in r2.view.lines() {
        if !line.contains("lines unchanged") {
            continue;
        }
        if let Some(start) = line.find("recall ks") {
            let after = &line[start + "recall ".len()..];
            let h: String = after.chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '_').collect();
            assert!(
                half.contains(&h),
                "compact view names a NON-resident handle in an `already in context` claim: `{}` in line `{}`",
                h,
                line
            );
            backref_handles += 1;
        }
    }
    // Sanity: we either emit zero backref claims (stateless guard won) or only
    // backrefs to resident handles. Both are correct outcomes — the test asserts
    // SAFETY, not compression.
    let _ = backref_handles;
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn fallback_when_scanner_says_ok_false_uses_ledger_only() {
    // The integration-shape test: api::pack_output drops the gate when scan.ok ==
    // false. Here we simulate that path by calling pack_with_transcript with None
    // (what api passes when scan.ok == false). Behaviour must match pack(...) exactly.
    let dir = tmp_dir("pack-fallback");
    let store = Store::new(dir.join("store"));
    let mut ledger_a = Ledger::in_memory();
    let mut ledger_b = Ledger::in_memory();
    let bytes = many_lines(80);

    pack(&bytes, ContentType::Log, &store, &mut ledger_a, 0);
    let a = pack(&bytes, ContentType::Log, &store, &mut ledger_a, 1);

    pack_with_transcript(&bytes, ContentType::Log, &store, &mut ledger_b, 0, None);
    let b = pack_with_transcript(&bytes, ContentType::Log, &store, &mut ledger_b, 1, None);

    assert_eq!(a.delta_hits, b.delta_hits, "None gate must behave identically to pack()");
    assert_eq!(a.shown_tokens_est, b.shown_tokens_est, "compact view sizes must match too");
    let _ = std::fs::remove_dir_all(&dir);
}

// ---- Sanity on the boundary heuristics ----

#[test]
fn handle_dedup_within_resident_set() {
    // The same handle appearing twice in the transcript shouldn't bloat the set.
    let dir = tmp_dir("dedup");
    let p = write_transcript(
        &dir,
        "dedup.jsonl",
        &[
            r#"{"role":"assistant","content":"see ks2_77777777777777777777777777777777"}"#,
            r#"{"role":"assistant","content":"reminder ks2_77777777777777777777777777777777 again"}"#,
        ],
    );
    let r = transcript::scan(&p);
    assert_eq!(r.resident.len(), 1, "deduped handle: {:?}", r.resident);
    let _ = std::fs::remove_dir_all(&dir);
}
