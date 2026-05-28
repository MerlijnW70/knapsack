//! The UNCONDITIONAL layer — Rucksack-equivalent structural compression of a byte
//! range. Produces a LOSSY view (for display) plus a list of `Elision`s, each naming a
//! handle and the EXACT byte range it stands for. The caller stores those exact bytes,
//! so the view may normalize/strip freely while recall stays byte-exact.
//!   code -> keep signatures/comments/closers; collapse bodies into elisions
//!   log  -> keep head + key (error/warn) lines + tail; elide the middle

use crate::block::split_lines;
use crate::content_type::ContentType;
use crate::hash::{handle, Handle};
use crate::token_estimate::tokens;

pub struct Elision {
    pub handle: Handle,
    pub start: usize,
    pub end: usize,
}

const MIN_RUN: usize = 4;
const LOG_HEAD: usize = 10;
const LOG_TAIL: usize = 6;
const LOG_MAX_KEY: usize = 12;

fn line_text(bytes: &[u8], s: usize, e: usize) -> String {
    String::from_utf8_lossy(&bytes[s..e])
        .trim_end_matches('\n')
        .trim_end_matches('\r')
        .to_string()
}

fn is_comment(t: &str) -> bool {
    t.starts_with("//")
        || t.starts_with('#')
        || t.starts_with('*')
        || t.starts_with("/*")
        || t.starts_with("--")
}

fn is_closer(line: &str) -> bool {
    let l = line.trim_end();
    if l.is_empty() || l.starts_with(|c: char| c.is_whitespace()) {
        return false;
    }
    let mut bracket = false;
    for c in l.chars() {
        match c {
            '}' | ']' | ')' => bracket = true,
            ';' | ',' => {}
            _ => return false,
        }
    }
    bracket
}

fn is_structural(text: &str) -> bool {
    if text.trim().is_empty() {
        return true;
    }
    let t = text.trim_start();
    is_comment(t)
        || crate::content_type::is_sig(t)
        || is_closer(text)
        || crate::content_type::is_method(t)
}

fn is_ident_start(c: char) -> bool {
    c.is_alphabetic() || c == '_' || c == '$'
}
fn is_ident_part(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '$'
}

/// Deterministic L0 hint (mechanism ④): names up to 3 functions a body calls. No LLM,
/// nothing invented — purely what appears as `name(` not preceded by `.` or an ident.
fn body_hint(body: &str) -> String {
    const KW: [&str; 16] = [
        "if", "for", "while", "switch", "catch", "return", "function", "await", "new", "typeof",
        "throw", "else", "do", "const", "let", "var",
    ];
    let chars: Vec<char> = body.chars().collect();
    let mut out: Vec<String> = Vec::new();
    let mut i = 0usize;
    while i < chars.len() && out.len() < 3 {
        if is_ident_start(chars[i]) {
            let prev = if i > 0 { Some(chars[i - 1]) } else { None };
            let mut j = i;
            while j < chars.len() && is_ident_part(chars[j]) {
                j += 1;
            }
            let name: String = chars[i..j].iter().collect();
            let mut k = j;
            while k < chars.len() && chars[k] == ' ' {
                k += 1;
            }
            let followed_paren = k < chars.len() && chars[k] == '(';
            let prev_ok =
                !matches!(prev, Some('.')) && !matches!(prev, Some(c) if is_ident_part(c));
            if followed_paren && prev_ok && !KW.contains(&name.as_str()) && !out.contains(&name) {
                out.push(name);
            }
            i = j;
        } else {
            i += 1;
        }
    }
    if out.is_empty() {
        String::new()
    } else {
        format!(" · calls {}", out.join(", "))
    }
}

pub fn compress(bytes: &[u8], start: usize, end: usize, ct: ContentType) -> (String, Vec<Elision>) {
    match ct {
        ContentType::Code => compress_code(bytes, start, end),
        ContentType::Log => compress_log(bytes, start, end),
        ContentType::Json => compress_json(bytes, start, end),
    }
}

