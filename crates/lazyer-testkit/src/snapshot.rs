//! The observable end state of a repository, taken with real git. Two
//! snapshots being equal is the acceptance criterion for every operation.

use crate::git::{git, output, run};
use anyhow::Result;
use std::hash::Hasher;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeSnap {
    pub name: String,
    /// `ref: refs/heads/x` or a commit id.
    pub head: String,
    /// `git ls-files -s` records: mode, object id, stage, path.
    pub index: Vec<String>,
    /// Every file below the worktree: path, kind, length, content hash.
    pub files: Vec<String>,
    /// `git status --porcelain=v2 --branch --untracked-files=all`.
    pub status: Vec<String>,
    /// Leftovers of an unfinished operation (rebase, merge, lock files).
    pub in_progress: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    /// Every ref with its target.
    pub refs: Vec<String>,
    pub stashes: Vec<String>,
    pub worktrees: Vec<WorktreeSnap>,
}

pub fn snapshot(repo: &Path, worktrees: &[PathBuf]) -> Result<Snapshot> {
    let refs = run(repo, &["for-each-ref", "--format=%(refname) %(objectname)"])?.lines().map(str::to_string).collect();
    let stashes = run(repo, &["stash", "list", "--format=%H %gs"])?.lines().map(str::to_string).collect();
    let mut snaps = Vec::new();
    for wt in worktrees {
        let head = match run(wt, &["symbolic-ref", "-q", "HEAD"]) {
            Ok(r) => format!("ref: {r}"),
            Err(_) => run(wt, &["rev-parse", "HEAD"])?,
        };
        let index = zsplit(&output(git(wt).args(["ls-files", "-s", "-z"]))?);
        let mut files = Vec::new();
        walk(wt, wt, &mut files)?;
        files.sort();
        let status = zsplit(&output(git(wt).args(["status", "--porcelain=v2", "-z", "--branch", "--untracked-files=all"]))?);
        let gitdir = PathBuf::from(run(wt, &["rev-parse", "--absolute-git-dir"])?);
        let mut in_progress = Vec::new();
        for name in ["rebase-merge", "rebase-apply", "MERGE_HEAD", "CHERRY_PICK_HEAD", "REVERT_HEAD", "BISECT_LOG", "index.lock", "HEAD.lock"] {
            if gitdir.join(name).exists() {
                in_progress.push(name.to_string());
            }
        }
        snaps.push(WorktreeSnap { name: wt.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default(), head, index, files, status, in_progress });
    }
    Ok(Snapshot { refs, stashes, worktrees: snaps })
}

/// `git fsck` on the object store and refs.
pub fn fsck(repo: &Path) -> Result<()> {
    run(repo, &["fsck", "--strict", "--no-dangling", "--no-progress"]).map(|_| ())
}

fn zsplit(bytes: &[u8]) -> Vec<String> {
    bytes.split(|b| *b == 0).filter(|s| !s.is_empty()).map(|s| String::from_utf8_lossy(s).to_string()).collect()
}

fn walk(root: &Path, dir: &Path, out: &mut Vec<String>) -> Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if dir == root && entry.file_name() == ".git" {
            continue;
        }
        let meta = std::fs::symlink_metadata(&path)?;
        let rel = path.strip_prefix(root)?.to_string_lossy().to_string();
        if meta.file_type().is_symlink() {
            out.push(format!("{rel}\tl\t{}", std::fs::read_link(&path)?.to_string_lossy()));
        } else if meta.is_dir() {
            let before = out.len();
            walk(root, &path, out)?;
            if out.len() == before {
                out.push(format!("{rel}\td\tempty"));
            }
        } else {
            use std::os::unix::fs::PermissionsExt;
            let kind = if meta.permissions().mode() & 0o111 != 0 { 'x' } else { 'f' };
            let data = std::fs::read(&path)?;
            let mut h = std::collections::hash_map::DefaultHasher::new();
            h.write(&data);
            out.push(format!("{rel}\t{kind}\t{}\t{:016x}", data.len(), h.finish()));
        }
    }
    Ok(())
}

/// Human-readable differences, empty when the snapshots are equal.
pub fn differences(reference: &Snapshot, candidate: &Snapshot) -> Vec<String> {
    let mut out = Vec::new();
    list_diff("refs", &reference.refs, &candidate.refs, &mut out);
    list_diff("stashes", &reference.stashes, &candidate.stashes, &mut out);
    for (a, b) in reference.worktrees.iter().zip(&candidate.worktrees) {
        let w = &a.name;
        if a.head != b.head {
            out.push(format!("{w}: HEAD reference `{}` candidate `{}`", a.head, b.head));
        }
        list_diff(&format!("{w}: index"), &a.index, &b.index, &mut out);
        list_diff(&format!("{w}: files"), &a.files, &b.files, &mut out);
        list_diff(&format!("{w}: status"), &a.status, &b.status, &mut out);
        list_diff(&format!("{w}: in-progress"), &a.in_progress, &b.in_progress, &mut out);
    }
    if reference.worktrees.len() != candidate.worktrees.len() {
        out.push("different number of worktrees".into());
    }
    out
}

fn list_diff(what: &str, a: &[String], b: &[String], out: &mut Vec<String>) {
    if a == b {
        return;
    }
    let sa: std::collections::BTreeSet<_> = a.iter().collect();
    let sb: std::collections::BTreeSet<_> = b.iter().collect();
    let only_a: Vec<_> = sa.difference(&sb).take(6).collect();
    let only_b: Vec<_> = sb.difference(&sa).take(6).collect();
    out.push(format!(
        "{what}: {} only in reference, {} only in candidate (order differs: {})\n    reference: {only_a:?}\n    candidate: {only_b:?}",
        sa.difference(&sb).count(),
        sb.difference(&sa).count(),
        sa == sb
    ));
}
