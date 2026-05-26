//! `knapsack pack <file>` — the safety contract and the structural-preservation
//! contract, pinned at the library level. We exercise `pack_doc` directly (the function
//! the CLI calls) and `sidecar_path` for the default output location. The CLI's flag
//! routing (--dry-run / --force) is verified by spawning the built binary, because that
//! lives in main.rs and is the part a user actually invokes.

use knapsack::pack_doc::{pack_doc, parse_packed, sidecar_path};
use knapsack::Store;
use std::io::Write;
use std::path::PathBuf;

fn tmpdir(tag: &str) -> PathBuf {
    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    let d = std::env::temp_dir().join(format!("knapsack-packdoc-{}-{}-{}", tag, std::process::id(), t));
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn write_file(p: &std::path::Path, contents: &str) {
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::File::create(p).unwrap().write_all(contents.as_bytes()).unwrap();
}

/// Realistic "CLAUDE.md / AGENTS.md / project-notes.md"-style fixture. Two long prose
/// blocks (each > 3 lines and > 300 chars) interleaved with headings, a code fence, an
/// absolute path, and a couple of short bullets. The point: these are the SHAPES of
/// content the brief calls out as must-survive, so a regression here is visible.
fn realistic_memory_fixture() -> String {
    String::from(
        "# Project Atlas — engineering notes\n\
\n\
## Overview\n\
\n\
Atlas is the ingest pipeline. It runs on the shared cluster at /opt/atlas.\n\
\n\
The on-call rotation lives in #atlas-oncall and the deploy runbook is /runbooks/atlas.md.\n\
\n\
## Operations\n\
\n\
- Deploy: `kubectl apply -f k8s/atlas.yaml`\n\
- Roll back: `kubectl rollout undo deploy/atlas`\n\
- Tail logs: `kubectl logs -f deploy/atlas -c worker`\n\
\n\
> Decision (2026-02): all writes go through the staging table first. Direct writes to the\n\
> production table are blocked at the schema level since the incident.\n\
\n\
## Why we shard by tenant_id, not region\n\
\n\
Region sharding sounded right at first because traffic is uneven, but our hottest tenant\n\
straddles three regions and a single hot tenant in a region-shard means the whole region's\n\
queue blocks behind it. Tenant sharding gives us isolation at the level the actual incidents\n\
happen at — one tenant misbehaving doesn't cascade into neighbours, and the in-region\n\
imbalance is absorbed by the underlying autoscaler. The trade-off we accept is that\n\
cross-region failovers are slightly more expensive because state for one tenant may live in\n\
the failed-over region; the runbook for that case is in /runbooks/atlas-failover.md.\n\
\n\
```rust\n\
fn route(event: &Event) -> ShardId {\n\
    ShardId::from_tenant(event.tenant_id)\n\
}\n\
```\n\
\n\
## TODO\n\
\n\
- [ ] Repoint the staging URL\n\
- [ ] Add a metric for the queue depth per tenant\n",
    )
}

#[test]
fn writes_side_car_next_to_the_original_and_leaves_original_untouched() {
    // The two-clause invariant: a side-car appears, and the original on disk is bit-for-bit
    // identical before and after. A bug that "improves the original in place" would fail this.
    let dir = tmpdir("sidecar");
    let store_dir = dir.join("store");
    let src = dir.join("CLAUDE.md");
    let original = realistic_memory_fixture();
    write_file(&src, &original);
    let before = std::fs::read(&src).unwrap();

    let store = Store::new(store_dir);
    let r = pack_doc(src.to_string_lossy().as_ref(), &before, &store);

    let out = sidecar_path(&src);
    assert_eq!(out.file_name().unwrap().to_string_lossy(), "CLAUDE.knapsack.md");
    std::fs::write(&out, r.view.as_bytes()).unwrap();
    assert!(out.exists(), "side-car must be written next to the original");

    let after = std::fs::read(&src).unwrap();
    assert_eq!(before, after, "original file must be byte-identical to before");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn byte_exact_original_is_recoverable_from_the_store() {
    // The hard guarantee behind every elision marker. If the store ever returns
    // different bytes, the packed view is misleading at best and a stealth edit at worst.
    let dir = tmpdir("storeexact");
    let store_dir = dir.join("store");
    let store = Store::new(store_dir);

    let original = realistic_memory_fixture().into_bytes();
    let r = pack_doc("CLAUDE.md", &original, &store);

    let recalled = store.get(&r.handle).expect("handle must resolve to stored bytes");
    assert_eq!(recalled, original, "store must return byte-exact original");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn realistic_memory_file_shrinks_meaningfully() {
    // The whole point of the feature. If a paragraph-heavy memory file doesn't shrink, the
    // elision heuristic is broken. We don't pin an exact percent — the heuristic is allowed
    // to evolve — but it MUST drop tokens, and elisions MUST happen on a fixture this shaped.
    let dir = tmpdir("shrinks");
    let store = Store::new(dir.join("store"));
    let original = realistic_memory_fixture().into_bytes();
    let r = pack_doc("notes.md", &original, &store);

    assert!(r.packed_tokens < r.raw_tokens, "expected packed < raw, got {}/{}", r.packed_tokens, r.raw_tokens);
    assert!(r.elisions >= 1, "expected at least one prose block elided, got {}", r.elisions);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn code_fences_paths_commands_and_decisions_survive() {
    // The brief calls out specific shapes that MUST survive elision. We pin them as
    // substring assertions because they are the bits that, if lost, would silently
    // degrade the user's project memory.
    let dir = tmpdir("structural");
    let store = Store::new(dir.join("store"));
    let original = realistic_memory_fixture().into_bytes();
    let r = pack_doc("notes.md", &original, &store);

    let v = &r.view;
    assert!(v.contains("```rust"), "code-fence open survives:\n{}", v);
    assert!(v.contains("```\n"), "code-fence close survives:\n{}", v);
    assert!(v.contains("fn route(event: &Event)"), "code body survives verbatim:\n{}", v);
    assert!(v.contains("/opt/atlas"), "absolute path survives (lives in short paragraph):\n{}", v);
    assert!(v.contains("`kubectl apply -f k8s/atlas.yaml`"), "command in a list item survives:\n{}", v);
    assert!(v.contains("# Project Atlas"), "top-level heading survives:\n{}", v);
    assert!(v.contains("## Operations"), "subheading survives:\n{}", v);
    assert!(v.contains("Decision (2026-02)"), "blockquote-anchored decision survives:\n{}", v);
    assert!(v.contains("- [ ] Repoint the staging URL"), "checkbox list item survives:\n{}", v);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn visible_marker_is_human_readable_and_has_no_hashes() {
    // The user-facing brief: "Knapsack moet context exact opslaan en compacte views
    // tonen" and "[Knapsack: unchanged detailed section omitted. Exact text available
    // if needed.]" — NOT a wall of ks-prefixed hashes. We pin the visible portion as a
    // human sentence, separately from the hidden recall metadata.
    let dir = tmpdir("marker_visible");
    let store = Store::new(dir.join("store"));
    let original = realistic_memory_fixture().into_bytes();
    let r = pack_doc("notes.md", &original, &store);

    // Strip out every HTML comment to look at ONLY what a human reading the file sees.
    let visible = strip_html_comments(&r.view);

    assert!(visible.contains("[Knapsack: section omitted"), "human-facing marker present:\n{}", visible);
    assert!(visible.contains("exact recall available"), "tells the reader recall exists:\n{}", visible);

    // The visible portion must not leak the handle. Programs grepping for `ks_` should
    // get zero hits in the rendered view — handles only live in HTML metadata.
    assert!(
        !visible.contains(r.handle.as_str()),
        "handle {} must not appear in the rendered (HTML-comment-stripped) view:\n{}",
        r.handle,
        visible
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn hidden_metadata_carries_handle_and_line_range_for_recall() {
    // The other half of the same contract: when HTML comments ARE parsed, every marker
    // resolves back to the handle + line range needed for `knapsack expand` / MCP.
    // Without this, the friendly text would be a lie.
    let dir = tmpdir("marker_meta");
    let store = Store::new(dir.join("store"));
    let original = realistic_memory_fixture().into_bytes();
    let r = pack_doc("notes.md", &original, &store);

    let m = parse_packed(&r.view);
    assert_eq!(m.whole_file_handle.as_deref(), Some(r.handle.as_str()), "header carries the whole-file handle");
    assert_eq!(m.source.as_deref(), Some("notes.md"), "header carries the source label");
    assert!(!m.markers.is_empty(), "at least one recall marker survived round-trip");
    for marker in &m.markers {
        assert_eq!(marker.handle, r.handle, "every marker cites the whole-file handle");
        assert!(marker.line_from >= 1, "1-indexed line range");
        assert!(marker.line_to >= marker.line_from, "inclusive A-B with A <= B");
        assert!(marker.tokens > 0, "tokens count populated");
    }

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn marker_overhead_per_elision_stays_modest() {
    // Insurance against marker bloat. The visible banner + HTML metadata combined
    // should stay under ~30 tokens — otherwise we're trading raw bytes for our own
    // ceremony. If this fails because the format changed, that's a product decision to
    // make consciously; update the cap and add a CHANGELOG note.
    use knapsack::token_estimate::tokens_bytes;
    let dir = tmpdir("marker_overhead");
    let store = Store::new(dir.join("store"));
    let original = realistic_memory_fixture().into_bytes();
    let r = pack_doc("notes.md", &original, &store);

    // Each elision contributes one line that contains both the visible text and the
    // trailing HTML comment. Find them and measure.
    // Filter to actual marker lines: they have BOTH the visible banner AND the
    // ks-recall trailing comment. A header comment that *describes* the marker format
    // (e.g., mentions the banner text) must not be counted as a marker itself.
    let marker_lines: Vec<&str> = r
        .view
        .lines()
        .filter(|l| l.contains("[Knapsack: section omitted") && l.contains("ks-recall"))
        .collect();
    assert!(!marker_lines.is_empty(), "at least one marker to measure");
    // 60-token budget: visible banner (~17 tok) + trailing HTML metadata (~33 tok) +
    // slack for token-count variance. The bump from 50 → 60 was the cost of the ks2_
    // handle format (ks2_<32 hex> = 36 chars vs the legacy ks_<10 hex> = 13 chars,
    // about 7 extra tokens per marker). If this fails because the format grew further,
    // that's a product decision — update the cap and add a CHANGELOG note.
    for line in &marker_lines {
        let tok = tokens_bytes(line.as_bytes());
        assert!(
            tok <= 60,
            "marker overhead = {} tokens, exceeds the 60-token budget; line:\n{}",
            tok,
            line
        );
    }

    let _ = std::fs::remove_dir_all(&dir);
}

/// Remove every `<!-- ... -->` HTML comment from a string. The point is to show
/// exactly what a human reading the rendered markdown would see — HTML comments are
/// invisible there. We don't bother handling nested or unclosed comments because we
/// write them ourselves.
fn strip_html_comments(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(start) = rest.find("<!--") {
        out.push_str(&rest[..start]);
        match rest[start..].find("-->") {
            Some(end) => rest = &rest[start + end + 3..],
            None => {
                rest = "";
                break;
            }
        }
    }
    out.push_str(rest);
    out
}

// ---------- CLI-level flag tests (run the built binary) ----------

fn knapsack_bin() -> PathBuf {
    // The cargo-built release binary lives at target/release/knapsack(.exe). We use the
    // release binary because that's what `cargo test --release` will have just compiled.
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("target");
    p.push("release");
    if cfg!(windows) {
        p.push("knapsack.exe");
    } else {
        p.push("knapsack");
    }
    p
}

#[test]
fn dry_run_writes_nothing() {
    let dir = tmpdir("dryrun");
    let src = dir.join("CLAUDE.md");
    write_file(&src, &realistic_memory_fixture());
    let expected_sidecar = dir.join("CLAUDE.knapsack.md");

    let bin = knapsack_bin();
    if !bin.exists() {
        // Skip when the release binary hasn't been built yet — the lib-level tests above
        // still cover the contract; this one is just the CLI dressing.
        eprintln!("skipping: {} not built yet", bin.display());
        return;
    }

    let store_dir = dir.join("store");
    let output = std::process::Command::new(&bin)
        .args(["pack", src.to_str().unwrap(), "--dry-run"])
        .env("KNAPSACK_STORE", &store_dir)
        .env("KNAPSACK_METRICS", dir.join("metrics.jsonl"))
        .output()
        .expect("spawn knapsack");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "dry-run should exit 0; stdout=\n{}\nstderr=\n{}", stdout, String::from_utf8_lossy(&output.stderr));
    assert!(stdout.contains("Dry run — nothing written"), "dry-run banner present:\n{}", stdout);
    assert!(!expected_sidecar.exists(), "dry-run must not create the side-car file");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn force_writes_even_when_packed_is_not_smaller() {
    // A tiny single-line file can't be elided (heuristic needs ≥3 lines AND ≥300 chars),
    // and the comment-header overhead pushes the packed view ABOVE the original. Default
    // behavior: refuse to write. With --force: write and exit 0.
    let dir = tmpdir("force");
    let src = dir.join("tiny.md");
    write_file(&src, "Just a single short line.\n");
    let expected_sidecar = dir.join("tiny.knapsack.md");

    let bin = knapsack_bin();
    if !bin.exists() {
        eprintln!("skipping: {} not built yet", bin.display());
        return;
    }

    let store_dir = dir.join("store");
    // 1. Without --force: refuses to write, exit code != 0, side-car absent.
    let no_force = std::process::Command::new(&bin)
        .args(["pack", src.to_str().unwrap()])
        .env("KNAPSACK_STORE", &store_dir)
        .env("KNAPSACK_METRICS", dir.join("metrics.jsonl"))
        .output()
        .expect("spawn knapsack");
    assert!(!no_force.status.success(), "no-savings + no --force must NOT succeed");
    assert!(!expected_sidecar.exists(), "must not have written the side-car");

    // 2. With --force: writes anyway, exit 0, side-car present.
    let forced = std::process::Command::new(&bin)
        .args(["pack", src.to_str().unwrap(), "--force"])
        .env("KNAPSACK_STORE", &store_dir)
        .env("KNAPSACK_METRICS", dir.join("metrics.jsonl"))
        .output()
        .expect("spawn knapsack");
    assert!(forced.status.success(), "with --force, must succeed even when packed >= original");
    assert!(expected_sidecar.exists(), "side-car written under --force");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn inspect_packed_file_lists_recall_index() {
    // Power-user surface: `knapsack inspect <packed-file>` parses the side-car back into
    // a per-section index of handle + line range. This is the documented escape hatch
    // from the friendly markers — readers shouldn't have to grep HTML.
    let dir = tmpdir("inspect");
    let src = dir.join("CLAUDE.md");
    write_file(&src, &realistic_memory_fixture());

    let bin = knapsack_bin();
    if !bin.exists() {
        eprintln!("skipping: {} not built yet", bin.display());
        return;
    }

    let store_dir = dir.join("store");
    // `--force`: this test is about `inspect`, not about whether `pack` net-saves on
    // the small realistic fixture. With ks2_<32 hex> handles, fixed overhead per marker
    // grew enough that this fixture can land at a tiny negative — irrelevant here, the
    // side-car is still well-formed and inspectable.
    let pack = std::process::Command::new(&bin)
        .args(["pack", src.to_str().unwrap(), "--force"])
        .env("KNAPSACK_STORE", &store_dir)
        .env("KNAPSACK_METRICS", dir.join("metrics.jsonl"))
        .output()
        .expect("spawn knapsack pack");
    assert!(pack.status.success(), "pack should succeed under --force; stderr=\n{}", String::from_utf8_lossy(&pack.stderr));

    let sidecar = dir.join("CLAUDE.knapsack.md");
    assert!(sidecar.exists(), "side-car written");

    let inspect = std::process::Command::new(&bin)
        .args(["inspect", sidecar.to_str().unwrap()])
        .env("KNAPSACK_STORE", &store_dir)
        .env("KNAPSACK_METRICS", dir.join("metrics.jsonl"))
        .output()
        .expect("spawn knapsack inspect");
    assert!(inspect.status.success(), "inspect on a packed file should succeed; stderr=\n{}", String::from_utf8_lossy(&inspect.stderr));
    let out = String::from_utf8_lossy(&inspect.stdout);
    assert!(out.contains("Knapsack packed view"), "report header present:\n{}", out);
    assert!(out.contains("whole-file handle"), "whole-file handle line present:\n{}", out);
    assert!(out.contains("knapsack expand"), "recall command suggested:\n{}", out);
    assert!(out.contains("elisions:"), "elision count present:\n{}", out);
    assert!(out.contains("lines "), "per-marker line range present:\n{}", out);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn inspect_rejects_file_with_only_ks_recall_markers_no_header() {
    // The dogfood-surfaced bug: documentation files (CHANGELOG.md, README.md)
    // sometimes contain `<!-- ks-recall handle=... -->` substrings as documented
    // EXAMPLES of the marker format. The pre-fix check accepted such a file as
    // a packed sidecar and proudly reported "elisions: 1, lines 0-0" on a doc
    // that was never packed. The authoritative signal is the `ks-pack` HEADER,
    // not stray recall markers.
    let dir = tmpdir("inspect_no_header");
    let src = dir.join("CHANGELOG.md");
    // A realistic doc file: contains ks-recall markers as documentation but
    // has no ks-pack header (because it was never packed).
    write_file(
        &src,
        "# Changelog\n\n\
         ## [Unreleased]\n\n\
         - Added new marker shape: `<!-- ks-recall handle=ks_abc123 lines=12-30 tokens=178 -->`\n\
         - Forward-compat: extra `<!-- ks-recall handle=ks_xyz lines=1-5 future_key=42 -->`\n\
         \n\
         Plenty of other documentation that's the real point of the file.\n",
    );

    let bin = knapsack_bin();
    if !bin.exists() {
        eprintln!("skipping: {} not built yet", bin.display());
        return;
    }
    let out = std::process::Command::new(&bin)
        .args(["inspect", src.to_str().unwrap()])
        .env("KNAPSACK_STORE", dir.join("store"))
        .output()
        .expect("spawn knapsack inspect");
    assert!(
        !out.status.success(),
        "inspect must REJECT a docs file that contains ks-recall markers but no ks-pack header — got success with stdout:\n{}",
        String::from_utf8_lossy(&out.stdout)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("missing `<!-- ks-pack source=") || stderr.contains("does not look like a knapsack-packed file"),
        "stderr must name the missing header; got: {stderr}"
    );
    // And critically: the misleading success output must NOT have been printed.
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        !stdout.contains("Knapsack packed view"),
        "the docs file must NOT be reported as a packed view; got stdout: {stdout}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn inspect_accepts_file_with_only_ks_pack_header_zero_elisions() {
    // Counter-test for the fix: a genuinely-packed file that happens to have
    // ZERO elisions (everything was short / structural) MUST still be accepted
    // because the ks-pack HEADER says it's packed. The fix gates on
    // `whole_file_handle.is_some()` precisely so this case still works.
    let dir = tmpdir("inspect_header_only");
    let src = dir.join("genuine.knapsack.md");
    write_file(
        &src,
        "<!-- ks-pack source=tiny.md handle=ks2_0123456789abcdef0123456789abcdef -->\n\
         <!-- knapsack inspect <this-file>  ·  knapsack expand ks2_0123456789abcdef0123456789abcdef  (full original) -->\n\
         \n\
         # Heading\n\
         A short paragraph too small to elide.\n",
    );

    let bin = knapsack_bin();
    if !bin.exists() { return; }
    let out = std::process::Command::new(&bin)
        .args(["inspect", src.to_str().unwrap()])
        .env("KNAPSACK_STORE", dir.join("store"))
        .output()
        .expect("spawn knapsack inspect");
    assert!(out.status.success(), "header-only packed file should be accepted; stderr:\n{}", String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("Knapsack packed view"), "header should be honored; got: {stdout}");
    assert!(stdout.contains("elisions: 0"), "should report 0 elisions; got: {stdout}");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn inspect_falls_through_to_handle_mode_when_arg_is_not_a_file() {
    // Preserves the historical CLI: `knapsack inspect <handle>` still hits the store.
    // We pass a WELL-FORMED legacy handle that just happens not to be stored — that
    // proves the dispatch chose handle-mode (file-mode would say "does not look like
    // packed file", validation would say "invalid handle"). Both wrong answers are
    // ruled out by asserting on the "no such handle" path.
    let dir = tmpdir("inspect_handle");
    let bin = knapsack_bin();
    if !bin.exists() {
        eprintln!("skipping: {} not built yet", bin.display());
        return;
    }
    let out = std::process::Command::new(&bin)
        .args(["inspect", "ks_0123456789"])
        .env("KNAPSACK_STORE", dir.join("store"))
        .output()
        .expect("spawn knapsack inspect");
    assert!(!out.status.success(), "missing handle should exit non-zero");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("no such handle"),
        "must report no such handle, not 'does not look like packed file' or 'invalid handle':\nstderr={}",
        stderr
    );

    let _ = std::fs::remove_dir_all(&dir);
}
