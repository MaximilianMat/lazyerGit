//! Every git command line the engine answers itself must produce exactly the
//! bytes and exit code real git produces, in a range of repository states.

use lazyer_core::Native;
use lazyer_testkit::git::{git, run};
use lazyer_testkit::{scenario, Fixture, Profile};
use std::path::Path;

const BRANCH_FORMAT: &str = "--format=%(HEAD)%00%(refname:short)%00%(upstream:short)%00%(upstream:track)%00%(push:track)%00%(push)%00%(subject)%00%(objectname)%00%(committerdate:unix)";

/// Command lines as lazygit issues them. `{branch}` and `{oid}` are filled in
/// per repository.
fn commands() -> Vec<Vec<&'static str>> {
    vec![
        vec!["status", "--untracked-files=all", "--porcelain", "-z", "--find-renames=50%"],
        vec!["rev-parse", "--path-format=absolute", "--absolute-git-dir"],
        vec!["rev-parse", "--path-format=absolute", "--show-toplevel", "--absolute-git-dir", "--git-common-dir", "--show-superproject-working-tree"],
        vec!["branch", "--show-current"],
        vec!["worktree", "list", "--porcelain"],
        vec!["stash", "list", "-z", "--pretty=%H|%ct|%gs"],
        vec!["for-each-ref", "--sort=-committerdate", BRANCH_FORMAT, "refs/heads"],
        vec!["for-each-ref", "--sort=-committerdate", "--format=%(refname)%00%(objectname)%00%(committerdate:unix)", "refs/remotes"],
        vec!["for-each-ref", "--format=%(objectname) %(refname)"],
        vec![
            "log",
            "HEAD",
            "--topo-order",
            "--oneline",
            "--pretty=format:+%H%x00%at%x00%aN%x00%ae%x00%P%x00%m%x00%D%x00%s",
            "--abbrev=40",
            "-300",
            "--no-show-signature",
            "--",
        ],
        vec![
            "-c",
            "log.showSignature=false",
            "log",
            "{branch}",
            "--topo-order",
            "--oneline",
            "--pretty=format:+%H%x00%at%x00%aN%x00%ae%x00%P%x00%m%x00%D%x00%s",
            "--abbrev=40",
            "-300",
            "--no-show-signature",
            "--",
        ],
        vec!["rev-parse", "--verify", "--quiet", "refs/heads/{branch}"],
        vec!["rev-parse", "-q", "--verify", "{branch}"],
        vec!["rev-parse", "--verify", "--quiet", "{oid}"],
        vec!["rev-parse", "--verify", "--quiet", "refs/heads/no-such-branch"],
        vec!["rev-parse", "--symbolic-full-name", "main@{u}"],
        vec!["rev-parse", "--symbolic-full-name", "master@{u}"],
        vec!["rev-parse", "--symbolic-full-name", "scenario/work@{u}"],
        vec!["rev-list", "refs/heads/scenario/work", "^scenario/work@{u}", "^refs/remotes/origin/main"],
        vec!["rev-list", "refs/heads/{branch}", "^{branch}@{u}", "^refs/remotes/origin/main"],
        vec!["config", "--local", "--get-regexp", r"^branch\."],
        vec!["config", "--local", "--get-regexp", r"^remote\.[^.]+\.(url|pushurl)$"],
        vec!["tag", "--list", "-n", "--sort=-creatordate"],
        vec!["-c", "log.showSignature=false", "log", "-g", "--format=+%H%x00%ct%x00%gs%x00%P"],
        vec!["rev-list", "HEAD", "^refs/remotes/origin/main"],
        vec!["rev-list", "refs/heads/main", "^main@{u}", "^refs/remotes/origin/main"],
        vec!["rev-list", "refs/heads/{branch}", "^refs/remotes/origin/main"],
        vec![
            "rev-list",
            "--max-count=1",
            "--ignore-missing",
            "refs/heads/{branch}",
            "^refs/remotes/origin/main",
            "^{oid}",
            "^0123456789012345678901234567890123456789",
            "--",
        ],
        vec!["rev-list", "--max-count=1", "--ignore-missing", "refs/heads/main", "^refs/heads/main", "--"],
        vec!["reflog", "show", "--format=%H", "refs/heads/main"],
        vec!["reflog", "show", "--format=%H", "refs/remotes/origin/main"],
        vec!["rev-parse", "-q", "--verify", "refs/heads/main@{0}"],
        vec!["rev-parse", "-q", "--verify", "refs/heads/main@{1}"],
        vec!["rev-parse", "-q", "--verify", "refs/heads/main@{2}"],
        vec!["rev-parse", "-q", "--verify", "refs/heads/main@{3}"],
        vec!["rev-parse", "-q", "--verify", "refs/heads/main@{50}"],
        vec!["rev-parse", "-q", "--verify", "refs/remotes/origin/main@{1}"],
        // "?" first: the engine may decline (a commit that adds and deletes
        // files could contain renames), but an answer has to be git's.
        vec![
            "?",
            "-c",
            "diff.dstPrefix=b/",
            "-c",
            "diff.srcPrefix=a/",
            "-c",
            "diff.noprefix=false",
            "diff",
            "--submodule",
            "--no-ext-diff",
            "--name-status",
            "-z",
            "--find-renames=50%",
            "{oid}^",
            "{oid}",
        ],
        vec!["?", "diff", "--submodule", "--no-ext-diff", "--name-status", "-z", "--find-renames=50%", "HEAD^", "HEAD"],
        vec!["?", "diff", "--submodule", "--no-ext-diff", "--name-status", "-z", "--find-renames=50%", "{oid}", "HEAD"],
        vec!["config", "--get", "--null", "user.name"],
        vec!["config", "--get", "--null", "no.such-key"],
    ]
}

