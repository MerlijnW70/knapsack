//! `knapsack pack <file>` — markdown-aware context-file packing.
//!
//! Reads a markdown/text document, preserves structural anchors (headings, code fences,
//! lists, blockquotes, short paragraphs), and elides long prose blocks behind recall
//! markers backed by a single whole-file handle in the byte-exact store. The original
//! bytes go in the store unchanged so `knapsack expand <handle> [--lines A-B]` returns
//! exact bytes — there is no per-section state to keep in sync.
//!
//! Safety contract (matches the CLI flags wired in main.rs):
//! - Never mutates the original file by default.
//! - Writes a side-car (default `<name>.knapsack.md`); caller controls the path.
//! - The CLI refuses to write when the packed view is not smaller unless `--force`.
//! - The stored original is byte-exact; recall is content-addressed, not view-derived.
//!
//! The heuristic for "what to elide" is deliberately conservative — see `should_elide`
//! and the docs above each predicate. Wrong rules here cause silent semantic damage to a
//! user's notes; better to keep a paragraph than to summarize it incorrectly.

use crate::hash::Handle;
use crate::store::Store;
use crate::token_estimate::tokens_bytes;

pub struct PackDocResult {
    /// The packed markdown view, written to the side-car (or to `--output`).
    pub view: String,
    /// Estimated tokens of the original input.
    pub raw_tokens: usize,
    /// Estimated tokens of the packed view.
    pub packed_tokens: usize,
    /// Handle of the byte-exact original in the recall store.
    pub handle: Handle,
    /// How many prose blocks were elided (informational; not part of the contract).
    pub elisions: usize,
}

/// Long-prose elision thresholds. Two shapes both qualify:
///   (a) a single physical line of `ELIDE_MIN_CHARS_SINGLE_LINE`+ chars — the common
///       case for CLAUDE.md / AGENTS.md / brief-style memory files where each paragraph
///       is one unwrapped line, and
///   (b) a multi-line paragraph of `ELIDE_MIN_LINES`+ physical lines AND `ELIDE_MIN_CHARS`+
///       chars — the case for hard-wrapped paragraphs (e.g., 80-col discipline).
/// Lists, headings, blockquotes, and code never qualify because they are matched as
/// structural lines BEFORE the paragraph accumulator runs. Tuning these thresholds is a
/// product call: too low and we erase short notes; too high and we miss the wins.
const ELIDE_MIN_CHARS_SINGLE_LINE: usize = 500;
const ELIDE_MIN_LINES: usize = 3;
const ELIDE_MIN_CHARS: usize = 300;

pub fn pack_doc(source_label: &str, bytes: &[u8], store: &Store) -> PackDocResult {
    let handle = store.put(bytes);
    let (view, elisions) = build_view(source_label, bytes, &handle);
    PackDocResult {
        raw_tokens: tokens_bytes(bytes),
        packed_tokens: tokens_bytes(view.as_bytes()),
        view,
        handle,
        elisions,
    }
}

