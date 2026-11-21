# lazyerGit

A fast replacement for lazygit's hot paths: commit, amend (HEAD and older
commits), moving a patch out of a commit into the index, switching branches
and switching worktrees.

The engine performs these operations in-process on the repository files.
Anything it does not handle itself is declined and handed to real git, so the
result is always what git would have produced.

## Layout

- `crates/lazyer-core`: the engine. `api.rs` is the contract; `native/` the
  implementation.
- `crates/lazyer-testkit`: synthetic repositories, the real-git reference
  backend, end-state snapshots, and the scenarios every backend must pass.
- `crates/lazyer-bench`: benchmarks over the same scenarios.
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
back to git on one of those fails the test.

## Benchmarks

    cargo build --release -p lazyer-bench
    target/release/lazyer-bench fixture work          # ~1 min, ~750 MB under target/fixtures
    target/release/lazyer-bench run --backends git,native [--filter switch] [--tuned]
    python3 scripts/lazygit_e2e.py                    # what lazygit itself takes

`LAZYER_TRACE=1` prints phase timings of the native engine.
