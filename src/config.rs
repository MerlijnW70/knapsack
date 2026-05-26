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

/// Maximum length of the SANITIZED session basename before we truncate and
/// append a hash suffix for collision-resistance. Tuned for Windows MAX_PATH
/// (260 chars) with comfortable headroom for a parent path like
/// `C:\Users\Name\.knapsack\sessions\` (~50 chars) plus the `.tsv` extension.
///
/// Going over this used to silently fail: ledger::save did `let _ = fs::write(...)`
/// so a too-long filename returned Err which was swallowed, every subsequent pack
/// in that session started cold without warning. Capping at safe length means
/// the file ALWAYS writes, and uniqueness is preserved via a 16-hex SHA-1 tail
/// when the original was longer.
const MAX_SESSION_BASENAME: usize = 128;

pub fn session_path(id: &str) -> PathBuf {
    // Defensive: empty (or whitespace-only) session ID used to land at
    // `sessions/.tsv` (a hidden zero-basename file). The CLI rejects this
    // loudly, but internal callers may still pass an empty id (e.g. an
    // event payload missing session_id), so we centralize the fallback here.
    let trimmed = id.trim();
    let effective = if trimmed.is_empty() { "default" } else { id };

    let mut safe: String = effective
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' })
        .collect();

    if safe.len() > MAX_SESSION_BASENAME {
        // Reserve 17 bytes for "_<16 hex SHA-1>" tail so two long IDs with the
        // same first-128-byte prefix get distinct paths. SHA-1 here is for
        // uniqueness only (zero-dep, already in tree); not security-critical.
        const TAIL_LEN: usize = 17; // 1 underscore + 16 hex
        let keep = MAX_SESSION_BASENAME - TAIL_LEN;
        let suffix = &crate::hash::sha1_hex(effective.as_bytes())[..16];
        // Truncate by BYTES (safe is ASCII after sanitize), not chars.
        safe.truncate(keep);
        safe.push('_');
        safe.push_str(suffix);
    }

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

    // ---------- session_path: the user-found bugs ----------

    fn basename(p: &PathBuf) -> String {
        p.file_name().unwrap().to_string_lossy().into_owned()
    }

    #[test]
    fn session_path_empty_id_falls_back_to_default_not_hidden_dotfile() {
        // Pre-fix: empty id -> `.tsv` (hidden zero-basename file). On unix `ls`
        // without -a wouldn't even show it; on Windows it polluted the sessions
        // dir with an unintuitive name. We now route through the "default" tag.
        let p = session_path("");
        assert_eq!(basename(&p), "default.tsv", "empty id must map to default.tsv, got {}", p.display());
    }

    #[test]
    fn session_path_whitespace_id_falls_back_to_default() {
        // A tab / space / newline alone is the same shape of mistake as empty.
        for ws in [" ", "  ", "\t", "\n", " \t \n "] {
            let p = session_path(ws);
            assert_eq!(
                basename(&p), "default.tsv",
                "whitespace-only id {:?} must map to default.tsv, got {}", ws, p.display()
            );
        }
    }

    #[test]
    fn session_path_short_id_unchanged() {
        // Regression: the fix must NOT alter short legitimate session IDs.
        let p = session_path("my-session-42");
        assert_eq!(basename(&p), "my-session-42.tsv");
    }

    #[test]
    fn session_path_long_id_is_truncated_with_hash_suffix() {
        // Pre-fix: a 500-char id produced a 500-char filename that overflowed
        // Windows MAX_PATH; fs::write silently failed in ledger::save and the
        // session ledger never persisted. Cap at MAX_SESSION_BASENAME + ".tsv"
        // so the write ALWAYS succeeds; preserve uniqueness via SHA-1 tail.
        let long: String = "a".repeat(500);
        let p = session_path(&long);
        let name = basename(&p);
        // Filename = MAX_SESSION_BASENAME chars + ".tsv" = 132 chars
        assert_eq!(name.len(), MAX_SESSION_BASENAME + 4, "long id filename should be capped, got {} chars", name.len());
        // Should END in "_<16 hex>.tsv"
        assert!(
            name[name.len() - 21..name.len() - 4].starts_with('_'),
            "tail must be '_<hex>.tsv': {}", name
        );
        // The hash tail must be 16 hex chars
        let hex_tail = &name[name.len() - 20..name.len() - 4];
        assert!(
            hex_tail.chars().all(|c| c.is_ascii_hexdigit()),
            "tail must be hex: {}", hex_tail
        );
    }

    #[test]
    fn session_path_long_ids_with_same_prefix_get_distinct_files() {
        // Two 500-char IDs that differ only at position 200 (well past the
        // truncation point) must produce DIFFERENT files. Without the hash
        // suffix, they'd both truncate to identical prefixes and collide,
        // silently merging two users' / two sessions' ledgers.
        let id_a = format!("{}{}", "a".repeat(200), "x".repeat(300));
        let id_b = format!("{}{}", "a".repeat(200), "y".repeat(300));
        assert_ne!(
            session_path(&id_a), session_path(&id_b),
            "two long IDs with identical 200-char prefix must hash-disambiguate"
        );
    }

    #[test]
    fn session_path_idempotent_within_a_call() {
        // Same input -> same path, always. The hash tail must be deterministic
        // (sha1_hex is); a fresh seed would silently move the ledger and lose
        // session continuity across calls.
        let id = "x".repeat(500);
        assert_eq!(session_path(&id), session_path(&id));
    }

    // ---------- read-hook env gate (existing) ----------

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
