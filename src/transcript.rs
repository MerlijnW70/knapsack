//! Transcript-driven residency: turn Claude Code's JSONL transcript into a "what's
//! actually still in the context window right now" set. The ledger says "I emitted
//! this handle N steps ago"; the transcript says "this handle text is visible in the
//! messages Claude can currently see". A backref is only safe when BOTH say yes.
//!
//! Conservatism by construction:
//!   - If the transcript can't be read or parsed at all, we return `ok: false`. The
//!     caller (api::pack_output) then falls back to ledger-only residency — same
//!     behaviour as before this module existed. No regression vs the old "approximate
//!     by budget" path; just no benefit.
//!   - Boundary detection is deliberately PERMISSIVE: anything that looks like
//!     /clear, compaction, or a session restart resets the resident set. False
//!     positives downgrade a backref to a fresh re-send (correctness intact, just
//!     less compression). False negatives leak a dangling backref — that's the bug
//!     this module exists to prevent.
//!
//! Schema-tolerance: Claude Code's transcript format may shift across versions. We
//! never assume a single field shape — we look for any field across a small set of
//! likely names, and we scan the raw line text for `/clear` markers in case the
//! event is buried in a content blob we don't fully recognise.

use crate::json::{self, Json};
use std::collections::HashSet;
use std::fs;
use std::path::Path;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Boundary {
    /// User typed `/clear` (Claude Code wipes the context window).
    Clear,
    /// Automatic context compaction event.
    Compaction,
    /// Session start or restart (treated the same — both reset what Claude sees).
    Restart,
}

pub struct ScanResult {
    /// Handles known to appear in the transcript AFTER the most recent boundary
    /// (or the whole transcript if no boundary was found). Always lower-bounded by
    /// what's actually parseable in the transcript — not the ledger.
    pub resident: HashSet<String>,
    /// What boundary we last detected, and at which line index. Useful for `doctor`
    /// / debug output; not load-bearing for the residency check itself.
    pub last_boundary: Option<(Boundary, usize)>,
    /// True if the transcript was readable and not totally empty. When false, the
    /// caller falls back to ledger-only residency (the safe-fallback contract).
    pub ok: bool,
    /// How many JSONL lines we inspected — for diagnostics.
    pub lines_scanned: usize,
}

impl ScanResult {
    fn empty(ok: bool) -> Self {
        Self {
            resident: HashSet::new(),
            last_boundary: None,
            ok,
            lines_scanned: 0,
        }
    }
}

/// Inspect a Claude Code transcript and decide which Knapsack handles are still
/// resident in the context window. See module docs for the safety contract.
pub fn scan(path: &Path) -> ScanResult {
    let text = match fs::read_to_string(path) {
        Ok(t) => t,
        Err(_) => return ScanResult::empty(false),
    };
    if text.trim().is_empty() {
        return ScanResult::empty(false);
    }

    let lines: Vec<&str> = text.lines().collect();
    let mut last_boundary: Option<(Boundary, usize)> = None;

    // PASS 1: find the latest boundary. We walk forward and record the LAST one we
    // see — boundary semantics in the user's brief are "the most recent reset
    // wins" (a /clear after compaction still gives an empty context window).
    for (i, line) in lines.iter().enumerate() {
        if let Some(b) = boundary_in_line(line) {
            last_boundary = Some((b, i));
        }
    }

    // PASS 2: from one past the boundary (or the start, if none was found) to the
    // end, gather every handle that appears in any line's raw text. We scan the
    // RAW LINE rather than parsing specific content fields — Claude's content
    // representation is multi-shape (string, array of blocks, tool_use objects),
    // and handles are ASCII so they survive any reasonable JSON-string encoding.
    let start = last_boundary.map(|(_, i)| i + 1).unwrap_or(0);
    let mut resident: HashSet<String> = HashSet::new();
    for line in &lines[start..] {
        collect_handles(line, &mut resident);
    }

    ScanResult {
        resident,
        last_boundary,
        ok: true,
        lines_scanned: lines.len(),
    }
}

/// Inspect one JSONL line and return a boundary type if it looks like one. Permissive:
/// we look at the parsed JSON, but also fall back to raw-text matching for cases where
/// the JSON is shaped differently than we expect.
fn boundary_in_line(line: &str) -> Option<Boundary> {
    // RAW-TEXT escape hatch FIRST — a /clear command typed by the user is the most
    // important boundary, and Claude Code may serialise it as different shapes
    // across versions ({"content":"/clear"} vs nested message-block arrays). The
    // raw line always contains the literal token.
    if line.contains("\"/clear\"") {
        return Some(Boundary::Clear);
    }

    let v = json::parse(line).ok()?;
    // Common shapes to probe: type/event/kind/subtype.
    for key in ["type", "event", "kind", "subtype"] {
        if let Some(s) = v.get(key).and_then(|x| x.as_str()) {
            if let Some(b) = boundary_from_label(s) {
                return Some(b);
            }
        }
    }
    // Nested: message.role + message.content == "/clear"
    if let Some(content) = v
        .get("message")
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_str())
    {
        if content.trim() == "/clear" {
            return Some(Boundary::Clear);
        }
    }
    if let Some(content) = v.get("content").and_then(|c| c.as_str()) {
        if content.trim() == "/clear" {
            return Some(Boundary::Clear);
        }
    }
    None
}

