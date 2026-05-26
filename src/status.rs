//! `knapsack status` — the product-facing summary. Compact, non-technical, and the
//! default action when `knapsack` is run with no arguments (so the `/knapsack` slash
//! command lands here). `doctor` stays as the long-form diagnostic; this view answers
//! the four questions a normal user actually asks: am I on, what did I save, is recall
//! healthy, what's the store costing me?
//!
//! Read-only by construction: collects from the same JSONL metrics the rest of the
//! engine writes, the install config files, and the on-disk store. No side effects.

use crate::ab::{self, Agg};
use crate::config;
use crate::install::{mcp_config_path, mcp_has_server, settings_has_hook, settings_path};
use crate::json;
use std::fs;
use std::path::{Path, PathBuf};

/// Everything `render` needs. Public so tests can synthesize a Status without touching
/// the user's real ~/.claude or ~/.knapsack — and so callers can override the paths the
/// collector reads from (mirrors the env-override pattern used by config.rs / install.rs).
pub struct Status {
    pub hook_installed: bool,
    pub mcp_installed: bool,
    pub latest_session: Option<(String, Agg)>,
    pub total: Agg,
    pub session_count: usize,
    pub store_blocks: usize,
    pub store_bytes: u64,
}

pub struct Paths {
    pub settings: PathBuf,
    pub mcp_config: PathBuf,
    pub metrics: PathBuf,
    pub store: PathBuf,
}

impl Paths {
    pub fn defaults() -> Self {
        Self {
            settings: settings_path(),
            mcp_config: mcp_config_path(),
            metrics: config::metrics_path(),
            store: config::store_dir(),
        }
    }
}

pub fn collect() -> Status {
    collect_from(&Paths::defaults())
}

pub fn collect_from(p: &Paths) -> Status {
    let (total, sessions) = ab::read(&p.metrics);
    let latest = latest_session_id(&p.metrics).and_then(|id| sessions.get(&id).cloned().map(|a| (id, a)));
    let (blocks, bytes) = store_size(&p.store);
    Status {
        hook_installed: settings_has_hook(&p.settings),
        mcp_installed: mcp_has_server(&p.mcp_config),
        latest_session: latest,
        total,
        session_count: sessions.len(),
        store_blocks: blocks,
        store_bytes: bytes,
    }
}

