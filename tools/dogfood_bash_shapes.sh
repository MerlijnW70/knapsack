#!/usr/bin/env bash
# Bash hook command-shape matrix. The hook decides whether to wrap a Bash command
# in `knapsack pack -` based on the program name and shell-meta heuristics. We test
# every common command shape Claude Code can emit, including malformed envelopes,
# to make sure wrapping is correct and the hook always fails open.
#
# Contract being tested:
#   - Allowlisted noisy programs (cargo, npm, etc.) WITH no shell meta -> wrapped
#   - Shell meta (pipes, redirects, background, comments) -> NOT wrapped
#   - Quiet commands (ls, pwd, echo without long output) -> not wrapped
#   - Already-knapsack-piped -> not wrapped (no double-wrap)
#   - Unknown tool name -> empty output (pass-through)
#   - Malformed JSON -> empty output, no panic
#   - The original command's exit code semantics are preserved post-wrap
set -u
KS="${KS:-$HOME/.knapsack/bin/knapsack}"
ROOT="D:/knapsack/target/dogfood_bash"
rm -rf "$ROOT" 2>/dev/null
mkdir -p "$ROOT"

PASS=0; FAIL=0
ok() { PASS=$((PASS+1)); echo "  ✓ $*"; }
no() { FAIL=$((FAIL+1)); echo "  ✗ $*"; }

# Drive the bash hook with a JSON envelope, return what it printed.
hook_run() {
  local envelope="$1"
  echo "$envelope" | "$KS" hook 2>"$ROOT/err.log"
}

# Test whether the hook EMITTED a wrap (non-empty JSON output with updatedInput).
expect_wrap() {
  local label="$1" envelope="$2"
  local out
  out=$(hook_run "$envelope")
  if [ -z "$out" ]; then
    no "$label: expected wrap, got empty (pass-through)"
    return
  fi
  if echo "$out" | python3 -c "
import json, sys
v = json.loads(sys.stdin.read())
u = v.get('hookSpecificOutput', {}).get('updatedInput', {})
new = u.get('command', '')
import re
sys.exit(0 if re.search(r'knapsack\\b.*\\bpack\\b', new) else 1)
"; then
    ok "$label: wrapped via knapsack pack"
  else
    no "$label: wrap envelope emitted but command doesn't contain 'knapsack pack'"
    echo "    | $out"
  fi
}

expect_passthrough() {
  local label="$1" envelope="$2"
  local out
  out=$(hook_run "$envelope")
  if [ -z "$out" ]; then
    ok "$label: pass-through (empty output)"
  else
    no "$label: expected pass-through, got wrap"
    echo "    | $out"
  fi
}

# ---- Wrappable noisy commands ----
echo "[A] noisy allowlisted commands — should wrap"
expect_wrap "cargo test"      '{"tool_name":"Bash","tool_input":{"command":"cargo test"},"session_id":"S1"}'
expect_wrap "cargo build"     '{"tool_name":"Bash","tool_input":{"command":"cargo build --release"},"session_id":"S2"}'
expect_wrap "npm install"     '{"tool_name":"Bash","tool_input":{"command":"npm install --production"},"session_id":"S3"}'
expect_wrap "pytest"          '{"tool_name":"Bash","tool_input":{"command":"pytest -v"},"session_id":"S4"}'
expect_wrap "jest"            '{"tool_name":"Bash","tool_input":{"command":"jest --runInBand"},"session_id":"S5"}'
expect_wrap "go build"        '{"tool_name":"Bash","tool_input":{"command":"go build ./..."},"session_id":"S6"}'
expect_wrap "docker run"      '{"tool_name":"Bash","tool_input":{"command":"docker run -d nginx"},"session_id":"S7"}'

# ---- Chains and combinations ----
echo
echo "[B] chained commands"
expect_wrap "cargo && npm chained"   '{"tool_name":"Bash","tool_input":{"command":"cargo build && npm test"},"session_id":"S"}'
expect_wrap "; chain"                '{"tool_name":"Bash","tool_input":{"command":"cargo check; cargo test"},"session_id":"S"}'

