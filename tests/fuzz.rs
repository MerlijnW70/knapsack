//! Property / fuzz coverage for the one non-negotiable invariant:
//! for ANY input bytes and ANY content type, pack-then-reconstruct is byte-exact.
//!
//! The compact VIEW may be lossy; the STORE and `reconstruct` must not be. These tests
//! throw random and deliberately adversarial bytes at that path — including inputs that
//! contain Knapsack's own view markers, so a confused parser can't sneak corruption in.
//! A dependency-free xorshift PRNG keeps the run deterministic and matches the zero-dep core.

use knapsack::content_type::ContentType;
use knapsack::{pack, reconstruct, Ledger, Store};
use std::path::PathBuf;

fn store_dir(tag: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("knapsack-fuzz-{}-{}", tag, std::process::id()));
    p
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

/// Generate a body biased toward realistic structure: many short lines, occasional blank
/// lines and long runs, a spread of byte values including NUL, high bytes and newlines.
fn gen(rng: &mut Rng, max_len: usize) -> Vec<u8> {
    let len = rng.below(max_len + 1);
    let mut v = Vec::with_capacity(len);
    while v.len() < len {
        match rng.below(10) {
            0 => v.push(b'\n'),
            1 => v.extend_from_slice(b"\r\n"),
            2 => v.push(0), // embedded NUL
            3 => v.extend_from_slice("café ⟨ks⟩ · résumé".as_bytes()),
            4..=6 => {
                // a "word" of printable ascii
                for _ in 0..rng.below(12) {
                    v.push(b'!' + (rng.below(93) as u8));
                }
                v.push(b' ');
            }
            _ => v.push(rng.below(256) as u8), // arbitrary byte, incl. invalid UTF-8
        }
    }
    v.truncate(len);
    v
}

fn assert_roundtrip(input: &[u8], ct: ContentType, store: &Store, ledger: &mut Ledger, step: u64) {
    let r = pack(input, ct, store, ledger, step);
    let back = reconstruct(input, ct, store)
        .unwrap_or_else(|| panic!("reconstruct returned None for {}-byte input (ct={:?})", input.len(), ct));
    assert_eq!(
        back,
        input,
        "byte-exact violated: {}-byte input (ct={:?}), view was {} blocks / {} delta hits",
        input.len(),
        ct,
        r.blocks,
        r.delta_hits
    );
}

#[test]
fn fuzz_reconstruct_is_byte_exact() {
    let store = Store::new(store_dir("rand"));
    let mut ledger = Ledger::in_memory();
    let mut rng = Rng(0x9E3779B97F4A7C15);
    for i in 0..1500u64 {
        let input = gen(&mut rng, 3000);
        let ct = if rng.below(2) == 0 { ContentType::Log } else { ContentType::Code };
        assert_roundtrip(&input, ct, &store, &mut ledger, i);
    }
}

#[test]
fn adversarial_inputs_reconstruct_exact() {
    let store = Store::new(store_dir("adv"));
    let mut ledger = Ledger::in_memory();

    // A literal-looking knapsack backref marker among the byte stream — the marker text
    // must not poison tiling/reconstruction (we never special-case our own output).
    let glyph_line = "[Knapsack: 5 lines unchanged · recall ks_deadbeef]\n";
    let cases: Vec<Vec<u8>> = vec![
        b"".to_vec(),                                   // empty
        b"\n".to_vec(),                                 // lone newline
        b"\r\n\r\n\r\n".to_vec(),                       // CRLF only
        b"\r\r\r".to_vec(),                             // lone CR
        b"no trailing newline".to_vec(),               // missing final \n
        vec![0u8; 64],                                  // all NUL
        b"\xEF\xBB\xBFwith BOM\n".to_vec(),            // UTF-8 BOM prefix
        vec![0xFF, 0xFE, 0x00, 0x01, 0x80, 0x7F],     // invalid UTF-8 / binary
        glyph_line.as_bytes().to_vec(),                // input that IS a fake recall marker
        glyph_line.repeat(50).into_bytes(),            // many fake markers
        format!("{0}{0}{0}", "x".repeat(5000)).into_bytes(), // one huge line
        "\n".repeat(2000).into_bytes(),                // thousands of blank lines
        (0..=255u8).cycle().take(8192).collect(),     // every byte value, repeated
    ];

    for (n, input) in cases.iter().enumerate() {
        for ct in [ContentType::Log, ContentType::Code] {
            let r = pack(input, ct, &store, &mut ledger, n as u64);
            let back = reconstruct(input, ct, &store)
                .unwrap_or_else(|| panic!("case #{n} ct={ct:?}: reconstruct None"));
            assert_eq!(back, *input, "case #{n} ct={ct:?}: not byte-exact (blocks={})", r.blocks);
        }
    }
}

#[test]
fn large_inputs_reconstruct_byte_exact() {
    // Exercise reconstruct through the sharded store at real scale (64 KB .. 1 MB).
    let store = Store::new(store_dir("large"));
    let mut ledger = Ledger::in_memory();
    let mut rng = Rng(0x243F6A8885A308D3);
    for (i, size) in [64 * 1024usize, 256 * 1024, 1024 * 1024].into_iter().enumerate() {
        let mut input = Vec::with_capacity(size + 64);
        while input.len() < size {
            for _ in 0..rng.below(40) {
                input.push(b'!' + (rng.below(90) as u8));
            }
            input.push(b'\n');
            if rng.below(50) == 0 {
                input.push(rng.next() as u8); // sprinkle a non-UTF-8 byte
            }
        }
        input.truncate(size);
        let ct = if i % 2 == 0 { ContentType::Log } else { ContentType::Code };
        let r = pack(&input, ct, &store, &mut ledger, i as u64);
        let back = reconstruct(&input, ct, &store).expect("all blocks present");
        assert_eq!(back, input, "{size}-byte input not byte-exact (blocks={})", r.blocks);
    }
}

#[test]
fn mutating_session_stays_exact_and_warms_up() {
    // Simulate an edit->test loop: the same growing/mutating buffer re-packed across steps
    // against a shared ledger. Each step must reconstruct to its CURRENT bytes, and after
    // the first step the conditional path must actually fire (delta hits > 0 somewhere).
    let store = Store::new(store_dir("session"));
    let mut ledger = Ledger::in_memory();
    let mut rng = Rng(0xD1B54A32D192ED03);

    let mut buf = gen(&mut rng, 1500);
    let mut total_delta = 0usize;
    for step in 0..30u64 {
        // mutate: append a line and flip a random interior byte
        buf.extend_from_slice(format!("line {step} appended\n").as_bytes());
        if !buf.is_empty() {
            let idx = rng.below(buf.len());
            buf[idx] ^= 0x20;
        }
        let r = pack(&buf, ContentType::Log, &store, &mut ledger, step);
        total_delta += r.delta_hits;
        let back = reconstruct(&buf, ContentType::Log, &store).expect("present");
        assert_eq!(back, buf, "step {step}: reconstruction drifted from current bytes");
    }
    assert!(total_delta > 0, "a 30-step edit loop never hit the conditional path");
}
