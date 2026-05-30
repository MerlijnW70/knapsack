//! The store-metadata contract from the brief:
//!   - valid ks2 expand (round-trip is byte-exact AND sidecar is written)
//!   - corrupted block fails (returns None)
//!   - wrong length fails (meta says 100, file is 50 → None)
//!   - missing metadata behavior is clear (falls back to truncated-prefix verify)
//!   - legacy expand still works (no .meta involvement)
//!   - gc removes block + metadata consistently

use knapsack::gc::{coverage, gc as gc_run};
use knapsack::hash::{handle, sha1_hex};
use knapsack::meta::{self, Meta};
use knapsack::Store;
use std::path::{Path, PathBuf};

// gc() also sweeps the Read-hook cache dir (config::read_cache_dir(), a
// PROCESS-GLOBAL KNAPSACK_READ_CACHE / shared default). Tests that assert exact
// gc counts must route that env var at an empty per-test dir, or a live session
// writing to the real cache makes `deleted`/`kept`/`meta_missing` non-deterministic
// (observed: legacy-fallback test saw deleted=2, expected 1). EnvSandbox does this
// (and serializes via a global lock); the explicit Store path below is unaffected.
mod common;
use common::EnvSandbox;

fn store_dir(tag: &str) -> PathBuf {
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "knapsack-storemeta-{}-{}-{}",
        tag,
        std::process::id(),
        t
    ))
}

/// Walk the shard layout and return every file in it (blocks AND .meta sidecars).
/// Used by gc tests to assert that paired deletion really removed both sides.
fn list_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(top) = std::fs::read_dir(dir) {
        for e in top.flatten() {
            let p = e.path();
            if p.is_dir() {
                if let Ok(sub) = std::fs::read_dir(&p) {
                    for e2 in sub.flatten() {
                        out.push(e2.path());
                    }
                }
            } else {
                out.push(p);
            }
        }
    }
    out
}

