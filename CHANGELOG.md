# Changelog

All notable changes to Knapsack are documented here. Format follows
[Keep a Changelog](https://keepachangelog.com/); versioning is [SemVer](https://semver.org/).

## [Unreleased]

### Added

- **`knapsack install --repair`** — force-converges the Claude Code hook **and** the MCP server
  onto the canonical installed binary (`current_exe`), backing up each config first, printing the
  canonical SHA-256, emitting safe User-PATH guidance, and ending with a full `doctor` run.
- **Binary-provenance checks in `knapsack doctor`**: reports the *configured* hook binary, the
  *configured* MCP binary, and a *configured binary drift* check (SHA-256) that fails when the
  hook and MCP point at different builds. Labels say "configured" so the report is about
  on-disk/config targets, never mistaken for what the session's already-running processes loaded.
- **Hand-rolled, zero-dependency SHA-256** (`sha256.rs`), pinned to FIPS 180-4 vectors, backing
  the provenance fingerprints.
- **Safe, idempotent User-PATH setup on Windows** in `install.ps1`.

### Changed

- **`knapsack ab` is now a knapsack-only savings report** (per-session + aggregate + verdict).
  The Rucksack head-to-head comparison and the `--rucksack` flag were removed.

### Fixed

- **Stale hooks are repointed, not ignored.** Installer hook detection now converges a knapsack
  hook to the exact canonical command — an absolute path to an old build is rewritten in place
  instead of being treated as "already installed" and left alone. An already-canonical hook
  stays a no-op. This was the root cause of a hook/MCP/PATH binary split that still produced
  healthy-looking metrics.
- **No more unsafe PATH guidance.** `install.ps1` no longer suggests `setx PATH "$dest;%PATH%"`,
  which truncates at 1024 chars and folds the combined machine+user PATH into the user scope.

## [0.0.1] — 2026-05-25

Initial release: the full product shape — a *conditional* token reducer for agents —
engine + Claude Code hook + MCP server + metrics + installer, hardened by a broad test sweep.

### Engine (deterministic, zero-dependency core)

- **Conditional compression** `H(output | seen)`: tool output is packed relative to what the
  session has already shown, not in isolation. On iterative loops (edit→test, re-reads) a
  re-read collapses to back-references — you pay only for what changed. (`pack.rs`, delta over
  tiling byte-range blocks.)
- **Never-worse-than-stateless guard**: `pack()` also compresses the whole buffer in isolation
  and emits the smaller of the two, so diffuse change falls back to (never loses to) stateless
  structural compression. Measured: ~88% reduction vs raw and ~75% beyond stateless on the
  delta-friendly target; a tie on diffuse change; zero overhead when nothing is resident.
- **Byte-exact content-addressed store** with verify-on-read: `get(put(b)) == b` for any bytes;
  a corrupt/torn file is rejected (re-sent) rather than served as wrong bytes. Blocks are
  sharded across 256 subdirs and written in parallel (large-output packing ~2.5× faster), with
  a backward-compatible read fallback to the legacy flat layout. (`store.rs`.)
- **Session ledger** with `Residency { Resident, Evicted, Unknown }` and conservative
  token-budget eviction, so a back-reference never dangles past the context window. (`ledger.rs`.)
- **Structural compressor** (code/log) with deterministic `· calls …` markers and byte-exact
  elisions. (`structural.rs`.)
- **Char-class token estimator** ported 1:1 from Rucksack (UTF-16 units → 0% drift). (`token_estimate.rs`.)
- **Hand-rolled SHA-1** pinned to NIST/RFC-3174 vectors; `ks_` handles. (`hash.rs`.)
- **Tiny in-tree JSON** parser/serializer with a recursion-depth cap (no stack overflow on
  hostile input) for the integration glue. (`json.rs`.)

### Claude Code integration

- **PreToolUse hook shim** (`knapsack hook`): rewrites noisy allowlisted Bash commands to pipe
  output through `knapsack pack -`, carries the CC `session_id`, leaves shell-meta commands
  alone, fails open, and preserves the original exit code. (`hook.rs`.)
- **MCP stdio server** (`knapsack mcp`, JSON-RPC 2.0, protocol 2024-11-05) exposing
  `knapsack_expand(handle, lines?, grep?, context?)`, `knapsack_inspect(handle)`,
  `knapsack_metrics(session_id?)`. (`mcp.rs`.)
- **Thin, stable API boundary**: `pack_output` / `expand_handle` / `record_residency` / `evict`. (`api.rs`.)

### Metrics & proof

- **JSONL metrics** (`~/.knapsack/metrics.jsonl`), written with atomic single-`write_all`
  appends (no line loss under concurrency); `net = saved − refetched` goes negative on
  over-expansion — the scoreboard never flatters. (`metrics.rs`.)
- **`knapsack ab`** — head-to-head vs Rucksack across both metrics schemas.
- **A/B/C benchmark** (`knapsack bench`): OFF vs Rucksack-style vs Knapsack over an edit→test loop.

### Packaging & lifecycle

- **`knapsack install --apply`** — merges (never clobbers) the hook into `settings.json` and the
  MCP server into `~/.claude.json`, writes timestamped backups, is idempotent, runs a smoke
  test + doctor, and prints the rollback command. Refuses to patch an unparseable *or*
  non-object config.
- **`knapsack doctor`** — 8 checks; **`knapsack uninstall [--purge]`** — removes only Knapsack's entries.
- One-line installers `install.sh` / `install.ps1`; `.github/workflows/release.yml` builds
  cross-platform binaries (linux/macos x64+arm64, windows) with per-asset SHA-256 on tag `v*`.
- Config paths env-overridable: `KNAPSACK_STORE`, `KNAPSACK_SESSIONS`, `KNAPSACK_METRICS`,
  `KNAPSACK_SETTINGS`, `KNAPSACK_MCP_CONFIG`, `KNAPSACK_RESIDENT_BUDGET`.

### Invariants & tests

- The compact view may be lossy; the store and expand path are **byte-exact**. Covered by
  fuzz (random + adversarial inputs to 1 MB), exact `split_blocks` tiling, concurrency
  (multi-process/-thread), corruption, and content-type-mismatch property tests.
- Zero external runtime dependencies in the core; `cargo clippy --all-targets -- -D warnings`
  is clean. ~95 tests across the suite.

### Known limits

- The delta win needs *localized* change to beat stateless compression (it ties, never loses,
  otherwise). The store is file-per-block (sharded + parallel); a single-file/batched store is
  future work. Token counts are estimated, not tokenizer-exact. Residency is a token-budget
  approximation until driven from the live transcript.
