//! Expand a handle. Full recall returns EXACT bytes. Line-range / grep slicing operates
//! only on a valid-UTF-8 decode of the stored bytes (per the byte-preservation rule:
//! the store keeps raw bytes; sliced views are a convenience over decoded text). If the
//! content isn't UTF-8, slicing falls back to returning the full exact bytes.

use crate::hash::Handle;
use crate::store::Store;

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

    if let Some(g) = grep {
        let gl = g.to_lowercase();
        if context == 0 {
            ls.retain(|l| l.to_lowercase().contains(&gl));
        } else {
            let n = ls.len();
            let mut keep = vec![false; n];
            for (i, l) in ls.iter().enumerate() {
                if l.to_lowercase().contains(&gl) {
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
    if let Some((a, b)) = lines {
        let a = a.max(1);
        let lo = a - 1;
        let hi = b.min(ls.len());
        ls = if lo < hi { ls[lo..hi].to_vec() } else { Vec::new() };
    }
    Some(RecallOut::Text(ls.join("\n")))
}

/// 1-based inclusive "A-B" -> (A, B).
pub fn parse_range(s: &str) -> Option<(usize, usize)> {
    let (a, b) = s.split_once('-')?;
    Some((a.trim().parse().ok()?, b.trim().parse().ok()?))
}
