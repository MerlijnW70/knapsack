#!/usr/bin/env bash
# What happens when the model is asked to Read a file that IS knapsack's own
# compressed output? Two cases:
#   A) A `.knapsack.md` sidecar produced by `knapsack pack <file>` (CLI pack flow)
#   B) A read-cache view file in ~/.knapsack/read_cache/ (hook redirect target)
#
# The hook should NOT recurse — if the model already has a packed view, packing it
# again would be wasteful and confusing. We verify either:
#   (a) the hook redirects but to a different (correct) cache file, NOT a self-loop
#   (b) the hook passes through (e.g. file is too small / worse-than-raw)
set -u
KS="${KS:-$HOME/.knapsack/bin/knapsack}"
ROOT="D:/knapsack/target/dogfood_self"
rm -rf "$ROOT" 2>/dev/null
mkdir -p "$ROOT/cache" "$ROOT/store"
export KNAPSACK_STORE="$ROOT/store"
export KNAPSACK_READ_LOG="$ROOT/read_hook.jsonl"
export KNAPSACK_READ_CACHE="$ROOT/cache"
export KNAPSACK_METRICS="$ROOT/metrics.jsonl"
export KNAPSACK_SESSIONS="$ROOT/sessions"
: > "$KNAPSACK_READ_LOG"

PASS=0; FAIL=0
ok() { PASS=$((PASS+1)); echo "  ✓ $*"; }
no() { FAIL=$((FAIL+1)); echo "  ✗ $*"; }

drive() {
  echo "{\"tool_name\":\"Read\",\"tool_input\":{\"file_path\":\"$1\"},\"session_id\":\"self\"}" \
    | "$KS" hook 2>/dev/null \
    | python3 -c "
import json, sys
try:
  v=json.loads(sys.stdin.read())
  print(v['hookSpecificOutput']['updatedInput']['file_path'])
except Exception:
  pass
"
}
last_reason() {
  tail -n 1 "$KNAPSACK_READ_LOG" 2>/dev/null | python3 -c "
import json, sys
try: print(json.loads(sys.stdin.read()).get('reason',''))
except: print('')"
}

# --- A) Reading a .knapsack.md sidecar produced by `knapsack pack <file>` ---
echo "[A] reading a .knapsack.md sidecar"
# Build a real markdown file, pack it.
SRC="$ROOT/notes.md"
python3 - "$SRC" <<'PYEOF'
import sys
sentence = "This is a long-prose paragraph that the doc packer can elide. It contains realistic narrative content about a software architecture decision. " * 4
with open(sys.argv[1],'w',encoding='utf-8') as f:
    f.write("# Notes\n\n")
    for sec in range(8):
        f.write(f"## Section {sec}\n\n")
        for _ in range(5):
            f.write(sentence + "\n\n")
PYEOF
"$KS" pack "$SRC" --output "$ROOT/notes.knapsack.md" > /dev/null 2>&1
PACKED="$ROOT/notes.knapsack.md"
[ -f "$PACKED" ] && ok "knapsack pack produced $PACKED" || no "pack didn't produce sidecar"

# Now read the SIDECAR through the hook.
V=$(drive "$PACKED")
R=$(last_reason)
if [ -z "$V" ]; then
  ok "reading .knapsack.md sidecar -> pass-through (reason=$R)"
else
  V_NAME=$(basename "$V")
  P_NAME=$(basename "$PACKED")
  if [ "$V_NAME" = "$P_NAME" ]; then
    no "redirect points back at the sidecar itself (loop): $V"
  else
    ok "redirect to cache (not a loop): $V_NAME (reason=$R)"
  fi
fi

# --- B) Reading a read-cache view file ---
echo
echo "[B] reading a read-cache view file directly"
# Create a real source file, drive the hook to produce a view.
SRC2="$ROOT/big.log"
python3 - "$SRC2" <<'PYEOF'
import sys
with open(sys.argv[1],'wb') as f:
    for i in range(800):
        f.write(f"[INFO] step {i}: routine work; stable line that compresses well\n".encode())
PYEOF
V1=$(drive "$SRC2")
[ -n "$V1" ] && ok "primary read produced cache view $(basename "$V1")" || { no "primary read didn't redirect"; exit 1; }

