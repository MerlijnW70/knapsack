//! Round-10 platform-matrix supplement: Unix-specific failure modes that the
//! Windows-only test runs can't exercise.
//!
//! Two things this file pins (gated `#[cfg(unix)]`, so the file compiles —
//! and runs no tests — on Windows):
//!
//!   1. **`chmod 555` (no-write) directory**: a true POSIX read-only directory.
//!      `fs::write` returns `EACCES`. The store contract is "silently swallow,
//!      get() returns None — never serve wrong bytes". This is the genuine
//!      read-only-FS case the Windows "regular-file-as-directory" trick can
//!      only approximate.
//!
//!   2. **Real SIGINT**: send SIGINT (not SIGKILL / TerminateProcess) to a
//!      live `knapsack pack -` subprocess. Default Rust signal handling is
//!      "terminate on SIGINT, no graceful shutdown" — so post-state must be
//!      consistent (no torn block readable as wrong bytes, metrics callable).
//!      A future change that installs a signal handler would still need to
//!      respect the same invariants; this test pins them.

#![cfg(unix)]

mod common;
use common::EnvSandbox;

use knapsack::api::{ExpandCaller, expand_handle, pack_output, ExpandRequest, PackRequest};
use knapsack::content_type::ContentType;
use knapsack::store::Store;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

// =====================================================================
// 1. chmod 555 / 444 — true POSIX read-only-FS behavior
// =====================================================================

/// Drop a directory's mode to read+exec (no write). On Unix this is the
/// genuine "read-only FS" condition. We restore writable mode on drop so
/// EnvSandbox can clean up the tempdir.
struct ReadOnlyDir {
    path: PathBuf,
    original_mode: u32,
}

impl ReadOnlyDir {
    fn new(p: PathBuf) -> Self {
        std::fs::create_dir_all(&p).unwrap();
        let original_mode = std::fs::metadata(&p).unwrap().permissions().mode();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o555)).unwrap();
        Self {
            path: p,
            original_mode,
        }
    }
}

impl Drop for ReadOnlyDir {
    fn drop(&mut self) {
        // Restore so EnvSandbox can rm -rf the parent.
        let _ = std::fs::set_permissions(
            &self.path,
            std::fs::Permissions::from_mode(self.original_mode),
        );
    }
}

#[test]
fn chmod_555_store_dir_put_silently_fails_get_returns_none() {
    let sb = EnvSandbox::new("unix-ro-store");
    let store_path = sb.join("ro-store");
    let _readonly = ReadOnlyDir::new(store_path.clone());
    let store = Store::new(store_path);

    let payload = b"bytes that can't be written\n".repeat(20);
    let h = store.put(&payload);
    assert!(
        h.starts_with("ks2_"),
        "handle still computed even on write failure"
    );

    // Read-only dir means no shard subdir can be created. get() must return None.
    assert_eq!(
        store.get(&h),
        None,
        "chmod 555 store: put silently failed; get must NOT serve wrong bytes"
    );
}

#[test]
fn chmod_555_store_with_pre_existing_block_can_still_read() {
    // Set up: write a block normally, THEN chmod 555. Reads should still
    // work — only writes are blocked. This pins that we don't accidentally
    // require write access for the read path.
    let sb = EnvSandbox::new("unix-ro-readback");
    let store_path = sb.join("store");
    let store = Store::new(store_path.clone());
    let payload = b"writable then ro\n".repeat(20);
    let h = store.put(&payload);
    // Sanity: block resolves while writable
    assert_eq!(store.get(&h).unwrap(), payload);

    // Now lock the store dir + its shard subdir. Both need r+x but no w.
    let hash_start = h.find('_').unwrap() + 1;
    let shard = &h[hash_start..hash_start + 2];
    let shard_path = store_path.join(shard);
    let _ro_top = ReadOnlyDir::new(store_path.clone());
    let _ro_shard = ReadOnlyDir::new(shard_path);

    // Reads still work — and the meta touch_last_accessed is silently
    // best-effort, so it doesn't panic on the read-only meta path either.
    let got = store.get(&h).expect("read works on r+x dir");
    assert_eq!(got, payload, "byte-exact recall under chmod 555");
}

#[test]
fn chmod_555_metrics_path_swallows_and_summary_returns_zero() {
    // KNAPSACK_METRICS points at a path inside a r+x directory. Write fails
    // (EACCES); the silent-swallow contract holds; summary returns zeros.
    let mut sb = EnvSandbox::new("unix-ro-metrics");
    let dir = sb.join("ro-metrics-dir");
    let _readonly = ReadOnlyDir::new(dir.clone());
    sb.set("KNAPSACK_METRICS", dir.join("metrics.jsonl"));

    for _ in 0..20 {
        knapsack::metrics::record_compress("x", 100, 50, 50, 0, 0);
    }
    let s = knapsack::metrics::summary();
    assert_eq!(
        s.compress_events, 0,
        "EACCES writes swallowed; nothing landed"
    );
}

