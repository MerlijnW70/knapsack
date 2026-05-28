//! Packaging / lifecycle: `install --apply`, `doctor`, `uninstall [--purge]`.
//!
//! Config is PATCHED, never clobbered: existing settings.json / mcp config are parsed,
//! our entries merged in, and a timestamped backup written first. Idempotent (re-running
//! changes nothing). If a file can't be parsed we leave it untouched and tell the user to
//! edit it manually — never corrupt a user's config. Paths are env-overridable so the
//! installer (and its tests) never have to touch the real ~/.claude during verification:
//!   KNAPSACK_SETTINGS     (default ~/.claude/settings.json)  — hook lives here
//!   KNAPSACK_MCP_CONFIG   (default ~/.claude.json)           — mcpServers live here

use crate::config;
use crate::json::{self, Json};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub fn settings_path() -> PathBuf {
    std::env::var_os("KNAPSACK_SETTINGS")
        .map(PathBuf::from)
        .unwrap_or_else(|| config::home().join(".claude").join("settings.json"))
}
pub fn mcp_config_path() -> PathBuf {
    std::env::var_os("KNAPSACK_MCP_CONFIG")
        .map(PathBuf::from)
        .unwrap_or_else(|| config::home().join(".claude.json"))
}
pub fn bin_path() -> String {
    std::env::current_exe()
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .unwrap_or_else(|_| "knapsack".into())
}

// ---------- small JSON object helpers (Obj is an ordered Vec) ----------
fn ensure_obj(j: &mut Json) {
    if !matches!(j, Json::Obj(_)) {
        *j = Json::Obj(Vec::new());
    }
}
fn entry<'a>(j: &'a mut Json, key: &str, default: Json) -> &'a mut Json {
    ensure_obj(j);
    if let Json::Obj(o) = j {
        let pos = o.iter().position(|(k, _)| k == key);
        match pos {
            Some(p) => &mut o[p].1,
            None => {
                o.push((key.to_string(), default));
                let l = o.len() - 1;
                &mut o[l].1
            }
        }
    } else {
        unreachable!()
    }
}
fn get_mut<'a>(j: &'a mut Json, key: &str) -> Option<&'a mut Json> {
    if let Json::Obj(o) = j {
        let pos = o.iter().position(|(k, _)| k == key)?;
        Some(&mut o[pos].1)
    } else {
        None
    }
}

// ---------- hook entry (settings.json: hooks.PreToolUse[]) ----------
fn cmd_is_knapsack(cmd: &str) -> bool {
    cmd.contains("knapsack") && cmd.contains("hook")
}
fn entry_is_knapsack(e: &Json) -> bool {
    if let Some(Json::Arr(hs)) = e.get("hooks").cloned() {
        return hs.iter().any(|h| {
            h.get("command")
                .and_then(|c| c.as_str())
                .map(cmd_is_knapsack)
                .unwrap_or(false)
        });
    }
    false
}
/// The one command string we own: `"<bin>" hook`. Convergence target for apply/repair.
fn canonical_cmd(bin: &str) -> String {
    format!("\"{}\" hook", bin)
}
fn hook_entry(bin: &str) -> Json {
    // Subscribe to BOTH Bash (output reduction) AND Read (input reduction).
    // Claude Code's matcher is regex-alternation; "Bash|Read" routes both
    // tool kinds into a single `knapsack hook` invocation, where the binary's
    // dispatch on tool_name (hook.rs:272) sends Bash through the wrap path
    // and Read through read_hook::run. Pre-fix the matcher was "Bash" only,
    // which left Read-tool input reduction silently dormant after install —
    // the README contract ("input reduction is on by default") wasn't met
    // because Claude Code never delivered Read events to the hook.
    Json::Obj(vec![
        ("matcher".into(), Json::Str("Bash|Read".into())),
        (
            "hooks".into(),
            Json::Arr(vec![Json::Obj(vec![
                ("type".into(), Json::Str("command".into())),
                ("command".into(), Json::Str(canonical_cmd(bin))),
            ])]),
        ),
    ])
}
fn root_has_hook(root: &Json) -> bool {
    if let Some(Json::Arr(a)) = root.get("hooks").and_then(|h| h.get("PreToolUse")).cloned() {
        return a.iter().any(entry_is_knapsack);
    }
    false
}
/// The canonical matcher: subscribes to both Bash (output reduction) and Read
/// (input reduction). See `hook_entry` for the full rationale.
const CANONICAL_MATCHER: &str = "Bash|Read";

