//! The session ledger persists residency across process invocations as TSV. It must survive
//! a save/load round-trip, tolerate a corrupt/truncated file (skip bad lines, never panic),
//! and evict oldest-first within budget.

use knapsack::{handle, Ledger, Residency};
use std::path::PathBuf;

fn tmp(tag: &str) -> PathBuf {
    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    std::env::temp_dir().join(format!("knapsack-ledger-{}-{}-{}", tag, std::process::id(), t)).join("s.tsv")
}

#[test]
fn save_load_roundtrip_preserves_residency() {
    let p = tmp("rt");
    let (h1, h2) = (handle(b"alpha"), handle(b"beta"));
    {
        let mut l = Ledger::load(p.clone());
        l.note(&h1, 0, 10);
        l.note(&h2, 1, 20);
        l.save();
    }
    let l = Ledger::load(p);
    assert_eq!(l.residency(&h1), Residency::Resident);
    assert_eq!(l.residency(&h2), Residency::Resident);
    assert_eq!(l.resident_tokens(), 30);
    assert_eq!(l.residency(&handle(b"unseen")), Residency::Unknown);
    assert_eq!(l.len(), 2);
}

#[test]
fn evicted_state_persists_and_is_not_counted() {
    let p = tmp("ev");
    let h = handle(b"x");
    {
        let mut l = Ledger::load(p.clone());
        l.note(&h, 0, 5);
        l.evict(&h);
        l.save();
    }
    let l = Ledger::load(p);
    assert_eq!(l.residency(&h), Residency::Evicted);
    assert_eq!(l.resident_tokens(), 0, "evicted handles are not counted as resident");
}

#[test]
fn corrupt_session_file_loads_gracefully() {
    let p = tmp("corrupt");
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    let good = handle(b"good");
    // one valid line surrounded by junk: no-tab line, non-numeric field, too-few fields, blank.
    let content = format!("oops no tabs here\n{good}\t0\t3\t42\nks_bad\tNOTNUM\t1\t1\nks_short\t0\t1\n\n");
    std::fs::write(&p, content).unwrap();
    let l = Ledger::load(p); // must not panic
    assert_eq!(l.residency(&good), Residency::Resident, "the one valid line must load");
    assert_eq!(l.resident_tokens(), 42);
    assert_eq!(l.len(), 1, "every malformed line is skipped");
}

#[test]
fn enforce_budget_evicts_oldest_first() {
    let mut l = Ledger::in_memory();
    let hs: Vec<_> = (0..5)
        .map(|i| {
            let h = handle(format!("b{i}").as_bytes());
            l.note(&h, i as u64, 10);
            h
        })
        .collect();
    assert_eq!(l.resident_tokens(), 50);
    let evicted = l.enforce_budget(25); // over by 25 -> drop oldest three (10+10+10 covers it)
    assert_eq!(evicted, 3);
    for h in &hs[0..3] {
        assert_eq!(l.residency(h), Residency::Evicted, "oldest three evicted");
    }
    for h in &hs[3..5] {
        assert_eq!(l.residency(h), Residency::Resident, "newest two kept");
    }
    assert_eq!(l.resident_tokens(), 20);
    assert_eq!(l.enforce_budget(25), 0, "already within budget -> no further eviction");
}