/// The session id of the latest COMPRESS event — the closest stand-in for "this
/// session" we can give without being told which one the caller belongs to. We
/// anchor on compresses (not any event) because expand events from the MCP server
/// land under session="mcp" and would otherwise hijack "latest" whenever the model
/// did a recall later than the most recent pack. That made `/knapsack` read
/// "no activity yet" right after a session that obviously had activity. Empty
/// session ids and unparseable lines skip; ties on `t` resolve last-wins so
/// re-runs over the same file give the same answer.
fn latest_session_id(metrics: &Path) -> Option<String> {
    let text = fs::read_to_string(metrics).ok()?;
    let mut best_t = f64::NEG_INFINITY;
    let mut best_id: Option<String> = None;
    for line in text.lines() {
        let v = match json::parse(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        // Only compress events define "this session" — expand-only sessions are
        // recalls, not work product.
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

/// (blocks, bytes) — walks the sharded store and the legacy flat layout (see store.rs).
/// Cheap enough for an interactive command on caches up to ~hundreds of thousands of files.
fn store_size(dir: &Path) -> (usize, u64) {
    let Ok(top) = fs::read_dir(dir) else {
        return (0, 0);
    };
    let mut count = 0usize;
    let mut bytes = 0u64;
    for e in top.flatten() {
        let meta = match e.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        if meta.is_file() {
            count += 1;
            bytes += meta.len();
        } else if meta.is_dir() {
            if let Ok(sub) = fs::read_dir(e.path()) {
                for e2 in sub.flatten() {
                    if let Ok(m2) = e2.metadata() {
                        if m2.is_file() {
                            count += 1;
                            bytes += m2.len();
                        }
                    }
                }
            }
        }
    }
    (count, bytes)
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

fn human_bytes(n: u64) -> String {
    const GB: u64 = 1 << 30;
    const MB: u64 = 1 << 20;
    const KB: u64 = 1 << 10;
    if n >= GB {
        format!("{:.1} GB", n as f64 / GB as f64)
    } else if n >= MB {
        format!("{:.1} MB", n as f64 / MB as f64)
    } else if n >= KB {
        format!("{} KB", n / KB)
    } else {
        format!("{} B", n)
    }
}

fn net_reduction_pct(a: &Agg) -> Option<i64> {
    if a.raw <= 0 {
        return None;
    }
    Some(a.net() * 100 / a.raw)
}

pub fn render(s: &Status) -> String {
    let mut o = String::new();

    // Header. Inactive is the single most important signal — a dashboard that says
    // "active" but isn't actually wired is a worse failure than admitting "inactive".
    // We gate on the on-disk hook config (the source of truth for whether Claude Code
    // will dispatch to us); install state, not session activity.
    if !s.hook_installed {
        o.push_str("Knapsack inactive\n\n");
        o.push_str("  Run `knapsack install` and restart Claude Code to enable.\n");
        return o;
    }
    o.push_str("Knapsack active\n\n");

    // Input + Output reduction lines. Each reads as "active" / "off" — no env-var
    // jargon, no EXPERIMENTAL labels, no dogfood mention. The two paths share the
    // same installed hook; they only diverge if a user has set the off-switch env var
    // OR has MCP missing (in which case output recall would silently fail).
    let input_active = crate::config::read_hook_enabled();
    o.push_str(&format!(
        "Input reduction:  {}\n",
        if input_active { "active" } else { "off" }
    ));
    let output_active = s.mcp_installed;
    o.push_str(&format!(
        "Output reduction: {}\n",
        if output_active { "active" } else { "off (recall not configured)" }
    ));

    // Savings — anchored on the most recent session (the closest stand-in for "this
    // session"). We report GROSS saves on its own line and refetched cost on its own
    // line, with the net reduction percentage as the rolled-up summary. Showing one
    // combined "Session saved: net" number can go negative when the model
    // over-recalls (each model recall recovers bytes, paying back part of what was
    // compressed) — a caveman user reads "-43k tokens saved" as "I lost tokens",
    // which is misleading. Saves and refetches are real, separable things; print
    // both honestly. When there's nothing yet, say so rather than print 0/0/0.
    match &s.latest_session {
        Some((_id, a)) if a.compress_events > 0 => {
            o.push_str(&format!("Session saved:    {} tokens\n", commafy(a.saved)));
            if a.refetched > 0 {
                o.push_str(&format!("Refetched:        {} tokens\n", commafy(a.refetched)));
            }
            match net_reduction_pct(a) {
                Some(pct) => o.push_str(&format!("Net reduction:    {}%\n", pct)),
                None => o.push_str("Net reduction:    n/a\n"),
            }
        }
        _ => {
            o.push_str("Session saved:    no activity yet\n");
            o.push_str("Net reduction:    n/a\n");
        }
    }

    // Recall health. Failed expands are the one thing that should jump out. Latest-
    // session scope keeps old failures from shouting forever; lifetime failures still
    // get a quieter mention in the footer below.
    let session_failed = s.latest_session.as_ref().map(|(_, a)| a.failed_expands).unwrap_or(0);
    if session_failed > 0 {
        o.push_str(&format!("Recall:           ⚠ {} failed (run `knapsack doctor`)\n", session_failed));
    } else if s.latest_session.is_some() {
        o.push_str("Recall:           healthy\n");
    } else {
        o.push_str("Recall:           idle\n");
    }

    if s.store_blocks == 0 {
        o.push_str("Store:            empty\n");
    } else {
        o.push_str(&format!(
            "Store:            {} blocks / {}\n",
            commafy(s.store_blocks as i64),
            human_bytes(s.store_bytes)
        ));
    }

    // Lifetime footer — only when there's more than one session of history, so a single
    // active session isn't double-billed by showing the same number twice.
    //
    // We report GROSS saved (not net) here on purpose. Compress events are tagged with
    // the actual Claude Code session id (from the PreToolUse payload), but expand
    // events from the MCP server are tagged "mcp" (no session id is propagated through
    // the MCP protocol). Summing nets across sessions would therefore include the MCP
    // session's negative net (expands without compresses), making the lifetime appear
    // SMALLER than the current session — which reads as "did I lose tokens?" The user
    // wants to see total benefit; we report saves and refetches as separate items so
    // the math is transparent.
    if s.session_count > 1 && s.total.compress_events > 0 {
        o.push_str(&format!(
            "\nLifetime: {} tokens saved across {} sessions",
            commafy(s.total.saved),
            s.session_count
        ));
        if s.total.refetched > 0 {
            o.push_str(&format!(" · {} refetched on recall", commafy(s.total.refetched)));
        }
        if s.total.failed_expands > 0 {
            o.push_str(&format!(" · {} recall failures total", s.total.failed_expands));
        }
        o.push('\n');
    }

    o
}

pub fn report() -> String {
    render(&collect())
}
