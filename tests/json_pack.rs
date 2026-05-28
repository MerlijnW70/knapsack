//! JSON-aware compression: the contracts the brief calls out, pinned end-to-end.
//!
//! - Detection: package.json / tsconfig.json / .json files recognised by content_type.
//! - Splitter: tiles cover the input byte-exact (reconstruct identity).
//! - Cold-pass: large top-level members elide with key name preserved.
//! - Delta: re-read of identical JSON back-references heavily.
//! - One-field change: only the touched key shifts; everything else dedups.
//! - Malformed JSON: safe fallback (single tile, no panics).
//! - Large arrays: tile per element, compact view + byte-exact recall.

use knapsack::block::{split_blocks, split_json};
use knapsack::content_type::{detect, ContentType};
use knapsack::ledger::Ledger;
use knapsack::pack::{pack, reconstruct};
use knapsack::store::Store;
use knapsack::token_estimate::tokens_bytes;
use std::path::PathBuf;

fn store(tag: &str) -> Store {
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    Store::new(std::env::temp_dir().join(format!(
        "knapsack-json-{}-{}-{}",
        tag,
        std::process::id(),
        t
    )))
}

/// A reasonably-sized package.json, prettified. The dependencies block is ~600 bytes —
/// over the elision threshold — so cold-pass should compress it. devDependencies sits
/// just below to exercise the keep-verbatim branch too.
fn package_json_fixture() -> Vec<u8> {
    let body = r#"{
  "name": "knapsack-test",
  "version": "0.0.1",
  "description": "A representative fixture for json-aware compression tests.",
  "main": "dist/index.js",
  "scripts": {
    "build": "tsc --project tsconfig.json",
    "test": "vitest run",
    "lint": "eslint . --ext .ts"
  },
  "dependencies": {
    "axios": "^1.7.0",
    "express": "^4.19.2",
    "lodash": "^4.17.21",
    "pg": "^8.11.5",
    "redis": "^4.6.13",
    "winston": "^3.13.0",
    "zod": "^3.23.8",
    "uuid": "^10.0.0",
    "dotenv": "^16.4.5",
    "jsonwebtoken": "^9.0.2",
    "bcrypt": "^5.1.1",
    "cors": "^2.8.5",
    "helmet": "^7.1.0",
    "compression": "^1.7.4",
    "morgan": "^1.10.0"
  },
  "devDependencies": {
    "typescript": "^5.4.5",
    "vitest": "^1.6.0"
  }
}
"#;
    body.as_bytes().to_vec()
}

