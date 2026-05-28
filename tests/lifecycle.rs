//! Config patching must MERGE (not clobber), be idempotent, back up, and cleanly reverse.
use knapsack::install::{
    apply, hook_binary, mcp_config_path, mcp_has_server, patch_mcp_file, patch_settings_file,
    settings_has_hook, settings_path, unpatch_mcp_file, unpatch_settings_file, Patch,
};
use knapsack::json;
use std::io::Write;
use std::path::PathBuf;

fn tmp(tag: &str, contents: Option<&str>) -> PathBuf {
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let p = std::env::temp_dir().join(format!(
        "knapsack-inst-{}-{}-{}.json",
        tag,
        std::process::id(),
        t
    ));
    if let Some(c) = contents {
        std::fs::File::create(&p)
            .unwrap()
            .write_all(c.as_bytes())
            .unwrap();
    }
    p
}

#[test]
fn hook_merges_without_clobbering_and_is_idempotent() {
    // Pre-existing settings with an unrelated model + an unrelated Edit hook.
    let p = tmp(
        "settings",
        Some(
            r#"{"model":"opus","hooks":{"PreToolUse":[{"matcher":"Edit","hooks":[{"type":"command","command":"echo hi"}]}]}}"#,
        ),
    );

    assert!(
        matches!(
            patch_settings_file(&p, "/bin/knapsack").unwrap(),
            Patch::Changed(Some(_))
        ),
        "first patch changes + backs up"
    );
    assert!(settings_has_hook(&p));

    // unrelated content preserved
    let v = json::parse(&std::fs::read_to_string(&p).unwrap()).unwrap();
    assert_eq!(v.get("model").and_then(|x| x.as_str()), Some("opus"));
    let pre = v.get("hooks").and_then(|h| h.get("PreToolUse")).unwrap();
    if let json::Json::Arr(a) = pre {
        assert_eq!(a.len(), 2, "Edit hook kept, Bash knapsack hook added");
    } else {
        panic!();
    }

    // idempotent
    assert!(matches!(
        patch_settings_file(&p, "/bin/knapsack").unwrap(),
        Patch::NoChange
    ));

    // reversible
    assert!(matches!(
        unpatch_settings_file(&p).unwrap(),
        Patch::Changed(_)
    ));
    assert!(!settings_has_hook(&p));
    let v = json::parse(&std::fs::read_to_string(&p).unwrap()).unwrap();
    assert_eq!(
        v.get("model").and_then(|x| x.as_str()),
        Some("opus"),
        "unrelated content survives uninstall"
    );
    let _ = std::fs::remove_file(&p);
}

#[test]
fn repoints_a_stale_hook_instead_of_no_oping() {
    // A knapsack hook already exists but points at a STALE absolute path, alongside an
    // unrelated Edit hook. Re-patching with a new bin must rewrite the stale command in
    // place (not add a second one, not leave it alone), and must leave the Edit hook be.
    let p = tmp(
        "stale",
        Some(concat!(
            r#"{"hooks":{"PreToolUse":["#,
            r#"{"matcher":"Edit","hooks":[{"type":"command","command":"echo hi"}]},"#,
            r#"{"matcher":"Bash","hooks":[{"type":"command","command":"\"H:/old/knapsack-rs/target/release/knapsack.exe\" hook"}]}"#,
            r#"]}}"#,
        )),
    );

    assert_eq!(
        hook_binary(&p).as_deref(),
        Some("H:/old/knapsack-rs/target/release/knapsack.exe"),
        "starts stale"
    );

    // converge to the canonical bin
    assert!(
        matches!(
            patch_settings_file(&p, "C:/Users/me/.knapsack/bin/knapsack.exe").unwrap(),
            Patch::Changed(Some(_))
        ),
        "stale path must be repointed + backed up"
    );
    assert_eq!(
        hook_binary(&p).as_deref(),
        Some("C:/Users/me/.knapsack/bin/knapsack.exe"),
        "hook now points at the canonical binary"
    );

    // the Edit hook survived and no duplicate knapsack hook was appended
    let v = json::parse(&std::fs::read_to_string(&p).unwrap()).unwrap();
    if let json::Json::Arr(a) = v.get("hooks").and_then(|h| h.get("PreToolUse")).unwrap() {
        assert_eq!(
            a.len(),
            2,
            "Edit hook kept, knapsack hook rewritten in place (not duplicated)"
        );
    } else {
        panic!("PreToolUse not an array");
    }

    // already canonical -> NoChange
    assert!(
        matches!(
            patch_settings_file(&p, "C:/Users/me/.knapsack/bin/knapsack.exe").unwrap(),
            Patch::NoChange
        ),
        "no-op once canonical"
    );
    let _ = std::fs::remove_file(&p);
}

