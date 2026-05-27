//! `/knapsack` (alias `knapsack status`) is the product-facing summary. Tests pin the
//! contract that matters: the headline shape, the Input/Output reduction lines, the
//! savings/recall/store block, and the inactive→install hint. No env-var jargon, no
//! EXPERIMENTAL/dogfood mention, no slash-command laundry list (that's all `doctor`).
//!
//! Paths are injected via the `Paths` struct so tests never touch the user's real
//! ~/.claude or ~/.knapsack (same pattern the installer uses for KNAPSACK_SETTINGS etc).

use knapsack::status::{collect_from, render, render_verbose, Paths};
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
fn no_activity_says_ready_with_no_savings_yet() {
    // The "fresh install, hook wired, nothing has happened yet" surface. The header
    // signals "Knapsack is ready" (not "active", which would imply work product) and
    // the savings block reads as the desired spec: a one-liner that the user can
    // parse without thinking. Store + Lifetime are verbose-only now.
    let _env = EnvGuard::with(None); // default-on
    let dir = tmpdir("idle");
    let p = paths(&dir);
    write_file(&p.settings, &settings_with_hook("/bin/knapsack"));
    write_file(&p.mcp_config, &mcp_with_server("/bin/knapsack"));

    let s = collect_from(&p);
    let out = render(&s);

    assert!(out.starts_with("Knapsack is ready\n"), "header reflects on-disk wiring + no activity:\n{}", out);
    assert!(out.contains("No savings yet."), "explicit 'No savings yet.' line on idle state:\n{}", out);
    assert!(out.contains("Input reduction:    active"), "input line on, given default read-hook gate:\n{}", out);
    assert!(out.contains("Output reduction:   active"), "output line on, given hook+MCP wired:\n{}", out);
    assert!(out.contains("Recall:             idle"), "no activity -> recall is idle, not 'healthy':\n{}", out);
    assert!(!out.contains("Tip:"), "idle state -> no tip; no behavior to advise on:\n{}", out);
    // Verbose-only detail must NOT appear on default surface.
    assert!(!out.contains("Store:"), "default surface omits Store line:\n{}", out);
    assert!(!out.contains("Lifetime:"), "default surface omits Lifetime footer:\n{}", out);
    // None of the cruft from the old surface should appear here.
    assert!(!out.contains("EXPERIMENTAL"), "no EXPERIMENTAL label:\n{}", out);
    assert!(!out.contains("dogfood"), "no dogfood mention:\n{}", out);
    assert!(!out.contains("KNAPSACK_READ_HOOK"), "no env-var hint:\n{}", out);
    assert!(!out.contains("/knapsack pack"), "no actions block:\n{}", out);
    assert!(!out.contains("/knapsack doctor"), "no actions block:\n{}", out);
    assert!(!out.contains("Modes"), "no Modes block:\n{}", out);
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

    // Hook wired but the off-switch is set AND no activity -> header is "ready",
    // not a flavor of "active". The Input line should still report the gate state.
    assert!(out.contains("Knapsack is ready"), "no activity -> ready header even with input off:\n{}", out);
    assert!(out.contains("Input reduction:    off"), "explicit `KNAPSACK_READ_HOOK=0` -> input reads off:\n{}", out);
    assert!(out.contains("Output reduction:   active"), "output is unaffected by the input off-switch:\n{}", out);
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

    // Hook installed but no activity -> the header is "ready". The Output line still
    // truthfully reports MCP as off so the user can wire it.
    assert!(out.contains("Knapsack is ready"), "hook installed, no activity -> ready:\n{}", out);
    assert!(out.contains("Output reduction:   off"), "MCP missing -> output reads off:\n{}", out);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn healthy_session_shows_saved_and_reduction_percent() {
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

    // Compress events: raw=10000, saved=8000. Expand: 600 tokens refetched (net positive).
    // Default surface: header is "saving context", "Saved this session" is the gross
    // compress benefit, "Reduction" is the rolled-up percent (net/raw = (8000-600)/10000 = 74%).
    // Refetched is hidden on the default surface for net-positive sessions — the
    // percentage already accounts for it, so a separate line would just be noise.
    assert!(out.starts_with("Knapsack is saving context\n"), "net-positive -> 'saving context' header:\n{}", out);
    assert!(out.contains("Saved this session: 8,000 tokens"), "shows gross saved (not net):\n{}", out);
    assert!(out.contains("Reduction:          74%"), "shows net/raw as integer percent:\n{}", out);
    assert!(!out.contains("Refetched:"), "net-positive default surface hides Refetched (noise):\n{}", out);
    assert!(!out.contains("Tip:"), "positive session -> no advice tip (nothing to address):\n{}", out);
    assert!(out.contains("Recall:             healthy"), "no failed expands -> healthy:\n{}", out);
    // Verbose-only detail must NOT appear on default.
    assert!(!out.contains("Store:"), "default surface omits Store line:\n{}", out);
    assert!(!out.contains("Lifetime:"), "single session -> no lifetime footer:\n{}", out);

    // The same Status under --verbose surfaces Refetched AND the Store line.
    let v = render_verbose(&s);
    assert!(v.contains("Saved this session: 8,000 tokens"), "verbose still shows gross saved:\n{}", v);
    assert!(v.contains("Refetched:          600 tokens"), "verbose surfaces refetch cost:\n{}", v);
    assert!(v.contains("Reduction:          74%"), "verbose keeps the percent:\n{}", v);
    assert!(v.contains("Store:              2 blocks"), "verbose surfaces store block count:\n{}", v);
    assert!(v.contains("3 KB"), "store bytes rendered in KB:\n{}", v);

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
    assert!(!out.contains("Recall:             healthy"), "must NOT also claim healthy:\n{}", out);
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

    assert!(out.contains("Recall:             healthy"), "latest session has no failures -> healthy:\n{}", out);
    // The default surface no longer shows lifetime — that lives in `--verbose` and in
    // `knapsack metrics`. The latest-session recall health is what the user sees on
    // the headline, so old-session failures don't shout forever. We assert the absence
    // on default, then assert presence under verbose.
    assert!(!out.contains("Lifetime:"), "default surface omits lifetime footer:\n{}", out);
    assert!(!out.contains("recall failures total"), "default omits historical failure tally:\n{}", out);

    let v = render_verbose(&s);
    assert!(v.contains("Lifetime:"), "verbose -> lifetime footer appears:\n{}", v);
    assert!(v.contains("2 sessions"), "verbose lifetime footer counts sessions:\n{}", v);
    // Lifetime reports GROSS saved (900 + 1800 = 2,700), not net — see comment in
    // status.rs. This prevents "lifetime less than current session" confusion when
    // the MCP session has expands but no compresses.
    assert!(v.contains("2,700 tokens saved"), "verbose lifetime saved is gross, not net:\n{}", v);
    assert!(v.contains("50 refetched on recall"), "verbose lifetime surfaces refetch cost:\n{}", v);
    assert!(v.contains("1 recall failures total"), "verbose lifetime surfaces historical failure:\n{}", v);
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
    // Default surface: current session reads GROSS 8000 (no expands attributed to it).
    // The lifetime line is now hidden on default — so the misleading "less than session"
    // framing literally cannot appear on the headline. Refetched is also hidden on a
    // net-positive default surface (8,000 with no recall against this session is +8,000).
    assert!(out.contains("Saved this session: 8,000 tokens"), "current session gross is 8,000:\n{}", out);
    assert!(!out.contains("Lifetime:"), "default surface omits lifetime footer:\n{}", out);

    // Verbose surface: lifetime is back, still gross, still safe ("Lifetime ... 8,000").
    let v = render_verbose(&s);
    assert!(v.contains("Lifetime: 8,000 tokens saved"), "verbose lifetime gross matches session:\n{}", v);
    assert!(v.contains("3,000 refetched on recall"), "verbose lifetime refetch cost shown separately:\n{}", v);
    let life_line = v.lines().find(|l| l.starts_with("Lifetime:")).expect("lifetime line present");
    assert!(!life_line.contains("5,000"), "verbose lifetime must not surface the misleading net (5,000):\n{}", life_line);
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
    // Net-NEGATIVE current session: header is the neutral "Knapsack is active" — not
    // "saving context" (which would be a lie) and not "ready" (which would be wrong
    // when there's work product). The Refetched line surfaces on the default surface
    // for net-negative sessions exactly so the user understands why Reduction is negative.
    assert!(out.starts_with("Knapsack is active\n"), "net-negative -> neutral 'active' header (never 'saving context'):\n{}", out);
    // Saved stays at the GROSS compress benefit, regardless of recall cost.
    assert!(out.contains("Saved this session: 8,000 tokens"), "saved is gross (always >= 0):\n{}", out);
    // Refetched line appears and shows the total recall cost (3 * 9000 = 27000).
    assert!(out.contains("Refetched:          27,000 tokens"), "net-negative -> Refetched IS shown on default:\n{}", out);
    // Reduction is negative: net = 8000 - 27000 = -19000; -19000/10000 = -190%. The
    // percentage can be negative; the saved/refetched numbers are each positive on
    // their own line, so the headline reads honestly without alarming via a "-X tokens"
    // figure under "Saved".
    assert!(out.contains("Reduction:          -190%"), "reduction reflects honest net math:\n{}", out);
    let saved_line = out.lines().find(|l| l.starts_with("Saved this session:")).expect("saved line");
    assert!(
        !saved_line.contains('-'),
        "Saved this session line must NEVER show a negative number — refetch goes on its own line: {saved_line}"
    );

    // When the session paid more in recall than it saved, the user gets actionable
    // advice. The tip points at narrower recall (the typical fix). Pinned because
    // the trigger is behavior-specific — present here, absent on positive sessions
    // and absent on idle sessions; see other tests in this file for the negatives.
    assert!(
        out.contains("Tip: recall smaller sections instead of expanding whole files."),
        "net-negative -> tip is shown:\n{}",
        out
    );

    // Verbose surface includes the same tip — it's user advice, equally useful with
    // or without the detail block. Spec example for verbose only shows the positive
    // case (no tip needed); behavior here is "tip when the trigger fires, regardless".
    let v = render_verbose(&s);
    assert!(
        v.contains("Tip: recall smaller sections instead of expanding whole files."),
        "verbose also surfaces the tip when net-negative:\n{}",
        v
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

    // valid lines: saved=2400 (gross), refetched=100; net = 2300; reduction = 76%.
    // Net-positive (2400 - 100 = 2300 > 0) -> "saving context" header, Refetched hidden
    // on default (the percentage already includes its cost). The contract that survives
    // here is the resilience: malformed JSONL lines don't crash or skew the math.
    assert!(out.starts_with("Knapsack is saving context\n"), "net-positive -> 'saving context' header:\n{}", out);
    assert!(out.contains("Saved this session: 2,400 tokens"), "saved is gross (2400 from the one valid compress):\n{}", out);
    assert!(out.contains("Reduction:          76%"), "percent computed from valid lines:\n{}", out);
    assert!(!out.contains("Refetched:"), "net-positive default surface hides Refetched line:\n{}", out);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn current_positive_run_does_not_get_buried_by_historical_negative_lifetime() {
    // The user-reported case: a fresh React run compresses 6,689 tokens to 1,012 shown
    // (saved 5,677, 85% reduction). The wider metrics file holds historical recall debt
    // from prior sessions — totals reaching -6.9M net. The default `knapsack status`
    // must lead with the CURRENT session's positive savings; nothing about the historical
    // lifetime should appear on the headline, and Refetched must NOT show against this
    // session (the model didn't recall anything during this run).
    let _env = EnvGuard::with(None);
    let dir = tmpdir("current-vs-history");
    let p = paths(&dir);
    write_file(&p.settings, &settings_with_hook("/bin/knapsack"));
    write_file(&p.mcp_config, &mcp_with_server("/bin/knapsack"));
    write_file(
        &p.metrics,
        concat!(
            // Old session: a few compresses with a small contribution.
            r#"{"t":100,"event":"compress","session":"old-1","raw":1012,"shown":150,"saved":862,"delta_hits":0,"evicted":0}"#, "\n",
            // The MCP recall debt — large, tagged against "old-1" via meta-attribution.
            r#"{"t":150,"event":"expand","session":"old-1","tokens":6952811,"ok":true}"#, "\n",
            // CURRENT session: 13 compress events summing to the user's numbers.
            r#"{"t":2000,"event":"compress","session":"react-fresh","raw":6689,"shown":1012,"saved":5677,"delta_hits":34,"evicted":0}"#, "\n",
        ),
    );

    let s = collect_from(&p);
    let out = render(&s);

    // Headline reflects the CURRENT session's net-positive state.
    assert!(out.starts_with("Knapsack is saving context\n"), "current session is +5,677 -> 'saving context':\n{}", out);
    assert!(out.contains("Saved this session: 5,677 tokens"), "current session's gross savings lead:\n{}", out);
    assert!(out.contains("Reduction:          84%"), "reduction = net/raw = 5677/6689 = 84%:\n{}", out);

    // None of the historical recall debt is allowed to dominate the default surface.
    // No literal of the 6.95M tokens, no "Lifetime" footer, no NET-negative scarecrow.
    assert!(!out.contains("Lifetime:"), "default surface omits lifetime footer:\n{}", out);
    assert!(!out.contains("6,952,811"), "historical refetched total must not appear on default:\n{}", out);
    assert!(!out.contains("-6,947,134"), "historical NET-negative must not appear on default:\n{}", out);
    assert!(!out.contains("Refetched:"), "this session had 0 refetches; line must NOT appear:\n{}", out);

    // Verbose still has the truth — the lifetime debt is not erased, just relocated.
    let v = render_verbose(&s);
    assert!(v.contains("Lifetime:"), "verbose surfaces the lifetime footer:\n{}", v);
    assert!(v.contains("6,539 tokens saved"), "verbose lifetime gross is 862 + 5677 = 6,539:\n{}", v);
    assert!(v.contains("6,952,811 refetched on recall"), "verbose surfaces the recall debt verbatim:\n{}", v);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn current_session_truly_net_negative_matches_spec_example() {
    // Pins the exact spec shape for the net-negative case:
    //   Knapsack is active
    //   Saved this session: 5,677 tokens
    //   Refetched:          8,200 tokens
    //   Reduction:          -31%
    //   Input reduction:    active
    //   Output reduction:   active
    //   Recall:             healthy
    //   Tip: recall smaller sections instead of expanding whole files.
    // Reduction = (5677-8200)/raw*100 = -2523/raw*100. Pick raw=8138 so -31% lands.
    let _env = EnvGuard::with(None);
    let dir = tmpdir("spec-neg");
    let p = paths(&dir);
    write_file(&p.settings, &settings_with_hook("/bin/knapsack"));
    write_file(&p.mcp_config, &mcp_with_server("/bin/knapsack"));
    write_file(
        &p.metrics,
        concat!(
            r#"{"t":100,"event":"compress","session":"current","raw":8138,"shown":2461,"saved":5677,"delta_hits":0,"evicted":0}"#, "\n",
            r#"{"t":200,"event":"expand","session":"current","tokens":8200,"ok":true}"#, "\n",
        ),
    );

    let s = collect_from(&p);
    let out = render(&s);

    // Lines in spec order.
    assert!(out.starts_with("Knapsack is active\n"), "net-negative header is neutral 'active':\n{}", out);
    assert!(out.contains("Saved this session: 5,677 tokens"), "saved is gross, comma-formatted:\n{}", out);
    assert!(out.contains("Refetched:          8,200 tokens"), "refetched line surfaces on net-negative default:\n{}", out);
    assert!(out.contains("Reduction:          -31%"), "reduction can go negative honestly:\n{}", out);
    assert!(out.contains("Input reduction:    active"), "input line preserved:\n{}", out);
    assert!(out.contains("Output reduction:   active"), "output line preserved:\n{}", out);
    assert!(out.contains("Recall:             healthy"), "recall line preserved:\n{}", out);
    assert!(out.contains("Tip: recall smaller sections instead of expanding whole files."), "tip appears on net-negative:\n{}", out);

    // The negative is shown without faking positivity AND without dumping detail.
    assert!(!out.contains("Knapsack is saving context"), "must not pretend savings are positive:\n{}", out);
    assert!(!out.contains("Lifetime:"), "default surface omits lifetime footer:\n{}", out);
    assert!(!out.contains("Store:"), "default surface omits store line:\n{}", out);
    assert!(!out.contains("raw tokens"), "default surface omits raw/shown breakdown:\n{}", out);

    let _ = std::fs::remove_dir_all(&dir);
}
