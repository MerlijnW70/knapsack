//! Round-10: disk-failure injection.
//!
//! Pin the "philosophy" of the round-10 brief end-to-end:
//!   - No panic on any disk failure mode.
//!   - Hook paths fail open (compression best-effort; never block the user's tool).
//!   - CLI paths give non-zero exits when recall would be unsafe (`expand` on a
//!     missing handle is exit 1, on a malformed handle is exit 2).
//!   - No corrupt partial state is ever served as valid recall (verify-on-read
//!     is the load-bearing check; meta is additive).
//!
//! Existing coverage in `tests/mcp_cli_symmetry_and_disk.rs` and
//! `tests/store_corruption.rs` covers the basic disk-failure shapes; this file
//! adds the cross-cutting *contract* tests they don't pin individually:
//!   - "broken store + valid pack" produces a view + a handle that simply
//!     returns None on expand — never returns wrong bytes.
//!   - "meta lost after block written" (the non-atomic 2-phase window) still
//!     resolves byte-exact via hash-only verification.
//!   - "metrics path is a directory" silently drops the write but never panics
//!     and never poisons a subsequent `summary()`.
//!   - CLI `expand` exit-code contract for unsafe-recall paths.

mod common;
use common::EnvSandbox;

use knapsack::api::{expand_handle, pack_output, ExpandRequest, PackRequest};
use knapsack::content_type::ContentType;
use knapsack::meta;
use knapsack::metrics;
use knapsack::store::Store;
use std::path::PathBuf;

// =====================================================================
// 1. Store + meta: broken disk never serves wrong bytes
// =====================================================================

/// Make a "store directory" that is actually a regular file. Any `fs::write`
/// inside it returns ENOTDIR / ERROR_DIRECTORY. Portable across Windows + Unix.
fn broken_store_dir(sb: &EnvSandbox) -> PathBuf {
    let p = sb.join("store-as-file");
    std::fs::write(&p, b"i pretend to be a directory").unwrap();
    p
}

#[test]
fn store_put_to_broken_dir_returns_handle_but_get_returns_none() {
    // Contract: `store.put` is silent on write failure (let _ = fs::write).
    // It STILL returns the computed handle so callers can build a view.
    // `get` must then return None — never serve bytes that didn't actually
    // get persisted. This is the "no false-positive recall" invariant.
    let sb = EnvSandbox::new("store-broken-put");
    let broken = broken_store_dir(&sb);
    let store = Store::new(broken);
    let payload = b"some bytes that will never make it to disk";
    let h = store.put(payload);
    assert!(h.starts_with("ks2_"), "handle still computed");
    assert_eq!(
        store.get(&h),
        None,
        "broken store must NOT serve recalled bytes (would be a false positive)"
    );
}

#[test]
fn pack_output_on_broken_store_succeeds_in_view_recall_returns_none() {
    // End-to-end version of the contract above: pack_output returns a
    // compressed view (the model gets some compression benefit), but the
    // handles named in that view simply don't resolve via expand. The
    // user/model gets a clear "no such handle" — never a wrong-bytes recall.
    let mut sb = EnvSandbox::new("pack-broken-store");
    let broken = broken_store_dir(&sb);
    sb.set("KNAPSACK_STORE", &broken);

    let payload = b"line one\nline two\nline three\n".repeat(40);
    let r = pack_output(PackRequest {
        session_id: "broken".into(),
        command: None,
        bytes: payload.to_vec(),
        content_hint: Some(ContentType::Log),
        step: 0,
        transcript_path: None,
    });
    assert!(r.raw_tokens_est > 0, "pack still ran in-memory");
    assert!(!r.view.is_empty(), "view still emitted");

    // The whole-buffer handle pack_output would have stored:
    let h = format!("ks2_{}", &knapsack::sha256::sha256_hex(&payload)[..32]);
    let out = expand_handle(ExpandRequest {
        handle: h,
        range: None,
        grep: None,
        context: 0,
        session_id: "broken".into(),
    });
    assert!(
        out.is_none(),
        "broken store -> expand returns None (no false-positive recall)"
    );
}

