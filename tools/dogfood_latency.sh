#!/usr/bin/env bash
# Concrete latency numbers for the hot paths a Claude Code session actually hits:
#   - PreToolUse hook (Bash pass-through, Bash wrap, Read pass-through, Read redirect)
#   - MCP request round-trip (initialize, tools/list, knapsack_expand)
#   - pack throughput on real cargo test output
# Output is meant to be a baseline: future regressions show up as a clear delta here.
set -u
KS="${KS:-$HOME/.knapsack/bin/knapsack}"
ROOT="D:/knapsack/target/dogfood_latency"
rm -rf "$ROOT" 2>/dev/null
mkdir -p "$ROOT/store" "$ROOT/cache"
export KNAPSACK_STORE="$ROOT/store"
export KNAPSACK_METRICS="$ROOT/metrics.jsonl"
export KNAPSACK_SESSIONS="$ROOT/sessions"
export KNAPSACK_READ_LOG="$ROOT/read_hook.jsonl"
export KNAPSACK_READ_CACHE="$ROOT/cache"

ns_now() { date +%s%N; }
elapsed_us() { echo $(( ($2 - $1) / 1000 )); }

run_n_avg() {
  local label="$1" n="$2"; shift 2
  local cmd_str="$*"
  local total_us=0
  for _ in $(seq 1 "$n"); do
    local s=$(ns_now)
    eval "$cmd_str" > /dev/null 2>&1
    local e=$(ns_now)
    total_us=$(( total_us + ($e - $s) / 1000 ))
  done
  local avg_us=$(( total_us / n ))
  printf "  %-44s avg %6d µs   (%d runs)\n" "$label" "$avg_us" "$n"
}

echo "[A] PreToolUse hook latency"
echo "  measuring: time from JSON-in to JSON-out (process spawn + parse + decide + emit)"

# Cold pass-through (non-Bash tool — just parse + dispatch)
EVT_PT='{"tool_name":"Edit","tool_input":{"file_path":"/x"}}'
run_n_avg "Read-tool: file too small (pass-through)" 20 "echo '{\"tool_name\":\"Read\",\"tool_input\":{\"file_path\":\"$ROOT/tiny\"}}' | '$KS' hook"
# Set up small + big files for read-hook timing.
echo "tiny" > "$ROOT/tiny"
python3 - "$ROOT/big.log" <<'PYEOF'
import sys
with open(sys.argv[1],'wb') as f:
    for i in range(2000):
        f.write(f"[INFO] step {i}: line\n".encode())
PYEOF
run_n_avg "Read-tool: big file pass-1 (build cache)" 5 "echo '{\"tool_name\":\"Read\",\"tool_input\":{\"file_path\":\"$ROOT/big.log\"},\"session_id\":\"perf\"}' | '$KS' hook"
run_n_avg "Read-tool: big file cache-hit"           20 "echo '{\"tool_name\":\"Read\",\"tool_input\":{\"file_path\":\"$ROOT/big.log\"},\"session_id\":\"perf\"}' | '$KS' hook"
run_n_avg "Bash-tool: shell-meta pass-through"      20 "echo '{\"tool_name\":\"Bash\",\"tool_input\":{\"command\":\"ls | head\"},\"session_id\":\"perf\"}' | '$KS' hook"
run_n_avg "Bash-tool: wrappable (emits wrap)"       20 "echo '{\"tool_name\":\"Bash\",\"tool_input\":{\"command\":\"cargo test\"},\"session_id\":\"perf\"}' | '$KS' hook"
run_n_avg "non-Bash tool (Edit) pass-through"       20 "echo '$EVT_PT' | '$KS' hook"
run_n_avg "garbage stdin"                            20 "echo 'not json' | '$KS' hook"

