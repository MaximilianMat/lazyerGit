//! The native engine: git operations performed in-process on the repository
//! files, without starting git.

pub mod checkout;
pub mod commitobj;
pub mod emu;
pub mod index;
pub mod odb;
pub mod ops;
pub mod patch;
pub mod refs;
pub mod similarity;
pub mod stage;
pub mod status;
pub mod tree;
pub mod view;

use crate::api::{unsupported, Backend, Ident, RepoView, Result, Status};
use ops::{Repo, Settings};
use refs::Layout;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// What is kept per repository between operations.
struct Shared {
    odb: odb::GixOdb,
    settings: Settings,
    /// For configuration lookups (upstreams, ignore settings).
    repo: gix::ThreadSafeRepository,
    memo: view::Memo,
    /// Modification times of the config files this was built from.
    config_stamp: Vec<Option<std::time::SystemTime>>,
}

fn config_stamp(l: &Layout) -> Vec<Option<std::time::SystemTime>> {
    // The repository's files and the usual places of the user's own. Files
    // pulled in through `include` are not watched.
    let mut files = vec![l.common_dir.join("config"), l.git_dir.join("config.worktree")];
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        files.push(home.join(".gitconfig"));
        files.push(std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).unwrap_or_else(|| home.join(".config")).join("git").join("config"));
    }
    files.push(PathBuf::from("/etc/gitconfig"));
    files.iter().map(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok()).collect()
}

pub struct Native {
    /// Ignore the user's global and system git configuration (tests).
    isolated: bool,
    repos: Mutex<HashMap<PathBuf, Arc<Shared>>>,
    /// Per worktree: what is remembered about its directories.
    dir_caches: Mutex<HashMap<PathBuf, Arc<status::DirCache>>>,
}

impl Default for Native {
    fn default() -> Self {
        Self::new()
    }
}

impl Native {
    pub fn new() -> Self {
        Native { isolated: false, repos: Mutex::new(HashMap::new()), dir_caches: Mutex::new(HashMap::new()) }
    }

    /// Like `new`, but only the repository's own configuration counts.
    pub fn isolated() -> Self {
        Native { isolated: true, repos: Mutex::new(HashMap::new()), dir_caches: Mutex::new(HashMap::new()) }
    }

    fn shared(&self, l: &Layout) -> Result<Arc<Shared>> {
        let stamp = config_stamp(l);
        if let Some(s) = self.repos.lock().unwrap().get(&l.common_dir) {
            if s.config_stamp == stamp {
                return Ok(s.clone());
            }
        }
        let opts = if self.isolated { gix::open::Options::isolated() } else { gix::open::Options::default() };
        let repo = gix::open_opts(&l.worktree, opts).map_err(|e| anyhow::anyhow!("opening {}: {e}", l.worktree.display()))?;
        let settings = Self::read_settings(l, &repo);
        let shared =
            Arc::new(Shared { odb: odb::GixOdb::open(&l.common_dir)?, settings, repo: repo.into_sync(), memo: Default::default(), config_stamp: stamp });
        self.repos.lock().unwrap().insert(l.common_dir.clone(), shared.clone());
        Ok(shared)
    }

    fn read_settings(l: &Layout, repo: &gix::Repository) -> Settings {
        let cfg = repo.config_snapshot();
        let mut content_needs_git = None;
        if cfg.string("core.autocrlf").is_some_and(|v| v != "false") {
            content_needs_git = Some("core.autocrlf".to_string());
        }
        for key in ["core.eol", "core.safecrlf", "core.checkRoundtripEncoding"] {
            if cfg.string(key).is_some() {
                content_needs_git = Some(key.to_string());
            }
        }
        for (key, expected) in [("core.sparseCheckout", false), ("index.sparse", false), ("core.symlinks", true), ("core.fileMode", true)] {
            if cfg.boolean(key).is_some_and(|v| v != expected) {
                content_needs_git = Some(key.to_string());
            }
        }
        let hooks_dir = match cfg.string("core.hooksPath") {
            Some(p) => {
                let p = PathBuf::from(p.to_string());
                if p.is_absolute() {
                    p
                } else {
                    l.worktree.join(p)
                }
            }
            None => l.common_dir.join("hooks"),
        };
        let user = match (cfg.string("user.name"), cfg.string("user.email")) {
            (Some(n), Some(e)) => Some((n.to_string(), e.to_string())),
            _ => None,
        };
        let excludes_file = cfg.trusted_path("core.excludesFile").ok().flatten().or_else(|| {
            let base = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
            Some(base.join("git").join("ignore"))
        });
        Settings {
            content_needs_git,
            gpg_sign: cfg.boolean("commit.gpgSign").unwrap_or(false),
            hooks_dir,
            skip_hash: cfg.boolean("index.skipHash").or(cfg.boolean("feature.manyFiles")).unwrap_or(false),
            user,
            excludes_file,
            ignore_case: cfg.boolean("core.ignoreCase").unwrap_or(false),
            precompose_unicode: cfg.boolean("core.precomposeUnicode").unwrap_or(false),
        }
    }

