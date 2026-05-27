#!/usr/bin/env bash
# Simulate a real multi-turn Claude-Code session against an ISOLATED store so we
# can read /knapsack and metrics back in known-clean state. The session sequence:
#
#  T1  noisy bash output -> packed (cold)
#  T2  same noisy bash output -> packed (warm, delta)
#  T3  model recalls a slice from T1 via knapsack expand --lines
#  T4  edit a source file, run tests again -> mostly back-ref
#  T5  read a large file via the Read hook -> redirect
#  T6  same file again -> cache hit
#  T7  GC pass (dry-run) -> nothing should be evicted (everything is recent)
#  T8  /knapsack should show real session_saved + reduction + store size
#  T9  MCP expand of one of the stored handles -> byte-exact
# T10  metrics line tail -> raw/shown numbers make sense
set -u
KS="${KS:-$HOME/.knapsack/bin/knapsack}"
ROOT="D:/knapsack/target/dogfood_session"
rm -rf "$ROOT" 2>/dev/null
mkdir -p "$ROOT/cache"
export KNAPSACK_STORE="$ROOT/store"
export KNAPSACK_METRICS="$ROOT/metrics.jsonl"
export KNAPSACK_SESSIONS="$ROOT/sessions"
export KNAPSACK_READ_LOG="$ROOT/read_hook.jsonl"
export KNAPSACK_READ_CACHE="$ROOT/cache"
: > "$KNAPSACK_METRICS"
: > "$KNAPSACK_READ_LOG"
SESSION="dogfood-real"
PASS=0; FAIL=0
say() { echo; echo "──── $* ────"; }
ok() { PASS=$((PASS+1)); echo "  ✓ $*"; }
no() { FAIL=$((FAIL+1)); echo "  ✗ $*"; }

cd "$(dirname "$0")/.."

# Capture a real noisy command once so all turns work from identical bytes.
TEST_OUT="$ROOT/test_out.txt"
cargo test --release pack_doc > "$TEST_OUT" 2>&1
say "T1: cold pack of cargo-test output ($(wc -c < "$TEST_OUT") bytes)"
"$KS" pack - --session "$SESSION" --cmd "cargo test pack_doc" --type log < "$TEST_OUT" > "$ROOT/T1.view" 2>/dev/null
read T1_raw T1_shown < <(tail -n 1 "$KNAPSACK_METRICS" | python3 -c "import json,sys; v=json.loads(sys.stdin.read()); sys.stdout.write(f\"{int(v['raw'])} {int(v['shown'])}\")")
echo "  raw=$T1_raw  shown=$T1_shown  saved=$((T1_raw - T1_shown))"
if [ "$T1_shown" -lt "$T1_raw" ]; then ok "cold pack saves tokens"; else no "cold pack didn't save"; fi

say "T2: warm pack (same bytes -> delta back-ref)"
"$KS" pack - --session "$SESSION" --cmd "cargo test pack_doc" --type log < "$TEST_OUT" > "$ROOT/T2.view" 2>/dev/null
read T2_raw T2_shown < <(tail -n 1 "$KNAPSACK_METRICS" | python3 -c "import json,sys; v=json.loads(sys.stdin.read()); sys.stdout.write(f\"{int(v['raw'])} {int(v['shown'])}\")")
echo "  raw=$T2_raw  shown=$T2_shown  saved=$((T2_raw - T2_shown))"
if [ "$T2_shown" -lt "$T1_shown" ]; then ok "warm pack is smaller than cold (delta hit)"; else no "warm pack didn't shrink further"; fi

say "T3: model recalls a slice via knapsack expand --lines"
# Find a handle from the T1 view. The view text contains ks2_... handles in headers.
HANDLE=$(grep -oE 'ks2_[0-9a-f]+' "$ROOT/T1.view" | head -1)
if [ -z "$HANDLE" ]; then HANDLE=$(grep -oE 'ks2_[0-9a-f]+' "$ROOT/T2.view" | head -1); fi
if [ -n "$HANDLE" ]; then
  SLICE=$( "$KS" expand "$HANDLE" --lines "1-3" 2>&1 | head -3 )
  if [ -n "$SLICE" ] && ! echo "$SLICE" | grep -q "no such handle"; then
    ok "expand --lines 1-3 returned content for $HANDLE"
  else
    no "expand --lines 1-3 failed for $HANDLE: $SLICE"
  fi
  # Now do a grep with context
  GREP=$( "$KS" expand "$HANDLE" --grep "test" --context 1 2>&1 | head -5 )
  if [ -n "$GREP" ]; then
    ok "expand --grep 'test' --context 1 returned content"
  else
    no "expand --grep failed"
  fi
