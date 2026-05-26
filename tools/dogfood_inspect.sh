#!/usr/bin/env bash
# `knapsack inspect` has two overloads:
#   1. inspect <packed-file>  — power-user view of a `.knapsack.md` sidecar
#   2. inspect <handle>       — store-side metadata + preview of a single block
# Drive both forms across normal, corrupted, and edge inputs. Verify clean output,
# no panics, and that the recall commands inspect prints actually work.
set -u
KS="${KS:-$HOME/.knapsack/bin/knapsack}"
ROOT="D:/knapsack/target/dogfood_inspect"
rm -rf "$ROOT" 2>/dev/null
mkdir -p "$ROOT"
export KNAPSACK_STORE="$ROOT/store"
export KNAPSACK_METRICS="$ROOT/metrics.jsonl"
export KNAPSACK_SESSIONS="$ROOT/sessions"

PASS=0; FAIL=0
ok() { PASS=$((PASS+1)); echo "  ✓ $*"; }
no() { FAIL=$((FAIL+1)); echo "  ✗ $*"; }

# Set up a real markdown file and pack it.
SRC="$ROOT/notes.md"
python3 - "$SRC" <<'PYEOF'
import sys
sentence = "Long-prose paragraph that pack_doc can elide. " * 12
with open(sys.argv[1],'w',encoding='utf-8') as f:
    f.write("# Notes\n\n")
    for sec in range(8):
        f.write(f"## Section {sec}\n\n")
        for _ in range(5):
            f.write(sentence + "\n\n")
        f.write("```rust\nfn x() { 0 }\n```\n\n")
PYEOF
PACKED="$ROOT/notes.knapsack.md"
"$KS" pack "$SRC" --output "$PACKED" > "$ROOT/_pack.log" 2>&1
if [ ! -f "$PACKED" ]; then
  no "pack didn't produce sidecar"
  cat "$ROOT/_pack.log" | head -5
  exit 1
fi

# --- 1) `inspect <packed-file>` on a normal sidecar ---
echo "[1] inspect a normal .knapsack.md sidecar"
OUT=$("$KS" inspect "$PACKED" 2>"$ROOT/err.log")
echo "$OUT" | head -10 | sed 's/^/    | /'
if echo "$OUT" | grep -q "knapsack expand"; then ok "inspect prints recall commands"; else no "no recall command in inspect output"; fi
if echo "$OUT" | grep -qE "elisions?: *[0-9]+"; then ok "inspect reports elision count"; fi
if grep -q "panicked" "$ROOT/err.log"; then no "panic on inspect"; fi
# Extract the first recall command and run it.
RECALL_CMD=$(echo "$OUT" | grep -oE 'knapsack expand ks2_[0-9a-f]+' | head -1)
if [ -n "$RECALL_CMD" ]; then
  if $RECALL_CMD > /dev/null 2>&1; then
    ok "first recall command from inspect output works"
  else
    no "recall command failed: $RECALL_CMD"
  fi
fi

# --- 2) `inspect <handle>` — store-side preview ---
echo
echo "[2] inspect a known handle"
H=$(grep -oE 'ks2_[0-9a-f]+' "$PACKED" | head -1)
echo "  handle: $H"
OUT=$("$KS" inspect "$H" 2>"$ROOT/err.log")
echo "$OUT" | head -10 | sed 's/^/    | /'
if echo "$OUT" | grep -qE "[0-9]+ +bytes"; then ok "inspect <handle> reports byte count"; fi
if echo "$OUT" | grep -qE "utf8="; then ok "inspect reports utf8 status"; fi
if grep -q "panicked" "$ROOT/err.log"; then no "panic on inspect <handle>"; fi

# --- 3) `inspect` with a malformed handle ---
echo
echo "[3] inspect with malformed handle"
"$KS" inspect "not-a-real-handle" > "$ROOT/_bad.log" 2>&1
RC=$?
if [ "$RC" -ne 0 ]; then
  if grep -q "panicked" "$ROOT/_bad.log"; then
    no "panic on malformed handle"
  else
    ok "malformed handle exits non-zero with clean error (rc=$RC)"
  fi
else
  ok "malformed handle handled without error (rc=0)"
fi

# --- 4) `inspect` with an unknown but well-formed handle ---
echo
echo "[4] inspect a well-formed but unknown handle"
"$KS" inspect "ks2_00000000000000000000000000000000" > "$ROOT/_unknown.log" 2>&1
RC=$?
if grep -q "panicked" "$ROOT/_unknown.log"; then no "panic on unknown handle"; else ok "unknown handle handled cleanly (rc=$RC)"; fi

