//! THE PROOF. A realistic agentic edit->test loop over 6 iterations, scored three ways
//! over the WHOLE session:
//!   A  OFF       raw tool output, no compression            (baseline bill)
//!   B  Rucksack  stateless structural compression each call (today's best)
//!   C  Knapsack  conditional: structural + delta vs ledger  (this engine)
//! A, B, C share the same structural compressor, so the only variable is C's delta
//! layer. Fully deterministic. Run via `knapsack bench` or `cargo bench`.

use crate::content_type::ContentType;
use crate::ledger::Ledger;
use crate::pack::pack;
use crate::store::Store;
use crate::structural;
use crate::token_estimate::{tokens, tokens_bytes};

const NFUN: usize = 40;
const NITER: usize = 6;

/// A 40-handler module. The first `edited` functions carry a tweaked constant, so
/// iteration k differs from k-1 by exactly one function block.
pub fn gen_file(edited: usize) -> String {
    let mut out = vec![
        "// service.js — request handlers".to_string(),
        String::new(),
    ];
    for i in 0..NFUN {
        let bump = if i < edited { 100 } else { 0 };
        out.push(format!(
            "/** Handler {}: validate the request, run the pipeline, return a result. */",
            i
        ));
        out.push(format!("function handler{}(input, options) {{", i));
        out.push("  const ctx = prepare(input, options);".to_string());
        out.push("  const items = ctx.items || [];".to_string());
        out.push("  let acc = 0, skipped = 0;".to_string());
        out.push("  for (let j = 0; j < items.length; j++) {".to_string());
        out.push("    const it = items[j];".to_string());
        out.push("    if (!it || it.disabled) { skipped++; continue; }".to_string());
        out.push("    const w = typeof it.weight === 'number' ? it.weight : 1;".to_string());
        out.push(format!("    acc += w * {} * (it.factor || 1);", i + bump));
        out.push("    if (it.bonus) acc += it.bonus;".to_string());
        out.push("  }".to_string());
        out.push("  const score = normalize(acc, items.length - skipped);".to_string());
        out.push(format!(
            "  if (score < 0) throw new RangeError('negative score in handler {}');",
            i
        ));
        out.push(format!("  return finalize(score, ctx, {});", i));
        out.push("}".to_string());
        out.push(String::new());
    }
    out.join("\n")
}

/// A test run. Starts with 4 failing tests; one more passes each iteration. One test
/// line flips and the summary changes — line positions stay stable (realistic).
pub fn gen_log(fixed: usize) -> String {
    let failing = 4usize.saturating_sub(fixed);
    let mut out = vec![
        "> service@1.0.0 test".to_string(),
        "> jest --runInBand".to_string(),
        String::new(),
    ];
    for i in 0..NFUN {
        out.push(format!(
            "{}  src/handler{}.test.js",
            if i < failing { "FAIL" } else { "PASS" },
            i
        ));
    }
    out.push(String::new());
    out.push(format!(
        "Tests: {} passed, {} failed, {} total",
        NFUN - failing,
        failing,
        NFUN
    ));
    out.push("Time: 3.2 s".to_string());
    out.join("\n")
}

pub fn run() {
    let dir = std::env::temp_dir().join(format!("knapsack-bench-{}", std::process::id()));
    let store = Store::new(dir.clone());
    let mut ledger = Ledger::in_memory(); // ONE session memory across all iterations

    let (mut a_tot, mut b_tot, mut c_tot) = (0usize, 0usize, 0usize);
    let mut rows: Vec<(usize, usize, usize, usize, usize)> = Vec::new();

    for k in 0..NITER {
        let file = gen_file(k);
        let log = gen_log(k);

        let a = tokens(&file) + tokens(&log);

        let b_file = structural::compress(file.as_bytes(), 0, file.len(), ContentType::Code).0;
        let b_log = structural::compress(log.as_bytes(), 0, log.len(), ContentType::Log).0;
        let b = tokens(&b_file) + tokens(&b_log);

        let c_file = pack(
            file.as_bytes(),
            ContentType::Code,
            &store,
            &mut ledger,
            k as u64,
        );
        let c_log = pack(
            log.as_bytes(),
            ContentType::Log,
            &store,
            &mut ledger,
            k as u64,
        );
        let c = c_file.shown_tokens_est + c_log.shown_tokens_est;
        let unchanged = c_file.delta_hits + c_log.delta_hits;

        a_tot += a;
        b_tot += b;
        c_tot += c;
        rows.push((k + 1, a, b, c, unchanged));
    }

    let pct = |x: usize, base: usize| (x * 100).checked_div(base).map_or(0, |q| 100 - q);
    let _ = tokens_bytes(b""); // silence unused on some paths

    println!(
        "\nKnapsack A/B/C — edit->test loop, {} iterations (read file + run tests each)\n",
        NITER
    );
    println!(
        "{:<11}{:>9}{:>13}{:>13}{:>11}",
        "iteration", "A:OFF", "B:Rucksack", "C:Knapsack", "unchanged"
    );
    println!("{}", "-".repeat(57));
    for (k, a, b, c, u) in &rows {
        println!(
            "{:<11}{:>9}{:>13}{:>13}{:>11}",
            format!("#{}", k),
            a,
            b,
            c,
            format!("{} blk", u)
        );
    }
    println!("{}", "-".repeat(57));
    println!("{:<11}{:>9}{:>13}{:>13}", "TOTAL", a_tot, b_tot, c_tot);
    println!(
        "\nvs OFF      : Rucksack -{}%   Knapsack -{}%",
        pct(b_tot, a_tot),
        pct(c_tot, a_tot)
    );
    println!(
        "vs Rucksack : Knapsack saves a further -{}%  ({} tokens over the session)",
        pct(c_tot, b_tot),
        b_tot.saturating_sub(c_tot)
    );
    println!(
        "recall store: {} handles · expands needed this run: 0\n",
        store.len()
    );
    let _ = std::fs::remove_dir_all(dir);
}
