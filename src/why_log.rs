//! Structured pass-through logging for the experimental Read hook.
//!
//! Every Read PreToolUse invocation lands here as one JSONL line — what we decided
//! and why. The log is the dogfood feedback channel: a user who turns the hook on
//! can run `knapsack why-last` and see, line by line, why each Read was rewritten or
//! left alone. Append-only on purpose; `knapsack gc` is the only thing that prunes.
//!
//! The format is INTENTIONALLY small + boring: a single flat object per line, with
//! a `reason` field that maps to one of the `Reason` variants below. Add new reasons
//! by extending the enum; old log entries still parse because the reader is tolerant
//! of unknown values.

use crate::config::read_log_path;
use crate::json::{self, Json};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

/// Every reason a Read decision can take. Stable wire names (kebab-case strings) so
/// downstream `knapsack why-last` rendering stays usable across refactors.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reason {
    /// Read hook explicitly disabled via `KNAPSACK_READ_HOOK=0` (or `off`/`false`/empty).
    /// Hook never touched the call. The default is ON — this only appears when the user
    /// (or a test) flipped the off-switch.
    GateDisabled,
    /// Event didn't carry a usable `file_path`, or `tool_input` was malformed.
    BadInput,
    /// Read had `offset`/`limit` set — we don't proxy partial reads in v1.
    SlicingRequested,
    /// `fs::read` on the original failed (missing, permission denied, …).
    FileUnreadable,
    /// File is under the redirect threshold; cheaper to just read it directly.
    TooSmall,
    /// File exceeds the safety ceiling; we don't try to pack arbitrarily large files.
    TooLarge,
    /// Source SHA-256 differs from the previously cached version. A new cache view
    /// gets generated; the user just sees this in the log as a transparency signal.
    FileChanged,
    /// Compact view didn't beat the original by enough to justify the redirect.
    /// Falls through; the user reads the raw file.
    WorseThanRaw,
    /// All checks passed; we emitted an `updatedInput` redirecting `file_path` at
    /// the compact view in the read cache.
    RedirectEmitted,
    /// Cache view was usable as-is — no regeneration needed for this read.
    CacheHit,
    // ---- Reserved for future plumbing (transcript-driven residency, Claude Code
    // post-call feedback). Listed so the wire format is stable now. ----
    NotResident,
    NoTranscriptProof,
    UpdatedInputRejected,
}

impl Reason {
    pub fn as_wire(&self) -> &'static str {
        match self {
            Reason::GateDisabled => "gate-disabled",
            Reason::BadInput => "bad-input",
            Reason::SlicingRequested => "slicing-requested",
            Reason::FileUnreadable => "file-unreadable",
            Reason::TooSmall => "too-small",
            Reason::TooLarge => "too-large",
            Reason::FileChanged => "file-changed",
            Reason::WorseThanRaw => "worse-than-raw",
            Reason::RedirectEmitted => "redirect-emitted",
            Reason::CacheHit => "cache-hit",
            Reason::NotResident => "not-resident",
            Reason::NoTranscriptProof => "no-transcript-proof",
            Reason::UpdatedInputRejected => "updated-input-rejected",
        }
    }
}

#[derive(Debug, Clone)]
pub struct LogEntry {
    pub t: u64,
    pub reason: Reason,
    pub path: Option<String>,
    pub bytes: Option<u64>,
    pub raw_tokens: Option<usize>,
    pub view_tokens: Option<usize>,
    pub redirect_to: Option<String>,
    pub note: Option<String>,
}

impl LogEntry {
    pub fn new(reason: Reason) -> Self {
        Self {
            t: now(),
            reason,
            path: None,
            bytes: None,
            raw_tokens: None,
            view_tokens: None,
            redirect_to: None,
            note: None,
        }
    }
    pub fn path(mut self, p: impl Into<String>) -> Self {
        self.path = Some(p.into());
        self
    }
    pub fn bytes(mut self, n: u64) -> Self {
        self.bytes = Some(n);
        self
    }
    pub fn raw_tokens(mut self, n: usize) -> Self {
        self.raw_tokens = Some(n);
        self
    }
    pub fn view_tokens(mut self, n: usize) -> Self {
        self.view_tokens = Some(n);
        self
    }
    pub fn redirect_to(mut self, p: impl Into<String>) -> Self {
        self.redirect_to = Some(p.into());
        self
    }
    pub fn note(mut self, n: impl Into<String>) -> Self {
        self.note = Some(n.into());
        self
    }

    fn to_json(&self) -> Json {
        let mut obj: Vec<(String, Json)> = vec![
            ("t".into(), Json::Num(self.t as f64)),
            (
                "reason".into(),
                Json::Str(self.reason.as_wire().to_string()),
            ),
        ];
        if let Some(p) = &self.path {
            obj.push(("path".into(), Json::Str(p.clone())));
        }
        if let Some(n) = self.bytes {
            obj.push(("bytes".into(), Json::Num(n as f64)));
        }
        if let Some(n) = self.raw_tokens {
            obj.push(("raw_tokens".into(), Json::Num(n as f64)));
        }
        if let Some(n) = self.view_tokens {
            obj.push(("view_tokens".into(), Json::Num(n as f64)));
        }
        if let Some(p) = &self.redirect_to {
            obj.push(("redirect_to".into(), Json::Str(p.clone())));
        }
        if let Some(n) = &self.note {
            obj.push(("note".into(), Json::Str(n.clone())));
        }
        Json::Obj(obj)
    }
}

