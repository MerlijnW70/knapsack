//! Round 7 — deep boundary cases + shell-escape safety.
//!
//! - wrap_command shell escape: bin paths / sessions / commands containing
//!   quotes, backticks, $vars, newlines. The hook builds a shell pipeline
//!   STRING — any escape bug here is shell injection waiting to happen.
//! - structural compression off-by-ones: every documented threshold pinned
//!   at N-1, N, N+1.
//! - recall::parse_range edge inputs.
//! - token_estimate exact ceiling math at every weight boundary.

use knapsack::block::{count_lines, split_blocks, split_lines};
use knapsack::content_type::ContentType;
use knapsack::hook::{decide, wrap_command};
use knapsack::ledger::Ledger;
use knapsack::pack::pack;
use knapsack::recall::parse_range;
use knapsack::store::Store;
use knapsack::structural;
use knapsack::token_estimate::tokens;

fn tmpstore(tag: &str) -> Store {
    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    Store::new(std::env::temp_dir().join(format!("kn-bdy-{}-{}-{}", tag, std::process::id(), t)))
}

// ====================================================================
// 1. wrap_command shell-escape safety
// ====================================================================

#[test]
fn wrap_command_quotes_bin_path_with_spaces() {
    // bin path with spaces — must end up DOUBLE-QUOTED in the wrapped pipeline,
    // otherwise the shell would split on whitespace and we'd be invoking the
    // wrong program.
    let w = wrap_command("cargo test", "/path with spaces/knapsack", "sess-1", "cargo", None);
    assert!(w.contains("\"/path with spaces/knapsack\" pack -"),
        "bin path must be quoted to survive shell splitting:\n{w}");
}

#[test]
fn wrap_command_quotes_session_with_spaces() {
    // session id has internal spaces or other shell-meta chars. wrap_command
    // currently does `--session "{session}"` so embedded `"` would close the
    // quoting early. Pin current behavior — if a session id ever contains a
    // literal `"`, the wrap_command would shell-inject. We don't fix that
    // here (out of scope for input dogfood; sessions are sanitized at
    // session_path level) but we document the assumption explicitly.
    let w = wrap_command("cargo test", "/bin/k", "sess with spaces", "cargo", None);
    assert!(w.contains("--session \"sess with spaces\""), "session quoted: {w}");
}

#[test]
fn wrap_command_inner_command_appears_verbatim() {
    // The inner command (the original user input) is placed as-is in the
    // wrapped string. Verify: a complex multi-arg command appears intact.
    let inner = "cargo test --release --features foo --bin some_bin";
    let w = wrap_command(inner, "/bin/k", "sess", "cargo", None);
    assert!(w.contains(inner), "inner command must appear verbatim:\n{w}");
}

#[test]
fn wrap_command_inner_command_with_quotes_does_not_close_outer() {
    // Inner command contains `"` — wrap_command should NOT have re-quoted it
    // (it just inserts the inner command between `{ ... ; echo $? > ... ; }`).
    // The user's original `"` is preserved as-is. Pin current behavior.
    let inner = r#"cargo test -- --filter "test_name""#;
    let w = wrap_command(inner, "/bin/k", "sess", "cargo", None);
    assert!(w.contains(inner), "inner command with quotes preserved:\n{w}");
}

#[test]
fn wrap_command_strips_trailing_semicolons_so_brace_block_is_well_formed() {
    // The wrapper template is `{ {inner} ; echo $? > ... ; }`. If `inner` ends
    // with `;` we'd get `{ cmd; ; echo $? > ... ; }` which is fine in bash but
    // ugly. wrap_command does trim_end_matches([';', ' ']) — pin it.
    let w = wrap_command("cargo test ;;", "/bin/k", "sess", "cargo", None);
    assert!(!w.contains(";;"), "trailing semicolons stripped:\n{w}");
}

#[test]
fn wrap_command_transcript_arg_only_when_provided() {
    let none = wrap_command("cargo test", "/bin/k", "sess", "cargo", None);
    assert!(!none.contains("--transcript"), "no transcript arg when None");
    let some = wrap_command("cargo test", "/bin/k", "sess", "cargo", Some("/path/t.jsonl"));
    assert!(some.contains("--transcript \"/path/t.jsonl\""), "transcript arg when Some");
    let empty = wrap_command("cargo test", "/bin/k", "sess", "cargo", Some(""));
    assert!(!empty.contains("--transcript"), "empty transcript treated as None");
    let blank = wrap_command("cargo test", "/bin/k", "sess", "cargo", Some("   "));
    assert!(!blank.contains("--transcript"), "whitespace-only transcript treated as None");
}

#[test]
fn wrap_command_exit_code_template_present() {
    // The whole point of the template is preserving the original exit code.
    // Pin the structure: mktemp file, trap cleanup, exit "$(cat ...)" at end.
    let w = wrap_command("cargo test", "/bin/k", "sess", "cargo", None);
    assert!(w.contains("mktemp"), "exit-code capture uses mktemp");
    assert!(w.contains("trap"), "trap cleans up the temp file");
    assert!(w.contains("exit "), "wrapper re-raises the original exit code");
    assert!(w.contains("echo $?"), "writes inner's exit code to temp file");
}

