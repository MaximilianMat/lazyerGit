#!/usr/bin/env bash
# Build the lazygit fork that talks to the lazyer daemon.
#
# Fetches lazygit at the pinned tag into fork/lazygit (not committed here),
# applies fork/patches/*.patch and builds:
#
#   target/lazygit-lazyer    lazygit, same UI, asks the daemon before running git
#   target/release/lazyer    the daemon
#
# Needs Go (the toolchain lazygit asks for is downloaded automatically) and
# Rust (cargo).
set -euo pipefail
TAG=v0.66.2
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT/fork"
if [ ! -d lazygit/.git ]; then
  git clone -q --depth 1 --branch "$TAG" https://github.com/jesseduffield/lazygit.git lazygit
fi
cd lazygit
if [ -z "$(git status --porcelain)" ]; then
  for p in ../patches/*.patch; do git apply --whitespace=nowarn "$p"; done
else
  echo "fork/lazygit has local changes: building it as it is, patches not re-applied" >&2
fi
go build -o "$ROOT/target/lazygit-lazyer" .
cd "$ROOT"
cargo build --release -p lazyer
echo "built target/lazygit-lazyer and target/release/lazyer"
