//! Foundational invariant: `split_blocks` must TILE the input exactly — the ranges, in
//! order, cover [0, len) with no gap and no overlap, and concatenating the sub-slices
//! reproduces the input byte-for-byte. Everything (reconstruct, delta, recall) rests on this.

use knapsack::block::split_blocks;
use knapsack::content_type::ContentType;

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

fn gen(rng: &mut Rng, max: usize) -> Vec<u8> {
    let len = rng.below(max + 1);
    let mut v = Vec::with_capacity(len);
    while v.len() < len {
        match rng.below(9) {
            0 => v.push(b'\n'),
            1 => v.extend_from_slice(b"\r\n"),
            2 => v.push(0),
            3 => v.extend_from_slice("ünïçödé ⟨ks⟩".as_bytes()),
            4 => v.extend_from_slice(b"    test foo ... ok"),
            5 => v.extend_from_slice(b"function f() {"),
            _ => v.push(rng.below(256) as u8),
        }
    }
    v.truncate(len);
    v
}

fn assert_tiles(bytes: &[u8], ct: ContentType) {
    let blocks = split_blocks(bytes, ct);
    // Concatenation of every block's bytes must equal the input exactly.
    let mut cat = Vec::with_capacity(bytes.len());
    for &(s, e) in &blocks {
        assert!(s <= e, "range must be non-inverted: ({s},{e})");
        assert!(e <= bytes.len(), "range must be in bounds: ({s},{e}) len {}", bytes.len());
        cat.extend_from_slice(&bytes[s..e]);
    }
    assert_eq!(cat, bytes, "blocks must concatenate to the EXACT input (ct={ct:?}, len {})", bytes.len());
    if !bytes.is_empty() {
        assert_eq!(blocks.first().unwrap().0, 0, "first block starts at 0");
        assert_eq!(blocks.last().unwrap().1, bytes.len(), "last block ends at len");
        for w in blocks.windows(2) {
            assert_eq!(w[0].1, w[1].0, "blocks must tile contiguously, no gap/overlap");
        }
    }
}

#[test]
fn split_blocks_tiles_exactly_fuzz() {
    let mut rng = Rng(0xCAFEF00DBA5EBA11);
    for _ in 0..4000 {
        let b = gen(&mut rng, 1500);
        assert_tiles(&b, ContentType::Log);
        assert_tiles(&b, ContentType::Code);
    }
}

#[test]
fn split_blocks_tiles_adversarial_fixed() {
    let cases: Vec<&[u8]> = vec![
        b"",
        b"\n",
        b"\r\n\r\n",
        b"a",
        b"no newline at end",
        b"\0\0\0",
        "emoji 🚀 and ⟨markers⟩\nsecond".as_bytes(),
        b"test a ... ok\ntest b ... ok\n",
    ];
    for c in cases {
        assert_tiles(c, ContentType::Log);
        assert_tiles(c, ContentType::Code);
    }
}
