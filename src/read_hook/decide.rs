//! The decision tree: given a Read PreToolUse event, decide whether to redirect.
//! The companion `view` module owns view construction; `apply` (in the parent module)
//! handles the side effects of acting on the decision.

use super::view::{build_view, populate_store};
use crate::config::{read_cache_dir, read_hook_enabled};
use crate::hash::sha1_hex;
use crate::json::Json;
use crate::sha256::sha256_hex;
use crate::token_estimate::{tokens, tokens_bytes};
use crate::why_log::{LogEntry, Reason};
use std::fs;
use std::path::PathBuf;

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

    // 5. Decide whether to load the cache view or build a fresh one. We CAPTURE
    //    `cache_existed_before` here, BEFORE any potential write below — otherwise
    //    a later `cache_path.exists()` check would always return true (we just
    //    wrote it) and we'd mislabel every fresh build as a cache hit. The
    //    `regenerated` flag records the rarer in-between case: the cache file
    //    was there but unreadable (corruption, permission flip), so we rebuilt.
    let raw_tokens = tokens_bytes(&source);
    let cache_existed_before = cache_path.exists();
    let mut regenerated = false;
    // Whether or not we have a cached view, we re-populate the store with the elision
    // blocks and the whole-file handle. This is what makes the `knapsack expand
    // ks2_X` instructions printed in the view actually resolve — the previous
    // implementation built the view but never stored the bytes, so every recall hit
    // "no such handle". Population is content-addressed + idempotent + cheap (O(file
    // size) one structural compress pass), and it self-heals the "user wiped the
    // store but kept the cache" recovery case.
    let view = if cache_existed_before {
        match fs::read_to_string(&cache_path) {
            Ok(v) => {
                // Cache hit: re-stamp store handles so recall keeps working even if
                // a stray `knapsack uninstall --purge` or `rm -rf ~/.knapsack/store`
                // happened between sessions.
                populate_store(&path, &source, &session_id);
                v
            }
            Err(_) => {
                regenerated = true;
                build_view(&path, &source, &session_id)
            }
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
    if !cache_existed_before {
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
    } else if regenerated {
        // Cache existed but was unreadable — we rebuilt the view in memory; overwrite
        // the on-disk file so the next read can hit a fresh, working cache. A second
        // write failure stays non-fatal (fail-open) for the same reason as elsewhere:
        // the in-memory view is still usable for THIS redirect.
        let _ = fs::write(&cache_path, view.as_bytes());
    }

    // 8. Pick the reason FROM THE PRE-WRITE STATE — the old `cache_path.exists()`
    //    after-the-write check would always report "cache-hit" since we just wrote
    //    it, so every first read for a file got the wrong label. Three distinct
    //    stories show up in `knapsack why-last`:
    //      - Reason::CacheHit                          → hot path, no rebuild
    //      - Reason::RedirectEmitted + note=regenerated → cache was corrupt, we rebuilt
    //      - Reason::RedirectEmitted (no note)         → first time we've seen this content
    let (reason, note) = if cache_existed_before && !regenerated {
        (Reason::CacheHit, None)
    } else if regenerated {
        (Reason::RedirectEmitted, Some("regenerated"))
    } else {
        (Reason::RedirectEmitted, None)
    };
    let mut entry = LogEntry::new(reason)
        .path(path_str)
        .bytes(bytes_len)
        .raw_tokens(raw_tokens)
        .view_tokens(view_tokens)
        .redirect_to(cache_path.display().to_string());
    if let Some(n) = note {
        entry = entry.note(n);
    }
    ReadDecision::Redirect { log: entry, redirect_to: cache_path }
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
    use crate::json::Json;

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
