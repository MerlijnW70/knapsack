#!/usr/bin/env bash
# Content-type detection edge cases. The hook routes through detect() which picks
# Code / Log / Json based on extension + content sniffing. What happens when those
# signals disagree, are missing, or are adversarial? Verify the choice is honest
# (correct compressor) or the never-worse-than-raw guard catches it.
set -u
KS="${KS:-$HOME/.knapsack/bin/knapsack}"
ROOT="D:/knapsack/target/dogfood_ct"
rm -rf "$ROOT" 2>/dev/null
mkdir -p "$ROOT/cache" "$ROOT/store"
export KNAPSACK_STORE="$ROOT/store"
export KNAPSACK_READ_LOG="$ROOT/read_hook.jsonl"
export KNAPSACK_READ_CACHE="$ROOT/cache"
: > "$KNAPSACK_READ_LOG"

PASS=0; FAIL=0
ok() { PASS=$((PASS+1)); echo "  ✓ $*"; }
no() { FAIL=$((FAIL+1)); echo "  ✗ $*"; }

drive() {
  echo "{\"tool_name\":\"Read\",\"tool_input\":{\"file_path\":\"$1\"},\"session_id\":\"ct\"}" \
    | "$KS" hook 2>/dev/null \
    | python3 -c "
import json, sys
try:
  v=json.loads(sys.stdin.read())
  print(v['hookSpecificOutput']['updatedInput']['file_path'])
except Exception:
  pass
"
}
last_reason() {
  tail -n 1 "$KNAPSACK_READ_LOG" 2>/dev/null | python3 -c "
import json, sys
try: print(json.loads(sys.stdin.read()).get('reason',''))
except: print('')"
}
verify_round_trip() {
  local view="$1" source="$2" label="$3"
  local h=$(grep -aoE 'knapsack expand ks2_[0-9a-f]+' "$view" | head -1 | awk '{print $3}')
  [ -z "$h" ] && { no "$label: no handle"; return; }
  "$KS" expand "$h" > "$ROOT/_rt.bin" 2>/dev/null
  if cmp -s "$ROOT/_rt.bin" "$source"; then
    ok "$label: byte-exact round trip"
  else
    no "$label: round trip lost bytes"
  fi
}

mk_fix() { mkdir -p "$(dirname "$1")"; }

# --- 1) .json file with non-JSON contents (text that doesn't parse) ---
echo "[1] .json extension but content is plain text"
F="$ROOT/fixtures/textfile.json"; mk_fix "$F"
python3 - "$F" <<'PYEOF'
import sys
with open(sys.argv[1],'w') as f:
    for i in range(800):
        f.write(f"line {i}: this is not json at all but the extension says it is\n")
PYEOF
V=$(drive "$F")
R=$(last_reason)
echo "  reason=$R view_path=$([ -n "$V" ] && basename "$V")"
if [ -n "$V" ]; then verify_round_trip "$V" "$F" ".json-with-text"; else ok ".json-with-text -> pass-through ($R)"; fi

# --- 2) .rs file with prose docstring (mostly text, not code) ---
echo
echo "[2] .rs file dominated by doc-comment prose"
F="$ROOT/fixtures/proseheavy.rs"; mk_fix "$F"
python3 - "$F" <<'PYEOF'
import sys
with open(sys.argv[1],'w') as f:
    for i in range(40):
        f.write(f"//! Paragraph {i}: this module solves a difficult problem in a specific way that is\n")
        f.write(f"//! described carefully here, with notes on the trade-offs, the alternatives, and\n")
        f.write(f"//! the eventual choice that was made and why future readers should care about it.\n")
    f.write("\nuse std::fmt;\n\n")
    for i in range(20):
        f.write(f"pub fn fn_{i}(x: u32) -> u32 {{\n")
        for _ in range(5):
            f.write("    let _ = compute();\n")
        f.write(f"    x + {i}\n}}\n\n")
