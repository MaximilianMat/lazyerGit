//! Bring a long-lived fixture back to its starting state between benchmark
//! iterations.

use crate::git::{lines, run, succeeds};
use anyhow::Result;
use std::path::{Path, PathBuf};

pub struct Baseline {
    /// Worktree, branch (empty when detached), commit.
    worktrees: Vec<(PathBuf, String, String)>,
}

impl Baseline {
    pub fn capture(worktrees: &[PathBuf]) -> Result<Self> {
        let mut v = Vec::new();
        for wt in worktrees {
            let branch = run(wt, &["symbolic-ref", "-q", "--short", "HEAD"]).unwrap_or_default();
            v.push((wt.clone(), branch, run(wt, &["rev-parse", "HEAD"])?));
        }
        Ok(Baseline { worktrees: v })
    }

    /// The fixture's original state: recorded in `baseline.txt` the first
    /// time this is called (on a pristine fixture), read from there afterwards
    /// so a fixture left dirty by an interrupted run can still be restored.
    pub fn of_fixture(root: &Path, worktrees: &[PathBuf]) -> Result<Self> {
        let file = root.join("baseline.txt");
        if let Ok(text) = std::fs::read_to_string(&file) {
            let mut v = Vec::new();
            for line in text.lines() {
                let f: Vec<&str> = line.split('\t').collect();
                if f.len() == 3 {
                    v.push((root.join(f[0]), f[1].to_string(), f[2].to_string()));
                }
            }
            return Ok(Baseline { worktrees: v });
        }
        let b = Self::capture(worktrees)?;
        let mut text = String::new();
        for (wt, branch, oid) in &b.worktrees {
            text.push_str(&format!("{}\t{branch}\t{oid}\n", wt.strip_prefix(root)?.display()));
        }
        std::fs::write(file, text)?;
        Ok(b)
    }

    pub fn restore(&self, repo: &Path) -> Result<()> {
        for (wt, _, oid) in &self.worktrees {
            if succeeds(wt, &["rev-parse", "-q", "--verify", "REBASE_HEAD"]) {
                let _ = run(wt, &["rebase", "--abort"]);
            }
            // Detach everywhere first: after a swap two worktrees hold each
            // other's branch.
            run(wt, &["checkout", "-q", "-f", "--detach", oid])?;
        }
        for (wt, branch, oid) in &self.worktrees {
            if !branch.is_empty() {
                run(wt, &["checkout", "-q", "-f", "-B", branch, oid])?;
            }
            run(wt, &["reset", "-q", "--hard", oid])?;
            run(wt, &["clean", "-q", "-f", "-d", "-e", "notes.txt", "-e", "scratch"])?;
        }
        // The two untracked files every fixture starts with.
        std::fs::write(repo.join("notes.txt"), "untracked notes\n")?;
        std::fs::create_dir_all(repo.join("scratch"))?;
        std::fs::write(repo.join("scratch/todo.md"), "- untracked\n")?;
        for name in lines(repo, &["for-each-ref", "--format=%(refname:short)", "refs/heads/scenario"])? {
            run(repo, &["branch", "-q", "-D", &name])?;
        }
        Ok(())
    }

    /// Make the index stat-clean and no longer "racy": files written within
    /// the same second as the index force git to re-read their contents.
    pub fn settle(&self) -> Result<()> {
        std::thread::sleep(std::time::Duration::from_millis(1100));
        for (wt, _, _) in &self.worktrees {
            run(wt, &["update-index", "-q", "--refresh", "--force-write-index"])?;
        }
        Ok(())
    }
}
