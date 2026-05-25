//! A tiny, dependency-free JSON value type + parser + serializer. This exists ONLY for
//! the integration glue (reading Claude Code's hook payload, writing metrics) — the
//! deterministic core never touches it. serde_json is the obvious later swap; until then
//! this is small, ordered (objects keep key order for faithful re-emit), and tested.

#[derive(Debug, Clone, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    Arr(Vec<Json>),
    Obj(Vec<(String, Json)>),
}

impl Json {
    pub fn get(&self, key: &str) -> Option<&Json> {
        if let Json::Obj(o) = self {
            o.iter().find(|(k, _)| k == key).map(|(_, v)| v)
        } else {
            None
        }
    }
    pub fn as_str(&self) -> Option<&str> {
        if let Json::Str(s) = self {
            Some(s)
        } else {
            None
        }
    }
    pub fn as_f64(&self) -> Option<f64> {
        if let Json::Num(n) = self {
            Some(*n)
        } else {
            None
        }
    }
    pub fn as_bool(&self) -> Option<bool> {
        if let Json::Bool(b) = self {
            Some(*b)
        } else {
            None
        }
    }
}

/// Cap on nesting depth. The parser is recursive-descent, so without a bound a pathologically
/// nested payload (the hook/MCP read semi-trusted input) would overflow the stack and ABORT
/// the process instead of failing open. 256 is far beyond any real Claude Code event or MCP
/// request, and well under the overflow point on a small (1 MB) stack.
const MAX_DEPTH: usize = 256;

pub fn parse(s: &str) -> Result<Json, String> {
    let mut p = Parser { c: s.chars().collect(), i: 0, depth: 0 };
    p.ws();
    let v = p.value()?;
    p.ws();
    Ok(v)
}

struct Parser {
    c: Vec<char>,
    i: usize,
    depth: usize,
}

impl Parser {
    fn peek(&self) -> Option<char> {
        self.c.get(self.i).copied()
    }
    fn ws(&mut self) {
        while matches!(self.peek(), Some(' ') | Some('\t') | Some('\n') | Some('\r')) {
            self.i += 1;
        }
    }
    fn value(&mut self) -> Result<Json, String> {
        self.ws();
        // Bound recursion depth so deep nesting errors out instead of overflowing the stack.
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err("nesting too deep".into());
        }
        let r = match self.peek() {
            Some('{') => self.obj(),
            Some('[') => self.arr(),
            Some('"') => Ok(Json::Str(self.string()?)),
            Some('t') | Some('f') => self.boolean(),
            Some('n') => self.null(),
            Some(c) if c == '-' || c.is_ascii_digit() => self.number(),
            other => Err(format!("unexpected {:?}", other)),
        };
        self.depth -= 1;
        r
    }
    fn obj(&mut self) -> Result<Json, String> {
        self.i += 1; // {
        let mut v = Vec::new();
        self.ws();
        if self.peek() == Some('}') {
            self.i += 1;
            return Ok(Json::Obj(v));
        }
        loop {
            self.ws();
            let k = self.string()?;
            self.ws();
            if self.peek() != Some(':') {
                return Err("expected ':'".into());
            }
            self.i += 1;
            let val = self.value()?;
            v.push((k, val));
            self.ws();
            match self.peek() {
                Some(',') => {
                    self.i += 1;
                    continue;
                }
                Some('}') => {
                    self.i += 1;
                    break;
                }
                other => return Err(format!("expected ',' or '}}', got {:?}", other)),
            }
        }
        Ok(Json::Obj(v))
    }
    fn arr(&mut self) -> Result<Json, String> {
        self.i += 1; // [
        let mut v = Vec::new();
        self.ws();
        if self.peek() == Some(']') {
            self.i += 1;
            return Ok(Json::Arr(v));
        }
        loop {
            v.push(self.value()?);
            self.ws();
            match self.peek() {
                Some(',') => {
                    self.i += 1;
                    continue;
                }
                Some(']') => {
                    self.i += 1;
                    break;
                }
                other => return Err(format!("expected ',' or ']', got {:?}", other)),
            }
        }
        Ok(Json::Arr(v))
    }
    fn string(&mut self) -> Result<String, String> {
        if self.peek() != Some('"') {
            return Err("expected string".into());
        }
        self.i += 1;
        let mut s = String::new();
        while let Some(c) = self.peek() {
            self.i += 1;
            match c {
                '"' => return Ok(s),
                '\\' => {
                    let e = self.peek().ok_or("bad escape")?;
                    self.i += 1;
                    match e {
                        '"' => s.push('"'),
                        '\\' => s.push('\\'),
                        '/' => s.push('/'),
                        'n' => s.push('\n'),
                        't' => s.push('\t'),
                        'r' => s.push('\r'),
                        'b' => s.push('\u{08}'),
                        'f' => s.push('\u{0C}'),
                        'u' => {
                            let cp = self.hex4()?;
                            if (0xD800..=0xDBFF).contains(&cp) {
                                // high surrogate; expect a following \uXXXX low surrogate
                                if self.peek() == Some('\\') {
                                    self.i += 1;
                                    if self.peek() == Some('u') {
                                        self.i += 1;
                                        let lo = self.hex4()?;
                                        let c = 0x10000 + (((cp as u32 - 0xD800) << 10) | (lo as u32 - 0xDC00));
                                        if let Some(ch) = char::from_u32(c) {
                                            s.push(ch);
                                        }
                                    }
                                }
                            } else if let Some(ch) = char::from_u32(cp as u32) {
                                s.push(ch);
                            }
                        }
                        other => return Err(format!("bad escape \\{}", other)),
                    }
                }
                _ => s.push(c),
            }
        }
        Err("unterminated string".into())
    }
    fn hex4(&mut self) -> Result<u16, String> {
        let mut h = String::new();
        for _ in 0..4 {
            h.push(self.peek().ok_or("short \\u")?);
            self.i += 1;
        }
        u16::from_str_radix(&h, 16).map_err(|_| "bad \\u".into())
    }
    fn boolean(&mut self) -> Result<Json, String> {
        if self.starts_with("true") {
            self.i += 4;
            Ok(Json::Bool(true))
        } else if self.starts_with("false") {
            self.i += 5;
            Ok(Json::Bool(false))
        } else {
            Err("bad literal".into())
        }
    }
    fn null(&mut self) -> Result<Json, String> {
        if self.starts_with("null") {
            self.i += 4;
            Ok(Json::Null)
        } else {
            Err("bad literal".into())
        }
    }
    fn starts_with(&self, lit: &str) -> bool {
        lit.chars().enumerate().all(|(k, ch)| self.c.get(self.i + k) == Some(&ch))
    }
    fn number(&mut self) -> Result<Json, String> {
        let start = self.i;
        while let Some(c) = self.peek() {
            if c == '-' || c == '+' || c == '.' || c == 'e' || c == 'E' || c.is_ascii_digit() {
                self.i += 1;
            } else {
                break;
            }
        }
        let s: String = self.c[start..self.i].iter().collect();
        s.parse::<f64>().map(Json::Num).map_err(|_| "bad number".into())
    }
}

