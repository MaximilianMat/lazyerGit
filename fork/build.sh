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
# Homebrew's rustup is keg-only, so cargo is often not on PATH.
if ! command -v cargo >/dev/null 2>&1; then
  for dir in /opt/homebrew/opt/rustup/bin /usr/local/opt/rustup/bin "$HOME/.cargo/bin"; do
    if [ -x "$dir/cargo" ]; then
      PATH="$dir:$PATH"
      break
    fi
  done
fi
if ! command -v cargo >/dev/null 2>&1; then
  echo "cargo not found: install Rust (brew install rustup && rustup default stable) or add it to PATH" >&2
  exit 1
fi
cargo build --release -p lazyer
# End a running daemon: it still is the previous build. The next request
# starts the new one.
"$ROOT/target/release/lazyer" stop >/dev/null 2>&1 || true
echo "built target/lazygit-lazyer and target/release/lazyer"
