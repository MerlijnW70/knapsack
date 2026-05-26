#!/usr/bin/env bash
# Heavier-workload follow-up to measure_real.sh: same code path (pack -), but on
# realistic-size tool output. Specifically, the test suite (~25 KB) and a `cargo
# clippy` run, run 4× each to expose how delta encoding ramps up. Every number is
# parsed from `knapsack`'s own metrics JSONL — no rephrasing.
set -u
KS="${KS:-$HOME/.knapsack/bin/knapsack}"
RUN_DIR=$(mktemp -d)
export KNAPSACK_STORE="$RUN_DIR/store"
export KNAPSACK_METRICS="$RUN_DIR/metrics.jsonl"
export KNAPSACK_SESSIONS="$RUN_DIR/sessions"

cd "$(dirname "$0")/.."
: > "$KNAPSACK_METRICS"

print_hr() { printf '%s\n' "--------------------------------------------------------------------------------"; }

echo
echo "=== Heavy tool-output pack — realistic Claude-Code-style commands, 4× each ==="
print_hr
printf "%-50s %8s %8s %8s %8s %7s\n" "command (run #)" "bytes" "raw_tok" "shown_tok" "saved" "%saved"
print_hr

# Pre-capture each command's output ONCE so the bytes-on-disk are identical across
# runs. The signal we care about is what pack does given identical input each time,
# not noise from clock-sensitive output.
CAP_DIR="$RUN_DIR/capture"
mkdir -p "$CAP_DIR"
CMDS=(
  "cargo test --release"
  "cargo clippy --release --all-targets"
  "cargo build --release"
  "ls -laR src tests"
)
for cmd in "${CMDS[@]}"; do
  safe=$(echo "$cmd" | tr -cs 'A-Za-z0-9' '_')
  eval "$cmd" > "$CAP_DIR/$safe.out" 2>&1
done

SESSION="heavy-measure"

for cmd in "${CMDS[@]}"; do
  safe=$(echo "$cmd" | tr -cs 'A-Za-z0-9' '_')
  src="$CAP_DIR/$safe.out"
  size=$(wc -c < "$src")
  for run in 1 2 3 4; do
    "$KS" pack - --session "$SESSION" --cmd "$cmd" --type log < "$src" > /dev/null 2>/dev/null
    line=$(tail -n 1 "$KNAPSACK_METRICS")
    raw_tok=$(echo "$line"   | python3 -c "import json,sys; print(json.loads(sys.stdin.read()).get('raw',0))"   2>/dev/null || echo 0)
    shown_tok=$(echo "$line" | python3 -c "import json,sys; print(json.loads(sys.stdin.read()).get('shown',0))" 2>/dev/null || echo 0)
    saved=$(( raw_tok - shown_tok ))
    pct=$(awk "BEGIN { if ($raw_tok > 0) printf \"%.1f%%\", 100*$saved/$raw_tok; else printf \"--\" }")
    label="$cmd  (#$run)"
    [ "${#label}" -gt 50 ] && label="${label:0:47}..."
    printf "%-50s %8s %8s %8s %8s %7s\n" "$label" "$size" "$raw_tok" "$shown_tok" "$saved" "$pct"
  done
done
print_hr

echo
echo "=== Aggregate over the heavy workload ==="
print_hr
"$KS" ab --knapsack "$KNAPSACK_METRICS"

rm -rf "$RUN_DIR"
