//! The units of work every backend has to pass: prepare a state with real
//! git, run one backend operation, compare the end state.

use crate::fixture::Fixture;
use crate::git::{git, output, output_with_stdin, run, TEST_EMAIL, TEST_NAME, TEST_TIME};
use anyhow::{bail, Context, Result};
use lazyer_core::{Backend, Ident};
use std::path::{Path, PathBuf};

/// What `prepare` hands to `run`.
pub struct Ctx {
    pub wt: PathBuf,
    pub args: Vec<String>,
}

pub struct Scenario {
    pub name: &'static str,
    pub about: &'static str,
    /// Untimed setup, done with real git on a baseline fixture.
    pub prepare: fn(&Fixture) -> Result<Ctx>,
    /// The operation under test. The returned text is compared as well.
    pub run: fn(&dyn Backend, &Ctx) -> Result<String>,
    /// Git refuses this operation; a backend has to refuse it too and leave
    /// everything as it was.
    pub must_fail: bool,
}

pub fn author() -> Ident {
    Ident::new(TEST_NAME, TEST_EMAIL, TEST_TIME + 100)
}

pub fn committer() -> Ident {
    Ident::new(TEST_NAME, TEST_EMAIL, TEST_TIME + 200)
}

pub fn all() -> Vec<Scenario> {
    vec![
        Scenario { name: "status_clean", about: "status of an unchanged worktree", prepare: |fx| ctx(fx, &[]), run: run_status, must_fail: false },
        Scenario {
            name: "status_dirty",
            about: "status with staged, unstaged, deleted and untracked files",
            prepare: prep_dirty,
            run: run_status,
            must_fail: false,
        },
        Scenario { name: "status_rename", about: "status with a staged rename", prepare: prep_rename, run: run_status, must_fail: false },
        Scenario {
            name: "status_changes",
            about: "status, then changes on disk, then status again",
            prepare: |fx| ctx(fx, &[]),
            run: run_status_twice,
            must_fail: false,
        },
        Scenario { name: "view_main", about: "everything the UI shows for the main worktree", prepare: |fx| ctx(fx, &[]), run: run_view, must_fail: false },
        Scenario {
            name: "view_other_worktree",
            about: "worktree switch: load the view of another worktree",
            prepare: prep_other_worktree,
            run: run_view,
            must_fail: false,
        },
        Scenario { name: "view_merges", about: "the view when HEAD's history contains merges", prepare: prep_merges, run: run_view, must_fail: false },
        Scenario {
            name: "switch_near",
            about: "check out a branch a few commits away",
            prepare: |fx| prep_switch(fx, false, false),
            run: run_switch,
            must_fail: false,
        },
        Scenario {
            name: "switch_far",
            about: "check out the most distant free branch",
            prepare: |fx| prep_switch(fx, true, false),
            run: run_switch,
            must_fail: false,
        },
        Scenario {
            name: "switch_dirty",
            about: "check out a branch with unrelated local changes",
            prepare: |fx| prep_switch(fx, true, true),
            run: run_switch,
            must_fail: false,
        },
        Scenario {
            name: "switch_blocked",
            about: "check out a branch that would overwrite a local change",
            prepare: prep_switch_blocked,
            run: run_switch,
            must_fail: true,
        },
        Scenario { name: "commit_staged", about: "commit staged changes, leave unstaged ones alone", prepare: prep_commit, run: run_commit, must_fail: false },
        Scenario {
            name: "amend_head",
            about: "amend HEAD with staged changes",
            prepare: |fx| prep_history(fx, 1, 0, true),
            run: run_amend_head,
            must_fail: false,
        },
        Scenario {
            name: "amend_older_d3",
            about: "amend the commit 3 below HEAD with staged changes",
            prepare: |fx| prep_history(fx, 5, 3, true),
            run: run_amend_older,
            must_fail: false,
        },
        Scenario {
            name: "amend_older_d20",
            about: "amend the commit 20 below HEAD with staged changes",
            prepare: |fx| prep_history(fx, 22, 20, true),
            run: run_amend_older,
            must_fail: false,
        },
        Scenario {
            name: "move_patch_d3",
            about: "move one file's changes out of the commit 3 below HEAD into the index",
            prepare: |fx| prep_history(fx, 5, 3, false),
            run: run_move_patch,
            must_fail: false,
        },
        Scenario {
            name: "amend_older_dirty",
            about: "amend an older commit while other files have unstaged changes",
            prepare: |fx| prep_history(fx, 5, 3, true).and_then(make_dirty),
            run: run_amend_older,
            must_fail: false,
        },
        Scenario {
            name: "move_patch_dirty",
            about: "move a patch out of an older commit while other files have unstaged changes",
            prepare: |fx| prep_history(fx, 5, 3, false).and_then(make_dirty),
            run: run_move_patch,
            must_fail: false,
        },
        Scenario {
            name: "swap_branch",
            about: "take a branch from another worktree, swap",
            prepare: prep_swap,
            run: |b, c| run_swap(b, c, false),
            must_fail: false,
        },
        Scenario {
            name: "swap_branch_detach",
            about: "take a branch from another worktree, leave it detached",
            prepare: prep_swap,
            run: |b, c| run_swap(b, c, true),
            must_fail: false,
        },
    ]
}

