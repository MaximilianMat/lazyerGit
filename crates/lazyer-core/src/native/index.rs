//! The git index file (`DIRC`), versions 2, 3 and 4.
//!
//! Own implementation rather than a library because the engine's speed rests
//! on what it can do with the index: read it in a few milliseconds, check or
//! replace a handful of entries without touching the rest, keep the
//! cache-tree extension valid so trees never have to be recomputed from
//! scratch, and write it back in whatever version it came in.

use anyhow::{bail, Context, Result};
use std::path::Path;

pub const MODE_FILE: u32 = 0o100644;
pub const MODE_EXEC: u32 = 0o100755;
pub const MODE_LINK: u32 = 0o120000;
pub const MODE_GITLINK: u32 = 0o160000;

const FLAG_EXTENDED: u16 = 0x4000;
const FLAG_STAGE_MASK: u16 = 0x3000;
const FLAG_NAME_MASK: u16 = 0x0fff;
/// Extended flags: skip-worktree and intent-to-add.
pub const XFLAG_SKIP_WORKTREE: u16 = 0x4000;
pub const XFLAG_INTENT_TO_ADD: u16 = 0x2000;

/// File metadata as the index stores it. Equal stat means "unchanged since
/// the object id was recorded".
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stat {
    pub ctime: u32,
    pub ctime_ns: u32,
    pub mtime: u32,
    pub mtime_ns: u32,
    pub dev: u32,
    pub ino: u32,
    pub uid: u32,
    pub gid: u32,
    pub size: u32,
}

impl Stat {
    pub fn from_metadata(m: &std::fs::Metadata) -> Stat {
        use std::os::unix::fs::MetadataExt;
        Stat {
            ctime: m.ctime() as u32,
            ctime_ns: m.ctime_nsec() as u32,
            mtime: m.mtime() as u32,
            mtime_ns: m.mtime_nsec() as u32,
            dev: m.dev() as u32,
            ino: m.ino() as u32,
            uid: m.uid(),
            gid: m.gid(),
            size: m.size() as u32,
        }
    }
}

impl Stat {
    /// Whether a file with metadata `fs` is unchanged since this stat was
    /// recorded, by git's rules: the device is not compared, and nanoseconds
    /// only when the index has them (not every git build writes them).
    pub fn matches(&self, fs: &Stat) -> bool {
        self.mtime == fs.mtime
            && self.ctime == fs.ctime
            && (self.mtime_ns == 0 || self.mtime_ns == fs.mtime_ns)
            && (self.ctime_ns == 0 || self.ctime_ns == fs.ctime_ns)
            && self.ino == fs.ino
            && self.uid == fs.uid
            && self.gid == fs.gid
            && self.size == fs.size
    }

    /// An entry as new as the index file itself may have been modified again
    /// without a visible change of its stat data.
    pub fn racy(&self, index_mtime: (u32, u32)) -> bool {
        if self.mtime_ns == 0 {
            self.mtime >= index_mtime.0
        } else {
            (self.mtime, self.mtime_ns) >= index_mtime
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub stat: Stat,
    pub mode: u32,
    pub oid: [u8; 20],
    /// Assume-valid and stage bits; name length and the extended bit are
    /// recomputed on write.
    pub flags: u16,
    pub xflags: u16,
    pub path: Box<[u8]>,
}

impl Entry {
    pub fn stage(&self) -> u8 {
        ((self.flags & FLAG_STAGE_MASK) >> 12) as u8
    }
}

/// One node of the cache-tree (`TREE` extension): the tree id for a directory
/// as long as nothing below it changed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CacheTree {
    pub name: Vec<u8>,
    /// Number of index entries covered, or -1 when invalid.
    pub entry_count: i32,
    pub oid: [u8; 20],
    pub children: Vec<CacheTree>,
}

impl CacheTree {
    fn parse(data: &[u8], pos: &mut usize) -> Result<CacheTree> {
        let nul = data[*pos..].iter().position(|b| *b == 0).context("cache-tree: path")? + *pos;
        let name = data[*pos..nul].to_vec();
        *pos = nul + 1;
        let sp = data[*pos..].iter().position(|b| *b == b' ').context("cache-tree: count")? + *pos;
        let entry_count: i32 = std::str::from_utf8(&data[*pos..sp])?.parse()?;
        *pos = sp + 1;
        let nl = data[*pos..].iter().position(|b| *b == b'\n').context("cache-tree: subtrees")? + *pos;
        let subtrees: usize = std::str::from_utf8(&data[*pos..nl])?.parse()?;
        *pos = nl + 1;
        let mut oid = [0u8; 20];
        if entry_count >= 0 {
            oid.copy_from_slice(data.get(*pos..*pos + 20).context("cache-tree: id")?);
            *pos += 20;
        }
        let mut children = Vec::with_capacity(subtrees);
        for _ in 0..subtrees {
            children.push(CacheTree::parse(data, pos)?);
        }
        Ok(CacheTree { name, entry_count, oid, children })
    }

