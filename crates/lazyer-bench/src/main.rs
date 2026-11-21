//! Benchmarks: every scenario, every backend, on a cached synthetic repo.
//!
//!   lazyer-bench fixture [small|work]
//!   lazyer-bench run [--profile work] [--backends git,git-shim] [--iters 5]
//!                    [--filter text] [--tuned]

use anyhow::{bail, Result};
use lazyer_core::Backend;
use lazyer_testkit::baseline::Baseline;
use lazyer_testkit::differential::compared_worktrees;
use lazyer_testkit::git::run;
use lazyer_testkit::{scenario, Fixture, GitCli, Profile};
use std::path::PathBuf;
use std::time::Instant;

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let opt = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
    let flag = |name: &str| args.iter().any(|a| a == name);
    match args.first().map(String::as_str) {
        Some("fixture") => {
            let name = args.get(1).map(String::as_str).unwrap_or("work");
            let Some(profile) = Profile::by_name(name) else { bail!("unknown profile {name}") };
            let t = Instant::now();
            let fx = Fixture::cached(&profile, &fixtures_dir())?;
            println!("{} ({:.1}s)", fx.repo.display(), t.elapsed().as_secs_f64());
            Ok(())
        }
        Some("run") => {
            let name = opt("--profile").unwrap_or_else(|| "work".into());
            let Some(profile) = Profile::by_name(&name) else { bail!("unknown profile {name}") };
            let iters: usize = opt("--iters").and_then(|s| s.parse().ok()).unwrap_or(5);
            let wanted = opt("--backends").unwrap_or_else(|| "git".into());
            let mut backends: Vec<Box<dyn Backend>> = Vec::new();
            for b in wanted.split(',') {
                backends.push(match b {
                    "git" => Box::new(GitCli::real()),
                    "git-shim" => Box::new(GitCli::shim()),
                    "native" => Box::new(lazyer_testkit::fallback::WithFallback::new(lazyer_core::Native::isolated())),
                    other => bail!("unknown backend {other}"),
                });
            }
            bench(&profile, &backends, iters, opt("--filter").as_deref(), flag("--tuned"))
        }
        Some("prepare") => {
            // Bring the fixture into a scenario's start state and print the
            // worktree and the scenario's arguments, one per line.
            let Some(sc_name) = args.get(1) else { bail!("prepare <scenario>") };
            let name = opt("--profile").unwrap_or_else(|| "work".into());
            let Some(profile) = Profile::by_name(&name) else { bail!("unknown profile {name}") };
            let fx = Fixture::cached(&profile, &fixtures_dir())?;
            Baseline::of_fixture(&fx.root, &compared_worktrees(&profile, &fx))?;
            let Some(sc) = scenario::all().into_iter().find(|s| s.name == sc_name) else { bail!("unknown scenario {sc_name}") };
            let ctx = (sc.prepare)(&fx)?;
            println!("{}", ctx.wt.display());
            for a in ctx.args {
                println!("{a}");
            }
            Ok(())
        }
        Some("restore") => {
            let name = opt("--profile").unwrap_or_else(|| "work".into());
            let Some(profile) = Profile::by_name(&name) else { bail!("unknown profile {name}") };
            let fx = Fixture::cached(&profile, &fixtures_dir())?;
            Baseline::of_fixture(&fx.root, &compared_worktrees(&profile, &fx))?.restore(&fx.repo)
        }
        _ => bail!("usage: lazyer-bench fixture [small|work] | run [--profile P] [--backends a,b] [--iters N] [--filter text] [--tuned]"),
    }
}

fn fixtures_dir() -> PathBuf {
    std::env::var_os("LAZYER_FIXTURES").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("target/fixtures"))
}

fn median(v: &mut [f64]) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[v.len() / 2]
}

/// Git's own speed-ups, all understood by stock git: file-system monitor,
/// untracked cache, index v4 without trailing hash.
fn tune(worktrees: &[PathBuf], on: bool) -> Result<()> {
    let repo = &worktrees[0];
    for (k, v) in [("core.fsmonitor", "true"), ("core.untrackedCache", "true"), ("feature.manyFiles", "true")] {
        if on {
            run(repo, &["config", k, v])?;
        } else {
            let _ = run(repo, &["config", "--unset", k]);
        }
    }
    for wt in worktrees {
        if on {
            run(wt, &["update-index", "--index-version", "4"])?;
            run(wt, &["status", "--porcelain"])?;
            run(wt, &["status", "--porcelain"])?;
        } else {
            let _ = run(wt, &["fsmonitor--daemon", "stop"]);
            run(wt, &["update-index", "--index-version", "2", "--no-fsmonitor", "--no-untracked-cache"])?;
        }
    }
    Ok(())
}

fn bench(profile: &Profile, backends: &[Box<dyn Backend>], iters: usize, filter: Option<&str>, tuned: bool) -> Result<()> {
    let fx = Fixture::cached(profile, &fixtures_dir())?;
    let worktrees = compared_worktrees(profile, &fx);
    let baseline = Baseline::of_fixture(&fx.root, &worktrees)?;
    baseline.restore(&fx.repo)?;
    if tuned {
        tune(&worktrees, true)?;
    }
    let result = bench_inner(&fx, &baseline, backends, iters, filter, tuned);
    baseline.restore(&fx.repo)?;
    if tuned {
        tune(&worktrees, false)?;
    }
    result
}

fn bench_inner(fx: &Fixture, baseline: &Baseline, backends: &[Box<dyn Backend>], iters: usize, filter: Option<&str>, tuned: bool) -> Result<()> {
    println!("profile: {}   iterations: {iters}   git config: {}", fx.root.file_name().unwrap().to_string_lossy(), if tuned { "tuned" } else { "stock" });
    println!("op = the operation alone; +view = operation followed by loading everything the UI shows\n");
    println!("| {:<22} | {:<10} | {:>10} | {:>10} |", "scenario", "backend", "op ms", "+view ms");
    println!("|{:-<24}|{:-<12}|{:->12}|{:->12}|", "", "", "", "");
    for sc in scenario::all() {
        if filter.is_some_and(|f| !sc.name.contains(f)) {
            continue;
        }
        let read_only = sc.name.starts_with("status_clean") || sc.name.starts_with("view_");
        for b in backends {
            let (mut op, mut total) = (Vec::new(), Vec::new());
            let mut failed = None;
            let mut ctx = None;
            for i in 0..iters {
                if !read_only || i == 0 {
                    baseline.restore(&fx.repo)?;
                    ctx = Some((sc.prepare)(fx)?);
                    baseline.settle()?;
                }
                let c = ctx.as_ref().unwrap();
                let t = Instant::now();
                let r = (sc.run)(b.as_ref(), c);
                let t_op = t.elapsed().as_secs_f64() * 1000.0;
                if let Err(e) = &r {
                    if !sc.must_fail {
                        failed = Some(format!("{e:#}"));
                        break;
                    }
                }
                let is_view = sc.name.starts_with("view_");
                if !is_view {
                    b.load_view(&c.wt)?;
                }
                op.push(t_op);
                total.push(t.elapsed().as_secs_f64() * 1000.0);
            }
            match failed {
                Some(e) => println!("| {:<22} | {:<10} | {:>23} |", sc.name, b.name(), format!("FAILED: {}", e.chars().take(60).collect::<String>())),
                None => println!("| {:<22} | {:<10} | {:>10.1} | {:>10.1} |", sc.name, b.name(), median(&mut op), median(&mut total)),
            }
        }
    }
    Ok(())
}
