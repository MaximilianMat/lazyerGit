//! The acceptance test of the native engine: on every scenario it either
//! produces exactly what git produces, or it declines and git takes over.
//! Anything else is a bug.

use lazyer_core::Native;
use lazyer_testkit::{differential, scenario, GitCli, Profile, Verdict};

/// Scenarios the engine must handle itself. Grows as operations are
/// implemented; a scenario in this list that falls back fails the test.
const NATIVE: &[&str] = &[
    "status_clean",
    "status_dirty",
    "status_changes",
    "status_rename",
    "status_two_hunks",
    "view_main",
    "view_other_worktree",
    "view_merges",
    "switch_near",
    "switch_far",
    "switch_dirty",
    "switch_blocked",
    "commit_staged",
    "amend_head",
    "amend_older_d3",
    "amend_older_d20",
    "move_patch_d3",
    "swap_branch",
    "swap_branch_detach",
];

#[test]
fn native_engine_matches_git() {
    let profile = Profile::small();
    let reference = GitCli::real();
    let native = Native::isolated();
    let mut failures = Vec::new();
    let mut report = Vec::new();
    std::thread::scope(|s| {
        let handles: Vec<_> = scenario::all()
            .into_iter()
            .map(|sc| {
                let (profile, reference, native) = (&profile, &reference, &native);
                s.spawn(move || (sc.name, differential(profile, &sc, reference, native)))
            })
            .collect();
        for h in handles {
            let (name, verdict) = h.join().unwrap();
            match verdict {
                Ok(Verdict::Match) => report.push(format!("  native    {name}")),
                Ok(Verdict::Fallback(why)) => {
                    report.push(format!("  fallback  {name}  ({why})"));
                    if NATIVE.contains(&name) {
                        failures.push(format!("{name}: must be native but fell back: {why}"));
                    }
                }
                Ok(Verdict::Mismatch(problems)) => failures.push(format!("{name}: MISMATCH\n  {}", problems.join("\n  "))),
                Err(e) => failures.push(format!("{name}: harness error: {e:#}")),
            }
        }
    });
    println!("{}", report.join("\n"));
    assert!(failures.is_empty(), "\n{}", failures.join("\n"));
}

/// The one-step swap the daemon offers to `git-worktree-swap` must leave the
/// repository exactly as the script's own sequence of git commands does.
#[test]
fn swap_command_matches_the_script() {
    use lazyer_core::Backend;
    use lazyer_testkit::differential::compared_worktrees;
    use lazyer_testkit::snapshot::{differences, snapshot};
    use lazyer_testkit::Fixture;

    let profile = Profile::small();
    for detach in [false, true] {
        let (fa, fb) = (Fixture::temp(&profile).unwrap(), Fixture::temp(&profile).unwrap());
        let branch = lazyer_testkit::git::run(&fa.worktree(1), &["symbolic-ref", "--short", "HEAD"]).unwrap();
        GitCli::real().swap_branch(&fa.repo, &branch, detach).unwrap();

        let mut argv = vec!["lazyer-swap-branch".to_string(), branch.clone()];
        if detach {
            argv.push("--detach-other".into());
        }
        let out = Native::isolated().emulate(&fb.repo, &argv).expect("the swap is handled");
        let text = String::from_utf8_lossy(&out.stdout).to_string();
        assert_eq!(out.code, 0);
        assert!(text.contains(&format!("-> {branch}")) && text.contains(if detach { "left detached" } else { "Swapped:" }), "{text}");

        let a = snapshot(&fa.repo, &compared_worktrees(&profile, &fa)).unwrap();
        let b = snapshot(&fb.repo, &compared_worktrees(&profile, &fb)).unwrap();
        assert!(differences(&a, &b).is_empty(), "{:#?}", differences(&a, &b));

        // A branch no other worktree holds is not this command's business.
        assert!(Native::isolated().emulate(&fb.repo, &["lazyer-swap-branch".to_string(), "no-such-branch".to_string()]).is_none());
    }
}