// ------------------------------------------------------------------ helpers

fn ctx(fx: &Fixture, args: &[String]) -> Result<Ctx> {
    Ok(Ctx { wt: fx.repo.clone(), args: args.to_vec() })
}

/// `n` tracked source files, spread evenly over the tree.
fn pick_files(wt: &Path, n: usize) -> Result<Vec<String>> {
    let all: Vec<String> = run(wt, &["ls-files", "--", "*.java"])?.lines().map(str::to_string).collect();
    if all.len() < n {
        bail!("fixture has only {} source files", all.len());
    }
    let step = all.len() / n;
    Ok((0..n).map(|i| all[i * step].clone()).collect())
}

fn prepend(wt: &Path, path: &str, text: &str) -> Result<()> {
    let p = wt.join(path);
    let old = std::fs::read(&p).with_context(|| format!("reading {path}"))?;
    let mut new = text.as_bytes().to_vec();
    new.extend_from_slice(&old);
    Ok(std::fs::write(p, new)?)
}

fn append(wt: &Path, path: &str, text: &str) -> Result<()> {
    let p = wt.join(path);
    let mut data = std::fs::read(&p).with_context(|| format!("reading {path}"))?;
    data.extend_from_slice(text.as_bytes());
    Ok(std::fs::write(p, data)?)
}

fn add(wt: &Path, paths: &[String]) -> Result<()> {
    let mut input = Vec::new();
    for p in paths {
        input.extend_from_slice(p.as_bytes());
        input.push(0);
    }
    output_with_stdin(git(wt).args(["add", "--pathspec-from-file=-", "--pathspec-file-nul"]), &input).map(|_| ())
}

/// Local branches no worktree has checked out, with their distance from HEAD.
fn free_branches(wt: &Path) -> Result<Vec<(String, u32)>> {
    let taken: Vec<String> =
        run(wt, &["worktree", "list", "--porcelain"])?.lines().filter_map(|l| l.strip_prefix("branch refs/heads/").map(str::to_string)).collect();
    let mut out = Vec::new();
    for line in run(wt, &["for-each-ref", "--format=%(refname:short) %(ahead-behind:HEAD)", "refs/heads"])?.lines() {
        let f: Vec<&str> = line.split(' ').collect();
        if f.len() == 3 && !taken.iter().any(|t| t == f[0]) && !f[0].starts_with("scenario/") {
            let d = f[1].parse::<u32>().unwrap_or(0) + f[2].parse::<u32>().unwrap_or(0);
            out.push((f[0].to_string(), d));
        }
    }
    out.sort_by(|a, b| a.1.cmp(&b.1).then(a.0.cmp(&b.0)));
    Ok(out)
}

// ---------------------------------------------------------------- prepare

fn prep_dirty(fx: &Fixture) -> Result<Ctx> {
    let wt = &fx.repo;
    let f = pick_files(wt, 16)?;
    for p in &f[0..8] {
        append(wt, p, "unstaged change\n")?;
    }
    for p in &f[8..12] {
        prepend(wt, p, "staged change\n")?;
    }
    add(wt, &f[8..12])?;
    append(wt, &f[8], "staged and then changed again\n")?;
    std::fs::remove_file(wt.join(&f[12]))?;
    run(wt, &["rm", "-q", "--", &f[13]])?;
    std::fs::write(wt.join("new-untracked.txt"), "new\n")?;
    std::fs::create_dir_all(wt.join("fresh/dir"))?;
    std::fs::write(wt.join("fresh/dir/a.txt"), "a\n")?;
    std::fs::write(wt.join("fresh/staged.txt"), "staged new file\n")?;
    add(wt, &["fresh/staged.txt".to_string()])?;
    ctx(fx, &[])
}

fn prep_rename(fx: &Fixture) -> Result<Ctx> {
    let wt = &fx.repo;
    let f = pick_files(wt, 3)?;
    run(wt, &["mv", "--", &f[0], "renamed-exact.java"])?;
    run(wt, &["mv", "--", &f[1], "renamed-edited.java"])?;
    append(wt, "renamed-edited.java", "a small edit after the rename\n")?;
    add(wt, &["renamed-edited.java".to_string()])?;
    ctx(fx, &[])
}

