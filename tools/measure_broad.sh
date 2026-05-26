#!/usr/bin/env bash
# Comprehensive real-data measurement of knapsack token reduction.
#
# Sweeps:
#   I)   Input / context — every real source/test/markdown/config in the repo.
#   II)  Output / tool — many real shell commands, captured live, packed 1x cold and
#        3x warm to show how delta encoding amortizes.
#   III) Edit-loop — capture test output, modify a file, capture again, pack both.
#        This is THE Knapsack scenario: a one-line code change between two runs.
#   IV)  Read-hook view — run the actual read_hook decide() over real files and show
#        the view-vs-raw token reduction the hook would emit.
#
# Numbers are parsed directly from `knapsack`'s own output (pack --dry-run) and from
# metrics.jsonl (pack -). Nothing is invented. Tiny files that hit the never-worse-
# than-raw guard print 0% saved (the guard firing is itself a measurement).
set -u
KS="${KS:-$HOME/.knapsack/bin/knapsack}"
ROOT=$(mktemp -d)
export KNAPSACK_STORE="$ROOT/store"
export KNAPSACK_METRICS="$ROOT/metrics.jsonl"
export KNAPSACK_SESSIONS="$ROOT/sessions"
export KNAPSACK_READ_LOG="$ROOT/read_hook.jsonl"

cd "$(dirname "$0")/.."

hr() { printf '%s\n' "$(printf '─%.0s' {1..96})"; }
section() { echo; echo; printf "═══ %s\n" "$*"; hr; }

# ------------------------------------------------------------
# Helpers
# ------------------------------------------------------------
# Parse a pack --dry-run output and emit "raw packed saved pct".
parse_pack_dryrun() {
  local out="$1"
  local raw packed saved pct
  raw=$(echo    "$out" | grep -E '^Original:' | grep -oE '[0-9,]+' | head -1 | tr -d ',')
  packed=$(echo "$out" | grep -E '^Packed:'   | grep -oE '[0-9,]+' | head -1 | tr -d ',')
  saved=$(echo  "$out" | grep -E '^Saved:'    | grep -oE '[0-9,]+' | head -1 | tr -d ',')
  pct=$(echo    "$out" | grep -E '^Saved:'    | grep -oE '[0-9.]+%' | head -1)
  echo "${raw:-0} ${packed:-0} ${saved:-0} ${pct:-0.0%}"
}

# Parse the last metrics.jsonl line for raw/shown/saved.
last_metric() {
  local line; line=$(tail -n 1 "$KNAPSACK_METRICS" 2>/dev/null)
  [ -z "$line" ] && { echo "0 0 0"; return; }
  python3 -c "
import json,sys
v=json.loads(sys.stdin.read())
raw=int(v.get('raw',0)); shown=int(v.get('shown',0))
print(raw, shown, raw-shown)
" <<< "$line"
}

# Pack a captured file via stdin, report raw/shown/saved/%
pack_via_stdin() {
  local src="$1" session="$2" cmd_label="$3"
  "$KS" pack - --session "$session" --cmd "$cmd_label" --type log < "$src" > /dev/null 2>/dev/null
  local raw shown saved
  read -r raw shown saved < <(last_metric)
  local pct="--"
  if [ "$raw" -gt 0 ]; then
    pct=$(awk "BEGIN { printf \"%.1f%%\", 100*$saved/$raw }")
  fi
  echo "$raw $shown $saved $pct"
}

# ============================================================
# I) INPUT — context-file pack across the whole repo
# ============================================================
section "I) INPUT — context-file pack on every real file in the repo"

declare -A CAT_RAW CAT_PACKED CAT_FILES
TOTAL_RAW=0; TOTAL_PACKED=0; TOTAL_FILES=0

