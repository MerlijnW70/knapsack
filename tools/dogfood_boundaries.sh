#!/usr/bin/env bash
# Probe the read-hook calibration at the exact boundary values the code uses:
#   REDIRECT_MIN_BYTES   = 8 * 1024
#   REDIRECT_MAX_BYTES   = 4 * 1024 * 1024
#   MIN_REDUCTION_PERCENT = 25
#
# We generate files of carefully chosen sizes and reduction profiles, then drive the
# hook and assert the decision matches the documented contract.
set -u
KS="${KS:-$HOME/.knapsack/bin/knapsack}"
SCRATCH="D:/knapsack/target/dogfood_bounds"
rm -rf "$SCRATCH" 2>/dev/null
mkdir -p "$SCRATCH/cache"
export KNAPSACK_READ_LOG="$SCRATCH/read_hook.jsonl"
export KNAPSACK_READ_CACHE="$SCRATCH/cache"
: > "$KNAPSACK_READ_LOG"

PASS=0; FAIL=0
run() {
  local file="$1" expect="$2" label="$3"
  local env="$4"  # KNAPSACK_READ_HOOK override (empty = unset)
  local envelope="{\"tool_name\":\"Read\",\"tool_input\":{\"file_path\":\"$file\"}}"
  local body
  if [ -z "$env" ]; then
    body=$( echo "$envelope" | "$KS" hook 2>/dev/null )
  else
    body=$( echo "$envelope" | KNAPSACK_READ_HOOK="$env" "$KS" hook 2>/dev/null )
  fi
  # Now look at the LAST why-last entry for the actual reason.
  local reason
  reason=$(tail -n 1 "$KNAPSACK_READ_LOG" 2>/dev/null | python3 -c "
import json,sys
try:
  v=json.loads(sys.stdin.read())
  print(v.get('reason',''))
except: print('')
")
  if [ "$reason" = "$expect" ]; then
    printf "  ✓ %-48s reason=%s\n" "$label" "$reason"
    PASS=$((PASS+1))
  else
    printf "  ✗ %-48s expected=%s got=%s\n" "$label" "$expect" "${reason:-<no log entry>}"
    FAIL=$((FAIL+1))
  fi
}

# ---- Size band probes ----------
# Generate a file of exactly N bytes of compressible content. We use repeated lines so
# the structural compressor would shrink it well above the 25% reduction threshold —
# the variable we control is SIZE, not compressibility. Heredoc is single-quoted so
# bash leaves the Python alone (no interpolation, no stray semicolon parsing).
gen_compressible() {
  python3 - "$1" "$2" <<'PYEOF'
import sys
path, size = sys.argv[1], int(sys.argv[2])
with open(path, 'wb') as f:
    chunk = b'[INFO] step 0001: routine work that compresses well; lots of repeated similar lines\n'
    nfull = size // len(chunk)
    rem = size % len(chunk)
    for _ in range(nfull):
        f.write(chunk)
    f.write(chunk[:rem])
PYEOF
}

echo "================================================================"
echo "Size band probes (REDIRECT_MIN_BYTES=8192, REDIRECT_MAX_BYTES=4194304)"
echo "================================================================"
B="$SCRATCH/band"
mkdir -p "$B"

# Below threshold by 1 byte
gen_compressible "$B/below_min.txt" 8191
run "$B/below_min.txt" "too-small" "8191 B (under min)" ""

# Exactly at threshold (the code uses `< REDIRECT_MIN_BYTES`, so 8192 should NOT trip TooSmall)
gen_compressible "$B/at_min.txt" 8192
run "$B/at_min.txt" "redirect-emitted" "8192 B (at min, expect redirect)" ""

# One byte over
gen_compressible "$B/above_min.txt" 8193
run "$B/above_min.txt" "redirect-emitted" "8193 B (just over min)" ""

# One byte under max
gen_compressible "$B/below_max.txt" 4194303
run "$B/below_max.txt" "redirect-emitted" "4 MB - 1 B (just under max)" ""

# Exactly at max (code uses `> REDIRECT_MAX_BYTES`, so 4194304 should still pass)
gen_compressible "$B/at_max.txt" 4194304
run "$B/at_max.txt" "redirect-emitted" "4194304 B (at max, still in band)" ""

# Just over max
gen_compressible "$B/above_max.txt" 4194305
run "$B/above_max.txt" "too-large" "4 MB + 1 B (over max)" ""

# ---- Reduction-threshold probes ----------
echo
echo "================================================================"
echo "Reduction threshold probes (MIN_REDUCTION_PERCENT=25)"
echo "================================================================"

# A file where the structural compressor LITERALLY can't compress (every line unique
# function signature, .js so it routes through compress_code which only elides bodies).
# This should hit WorseThanRaw.
T="$SCRATCH/threshold"
mkdir -p "$T"
python3 -c "
import sys
with open('$T/uncompressible.js','w') as f:
  for i in range(800):
    f.write(f'function handler{i}() {{ return {i}; }}\n')
"
run "$T/uncompressible.js" "worse-than-raw" "incompressible .js (lots of one-line fns)" ""

# A file that compresses well (above 25%) — already covered by the size probes; one more
# to be explicit:
gen_compressible "$T/very_compressible.txt" 100000
run "$T/very_compressible.txt" "redirect-emitted" "highly compressible 100 KB log" ""

# ---- Off-switch values ----------
echo
echo "================================================================"
echo "Off-switch values (the gate accepts 0/off/false/no/empty)"
echo "================================================================"
gen_compressible "$T/big.txt" 50000
# NOTE: we don't include the empty-string case here — Windows treats `VAR=""` as
# unsetting the variable from the child's perspective, so the empty case is a no-op
# on this platform. The Rust code DOES treat `Ok("")` as off-switch (see
# `read_hook_enabled` in src/config.rs) and the unit test `gate_disabled_*` pins
# that path. We just can't deliver an empty string through the shell here.
for v in "0" "off" "false" "no" "OFF" "False" "NO"; do
  run "$T/big.txt" "gate-disabled" "KNAPSACK_READ_HOOK=$v" "$v"
done
# Anything else should leave it ON (default).
for v in "1" "yes" "on" "true" "abc"; do
  run "$T/big.txt" "redirect-emitted" "KNAPSACK_READ_HOOK=$v (default-on accepted)" "$v"
done

echo
echo "================================================================"
echo "Boundary probe: $PASS pass, $FAIL fail"
echo "================================================================"
exit $FAIL
