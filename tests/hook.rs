//! Locks the Claude Code PreToolUse contract end-to-end (minus stdin/stdout plumbing):
//! parse a realistic event, decide, rewrite the command, and confirm the emitted envelope
//! is the exact shape Claude Code expects.
use knapsack::hook::{decide, wrap_command};
use knapsack::json::{self, Json};

fn event(cmd: &str) -> String {
    // Mirrors a real PreToolUse payload, including escaped quotes in the command.
    format!(
        r#"{{"tool_name":"Bash","session_id":"sess-42","cwd":"/repo","tool_input":{{"command":"{}","description":"x"}}}}"#,
        cmd.replace('"', "\\\"")
    )
}

#[test]
fn end_to_end_envelope_is_correct() {
    let raw = event("cargo test");
    let evt = json::parse(&raw).unwrap();
    assert_eq!(evt.get("tool_name").and_then(|v| v.as_str()), Some("Bash"));

    let ti = evt.get("tool_input").unwrap();
    let cmd = ti.get("command").and_then(|v| v.as_str()).unwrap();
    let session = evt.get("session_id").and_then(|v| v.as_str()).unwrap();

    let d = decide(cmd);
    assert!(d.wrap && d.matched.as_deref() == Some("cargo"));

    let wrapped = wrap_command(cmd, "/bin/knapsack", session, "cargo", None);
    assert!(wrapped.contains("cargo test"));
    assert!(wrapped.contains(r#"pack - --session "sess-42""#));

    // Build the updatedInput envelope the way the hook does and re-serialize it.
    let mut obj = if let Json::Obj(o) = ti { o.clone() } else { panic!() };
    json::set_key(&mut obj, "command", Json::Str(wrapped));
    let out = Json::Obj(vec![(
        "hookSpecificOutput".into(),
        Json::Obj(vec![
            ("hookEventName".into(), Json::Str("PreToolUse".into())),
            ("updatedInput".into(), Json::Obj(obj)),
        ]),
    )]);
    let s = json::to_string(&out);

    // It must re-parse, preserve the untouched field, and carry the rewritten command.
    let back = json::parse(&s).unwrap();
    let ui = back.get("hookSpecificOutput").and_then(|h| h.get("updatedInput")).unwrap();
    assert_eq!(ui.get("description").and_then(|v| v.as_str()), Some("x"));
    assert!(ui.get("command").and_then(|v| v.as_str()).unwrap().contains("pack -"));
    assert_eq!(
        back.get("hookSpecificOutput").and_then(|h| h.get("hookEventName")).and_then(|v| v.as_str()),
        Some("PreToolUse")
    );
}

#[test]
fn shell_meta_is_left_alone() {
    for c in ["npm test | tail -5", "cargo build 2>&1", "pytest &", "ls"] {
        assert!(!decide(c).wrap, "should not wrap: {}", c);
    }
}
