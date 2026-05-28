//! Round-11: the Bash hook's wrapper MUST preserve the wrapped command's exit
//! code byte-exact. Without this, the agentic loop is broken in a way that's
//! invisible to the user — a failing `cargo test` looks like success to any
//! script checking `$?`, every `cmd && deploy` chain silently mis-fires, and
//! the "never worse than raw" contract is violated on the dimension that
//! matters most.
//!
//! The wrap shape already does the right thing (temp-file capture of `$?`,
//! then `exit "$(cat …)"`) but had only a string-shape regression test that
//! pinned the substring `exit ` and the bin path — never actually ran the
//! wrap and observed the exit code. These tests close that gap.
//!
//! Two kinds of coverage:
//!   1. STRING-SHAPE pins (run anywhere) — the wrap must include the
//!      tempfile-capture pattern and must NOT depend on `pipefail` (POSIX
//!      sh doesn't have it; bash-only would break on macOS /bin/sh).
//!   2. RUNTIME pins (require POSIX `sh`) — generate the wrap, invoke it
//!      via `sh -c`, and assert the spawned process returns the inner
//!      command's exit code. These are the tests that would have caught
//!      the bug described in the round-11 brief, had it existed.

use knapsack::hook::wrap_command;
use std::process::Command;

// ---------- string-shape pins (portable, no shell required) ------------------

/// Pin every load-bearing element of the wrap's exit-code-preservation logic.
/// If any of these disappear in a refactor the runtime tests below would also
/// catch it, but the string-shape pin gives a faster, platform-independent
/// signal — and runs on hosts that don't have POSIX sh (e.g. CI without
/// Git Bash installed).
#[test]
fn wrap_captures_inner_exit_code_into_tempfile() {
    let w = wrap_command("cargo test", "/path/knapsack", "sess", "cargo", None);

    // The tempfile is created via mktemp with a TMPDIR fallback. Without
    // mktemp on the PATH the wrap still has somewhere to write `$?`.
    assert!(w.contains("mktemp"), "wrap must create a tempfile: {w}");
    assert!(
        w.contains("TMPDIR"),
        "wrap must fall back to $TMPDIR when mktemp is missing: {w}"
    );

    // The brace group runs the inner command and immediately captures `$?`
    // BEFORE the pipeline's downstream `pack` invocation overwrites it.
    // Without this exact ordering the captured value is pack's exit, not
    // the inner command's.
    assert!(
        w.contains("echo $?"),
        "wrap must `echo $?` to capture the inner command's exit: {w}"
    );

    // The wrapper's final action must be `exit "$(cat <tempfile>…)"` —
    // anything else (bare `exit`, a literal code, exit on the pipeline
    // status) loses the captured value.
    assert!(
        w.contains("exit \"$(cat"),
        "wrap must re-emit the captured exit code via `exit \"$(cat …)\"`: {w}"
    );

    // EXIT/INT/TERM trap cleans the tempfile so SIGINT mid-pipeline doesn't
    // leave debris in $TMPDIR.
    assert!(w.contains("trap "), "wrap must install a cleanup trap: {w}");
    assert!(
        w.contains("EXIT INT TERM"),
        "trap must fire on EXIT plus the common signals: {w}"
    );
}

/// `set -o pipefail` is bash/zsh-only. POSIX `sh` (the default `/bin/sh` on
/// many macOS and Alpine systems) does not implement it; relying on it would
/// silently break exit-code propagation on those hosts. The tempfile pattern
/// is the portable approach; this test pins that we never regress into
/// `pipefail`-based wraps.
#[test]
fn wrap_does_not_rely_on_pipefail() {
    let w = wrap_command("cargo test", "/path/knapsack", "sess", "cargo", None);
    assert!(
        !w.contains("pipefail"),
        "wrap must work on POSIX sh, which has no `pipefail` — found: {w}"
    );
}

// ---------- runtime pins (require POSIX sh) ----------------------------------

/// Some hosts (Windows without Git Bash) can't run the wrap at all because it
/// needs POSIX sh. Skip runtime tests there with a clear note; the
/// string-shape pins above still run and provide partial coverage.
fn sh_available() -> bool {
    Command::new("sh")
        .arg("-c")
        .arg("exit 0")
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// `CARGO_BIN_EXE_knapsack` is set by Cargo for integration tests, pointing
/// at the built binary. We need a real knapsack so the `pack -` step in the
/// pipeline actually sinks stdin and exits cleanly — using `cat` or `true`
/// would change the pipeline's behavior and stop testing the production path.
fn knapsack_bin() -> &'static str {
    env!("CARGO_BIN_EXE_knapsack")
}

