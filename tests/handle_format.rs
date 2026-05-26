//! Lock the handle-format contract: new writes are `ks2_<32 hex>`, legacy `ks_<10|16
//! hex>` reads still resolve byte-exact via verify routing, and malformed handles get
//! rejected at the public boundaries. These pin the migration guarantees promised in
//! the CHANGELOG — break any one of them and the format bump is a backwards-incompat
//! change pretending to be additive.

use knapsack::hash::{handle, is_valid_handle, sha1_hex, verify};
use knapsack::Store;
use std::path::PathBuf;

fn store(tag: &str) -> Store {
    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    Store::new(std::env::temp_dir().join(format!("knapsack-handle-{}-{}-{}", tag, std::process::id(), t)))
}

fn knapsack_bin() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("target");
    p.push("release");
    if cfg!(windows) {
        p.push("knapsack.exe");
    } else {
        p.push("knapsack");
    }
    p
}

#[test]
fn new_writes_always_produce_ks2_and_match_expected_shape() {
    // Every write through the API surface lands on ks2_<32 hex>. If a future refactor
    // accidentally introduces a second handle path that still produces ks_, this test
    // is the loud canary.
    let s = store("new");
    let inputs: Vec<&[u8]> = vec![b"alpha", b"beta", b"gamma\n\nwith newlines", &[0u8; 200]];
    for b in inputs {
        let h = handle(b);
        assert!(h.starts_with("ks2_"), "new writes start with ks2_; got `{}` for input of {} bytes", h, b.len());
        assert_eq!(h.len(), 4 + 32, "ks2_ + 32 hex = 36 chars: got `{}`", h);
        assert!(h.chars().skip(4).all(|c| c.is_ascii_hexdigit()), "hex only after prefix: `{}`", h);
        // Round-trip: just-stored bytes are immediately recallable byte-exact.
        let stored_h = s.put(b);
        assert_eq!(stored_h, h, "store.put and bare handle() must agree");
        assert_eq!(s.get(&stored_h).as_deref(), Some(b), "round-trip byte-exact");
    }
}

#[test]
fn different_inputs_give_different_ks2_handles() {
    // Truncated SHA-256 at 128 bits keeps enough entropy that small inputs don't
    // collide. This also catches the failure mode of returning a constant.
    let inputs: Vec<&[u8]> = vec![b"", b"a", b"b", b"ab", b"ba", b"hello world", b"hello world\n"];
    let mut seen = std::collections::HashSet::new();
    for b in &inputs {
        let h = handle(b);
        assert!(seen.insert(h.clone()), "collision on inputs {:?}: handle {}", b, h);
    }
}

