//! The honest scoreboard, as JSONL (`~/.knapsack/metrics.jsonl`). Records both sides so
//! we can prove the live claim: does conditional compression improve SESSION net_saved
//! over Rucksack-style, once recall (expand) cost is paid back?
//!   compress: {event,session,raw,shown,saved,delta_hits,evicted}
//!   expand:   {event,session,tokens,ok}
//! Reuses the in-tree json module (zero-dep) for write + parse.

use crate::config::metrics_path;
use crate::json::{self, Json};
use std::fs;
use std::fs::OpenOptions;
use std::io::Write;
use std::time::{SystemTime, UNIX_EPOCH};

fn now_ms() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as f64).unwrap_or(0.0)
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

pub fn record_compress(session: &str, raw: usize, shown: usize, saved: isize, delta_hits: usize, evicted: usize) {
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

pub fn record_expand(session: &str, tokens: usize, ok: bool) {
    append(&Json::Obj(vec![
        ("t".into(), Json::Num(now_ms())),
        ("event".into(), Json::Str("expand".into())),
        ("session".into(), Json::Str(session.into())),
        ("tokens".into(), Json::Num(tokens as f64)),
        ("ok".into(), Json::Bool(ok)),
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
    report_for(None)
}

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
