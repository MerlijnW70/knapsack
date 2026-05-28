//! Adversarial dogfood for the in-tree regex (src/regex.rs).
//!
//! Goals:
//! - Pin every documented-supported behavior (literals, dot, anchors, classes,
//!   shorthand, quantifiers, case-insensitivity, escapes).
//! - Verify unsupported metacharacters error cleanly (so callers can fall back
//!   to substring matching).
//! - Catch perf cliffs: a greedy backtracker without atomic groups can blow up
//!   on `a*a*a*…` style patterns. Each "perf" test asserts under a wall-clock
//!   ceiling so a future refactor that introduces exponential behavior fails
//!   visibly here instead of hanging in production.

use knapsack::regex::Regex;
use std::time::{Duration, Instant};

fn r(p: &str) -> Regex {
    Regex::new(p).unwrap_or_else(|_| panic!("regex compile must succeed: {p}"))
}

fn ri(p: &str) -> Regex {
    Regex::new_ignore_case(p).unwrap_or_else(|_| panic!("regex compile must succeed: {p}"))
}

/// Run `f` and assert it completes inside `cap`. Panic with a helpful message
/// otherwise — that's the ReDoS / perf-cliff alarm.
fn within<F: FnOnce() -> bool>(cap: Duration, label: &str, f: F) -> bool {
    let t = Instant::now();
    let r = f();
    let elapsed = t.elapsed();
    assert!(
        elapsed < cap,
        "{label}: took {elapsed:?}, cap was {cap:?} — regex backtracking is exponential"
    );
    r
}

// ---------- compile / empty edge cases ----------

#[test]
fn empty_pattern_matches_empty_input() {
    let re = r("");
    assert!(re.is_match(""), "empty pattern matches empty input");
    assert!(
        re.is_match("anything"),
        "empty pattern matches anywhere (zero-length match)"
    );
}

#[test]
fn empty_input_against_nonempty_pattern() {
    assert!(!r("abc").is_match(""));
    assert!(r("a*").is_match(""), "zero-or-more on empty input is fine");
    assert!(!r("a+").is_match(""), "one-or-more on empty input is not");
    assert!(r("a?").is_match(""), "zero-or-one on empty input is fine");
}

#[test]
fn anchored_empty_input_with_anchor() {
    // `^$` on empty input is a match.
    assert!(r("^$").is_match(""));
    // `^$` on non-empty input is NOT a match (line has content).
    assert!(!r("^$").is_match("nonempty"));
}

#[test]
fn just_anchors() {
    assert!(r("^").is_match("anything"));
    assert!(r("$").is_match("anything"));
    assert!(r("^").is_match(""));
    assert!(r("$").is_match(""));
}

// ---------- quantifier edge cases ----------

#[test]
fn quantifier_at_pattern_start_is_error_or_quirky() {
    // What happens with `*abc`? The compiler picks `*` up after the first atom,
    // but `*` as the FIRST item has no preceding atom. compile_atom is called on
    // `*` as a literal char — but wait, `*` isn't in the unsupported list, so it
    // becomes Kind::Char('*'). Then if the NEXT char is also '*' it'd be a quant
    // on the literal '*'. Pin current behavior either way (no panic).
    let _ = Regex::new("*abc");
    let _ = Regex::new("?abc");
    let _ = Regex::new("+abc");
}

#[test]
fn back_to_back_quantifiers_dont_crash() {
    // `a**` — second `*` applies to nothing in particular. Pin current behavior.
    let _ = Regex::new("a**");
    let _ = Regex::new("a+?");
    let _ = Regex::new("a*+");
}

// ---------- character class edge cases ----------

#[test]
fn unterminated_character_class_errors() {
    assert!(Regex::new("[abc").is_err(), "unterminated [ must error");
}

#[test]
fn empty_character_class_compiles() {
    // `[]` — zero entries, matches nothing (always fails) for unnegated.
    // Pin current behavior: compile_class loop exits immediately at `]`,
    // ranges is empty, so any input fails to match.
    let re = Regex::new("[]").expect("empty class compiles");
    assert!(!re.is_match("a"));
}

