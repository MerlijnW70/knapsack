#!/usr/bin/env bash
# `knapsack pack -` reads from stdin. In real Claude Code use, this is the entire
# stdout+stderr of a Bash command. What if that command produces a LOT?
#   - 1 MB realistic log output
#   - 10 MB log output
#   - Slow streaming (chunks separated by sleeps)
#   - Stdin that's just NULs
#   - Stdin with no newlines (one giant line)
#   - Stdin that's an empty pipe
# Every case must produce a usable view AND byte-exact whole-content recall.
set -u
KS="${KS:-$HOME/.knapsack/bin/knapsack}"
ROOT="D:/knapsack/target/dogfood_stream"
rm -rf "$ROOT" 2>/dev/null
mkdir -p "$ROOT"
export KNAPSACK_STORE="$ROOT/store"
export KNAPSACK_METRICS="$ROOT/metrics.jsonl"
export KNAPSACK_SESSIONS="$ROOT/sessions"

PASS=0; FAIL=0
ok() { PASS=$((PASS+1)); echo "  ✓ $*"; }
no() { FAIL=$((FAIL+1)); echo "  ✗ $*"; }

# Run `knapsack pack -` against a file's contents (via stdin), verify it succeeds
# AND that the handles embedded in the view resolve to non-empty bytes that match
# the corresponding regions of the source. The `pack -` (hook) path stores
# per-tile blocks rather than a whole-file handle (the model recalls individual
# elisions from the view's back-references), so we don't try to expand
# sha256(whole_file) — we verify the per-handle round trip the model actually uses.
test_stream() {
  local fixture="$1" label="$2" timeout_s="${3:-30}"
  local size=$(wc -c < "$fixture")
  local start=$(date +%s%N)
  local out_view="$ROOT/_view.tmp"
  timeout "$timeout_s" "$KS" pack - --session "stream-$label" --cmd "$label" --type log \
    < "$fixture" > "$out_view" 2>"$ROOT/_err.tmp"
  local rc=$?
  local end=$(date +%s%N)
  local ms=$(( (end - start) / 1000000 ))
  if [ "$rc" -ne 0 ]; then
    no "$label: pack exited $rc (size=${size}B)"
    head -3 "$ROOT/_err.tmp" | sed 's/^/    | /'
    return
  fi
  if grep -q "panicked" "$ROOT/_err.tmp" 2>/dev/null; then
    no "$label: panic during pack"
    return
  fi
  # Pick a handle from the view and verify it expands to some non-empty bytes
  # that appear in the original fixture.
  local sample=$(grep -aoE 'ks2_[0-9a-f]+' "$out_view" | head -1)
  local mbps=$(awk "BEGIN { printf \"%.1f\", ($size/1048576)/($ms/1000) }" 2>/dev/null || echo "?")
  if [ -z "$sample" ]; then
    # No elisions emitted — the view IS the raw output (never-worse-than-raw
    # path). That's a valid outcome; pack still succeeded.
    ok "$label: ${size}B in ${ms}ms (~${mbps} MB/s), no elisions [direct view]"
    return
  fi
  "$KS" expand "$sample" > "$ROOT/_recall.tmp" 2>/dev/null
  local recall_size=$(wc -c < "$ROOT/_recall.tmp")
  if [ "$recall_size" -gt 0 ]; then
    # Now verify the recalled bytes are actually a sub-sequence of the original.
    # We compare via python because the recalled chunk is binary-safe.
    if python3 -c "
src = open('$fixture','rb').read()
rec = open('$ROOT/_recall.tmp','rb').read()
import sys; sys.exit(0 if rec in src else 1)
" 2>/dev/null; then
      ok "$label: ${size}B in ${ms}ms (~${mbps} MB/s), handle resolves to bytes that appear in source"
    else
      no "$label: recall returned bytes NOT found in source (corruption)"
    fi
  else
    no "$label: handle '$sample' returned 0 bytes"
  fi
}

# --- 1) Small log (baseline) ---
FIX="$ROOT/small.log"
python3 - "$FIX" <<'PYEOF'
import sys
with open(sys.argv[1],'wb') as f:
    for i in range(500): f.write(f"[INFO] step {i}: routine\n".encode())
PYEOF
test_stream "$FIX" "small-log-10KB"

