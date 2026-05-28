//! Corruption resilience for the byte-exact store. The cardinal contract is
//! "get(handle) == put bytes, or None — never wrong bytes". This file pounds
//! on the three layers of defense (meta-sha256, hash::verify on prefix, missing
//! meta fallback) with surgical filesystem tampering.

use knapsack::hash::{handle, sha1_hex, verify};
use knapsack::meta::{self, Meta};
use knapsack::sha256::sha256_hex;
use knapsack::store::Store;
use std::path::PathBuf;

fn tmpstore(tag: &str) -> Store {
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "kn-storecorrupt-{}-{}-{}",
        tag,
        std::process::id(),
        t
    ));
    Store::new(dir)
}

/// Resolve a block file path inside the sharded store layout. Mirrors the
/// shard logic in store.rs::path so the test can poke directly at the on-disk
/// file (corrupting it) and then verify get() handles it correctly.
fn block_path(store_dir: &std::path::Path, h: &str) -> PathBuf {
    let hash_start = h.find('_').map(|i| i + 1).unwrap_or(0);
    let shard = h.get(hash_start..hash_start + 2).unwrap_or("00");
    store_dir.join(shard).join(h)
}

// ---------- the cardinal "byte-exact or None, never wrong" contract ----------

#[test]
fn corrupt_block_bytes_read_as_none_not_wrong() {
    let store = tmpstore("corrupt-bytes");
    let original = b"the original content";
    let h = store.put(original);
    // Get works
    assert_eq!(store.get(&h).as_deref(), Some(&original[..]));

    // Corrupt the on-disk block — flip one byte
    let bp = block_path(store.dir(), &h);
    let mut bytes = std::fs::read(&bp).unwrap();
    bytes[0] ^= 0x01;
    std::fs::write(&bp, bytes).unwrap();

    // get() MUST return None (never wrong bytes)
    assert_eq!(store.get(&h), None, "corrupt block must read as None");
}

#[test]
fn truncated_block_reads_as_none() {
    let store = tmpstore("trunc");
    let original = b"some payload here";
    let h = store.put(original);
    let bp = block_path(store.dir(), &h);
    // Drop the last byte
    let mut bytes = std::fs::read(&bp).unwrap();
    bytes.pop();
    std::fs::write(&bp, bytes).unwrap();
    assert_eq!(store.get(&h), None, "truncated block must read as None");
}

#[test]
fn extra_appended_bytes_in_block_read_as_none() {
    let store = tmpstore("extend");
    let original = b"some payload here";
    let h = store.put(original);
    let bp = block_path(store.dir(), &h);
    let mut bytes = std::fs::read(&bp).unwrap();
    bytes.push(0xff);
    std::fs::write(&bp, bytes).unwrap();
    assert_eq!(store.get(&h), None, "extended block must read as None");
}

#[test]
fn empty_block_file_reads_as_none_for_nonempty_handle() {
    let store = tmpstore("empty-block");
    let original = b"payload";
    let h = store.put(original);
    let bp = block_path(store.dir(), &h);
    std::fs::write(&bp, b"").unwrap();
    assert_eq!(
        store.get(&h),
        None,
        "empty-out block must not match a nonempty handle"
    );
}

// ---------- meta sidecar corruption (3-layer defense) ----------

#[test]
fn missing_meta_falls_back_to_hash_verify() {
    // ks2_ block with no .meta sidecar. store.rs::get is supposed to fall
    // back to hash::verify (truncated SHA-256 prefix). Should still work
    // byte-exact for the unchanged block.
    let store = tmpstore("no-meta");
    let payload = b"sidecar-less block content";
    let h = store.put(payload);
    let bp = block_path(store.dir(), &h);
    // Delete the meta sidecar
    let mp = meta::meta_path(&bp);
    std::fs::remove_file(&mp).unwrap();
    // get() should still work via hash::verify
    assert_eq!(
        store.get(&h).as_deref(),
        Some(&payload[..]),
        "no meta -> hash::verify still works"
    );
}

