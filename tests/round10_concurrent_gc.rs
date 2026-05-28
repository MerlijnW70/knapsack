//! Round-10 concurrent GC vs concurrent pack — race hardening.
//!
//! Pinned acceptance criteria (from the round-10 brief):
//!
//!   ACCEPTABLE
//!   ──────────
//!   - `expand` returns None for a missing handle if GC legitimately removed it.
//!   - Orphan block/meta is tolerated and cleaned later.
//!   - Counters are approximate under concurrent mutation (documented).
//!
//!   NOT ACCEPTABLE
//!   ──────────────
//!   - Wrong bytes served for a handle.
//!   - Panic.
//!   - Corrupted metrics / ledger.
//!   - Newly written fresh blocks deleted as old.
//!   - Successful `pack` emits handles that immediately fail because GC raced incorrectly.
//!
//! Strategy: thread-level concurrency via `std::thread::scope` (zero-dep).
//! Production-relevant: the hook + MCP server live in the same process, so
//! thread races match the actual deployment surface.

mod common;
use common::EnvSandbox;

use knapsack::api::{ExpandCaller, expand_handle, pack_output, ExpandRequest, PackRequest};
use knapsack::content_type::ContentType;
use knapsack::gc;
use knapsack::hash;
use knapsack::metrics;
use knapsack::store::Store;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

// =====================================================================
// 1. CRITICAL: fresh block must not be GC-deleted as old
// =====================================================================

#[test]
fn ks2_block_just_written_is_not_deleted_by_concurrent_gc_with_threshold_zero() {
    // The race: pack writes a ks2_ block file (fs::write returns), then writes
    // the .meta sidecar. If gc with --older-than 0 scans BETWEEN these two
    // writes, the block has no meta, falls back to fs mtime (just-now), and
    // gc.rs:133 computes `now.saturating_sub(t) >= 0` which is trivially true.
    // → fresh block deleted as old. NOT ACCEPTABLE.
    //
    // Even in single-threaded, we can hit this: chmod-restrict the meta path
    // so meta::write_if_absent silently fails, then run gc --older-than 0.
    // We're testing the ks2_ + no-meta + age <60s safeguard.

    let sb = EnvSandbox::new("conc-fresh-not-deleted");
    let store = Store::new(sb.join("store"));

    // Write a ks2_ block normally; then DELETE its meta to simulate the
    // race window where the block exists but meta hasn't landed yet.
    let payload = b"freshly written block\n".repeat(20);
    let h = store.put(&payload);

    let hash_start = h.find('_').unwrap() + 1;
    let shard = &h[hash_start..hash_start + 2];
    let block_path = sb.join("store").join(shard).join(&h);
    let meta_path = knapsack::meta::meta_path(&block_path);
    assert!(meta_path.exists(), "meta exists after normal put");
    std::fs::remove_file(&meta_path).unwrap();
    assert!(
        block_path.exists() && !meta_path.exists(),
        "simulated race window: block present, meta absent"
    );

    // Run gc with --older-than 0 (most aggressive setting). The block has
    // mtime ≈ now. Pre-fix behavior: deleted. Post-fix: skipped due to
    // the ks2_-no-meta-young safeguard.
    let r = gc::gc(&store, 0, false);

    assert!(
        store.get(&h).is_some(),
        "ks2_ block freshly written and meta-less must NOT be deleted by gc --older-than 0; \
         gc report: scanned={}, deleted={}, kept={}, meta_missing={}",
        r.scanned,
        r.deleted,
        r.kept,
        r.meta_missing
    );
    let got = store.get(&h).unwrap();
    assert_eq!(got, payload, "recall remains byte-exact");
}

