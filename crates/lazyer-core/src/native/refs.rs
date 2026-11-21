//! Repository layout, references and reflogs (files backend).

use super::tree::{hex, unhex, Oid};
use crate::api::{unsupported, Ident, Result};
use anyhow::{bail, Context};
use std::io::Write;
use std::path::{Path, PathBuf};

/// Where the pieces of a (possibly linked) worktree live.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Layout {
    pub worktree: PathBuf,
    /// Per-worktree git directory: HEAD, index, logs/HEAD.
    pub git_dir: PathBuf,
    /// Shared git directory: objects, refs, packed-refs, config.
    pub common_dir: PathBuf,
}

impl Layout {
    pub fn discover(worktree: &Path) -> Result<Layout> {
        let dot_git = worktree.join(".git");
        let meta = std::fs::symlink_metadata(&dot_git).with_context(|| format!("{} is not a worktree root", worktree.display()))?;
        let git_dir = if meta.is_dir() {
            dot_git
        } else {
            let text = std::fs::read_to_string(&dot_git)?;
            let target = text.trim().strip_prefix("gitdir: ").context("malformed .git file")?;
            let p = PathBuf::from(target);
            if p.is_absolute() {
                p
            } else {
                worktree.join(p)
            }
        };
        let common_dir = match std::fs::read_to_string(git_dir.join("commondir")) {
            Ok(text) => {
                let p = PathBuf::from(text.trim());
                let p = if p.is_absolute() { p } else { git_dir.join(p) };
                p.canonicalize().unwrap_or(p)
            }
            Err(_) => git_dir.clone(),
        };
        if common_dir.join("reftable").exists() {
            return unsupported("reftable ref storage");
        }
        Ok(Layout { worktree: worktree.to_path_buf(), git_dir, common_dir })
    }

    /// HEAD and other pseudo-refs are per worktree, everything below `refs/` is shared.
    fn ref_file(&self, name: &str) -> PathBuf {
        if name.starts_with("refs/") {
            self.common_dir.join(name)
        } else {
            self.git_dir.join(name)
        }
    }