#[test]
fn block_persisted_but_meta_write_fails_get_still_byte_exact() {
    // Pin the two-phase block+meta contract. We write the block normally,
    // then delete just the meta sidecar to simulate the failure window
    // (block written, meta-write crashed). hash::verify against the handle
    // is the load-bearing check; meta is additive. get() MUST still resolve.
    let sb = EnvSandbox::new("meta-vanished");
    let store = Store::new(sb.join("store"));
    let payload = b"block survives without meta sidecar\n".repeat(10);
    let h = store.put(&payload);
    // Locate the block + sidecar in the sharded layout. We rebuild the
    // exact path the store would have used.
    let hash_start = h.find('_').unwrap() + 1;
    let shard = &h[hash_start..hash_start + 2];
    let block_path = sb.join("store").join(shard).join(&h);
    assert!(
        block_path.exists(),
        "block must exist before meta-loss simulation"
    );
    let meta_path = meta::meta_path(&block_path);
    if meta_path.exists() {
        std::fs::remove_file(&meta_path).unwrap();
    }
    // Now meta is gone but the block remains. get() must use hash-only
    // verification and return the bytes byte-exact.
    let recalled = store.get(&h).expect("hash-only path must still resolve");
    assert_eq!(
        recalled, payload,
        "byte-exact recall via hash commitment alone"
    );
}

#[test]
fn block_persisted_but_meta_lies_get_rejects() {
    // Inverse: meta exists but disagrees with the block (a corruption shape
    // that DID happen pre-store-fix). get() must reject — hash-verify might
    // still pass (handle is only 128 bits) but meta's full 256-bit length+sha
    // strengthens the commitment. With a hand-crafted lying meta, get() must
    // refuse to serve the bytes. Pin this: meta-as-additional-strength.
    let sb = EnvSandbox::new("meta-lies");
    let store = Store::new(sb.join("store"));
    let payload = b"truth bytes\n".repeat(50);
    let h = store.put(&payload);

    let hash_start = h.find('_').unwrap() + 1;
    let shard = &h[hash_start..hash_start + 2];
    let block_path = sb.join("store").join(shard).join(&h);
    let meta_path = meta::meta_path(&block_path);
    assert!(meta_path.exists(), "meta written for ks2_ block");

    // Overwrite the meta with valid JSON whose sha256 names *different* bytes
    // — same length to pass the cheap len check, different sha to trip
    // matches() at the expensive check. (A malformed meta would just fail
    // from_json and fall through to hash-only verify, which would pass —
    // that's the "meta-as-additive-strength" contract, tested separately.)
    let lie = b"liar".repeat(payload.len() / 4);
    let lie_sha = knapsack::sha256::sha256_hex(&lie);
    let lying_meta = format!(
        r#"{{"sha256":"{}","len":{},"created":0,"accessed":0}}"#,
        lie_sha,
        payload.len()
    );
    std::fs::write(&meta_path, lying_meta).unwrap();

    // get() must NOT serve the bytes — meta says "this isn't what was stored".
    assert_eq!(
        store.get(&h),
        None,
        "valid-JSON lying meta must veto recall even though hash::verify would pass"
    );
}

// =====================================================================
// 2. Metrics: write-failure swallow, no panic, no poisoned subsequent reads
// =====================================================================

#[test]
fn metrics_record_to_directory_path_does_not_panic_subsequent_summary_clean() {
    // Point KNAPSACK_METRICS at a directory. record_compress must silently
    // swallow the open-as-file failure (no panic). A subsequent summary()
    // must return clean zeros — the metrics file is effectively absent.
    let mut sb = EnvSandbox::new("metrics-as-dir");
    let bad = sb.join("metrics-is-a-dir");
    std::fs::create_dir_all(&bad).unwrap();
    sb.set("KNAPSACK_METRICS", &bad);

    // 100 record calls; none should panic.
    for i in 0..100 {
        metrics::record_compress(&format!("sess-{i}"), 100, 50, 50, 1, 0);
    }
    let s = metrics::summary();
    // metrics::summary() reads the file. If the path is a directory, read
    // fails and we get clean zeros — never half-state.
    assert_eq!(
        s.compress_events, 0,
        "directory-as-metrics path produces zero events, not a panic or half-state"
    );
}

