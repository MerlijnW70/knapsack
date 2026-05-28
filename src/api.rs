//! The Claude-facing integration boundary — kept deliberately thin and stable. Whatever
//! wires Knapsack into Claude Code (a PreToolUse hook, an MCP server, or a later Rust
//! `mcp.rs`) speaks ONLY through these four calls. Everything correctness-critical lives
//! below this line in the deterministic core; the glue above it stays replaceable.
//!
//!   pack_output(req)      compress tool output for a session, conditioned on what it saw
//!   expand_handle(req)    recall exact bytes (or a slice) behind a handle
//!   record_residency(..)  mark a block still present in the context window
//!   evict(..)             mark a block paged out (so it's re-sent, not back-referenced)

use crate::config::{resident_budget, session_path, store_dir};
use crate::content_type::{detect, ContentType};
use crate::ledger::Ledger;
pub use crate::metrics::{ExpandCaller, ExpandMode};
use crate::pack::{pack_with_transcript, PackResult};
use crate::recall::{expand, RecallOut};
use crate::store::Store;
use crate::transcript;
use crate::{hash::Handle, metrics};
use std::path::PathBuf;

pub struct PackRequest {
    pub session_id: String,
    pub command: Option<String>,
    pub bytes: Vec<u8>,
    pub content_hint: Option<ContentType>,
    pub step: u64,
    /// Optional transcript path from the Claude Code hook event. When provided AND
    /// readable, residency is intersected with handles the transcript proves are
    /// still in context after the latest boundary. When None or unreadable, behaves
    /// as before — ledger-only residency. See `src/transcript.rs`.
    pub transcript_path: Option<PathBuf>,
}

pub struct ExpandRequest {
    pub handle: Handle,
    pub range: Option<(usize, usize)>,
    pub grep: Option<String>,
    pub context: usize,
    pub session_id: String,
    /// Which integration surface invoked this expand — surfaced into metrics.jsonl so
    /// post-hoc analysis can attribute recall cost to the right caller (model via MCP,
    /// user via CLI, or the Read hook's self-heal path).
    pub caller: ExpandCaller,
}

/// Compress tool output for a session. Loads the session ledger, packs conditionally,
/// persists residency, and records metrics. Returns the lossy view + recall handles.
pub fn pack_output(req: PackRequest) -> PackResult {
    // Stamp each block's `.meta` sidecar with the session id so a later `expand` from
    // any process can attribute the refetch back to THIS session — see
    // `Store::with_session` for why this exists.
    let store = Store::with_session(store_dir(), &req.session_id);
    let mut ledger = Ledger::load(session_path(&req.session_id));
    let ct = req
        .content_hint
        .unwrap_or_else(|| detect(&req.bytes, req.command.as_deref()));
    // Conservative transcript gate: scan only when the path is provided AND the file
    // parses with ok=true. Any failure (missing file, empty, totally unparseable)
    // returns ok=false and we drop the gate — same behaviour as before this feature.
    let scan = req.transcript_path.as_deref().map(transcript::scan);
    let resident_set = scan
        .as_ref()
        .and_then(|s| if s.ok { Some(&s.resident) } else { None });
    let r = pack_with_transcript(&req.bytes, ct, &store, &mut ledger, req.step, resident_set);
    // Conservative residency: keep the resident set within budget so delta back-references
    // never point past the (estimated) context window.
    ledger.enforce_budget(resident_budget());
    ledger.save();
    metrics::record_compress(
        &req.session_id,
        r.raw_tokens_est,
        r.shown_tokens_est,
        r.saved_tokens_est,
        r.delta_hits,
        r.evicted_resends,
    );
    r
}

/// Recall behind a handle. Full = exact bytes; range/grep = sliced decoded text.
///
/// Session attribution: the refetch token cost is charged to the session that ORIGINALLY
/// stored the block (looked up via the `.meta` sidecar), NOT the caller's session id.
/// This is what makes per-session net accounting (saved minus refetched) cohere when
/// the bash hook and the MCP server live in different processes with different session
/// ids. Falls back to `req.session_id` when the block has no meta (legacy / pre-stamping
/// blocks) or no session field set. See `Store::with_session` + `Store::block_session`.
pub fn expand_handle(req: ExpandRequest) -> Option<RecallOut> {
    let store = Store::new(store_dir());
    let out = expand(
        &store,
        &req.handle,
        req.range,
        req.grep.as_deref(),
        req.context,
    );
    let attrib = store
        .block_session(&req.handle)
        .unwrap_or_else(|| req.session_id.clone());
    // Mode reflects the slicing path the caller asked for. Grep wins when both are set
    // because the range only acts as a pre-filter for the grep — the cost the recall
    // actually pays is the grep result.
    let mode = if req.grep.is_some() {
        ExpandMode::Grep
    } else if req.range.is_some() {
        ExpandMode::Lines
    } else {
        ExpandMode::Whole
    };
    match &out {
        Some(o) => {
            let n = match o {
                RecallOut::Bytes(b) => crate::token_estimate::tokens_bytes(b),
                RecallOut::Text(t) => crate::token_estimate::tokens(t),
            };
            metrics::record_expand(&attrib, &req.handle, n, true, mode, req.caller);
        }
        None => metrics::record_expand(&attrib, &req.handle, 0, false, mode, req.caller),
    }
    out
}

pub fn record_residency(session_id: &str, handle: &Handle, step: u64) {
    let mut ledger = Ledger::load(session_path(session_id));
    ledger.note(handle, step, 0); // token weight unknown when marked externally
    ledger.save();
}

pub fn evict(session_id: &str, handle: &Handle) {
    let mut ledger = Ledger::load(session_path(session_id));
    ledger.evict(handle);
    ledger.save();
}
