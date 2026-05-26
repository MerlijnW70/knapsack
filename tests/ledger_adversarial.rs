//! Ledger corruption + size dogfood (src/ledger.rs). The ledger is a tiny
//! per-session TSV file. Corrupt or huge ledgers must not crash; the LOAD
//! must skip malformed lines (forward-compat with future schema changes) and
//! keep the valid ones; SAVE must not lose information; budget enforcement
//! must converge in linear time even on 10K-entry ledgers.

use knapsack::ledger::{Ledger, Residency};
use std::path::PathBuf;

fn tmp_ledger_path(tag: &str) -> PathBuf {
    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    let p = std::env::temp_dir().join(format!("kn-ledger-{}-{}-{}.tsv", tag, std::process::id(), t));
    p
}

// ---------- empty / missing file ----------

#[test]
fn load_missing_file_returns_empty_ledger() {
    let p = std::env::temp_dir().join(format!("kn-noexist-{}-{}.tsv", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
    assert!(!p.exists());
    let l = Ledger::load(p);
    assert_eq!(l.len(), 0);
}

#[test]
fn load_empty_file_returns_empty_ledger() {
    let p = tmp_ledger_path("empty");
    std::fs::write(&p, b"").unwrap();
    let l = Ledger::load(p.clone());
    assert_eq!(l.len(), 0);
    let _ = std::fs::remove_file(&p);
}

#[test]
fn save_in_memory_ledger_is_noop() {
    // in_memory() has no path — save() should silently do nothing, not panic.
    let l = Ledger::in_memory();
    l.save(); // must not panic
    assert_eq!(l.len(), 0);
}

// ---------- corruption ----------

#[test]
fn load_skips_malformed_lines_keeps_valid_ones() {
    let p = tmp_ledger_path("malformed");
    // Mix of valid + invalid lines.
    let content = "\
ks2_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\t0\t1\t100
not a tsv line at all
ks2_bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\t1\t2\t200
malformed\tnonint\tnonint\tnonint
ks2_cccccccccccccccccccccccccccccccc\t0\t3\t300

ks2_dddddddddddddddddddddddddddddddd\t0\t4\t400\textra-field-ignored
";
    std::fs::write(&p, content).unwrap();
    let l = Ledger::load(p.clone());
    // 4 valid lines (a, b, c, d). Malformed and empty lines silently skipped.
    assert_eq!(l.len(), 4, "must keep 4 valid entries, skip the rest");
    // Spot-check: 'a' is Resident, 'b' is Evicted (code 1).
    assert_eq!(l.residency(&"ks2_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string()), Residency::Resident);
    assert_eq!(l.residency(&"ks2_bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_string()), Residency::Evicted);
    let _ = std::fs::remove_file(&p);
}

#[test]
fn load_handles_corrupt_field_types_gracefully() {
    let p = tmp_ledger_path("corrupt-fields");
    let content = "\
ks2_a\tnotbyte\t1\t100
ks2_b\t0\tnotnumber\t100
ks2_c\t0\t1\tnottoken
ks2_d\t0\t1\t100
";
    std::fs::write(&p, content).unwrap();
    let l = Ledger::load(p.clone());
    assert_eq!(l.len(), 1, "only the all-valid line survives, got {}", l.len());
    let _ = std::fs::remove_file(&p);
}

#[test]
fn load_with_too_few_fields_skips_lines() {
    let p = tmp_ledger_path("short");
    let content = "\
ks2_a
ks2_b\t0
ks2_c\t0\t1
ks2_d\t0\t1\t100
";
    std::fs::write(&p, content).unwrap();
    let l = Ledger::load(p.clone());
    // Only the 4-field line is valid (per ledger.rs's `if f.len() >= 4`).
    assert_eq!(l.len(), 1);
    let _ = std::fs::remove_file(&p);
}

#[test]
fn load_with_extra_fields_keeps_them() {
    // ledger.rs uses `if f.len() >= 4` so EXTRA fields are fine — forward-compat
    // for a future schema bump. Verify current tolerance.
    let p = tmp_ledger_path("extra");
    let content = "ks2_a\t0\t1\t100\textra\tfields\there\n";
    std::fs::write(&p, content).unwrap();
    let l = Ledger::load(p.clone());
    assert_eq!(l.len(), 1);
    let _ = std::fs::remove_file(&p);
}

#[test]
fn load_with_no_trailing_newline_works() {
    let p = tmp_ledger_path("no-trail-nl");
    let content = "ks2_a\t0\t1\t100"; // no trailing \n
    std::fs::write(&p, content).unwrap();
    let l = Ledger::load(p.clone());
    assert_eq!(l.len(), 1);
    let _ = std::fs::remove_file(&p);
}

#[test]
fn load_handles_crlf_line_endings() {
    let p = tmp_ledger_path("crlf");
    let content = "ks2_a\t0\t1\t100\r\nks2_b\t0\t2\t200\r\n";
    std::fs::write(&p, content).unwrap();
    let l = Ledger::load(p.clone());
    // text.lines() handles both \n and \r\n, BUT the last \r might get included
    // in the token field. Verify current behavior — should still parse both.
    // (If it doesn't, that's a Windows-on-Windows readback bug.)
    assert!(l.len() >= 1, "CRLF must not break loading: got {} entries", l.len());
    let _ = std::fs::remove_file(&p);
}

// ---------- save round-trip ----------

#[test]
fn save_then_reload_preserves_all_entries() {
    let p = tmp_ledger_path("roundtrip");
    let mut l = Ledger::load(p.clone());
    l.note(&"ks2_a".to_string(), 1, 100);
    l.note(&"ks2_b".to_string(), 2, 200);
    l.evict(&"ks2_b".to_string());
    l.save();

    let l2 = Ledger::load(p.clone());
    assert_eq!(l2.len(), 2);
    assert_eq!(l2.residency(&"ks2_a".to_string()), Residency::Resident);
    assert_eq!(l2.residency(&"ks2_b".to_string()), Residency::Evicted);
    let _ = std::fs::remove_file(&p);
}

#[test]
fn evict_on_unknown_handle_is_noop() {
    let mut l = Ledger::in_memory();
    l.evict(&"ks2_neverseen".to_string()); // must not panic
    assert_eq!(l.residency(&"ks2_neverseen".to_string()), Residency::Unknown);
}

#[test]
fn residency_of_unknown_handle_is_unknown() {
    let l = Ledger::in_memory();
    assert_eq!(l.residency(&"ks2_neverseen".to_string()), Residency::Unknown);
}

// ---------- budget enforcement ----------

#[test]
fn enforce_budget_evicts_oldest_first() {
    let mut l = Ledger::in_memory();
    // Add 5 handles with increasing step.
    for i in 1..=5 {
        l.note(&format!("ks2_{:032x}", i), i as u64, 100);
    }
    // All 5 resident = 500 tokens. Budget of 250 = evict the 3 oldest.
    let evicted = l.enforce_budget(250);
    assert_eq!(evicted, 3, "must evict 3 of 5");
    assert_eq!(l.residency(&format!("ks2_{:032x}", 1)), Residency::Evicted);
    assert_eq!(l.residency(&format!("ks2_{:032x}", 2)), Residency::Evicted);
    assert_eq!(l.residency(&format!("ks2_{:032x}", 3)), Residency::Evicted);
    assert_eq!(l.residency(&format!("ks2_{:032x}", 4)), Residency::Resident);
    assert_eq!(l.residency(&format!("ks2_{:032x}", 5)), Residency::Resident);
}

#[test]
fn enforce_budget_is_noop_when_under_budget() {
    let mut l = Ledger::in_memory();
    l.note(&"ks2_a".to_string(), 1, 100);
    let evicted = l.enforce_budget(1000);
    assert_eq!(evicted, 0);
    assert_eq!(l.residency(&"ks2_a".to_string()), Residency::Resident);
}

#[test]
fn enforce_budget_zero_evicts_everything_resident() {
    let mut l = Ledger::in_memory();
    for i in 1..=10 {
        l.note(&format!("ks2_{:032x}", i), i as u64, 100);
    }
    let evicted = l.enforce_budget(0);
    assert_eq!(evicted, 10);
    for i in 1..=10 {
        assert_eq!(l.residency(&format!("ks2_{:032x}", i)), Residency::Evicted);
    }
}

// ---------- size stress ----------

#[test]
fn ten_thousand_entries_load_and_save() {
    let p = tmp_ledger_path("10k");
    let mut l = Ledger::load(p.clone());
    let start = std::time::Instant::now();
    for i in 0..10_000 {
        l.note(&format!("ks2_{:032x}", i), i as u64, 10);
    }
    l.save();
    let save_dur = start.elapsed();

    let start = std::time::Instant::now();
    let l2 = Ledger::load(p.clone());
    let load_dur = start.elapsed();
    assert_eq!(l2.len(), 10_000);

    // Should be under a second each on a modern machine. The 5-second bound
    // catches a quadratic regression.
    assert!(save_dur.as_secs() < 5, "save 10K entries took {save_dur:?}");
    assert!(load_dur.as_secs() < 5, "load 10K entries took {load_dur:?}");
    let _ = std::fs::remove_file(&p);
}

#[test]
fn enforce_budget_on_10k_entries_terminates_quickly() {
    let mut l = Ledger::in_memory();
    for i in 0..10_000 {
        l.note(&format!("ks2_{:032x}", i), i as u64, 1);
    }
    // 10K resident, budget 5000 -> evict half.
    let start = std::time::Instant::now();
    let evicted = l.enforce_budget(5000);
    let dur = start.elapsed();
    assert!(evicted >= 5000, "should evict at least 5000");
    assert!(dur.as_secs() < 2, "10K budget enforce took {dur:?} — quadratic regression?");
}