#[test]
fn metrics_record_to_unwritable_subdir_swallows() {
    // KNAPSACK_METRICS points at a file inside a directory that doesn't
    // exist and can't be created (it's already a regular file). The OpenOptions
    // chain must fail; the let-_-swallow holds.
    let mut sb = EnvSandbox::new("metrics-no-parent");
    // Parent is a regular file, so creating a file UNDER it fails.
    let parent_as_file = sb.join("nope");
    std::fs::write(&parent_as_file, b"i am a file").unwrap();
    sb.set("KNAPSACK_METRICS", parent_as_file.join("metrics.jsonl"));

    for _ in 0..10 {
        metrics::record_compress("x", 100, 50, 50, 0, 0);
    }
    // summary() can't read the path either; clean zeros.
    let s = metrics::summary();
    assert_eq!(s.compress_events, 0);
}

// =====================================================================
// 3. CLI contract: expand on unsafe-recall paths gives non-zero exit
// =====================================================================
//
// We assert via the api::expand_handle return shape: the CLI dispatches None
// -> exit(1) (main.rs:252-255). We test the api directly so the test stays
// in-process and parallel-safe; the CLI mapping is one-liner glue.

#[test]
fn api_expand_handle_returns_none_for_unknown_handle_in_clean_store() {
    let _sb = EnvSandbox::new("expand-unknown-clean");
    let out = expand_handle(ExpandRequest {
        handle: "ks2_00000000000000000000000000000000".into(),
        range: None,
        grep: None,
        context: 0,
        session_id: "x".into(),
    });
    assert!(
        out.is_none(),
        "unknown handle in a healthy store must return None — CLI surfaces this as exit 1"
    );
}

#[test]
fn api_expand_handle_returns_none_for_known_handle_on_broken_store() {
    // The cross-cutting contract: even if the user / hook BELIEVES this
    // handle was stored (because pack_output returned it), if the store
    // can't serve the bytes, expand returns None. CLI exits 1. No silent
    // pretend-success.
    let mut sb = EnvSandbox::new("expand-broken-store");
    let broken = broken_store_dir(&sb);
    sb.set("KNAPSACK_STORE", &broken);
    // Even a "correct" handle for some payload won't resolve.
    let payload = b"recall-on-broken-store test";
    let h = knapsack::hash::handle(payload);
    let out = expand_handle(ExpandRequest {
        handle: h,
        range: None,
        grep: None,
        context: 0,
        session_id: "x".into(),
    });
    assert!(out.is_none());
}

// =====================================================================
// 4. Idempotence under repeated broken-disk write
// =====================================================================

#[test]
fn repeated_put_to_broken_store_doesnt_corrupt_state_for_later_recovery() {
    // A user might fix the disk problem and try again. We simulate this:
    // 100 puts to a broken store path (all silently fail). Then we swap the
    // store path to a healthy directory and pack the same bytes — expand
    // must resolve.
    let mut sb = EnvSandbox::new("recovery");
    let broken = broken_store_dir(&sb);
    sb.set("KNAPSACK_STORE", &broken);
    let payload = b"recovery payload\n".repeat(20);

    for _ in 0..100 {
        let _ = pack_output(PackRequest {
            session_id: "rec".into(),
            command: None,
            bytes: payload.to_vec(),
            content_hint: Some(ContentType::Log),
            step: 0,
            transcript_path: None,
        });
    }

    // Swap to a real store dir
    let healthy = sb.join("healthy-store");
    sb.set("KNAPSACK_STORE", &healthy);
    let r = pack_output(PackRequest {
        session_id: "rec".into(),
        command: None,
        bytes: payload.to_vec(),
        content_hint: Some(ContentType::Log),
        step: 1,
        transcript_path: None,
    });
    assert!(r.raw_tokens_est > 0);

    let h = format!("ks2_{}", &knapsack::sha256::sha256_hex(&payload)[..32]);
    let out = expand_handle(ExpandRequest {
        handle: h,
        range: None,
        grep: None,
        context: 0,
        session_id: "rec".into(),
    });
    assert!(out.is_some(), "after store path swap, recall works");
}
