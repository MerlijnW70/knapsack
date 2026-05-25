# Knapsack (Rust) — a *conditional* token reducer for agents

The canonical Knapsack engine: a standalone `knapsack` binary **and** a reusable Rust
library, with a thin, replaceable Claude-facing integration boundary. Zero external
dependencies in the correctness-critical core.

```
Rucksack minimizes:   H(output)            ← unconditional, re-paid every call
Knapsack minimizes:   H(output | seen)     ← an unchanged+resident block costs ~1 backref
```

The dominant agentic workload is an **iterative loop over slowly-changing artifacts**
(edit → build → test → re-read → re-test). A stateless codec re-pays full price for each
near-identical re-read; a conditional one pays only for what changed.

## Quickstart

```sh
# Linux / macOS
curl -fsSL https://raw.githubusercontent.com/MerlijnW70/knapsack/main/install.sh | sh
# Windows (PowerShell)
irm https://raw.githubusercontent.com/MerlijnW70/knapsack/main/install.ps1 | iex
```

The installer downloads the binary, verifies its checksum, creates `~/.knapsack`, **backs up
and patches** your Claude Code hook + MCP config, runs a smoke test + `knapsack doctor`, and
prints the rollback command. Restart Claude Code and noisy command output starts packing.

From source: `cargo build --release` then `./target/release/knapsack install --apply`.
Rollback: `knapsack uninstall` (add `--purge` to also delete the store/metrics).
Health check: `knapsack doctor`.

### Windows note

If `cargo build --release` fails with **Access is denied** for `target\release\knapsack.exe`,
the persistent MCP server is still running the old binary and holding a lock on it. Rename it
out of the way, then build:

```powershell
Rename-Item target\release\knapsack.exe knapsack.old.exe
cargo build --release
```

After the next Claude Code restart the old process exits and `knapsack.old.exe` can be deleted.

## Result (`knapsack bench` or `cargo bench`)

A deterministic 6-iteration edit→test session, scored three ways that **share the same
structural compressor**, so the only variable is C's delta-against-seen layer:

```
iteration      A:OFF   B:Rucksack   C:Knapsack  unchanged
---------------------------------------------------------
#1              8921         2579         2579      0 blk   ← cold start: C == B exactly
#2              8923         2581          251     46 blk
...
#6              8928         2583          149     48 blk
---------------------------------------------------------
TOTAL          53549        15490         3630

vs OFF      : Rucksack -72%   Knapsack -94%
vs Rucksack : Knapsack saves a further -77%  (11860 tokens over the session)
```

`A:OFF` is byte-identical to the JS prototype (53549) — the token estimator is a 1:1 port
(UTF-16 code-unit classification matches JS `charCodeAt`), so numbers stay comparable.

## The invariant it never breaks (`cargo test`)

Strengthened from the JS prototype's "every non-blank line recoverable" to **byte-exact**:

- **The view may be lossy. The store and expand path are byte-exact.**
- `tests/store.rs` — `get(put(b)) == b` for any bytes (non-UTF-8, embedded NUL, CRLF, empty, unicode).
- `tests/faithful.rs` — the whole input reconstructs **bit-for-bit** from block handles,
  including CRLF and no-trailing-newline inputs a normalizing path would corrupt; every
  body elision expands to exactly its bytes.
- `tests/delta.rs` — a re-read after a 1-function edit references nearly every block; an
  **evicted** block is re-sent, never blindly back-referenced (residency is honored).
- `src/hash.rs` — SHA-1 pinned to NIST/RFC-3174 known-answer vectors.

Line slicing (`expand --lines A-B` / `--grep`) operates on a UTF-8 decode of the stored
bytes; non-UTF-8 content falls back to returning the full exact bytes.

## Architecture

Deterministic byte-exact core, with the integration glue isolated above `api`:

```
src/
  hash.rs            SHA-1 + ks_ handles (blake3 = later swap behind handle())
  token_estimate.rs  char-class estimator, 1:1 with JS (tokenizer-exact = later)
  content_type.rs    code vs log detection
  block.rs           tiling byte-range partitioner (delta quality lives here)
  structural.rs      unconditional layer (Rucksack-equivalent) + "· calls x,y" markers (idea ④)
  store.rs           byte-exact content-addressed recall store
  ledger.rs          session memory + Residency { Resident, Evicted, Unknown }
  pack.rs            the conditional packer (idea ①) + reconstruct()
  recall.rs          expand: exact bytes, or sliced decoded view
  metrics.rs         honest scoreboard (saved vs refetched)
  config.rs          ~/.knapsack paths (state persists across process invocations)
  api.rs             THE BOUNDARY: pack_output / expand_handle / record_residency / evict
  bench.rs           the A/B/C proof
```

## JS prototype → Rust mapping

| JS (`../knapsack`) | Rust | change |
|---|---|---|
| `lib/tokens.js` | `token_estimate.rs` + `hash.rs` | identical estimator; SHA-1 hand-rolled |
| `lib/structural.js` | `structural.rs` | now emits exact byte-range elisions |
| `lib/blocks.js` | `block.rs` | byte ranges that tile exactly (was string split) |
| `lib/ledger.js` | `ledger.rs` + `store.rs` | adds `Residency`; disk-persisted, byte-exact store |
| `lib/knapsack.js` | `pack.rs` | same algorithm; `reconstruct()` proves byte-exactness |
| `bench/loop.js` | `bench.rs` + `benches/loop.rs` | same A/B/C fixtures |
| `test/faithful.js` | `tests/*.rs` | strengthened line-level → byte-exact |
| `bin/knapsack.js` | `main.rs` | `pack/expand/delta/store/metrics/bench/install` |

## CLI

```
knapsack hook                                             # PreToolUse shim (stdin = CC event)
knapsack mcp                                              # MCP stdio server (recall tools)
knapsack pack <file|-> [--session ID] [--cmd C] [--type code|log]
knapsack expand <handle> [--lines A-B] [--grep P] [--context N]   # full = byte-exact stdout
knapsack inspect <handle>                                 # size/lines/tokens + preview
knapsack delta <old> <new>
knapsack store put <file>
knapsack metrics
knapsack ab [--knapsack PATH] [--rucksack PATH]   # head-to-head vs Rucksack
knapsack bench
knapsack install            # prints settings.json hook wiring + .mcp.json
```

## Proving the claim: `knapsack ab`

Reads both `metrics.jsonl` files and computes net the same way for each
(`net = saved − refetched`), normalizing the two schemas (`raw`/`orig`, `shown`/`comp`):

```
knapsack ab --knapsack ~/.knapsack/metrics.jsonl --rucksack ~/.rucksack/metrics.jsonl

per session (knapsack)
session                     raw      saved   refetch        net  delta  evict   exp(f)
ghi789                   52,000     48,900         0     48,900    210      4     0(0)
abc123                    6,880      6,655       120      6,535     50      0     1(0)
def456                    2,000      1,600     1,900       -300      2      1     2(1)   ← over-expanded, honestly negative

head-to-head (aggregate)
engine        compress         raw       saved   refetched         NET
rucksack             3      57,440      41,040         300      40,740
knapsack             4      60,880      57,155       2,020      55,135
winner: knapsack   (+14,395 net tokens, 35% better)
```

**Honest caveat (printed by the tool):** Rucksack's metrics carry no session id, so the
per-session table is Knapsack-only; the head-to-head aggregate is the apples-to-apples
figure. A session that over-expands shows a **negative** net — the metric never flatters.

## The full product shape

```
PreToolUse hook  →  compresses noisy command output, conditioned on the session   (savings)
MCP server       →  knapsack_expand / knapsack_inspect / knapsack_metrics          (recall)
metrics.jsonl    →  net_saved = saved − refetched, per session                     (proof)
```

### MCP stdio server (`knapsack mcp`)

JSON-RPC 2.0 over stdio (protocol `2024-11-05`), zero-dep, mirroring Rucksack's contract
(`initialize` / `tools/list` / `tools/call`). Makes recall ergonomic — the model expands a
`ks_...` handle as a tool call instead of shelling out. Register via `.mcp.json`:

```json
{ "mcpServers": { "knapsack": { "command": "<abs path>/knapsack", "args": ["mcp"] } } }
```