    /// The full name of the ref a branch tracks, from `branch.<name>.remote`
    /// and `.merge`.
    fn upstream_of(repo: &gix::Repository, branch_ref: &str) -> Option<String> {
        let name = gix::refs::FullName::try_from(branch_ref).ok()?;
        let tracking = repo.branch_remote_tracking_ref_name(name.as_ref(), gix::remote::Direction::Fetch)?.ok()?;
        Some(tracking.as_bstr().to_string())
    }

    fn status_in(&self, r: &Repo, shared: &Shared) -> Result<(Status, refs::Head)> {
        let head = refs::read_head(&r.l)?;
        let (index, index_mtime) = r.load_index()?;
        let head_tree = match head.oid() {
            Some(c) => Some(r.tree_of(&c)?),
            None => None,
        };
        let mut buf = Vec::new();
        let base_rules = gix::ignore::Search::from_git_dir(&r.l.common_dir, shared.settings.excludes_file.clone(), &mut buf, Default::default())?;
        let dir_cache = self.dir_caches.lock().unwrap().entry(r.l.worktree.clone()).or_default().clone();
        let entries = status::entries(&status::StatusInput {
            root: &r.l.worktree,
            odb: r.odb,
            index: &index,
            index_mtime,
            head_tree,
            base_rules,
            ignore_case: shared.settings.ignore_case,
            precompose_unicode: shared.settings.precompose_unicode,
            dir_cache: &dir_cache,
        })?;
        let mut st = Status { head_oid: head.oid().map(|o| tree::hex(&o)).unwrap_or_else(|| "(initial)".into()), entries, ..Default::default() };
        if let refs::Head::Branch(name, tip) = &head {
            st.branch = Some(name.strip_prefix("refs/heads/").unwrap_or(name).to_string());
            let repo = shared.repo.to_thread_local();
            if let Some(up) = shared.memo.upstream(name, || Self::upstream_of(&repo, name)) {
                st.upstream = Some(view::short_upstream(&up));
                if let (Some(tip), Some(up_oid)) = (tip, refs::resolve(&r.l, &up)?) {
                    st.ahead_behind = Some(view::Commits::new(r.odb, &r.l.common_dir).with_memo(&shared.memo).ahead_behind(tip, &up_oid)?);
                }
            }
        }
        Ok((st, head))
    }

    fn view_in(&self, r: &Repo, shared: &Shared) -> Result<RepoView> {
        if r.l.common_dir.join("shallow").exists() || r.l.worktree.join(".mailmap").exists() {
            return unsupported("shallow clone or mailmap");
        }
        let (status, rest) = rayon::join(|| self.status_in(r, shared), || self.view_rest(r, shared));
        Ok(RepoView { status: status?.0, ..rest? })
    }