fn build_view(source_label: &str, bytes: &[u8], handle: &Handle) -> (String, usize) {
    // We work over a UTF-8 decode for line semantics; the STORE keeps raw bytes, so
    // non-UTF-8 content still recalls byte-exact. The decode is lossy on purpose: a
    // bad byte becomes U+FFFD in the packed view only, never in the store.
    let text = String::from_utf8_lossy(bytes);

    // split('\n') vs lines(): we want the explicit line index for recall references,
    // and we need to handle the trailing-newline case without emitting a phantom blank
    // line at the end of the packed view.
    let mut lines: Vec<&str> = text.split('\n').collect();
    let had_trailing_newline = lines.last().map(|l| l.is_empty()).unwrap_or(false);
    if had_trailing_newline {
        lines.pop();
    }

    let mut out = String::new();
    // Two header comments, both HTML so they don't render in markdown. The first is a
    // machine-readable manifest (parsed by `knapsack inspect <packed-file>`); the second
    // is a one-line hint for a human who opens the file in an editor. We keep these
    // SHORT on purpose — header overhead is paid once per file and was the dominant cost
    // on small fixtures in the previous iteration.
    out.push_str(&format!(
        "<!-- ks-pack source={label} handle={h} -->\n",
        label = source_label,
        h = handle
    ));
    out.push_str(&format!(
        "<!-- knapsack inspect <this-file>  ·  knapsack expand {h}  (full original) -->\n\n",
        h = handle
    ));

    let mut elisions = 0usize;
    let mut i = 0;

    while i < lines.len() {
        let line = lines[i];
        let trimmed = line.trim_start();

        // Code fences: emit verbatim from open to close (inclusive). Keeping the closing
        // fence on a separate iteration would mis-classify the closing line as prose.
        if trimmed.starts_with("```") {
            out.push_str(line);
            out.push('\n');
            i += 1;
            while i < lines.len() {
                out.push_str(lines[i]);
                out.push('\n');
                let closes = lines[i].trim_start().starts_with("```");
                i += 1;
                if closes {
                    break;
                }
            }
            continue;
        }

        if is_structural_line(trimmed) || trimmed.is_empty() {
            out.push_str(line);
            out.push('\n');
            i += 1;
            continue;
        }

        // Start of a prose paragraph. Greedily accumulate until a structural line, a
        // blank line, or EOF. We count chars (not bytes) to keep the threshold sane for
        // multi-byte text.
        let para_start = i;
        let mut para_chars = 0usize;
        let mut para_lines = 0usize;
        while i < lines.len() {
            let l = lines[i];
            let t = l.trim_start();
            if t.is_empty() || is_structural_line(t) {
                break;
            }
            para_chars += l.chars().count() + 1;
            para_lines += 1;
            i += 1;
        }
        let para_end = i; // exclusive (0-indexed)

        if should_elide(para_lines, para_chars) {
            let line_from = para_start + 1; // 1-indexed inclusive
            let line_to = para_end;
            let tok = tokens_bytes(lines[para_start..para_end].join("\n").as_bytes());
            // The marker has two parts:
            //   - a short, human-readable banner (this is what the AI/reader sees in
            //     rendered markdown — no jargon, no hex)
            //   - a trailing HTML comment carrying the recall metadata (handle + line
            //     range), stripped by markdown renderers but trivially regex-parseable
            //     by `knapsack inspect <packed-file>` for power users.
            // The two are emitted on the SAME line so the metadata travels with the
            // banner even if the file is edited by hand. `inspect` is the documented
            // way to recover the recall details — never asking the reader to grep HTML.
            out.push_str(&format!(
                "[Knapsack: section omitted · ~{tok} tokens · exact recall available] <!-- ks-recall handle={h} lines={a}-{b} tokens={tok} -->\n",
                a = line_from,
                b = line_to,
                tok = tok,
                h = handle
            ));
            elisions += 1;
        } else {
            for l in &lines[para_start..para_end] {
                out.push_str(l);
                out.push('\n');
            }
        }
    }

    (out, elisions)
}

/// "This line is structure, not prose — keep it verbatim regardless of paragraph size."
/// Anything not matched here is a candidate for elision when grouped into a paragraph.
fn is_structural_line(trimmed: &str) -> bool {
    is_heading(trimmed)
        || is_list_item(trimmed)
        || trimmed.starts_with('>')
        || trimmed.starts_with("```")
}

fn is_heading(t: &str) -> bool {
    let bytes = t.as_bytes();
    if bytes.is_empty() || bytes[0] != b'#' {
        return false;
    }
    let mut n = 0;
    for &b in bytes {
        if b == b'#' {
            n += 1;
        } else {
            break;
        }
        if n > 6 {
            return false;
        }
    }
    // ATX headings require a space after the hashes (per CommonMark). Without this we'd
    // misclassify lines like "#!/bin/sh" or "#define X" as headings.
    n >= 1 && bytes.get(n) == Some(&b' ')
}

