//! EXPERIMENTAL Read-hook decision tests.
//!
//! The hook is gated behind `KNAPSACK_READ_HOOK=1` and default-off. We pass the gate
//! state in explicitly (`decide_with_gate`) so tests don't race on a process-wide env
//! var. Cache dir is also redirected via the env override so tests never touch the
//! user's real ~/.knapsack/read_cache.
//!
//! Env-var hygiene: `read_cache_dir()` and `read_log_path()` read process-global
//! env vars at call time. Cargo runs integration tests in parallel by default, so
//! every test that sets `KNAPSACK_READ_CACHE` / `KNAPSACK_READ_LOG` MUST take the
//! `ENV_LOCK` guard for the duration of the env override. Without it, sibling tests
//! clobber each other's cache dir mid-call and you get cross-test path comparisons.

use knapsack::json::Json;
use knapsack::read_hook::{decide_with_gate, ReadDecision};
use knapsack::why_log::{self, Reason};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock};

fn env_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    // PoisonError is fine here — a poisoned lock just means a previous test panicked
    // while holding it; the env state is restored by EnvGuard's Drop either way.
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

/// RAII guard that locks the env mutex and restores the prior values of the read-hook
/// env vars on drop. Holding this for the duration of a `decide_with_gate` call is
/// what makes the env overrides race-free. KNAPSACK_STORE is included because the
/// read hook now writes elision + whole-file blocks into the store (so the model's
/// recall instructions resolve byte-exact); without isolating the store, parallel
/// tests would scribble into each other's data — AND into the user's real store
/// when run outside a sandbox.
struct EnvGuard {
    _lock: MutexGuard<'static, ()>,
    prior_cache: Option<std::ffi::OsString>,
    prior_log: Option<std::ffi::OsString>,
    prior_store: Option<std::ffi::OsString>,
}

impl EnvGuard {
    fn new() -> Self {
        let lock = env_lock();
        Self {
            _lock: lock,
            prior_cache: std::env::var_os("KNAPSACK_READ_CACHE"),
            prior_log: std::env::var_os("KNAPSACK_READ_LOG"),
            prior_store: std::env::var_os("KNAPSACK_STORE"),
        }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        match self.prior_cache.take() {
            Some(v) => std::env::set_var("KNAPSACK_READ_CACHE", v),
            None => std::env::remove_var("KNAPSACK_READ_CACHE"),
        }
        match self.prior_log.take() {
            Some(v) => std::env::set_var("KNAPSACK_READ_LOG", v),
            None => std::env::remove_var("KNAPSACK_READ_LOG"),
        }
        match self.prior_store.take() {
            Some(v) => std::env::set_var("KNAPSACK_STORE", v),
            None => std::env::remove_var("KNAPSACK_STORE"),
        }
    }
}