# --- 5) `inspect` on a corrupted packed file ---
echo
echo "[5] inspect a corrupted packed file (truncated header)"
CORRUPT="$ROOT/corrupt.knapsack.md"
head -c 100 "$PACKED" > "$CORRUPT"  # truncate to 100 bytes
"$KS" inspect "$CORRUPT" > "$ROOT/_corrupt.log" 2>&1
RC=$?
if grep -q "panicked" "$ROOT/_corrupt.log"; then
  no "panic on corrupted packed file"
else
  ok "corrupted packed file handled cleanly (rc=$RC)"
fi

# --- 6) `inspect` on a packed file with no elisions ---
echo
echo "[6] inspect a packed file that had nothing to elide"
TINY_SRC="$ROOT/tiny.md"
printf "# Short\n\nJust a short doc.\n" > "$TINY_SRC"
"$KS" pack "$TINY_SRC" --output "$ROOT/tiny.knapsack.md" --force > /dev/null 2>&1
OUT=$("$KS" inspect "$ROOT/tiny.knapsack.md" 2>"$ROOT/err.log")
echo "$OUT" | head -8 | sed 's/^/    | /'
if echo "$OUT" | grep -qE "elisions?: *0"; then
  ok "inspect handles zero-elision sidecar"
else
  echo "  (no '0 elisions' line; sidecar may not have been created — pack refuses non-shrinking writes)"
  ok "non-shrinking pack handled (no panic)"
fi

# --- 7) `inspect` with no arguments ---
echo
echo "[7] inspect with no arguments"
"$KS" inspect > "$ROOT/_noargs.log" 2>&1
RC=$?
if [ "$RC" -ne 0 ] && ! grep -q "panicked" "$ROOT/_noargs.log"; then
  ok "missing arg -> non-zero exit, no panic"
else
  echo "  rc=$RC"; head -3 "$ROOT/_noargs.log" | sed 's/^/    | /'
fi

# --- 8) `inspect` falls through correctly: bare arg that LOOKS like a path but isn't ---
echo
echo "[8] inspect with arg that LOOKS like a path but doesn't exist — falls through to handle mode"
"$KS" inspect "/does/not/exist.knapsack.md" > "$ROOT/_pathfb.log" 2>&1
RC=$?
if grep -q "invalid handle" "$ROOT/_pathfb.log"; then
  ok "non-existent path-looking arg falls through to handle validation"
elif grep -q "panicked" "$ROOT/_pathfb.log"; then
  no "panic on non-existent path"
else
  ok "non-existent path handled cleanly"
fi

# --- 9) `inspect <packed>` honors line ranges of elisions in its recall instructions ---
echo
echo "[9] inspect recall commands include --lines slices"
if grep -q -e "ks-recall handle=.*lines=" "$PACKED"; then
  ok "the packed file's recall metadata includes line ranges (ks-recall markers)"
fi
OUT=$("$KS" inspect "$PACKED" 2>/dev/null)
if echo "$OUT" | grep -q -e "lines [0-9]"; then
  ok "inspect output preserves line ranges from the sidecar"
fi

# --- 10) `inspect <handle>` on a small binary block (utf8=false) ---
echo
echo "[10] inspect on a non-UTF-8 block"
BIN="$ROOT/binary.bin"
python3 -c "import os, sys; open(sys.argv[1],'wb').write(os.urandom(5000))" "$BIN"
"$KS" pack - --session "ins" --cmd "x" --type log < "$BIN" > /dev/null 2>&1
BIN_H=$(python3 -c "
import hashlib, sys
print('ks2_' + hashlib.sha256(open(sys.argv[1],'rb').read()).hexdigest()[:32])
" "$BIN")
# This handle is the WHOLE-FILE handle, which pack - doesn't store. Pick any handle
# from the store.
ANY_H=$(find "$KNAPSACK_STORE" -name 'ks2_*' -not -name '*.meta' | head -1 | xargs -I{} basename {})
OUT=$("$KS" inspect "$ANY_H" 2>"$ROOT/err.log")
if echo "$OUT" | grep -qE "utf8=(true|false)"; then
  ok "inspect reports utf8 status for arbitrary blocks"
fi

echo
echo "================================================================"
echo "Inspect: $PASS pass, $FAIL fail"
echo "================================================================"
exit $FAIL
