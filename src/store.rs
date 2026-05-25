//! Byte-exact, content-addressed recall store. The compact view may be lossy; the
//! store is the source of truth and `get(put(b)) == b` for ANY bytes (UTF-8 or not,
//! CRLF or LF). One file per handle, named by the handle, sharded into `<2-hex>/` subdirs
//! so large packs don't serialize every create on a single directory's lock; a pack's
//! blocks are written in parallel. Reads fall back to the pre-sharding flat layout, so
//! older caches keep resolving.

use crate::hash::{handle, Handle};
use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;

pub struct Store {
    dir: PathBuf,
}

fn sanitize(h: &str) -> String {
    h.chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' })
        .collect()
}

impl Store {
    pub fn new(dir: PathBuf) -> Self {
        let _ = fs::create_dir_all(&dir);
        Store { dir }
    }

    fn path(&self, h: &str) -> PathBuf {
        let s = sanitize(h);
        // Shard by the first 2 hex chars after the `ks_` prefix (256 buckets) so files — and
        // the NTFS per-directory creation lock — spread across subdirs; parallel creates in
        // different buckets then don't serialize on one directory's index.
        let shard = s.get(3..5).map(str::to_owned).unwrap_or_else(|| "00".into());
        self.dir.join(shard).join(s)
    }

    /// Legacy pre-sharding location: a file directly under the store root. Reads fall back to
    /// it so caches written by older versions still resolve; nothing is ever written here now.
    fn flat_path(&self, h: &str) -> PathBuf {
        self.dir.join(sanitize(h))
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
            let _ = fs::write(p, bytes);
        }
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
        // race on directory creation and never pay create_dir_all per file.
        let shards: HashSet<&str> = handles.iter().filter_map(|h| h.get(3..5)).collect();
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
    /// VERIFY-ON-READ: the handle IS the SHA-1 of the content, so we recompute it and reject
    /// any file whose bytes no longer match (bit-rot, or a torn write from a crash mid-write).
    /// A rejected copy reads as missing, which the caller re-sends — corruption self-heals
    /// rather than silently violating the byte-exact guarantee.
    pub fn get(&self, h: &Handle) -> Option<Vec<u8>> {
        for p in [self.path(h), self.flat_path(h)] {
            if let Ok(bytes) = fs::read(&p) {
                if handle(&bytes) == *h {
                    return Some(bytes);
                }
            }
        }
        None
    }

    pub fn len(&self) -> usize {
        // Sharded files live under shard subdirs; legacy flat files sit at the root. Count both.
        let Ok(top) = fs::read_dir(&self.dir) else {
            return 0;
        };
        let mut n = 0;
        for e in top.flatten() {
            let p = e.path();
            if p.is_dir() {
                n += fs::read_dir(&p).map(Iterator::count).unwrap_or(0);
            } else {
                n += 1;
            }
        }
        n
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}
