//! The CONDITIONAL layer — the actual contribution. Compresses a blob relative to what
//! the model has already seen (the ledger), not in isolation:
//!
//!   H(output)            <- Rucksack: unconditional, re-paid every call
//!   H(output | seen)     <- Knapsack: an unchanged+resident block costs ~1 backref
//!
//! Walk the tiling blocks; coalesce consecutive UNCHANGED+resident blocks into one
//! back-reference marker (the delta win); coalesce consecutive NEW blocks into one run
//! that is structurally compressed (so a cold first call equals the Rucksack baseline).
//! Every block's exact bytes are stored, so whole-input recall is byte-exact.

use crate::block::{count_lines, split_blocks};
use crate::content_type::ContentType;
use crate::hash::{handle, Handle};
use crate::ledger::{Ledger, Residency};
use crate::store::Store;
use crate::structural;
use crate::token_estimate::{tokens, tokens_bytes};

pub struct PackResult {
    pub view: String,
    pub raw_tokens_est: usize,
    pub shown_tokens_est: usize,
    pub saved_tokens_est: isize,
    pub handles: Vec<Handle>,
    pub delta_hits: usize,
    /// Blocks seen before but evicted, so re-sent instead of back-referenced. A backref
    /// to a paged-out block would dangle; counting these shows the eviction policy's cost.
    pub evicted_resends: usize,
    pub blocks: usize,
}

/// Mutable state for one packing pass: the input plus the sinks the two flush helpers
/// share. Bundling it (instead of threading eight arguments through free functions) keeps
/// each helper to a single `&mut self` and reads as the small state machine `pack` is.
struct Packer<'a> {
    bytes: &'a [u8],
    ct: ContentType,
    store: &'a Store,
    ledger: &'a mut Ledger,
    step: u64,
    out: Vec<String>,
    handles: Vec<Handle>,
    new_run: Vec<(usize, usize)>,
    ref_run: Vec<(usize, usize)>,
    delta_hits: usize,
    evicted_resends: usize,
}

impl Packer<'_> {
    /// Flush the pending run of NEW blocks: store each block's EXACT bytes and mark it
    /// resident (it's about to be shown), recording its token weight so budget-based
    /// eviction can reason about size, then emit one structurally compressed view.
    fn flush_new(&mut self) {
        if self.new_run.is_empty() {
            return;
        }
        // Write the whole run's blocks in parallel (file creation dominates large packs).
        let block_handles = {
            let slices: Vec<&[u8]> = self.new_run.iter().map(|&(s, e)| &self.bytes[s..e]).collect();
            self.store.put_many(&slices)
        };
        for (&(s, e), h) in self.new_run.iter().zip(block_handles) {
            self.ledger.note(&h, self.step, tokens_bytes(&self.bytes[s..e]));
            self.handles.push(h);
        }
        let rs = self.new_run.first().unwrap().0;
        let re = self.new_run.last().unwrap().1;
        let (view, elisions) = structural::compress(self.bytes, rs, re, self.ct);
        for el in elisions {
            self.store.put_with_handle(&el.handle, &self.bytes[el.start..el.end]);
            self.handles.push(el.handle);
        }
        self.out.push(view);
        self.new_run.clear();
    }

    /// Flush the pending run of UNCHANGED+resident blocks into one back-reference marker.
    fn flush_ref(&mut self) {
        if self.ref_run.is_empty() {
            return;
        }
        let rs = self.ref_run.first().unwrap().0;
        let re = self.ref_run.last().unwrap().1;
        let h = self.store.put(&self.bytes[rs..re]);
        self.out.push(format!(
            "⟨knapsack: {} block(s) / {} lines unchanged — already in context · recall {}⟩",
            self.ref_run.len(),
            count_lines(&self.bytes[rs..re]),
            h
        ));
        self.delta_hits += self.ref_run.len();
        self.handles.push(h);
        self.ref_run.clear();
    }
}

pub fn pack(bytes: &[u8], ct: ContentType, store: &Store, ledger: &mut Ledger, step: u64) -> PackResult {
    let blocks = split_blocks(bytes, ct);
    let nblocks = blocks.len();

    let mut p = Packer {
        bytes,
        ct,
        store,
        ledger,
        step,
        out: Vec::new(),
        handles: Vec::new(),
        new_run: Vec::new(),
        ref_run: Vec::new(),
        delta_hits: 0,
        evicted_resends: 0,
    };

    for &(s, e) in &blocks {
        let bh = handle(&bytes[s..e]);
        match p.ledger.residency(&bh) {
            Residency::Resident => {
                p.flush_new();
                p.ref_run.push((s, e));
            }
            other => {
                if other == Residency::Evicted {
                    p.evicted_resends += 1; // seen before but paged out -> re-sent, not backref'd
                }
                p.flush_ref();
                p.new_run.push((s, e));
            }
        }
    }
    p.flush_new();
    p.flush_ref();

    let raw = tokens_bytes(bytes);
    let conditional_view = p.out.join("\n");
    let conditional_shown = tokens(&conditional_view);
    let mut handles = p.handles;

    // NEVER-WORSE-THAN-STATELESS guard. The block delta can LOSE when change is diffuse (a
    // one-line edit invalidates a whole block) or when the buffer is so repetitive that
    // compressing it whole already elides the middle — both fragment the conditional view into
    // many small pieces that beat the stateless compressor's single-pass elision. So when the
    // conditional view is fragmented (>1 run), also compress the whole buffer in isolation and
    // emit whichever is smaller. A single run is already optimal (cold == stateless; a fully
    // resident re-read is a tiny back-ref), so skip the extra pass there.
    //
    // Every block was stored individually above, so `reconstruct` stays byte-exact regardless
    // of which view we emit; if we pick the stateless one we just also store ITS elision
    // handles so the model can still expand them.
    let (view, shown, delta_hits, evicted_resends) = if p.out.len() > 1 {
        let (stateless_view, stateless_elisions) = structural::compress(bytes, 0, bytes.len(), ct);
        let stateless_shown = tokens(&stateless_view);
        if stateless_shown < conditional_shown {
            for el in stateless_elisions {
                store.put_with_handle(&el.handle, &bytes[el.start..el.end]);
                handles.push(el.handle);
            }
            (stateless_view, stateless_shown, 0, 0) // emitted the stateless view; no delta used
        } else {
            (conditional_view, conditional_shown, p.delta_hits, p.evicted_resends)
        }
    } else {
        (conditional_view, conditional_shown, p.delta_hits, p.evicted_resends)
    };

    PackResult {
        view,
        raw_tokens_est: raw,
        shown_tokens_est: shown,
        saved_tokens_est: raw as isize - shown as isize,
        handles,
        delta_hits,
        evicted_resends,
        blocks: nblocks,
    }
}

/// Byte-exact whole-input reconstruction from the store via block handles. The
/// faithfulness guarantee, callable: `reconstruct(..) == bytes` for any packed input.
pub fn reconstruct(bytes: &[u8], ct: ContentType, store: &Store) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(bytes.len());
    for (s, e) in split_blocks(bytes, ct) {
        let h = handle(&bytes[s..e]);
        out.extend_from_slice(&store.get(&h)?);
    }
    Some(out)
}
