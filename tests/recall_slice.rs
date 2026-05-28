//! Recall slicing edge cases: line ranges (out-of-range, reversed, zero), grep (literal not
//! regex, case-insensitive incl. unicode, no match), grep+lines combined, and the non-UTF-8
//! fallback to exact bytes. Slicing must never panic and must be byte/line accurate.

use knapsack::recall::{expand, parse_range, RecallOut};
use knapsack::Store;

fn store(tag: &str) -> Store {
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    Store::new(std::env::temp_dir().join(format!(
        "knapsack-slice-{}-{}-{}",
        tag,
        std::process::id(),
        t
    )))
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
    assert_eq!(
        text(expand(&s, &h, Some((2, 4)), None, 0)),
        "l2\nl3\nl4",
        "1-based inclusive"
    );
    assert_eq!(
        text(expand(&s, &h, Some((0, 2)), None, 0)),
        "l1\nl2",
        "a=0 clamps to 1"
    );
    assert_eq!(
        text(expand(&s, &h, Some((3, 999)), None, 0)),
        "l3\nl4\nl5",
        "b clamps to end"
    );
    assert_eq!(
        text(expand(&s, &h, Some((4, 2)), None, 0)),
        "",
        "reversed range -> empty"
    );
    assert_eq!(
        text(expand(&s, &h, Some((50, 60)), None, 0)),
        "",
        "out of range -> empty"
    );
    assert_eq!(
        text(expand(&s, &h, Some((1, 1)), None, 0)),
        "l1",
        "single line"
    );
}

#[test]
fn grep_is_regex_with_substring_fallback() {
    // Updated contract: grep is a real (subset) regex — `.` is a metachar. Plain words
    // still behave like substring search because they have no metacharacters.
    let s = store("grep");
    let h = s.put(b"a.b\naxb\nzzz");

    // `.` as a regex matches any char → both "a.b" and "axb" match; "zzz" does not.
    let got = text(expand(&s, &h, None, Some("a.b"), 0));
    assert!(
        got.contains("a.b"),
        "regex `a.b` matches literal `a.b`: {got:?}"
    );
    assert!(
        got.contains("axb"),
        "regex `a.b` matches `axb` via `.`: {got:?}"
    );
    assert!(!got.contains("zzz"), "non-matching line excluded: {got:?}");

    // Escape `.` to recover literal-only matching.
    let got = text(expand(&s, &h, None, Some("a\\.b"), 0));
    assert!(
        got.contains("a.b"),
        "escaped `\\.` matches literal `.`: {got:?}"
    );
    assert!(
        !got.contains("axb"),
        "escaped `\\.` does NOT match `x`: {got:?}"
    );
}

#[test]
fn grep_falls_back_to_substring_on_unsupported_regex() {
    // If the pattern uses metacharacters we don't implement (`|`, `()`, `{n,m}`), we
    // fall back to case-insensitive substring matching so the call doesn't fail
    // silently. The compiler error is suppressed; the user sees substring semantics.
    let s = store("grepfallback");
    let h = s.put(b"alpha\n(grouped)\nbeta");
    // `(grouped)` as a regex would be a group — unsupported. As substring, it matches.
    let got = text(expand(&s, &h, None, Some("(grouped)"), 0));
    assert_eq!(got, "(grouped)", "unsupported regex -> substring fallback");
}

#[test]
fn grep_case_insensitive_including_unicode() {
    let s = store("case");
    let h = s.put("café\nCAFÉ\ntea".as_bytes());
    let got = text(expand(&s, &h, None, Some("CAFÉ"), 0));
    assert!(
        got.contains("café") && got.contains("CAFÉ"),
        "case-insensitive incl. unicode folding: {got:?}"
    );
    assert!(!got.contains("tea"));
}

#[test]
fn grep_with_context_and_no_match() {
    let s = store("ctx");
    let h = s.put(b"a\nb\nMATCH\nd\ne");
    assert_eq!(
        text(expand(&s, &h, None, Some("match"), 1)),
        "b\nMATCH\nd",
        "context=1 around the match"
    );
    assert_eq!(
        text(expand(&s, &h, None, Some("nope"), 2)),
        "",
        "no match -> empty"
    );
}

#[test]
fn lines_window_first_then_grep_filters_inside() {
    // Order contract: `--lines A-B` defines a 1-based window into the ORIGINAL line
    // numbering, and `--grep` filters within that window. If grep applied first
    // (the old order), the lines index would index into the post-grep vector — a
    // request like `--lines 50-100 --grep TARGET` against a 200-line file where
    // only 3 lines match would index a 3-element vec from [49..] and return empty.
    // This is the regression test for that bug.
    let s = store("compose");
    // 10 lines; only 3 contain "MATCH" — at indices 1, 5, 8 (1-based).
    let h = s.put(b"a\nMATCH-1\nb\nc\nd\nMATCH-2\ne\nf\nMATCH-3\ng");

    // Whole file -> grep gives 3 matches.
    assert_eq!(
        text(expand(&s, &h, None, Some("MATCH"), 0)),
        "MATCH-1\nMATCH-2\nMATCH-3"
    );

    // Window 4-7 selects "c\nd\ne\nMATCH-2". Grep inside should yield ONLY MATCH-2.
    // Under the OLD order (grep first), this would index a 3-element vec at [3..7]
    // and return empty.
    assert_eq!(
        text(expand(&s, &h, Some((4, 7)), Some("MATCH"), 0)),
        "MATCH-2",
        "--lines 4-7 + --grep MATCH must return only the in-window match"
    );

    // Window 4-7 + grep + context=1 -> MATCH-2 plus its in-window neighbours d, e.
    assert_eq!(
        text(expand(&s, &h, Some((4, 7)), Some("MATCH"), 1)),
        "d\nMATCH-2\ne",
        "context expands within the line window"
    );

    // Window that contains no matches returns empty (not a panic, not whole-file).
    assert_eq!(
        text(expand(&s, &h, Some((3, 5)), Some("MATCH"), 0)),
        "",
        "window with no matches -> empty"
    );

    // Empty post-slice (out-of-range) with grep+context must NOT panic on `n-1`.
    assert_eq!(
        text(expand(&s, &h, Some((1000, 2000)), Some("MATCH"), 5)),
        "",
        "empty post-slice survives context branch"
    );
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
        assert_eq!(
            parse_range(junk),
            None,
            "junk range {junk:?} must be rejected"
        );
    }
}
