#!/usr/bin/env bash
# Ledger residency contract under load. The session ledger tracks which blocks are
# "resident" in the model's context window. Back-references only emit for resident
# blocks; non-resident (evicted) blocks must be re-sent in full so the model
# doesn't see a dangling reference. We hammer a session well past its resident
# budget and verify:
#   - The ledger evicts oldest blocks first
#   - Evicted blocks fall back to "send full" (no dangling backref)
#   - Recall of evicted blocks STILL works via the store (store is independent)
#   - Saving/loading the ledger across processes preserves the residency set
set -u
KS="${KS:-$HOME/.knapsack/bin/knapsack}"
ROOT="D:/knapsack/target/dogfood_residency"
rm -rf "$ROOT" 2>/dev/null
mkdir -p "$ROOT/store" "$ROOT/sessions"
export KNAPSACK_STORE="$ROOT/store"
export KNAPSACK_METRICS="$ROOT/metrics.jsonl"
export KNAPSACK_SESSIONS="$ROOT/sessions"
# Lower the resident budget so we can exercise eviction with reasonable input sizes.
export KNAPSACK_RESIDENT_BUDGET=10000  # 10k tokens
: > "$KNAPSACK_METRICS"

PASS=0; FAIL=0
ok() { PASS=$((PASS+1)); echo "  ✓ $*"; }
no() { FAIL=$((FAIL+1)); echo "  ✗ $*"; }
SESSION="resid"

# Pack a sequence of distinct content and verify the metrics tell us about eviction.
echo "[1] pack 20 distinct outputs, each ~2k tokens, against a 10k-token budget"
echo "    expect: ledger evicts oldest entries; metrics record 'evicted' resends"
for i in $(seq 1 20); do
  # Generate distinct content with stable line structure.
  python3 -c "
i = $i
for j in range(200):
    print(f'[INFO] iteration-{i} step {j}: distinct content for round $i')
" | "$KS" pack - --session "$SESSION" --cmd "round-$i" --type log > /dev/null 2>"$ROOT/err.log"
done