fn prep_other_worktree(fx: &Fixture) -> Result<Ctx> {
    Ok(Ctx { wt: fx.worktree(1), args: vec![] })
}

/// Merge three branches into main (keeping main's content, so there are no
/// conflicts), with a commit in between: the log has to interleave the side
/// histories exactly as git does.
fn prep_merges(fx: &Fixture) -> Result<Ctx> {
    let wt = &fx.repo;
    let branches = free_branches(wt)?;
    let picks: Vec<&(String, u32)> = branches.iter().filter(|b| b.1 > 0).rev().take(3).collect();
    for (i, (name, _)) in picks.iter().enumerate() {
        output(git(wt).env("GIT_COMMITTER_DATE", format!("{} +0000", TEST_TIME + 10 * i as i64)).args([
            "merge",
            "-q",
            "--no-ff",
            "-s",
            "ours",
            "-m",
            &format!("merge {i}"),
            name,
        ]))?;
        if i == 0 {
            let f = pick_files(wt, 1)?;
            append(wt, &f[0], "between merges\n")?;
            add(wt, &f)?;
            run(wt, &["commit", "-q", "-m", "between merges"])?;
        }
    }
    ctx(fx, &[])
}

fn prep_switch(fx: &Fixture, far: bool, dirty: bool) -> Result<Ctx> {
    let wt = &fx.repo;
    let branches = free_branches(wt)?;
    let candidates: Vec<_> = branches.iter().filter(|b| b.1 > 0).collect();
    let target = if far { candidates.last() } else { candidates.first() }.context("no free branch")?.0.clone();
    if dirty {
        let differing: std::collections::HashSet<String> = run(wt, &["diff", "--name-only", "HEAD", &target])?.lines().map(str::to_string).collect();
        let untouched: Vec<String> = pick_files(wt, 40)?.into_iter().filter(|p| !differing.contains(p)).take(4).collect();
        for p in &untouched[..2] {
            append(wt, p, "local edit that must survive the switch\n")?;
        }
        prepend(wt, &untouched[2], "staged edit that must survive the switch\n")?;
        add(wt, &untouched[2..3])?;
        std::fs::write(wt.join("untracked-survivor.txt"), "still here\n")?;
    }
    ctx(fx, &[target])
}

fn prep_switch_blocked(fx: &Fixture) -> Result<Ctx> {
    let wt = &fx.repo;
    let branches = free_branches(wt)?;
    for (target, _) in branches.iter().rev() {
        let differing = run(wt, &["diff", "--name-only", "--diff-filter=M", "HEAD", target])?;
        if let Some(path) = differing.lines().next() {
            append(wt, path, "local edit in the way\n")?;
            return ctx(fx, &[target.clone()]);
        }
    }
    bail!("no branch modifies a file relative to HEAD")
}

fn prep_commit(fx: &Fixture) -> Result<Ctx> {
    let wt = &fx.repo;
    let f = pick_files(wt, 8)?;
    for p in &f[0..5] {
        prepend(wt, p, "change to commit\n")?;
    }
    std::fs::create_dir_all(wt.join("added"))?;
    std::fs::write(wt.join("added/New.java"), "class New {}\n")?;
    let mut staged = f[0..5].to_vec();
    staged.push("added/New.java".into());
    add(wt, &staged)?;
    run(wt, &["rm", "-q", "--", &f[5]])?;
    append(wt, &f[6], "unstaged, stays out of the commit\n")?;
    ctx(fx, &["Add New and adjust five files".to_string()])
}

/// A branch `scenario/work` with `commits` commits of three files each. With
/// `stage`, also staged changes meant for the commit `depth` below HEAD: an
/// edit to one of its files and a new file. Args: target commit, one path
/// that commit changed.
fn prep_history(fx: &Fixture, commits: usize, depth: usize, stage: bool) -> Result<Ctx> {
    let wt = &fx.repo;
    run(wt, &["checkout", "-q", "-b", "scenario/work"])?;
    let files = pick_files(wt, commits * 3)?;
    for k in 0..commits {
        let set = &files[k * 3..k * 3 + 3];
        for p in set {
            prepend(wt, p, &format!("scenario commit {k}\n"))?;
        }
        add(wt, set)?;
        output(git(wt).env("GIT_COMMITTER_DATE", format!("{} +0000", TEST_TIME + k as i64)).args(["commit", "-q", "-m", &format!("scenario commit {k}")]))?;
    }
    let target_index = commits - 1 - depth;
    let target = run(wt, &["rev-parse", &format!("HEAD~{depth}")])?;
    let touched = &files[target_index * 3..target_index * 3 + 3];
    if stage {
        append(wt, &touched[1], "amended into the target commit\n")?;
        std::fs::write(wt.join("amend-extra.txt"), "new file for the target commit\n")?;
        add(wt, &[touched[1].clone(), "amend-extra.txt".to_string()])?;
    }
    ctx(fx, &[target, touched[0].clone()])
}

