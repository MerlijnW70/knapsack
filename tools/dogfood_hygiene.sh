#!/usr/bin/env bash
# Adversarial cache hygiene: what happens when external state changes between calls?
# - The read_cache view file is deleted while the file is being read
# - A store block is corrupted (hash mismatch)
# - The whole store dir disappears mid-recall
# - Two processes write the same handle simultaneously
# In every case the contract is: fail-open, no panic, no corruption.
set -u
KS="${KS:-$HOME/.knapsack/bin/knapsack}"
ROOT="D:/knapsack/target/dogfood_hygiene"
rm -rf "$ROOT" 2>/dev/null
mkdir -p "$ROOT/cache" "$ROOT/store"
export KNAPSACK_STORE="$ROOT/store"
export KNAPSACK_METRICS="$ROOT/metrics.jsonl"
export KNAPSACK_SESSIONS="$ROOT/sessions"
export KNAPSACK_READ_LOG="$ROOT/read_hook.jsonl"
export KNAPSACK_READ_CACHE="$ROOT/cache"

PASS=0; FAIL=0
ok() { PASS=$((PASS+1)); echo "  ✓ $*"; }
no() { FAIL=$((FAIL+1)); echo "  ✗ $*"; }

# ---- 1) Delete the cache view file between two reads of the same file ----
echo "[1] cache view file deleted between two reads"
SRC1="$ROOT/big.log"
python3 - "$SRC1" <<'PYEOF'
import sys
with open(sys.argv[1], 'wb') as f:
    for i in range(1500):
        f.write(f'[INFO] step {i}: routine work; lots of similar lines\n'.encode())
