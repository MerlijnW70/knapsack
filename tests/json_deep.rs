//! The JSON layer parses Claude Code's hook payloads and MCP requests — semi-trusted input.
//! It must (a) round-trip any value it can represent, (b) never PANIC on malformed bytes
//! (return Err instead), and (c) handle reasonably deep nesting. Recursion depth is probed
//! separately (live) since an unbounded recursive-descent parser can stack-overflow.

use knapsack::json::{self, Json};

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

fn rand_str(rng: &mut Rng) -> String {
    let n = rng.below(10);
    (0..n)
        .map(|_| match rng.below(11) {
            0 => '"',
            1 => '\\',
            2 => '\n',
            3 => '\t',
            4 => '\r',
            5 => 'é',
            6 => '🚀',
            7 => '\u{1}', // control char -> \u00xx
            _ => (b'a' + rng.below(26) as u8) as char,
        })
        .collect()
}

fn gen_json(rng: &mut Rng, depth: usize) -> Json {
    if depth == 0 {
        return match rng.below(4) {
            0 => Json::Null,
            1 => Json::Bool(rng.next().is_multiple_of(2)),
            2 => Json::Num((rng.next() % 1_000_000) as f64), // exact-integer f64 -> clean round-trip
            _ => Json::Str(rand_str(rng)),
        };
    }
    match rng.below(6) {
        0 => Json::Null,
        1 => Json::Bool(true),
        2 => Json::Num((rng.next() % 1_000_000) as f64),
        3 => Json::Str(rand_str(rng)),
        4 => {
            let n = rng.below(5);
            Json::Arr((0..n).map(|_| gen_json(rng, depth - 1)).collect())
        }
        _ => {
            let n = rng.below(5);
            Json::Obj((0..n).map(|i| (format!("k{i}_{}", rng.below(100)), gen_json(rng, depth - 1))).collect())
        }
    }
}

#[test]
fn json_roundtrips_any_representable_value() {
    let mut rng = Rng(0x1234_5678_9ABC_DEF0);
    for _ in 0..5000 {
        let v = gen_json(&mut rng, 5);
        let s = json::to_string(&v);
        let back = json::parse(&s).unwrap_or_else(|e| panic!("failed to re-parse our own output {s:?}: {e}"));
        assert_eq!(back, v, "JSON round-trip must be lossless. serialized: {s}");
    }
}

#[test]
fn malformed_input_never_panics() {
    let mut rng = Rng(0x00FE_EDFA_CEC0_FFEE);
    let seeds = [
        "", "{", "}", "[", "]", "\"", "\\", "{\"a\"", "{\"a\":}", "[,]", "tru", "nul",
        "-", "1.2.3", "{\"a\":1,}", "\"\\u00\"", "\"\\uZZZZ\"", "\"\\q\"", "{1:2}", "1e", "1e999",
    ];
    for s in seeds {
        let _ = json::parse(s); // must return Ok/Err, never panic
    }
    // random byte soup (lossy-decoded to a &str) must also never panic the parser
    for _ in 0..5000 {
        let n = rng.below(40);
        let bytes: Vec<u8> = (0..n).map(|_| rng.next() as u8).collect();
        let s = String::from_utf8_lossy(&bytes);
        let _ = json::parse(&s);
    }
}

#[test]
fn moderately_deep_nesting_parses() {
    // Well under the depth cap: normal-to-deep nesting must still parse correctly.
    let depth = 100;
    let s = format!("{}{}{}", "[".repeat(depth), "1", "]".repeat(depth));
    let v = json::parse(&s).expect("100-deep array should parse");
    let mut cur = &v;
    let mut got = 0;
    while let Json::Arr(a) = cur {
        if a.is_empty() {
            break;
        }
        cur = &a[0];
        got += 1;
    }
    assert_eq!(got, depth, "nesting depth preserved");
}

#[test]
fn extreme_nesting_errors_instead_of_overflowing_the_stack() {
    // Regression: an unbounded recursive-descent parser overflowed the stack (and ABORTED the
    // process) at a few thousand levels. The depth cap must turn that into a clean Err — if
    // this test ever crashes the test binary instead of failing, the cap regressed.
    let s = format!("{}{}{}", "[".repeat(5000), "1", "]".repeat(5000));
    assert!(json::parse(&s).is_err(), "deeply nested input must error, not crash");
    // Same for objects.
    let o = format!("{}{}{}", "{\"k\":".repeat(5000), "1", "}".repeat(5000));
    assert!(json::parse(&o).is_err(), "deeply nested object must error, not crash");
}
