//! PreToolUse hook shim — the smallest live integration surface. For a *noisy* Bash
//! command it rewrites the command so its merged output pipes through `knapsack pack -`,
//! carrying the Claude Code session id. Large output then enters context compressed and
//! conditioned on what the session already saw; the original exit code is preserved.
//!
//! Faithful to Rucksack's proven contract:
//!   in  (stdin):  {"tool_name":"Bash","tool_input":{"command":...},"session_id":...,"cwd":...}
//!   out (stdout): {"hookSpecificOutput":{"hookEventName":"PreToolUse","updatedInput":{...}}}
//!   FAIL OPEN: on any doubt, emit nothing -> Claude runs the command unchanged.
//!
//! Decision (deny_shell_meta): wrap only a known allowlist of high-volume programs, and
//! never touch a command with a pipe, redirect, background &, `#` comment, or an existing
//! knapsack/rucksack call — to preserve the shell's exact semantics.

use crate::json::{self, Json};
use std::io::Read;

const COMMANDS: [&str; 41] = [
    "npm",
    "pnpm",
    "yarn",
    "cargo",
    "go",
    "make",
    "gradle",
    "mvn",
    "bundle",
    "pip",
    "poetry",
    "node",
    "bun",
    "deno",
    "dotnet",
    "gradlew",
    "rspec",
    "phpunit",
    "ctest",
    "tox",
    "nox",
    "ninja",
    "swift",
    "tsx",
    "psql",
    "mysql",
    "sqlite3",
    "pytest",
    "jest",
    "vitest",
    "mocha",
    "tsc",
    "eslint",
    "prettier",
    "webpack",
    "vite",
    "rollup",
    "docker",
    "kubectl",
    "terraform",
    "ansible",
];
const EXTRA: [&str; 6] = ["grep", "rg", "ag", "find", "tree", "du"];
const LOG_COMMANDS: [&str; 3] = ["tail", "head", "cat"];
const WRAPPERS: [&str; 6] = ["sudo", "env", "time", "nice", "command", "npx"];

pub struct Decision {
    pub wrap: bool,
    pub matched: Option<String>,
}

fn basename(token: &str) -> &str {
    token.rsplit(['/', '\\']).next().unwrap_or(token)
}

/// Blank out quoted spans so shell-operator scanning ignores `|`/`>` inside strings
/// (e.g. `rg 'a|b'`). Length-preserving.
fn strip_quoted(cmd: &str) -> String {
    let mut out = String::with_capacity(cmd.len());
    let mut q: Option<char> = None;
    for c in cmd.chars() {
        match q {
            Some(qc) => {
                out.push(' ');
                if c == qc {
                    q = None;
                }
            }
            None => {
                if c == '"' || c == '\'' {
                    q = Some(c);
                    out.push(' ');
                } else {
                    out.push(c);
                }
            }
        }
    }
    out
}

fn has_shell_meta(cmd: &str) -> bool {
    let scan = strip_quoted(cmd);
    let bytes: Vec<char> = scan.chars().collect();
    if scan.contains('|') || scan.contains('<') || scan.contains('>') {
        return true;
    }
    if scan.contains("knapsack") || scan.contains("rucksack") {
        return true;
    }
    // standalone & (background), not && and not the & in 2>&1 / &>file
    for (i, &c) in bytes.iter().enumerate() {
        if c == '&' {
            let prev = if i > 0 { bytes[i - 1] } else { ' ' };
            let next = if i + 1 < bytes.len() {
                bytes[i + 1]
            } else {
                ' '
            };
            if prev != '&' && next != '&' && prev != '>' && next != '>' {
                return true;
            }
        }
    }
    // an unquoted # after start/whitespace/metachar -> shell comment
    for (i, &c) in bytes.iter().enumerate() {
        if c == '#' {
            let prev = if i > 0 { bytes[i - 1] } else { ' ' };
            if i == 0 || matches!(prev, ' ' | '\t' | ';' | '(' | ')' | '|' | '&' | '<' | '>') {
                return true;
            }
        }
    }
    false
}

