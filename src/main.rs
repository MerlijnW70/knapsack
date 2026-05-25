//! `knapsack` CLI — the standalone binary and the integration surface. Zero-dep arg
//! parsing on purpose (clap is a fine later swap, but the core ships without it).
//!
//!   knapsack hook                     PreToolUse shim (reads CC event on stdin)
//!   knapsack pack <file|-> [--session ID] [--cmd C] [--type code|log]
//!   knapsack expand <handle> [--lines A-B] [--grep P] [--session ID]
//!   knapsack inspect <handle>         metadata + preview, without dumping content
//!   knapsack delta <old> <new>        what a re-read costs after the first read
//!   knapsack store put <file>         store exact bytes, print handle
//!   knapsack metrics                  the live savings scoreboard
//!   knapsack bench                    the A/B/C edit->test loop benchmark
//!   knapsack install                  print Claude Code hook wiring

use knapsack::api::{expand_handle, pack_output, ExpandRequest, PackRequest};
use knapsack::block::count_lines;
use knapsack::content_type::{detect, ContentType};
use knapsack::recall::{parse_range, RecallOut};
use knapsack::token_estimate::tokens_bytes;
use knapsack::{config, hook, metrics};
use std::io::{Read, Write};
use std::process::exit;

fn flag<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).map(|s| s.as_str())
}

fn read_file(path: &str) -> Vec<u8> {
    std::fs::read(path).unwrap_or_else(|e| {
        eprintln!("knapsack: cannot read {}: {}", path, e);
        exit(2);
    })
}

fn read_stdin() -> Vec<u8> {
    let mut v = Vec::new();
    let _ = std::io::stdin().read_to_end(&mut v);
    v
}

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let cmd = argv.first().map(|s| s.as_str()).unwrap_or("");
    let rest: &[String] = if argv.is_empty() { &[] } else { &argv[1..] };

    match cmd {
        "hook" => hook::run_hook(),
        "mcp" => knapsack::mcp::serve(),

        "pack" => {
            let path = rest.first().cloned().unwrap_or_else(|| usage());
            let from_stdin = path == "-";
            let bytes = if from_stdin { read_stdin() } else { read_file(&path) };
            let session = flag(rest, "--session").unwrap_or("cli").to_string();
            let cmd_label = flag(rest, "--cmd").map(|s| s.to_string());
            let ct = match flag(rest, "--type") {
                Some("code") => Some(ContentType::Code),
                Some("log") => Some(ContentType::Log),
                _ => Some(if from_stdin { detect(&bytes, None) } else { detect(&bytes, Some(&path)) }),
            };
            let r = pack_output(PackRequest {
                session_id: session,
                command: cmd_label.or(if from_stdin { None } else { Some(path) }),
                bytes,
                content_hint: ct,
                step: 0,
            });
            println!("{}", r.view);
            // Percent reduction, signed: negative (shown "+N%") when the compact view ends
            // up LARGER than the input — real for tiny low-signal blobs where the recall
            // marker costs more than the bytes it replaces. i64 avoids the usize underflow
            // that turned a grown view into a garbage 1.8e19% figure.
            let (raw, shown) = (r.raw_tokens_est as i64, r.shown_tokens_est as i64);
            let pct = ((raw - shown) * 100).checked_div(raw).unwrap_or(0);
            println!(
                "\n[knapsack {}->{} tok ({}{}%) · {} blocks · {} unchanged · {} re-sent]",
                r.raw_tokens_est,
                r.shown_tokens_est,
                if pct >= 0 { "-" } else { "+" },
                pct.abs(),
                r.blocks,
                r.delta_hits,
                r.evicted_resends
            );
        }

        "expand" => {
            let handle = rest.first().cloned().unwrap_or_else(|| usage());
            let req = ExpandRequest {
                handle: handle.clone(),
                range: flag(rest, "--lines").and_then(parse_range),
                grep: flag(rest, "--grep").map(|s| s.to_string()),
                context: flag(rest, "--context").and_then(|s| s.parse().ok()).unwrap_or(0),
                session_id: flag(rest, "--session").unwrap_or("cli").to_string(),
            };
            match expand_handle(req) {
                Some(RecallOut::Bytes(b)) => {
                    let _ = std::io::stdout().write_all(&b);
                }
                Some(RecallOut::Text(t)) => println!("{}", t),
                None => {
                    eprintln!("knapsack: no such handle: {}", handle);
                    exit(1);
                }
            }
        }

        "inspect" => {
            let handle = rest.first().cloned().unwrap_or_else(|| usage());
            let store = knapsack::Store::new(config::store_dir());
            match store.get(&handle) {
                None => {
                    eprintln!("knapsack: no such handle: {}", handle);
                    exit(1);
                }
                Some(b) => {
                    let utf8 = std::str::from_utf8(&b).is_ok();
                    println!(
                        "{}: {} bytes · {} lines · ~{} tok · utf8={}",
                        handle,
                        b.len(),
                        count_lines(&b),
                        tokens_bytes(&b),
                        utf8
                    );
                    if utf8 {
                        for l in String::from_utf8_lossy(&b).lines().take(3) {
                            println!("  | {}", l);
                        }
                    }
                }
            }
        }

        "delta" => {
            let (old, new) = match (rest.first(), rest.get(1)) {
                (Some(a), Some(b)) => (a.clone(), b.clone()),
                _ => usage(),
            };
            let oldb = read_file(&old);
            let newb = read_file(&new);
            let store = knapsack::Store::new(std::env::temp_dir().join(format!("knapsack-delta-{}", std::process::id())));
            let mut ledger = knapsack::Ledger::in_memory();
            let ct = detect(&oldb, Some(&old));
            knapsack::pack(&oldb, ct, &store, &mut ledger, 0);
            let r = knapsack::pack(&newb, ct, &store, &mut ledger, 1);
            println!("{}", r.view);
            println!(
                "\n[knapsack delta: new {} -> {} tok · {}/{} blocks unchanged]",
                r.raw_tokens_est, r.shown_tokens_est, r.delta_hits, r.blocks
            );
        }

        "store" if rest.first().map(|s| s.as_str()) == Some("put") => {
            let path = rest.get(1).cloned().unwrap_or_else(|| usage());
            let bytes = read_file(&path);
            let store = knapsack::Store::new(config::store_dir());
            println!("{}", store.put(&bytes));
        }

        "metrics" => println!("{}", metrics::report()),

        "ab" => {
            let kn = flag(rest, "--knapsack").map(std::path::PathBuf::from).unwrap_or_else(config::metrics_path);
            let ru = flag(rest, "--rucksack")
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|| config::home().join(".rucksack").join("metrics.jsonl"));
            print!("{}", knapsack::ab::format(&knapsack::ab::compare(&kn, &ru)));
        }

        "bench" => knapsack::bench::run(),
        "doctor" => print!("{}", knapsack::install::doctor()),
        "install" => {
            if rest.iter().any(|a| a == "--apply") {
                print!("{}", knapsack::install::apply());
            } else {
                print_install();
            }
        }
        "uninstall" => print!("{}", knapsack::install::uninstall(rest.iter().any(|a| a == "--purge"))),
        _ => usage(),
    }
}

