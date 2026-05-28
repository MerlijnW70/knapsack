//! Round-10 first-user UX bug fix: hook matcher includes Read.
//!
//! Pre-fix: `install` subscribed the PreToolUse hook only to `"Bash"`, so
//! Claude Code never invoked `knapsack hook` for Read tool calls. The Read
//! hook code in src/read_hook.rs was wired up but dormant — the README's
//! "input reduction is on by default" contract was silently violated for
//! every fresh install.
//!
//! Post-fix: the canonical matcher is `"Bash|Read"` (regex alternation, the
//! same shape Claude Code's matcher accepts and that other hooks in the
//! wild use). `install --repair` converges stale `"Bash"`-only matchers to
//! the canonical value, so existing users get input reduction back without
//! having to uninstall+reinstall.

mod common;
use common::EnvSandbox;

use knapsack::install::{patch_settings_file, run_checks, Check, Patch, Status};
use knapsack::json::{self, Json};
use std::path::PathBuf;

fn tmpfile(tag: &str) -> PathBuf {
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("kn-matcher-{}-{}-{}", tag, std::process::id(), t))
}

/// Read the `matcher` field of the (single) knapsack PreToolUse entry.
fn extract_knapsack_matcher(settings_path: &PathBuf) -> String {
    let v = json::parse(&std::fs::read_to_string(settings_path).unwrap()).unwrap();
    let pre = v
        .get("hooks")
        .and_then(|h| h.get("PreToolUse"))
        .expect("PreToolUse present");
    let Json::Arr(entries) = pre else {
        panic!("PreToolUse not array")
    };
    for e in entries {
        // The entry is "ours" if its hooks[].command contains "knapsack" + "hook".
        let is_ours = matches!(e.get("hooks"), Some(Json::Arr(hs)) if hs.iter().any(|h| {
            h.get("command").and_then(|c| c.as_str())
                .map(|cmd| cmd.contains("knapsack") && cmd.contains("hook"))
                .unwrap_or(false)
        }));
        if !is_ours {
            continue;
        }
        return e
            .get("matcher")
            .and_then(|m| m.as_str())
            .expect("knapsack entry has matcher")
            .to_string();
    }
    panic!("no knapsack entry found in PreToolUse")
}

#[test]
fn fresh_install_subscribes_to_both_bash_and_read_tools() {
    // Empty settings.json → first install → matcher MUST be "Bash|Read".
    // This is the contract the README ("input reduction is on by default")
    // depends on.
    let _sb = EnvSandbox::new("matcher-fresh");
    let p = tmpfile("fresh.json");
    std::fs::write(&p, "{}").unwrap();
    let r = patch_settings_file(&p, "/bin/knapsack");
    assert!(matches!(r, Ok(Patch::Changed(_))));
    assert_eq!(
        extract_knapsack_matcher(&p),
        "Bash|Read",
        "fresh install must subscribe to both Bash AND Read tools"
    );
    let _ = std::fs::remove_file(&p);
}

#[test]
fn repair_converges_legacy_bash_only_matcher_to_canonical() {
    // The migration case: an existing user has the pre-fix entry with
    // matcher="Bash". `install --repair` must rewrite the matcher to
    // "Bash|Read" so input reduction starts working for them. Without
    // this, the only way to recover input reduction would be to manually
    // uninstall+reinstall — bad UX for an automatic upgrade path.
    let _sb = EnvSandbox::new("matcher-legacy");
    let p = tmpfile("legacy.json");
    // Stale entry: command matches our knapsack predicate, matcher is the
    // pre-fix "Bash" only.
    let legacy = r#"{"hooks":{"PreToolUse":[
        {"matcher":"Bash","hooks":[{"type":"command","command":"\"/old/path/knapsack\" hook"}]}
    ]}}"#;
    std::fs::write(&p, legacy).unwrap();

    let r = patch_settings_file(&p, "/bin/knapsack");
    assert!(matches!(r, Ok(Patch::Changed(_))), "repair detects drift");

    assert_eq!(
        extract_knapsack_matcher(&p),
        "Bash|Read",
        "repair converged stale matcher to canonical"
    );

    // Also confirm the command itself was repointed (the older convergence
    // we already pinned, but worth asserting in the same shape).
    let v = json::parse(&std::fs::read_to_string(&p).unwrap()).unwrap();
    let pre = v.get("hooks").and_then(|h| h.get("PreToolUse")).unwrap();
    if let Json::Arr(a) = pre {
        let cmd = a[0]
            .get("hooks")
            .and_then(|h| match h {
                Json::Arr(hs) => hs.first(),
                _ => None,
            })
            .and_then(|h| h.get("command"))
            .and_then(|c| c.as_str())
            .unwrap();
        assert!(cmd.contains("/bin/knapsack"), "command repointed: {cmd}");
    }
    let _ = std::fs::remove_file(&p);
}

#[test]
fn repair_is_idempotent_when_matcher_already_canonical() {
    // Already at the canonical state → patch returns NoChange. This is the
    // "repair is a no-op when nothing is drifting" promise.
    let _sb = EnvSandbox::new("matcher-noop");
    let p = tmpfile("canonical.json");
    let canonical = r#"{"hooks":{"PreToolUse":[
        {"matcher":"Bash|Read","hooks":[{"type":"command","command":"\"/bin/knapsack\" hook"}]}
    ]}}"#;
    std::fs::write(&p, canonical).unwrap();

    let r = patch_settings_file(&p, "/bin/knapsack");
    assert!(
        matches!(r, Ok(Patch::NoChange)),
        "already-canonical entry → NoChange"
    );
    let _ = std::fs::remove_file(&p);
}