/// Converge the knapsack PreToolUse hook to the canonical command AND matcher,
/// not just "present or not". Predicate = semantic ownership (cmd_is_knapsack)
/// AND exact desired target: a knapsack hook pointing at a *stale* binary path
/// is rewritten in place; a knapsack hook with a *stale matcher* (pre-fix the
/// default was just "Bash", which left Read events unsubscribed) is also
/// rewritten; an already-canonical entry is left untouched (NoChange). This is
/// what makes a re-point/repair actually fix drift instead of seeing "a knapsack
/// hook exists" and doing nothing.
fn apply_hook(root: &mut Json, bin: &str) -> bool {
    let want_cmd = canonical_cmd(bin);
    let hooks = entry(root, "hooks", Json::Obj(vec![]));
    let pre = entry(hooks, "PreToolUse", Json::Arr(vec![]));
    if !matches!(pre, Json::Arr(_)) {
        *pre = Json::Arr(vec![]);
    }
    let mut found = false;
    let mut changed = false;
    if let Json::Arr(entries) = pre {
        for e in entries.iter_mut() {
            // Is this entry's hooks-array one of ours? (i.e. command contains "knapsack hook")
            let is_ours = matches!(e.get("hooks"), Some(Json::Arr(hs)) if hs.iter().any(|h| {
                h.get("command").and_then(|c| c.as_str()).map(cmd_is_knapsack).unwrap_or(false)
            }));
            if !is_ours {
                continue;
            }
            found = true;
            // Repair matcher: if it isn't already the canonical alternation,
            // rewrite. This catches the pre-fix "Bash"-only default.
            if let Json::Obj(o) = e {
                if let Some(p) = o.iter().position(|(k, _)| k == "matcher") {
                    if o[p].1 != Json::Str(CANONICAL_MATCHER.into()) {
                        o[p].1 = Json::Str(CANONICAL_MATCHER.into());
                        changed = true;
                    }
                } else {
                    o.push(("matcher".into(), Json::Str(CANONICAL_MATCHER.into())));
                    changed = true;
                }
            }
            // Repair command: rewrite stale binary paths in place.
            if let Some(Json::Arr(hs)) = get_mut(e, "hooks") {
                for h in hs.iter_mut() {
                    let is_h_ours = h
                        .get("command")
                        .and_then(|c| c.as_str())
                        .map(cmd_is_knapsack)
                        .unwrap_or(false);
                    if !is_h_ours {
                        continue;
                    }
                    if let Json::Obj(o) = h {
                        if let Some(p) = o.iter().position(|(k, _)| k == "command") {
                            if o[p].1 != Json::Str(want_cmd.clone()) {
                                o[p].1 = Json::Str(want_cmd.clone());
                                changed = true;
                            }
                        }
                    }
                }
            }
        }
        if !found {
            entries.push(hook_entry(bin));
            changed = true;
        }
    }
    changed
}
fn remove_hook(root: &mut Json) -> bool {
    let mut changed = false;
    if let Some(hooks) = get_mut(root, "hooks") {
        if let Some(Json::Arr(a)) = get_mut(hooks, "PreToolUse") {
            let before = a.len();
            a.retain(|e| !entry_is_knapsack(e));
            changed = a.len() != before;
        }
        // After removing our entry, prune any empty scaffolding we may have created:
        // empty PreToolUse array → drop the key, empty hooks object → drop the key.
        // Without this, a clean `install` → `uninstall` cycle leaves
        // `{"hooks":{"PreToolUse":[]}}` lying in the file instead of restoring the
        // pre-install shape. Only prunes EMPTY containers — anything else (e.g.
        // unrelated Edit hooks) is preserved verbatim.
        if changed {
            prune_empty_array(hooks, "PreToolUse");
            prune_empty_object(root, "hooks");
        }
    }
    changed
}

// ---------- mcp entry (mcpServers.knapsack) ----------
fn mcp_desired(bin: &str) -> Json {
    Json::Obj(vec![
        ("command".into(), Json::Str(bin.into())),
        ("args".into(), Json::Arr(vec![Json::Str("mcp".into())])),
    ])
}
fn root_has_mcp(root: &Json) -> bool {
    root.get("mcpServers")
        .and_then(|s| s.get("knapsack"))
        .is_some()
}
fn apply_mcp(root: &mut Json, bin: &str) -> bool {
    let servers = entry(root, "mcpServers", Json::Obj(vec![]));
    ensure_obj(servers);
    let desired = mcp_desired(bin);
    if let Json::Obj(o) = servers {
        let pos = o.iter().position(|(k, _)| k == "knapsack");
        match pos {
            Some(p) => {
                if o[p].1 == desired {
                    false
                } else {
                    o[p].1 = desired;
                    true
                }
            }
            None => {
                o.push(("knapsack".into(), desired));
                true
            }
        }
    } else {
        false
    }
}
fn remove_mcp(root: &mut Json) -> bool {
    let mut changed = false;
    if let Some(Json::Obj(o)) = get_mut(root, "mcpServers") {
        let before = o.len();
        o.retain(|(k, _)| k != "knapsack");
        changed = o.len() != before;
    }
    // Drop the empty `mcpServers: {}` scaffold so a clean install/uninstall round-trip
    // leaves the file as close to its pre-install state as we can manage. Only prunes
    // when the object is fully empty — preserving any unrelated MCP servers the user
    // installed alongside knapsack.
    if changed {
        prune_empty_object(root, "mcpServers");
    }
    changed
}