#[test]
fn meta_with_corrupted_sha256_field_rejects_via_fallback() {
    let store = tmpstore("meta-bad-sha");
    let payload = b"content";
    let h = store.put(payload);
    let bp = block_path(store.dir(), &h);
    let mp = meta::meta_path(&bp);
    // Overwrite meta with a JSON that has the WRONG sha256.
    let bad = r#"{"sha256":"deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef","len":7,"created":1700000000,"accessed":1700000000}"#;
    std::fs::write(&mp, bad).unwrap();
    // Meta now contradicts the actual bytes. matches() fails. Per store.rs::get
    // logic, the get returns None for THIS shard path; flat_path doesn't exist;
    // returns None overall.
    assert_eq!(
        store.get(&h),
        None,
        "wrong sha256 in meta -> reject (never wrong bytes)"
    );
}

#[test]
fn meta_with_wrong_length_rejects_fast() {
    let store = tmpstore("meta-bad-len");
    let payload = b"hello world content";
    let h = store.put(payload);
    let bp = block_path(store.dir(), &h);
    let mp = meta::meta_path(&bp);
    // Meta says len 9999 but file is 19 bytes.
    let bad = format!(
        r#"{{"sha256":"{}","len":9999,"created":1700000000,"accessed":1700000000}}"#,
        sha256_hex(payload)
    );
    std::fs::write(&mp, &bad).unwrap();
    assert_eq!(store.get(&h), None, "wrong len in meta -> reject");
}

#[test]
fn meta_with_garbage_json_falls_back_to_hash_verify() {
    let store = tmpstore("meta-garbage");
    let payload = b"valid content";
    let h = store.put(payload);
    let bp = block_path(store.dir(), &h);
    let mp = meta::meta_path(&bp);
    // Garbage JSON — meta::read returns None — store falls back to hash::verify.
    std::fs::write(&mp, b"{ this is not json").unwrap();
    assert_eq!(
        store.get(&h).as_deref(),
        Some(&payload[..]),
        "garbage meta -> fall back to hash::verify"
    );
}

#[test]
fn meta_with_non_hex_sha256_field_rejects() {
    // matches() pre-checks: sha256 must be 64 hex chars. Non-hex returns false
    // immediately, the get sees verified=false, returns None.
    let store = tmpstore("meta-non-hex");
    let payload = b"hex-test content";
    let h = store.put(payload);
    let bp = block_path(store.dir(), &h);
    let mp = meta::meta_path(&bp);
    let bad = format!(
        r#"{{"sha256":"not-hex-at-all-this-is-broken-and-should-not-match","len":{},"created":0,"accessed":0}}"#,
        payload.len()
    );
    std::fs::write(&mp, &bad).unwrap();
    assert_eq!(store.get(&h), None, "non-hex sha256 in meta -> reject");
}

// ---------- Meta unit-level corruption checks ----------

#[test]
fn meta_matches_returns_false_for_any_corruption() {
    let bytes = b"some test content";
    let m = Meta::from_bytes(bytes);
    assert!(m.matches(bytes));
    assert!(!m.matches(b"different content"));
    let mut shorter = bytes.to_vec();
    shorter.pop();
    assert!(!m.matches(&shorter));
    let mut longer = bytes.to_vec();
    longer.push(0);
    assert!(!m.matches(&longer));
}

#[test]
fn meta_round_trip_via_json() {
    let mut m = Meta::from_bytes(b"round-trip-test");
    m.session = Some("test-session".into());
    m.source = Some("test-source".into());
    let s = m.to_json();
    let parsed = Meta::from_json(&s).expect("parses");
    assert_eq!(parsed, m);
}

