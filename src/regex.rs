//! A tiny, zero-dep regex matcher. Just enough to make `knapsack_expand(grep:…)` an
//! HONEST "pattern" instead of a substring match dressed up as one.
//!
//! Supported subset (everything in one paragraph so clippy doesn't think this is a
//! markdown list with broken indentation): literals; `.` for any char except `\n`;
//! greedy quantifiers `*`, `+`, `?`; line-anchors `^` and `$` (we match per-line, so
//! these anchor to start/end of each input line); character classes `[abc]`, `[a-z]`,
//! `[^abc]`; shorthand classes `\d` (digit), `\w` (word char), `\s` (whitespace) and
//! their negations `\D`, `\W`, `\S`; backslash-escape any metachar to make it literal
//! (`\.`, `\*`, …).
//!
//! Not supported (yet): alternation `|`, groups `()`, lazy quantifiers, `{n,m}`,
//! lookaround. If a pattern can't be compiled, callers fall back to substring matching
//! so legacy invocations don't suddenly start failing.
//!
//! Matching strategy: recursive backtracker over a flat list of `Item`s. Greedy
//! quantifiers consume eagerly, then back off byte-by-byte (well, item-by-item) until
//! the rest of the pattern matches. The recursion depth is bounded by the pattern
//! length, not the input — so even on log-line-sized text this is fine.
//!
//! Case sensitivity: handled at compile time. `Regex::new` is case-sensitive;
//! `Regex::new_ignore_case` lowercases literal chars and class ranges, and the matcher
//! lowercases the input as it scans.

#[derive(Debug, Clone)]
pub struct Regex {
    items: Vec<Item>,
    anchored_start: bool,
    ignore_case: bool,
}

#[derive(Debug, Clone)]
struct Item {
    kind: Kind,
    quant: Quant,
}

#[derive(Debug, Clone)]
enum Kind {
    Char(char),
    Any,
    Class { ranges: Vec<(char, char)>, negated: bool },
    EndAnchor, // $ — only valid as the last item
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Quant {
    One,
    ZeroOrMore,
    OneOrMore,
    ZeroOrOne,
}

impl Regex {
    pub fn new(pattern: &str) -> Result<Self, String> {
        Self::compile(pattern, false)
    }

    pub fn new_ignore_case(pattern: &str) -> Result<Self, String> {
        // Full-Unicode lowercasing of the pattern STRING up-front (e.g. `CAFÉ` → `café`
        // including the diacritic), so the matcher only has to do ASCII work on literal
        // chars. Matching also lowercases the input — see `is_match`.
        let lowered = pattern.to_lowercase();
        let mut me = Self::compile(&lowered, false)?;
        me.ignore_case = true;
        Ok(me)
    }

    fn compile(pattern: &str, ignore_case: bool) -> Result<Self, String> {
        let chars: Vec<char> = pattern.chars().collect();
        let mut items: Vec<Item> = Vec::new();
        let mut i = 0;
        let anchored_start = chars.first() == Some(&'^');
        if anchored_start {
            i += 1;
        }
        while i < chars.len() {
            let c = chars[i];
            // `$` as the LAST char is the end anchor; anywhere else it's a literal `$`.
            if c == '$' && i == chars.len() - 1 {
                items.push(Item { kind: Kind::EndAnchor, quant: Quant::One });
                i += 1;
                continue;
            }
            // Reject metacharacters that we explicitly don't support so the caller can
            // fall back to substring instead of getting silently-wrong matches.
            if c == '(' || c == ')' || c == '|' || c == '{' || c == '}' {
                return Err(format!("unsupported metacharacter `{}` at offset {}", c, i));
            }
            let (kind, consumed) = compile_atom(&chars[i..], ignore_case)?;
            i += consumed;
            let quant = if i < chars.len() {
                match chars[i] {
                    '*' => {
                        i += 1;
                        Quant::ZeroOrMore
                    }
                    '+' => {
                        i += 1;
                        Quant::OneOrMore
                    }
                    '?' => {
                        i += 1;
                        Quant::ZeroOrOne
                    }
                    _ => Quant::One,
                }
            } else {
                Quant::One
            };
            items.push(Item { kind, quant });
        }
        Ok(Regex { items, anchored_start, ignore_case })
    }

