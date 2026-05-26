#!/usr/bin/env bash
# Measure REAL token reduction on REAL data, two scenarios:
#
#  A) Context-file pack       — `knapsack pack <file>` on actual project files.
#                                Represents what happens when a large file is loaded
#                                into context (e.g. via the Read-hook view).
#
#  B) Tool-output pack        — `knapsack pack -` on captured stdout from real
#                                shell commands. Represents what happens when a noisy
#                                Bash tool result goes through the PreToolUse hook.
#                                Run twice per command to expose delta-encoding.
#
# Every number printed is parsed from `knapsack` output or the JSONL metrics file.
# Nothing is hand-rolled or hard-coded. The token estimator is the same one knapsack
# uses internally (token_estimate::tokens, UTF-16 char-class weights from Rucksack).
set -u
KS="${KS:-$HOME/.knapsack/bin/knapsack}"
RUN_DIR=$(mktemp -d)
export KNAPSACK_STORE="$RUN_DIR/store"
export KNAPSACK_METRICS="$RUN_DIR/metrics.jsonl"
export KNAPSACK_SESSIONS="$RUN_DIR/sessions"

cd "$(dirname "$0")/.."

print_hr() { printf '%s\n' "------------------------------------------------------------------------"; }

# ------------------------------------------------------------
# A) Context-file pack on real project files.
# ------------------------------------------------------------
echo
echo "=== A) Context-file pack — real files from this repo ==="
print_hr
printf "%-30s %10s %10s %10s %7s\n" "file" "raw_tok" "packed_tok" "saved_tok" "%saved"
print_hr

FILES=(
  src/api.rs
  src/main.rs
  src/store.rs
  src/structural.rs
  src/hook.rs
  CHANGELOG.md
  README.md
  Cargo.toml
)
A_RAW=0; A_PACKED=0; A_FILES=0
for f in "${FILES[@]}"; do
  [ -e "$f" ] || continue
  # Parse the four lines: "Original: N tokens", "Packed: N tokens", "Saved: N tokens / X%".
  out=$( "$KS" pack "$f" --dry-run 2>&1 )
  raw=$(echo "$out"    | grep -E '^Original:' | grep -oE '[0-9,]+' | head -1 | tr -d ',')
  packed=$(echo "$out" | grep -E '^Packed:'   | grep -oE '[0-9,]+' | head -1 | tr -d ',')
  saved=$(echo "$out"  | grep -E '^Saved:'    | grep -oE '[0-9,]+' | head -1 | tr -d ',')
  pct=$(echo "$out"    | grep -E '^Saved:'    | grep -oE '[0-9.]+%' | head -1)
  [ -z "$raw" ] && continue
  printf "%-30s %10s %10s %10s %7s\n" "$f" "$raw" "$packed" "$saved" "$pct"
  A_RAW=$(( A_RAW + raw ))
  A_PACKED=$(( A_PACKED + packed ))
  A_FILES=$(( A_FILES + 1 ))
done
print_hr
A_SAVED=$(( A_RAW - A_PACKED ))
A_PCT=$(awk "BEGIN { printf \"%.1f\", 100*$A_SAVED/$A_RAW }")
printf "%-30s %10s %10s %10s %6s%%\n" "TOTAL ($A_FILES files)" "$A_RAW" "$A_PACKED" "$A_SAVED" "$A_PCT"
echo
echo "Interpretation: this is what context-file compression saves when the model"
echo "reads each of these files exactly once. No delta encoding; cold-pass only."
echo

# ------------------------------------------------------------
# B) Tool-output pack — real commands, twice each (cold + warm).
# ------------------------------------------------------------
echo
echo "=== B) Tool-output pack — real shell commands, twice each ==="
print_hr
printf "%-40s %6s %8s %8s %8s %7s\n" "command (run #)" "size" "raw_tok" "shown_tok" "saved" "%saved"
print_hr

# Reset the metrics file so we read clean numbers below.
: > "$KNAPSACK_METRICS"

CMDS=(
  "cargo --version"
  "cargo metadata --no-deps"
  "ls -la src"
  "git status"
  "git log --oneline -50"
  "find src -name '*.rs' -printf '%f %s\n'"
)

SESSION="real-measure"

for cmd in "${CMDS[@]}"; do
  for run in 1 2; do
    raw_file=$(mktemp)
    eval "$cmd" > "$raw_file" 2>&1
    size=$(wc -c < "$raw_file")
    # Skip empty outputs.
    [ "$size" -eq 0 ] && { rm -f "$raw_file"; continue; }
    # Pipe through `pack -`. It writes the visible (delta-compressed) form to stdout
    # AND appends a {"event":"compress",...} line to KNAPSACK_METRICS with the exact
    # raw/shown/saved numbers.
    shown_file=$(mktemp)
    "$KS" pack - --session "$SESSION" --cmd "$cmd" --type log < "$raw_file" > "$shown_file" 2>/dev/null
    # Read the LAST event in the metrics file (== the one we just wrote).
    line=$(tail -n 1 "$KNAPSACK_METRICS")
    raw_tok=$(echo "$line"   | python3 -c "import json,sys; print(json.loads(sys.stdin.read())['raw'])"   2>/dev/null || echo "0")
    shown_tok=$(echo "$line" | python3 -c "import json,sys; print(json.loads(sys.stdin.read())['shown'])" 2>/dev/null || echo "0")
    saved=$(( raw_tok - shown_tok ))
    pct=$(awk "BEGIN { if ($raw_tok > 0) printf \"%.1f%%\", 100*$saved/$raw_tok; else printf \"--\" }")
    label="$cmd  (#$run)"
    # Truncate long labels.
    [ "${#label}" -gt 40 ] && label="${label:0:37}..."
    printf "%-40s %6s %8s %8s %8s %7s\n" "$label" "$size" "$raw_tok" "$shown_tok" "$saved" "$pct"
    rm -f "$raw_file" "$shown_file"
  done
done

print_hr
echo
echo "Interpretation: run #1 is COLD — only structural elision saves tokens."
echo "Run #2 is WARM — knapsack sees identical content already in the session"
echo "ledger and emits back-references, which is where the big wins come from."
echo

# ------------------------------------------------------------
# C) Aggregate via the same `knapsack ab` report Claude Code would see.
# ------------------------------------------------------------
echo
echo "=== C) knapsack ab — same report Claude Code's metrics surface uses ==="
print_hr
"$KS" ab --knapsack "$KNAPSACK_METRICS"
print_hr

rm -rf "$RUN_DIR"
