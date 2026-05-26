//! Adversarial extremes for the structural compressor (src/structural.rs) and
//! the upstream block splitter (src/block.rs). Each test pins behavior at an
//! input shape that would normally never occur — pathological code, all-error
//! logs, deeply nested JSON, single-character files, etc. — and confirms two
//! invariants regardless of shape:
//!   1. The function does not panic.
//!   2. `reconstruct(...) == bytes` for any input we successfully pack.

use knapsack::content_type::ContentType;
use knapsack::ledger::Ledger;
use knapsack::pack::{pack, reconstruct};
use knapsack::store::Store;
use knapsack::structural;
use std::path::PathBuf;

fn tmpstore(tag: &str) -> Store {
    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    let dir = std::env::temp_dir().join(format!("kn-sa-{}-{}-{}", tag, std::process::id(), t));
    Store::new(dir)
}

fn pack_and_reconstruct(bytes: &[u8], ct: ContentType, tag: &str) {
    let store = tmpstore(tag);
    let mut ledger = Ledger::in_memory();
    let _r = pack(bytes, ct, &store, &mut ledger, 0);
    if !bytes.is_empty() {
        let back = reconstruct(bytes, ct, &store)
            .expect(&format!("{tag}: reconstruct must return Some"));
        assert_eq!(back, bytes, "{tag}: reconstruct must be byte-exact");
    }
}

// ---------- empty / one-byte / one-line inputs ----------

#[test]
fn empty_input_does_not_panic() {
    pack_and_reconstruct(b"", ContentType::Code, "empty-code");
    pack_and_reconstruct(b"", ContentType::Log, "empty-log");
    pack_and_reconstruct(b"", ContentType::Json, "empty-json");
}

#[test]
fn single_byte_inputs() {
    for &c in b"a 0 \n\t!" {
        let bytes = &[c];
        pack_and_reconstruct(bytes, ContentType::Code, &format!("byte-{c}"));
        pack_and_reconstruct(bytes, ContentType::Log, &format!("byte-{c}"));
    }
}

#[test]
fn single_newline_only() {
    pack_and_reconstruct(b"\n", ContentType::Code, "newline-code");
    pack_and_reconstruct(b"\n", ContentType::Log, "newline-log");
}

#[test]
fn only_whitespace_inputs() {
    let bytes = b"   \t\t\n  \n\t\n  ";
    pack_and_reconstruct(bytes, ContentType::Code, "ws-code");
    pack_and_reconstruct(bytes, ContentType::Log, "ws-log");
}

#[test]
fn only_blank_lines() {
    let bytes = b"\n\n\n\n\n\n\n\n\n\n";
    pack_and_reconstruct(bytes, ContentType::Code, "blanks-code");
    pack_and_reconstruct(bytes, ContentType::Log, "blanks-log");
}

#[test]
fn no_trailing_newline() {
    let bytes = b"line without trailing newline";
    pack_and_reconstruct(bytes, ContentType::Code, "no-trail-newline");
}

#[test]
fn crlf_line_endings() {
    let bytes = b"line one\r\nline two\r\nline three\r\n";
    pack_and_reconstruct(bytes, ContentType::Code, "crlf");
    pack_and_reconstruct(bytes, ContentType::Log, "crlf-log");
}

#[test]
fn mixed_crlf_and_lf() {
    let bytes = b"crlf\r\nlf\nmore\r\n\nfinal";
    pack_and_reconstruct(bytes, ContentType::Code, "mixed-eol");
}

// ---------- pathological code shapes ----------

#[test]
fn code_with_one_thousand_line_function_body() {
    // Single function with a body of 1000 lines. The block splitter sees one
    // top-level definition (the `fn`), so the entire 1000-line body is ONE
    // block. The structural compressor's flush loop should handle it.
    let mut body = String::from("fn enormous() {\n");
    for i in 0..1000 {
        body.push_str(&format!("    let x{i} = {i} + 1;\n"));
    }
    body.push_str("}\n");
    pack_and_reconstruct(body.as_bytes(), ContentType::Code, "1k-line-fn");
}

#[test]
fn code_with_no_recognisable_definitions_falls_back_to_blank_line_split() {
    // Minified JS: one giant line, no `fn`/`function`/`class`/`def`. Splitter
    // falls through to split_code_by_blank_lines, which produces a single
    // block since there are no blank lines either.
    let mut bytes = String::new();
    bytes.push_str("var a=1;");
    for i in 0..500 {
        bytes.push_str(&format!("var x{i}={i};"));
    }
    pack_and_reconstruct(bytes.as_bytes(), ContentType::Code, "minified");
}