#[test]
fn negated_empty_class_matches_anything() {
    // `[^]` — negated empty class. ranges empty, `in_class` is false, `negated`
    // is true → matches any char. (Behavior consistent with PCRE.)
    let re = r("[^]a");
    // The negated empty class consumes one char, then we need `a`.
    assert!(re.is_match("ba"), "any prefix then 'a' should match");
}

#[test]
fn class_with_dash_at_end_is_literal_dash() {
    // `[a-z-]` — final `-` should be a literal dash, not a malformed range.
    let re = r("[a-z-]+");
    assert!(re.is_match("abc-def"));
    assert!(re.is_match("-"));
}

#[test]
fn class_with_dash_at_start_is_literal() {
    // `[-abc]` — leading dash is literal.
    let re = r("[-abc]+");
    assert!(re.is_match("a-c"));
    assert!(re.is_match("---"));
}

#[test]
fn class_with_escaped_closing_bracket() {
    // `[\]]` — literal `]` inside class. Tested via the backslash escape path.
    let re = r("[\\]]+");
    assert!(re.is_match("a]b"));
}

#[test]
fn class_range_with_descending_order() {
    // `[z-a]` — out of order range. Current implementation creates ranges
    // (z, a), which `c >= z && c <= a` is always false → matches nothing.
    // Pin that behavior.
    let re = r("[z-a]");
    assert!(!re.is_match("a"), "descending range matches nothing");
    assert!(!r("[z-a]").is_match("z"));
}

#[test]
fn class_with_unicode_chars_works() {
    // `[éñ]+` — multi-byte chars in the class.
    let re = r("[éñü]+");
    assert!(re.is_match("hello éñü world"));
    assert!(!re.is_match("hello world"));
}

// ---------- shorthand class edge cases ----------

#[test]
fn shorthand_classes_inside_brackets() {
    // `[\d\s]+` should match digits + whitespace.
    let re = r("[\\d\\s]+");
    assert!(re.is_match("foo 42 bar"));
    assert!(!re.is_match("AAAAAA"));
}

#[test]
fn negated_shorthand_inside_brackets_errors_so_caller_falls_back() {
    // Pre-fix: `[\D]` silently compiled as a class containing only the literal
    // char `D`, so a user typing `[\D]+` expecting "non-digit runs" got only
    // matches against literal 'D' — a serious silent-misparse.
    //
    // Post-fix: compile_class explicitly rejects \D, \W, \S inside `[...]`
    // with a clear error naming the unsupported metachar. recall.rs's
    // LineMatcher::build catches the compile error and routes to substring
    // matching (the documented fallback contract), so the user's search
    // still works — just literally instead of by class.
    //
    // To actually support `[\D]` semantically we'd need to invert ranges
    // across the full Unicode codespace, which is a bigger surface than is
    // worth for a grep subset. The error-and-fallback path is the right
    // safety/scope trade-off; pin it.
    let err = Regex::new("[\\D]").unwrap_err();
    assert!(
        err.contains("\\D") && err.contains("[^"),
        "error must name the metachar AND point at the alternative; got: {err}"
    );
    for pat in ["[\\D]", "[\\W]", "[\\S]", "[abc\\D]", "[\\d\\D]"] {
        assert!(
            Regex::new(pat).is_err(),
            "{pat:?} must error so caller falls back to substring"
        );
    }
}

