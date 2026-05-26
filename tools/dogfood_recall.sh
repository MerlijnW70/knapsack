#!/usr/bin/env bash
# Recall combinations: every pairing of --lines, --grep, --context against a known
# fixture so we can compute expected output and compare byte-exact.
set -u
KS="${KS:-$HOME/.knapsack/bin/knapsack}"
ROOT="D:/knapsack/target/dogfood_recall"
rm -rf "$ROOT" 2>/dev/null
mkdir -p "$ROOT"
export KNAPSACK_STORE="$ROOT/store"
export KNAPSACK_METRICS="$ROOT/metrics.jsonl"
export KNAPSACK_SESSIONS="$ROOT/sessions"

PASS=0; FAIL=0
ok() { PASS=$((PASS+1)); echo "  ✓ $*"; }
no() { FAIL=$((FAIL+1)); echo "  ✗ $*"; }

# Build a fixture with 200 numbered lines. Use BINARY mode so we get pure LF endings
# regardless of OS (Windows Python text-mode would write \r\n and break cmp-vs-sed).
FIX="$ROOT/fixture.log"
python3 - "$FIX" <<'PYEOF'
import sys
with open(sys.argv[1], 'wb') as f:
    for i in range(1, 201):
        marker = "TARGET" if i in {37, 88, 142} else "normal"
        line = f"line {i:03d}: {marker} entry — payload for line {i}\n"
        f.write(line.encode('utf-8'))
PYEOF

# Store the whole file (via pack - so it's in the byte-exact store).
H=$( "$KS" pack - --session "rec" --cmd "rec" --type log < "$FIX" 2>&1 | grep -oE 'ks2_[0-9a-f]+' | head -1 )
echo "  handle: $H"

# ---- 1) Whole-file expand -> byte-exact ----
if "$KS" expand "$H" 2>/dev/null | cmp -s - "$FIX"; then
  ok "expand whole -> byte-exact"
else
  no "expand whole -> diff vs original"
fi

# ---- 2) --lines A-B ----
expected=$(sed -n '10,20p' "$FIX")
got=$( "$KS" expand "$H" --lines "10-20" 2>/dev/null )
if [ "$got" = "$expected" ]; then
  ok "--lines 10-20 matches sed output"
else
  no "--lines 10-20 mismatch (got $(echo "$got" | wc -l) lines, expected $(echo "$expected" | wc -l))"
fi

# ---- 3) --lines 1-1 ----
expected=$(sed -n '1p' "$FIX")
got=$( "$KS" expand "$H" --lines "1-1" 2>/dev/null )
if [ "$got" = "$expected" ]; then ok "--lines 1-1 returns line 1 only"; else no "--lines 1-1 mismatch"; fi

# ---- 4) --lines 200-200 (last line) ----
expected=$(sed -n '200p' "$FIX")
got=$( "$KS" expand "$H" --lines "200-200" 2>/dev/null )
if [ "$got" = "$expected" ]; then ok "--lines 200-200 returns last line"; else no "last-line mismatch"; fi

# ---- 5) --lines way out of range (e.g. 9999-10000) ----
got=$( "$KS" expand "$H" --lines "9999-10000" 2>/dev/null )
if [ -z "$got" ]; then ok "--lines out-of-range -> empty"; else no "out-of-range returned: $got"; fi

# ---- 6) --lines clamped at end (e.g. 195-300 should yield 195-200) ----
expected=$(sed -n '195,200p' "$FIX")
got=$( "$KS" expand "$H" --lines "195-300" 2>/dev/null )
if [ "$got" = "$expected" ]; then ok "--lines end-clamp 195-300 -> 195-200"; else no "end-clamp mismatch"; fi

# ---- 7) --grep TARGET (3 hits) ----
expected=$(grep "TARGET" "$FIX")
got=$( "$KS" expand "$H" --grep "TARGET" 2>/dev/null )
if [ "$got" = "$expected" ]; then ok "--grep TARGET returns 3 matching lines"; else no "--grep TARGET mismatch"; fi

# ---- 8) --grep with no matches -> empty ----
got=$( "$KS" expand "$H" --grep "DOESNOTEXIST" 2>/dev/null )
if [ -z "$got" ]; then ok "--grep no-match -> empty"; else no "no-match returned: $got"; fi

