//! Content addressing.
//!
//! Current scheme: `ks2_` + first 32 hex chars of SHA-256 (128-bit truncation). New
//! writes always produce this. The `2` in the prefix is a format-version tag — the next
//! algorithm bump (blake3, longer truncation) can ship as `ks3_` without breaking
//! anything that already shipped.
//!
//! Legacy scheme: `ks_<10 hex>` or `ks_<16 hex>` — first N hex chars of SHA-1. We keep
//! BOTH the hash function (`sha1`/`sha1_hex`) and the read path so old stores still
//! resolve their existing files; we just never produce new `ks_` handles.
//!
//! The verifier (`verify`) routes by handle format, so a `get()` against a legacy file
//! recomputes SHA-1 truncated to the same length and matches byte-exact; a `get()`
//! against a new file recomputes SHA-256 truncated to 32 hex. That's how we keep the
//! byte-exact recall guarantee across the format bump.

use crate::sha256::sha256_hex;

pub type Handle = String;

pub fn sha1(msg: &[u8]) -> [u8; 20] {
    let mut h: [u32; 5] = [0x6745_2301, 0xEFCD_AB89, 0x98BA_DCFE, 0x1032_5476, 0xC3D2_E1F0];
    let ml = (msg.len() as u64).wrapping_mul(8);
    let mut data = msg.to_vec();
    data.push(0x80);
    while data.len() % 64 != 56 {
        data.push(0);
    }
    data.extend_from_slice(&ml.to_be_bytes());

    for chunk in data.chunks(64) {
        let mut w = [0u32; 80];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([chunk[i * 4], chunk[i * 4 + 1], chunk[i * 4 + 2], chunk[i * 4 + 3]]);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }
        let (mut a, mut b, mut c, mut d, mut e) = (h[0], h[1], h[2], h[3], h[4]);
        for (i, &wi) in w.iter().enumerate() {
            let (f, k) = if i < 20 {
                ((b & c) | ((!b) & d), 0x5A82_7999u32)
            } else if i < 40 {
                (b ^ c ^ d, 0x6ED9_EBA1)
            } else if i < 60 {
                ((b & c) | (b & d) | (c & d), 0x8F1B_BCDC)
            } else {
                (b ^ c ^ d, 0xCA62_C1D6)
            };
            let temp = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(wi);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = temp;
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
    }

    let mut out = [0u8; 20];
    for i in 0..5 {
        out[i * 4..i * 4 + 4].copy_from_slice(&h[i].to_be_bytes());
    }
    out
}

pub fn sha1_hex(msg: &[u8]) -> String {
    let mut s = String::with_capacity(40);
    for b in sha1(msg) {
        s.push_str(&format!("{:02x}", b));
    }
    s
}

/// Content-addressed handle for a byte range. New writes always use `ks2_` (SHA-256
/// truncated to 128 bits). Legacy `ks_` handles only appear when reading older stores.
pub fn handle(bytes: &[u8]) -> Handle {
    format!("ks2_{}", &sha256_hex(bytes)[..32])
}

/// Strict format validation, used at CLI / MCP entry points so a malformed handle gets
/// a clear "invalid handle" reject instead of a confusing "no such handle". Three forms
/// are accepted: `ks2_<32 hex>` (new), `ks_<10 hex>` (legacy 40-bit), `ks_<16 hex>`
/// (legacy 64-bit). Anything else is rejected — wrong prefix, wrong length, non-hex.
pub fn is_valid_handle(s: &str) -> bool {
    if let Some(hex) = s.strip_prefix("ks2_") {
        return hex.len() == 32 && hex.chars().all(|c| c.is_ascii_hexdigit());
    }
    if let Some(hex) = s.strip_prefix("ks_") {
        return matches!(hex.len(), 10 | 16) && hex.chars().all(|c| c.is_ascii_hexdigit());
    }
    false
}

/// Render an untrusted handle string for inclusion in an error message. Valid handles
/// are at most 36 chars (`ks2_` + 32 hex), so anything longer is junk the caller can
/// only have produced by mistake or maliciously; surfacing a megabyte of it in stderr
/// or in a JSON-RPC error reply is just amplification noise. We keep the first 64 chars
/// (still wider than any legitimate handle) and append an ellipsis so the user can see
/// how the input started.
pub fn display_handle(s: &str) -> String {
    const MAX_ECHO_CHARS: usize = 64;
    if s.chars().count() <= MAX_ECHO_CHARS {
        return s.to_string();
    }
    let head: String = s.chars().take(MAX_ECHO_CHARS).collect();
    format!("{head}… ({} chars total)", s.chars().count())
}