#[test]
fn fallback_path_via_caller_keeps_search_usable() {
    // Verify the END-TO-END contract: when the regex can't compile, the
    // user's grep STILL finds matches via substring fallback. We exercise
    // this through `knapsack expand --grep <pattern>` (which uses
    // recall.rs::LineMatcher) via the binary — that's the actual user
    // surface the fix protects.
    use std::process::{Command, Stdio};

    // Find the release binary (this test only runs after `cargo build`).
    let bin = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join(if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        })
        .join(if cfg!(windows) {
            "knapsack.exe"
        } else {
            "knapsack"
        });
    if !bin.exists() {
        eprintln!(
            "skipping fallback integration test: {} not built",
            bin.display()
        );
        return;
    }
    // Seed a small payload via `store put`, then grep for a pattern that
    // would COMPILE in a strict regex (`[\D]+`) but in our subset rejects —
    // recall.rs must fall back to substring matching the LITERAL string
    // `[\D]+`. We use a payload that contains that literal string to confirm
    // the substring fallback works.
    let dir = std::env::temp_dir().join(format!(
        "kn-regex-fb-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let src = dir.join("seed.txt");
    std::fs::write(&src, "line one\nliteral [\\D]+ marker line\nline three\n").unwrap();

    let put = Command::new(&bin)
        .args(["store", "put", src.to_str().unwrap()])
        .env("KNAPSACK_STORE", dir.join("store"))
        .output()
        .expect("spawn put");
    assert!(put.status.success());
    let handle = String::from_utf8_lossy(&put.stdout).trim().to_string();

    // Grep for the LITERAL string "[\\D]+". recall.rs first tries Regex::new
    // (which now errors thanks to our fix), then falls back to case-insensitive
    // substring matching against the literal pattern.
    let exp = Command::new(&bin)
        .args(["expand", &handle, "--grep", "[\\D]+"])
        .env("KNAPSACK_STORE", dir.join("store"))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("spawn expand");
    assert!(
        exp.status.success(),
        "expand should succeed via substring fallback; stderr:\n{}",
        String::from_utf8_lossy(&exp.stderr)
    );
    let out = String::from_utf8_lossy(&exp.stdout);
    assert!(
        out.contains("literal [\\D]+ marker line"),
        "substring fallback must find the literal `[\\D]+` line; got:\n{out}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------- unsupported metachars (must error cleanly so callers fall back) ----------

#[test]
fn alternation_errors() {
    assert!(Regex::new("foo|bar").is_err());
}

#[test]
fn groups_error() {
    assert!(Regex::new("(abc)").is_err());
    assert!(Regex::new("(a|b)").is_err());
    assert!(Regex::new(")").is_err());
}

#[test]
fn brace_quantifiers_error() {
    assert!(Regex::new("a{2,3}").is_err());
    assert!(Regex::new("a{5}").is_err());
    assert!(Regex::new("}").is_err());
}

// ---------- backslash edge cases ----------

#[test]
fn trailing_backslash_errors() {
    assert!(Regex::new("abc\\").is_err());
}

#[test]
fn backslash_escapes_metacharacters_to_literal() {
    assert!(r("\\.").is_match("3.14"));
    assert!(r("\\*").is_match("a*b"));
    assert!(r("\\+").is_match("a+b"));
    assert!(r("\\?").is_match("a?b"));
    assert!(r("\\[").is_match("a[b"));
    assert!(r("\\\\").is_match("a\\b"));
    // Backslash of a non-meta char: just the char.
    assert!(r("\\a").is_match("abc"));
}

// ---------- dot ----------

#[test]
fn dot_does_not_match_newline_in_input() {
    assert!(!r(".").is_match("\n"), "dot must NOT match newline");
    assert!(r(".").is_match(" "));
    assert!(r(".").is_match("a"));
    assert!(r(".").is_match("中"), "dot matches multi-byte UTF-8 chars");
}

#[test]
fn dot_star_does_not_cross_newline() {
    // `a.*b` on "a\nb" — the dot can't bridge the newline, so no match.
    assert!(!r("a.*b").is_match("a\nb"));
    assert!(r("a.*b").is_match("axxxxxxb"));
}

// ---------- Unicode in input ----------

#[test]
fn unicode_input_with_literal_pattern() {
    assert!(r("中文").is_match("hello 中文 world"));
    assert!(r("éñü").is_match("éñü"));
}

#[test]
fn unicode_word_boundary_via_negated_class() {
    // `\w+` matches ASCII word chars only (word_ranges is A-Z, a-z, 0-9, _).
    // Multi-byte chars are NOT word chars per this regex.
    assert!(!r("^\\w+$").is_match("中"), "\\w is ASCII-only");
    assert!(r("^\\w+$").is_match("abc_123"));
}

#[test]
fn surrogate_pair_emoji_in_input() {
    // Pattern `.+`should match emoji input — each char (codepoint) is one step.
    assert!(r(".+").is_match("🎒🌍😀"));
    // Literal emoji as part of the pattern.
    assert!(r("🎒").is_match("a 🎒 b"));
}

// ---------- case-insensitivity ----------

#[test]
fn ignore_case_with_class_range() {
    let re = ri("[a-z]+");
    assert!(re.is_match("ABCDEF"));
    assert!(re.is_match("AbCdEf"));
    assert!(!re.is_match("12345"));
}

#[test]
fn ignore_case_with_shorthand_unaffected() {
    // \d is digits, case-insensitivity is a no-op on digits.
    let re = ri("\\d+");
    assert!(re.is_match("foo 42 bar"));
}

#[test]
fn ignore_case_with_unicode() {
    // ä / Ä — full-Unicode lowercasing in `new_ignore_case`.
    let re = ri("CAFÉ");
    assert!(re.is_match("café"));
    assert!(re.is_match("CAFÉ"));
    assert!(re.is_match("Café"));
}

#[test]
fn ignore_case_preserves_negated_shorthand_class_semantics() {
    // The real bug behind the `[\D]` issue: `Regex::new_ignore_case` used to
    // lowercase the WHOLE pattern up-front, silently flipping `\D` (non-digit)
    // to `\d` (digit) — the OPPOSITE semantics. Now `new_ignore_case` uses
    // `smart_lowercase_pattern` which preserves backslash escapes verbatim.
    //
    // Outside `[...]`, our regex supports both \D and \d. After the fix,
    // `\D+` (ignore-case) must still mean "one-or-more non-digit chars",
    // NOT "one-or-more digit chars".
    let non_digit_re = Regex::new_ignore_case("\\D+").unwrap();
    assert!(
        non_digit_re.is_match("hello"),
        "\\D+ must match non-digit runs"
    );
    assert!(
        !non_digit_re.is_match("123"),
        "\\D+ must NOT match digit-only"
    );
    // The opposite class still works case-insensitively.
    let digit_re = Regex::new_ignore_case("\\d+").unwrap();
    assert!(digit_re.is_match("123"));
    assert!(!digit_re.is_match("hello"));
    // \W \w pair
    let non_word_re = Regex::new_ignore_case("\\W+").unwrap();
    assert!(non_word_re.is_match("   "));
    assert!(!non_word_re.is_match("abc_123"));
    let word_re = Regex::new_ignore_case("\\w+").unwrap();
    assert!(word_re.is_match("abc_123"));
    assert!(!word_re.is_match("   "));
    // \S \s pair
    let non_space_re = Regex::new_ignore_case("\\S+").unwrap();
    assert!(non_space_re.is_match("hello"));
    assert!(!non_space_re.is_match("   "));
    let space_re = Regex::new_ignore_case("\\s+").unwrap();
    assert!(space_re.is_match("a b"));
    assert!(!space_re.is_match("abc"));
}

#[test]
fn ignore_case_still_lowercases_literal_chars() {
    // Regression guard for the smart_lowercase_pattern fix: literal chars in
    // the pattern MUST still be lowercased (so `ABC` matches `abc`). Only the
    // char immediately following a `\` is preserved.
    let re = Regex::new_ignore_case("HELLO").unwrap();
    assert!(
        re.is_match("hello"),
        "literal CAPS still lowercased for case-insensitive match"
    );
    assert!(re.is_match("Hello"));
    assert!(re.is_match("HELLO"));
}

#[test]
fn ignore_case_with_mixed_literal_and_escape() {
    // `ABC\D+` ignore-case = "literal abc (any case) + non-digit runs".
    // Pattern after smart-lowercase: `abc\D+`. Compile succeeds; \D outside
    // class is supported. Test that BOTH the literal-lowercase AND the
    // escape-preservation co-exist correctly.
    let re = Regex::new_ignore_case("ABC\\D+").unwrap();
    assert!(re.is_match("ABCdef"));
    assert!(re.is_match("abcXYZ"));
    assert!(
        !re.is_match("ABC123"),
        "after ABC must be non-digit chars; 123 fails"
    );
}

// ---------- perf / ReDoS guards ----------

#[test]
fn redos_a_star_repeated_terminates_quickly() {
    // The classic ReDoS pattern is `(a+)+`, which we block (no `()`).
    // The non-grouped equivalent `a*a*a*a*a*X` on `aaaaaa` could still
    // backtrack pessimistically. Cap at 2s (very generous) — anything
    // exponential would blow past this on 20+ a's.
    let pat = "a*a*a*a*a*X";
    let input = "a".repeat(25); // no X — must fail
    within(Duration::from_secs(2), "ReDoS a*a*a*a*a*", || {
        r(pat).is_match(&input)
    });
}

#[test]
fn redos_dot_star_then_literal_terminates() {
    // `.*.*X` on long non-X input. Greedy `.*` builds an offsets array and
    // backs off byte-by-byte; the unanchored outer `is_match` retries at every
    // starting position. The failure scan is therefore O(N³) — polynomial,
    // *not* exponential. That's what this perf guard is here to enforce:
    // a refactor that drops the offsets memoization or introduces catastrophic
    // backtracking would push N=500 to seconds-per-iteration, blowing past
    // either cap below by orders of magnitude.
    //
    // The caps split by build profile. CI runs `cargo test --release`
    // (.github/workflows/ci.yml), where LTO + opt 3 keep the O(N³) constant
    // small enough that 500ms is a tight, meaningful bound. Local `cargo test`
    // defaults to debug, where the same work runs ~3× slower (≈1.4s on a
    // current Windows laptop); a 3s debug cap stays well below "exponential"
    // and keeps the test useful for local iteration without flaking on the
    // build mode developers actually run. The point of the assertion is the
    // shape of the algorithm, not the millisecond budget.
    let pat = ".*.*X";
    let input = "y".repeat(500);
    let cap = if cfg!(debug_assertions) {
        Duration::from_secs(3)
    } else {
        Duration::from_millis(500)
    };
    within(cap, "ReDoS .*.*X", || r(pat).is_match(&input));
}

#[test]
fn very_long_input_against_simple_pattern_is_linear() {
    let pat = "needle";
    let input = format!("{}needle{}", "a".repeat(100_000), "b".repeat(100_000));
    let re = r(pat);
    within(Duration::from_millis(500), "long input", || {
        re.is_match(&input)
    });
}

#[test]
fn very_long_pattern_compiles_and_matches() {
    // 200-char pattern of literals. Each compiles to a Kind::Char item.
    let pat: String = ('a'..='z').cycle().take(200).collect();
    let re = r(&pat);
    let input = format!("xxx{pat}yyy");
    assert!(re.is_match(&input));
}

// ---------- match_one boundary behaviors ----------

#[test]
fn dollar_mid_pattern_is_literal_dollar() {
    // Pinned in existing tests but worth re-asserting in adversarial set.
    assert!(r("\\$5").is_match("price: $5"));
    assert!(r("3\\$").is_match("3$"));
    // `$` at end is anchor:
    assert!(r("done$").is_match("we are done"));
    assert!(!r("done$").is_match("done now"));
}

#[test]
fn caret_mid_pattern_is_literal_caret() {
    // `^` is only anchor at position 0 of the pattern. Mid-pattern it should
    // be treated as a literal (no special compile path).
    let re = r("a^b");
    assert!(re.is_match("a^b"));
    assert!(!re.is_match("ab"));
}

// ---------- real-world fixture-class patterns ----------

#[test]
fn ansi_escape_sequence_pattern() {
    // ANSI color escape: ESC[<digits>m
    let re = r("\\[\\d+m");
    assert!(re.is_match("\x1b[31mred\x1b[0m"));
}

#[test]
fn url_like_pattern() {
    let re = r("https?://[^ ]+");
    assert!(re.is_match("see https://example.com/page for details"));
    assert!(re.is_match("see http://x for details"));
    assert!(!re.is_match("just text"));
}

#[test]
fn ipv4_like_pattern() {
    // Not a strict IP validator, just a real-world-ish shape.
    let re = r("\\d+\\.\\d+\\.\\d+\\.\\d+");
    assert!(re.is_match("connecting to 192.168.1.42 port 80"));
}
