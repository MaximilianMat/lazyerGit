//! Answering git command lines without running git.
//!
//! A front end such as lazygit asks for data by running `git status ...`,
//! `git for-each-ref ...`, `git log ...` and parsing what they print. Given a
//! working directory and an argument list, [`Native::emulate`] produces the
//! exact bytes git would write to stdout and its exit code, or declines. A
//! declined command is simply run with real git, so only command lines that
//! are understood completely are answered here; anything unfamiliar in the
//! arguments means "not mine".

use super::commitobj::RawCommit;
use super::ops::Repo;
use super::refs::{self, Head};
use super::stage;
use super::tree::{hex, unhex, Kind, Oid};
use super::view::{self, Commits};
use super::{Native, Shared};
use crate::api::{unsupported, Backend, Ident, Result, Unsupported};
use std::collections::{BTreeMap, HashMap};
use std::path::Path;

pub struct Emulated {
    pub stdout: Vec<u8>,
    pub code: i32,
}

fn ok(stdout: impl Into<Vec<u8>>) -> Result<Emulated> {
    Ok(Emulated { stdout: stdout.into(), code: 0 })
}

impl Native {
    /// The output of `git <argv>` run in `cwd`, or `None` when the command is
    /// not handled and has to be run for real.
    pub fn emulate(&self, cwd: &Path, argv: &[String]) -> Option<Emulated> {
        let mut dir = cwd.to_path_buf();
        let mut i = 0;
        loop {
            match argv.get(i).map(String::as_str) {
                Some("-C") => {
                    dir = dir.join(argv.get(i + 1)?);
                    i += 2;
                }
                // Settings that do not change the output of anything answered here.
                Some("-c") => {
                    let kv = argv.get(i + 1)?.to_ascii_lowercase();
                    if !["log.showsignature=false", "color.ui=false", "core.quotepath=false"].contains(&kv.as_str()) {
                        return None;
                    }
                    i += 2;
                }
                Some("--no-optional-locks") | Some("--no-pager") => i += 1,
                _ => break,
            }
        }
        let args: Vec<&str> = argv[i..].iter().map(String::as_str).collect();
        match self.with_shared(&dir, |r, shared| self.dispatch(r, shared, &args)) {
            Ok(out) => Some(out),
            Err(e) => {
                if std::env::var_os("LAZYER_TRACE").is_some() && e.downcast_ref::<Unsupported>().is_none() {
                    eprintln!("  trace emulate {args:?} failed: {e:#}");
                }
                None
            }
        }
    }

    fn dispatch(&self, r: &Repo, shared: &Shared, args: &[&str]) -> Result<Emulated> {
        let Some((cmd, rest)) = args.split_first() else { return unsupported("no command") };
        match *cmd {
            "status" => self.emu_status(r, shared, rest),
            "rev-parse" => self.emu_rev_parse(r, shared, rest),
            "branch" if rest == ["--show-current"] => match refs::read_head(&r.l)? {
                Head::Branch(name, _) => ok(format!("{}\n", name.strip_prefix("refs/heads/").unwrap_or(&name))),
                Head::Detached(_) => ok(""),
            },
            "worktree" if rest == ["list", "--porcelain"] => emu_worktree_list(r),
            "stash" => emu_stash_list(r, rest),
            "for-each-ref" => self.emu_for_each_ref(r, shared, rest),
            "log" => self.emu_log(r, shared, rest),
            "config" => emu_config(r, shared, rest),
            "reflog" => emu_reflog_show(r, rest),
            "rev-list" => self.emu_rev_list(r, shared, rest),
            "tag" if rest.first() == Some(&"--") && (rest.len() == 2 || rest.len() == 3) => {
                // `git tag -- <name> [<ref>]`: a lightweight tag.
                let Some(target) = resolve_name(r, rest.get(2).copied().unwrap_or("HEAD"))? else { return unsupported("unknown revision") };
                r.create_tag(rest[1], &target)?;
                ok("")
            }
            "tag" => emu_tag_list(r, shared, rest),
            "checkout" => self.emu_checkout(r, shared, rest),
            "add" => {
                let (tracked_only, specs) = match rest {
                    ["-A"] => (false, vec![".".to_string()]),
                    ["-u"] => (true, vec![".".to_string()]),
                    ["--", paths @ ..] => (false, stage::literal_specs(paths)?),
                    ["-u", "--", paths @ ..] => (true, stage::literal_specs(paths)?),
                    _ => return unsupported("add form"),
                };
                let (status, _) = self.status_in(r, shared)?;
                r.stage(&status.entries, &specs, tracked_only)?;
                ok("")
            }
            "reset" => match rest {
                [] => r.unstage(None).and_then(|_| ok("")),
                ["HEAD", "--", paths @ ..] | ["--", paths @ ..] => r.unstage(Some(&stage::literal_specs(paths)?)).and_then(|_| ok("")),
                [strength @ ("--soft" | "--mixed" | "--hard"), name] => {
                    let mode = match *strength {
                        "--soft" => stage::ResetMode::Soft,
                        "--mixed" => stage::ResetMode::Mixed,
                        _ => stage::ResetMode::Hard,
                    };
                    let Some(target) = resolve_commit(r, name)? else { return unsupported("unknown revision") };
                    r.reset(mode, &target, name)?;
                    ok("")
                }
                _ => unsupported("reset form"),
            },
            "rm" => match rest {
                ["--cached", "--force", "--", paths @ ..] if !paths.is_empty() => {
                    r.remove_cached(&stage::literal_specs(paths)?)?;
                    ok("")
                }
                _ => unsupported("rm form"),
            },
            "branch" => match rest {
                // `git branch <name> <base>`: a new branch, not checked out.
                [name, base] if !name.starts_with('-') && !base.starts_with('-') => {
                    let target = branch_base(r, shared, base, false)?;
                    r.create_branch(name, &target, base, false)?;
                    ok("")
                }
                _ => unsupported("branch form"),
            },
            "symbolic-ref" => match (rest, refs::read_head(&r.l)?) {
                (["HEAD"] | ["-q", "HEAD"], Head::Branch(name, _)) => ok(format!("{name}\n")),
                (["--short", "HEAD"], Head::Branch(name, _)) => ok(format!("{}\n", name.strip_prefix("refs/heads/").unwrap_or(&name))),
                (["-q", "HEAD"], Head::Detached(_)) => Ok(Emulated { stdout: Vec::new(), code: 1 }),
                _ => unsupported("symbolic-ref form"),
            },
            "commit" => self.emu_commit(r, shared, rest),
            // Operations that have no single git command line.
            "lazyer-amend-older" if rest.len() == 1 => {
                let who = current_ident(shared)?;
                self.amend_older(&r.l.worktree, rest[0], &who)?;
                ok("")
            }
            "lazyer-move-patch" if rest.len() >= 2 => {
                let who = current_ident(shared)?;
                let paths: Vec<String> = rest[1..].iter().map(|s| s.to_string()).collect();
                self.move_patch_to_index(&r.l.worktree, rest[0], &paths, &who)?;
                ok("")
            }
            // `git-worktree-swap <branch> [--detach-other]` in one step.
            // Prints what the script prints.
            "lazyer-swap-branch" if rest.len() == 1 || (rest.len() == 2 && rest[1] == "--detach-other") => {
                let (branch, detach) = (rest[0], rest.len() == 2);
                let refname = format!("refs/heads/{branch}");
                let here = r.l.worktree.display().to_string();
                let current = match refs::read_head(&r.l)? {
                    Head::Branch(name, _) => name.strip_prefix("refs/heads/").map(str::to_string),
                    Head::Detached(_) => None,
                };
                let mut other = None;
                for (root, git_dir) in refs::worktrees(&r.l)? {
                    let l = refs::Layout { worktree: root.clone(), git_dir, common_dir: r.l.common_dir.clone() };
                    if matches!(refs::read_head(&l), Ok(Head::Branch(name, _)) if name == refname) {
                        other = Some((root, l.git_dir));
                    }
                }
                // Not checked out anywhere else: the script explains and fails.
                let Some((other_root, other_git_dir)) = other else { return unsupported("branch is not checked out in another worktree") };
                if other_git_dir == r.l.git_dir {
                    return ok(format!("Already on {branch}\n"));
                }
                self.swap_branch(&r.l.worktree, branch, detach)?;
                match (current, detach) {
                    (Some(current), false) => ok(format!("Swapped: {here} -> {branch}, {} -> {current}\n", other_root.display())),
                    _ => ok(format!("{here} -> {branch}, {} left detached\n", other_root.display())),
                }
            }
            _ => unsupported("command"),
        }
    }

