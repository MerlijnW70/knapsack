//! The honest scoreboard, as JSONL (`~/.knapsack/metrics.jsonl`). Records both sides so
//! we can prove the live claim: does conditional compression improve SESSION net_saved
//! over Rucksack-style, once recall (expand) cost is paid back?
//!   compress: {event,session,raw,shown,saved,delta_hits,evicted}
//!   expand:   {event,session,handle,tokens,ok,mode,caller}
//! Reuses the in-tree json module (zero-dep) for write + parse.
//!
//! `mode` and `caller` were added so post-hoc analysis can answer "which handle is
//! getting whole-expanded repeatedly and who's asking?" without grepping the codebase.
//! The reader (`summary_filtered`) ignores them; they're write-only diagnostic fields.

use crate::config::metrics_path;
use crate::json::{self, Json};
use std::fs;
use std::fs::OpenOptions;
use std::io::Write;
use std::time::{SystemTime, UNIX_EPOCH};

fn now_ms() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as f64)
        .unwrap_or(0.0)
}

fn append(j: &Json) {
    let p = metrics_path();
    if let Some(parent) = p.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&p) {
        // Write the whole line (JSON + '\n') in ONE write_all: an O_APPEND write of a small
        // buffer is atomic on the common platforms, so concurrent writers from multiple
        // sessions/threads don't interleave and shred each other's lines. `writeln!` could
        // split into several writes and lose ~9% of lines under contention.
        let mut line = json::to_string(j);
        line.push('\n');
        let _ = f.write_all(line.as_bytes());
    }
}

pub fn record_compress(
    session: &str,
    raw: usize,
    shown: usize,
    saved: isize,
    delta_hits: usize,
    evicted: usize,
) {
    append(&Json::Obj(vec![
        ("t".into(), Json::Num(now_ms())),
        ("event".into(), Json::Str("compress".into())),
        ("session".into(), Json::Str(session.into())),
        ("raw".into(), Json::Num(raw as f64)),
        ("shown".into(), Json::Num(shown as f64)),
        ("saved".into(), Json::Num(saved as f64)),
        ("delta_hits".into(), Json::Num(delta_hits as f64)),
        ("evicted".into(), Json::Num(evicted as f64)),
    ]));
}

/// Which slicing path produced the recall, for post-hoc cost analysis.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ExpandMode {
    Whole,
    Lines,
    Grep,
}

impl ExpandMode {
    fn as_str(self) -> &'static str {
        match self {
            ExpandMode::Whole => "whole",
            ExpandMode::Lines => "lines",
            ExpandMode::Grep => "grep",
        }
    }
}

/// Which integration surface invoked the recall. Lets us tell "model retried via MCP"
/// apart from "user typed `knapsack expand` in the shell" apart from "Read hook
/// recovered cache". Without this, every expand looks the same in metrics.jsonl.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ExpandCaller {
    Cli,
    Mcp,
    Hook,
}

impl ExpandCaller {
    fn as_str(self) -> &'static str {
        match self {
            ExpandCaller::Cli => "cli",
            ExpandCaller::Mcp => "mcp",
            ExpandCaller::Hook => "hook",
        }
    }
}

pub fn record_expand(
    session: &str,
    handle: &str,
    tokens: usize,
    ok: bool,
    mode: ExpandMode,
    caller: ExpandCaller,
) {
    append(&Json::Obj(vec![
        ("t".into(), Json::Num(now_ms())),
        ("event".into(), Json::Str("expand".into())),
        ("session".into(), Json::Str(session.into())),
        ("handle".into(), Json::Str(handle.into())),
        ("tokens".into(), Json::Num(tokens as f64)),
        ("ok".into(), Json::Bool(ok)),
        ("mode".into(), Json::Str(mode.as_str().into())),
        ("caller".into(), Json::Str(caller.as_str().into())),
    ]));
}

#[derive(Default)]
pub struct Summary {
    pub compress_events: usize,
    pub raw: usize,
    pub shown: usize,
    pub saved: isize,
    pub delta_hits: usize,
    pub evicted_backrefs_avoided: usize,
    pub expand_calls: usize,
    pub failed_expands: usize,
    pub refetched: usize,
    pub net: isize,
}

pub fn summary() -> Summary {
    summary_filtered(None)
}

