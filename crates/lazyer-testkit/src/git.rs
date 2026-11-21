//! Running real git with a fixed, user-independent environment.

use anyhow::{bail, Context, Result};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;

pub const TEST_NAME: &str = "Lazyer Test";
pub const TEST_EMAIL: &str = "test@lazyer.invalid";
pub const TEST_TIME: i64 = 1_700_000_000;

/// The git binary itself, bypassing Apple's `/usr/bin/git` shim. Override with
/// `LAZYER_GIT`.
pub fn real_git() -> &'static Path {
    static BIN: OnceLock<PathBuf> = OnceLock::new();
    BIN.get_or_init(|| {
        if let Ok(p) = std::env::var("LAZYER_GIT") {
            return PathBuf::from(p);
        }
        if let Ok(out) = Command::new("xcrun").args(["-f", "git"]).output() {
            let p = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if out.status.success() && !p.is_empty() {
                return PathBuf::from(p);
            }
        }
        PathBuf::from("git")
    })
}

/// A git command in `dir` that ignores the user's global and system config
/// and uses a fixed identity and date.
pub fn git_with(bin: &Path, dir: &Path) -> Command {
    let mut c = Command::new(bin);
    c.current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_AUTHOR_NAME", TEST_NAME)
        .env("GIT_AUTHOR_EMAIL", TEST_EMAIL)
        .env("GIT_AUTHOR_DATE", format!("{TEST_TIME} +0000"))
        .env("GIT_COMMITTER_NAME", TEST_NAME)
        .env("GIT_COMMITTER_EMAIL", TEST_EMAIL)
        .env("GIT_COMMITTER_DATE", format!("{TEST_TIME} +0000"))
        .env("LC_ALL", "C")
        // Fixtures live below this project's own repository: never let a
        // command that misses its fixture fall through to the enclosing one.
        .env("GIT_CEILING_DIRECTORIES", dir.parent().unwrap_or(dir))
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE");
    c
}

pub fn git(dir: &Path) -> Command {
    git_with(real_git(), dir)
}

/// Run to completion, return stdout; a non-zero exit is an error carrying stderr.
pub fn output(cmd: &mut Command) -> Result<Vec<u8>> {
    let out = cmd.stdin(Stdio::null()).output().with_context(|| format!("spawning {cmd:?}"))?;
    if !out.status.success() {
        bail!("{:?} failed ({}): {}", cmd, out.status, String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(out.stdout)
}

pub fn output_with_stdin(cmd: &mut Command, input: &[u8]) -> Result<Vec<u8>> {
    let mut child = cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().with_context(|| format!("spawning {cmd:?}"))?;
    child.stdin.take().unwrap().write_all(input)?;
    let out = child.wait_with_output()?;
    if !out.status.success() {
        bail!("{:?} failed ({}): {}", cmd, out.status, String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(out.stdout)
}

/// `git <args>` in `dir`, stdout as trimmed text.
pub fn run(dir: &Path, args: &[&str]) -> Result<String> {
    let out = output(git(dir).args(args))?;
    Ok(String::from_utf8_lossy(&out).trim_end().to_string())
}

pub fn lines(dir: &Path, args: &[&str]) -> Result<Vec<String>> {
    Ok(run(dir, args)?.lines().map(str::to_string).collect())
}

/// True when the command exits with status 0.
pub fn succeeds(dir: &Path, args: &[&str]) -> bool {
    git(dir).args(args).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status().map(|s| s.success()).unwrap_or(false)
}
