//! CLI surface: numeric flags reject garbage instead of silently using the default.
//! Covers `gc --older-than`, `expand --context`, `expand --lines`, and the `why-last`
//! positional. Before this lock, each one had the same shape: a parse failure was
//! swallowed by `unwrap_or(default)`, so a typo / negative / wrong format quietly ran
//! with the WRONG numeric value and no signal anything was off. The contract everywhere:
//! present-but-unparseable -> exit 2 with a clear message naming the flag and the value;
//! absent -> use the documented default; valid -> use it.

use std::path::PathBuf;

fn knapsack_bin() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("target");
    p.push("release");
    if cfg!(windows) {
        p.push("knapsack.exe");
    } else {
        p.push("knapsack");
    }
    p
}

fn tmp_dir(tag: &str) -> PathBuf {
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let d = std::env::temp_dir().join(format!(
        "knapsack-cli-gc-{}-{}-{}",
        tag,
        std::process::id(),
        t
    ));
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn run_gc(value: Option<&str>, tag: &str) -> std::process::Output {
    let bin = knapsack_bin();
    let dir = tmp_dir(tag);
    let mut cmd = std::process::Command::new(&bin);
    cmd.arg("gc").arg("--dry-run");
    if let Some(v) = value {
        cmd.arg("--older-than").arg(v);
    }
    let out = cmd
        .env("KNAPSACK_STORE", dir.join("store"))
        .env("KNAPSACK_METRICS", dir.join("metrics.jsonl"))
        .output()
        .expect("spawn knapsack");
    let _ = std::fs::remove_dir_all(&dir);
    out
}

#[test]
fn older_than_negative_is_rejected_not_silently_defaulted() {
    let bin = knapsack_bin();
    if !bin.exists() {
        eprintln!("skipping: {} not built yet", bin.display());
        return;
    }
    let out = run_gc(Some("-5"), "neg");
    assert!(!out.status.success(), "negative input must not exit 0");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("--older-than") && stderr.contains("non-negative integer"),
        "expected a clear `--older-than ... non-negative integer` message; got: {stderr}"
    );
}

#[test]
fn older_than_non_numeric_is_rejected() {
    let bin = knapsack_bin();
    if !bin.exists() {
        return;
    }
    let out = run_gc(Some("abc"), "abc");
    assert!(!out.status.success(), "non-numeric input must not exit 0");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("--older-than"),
        "stderr must name the flag: {stderr}"
    );
}

#[test]
fn older_than_empty_is_rejected() {
    let bin = knapsack_bin();
    if !bin.exists() {
        return;
    }
    let out = run_gc(Some(""), "empty");
    assert!(!out.status.success(), "empty value must not exit 0");
}

#[test]
fn older_than_absent_uses_default_thirty_days() {
    let bin = knapsack_bin();
    if !bin.exists() {
        return;
    }
    let out = run_gc(None, "default");
    assert!(
        out.status.success(),
        "absent flag should fall through to the default"
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    // 30 days * 86_400 s/day = 2_592_000 s — the report prints this verbatim.
    assert!(
        stdout.contains("older-than 2592000 s"),
        "expected the 30-day default in the report; got:\n{stdout}"
    );
}

#[test]
fn older_than_valid_integer_is_used() {
    let bin = knapsack_bin();
    if !bin.exists() {
        return;
    }
    let out = run_gc(Some("7"), "seven");
    assert!(out.status.success(), "valid integer must succeed");
    let stdout = String::from_utf8_lossy(&out.stdout);
    // 7 days * 86_400 s/day = 604_800 s
    assert!(
        stdout.contains("older-than 604800 s"),
        "expected 7-day window in the report; got:\n{stdout}"
    );
}

// ---------- expand --context ----------

fn run_expand(extra: &[&str], tag: &str) -> std::process::Output {
    let bin = knapsack_bin();
    let dir = tmp_dir(tag);
    // A handle in the valid format but not stored — the parse-vs-validate failure must
    // happen BEFORE the store lookup, so this is fine.
    let mut cmd = std::process::Command::new(&bin);
    cmd.arg("expand")
        .arg("ks2_0123456789abcdef0123456789abcdef");
    for a in extra {
        cmd.arg(a);
    }
    let out = cmd
        .env("KNAPSACK_STORE", dir.join("store"))
        .env("KNAPSACK_METRICS", dir.join("metrics.jsonl"))
        .output()
        .expect("spawn knapsack");
    let _ = std::fs::remove_dir_all(&dir);
    out
}

#[test]
fn expand_context_non_numeric_is_rejected() {
    let bin = knapsack_bin();
    if !bin.exists() {
        return;
    }
    let out = run_expand(&["--grep", "x", "--context", "abc"], "ctx-abc");
    assert!(!out.status.success(), "--context abc must not exit 0");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("--context") && stderr.contains("non-negative integer"),
        "stderr must name the flag and complain about the value: {stderr}"
    );
}

