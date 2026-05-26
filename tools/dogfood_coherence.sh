#!/usr/bin/env bash
# Cache coherence under realistic editing patterns. The cache is content-addressed
# (SHA-256 of source bytes), so:
#  - Same content at two different paths -> same cache file (dedup)
#  - Same path edited -> different cache file per content (no false hit)
#  - A trivial edit (e.g. one byte added) -> different cache file (digest changes)
#  - Cache file name is the SHA-256 prefix of source bytes
set -u
KS="${KS:-$HOME/.knapsack/bin/knapsack}"
ROOT="D:/knapsack/target/dogfood_coh"
rm -rf "$ROOT" 2>/dev/null
mkdir -p "$ROOT/cache" "$ROOT/store"
export KNAPSACK_STORE="$ROOT/store"
export KNAPSACK_READ_LOG="$ROOT/read_hook.jsonl"
export KNAPSACK_READ_CACHE="$ROOT/cache"
: > "$KNAPSACK_READ_LOG"

PASS=0; FAIL=0
ok() { PASS=$((PASS+1)); echo "  ✓ $*"; }
no() { FAIL=$((FAIL+1)); echo "  ✗ $*"; }

big() {
  python3 - "$1" "$2" <<'PYEOF'
import sys
with open(sys.argv[1],'wb') as f:
    for i in range(int(sys.argv[2])):
        f.write(f"[INFO] step {i}: routine work\n".encode())
PYEOF
}

