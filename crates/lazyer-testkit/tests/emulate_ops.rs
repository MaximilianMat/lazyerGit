//! Git commands that change the repository and that the engine performs
//! itself: after `git <args>` on one fixture and the engine on its twin, both
//! repositories have to be in the same state.

use lazyer_core::Native;
use lazyer_testkit::differential::compared_worktrees;
use lazyer_testkit::git::{git, run};
use lazyer_testkit::snapshot::{differences, fsck, snapshot};
use lazyer_testkit::{scenario, Fixture, Profile};
use std::path::Path;

fn lines(wt: &Path, args: &[&str]) -> Vec<String> {
    run(wt, args).unwrap().lines().map(str::to_string).collect()
}

fn s(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|p| p.to_string()).collect()
}

fn with(parts: &[&str], more: &[String]) -> Vec<String> {
    let mut v = s(parts);
    v.extend(more.iter().cloned());
    v
}

type Args = fn(&Path) -> Vec<String>;

/// A tracked source file with first line, last line and two appended lines
/// changed: a diff with two separate hunks. Returns the worktree.
fn two_hunks(fx: &Fixture, staged: bool) -> std::path::PathBuf {
    let wt = fx.repo.clone();
    let file =
        lines(&wt, &["ls-files", "--", "*.java"]).into_iter().find(|f| std::fs::read_to_string(wt.join(f)).is_ok_and(|t| t.lines().count() >= 14)).unwrap();
    let text = std::fs::read_to_string(wt.join(&file)).unwrap();
    let mut ls: Vec<String> = text.lines().map(str::to_string).collect();
    let n = ls.len();
    ls[0] = "first line changed".into();
    ls[n - 1] = "last line changed".into();
    ls.push("an added line".into());
    ls.push("another added line".into());
    std::fs::write(wt.join(&file), ls.join("\n") + "\n").unwrap();
    if staged {
        run(&wt, &["add", "--", &file]).unwrap();
    }
    std::fs::write(fx.root.join("hunk-file"), &file).unwrap();
    wt
}

/// The diff of the prepared file, reduced by `edit`, written next to the
/// repository; returns the `git apply` arguments for it.
fn patch_args(wt: &Path, cached: bool, reverse: bool, name: &str, edit: fn(&str) -> String) -> Vec<String> {
    let root = wt.parent().unwrap();
    let file = std::fs::read_to_string(root.join("hunk-file")).unwrap();
    let diff = if cached { run(wt, &["diff", "--cached", "--", &file]) } else { run(wt, &["diff", "--", &file]) }.unwrap() + "\n";
    let path = root.join(format!("{name}.patch"));
    std::fs::write(&path, edit(&diff)).unwrap();
    let mut argv = s(&["apply", "--cached"]);
    if reverse {
        argv.push("--reverse".into());
    }
    argv.push(path.to_str().unwrap().to_string());
    argv
}

/// File header plus the `n`-th hunk only.
fn only_hunk(diff: &str, n: usize) -> String {
    let first = diff.find("\n@@ ").unwrap() + 1;
    let hunks: Vec<&str> = diff[first..].split("\n@@ ").collect();
    let body = hunks[n].trim_start_matches("@@ ");
    format!("{}@@ {}{}", &diff[..first], body, if body.ends_with('\n') { "" } else { "\n" })
}

/// Rewrite the counts in a single-hunk patch's header.
fn with_counts(patch: &str, old_delta: i64, new_delta: i64) -> String {
    let at = patch.find("\n@@ -").unwrap() + 1;
    let end = at + patch[at..].find('\n').unwrap();
    let header = &patch[at..end];
    let parts: Vec<&str> = header.split(' ').collect();
    let bump = |spec: &str, delta: i64| {
        let (start, count) = spec[1..].split_once(',').unwrap();
        format!("{}{start},{}", &spec[..1], count.parse::<i64>().unwrap() + delta)
    };
    format!("{}@@ {} {} @@{}", &patch[..at], bump(parts[1], old_delta), bump(parts[2], new_delta), &patch[end..])
}

/// The second hunk without its last added line: a line-level selection.
fn second_hunk_without_last_added_line(diff: &str) -> String {
    let hunk = only_hunk(diff, 1);
    let cut = hunk.trim_end_matches('\n').rfind("\n+").unwrap() + 1;
    with_counts(&hunk[..cut], 0, -1)
}

/// The second hunk with its last added line kept as context: what unstaging
/// every selected line but that one looks like.
fn second_hunk_keeping_last_added_line(diff: &str) -> String {
    let hunk = only_hunk(diff, 1);
    let cut = hunk.trim_end_matches('\n').rfind("\n+").unwrap() + 1;
    with_counts(&format!("{} {}", &hunk[..cut], &hunk[cut + 1..]), 1, 0)
}