#[test]
fn code_with_only_definitions_no_bodies() {
    // 50 top-level `pub fn` decls with no bodies — pure boilerplate.
    let mut bytes = String::new();
    for i in 0..50 {
        bytes.push_str(&format!("pub fn name_{i}();\n"));
    }
    pack_and_reconstruct(bytes.as_bytes(), ContentType::Code, "all-defs");
}

#[test]
fn deeply_indented_code() {
    // Indented WAY past the column-0 detection — splitter should NOT treat
    // these as boundaries (column-0 hard requirement). All lines join into
    // one block (no def-starts), falling back to blank-line splitter.
    let mut bytes = String::new();
    for _ in 0..30 {
        bytes.push_str("                                fn inner_method() {}\n");
    }
    pack_and_reconstruct(bytes.as_bytes(), ContentType::Code, "deep-indent");
}

// ---------- pathological log shapes ----------

#[test]
fn log_with_all_error_lines() {
    // Every line is an "anchor" line per is_log_anchor. With LOG_HEAD=10 + LOG_TAIL=6,
    // a 100-line all-error log still gets the head/key/tail treatment.
    let mut bytes = String::new();
    for i in 0..100 {
        bytes.push_str(&format!("error[E000{}]: bad thing at line {i}\n", i % 10));
    }
    pack_and_reconstruct(bytes.as_bytes(), ContentType::Log, "all-errors");
}

#[test]
fn log_with_one_hundred_thousand_short_lines() {
    let mut bytes = String::new();
    for i in 0..100_000 {
        bytes.push_str(&format!("line {i}\n"));
    }
    pack_and_reconstruct(bytes.as_bytes(), ContentType::Log, "100k-lines");
}

#[test]
fn log_with_no_newlines_one_giant_line() {
    let bytes = "no newlines here just a very long single line of content".repeat(1000);
    pack_and_reconstruct(bytes.as_bytes(), ContentType::Log, "one-giant-line");
}

#[test]
fn log_with_alternating_blank_and_content_lines() {
    let mut bytes = String::new();
    for i in 0..100 {
        bytes.push_str(&format!("content line {i}\n\n"));
    }
    pack_and_reconstruct(bytes.as_bytes(), ContentType::Log, "alt-blank");
}

// ---------- pathological JSON shapes ----------

#[test]
fn json_empty_object_and_array() {
    pack_and_reconstruct(b"{}", ContentType::Json, "empty-obj");
    pack_and_reconstruct(b"[]", ContentType::Json, "empty-arr");
    pack_and_reconstruct(b"  {  }  ", ContentType::Json, "obj-with-ws");
}

#[test]
fn json_root_scalar_falls_back_safely() {
    // split_json's `single = vec![(0, n)]` fallback returns one tile for root
    // scalars. Pack should still recall byte-exact.
    pack_and_reconstruct(b"42", ContentType::Json, "root-num");
    pack_and_reconstruct(b"true", ContentType::Json, "root-bool");
    pack_and_reconstruct(b"null", ContentType::Json, "root-null");
    pack_and_reconstruct(b"\"hello\"", ContentType::Json, "root-string");
}

#[test]
fn json_with_one_huge_value() {
    // A single top-level member with a 50KB string value. The splitter sees
    // one big tile; the structural compressor's elision kicks in.
    let mut bytes = String::from("{\"big\":\"");
    bytes.push_str(&"x".repeat(50_000));
    bytes.push_str("\"}");
    pack_and_reconstruct(bytes.as_bytes(), ContentType::Json, "huge-value");
}

#[test]
fn json_with_one_huge_key() {
    // A 10KB key name (technically valid JSON).
    let key = "k".repeat(10_000);
    let bytes = format!("{{\"{key}\":\"small value\"}}");
    pack_and_reconstruct(bytes.as_bytes(), ContentType::Json, "huge-key");
}

#[test]
fn json_with_many_top_level_members() {
    let mut bytes = String::from("{");
    for i in 0..1000 {
        if i > 0 {
            bytes.push(',');
        }
        bytes.push_str(&format!("\"k{i}\":{i}"));
    }
    bytes.push('}');
    pack_and_reconstruct(bytes.as_bytes(), ContentType::Json, "1k-members");
}