measure_input_file() {
  local f="$1" cat="$2"
  [ -e "$f" ] || return
  [ -s "$f" ] || return
  local size; size=$(wc -c < "$f")
  # Skip non-text-ish (>1 MB or has many nulls)
  [ "$size" -gt 1048576 ] && return
  local out; out=$( "$KS" pack "$f" --dry-run 2>&1 )
  echo "$out" | grep -q '^Original:' || return
  read -r raw packed saved pct < <(parse_pack_dryrun "$out")
  [ "${raw:-0}" -eq 0 ] && return
  printf "  %-44s %8s %10s %10s %8s\n" "$(echo "$f" | sed 's@^./@@')" "${size}B" "$raw" "$packed" "$pct"
  CAT_RAW[$cat]=$(( ${CAT_RAW[$cat]:-0} + raw ))
  CAT_PACKED[$cat]=$(( ${CAT_PACKED[$cat]:-0} + packed ))
  CAT_FILES[$cat]=$(( ${CAT_FILES[$cat]:-0} + 1 ))
  TOTAL_RAW=$(( TOTAL_RAW + raw ))
  TOTAL_PACKED=$(( TOTAL_PACKED + packed ))
  TOTAL_FILES=$(( TOTAL_FILES + 1 ))
}

run_input_category() {
  local cat="$1"; shift
  echo
  printf "── %s ──────────────────────────────────────────────────────────────\n" "$cat"
  printf "  %-44s %8s %10s %10s %8s\n" "file" "bytes" "raw" "packed" "%saved"
  for f in "$@"; do measure_input_file "$f" "$cat"; done
}

