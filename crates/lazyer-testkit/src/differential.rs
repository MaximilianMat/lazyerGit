//! Run one scenario through the reference and a candidate backend on two
//! identical fixtures and compare the end states.

use crate::fixture::{Fixture, Profile};
use crate::scenario::Scenario;
use crate::snapshot::{differences, fsck, snapshot};
use anyhow::Result;
use lazyer_core::{Backend, Unsupported};
use std::path::PathBuf;

#[derive(Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Same result text and same end state.
    Match,
    /// The candidate declined; the caller would hand the operation to git.
    Fallback(String),
    Mismatch(Vec<String>),
}

/// The worktrees whose state is compared: the main one and every fully
/// checked-out linked one.
pub fn compared_worktrees(profile: &Profile, fx: &Fixture) -> Vec<PathBuf> {
    let mut v = vec![fx.repo.clone()];
    v.extend((1..=profile.full_worktrees).map(|n| fx.worktree(n)));
    v
}

pub fn differential(profile: &Profile, sc: &Scenario, reference: &dyn Backend, candidate: &dyn Backend) -> Result<Verdict> {
    let fa = Fixture::temp(profile)?;
    let fb = Fixture::temp(profile)?;
    let ca = (sc.prepare)(&fa)?;
    let cb = (sc.prepare)(&fb)?;
    let ra = (sc.run)(reference, &ca);
    let rb = (sc.run)(candidate, &cb);

    let mut problems = Vec::new();
    match &ra {
        Ok(_) if sc.must_fail => problems.push("scenario is broken: the reference was expected to refuse".to_string()),
        Err(e) if !sc.must_fail => problems.push(format!("scenario is broken: the reference failed: {e:#}")),
        _ => {}
    }
    match (&ra, &rb) {
        (_, Err(e)) if e.downcast_ref::<Unsupported>().is_some() => return Ok(Verdict::Fallback(e.to_string())),
        (Ok(a), Ok(b)) => {
            if a != b {
                // Paths inside the two fixtures differ only by their root.
                let a2 = a.replace(fa.root.to_str().unwrap_or(""), "<root>");
                let b2 = b.replace(fb.root.to_str().unwrap_or(""), "<root>");
                if a2 != b2 {
                    problems.push(first_text_difference(&a2, &b2));
                }
            }
        }
        (Err(_), Err(_)) => {}
        (Ok(_), Err(e)) => problems.push(format!("reference succeeded, candidate failed: {e:#}")),
        (Err(e), Ok(_)) => problems.push(format!("reference failed, candidate succeeded: {e:#}")),
    }

    let sa = snapshot(&fa.repo, &compared_worktrees(profile, &fa))?;
    let sb = snapshot(&fb.repo, &compared_worktrees(profile, &fb))?;
    problems.extend(differences(&sa, &sb));
    if let Err(e) = fsck(&fb.repo) {
        problems.push(format!("git fsck on the candidate repository: {e:#}"));
    }
    Ok(if problems.is_empty() { Verdict::Match } else { Verdict::Mismatch(problems) })
}

fn first_text_difference(a: &str, b: &str) -> String {
    for (i, (la, lb)) in a.lines().zip(b.lines()).enumerate() {
        if la != lb {
            return format!("result differs at line {}:\n    reference: {la}\n    candidate: {lb}", i + 1);
        }
    }
    format!("result differs in length: reference {} lines, candidate {} lines", a.lines().count(), b.lines().count())
}
