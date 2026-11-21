//! Moving a worktree from one tree to another (`git checkout <branch>`).
//!
//! Git walks the whole index and stats every tracked file. Here only the
//! paths that differ between the two trees are looked at: everything else is
//! by definition not affected by the switch.

use super::index::{Entry, Index, Stat, MODE_EXEC, MODE_GITLINK, MODE_LINK};
use super::refs::Layout;
use super::tree::{diff_trees, Change, Kind, Objects, Oid};
use crate::api::{unsupported, Result};
use anyhow::{bail, Context};
use std::collections::HashSet;
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

fn fs_path(root: &Path, path: &[u8]) -> PathBuf {
    root.join(OsStr::from_bytes(path))
}

/// Git's blob id of a file's current content (symlink: its target).
pub(crate) fn worktree_blob_id(path: &Path, meta: &std::fs::Metadata) -> Result<Oid> {
    let data = if meta.file_type().is_symlink() { std::fs::read_link(path)?.as_os_str().as_bytes().to_vec() } else { std::fs::read(path)? };
    Ok(super::odb::hash_object(Kind::Blob, &data))
}

/// Whether the file at `path` still is what the index entry says. A missing
/// file counts as unchanged, as it does for git: nothing would be lost.
pub fn entry_unchanged(root: &Path, e: &Entry, index_mtime: (u32, u32)) -> Result<bool> {
    let path = fs_path(root, &e.path);
    let meta = match std::fs::symlink_metadata(&path) {
        Ok(m) => m,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(true),
        Err(err) if err.raw_os_error() == Some(libc_enotdir()) => return Ok(true),
        Err(err) => return Err(err.into()),
    };
    // Directories, and FIFOs or devices that must not be opened.
    if !meta.is_file() && !meta.file_type().is_symlink() {
        return Ok(false);
    }
    let stat = Stat::from_metadata(&meta);
    let mode_matches = if meta.file_type().is_symlink() {
        e.mode == MODE_LINK
    } else {
        e.mode != MODE_LINK && (meta.permissions().mode() & 0o100 != 0) == (e.mode == MODE_EXEC)
    };
    // An entry written in the same instant as the index may have been
    // modified afterwards without a visible stat change ("racy"): look at
    // the content then.
    let racy = e.stat.racy(index_mtime);
    if e.stat.matches(&stat) && mode_matches && !racy {
        return Ok(true);
    }
    if !mode_matches {
        return Ok(false);
    }
    Ok(worktree_blob_id(&path, &meta)? == e.oid)
}

fn libc_enotdir() -> i32 {
    20
}

/// Line-ending conversion, filters and keyword expansion change what is
/// written to disk. The engine writes blobs verbatim, so a repository using
/// any of these is left to git.
pub fn attributes_need_git(odb: &dyn Objects, index: &Index) -> Result<bool> {
    for e in &index.entries {
        let is_attr = &*e.path == b".gitattributes" || e.path.ends_with(b"/.gitattributes");
        if !is_attr {
            continue;
        }
        let (_, data) = odb.read(&e.oid)?;
        for line in data.split(|b| *b == b'\n') {
            if line.first() == Some(&b'#') {
                continue;
            }
            for word in line.split(|b| b.is_ascii_whitespace()).skip(1) {
                let word = word.strip_prefix(b"-").or(word.strip_prefix(b"!")).unwrap_or(word);
                let name = word.split(|b| *b == b'=').next().unwrap_or(word);
                if matches!(name, b"text" | b"eol" | b"crlf" | b"filter" | b"ident" | b"working-tree-encoding") {
                    return Ok(true);
                }
            }
        }
    }
    Ok(false)
}

/// What a switch from one tree to another has to do to the worktree.
#[derive(Debug, Default)]
pub struct Plan {
    pub deletes: Vec<Vec<u8>>,
    pub writes: Vec<(Vec<u8>, u32, Oid)>,
}

impl Plan {
    pub fn is_empty(&self) -> bool {
        self.deletes.is_empty() && self.writes.is_empty()
    }
}