    /// True iff the pattern matches somewhere in `text`. (`^`/`$` are line-anchors per
    /// the doc above; callers pass one line at a time.)
    pub fn is_match(&self, text: &str) -> bool {
        let owned;
        let scan: &str = if self.ignore_case {
            owned = text.to_lowercase();
            &owned
        } else {
            text
        };
        if self.anchored_start {
            return match_here(&self.items, scan);
        }
        let bytes = scan.as_bytes();
        let mut start = 0;
        loop {
            if match_here(&self.items, &scan[start..]) {
                return true;
            }
            if start >= bytes.len() {
                return false;
            }
            // Advance by one UTF-8 char.
            let c = scan[start..].chars().next().unwrap();
            start += c.len_utf8();
        }
    }
}

fn compile_atom(chars: &[char], ignore_case: bool) -> Result<(Kind, usize), String> {
    let c = chars[0];
    if c == '\\' {
        if chars.len() < 2 {
            return Err("trailing backslash".into());
        }
        let next = chars[1];
        let kind = match next {
            'd' => Kind::Class { ranges: vec![('0', '9')], negated: false },
            'D' => Kind::Class { ranges: vec![('0', '9')], negated: true },
            'w' => Kind::Class { ranges: word_ranges(), negated: false },
            'W' => Kind::Class { ranges: word_ranges(), negated: true },
            's' => Kind::Class { ranges: space_ranges(), negated: false },
            'S' => Kind::Class { ranges: space_ranges(), negated: true },
            esc => Kind::Char(maybe_lower(esc, ignore_case)),
        };
        return Ok((kind, 2));
    }
    if c == '.' {
        return Ok((Kind::Any, 1));
    }
    if c == '[' {
        return compile_class(chars, ignore_case);
    }
    Ok((Kind::Char(maybe_lower(c, ignore_case)), 1))
}

fn compile_class(chars: &[char], ignore_case: bool) -> Result<(Kind, usize), String> {
    let mut i = 1usize; // skip [
    let mut negated = false;
    if i < chars.len() && chars[i] == '^' {
        negated = true;
        i += 1;
    }
    let mut ranges: Vec<(char, char)> = Vec::new();
    while i < chars.len() && chars[i] != ']' {
        // Backslash inside class: support \d \w \s and literal escapes.
        if chars[i] == '\\' {
            if i + 1 >= chars.len() {
                return Err("trailing backslash in class".into());
            }
            let n = chars[i + 1];
            match n {
                'd' => ranges.push(('0', '9')),
                'w' => ranges.extend(word_ranges()),
                's' => ranges.extend(space_ranges()),
                esc => ranges.push((maybe_lower(esc, ignore_case), maybe_lower(esc, ignore_case))),
            }
            i += 2;
            continue;
        }
        let lo = maybe_lower(chars[i], ignore_case);
        // Range: a-z. Otherwise single char.
        if i + 2 < chars.len() && chars[i + 1] == '-' && chars[i + 2] != ']' {
            let hi = maybe_lower(chars[i + 2], ignore_case);
            ranges.push((lo, hi));
            i += 3;
        } else {
            ranges.push((lo, lo));
            i += 1;
        }
    }
    if i >= chars.len() {
        return Err("unterminated character class".into());
    }
    Ok((Kind::Class { ranges, negated }, i + 1)) // consume the closing ]
}

fn word_ranges() -> Vec<(char, char)> {
    vec![('A', 'Z'), ('a', 'z'), ('0', '9'), ('_', '_')]
}
fn space_ranges() -> Vec<(char, char)> {
    vec![(' ', ' '), ('\t', '\t'), ('\n', '\n'), ('\r', '\r'), ('\x0b', '\x0b'), ('\x0c', '\x0c')]
}
fn maybe_lower(c: char, lower: bool) -> char {
    if lower {
        c.to_ascii_lowercase()
    } else {
        c
    }
}

fn match_here(items: &[Item], text: &str) -> bool {
    if items.is_empty() {
        return true;
    }
    let item = &items[0];
    let rest = &items[1..];

    // End anchor: succeeds iff we're at EOL.
    if matches!(item.kind, Kind::EndAnchor) {
        return text.is_empty();
    }

    match item.quant {
        Quant::One => match match_one(&item.kind, text) {
            Some(n) => match_here(rest, &text[n..]),
            None => false,
        },
        Quant::ZeroOrOne => {
            if let Some(n) = match_one(&item.kind, text) {
                if match_here(rest, &text[n..]) {
                    return true;
                }
            }
            match_here(rest, text)
        }
        Quant::ZeroOrMore | Quant::OneOrMore => {
            // Greedy: try the longest match first, then back off one char at a time.
            let mut offsets: Vec<usize> = vec![0];
            let mut t = text;
            while let Some(n) = match_one(&item.kind, t) {
                let last = *offsets.last().unwrap();
                offsets.push(last + n);
                t = &t[n..];
            }
            let min_count: usize = if matches!(item.quant, Quant::OneOrMore) { 1 } else { 0 };
            // offsets has (consumed_chars + 1) entries — index by count of matches consumed.
            let max = offsets.len() - 1;
            if max < min_count {
                return false;
            }
            for i in (min_count..=max).rev() {
                if match_here(rest, &text[offsets[i]..]) {
                    return true;
                }
            }
            false
        }
    }
}

/// Try to consume one occurrence of `kind` at the start of `text`. Returns bytes consumed.
fn match_one(kind: &Kind, text: &str) -> Option<usize> {
    let c = text.chars().next()?;
    // `.` does not match newline (PCRE default).
    if matches!(kind, Kind::Any) && c == '\n' {
        return None;
    }
    let ok = match kind {
        Kind::Any => true,
        Kind::Char(k) => c == *k,
        Kind::Class { ranges, negated } => {
            let in_class = ranges.iter().any(|(lo, hi)| c >= *lo && c <= *hi);
            in_class != *negated
        }
        Kind::EndAnchor => return None, // handled by caller
    };
    if ok {
        Some(c.len_utf8())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(p: &str) -> Regex {
        Regex::new(p).expect("compile")
    }
    fn ri(p: &str) -> Regex {
        Regex::new_ignore_case(p).expect("compile")
    }

    #[test]
    fn literal_substring() {
        assert!(r("error").is_match("compilation error here"));
        assert!(!r("error").is_match("all good"));
    }

    #[test]
    fn dot_matches_any_except_newline() {
        assert!(r("a.b").is_match("axb"));
        assert!(r("a.b").is_match("a b"));
        assert!(!r("a.b").is_match("ab"));
        // `.` must not match `\n` — protects line-anchored semantics.
        assert!(!r("a.b").is_match("a\nb"));
    }

    #[test]
    fn anchors() {
        assert!(r("^error").is_match("error TS1234"));
        assert!(!r("^error").is_match(" leading error"));
        assert!(r("ok$").is_match("all ok"));
        assert!(!r("ok$").is_match("not okay"));
        assert!(r("^exact$").is_match("exact"));
    }

    #[test]
    fn quantifiers_greedy() {
        assert!(r("a+b").is_match("aaab"));
        assert!(!r("a+b").is_match("b"));
        assert!(r("a*b").is_match("b"));
        assert!(r("colou?r").is_match("color"));
        assert!(r("colou?r").is_match("colour"));
        assert!(!r("colou?r").is_match("colouur"));
    }

    #[test]
    fn character_classes() {
        assert!(r("[abc]").is_match("xbz"));
        assert!(!r("[abc]").is_match("xyz"));
        assert!(r("[a-z]").is_match("Aa"));
        assert!(!r("[^a-z]+").is_match("abc"), "negated class with + needs at least one non-az");
        assert!(r("[^a-z]+").is_match("ABC"));
    }

    #[test]
    fn shorthand_classes() {
        assert!(r("\\d+").is_match("error at line 42"));
        assert!(!r("\\d+").is_match("no numbers here"));
        assert!(r("\\w+").is_match(" foo_bar123 "));
        assert!(r("\\s+").is_match("a\tb"));
        // Negated shorthand
        assert!(r("\\D").is_match("abc"));
        assert!(!r("\\D").is_match("12345"));
    }

    #[test]
    fn escape_metacharacters() {
        assert!(r("3\\.14").is_match("π ≈ 3.14"));
        assert!(!r("3\\.14").is_match("3X14"));
        assert!(r("\\*").is_match("a*b"));
    }

    #[test]
    fn ignore_case() {
        assert!(ri("ERROR").is_match("compilation error here"));
        assert!(ri("[a-z]+").is_match("ABCDE"));
    }

    #[test]
    fn unsupported_metachars_return_error() {
        // Caller (recall.rs) uses this to fall back to substring matching.
        assert!(Regex::new("a|b").is_err());
        assert!(Regex::new("(abc)").is_err());
        assert!(Regex::new("a{2,3}").is_err());
    }

    #[test]
    fn dollar_is_literal_unless_last() {
        // `$` is the end anchor only at the end of the pattern. Mid-pattern, it's a
        // literal `$` — which matches what most users expect when they search logs
        // containing things like "$VAR" or "$3.50".
        assert!(r("3\\$").is_match("price is 3$"));
        assert!(r("\\$VAR").is_match("$VAR is set"));
        // And the genuine end anchor still works.
        assert!(r("done$").is_match("all done"));
        assert!(!r("done$").is_match("done now"));
    }

    // A handful of real-log fixtures — the patterns power-users actually write.
    #[test]
    fn real_world_patterns() {
        // Python traceback frame
        assert!(r("File \".*\", line \\d+").is_match("  File \"app/main.py\", line 42, in handler"));
        // Rust error code
        assert!(r("^error\\[E\\d+\\]").is_match("error[E0277]: trait bound not satisfied"));
        // Node stack frame
        assert!(r("at .*:\\d+:\\d+").is_match("    at fetch (src/http.js:14:9)"));
        // pytest failure line
        assert!(ri("^FAILED ").is_match("FAILED tests/test_x.py::test_thing"));
    }
}
