//! `knapsack` CLI — the standalone binary and the integration surface. Zero-dep arg
//! parsing on purpose (clap is a fine later swap, but the core ships without it).
//!
//!   knapsack                          product summary (default — same as `status`)
//!   knapsack status                   product summary (Claude Code `/knapsack` lands here)
//!   knapsack hook                     PreToolUse shim (reads CC event on stdin)
//!   knapsack pack <file>              context-file pack — writes <name>.knapsack.md
//!                                     [--output P] [--force] [--dry-run]
//!   knapsack pack - [--session ID] [--cmd C] [--type code|log]
//!                                     stdin pipeline pack (used by the PreToolUse hook)
//!   knapsack expand <handle> [--lines A-B] [--grep P] [--session ID]
//!   knapsack inspect <handle>         metadata + preview, without dumping content
//!   knapsack delta <old> <new>        what a re-read costs after the first read
//!   knapsack store put <file>         store exact bytes, print handle
//!   knapsack metrics                  the live savings scoreboard
//!   knapsack bench                    the A/B/C edit->test loop benchmark
//!   knapsack install                  print Claude Code hook wiring
//!   knapsack gc                       drop blocks older than --older-than DAYS (default 30)

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
        // Bare `knapsack` and `knapsack status` both render the product-facing summary —
        // this is what the Claude Code `/knapsack` slash command invokes. `doctor` keeps
        // the long-form diagnostic; everything new goes here.
        "" | "status" => print!("{}", knapsack::status::report()),
        "hook" => hook::run_hook(),
        "mcp" => knapsack::mcp::serve(),

        "pack" => {
            let path = rest.first().cloned().unwrap_or_else(|| usage());
            let from_stdin = path == "-";
            if from_stdin {
                run_pack_stdin(&path, rest);
            } else {
                run_pack_doc(&path, rest);
            }
        }

        "expand" => {
            let handle = rest.first().cloned().unwrap_or_else(|| usage());
            // Reject malformed handles up front. Without this we'd silently fall through
            // to a "no such handle" — clear-but-wrong; the user can't tell whether the
            // handle is malformed or just missing. is_valid_handle accepts ks2_<32 hex>
            // (new) and legacy ks_<10 hex> / ks_<16 hex>.
            if !knapsack::hash::is_valid_handle(&handle) {
                eprintln!(
                    "knapsack: invalid handle: {} (expected ks2_<32 hex> or legacy ks_<10|16 hex>)",
                    knapsack::hash::display_handle(&handle)
                );
                exit(2);
            }
            // Parse numeric/range flags up front so a malformed value fails loudly
            // instead of silently falling through to the default. `--lines garbage`
            // used to expand the entire (possibly huge) file; `--context abc` used to
            // become zero context.
            let range = match flag(rest, "--lines") {
                None => None,
                Some(s) => match parse_range(s) {
                    Some(r) => Some(r),
                    None => {
                        eprintln!("knapsack: --lines expects A-B (1-based inclusive), got: {}", knapsack::hash::display_handle(s));
                        exit(2);
                    }
                },
            };
            let context: usize = match flag(rest, "--context") {
                None => 0,
                Some(s) => s.parse().unwrap_or_else(|_| {
                    eprintln!("knapsack: --context expects a non-negative integer, got: {}", knapsack::hash::display_handle(s));
                    exit(2);
                }),
            };
            let req = ExpandRequest {
                handle: handle.clone(),
                range,
                grep: flag(rest, "--grep").map(|s| s.to_string()),
                context,
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
            // Two overloads, distinguished by whether the arg is an existing file path:
            //   knapsack inspect <packed-file>   power-user view of a .knapsack.md
            //   knapsack inspect <handle>        store-level peek at a single handle
            // A handle is `ks_<hex>`; a path with a slash or `.knapsack.` in its name is
            // almost certainly a file. We fall through to handle-mode when neither
            // applies, which preserves the historical CLI shape.
            let arg = rest.first().cloned().unwrap_or_else(|| usage());
            let p = std::path::Path::new(&arg);
            if p.is_file() {
                run_inspect_doc(p);
            } else {
                run_inspect_handle(&arg);
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
            print!("{}", knapsack::ab::format(&knapsack::ab::build(&kn)));
        }

        "bench" => knapsack::bench::run(),
        "doctor" => print!("{}", knapsack::install::doctor()),
        "gc" => {
            // Distinguish "flag absent" (default 30 days) from "flag present but garbage"
            // (hard error). The old code used unwrap_or(30) for both, so `--older-than -5`
            // silently used the 30-day default — confusing, since gc would scan and report
            // as if the user had asked for the default window.
            let days: u64 = match flag(rest, "--older-than") {
                None => 30,
                Some(s) => match s.parse() {
                    Ok(n) => n,
                    Err(_) => {
                        eprintln!(
                            "knapsack: --older-than expects a non-negative integer (days), got: {}",
                            knapsack::hash::display_handle(s)
                        );
                        exit(2);
                    }
                },
            };
            let dry_run = rest.iter().any(|a| a == "--dry-run");
            let store = knapsack::Store::new(config::store_dir());
            let r = knapsack::gc::gc(&store, days * 86_400, dry_run);
            print!("{}", knapsack::gc::format(&r));
        }
        "transcript" => {
            // Debug surface: scan a Claude Code transcript and report what residency
            // gating would see. Useful when an emitted backref looks wrong.
            let path = rest.first().cloned().unwrap_or_else(|| usage());
            run_transcript_inspect(std::path::Path::new(&path));
        }
        "why-last" => {
            // Read-hook debug: print the last N pass-through decisions so you can see
            // why a Read was (or wasn't) redirected. Read hook is on by default after
            // `knapsack install`; the log fills up automatically during a Claude session.
            // Bad N fails loudly so `why-last abc` doesn't quietly print the default 10.
            let n: usize = match rest.first() {
                None => 10,
                Some(s) => s.parse().unwrap_or_else(|_| {
                    eprintln!("knapsack: why-last expects a non-negative integer, got: {}", knapsack::hash::display_handle(s));
                    exit(2);
                }),
            };
            run_why_last(n);
        }
        "install" => {
            // Bare `knapsack install` IS the one-shot now: wire the hook + MCP into the
            // user's Claude Code config so the next `claude` session has Input + Output
            // reduction active without further setup. `--repair` rewrites stale binary
            // paths in already-installed configs; `--print` keeps the old manual-snippet
            // surface for anyone who prefers to paste config by hand.
            //
            // Exit non-zero on failure so CI/post-update scripts (and the one-line
            // installer scripts) can detect a partial install and react instead of
            // silently leaving Claude Code unwired. The friendly transcript still
            // prints either way; the user sees what failed AND the shell knows.
            if rest.iter().any(|a| a == "--repair") {
                let r = knapsack::install::repair();
                print!("{}", r);
                if !r.success {
                    exit(1);
                }
            } else if rest.iter().any(|a| a == "--print") {
                print_install();
            } else {
                // `--apply` is still accepted as an explicit alias; new default is the same.
                let r = knapsack::install::apply();
                print!("{}", r);
                if !r.success {
                    exit(1);
                }
            }
        }
        "uninstall" => print!("{}", knapsack::install::uninstall(rest.iter().any(|a| a == "--purge"))),
        _ => usage(),
    }
}