    fn emu_status(&self, r: &Repo, shared: &Shared, rest: &[&str]) -> Result<Emulated> {
        let mut want = ["--untracked-files=all", "--porcelain", "-z", "--find-renames=50%"].to_vec();
        for a in rest {
            match want.iter().position(|w| w == a) {
                Some(i) => {
                    want.remove(i);
                }
                None => return unsupported("status option"),
            }
        }
        if !want.is_empty() {
            return unsupported("status without the expected options");
        }
        let (status, _) = self.status_in(r, shared)?;
        // Tracked changes in path order, then untracked files.
        let mut out = Vec::new();
        for untracked in [false, true] {
            for e in status.entries.iter().filter(|e| (e.index == '?') == untracked) {
                out.extend_from_slice(format!("{}{} {}\0", e.index, e.worktree, e.path).as_bytes());
                if let Some(orig) = &e.orig_path {
                    out.extend_from_slice(orig.as_bytes());
                    out.push(0);
                }
            }
        }
        ok(out)
    }

    fn emu_rev_parse(&self, r: &Repo, shared: &Shared, rest: &[&str]) -> Result<Emulated> {
        let real = |p: &Path| -> Result<String> { Ok(p.canonicalize()?.to_string_lossy().to_string()) };
        match rest {
            ["HEAD"] | ["--verify", "HEAD"] => match refs::read_head(&r.l)?.oid() {
                Some(oid) => ok(format!("{}\n", hex(&oid))),
                None => unsupported("unborn branch"),
            },
            ["--path-format=absolute", "--absolute-git-dir"] => ok(format!("{}\n", real(&r.l.git_dir)?)),
            ["--path-format=absolute", "--show-toplevel", "--absolute-git-dir", "--git-common-dir", "--show-superproject-working-tree"] => {
                if r.l.common_dir.components().any(|c| c.as_os_str() == "modules") {
                    return unsupported("submodule");
                }
                ok(format!("{}\n{}\n{}\n", real(&r.l.worktree)?, real(&r.l.git_dir)?, real(&r.l.common_dir)?))
            }
            ["--verify", "--quiet", name] | ["--quiet", "--verify", name] | ["-q", "--verify", name] | ["--verify", "-q", name]
                if name.ends_with('}') && name.contains("@{") =>
            {
                // `ref@{N}`: the value the ref had N updates ago.
                let (refname, n) = name[..name.len() - 1].split_once("@{").unwrap();
                let Ok(n) = n.parse::<usize>() else { return unsupported("reflog selector") };
                if !refname.starts_with("refs/") || refs::resolve(&r.l, refname)?.is_none() {
                    return unsupported("reflog of an unknown ref");
                }
                let entries = reflog_entries(r, refname);
                let quiet_failure = Ok(Emulated { stdout: Vec::new(), code: 128 });
                if entries.is_empty() {
                    return if n == 0 { unsupported("empty reflog") } else { quiet_failure };
                }
                // Newest first. One step past the oldest entry is the value
                // the ref had before that entry.
                let oid = if n < entries.len() {
                    entries[entries.len() - 1 - n].1.clone()
                } else if n == entries.len() && entries[0].0.bytes().any(|b| b != b'0') {
                    entries[0].0.clone()
                } else {
                    return quiet_failure;
                };
                ok(format!("{oid}\n"))
            }
            ["--verify", "--quiet", name] | ["--quiet", "--verify", name] | ["-q", "--verify", name] | ["--verify", "-q", name] => match resolve_name(r, name)?
            {
                Some(oid) => ok(format!("{}\n", hex(&oid))),
                None => Ok(Emulated { stdout: Vec::new(), code: 1 }),
            },
            ["--symbolic-full-name", spec] => {
                let Some(branch) = spec.strip_suffix("@{u}").or_else(|| spec.strip_suffix("@{upstream}")) else { return unsupported("rev-parse form") };
                let refname = format!("refs/heads/{branch}");
                // An unknown branch or one without upstream: git complains
                // on stderr and fails.
                let failure = Ok(Emulated { stdout: Vec::new(), code: 128 });
                if branch.is_empty() || branch.contains(['^', '~', ':', ' ', '@']) {
                    return unsupported("rev-parse form");
                }
                if refs::resolve(&r.l, &refname)?.is_none() {
                    return failure;
                }
                let repo = shared.repo.to_thread_local();
                match shared.memo.upstream(&refname, || Self::upstream_of(&repo, &refname)) {
                    Some(up) if refs::resolve(&r.l, &up)?.is_some() => ok(format!("{up}\n")),
                    // Configured but not fetched: leave the explanation to git.
                    Some(_) => unsupported("upstream is gone"),
                    None => failure,
                }
            }
            _ => unsupported("rev-parse form"),
        }
    }

