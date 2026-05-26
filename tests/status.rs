//! `/knapsack` (alias `knapsack status`) is the product-facing summary. Tests pin the
//! contract that matters: the headline shape, the Input/Output reduction lines, the
//! savings/recall/store block, and the inactive→install hint. No env-var jargon, no
//! EXPERIMENTAL/dogfood mention, no slash-command laundry list (that's all `doctor`).
//!
//! Paths are injected via the `Paths` struct so tests never touch the user's real
//! ~/.claude or ~/.knapsack (same pattern the installer uses for KNAPSACK_SETTINGS etc).

use knapsack::status::{collect_from, render, Paths};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock};

fn tmpdir(tag: &str) -> PathBuf {
    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    let d = std::env::temp_dir().join(format!("knapsack-status-{}-{}-{}", tag, std::process::id(), t));
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn write_file(p: &Path, contents: &str) {
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::File::create(p).unwrap().write_all(contents.as_bytes()).unwrap();
}

fn paths(dir: &Path) -> Paths {
    Paths {
        settings: dir.join("settings.json"),
        mcp_config: dir.join("claude.json"),
        metrics: dir.join("metrics.jsonl"),
        store: dir.join("store"),
    }
}

fn settings_with_hook(bin: &str) -> String {
    format!(
        r#"{{"hooks":{{"PreToolUse":[{{"matcher":"Bash","hooks":[{{"type":"command","command":"\"{}\" hook"}}]}}]}}}}"#,
        bin
    )
}

fn mcp_with_server(bin: &str) -> String {
    format!(r#"{{"mcpServers":{{"knapsack":{{"command":"{}","args":["mcp"]}}}}}}"#, bin)
}

// `render` calls `config::read_hook_enabled()` which reads the process env. Any test
// that wants to assert on the "Input reduction:" line therefore has to lock the env
// for the duration of the call AND restore prior state on drop — exactly the same
// hazard as tests/read_hook.rs. Zero-dep policy forbids serial_test, so we DIY.
fn env_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(())).lock().unwrap_or_else(|e| e.into_inner())
}

struct EnvGuard {
    _lock: MutexGuard<'static, ()>,
    prior: Option<std::ffi::OsString>,
}

impl EnvGuard {
    fn with(value: Option<&str>) -> Self {
        let lock = env_lock();
        let prior = std::env::var_os("KNAPSACK_READ_HOOK");
        match value {
            Some(v) => std::env::set_var("KNAPSACK_READ_HOOK", v),
            None => std::env::remove_var("KNAPSACK_READ_HOOK"),
        }
        Self { _lock: lock, prior }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        match self.prior.take() {
            Some(v) => std::env::set_var("KNAPSACK_READ_HOOK", v),
            None => std::env::remove_var("KNAPSACK_READ_HOOK"),
        }
    }
}

