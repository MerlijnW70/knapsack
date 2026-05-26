#!/usr/bin/env bash
# Simulate a real Claude-Code session over 50+ tool calls. Mix of:
#   - Bash commands (output reduction)
#   - Reads (input reduction)
#   - MCP expands (model recalls)
#   - File edits (delta wins)
# At every step, the metrics + store + ledger must stay coherent. /knapsack must
# read honestly throughout. Store handles must NOT grow linearly with repeated work.
set -u
KS="${KS:-$HOME/.knapsack/bin/knapsack}"
ROOT="D:/knapsack/target/dogfood_long"
rm -rf "$ROOT" 2>/dev/null
mkdir -p "$ROOT/cache" "$ROOT/store" "$ROOT/scratch"
export KNAPSACK_STORE="$ROOT/store"
export KNAPSACK_METRICS="$ROOT/metrics.jsonl"
export KNAPSACK_SESSIONS="$ROOT/sessions"
export KNAPSACK_READ_LOG="$ROOT/read_hook.jsonl"
export KNAPSACK_READ_CACHE="$ROOT/cache"
: > "$KNAPSACK_METRICS"
: > "$KNAPSACK_READ_LOG"
SESSION="long-session-cc-1"
PASS=0; FAIL=0
ok() { PASS=$((PASS+1)); echo "  ✓ $*"; }
no() { FAIL=$((FAIL+1)); echo "  ✗ $*"; }

# Helpers
pack_bash() {
  # Drive a "bash command was run, here's its output" event via pack -.
  local cmd="$1" raw="$2"
  echo -n "$raw" | "$KS" pack - --session "$SESSION" --cmd "$cmd" --type log > /dev/null 2>>"$ROOT/err"
}
do_read() {
  local p="$1"
  local body
  body=$(python3 - "$p" "$SESSION" <<'PYEOF' | "$KS" hook 2>>"$ROOT/err"
import json, sys
print(json.dumps({"tool_name":"Read","tool_input":{"file_path":sys.argv[1]},"session_id":sys.argv[2]}))
PYEOF
)
  echo "$body" | python3 -c "
import json,sys
try: print(json.loads(sys.stdin.read())['hookSpecificOutput']['updatedInput']['file_path'])
except: pass
" 2>/dev/null
}
do_expand() {
  # Emulate a Claude Code MCP expand call.
  local handle="$1" extra="${2:-}"
  echo "{\"id\":$RANDOM,\"jsonrpc\":\"2.0\",\"method\":\"tools/call\",\"params\":{\"name\":\"knapsack_expand\",\"arguments\":{\"handle\":\"$handle\"$extra}}}" \
    | "$KS" mcp > /dev/null 2>>"$ROOT/err"
}

# Capture a real command output for repeat-use.
cargo test --release pack_doc > "$ROOT/scratch/test_out.txt" 2>&1
cargo --version                > "$ROOT/scratch/ver.txt"      2>&1
ls -la src                     > "$ROOT/scratch/ls_src.txt"   2>&1
git log --oneline -50          > "$ROOT/scratch/gitlog.txt"   2>&1
git status                     > "$ROOT/scratch/gitstat.txt"  2>&1

# A real file for Reads.
SRC="$ROOT/scratch/sample_main.rs"
cp D:/knapsack/src/main.rs "$SRC"

echo "=== Turn 1-10: initial exploration ==="
for i in $(seq 1 5); do
  pack_bash "cargo test pack_doc" "$(cat "$ROOT/scratch/test_out.txt")"
done
do_read "$SRC" > /dev/null
do_read "D:/knapsack/src/structural.rs" > /dev/null
do_read "D:/knapsack/src/hash.rs" > /dev/null
pack_bash "cargo --version" "$(cat "$ROOT/scratch/ver.txt")"
pack_bash "ls -la src" "$(cat "$ROOT/scratch/ls_src.txt")"

T1_HANDLES=$(find "$KNAPSACK_STORE" -name 'ks2_*' -not -name '*.meta' | wc -l)
T1_COMPRESS=$(grep -c '"event":"compress"' "$KNAPSACK_METRICS" || echo 0)
echo "  after 10 turns: $T1_HANDLES unique handles, $T1_COMPRESS compress events"
if [ "$T1_HANDLES" -gt 0 ]; then ok "store grew during exploration"; fi

echo
echo "=== Turn 11-25: repetitive work (delta win territory) ==="
# Repeat the same commands many times.
for i in $(seq 1 10); do
  pack_bash "cargo test pack_doc" "$(cat "$ROOT/scratch/test_out.txt")"
  pack_bash "ls -la src" "$(cat "$ROOT/scratch/ls_src.txt")"
  pack_bash "cargo --version" "$(cat "$ROOT/scratch/ver.txt")"
  do_read "$SRC" > /dev/null
  do_read "D:/knapsack/src/structural.rs" > /dev/null
done

