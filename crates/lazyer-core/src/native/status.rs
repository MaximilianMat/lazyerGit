//! `git status`: staged changes (HEAD against index), unstaged changes
//! (index against files) and untracked files.
//!
//! The three parts are independent and each is bounded by what actually
//! changed: staged changes come out of the cache-tree without reading
//! unchanged directories, tracked files are checked in parallel, and the
//! search for untracked files walks directories in parallel.

use super::checkout::worktree_blob_id;
use super::index::{CacheTree, Entry, Index, Stat, MODE_EXEC, MODE_GITLINK, MODE_LINK, XFLAG_INTENT_TO_ADD, XFLAG_SKIP_WORKTREE};
use super::tree::{Objects, Oid, MODE_TREE};
use crate::api::{unsupported, Result, StatusEntry};
use gix::bstr::BStr;
use rayon::prelude::*;
use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Arc;

type Slot = Option<(u32, Oid)>;

/// A difference between HEAD's tree and the index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Staged {
    pub path: Vec<u8>,
    pub old: Slot,
    pub new: Slot,
}

/// Staged changes. Directories whose cache-tree entry is valid and equal to
/// HEAD's tree are skipped without looking at their entries.
pub fn staged_changes(odb: &dyn Objects, index: &Index, head_tree: Option<&Oid>) -> Result<Vec<Staged>> {
    if index.has_conflicts() {
        return unsupported("unmerged entries in the index");
    }
    let mut out = Vec::new();
    let mut pos = 0;
    staged_rec(odb, &index.entries, &mut pos, 0, index.cache_tree.as_ref(), head_tree.copied(), &mut out)?;
    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

fn flatten(odb: &dyn Objects, tree: &Oid, prefix: &mut Vec<u8>, out: &mut Vec<Staged>) -> Result<()> {
    for e in odb.read_tree(tree)? {
        let len = prefix.len();
        prefix.extend_from_slice(&e.name);
        if e.mode == MODE_TREE {
            prefix.push(b'/');
            flatten(odb, &e.oid, prefix, out)?;
        } else {
            out.push(Staged { path: prefix.clone(), old: Some((e.mode, e.oid)), new: None });
        }
        prefix.truncate(len);
    }
    Ok(())
}

fn staged_rec(
    odb: &dyn Objects,
    entries: &[Entry],
    pos: &mut usize,
    depth: usize,
    cache: Option<&CacheTree>,
    head: Option<Oid>,
    out: &mut Vec<Staged>,
) -> Result<()> {
    if let Some(c) = cache {
        if c.entry_count >= 0 && Some(c.oid) == head {
            *pos += c.entry_count as usize;
            return Ok(());
        }
    }
    let in_head: HashMap<Vec<u8>, (u32, Oid)> = match &head {
        Some(t) => odb.read_tree(t)?.into_iter().map(|e| (e.name, (e.mode, e.oid))).collect(),
        None => HashMap::new(),
    };
    let prefix: Vec<u8> = entries.get(*pos).map(|e| e.path[..depth.min(e.path.len())].to_vec()).unwrap_or_default();
    let mut seen: HashSet<&[u8]> = HashSet::new();
    while let Some(e) = entries.get(*pos) {
        if e.path.len() < depth || e.path[..depth] != prefix[..] {
            break;
        }
        let rest = &e.path[depth..];
        match rest.iter().position(|b| *b == b'/') {
            None => {
                seen.insert(rest);
                *pos += 1;
                if e.xflags & XFLAG_INTENT_TO_ADD != 0 {
                    return unsupported("intent-to-add entries");
                }
                let new = Some((e.mode, e.oid));
                match in_head.get(rest).copied() {
                    Some((MODE_TREE, tree)) => {
                        let mut p = e.path.to_vec();
                        p.push(b'/');
                        flatten(odb, &tree, &mut p, out)?;
                        out.push(Staged { path: e.path.to_vec(), old: None, new });
                    }
                    old if old != new => out.push(Staged { path: e.path.to_vec(), old, new }),
                    _ => {}
                }
            }
            Some(slash) => {
                let dir = &rest[..slash];
                seen.insert(dir);
                let sub = match in_head.get(dir).copied() {
                    Some((MODE_TREE, tree)) => Some(tree),
                    Some(file) => {
                        out.push(Staged { path: e.path[..depth + slash].to_vec(), old: Some(file), new: None });
                        None
                    }
                    None => None,
                };
                let child = cache.and_then(|c| c.children.iter().find(|c| c.name == dir));
                staged_rec(odb, entries, pos, depth + slash + 1, child, sub, out)?;
            }
        }
    }
    for (name, (mode, oid)) in &in_head {
        if seen.contains(name.as_slice()) {
            continue;
        }
        let mut p = prefix.clone();
        p.extend_from_slice(name);
        if *mode == MODE_TREE {
            p.push(b'/');
            flatten(odb, oid, &mut p, out)?;
        } else {
            out.push(Staged { path: p, old: Some((*mode, *oid)), new: None });
        }
    }
    Ok(())
}

fn kind_of(mode: u32) -> u8 {
    match mode {
        MODE_LINK => b'l',
        MODE_GITLINK => b'g',
        _ => b'f',
    }
}

/// Turn staged changes into status entries, pairing deletions with additions
/// of identical content as renames. Whether two *different* contents count as
/// a rename is decided by git's similarity estimate; when such a pair is
/// possible at all, the status is left to git.
fn staged_entries(odb: &dyn Objects, staged: Vec<Staged>) -> Result<Vec<(Vec<u8>, char, Option<Vec<u8>>)>> {
    let mut out = Vec::new();
    let mut deleted: Vec<&Staged> = Vec::new();
    let mut added: Vec<&Staged> = Vec::new();
    for s in &staged {
        match (s.old, s.new) {
            (Some(o), Some(n)) => out.push((s.path.clone(), if kind_of(o.0) != kind_of(n.0) { 'T' } else { 'M' }, None)),
            (Some(_), None) => deleted.push(s),
            (None, Some(_)) => added.push(s),
            (None, None) => {}
        }
    }
    if !deleted.is_empty() && !added.is_empty() {
        // Exact renames: same blob on both sides.
        let mut by_oid: HashMap<Oid, Vec<usize>> = HashMap::new();
        for (i, d) in deleted.iter().enumerate() {
            by_oid.entry(d.old.unwrap().1).or_default().push(i);
        }
        let mut used = vec![false; deleted.len()];
        let mut paired = vec![false; added.len()];
        for (ai, a) in added.iter().enumerate() {
            let Some(candidates) = by_oid.get(&a.new.unwrap().1) else { continue };
            let free: Vec<usize> = candidates.iter().copied().filter(|i| !used[*i]).collect();
            let base = |p: &[u8]| p.rsplit(|b| *b == b'/').next().unwrap_or(p).to_vec();
            let same_name: Vec<usize> = free.iter().copied().filter(|i| base(&deleted[*i].path) == base(&a.path)).collect();
            let pick = match (same_name.len(), free.len()) {
                (1, _) => same_name[0],
                (0, 1) => free[0],
                (0, 0) => continue,
                // Several equally good sources: git's choice depends on its
                // internal ordering.
                _ => return unsupported("ambiguous exact renames"),
            };
            used[pick] = true;
            paired[ai] = true;
            out.push((a.path.clone(), 'R', Some(deleted[pick].path.clone())));
        }
        // Inexact renames among what is left, by git's similarity estimate.
        // Taken only when the pairing is unambiguous: with several
        // candidates for one file git's choice depends on its internal
        // ordering, and then git decides.
        let rest_d: Vec<usize> = (0..deleted.len()).filter(|i| !used[*i]).collect();
        let rest_a: Vec<usize> = (0..added.len()).filter(|i| !paired[*i]).collect();
        if !rest_d.is_empty() && !rest_a.is_empty() {
            if rest_d.len() * rest_a.len() > 400 {
                return unsupported("many rename candidates");
            }
            // Only regular files take part; a symlink is renamed only when identical.
            let blob = |s: &Slot| -> Result<Option<Vec<u8>>> {
                match s {
                    Some((mode, oid)) if *mode == super::index::MODE_FILE || *mode == MODE_EXEC => Ok(Some(odb.read(oid)?.1)),
                    _ => Ok(None),
                }
            };
            let d_blobs: Vec<Option<Vec<u8>>> = rest_d.iter().map(|i| blob(&deleted[*i].old)).collect::<Result<_>>()?;
            let a_blobs: Vec<Option<Vec<u8>>> = rest_a.iter().map(|i| blob(&added[*i].new)).collect::<Result<_>>()?;
            let mut candidates: Vec<(usize, usize)> = Vec::new();
            for (di, d) in d_blobs.iter().enumerate() {
                for (ai, a) in a_blobs.iter().enumerate() {
                    if let (Some(d), Some(a)) = (d, a) {
                        if super::similarity::score(d, a) >= super::similarity::MIN_SCORE {
                            candidates.push((di, ai));
                        }
                    }
                }
            }
            for (n, (di, ai)) in candidates.iter().enumerate() {
                if candidates[n + 1..].iter().any(|(d2, a2)| d2 == di || a2 == ai) {
                    return unsupported("ambiguous inexact renames");
                }
            }
            for (di, ai) in candidates {
                used[rest_d[di]] = true;
                paired[rest_a[ai]] = true;
                out.push((added[rest_a[ai]].path.clone(), 'R', Some(deleted[rest_d[di]].path.clone())));
            }
        }
        for (i, d) in deleted.iter().enumerate() {
            if !used[i] {
                out.push((d.path.clone(), 'D', None));
            }
        }
        for (i, a) in added.iter().enumerate() {
            if !paired[i] {
                out.push((a.path.clone(), 'A', None));
            }
        }
    } else {
        out.extend(deleted.iter().map(|d| (d.path.clone(), 'D', None)));
        out.extend(added.iter().map(|a| (a.path.clone(), 'A', None)));
    }
    Ok(out)
}

/// How a tracked file differs from its index entry: `M`, `D`, `T` or nothing.
fn unstaged_state(root: &Path, e: &Entry, index_mtime: (u32, u32)) -> Result<Option<char>> {
    let path = root.join(OsStr::from_bytes(&e.path));
    let meta = match std::fs::symlink_metadata(&path) {
        Ok(m) => m,
        Err(_) => return Ok(Some('D')),
    };
    if meta.is_dir() {
        return Ok(Some('D'));
    }
    let is_link = meta.file_type().is_symlink();
    if !is_link && !meta.is_file() {
        return unsupported("a FIFO, socket or device in place of a tracked file");
    }
    if is_link != (e.mode == MODE_LINK) {
        return Ok(Some('T'));
    }
    if !is_link && (meta.permissions().mode() & 0o100 != 0) != (e.mode == MODE_EXEC) {
        return Ok(Some('M'));
    }
    let racy = e.stat.racy(index_mtime);
    if e.stat.matches(&Stat::from_metadata(&meta)) && !racy {
        return Ok(None);
    }
    // Stat differs or cannot be trusted: the content decides.
    if e.stat.size != 0 && meta.len() != e.stat.size as u64 {
        return Ok(Some('M'));
    }
    Ok((worktree_blob_id(&path, &meta)? != e.oid).then_some('M'))
}

/// Ignore rules in effect for one directory: the repository-wide ones plus
/// every `.gitignore` from the root down to it.
type Rules = Arc<gix::ignore::Search>;

/// Directory listings kept between status runs. A directory is read again
/// only when its modification time changed, which every creation, removal or
/// rename inside it causes; otherwise finding untracked files costs one
/// `lstat` per directory instead of reading all of them.
#[derive(Default)]
pub struct DirCache {
    dirs: std::sync::RwLock<HashMap<Vec<u8>, Arc<Listing>>>,
}

struct Listing {
    /// Modification time and inode of the directory when it was read.
    stamp: (i64, i64, u64),
    /// Name and whether it is a directory.
    names: Vec<(Vec<u8>, bool)>,
}

impl DirCache {
    fn listing(&self, abs: &Path, rel_dir: &[u8]) -> Result<Option<Arc<Listing>>> {
        use std::os::unix::fs::MetadataExt;
        let Ok(meta) = std::fs::symlink_metadata(abs) else { return Ok(None) };
        if !meta.is_dir() {
            return Ok(None);
        }
        let stamp = (meta.mtime(), meta.mtime_nsec(), meta.ino());
        if let Some(l) = self.dirs.read().unwrap().get(rel_dir) {
            if l.stamp == stamp {
                return Ok(Some(l.clone()));
            }
        }
        let Ok(read) = std::fs::read_dir(abs) else { return Ok(None) };
        let mut names = Vec::new();
        for entry in read {
            let entry = entry?;
            names.push((entry.file_name().as_bytes().to_vec(), entry.file_type()?.is_dir()));
        }
        let listing = Arc::new(Listing { stamp, names });
        // A directory modified this very moment could change again without
        // its timestamp moving: do not remember such a listing.
        let age = std::time::SystemTime::now().duration_since(meta.modified()?).unwrap_or_default();
        let settled = if stamp.1 != 0 { age > std::time::Duration::from_millis(5) } else { age > std::time::Duration::from_secs(2) };
        if settled {
            self.dirs.write().unwrap().insert(rel_dir.to_vec(), listing.clone());
        } else {
            self.dirs.write().unwrap().remove(rel_dir);
        }
        Ok(Some(listing))
    }
}

struct Walk<'a> {
    root: &'a Path,
    index: &'a Index,
    case: gix::glob::pattern::Case,
    cache: &'a DirCache,
}