    fn emu_for_each_ref(&self, r: &Repo, shared: &Shared, rest: &[&str]) -> Result<Emulated> {
        let mut format = None;
        let mut sort_by_date = false;
        let mut patterns = Vec::new();
        for a in rest {
            if let Some(f) = a.strip_prefix("--format=") {
                format = Some(parse_ref_format(f)?);
            } else if *a == "--sort=-committerdate" {
                sort_by_date = true;
            } else if a.starts_with('-') || a.contains(['*', '?', '[']) {
                return unsupported("for-each-ref option");
            } else {
                patterns.push(a.trim_end_matches('/').to_string());
            }
        }
        let Some(format) = format else { return unsupported("for-each-ref without format") };
        if patterns.is_empty() {
            patterns.push("refs".into());
        }
        let mut all: BTreeMap<String, Oid> = BTreeMap::new();
        for p in &patterns {
            all.extend(view::list_refs(&r.l, &format!("{p}/"))?);
            if let Some(oid) = refs::resolve(&r.l, p)?.filter(|_| p.starts_with("refs/") && p.matches('/').count() >= 2) {
                all.insert(p.clone(), oid);
            }
        }

        let commits = Commits::new(r.odb, &r.l.common_dir).with_memo(&shared.memo);
        let repo = shared.repo.to_thread_local();
        let head = refs::read_head(&r.l)?;
        let needs_commit = sort_by_date || format.iter().any(|a| matches!(a, RefAtom::Subject | RefAtom::CommitterUnix));
        let needs_short = format.iter().any(|a| matches!(a, RefAtom::RefShort));
        if needs_short {
            let mut seen: HashMap<&str, usize> = HashMap::new();
            for name in all.keys() {
                *seen.entry(short_ref(name)).or_default() += 1;
            }
            if seen.values().any(|n| *n > 1) {
                return unsupported("ambiguous short ref names");
            }
        }
        let needs_push = format.iter().any(|a| matches!(a, RefAtom::Push | RefAtom::PushTrack));
        if needs_push {
            let cfg = repo.config_snapshot();
            let simple = cfg.string("push.default").is_none_or(|v| v == "simple");
            if !simple || cfg.string("remote.pushDefault").is_some() {
                return unsupported("push configuration");
            }
        }

        struct Row {
            name: String,
            oid: Oid,
            commit: Option<std::sync::Arc<RawCommit>>,
            time: i64,
        }
        let needs_subject = format.iter().any(|a| matches!(a, RefAtom::Subject));
        let mut rows = Vec::with_capacity(all.len());
        for (name, oid) in all {
            // The commit time alone comes from the commit-graph.
            let (commit, time) = match (needs_subject, needs_commit) {
                (true, _) => {
                    let c = commits.raw(&oid)?;
                    let t = view::ident_time(&c.committer);
                    (Some(c), t)
                }
                (false, true) => (None, commits.node(&oid)?.time),
                (false, false) => (None, 0),
            };
            rows.push(Row { name, oid, commit, time });
        }
        if sort_by_date {
            rows.sort_by(|a, b| b.time.cmp(&a.time).then_with(|| a.name.cmp(&b.name)));
        }

        let track = |local: &Oid, other: Option<&String>| -> Result<String> {
            let Some(other) = other else { return Ok(String::new()) };
            let Some(theirs) = refs::resolve(&r.l, other)? else { return Ok("[gone]".into()) };
            Ok(match commits.ahead_behind(local, &theirs)? {
                (0, 0) => String::new(),
                (a, 0) => format!("[ahead {a}]"),
                (0, b) => format!("[behind {b}]"),
                (a, b) => format!("[ahead {a}, behind {b}]"),
            })
        };
        let mut out = Vec::new();
        for row in &rows {
            let is_branch = row.name.starts_with("refs/heads/");
            let upstream = if is_branch { shared.memo.upstream(&row.name, || Self::upstream_of(&repo, &row.name)) } else { None };
            // With `push.default=simple` a branch is pushed to its upstream,
            // and only when that has the same name on the branch's remote.
            let push = match (&upstream, needs_push && is_branch) {
                (Some(up), true) => {
                    let short = &row.name["refs/heads/".len()..];
                    let cfg = repo.config_snapshot();
                    if cfg.string(format!("branch.{short}.pushRemote").as_str()).is_some() {
                        return unsupported("push configuration");
                    }
                    let remote = cfg.string(format!("branch.{short}.remote").as_str()).map(|v| v.to_string()).unwrap_or_default();
                    if remote == "." || cfg.string(format!("remote.{remote}.push").as_str()).is_some() {
                        return unsupported("push configuration");
                    }
                    (*up == format!("refs/remotes/{remote}/{short}")).then(|| up.clone())
                }
                _ => None,
            };
            for atom in &format {
                match atom {
                    RefAtom::Lit(b) => out.extend_from_slice(b),
                    RefAtom::ObjectName => out.extend_from_slice(hex(&row.oid).as_bytes()),
                    RefAtom::RefName => out.extend_from_slice(row.name.as_bytes()),
                    RefAtom::RefShort => out.extend_from_slice(short_ref(&row.name).as_bytes()),
                    RefAtom::Head => out.push(if matches!(&head, Head::Branch(h, _) if *h == row.name) { b'*' } else { b' ' }),
                    RefAtom::UpstreamShort => out.extend_from_slice(upstream.as_deref().map(view::short_upstream).unwrap_or_default().as_bytes()),
                    RefAtom::UpstreamTrack => out.extend_from_slice(track(&row.oid, upstream.as_ref())?.as_bytes()),
                    RefAtom::Push => out.extend_from_slice(push.as_deref().unwrap_or_default().as_bytes()),
                    RefAtom::PushTrack => out.extend_from_slice(track(&row.oid, push.as_ref())?.as_bytes()),
                    RefAtom::Subject => out.extend_from_slice(view::subject(&row.commit.as_ref().unwrap().message).as_bytes()),
                    RefAtom::CommitterUnix => out.extend_from_slice(row.time.to_string().as_bytes()),
                }
            }
            out.push(b'\n');
        }
        ok(out)
    }

