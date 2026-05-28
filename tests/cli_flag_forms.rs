//! End-to-end proof that the CLI accepts BOTH `--name VALUE` and `--name=VALUE`
//! forms across every flag the binary parses.
//!
//! Before this fix the equals form was silently dropped — `--session=mysess`
//! matched no exact arg, so the entire flag fell through and the caller's
//! default kicked in. The first-observed symptom was packs landing in the
//! default `cli` session (polluting per-session metrics), but every flag in
//! the CLI surface was equally affected. These tests pin that the fix applies
//! UNIFORMLY: each test exercises the equals form for a specific subcommand,
//! then verifies the downstream effect (metrics tag, expand slice, gc behavior,
//! pack-doc output path).
//!
//! Integration-style: we run the compiled binary via std::process::Command,
//! sandboxed through KNAPSACK_* env overrides so the user's real ~/.knapsack
//! is never touched.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Find the test target binary. Cargo puts integration tests in the same
/// profile as the binary, so `release` integration tests find the release
/// binary and `debug` find the debug one — no manual selection required.
fn bin() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("target");
    p.push(if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    });
    p.push(if cfg!(windows) {
        "knapsack.exe"
    } else {
        "knapsack"
    });
    p
}

fn sandbox(tag: &str) -> PathBuf {
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let p = std::env::temp_dir().join(format!("kn-cliflag-{}-{}-{}", tag, std::process::id(), t));
    std::fs::create_dir_all(&p).unwrap();
    p
}

/// Run the binary with sandbox env overrides + the supplied args. Optionally
/// pipes `stdin` in. Returns (stdout, stderr, exit_code).
fn run(sb: &Path, args: &[&str], stdin: Option<&[u8]>) -> (String, String, i32) {
    let mut cmd = Command::new(bin());
    cmd.args(args)
        .env("KNAPSACK_STORE", sb.join("store"))
        .env("KNAPSACK_SESSIONS", sb.join("sessions"))
        .env("KNAPSACK_METRICS", sb.join("metrics.jsonl"))
        .env("KNAPSACK_READ_CACHE", sb.join("read_cache"))
        .env("KNAPSACK_READ_LOG", sb.join("read_hook.jsonl"))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if stdin.is_some() {
        cmd.stdin(Stdio::piped());
    }
    let mut child = cmd.spawn().expect("spawn");
    if let Some(data) = stdin {
        child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(data)
            .expect("stdin write");
        drop(child.stdin.take());
    }
    let out = child.wait_with_output().expect("wait");
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.code().unwrap_or(-1),
    )
}

// ---------- pack: --session, --cmd, --type, --transcript ----------