/// Build the wrap, run it under `sh -c`, return the captured exit code.
/// `inner` is the command we want the wrap to preserve the exit code of.
fn run_wrapped(inner: &str) -> i32 {
    let wrapped = wrap_command(inner, knapsack_bin(), "round11", "prog", None);
    let status = Command::new("sh")
        .arg("-c")
        .arg(&wrapped)
        .status()
        .expect("spawn sh");
    // On Unix, .code() can be None when killed by a signal — for these
    // synthetic commands that should never happen. Treat None as a failure
    // with a sentinel that no real test value would collide with.
    status.code().unwrap_or(i128::MAX as i32)
}

#[test]
fn wrap_preserves_zero_exit_from_successful_command() {
    if !sh_available() {
        eprintln!("skipping: no POSIX sh on this host");
        return;
    }
    assert_eq!(
        run_wrapped("true"),
        0,
        "successful inner command must surface as exit 0"
    );
}

#[test]
fn wrap_preserves_nonzero_exit_from_failing_command() {
    if !sh_available() {
        eprintln!("skipping: no POSIX sh on this host");
        return;
    }
    let code = run_wrapped("false");
    assert_ne!(
        code, 0,
        "failing inner command must surface as a non-zero exit, got {code}"
    );
}

#[test]
fn wrap_preserves_specific_exit_code_42() {
    // The headline case from the round-11 brief: an inner command that exits
    // with a specific non-trivial code must surface that exact code. 42 is
    // the value the brief calls out; we test it directly so the pin is
    // unambiguous in a future grep.
    if !sh_available() {
        eprintln!("skipping: no POSIX sh on this host");
        return;
    }
    assert_eq!(
        run_wrapped("sh -c 'exit 42'"),
        42,
        "inner `sh -c 'exit 42'` must surface as exit 42"
    );
}

#[test]
fn wrap_preserves_exit_when_inner_writes_to_stderr() {
    // The wrap merges stderr into stdout via `2>&1` before piping to pack.
    // A regression that drops the merge would split the streams; a
    // regression in the brace-group ordering would lose the exit code even
    // when the merge works. Verify both at once: stderr-writing inner
    // exiting with code 3 must surface as exit 3.
    if !sh_available() {
        eprintln!("skipping: no POSIX sh on this host");
        return;
    }
    assert_eq!(
        run_wrapped("sh -c 'echo to-stderr 1>&2; exit 3'"),
        3,
        "stderr-writing failing inner must still surface its exit code"
    );
}

#[test]
fn wrap_passes_inner_command_output_through_pack() {
    // The wrap's other half: inner stdout must actually flow to pack. If
    // pack received nothing the user would still see "exit 42" but no
    // output, which is just as bad as a lost exit. A unique marker the
    // inner echoes must show up in pack's stdout (the view, possibly
    // compressed but at least non-empty for a one-line input).
    if !sh_available() {
        eprintln!("skipping: no POSIX sh on this host");
        return;
    }
    let wrapped = wrap_command(
        "sh -c 'echo round11-marker-7af2c33d'",
        knapsack_bin(),
        "round11",
        "echo",
        None,
    );
    let out = Command::new("sh")
        .arg("-c")
        .arg(&wrapped)
        .output()
        .expect("spawn sh");
    assert_eq!(out.status.code(), Some(0), "echo exit is 0");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("round11-marker-7af2c33d"),
        "inner echo's output must reach pack's stdout; got: {stdout}"
    );
}

#[test]
fn wrap_preserves_exit_code_for_inner_command_that_writes_lots_of_output() {
    // The temp-file capture mechanism is independent of how much the inner
    // command writes. But a future refactor that, say, swapped the temp-file
    // approach for `read`-on-stdout might silently break on large outputs
    // (buffer fills, blocks before exit, deadlocks). This test exercises a
    // 200-line stream from the inner and asserts the exit code still
    // propagates. (Pack handles much bigger inputs in production — this is
    // a smoke check that the pipeline doesn't choke at modest size.)
    if !sh_available() {
        eprintln!("skipping: no POSIX sh on this host");
        return;
    }
    // 200 lines of unique-ish text → ~3 KB.
    let inner = "sh -c 'for i in $(seq 1 200); do echo line $i; done; exit 7'";
    assert_eq!(
        run_wrapped(inner),
        7,
        "exit code must survive a non-trivial stdout stream"
    );
}