/// (name, start state, arguments). The start states are those of the
/// scenarios of the same name.
fn cases() -> Vec<(&'static str, &'static str, Args)> {
    let unstaged_modified: Args = |wt| with(&["add", "--"], &lines(wt, &["diff", "--name-only", "--diff-filter=M"])[..1]);
    vec![
        ("stage a modified file", "status_dirty", unstaged_modified),
        ("stage a deleted file", "status_dirty", |wt| with(&["add", "--"], &lines(wt, &["diff", "--name-only", "--diff-filter=D"])[..1])),
        ("stage an untracked file", "status_dirty", |_| s(&["add", "--", "new-untracked.txt"])),
        ("stage a directory", "status_dirty", |_| s(&["add", "--", "fresh"])),
        ("stage several paths", "status_dirty", |wt| with(&["add", "--", "new-untracked.txt"], &lines(wt, &["diff", "--name-only"]))),
        ("stage everything", "status_dirty", |_| s(&["add", "-A"])),
        ("stage everything, renames", "status_rename", |_| s(&["add", "-A"])),
        ("stage tracked only", "status_dirty", |_| s(&["add", "-u"])),
        ("stage tracked only, paths", "status_dirty", |_| s(&["add", "-u", "--", "."])),
        ("stage dot", "status_dirty", |_| s(&["add", "--", "."])),
        ("unstage everything", "status_dirty", |_| s(&["reset"])),
        ("unstage everything, renames", "status_rename", |_| s(&["reset"])),
        ("unstage a modified file", "status_dirty", |wt| {
            with(&["reset", "HEAD", "--"], &lines(wt, &["diff", "--cached", "--name-only", "--diff-filter=M"])[..1])
        }),
        ("unstage a deletion", "status_dirty", |wt| with(&["reset", "HEAD", "--"], &lines(wt, &["diff", "--cached", "--name-only", "--diff-filter=D"])[..1])),
        ("unstage a new file", "status_dirty", |_| s(&["reset", "HEAD", "--", "fresh/staged.txt"])),
        ("unstage a directory", "status_dirty", |_| s(&["reset", "HEAD", "--", "fresh"])),
        ("unstage one path, short form", "status_dirty", |wt| with(&["reset", "--"], &lines(wt, &["diff", "--cached", "--name-only", "--diff-filter=M"])[..1])),
        ("forget a newly added file", "status_dirty", |_| s(&["rm", "--cached", "--force", "--", "fresh/staged.txt"])),
        ("discard changes of a file", "status_dirty", |wt| with(&["checkout", "--"], &lines(wt, &["diff", "--name-only", "--diff-filter=M"])[..1])),
        ("restore a deleted file", "status_dirty", |wt| with(&["checkout", "--"], &lines(wt, &["diff", "--name-only", "--diff-filter=D"])[..1])),
        ("discard all unstaged changes", "status_dirty", |_| s(&["checkout", "--", "."])),
        ("soft reset", "commit_staged", |wt| with(&["reset", "--soft"], &lines(wt, &["rev-parse", "HEAD~2"]))),
        ("mixed reset", "commit_staged", |wt| with(&["reset", "--mixed"], &lines(wt, &["rev-parse", "HEAD~2"]))),
        ("mixed reset to HEAD", "status_dirty", |_| s(&["reset", "--mixed", "HEAD"])),
        ("hard reset", "status_dirty", |wt| with(&["reset", "--hard"], &lines(wt, &["rev-parse", "HEAD~3"]))),
        ("hard reset to HEAD", "status_dirty", |_| s(&["reset", "--hard", "HEAD"])),
        ("hard reset with renames", "status_rename", |_| s(&["reset", "--hard", "HEAD"])),
        ("hard reset to another branch", "baseline", |wt| {
            with(&["reset", "--hard"], &lines(wt, &["for-each-ref", "--count=1", "--format=%(refname:short)", "refs/heads/feature"]))
        }),
        ("new branch here", "baseline", |_| s(&["checkout", "-b", "topic/new", "main"])),
        ("new branch here, dirty", "status_dirty", |_| s(&["checkout", "-b", "topic/dirty", "HEAD"])),
        ("new branch at an older commit", "baseline", |wt| with(&["checkout", "-b", "topic/older"], &lines(wt, &["rev-parse", "HEAD~5"]))),
        ("new branch from another branch", "baseline", |wt| {
            with(&["checkout", "-b", "topic/from-other"], &lines(wt, &["for-each-ref", "--count=1", "--format=%(refname:short)", "refs/heads/feature"]))
        }),
        ("new branch without tracking", "baseline", |_| s(&["checkout", "-b", "topic/untracked", "origin/main", "--no-track"])),
        ("new branch, not checked out", "baseline", |wt| with(&["branch", "plain-new"], &lines(wt, &["rev-parse", "HEAD~4"]))),
        ("stage a whole diff as a patch", "two_hunks", |wt| patch_args(wt, false, false, "whole", |d| d.to_string())),
        ("stage the first hunk", "two_hunks", |wt| patch_args(wt, false, false, "first", |d| only_hunk(d, 0))),
        ("stage the second hunk", "two_hunks", |wt| patch_args(wt, false, false, "second", |d| only_hunk(d, 1))),
        ("stage some lines of a hunk", "two_hunks", |wt| patch_args(wt, false, false, "lines", second_hunk_without_last_added_line)),
        ("unstage a whole diff as a patch", "two_hunks_staged", |wt| patch_args(wt, true, true, "un-whole", |d| d.to_string())),
        ("unstage the first hunk", "two_hunks_staged", |wt| patch_args(wt, true, true, "un-first", |d| only_hunk(d, 0))),
        ("unstage the second hunk", "two_hunks_staged", |wt| patch_args(wt, true, true, "un-second", |d| only_hunk(d, 1))),
        ("unstage some lines of a hunk", "two_hunks_staged", |wt| patch_args(wt, true, true, "un-lines", second_hunk_keeping_last_added_line)),
        ("lightweight tag", "baseline", |_| s(&["tag", "--", "v-new"])),
        ("lightweight tag elsewhere", "baseline", |wt| with(&["tag", "--", "v-older"], &lines(wt, &["rev-parse", "HEAD~7"]))),
    ]
}

