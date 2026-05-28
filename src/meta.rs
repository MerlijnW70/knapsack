//! Per-block sidecar metadata for `ks2_` writes. The store remains content-addressed
//! and byte-exact — the block file IS the bytes — and this is a small JSON sidecar at
//! `<block>.meta` that carries everything we'd want to know about a block beyond its
//! identity:
//!
//!   - `sha256` — the FULL 64-hex digest (the handle only carries the 128-bit prefix).
//!     This is the second verification belt: on read we recompute the full digest and
//!     reject any mismatch, so a truncated-prefix collision (theoretical for now, but
//!     the safety margin matters for archived data) can't ever return wrong bytes.
//!   - `len` — byte length. Tiny but catches a torn write that happens to land on the
//!     32-hex prefix boundary by accident, before we pay for the SHA-256.
//!   - `created_at`, `last_accessed` — unix seconds; what `gc` reads.
//!   - `ct` — content_type when known (`code`/`log`). Optional.
//!   - `source`, `session`, `project` — optional provenance fields. Not populated by
//!     the default `put` path; reserved for callers that want to record where bytes
//!     came from (and where doing so is "safe" — see the user-facing brief).
//!
//! Backwards compatibility: blocks written before this module shipped have no
//! `.meta` sidecar. Verification in `store.rs` falls back to truncated-prefix
//! `hash::verify` when meta is missing, so legacy stores keep resolving byte-exact.

use crate::json::{self, Json};
use crate::sha256::sha256_hex;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Meta {
    pub sha256: String,
    pub len: u64,
    pub created_at: u64,
    pub last_accessed: u64,
    pub content_type: Option<String>,
    pub source: Option<String>,
    pub session: Option<String>,
    pub project: Option<String>,
}

impl Meta {
    /// Build a Meta from bytes alone. Timestamps are `now`. All optional fields are
    /// None — callers that know more (`put_with_meta`) can fill them in.
    pub fn from_bytes(bytes: &[u8]) -> Self {
        let now = unix_now();
        Self {
            sha256: sha256_hex(bytes),
            len: bytes.len() as u64,
            created_at: now,
            last_accessed: now,
            content_type: None,
            source: None,
            session: None,
            project: None,
        }
    }

    /// Verify bytes belong to this meta. Length is checked FIRST as a cheap reject for
    /// torn writes / truncations; only if the length matches do we pay for the SHA-256.
    /// A bad sha256 in meta itself (corrupt meta) is treated as no-match and the caller
    /// falls back to hash::verify.
    pub fn matches(&self, bytes: &[u8]) -> bool {
        if bytes.len() as u64 != self.len {
            return false;
        }
        if self.sha256.len() != 64 || !self.sha256.chars().all(|c| c.is_ascii_hexdigit()) {
            return false;
        }
        sha256_hex(bytes) == self.sha256
    }

    pub fn to_json(&self) -> String {
        let mut obj: Vec<(String, Json)> = vec![
            ("sha256".into(), Json::Str(self.sha256.clone())),
            ("len".into(), Json::Num(self.len as f64)),
            ("created".into(), Json::Num(self.created_at as f64)),
            ("accessed".into(), Json::Num(self.last_accessed as f64)),
        ];
        if let Some(ct) = &self.content_type {
            obj.push(("ct".into(), Json::Str(ct.clone())));
        }
        if let Some(s) = &self.source {
            obj.push(("source".into(), Json::Str(s.clone())));
        }
        if let Some(s) = &self.session {
            obj.push(("session".into(), Json::Str(s.clone())));
        }
        if let Some(s) = &self.project {
            obj.push(("project".into(), Json::Str(s.clone())));
        }
        json::to_string(&Json::Obj(obj))
    }

    pub fn from_json(s: &str) -> Option<Self> {
        let v = json::parse(s).ok()?;
        Some(Self {
            sha256: v
                .get("sha256")
                .and_then(|x| x.as_str())
                .map(|s| s.to_string())?,
            len: v.get("len").and_then(|x| x.as_f64())? as u64,
            created_at: v.get("created").and_then(|x| x.as_f64()).unwrap_or(0.0) as u64,
            last_accessed: v.get("accessed").and_then(|x| x.as_f64()).unwrap_or(0.0) as u64,
            content_type: v.get("ct").and_then(|x| x.as_str()).map(|s| s.to_string()),
            source: v
                .get("source")
                .and_then(|x| x.as_str())
                .map(|s| s.to_string()),
            session: v
                .get("session")
                .and_then(|x| x.as_str())
                .map(|s| s.to_string()),
            project: v
                .get("project")
                .and_then(|x| x.as_str())
                .map(|s| s.to_string()),
        })
    }
}

pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// The sidecar path that pairs with a block at `block_path`. Keeping the bytes file
/// extension-free and the meta on `.meta` means a single `read_dir` listing tells you
/// which is which without parsing every filename.
pub fn meta_path(block_path: &Path) -> PathBuf {
    let mut p = block_path.as_os_str().to_owned();
    p.push(".meta");
    PathBuf::from(p)
}

/// Persist a new meta. Idempotent: if the file already exists (a parallel writer beat
/// us, or the block was put earlier), DO NOT overwrite — preserves the original
/// `created_at`. Write atomicity isn't critical (meta is a hint, the bytes are truth);
/// last-write-wins under contention is fine for the timestamp-class fields.
pub fn write_if_absent(meta_path: &Path, meta: &Meta) {
    if meta_path.exists() {
        return;
    }
    if let Some(parent) = meta_path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let _ = fs::write(meta_path, meta.to_json().as_bytes());
}

pub fn read(meta_path: &Path) -> Option<Meta> {
    let text = fs::read_to_string(meta_path).ok()?;
    Meta::from_json(&text)
}

/// Debounced last-access touch. Only rewrites the sidecar when `last_accessed` is
/// older than `min_age_secs`, so a hot block doesn't pay a write per read. Failures
/// are silent — meta is a hint, never a load-bearing path on read.
pub fn touch_last_accessed(meta_path: &Path, min_age_secs: u64) {
    let Some(mut m) = read(meta_path) else { return };
    let now = unix_now();
    if now.saturating_sub(m.last_accessed) < min_age_secs {
        return;
    }
    m.last_accessed = now;
    let _ = fs::write(meta_path, m.to_json().as_bytes());
}

/// Remove block + sidecar as a pair. Returns (bytes_removed, meta_removed). Either
/// side may already be absent (legacy blocks have no meta) — that's not an error,
/// the call still succeeds for the side that existed.
pub fn delete_pair(block_path: &Path) -> (bool, bool) {
    let meta = meta_path(block_path);
    let block_removed = fs::remove_file(block_path).is_ok();
    let meta_removed = fs::remove_file(&meta).is_ok();
    (block_removed, meta_removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_json() {
        let b = b"hello meta world";
        let m = Meta::from_bytes(b);
        let s = m.to_json();
        let parsed = Meta::from_json(&s).expect("parses");
        assert_eq!(parsed, m);
    }

    #[test]
    fn matches_reflects_length_and_full_sha256() {
        let bytes = b"the original exact bytes";
        let m = Meta::from_bytes(bytes);
        assert!(m.matches(bytes), "same bytes verify");
        assert!(
            !m.matches(b"different bytes here"),
            "different content rejected"
        );
        // Wrong length but byte-shorter content: rejected without paying for SHA-256.
        let mut shortened = bytes.to_vec();
        shortened.pop();
        assert!(!m.matches(&shortened), "length mismatch rejected fast");
    }

    #[test]
    fn corrupt_sha256_field_does_not_falsely_match() {
        let bytes = b"some content";
        let mut m = Meta::from_bytes(bytes);
        m.sha256 = "not-hex-at-all".into();
        assert!(!m.matches(bytes), "garbage sha256 string rejects");
    }

    #[test]
    fn meta_path_appends_meta_extension() {
        assert_eq!(
            meta_path(Path::new("/x/ks2_abc")),
            Path::new("/x/ks2_abc.meta")
        );
    }

    #[test]
    fn optional_fields_round_trip() {
        let mut m = Meta::from_bytes(b"x");
        m.content_type = Some("code".into());
        m.source = Some("Bash:cargo test".into());
        m.session = Some("sess-A".into());
        m.project = Some("knapsack".into());
        let s = m.to_json();
        let parsed = Meta::from_json(&s).unwrap();
        assert_eq!(parsed.content_type.as_deref(), Some("code"));
        assert_eq!(parsed.source.as_deref(), Some("Bash:cargo test"));
        assert_eq!(parsed.session.as_deref(), Some("sess-A"));
        assert_eq!(parsed.project.as_deref(), Some("knapsack"));
    }
}
