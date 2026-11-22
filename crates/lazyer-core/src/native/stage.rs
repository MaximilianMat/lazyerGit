//! Operations on the index and on single files: staging, unstaging,
//! discarding changes, resets, and creating branches and tags.

use super::checkout::{attributes_need_git, entry_unchanged, worktree_blob_id, write_file};
use super::index::{Entry, Index, Stat, MODE_EXEC, MODE_FILE, MODE_GITLINK, MODE_LINK};
use super::ops::Repo;
use super::refs::{self, Head, IndexLock};
use super::status::staged_changes;
use super::tree::{Kind, Oid};
use super::view;
use crate::api::{unsupported, Result, StatusEntry};
use anyhow::{bail, Context};
use rayon::prelude::*;
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

type Slot = Option<(u32, Oid)>;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ResetMode {
    Soft,
    Mixed,
    Hard,
}

/// Path arguments as lazygit passes them: literal files or directories
/// relative to the worktree root, or `.` for everything. Anything that git
/// would interpret (globs, pathspec magic, parent references) is declined.
pub fn literal_specs(specs: &[&str]) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for s in specs {
        let s = s.trim_end_matches('/');
        if s.is_empty() || s.starts_with([':', '-', '/']) || s.contains(['*', '?', '[', '\\']) || s.split('/').any(|c| c == ".." || (c == "." && s != ".")) {
            return unsupported("pathspec");
        }
        out.push(s.to_string());
    }
    if out.is_empty() {
        return unsupported("no paths");
    }
    Ok(out)
}

fn spec_matches(spec: &str, path: &[u8]) -> bool {
    spec == "." || path == spec.as_bytes() || (path.len() > spec.len() && path.starts_with(spec.as_bytes()) && path[spec.len()] == b'/')
}

fn any_matches(specs: &[String], path: &[u8]) -> bool {
    specs.iter().any(|s| spec_matches(s, path))
}

/// Every spec has to name something git knows; otherwise git reports it.
fn every_spec_names_something(specs: &[String], index: &Index, also: &[&[u8]]) -> Result<()> {
    for s in specs {
        if s != "." && !index.entries.iter().any(|e| spec_matches(s, &e.path)) && !also.iter().any(|p| spec_matches(s, p)) {
            return unsupported("a path matches nothing");
        }
    }
    Ok(())
}