#[test]
fn pack_session_flag_equals_form_lands_in_correct_metrics_session() {
    let sb = sandbox("pack-session-eq");
    let payload = b"line1\nline2\nline3 of test output\nline4\nline5\n";
    let (_, stderr, code) = run(
        &sb,
        &[
            "pack",
            "-",
            "--session=eqsess",
            "--cmd",
            "test",
            "--type",
            "log",
        ],
        Some(payload),
    );
    assert_eq!(code, 0, "pack must succeed; stderr was: {stderr}");
    let metrics = std::fs::read_to_string(sb.join("metrics.jsonl"))
        .expect("metrics file must be written by pack_output");
    assert!(
        metrics.contains(r#""session":"eqsess""#),
        "metrics MUST record eqsess (the equals-form value), not fall back to default; got:\n{metrics}"
    );
    assert!(
        !metrics.contains(r#""session":"cli""#),
        "metrics MUST NOT contain a 'cli' default-session entry (would mean the equals-form was silently dropped); got:\n{metrics}"
    );
    let _ = std::fs::remove_dir_all(&sb);
}

#[test]
fn pack_session_flag_space_form_still_works() {
    // Regression: the fix must not break the long-standing space form.
    let sb = sandbox("pack-session-space");
    let payload = b"output line\nmore output\nanother line\n";
    let (_, _, code) = run(
        &sb,
        &[
            "pack",
            "-",
            "--session",
            "spcsess",
            "--cmd",
            "test",
            "--type",
            "log",
        ],
        Some(payload),
    );
    assert_eq!(code, 0);
    let metrics = std::fs::read_to_string(sb.join("metrics.jsonl")).unwrap();
    assert!(
        metrics.contains(r#""session":"spcsess""#),
        "space form unchanged; got:\n{metrics}"
    );
    let _ = std::fs::remove_dir_all(&sb);
}

#[test]
fn pack_mixed_forms_in_one_invocation_work() {
    // A user reaches for one form for one flag and another form for the next;
    // every flag must resolve independently.
    let sb = sandbox("pack-mixed");
    let payload = b"some long-enough output to pack\nlinetwo\nlinethree\n";
    let (_, _, code) = run(
        &sb,
        &[
            "pack",
            "-",
            "--session=mixsess",
            "--cmd",
            "cargo",
            "--type=log",
        ],
        Some(payload),
    );
    assert_eq!(code, 0);
    let metrics = std::fs::read_to_string(sb.join("metrics.jsonl")).unwrap();
    assert!(
        metrics.contains(r#""session":"mixsess""#),
        "session via equals form recognized"
    );
    let _ = std::fs::remove_dir_all(&sb);
}

// ---------- expand: --lines, --grep, --context, --session ----------

/// Helper: store exact bytes via `knapsack store put` and return the handle.
/// This is the deterministic test path — no risk of the pack pipeline's
/// never-worse-than-raw guard firing on small fixtures and not emitting a
/// recall handle. The byte-exact handle returned is what `expand` operates on,
/// which is exactly what these tests need to drive the slicing API.
fn store_put(sb: &Path, payload: &[u8], filename: &str) -> String {
    let p = sb.join(filename);
    std::fs::write(&p, payload).unwrap();
    let (out, stderr, code) = run(sb, &["store", "put", p.to_str().unwrap()], None);
    assert_eq!(code, 0, "store put must succeed; stderr: {stderr}");
    let handle = out.trim().to_string();
    assert!(
        handle.starts_with("ks2_") && handle.len() == 36,
        "store put must return a ks2_<32 hex> handle; got: {handle:?}"
    );
    handle
}

#[test]
fn expand_lines_equals_form_parses_and_slices_correctly() {
    let sb = sandbox("expand-lines-eq");
    let payload = b"first\nsecond\nthird\nfourth\nfifth\nsixth\nseventh\n";
    let handle = store_put(&sb, payload, "lines.txt");

    // Equals form: --lines=2-4 must return lines 2..=4 (1-based inclusive)
    let (out, stderr, code) = run(&sb, &["expand", &handle, "--lines=2-4"], None);
    assert_eq!(
        code, 0,
        "expand with --lines= must succeed; stderr: {stderr}"
    );
    let lines: Vec<&str> = out.trim_end().split('\n').collect();
    assert_eq!(
        lines,
        vec!["second", "third", "fourth"],
        "equals form slicing produced {:?}",
        lines
    );
    let _ = std::fs::remove_dir_all(&sb);
}

#[test]
fn expand_context_equals_form_parses_as_number() {
    // --context expects a non-negative integer. Equals form must reach the
    // numeric parser, not silently become 0.
    let sb = sandbox("expand-ctx-eq");
    let payload = b"unrelated1\nunrelated2\nMATCH_HERE\nunrelated3\nunrelated4\n";
    let handle = store_put(&sb, payload, "ctx.txt");

    let (out, stderr, code) = run(
        &sb,
        &["expand", &handle, "--grep=MATCH", "--context=1"],
        None,
    );
    assert_eq!(code, 0, "stderr: {stderr}");
    let lines: Vec<&str> = out.trim_end().split('\n').collect();
    // Context 1 around MATCH_HERE: previous line + match + next line = 3 lines
    assert_eq!(
        lines.len(),
        3,
        "context=1 returns 3 lines (prev+match+next); got {:?}",
        lines
    );
    assert!(lines.contains(&"MATCH_HERE"));
    let _ = std::fs::remove_dir_all(&sb);
}

#[test]
fn expand_grep_equals_form_works() {
    let sb = sandbox("expand-grep-eq");
    let payload = b"alpha\nbeta\nFINDME-needle\ngamma\nFINDME-another\ndelta\n";
    let handle = store_put(&sb, payload, "grep.txt");

    let (out, stderr, code) = run(&sb, &["expand", &handle, "--grep=FINDME"], None);
    assert_eq!(code, 0, "stderr: {stderr}");
    let lines: Vec<&str> = out
        .trim_end()
        .split('\n')
        .filter(|s| !s.is_empty())
        .collect();
    assert_eq!(
        lines.len(),
        2,
        "grep should match both FINDME lines; got {:?}",
        lines
    );
    let _ = std::fs::remove_dir_all(&sb);
}

#[test]
fn expand_bad_equals_value_still_rejects_loudly() {
    // The equals form must route through the SAME numeric-validation gate as
    // the space form — a garbage value must exit 2 with a clear message, not
    // silently fall back to defaults.
    let sb = sandbox("expand-bad-eq");
    let payload = b"line1\nline2\nline3\nline4\n";
    let handle = store_put(&sb, payload, "bad.txt");

    let (_, stderr, code) = run(&sb, &["expand", &handle, "--lines=garbage"], None);
    assert_eq!(code, 2, "garbage --lines= must exit 2 (loud reject)");
    assert!(
        stderr.contains("--lines") && stderr.contains("garbage"),
        "stderr must name the offending flag and value; got: {stderr}"
    );
    let _ = std::fs::remove_dir_all(&sb);
}

// ---------- gc: --older-than ----------

#[test]
fn gc_older_than_equals_form_parses_as_days() {
    // --older-than expects a non-negative integer (days). Equals form must
    // parse through the same validator as space form.
    let sb = sandbox("gc-older-eq");
    // `--dry-run` keeps it side-effect-free; an empty store still produces a
    // valid report with 0/0 counts.
    let (out, _, code) = run(&sb, &["gc", "--older-than=7", "--dry-run"], None);
    assert_eq!(code, 0, "gc with --older-than=N must succeed");
    assert!(
        out.contains("knapsack gc"),
        "gc must produce its report; got:\n{out}"
    );
    let _ = std::fs::remove_dir_all(&sb);
}

#[test]
fn gc_older_than_equals_form_bad_value_rejects() {
    let sb = sandbox("gc-older-eq-bad");
    let (_, stderr, code) = run(&sb, &["gc", "--older-than=notanumber", "--dry-run"], None);
    assert_eq!(code, 2, "garbage --older-than= must exit 2");
    assert!(
        stderr.contains("--older-than") && stderr.contains("notanumber"),
        "stderr must name the offending flag and value; got: {stderr}"
    );
    let _ = std::fs::remove_dir_all(&sb);
}

// ---------- --session: empty + very long (dogfood-surfaced bugs) ----------

#[test]
fn pack_session_empty_value_loud_rejects_via_space_form() {
    // Pre-fix: `pack - --session ""` silently used "" as the session id, which
    // (a) created a hidden `.tsv` zero-basename file and (b) tagged metrics
    // with an empty session, breaking per-session filtering. Now: exit 2 with
    // a clear error pointing at the user's mistake. The shell-substitution
    // mistake (`--session "$UNSET_VAR"`) is the most likely real-world cause.
    let sb = sandbox("empty-session-space");
    let (_, stderr, code) = run(
        &sb,
        &[
            "pack",
            "-",
            "--session",
            "",
            "--cmd",
            "test",
            "--type",
            "log",
        ],
        Some(b"some output\n"),
    );
    assert_eq!(code, 2, "empty --session must exit 2; stderr: {stderr}");
    assert!(
        stderr.contains("--session was given an empty value"),
        "stderr must explain the empty-value rejection; got: {stderr}"
    );
    // And no metrics entry should have been written for an empty session.
    let metrics_path = sb.join("metrics.jsonl");
    if metrics_path.exists() {
        let metrics = std::fs::read_to_string(&metrics_path).unwrap();
        assert!(
            !metrics.contains(r#""session":""#) || !metrics.contains(r#""session":"""#),
            "no metrics row with empty session string should have been written; got: {metrics}"
        );
    }
    let _ = std::fs::remove_dir_all(&sb);
}

#[test]
fn pack_session_empty_value_loud_rejects_via_equals_form() {
    // Symmetric pin for the equals form. After the previous bugfix that made
    // `--session=value` parse, `--session=` (empty after =) reaches the same
    // empty-value gate and must produce the same loud error.
    let sb = sandbox("empty-session-eq");
    let (_, stderr, code) = run(
        &sb,
        &["pack", "-", "--session=", "--cmd", "test", "--type", "log"],
        Some(b"some output\n"),
    );
    assert_eq!(code, 2, "empty --session= must exit 2; stderr: {stderr}");
    assert!(
        stderr.contains("--session was given an empty value"),
        "stderr must explain the empty-value rejection; got: {stderr}"
    );
    let _ = std::fs::remove_dir_all(&sb);
}

#[test]
fn pack_session_whitespace_only_loud_rejects() {
    // A space / tab alone is the same shape of mistake as empty — most often
    // a `--session " "` argument from a shell quoting accident. We trim before
    // the check so any whitespace-only value gets the same loud reject.
    let sb = sandbox("ws-session");
    let (_, stderr, code) = run(
        &sb,
        &[
            "pack",
            "-",
            "--session",
            "   ",
            "--cmd",
            "test",
            "--type",
            "log",
        ],
        Some(b"some output\n"),
    );
    assert_eq!(
        code, 2,
        "whitespace --session must exit 2; stderr: {stderr}"
    );
    assert!(
        stderr.contains("--session"),
        "stderr names the flag: {stderr}"
    );
    let _ = std::fs::remove_dir_all(&sb);
}

#[test]
fn expand_session_empty_value_loud_rejects_too() {
    // Symmetric protection on the expand path. expand uses --session to
    // attribute the refetch cost; an empty session there would have the same
    // bug shape as on pack (mis-tagged metrics + hidden file).
    let sb = sandbox("empty-session-expand");
    // First put a known handle so we have something to expand.
    let payload = b"line1\nline2\nline3\nline4\n";
    let p = sb.join("seed.txt");
    std::fs::write(&p, payload).unwrap();
    let (out, _, _) = run(&sb, &["store", "put", p.to_str().unwrap()], None);
    let handle = out.trim().to_string();

    let (_, stderr, code) = run(&sb, &["expand", &handle, "--session", ""], None);
    assert_eq!(
        code, 2,
        "expand with empty --session must exit 2; stderr: {stderr}"
    );
    assert!(
        stderr.contains("--session"),
        "stderr names the flag: {stderr}"
    );
    let _ = std::fs::remove_dir_all(&sb);
}

#[test]
fn pack_with_500_char_session_succeeds_and_persists_ledger() {
    // Pre-fix: a 500-char session id produced a 500-char filename that
    // overflowed Windows MAX_PATH; the underlying `fs::write` in `ledger::save`
    // returned Err which was swallowed (`let _ = ...`), so the session ledger
    // never persisted. The user got cold compression on every subsequent pack
    // in that session with NO warning. Post-fix: `session_path` caps the
    // basename at 128 chars + 16-hex SHA-1 tail for collision-resistance, so
    // the file always writes regardless of input length.
    let sb = sandbox("long-session");
    let payload: Vec<u8> = b"output line for long-session test\n".repeat(50);
    let long: String = "a".repeat(500);
    let (_, stderr, code) = run(
        &sb,
        &[
            "pack",
            "-",
            "--session",
            &long,
            "--cmd",
            "test",
            "--type",
            "log",
        ],
        Some(&payload),
    );
    assert_eq!(
        code, 0,
        "500-char session must pack successfully; stderr: {stderr}"
    );

    // The session ledger MUST have been persisted (sub-128-char filename + .tsv).
    let sessions_dir = sb.join("sessions");
    let files: Vec<_> = std::fs::read_dir(&sessions_dir)
        .expect("sessions dir must exist")
        .filter_map(|e| e.ok())
        .collect();
    assert_eq!(
        files.len(),
        1,
        "exactly one session file expected, got {:?}",
        files.iter().map(|f| f.file_name()).collect::<Vec<_>>()
    );
    let fname = files[0].file_name().to_string_lossy().into_owned();
    assert!(
        fname.ends_with(".tsv"),
        "session file must end in .tsv: {fname}"
    );
    assert!(
        fname.len() <= 132,
        "session filename must be capped at MAX_SESSION_BASENAME(128) + '.tsv'(4); got {} chars: {fname}",
        fname.len()
    );

    // And the file must not be empty — i.e. the ledger actually wrote.
    let ledger_bytes = std::fs::read(files[0].path()).unwrap();
    assert!(
        !ledger_bytes.is_empty(),
        "ledger file must contain at least one block entry"
    );

    let _ = std::fs::remove_dir_all(&sb);
}

// ---------- pack <file>: --output ----------

#[test]
fn pack_doc_output_equals_form_writes_to_chosen_path() {
    // `knapsack pack <file>` uses --output to override the default side-car
    // location. Equals form must reach the path arg, not silently fall back.
    let sb = sandbox("packdoc-output-eq");
    let src = sb.join("input.md");
    // Make the doc large enough that pack_doc wants to elide something —
    // long-prose threshold is 500+ chars on a single line, or 3+ lines ≥300 chars.
    let big = "x".repeat(800);
    std::fs::write(&src, format!("# Heading\n\n{big}\n\nshort line\n")).unwrap();
    let custom = sb.join("custom-output.md");

    let (_, stderr, code) = run(
        &sb,
        &[
            "pack",
            src.to_str().unwrap(),
            &format!("--output={}", custom.display()),
            "--force",
        ],
        None,
    );
    assert_eq!(code, 0, "pack <file> must succeed; stderr: {stderr}");
    assert!(
        custom.exists(),
        "the --output= path must be written to (not the default side-car)"
    );
    let default_sidecar = sb.join("input.knapsack.md");
    assert!(
        !default_sidecar.exists(),
        "the default side-car must NOT exist when --output= was specified"
    );
    let _ = std::fs::remove_dir_all(&sb);
}