fn tmp(tag: &str) -> PathBuf {
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let d = std::env::temp_dir().join(format!(
        "knapsack-readhook-{}-{}-{}",
        tag,
        std::process::id(),
        t
    ));
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn make_event(file_path: &str, extra_fields: &[(&str, Json)]) -> Json {
    let mut tool_input: Vec<(String, Json)> =
        vec![("file_path".into(), Json::Str(file_path.into()))];
    for (k, v) in extra_fields {
        tool_input.push((k.to_string(), v.clone()));
    }
    Json::Obj(vec![
        ("tool_name".into(), Json::Str("Read".into())),
        ("tool_input".into(), Json::Obj(tool_input)),
    ])
}

fn unwrap_pass(d: ReadDecision) -> Reason {
    match d {
        ReadDecision::PassThrough { log } => log.reason,
        ReadDecision::Redirect { log, .. } => {
            panic!("expected PassThrough; got Redirect({:?})", log.reason)
        }
    }
}

fn unwrap_redirect(d: ReadDecision) -> (PathBuf, Reason) {
    match d {
        ReadDecision::Redirect { redirect_to, log } => (redirect_to, log.reason),
        ReadDecision::PassThrough { log } => {
            panic!("expected Redirect; got PassThrough({:?})", log.reason)
        }
    }
}

/// Build a "small but compressible" file: lots of similar lines. Tuned so it's big
/// enough to clear `REDIRECT_MIN_BYTES` (8 KB) and the structural log compressor
/// produces a meaningfully shorter view.
fn big_compressible_file(dir: &Path, name: &str) -> PathBuf {
    let p = dir.join(name);
    let mut f = std::fs::File::create(&p).unwrap();
    for i in 0..500 {
        writeln!(f, "[INFO] step {i}: routine work that compresses well; lots of similar lines for the structural log compressor to find a stable middle to elide").unwrap();
    }
    p
}

#[test]
fn gate_disabled_always_passes_through_with_clear_reason() {
    let _env = EnvGuard::new();
    let dir = tmp("gate");
    let src = big_compressible_file(&dir, "src.txt");
    std::env::set_var("KNAPSACK_READ_CACHE", dir.join("cache"));
    std::env::set_var("KNAPSACK_STORE", dir.join("store"));
    let evt = make_event(src.to_str().unwrap(), &[]);
    let reason = unwrap_pass(decide_with_gate(false, &evt));
    assert_eq!(reason, Reason::GateDisabled);
    // Cache dir is never even created when the gate is off.
    assert!(
        !dir.join("cache").exists(),
        "gate off must not create cache dir"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn too_small_file_passes_through() {
    let _env = EnvGuard::new();
    let dir = tmp("small");
    let src = dir.join("tiny.txt");
    std::fs::write(&src, b"just a tiny file").unwrap();
    std::env::set_var("KNAPSACK_READ_CACHE", dir.join("cache"));
    std::env::set_var("KNAPSACK_STORE", dir.join("store"));
    let evt = make_event(src.to_str().unwrap(), &[]);
    assert_eq!(unwrap_pass(decide_with_gate(true, &evt)), Reason::TooSmall);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn too_large_file_passes_through() {
    let _env = EnvGuard::new();
    let dir = tmp("large");
    let src = dir.join("huge.txt");
    // 5 MB is over the 4 MB ceiling.
    let buf = vec![b'x'; 5 * 1024 * 1024];
    std::fs::write(&src, &buf).unwrap();
    std::env::set_var("KNAPSACK_READ_CACHE", dir.join("cache"));
    std::env::set_var("KNAPSACK_STORE", dir.join("store"));
    let evt = make_event(src.to_str().unwrap(), &[]);
    assert_eq!(unwrap_pass(decide_with_gate(true, &evt)), Reason::TooLarge);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn missing_file_path_is_bad_input() {
    let _env = EnvGuard::new();
    let dir = tmp("badinput");
    std::env::set_var("KNAPSACK_READ_CACHE", dir.join("cache"));
    std::env::set_var("KNAPSACK_STORE", dir.join("store"));
    // tool_input present but file_path absent.
    let evt = Json::Obj(vec![
        ("tool_name".into(), Json::Str("Read".into())),
        ("tool_input".into(), Json::Obj(vec![])),
    ]);
    assert_eq!(unwrap_pass(decide_with_gate(true, &evt)), Reason::BadInput);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn slicing_offset_or_limit_passes_through() {
    let _env = EnvGuard::new();
    let dir = tmp("slice");
    let src = big_compressible_file(&dir, "src.txt");
    std::env::set_var("KNAPSACK_READ_CACHE", dir.join("cache"));
    std::env::set_var("KNAPSACK_STORE", dir.join("store"));

    let evt_offset = make_event(src.to_str().unwrap(), &[("offset", Json::Num(100.0))]);
    assert_eq!(
        unwrap_pass(decide_with_gate(true, &evt_offset)),
        Reason::SlicingRequested
    );

    let evt_limit = make_event(src.to_str().unwrap(), &[("limit", Json::Num(50.0))]);
    assert_eq!(
        unwrap_pass(decide_with_gate(true, &evt_limit)),
        Reason::SlicingRequested
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn unreadable_file_passes_through() {
    let _env = EnvGuard::new();
    let dir = tmp("unread");
    std::env::set_var("KNAPSACK_READ_CACHE", dir.join("cache"));
    std::env::set_var("KNAPSACK_STORE", dir.join("store"));
    let evt = make_event(dir.join("does-not-exist.txt").to_str().unwrap(), &[]);
    assert_eq!(
        unwrap_pass(decide_with_gate(true, &evt)),
        Reason::FileUnreadable
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn worse_than_raw_passes_through() {
    // A file that the structural compressor literally cannot compress: a .js file made
    // entirely of distinct function signatures with no body runs. The code compressor
    // only elides body runs (>= MIN_RUN consecutive non-structural lines); with zero
    // body lines, the compact view equals the raw input → reduction ~0% → worse-than-
    // raw triggers and we refuse the redirect.
    let dir = tmp("worse");
    let src = dir.join("unique.js"); // .js -> ContentType::Code (no content sniff needed)
    let mut f = std::fs::File::create(&src).unwrap();
    for i in 0..600 {
        // Each line is a complete one-line method signature. `is_method` matches all of
        // them, so structural::compress_code keeps them all verbatim — zero elision.
        writeln!(f, "function handler{i}() {{ return {i}; }}").unwrap();
    }
    drop(f);
    assert!(
        std::fs::metadata(&src).unwrap().len() > 8 * 1024,
        "fixture must clear the too-small bar"
    );

    let _env = EnvGuard::new();
    std::env::set_var("KNAPSACK_READ_CACHE", dir.join("cache"));
    std::env::set_var("KNAPSACK_STORE", dir.join("store"));
    let evt = make_event(src.to_str().unwrap(), &[]);
    let reason = unwrap_pass(decide_with_gate(true, &evt));
    assert_eq!(
        reason,
        Reason::WorseThanRaw,
        "non-compressible file must refuse the redirect with a clear reason"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn redirect_emitted_writes_cache_and_returns_path() {
    // The happy path: gate on, file in size band, compresses well. We get a Redirect,
    // and the cache file exists at the returned path.
    let _env = EnvGuard::new();
    let dir = tmp("redirect");
    let src = big_compressible_file(&dir, "src.txt");
    let cache = dir.join("cache");
    std::env::set_var("KNAPSACK_READ_CACHE", &cache);
    std::env::set_var("KNAPSACK_STORE", dir.join("store"));

    let evt = make_event(src.to_str().unwrap(), &[]);
    let (redirect_to, reason) = unwrap_redirect(decide_with_gate(true, &evt));
    assert_eq!(reason, Reason::RedirectEmitted);
    assert!(
        redirect_to.exists(),
        "cache file should exist after a successful decide"
    );
    assert!(
        redirect_to.starts_with(&cache),
        "redirect must point inside the configured cache dir"
    );

    // Header UX: original path + recall instructions, both clearly stated.
    let view = std::fs::read_to_string(&redirect_to).unwrap();
    assert!(
        view.contains("Knapsack read cache"),
        "header banner present:\n{}",
        view
    );
    assert!(
        view.contains(src.to_string_lossy().as_ref()),
        "header names the ORIGINAL path"
    );
    assert!(
        view.contains("knapsack expand ks2_"),
        "header surfaces the recall command"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn cache_hit_on_unchanged_source_does_not_rewrite_view() {
    // Re-deciding for the same source should reuse the existing cache file — the
    // file's bytes must not change (proves we didn't regenerate unnecessarily) AND
    // the why-log reason must flip from RedirectEmitted (fresh) to CacheHit (warm).
    // Before the cache-existence-captured-before-write fix, every first read was
    // mislabelled with note=cache-hit because the existence check ran AFTER our
    // own write step; now the reason itself distinguishes the two cases.
    let _env = EnvGuard::new();
    let dir = tmp("cachehit");
    let src = big_compressible_file(&dir, "src.txt");
    std::env::set_var("KNAPSACK_READ_CACHE", dir.join("cache"));
    std::env::set_var("KNAPSACK_STORE", dir.join("store"));

    let evt = make_event(src.to_str().unwrap(), &[]);
    let (redirect_to_1, reason_1) = unwrap_redirect(decide_with_gate(true, &evt));
    assert_eq!(
        reason_1,
        Reason::RedirectEmitted,
        "first read of a file is a fresh redirect"
    );
    let bytes_1 = std::fs::read(&redirect_to_1).unwrap();
    let mtime_1 = std::fs::metadata(&redirect_to_1)
        .unwrap()
        .modified()
        .unwrap();

    let (redirect_to_2, reason_2) = unwrap_redirect(decide_with_gate(true, &evt));
    assert_eq!(
        reason_2,
        Reason::CacheHit,
        "second read of the same file is a cache hit"
    );
    assert_eq!(
        redirect_to_1, redirect_to_2,
        "same source -> same cache path"
    );
    let bytes_2 = std::fs::read(&redirect_to_2).unwrap();
    assert_eq!(bytes_1, bytes_2, "cache contents unchanged on a re-hit");
    let mtime_2 = std::fs::metadata(&redirect_to_2)
        .unwrap()
        .modified()
        .unwrap();
    // Note: mtime equality is OS-dependent at sub-second resolution; we don't assert
    // equality, just that the cache file still exists and the bytes match.
    let _ = (mtime_1, mtime_2);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn fresh_redirect_is_not_mislabelled_as_cache_hit() {
    // Direct pin for the bug surfaced in input-reduction dogfooding: before the fix
    // in `decide_with_gate`, `cache_note` was computed as `if cache_path.exists()`
    // AFTER step 7 wrote the cache file, so every first read for a file got the
    // wrong label (note=cache-hit) and a user running `knapsack why-last` would see
    // "cache-hit" on a file that had never been seen before. The fix captures the
    // pre-write existence flag and chooses Reason::CacheHit vs RedirectEmitted from
    // THAT — so the very first decide on a brand-new file MUST come back as
    // RedirectEmitted with no `cache-hit` note hidden anywhere.
    let _env = EnvGuard::new();
    let dir = tmp("freshlabel");
    let src = big_compressible_file(&dir, "src.txt");
    std::env::set_var("KNAPSACK_READ_CACHE", dir.join("cache"));
    std::env::set_var("KNAPSACK_STORE", dir.join("store"));
    std::env::set_var("KNAPSACK_READ_LOG", dir.join("read_hook.jsonl"));

    let evt = make_event(src.to_str().unwrap(), &[]);
    // Drive through `apply` so the why-log line lands on disk exactly the way it
    // would in production — this is the surface a real user sees via `why-last`.
    knapsack::read_hook::apply(&evt, decide_with_gate(true, &evt));
    let log_line = std::fs::read_to_string(dir.join("read_hook.jsonl")).unwrap();
    assert!(
        log_line.contains("\"reason\":\"redirect-emitted\""),
        "first read must log redirect-emitted, not cache-hit; got: {log_line}"
    );
    assert!(
        !log_line.contains("\"note\":\"cache-hit\""),
        "first read must NOT carry note=cache-hit; got: {log_line}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn cache_corruption_routes_through_regenerated_branch() {
    // Cache file exists but is unreadable (e.g. permission flip, partial write,
    // user wiped contents). The decider must rebuild the view in memory AND
    // overwrite the stale cache file, log it as RedirectEmitted + note=regenerated,
    // and the next read on top of that must hit the fresh cache cleanly.
    let _env = EnvGuard::new();
    let dir = tmp("regen");
    let src = big_compressible_file(&dir, "src.txt");
    let cache_dir = dir.join("cache");
    std::env::set_var("KNAPSACK_READ_CACHE", &cache_dir);
    std::env::set_var("KNAPSACK_STORE", dir.join("store"));

    let evt = make_event(src.to_str().unwrap(), &[]);
    let (cache_path, reason_1) = unwrap_redirect(decide_with_gate(true, &evt));
    assert_eq!(reason_1, Reason::RedirectEmitted, "first read = fresh");

    // Corrupt the cache file by replacing it with invalid UTF-8 — `read_to_string`
    // returns Err, which is the trigger for the `regenerated` branch.
    std::fs::write(&cache_path, &[0xff, 0xfe, 0xff, 0xfe]).unwrap();

    let (cache_path_2, _reason_2) = unwrap_redirect(decide_with_gate(true, &evt));
    // The corrupt branch still returns the same cache path AND we wrote a fresh
    // view to it. The reason is RedirectEmitted (not CacheHit — the cache wasn't
    // really usable) — the existing infrastructure logs note=regenerated alongside.
    assert_eq!(
        cache_path, cache_path_2,
        "same content -> same cache filename"
    );
    let bytes_after = std::fs::read(&cache_path_2).unwrap();
    assert!(
        std::str::from_utf8(&bytes_after).is_ok(),
        "regenerated cache must be valid UTF-8 again (we rewrote it)"
    );
    assert_ne!(
        bytes_after,
        vec![0xff, 0xfe, 0xff, 0xfe],
        "corrupt bytes must be replaced"
    );

    // Third read should now be a clean CacheHit.
    let (_, reason_3) = unwrap_redirect(decide_with_gate(true, &evt));
    assert_eq!(
        reason_3,
        Reason::CacheHit,
        "post-regeneration read is a cache hit"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn changed_source_routes_to_different_cache_file() {
    // Content addressed: change the bytes, the digest changes, the cache file is new.
    let _env = EnvGuard::new();
    let dir = tmp("changed");
    let src = big_compressible_file(&dir, "src.txt");
    std::env::set_var("KNAPSACK_READ_CACHE", dir.join("cache"));
    std::env::set_var("KNAPSACK_STORE", dir.join("store"));

    let evt = make_event(src.to_str().unwrap(), &[]);
    let (cache_1, _) = unwrap_redirect(decide_with_gate(true, &evt));

    // Modify the source. Same path, different content -> different digest -> different
    // cache file. The old cache file may still exist; that's a job for `knapsack gc`.
    {
        let mut f = std::fs::OpenOptions::new().append(true).open(&src).unwrap();
        writeln!(f, "[INFO] new tail line that shifts the digest").unwrap();
    }
    let (cache_2, _) = unwrap_redirect(decide_with_gate(true, &evt));
    assert_ne!(
        cache_1, cache_2,
        "changed source must route to a different cache file"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------- why-last log integration ----------

#[test]
fn decisions_land_in_the_why_log() {
    // Decide a few times, then read the log back via the public API. This is the
    // dogfood feedback channel: every decision must show up in `knapsack why-last`.
    let _env = EnvGuard::new();
    let dir = tmp("whylog");
    std::env::set_var("KNAPSACK_READ_LOG", dir.join("read_hook.jsonl"));
    std::env::set_var("KNAPSACK_READ_CACHE", dir.join("cache"));
    std::env::set_var("KNAPSACK_STORE", dir.join("store"));

    let small = dir.join("small.txt");
    std::fs::write(&small, b"tiny").unwrap();
    let big = big_compressible_file(&dir, "big.txt");

    // We can't call `apply` directly because it would print to stdout from a test
    // (noisy); record decisions through the why_log API the same way `apply` would.
    let cases = [
        (false, make_event(big.to_str().unwrap(), &[])),
        (true, make_event(small.to_str().unwrap(), &[])),
        (true, make_event(big.to_str().unwrap(), &[])),
    ];
    for (gate, evt) in &cases {
        let d = decide_with_gate(*gate, evt);
        match d {
            ReadDecision::PassThrough { log } | ReadDecision::Redirect { log, .. } => {
                why_log::write_to(&dir.join("read_hook.jsonl"), &log);
            }
        }
    }

    let tail = why_log::read_last_from(&dir.join("read_hook.jsonl"), 10);
    assert_eq!(tail.len(), 3);
    assert_eq!(tail[0].reason, Reason::GateDisabled, "first call: gate off");
    assert_eq!(tail[1].reason, Reason::TooSmall, "second call: too small");
    assert_eq!(
        tail[2].reason,
        Reason::RedirectEmitted,
        "third call: redirect happy path"
    );
    assert!(
        tail[2].redirect_to.is_some(),
        "redirect entry carries the cache path"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------- GC contract ----------

#[test]
fn gc_cleans_read_cache_files() {
    // `knapsack gc` walks the read cache too. We synthesize stale + fresh entries by
    // touching mtimes; gc with older_than=0 removes everything stale (= everything).
    use knapsack::gc::gc as gc_run;
    use knapsack::Store;

    let _env = EnvGuard::new();
    let dir = tmp("gc");
    let cache = dir.join("cache");
    std::fs::create_dir_all(&cache).unwrap();
    std::env::set_var("KNAPSACK_READ_CACHE", &cache);
    std::env::set_var("KNAPSACK_STORE", dir.join("store"));
    // Two synthetic cache files.
    std::fs::write(cache.join("aaaa.md"), b"view of file A").unwrap();
    std::fs::write(cache.join("bbbb.md"), b"view of file B").unwrap();

    let store = Store::new(dir.join("store"));
    let report = gc_run(&store, 0, false);
    assert!(
        report.read_cache_scanned >= 2,
        "gc scanned both cache files"
    );
    assert!(
        report.read_cache_deleted >= 2,
        "gc removed stale cache files"
    );
    assert!(!cache.join("aaaa.md").exists(), "cache file gone");
    assert!(!cache.join("bbbb.md").exists(), "cache file gone");
    let _ = std::fs::remove_dir_all(&dir);
}
