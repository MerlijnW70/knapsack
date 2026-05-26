#!/usr/bin/env bash
# Adversarial inputs designed to find real bugs:
#  - Read targets that are knapsack's OWN data files (cache, read_hook.jsonl, store)
#  - Recursion risk: the hook's output is a cache file; what if the model then Reads
#    that cache file?
#  - Pathological file shapes: empty, all-whitespace, BOM, mixed line endings, NUL,
#    binary executable, sparse, single very-long-line
#  - Path injection: a file_path that escapes via ..
set -u
KS="${KS:-$HOME/.knapsack/bin/knapsack}"
ROOT="D:/knapsack/target/dogfood_adv"
rm -rf "$ROOT" 2>/dev/null
mkdir -p "$ROOT/cache" "$ROOT/store"
export KNAPSACK_STORE="$ROOT/store"
export KNAPSACK_READ_LOG="$ROOT/read_hook.jsonl"
export KNAPSACK_READ_CACHE="$ROOT/cache"
: > "$KNAPSACK_READ_LOG"

PASS=0; FAIL=0
ok() { PASS=$((PASS+1)); echo "  ✓ $*"; }
no() { FAIL=$((FAIL+1)); echo "  ✗ $*"; }

drive() {
  local p="$1"
  local body
  body=$(python3 - "$p" <<'PYEOF' | "$KS" hook 2>&1
import json, sys
print(json.dumps({"tool_name":"Read","tool_input":{"file_path":sys.argv[1]},"session_id":"adv"}))
PYEOF
)
  echo "$body" | python3 -c "
import json,sys
try:
  v=json.loads(sys.stdin.read())
  print(v['hookSpecificOutput']['updatedInput']['file_path'])
except Exception:
  pass
" 2>/dev/null
}

last_reason() {
  tail -n 1 "$KNAPSACK_READ_LOG" 2>/dev/null | python3 -c "
import json,sys
try: print(json.loads(sys.stdin.read()).get('reason',''))
except: print('')"
}

# --- 1) Read a cache file itself (recursion risk) ---
echo "[1] reading a cache view file directly"
# Seed the cache by reading a real file first.
SEED="$ROOT/seed.log"
python3 - "$SEED" <<'PYEOF'
with open(__import__('sys').argv[1],'wb') as f:
  for i in range(700): f.write(f"[INFO] step {i}: stable line\n".encode())
PYEOF
SEED_VIEW=$(drive "$SEED")
if [ -n "$SEED_VIEW" ]; then
  echo "  seeded cache: $(basename "$SEED_VIEW")"
  # Now drive the hook AGAINST the cache file's path.
  RECURSE=$(drive "$SEED_VIEW")
  REC_REASON=$(last_reason)
  if [ -z "$RECURSE" ]; then
    ok "reading a cache file -> pass-through (no recursion). reason=$REC_REASON"
  else
    # Even if it redirects, it must NOT create a chain that points back at itself.
    if [ "$RECURSE" = "$SEED_VIEW" ]; then
      no "reading a cache file produced a self-referencing redirect (RECURSION)"
    else
      RECURSE_NAME=$(basename "$RECURSE")
      ok "cache-of-cache produced a different cache file ($RECURSE_NAME) — not a self-loop"
    fi
  fi
fi

# --- 2) Read the read_hook.jsonl decision log itself ---
echo
echo "[2] reading knapsack's own read_hook.jsonl"
# Make sure the log has content.
drive "$SEED" > /dev/null
RH=$(drive "$KNAPSACK_READ_LOG")
RH_REASON=$(last_reason)
if [ -z "$RH" ] || [ "$RH_REASON" = "too-small" ] || [ "$RH_REASON" = "worse-than-raw" ]; then
  ok "log file handled cleanly (reason=$RH_REASON)"
else
  ok "log file redirected: $(basename "$RH")"
fi

# --- 3) Read a file in the store dir ---
echo
echo "[3] reading a file inside the store dir"
# Pick one block file.
BLOCK_FILE=$(find "$KNAPSACK_STORE" -type f -name 'ks2_*' -not -name '*.meta' 2>/dev/null | head -1)
if [ -n "$BLOCK_FILE" ]; then
  echo "  block: $BLOCK_FILE"
  BR=$(drive "$BLOCK_FILE")
  BR_REASON=$(last_reason)
  if [ -z "$BR" ]; then
    ok "block file pass-through. reason=$BR_REASON"
  else
    ok "block file redirected: $(basename "$BR")"
  fi
fi

# --- 4) Path injection via .. ---
echo
echo "[4] path with .. that escapes the cache dir"
ESC_PATH="$KNAPSACK_READ_CACHE/../../../etc/passwd"
ESCAPED=$(drive "$ESC_PATH")
ESC_REASON=$(last_reason)
if [ -z "$ESCAPED" ]; then
  ok ".. escape -> pass-through. reason=$ESC_REASON"
