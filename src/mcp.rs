//! MCP stdio server (JSON-RPC 2.0, newline-delimited, zero-dep). Makes Knapsack recall
//! ergonomic for Claude — the hook compresses output and leaves `ks_...` handles; these
//! tools turn a handle back into content WITHOUT a shell-out. Mirrors Rucksack's proven
//! protocol (initialize / tools/list / tools/call, protocol 2024-11-05).
//!
//! Tools:
//!   knapsack_expand(handle, lines?, grep?, context?)  recall the slice you need
//!   knapsack_inspect(handle)                          metadata + preview, no full dump
//!   knapsack_metrics(session_id?)                     the savings scoreboard

use crate::api::{expand_handle, ExpandRequest};
use crate::config;
use crate::json::{self, Json};
use crate::metrics::ExpandCaller;
use crate::recall::{parse_range, RecallOut};
use crate::store::Store;
use crate::{block, metrics, token_estimate};
use std::io::{BufRead, Write};

const PROTOCOL: &str = "2024-11-05";
// Single source of truth: tracks Cargo.toml's version, so the MCP handshake can't drift.
const VERSION: &str = env!("CARGO_PKG_VERSION");
const INSTRUCTIONS: &str = "Knapsack recall: the PreToolUse hook compresses noisy command output and leaves ks2_<hex> handles (legacy ks_<hex> from older stores still works). \
Use the compact view first — it keeps errors, signatures, and structure, which is usually enough. \
When you must recall, expand only the slice you need: knapsack_expand(handle, {lines:\"40-60\"}) or {grep:\"pattern\", context:2}. \
Pulling a whole region back spends the tokens the hook just saved (knapsack_metrics shows net_saved go negative if you over-expand). \
Use knapsack_inspect(handle) to size a region before deciding.";

// ---------- JSON-RPC envelope helpers ----------
fn reply(id: Option<Json>, result: Json) -> String {
    json::to_string(&Json::Obj(vec![
        ("jsonrpc".into(), Json::Str("2.0".into())),
        ("id".into(), id.unwrap_or(Json::Null)),
        ("result".into(), result),
    ]))
}

fn rpc_error(id: Option<Json>, code: i64, message: &str) -> String {
    json::to_string(&Json::Obj(vec![
        ("jsonrpc".into(), Json::Str("2.0".into())),
        ("id".into(), id.unwrap_or(Json::Null)),
        (
            "error".into(),
            Json::Obj(vec![
                ("code".into(), Json::Num(code as f64)),
                ("message".into(), Json::Str(message.into())),
            ]),
        ),
    ]))
}

fn text_result(id: Option<Json>, text: String, is_error: bool) -> String {
    reply(
        id,
        Json::Obj(vec![
            (
                "content".into(),
                Json::Arr(vec![Json::Obj(vec![
                    ("type".into(), Json::Str("text".into())),
                    ("text".into(), Json::Str(text)),
                ])]),
            ),
            ("isError".into(), Json::Bool(is_error)),
        ]),
    )
}

// ---------- tool catalog ----------
fn prop(name: &str, ty: &str, desc: &str) -> (String, Json) {
    (
        name.into(),
        Json::Obj(vec![
            ("type".into(), Json::Str(ty.into())),
            ("description".into(), Json::Str(desc.into())),
        ]),
    )
}

fn tool(name: &str, desc: &str, props: Vec<(String, Json)>, required: &[&str]) -> Json {
    Json::Obj(vec![
        ("name".into(), Json::Str(name.into())),
        ("description".into(), Json::Str(desc.into())),
        (
            "inputSchema".into(),
            Json::Obj(vec![
                ("type".into(), Json::Str("object".into())),
                ("properties".into(), Json::Obj(props)),
                (
                    "required".into(),
                    Json::Arr(required.iter().map(|r| Json::Str((*r).into())).collect()),
                ),
            ]),
        ),
    ])
}

