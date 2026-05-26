//! The Read-tool side of the PreToolUse hook — the "input reduction" path. Default ON
//! after `knapsack install`; set `KNAPSACK_READ_HOOK=0` to disable.
//!
//! What it does:
//!   - Sees a Read event for an absolute `file_path`.
//!   - Decides (see `Reason` in `why_log`) whether to redirect.
//!   - When all checks pass: writes a compact view of the file to the read cache
//!     (`~/.knapsack/read_cache/<sha256>.md`) and emits `updatedInput.file_path`
//!     pointing at it. Claude Code then reads the small view instead of the large
//!     original. The full original is recoverable via `knapsack expand <handle>`
//!     OR by reading the original path directly (we never mutate the source).
//!   - Every decision is recorded to the JSONL log so `knapsack why-last` answers
//!     "why didn't Knapsack redirect that 800-KB file?".
//!
//! Conservatism by construction:
//!   - Fails open everywhere. Any error or any uncertain branch logs a reason and
//!     emits NO rewrite — Claude reads the original as if Knapsack didn't exist.
//!   - Refuses to emit a rewrite if the view isn't meaningfully smaller than raw.
//!   - Never touches the original file. Never alters the prompt or other tool
//!     inputs. The only mutation is `file_path` in the redirect case.

use crate::config::{read_cache_dir, read_hook_enabled, store_dir};
use crate::content_type::detect;
use crate::hash::sha1_hex;
use crate::json::{self, Json};
use crate::sha256::sha256_hex;
use crate::store::Store;
use crate::structural;
use crate::token_estimate::{tokens, tokens_bytes};
use crate::why_log::{self, LogEntry, Reason};
use std::fs;
use std::path::{Path, PathBuf};

// Knobs — conservative starting values. Tuned later from `why-last` evidence.
const REDIRECT_MIN_BYTES: u64 = 8 * 1024; // smaller than this -> read raw, cheaper
const REDIRECT_MAX_BYTES: u64 = 4 * 1024 * 1024; // larger than this -> too risky for v1
const MIN_REDUCTION_PERCENT: i64 = 25; // must save >=25% of tokens to justify the indirection

/// What the hook decided to do. Drives both the `updatedInput` emission and the log
/// entry that goes to the JSONL trail. Carrying the path + sizes here makes the call
/// site straightforward and keeps every reason consistently logged.
pub enum ReadDecision {
    /// Pass through unchanged (no rewrite). `log` carries the structured reason.
    PassThrough { log: LogEntry },
    /// Emit a rewrite: Claude Code reads `redirect_to` instead of the original. The
    /// log entry's `reason` is always `RedirectEmitted` for this variant.
    Redirect { redirect_to: PathBuf, log: LogEntry },
}

/// Inspect a Read PreToolUse event and decide what to do. Reads the env-driven gate
/// (`KNAPSACK_READ_HOOK`) and dispatches to `decide_with_gate`. Most callers want this.
pub fn decide(evt: &Json) -> ReadDecision {
    decide_with_gate(read_hook_enabled(), evt)
}

