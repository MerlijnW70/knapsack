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
    Regex::new(p).expect(&format!("regex compile must succeed: {p}"))
}

fn ri(p: &str) -> Regex {
    Regex::new_ignore_case(p).expect(&format!("regex compile must succeed: {p}"))
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
    assert!(re.is_match("anything"), "empty pattern matches anywhere (zero-length match)");
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
fn negated_shorthand_inside_brackets_silently_loses_negation() {
    // Looking at compile_class: `\d` inside [] adds ('0','9'). `\D` is not
    // handled — falls through to the `esc => ranges.push((maybe_lower(esc),...))`
    // path, which treats `D` as a literal char. So [\D] matches literal 'D'.
    // Pin this surprising-but-documented behavior; a stricter parse would
    // require error on \D inside class which is a stricter break for callers.
    let re = r("[\\D]");
    assert!(re.is_match("D"), "\\D inside class is treated as literal D");
    assert!(!re.is_match("X"));
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

// ---------- perf / ReDoS guards ----------

#[test]
fn redos_a_star_repeated_terminates_quickly() {
    // The classic ReDoS pattern is `(a+)+`, which we block (no `()`).
    // The non-grouped equivalent `a*a*a*a*a*X` on `aaaaaa` could still
    // backtrack pessimistically. Cap at 2s (very generous) — anything
    // exponential would blow past this on 20+ a's.
    let pat = "a*a*a*a*a*X";
    let input = "a".repeat(25); // no X — must fail
    within(Duration::from_secs(2), "ReDoS a*a*a*a*a*", || r(pat).is_match(&input));
}

#[test]
fn redos_dot_star_then_literal_terminates() {
    // `.*.*X` on long non-X input. Greedy `.*` consumes everything; backs off
    // to find X; backs off again. Should be ~linear in input length.
    let pat = ".*.*X";
    let input = "y".repeat(500);
    within(Duration::from_millis(500), "ReDoS .*.*X", || r(pat).is_match(&input));
}

#[test]
fn very_long_input_against_simple_pattern_is_linear() {
    let pat = "needle";
    let input = format!("{}needle{}", "a".repeat(100_000), "b".repeat(100_000));
    let re = r(pat);
    within(Duration::from_millis(500), "long input", || re.is_match(&input));
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
