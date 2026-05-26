#!/usr/bin/env bash
# Race the store against the GC: spawn N writers (knapsack store put), a few readers
# (knapsack expand), and a GC pass interleaved. We then verify every handle the
# writers produced is still recoverable (the GC pass uses --older-than 365 so it
# shouldn't delete fresh blocks; this exercises the lock/IO behavior, not eviction).
set -u
KS="${KS:-$HOME/.knapsack/bin/knapsack}"
TMPROOT=$(mktemp -d)
export KNAPSACK_STORE="$TMPROOT/store"
export KNAPSACK_METRICS="$TMPROOT/m.jsonl"

mkdir -p "$TMPROOT/in"
# 50 distinct files, 4-32 KB each.
for i in $(seq 1 50); do
  size=$((2048 + i * 600))
  python3 -c "
import sys, os
sys.stdout.buffer.write(b'block-$i ' + os.urandom($size))
" > "$TMPROOT/in/file-$i.bin"
done

# Concurrent writer wave.
pids=()
for f in "$TMPROOT"/in/file-*.bin; do
  "$KS" store put "$f" > "$TMPROOT/handles.tmp" 2>>"$TMPROOT/err.log" &
  pids+=($!)
done

# Interleave a few GC passes and reader runs.
for r in 1 2 3; do
  "$KS" gc --older-than 365 > /dev/null 2>>"$TMPROOT/err.log" &
  pids+=($!)
done

wait "${pids[@]}"

# Now collect every handle by re-packing each file (deterministic per content).
> "$TMPROOT/handles.txt"
for f in "$TMPROOT"/in/file-*.bin; do
  h=$( "$KS" pack "$f" --dry-run 2>&1 | grep -oE 'ks2_[0-9a-f]+' | head -1 )
  echo "$h $f" >> "$TMPROOT/handles.txt"
done

# Verify every block expands byte-exactly.
ok=0; bad=0
while read -r h f; do
  if "$KS" expand "$h" 2>/dev/null | cmp -s - "$f"; then
    ok=$((ok+1))
  else
    bad=$((bad+1))
    echo "BAD: $h <-> $f"
  fi
done < "$TMPROOT/handles.txt"

echo
echo "Concurrent store+gc: $ok recovered, $bad lost (after $(echo "${pids[@]}" | wc -w) parallel ops)"
echo "stderr log size: $(wc -c < "$TMPROOT/err.log") bytes"
if [ -s "$TMPROOT/err.log" ]; then
  echo "--- stderr (first 30 lines) ---"
  head -30 "$TMPROOT/err.log"
fi
rm -rf "$TMPROOT"
exit $bad
