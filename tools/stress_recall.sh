#!/usr/bin/env bash
# Hostile-input recall stress: pack a bunch of pathological fixtures, then verify
# the byte-exact recall handle returns the original byte-for-byte. Run from any cwd;
# fixtures land under $TMPDIR/ks-stress.
set -u
KS="${KS:-$HOME/.knapsack/bin/knapsack}"
ROOT="${TMPDIR:-/tmp}/ks-stress"
rm -rf "$ROOT"; mkdir -p "$ROOT/in" "$ROOT/out"

mk_fixture() {
  local name="$1"; shift
  printf "%s" "$@" > "$ROOT/in/$name"
}

# (a) empty
: > "$ROOT/in/empty.txt"
# (b) single byte
printf 'a' > "$ROOT/in/one.txt"
# (c) no trailing newline
printf 'line1\nline2' > "$ROOT/in/no_trailing_nl.txt"
# (d) only newlines
python3 -c "import sys; sys.stdout.write('\n'*10000)" > "$ROOT/in/all_newlines.txt"
# (e) very long single line
python3 -c "import sys; sys.stdout.write('x'*200000)" > "$ROOT/in/long_line.txt"
# (f) UTF-8 BOM
printf '\xEF\xBB\xBFhello world\n' > "$ROOT/in/bom.txt"
# (g) CRLF mixed
printf 'a\r\nb\nc\rd\r\n' > "$ROOT/in/mixed_eol.txt"
# (h) embedded nulls
printf 'a\x00b\x00c\x00\n' > "$ROOT/in/nulls.bin"
# (i) random binary
python3 -c "import os,sys; sys.stdout.buffer.write(os.urandom(64*1024))" > "$ROOT/in/random.bin"
# (j) high-bit byte stream (invalid UTF-8)
python3 -c "import sys; sys.stdout.buffer.write(bytes(range(128,256))*512)" > "$ROOT/in/invalid_utf8.bin"
# (k) very large compressible log
python3 -c "
import sys
for i in range(200000):
    sys.stdout.write(f'[INFO] step {i}: routine work x' + ' '*40 + '\n')
" > "$ROOT/in/big_log.txt"

# (l) tricky JSON
python3 -c "
import json,sys
sys.stdout.write(json.dumps({'a':[1,2,{'b':'c\\u0000d'},'\xff'.encode().decode('latin-1')], 'unicode':'héllo·世界·☃','quotes':'\"q\"','escapes':'\\\\n'}))
" > "$ROOT/in/tricky.json"

pass=0; fail=0
for f in "$ROOT"/in/*; do
  name=$(basename "$f")
  # Read the handle line: `Exact original: recoverable via \`knapsack expand HANDLE\``
  handle=$( "$KS" pack "$f" --dry-run 2>&1 | grep -oE 'ks2_[0-9a-f]+' | head -1 )
  if [ -z "$handle" ]; then
    echo "FAIL[$name]: no handle in pack output"
    fail=$((fail+1)); continue
  fi
  # Persist to store via `knapsack store put`
  "$KS" store put "$f" > /dev/null 2>&1
  # Expand to a tmp file
  out="$ROOT/out/$name.expanded"
  "$KS" expand "$handle" > "$out" 2>/dev/null
  if cmp -s "$f" "$out"; then
    pass=$((pass+1))
    echo "OK   [$name]  size=$(stat -c%s "$f")  handle=${handle:0:24}.."
  else
    fail=$((fail+1))
    echo "FAIL [$name]  size=$(stat -c%s "$f")  expanded=$(stat -c%s "$out")"
  fi
done

echo
echo "Stress summary: $pass passed, $fail failed"
exit $fail
