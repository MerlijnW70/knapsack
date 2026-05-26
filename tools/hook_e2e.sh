#!/usr/bin/env bash
# Drive `knapsack hook` with realistic PreToolUse envelopes and assert the output
# shape matches what Claude Code consumes: either empty (fail-open) or
# {"hookSpecificOutput":{"hookEventName":"PreToolUse","updatedInput":{...}}}.
set -u
KS="${KS:-$HOME/.knapsack/bin/knapsack}"
PASS=0; FAIL=0

run() {
  local label="$1"; local expect="$2"; shift 2
  local payload="$*"
  local out err
  out=$(mktemp); err=$(mktemp)
  echo "$payload" | timeout 10 "$KS" hook > "$out" 2>"$err"
  local rc=$?
  if grep -q "panicked" "$err"; then
    echo "PANIC  [$label]"; sed 's/^/  | /' "$err"; FAIL=$((FAIL+1))
  elif [ "$rc" -eq 124 ]; then
    echo "HANG   [$label]"; FAIL=$((FAIL+1))
  else
    local body=$(cat "$out")
    case "$expect" in
      empty)
        if [ -z "$body" ]; then
          echo "OK     [$label]   (empty -> pass-through)"; PASS=$((PASS+1))
        else
          echo "FAIL   [$label]   expected empty, got: ${body:0:200}"; FAIL=$((FAIL+1))
        fi
        ;;
      wrap)
        if python3 -c "
import json,sys
v = json.loads(sys.argv[1])
ok = (v.get('hookSpecificOutput',{}).get('hookEventName') == 'PreToolUse'
      and 'updatedInput' in v['hookSpecificOutput'])
sys.exit(0 if ok else 1)
" "$body" 2>/dev/null; then
          echo "OK     [$label]   (wrapped)"; PASS=$((PASS+1))
        else
          echo "FAIL   [$label]   expected wrap envelope, got: ${body:0:200}"; FAIL=$((FAIL+1))
        fi
        ;;
      any-json)
        if python3 -c "import json,sys; json.loads(sys.argv[1])" "$body" 2>/dev/null; then
          echo "OK     [$label]   (valid json: ${body:0:100})"; PASS=$((PASS+1))
        else
          echo "FAIL   [$label]   expected JSON, got: ${body:0:200}"; FAIL=$((FAIL+1))
        fi
        ;;
    esac
  fi
  rm -f "$out" "$err"
}

# Bash noisy: cargo test -> should wrap
run "bash cargo test"        wrap '{"tool_name":"Bash","tool_input":{"command":"cargo test"},"session_id":"S1","cwd":"/tmp"}'
# Bash noisy: npm install -> wrap
run "bash npm install"       wrap '{"tool_name":"Bash","tool_input":{"command":"npm install --production"},"session_id":"S1","cwd":"/tmp"}'
# Bash quiet: ls -> empty (no wrap)
run "bash ls"                empty '{"tool_name":"Bash","tool_input":{"command":"ls -la"},"session_id":"S1","cwd":"/tmp"}'
# Bash with pipe -> shell meta means hands-off
run "bash with pipe"         empty '{"tool_name":"Bash","tool_input":{"command":"cargo test | head"},"session_id":"S1","cwd":"/tmp"}'
# Bash with redirect
run "bash with redirect"     empty '{"tool_name":"Bash","tool_input":{"command":"cargo test > out.log"},"session_id":"S1","cwd":"/tmp"}'
# Bash with knapsack already
run "bash already knapsack"  empty '{"tool_name":"Bash","tool_input":{"command":"cargo test 2>&1 | knapsack pack -"},"session_id":"S1","cwd":"/tmp"}'
# Bash chained &&
run "bash chained && wrap"   wrap  '{"tool_name":"Bash","tool_input":{"command":"npm ci && npm test"},"session_id":"S1","cwd":"/tmp"}'
# Bash with comment
run "bash comment"           empty '{"tool_name":"Bash","tool_input":{"command":"cargo test # quiet"},"session_id":"S1","cwd":"/tmp"}'
# Bash backgrounded
run "bash backgrounded"      empty '{"tool_name":"Bash","tool_input":{"command":"cargo build &"},"session_id":"S1","cwd":"/tmp"}'

# Read tool: with KNAPSACK_READ_HOOK off, must be empty/pass-through
run "read tool gate off"     empty '{"tool_name":"Read","tool_input":{"file_path":"/tmp/whatever"}}'

# Edit / Write / Glob / Unknown -> empty
run "edit tool"              empty '{"tool_name":"Edit","tool_input":{"file_path":"/tmp/x","old_string":"a","new_string":"b"}}'
run "write tool"             empty '{"tool_name":"Write","tool_input":{"file_path":"/tmp/x","content":"hi"}}'
run "unknown tool"           empty '{"tool_name":"BlahBlah","tool_input":{}}'

# Broken / partial JSON -> empty
run "garbage stdin"          empty 'this is not json'
run "empty stdin"            empty ''

# Bash with missing command field
run "bash no command"        empty '{"tool_name":"Bash","tool_input":{}}'
# Bash with command as wrong type
run "bash numeric command"   empty '{"tool_name":"Bash","tool_input":{"command":123}}'

echo
echo "Hook E2E: $PASS pass, $FAIL fail"
exit $FAIL
