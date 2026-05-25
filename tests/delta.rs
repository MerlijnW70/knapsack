//! The conditional behavior: a re-read after an edit costs ~one changed block, and
//! residency is honored — an evicted block is re-sent, never back-referenced blindly.
use knapsack::content_type::ContentType;
use knapsack::{handle, pack, reconstruct, Ledger, Store};
use std::path::PathBuf;

fn tmp(tag: &str) -> PathBuf {
    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    std::env::temp_dir().join(format!("knapsack-test-{}-{}-{}", tag, std::process::id(), t))
}

fn file(edited: usize) -> String {
    let mut s = String::from("// mod\n\n");
    for i in 0..12 {
        let v = if i < edited { i + 100 } else { i };
        s.push_str(&format!(
            "/** h{i} */\nfunction h{i}(x) {{\n  const a = prepare(x);\n  let acc = 0;\n  for (const it of a) acc += it.w * {v};\n  return finalize(acc);\n}}\n\n",
            i = i,
            v = v
        ));
    }
    s
}

#[test]
fn reread_after_edit_mostly_references() {
    let store = Store::new(tmp("delta"));
    let mut ledger = Ledger::in_memory();

    let v1 = file(0);
    let r1 = pack(v1.as_bytes(), ContentType::Code, &store, &mut ledger, 0);
    assert_eq!(r1.delta_hits, 0, "cold read references nothing");

    let v2 = file(1); // exactly one function changed
    let r2 = pack(v2.as_bytes(), ContentType::Code, &store, &mut ledger, 1);
    assert!(
        r2.delta_hits >= r2.blocks - 2,
        "after a 1-function edit nearly every block should be referenced (hits={}, blocks={})",
        r2.delta_hits,
        r2.blocks
    );
    assert!(r2.shown_tokens_est * 3 < r1.shown_tokens_est, "the re-read should be far cheaper than the first read");
}

#[test]
fn evicted_block_is_resent_not_referenced() {
    let store = Store::new(tmp("evict"));
    let mut ledger = Ledger::in_memory();

    let v = file(0);
    pack(v.as_bytes(), ContentType::Code, &store, &mut ledger, 0);
    let full_hits = pack(v.as_bytes(), ContentType::Code, &store, &mut ledger, 1).delta_hits;

    // Page out the first function block; it must now be re-sent, dropping the hit count.
    let blocks = knapsack::block::split_blocks(v.as_bytes(), ContentType::Code);
    let (s, e) = blocks[1];
    ledger.evict(&handle(&v.as_bytes()[s..e]));
    let after = pack(v.as_bytes(), ContentType::Code, &store, &mut ledger, 2).delta_hits;

    assert!(after < full_hits, "evicting a block must reduce references (was {}, now {})", full_hits, after);
}

// Regression for the dogfood finding: a `cargo test` run after an edit gains a
// leading `Compiling …` line, which under fixed offset chunks shifted every window and
// reset delta to cold (run #3: 0 hits). Content-defined boundaries must keep the
// unchanged test-output blocks deduping despite the inserted header.
#[test]
fn log_dedup_survives_inserted_header() {
    let store = Store::new(tmp("loghdr"));
    let mut ledger = Ledger::in_memory();

    let a = "    Finished `test` profile\n     Running tests/foo.rs\ntest a ... ok\ntest b ... ok\ntest c ... ok\n";
    // identical run, but recompiled first -> one extra header line at the very top
    let b = "   Compiling crate v0.1.0\n    Finished `test` profile\n     Running tests/foo.rs\ntest a ... ok\ntest b ... ok\ntest c ... ok\n";

    let r1 = pack(a.as_bytes(), ContentType::Log, &store, &mut ledger, 0);
    assert_eq!(r1.delta_hits, 0, "cold first read references nothing");

    let r2 = pack(b.as_bytes(), ContentType::Log, &store, &mut ledger, 1);
    assert!(
        r2.delta_hits > 0,
        "inserted header must NOT reset delta to cold (this was the original bug)"
    );
    assert_eq!(
        r2.delta_hits,
        r2.blocks - 1,
        "only the inserted header block is new; every other block is referenced (hits={}, blocks={})",
        r2.delta_hits,
        r2.blocks
    );

    let back = reconstruct(b.as_bytes(), ContentType::Log, &store).expect("all blocks present");
    assert_eq!(back, b.as_bytes(), "reconstruction stays byte-exact under the new boundaries");
}