    fn emu_log(&self, r: &Repo, shared: &Shared, rest: &[&str]) -> Result<Emulated> {
        if rest.first() == Some(&"-g") {
            return emu_reflog_log(r, shared, &rest[1..]);
        }
        let (mut format, mut limit, mut topo, mut rev) = (None, None, false, None);
        for a in rest {
            if let Some(f) = a.strip_prefix("--pretty=format:").or_else(|| a.strip_prefix("--format=format:")) {
                format = Some(f);
            } else if *a == "--topo-order" {
                topo = true;
            } else if ["--oneline", "--no-show-signature", "--abbrev=40", "--"].contains(a) {
            } else if let Some(n) = a.strip_prefix('-').and_then(|n| n.parse::<usize>().ok()) {
                limit = Some(n);
            } else if !a.starts_with('-') && rev.is_none() {
                rev = Some(*a);
            } else {
                return unsupported("log option");
            }
        }
        let (Some(format), Some(limit), true) = (format, limit, topo) else { return unsupported("log form") };
        if r.l.worktree.join(".mailmap").exists() || r.l.common_dir.join("shallow").exists() || r.l.common_dir.join("refs/replace").exists() {
            return unsupported("mailmap, shallow or replace refs");
        }
        let Some(start) = resolve_name(r, rev.unwrap_or("HEAD"))? else { return unsupported("unknown revision") };
        let commits = Commits::new(r.odb, &r.l.common_dir).with_memo(&shared.memo);
        let decorations = if format.contains("%D") { Some(decorations(r, &shared.memo)?) } else { None };
        let mut out = Vec::new();
        for (n, oid) in commits.topo_log(&start, limit)?.iter().enumerate() {
            if n > 0 {
                out.push(b'\n');
            }
            let c = commits.raw(oid)?;
            let author = String::from_utf8_lossy(&c.author).to_string();
            let (name, tail) = author.split_once(" <").unwrap_or((author.as_str(), ""));
            let (email, _) = tail.split_once('>').unwrap_or((tail, ""));
            let mut chars = format.chars().peekable();
            while let Some(ch) = chars.next() {
                if ch != '%' {
                    let mut buf = [0u8; 4];
                    out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
                    continue;
                }
                match chars.next() {
                    Some('H') => out.extend_from_slice(hex(oid).as_bytes()),
                    Some('P') => out.extend_from_slice(c.parents.join(" ").as_bytes()),
                    Some('m') => out.push(b'>'),
                    Some('s') => out.extend_from_slice(view::subject(&c.message).as_bytes()),
                    Some('D') => out.extend_from_slice(decorations.as_ref().and_then(|d| d.get(oid)).map(|v| v.join(", ")).unwrap_or_default().as_bytes()),
                    Some('%') => out.push(b'%'),
                    Some('x') => {
                        let hi = chars.next().and_then(|c| c.to_digit(16));
                        let lo = chars.next().and_then(|c| c.to_digit(16));
                        match (hi, lo) {
                            (Some(h), Some(l)) => out.push((h * 16 + l) as u8),
                            _ => return unsupported("log format"),
                        }
                    }
                    Some('a') => match chars.next() {
                        Some('t') => out.extend_from_slice(view::ident_time(&c.author).to_string().as_bytes()),
                        Some('N') => out.extend_from_slice(name.as_bytes()),
                        Some('e') => out.extend_from_slice(email.as_bytes()),
                        _ => return unsupported("log format"),
                    },
                    Some('c') if chars.peek() == Some(&'t') => {
                        chars.next();
                        out.extend_from_slice(view::ident_time(&c.committer).to_string().as_bytes());
                    }
                    _ => return unsupported("log format"),
                }
            }
        }
        ok(out)
    }