/// Split into top-level segments on `&&` and `;` (quote-aware).
fn split_segments(cmd: &str) -> Vec<String> {
    let mut segs = Vec::new();
    let mut cur = String::new();
    let mut q: Option<char> = None;
    let ch: Vec<char> = cmd.chars().collect();
    let mut i = 0;
    while i < ch.len() {
        let c = ch[i];
        if let Some(qc) = q {
            cur.push(c);
            if c == qc {
                q = None;
            }
            i += 1;
            continue;
        }
        if c == '"' || c == '\'' {
            q = Some(c);
            cur.push(c);
            i += 1;
            continue;
        }
        if c == ';' {
            segs.push(std::mem::take(&mut cur));
            i += 1;
            continue;
        }
        if c == '&' && ch.get(i + 1) == Some(&'&') {
            segs.push(std::mem::take(&mut cur));
            i += 2;
            continue;
        }
        cur.push(c);
        i += 1;
    }
    segs.push(cur);
    segs.into_iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Strip leading `VAR=value` assignments and known wrappers; return the effective program.
fn effective_program(segment: &str) -> Option<String> {
    let mut s = segment.trim();
    loop {
        if let Some(rest) = strip_env_assign(s) {
            s = rest.trim_start();
            continue;
        }
        let tok = s.split_whitespace().next().unwrap_or("");
        if WRAPPERS.contains(&basename(tok)) {
            s = s[tok.len()..].trim_start();
            continue;
        }
        break;
    }
    s.split_whitespace().next().map(|t| basename(t).to_string())
}

/// If `s` starts with `IDENT=value` (value may be quoted), return the remainder.
fn strip_env_assign(s: &str) -> Option<&str> {
    let ch: Vec<char> = s.chars().collect();
    if ch.is_empty() || !(ch[0].is_ascii_alphabetic() || ch[0] == '_') {
        return None;
    }
    let mut i = 0;
    while i < ch.len() && (ch[i].is_ascii_alphanumeric() || ch[i] == '_') {
        i += 1;
    }
    if i >= ch.len() || ch[i] != '=' {
        return None;
    }
    i += 1; // past '='
            // consume the value: quoted span, or up to whitespace
    if i < ch.len() && (ch[i] == '"' || ch[i] == '\'') {
        let q = ch[i];
        i += 1;
        while i < ch.len() && ch[i] != q {
            i += 1;
        }
        if i < ch.len() {
            i += 1;
        }
    } else {
        while i < ch.len() && !ch[i].is_whitespace() {
            i += 1;
        }
    }
    // byte index for the char index i
    let byte_idx: usize = s.char_indices().nth(i).map(|(b, _)| b).unwrap_or(s.len());
    Some(&s[byte_idx..])
}

fn match_effective(segment: &str) -> Option<String> {
    let prog = effective_program(segment)?;
    if COMMANDS.contains(&prog.as_str()) || EXTRA.contains(&prog.as_str()) {
        return Some(prog);
    }
    if LOG_COMMANDS.contains(&prog.as_str()) && segment.contains(".log") {
        return Some(prog);
    }
    None
}

pub fn decide(cmd: &str) -> Decision {
    let cmd = cmd.trim();
    if cmd.is_empty() || has_shell_meta(cmd) {
        return Decision {
            wrap: false,
            matched: None,
        };
    }
    for seg in split_segments(cmd) {
        if let Some(m) = match_effective(&seg) {
            return Decision {
                wrap: true,
                matched: Some(m),
            };
        }
    }
    Decision {
        wrap: false,
        matched: None,
    }
}

/// Build the shell command Claude Code will run instead: run the original, capture its
/// exit code to a temp file, merge stderr, pipe through `knapsack pack -`, re-raise the
/// code. Portable bash/POSIX sh; mktemp'd + trap-cleaned (ported from Rucksack).
///
/// `transcript_path` is appended as `--transcript "..."` when present; the packer uses
/// it to gate "already in context" backrefs against the live transcript (so /clear
/// can't leave dangling backrefs). Empty/None means "no transcript-driven gating" —
/// the safe-fallback contract from the brief, identical to behaviour before this
/// argument existed.
pub fn wrap_command(
    cmd: &str,
    bin: &str,
    session: &str,
    prog: &str,
    transcript_path: Option<&str>,
) -> String {
    let inner = cmd.trim_end_matches([';', ' ']);
    let transcript_arg = match transcript_path {
        Some(p) if !p.trim().is_empty() => format!(" --transcript \"{}\"", p),
        _ => String::new(),
    };
    format!(
        "__kn=$(mktemp 2>/dev/null || echo \"${{TMPDIR:-/tmp}}/kn_ec.$$\") ; \
         trap 'rm -f \"$__kn\"' EXIT INT TERM ; \
         {{ {inner} ; echo $? > \"$__kn\" ; }} 2>&1 | \"{bin}\" pack - --session \"{session}\" --cmd \"{prog}\"{transcript_arg} ; \
         exit \"$(cat \"$__kn\" 2>/dev/null || echo 0)\"",
        inner = inner,
        bin = bin,
        session = session,
        prog = prog,
        transcript_arg = transcript_arg,
    )
}

fn day_stamp() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("{}", secs / 86_400)
}