/// Same as `decide`, but with the gate passed in explicitly. Tests use this so they
/// don't have to race on a global env var; the live hook uses `decide()`.
pub fn decide_with_gate(enabled: bool, evt: &Json) -> ReadDecision {
    // 1. Gate. Default-off contract — gated explicitly here so the rest of the
    //    decision tree can assume "we're allowed to do something".
    if !enabled {
        return PassThrough(LogEntry::new(Reason::GateDisabled));
    }
    // 2. Input shape.
    let tool_input = match evt.get("tool_input") {
        Some(t) => t,
        None => return PassThrough(LogEntry::new(Reason::BadInput).note("missing tool_input")),
    };
    let path_str = match tool_input.get("file_path").and_then(|v| v.as_str()) {
        Some(p) if !p.trim().is_empty() => p.to_string(),
        _ => return PassThrough(LogEntry::new(Reason::BadInput).note("missing file_path")),
    };
    let path = PathBuf::from(&path_str);

    // Extract the Claude Code session id from the event so every block we put into
    // the store gets stamped with it (via Store::with_session). This makes any
    // subsequent `knapsack_expand` MCP call attribute the recall cost back to THIS
    // session — same plumbing as the bash hook's `pack -` path uses. Falls back to
    // "read-hook" when the event has no session_id (CLI-driven tests, dry-runs).
    let session_id = evt
        .get("session_id")
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
        .unwrap_or("read-hook")
        .to_string();

    // 3. Slicing — partial reads aren't worth the indirection (a cache miss on the
    //    full file is much cheaper than rebuilding for every offset).
    if tool_input.get("offset").is_some() || tool_input.get("limit").is_some() {
        return PassThrough(
            LogEntry::new(Reason::SlicingRequested).path(path_str).note("offset/limit set"),
        );
    }

    // 4. Stat + read the source. Both are required before any decision; cheap fast
    //    failure for unreadable / disappeared files.
    let meta = match fs::metadata(&path) {
        Ok(m) => m,
        Err(e) => {
            return PassThrough(
                LogEntry::new(Reason::FileUnreadable).path(path_str).note(format!("stat: {e}")),
            );
        }
    };
    let bytes_len = meta.len();
    if bytes_len < REDIRECT_MIN_BYTES {
        return PassThrough(LogEntry::new(Reason::TooSmall).path(path_str).bytes(bytes_len));
    }
    if bytes_len > REDIRECT_MAX_BYTES {
        return PassThrough(LogEntry::new(Reason::TooLarge).path(path_str).bytes(bytes_len));
    }

    let source = match fs::read(&path) {
        Ok(b) => b,
        Err(e) => {
            return PassThrough(
                LogEntry::new(Reason::FileUnreadable).path(path_str).note(format!("read: {e}")),
            );
        }
    };
    let digest = sha256_hex(&source);
    let cache_dir = read_cache_dir();
    // Cache filename is `<sha256(source)[:32]>_<sha1(path)[:8]>.md`. Adding the path
    // tag means two different paths that happen to hold the same bytes (a copy, an
    // identical lock file across workspaces) get DISTINCT cache files — each with its
    // own correct `Original file: <path>` header. The byte-exact store is unaffected
    // (still one entry per unique content); we just spend a few hundred extra bytes
    // of disk in the rare collision case to keep the per-read header honest.
    let path_tag = &sha1_hex(path_str.as_bytes())[..8];
    let cache_path = cache_dir.join(format!("{}_{}.md", &digest[..32], path_tag));

    // 5. Cache present? Note the reason so `why-last` shows whether we're hitting
    //    a stable cache or regenerating after a change. We can't distinguish "first
    //    time seen" from "file changed" without a path→hash index; for the spike we
    //    treat both as a cache miss (reason: FileChanged) so the log stays honest.
    let raw_tokens = tokens_bytes(&source);
    // Whether or not we have a cached view, we re-populate the store with the elision
    // blocks and the whole-file handle. This is what makes the `knapsack expand
    // ks2_X` instructions printed in the view actually resolve — the previous
    // implementation built the view but never stored the bytes, so every recall hit
    // "no such handle". Population is content-addressed + idempotent + cheap (O(file
    // size) one structural compress pass), and it self-heals the "user wiped the
    // store but kept the cache" recovery case.
    let view = if cache_path.exists() {
        match fs::read_to_string(&cache_path) {
            Ok(v) => {
                // Cache hit: re-stamp store handles so recall keeps working even if
                // a stray `knapsack uninstall --purge` or `rm -rf ~/.knapsack/store`
                // happened between sessions.
                populate_store(&path, &source, &session_id);
                v
            }
            Err(_) => build_view(&path, &source, &session_id),
        }
    } else {
        build_view(&path, &source, &session_id)
    };
    let view_tokens = tokens(&view);

    // 6. Never-worse-than-raw — if the view isn't meaningfully smaller, the
    //    indirection costs more than it saves (extra file open, header bytes, model
    //    interpretation overhead). Pass through, log, move on.
    let saved = raw_tokens as i64 - view_tokens as i64;
    let pct = if raw_tokens > 0 { saved * 100 / raw_tokens as i64 } else { 0 };
    if pct < MIN_REDUCTION_PERCENT {
        return PassThrough(
            LogEntry::new(Reason::WorseThanRaw)
                .path(path_str)
                .bytes(bytes_len)
                .raw_tokens(raw_tokens)
                .view_tokens(view_tokens),
        );
    }

    // 7. Persist the view if not already on disk. Any write failure -> pass through.
    if !cache_path.exists() {
        if let Some(parent) = cache_path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        if let Err(e) = fs::write(&cache_path, view.as_bytes()) {
            return PassThrough(
                LogEntry::new(Reason::FileUnreadable)
                    .path(path_str)
                    .note(format!("cache write: {e}")),
            );
        }
    }
    // One log line per decision, please — anything more is noise for `why-last`.
    // The redirect IS the action; cache state goes in `note` for transparency.
    let cache_note = if cache_path.exists() { "cache-hit" } else { "regenerated" };
    ReadDecision::Redirect {
        log: LogEntry::new(Reason::RedirectEmitted)
            .path(path_str)
            .bytes(bytes_len)
            .raw_tokens(raw_tokens)
            .view_tokens(view_tokens)
            .redirect_to(cache_path.display().to_string())
            .note(cache_note),
        redirect_to: cache_path,
    }
}