/// Verify-on-read: `bytes` IS the content `h` addresses, byte-exact. Routed by handle
/// format so legacy SHA-1-derived handles AND new SHA-256-derived handles both verify
/// — that's how the store keeps the exact-or-None guarantee across the format bump.
/// Returns false for any malformed handle (so callers can use this as a single check).
pub fn verify(h: &str, bytes: &[u8]) -> bool {
    if let Some(hex) = h.strip_prefix("ks2_") {
        if hex.len() != 32 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
            return false;
        }
        return hex == &sha256_hex(bytes)[..32];
    }
    if let Some(hex) = h.strip_prefix("ks_") {
        if !hex.chars().all(|c| c.is_ascii_hexdigit()) {
            return false;
        }
        return match hex.len() {
            10 => hex == &sha1_hex(bytes)[..10],
            16 => hex == &sha1_hex(bytes)[..16],
            _ => false,
        };
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn known_answer() {
        // NIST/RFC 3174 vectors.
        assert_eq!(sha1_hex(b""), "da39a3ee5e6b4b0d3255bfef95601890afd80709");
        assert_eq!(sha1_hex(b"abc"), "a9993e364706816aba3e25717850c26c9cd0d89d");
        assert_eq!(
            sha1_hex(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
            "84983e441c3bd26ebaae4aa1f95129e5e54670f1"
        );
    }
    #[test]
    fn new_writes_produce_ks2_with_32_hex() {
        let h = handle(b"hello");
        assert!(h.starts_with("ks2_"), "new writes start with ks2_");
        assert_eq!(h.len(), 4 + 32, "ks2_ + 32 hex = 36 chars total");
        assert!(h.chars().skip(4).all(|c| c.is_ascii_hexdigit()), "hex only after prefix");
    }
    #[test]
    fn different_inputs_give_different_handles() {
        // Cheap sanity that the truncation kept enough entropy — and that we wired the
        // hash, not a constant. Three small inputs are enough to catch the "I returned
        // ks2_0000…" failure mode that a careless refactor could land.
        let a = handle(b"alpha");
        let b = handle(b"beta");
        let c = handle(b"gamma");
        assert_ne!(a, b);
        assert_ne!(b, c);
        assert_ne!(a, c);
    }
    #[test]
    fn is_valid_handle_strict() {
        // ks2_ must be exactly 32 hex
        assert!(is_valid_handle("ks2_0123456789abcdef0123456789abcdef"));
        assert!(!is_valid_handle("ks2_0123456789abcdef0123456789abcde"), "31 hex rejected");
        assert!(!is_valid_handle("ks2_0123456789abcdef0123456789abcdeff"), "33 hex rejected");
        assert!(!is_valid_handle("ks2_0123456789ABCDEF0123456789abcdex"), "non-hex rejected");
        // ks_ accepts only 10 or 16
        assert!(is_valid_handle("ks_0123456789"));
        assert!(is_valid_handle("ks_0123456789abcdef"));
        assert!(!is_valid_handle("ks_012345678"), "9 hex rejected (not legacy)");
        assert!(!is_valid_handle("ks_01234567890"), "11 hex rejected (not legacy)");
        assert!(!is_valid_handle("ks_0123456789abcdef0"), "17 hex rejected (not legacy)");
        // Wrong prefix
        assert!(!is_valid_handle("rk_0123456789"), "rucksack prefix rejected");
        assert!(!is_valid_handle("0123456789"), "no prefix rejected");
        assert!(!is_valid_handle(""), "empty rejected");
    }
    #[test]
    fn display_handle_truncates_oversized_input() {
        // A real handle passes through untouched.
        let real = "ks2_0123456789abcdef0123456789abcdef";
        assert_eq!(display_handle(real), real);
        // A 1 MB junk string must not echo verbatim — it must be bounded.
        let huge = "x".repeat(1_000_000);
        let out = display_handle(&huge);
        assert!(out.len() < 200, "display must bound oversized input, got {} chars", out.len());
        assert!(out.starts_with("xxxx"), "the head is preserved so the user can see what they typed");
        assert!(out.contains("1000000"), "total length is reported");
        // Multi-byte chars are counted by char, not byte.
        let unicode_big = "é".repeat(200);
        let out = display_handle(&unicode_big);
        assert!(out.chars().count() < 200, "unicode counted by char");
    }
    #[test]
    fn verify_routes_by_handle_format() {
        let b = b"hello";
        // New format: SHA-256 truncated to 32
        let h2 = handle(b);
        assert!(verify(&h2, b));
        assert!(!verify(&h2, b"different"));
        // Legacy 10-hex: SHA-1 truncated
        let h_legacy_10 = format!("ks_{}", &sha1_hex(b)[..10]);
        assert!(verify(&h_legacy_10, b), "legacy 10-hex still verifies via SHA-1");
        assert!(!verify(&h_legacy_10, b"other"));
        // Legacy 16-hex: SHA-1 truncated
        let h_legacy_16 = format!("ks_{}", &sha1_hex(b)[..16]);
        assert!(verify(&h_legacy_16, b), "legacy 16-hex still verifies via SHA-1");
        // Malformed
        assert!(!verify("ks2_short", b));
        assert!(!verify("not-a-handle", b));
    }
}