fn compare(native: &Native, wt: &Path, state: &str, failures: &mut Vec<String>) {
    let branch = run(wt, &["for-each-ref", "--count=1", "--format=%(refname:short)", "refs/heads/feature"]).unwrap();
    let oid = run(wt, &["rev-parse", "HEAD~2"]).unwrap();
    for template in commands() {
        let optional = template[0] == "?";
        let argv: Vec<String> = template.iter().skip(optional as usize).map(|a| a.replace("{branch}", &branch).replace("{oid}", &oid)).collect();
        let real = git(wt).args(&argv).stdin(std::process::Stdio::null()).output().unwrap();
        let shown = argv.iter().map(|a| a.chars().take(40).collect::<String>()).collect::<Vec<_>>().join(" ");
        match native.emulate(wt, &argv) {
            // Declining a command git itself rejects is fine: git then
            // reports the problem in its own words.
            None if !real.status.success() || optional => {}
            None => failures.push(format!("[{state}] not handled: git {shown}")),
            Some(e) => {
                if e.code != real.status.code().unwrap_or(-1) {
                    failures.push(format!("[{state}] exit code {} instead of {:?}: git {shown}", e.code, real.status.code()));
                } else if e.stdout != real.stdout {
                    let (a, b) = (String::from_utf8_lossy(&real.stdout).replace('\0', "␀"), String::from_utf8_lossy(&e.stdout).replace('\0', "␀"));
                    let at = a.bytes().zip(b.bytes()).position(|(x, y)| x != y).unwrap_or(a.len().min(b.len()));
                    let from = (0..=at.saturating_sub(60)).rev().find(|i| a.is_char_boundary(*i) && b.is_char_boundary(*i)).unwrap_or(0);
                    let cut = |s: &str| s[from..].chars().take(200).collect::<String>();
                    failures.push(format!("[{state}] output differs at byte {at}: git {shown}\n    git:    {}\n    engine: {}", cut(&a), cut(&b)));
                }
            }
        }
    }
}

#[test]
fn emulated_commands_print_what_git_prints() {
    let profile = Profile::small();
    let mut failures = Vec::new();
    // The baseline plus the start states of scenarios that change what the
    // commands show: dirty files, renames, merges, a new branch with commits.
    for state in ["baseline", "status_dirty", "status_rename", "view_merges", "amend_older_d3", "view_other_worktree"] {
        let fx = Fixture::temp(&profile).unwrap();
        let wt = match scenario::all().into_iter().find(|s| s.name == state) {
            Some(sc) => (sc.prepare)(&fx).unwrap().wt,
            None => fx.repo.clone(),
        };
        compare(&Native::isolated(), &wt, state, &mut failures);
    }
    // Many names on one commit: the order of decorations has to be git's.
    let fx = Fixture::temp(&profile).unwrap();
    for args in [
        vec!["branch", "zzz", "HEAD"],
        vec!["branch", "aaa", "HEAD"],
        vec!["branch", "mmm/nested", "HEAD~1"],
        vec!["tag", "t-light", "HEAD"],
        vec!["tag", "-a", "-m", "annotated", "t-annotated", "HEAD"],
        vec!["tag", "-a", "-m", "older", "a-annotated", "HEAD~1"],
        vec!["update-ref", "refs/remotes/origin/zzz", "HEAD"],
        vec!["update-ref", "refs/remotes/upstream/main", "HEAD"],
        vec!["checkout", "-q", "--detach", "HEAD"],
    ] {
        run(&fx.repo, &args).unwrap();
    }
    compare(&Native::isolated(), &fx.repo, "decorated", &mut failures);

    // A branch that diverged from its upstream, with reflogs on both sides:
    // what lazygit inspects to tell a rewritten upstream from real divergence.
    let fx = Fixture::temp(&profile).unwrap();
    for args in [
        vec!["update-ref", "-m", "fetch: first", "refs/remotes/origin/main", "HEAD~6"],
        vec!["update-ref", "-m", "fetch: forced", "refs/remotes/origin/main", "HEAD~4"],
        vec!["reset", "-q", "--hard", "HEAD~5"],
        vec!["commit", "-q", "--allow-empty", "-m", "local one"],
        vec!["commit", "-q", "--allow-empty", "-m", "local two"],
        vec!["reset", "-q", "--hard", "HEAD~1"],
    ] {
        run(&fx.repo, &args).unwrap();
    }
    compare(&Native::isolated(), &fx.repo, "diverged", &mut failures);
    assert!(failures.is_empty(), "\n{}", failures.join("\n"));
}
