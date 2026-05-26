//! Code-block splitter contract. The new boundary-based rule says: a block is a
//! top-level definition (fn/function/class/def/impl/struct/...) plus everything up
//! to the next top-level definition. Internal blank lines INSIDE a function MUST NOT
//! fragment the block — that was the regression the old blank-line-only splitter
//! caused for the edit-test loop on real codebases.
//!
//! When a file has NO recognisable definitions (minified code, scripts, ad-hoc dumps)
//! the splitter falls back to the historical blank-line behaviour — never worse than
//! before.

use knapsack::block::split_blocks;
use knapsack::content_type::ContentType;

fn blocks_of(s: &str) -> Vec<String> {
    let bytes = s.as_bytes();
    split_blocks(bytes, ContentType::Code)
        .into_iter()
        .map(|(a, b)| String::from_utf8_lossy(&bytes[a..b]).into_owned())
        .collect()
}

/// Tile invariants: contiguous, covers the input exactly, byte-exact concat == input.
fn assert_tiles(input: &str, blocks: &[(usize, usize)]) {
    let bytes = input.as_bytes();
    if bytes.is_empty() {
        assert!(blocks.is_empty(), "empty input -> no tiles");
        return;
    }
    assert_eq!(blocks.first().map(|b| b.0), Some(0), "first tile starts at 0");
    assert_eq!(blocks.last().map(|b| b.1), Some(bytes.len()), "last tile ends at len");
    for w in blocks.windows(2) {
        assert_eq!(w[0].1, w[1].0, "no gap/overlap");
    }
    let rejoined: Vec<u8> = blocks.iter().flat_map(|&(s, e)| bytes[s..e].iter().copied()).collect();
    assert_eq!(&rejoined, bytes, "byte-exact concat");
}

#[test]
fn function_with_internal_blank_lines_stays_one_block() {
    // The classic case the old splitter mangled: a function with paragraph breaks
    // (a vertical breath inside the body) used to become 3+ tiny blocks. Now: one.
    let src = "\
fn process_request(input: &Request) -> Response {
    let parsed = parse(input);

    let validated = validate(&parsed);

    let result = run(&validated);
    finalize(result)
}
";
    let blocks = split_blocks(src.as_bytes(), ContentType::Code);
    assert_tiles(src, &blocks);
    assert_eq!(blocks.len(), 1, "one function -> one block; got {:?}", blocks_of(src));
}

#[test]
fn multiple_top_level_functions_become_separate_blocks() {
    // Two functions, with the second carrying an internal blank. The boundary lands
    // on `fn second(...)`; the blank inside it doesn't open another block.
    let src = "\
fn first(x: i32) -> i32 {
    x + 1
}

fn second(x: i32) -> i32 {
    let a = x * 2;

    a - 1
}
";
    let blocks = split_blocks(src.as_bytes(), ContentType::Code);
    assert_tiles(src, &blocks);
    assert_eq!(blocks.len(), 2, "two top-level fns -> two blocks; got {:?}", blocks_of(src));
    let texts = blocks_of(src);
    assert!(texts[0].contains("fn first"), "first block holds first()");
    assert!(texts[1].contains("fn second"), "second block holds second()");
    assert!(texts[1].contains("a - 1"), "second's internal blank doesn't break out");
}

