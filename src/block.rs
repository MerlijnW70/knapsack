//! Block partitioning — the unit of the delta. Blocks are BYTE RANGES that TILE the
//! input exactly (block[i].end == block[i+1].start, covering [0, len)). This is what
//! makes recall byte-exact: a stored block is a verbatim slice of the original, and
//! concatenating every block reproduces the input bit-for-bit. Boundary choice decides
//! delta quality: one block per top-level unit for code, small line-chunks for logs.

use crate::content_type::ContentType;

/// Byte ranges of each line, INCLUDING its trailing `\n`. Tiles [0, len) exactly.
pub fn split_lines(b: &[u8]) -> Vec<(usize, usize)> {
    let mut v = Vec::new();
    let mut start = 0usize;
    for (i, &c) in b.iter().enumerate() {
        if c == b'\n' {
            v.push((start, i + 1));
            start = i + 1;
        }
    }
    if start < b.len() {
        v.push((start, b.len()));
    }
    v
}

pub fn line_is_blank(b: &[u8], s: usize, e: usize) -> bool {
    b[s..e].iter().all(|&c| c == b' ' || c == b'\t' || c == b'\r' || c == b'\n')
}

const LOG_CHUNK: usize = 6;

/// A line (left-trimmed of spaces/tabs, trailing CR/LF dropped) that marks a stable
/// boundary in build/test output. Boundaries chosen by CONTENT — not absolute offset —
/// so inserting or removing a header line (e.g. cargo's `Compiling …` after an edit) only
/// changes its own block; the unchanged test-output blocks keep identical bytes and still
/// dedup across an edit→test loop. Heuristic, fail-safe: a miss just yields larger blocks.
fn is_log_anchor(b: &[u8], s: usize, e: usize) -> bool {
    let mut a = s;
    while a < e && matches!(b[a], b' ' | b'\t') {
        a += 1;
    }
    let mut z = e;
    while z > a && matches!(b[z - 1], b'\n' | b'\r') {
        z -= 1;
    }
    let line = &b[a..z];
    const PREFIX: [&[u8]; 11] = [
        b"test ",      // `test name ... ok` and `test result: …`
        b"Compiling ", // volatile build headers, isolated into their own block
        b"Finished ",
        b"Running ",
        b"running ", // `running N tests`
        b"Doc-tests ",
        b"failures:",
        b"---- ", // `---- <test> stdout ----`
        b"thread '",
        b"error",
        b"warning",
    ];
    if PREFIX.iter().any(|p| line.starts_with(p)) {
        return true;
    }
    const SUB: [&[u8]; 3] = [b"... ok", b"... FAILED", b"panicked"];
    SUB.iter().any(|n| contains(line, n))
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() {
        return true;
    }
    hay.len() >= needle.len() && hay.windows(needle.len()).any(|w| w == needle)
}

/// Partition into tiling byte-range blocks. Sum of ranges == [0, len).
pub fn split_blocks(bytes: &[u8], ct: ContentType) -> Vec<(usize, usize)> {
    let lines = split_lines(bytes);
    if lines.is_empty() {
        return Vec::new();
    }
    match ct {
        // Code: close a block after each blank line (the separator stays attached), so
        // one function/section = one block and editing it moves only that block's hash.
        ContentType::Code => {
            let mut blocks = Vec::new();
            let mut bstart: Option<usize> = None;
            let mut last_end = 0usize;
            for &(s, e) in &lines {
                if bstart.is_none() {
                    bstart = Some(s);
                }
                last_end = e;
                if line_is_blank(bytes, s, e) {
                    blocks.push((bstart.take().unwrap(), e));
                }
            }
            if let Some(bs) = bstart {
                blocks.push((bs, last_end));
            }
            if blocks.is_empty() {
                blocks.push((0, bytes.len()));
            }
            blocks
        }
        // Log/test output: CONTENT-DEFINED line groups. A new block opens at a stable
        // semantic anchor (`is_log_anchor`) or right after a blank line; a dense run with
        // no anchor is capped at LOG_CHUNK lines. Boundaries follow content, so a header
        // line inserted/removed at the top shifts only its own block — unchanged
        // test-output blocks keep identical bytes and keep deduping.
        ContentType::Log => {
            let mut blocks = Vec::new();
            let mut bstart: Option<usize> = None;
            let mut in_block = 0usize;
            let mut prev_blank = false;
            let mut last_end = 0usize;
            for &(s, e) in &lines {
                let blank = line_is_blank(bytes, s, e);
                let start_new = bstart.is_some()
                    && (blank || prev_blank || in_block >= LOG_CHUNK || is_log_anchor(bytes, s, e));
                if start_new {
                    blocks.push((bstart.take().unwrap(), s));
                    in_block = 0;
                }
                if bstart.is_none() {
                    bstart = Some(s);
                }
                in_block += 1;
                last_end = e;
                prev_blank = blank;
            }
            if let Some(bs) = bstart {
                blocks.push((bs, last_end));
            }
            if blocks.is_empty() {
                blocks.push((0, bytes.len()));
            }
            blocks
        }
    }
}

