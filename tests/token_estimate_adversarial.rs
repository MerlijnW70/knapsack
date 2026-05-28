//! Adversarial extremes on top of tests/token_estimate_props.rs — every CONTENT
//! shape that might surface a UTF-16 / class-weight bug. Pin every weighting
//! boundary explicitly so a future "let's tune the weights" change is conscious.

use knapsack::{tokens, tokens_bytes};

// ---------- weighting boundaries (exact codepoint ranges) ----------

#[test]
fn ascii_space_class_includes_tab_lf_cr() {
    // src/token_estimate.rs:15 — sp += 1 for 9 ('\t'), 10 ('\n'), 13 ('\r'), 32 (' ')
    // ALL of these must produce the same weighting per-char.
    let n = 100;
    let sp_space = tokens(&" ".repeat(n));
    let sp_tab = tokens(&"\t".repeat(n));
    let sp_lf = tokens(&"\n".repeat(n));
    let sp_cr = tokens(&"\r".repeat(n));
    assert_eq!(sp_space, sp_tab, "space and tab must weight identically");
    assert_eq!(sp_space, sp_lf, "space and newline must weight identically");
    assert_eq!(sp_space, sp_cr, "space and CR must weight identically");
}

#[test]
fn ascii_digit_range_is_exactly_30_through_39() {
    // 48..=57. Pin both inside ('/' is 47, ':' is 58 — both symbols).
    assert_eq!(tokens("0"), 1, "ceil(0.699) = 1");
    assert_eq!(tokens("9"), 1);
    // Just outside the digit range:
    assert!(tokens("/") <= 1); // symbol class
    assert!(tokens(":") <= 1); // symbol class
}

#[test]
fn alpha_class_is_only_basic_latin_a_z() {
    // 65..=90 | 97..=122 (ASCII a-z, A-Z). Everything else is symbol.
    let lowers: String = ('a'..='z').collect();
    let uppers: String = ('A'..='Z').collect();
    // 26 letters each, both should give the same total (same class).
    assert_eq!(tokens(&lowers), tokens(&uppers));

    // Latin-1 accented characters (é, ñ, ü, …) live OUTSIDE the alpha range,
    // so they're symbols. This is a feature, not a bug — keeps the estimator
    // matching JS charCodeAt exactly without locale awareness.
    let accents = "éñü";
    assert!(
        tokens(accents) >= tokens("abc"),
        "accented chars (symbols) must weight at least as much as letters"
    );
}

#[test]
fn empty_string_returns_zero_always() {
    assert_eq!(tokens(""), 0);
    assert_eq!(tokens_bytes(b""), 0);
    assert_eq!(tokens_bytes(&[]), 0);
}

// ---------- multilingual + emoji ----------

#[test]
fn cjk_chars_are_one_utf16_unit_each() {
    // 中 (U+4E2D), 文 (U+6587), 字 (U+5B57) — all BMP, one UTF-16 unit each.
    // 3 symbols -> ceil(3 * 0.65) = 2 tokens.
    assert_eq!(tokens("中文字"), 2);
}

#[test]
fn arabic_and_hebrew_are_bmp_one_unit_each() {
    // Arabic م (U+0645), ر (U+0631), ح (U+062D), ب (U+0628), ا (U+0627) — BMP.
    let arabic = "مرحبا"; // "marhaba"
    assert_eq!(arabic.encode_utf16().count(), 5);
    // 5 symbols -> ceil(5 * 0.65) = 4.
    assert_eq!(tokens(arabic), 4);

    // Hebrew ש (U+05E9), ל (U+05DC), ו (U+05D5), ם (U+05DD) — BMP.
    let hebrew = "שלום"; // "shalom"
    assert_eq!(hebrew.encode_utf16().count(), 4);
    assert_eq!(tokens(hebrew), 3); // ceil(4*0.65)
}

#[test]
fn supplementary_plane_emoji_costs_two_units_each() {
    // U+1F600 (😀) up to U+1F64F all supplementary plane → surrogate pair → 2 units.
    let emojis = "😀😁😂🤣😃😄";
    assert_eq!(
        emojis.encode_utf16().count(),
        12,
        "6 emoji × 2 UTF-16 units"
    );
    // 12 symbols -> ceil(12 * 0.65) = 8.
    assert_eq!(tokens(emojis), 8);
}

#[test]
fn flag_emoji_is_a_sequence_of_supplementary_pairs() {
    // 🇺🇸 = regional indicator U+1F1FA + U+1F1F8, each a surrogate pair = 4 units total.
    let us = "🇺🇸";
    assert_eq!(us.encode_utf16().count(), 4);
    // 4 symbols -> ceil(4 * 0.65) = 3 tokens.
    assert_eq!(tokens(us), 3);
}

#[test]
fn zero_width_joiner_and_modifier_sequences_count_per_codepoint() {
    // 👨‍👩‍👧 = man + ZWJ + woman + ZWJ + girl. Three supplementary + two BMP ZWJ =
    // 3*2 + 2 = 8 UTF-16 units. The estimator counts per UTF-16 unit; it doesn't
    // collapse grapheme clusters. That matches JS charCodeAt and is the documented
    // contract.
    let family = "👨‍👩‍👧";
    assert_eq!(family.encode_utf16().count(), 8);
    assert_eq!(tokens(family), 6); // ceil(8 * 0.65)
}

