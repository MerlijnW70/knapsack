//! Shared test helper: per-test env sandbox with process-global serialization.
//!
//! Cargo runs tests inside a single test binary in parallel by default. Knapsack
//! reads `KNAPSACK_STORE`, `KNAPSACK_METRICS`, `KNAPSACK_SESSIONS`,
//! `KNAPSACK_READ_CACHE`, `KNAPSACK_READ_LOG`, … as process-global env vars at
//! call time. Two parallel tests setting different paths would clobber each
//! other mid-call; `--test-threads=1` was the only workaround.
//!
//! This helper makes parallel runs safe:
//!   * a single `OnceLock<Mutex<()>>` serializes every test that mutates env;
//!   * an RAII `EnvSandbox` guard creates a unique temp dir, points every
//!     standard `KNAPSACK_*` env var inside it, and restores the prior values
//!     (and removes the temp dir) on drop.
//!
//! Zero-dep policy: this is a hand-rolled `serial_test`-equivalent. Each
//! integration test binary `cargo test` produces is a separate process, so
//! the `OnceLock` is per-binary; sibling binaries can still parallelize.
//!
//! Usage:
//! ```ignore
//! mod common;
//! use common::EnvSandbox;
//!
//! #[test]
//! fn my_test() {
//!     let _sb = EnvSandbox::new("my-test");
//!     // KNAPSACK_STORE etc. now point under _sb.dir(); released on drop.
//! }
//! ```

#![allow(dead_code)] // Each integration test only uses a subset of the helpers.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock};

/// Standard knapsack env vars that point at on-disk state. `EnvSandbox::new`
/// routes every entry under the per-test temp dir.
const SANDBOX_VARS: &[(&str, &str)] = &[
    ("KNAPSACK_STORE", "store"),
    ("KNAPSACK_SESSIONS", "sessions"),
    ("KNAPSACK_METRICS", "metrics.jsonl"),
    ("KNAPSACK_READ_CACHE", "read_cache"),
    ("KNAPSACK_READ_LOG", "read_hook.jsonl"),
];

fn env_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    // PoisonError is fine — a poisoned lock just means a previous test panicked
    // while holding it; EnvSandbox::drop restores env state regardless.
    LOCK.get_or_init(|| Mutex::new(())).lock().unwrap_or_else(|e| e.into_inner())
}

/// RAII sandbox: holds the global env lock for its lifetime, points all
/// standard `KNAPSACK_*` env vars at a unique temp dir, and restores both env
/// and disk state on drop.
pub struct EnvSandbox {
    _lock: MutexGuard<'static, ()>,
    dir: PathBuf,
    restore: Vec<(&'static str, Option<OsString>)>,
}

impl EnvSandbox {
    /// Acquire the env lock, allocate a tagged temp dir, and route every
    /// standard knapsack env var at a unique path inside it.
    pub fn new(tag: &str) -> Self {
        let lock = env_lock();
        let t = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "knapsack-test-{}-{}-{}",
            sanitize_tag(tag),
            std::process::id(),
            t
        ));
        std::fs::create_dir_all(&dir).unwrap();

        let mut restore = Vec::with_capacity(SANDBOX_VARS.len());
        for (key, rel) in SANDBOX_VARS {
            restore.push((*key, std::env::var_os(key)));
            std::env::set_var(key, dir.join(rel));
        }

        Self { _lock: lock, dir, restore }
    }

    /// The temp directory backing this sandbox. Tests can read/write here
    /// directly; everything under it is removed on drop.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Override an additional env var (beyond the standard `KNAPSACK_*` set
    /// `new` already handles — e.g. `KNAPSACK_SETTINGS`, `KNAPSACK_MCP_CONFIG`,
    /// `KNAPSACK_READ_HOOK`). The prior value is snapshotted on first call and
    /// restored on drop.
    pub fn set<K: AsRef<OsStr>>(&mut self, key: &'static str, value: K) {
        if !self.restore.iter().any(|(k, _)| *k == key) {
            self.restore.push((key, std::env::var_os(key)));
        }
        std::env::set_var(key, value);
    }

    /// Unset an env var for the lifetime of the sandbox. Symmetric with `set`.
    pub fn unset(&mut self, key: &'static str) {
        if !self.restore.iter().any(|(k, _)| *k == key) {
            self.restore.push((key, std::env::var_os(key)));
        }
        std::env::remove_var(key);
    }

    /// Join the sandbox dir with a relative path — for tests that need a
    /// per-test scratch file alongside the standard env-routed paths.
    pub fn join(&self, rel: impl AsRef<Path>) -> PathBuf {
        self.dir.join(rel)
    }
}

impl Drop for EnvSandbox {
    fn drop(&mut self) {
        for (k, prior) in self.restore.drain(..) {
            match prior {
                Some(v) => std::env::set_var(k, v),
                None => std::env::remove_var(k),
            }
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn sanitize_tag(tag: &str) -> String {
    tag.chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' })
        .collect()
}
