//! Unified-diff patches, as lazygit builds them when single hunks or lines
//! are staged or moved out of a commit, applied directly to blobs.
//!
//! Application is strict: a hunk goes where its header says, or, failing
//! that, at the one place in the file where its lines occur. Whenever git
//! would have to choose or approximate, the patch is declined.

use crate::api::{unsupported, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hunk {
    pub old_start: usize,
    pub old_count: usize,
    pub new_start: usize,
    pub new_count: usize,
    /// Tag (` `, `+`, `-`) and the line including its newline, unless the
    /// patch says the line has none.
    pub lines: Vec<(u8, Vec<u8>)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilePatch {
    pub path: String,
    /// The patch creates the file (`new file mode`).
    pub creates: bool,
    /// The patch deletes the file (`deleted file mode`).
    pub deletes: bool,
    pub hunks: Vec<Hunk>,
}

fn strip_prefix_path(raw: &[u8], prefix: &[u8]) -> Result<Option<String>> {
    let raw = raw.strip_suffix(b"\t").unwrap_or(raw);
    if raw == b"/dev/null" {
        return Ok(None);
    }
    if raw.first() == Some(&b'"') {
        return unsupported("quoted path in patch");
    }
    match raw.strip_prefix(prefix) {
        Some(p) => Ok(Some(String::from_utf8(p.to_vec())?)),
        None => unsupported("patch path without a/ b/ prefix"),
    }
}

fn range(spec: &str) -> Result<(usize, usize)> {
    let (start, count) = match spec.split_once(',') {
        Some((s, c)) => (s, c),
        None => (spec, "1"),
    };
    Ok((start.parse()?, count.parse()?))
}

pub fn parse(text: &[u8]) -> Result<Vec<FilePatch>> {
    let lines: Vec<&[u8]> = text.split_inclusive(|b| *b == b'\n').collect();
    let body = |l: &[u8]| -> Vec<u8> { l.strip_suffix(b"\n").unwrap_or(l).to_vec() };
    let mut out: Vec<FilePatch> = Vec::new();
    let mut i = 0;
    let (mut creates, mut deletes) = (false, false);
    let (mut old_path, mut new_path): (Option<Option<String>>, Option<Option<String>>) = (None, None);
    while i < lines.len() {
        let line = body(lines[i]);
        if line.starts_with(b"diff --git ") {
            (creates, deletes, old_path, new_path) = (false, false, None, None);
        } else if line.starts_with(b"new file mode ") {
            creates = true;
        } else if line.starts_with(b"deleted file mode ") {
            deletes = true;
        } else if line.starts_with(b"old mode ")
            || line.starts_with(b"new mode ")
            || line.starts_with(b"rename ")
            || line.starts_with(b"copy ")
            || line.starts_with(b"similarity ")
        {
            return unsupported("mode change or rename in patch");
        } else if line.starts_with(b"Binary files ") || line.starts_with(b"GIT binary patch") {
            return unsupported("binary patch");
        } else if let Some(p) = line.strip_prefix(b"--- ") {
            old_path = Some(strip_prefix_path(p, b"a/")?);
        } else if let Some(p) = line.strip_prefix(b"+++ ") {
            new_path = Some(strip_prefix_path(p, b"b/")?);
        } else if line.starts_with(b"@@ ") {
            let (Some(old), Some(new)) = (&old_path, &new_path) else { return unsupported("hunk without file header") };
            let Some(path) = new.clone().or_else(|| old.clone()) else { return unsupported("patch without a path") };
            let header = String::from_utf8_lossy(&line).to_string();
            let mut parts = header.split(' ');
            let (_, Some(o), Some(n)) = (parts.next(), parts.next(), parts.next()) else { return unsupported("hunk header") };
            let (Some(o), Some(n)) = (o.strip_prefix('-'), n.strip_prefix('+')) else { return unsupported("hunk header") };
            let ((old_start, old_count), (new_start, new_count)) = (range(o)?, range(n)?);
            let mut hunk = Hunk { old_start, old_count, new_start, new_count, lines: Vec::new() };
            let (mut need_old, mut need_new) = (old_count, new_count);
            i += 1;
            while i < lines.len() && (need_old > 0 || need_new > 0) {
                let raw = lines[i];
                let (tag, content) = match raw.first() {
                    Some(b' ') | Some(b'+') | Some(b'-') => (raw[0], raw[1..].to_vec()),
                    // Some tools strip the single space of an empty context line.
                    Some(b'\n') => (b' ', raw.to_vec()),
                    _ => return unsupported("unexpected line inside a hunk"),
                };
                if tag != b'+' {
                    need_old = need_old.checked_sub(1).ok_or_else(|| crate::api::Unsupported("hunk longer than its header".into()))?;
                }
                if tag != b'-' {
                    need_new = need_new.checked_sub(1).ok_or_else(|| crate::api::Unsupported("hunk longer than its header".into()))?;
                }
                hunk.lines.push((tag, content));
                i += 1;
                // "\ No newline at end of file" belongs to the line before it.
                if i < lines.len() && lines[i].starts_with(b"\\") {
                    if let Some((_, last)) = hunk.lines.last_mut() {
                        if last.last() == Some(&b'\n') {
                            last.pop();
                        }
                    }
                    i += 1;
                }
            }
            if need_old > 0 || need_new > 0 {
                return unsupported("truncated hunk");
            }
            match out.last_mut() {
                Some(f) if f.path == path => f.hunks.push(hunk),
                _ => out.push(FilePatch { path, creates, deletes, hunks: vec![hunk] }),
            }
            continue;
        }
        i += 1;
    }
    if out.is_empty() {
        return unsupported("patch without hunks");
    }
    Ok(out)
}

/// Apply hunks to `base`. With `reverse`, undo them instead.
pub fn apply(base: &[u8], hunks: &[Hunk], reverse: bool) -> Result<Vec<u8>> {
    let lines: Vec<&[u8]> = base.split_inclusive(|b| *b == b'\n').collect();
    let (gone, added) = if reverse { (b'+', b'-') } else { (b'-', b'+') };
    let mut out: Vec<u8> = Vec::with_capacity(base.len() + 256);
    let mut cursor = 0;
    for h in hunks {
        let before: Vec<&[u8]> = h.lines.iter().filter(|(t, _)| *t == b' ' || *t == gone).map(|(_, l)| l.as_slice()).collect();
        let (start, count) = if reverse { (h.new_start, h.new_count) } else { (h.old_start, h.old_count) };
        if count != before.len() {
            return unsupported("hunk does not match its header");
        }
        // A hunk without lines on this side is an insertion after `start`.
        let hint = if before.is_empty() { start } else { start.saturating_sub(1) };
        let fits = |at: usize| at >= cursor && at + before.len() <= lines.len() && lines[at..at + before.len()] == before[..];
        let at = if fits(hint) {
            hint
        } else if before.is_empty() {
            return unsupported("insertion outside the file");
        } else {
            let mut found = (cursor..=lines.len().saturating_sub(before.len())).filter(|at| fits(*at));
            match (found.next(), found.next()) {
                (Some(at), None) => at,
                _ => return unsupported("hunk does not apply at one definite place"),
            }
        };
        for l in &lines[cursor..at] {
            out.extend_from_slice(l);
        }
        for (_, l) in h.lines.iter().filter(|(t, _)| *t == b' ' || *t == added) {
            out.extend_from_slice(l);
        }
        cursor = at + before.len();
    }
    for l in &lines[cursor..] {
        out.extend_from_slice(l);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &str = "one\ntwo\nthree\nfour\nfive\nsix\nseven\neight\nnine\nten\n";
    const PATCH: &str = "diff --git a/dir/f.txt b/dir/f.txt\nindex 1111111..2222222 100644\n--- a/dir/f.txt\n+++ b/dir/f.txt\n@@ -1,3 +1,3 @@\n-one\n+ONE\n two\n three\n@@ -8,3 +8,4 @@ seven\n eight\n nine\n-ten\n+TEN\n+eleven\n";

    #[test]
    fn applies_and_reverses() {
        let files = parse(PATCH.as_bytes()).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, "dir/f.txt");
        assert_eq!(files[0].hunks.len(), 2);
        let applied = apply(BASE.as_bytes(), &files[0].hunks, false).unwrap();
        assert_eq!(String::from_utf8(applied.clone()).unwrap(), "ONE\ntwo\nthree\nfour\nfive\nsix\nseven\neight\nnine\nTEN\neleven\n");
        assert_eq!(apply(&applied, &files[0].hunks, true).unwrap(), BASE.as_bytes());
    }

    #[test]
    fn a_single_hunk_applies_alone_in_both_directions() {
        let files = parse(PATCH.as_bytes()).unwrap();
        let second = &files[0].hunks[1..];
        let applied = apply(BASE.as_bytes(), second, false).unwrap();
        assert_eq!(String::from_utf8(applied.clone()).unwrap(), "one\ntwo\nthree\nfour\nfive\nsix\nseven\neight\nnine\nTEN\neleven\n");
        assert_eq!(apply(&applied, second, true).unwrap(), BASE.as_bytes());
    }

    #[test]
    fn handles_missing_final_newline() {
        let patch = "--- a/f\n+++ b/f\n@@ -1,2 +1,2 @@\n a\n-b\n\\ No newline at end of file\n+c\n\\ No newline at end of file\n";
        let files = parse(patch.as_bytes()).unwrap();
        assert_eq!(apply(b"a\nb", &files[0].hunks, false).unwrap(), b"a\nc");
        assert_eq!(apply(b"a\nc", &files[0].hunks, true).unwrap(), b"a\nb");
    }

    #[test]
    fn declines_what_does_not_apply_exactly() {
        let files = parse(PATCH.as_bytes()).unwrap();
        // Content differs from what the hunk expects.
        assert!(apply(b"uno\ntwo\nthree\n", &files[0].hunks[..1], false).is_err());
        // Shifted, but still found at its only possible place.
        let shifted = format!("zero\n{BASE}");
        assert!(apply(shifted.as_bytes(), &files[0].hunks, false).is_ok());
        // Two possible places.
        let patch = "--- a/f\n+++ b/f\n@@ -9,1 +9,1 @@\n-x\n+y\n";
        let files = parse(patch.as_bytes()).unwrap();
        assert!(apply(b"x\na\nx\n", &files[0].hunks, false).is_err());
        assert!(parse(b"Binary files a/x and b/x differ\n").is_err());
    }
}
