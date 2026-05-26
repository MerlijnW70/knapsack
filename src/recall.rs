//! Expand a handle. Full recall returns EXACT bytes. Line-range / grep slicing operates
//! only on a valid-UTF-8 decode of the stored bytes (per the byte-preservation rule:
//! the store keeps raw bytes; sliced views are a convenience over decoded text). If the
//! content isn't UTF-8, slicing falls back to returning the full exact bytes.
//!
//! `grep` is a regex pattern (subset: `. * + ? ^ $ [class] \d \w \s` and negations).
//! It's compiled case-insensitively. If the pattern uses unsupported metacharacters
//! (`|`, `()`, `{n,m}`), we fall back to a case-insensitive substring match so legacy
//! callers (who passed plain words) keep working.

use crate::hash::Handle;
use crate::regex::Regex;
use crate::store::Store;

/// One concrete predicate for line filtering. The two variants represent the two paths
/// `grep` can take: a compiled regex, or — when compilation fails — case-insensitive
/// substring (the historical behaviour).
enum LineMatcher {
    Regex(Regex),
    Substring(String),
}

impl LineMatcher {
    fn build(pattern: &str) -> Self {
        match Regex::new_ignore_case(pattern) {
            Ok(re) => LineMatcher::Regex(re),
            Err(_) => LineMatcher::Substring(pattern.to_lowercase()),
        }
    }
    fn matches(&self, line: &str) -> bool {
        match self {
            LineMatcher::Regex(re) => re.is_match(line),
            LineMatcher::Substring(needle) => line.to_lowercase().contains(needle),
        }
    }
}

pub enum RecallOut {
    Bytes(Vec<u8>),
    Text(String),
}

/// `context` adds N lines of context around each grep match (0 = matching lines only).
pub fn expand(
    store: &Store,
    h: &Handle,
    lines: Option<(usize, usize)>,
    grep: Option<&str>,
    context: usize,
) -> Option<RecallOut> {
    let raw = store.get(h)?;
    if lines.is_none() && grep.is_none() {
        return Some(RecallOut::Bytes(raw));
    }
    let text = match std::str::from_utf8(&raw) {
        Ok(t) => t.to_string(),
        Err(_) => return Some(RecallOut::Bytes(raw)),
    };
    let mut ls: Vec<String> = text.split('\n').map(|s| s.to_string()).collect();

    // ORDER MATTERS: slice by --lines FIRST, then grep within that window. The
    // user's mental model is "1-based original-line numbers". If we grep first,
    // `--lines 50-100 --grep TARGET` would index the post-grep vector (3 matches
    // total) and return empty for indices 50..100 — confusing and wrong. Slicing
    // first keeps --lines anchored to the original file numbering and lets grep
    // refine within that window.
    if let Some((a, b)) = lines {
        let a = a.max(1);
        let lo = a - 1;
        let hi = b.min(ls.len());
        ls = if lo < hi { ls[lo..hi].to_vec() } else { Vec::new() };
    }

    if let Some(g) = grep {
        let matcher = LineMatcher::build(g);
        if context == 0 {
            ls.retain(|l| matcher.matches(l));
        } else {
            let n = ls.len();
            // n == 0 (empty post-slice) needs a guard before the (n-1) below.
            if n > 0 {
                let mut keep = vec![false; n];
                for (i, l) in ls.iter().enumerate() {
                    if matcher.matches(l) {
                        let lo = i.saturating_sub(context);
                        let hi = (i + context).min(n - 1);
                        for slot in &mut keep[lo..=hi] {
                            *slot = true;
                        }
                    }
                }
                ls = ls.into_iter().enumerate().filter(|(i, _)| keep[*i]).map(|(_, l)| l).collect();
            }
        }
    }
    Some(RecallOut::Text(ls.join("\n")))
}

/// 1-based inclusive "A-B" -> (A, B).
pub fn parse_range(s: &str) -> Option<(usize, usize)> {
    let (a, b) = s.split_once('-')?;
    Some((a.trim().parse().ok()?, b.trim().parse().ok()?))
}
