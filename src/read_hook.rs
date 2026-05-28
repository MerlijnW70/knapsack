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
//!
//! Module layout:
//!   - `decide`  — the decision tree (`decide`, `decide_with_gate`, `ReadDecision`).
//!   - `view`    — view construction + store population (private to this module).
//!   - this file — the side-effect layer (`apply`, `run`) that turns a decision into
//!     a JSONL log entry, a metrics row, and the `updatedInput` envelope on stdout.

mod decide;
mod view;

pub use decide::{decide, decide_with_gate, ReadDecision};

use crate::json::{self, Json};
use crate::why_log;

/// Apply a decision to the PreToolUse event and (when redirecting) print the
/// `hookSpecificOutput.updatedInput` envelope. Records the reason to the JSONL log.
pub fn apply(evt: &Json, decision: ReadDecision) {
    match decision {
        ReadDecision::PassThrough { log } => {
            why_log::record(&log);
        }
        ReadDecision::Redirect { redirect_to, log } => {
            why_log::record(&log);
            // Write-through to metrics.jsonl as a `compress` event so `knapsack status`,
            // `metrics`, and `ab` see read-side savings alongside Bash-hook compresses.
            // Without this, a session whose only activity was Read redirects renders as
            // "0 tokens saved" in /knapsack status even though the model received heavily
            // compressed file views. Fires only on the Redirect branch (the PassThrough
            // arm has no saving to record). Both Reason::RedirectEmitted and
            // Reason::CacheHit land here, which is correct: re-using a cached compact
            // view saves the same (raw - view) tokens as a fresh redirect.
            if let (Some(raw), Some(view)) = (log.raw_tokens, log.view_tokens) {
                let session_id = evt
                    .get("session_id")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.trim().is_empty())
                    .unwrap_or("read-hook");
                let saved = raw as isize - view as isize;
                crate::metrics::record_compress(session_id, raw, view, saved, 0, 0);
            }
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