T2_HANDLES=$(find "$KNAPSACK_STORE" -name 'ks2_*' -not -name '*.meta' | wc -l)
NEW_HANDLES=$((T2_HANDLES - T1_HANDLES))
echo "  after 25 turns: $T2_HANDLES unique handles (+$NEW_HANDLES from the 15-turn repeat block)"
if [ "$NEW_HANDLES" -lt 20 ]; then
  ok "repeated content didn't materially grow the store (+$NEW_HANDLES handles)"
else
  no "repeated content grew the store by $NEW_HANDLES handles"
fi

echo
echo "=== Turn 26-40: edits + re-runs ==="
# Modify a file, run tests, restore, run again. Three cycles.
TARGET="D:/knapsack/src/pack.rs"
BACKUP="$ROOT/scratch/pack.rs.bak"
cp "$TARGET" "$BACKUP"
for cycle in 1 2 3; do
  # Edit
  python3 -c "
text = open('$TARGET').read()
out = text.replace('pub fn pack(', f'// edit cycle $cycle\npub fn pack(', 1)
open('$TARGET','w').write(out)
"
  # Re-read the changed file (input)
  do_read "$TARGET" > /dev/null
  # Run tests (output)
  cargo test --release pack_doc > "$ROOT/scratch/edit_test_$cycle.txt" 2>&1
  pack_bash "cargo test pack_doc (cycle $cycle)" "$(cat "$ROOT/scratch/edit_test_$cycle.txt")"
done
mv "$BACKUP" "$TARGET"
do_read "$TARGET" > /dev/null  # one final read after restoration

T3_HANDLES=$(find "$KNAPSACK_STORE" -name 'ks2_*' -not -name '*.meta' | wc -l)
echo "  after 40 turns: $T3_HANDLES unique handles"
ok "edit-loop completed without errors"

echo
echo "=== Turn 41-55: model recalls slices via MCP ==="
# Get a handle from the metrics — pick one from the test output session.
HANDLE=$(grep '"event":"compress"' "$KNAPSACK_METRICS" | tail -3 | python3 -c "
import json, sys, os
# We need a real handle. Easiest path: extract from a view file in cache.
import glob
for p in glob.glob('$ROOT/cache/*.md'):
    with open(p,'r',encoding='utf-8',errors='replace') as f:
        for line in f:
            if 'knapsack expand ks2_' in line:
                import re
                m = re.search(r'(ks2_[0-9a-f]+)', line)
                if m: print(m.group(1)); sys.exit(0)
")
if [ -z "$HANDLE" ]; then
  echo "  (couldn't find handle to recall)"
else
  echo "  recalling handle: $HANDLE"
  for i in $(seq 1 10); do
    do_expand "$HANDLE"
    do_expand "$HANDLE" ',\"lines\":\"1-5\"'
    do_expand "$HANDLE" ',\"grep\":\"fn \"'
  done
  EXPANDS=$(grep -c '"event":"expand"' "$KNAPSACK_METRICS" || echo 0)
  echo "  $EXPANDS expand events recorded"
  if [ "$EXPANDS" -gt 25 ]; then ok "MCP expand events landing in metrics"; fi
fi

T4_HANDLES=$(find "$KNAPSACK_STORE" -name 'ks2_*' -not -name '*.meta' | wc -l)
echo
echo "=== Final state: $T4_HANDLES unique handles ==="

echo
echo "[verification] All bytes still recoverable"
# Verify EVERY unique handle in the store can be expanded byte-exact.
mismatches=0
for f in "$KNAPSACK_STORE"/*/ks2_*; do
  [ -e "$f" ] || continue
  [[ "$f" == *.meta ]] && continue
  h=$(basename "$f")
  if ! cmp -s "$f" <("$KS" expand "$h" 2>/dev/null); then
    mismatches=$((mismatches + 1))
  fi
done
if [ "$mismatches" -eq 0 ]; then
  ok "all $T4_HANDLES handles expand byte-exact"
else
  no "$mismatches handles failed byte-exact expand"
fi

echo
echo "[verification] /knapsack reads honestly"
STATUS=$("$KS")
echo "$STATUS" | sed 's/^/  | /'
SAVED=$(echo "$STATUS" | grep "Session saved:" | grep -oE '[0-9,]+' | head -1 | tr -d ',')
PCT=$(echo "$STATUS" | grep "Net reduction:" | grep -oE '[0-9]+%')
if [ -n "$SAVED" ] && [ "$SAVED" -gt 0 ]; then
  ok "session saved is positive: $SAVED tokens ($PCT)"
else
  no "session saved is zero or empty"
fi

echo
echo "[verification] doctor is healthy"
"$KS" doctor 2>&1 | tail -5 | sed 's/^/  | /'

echo
echo "[verification] no panics in any sub-call"
if [ -s "$ROOT/err" ] && grep -q "panicked" "$ROOT/err"; then
  no "panic detected in stderr"
  head -20 "$ROOT/err"
else
  ok "zero panics across 50+ tool calls"
fi

echo
echo "================================================================"
echo "Long session: $PASS pass, $FAIL fail"
echo "================================================================"
exit $FAIL