fn tools() -> Json {
    Json::Arr(vec![
        tool(
            "knapsack_expand",
            "Restore content behind a Knapsack recall handle (`ks2_<32 hex>`, or legacy `ks_<10|16 hex>` from older stores). Returns the full region by default (byte-exact); pass `lines` (e.g. \"40-60\") or `grep` to recall only the slice you need, which costs fewer tokens. `context` adds N lines around each grep match.",
            vec![
                prop("handle", "string", "A knapsack handle, e.g. ks2_0123456789abcdef0123456789abcdef (legacy ks_… still accepted)"),
                prop("lines", "string", "Optional 1-based inclusive range, e.g. \"40-60\""),
                prop(
                    "grep",
                    "string",
                    "Optional case-insensitive regex (subset: . * + ? ^ $ [class] \\d \\w \\s and negations). Plain words still work — unsupported metacharacters fall back to substring match.",
                ),
                prop("context", "number", "Lines of context to include around each grep match (default 0)"),
            ],
            &["handle"],
        ),
        tool(
            "knapsack_inspect",
            "Show metadata about a handle (bytes, lines, estimated tokens, whether it's UTF-8) plus a short preview, WITHOUT dumping the full content. Use it to decide whether — and how much — to expand.",
            vec![prop(
                "handle",
                "string",
                "A knapsack handle, e.g. ks2_0123456789abcdef0123456789abcdef (legacy ks_… still accepted)",
            )],
            &["handle"],
        ),
        tool(
            "knapsack_metrics",
            "Knapsack savings scoreboard: tokens saved vs refetched, NET saved, delta hits, expand calls. Pass session_id to scope it to one session.",
            vec![prop("session_id", "string", "Optional session id to filter by")],
            &[],
        ),
    ])
}

// ---------- dispatch ----------
fn arg_str(args: Option<&Json>, key: &str) -> Option<String> {
    args.and_then(|a| a.get(key))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}
fn call_tool(id: Option<Json>, name: &str, args: Option<&Json>) -> String {
    match name {
        "knapsack_expand" => {
            let handle = match arg_str(args, "handle") {
                Some(h) => h,
                None => {
                    return text_result(id, "knapsack_expand: 'handle' is required".into(), true)
                }
            };
            if !crate::hash::is_valid_handle(&handle) {
                return text_result(
                    id,
                    format!(
                        "knapsack_expand: invalid handle: {} (expected ks2_<32 hex> or legacy ks_<10|16 hex>)",
                        crate::hash::display_handle(&handle)
                    ),
                    true,
                );
            }
            // Parse numeric/range fields up front. If the caller passed `lines` or
            // `context` but the value was garbage (string for context, malformed range
            // for lines), surface that as an isError result instead of silently
            // expanding the whole file / using zero context.
            let range = match arg_str(args, "lines").as_deref() {
                None => None,
                Some(s) => {
                    match parse_range(s) {
                        Some(r) => Some(r),
                        None => {
                            return text_result(
                            id,
                            format!("knapsack_expand: 'lines' expects A-B (1-based inclusive), got: {}", crate::hash::display_handle(s)),
                            true,
                        );
                        }
                    }
                }
            };
            let context: usize = match args.and_then(|a| a.get("context")) {
                None => 0,
                Some(v) => match v.as_f64() {
                    Some(n) if n.is_finite() && n >= 0.0 => n as usize,
                    _ => {
                        return text_result(
                            id,
                            "knapsack_expand: 'context' expects a non-negative number".into(),
                            true,
                        );
                    }
                },
            };
            let req = ExpandRequest {
                handle: handle.clone(),
                range,
                grep: arg_str(args, "grep"),
                context,
                session_id: "mcp".into(),
                caller: ExpandCaller::Mcp,
            };
            match expand_handle(req) {
                Some(RecallOut::Text(t)) => text_result(id, t, false),
                Some(RecallOut::Bytes(b)) => match String::from_utf8(b) {
                    Ok(t) => text_result(id, t, false),
                    Err(e) => text_result(
                        id,
                        format!(
                            "[binary content: {} bytes — not shown]",
                            e.into_bytes().len()
                        ),
                        false,
                    ),
                },
                None => text_result(id, format!("No such handle: {}", handle), true),
            }
        }
        "knapsack_inspect" => {
            let handle = match arg_str(args, "handle") {
                Some(h) => h,
                None => {
                    return text_result(id, "knapsack_inspect: 'handle' is required".into(), true)
                }
            };
            if !crate::hash::is_valid_handle(&handle) {
                return text_result(
                    id,
                    format!(
                        "knapsack_inspect: invalid handle: {} (expected ks2_<32 hex> or legacy ks_<10|16 hex>)",
                        crate::hash::display_handle(&handle)
                    ),
                    true,
                );
            }
            let store = Store::new(config::store_dir());
            match store.get(&handle) {
                None => text_result(id, format!("No such handle: {}", handle), true),
                Some(b) => {
                    let utf8 = std::str::from_utf8(&b).is_ok();
                    let mut t = format!(
                        "{}: {} bytes · {} lines · ~{} tok · utf8={}",
                        handle,
                        b.len(),
                        block::count_lines(&b),
                        token_estimate::tokens_bytes(&b),
                        utf8
                    );
                    if utf8 {
                        for l in String::from_utf8_lossy(&b).lines().take(3) {
                            t.push_str(&format!("\n  | {}", l));
                        }
                    }
                    text_result(id, t, false)
                }
            }
        }
        "knapsack_metrics" => {
            // Symmetry with the CLI `knapsack metrics`: when no session_id is supplied,
            // call `metrics::report()` (which prepends the "current session" block before
            // the lifetime table). With an explicit session_id, the caller wants the
            // single-session filtered view, so use `report_for` directly. This keeps the
            // MCP and CLI surfaces returning byte-identical text — pinned by the
            // mcp_cli_symmetry tests.
            let text = match arg_str(args, "session_id") {
                Some(s) => metrics::report_for(Some(s.as_str())),
                None => metrics::report(),
            };
            text_result(id, text, false)
        }
        _ => rpc_error(id, -32601, &format!("Unknown tool: {}", name)),
    }
}