/// Remove an object-valued key from `parent` iff its value is an empty object `{}`.
/// No-op for missing keys, non-object values, or non-empty objects.
fn prune_empty_object(parent: &mut Json, key: &str) {
    let Some(Json::Obj(o)) = get_mut(parent, key) else {
        return;
    };
    if !o.is_empty() {
        return;
    }
    if let Json::Obj(parent_obj) = parent {
        parent_obj.retain(|(k, _)| k != key);
    }
}

/// Remove an array-valued key from `parent` iff its value is an empty array `[]`.
fn prune_empty_array(parent: &mut Json, key: &str) {
    let Some(Json::Arr(a)) = get_mut(parent, key) else {
        return;
    };
    if !a.is_empty() {
        return;
    }
    if let Json::Obj(parent_obj) = parent {
        parent_obj.retain(|(k, _)| k != key);
    }
}

// ---------- file patching with backup ----------
fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Find a non-colliding backup filename and copy the file there. Returns the chosen
/// path on success.
///
/// Why this loops: `now_secs()` is 1-second resolution and a user (or test) running
/// `install` → `uninstall` → `install` in fast succession would otherwise land all
/// backups on the same filename — every later one CLOBBERING the earlier. A user
/// who hits a bad config and retries would silently lose the only rollback target
/// for the original good config. The loop walks `_2`, `_3`, … until it finds a
/// free name. Bounded by `MAX` so a pathological filesystem can't spin forever;
/// at 1000 retries we give up and return None (caller surfaces "no backup" — same
/// shape as a non-existent source file).
fn backup(path: &Path) -> Option<PathBuf> {
    if !path.exists() {
        return None;
    }
    const MAX: u32 = 1000;
    let secs = now_secs();
    for n in 0..MAX {
        let candidate = if n == 0 {
            PathBuf::from(format!("{}.knapsack-bak-{}", path.display(), secs))
        } else {
            PathBuf::from(format!(
                "{}.knapsack-bak-{}_{}",
                path.display(),
                secs,
                n + 1
            ))
        };
        if candidate.exists() {
            continue;
        }
        if fs::copy(path, &candidate).is_ok() {
            return Some(candidate);
        }
        return None;
    }
    None
}

/// Strip a UTF-8 BOM (U+FEFF) if present. Many real-world editors and shells write
/// JSON config files with a BOM — PowerShell 5.1's default `Set-Content -Encoding utf8`,
/// Notepad's UTF-8 save, some IDE auto-encoders — and our strict in-tree JSON parser
/// rejects the BOM with an opaque `unexpected Some('\u{feff}')` error. Stripping the
/// BOM here lets us patch those files in place without ever surfacing the technical
/// detail to a user who would have no idea what `\u{feff}` means. We normalize on
/// write (the patched file is serialized fresh as BOM-less UTF-8), which is the
/// modern de-facto standard that Claude Code and every other consumer of these files
/// already handles.
fn strip_bom(s: &str) -> &str {
    s.strip_prefix('\u{feff}').unwrap_or(s)
}

/// Render an engine-level parse / IO error as a sentence a non-technical user can act on.
/// The raw `json::parse` error names a character offset and an unexpected token; that's
/// useful when debugging but worse than useless to a Windows user opening a one-line
/// installer for the first time. Map the small set of failures we actually see in the
/// wild to actionable phrasing; pass anything else through verbatim so we don't lose
/// information the engineer would need.
fn humanize_patch_error(path: &Path, raw: &str) -> String {
    let p = path.display();
    if raw.contains("'\\u{feff}'") {
        return format!(
            "{p} starts with a UTF-8 BOM that knapsack couldn't strip. Open it in any editor, save as UTF-8 (without BOM), then re-run."
        );
    }
    if raw.contains("read ")
        && (raw.contains("Access is denied") || raw.contains("Permission denied"))
    {
        return format!(
            "{p} can't be read (permission denied). Close any program that has it open, or re-run the installer as the user who owns the file."
        );
    }
    if raw.contains("write ")
        && (raw.contains("Access is denied") || raw.contains("Permission denied"))
    {
        return format!(
            "{p} can't be written (permission denied — file may be read-only or in use). Clear the read-only attribute or close Claude Code, then re-run."
        );
    }
    if raw.contains("could not parse") {
        // The parser itself appends the path; keep the underlying message but tag the
        // most likely cause for users who copy-pasted from a JSON-with-comments source.
        return format!(
            "{raw}\n     Common causes: trailing commas, // comments, or UTF-16 encoding — knapsack needs strict JSON."
        );
    }
    raw.to_string()
}

pub enum Patch {
    NoChange,
    Changed(Option<PathBuf>), // backup path, if the file pre-existed
}