impl Repo<'_> {
    fn abs(&self, path: &[u8]) -> PathBuf {
        self.l.worktree.join(OsStr::from_bytes(path))
    }

    fn content_guard(&self, index: &Index) -> Result<()> {
        if let Some(why) = &self.settings.content_needs_git {
            return unsupported(why.clone());
        }
        if attributes_need_git(self.odb, index)? {
            return unsupported(".gitattributes rules that change file content");
        }
        if index.has_conflicts() {
            return unsupported("unmerged entries in the index");
        }
        Ok(())
    }

    /// The stat to record for an entry that is set to `oid`: the file's own
    /// when its content is exactly that blob, otherwise none (git then
    /// compares content the next time it looks).
    fn stat_if_clean(&self, path: &[u8], mode: u32, oid: &Oid) -> Stat {
        let abs = self.abs(path);
        let Ok(meta) = std::fs::symlink_metadata(&abs) else { return Stat::default() };
        let is_link = meta.file_type().is_symlink();
        if meta.is_dir() || (!meta.is_file() && !is_link) || is_link != (mode == MODE_LINK) {
            return Stat::default();
        }
        if !is_link && (meta.permissions().mode() & 0o100 != 0) != (mode == MODE_EXEC) {
            return Stat::default();
        }
        match worktree_blob_id(&abs, &meta) {
            Ok(id) if id == *oid => Stat::from_metadata(&meta),
            _ => Stat::default(),
        }
    }

    /// Make the index entries of `wanted` paths equal to the given tree
    /// entries (`None` removes the path).
    fn set_entries(&self, index: &mut Index, wanted: &[(Vec<u8>, Slot)]) -> Result<()> {
        for (path, slot) in wanted {
            match (slot, index.find(path)) {
                (Some((mode, oid)), found) => {
                    if *mode == MODE_GITLINK {
                        return unsupported("submodules");
                    }
                    let entry = Entry {
                        stat: self.stat_if_clean(path, *mode, oid),
                        mode: *mode,
                        oid: *oid,
                        flags: 0,
                        xflags: 0,
                        path: path.clone().into_boxed_slice(),
                    };
                    match found {
                        Ok(i) => index.entries[i] = entry,
                        Err(i) => index.entries.insert(i, entry),
                    }
                }
                (None, Ok(i)) => {
                    index.entries.remove(i);
                }
                (None, Err(_)) => {}
            }
            if let Some(tree) = index.cache_tree.as_mut() {
                tree.invalidate(path);
            }
        }
        if !wanted.is_empty() {
            index.entries_changed();
        }
        Ok(())
    }

    /// `git add -- <paths>`, `git add -A`, `git add -u`: record the current
    /// content of changed (and, unless `tracked_only`, new) files.
    pub fn stage(&self, status: &[StatusEntry], specs: &[String], tracked_only: bool) -> Result<()> {
        let lock = IndexLock::acquire(&self.l)?;
        let (mut index, _) = self.load_index()?;
        self.content_guard(&index)?;
        if index.entries.iter().any(|e| e.xflags != 0) {
            return unsupported("skip-worktree or intent-to-add entries");
        }
        let targets: Vec<&StatusEntry> =
            status.iter().filter(|e| e.worktree != ' ' && !(tracked_only && e.index == '?') && any_matches(specs, e.path.as_bytes())).collect();
        if targets.iter().any(|e| e.path.contains('\u{fffd}')) {
            return unsupported("path that is not UTF-8");
        }
        let status_paths: Vec<&[u8]> = status.iter().map(|e| e.path.as_bytes()).collect();
        every_spec_names_something(specs, &index, &status_paths)?;

        // Hash and store the files in parallel, then update the entries.
        let staged: Vec<(Vec<u8>, Option<Entry>)> = targets
            .par_iter()
            .map(|e| -> Result<(Vec<u8>, Option<Entry>)> {
                let path = e.path.as_bytes().to_vec();
                let abs = self.abs(&path);
                let meta = match std::fs::symlink_metadata(&abs) {
                    Ok(m) => m,
                    Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok((path, None)),
                    Err(err) => return Err(err.into()),
                };
                if meta.is_dir() {
                    // A directory where a tracked file was: the file is gone.
                    return Ok((path, None));
                }
                let is_link = meta.file_type().is_symlink();
                if !is_link && !meta.is_file() {
                    return unsupported("a FIFO, socket or device");
                }
                let data = if is_link { std::fs::read_link(&abs)?.as_os_str().as_bytes().to_vec() } else { std::fs::read(&abs)? };
                let oid = self.odb.write(Kind::Blob, &data)?;
                let mode = if is_link {
                    MODE_LINK
                } else if meta.permissions().mode() & 0o100 != 0 {
                    MODE_EXEC
                } else {
                    MODE_FILE
                };
                Ok((path.clone(), Some(Entry { stat: Stat::from_metadata(&meta), mode, oid, flags: 0, xflags: 0, path: path.into_boxed_slice() })))
            })
            .collect::<Result<_>>()?;
        if staged.is_empty() {
            return Ok(());
        }
        for (path, entry) in staged {
            match (entry, index.find(&path)) {
                (Some(e), Ok(i)) => index.entries[i] = e,
                (Some(e), Err(i)) => index.entries.insert(i, e),
                (None, Ok(i)) => {
                    index.entries.remove(i);
                }
                (None, Err(_)) => {}
            }
            if let Some(tree) = index.cache_tree.as_mut() {
                tree.invalidate(&path);
            }
        }
        index.entries_changed();
        self.store_index(&index, lock)
    }

    /// `git reset HEAD -- <paths>` and `git reset`: make the index match HEAD
    /// for the given paths (all when `specs` is `None`), files untouched.
    pub fn unstage(&self, specs: Option<&[String]>) -> Result<()> {
        self.nothing_in_progress()?;
        let lock = IndexLock::acquire(&self.l)?;
        let (mut index, _) = self.load_index()?;
        let head_tree = match refs::read_head(&self.l)?.oid() {
            Some(c) => Some(self.tree_of(&c)?),
            None => None,
        };
        let staged = staged_changes(self.odb, &index, head_tree.as_ref())?;
        if let Some(specs) = specs {
            let staged_paths: Vec<&[u8]> = staged.iter().map(|s| s.path.as_slice()).collect();
            every_spec_names_something(specs, &index, &staged_paths)?;
        }
        let wanted: Vec<(Vec<u8>, Slot)> = staged.into_iter().filter(|s| specs.is_none_or(|sp| any_matches(sp, &s.path))).map(|s| (s.path, s.old)).collect();
        if wanted.is_empty() {
            return Ok(());
        }
        self.set_entries(&mut index, &wanted)?;
        self.store_index(&index, lock)
    }

    /// `git apply --cached [--reverse] <patch>`: stage (or unstage) single
    /// hunks and lines. Only the index changes.
    pub fn apply_cached(&self, patch: &[u8], reverse: bool) -> Result<()> {
        let files = super::patch::parse(patch)?;
        let lock = IndexLock::acquire(&self.l)?;
        let (mut index, _) = self.load_index()?;
        if index.has_conflicts() {
            return unsupported("unmerged entries in the index");
        }
        let mut wanted: Vec<(Vec<u8>, Slot)> = Vec::new();
        for f in &files {
            if f.creates || f.deletes {
                return unsupported("patch adds or deletes a whole file");
            }
            let Ok(i) = index.find(f.path.as_bytes()) else { return unsupported("patched file is not tracked") };
            let e = &index.entries[i];
            if e.xflags != 0 || (e.mode != MODE_FILE && e.mode != MODE_EXEC) {
                return unsupported("patched path is not a plain tracked file");
            }
            let (_, data) = self.odb.read(&e.oid)?;
            let patched = super::patch::apply(&data, &f.hunks, reverse)?;
            wanted.push((f.path.as_bytes().to_vec(), Some((e.mode, self.odb.write(Kind::Blob, &patched)?))));
        }
        self.set_entries(&mut index, &wanted)?;
        self.store_index(&index, lock)
    }

    /// `git rm --cached --force -- <files>`: forget files, keep them on disk.
    pub fn remove_cached(&self, paths: &[String]) -> Result<()> {
        let lock = IndexLock::acquire(&self.l)?;
        let (mut index, _) = self.load_index()?;
        if index.has_conflicts() {
            return unsupported("unmerged entries in the index");
        }
        for p in paths {
            match index.find(p.as_bytes()) {
                Ok(i) => {
                    index.entries.remove(i);
                }
                // Directories need `-r`, unknown paths are an error: git's.
                Err(_) => return unsupported("not a tracked file"),
            }
            if let Some(tree) = index.cache_tree.as_mut() {
                tree.invalidate(p.as_bytes());
            }
        }
        index.entries_changed();
        self.store_index(&index, lock)
    }

    /// Write index entries back to the worktree where the file differs.
    fn restore_files(&self, index: &mut Index, index_mtime: (u32, u32), which: &(dyn Fn(&Entry) -> bool + Sync)) -> Result<usize> {
        let root = &self.l.worktree;
        let restored: Vec<(usize, Stat)> = index
            .entries
            .par_iter()
            .enumerate()
            .filter(|(_, e)| which(e))
            .map(|(i, e)| -> Result<Option<(usize, Stat)>> {
                if e.mode == MODE_GITLINK || e.xflags != 0 {
                    return unsupported("submodule, skip-worktree or intent-to-add entry");
                }
                let abs = self.abs(&e.path);
                match std::fs::symlink_metadata(&abs) {
                    Ok(m) if m.is_dir() => return unsupported("a directory is where a file is needed"),
                    Ok(m) => {
                        if entry_unchanged(root, e, index_mtime)? {
                            // Content is right; record the stat if the entry had none.
                            return Ok((e.stat == Stat::default()).then(|| (i, Stat::from_metadata(&m))));
                        }
                    }
                    // A deleted file comes back.
                    Err(_) => {}
                }
                if let Some(dir) = abs.parent() {
                    std::fs::create_dir_all(dir)?;
                }
                let (kind, data) = self.odb.read(&e.oid)?;
                if kind != Kind::Blob {
                    bail!("{} is not a blob", String::from_utf8_lossy(&e.path));
                }
                let meta = write_file(&abs, e.mode, &data).with_context(|| format!("writing {}", abs.display()))?;
                Ok(Some((i, Stat::from_metadata(&meta))))
            })
            .filter_map(|r| r.transpose())
            .collect::<Result<_>>()?;
        for (i, stat) in &restored {
            index.entries[*i].stat = *stat;
        }
        Ok(restored.len())
    }

    /// `git checkout -- <paths>`: throw away unstaged changes.
    pub fn discard_unstaged(&self, specs: &[String]) -> Result<()> {
        let lock = IndexLock::acquire(&self.l)?;
        let (mut index, index_mtime) = self.load_index()?;
        self.content_guard(&index)?;
        every_spec_names_something(specs, &index, &[])?;
        if self.restore_files(&mut index, index_mtime, &|e| any_matches(specs, &e.path))? > 0 {
            self.store_index(&index, lock)?;
        }
        Ok(())
    }

    /// `git reset --soft|--mixed|--hard <commit>`.
    pub fn reset(&self, mode: ResetMode, target: &Oid, label: &str) -> Result<()> {
        self.nothing_in_progress()?;
        self.no_hooks(&["reference-transaction", "post-checkout"])?;
        let head = refs::read_head(&self.l)?;
        let Some(old) = head.oid() else { return unsupported("unborn branch") };
        let target_tree = self.tree_of(target)?;
        let who = self.reflog_ident(super::ops::now());
        let message = format!("reset: moving to {label}");

        if mode != ResetMode::Soft {
            let lock = IndexLock::acquire(&self.l)?;
            let (mut index, index_mtime) = self.load_index()?;
            if mode == ResetMode::Hard {
                self.content_guard(&index)?;
            } else if index.has_conflicts() {
                return unsupported("unmerged entries in the index");
            }
            // What the index has that the target tree does not, and the
            // other way round.
            let differing = staged_changes(self.odb, &index, Some(&target_tree))?;
            let wanted: Vec<(Vec<u8>, Slot)> = differing.iter().map(|s| (s.path.clone(), s.old)).collect();
            if mode == ResetMode::Hard {
                // Files that have to disappear: tracked now, not in the target.
                for s in differing.iter().filter(|s| s.old.is_none()) {
                    let abs = self.abs(&s.path);
                    match std::fs::remove_file(&abs) {
                        Ok(()) => {}
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                        Err(_) if abs.is_dir() => return unsupported("a directory is where a file was"),
                        Err(e) => return Err(e.into()),
                    }
                    let mut dir = abs.parent();
                    while let Some(d) = dir {
                        if d == self.l.worktree || std::fs::remove_dir(d).is_err() {
                            break;
                        }
                        dir = d.parent();
                    }
                }
            }
            self.set_entries(&mut index, &wanted)?;
            let mut changed = !wanted.is_empty();
            if mode == ResetMode::Hard {
                // Every tracked file back to its recorded content.
                changed |= self.restore_files(&mut index, index_mtime, &|_| true)? > 0;
            }
            if changed {
                self.store_index(&index, lock)?;
            }
        }
        refs::set_orig_head(&self.l, &old)?;
        if old != *target {
            self.move_head(&head, target, &who, &message)?;
        } else {
            refs::log_head(&self.l, Some(&old), target, &who, &message)?;
        }
        Ok(())
    }

    /// A new branch at `base`. With `switch`, also check it out.
    pub fn create_branch(&self, name: &str, base: &Oid, base_label: &str, switch: bool) -> Result<()> {
        self.no_hooks(&["reference-transaction", "post-checkout"])?;
        valid_ref_name(name)?;
        let refname = format!("refs/heads/{name}");
        for existing in view::list_refs(&self.l, "refs/heads/")?.keys() {
            // Exists already, or one name would be a directory of the other.
            if *existing == refname || existing.starts_with(&format!("{refname}/")) || refname.starts_with(&format!("{existing}/")) {
                return unsupported("branch name is taken");
            }
        }
        let who = self.reflog_ident(super::ops::now());
        refs::update_ref(&self.l, &refname, base, None, &who, &format!("branch: Created from {base_label}"))?;
        if switch {
            if let Err(e) = self.switch_branch(name) {
                // Leave no trace of a branch that could not be checked out.
                let _ = std::fs::remove_file(self.l.common_dir.join(&refname));
                let _ = std::fs::remove_file(self.l.common_dir.join("logs").join(&refname));
                return Err(e);
            }
        }
        Ok(())
    }

    /// A new lightweight tag.
    pub fn create_tag(&self, name: &str, target: &Oid) -> Result<()> {
        self.no_hooks(&["reference-transaction"])?;
        valid_ref_name(name)?;
        let refname = format!("refs/tags/{name}");
        for existing in view::list_refs(&self.l, "refs/tags/")?.keys() {
            if *existing == refname || existing.starts_with(&format!("{refname}/")) || refname.starts_with(&format!("{existing}/")) {
                return unsupported("tag name is taken");
            }
        }
        refs::create_ref(&self.l, &refname, target)
    }

    /// The current commit, for callers that only need to know there is one.
    pub fn head_commit(&self) -> Result<Option<Oid>> {
        Ok(match refs::read_head(&self.l)? {
            Head::Branch(_, oid) => oid,
            Head::Detached(oid) => Some(oid),
        })
    }
}

/// A conservative subset of what `git check-ref-format` accepts.
fn valid_ref_name(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name.chars().all(|c| c.is_ascii_alphanumeric() || "._/-+#=".contains(c))
        && !name.starts_with(['-', '/', '.'])
        && !name.ends_with(['/', '.'])
        && !name.contains("..")
        && !name.contains("//")
        && !name.contains("/.")
        && !name.ends_with(".lock")
        && !name.split('/').any(|c| c.ends_with(".lock"))
        && name != "HEAD";
    if ok {
        Ok(())
    } else {
        unsupported("ref name")
    }
}
