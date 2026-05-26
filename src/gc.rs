//! `knapsack gc` — drop cold blocks from the store.
//!
//! Per-block age comes from `meta.last_accessed` (touched on every successful `get`,
//! debounced) for ks2_ blocks. Blocks without a `.meta` sidecar — legacy `ks_…`
//! and ks2_ blocks written before meta shipped — fall back to filesystem mtime,
//! which `fs::write` updates on creation. Either way: a block reads as "cold" only
//! when nothing has touched it for the configured window.
//!
//! Cleanup is paired: every deleted block also deletes its `.meta` sidecar via
//! `Store::delete`. We never leave half a pair behind, never delete a meta whose
//! block is still wanted, never delete during a `--dry-run`.

use crate::meta;
use crate::store::Store;
use std::fs;
use std::path::{Path, PathBuf};

pub struct GcReport {
    pub scanned: usize,
    pub deleted: usize,
    pub kept: usize,
    pub bytes_freed: u64,
    pub meta_present: usize,
    pub meta_missing: usize,
    /// Read-hook cache stats (separate sub-tally so the user can see whether the
    /// experimental cache was actually a contributor). Counted into the totals above.
    pub read_cache_scanned: usize,
    pub read_cache_deleted: usize,
    pub dry_run: bool,
    pub older_than_secs: u64,
}

/// Walk every block in the store and delete those whose age (from meta.last_accessed
/// when present, otherwise fs mtime) exceeds `older_than_secs`. `dry_run = true`
/// reports what would happen without touching the filesystem.
pub fn gc(store: &Store, older_than_secs: u64, dry_run: bool) -> GcReport {
    let now = meta::unix_now();
    let mut r = GcReport {
        scanned: 0,
        deleted: 0,
        kept: 0,
        bytes_freed: 0,
        meta_present: 0,
        meta_missing: 0,
        read_cache_scanned: 0,
        read_cache_deleted: 0,
        dry_run,
        older_than_secs,
    };

    let Ok(top) = fs::read_dir(store.dir()) else {
        // The store dir may not exist (fresh install). Still walk the read cache
        // — those two directories live independently.
        gc_read_cache(now, &mut r);
        return r;
    };
    for shard_entry in top.flatten() {
        let shard_path = shard_entry.path();
        // The store has two shapes: <store>/<shard>/<handle> (sharded) and a few
        // pre-sharding `<store>/<handle>` files at the root (legacy flat). Walk both.
        if shard_path.is_dir() {
            consider_dir(&shard_path, now, &mut r, store);
        } else if is_block_file(&shard_path) {
            consider_block(&shard_path, now, &mut r, store);
        }
    }
    // The Read-hook cache is a flat directory of <sha256>.md files; the gc rule is the
    // same (drop anything whose fs mtime is older than the threshold). It's tallied
    // into the same `deleted`/`bytes_freed`/`scanned` totals AND into the read-cache
    // sub-counters so `format()` can show the contribution.
    gc_read_cache(now, &mut r);
    r
}

/// Walk the experimental Read-hook cache and apply the same age-based cleanup the
/// store gets. No meta sidecars here — these are plain compressed-view files; fs
/// mtime is the only signal. Skips silently if the cache directory doesn't exist.
fn gc_read_cache(now: u64, r: &mut GcReport) {
    let dir = crate::config::read_cache_dir();
    let Ok(entries) = fs::read_dir(&dir) else { return };
    for e in entries.flatten() {
        let p = e.path();
        if !p.is_file() {
            continue;
        }
        let len = e.metadata().map(|m| m.len()).unwrap_or(0);
        let mtime = e
            .metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|st| st.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs());
        r.scanned += 1;
        r.read_cache_scanned += 1;
        r.meta_missing += 1; // read-cache files have no .meta sidecar
        let stale = match mtime {
            Some(t) => now.saturating_sub(t) >= r.older_than_secs,
            None => false,
        };
        if !stale {
            r.kept += 1;
            continue;
        }
        r.deleted += 1;
        r.read_cache_deleted += 1;
        r.bytes_freed += len;
        if !r.dry_run {
            let _ = fs::remove_file(&p);
        }
    }
}