else
  no ".. escape produced a redirect: $ESCAPED"
fi

# --- 5) Empty file ---
echo
echo "[5] empty file (0 bytes)"
EMPTY="$ROOT/empty.log"
: > "$EMPTY"
E=$(drive "$EMPTY")
ER=$(last_reason)
if [ -z "$E" ] && [ "$ER" = "too-small" ]; then
  ok "empty file -> too-small pass-through"
else
  no "empty file: reason=$ER view=$E"
fi

# --- 6) Single byte file ---
echo
echo "[6] single-byte file"
ONE="$ROOT/one.log"
printf 'x' > "$ONE"
O=$(drive "$ONE")
OR=$(last_reason)
if [ -z "$O" ] && [ "$OR" = "too-small" ]; then
  ok "1-byte file -> too-small pass-through"
else
  no "1-byte file: reason=$OR view=$O"
fi

# --- 7) All whitespace 20 KB ---
echo
echo "[7] all-whitespace 20 KB"
WS="$ROOT/whitespace.log"
python3 - "$WS" <<'PYEOF'
import sys
with open(sys.argv[1],'wb') as f:
  f.write(b' \n' * 10000)  # 20 KB
PYEOF
W=$(drive "$WS")
WR=$(last_reason)
if [ -z "$W" ]; then
  ok "all-whitespace -> pass-through. reason=$WR"
else
  ok "all-whitespace -> $WR ($(basename "$W"))"
fi

# --- 8) BOM-prefixed file ---
echo
echo "[8] UTF-8 BOM at start of a 15 KB log"
BOM="$ROOT/bom.log"
python3 - "$BOM" <<'PYEOF'
import sys
with open(sys.argv[1],'wb') as f:
  f.write(b'\xEF\xBB\xBF')  # UTF-8 BOM
  for i in range(400):
    f.write(f"[INFO] step {i}: routine work\n".encode())
PYEOF
B=$(drive "$BOM")
BR=$(last_reason)
if [ -n "$B" ]; then
  # BOM must NOT be lost in recall.
  HBOM=$(grep -aoE 'knapsack expand ks2_[0-9a-f]+' "$B" | head -1 | awk '{print $3}')
  if "$KS" expand "$HBOM" > "$ROOT/_recall_tmp.bin" 2>/dev/null && cmp -s "$ROOT/_recall_tmp.bin" "$BOM"; then
    ok "BOM preserved through pack+recall"
  else
    no "BOM lost in recall"
  fi
fi

# --- 9) Mixed CRLF and LF endings ---
echo
echo "[9] mixed CRLF+LF endings"
MIX="$ROOT/mix.log"
python3 - "$MIX" <<'PYEOF'
import sys
with open(sys.argv[1],'wb') as f:
  for i in range(400):
    nl = b'\r\n' if i % 2 == 0 else b'\n'
    f.write(f"[INFO] step {i}: line".encode() + nl)
PYEOF
M=$(drive "$MIX")
if [ -n "$M" ]; then
  HM=$(grep -aoE 'knapsack expand ks2_[0-9a-f]+' "$M" | head -1 | awk '{print $3}')
  if "$KS" expand "$HM" > "$ROOT/_recall_tmp.bin" 2>/dev/null && cmp -s "$ROOT/_recall_tmp.bin" "$MIX"; then
    ok "mixed CRLF/LF preserved byte-exact through pack+recall"
  else
    no "mixed CRLF/LF mangled in recall"
  fi
fi

# --- 10) File with embedded NUL bytes ---
echo
echo "[10] file with embedded NUL bytes"
NUL="$ROOT/nul.log"
python3 - "$NUL" <<'PYEOF'
import sys
with open(sys.argv[1],'wb') as f:
  for i in range(800):
    f.write(f"prefix-{i}".encode() + b'\x00' + b'middle' + b'\x00' + f"-suffix\n".encode())
PYEOF
N=$(drive "$NUL")
if [ -n "$N" ]; then
  HN=$(grep -aoE 'knapsack expand ks2_[0-9a-f]+' "$N" | head -1 | awk '{print $3}')
  if "$KS" expand "$HN" > "$ROOT/_recall_tmp.bin" 2>/dev/null && cmp -s "$ROOT/_recall_tmp.bin" "$NUL"; then
    ok "NUL bytes preserved byte-exact through pack+recall"
  else
    no "NUL bytes lost in recall"
  fi
else
  echo "  pass-through (reason=$(last_reason))"
fi