fn patch_file<F: FnOnce(&mut Json) -> bool>(path: &Path, f: F) -> Result<Patch, String> {
    let existed = path.exists();
    let mut root = if existed {
        let txt = fs::read_to_string(path)
            .map_err(|e| humanize_patch_error(path, &format!("read {}: {}", path.display(), e)))?;
        // Strip a leading BOM before parsing. The patched file is then serialized
        // BOM-less, normalising to the modern UTF-8 convention. See `strip_bom`.
        let txt = strip_bom(&txt);
        if txt.trim().is_empty() {
            Json::Obj(vec![])
        } else {
            json::parse(txt).map_err(|e| {
                humanize_patch_error(
                    path,
                    &format!(
                        "could not parse {} ({}). Left unchanged — add the entry manually.",
                        path.display(),
                        e
                    ),
                )
            })?
        }
    } else {
        Json::Obj(vec![])
    };
    // Refuse to patch a file that is valid JSON but not an object: the merge helpers would
    // otherwise replace the whole root with `{}`, clobbering it. A parse error and a wrong
    // top-level shape are equally "can't safely patch" — leave it untouched, like unparseable.
    if !matches!(root, Json::Obj(_)) {
        return Err(format!(
            "{} is valid JSON but not an object; refusing to patch. Left unchanged — add the entry manually.",
            path.display()
        ));
    }
    if !f(&mut root) {
        return Ok(Patch::NoChange);
    }
    let bak = if existed { backup(path) } else { None };
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    fs::write(path, json::to_string(&root))
        .map_err(|e| humanize_patch_error(path, &format!("write {}: {}", path.display(), e)))?;
    Ok(Patch::Changed(bak))
}

// Public, testable wrappers --------------------------------------------------
pub fn patch_settings_file(path: &Path, bin: &str) -> Result<Patch, String> {
    patch_file(path, |r| apply_hook(r, bin))
}
pub fn patch_mcp_file(path: &Path, bin: &str) -> Result<Patch, String> {
    patch_file(path, |r| apply_mcp(r, bin))
}
pub fn unpatch_settings_file(path: &Path) -> Result<Patch, String> {
    patch_file(path, remove_hook)
}
pub fn unpatch_mcp_file(path: &Path) -> Result<Patch, String> {
    patch_file(path, remove_mcp)
}
pub fn settings_has_hook(path: &Path) -> bool {
    fs::read_to_string(path)
        .ok()
        .and_then(|t| json::parse(&t).ok())
        .map(|r| root_has_hook(&r))
        .unwrap_or(false)
}
pub fn mcp_has_server(path: &Path) -> bool {
    fs::read_to_string(path)
        .ok()
        .and_then(|t| json::parse(&t).ok())
        .map(|r| root_has_mcp(&r))
        .unwrap_or(false)
}

// ---------- provenance: which binary does each side actually point at? ----------
/// Pull the executable out of a hook command. Canonical form is `"<bin>" hook`; we also
/// tolerate a bare `bin hook`. Returns the path token, not the whole command line.
fn cmd_bin(cmd: &str) -> Option<String> {
    let c = cmd.trim();
    if let Some(rest) = c.strip_prefix('"') {
        return rest.split_once('"').map(|(b, _)| b.to_string());
    }
    c.split_whitespace().next().map(|s| s.to_string())
}
fn hook_cmd_in(root: &Json) -> Option<String> {
    if let Some(Json::Arr(a)) = root.get("hooks").and_then(|h| h.get("PreToolUse")) {
        for e in a {
            if let Some(Json::Arr(hs)) = e.get("hooks") {
                for h in hs {
                    if let Some(cmd) = h.get("command").and_then(|c| c.as_str()) {
                        if cmd_is_knapsack(cmd) {
                            return Some(cmd.to_string());
                        }
                    }
                }
            }
        }
    }
    None
}
/// The binary the PreToolUse knapsack hook would run, per settings.json.
pub fn hook_binary(path: &Path) -> Option<String> {
    let root = fs::read_to_string(path)
        .ok()
        .and_then(|t| json::parse(&t).ok())?;
    hook_cmd_in(&root).as_deref().and_then(cmd_bin)
}

/// What the settings.json knapsack PreToolUse entry's `matcher` field looks like.
/// `doctor` consumes this to flag the silently-dormant case: pre-fix installs left
/// `"Bash"` only, so check 4 ("hook installed") passed but Claude Code never routed
/// Read events to the hook — input reduction was dead until the user thought to
/// run `--repair`. Doctor surfacing the matcher value lets users discover the drift
/// without prior knowledge.
pub enum HookMatcher {
    /// No knapsack entry in PreToolUse at all — check 4 already surfaces this as Warn.
    NoEntry,
    /// Entry exists but has no `matcher` key (hand-edited, or hook malformed).
    Missing,
    /// Entry exists with this matcher value. Canonical is `Bash|Read`.
    Value(String),
}

