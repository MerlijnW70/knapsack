//! Dogfood the lib.rs public API directly. Every PackRequest field, every
//! ExpandRequest field, every cross-session attribution edge. The CLI is one
//! caller of this API; other callers (a future plugin, a custom hook, an
//! agent SDK) must see the same contract — so we test through the API itself.
//!
//! Parallel-safe via the shared `common::EnvSandbox` helper: each test holds the
//! process-global env lock for its duration, so the standard `KNAPSACK_*` paths
//! point at a per-test temp dir even when cargo parallelizes the binary.

mod common;
use common::EnvSandbox;

use knapsack::api::{evict, expand_handle, pack_output, record_residency, ExpandRequest, PackRequest};
use knapsack::content_type::ContentType;
use knapsack::recall::RecallOut;
use std::path::PathBuf;

// ---------- pack_output: every PackRequest field ----------

#[test]
fn pack_output_with_minimal_request() {
    let _sb = EnvSandbox::new("pack-minimal");
    let r = pack_output(PackRequest {
        session_id: "min".into(),
        command: None,
        bytes: b"some output\nline two\nline three\n".to_vec(),
        content_hint: None, // let detect() figure it out
        step: 0,
        transcript_path: None,
    });
    assert!(r.raw_tokens_est > 0);
    assert!(!r.view.is_empty());
    assert_eq!(r.delta_hits, 0, "first pack -> no delta hits");
}

#[test]
fn pack_output_content_hint_overrides_detection() {
    let _sb = EnvSandbox::new("pack-hint");
    // Bytes look like log but we declare them as Code — the structural strategy
    // should follow our hint, not the heuristic.
    let bytes = b"line one\nline two with error\nline three\nline four\n".repeat(20);
    let r_log = pack_output(PackRequest {
        session_id: "hint-log".into(),
        command: None,
        bytes: bytes.to_vec(),
        content_hint: Some(ContentType::Log),
        step: 0,
        transcript_path: None,
    });
    let r_code = pack_output(PackRequest {
        session_id: "hint-code".into(),
        command: None,
        bytes: bytes.to_vec(),
        content_hint: Some(ContentType::Code),
        step: 0,
        transcript_path: None,
    });
    // Same bytes, different content types -> different block splittings -> different views.
    assert!(
        r_log.view != r_code.view || r_log.blocks != r_code.blocks,
        "hint must actually affect strategy"
    );
}

#[test]
fn pack_output_same_session_twice_hits_delta() {
    let _sb = EnvSandbox::new("pack-delta");
    let payload = b"identical output across two packs\nlinetwo\nlinethree\n".repeat(20);

    let r1 = pack_output(PackRequest {
        session_id: "delta".into(),
        command: None,
        bytes: payload.to_vec(),
        content_hint: None,
        step: 0,
        transcript_path: None,
    });
    let r2 = pack_output(PackRequest {
        session_id: "delta".into(),
        command: None,
        bytes: payload.to_vec(),
        content_hint: None,
        step: 1,
        transcript_path: None,
    });

    assert_eq!(r1.delta_hits, 0, "cold pack");
    assert!(r2.delta_hits > 0, "warm pack must record delta hits");
    assert!(r2.shown_tokens_est <= r1.shown_tokens_est, "warm view shouldn't grow");
}

#[test]
fn pack_output_different_sessions_isolate() {
    let _sb = EnvSandbox::new("pack-isolate");
    let payload = b"isolation test content\nlinetwo\nlinethree\n".repeat(20);

    let r1 = pack_output(PackRequest {
        session_id: "session-a".into(),
        command: None,
        bytes: payload.to_vec(),
        content_hint: None,
        step: 0,
        transcript_path: None,
    });
    // Second pack in a DIFFERENT session must NOT see the first session's ledger
    let r2 = pack_output(PackRequest {
        session_id: "session-b".into(),
        command: None,
        bytes: payload.to_vec(),
        content_hint: None,
        step: 0,
        transcript_path: None,
    });
    assert_eq!(r1.delta_hits, 0);
    assert_eq!(r2.delta_hits, 0, "session-b is cold even though bytes match session-a");
}

#[test]
fn pack_output_command_label_is_metadata_only() {
    // The command label is stored in metrics + meta sidecars for attribution;
    // it doesn't affect the compression result. Pack the same bytes with two
    // different command labels and check the view is identical.
    let _sb = EnvSandbox::new("pack-cmd-label");
    let payload = b"output for cmd-label test\nlinetwo\nlinethree\n".repeat(20);
    let r1 = pack_output(PackRequest {
        session_id: "cmd-label-1".into(),
        command: Some("cargo test".into()),
        bytes: payload.to_vec(),
        content_hint: Some(ContentType::Log),
        step: 0,
        transcript_path: None,
    });
    let r2 = pack_output(PackRequest {
        session_id: "cmd-label-2".into(),
        command: Some("pytest -xvs".into()),
        bytes: payload.to_vec(),
        content_hint: Some(ContentType::Log),
        step: 0,
        transcript_path: None,
    });
    // Same bytes, same content type, different session — views should be identical
    // (the command label doesn't shape compression).
    assert_eq!(r1.view, r2.view, "command label must not affect view");
}

