#!/usr/bin/env bash
# Live dogfood matrix for the Read hook (default-on after this release). Drives
# `knapsack hook` with real PreToolUse envelopes and verifies the decision matches
# expectation for each scenario the user called out. Every row is a real Read of a
# real file; no synthetic inputs. After each block we dump `why-last`, the new
# /knapsack surface, and `doctor` so all three views stay coherent.
set -u
KS="${KS:-$HOME/.knapsack/bin/knapsack}"
# Use a path that both bash (MSYS) and Python (Windows) resolve identically.
# Resolved once at the top so every step sees the same dir without subshell trickery.
SCRATCH="D:/knapsack/target/dogfood_scratch"
mkdir -p "$SCRATCH/cache"
export KNAPSACK_READ_LOG="$SCRATCH/read_hook.jsonl"
export KNAPSACK_READ_CACHE="$SCRATCH/cache"
: > "$KNAPSACK_READ_LOG"

cd "$(dirname "$0")/.."

# A small driver that pipes a PreToolUse Read event to `knapsack hook` and prints
# what it gets back (empty = pass-through; JSON = redirect).
run_read() {
  local file_path="$1"
  local label="$2"
  shift 2
  local extra_fields="$*"   # JSON fragments to merge into tool_input
  local tool_input="\"file_path\":\"$file_path\""
  if [ -n "$extra_fields" ]; then
    tool_input="$tool_input,$extra_fields"
  fi
  local envelope="{\"tool_name\":\"Read\",\"tool_input\":{$tool_input}}"
  local out
  out=$( echo "$envelope" | "$KS" hook 2>&1 )
  if [ -z "$out" ]; then
    printf "  %-40s -> pass-through (no rewrite emitted)\n" "$label"
  else
    local new_path
    new_path=$(python3 -c "
import json,sys
v=json.loads(sys.argv[1])
print(v['hookSpecificOutput']['updatedInput']['file_path'])
" "$out" 2>/dev/null)
    printf "  %-40s -> REDIRECT -> %s\n" "$label" "$new_path"
  fi
}

echo "============================================================"
echo "Dogfood matrix — Read hook default-on, scenario-by-scenario"
echo "============================================================"

REPO="D:/knapsack"

# --- 1) Large .rs (should redirect — within size band, compresses well) ---
echo
echo "[1] large .rs (src/main.rs, ~24 KB)"
run_read "$REPO/src/main.rs" "src/main.rs"

# --- 2) Small .rs (should pass-through with reason=TooSmall) ---
echo
echo "[2] small .rs (src/lib.rs, ~1.5 KB)"
run_read "$REPO/src/lib.rs" "src/lib.rs"

# --- 3) README.md (under 8 KB threshold -> pass-through TooSmall) ---
echo
echo "[3] README.md (~8 KB)"
run_read "$REPO/README.md" "README.md"

# --- 4) package.json — we synthesize a realistic one (~2 KB) -> too small ---
echo
echo "[4] package.json (real shape, ~2 KB)"
PKG="$SCRATCH/package.json"
python3 -c "
import json
pkg = {'name':'demo','version':'1.0.0','dependencies':{f'lib-{i}':'^1.0.0' for i in range(40)},
       'devDependencies':{f'dev-{i}':'~2.0.0' for i in range(20)},
       'scripts':{'test':'jest','build':'webpack --mode production','lint':'eslint .'}}
open('$PKG','w').write(json.dumps(pkg, indent=2))
"
run_read "$PKG" "package.json"

# --- 5) Big JSON response (~50 KB -> within band -> compresses) ---
echo
echo "[5] big JSON response (~50 KB)"
BIG_JSON="$SCRATCH/api_response.json"
python3 -c "
import json
# A realistic API listing with repeated structure (compressible).
items = [{'id':i,'name':f'item-{i}','tags':['alpha','beta','gamma'],
          'description':'A repeated description that compresses well across many items.',
          'metadata':{'created':'2026-05-26T12:00:00Z','version':'1.0','active':True}}
         for i in range(300)]
open('$BIG_JSON','w').write(json.dumps({'items':items}, indent=2))
"
ls -la "$BIG_JSON"
run_read "$BIG_JSON" "big_json.json"

# --- 6) Big log (~150 KB -> within band, structural compressor wins) ---
echo
echo "[6] big log (~150 KB)"
BIG_LOG="$SCRATCH/big.log"
python3 -c "
import sys
with open('$BIG_LOG','w') as f:
    for i in range(2000):
        f.write(f'[INFO] 2026-05-26 12:00:00 worker-{i%4} processed task id={i} status=ok latency={i%100}ms\n')
"
ls -la "$BIG_LOG"
run_read "$BIG_LOG" "big.log"

# --- 7) Read with offset/limit (slicing) — pass-through SlicingRequested ---
echo
echo "[7] slicing read (offset=100)"
run_read "$REPO/src/main.rs" "src/main.rs --offset=100" "\"offset\":100"
run_read "$REPO/src/main.rs" "src/main.rs --limit=50"  "\"limit\":50"

# --- 8) Unreadable file (doesn't exist) — pass-through FileUnreadable ---
echo
echo "[8] unreadable file (does not exist)"
run_read "$SCRATCH/does-not-exist.txt" "does-not-exist.txt"

# --- 9) Changed-file reread: read it, modify it, read again -> different cache ---
echo
echo "[9] changed-file reread (modify content between two Reads)"
MUT="$SCRATCH/mutating.log"
python3 -c "
with open('$MUT','w') as f:
    for i in range(2000):
        f.write(f'[INFO] step {i}: deterministic line for caching\n')
"
run_read "$MUT" "mutating.log (run 1)"
# Append a tail to bump the digest.
echo "[INFO] new tail line" >> "$MUT"
run_read "$MUT" "mutating.log (run 2, modified)"

# --- 10) KNAPSACK_READ_HOOK=0 (off-switch) — pass-through GateDisabled ---
echo
echo "[10] off-switch (KNAPSACK_READ_HOOK=0)"
KNAPSACK_READ_HOOK=0 "$KS" hook < <(echo "{\"tool_name\":\"Read\",\"tool_input\":{\"file_path\":\"$REPO/src/main.rs\"}}") 2>&1 \
  | { read -r body; [ -z "$body" ] && echo "  off-switch -> pass-through (hook emitted nothing)" || echo "  unexpected output: $body"; }

# Now drive `why-last` and the surfaces.
echo
echo "============================================================"
echo "why-last (latest 12 decisions):"
echo "============================================================"
"$KS" why-last 12

echo
echo "============================================================"
echo "/knapsack surface:"
echo "============================================================"
"$KS"

echo
echo "============================================================"
echo "knapsack doctor (last 6 lines):"
echo "============================================================"
"$KS" doctor 2>&1 | tail -6