#[test]
fn rust_preamble_imports_become_their_own_block_before_first_fn() {
    let src = "\
//! crate doc

use std::collections::HashMap;
use std::path::Path;

fn main() {
    println!(\"hi\");
}
";
    let blocks = split_blocks(src.as_bytes(), ContentType::Code);
    assert_tiles(src, &blocks);
    assert_eq!(blocks.len(), 2, "preamble + main: {:?}", blocks_of(src));
    let texts = blocks_of(src);
    assert!(texts[0].contains("use std::collections::HashMap"), "preamble holds imports");
    assert!(!texts[0].contains("fn main"), "preamble does NOT contain main()");
    assert!(texts[1].starts_with("fn main"), "second block starts at the boundary");
}

#[test]
fn python_class_and_top_level_def_split_independently() {
    // The body of the class includes an indented `def` — that MUST NOT be a
    // boundary, because it's at column 4. Only column-0 `def`/`class` count.
    let src = "\
\"\"\"module doc\"\"\"

import sys

def helper(x):
    return x * 2

class Service:
    def __init__(self):
        self.x = 0

    def run(self):
        return self.x + 1

def main():
    s = Service()
    print(s.run())
";
    let blocks = split_blocks(src.as_bytes(), ContentType::Code);
    assert_tiles(src, &blocks);
    // Expected: preamble · helper · Service · main = 4 blocks.
    assert_eq!(blocks.len(), 4, "py preamble + helper + class + main = 4 blocks; got {:?}", blocks_of(src));
    let texts = blocks_of(src);
    assert!(texts[0].contains("import sys"));
    assert!(texts[1].contains("def helper"));
    assert!(texts[2].contains("class Service"));
    assert!(texts[2].contains("def __init__"), "class block keeps its INDENTED defs");
    assert!(texts[2].contains("def run"), "class block keeps both indented methods");
    assert!(texts[3].contains("def main"));
}

#[test]
fn javascript_classes_and_functions_split() {
    let src = "\
import { foo } from './foo.js';

export function handler(req, res) {
  return res.send('hi');
}

export class Service {
  constructor() {
    this.x = 0;
  }
  run() {
    return this.x + 1;
  }
}

async function init() {
  await ready();
}
";
    let blocks = split_blocks(src.as_bytes(), ContentType::Code);
    assert_tiles(src, &blocks);
    assert_eq!(blocks.len(), 4, "preamble + handler + Service + init: {:?}", blocks_of(src));
}

#[test]
fn minified_code_with_no_definitions_falls_back_to_blank_line_split() {
    // Single-line bundle: no column-0 fn/function/class. The splitter must NOT crash,
    // and the result must remain a valid tiling. Old behaviour: one big block. New
    // behaviour preserves that — `has_def == false` triggers the fallback.
    let src = "var a=1;var b=2;var c=3;var d=a+b+c;console.log(d);";
    let blocks = split_blocks(src.as_bytes(), ContentType::Code);
    assert_tiles(src, &blocks);
    assert_eq!(blocks.len(), 1, "no defs found -> single block fallback: {:?}", blocks_of(src));
}

#[test]
fn empty_code_input_returns_no_blocks() {
    let blocks = split_blocks(b"", ContentType::Code);
    assert!(blocks.is_empty(), "empty input -> no blocks");
}

#[test]
fn single_function_at_top_returns_one_block_with_function() {
    // A file that's nothing but one function (no preamble). The splitter should
    // produce ONE block holding the entire function.
    let src = "\
fn only() {
    do_thing();

    do_other_thing();
}
";
    let blocks = split_blocks(src.as_bytes(), ContentType::Code);
    assert_tiles(src, &blocks);
    assert_eq!(blocks.len(), 1);
    assert!(blocks_of(src)[0].contains("fn only"));
}

#[test]
fn one_function_edit_only_invalidates_that_block() {
    // The whole point of better splitting: editing one function changes ONE block,
    // not all of them. Use our own packer to confirm the delta is local.
    use knapsack::ledger::Ledger;
    use knapsack::pack::pack;
    use knapsack::store::Store;

    let dir = std::env::temp_dir().join(format!(
        "knapsack-codesplit-{}-{}",
        std::process::id(),
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
    ));
    let store = Store::new(dir.clone());
    let mut ledger = Ledger::in_memory();

    fn file(edited: usize) -> String {
        let mut s = String::from("// module\n\nuse std::fmt;\n\n");
        for i in 0..8 {
            let v = if i == 3 { edited } else { 0 };
            s.push_str(&format!(
                "fn handler{i}() {{\n    let acc = {v};\n\n    finalize(acc);\n}}\n\n",
                i = i,
                v = v
            ));
        }
        s
    }

    let v1 = file(0);
    let v2 = file(42); // edit handler3
    let _r1 = pack(v1.as_bytes(), ContentType::Code, &store, &mut ledger, 0);
    let r2 = pack(v2.as_bytes(), ContentType::Code, &store, &mut ledger, 1);

    let blocks_v2 = split_blocks(v2.as_bytes(), ContentType::Code);
    // 1 preamble + 8 handlers = 9 blocks. Only handler3 changes; the rest are resident
    // and back-reference. delta_hits == blocks - 1 (the changed handler3).
    assert!(blocks_v2.len() >= 8, "split produces per-function blocks");
    assert!(
        r2.delta_hits >= blocks_v2.len() - 1,
        "only ONE block (the edited handler) should be new; got delta_hits={} of {} blocks",
        r2.delta_hits,
        blocks_v2.len()
    );

    let _ = std::fs::remove_dir_all(&dir);
}