fn config(fx: &Fixture) -> String {
    run(&fx.repo, &["config", "--local", "--list"]).unwrap().replace(fx.root.to_str().unwrap(), "<root>")
}

fn check(name: &str, state: &str, args: Args) -> Option<String> {
    let profile = Profile::small();
    let (fa, fb) = (Fixture::temp(&profile).unwrap(), Fixture::temp(&profile).unwrap());
    let (wa, wb) = match (state, scenario::all().into_iter().find(|sc| sc.name == state)) {
        ("two_hunks", _) => (two_hunks(&fa, false), two_hunks(&fb, false)),
        ("two_hunks_staged", _) => (two_hunks(&fa, true), two_hunks(&fb, true)),
        (_, Some(sc)) => ((sc.prepare)(&fa).unwrap().wt, (sc.prepare)(&fb).unwrap().wt),
        (_, None) => (fa.repo.clone(), fb.repo.clone()),
    };
    let (argv, argv_b) = (args(&wa), args(&wb));
    let rootless = |v: &[String], fx: &Fixture| v.iter().map(|a| a.replace(fx.root.to_str().unwrap(), "<root>")).collect::<Vec<_>>();
    assert_eq!(rootless(&argv, &fa), rootless(&argv_b, &fb), "{name}: the two fixtures differ before the command");
    let shown = rootless(&argv, &fa).join(" ");
    let real = git(&wa).args(&argv).stdin(std::process::Stdio::null()).output().unwrap();
    if !real.status.success() {
        return Some(format!("{name}: the case is broken, git {shown} failed: {}", String::from_utf8_lossy(&real.stderr).trim()));
    }
    match Native::isolated().emulate(&wb, &argv_b) {
        None => return Some(format!("{name}: not handled: git {shown}")),
        Some(e) if e.code != 0 => return Some(format!("{name}: exit code {} from: git {shown}", e.code)),
        Some(_) => {}
    }
    let a = snapshot(&fa.repo, &compared_worktrees(&profile, &fa)).unwrap();
    let b = snapshot(&fb.repo, &compared_worktrees(&profile, &fb)).unwrap();
    let mut problems = differences(&a, &b);
    if config(&fa) != config(&fb) {
        problems.push("the repository configuration differs".into());
    }
    if let Err(e) = fsck(&fb.repo) {
        problems.push(format!("git fsck: {e:#}"));
    }
    (!problems.is_empty()).then(|| format!("{name}: git {shown}\n  {}", problems.join("\n  ")))
}

#[test]
fn engine_commands_leave_the_state_git_leaves() {
    let mut failures = Vec::new();
    let all = cases();
    for chunk in all.chunks(8) {
        std::thread::scope(|sc| {
            let handles: Vec<_> = chunk.iter().map(|(name, state, args)| sc.spawn(move || check(name, state, *args))).collect();
            for h in handles {
                failures.extend(h.join().unwrap());
            }
        });
    }
    assert!(failures.is_empty(), "\n{}", failures.join("\n\n"));
}

/// Commands git rejects must not be answered with success.
#[test]
fn rejected_commands_are_left_to_git() {
    let profile = Profile::small();
    let fx = Fixture::temp(&profile).unwrap();
    for argv in [
        s(&["add", "--", "no-such-file"]),
        s(&["checkout", "--", "no-such-file"]),
        s(&["checkout", "-b", "main", "HEAD"]),
        s(&["branch", "main", "HEAD"]),
        s(&["tag", "--", "v1.0.0"]),
        s(&["reset", "--hard", "no-such-ref"]),
        s(&["rm", "--cached", "--force", "--", "no-such-file"]),
    ] {
        let real = git(&fx.repo).args(&argv).stdin(std::process::Stdio::null()).output().unwrap();
        assert!(!real.status.success(), "expected git to reject {argv:?}");
        let ours = Native::isolated().emulate(&fx.repo, &argv);
        assert!(ours.as_ref().is_none_or(|e| e.code != 0), "{argv:?} was answered with success");
    }
}
