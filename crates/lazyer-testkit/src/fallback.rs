//! The engine as it is meant to be used: native where it can, real git for
//! everything it declines.

use crate::gitcli::GitCli;
use lazyer_core::{Backend, Ident, Native, RepoView, Result, Status, Unsupported};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};

pub struct WithFallback {
    pub native: Native,
    pub git: GitCli,
    /// How many calls ended up in git.
    pub fallbacks: AtomicUsize,
}

impl WithFallback {
    pub fn new(native: Native) -> Self {
        WithFallback { native, git: GitCli::real(), fallbacks: AtomicUsize::new(0) }
    }

    fn run<T>(&self, native: impl FnOnce(&Native) -> Result<T>, git: impl FnOnce(&GitCli) -> Result<T>) -> Result<T> {
        match native(&self.native) {
            Err(e) if e.downcast_ref::<Unsupported>().is_some() => {
                self.fallbacks.fetch_add(1, Ordering::Relaxed);
                if std::env::var_os("LAZYER_TRACE").is_some() {
                    eprintln!("  trace fallback to git: {e}");
                }
                git(&self.git)
            }
            other => other,
        }
    }
}

impl Backend for WithFallback {
    fn name(&self) -> &str {
        "native"
    }
    fn status(&self, wt: &Path) -> Result<Status> {
        self.run(|n| n.status(wt), |g| g.status(wt))
    }
    fn load_view(&self, wt: &Path) -> Result<RepoView> {
        // When only the file status needs git, the rest still comes from the engine.
        self.run(
            |n| n.load_view(wt),
            |g| match self.native.load_view_without_status(wt) {
                Ok(view) => Ok(RepoView { status: g.status(wt)?, ..view }),
                Err(_) => g.load_view(wt),
            },
        )
    }
    fn switch_branch(&self, wt: &Path, branch: &str) -> Result<()> {
        self.run(|n| n.switch_branch(wt, branch), |g| g.switch_branch(wt, branch))
    }
    fn commit(&self, wt: &Path, message: &str, author: &Ident, committer: &Ident) -> Result<()> {
        self.run(|n| n.commit(wt, message, author, committer), |g| g.commit(wt, message, author, committer))
    }
    fn amend_head(&self, wt: &Path, committer: &Ident) -> Result<()> {
        self.run(|n| n.amend_head(wt, committer), |g| g.amend_head(wt, committer))
    }
    fn amend_older(&self, wt: &Path, target: &str, committer: &Ident) -> Result<()> {
        self.run(|n| n.amend_older(wt, target, committer), |g| g.amend_older(wt, target, committer))
    }
    fn move_patch_to_index(&self, wt: &Path, commit: &str, paths: &[String], committer: &Ident) -> Result<()> {
        self.run(|n| n.move_patch_to_index(wt, commit, paths, committer), |g| g.move_patch_to_index(wt, commit, paths, committer))
    }
    fn swap_branch(&self, wt: &Path, branch: &str, detach_other: bool) -> Result<()> {
        self.run(|n| n.swap_branch(wt, branch, detach_other), |g| g.swap_branch(wt, branch, detach_other))
    }
}
