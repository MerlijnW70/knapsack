//! The view-to-recall round trip from the model's perspective. When the read hook
//! emits a compressed view, every `ks2_X` it advertises (in the header and in every
//! `[Knapsack: ... · recall ks2_X]` elision marker) MUST resolve to byte-exact
//! original bytes. The earlier implementation only built the view and forgot to
//! populate the store; the model would try to recall and get `no such handle`. This
//! test pins the contract: produce a view, recall every handle in it, compare to the
//! source byte-by-byte.

use knapsack::json::Json;
use knapsack::read_hook::{decide_with_gate, ReadDecision};
use knapsack::store::Store;
use std::collections::BTreeSet;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard, OnceLock};

fn env_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(())).lock().unwrap_or_else(|e| e.into_inner())
}

struct EnvScope {
    _lock: MutexGuard<'static, ()>,
    prior: Vec<(&'static str, Option<std::ffi::OsString>)>,
}
impl EnvScope {
    fn new(dir: &std::path::Path) -> Self {
        let lock = env_lock();
        let keys = ["KNAPSACK_STORE", "KNAPSACK_READ_LOG", "KNAPSACK_READ_CACHE", "KNAPSACK_METRICS"];
        let prior: Vec<(&str, Option<_>)> = keys.iter().map(|k| (*k, std::env::var_os(k))).collect();
        std::env::set_var("KNAPSACK_STORE", dir.join("store"));
        std::env::set_var("KNAPSACK_READ_LOG", dir.join("read_hook.jsonl"));
        std::env::set_var("KNAPSACK_READ_CACHE", dir.join("cache"));
        std::env::set_var("KNAPSACK_METRICS", dir.join("metrics.jsonl"));
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

fn tmp(tag: &str) -> PathBuf {
    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    let d = std::env::temp_dir().join(format!("knapsack-rh-recall-{}-{}-{}", tag, std::process::id(), t));
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn big_compressible(dir: &std::path::Path, name: &str) -> PathBuf {
    let p = dir.join(name);
    let mut f = std::fs::File::create(&p).unwrap();
    for i in 0..500 {
        writeln!(
            f,
            "[INFO] step {i}: routine work that compresses well; lots of similar lines for the structural log compressor to find a stable middle to elide"
        )
        .unwrap();
    }
    p
}

fn make_event(file_path: &str, session_id: Option<&str>) -> Json {
    let mut obj = vec![
        ("tool_name".into(), Json::Str("Read".into())),
        ("tool_input".into(), Json::Obj(vec![("file_path".into(), Json::Str(file_path.into()))])),
    ];
    if let Some(s) = session_id {
        obj.push(("session_id".into(), Json::Str(s.into())));
    }
    Json::Obj(obj)
}

fn handles_in_view(view: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for (i, _) in view.char_indices() {
        // Look for `ks2_` followed by exactly 32 hex chars.
        if !view[i..].starts_with("ks2_") {
            continue;
        }
        let rest = &view[i + 4..];
        if rest.len() >= 32 && rest[..32].chars().all(|c| c.is_ascii_hexdigit()) {
            out.insert(format!("ks2_{}", &rest[..32]));
        }
    }
    out
}

#[test]
fn every_handle_in_the_view_resolves_byte_exact() {
    let dir = tmp("roundtrip");
    let _env = EnvScope::new(&dir);

    let src = big_compressible(&dir, "src.txt");
    let bytes = std::fs::read(&src).unwrap();

    let evt = make_event(src.to_str().unwrap(), Some("test-session"));
    let decision = decide_with_gate(true, &evt);
    let redirect_to = match decision {
        ReadDecision::Redirect { redirect_to, .. } => redirect_to,
        ReadDecision::PassThrough { log } => panic!("expected Redirect, got PassThrough({:?})", log.reason),
    };

    let view = std::fs::read_to_string(&redirect_to).expect("view exists");
    let handles = handles_in_view(&view);
    assert!(!handles.is_empty(), "view must advertise at least one handle");

    // Open the SAME store the hook just wrote into and verify every handle resolves
    // to non-empty bytes that hash back to its handle.
    let store = Store::new(dir.join("store"));
    let mut missing: Vec<String> = Vec::new();
    let mut wrong_hash: Vec<String> = Vec::new();
    for h in &handles {
        match store.get(h) {
            None => missing.push(h.clone()),
            Some(b) => {
                if !knapsack::hash::verify(h, &b) {
                    wrong_hash.push(h.clone());
                }
            }
        }
    }
    assert!(missing.is_empty(), "view advertises {} handles that don't resolve: {:?}", missing.len(), &missing[..missing.len().min(3)]);
    assert!(wrong_hash.is_empty(), "view advertises {} handles that resolve to wrong bytes: {:?}", wrong_hash.len(), &wrong_hash[..wrong_hash.len().min(3)]);

    // The whole-file handle specifically (the one the header points at) must round-
    // trip BYTE-EXACT against the original source.
    let whole_handle = knapsack::hash::handle(&bytes);
    let recovered = store.get(&whole_handle).expect("whole-file handle in store");
    assert_eq!(recovered, bytes, "whole-file recall must equal the original byte-for-byte");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn view_header_advertises_a_resolvable_whole_file_handle() {
    // Pin the header text contract: the line that says "knapsack expand ks2_X" MUST
    // name a handle that, when expanded, returns the ORIGINAL bytes of the source —
    // not some intermediate elision and not a 404.
    let dir = tmp("header-handle");
    let _env = EnvScope::new(&dir);

    let src = big_compressible(&dir, "src.txt");
    let bytes = std::fs::read(&src).unwrap();

    let evt = make_event(src.to_str().unwrap(), Some("hdr"));
    let redirect_to = match decide_with_gate(true, &evt) {
        ReadDecision::Redirect { redirect_to, .. } => redirect_to,
        _ => panic!("expected Redirect"),
    };
    let view = std::fs::read_to_string(&redirect_to).unwrap();

    // The header line we care about looks like:
    //   <!--   Or recall exact bytes via `knapsack expand ks2_<32hex>`. -->
    let header_line = view
        .lines()
        .find(|l| l.contains("Or recall exact bytes"))
        .expect("view must carry the 'Or recall exact bytes' header line");
    // Extract the handle. Format is `knapsack expand <HANDLE>` inside backticks.
    let h = header_line
        .split("knapsack expand ")
        .nth(1)
        .and_then(|s| s.split('`').next())
        .map(|s| s.trim().to_string())
        .expect("header line contains a parseable handle");
    assert!(h.starts_with("ks2_"), "header handle must be a ks2 handle, got {h:?}");
    assert!(knapsack::hash::is_valid_handle(&h), "header handle must be syntactically valid: {h:?}");

    // Resolve it AGAINST THE SAME store the hook wrote into.
    let store = Store::new(dir.join("store"));
    let recovered = store.get(&h).expect("header-advertised handle must resolve");
    assert_eq!(recovered, bytes, "header-advertised handle must return the ORIGINAL bytes byte-exact");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn cache_hit_re_populates_store_after_a_wipe() {
    // Recovery contract: if the cache dir survives but the store was wiped (uninstall
    // --purge then restoring the cache from a backup, or a hand-edit gone wrong),
    // the next Read should self-heal by re-stamping the blocks into the store. Without
    // this, the user would see views that point at "no such handle" until the file
    // was naturally modified.
    let dir = tmp("recovery");
    let _env = EnvScope::new(&dir);

    let src = big_compressible(&dir, "src.txt");
    let bytes = std::fs::read(&src).unwrap();

    let evt = make_event(src.to_str().unwrap(), Some("recovery"));
    // First read: builds view + stamps store.
    let _ = decide_with_gate(true, &evt);
    let whole_handle = knapsack::hash::handle(&bytes);
    {
        let store = Store::new(dir.join("store"));
        assert!(store.get(&whole_handle).is_some(), "store should have the whole-file handle after the first read");
    }

    // Now nuke the store dir (NOT the cache). Verify the next read re-populates.
    std::fs::remove_dir_all(dir.join("store")).expect("can remove store");
    assert!(Store::new(dir.join("store")).get(&whole_handle).is_none(), "store really empty");

    // Second read: cache hit. Should re-stamp the store from the source bytes.
    let _ = decide_with_gate(true, &evt);
    let store = Store::new(dir.join("store"));
    assert!(store.get(&whole_handle).is_some(), "cache-hit path must re-stamp the store");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn two_paths_same_content_get_distinct_cache_files_with_correct_headers() {
    // The content-addressed cache used to share one file across paths, so two paths
    // with identical bytes (e.g. an identical lock file in two workspaces, or a
    // backup copy) shared a header that named the FIRST writer's path. A second
    // reader saw a misleading "Original file: <not-my-path>" in the view. The fix is
    // a per-(content, path) cache filename: SHA-256 prefix of content + SHA-1 prefix
    // of path, so each path gets its own view file with its own header — while the
    // byte-exact store still dedupes the actual content to one entry.
    let dir = tmp("coh-paths");
    let _env = EnvScope::new(&dir);

    let path_a = dir.join("workspace_a").join("file.log");
    let path_b = dir.join("workspace_b").join("file.log");
    std::fs::create_dir_all(path_a.parent().unwrap()).unwrap();
    std::fs::create_dir_all(path_b.parent().unwrap()).unwrap();

    // Write IDENTICAL bytes to both paths.
    let mut f = std::fs::File::create(&path_a).unwrap();
    for i in 0..500 {
        writeln!(f, "[INFO] step {i}: stable repeated line").unwrap();
    }
    drop(f);
    std::fs::copy(&path_a, &path_b).unwrap();

    let evt_a = make_event(path_a.to_str().unwrap(), Some("coh"));
    let evt_b = make_event(path_b.to_str().unwrap(), Some("coh"));
    let redirect_a = match decide_with_gate(true, &evt_a) {
        ReadDecision::Redirect { redirect_to, .. } => redirect_to,
        _ => panic!("expected Redirect for path A"),
    };
    let redirect_b = match decide_with_gate(true, &evt_b) {
        ReadDecision::Redirect { redirect_to, .. } => redirect_to,
        _ => panic!("expected Redirect for path B"),
    };

    // Two distinct cache files, sharing the content-digest prefix.
    let name_a = redirect_a.file_name().unwrap().to_string_lossy().into_owned();
    let name_b = redirect_b.file_name().unwrap().to_string_lossy().into_owned();
    assert_ne!(name_a, name_b, "two paths must get distinct cache files");
    let prefix_a = name_a.split('_').next().unwrap();
    let prefix_b = name_b.split('_').next().unwrap();
    assert_eq!(prefix_a, prefix_b, "shared content -> shared digest prefix in both cache filenames");

    // Each cache file's "Original file:" header must name ITS OWN path.
    let view_a = std::fs::read_to_string(&redirect_a).unwrap();
    let view_b = std::fs::read_to_string(&redirect_b).unwrap();
    let header_a = view_a.lines().find(|l| l.starts_with("<!-- Original file: ")).expect("header A");
    let header_b = view_b.lines().find(|l| l.starts_with("<!-- Original file: ")).expect("header B");
    assert!(header_a.contains(path_a.to_str().unwrap()), "view A header must name path A, got: {header_a}");
    assert!(header_b.contains(path_b.to_str().unwrap()), "view B header must name path B, got: {header_b}");
    assert!(!header_a.contains(path_b.to_str().unwrap()), "view A must NOT name path B");
    assert!(!header_b.contains(path_a.to_str().unwrap()), "view B must NOT name path A");

    // Store dedup is preserved: both views advertise the SAME whole-file handle
    // (which expands to the shared content byte-exact).
    let h_a = view_a.lines().find_map(|l| l.split("knapsack expand ").nth(1)).and_then(|s| s.split('`').next()).unwrap().to_string();
    let h_b = view_b.lines().find_map(|l| l.split("knapsack expand ").nth(1)).and_then(|s| s.split('`').next()).unwrap().to_string();
    assert_eq!(h_a, h_b, "both views must advertise the SAME store handle (content dedup is intact)");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn markdown_routes_through_pack_doc_not_structural_log() {
    // Before this wire-up, .md files fell through to the structural log compressor
    // which doesn't recognise heading / code-fence / list structure, so most
    // markdown hit `worse-than-raw` and passed through unchanged. pack_doc DOES
    // understand markdown — for a long-prose design doc it routinely beats raw by
    // 50%+. This test pins the wire-up: a long-prose .md file redirects, the view
    // carries pack_doc-shape elisions (`<!-- ks-recall handle=... lines=A-B -->`),
    // the whole-file handle in the header round-trips byte-exact, AND the view
    // does NOT carry pack_doc's own header (the outer read-cache header is the
    // single source of identity).
    let dir = tmp("md");
    let _env = EnvScope::new(&dir);

    // Long-prose markdown — single-line paragraphs ≥500 chars are the canonical
    // pack_doc target. We synthesize one that we KNOW clears pack_doc's threshold
    // AND the read hook's 25% reduction floor.
    let md_path = dir.join("design.md");
    let mut f = std::fs::File::create(&md_path).unwrap();
    writeln!(f, "# Design Document\n").unwrap();
    let long_sentence = "This section describes a distributed system component that handles authentication and rate limiting across multiple regions, with careful attention to consistency guarantees, latency budgets, and operational complexity. ".repeat(4);
    for sec in 0..6 {
        writeln!(f, "## Section {sec}\n").unwrap();
        for _ in 0..6 {
            writeln!(f, "{long_sentence}\n").unwrap();
        }
        writeln!(f, "```rust\nfn handler() -> Result<u64, Error> {{ Ok(0) }}\n```\n").unwrap();
    }
    drop(f);
    let bytes = std::fs::read(&md_path).unwrap();

    let evt = make_event(md_path.to_str().unwrap(), Some("md-test"));
    let redirect_to = match decide_with_gate(true, &evt) {
        ReadDecision::Redirect { redirect_to, .. } => redirect_to,
        ReadDecision::PassThrough { log } => {
            panic!("expected long-prose .md to redirect, got pass-through with reason {:?}", log.reason)
        }
    };
    let view = std::fs::read_to_string(&redirect_to).unwrap();

    // The body uses pack_doc's elision shape: `<!-- ks-recall handle=... lines=A-B -->`.
    assert!(
        view.contains("ks-recall handle="),
        "markdown view must carry pack_doc-shape ks-recall markers, got:\n{}",
        &view[..view.len().min(500)]
    );
    assert!(
        view.contains("section omitted"),
        "markdown view must carry the user-facing 'section omitted' banner"
    );

    // pack_doc's own two-line header MUST be stripped — the outer read-cache header
    // is the single source of identity.
    assert!(
        !view.contains("<!-- ks-pack source="),
        "pack_doc's manifest header must be stripped, got it at:\n{}",
        view.lines().take(15).collect::<Vec<_>>().join("\n")
    );

    // Whole-file handle from the outer header round-trips byte-exact against the
    // ORIGINAL bytes (not the view). This is what makes the user's `expand` work.
    let h_line = view
        .lines()
        .find(|l| l.contains("Or recall exact bytes"))
        .expect("outer header carries recall instruction");
    let h: String = h_line
        .split("knapsack expand ")
        .nth(1)
        .and_then(|s| s.split('`').next())
        .map(|s| s.trim().to_string())
        .expect("header handle parseable");
    let store = Store::new(dir.join("store"));
    let recovered = store.get(&h).expect("whole-file handle in store");
    assert_eq!(recovered, bytes, "whole-file recall must equal the source byte-exact");

    // Every `ks-recall` handle in the body should ALSO be the same whole-file
    // handle (pack_doc puts only one handle in the store; line ranges slice it).
    let all_ks_recall: BTreeSet<String> = view
        .match_indices("ks-recall handle=")
        .filter_map(|(i, _)| {
            let rest = &view[i + "ks-recall handle=".len()..];
            rest.split_whitespace().next().map(|s| s.to_string())
        })
        .collect();
    assert_eq!(all_ks_recall.len(), 1, "pack_doc elisions should all reference one handle");
    assert!(all_ks_recall.contains(&h), "ks-recall handles must match the header's whole-file handle");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn header_no_longer_says_experimental_or_default_off() {
    // Sanity check: the user-facing surface had been cleaned up everywhere else, but
    // the view header was still flagging EXPERIMENTAL / default-off. Pin the cleanup.
    let dir = tmp("no-experimental");
    let _env = EnvScope::new(&dir);

    let src = big_compressible(&dir, "src.txt");
    let evt = make_event(src.to_str().unwrap(), Some("clean"));
    let redirect_to = match decide_with_gate(true, &evt) {
        ReadDecision::Redirect { redirect_to, .. } => redirect_to,
        _ => panic!("expected Redirect"),
    };
    let view = std::fs::read_to_string(&redirect_to).unwrap();
    let header_block: String = view.lines().take(8).collect::<Vec<_>>().join("\n");
    assert!(!header_block.contains("EXPERIMENTAL"), "view header must not advertise EXPERIMENTAL:\n{header_block}");
    assert!(!header_block.contains("default-off"), "view header must not say default-off:\n{header_block}");
    assert!(!header_block.contains("(when packed)"), "header must not say '(when packed)' — the handle is always packed now:\n{header_block}");

    let _ = std::fs::remove_dir_all(&dir);
}
