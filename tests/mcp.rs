//! Drives the MCP dispatch directly (no real stdio) to lock the JSON-RPC contract and the
//! three tools. One test fn so the process-global store/metrics env vars don't race.
use knapsack::json::{self, Json};
use knapsack::mcp::handle_message;
use knapsack::{config, Store};

fn text_of(resp: &str) -> String {
    let v = json::parse(resp).unwrap();
    let content = v.get("result").and_then(|r| r.get("content")).unwrap();
    if let Json::Arr(a) = content {
        a[0].get("text")
            .and_then(|t| t.as_str())
            .unwrap()
            .to_string()
    } else {
        panic!("no content")
    }
}

#[test]
fn mcp_protocol_and_tools() {
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("knapsack-mcp-{}-{}", std::process::id(), t));
    std::env::set_var("KNAPSACK_STORE", dir.join("store"));
    std::env::set_var("KNAPSACK_METRICS", dir.join("m.jsonl"));

    // initialize
    let r = handle_message(r#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#).unwrap();
    let v = json::parse(&r).unwrap();
    assert_eq!(
        v.get("result")
            .and_then(|x| x.get("protocolVersion"))
            .and_then(|x| x.as_str()),
        Some("2024-11-05")
    );
    assert_eq!(
        v.get("result")
            .and_then(|x| x.get("serverInfo"))
            .and_then(|x| x.get("name"))
            .and_then(|x| x.as_str()),
        Some("knapsack")
    );

    // notification -> no response
    assert!(handle_message(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#).is_none());

    // tools/list -> exactly the three tools
    let r = handle_message(r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#).unwrap();
    let v = json::parse(&r).unwrap();
    if let Some(Json::Arr(a)) = v.get("result").and_then(|x| x.get("tools")) {
        assert_eq!(a.len(), 3);
    } else {
        panic!("tools not an array");
    }
    assert!(
        r.contains("knapsack_expand")
            && r.contains("knapsack_inspect")
            && r.contains("knapsack_metrics")
    );

    // seed a handle
    let store = Store::new(config::store_dir());
    let h = store.put(b"alpha\nbeta\ngamma\ndelta");

    // full expand -> byte-exact text
    let call = format!(
        r#"{{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{{"name":"knapsack_expand","arguments":{{"handle":"{}"}}}}}}"#,
        h
    );
    assert_eq!(
        text_of(&handle_message(&call).unwrap()),
        "alpha\nbeta\ngamma\ndelta"
    );

    // grep + context -> beta and its neighbours, not delta
    let call = format!(
        r#"{{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{{"name":"knapsack_expand","arguments":{{"handle":"{}","grep":"beta","context":1}}}}}}"#,
        h
    );
    let t = text_of(&handle_message(&call).unwrap());
    assert_eq!(t, "alpha\nbeta\ngamma");

    // inspect
    let call = format!(
        r#"{{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{{"name":"knapsack_inspect","arguments":{{"handle":"{}"}}}}}}"#,
        h
    );
    assert!(text_of(&handle_message(&call).unwrap()).contains("4 lines"));

    // unknown handle -> isError
    let r = handle_message(
        r#"{"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"knapsack_expand","arguments":{"handle":"ks_nope"}}}"#,
    )
    .unwrap();
    assert!(r.contains("\"isError\":true"));

    // metrics tool
    let r = handle_message(r#"{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"knapsack_metrics","arguments":{}}}"#).unwrap();
    assert!(text_of(&r).contains("knapsack live stats"));

    // unknown method on a request -> JSON-RPC error
    let r = handle_message(r#"{"jsonrpc":"2.0","id":8,"method":"no/such"}"#).unwrap();
    assert!(r.contains("\"error\"") && r.contains("-32601"));
}
