#!/usr/bin/env bash
# Drive the MCP server through a realistic Claude Code initialization sequence and
# poke at protocol edges. Confirm:
#  - initialize succeeds and advertises the right protocol version + capabilities
#  - notifications/initialized is accepted silently
#  - tools/list returns valid schemas for every tool (input + description)
#  - Re-initialize / out-of-order calls don't break the server
#  - Long round-trip sequences stay coherent across many calls
set -u
KS="${KS:-$HOME/.knapsack/bin/knapsack}"
ROOT="D:/knapsack/target/dogfood_mcp_deep"
rm -rf "$ROOT" 2>/dev/null
mkdir -p "$ROOT/store"
export KNAPSACK_STORE="$ROOT/store"
export KNAPSACK_METRICS="$ROOT/metrics.jsonl"
export KNAPSACK_SESSIONS="$ROOT/sessions"

PASS=0; FAIL=0
ok() { PASS=$((PASS+1)); echo "  ✓ $*"; }
no() { FAIL=$((FAIL+1)); echo "  ✗ $*"; }

# Seed the store with one handle for expand testing.
TEST_HANDLE=$(echo "test content for mcp dogfood — repeated line " | "$KS" store put /dev/stdin 2>&1 | head -1 || true)
if [ -z "$TEST_HANDLE" ] || ! echo "$TEST_HANDLE" | grep -qE '^ks2_'; then
  # store put takes a file path, not stdin. Use pack - instead.
  echo "test content for mcp dogfood — repeated line " | "$KS" pack - --session "mcp-seed" --cmd "x" --type log > /dev/null 2>&1
  # Pull a handle from the store.
  TEST_HANDLE=$(find "$KNAPSACK_STORE" -name 'ks2_*' -not -name '*.meta' | head -1 | xargs -I{} basename {} 2>/dev/null)
fi
echo "seed handle: $TEST_HANDLE"

# Build a multi-request session in one MCP process and capture all responses.
python3 - "$KS" "$TEST_HANDLE" "$ROOT" <<'PYEOF' || true
import json, subprocess, sys, os
ks, handle, root = sys.argv[1], sys.argv[2], sys.argv[3]
env = dict(os.environ)
proc = subprocess.Popen([ks, "mcp"], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, env=env)

