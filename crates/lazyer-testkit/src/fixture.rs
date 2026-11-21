//! Deterministic synthetic repositories, written through `git fast-import`.
//!
//! `Profile::work()` follows the shape measured by `scripts/probe.sh` on the
//! real target repository; `Profile::small()` is a quick one for correctness
//! tests and includes awkward paths and file modes.

use crate::git::{git, output, output_with_stdin, run};
use crate::rng::{mix, Rng};
use anyhow::{bail, Context, Result};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU32, Ordering};

/// Bump when the generator changes, so cached fixtures are rebuilt.
const GENERATOR_VERSION: u32 = 1;
const BASE_TIME: i64 = 1_600_000_000;

#[derive(Clone, Debug)]
pub struct Profile {
    pub name: &'static str,
    pub seed: u64,
    pub files: usize,
    pub dirs: usize,
    pub max_depth: usize,
    pub size_median: f64,
    pub size_sigma: f64,
    pub big_blobs: usize,
    pub big_blob_max: usize,
    /// Commits on main.
    pub main_commits: usize,
    /// The newest commits on main and all branch commits touch
    /// `files_per_commit` files on average; older history touches one or two.
    pub recent_window: usize,
    pub files_per_commit: f64,
    pub local_branches: usize,
    pub remote_refs: usize,
    pub tags: usize,
    pub stashes: usize,
    /// Linked worktrees in addition to the main one.
    pub worktrees: usize,
    /// How many of them are fully checked out; the rest use `--no-checkout`.
    pub full_worktrees: usize,
    pub ignored_dirs: usize,
    /// Add paths with spaces and non-ASCII names, a symlink, an executable,
    /// an empty and a binary file.
    pub edge_cases: bool,
    /// Repack, commit-graph and multi-pack-index after the import.
    pub maintenance: bool,
}

impl Profile {
    pub fn small() -> Self {
        Profile {
            name: "small",
            seed: 7,
            files: 240,
            dirs: 40,
            max_depth: 6,
            size_median: 600.0,
            size_sigma: 1.0,
            big_blobs: 1,
            big_blob_max: 200_000,
            main_commits: 60,
            recent_window: 30,
            files_per_commit: 4.0,
            local_branches: 8,
            remote_refs: 14,
            tags: 6,
            stashes: 3,
            worktrees: 3,
            full_worktrees: 2,
            ignored_dirs: 3,
            edge_cases: true,
            maintenance: false,
        }
    }

    /// Shape of the probed work repository (probe of 2026-11-21).
    pub fn work() -> Self {
        Profile {
            name: "work",
            seed: 42,
            files: 32_147,
            dirs: 7_047,
            max_depth: 15,
            size_median: 1_346.0,
            size_sigma: 1.37,
            big_blobs: 14,
            big_blob_max: 14_262_594,
            main_commits: 21_824,
            recent_window: 600,
            files_per_commit: 7.0,
            local_branches: 328,
            remote_refs: 9_009,
            tags: 2_146,
            stashes: 133,
            worktrees: 37,
            full_worktrees: 1,
            ignored_dirs: 200,
            edge_cases: false,
            maintenance: true,
        }
    }

    pub fn by_name(name: &str) -> Option<Self> {
        match name {
            "small" => Some(Self::small()),
            "work" => Some(Self::work()),
            _ => None,
        }
    }
}

/// A generated repository: `repo` is the main worktree, linked worktrees are
/// `wt/01`, `wt/02`, ... next to it.
#[derive(Debug)]
pub struct Fixture {
    pub root: PathBuf,
    pub repo: PathBuf,
    temp: bool,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if self.temp {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }
}

impl Fixture {
    pub fn worktree(&self, n: usize) -> PathBuf {
        self.root.join("wt").join(format!("{n:02}"))
    }