pub fn count_lines(b: &[u8]) -> usize {
    split_lines(b).len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::content_type::ContentType;

    // Same test output, B with one extra build-header line ("Compiling …") at the top —
    // the exact shape that made dogfood run #3 fall back cold under fixed offset chunks.
    const LOG_A: &[u8] =
        b"    Finished `test` profile\n     Running tests/foo.rs\ntest a ... ok\ntest b ... ok\ntest c ... ok\n";
    const LOG_B: &[u8] =
        b"   Compiling crate v0.1.0\n    Finished `test` profile\n     Running tests/foo.rs\ntest a ... ok\ntest b ... ok\ntest c ... ok\n";

    fn assert_tiles(bytes: &[u8], blocks: &[(usize, usize)]) {
        assert_eq!(blocks.first().map(|b| b.0), Some(0), "first block must start at 0");
        assert_eq!(blocks.last().map(|b| b.1), Some(bytes.len()), "last block must end at len");
        for w in blocks.windows(2) {
            assert_eq!(w[0].1, w[1].0, "blocks must tile with no gap or overlap");
        }
    }

    #[test]
    fn log_blocks_tile_exactly() {
        for input in [LOG_A, LOG_B, b"" as &[u8], b"no trailing newline", b"\n\n\n"] {
            let blocks = split_blocks(input, ContentType::Log);
            if input.is_empty() {
                assert!(blocks.is_empty());
            } else {
                assert_tiles(input, &blocks);
            }
        }
    }

    #[test]
    fn inserted_header_keeps_later_blocks_identical() {
        let a = split_blocks(LOG_A, ContentType::Log);
        let b = split_blocks(LOG_B, ContentType::Log);
        let b_slices: Vec<&[u8]> = b.iter().map(|&(s, e)| &LOG_B[s..e]).collect();
        // Every block of A must survive byte-for-byte as a block of B, so it still dedups
        // after a header line is inserted (fixed offset chunks would shift them all).
        for &(s, e) in &a {
            let slice = &LOG_A[s..e];
            assert!(
                b_slices.contains(&slice),
                "block {:?} from A should survive header insertion",
                String::from_utf8_lossy(slice)
            );
        }
        assert_eq!(b.len(), a.len() + 1, "B differs from A only by the inserted header block");
    }

    #[test]
    fn each_test_line_is_its_own_block() {
        let texts: Vec<String> = split_blocks(LOG_A, ContentType::Log)
            .iter()
            .map(|&(s, e)| String::from_utf8_lossy(&LOG_A[s..e]).into_owned())
            .collect();
        for want in ["test a ... ok\n", "test b ... ok\n", "test c ... ok\n"] {
            assert!(texts.iter().any(|t| t == want), "missing per-test block {:?}", want);
        }
    }

    #[test]
    fn dense_unanchored_run_is_bounded() {
        // No anchors, no blanks -> still capped at LOG_CHUNK lines per block.
        let mut s = String::new();
        for i in 0..20 {
            s.push_str(&format!("plain data line {}\n", i));
        }
        let blocks = split_blocks(s.as_bytes(), ContentType::Log);
        assert_tiles(s.as_bytes(), &blocks);
        for &(bs, be) in &blocks {
            assert!(count_lines(&s.as_bytes()[bs..be]) <= LOG_CHUNK, "blocks stay bounded");
        }
    }
}