drive() {
  echo "{\"tool_name\":\"Read\",\"tool_input\":{\"file_path\":\"$1\"},\"session_id\":\"coh\"}" \
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

# --- 1) Same bytes at two paths -> DISTINCT cache files, each with its own header ---
# The cache filename embeds both the content digest AND a path tag so two paths that
# happen to hold the same bytes (a copy, identical lock files across workspaces) get
# their own cache view with the correct "Original file: <path>" header. The shared
# store still dedupes the actual byte storage to one entry per unique content.
echo "[1] same content at two paths -> distinct cache files, each header points at its own path"
A="$ROOT/a/file.log"
B="$ROOT/b/file.log"
mkdir -p "$(dirname "$A")" "$(dirname "$B")"
big "$A" 500
cp "$A" "$B"
VA=$(drive "$A")
VB=$(drive "$B")
# Both cache filenames must share the same content-digest PREFIX (32 hex), but differ
# in the path-tag suffix.
PREFIX_A=$(basename "$VA" | cut -d_ -f1)
PREFIX_B=$(basename "$VB" | cut -d_ -f1)
if [ "$PREFIX_A" = "$PREFIX_B" ] && [ "$(basename "$VA")" != "$(basename "$VB")" ]; then
  ok "same content -> same digest prefix ($PREFIX_A), different path tags"
else
  no "expected shared prefix + different suffix; got A=$(basename "$VA") B=$(basename "$VB")"
fi
# Each header should name ITS OWN path.
H_A=$(grep -E "Original file:" "$VA" | sed 's/.*Original file: //;s/ -->//;s/[[:space:]]*$//')
H_B=$(grep -E "Original file:" "$VB" | sed 's/.*Original file: //;s/ -->//;s/[[:space:]]*$//')
if [ "$H_A" = "$A" ]; then ok "view A header names path A: $H_A"; else no "view A header: got '$H_A' expected '$A'"; fi
if [ "$H_B" = "$B" ]; then ok "view B header names path B: $H_B"; else no "view B header: got '$H_B' expected '$B'"; fi
# Store dedup: only one whole-file handle present in the store for both paths.
HANDLE_A=$(grep -oE 'knapsack expand ks2_[0-9a-f]+' "$VA" | head -1 | awk '{print $3}')
HANDLE_B=$(grep -oE 'knapsack expand ks2_[0-9a-f]+' "$VB" | head -1 | awk '{print $3}')
if [ "$HANDLE_A" = "$HANDLE_B" ]; then
  ok "both views advertise the SAME store handle (content-addressed dedup): $HANDLE_A"
else
  no "handles differ: A=$HANDLE_A B=$HANDLE_B"
fi

# --- 2) Same path, two different contents -> different cache files ---
echo
echo "[2] same path, two different contents -> different cache files"
C="$ROOT/c.log"
big "$C" 500
VC1=$(drive "$C")
big "$C" 600
VC2=$(drive "$C")
if [ "$(basename "$VC1")" != "$(basename "$VC2")" ]; then
  ok "different content -> different cache: $(basename "$VC1") vs $(basename "$VC2")"
else
  no "same cache file for different content"
fi

# --- 3) Trivial edit (append one byte) -> new cache file ---
echo
echo "[3] trivial edit -> different cache file"
D="$ROOT/d.log"
big "$D" 500
VD1=$(drive "$D")
printf "x" >> "$D"
VD2=$(drive "$D")
if [ "$(basename "$VD1")" != "$(basename "$VD2")" ]; then
  ok "one-byte edit -> different cache: $(basename "$VD1") vs $(basename "$VD2")"
else
  no "one-byte edit didn't change cache"
fi

# --- 4) Cache file name embeds sha256(source)[:32] + sha1(path)[:8] ---
echo
echo "[4] cache filename embeds sha256(source)[:32] and sha1(path)[:8]"
E="$ROOT/e.log"
big "$E" 700
VE=$(drive "$E")
ACTUAL=$(basename "$VE")
EXPECTED_PREFIX=$(python3 -c "
import hashlib, sys
data = open(sys.argv[1],'rb').read()
print(hashlib.sha256(data).hexdigest()[:32])
" "$E")
# Filename layout: <32-hex>_<8-hex>.md
if echo "$ACTUAL" | grep -qE "^${EXPECTED_PREFIX}_[0-9a-f]{8}\.md$"; then
  ok "cache filename matches expected layout: $ACTUAL"
else
  no "filename layout mismatch: $ACTUAL (expected ${EXPECTED_PREFIX}_<8hex>.md)"
fi

# --- 5) Editing within same digest length boundary -> still different cache ---
echo
echo "[5] in-place edit preserving exact length -> still different cache (digest changes)"
F="$ROOT/f.log"
big "$F" 500
VF1=$(drive "$F")
# Swap a few bytes in place WITHOUT changing length.
python3 - "$F" <<'PYEOF'
import sys
p = sys.argv[1]
b = bytearray(open(p,'rb').read())
b[100:104] = b'EDIT'  # 4 bytes -> 4 bytes (no length change)
open(p,'wb').write(b)
PYEOF
VF2=$(drive "$F")
if [ "$(basename "$VF1")" != "$(basename "$VF2")" ]; then
  ok "in-place 4-byte edit -> different cache"
else
  no "in-place edit didn't change cache"
fi

# --- 6) Reading the same path twice with NO changes -> cache-hit (same path returned) ---
echo
echo "[6] re-read of unchanged file -> cache hit (same path, no regeneration)"
G="$ROOT/g.log"
big "$G" 800
VG1=$(drive "$G")
MT1=$(python3 -c "import os,sys; print(os.path.getmtime(sys.argv[1]))" "$VG1")
sleep 0.5
VG2=$(drive "$G")
MT2=$(python3 -c "import os,sys; print(os.path.getmtime(sys.argv[1]))" "$VG2")
if [ "$VG1" = "$VG2" ]; then
  ok "second read returns same cache path"
else
  no "re-read returned different cache path"
fi
# Check the why-last note
LAST=$(tail -n 1 "$KNAPSACK_READ_LOG" | python3 -c "import json,sys; print(json.loads(sys.stdin.read()).get('note',''))")
if [ "$LAST" = "cache-hit" ]; then
  ok "why-last notes 'cache-hit'"
else
  echo "  (why-last note: $LAST)"
fi

# --- 7) Multiple unrelated files -> multiple cache files, each correct ---
echo
echo "[7] 10 unrelated files -> 10 distinct cache files, each recalls byte-exact"
mkdir -p "$ROOT/many"
for i in $(seq 1 10); do
  P="$ROOT/many/file-$i.log"
  big "$P" $((400 + i * 30))
done
declare -A SEEN
mismatches=0
for f in "$ROOT/many"/file-*.log; do
  v=$(drive "$f")
  cf=$(basename "$v")
  # Whole-file handle should recall byte-exact.
  H=$(grep -oE 'knapsack expand ks2_[0-9a-f]+' "$v" | head -1 | awk '{print $3}')
  if ! "$KS" expand "$H" 2>/dev/null | cmp -s - "$f"; then
    mismatches=$((mismatches+1))
  fi
  SEEN[$cf]=1
done
distinct=${#SEEN[@]}
if [ "$distinct" -eq 10 ] && [ "$mismatches" -eq 0 ]; then
  ok "10 distinct cache files, all 10 recall byte-exact"
else
  no "distinct=$distinct mismatches=$mismatches"
fi

echo
echo "================================================================"
echo "Cache coherence: $PASS pass, $FAIL fail"
echo "================================================================"
exit $FAIL
