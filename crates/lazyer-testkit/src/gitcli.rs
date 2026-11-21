//! The reference backend: real git, driven with the command sequences lazygit
//! uses for each operation (minus lazygit's own refresh, which is measured
//! separately end to end).

use crate::git::{git_with, output, real_git};
use anyhow::{bail, Context, Result};
use lazyer_core::{Backend, BranchInfo, CommitInfo, Ident, RepoView, StashInfo, Status, StatusEntry, WorktreeInfo, LOG_LIMIT};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};

pub struct GitCli {
    pub bin: PathBuf,
    pub label: String,
}

impl GitCli {
    /// The git binary itself.
    pub fn real() -> Self {
        GitCli { bin: real_git().to_path_buf(), label: "git".into() }
    }

    /// `/usr/bin/git`, Apple's shim that looks up the real binary on every call.
    pub fn shim() -> Self {
        GitCli { bin: PathBuf::from("/usr/bin/git"), label: "git-shim".into() }
    }

    fn g(&self, wt: &Path) -> Command {
        git_with(&self.bin, wt)
    }

    fn text(&self, wt: &Path, args: &[&str]) -> Result<String> {
        Ok(String::from_utf8_lossy(&output(self.g(wt).args(args))?).trim_end().to_string())
    }

    fn committing(&self, wt: &Path, author: Option<&Ident>, committer: &Ident) -> Command {
        let mut c = self.g(wt);
        if let Some(a) = author {
            c.env("GIT_AUTHOR_NAME", &a.name).env("GIT_AUTHOR_EMAIL", &a.email).env("GIT_AUTHOR_DATE", a.git_date());
        }
        c.env("GIT_COMMITTER_NAME", &committer.name)
            .env("GIT_COMMITTER_EMAIL", &committer.email)
            .env("GIT_COMMITTER_DATE", committer.git_date())
            .env("GIT_EDITOR", "true");
        c
    }
}

fn parse_track(upstream: &str, track: &str) -> Option<(u32, u32)> {
    if upstream.is_empty() || track == "[gone]" {
        return None;
    }
    let inner = track.trim_matches(|c| c == '[' || c == ']');
    let (mut ahead, mut behind) = (0, 0);
    for part in inner.split(", ") {
        if let Some(n) = part.strip_prefix("ahead ") {
            ahead = n.parse().unwrap_or(0);
        } else if let Some(n) = part.strip_prefix("behind ") {
            behind = n.parse().unwrap_or(0);
        }
    }
    Some((ahead, behind))
}

pub fn parse_status_v2(bytes: &[u8]) -> Result<Status> {
    let mut st = Status::default();
    let mut records = bytes.split(|b| *b == 0).map(|r| String::from_utf8_lossy(r).to_string());
    while let Some(rec) = records.next() {
        if rec.is_empty() {
            continue;
        }
        if let Some(h) = rec.strip_prefix("# ") {
            if let Some(v) = h.strip_prefix("branch.oid ") {
                st.head_oid = v.to_string();
            } else if let Some(v) = h.strip_prefix("branch.head ") {
                st.branch = (v != "(detached)").then(|| v.to_string());
            } else if let Some(v) = h.strip_prefix("branch.upstream ") {
                st.upstream = Some(v.to_string());
            } else if let Some(v) = h.strip_prefix("branch.ab ") {
                let mut it = v.split(' ');
                let a = it.next().and_then(|s| s.trim_start_matches('+').parse().ok()).unwrap_or(0);
                let b = it.next().and_then(|s| s.trim_start_matches('-').parse().ok()).unwrap_or(0);
                st.ahead_behind = Some((a, b));
            }
            continue;
        }
        let kind = rec.as_bytes()[0];
        let xy = |s: &str| {
            let b = s.as_bytes();
            let f = |c: u8| if c == b'.' { ' ' } else { c as char };
            (f(b[0]), f(b[1]))
        };
        match kind {
            b'1' => {
                let f: Vec<&str> = rec.splitn(9, ' ').collect();
                let (x, y) = xy(f[1]);
                st.entries.push(StatusEntry { path: f[8].to_string(), index: x, worktree: y, orig_path: None });
            }
            b'2' => {
                let f: Vec<&str> = rec.splitn(10, ' ').collect();
                let (x, y) = xy(f[1]);
                let orig = records.next().context("rename record without source path")?;
                st.entries.push(StatusEntry { path: f[9].to_string(), index: x, worktree: y, orig_path: Some(orig) });
            }
            b'u' => {
                let f: Vec<&str> = rec.splitn(11, ' ').collect();
                let (x, y) = xy(f[1]);
                st.entries.push(StatusEntry { path: f[10].to_string(), index: x, worktree: y, orig_path: None });
            }
            b'?' | b'!' => {
                let c = kind as char;
                st.entries.push(StatusEntry { path: rec[2..].to_string(), index: c, worktree: c, orig_path: None });
            }
            _ => bail!("unexpected status record: {rec}"),
        }
    }
    st.entries.sort();
    Ok(st)
}