/// Handle one JSON-RPC message. Returns the response line, or None for notifications
/// (and unparseable input — fail quiet, never crash the stream).
pub fn handle_message(line: &str) -> Option<String> {
    let msg = json::parse(line).ok()?;
    let id = msg.get("id").cloned();
    let method = msg.get("method").and_then(|m| m.as_str()).unwrap_or("");
    match method {
        "initialize" => Some(reply(
            id,
            Json::Obj(vec![
                ("protocolVersion".into(), Json::Str(PROTOCOL.into())),
                (
                    "capabilities".into(),
                    Json::Obj(vec![("tools".into(), Json::Obj(vec![]))]),
                ),
                (
                    "serverInfo".into(),
                    Json::Obj(vec![
                        ("name".into(), Json::Str("knapsack".into())),
                        ("version".into(), Json::Str(VERSION.into())),
                    ]),
                ),
                ("instructions".into(), Json::Str(INSTRUCTIONS.into())),
            ]),
        )),
        "notifications/initialized" => None,
        "tools/list" => Some(reply(id, Json::Obj(vec![("tools".into(), tools())]))),
        "tools/call" => {
            let params = msg.get("params");
            let name = params
                .and_then(|p| p.get("name"))
                .and_then(|n| n.as_str())
                .unwrap_or("");
            let args = params.and_then(|p| p.get("arguments"));
            Some(call_tool(id, name, args))
        }
        _ => {
            // Respond with an error only to requests (which carry an id), not notifications.
            if id.is_some() {
                Some(rpc_error(
                    id,
                    -32601,
                    &format!("Method not found: {}", method),
                ))
            } else {
                None
            }
        }
    }
}

/// Run the stdio server loop: one JSON-RPC message per line in, one per line out.
pub fn serve() {
    let stdin = std::io::stdin();
    let mut out = std::io::stdout();
    for line in stdin.lock().lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Some(resp) = handle_message(trimmed) {
            if out.write_all(resp.as_bytes()).is_err() || out.write_all(b"\n").is_err() {
                break;
            }
            let _ = out.flush();
        }
    }
}