    fn serialize(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.name);
        out.push(0);
        out.extend_from_slice(format!("{} {}\n", self.entry_count, self.children.len()).as_bytes());
        if self.entry_count >= 0 {
            out.extend_from_slice(&self.oid);
        }
        for c in &self.children {
            c.serialize(out);
        }
    }

    /// Mark the directory containing `path` and all its ancestors invalid.
    pub fn invalidate(&mut self, path: &[u8]) {
        self.entry_count = -1;
        if let Some(slash) = path.iter().position(|b| *b == b'/') {
            let (dir, rest) = (&path[..slash], &path[slash + 1..]);
            if let Some(child) = self.children.iter_mut().find(|c| c.name == dir) {
                child.invalidate(rest);
            }
        }
    }
}

#[derive(Clone, Debug)]
pub struct Index {
    pub version: u32,
    /// Sorted by path, then stage.
    pub entries: Vec<Entry>,
    pub cache_tree: Option<CacheTree>,
    /// Extensions carried over unchanged (`REUC`).
    pub other_extensions: Vec<([u8; 4], Vec<u8>)>,
    /// The file ended with a real checksum rather than zeros (`index.skipHash`).
    pub had_checksum: bool,
    /// Extensions that describe the entry list positionally or cache
    /// directory state (`UNTR`, `FSMN`, `EOIE`, `IEOT`). They are kept only
    /// while the set of entries is unchanged; git rebuilds them on demand.
    positional_extensions: Vec<([u8; 4], Vec<u8>)>,
}

fn be32(d: &[u8], at: usize) -> u32 {
    u32::from_be_bytes([d[at], d[at + 1], d[at + 2], d[at + 3]])
}

fn be16(d: &[u8], at: usize) -> u16 {
    u16::from_be_bytes([d[at], d[at + 1]])
}

impl Index {
    pub fn read(path: &Path) -> Result<Index> {
        let data = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        Index::parse(&data)
    }