/// Stdin pipeline pack — the historic shape used by the PreToolUse hook. Reads stdin,
/// runs the conditional engine (delta vs the session-seen ledger), prints the compact
/// view + metrics line to stdout. Unchanged behavior on purpose: this is what
/// `knapsack hook` rewrites bash commands to invoke (see hook.rs::wrap_command).
fn run_pack_stdin(path: &str, rest: &[String]) {
    let bytes = read_stdin();
    let session = flag(rest, "--session").unwrap_or("cli").to_string();
    let cmd_label = flag(rest, "--cmd").map(|s| s.to_string());
    let ct = match flag(rest, "--type") {
        Some("code") => Some(ContentType::Code),
        Some("log") => Some(ContentType::Log),
        _ => Some(detect(&bytes, None)),
    };
    let _ = path; // present only as the literal "-"; never used to read a file here
    // Optional transcript path from the hook; pack_output treats unreadable/missing
    // as no-gating, so we don't need to validate it here.
    let transcript_path = flag(rest, "--transcript").map(std::path::PathBuf::from);
    let r = pack_output(PackRequest {
        session_id: session,
        command: cmd_label,
        bytes,
        content_hint: ct,
        step: 0,
        transcript_path,
    });
    println!("{}", r.view);
    // Percent reduction, signed: negative (shown "+N%") when the compact view ends up
    // LARGER than the input — real for tiny low-signal blobs where the recall marker
    // costs more than the bytes it replaces. i64 avoids the usize underflow that turned
    // a grown view into a garbage 1.8e19% figure.
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

/// Render the last N entries from the Read-hook decision log. One line per entry,
/// padded so the reason column lines up; full path on the same line.
fn run_why_last(n: usize) {
    let log = knapsack::why_log::log_path();
    let entries = knapsack::why_log::read_last(n);
    if entries.is_empty() {
        println!("knapsack why-last: no entries in {}", log.display());
        if !knapsack::config::read_hook_enabled() {
            println!();
            println!("  (Read hook is OFF — `KNAPSACK_READ_HOOK=0` is set; unset it to re-enable.)");
        }
        return;
    }
    println!("knapsack why-last  ({} entries from {})", entries.len(), log.display());
    println!();
    for e in &entries {
        let path = e.path.as_deref().unwrap_or("");
        let bytes = e.bytes.map(|n| format!("{}B", n)).unwrap_or_default();
        let savings = match (e.raw_tokens, e.view_tokens) {
            (Some(r), Some(v)) if r > 0 => format!("  {}->{} tok", r, v),
            _ => String::new(),
        };
        let note = e.note.as_deref().map(|n| format!("  [{}]", n)).unwrap_or_default();
        println!("  {:<22}  {:<9} {}{}{}", e.reason.as_wire(), bytes, path, savings, note);
    }
}

/// Inspect a Claude Code JSONL transcript and report what residency gating sees:
/// enabled/disabled (i.e. whether the file is parseable), last boundary detected,
/// and the resident handle set's size. This is the documented debug surface for
/// "why did Knapsack treat handle X as not-resident?".
fn run_transcript_inspect(path: &std::path::Path) {
    let scan = knapsack::transcript::scan(path);
    println!("knapsack transcript inspect: {}", path.display());
    println!();
    if !scan.ok {
        println!("  status:           DISABLED — transcript unreadable or empty");
        println!("  fallback:         ledger-only residency (safe default)");
        println!();
        println!("  Provide a valid Claude Code JSONL transcript to see boundary + resident analysis.");
        return;
    }
    println!("  status:           ENABLED");
    println!("  lines scanned:    {}", scan.lines_scanned);
    match scan.last_boundary {
        Some((b, i)) => println!(
            "  last boundary:    {} (line {} of {})",
            knapsack::transcript::boundary_label(b),
            i + 1,
            scan.lines_scanned
        ),
        None => println!("  last boundary:    none detected"),
    }
    println!("  resident handles: {}", scan.resident.len());
    if !scan.resident.is_empty() {
        // Show up to 5 so the output stays compact for very long transcripts.
        let mut sample: Vec<&String> = scan.resident.iter().collect();
        sample.sort();
        for h in sample.iter().take(5) {
            println!("    · {}", h);
        }
        if scan.resident.len() > 5 {
            println!("    · … {} more", scan.resident.len() - 5);
        }
    }
}

/// Inspect a packed side-car: parse out the manifest + every recall marker, and print
/// the per-section index a power user actually wants. This is the documented "where do
/// the friendly markers point?" command — readers never need to grep HTML comments.
fn run_inspect_doc(path: &std::path::Path) {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("knapsack: cannot read {}: {}", path.display(), e);
            exit(2);
        }
    };
    let m = knapsack::pack_doc::parse_packed(&content);
    if m.whole_file_handle.is_none() && m.markers.is_empty() {
        eprintln!(
            "knapsack inspect: {} does not look like a knapsack-packed file (no `ks-pack` header, no `ks-recall` markers).",
            path.display()
        );
        exit(1);
    }
    println!("Knapsack packed view: {}", path.display());
    if let Some(src) = &m.source {
        println!("  original source: {}", src);
    }
    if let Some(h) = &m.whole_file_handle {
        println!("  whole-file handle: {}", h);
        println!("  full recall: knapsack expand {}", h);
    }
    println!("  elisions: {}", m.markers.len());
    if !m.markers.is_empty() {
        println!();
        // Aligned columns for human scanning: index · line range · token count.
        for (i, mk) in m.markers.iter().enumerate() {
            println!(
                "  #{i:<2}  lines {a}-{b:<5}  ~{tok} tokens   recall: knapsack expand {h} --lines {a}-{b}",
                i = i + 1,
                a = mk.line_from,
                b = mk.line_to,
                tok = mk.tokens,
                h = mk.handle
            );
        }
    }
}