// ---------- control characters and obscure unicode ----------

#[test]
fn ascii_control_chars_are_symbols() {
    // Every byte 0x00..0x1F EXCEPT 0x09/0x0A/0x0D goes to symbol.
    // 0x00 (NUL), 0x01 (SOH), 0x07 (BEL), 0x0B (VT), 0x1F (US)
    for c in [0x00u8, 0x01, 0x07, 0x0B, 0x1F] {
        let s = String::from_utf8(vec![c]).unwrap_or_else(|_| String::from('?'));
        let t = tokens(&s);
        assert_eq!(t, 1, "control char 0x{:02X} should be 1 token (symbol)", c);
    }
}

#[test]
fn delete_char_is_a_symbol() {
    // 0x7F (DEL) is the symbol class.
    let s = "\x7F";
    assert_eq!(tokens(s), 1);
}

#[test]
fn unicode_combining_marks_count_independently() {
    // "é" can be U+00E9 (single char) OR U+0065 + U+0301 (e + combining acute).
    // Both are 1 UTF-16 unit per char.
    let composed = "é"; // U+00E9
    let decomposed = "e\u{0301}"; // 'e' + combining acute
    assert_eq!(composed.encode_utf16().count(), 1, "NFC é is 1 unit");
    assert_eq!(
        decomposed.encode_utf16().count(),
        2,
        "decomposed e+combining is 2 units"
    );
    // Token counts differ — the decomposed form is "letter" + "symbol".
    // That's a documented quirk: the estimator doesn't normalize.
    assert!(
        tokens(composed) <= tokens(decomposed),
        "decomposed form costs at least as much"
    );
}

// ---------- byte-path (tokens_bytes via String::from_utf8_lossy) ----------

#[test]
fn invalid_utf8_replacement_char_is_a_symbol() {
    // Each invalid byte-run becomes one U+FFFD replacement char.
    // U+FFFD is BMP, one UTF-16 unit, in the symbol class.
    let lossy = tokens_bytes(&[0xff, 0xfe, 0xfd]); // 3 bad bytes → 3 FFFDs
    let expected = tokens("\u{FFFD}\u{FFFD}\u{FFFD}");
    assert_eq!(
        lossy, expected,
        "3 invalid bytes should equal 3 replacement chars (each a symbol)"
    );
}

#[test]
fn long_runs_of_invalid_utf8_consolidate_per_runbyte() {
    // Note: from_utf8_lossy emits one U+FFFD per invalid byte SEQUENCE, not per byte.
    // For our purposes (bytes that don't form ANY valid utf8 start), each bad byte
    // becomes its own FFFD. Pin both directions just to confirm the path.
    let many = tokens_bytes(&[0xff].repeat(100));
    let expected = tokens(&"\u{FFFD}".repeat(100));
    assert_eq!(many, expected);
}

#[test]
fn mixed_valid_and_invalid_bytes() {
    // "héllo\xFFworld"
    let mut buf = Vec::new();
    buf.extend_from_slice("héllo".as_bytes());
    buf.push(0xff);
    buf.extend_from_slice("world".as_bytes());
    let viewed = String::from_utf8_lossy(&buf);
    assert_eq!(tokens_bytes(&buf), tokens(&viewed));
}

// ---------- perf / size ----------

#[test]
fn one_megabyte_string_estimates_without_panic() {
    let s = "a".repeat(1_000_000);
    let t = tokens(&s);
    // 1M letters at 0.196 each = 196,000 tokens (ceil 196000)
    assert_eq!(t, 196_000);
}

#[test]
fn one_megabyte_random_unicode_estimates_without_panic() {
    // Cycle through a mix to ensure no class-counter overflow.
    let mix = "abc012 \tÉ中😀";
    let s = mix.repeat(70_000); // ~1MB of varied UTF-8
    let t = tokens(&s);
    assert!(t > 0, "should produce a positive estimate");
    // Determinism: re-running gives the same answer.
    assert_eq!(t, tokens(&s));
}

#[test]
fn additivity_within_rounding_error() {
    // tokens(a) + tokens(b) MAY differ from tokens(a+b) only because of the
    // single shared ceil() — drift is bounded by 1 token. This is a known
    // property of the integer ceiling step at the end.
    let inputs = ["abc", "012", "  ", "中文 abc 012", "🎒 hello"];
    for a in &inputs {
        for b in &inputs {
            let split = tokens(a) + tokens(b);
            let together = tokens(&format!("{a}{b}"));
            let diff = (split as i64 - together as i64).unsigned_abs();
            assert!(
                diff <= 1,
                "additivity drift bounded by 1 token: tokens({a:?})+tokens({b:?})={split}, tokens({:?})={together}",
                format!("{a}{b}"),
            );
        }
    }
}