impl Backend for GitCli {
    fn name(&self) -> &str {
        &self.label
    }

    fn status(&self, wt: &Path) -> Result<Status> {
        let out = output(self.g(wt).args(["status", "--porcelain=v2", "-z", "--branch", "--untracked-files=all", "--find-renames=50%"]))?;
        parse_status_v2(&out)
    }

    fn load_view(&self, wt: &Path) -> Result<RepoView> {
        let mut view = RepoView { status: self.status(wt)?, ..Default::default() };

        let fmt = "--format=%(HEAD)%00%(refname:short)%00%(upstream:short)%00%(upstream:track)%00%(subject)%00%(objectname)%00%(committerdate:unix)";
        for line in self.text(wt, &["for-each-ref", "--sort=-committerdate", fmt, "refs/heads"])?.lines() {
            let f: Vec<&str> = line.split('\0').collect();
            if f.len() < 7 {
                continue;
            }
            view.branches.push(BranchInfo {
                name: f[1].to_string(),
                oid: f[5].to_string(),
                is_head: f[0] == "*",
                upstream: (!f[2].is_empty()).then(|| f[2].to_string()),
                ahead_behind: parse_track(f[2], f[3]),
                subject: f[4].to_string(),
                committer_time: f[6].parse().unwrap_or(0),
            });
        }
        view.branches.sort();

        for line in self.text(wt, &["for-each-ref", "--format=%(refname:short)%00%(objectname)", "refs/remotes"])?.lines() {
            if let Some((n, o)) = line.split_once('\0') {
                view.remote_branches.push((n.to_string(), o.to_string()));
            }
        }
        view.remote_branches.sort();
        for line in self.text(wt, &["for-each-ref", "--format=%(refname:short)%00%(objectname)", "refs/tags"])?.lines() {
            if let Some((n, o)) = line.split_once('\0') {
                view.tags.push((n.to_string(), o.to_string()));
            }
        }
        view.tags.sort();

        let limit = format!("-{LOG_LIMIT}");
        let log =
            self.text(wt, &["log", "HEAD", "--topo-order", "--pretty=format:%H%x00%at%x00%aN%x00%ae%x00%P%x00%s", &limit, "--no-show-signature", "--"])?;
        for line in log.lines() {
            let f: Vec<&str> = line.splitn(6, '\0').collect();
            if f.len() < 6 {
                continue;
            }
            view.commits.push(CommitInfo {
                oid: f[0].to_string(),
                author_time: f[1].parse().unwrap_or(0),
                author_name: f[2].to_string(),
                author_email: f[3].to_string(),
                parents: f[4].split(' ').filter(|s| !s.is_empty()).map(str::to_string).collect(),
                subject: f[5].to_string(),
            });
        }

        for line in self.text(wt, &["stash", "list", "--pretty=%H%x00%gs"])?.lines() {
            if let Some((o, m)) = line.split_once('\0') {
                view.stashes.push(StashInfo { oid: o.to_string(), message: m.to_string() });
            }
        }

        let mut cur: Option<WorktreeInfo> = None;
        for line in self.text(wt, &["worktree", "list", "--porcelain"])?.lines().chain(std::iter::once("")) {
            if let Some(p) = line.strip_prefix("worktree ") {
                cur = Some(WorktreeInfo { path: p.to_string(), head: String::new(), branch: None });
            } else if let Some(h) = line.strip_prefix("HEAD ") {
                if let Some(w) = cur.as_mut() {
                    w.head = h.to_string();
                }
            } else if let Some(b) = line.strip_prefix("branch ") {
                if let Some(w) = cur.as_mut() {
                    w.branch = Some(b.to_string());
                }
            } else if line.is_empty() {
                if let Some(w) = cur.take() {
                    view.worktrees.push(w);
                }
            }
        }
        view.worktrees.sort();
        Ok(view)
    }

