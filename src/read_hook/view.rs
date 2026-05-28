//! View construction for the Read hook. Builds the compact view the model reads
//! instead of the original AND populates the byte-exact store with every handle the
//! view names — so the `knapsack expand ks2_X` instructions in the view resolve
//! byte-exact when the model invokes them.

use crate::config::store_dir;
use crate::content_type::detect;
use crate::sha256::sha256_hex;
use crate::store::Store;
use crate::structural;
use std::path::Path;

/// Detects markdown by file extension. Used by the read hook to route through
/// `pack_doc` instead of `structural::compress`, which doesn't recognise heading /
/// code-fence / list structure. Content sniffing would be brittle (any text starting
/// with `#` could be markdown or could be a config comment) and extension matches
/// real-world authoring tooling, so we keep it strict.
fn is_markdown_path(p: &Path) -> bool {
    p.extension()
        .and_then(|e| e.to_str())
        .map(|e| {
            let lc = e.to_ascii_lowercase();
            matches!(lc.as_str(), "md" | "markdown" | "mdown" | "mkd" | "mkdn")
        })
        .unwrap_or(false)
}

/// Compute the (compact view body, whole-file handle) pair for a source, picking the
/// right compressor for its content type.
///
/// Three classes of handle land in the store:
///   1. Every elision returned by `structural::compress` (when used) — recall targets
///      the view points at in its `[Knapsack: ... · recall ks2_X]` markers.
///   2. The whole-file handle — recall target the outer header advertises, and (for
///      markdown) the target every `<!-- ks-recall handle=... lines=A-B -->` points
///      at via `--lines`.
///   3. Storage is content-addressed + idempotent (`put_with_handle` skips when the
///      file already exists), so calling this on every view-build is cheap.
///
/// Session id is stamped into each block's `.meta` so a later `expand_handle`
/// attributes the recall to THIS session.
fn compile_compact(source_path: &Path, bytes: &[u8], store: &Store) -> (String, crate::hash::Handle) {
    if is_markdown_path(source_path) {
        // pack_doc understands markdown structure (headings, code fences, lists,
        // blockquotes). It puts the WHOLE FILE in the store under one handle and
        // emits `<!-- ks-recall handle=H lines=A-B -->` markers that recall via
        // `knapsack expand H --lines A-B`. No per-section block writes needed.
        let r = crate::pack_doc::pack_doc(&source_path.to_string_lossy(), bytes, store);
        // pack_doc emits its own two-line HTML header + a blank line — strip them so
        // the read-cache header above stays the single source of identity.
        let body = strip_pack_doc_header(&r.view);
        (body, r.handle)
    } else {
        // Everything else goes through the general structural compressor (Code/Log/JSON).
        let ct = detect(bytes, Some(source_path.to_string_lossy().as_ref()));
        let (compact, elisions) = structural::compress(bytes, 0, bytes.len(), ct);
        for el in &elisions {
            store.put_with_handle(&el.handle, &bytes[el.start..el.end]);
        }
        let whole = store.put(bytes);
        (compact, whole)
    }
}

/// Strip `pack_doc`'s two-line header (machine manifest + inspect hint) plus the
/// blank line that follows. We replace it with the read-cache header so users see
/// one consistent header across all file types. If the input doesn't start with
/// pack_doc's manifest comment, we hand it back unchanged — safe fallback.
fn strip_pack_doc_header(view: &str) -> String {
    if !view.starts_with("<!-- ks-pack source=") {
        return view.to_string();
    }
    let mut iter = view.splitn(4, '\n');
    let _l1 = iter.next(); // <!-- ks-pack source=... -->
    let _l2 = iter.next(); // <!-- knapsack inspect ... -->
    let _l3 = iter.next(); // blank
    iter.next().unwrap_or("").to_string()
}

/// Build the compact view file that Claude reads instead of the original AND populate
/// the byte-exact store with every handle the view names — so the `knapsack expand
/// ks2_X` instructions in the view resolve byte-exact when the model invokes them.
pub(super) fn build_view(source_path: &Path, bytes: &[u8], session_id: &str) -> String {
    let store = Store::with_session(store_dir(), session_id);
    let (compact, whole_handle) = compile_compact(source_path, bytes, &store);

    let mut o = String::new();
    o.push_str("<!-- Knapsack read cache -->\n");
    o.push_str(&format!("<!-- Original file: {} -->\n", source_path.display()));
    o.push_str(&format!("<!-- Source digest: sha256={} -->\n", sha256_hex(bytes)));
    o.push_str("<!-- This file is a COMPRESSED VIEW. -->\n");
    o.push_str("<!--   Exact original is on disk at the path above. -->\n");
    o.push_str(&format!(
        "<!--   Or recall exact bytes via `knapsack expand {}`. -->\n\n",
        whole_handle
    ));
    o.push_str("[Knapsack: read-cache view · the original at the path above remains the source of truth]\n\n");
    o.push_str(&compact);
    o
}

/// Mirror of the store-population side of `build_view` without producing the view text.
/// Used on the cache-hit path so the store stays in sync with the cache even if the
/// store dir was wiped between sessions (a `knapsack uninstall --purge` followed by
/// the user keeping their cache dir, e.g. when restoring from a backup). Calls
/// `compile_compact` so the same routing logic (markdown -> pack_doc, else
/// structural::compress) applies to the recovery write as to the original build —
/// no path-specific shortcut that could leave the store missing handles the cached
/// view advertises.
pub(super) fn populate_store(source_path: &Path, bytes: &[u8], session_id: &str) {
    let store = Store::with_session(store_dir(), session_id);
    // Side-effect of compile_compact is the store population we want; the returned
    // (compact, handle) pair is unused here.
    let _ = compile_compact(source_path, bytes, &store);
}