fn consider_dir(dir: &Path, now: u64, r: &mut GcReport, store: &Store) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let path = e.path();
        if is_block_file(&path) {
            consider_block(&path, now, r, store);
        }
    }
}

fn consider_block(block_path: &Path, now: u64, r: &mut GcReport, store: &Store) {
    r.scanned += 1;
    let meta_path = meta::meta_path(block_path);
    let (last_active, len) = block_age_and_size(block_path, &meta_path);
    if last_active.is_some() && meta_path.exists() {
        r.meta_present += 1;
    } else {
        r.meta_missing += 1;
    }
    let stale = match last_active {
        Some(t) => now.saturating_sub(t) >= r.older_than_secs,
        // No fs metadata reachable at all (already-vanished file mid-scan, weird FS).
        // Skip it — better to leave a block than to delete based on no signal.
        None => false,
    };
    if !stale {
        r.kept += 1;
        return;
    }
    r.deleted += 1;
    r.bytes_freed += len;
    if !r.dry_run {
        // Pull the handle from the file name and route through Store::delete so we
        // never get out of sync with how the store decides paths.
        if let Some(fname) = block_path.file_name().and_then(|n| n.to_str()) {
            let h: crate::hash::Handle = fname.to_string();
            store.delete(&h);
        } else {
            // Defensive fallback: no usable file name — drop the pair directly.
            meta::delete_pair(block_path);
        }
    }
}

fn block_age_and_size(block_path: &Path, meta_path: &Path) -> (Option<u64>, u64) {
    let len = fs::metadata(block_path).map(|m| m.len()).unwrap_or(0);
    // Meta-driven age: prefer last_accessed, fall back to created_at if last_accessed
    // is missing (corrupt meta) — created_at is still a real lower bound on activity.
    if let Some(m) = meta::read(meta_path) {
        let when = if m.last_accessed > 0 { m.last_accessed } else { m.created_at };
        if when > 0 {
            return (Some(when), len);
        }
    }
    // Filesystem mtime as the legacy/no-meta fallback. Reading converts SystemTime →
    // unix seconds; an unreadable mtime returns None so the caller leaves the block be.
    let mtime = fs::metadata(block_path)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|st| st.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs());
    (mtime, len)
}

fn is_block_file(p: &Path) -> bool {
    // Anything with a `.meta` extension is a sidecar — skip it; the block deletion
    // handles meta cleanup. We deliberately tolerate other extensions (none should
    // exist in a healthy store) so a manually-dropped tool file isn't garbaged out.
    p.is_file() && p.extension().and_then(|e| e.to_str()) != Some("meta")
}

/// Pretty-print a GcReport for the CLI.
pub fn format(r: &GcReport) -> String {
    let mode = if r.dry_run { "dry-run" } else { "live" };
    format!(
        "knapsack gc  ({}, older-than {} s)\n\n  \
         scanned        : {}\n  \
         deleted        : {}\n  \
         kept           : {}\n  \
         bytes freed    : {}\n  \
         meta coverage  : {}/{} blocks have .meta\n  \
         read cache     : {} scanned, {} deleted (EXPERIMENTAL)\n",
        mode,
        r.older_than_secs,
        r.scanned,
        r.deleted,
        r.kept,
        r.bytes_freed,
        r.meta_present,
        r.scanned,
        r.read_cache_scanned,
        r.read_cache_deleted
    )
}

/// Doctor's informational metadata-coverage line: returns (blocks, blocks_with_meta).
/// Walks the store the same way gc does, but never deletes; used purely for reporting.
pub fn coverage(store: &Store) -> (usize, usize) {
    let mut total = 0;
    let mut with_meta = 0;
    let Ok(top) = fs::read_dir(store.dir()) else {
        return (0, 0);
    };
    for shard_entry in top.flatten() {
        let shard_path = shard_entry.path();
        let paths: Vec<PathBuf> = if shard_path.is_dir() {
            fs::read_dir(&shard_path).map(|d| d.flatten().map(|e| e.path()).collect()).unwrap_or_default()
        } else {
            vec![shard_path]
        };
        for p in paths {
            if !is_block_file(&p) {
                continue;
            }
            total += 1;
            if meta::meta_path(&p).exists() {
                with_meta += 1;
            }
        }
    }
    (total, with_meta)
}
