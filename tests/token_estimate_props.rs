//! Properties of the token estimator (the unit the whole product is measured in): empty is
//! zero, it's monotonic under append (never undercounts a longer string), and it counts
//! UTF-16 code units exactly — so a surrogate-pair emoji is two units, matching JS charCodeAt
//! (the "0% drift vs Rucksack" claim). Determinism and lossy-byte handling too.

use knapsack::{tokens, tokens_bytes};

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

#[test]
fn empty_is_zero_and_deterministic() {
    assert_eq!(tokens(""), 0);
    assert_eq!(tokens_bytes(b""), 0);
    let s = "a mix of Letters 123 and !@# symbols\twith space";
    assert_eq!(tokens(s), tokens(s), "deterministic");
}

#[test]
fn monotonic_under_append() {
    // Appending any character must never DECREASE the estimate.
    let mut rng = Rng(0x5DEECE66D);
    let alphabet: Vec<char> = "abZ09 \t\n!@#éç".chars().collect();
    for _ in 0..3000 {
        let len = rng.below(40);
        let s: String = (0..len).map(|_| alphabet[rng.below(alphabet.len())]).collect();
        let base = tokens(&s);
        let extra = alphabet[rng.below(alphabet.len())];
        let longer = format!("{s}{extra}");
        assert!(tokens(&longer) >= base, "append must not decrease tokens: {s:?}+{extra:?} ({} < {base})", tokens(&longer));
    }
}

#[test]
fn class_weights_are_ordered() {
    // digits weigh most, then symbols, then space, then letters (per the W_* constants).
    let n = 100;
    let digits: String = "9".repeat(n);
    let symbols: String = "#".repeat(n);
    let spaces: String = " ".repeat(n);
    let letters: String = "a".repeat(n);
    assert!(tokens(&digits) > tokens(&symbols), "digit > symbol");
    assert!(tokens(&symbols) > tokens(&spaces), "symbol > space");
    assert!(tokens(&spaces) > tokens(&letters), "space > letter");
}

#[test]
fn emoji_counts_as_two_utf16_units() {
    // 🚀 (U+1F680) is a surrogate pair -> two UTF-16 units, both symbols.
    // ceil(2 * 0.65) = 2 ; ceil(4 * 0.65) = 3.
    assert_eq!(tokens("🚀"), 2, "one emoji = two symbol units");
    assert_eq!(tokens("🚀🚀"), 3);
    // A BMP accented char is a single unit.
    assert_eq!(tokens("é"), 1);
}

#[test]
fn lossy_bytes_match_replacement_chars() {
    // Invalid UTF-8 decodes to U+FFFD (one UTF-16 unit, a symbol) per bad byte-run.
    assert_eq!(tokens_bytes(&[0xff, 0xff]), tokens("\u{FFFD}\u{FFFD}"));
    // Valid UTF-8 bytes match the str path exactly.
    let s = "héllo 🌍 123";
    assert_eq!(tokens_bytes(s.as_bytes()), tokens(s));
}