/// Read the matcher field of the knapsack entry in `path`'s PreToolUse list.
/// Stops at the first knapsack-owned entry (the `apply_hook` path enforces at-most-one).
pub fn hook_matcher(path: &Path) -> HookMatcher {
    let root = match fs::read_to_string(path)
        .ok()
        .and_then(|t| json::parse(&t).ok())
    {
        Some(r) => r,
        None => return HookMatcher::NoEntry,
    };
    let arr = match root.get("hooks").and_then(|h| h.get("PreToolUse")) {
        Some(Json::Arr(a)) => a.clone(),
        _ => return HookMatcher::NoEntry,
    };
    for e in &arr {
        if !entry_is_knapsack(e) {
            continue;
        }
        return match e.get("matcher").and_then(|m| m.as_str()) {
            Some(s) => HookMatcher::Value(s.to_string()),
            None => HookMatcher::Missing,
        };
    }
    HookMatcher::NoEntry
}
/// The binary the knapsack MCP server would run, per the mcp config.
pub fn mcp_binary(path: &Path) -> Option<String> {
    let root = fs::read_to_string(path)
        .ok()
        .and_then(|t| json::parse(&t).ok())?;
    root.get("mcpServers")
        .and_then(|s| s.get("knapsack"))
        .and_then(|k| k.get("command"))
        .and_then(|c| c.as_str())
        .map(|s| s.to_string())
}

// ---------- smoke test (self-contained, temp store) ----------
pub fn smoke() -> Result<(), String> {
    use crate::content_type::ContentType;
    use crate::ledger::Ledger;
    use crate::pack::{pack, reconstruct};
    use crate::store::Store;

    let dir = std::env::temp_dir().join(format!(
        "knapsack-smoke-{}-{}",
        std::process::id(),
        now_secs()
    ));
    let store = Store::new(dir.clone());
    let mut ledger = Ledger::in_memory();
    let input = b"/** f */\nfunction f(x) {\n  const a = prepare(x);\n  let acc = 0;\n  for (const i of a) acc += i;\n  return finalize(acc);\n}\n";
    let r = pack(input, ContentType::Code, &store, &mut ledger, 0);
    if r.view.is_empty() {
        return Err("pack produced an empty view".into());
    }
    let back = reconstruct(input, ContentType::Code, &store).ok_or("reconstruct failed")?;
    let _ = fs::remove_dir_all(&dir);
    if back == input {
        Ok(())
    } else {
        Err("recall was not byte-exact".into())
    }
}

// ---------- doctor ----------
#[derive(PartialEq)]
pub enum Status {
    Ok,
    Warn,
    Fail,
}
pub struct Check {
    pub name: String,
    pub status: Status,
    pub detail: String,
}

fn writable(dir: &Path) -> bool {
    let _ = fs::create_dir_all(dir);
    let probe = dir.join(".knapsack-write-probe");
    let ok = fs::write(&probe, b"ok").is_ok();
    let _ = fs::remove_file(&probe);
    ok
}