PYEOF
V=$(drive "$F")
R=$(last_reason)
echo "  reason=$R view_path=$([ -n "$V" ] && basename "$V")"
if [ -n "$V" ]; then verify_round_trip "$V" "$F" "prose-heavy-rs"; fi

# --- 3) File with NO extension, content looks like JSON ---
echo
echo "[3] no extension but content is JSON"
F="$ROOT/fixtures/anon_json"; mk_fix "$F"
python3 - "$F" <<'PYEOF'
import json, sys
data = {"items": [{"id": i, "name": f"item-{i}", "active": True} for i in range(400)]}
with open(sys.argv[1],'w') as f: json.dump(data, f, indent=2)
PYEOF
V=$(drive "$F")
R=$(last_reason)
echo "  reason=$R view_path=$([ -n "$V" ] && basename "$V")"
if [ -n "$V" ]; then verify_round_trip "$V" "$F" "anon-json"; fi

# --- 4) File with .txt extension but content is JSON ---
echo
echo "[4] .txt extension, content is JSON"
F="$ROOT/fixtures/disguised.txt"; mk_fix "$F"
python3 - "$F" <<'PYEOF'
import json, sys
data = {"items": [{"id": i, "data": "x"*50} for i in range(300)]}
with open(sys.argv[1],'w') as f: json.dump(data, f, indent=2)
PYEOF
V=$(drive "$F")
R=$(last_reason)
echo "  reason=$R view_path=$([ -n "$V" ] && basename "$V")"
if [ -n "$V" ]; then verify_round_trip "$V" "$F" "disguised-txt-json"; fi

# --- 5) JSON file with non-UTF-8 content ---
echo
echo "[5] .json file with non-UTF-8 bytes (latin-1)"
F="$ROOT/fixtures/latin1.json"; mk_fix "$F"
python3 - "$F" <<'PYEOF'
import sys
with open(sys.argv[1],'wb') as f:
    # Start with a JSON-ish opener, then write some latin-1 high bytes.
    f.write(b'{"data":"')
    f.write(bytes(range(192, 255)) * 200)  # high-byte stream
    f.write(b'"}\n')
PYEOF
V=$(drive "$F")
R=$(last_reason)
echo "  reason=$R view_path=$([ -n "$V" ] && basename "$V")"
if [ -n "$V" ]; then verify_round_trip "$V" "$F" "latin1-json"; fi

# --- 6) Ambiguous: starts with `{` but not valid JSON ---
echo
echo "[6] starts with '{' but isn't JSON (bash heredoc-ish)"
F="$ROOT/fixtures/braceopen.txt"; mk_fix "$F"
python3 - "$F" <<'PYEOF'
import sys
with open(sys.argv[1],'w') as f:
    f.write("{ this is not json, it just happens to start with a brace\n")
    for i in range(500):
        f.write(f"  some line {i} with text and {{braces}} and 'quotes'\n")
    f.write("}\n")
PYEOF
V=$(drive "$F")
R=$(last_reason)
echo "  reason=$R view_path=$([ -n "$V" ] && basename "$V")"
if [ -n "$V" ]; then verify_round_trip "$V" "$F" "brace-not-json"; fi

# --- 7) .py file (Python - listed in CODE_EXT?) ---
F="$ROOT/fixtures/script.py"; mk_fix "$F"
echo
echo "[7] .py file"
python3 - "$F" <<'PYEOF'
import sys
with open(sys.argv[1],'w') as f:
    for i in range(50):
        f.write(f"def func_{i}(x):\n    return x + {i}\n\n")
PYEOF
V=$(drive "$F")
R=$(last_reason)
echo "  reason=$R view_path=$([ -n "$V" ] && basename "$V")"
if [ -n "$V" ]; then verify_round_trip "$V" "$F" "py"; fi

