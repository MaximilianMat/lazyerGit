# lazyerGit

A fast replacement for lazygit's hot paths: commit, amend (HEAD and older
commits), moving a patch out of a commit into the index, switching branches
and switching worktrees.

The engine performs these operations in-process on the repository files.
Anything it does not handle itself is declined and handed to real git, so the
result is always what git would have produced.

## Install

    ./install.sh

builds everything and installs two files, `lazygit` (the fork) and `lazyer`
(the daemon), into `~/bin` (or `--prefix DIR`). If that directory is not ahead
of the stock lazygit on your PATH, one marked line is added to your shell
profile. The stock lazygit is not touched. `./install.sh --uninstall` removes
the two files and that line again. Needs Go and Rust.

To try a build without installing:

    fork/build.sh
    LAZYER_BIN=$PWD/target/release/lazyer target/lazygit-lazyer -p <worktree>

`lazygit-lazyer` is lazygit v0.66.2 with a small patch: before it starts a git
process it asks the `lazyer` daemon whether it can answer that exact command
line. The daemon starts on first use, keeps running (it ends itself after
eight idle hours) and serves every repository through
`~/.cache/lazyer/daemon.sock`. `LAZYER=0` switches the fork back to plain
lazygit behaviour; `lazyer status` and `lazyer stop` inspect and end the
daemon. With `LAZYER_TRACE=1` in the daemon's environment it logs every
request and whether it answered it.

## Layout

- `crates/lazyer-core`: the engine. `api.rs` is the contract; `native/` the
  implementation.
- `crates/lazyer-testkit`: synthetic repositories, the real-git reference
  backend, end-state snapshots, and the scenarios every backend must pass.
- `crates/lazyer`: the daemon and a small command-line client.
- `crates/lazyer-bench`: benchmarks over the same scenarios.
- `fork/`: the lazygit patch and `build.sh`; `fork/lazygit` is a local clone,
  not committed here.
- `scripts/probe.sh`: measures the shape and git/lazygit timings of a real
  repository (numbers only) so a synthetic one can be built to match.
- `scripts/lazygit_e2e.py`: end-to-end latency of lazygit itself on a fixture.

## Acceptance

A scenario prepares a state with real git on two identical fixtures, runs the
operation with git on one and with the candidate on the other, and compares
refs, HEAD, commit ids, index entries, every file in the worktree,
`git status` and `git fsck`.

    cargo test                       # unit tests + differential tests

`tests/native.rs` lists the scenarios the engine must handle itself; falling
back to git on one of those fails the test. `tests/emulate.rs` runs every git
command line the daemon answers through real git as well and requires the
same bytes on stdout and the same exit code.

## Benchmarks

    cargo build --release -p lazyer-bench
    target/release/lazyer-bench fixture work          # ~1 min, ~750 MB under target/fixtures
    target/release/lazyer-bench run --backends git,native [--filter switch] [--tuned]
    python3 scripts/lazygit_e2e.py                    # what lazygit itself takes
    LAZYER_BIN=$PWD/target/release/lazyer python3 scripts/lazygit_e2e.py --bin target/lazygit-lazyer

`LAZYER_TRACE=1` prints phase timings of the native engine.
