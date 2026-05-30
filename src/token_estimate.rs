//! Char-class token estimator, ported from Rucksack's lib/compress.js. Iterates UTF-16
//! code units (`encode_utf16`) to match JS `charCodeAt` exactly, so the Rust benchmark
//! reproduces the JS numbers with 0% estimator drift.
//!
//! This stays the canonical ENGINE counter — `pack`/`structural`/`pack_doc`/`ledger`/
//! `bench` call it directly for deterministic, offline, zero-cost compression decisions.
//! Tokenizer-exact counting (mechanism ⑨) now lives behind the selectable boundary in
//! [`crate::tokenizer`] (`knapsack tokens --tokenizer …`), which wraps this `tokens()`
//! function as its default `Estimate` backend — the reporting surface can opt into exact
//! counts without ever putting a network call or a multi-MB BPE on the hot path.

pub const W_ALPHA: f64 = 0.196;
pub const W_DIGIT: f64 = 0.699;
pub const W_SYMBOL: f64 = 0.65;
pub const W_SPACE: f64 = 0.433;

pub fn tokens(s: &str) -> usize {
    let (mut a, mut d, mut sym, mut sp) = (0usize, 0usize, 0usize, 0usize);
    for u in s.encode_utf16() {
        match u {
            32 | 9 | 10 | 13 => sp += 1,
            48..=57 => d += 1,
            65..=90 | 97..=122 => a += 1,
            _ => sym += 1,
        }
    }
    (a as f64 * W_ALPHA + d as f64 * W_DIGIT + sym as f64 * W_SYMBOL + sp as f64 * W_SPACE).ceil()
        as usize
}

/// Token estimate for raw bytes (lossy-decoded). Used for the "raw" baseline.
pub fn tokens_bytes(b: &[u8]) -> usize {
    tokens(&String::from_utf8_lossy(b))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn matches_classes() {
        // 3 letters -> ceil(3*0.196)=1 ; digits cost more than letters
        assert!(tokens("abcdefghij") < tokens("0123456789"));
        assert_eq!(tokens(""), 0);
    }
}