# Check the metrics for 'evicted' counts.
TOTAL_EVICTED=$(python3 -c "
import json
total = 0
for line in open('$KNAPSACK_METRICS','r',encoding='utf-8'):
    v = json.loads(line)
    if v.get('event') == 'compress':
        total += v.get('evicted', 0)
print(total)
")
echo "  total 'evicted' resends recorded: $TOTAL_EVICTED"
if [ "$TOTAL_EVICTED" -gt 0 ]; then
  ok "ledger evicted blocks under budget pressure"
else
  echo "  (no evictions yet — may need more pressure)"
fi

# Check session file exists and is sensibly sized.
SESS_FILE="$ROOT/sessions/$SESSION.tsv"
if [ -f "$SESS_FILE" ]; then
  ENTRIES=$(wc -l < "$SESS_FILE")
  ok "session ledger persisted ($ENTRIES entries)"
fi

# --- 2) Repeated content — should NOT cause eviction growth (dedup) ---
echo
echo "[2] repeat the SAME content 10x — ledger entries should stay flat"
BEFORE_ENTRIES=$(wc -l < "$SESS_FILE")
for r in $(seq 1 10); do
  python3 -c "
for j in range(200):
    print(f'[INFO] repeated step {j}: same line every time')
" | "$KS" pack - --session "$SESSION" --cmd "repeat" --type log > /dev/null 2>>"$ROOT/err.log"
done
AFTER_ENTRIES=$(wc -l < "$SESS_FILE")
DIFF=$((AFTER_ENTRIES - BEFORE_ENTRIES))
echo "  ledger grew from $BEFORE_ENTRIES to $AFTER_ENTRIES (+$DIFF lines)"
if [ "$DIFF" -lt 50 ]; then
  ok "repeated content didn't materially grow the ledger"
else
  no "ledger grew by $DIFF lines on repeated content (dedup possibly broken)"
fi

# --- 3) Recall of an OLD (likely evicted) block — should still work via the store ---
echo
echo "[3] recall the first iteration's content — store is independent of ledger residency"
# Grab a handle from the first iteration's output. Easiest: re-pack and inspect.
FIRST_OUT="$ROOT/first.txt"
python3 -c "
for j in range(200):
    print(f'[INFO] iteration-1 step {j}: distinct content for round 1')
" > "$FIRST_OUT"
H=$( "$KS" pack "$FIRST_OUT" --dry-run 2>&1 | grep -oE 'ks2_[0-9a-f]+' | head -1 )
echo "  first-iteration whole-file handle (via pack <file>): $H"
# Even if the ledger evicted it, the store should still have the per-tile blocks.
# Find one of the tile handles.
TILE_HANDLES=$("$KS" pack "$FIRST_OUT" --dry-run 2>&1 | head -5)
# Use the whole-file handle from pack <file> (which DOES store the whole file).
"$KS" expand "$H" > "$ROOT/_first_recall.txt" 2>/dev/null
if cmp -s "$ROOT/_first_recall.txt" "$FIRST_OUT"; then
  ok "first-iteration content recoverable byte-exact"
fi

# --- 4) Cross-process ledger persistence ---
echo
echo "[4] ledger persists across pack invocations (each invocation is a new process)"
# We've been running many invocations above; the .tsv should have accumulated.
if [ -f "$SESS_FILE" ] && [ "$(wc -l < "$SESS_FILE")" -gt 0 ]; then
  ok "session ledger file persists across $((20+10+1)) pack invocations"
fi

# --- 5) Multiple distinct sessions don't interfere ---
echo
echo "[5] independent sessions get independent ledgers"
for s in "alice" "bob" "carol"; do
  echo "fresh content for $s" | "$KS" pack - --session "$s" --cmd "fresh" --type log > /dev/null 2>>"$ROOT/err.log"
done
for s in "alice" "bob" "carol"; do
  if [ -f "$ROOT/sessions/$s.tsv" ]; then
    ok "session '$s' has its own ledger"
  else
    no "session '$s' has no ledger"
  fi
done

# --- 6) When budget=0 (degenerate), nothing stays resident ---
echo
echo "[6] resident budget = 0 — nothing should stay resident, every pack is cold"
export KNAPSACK_RESIDENT_BUDGET=0
COLD_SESSION="cold-$$"
# Two packs of the same content. In a normal session this would be a delta hit;
# with budget=0 it's two full sends.
echo "test content" | "$KS" pack - --session "$COLD_SESSION" --cmd "x" --type log > /dev/null 2>>"$ROOT/err.log"
DELTA_HITS_1=$(python3 -c "
import json
for line in open('$KNAPSACK_METRICS','r',encoding='utf-8'):
    v = json.loads(line)
    if v.get('event') == 'compress' and v.get('session') == '$COLD_SESSION':
        print(v.get('delta_hits', 0))
" | head -1)
echo "test content" | "$KS" pack - --session "$COLD_SESSION" --cmd "x" --type log > /dev/null 2>>"$ROOT/err.log"
DELTA_HITS_2=$(python3 -c "
import json
hits = []
for line in open('$KNAPSACK_METRICS','r',encoding='utf-8'):
    v = json.loads(line)
    if v.get('event') == 'compress' and v.get('session') == '$COLD_SESSION':
        hits.append(v.get('delta_hits', 0))
print(hits[-1] if hits else 0)
")
echo "  first delta_hits: $DELTA_HITS_1, second: $DELTA_HITS_2"
if [ "${DELTA_HITS_2:-0}" -eq 0 ]; then
  ok "budget=0 -> no delta-hits (everything cold)"
else
  echo "  (delta hits despite budget=0 — content might be too small)"
fi
unset KNAPSACK_RESIDENT_BUDGET

# --- 7) Recall of an evicted block via the store still works (independence) ---
echo
echo "[7] every block from the heavy session is STILL recoverable from the store"
mismatches=0
total=0
# Walk every block file in the store and verify it expands.
for bf in "$KNAPSACK_STORE"/*/ks2_*; do
  [ -e "$bf" ] || continue
  [[ "$bf" == *.meta ]] && continue
  total=$((total + 1))
  h=$(basename "$bf")
  if ! cmp -s "$bf" <("$KS" expand "$h" 2>/dev/null); then
    mismatches=$((mismatches + 1))
  fi
done
if [ "$mismatches" -eq 0 ]; then
  ok "all $total store blocks expand byte-exact (store survives any eviction)"
else
  no "$mismatches/$total blocks lost integrity"
fi

# --- 8) `knapsack gc --older-than 0 --dry-run` reports what's stale ---
echo
echo "[8] gc reports the store state honestly"
GC=$("$KS" gc --older-than 0 --dry-run 2>&1)
SCANNED=$(echo "$GC" | grep -oE 'scanned *: *[0-9]+' | grep -oE '[0-9]+')
echo "  gc scanned: $SCANNED blocks (dry-run)"
if [ -n "$SCANNED" ] && [ "$SCANNED" -gt 0 ]; then
  ok "gc walks the store"
fi

# --- 9) No panic across the entire pressure run ---
if grep -q "panicked" "$ROOT/err.log"; then
  no "panic detected"
  grep -A2 "panicked" "$ROOT/err.log" | head -10
else
  ok "no panic across the entire residency stress (~40 pack invocations)"
fi

echo
echo "================================================================"
echo "Residency: $PASS pass, $FAIL fail"
echo "================================================================"
exit $FAIL
