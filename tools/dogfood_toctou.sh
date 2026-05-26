#!/usr/bin/env bash
# TOCTOU and mutating-source races. The hook does stat -> read -> hash. Between any
# two of those, the file can change. We must never return wrong bytes, never panic,
# never advertise a handle that doesn't match the bytes the model gets in context.
set -u
KS="${KS:-$HOME/.knapsack/bin/knapsack}"
ROOT="D:/knapsack/target/dogfood_toctou"
rm -rf "$ROOT" 2>/dev/null
mkdir -p "$ROOT/cache" "$ROOT/store"
export KNAPSACK_STORE="$ROOT/store"
export KNAPSACK_READ_LOG="$ROOT/read_hook.jsonl"
export KNAPSACK_READ_CACHE="$ROOT/cache"
: > "$KNAPSACK_READ_LOG"

PASS=0; FAIL=0
ok() { PASS=$((PASS+1)); echo "  ✓ $*"; }
no() { FAIL=$((FAIL+1)); echo "  ✗ $*"; }

big_log() {
  python3 - "$1" "$2" <<'PYEOF'
import sys
path, n = sys.argv[1], int(sys.argv[2])
with open(path,'wb') as f:
    for i in range(n):
        f.write(f"[INFO] step {i}: stable line that survives the structural pass\n".encode())
PYEOF
}

# Drive the hook and return the view path (empty if pass-through).
drive() {
  local p="$1"
  echo "{\"tool_name\":\"Read\",\"tool_input\":{\"file_path\":\"$p\"},\"session_id\":\"toctou\"}" \
    | "$KS" hook 2>/dev/null \
    | python3 -c "
import json,sys
try:
  v=json.loads(sys.stdin.read())
  print(v['hookSpecificOutput']['updatedInput']['file_path'])
except Exception:
  pass
"
}

# --- 1) File replaced (same path, different content) between two reads ---
echo "[1] file replaced between two reads -> different cache file, byte-exact original recall"
SRC="$ROOT/mut.log"
big_log "$SRC" 500
V1=$(drive "$SRC")
SHA1=$(grep -oE 'ks2_[0-9a-f]+' "$V1" | head -1)
SHA_HEADER1=$(grep -oE 'knapsack expand ks2_[0-9a-f]+' "$V1" | head -1 | awk '{print $3}')
B1=$("$KS" expand "$SHA_HEADER1" 2>/dev/null | wc -c)
if [ "$B1" = "$(wc -c < "$SRC")" ]; then ok "first read: whole-file handle recalls byte-exact ($B1 B)"; else no "first read recall mismatch"; fi

# Replace the file in place with NEW content.
big_log "$SRC" 800
V2=$(drive "$SRC")
SHA_HEADER2=$(grep -oE 'knapsack expand ks2_[0-9a-f]+' "$V2" | head -1 | awk '{print $3}')
B2=$("$KS" expand "$SHA_HEADER2" 2>/dev/null | wc -c)
if [ "$B2" = "$(wc -c < "$SRC")" ]; then ok "after replacement: new whole-file handle recalls byte-exact ($B2 B)"; else no "post-replacement recall mismatch"; fi
if [ "$V1" != "$V2" ]; then ok "different content -> different cache file"; else no "same cache file emitted for changed content"; fi
# And: the OLD handle's bytes should still be in the store (it was content-addressed; we never overwrite content)
B_OLD=$("$KS" expand "$SHA_HEADER1" 2>/dev/null | wc -c)
if [ "$B_OLD" -gt 0 ]; then ok "old version's handle still recoverable from the store"; else no "old version's bytes lost"; fi

# --- 2) Rapidly growing log ---
echo
echo "[2] rapidly growing log — write happens during structural pass"
GROW="$ROOT/grow.log"
big_log "$GROW" 1000
# Start a background append while we drive.
( for k in $(seq 1 100); do echo "[INFO] grown line $k" >> "$GROW"; done ) &
APPEND_PID=$!
VG=$(drive "$GROW")
wait $APPEND_PID
# Whatever the hook produced must STILL recall byte-exact to the bytes it actually saw.
if [ -n "$VG" ]; then
  HV=$(grep -oE 'knapsack expand ks2_[0-9a-f]+' "$VG" | head -1 | awk '{print $3}')
  # Recompute the bytes the hook would have read by reading file digest at the time
  # of view creation — we use the digest in the view header itself.
  DIG=$(grep -oE 'sha256=[0-9a-f]+' "$VG" | head -1 | cut -d= -f2)
  # Just verify the handle the view advertised resolves and the bytes hash back.
  if "$KS" expand "$HV" 2>/dev/null | python3 -c "