/// Apply a decision to the PreToolUse event and (when redirecting) print the
/// `hookSpecificOutput.updatedInput` envelope. Records the reason to the JSONL log.
pub fn apply(evt: &Json, decision: ReadDecision) {
    match decision {
        ReadDecision::PassThrough { log } => {
            why_log::record(&log);
        }
        ReadDecision::Redirect { redirect_to, log } => {
            why_log::record(&log);
            let new_path = redirect_to.display().to_string();
            let tool_input = match evt.get("tool_input") {
                Some(t) => t,
                None => return,
            };
            let mut obj = match tool_input {
                Json::Obj(o) => o.clone(),
                _ => return,
            };
            json::set_key(&mut obj, "file_path", Json::Str(new_path));
            let out = Json::Obj(vec![(
                "hookSpecificOutput".into(),
                Json::Obj(vec![
                    ("hookEventName".into(), Json::Str("PreToolUse".into())),
                    ("updatedInput".into(), Json::Obj(obj)),
                ]),
            )]);
            print!("{}", json::to_string(&out));
        }
    }
}

/// Entry point invoked from `hook::run_hook` when the event is a Read. End-to-end:
/// decide, log, emit. Always fails open — never panics, never blocks the call.
pub fn run(evt: &Json) {
    let d = decide(evt);
    apply(evt, d);
}

/// Detects markdown by file extension. Used by the read hook to route through
/// `pack_doc` instead of `structural::compress`, which doesn't recognise heading /
/// code-fence / list structure. Content sniffing would be brittle (any text starting
/// with `#` could be markdown or could be a config comment) and extension matches
/// real-world authoring tooling, so we keep it strict.
fn is_markdown_path(p: &Path) -> bool {
    p.extension()
        .and_then(|e| e.to_str())
        .map(|e| {
            let lc = e.to_ascii_lowercase();
            matches!(lc.as_str(), "md" | "markdown" | "mdown" | "mkd" | "mkdn")
        })
        .unwrap_or(false)
}

/// Compute the (compact view body, whole-file handle) pair for a source, picking the
/// right compressor for its content type.
///
/// Three classes of handle land in the store:
///   1. Every elision returned by `structural::compress` (when used) — recall targets
///      the view points at in its `[Knapsack: ... · recall ks2_X]` markers.
///   2. The whole-file handle — recall target the outer header advertises, and (for
///      markdown) the target every `<!-- ks-recall handle=... lines=A-B -->` points
///      at via `--lines`.
///   3. Storage is content-addressed + idempotent (`put_with_handle` skips when the
///      file already exists), so calling this on every view-build is cheap.
///
/// Session id is stamped into each block's `.meta` so a later `expand_handle`
/// attributes the recall to THIS session.
fn compile_compact(source_path: &Path, bytes: &[u8], store: &Store) -> (String, crate::hash::Handle) {
    if is_markdown_path(source_path) {
        // pack_doc understands markdown structure (headings, code fences, lists,
        // blockquotes). It puts the WHOLE FILE in the store under one handle and
        // emits `<!-- ks-recall handle=H lines=A-B -->` markers that recall via
        // `knapsack expand H --lines A-B`. No per-section block writes needed.
        let r = crate::pack_doc::pack_doc(&source_path.to_string_lossy(), bytes, store);
        // pack_doc emits its own two-line HTML header + a blank line — strip them so
        // the read-cache header above stays the single source of identity.
        let body = strip_pack_doc_header(&r.view);
        (body, r.handle)
    } else {
        // Everything else goes through the general structural compressor (Code/Log/JSON).
        let ct = detect(bytes, Some(source_path.to_string_lossy().as_ref()));
        let (compact, elisions) = structural::compress(bytes, 0, bytes.len(), ct);
        for el in &elisions {
            store.put_with_handle(&el.handle, &bytes[el.start..el.end]);
        }
        let whole = store.put(bytes);
        (compact, whole)
    }
}