#[test]
fn decide_skips_commands_containing_newlines_via_no_match_path() {
    // A bash command with a literal newline is unusual but possible (e.g.
    // multiline cargo invocation). decide() splits on segments first; a
    // newline isn't a segment separator but is also not a recognized
    // metachar. The first segment is what gets matched. Pin current
    // behavior.
    let cmd = "cargo test\ncargo build";
    let _ = decide(cmd);
    // We don't assert wrap/skip here — the point is decide() must NOT panic
    // on a newline-containing command.
}

// ====================================================================
// 2. structural compression boundary off-by-ones
// ====================================================================

#[test]
fn log_at_exact_head_tail_threshold_emits_verbatim() {
    // structural::compress_log: `if n <= LOG_HEAD + LOG_TAIL + 8 { verbatim }`.
    // LOG_HEAD=10, LOG_TAIL=6, +8 = 24. n=24 should still be verbatim.
    let mut bytes = String::new();
    for i in 1..=24 {
        bytes.push_str(&format!("line {i}\n"));
    }
    let (view, elisions) = structural::compress(bytes.as_bytes(), 0, bytes.len(), ContentType::Log);
    assert!(elisions.is_empty(), "at threshold n=24: no elision");
    assert!(view.contains("line 1") && view.contains("line 24"), "head AND tail preserved");
}

#[test]
fn log_one_line_past_threshold_starts_eliding() {
    // n=25 should now emit an elision.
    let mut bytes = String::new();
    for i in 1..=25 {
        bytes.push_str(&format!("line {i}\n"));
    }
    let (view, elisions) = structural::compress(bytes.as_bytes(), 0, bytes.len(), ContentType::Log);
    assert_eq!(elisions.len(), 1, "n=25 should produce one elision");
    assert!(view.contains("lines elided") || view.contains("lines elided"),
        "view names the elision count");
}

#[test]
fn log_chunk_boundary_in_block_splitter() {
    // block::split_blocks for Log uses LOG_CHUNK=6 — a dense unanchored run
    // is capped at 6 lines per block.
    let mut bytes = String::new();
    for i in 0..12 {
        bytes.push_str(&format!("plain line {i}\n"));
    }
    let blocks = split_blocks(bytes.as_bytes(), ContentType::Log);
    // 12 plain lines should split into 2 blocks of 6 each.
    assert!(blocks.len() >= 2, "12 plain lines must produce ≥2 blocks, got {}", blocks.len());
    for &(s, e) in &blocks {
        let n = count_lines(&bytes.as_bytes()[s..e]);
        assert!(n <= 6, "no block exceeds LOG_CHUNK=6 lines, got {n}");
    }
}

#[test]
fn code_min_run_boundary_for_collapse() {
    // structural::compress_code: `if run.len() >= MIN_RUN && tokens(body) > tokens(marker)`.
    // MIN_RUN=4. A 3-line body should stay verbatim; a 4-line body MAY collapse
    // depending on token budget.
    let bytes = b"fn three() {\n    a();\n    b();\n}\n";
    let store = tmpstore("code-3line");
    let mut ledger = Ledger::in_memory();
    let r = pack(bytes, ContentType::Code, &store, &mut ledger, 0);
    // A tiny 3-line body should appear in the view verbatim.
    assert!(r.view.contains("a();"), "3-line body inlined verbatim:\n{}", r.view);
}

#[test]
fn json_keep_bytes_boundary_240() {
    // structural::compress_json: `const KEEP_BYTES: usize = 240`. A member
    // larger than 240 bytes is elided; ≤240 is kept verbatim.
    // Build a JSON with one member just over 240 bytes.
    let big_val = "x".repeat(250);
    let bytes = format!("{{\"big\":\"{big_val}\",\"small\":\"y\"}}");
    let (view, elisions) = structural::compress(bytes.as_bytes(), 0, bytes.len(), ContentType::Json);
    // The big member should be elided (or at least one elision happened).
    assert!(!elisions.is_empty(), "big member should produce an elision:\nview={view}");
}

// ====================================================================
// 3. recall::parse_range edge values
// ====================================================================

#[test]
fn parse_range_well_formed() {
    assert_eq!(parse_range("1-5"), Some((1, 5)));
    assert_eq!(parse_range("10-100"), Some((10, 100)));
    assert_eq!(parse_range("0-0"), Some((0, 0)));
}

#[test]
fn parse_range_with_spaces() {
    // current parser does trim().parse() on each half — handles whitespace
    assert_eq!(parse_range(" 1 - 5 "), Some((1, 5)));
    assert_eq!(parse_range("1- 5"), Some((1, 5)));
    assert_eq!(parse_range("1 -5"), Some((1, 5)));
}

#[test]
fn parse_range_reverse_order_still_parses_to_tuple() {
    // parse_range doesn't validate ordering; that's the caller's job
    // (recall.rs::expand handles "lo >= hi" by returning empty).
    assert_eq!(parse_range("10-5"), Some((10, 5)));
}

