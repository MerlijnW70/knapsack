//! Recall-path robustness beyond the happy path: eviction must not touch the store, and
//! `reconstruct` must NEVER hand back wrong bytes — even when called with the wrong content
//! type — thanks to exact tiling plus verify-on-read (it returns None or the exact input).

use knapsack::content_type::ContentType;
use knapsack::{pack, reconstruct, Ledger, Store};
use std::path::PathBuf;

fn tmp(tag: &str) -> PathBuf {
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "knapsack-recall-{}-{}-{}",
        tag,
        std::process::id(),
        t
    ))
}

#[test]
fn eviction_is_ledger_only_store_keeps_every_block() {
    let store = Store::new(tmp("evrec"));
    let mut ledger = Ledger::in_memory();
    let mut s = String::new();
    for i in 0..300 {
        s.push_str(&format!("log line {i} value={}\n", i * 9));
    }
    let bytes = s.into_bytes();
    pack(&bytes, ContentType::Log, &store, &mut ledger, 0);
    let evicted = ledger.enforce_budget(1); // brutal budget -> evict almost everything
    assert!(evicted > 0, "expected the budget to force eviction");
    assert_eq!(
        reconstruct(&bytes, ContentType::Log, &store).as_deref(),
        Some(bytes.as_slice()),
        "eviction is a ledger-only operation; the store must still reconstruct byte-exact"
    );
}

#[test]
fn reconstruct_never_yields_wrong_bytes_under_ct_mismatch() {
    let store = Store::new(tmp("ctmix"));
    let mut ledger = Ledger::in_memory();
    let mut s = String::new();
    for i in 0..40 {
        s.push_str(&format!("function f{i}() {{ return {i}; }}\n"));
    }
    let bytes = s.into_bytes();
    pack(&bytes, ContentType::Code, &store, &mut ledger, 0);

    // Same content type used to pack -> always byte-exact.
    assert_eq!(
        reconstruct(&bytes, ContentType::Code, &store).as_deref(),
        Some(bytes.as_slice())
    );

    // WRONG content type splits differently, so blocks may be missing -> None is allowed.
    // But it must NEVER return Some(wrong): tiling + verify-on-read guarantee exact-or-None.
    if let Some(b) = reconstruct(&bytes, ContentType::Log, &store) {
        assert_eq!(b, bytes, "ct mismatch must never produce WRONG bytes");
    }
}

#[test]
fn reconstruct_of_unstored_input_is_none_not_garbage() {
    let store = Store::new(tmp("empty"));
    // Nothing was ever packed into this store.
    let bytes = b"content that was never stored\nsecond line\n";
    assert!(
        reconstruct(bytes, ContentType::Log, &store).is_none(),
        "missing blocks -> None, never partial garbage"
    );
}