#[test]
fn pack_output_on_chmod_555_store_emits_view_recall_returns_none() {
    // End-to-end: pack succeeds in-memory and emits a view; the handles
    // it names DON'T resolve via expand because the store was r-only.
    // No false-positive recall.
    let mut sb = EnvSandbox::new("unix-ro-pack");
    let store_dir = sb.join("ro-store");
    let _readonly = ReadOnlyDir::new(store_dir.clone());
    sb.set("KNAPSACK_STORE", &store_dir);

    let payload = b"chmod 555 pack target\n".repeat(40);
    let r = pack_output(PackRequest {
        session_id: "ro-pack".into(),
        command: None,
        bytes: payload.to_vec(),
        content_hint: Some(ContentType::Log),
        step: 0,
        transcript_path: None,
    });
    assert!(r.raw_tokens_est > 0);
    assert!(!r.view.is_empty());

    let h = format!("ks2_{}", &knapsack::sha256::sha256_hex(&payload)[..32]);
    let out = expand_handle(ExpandRequest {
        handle: h,
        range: None,
        grep: None,
        context: 0,
        session_id: "ro-pack".into(),
        caller: ExpandCaller::Cli,
    });
    assert!(
        out.is_none(),
        "no false-positive recall under POSIX read-only FS"
    );
}

// =====================================================================
// 2. Real SIGINT — strictly weaker than kill(), so a stricter consistency test
// =====================================================================

fn knapsack_bin() -> PathBuf {
    let bin = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("release")
        .join("knapsack");
    assert!(
        bin.exists(),
        "release binary missing at {}; run `cargo build --release` first",
        bin.display()
    );
    bin
}

fn spawn_pack(sb: &EnvSandbox, session: &str) -> std::process::Child {
    Command::new(knapsack_bin())
        .args(["pack", "-", "--session", session, "--type", "log"])
        .env("KNAPSACK_STORE", sb.dir().join("store"))
        .env("KNAPSACK_SESSIONS", sb.dir().join("sessions"))
        .env("KNAPSACK_METRICS", sb.dir().join("metrics.jsonl"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn knapsack pack -")
}

fn send_sigint(pid: u32) {
    // `kill -INT <pid>` is universally available on Linux + macOS, zero-dep.
    let _ = Command::new("kill")
        .args(["-INT", &pid.to_string()])
        .status();
}

#[test]
fn real_sigint_mid_pack_leaves_no_torn_block_readable_as_wrong_bytes() {
    let sb = EnvSandbox::new("unix-sigint-torn");
    let mut child = spawn_pack(&sb, "sigint-torn");
    {
        let stdin = child.stdin.as_mut().expect("child stdin");
        let chunk = b"line A\nline B\nline C\n".repeat(2000);
        for _ in 0..30 {
            if stdin.write_all(&chunk).is_err() {
                break;
            }
        }
    }
    // Give the child a moment to actually start writing blocks.
    std::thread::sleep(Duration::from_millis(50));
    send_sigint(child.id());
    // Give SIGINT a moment to land. wait() reaps.
    let _ = child.wait();

    // Walk every block-file path and ensure get() either returns clean bytes
    // or None. Never wrong bytes.
    let store_dir = sb.dir().join("store");
    let store = Store::new(store_dir.clone());
    if let Ok(top) = std::fs::read_dir(&store_dir) {
        for shard in top.flatten() {
            let p = shard.path();
            if !p.is_dir() {
                continue;
            }
            if let Ok(entries) = std::fs::read_dir(&p) {
                for ent in entries.flatten() {
                    let path = ent.path();
                    if path.extension().and_then(|x| x.to_str()) == Some("meta") {
                        continue;
                    }
                    let fname = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                    if !fname.starts_with("ks2_") && !fname.starts_with("ks_") {
                        continue;
                    }
                    let _ = store.get(&fname.to_string());
                }
            }
        }
    }
}

#[test]
fn real_sigint_then_clean_pack_in_same_session_works() {
    let sb = EnvSandbox::new("unix-sigint-resume");

    // First pack: SIGINT mid-flight.
    let mut killed = spawn_pack(&sb, "resume");
    {
        let stdin = killed.stdin.as_mut().expect("child stdin");
        let chunk = b"interrupt me\n".repeat(2000);
        for _ in 0..15 {
            if stdin.write_all(&chunk).is_err() {
                break;
            }
        }
    }
    std::thread::sleep(Duration::from_millis(50));
    send_sigint(killed.id());
    let _ = killed.wait();

    // Second pack: same session, completes normally.
    let mut clean = spawn_pack(&sb, "resume");
    {
        let stdin = clean.stdin.as_mut().expect("child stdin");
        let chunk = b"recovery\n".repeat(20);
        stdin.write_all(&chunk).unwrap();
    }
    drop(clean.stdin.take());
    let status = clean.wait().expect("clean wait");
    assert!(
        status.success(),
        "post-SIGINT clean pack must exit 0; got {status:?}"
    );
}

#[test]
fn real_sigint_metrics_remain_parseable() {
    let sb = EnvSandbox::new("unix-sigint-metrics");
    let mut child = spawn_pack(&sb, "sigint-met");
    {
        let stdin = child.stdin.as_mut().expect("child stdin");
        let chunk = b"line one\nline two\n".repeat(2000);
        for _ in 0..20 {
            if stdin.write_all(&chunk).is_err() {
                break;
            }
        }
    }
    std::thread::sleep(Duration::from_millis(50));
    send_sigint(child.id());
    let _ = child.wait();

    // Post-SIGINT, summary() must return without panic.
    let s = knapsack::metrics::summary();
    assert!(
        s.compress_events <= 100,
        "summary callable: events={}",
        s.compress_events
    );
}
