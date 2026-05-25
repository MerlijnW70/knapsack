//! Config patching must MERGE (not clobber), be idempotent, back up, and cleanly reverse.
use knapsack::install::{
    mcp_has_server, patch_mcp_file, patch_settings_file, settings_has_hook, unpatch_mcp_file,
    unpatch_settings_file, Patch,
};
use knapsack::json;
use std::io::Write;
use std::path::PathBuf;

fn tmp(tag: &str, contents: Option<&str>) -> PathBuf {
    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    let p = std::env::temp_dir().join(format!("knapsack-inst-{}-{}-{}.json", tag, std::process::id(), t));
    if let Some(c) = contents {
        std::fs::File::create(&p).unwrap().write_all(c.as_bytes()).unwrap();
    }
    p
}

#[test]
fn hook_merges_without_clobbering_and_is_idempotent() {
    // Pre-existing settings with an unrelated model + an unrelated Edit hook.
    let p = tmp(
        "settings",
        Some(r#"{"model":"opus","hooks":{"PreToolUse":[{"matcher":"Edit","hooks":[{"type":"command","command":"echo hi"}]}]}}"#),
    );

    assert!(matches!(patch_settings_file(&p, "/bin/knapsack").unwrap(), Patch::Changed(Some(_))), "first patch changes + backs up");
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
    assert!(matches!(patch_settings_file(&p, "/bin/knapsack").unwrap(), Patch::NoChange));

    // reversible
    assert!(matches!(unpatch_settings_file(&p).unwrap(), Patch::Changed(_)));
    assert!(!settings_has_hook(&p));
    let v = json::parse(&std::fs::read_to_string(&p).unwrap()).unwrap();
    assert_eq!(v.get("model").and_then(|x| x.as_str()), Some("opus"), "unrelated content survives uninstall");
    let _ = std::fs::remove_file(&p);
}

#[test]
fn mcp_merges_and_reverses() {
    let p = tmp("claude", Some(r#"{"mcpServers":{"other":{"command":"x","args":[]}},"numFavorites":3}"#));
    assert!(matches!(patch_mcp_file(&p, "/bin/knapsack").unwrap(), Patch::Changed(Some(_))));
    assert!(mcp_has_server(&p));
    let v = json::parse(&std::fs::read_to_string(&p).unwrap()).unwrap();
    assert!(v.get("mcpServers").and_then(|s| s.get("other")).is_some(), "other server preserved");
    assert_eq!(v.get("numFavorites").and_then(|x| x.as_f64()), Some(3.0), "unrelated key preserved");
    assert!(matches!(patch_mcp_file(&p, "/bin/knapsack").unwrap(), Patch::NoChange));
    assert!(matches!(unpatch_mcp_file(&p).unwrap(), Patch::Changed(_)));
    assert!(!mcp_has_server(&p));
    let _ = std::fs::remove_file(&p);
}

#[test]
fn creates_file_when_absent() {
    let p = tmp("fresh", None); // does not exist
    assert!(matches!(patch_settings_file(&p, "/bin/knapsack").unwrap(), Patch::Changed(None)), "no backup when file is new");
    assert!(settings_has_hook(&p));
    let _ = std::fs::remove_file(&p);
}

#[test]
fn unparseable_config_is_left_untouched() {
    let p = tmp("broken", Some("{ this is not json"));
    let before = std::fs::read_to_string(&p).unwrap();
    assert!(patch_settings_file(&p, "/bin/knapsack").is_err(), "must refuse to write a config it can't parse");
    assert_eq!(std::fs::read_to_string(&p).unwrap(), before, "file left exactly as-is");
    let _ = std::fs::remove_file(&p);
}