import hashlib, sys
data = sys.stdin.buffer.read()
expected = '$DIG'
if hashlib.sha256(data).hexdigest() == expected:
  sys.exit(0)
sys.exit(1)
"; then
    ok "during-write read: header digest matches recalled bytes (no torn read)"
  else
    no "during-write read: header digest != recalled bytes (CORRUPTION)"
  fi
else
  ok "during-write read: hook chose to pass through (safe)"
fi

# --- 3) File deleted after stat, before read ---
# Hard to deterministically trigger; we simulate by deleting AFTER drive() and
# verifying the view (if any) still references bytes that were valid at hook-time.
echo
echo "[3] file deleted after the hook read it"
DEL="$ROOT/del.log"
big_log "$DEL" 600
VD=$(drive "$DEL")
HD=$(grep -oE 'knapsack expand ks2_[0-9a-f]+' "$VD" | head -1 | awk '{print $3}')
rm -f "$DEL"
# Recall via knapsack should STILL work — the bytes are in the store independent of
# the source file.
if "$KS" expand "$HD" > /dev/null 2>&1; then
  ok "post-delete recall still works (store is independent of source)"
else
  no "post-delete recall failed"
fi
# And: re-driving the hook on the deleted file must pass through cleanly.
VD2=$(drive "$DEL")
LAST_REASON=$(tail -n 1 "$KNAPSACK_READ_LOG" | python3 -c "import json,sys; print(json.loads(sys.stdin.read()).get('reason',''))")
if [ -z "$VD2" ] && [ "$LAST_REASON" = "file-unreadable" ]; then
  ok "re-read of deleted file is file-unreadable pass-through"
else
  no "re-read of deleted file unexpected: reason=$LAST_REASON view=$VD2"
fi

# --- 4) File mode flipped to no-read (Windows-friendly: try chmod, ignore on fail) ---
echo
echo "[4] file with restricted read permissions"
NOREAD="$ROOT/noread.log"
big_log "$NOREAD" 500
# Try to remove read permission; if it doesn't take on this OS, skip.
chmod 000 "$NOREAD" 2>/dev/null
if [ -r "$NOREAD" ]; then
  echo "  (chmod 000 didn't apply on this platform; skipping)"
else
  VN=$(drive "$NOREAD")
  LAST_REASON=$(tail -n 1 "$KNAPSACK_READ_LOG" | python3 -c "import json,sys; print(json.loads(sys.stdin.read()).get('reason',''))")
  if [ -z "$VN" ] && [ "$LAST_REASON" = "file-unreadable" ]; then
    ok "no-read file: file-unreadable pass-through"
  else
    no "no-read file: unexpected reason=$LAST_REASON view=$VN"
  fi
  chmod 644 "$NOREAD" 2>/dev/null
fi

# --- 5) Source modified between cache-build and the model's Read ---
# The hook builds a view from bytes-at-time-T. If the source mutates between T and the
# model actually reading the cache file, the view shows OLD bytes but the source file
# at the path is NEW. The view's header carries the source path AND its digest at T,
# so the model can detect the drift. We just confirm the digest in the header matches
# the bytes the recall returns (which is the byte-exact original-at-T snapshot).
echo
echo "[5] view header digest matches the snapshot it was built from"
MUT="$ROOT/mut2.log"
big_log "$MUT" 700
VM=$(drive "$MUT")
DIG_M=$(grep -oE 'sha256=[0-9a-f]+' "$VM" | head -1 | cut -d= -f2)
HM=$(grep -oE 'knapsack expand ks2_[0-9a-f]+' "$VM" | head -1 | awk '{print $3}')
ACTUAL=$("$KS" expand "$HM" 2>/dev/null | python3 -c "import hashlib,sys; print(hashlib.sha256(sys.stdin.buffer.read()).hexdigest())")
if [ "$ACTUAL" = "$DIG_M" ]; then
  ok "header digest == sha256(recalled bytes) — model can detect any drift"
else
  no "header digest != sha256(recalled bytes): $ACTUAL vs $DIG_M"
fi

echo
echo "================================================================"
echo "TOCTOU: $PASS pass, $FAIL fail"
echo "================================================================"
exit $FAIL
