#!/usr/bin/env bash
# CLI fuzz: feed the knapsack subcommands malformed/edge args. We don't expect any
# particular exit code — we expect: no panics ("thread 'main' panicked"), no infinite
# loops (everything finishes under the timeout), and a sane stderr message.
set -u
KS="${KS:-$HOME/.knapsack/bin/knapsack}"
TIMEOUT=10  # seconds per call
PASS=0; FAIL=0

run() {
  local label="$1"; shift
  local out err rc
  err=$(mktemp)
  timeout --foreground "$TIMEOUT" "$KS" "$@" >/dev/null 2>"$err"
  rc=$?
  # rc=124 = timeout (hang). Any panic is a bug.
  if [ "$rc" -eq 124 ]; then
    echo "HANG   [$label]  rc=124"
    FAIL=$((FAIL+1))
  elif grep -q "panicked" "$err"; then
    echo "PANIC  [$label]"
    sed 's/^/  | /' "$err"
    FAIL=$((FAIL+1))
  else
    echo "OK     [$label]  rc=$rc"
    PASS=$((PASS+1))
  fi
  rm -f "$err"
}

# Set fresh state.
TMPROOT=$(mktemp -d)
export KNAPSACK_STORE="$TMPROOT/store"
export KNAPSACK_METRICS="$TMPROOT/m.jsonl"

# --- expand ---
run "expand no args"                 expand
run "expand junk handle"             expand "junk"
run "expand wrong prefix"            expand "ks3_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
run "expand short hex"               expand "ks2_aabb"
run "expand long hex"                expand "ks2_$(printf 'a%.0s' {1..200})"
run "expand non-hex"                 expand "ks2_zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz"
run "expand unknown handle"          expand "ks2_$(printf '0%.0s' {1..32})"
run "expand --lines junk"            expand "ks2_$(printf '0%.0s' {1..32})" --lines "abc"
run "expand --lines reversed"        expand "ks2_$(printf '0%.0s' {1..32})" --lines "100-1"
run "expand --lines negative"        expand "ks2_$(printf '0%.0s' {1..32})" --lines "-1-2"
run "expand --grep regex bomb"       expand "ks2_$(printf '0%.0s' {1..32})" --grep "(a+)+$"
run "expand --context negative"      expand "ks2_$(printf '0%.0s' {1..32})" --grep "x" --context "-5"
run "expand --context huge"          expand "ks2_$(printf '0%.0s' {1..32})" --grep "x" --context "999999999"

# --- inspect ---
run "inspect no args"                inspect
run "inspect junk handle"            inspect "garbage"
run "inspect missing file path"      inspect "/path/does/not/exist.knapsack.md"

# --- pack ---
run "pack no args"                   pack
run "pack missing file"              pack "/does/not/exist.txt"
run "pack dir-as-file"               pack "$TMPROOT"
run "pack output to dir-as-file"     pack "$0" --output "$TMPROOT"
run "pack output to readonly path"   pack "$0" --output "/"

# --- store put ---
run "store put no args"              store put
run "store put missing file"         store put "/no/such/file"
run "store put dir-as-file"          store put "$TMPROOT"

# --- delta ---
run "delta no args"                  delta
run "delta one arg"                  delta "/no/such/file"
run "delta missing files"            delta "/no/a" "/no/b"

# --- gc ---
run "gc older-than negative"         gc --older-than "-5"
run "gc dry-run"                     gc --dry-run

# --- transcript ---
run "transcript missing file"        transcript "/no/such/transcript.jsonl"
run "transcript empty file"          transcript "$(mktemp)"

# --- why-last ---
run "why-last 0"                     why-last 0
run "why-last huge"                  why-last 999999999

# --- bench / status / metrics ---
run "status"                         status
run "metrics"                        metrics
# bench writes its own store — skip if would touch real one
KNAPSACK_STORE="$TMPROOT/bench_store" run "bench" bench

echo
echo "CLI fuzz: $PASS ok, $FAIL bad"
rm -rf "$TMPROOT"
exit $FAIL