    /// The view without file status.
    fn view_rest(&self, r: &Repo, shared: &Shared) -> Result<RepoView> {
        let head = refs::read_head(&r.l)?;
        let repo = shared.repo.to_thread_local();
        let commits = view::Commits::new(r.odb, &r.l.common_dir).with_memo(&shared.memo);
        let short = |prefix: &str| -> Result<Vec<(String, String)>> {
            Ok(view::list_refs(&r.l, prefix)?.into_iter().map(|(n, o)| (n[prefix.len()..].to_string(), tree::hex(&o))).collect())
        };
        let log = match head.oid() {
            Some(tip) => commits.topo_log(&tip, crate::api::LOG_LIMIT)?.iter().map(|c| view::commit_info(&commits, c)).collect::<Result<_>>()?,
            None => Vec::new(),
        };
        Ok(RepoView {
            status: Status::default(),
            branches: view::branches(&r.l, &commits, &head, &|name| shared.memo.upstream(name, || Self::upstream_of(&repo, name)))?,
            remote_branches: short("refs/remotes/")?,
            tags: short("refs/tags/")?,
            commits: log,
            stashes: view::stashes(&r.l)?,
            worktrees: view::worktrees(&r.l)?,
        })
    }

    /// Move selected hunks or lines out of a commit into the index; see
    /// [`Repo::move_patch_text_to_index`].
    pub fn move_patch_text_to_index(&self, wt: &Path, commit: &str, patch: &[u8], committer: &Ident) -> Result<()> {
        self.with_repo(wt, |r| r.move_patch_text_to_index(commit, patch, committer))
    }

    /// Everything of `load_view` except the file status, for when the status
    /// has to come from git.
    pub fn load_view_without_status(&self, wt: &Path) -> Result<RepoView> {
        self.with_shared(wt, |r, shared| {
            if r.l.common_dir.join("shallow").exists() || r.l.worktree.join(".mailmap").exists() {
                return unsupported("shallow clone or mailmap");
            }
            self.view_rest(r, shared)
        })
    }

    fn with_repo<T>(&self, wt: &Path, f: impl FnOnce(&Repo) -> Result<T>) -> Result<T> {
        let l = Layout::discover(wt)?;
        let shared = self.shared(&l)?;
        f(&Repo { l, odb: &shared.odb, settings: &shared.settings })
    }

    fn with_shared<T>(&self, wt: &Path, f: impl FnOnce(&Repo, &Shared) -> Result<T>) -> Result<T> {
        let l = Layout::discover(wt)?;
        let shared = self.shared(&l)?;
        f(&Repo { l, odb: &shared.odb, settings: &shared.settings }, &shared)
    }
}

impl Backend for Native {
    fn name(&self) -> &str {
        "native"
    }

    fn status(&self, wt: &Path) -> Result<Status> {
        self.with_shared(wt, |r, shared| Ok(self.status_in(r, shared)?.0))
    }

    fn load_view(&self, wt: &Path) -> Result<RepoView> {
        self.with_shared(wt, |r, shared| self.view_in(r, shared))
    }

    fn switch_branch(&self, wt: &Path, branch: &str) -> Result<()> {
        self.with_repo(wt, |r| r.switch_branch(branch))
    }

    fn commit(&self, wt: &Path, message: &str, author: &Ident, committer: &Ident) -> Result<()> {
        self.with_repo(wt, |r| r.commit(message, author, committer))
    }

    fn amend_head(&self, wt: &Path, committer: &Ident) -> Result<()> {
        self.with_repo(wt, |r| r.amend_head(committer))
    }

    fn amend_older(&self, wt: &Path, target: &str, committer: &Ident) -> Result<()> {
        self.with_repo(wt, |r| r.amend_older(target, committer))
    }

    fn move_patch_to_index(&self, wt: &Path, commit: &str, paths: &[String], committer: &Ident) -> Result<()> {
        self.with_repo(wt, |r| r.move_patch_to_index(commit, paths, committer))
    }

    fn swap_branch(&self, wt: &Path, branch: &str, detach_other: bool) -> Result<()> {
        self.with_repo(wt, |r| r.swap_branch(branch, detach_other))
    }
}

/// Phase timings on stderr when `LAZYER_TRACE` is set.
pub(crate) struct Trace {
    on: bool,
    last: std::time::Instant,
}

impl Trace {
    pub(crate) fn start() -> Self {
        static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        Trace { on: *ON.get_or_init(|| std::env::var_os("LAZYER_TRACE").is_some()), last: std::time::Instant::now() }
    }

    pub(crate) fn step(&mut self, what: &str) {
        if self.on {
            eprintln!("  trace {what:<28} {:>8.2} ms", self.last.elapsed().as_secs_f64() * 1000.0);
            self.last = std::time::Instant::now();
        }
    }
}