#[test]
fn valid_ks2_round_trip_writes_meta_and_recalls_byte_exact() {
    // Brief: "valid ks2 expand". A fresh write must produce both <handle> and
    // <handle>.meta; the meta must self-describe the bytes; recall is byte-exact.
    let dir = store_dir("valid");
    let s = Store::new(dir.clone());
    let bytes = b"some content worth recalling, line 1\nline 2\nline 3\n";

    let h = s.put(bytes);
    assert!(h.starts_with("ks2_"), "new writes are ks2_");
    let block = walk_for(&dir, &h).expect("block file landed on disk");
    let m = meta::meta_path(&block);
    assert!(
        m.exists(),
        "ks2 writes must produce a .meta sidecar at {}",
        m.display()
    );

    let parsed = meta::read(&m).expect(".meta is parseable JSON");
    assert_eq!(parsed.len, bytes.len() as u64, "len matches");
    assert_eq!(
        parsed.sha256,
        knapsack::sha256::sha256_hex(bytes),
        "full SHA-256 stored"
    );
    assert!(
        parsed.created_at > 0 && parsed.last_accessed >= parsed.created_at,
        "timestamps populated"
    );

    assert_eq!(
        s.get(&h).as_deref(),
        Some(bytes.as_ref()),
        "recall byte-exact"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn corrupted_block_with_valid_meta_fails_recall() {
    // Brief: "corrupted block fails". Tamper with the bytes file — meta says one
    // SHA-256, bytes hash to another. get() must return None, never the wrong bytes.
    let dir = store_dir("corrupt-block");
    let s = Store::new(dir.clone());
    let bytes = b"exact original bytes";
    let h = s.put(bytes);
    let block = walk_for(&dir, &h).expect("block exists");
    std::fs::write(&block, b"TAMPERED").unwrap();

    assert!(
        s.get(&h).is_none(),
        "tampered bytes vs intact meta -> None, never wrong bytes"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn wrong_length_in_meta_fails_recall() {
    // Brief: "wrong length fails". Write valid bytes, then HAND-EDIT the meta to claim
    // a wrong length. matches() rejects on length BEFORE paying for SHA-256, so this
    // also exercises the cheap-reject path.
    let dir = store_dir("wronglen");
    let s = Store::new(dir.clone());
    let bytes = b"twenty bytes exactly";
    assert_eq!(bytes.len(), 20);
    let h = s.put(bytes);

    let block = walk_for(&dir, &h).expect("block");
    let mp = meta::meta_path(&block);
    let mut m = meta::read(&mp).unwrap();
    m.len = 999; // lie
    std::fs::write(&mp, m.to_json()).unwrap();

    assert!(s.get(&h).is_none(), "length disagreement -> None");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn missing_metadata_falls_back_to_truncated_prefix_verify() {
    // Brief: "missing metadata behavior duidelijk". A ks2 block whose .meta has been
    // deleted (e.g. user manually cleaned, or written by a pre-meta build) must STILL
    // resolve via hash::verify — SHA-256 truncated to the 32-hex handle suffix.
    let dir = store_dir("nometa");
    let s = Store::new(dir.clone());
    let bytes = b"content with no meta sidecar";
    let h = s.put(bytes);
    let block = walk_for(&dir, &h).expect("block");
    let mp = meta::meta_path(&block);
    std::fs::remove_file(&mp).unwrap();
    assert!(!mp.exists(), "meta gone");

    assert_eq!(
        s.get(&h).as_deref(),
        Some(bytes.as_ref()),
        "no meta -> falls back to hash::verify, still byte-exact"
    );
    // And corrupting the bytes still fails through the fallback path.
    std::fs::write(&block, b"TAMPERED no-meta").unwrap();
    assert!(
        s.get(&h).is_none(),
        "no meta + bad bytes -> None via fallback verify"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn legacy_ks_handles_still_expand_without_meta() {
    // Brief: "legacy expand blijft werken". Synthesize a pre-format-bump store: a file
    // at the legacy path, no sidecar. get() verifies via SHA-1 (because the handle is
    // ks_<10 hex>) and returns the bytes.
    let dir = store_dir("legacy");
    let s = Store::new(dir.clone());
    let bytes = b"older legacy block, predates ks2 and predates meta";
    let legacy_handle = format!("ks_{}", &sha1_hex(bytes)[..10]);
    // Write directly into the sharded path the store would pick (so we never go
    // through put(), which produces ks2_ + meta).
    let shard = &legacy_handle[3..5];
    let shard_dir = dir.join(shard);
    std::fs::create_dir_all(&shard_dir).unwrap();
    std::fs::write(shard_dir.join(&legacy_handle), bytes).unwrap();

    assert_eq!(
        s.get(&legacy_handle).as_deref(),
        Some(bytes.as_ref()),
        "legacy handle still resolves byte-exact"
    );
    assert!(
        !meta::meta_path(&shard_dir.join(&legacy_handle)).exists(),
        "legacy stays meta-free"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn gc_removes_block_and_meta_as_a_pair() {
    // Brief: "gc verwijdert block + metadata consistent".
    let _sb = EnvSandbox::new("gc-pair"); // isolate the read-cache sweep
    let dir = store_dir("gc-pair");
    let s = Store::new(dir.clone());
    let bytes = b"block that's about to age out";
    let h = s.put(bytes);
    let block = walk_for(&dir, &h).expect("block");
    let mp = meta::meta_path(&block);
    assert!(
        block.exists() && mp.exists(),
        "both block and meta exist pre-gc"
    );

    // older_than=0 → everything older than 0 seconds (i.e. everything) is stale.
    let r = gc_run(&s, 0, false);
    assert_eq!(r.deleted, 1, "deleted exactly the one block");
    assert!(!block.exists(), "block file gone after gc");
    assert!(
        !mp.exists(),
        "meta sidecar gone after gc — never half a pair"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn gc_dry_run_reports_without_touching() {
    // Brief contract: gc must be safely previewable. With --dry-run, NOTHING is
    // removed but the report still says what WOULD be removed.
    let _sb = EnvSandbox::new("gc-dry"); // isolate the read-cache sweep
    let dir = store_dir("gc-dry");
    let s = Store::new(dir.clone());
    s.put(b"a");
    s.put(b"b");
    let before = list_files(&dir);
    assert!(!before.is_empty());

    let r = gc_run(&s, 0, true);
    assert_eq!(r.deleted, 2, "report says it would delete two");
    let after = list_files(&dir);
    assert_eq!(before.len(), after.len(), "dry-run touched zero files");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn gc_keeps_fresh_blocks() {
    // A block whose last_accessed is recent must NOT be removed. We can't slow the
    // test down enough for created_at to age, so we point gc at a huge threshold —
    // proves the keep path is reachable too, not just delete.
    let _sb = EnvSandbox::new("gc-fresh"); // isolate the read-cache sweep
    let dir = store_dir("gc-fresh");
    let s = Store::new(dir.clone());
    s.put(b"keep me");

    let r = gc_run(&s, 365 * 86_400, false); // older than a year
    assert_eq!(r.deleted, 0, "nothing aged out");
    assert_eq!(r.kept, 1);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn gc_handles_legacy_blocks_via_fs_mtime_fallback() {
    // A block with no .meta — pre-format-bump — still needs an age signal. gc falls
    // back to filesystem mtime; with a "0 seconds" threshold it still gets removed.
    let _sb = EnvSandbox::new("gc-legacy"); // isolate the read-cache sweep
    let dir = store_dir("gc-legacy");
    let s = Store::new(dir.clone());
    let bytes = b"legacy block, no sidecar";
    let legacy_handle = format!("ks_{}", &sha1_hex(bytes)[..10]);
    let shard = &legacy_handle[3..5];
    std::fs::create_dir_all(dir.join(shard)).unwrap();
    let block = dir.join(shard).join(&legacy_handle);
    std::fs::write(&block, bytes).unwrap();

    let r = gc_run(&s, 0, false);
    assert_eq!(r.deleted, 1, "legacy block aged out via fs mtime");
    assert_eq!(r.meta_missing, 1, "report tags it as meta-missing");
    assert!(!block.exists(), "legacy block removed");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn coverage_reports_meta_density_for_doctor() {
    // The doctor's informational line uses gc::coverage(). For a fresh store it
    // should report total > 0 and with_meta == total (because every new put writes
    // a sidecar). Mixing in a legacy file should drop the ratio.
    let dir = store_dir("cov");
    let s = Store::new(dir.clone());
    s.put(b"new block 1");
    s.put(b"new block 2");

    let (total, with_meta) = coverage(&s);
    assert_eq!(total, 2);
    assert_eq!(with_meta, 2, "fresh writes always carry meta");

    // Drop in a legacy block by hand.
    let legacy = format!("ks_{}", &sha1_hex(b"legacy")[..10]);
    std::fs::create_dir_all(dir.join(&legacy[3..5])).unwrap();
    std::fs::write(dir.join(&legacy[3..5]).join(&legacy), b"legacy").unwrap();

    let (total, with_meta) = coverage(&s);
    assert_eq!(total, 3, "legacy block counted");
    assert_eq!(with_meta, 2, "but legacy has no meta");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn meta_from_bytes_carries_full_sha256_not_truncated() {
    // The handle truncates SHA-256 to 128 bits (32 hex). The meta must carry the
    // FULL 256-bit digest — that's the safety belt promised in the brief.
    let bytes = b"safety margin: meta carries the full digest";
    let m = Meta::from_bytes(bytes);
    assert_eq!(m.sha256.len(), 64, "full SHA-256 = 64 hex chars");
    assert_eq!(m.sha256, knapsack::sha256::sha256_hex(bytes));
    let h = handle(bytes);
    assert_eq!(
        h.strip_prefix("ks2_").unwrap(),
        &m.sha256[..32],
        "handle is the 32-hex prefix of the same digest"
    );
}

/// Helper: walk the sharded layout to find the file named `handle`. Tests need this
/// because the store doesn't publicly expose its `path()` method.
fn walk_for(dir: &Path, handle: &str) -> Option<PathBuf> {
    for shard in std::fs::read_dir(dir).ok()?.flatten() {
        let p = shard.path();
        if !p.is_dir() {
            continue;
        }
        let candidate = p.join(handle);
        if candidate.exists() {
            return Some(candidate);
        }
    }
    None
}