#[test]
fn meta_from_json_rejects_malformed() {
    assert!(Meta::from_json("not json").is_none());
    assert!(Meta::from_json("{}").is_none(), "missing required fields");
    assert!(Meta::from_json(r#"{"sha256":""}"#).is_none(), "missing len");
}

// ---------- hash::verify routing ----------

#[test]
fn verify_routes_by_handle_format() {
    let bytes = b"verify-routing";
    // New format ks2_
    let h2 = handle(bytes); // ks2_<32 hex>
    assert!(verify(&h2, bytes));
    assert!(!verify(&h2, b"different"));
    // Legacy 10-hex ks_
    let legacy10 = format!("ks_{}", &sha1_hex(bytes)[..10]);
    assert!(verify(&legacy10, bytes));
    // Legacy 16-hex ks_
    let legacy16 = format!("ks_{}", &sha1_hex(bytes)[..16]);
    assert!(verify(&legacy16, bytes));
    // Wrong prefix
    assert!(!verify("rk_abc", bytes));
    // Empty
    assert!(!verify("", bytes));
}

#[test]
fn verify_rejects_handle_with_non_hex_chars() {
    let bytes = b"x";
    // ks2_ length is right but contains non-hex
    let bad = "ks2_g0123456789abcdef0123456789abcde";
    assert!(!verify(bad, bytes), "non-hex char must reject");
}

#[test]
fn verify_rejects_wrong_length_handles() {
    let bytes = b"x";
    assert!(!verify("ks2_short", bytes), "31-hex ks2_ rejected");
    assert!(
        !verify("ks2_0123456789abcdef0123456789abcdef0", bytes),
        "33-hex ks2_ rejected"
    );
    assert!(!verify("ks_short", bytes), "non 10/16 ks_ rejected");
}

// ---------- "double-corrupted" — both block and meta wrong ----------

#[test]
fn both_corrupt_returns_none() {
    // The hard test: an attacker corrupts BOTH the block file AND its meta
    // sidecar so the meta self-consistently certifies the corrupted bytes.
    // The handle's prefix (a SHA-256 truncation over the ORIGINAL bytes) is
    // a cryptographic commitment to what the bytes should be. A correct
    // verify-on-read MUST notice that meta agrees with the bytes BUT neither
    // matches the handle's commitment, and return None.
    //
    // If this assertion fails, store::get has a verification gap: the meta
    // sidecar can be a forged "approval" that bypasses the handle's commitment.
    // The fix would be: even when meta.matches(bytes) returns true, ALSO call
    // hash::verify(handle, bytes) before returning the bytes.
    let store = tmpstore("both-corrupt");
    let payload = b"both-bad payload";
    let h = store.put(payload);
    let bp = block_path(store.dir(), &h);
    let mp = meta::meta_path(&bp);
    // Corrupt block — flip the top byte
    let mut bytes = std::fs::read(&bp).unwrap();
    bytes[0] ^= 0xFF;
    std::fs::write(&bp, bytes).unwrap();
    // Forge meta to match the corrupted bytes (self-consistent corruption)
    let corrupted_bytes_after = std::fs::read(&bp).unwrap();
    let new_sha = sha256_hex(&corrupted_bytes_after);
    let synthesized_meta = format!(
        r#"{{"sha256":"{}","len":{},"created":0,"accessed":0}}"#,
        new_sha,
        corrupted_bytes_after.len()
    );
    std::fs::write(&mp, &synthesized_meta).unwrap();

    let result = store.get(&h);
    assert!(
        result.is_none(),
        "VERIFICATION GAP: store::get accepted forged meta + corrupted block for handle {h}. \
         Returned bytes (first 32): {:?}. \
         Original payload: {:?}. \
         Fix: in store::get's meta-verify branch, ALSO call hash::verify(handle, bytes) before \
         returning, so the handle's cryptographic commitment is enforced even when meta is forged.",
        result.as_ref().map(|b| &b[..b.len().min(32)]),
        &payload[..payload.len().min(32)],
    );
}
