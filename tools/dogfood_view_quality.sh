#!/usr/bin/env bash
# Run the read hook against a battery of REAL file types and inspect the resulting
# view: how lossy is the compression, which anchors survive, which don't. For every
# file we record raw bytes, view bytes, view tokens, elision count, and grep for a
# few "anchors" we'd expect a model to need (function signatures for code, top-level
# keys for JSON, headings for markdown, error/warning lines for logs).
set -u
KS="${KS:-$HOME/.knapsack/bin/knapsack}"
ROOT="D:/knapsack/target/dogfood_viewq"
rm -rf "$ROOT" 2>/dev/null
mkdir -p "$ROOT/cache" "$ROOT/store"
export KNAPSACK_STORE="$ROOT/store"
export KNAPSACK_READ_LOG="$ROOT/read_hook.jsonl"
export KNAPSACK_READ_CACHE="$ROOT/cache"
: > "$KNAPSACK_READ_LOG"

PASS=0; FAIL=0
ok() { PASS=$((PASS+1)); echo "  ✓ $*"; }
no() { FAIL=$((FAIL+1)); echo "  ✗ $*"; }

# Drive the hook on a real file, return the view path (or "" if pass-through).
drive() {
  local path="$1"
  local envelope; envelope=$(python3 - "$path" <<'PYEOF'
import json, sys
print(json.dumps({"tool_name":"Read","tool_input":{"file_path":sys.argv[1]},"session_id":"viewq"}))
PYEOF
)
  local body
  body=$(echo "$envelope" | "$KS" hook 2>/dev/null)
  if [ -z "$body" ]; then echo ""; return; fi
  echo "$body" | python3 -c "
import json,sys
v=json.loads(sys.stdin.read())
print(v['hookSpecificOutput']['updatedInput']['file_path'])
"
}

# Generate a fixture file from stdin if it doesn't already exist.
make_fixture() {
  local path="$1"; mkdir -p "$(dirname "$path")"
  cat > "$path"
}

# Fixtures dir
FIX="$ROOT/fixtures"
mkdir -p "$FIX"

# ---- 1) Real Rust source from the repo (already 24 KB, redirects nicely) ----
RUST_SRC="D:/knapsack/src/main.rs"

# ---- 2a) Markdown README — short paragraphs (pack_doc thresholds don't trip; honest pass-through expected) ----
python3 - "$FIX/big_readme.md" <<'PYEOF'
import sys
with open(sys.argv[1],'w',encoding='utf-8') as f:
    f.write("# My Project\n\n")
    f.write("[![Status](https://img.shields.io/badge/status-shipping-green)]()\n\n")
    f.write("A real-shape README with multiple sections, code fences, lists, links.\n\n")
    for sec in range(8):
        f.write(f"## Section {sec}: Long prose section\n\n")
        f.write("This section contains realistic narrative prose explaining a concept.\n")
        for p in range(20):
            f.write(f"Paragraph {p}: this is a sentence about subject matter that flows naturally and includes specific words like processor, configuration, and serialization.\n\n")
        f.write("```rust\n")
        for i in range(15):
            f.write(f"fn example_{sec}_{i}(input: &str) -> Result<u64, Error> {{ Ok({i}) }}\n")
        f.write("```\n\n")
        f.write("- Bullet point one\n- Bullet point two\n- Bullet point three\n\n")
PYEOF

# ---- 2b) Markdown long-prose design doc — clears pack_doc's ≥500-char single-line threshold ----
python3 - "$FIX/design_doc.md" <<'PYEOF'
import sys
sentence = "This section describes a distributed system component that handles authentication and rate limiting across multiple regions, with careful attention to consistency guarantees, latency budgets, and operational complexity. "
with open(sys.argv[1],'w',encoding='utf-8') as f:
    f.write("# Design Document: Distributed System Architecture\n\n")
    for sec in range(8):
        f.write(f"## Section {sec}: A real subsystem\n\n")
        for _ in range(6):
            f.write(sentence * 4 + "\n\n")
        f.write("```rust\nfn handler() -> Result<u64, Error> { Ok(0) }\n```\n\n")
        f.write("- key point\n- another point\n\n")
PYEOF

# ---- 3) JSON — large API response (already 100 KB shape) ----
python3 - "$FIX/api_response.json" <<'PYEOF'
import json, sys
data = {
    "version": "1.0.0",
    "items": [
        {"id": i, "name": f"item-{i:04d}", "tags": ["alpha","beta","gamma"],
         "description": "A repeated description that compresses well across many items.",
         "metadata": {"created": "2026-05-26T12:00:00Z", "version": "1.0", "active": True,
                      "owner": "team@example.com", "annotations": {"priority": i % 5}}}
        for i in range(400)
    ],
    "totalCount": 400,
    "nextPageToken": "abc-def-ghi",
}
with open(sys.argv[1],'w') as f:
    json.dump(data, f, indent=2)
