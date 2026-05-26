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

/// Read-hook cache directory. Compressed views of source files keyed by content SHA-256
/// live here. Read-hook is on by default; set `KNAPSACK_READ_HOOK=0` to disable.
/// Env-overridable for tests (KNAPSACK_READ_CACHE).
pub fn read_cache_dir() -> PathBuf {
    env::var_os("KNAPSACK_READ_CACHE").map(PathBuf::from).unwrap_or_else(|| base().join("read_cache"))
}

/// Structured decision log for the Read hook. One JSONL line per Read PreToolUse
/// invocation: what we decided and why. Append-only; bounded only by `knapsack gc`.
pub fn read_log_path() -> PathBuf {
    env::var_os("KNAPSACK_READ_LOG").map(PathBuf::from).unwrap_or_else(|| base().join("read_hook.jsonl"))
}

/// True iff the Read hook is enabled for this process. Single source of truth so the
/// hook dispatch, the cache view header, `knapsack status`, and tests all agree.
///
/// Default: ON. The env var is an OFF-switch — set `KNAPSACK_READ_HOOK=0` (or `off` /
/// `false` / empty) to disable. Anything else, including unset, leaves it on. This
/// matches the product position: after `knapsack install`, input + output reduction
/// are both active with no further configuration; the env var stays as an emergency
/// off-switch and is the only thing tests need to toggle.
pub fn read_hook_enabled() -> bool {
    match env::var("KNAPSACK_READ_HOOK") {
        Err(_) => true, // unset -> default ON
        Ok(v) => {
            let s = v.trim().to_ascii_lowercase();
            !matches!(s.as_str(), "0" | "off" | "false" | "no" | "")
        }
    }
}

#[cfg(test)]
mod tests {
    // The read-hook gate has a tight, documented decision tree. These tests pin every
    // branch directly (without going through the shell or Claude Code), so the off-
    // switch contract can't drift silently. Tests are #[serial]-style via a Mutex
    // because they mutate the process-global env var — zero-dep policy forbids
    // `serial_test`, so we DIY the same pattern as tests/read_hook.rs.
    use super::*;
    use std::sync::{Mutex, MutexGuard, OnceLock};

    fn env_lock() -> MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(())).lock().unwrap_or_else(|e| e.into_inner())
    }

    struct Guard {
        _lock: MutexGuard<'static, ()>,
        prior: Option<std::ffi::OsString>,
    }
    impl Guard {
        fn with(value: Option<&str>) -> Self {
            let lock = env_lock();
            let prior = env::var_os("KNAPSACK_READ_HOOK");
            match value {
                Some(v) => env::set_var("KNAPSACK_READ_HOOK", v),
                None => env::remove_var("KNAPSACK_READ_HOOK"),
            }
            Self { _lock: lock, prior }
        }
    }
    impl Drop for Guard {
        fn drop(&mut self) {
            match self.prior.take() {
                Some(v) => env::set_var("KNAPSACK_READ_HOOK", v),
                None => env::remove_var("KNAPSACK_READ_HOOK"),
            }
        }
    }

    #[test]
    fn default_on_when_unset() {
        let _g = Guard::with(None);
        assert!(read_hook_enabled(), "unset env var -> ON (default)");
    }

    #[test]
    fn off_when_explicit_off_switch() {
        for v in ["0", "off", "false", "no", "OFF", "False", "No", " 0 ", "\toff\t"] {
            let _g = Guard::with(Some(v));
            assert!(!read_hook_enabled(), "value {:?} should disable", v);
        }
    }

    #[test]
    fn off_when_empty_string() {
        // On Unix, `VAR=""` is a real value and the off-switch should fire. The bash
        // shell on Windows turns this into "unset" so the dogfood matrix can't test
        // it directly — this Rust test does.
        let _g = Guard::with(Some(""));
        assert!(!read_hook_enabled(), "empty string -> OFF (treated as cleared)");
    }

    #[test]
    fn on_for_anything_else() {
        for v in ["1", "yes", "on", "true", "abc", "anything"] {
            let _g = Guard::with(Some(v));
            assert!(read_hook_enabled(), "value {:?} should leave it on", v);
        }
    }
}
