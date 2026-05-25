//! Content-type detection — picks the structural strategy. Code vs Log for now (JSON,
//! diff, markdown are future variants behind the same enum).

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ContentType {
    Code,
    Log,
}

const CODE_EXT: [&str; 21] = [
    ".js", ".jsx", ".ts", ".tsx", ".mjs", ".cjs", ".py", ".rs", ".go", ".java", ".c", ".h", ".cpp",
    ".cc", ".hpp", ".cs", ".rb", ".php", ".swift", ".kt", ".scala",
];

pub fn detect(bytes: &[u8], file_path: Option<&str>) -> ContentType {
    if let Some(p) = file_path {
        let pl = p.to_ascii_lowercase();
        if CODE_EXT.iter().any(|e| pl.ends_with(e)) {
            return ContentType::Code;
        }
    }
    let text = String::from_utf8_lossy(bytes);
    let (mut sig, mut nb) = (0usize, 0usize);
    for l in text.lines().take(400) {
        if l.trim().is_empty() {
            continue;
        }
        nb += 1;
        let t = l.trim_start();
        if is_sig(t) || is_method(t) {
            sig += 1;
        }
    }
    if nb >= 8 && (sig as f64) / (nb as f64) >= 0.06 {
        ContentType::Code
    } else {
        ContentType::Log
    }
}

const SIG_KW: [&str; 16] = [
    "import", "export", "require(", "module", "package", "#include", "function", "class",
    "interface", "enum", "namespace", "fn ", "func ", "def ", "pub ", "type ",
];

/// A line (already left-trimmed) that declares structure worth always keeping.
pub fn is_sig(t: &str) -> bool {
    if SIG_KW.iter().any(|k| t.starts_with(k)) {
        return true;
    }
    if (t.starts_with("const ") || t.starts_with("let ") || t.starts_with("var "))
        && t.contains('=')
        && (t.contains("function") || t.contains("=>") || t.trim_end().ends_with('{'))
    {
        return true;
    }
    false
}

/// A bare method/function signature line opening a block: `name(args) {`, not control flow.
pub fn is_method(t: &str) -> bool {
    let l = t.trim_end();
    if !l.ends_with('{') {
        return false;
    }
    let ident: String = l.chars().take_while(|c| c.is_alphanumeric() || *c == '_' || *c == '$').collect();
    if ident.is_empty() {
        return false;
    }
    const CTRL: [&str; 10] = ["if", "for", "while", "switch", "catch", "do", "return", "else", "function", "class"];
    if CTRL.contains(&ident.as_str()) {
        return false;
    }
    l.contains('(') && l.contains(')')
}
