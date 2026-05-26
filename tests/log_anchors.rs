//! Anchor coverage for real-world compiler / test-runner output. Each fixture is a
//! long log with an error stanza in the MIDDLE — far enough from the head/tail that the
//! log compressor would elide it if not for `important()`. The contract pinned here is:
//! the diagnostic anchor lines survive into the compact view.
//!
//! When you add a new language, add a fixture here. Failing this test means a class of
//! debugging output silently disappears behind the elision marker.

use knapsack::content_type::ContentType;
use knapsack::ledger::Ledger;
use knapsack::pack::pack;
use knapsack::store::Store;
use std::path::PathBuf;

fn store_dir(tag: &str) -> PathBuf {
    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    let d = std::env::temp_dir().join(format!("knapsack-anchors-{}-{}-{}", tag, std::process::id(), t));
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// Pad a fixture with N lines of routine output before + after the anchor stanza, so the
/// log compressor is actually FORCED to elide the middle (n > LOG_HEAD + LOG_TAIL + 8).
fn buried(prefix_count: usize, anchor: &str, suffix_count: usize) -> Vec<u8> {
    let mut s = String::new();
    for i in 0..prefix_count {
        s.push_str(&format!("[INFO] step {i}: doing routine work, no problems here\n"));
    }
    s.push_str(anchor);
    if !anchor.ends_with('\n') {
        s.push('\n');
    }
    for i in 0..suffix_count {
        s.push_str(&format!("[INFO] more routine output line {i}\n"));
    }
    s.into_bytes()
}

/// Pack with Log content-type + an empty ledger; return the compact view.
fn pack_view(bytes: &[u8], tag: &str) -> String {
    let store = Store::new(store_dir(tag));
    let mut ledger = Ledger::in_memory();
    let r = pack(bytes, ContentType::Log, &store, &mut ledger, 0);
    r.view
}

fn assert_anchored(view: &str, needles: &[&str], tag: &str) {
    for n in needles {
        assert!(
            view.contains(n),
            "[{}] anchor missing from compact view; expected `{}` to survive elision.\n--- view ---\n{}\n--- end view ---",
            tag,
            n,
            view
        );
    }
}

#[test]
fn python_traceback_survives_elision() {
    // The Python interpreter's standard traceback: a header + N frame lines + the
    // exception. None of the frame lines contain "error" / "warn" / etc., so without
    // a dedicated anchor they vanish into the middle.
    let anchor = "\
Traceback (most recent call last):
  File \"app/main.py\", line 42, in handler
    result = compute(payload)
  File \"app/lib.py\", line 17, in compute
    return x / y
ZeroDivisionError: division by zero
";
    let bytes = buried(40, anchor, 40);
    let view = pack_view(&bytes, "py");
    assert_anchored(
        &view,
        &[
            "Traceback (most recent call last)",
            "File \"app/main.py\", line 42, in handler",
            "ZeroDivisionError",
        ],
        "python",
    );
}

#[test]
fn node_stack_frames_survive_elision() {
    // Node.js stack frames are anchored by the leading `at ` token, not by any severity
    // keyword. The "Error: …" line itself is caught by "error", but the frames must be
    // visible too — that's where you read which file is to blame.
    let anchor = "\
Error: ECONNREFUSED 127.0.0.1:5432
    at TCPConnectWrap.afterConnect [as oncomplete] (node:net:1494:16)
    at Pool.connect (/app/node_modules/pg/lib/pool.js:222:13)
    at handler (/app/src/http.js:14:9)
";
    let bytes = buried(40, anchor, 40);
    let view = pack_view(&bytes, "node");
    assert_anchored(
        &view,
        &[
            "Error: ECONNREFUSED",
            "at TCPConnectWrap.afterConnect",
            "at handler",
        ],
        "node",
    );
}

#[test]
fn npm_err_lines_survive_elision() {
    // `npm ERR!` contains the substring `err` but not `error`; the old generic list
    // missed this, so we anchored on `npm err!` (lowercased) directly.
    let anchor = "\
npm ERR! code ELIFECYCLE
npm ERR! errno 1
npm ERR! mypkg@0.1.0 build: `tsc -p .`
npm ERR! Exit status 1
";
    let bytes = buried(40, anchor, 40);
    let view = pack_view(&bytes, "npm");
    assert_anchored(
        &view,
        &["npm ERR! code ELIFECYCLE", "npm ERR! mypkg@0.1.0 build"],
        "npm",
    );
}

#[test]
fn rust_cargo_error_codes_survive_elision() {
    // `error[E0XXX]: …` is the Rust diagnostic format. Caught by the generic "error"
    // keyword today, but pinning it explicitly protects against future keyword trims.
    let anchor = "\
error[E0277]: the trait bound `Foo: Bar` is not satisfied
  --> src/lib.rs:42:9
   |
42 |         foo.bar()
   |             ^^^ the trait `Bar` is not implemented for `Foo`
error: aborting due to 1 previous error
";
    let bytes = buried(40, anchor, 40);
    let view = pack_view(&bytes, "rust");
    assert_anchored(&view, &["error[E0277]", "aborting due to 1 previous error"], "rust");
}

#[test]
fn typescript_tsc_diagnostics_survive_elision() {
    let anchor = "\
src/foo.ts(12,34): error TS2322: Type 'string' is not assignable to type 'number'.
src/bar.ts(1,1): error TS2304: Cannot find name 'unknownSymbol'.
";
    let bytes = buried(40, anchor, 40);
    let view = pack_view(&bytes, "ts");
    assert_anchored(
        &view,
        &["error TS2322", "error TS2304"],
        "typescript",
    );
}

#[test]
fn gradle_caused_by_and_what_went_wrong_survive_elision() {
    // Gradle splits the diagnostic across multiple sections; `* What went wrong:` is the
    // header you scroll back to, `Caused by:` is the actual root cause.
    let anchor = "\
* What went wrong:
Execution failed for task ':compileJava'.
> Compilation failed; see the compiler error output for details.

Caused by: java.lang.NullPointerException
\tat com.example.Service.process(Service.java:42)
\tat com.example.App.main(App.java:11)
";
    let bytes = buried(40, anchor, 40);
    let view = pack_view(&bytes, "gradle");
    assert_anchored(
        &view,
        &["* What went wrong:", "Execution failed for task", "Caused by: java.lang.NullPointerException"],
        "gradle",
    );
}

#[test]
fn pytest_failure_summary_survives_elision() {
    // pytest's summary lines: `FAILED tests/x.py::test_foo - ExceptionName: ...`.
    // Caught by "fail" today; the test pins it so a future re-shuffle doesn't break it.
    let anchor = "\
FAILED tests/test_users.py::test_create - sqlalchemy.exc.IntegrityError
FAILED tests/test_users.py::test_delete - AssertionError: expected 1, got 0
1 passed, 2 failed in 0.42s
";
    let bytes = buried(40, anchor, 40);
    let view = pack_view(&bytes, "pytest");
    assert_anchored(
        &view,
        &[
            "FAILED tests/test_users.py::test_create",
            "FAILED tests/test_users.py::test_delete",
        ],
        "pytest",
    );
}

#[test]
fn bazel_error_with_build_path_survives_elision() {
    // Bazel's `ERROR: /path/BUILD:1:2: …` — already caught by "error" keyword, pinned
    // here so the path-and-line bit survives intact (not just the keyword).
    let anchor = "\
ERROR: /workspace/src/BUILD.bazel:42:11: Compiling src/main.cc failed: undeclared inclusion(s) in rule '//src:main'
INFO: Elapsed time: 1.234s, Critical Path: 0.05s
FAILED: Build did NOT complete successfully
";
    let bytes = buried(40, anchor, 40);
    let view = pack_view(&bytes, "bazel");
    assert_anchored(
        &view,
        &["ERROR: /workspace/src/BUILD.bazel:42:11", "Build did NOT complete successfully"],
        "bazel",
    );
}

#[test]
fn never_worse_than_raw_guard_still_holds() {
    // Diffuse change / no-elision fixture: a flat list of unique short lines.
    // The compact view must never exceed the raw input — pack.rs falls back to the
    // stateless single-pass compressor when the conditional view is fragmented.
    use knapsack::token_estimate::tokens_bytes;
    let mut bytes = Vec::new();
    for i in 0..200 {
        bytes.extend_from_slice(format!("plain output line {i}\n").as_bytes());
    }
    let store = Store::new(store_dir("guard"));
    let mut ledger = Ledger::in_memory();
    let r = pack(&bytes, ContentType::Log, &store, &mut ledger, 0);
    let raw = tokens_bytes(&bytes);
    assert!(
        r.shown_tokens_est <= raw,
        "never-worse-than-raw: shown={} > raw={}",
        r.shown_tokens_est,
        raw
    );
}
