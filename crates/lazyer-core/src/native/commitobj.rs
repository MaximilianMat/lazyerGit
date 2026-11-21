//! Commit objects as raw bytes.
//!
//! Rewriting history must keep everything it does not mean to change exactly
//! as it was (author line, encoding header, message bytes), otherwise the new
//! commit ids differ from what git produces. So commits are split into their
//! header lines without interpreting them and put back together verbatim.

use crate::api::Ident;
use anyhow::{bail, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawCommit {
    /// Hex id of the tree.
    pub tree: String,
    /// Hex ids of the parents.
    pub parents: Vec<String>,
    /// Everything after `author `.
    pub author: Vec<u8>,
    /// Everything after `committer `.
    pub committer: Vec<u8>,
    /// Remaining headers in their original order, name and raw value
    /// (continuation lines included, without the final newline).
    pub extra: Vec<(Vec<u8>, Vec<u8>)>,
    /// Everything after the blank line.
    pub message: Vec<u8>,
}

impl RawCommit {
    pub fn parse(data: &[u8]) -> Result<RawCommit> {
        let (mut tree, mut author, mut committer) = (None, None, None);
        let mut parents = Vec::new();
        let mut extra: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
        let mut pos = 0;
        loop {
            if pos >= data.len() {
                // No blank line: a commit without message.
                break;
            }
            let end = data[pos..].iter().position(|b| *b == b'\n').map(|i| pos + i).unwrap_or(data.len());
            let line = &data[pos..end];
            let next = (end + 1).min(data.len());
            if line.is_empty() {
                pos = next;
                break;
            }
            if line[0] == b' ' {
                // Continuation of the previous header.
                match extra.last_mut() {
                    Some((_, value)) => {
                        value.push(b'\n');
                        value.extend_from_slice(line);
                    }
                    None => bail!("commit header starts with a continuation line"),
                }
                pos = next;
                continue;
            }
            let sp = line.iter().position(|b| *b == b' ').unwrap_or(line.len());
            let (name, value) = (&line[..sp], &line[(sp + 1).min(line.len())..]);
            match name {
                b"tree" if tree.is_none() => tree = Some(String::from_utf8_lossy(value).to_string()),
                b"parent" if author.is_none() => parents.push(String::from_utf8_lossy(value).to_string()),
                b"author" if author.is_none() => author = Some(value.to_vec()),
                b"committer" if committer.is_none() => committer = Some(value.to_vec()),
                _ => extra.push((name.to_vec(), value.to_vec())),
            }
            pos = next;
        }
        let (Some(tree), Some(author), Some(committer)) = (tree, author, committer) else {
            bail!("commit object lacks tree, author or committer");
        };
        Ok(RawCommit { tree, parents, author, committer, extra, message: data[pos..].to_vec() })
    }

    pub fn serialize(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(256 + self.message.len());
        out.extend_from_slice(b"tree ");
        out.extend_from_slice(self.tree.as_bytes());
        out.push(b'\n');
        for p in &self.parents {
            out.extend_from_slice(b"parent ");
            out.extend_from_slice(p.as_bytes());
            out.push(b'\n');
        }
        out.extend_from_slice(b"author ");
        out.extend_from_slice(&self.author);
        out.extend_from_slice(b"\ncommitter ");
        out.extend_from_slice(&self.committer);
        out.push(b'\n');
        for (name, value) in &self.extra {
            out.extend_from_slice(name);
            out.push(b' ');
            out.extend_from_slice(value);
            out.push(b'\n');
        }
        out.push(b'\n');
        out.extend_from_slice(&self.message);
        out
    }

    /// A signed commit cannot be rewritten without dropping or redoing the
    /// signature; that decision is git's.
    pub fn is_signed(&self) -> bool {
        self.extra.iter().any(|(n, _)| n == b"gpgsig" || n == b"gpgsig-sha256")
    }

    /// First line of the message, as git puts it into reflog entries.
    pub fn subject(&self) -> String {
        let end = self.message.iter().position(|b| *b == b'\n').unwrap_or(self.message.len());
        String::from_utf8_lossy(&self.message[..end]).to_string()
    }
}

/// `Name <email> seconds zone`, the value of an author or committer header.
pub fn ident_line(ident: &Ident) -> Vec<u8> {
    format!("{} <{}> {} {}", ident.name, ident.email, ident.time, ident.tz).into_bytes()
}

/// Git's default clean-up for a message given with `-m`: trailing whitespace
/// removed from every line, runs of blank lines collapsed, no blank lines at
/// the start or end, and a final newline.
pub fn cleanup_message(message: &str) -> String {
    let mut out = String::with_capacity(message.len() + 1);
    let mut pending_blank = false;
    for line in message.split('\n') {
        let line = line.trim_end_matches([' ', '\t', '\r', '\x0b', '\x0c']);
        if line.is_empty() {
            pending_blank = !out.is_empty();
            continue;
        }
        if pending_blank {
            out.push('\n');
            pending_blank = false;
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_headers_verbatim() {
        let raw = b"tree 4b825dc642cb6eb9a060e54bf8d69288fbee4904\nparent 1111111111111111111111111111111111111111\nparent 2222222222222222222222222222222222222222\nauthor A U Thor <a@example.com> 1700000000 +0100\ncommitter C O Mitter <c@example.com> 1700000001 -0500\nencoding ISO-8859-1\ngpgsig -----BEGIN PGP SIGNATURE-----\n \n abc\n -----END PGP SIGNATURE-----\n\nSubject line\n\nBody\n";
        let c = RawCommit::parse(raw).unwrap();
        assert_eq!(c.parents.len(), 2);
        assert_eq!(c.author, b"A U Thor <a@example.com> 1700000000 +0100");
        assert!(c.is_signed());
        assert_eq!(c.subject(), "Subject line");
        assert_eq!(c.serialize(), raw);
    }

    #[test]
    fn message_without_trailing_newline_and_empty_message() {
        let raw = b"tree 4b825dc642cb6eb9a060e54bf8d69288fbee4904\nauthor A <a@b> 1 +0000\ncommitter A <a@b> 1 +0000\n\nno newline";
        let c = RawCommit::parse(raw).unwrap();
        assert_eq!(c.message, b"no newline");
        assert_eq!(c.serialize(), raw);
        let raw = b"tree 4b825dc642cb6eb9a060e54bf8d69288fbee4904\nauthor A <a@b> 1 +0000\ncommitter A <a@b> 1 +0000\n\n";
        assert_eq!(RawCommit::parse(raw).unwrap().serialize(), raw);
    }

    #[test]
    fn cleans_messages_like_git() {
        assert_eq!(cleanup_message("subject"), "subject\n");
        assert_eq!(cleanup_message("\n\n  subject  \n\n\n\nbody \t\n\n"), "  subject\n\nbody\n");
        assert_eq!(cleanup_message(" \n\t\n"), "");
    }
}
