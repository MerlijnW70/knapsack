//! Round-9: stale-cache self-heal, pack-doc on real project markdown,
//! residency interactions, extreme transcript paths, expand exotic combos.
//!
//! Parallel-safe via `common::EnvSandbox`.

mod common;
use common::EnvSandbox;

use knapsack::api::{ExpandCaller, expand_handle, pack_output, record_residency, ExpandRequest, PackRequest};
use knapsack::content_type::ContentType;
use knapsack::hook::wrap_command;
use knapsack::json::Json;
use knapsack::ledger::{Ledger, Residency};
use knapsack::pack_doc::{pack_doc, parse_packed};
use knapsack::read_hook::{decide_with_gate, ReadDecision};
use knapsack::store::Store;
use knapsack::why_log::Reason;
use std::path::PathBuf;

fn tmp(tag: &str) -> PathBuf {
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let d = std::env::temp_dir().join(format!("kn-r9-{}-{}-{}", tag, std::process::id(), t));
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn make_read_event(path: &str) -> Json {
    Json::Obj(vec![
        ("tool_name".into(), Json::Str("Read".into())),
        (
            "tool_input".into(),
            Json::Obj(vec![("file_path".into(), Json::Str(path.into()))]),
        ),
    ])
}

// =====================================================================
// 1. Stale-cache self-heal
// =====================================================================

#[test]
fn read_hook_self_heals_when_store_was_wiped_but_cache_kept() {
    // Setup: pack a file via the Read hook → cache + store both populated.
    // Wipe the STORE (rm -rf ~/.knapsack/store). Read the file AGAIN — the
    // cache file exists, but the store handles named in the cache view no
    // longer resolve. read_hook is documented to re-populate the store on
    // cache hit so the recall handles work again. Verify that contract
    // end-to-end.
    let sb = EnvSandbox::new("self-heal");
    let src = sb.join("src.rs");
    // Use this project's install.rs (~30KB, known to compress past threshold)
    let real_src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/install.rs");
    let content = std::fs::read(&real_src).expect("install.rs must exist");
    std::fs::write(&src, &content).unwrap();

    let evt = make_read_event(src.to_str().unwrap());
    // First read: builds the cache view + populates the store.
    let d1 = decide_with_gate(true, &evt);
    let cache_path = match &d1 {
        ReadDecision::Redirect { redirect_to, .. } => redirect_to.clone(),
        _ => panic!("expected redirect on first read"),
    };
    let view = std::fs::read_to_string(&cache_path).unwrap();
    let whole_handle: String = view
        .lines()
        .filter_map(|l| {
            let i = l.find("knapsack expand ks2_")?;
            let rest = &l[i + "knapsack expand ".len()..];
            // Stop at the first char that isn't alphanumeric or underscore.
            // The handle is `ks2_<32 hex>`: `k`/`s` are alpha, `2`/hex are
            // alnum, `_` is allowed. Trailing `\``, `.`, ` ` end the scan.
            let end = rest
                .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                .unwrap_or(rest.len());
            Some(rest[..end].to_string())
        })
        .next()
        .expect("cache view header must name the whole-file handle");
    assert!(
        whole_handle.starts_with("ks2_") && whole_handle.len() == 36,
        "extracted handle malformed: {whole_handle:?}"
    );

    // Sanity: expand resolves before the wipe.
    let pre_wipe = expand_handle(ExpandRequest {
        handle: whole_handle.clone(),
        range: None,
        grep: None,
        context: 0,
        session_id: "test".into(),
        caller: ExpandCaller::Cli,
    });
    assert!(pre_wipe.is_some(), "handle resolves pre-wipe");

    // WIPE the store. Cache file remains.
    std::fs::remove_dir_all(sb.join("store")).unwrap();
    assert!(!sb.join("store").exists());
    assert!(cache_path.exists(), "cache file MUST still exist post-wipe");

    // Second read: should HIT the cache AND re-populate the store.
    let d2 = decide_with_gate(true, &evt);
    assert!(
        matches!(d2, ReadDecision::Redirect { .. }),
        "second read still redirects"
    );

    // After the cache-hit path runs, the handle should resolve again.
    let post_heal = expand_handle(ExpandRequest {
        handle: whole_handle.clone(),
        range: None,
        grep: None,
        context: 0,
        session_id: "test".into(),
        caller: ExpandCaller::Cli,
    });
    assert!(post_heal.is_some(), "handle MUST resolve after self-heal");
    // And byte-exact: post-heal bytes == original
    let got = match post_heal.unwrap() {
        knapsack::recall::RecallOut::Bytes(b) => b,
        knapsack::recall::RecallOut::Text(t) => t.into_bytes(),
    };
    assert_eq!(
        got, content,
        "self-healed bytes must be byte-exact original"
    );
}

#[test]
fn read_hook_cache_hit_reason_after_self_heal() {
    // Same self-heal scenario, but verify the WHY-LOG reason is CacheHit on
    // the second read (post-fix: reason now correctly differs between fresh
    // and cache-hit).
    let sb = EnvSandbox::new("self-heal-reason");
    let src = sb.join("src.rs");
    let real_src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/install.rs");
    std::fs::write(&src, std::fs::read(&real_src).unwrap()).unwrap();

    let evt = make_read_event(src.to_str().unwrap());
    let d1 = decide_with_gate(true, &evt);
    let r1 = match d1 {
        ReadDecision::Redirect { log, .. } => log.reason,
        _ => panic!(),
    };
    assert_eq!(
        r1,
        Reason::RedirectEmitted,
        "first read is RedirectEmitted (fresh)"
    );

    // Wipe store, repeat
    std::fs::remove_dir_all(sb.join("store")).unwrap();
    let d2 = decide_with_gate(true, &evt);
    let r2 = match d2 {
        ReadDecision::Redirect { log, .. } => log.reason,
        _ => panic!(),
    };
    assert_eq!(
        r2,
        Reason::CacheHit,
        "second read is CacheHit (warm cache, store re-populated under the hood)"
    );
}

// =====================================================================
// 2. Pack-doc round-trip every project markdown
// =====================================================================

fn pack_doc_round_trip(src_filename: &str, store_dir: PathBuf) {
    // The caller is responsible for ensuring KNAPSACK_STORE points at `store_dir`
    // so the direct `Store::new` handle and the env-var-resolved `expand_handle`
    // operate on the same on-disk store. EnvSandbox in the outer test handles that.
    let store = Store::new(store_dir);
    let project_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let src_path = project_root.join(src_filename);
    if !src_path.exists() {
        // Skip if the file isn't in the repo (e.g. DOGFOOD.md may not always be there)
        return;
    }
    let original = std::fs::read(&src_path).expect("source must read");
    let r = pack_doc(src_filename, &original, &store);

    // 1. whole-file handle must resolve byte-exact
    let recalled = store.get(&r.handle).expect("whole-file handle resolves");
    assert_eq!(
        recalled, original,
        "{src_filename}: whole-file recall byte-exact"
    );

    // 2. inspect (parse_packed) extracts the same handle + at least zero markers
    let m = parse_packed(&r.view);
    assert_eq!(
        m.whole_file_handle.as_deref(),
        Some(r.handle.as_str()),
        "{src_filename}: inspect agrees on whole-file handle"
    );

    // 3. Each marker's --lines slice resolves and matches the line range in the original
    let original_text = String::from_utf8_lossy(&original);
    let original_lines: Vec<&str> = original_text.split('\n').collect();
    for marker in &m.markers {
        // 1-based inclusive
        let lo = marker.line_from.saturating_sub(1);
        let hi = marker.line_to.min(original_lines.len());
        if lo >= hi {
            continue;
        }
        let want: String = original_lines[lo..hi].join("\n");
        let got = expand_handle(ExpandRequest {
            handle: marker.handle.clone(),
            range: Some((marker.line_from, marker.line_to)),
            grep: None,
            context: 0,
            session_id: "rt".into(),
            caller: ExpandCaller::Cli,
        });
        let got_text = match got {
            Some(knapsack::recall::RecallOut::Text(t)) => t,
            Some(knapsack::recall::RecallOut::Bytes(b)) => String::from_utf8_lossy(&b).into_owned(),
            None => panic!("{src_filename}: marker {marker:?} did not resolve"),
        };
        assert_eq!(
            got_text, want,
            "{src_filename}: marker {marker:?} did not stitch back to original lines"
        );
    }
}

#[test]
fn pack_doc_round_trip_changelog_md() {
    let sb = EnvSandbox::new("rt-changelog");
    pack_doc_round_trip("CHANGELOG.md", sb.join("store"));
}

#[test]
fn pack_doc_round_trip_readme_md() {
    let sb = EnvSandbox::new("rt-readme");
    pack_doc_round_trip("README.md", sb.join("store"));
}

#[test]
fn pack_doc_round_trip_dogfood_md() {
    let sb = EnvSandbox::new("rt-dogfood");
    pack_doc_round_trip("DOGFOOD.md", sb.join("store"));
}

// =====================================================================
// 3. record_residency + transcript-gate interaction
// =====================================================================

#[test]
fn programmatic_record_residency_visible_in_subsequent_pack() {
    let _sb = EnvSandbox::new("residency-api");

    let session = "rec-resident";
    let h = "ks2_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    record_residency(session, &h.to_string(), 0);

    // Load the ledger from disk and confirm the handle is Resident
    let ledger = Ledger::load(knapsack::config::session_path(session));
    assert_eq!(ledger.residency(&h.to_string()), Residency::Resident);
}

#[test]
fn transcript_says_no_overrides_ledger_says_yes() {
    // The transcript intersection is AND-gated: a handle is treated as
    // Resident only when BOTH the ledger AND the transcript agree. If
    // ledger thinks it's resident but transcript doesn't mention it,
    // it's downgraded → no back-ref emitted → block re-sent.
    use knapsack::pack::pack_with_transcript;
    use std::collections::HashSet;

    let dir = tmp("trans-overrides");
    let store = Store::new(dir.clone());
    let mut ledger = Ledger::in_memory();

    let payload = b"shared content\n".repeat(20);
    // Cold pack populates the ledger
    let _ = pack_with_transcript(&payload, ContentType::Log, &store, &mut ledger, 0, None);
    // Warm pack with NO transcript — uses ledger-only — back-refs fire
    let warm = pack_with_transcript(&payload, ContentType::Log, &store, &mut ledger, 1, None);
    let warm_delta_hits = warm.delta_hits;
    assert!(
        warm_delta_hits > 0,
        "warm pack must produce delta hits with ledger-only"
    );

    // Now warm pack with EMPTY transcript-resident set — ledger says yes but
    // transcript says no → effective Resident is false → no back-refs.
    let empty_set: HashSet<String> = HashSet::new();
    let gated = pack_with_transcript(
        &payload,
        ContentType::Log,
        &store,
        &mut ledger,
        2,
        Some(&empty_set),
    );
    assert_eq!(
        gated.delta_hits, 0,
        "transcript-says-no must override ledger-says-yes"
    );
    assert!(
        gated.evicted_resends >= warm_delta_hits / 2,
        "transcript-downgrade should count as evicted-resends (got {} for {} hits)",
        gated.evicted_resends,
        warm_delta_hits
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn ledger_says_no_means_no_regardless_of_transcript() {
    // Reverse: if ledger doesn't have the handle (cold session), transcript
    // claiming residency doesn't help — Unknown != Resident.
    use knapsack::pack::pack_with_transcript;
    use std::collections::HashSet;

    let dir = tmp("ledger-cold");
    let store = Store::new(dir.clone());
    let mut ledger = Ledger::in_memory(); // empty — Unknown for every handle

    let payload = b"shared content\n".repeat(20);
    // Pre-fill the transcript-resident set with what would be the block handles.
    // We don't know them yet — but it doesn't matter: ledger is Unknown so
    // even matching transcript can't promote to Resident.
    let mut transcript: HashSet<String> = HashSet::new();
    // Inject some fake handles to mimic a transcript with claims we can't verify
    transcript.insert("ks2_ffffffffffffffffffffffffffffffff".into());

    let cold = pack_with_transcript(
        &payload,
        ContentType::Log,
        &store,
        &mut ledger,
        0,
        Some(&transcript),
    );
    assert_eq!(
        cold.delta_hits, 0,
        "cold ledger + transcript claims → still no hits"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

// =====================================================================
// 4. Extreme transcript_path in hook event
// =====================================================================

#[test]
fn wrap_command_with_very_long_transcript_path() {
    let long_path: String = format!("C:/temp/{}.jsonl", "a".repeat(2000));
    let w = wrap_command("cargo test", "/bin/k", "sess", "cargo", Some(&long_path));
    // Must appear quoted with the full path (no truncation)
    assert!(
        w.contains(&format!("--transcript \"{long_path}\"")),
        "very long transcript path must be quoted verbatim"
    );
}

#[test]
fn wrap_command_with_transcript_path_containing_quotes() {
    // Path with embedded `"` would break our naive `--transcript "{path}"`
    // template. We don't currently escape — pin the current behavior so a
    // future hardening pass is conscious.
    let path = "/some path with \"quote\" in it.jsonl";
    let w = wrap_command("cargo test", "/bin/k", "sess", "cargo", Some(path));
    // Current behavior just substitutes the path. The embedded `"` will break
    // shell parsing in practice. Document that we DON'T claim safety here.
    // The transcript_path on the wire comes from Claude Code's own filesystem
    // path, which shouldn't contain literal `"` — but worth pinning.
    assert!(w.contains("--transcript"), "transcript arg appears");
}

#[test]
fn wrap_command_with_transcript_path_empty_or_whitespace_is_skipped() {
    let w1 = wrap_command("cargo test", "/bin/k", "sess", "cargo", Some(""));
    assert!(
        !w1.contains("--transcript"),
        "empty transcript_path skipped"
    );
    let w2 = wrap_command("cargo test", "/bin/k", "sess", "cargo", Some("   "));
    assert!(
        !w2.contains("--transcript"),
        "whitespace-only transcript_path skipped"
    );
    let w3 = wrap_command("cargo test", "/bin/k", "sess", "cargo", Some("\t\n"));
    assert!(
        !w3.contains("--transcript"),
        "tab+newline transcript_path skipped"
    );
}

// =====================================================================
// 5. expand exotic argument combinations
// =====================================================================

#[test]
fn expand_with_lines_then_grep_filters_in_window() {
    // Order matters: --lines slices first, then --grep filters within the slice.
    let sb = EnvSandbox::new("exp-lines-grep");
    let store = Store::new(sb.join("store"));
    let payload =
        b"line1 hello\nline2 world\nline3 hello world\nline4 nothing\nline5 hello again\n";
    let h = store.put(payload);

    // --lines 1-4 → first 4 lines. --grep hello → matches lines 1, 3 (not 5).
    let out = expand_handle(ExpandRequest {
        handle: h.clone(),
        range: Some((1, 4)),
        grep: Some("hello".into()),
        context: 0,
        session_id: "x".into(),
        caller: ExpandCaller::Cli,
    });
    let text = match out.unwrap() {
        knapsack::recall::RecallOut::Text(t) => t,
        _ => panic!(),
    };
    assert!(text.contains("line1 hello"));
    assert!(text.contains("line3 hello world"));
    assert!(
        !text.contains("line5"),
        "line 5 is outside the --lines window"
    );
}

#[test]
fn expand_with_grep_and_context_combined() {
    let sb = EnvSandbox::new("exp-grep-ctx");
    let store = Store::new(sb.join("store"));
    let payload = b"unrelated 1\nunrelated 2\nTARGET\nunrelated 3\nunrelated 4\n";
    let h = store.put(payload);

    let out = expand_handle(ExpandRequest {
        handle: h.clone(),
        range: None,
        grep: Some("TARGET".into()),
        context: 1,
        session_id: "x".into(),
        caller: ExpandCaller::Cli,
    });
    let text = match out.unwrap() {
        knapsack::recall::RecallOut::Text(t) => t,
        _ => panic!(),
    };
    // context=1 around TARGET: lines 2, 3, 4 (1-based) = "unrelated 2\nTARGET\nunrelated 3"
    assert!(text.contains("unrelated 2"));
    assert!(text.contains("TARGET"));
    assert!(text.contains("unrelated 3"));
    assert!(
        !text.contains("unrelated 1"),
        "context=1 doesn't include unrelated 1"
    );
    assert!(
        !text.contains("unrelated 4"),
        "context=1 doesn't include unrelated 4"
    );
}

#[test]
fn expand_grep_with_unicode_pattern() {
    let sb = EnvSandbox::new("exp-grep-unicode");
    let store = Store::new(sb.join("store"));
    let payload = "ascii line\n世界 line\nemoji 🎒 line\n更多 line\nfinal ascii\n".as_bytes();
    let h = store.put(payload);

    let out = expand_handle(ExpandRequest {
        handle: h.clone(),
        range: None,
        grep: Some("世界".into()),
        context: 0,
        session_id: "x".into(),
        caller: ExpandCaller::Cli,
    });
    let text = match out.unwrap() {
        knapsack::recall::RecallOut::Text(t) => t,
        _ => panic!(),
    };
    assert!(text.contains("世界"), "unicode grep finds CJK match");
    assert!(!text.contains("ascii line"));
}

#[test]
fn expand_lines_on_non_utf8_bytes_returns_full_bytes() {
    // recall::expand: if the bytes aren't UTF-8, --lines slicing falls back
    // to returning the full Bytes (documented).
    let sb = EnvSandbox::new("exp-binary-lines");
    let store = Store::new(sb.join("store"));
    let payload: Vec<u8> = (0..=255u8).collect(); // binary, definitely not UTF-8
    let h = store.put(&payload);

    let out = expand_handle(ExpandRequest {
        handle: h.clone(),
        range: Some((1, 5)),
        grep: None,
        context: 0,
        session_id: "x".into(),
        caller: ExpandCaller::Cli,
    });
    // Per recall.rs: "If the content isn't UTF-8, slicing falls back to
    // returning the full exact bytes."
    match out.unwrap() {
        knapsack::recall::RecallOut::Bytes(b) => {
            assert_eq!(b, payload, "fallback returns full bytes")
        }
        knapsack::recall::RecallOut::Text(_) => panic!("non-UTF-8 should return Bytes"),
    }
}

// =====================================================================
// 6. pack_output then expand with various sessions (refetch attribution)
// =====================================================================

#[test]
fn back_to_back_packs_with_alternating_sessions_dont_interfere() {
    let _sb = EnvSandbox::new("alt-sessions");

    let payload = b"alt sessions test\nlinetwo\nlinethree\n".repeat(20);

    // A→B→A→B in steps 0..4
    for step in 0..4 {
        let sess = if step % 2 == 0 { "alt-a" } else { "alt-b" };
        let r = pack_output(PackRequest {
            session_id: sess.into(),
            command: None,
            bytes: payload.to_vec(),
            content_hint: Some(ContentType::Log),
            step,
            transcript_path: None,
        });
        if step == 0 || step == 1 {
            assert_eq!(r.delta_hits, 0, "cold first pack in each session");
        }
        if step == 2 || step == 3 {
            assert!(
                r.delta_hits > 0,
                "warm pack in session {sess} (step {step}) should have delta hits"
            );
        }
    }
}