#[test]
fn mcp_merges_and_reverses() {
    let p = tmp(
        "claude",
        Some(r#"{"mcpServers":{"other":{"command":"x","args":[]}},"numFavorites":3}"#),
    );
    assert!(matches!(
        patch_mcp_file(&p, "/bin/knapsack").unwrap(),
        Patch::Changed(Some(_))
    ));
    assert!(mcp_has_server(&p));
    let v = json::parse(&std::fs::read_to_string(&p).unwrap()).unwrap();
    assert!(
        v.get("mcpServers").and_then(|s| s.get("other")).is_some(),
        "other server preserved"
    );
    assert_eq!(
        v.get("numFavorites").and_then(|x| x.as_f64()),
        Some(3.0),
        "unrelated key preserved"
    );
    assert!(matches!(
        patch_mcp_file(&p, "/bin/knapsack").unwrap(),
        Patch::NoChange
    ));
    assert!(matches!(unpatch_mcp_file(&p).unwrap(), Patch::Changed(_)));
    assert!(!mcp_has_server(&p));
    let _ = std::fs::remove_file(&p);
}

#[test]
fn creates_file_when_absent() {
    let p = tmp("fresh", None); // does not exist
    assert!(
        matches!(
            patch_settings_file(&p, "/bin/knapsack").unwrap(),
            Patch::Changed(None)
        ),
        "no backup when file is new"
    );
    assert!(settings_has_hook(&p));
    let _ = std::fs::remove_file(&p);
}

#[test]
fn unparseable_config_is_left_untouched() {
    let p = tmp("broken", Some("{ this is not json"));
    let before = std::fs::read_to_string(&p).unwrap();
    assert!(
        patch_settings_file(&p, "/bin/knapsack").is_err(),
        "must refuse to write a config it can't parse"
    );
    assert_eq!(
        std::fs::read_to_string(&p).unwrap(),
        before,
        "file left exactly as-is"
    );
    let _ = std::fs::remove_file(&p);
}

#[test]
fn rapid_patch_cycles_do_not_clobber_each_others_backups() {
    // `now_secs()` is 1-second resolution. Before the collision-walk fix, an
    // install → uninstall → install in <1s landed three backups on the same
    // filename — every later one overwrote the earlier, dropping rollback
    // state silently. The user's only recovery path was gone. Now we walk
    // `_2`, `_3`, … until we find a free name. Eight rapid patch+unpatch
    // cycles should produce sixteen distinct backup files.
    let p = tmp("rapid", Some(r#"{"model":"original"}"#));
    let bin = "/bin/knapsack";
    let mut backups: Vec<PathBuf> = Vec::new();
    for _ in 0..8 {
        if let Ok(Patch::Changed(Some(b))) = patch_settings_file(&p, bin) {
            backups.push(b);
        }
        if let Ok(Patch::Changed(Some(b))) = unpatch_settings_file(&p) {
            backups.push(b);
        }
    }
    assert_eq!(backups.len(), 16, "every patch must produce a backup");
    // Each path must be UNIQUE — that's the actual invariant the fix exists for.
    let mut sorted = backups.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(
        sorted.len(),
        backups.len(),
        "no two backups may share a filename"
    );
    // Every backup file must still EXIST on disk (i.e. nothing clobbered).
    for b in &backups {
        assert!(
            b.exists(),
            "backup must survive subsequent patches: {}",
            b.display()
        );
    }
    // Cleanup.
    for b in backups {
        let _ = std::fs::remove_file(b);
    }
    let _ = std::fs::remove_file(&p);
}

#[test]
fn uninstall_prunes_empty_containers_left_behind() {
    // After a clean install→uninstall on an EMPTY starting file, we used to
    // leave `{"hooks":{"PreToolUse":[]}}` and `{"mcpServers":{}}` in the file
    // — cosmetic but visibly noisy in a user's config. The pruning step in
    // `remove_hook`/`remove_mcp` now drops empty scaffolding so the post-
    // uninstall file matches the pre-install file as closely as possible.
    let s = tmp("scaffold-settings", Some("{}"));
    let m = tmp("scaffold-mcp", Some("{}"));
    let bin = "/bin/knapsack";

    assert!(patch_settings_file(&s, bin).is_ok());
    assert!(patch_mcp_file(&m, bin).is_ok());

    assert!(matches!(
        unpatch_settings_file(&s).unwrap(),
        Patch::Changed(_)
    ));
    assert!(matches!(unpatch_mcp_file(&m).unwrap(), Patch::Changed(_)));

    let sv = json::parse(&std::fs::read_to_string(&s).unwrap()).unwrap();
    let mv = json::parse(&std::fs::read_to_string(&m).unwrap()).unwrap();

    assert!(
        sv.get("hooks").is_none(),
        "empty hooks scaffold pruned, got {sv:?}"
    );
    assert!(
        mv.get("mcpServers").is_none(),
        "empty mcpServers scaffold pruned, got {mv:?}"
    );

    let _ = std::fs::remove_file(&s);
    let _ = std::fs::remove_file(&m);
}

#[test]
fn uninstall_does_not_prune_user_data() {
    // Counter-test for the pruning fix: when an UNRELATED Edit hook or MCP server
    // lives in the same container, uninstall must leave the container in place.
    // Otherwise the prune logic would happily erase the user's other tooling
    // every time they uninstalled knapsack.
    let s = tmp(
        "user-data-settings",
        Some(
            r#"{"hooks":{"PreToolUse":[{"matcher":"Edit","hooks":[{"type":"command","command":"echo edit"}]}]}}"#,
        ),
    );
    let m = tmp(
        "user-data-mcp",
        Some(r#"{"mcpServers":{"cavewoman":{"command":"node","args":["x"]}}}"#),
    );
    let bin = "/bin/knapsack";

    assert!(patch_settings_file(&s, bin).is_ok());
    assert!(patch_mcp_file(&m, bin).is_ok());

    assert!(matches!(
        unpatch_settings_file(&s).unwrap(),
        Patch::Changed(_)
    ));
    assert!(matches!(unpatch_mcp_file(&m).unwrap(), Patch::Changed(_)));

    let sv = json::parse(&std::fs::read_to_string(&s).unwrap()).unwrap();
    let mv = json::parse(&std::fs::read_to_string(&m).unwrap()).unwrap();

    let pre = sv.get("hooks").and_then(|h| h.get("PreToolUse"));
    assert!(
        matches!(pre, Some(json::Json::Arr(a)) if a.len() == 1),
        "user's Edit hook must survive"
    );
    assert!(
        mv.get("mcpServers")
            .and_then(|s| s.get("cavewoman"))
            .is_some(),
        "user's cavewoman MCP must survive"
    );

    let _ = std::fs::remove_file(&s);
    let _ = std::fs::remove_file(&m);
}

#[test]
fn apply_returns_success_false_when_patch_fails() {
    // Failure exit-code propagation. Before the ApplyResult struct, `apply()`
    // returned only the human transcript — an automated installer or CI pipeline
    // saw exit code 0 even when both patches failed. Now `apply()` reports
    // `success=false` and main.rs exits 1. We force failure by pointing
    // KNAPSACK_SETTINGS at an unparseable file, then verify the result reflects
    // it. (Adversarial environment is necessarily test-local; we clear the env
    // vars on the way out.)
    let bad_settings = tmp("bad-settings", Some("not json at all"));
    let fresh_mcp = tmp("fresh-mcp", None);
    let store_dir = std::env::temp_dir().join(format!("kn-test-store-{}", std::process::id()));
    let metrics =
        std::env::temp_dir().join(format!("kn-test-metrics-{}.jsonl", std::process::id()));
    let sessions = std::env::temp_dir().join(format!("kn-test-sessions-{}", std::process::id()));

    // SAFETY: these env vars are scoped to this single test process. Other tests
    // that touch the same vars use the same pattern; cargo runs each integration
    // test binary in its own process so cross-test interference is not a concern.
    std::env::set_var("KNAPSACK_SETTINGS", &bad_settings);
    std::env::set_var("KNAPSACK_MCP_CONFIG", &fresh_mcp);
    std::env::set_var("KNAPSACK_STORE", &store_dir);
    std::env::set_var("KNAPSACK_METRICS", &metrics);
    std::env::set_var("KNAPSACK_SESSIONS", &sessions);

    // Sanity: confirm the env override is reachable through the canonical accessor.
    assert_eq!(
        settings_path(),
        bad_settings,
        "test env override must reach apply()"
    );
    assert_eq!(mcp_config_path(), fresh_mcp);

    let result = apply();
    assert!(
        !result.success,
        "apply() must report failure when a patch errored"
    );
    assert!(
        result.output.contains("✗ hook NOT patched"),
        "transcript still shows the ✗ line:\n{}",
        result.output
    );

    std::env::remove_var("KNAPSACK_SETTINGS");
    std::env::remove_var("KNAPSACK_MCP_CONFIG");
    std::env::remove_var("KNAPSACK_STORE");
    std::env::remove_var("KNAPSACK_METRICS");
    std::env::remove_var("KNAPSACK_SESSIONS");

    let _ = std::fs::remove_file(&bad_settings);
    let _ = std::fs::remove_file(&fresh_mcp);
    let _ = std::fs::remove_dir_all(&store_dir);
    let _ = std::fs::remove_file(&metrics);
    let _ = std::fs::remove_dir_all(&sessions);
}