/// Work out the switch from `from` to `to` without changing anything. Fails
/// when a local change would be lost; declines what needs git's judgement.
pub fn plan_switch(l: &Layout, odb: &dyn Objects, index: &Index, index_mtime: (u32, u32), from: &Oid, to: &Oid) -> Result<Plan> {
    let mut changes = Vec::new();
    diff_trees(odb, Some(from), Some(to), &mut changes)?;
    if changes.is_empty() {
        return Ok(Plan::default());
    }
    if index.has_conflicts() {
        return unsupported("unmerged entries in the index");
    }
    let root = &l.worktree;

    // ---- Decide, without changing anything.
    let mut deletes: Vec<&Change> = Vec::new();
    let mut writes: Vec<(&Change, (u32, Oid))> = Vec::new();
    for c in &changes {
        if c.path == b".gitattributes" || c.path.ends_with(b"/.gitattributes") {
            return unsupported("the switch changes .gitattributes");
        }
    }
    for c in &changes {
        if matches!(c.old, Some((MODE_GITLINK, _))) || matches!(c.new, Some((MODE_GITLINK, _))) {
            return unsupported("submodules");
        }
        let entry = index.find(&c.path).ok().map(|i| &index.entries[i]);
        let staged = entry.map(|e| (e.mode, e.oid));
        if staged == c.new {
            // Already what the target wants; local edits on top stay.
            continue;
        }
        if staged != c.old {
            bail!("your staged changes to {} would be overwritten by checkout", String::from_utf8_lossy(&c.path));
        }
        match entry {
            Some(e) => {
                if e.xflags != 0 {
                    return unsupported("skip-worktree or intent-to-add entries");
                }
                if !entry_unchanged(root, e, index_mtime)? {
                    bail!("your local changes to {} would be overwritten by checkout", String::from_utf8_lossy(&c.path));
                }
            }
            None => {
                // New in the target: nothing may be in the way. Whether an
                // existing file may be overwritten depends on ignore rules.
                if std::fs::symlink_metadata(fs_path(root, &c.path)).is_ok() {
                    return unsupported("an untracked file is in the way");
                }
            }
        }
        match c.new {
            Some(new) => writes.push((c, new)),
            None => deletes.push(c),
        }
    }
    // A new file below a path that is currently a file (or the reverse) only
    // works when that file is one of the deletions.
    let deleted: HashSet<&[u8]> = deletes.iter().map(|c| c.path.as_slice()).collect();
    let mut checked: HashSet<Vec<u8>> = HashSet::new();
    // Every parent of a touched path has to be a real directory: through a
    // symlink the switch would reach files outside the worktree.
    for (c, is_write) in writes.iter().map(|(c, _)| (*c, true)).chain(deletes.iter().map(|c| (*c, false))) {
        let mut at = 0;
        while let Some(i) = c.path[at..].iter().position(|b| *b == b'/') {
            let dir = &c.path[..at + i];
            at += i + 1;
            if !checked.insert(dir.to_vec()) {
                continue;
            }
            match std::fs::symlink_metadata(fs_path(root, dir)) {
                Ok(m) if m.is_dir() => {}
                Ok(_) if deleted.contains(dir) => {}
                Ok(_) => return unsupported("a file is where a directory is needed"),
                Err(_) => {}
            }
        }
        if is_write && std::fs::symlink_metadata(fs_path(root, &c.path)).is_ok_and(|m| m.is_dir()) {
            return unsupported("a directory is where a file is needed");
        }
    }

    Ok(Plan {
        deletes: deletes.iter().map(|c| c.path.clone()).collect(),
        writes: writes.iter().map(|(c, (mode, oid))| (c.path.clone(), *mode, *oid)).collect(),
    })
}

