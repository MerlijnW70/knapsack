//! Content-type detection picks the block strategy. It's internally consistent (reconstruct
//! uses the same ct), but classification quality matters for compression. Lock the clear
//! cases: a code extension wins outright; obvious code vs log content classify correctly;
//! too-little signal and binary default to Log; detection never panics.

use knapsack::content_type::{detect, ContentType};

#[test]
fn code_extension_wins_regardless_of_content() {
    for ext in [
        "rs", "js", "jsx", "ts", "tsx", "py", "go", "java", "c", "h", "cpp", "rb", "php", "swift",
        "kt",
    ] {
        let path = format!("src/thing.{ext}");
        assert_eq!(
            detect(
                b"this is not even code\njust some words here\n",
                Some(&path)
            ),
            ContentType::Code,
            "extension .{ext} must classify as Code"
        );
    }
    // Uppercase extension too (detection lowercases the path).
    assert_eq!(detect(b"whatever", Some("Main.RS")), ContentType::Code);
}

#[test]
fn non_code_extension_falls_through_to_content() {
    let mut log = String::new();
    for i in 0..20 {
        log.push_str(&format!("[INFO] line {i} ok\n"));
    }
    assert_eq!(detect(log.as_bytes(), Some("output.txt")), ContentType::Log);
    assert_eq!(detect(log.as_bytes(), Some("server.log")), ContentType::Log);
}

#[test]
fn obvious_code_content_without_extension_is_code() {
    let code = "import os\nimport sys\nclass Foo:\n    def bar(self):\n        return 1\n\
                def baz():\n    pass\nexport function q() {}\nconst x = require('y');\nfn main() {}\n";
    assert_eq!(detect(code.as_bytes(), None), ContentType::Code);
}

#[test]
fn obvious_log_content_without_extension_is_log() {
    let mut s = String::new();
    for i in 0..40 {
        s.push_str(&format!(
            "2026-05-25T10:{:02}:00 request id={i} latency={}ms status=200\n",
            i % 60,
            i * 3
        ));
    }
    assert_eq!(detect(s.as_bytes(), None), ContentType::Log);
}

#[test]
fn too_little_signal_defaults_to_log() {
    // Fewer than the minimum non-blank lines -> Log even if the lines look code-ish.
    assert_eq!(
        detect(b"def f():\n    pass\n", None),
        ContentType::Log,
        "2 lines is not enough to call it code"
    );
    assert_eq!(detect(b"", None), ContentType::Log);
    assert_eq!(detect(b"\n\n\n\n", None), ContentType::Log);
    assert_eq!(detect(b"a single line", None), ContentType::Log);
}

#[test]
fn binary_and_adversarial_never_panic() {
    let blob: Vec<u8> = (0..4000).map(|i| (i * 7 % 256) as u8).collect();
    assert_eq!(detect(&blob, None), ContentType::Log);
    // mixed nul / high bytes / newlines with a path that isn't a code ext
    let mixed: Vec<u8> = b"\x00\xff\n\xfe\x80line\n"
        .iter()
        .cycle()
        .take(3000)
        .copied()
        .collect();
    let _ = detect(&mixed, Some("data.bin"));
    let _ = detect(&mixed, None);
}
