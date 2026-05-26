#!/usr/bin/env bash
# Install round-trip stress: install -> verify -> uninstall -> verify -> install -> ...
# Use a synthetic ~/.claude to avoid touching the real one. Make sure user's unrelated
# config (model, other hooks, other MCP servers, top-level keys) survives every cycle.
set -u
KS="${KS:-$HOME/.knapsack/bin/knapsack}"
ROOT="D:/knapsack/target/dogfood_install"
rm -rf "$ROOT" 2>/dev/null
mkdir -p "$ROOT"

# We use the public install primitives via separate small Rust-binary calls in the
# /tests/lifecycle.rs path. Here we're driving the user-facing CLI directly to make
# sure the SHELL flow works too.
PASS=0; FAIL=0
ok() { PASS=$((PASS+1)); echo "  ✓ $*"; }
no() { FAIL=$((FAIL+1)); echo "  ✗ $*"; }

# Build a synthetic ~/.claude with unrelated user data we don't want clobbered.
SETTINGS="$ROOT/.claude/settings.json"
CLAUDE_JSON="$ROOT/.claude.json"
mkdir -p "$(dirname "$SETTINGS")"
cat > "$SETTINGS" <<'EOF'
{
  "model": "opus",
  "theme": "dark",
  "hooks": {
    "PreToolUse": [
      { "matcher": "Edit", "hooks": [{"type": "command", "command": "echo user-hook"}] }
    ]
  }
}
EOF
cat > "$CLAUDE_JSON" <<'EOF'
{
  "userId": "abc-123",
  "mcpServers": {
    "other": { "command": "/usr/bin/other-mcp", "args": ["x", "y"] }
  },
  "numFavorites": 5
}
EOF

# Snapshot to compare against later.
cp "$SETTINGS" "$ROOT/settings.snapshot.json"
cp "$CLAUDE_JSON" "$ROOT/claude.snapshot.json"

# Make the binary install into THIS test's synthetic config tree by overriding HOME
# and USERPROFILE.
export HOME="$ROOT"
export USERPROFILE="$ROOT"

cycle() {
  local label="$1"
  echo
  echo "── cycle: $label ──"
  "$KS" install > "$ROOT/install.log" 2>&1
  if grep -qE "^Healthy ✓" "$ROOT/install.log"; then ok "install reports healthy"; else
    no "install did not report healthy"
    cat "$ROOT/install.log" | sed 's/^/    | /'
  fi
  # The hook + MCP should be present.
  if grep -qE '"command".*knapsack' "$SETTINGS"; then ok "hook present in settings"; else no "hook missing"; fi
  if grep -qE '"knapsack"' "$CLAUDE_JSON"; then ok "MCP server present"; else no "MCP missing"; fi
  # User's unrelated data preserved.
  python3 - "$SETTINGS" "$CLAUDE_JSON" <<'PYEOF'
import json,sys
s = json.load(open(sys.argv[1]))
c = json.load(open(sys.argv[2]))
ok = True
if s.get("model") != "opus": print("  ✗ model lost"); ok = False
if s.get("theme") != "dark": print("  ✗ theme lost"); ok = False
edit_hook = any(
    h.get("matcher") == "Edit"
    for h in s.get("hooks", {}).get("PreToolUse", [])
)
if not edit_hook: print("  ✗ user's Edit hook lost"); ok = False
if c.get("userId") != "abc-123": print("  ✗ userId lost"); ok = False
if c.get("numFavorites") != 5: print("  ✗ numFavorites lost"); ok = False
if "other" not in c.get("mcpServers", {}): print("  ✗ other MCP server lost"); ok = False
print("  ✓ all unrelated user config preserved" if ok else "  ✗ user config corrupted")
PYEOF

  # Doctor should be healthy in the synthetic tree.
  "$KS" doctor > "$ROOT/doctor.log" 2>&1
  if grep -qE "Healthy ✓" "$ROOT/doctor.log"; then ok "doctor reports healthy"; else
    no "doctor not healthy"
    cat "$ROOT/doctor.log" | sed 's/^/    | /'
  fi
}

# Run cycle 3 times to test idempotence and the install-after-install case.
cycle "first install"
cycle "second install (idempotent)"
cycle "third install (idempotent again)"

# Now uninstall.
echo
echo "── uninstall ──"
"$KS" uninstall > "$ROOT/uninstall.log" 2>&1
if grep -q "knapsack uninstall" "$ROOT/uninstall.log"; then ok "uninstall ran"; else no "uninstall did not run"; fi
if ! grep -qE '"command".*knapsack' "$SETTINGS"; then ok "hook removed from settings"; else no "hook still present after uninstall"; fi
if ! grep -qE '"knapsack"' "$CLAUDE_JSON"; then ok "MCP removed"; else no "MCP still present after uninstall"; fi
# User's unrelated data still preserved
python3 - "$SETTINGS" "$CLAUDE_JSON" <<'PYEOF'
import json,sys
s = json.load(open(sys.argv[1]))
c = json.load(open(sys.argv[2]))
ok = True
if s.get("model") != "opus": print("  ✗ model lost during uninstall"); ok = False
if s.get("theme") != "dark": print("  ✗ theme lost"); ok = False
edit_hook = any(
    h.get("matcher") == "Edit"
    for h in s.get("hooks", {}).get("PreToolUse", [])
)
if not edit_hook: print("  ✗ user's Edit hook lost"); ok = False
if c.get("userId") != "abc-123": print("  ✗ userId lost"); ok = False
if "other" not in c.get("mcpServers", {}): print("  ✗ other MCP server lost"); ok = False
print("  ✓ user config preserved through uninstall" if ok else "  ✗ user config harmed by uninstall")
PYEOF

# Re-install after uninstall.
echo
echo "── re-install after uninstall ──"
"$KS" install > "$ROOT/install2.log" 2>&1
if grep -qE "Healthy ✓" "$ROOT/install2.log"; then ok "re-install healthy"; else no "re-install not healthy"; fi
if grep -qE '"command".*knapsack' "$SETTINGS"; then ok "hook restored"; else no "hook not restored"; fi

# Test --repair (with stale binary path)
echo
echo "── repair when settings point at stale binary ──"
# Edit settings to point at a non-existent binary
python3 - "$SETTINGS" <<'PYEOF'
import json,sys
p = sys.argv[1]
s = json.load(open(p))
for h in s.get("hooks",{}).get("PreToolUse",[]):
    if h.get("matcher") == "Bash":
        for hh in h.get("hooks", []):
            hh["command"] = '"C:/old/stale/knapsack.exe" hook'
open(p,'w').write(json.dumps(s, indent=2))
PYEOF
"$KS" install --repair > "$ROOT/repair.log" 2>&1
if grep -qE "Healthy ✓" "$ROOT/repair.log"; then ok "repair runs"; else no "repair didn't run cleanly"; fi
if grep -qE '"command".*\\.knapsack' "$SETTINGS"; then ok "stale path rewritten"; else
  no "stale path not rewritten"
  grep -E '"command"' "$SETTINGS" | sed 's/^/    | /'
fi

echo
echo "================================================================"
echo "Install round-trip: $PASS pass, $FAIL fail"
echo "================================================================"
exit $FAIL