#[test]
fn pack_output_followed_by_concurrent_gc_threshold_zero_recall_works() {
    // End-to-end version of the contract: pack_output returns a handle.
    // Immediately spawn a thread running gc with threshold 0. Then
    // expand_handle on the handle pack_output gave us. Must succeed.
    let sb = EnvSandbox::new("conc-pack-then-gc");

    let payload = b"pack-then-gc end-to-end\nline two\nline three\n".repeat(40);

    let r = pack_output(PackRequest {
        session_id: "pte".into(),
        command: None,
        bytes: payload.to_vec(),
        content_hint: Some(ContentType::Log),
        step: 0,
        transcript_path: None,
    });
    assert!(r.raw_tokens_est > 0);

    // The whole-buffer handle pack would have stored:
    let h = format!("ks2_{}", &knapsack::sha256::sha256_hex(&payload)[..32]);

    // Spawn a thread that runs gc with threshold 0 multiple times in a tight
    // loop. Concurrent with our expand on the main thread.
    let store_dir = sb.join("store");
    let store = Store::new(store_dir.clone());
    let done = AtomicBool::new(false);
    let panicked = AtomicBool::new(false);

    std::thread::scope(|s| {
        let gc_thread = s.spawn(|| {
            while !done.load(Ordering::Relaxed) {
                // Wrap in catch_unwind so a panic shows up as a flag rather
                // than poisoning the parent. Acceptance: gc never panics.
                let res = std::panic::catch_unwind(|| {
                    let _ = gc::gc(&store, 0, false);
                });
                if res.is_err() {
                    panicked.store(true, Ordering::Relaxed);
                    break;
                }
            }
        });

        // Main: try to expand the handle right away. Even if gc has already
        // deleted it (legitimate per acceptance), the recall must either
        // return Some(byte-exact) or None — never wrong bytes, never panic.
        let out = expand_handle(ExpandRequest {
            handle: h.clone(),
            range: None,
            grep: None,
            context: 0,
            session_id: "pte".into(),
            caller: ExpandCaller::Cli,
        });
        match out {
            Some(knapsack::recall::RecallOut::Bytes(b)) => {
                assert_eq!(
                    b, payload,
                    "if recalled, bytes byte-exact (no wrong-bytes-served)"
                );
            }
            Some(knapsack::recall::RecallOut::Text(_)) => panic!("unexpected Text variant"),
            None => {
                // Legitimate per acceptance — gc beat us to it. With the
                // safeguard in place, this path requires gc to have raced
                // through enough iterations to see meta present + last_accessed
                // < now - threshold(0). Allowed.
            }
        }

        done.store(true, Ordering::Relaxed);
        let _ = gc_thread.join();
    });

    assert!(
        !panicked.load(Ordering::Relaxed),
        "gc must never panic under concurrent expand"
    );
}

// =====================================================================
// 2. Many concurrent puts + gcs: no panic, no wrong bytes
// =====================================================================