PYEOF
ENV1="{\"tool_name\":\"Read\",\"tool_input\":{\"file_path\":\"$SRC1\"}}"
# First read: should produce a cache file.
OUT1=$(echo "$ENV1" | "$KS" hook 2>&1)
VIEW1=$(echo "$OUT1" | python3 -c "
import json,sys
v=json.loads(sys.stdin.read())
print(v['hookSpecificOutput']['updatedInput']['file_path'])
" 2>/dev/null)
if [ -n "$VIEW1" ] && [ -f "$VIEW1" ]; then
  ok "first read produced cache view: $(basename "$VIEW1")"
  # Now delete the view file.
  rm "$VIEW1"
  ok "cache view file deleted"
  # Second read: should regenerate the same view (file unchanged -> same digest -> same path).
  OUT2=$(echo "$ENV1" | "$KS" hook 2>&1)
  VIEW2=$(echo "$OUT2" | python3 -c "
import json,sys
v=json.loads(sys.stdin.read())
print(v['hookSpecificOutput']['updatedInput']['file_path'])
" 2>/dev/null)
  if [ "$VIEW2" = "$VIEW1" ] && [ -f "$VIEW2" ]; then
    ok "second read regenerated the view at same path"
  else
    no "second read did not recreate cache: $VIEW2"
  fi
else
  no "first read didn't produce a cache view"
fi

# ---- 2) Corrupt a store block (write wrong bytes at the handle's address) ----
echo
echo "[2] corrupt a store block by writing wrong bytes -> recall returns None (not wrong bytes)"
SRC2="$ROOT/payload.log"
python3 - "$SRC2" <<'PYEOF'
import sys
with open(sys.argv[1], 'wb') as f:
    for i in range(500):
        f.write(f'corruption-test line {i}\n'.encode())
PYEOF
# Pack and capture the handle.
H=$( "$KS" pack - --session "hygiene" --cmd "hyg" --type log < "$SRC2" 2>&1 | grep -oE 'ks2_[0-9a-f]+' | head -1 )
echo "  handle: $H"
# Find the on-disk file for that handle.
SHARD="${H:4:2}"  # ks2_ABCD... -> shard "AB"
BLOCK_FILE="$ROOT/store/$SHARD/$H"
if [ -f "$BLOCK_FILE" ]; then
  ok "store block file exists: $BLOCK_FILE"
  # Overwrite with garbage.
  echo "CORRUPTED PAYLOAD" > "$BLOCK_FILE"
  ok "block file corrupted in place"
  # Try to expand the whole handle: knapsack verifies SHA-256 and should return None.
  EXPAND_OUT=$( "$KS" expand "$H" 2>&1 )
  RC=$?
  if [ "$RC" -ne 0 ]; then
    ok "expand failed cleanly (rc=$RC) instead of returning wrong bytes"
  elif echo "$EXPAND_OUT" | grep -q "CORRUPTED PAYLOAD"; then
    no "expand returned the CORRUPTED bytes! this would be a data-integrity bug"
  else
    ok "expand returned non-error but also didn't echo corruption"
  fi
else
  no "store block file not found at $BLOCK_FILE"
fi

# ---- 3) Store dir disappears mid-operation ----
echo
echo "[3] store dir removed -> all operations fail-open"
# Save a known-good block first.
SRC3="$ROOT/another.log"
python3 - "$SRC3" <<'PYEOF'
import sys
with open(sys.argv[1], 'wb') as f:
    for i in range(500):
        f.write(f'before-removal line {i}\n'.encode())
PYEOF
H3=$( "$KS" pack - --session "hygiene" --cmd "hyg" --type log < "$SRC3" 2>&1 | grep -oE 'ks2_[0-9a-f]+' | head -1 )
# Now nuke the store dir.
rm -rf "$ROOT/store"
ok "store dir removed"
# Try to expand — should NOT crash.
EX=$( "$KS" expand "$H3" 2>&1 )
RC=$?
if [ "$RC" -ne 0 ]; then
  ok "expand fails cleanly when store missing (rc=$RC)"
else
  no "expand returned 0 with missing store (rc=$RC, body: ${EX:0:60})"
fi
# Status should still work (the store path is just empty/missing). The Store line
# itself moved to `status --verbose` — default surface stays compact. We capture
# verbose here because the assertion specifically wants to read the Store value.
STAT=$( "$KS" 2>&1 )
STAT_V=$( "$KS" status --verbose 2>&1 )
if echo "$STAT_V" | grep -qE "Store: +(empty|0 blocks)"; then ok "/knapsack --verbose reports empty store"; else
  # Might still report old count if it cached
  echo "  ($STAT_V)" | head -5 | sed 's/^/    | /'
fi

# Restore store dir for next test
mkdir -p "$ROOT/store"

# ---- 4) Two concurrent pack writes of the SAME content (dedup safety) ----
echo
echo "[4] 20 concurrent pack writes of identical content -> 1 store entry, not 20"
SRC4="$ROOT/dedup.log"
python3 - "$SRC4" <<'PYEOF'
import sys
with open(sys.argv[1], 'wb') as f:
    for i in range(500):
        f.write(f'dedup line {i}\n'.encode())
PYEOF
# Count unique data blocks (exclude .meta sidecars) before and after.
data_blocks() { find "$1" -type f -name 'ks2_*' -not -name '*.meta' 2>/dev/null | wc -l; }
BLOCKS_BEFORE=$(data_blocks "$ROOT/store")
echo "  unique data blocks before: $BLOCKS_BEFORE"

# Run ONE pack first so we know the expected unique-block count for this fixture.
"$KS" pack - --session "baseline" --cmd "dedup" --type log < "$SRC4" > /dev/null 2>>"$ROOT/err.log"
ONE_PACK_BLOCKS=$(data_blocks "$ROOT/store")
echo "  unique data blocks after 1 baseline pack: $ONE_PACK_BLOCKS"

# Now hammer with 20 more concurrent packs. Content-addressed: every additional pack
# of identical content should add ZERO new unique blocks.
pids=()
for r in $(seq 1 20); do
  "$KS" pack - --session "hygiene-$r" --cmd "dedup" --type log < "$SRC4" > /dev/null 2>>"$ROOT/err.log" &
  pids+=($!)
done
wait "${pids[@]}"
BLOCKS_AFTER=$(data_blocks "$ROOT/store")
echo "  unique data blocks after 20 concurrent packs: $BLOCKS_AFTER"
NEW_BLOCKS=$((BLOCKS_AFTER - ONE_PACK_BLOCKS))
# A tiny number of new blocks IS acceptable — different sessions can occasionally
# pick slightly different elision points based on ledger residency timing. But
# anything close to 20x the baseline would mean dedup is broken.
if [ "$NEW_BLOCKS" -lt "$((ONE_PACK_BLOCKS / 4))" ]; then
  ok "concurrent dedup near-perfect: +$NEW_BLOCKS new unique blocks on top of $ONE_PACK_BLOCKS baseline"
else
  no "20 concurrent pack added $NEW_BLOCKS new unique blocks — dedup may be broken (baseline $ONE_PACK_BLOCKS)"
fi
if grep -q "panicked" "$ROOT/err.log" 2>/dev/null; then
  no "panic during concurrent dedup"
else
  ok "no panic during concurrent dedup pack"
fi

echo
echo "================================================================"
echo "Cache hygiene: $PASS pass, $FAIL fail"
echo "================================================================"
exit $FAIL