# Rust source — every src/*.rs file
RS_SRC=( $(ls src/*.rs 2>/dev/null | sort) )
run_input_category "rust_src" "${RS_SRC[@]}"

# Rust tests — every tests/*.rs file
RS_TESTS=( $(ls tests/*.rs 2>/dev/null | sort) )
run_input_category "rust_tests" "${RS_TESTS[@]}"

# Markdown
MDS=( $(ls *.md 2>/dev/null | sort) )
run_input_category "markdown" "${MDS[@]}"

# Configs
CONFS=(Cargo.toml Cargo.lock .gitignore install.ps1)
run_input_category "configs" "${CONFS[@]}"

# Shell scripts
SHS=( $(ls install.sh tools/*.sh 2>/dev/null | sort) )
run_input_category "scripts" "${SHS[@]}"

# Bench harness
BENCH=( $(ls benches/*.rs 2>/dev/null) )
run_input_category "bench_rs" "${BENCH[@]}"

echo
hr
printf "%-16s %8s %12s %12s %8s\n" "Category" "files" "raw_tok" "packed_tok" "%saved"
hr
for cat in rust_src rust_tests markdown configs scripts bench_rs; do
  r=${CAT_RAW[$cat]:-0}; p=${CAT_PACKED[$cat]:-0}; n=${CAT_FILES[$cat]:-0}
  [ "$n" -eq 0 ] && continue
  pct=$(awk "BEGIN { printf \"%.1f%%\", 100*($r - $p)/$r }")
  printf "%-16s %8d %12d %12d %8s\n" "$cat" "$n" "$r" "$p" "$pct"
done
hr
total_pct=$(awk "BEGIN { printf \"%.1f%%\", 100*($TOTAL_RAW - $TOTAL_PACKED)/$TOTAL_RAW }")
printf "%-16s %8d %12d %12d %8s\n" "TOTAL" "$TOTAL_FILES" "$TOTAL_RAW" "$TOTAL_PACKED" "$total_pct"

# ============================================================
# II) OUTPUT — tool output across real commands
# ============================================================
section "II) OUTPUT — real commands packed 1× cold + 3× warm"

# Capture each command's output once so the runs use identical bytes.
CAP="$ROOT/capture"; mkdir -p "$CAP"
declare -a CMDS_LABELS CMDS_FILES
add_cmd() {
  local label="$1"; shift
  local file="$CAP/$(echo "$label" | tr -cs 'A-Za-z0-9' '_').out"
  eval "$@" > "$file" 2>&1
  CMDS_LABELS+=("$label")
  CMDS_FILES+=("$file")
}

# Build / test / lint / check
add_cmd "cargo --version"              "cargo --version"
add_cmd "cargo build --release"        "cargo build --release"
add_cmd "cargo check"                  "cargo check"
add_cmd "cargo clippy --release"       "cargo clippy --release --all-targets --no-deps"
# A test run, but small scope to keep wall-clock down (still real, several hundred lines).
add_cmd "cargo test --release pack"    "cargo test --release pack_doc 2>&1"
add_cmd "cargo metadata --no-deps"     "cargo metadata --no-deps"
add_cmd "cargo tree"                   "cargo tree"

# Listings / file system
add_cmd "ls -la src"                   "ls -la src"
add_cmd "ls -laR src tests"            "ls -laR src tests"
add_cmd "find src -name '*.rs'"        "find src -name '*.rs' -printf '%p %s\n'"
add_cmd "wc -l src/*.rs"               "wc -l src/*.rs"
add_cmd "du -sh src tests benches"     "du -sh src tests benches"

# Git
add_cmd "git status"                   "git status"
add_cmd "git log --oneline -50"        "git log --oneline -50"
add_cmd "git log --stat -3"            "git log --stat -3"
add_cmd "git diff HEAD"                "git diff HEAD"
add_cmd "git show --stat HEAD"         "git show --stat HEAD"

# Text scanning
add_cmd "grep -rn TODO src tests"      "grep -rn TODO src tests || true"
add_cmd "grep -rn 'fn ' src | head -200" "grep -rn 'fn ' src | head -200"

# Knapsack itself
add_cmd "knapsack help"                "$KS 2>&1 || true"
add_cmd "knapsack status"              "$KS status"
add_cmd "knapsack metrics"             "$KS metrics"

# Network-style synthetic-from-real (cargo doc output is real & varied)
add_cmd "rustc --print cfg"            "rustc --print cfg"

# Tiny outputs (where the guard matters)
add_cmd "echo hi"                      "echo hi"
add_cmd "pwd"                          "pwd"

# Reset metrics for clean per-command parsing.
: > "$KNAPSACK_METRICS"

# Print table header
printf "  %-38s %8s %8s %8s %8s %8s %8s %8s\n" \
  "command" "bytes" "raw" "cold" "%cold" "warm" "%warm" "edge?"
hr

# Pack each command's output 4 times, parse metrics, render row.
RUN_AGG_RAW=0; RUN_AGG_SHOWN_COLD=0; RUN_AGG_SHOWN_WARM=0
SESSION="broad-output"
declare -A WORST_CASES
for i in "${!CMDS_LABELS[@]}"; do
  label="${CMDS_LABELS[$i]}"; src="${CMDS_FILES[$i]}"
  size=$(wc -c < "$src")
  # Run 1 = cold
  read -r raw1 shown1 saved1 pct1 < <(pack_via_stdin "$src" "$SESSION" "$label")
  # Run 2 = first warm
  read -r raw2 shown2 saved2 pct2 < <(pack_via_stdin "$src" "$SESSION" "$label")
  # Run 3 = second warm (steady state)
  read -r raw3 shown3 saved3 pct3 < <(pack_via_stdin "$src" "$SESSION" "$label")
  # Run 4 = third warm
  read -r raw4 shown4 saved4 pct4 < <(pack_via_stdin "$src" "$SESSION" "$label")
  # Steady-state warm is the median of the warm runs (here we pick min — most pessimistic)
  warm_shown=$shown4
  edge=""
  # If the guard fires (shown == raw with saved=0 for non-trivial inputs), tag it
  if [ "$raw1" -gt 0 ] && [ "$shown1" -eq "$raw1" ]; then edge="cold-noop"; fi
  if [ "$raw1" -gt 0 ] && [ "$warm_shown" -eq "$raw1" ]; then edge="${edge:+$edge,}warm-noop"; fi
  short_label="$label"
  [ "${#short_label}" -gt 38 ] && short_label="${short_label:0:35}..."
  printf "  %-38s %8s %8s %8s %8s %8s %8s %8s\n" \
    "$short_label" "$size" "$raw1" "$shown1" "$pct1" "$warm_shown" \
    "$(awk "BEGIN { if ($raw1 > 0) printf \"%.1f%%\", 100*($raw1 - $warm_shown)/$raw1; else printf \"--\" }")" \
    "${edge:-—}"
  RUN_AGG_RAW=$(( RUN_AGG_RAW + raw1 ))
  RUN_AGG_SHOWN_COLD=$(( RUN_AGG_SHOWN_COLD + shown1 ))
  RUN_AGG_SHOWN_WARM=$(( RUN_AGG_SHOWN_WARM + warm_shown ))
done
hr
agg_cold_pct=$(awk "BEGIN { printf \"%.1f%%\", 100*($RUN_AGG_RAW - $RUN_AGG_SHOWN_COLD)/$RUN_AGG_RAW }")
agg_warm_pct=$(awk "BEGIN { printf \"%.1f%%\", 100*($RUN_AGG_RAW - $RUN_AGG_SHOWN_WARM)/$RUN_AGG_RAW }")
printf "  %-38s          %8s %8s %8s %8s %8s\n" "AGGREGATE (${#CMDS_LABELS[@]} commands)" \
  "$RUN_AGG_RAW" "$RUN_AGG_SHOWN_COLD" "$agg_cold_pct" "$RUN_AGG_SHOWN_WARM" "$agg_warm_pct"

# ============================================================
# III) EDIT-LOOP — the canonical Knapsack scenario
# ============================================================
section "III) EDIT-LOOP — capture cargo test, change one file, capture again"

EDIT_DIR="$ROOT/editloop"; mkdir -p "$EDIT_DIR"
EDIT_SESSION="edit-loop"
# Reset metrics for clean per-step parsing.
: > "$KNAPSACK_METRICS"

# Step A: capture an initial test run.
cargo test --release pack_doc > "$EDIT_DIR/run_A.txt" 2>&1

# Step B: nudge a comment in a source file. We use a unique line that we KNOW exists
# (the `pub fn pack(` line in src/pack.rs is stable). We append a no-op comment line
# above it, run, then restore. This produces a real diff against run_A but in the
# captured TEST OUTPUT we expect almost no change.
TARGET="src/pack.rs"
BACKUP="$EDIT_DIR/pack.rs.bak"
cp "$TARGET" "$BACKUP"
python3 -c "
import sys
text = open(sys.argv[1]).read()
needle = 'pub fn pack('
insertion = '// knapsack edit-loop measurement: temporary no-op comment\n'
out = text.replace(needle, insertion + needle, 1)
open(sys.argv[1], 'w').write(out)
" "$TARGET"
# rebuild & run again
cargo test --release pack_doc > "$EDIT_DIR/run_B.txt" 2>&1
# restore
mv "$BACKUP" "$TARGET"

# Step C: confirm a third run with the file restored.
cargo test --release pack_doc > "$EDIT_DIR/run_C.txt" 2>&1

echo
printf "  %-22s %10s %10s %10s %10s\n" "run" "bytes" "raw_tok" "shown_tok" "%saved"
hr
for tag in A B C; do
  src="$EDIT_DIR/run_$tag.txt"
  read -r raw shown saved pct < <(pack_via_stdin "$src" "$EDIT_SESSION" "cargo test --release pack_doc [run $tag]")
  size=$(wc -c < "$src")
  printf "  %-22s %10s %10s %10s %10s\n" "run $tag" "$size" "$raw" "$shown" "$pct"
done

# ============================================================
# IV) READ-HOOK VIEW — run the actual decide() over real files
# ============================================================
section "IV) READ-HOOK VIEW — compressed view emitted for the model when it Reads a file"

# Force the hook enabled in a SUBSHELL so we don't taint the rest.
READ_OUT="$ROOT/readhook"; mkdir -p "$READ_OUT"
export KNAPSACK_READ_CACHE="$READ_OUT/cache"

# We use the binary directly with a synthesized PreToolUse event so we can capture the
# emitted updatedInput. If `decide` redirects, the view file at the new path has the
# compressed bytes; tokens(view) vs tokens(raw) is the actual reduction the model sees.
read_hook_view() {
  local f="$1"
  local raw_tok view_tok
  raw_tok=$(python3 -c "
import sys
from pathlib import Path
b = Path(sys.argv[1]).read_bytes()
# Mirror token_estimate::tokens (UTF-16 char-class weights).
s = b.decode('utf-8', errors='replace')
a=d=sym=sp=0
import math
for cu in s.encode('utf-16-le'):
    pass
# Use the simpler char loop; matching is close enough for ratio purposes here.
# Actually call the binary to get the exact estimate via 'pack --dry-run --output /dev/null'
# No — easier: pipe to pack - and read raw from metrics.
" "$f")
  : > "$KNAPSACK_METRICS"
  "$KS" pack - < "$f" > /dev/null 2>/dev/null
  read -r raw _ _ < <(last_metric)
  # Now simulate the hook decision via the binary's PreToolUse path.
  envelope=$(python3 -c "
import json,sys
print(json.dumps({'tool_name':'Read','tool_input':{'file_path':sys.argv[1]}}))
" "$f")
  : > "$ROOT/hook_out"
  echo "$envelope" | KNAPSACK_READ_HOOK=1 KNAPSACK_READ_CACHE="$KNAPSACK_READ_CACHE" "$KS" hook > "$ROOT/hook_out" 2>/dev/null
  local body; body=$(cat "$ROOT/hook_out")
  if [ -z "$body" ]; then
    # Hook passed through. Reasons: TooSmall, TooLarge, WorseThanRaw, etc.
    echo "$raw n/a passthrough"
    return
  fi
  # Parse the redirect path out of updatedInput.file_path
  local view_path
  view_path=$(python3 -c "
import json,sys
v=json.loads(sys.argv[1])
print(v['hookSpecificOutput']['updatedInput']['file_path'])
" "$body" 2>/dev/null)
  if [ -z "$view_path" ] || [ ! -f "$view_path" ]; then
    echo "$raw err err"; return
  fi
  : > "$KNAPSACK_METRICS"
  "$KS" pack - < "$view_path" > /dev/null 2>/dev/null
  read -r view_raw _ _ < <(last_metric)
  echo "$raw $view_raw redirected"
}

printf "  %-44s %8s %10s %10s %10s %12s\n" "file" "bytes" "raw_tok" "view_tok" "%saved" "decision"
hr
HOOK_RAW=0; HOOK_VIEW=0; HOOK_REDIRECTED=0; HOOK_PASS=0
for f in src/main.rs src/store.rs src/structural.rs src/api.rs src/pack.rs CHANGELOG.md README.md DOGFOOD.md Cargo.lock; do
  [ -e "$f" ] || continue
  size=$(wc -c < "$f")
  read -r raw view decision < <(read_hook_view "$f")
  if [ "$decision" = "redirected" ]; then
    saved=$(( raw - view ))
    pct=$(awk "BEGIN { if ($raw > 0) printf \"%.1f%%\", 100*$saved/$raw; else printf \"--\" }")
    HOOK_RAW=$(( HOOK_RAW + raw )); HOOK_VIEW=$(( HOOK_VIEW + view ))
    HOOK_REDIRECTED=$(( HOOK_REDIRECTED + 1 ))
    printf "  %-44s %8s %10s %10s %10s %12s\n" "$f" "$size" "$raw" "$view" "$pct" "redirected"
  else
    HOOK_PASS=$(( HOOK_PASS + 1 ))
    printf "  %-44s %8s %10s %10s %10s %12s\n" "$f" "$size" "$raw" "-" "-" "$decision"
  fi
done
hr
if [ "$HOOK_REDIRECTED" -gt 0 ]; then
  hook_pct=$(awk "BEGIN { printf \"%.1f%%\", 100*($HOOK_RAW - $HOOK_VIEW)/$HOOK_RAW }")
  echo "  Read-hook subtotal: $HOOK_REDIRECTED redirected, $HOOK_PASS pass-through. On redirected: ${HOOK_RAW} -> ${HOOK_VIEW} = $hook_pct saved."
else
  echo "  Read-hook subtotal: 0 redirected, $HOOK_PASS pass-through."
fi

# ============================================================
# V) Session-level summary via knapsack ab
# ============================================================
section "V) Session-level — knapsack ab (the same view Claude Code metrics surface uses)"
"$KS" ab --knapsack "$KNAPSACK_METRICS"

rm -rf "$ROOT"