PYEOF

# ---- 4) Log — error/warning-heavy realistic build log ----
python3 - "$FIX/build.log" <<'PYEOF'
import sys
with open(sys.argv[1],'w',encoding='utf-8') as f:
    for i in range(50):
        f.write(f"   Compiling crate-{i} v0.{i}.0\n")
        for j in range(80):
            f.write(f"warning: unused variable: `x_{j}` in crate-{i}\n")
            f.write(f"  --> src/lib.rs:{j}:5\n")
            f.write(f"   |\n")
            f.write(f"{j+10} |     let x_{j} = compute();\n")
            f.write(f"   |         ^^^^ help: if this is intentional, prefix it with an underscore: `_x_{j}`\n\n")
    f.write("error[E0308]: mismatched types\n  --> src/main.rs:42:13\n   |\n42 |     let x: u32 = \"hello\";\n   |             ^^^ expected `u32`, found `&str`\n\n")
    f.write("error: aborting due to previous error\n")
PYEOF

# ---- 5) CSV — synthetic but real-shaped ----
python3 - "$FIX/data.csv" <<'PYEOF'
import sys
with open(sys.argv[1],'w') as f:
    f.write("id,name,email,status,created_at,score\n")
    for i in range(800):
        f.write(f"{i},user{i},user{i}@example.com,{'active' if i%3 else 'inactive'},2026-05-26T12:00:00Z,{(i*37) % 1000}\n")
PYEOF

# ---- 6) SQL — schema + a load of inserts ----
python3 - "$FIX/dump.sql" <<'PYEOF'
import sys
with open(sys.argv[1],'w') as f:
    f.write("-- SQL dump for testing\n\n")
    f.write("CREATE TABLE users (id INT PRIMARY KEY, name TEXT, email TEXT);\n")
    f.write("CREATE TABLE orders (id INT, user_id INT, amount DECIMAL, created_at TIMESTAMP);\n\n")
    for i in range(500):
        f.write(f"INSERT INTO users (id, name, email) VALUES ({i}, 'user-{i}', 'user{i}@example.com');\n")
    f.write("\n")
    for i in range(800):
        f.write(f"INSERT INTO orders (id, user_id, amount, created_at) VALUES ({i}, {i%500}, {(i*17)%1000}.00, '2026-05-26 12:00:00');\n")
PYEOF

# ---- 7) HTML — realistic page ----
python3 - "$FIX/page.html" <<'PYEOF'
import sys
with open(sys.argv[1],'w',encoding='utf-8') as f:
    f.write("<!DOCTYPE html>\n<html><head><title>Test Page</title></head><body>\n")
    f.write("<header><h1>Welcome</h1></header>\n")
    f.write("<main>\n")
    for s in range(30):
        f.write(f"<section><h2>Section {s}</h2>\n<p>Lorem ipsum dolor sit amet, consectetur adipiscing elit. Sed do eiusmod tempor incididunt ut labore et dolore magna aliqua. Section content {s}.</p>\n")
        f.write("<ul>\n")
        for i in range(10): f.write(f"  <li>Item {s}.{i}</li>\n")
        f.write("</ul></section>\n")
    f.write("</main></body></html>\n")
PYEOF

# ---- 8) Minified JS — single very long line ----
python3 - "$FIX/bundle.min.js" <<'PYEOF'
import sys
with open(sys.argv[1],'w') as f:
    parts = []
    for i in range(800):
        parts.append(f"function fn{i}(a,b){{return a+b+{i};}}")
    f.write(';'.join(parts) + ';\n')
PYEOF

# ---- 9) Python source ----
python3 - "$FIX/script.py" <<'PYEOF'
import sys
with open(sys.argv[1],'w') as f:
    f.write('#!/usr/bin/env python3\n"""Module-level docstring describing the purpose."""\n\n')
    f.write('import sys\nimport json\nimport os\n\n')
    for i in range(40):
        f.write(f'def handler_{i}(input_data, options):\n')
        f.write(f'    """Handler {i}: validate then process."""\n')
        for j in range(8):
            f.write(f'    result_{j} = compute_step_{j}(input_data)\n')
        f.write(f'    return finalize(result_0)\n\n')
    f.write('class MyService:\n    """A class containing multiple methods."""\n\n')
    for i in range(10):
        f.write(f'    def method_{i}(self, arg):\n')
        for j in range(5):
            f.write(f'        self.value_{j} = arg + {j}\n')
        f.write(f'        return self.value_0\n\n')
