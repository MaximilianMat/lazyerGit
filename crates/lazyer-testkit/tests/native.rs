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