# ---- Shell meta — should NOT wrap ----
echo
echo "[C] shell-meta commands — must NOT wrap"
expect_passthrough "pipe"                  '{"tool_name":"Bash","tool_input":{"command":"cargo test | head"},"session_id":"S"}'
expect_passthrough "redirect stdout"       '{"tool_name":"Bash","tool_input":{"command":"cargo test > out.log"},"session_id":"S"}'
expect_passthrough "redirect stderr"       '{"tool_name":"Bash","tool_input":{"command":"cargo test 2> err.log"},"session_id":"S"}'
expect_passthrough "input redirect"        '{"tool_name":"Bash","tool_input":{"command":"sort < input.txt"},"session_id":"S"}'
expect_passthrough "background &"          '{"tool_name":"Bash","tool_input":{"command":"cargo build &"},"session_id":"S"}'
expect_passthrough "comment"               '{"tool_name":"Bash","tool_input":{"command":"cargo test # quiet build"},"session_id":"S"}'
expect_passthrough "heredoc input"         '{"tool_name":"Bash","tool_input":{"command":"cat <<EOF\nhi\nEOF"},"session_id":"S"}'

# ---- Already wrapped / contains knapsack reference ----
echo
echo "[D] already-knapsack commands — must NOT double-wrap"
expect_passthrough "knapsack pipe"     '{"tool_name":"Bash","tool_input":{"command":"cargo test 2>&1 | knapsack pack -"},"session_id":"S"}'
expect_passthrough "rucksack pipe"     '{"tool_name":"Bash","tool_input":{"command":"cargo test 2>&1 | rucksack pack -"},"session_id":"S"}'

# ---- Wrappers (sudo, env, time, nice) ----
echo
echo "[E] wrapper commands (sudo, env, time, nice, npx)"
expect_wrap "sudo cargo"   '{"tool_name":"Bash","tool_input":{"command":"sudo cargo test"},"session_id":"S"}'
expect_wrap "env cargo"    '{"tool_name":"Bash","tool_input":{"command":"env RUST_LOG=trace cargo test"},"session_id":"S"}'
expect_wrap "time cargo"   '{"tool_name":"Bash","tool_input":{"command":"time cargo test"},"session_id":"S"}'
expect_wrap "npx jest"     '{"tool_name":"Bash","tool_input":{"command":"npx jest"},"session_id":"S"}'

# ---- Quiet commands — should NOT wrap (not on allowlist) ----
echo
echo "[F] quiet/unknown commands — not wrapped"
expect_passthrough "ls"        '{"tool_name":"Bash","tool_input":{"command":"ls -la"},"session_id":"S"}'
expect_passthrough "pwd"       '{"tool_name":"Bash","tool_input":{"command":"pwd"},"session_id":"S"}'
expect_passthrough "echo"      '{"tool_name":"Bash","tool_input":{"command":"echo hello"},"session_id":"S"}'
expect_passthrough "git status" '{"tool_name":"Bash","tool_input":{"command":"git status"},"session_id":"S"}'  # not on allowlist
expect_passthrough "unknown"   '{"tool_name":"Bash","tool_input":{"command":"my-custom-tool --flag"},"session_id":"S"}'

# ---- Quotes around metachars ----
echo
echo "[G] metachars inside quotes are NOT real shell meta"
expect_wrap "quoted pipe"    '{"tool_name":"Bash","tool_input":{"command":"rg \"a|b\" src/"},"session_id":"S"}'  # rg is allowlisted (EXTRA), and the | is inside quotes
expect_wrap "quoted arrow"   '{"tool_name":"Bash","tool_input":{"command":"grep \">\" file.txt"},"session_id":"S"}' # grep is in EXTRA list

# ---- Unknown tool name — pass-through ----
echo
echo "[H] non-Bash tools — pass-through"
expect_passthrough "Edit tool"     '{"tool_name":"Edit","tool_input":{"file_path":"/tmp/x"}}'
expect_passthrough "Write tool"    '{"tool_name":"Write","tool_input":{"file_path":"/tmp/x"}}'
expect_passthrough "Grep tool"     '{"tool_name":"Grep","tool_input":{"pattern":"foo"}}'
expect_passthrough "no tool_name"  '{"tool_input":{"command":"cargo test"}}'
expect_passthrough "empty"         ''
expect_passthrough "garbage"       'not json at all'