/// Moving one hunk of an older commit into the index has to end where the
/// rebase lazygit drives ends: stop at the commit, take the hunk out, amend,
/// continue, apply the hunk to the index.
#[test]
fn moving_a_hunk_out_of_a_commit_matches_the_rebase() {
    use lazyer_testkit::differential::compared_worktrees;
    use lazyer_testkit::git::{git, output, run};
    use lazyer_testkit::scenario::committer;
    use lazyer_testkit::snapshot::{differences, snapshot};
    use lazyer_testkit::Fixture;

    let profile = Profile::small();
    let (fa, fb) = (Fixture::temp(&profile).unwrap(), Fixture::temp(&profile).unwrap());
    let mut prepared = Vec::new();
    for fx in [&fa, &fb] {
        let wt = &fx.repo;
        run(wt, &["checkout", "-q", "-b", "scenario/hunks"]).unwrap();
        let files: Vec<String> = run(wt, &["ls-files", "--", "*.java"]).unwrap().lines().map(str::to_string).collect();
        let target = files.iter().find(|f| std::fs::read_to_string(wt.join(f)).is_ok_and(|t| t.lines().count() >= 14)).unwrap().clone();
        let others: Vec<&String> = files.iter().filter(|f| **f != target).take(3).collect();
        // The commit to edit: two hunks in one file, plus another file.
        let text = std::fs::read_to_string(wt.join(&target)).unwrap();
        let mut ls: Vec<String> = text.lines().map(str::to_string).collect();
        let n = ls.len();
        ls[0] = "first line changed".into();
        ls[n - 1] = "last line changed".into();
        std::fs::write(wt.join(&target), ls.join("\n") + "\n").unwrap();
        std::fs::write(wt.join(others[0]), "rewritten\n").unwrap();
        run(wt, &["commit", "-q", "-am", "two hunks and another file"]).unwrap();
        let commit = run(wt, &["rev-parse", "HEAD"]).unwrap();
        for (i, f) in others[1..].iter().enumerate() {
            std::fs::write(wt.join(f), format!("later {i}\n")).unwrap();
            run(wt, &["commit", "-q", "-am", &format!("later {i}")]).unwrap();
        }
        // The selection: the second hunk of the target file.
        let diff = run(wt, &["diff", &format!("{commit}^"), &commit, "--", &target]).unwrap() + "\n";
        let first = diff.find("\n@@ ").unwrap() + 1;
        let second = first + diff[first + 1..].find("\n@@ ").unwrap() + 2;
        let patch = format!("{}{}", &diff[..first], &diff[second..]);
        let file = fx.root.join("hunk.patch");
        std::fs::write(&file, &patch).unwrap();
        prepared.push((commit, patch, file));
    }
    assert_eq!(prepared[0].0, prepared[1].0);
    let who = committer();

    // Reference: the rebase.
    let (commit, _, file) = &prepared[0];
    let file = file.to_str().unwrap();
    let step = |args: &[&str], editor: Option<&str>| {
        let mut c = git(&fa.repo);
        c.env("GIT_COMMITTER_DATE", who.git_date()).env("GIT_EDITOR", "true");
        if let Some(e) = editor {
            c.env("GIT_SEQUENCE_EDITOR", e);
        }
        output(c.args(args)).unwrap();
    };
    step(
        &["rebase", "-q", "--interactive", "--autostash", "--keep-empty", "--no-autosquash", "--rebase-merges", &format!("{commit}^")],
        Some("perl -pi -e 'if (!$done && s/^pick/edit/) { $done = 1 }'"),
    );
    step(&["apply", "--index", "--3way", "--reverse", file], None);
    step(&["commit", "-q", "--amend", "--no-edit", "--allow-empty"], None);
    step(&["rebase", "--continue"], None);
    step(&["apply", "--index", "--3way", file], None);

    // Candidate: the engine.
    Native::isolated().move_patch_text_to_index(&fb.repo, &prepared[1].0, prepared[1].1.as_bytes(), &who).unwrap();

    let a = snapshot(&fa.repo, &compared_worktrees(&profile, &fa)).unwrap();
    let b = snapshot(&fb.repo, &compared_worktrees(&profile, &fb)).unwrap();
    assert!(differences(&a, &b).is_empty(), "{:#?}", differences(&a, &b));
    // The hunk is staged, the first one is still committed.
    assert!(run(&fb.repo, &["diff", "--cached", "--stat"]).unwrap().contains("1 file changed"));
}
