//! Byte-exact, content-addressed recall store. The compact view may be lossy; the
//! store is the source of truth and `get(put(b)) == b` for ANY bytes (UTF-8 or not,
//! CRLF or LF). One file per handle, named by the handle, sharded into `<2-hex>/` subdirs
//! so large packs don't serialize every create on a single directory's lock; a pack's
//! blocks are written in parallel. Reads fall back to the pre-sharding flat layout, so
//! older caches keep resolving.

use crate::hash::{handle, verify, Handle};
use crate::meta::{self, Meta};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

/// How long a block's `last_accessed` must be stale before a successful `get` rewrites
/// the meta sidecar. 60 s keeps a hot block from paying a write per read while still
/// letting `gc` see fresh activity at minute resolution.
const LAST_ACCESS_DEBOUNCE_SECS: u64 = 60;

pub struct Store {
    dir: PathBuf,
    /// Session id stamped into the `.meta` sidecar of every block this Store writes.
    /// When set, `expand_handle` later attributes the refetch token cost to THIS
    /// session — so per-session net (saved minus refetched) is coherent across
    /// processes. Bash hook + MCP server live in separate processes; without this
    /// linkage they'd land under different session ids and the per-session report
    /// would show the recall-only session with a misleading negative net.
    ///
    /// None means "don't stamp" — legacy callers (and tests that just want a store)
    /// keep working without thinking about sessions.
    session_id: Option<String>,
}

fn sanitize(h: &str) -> String {
    h.chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' })
        .collect()
}

impl Store {
    pub fn new(dir: PathBuf) -> Self {
        let _ = fs::create_dir_all(&dir);
        Store { dir, session_id: None }
    }

    /// Same as `new`, but every block written through this Store stamps the given
    /// session id into its `.meta` sidecar. Used by `api::pack_output` so subsequent
    /// `expand_handle` calls — from ANY process — can recover which session
    /// originally compressed each block and attribute the recall there.
    pub fn with_session(dir: PathBuf, session_id: &str) -> Self {
        let _ = fs::create_dir_all(&dir);
        Store { dir, session_id: Some(session_id.to_string()) }
    }

    fn path(&self, h: &str) -> PathBuf {
        let s = sanitize(h);
        // Shard by the first 2 hex chars of the HASH portion (after the `_` separator),
        // so files — and the NTFS per-directory creation lock — spread across 256
        // buckets. Locating `_` instead of using a fixed offset means both legacy
        // `ks_<hex>` (offset 3) and the new `ks2_<hex>` (offset 4) shard identically:
        // by the same two hex chars of the underlying hash, just from different
        // algorithms. A handle with no `_` (malformed) falls into bucket "00".
        let hash_start = s.find('_').map(|i| i + 1).unwrap_or(0);
        let shard = s.get(hash_start..hash_start + 2).map(str::to_owned).unwrap_or_else(|| "00".into());
        self.dir.join(shard).join(s)
    }

    /// Legacy pre-sharding location: a file directly under the store root. Reads fall back to
    /// it so caches written by older versions still resolve; nothing is ever written here now.
    fn flat_path(&self, h: &str) -> PathBuf {
        self.dir.join(sanitize(h))
    }

    /// Where the store dir lives — for gc to walk.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Write the meta sidecar for a freshly-stored block, but only for `ks2_` handles
    /// — legacy `ks_` blocks never had meta and we don't synthesize any. Skipping the
    /// call for non-ks2 handles keeps the legacy store layout unchanged.
    fn write_meta_for(&self, h: &Handle, bytes: &[u8]) {
        if !h.starts_with("ks2_") {
            return;
        }
        let block = self.path(h);
        let meta_path = meta::meta_path(&block);
        let mut m = Meta::from_bytes(bytes);
        m.session = self.session_id.clone(); // None when Store::new was used
        meta::write_if_absent(&meta_path, &m);
    }