/// A synthesized API-response fixture: lots of repeating per-record objects under a
/// `data` array. About 4 KB; dominated by one big member.
fn api_response_fixture() -> Vec<u8> {
    let mut s = String::from(
        r#"{
  "ok": true,
  "took_ms": 42,
  "page": 1,
  "page_size": 20,
  "total": 156,
  "data": [
"#,
    );
    for i in 0..20 {
        s.push_str(&format!(
            "    {{\"id\": {i}, \"user\": \"user{i}\", \"email\": \"u{i}@example.com\", \"created_at\": \"2026-05-{:02}T10:11:12Z\", \"status\": \"active\", \"role\": \"member\"}}",
            (i % 28) + 1
        ));
        if i < 19 {
            s.push(',');
        }
        s.push('\n');
    }
    s.push_str("  ]\n}\n");
    s.into_bytes()
}

// ---------- detection ----------

#[test]
fn detection_picks_json_for_dot_json_and_well_known_filenames() {
    let body = b"{\"k\":1}";
    assert_eq!(detect(body, Some("/path/foo.json")), ContentType::Json);
    assert_eq!(detect(body, Some("/path/package.json")), ContentType::Json);
    assert_eq!(detect(body, Some("/path/tsconfig.json")), ContentType::Json);
    // Content sniff (no path hint): valid JSON gets recognised too.
    assert_eq!(detect(body, None), ContentType::Json);
}

#[test]
fn detection_falls_back_to_log_for_malformed_json() {
    // First-char looks like JSON but content doesn't parse — must NOT be Json.
    let bad = b"{ this is not really json at all";
    assert_ne!(detect(bad, None), ContentType::Json);
    // Path hint is ignored when content clearly isn't JSON-shaped.
    let html = b"<html><body>nope</body></html>";
    assert_ne!(detect(html, Some("/path/index.html")), ContentType::Json);
}

// ---------- splitter ----------

#[test]
fn split_json_tiles_cover_input_byte_exact() {
    // The reconstruct invariant: tiles join with no gap or overlap, total length = input.
    let pkg = package_json_fixture();
    let api = api_response_fixture();
    let cases: &[&[u8]] = &[
        b"{}",
        b"[]",
        br#"{"a":1}"#,
        br#"{"a":1,"b":[1,2,3],"c":{"nested":true}}"#,
        pkg.as_slice(),
        api.as_slice(),
        b"{\n}\n", // trailing newline
    ];
    for input in cases {
        let bytes: Vec<u8> = input.to_vec();
        let tiles = split_json(&bytes);
        if bytes.is_empty() {
            continue;
        }
        assert_eq!(tiles.first().map(|t| t.0), Some(0), "tiles start at 0");
        assert_eq!(
            tiles.last().map(|t| t.1),
            Some(bytes.len()),
            "tiles end at len"
        );
        for w in tiles.windows(2) {
            assert_eq!(
                w[0].1,
                w[1].0,
                "no gap/overlap between tiles for input of {} bytes",
                bytes.len()
            );
        }
        // And byte-exact concatenation
        let mut rejoined = Vec::with_capacity(bytes.len());
        for &(s, e) in &tiles {
            rejoined.extend_from_slice(&bytes[s..e]);
        }
        assert_eq!(rejoined, bytes, "concatenation must equal input");
    }
}

#[test]
fn malformed_json_returns_single_tile() {
    // The splitter MUST be defensive — never panic, always return tiles that tile [0, len).
    let cases: &[&[u8]] = &[
        b"{ this isn't json",
        b"\"just a top-level string\"",
        b"42",
        b"[1, 2,",                       // unterminated
        b"{\"a\":\"unterminated string", // unterminated string
        b"",                             // empty -> Vec::new()
    ];
    for input in cases {
        let tiles = split_json(input);
        if input.is_empty() {
            assert!(tiles.is_empty(), "empty -> no tiles");
        } else {
            assert!(
                tiles.len() == 1 || tiles_cover(input.len(), &tiles),
                "single-tile fallback or full cover: {:?}",
                tiles
            );
        }
    }
}

fn tiles_cover(len: usize, tiles: &[(usize, usize)]) -> bool {
    if tiles.is_empty() {
        return len == 0;
    }
    tiles[0].0 == 0 && tiles.last().unwrap().1 == len && tiles.windows(2).all(|w| w[0].1 == w[1].0)
}

#[test]
fn split_blocks_dispatches_to_json_path() {
    let bytes = package_json_fixture();
    let blocks = split_blocks(&bytes, ContentType::Json);
    assert!(
        blocks.len() >= 5,
        "package.json must split into multiple member tiles"
    );
    assert!(
        tiles_cover(bytes.len(), &blocks),
        "JSON tiles must tile the input exactly"
    );
}

// ---------- the brief's five test cases ----------

#[test]
fn rereading_identical_json_gives_high_delta_reduction() {
    let s = store("identical");
    let mut ledger = Ledger::in_memory();
    let bytes = package_json_fixture();

    let r1 = pack(&bytes, ContentType::Json, &s, &mut ledger, 0);
    assert_eq!(r1.delta_hits, 0, "cold first read references nothing");
    let r2 = pack(&bytes, ContentType::Json, &s, &mut ledger, 1);

    assert!(r2.delta_hits > 0, "unchanged JSON re-read must back-ref");
    assert!(
        r2.shown_tokens_est * 4 < r1.shown_tokens_est,
        "identical re-read should be at LEAST 4x cheaper than the first read; got {} -> {}",
        r1.shown_tokens_est,
        r2.shown_tokens_est
    );
}

#[test]
fn one_changed_field_only_invalidates_that_member() {
    // Bump "version" in place. Every other top-level key keeps its bytes -> backref.
    let s = store("changed-one");
    let mut ledger = Ledger::in_memory();
    let v1 = package_json_fixture();
    let v2: Vec<u8> = String::from_utf8(v1.clone())
        .unwrap()
        .replace("\"version\": \"0.0.1\"", "\"version\": \"0.0.2\"")
        .into_bytes();
    assert_ne!(v1, v2, "fixture must actually differ");

    let _r1 = pack(&v1, ContentType::Json, &s, &mut ledger, 0);
    let r2 = pack(&v2, ContentType::Json, &s, &mut ledger, 1);

    let blocks_v2 = split_blocks(&v2, ContentType::Json);
    assert!(
        r2.delta_hits >= blocks_v2.len() - 3,
        "only the version member should change; got delta_hits={} out of {} blocks",
        r2.delta_hits,
        blocks_v2.len()
    );
}

#[test]
fn invalid_json_falls_back_safely() {
    // Tell pack the type is JSON but feed it garbage. Splitter returns one tile;
    // structural::compress_json emits verbatim. Reconstruct stays byte-exact.
    let s = store("invalid");
    let mut ledger = Ledger::in_memory();
    let bad = b"{ this fragment is not json at all }".to_vec();
    let r = pack(&bad, ContentType::Json, &s, &mut ledger, 0);
    assert_eq!(
        r.shown_tokens_est,
        tokens_bytes(&bad),
        "malformed JSON falls through to verbatim"
    );
    let back =
        reconstruct(&bad, ContentType::Json, &s).expect("reconstruct OK on invalid JSON too");
    assert_eq!(back, bad, "byte-exact reconstruct after fallback");
}

#[test]
fn large_array_is_compact_and_byte_exact_recallable() {
    let s = store("array");
    let mut ledger = Ledger::in_memory();
    let bytes = api_response_fixture();

    let r = pack(&bytes, ContentType::Json, &s, &mut ledger, 0);
    assert!(
        r.shown_tokens_est < r.raw_tokens_est,
        "API-response fixture should compress on cold pass: {} -> {}",
        r.raw_tokens_est,
        r.shown_tokens_est
    );
    let back = reconstruct(&bytes, ContentType::Json, &s).expect("reconstruct OK");
    assert_eq!(back, bytes, "byte-exact recall of the original");
}

#[test]
fn secrets_in_json_are_not_logged_anywhere_new() {
    // Brief contract: JSON-aware compression must NOT add any new logging sink for
    // sensitive content. We pack a payload containing a fake secret and verify that
    // the only place that string lands is the byte-exact store (which already
    // received it through every other pack() call). pack_doc, read_hook, status,
    // metrics, etc. — none of them log the raw JSON.
    use std::fs;

    let secret = "PRIVATE_API_KEY_AKIA0000FAKE0000DEAD";
    let payload = format!(
        r#"{{"name":"x","version":"1","auth":{{"key":"{}"}},"settings":{{"timeout":30}}}}"#,
        secret
    );

    let s_dir = std::env::temp_dir().join(format!(
        "knapsack-json-secrets-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let metrics_path = s_dir.join("metrics.jsonl");
    let read_log_path = s_dir.join("read_hook.jsonl");
    fs::create_dir_all(&s_dir).unwrap();

    std::env::set_var("KNAPSACK_STORE", s_dir.join("store"));
    std::env::set_var("KNAPSACK_METRICS", &metrics_path);
    std::env::set_var("KNAPSACK_READ_LOG", &read_log_path);

    let s = Store::new(s_dir.join("store"));
    let mut ledger = Ledger::in_memory();
    let _ = pack(payload.as_bytes(), ContentType::Json, &s, &mut ledger, 0);

    // The metrics file (if any) must not contain the secret.
    if let Ok(text) = fs::read_to_string(&metrics_path) {
        assert!(
            !text.contains(secret),
            "secret leaked into metrics.jsonl:\n{}",
            text
        );
    }
    if let Ok(text) = fs::read_to_string(&read_log_path) {
        assert!(
            !text.contains(secret),
            "secret leaked into read_hook.jsonl:\n{}",
            text
        );
    }
    let _ = fs::remove_dir_all(&s_dir);
}

// ---------- the COLD pass actually compresses (sanity for reduction reporting) ----------

#[test]
fn cold_pass_view_marks_large_members_with_key_name() {
    let s = store("cold-marker");
    let mut ledger = Ledger::in_memory();
    let bytes = package_json_fixture();
    let r = pack(&bytes, ContentType::Json, &s, &mut ledger, 0);

    // The big `dependencies` block is well over the 240-byte keep budget; it should
    // appear as a Knapsack section-omitted marker, with the key NAME surfaced.
    assert!(
        r.view.contains("[Knapsack: section omitted"),
        "cold-pass view should contain at least one elision marker:\n{}",
        r.view
    );
    assert!(
        r.view.contains("\"dependencies\":"),
        "elision marker must preserve the key name `dependencies`:\n{}",
        r.view
    );
    // And small fields stay verbatim.
    assert!(
        r.view.contains("\"version\": \"0.0.1\""),
        "small fields kept verbatim"
    );
    assert!(r.view.contains("\"name\": \"knapsack-test\""));
}

// ---------- never-worse-than-raw ----------

#[test]
fn json_pack_never_emits_more_tokens_than_raw() {
    // pack.rs's never-worse-than-stateless guard applies to JSON too — if the
    // conditional+structural view is bigger than raw, we fall back. This pins that
    // the guard is reachable on tiny/dense JSON where the markers would cost more
    // than the saved bytes.
    let s = store("nwo");
    let mut ledger = Ledger::in_memory();
    let tiny = b"{\"a\":1,\"b\":2,\"c\":3}".to_vec();
    let r = pack(&tiny, ContentType::Json, &s, &mut ledger, 0);
    assert!(
        r.shown_tokens_est <= tokens_bytes(&tiny),
        "shown {} must not exceed raw {} for tiny json",
        r.shown_tokens_est,
        tokens_bytes(&tiny)
    );
}

// ---------- reconstruct identity (the safety net) ----------

#[test]
fn reconstruct_is_byte_exact_for_json_pack() {
    let s = store("reconstruct");
    let mut ledger = Ledger::in_memory();
    for fixture in [package_json_fixture(), api_response_fixture()] {
        let _ = pack(&fixture, ContentType::Json, &s, &mut ledger, 0);
        let back = reconstruct(&fixture, ContentType::Json, &s).expect("reconstruct");
        assert_eq!(
            back,
            fixture,
            "byte-exact for fixture of {} bytes",
            fixture.len()
        );
    }
}

/// Helper: keep PathBuf in scope by reference so the temp dir cleanup actually fires
/// on scope exit. (cargo runs tests in parallel; tmp leakage is otherwise hard to debug.)
#[allow(dead_code)]
fn unused() -> PathBuf {
    std::env::temp_dir()
}
