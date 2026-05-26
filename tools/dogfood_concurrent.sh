#!/usr/bin/env bash
# Hammer the store / metrics / read-cache from many processes at once. The store
# is content-addressed so concurrent writes must dedup not corrupt; the metrics file
# is JSONL so concurrent appends must not interleave inside a record.
set -u
KS="${KS:-$HOME/.knapsack/bin/knapsack}"
ROOT="D:/knapsack/target/dogfood_conc"
rm -rf "$ROOT" 2>/dev/null
mkdir -p "$ROOT/cache"
export KNAPSACK_STORE="$ROOT/store"
export KNAPSACK_METRICS="$ROOT/metrics.jsonl"
export KNAPSACK_SESSIONS="$ROOT/sessions"
export KNAPSACK_READ_LOG="$ROOT/read_hook.jsonl"
export KNAPSACK_READ_CACHE="$ROOT/cache"
: > "$KNAPSACK_METRICS"
: > "$KNAPSACK_READ_LOG"

PASS=0; FAIL=0
ok() { PASS=$((PASS+1)); echo "  ✓ $*"; }
no() { FAIL=$((FAIL+1)); echo "  ✗ $*"; }

# Prepare a directory of varied real-ish content
mkdir -p "$ROOT/fixtures"
python3 - "$ROOT/fixtures" <<'PYEOF'
import os, sys, random
random.seed(42)
d = sys.argv[1]
for i in range(50):
    # Half identical content (dedup target), half unique.
    if i < 25:
        body = ('[INFO] common shared content for dedup test ' + '-' * 200 + '\n') * 200
    else:
        body = ''.join(f'[INFO] worker-{i} step {j} status=ok latency={random.randint(0,500)}ms\n' for j in range(200))
    with open(os.path.join(d, f'f{i}.log'), 'w') as f:
        f.write(body)
PYEOF

# ---- A) Many concurrent `pack -` writes to the same store ----
echo "[A] 30 parallel `pack -` invocations into the same store"
pids=()
for f in "$ROOT"/fixtures/f*.log; do
  "$KS" pack - --session "conc-$$" --cmd "concurrent" --type log < "$f" > /dev/null 2>"$ROOT/conc.err" &
  pids+=($!)
done
# Concurrent reads of the same store from `expand` — pick a handle that EXISTS
sample=$( "$KS" pack - --session "spy" --cmd "spy" --type log < "$ROOT/fixtures/f0.log" 2>&1 | grep -oE 'ks2_[0-9a-f]+' | head -1 )
for r in $(seq 1 5); do
  "$KS" expand "$sample" > /dev/null 2>>"$ROOT/conc.err" &
  pids+=($!)
done
# And a GC pass at the same time
"$KS" gc --older-than 365 > /dev/null 2>>"$ROOT/conc.err" &
pids+=($!)
wait "${pids[@]}"

# Check: no panics in stderr
if grep -q "panicked" "$ROOT/conc.err"; then
  no "panic detected"; head -30 "$ROOT/conc.err"