#[test]
fn inactive_when_hook_missing_and_explains_how_to_enable() {
    let dir = tmpdir("inactive");
    let p = paths(&dir);
    // no settings.json, no mcp config -> hook off
    let s = collect_from(&p);
    let out = render(&s);

    assert!(out.contains("Knapsack inactive"), "must lead with inactive header:\n{}", out);
    assert!(out.contains("knapsack install"), "must tell user how to fix it:\n{}", out);
    // Inactive output is intentionally minimal — no Input/Output reduction lines,
    // no Modes block, no actions laundry list. Just the one-liner fix.
    assert!(!out.contains("Input reduction"), "inactive should not advertise inputs as anything:\n{}", out);
    assert!(!out.contains("Output reduction"), "inactive should not advertise outputs as anything:\n{}", out);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn active_with_no_activity_shows_clean_surface() {
    let _env = EnvGuard::with(None); // default-on
    let dir = tmpdir("idle");
    let p = paths(&dir);
    write_file(&p.settings, &settings_with_hook("/bin/knapsack"));
    write_file(&p.mcp_config, &mcp_with_server("/bin/knapsack"));

    let s = collect_from(&p);
    let out = render(&s);

    assert!(out.starts_with("Knapsack active\n"), "header reflects on-disk wiring:\n{}", out);
    assert!(out.contains("Input reduction:  active"),  "input line on, given default read-hook gate:\n{}", out);
    assert!(out.contains("Output reduction: active"), "output line on, given hook+MCP wired:\n{}", out);
    assert!(out.contains("no activity yet"), "no metrics -> say so, do not print 0/0:\n{}", out);
    assert!(out.contains("Recall:           idle"), "no activity -> recall is idle, not 'healthy':\n{}", out);
    assert!(out.contains("Store:            empty"), "empty store -> say empty:\n{}", out);
    // None of the cruft from the old surface should appear here.
    assert!(!out.contains("EXPERIMENTAL"), "no EXPERIMENTAL label:\n{}", out);
    assert!(!out.contains("dogfood"), "no dogfood mention:\n{}", out);
    assert!(!out.contains("KNAPSACK_READ_HOOK"), "no env-var hint:\n{}", out);
    assert!(!out.contains("/knapsack pack"), "no actions block:\n{}", out);
    assert!(!out.contains("/knapsack doctor"), "no actions block:\n{}", out);
    assert!(!out.contains("Modes"), "no Modes block:\n{}", out);
    assert!(!out.contains("Lifetime:"), "single session -> no lifetime footer:\n{}", out);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn input_reduction_reads_off_when_explicit_offswitch_set() {
    let _env = EnvGuard::with(Some("0"));
    let dir = tmpdir("input-off");
    let p = paths(&dir);
    write_file(&p.settings, &settings_with_hook("/bin/knapsack"));
    write_file(&p.mcp_config, &mcp_with_server("/bin/knapsack"));

    let s = collect_from(&p);
    let out = render(&s);

    assert!(out.contains("Knapsack active"), "header still active when only input is disabled:\n{}", out);
    assert!(out.contains("Input reduction:  off"), "explicit `KNAPSACK_READ_HOOK=0` -> input reads off:\n{}", out);
    assert!(out.contains("Output reduction: active"), "output is unaffected by the input off-switch:\n{}", out);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn output_reduction_reads_off_when_mcp_missing() {
    let _env = EnvGuard::with(None);
    let dir = tmpdir("output-off");
    let p = paths(&dir);
    write_file(&p.settings, &settings_with_hook("/bin/knapsack"));
    // no mcp config

    let s = collect_from(&p);
    let out = render(&s);

    assert!(out.contains("Knapsack active"), "hook installed -> active even with MCP missing:\n{}", out);
    assert!(out.contains("Output reduction: off"), "MCP missing -> output reads off:\n{}", out);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn healthy_session_shows_net_saved_and_reduction_percent() {
    let _env = EnvGuard::with(None);
    let dir = tmpdir("healthy");
    let p = paths(&dir);
    write_file(&p.settings, &settings_with_hook("/bin/knapsack"));
    write_file(&p.mcp_config, &mcp_with_server("/bin/knapsack"));
    // one session: raw=10000, shown=2000 (saved 8000), one expand refetches 600.
    // net = 8000 - 600 = 7400. net/raw = 74%.
    write_file(
        &p.metrics,
        concat!(
            r#"{"t":100,"event":"compress","session":"sess-A","raw":10000,"shown":2000,"saved":8000,"delta_hits":4,"evicted":0}"#,
            "\n",
            r#"{"t":200,"event":"expand","session":"sess-A","tokens":600,"ok":true}"#,
            "\n",
        ),
    );
    // a couple of fake store files in the sharded layout
    std::fs::create_dir_all(p.store.join("ab")).unwrap();
    write_file(&p.store.join("ab/ks_ab1234"), &"x".repeat(2048));
    write_file(&p.store.join("ab/ks_ab5678"), &"y".repeat(1024));

    let s = collect_from(&p);
    let out = render(&s);

    // Compress events: raw=10000, saved=8000. Expand: 600 tokens refetched.
    // Saved is GROSS (the compress benefit), Refetched is the recall cost, Net% is
    // the rolled-up reduction (net / raw = (8000-600)/10000 = 74%).
    assert!(out.contains("Session saved:    8,000 tokens"), "shows gross saved (not net):\n{}", out);
    assert!(out.contains("Refetched:        600 tokens"), "refetch cost shown separately:\n{}", out);
    assert!(out.contains("Net reduction:    74%"), "shows net/raw as integer percent:\n{}", out);
    assert!(out.contains("Recall:           healthy"), "no failed expands -> healthy:\n{}", out);
    assert!(out.contains("Store:            2 blocks"), "store block count rendered:\n{}", out);
    assert!(out.contains("3 KB"), "store bytes rendered in KB:\n{}", out);
    assert!(!out.contains("Lifetime:"), "single session -> no lifetime footer:\n{}", out);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn recall_failure_in_latest_session_is_a_warning() {
    let _env = EnvGuard::with(None);
    let dir = tmpdir("recallfail");
    let p = paths(&dir);
    write_file(&p.settings, &settings_with_hook("/bin/knapsack"));
    write_file(&p.mcp_config, &mcp_with_server("/bin/knapsack"));
    write_file(
        &p.metrics,
        concat!(
            r#"{"t":100,"event":"compress","session":"sess-B","raw":5000,"shown":500,"saved":4500,"delta_hits":0,"evicted":0}"#,
            "\n",
            r#"{"t":200,"event":"expand","session":"sess-B","tokens":0,"ok":false}"#,
            "\n",
            r#"{"t":300,"event":"expand","session":"sess-B","tokens":0,"ok":false}"#,
            "\n",
        ),
    );

    let s = collect_from(&p);
    let out = render(&s);

    assert!(out.contains("⚠ 2 failed"), "two failed expands surface as a warning:\n{}", out);
    assert!(out.contains("knapsack doctor"), "warning should point at the diagnostic:\n{}", out);
    assert!(!out.contains("Recall:           healthy"), "must NOT also claim healthy:\n{}", out);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn old_session_failures_dont_poison_a_clean_current_session() {
    // Old session had failures; the latest session is clean. The header line should reflect
    // the LATEST session (healthy), but the lifetime footer should still own up to the
    // historical failures so over-expanding isn't quietly forgotten.
    let _env = EnvGuard::with(None);
    let dir = tmpdir("oldfail");
    let p = paths(&dir);
    write_file(&p.settings, &settings_with_hook("/bin/knapsack"));
    write_file(&p.mcp_config, &mcp_with_server("/bin/knapsack"));
    write_file(
        &p.metrics,
        concat!(
            // older session: had a failure
            r#"{"t":100,"event":"compress","session":"old","raw":1000,"shown":100,"saved":900,"delta_hits":0,"evicted":0}"#,
            "\n",
            r#"{"t":150,"event":"expand","session":"old","tokens":0,"ok":false}"#,
            "\n",
            // current session: clean
            r#"{"t":500,"event":"compress","session":"current","raw":2000,"shown":200,"saved":1800,"delta_hits":1,"evicted":0}"#,
            "\n",
            r#"{"t":600,"event":"expand","session":"current","tokens":50,"ok":true}"#,
            "\n",
        ),
    );

    let s = collect_from(&p);
    let out = render(&s);

    assert!(out.contains("Recall:           healthy"), "latest session has no failures -> healthy:\n{}", out);
    assert!(out.contains("Lifetime:"), "multi-session -> lifetime footer appears:\n{}", out);
    assert!(out.contains("2 sessions"), "lifetime footer counts sessions:\n{}", out);
    // Lifetime reports GROSS saved (900 + 1800 = 2,700), not net — see comment in
    // status.rs. This prevents "lifetime less than current session" confusion when
    // the MCP session has expands but no compresses.
    assert!(out.contains("2,700 tokens saved"), "lifetime saved is gross, not net:\n{}", out);
    // Refetch line — only when refetched > 0. Here: 50 tokens refetched in "current".
    assert!(out.contains("50 refetched on recall"), "lifetime surfaces refetch cost:\n{}", out);
    assert!(out.contains("1 recall failures total"), "lifetime footer surfaces historical failure:\n{}", out);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn lifetime_never_reads_less_than_a_clean_current_session() {
    // The bug this regression test pins: when one session has only compress events
    // (positive net) and a SEPARATE session has only expand events (negative net),
    // the old footer summed nets across sessions and could show "Lifetime" smaller
    // than "Session saved". A caveman user reads that as "did I lose tokens?".
    let _env = EnvGuard::with(None);
    let dir = tmpdir("lifetime-honest");
    let p = paths(&dir);
    write_file(&p.settings, &settings_with_hook("/bin/knapsack"));
    write_file(&p.mcp_config, &mcp_with_server("/bin/knapsack"));
    write_file(
        &p.metrics,
        concat!(
            // The "real" session under the Claude Code session id — pure savings.
            r#"{"t":100,"event":"compress","session":"cc-123","raw":10000,"shown":2000,"saved":8000,"delta_hits":4,"evicted":0}"#,
            "\n",
            // The "mcp" session, which only carries expand events (model recalls).
            // Under the OLD net-based footer this would give: lifetime = 8000 - 3000 = 5000
            // while "Session saved" for cc-123 reads 8000. User sees lifetime < session.
            r#"{"t":200,"event":"expand","session":"mcp","tokens":3000,"ok":true}"#,
            "\n",
        ),
    );
    let s = collect_from(&p);
    let out = render(&s);
    // The current session ("cc-123") read shows GROSS 8000 (no expands in it).
    assert!(out.contains("Session saved:    8,000 tokens"), "current session gross is 8,000:\n{}", out);
    // The lifetime line MUST NOT report a number less than 8,000 — that would confuse
    // the user. It reports gross (still 8,000) with refetch surfaced separately.
    assert!(out.contains("Lifetime: 8,000 tokens saved"), "lifetime gross matches what the user saw:\n{}", out);
    assert!(out.contains("3,000 refetched on recall"), "lifetime refetch cost is shown separately:\n{}", out);
    // And critically: no number < 8000 anywhere on the lifetime line.
    let life_line = out.lines().find(|l| l.starts_with("Lifetime:")).expect("lifetime line present");
    assert!(!life_line.contains("5,000"), "lifetime must not surface the misleading net (5,000):\n{}", life_line);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn session_over_recall_shows_gross_saved_plus_refetched_not_a_negative_net() {
    // Regression: a session that compresses 8000 tokens and then over-recalls (model
    // recalls the same handle 5 times, paying back 5*X tokens) used to display
    // "Session saved: -<big number>" because the line showed `net = saved -
    // refetched`. The fix splits the display: "Session saved: 8,000 tokens" (gross)
    // + "Refetched: <big> tokens" + "Net reduction: <maybe-negative>%". The caveman
    // user always sees a non-negative "saved" number; the recall cost is its own
    // line; the percent is the rolled-up summary.
    let _env = EnvGuard::with(None);
    let dir = tmpdir("over-recall");
    let p = paths(&dir);
    write_file(&p.settings, &settings_with_hook("/bin/knapsack"));
    write_file(&p.mcp_config, &mcp_with_server("/bin/knapsack"));
    write_file(
        &p.metrics,
        concat!(
            r#"{"t":100,"event":"compress","session":"sess","raw":10000,"shown":2000,"saved":8000,"delta_hits":0,"evicted":0}"#,
            "\n",
            // The model over-recalls — three big expands of the same content.
            r#"{"t":200,"event":"expand","session":"sess","tokens":9000,"ok":true}"#,
            "\n",
            r#"{"t":300,"event":"expand","session":"sess","tokens":9000,"ok":true}"#,
            "\n",
            r#"{"t":400,"event":"expand","session":"sess","tokens":9000,"ok":true}"#,
            "\n",
        ),
    );
    let s = collect_from(&p);
    let out = render(&s);
    // Saved stays at the GROSS compress benefit, regardless of recall cost.
    assert!(out.contains("Session saved:    8,000 tokens"), "saved is gross (always >= 0):\n{}", out);
    // Refetched line appears and shows the total recall cost (3 * 9000 = 27000).
    assert!(out.contains("Refetched:        27,000 tokens"), "refetched shown on its own line:\n{}", out);
    // Net reduction reflects the actual math: net = 8000 - 27000 = -19000; -19000/10000 = -190%.
    // The display can be negative HERE (it's the percentage), but the saved/refetched
    // numbers are each clear and positive. No "-X tokens" alarms the user.
    let saved_line = out.lines().find(|l| l.starts_with("Session saved:")).expect("saved line");
    assert!(
        !saved_line.contains('-'),
        "Session saved line must NEVER show a negative number — refetch goes on its own line: {saved_line}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn malformed_metrics_lines_are_skipped_not_fatal() {
    // The metrics file is JSONL written by independent processes; one torn write or a
    // forward-compat field shouldn't crash the user-facing summary.
    let _env = EnvGuard::with(None);
    let dir = tmpdir("malformed");
    let p = paths(&dir);
    write_file(&p.settings, &settings_with_hook("/bin/knapsack"));
    write_file(&p.mcp_config, &mcp_with_server("/bin/knapsack"));
    write_file(
        &p.metrics,
        concat!(
            "this is not json\n",
            r#"{"t":100,"event":"compress","session":"sess","raw":3000,"shown":600,"saved":2400,"delta_hits":0,"evicted":0}"#,
            "\n",
            "{ unterminated\n",
            r#"{"t":200,"event":"expand","session":"sess","tokens":100,"ok":true}"#,
            "\n",
        ),
    );

    let s = collect_from(&p);
    let out = render(&s);

    // valid lines: saved=2400 (gross), refetched=100; net = 2300; reduction = 76%
    assert!(out.contains("Session saved:    2,400 tokens"), "saved is gross (2400 from the one valid compress):\n{}", out);
    assert!(out.contains("Refetched:        100 tokens"), "refetch from the one valid expand line:\n{}", out);
    assert!(out.contains("Net reduction:    76%"), "percent computed from valid lines:\n{}", out);
    let _ = std::fs::remove_dir_all(&dir);
}