    /// Read the originating session id from a block's `.meta` sidecar.
    ///
    /// Returns None when:
    ///   - the handle is legacy `ks_*` (never had meta)
    ///   - the meta file doesn't exist (older block, or written by a Store::new)
    ///   - the meta file's `session` field is absent
    ///
    /// Backwards-compatible by design: any of those cases means callers fall through
    /// to whatever session_id the caller passed in. This is the lookup that lets
    /// `expand_handle` attribute the refetch to the session that originally stored
    /// the block, not the process doing the recall.
    pub fn block_session(&self, h: &Handle) -> Option<String> {
        if !h.starts_with("ks2_") {
            return None;
        }
        // Try the sharded path first (current layout), then the legacy flat layout.
        for block in [self.path(h), self.flat_path(h)] {
            if !block.exists() {
                continue;
            }
            let mp = meta::meta_path(&block);
            if let Some(m) = meta::read(&mp) {
                if let Some(s) = m.session {
                    return Some(s);
                }
            }
            return None; // block exists but no usable session field — don't keep looking
        }
        None
    }

    /// Store exact bytes, returning their handle. Idempotent: identical content writes once.
    pub fn put(&self, bytes: &[u8]) -> Handle {
        let h = handle(bytes);
        self.put_with_handle(&h, bytes);
        h
    }

    pub fn put_with_handle(&self, h: &Handle, bytes: &[u8]) {
        let p = self.path(h);
        if !p.exists() {
            if let Some(parent) = p.parent() {
                let _ = fs::create_dir_all(parent);
            }
            let _ = fs::write(&p, bytes);
        }
        // Meta is written even when bytes already existed (a previous version may have
        // stored the block without meta) — `write_if_absent` makes that idempotent.
        self.write_meta_for(h, bytes);
    }

