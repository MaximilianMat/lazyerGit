#!/usr/bin/env bash
# Build lazyerGit and install it so that `lazygit` is the fork.
#
#   ./install.sh               build, install, make sure it shadows stock lazygit
#   ./install.sh --uninstall   remove what this script installed
#
# Options:
#   --prefix DIR    install into DIR (default: ~/bin, else ~/.local/bin)
#   --no-profile    never touch the shell profile; only say what is missing
#   --no-build      install the binaries already in target/
#   --force         replace a `lazygit` in DIR that this script did not put there
#
# Two files are installed: DIR/lazygit (lazygit with the lazyer patch) and
# DIR/lazyer (the daemon). The stock lazygit stays where it is; it is simply
# found later on the PATH. If DIR is not ahead of it on the PATH, one line is
# added to your shell profile (marked `# lazyerGit`), and removed again by
# --uninstall.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")" && pwd)"
STATE="$HOME/.cache/lazyer"
MANIFEST="$STATE/installed"
MARK="# lazyerGit"
PREFIX=""
PROFILE=1
BUILD=1
FORCE=0
UNINSTALL=0

while [ $# -gt 0 ]; do
  case "$1" in
    --prefix) PREFIX="${2:?--prefix needs a directory}"; shift ;;
    --no-profile) PROFILE=0 ;;
    --no-build) BUILD=0 ;;
    --force) FORCE=1 ;;
    --uninstall) UNINSTALL=1 ;;
    -h|--help) sed -n '2,17p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "unknown option: $1" >&2; exit 2 ;;
  esac
  shift
done

profile_file() {
  case "${SHELL:-}" in
    */zsh) echo "$HOME/.zshrc" ;;
    */bash) if [ -f "$HOME/.bash_profile" ]; then echo "$HOME/.bash_profile"; else echo "$HOME/.bashrc"; fi ;;
    *) echo "$HOME/.profile" ;;
  esac
}

remove_profile_line() {
  local file
  file="$(profile_file)"
  if [ -f "$file" ] && grep -q "$MARK\$" "$file"; then
    grep -v "$MARK\$" "$file" > "$file.lazyer-tmp" && mv "$file.lazyer-tmp" "$file"
    echo "removed the PATH line from $file"
  fi
}

if [ "$UNINSTALL" = 1 ]; then
  if [ -f "$MANIFEST" ]; then
    "$(sed -n '2p' "$MANIFEST")" stop >/dev/null 2>&1 || true
    while IFS= read -r file; do
      [ -n "$file" ] && rm -f "$file" && echo "removed $file"
    done < "$MANIFEST"
    rm -f "$MANIFEST"
  else
    echo "nothing recorded as installed"
  fi
  remove_profile_line
  hash -r 2>/dev/null || true
  echo "lazygit is now: $(command -v lazygit || echo 'not on PATH')"
  exit 0
fi

# ---- where to install
on_path() { case ":$PATH:" in *":$1:"*) return 0 ;; *) return 1 ;; esac; }
if [ -z "$PREFIX" ]; then
  if on_path "$HOME/bin" || [ -d "$HOME/bin" ]; then PREFIX="$HOME/bin"
  elif on_path "$HOME/.local/bin"; then PREFIX="$HOME/.local/bin"
  else PREFIX="$HOME/bin"
  fi
fi
mkdir -p "$PREFIX" "$STATE"
PREFIX="$(cd "$PREFIX" && pwd)"

ours() {
  # Ours if recorded, or a link into this checkout.
  { [ -f "$MANIFEST" ] && grep -qx "$1" "$MANIFEST"; } && return 0
  [ -L "$1" ] && case "$(readlink "$1")" in "$ROOT"/*) return 0 ;; esac
  return 1
}
for name in lazygit lazyer; do
  if [ -e "$PREFIX/$name" ] || [ -L "$PREFIX/$name" ]; then
    if ! ours "$PREFIX/$name" && [ "$FORCE" = 0 ]; then
      echo "$PREFIX/$name exists and was not installed by this script." >&2
      echo "Use --force to replace it, or --prefix to install elsewhere." >&2
      exit 1
    fi
  fi
done

# ---- build
if [ "$BUILD" = 1 ]; then
  "$ROOT/fork/build.sh"
fi
for built in "$ROOT/target/lazygit-lazyer" "$ROOT/target/release/lazyer"; do
  [ -x "$built" ] || { echo "$built is missing: run without --no-build" >&2; exit 1; }
done

# ---- install: copy next to the target, then rename, so a running lazygit
# never sees a half-written file.
put() {
  rm -f "$2.new"
  cp "$1" "$2.new"
  chmod 755 "$2.new"
  mv -f "$2.new" "$2"
}
"$PREFIX/lazyer" stop >/dev/null 2>&1 || "$ROOT/target/release/lazyer" stop >/dev/null 2>&1 || true
put "$ROOT/target/lazygit-lazyer" "$PREFIX/lazygit"
put "$ROOT/target/release/lazyer" "$PREFIX/lazyer"
printf '%s\n%s\n' "$PREFIX/lazygit" "$PREFIX/lazyer" > "$MANIFEST"
echo "installed $PREFIX/lazygit and $PREFIX/lazyer"

# ---- does it shadow the stock lazygit?
first_lazygit() {
  local dir IFS=:
  for dir in $1; do
    if [ -x "$dir/lazygit" ]; then echo "$dir/lazygit"; return; fi
  done
}
if [ "$(first_lazygit "$PATH")" = "$PREFIX/lazygit" ]; then
  echo "lazygit now resolves to $PREFIX/lazygit"
  echo "(in a shell that already ran lazygit, type 'rehash' or open a new terminal)"
else
  line="export PATH=\"$PREFIX:\$PATH\"  $MARK"
  file="$(profile_file)"
  if [ "$PROFILE" = 1 ]; then
    if ! { [ -f "$file" ] && grep -q "$MARK\$" "$file"; }; then
      printf '\n%s\n' "$line" >> "$file"
      echo "added to $file:  $line"
    fi
    echo "open a new terminal (or run: source $file) for lazygit to resolve to $PREFIX/lazygit"
  else
    echo "lazygit still resolves to: $(first_lazygit "$PATH")"
    echo "put $PREFIX ahead of it on your PATH, for example:  $line"
  fi
fi
other="$(PATH="$(printf '%s' "$PATH" | tr ':' '\n' | grep -vx "$PREFIX" | paste -sd: -)" command -v lazygit || true)"
[ -n "$other" ] && echo "stock lazygit is untouched at $other"
echo "switch back any time with: $0 --uninstall"