fn is_list_item(t: &str) -> bool {
    if t.starts_with("- ") || t.starts_with("* ") || t.starts_with("+ ") {
        return true;
    }
    // Ordered list: "1. ", "10. ", "23) " etc.  We accept both "." and ")" markers.
    let bytes = t.as_bytes();
    let mut i = 0;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    i > 0
        && matches!(bytes.get(i), Some(b'.') | Some(b')'))
        && matches!(bytes.get(i + 1), Some(b' '))
}

fn should_elide(lines: usize, chars: usize) -> bool {
    (lines == 1 && chars >= ELIDE_MIN_CHARS_SINGLE_LINE)
        || (lines >= ELIDE_MIN_LINES && chars >= ELIDE_MIN_CHARS)
}

/// Derive the default side-car path for an input file.
/// `foo/bar.md` → `foo/bar.knapsack.md`; `foo/notes` → `foo/notes.knapsack.md`.
pub fn sidecar_path(input: &std::path::Path) -> std::path::PathBuf {
    let parent = input.parent().map(|p| p.to_path_buf()).unwrap_or_default();
    let file_name = input
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let new_name = if let Some(dot) = file_name.rfind('.') {
        format!("{}.knapsack.{}", &file_name[..dot], &file_name[dot + 1..])
    } else {
        format!("{}.knapsack.md", file_name)
    };
    if parent.as_os_str().is_empty() {
        std::path::PathBuf::from(new_name)
    } else {
        parent.join(new_name)
    }
}

// ---------- inspect: parse a packed file back into its recall map ----------

/// One elided block discovered in a packed file. The handle + line range are exactly
/// what `knapsack expand <handle> --lines A-B` (or the MCP `knapsack_expand` tool with
/// `lines: "A-B"`) needs to recall the byte-exact original.
#[derive(Debug, PartialEq, Eq)]
pub struct RecallMarker {
    pub handle: String,
    pub line_from: usize,
    pub line_to: usize,
    pub tokens: usize,
}

/// What a packed file declares about itself, plus every recall marker inside it. This
/// is the structured form behind `knapsack inspect <packed-file>`: power-user view of
/// what the friendly markers actually point at.
#[derive(Debug, Default)]
pub struct PackedManifest {
    pub source: Option<String>,
    pub whole_file_handle: Option<String>,
    pub markers: Vec<RecallMarker>,
}

const PACK_HEADER_PREFIX: &str = "<!-- ks-pack ";
const RECALL_MARKER_NEEDLE: &str = "<!-- ks-recall ";

/// Parse a packed view back into its manifest. Tolerant by design: missing fields,
/// extra whitespace, and unknown keys are ignored rather than errored. The format is
/// stable enough (key=value, space-separated, terminated by ` -->`) that we can lean on
/// it from CLI tooling without dragging in a parser dependency.
pub fn parse_packed(content: &str) -> PackedManifest {
    let mut m = PackedManifest::default();
    for raw_line in content.lines() {
        // Header (one per file by convention; if duplicated, last one wins).
        if let Some(rest) = raw_line.trim_start().strip_prefix(PACK_HEADER_PREFIX) {
            if let Some(body) = rest.strip_suffix(" -->") {
                for (k, v) in parse_kv(body) {
                    match k {
                        "source" => m.source = Some(v.to_string()),
                        "handle" => m.whole_file_handle = Some(v.to_string()),
                        _ => {}
                    }
                }
            }
            continue;
        }
        // Per-elision recall marker. Anywhere on the line; everything after the needle
        // and before the closing ` -->` is the kv body.
        if let Some(idx) = raw_line.find(RECALL_MARKER_NEEDLE) {
            let after = &raw_line[idx + RECALL_MARKER_NEEDLE.len()..];
            if let Some(end) = after.find(" -->") {
                let body = &after[..end];
                let mut handle = None;
                let mut line_from = 0usize;
                let mut line_to = 0usize;
                let mut tokens = 0usize;
                for (k, v) in parse_kv(body) {
                    match k {
                        "handle" => handle = Some(v.to_string()),
                        "lines" => {
                            if let Some((a, b)) = v.split_once('-') {
                                line_from = a.parse().unwrap_or(0);
                                line_to = b.parse().unwrap_or(0);
                            }
                        }
                        "tokens" => tokens = v.parse().unwrap_or(0),
                        _ => {}
                    }
                }
                if let Some(h) = handle {
                    m.markers.push(RecallMarker {
                        handle: h,
                        line_from,
                        line_to,
                        tokens,
                    });
                }
            }
        }
    }
    m
}