PYEOF

# ---- 10) TypeScript ----
python3 - "$FIX/types.ts" <<'PYEOF'
import sys
with open(sys.argv[1],'w') as f:
    f.write('// Type definitions\nexport interface User { id: number; name: string; email: string; }\n\n')
    for i in range(50):
        f.write(f'export interface Response{i} {{\n')
        for j in range(8):
            f.write(f'  field_{j}: string;\n')
        f.write(f'}}\n\nexport function handle{i}(req: Response{i}): User {{\n')
        for j in range(6):
            f.write(f'  const local_{j} = process(req.field_{j});\n')
        f.write(f'  return {{ id: {i}, name: "user", email: "x" }};\n}}\n\n')
PYEOF

# ---- 11) YAML config ----
python3 - "$FIX/config.yaml" <<'PYEOF'
import sys
with open(sys.argv[1],'w') as f:
    f.write("# Application config\nversion: 1\n\nservices:\n")
    for i in range(30):
        f.write(f'  service-{i}:\n    image: org/service-{i}:latest\n    port: {3000+i}\n    env:\n')
        for k in range(8):
            f.write(f'      VAR_{k}: value-{i}-{k}\n')
        f.write(f'    healthcheck:\n      path: /health\n      interval: 30s\n')
PYEOF

# ---- Now drive each fixture through the hook and inspect ----
echo
echo "================================================================"
echo "Quality grid — for each file, what does the view actually carry?"
echo "================================================================"
printf "%-30s %10s %10s %8s %s\n" "file" "raw_B" "view_B" "%saved" "anchors"
echo "----------------------------------------------------------------"

check_file() {
  local file="$1" anchor_grep="$2" must_contain="$3" label="${4:-$(basename "$file")}"
  local raw view view_path
  raw=$(wc -c < "$file")
  view_path=$(drive "$file")
  if [ -z "$view_path" ]; then
    printf "%-30s %10s %10s %8s %s\n" "$label" "$raw" "-" "PASS-THRU" "(reason in why-last)"
    return
  fi
  view=$(wc -c < "$view_path")
  local pct
  pct=$(awk "BEGIN { printf \"%.1f%%\", 100*($raw-$view)/$raw }")
  # Count how many "anchor" lines from the original survive in the view.
  local original_anchor_count=$(grep -cE "$anchor_grep" "$file" 2>/dev/null || echo 0)
  local view_anchor_count=$(grep -cE "$anchor_grep" "$view_path" 2>/dev/null || echo 0)
  local note="anchors $view_anchor_count/$original_anchor_count"
  # Does the view contain the must-contain string?
  if [ -n "$must_contain" ] && ! grep -q "$must_contain" "$view_path"; then
    note="$note  ✗ missing '$must_contain'"
    FAIL=$((FAIL+1))
  fi
  printf "%-30s %10s %10s %8s %s\n" "$label" "$raw" "$view" "$pct" "$note"
}

# For each: anchor pattern (what the model would want preserved) + a must-contain check
check_file "$RUST_SRC"           '^[[:space:]]*("[^"]+" \||fn |pub fn )' '' "rust src/main.rs"
check_file "$FIX/big_readme.md"  '^#'                                    '# My Project' "markdown README (short)"
check_file "$FIX/design_doc.md"  '^#'                                    '# Design Document' "markdown design doc (long-prose)"
check_file "$FIX/api_response.json" '"totalCount"|"version"|"nextPageToken"' '"totalCount"' "JSON API response"
check_file "$FIX/build.log"      'error\[?E?[0-9]*\]?:|warning:'         'error: aborting' "build log w/ errors"
check_file "$FIX/data.csv"       '^id,name,email|^[0-9]+,'               'id,name,email' "CSV (800 rows)"
check_file "$FIX/dump.sql"       '^CREATE TABLE|^INSERT INTO'            'CREATE TABLE users' "SQL dump"
check_file "$FIX/page.html"      '<h[12]>|<section'                      '<title>' "HTML"
check_file "$FIX/bundle.min.js"  'function fn[0-9]+'                     '' "minified JS bundle"
check_file "$FIX/script.py"      '^def |^class '                         'class MyService' "Python source"
check_file "$FIX/types.ts"       '^export (interface|function)'          'export interface User' "TypeScript"
check_file "$FIX/config.yaml"    'services:|service-[0-9]+'              'services:' "YAML config"

echo
echo "================================================================"
echo "View quality grid: $FAIL anchor-preservation failures"
echo "================================================================"
exit $FAIL