fn boundary_from_label(s: &str) -> Option<Boundary> {
    let lc = s.to_ascii_lowercase();
    match lc.as_str() {
        "clear" => Some(Boundary::Clear),
        "compact" | "compaction" | "compact_complete" | "compacted" => Some(Boundary::Compaction),
        "session_start" | "session_restart" | "session_reset" | "restart" => {
            Some(Boundary::Restart)
        }
        _ => None,
    }
}

/// Find every `ks_<10|16 hex>` and `ks2_<32 hex>` occurrence in `text` and add it to
/// `out`. ASCII-only by design; works on raw JSON lines without decoding.
fn collect_handles(text: &str, out: &mut HashSet<String>) {
    let bytes = text.as_bytes();
    let n = bytes.len();
    let mut i = 0;
    while i + 4 <= n {
        // Sniff for `ks` at i; cheap reject otherwise.
        if bytes[i] != b'k' || bytes[i + 1] != b's' {
            i += 1;
            continue;
        }
        // ks2_<32 hex>
        if bytes[i + 2] == b'2' && i + 3 < n && bytes[i + 3] == b'_' {
            let hex_start = i + 4;
            let mut j = hex_start;
            while j < n && (bytes[j] as char).is_ascii_hexdigit() {
                j += 1;
            }
            if j - hex_start == 32 {
                out.insert(text[i..j].to_string());
            }
            i = j.max(i + 1);
            continue;
        }
        // ks_<10|16 hex>
        if bytes[i + 2] == b'_' {
            let hex_start = i + 3;
            let mut j = hex_start;
            while j < n && (bytes[j] as char).is_ascii_hexdigit() {
                j += 1;
            }
            let len = j - hex_start;
            if len == 10 || len == 16 {
                out.insert(text[i..j].to_string());
            }
            i = j.max(i + 1);
            continue;
        }
        i += 1;
    }
}

/// Human-readable boundary label for status / debug output.
pub fn boundary_label(b: Boundary) -> &'static str {
    match b {
        Boundary::Clear => "/clear",
        Boundary::Compaction => "compaction",
        Boundary::Restart => "session restart",
    }
}

// Unused-import guard: `Json` is referenced in tests but not in the prod path because
// we only call json::parse. Keeping the explicit re-export for editor convenience.
#[allow(dead_code)]
fn _force_json_use(j: Json) -> Json {
    j
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handle_collector_finds_both_formats() {
        let mut out = HashSet::new();
        let txt = r#"{"content":"see [Knapsack: 5 lines · recall ks2_0123456789abcdef0123456789abcdef] and ks_7e1680c08c plus ks_0123456789abcdef"}"#;
        collect_handles(txt, &mut out);
        assert!(out.contains("ks2_0123456789abcdef0123456789abcdef"));
        assert!(out.contains("ks_7e1680c08c"));
        assert!(out.contains("ks_0123456789abcdef"));
    }

    #[test]
    fn handle_collector_rejects_wrong_lengths() {
        let mut out = HashSet::new();
        // 31 hex (off by one), and 11 hex (off by one for legacy). Neither must collect.
        collect_handles(
            "ks2_0123456789abcdef0123456789abcde ks_01234567890",
            &mut out,
        );
        assert!(
            out.is_empty(),
            "off-by-one lengths must not be collected: {:?}",
            out
        );
    }

    #[test]
    fn boundary_detector_finds_slash_clear() {
        assert_eq!(
            boundary_in_line(r#"{"role":"user","content":"/clear"}"#),
            Some(Boundary::Clear)
        );
    }

    #[test]
    fn boundary_detector_finds_compaction_event() {
        assert_eq!(
            boundary_in_line(r#"{"type":"compact","when":1234}"#),
            Some(Boundary::Compaction)
        );
        assert_eq!(
            boundary_in_line(r#"{"type":"COMPACTION"}"#),
            Some(Boundary::Compaction),
            "case-insensitive label match"
        );
    }

    #[test]
    fn boundary_detector_finds_session_restart() {
        assert_eq!(
            boundary_in_line(r#"{"type":"session_start"}"#),
            Some(Boundary::Restart)
        );
    }

    #[test]
    fn boundary_detector_rejects_unrelated_lines() {
        assert!(boundary_in_line(r#"{"type":"assistant","content":"hello"}"#).is_none());
        assert!(boundary_in_line("not json at all").is_none());
        assert!(boundary_in_line("").is_none());
    }
}
