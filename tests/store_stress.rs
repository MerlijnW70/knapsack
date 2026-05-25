//! Deep stress + property coverage for the sharded, parallel-write store. The store is the
//! byte-exact source of truth; the sharded layout + parallel `put_many` + read fallback are
//! the newest and riskiest code, so this hammers their invariants directly.

use knapsack::{handle, Store};
use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;

fn tmp(tag: &str) -> PathBuf {
    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    std::env::temp_dir().join(format!("knapsack-stress-{}-{}-{}", tag, std::process::id(), t))
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

// ---------- put_many invariants ----------

#[test]
fn put_many_preserves_order_and_is_byte_exact() {
    let store = Store::new(tmp("order"));
    let blocks: Vec<Vec<u8>> = (0..1000).map(|i| format!("block number {i}\nsecond line {i}\n").into_bytes()).collect();
    let slices: Vec<&[u8]> = blocks.iter().map(Vec::as_slice).collect();
    let handles = store.put_many(&slices);
    assert_eq!(handles.len(), blocks.len(), "one handle per input, same count");
    for (b, h) in blocks.iter().zip(&handles) {
        assert_eq!(*h, handle(b), "handle[i] must be the handle of block[i] (order preserved)");
        assert_eq!(store.get(h).as_deref(), Some(b.as_slice()), "byte-exact via put_many");
    }
}

#[test]
fn put_many_dedups_within_a_call() {
    let store = Store::new(tmp("dups"));
    let a = b"alpha payload".to_vec();
    let b = b"beta payload".to_vec();
    let blocks = vec![a.as_slice(), b.as_slice(), a.as_slice(), a.as_slice(), b.as_slice()];
    let h = store.put_many(&blocks);
    assert_eq!(h[0], h[2]);
    assert_eq!(h[0], h[3]);
    assert_eq!(h[1], h[4]);
    assert_eq!(store.get(&h[0]).as_deref(), Some(a.as_slice()));
    assert_eq!(store.get(&h[1]).as_deref(), Some(b.as_slice()));
}

#[test]
fn put_many_handles_empty_and_single() {
    let store = Store::new(tmp("edge"));
    assert!(store.put_many(&[]).is_empty(), "empty input -> empty handles");
    let one = store.put_many(&[b"only one block".as_slice()]);
    assert_eq!(one.len(), 1);
    assert_eq!(store.get(&one[0]).as_deref(), Some(&b"only one block"[..]));
}

#[test]
fn put_many_varied_sizes_incl_large_blocks() {
    let store = Store::new(tmp("sizes"));
    // 0 bytes up to ~250 KB, plus embedded NUL / high bytes
    let blocks: Vec<Vec<u8>> = (0..60).map(|i| {
        let len = i * 4096;
        (0..len).map(|j| ((i + j) % 256) as u8).collect()
    }).collect();
    let slices: Vec<&[u8]> = blocks.iter().map(Vec::as_slice).collect();
    let handles = store.put_many(&slices);
    for (b, h) in blocks.iter().zip(&handles) {
        assert_eq!(store.get(h).as_deref(), Some(b.as_slice()), "large/binary block must roundtrip");
    }
}

// ---------- concurrency ----------

#[test]
fn concurrent_distinct_and_overlapping_puts_are_byte_exact() {
    let store = Store::new(tmp("concmix"));
    let threads = 12;
    let per = 400usize;
    // Shared blocks (every thread writes them -> identical sharded paths race) plus
    // per-thread unique blocks (distinct paths, exercise many shards at once).
    let shared: Vec<Vec<u8>> = (0..per).map(|i| format!("SHARED block {i}\n").into_bytes()).collect();
    std::thread::scope(|s| {
        for t in 0..threads {
            let store = &store;
            let shared = &shared;
            s.spawn(move || {
                let mut blocks: Vec<Vec<u8>> = shared.clone();
                for i in 0..per {
                    blocks.push(format!("thread {t} unique block {i}\n").into_bytes());
                }
                let slices: Vec<&[u8]> = blocks.iter().map(Vec::as_slice).collect();
                let _ = store.put_many(&slices);
            });
        }
    });
    for i in 0..per {
        let b = format!("SHARED block {i}\n").into_bytes();
        assert_eq!(store.get(&handle(&b)).as_deref(), Some(b.as_slice()), "shared block corrupted under contention");
    }
    for t in 0..threads {
        for i in 0..per {
            let b = format!("thread {t} unique block {i}\n").into_bytes();
            assert_eq!(store.get(&handle(&b)).as_deref(), Some(b.as_slice()), "unique block lost under contention");
        }
    }
}

// ---------- shard distribution ----------

#[test]
fn shards_are_well_distributed_on_disk() {
    // The sharding key (handle[3..5]) must spread blocks over many of the 256 buckets, else
    // the perf fix is moot. Store 5000 blocks, then count how many shard subdirs got files.
    let dir = tmp("dist");
    let store = Store::new(dir.clone());
    let blocks: Vec<Vec<u8>> = (0..5000).map(|i| format!("distribution probe {i}\n").into_bytes()).collect();
    let slices: Vec<&[u8]> = blocks.iter().map(Vec::as_slice).collect();
    store.put_many(&slices);
    let used_dirs = fs::read_dir(&dir).unwrap().flatten().filter(|e| e.path().is_dir()).count();
    let distinct_prefixes: HashSet<String> = blocks.iter().map(|b| handle(b)[3..5].to_string()).collect();
    assert!(used_dirs > 128, "expected >128 of 256 shard dirs used, got {used_dirs}");
    assert_eq!(used_dirs, distinct_prefixes.len(), "on-disk shard dirs must match distinct handle prefixes");
}

// ---------- read fallback / unknown / corruption probe ----------

#[test]
fn unknown_handle_returns_none() {
    let store = Store::new(tmp("unknown"));
    assert!(store.get(&"ks_0000000000".to_string()).is_none());
    assert!(store.get(&"not even a handle".to_string()).is_none());
}

#[test]
fn corrupted_file_reads_as_missing() {
    // Verify-on-read: a tampered/torn store file must read as None (-> caller re-sends),
    // never as silently-wrong bytes. Protects the byte-exact guarantee under corruption.
    let dir = tmp("corrupt");
    let store = Store::new(dir.clone());
    let h = store.put(b"important exact bytes");
    fs::write(dir.join(&h[3..5]).join(&h), b"TAMPERED").unwrap(); // corrupt the sharded file
    assert!(store.get(&h).is_none(), "a corrupted file must read as missing, not wrong bytes");
}

#[test]
fn corrupt_shard_falls_back_to_valid_flat() {
    let dir = tmp("corruptfallback");
    let store = Store::new(dir.clone());
    let bytes = b"recoverable content here";
    let h = store.put(bytes); // valid sharded copy
    fs::write(dir.join(&h), bytes).unwrap(); // valid legacy flat copy
    fs::write(dir.join(&h[3..5]).join(&h), b"GARBAGE").unwrap(); // corrupt the sharded copy
    assert_eq!(store.get(&h).as_deref(), Some(&bytes[..]), "must fall back to the valid flat copy");
}

#[test]
fn empty_block_roundtrips() {
    let store = Store::new(tmp("emptyblock"));
    let h = store.put(b"");
    assert_eq!(store.get(&h).as_deref(), Some(&b""[..]), "the empty block must store and recall");
}

#[test]
fn len_counts_sharded_and_legacy_flat() {
    let dir = tmp("lenmix");
    let store = Store::new(dir.clone());
    store.put(b"sharded one");
    store.put(b"sharded two");
    // a pre-sharding legacy file dropped directly in the root
    let legacy = b"legacy flat entry";
    fs::write(dir.join(handle(legacy)), legacy).unwrap();
    assert_eq!(store.len(), 3, "len must count both sharded files and legacy flat files");
    assert!(!store.is_empty());
}

#[test]
fn two_megabyte_block_roundtrips() {
    let store = Store::new(tmp("huge"));
    let big: Vec<u8> = (0..2_000_000u32).map(|i| (i.wrapping_mul(2654435761) >> 16) as u8).collect();
    let h = store.put(&big);
    assert_eq!(store.get(&h).as_deref(), Some(big.as_slice()), "a 2 MB block must roundtrip byte-exact");
}

#[test]
fn many_sessions_share_one_store_byte_exact() {
    // Distinct logical sessions reusing the same store dir (the real cross-session case).
    let dir = tmp("shared-store");
    let mut rng = Rng(0xABCDEF0123456789);
    let mut all: Vec<Vec<u8>> = Vec::new();
    for _ in 0..50 {
        let store = Store::new(dir.clone()); // a fresh Store handle, same dir = new "process"
        let n = 20 + rng.below(80);
        let blocks: Vec<Vec<u8>> = (0..n).map(|_| {
            let len = rng.below(400);
            (0..len).map(|_| rng.next() as u8).collect()
        }).collect();
        let slices: Vec<&[u8]> = blocks.iter().map(Vec::as_slice).collect();
        store.put_many(&slices);
        all.extend(blocks);
    }
    // A final fresh handle must recall every block ever written, byte-exact.
    let store = Store::new(dir);
    for b in &all {
        assert_eq!(store.get(&handle(b)).as_deref(), Some(b.as_slice()), "cross-session recall must stay byte-exact");
    }
}