/// Put `data` at `path` with the given mode and return the file's metadata.
///
/// An existing plain file is rewritten in place. Git unlinks and re-creates,
/// which costs two directory updates per file; on APFS those dominate a
/// checkout and do not parallelise. In-place is only taken when the result is
/// indistinguishable: a regular file, not a symlink, with no other hard link.
pub(crate) fn write_file(path: &Path, mode: u32, data: &[u8]) -> std::io::Result<std::fs::Metadata> {
    use std::io::Write;
    use std::os::unix::fs::MetadataExt;
    const O_NOFOLLOW: i32 = 0x0100; // macOS; Linux uses 0o400000
    let exec = mode == MODE_EXEC;
    if mode != MODE_LINK && cfg!(target_os = "macos") {
        if let Ok(mut f) = std::fs::OpenOptions::new().write(true).custom_flags(O_NOFOLLOW).open(path) {
            let before = f.metadata()?;
            if before.is_file() && before.nlink() == 1 {
                // Overwrite, then cut off what is left of a longer old file:
                // keeps the allocated blocks instead of freeing and re-allocating.
                f.write_all(data)?;
                if before.size() > data.len() as u64 {
                    f.set_len(data.len() as u64)?;
                }
                if (before.mode() & 0o100 != 0) != exec {
                    let perm = if exec { before.mode() | ((before.mode() & 0o444) >> 2) } else { before.mode() & !0o111 };
                    f.set_permissions(std::fs::Permissions::from_mode(perm & 0o7777))?;
                }
                return f.metadata();
            }
        }
    }
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    if mode == MODE_LINK {
        std::os::unix::fs::symlink(OsStr::from_bytes(data), path)?;
        return std::fs::symlink_metadata(path);
    }
    let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(if exec { 0o777 } else { 0o666 }).open(path)?;
    f.write_all(data)?;
    f.metadata()
}

/// Carry out a plan: update the worktree and the index entries. The index is
/// changed in memory; the caller writes it.
pub fn apply_switch(l: &Layout, odb: &dyn Objects, index: &mut Index, plan: &Plan) -> Result<()> {
    let root = &l.worktree;
    // Deletions first, so files can take the place of directories.
    for del in &plan.deletes {
        let path = fs_path(root, del);
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).with_context(|| format!("removing {}", path.display())),
        }
        let mut dir = path.parent();
        while let Some(d) = dir {
            if d == root || std::fs::remove_dir(d).is_err() {
                break;
            }
            dir = d.parent();
        }
        if let Ok(i) = index.find(del) {
            index.entries.remove(i);
        }
    }
    // Directories first and in order, then the files in parallel: reading
    // and inflating blobs and creating files both scale with cores.
    let mut made_dirs: HashSet<&Path> = HashSet::new();
    let paths: Vec<PathBuf> = plan.writes.iter().map(|(file, _, _)| fs_path(root, file)).collect();
    for path in &paths {
        if let Some(dir) = path.parent() {
            if made_dirs.insert(dir) {
                std::fs::create_dir_all(dir)?;
            }
        }
    }
    use rayon::prelude::*;
    let mut written: Vec<Entry> = plan
        .writes
        .par_iter()
        .zip(paths.par_iter())
        .map(|((file, mode, oid), path)| -> Result<Entry> {
            let (kind, data) = odb.read(oid)?;
            if kind != Kind::Blob {
                bail!("{} is not a blob", String::from_utf8_lossy(file));
            }
            let meta = write_file(path, *mode, &data).with_context(|| format!("writing {}", path.display()))?;
            Ok(Entry { stat: Stat::from_metadata(&meta), mode: *mode, oid: *oid, flags: 0, xflags: 0, path: file.clone().into_boxed_slice() })
        })
        .collect::<Result<Vec<Entry>>>()?;

    // One merge pass instead of shifting the entry list per file.
    written.sort_by(|a, b| a.path.cmp(&b.path));
    let old = std::mem::take(&mut index.entries);
    let mut merged = Vec::with_capacity(old.len() + written.len());
    let mut new = written.into_iter().peekable();
    for e in old {
        while new.peek().is_some_and(|n| n.path < e.path) {
            merged.push(new.next().unwrap());
        }
        if new.peek().is_some_and(|n| n.path == e.path) {
            merged.push(new.next().unwrap());
        } else {
            merged.push(e);
        }
    }
    merged.extend(new);
    index.entries = merged;
    if !plan.is_empty() {
        if let Some(tree) = index.cache_tree.as_mut() {
            for path in plan.deletes.iter().chain(plan.writes.iter().map(|(p, _, _)| p)) {
                tree.invalidate(path);
            }
        }
        index.entries_changed();
    }
    Ok(())
}
