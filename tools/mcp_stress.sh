#!/usr/bin/env bash
# MCP stdio stress: drive `knapsack mcp` with edge JSON-RPC frames and verify every
# request gets a syntactically valid JSON-RPC 2.0 response (or no response, for
# notifications/garbage). The server must never crash the stream.
set -u
KS="${KS:-$HOME/.knapsack/bin/knapsack}"
TMPROOT=$(mktemp -d)
export KNAPSACK_STORE="$TMPROOT/store"
export KNAPSACK_METRICS="$TMPROOT/m.jsonl"

# Build a barrage of frames. Each line is one JSON-RPC message.
{
  # Two initializes back-to-back (idempotent expected).
  echo '{"id":1,"jsonrpc":"2.0","method":"initialize"}'
  echo '{"id":2,"jsonrpc":"2.0","method":"initialize"}'

  # Tools list, then a tools call that should succeed.
  echo '{"id":3,"jsonrpc":"2.0","method":"tools/list"}'
  echo '{"id":4,"jsonrpc":"2.0","method":"tools/call","params":{"name":"knapsack_metrics","arguments":{}}}'

  # Garbage frames mixed in — server must skip without dying.
  echo 'not json at all'
  echo '{'
  echo ''
  echo '   '
  echo 'null'
  echo '[]'

  # Notification (no id) — no response expected.
  echo '{"jsonrpc":"2.0","method":"notifications/initialized"}'

  # Unknown method WITH id — must yield -32601.
  echo '{"id":5,"jsonrpc":"2.0","method":"nope/method"}'

  # Tools/call with unknown tool name.
  echo '{"id":6,"jsonrpc":"2.0","method":"tools/call","params":{"name":"no_such_tool","arguments":{}}}'

  # expand with junk handle.
  echo '{"id":7,"jsonrpc":"2.0","method":"tools/call","params":{"name":"knapsack_expand","arguments":{"handle":"junk"}}}'

  # expand with wrong-typed handle.
  echo '{"id":8,"jsonrpc":"2.0","method":"tools/call","params":{"name":"knapsack_expand","arguments":{"handle":123}}}'

  # inspect missing arg.
  echo '{"id":9,"jsonrpc":"2.0","method":"tools/call","params":{"name":"knapsack_inspect","arguments":{}}}'

  # A huge-string argument (~1 MB).
  python3 -c "
import json
print(json.dumps({'id':10,'jsonrpc':'2.0','method':'tools/call','params':{'name':'knapsack_expand','arguments':{'handle':'x'*1000000}}}))"

  # String id, int id, null id — all valid, all must echo back same type.
  echo '{"id":"s-1","jsonrpc":"2.0","method":"tools/list"}'
  echo '{"id":0,"jsonrpc":"2.0","method":"tools/list"}'
  echo '{"id":null,"jsonrpc":"2.0","method":"tools/list"}'

  # Server should keep responding after every one of those.
  echo '{"id":99,"jsonrpc":"2.0","method":"tools/list"}'
} | timeout 30 "$KS" mcp > "$TMPROOT/responses.jsonl" 2>"$TMPROOT/stderr.log"
RC=$?

# Crash check.
if grep -q 'panicked' "$TMPROOT/stderr.log"; then
  echo "PANIC detected:"; cat "$TMPROOT/stderr.log"; exit 1
fi
if [ "$RC" -eq 124 ]; then
  echo "HANG: server did not exit on EOF"; exit 1
fi

# Validate every response line is parseable JSON, jsonrpc=2.0, has an id field.
bad=0; ok=0
while IFS= read -r line; do
  [ -z "$line" ] && continue
  if ! python3 -c "
import json,sys
v = json.loads(sys.argv[1])
assert v.get('jsonrpc') == '2.0', f'wrong jsonrpc: {v}'
assert 'id' in v, f'missing id: {v}'
" "$line" >/dev/null 2>&1; then
    bad=$((bad+1))
    echo "BAD LINE: $line"
  else
    ok=$((ok+1))
  fi
done < "$TMPROOT/responses.jsonl"

echo
echo "Stream-level: ok=$ok bad=$bad   (rc=$RC, stderr lines=$(wc -l < "$TMPROOT/stderr.log"))"
echo "Server stderr (first 30 lines):"
head -30 "$TMPROOT/stderr.log" | sed 's/^/  | /'

rm -rf "$TMPROOT"
exit $bad
