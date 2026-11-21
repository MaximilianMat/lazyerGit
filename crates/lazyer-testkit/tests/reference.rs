//! The harness itself must be deterministic: the reference backend compared
//! with itself has to match on every scenario, otherwise no verdict about a
//! candidate means anything.

use lazyer_testkit::{differential, scenario, GitCli, Profile, Verdict};

#[test]
fn reference_matches_itself_on_every_scenario() {
    let profile = Profile::small();
    let reference = GitCli::real();
    let mut failures = Vec::new();
    std::thread::scope(|s| {
        let handles: Vec<_> = scenario::all()
            .into_iter()
            .map(|sc| {
                let (profile, reference) = (&profile, &reference);
                s.spawn(move || (sc.name, differential(profile, &sc, reference, reference)))
            })
            .collect();
        for h in handles {
            let (name, verdict) = h.join().unwrap();
            match verdict {
                Ok(Verdict::Match) => {}
                other => failures.push(format!("{name}: {other:#?}")),
            }
        }
    });
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// A backend that is subtly wrong in every mutating operation. The harness
/// has to notice each time.
struct Sabotaged(GitCli);

impl lazyer_core::Backend for Sabotaged {
    fn name(&self) -> &str {
        "sabotaged"
    }
    fn status(&self, wt: &std::path::Path) -> lazyer_core::Result<lazyer_core::Status> {
        let mut s = self.0.status(wt)?;
        s.entries.pop();
        Ok(s)
    }
    fn load_view(&self, wt: &std::path::Path) -> lazyer_core::Result<lazyer_core::RepoView> {
        let mut v = self.0.load_view(wt)?;
        v.commits.truncate(v.commits.len() - 1);
        Ok(v)
    }
    fn switch_branch(&self, wt: &std::path::Path, branch: &str) -> lazyer_core::Result<()> {
        self.0.switch_branch(wt, branch)?;
        std::fs::write(wt.join(".gitignore"), "build/\n*.log\nnode_modules/\n.idea/\n\n")?;
        Ok(())
    }
    fn commit(&self, wt: &std::path::Path, message: &str, a: &lazyer_core::Ident, c: &lazyer_core::Ident) -> lazyer_core::Result<()> {
        let mut c2 = c.clone();
        c2.time += 1;
        self.0.commit(wt, message, a, &c2)
    }
    fn amend_head(&self, wt: &std::path::Path, c: &lazyer_core::Ident) -> lazyer_core::Result<()> {
        self.0.amend_head(wt, c)?;
        std::fs::remove_file(wt.join("notes.txt"))?;
        Ok(())
    }
    fn amend_older(&self, wt: &std::path::Path, target: &str, c: &lazyer_core::Ident) -> lazyer_core::Result<()> {
        // Amends the commit above the requested one.
        let above = lazyer_testkit::git::run(wt, &["rev-list", "--reverse", &format!("{target}..HEAD")])?;
        self.0.amend_older(wt, above.lines().next().unwrap(), c)
    }
    fn move_patch_to_index(&self, wt: &std::path::Path, commit: &str, paths: &[String], c: &lazyer_core::Ident) -> lazyer_core::Result<()> {
        self.0.move_patch_to_index(wt, commit, paths, c)?;
        // Leaves the moved change unstaged instead of staged.
        lazyer_testkit::git::run(wt, &["reset", "-q"]).map(|_| ())
    }
    fn swap_branch(&self, wt: &std::path::Path, branch: &str, _detach_other: bool) -> lazyer_core::Result<()> {
        self.0.swap_branch(wt, branch, true)
    }
}

#[test]
fn harness_notices_a_wrong_backend() {
    let profile = Profile::small();
    let reference = GitCli::real();
    let wrong = Sabotaged(GitCli::real());
    let mut missed = Vec::new();
    std::thread::scope(|s| {
        let handles: Vec<_> = scenario::all()
            .into_iter()
            // The sabotage of these is invisible by construction: a clean
            // status has no entry to drop, the refused switch never runs the
            // extra write, and detaching is what swap_branch_detach asks for.
            .filter(|sc| !["status_clean", "switch_blocked", "swap_branch_detach"].contains(&sc.name))
            .map(|sc| {
                let (profile, reference, wrong) = (&profile, &reference, &wrong);
                s.spawn(move || (sc.name, differential(profile, &sc, reference, wrong)))
            })
            .collect();
        for h in handles {
            let (name, verdict) = h.join().unwrap();
            if !matches!(verdict, Ok(Verdict::Mismatch(_))) {
                missed.push(format!("{name}: {verdict:?}"));
            }
        }
    });
    assert!(missed.is_empty(), "not detected:\n{}", missed.join("\n"));
}