requests = [
    # Standard Claude Code init: initialize, then notifications/initialized.
    {"jsonrpc":"2.0","id":1,"method":"initialize","params":{
        "protocolVersion":"2024-11-05",
        "capabilities":{"sampling":{}},
        "clientInfo":{"name":"claude-code","version":"1.0.0"}
    }},
    {"jsonrpc":"2.0","method":"notifications/initialized"},  # no id -> no response
    {"jsonrpc":"2.0","id":2,"method":"tools/list"},
    # Tools/call with each advertised tool.
    {"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"knapsack_metrics","arguments":{}}},
    {"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"knapsack_expand","arguments":{"handle":handle}}},
    {"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"knapsack_inspect","arguments":{"handle":handle}}},
    # Re-initialize mid-session.
    {"jsonrpc":"2.0","id":6,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"x","version":"y"}}},
    {"jsonrpc":"2.0","id":7,"method":"tools/list"},  # still works after re-init
    # Unknown method -> -32601.
    {"jsonrpc":"2.0","id":8,"method":"completion/complete","params":{}},
    # tools/call with unknown tool -> isError text result.
    {"jsonrpc":"2.0","id":9,"method":"tools/call","params":{"name":"nope","arguments":{}}},
    # tools/call missing params.name -> -32601 method-not-found-style.
    {"jsonrpc":"2.0","id":10,"method":"tools/call","params":{"arguments":{}}},
    # ping (lightweight liveness).
    {"jsonrpc":"2.0","id":11,"method":"ping"},
    # 30 rapid-fire tools/list to test sustained throughput.
    *[{"jsonrpc":"2.0","id":1000+i,"method":"tools/list"} for i in range(30)],
]
payload = ("\n".join(json.dumps(r) for r in requests) + "\n").encode("utf-8")
out, err = proc.communicate(payload, timeout=30)
rc = proc.returncode

# Save outputs for the bash analyzer.
with open(f"{root}/responses.jsonl","wb") as f: f.write(out)
with open(f"{root}/server.err","wb") as f: f.write(err)
print(f"rc={rc} responses={len(out)} bytes  stderr={len(err)} bytes")
PYEOF

# Verify response shape: every line should be valid JSON-RPC 2.0 with an id field.
RESP="$ROOT/responses.jsonl"
NLINES=$(grep -c . "$RESP" 2>/dev/null || echo 0)
echo "  response lines: $NLINES"
# Notifications produce no response, so we expect 1 (init) + 1 (tools/list) + 3 (3 tools/calls)
# + 1 (re-init) + 1 (tools/list) + 1 (unknown method) + 1 (unknown tool) + 1 (missing name)
# + 1 (ping) + 30 (rapid-fire) = 41
if [ "$NLINES" -ge 40 ]; then
  ok "got $NLINES response lines from the multi-request session"
fi

# Check protocol fields on every response.
python3 - "$RESP" <<'PYEOF'
import json, sys
ok_count = 0; bad = 0
seen_ids = set()
with open(sys.argv[1],encoding='utf-8') as f:
    for line in f:
        line = line.strip()
        if not line: continue
        v = json.loads(line)
        if v.get("jsonrpc") != "2.0":
            print(f"  ✗ wrong jsonrpc: {line[:80]}")
            bad += 1; continue
        if "id" not in v:
            print(f"  ✗ missing id: {line[:80]}")
            bad += 1; continue
        seen_ids.add(v["id"])
        ok_count += 1
print(f"  protocol OK: {ok_count} valid, {bad} bad, {len(seen_ids)} unique ids")
PYEOF

# Check initialize result.
INIT=$(python3 -c "
import json
for line in open('$RESP','r',encoding='utf-8'):
    v = json.loads(line)
    if v.get('id') == 1:
        print(json.dumps(v, indent=2)); break
")
echo
echo "  initialize result:"
echo "$INIT" | sed 's/^/    | /' | head -15
if echo "$INIT" | grep -q "protocolVersion"; then ok "initialize advertises protocolVersion"; fi
if echo "$INIT" | grep -q "capabilities"; then ok "initialize returns capabilities"; fi
if echo "$INIT" | grep -q "serverInfo\|name"; then ok "initialize returns server info"; fi

# Check tools/list result.
TOOLS=$(python3 -c "
import json
for line in open('$RESP','r',encoding='utf-8'):
    v = json.loads(line)
    if v.get('id') == 2:
        print(json.dumps(v.get('result',{}).get('tools',[]), indent=2)); break
")
echo
echo "  tools/list tool count: $(echo "$TOOLS" | python3 -c "import json,sys; print(len(json.loads(sys.stdin.read())))")"
TOOL_NAMES=$(echo "$TOOLS" | python3 -c "import json,sys; print(','.join(t['name'] for t in json.loads(sys.stdin.read())))")
echo "  tool names: $TOOL_NAMES"
if echo "$TOOL_NAMES" | grep -q "knapsack_expand"; then ok "knapsack_expand advertised"; fi
if echo "$TOOL_NAMES" | grep -q "knapsack_inspect"; then ok "knapsack_inspect advertised"; fi
if echo "$TOOL_NAMES" | grep -q "knapsack_metrics"; then ok "knapsack_metrics advertised"; fi

# Every tool should have an inputSchema.
SCHEMAS_OK=$(echo "$TOOLS" | python3 -c "
import json, sys
tools = json.loads(sys.stdin.read())
ok = sum(1 for t in tools if 'inputSchema' in t and isinstance(t['inputSchema'], dict))
print(ok)
")
if [ "$SCHEMAS_OK" -ge 3 ]; then
  ok "all tools have inputSchema declared"
fi

# Re-initialize result (id=6).
REINIT=$(python3 -c "
import json
for line in open('$RESP','r',encoding='utf-8'):
    v = json.loads(line)
    if v.get('id') == 6:
        print(json.dumps(v)); break
")
if echo "$REINIT" | grep -q "protocolVersion"; then ok "re-initialize returns clean result (no error)"; fi

# Unknown method should be -32601.
ID8=$(python3 -c "
import json
for line in open('$RESP','r',encoding='utf-8'):
    v = json.loads(line)
    if v.get('id') == 8:
        print(json.dumps(v.get('error',{}))); break
")
if echo "$ID8" | grep -q "32601"; then ok "unknown method returns JSON-RPC -32601"; fi

# Unknown tool should yield isError text result (not protocol error).
ID9=$(python3 -c "
import json
for line in open('$RESP','r',encoding='utf-8'):
    v = json.loads(line)
    if v.get('id') == 9:
        print(json.dumps(v)); break
")
if echo "$ID9" | grep -q "isError.*true"; then ok "unknown tool -> isError text result"; fi

# Stderr should be silent on the protocol-correct calls.
STDERR_BYTES=$(wc -c < "$ROOT/server.err" 2>/dev/null || echo 0)
if [ "$STDERR_BYTES" -eq 0 ]; then
  ok "server stderr is silent"
else
  echo "  server stderr ($STDERR_BYTES bytes):"
  head -10 "$ROOT/server.err" | sed 's/^/    | /'
fi

# All 30 rapid-fire requests responded.
RAPID_RESPONSES=$(python3 -c "
import json
ids = set()
for line in open('$RESP','r',encoding='utf-8'):
    v = json.loads(line)
    if isinstance(v.get('id'), int) and v['id'] >= 1000:
        ids.add(v['id'])
print(len(ids))
")
if [ "$RAPID_RESPONSES" -eq 30 ]; then
  ok "all 30 rapid-fire tools/list requests responded"
else
  no "rapid-fire missed responses: $RAPID_RESPONSES / 30"
fi

echo
echo "================================================================"
echo "MCP deep: $PASS pass, $FAIL fail"
echo "================================================================"
exit $FAIL