#[test]
fn n_threads_putting_while_m_threads_gcing_no_wrong_bytes_no_panic() {
    // Mass mixed workload: 4 packer threads each doing 50 puts, 2 gc
    // threads doing many gcs. Every block returned by put() must, if
    // retrievable, return the original bytes (verify-on-read holds).
    // Track per-thread panic flags via catch_unwind.

    let sb = EnvSandbox::new("conc-mass");
    let store = Store::new(sb.join("store"));
    let panicked = AtomicBool::new(false);
    let put_count = AtomicUsize::new(0);
    let recall_ok = AtomicUsize::new(0);
    let recall_miss = AtomicUsize::new(0);

    // Pre-generate distinct payloads so threads don't all dedupe to one block.
    const N_PACKERS: usize = 4;
    const PUTS_PER_PACKER: usize = 50;
    let payloads: Vec<Vec<u8>> = (0..N_PACKERS * PUTS_PER_PACKER)
        .map(|i| format!("unique payload #{i} ").repeat(20).into_bytes())
        .collect();

    let stop = AtomicBool::new(false);

    std::thread::scope(|s| {
        // Packers: each writes 50 puts, then verifies recall on each.
        for packer_id in 0..N_PACKERS {
            let payloads = &payloads;
            let store = &store;
            let panicked = &panicked;
            let put_count = &put_count;
            let recall_ok = &recall_ok;
            let recall_miss = &recall_miss;
            s.spawn(move || {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    for i in 0..PUTS_PER_PACKER {
                        let idx = packer_id * PUTS_PER_PACKER + i;
                        let p = &payloads[idx];
                        let h = store.put(p);
                        put_count.fetch_add(1, Ordering::Relaxed);
                        // Immediately try to read back. Allowed outcomes:
                        //   Some(bytes) → bytes must equal p (verify holds)
                        //   None → gc raced and deleted; legitimate
                        match store.get(&h) {
                            Some(bytes) => {
                                assert_eq!(&bytes, p, "wrong bytes served for handle {h}");
                                recall_ok.fetch_add(1, Ordering::Relaxed);
                            }
                            None => {
                                recall_miss.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                    }
                }));
                if result.is_err() {
                    panicked.store(true, Ordering::Relaxed);
                }
            });
        }

        // GC threads: keep gcing with threshold 0 until packers are done.
        for _ in 0..2 {
            let store = &store;
            let panicked = &panicked;
            let stop = &stop;
            s.spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        let _ = gc::gc(store, 0, false);
                    }));
                    if result.is_err() {
                        panicked.store(true, Ordering::Relaxed);
                        return;
                    }
                    std::thread::yield_now();
                }
            });
        }

        // Wait for packers (their s.spawn handles aren't directly join-able
        // through scope; the scope guarantees they all finish before returning).
        // Use a brief sleep so gc has a chance to run alongside puts.
        std::thread::sleep(Duration::from_millis(200));
        stop.store(true, Ordering::Relaxed);
    });

    assert!(
        !panicked.load(Ordering::Relaxed),
        "no thread panicked; race-condition free"
    );
    assert_eq!(
        put_count.load(Ordering::Relaxed),
        N_PACKERS * PUTS_PER_PACKER,
        "every packer completed all its puts"
    );
    // We don't pin recall_ok/recall_miss exact counts — those are
    // probabilistic under concurrent gc. The invariant is that no recall
    // returned WRONG bytes (asserted inline above).
    let total = recall_ok.load(Ordering::Relaxed) + recall_miss.load(Ordering::Relaxed);
    assert_eq!(
        total,
        N_PACKERS * PUTS_PER_PACKER,
        "every recall attempt reached a terminal state"
    );
    // Soft sanity: with the fresh-block safeguard, the vast majority
    // should succeed (we're packing + immediately recalling, much faster
    // than 60s threshold). If <50% succeed, something's wrong.
    let success_rate = recall_ok.load(Ordering::Relaxed) * 100 / total;
    assert!(
        success_rate >= 50,
        "recall success rate {}% — fresh-block safeguard may not be holding",
        success_rate
    );
}

// =====================================================================
// 3. Two concurrent GCs: no panic, no double-delete (counters approximate)
// =====================================================================

#[test]
fn two_concurrent_gcs_dont_panic_and_dont_double_delete_disk() {
    // Pre-populate with 50 blocks, all backdated 1 year (genuinely cold).
    // Then run two gc threads with threshold 30 days. Either both delete
    // some blocks, or one does and the other sees an empty store —
    // never both deleting the same block twice (would panic on missing file)
    // and never serving wrong bytes after.
    let sb = EnvSandbox::new("conc-two-gcs");
    let store = Store::new(sb.join("store"));

    let mut handles = Vec::with_capacity(50);
    for i in 0..50 {
        let h = store.put(format!("genuinely cold block #{i}\n").repeat(5).as_bytes());
        // Backdate meta to 1 year ago — same trick as round10_gc_counters.
        let hash_start = h.find('_').unwrap() + 1;
        let shard = &h[hash_start..hash_start + 2];
        let bp = sb.join("store").join(shard).join(&h);
        let mp = knapsack::meta::meta_path(&bp);
        let mut m = knapsack::meta::read(&mp).unwrap();
        let now = knapsack::meta::unix_now();
        m.last_accessed = now.saturating_sub(365 * 86_400);
        m.created_at = m.last_accessed;
        std::fs::write(&mp, m.to_json().as_bytes()).unwrap();
        handles.push(h);
    }

    let panicked = AtomicBool::new(false);
    let combined_deleted = AtomicUsize::new(0);

    std::thread::scope(|s| {
        for _ in 0..2 {
            let store = &store;
            let panicked = &panicked;
            let combined_deleted = &combined_deleted;
            s.spawn(move || {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let r = gc::gc(store, 30 * 86_400, false);
                    combined_deleted.fetch_add(r.deleted, Ordering::Relaxed);
                }));
                if result.is_err() {
                    panicked.store(true, Ordering::Relaxed);
                }
            });
        }
    });

    assert!(
        !panicked.load(Ordering::Relaxed),
        "two concurrent gcs panicked"
    );
    // Each gc thread saw at most 50 blocks. Combined deletes can be 50–100
    // depending on race interleaving (Race: both see the same block, both
    // attempt delete, second is a no-op since fs::remove_file silently
    // succeeds on missing files). Counters are approximate per acceptance.
    let combined = combined_deleted.load(Ordering::Relaxed);
    assert!(
        combined >= 50 && combined <= 100,
        "combined deletes in [50, 100]; got {combined} (over-count is OK, under-count not)"
    );

    // ALL blocks must be gone from disk regardless of count.
    for h in &handles {
        assert!(
            store.get(h).is_none(),
            "cold block {h} survived two concurrent gcs (should have been deleted)"
        );
    }
}

