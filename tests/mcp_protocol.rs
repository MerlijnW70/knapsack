//! MCP JSON-RPC protocol conformance beyond the happy path: unparseable lines and
//! notifications produce no response; requests always do; the `id` is echoed with its type
//! preserved; unknown methods/tools and missing/wrong-typed args fail gracefully; and EVERY
//! emitted response is valid JSON tagged jsonrpc 2.0. One test fn (process-global env).

use knapsack::json::{self, Json};
use knapsack::mcp::handle_message;

#[test]
fn mcp_protocol_conformance_edges() {
    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    let dir = std::env::temp_dir().join(format!("knapsack-mcpproto-{}-{}", std::process::id(), t));
    std::env::set_var("KNAPSACK_STORE", dir.join("store"));
    std::env::set_var("KNAPSACK_METRICS", dir.join("m.jsonl"));

    // Unparseable / empty input -> None (fail quiet, never crash the stream).
    assert!(handle_message("not json at all").is_none());
    assert!(handle_message("").is_none());
    assert!(handle_message("{\"oops\":").is_none());

    // Notifications (no id) -> no response, for both known and unknown methods.
    assert!(handle_message(r#"{"method":"notifications/initialized"}"#).is_none());
    assert!(handle_message(r#"{"method":"some/unknown/notification"}"#).is_none());

    // Unknown method WITH id -> JSON-RPC -32601.
    let r = handle_message(r#"{"id":9,"method":"no/such"}"#).unwrap();
    assert!(r.contains("-32601") && r.contains("\"error\""));

    // id type is preserved: number stays number, string stays string, absent -> null.
    let v = json::parse(&handle_message(r#"{"id":7,"method":"tools/list"}"#).unwrap()).unwrap();
    assert_eq!(v.get("id").and_then(|x| x.as_f64()), Some(7.0));
    let v = json::parse(&handle_message(r#"{"id":"abc-1","method":"tools/list"}"#).unwrap()).unwrap();
    assert_eq!(v.get("id").and_then(|x| x.as_str()), Some("abc-1"));
    let v = json::parse(&handle_message(r#"{"method":"initialize"}"#).unwrap()).unwrap();
    assert!(matches!(v.get("id"), Some(Json::Null)), "absent id -> null in the response");

    // tools/call: unknown tool, missing name, missing params -> graceful error (no panic).
    assert!(handle_message(r#"{"id":1,"method":"tools/call","params":{"name":"nope","arguments":{}}}"#).unwrap().contains("Unknown tool"));
    assert!(handle_message(r#"{"id":2,"method":"tools/call","params":{"arguments":{}}}"#).unwrap().contains("-32601"));
    assert!(handle_message(r#"{"id":3,"method":"tools/call"}"#).unwrap().contains("-32601"));

    // expand: missing required handle, and wrong-typed handle -> isError text result.
    let r = handle_message(r#"{"id":4,"method":"tools/call","params":{"name":"knapsack_expand","arguments":{}}}"#).unwrap();
    assert!(r.contains("\"isError\":true") && r.contains("'handle' is required"));
    let r = handle_message(r#"{"id":5,"method":"tools/call","params":{"name":"knapsack_expand","arguments":{"handle":123}}}"#).unwrap();
    assert!(r.contains("\"isError\":true"), "numeric handle (wrong type) -> treated as missing, not a panic");

    // expand: 'lines' present but malformed -> isError, not a silent whole-file expand.
    let r = handle_message(r#"{"id":51,"method":"tools/call","params":{"name":"knapsack_expand","arguments":{"handle":"ks2_0123456789abcdef0123456789abcdef","lines":"garbage"}}}"#).unwrap();
    assert!(r.contains("\"isError\":true") && r.contains("'lines'"), "malformed lines must isError: {r}");
    // expand: 'context' present but wrong type -> isError. (Used to silently become 0.)
    let r = handle_message(r#"{"id":52,"method":"tools/call","params":{"name":"knapsack_expand","arguments":{"handle":"ks2_0123456789abcdef0123456789abcdef","context":"abc"}}}"#).unwrap();
    assert!(r.contains("\"isError\":true") && r.contains("'context'"), "wrong-typed context must isError: {r}");
    // expand: 'context' present and negative -> isError. (Used to be clamped to 0 silently.)
    let r = handle_message(r#"{"id":53,"method":"tools/call","params":{"name":"knapsack_expand","arguments":{"handle":"ks2_0123456789abcdef0123456789abcdef","context":-3}}}"#).unwrap();
    assert!(r.contains("\"isError\":true") && r.contains("'context'"), "negative context must isError: {r}");

    // metrics tool with empty args is fine.
    assert!(handle_message(r#"{"id":6,"method":"tools/call","params":{"name":"knapsack_metrics","arguments":{}}}"#).unwrap().contains("knapsack live stats"));

    // EVERY response across a barrage must be valid JSON tagged jsonrpc 2.0.
    let probes = [
        r#"{"id":1,"method":"initialize"}"#,
        r#"{"id":2,"method":"tools/list"}"#,
        r#"{"id":3,"method":"bogus"}"#,
        r#"{"id":4,"method":"tools/call","params":{"name":"knapsack_inspect","arguments":{"handle":"ks_nope000000"}}}"#,
        r#"{"id":"s","method":"tools/call","params":{"name":"knapsack_metrics","arguments":{}}}"#,
    ];
    for p in probes {
        let resp = handle_message(p).expect("request must get a response");
        let v = json::parse(&resp).unwrap_or_else(|e| panic!("server emitted invalid JSON for {p}: {e}"));
        assert_eq!(v.get("jsonrpc").and_then(|x| x.as_str()), Some("2.0"), "every response is jsonrpc 2.0: {resp}");
        assert!(v.get("id").is_some(), "every response echoes an id field");
    }
}