# ---- Malformed Bash payloads — pass-through ----
echo
echo "[I] malformed Bash payloads"
expect_passthrough "missing command"  '{"tool_name":"Bash","tool_input":{}}'
expect_passthrough "numeric command"  '{"tool_name":"Bash","tool_input":{"command":123}}'
expect_passthrough "null command"     '{"tool_name":"Bash","tool_input":{"command":null}}'
expect_passthrough "array command"    '{"tool_name":"Bash","tool_input":{"command":["cargo","test"]}}'

# ---- Session id variations ----
echo
echo "[J] session_id variations"
# Missing session_id falls back to a cwd+day key. Wrap still happens.
expect_wrap "no session_id"   '{"tool_name":"Bash","tool_input":{"command":"cargo test"},"cwd":"/tmp"}'
expect_wrap "empty session_id" '{"tool_name":"Bash","tool_input":{"command":"cargo test"},"session_id":""}'
expect_wrap "numeric session_id" '{"tool_name":"Bash","tool_input":{"command":"cargo test"},"session_id":42}'

# ---- Output shape: emitted JSON is well-formed ----
echo
echo "[K] emitted wrap envelope is valid JSON-RPC-shape"
OUT=$(hook_run '{"tool_name":"Bash","tool_input":{"command":"cargo test"},"session_id":"S"}')
if echo "$OUT" | python3 -c "
import json, sys
v = json.loads(sys.stdin.read())
assert 'hookSpecificOutput' in v
hso = v['hookSpecificOutput']
assert hso.get('hookEventName') == 'PreToolUse', f'wrong event: {hso}'
assert 'updatedInput' in hso, 'missing updatedInput'
u = hso['updatedInput']
assert 'command' in u, 'missing command'
"; then
  ok "wrap envelope shape matches Claude Code's PreToolUse contract"
else
  no "wrap envelope shape broken"
fi

# Look for the actual wrap content
NEW_CMD=$(echo "$OUT" | python3 -c "
import json, sys
print(json.loads(sys.stdin.read())['hookSpecificOutput']['updatedInput']['command'])
")
if echo "$NEW_CMD" | grep -qE "knapsack.*pack - --session"; then
  ok "wrap preserves session via --session flag"
else
  no "wrap doesn't carry --session"
fi
if echo "$NEW_CMD" | grep -qE 'echo \$\? > '; then
  ok "wrap preserves exit code via temp file"
else
  no "wrap doesn't capture exit code"
fi

# ---- Stderr is silent on wraps ----
echo
echo "[L] hook never writes to stderr on wrap success"
hook_run '{"tool_name":"Bash","tool_input":{"command":"cargo test"},"session_id":"S"}' > /dev/null
if [ -s "$ROOT/err.log" ]; then
  no "stderr non-empty: $(cat "$ROOT/err.log")"
else
  ok "stderr is empty on successful wrap"
fi

# ---- panic-free under fuzz ----
echo
echo "[M] hook never panics under randomized inputs"
panic_count=0
for i in $(seq 1 50); do
  ENV=$(python3 -c "
import json, random, string
random.seed($i)
# Generate semi-realistic random envelopes
shape = random.randint(0, 4)
if shape == 0:
    v = {'tool_name': random.choice(['Bash','Read','Edit','Foo','']),
         'tool_input': {'command': ''.join(random.choices(string.printable, k=random.randint(0,50)))}}
elif shape == 1:
    v = {'tool_name': 'Bash', 'tool_input': {'command': random.choice(['cargo test','npm i','rm -rf /','ls > x.log','echo']*1)}}
elif shape == 2:
    v = {'tool_name': 'Bash', 'tool_input': {}}
elif shape == 3:
    v = {'random_field': random.randint(0, 10000)}
else:
    v = []
print(json.dumps(v))
")
  hook_run "$ENV" > /dev/null
  if grep -q "panicked" "$ROOT/err.log"; then
    panic_count=$((panic_count+1))
  fi
done
if [ "$panic_count" -eq 0 ]; then
  ok "50 randomized envelopes, 0 panics"
else
  no "$panic_count of 50 envelopes panicked"
fi

echo
echo "================================================================"
echo "Bash hook shapes: $PASS pass, $FAIL fail"
echo "================================================================"
exit $FAIL