    /// Generate into a fresh temporary directory that is removed on drop.
    pub fn temp(profile: &Profile) -> Result<Fixture> {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!("lazyer-fx-{}-{n}", std::process::id()));
        if root.exists() {
            std::fs::remove_dir_all(&root)?;
        }
        // The real path, as git reports worktree paths resolved.
        std::fs::create_dir_all(&root)?;
        let root = root.canonicalize()?;
        let mut fx = Fixture::generate(profile, &root)?;
        fx.temp = true;
        Ok(fx)
    }

    /// Generate once under `dir` and reuse on later calls.
    pub fn cached(profile: &Profile, dir: &Path) -> Result<Fixture> {
        std::fs::create_dir_all(dir)?;
        let root = dir.canonicalize()?.join(format!("{}-s{}-g{}", profile.name, profile.seed, GENERATOR_VERSION));
        let marker = root.join(".complete");
        if marker.exists() {
            return Ok(Fixture { repo: root.join("repo"), root, temp: false });
        }
        if root.exists() {
            std::fs::remove_dir_all(&root)?;
        }
        std::fs::create_dir_all(&root)?;
        let fx = Fixture::generate(profile, &root)?;
        std::fs::write(marker, "")?;
        Ok(fx)
    }

    pub fn generate(p: &Profile, root: &Path) -> Result<Fixture> {
        let repo = root.join("repo");
        std::fs::create_dir_all(&repo)?;
        run(&repo, &["init", "-q", "-b", "main", "."])?;
        for (k, v) in [
            ("user.name", crate::git::TEST_NAME),
            ("user.email", crate::git::TEST_EMAIL),
            ("commit.gpgsign", "false"),
            ("gc.auto", "0"),
            ("maintenance.auto", "false"),
            ("core.precomposeunicode", "true"),
        ] {
            run(&repo, &["config", k, v])?;
        }

        let plan = import(p, &repo)?;

        run(&repo, &["reset", "-q", "--hard", "main"])?;

        // Stashes were imported as plain commits; register them in order.
        let stash_refs = run(&repo, &["for-each-ref", "--format=%(objectname) %(refname)", "refs/lazyer-tmp/stash"])?;
        let mut deletes = String::new();
        for (i, line) in stash_refs.lines().enumerate() {
            let (oid, name) = line.split_once(' ').context("stash ref line")?;
            run(&repo, &["stash", "store", "-m", &format!("WIP on main: change set {i}"), oid])?;
            deletes.push_str(&format!("delete {name}\n"));
        }
        for name in run(&repo, &["for-each-ref", "--format=%(refname)", "refs/lazyer-tmp"])?.lines() {
            if !deletes.contains(&format!("delete {name}\n")) {
                deletes.push_str(&format!("delete {name}\n"));
            }
        }
        if !deletes.is_empty() {
            output_with_stdin(git(&repo).args(["update-ref", "--stdin"]), deletes.as_bytes())?;
        }

        // Remote and upstream configuration.
        let mut cfg = std::fs::OpenOptions::new().append(true).open(repo.join(".git/config"))?;
        writeln!(cfg, "[remote \"origin\"]\n\turl = {}\n\tfetch = +refs/heads/*:refs/remotes/origin/*", root.join("origin.git").display())?;
        for name in &plan.tracked_branches {
            writeln!(cfg, "[branch \"{name}\"]\n\tremote = origin\n\tmerge = refs/heads/{name}")?;
        }
        drop(cfg);

        run(&repo, &["pack-refs", "--all"])?;
        if p.maintenance {
            run(&repo, &["repack", "-a", "-d", "-q"])?;
            run(&repo, &["commit-graph", "write", "--reachable"])?;
            run(&repo, &["multi-pack-index", "write"])?;
        }

        std::fs::create_dir_all(root.join("wt"))?;
        for i in 0..p.worktrees.min(plan.local_branches.len()) {
            let path = root.join("wt").join(format!("{:02}", i + 1));
            let path = path.to_str().context("utf-8 path")?.to_string();
            let branch = &plan.local_branches[i];
            if i < p.full_worktrees {
                run(&repo, &["worktree", "add", "-q", &path, branch])?;
            } else {
                run(&repo, &["worktree", "add", "-q", "--no-checkout", &path, branch])?;
            }
        }

        // Ignored build output and two untracked files, as in the probed repo.
        let mut r = Rng::new(p.seed ^ 0xD1);
        for i in 0..p.ignored_dirs {
            let dir = &plan.dirs[r.below(plan.dirs.len())];
            let build = repo.join(dir).join("build");
            if repo.join(dir).is_dir() {
                std::fs::create_dir_all(&build)?;
                for j in 0..3 {
                    std::fs::write(build.join(format!("out{i}_{j}.o")), format!("artifact {i} {j}\n"))?;
                }
            }
        }
        std::fs::write(repo.join("notes.txt"), "untracked notes\n")?;
        std::fs::create_dir_all(repo.join("scratch"))?;
        std::fs::write(repo.join("scratch/todo.md"), "- untracked\n")?;

        run(&repo, &["status", "--porcelain"])?;
        Ok(Fixture { root: root.to_path_buf(), repo, temp: false })
    }
}