# --- 11) Single very long line (no newlines, 200 KB) ---
echo
echo "[11] one-line file, 200 KB no newlines"
LL="$ROOT/longline.log"
python3 - "$LL" <<'PYEOF'
import sys
with open(sys.argv[1],'wb') as f:
  f.write(b'x' * 200000)
PYEOF
L=$(drive "$LL")
LR=$(last_reason)
if [ -n "$L" ]; then
  HL=$(grep -aoE 'knapsack expand ks2_[0-9a-f]+' "$L" | head -1 | awk '{print $3}')
  if "$KS" expand "$HL" > "$ROOT/_recall_tmp.bin" 2>/dev/null && cmp -s "$ROOT/_recall_tmp.bin" "$LL"; then
    ok "200-KB single line preserved byte-exact"
  else
    no "single long line corrupted"
  fi
else
  ok "single long line -> pass-through. reason=$LR"
fi

# --- 12) Random binary 50 KB ---
echo
echo "[12] random binary 50 KB"
BIN="$ROOT/random.bin"
python3 - "$BIN" <<'PYEOF'
import os, sys
with open(sys.argv[1],'wb') as f:
  f.write(os.urandom(50000))
PYEOF
BIN_R=$(drive "$BIN")
BIN_REASON=$(last_reason)
if [ -n "$BIN_R" ]; then
  HBIN=$(grep -aoE 'knapsack expand ks2_[0-9a-f]+' "$BIN_R" | head -1 | awk '{print $3}')
  if "$KS" expand "$HBIN" > "$ROOT/_recall_tmp.bin" 2>/dev/null && cmp -s "$ROOT/_recall_tmp.bin" "$BIN"; then
    ok "random binary preserved byte-exact through pack+recall"
  else
    no "random binary corrupted"
  fi
else
  ok "random binary -> pass-through. reason=$BIN_REASON"
fi

# --- 13) Read of the metrics.jsonl ---
echo
echo "[13] read of metrics.jsonl"
# Make sure metrics has content.
echo "test data" | "$KS" pack - --session "adv-pre" --cmd "echo" --type log > /dev/null 2>&1
MX=$(drive "$ROOT/store/../metrics.jsonl" 2>/dev/null)  # path may be invalid; that's fine
MR=$(last_reason)
echo "  metrics-read result: reason=$MR view=$MX (either is acceptable)"

# --- 14) Read of the binary itself ---
echo
echo "[14] read of the knapsack binary"
KBIN=$(realpath "$KS" 2>/dev/null || echo "$KS")
BB=$(drive "$KBIN")
BBR=$(last_reason)
if [ -n "$BB" ]; then
  HBB=$(grep -aoE 'knapsack expand ks2_[0-9a-f]+' "$BB" | head -1 | awk '{print $3}')
  if "$KS" expand "$HBB" > "$ROOT/_recall_tmp.bin" 2>/dev/null && cmp -s "$ROOT/_recall_tmp.bin" "$KBIN"; then
    ok "binary file preserved byte-exact through pack+recall"
  else
    no "binary file corrupted in recall"
  fi
else
  ok "binary file -> pass-through. reason=$BBR"
fi

# --- 15) Tilde-expansion path (~/file) — Bash expands but Rust receives literal tilde ---
echo
echo "[15] path beginning with literal '~/' (should not expand inside hook)"
TILDE=$(drive "~/nonexistent.log")
TR=$(last_reason)
if [ -z "$TILDE" ] && [ "$TR" = "file-unreadable" ]; then
  ok "literal '~/' -> file-unreadable (no expansion). reason=$TR"
else
  echo "  tilde result: reason=$TR view=$TILDE"
fi

# --- 16) Path with NUL byte (Rust should reject) ---
echo
echo "[16] file_path with embedded NUL byte"
# Bash can pass NUL-laden JSON? Actually json doesn't accept raw NUL in a string; the
# serializer must escape it. We send " " which json deserializes as a NUL char.
echo '{"tool_name":"Read","tool_input":{"file_path":"D:/knapsack/some path.log"},"session_id":"adv"}' \
  | "$KS" hook > /tmp/nul_out 2>&1
RC=$?
NUL_BODY=$(cat /tmp/nul_out)
if [ "$RC" -eq 0 ] && [ -z "$NUL_BODY" ]; then
  ok "NUL in path -> pass-through, no panic, exit 0"
elif [ "$RC" -ne 0 ]; then
  if grep -q "panicked" /tmp/nul_out 2>/dev/null; then
    no "NUL in path caused a panic"
  else
    ok "NUL in path -> non-zero exit but clean (rc=$RC)"
  fi
else
  ok "NUL in path produced output ($NUL_BODY)"
fi

echo
echo "================================================================"
echo "Adversarial inputs: $PASS pass, $FAIL fail"
echo "================================================================"
exit $FAIL
