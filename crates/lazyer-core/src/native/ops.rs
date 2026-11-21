//! The mutating operations, on top of objects, refs and the index.
//!
//! Rewrites of history (`amend_older`, `move_patch_to_index`) are computed on
//! trees alone. Git performs them as an interactive rebase that checks out
//! every commit in turn; here no file in the worktree is read or written,
//! because the end state of the worktree is known in advance.

use super::checkout::{apply_switch, attributes_need_git, plan_switch, Plan};
use super::commitobj::{cleanup_message, ident_line, RawCommit};
use super::index::Index;
use super::odb::sha1;
use super::refs::{self, Head, IndexLock, Layout};
use super::tree::{hex, lookup, merge_trees, unhex, with_path, write_index_tree, Kind, Objects, Oid, EMPTY_TREE};
use crate::api::{unsupported, Ident, Result};
use anyhow::{bail, Context};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

/// The configuration the operations depend on.
#[derive(Clone, Debug, Default)]
pub struct Settings {
    /// Something that changes file content on checkout or commit is
    /// configured (autocrlf, a sparse checkout, no symlink support, ...).
    pub content_needs_git: Option<String>,
    pub gpg_sign: bool,
    pub hooks_dir: PathBuf,
    /// `index.skipHash`: write zeros instead of a checksum.
    pub skip_hash: bool,
    /// `user.name` and `user.email`, for reflog entries.
    pub user: Option<(String, String)>,
    /// `core.excludesFile` or its default location.
    pub excludes_file: Option<PathBuf>,
    pub ignore_case: bool,
}

pub struct Repo<'a> {
    pub l: Layout,
    pub odb: &'a dyn Objects,
    pub settings: &'a Settings,
}