/// Cold-pass compression for JSON: each top-level member kept verbatim when small,
/// or replaced by `"<key>": [Knapsack: ~N tokens · recall ks2_…]` when large. The
/// elided bytes go in the store under their own handle so `knapsack expand` returns
/// the exact original member. Falls back to verbatim if the input doesn't split into
/// useful tiles (malformed JSON, root scalar, single tile).
fn compress_json(bytes: &[u8], start: usize, end: usize) -> (String, Vec<Elision>) {
    use crate::block::split_json;
    if end <= start {
        return (String::new(), Vec::new());
    }
    // Split the sub-range exactly the way pack.rs's block layer did. Tiles here are
    // RELATIVE to `start`; we'll add `start` back when storing elisions so they index
    // into the ORIGINAL bytes (Elision.start/end are absolute by contract).
    let region_len = end - start;
    let region = &bytes[start..end];
    let tiles_rel = split_json(region);
    if tiles_rel.len() <= 1 {
        // Nothing useful to split; emit verbatim. Cold-pass falls back to "view == raw"
        // and the never-worse-than-stateless guard in pack.rs handles the rest.
        return (lossy_string(region), Vec::new());
    }

    // Per-member elision threshold. A member smaller than this is cheaper to ship
    // verbatim than to round-trip a marker + handle (which costs ~30–50 tokens).
    const KEEP_BYTES: usize = 240;

    let mut out = String::new();
    let mut elisions: Vec<Elision> = Vec::new();
    let last = tiles_rel.len() - 1;

    for (i, &(rs, re)) in tiles_rel.iter().enumerate() {
        let abs_s = start + rs;
        let abs_e = start + re;
        let tile = &bytes[abs_s..abs_e];
        let is_framing_open = i == 0;
        let is_framing_close =
            i == last || (i == last - 1 && tile.len() == 1 && matches!(tile[0], b'}' | b']'));
        // Framing tiles (the opening `{`/`[`, the closing `}`/`]`, and any trailing
        // whitespace) always pass through verbatim — they're tiny and necessary for
        // the view to look like JSON.
        if is_framing_open || is_framing_close {
            out.push_str(&lossy_string(tile));
            continue;
        }
        // Interior tile: a top-level member with optional leading whitespace and
        // optional trailing comma. Decide keep vs elide on the trimmed length.
        let lead = lead_ws(tile);
        let payload_len = tile.len().saturating_sub(lead);
        if payload_len <= KEEP_BYTES {
            out.push_str(&lossy_string(tile));
            continue;
        }
        // Large member: store exact bytes, emit marker. Try to extract the key name
        // so the lossy view stays scannable — "dependencies" omitted reads better
        // than an anonymous marker.
        let h = handle(tile);
        elisions.push(Elision {
            handle: h.clone(),
            start: abs_s,
            end: abs_e,
        });
        let key = extract_member_key(&tile[lead..]);
        let toks = tokens(&String::from_utf8_lossy(tile));
        let trailing_comma = tile.last() == Some(&b',');
        // Preserve the leading whitespace so the view's indentation looks like JSON.
        out.push_str(&lossy_string(&tile[..lead]));
        match key {
            Some(k) => out.push_str(&format!(
                "\"{k}\": [Knapsack: section omitted · ~{toks} tokens · recall {h}]{c}",
                c = if trailing_comma { "," } else { "" },
            )),
            None => out.push_str(&format!(
                "[Knapsack: section omitted · ~{toks} tokens · recall {h}]{c}",
                c = if trailing_comma { "," } else { "" },
            )),
        }
    }

    let _ = region_len;
    (out, elisions)
}