    pub fn parse(d: &[u8]) -> Result<Index> {
        if d.len() < 12 + 20 || &d[..4] != b"DIRC" {
            bail!("not an index file");
        }
        let version = be32(d, 4);
        if !(2..=4).contains(&version) {
            bail!("index version {version}");
        }
        let count = be32(d, 8) as usize;
        let body_end = d.len() - 20;
        let mut entries = Vec::with_capacity(count);
        let mut pos = 12;
        let mut prev: Vec<u8> = Vec::new();
        for _ in 0..count {
            if pos + 62 > body_end {
                bail!("truncated index");
            }
            let start = pos;
            let stat = Stat {
                ctime: be32(d, pos),
                ctime_ns: be32(d, pos + 4),
                mtime: be32(d, pos + 8),
                mtime_ns: be32(d, pos + 12),
                dev: be32(d, pos + 16),
                ino: be32(d, pos + 20),
                uid: be32(d, pos + 28),
                gid: be32(d, pos + 32),
                size: be32(d, pos + 36),
            };
            let mode = be32(d, pos + 24);
            let mut oid = [0u8; 20];
            oid.copy_from_slice(&d[pos + 40..pos + 60]);
            let flags = be16(d, pos + 60);
            pos += 62;
            let mut xflags = 0;
            if flags & FLAG_EXTENDED != 0 {
                if version < 3 {
                    bail!("extended flags in a version 2 index");
                }
                xflags = be16(d, pos);
                pos += 2;
            }
            let path: Box<[u8]> = if version == 4 {
                // Number of bytes to drop from the previous path, as an
                // offset-encoded varint, then the new suffix.
                let mut c = d[pos];
                pos += 1;
                let mut strip = (c & 0x7f) as usize;
                while c & 0x80 != 0 {
                    c = d[pos];
                    pos += 1;
                    strip = ((strip + 1) << 7) | (c & 0x7f) as usize;
                }
                let nul = d[pos..body_end].iter().position(|b| *b == 0).context("index path")? + pos;
                if strip > prev.len() {
                    bail!("corrupt path compression");
                }
                prev.truncate(prev.len() - strip);
                prev.extend_from_slice(&d[pos..nul]);
                pos = nul + 1;
                prev.clone().into_boxed_slice()
            } else {
                let name_len = (flags & FLAG_NAME_MASK) as usize;
                let nul = if name_len < FLAG_NAME_MASK as usize {
                    pos + name_len
                } else {
                    d[pos..body_end].iter().position(|b| *b == 0).context("index path")? + pos
                };
                let path = d.get(pos..nul).context("index path")?.to_vec().into_boxed_slice();
                // Entries are padded with 1 to 8 NULs to a multiple of 8 bytes.
                pos = start + ((nul - start + 8) & !7);
                path
            };
            entries.push(Entry { stat, mode, oid, flags: flags & !(FLAG_NAME_MASK | FLAG_EXTENDED), xflags, path });
        }

        let mut cache_tree = None;
        let mut other_extensions = Vec::new();
        let mut positional_extensions = Vec::new();
        while pos + 8 <= body_end {
            let mut sig = [0u8; 4];
            sig.copy_from_slice(&d[pos..pos + 4]);
            let len = be32(d, pos + 4) as usize;
            let data = d.get(pos + 8..pos + 8 + len).context("index extension")?;
            pos += 8 + len;
            match &sig {
                b"TREE" => {
                    let mut p = 0;
                    cache_tree = Some(CacheTree::parse(data, &mut p)?);
                }
                b"UNTR" | b"FSMN" | b"EOIE" | b"IEOT" => positional_extensions.push((sig, data.to_vec())),
                b"REUC" => other_extensions.push((sig, data.to_vec())),
                // Split index and sparse directories change what the entry
                // list means; other lowercase extensions are mandatory too.
                _ if sig[0].is_ascii_lowercase() => return Err(crate::api::Unsupported(format!("index extension {}", String::from_utf8_lossy(&sig))).into()),
                _ => {}
            }
        }
        let had_checksum = d[body_end..].iter().any(|b| *b != 0);
        Ok(Index { version, entries, cache_tree, other_extensions, had_checksum, positional_extensions })
    }

    /// Position of the stage-0 entry for `path`, or where it would be inserted.
    pub fn find(&self, path: &[u8]) -> std::result::Result<usize, usize> {
        self.entries.binary_search_by(|e| (&*e.path, e.stage()).cmp(&(path, 0)))
    }

    pub fn has_conflicts(&self) -> bool {
        self.entries.iter().any(|e| e.stage() != 0)
    }

    /// Call after adding or removing entries: positional extensions no
    /// longer describe the entry list.
    pub fn entries_changed(&mut self) {
        self.positional_extensions.clear();
    }