struct Plan {
    dirs: Vec<String>,
    local_branches: Vec<String>,
    tracked_branches: Vec<String>,
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Text,
    Exec,
    Binary,
    Symlink,
}

struct FileSpec {
    path: String,
    size: usize,
    kind: Kind,
}

const WORDS: &[&str] = &[
    "account",
    "adapter",
    "billing",
    "bridge",
    "cache",
    "catalog",
    "client",
    "command",
    "config",
    "context",
    "control",
    "convert",
    "customer",
    "delivery",
    "domain",
    "engine",
    "event",
    "export",
    "factory",
    "filter",
    "gateway",
    "handler",
    "import",
    "index",
    "invoice",
    "ledger",
    "listener",
    "manager",
    "mapper",
    "message",
    "metric",
    "model",
    "module",
    "notify",
    "order",
    "parser",
    "payment",
    "policy",
    "process",
    "product",
    "provider",
    "query",
    "queue",
    "record",
    "registry",
    "report",
    "request",
    "resolver",
    "response",
    "routing",
    "schedule",
    "schema",
    "service",
    "session",
    "shipment",
    "storage",
    "stream",
    "tenant",
    "token",
    "validator",
];
const EXTS: &[&str] = &["java", "java", "java", "ts", "ts", "xml", "json", "md", "yaml", "sql", "kt", "properties"];
const AUTHORS: &[&str] = &["Ada Berg", "Bo Chen", "Cy Dietz", "Di Evans", "Eli Faro", "Fay Gold", "Gus Hahn", "Ida Jost"];

fn camel(r: &mut Rng, words: usize) -> String {
    (0..words)
        .map(|_| {
            let w = *r.pick(WORDS);
            w[..1].to_uppercase() + &w[1..]
        })
        .collect()
}

/// Content of `file` at `version`. A new version rewrites a subset of the
/// lines, so consecutive versions produce small multi-hunk diffs and versions
/// with different `version % 16` touch different lines.
pub fn text_content(file: u32, size: usize, version: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity(size + 96);
    let modulus = (size / 48).clamp(1, 16) as u32;
    let mut line = 0u32;
    while out.len() < size {
        let v = if version > 0 && line % modulus == version % modulus { version } else { 0 };
        let mut r = Rng::new(mix(file as u64, line as u64, v as u64));
        let words = 3 + r.below(9);
        for w in 0..words {
            if w > 0 {
                out.push(b' ');
            }
            out.extend_from_slice(r.pick(WORDS).as_bytes());
        }
        out.push(b'\n');
        line += 1;
    }
    out
}

fn binary_content(file: u32, size: usize, version: u32) -> Vec<u8> {
    let mut r = Rng::new(mix(file as u64, 0xB1, version as u64));
    let mut out = Vec::with_capacity(size + 8);
    while out.len() < size {
        out.extend_from_slice(&r.next_u64().to_le_bytes());
    }
    out.truncate(size);
    out
}

struct Stream<W: Write> {
    w: W,
    mark: u32,
}

impl<W: Write> Stream<W> {
    fn begin(&mut self, refname: &str, author: usize, time: i64, msg: &str, from: Option<u32>, merge: Option<u32>) -> Result<u32> {
        self.mark += 1;
        let who = AUTHORS[author % AUTHORS.len()];
        let email = who.to_lowercase().replace(' ', ".");
        write!(
            self.w,
            "commit {refname}\nmark :{}\nauthor {who} <{email}@example.invalid> {time} +0000\ncommitter {who} <{email}@example.invalid> {time} +0000\ndata {}\n{msg}\n",
            self.mark,
            msg.len()
        )?;
        if let Some(m) = from {
            writeln!(self.w, "from :{m}")?;
        }
        if let Some(m) = merge {
            writeln!(self.w, "merge :{m}")?;
        }
        Ok(self.mark)
    }