#[test]
fn expand_context_negative_is_rejected() {
    let bin = knapsack_bin();
    if !bin.exists() {
        return;
    }
    let out = run_expand(&["--grep", "x", "--context", "-5"], "ctx-neg");
    assert!(!out.status.success(), "--context -5 must not exit 0");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("--context"),
        "stderr must name the flag: {stderr}"
    );
}

// ---------- expand --lines ----------

#[test]
fn expand_lines_garbage_is_rejected_not_silently_whole_file() {
    let bin = knapsack_bin();
    if !bin.exists() {
        return;
    }
    let out = run_expand(&["--lines", "garbage"], "lines-junk");
    assert!(!out.status.success(), "--lines garbage must not exit 0");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("--lines") && stderr.contains("A-B"),
        "stderr must name the flag and the expected format: {stderr}"
    );
}

#[test]
fn expand_lines_one_endpoint_only_is_rejected() {
    let bin = knapsack_bin();
    if !bin.exists() {
        return;
    }
    // "5" has no `-` separator -> parse_range -> None -> error (used to silently expand
    // the whole file as if --lines had been omitted).
    let out = run_expand(&["--lines", "5"], "lines-bare");
    assert!(
        !out.status.success(),
        "--lines 5 (no range) must not exit 0"
    );
}

// ---------- why-last positional ----------

fn run_why_last(arg: Option<&str>, tag: &str) -> std::process::Output {
    let bin = knapsack_bin();
    let dir = tmp_dir(tag);
    let mut cmd = std::process::Command::new(&bin);
    cmd.arg("why-last");
    if let Some(a) = arg {
        cmd.arg(a);
    }
    let out = cmd
        .env("KNAPSACK_READ_LOG", dir.join("read_hook.jsonl"))
        .env("KNAPSACK_STORE", dir.join("store"))
        .output()
        .expect("spawn knapsack");
    let _ = std::fs::remove_dir_all(&dir);
    out
}

#[test]
fn why_last_non_numeric_is_rejected() {
    let bin = knapsack_bin();
    if !bin.exists() {
        return;
    }
    let out = run_why_last(Some("abc"), "why-abc");
    assert!(!out.status.success(), "why-last abc must not exit 0");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("why-last") && stderr.contains("non-negative integer"),
        "stderr must name the command and complain: {stderr}"
    );
}

#[test]
fn why_last_absent_uses_default() {
    let bin = knapsack_bin();
    if !bin.exists() {
        return;
    }
    let out = run_why_last(None, "why-default");
    assert!(
        out.status.success(),
        "why-last with no arg should use default 10 and exit 0"
    );
}

#[test]
fn why_last_valid_integer_is_used() {
    let bin = knapsack_bin();
    if !bin.exists() {
        return;
    }
    let out = run_why_last(Some("5"), "why-five");
    assert!(out.status.success(), "why-last 5 must exit 0");
}