pub fn run_checks() -> Vec<Check> {
    let mut c = Vec::new();
    let mk = |name: &str, status: Status, detail: String| Check {
        name: name.into(),
        status,
        detail,
    };

    // 1. binary found
    match std::env::current_exe() {
        Ok(p) => c.push(mk("binary found", Status::Ok, p.display().to_string())),
        Err(e) => c.push(mk("binary found", Status::Fail, e.to_string())),
    }
    // 2. store writable
    let sd = config::store_dir();
    c.push(if writable(&sd) {
        mk("store writable", Status::Ok, sd.display().to_string())
    } else {
        mk("store writable", Status::Fail, sd.display().to_string())
    });
    // 3. metrics writable
    let mp = config::metrics_path();
    let mdir = mp
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));
    c.push(if writable(&mdir) {
        mk("metrics writable", Status::Ok, mp.display().to_string())
    } else {
        mk("metrics writable", Status::Fail, mp.display().to_string())
    });
    // 4. hook installed
    let sp = settings_path();
    c.push(if settings_has_hook(&sp) {
        mk("hook installed", Status::Ok, sp.display().to_string())
    } else {
        mk(
            "hook installed",
            Status::Warn,
            format!("not in {} — run `knapsack install`", sp.display()),
        )
    });
    // 4b. hook matcher — presence alone (check 4) is not enough to honor the install
    // contract. Claude Code only routes a tool call to the hook when the matcher
    // matches the tool name; a `"Bash"`-only matcher (the pre-fix default) lets
    // output reduction work while leaving input reduction silently dormant. Fail
    // loud when the matcher is wrong so users notice without having to know to run
    // `--repair`. Skip when there's no entry at all — that's check 4's territory.
    match hook_matcher(&sp) {
        HookMatcher::NoEntry => {} // covered by check 4 as Warn
        HookMatcher::Value(ref v) if v == CANONICAL_MATCHER => {
            c.push(mk(
                "hook matcher",
                Status::Ok,
                format!("`{}` — output + input reduction", v),
            ));
        }
        HookMatcher::Value(v) => {
            c.push(mk(
                "hook matcher",
                Status::Fail,
                format!(
                    "matcher `{}` — expected `{}`; one of output/input reduction is dormant. Run `knapsack install --repair`",
                    v, CANONICAL_MATCHER
                ),
            ));
        }
        HookMatcher::Missing => {
            c.push(mk(
                "hook matcher",
                Status::Fail,
                "hook entry missing `matcher` field. Run `knapsack install --repair`".into(),
            ));
        }
    }
    // 5. mcp config present
    let mcp = mcp_config_path();
    c.push(if mcp_has_server(&mcp) {
        mk("MCP configured", Status::Ok, mcp.display().to_string())
    } else {
        mk(
            "MCP configured",
            Status::Warn,
            format!("not in {} — run `knapsack install`", mcp.display()),
        )
    });
    // 5b. binary provenance: what the hook and MCP are *configured* to launch (the path on
    // disk), plus the binary running THIS doctor. This is on-disk/config provenance, NOT a
    // claim about what the session's already-running hook/MCP processes loaded — those keep
    // their old binary until Claude Code restarts. Labels say "configured" so the report
    // can't be misread as runtime provenance. A 3-way split here is the "accidental install".
    let sha = |p: &str| crate::sha256::sha256_file(Path::new(p));
    let this_sha = std::env::current_exe()
        .ok()
        .and_then(|p| crate::sha256::sha256_file(&p));
    let hook_bin = hook_binary(&sp);
    let mcp_bin = mcp_binary(&mcp);
    let hook_sha = hook_bin.as_deref().and_then(&sha);
    let mcp_sha = mcp_bin.as_deref().and_then(&sha);
    let prov = |label: &str, bin: &Option<String>, s: &Option<String>| -> Check {
        match (bin, s) {
            (Some(p), Some(s)) => mk(
                label,
                Status::Ok,
                format!("{}  (sha {})", p, crate::sha256::short_hex(s)),
            ),
            (Some(p), None) => mk(label, Status::Fail, format!("{} — file not found", p)),
            (None, _) => mk(
                label,
                Status::Warn,
                "not configured — run `knapsack install`".into(),
            ),
        }
    };
    c.push(prov("hook configured binary", &hook_bin, &hook_sha));
    c.push(prov("MCP configured binary", &mcp_bin, &mcp_sha));
    c.push(match (&hook_sha, &mcp_sha) {
        (Some(h), Some(m)) if h == m => {
            if this_sha.as_ref().map(|t| t == h).unwrap_or(true) {
                mk(
                    "configured binary drift",
                    Status::Ok,
                    format!(
                        "hook == MCP == current binary (sha {})",
                        crate::sha256::short_hex(h)
                    ),
                )
            } else {
                mk(
                    "configured binary drift",
                    Status::Fail,
                    format!(
                        "hook/MCP {} != current binary {} — run `knapsack install --repair`",
                        crate::sha256::short_hex(h),
                        crate::sha256::short_hex(this_sha.as_deref().unwrap_or("?"))
                    ),
                )
            }
        }
        (Some(h), Some(m)) => mk(
            "configured binary drift",
            Status::Fail,
            format!(
                "hook {} != MCP {} — run `knapsack install --repair`",
                crate::sha256::short_hex(h),
                crate::sha256::short_hex(m)
            ),
        ),
        _ => mk(
            "configured binary drift",
            Status::Warn,
            "can't compare — a referenced binary is missing/unconfigured".into(),
        ),
    });
    // 6. MCP initialize works
    let init = crate::mcp::handle_message(r#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#)
        .and_then(|r| json::parse(&r).ok())
        .and_then(|v| {
            v.get("result")
                .and_then(|x| x.get("protocolVersion"))
                .and_then(|x| x.as_str())
                .map(|s| s.to_string())
        });
    c.push(match init {
        Some(p) => mk("MCP initialize", Status::Ok, format!("protocol {}", p)),
        None => mk(
            "MCP initialize",
            Status::Fail,
            "no protocolVersion in response".into(),
        ),
    });
    // 7. pack/expand smoke
    c.push(match smoke() {
        Ok(()) => mk("pack/expand smoke", Status::Ok, "byte-exact recall".into()),
        Err(e) => mk("pack/expand smoke", Status::Fail, e),
    });
    // 8. ab command works
    let rep = crate::ab::build(Path::new("\0nonexistent-kn"));
    let out = crate::ab::format(&rep);
    c.push(if out.contains("aggregate") {
        mk("ab report", Status::Ok, "renders".into())
    } else {
        mk("ab report", Status::Fail, "did not render".into())
    });

    // 9. store metadata coverage — informational. A new install starts at 0/0 and
    // grows; legacy stores intentionally show low coverage. Never a fail/warn — the
    // bytes are still byte-exact verifiable via hash::verify even without meta.
    let store = crate::store::Store::new(crate::config::store_dir());
    let (total, with_meta) = crate::gc::coverage(&store);
    c.push(mk(
        "store metadata",
        Status::Ok,
        if total == 0 {
            "0 blocks · store empty".to_string()
        } else {
            format!("{}/{} blocks have .meta sidecars", with_meta, total)
        },
    ));

    c
}

