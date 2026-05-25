//! Store is byte-exact for ANY bytes — the foundation everything else rests on.
use knapsack::{handle, Store};
use std::fs;
use std::path::PathBuf;

fn tmp(tag: &str) -> PathBuf {
    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    std::env::temp_dir().join(format!("knapsack-test-{}-{}-{}", tag, std::process::id(), t))
}

#[test]
fn roundtrip_is_byte_exact() {
    let store = Store::new(tmp("store"));
    let cases: Vec<Vec<u8>> = vec![
        b"hello world".to_vec(),
        b"crlf\r\nlines\r\nhere\r\n".to_vec(),
        b"no trailing newline".to_vec(),
        vec![0u8, 1, 2, 255, 254, 0, 128, 200], // non-UTF-8, embedded NUL
        b"".to_vec(),
        "unicode: \u{1F600} \u{00e9} \u{2728}".as_bytes().to_vec(),
    ];
    for c in cases {
        let h = store.put(&c);
        assert_eq!(store.get(&h).unwrap(), c, "store must return exact bytes");
    }
}

#[test]
fn identical_content_dedups() {
    let store = Store::new(tmp("dedup"));
    let h1 = store.put(b"same bytes");
    let h2 = store.put(b"same bytes");
    assert_eq!(h1, h2, "content-addressed: identical bytes -> identical handle");
}

// ---- sharded layout + backward-compatible read fallback ----

#[test]
fn new_writes_are_sharded_and_roundtrip() {
    let dir = tmp("shardwrite");
    let store = Store::new(dir.clone());
    let bytes = b"sharded content\nline two\n";
    let h = store.put(bytes);
    // New writes must NOT land in the flat root; they go under a shard subdir.
    assert!(!dir.join(&h).exists(), "new write must not be at the flat path");
    assert_eq!(store.get(&h).as_deref(), Some(&bytes[..]), "sharded write must read back byte-exact");
}

#[test]
fn legacy_flat_files_still_read() {
    let dir = tmp("legacy");
    let store = Store::new(dir.clone());
    let bytes = b"old flat-format cache entry";
    let h = handle(bytes);
    // Simulate a store written by a pre-sharding version: a file directly in the root.
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join(&h), bytes).unwrap();
    assert_eq!(store.get(&h).as_deref(), Some(&bytes[..]), "must fall back to the legacy flat path");
}

#[test]
fn sharded_wins_when_both_exist() {
    let dir = tmp("both");
    let store = Store::new(dir.clone());
    let bytes = b"canonical sharded bytes";
    let h = store.put(bytes); // sharded
    // Drop a DIFFERENT flat file under the same handle to lock the precedence rule.
    fs::write(dir.join(&h), b"STALE FLAT").unwrap();
    assert_eq!(store.get(&h).as_deref(), Some(&bytes[..]), "sharded copy must deterministically win");
}

#[test]
fn concurrent_puts_are_byte_exact() {
    let store = Store::new(tmp("conc"));
    let blocks: Vec<Vec<u8>> = (0..300).map(|i| format!("concurrent block #{i}\nwith a second line\n").into_bytes()).collect();
    // 8 threads all storing the SAME blocks -> heavy contention on identical sharded paths.
    std::thread::scope(|s| {
        for _ in 0..8 {
            let store = &store;
            let blocks = &blocks;
            s.spawn(move || {
                let slices: Vec<&[u8]> = blocks.iter().map(Vec::as_slice).collect();
                let _ = store.put_many(&slices);
            });
        }
    });
    for b in &blocks {
        assert_eq!(store.get(&handle(b)).as_deref(), Some(b.as_slice()), "concurrent writes corrupted a block");
    }
}
