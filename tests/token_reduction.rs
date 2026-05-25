//! Honest, broad measurement of the token reduction Knapsack actually delivers.
//!
//! Three engines, scored on the SAME estimator over multi-turn sessions:
//!   OFF       raw tool output every turn                         (baseline bill)
//!   RUCKSACK  stateless structural compression each turn         H(output)
//!   KNAPSACK  conditional: structural + delta vs the seen-ledger  H(output | seen)
//!
//! It deliberately includes the UNFLATTERING cases (cold first read, fully-changing
//! output, incompressible random) and an over-expansion anti-pattern, and asserts bounds
//! on them — so the headline number can't be cherry-picked. Run the numbers with:
//!   cargo test --test token_reduction -- --nocapture

use knapsack::content_type::ContentType;
use knapsack::structural;
use knapsack::{pack, reconstruct, tokens, tokens_bytes, Ledger, Store};
use std::path::PathBuf;

type Turn = (Vec<u8>, ContentType);

struct Totals {
    off: usize,
    rucksack: usize,
    knapsack: usize,
    /// What an agent pays if it reflexively expands the whole region back every turn: it
    /// consumed the compact view AND then the full content. The honest downside of over-recall.
    knapsack_if_fully_expanded: usize,
    delta_hits: usize,
    handles: usize,
}

fn store_dir(tag: &str) -> PathBuf {
    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    std::env::temp_dir().join(format!("knapsack-tokred-{}-{}-{}", tag, std::process::id(), t))
}

/// Run a session through all three engines, sharing one ledger for Knapsack.
fn measure(tag: &str, turns: &[Turn]) -> Totals {
    let dir = store_dir(tag);
    let store = Store::new(dir.clone());
    let mut ledger = Ledger::in_memory();
    let mut t = Totals {
        off: 0,
        rucksack: 0,
        knapsack: 0,
        knapsack_if_fully_expanded: 0,
        delta_hits: 0,
        handles: 0,
    };
    for (k, (bytes, ct)) in turns.iter().enumerate() {
        let off = tokens_bytes(bytes);
        t.off += off;
        // RUCKSACK: stateless structural compression (identical compressor, no delta).
        let rk_view = structural::compress(bytes, 0, bytes.len(), *ct).0;
        t.rucksack += tokens(&rk_view);
        // KNAPSACK: conditional, against the persistent ledger.
        let r = pack(bytes, *ct, &store, &mut ledger, k as u64);
        t.knapsack += r.shown_tokens_est;
        t.delta_hits += r.delta_hits;
        // Over-expansion: read the compact view, then pull the whole region back -> paid both.
        t.knapsack_if_fully_expanded += r.shown_tokens_est + off;
    }
    t.handles = store.len();
    let _ = std::fs::remove_dir_all(dir);
    t
}

/// Percent reduction vs a base, signed (negative = grew). i64 avoids underflow.
fn pct(part: usize, base: usize) -> i64 {
    if base == 0 {
        return 0;
    }
    100 - (part as i64 * 100 / base as i64)
}

fn report(name: &str, t: &Totals) {
    println!(
        "{name:<22} OFF {:>8}   RK {:>8} ({:>3}%)   KS {:>8} ({:>3}%)   KS<RK {:>3}%   hits {:>5}  files {:>4}",
        t.off,
        t.rucksack,
        pct(t.rucksack, t.off),
        t.knapsack,
        pct(t.knapsack, t.off),
        pct(t.knapsack, t.rucksack),
        t.delta_hits,
        t.handles,
    );
}

// ---------- workload generators ----------

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
}

/// A jest-style run where one more test passes each turn; line positions stay stable.
fn test_log(passed: usize, total: usize) -> Vec<u8> {
    let mut s = String::from("> app@1.0.0 test\n> jest --runInBand\n\n");
    for i in 0..total {
        s.push_str(&format!("{} src/mod{i}.test.js ({} ms)\n", if i < passed { "PASS" } else { "FAIL" }, 8 + (i * 7) % 60));
    }
    s.push_str(&format!("\nTests: {passed} passed, {} failed, {total} total\nTime: 3.{passed} s\n", total - passed));
    s.into_bytes()
}