/// Append a single decision line. Failures are silent — logging must never break the
/// hook (the hook fails open; the log is observability, not control flow).
pub fn record(entry: &LogEntry) {
    write_to(&read_log_path(), entry);
}

pub fn write_to(path: &std::path::Path, entry: &LogEntry) {
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(path) {
        let mut line = json::to_string(&entry.to_json());
        line.push('\n');
        let _ = f.write_all(line.as_bytes());
    }
}

/// Read the last `n` entries from the log, newest LAST. Tolerant: lines that don't
/// parse are silently skipped (forward-compat with format extensions / torn writes).
pub fn read_last(n: usize) -> Vec<LogEntry> {
    read_last_from(&read_log_path(), n)
}

pub fn read_last_from(path: &std::path::Path, n: usize) -> Vec<LogEntry> {
    let Ok(text) = fs::read_to_string(path) else {
        return Vec::new();
    };
    let parsed: Vec<LogEntry> = text.lines().filter_map(parse_line).collect();
    let start = parsed.len().saturating_sub(n);
    parsed[start..].to_vec()
}

fn parse_line(line: &str) -> Option<LogEntry> {
    let v = json::parse(line).ok()?;
    let t = v.get("t").and_then(|x| x.as_f64())? as u64;
    let reason_str = v.get("reason").and_then(|x| x.as_str())?;
    let reason = parse_reason(reason_str)?;
    Some(LogEntry {
        t,
        reason,
        path: v.get("path").and_then(|x| x.as_str()).map(str::to_string),
        bytes: v.get("bytes").and_then(|x| x.as_f64()).map(|n| n as u64),
        raw_tokens: v
            .get("raw_tokens")
            .and_then(|x| x.as_f64())
            .map(|n| n as usize),
        view_tokens: v
            .get("view_tokens")
            .and_then(|x| x.as_f64())
            .map(|n| n as usize),
        redirect_to: v
            .get("redirect_to")
            .and_then(|x| x.as_str())
            .map(str::to_string),
        note: v.get("note").and_then(|x| x.as_str()).map(str::to_string),
    })
}

fn parse_reason(s: &str) -> Option<Reason> {
    Some(match s {
        "gate-disabled" => Reason::GateDisabled,
        "bad-input" => Reason::BadInput,
        "slicing-requested" => Reason::SlicingRequested,
        "file-unreadable" => Reason::FileUnreadable,
        "too-small" => Reason::TooSmall,
        "too-large" => Reason::TooLarge,
        "file-changed" => Reason::FileChanged,
        "worse-than-raw" => Reason::WorseThanRaw,
        "redirect-emitted" => Reason::RedirectEmitted,
        "cache-hit" => Reason::CacheHit,
        "not-resident" => Reason::NotResident,
        "no-transcript-proof" => Reason::NoTranscriptProof,
        "updated-input-rejected" => Reason::UpdatedInputRejected,
        _ => return None,
    })
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Path of the default log — exposed for the `why-last` debug command and tests.
pub fn log_path() -> PathBuf {
    read_log_path()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_one_line() {
        let dir = std::env::temp_dir().join(format!("kn-whylog-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let p = dir.join("log.jsonl");
        let entry = LogEntry::new(Reason::RedirectEmitted)
            .path("/some/file.rs")
            .bytes(2048)
            .raw_tokens(512)
            .view_tokens(120)
            .redirect_to("/tmp/cache/abc.md");
        write_to(&p, &entry);

        let back = read_last_from(&p, 10);
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].reason, Reason::RedirectEmitted);
        assert_eq!(back[0].path.as_deref(), Some("/some/file.rs"));
        assert_eq!(back[0].bytes, Some(2048));
        assert_eq!(back[0].raw_tokens, Some(512));
        assert_eq!(back[0].view_tokens, Some(120));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_last_returns_tail_in_order() {
        let dir = std::env::temp_dir().join(format!("kn-whylog-tail-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let p = dir.join("log.jsonl");
        for r in [
            Reason::TooSmall,
            Reason::GateDisabled,
            Reason::RedirectEmitted,
            Reason::WorseThanRaw,
        ] {
            write_to(&p, &LogEntry::new(r));
        }
        let tail = read_last_from(&p, 2);
        assert_eq!(tail.len(), 2);
        assert_eq!(tail[0].reason, Reason::RedirectEmitted);
        assert_eq!(tail[1].reason, Reason::WorseThanRaw);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn unknown_reason_is_skipped_not_fatal() {
        // Forward-compat: an entry from a future build with a new reason name shouldn't
        // crash `why-last` — we drop it and keep going.
        let dir = std::env::temp_dir().join(format!("kn-whylog-fwd-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let p = dir.join("log.jsonl");
        fs::write(
            &p,
            "{\"t\":1,\"reason\":\"future-reason\"}\n{\"t\":2,\"reason\":\"too-small\"}\n",
        )
        .unwrap();
        let back = read_last_from(&p, 10);
        assert_eq!(back.len(), 1, "only the known-reason line survives");
        assert_eq!(back[0].reason, Reason::TooSmall);
        let _ = fs::remove_dir_all(&dir);
    }
}