/// Unstaged edits to tracked files no scenario commit touches, and an
/// untracked file.
fn make_dirty(c: Ctx) -> Result<Ctx> {
    let all: Vec<String> = run(&c.wt, &["ls-files", "--", "*.ts"])?.lines().map(str::to_string).collect();
    for p in all.iter().take(2) {
        append(&c.wt, p, "unstaged work in progress\n")?;
    }
    std::fs::write(c.wt.join("wip-untracked.txt"), "untracked work in progress\n")?;
    Ok(c)
}

fn prep_swap(fx: &Fixture) -> Result<Ctx> {
    let other = fx.worktree(1);
    let branch = run(&other, &["symbolic-ref", "--short", "HEAD"])?;
    ctx(fx, &[branch])
}

// -------------------------------------------------------------------- run

fn run_status(b: &dyn Backend, c: &Ctx) -> Result<String> {
    Ok(format!("{:#?}", b.status(&c.wt)?))
}

/// A backend may remember things between calls; whatever it remembers has to
/// notice every kind of change made behind its back.
fn run_status_twice(b: &dyn Backend, c: &Ctx) -> Result<String> {
    let wt = &c.wt;
    let first = format!("{:#?}", b.status(wt)?);
    let f = pick_files(wt, 6)?;
    // New files in an existing and in a new directory, a removed file, an
    // edit that keeps the size, a staged change, a file that becomes
    // ignored, and a new ignore rule for an existing untracked file.
    let dir = Path::new(&f[0]).parent().unwrap_or(Path::new("")).to_path_buf();
    std::fs::write(wt.join(&dir).join("brand-new.txt"), "x\n")?;
    std::fs::create_dir_all(wt.join("made/later"))?;
    std::fs::write(wt.join("made/later/file.txt"), "y\n")?;
    std::fs::remove_file(wt.join(&f[1]))?;
    let mut data = std::fs::read(wt.join(&f[2]))?;
    if let Some(byte) = data.first_mut() {
        *byte = if *byte == b'z' { b'y' } else { b'z' };
    }
    std::fs::write(wt.join(&f[2]), data)?;
    prepend(wt, &f[3], "staged later\n")?;
    add(wt, &f[3..4])?;
    std::fs::write(wt.join(&dir).join("debug.log"), "ignored by *.log\n")?;
    append(wt, ".gitignore", "notes.txt\n")?;
    std::fs::remove_file(wt.join("scratch/todo.md"))?;
    let second = format!("{:#?}", b.status(wt)?);
    // And back again: nothing may stick.
    std::fs::remove_file(wt.join(&dir).join("brand-new.txt"))?;
    std::fs::remove_dir_all(wt.join("made"))?;
    let third = format!("{:#?}", b.status(wt)?);
    Ok(format!("{first}\n{second}\n{third}"))
}

fn run_view(b: &dyn Backend, c: &Ctx) -> Result<String> {
    Ok(format!("{:#?}", b.load_view(&c.wt)?))
}

fn run_switch(b: &dyn Backend, c: &Ctx) -> Result<String> {
    b.switch_branch(&c.wt, &c.args[0])?;
    Ok(String::new())
}

fn run_commit(b: &dyn Backend, c: &Ctx) -> Result<String> {
    b.commit(&c.wt, &c.args[0], &author(), &committer())?;
    Ok(String::new())
}

fn run_amend_head(b: &dyn Backend, c: &Ctx) -> Result<String> {
    b.amend_head(&c.wt, &committer())?;
    Ok(String::new())
}

fn run_amend_older(b: &dyn Backend, c: &Ctx) -> Result<String> {
    b.amend_older(&c.wt, &c.args[0], &committer())?;
    Ok(String::new())
}

fn run_move_patch(b: &dyn Backend, c: &Ctx) -> Result<String> {
    b.move_patch_to_index(&c.wt, &c.args[0], &c.args[1..2], &committer())?;
    Ok(String::new())
}

fn run_swap(b: &dyn Backend, c: &Ctx, detach: bool) -> Result<String> {
    b.swap_branch(&c.wt, &c.args[0], detach)?;
    Ok(String::new())
}
