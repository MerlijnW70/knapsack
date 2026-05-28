//! `structural::compress` produces a LOSSY view plus a list of byte-exact elisions. The
//! view can drop anything, but each elision must (a) be an in-bounds, ordered, non-overlapping
//! range of the input and (b) be content-addressed — its handle is the SHA-1 of the bytes it
//! names. (b) is what makes verify-on-read and recall sound for the stateless/guard path.

use knapsack::content_type::ContentType;
use knapsack::hash::handle;
use knapsack::structural::compress;

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

/// Code-ish and log-ish content so both `compress` branches get exercised.
fn gen(rng: &mut Rng) -> Vec<u8> {
    let lines = 1 + rng.below(80);
    let mut s = String::new();
    for i in 0..lines {
        match rng.below(6) {
            0 => s.push_str(&format!("function handler{i}(a, b) {{\n")),
            1 => s.push_str("  return compute(a) + finalize(b);\n"),
            2 => s.push_str("}\n"),
            3 => s.push_str(&format!("PASS src/test{i}.js ({} ms)\n", i * 3)),
            4 => s.push('\n'),
            _ => s.push_str(&format!(
                "[INFO] event {i} processed status=ok value={}\n",
                i * 7
            )),
        }
    }
    s.into_bytes()
}

fn check(bytes: &[u8], ct: ContentType) {
    let (_view, elisions) = compress(bytes, 0, bytes.len(), ct);
    let mut prev_end = 0usize;
    for el in &elisions {
        assert!(
            el.start <= el.end,
            "elision range non-inverted: ({},{})",
            el.start,
            el.end
        );
        assert!(
            el.end <= bytes.len(),
            "elision in bounds: ({},{}) len {}",
            el.start,
            el.end,
            bytes.len()
        );
        assert!(
            el.start >= prev_end,
            "elisions must be ordered and non-overlapping"
        );
        prev_end = el.end;
        assert_eq!(
            handle(&bytes[el.start..el.end]),
            el.handle,
            "elision handle MUST be the content hash of the bytes it names"
        );
    }
}

#[test]
fn elisions_are_in_bounds_ordered_and_content_addressed_fuzz() {
    let mut rng = Rng(0x0DDB1A5E5EED5EED);
    for _ in 0..2000 {
        let b = gen(&mut rng);
        check(&b, ContentType::Code);
        check(&b, ContentType::Log);
    }
}

#[test]
fn compress_handles_edges_without_panic() {
    for c in [&b""[..], b"\n", b"x", b"one line only", b"a\nb\nc\n"] {
        let _ = compress(c, 0, c.len(), ContentType::Code);
        let _ = compress(c, 0, c.len(), ContentType::Log);
    }
}