fn usage() -> ! {
    eprintln!(
        "knapsack — conditional token reducer\n\n\
         usage:\n  \
         knapsack hook                     (PreToolUse shim)\n  \
         knapsack mcp                      (MCP stdio server: expand/inspect/metrics)\n  \
         knapsack pack <file|-> [--session ID] [--cmd C] [--type code|log]\n  \
         knapsack expand <handle> [--lines A-B] [--grep P] [--context N] [--session ID]\n  \
         knapsack inspect <handle>\n  \
         knapsack delta <old> <new>\n  \
         knapsack store put <file>\n  \
         knapsack metrics\n  \
         knapsack ab [--knapsack PATH] [--rucksack PATH]\n  \
         knapsack bench\n  \
         knapsack doctor\n  \
         knapsack install [--apply]\n  \
         knapsack uninstall [--purge]"
    );
    exit(1);
}

fn print_install() {
    let bin = std::env::current_exe()
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .unwrap_or_else(|_| "knapsack".into());
    println!(
        "Add this to your Claude Code settings.json (hooks). The shim reads the session id\n\
         from the PreToolUse payload, so no env var is needed:\n\n\
         {{\n  \
           \"hooks\": {{\n    \
             \"PreToolUse\": [\n      \
               {{ \"matcher\": \"Bash\", \"hooks\": [ {{ \"type\": \"command\", \"command\": \"\\\"{bin}\\\" hook\" }} ] }}\n    \
             ]\n  \
           }}\n\
         }}\n\n\
         Then: noisy allowlisted Bash commands (cargo/npm/pytest/...) get their output packed\n\
         conditionally per session; recall with `knapsack expand <handle>`; check `knapsack metrics`.\n\n\
         For ergonomic recall as MCP tools (knapsack_expand / knapsack_inspect / knapsack_metrics),\n\
         add to .mcp.json:\n\n\
         {{\n  \
           \"mcpServers\": {{\n    \
             \"knapsack\": {{ \"command\": \"{bin}\", \"args\": [\"mcp\"] }}\n  \
           }}\n\
         }}",
        bin = bin
    );
}
