//! Config patching against ADVERSARIAL-but-valid JSON. The promise is "merge, never clobber a
//! file we can't safely patch". A wrong top-level SHAPE (array/string/number) must be refused
//! and left untouched (like an unparseable file); within a valid object, unrelated keys must
//! always survive and the knapsack entry must end up present, without panicking.
//!
//! (File deliberately NOT named *install*/*setup*/*patch*: Windows' UAC installer-detection
//! heuristic refuses to launch a test binary whose name contains those words — os error 740.)

use knapsack::install::{mcp_has_server, patch_mcp_file, patch_settings_file, settings_has_hook};
use knapsack::json;
use std::io::Write;
use std::path::PathBuf;

fn tmp(tag: &str, contents: &str) -> PathBuf {
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let p = std::env::temp_dir().join(format!(
        "knapsack-adv-{}-{}-{}.json",
        tag,
        std::process::id(),
        t
    ));
    std::fs::File::create(&p)
        .unwrap()
        .write_all(contents.as_bytes())
        .unwrap();
    p
}

#[test]
fn refuses_non_object_root_and_leaves_it_untouched() {
    for (tag, body) in [
        ("arr", "[1,2,3]"),
        ("str", "\"just a string\""),
        ("num", "42"),
        ("bool", "true"),
    ] {
        let p = tmp(tag, body);
        let before = std::fs::read_to_string(&p).unwrap();
        assert!(
            patch_settings_file(&p, "/bin/knapsack").is_err(),
            "{tag}: must refuse a non-object root"
        );
        assert_eq!(
            std::fs::read_to_string(&p).unwrap(),
            before,
            "{tag}: file must be left exactly as-is"
        );
        let _ = std::fs::remove_file(&p);
    }
}

#[test]
fn wrong_typed_hooks_field_does_not_lose_other_keys() {
    // `hooks` is a string (malformed), but `model` is real user data that must survive.
    let p = tmp(
        "wronghooks",
        r#"{"model":"opus","hooks":"oops not an object","theme":"dark"}"#,
    );
    assert!(
        patch_settings_file(&p, "/bin/knapsack").is_ok(),
        "a valid object with a wrong-typed field must still patch"
    );
    assert!(settings_has_hook(&p), "knapsack hook added");
    let v = json::parse(&std::fs::read_to_string(&p).unwrap()).unwrap();
    assert_eq!(
        v.get("model").and_then(|x| x.as_str()),
        Some("opus"),
        "unrelated key preserved"
    );
    assert_eq!(
        v.get("theme").and_then(|x| x.as_str()),
        Some("dark"),
        "unrelated key preserved"
    );
    let _ = std::fs::remove_file(&p);
}

#[test]
fn wrong_typed_pretooluse_is_replaced_not_panicked() {
    // PreToolUse is an object instead of an array; patch must recover (replace it), not panic.
    let p = tmp(
        "wrongpre",
        r#"{"hooks":{"PreToolUse":{"weird":1}},"keep":7}"#,
    );
    assert!(patch_settings_file(&p, "/bin/knapsack").is_ok());
    assert!(settings_has_hook(&p));
    let v = json::parse(&std::fs::read_to_string(&p).unwrap()).unwrap();
    assert_eq!(
        v.get("keep").and_then(|x| x.as_f64()),
        Some(7.0),
        "unrelated key preserved"
    );
    let _ = std::fs::remove_file(&p);
}

#[test]
fn wrong_typed_mcpservers_field_preserves_siblings() {
    let p = tmp(
        "wrongmcp",
        r#"{"mcpServers":42,"numFavorites":3,"nested":{"a":[1,2]}}"#,
    );
    assert!(patch_mcp_file(&p, "/bin/knapsack").is_ok());
    assert!(mcp_has_server(&p));
    let v = json::parse(&std::fs::read_to_string(&p).unwrap()).unwrap();
    assert_eq!(v.get("numFavorites").and_then(|x| x.as_f64()), Some(3.0));
    assert!(
        v.get("nested").and_then(|n| n.get("a")).is_some(),
        "deeply nested unrelated data preserved"
    );
    let _ = std::fs::remove_file(&p);
}

#[test]
fn empty_object_gets_patched() {
    let p = tmp("empty", "{}");
    assert!(patch_settings_file(&p, "/bin/knapsack").is_ok());
    assert!(settings_has_hook(&p));
    let _ = std::fs::remove_file(&p);
}

#[test]
fn utf8_bom_is_stripped_and_file_patches_normally() {
    // Real-world #1 failure mode on Windows: PowerShell 5.1's default
    // `Set-Content -Encoding utf8` writes a UTF-8 BOM. Notepad does too. Our
    // strict JSON parser would otherwise choke on the leading U+FEFF with an
    // opaque "unexpected Some('\u{feff}')" error and refuse to patch — a user
    // would see "✗ hook NOT patched" with no idea what to do. The fix in
    // install.rs::strip_bom normalises BOM-prefixed UTF-8 to plain UTF-8 on
    // read, and the patched file is re-serialized BOM-less.
    let bom = "\u{feff}";
    let p = tmp("bom-valid", &format!(r#"{}{{"model":"opus"}}"#, bom));
    assert!(
        patch_settings_file(&p, "/bin/knapsack").is_ok(),
        "BOM must not block patching"
    );
    assert!(settings_has_hook(&p), "hook entry added");
    let txt = std::fs::read_to_string(&p).unwrap();
    assert!(
        !txt.starts_with('\u{feff}'),
        "output normalised to BOM-less UTF-8"
    );
    // Parse confirms the BOM didn't poison the round-trip.
    let v = json::parse(&txt).unwrap();
    assert_eq!(
        v.get("model").and_then(|x| x.as_str()),
        Some("opus"),
        "unrelated content survives"
    );
    let _ = std::fs::remove_file(&p);
}

#[test]
fn bom_only_file_is_treated_as_empty_object() {
    // A file containing nothing but a BOM is the degenerate case (e.g. an editor
    // created the file but the user never typed anything). Treat it the same as
    // an empty file — initialise to `{}` and patch.
    let p = tmp("bom-only", "\u{feff}");
    assert!(patch_settings_file(&p, "/bin/knapsack").is_ok());
    assert!(settings_has_hook(&p));
    let _ = std::fs::remove_file(&p);
}

#[test]
fn bom_followed_by_invalid_json_humanizes_the_error() {
    // BOM + junk: we still can't patch, but the user MUST get a sentence they can
    // act on instead of `unexpected Some('\u{feff}')` jargon. Confirms the
    // humanizer triggers on the parser's downstream error (the BOM itself is
    // stripped, but the remaining content still doesn't parse).
    let p = tmp("bom-junk", "\u{feff}{ this is not json");
    let err = patch_settings_file(&p, "/bin/knapsack")
        .err()
        .expect("must refuse");
    assert!(
        err.contains("trailing commas") || err.contains("comments") || err.contains("UTF-16"),
        "error must point users at common causes; got: {err}",
    );
    let _ = std::fs::remove_file(&p);
}
