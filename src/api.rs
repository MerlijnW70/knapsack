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
use crate::pack::{pack, PackResult};
use crate::recall::{expand, RecallOut};
use crate::store::Store;
use crate::{hash::Handle, metrics};

pub struct PackRequest {
    pub session_id: String,
    pub command: Option<String>,
    pub bytes: Vec<u8>,
    pub content_hint: Option<ContentType>,
    pub step: u64,
}

pub struct ExpandRequest {
    pub handle: Handle,
    pub range: Option<(usize, usize)>,
    pub grep: Option<String>,
    pub context: usize,
    pub session_id: String,
}

/// Compress tool output for a session. Loads the session ledger, packs conditionally,
/// persists residency, and records metrics. Returns the lossy view + recall handles.
pub fn pack_output(req: PackRequest) -> PackResult {
    let store = Store::new(store_dir());
    let mut ledger = Ledger::load(session_path(&req.session_id));
    let ct = req.content_hint.unwrap_or_else(|| detect(&req.bytes, req.command.as_deref()));
    let r = pack(&req.bytes, ct, &store, &mut ledger, req.step);
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
pub fn expand_handle(req: ExpandRequest) -> Option<RecallOut> {
    let store = Store::new(store_dir());
    let out = expand(&store, &req.handle, req.range, req.grep.as_deref(), req.context);
    match &out {
        Some(o) => {
            let n = match o {
                RecallOut::Bytes(b) => crate::token_estimate::tokens_bytes(b),
                RecallOut::Text(t) => crate::token_estimate::tokens(t),
            };
            metrics::record_expand(&req.session_id, n, true);
        }
        None => metrics::record_expand(&req.session_id, 0, false),
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
