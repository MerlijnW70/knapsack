#!/usr/bin/env bash
# Path / encoding edge cases for the Read hook. Many tools choke on spaces,
# non-ASCII, very long paths, paths with backslashes, paths inside hidden dirs,
# symlinks (where supported). Verify the hook stays fail-open on every weirdness.
set -u
KS="${KS:-$HOME/.knapsack/bin/knapsack}"
ROOT="D:/knapsack/target/dogfood_paths"
rm -rf "$ROOT" 2>/dev/null
mkdir -p "$ROOT/cache"
export KNAPSACK_READ_LOG="$ROOT/read_hook.jsonl"
export KNAPSACK_READ_CACHE="$ROOT/cache"
: > "$KNAPSACK_READ_LOG"

PASS=0; FAIL=0
ok() { PASS=$((PASS+1)); echo "  ✓ $*"; }
no() { FAIL=$((FAIL+1)); echo "  ✗ $*"; }

# Helper: drive the hook with a single PreToolUse Read event, then read the LAST
# log line and assert the reason matches expected (or the call returns no rewrite
# but the log shows the documented reason).
drive() {
  local file_path="$1" expect_redirect="$2" label="$3"
  local envelope
  envelope=$(python3 - "$file_path" <<'PYEOF'
import json,sys
print(json.dumps({"tool_name":"Read","tool_input":{"file_path":sys.argv[1]}}))
PYEOF
)
  local body
  body=$( echo "$envelope" | "$KS" hook 2>&1 )
  local reason
  reason=$(tail -n 1 "$KNAPSACK_READ_LOG" 2>/dev/null | python3 -c "
import json,sys
try: print(json.loads(sys.stdin.read()).get('reason',''))
except: print('')
")
  if [ "$expect_redirect" = "yes" ]; then
    if echo "$body" | grep -q "updatedInput"; then
      ok "$label -> redirect"
    else
      no "$label -> expected redirect, got pass-through (reason=$reason)"
    fi
  else
    if [ -z "$body" ]; then
      ok "$label -> pass-through (reason=$reason)"
    else
      no "$label -> expected pass-through, got rewrite"
    fi
  fi
}

# Synthesize a compressible 50 KB log under each tricky path.
gen() {
  python3 - "$1" <<'PYEOF'
import os, sys
p = sys.argv[1]
os.makedirs(os.path.dirname(p), exist_ok=True)
with open(p,'w', encoding='utf-8') as f:
    for i in range(800):
        f.write(f'[INFO] step {i}: routine work; lots of similar lines for the structural compressor\n')
PYEOF
}

# 1) Spaces in the directory name
gen "$ROOT/dir with spaces/file.log"
drive "$ROOT/dir with spaces/file.log" yes "spaces in dir"

# 2) Spaces in the file name
gen "$ROOT/sub/file with spaces.log"
drive "$ROOT/sub/file with spaces.log" yes "spaces in file"

# 3) Unicode in the file name
gen "$ROOT/unicode/héllo·世界·☃.log"
drive "$ROOT/unicode/héllo·世界·☃.log" yes "Unicode in path"

# 4) Leading dot (hidden dir)
gen "$ROOT/.hidden/file.log"
drive "$ROOT/.hidden/file.log" yes "leading-dot dir"

# 5) Very long file name
gen "$ROOT/long/$(python3 -c 'print("a"*200)').log"
drive "$ROOT/long/$(python3 -c 'print("a"*200)').log" yes "200-char filename"

# 6) Deeply nested path
DEEP="$ROOT"
for _ in {1..10}; do DEEP="$DEEP/depth"; done
gen "$DEEP/file.log"
drive "$DEEP/file.log" yes "10-deep nested"

# 7) Trailing whitespace in the file name (Windows often refuses this — should pass-through)
gen "$ROOT/trailing/normal.log"
drive "$ROOT/trailing/normal.log " no "trailing space in event path"

# 8) Backslash form (Windows native)
NATIVE=$(echo "$ROOT" | sed 's|/|\\|g')
drive "$NATIVE\\sub\\file with spaces.log" yes "Windows backslash form"

# 9) Mixed slashes
drive "$ROOT\\sub/file with spaces.log" yes "mixed slashes"

# 10) Forward-slash relative-style with .. inside (canonical: pass-through if not exist)
drive "$ROOT/sub/../sub/file with spaces.log" yes "path with ../"

# 11) Empty file_path (this is BadInput, hook should pass through)
echo '{"tool_name":"Read","tool_input":{"file_path":""}}' | "$KS" hook > /dev/null 2>&1
reason=$(tail -n 1 "$KNAPSACK_READ_LOG" | python3 -c "import json,sys; print(json.loads(sys.stdin.read()).get('reason',''))")
if [ "$reason" = "bad-input" ]; then ok "empty file_path -> bad-input"; else no "empty file_path -> got $reason"; fi

# 12) file_path is a directory, not a file (should fail-open)
mkdir -p "$ROOT/just_a_dir"
drive "$ROOT/just_a_dir" no "directory path (not a file)"

# 13) Very long file_path (path itself ~ 300+ chars)
LONGPATH="$ROOT/long2/$(python3 -c 'print("seg/" * 30)')file.log"
gen "$LONGPATH"
drive "$LONGPATH" yes "very long total path (~300 chars)"

# 14) Path with control characters (tab)
gen "$ROOT/tabby/has	tab.log"
drive "$ROOT/tabby/has	tab.log" yes "embedded tab in name"

echo
echo "================================================================"
echo "Path/encoding probes: $PASS pass, $FAIL fail"
echo "================================================================"
exit $FAIL