    /// `git rev-list [--max-count=N] [--ignore-missing] <tip> ^<excluded>... [--]`:
    /// the commits reachable from the tips but from none of the excluded
    /// ones, newest first.
    fn emu_rev_list(&self, r: &Repo, shared: &Shared, rest: &[&str]) -> Result<Emulated> {
        let (mut max, mut ignore_missing) = (usize::MAX, false);
        let (mut tips, mut excluded) = (Vec::new(), Vec::new());
        let repo = shared.repo.to_thread_local();
        for a in rest {
            if let Some(n) = a.strip_prefix("--max-count=") {
                max = n.parse().map_err(|_| Unsupported("rev-list count".into()))?;
            } else if *a == "--ignore-missing" {
                ignore_missing = true;
            } else if *a == "--" {
            } else if a.starts_with('-') {
                return unsupported("rev-list option");
            } else {
                let (negative, name) = match a.strip_prefix('^') {
                    Some(n) => (true, n),
                    None => (false, *a),
                };
                let oid = match name.strip_suffix("@{u}") {
                    Some(branch) => {
                        let refname = if branch.starts_with("refs/heads/") { branch.to_string() } else { format!("refs/heads/{branch}") };
                        if refs::resolve(&r.l, &refname)?.is_none() {
                            return unsupported("unknown branch");
                        }
                        match shared.memo.upstream(&refname, || Self::upstream_of(&repo, &refname)) {
                            Some(up) => match refs::resolve(&r.l, &up)? {
                                Some(oid) => Some(oid),
                                None => return unsupported("upstream is gone"),
                            },
                            // A branch without upstream: git fails, printing nothing.
                            None => return Ok(Emulated { stdout: Vec::new(), code: 128 }),
                        }
                    }
                    None => resolve_name(r, name)?,
                };
                match oid {
                    Some(oid) if negative => excluded.push(oid),
                    Some(oid) => tips.push(oid),
                    // Without --ignore-missing git stops with an error.
                    None if ignore_missing => {}
                    None => return unsupported("unknown revision"),
                }
            }
        }
        if tips.len() != 1 {
            return unsupported("rev-list with several or no tips");
        }
        let commits = Commits::new(r.odb, &r.l.common_dir).with_memo(&shared.memo);
        // Colour history from the top: descendants first, so every commit's
        // colour is final when it is taken out of the queue.
        let key = |o: &Oid| -> Result<(u32, i64)> {
            let n = commits.node(o)?;
            Ok((n.generation, n.time))
        };
        let mut excluded_set: HashMap<Oid, bool> = HashMap::new();
        let mut queue = std::collections::BinaryHeap::new();
        for e in &excluded {
            if excluded_set.insert(*e, true).is_none() {
                queue.push((key(e)?, *e));
            }
        }
        if !excluded_set.contains_key(&tips[0]) {
            excluded_set.insert(tips[0], false);
            queue.push((key(&tips[0])?, tips[0]));
        }
        let mut wanted: std::collections::HashSet<Oid> = Default::default();
        while queue.iter().any(|(_, o)| !excluded_set[o]) {
            let (_, oid) = queue.pop().unwrap();
            let is_excluded = excluded_set[&oid];
            if !is_excluded {
                wanted.insert(oid);
            }
            for p in &commits.node(&oid)?.parents {
                match excluded_set.get(p).copied() {
                    None => {
                        excluded_set.insert(*p, is_excluded);
                        queue.push((key(p)?, *p));
                    }
                    Some(false) if is_excluded => {
                        excluded_set.insert(*p, true);
                        queue.push((key(p)?, *p));
                    }
                    _ => {}
                }
            }
        }
        wanted.retain(|o| !excluded_set[o]);
        // Output order: by commit date, newest first, starting at the tip.
        let mut out = String::new();
        let mut walk = std::collections::BinaryHeap::new();
        let mut seq = 0u64;
        let mut queued: std::collections::HashSet<Oid> = Default::default();
        if wanted.contains(&tips[0]) {
            walk.push((commits.node(&tips[0])?.time, std::cmp::Reverse(seq), tips[0]));
            queued.insert(tips[0]);
        }
        let mut printed = 0;
        while let Some((_, _, oid)) = walk.pop() {
            if printed == max {
                break;
            }
            out.push_str(&hex(&oid));
            out.push('\n');
            printed += 1;
            for p in &commits.node(&oid)?.parents {
                if wanted.contains(p) && queued.insert(*p) {
                    seq += 1;
                    walk.push((commits.node(p)?.time, std::cmp::Reverse(seq), *p));
                }
            }
        }
        ok(out)
    }

    /// `git checkout <local branch>`.
    fn emu_checkout(&self, r: &Repo, shared: &Shared, rest: &[&str]) -> Result<Emulated> {
        match rest {
            // `git checkout -- <paths>`: discard unstaged changes.
            ["--", paths @ ..] if !paths.is_empty() => {
                r.discard_unstaged(&stage::literal_specs(paths)?)?;
                return ok("");
            }
            // `git checkout -b <name> <base> [--no-track]`: new branch, checked out.
            ["-b", name, base] | ["-b", name, base, "--no-track"] => {
                let target = branch_base(r, shared, base, rest.len() == 4)?;
                r.create_branch(name, &target, base, true)?;
                return ok("");
            }
            _ => {}
        }
        let [branch] = rest else { return unsupported("checkout form") };
        if branch.starts_with('-') || refs::resolve(&r.l, &format!("refs/heads/{branch}"))?.is_none() {
            return unsupported("not a local branch");
        }
        // A file or another ref of the same name makes the argument ambiguous.
        if r.l.worktree.join(branch).exists() || refs::resolve(&r.l, &format!("refs/tags/{branch}"))?.is_some() {
            return unsupported("ambiguous checkout argument");
        }
        self.switch_branch(&r.l.worktree, branch)?;
        ok("")
    }

    /// `git commit -m <summary> [-m <description>]` and
    /// `git commit --amend --no-edit --allow-empty`.
    fn emu_commit(&self, r: &Repo, shared: &Shared, rest: &[&str]) -> Result<Emulated> {
        let who = current_ident(shared)?;
        let mut sorted = rest.to_vec();
        sorted.sort();
        if sorted == ["--allow-empty", "--amend", "--no-edit"] || sorted == ["--allow-empty", "--allow-empty-message", "--amend", "--no-edit"] {
            self.amend_head(&r.l.worktree, &who)?;
            return ok("");
        }
        let mut paragraphs = Vec::new();
        let mut it = rest.iter();
        while let Some(a) = it.next() {
            match *a {
                "-m" => paragraphs.push(*it.next().ok_or_else(|| Unsupported("commit form".into()))?),
                _ => return unsupported("commit option"),
            }
        }
        if paragraphs.is_empty() {
            return unsupported("commit without message");
        }
        self.commit(&r.l.worktree, &paragraphs.join("\n\n"), &who, &who)?;
        ok("")
    }
}