else
  ok "no panic across $(wc -l < /dev/null; echo "${#pids[@]}") parallel ops"
fi
# Check: every fixture's bytes are recoverable byte-exact
mismatches=0
for f in "$ROOT"/fixtures/f*.log; do
  h=$( "$KS" pack "$f" --dry-run 2>&1 | grep -oE 'ks2_[0-9a-f]+' | head -1 )
  exp=$("$KS" expand "$h" 2>/dev/null | wc -c)
  raw=$(wc -c < "$f")
  if [ "$exp" -ne "$raw" ]; then
    mismatches=$((mismatches + 1))
  fi
done
if [ "$mismatches" -eq 0 ]; then
  ok "all 50 fixtures recoverable byte-exact"
else
  no "$mismatches fixtures lost bytes after concurrent ops"
fi

# Check: metrics.jsonl has no torn lines
torn=0
while IFS= read -r line; do
  [ -z "$line" ] && continue
  if ! echo "$line" | python3 -c "import json,sys; json.loads(sys.stdin.read())" 2>/dev/null; then
    torn=$((torn + 1))
  fi
done < "$KNAPSACK_METRICS"
if [ "$torn" -eq 0 ]; then
  ok "metrics.jsonl has $(wc -l < "$KNAPSACK_METRICS") clean JSON lines"
else
  no "$torn torn lines in metrics.jsonl"
fi

# ---- B) Concurrent read-hook calls + GC pass ----
echo
echo "[B] 20 parallel read-hook invocations on different files + 2 GC passes"
: > "$KNAPSACK_READ_LOG"
pids=()
for f in "$ROOT"/fixtures/f*.log; do
  envelope="{\"tool_name\":\"Read\",\"tool_input\":{\"file_path\":\"$f\"}}"
  echo "$envelope" | "$KS" hook > /dev/null 2>>"$ROOT/conc.err" &
  pids+=($!)
done
"$KS" gc --older-than 365 > /dev/null 2>>"$ROOT/conc.err" &
pids+=($!)
"$KS" gc --older-than 0 --dry-run > /dev/null 2>>"$ROOT/conc.err" &
pids+=($!)
wait "${pids[@]}"

if grep -q "panicked" "$ROOT/conc.err"; then
  no "panic in B"
else
  ok "no panic across ${#pids[@]} parallel hook+gc ops"
fi
# Check: read_hook.jsonl has no torn lines
torn=0
while IFS= read -r line; do
  [ -z "$line" ] && continue
  if ! echo "$line" | python3 -c "import json,sys; json.loads(sys.stdin.read())" 2>/dev/null; then
    torn=$((torn + 1))
  fi
done < "$KNAPSACK_READ_LOG"
if [ "$torn" -eq 0 ]; then
  ok "read_hook.jsonl has $(wc -l < "$KNAPSACK_READ_LOG") clean JSON lines after races"
else
  no "$torn torn lines in read_hook.jsonl"
fi

# ---- C) MCP server stress: concurrent requests on the same stdio process ----
echo
echo "[C] Single MCP process, 30 rapid-fire mixed requests"
python3 - "$KS" "$sample" <<'PYEOF'
import json, subprocess, sys, os
ks, handle = sys.argv[1], sys.argv[2]
env = dict(os.environ)
proc = subprocess.Popen([ks, "mcp"], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, env=env)
reqs = []
for i in range(30):
    if i % 3 == 0:
        reqs.append({"id": i, "jsonrpc":"2.0","method":"tools/list"})
    elif i % 3 == 1:
        reqs.append({"id": i, "jsonrpc":"2.0","method":"tools/call","params":{"name":"knapsack_expand","arguments":{"handle":handle}}})
    else:
        reqs.append({"id": i, "jsonrpc":"2.0","method":"tools/call","params":{"name":"knapsack_metrics","arguments":{}}})
payload = ("\n".join(json.dumps(r) for r in reqs) + "\n").encode("utf-8")
out, err = proc.communicate(payload, timeout=30)
ids = []
bad = 0
for line in out.decode("utf-8", errors="replace").splitlines():
    if not line.strip(): continue
    try:
        v = json.loads(line)
        ids.append(v.get("id"))
        if v.get("jsonrpc") != "2.0":
            bad += 1
    except Exception:
        bad += 1
expected = set(range(30))
got = set(i for i in ids if isinstance(i, int))
missing = expected - got
print(f"ok responses: {len(got)} of 30; bad: {bad}; missing ids: {sorted(missing)[:10]}; stderr: {err[:200]!r}")
if proc.returncode != 0:
    print(f"  MCP rc={proc.returncode}")
PYEOF

# Re-verify store byte-exactness AFTER everything
echo
echo "[D] Post-stress byte-exact recall over the entire fixture set"
mismatches=0
for f in "$ROOT"/fixtures/f*.log; do
  h=$( "$KS" pack "$f" --dry-run 2>&1 | grep -oE 'ks2_[0-9a-f]+' | head -1 )
  if ! "$KS" expand "$h" 2>/dev/null | cmp -s - "$f"; then
    mismatches=$((mismatches + 1))
  fi
done
if [ "$mismatches" -eq 0 ]; then
  ok "all 50 fixtures still byte-exact after all stress"
else
  no "$mismatches lost byte-exactness"
fi

echo
echo "================================================================"
echo "Concurrent ops: $PASS pass, $FAIL fail"
echo "================================================================"
exit $FAIL