else
  echo "  (no handle found in view — skip)"
fi

say "T4: edit a source file, run tests again (delta encoding the diff)"
TARGET="src/pack.rs"
BACKUP="$ROOT/pack.rs.bak"
cp "$TARGET" "$BACKUP"
python3 -c "
import sys
text = open(sys.argv[1]).read()
text = text.replace('pub fn pack(', '// dogfood edit\npub fn pack(', 1)
open(sys.argv[1], 'w').write(text)
" "$TARGET"
EDIT_OUT="$ROOT/edit_test_out.txt"
cargo test --release pack_doc > "$EDIT_OUT" 2>&1
mv "$BACKUP" "$TARGET"
"$KS" pack - --session "$SESSION" --cmd "cargo test pack_doc (after edit)" --type log < "$EDIT_OUT" > /dev/null 2>/dev/null
read T4_raw T4_shown < <(tail -n 1 "$KNAPSACK_METRICS" | python3 -c "import json,sys; v=json.loads(sys.stdin.read()); sys.stdout.write(f\"{int(v['raw'])} {int(v['shown'])}\")")
echo "  raw=$T4_raw  shown=$T4_shown  saved=$((T4_raw - T4_shown))"
if [ "$T4_shown" -lt "$T1_shown" ]; then ok "post-edit pack mostly back-references prior session"; else no "post-edit pack didn't benefit from session history"; fi

say "T5: large file Read via the hook -> redirect"
ENVELOPE='{"tool_name":"Read","tool_input":{"file_path":"D:/knapsack/src/main.rs"}}'
T5_OUT=$(echo "$ENVELOPE" | "$KS" hook 2>&1)
if echo "$T5_OUT" | grep -q "updatedInput"; then
  REDIRECT=$(echo "$T5_OUT" | python3 -c "import json,sys; print(json.loads(sys.stdin.read())['hookSpecificOutput']['updatedInput']['file_path'])")
  if [ -f "$REDIRECT" ]; then
    ok "redirect emitted, cache file exists: $REDIRECT"
  else
    no "redirect emitted but cache file missing: $REDIRECT"
  fi
else
  no "no redirect for large file: $T5_OUT"
fi

say "T6: same file again -> cache hit"
T6_OUT=$(echo "$ENVELOPE" | "$KS" hook 2>&1)
if echo "$T6_OUT" | grep -q "updatedInput"; then
  REDIRECT2=$(echo "$T6_OUT" | python3 -c "import json,sys; print(json.loads(sys.stdin.read())['hookSpecificOutput']['updatedInput']['file_path'])")
  if [ "$REDIRECT" = "$REDIRECT2" ]; then
    ok "second read returned the same cache path (cache hit)"
  else
    no "second read produced a different path: $REDIRECT vs $REDIRECT2"
  fi
  # And the why-last entry should note cache-hit
  last_note=$(tail -n 1 "$KNAPSACK_READ_LOG" | python3 -c "import json,sys; print(json.loads(sys.stdin.read()).get('note',''))")
  if [ "$last_note" = "cache-hit" ]; then ok "log notes cache-hit"; else echo "  log note: $last_note"; fi
fi

say "T7: GC dry-run on fresh blocks -> nothing should be removed"
GC=$( "$KS" gc --dry-run --older-than 30 2>&1 )
deleted=$(echo "$GC" | grep -oE 'deleted *: *[0-9]+' | grep -oE '[0-9]+')
echo "  $GC" | head -3
if [ "${deleted:-0}" = "0" ]; then ok "GC dry-run found no stale blocks"; else no "GC reported $deleted deletions"; fi

