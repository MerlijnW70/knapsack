//! The non-negotiable invariant, strengthened from "every non-blank line recoverable"
//! to BYTE-EXACT: the whole input reconstructs bit-for-bit from the store, and every
//! elision handle expands to exactly the bytes it stands for — including CRLF and
//! no-trailing-newline inputs that a normalizing view would mangle.
use knapsack::content_type::ContentType;
use knapsack::{pack, reconstruct, structural, Ledger, Store};
use std::path::PathBuf;

fn tmp(tag: &str) -> PathBuf {
    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    std::env::temp_dir().join(format!("knapsack-test-{}-{}-{}", tag, std::process::id(), t))
}

fn sample_code(crlf: bool) -> Vec<u8> {
    let nl = if crlf { "\r\n" } else { "\n" };
    let mut s = String::new();
    for i in 0..8 {
        s.push_str(&format!("/** doc {i} */{nl}function f{i}(x) {{{nl}", i = i, nl = nl));
        s.push_str(&format!("  const a = prepare(x);{nl}  let acc = 0;{nl}", nl = nl));
        s.push_str(&format!("  for (const it of a) {{ acc += it.w * {i}; }}{nl}", i = i, nl = nl));
        s.push_str(&format!("  return finalize(acc);{nl}}}{nl}{nl}", nl = nl));
    }
    s.into_bytes()
}

#[test]
fn whole_input_reconstructs_byte_exact() {
    for crlf in [false, true] {
        let store = Store::new(tmp("faithful"));
        let mut ledger = Ledger::in_memory();
        let input = sample_code(crlf);
        pack(&input, ContentType::Code, &store, &mut ledger, 0);
        let back = reconstruct(&input, ContentType::Code, &store).expect("all blocks present");
        assert_eq!(back, input, "byte-exact reconstruction (crlf={})", crlf);
    }
}

#[test]
fn no_trailing_newline_is_preserved() {
    let store = Store::new(tmp("notrail"));
    let mut ledger = Ledger::in_memory();
    let input = b"line one\nline two\nlast line no newline".to_vec();
    pack(&input, ContentType::Log, &store, &mut ledger, 0);
    let back = reconstruct(&input, ContentType::Log, &store).unwrap();
    assert_eq!(back, input);
}

#[test]
fn every_elision_handle_is_byte_exact() {
    let store = Store::new(tmp("elision"));
    let input = sample_code(true); // CRLF: a normalizing path would corrupt this
    let (_view, elisions) = structural::compress(&input, 0, input.len(), ContentType::Code);
    assert!(!elisions.is_empty(), "the sample should produce body elisions");
    for el in elisions {
        store.put_with_handle(&el.handle, &input[el.start..el.end]);
        let got = store.get(&el.handle).unwrap();
        assert_eq!(got, &input[el.start..el.end], "elision must store exact bytes");
    }
}

#[test]
fn session_steps_all_recover() {
    let store = Store::new(tmp("session"));
    let mut ledger = Ledger::in_memory();
    let steps: Vec<(Vec<u8>, ContentType)> = vec![
        (sample_code(false), ContentType::Code),
        (b"> test\n\nPASS a\nPASS b\nFAIL c\nTests: 2 passed".to_vec(), ContentType::Log),
        (sample_code(false), ContentType::Code), // re-read (all referenced)
    ];
    for (i, (bytes, ct)) in steps.iter().enumerate() {
        pack(bytes, *ct, &store, &mut ledger, i as u64);
        let back = reconstruct(bytes, *ct, &store).expect("recoverable");
        assert_eq!(&back, bytes, "step {} byte-exact", i);
    }
}