fn esc(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 2);
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\t' => o.push_str("\\t"),
            '\r' => o.push_str("\\r"),
            c if (c as u32) < 0x20 => o.push_str(&format!("\\u{:04x}", c as u32)),
            c => o.push(c),
        }
    }
    o
}

pub fn to_string(j: &Json) -> String {
    match j {
        Json::Null => "null".into(),
        Json::Bool(b) => b.to_string(),
        Json::Num(n) => {
            if n.fract() == 0.0 && n.is_finite() && n.abs() < 9.0e15 {
                (*n as i64).to_string()
            } else {
                n.to_string()
            }
        }
        Json::Str(s) => format!("\"{}\"", esc(s)),
        Json::Arr(a) => format!("[{}]", a.iter().map(to_string).collect::<Vec<_>>().join(",")),
        Json::Obj(o) => format!(
            "{{{}}}",
            o.iter().map(|(k, v)| format!("\"{}\":{}", esc(k), to_string(v))).collect::<Vec<_>>().join(",")
        ),
    }
}

/// Replace or insert a key in an object's entries.
pub fn set_key(obj: &mut Vec<(String, Json)>, key: &str, val: Json) {
    if let Some(e) = obj.iter_mut().find(|(k, _)| k == key) {
        e.1 = val;
    } else {
        obj.push((key.to_string(), val));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_nested_with_escapes() {
        // A command field with escaped quotes, a backslash, and a newline — the kind of
        // payload that breaks naive substring extraction.
        let raw = r#"{"tool_name":"Bash","session_id":"abc-123","tool_input":{"command":"echo \"hi\" && rg 'a\\b'\ndone","timeout":5}}"#;
        let v = parse(raw).unwrap();
        assert_eq!(v.get("tool_name").and_then(|x| x.as_str()), Some("Bash"));
        assert_eq!(v.get("session_id").and_then(|x| x.as_str()), Some("abc-123"));
        let cmd = v.get("tool_input").and_then(|t| t.get("command")).and_then(|x| x.as_str()).unwrap();
        assert!(cmd.contains("echo \"hi\""));
        assert!(cmd.contains("a\\b"));
        assert!(cmd.contains('\n'));
    }
    #[test]
    fn roundtrip_preserves_command() {
        let raw = r#"{"command":"npm test","description":"run"}"#;
        let v = parse(raw).unwrap();
        let s = to_string(&v);
        let v2 = parse(&s).unwrap();
        assert_eq!(v, v2);
    }
    #[test]
    fn integers_serialize_clean() {
        assert_eq!(to_string(&Json::Num(42.0)), "42");
        assert_eq!(to_string(&Json::Num(1_700_000_000_000.0)), "1700000000000");
    }
}