impl Repo<'_> {
    pub(crate) fn load_index(&self) -> Result<(Index, (u32, u32))> {
        use std::os::unix::fs::MetadataExt;
        let path = self.l.git_dir.join("index");
        let meta = std::fs::metadata(&path).context("reading the index")?;
        Ok((Index::read(&path)?, (meta.mtime() as u32, meta.mtime_nsec() as u32)))
    }

    pub(crate) fn store_index(&self, index: &Index, lock: IndexLock) -> Result<()> {
        let mut bytes = index.serialize();
        let checksum = if self.settings.skip_hash && !index.had_checksum { [0u8; 20] } else { sha1(&bytes) };
        bytes.extend_from_slice(&checksum);
        lock.commit(&bytes)
    }

    /// Hooks run arbitrary programs at defined points; an operation that
    /// would trigger one is git's.
    pub(crate) fn no_hooks(&self, names: &[&str]) -> Result<()> {
        for name in names {
            if let Ok(meta) = std::fs::metadata(self.settings.hooks_dir.join(name)) {
                if meta.is_file() && meta.permissions().mode() & 0o111 != 0 {
                    return unsupported(format!("{name} hook"));
                }
            }
        }
        Ok(())
    }

    pub(crate) fn nothing_in_progress(&self) -> Result<()> {
        for name in ["MERGE_HEAD", "CHERRY_PICK_HEAD", "REVERT_HEAD", "rebase-merge", "rebase-apply", "BISECT_LOG"] {
            if self.l.git_dir.join(name).exists() {
                return unsupported(format!("{name} in progress"));
            }
        }
        Ok(())
    }

    fn commit_of(&self, oid: &Oid) -> Result<RawCommit> {
        let (kind, data) = self.odb.read(oid)?;
        if kind != Kind::Commit {
            bail!("{} is not a commit", hex(oid));
        }
        RawCommit::parse(&data)
    }

    pub(crate) fn tree_of(&self, commit: &Oid) -> Result<Oid> {
        unhex(&self.commit_of(commit)?.tree)
    }

    pub(crate) fn reflog_ident(&self, time: i64) -> Ident {
        let (name, email) = self.settings.user.clone().unwrap_or_else(|| ("lazyerGit".into(), "lazyergit@localhost".into()));
        Ident { name, email, time, tz: "+0000".into() }
    }

    /// Move the current branch (or a detached HEAD) to `new`.
    pub(crate) fn move_head(&self, head: &Head, new: &Oid, who: &Ident, message: &str) -> Result<()> {
        match head {
            Head::Branch(name, old) => {
                refs::update_ref(&self.l, name, new, old.as_ref(), who, message)?;
                refs::log_head(&self.l, old.as_ref(), new, who, message)
            }
            Head::Detached(old) => {
                refs::set_head_detached(&self.l, new)?;
                refs::log_head(&self.l, Some(old), new, who, message)
            }
        }
    }

    pub fn commit(&self, message: &str, author: &Ident, committer: &Ident) -> Result<()> {
        self.no_hooks(&["pre-commit", "prepare-commit-msg", "commit-msg", "post-commit", "reference-transaction"])?;
        self.nothing_in_progress()?;
        if self.settings.gpg_sign {
            return unsupported("commit signing");
        }
        let lock = IndexLock::acquire(&self.l)?;
        let (mut index, _) = self.load_index()?;
        let cache_was_valid = index.cache_tree.as_ref().is_some_and(|t| t.entry_count >= 0);
        let tree = write_index_tree(self.odb, &mut index)?;
        let head = refs::read_head(&self.l)?;
        let parent = head.oid();
        let parent_tree = match &parent {
            Some(p) => self.tree_of(p)?,
            None => EMPTY_TREE,
        };
        if tree == parent_tree {
            bail!("nothing to commit");
        }
        let message = cleanup_message(message);
        if message.is_empty() {
            bail!("aborting commit due to empty commit message");
        }
        let commit = RawCommit {
            tree: hex(&tree),
            parents: parent.iter().map(hex).collect(),
            author: ident_line(author),
            committer: ident_line(committer),
            extra: Vec::new(),
            message: message.into_bytes(),
        };
        let oid = self.odb.write(Kind::Commit, &commit.serialize())?;
        let what = if parent.is_some() { "commit" } else { "commit (initial)" };
        self.move_head(&head, &oid, committer, &format!("{what}: {}", commit.subject()))?;
        if !cache_was_valid {
            self.store_index(&index, lock)?;
        }
        Ok(())
    }

    pub fn amend_head(&self, committer: &Ident) -> Result<()> {
        self.no_hooks(&["pre-commit", "prepare-commit-msg", "commit-msg", "post-commit", "post-rewrite", "reference-transaction"])?;
        self.nothing_in_progress()?;
        if self.settings.gpg_sign {
            return unsupported("commit signing");
        }
        let head = refs::read_head(&self.l)?;
        let Some(tip) = head.oid() else { bail!("nothing to amend") };
        let old = self.commit_of(&tip)?;
        if old.is_signed() || !old.extra.is_empty() {
            return unsupported("commit with extra headers");
        }
        let lock = IndexLock::acquire(&self.l)?;
        let (mut index, _) = self.load_index()?;
        let cache_was_valid = index.cache_tree.as_ref().is_some_and(|t| t.entry_count >= 0);
        let tree = write_index_tree(self.odb, &mut index)?;
        let new = RawCommit { tree: hex(&tree), committer: ident_line(committer), ..old };
        let oid = self.odb.write(Kind::Commit, &new.serialize())?;
        self.move_head(&head, &oid, committer, &format!("commit (amend): {}", new.subject()))?;
        if !cache_was_valid {
            self.store_index(&index, lock)?;
        }
        Ok(())
    }

    /// The commits above `target` up to `tip`, oldest first.
    fn chain_above(&self, target: &Oid, tip: &Oid) -> Result<Vec<RawCommit>> {
        let mut chain = Vec::new();
        let mut at = *tip;
        while at != *target {
            let c = self.commit_of(&at)?;
            if c.parents.len() != 1 {
                return unsupported("a merge or root commit between the target and HEAD");
            }
            if c.is_signed() || !c.extra.is_empty() {
                return unsupported("commit with extra headers");
            }
            at = unhex(&c.parents[0])?;
            chain.push(c);
            if chain.len() > 5000 {
                bail!("{} is not an ancestor of HEAD", hex(target));
            }
        }
        chain.reverse();
        Ok(chain)
    }

    /// Re-create `chain` on top of `parent`, whose tree is `parent_tree`.
    /// Every commit keeps its author and message and gets the new committer.
    fn replay(&self, chain: &[RawCommit], mut parent: Oid, mut parent_tree: Oid, committer: &Ident) -> Result<(Oid, Oid)> {
        for c in chain {
            let old_parent_tree = self.tree_of(&unhex(&c.parents[0])?)?;
            let own_tree = unhex(&c.tree)?;
            let tree = merge_trees(self.odb, Some(&old_parent_tree), Some(&parent_tree), Some(&own_tree))?.unwrap_or(EMPTY_TREE);
            if tree == parent_tree && own_tree != old_parent_tree {
                // What git does with a commit that becomes empty depends on options.
                return unsupported("a commit would become empty");
            }
            let new = RawCommit { tree: hex(&tree), parents: vec![hex(&parent)], committer: ident_line(committer), ..c.clone() };
            parent = self.odb.write(Kind::Commit, &new.serialize())?;
            parent_tree = tree;
        }
        Ok((parent, parent_tree))
    }

    fn rewrite_preconditions(&self) -> Result<()> {
        self.no_hooks(&[
            "pre-commit",
            "prepare-commit-msg",
            "commit-msg",
            "post-commit",
            "pre-rebase",
            "post-rewrite",
            "post-checkout",
            "reference-transaction",
        ])?;
        self.nothing_in_progress()?;
        if self.settings.gpg_sign {
            return unsupported("commit signing");
        }
        Ok(())
    }

    pub fn amend_older(&self, target: &str, committer: &Ident) -> Result<()> {
        self.rewrite_preconditions()?;
        let target = unhex(target)?;
        let head = refs::read_head(&self.l)?;
        let Some(tip) = head.oid() else { bail!("no commits yet") };
        let (mut index, _) = self.load_index()?;
        let staged = write_index_tree(self.odb, &mut index)?;
        let head_tree = self.tree_of(&tip)?;
        if staged == head_tree {
            bail!("nothing staged to amend with");
        }
        let chain = self.chain_above(&target, &tip)?;
        let old = self.commit_of(&target)?;
        if old.parents.len() != 1 || old.is_signed() || !old.extra.is_empty() {
            return unsupported("target is a root, merge or signed commit");
        }
        // Applying "HEAD -> staged" to the target is a merge of the staged
        // tree into the target's tree with HEAD's tree as the base.
        let target_tree = unhex(&old.tree)?;
        let new_target_tree = merge_trees(self.odb, Some(&head_tree), Some(&target_tree), Some(&staged))?.unwrap_or(EMPTY_TREE);
        let new_target = RawCommit { tree: hex(&new_target_tree), committer: ident_line(committer), ..old };
        let new_target_oid = self.odb.write(Kind::Commit, &new_target.serialize())?;
        let (new_tip, final_tree) = self.replay(&chain, new_target_oid, new_target_tree, committer)?;
        if final_tree != staged {
            return unsupported("the rewritten history does not end at the staged state");
        }
        refs::set_orig_head(&self.l, &tip)?;
        self.move_head(&head, &new_tip, committer, &format!("rebase (finish): amend {}", &hex(&target)[..7]))
        // The index already holds the final tree and the worktree was never
        // part of this: nothing else to do.
    }

    pub fn move_patch_to_index(&self, commit: &str, paths: &[String], committer: &Ident) -> Result<()> {
        self.rewrite_preconditions()?;
        let commit = unhex(commit)?;
        let head = refs::read_head(&self.l)?;
        let Some(tip) = head.oid() else { bail!("no commits yet") };
        let (mut index, index_mtime) = self.load_index()?;
        let head_tree = self.tree_of(&tip)?;
        if write_index_tree(self.odb, &mut index)? != head_tree {
            return unsupported("staged changes present");
        }
        let chain = self.chain_above(&commit, &tip)?;
        let old = self.commit_of(&commit)?;
        if old.parents.len() != 1 || old.is_signed() || !old.extra.is_empty() {
            return unsupported("target is a root, merge or signed commit");
        }
        let parent_tree = self.tree_of(&unhex(&old.parents[0])?)?;
        let old_tree = unhex(&old.tree)?;
        let mut new_tree = Some(old_tree);
        for path in paths {
            let p = path.as_bytes();
            let before = lookup(self.odb, &parent_tree, p)?;
            let after = lookup(self.odb, &old_tree, p)?;
            // The change has to arrive in index and worktree unaltered: no
            // later commit may have touched the file, and the file on disk
            // has to be the committed one.
            if lookup(self.odb, &head_tree, p)? != after {
                return unsupported("a later commit changes the same file");
            }
            if let Ok(i) = index.find(p) {
                if !super::checkout::entry_unchanged(&self.l.worktree, &index.entries[i], index_mtime)? {
                    return unsupported("the file has local changes");
                }
            }
            new_tree = with_path(self.odb, new_tree.as_ref(), p, before)?;
        }
        let new_tree = new_tree.unwrap_or(EMPTY_TREE);
        if new_tree == old_tree {
            bail!("the commit does not change the given paths");
        }
        let rewritten = RawCommit { tree: hex(&new_tree), committer: ident_line(committer), ..old };
        let rewritten_oid = self.odb.write(Kind::Commit, &rewritten.serialize())?;
        let (new_tip, _) = self.replay(&chain, rewritten_oid, new_tree, committer)?;
        refs::set_orig_head(&self.l, &tip)?;
        self.move_head(&head, &new_tip, committer, &format!("rebase (finish): move patch out of {}", &hex(&commit)[..7]))
        // Index and worktree still hold the old HEAD's content, which now
        // differs from the new HEAD by exactly the moved change: it is staged.
    }

    /// The checks and the plan for switching this worktree to `branch`.
    fn plan_branch_switch(&self, branch: &str, index: &Index, index_mtime: (u32, u32), from_commit: &Oid) -> Result<(Oid, Plan)> {
        if let Some(why) = &self.settings.content_needs_git {
            return unsupported(why.clone());
        }
        self.no_hooks(&["post-checkout", "reference-transaction"])?;
        self.nothing_in_progress()?;
        let refname = format!("refs/heads/{branch}");
        let Some(target) = refs::resolve(&self.l, &refname)? else { return unsupported("not a local branch") };
        if attributes_need_git(self.odb, index)? {
            return unsupported(".gitattributes rules that change file content");
        }
        let plan = plan_switch(&self.l, self.odb, index, index_mtime, &self.tree_of(from_commit)?, &self.tree_of(&target)?)?;
        Ok((target, plan))
    }

    fn finish_switch(&self, mut index: Index, lock: IndexLock, plan: &Plan, old: &Oid, old_name: &str, target: &Oid, branch: &str) -> Result<()> {
        let mut t = super::Trace::start();
        if !plan.is_empty() {
            apply_switch(&self.l, self.odb, &mut index, plan)?;
            t.step("apply_switch");
            // Like git, leave a fully valid cache-tree behind.
            if index.entries.iter().all(|e| e.xflags == 0) {
                write_index_tree(self.odb, &mut index)?;
            }
            t.step("write_index_tree");
            self.store_index(&index, lock)?;
            t.step("store_index");
        } else {
            drop(lock);
        }
        refs::set_head_to_branch(&self.l, &format!("refs/heads/{branch}"))?;
        let who = self.reflog_ident(now());
        refs::log_head(&self.l, Some(old), target, &who, &format!("checkout: moving from {old_name} to {branch}"))
    }

    pub fn switch_branch(&self, branch: &str) -> Result<()> {
        let head = refs::read_head(&self.l)?;
        let Some(old) = head.oid() else { return unsupported("unborn branch") };
        let refname = format!("refs/heads/{branch}");
        if matches!(&head, Head::Branch(name, _) if *name == refname) {
            return Ok(());
        }
        for (root, git_dir) in refs::worktrees(&self.l)? {
            if git_dir != self.l.git_dir {
                let other = Layout { worktree: root.clone(), git_dir, common_dir: self.l.common_dir.clone() };
                if matches!(refs::read_head(&other), Ok(Head::Branch(name, _)) if name == refname) {
                    bail!("'{branch}' is already used by worktree at '{}'", root.display());
                }
            }
        }
        let mut t = super::Trace::start();
        let lock = IndexLock::acquire(&self.l)?;
        let (index, index_mtime) = self.load_index()?;
        t.step("load_index");
        let (target, plan) = self.plan_branch_switch(branch, &index, index_mtime, &old)?;
        t.step(&format!("plan ({}w {}d)", plan.writes.len(), plan.deletes.len()));
        self.finish_switch(index, lock, &plan, &old, &head_name(&head), &target, branch)
    }

    pub fn swap_branch(&self, branch: &str, detach_other: bool) -> Result<()> {
        let refname = format!("refs/heads/{branch}");
        let head = refs::read_head(&self.l)?;
        let Some(old) = head.oid() else { return unsupported("unborn branch") };
        let mut other = None;
        for (root, git_dir) in refs::worktrees(&self.l)? {
            let l = Layout { worktree: root, git_dir, common_dir: self.l.common_dir.clone() };
            if matches!(refs::read_head(&l), Ok(Head::Branch(name, _)) if name == refname) {
                other = Some(l);
            }
        }
        let Some(other) = other else { bail!("{branch} is not checked out in another worktree; nothing to swap") };
        if other.git_dir == self.l.git_dir {
            return Ok(());
        }
        let there = Repo { l: other, odb: self.odb, settings: self.settings };
        let Some(branch_tip) = refs::resolve(&self.l, &refname)? else { bail!("{branch} has no commit") };

        // Plan both sides before changing anything, so that declining leaves
        // the repository as it was.
        let lock_here = IndexLock::acquire(&self.l)?;
        let lock_there = IndexLock::acquire(&there.l)?;
        let (index, index_mtime) = self.load_index()?;
        let (target, plan_here) = self.plan_branch_switch(branch, &index, index_mtime, &old)?;
        let give_back = match (&head, detach_other) {
            (Head::Branch(name, _), false) => name.strip_prefix("refs/heads/").map(str::to_string),
            _ => None,
        };
        let (their_index, their_mtime) = there.load_index()?;
        let plan_there = match &give_back {
            Some(current) => Some(there.plan_branch_switch(current, &their_index, their_mtime, &branch_tip)),
            None => None,
        };
        // If the other worktree cannot take this branch (local changes in
        // the way, or something that needs git), decline before touching
        // anything: git then does the swap step by step and reports where
        // it stopped.
        if let Some(Err(e)) = &plan_there {
            return unsupported(format!("other worktree: {e}"));
        }

        let who = self.reflog_ident(now());
        refs::set_head_detached(&there.l, &branch_tip)?;
        refs::log_head(&there.l, Some(&branch_tip), &branch_tip, &who, &format!("checkout: moving from {branch} to {}", hex(&branch_tip)))?;
        self.finish_switch(index, lock_here, &plan_here, &old, &head_name(&head), &target, branch)?;
        match (give_back, plan_there) {
            (Some(current), Some(Ok((their_target, plan)))) => {
                there.finish_switch(their_index, lock_there, &plan, &branch_tip, &hex(&branch_tip), &their_target, &current)
            }
            (Some(current), Some(Err(e))) => Err(e.context(format!("could not switch the other worktree to {current}; it is left detached"))),
            _ => Ok(()),
        }
    }
}

fn head_name(head: &Head) -> String {
    match head {
        Head::Branch(name, _) => name.strip_prefix("refs/heads/").unwrap_or(name).to_string(),
        Head::Detached(oid) => hex(oid),
    }
}

pub(crate) fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}