    /// Serialize without the trailing checksum; the caller appends either a
    /// hash of these bytes or 20 zero bytes.
    pub fn serialize(&self) -> Vec<u8> {
        let needs_v3 = self.entries.iter().any(|e| e.xflags != 0);
        let version: u32 = if self.version == 4 {
            4
        } else if needs_v3 {
            3
        } else {
            2
        };
        let mut out = Vec::with_capacity(self.entries.len() * 110 + 1024);
        out.extend_from_slice(b"DIRC");
        out.extend_from_slice(&version.to_be_bytes());
        out.extend_from_slice(&(self.entries.len() as u32).to_be_bytes());
        let mut prev: &[u8] = &[];
        for e in &self.entries {
            let start = out.len();
            let s = &e.stat;
            for v in [s.ctime, s.ctime_ns, s.mtime, s.mtime_ns, s.dev, s.ino, e.mode, s.uid, s.gid, s.size] {
                out.extend_from_slice(&v.to_be_bytes());
            }
            out.extend_from_slice(&e.oid);
            let mut flags = e.flags | (e.path.len().min(FLAG_NAME_MASK as usize) as u16);
            if e.xflags != 0 {
                flags |= FLAG_EXTENDED;
            }
            out.extend_from_slice(&flags.to_be_bytes());
            if e.xflags != 0 {
                out.extend_from_slice(&e.xflags.to_be_bytes());
            }
            if version == 4 {
                let common = prev.iter().zip(e.path.iter()).take_while(|(a, b)| a == b).count();
                let mut strip = prev.len() - common;
                // Offset-encoded varint, most significant group first.
                let mut buf = [0u8; 10];
                let mut i = buf.len() - 1;
                buf[i] = (strip & 0x7f) as u8;
                strip >>= 7;
                while strip != 0 {
                    strip -= 1;
                    i -= 1;
                    buf[i] = 0x80 | (strip & 0x7f) as u8;
                    strip >>= 7;
                }
                out.extend_from_slice(&buf[i..]);
                out.extend_from_slice(&e.path[common..]);
                out.push(0);
                prev = &e.path;
            } else {
                out.extend_from_slice(&e.path);
                let len = out.len() - start;
                let padded = (len + 8) & !7;
                out.resize(start + padded, 0);
            }
        }
        let mut ext = |sig: &[u8; 4], data: &[u8]| {
            out.extend_from_slice(sig);
            out.extend_from_slice(&(data.len() as u32).to_be_bytes());
            out.extend_from_slice(data);
        };
        if let Some(tree) = &self.cache_tree {
            let mut data = Vec::new();
            tree.serialize(&mut data);
            ext(b"TREE", &data);
        }
        for (sig, data) in self.other_extensions.iter().chain(&self.positional_extensions) {
            // The end-of-index and offset-table extensions hold byte offsets
            // into this file and cannot be carried over.
            if sig != b"EOIE" && sig != b"IEOT" {
                ext(sig, data);
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(path: &str, x: u16) -> Entry {
        Entry { stat: Stat { mtime: 7, size: 3, ..Default::default() }, mode: MODE_FILE, oid: [9; 20], flags: 0, xflags: x, path: path.as_bytes().into() }
    }

    fn round_trip(version: u32, paths: &[&str], x: u16) {
        let tree = CacheTree {
            name: vec![],
            entry_count: 2,
            oid: [1; 20],
            children: vec![CacheTree { name: b"a".to_vec(), entry_count: -1, oid: [0; 20], children: vec![] }],
        };
        let idx = Index {
            version,
            entries: paths.iter().map(|p| entry(p, x)).collect(),
            cache_tree: Some(tree),
            other_extensions: vec![],
            had_checksum: true,
            positional_extensions: vec![],
        };
        let mut bytes = idx.serialize();
        bytes.extend_from_slice(&[0xAA; 20]);
        let back = Index::parse(&bytes).unwrap();
        assert_eq!(back.entries, idx.entries);
        assert_eq!(back.cache_tree, idx.cache_tree);
        assert!(back.had_checksum);
    }

    #[test]
    fn round_trips_all_versions() {
        let long = "d/".repeat(2100) + "f";
        let paths = ["a", "a/b", "a/b.c", "abcdefg", "abcdefgh", "dir/sub/file.txt", long.as_str()];
        round_trip(2, &paths, 0);
        round_trip(3, &paths, XFLAG_INTENT_TO_ADD);
        round_trip(4, &paths, 0);
        round_trip(4, &paths, XFLAG_SKIP_WORKTREE);
    }

    #[test]
    fn invalidates_ancestors_only() {
        let leaf = |n: &str| CacheTree { name: n.as_bytes().to_vec(), entry_count: 1, oid: [1; 20], children: vec![] };
        let mut t = CacheTree { name: vec![], entry_count: 3, oid: [1; 20], children: vec![CacheTree { children: vec![leaf("x")], ..leaf("a") }, leaf("b")] };
        t.invalidate(b"a/file");
        assert_eq!(t.entry_count, -1);
        assert_eq!(t.children[0].entry_count, -1);
        assert_eq!(t.children[0].children[0].entry_count, 1);
        assert_eq!(t.children[1].entry_count, 1);
    }
}
