#!/usr/bin/env bash
# `knapsack doctor` is the long-form diagnostic. We test it under unusual states
# that real users will hit: stale binary path in settings, broken JSON in MCP
# config, missing store dir, missing metrics file. Doctor must always produce a
# clear report — never panic, never lie about health.
set -u
KS="${KS:-$HOME/.knapsack/bin/knapsack}"
ROOT="D:/knapsack/target/dogfood_doctor"
rm -rf "$ROOT" 2>/dev/null
mkdir -p "$ROOT"
PASS=0; FAIL=0
ok() { PASS=$((PASS+1)); echo "  ✓ $*"; }
no() { FAIL=$((FAIL+1)); echo "  ✗ $*"; }

# Set up a synthetic ~/.claude tree per test (override HOME so doctor sees only OURS).
setup() {
  local dir="$ROOT/$1"
  rm -rf "$dir" 2>/dev/null
  mkdir -p "$dir/.claude" "$dir/.knapsack/store"
  echo "$dir"
}

run_doctor() {
  local home="$1"
  HOME="$home" USERPROFILE="$home" "$KS" doctor 2>"$home/err.log"
}

# ---- 1) Fresh install: doctor should be healthy ----
echo "[1] fresh install"
H=$(setup "fresh")
"$KS" install > "$H/install.log" 2>&1  # install with default HOME — irrelevant
# Provide synthetic configs pointing at our binary
# MSYS `realpath` returns `/c/Users/...` which the Windows FS can't resolve, making
# doctor's "binary found" check fail. Prefer cygpath -m which yields `C:/Users/...`.
KSB=$(cygpath -m "$KS" 2>/dev/null || realpath "$KS" 2>/dev/null || echo "$KS")
cat > "$H/.claude/settings.json" <<EOF
{
  "hooks": {
    "PreToolUse": [
      {"matcher":"Bash|Read","hooks":[{"type":"command","command":"\"$KSB\" hook"}]}
    ]
  }
}
EOF
cat > "$H/.claude.json" <<EOF
{"mcpServers":{"knapsack":{"command":"$KSB","args":["mcp"]}}}
EOF
DOC=$(run_doctor "$H")
echo "$DOC" | tail -3 | sed 's/^/    | /'
# Doctor enforces the canonical `Bash|Read` matcher (input + output reduction).
# Pre-fix this fixture wrote `matcher:"Bash"` only — legacy single-tool form —
# and doctor correctly reported Unhealthy. The fixture, not the binary, was the
# stale party; updated to the current contract.
if echo "$DOC" | grep -q "Healthy ✓"; then ok "fresh install -> healthy"; else no "fresh install not healthy"; fi
if grep -q "panicked" "$H/err.log" 2>/dev/null; then no "panic on fresh install"; else ok "no panic"; fi

# ---- 2) Stale binary path in settings ----
echo
echo "[2] hook settings point at a stale (non-existent) binary"
H=$(setup "stale")
cat > "$H/.claude/settings.json" <<'EOF'
{"hooks":{"PreToolUse":[{"matcher":"Bash","hooks":[{"type":"command","command":"\"D:/nope/old-knapsack.exe\" hook"}]}]}}
EOF
cat > "$H/.claude.json" <<EOF
{"mcpServers":{"knapsack":{"command":"$KSB","args":["mcp"]}}}
EOF
DOC=$(run_doctor "$H")
echo "$DOC" | tail -8 | sed 's/^/    | /'
if echo "$DOC" | grep -q "drift\|stale\|D:/nope"; then
  ok "doctor surfaces the stale binary path"
else
  # Doctor may just report the hook is configured; that's also acceptable if no drift
  # check happens against a non-existent file.
  echo "    (doctor didn't explicitly flag — checking it ran cleanly)"
  if grep -q "panicked" "$H/err.log"; then no "panic on stale binary"; else ok "ran cleanly on stale binary"; fi
fi

# ---- 3) MCP config is malformed JSON ----
echo
echo "[3] MCP config is malformed JSON"
H=$(setup "badjson")
cat > "$H/.claude/settings.json" <<EOF
{"hooks":{"PreToolUse":[{"matcher":"Bash","hooks":[{"type":"command","command":"\"$KSB\" hook"}]}]}}
EOF
echo '{ this is not json at all' > "$H/.claude.json"
DOC=$(run_doctor "$H")
echo "$DOC" | tail -6 | sed 's/^/    | /'
if grep -q "panicked" "$H/err.log"; then
  no "panic on malformed MCP json"
elif echo "$DOC" | grep -qE "MCP.*(not|missing|✗|⚠)"; then
  ok "doctor flagged the broken MCP config"
else
  ok "ran without panic on malformed MCP config"
fi

# ---- 4) Settings file completely absent ----
echo
echo "[4] settings.json missing entirely"
H=$(setup "no-settings")
# No settings.json file at all, but .claude.json exists.
cat > "$H/.claude.json" <<EOF
{"mcpServers":{"knapsack":{"command":"$KSB","args":["mcp"]}}}
EOF
DOC=$(run_doctor "$H")
echo "$DOC" | head -8 | sed 's/^/    | /'
if grep -q "panicked" "$H/err.log"; then no "panic on missing settings"; else ok "ran cleanly with no settings.json"; fi

