use std::fmt;
use std::path::Path;

pub type Result<T> = anyhow::Result<T>;

/// Returned by a backend for an operation (or a repository state) it does not
/// handle natively. The caller falls back to real git.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unsupported(pub String);

impl fmt::Display for Unsupported {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "unsupported: {}", self.0)
    }
}

impl std::error::Error for Unsupported {}

pub fn unsupported<T>(what: impl Into<String>) -> Result<T> {
    Err(Unsupported(what.into()).into())
}

/// Author or committer identity with an explicit timestamp, so that commits
/// are reproducible and both backends produce the same object ids.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ident {
    pub name: String,
    pub email: String,
    /// Seconds since the epoch.
    pub time: i64,
    /// Offset as git writes it, e.g. `+0100`.
    pub tz: String,
}

impl Ident {
    pub fn new(name: &str, email: &str, time: i64) -> Self {
        Ident { name: name.into(), email: email.into(), time, tz: "+0000".into() }
    }

    pub fn git_date(&self) -> String {
        format!("{} {}", self.time, self.tz)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct StatusEntry {
    pub path: String,
    /// Staged state as in `git status --porcelain`: one of ` MADRCU?!`.
    pub index: char,
    /// Working-tree state, same alphabet.
    pub worktree: char,
    /// Source path of a rename or copy.
    pub orig_path: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Status {
    /// Commit id of HEAD, or `(initial)`.
    pub head_oid: String,
    /// Short branch name, `None` when detached.
    pub branch: Option<String>,
    pub upstream: Option<String>,
    /// Ahead/behind the upstream; `None` without an upstream or when it is gone.
    pub ahead_behind: Option<(u32, u32)>,
    /// Sorted by path.
    pub entries: Vec<StatusEntry>,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct BranchInfo {
    pub name: String,
    pub oid: String,
    pub is_head: bool,
    pub upstream: Option<String>,
    /// `None` without an upstream or when the upstream ref is gone.
    pub ahead_behind: Option<(u32, u32)>,
    pub subject: String,
    pub committer_time: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommitInfo {
    pub oid: String,
    pub parents: Vec<String>,
    pub author_name: String,
    pub author_email: String,
    pub author_time: i64,
    pub subject: String,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct WorktreeInfo {
    pub path: String,
    pub head: String,
    /// Full ref name, `None` when detached.
    pub branch: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StashInfo {
    pub oid: String,
    pub message: String,
}

/// Everything the UI shows after opening a worktree: what lazygit loads at
/// startup and what has to be ready after a worktree switch.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct RepoView {
    pub status: Status,
    /// Sorted by name.
    pub branches: Vec<BranchInfo>,
    /// `(short name, oid)`, sorted by name.
    pub remote_branches: Vec<(String, String)>,
    /// `(name, oid of the ref target)`, sorted by name.
    pub tags: Vec<(String, String)>,
    /// First 300 commits from HEAD in topological order.
    pub commits: Vec<CommitInfo>,
    /// Newest first.
    pub stashes: Vec<StashInfo>,
    /// Sorted by path.
    pub worktrees: Vec<WorktreeInfo>,
}

pub const LOG_LIMIT: usize = 300;

/// The operations of the hot workflows. `wt` is always the root of a worktree.
pub trait Backend: Send + Sync {
    fn name(&self) -> &str;

    fn status(&self, wt: &Path) -> Result<Status>;

    /// What a worktree switch has to produce.
    fn load_view(&self, wt: &Path) -> Result<RepoView>;

    /// `git checkout <branch>` for a local branch.
    fn switch_branch(&self, wt: &Path, branch: &str) -> Result<()>;

    /// Commit the index.
    fn commit(&self, wt: &Path, message: &str, author: &Ident, committer: &Ident) -> Result<()>;

    /// Fold the index into HEAD, keeping message and author.
    fn amend_head(&self, wt: &Path, committer: &Ident) -> Result<()>;

    /// Fold the index into `target`, an ancestor of HEAD on the current
    /// branch, and replay the commits above it (lazygit: "amend commit with
    /// staged changes").
    fn amend_older(&self, wt: &Path, target: &str, committer: &Ident) -> Result<()>;

    /// Remove the changes `commit` makes to `paths` from that commit, replay
    /// the commits above it and leave those changes staged (lazygit: custom
    /// patch, "move patch out into index").
    fn move_patch_to_index(&self, wt: &Path, commit: &str, paths: &[String], committer: &Ident) -> Result<()>;

    /// Check out `branch` here although another worktree has it; that worktree
    /// gets the branch this one was on, or is left detached.
    fn swap_branch(&self, wt: &Path, branch: &str, detach_other: bool) -> Result<()>;
}