    fn modify(&mut self, id: u32, f: &FileSpec, version: u32) -> Result<()> {
        let (mode, data) = match f.kind {
            Kind::Text => ("100644", text_content(id, f.size, version)),
            Kind::Exec => ("100755", text_content(id, f.size, version)),
            Kind::Binary => ("100644", binary_content(id, f.size, version)),
            Kind::Symlink => ("120000", if version == 0 { b"docs".to_vec() } else { b"src".to_vec() }),
        };
        self.raw(mode, &f.path, &data)
    }

    fn raw(&mut self, mode: &str, path: &str, data: &[u8]) -> Result<()> {
        write!(self.w, "M {mode} inline {path}\ndata {}\n", data.len())?;
        self.w.write_all(data)?;
        self.w.write_all(b"\n")?;
        Ok(())
    }

    fn delete(&mut self, path: &str) -> Result<()> {
        writeln!(self.w, "D {path}")?;
        Ok(())
    }

    fn end(&mut self) -> Result<()> {
        self.w.write_all(b"\n")?;
        Ok(())
    }
}

/// Changes of one commit: mostly modifications, with probability `structure`
/// per slot an added or a deleted file instead.
fn change<W: Write>(
    s: &mut Stream<W>,
    r: &mut Rng,
    files: &mut Vec<FileSpec>,
    live: &mut Vec<u32>,
    version: &mut Vec<u32>,
    n: usize,
    structure: f64,
) -> Result<()> {
    let mut touched: Vec<u32> = Vec::with_capacity(n);
    for _ in 0..n {
        if r.chance(structure) && live.len() > 16 {
            if r.chance(0.7) {
                let id = files.len() as u32;
                let near = &files[*r.pick(live) as usize].path;
                let dir = near.rsplit_once('/').map(|(d, _)| d).unwrap_or("src");
                let path = format!("{dir}/{}{id}.java", camel(r, 2));
                let size = 200 + r.below(2000);
                files.push(FileSpec { path, size, kind: Kind::Text });
                version.push(0);
                live.push(id);
                touched.push(id);
                s.modify(id, &files[id as usize], 0)?;
            } else {
                let at = r.below(live.len());
                let id = live[at];
                if touched.contains(&id) || files[id as usize].kind != Kind::Text {
                    continue;
                }
                live.swap_remove(at);
                s.delete(&files[id as usize].path)?;
            }
            continue;
        }
        let id = *r.pick(live);
        let f = &files[id as usize];
        if touched.contains(&id) || f.size > 64_000 || f.size == 0 {
            continue;
        }
        touched.push(id);
        version[id as usize] += 1;
        s.modify(id, f, version[id as usize])?;
    }
    Ok(())
}