/// The historic `knapsack inspect <handle>` path — handle metadata + preview.
fn run_inspect_handle(handle: &str) {
    // Same gate as `expand`: reject malformed handles instead of bouncing through the
    // store and returning a misleading "no such handle".
    if !knapsack::hash::is_valid_handle(handle) {
        eprintln!(
            "knapsack: invalid handle: {} (expected ks2_<32 hex> or legacy ks_<10|16 hex>)",
            knapsack::hash::display_handle(handle)
        );
        exit(2);
    }
    let store = knapsack::Store::new(config::store_dir());
    match store.get(&handle.to_string()) {
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

/// Context-file pack — the user-facing path behind `/knapsack pack <file>`. Reads the
/// file, stores byte-exact original, writes a markdown-aware compact view to a side-car
/// (default `<name>.knapsack.md`). Safety invariants enforced here, not in pack_doc:
/// - the original file is never touched
/// - we refuse to write a non-shrinking view unless `--force` was passed
/// - `--dry-run` writes nothing and reports what would have happened
fn run_pack_doc(path: &str, rest: &[String]) {
    let bytes = read_file(path);
    let force = rest.iter().any(|a| a == "--force");
    let dry_run = rest.iter().any(|a| a == "--dry-run");
    let output_override = flag(rest, "--output").map(std::path::PathBuf::from);

    let store = knapsack::Store::new(config::store_dir());
    let r = knapsack::pack_doc::pack_doc(path, &bytes, &store);

    let out_path = output_override.unwrap_or_else(|| knapsack::pack_doc::sidecar_path(std::path::Path::new(path)));
    let saved = r.raw_tokens as i64 - r.packed_tokens as i64;
    let is_smaller = r.packed_tokens < r.raw_tokens;

    println!("Packed {}", path);
    println!();
    println!("Original: {} tokens", commafy(r.raw_tokens as i64));
    println!("Packed:   {} tokens", commafy(r.packed_tokens as i64));
    if r.raw_tokens > 0 {
        // tenths-of-a-percent, computed as integer math to avoid float-formatting drift
        let pct10 = saved * 1000 / r.raw_tokens as i64;
        let sign = if pct10 < 0 { "-" } else { "" };
        println!(
            "Saved:    {} tokens / {}{}.{}%",
            commafy(saved),
            sign,
            (pct10.abs() / 10),
            (pct10.abs() % 10)
        );
    } else {
        println!("Saved:    n/a (empty input)");
    }
    println!("Elisions: {}", r.elisions);
    println!("Exact original: recoverable via `knapsack expand {}`", r.handle);
    println!();

    if dry_run {
        println!("Dry run — nothing written.");
        println!("Would write: {}", out_path.display());
        return;
    }

    if !is_smaller && !force {
        eprintln!("Packed view is not smaller than the original — refusing to write.");
        eprintln!("Re-run with `--force` to write anyway, or `--dry-run` to inspect first.");
        eprintln!("Would write: {}", out_path.display());
        exit(3);
    }

    if let Err(e) = std::fs::write(&out_path, r.view.as_bytes()) {
        eprintln!("knapsack: cannot write {}: {}", out_path.display(), e);
        exit(2);
    }
    println!("Wrote:");
    println!("  {}", out_path.display());
}

fn commafy(n: i64) -> String {
    let neg = n < 0;
    let digits = n.abs().to_string();
    let len = digits.len();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (len - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    if neg {
        format!("-{}", out)
    } else {
        out
    }
}

fn usage() -> ! {
    eprintln!(
        "knapsack — conditional token reducer\n\n\
         usage:\n  \
         knapsack                          (product summary — same as `status`)\n  \
         knapsack status                   (product summary; `/knapsack` in Claude Code)\n  \
         knapsack hook                     (PreToolUse shim)\n  \
         knapsack mcp                      (MCP stdio server: expand/inspect/metrics)\n  \
         knapsack pack <file> [--output P] [--force] [--dry-run]\n  \
         knapsack pack - [--session ID] [--cmd C] [--type code|log]\n  \
         knapsack expand <handle> [--lines A-B] [--grep P] [--context N] [--session ID]\n  \
         knapsack inspect <handle>\n  \
         knapsack delta <old> <new>\n  \
         knapsack store put <file>\n  \
         knapsack metrics\n  \
         knapsack ab [--knapsack PATH]\n  \
         knapsack bench\n  \
         knapsack doctor\n  \
         knapsack gc [--older-than DAYS] [--dry-run]\n  \
         knapsack transcript <path>        (debug: scan a CC transcript for boundaries + handles)\n  \
         knapsack why-last [N]             (debug: last N Read-hook decisions; EXPERIMENTAL)\n  \
         knapsack install [--apply|--repair]\n  \
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