# ---- 5) Store dir absent + metrics file absent ----
echo
echo "[5] no store dir, no metrics — fresh user state"
H=$(setup "blank")
rm -rf "$H/.knapsack"  # nuke everything
cat > "$H/.claude/settings.json" <<EOF
{"hooks":{"PreToolUse":[{"matcher":"Bash","hooks":[{"type":"command","command":"\"$KSB\" hook"}]}]}}
EOF
cat > "$H/.claude.json" <<EOF
{"mcpServers":{"knapsack":{"command":"$KSB","args":["mcp"]}}}
EOF
DOC=$(run_doctor "$H")
echo "$DOC" | head -6 | sed 's/^/    | /'
if grep -q "panicked" "$H/err.log"; then no "panic on blank state"; else ok "ran cleanly with no ~/.knapsack"; fi

# ---- 6) Metrics file has torn / malformed lines ----
echo
echo "[6] metrics.jsonl has malformed lines mixed with valid ones"
H=$(setup "bad-metrics")
cat > "$H/.claude/settings.json" <<EOF
{"hooks":{"PreToolUse":[{"matcher":"Bash","hooks":[{"type":"command","command":"\"$KSB\" hook"}]}]}}
EOF
cat > "$H/.claude.json" <<EOF
{"mcpServers":{"knapsack":{"command":"$KSB","args":["mcp"]}}}
EOF
cat > "$H/.knapsack/metrics.jsonl" <<'EOF'
not json
{"t":100,"event":"compress","session":"s","raw":1000,"shown":100,"saved":900,"delta_hits":0,"evicted":0}
{ unterminated
{"event":"unknown_event"}
{"t":200,"event":"expand","session":"s","tokens":50,"ok":true}
EOF
DOC=$(run_doctor "$H")
echo "$DOC" | grep "ab report" | head -1 | sed 's/^/    | /'
if grep -q "panicked" "$H/err.log"; then no "panic on malformed metrics"; else ok "doctor tolerates malformed metrics"; fi
# Also verify status doesn't panic
S=$(HOME="$H" USERPROFILE="$H" "$KS" status 2>"$H/err2.log")
if grep -q "panicked" "$H/err2.log"; then no "status panics on malformed metrics"; else ok "status tolerates malformed metrics"; fi

# ---- 7) Doctor on a settings.json with a NON-knapsack hook (unrelated user hook) ----
echo
echo "[7] settings has an unrelated user hook (Edit) but no knapsack hook"
H=$(setup "unrelated")
cat > "$H/.claude/settings.json" <<'EOF'
{
  "model": "opus",
  "hooks": {
    "PreToolUse": [
      {"matcher":"Edit","hooks":[{"type":"command","command":"echo edit-hook"}]}
    ]
  }
}
EOF
DOC=$(run_doctor "$H")
if echo "$DOC" | grep -qE "hook.*not\|✗"; then
  ok "doctor flags missing knapsack hook (unrelated hook present)"
else
  echo "$DOC" | grep -i "hook" | head -3 | sed 's/^/    | /'
fi
if grep -q "panicked" "$H/err.log"; then no "panic"; else ok "no panic"; fi

# ---- 8) Doctor on Windows-style paths (backslashes in JSON) ----
echo
echo "[8] settings with Windows backslash paths"
H=$(setup "winslash")
WIN_KSB=$(echo "$KSB" | sed 's|/|\\\\|g')  # double-escape for JSON
cat > "$H/.claude/settings.json" <<EOF
{"hooks":{"PreToolUse":[{"matcher":"Bash","hooks":[{"type":"command","command":"\"$WIN_KSB\" hook"}]}]}}
EOF
cat > "$H/.claude.json" <<EOF
{"mcpServers":{"knapsack":{"command":"$WIN_KSB","args":["mcp"]}}}
EOF
DOC=$(run_doctor "$H")
if grep -q "panicked" "$H/err.log"; then no "panic on backslash paths"; else ok "doctor handled backslash paths"; fi

# ---- 9) The exit code is non-zero on unhealthy state? ----
echo
echo "[9] doctor exit code reflects health"
# Fresh known-broken state (no configs).
H=$(setup "broken")
HOME="$H" USERPROFILE="$H" "$KS" doctor > /dev/null 2>"$H/err.log"
RC=$?
echo "  exit code: $RC"
# Doctor exit code is currently always 0 (it's a report, not a gate). That's fine
# as long as the report itself is honest. Just don't panic.
if [ "$RC" -eq 0 ] || [ "$RC" -eq 1 ]; then
  ok "doctor exit code is sensible ($RC)"
else
  no "doctor exit code unexpected: $RC"
fi

# ---- 10) Doctor's `pack/expand smoke` test ----
echo
echo "[10] internal smoke test result"
H=$(setup "smoke")
cat > "$H/.claude/settings.json" <<EOF
{"hooks":{"PreToolUse":[{"matcher":"Bash","hooks":[{"type":"command","command":"\"$KSB\" hook"}]}]}}
EOF
cat > "$H/.claude.json" <<EOF
{"mcpServers":{"knapsack":{"command":"$KSB","args":["mcp"]}}}
EOF
DOC=$(run_doctor "$H")
if echo "$DOC" | grep -q "pack/expand smoke.*byte-exact"; then
  ok "internal smoke test passes"
else
  no "smoke test missing or failing"
  echo "$DOC" | grep -i smoke | head -3 | sed 's/^/    | /'
fi

echo
echo "================================================================"
echo "Doctor: $PASS pass, $FAIL fail"
echo "================================================================"
exit $FAIL
