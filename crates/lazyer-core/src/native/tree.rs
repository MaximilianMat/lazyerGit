//! Tree objects: parsing, building from the index, two-way diff and the
//! three-way merge that replays commits without a working tree.

use super::index::{CacheTree, Index, MODE_GITLINK};
use crate::api::{unsupported, Result};
use anyhow::{bail, Context};
use std::collections::BTreeMap;

pub type Oid = [u8; 20];
pub const MODE_TREE: u32 = 0o40000;
/// The id of the empty tree.
pub const EMPTY_TREE: Oid = [0x4b, 0x82, 0x5d, 0xc6, 0x42, 0xcb, 0x6e, 0xb9, 0xa0, 0x60, 0xe5, 0x4b, 0xf8, 0xd6, 0x92, 0x88, 0xfb, 0xee, 0x49, 0x04];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Blob,
    Tree,
    Commit,
    Tag,
}

/// The object database as the engine needs it.
pub trait Objects: Sync {
    fn read(&self, oid: &Oid) -> Result<(Kind, Vec<u8>)>;
    /// Store an object and return its id; storing an existing object is a no-op.
    fn write(&self, kind: Kind, data: &[u8]) -> Result<Oid>;

    fn read_tree(&self, oid: &Oid) -> Result<Vec<TreeEntry>> {
        if *oid == EMPTY_TREE {
            return Ok(Vec::new());
        }
        let (kind, data) = self.read(oid)?;
        if kind != Kind::Tree {
            bail!("{} is not a tree", hex(oid));
        }
        parse_tree(&data)
    }
}