/// The identity git would commit with, at the current time. Identities from
/// the environment (`GIT_AUTHOR_NAME`, ...) are the caller's business.
fn current_ident(shared: &Shared) -> Result<Ident> {
    let Some((name, email)) = shared.settings.user.clone() else { return unsupported("user.name or user.email not configured") };
    for var in ["GIT_AUTHOR_NAME", "GIT_AUTHOR_EMAIL", "GIT_AUTHOR_DATE", "GIT_COMMITTER_NAME", "GIT_COMMITTER_EMAIL", "GIT_COMMITTER_DATE"] {
        if std::env::var_os(var).is_some() {
            return unsupported("identity from the environment");
        }
    }
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_secs() as i64;
    let offset = local_offset_minutes(now);
    let tz = format!("{}{:02}{:02}", if offset < 0 { '-' } else { '+' }, offset.abs() / 60, offset.abs() % 60);
    Ok(Ident { name, email, time: now, tz })
}

fn local_offset_minutes(now: i64) -> i64 {
    // SAFETY: localtime_r writes only into the struct passed to it.
    unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        let t = now as libc::time_t;
        if libc::localtime_r(&t, &mut tm).is_null() {
            return 0;
        }
        tm.tm_gmtoff / 60
    }
}

/// A plain ref name or object id, the way git resolves a revision argument
/// without operators.
fn resolve_name(r: &Repo, name: &str) -> Result<Option<Oid>> {
    if name.is_empty() || name.contains(['^', '~', '@', ':', '{', '*', '?', '[', ' ', '\\']) || name.contains("..") {
        return unsupported("revision expression");
    }
    if name == "HEAD" {
        return Ok(refs::read_head(&r.l)?.oid());
    }
    for candidate in [
        name.to_string(),
        format!("refs/{name}"),
        format!("refs/tags/{name}"),
        format!("refs/heads/{name}"),
        format!("refs/remotes/{name}"),
        format!("refs/remotes/{name}/HEAD"),
    ] {
        if !candidate.starts_with("refs/") {
            // Other pseudo-refs (ORIG_HEAD, MERGE_HEAD, ...) and abbreviated ids.
            continue;
        }
        if let Some(oid) = refs::resolve(&r.l, &candidate)? {
            return Ok(Some(oid));
        }
    }
    if name.len() == 40 && name.bytes().all(|b| b.is_ascii_hexdigit()) {
        let oid = unhex(&name.to_ascii_lowercase())?;
        return Ok(r.odb.read(&oid).is_ok().then_some(oid));
    }
    if name.bytes().all(|b| b.is_ascii_hexdigit()) || name.bytes().all(|b| b.is_ascii_uppercase() || b == b'_') {
        return unsupported("abbreviated id or pseudo-ref");
    }
    Ok(None)
}

/// A revision that has to be a commit (an annotated tag is followed).
fn resolve_commit(r: &Repo, name: &str) -> Result<Option<Oid>> {
    let Some(mut oid) = resolve_name(r, name)? else { return Ok(None) };
    for _ in 0..8 {
        let (kind, data) = r.odb.read(&oid)?;
        match kind {
            Kind::Commit => return Ok(Some(oid)),
            Kind::Tag => {
                let line = data.split(|b| *b == b'\n').next().unwrap_or_default();
                oid = unhex(std::str::from_utf8(line.strip_prefix(b"object ").unwrap_or_default())?)?;
            }
            _ => return unsupported("not a commit"),
        }
    }
    unsupported("tag chain")
}

/// The commit a new branch starts at. Starting from a remote-tracking branch
/// makes git record an upstream in the config (unless told not to), and so
/// does `branch.autoSetupMerge=always|inherit`; those are left to git.
fn branch_base(r: &Repo, shared: &Shared, base: &str, no_track: bool) -> Result<Oid> {
    let repo = shared.repo.to_thread_local();
    let setup = repo.config_snapshot().string("branch.autoSetupMerge").map(|v| v.to_string().to_ascii_lowercase());
    if matches!(setup.as_deref(), Some("always" | "inherit" | "simple")) && !no_track {
        return unsupported("branch.autoSetupMerge");
    }
    let is_remote = base.starts_with("refs/remotes/") || refs::resolve(&r.l, &format!("refs/remotes/{base}"))?.is_some();
    let is_local = refs::resolve(&r.l, &format!("refs/heads/{base}"))?.is_some() || refs::resolve(&r.l, &format!("refs/tags/{base}"))?.is_some();
    if is_remote && !is_local && !no_track && setup.as_deref() != Some("false") {
        return unsupported("starting point is a remote-tracking branch");
    }
    match resolve_commit(r, base)? {
        Some(oid) => Ok(oid),
        None => unsupported("unknown revision"),
    }
}

fn short_ref(name: &str) -> &str {
    name.strip_prefix("refs/heads/").or_else(|| name.strip_prefix("refs/tags/")).or_else(|| name.strip_prefix("refs/remotes/")).unwrap_or(name)
}

enum RefAtom {
    Lit(Vec<u8>),
    ObjectName,
    RefName,
    RefShort,
    Head,
    UpstreamShort,
    UpstreamTrack,
    Push,
    PushTrack,
    Subject,
    CommitterUnix,
}

