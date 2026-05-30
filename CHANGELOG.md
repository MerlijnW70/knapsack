# Changelog

All notable changes to Knapsack are documented here. Format follows
[Keep a Changelog](https://keepachangelog.com/); versioning is [SemVer](https://semver.org/).

## [Unreleased]

### Added (tokenizer boundary)

- **Selectable token-counting backend behind one stable seam** (`src/tokenizer.rs`).
  Knapsack reports *measured* savings, so how it counts tokens is its credibility surface.
  Until now there was a single counter — the char-class estimator (`token_estimate`), which
  is fast/offline/zero-dep but *estimated*, not tokenizer-exact. The new `Backend` enum
  (`Estimate` / `ClaudeApi` / `GptCl100k` / `GptO200k`) is resolved from a `--tokenizer`
  flag, then the `KNAPSACK_TOKENIZER` env var, then the `Estimate` default.
  - **`knapsack tokens [<file>|-] [--tokenizer SPEC] [--model M]`** — the point-of-truth
    surface. `estimate` (default) prints `~N` (honestly inexact); exact backends print `N`.
    Loud, non-zero exit on a bad spec / unavailable backend / API failure — it never
    silently falls back to the estimate when you asked for an exact count.
  - **Engine vs reporting split is deliberate.** The compression hot path (`pack`,
    `structural`, `pack_doc`, `ledger`, `bench`) keeps calling `token_estimate` directly and
    is untouched — routing it through a network call or a multi-MB BPE would wreck latency
    and make `bench` irreproducible. Only the human-facing reporting surface resolves a
    `Backend`. The default path is byte-identical to before (pinned by
    `default_backend_matches_engine_estimator_exactly`).
  - **`claude-api`** — opt-in, exact for the model Knapsack actually reports on (Claude has
    no public offline tokenizer). Calls Anthropic's `/v1/messages/count_tokens` via `curl`
    (already an install-time dependency, so the binary stays dep-free), reads `input_tokens`,
    requires `ANTHROPIC_API_KEY`. Request-build and response-parse are pure, unit-tested
    functions; the request body goes over stdin so content never lands in process argv.
  - **`gpt-cl100k` / `gpt-o200k`** — offline GPT BPE, behind the new zero-dependency
    `exact-tokenizer` Cargo feature (default build stays tiny + zero-dep). Producing a
    *provably* exact GPT count needs the official multi-MB merge tables vendored AND a
    Unicode-correct pretokenizer; until that lands these return a typed `Unavailable`
    (with an actionable message) rather than a plausible-but-wrong number. Knapsack does
    not report a token count it cannot stand behind.

## [0.0.2] — 2026-05-28

### Fixed (release pipeline)

- **`release.yml` switched from `softprops/action-gh-release@v2` to
  `gh release create` / `gh release upload --clobber`.** v0.0.2's first
  tag-push produced an incomplete release: all four matrix jobs raced to
  *create* the Release object via `softprops/action-gh-release@v2`
  simultaneously. Linux and Windows lost with `HttpError: Validation
  Failed: {"resource":"Release","code":"already_exists","field":"tag_name"}`;
  aarch64-darwin reported job success but its assets silently never landed
  (overwritten in the race). Only `x86_64-apple-darwin` shipped. A
  `gh run rerun --failed` did NOT recover — the same `already_exists`
  error fires on every invocation when the release exists and `tag_name`
  is set, because the action issues a PATCH that GitHub's REST API
  rejects. Fix replaces the action entirely with `gh` CLI calls:
  - One `create-release` job runs first, `gh release create … || gh
    release view …` (idempotent — if the release exists, no-op).
  - Matrix `build` jobs `needs: create-release` and upload with
    `gh release upload "<tag>" <files…> --clobber`. No PATCH on the
    release object; just per-asset POSTs. No race possible.
  - Publish step split by `matrix.kind` so unix lists `.tar.gz` only and
    Windows lists `.zip` only — removes the noisy `🤔 Pattern '...zip'
    does not match any files.` warnings the previous workflow emitted.
  - Install scripts (`install.sh`, `install.ps1`) attached via the same
    `gh release upload --clobber`, gated on the matrix being green
    (`needs: build`).



### Changed (read_hook split + caller attribution)

- **`src/read_hook` decomposed into `decide.rs` + `view.rs`.** The monolithic
  module is split by concern (gate/cache decision flow vs. compact-view
  emission), shrinking each piece to one testable responsibility and the
  read-hook call-site to a thin orchestrator. (#2)

- **`expand` events now carry explicit caller attribution.**
  `api::ExpandRequest` gained a `caller: ExpandCaller` field and
  `metrics::record_expand` widened from 3 → 6 args (adds `handle`, `mode`,
  `caller`). Recall events in `metrics.jsonl` now name their originating
  surface — CLI, MCP, or the Read hook's self-heal path — so post-hoc
  analysis can attribute recall cost back to the right caller instead of
  the session that happened to be active. (#2)

### Infrastructure (Hardened Production Mode pre-convoy)

- **Repo-wide `cargo fmt` sweep + `clippy::doc_lazy_continuation` fix in
  `src/gc.rs`.** Cleared pre-existing main-debt that was blocking this
  repo's new Hardened gate (`cargo fmt --check` ∧ `cargo clippy -D warnings`
  ∧ `cargo build --release --tests` ∧ `cargo test --release` ∧ `cargo
  audit` conditional on non-zero deps). Pure formatting plus one blank
  `//!` line to break a doc-comment list continuation; zero behavioral
  drift. (#1)

### Infrastructure (Hardened gate widened to `--all-targets`)

- **`cargo clippy -- -D warnings` widened to `cargo clippy --release
  --all-targets -- -D warnings` across the Hardened gate.** PRs #1–#4
  formally passed Lock 2 on a narrower scope than the spec promised —
  the library-only default skipped `tests/`, `benches/`, and `examples/`,
  letting test-clippy debt accumulate silently. This change clears that
  debt and widens the gate. Lints cleared (tests/ unless noted):
  `clippy::ptr_arg` (×4, incl. `src/config.rs:141` in a unit-test mod),
  `expect_fun_call` (×4 — `.expect(&format!(...))` → `.unwrap_or_else(|_|
  panic!(...))`), `len_zero`, `manual_range_contains`,
  `needless_borrows_for_generic_args`, `approx_constant` (JSON-parser
  float test used `3.14`, now `4.25`), `vec_init_then_push`,
  `assertions_on_constants` (placeholder `assert!(true, …)` deleted —
  comment carries the rationale), `items_after_test_module` (allow-attr
  on `src/main.rs`'s test mod, which is deliberately co-located next to
  the `flag()` fn it tests), and a `double_ended_iterator_last` →
  `filter_next` cascade folded into `.lines().rfind(...)`. Zero
  behavioral change; pure lint hygiene.

### Fixed (installer hardening)

- **UTF-8 BOM in settings.json / .claude.json no longer breaks install.** Many
  real-world editors and shells write JSON config files with a leading byte-order
  mark — PowerShell 5.1's default `Set-Content -Encoding utf8`, Notepad's UTF-8
  save, some IDE auto-encoders. The strict in-tree JSON parser rejected the BOM
  with an opaque `unexpected Some('\u{feff}')` error, leaving the user staring
  at `✗ hook NOT patched  Left unchanged — add the entry manually.` with no
  recovery path. `install.rs::strip_bom` now normalizes BOM-prefixed UTF-8 on
  read, and patched files are re-serialized BOM-less (the modern de-facto
  standard that Claude Code already handles). Pinned by three cases in
  `tests/config_merge_adversarial.rs`: `utf8_bom_is_stripped_and_file_patches_normally`,
  `bom_only_file_is_treated_as_empty_object`,
  `bom_followed_by_invalid_json_humanizes_the_error`.

- **Backup files no longer clobber each other under fast install cycles.** The
  old `backup()` named files `<path>.knapsack-bak-<unix_secs>`. Two patches in
  the same second collapsed onto the same name — every later one overwrote
  the earlier, so a user who hit a bad config and re-ran `install` lost the
  rollback target for their ORIGINAL good config. Measured: 4 install + 4
  uninstall cycles in tight succession produced 2 backup files out of 8 expected
  (75% loss). The fix walks `_2`, `_3`, … until it finds a free name; bounded
  by 1000 attempts so a pathological filesystem can't spin forever. Pinned by
  `rapid_patch_cycles_do_not_clobber_each_others_backups` in `tests/lifecycle.rs`
  — 8 patch+unpatch cycles produce 16 distinct, all-still-on-disk backup files.

- **`knapsack uninstall` no longer leaves empty scaffolding behind.** A clean
  install→uninstall on an originally-empty config used to leave
  `{"hooks":{"PreToolUse":[]}}` (settings.json) and `{"mcpServers":{}}`
  (.claude.json) — cosmetic but visibly noisy in the user's config. New
  `prune_empty_array` / `prune_empty_object` helpers in `install.rs` drop
  empty containers ONLY after removing knapsack's own entry, leaving the
  file as close to its pre-install shape as possible. The counter-test
  `uninstall_does_not_prune_user_data` pins that an unrelated Edit hook or a
  cavewoman MCP server in the same container is preserved verbatim — pruning
  is strictly empty-only.

- **`knapsack install` now exits non-zero when any patch fails.** The old
  `apply()` returned only the human transcript; the bash hook fires the install
  the same way, and CI / post-update scripts had no signal that a half-installed
  state was their actual outcome. New `ApplyResult { output, success }` is
  consumed by `main.rs::install` dispatch and propagates exit code 1 on any
  patch failure OR any post-install doctor `Status::Fail`. `repair()` got the
  same treatment. Warnings don't bump the fail counter (a warn-only state means
  "engine healthy, not wired in" — the normal post-`uninstall` state). Pinned by
  `apply_returns_success_false_when_patch_fails` in `tests/lifecycle.rs`.

- **Install error messages are now humanized.** `unexpected Some('\u{feff}')`
  meant nothing to a normal user; `Left unchanged — add the entry manually.`
  gave no concrete next step. New `humanize_patch_error` maps the small set of
  failures we actually see in the wild (BOM that somehow survived the strip,
  permission denied on read or write, generic parse error) to one-sentence
  guidance the user can act on — "Open it in any editor, save as UTF-8 (without
  BOM), then re-run.", "Close any program that has it open…", "Common causes:
  trailing commas, // comments, or UTF-16 encoding — knapsack needs strict JSON."

- **install.ps1 hardened end-to-end.** Silenced IWR progress bar (10×+ download
  speed-up on PS 5.1). Retry-with-exponential-backoff wrapper around download
  (3 attempts, 2-4-8s) for transient Wi-Fi blips and GitHub 503s. Real
  `knapsack-installer/0.1` User-Agent (default PS UA gets rate-limited harder
  by GitHub). WOW64-aware architecture detection (a 32-bit PowerShell on
  64-bit Windows reports `PROCESSOR_ARCHITECTURE=x86`; we now also check
  `PROCESSOR_ARCHITEW6432` for the host arch). Running-binary detection: if
  `knapsack.exe` already exists, rename it out of the way before copying — the
  in-memory image stays valid until Claude Code exits, so the install never
  fails with "file in use" when Claude Code is open. PATH update is now silent
  unless `$env:KNAPSACK_VERBOSE` is set — the hook + MCP use absolute paths so
  PATH isn't required for the product to work. Failure surface is `Die`-style:
  red error sentence + yellow recovery hint. Success surface is one green line:
  `Knapsack installed. Restart Claude Code to load it.`

- **install.sh hardened end-to-end.** curl gets `--retry 3 --retry-delay 2
  --retry-connrefused` for transient failures. Real User-Agent. PATH guidance
  is silent unless `KNAPSACK_VERBOSE=1`. WSL hint when `Linux` is detected
  inside a WSL kernel (the Linux binary is the right one — but Claude Code on
  the Windows side would want install.ps1 in PowerShell). Failure surface is
  `die`-style with a recovery hint; success is one line.

### Changed

- **Read hook (input reduction) is now on by default** after `knapsack install`.
  Previously gated behind `KNAPSACK_READ_HOOK=1` and labelled EXPERIMENTAL. The
  hook keeps its safety contract — never mutates the original file, passes through
  on any uncertainty (missing/unreadable/slicing reads, files outside the 8 KB–4 MB
  band), refuses to redirect if the compressed view doesn't beat raw by ≥25%, logs
  every decision so `knapsack why-last` can explain it — and a new off-switch is
  available: set `KNAPSACK_READ_HOOK=0` (or `off`/`false`/`no`) to disable. The
  product position is now "Input + Output reduction are both active after one
  install" instead of "Input is a dogfood spike behind an env var".

- **Bare `knapsack install` now applies in one shot** (wires the hook + MCP into
  Claude Code). Previously it printed manual settings.json instructions and
  required `--apply` to actually do anything. `--apply` is still accepted as an
  explicit alias; `--print` keeps the old paste-it-yourself form for users who
  prefer to edit configs by hand.

- **`/knapsack` (status) surface redesigned for product use.** No env-var jargon,
  no EXPERIMENTAL / dogfood labels, no actions laundry list. The new shape:

  ```
  Knapsack active

  Input reduction:  active
  Output reduction: active
  Session saved:    12,840 tokens
  Net reduction:    74%
  Recall:           healthy
  Store:            381 blocks / 2.4 MB
  ```

  The technical breakdown (binary sha drift, smoke tests, MCP protocol check,
  store metadata coverage) lives under `/knapsack doctor`.

### Fixed

- **`pack -` (tool-output path) now respects the never-worse-than-raw invariant.**
  On very small or already-tight outputs (e.g. `cargo build` clean with no errors,
  ~60 bytes, ~20 tokens) the back-reference envelope was bigger than the raw bytes
  themselves, leaving `shown_tokens_est` higher than `raw_tokens_est` and saved
  going negative. The fix in `pack::pack_with_transcript`: if neither the
  conditional-delta nor the stateless view beats raw, emit the raw bytes
  themselves and zero the delta accounting. Blocks are still stored, so
  `reconstruct(...)` stays byte-exact — only what the model SEES changes. Pinned
  by `pack_never_emits_more_tokens_than_raw` in `tests/token_reduction.rs`.

- **All numeric CLI/MCP flags now reject garbage loudly** instead of silently
  using a default. Same shape audit fixed five sites: `gc --older-than`,
  `expand --context`, `expand --lines`, the `why-last N` positional, and the MCP
  `knapsack_expand` arguments `lines` and `context`. Present-but-unparseable now
  exits 2 (or returns `isError: true` for MCP) with a clear message naming the
  flag and the offending value; absent uses the documented default; valid is
  used as-is. Pinned by `tests/cli_numeric_flags.rs` (12 tests) and three new
  MCP cases in `tests/mcp_protocol.rs`.

- **Invalid-handle error messages are bounded** in length. A megabyte-sized
  hostile handle (CLI typo or MCP client bug) no longer produces a megabyte-sized
  echo in stderr / JSON-RPC reply. New `hash::display_handle()` caps the echo at
  64 chars and annotates the total length. Applied at all four echo sites (CLI
  `expand` / `inspect`, MCP `knapsack_expand` / `knapsack_inspect`, `gc
  --older-than`). Pinned by `hash::tests::display_handle_truncates_oversized_input`.

- **`tests/read_hook.rs` race condition resolved.** Multiple tests set the same
  process-global env vars (`KNAPSACK_READ_CACHE` / `KNAPSACK_READ_LOG`) and ran
  in parallel under cargo's default test runner, so siblings clobbered each
  other's cache dir mid-call. Failures showed up as cross-test path comparisons
  (e.g. a `cachehit` test asserting equality between paths from a `gate` test
  and a `worse` test). Fix: an `EnvGuard` RAII type that locks a static Mutex
  for the duration of the env override and restores the prior values on drop.
  Zero new dependencies (the project's no-dep policy still holds). All 12
  read-hook tests now pass in the default parallel mode, 5 runs in a row.

- **Unit test `decide_passes_through_when_gate_disabled`** used `decide(&evt)`
  which reads the gate from the process env. With `KNAPSACK_READ_HOOK=1` set in
  the shell (and now with the default-on flip), it would assert the wrong reason.
  Switched to `decide_with_gate(false, &evt)` to make the test deterministic
  regardless of the runtime environment.

- **Code block splitting now boundary-aware** instead of blank-line-only. Blocks
  open at column-0 top-level **definitions** (`fn`/`pub fn`/`async fn`,
  `function`/`async function`/`export …function`, `class`/`export class`,
  `interface`/`type`/`enum`, `struct`/`trait`/`impl`/`mod`, Python `def`/`async
  def`/`class`, `macro_rules!`, …). A block runs from one definition to the
  next, so:
  - A function with INTERNAL blank lines (paragraph breaks inside its body) is
    now one block. The old splitter fragmented these into 3+ tiny blocks, which
    made one-line edits invalidate too many blocks.
  - Multiple top-level functions become **separate** blocks — edits stay
    local. Pinned by `one_function_edit_only_invalidates_that_block`.
  - Python classes hold their indented methods (column-4+ `def`s are NOT
    boundaries — only column-0 ones).
  - Preamble (doc comments, imports, attributes before the first definition)
    is its own block.
  - **Fallback preserved**: code with no recognisable definitions (minified
    bundles, single-statement scripts) routes through the historical
    blank-line splitter — never worse than before. Pinned by
    `minified_code_with_no_definitions_falls_back_to_blank_line_split`.
  - Detection is zero-dep keyword matching at column 0; a tree-sitter pass
    behind a feature flag is the obvious next step but not needed for this
    patch.
  - Existing bench (`knapsack bench`) is essentially flat
    (4,385 → 4,377 tokens) — the synthetic fixture already had clean
    function boundaries. The improvement shows up on real codebases with
    internal blank lines, where the old splitter created many tiny blocks
    that fragmented the delta cache.

### Added

- **JSON-aware compression.** New `ContentType::Json` recognises `.json` files,
  `package.json` / `tsconfig.json` / `package-lock.json` / `composer.json` /
  `jsconfig.json` / `tsconfig.base.json` by name, and any input ≤256 KB whose first
  non-whitespace byte is `{` or `[` AND parses as JSON. Malformed JSON falls back
  to the Log heuristic — never claims a malformed file is JSON.
  - **Quote/escape/depth-aware tile splitter** (`block::split_json`): splits at
    top-level member boundaries with a single pass. Each `"key": …,` (or array
    element) becomes one tile; framing braces/brackets become their own tiny tiles.
    Tiles cover [0, len) byte-exact so `reconstruct(...) == bytes` keeps holding.
    Any malformation (unterminated string, missing close, root scalar) safely
    returns a single tile.
  - **Cold-pass compressor** (`structural::compress_json`): each top-level member
    larger than 240 bytes is replaced by `"<key>": [Knapsack: section omitted ·
    ~N tokens · recall ks2_…]`, with the **key name preserved** so the lossy view
    stays scannable. Each elision's exact bytes go in the byte-exact store under
    its own handle — `knapsack expand <handle>` returns the original member.
  - **Delta on key paths**: a one-field edit (e.g. `"version": "0.0.1"` →
    `"0.0.2"`) invalidates only the tile carrying that key. Other top-level keys
    keep their bytes → back-reference. Measured: 9/10 blocks back-ref on a real
    package.json version bump.
  - **Never-worse-than-raw guard** unchanged — applies to JSON the same way.
  - **Secrets contract**: no new logging sinks for JSON content. The byte-exact
    store still receives bytes (same as every other pack); `metrics.jsonl` and
    `read_hook.jsonl` see counts, not content. Pinned by
    `secrets_in_json_are_not_logged_anywhere_new`.

### Added (experimental)

- **Read hook spike** — `KNAPSACK_READ_HOOK=1` gated, default OFF. When enabled,
  PreToolUse Read events are inspected and (for files in the 8 KB – 4 MB band that
  compress meaningfully) `file_path` is rewritten at a cached compressed view in
  `~/.knapsack/read_cache/<sha256>.md`. The original file is never touched; Claude
  can still read it directly or recall via `knapsack expand <handle>`. Source:
  `src/read_hook.rs`. **Do not enable in production until live acceptance is
  green** — see the dogfood guide in README.
- **Structured pass-through logging** (`src/why_log.rs`) — every Read decision
  appends one JSONL line to `~/.knapsack/read_hook.jsonl` with a stable reason
  code: `gate-disabled`, `bad-input`, `slicing-requested`, `file-unreadable`,
  `too-small`, `too-large`, `file-changed`, `worse-than-raw`, `redirect-emitted`,
  `cache-hit`. Reserved-but-not-yet-wired: `not-resident`, `no-transcript-proof`,
  `updated-input-rejected`. Forward-compat: unknown reasons in the log are
  silently skipped by the reader.
- **`knapsack why-last [N]`** debug command — prints the last N Read-hook
  decisions (default 10). Each line: reason · bytes · path · token before→after
  · note. The dogfood feedback channel.
- **`knapsack gc` cleans the read cache** — read-cache files share the same
  age-based cleanup as the store, tallied in `read_cache_scanned` /
  `read_cache_deleted` in the report so you can see the experimental cache's
  contribution.
- **`knapsack status` reflects the gate** — shows `✓ read hook (EXPERIMENTAL)`
  when `KNAPSACK_READ_HOOK=1` is set in the current shell, otherwise the
  default off-state line with the env-var hint.

### Added

- **Transcript-driven residency.** The hook now passes Claude Code's
  `transcript_path` through to `pack`, and `pack_with_transcript` intersects the
  ledger's notion of "resident" with the set of handles the transcript proves
  are still in the context window AFTER the most recent boundary. Closes the
  dangling-backref bug where the ledger thought content was still in context
  but `/clear` (or compaction) had wiped it. Source: `src/transcript.rs`,
  `src/pack.rs::pack_with_transcript`, hook plumbing in `src/hook.rs`.
  - **Boundaries detected**: a `/clear` user command (raw-text and structured
    forms), compaction events (`type` = `compact`/`compaction`/`compacted`/
    `compact_complete`), session restart (`type` = `session_start`/
    `session_restart`/`session_reset`/`restart`). All case-insensitive on the
    label; raw-text fallback for shapes we don't yet parse.
  - **Safe fallback**: missing or unreadable `transcript_path` → `ok: false`
    → caller drops the gate → ledger-only behaviour, identical to before
    this change. Corrupt JSONL lines are skipped per-line, not fatally.
  - **Wins where ledger lied**: when the transcript proves a handle is gone,
    the engine treats the block as evicted — emits a fresh structural view
    instead of an "already in context" backref. The never-worse-than-stateless
    guard still applies, so fragmented partial-residency runs route through
    the stateless compressor when that's smaller.
  - **`knapsack transcript <path>`** debug subcommand: prints status
    (enabled/disabled), lines scanned, last boundary detected (`/clear`,
    compaction, session restart) with line number, and up to 5 sample
    resident handles. Use this to answer "why did Knapsack treat handle X
    as not-resident?".
- **Per-block metadata sidecars for `ks2_` writes.** Each new block now has a
  companion `<handle>.meta` JSON sidecar in the same shard directory carrying:
  full 64-hex SHA-256 (the safety belt behind the 128-bit handle prefix), exact
  byte length, `created_at`, `last_accessed`, and optional `ct` / `source` /
  `session` / `project` fields. The block file IS still the bytes — sidecar is
  pure metadata, never load-bearing for content. Source: `src/meta.rs`.
  - **Verify-on-read is now three-layer.** `Store::get` first checks the meta
    sidecar (length THEN full SHA-256) when present; falls back to the existing
    `hash::verify` (truncated-prefix) when meta is missing; either way a
    mismatch reads as None. The byte-exact-or-None invariant is preserved across
    the upgrade — legacy stores keep working with no behavior change.
  - **`last_accessed` is touched on successful reads, debounced to 60 s** so a
    hot block doesn't pay a write per `get`. Touch failures (read-only mounts,
    etc.) are silently swallowed — meta is a hint, never on the critical path.
- **`knapsack gc [--older-than DAYS] [--dry-run]`** — drop cold blocks from the
  store. Uses `meta.last_accessed` when available, falls back to filesystem
  mtime for legacy blocks. Always deletes block + sidecar as a pair via
  `Store::delete` — never leaves half a pair behind. `--dry-run` reports what
  would happen without touching the filesystem. Default threshold: 30 days.
- **`doctor` gains an informational `store metadata` line** showing
  `<with_meta>/<total>` block coverage. Never fail/warn — it's a roadmap signal
  that grows as legacy stores get exercised and re-packed.

### Changed

- **Handle format is now `ks2_<32 hex>` (128-bit truncated SHA-256).** Legacy
  `ks_<10 hex>` (40-bit SHA-1) and `ks_<16 hex>` (64-bit SHA-1) are still **read**
  — old stores keep working, recall is byte-exact for both formats — but every
  new write produces `ks2_`. The `2` in the prefix is a format-version tag so a
  future bump (blake3, longer truncation) can ship as `ks3_` additively.
  - **Store verify-on-read routes by handle format** (`hash::verify`): legacy
    handles re-hash with SHA-1, new handles with SHA-256. The "byte-exact-or-None"
    invariant survives the format bump — a corrupted file still reads as None
    under either algorithm.
  - **Store sharding is now prefix-aware**: shard chars are the first two hex of
    the hash regardless of prefix (`s.find('_')`-based), so `ks2_<hex>` and
    `ks_<hex>` shard identically and the existing 256-bucket distribution holds.
  - **Strict handle validation** (`hash::is_valid_handle`) at every public
    boundary — `knapsack expand`, `knapsack inspect`, MCP `knapsack_expand`,
    MCP `knapsack_inspect`. Malformed input gets a clear "invalid handle" reject
    instead of a misleading "no such handle".
  - **MCP tool descriptions and instructions** now show the `ks2_` example
    explicitly and note that legacy `ks_…` handles still resolve.
  - **Per-marker overhead grew by ~7 tokens** because the inline handle is now
    36 chars instead of 13. The pack_doc marker-overhead test budget moved from
    50 → 60 tokens to reflect this; benchmark net savings dropped from −94% to
    −92% vs raw (12k vs 14k tokens over the A/B/C session, depending on the
    fixture) — a defensible price for cryptographic safety margin and a
    versioned format.
  - **No behaviour change anywhere else.** Pack, recall, structural compression,
    the conditional layer, MCP wire format — all unchanged outside handle
    rendering and verification.

### Added

- **Tool-output markers unified with the `pack_doc` style.** The hook's compact view
  now uses the same `[Knapsack: …]` brackets as side-cars, with consistent vocabulary
  (`recall` everywhere, never `expand` in user-facing text). Old `⟨knapsack: 3 block(s) /
  42 lines unchanged — already in context · recall ks_abc⟩` becomes the much shorter
  `[Knapsack: 42 lines unchanged · recall ks_abc]`. Body and log-middle markers shifted
  the same way. The handle stays visible (unlike in `pack_doc`'s human-readable files):
  the hook's output IS fed back to Claude as tool output, and Claude needs the handle to
  call `knapsack_expand`. Source: `src/pack.rs::flush_ref`, `src/structural.rs`.
- **Anchors for compiler / test-runner output.** `important()` in the log compressor now
  recognises framings that previously slipped through the `error/warn/fail/panic` net:
  Python tracebacks + `File "x.py", line N` frames, Node stack frames (`    at fn
  (file:line:col)`), `Caused by:` (JVM), Gradle `* What went wrong:` / `* Try:`, `npm
  ERR!` (note the `!`), TypeScript `error TS` / `warning TS`, and Rust `error[E…]`
  diagnostic codes. Each family is pinned by a fixture in `tests/log_anchors.rs` that
  buries the anchor stanza in the middle of a long log and asserts it survives elision.
- **Real (subset) regex for `knapsack_expand(grep: …)`.** New zero-dep `src/regex.rs`
  supports literals, `.`, greedy `* + ?`, line anchors `^` and `$`, character classes
  including ranges and negation, and shorthand classes `\d \w \s` plus negations. Plain
  words still behave like substring search (no metachars, no change). Patterns that use
  unsupported metacharacters (`|`, `()`, `{n,m}`) fall back transparently to the old
  case-insensitive substring path, so existing callers keep working. MCP `grep`
  description updated to declare the subset.
- **Human-readable packed views.** The elision marker is now a short, jargon-free banner
  with the recall metadata moved into a trailing HTML comment that's invisible in
  rendered markdown:

      [Knapsack: section omitted · ~178 tokens · exact recall available] <!-- ks-recall handle=ks_… lines=A-B tokens=178 -->

  No `ks_…` hashes leak into what a reader sees. Pinned by tests
  (`visible_marker_is_human_readable_and_has_no_hashes`, `marker_overhead_per_elision_stays_modest`).
- **`knapsack inspect <packed-file>`** — power-user view of a `.knapsack.md` side-car.
  Parses the embedded manifest + every recall marker and prints a per-section index with
  the exact `knapsack expand <handle> --lines A-B` invocation for each. The historic
  `knapsack inspect <handle>` form still works; dispatch is by whether the arg is an
  existing file. Source: `src/pack_doc.rs::parse_packed`, `src/main.rs::run_inspect_doc`.
- **`knapsack pack <file>`** — explicit, opt-in **context-file** packing. Reads a
  markdown/text document, stores the byte-exact original in the recall store, and writes
  a markdown-aware compact view to a side-car `<name>.knapsack.md`. Preserves headings,
  code fences, blockquotes, lists, and short paragraphs; replaces only long prose blocks
  (single line ≥500 chars OR ≥3 lines AND ≥300 chars) with the markers described above.
  - Safety contract: never mutates the original file; refuses to write when the packed
    view is not smaller (override with `--force`); `--dry-run` writes nothing and just
    reports what would happen; `--output <path>` overrides the default side-car location.
  - Stdin form is unchanged: `knapsack pack -` is still the hook's pipeline pack
    (`pack.rs` / `api::pack_output`). The two paths share only the byte-exact store.
  - Source: `src/pack_doc.rs`; CLI wiring: `src/main.rs` (`run_pack_doc`).
- **`knapsack status`** (and the `/knapsack` Claude Code slash command) — a compact,
  product-facing summary that answers four user questions at a glance: is Knapsack active,
  what did this session save, is recall healthy, and what does the store cost? It's the
  default when `knapsack` is run with no arguments, and is intentionally short and
  non-technical — `knapsack doctor` keeps the long-form diagnostic. Surfaces recall
  failures as a warning (with a pointer to `doctor`), shows net savings (not gross), and
  emits a lifetime footer only when there's more than one session of history (so a single
  active session isn't double-billed). Source: `src/status.rs`; Claude Code wiring:
  `.claude/commands/knapsack.md`.
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