/// Stable session key. Primary: Claude Code's session_id. Fallback: a per-cwd, per-day
/// key (never merges unrelated directories or days — bad cross-session residency is a
/// correctness trap).
fn session_key(evt: &Json) -> String {
    if let Some(s) = evt.get("session_id").and_then(|v| v.as_str()) {
        if !s.trim().is_empty() {
            return s.to_string();
        }
    }
    let cwd = evt.get("cwd").and_then(|v| v.as_str()).unwrap_or("");
    format!(
        "fallback-{}-{}",
        &crate::hash::sha1_hex(cwd.as_bytes())[..8],
        day_stamp()
    )
}

/// PreToolUse entry point. Always exits 0; emits the rewrite only when wrapping.
pub fn run_hook() {
    let mut input = String::new();
    if std::io::stdin().read_to_string(&mut input).is_err() {
        return; // fail open
    }
    let evt = match json::parse(&input) {
        Ok(v) => v,
        Err(_) => return,
    };
    // Dispatch on tool_name. Bash drives output reduction; Read drives input reduction
    // (both gated inside their own modules — Read can be disabled with KNAPSACK_READ_HOOK=0
    // as an off-switch). Anything else passes through unchanged — fail-open contract preserved.
    match evt.get("tool_name").and_then(|v| v.as_str()) {
        Some("Bash") => {}
        Some("Read") => {
            crate::read_hook::run(&evt);
            return;
        }
        _ => return,
    }
    let tool_input = match evt.get("tool_input") {
        Some(t) => t,
        None => return,
    };
    let cmd = match tool_input.get("command").and_then(|v| v.as_str()) {
        Some(c) if !c.trim().is_empty() => c.to_string(),
        _ => return,
    };
    let dec = decide(&cmd);
    if !dec.wrap {
        return;
    }
    let bin = match std::env::current_exe() {
        Ok(p) => p.to_string_lossy().replace('\\', "/"),
        Err(_) => return,
    };
    let session = session_key(&evt);
    // transcript_path is optional in the PreToolUse payload (older Claude Code builds
    // may not include it). We pass through whatever we got and let pack treat the
    // missing/unreadable case as "no gating" — see api::pack_output + transcript::scan.
    let transcript_path = evt.get("transcript_path").and_then(|v| v.as_str());
    let wrapped = wrap_command(
        &cmd,
        &bin,
        &session,
        dec.matched.as_deref().unwrap_or(""),
        transcript_path,
    );

    let mut obj = match tool_input {
        Json::Obj(o) => o.clone(),
        _ => return,
    };
    json::set_key(&mut obj, "command", Json::Str(wrapped));

    let out = Json::Obj(vec![(
        "hookSpecificOutput".into(),
        Json::Obj(vec![
            ("hookEventName".into(), Json::Str("PreToolUse".into())),
            ("updatedInput".into(), Json::Obj(obj)),
        ]),
    )]);
    print!("{}", json::to_string(&out));
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn wraps_plain_noisy() {
        assert!(decide("cargo test").wrap);
        assert!(decide("npm run build").wrap);
        assert!(decide("NODE_ENV=production npm run build").wrap);
        assert!(decide("sudo cargo build").wrap);
        assert!(decide("cd app && pytest").wrap);
        assert_eq!(decide("cargo test").matched.as_deref(), Some("cargo"));
    }
    #[test]
    fn skips_meta_and_unknown() {
        assert!(!decide("npm test | tail").wrap); // pipe
        assert!(!decide("cargo build 2>&1").wrap); // redirect
        assert!(!decide("npm test &").wrap); // background
        assert!(!decide("echo remember to npm install").wrap); // echo not on allowlist
        assert!(
            decide("rg 'a|b' src/").wrap,
            "a pipe inside quotes is not a shell operator"
        );
    }
    #[test]
    fn wrap_command_shape() {
        let w = wrap_command("cargo test", "/path/knapsack", "sess-1", "cargo", None);
        assert!(w.contains("cargo test"));
        assert!(w.contains("\"/path/knapsack\" pack - --session \"sess-1\""));
        assert!(w.contains("exit "));
        assert!(!w.contains("--transcript"), "no transcript -> no flag");
    }

    #[test]
    fn wrap_command_passes_transcript_flag_when_present() {
        let w = wrap_command(
            "cargo test",
            "/path/knapsack",
            "sess-1",
            "cargo",
            Some("/tmp/cc-transcript.jsonl"),
        );
        assert!(
            w.contains("--transcript \"/tmp/cc-transcript.jsonl\""),
            "transcript path flows through into the pack invocation:\n{}",
            w
        );
    }
}
