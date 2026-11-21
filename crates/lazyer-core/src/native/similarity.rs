//! Git's similarity estimate for rename detection (`diffcore-delta.c`).
//!
//! Both files are cut into chunks (a line, or 64 bytes at most), every chunk
//! is hashed, and the score is the share of the larger file's bytes that sit
//! in chunks the two files have in common. Reproduced exactly, because
//! whether a staged delete plus add is shown as a rename depends on it.

use std::collections::HashMap;

pub const MAX_SCORE: u64 = 60_000;
/// 50%, git's default and what lazygit asks for.
pub const MIN_SCORE: u64 = 30_000;
const HASH_BASE: u32 = 107_927;

fn is_binary(data: &[u8]) -> bool {
    data[..data.len().min(8000)].contains(&0)
}

/// Bytes per chunk hash.
fn chunks(data: &[u8]) -> HashMap<u32, u32> {
    let is_text = !is_binary(data);
    let mut out: HashMap<u32, u32> = HashMap::new();
    let (mut n, mut accum1, mut accum2) = (0u32, 0u32, 0u32);
    let mut i = 0;
    while i < data.len() {
        let c = data[i] as u32;
        i += 1;
        // A carriage return before a line feed does not count in text.
        if is_text && c == b'\r' as u32 && data.get(i) == Some(&b'\n') {
            continue;
        }
        let old_1 = accum1;
        accum1 = (accum1 << 7) ^ (accum2 >> 25);
        accum2 = (accum2 << 7) ^ (old_1 >> 25);
        accum1 = accum1.wrapping_add(c);
        n += 1;
        if n < 64 && c != b'\n' as u32 {
            continue;
        }
        *out.entry(accum1.wrapping_add(accum2.wrapping_mul(0x61)) % HASH_BASE).or_insert(0) += n;
        (n, accum1, accum2) = (0, 0, 0);
    }
    if n > 0 {
        *out.entry(accum1.wrapping_add(accum2.wrapping_mul(0x61)) % HASH_BASE).or_insert(0) += n;
    }
    out
}

/// How much of `src` survives in `dst`, from 0 to `MAX_SCORE`.
pub fn score(src: &[u8], dst: &[u8]) -> u64 {
    let max = src.len().max(dst.len()) as u64;
    let delta = max - src.len().min(dst.len()) as u64;
    // Too different in size to reach the minimum, whatever the content.
    if dst.is_empty() || max * (MAX_SCORE - MIN_SCORE) < delta * MAX_SCORE {
        return 0;
    }
    let (s, d) = (chunks(src), chunks(dst));
    let copied: u64 = s.iter().map(|(hash, n)| (*n).min(d.get(hash).copied().unwrap_or(0)) as u64).sum();
    copied * MAX_SCORE / max
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_and_unrelated() {
        let a = b"one\ntwo\nthree\nfour\n";
        assert_eq!(score(a, a), MAX_SCORE);
        assert_eq!(score(a, b"completely\ndifferent\ncontent\nhere\n"), 0);
        assert_eq!(score(a, b""), 0);
    }

    #[test]
    fn a_small_edit_keeps_most_of_the_score() {
        let a = b"alpha\nbeta\ngamma\ndelta\nepsilon\nzeta\neta\ntheta\n";
        let b = b"alpha\nbeta\ngamma\ndelta\nepsilon\nzeta\neta\ntheta\nadded\n";
        let s = score(a, b);
        assert!(s > MIN_SCORE && s < MAX_SCORE, "{s}");
    }

    #[test]
    fn size_ratio_rules_out_a_match() {
        let a = b"line\n".repeat(10);
        let b = b"line\n".repeat(30);
        assert_eq!(score(&a, &b), 0);
    }
}