/// Walk a `key=value key=value` body. Values are bare (no quotes) — the writer controls
/// the shape, and we never embed spaces in values. Keeps the parser zero-dep.
fn parse_kv(body: &str) -> impl Iterator<Item = (&str, &str)> {
    body.split_whitespace()
        .filter_map(|tok| tok.split_once('='))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn sidecar_inserts_knapsack_before_extension() {
        assert_eq!(
            sidecar_path(Path::new("CLAUDE.md")),
            Path::new("CLAUDE.knapsack.md")
        );
        assert_eq!(
            sidecar_path(Path::new("docs/spec.md")),
            Path::new("docs/spec.knapsack.md")
        );
        assert_eq!(
            sidecar_path(Path::new("notes")),
            Path::new("notes.knapsack.md")
        );
    }

    #[test]
    fn heading_detector_requires_space_after_hashes() {
        assert!(is_heading("# x"));
        assert!(is_heading("###### x"));
        assert!(
            !is_heading("#x"),
            "no space → not a heading (could be #!/...)"
        );
        assert!(
            !is_heading("####### x"),
            "7 hashes → not a CommonMark heading"
        );
        assert!(!is_heading(""));
    }

    #[test]
    fn list_detector_handles_markers_and_ordered() {
        assert!(is_list_item("- x"));
        assert!(is_list_item("* x"));
        assert!(is_list_item("+ x"));
        assert!(is_list_item("1. x"));
        assert!(is_list_item("23) x"));
        assert!(!is_list_item("1.x"), "missing space after . → not a list");
        assert!(!is_list_item("plain prose."));
    }

    #[test]
    fn parse_packed_round_trips_header_and_markers() {
        let content = "<!-- ks-pack source=CLAUDE.md handle=ks_abc123 -->\n\
            <!-- Knapsack packed view. Long prose blocks ... -->\n\
            \n\
            # Heading\n\
            \n\
            [Knapsack: unchanged section · ~178 tokens · exact text available on request] <!-- ks-recall handle=ks_abc123 lines=12-30 tokens=178 -->\n\
            \n\
            ## Next\n\
            \n\
            [Knapsack: unchanged section · ~52 tokens · exact text available on request] <!-- ks-recall handle=ks_abc123 lines=44-58 tokens=52 -->\n";
        let m = parse_packed(content);
        assert_eq!(m.source.as_deref(), Some("CLAUDE.md"));
        assert_eq!(m.whole_file_handle.as_deref(), Some("ks_abc123"));
        assert_eq!(
            m.markers,
            vec![
                RecallMarker {
                    handle: "ks_abc123".into(),
                    line_from: 12,
                    line_to: 30,
                    tokens: 178
                },
                RecallMarker {
                    handle: "ks_abc123".into(),
                    line_from: 44,
                    line_to: 58,
                    tokens: 52
                },
            ]
        );
    }

    #[test]
    fn parse_packed_is_tolerant_of_missing_and_unknown_fields() {
        // No header, no tokens key, unknown key — should not panic and should still
        // extract whatever it can. This is what protects `knapsack inspect <file>` from
        // crashing on a manually edited or older-format side-car.
        let content = "<!-- ks-recall handle=ks_xyz lines=1-5 future_key=42 -->\n";
        let m = parse_packed(content);
        assert!(m.source.is_none());
        assert!(m.whole_file_handle.is_none());
        assert_eq!(m.markers.len(), 1);
        assert_eq!(m.markers[0].handle, "ks_xyz");
        assert_eq!(
            m.markers[0].tokens, 0,
            "missing tokens key falls back to 0, not panic"
        );
    }
}