fn import(p: &Profile, repo: &Path) -> Result<Plan> {
    let mut r = Rng::new(p.seed);

    // Directory tree: every new directory hangs below a random existing one.
    let mut dirs: Vec<(String, usize)> = Vec::with_capacity(p.dirs);
    let mut seen = std::collections::HashSet::new();
    for top in ["src", "app", "libs", "services", "tools", "docs"] {
        dirs.push((top.to_string(), 1));
    }
    while dirs.len() < p.dirs {
        let (parent, depth) = dirs[r.below(dirs.len())].clone();
        if depth >= p.max_depth {
            continue;
        }
        let name = if r.chance(0.5) { format!("{}_{}", r.pick(WORDS), r.pick(WORDS)) } else { format!("{}{}", r.pick(WORDS), dirs.len()) };
        let path = format!("{parent}/{name}");
        if !seen.insert(path.clone()) {
            continue;
        }
        dirs.push((path, depth + 1));
    }

    // Files per directory follow a lognormal weight (median 2, long tail).
    let weights: Vec<f64> = (0..dirs.len()).map(|_| r.lognormal(2.0, 1.28)).collect();
    let total: f64 = weights.iter().sum();
    let mut files: Vec<FileSpec> = Vec::with_capacity(p.files + 64);
    files.push(FileSpec { path: ".gitignore".into(), size: 0, kind: Kind::Text });
    for (d, w) in weights.iter().enumerate() {
        let n = (w / total * p.files as f64).round() as usize;
        for k in 0..n {
            let ext = r.pick(EXTS);
            let words = 2 + r.below(3);
            let name = camel(&mut r, words);
            let size = r.lognormal(p.size_median, p.size_sigma).clamp(20.0, 400_000.0) as usize;
            files.push(FileSpec { path: format!("{}/{name}{k}.{ext}", dirs[d].0), size, kind: Kind::Text });
        }
    }
    for b in 0..p.big_blobs {
        let size = p.big_blob_max / (b + 1).max(1);
        let dir = &dirs[r.below(dirs.len())].0;
        files.push(FileSpec { path: format!("{dir}/asset{b}.bin"), size: size.max(64), kind: Kind::Binary });
    }
    if p.edge_cases {
        for (path, size, kind) in [
            ("docs/read me.md", 300, Kind::Text),
            ("docs/ünïcode-文件.txt", 200, Kind::Text),
            ("tools/run.sh", 120, Kind::Exec),
            ("docs/empty.txt", 0, Kind::Text),
            ("link-to-docs", 0, Kind::Symlink),
            ("src/deep/a/b/c/d/e/Leaf.java", 500, Kind::Text),
        ] {
            files.push(FileSpec { path: path.into(), size, kind });
        }
    }

    let mut child = git(repo)
        .args(["fast-import", "--quiet", "--date-format=raw"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .context("spawning git fast-import")?;
    let mut s = Stream { w: BufWriter::with_capacity(1 << 20, child.stdin.take().unwrap()), mark: 0 };

    let mut version = vec![0u32; files.len()];
    let mut live: Vec<u32> = (1..files.len() as u32).collect();

    // Root commit with every file.
    let root_mark = s.begin("refs/heads/main", 0, BASE_TIME, "Initial import", None, None)?;
    s.raw("100644", ".gitignore", b"build/\n*.log\nnode_modules/\n.idea/\n")?;
    for (id, f) in files.iter().enumerate().skip(1) {
        s.modify(id as u32, f, 0)?;
    }
    s.end()?;

    // Linear history on main.
    let mut main_marks = vec![root_mark];
    for i in 1..p.main_commits {
        let recent = i + p.recent_window >= p.main_commits;
        let n = if recent { r.count_with_mean(p.files_per_commit) } else { 1 + r.below(2) };
        let msg = format!("PROJ-{} {} {} on main", 100 + i, r.pick(WORDS), r.pick(WORDS));
        let m = s.begin("refs/heads/main", r.below(8), BASE_TIME + i as i64 * 1200, &msg, None, None)?;
        change(&mut s, &mut r, &mut files, &mut live, &mut version, n, if recent { 0.06 } else { 0.01 })?;
        s.end()?;
        main_marks.push(m);
    }
    let tip_time = BASE_TIME + p.main_commits as i64 * 1200;
    let main_tip = *main_marks.last().unwrap();

    // Local branches: fork a little behind main, some commits ahead.
    let mut local_branches = Vec::new();
    let mut tracked_branches = Vec::new();
    let mut remote_count = 0usize;
    for b in 0..p.local_branches {
        let kind = ["feature", "feature", "feature", "bugfix", "chore"][r.below(5)];
        let name = format!("{kind}/PROJ-{}-{}-{}", 1000 + b, r.pick(WORDS), r.pick(WORDS));
        let behind = r.below(90.min(main_marks.len()));
        let fork = main_marks[main_marks.len() - 1 - behind];
        let ahead = if b % 10 == 9 {
            0
        } else if r.chance(0.2) {
            20 + r.below(66)
        } else {
            1 + r.below(8)
        };
        let refname = format!("refs/heads/{name}");
        let mut marks = vec![fork];
        // Adds and deletes on a branch must not leak into main's file set.
        let mut branch_live = live.clone();
        if ahead == 0 {
            writeln!(s.w, "reset {refname}\nfrom :{fork}\n")?;
        }
        for c in 0..ahead {
            let msg = format!("PROJ-{} {} {} step {c}", 1000 + b, r.pick(WORDS), r.pick(WORDS));
            let from = if c == 0 { Some(fork) } else { None };
            let m = s.begin(&refname, b, tip_time + (b * 97 + c * 3) as i64 * 60, &msg, from, None)?;
            let n = r.count_with_mean(p.files_per_commit);
            change(&mut s, &mut r, &mut files, &mut branch_live, &mut version, n, 0.1)?;
            s.end()?;
            marks.push(m);
        }
        if r.chance(0.7) && remote_count < p.remote_refs {
            let at = marks[marks.len() - 1 - r.below(marks.len().min(3))];
            writeln!(s.w, "reset refs/remotes/origin/{name}\nfrom :{at}\n")?;
            tracked_branches.push(name.clone());
            remote_count += 1;
        }
        local_branches.push(name);
    }
    writeln!(s.w, "reset refs/remotes/origin/main\nfrom :{}\n", main_marks[main_marks.len().saturating_sub(3)])?;
    tracked_branches.push("main".to_string());
    remote_count += 1;

    // Remote-only branches spread over the whole history.
    let mut i = 0usize;
    while remote_count < p.remote_refs {
        let back = ((-r.f64().max(1e-9).ln()) * main_marks.len() as f64 / 6.0) as usize;
        let fork = main_marks[main_marks.len() - 1 - back.min(main_marks.len() - 1)];
        let refname = format!("refs/remotes/origin/team{}/PROJ-{}-{}", i % 23, 5000 + i, r.pick(WORDS));
        let own = if r.chance(0.45) { 0 } else { r.count_with_mean(1.8) };
        if own == 0 {
            writeln!(s.w, "reset {refname}\nfrom :{fork}\n")?;
        }
        for c in 0..own {
            let msg = format!("PROJ-{} remote work {c}", 5000 + i);
            let from = if c == 0 { Some(fork) } else { None };
            s.begin(&refname, i, BASE_TIME + (main_marks.len() - back.min(main_marks.len() - 1)) as i64 * 1200 + c as i64 * 60, &msg, from, None)?;
            let id = *r.pick(&live);
            let f = &files[id as usize];
            if f.size > 0 && f.size < 64_000 {
                version[id as usize] += 1;
                s.modify(id, f, version[id as usize])?;
            }
            s.end()?;
        }
        remote_count += 1;
        i += 1;
    }

    // Tags along main, every other one annotated.
    for t in 0..p.tags {
        let at = main_marks[(t * (main_marks.len() - 1)) / p.tags.max(1)];
        let name = format!("v{}.{}.{}", 1 + t / 100, (t / 10) % 10, t % 10);
        if t % 2 == 0 {
            let msg = format!("Release {name}");
            write!(s.w, "tag {name}\nfrom :{at}\ntagger Release Bot <bot@example.invalid> {} +0000\ndata {}\n{msg}\n", BASE_TIME + t as i64, msg.len())?;
        } else {
            writeln!(s.w, "reset refs/tags/{name}\nfrom :{at}\n")?;
        }
    }

    // Stashes: an index commit and a work-tree commit on top of main each.
    for k in 0..p.stashes {
        let time = tip_time + k as i64 * 30;
        let idx = s.begin(&format!("refs/lazyer-tmp/idx/{k:04}"), 0, time, &format!("index on main: change set {k}"), Some(main_tip), None)?;
        s.end()?;
        s.begin(&format!("refs/lazyer-tmp/stash/{k:04}"), 0, time, &format!("WIP on main: change set {k}"), Some(main_tip), Some(idx))?;
        for _ in 0..1 + r.below(3) {
            let id = *r.pick(&live);
            let f = &files[id as usize];
            if f.size > 0 && f.size < 64_000 {
                version[id as usize] += 1;
                s.modify(id, f, version[id as usize])?;
            }
        }
        s.end()?;
    }

    s.w.flush()?;
    drop(s);
    let out = child.wait_with_output()?;
    if !out.status.success() {
        bail!("git fast-import failed: {}", String::from_utf8_lossy(&out.stderr));
    }
    let _ = output(git(repo).args(["rev-parse", "--verify", "-q", "refs/heads/main"]))?;

    Ok(Plan { dirs: dirs.into_iter().map(|(d, _)| d).collect(), local_branches, tracked_branches })
}
