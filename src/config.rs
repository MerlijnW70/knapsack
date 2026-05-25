//! Paths. Store + sessions (the ledger) + metrics live under ~/.knapsack (each path
//! overridable via env), so the engine is stateful ACROSS process invocations — each
//! Claude tool call is a separate `knapsack` run, and residency/recall must survive between
//! them. Env overrides: KNAPSACK_STORE, KNAPSACK_SESSIONS, KNAPSACK_METRICS.

use std::env;
use std::path::PathBuf;

pub fn home() -> PathBuf {
    env::var_os("USERPROFILE")
        .or_else(|| env::var_os("HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn base() -> PathBuf {
    home().join(".knapsack")
}

pub fn store_dir() -> PathBuf {
    env::var_os("KNAPSACK_STORE").map(PathBuf::from).unwrap_or_else(|| base().join("store"))
}

pub fn metrics_path() -> PathBuf {
    env::var_os("KNAPSACK_METRICS").map(PathBuf::from).unwrap_or_else(|| base().join("metrics.jsonl"))
}

/// Conservative resident-token budget. When a session's resident set exceeds this, the
/// oldest spans are evicted so back-references never point past the context window.
pub fn resident_budget() -> usize {
    env::var("KNAPSACK_RESIDENT_BUDGET").ok().and_then(|s| s.parse().ok()).unwrap_or(120_000)
}

pub fn sessions_dir() -> PathBuf {
    env::var_os("KNAPSACK_SESSIONS").map(PathBuf::from).unwrap_or_else(|| base().join("sessions"))
}

pub fn session_path(id: &str) -> PathBuf {
    let safe: String = id
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' })
        .collect();
    sessions_dir().join(format!("{}.tsv", safe))
}