    fn log_file(&self, name: &str) -> PathBuf {
        if name.starts_with("refs/") {
            self.common_dir.join("logs").join(name)
        } else {
            self.git_dir.join("logs").join(name)
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Head {
    /// Full ref name and its commit, `None` on an unborn branch.
    Branch(String, Option<Oid>),
    Detached(Oid),
}

impl Head {
    pub fn oid(&self) -> Option<Oid> {
        match self {
            Head::Branch(_, o) => *o,
            Head::Detached(o) => Some(*o),
        }
    }
}

pub fn read_head(l: &Layout) -> Result<Head> {
    let text = std::fs::read_to_string(l.git_dir.join("HEAD")).context("reading HEAD")?;
    let text = text.trim();
    match text.strip_prefix("ref: ") {
        Some(name) => Ok(Head::Branch(name.to_string(), resolve(l, name)?)),
        None => Ok(Head::Detached(unhex(text)?)),
    }
}

/// The commit a ref points to: loose file first, then packed-refs.
pub fn resolve(l: &Layout, name: &str) -> Result<Option<Oid>> {
    let mut name = name.to_string();
    for _ in 0..8 {
        match std::fs::read_to_string(l.ref_file(&name)) {
            Ok(text) => {
                let text = text.trim();
                match text.strip_prefix("ref: ") {
                    Some(next) => name = next.to_string(),
                    None => return Ok(Some(unhex(text)?)),
                }
            }
            Err(_) => return packed_lookup(l, &name),
        }
    }
    bail!("symbolic ref loop at {name}")
}

fn packed_lookup(l: &Layout, name: &str) -> Result<Option<Oid>> {
    let Ok(data) = std::fs::read(l.common_dir.join("packed-refs")) else { return Ok(None) };
    let needle = name.as_bytes();
    for line in data.split(|b| *b == b'\n') {
        if line.len() == 41 + needle.len() && line[40] == b' ' && &line[41..] == needle {
            return Ok(Some(unhex(std::str::from_utf8(&line[..40])?)?));
        }
    }
    Ok(None)
}

fn reflog_line(old: Option<&Oid>, new: &Oid, who: &Ident, message: &str) -> String {
    let zero = [0u8; 20];
    format!("{} {} {} <{}> {} {}\t{}\n", hex(old.unwrap_or(&zero)), hex(new), who.name, who.email, who.time, who.tz, message)
}

fn append_log(l: &Layout, name: &str, line: &str) -> Result<()> {
    let path = l.log_file(name);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&path)?;
    f.write_all(line.as_bytes())?;
    Ok(())
}

/// Replace a file the way git does: write `<file>.lock` exclusively, rename
/// over the target. A lock held by another process is an error.
fn replace_file(path: &Path, content: &[u8]) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut lock = path.as_os_str().to_owned();
    lock.push(".lock");
    let lock = PathBuf::from(lock);
    let mut f =
        std::fs::OpenOptions::new().write(true).create_new(true).open(&lock).with_context(|| format!("{} is locked by another git process", path.display()))?;
    let written = f.write_all(content);
    drop(f);
    if let Err(e) = written.and_then(|_| std::fs::rename(&lock, path)) {
        let _ = std::fs::remove_file(&lock);
        return Err(e.into());
    }
    Ok(())
}

/// Point a branch (or other ref below `refs/`) at `new`. `expected` is what
/// the ref must currently be; a concurrent change makes this fail.
pub fn update_ref(l: &Layout, name: &str, new: &Oid, expected: Option<&Oid>, who: &Ident, message: &str) -> Result<()> {
    let current = resolve(l, name)?;
    if current.as_ref() != expected {
        bail!("{name} changed underneath: expected {:?}, found {:?}", expected.map(hex), current.as_ref().map(hex));
    }
    replace_file(&l.ref_file(name), format!("{}\n", hex(new)).as_bytes())?;
    append_log(l, name, &reflog_line(expected, new, who, message))
}

/// Record a movement of HEAD in its reflog. Used when the checked-out branch
/// moved or when HEAD itself was repointed.
pub fn log_head(l: &Layout, old: Option<&Oid>, new: &Oid, who: &Ident, message: &str) -> Result<()> {
    append_log(l, "HEAD", &reflog_line(old, new, who, message))
}

pub fn set_head_to_branch(l: &Layout, name: &str) -> Result<()> {
    replace_file(&l.git_dir.join("HEAD"), format!("ref: {name}\n").as_bytes())
}

pub fn set_head_detached(l: &Layout, oid: &Oid) -> Result<()> {
    replace_file(&l.git_dir.join("HEAD"), format!("{}\n", hex(oid)).as_bytes())
}

/// `ORIG_HEAD`, which git sets before history-rewriting operations.
pub fn set_orig_head(l: &Layout, oid: &Oid) -> Result<()> {
    replace_file(&l.git_dir.join("ORIG_HEAD"), format!("{}\n", hex(oid)).as_bytes())
}

/// Git's `index.lock`: held from before the index is read until the new
/// index is in place, so a concurrent `git add` or checkout cannot be lost.
/// Released without changing anything when dropped.
pub struct IndexLock {
    lock: PathBuf,
    target: PathBuf,
    committed: bool,
}

impl IndexLock {
    pub fn acquire(l: &Layout) -> Result<IndexLock> {
        let target = l.git_dir.join("index");
        let lock = l.git_dir.join("index.lock");
        std::fs::OpenOptions::new().write(true).create_new(true).open(&lock).context("the index is locked by another git process")?;
        Ok(IndexLock { lock, target, committed: false })
    }

    /// Make `content` the new index.
    pub fn commit(mut self, content: &[u8]) -> Result<()> {
        std::fs::write(&self.lock, content)?;
        std::fs::rename(&self.lock, &self.target)?;
        self.committed = true;
        Ok(())
    }
}

impl Drop for IndexLock {
    fn drop(&mut self) {
        if !self.committed {
            let _ = std::fs::remove_file(&self.lock);
        }
    }
}

/// Worktrees of the repository as `(root, git dir)`, the main one first.
pub fn worktrees(l: &Layout) -> Result<Vec<(PathBuf, PathBuf)>> {
    let mut out = Vec::new();
    if let Some(main) = l.common_dir.parent() {
        if l.common_dir.file_name().is_some_and(|n| n == ".git") {
            out.push((main.to_path_buf(), l.common_dir.clone()));
        }
    }
    let Ok(dir) = std::fs::read_dir(l.common_dir.join("worktrees")) else { return Ok(out) };
    let mut linked = Vec::new();
    for entry in dir.flatten() {
        let git_dir = entry.path();
        // `gitdir` holds the path of the worktree's `.git` file.
        let Ok(text) = std::fs::read_to_string(git_dir.join("gitdir")) else { continue };
        let dot_git = PathBuf::from(text.trim());
        if let Some(root) = dot_git.parent() {
            linked.push((root.to_path_buf(), git_dir));
        }
    }
    linked.sort();
    out.extend(linked);
    Ok(out)
}