// ----- doctor surfaces matcher drift -----------------------------------------
//
// The matcher-repair tests above prove `install --repair` *can* fix drift. These
// prove `doctor` *tells the user* there is drift — without that, a legacy user
// with `"Bash"`-only sees doctor go green and never runs repair. Doctor pinning
// the matcher closes the loop on the "input reduction is on by default" contract.
//
// We assert on the named check's `status` (the firm contract) plus a stable
// substring of `detail` (the user-readable recovery hint). The rest of doctor's
// text is intentionally NOT pinned — it's wording, not contract.

/// Build a minimal settings.json with a single knapsack PreToolUse entry whose
/// matcher field is `m`, or with the matcher field omitted entirely when `m` is
/// None. The command is a real-looking knapsack path so `entry_is_knapsack`
/// recognizes the entry as ours.
fn settings_with(m: Option<&str>) -> String {
    match m {
        Some(v) => format!(
            r#"{{"hooks":{{"PreToolUse":[{{"matcher":"{}","hooks":[{{"type":"command","command":"\"/bin/knapsack\" hook"}}]}}]}}}}"#,
            v
        ),
        None => r#"{"hooks":{"PreToolUse":[{"hooks":[{"type":"command","command":"\"/bin/knapsack\" hook"}]}]}}"#.to_string(),
    }
}

/// Point `KNAPSACK_SETTINGS` at a temp file containing `settings_json`, run the
/// full doctor checks, and return the `hook matcher` check. Also points
/// `KNAPSACK_MCP_CONFIG` at a non-existent path so unrelated MCP checks don't
/// depend on the developer's real `~/.claude.json`.
fn doctor_matcher_check(settings_json: &str, tag: &str) -> Check {
    let mut sb = EnvSandbox::new(tag);
    let p = tmpfile(tag);
    std::fs::write(&p, settings_json).unwrap();
    sb.set("KNAPSACK_SETTINGS", &p);
    sb.set("KNAPSACK_MCP_CONFIG", sb.join("no-such-mcp.json"));
    let checks = run_checks();
    let ch = checks
        .into_iter()
        .find(|c| c.name == "hook matcher")
        .expect("doctor must emit a `hook matcher` check whenever a knapsack entry exists");
    let _ = std::fs::remove_file(&p);
    ch
}

#[test]
fn doctor_passes_on_canonical_bash_read_matcher() {
    // The healthy state: matcher subscribes to both tool kinds → doctor green.
    let ch = doctor_matcher_check(&settings_with(Some("Bash|Read")), "doctor-canonical");
    assert!(
        matches!(ch.status, Status::Ok),
        "canonical matcher must be Ok; detail was: {}",
        ch.detail
    );
    assert!(
        ch.detail.contains("Bash|Read"),
        "Ok detail should name the matcher so users can see what doctor saw: {}",
        ch.detail
    );
}

#[test]
fn doctor_fails_on_legacy_bash_only_matcher() {
    // The bug this check exists to catch: pre-fix users have `"Bash"` only,
    // doctor used to go green (because check 4 only saw "an entry exists"),
    // and they'd never learn that input reduction was dormant. Fail-loud is
    // the right severity — the install contract says "input AND output
    // reduction active by default".
    let ch = doctor_matcher_check(&settings_with(Some("Bash")), "doctor-bash-only");
    assert!(
        matches!(ch.status, Status::Fail),
        "legacy `Bash`-only matcher must Fail (not Warn); detail was: {}",
        ch.detail
    );
    assert!(
        ch.detail.contains("Bash") && ch.detail.contains("Bash|Read"),
        "Fail detail should name BOTH the offending and expected matchers: {}",
        ch.detail
    );
}

#[test]
fn doctor_fails_on_missing_matcher_field() {
    // Defensive edge: someone hand-deleted the matcher key. Without it the
    // entry will never fire for any tool — doctor must surface this rather
    // than relying on check 4's mere presence test.
    let ch = doctor_matcher_check(&settings_with(None), "doctor-missing");
    assert!(
        matches!(ch.status, Status::Fail),
        "missing matcher must Fail; detail was: {}",
        ch.detail
    );
    assert!(
        ch.detail.contains("matcher"),
        "Fail detail should name what is missing: {}",
        ch.detail
    );
}

#[test]
fn doctor_reports_repair_hint_for_stale_matcher() {
    // A failure with no recovery path is unhelpful. The detail line must point
    // the user at `install --repair` for every Fail mode of this check.
    let bash_only = doctor_matcher_check(&settings_with(Some("Bash")), "doctor-hint-bash");
    assert!(
        bash_only.detail.contains("install --repair"),
        "stale-matcher Fail must name the fix: {}",
        bash_only.detail
    );

    let missing = doctor_matcher_check(&settings_with(None), "doctor-hint-missing");
    assert!(
        missing.detail.contains("install --repair"),
        "missing-matcher Fail must name the fix: {}",
        missing.detail
    );
}

#[test]
fn matcher_missing_field_is_added_back_during_repair() {
    // Defensive edge: someone hand-deleted the matcher field. install --repair
    // should add it back with the canonical value. Without this, the entry
    // would never fire for any tool (Claude Code wouldn't know what to match).
    let _sb = EnvSandbox::new("matcher-missing");
    let p = tmpfile("missing.json");
    let no_matcher = r#"{"hooks":{"PreToolUse":[
        {"hooks":[{"type":"command","command":"\"/bin/knapsack\" hook"}]}
    ]}}"#;
    std::fs::write(&p, no_matcher).unwrap();

    let r = patch_settings_file(&p, "/bin/knapsack");
    assert!(
        matches!(r, Ok(Patch::Changed(_))),
        "missing matcher counts as drift"
    );
    assert_eq!(extract_knapsack_matcher(&p), "Bash|Read");
    let _ = std::fs::remove_file(&p);
}