/// A stable ~250-line source module (re-read unchanged).
fn source_module() -> Vec<u8> {
    let mut s = String::from("// handlers.rs — request pipeline\n\n");
    for i in 0..30 {
        s.push_str(&format!("/// Handler {i}: validate, run, finalize.\n"));
        s.push_str(&format!("pub fn handler_{i}(input: &Input, opts: &Opts) -> Result<Out, Err> {{\n"));
        s.push_str("    let ctx = prepare(input, opts)?;\n");
        s.push_str("    let mut acc = 0u64;\n");
        s.push_str("    for it in ctx.items.iter().filter(|x| !x.disabled) {\n");
        s.push_str("        acc += it.weight.unwrap_or(1) * it.factor.unwrap_or(1);\n");
        s.push_str("    }\n");
        s.push_str(&format!("    finalize(normalize(acc, ctx.len()), ctx, {i})\n"));
        s.push_str("}\n\n");
    }
    s.into_bytes()
}

/// The guard must hold per-turn (pack <= stateless) AND must not disturb byte-exactness on
/// exactly the workload that triggers it (diffuse change, where it emits the stateless view).
#[test]
fn guard_holds_and_reconstruct_stays_byte_exact_on_diffuse_change() {
    let dir = store_dir("guard");
    let store = Store::new(dir.clone());
    let mut ledger = Ledger::in_memory();
    for turn in 0..8usize {
        let mut s = String::new();
        for i in 0..200 {
            let v = if i % 10 == turn % 10 { turn * 1000 + i } else { i };
            s.push_str(&format!("setting_{i} = {v}\n"));
        }
        let bytes = s.into_bytes();
        let r = pack(&bytes, ContentType::Log, &store, &mut ledger, turn as u64);
        let stateless = tokens(&structural::compress(&bytes, 0, bytes.len(), ContentType::Log).0);
        assert!(r.shown_tokens_est <= stateless, "turn {turn}: guard broken (pack {} > stateless {stateless})", r.shown_tokens_est);
        assert_eq!(
            reconstruct(&bytes, ContentType::Log, &store).as_deref(),
            Some(bytes.as_slice()),
            "turn {turn}: reconstruct must stay byte-exact even when the guard emits the stateless view"
        );
    }
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn measure_token_reduction_across_workloads() {
    println!("\n=== Token reduction: OFF vs RUCKSACK (stateless) vs KNAPSACK (conditional) ===");
    println!("(RK% / KS% = reduction vs OFF; KS<RK% = Knapsack's extra saving over Rucksack)\n");

    // ---------- FAVORABLE: the agentic loops Knapsack targets ----------
    let edit_loop: Vec<Turn> = (0..10).map(|k| (test_log(k.min(60), 60), ContentType::Log)).collect();
    let stable_reread: Vec<Turn> = (0..6).map(|_| (source_module(), ContentType::Code)).collect();
    let growing_log: Vec<Turn> = (0..8)
        .map(|turn| {
            let mut s = String::new();
            for i in 0..(50 + turn * 25) {
                s.push_str(&format!("[INFO] 2026-05-25 event {i} processed id={} status=ok\n", i * 3));
            }
            (s.into_bytes(), ContentType::Log)
        })
        .collect();
    let rotating: Vec<Turn> = (0..8)
        .map(|turn| {
            let mut s = String::new();
            for i in 0..200 {
                let v = if i % 10 == turn % 10 { turn * 1000 + i } else { i };
                s.push_str(&format!("setting_{i} = {v}\n"));
            }
            (s.into_bytes(), ContentType::Log)
        })
        .collect();

    let m_edit = measure("edit", &edit_loop);
    let m_stable = measure("stable", &stable_reread);
    let m_grow = measure("grow", &growing_log);
    let m_rot = measure("rot", &rotating);

    println!("DELTA WINS (localized change / full stability — Knapsack's target):");
    report("edit->test loop x10", &m_edit);
    report("stable file reread x6", &m_stable);

    println!("\nDELTA WEAK (diffuse change / already-elidable — the guard ties Rucksack, never worse):");
    report("growing log x8", &m_grow);
    report("rotating config x8", &m_rot);

    // Headline aggregate over the delta-friendly mix (Knapsack's actual target workload).
    let agg = Totals {
        off: m_edit.off + m_stable.off,
        rucksack: m_edit.rucksack + m_stable.rucksack,
        knapsack: m_edit.knapsack + m_stable.knapsack,
        knapsack_if_fully_expanded: 0,
        delta_hits: m_edit.delta_hits + m_stable.delta_hits,
        handles: 0,
    };
    println!("\nAGGREGATE (delta-friendly target workload):");
    report("  combined", &agg);

    // ---------- UNFLATTERING: where the delta layer can't help ----------
    let cold_single: Vec<Turn> = vec![(source_module(), ContentType::Code)];
    let mut rng = Rng(0x9E3779B97F4A7C15);
    let fully_changing: Vec<Turn> = (0..6)
        .map(|_| {
            let mut s = String::new();
            for _ in 0..100 {
                s.push_str(&format!("{:016x} req lat={}ms user={}\n", rng.next(), rng.next() % 900, rng.next() % 99999));
            }
            (s.into_bytes(), ContentType::Log)
        })
        .collect();
    let incompressible: Vec<Turn> = (0..4)
        .map(|_| {
            let blob: Vec<u8> = (0..8192).map(|_| rng.next() as u8).collect();
            (blob, ContentType::Log)
        })
        .collect();

    let m_cold = measure("cold", &cold_single);
    let m_churn = measure("churn", &fully_changing);
    let m_rand = measure("rand", &incompressible);

    println!("\nUNFLATTERING (no delta to exploit):");
    report("cold single read", &m_cold);
    report("fully-changing x6", &m_churn);
    report("incompressible x4", &m_rand);

    println!("\nOVER-EXPANSION anti-pattern (read compact view, then pull WHOLE region back each turn):");
    println!(
        "  stable reread: compact-view KS {} tok  vs  if-fully-expanded {} tok  vs  OFF {} tok",
        m_stable.knapsack, m_stable.knapsack_if_fully_expanded, m_stable.off
    );

    // ---------- HONEST INVARIANTS (so the numbers can't be gamed) ----------

    // On its target (localized change / stability) the delta layer wins big and beats stateless.
    assert!(pct(m_edit.knapsack, m_edit.off) >= 70, "edit loop should cut >=70% vs OFF, got {}%", pct(m_edit.knapsack, m_edit.off));
    assert!(m_edit.knapsack < m_edit.rucksack, "edit loop: conditional must beat stateless");
    assert!(pct(m_stable.knapsack, m_stable.off) >= 85, "stable reread should cut >=85% vs OFF");
    assert!(m_stable.knapsack < m_stable.rucksack, "stable reread: conditional must beat stateless");
    assert!(pct(agg.knapsack, agg.off) >= 75, "delta-friendly aggregate should cut >=75% vs OFF, got {}%", pct(agg.knapsack, agg.off));
    assert!(agg.knapsack < agg.rucksack, "delta-friendly aggregate: conditional must beat stateless");

    // DELTA WEAK: diffuse change can't be back-referenced block-by-block, but the
    // never-worse-than-stateless guard means Knapsack falls back to the whole-buffer
    // structural view, so it TIES Rucksack instead of losing. Still a real win vs OFF.
    assert!(pct(m_grow.knapsack, m_grow.off) >= 50, "growing log should still cut >=50% vs OFF");

    // No delta to exploit: conditional must add ZERO overhead over stateless (no spurious
    // markers when nothing is resident). These are exactly equal in practice.
    assert_eq!(m_cold.knapsack, m_cold.rucksack, "cold read: KS must equal RK (no delta, no overhead)");
    assert_eq!(m_churn.knapsack, m_churn.rucksack, "fully-changing: KS must equal RK");
    assert_eq!(m_rand.knapsack, m_rand.rucksack, "incompressible: KS must equal RK");

    // THE GUARANTEE: across EVERY workload, Knapsack is never worse than stateless Rucksack.
    // (the min(conditional, stateless) guard in pack()). This is the invariant that turns the
    // old diffuse-change loss into a tie.
    for (name, m) in [
        ("edit", &m_edit), ("stable", &m_stable), ("grow", &m_grow), ("rotating", &m_rot),
        ("cold", &m_cold), ("churn", &m_churn), ("random", &m_rand),
    ] {
        assert!(m.knapsack <= m.rucksack, "{name}: Knapsack must never be worse than stateless Rucksack (KS {} > RK {})", m.knapsack, m.rucksack);
    }

    // Over-expansion is strictly worse than not compressing — proving the scoreboard never
    // flatters and 'compact view first' is the rule, not optional.
    assert!(m_stable.knapsack_if_fully_expanded > m_stable.off, "reflexive full expansion must cost MORE than OFF");

    println!("\nInvariants hold: delta-friendly wins are real & beat stateless; Knapsack is NEVER worse");
    println!("than stateless on any workload (guard); no-delta cases add zero overhead; over-recall is a net loss.\n");
}