say "T8: /knapsack — does the surface reflect real session data?"
# Default surface is the compact, user-facing summary; the Store line + Lifetime
# footer live under --verbose. We capture both so this test can verify the
# user-facing strings AND the engineer-facing detail without re-running the binary
# (cheaper, and keeps the assertions co-located).
STATUS=$( "$KS" )
STATUS_V=$( "$KS" status --verbose )
echo "$STATUS" | sed 's/^/  | /'
# Header is state-driven: "saving context" (net>0) / "active" (net<=0 with work) /
# "ready" (no activity). T1-T6 packed several blobs into this session so we expect
# "saving context" — but allow "active" (e.g. if MCP recall pushed net non-positive).
if echo "$STATUS" | grep -qE "Knapsack is (saving context|active)"; then ok "status header reflects activity"; else no "status header not active"; fi
if echo "$STATUS" | grep -q "Input reduction:    active"; then ok "input reduction active"; else no "input reduction not active"; fi
if echo "$STATUS" | grep -q "Output reduction:   active"; then ok "output reduction active"; else no "output reduction not active"; fi
if echo "$STATUS" | grep -qE "Saved this session: +[0-9,]+ tokens"; then ok "shows real session saved tokens"; else no "no session_saved figure"; fi
# Reduction may be negative honestly (the spec allows it); -? in the regex.
if echo "$STATUS" | grep -qE "Reduction: +-?[0-9]+%"; then ok "shows real reduction %"; else no "no reduction figure"; fi
# Store moved to --verbose; the assertion follows.
if echo "$STATUS_V" | grep -qE "Store: +[0-9,]+ blocks"; then ok "store has blocks (verbose)"; else no "store is empty"; fi
if echo "$STATUS" | grep -q "Recall:             healthy"; then ok "recall healthy"; else no "recall not healthy"; fi

say "T9: MCP expand a stored handle (drives the MCP path the way Claude would)"
if [ -n "$HANDLE" ]; then
  REQ="{\"id\":1,\"jsonrpc\":\"2.0\",\"method\":\"tools/call\",\"params\":{\"name\":\"knapsack_expand\",\"arguments\":{\"handle\":\"$HANDLE\"}}}"
  MCP=$( echo "$REQ" | "$KS" mcp 2>&1 | head -1 )
  if echo "$MCP" | python3 -c "import json,sys; v=json.loads(sys.stdin.read()); sys.exit(0 if 'result' in v and not v['result'].get('isError', False) else 1)" 2>/dev/null; then
    ok "MCP expand returned success for $HANDLE"
  else
    no "MCP expand failed: $(echo "$MCP" | head -c 200)"
  fi
fi

say "T10: metrics tail — does the JSONL match what /knapsack says?"
N_EVENTS=$(grep -c '"event":"compress"' "$KNAPSACK_METRICS" 2>/dev/null || echo 0)
echo "  compress events: $N_EVENTS"
if [ "$N_EVENTS" -ge 3 ]; then ok "metrics JSONL has events for every pack"; else no "fewer events than expected"; fi
# Compute net saved from the JSONL and compare with /knapsack output
# The ab "session" row formats like: "dogfood-real  7,788  4,350  0  4,350  27  0  0(0)"
# Columns:                            session       raw    saved refetch net   delta evict exp(f)
# Pull the dogfood-real row and read column 5 (net). awk strips commas as a side
# effect of treating the field as text-then-int.
NET=$( "$KS" ab --knapsack "$KNAPSACK_METRICS" 2>&1 \
       | awk '/^[[:space:]]+dogfood-real/ { gsub(",","",$5); print $5 }' )
echo "  ab reports net saved: $NET"
SURFACE_NET=$(echo "$STATUS" | grep "Saved this session:" | grep -oE '[0-9,]+' | tr -d ',')
echo "  /knapsack reports:    $SURFACE_NET"
if [ -n "$NET" ] && [ -n "$SURFACE_NET" ] && [ "$NET" = "$SURFACE_NET" ]; then
  ok "ab and /knapsack agree on session net ($NET tokens)"
elif [ -z "$SURFACE_NET" ] || [ -z "$NET" ]; then
  echo "  (skip cross-check — couldn't parse one of them)"
else
  no "ab=$NET vs /knapsack=$SURFACE_NET"
fi

echo
echo "================================================================"
echo "Session simulation: $PASS pass, $FAIL fail"
echo "================================================================"
exit $FAIL
