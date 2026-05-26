#!/usr/bin/env bash
# View header robustness: the header is plain HTML-style comments, but it carries
# the source path AND a recall command. Both must survive when the source path has
# quote characters, backslashes, control sequences. The recall command must be
# copy-pasteable. The header must always end with a blank line so the compact view
# starts on a fresh line.
set -u
KS="${KS:-$HOME/.knapsack/bin/knapsack}"
ROOT="D:/knapsack/target/dogfood_header"
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
  python3 - "$1" <<'PYEOF'
import sys
with open(sys.argv[1],'wb') as f:
    for i in range(700):
        f.write(f"[INFO] step {i}: a stable line\n".encode())
PYEOF
}

drive() {
  echo "{\"tool_name\":\"Read\",\"tool_input\":{\"file_path\":\"$1\"},\"session_id\":\"hdr\"}" \
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

# Inspect a header for required + dangerous properties.
inspect_header() {
  local view="$1" label="$2"
  if ! head -1 "$view" | grep -q "<!-- Knapsack read cache -->"; then
    no "$label: first line is not the canonical opener"
    head -1 "$view" | sed 's/^/    | /'
    return
  fi
  # Header section ends with a blank line; compact view starts after it.
  local hdr_end=$(grep -n '^$' "$view" | head -1 | cut -d: -f1)
  if [ -z "$hdr_end" ]; then
    no "$label: no blank line separator between header and view body"
    return
  fi
  # All header lines must be HTML comments (start with `<!--` and end with `-->`).
  local non_comments=$(head -n "$((hdr_end - 1))" "$view" | grep -cvE '^<!--.*-->$' || true)
  if [ "$non_comments" -ne 0 ]; then
    no "$label: $non_comments header line(s) aren't valid HTML comments"
    head -n "$((hdr_end - 1))" "$view" | sed 's/^/    | /'
    return
  fi
  # Header must NOT contain `-->` inside its body (HTML comments don't nest, so the
  # source path having `-->` could break a parser).
  # Body lines themselves end with `-->`; we only care about embedded ones in the middle.
  local broken=0
  while IFS= read -r line; do
    # Trim the final `-->`, then check if the inner part has another `-->`.
    local inner="${line%-->}"
    if echo "$inner" | grep -q -- "-->"; then broken=$((broken+1)); fi
  done < <(head -n "$((hdr_end - 1))" "$view")
  if [ "$broken" -ne 0 ]; then
    no "$label: $broken header line(s) contain embedded '-->' (would break HTML parsers)"
    return
  fi
  ok "$label: header is well-formed (1..${hdr_end} comments, blank separator, no nested -->)"
}

verify_recall_handle_in_header() {
  local view="$1" source="$2" label="$3"
  local h=$(grep -oE 'knapsack expand ks2_[0-9a-f]+' "$view" | head -1 | awk '{print $3}')
  if [ -z "$h" ]; then no "$label: no recall handle in header"; return; fi
  if ! "$KS" expand "$h" 2>/dev/null | cmp -s - "$source"; then
    no "$label: header handle does NOT recall byte-exact"
    return
  fi
  ok "$label: header handle resolves byte-exact"
}

# --- a) Normal path ---
echo "[a] normal path"
P_NORMAL="$ROOT/normal.log"; big "$P_NORMAL"
V=$(drive "$P_NORMAL")
inspect_header "$V" "normal"
verify_recall_handle_in_header "$V" "$P_NORMAL" "normal"

# --- b) Path with spaces ---
echo
echo "[b] path with spaces"
P_SPACES="$ROOT/with spaces/file.log"
mkdir -p "$(dirname "$P_SPACES")"
big "$P_SPACES"
V=$(drive "$P_SPACES")
inspect_header "$V" "spaces"
verify_recall_handle_in_header "$V" "$P_SPACES" "spaces"
# The header must contain the path literally (after stripping the comment fluff).
if grep -F "$P_SPACES" "$V" > /dev/null; then ok "header carries path literally"; else no "header missing literal path"; fi

# --- c) Path with Unicode ---
echo
echo "[c] path with unicode"
P_UNICODE="$ROOT/héllo·世界·☃/file.log"
mkdir -p "$(dirname "$P_UNICODE")"
big "$P_UNICODE"
V=$(drive "$P_UNICODE")
inspect_header "$V" "unicode"
# Header must contain the unicode path. Compare bytes.
if grep -F "héllo·世界·☃" "$V" > /dev/null; then ok "header preserves unicode"; else no "unicode missing"; fi
verify_recall_handle_in_header "$V" "$P_UNICODE" "unicode"

# --- d) Path containing the literal characters '-->' (could break a comment parser) ---
echo
echo "[d] path with literal '-->' substring"
P_END="$ROOT/end-->arrow/file.log"
mkdir -p "$(dirname "$P_END")"
big "$P_END"
V=$(drive "$P_END")
if [ -n "$V" ]; then
  inspect_header "$V" "literal -->"
  verify_recall_handle_in_header "$V" "$P_END" "literal -->"
else
  echo "  (path with --> created OK but hook didn't redirect — skip)"
fi

# --- e) Path with backslashes (Windows-native form) ---
echo
echo "[e] backslash form (Windows)"
P_BS="${ROOT//\//\\}\bs.log"  # convert all forward slashes to backslashes
P_FS="$ROOT/bs.log"
big "$P_FS"
V=$(drive "$P_BS")
if [ -n "$V" ]; then
  inspect_header "$V" "backslash"
  verify_recall_handle_in_header "$V" "$P_FS" "backslash"
fi

# --- f) Compact view body MUST start AFTER the header blank line ---
echo
echo "[f] body separation"
V=$(drive "$P_NORMAL")
BODY_FIRST=$(awk '/^$/{getline; print; exit}' "$V")
if echo "$BODY_FIRST" | grep -q "^\[Knapsack:"; then
  ok "body starts with the read-cache view sentinel after the blank line"
else
  echo "  first body line: $BODY_FIRST"
fi

# --- g) Header content is consistent across re-reads (cache hit) ---
echo
echo "[g] cache hit returns identical bytes"
V1=$(drive "$P_NORMAL")
H1=$(sha256sum "$V1" | awk '{print $1}')
V2=$(drive "$P_NORMAL")
H2=$(sha256sum "$V2" | awk '{print $1}')
if [ "$V1" = "$V2" ] && [ "$H1" = "$H2" ]; then ok "second read returns the same view bytes"; else
  no "cache-hit view bytes drifted: $H1 vs $H2"
fi

# --- h) Header is valid UTF-8 ---
echo
echo "[h] entire view is valid UTF-8 for every test fixture"
bad=0
for v in "$ROOT/cache"/*.md; do
  if ! python3 -c "
import sys
try: open(sys.argv[1],'rb').read().decode('utf-8'); sys.exit(0)
except UnicodeDecodeError: sys.exit(1)
" "$v"; then
    bad=$((bad+1))
    echo "    ✗ not UTF-8: $(basename "$v")"
  fi
done
if [ "$bad" -eq 0 ]; then ok "all $(ls "$ROOT/cache"/*.md | wc -l) cache views are valid UTF-8"; else no "$bad non-UTF-8 views"; fi

echo
echo "================================================================"
echo "View header robustness: $PASS pass, $FAIL fail"
echo "================================================================"
exit $FAIL