impl Walk<'_> {
    fn ignored(&self, rules: &Rules, rel: &[u8], is_dir: bool) -> bool {
        rules.pattern_matching_relative_path(BStr::new(rel), Some(is_dir), self.case).is_some_and(|m| !m.pattern.is_negative())
    }

    /// Untracked files below `rel_dir` (empty for the root, otherwise with a
    /// trailing slash).
    fn dir(&self, rel_dir: &[u8], rules: &Rules) -> Result<Vec<Vec<u8>>> {
        let abs = if rel_dir.is_empty() { self.root.to_path_buf() } else { self.root.join(OsStr::from_bytes(&rel_dir[..rel_dir.len() - 1])) };
        let Some(listing) = self.cache.listing(&abs, rel_dir)? else { return Ok(Vec::new()) };
        let mut gitignore = false;
        for (name, is_dir) in &listing.names {
            if name == b".git" && !rel_dir.is_empty() {
                return unsupported("nested repository");
            }
            if name == b".gitignore" && !is_dir {
                gitignore = true;
            }
        }
        let rules = if gitignore {
            let source = abs.join(".gitignore");
            let mut search = (**rules).clone();
            if let Ok(bytes) = std::fs::read(&source) {
                search.add_patterns_buffer(&bytes, source, Some(self.root), Default::default())?;
            }
            Arc::new(search)
        } else {
            rules.clone()
        };

        // Tracked names directly in this directory: the index is sorted, so
        // they form one contiguous range.
        let start = self.index.entries.partition_point(|e| &*e.path < rel_dir);
        let mut tracked_files: HashSet<&[u8]> = HashSet::new();
        for e in &self.index.entries[start..] {
            let Some(rest) = e.path.strip_prefix(rel_dir) else { break };
            if !rest.contains(&b'/') {
                tracked_files.insert(rest);
            }
        }

        let mut out = Vec::new();
        let mut subdirs = Vec::new();
        for (name, is_dir) in &listing.names {
            if name == b".git" {
                continue;
            }
            let (name, is_dir) = (name, *is_dir);
            let mut rel = rel_dir.to_vec();
            rel.extend_from_slice(name);
            if is_dir {
                // An ignored directory is only entered for the tracked files
                // in it, and those are not this walk's business.
                if self.ignored(&rules, &rel, true) {
                    continue;
                }
                rel.push(b'/');
                subdirs.push(rel);
            } else if !tracked_files.contains(name.as_slice()) && !self.ignored(&rules, &rel, false) {
                out.push(rel);
            }
        }
        let nested: Vec<Vec<Vec<u8>>> = if subdirs.len() > 1 {
            subdirs.par_iter().map(|d| self.dir(d, &rules)).collect::<Result<_>>()?
        } else {
            subdirs.iter().map(|d| self.dir(d, &rules)).collect::<Result<_>>()?
        };
        out.extend(nested.into_iter().flatten());
        Ok(out)
    }
}