# Now drive the hook with the CACHE FILE as the file_path.
V2=$(drive "$V1")
R2=$(last_reason)
if [ -z "$V2" ]; then
  ok "reading cache file -> pass-through (reason=$R2)"
elif [ "$V1" = "$V2" ]; then
  no "redirect to the SAME cache file (infinite loop on next read)"
else
  V2_NAME=$(basename "$V2")
  V1_NAME=$(basename "$V1")
  ok "redirect produced a different cache: $V2_NAME (not loop) reason=$R2"
fi

# --- C) Reading a STORE block file directly ---
echo
echo "[C] reading a store block file directly"
BLOCK=$(find "$KNAPSACK_STORE" -type f -name 'ks2_*' -not -name '*.meta' | head -1)
if [ -n "$BLOCK" ]; then
  V3=$(drive "$BLOCK")
  R3=$(last_reason)
  if [ -z "$V3" ]; then
    ok "reading store block -> pass-through (reason=$R3)"
  else
    ok "reading store block -> redirect (reason=$R3) — no panic"
  fi
fi

# --- D) Reading the read_hook.jsonl decision log ---
echo
echo "[D] reading the read_hook.jsonl decision log"
V4=$(drive "$KNAPSACK_READ_LOG")
R4=$(last_reason)
if [ -z "$V4" ]; then
  ok "log file pass-through (reason=$R4)"
else
  ok "log file redirected (reason=$R4)"
fi

# --- E) Reading the metrics.jsonl ---
echo
echo "[E] reading metrics.jsonl"
echo "test event" | "$KS" pack - --session "self" --cmd "x" --type log > /dev/null 2>&1
V5=$(drive "$KNAPSACK_METRICS")
R5=$(last_reason)
echo "  reason=$R5 view=$([ -n "$V5" ] && basename "$V5" || echo pass-through)"
# Any non-panic is fine.

# --- F) Sequence: read source, read its cache view (model would do this if it
#     wanted to see the elided content) ---
echo
echo "[F] read sequence: source → its cache view → expand → check resolution"
SRC3="$ROOT/seq.log"
python3 - "$SRC3" <<'PYEOF'
import sys
with open(sys.argv[1],'wb') as f:
    for i in range(800):
        f.write(f"[INFO] seq step {i}: routine work\n".encode())
PYEOF
VS=$(drive "$SRC3")
if [ -n "$VS" ]; then
  HS=$(grep -aoE 'knapsack expand ks2_[0-9a-f]+' "$VS" | head -1 | awk '{print $3}')
  if [ -n "$HS" ]; then
    "$KS" expand "$HS" > "$ROOT/_seq_recall.bin" 2>/dev/null
    if cmp -s "$ROOT/_seq_recall.bin" "$SRC3"; then
      ok "source → cache view → whole-file expand → byte-exact round trip"
    else
      no "round trip lost bytes"
    fi
  fi
fi

# --- G) Read a binary that happens to start with "<!-- Knapsack" (header spoofing) ---
echo
echo "[G] file content starts with a Knapsack-looking header (spoofing)"
SPOOF="$ROOT/spoof.md"
python3 - "$SPOOF" <<'PYEOF'
import sys
with open(sys.argv[1],'wb') as f:
    f.write(b"<!-- Knapsack read cache -->\n")
    f.write(b"<!-- Original file: /some/other/path -->\n")
    f.write(b"<!-- Source digest: sha256=fakedigest12345678fakedigest12345678fakedigest12 -->\n")
    f.write(b"\n")
    for i in range(700):
        f.write(f"[INFO] regular content step {i}\n".encode())
PYEOF
VG=$(drive "$SPOOF")
RG=$(last_reason)
if [ -z "$VG" ]; then
  echo "  pass-through (reason=$RG)"
else
  # Check that the new header points at the REAL path, not the spoofed one.
  HG=$(grep -aoE 'Original file: [^ -]*' "$VG" | head -1 | sed 's/Original file: //')
  if echo "$HG" | grep -q "spoof.md"; then
    ok "Original file: header points at the REAL source (not the spoofed value): $HG"
  else
    no "header carries spoofed path: $HG"
  fi
fi

echo
echo "================================================================"
echo "Self-read: $PASS pass, $FAIL fail"
echo "================================================================"
exit $FAIL