fn lossy_string(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

fn lead_ws(tile: &[u8]) -> usize {
    let mut i = 0;
    while i < tile.len() && matches!(tile[i], b' ' | b'\t' | b'\r' | b'\n') {
        i += 1;
    }
    i
}

/// Pull `key` out of a `"key": …` tile body (leading whitespace already skipped). None
/// if the tile doesn't start with a quoted string — common for array elements and
/// malformed shapes; the caller falls back to an anonymous marker.
fn extract_member_key(body: &[u8]) -> Option<String> {
    if body.first() != Some(&b'"') {
        return None;
    }
    let mut i = 1;
    let mut escape = false;
    while i < body.len() {
        let c = body[i];
        if escape {
            escape = false;
        } else if c == b'\\' {
            escape = true;
        } else if c == b'"' {
            return std::str::from_utf8(&body[1..i]).ok().map(str::to_string);
        }
        i += 1;
    }
    None
}

fn compress_code(bytes: &[u8], start: usize, end: usize) -> (String, Vec<Elision>) {
    let lines: Vec<(usize, usize)> = split_lines(&bytes[start..end])
        .into_iter()
        .map(|(s, e)| (s + start, e + start))
        .collect();
    let mut out: Vec<String> = Vec::new();
    let mut elisions: Vec<Elision> = Vec::new();
    let mut run: Vec<(usize, usize)> = Vec::new();

    let flush =
        |run: &mut Vec<(usize, usize)>, out: &mut Vec<String>, elisions: &mut Vec<Elision>| {
            if run.is_empty() {
                return;
            }
            let bstart = run.first().unwrap().0;
            let bend = run.last().unwrap().1;
            let body_bytes = &bytes[bstart..bend];
            let body_str = String::from_utf8_lossy(body_bytes);
            let h = handle(body_bytes);
            // Match pack.rs / pack_doc style: plain ASCII brackets, capital K, `recall` not
            // `expand` (consistent vocabulary across all elision markers).
            let marker = format!(
                "[Knapsack: {}-line body{} · recall {}]",
                run.len(),
                body_hint(&body_str),
                h
            );
            if run.len() >= MIN_RUN && tokens(&body_str) > tokens(&marker) {
                elisions.push(Elision {
                    handle: h,
                    start: bstart,
                    end: bend,
                });
                out.push(marker);
            } else {
                for &(s, e) in run.iter() {
                    out.push(line_text(bytes, s, e));
                }
            }
            run.clear();
        };

    for &(s, e) in &lines {
        let text = line_text(bytes, s, e);
        if is_structural(&text) {
            flush(&mut run, &mut out, &mut elisions);
            out.push(text);
        } else {
            run.push((s, e));
        }
    }
    flush(&mut run, &mut out, &mut elisions);
    (out.join("\n"), elisions)
}

/// True when a log line is a likely diagnostic anchor — the kind of line a user reads
/// FIRST when staring at noisy test/build output. Catches generic keywords plus the
/// specific framings that compilers and test runners actually print. Each new family
/// has a test fixture in `tests/log_anchors.rs`; if you add one here, add the fixture.
fn important(t: &str) -> bool {
    let l = t.to_lowercase();
    let tt = t.trim_start();
    let lt = l.trim_start();

    // 1. Generic severity keywords + visual markers (original set).
    const W: [&str; 8] = [
        "error",
        "warn",
        "fail",
        "panic",
        "exception",
        "fatal",
        "denied",
        "refused",
    ];
    if W.iter().any(|w| l.contains(w)) {
        return true;
    }
    if t.contains('✗') || t.contains('❌') || t.contains('●') {
        return true;
    }

    // 2. Python tracebacks. The `Traceback (most recent call last):` header is the
    //    anchor; the per-frame `File "x.py", line N, in fn` lines locate the crash.
    //    Neither contains a severity keyword, so both would otherwise be elided.
    if lt.starts_with("traceback (most recent call last)") {
        return true;
    }
    if lt.starts_with("file \"") && lt.contains("\", line ") {
        return true;
    }

    // 3. Node.js stack frames: `    at functionName (path/to/file.js:12:34)` or the
    //    bare `at path:line:col`. Same anchor logic — no severity word, but every
    //    debug session needs them visible.
    if tt.starts_with("at ") && tt.contains(':') {
        return true;
    }

    // 4. Java/JVM/Gradle. "Caused by:" introduces nested exceptions; Gradle prints
    //    "* What went wrong:" / "* Try:" sections; ":task FAILED" is already caught by
    //    "fail", but the `BUILD FAILED` summary lives at the top of the line.
    if lt.starts_with("caused by:")
        || tt.starts_with("* What went wrong")
        || tt.starts_with("* Try:")
    {
        return true;
    }

    // 5. npm prints `npm ERR! …` (note the `!`) and `npm WARN …`. The `ERR!` token
    //    on its own does not contain "error", so the generic list misses it.
    if l.contains("npm err!") || l.contains("npm warn") {
        return true;
    }

    // 6. TypeScript `tsc` diagnostics: `path/to/file.ts(12,34): error TS1234: …`.
    //    "error TS" / "warning TS" is the discriminator. Caught by "error" today, but
    //    keep it as an explicit anchor so a future severity-word trim doesn't break it.
    if l.contains(": error ts") || l.contains(": warning ts") {
        return true;
    }

    // 7. Rust diagnostic codes: `error[E0XXX]: …`, `warning: unused variable …`.
    //    Already caught by "error"/"warn", but `error[E…]` is a strong anchor and
    //    listing it makes the intent legible to future readers of this function.
    if lt.starts_with("error[e") {
        return true;
    }

    // 8. Bazel: `ERROR: /path/BUILD:1:2: …` — caught by "error".
    //    pytest summary `FAILED tests/x.py::test_foo` — caught by "fail".
    //    Listed only as comments; no extra check needed.

    false
}

fn compress_log(bytes: &[u8], start: usize, end: usize) -> (String, Vec<Elision>) {
    let lines: Vec<(usize, usize)> = split_lines(&bytes[start..end])
        .into_iter()
        .map(|(s, e)| (s + start, e + start))
        .collect();
    let n = lines.len();
    let text_of = |s: usize, e: usize| line_text(bytes, s, e);

    if n <= LOG_HEAD + LOG_TAIL + 8 {
        let view: Vec<String> = lines.iter().map(|&(s, e)| text_of(s, e)).collect();
        return (view.join("\n"), Vec::new());
    }
    let mid = &lines[LOG_HEAD..n - LOG_TAIL];
    let mstart = mid.first().unwrap().0;
    let mend = mid.last().unwrap().1;
    let mid_bytes = &bytes[mstart..mend];
    let h = handle(mid_bytes);

    let mut view: Vec<String> = lines[..LOG_HEAD]
        .iter()
        .map(|&(s, e)| text_of(s, e))
        .collect();
    let keys: Vec<String> = mid
        .iter()
        .map(|&(s, e)| text_of(s, e))
        .filter(|t| important(t))
        .take(LOG_MAX_KEY)
        .collect();
    if !keys.is_empty() {
        view.push("[Knapsack: key lines from the elided middle]".to_string());
        view.extend(keys);
    }
    view.push(format!(
        "[Knapsack: {} lines elided · ~{} tok · recall {}]",
        mid.len(),
        tokens(&String::from_utf8_lossy(mid_bytes)),
        h
    ));
    view.extend(lines[n - LOG_TAIL..].iter().map(|&(s, e)| text_of(s, e)));

    (
        view.join("\n"),
        vec![Elision {
            handle: h,
            start: mstart,
            end: mend,
        }],
    )
}