pub fn doctor() -> String {
    doctor_with_status().0
}

/// Same as `doctor()` but also returns the count of failing checks, so callers
/// (`apply`, `repair`) can propagate the failure into their exit-status accounting.
/// Warnings don't bump the fail counter — a warn-only state means "engine healthy
/// but not wired in", which is the normal state after `uninstall`.
fn doctor_with_status() -> (String, usize) {
    let checks = run_checks();
    let mut o = String::from("knapsack doctor\n\n");
    let mut fails = 0;
    let mut warns = 0;
    for ch in &checks {
        let sym = match ch.status {
            Status::Ok => "✓",
            Status::Warn => "•",
            Status::Fail => "✗",
        };
        if ch.status == Status::Fail {
            fails += 1;
        }
        if ch.status == Status::Warn {
            warns += 1;
        }
        o.push_str(&format!("  {} {:<24} {}\n", sym, ch.name, ch.detail));
    }
    o.push('\n');
    o.push_str(if fails > 0 {
        "Unhealthy ✗ — fix the failing checks above."
    } else if warns > 0 {
        "Engine healthy ✓ — but not wired in yet. Run `knapsack install`."
    } else {
        "Healthy ✓ — engine, hook, and MCP are all wired in."
    });
    o.push('\n');
    (o, fails)
}

// ---------- install / uninstall ----------

/// Result of an install / repair lifecycle action. Owns the human-readable transcript
/// AND a `success` bit so callers (e.g. main.rs, CI scripts, automated post-update
/// hooks) can detect partial failure and exit non-zero. Before this struct, a failing
/// install printed a ✗ line but exited 0, so an automated installer had no signal.
pub struct ApplyResult {
    pub output: String,
    pub success: bool,
}

impl std::fmt::Display for ApplyResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.output)
    }
}

pub fn apply() -> ApplyResult {
    let bin = bin_path();
    let mut o = String::from("knapsack install\n\n");
    let mut had_failure = false;

    // 3. ensure ~/.knapsack
    let _ = fs::create_dir_all(config::store_dir());
    if let Some(p) = config::metrics_path().parent() {
        let _ = fs::create_dir_all(p);
    }
    o.push_str(&format!(
        "  ✓ store dir         {}\n",
        config::store_dir().display()
    ));

    // 4. hook  + 5. mcp  (each with backup)
    let sp = settings_path();
    match patch_settings_file(&sp, &bin) {
        Ok(Patch::Changed(bak)) => o.push_str(&format!(
            "  ✓ hook patched      {}{}\n",
            sp.display(),
            bak_note(&bak)
        )),
        Ok(Patch::NoChange) => o.push_str(&format!("  • hook already set  {}\n", sp.display())),
        Err(e) => {
            o.push_str(&format!("  ✗ hook NOT patched  {}\n", e));
            had_failure = true;
        }
    }
    let mcp = mcp_config_path();
    match patch_mcp_file(&mcp, &bin) {
        Ok(Patch::Changed(bak)) => o.push_str(&format!(
            "  ✓ MCP patched       {}{}\n",
            mcp.display(),
            bak_note(&bak)
        )),
        Ok(Patch::NoChange) => o.push_str(&format!("  • MCP already set   {}\n", mcp.display())),
        Err(e) => {
            o.push_str(&format!("  ✗ MCP NOT patched   {}\n", e));
            had_failure = true;
        }
    }

    // 7. smoke + 8. doctor
    let smoke_ok = smoke().is_ok();
    o.push_str(&format!(
        "  {} smoke test\n",
        if smoke_ok { "✓" } else { "✗" }
    ));
    if !smoke_ok {
        had_failure = true;
    }
    o.push('\n');
    let (doctor_text, doctor_fails) = doctor_with_status();
    o.push_str(&doctor_text);
    if doctor_fails > 0 {
        had_failure = true;
    }

    // 9. rollback
    o.push_str("\nRestart Claude Code to load the hook + MCP server.\n");
    o.push_str(
        "Rollback any time:  knapsack uninstall   (add --purge to also delete the store/metrics)\n",
    );
    ApplyResult {
        output: o,
        success: !had_failure,
    }
}

fn bak_note(bak: &Option<PathBuf>) -> String {
    match bak {
        Some(b) => format!("  (backup: {})", b.display()),
        None => String::new(),
    }
}