#[test]
fn json_with_deeply_nested_value() {
    // Nest 100 levels — JSON splitter only splits at TOP-LEVEL boundaries,
    // so deeply nested doesn't split — it's one big member tile.
    let depth = 100;
    let mut bytes = String::from("{\"k\":");
    bytes.push_str(&"[".repeat(depth));
    bytes.push('1');
    bytes.push_str(&"]".repeat(depth));
    bytes.push('}');
    pack_and_reconstruct(bytes.as_bytes(), ContentType::Json, "deeply-nested-value");
}

#[test]
fn json_with_unbalanced_braces_falls_back_safely() {
    // Malformed JSON: missing close. split_json's fallback returns a single
    // tile; reconstruct stays byte-exact regardless.
    pack_and_reconstruct(b"{\"a\":1", ContentType::Json, "missing-close");
    pack_and_reconstruct(b"{\"a\":[1,2", ContentType::Json, "missing-arr-close");
}

#[test]
fn json_with_brace_inside_string_doesnt_fool_splitter() {
    // `{"key":"value with } inside"}` — closing brace inside a string MUST
    // NOT cause split_json to terminate early. State machine tracks quote state.
    let bytes = b"{\"k1\":\"value with } and { and \\\" embedded\",\"k2\":42}";
    pack_and_reconstruct(bytes, ContentType::Json, "brace-in-string");
}

// ---------- byte-level extremes ----------

#[test]
fn null_bytes_in_input() {
    let bytes: Vec<u8> = b"before\x00after\x00more\n"
        .iter()
        .chain(b"line two\n".iter())
        .copied()
        .collect();
    pack_and_reconstruct(&bytes, ContentType::Code, "nulls-code");
    pack_and_reconstruct(&bytes, ContentType::Log, "nulls-log");
}

#[test]
fn non_utf8_bytes_in_input() {
    // Pure binary — the structural compressor decodes lossily but the store
    // keeps the bytes byte-exact, so reconstruct still works.
    let bytes: Vec<u8> = (0..=255u8).cycle().take(5000).collect();
    pack_and_reconstruct(&bytes, ContentType::Code, "binary-code");
    pack_and_reconstruct(&bytes, ContentType::Log, "binary-log");
}

#[test]
fn ansi_escape_sequences_pass_through() {
    let bytes = b"\x1b[32mok\x1b[0m\n\x1b[31mfail\x1b[0m\n";
    pack_and_reconstruct(bytes, ContentType::Log, "ansi");
}

#[test]
fn high_unicode_emoji_input() {
    let bytes: Vec<u8> = "🎒🌍😀 ".repeat(200).into_bytes();
    pack_and_reconstruct(&bytes, ContentType::Log, "emoji-spam");
}

// ---------- direct structural::compress probes ----------

#[test]
fn compress_with_zero_length_range_returns_empty() {
    let bytes = b"hello world";
    let (view, elisions) = structural::compress(bytes, 5, 5, ContentType::Log);
    assert_eq!(view, "", "zero-length range -> empty view");
    assert!(elisions.is_empty());
}

#[test]
fn compress_with_full_range() {
    let bytes = b"line one\nline two\nline three\n";
    let (view, _elisions) = structural::compress(bytes, 0, bytes.len(), ContentType::Log);
    // Short input -> the head+tail+8 guard returns it verbatim (no elision).
    assert!(view.contains("line one"));
    assert!(view.contains("line three"));
}

#[test]
fn compress_partial_range_doesnt_panic() {
    let bytes = b"alpha\nbeta\ngamma\ndelta\n";
    // Compress just the middle two lines.
    let start = "alpha\n".len();
    let end = start + "beta\ngamma\n".len();
    let (view, _) = structural::compress(bytes, start, end, ContentType::Log);
    assert!(view.contains("beta"));
    assert!(view.contains("gamma"));
}

// ---------- never-worse-than-raw on tiny inputs ----------

#[test]
fn tiny_inputs_emit_raw_bytes_view() {
    use knapsack::token_estimate::tokens_bytes;
    let store = tmpstore("tiny-rawview");
    let mut ledger = Ledger::in_memory();
    // Very small log — view should NOT be larger than raw.
    let bytes = b"ok\n";
    let r = pack(bytes, ContentType::Log, &store, &mut ledger, 0);
    let raw = tokens_bytes(bytes);
    assert!(
        r.shown_tokens_est <= raw,
        "shown ({}) must not exceed raw ({}) for tiny inputs — never-worse-than-raw guard",
        r.shown_tokens_est, raw
    );
}