/// Aggregate metrics, optionally restricted to one session id.
pub fn summary_filtered(session: Option<&str>) -> Summary {
    let mut s = Summary::default();
    if let Ok(text) = fs::read_to_string(metrics_path()) {
        for line in text.lines() {
            let v = match json::parse(line) {
                Ok(v) => v,
                Err(_) => continue,
            };
            if let Some(f) = session {
                if v.get("session").and_then(|x| x.as_str()) != Some(f) {
                    continue;
                }
            }
            let num = |k: &str| v.get(k).and_then(|x| x.as_f64()).unwrap_or(0.0);
            match v.get("event").and_then(|x| x.as_str()) {
                Some("compress") => {
                    s.compress_events += 1;
                    s.raw += num("raw") as usize;
                    s.shown += num("shown") as usize;
                    s.saved += num("saved") as isize;
                    s.delta_hits += num("delta_hits") as usize;
                    s.evicted_backrefs_avoided += num("evicted") as usize;
                }
                Some("expand") => {
                    s.expand_calls += 1;
                    if v.get("ok").and_then(|x| x.as_bool()) == Some(false) {
                        s.failed_expands += 1;
                    } else {
                        s.refetched += num("tokens") as usize;
                    }
                }
                _ => {}
            }
        }
    }
    s.net = s.saved - s.refetched as isize;
    s
}

pub fn report() -> String {
    // Default report: lead with the CURRENT session block (positive, recent work)
    // before the lifetime table. A user-reported confusion was that a fresh successful
    // run (+5,677 tokens, 85% reduction) read its own metrics output and saw the
    // lifetime −6.95M refetch headline first. Lifetime stays in full — nothing is
    // hidden — but the current session is what answers "did this run work?".
    let mut o = String::new();
    if let Some(id) = latest_compress_session_id() {
        let cur = summary_filtered(Some(&id));
        if cur.compress_events > 0 {
            o.push_str("current session\n  ");
            o.push_str(&format!("saved tokens           : {}\n  ", cur.saved));
            if cur.refetched > 0 {
                o.push_str(&format!("refetched              : {}\n  ", cur.refetched));
            }
            match net_reduction_pct(&cur) {
                Some(pct) => o.push_str(&format!("reduction              : {}%\n\n", pct)),
                None => o.push_str("reduction              : n/a\n\n"),
            }
        }
    }
    o.push_str(&report_for(None));
    o
}

/// Pre-existing lifetime/single-session report. Default `report()` now prepends a
/// "current session" block; this path is preserved untouched so `report_for(Some(s))`
/// (used internally) keeps the historic single-block shape.
pub fn report_for(session: Option<&str>) -> String {
    let s = summary_filtered(session);
    let verdict = if s.compress_events == 0 {
        "no data yet — wire the hook, run a session, then re-check"
    } else if s.net > 0 {
        "net POSITIVE — conditional compression is paying off"
    } else {
        "net NEGATIVE — expanding too much; tune what gets elided"
    };
    format!(
        "knapsack live stats\n  \
         compress events        : {}\n  \
         raw tokens             : {}\n  \
         shown tokens           : {}\n  \
         saved tokens           : {}\n  \
         delta hits             : {}\n  \
         evicted backrefs avoided: {}\n  \
         expand calls           : {}   (failed: {})\n  \
         tokens refetched       : {}\n  \
         NET saved              : {}\n\n  verdict: {}",
        s.compress_events,
        s.raw,
        s.shown,
        s.saved,
        s.delta_hits,
        s.evicted_backrefs_avoided,
        s.expand_calls,
        s.failed_expands,
        s.refetched,
        s.net,
        verdict
    )
}

fn net_reduction_pct(s: &Summary) -> Option<isize> {
    if s.raw == 0 {
        return None;
    }
    Some(s.net * 100 / s.raw as isize)
}

/// Latest compress-event session id (same heuristic as `status::latest_session_id`).
/// We duplicate the ~10-line function rather than make the status helper pub-crate
/// because the two modules are intentionally independent: `status` builds via
/// `ab::read`, `metrics` runs its own JSONL pass. The heuristic itself is the
/// contract — anchor on compresses, ignore expand-only sessions (e.g. "mcp"),
/// last-wins on `t` ties.
fn latest_compress_session_id() -> Option<String> {
    let text = fs::read_to_string(metrics_path()).ok()?;
    let mut best_t = f64::NEG_INFINITY;
    let mut best_id: Option<String> = None;
    for line in text.lines() {
        let v = match json::parse(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if v.get("event").and_then(|x| x.as_str()) != Some("compress") {
            continue;
        }
        let t = v.get("t").and_then(|x| x.as_f64()).unwrap_or(0.0);
        let id = v.get("session").and_then(|x| x.as_str()).unwrap_or("");
        if id.is_empty() {
            continue;
        }
        if t >= best_t {
            best_t = t;
            best_id = Some(id.to_string());
        }
    }
    best_id
}