fn parse_ref_format(f: &str) -> Result<Vec<RefAtom>> {
    let b = f.as_bytes();
    let mut out = Vec::new();
    let mut lit = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] != b'%' {
            lit.push(b[i]);
            i += 1;
            continue;
        }
        if b.get(i + 1) == Some(&b'%') {
            lit.push(b'%');
            i += 2;
        } else if b.get(i + 1) == Some(&b'(') {
            let end = f[i..].find(')').map(|e| i + e).ok_or_else(|| Unsupported("format".into()))?;
            if !lit.is_empty() {
                out.push(RefAtom::Lit(std::mem::take(&mut lit)));
            }
            out.push(match &f[i + 2..end] {
                "objectname" => RefAtom::ObjectName,
                "refname" => RefAtom::RefName,
                "refname:short" => RefAtom::RefShort,
                "HEAD" => RefAtom::Head,
                "upstream:short" => RefAtom::UpstreamShort,
                "upstream:track" => RefAtom::UpstreamTrack,
                "push" => RefAtom::Push,
                "push:track" => RefAtom::PushTrack,
                "subject" => RefAtom::Subject,
                "committerdate:unix" => RefAtom::CommitterUnix,
                _ => return unsupported("format atom"),
            });
            i = end + 1;
        } else {
            // `%00`: a byte given as two hex digits.
            let byte = f.get(i + 1..i + 3).and_then(|h| u8::from_str_radix(h, 16).ok()).ok_or_else(|| Unsupported("format".into()))?;
            lit.push(byte);
            i += 3;
        }
    }
    if !lit.is_empty() {
        out.push(RefAtom::Lit(lit));
    }
    Ok(out)
}

/// What `%D` shows per commit. Git collects decorations by walking all refs
/// in name order and putting each new one in front, then HEAD in front of
/// everything; the checked-out branch is folded into `HEAD -> name`.
fn decorations(r: &Repo, memo: &view::Memo) -> Result<HashMap<Oid, Vec<String>>> {
    let mut map: HashMap<Oid, Vec<String>> = HashMap::new();
    for (name, oid) in view::list_refs(&r.l, "refs/")? {
        let label = if let Some(n) = name.strip_prefix("refs/heads/") {
            n.to_string()
        } else if let Some(n) = name.strip_prefix("refs/remotes/") {
            n.to_string()
        } else if let Some(n) = name.strip_prefix("refs/tags/") {
            format!("tag: {n}")
        } else if name == "refs/stash" {
            name.clone()
        } else {
            continue;
        };
        // An annotated tag decorates the commit it finally points to.
        let target = if name.starts_with("refs/tags/") {
            memo.peeled(&oid, || {
                let mut target = oid;
                for _ in 0..8 {
                    let (kind, data) = r.odb.read(&target)?;
                    if kind != Kind::Tag {
                        break;
                    }
                    let line = data.split(|b| *b == b'\n').next().unwrap_or_default();
                    target = unhex(std::str::from_utf8(line.strip_prefix(b"object ").unwrap_or_default())?)?;
                }
                Ok(target)
            })?
        } else {
            oid
        };
        map.entry(target).or_default().insert(0, label);
    }
    match refs::read_head(&r.l)? {
        Head::Branch(name, Some(oid)) => {
            let short = name.strip_prefix("refs/heads/").unwrap_or(&name).to_string();
            let list = map.entry(oid).or_default();
            list.retain(|l| *l != short);
            list.insert(0, format!("HEAD -> {short}"));
        }
        Head::Detached(oid) => map.entry(oid).or_default().insert(0, "HEAD".into()),
        Head::Branch(_, None) => {}
    }
    Ok(map)
}

fn emu_worktree_list(r: &Repo) -> Result<Emulated> {
    if r.l.common_dir.file_name().is_none_or(|n| n != ".git") {
        return unsupported("bare repository");
    }
    let mut out = String::new();
    for (root, git_dir) in refs::worktrees(&r.l)? {
        // Locked and missing worktrees get extra lines.
        if git_dir.join("locked").exists() || !root.is_dir() || !root.is_absolute() {
            return unsupported("locked, missing or relative worktree");
        }
        let l = refs::Layout { worktree: root.clone(), git_dir, common_dir: r.l.common_dir.clone() };
        out.push_str(&format!("worktree {}\n", root.display()));
        match refs::read_head(&l)? {
            Head::Branch(name, Some(oid)) => out.push_str(&format!("HEAD {}\nbranch {name}\n\n", hex(&oid))),
            Head::Detached(oid) => out.push_str(&format!("HEAD {}\ndetached\n\n", hex(&oid))),
            Head::Branch(_, None) => return unsupported("worktree on an unborn branch"),
        }
    }
    ok(out)
}

fn emu_stash_list(r: &Repo, rest: &[&str]) -> Result<Emulated> {
    if rest != ["list", "-z", "--pretty=%H|%ct|%gs"] {
        return unsupported("stash form");
    }
    let Ok(text) = std::fs::read_to_string(r.l.common_dir.join("logs/refs/stash")) else { return ok("") };
    let mut out = Vec::new();
    for line in text.lines().rev() {
        let (head, message) = line.split_once('\t').unwrap_or((line, ""));
        let oid = head.split(' ').nth(1).ok_or_else(|| Unsupported("stash log".into()))?;
        let (_, data) = r.odb.read(&unhex(oid)?)?;
        let time = view::ident_time(&RawCommit::parse(&data)?.committer);
        out.extend_from_slice(format!("{oid}|{time}|{message}\0").as_bytes());
    }
    ok(out)
}