    fn switch_branch(&self, wt: &Path, branch: &str) -> Result<()> {
        output(self.g(wt).args(["checkout", "-q", branch])).map(|_| ())
    }

    fn commit(&self, wt: &Path, message: &str, author: &Ident, committer: &Ident) -> Result<()> {
        output(self.committing(wt, Some(author), committer).args(["commit", "-q", "-m", message])).map(|_| ())
    }

    fn amend_head(&self, wt: &Path, committer: &Ident) -> Result<()> {
        output(self.committing(wt, None, committer).args(["commit", "-q", "--amend", "--no-edit", "--allow-empty"])).map(|_| ())
    }

    fn amend_older(&self, wt: &Path, target: &str, committer: &Ident) -> Result<()> {
        output(self.committing(wt, Some(committer), committer).args(["commit", "-q", &format!("--fixup={target}")]))?;
        let onto = format!("{target}^");
        output(self.committing(wt, None, committer).env("GIT_SEQUENCE_EDITOR", "true").args([
            "rebase",
            "-q",
            "--interactive",
            "--autosquash",
            "--autostash",
            "--keep-empty",
            "--rebase-merges",
            &onto,
        ]))
        .map(|_| ())
    }

    fn move_patch_to_index(&self, wt: &Path, commit: &str, paths: &[String], committer: &Ident) -> Result<()> {
        static N: AtomicU32 = AtomicU32::new(0);
        let patch_file = std::env::temp_dir().join(format!("lazyer-patch-{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
        let (from, to) = (format!("{commit}^"), commit.to_string());
        let mut args = vec!["diff", "--binary", "--full-index", from.as_str(), to.as_str(), "--"];
        args.extend(paths.iter().map(String::as_str));
        let patch = output(self.g(wt).args(&args))?;
        std::fs::write(&patch_file, patch)?;
        let patch_arg = patch_file.to_str().context("utf-8 temp path")?.to_string();

        let result = (|| -> Result<()> {
            // Stop at the commit, take the patch out of it, replay the rest,
            // then apply the patch to index and worktree.
            output(self.committing(wt, None, committer).env("GIT_SEQUENCE_EDITOR", "perl -pi -e 'if (!$done && s/^pick/edit/) { $done = 1 }'").args([
                "rebase",
                "-q",
                "--interactive",
                "--autostash",
                "--keep-empty",
                "--no-autosquash",
                "--rebase-merges",
                &from,
            ]))?;
            output(self.g(wt).args(["apply", "--index", "--3way", "--reverse", &patch_arg]))?;
            output(self.committing(wt, None, committer).args(["commit", "-q", "--amend", "--no-edit", "--allow-empty"]))?;
            output(self.committing(wt, None, committer).args(["rebase", "--continue"]))?;
            output(self.g(wt).args(["apply", "--index", "--3way", &patch_arg]))?;
            Ok(())
        })();
        let _ = std::fs::remove_file(&patch_file);
        result
    }

    fn swap_branch(&self, wt: &Path, branch: &str, detach_other: bool) -> Result<()> {
        // Port of ~/bin/git-worktree-swap.
        let here = self.text(wt, &["rev-parse", "--show-toplevel"])?;
        let current = self.text(wt, &["symbolic-ref", "--quiet", "--short", "HEAD"]).unwrap_or_default();
        let wanted = format!("branch refs/heads/{branch}");
        let mut other = None;
        let mut path = String::new();
        for line in self.text(wt, &["worktree", "list", "--porcelain"])?.lines() {
            if let Some(p) = line.strip_prefix("worktree ") {
                path = p.to_string();
            } else if line == wanted {
                other = Some(path.clone());
            }
        }
        let Some(other) = other else { bail!("{branch} is not checked out in another worktree; nothing to swap") };
        if other == here {
            return Ok(());
        }
        let other = PathBuf::from(other);
        output(self.g(&other).args(["switch", "--detach", "--quiet"]))?;
        if let Err(e) = output(self.g(wt).args(["switch", "--quiet", branch])) {
            let _ = output(self.g(&other).args(["switch", "--quiet", branch]));
            return Err(e);
        }
        if !detach_other && !current.is_empty() {
            output(self.g(&other).args(["switch", "--quiet", &current]))?;
        }
        Ok(())
    }
}