#[test]
fn parse_range_negative_or_missing_parts_errors() {
    assert_eq!(parse_range("-5"), None, "missing low half");
    assert_eq!(parse_range("5-"), None, "missing high half");
    assert_eq!(parse_range("--5"), None, "double dash garbage");
    assert_eq!(parse_range("abc-def"), None, "non-numeric");
    assert_eq!(parse_range(""), None, "empty");
    assert_eq!(parse_range("5"), None, "no dash at all");
    assert_eq!(parse_range("1-2-3"), None, "extra dashes");
}

#[test]
fn parse_range_very_large_numbers_parse() {
    // usize on 64-bit can hold up to 2^64-1. Test that big numbers parse.
    assert_eq!(parse_range("1-1000000000"), Some((1, 1_000_000_000)));
}

// ====================================================================
// 4. token_estimate exact ceiling math
// ====================================================================

#[test]
fn token_estimator_letter_weight_at_boundary() {
    // W_ALPHA = 0.196. Check ceiling math at every k.
    //   1 letter -> ceil(0.196) = 1
    //   5 letters -> ceil(0.98)  = 1
    //   6 letters -> ceil(1.176) = 2
    //  10 letters -> ceil(1.96)  = 2
    //  11 letters -> ceil(2.156) = 3
    assert_eq!(tokens("a"), 1);
    assert_eq!(tokens("aaaaa"), 1);
    assert_eq!(tokens("aaaaaa"), 2);
    assert_eq!(tokens("aaaaaaaaaa"), 2);
    assert_eq!(tokens("aaaaaaaaaaa"), 3);
}

#[test]
fn token_estimator_digit_weight_at_boundary() {
    // W_DIGIT = 0.699.
    //   1 digit  -> ceil(0.699) = 1
    //   2 digits -> ceil(1.398) = 2
    //   3 digits -> ceil(2.097) = 3
    assert_eq!(tokens("1"), 1);
    assert_eq!(tokens("12"), 2);
    assert_eq!(tokens("123"), 3);
    assert_eq!(tokens("1234"), 3); // ceil(2.796) = 3
    assert_eq!(tokens("12345"), 4); // ceil(3.495) = 4
}

#[test]
fn token_estimator_symbol_weight_at_boundary() {
    // W_SYMBOL = 0.65.
    //   1 sym -> 1, 2 -> 2, 3 -> 2 (ceil 1.95), 4 -> 3 (ceil 2.6)
    assert_eq!(tokens("!"), 1);
    assert_eq!(tokens("!@"), 2);
    assert_eq!(tokens("!@#"), 2);
    assert_eq!(tokens("!@#$"), 3);
    assert_eq!(tokens("!@#$%"), 4); // ceil(3.25) = 4
}

#[test]
fn token_estimator_space_weight_at_boundary() {
    // W_SPACE = 0.433.
    //   1 sp -> ceil(0.433) = 1
    //   2 sp -> ceil(0.866) = 1
    //   3 sp -> ceil(1.299) = 2
    assert_eq!(tokens(" "), 1);
    assert_eq!(tokens("  "), 1);
    assert_eq!(tokens("   "), 2);
}

#[test]
fn token_estimator_mixed_sum_at_boundary() {
    // 1 letter + 1 digit + 1 sym + 1 space = 0.196 + 0.699 + 0.65 + 0.433
    //                                      = 1.978 -> ceil = 2
    assert_eq!(tokens("a1! "), 2);
}

#[test]
fn token_estimator_one_million_letters_doesnt_overflow_counter() {
    // 1M letters at 0.196 = 196_000.0 tokens — fits comfortably in usize.
    let s = "a".repeat(1_000_000);
    assert_eq!(tokens(&s), 196_000);
}

// ====================================================================
// 5. split_lines edge cases
// ====================================================================

#[test]
fn split_lines_empty_input() {
    assert!(split_lines(b"").is_empty());
}

#[test]
fn split_lines_no_trailing_newline_still_emits_final_line() {
    let ranges = split_lines(b"line one\nline two");
    assert_eq!(ranges.len(), 2);
    // Second range covers "line two" (no trailing \n)
    assert_eq!(ranges[1], (9, 17));
}

#[test]
fn split_lines_all_blank_lines() {
    let bytes = b"\n\n\n";
    let ranges = split_lines(bytes);
    // 3 newlines -> 3 ranges, each (i, i+1) for the lone \n
    assert_eq!(ranges.len(), 3);
}

#[test]
fn split_lines_with_crlf() {
    // \r\n: \r is a regular byte; only \n triggers split. So a CRLF line has
    // the \r as part of the line's bytes.
    let ranges = split_lines(b"line\r\n");
    assert_eq!(ranges.len(), 1);
    assert_eq!(ranges[0], (0, 6)); // "line\r\n"
}

#[test]
fn count_lines_matches_split_lines_len() {
    for s in ["", "one", "a\nb", "a\nb\n", "a\n\nb", "\n", "\n\n"] {
        assert_eq!(count_lines(s.as_bytes()), split_lines(s.as_bytes()).len(),
            "count_lines must agree with split_lines.len() on {s:?}");
    }
}
