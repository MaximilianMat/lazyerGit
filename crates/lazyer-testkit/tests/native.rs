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