pub struct StatusInput<'a> {
    pub root: &'a Path,
    pub odb: &'a dyn Objects,
    pub index: &'a Index,
    pub index_mtime: (u32, u32),
    pub head_tree: Option<Oid>,
    /// `info/exclude` and the user's global excludes file.
    pub base_rules: gix::ignore::Search,
    pub ignore_case: bool,
    pub dir_cache: &'a DirCache,
}

/// All entries of `git status --untracked-files=all`, sorted.
pub fn entries(input: &StatusInput) -> Result<Vec<StatusEntry>> {
    let index = input.index;
    if index.entries.iter().any(|e| e.mode == MODE_GITLINK) {
        return unsupported("submodules");
    }
    if index.entries.iter().any(|e| e.xflags & XFLAG_SKIP_WORKTREE != 0) {
        return unsupported("skip-worktree entries");
    }
    let case = if input.ignore_case { gix::glob::pattern::Case::Fold } else { gix::glob::pattern::Case::Sensitive };
    let walk = Walk { root: input.root, index, case, cache: input.dir_cache };
    let rules: Rules = Arc::new(input.base_rules.clone());

    let (staged, (unstaged, untracked)) = rayon::join(
        || staged_changes(input.odb, index, input.head_tree.as_ref()).and_then(|s| staged_entries(input.odb, s)),
        || {
            rayon::join(
                || {
                    index
                        .entries
                        .par_iter()
                        .filter_map(|e| match unstaged_state(input.root, e, input.index_mtime) {
                            Ok(None) => None,
                            Ok(Some(c)) => Some(Ok((&*e.path, c))),
                            Err(err) => Some(Err(err)),
                        })
                        .collect::<Result<Vec<_>>>()
                },
                || walk.dir(b"", &rules),
            )
        },
    );

    let mut by_path: HashMap<Vec<u8>, StatusEntry> = HashMap::new();
    let text = |p: &[u8]| String::from_utf8_lossy(p).to_string();
    for (path, x, orig) in staged? {
        by_path.insert(path.clone(), StatusEntry { path: text(&path), index: x, worktree: ' ', orig_path: orig.map(|o| text(&o)) });
    }
    for (path, y) in unstaged? {
        by_path.entry(path.to_vec()).or_insert_with(|| StatusEntry { path: text(path), index: ' ', worktree: ' ', orig_path: None }).worktree = y;
    }
    let mut out: Vec<StatusEntry> = by_path.into_values().collect();
    for path in untracked? {
        out.push(StatusEntry { path: text(&path), index: '?', worktree: '?', orig_path: None });
    }
    out.sort();
    Ok(out)
}