/// `git config --get --null <key>` and the two `--local --get-regexp` reads
/// lazygit does (branch and remote sections).
fn emu_config(r: &Repo, shared: &Shared, rest: &[&str]) -> Result<Emulated> {
    if let ["--local", "--get-regexp", pattern] = rest {
        let wanted: fn(&str, &str) -> bool = match *pattern {
            r"^branch\." => |section, _| section == "branch",
            r"^remote\.[^.]+\.(url|pushurl)$" => |section, key| section == "remote" && (key == "url" || key == "pushurl"),
            _ => return unsupported("config pattern"),
        };
        // The repository's own config file, read as text: only plain
        // `[section "name"]` headers and `key = value` lines are understood.
        let text = std::fs::read_to_string(r.l.common_dir.join("config"))?;
        let mut out = String::new();
        let mut section: Option<(String, String)> = None;
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
                continue;
            }
            if let Some(header) = line.strip_prefix('[') {
                let Some(header) = header.strip_suffix(']') else { return unsupported("config syntax") };
                section = Some(match header.split_once(' ') {
                    None => (header.to_ascii_lowercase(), String::new()),
                    Some((name, sub)) => {
                        let sub = sub.trim();
                        if !(sub.starts_with('"') && sub.ends_with('"') && sub.len() >= 2) || sub[1..sub.len() - 1].contains(['"', '\\']) {
                            return unsupported("config syntax");
                        }
                        (name.to_ascii_lowercase(), sub[1..sub.len() - 1].to_string())
                    }
                });
                continue;
            }
            let Some((name, sub)) = &section else { return unsupported("config syntax") };
            let (key, value) = match line.split_once('=') {
                Some((k, v)) => (k.trim().to_ascii_lowercase(), v.trim()),
                None => (line.to_ascii_lowercase(), ""),
            };
            if name == "include" || name == "includeif" {
                return unsupported("config include");
            }
            if !wanted(name, &key) {
                continue;
            }
            if !line.contains('=') || value.contains(['"', '\\', '#', ';']) || sub.is_empty() {
                return unsupported("config syntax");
            }
            out.push_str(&format!("{name}.{sub}.{key} {value}\n"));
        }
        return Ok(Emulated { code: if out.is_empty() { 1 } else { 0 }, stdout: out.into_bytes() });
    }
    let ["--get", "--null", key] = rest else { return unsupported("config form") };
    let repo = shared.repo.to_thread_local();
    match repo.config_snapshot().string(*key) {
        Some(v) => {
            let mut out = v.to_vec();
            out.push(0);
            ok(out)
        }
        None => Ok(Emulated { stdout: Vec::new(), code: 1 }),
    }
}

/// Reflog of a ref as `(old id, new id, message)`, oldest first.
fn reflog_entries(r: &Repo, refname: &str) -> Vec<(String, String, String)> {
    let path = if refname.starts_with("refs/") { r.l.common_dir.join("logs").join(refname) } else { r.l.git_dir.join("logs").join(refname) };
    let Ok(text) = std::fs::read_to_string(path) else { return Vec::new() };
    text.lines()
        .filter_map(|line| {
            let (head, message) = line.split_once('\t').unwrap_or((line, ""));
            let mut ids = head.split(' ');
            Some((ids.next()?.to_string(), ids.next()?.to_string(), message.to_string()))
        })
        .collect()
}

/// `git reflog show --format=%H <ref>`.
fn emu_reflog_show(r: &Repo, rest: &[&str]) -> Result<Emulated> {
    let ["show", "--format=%H", refname] = rest else { return unsupported("reflog form") };
    if !refname.starts_with("refs/") || refs::resolve(&r.l, refname)?.is_none() {
        return unsupported("reflog of an unknown ref");
    }
    let mut out = String::new();
    for (_, new, _) in reflog_entries(r, refname).iter().rev() {
        out.push_str(new);
        out.push('\n');
    }
    ok(out)
}

/// `git log -g --format=+%H%x00%ct%x00%gs%x00%P`: HEAD's reflog, newest first.
fn emu_reflog_log(r: &Repo, shared: &Shared, rest: &[&str]) -> Result<Emulated> {
    if rest != ["--format=+%H%x00%ct%x00%gs%x00%P"] {
        return unsupported("reflog log form");
    }
    let commits = Commits::new(r.odb, &r.l.common_dir).with_memo(&shared.memo);
    let mut out = Vec::new();
    for (_, new, message) in reflog_entries(r, "HEAD").iter().rev() {
        let c = commits.raw(&unhex(new)?)?;
        out.extend_from_slice(format!("+{new}\0{}\0{message}\0{}\n", view::ident_time(&c.committer), c.parents.join(" ")).as_bytes());
    }
    ok(out)
}

/// `git tag --list -n --sort=-creatordate`: name, padded, and the first line
/// of the annotation (or of the commit message for a lightweight tag).
fn emu_tag_list(r: &Repo, shared: &Shared, rest: &[&str]) -> Result<Emulated> {
    if rest != ["--list", "-n", "--sort=-creatordate"] {
        return unsupported("tag form");
    }
    let commits = Commits::new(r.odb, &r.l.common_dir).with_memo(&shared.memo);
    let mut rows = Vec::new();
    for (name, oid) in view::list_refs(&r.l, "refs/tags/")? {
        let (kind, data) = r.odb.read(&oid)?;
        let (date, line) = match kind {
            Kind::Commit => {
                let c = commits.raw(&oid)?;
                (view::ident_time(&c.committer), first_line(&c.message))
            }
            Kind::Tag => {
                let split = data.windows(2).position(|w| w == b"\n\n").unwrap_or(data.len());
                let header = String::from_utf8_lossy(&data[..split]).to_string();
                let tagger = header.lines().find_map(|l| l.strip_prefix("tagger ")).unwrap_or("");
                (view::ident_time(tagger.as_bytes()), first_line(data.get(split + 2..).unwrap_or_default()))
            }
            _ => return unsupported("tag of a tree or blob"),
        };
        rows.push((date, name["refs/tags/".len()..].to_string(), line));
    }
    rows.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    let mut out = String::new();
    for (_, name, line) in rows {
        if name.chars().any(|c| !c.is_ascii()) {
            return unsupported("non-ASCII tag name");
        }
        out.push_str(&format!("{name:<15} {line}\n"));
    }
    ok(out)
}

fn first_line(message: &[u8]) -> String {
    let text = String::from_utf8_lossy(message);
    text.lines().next().unwrap_or("").to_string()
}