/// Strip `pack_doc`'s two-line header (machine manifest + inspect hint) plus the
/// blank line that follows. We replace it with the read-cache header so users see
/// one consistent header across all file types. If the input doesn't start with
/// pack_doc's manifest comment, we hand it back unchanged — safe fallback.
fn strip_pack_doc_header(view: &str) -> String {
    if !view.starts_with("<!-- ks-pack source=") {
        return view.to_string();
    }
    let mut iter = view.splitn(4, '\n');
    let _l1 = iter.next(); // <!-- ks-pack source=... -->
    let _l2 = iter.next(); // <!-- knapsack inspect ... -->
    let _l3 = iter.next(); // blank
    iter.next().unwrap_or("").to_string()
}

/// Build the compact view file that Claude reads instead of the original AND populate
/// the byte-exact store with every handle the view names — so the `knapsack expand
/// ks2_X` instructions in the view resolve byte-exact when the model invokes them.
fn build_view(source_path: &Path, bytes: &[u8], session_id: &str) -> String {
    let store = Store::with_session(store_dir(), session_id);
    let (compact, whole_handle) = compile_compact(source_path, bytes, &store);

    let mut o = String::new();
    o.push_str("<!-- Knapsack read cache -->\n");
    o.push_str(&format!("<!-- Original file: {} -->\n", source_path.display()));
    o.push_str(&format!("<!-- Source digest: sha256={} -->\n", sha256_hex(bytes)));
    o.push_str("<!-- This file is a COMPRESSED VIEW. -->\n");
    o.push_str("<!--   Exact original is on disk at the path above. -->\n");
    o.push_str(&format!(
        "<!--   Or recall exact bytes via `knapsack expand {}`. -->\n\n",
        whole_handle
    ));
    o.push_str("[Knapsack: read-cache view · the original at the path above remains the source of truth]\n\n");
    o.push_str(&compact);
    o
}

/// Mirror of the store-population side of `build_view` without producing the view text.
/// Used on the cache-hit path so the store stays in sync with the cache even if the
/// store dir was wiped between sessions (a `knapsack uninstall --purge` followed by
/// the user keeping their cache dir, e.g. when restoring from a backup). Calls
/// `compile_compact` so the same routing logic (markdown -> pack_doc, else
/// structural::compress) applies to the recovery write as to the original build —
/// no path-specific shortcut that could leave the store missing handles the cached
/// view advertises.
fn populate_store(source_path: &Path, bytes: &[u8], session_id: &str) {
    let store = Store::with_session(store_dir(), session_id);
    // Side-effect of compile_compact is the store population we want; the returned
    // (compact, handle) pair is unused here.
    let _ = compile_compact(source_path, bytes, &store);
}

// Decision-tree helper: every "this is why we don't redirect" path uses the same
// shape, so giving it a name keeps the control flow scannable.
#[allow(non_snake_case)]
fn PassThrough(log: LogEntry) -> ReadDecision {
    ReadDecision::PassThrough { log }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_event(file_path: &str, extra: &[(&str, Json)]) -> Json {
        let mut tool_input: Vec<(String, Json)> = vec![("file_path".into(), Json::Str(file_path.into()))];
        for (k, v) in extra {
            tool_input.push((k.to_string(), v.clone()));
        }
        Json::Obj(vec![
            ("tool_name".into(), Json::Str("Read".into())),
            ("tool_input".into(), Json::Obj(tool_input)),
        ])
    }

    #[test]
    fn decide_passes_through_when_gate_disabled() {
        // Gate is passed in explicitly so the test doesn't race on the process-global
        // KNAPSACK_READ_HOOK env var — `decide()` would have flipped on if the shell
        // exported it.
        let evt = make_event("/some/file", &[]);
        match decide_with_gate(false, &evt) {
            ReadDecision::PassThrough { log } => assert_eq!(log.reason, Reason::GateDisabled),
            _ => panic!("expected pass-through with gate-disabled reason"),
        }
    }
}