    /// Store many blocks at once, returning their handles in input order. File creation is
    /// the dominant cost on large packs and is I/O-wait bound, so the writes are spread over
    /// a small pool of scoped threads to overlap the waits. Each block still lands in its own
    /// content-addressed file (identical format to `put`), so dedup, byte-exactness and crash
    /// safety are unchanged; duplicate blocks map to the same path and write identical bytes.
    pub fn put_many(&self, blocks: &[&[u8]]) -> Vec<Handle> {
        let handles: Vec<Handle> = blocks.iter().map(|b| handle(b)).collect();
        // Not worth spawning threads for a trivial run.
        if blocks.len() < 2 {
            if let Some(b) = blocks.first() {
                self.put_with_handle(&handles[0], b);
            }
            return handles;
        }
        // Pre-create the DISTINCT shard dirs once (≤256), so the parallel writers below never
        // race on directory creation and never pay create_dir_all per file. The shard
        // computation MUST agree with `path()` — both locate `_` rather than using a fixed
        // offset, so legacy `ks_<hex>` and new `ks2_<hex>` both shard by the same two hex
        // chars of the hash. A mismatch here would skip pre-create and the parallel
        // writers would silently lose blocks (the parent dir wouldn't exist).
        let shards: HashSet<String> = handles
            .iter()
            .filter_map(|h| {
                let i = h.find('_').map(|i| i + 1)?;
                h.get(i..i + 2).map(str::to_owned)
            })
            .collect();
        for shard in shards {
            let _ = fs::create_dir_all(self.dir.join(shard));
        }
        let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).min(blocks.len());
        let chunk = handles.len().div_ceil(threads);
        let this = self; // shared, Sync borrow so each thread reuses the same path() logic
        std::thread::scope(|scope| {
            for t in 0..threads {
                let lo = t * chunk;
                if lo >= handles.len() {
                    break;
                }
                let hi = (lo + chunk).min(handles.len());
                let hs = &handles[lo..hi];
                let bs = &blocks[lo..hi];
                scope.spawn(move || {
                    for (h, b) in hs.iter().zip(bs.iter()) {
                        let p = this.path(h);
                        if !p.exists() {
                            let _ = fs::write(&p, *b);
                        }
                        // Meta write piggybacks on the same parallel scope so a large
                        // pack stores everything in one pass. Idempotent + cheap.
                        this.write_meta_for(h, b);
                    }
                });
            }
        });
        handles
    }

    /// Exact bytes for a handle, or None if unknown. Tries the sharded layout first, then the
    /// legacy flat path — so when both exist the sharded copy deterministically wins, and a
    /// corrupt sharded copy still falls back to a valid flat one. New writes are always
    /// sharded; old flat files are left untouched and migrate as content is repacked.
    ///
    /// VERIFY-ON-READ — handle commitment ALWAYS, meta as extra strength:
    ///   1. `hash::verify(handle, bytes)` ALWAYS runs first. The handle's truncated
    ///      prefix (128-bit SHA-256 for ks2_, truncated SHA-1 for legacy ks_) is a
    ///      cryptographic commitment to the ORIGINAL bytes. If this fails, the bytes
    ///      on disk are not what the handle promised — reject regardless of meta state.
    ///   2. IF a `.meta` sidecar exists, additionally check `meta.matches(bytes)` —
    ///      that's `len` then full 64-hex SHA-256. Strengthens the 128-bit handle
    ///      prefix to a full 256-bit commitment.
    ///   3. Both checks must pass for the bytes to be returned.
    ///
    /// Pre-fix bug: meta was used as a REPLACEMENT for hash::verify. An attacker (or
    /// filesystem corruption) producing a self-consistent meta+block pair where meta
    /// validated the corrupted bytes could BYPASS the handle's commitment and serve
    /// wrong bytes for the handle. Now meta is purely additive — it strengthens but
    /// never replaces the handle's cryptographic commitment. Pinned by
    /// `both_corrupt_returns_none` in tests/store_corruption.rs.
    ///
    /// On a successful sharded read, `last_accessed` is touched (debounced) so `gc`
    /// can age out cold blocks.
    pub fn get(&self, h: &Handle) -> Option<Vec<u8>> {
        for (idx, p) in [self.path(h), self.flat_path(h)].iter().enumerate() {
            let Ok(bytes) = fs::read(p) else { continue };
            // Handle commitment first — the truncated prefix is what the API uses to
            // identify the block, so it MUST hold. If bytes don't match the handle,
            // we have no business returning them no matter what meta claims.
            if !verify(h, &bytes) {
                continue;
            }
            // Meta (when present) is purely additional belt — it must agree.
            let meta_p = meta::meta_path(p);
            if let Some(m) = meta::read(&meta_p) {
                if !m.matches(&bytes) {
                    continue;
                }
            }
            // Touch only on the sharded path (idx 0). The legacy flat path is
            // intentionally read-only; we never bump access times there.
            if idx == 0 {
                meta::touch_last_accessed(&meta_p, LAST_ACCESS_DEBOUNCE_SECS);
            }
            return Some(bytes);
        }
        None
    }

    /// Remove a block and its sidecar atomically (as a pair) from the sharded layout.
    /// `gc` is the only caller. Legacy flat-layout blocks aren't touched — those are
    /// pre-format-bump and predate the meta concept. Returns true if either the block
    /// or the sidecar was removed (i.e. something existed and was cleaned up).
    pub fn delete(&self, h: &Handle) -> bool {
        let block = self.path(h);
        let (b, m) = meta::delete_pair(&block);
        b || m
    }

    pub fn len(&self) -> usize {
        // Block count: sharded blocks + legacy flat blocks. We MUST exclude `.meta`
        // sidecars from the tally — they live in the same shard directories as the
        // blocks they describe, and counting them would double-count every block.
        let Ok(top) = fs::read_dir(&self.dir) else {
            return 0;
        };
        let is_block = |p: &Path| -> bool {
            p.extension().and_then(|x| x.to_str()) != Some("meta")
        };
        let mut n = 0;
        for e in top.flatten() {
            let p = e.path();
            if p.is_dir() {
                if let Ok(sub) = fs::read_dir(&p) {
                    n += sub.flatten().filter(|e| is_block(&e.path())).count();
                }
            } else if is_block(&p) {
                n += 1;
            }
        }
        n
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}