/// Safe, idempotent User-PATH guidance for the canonical binary's directory. Advisory only:
/// the hook and MCP run by absolute path, so PATH matters only for typing `knapsack` in a
/// shell. Never emits the `setx PATH "$dest;%PATH%"` form (truncates at 1024 chars and folds
/// the combined PATH into the User scope) — uses the registry-scoped .NET setter instead.
fn path_guidance() -> String {
    let dir = match std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
    {
        Some(d) => d.display().to_string(),
        None => return String::new(),
    };
    format!(
        "\n  To resolve `knapsack` in your shell, add it to your user PATH (safe + idempotent):\n    \
         $d = '{}'; $u = [Environment]::GetEnvironmentVariable('Path','User'); \
         if (($u -split ';') -notcontains $d) {{ [Environment]::SetEnvironmentVariable('Path', \"$d;$u\", 'User') }}\n",
        dir
    )
}

/// `install --repair` — the stronger sibling of `--apply`. Force-converges the hook AND the
/// MCP entry to *this* binary (current_exe), preserving backups; prints the canonical
/// SHA-256 so the hook==MCP==this invariant is verifiable; emits safe PATH guidance; and
/// ends with a full doctor run. Use after promoting a fresh build to the install location.
pub fn repair() -> ApplyResult {
    let bin = bin_path();
    let mut o = String::from("knapsack install --repair\n\n");
    let mut had_failure = false;

    let canon_sha = std::env::current_exe()
        .ok()
        .and_then(|p| crate::sha256::sha256_file(&p));
    o.push_str(&format!("  canonical binary    {}\n", bin));
    o.push_str(&format!(
        "  canonical sha256    {}\n\n",
        canon_sha.as_deref().unwrap_or("<unreadable>")
    ));

    let _ = fs::create_dir_all(config::store_dir());
    if let Some(p) = config::metrics_path().parent() {
        let _ = fs::create_dir_all(p);
    }

    let sp = settings_path();
    match patch_settings_file(&sp, &bin) {
        Ok(Patch::Changed(bak)) => o.push_str(&format!(
            "  ✓ hook repointed    {}{}\n",
            sp.display(),
            bak_note(&bak)
        )),
        Ok(Patch::NoChange) => o.push_str(&format!("  • hook already ok   {}\n", sp.display())),
        Err(e) => {
            o.push_str(&format!("  ✗ hook NOT fixed    {}\n", e));
            had_failure = true;
        }
    }
    let mcp = mcp_config_path();
    match patch_mcp_file(&mcp, &bin) {
        Ok(Patch::Changed(bak)) => o.push_str(&format!(
            "  ✓ MCP repointed     {}{}\n",
            mcp.display(),
            bak_note(&bak)
        )),
        Ok(Patch::NoChange) => o.push_str(&format!("  • MCP already ok    {}\n", mcp.display())),
        Err(e) => {
            o.push_str(&format!("  ✗ MCP NOT fixed     {}\n", e));
            had_failure = true;
        }
    }

    o.push_str(&path_guidance());
    o.push('\n');
    let (doctor_text, doctor_fails) = doctor_with_status();
    o.push_str(&doctor_text);
    if doctor_fails > 0 {
        had_failure = true;
    }
    o.push_str("\nRestart Claude Code to load the repointed hook + MCP server.\n");
    ApplyResult {
        output: o,
        success: !had_failure,
    }
}

pub fn uninstall(purge: bool) -> String {
    let mut o = String::from("knapsack uninstall\n\n");
    let sp = settings_path();
    match unpatch_settings_file(&sp) {
        Ok(Patch::Changed(bak)) => o.push_str(&format!(
            "  ✓ hook removed      {}{}\n",
            sp.display(),
            bak_note(&bak)
        )),
        Ok(Patch::NoChange) => o.push_str(&format!("  • no hook found     {}\n", sp.display())),
        Err(e) => o.push_str(&format!("  ✗ {}\n", e)),
    }
    let mcp = mcp_config_path();
    match unpatch_mcp_file(&mcp) {
        Ok(Patch::Changed(bak)) => o.push_str(&format!(
            "  ✓ MCP removed       {}{}\n",
            mcp.display(),
            bak_note(&bak)
        )),
        Ok(Patch::NoChange) => o.push_str(&format!("  • no MCP entry      {}\n", mcp.display())),
        Err(e) => o.push_str(&format!("  ✗ {}\n", e)),
    }

    if purge {
        let _ = fs::remove_dir_all(config::store_dir());
        let _ = fs::remove_file(config::metrics_path());
        o.push_str("  ✓ purged store + metrics\n");
    } else {
        o.push_str(&format!(
            "  • kept store + metrics ({} , {}) — re-run with --purge to delete\n",
            config::store_dir().display(),
            config::metrics_path().display()
        ));
    }
    o.push_str("\nRestart Claude Code to unload the hook + MCP server.\n");
    o
}