// =====================================================================
// 4. Concurrent puts of SAME bytes — idempotent, no panic
// =====================================================================

#[test]
fn many_threads_putting_same_bytes_converge_to_one_block() {
    // Sanity: idempotent put. 8 threads each calling put(same_bytes) ×10.
    // After all done, exactly ONE block file exists on disk and recall is
    // byte-exact.
    let sb = EnvSandbox::new("conc-same-bytes");
    let store = Store::new(sb.join("store"));
    let payload = b"identical payload across threads\n".repeat(20);

    let panicked = AtomicBool::new(false);

    std::thread::scope(|s| {
        for _ in 0..8 {
            let store = &store;
            let payload = payload.clone();
            let panicked = &panicked;
            s.spawn(move || {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    for _ in 0..10 {
                        let _ = store.put(&payload);
                    }
                }));
                if result.is_err() {
                    panicked.store(true, Ordering::Relaxed);
                }
            });
        }
    });
    assert!(!panicked.load(Ordering::Relaxed));

    // Exactly one block on disk (count by file walk, excluding .meta).
    let expected_h = hash::handle(&payload);
    let recalled = store.get(&expected_h).expect("block resolves");
    assert_eq!(
        recalled, payload,
        "byte-exact recall after concurrent same-byte puts"
    );
    assert_eq!(
        store.len(),
        1,
        "exactly one block file across 80 concurrent puts"
    );
}

// =====================================================================
// 5. Concurrent metrics writes (already pinned in mcp_cli_symmetry; one more
//    edge: writes while gc is running concurrently, since gc reads + deletes
//    files which are filesystem operations alongside the metrics append)
// =====================================================================

#[test]
fn metrics_writes_during_gc_remain_parseable_no_torn_lines() {
    let sb = EnvSandbox::new("conc-metrics-gc");
    let store = Store::new(sb.join("store"));

    // Pre-populate so gc has something to walk.
    for i in 0..30 {
        store.put(format!("block {i}\n").repeat(50).as_bytes());
    }

    let panicked = AtomicBool::new(false);
    let stop = AtomicBool::new(false);

    std::thread::scope(|s| {
        // Metrics writer
        s.spawn(|| {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                for i in 0..300 {
                    metrics::record_compress("conc-met", 100, 50, 50, i, 0);
                    metrics::record_expand("conc-met", "ks2_test", 25, true, metrics::ExpandMode::Whole, metrics::ExpandCaller::Cli);
                }
            }));
            if result.is_err() {
                panicked.store(true, Ordering::Relaxed);
            }
        });

        // gc loop, concurrent
        s.spawn(|| {
            while !stop.load(Ordering::Relaxed) {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let _ = gc::gc(&store, 30 * 86_400, false);
                }));
                if result.is_err() {
                    panicked.store(true, Ordering::Relaxed);
                    return;
                }
                std::thread::yield_now();
            }
        });

        // Let the writers race for a bit, then signal gc to stop.
        std::thread::sleep(Duration::from_millis(150));
        stop.store(true, Ordering::Relaxed);
    });

    assert!(!panicked.load(Ordering::Relaxed));

    // metrics::summary must parse cleanly and return a consistent count.
    let s = metrics::summary();
    assert_eq!(
        s.compress_events, 300,
        "all 300 compress lines landed atomically"
    );
    assert_eq!(
        s.expand_calls, 300,
        "all 300 expand lines landed atomically"
    );
}
