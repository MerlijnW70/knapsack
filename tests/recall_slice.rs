//! Recall slicing edge cases: line ranges (out-of-range, reversed, zero), grep (literal not
//! regex, case-insensitive incl. unicode, no match), grep+lines combined, and the non-UTF-8
//! fallback to exact bytes. Slicing must never panic and must be byte/line accurate.

use knapsack::recall::{expand, parse_range, RecallOut};
use knapsack::Store;

fn store(tag: &str) -> Store {
    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    Store::new(std::env::temp_dir().join(format!("knapsack-slice-{}-{}-{}", tag, std::process::id(), t)))
}

fn text(out: Option<RecallOut>) -> String {
    match out.expect("some") {
        RecallOut::Text(t) => t,
        RecallOut::Bytes(b) => String::from_utf8_lossy(&b).into_owned(),
    }
}

#[test]
fn full_recall_is_exact_bytes() {
    let s = store("full");
    let h = s.put(b"l1\nl2\nl3");
    match expand(&s, &h, None, None, 0).unwrap() {
        RecallOut::Bytes(b) => assert_eq!(b, b"l1\nl2\nl3"),
        RecallOut::Text(_) => panic!("full recall must return exact Bytes, not Text"),
    }
}

#[test]
fn line_ranges_inclusive_and_clamped() {
    let s = store("lines");
    let h = s.put(b"l1\nl2\nl3\nl4\nl5");
    assert_eq!(text(expand(&s, &h, Some((2, 4)), None, 0)), "l2\nl3\nl4", "1-based inclusive");
    assert_eq!(text(expand(&s, &h, Some((0, 2)), None, 0)), "l1\nl2", "a=0 clamps to 1");
    assert_eq!(text(expand(&s, &h, Some((3, 999)), None, 0)), "l3\nl4\nl5", "b clamps to end");
    assert_eq!(text(expand(&s, &h, Some((4, 2)), None, 0)), "", "reversed range -> empty");
    assert_eq!(text(expand(&s, &h, Some((50, 60)), None, 0)), "", "out of range -> empty");
    assert_eq!(text(expand(&s, &h, Some((1, 1)), None, 0)), "l1", "single line");
}

#[test]
fn grep_is_literal_not_regex() {
    let s = store("grep");
    let h = s.put(b"a.b\naxb\nzzz");
    // "a.b" as a regex would match "axb"; as a literal it must not.
    assert_eq!(text(expand(&s, &h, None, Some("a.b"), 0)), "a.b", "grep is a literal substring match");
}

#[test]
fn grep_case_insensitive_including_unicode() {
    let s = store("case");
    let h = s.put("café\nCAFÉ\ntea".as_bytes());
    let got = text(expand(&s, &h, None, Some("CAFÉ"), 0));
    assert!(got.contains("café") && got.contains("CAFÉ"), "case-insensitive incl. unicode folding: {got:?}");
    assert!(!got.contains("tea"));
}

#[test]
fn grep_with_context_and_no_match() {
    let s = store("ctx");
    let h = s.put(b"a\nb\nMATCH\nd\ne");
    assert_eq!(text(expand(&s, &h, None, Some("match"), 1)), "b\nMATCH\nd", "context=1 around the match");
    assert_eq!(text(expand(&s, &h, None, Some("nope"), 2)), "", "no match -> empty");
}

#[test]
fn grep_then_lines_compose() {
    let s = store("compose");
    let h = s.put(b"keep1\nkeep2\nkeep3\nkeep4");
    // grep keeps all 4, then lines 2-3 slices the filtered set.
    assert_eq!(text(expand(&s, &h, Some((2, 3)), Some("keep"), 0)), "keep2\nkeep3");
}

#[test]
fn non_utf8_slicing_falls_back_to_exact_bytes() {
    let s = store("binary");
    let blob = vec![0x00, 0xff, 0xfe, b'\n', 0x80, 0x81];
    let h = s.put(&blob);
    // Asking for lines on non-UTF-8 content returns the FULL exact bytes, not a lossy slice.
    match expand(&s, &h, Some((1, 1)), None, 0).unwrap() {
        RecallOut::Bytes(b) => assert_eq!(b, blob, "non-UTF-8 slicing falls back to exact bytes"),
        RecallOut::Text(_) => panic!("must not lossily decode non-UTF-8 for slicing"),
    }
}

#[test]
fn unknown_handle_is_none() {
    let s = store("none");
    assert!(expand(&s, &"ks_missing00".to_string(), Some((1, 1)), None, 0).is_none());
}

#[test]
fn parse_range_accepts_valid_rejects_junk() {
    assert_eq!(parse_range("2-5"), Some((2, 5)));
    assert_eq!(parse_range("  2 - 5 "), Some((2, 5)));
    assert_eq!(parse_range("0-0"), Some((0, 0)));
    for junk in ["abc", "2-", "-5", "2-5-8", "", "-", "2..5"] {
        assert_eq!(parse_range(junk), None, "junk range {junk:?} must be rejected");
    }
}