Tools:
- `knapsack_expand(handle, lines?, grep?, context?)` — recall the slice you need (full = byte-exact).
- `knapsack_inspect(handle)` — bytes / lines / est. tokens / utf8 + preview, to size a region before expanding.
- `knapsack_metrics(session_id?)` — the savings scoreboard, optionally per session.

The server's `initialize` instructions tell the model to prefer the compact view and expand
only a slice — because `knapsack_metrics` will show `net_saved` go negative if it over-expands.

## Live wiring — v0.1 PreToolUse hook shim

The smallest live surface, mirroring Rucksack's proven contract. For a *noisy, allowlisted*
Bash command (cargo/npm/pytest/node/grep/...), the hook rewrites it so its merged output
pipes through `knapsack pack -`, carrying the **Claude Code `session_id`** (read straight
from the hook payload — the stable session key your residency model needs). Commands with a
pipe/redirect/background `&`/`#` comment, or non-allowlisted programs, are left untouched.
The original exit code is always preserved. On any doubt the hook **fails open** (emits
nothing → command runs verbatim).

Install: `knapsack install` prints the `settings.json` snippet (a single `"command": "knapsack hook"` under `PreToolUse`/`Bash`).

Proven end-to-end (a 300-line build, run twice in one session):

```
RUN #1 (cold)         3440 -> 194 tok (-95%)   exit 3 preserved
RUN #2 (same session) 3440 ->  31 tok (-100%)  exit 3 preserved   ← whole output = 1 backref
```

Recall is live too: `expand` returns the elided region byte-exact (full / `--lines` / `--grep`),
failed handles are reported and counted. `knapsack metrics` is the honest scoreboard —
`net_saved = saved − refetched` — and it correctly goes **negative** if you expand
reflexively, which is the whole point of "compact view first, partial expand only".

### Conservative residency (until eviction is driven from the live transcript)

Residency is approximated by a token budget (`KNAPSACK_RESIDENT_BUDGET`, default 120k). When
a session's resident set exceeds it, the oldest spans are marked `Evicted`, so a back-reference
is emitted only for content still in context — an evicted block is re-sent (counted as
`evicted_backrefs_avoided`), never dangled.

## What's next

1. ✅ **PreToolUse hook shim** — done (see "Live wiring" above). Run real sessions and let
   `knapsack metrics` accumulate `session_net_saved` to A/B against Rucksack.
2. ✅ **MCP stdio server** (`mcp.rs`) — done. `knapsack_expand` / `knapsack_inspect` /
   `knapsack_metrics` over `api`, JSON-RPC 2.0, protocol 2024-11-05.
3. ✅ **A/B report** (`knapsack ab`) — done. Now the phase is *measure*: run both engines
   on real sessions and let `knapsack ab` tally `session_net_saved` over real workloads.
4. **Mechanism ② semantic content-addressing** — normalize-then-hash so whitespace-only
   changes still dedup (drop in behind `hash::handle` + a block-fingerprint step).
4. **Mechanism ③ predictive prefetch** — a policy layer ABOVE this lossless core.
5. **Live eviction from the transcript** — replace the budget heuristic with real residency.
6. **Tokenizer-exact counting** (⑨) and **tree-sitter blocks** — both behind existing
   signatures (`token_estimate::tokens`, `block::split_blocks`).
7. **Batched / single-file store** — the recall store is file-per-block. Those files are
   sharded across 256 subdirs and written in parallel, so a large output no longer
   serializes every create on one directory's lock (~2.5× faster: a 778 KB / 40k-line log
   went 4.3s → ~1.7s). It's still one tiny file per block, though; a single-file / batched
   (log-structured / embedded-KV) store behind the `store.rs` boundary could cut large-pack
   latency further. Reads stay backward-compatible across the layout change.

Deliberate limits today: residency is honored but must be driven from the live transcript
by the integration layer; block fingerprints are exact-byte (semantic match is ②); token
counts are estimated, not tokenizer-exact; and the store is still file-per-block (now
sharded + parallel), so a single-file store could cut large-pack latency further (see
"Batched / single-file store" above).
```
