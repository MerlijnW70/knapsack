//! Cross-tool interaction: how does knapsack behave when it encounters its
//! OWN markers in input (recursive scenarios), or runs alongside rucksack?
//! These tests stress the boundary cases where two tool layers meet — the
//! places where silent double-wrapping or recursive corruption would hide.

use knapsack::content_type::ContentType;
use knapsack::hook;
use knapsack::ledger::Ledger;
use knapsack::pack::{pack, reconstruct};
use knapsack::pack_doc::pack_doc;
use knapsack::store::Store;

fn tmpstore(tag: &str) -> Store {
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("kn-xtool-{}-{}-{}", tag, std::process::id(), t));
    Store::new(dir)
}

// ---------- the bash hook on a 'knapsack' / 'rucksack' literal ----------

#[test]
fn bash_hook_skips_a_command_already_invoking_knapsack() {
    // Pre-fix dogfood found this: hook.rs::has_shell_meta already lists
    // "knapsack"/"rucksack" as skip-triggers. Verify the contract holds for
    // every flavor.
    for cmd in [
        "knapsack expand ks2_abc",
        "knapsack pack -",
        "rucksack run cargo test",
        "/path/to/knapsack hook",
        "cargo test && knapsack metrics",
    ] {
        assert!(
            !hook::decide(cmd).wrap,
            "must NOT wrap '{cmd}' (would double-wrap)"
        );
    }
}

#[test]
fn bash_hook_skips_command_with_knapsack_substring_anywhere() {
    // Even more aggressive: any literal "knapsack"/"rucksack" anywhere in the
    // command skips. This includes legitimate uses (e.g. cargo test on the
    // knapsack PROJECT itself, where args contain "knapsack" as a directory).
    // Pin this trade-off explicitly so a future "only skip if it LOOKS like
    // a knapsack invocation" refactor is conscious.
    assert!(!hook::decide("cargo test --features knapsack-hook").wrap);
    assert!(!hook::decide("rg --no-knapsack pattern src/").wrap);
    assert!(!hook::decide("cd /home/me/projects/knapsack && cargo build").wrap);
}

// ---------- recursive pack: pack output FED BACK into pack ----------

#[test]
fn packing_packed_output_doesnt_corrupt_anything() {
    // Cold pack a buffer → get the view. Pack the VIEW as a new input.
    // Both passes must reconstruct byte-exact to their respective originals.
    let store = tmpstore("recursive-pack");
    let mut l1 = Ledger::in_memory();
    let original = "test output\n".repeat(100);

    let r1 = pack(original.as_bytes(), ContentType::Log, &store, &mut l1, 0);
    // The view contains a back-ref marker after the first warm pack. Cold pack
    // result is structurally compressed. Both should reconstruct.
    let back =
        reconstruct(original.as_bytes(), ContentType::Log, &store).expect("first reconstruct");
    assert_eq!(back, original.as_bytes(), "first reconstruct byte-exact");

    // Now pack the VIEW. It contains text + maybe a marker.
    let mut l2 = Ledger::in_memory();
    let _r2 = pack(r1.view.as_bytes(), ContentType::Log, &store, &mut l2, 0);
    let back2 =
        reconstruct(r1.view.as_bytes(), ContentType::Log, &store).expect("second reconstruct");
    assert_eq!(back2, r1.view.as_bytes(), "recursive pack still byte-exact");
}

#[test]
fn packing_packed_markdown_via_pack_doc_works() {
    // pack_doc on a real markdown → side-car contains ks-pack header + ks-recall
    // markers. Pack THAT side-car again — what does it look like? It IS still a
    // valid markdown file; pack_doc should treat the markers as just paragraphs.
    let store = tmpstore("recursive-packdoc");
    let original = "# heading\n\n".to_string()
        + &"This is a long paragraph that's going to be elided. ".repeat(20)
        + "\n\n## next\n";
    let r1 = pack_doc("notes.md", original.as_bytes(), &store);
    // The side-car (r1.view) contains the ks-pack header + ks-recall markers.

    // Pack the side-car AGAIN. Should not corrupt anything, just produce
    // another valid side-car (possibly with no new elisions because the
    // visible content is small).
    let r2 = pack_doc("notes.knapsack.md", r1.view.as_bytes(), &store);
    // Both views must be parseable (their handles must resolve via the store).
    assert!(
        store.get(&r1.handle).is_some(),
        "r1 whole-file handle resolves"
    );
    assert!(
        store.get(&r2.handle).is_some(),
        "r2 whole-file handle resolves"
    );
}

// ---------- mixed valid + lookalike markers in input ----------

#[test]
fn input_containing_ks_recall_text_packs_unchanged() {
    // If a user pastes documentation containing the text "[Knapsack: section
    // omitted ...]" into a log, the pack pipeline should treat it as literal
    // text (no special handling). reconstruct must still give back the exact
    // original bytes.
    let store = tmpstore("lookalike");
    let mut l = Ledger::in_memory();
    let input = "first line\n\
                 [Knapsack: 5 lines unchanged · recall ks2_abc] this is just text not a real marker\n\
                 second line\n\
                 third line\n".repeat(20);
    pack(input.as_bytes(), ContentType::Log, &store, &mut l, 0);
    let back = reconstruct(input.as_bytes(), ContentType::Log, &store).expect("must reconstruct");
    assert_eq!(
        back,
        input.as_bytes(),
        "input with marker-like text must reconstruct byte-exact"
    );
}

// ---------- handle for content that no longer exists ----------

#[test]
fn handle_for_evicted_content_returns_none_cleanly() {
    let store = tmpstore("evicted");
    let payload = b"some content";
    let h = store.put(payload);
    assert!(store.get(&h).is_some(), "fresh handle resolves");

    // Delete the block from disk (simulate gc cleanup or manual deletion).
    let hash_start = h.find('_').map(|i| i + 1).unwrap_or(0);
    let shard = h.get(hash_start..hash_start + 2).unwrap_or("00");
    let bp = store.dir().join(shard).join(&h);
    std::fs::remove_file(&bp).unwrap();
    // Now get must return None, not panic, not return wrong bytes from somewhere.
    assert_eq!(store.get(&h), None, "deleted block returns None");
}

#[test]
fn handle_for_completely_unknown_content_returns_none() {
    let store = tmpstore("unknown");
    let fake = "ks2_00000000000000000000000000000000".to_string();
    assert_eq!(store.get(&fake), None);
}

// ---------- the hook is idempotent on its OWN wrapped output ----------

#[test]
fn wrap_command_then_re_decide_skips_the_wrapped_form() {
    // wrap_command produces something like `{ <cmd> ; ... } | "<bin>" pack - ...`.
    // The wrapped form contains literal "knapsack" (via `pack -` arg). If the
    // hook ran a SECOND time on the wrapped form, has_shell_meta should skip
    // it (both because of the pipe AND because of "knapsack" substring).
    let wrapped = hook::wrap_command("cargo test", "/bin/knapsack", "sess-1", "cargo", None);
    let d = hook::decide(&wrapped);
    assert!(!d.wrap, "wrapped form must not re-wrap");
}