#[test]
fn legacy_ks_handles_still_resolve_byte_exact() {
    // Simulate an old store: a file at the legacy path, named with a legacy `ks_<10
    // hex>` handle. New store code must verify it via SHA-1 (not SHA-256) and return
    // its bytes unchanged. Mirrors what happens when a user updates Knapsack but their
    // ~/.knapsack/store is still full of pre-migration files.
    let s = store("legacy10");
    let bytes = b"legacy content that predates the format bump\n";
    let legacy_handle = format!("ks_{}", &sha1_hex(bytes)[..10]);

    // We can't use `s.put(bytes)` because that produces a ks2_ handle. Instead, we
    // synthesize the legacy file by writing to the path the store would expect.
    // (Implementation detail: the store shards by the first 2 hex chars of the hash —
    // either algorithm. We exercise the public path: get() must resolve it.)
    let legacy_dir = std::env::temp_dir().join(format!("knapsack-handle-legacy10-{}-{}",
        std::process::id(),
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
    std::fs::create_dir_all(&legacy_dir).unwrap();
    let shard = &legacy_handle[3..5]; // ks_<XX>... — sharded by first 2 hex of hash
    let shard_dir = legacy_dir.join(shard);
    std::fs::create_dir_all(&shard_dir).unwrap();
    std::fs::write(shard_dir.join(&legacy_handle), bytes).unwrap();

    let legacy_store = Store::new(legacy_dir.clone());
    assert_eq!(
        legacy_store.get(&legacy_handle).as_deref(),
        Some(bytes.as_ref()),
        "legacy 10-hex handle must still resolve byte-exact"
    );
    let _ = std::fs::remove_dir_all(&legacy_dir);
    let _ = s.get(&"ks_unused".to_string()); // keep `s` alive for warning-free unused var
}

#[test]
fn legacy_ks_16_hex_handles_also_resolve() {
    // Some past version may have shipped ks_<16 hex> (64-bit truncation). is_valid_handle
    // accepts both; verify routes both through SHA-1. This pins that the 16-hex form
    // still works end-to-end through store.get.
    let bytes = b"older legacy content with 16-hex handle\n";
    let legacy_handle = format!("ks_{}", &sha1_hex(bytes)[..16]);
    let legacy_dir = std::env::temp_dir().join(format!("knapsack-handle-legacy16-{}-{}",
        std::process::id(),
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
    std::fs::create_dir_all(&legacy_dir).unwrap();
    let shard = &legacy_handle[3..5];
    std::fs::create_dir_all(legacy_dir.join(shard)).unwrap();
    std::fs::write(legacy_dir.join(shard).join(&legacy_handle), bytes).unwrap();

    let s = Store::new(legacy_dir.clone());
    assert_eq!(
        s.get(&legacy_handle).as_deref(),
        Some(bytes.as_ref()),
        "legacy 16-hex handle must also resolve byte-exact"
    );
    let _ = std::fs::remove_dir_all(&legacy_dir);
}

#[test]
fn legacy_handle_with_wrong_bytes_is_rejected_by_verify() {
    // Verify-on-read is the corruption guard. Plant a file at a legacy path but with
    // bytes that DON'T hash to that handle — get() must return None, not the wrong
    // bytes. This is the byte-exact-or-None invariant, preserved across the format
    // bump.
    let bytes_truth = b"the real legacy content\n";
    let bytes_corrupt = b"someone tampered with this file\n";
    let legacy_handle = format!("ks_{}", &sha1_hex(bytes_truth)[..10]);
    let legacy_dir = std::env::temp_dir().join(format!("knapsack-handle-corrupt-{}-{}",
        std::process::id(),
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
    std::fs::create_dir_all(&legacy_dir).unwrap();
    let shard = &legacy_handle[3..5];
    std::fs::create_dir_all(legacy_dir.join(shard)).unwrap();
    std::fs::write(legacy_dir.join(shard).join(&legacy_handle), bytes_corrupt).unwrap();

    let s = Store::new(legacy_dir.clone());
    assert!(s.get(&legacy_handle).is_none(), "corrupted legacy file must read as None, never as wrong bytes");
    let _ = std::fs::remove_dir_all(&legacy_dir);
}

#[test]
fn is_valid_handle_accepts_both_formats_and_rejects_junk() {
    // Whitelist
    assert!(is_valid_handle("ks2_00112233445566778899aabbccddeeff"), "32-hex ks2 ok");
    assert!(is_valid_handle("ks_0123456789"), "10-hex legacy ok");
    assert!(is_valid_handle("ks_0123456789abcdef"), "16-hex legacy ok");
    // Blacklist — wrong prefix, wrong length, non-hex, traversal attempts
    let junk = [
        "",                                            // empty
        "ks_",                                         // empty hex
        "ks_short",                                    // not hex / wrong length
        "ks_0123456789abcde",                          // 15 hex (not legacy)
        "ks_0123456789abcdef0",                        // 17 hex (not legacy)
        "ks2_short",                                   // wrong length
        "ks2_0123456789abcdef0123456789abcdeg",        // non-hex 'g'
        "rk_0123456789",                               // rucksack prefix
        "ks2_",                                        // empty hex
        "../etc/passwd",                               // traversal attempt
        "ks2_../path",                                 // mixed
    ];
    for j in junk {
        assert!(!is_valid_handle(j), "expected rejection for: {:?}", j);
    }
}

#[test]
fn verify_round_trips_both_formats() {
    let payload: &[u8] = b"some test bytes\nwith two lines\n";
    let ks2 = handle(payload);
    assert!(verify(&ks2, payload), "ks2_ verifies");
    assert!(!verify(&ks2, b"other"), "wrong bytes don't verify");

    let legacy10 = format!("ks_{}", &sha1_hex(payload)[..10]);
    assert!(verify(&legacy10, payload), "legacy 10-hex verifies via SHA-1");
    let legacy16 = format!("ks_{}", &sha1_hex(payload)[..16]);
    assert!(verify(&legacy16, payload), "legacy 16-hex verifies via SHA-1");
}

// ---- CLI boundary: invalid handles get a clear error, not a misleading 404 ----

#[test]
fn cli_expand_rejects_malformed_handle() {
    let bin = knapsack_bin();
    if !bin.exists() {
        eprintln!("skipping: {} not built yet", bin.display());
        return;
    }
    let dir = std::env::temp_dir().join(format!("knapsack-cli-reject-{}-{}",
        std::process::id(),
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
    std::fs::create_dir_all(&dir).unwrap();

    let out = std::process::Command::new(&bin)
        .args(["expand", "garbage_handle_string"])
        .env("KNAPSACK_STORE", dir.join("store"))
        .env("KNAPSACK_METRICS", dir.join("metrics.jsonl"))
        .output()
        .expect("spawn knapsack");
    assert!(!out.status.success(), "malformed handle should not exit 0");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("invalid handle"), "must say `invalid handle`, not `no such handle`:\nstderr={}", stderr);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn cli_inspect_rejects_malformed_handle() {
    let bin = knapsack_bin();
    if !bin.exists() {
        eprintln!("skipping: {} not built yet", bin.display());
        return;
    }
    let dir = std::env::temp_dir().join(format!("knapsack-cli-reject-i-{}-{}",
        std::process::id(),
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
    std::fs::create_dir_all(&dir).unwrap();

    let out = std::process::Command::new(&bin)
        .args(["inspect", "ks_definitelyTooLongToBeLegacyAndNotKs2"])
        .env("KNAPSACK_STORE", dir.join("store"))
        .output()
        .expect("spawn knapsack");
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("invalid handle"), "must say `invalid handle`:\nstderr={}", stderr);

    let _ = std::fs::remove_dir_all(&dir);
}