#[test]
fn pack_output_empty_bytes() {
    let _sb = EnvSandbox::new("pack-empty");
    let r = pack_output(PackRequest {
        session_id: "empty".into(),
        command: None,
        bytes: vec![],
        content_hint: None,
        step: 0,
        transcript_path: None,
    });
    assert_eq!(r.raw_tokens_est, 0);
    assert_eq!(r.shown_tokens_est, 0);
    assert_eq!(r.blocks, 0);
}

#[test]
fn pack_output_with_missing_transcript_safe_fallback() {
    let _sb = EnvSandbox::new("pack-missing-trans");
    let r = pack_output(PackRequest {
        session_id: "trans-missing".into(),
        command: None,
        bytes: b"some output\nline two\nline three\n".repeat(20),
        content_hint: None,
        step: 0,
        transcript_path: Some(PathBuf::from("/no/such/transcript.jsonl")),
    });
    // Must succeed (safe fallback to ledger-only).
    assert!(r.raw_tokens_est > 0);
}

// ---------- expand_handle: every ExpandRequest field ----------

#[test]
fn expand_handle_full_recall_is_byte_exact() {
    let _sb = EnvSandbox::new("expand-full");
    let payload = b"recall test content\nlinetwo\nlinethree\n".repeat(20);
    pack_output(PackRequest {
        session_id: "exp-full".into(),
        command: None,
        bytes: payload.to_vec(),
        content_hint: Some(ContentType::Log),
        step: 0,
        transcript_path: None,
    });
    // The whole-buffer handle exists after a warm pack. Compute it directly.
    let h = format!("ks2_{}", &knapsack::sha256::sha256_hex(&payload)[..32]);
    // Force the whole-buffer to be in the store by warm-repacking.
    pack_output(PackRequest {
        session_id: "exp-full".into(),
        command: None,
        bytes: payload.to_vec(),
        content_hint: Some(ContentType::Log),
        step: 1,
        transcript_path: None,
    });

    let out = expand_handle(ExpandRequest {
        handle: h.clone(),
        range: None,
        grep: None,
        context: 0,
        session_id: "exp-caller".into(),
    });
    match out {
        Some(RecallOut::Bytes(b)) => assert_eq!(b, payload, "full recall must be byte-exact"),
        Some(RecallOut::Text(_)) => panic!("full recall (no range/grep) must return Bytes"),
        None => panic!("handle {h} must resolve after warm pack"),
    }
}

#[test]
fn expand_handle_unknown_returns_none() {
    let _sb = EnvSandbox::new("expand-unknown");
    let out = expand_handle(ExpandRequest {
        handle: "ks2_00000000000000000000000000000000".into(),
        range: None,
        grep: None,
        context: 0,
        session_id: "x".into(),
    });
    assert!(out.is_none());
}

#[test]
fn expand_handle_records_refetch_metric() {
    let sb = EnvSandbox::new("expand-metric");
    let payload = b"metric attribution test\nlinetwo\nlinethree\n".repeat(20);
    pack_output(PackRequest {
        session_id: "exp-metric".into(),
        command: None,
        bytes: payload.to_vec(),
        content_hint: Some(ContentType::Log),
        step: 0,
        transcript_path: None,
    });
    pack_output(PackRequest {
        session_id: "exp-metric".into(),
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
        session_id: "different-caller".into(),
    });

    // metrics.jsonl should now contain an expand event.
    let metrics = std::fs::read_to_string(sb.join("metrics.jsonl")).unwrap();
    assert!(metrics.contains(r#""event":"expand""#), "expand event recorded");
}

// ---------- record_residency / evict programmatic ledger ----------

#[test]
fn record_residency_then_evict_round_trip() {
    let _sb = EnvSandbox::new("ledger-api");
    let session = "ledger-roundtrip";
    let handle = "ks2_deadbeefdeadbeefdeadbeefdeadbeef";

    record_residency(session, &handle.to_string(), 1);
    // Load the ledger from disk and confirm the handle is Resident
    let ledger = knapsack::Ledger::load(knapsack::config::session_path(session));
    assert_eq!(ledger.residency(&handle.to_string()), knapsack::Residency::Resident);

    evict(session, &handle.to_string());
    let ledger2 = knapsack::Ledger::load(knapsack::config::session_path(session));
    assert_eq!(ledger2.residency(&handle.to_string()), knapsack::Residency::Evicted);
}

#[test]
fn record_residency_creates_session_file_if_missing() {
    let _sb = EnvSandbox::new("ledger-create");
    let session = "newsess";
    let handle = "ks2_cafef00dcafef00dcafef00dcafef00d";
    record_residency(session, &handle.to_string(), 0);
    let p = knapsack::config::session_path(session);
    assert!(p.exists(), "ledger file must be created on first note");
}

#[test]
fn evict_on_unknown_session_silently_creates_then_no_ops() {
    let _sb = EnvSandbox::new("evict-unknown");
    let handle = "ks2_abcabcabcabcabcabcabcabcabcabcab";
    // Evict on a session that has no ledger. Current behavior: Ledger::load
    // returns an empty ledger, evict() on missing handle is a no-op, save()
    // writes an empty ledger file.
    evict("ghost-session", &handle.to_string());
    let p = knapsack::config::session_path("ghost-session");
    // The file should exist (save wrote it) but contain no entries.
    assert!(p.exists());
    let content = std::fs::read_to_string(&p).unwrap();
    assert!(content.is_empty(), "ledger from empty in-memory state is empty file");
}