echo
echo "[B] MCP request latency (single-shot processes)"
# Each measurement spawns its own MCP process, sends one request, reads one response.
mcp_one() {
  local req="$1"
  echo "$req" | timeout 5 "$KS" mcp > /dev/null 2>&1
}
run_n_avg "initialize"     10 "mcp_one '{\"id\":1,\"jsonrpc\":\"2.0\",\"method\":\"initialize\"}'"
run_n_avg "tools/list"     10 "mcp_one '{\"id\":2,\"jsonrpc\":\"2.0\",\"method\":\"tools/list\"}'"
# Seed a handle.
echo "test content" | "$KS" pack - --session "perf-seed" --cmd "x" --type log > /dev/null 2>&1
H=$(find "$KNAPSACK_STORE" -name 'ks2_*' -not -name '*.meta' | head -1 | xargs -I{} basename {})
echo "  (using seed handle: $H)"
run_n_avg "knapsack_expand whole"   10 "mcp_one '{\"id\":3,\"jsonrpc\":\"2.0\",\"method\":\"tools/call\",\"params\":{\"name\":\"knapsack_expand\",\"arguments\":{\"handle\":\"$H\"}}}'"
run_n_avg "knapsack_inspect"        10 "mcp_one '{\"id\":4,\"jsonrpc\":\"2.0\",\"method\":\"tools/call\",\"params\":{\"name\":\"knapsack_inspect\",\"arguments\":{\"handle\":\"$H\"}}}'"
run_n_avg "knapsack_metrics"        10 "mcp_one '{\"id\":5,\"jsonrpc\":\"2.0\",\"method\":\"tools/call\",\"params\":{\"name\":\"knapsack_metrics\",\"arguments\":{}}}'"

echo
echo "[C] Pack throughput on a real cargo test output"
cargo test --release pack_doc > "$ROOT/_cargo_out.txt" 2>&1
SIZE=$(wc -c < "$ROOT/_cargo_out.txt")
echo "  cargo test output: $SIZE B"
run_n_avg "pack - (cold)" 5 "'$KS' pack - --session \"perf-c\" --cmd \"cargo\" --type log < '$ROOT/_cargo_out.txt'"
# Warm runs (delta hits).
"$KS" pack - --session "perf-w" --cmd "cargo" --type log < "$ROOT/_cargo_out.txt" > /dev/null 2>&1  # prime ledger
run_n_avg "pack - (warm, repeated)" 5 "'$KS' pack - --session \"perf-w\" --cmd \"cargo\" --type log < '$ROOT/_cargo_out.txt'"

echo
echo "[D] /knapsack status (the user types this often)"
run_n_avg "knapsack status"     10 "'$KS' status"
run_n_avg "knapsack (bare)"     10 "'$KS'"
run_n_avg "knapsack metrics"    10 "'$KS' metrics"
run_n_avg "knapsack doctor"      5 "'$KS' doctor"
run_n_avg "knapsack why-last 5" 10 "'$KS' why-last 5"

echo
echo "[E] Worst case: very-large file Read hook"
python3 - "$ROOT/huge.log" <<'PYEOF'
import sys
with open(sys.argv[1],'wb') as f:
    for i in range(100000):
        f.write(f"[INFO] step {i}: stable line\n".encode())
PYEOF
SIZE=$(wc -c < "$ROOT/huge.log")
echo "  fixture: $SIZE B"
run_n_avg "Read-tool: 3 MB file (cold)" 3 "echo '{\"tool_name\":\"Read\",\"tool_input\":{\"file_path\":\"$ROOT/huge.log\"},\"session_id\":\"huge\"}' | '$KS' hook"
run_n_avg "Read-tool: 3 MB file (cache hit)" 5 "echo '{\"tool_name\":\"Read\",\"tool_input\":{\"file_path\":\"$ROOT/huge.log\"},\"session_id\":\"huge\"}' | '$KS' hook"

echo
echo "================================================================"
echo "Baseline numbers captured. Re-run to detect regressions."
echo "================================================================"