pub fn hex(oid: &Oid) -> String {
    oid.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn unhex(s: &str) -> Result<Oid> {
    let b = s.as_bytes();
    if b.len() != 40 {
        bail!("not an object id: {s}");
    }
    let mut oid = [0u8; 20];
    for (i, o) in oid.iter_mut().enumerate() {
        *o = u8::from_str_radix(std::str::from_utf8(&b[i * 2..i * 2 + 2])?, 16).with_context(|| format!("not an object id: {s}"))?;
    }
    Ok(oid)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TreeEntry {
    pub mode: u32,
    pub name: Vec<u8>,
    pub oid: Oid,
}

pub fn parse_tree(mut d: &[u8]) -> Result<Vec<TreeEntry>> {
    let mut out = Vec::with_capacity(d.len() / 36);
    while !d.is_empty() {
        let sp = d.iter().position(|b| *b == b' ').context("tree entry mode")?;
        let mut mode = 0u32;
        for b in &d[..sp] {
            mode = (mode << 3) | (b.wrapping_sub(b'0') as u32 & 7);
        }
        let nul = d[sp..].iter().position(|b| *b == 0).context("tree entry name")? + sp;
        let mut oid = [0u8; 20];
        oid.copy_from_slice(d.get(nul + 1..nul + 21).context("tree entry id")?);
        out.push(TreeEntry { mode, name: d[sp + 1..nul].to_vec(), oid });
        d = &d[nul + 21..];
    }
    Ok(out)
}

/// Entries must already be in git's tree order.
pub fn serialize_tree(entries: &[TreeEntry]) -> Vec<u8> {
    let mut out = Vec::with_capacity(entries.iter().map(|e| e.name.len() + 28).sum());
    for e in entries {
        out.extend_from_slice(format!("{:o} ", e.mode).as_bytes());
        out.extend_from_slice(&e.name);
        out.push(0);
        out.extend_from_slice(&e.oid);
    }
    out
}

/// Git orders tree entries by name, with directories compared as `name/`.
fn sort_key(e: &TreeEntry) -> Vec<u8> {
    let mut k = e.name.clone();
    if e.mode == MODE_TREE {
        k.push(b'/');
    }
    k
}

type Slot = Option<(u32, Oid)>;

fn by_name(entries: Vec<TreeEntry>) -> BTreeMap<Vec<u8>, (u32, Oid)> {
    entries.into_iter().map(|e| (e.name, (e.mode, e.oid))).collect()
}

fn is_tree(s: &Slot) -> bool {
    matches!(s, Some((MODE_TREE, _)))
}

/// A file-level difference between two trees.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Change {
    pub path: Vec<u8>,
    pub old: Slot,
    pub new: Slot,
}

/// All files that differ between two trees, in path order. Only subtrees
/// that differ are read, and sibling subtrees are compared in parallel.
pub fn diff_trees(odb: &dyn Objects, old: Option<&Oid>, new: Option<&Oid>, out: &mut Vec<Change>) -> Result<()> {
    out.extend(diff_rec(odb, old, new, &[])?);
    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(())
}

/// Entries of a raw tree object without copying names.
fn raw_entries(mut d: &[u8]) -> Result<Vec<(&[u8], u32, Oid)>> {
    let mut out = Vec::with_capacity(d.len() / 36);
    while !d.is_empty() {
        let sp = d.iter().position(|b| *b == b' ').context("tree entry mode")?;
        let mut mode = 0u32;
        for b in &d[..sp] {
            mode = (mode << 3) | (b.wrapping_sub(b'0') as u32 & 7);
        }
        let nul = d[sp..].iter().position(|b| *b == 0).context("tree entry name")? + sp;
        let mut oid = [0u8; 20];
        oid.copy_from_slice(d.get(nul + 1..nul + 21).context("tree entry id")?);
        out.push((&d[sp + 1..nul], mode, oid));
        d = &d[nul + 21..];
    }
    Ok(out)
}

fn read_raw(odb: &dyn Objects, tree: Option<&Oid>) -> Result<Vec<u8>> {
    match tree {
        Some(o) if *o != EMPTY_TREE => {
            let (kind, data) = odb.read(o)?;
            if kind != Kind::Tree {
                bail!("{} is not a tree", hex(o));
            }
            Ok(data)
        }
        _ => Ok(Vec::new()),
    }
}

fn diff_rec(odb: &dyn Objects, old: Option<&Oid>, new: Option<&Oid>, prefix: &[u8]) -> Result<Vec<Change>> {
    use rayon::prelude::*;
    if old == new {
        return Ok(Vec::new());
    }
    let (da, db) = (read_raw(odb, old)?, read_raw(odb, new)?);
    let (a, b) = (raw_entries(&da)?, raw_entries(&db)?);
    let in_a: std::collections::HashMap<&[u8], (u32, Oid)> = a.iter().map(|(n, m, o)| (*n, (*m, *o))).collect();
    let in_b: std::collections::HashSet<&[u8]> = b.iter().map(|(n, _, _)| *n).collect();
    // (name, old slot, new slot) for every name that differs.
    let mut differing: Vec<(&[u8], Slot, Slot)> = Vec::new();
    for (name, mode, oid) in &b {
        let x = in_a.get(name).copied();
        if x != Some((*mode, *oid)) {
            differing.push((name, x, Some((*mode, *oid))));
        }
    }
    for (name, mode, oid) in &a {
        if !in_b.contains(name) {
            differing.push((name, Some((*mode, *oid)), None));
        }
    }
    let path_of = |name: &[u8]| {
        let mut p = Vec::with_capacity(prefix.len() + name.len() + 1);
        p.extend_from_slice(prefix);
        p.extend_from_slice(name);
        p
    };
    let mut out = Vec::new();
    let mut subtrees = Vec::new();
    for (name, x, y) in differing {
        let (xt, yt) = (is_tree(&x), is_tree(&y));
        if xt || yt {
            let mut dir = path_of(name);
            dir.push(b'/');
            subtrees.push((dir, x.filter(|_| xt).map(|s| s.1), y.filter(|_| yt).map(|s| s.1)));
        }
        let (xf, yf) = (x.filter(|_| !xt), y.filter(|_| !yt));
        if xf.is_some() || yf.is_some() {
            out.push(Change { path: path_of(name), old: xf, new: yf });
        }
    }
    let nested: Vec<Vec<Change>> = if subtrees.len() > 1 {
        subtrees.par_iter().map(|(dir, x, y)| diff_rec(odb, x.as_ref(), y.as_ref(), dir)).collect::<Result<_>>()?
    } else {
        subtrees.iter().map(|(dir, x, y)| diff_rec(odb, x.as_ref(), y.as_ref(), dir)).collect::<Result<_>>()?
    };
    out.extend(nested.into_iter().flatten());
    Ok(out)
}

/// Look up a path in a tree.
pub fn lookup(odb: &dyn Objects, tree: &Oid, path: &[u8]) -> Result<Slot> {
    let mut current = *tree;
    let mut parts = path.split(|b| *b == b'/').peekable();
    while let Some(part) = parts.next() {
        let Some(e) = odb.read_tree(&current)?.into_iter().find(|e| e.name == part) else { return Ok(None) };
        if parts.peek().is_none() {
            return Ok(Some((e.mode, e.oid)));
        }
        if e.mode != MODE_TREE {
            return Ok(None);
        }
        current = e.oid;
    }
    Ok(None)
}

/// A copy of `tree` with `path` set to `value` (`None` removes it). Empty
/// directories disappear.
pub fn with_path(odb: &dyn Objects, tree: Option<&Oid>, path: &[u8], value: Slot) -> Result<Option<Oid>> {
    let mut entries = match tree {
        Some(t) => odb.read_tree(t)?,
        None => Vec::new(),
    };
    let (name, rest) = match path.iter().position(|b| *b == b'/') {
        Some(i) => (&path[..i], Some(&path[i + 1..])),
        None => (path, None),
    };
    let at = entries.iter().position(|e| e.name == name);
    let new: Slot = match rest {
        None => value,
        Some(rest) => {
            let sub = at.map(|i| &entries[i]).filter(|e| e.mode == MODE_TREE).map(|e| e.oid);
            with_path(odb, sub.as_ref(), rest, value)?.map(|oid| (MODE_TREE, oid))
        }
    };
    if let Some(i) = at {
        entries.remove(i);
    }
    if let Some((mode, oid)) = new {
        entries.push(TreeEntry { mode, name: name.to_vec(), oid });
    }
    if entries.is_empty() {
        return Ok(None);
    }
    entries.sort_by_key(sort_key);
    Ok(Some(odb.write(Kind::Tree, &serialize_tree(&entries))?))
}

/// Three-way merge of trees, limited to the cases with exactly one possible
/// result: for every path at most one side changed it, or both made the same
/// change. As soon as a path was changed differently on both sides the merge
/// is declined, because then git's rename detection and content merge decide
/// and the operation is git's to do.
pub fn merge_trees(odb: &dyn Objects, base: Option<&Oid>, ours: Option<&Oid>, theirs: Option<&Oid>) -> Result<Option<Oid>> {
    if ours == theirs || theirs == base {
        return Ok(ours.copied());
    }
    if ours == base {
        return Ok(theirs.copied());
    }
    let read = |t: Option<&Oid>| -> Result<BTreeMap<Vec<u8>, (u32, Oid)>> {
        Ok(match t {
            Some(o) => by_name(odb.read_tree(o)?),
            None => BTreeMap::new(),
        })
    };
    let (b, o, t) = (read(base)?, read(ours)?, read(theirs)?);
    let names: std::collections::BTreeSet<&Vec<u8>> = b.keys().chain(o.keys()).chain(t.keys()).collect();
    let mut merged = Vec::with_capacity(names.len());
    for name in names {
        let (bs, os, ts) = (b.get(name).copied(), o.get(name).copied(), t.get(name).copied());
        let result: Slot = if os == ts || ts == bs {
            os
        } else if os == bs {
            ts
        } else if is_tree(&os) && is_tree(&ts) && (bs.is_none() || is_tree(&bs)) {
            merge_trees(odb, bs.as_ref().map(|s| &s.1), os.as_ref().map(|s| &s.1), ts.as_ref().map(|s| &s.1))?.map(|oid| (MODE_TREE, oid))
        } else {
            return unsupported(format!("both sides changed {}", String::from_utf8_lossy(name)));
        };
        if let Some((mode, oid)) = result {
            merged.push(TreeEntry { mode, name: name.clone(), oid });
        }
    }
    if merged.is_empty() {
        return Ok(None);
    }
    merged.sort_by_key(sort_key);
    Ok(Some(odb.write(Kind::Tree, &serialize_tree(&merged))?))
}

/// Write the tree of the index (`git write-tree`) and make its cache-tree
/// fully valid. Directories whose cache-tree entry is valid are not touched.
pub fn write_index_tree(odb: &dyn Objects, index: &mut Index) -> Result<Oid> {
    if index.has_conflicts() {
        bail!("the index has unmerged entries");
    }
    let cache = index.cache_tree.take();
    let mut pos = 0;
    let node = build(odb, index, &mut pos, 0, cache.as_ref(), Vec::new())?;
    let oid = node.oid;
    index.cache_tree = Some(node);
    Ok(oid)
}

fn build(odb: &dyn Objects, index: &Index, pos: &mut usize, depth: usize, cache: Option<&CacheTree>, name: Vec<u8>) -> Result<CacheTree> {
    if let Some(c) = cache {
        if c.entry_count >= 0 {
            *pos += c.entry_count as usize;
            return Ok(c.clone());
        }
    }
    let start = *pos;
    let prefix: Vec<u8> = index.entries.get(start).map(|e| e.path[..depth].to_vec()).unwrap_or_default();
    let mut entries = Vec::new();
    let mut children = Vec::new();
    while let Some(e) = index.entries.get(*pos) {
        if e.path.len() < depth || e.path[..depth] != prefix[..] {
            break;
        }
        let rest = &e.path[depth..];
        match rest.iter().position(|b| *b == b'/') {
            None => {
                // Intent-to-add entries are not part of the tree.
                if e.xflags & super::index::XFLAG_INTENT_TO_ADD == 0 {
                    entries.push(TreeEntry { mode: if e.mode == MODE_GITLINK { MODE_GITLINK } else { e.mode }, name: rest.to_vec(), oid: e.oid });
                }
                *pos += 1;
            }
            Some(slash) => {
                let dir = rest[..slash].to_vec();
                let cached = cache.and_then(|c| c.children.iter().find(|c| c.name == dir));
                let child = build(odb, index, pos, depth + slash + 1, cached, dir.clone())?;
                if child.entry_count > 0 {
                    entries.push(TreeEntry { mode: MODE_TREE, name: dir, oid: child.oid });
                    children.push(child);
                }
            }
        }
    }
    // Index order equals tree order: a directory sorts as `name/` in both.
    let oid = odb.write(Kind::Tree, &serialize_tree(&entries))?;
    Ok(CacheTree { name, entry_count: (*pos - start) as i32, oid, children })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    /// Objects in memory, with ids that are just a counter.
    #[derive(Default)]
    struct Mem(Mutex<(HashMap<Oid, (Kind, Vec<u8>)>, HashMap<Vec<u8>, Oid>)>);

    impl Objects for Mem {
        fn read(&self, oid: &Oid) -> Result<(Kind, Vec<u8>)> {
            self.0.lock().unwrap().0.get(oid).cloned().context("missing object")
        }
        fn write(&self, kind: Kind, data: &[u8]) -> Result<Oid> {
            let mut g = self.0.lock().unwrap();
            if let Some(oid) = g.1.get(data) {
                return Ok(*oid);
            }
            let mut oid = [0u8; 20];
            oid[..8].copy_from_slice(&(g.0.len() as u64 + 1).to_be_bytes());
            g.0.insert(oid, (kind, data.to_vec()));
            g.1.insert(data.to_vec(), oid);
            Ok(oid)
        }
    }

    fn blob(odb: &Mem, text: &str) -> Slot {
        Some((0o100644, odb.write(Kind::Blob, text.as_bytes()).unwrap()))
    }

    fn tree(odb: &Mem, files: &[(&str, &str)]) -> Option<Oid> {
        let mut t = None;
        for (path, text) in files {
            t = with_path(odb, t.as_ref(), path.as_bytes(), blob(odb, text)).unwrap();
        }
        t
    }

    #[test]
    fn diff_lists_changed_files_only() {
        let odb = Mem::default();
        let a = tree(&odb, &[("a/x", "1"), ("a/y", "2"), ("b", "3"), ("c/d/e", "4")]);
        let b = tree(&odb, &[("a/x", "1"), ("a/y", "2!"), ("b/now-a-dir", "3"), ("z", "5")]);
        let mut out = Vec::new();
        diff_trees(&odb, a.as_ref(), b.as_ref(), &mut out).unwrap();
        let paths: Vec<String> =
            out.iter().map(|c| format!("{}:{}{}", String::from_utf8_lossy(&c.path), c.old.is_some() as u8, c.new.is_some() as u8)).collect();
        assert_eq!(paths, ["a/y:11", "b:10", "b/now-a-dir:01", "c/d/e:10", "z:01"]);
    }

    #[test]
    fn merge_takes_the_single_change_per_path() {
        let odb = Mem::default();
        let base = tree(&odb, &[("a/x", "1"), ("a/y", "2"), ("b", "3")]);
        let ours = tree(&odb, &[("a/x", "1 ours"), ("a/y", "2"), ("b", "3"), ("new/ours", "n")]);
        let theirs = tree(&odb, &[("a/x", "1"), ("a/y", "2 theirs"), ("new/theirs", "m")]);
        let want = tree(&odb, &[("a/x", "1 ours"), ("a/y", "2 theirs"), ("new/ours", "n"), ("new/theirs", "m")]);
        assert_eq!(merge_trees(&odb, base.as_ref(), ours.as_ref(), theirs.as_ref()).unwrap(), want);
    }

    #[test]
    fn merge_declines_when_both_sides_changed_a_path() {
        let odb = Mem::default();
        let base = tree(&odb, &[("a/x", "1")]);
        let ours = tree(&odb, &[("a/x", "ours")]);
        let theirs = tree(&odb, &[("a/x", "theirs")]);
        let err = merge_trees(&odb, base.as_ref(), ours.as_ref(), theirs.as_ref()).unwrap_err();
        assert!(err.downcast_ref::<crate::api::Unsupported>().is_some());
        // A directory removed on one side and changed on the other, too.
        let gone = tree(&odb, &[("other", "o")]);
        let base2 = tree(&odb, &[("a/x", "1"), ("other", "o")]);
        let theirs2 = tree(&odb, &[("a/x", "1"), ("a/added", "2"), ("other", "o")]);
        assert!(merge_trees(&odb, base2.as_ref(), gone.as_ref(), theirs2.as_ref()).is_err());
    }
}