# --- 8) Cargo.lock (TOML-ish, mostly small entries) ---
echo
echo "[8] Cargo.lock-shaped TOML"
F="$ROOT/fixtures/Cargo.lock"; mk_fix "$F"
python3 - "$F" <<'PYEOF'
import sys
with open(sys.argv[1],'w') as f:
    f.write("# This file is automatically @generated by Cargo.\n# It is not intended for manual editing.\nversion = 4\n\n")
    for i in range(150):
        f.write(f'[[package]]\nname = "crate-{i}"\nversion = "1.{i}.0"\nsource = "registry+https://github.com/rust-lang/crates.io-index"\nchecksum = "{i*1234567890abcdef:016x}"\ndependencies = [\n  "dep-a",\n  "dep-b",\n]\n\n')
PYEOF
V=$(drive "$F")
R=$(last_reason)
echo "  reason=$R view_path=$([ -n "$V" ] && basename "$V")"
if [ -n "$V" ]; then verify_round_trip "$V" "$F" "Cargo.lock"; fi

# --- 9) package-lock.json (JSON_FILENAME_HINTS — even huge, should route to JSON) ---
echo
echo "[9] package-lock.json (filename hint forces JSON regardless of content sniff)"
F="$ROOT/fixtures/package-lock.json"; mk_fix "$F"
python3 - "$F" <<'PYEOF'
import json, sys
data = {
    "name": "x", "version": "1.0.0", "lockfileVersion": 3,
    "packages": {"": {"name": "x", "version": "1.0.0"}}
}
data["packages"].update({
    f"node_modules/pkg-{i}": {"version": f"1.{i}.0", "resolved": f"https://r/pkg-{i}/-/pkg-{i}-1.{i}.0.tgz",
                              "integrity": f"sha512-{'x'*88}=="}
    for i in range(300)
})
with open(sys.argv[1],'w') as f: json.dump(data, f, indent=2)
PYEOF
V=$(drive "$F")
R=$(last_reason)
echo "  reason=$R view_path=$([ -n "$V" ] && basename "$V")"
if [ -n "$V" ]; then verify_round_trip "$V" "$F" "package-lock.json"; fi

# --- 10) UTF-16 file (BOM-LE) ---
echo
echo "[10] UTF-16 LE file"
F="$ROOT/fixtures/utf16.txt"; mk_fix "$F"
python3 - "$F" <<'PYEOF'
import sys
with open(sys.argv[1],'wb') as f:
    f.write(b'\xff\xfe')  # UTF-16 LE BOM
    text = ''.join(f"line {i}: utf-16 content\n" for i in range(800))
    f.write(text.encode('utf-16-le'))
PYEOF
V=$(drive "$F")
R=$(last_reason)
echo "  reason=$R view_path=$([ -n "$V" ] && basename "$V")"
if [ -n "$V" ]; then verify_round_trip "$V" "$F" "utf16"; fi

# --- 11) Pure binary masquerading as text (.txt) ---
echo
echo "[11] random bytes with .txt extension"
F="$ROOT/fixtures/binary.txt"; mk_fix "$F"
python3 - "$F" <<'PYEOF'
import os, sys
with open(sys.argv[1],'wb') as f: f.write(os.urandom(30000))
PYEOF
V=$(drive "$F")
R=$(last_reason)
echo "  reason=$R view_path=$([ -n "$V" ] && basename "$V")"
if [ -n "$V" ]; then verify_round_trip "$V" "$F" "binary-as-txt"; fi

# --- 12) Empty JSON object {} as a 9KB file (padded with whitespace) ---
echo
echo "[12] valid JSON but degenerate ({} surrounded by whitespace)"
F="$ROOT/fixtures/empty_obj.json"; mk_fix "$F"
python3 - "$F" <<'PYEOF'
import sys
with open(sys.argv[1],'w') as f:
    f.write("{\n")
    f.write("  " * 4500)  # 9KB of indent whitespace
    f.write("\n}\n")
PYEOF
V=$(drive "$F")
R=$(last_reason)
echo "  reason=$R view_path=$([ -n "$V" ] && basename "$V")"
if [ -n "$V" ]; then verify_round_trip "$V" "$F" "padded-empty-json"; fi

echo
echo "================================================================"
echo "Content-type edge cases: $PASS pass, $FAIL fail"
echo "================================================================"
exit $FAIL