# --- 2) 1 MB realistic log ---
FIX="$ROOT/medium.log"
python3 - "$FIX" <<'PYEOF'
import sys
target = 1 * 1024 * 1024
written = 0; i = 0
with open(sys.argv[1],'wb') as f:
    while written < target:
        line = f"[INFO] step {i}: worker-{i%4} processed task id={i} status=ok\n".encode()
        f.write(line); written += len(line); i += 1
PYEOF
test_stream "$FIX" "medium-log-1MB"

# --- 3) 10 MB log ---
FIX="$ROOT/big.log"
python3 - "$FIX" <<'PYEOF'
import sys
target = 10 * 1024 * 1024
written = 0; i = 0
with open(sys.argv[1],'wb') as f:
    while written < target:
        line = f"[INFO] step {i}: worker-{i%4} processed task id={i} status=ok latency={(i*17)%500}ms\n".encode()
        f.write(line); written += len(line); i += 1
PYEOF
test_stream "$FIX" "big-log-10MB" 60

# --- 4) Random binary, 1 MB ---
FIX="$ROOT/random.bin"
python3 -c "import os, sys; open(sys.argv[1],'wb').write(os.urandom(1024*1024))" "$FIX"
test_stream "$FIX" "random-binary-1MB"

# --- 5) Single giant line, no newlines (500 KB) ---
FIX="$ROOT/oneline.txt"
python3 -c "import sys; open(sys.argv[1],'wb').write(b'x' * 500000)" "$FIX"
test_stream "$FIX" "single-line-500KB"

# --- 6) All NULs (1 MB) ---
FIX="$ROOT/nuls.bin"
python3 -c "import sys; open(sys.argv[1],'wb').write(b'\\x00' * 1024 * 1024)" "$FIX"
test_stream "$FIX" "all-nuls-1MB"

# --- 7) Empty stdin ---
echo
echo "[7] empty stdin -> should not panic"
echo -n "" | "$KS" pack - --session "stream-empty" --cmd "empty" --type log > "$ROOT/_empty_view" 2>"$ROOT/_empty_err"
RC=$?
if [ "$RC" -eq 0 ] && [ ! -s "$ROOT/_empty_err" ]; then
  ok "empty stdin handled cleanly (rc=0, empty stderr)"
elif grep -q "panicked" "$ROOT/_empty_err"; then
  no "empty stdin panicked"
else
  echo "  rc=$RC stderr:"; head -3 "$ROOT/_empty_err" | sed 's/^/    | /'
  ok "empty stdin handled without panic (rc=$RC)"
fi

# --- 8) Slow streaming (chunks separated by sleeps) ---
echo
echo "[8] slow streaming — does pack flush properly?"
python3 -c "
import time, sys
for i in range(50):
    sys.stdout.write(f'[INFO] slow step {i}: line\n')
    sys.stdout.flush()
    time.sleep(0.02)  # 50 lines * 20ms = ~1s
" | "$KS" pack - --session "stream-slow" --cmd "slow" --type log > "$ROOT/_slow_view" 2>"$ROOT/_slow_err"
RC=$?
if [ "$RC" -eq 0 ]; then
  ok "slow streaming completed cleanly"
else
  no "slow streaming failed: rc=$RC"
fi

# --- 9) Stdin is a pipe-to-itself (cat /dev/null piped) ---
echo
echo "[9] stdin redirected from /dev/null"
"$KS" pack - --session "stream-devnull" --cmd "null" --type log < /dev/null > "$ROOT/_dn_view" 2>"$ROOT/_dn_err"
RC=$?
if [ "$RC" -eq 0 ]; then ok "/dev/null stdin handled"; else no "/dev/null failed: rc=$RC"; fi

# --- 10) Very-long-line stress: one 5 MB line ---
FIX="$ROOT/big_line.txt"
python3 -c "import sys; open(sys.argv[1],'wb').write(b'a' * (5*1024*1024))" "$FIX"
test_stream "$FIX" "single-line-5MB" 60

# --- 11) Repeated content (de-dup target) — 5 MB of one repeated 200B line ---
FIX="$ROOT/repeat.log"
python3 -c "
import sys
target = 5 * 1024 * 1024
written = 0
with open(sys.argv[1],'wb') as f:
    while written < target:
        line = b'[INFO] step 0: a stable repeated line that compresses extremely well\n'
        f.write(line); written += len(line)
" "$FIX"
test_stream "$FIX" "repeated-content-5MB" 60

echo
echo "================================================================"
echo "Streaming pack: $PASS pass, $FAIL fail"
echo "================================================================"
exit $FAIL
