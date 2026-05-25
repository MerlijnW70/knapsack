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
    t.starts_with("//") || t.starts_with('#') || t.starts_with('*') || t.starts_with("/*") || t.starts_with("--")
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
            let prev_ok = !matches!(prev, Some('.') ) && !matches!(prev, Some(c) if is_ident_part(c));
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
    }
}

fn compress_code(bytes: &[u8], start: usize, end: usize) -> (String, Vec<Elision>) {
    let lines: Vec<(usize, usize)> = split_lines(&bytes[start..end])
        .into_iter()
        .map(|(s, e)| (s + start, e + start))
        .collect();
    let mut out: Vec<String> = Vec::new();
    let mut elisions: Vec<Elision> = Vec::new();
    let mut run: Vec<(usize, usize)> = Vec::new();

    let flush = |run: &mut Vec<(usize, usize)>, out: &mut Vec<String>, elisions: &mut Vec<Elision>| {
        if run.is_empty() {
            return;
        }
        let bstart = run.first().unwrap().0;
        let bend = run.last().unwrap().1;
        let body_bytes = &bytes[bstart..bend];
        let body_str = String::from_utf8_lossy(body_bytes);
        let h = handle(body_bytes);
        let marker = format!("⟨body: {} lines{} — expand {}⟩", run.len(), body_hint(&body_str), h);
        if run.len() >= MIN_RUN && tokens(&body_str) > tokens(&marker) {
            elisions.push(Elision { handle: h, start: bstart, end: bend });
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

fn important(t: &str) -> bool {
    let l = t.to_lowercase();
    const W: [&str; 8] = ["error", "warn", "fail", "panic", "exception", "fatal", "denied", "refused"];
    W.iter().any(|w| l.contains(w)) || t.contains('✗') || t.contains('❌') || t.contains('●')
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

    let mut view: Vec<String> = lines[..LOG_HEAD].iter().map(|&(s, e)| text_of(s, e)).collect();
    let keys: Vec<String> = mid
        .iter()
        .map(|&(s, e)| text_of(s, e))
        .filter(|t| important(t))
        .take(LOG_MAX_KEY)
        .collect();
    if !keys.is_empty() {
        view.push("⟨key lines:⟩".to_string());
        view.extend(keys);
    }
    view.push(format!(
        "⟨elided {} lines (~{} tok) — expand {}⟩",
        mid.len(),
        tokens(&String::from_utf8_lossy(mid_bytes)),
        h
    ));
    view.extend(lines[n - LOG_TAIL..].iter().map(|&(s, e)| text_of(s, e)));

    (view.join("\n"), vec![Elision { handle: h, start: mstart, end: mend }])
}
