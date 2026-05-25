//! `knapsack ab` — the head-to-head. Reads two metrics.jsonl files (Knapsack's and
//! Rucksack's), computes net token savings the same way for both (net = saved −
//! refetched), and reports per-session detail plus an aggregate head-to-head.
//!
//! Honest caveat baked into the output: Rucksack's metrics carry NO session id, so the
//! per-session table is Knapsack-only; the apples-to-apples comparison is the aggregate.
//! Field names are normalized across the two schemas:
//!   raw   = "raw" (knapsack) | "orig" (rucksack)
//!   shown = "shown"          | "comp"
//!   saved = "saved" (both)        refetched = expand "tokens" (both)

use crate::json::{self, Json};
use std::collections::HashMap;
use std::fs;
use std::path::Path;

#[derive(Default, Clone)]
pub struct Agg {
    pub compress_events: i64,
    pub expand_calls: i64,
    pub failed_expands: i64,
    pub raw: i64,
    pub shown: i64,
    pub saved: i64,
    pub refetched: i64,
    pub delta_hits: i64,
    pub evicted: i64,
}

impl Agg {
    pub fn net(&self) -> i64 {
        self.saved - self.refetched
    }
}

pub struct Report {
    pub knapsack_total: Agg,
    pub rucksack_total: Agg,
    pub knapsack_sessions: Vec<(String, Agg)>,
    pub knapsack_path: String,
    pub rucksack_path: String,
}

fn numk(v: &Json, keys: &[&str]) -> i64 {
    for k in keys {
        if let Some(n) = v.get(k).and_then(|x| x.as_f64()) {
            return n as i64;
        }
    }
    0
}

fn add_compress(a: &mut Agg, raw: i64, shown: i64, saved: i64, dh: i64, ev: i64) {
    a.compress_events += 1;
    a.raw += raw;
    a.shown += shown;
    a.saved += saved;
    a.delta_hits += dh;
    a.evicted += ev;
}

fn add_expand(a: &mut Agg, tokens: i64, ok: bool) {
    a.expand_calls += 1;
    if ok {
        a.refetched += tokens;
    } else {
        a.failed_expands += 1;
    }
}

/// Read one metrics.jsonl into a grand total + per-session aggregates.
pub fn read(path: &Path) -> (Agg, HashMap<String, Agg>) {
    let mut total = Agg::default();
    let mut map: HashMap<String, Agg> = HashMap::new();
    let text = fs::read_to_string(path).unwrap_or_default();
    for line in text.lines() {
        let v = match json::parse(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let session = v.get("session").and_then(|x| x.as_str()).unwrap_or("(no session)").to_string();
        let e = map.entry(session).or_default();
        match v.get("event").and_then(|x| x.as_str()) {
            Some("compress") => {
                let raw = numk(&v, &["raw", "orig"]);
                let shown = numk(&v, &["shown", "comp"]);
                let saved = if v.get("saved").is_some() { numk(&v, &["saved"]) } else { raw - shown };
                let dh = numk(&v, &["delta_hits"]);
                let ev = numk(&v, &["evicted"]);
                add_compress(e, raw, shown, saved, dh, ev);
                add_compress(&mut total, raw, shown, saved, dh, ev);
            }
            Some("expand") => {
                let ok = v.get("ok").and_then(|x| x.as_bool()).unwrap_or(true);
                let tokens = numk(&v, &["tokens"]);
                add_expand(e, tokens, ok);
                add_expand(&mut total, tokens, ok);
            }
            _ => {}
        }
    }
    (total, map)
}

pub fn compare(knapsack: &Path, rucksack: &Path) -> Report {
    let (kt, ks) = read(knapsack);
    let (rt, _rs) = read(rucksack);
    let mut sessions: Vec<(String, Agg)> = ks.into_iter().collect();
    sessions.sort_by_key(|s| std::cmp::Reverse(s.1.net())); // best net first
    Report {
        knapsack_total: kt,
        rucksack_total: rt,
        knapsack_sessions: sessions,
        knapsack_path: knapsack.display().to_string(),
        rucksack_path: rucksack.display().to_string(),
    }
}

fn commafy(n: i64) -> String {
    let neg = n < 0;
    let digits = n.abs().to_string();
    let len = digits.len();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (len - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    if neg {
        format!("-{}", out)
    } else {
        out
    }
}

fn short(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(n - 1).collect::<String>())
    }
}

pub fn format(r: &Report) -> String {
    let mut o = String::new();
    o.push_str("Knapsack vs Rucksack — net token savings   (net = saved − refetched)\n\n");

    // per-session (knapsack only — rucksack metrics are sessionless)
    o.push_str("per session (knapsack)\n");
    o.push_str(&format!(
        "{:<20}{:>11}{:>11}{:>10}{:>11}{:>7}{:>7}{:>9}\n",
        "session", "raw", "saved", "refetch", "net", "delta", "evict", "exp(f)"
    ));
    o.push_str(&"─".repeat(86));
    o.push('\n');
    if r.knapsack_sessions.is_empty() {
        o.push_str("  (no knapsack data found at ");
        o.push_str(&r.knapsack_path);
        o.push_str(")\n");
    }
    for (sid, a) in &r.knapsack_sessions {
        o.push_str(&format!(
            "{:<20}{:>11}{:>11}{:>10}{:>11}{:>7}{:>7}{:>9}\n",
            short(sid, 19),
            commafy(a.raw),
            commafy(a.saved),
            commafy(a.refetched),
            commafy(a.net()),
            commafy(a.delta_hits),
            commafy(a.evicted),
            format!("{}({})", a.expand_calls, a.failed_expands),
        ));
    }

    // head-to-head aggregate
    let (kn, ru) = (&r.knapsack_total, &r.rucksack_total);
    o.push_str("\nhead-to-head (aggregate)\n");
    o.push_str(&format!("{:<12}{:>10}{:>12}{:>12}{:>12}{:>12}\n", "engine", "compress", "raw", "saved", "refetched", "NET"));
    o.push_str(&"─".repeat(70));
    o.push('\n');
    let row = |name: &str, a: &Agg| {
        format!(
            "{:<12}{:>10}{:>12}{:>12}{:>12}{:>12}\n",
            name,
            commafy(a.compress_events),
            commafy(a.raw),
            commafy(a.saved),
            commafy(a.refetched),
            commafy(a.net())
        )
    };
    o.push_str(&row("rucksack", ru));
    o.push_str(&row("knapsack", kn));
    o.push_str(&"─".repeat(70));
    o.push('\n');

    let (winner, delta) = if kn.net() >= ru.net() {
        ("knapsack", kn.net() - ru.net())
    } else {
        ("rucksack", ru.net() - kn.net())
    };
    let pct = if ru.net() > 0 {
        format!("{}% better", (kn.net() - ru.net()) * 100 / ru.net())
    } else {
        "n/a (no positive rucksack baseline)".to_string()
    };
    o.push_str(&format!("winner: {}   (+{} net tokens, {})\n", winner, commafy(delta), pct));

    if ru.compress_events == 0 {
        o.push_str(&format!("\nnote: no rucksack data at {} — showing knapsack only.\n", r.rucksack_path));
    }
    o.push_str("note: rucksack's metrics carry no session id, so the per-session table is\n");
    o.push_str("      knapsack-only; the head-to-head above is the apples-to-apples figure.\n");
    o
}