# ---- 9) --grep + --context 1 (each hit's neighbour lines) ----
# Expect: line 36, 37, 38, 87, 88, 89, 141, 142, 143
expected=$(sed -n '36,38p;87,89p;141,143p' "$FIX")
got=$( "$KS" expand "$H" --grep "TARGET" --context 1 2>/dev/null )
if [ "$got" = "$expected" ]; then ok "--grep TARGET --context 1 returns neighbours"; else
  no "--grep + --context 1 mismatch"
  echo "  expected:" ; echo "$expected" | sed 's/^/      | /'
  echo "  got:"      ; echo "$got"      | sed 's/^/      | /'
fi

# ---- 10) --grep + --context 0 (same as --grep) ----
expected=$(grep "TARGET" "$FIX")
got=$( "$KS" expand "$H" --grep "TARGET" --context 0 2>/dev/null )
if [ "$got" = "$expected" ]; then ok "--grep + --context 0 == --grep"; else no "context 0 mismatch"; fi

# ---- 11) --grep regex (e.g. line 03[0-9]) ----
expected=$(grep -E 'line 03[0-9]' "$FIX")
got=$( "$KS" expand "$H" --grep "line 03[0-9]" 2>/dev/null )
if [ "$got" = "$expected" ]; then ok "--grep regex character class works"; else no "regex char class mismatch"; fi

# ---- 12) --grep '^line 100' anchor ----
expected=$(grep -E '^line 100' "$FIX")
got=$( "$KS" expand "$H" --grep "^line 100" 2>/dev/null )
if [ "$got" = "$expected" ]; then ok "--grep ^anchor works"; else no "^anchor mismatch"; fi

# ---- 13) Combined --lines + --grep ----
# Restrict to lines 50-100 THEN grep TARGET inside; line 88 should match (in window)
# and lines 37 + 142 should NOT match (outside window).
expected=$(sed -n '50,100p' "$FIX" | grep "TARGET")
got=$( "$KS" expand "$H" --lines "50-100" --grep "TARGET" 2>/dev/null )
if [ "$got" = "$expected" ]; then ok "--lines + --grep composes correctly"; else
  no "--lines + --grep mismatch"
  echo "  expected:" ; echo "$expected" | sed 's/^/      | /'
  echo "  got:"      ; echo "$got"      | sed 's/^/      | /'
fi

# ---- 14) Combined --lines + --grep + --context ----
# In window 50-100, grep TARGET (matches line 88), context 1 -> lines 87, 88, 89
expected=$(sed -n '87,89p' "$FIX")
got=$( "$KS" expand "$H" --lines "50-100" --grep "TARGET" --context 1 2>/dev/null )
if [ "$got" = "$expected" ]; then ok "lines + grep + context compose"; else
  no "three-way combo mismatch"
  echo "  expected:" ; echo "$expected" | sed 's/^/      | /'
  echo "  got:"      ; echo "$got"      | sed 's/^/      | /'
fi

# ---- 15) Case-insensitive default? Knapsack's regex impl is case-INsensitive by default? ----
# Check current behavior: 'target' lowercase should or should not match TARGET?
got=$( "$KS" expand "$H" --grep "target" 2>/dev/null )
hits=$(echo "$got" | grep -c "TARGET" || echo 0)
case_total=$(grep -c "TARGET" "$FIX")
if [ "$hits" = "$case_total" ]; then
  ok "grep is case-insensitive by default (matches: $hits of $case_total)"
elif [ "$hits" = "0" ]; then
  ok "grep is case-sensitive by default (0 hits for lowercase target)"
else
  no "grep gave partial hits: $hits of $case_total — neither pure case-sens nor case-insens"
fi

# ---- 16) --grep with regex that fails to compile -> substring fallback ----
# E.g. "(unterminated"
got=$( "$KS" expand "$H" --grep "(unterminated" 2>/dev/null )
# Should fall back to substring matching of literal "(unterminated" → 0 hits.
if [ -z "$got" ]; then ok "bad regex falls back to substring (no hits as expected)"; else no "bad regex returned hits"; fi

# ---- 17) --context very large (200+) on a 200-line file ----
expected="$(cat "$FIX")"
got=$( "$KS" expand "$H" --grep "TARGET" --context 500 2>/dev/null )
# Should be at most the whole file.
got_lines=$(echo "$got" | wc -l)
file_lines=$(wc -l < "$FIX")
if [ "$got_lines" -le "$((file_lines + 1))" ]; then
  ok "huge --context clamped to whole file ($got_lines lines)"
else
  no "huge --context exceeded file: got $got_lines vs file $file_lines"
fi

echo
echo "================================================================"
echo "Recall combinations: $PASS pass, $FAIL fail"
echo "================================================================"
exit $FAIL
