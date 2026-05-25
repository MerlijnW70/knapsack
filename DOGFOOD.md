# Knapsack v0.0.1 — Dogfood preflight & test matrix

Controlled private-beta dogfooding. Goal: **prove the v0.0.1 claim on a real machine**, then
let metrics drive hardening — not add features. Run the phases in order; stop and note any
failure. Windows commands shown; drop `.exe` / flip slashes on Linux/macOS.

> ⚠️ `install --apply` mutates your real Claude Code config (`~/.claude/settings.json`,
> `~/.claude.json`). It backs up first and is fully reversible via `uninstall`. Run it only
> when you're ready. After install/uninstall, **restart Claude Code** so it (re)loads the
> hook + MCP server.

What we're proving: install/uninstall is safe · hook + MCP work live · `net_saved` stays
positive · recall is usable · no config damage · no correctness damage.

---

## Phase 0 — Lifecycle (before any real session)

Prove install / uninstall / backups / idempotence / rollback on the real binary.

```powershell
.\target\release\knapsack.exe install --apply     # expect: ✓ hook + ✓ MCP patched (+backup), Healthy ✓
.\target\release\knapsack.exe doctor              # expect: Healthy ✓ (all 8 checks)
.\target\release\knapsack.exe install --apply     # expect: "• already set" (idempotent, no new backup)
.\target\release\knapsack.exe uninstall           # expect: ✓ hook + ✓ MCP removed, store kept
.\target\release\knapsack.exe doctor              # expect: hook/MCP now • (warn) -> "not wired in yet"
.\target\release\knapsack.exe install --apply     # re-install cleanly
.\target\release\knapsack.exe doctor              # expect: Healthy ✓ again
```

Verify: backups created on first patch; unrelated config (model, other hooks, other MCP
servers) preserved; doctor state flips correctly between wired/unwired. **Restart Claude Code.**

## Phase 1 — Tiny controlled session (not a full day)

Prove the end-to-end loop, then stop and read the numbers.

1. Run a test command (e.g. `cargo test` or `npm test`).
2. Run the **exact same** command again.
3. Inspect the compact output of run #2 — should be ~one back-reference marker.
4. Expand one handle — MCP `knapsack_expand(handle, grep?, lines?, context?)`, or CLI
   `.\target\release\knapsack.exe expand <ks_...> --grep <pat> --context 2`.
5. `.\target\release\knapsack.exe metrics` — sanity-check saved/refetched/delta_hits.
6. `.\target\release\knapsack.exe ab` — first head-to-head datapoint.

## Phase 2 — Representative workloads

| Workload | Why | Verify |
|---|---|---|
| exact same command twice | delta/backref should win maximally | run #2 ≈ −100%, `delta_hits` jumps |
| test output, one changed failure | delta should stay local | only the changed chunk re-sent |
| large stack trace | recall must be usable | failure visible **without** expanding; expand = exact |
| rg / grep output | allowlist + line recall | packed to samples; `expand --grep` returns the rest |
| command exit code ≠ 0 | exit-code preservation | Claude sees the original nonzero code |
| command with pipe / redirect | must stay passthrough | output is raw/unpacked (no `[knapsack …]` footer) |
| binary / non-UTF-8 output | byte-exact store must not break | no corruption; full expand returns exact bytes |
| very long session | eviction kicks in | `evicted_backrefs_avoided` > 0 over time |
| deliberate over-expansion | honest accounting | `net_saved` drops (can go negative) |

## Phase 3 — Compare against Rucksack

```powershell
.\target\release\knapsack.exe ab
```

| Metric | Interpretation |
|---|---|
| `net_saved` high positive | Knapsack is working |
| `refetched` high | improve previews / `inspect` / recall UX |
| `delta_hits` high | session ledger is paying off |
| `evicted_backrefs_avoided` high | residency budget is protecting correctness |
| negative sessions | harden those workloads first |
| little difference vs Rucksack | allowlist may miss commands, or workload has little overlap |

Target: *"Knapsack v0.0.1 achieved X net_saved over Y real sessions, Z% better than Rucksack."*

## Phase 4 — Harden (only on failures / metrics)

Pick from these **only when a Phase 0–3 result proves it's needed** — never speculatively:
TOML config · transcript-driven eviction · allowlist tuning · better previews ·
tokenizer-exact accounting · prefetch · semantic dedup.

## Rollback (any time)

```powershell
.\target\release\knapsack.exe uninstall            # remove hook + MCP, keep store/metrics
.\target\release\knapsack.exe uninstall --purge    # also delete store + metrics
```

Removes only Knapsack's entries (backups listed); unrelated config preserved. Restart Claude Code.

---

*Do not implement TOML config, transcript-driven eviction, tokenizer-exact accounting,
prefetch, semantic dedup, or allowlist tuning until a test or metric directly proves it.*
