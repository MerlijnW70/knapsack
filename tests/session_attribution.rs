//! Per-block session attribution lifecycle. The bash hook and the MCP server run in
//! separate processes with different session ids; without per-block attribution the
//! recall token cost would land under a different session than the compress that
//! created the block, and per-session net (saved minus refetched) would lie.
//!
//! The contract this file pins:
//!   1. `Store::with_session(dir, "A")` stamps "A" into every `.meta` written.
//!   2. `Store::block_session(h)` returns "A" for those blocks regardless of which
//!      process / Store instance reads them later.
//!   3. `expand_handle` records the refetch under THAT session id, ignoring the
//!      `req.session_id` of the caller — but ONLY when the block has a usable meta.
//!   4. Legacy blocks (no meta, or meta without a session field) fall through to
//!      `req.session_id` so callers without sessions, and old caches written before
//!      this feature, keep working unchanged.

use knapsack::api::{expand_handle, pack_output, ExpandCaller, ExpandRequest, PackRequest};
use knapsack::content_type::ContentType;
use knapsack::hash::handle as block_handle;
use knapsack::store::Store;
use std::path::{Path, PathBuf};

fn tmp(tag: &str) -> PathBuf {
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let d = std::env::temp_dir().join(format!(
        "knapsack-attrib-{}-{}-{}",
        tag,
        std::process::id(),
        t
    ));
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// Set the env vars that scope the store / metrics / sessions / read-log to this
/// test's tempdir. Tests in this file run via the same process, so we use a Mutex
/// to make env mutation race-safe (matches the pattern in tests/read_hook.rs).
use std::sync::{Mutex, MutexGuard, OnceLock};
fn env_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}
struct EnvScope {
    _lock: MutexGuard<'static, ()>,
    prior: Vec<(&'static str, Option<std::ffi::OsString>)>,
}
impl EnvScope {
    fn new(dir: &Path) -> Self {
        let lock = env_lock();
        let keys = [
            "KNAPSACK_STORE",
            "KNAPSACK_METRICS",
            "KNAPSACK_SESSIONS",
            "KNAPSACK_READ_LOG",
        ];
        let prior: Vec<(&str, Option<_>)> =
            keys.iter().map(|k| (*k, std::env::var_os(k))).collect();
        std::env::set_var("KNAPSACK_STORE", dir.join("store"));
        std::env::set_var("KNAPSACK_METRICS", dir.join("metrics.jsonl"));
        std::env::set_var("KNAPSACK_SESSIONS", dir.join("sessions"));
        std::env::set_var("KNAPSACK_READ_LOG", dir.join("read_hook.jsonl"));
        Self { _lock: lock, prior }
    }
}
impl Drop for EnvScope {
    fn drop(&mut self) {
        for (k, v) in self.prior.drain(..) {
            match v {
                Some(s) => std::env::set_var(k, s),
                None => std::env::remove_var(k),
            }
        }
    }
}

fn metrics_lines(p: &std::path::Path) -> Vec<knapsack::json::Json> {
    let text = std::fs::read_to_string(p).unwrap_or_default();
    text.lines()
        .filter_map(|l| knapsack::json::parse(l).ok())
        .collect()
}

fn count_events(lines: &[knapsack::json::Json], event: &str, session: &str) -> usize {
    lines
        .iter()
        .filter(|v| v.get("event").and_then(|x| x.as_str()) == Some(event))
        .filter(|v| v.get("session").and_then(|x| x.as_str()) == Some(session))
        .count()
}

fn refetched_tokens(lines: &[knapsack::json::Json], session: &str) -> i64 {
    lines
        .iter()
        .filter(|v| v.get("event").and_then(|x| x.as_str()) == Some("expand"))
        .filter(|v| v.get("session").and_then(|x| x.as_str()) == Some(session))
        .filter(|v| v.get("ok").and_then(|x| x.as_bool()).unwrap_or(false))
        .filter_map(|v| v.get("tokens").and_then(|x| x.as_f64()))
        .map(|n| n as i64)
        .sum()
}

#[test]
fn store_with_session_stamps_meta_and_block_session_reads_it_back() {
    let dir = tmp("stamp");
    let store = Store::with_session(dir.join("store"), "session-A");
    let bytes = b"the content of a specific block";
    let h = store.put(bytes);
    // The handle's session, looked up from the meta sidecar, must be "session-A".
    assert_eq!(store.block_session(&h).as_deref(), Some("session-A"));
    // A DIFFERENT Store instance pointing at the same dir reads the same answer.
    let other = Store::new(dir.join("store"));
    assert_eq!(
        other.block_session(&h).as_deref(),
        Some("session-A"),
        "a different Store reading the same dir sees the originating session"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn store_new_does_not_stamp_so_block_session_is_none() {
    let dir = tmp("nostamp");
    let store = Store::new(dir.join("store"));
    let h = store.put(b"some bytes");
    // No session id was provided -> meta carries no session field -> lookup returns None.
    assert_eq!(
        store.block_session(&h),
        None,
        "Store::new must not stamp a session, so legacy callers keep working"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn pack_output_stamps_blocks_with_caller_session() {
    let dir = tmp("pack");
    let _env = EnvScope::new(&dir);

    let payload: Vec<u8> = (0..2000)
        .map(|i| format!("[INFO] {i}: a line that's stable and repeated\n"))
        .collect::<String>()
        .into_bytes();

    let r = pack_output(PackRequest {
        session_id: "cc-real-session".into(),
        command: Some("cargo test".into()),
        bytes: payload.clone(),
        content_hint: Some(ContentType::Log),
        step: 0,
        transcript_path: None,
    });
    assert!(!r.handles.is_empty(), "pack stored at least one block");

    // Every handle pack returned should carry our session in its meta.
    let store = Store::new(dir.join("store"));
    for h in &r.handles {
        assert_eq!(
            store.block_session(h).as_deref(),
            Some("cc-real-session"),
            "block {h} not stamped with the pack's session id",
        );
    }
}

#[test]
fn expand_attributes_refetch_to_originating_session_not_caller() {
    // THE main contract. Pack a block under "cc-A", then expand it from a process
    // tagged "mcp-server" (the way the MCP path looks today). The metrics event must
    // land under "cc-A" — that's the session whose savings the recall is "undoing".
    let dir = tmp("attribute");
    let _env = EnvScope::new(&dir);

    let bytes = (0..2000)
        .map(|i| format!("attribution-test line {i}\n"))
        .collect::<String>()
        .into_bytes();

    let r = pack_output(PackRequest {
        session_id: "cc-A".into(),
        command: Some("bash".into()),
        bytes: bytes.clone(),
        content_hint: Some(ContentType::Log),
        step: 0,
        transcript_path: None,
    });

    // Pick one stamped block to recall. (Any one of pack's handles will do; they're
    // all stamped under "cc-A".)
    let h = r
        .handles
        .first()
        .expect("pack stored at least one block")
        .clone();

    // Now expand AS IF FROM THE MCP SERVER — a different process, different session id.
    let out = expand_handle(ExpandRequest {
        handle: h.clone(),
        range: None,
        grep: None,
        context: 0,
        session_id: "mcp-server".into(),
        caller: ExpandCaller::Cli,
    });
    assert!(out.is_some(), "expand should succeed for a stored block");

    // The metrics event for the expand must be under "cc-A", NOT under "mcp-server".
    let metrics_path = dir.join("metrics.jsonl");
    let events = metrics_lines(&metrics_path);
    let under_a = count_events(&events, "expand", "cc-A");
    let under_mcp = count_events(&events, "expand", "mcp-server");
    assert_eq!(
        under_a, 1,
        "expand event must land under the originating session 'cc-A'"
    );
    assert_eq!(
        under_mcp, 0,
        "no event should land under the caller's session id"
    );

    // And concretely: cc-A's refetched bytes are non-zero (the user paid the recall
    // tax for cc-A's own block).
    assert!(
        refetched_tokens(&events, "cc-A") > 0,
        "cc-A's refetch count must include this recall"
    );
}

#[test]
fn expand_falls_back_to_caller_session_when_meta_missing() {
    // Backwards compatibility. Build a block directly via Store::new (no meta or no
    // session field). expand_handle should fall back to req.session_id so old caches
    // and CLI ad-hoc tests still record SOMETHING coherent.
    let dir = tmp("fallback");
    let _env = EnvScope::new(&dir);

    let bytes = b"a single small block recorded without a session".to_vec();
    let store = Store::new(dir.join("store"));
    let h = store.put(&bytes);
    // Sanity: no session stamp.
    assert_eq!(store.block_session(&h), None);
    drop(store);

    let _ = expand_handle(ExpandRequest {
        handle: h.clone(),
        range: None,
        grep: None,
        context: 0,
        session_id: "fallback-session".into(),
        caller: ExpandCaller::Cli,
    });

    let events = metrics_lines(&dir.join("metrics.jsonl"));
    assert_eq!(
        count_events(&events, "expand", "fallback-session"),
        1,
        "with no .meta stamp, refetch falls back to the caller's session id"
    );
}

#[test]
fn dedupes_to_one_session_when_the_same_block_is_packed_twice_in_different_sessions() {
    // Subtle case: if "cc-A" packs block X, then "cc-B" packs the same content, the
    // meta `write_if_absent` policy means the SECOND write doesn't overwrite —
    // "cc-A" stays as the originating session. That's correct: the first to store a
    // unique block paid the upfront cost; subsequent identical content is a dedup
    // hit, no new compression happened in "cc-B" beyond the back-reference.
    let dir = tmp("dedup-attrib");
    let bytes_dir = dir.join("store");

    let block = b"identical content that two sessions independently produce".to_vec();
    let h = block_handle(&block);

    let store_a = Store::with_session(bytes_dir.clone(), "cc-A");
    let _ = store_a.put(&block);
    let store_b = Store::with_session(bytes_dir.clone(), "cc-B");
    let _ = store_b.put(&block);

    let read_back = Store::new(bytes_dir.clone()).block_session(&h);
    assert_eq!(
        read_back.as_deref(),
        Some("cc-A"),
        "first-writer-wins on meta; subsequent identical writes don't overwrite the session"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
