#!/usr/bin/env bash
# lazyerGit probe: measures the shape of a git repository and how long git and
# lazygit take in it, so a synthetic repo with the same shape can be built
# elsewhere for benchmarks.
#
# Run it from inside the repository (any worktree):
#
#   bash probe.sh > probe.txt                 # read-only
#   bash probe.sh --with-writes > probe.txt   # also times checkout/commit/rebase
#
# What it prints: counts, sizes, timings, git config switches, names of hooks
# and of known endpoint-security processes. It never prints file names, paths,
# branch names, commit messages, authors, remotes or file contents.
#
# Read-only mode takes no locks and writes nothing into the repository
# (GIT_OPTIONAL_LOCKS=0), except that the lazygit trace starts lazygit the way
# you normally would (skip with --no-lazygit).
#
# --with-writes creates a temporary detached worktree in $TMPDIR, checks out
# other commits, commits, amends and rebases inside it, then removes it. Your
# branches, index and working tree are not touched; hooks and signing are
# disabled for these commits. It leaves a few unreachable objects behind that
# `git gc` collects later.

set -u
export LC_ALL=C

WITH_WRITES=0
RUNS=5
SETTLE=10
NO_LAZYGIT=0

usage() { sed -n '2,24p' "$0" | sed 's/^# \{0,1\}//'; echo; echo "Options: --with-writes  --no-lazygit  --runs N (default 5)  --settle SECONDS (default 10)"; }

while [ $# -gt 0 ]; do
  case "$1" in
    --with-writes) WITH_WRITES=1 ;;
    --no-lazygit) NO_LAZYGIT=1 ;;
    --runs) RUNS=$2; shift ;;
    --settle) SETTLE=$2; shift ;;
    -h|--help) usage; exit 0 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
  shift
done

git rev-parse --is-inside-work-tree >/dev/null 2>&1 || { echo "run this inside a git worktree" >&2; exit 1; }
cd "$(git rev-parse --show-toplevel)" || exit 1

kv() { printf '%s=%s\n' "$1" "$2"; }
section() { printf '\n[%s]\n' "$1"; }
count() { wc -l | tr -d ' '; }

# stats: numbers on stdin -> n, sum, percentiles
stats() {
  perl -e '
    my @v = sort { $a <=> $b } grep { /^\d+(\.\d+)?$/ } map { chomp; $_ } <STDIN>;
    my $n = @v; my $s = 0; $s += $_ for @v;
    my $p = sub { $n ? $v[ int(($n - 1) * $_[0]) ] : 0 };
    printf "n=%d sum=%d p50=%d p90=%d p99=%d max=%d\n", $n, $s, $p->(.5), $p->(.9), $p->(.99), $n ? $v[-1] : 0;
  '
}

# tm LABEL RUNS CMD...: wall time of CMD, output discarded
tm() {
  local label=$1 runs=$2
  shift 2
  perl -MTime::HiRes=time -e '
    my ($label, $runs, @cmd) = @ARGV; my @t; my $rc = 0;
    for (1 .. $runs) {
      my $s = time; my $pid = fork;
      if (!$pid) { open STDOUT, ">", "/dev/null"; open STDERR, ">", "/dev/null"; exec @cmd or exit 127 }
      waitpid $pid, 0; $rc ||= $? >> 8; push @t, (time - $s) * 1000;
    }
    my @s = sort { $a <=> $b } @t;
    printf "time.%s first_ms=%.1f min_ms=%.1f median_ms=%.1f runs=%d rc=%d\n", $label, $t[0], $s[0], $s[ int($#s / 2) ], $runs, $rc;
  ' "$label" "$runs" "$@"
}

GIT_COMMON=$(cd "$(git rev-parse --git-common-dir)" && pwd)
GIT_DIR_ABS=$(cd "$(git rev-parse --git-dir)" && pwd)

echo "# lazyerGit probe v1  with_writes=$WITH_WRITES runs=$RUNS"

# ---------------------------------------------------------------- environment
section env
kv os "$(sw_vers -productName 2>/dev/null) $(sw_vers -productVersion 2>/dev/null) $(uname -sm)"
kv cpu "$(sysctl -n machdep.cpu.brand_string 2>/dev/null)"
kv ncpu "$(sysctl -n hw.ncpu 2>/dev/null || nproc 2>/dev/null)"
kv mem_gb "$(( $(sysctl -n hw.memsize 2>/dev/null || echo 0) / 1073741824 ))"
kv git_version "$(git --version | sed 's/^git version //')"
kv git_bin "$(command -v git)"
kv git_real_bin "$(xcrun -f git 2>/dev/null)"
kv lazygit_version "$(lazygit --version 2>/dev/null | sed -n 's/.*, version=\([^,]*\),.*/\1/p')"
kv python3 "$(command -v python3 >/dev/null 2>&1 && echo yes || echo no)"
dev=$(df . 2>/dev/null | awk 'NR==2{print $1}')
kv fs "$(mount 2>/dev/null | grep "^$dev " | sed 's/.*(\(.*\))/\1/' | head -1)"
tmpdev=$(df "${TMPDIR:-/tmp}" 2>/dev/null | awk 'NR==2{print $1}')
kv tmpdir_same_volume "$([ "$dev" = "$tmpdev" ] && echo yes || echo no)"
kv security_agents "$(ps -axo comm= 2>/dev/null | grep -iEo 'falcon[a-z_-]*|crowdstrike[a-z_-]*|wdavdaemon[a-z_-]*|microsoft defender[a-z ]*|sentinel(one|d|agent)[a-z_-]*|jamfprotect[a-z_-]*|jamf[a-z_-]*|sophos[a-z_-]*|cbdefense[a-z_-]*|carbon ?black[a-z_-]*|cylance[a-z_-]*|eset[_a-z-]*daemon|mcafee[a-z_-]*|trellix[a-z_-]*|symantec[a-z_-]*|elastic-endpoint|osqueryd|santad?|netskope[a-z_-]*|zscaler[a-z_-]*|cortex ?xdr[a-z_-]*|traps[a-z_-]*|kandji[a-z_-]*|tanium[a-z_-]*|fireeye[a-z_-]*|xagt' | sort -u | tr '\n' ',')"

# --------------------------------------------------------------------- config
section config
for k in core.fsmonitor core.untrackedcache core.preloadindex core.splitindex core.ignorecase \
         core.precomposeunicode core.autocrlf core.sparsecheckout core.sparsecheckoutcone core.commitgraph \
         core.multipackindex feature.manyfiles feature.experimental index.version index.skiphash \
         index.threads index.sparse fetch.writecommitgraph gc.auto gc.writecommitgraph maintenance.auto \
         maintenance.strategy pack.usebitmaps commit.gpgsign tag.gpgsign gpg.format extensions.refstorage \
         extensions.worktreeconfig checkout.workers status.showuntrackedfiles pull.rebase rebase.autosquash \
         rebase.autostash rebase.updaterefs rerere.enabled protocol.version; do
  v=$(git config --get "$k" 2>/dev/null) || v="(unset)"
  case "$k:$v" in core.fsmonitor:true|core.fsmonitor:false|core.fsmonitor:"(unset)") ;; core.fsmonitor:*) v="(hook)" ;; esac
  kv "$k" "$v"
done
for k in core.hookspath user.signingkey remote.origin.promisor remote.origin.partialclonefilter core.pager; do
  kv "$k" "$(git config --get "$k" >/dev/null 2>&1 && echo "(set)" || echo "(unset)")"
done
case "$(git remote get-url origin 2>/dev/null)" in
  https://*) kv remote_scheme https ;; http://*) kv remote_scheme http ;; ssh://*|*@*:*) kv remote_scheme ssh ;;
  "") kv remote_scheme none ;; *) kv remote_scheme other ;;
esac

# ---------------------------------------------------------------------- shape
section shape
kv tracked_files "$(git ls-files | count)"
git ls-files -z | perl -0 -e '
  my (%d, $maxdepth, $pathlen, $n);
  while (<STDIN>) { chomp; $n++; $pathlen += length; my @p = split m{/}; my $depth = @p - 1;
    $maxdepth = $depth if $depth > ($maxdepth // 0); pop @p; $d{ join "/", @p }++ }
  my @f = sort { $a <=> $b } values %d; my $nd = @f;
  printf "dirs_with_files=%d\nmax_depth=%d\navg_path_len=%d\n", $nd, $maxdepth // 0, $n ? $pathlen / $n : 0;
  printf "files_per_dir=p50:%d p90:%d p99:%d max:%d\n", map({ $nd ? $f[ int(($nd - 1) * $_) ] : 0 } .5, .9, .99), $nd ? $f[-1] : 0;
'
kv blob_bytes "$(git ls-tree -r -l HEAD 2>/dev/null | awk '{print $4}' | stats)"
kv blobs_over_1mb "$(git ls-tree -r -l HEAD 2>/dev/null | awk '$4+0 > 1048576' | count)"
kv index_bytes "$(wc -c < "$GIT_DIR_ABS/index" 2>/dev/null | tr -d ' ')"
kv index_version "$(perl -e 'open F, "<", $ARGV[0] or exit; binmode F; read F, $b, 8; print unpack("N", substr($b, 4))' "$GIT_DIR_ABS/index" 2>/dev/null)"
kv index_extensions "$(perl -e '
  open F, "<", $ARGV[0] or exit; binmode F; local $/; my $b = <F>; my ($sig, $ver, $n) = unpack "a4NN", $b;
  my $hashlen = 20; my $pos = 12;
  for (1 .. $n) {
    my $flags = unpack "n", substr $b, $pos + 40 + $hashlen, 2; my $namepos = $pos + 42 + $hashlen;
    $namepos += 2 if $flags & 0x4000;
    if ($ver == 4) { my $i = $namepos; 1 while ord(substr $b, $i++, 1) & 0x80; my $e = index $b, "\0", $i; $pos = $e + 1 }
    else { my $e = index $b, "\0", $namepos; my $len = $e - $pos; $pos += ($len + 8) & ~7 }
  }
  my @x; while ($pos + 8 <= length($b) - $hashlen) { my ($s, $l) = unpack "a4N", substr $b, $pos, 8; last unless $s =~ /^[A-Za-z]{4}$/; push @x, "$s:$l"; $pos += 8 + $l }
  print join ",", @x;' "$GIT_DIR_ABS/index" 2>/dev/null)"
kv commits_all "$(git rev-list --count --all 2>/dev/null)"
kv commits_head "$(git rev-list --count HEAD 2>/dev/null)"
kv merges_head "$(git rev-list --count --merges HEAD 2>/dev/null)"
kv refs_heads "$(git for-each-ref refs/heads | count)"
kv refs_remotes "$(git for-each-ref refs/remotes | count)"
kv refs_tags "$(git for-each-ref refs/tags | count)"
kv packed_refs_bytes "$([ -f "$GIT_COMMON/packed-refs" ] && wc -c < "$GIT_COMMON/packed-refs" | tr -d ' ' || echo 0)"
kv count_objects "$(git count-objects -v | tr '\n' ' ')"
kv commit_graph "$([ -e "$GIT_COMMON/objects/info/commit-graph" ] || [ -d "$GIT_COMMON/objects/info/commit-graphs" ] && echo yes || echo no)"
kv multi_pack_index "$([ -e "$GIT_COMMON/objects/pack/multi-pack-index" ] && echo yes || echo no)"
kv bitmaps "$(ls "$GIT_COMMON/objects/pack/"*.bitmap 2>/dev/null | count)"
kv worktrees "$(git worktree list --porcelain | grep -c '^worktree ')"
kv stashes "$(git stash list 2>/dev/null | count)"
kv submodules "$( [ -f .gitmodules ] && git config -f .gitmodules --get-regexp '\.path$' | count || echo 0)"
kv attr_filters "$(git ls-files -z -- '.gitattributes' '*/.gitattributes' | xargs -0 grep -h -o 'filter=[A-Za-z0-9_-]*' 2>/dev/null | sort | uniq -c | awk '{printf "%s:%s,", $2, $1}')"
kv attr_eol_rules "$(git ls-files -z -- '.gitattributes' '*/.gitattributes' | xargs -0 grep -h -c -E '(^|[[:space:]])(text|eol|ident|working-tree-encoding)' 2>/dev/null | awk '{s+=$1} END{print s+0}')"
hooksdir=$(git rev-parse --git-path hooks)
kv hooks "$(for h in "$hooksdir"/*; do [ -f "$h" ] && [ -x "$h" ] && case "$h" in *.sample) ;; *) basename "$h" ;; esac; done 2>/dev/null | tr '\n' ',')"
kv hook_frameworks "$([ -f .pre-commit-config.yaml ] && printf 'pre-commit,'; [ -d .husky ] && printf 'husky,'; ls lefthook.y*ml .lefthook.y*ml >/dev/null 2>&1 && printf 'lefthook,')"

section worktree_state
export GIT_OPTIONAL_LOCKS=0
kv modified_tracked "$(git diff --name-only | count)"
kv staged "$(git diff --cached --name-only | count)"
kv untracked "$(git ls-files -o --exclude-standard | count)"
kv ignored_entries "$(git ls-files -o -i --exclude-standard --directory | count)"

section history
kv files_per_commit_last500 "$(git log -500 --no-merges --format='format:@' --name-only 2>/dev/null | awk '/^@$/{if(seen)print n; n=0; seen=1; next} NF{n++} END{if(seen)print n}' | stats)"
i=0; MAXDIFF=0; MAXSHA=""
for sha in $(git for-each-ref --sort=-committerdate --count=15 --format='%(objectname)' refs/heads); do
  i=$((i + 1))
  nd=$(git diff --name-only HEAD "$sha" 2>/dev/null | count)
  lr=$(git rev-list --left-right --count "HEAD...$sha" 2>/dev/null | tr '\t' '/')
  kv "branch.$i" "files_differ_from_head:$nd ahead/behind:$lr"
  if [ "$nd" -gt "$MAXDIFF" ]; then MAXDIFF=$nd; MAXSHA=$sha; fi
done

# -------------------------------------------------------- read-only timings
section timings_readonly
tm rev_parse_head "$RUNS" git rev-parse HEAD
REALGIT=$(xcrun -f git 2>/dev/null)
[ -n "$REALGIT" ] && [ -x "$REALGIT" ] && tm rev_parse_head_real_bin "$RUNS" "$REALGIT" rev-parse HEAD
tm status_v2 "$RUNS" git status --porcelain=v2 --branch
tm status_lazygit "$RUNS" git status --untracked-files=all --porcelain -z --find-renames=50%
tm status_no_untracked "$RUNS" git status --porcelain -uno
tm ls_files "$RUNS" git ls-files
tm lstat_all_tracked "$RUNS" sh -c 'git ls-files -z | perl -0 -ne "chomp; lstat"'
tm untracked_walk "$RUNS" git ls-files -o --exclude-standard
tm diff_worktree "$RUNS" git diff --stat
tm diff_cached "$RUNS" git diff --cached --stat
tm log_300_lazygit "$RUNS" git log HEAD --topo-order --oneline "--pretty=format:%H%x00%at%x00%aN%x00%ae%x00%D%x00%P%x00%m%x00%s" --abbrev=40 -300 --no-show-signature --
tm for_each_ref_heads "$RUNS" git for-each-ref --sort=-committerdate "--format=%(HEAD)%00%(refname:short)%00%(upstream:short)%00%(upstream:track)%00%(subject)%00%(objectname)" refs/heads
tm for_each_ref_all "$RUNS" git for-each-ref
tm rev_list_count_all "$RUNS" git rev-list --count --all
tm worktree_list "$RUNS" git worktree list --porcelain
tm stash_list "$RUNS" git stash list
for back in 1 100; do
  if git rev-parse -q --verify "HEAD~$back" >/dev/null 2>&1; then
    kv "switch_dryrun.head~$back.files" "$(git diff --name-only HEAD "HEAD~$back" | count)"
    tm "switch_dryrun.head~$back" "$RUNS" git read-tree -n -m -u HEAD "HEAD~$back"
  fi
done
if [ -n "$MAXSHA" ]; then
  kv switch_dryrun.most_divergent_branch.files "$MAXDIFF"
  tm switch_dryrun.most_divergent_branch "$RUNS" git read-tree -n -m -u HEAD "$MAXSHA"
fi
unset GIT_OPTIONAL_LOCKS

# ------------------------------------------------------------ lazygit trace
if [ "$NO_LAZYGIT" = 0 ] && command -v lazygit >/dev/null 2>&1 && command -v python3 >/dev/null 2>&1; then
  section lazygit_startup
  tm lazygit_binary_start "$RUNS" lazygit --version
  LGTMP=$(mktemp -d)
  # Runs a command in a real pseudo-terminal (160x50), answers the terminal
  # queries lazygit sends, presses q after the settle time.
  cat > "$LGTMP/ptyrun.py" <<'PYEOF'
import os, pty, sys, time, select, struct, fcntl, termios, signal
settle = float(sys.argv[1]); cmd = sys.argv[2:]
pid, fd = pty.fork()
if pid == 0:
    fcntl.ioctl(0, termios.TIOCSWINSZ, struct.pack("HHHH", 50, 160, 0, 0))
    os.environ["TERM"] = "xterm-256color"
    os.execvp(cmd[0], cmd)
start = time.time(); sent = 0; t_q = 0; nbytes = 0; last_out = start; killed = "no"
while True:
    r, _, _ = select.select([fd], [], [], 0.1)
    now = time.time()
    if r:
        try:
            data = os.read(fd, 65536)
        except OSError:
            break
        if not data:
            break
        nbytes += len(data)
        if sent == 0: last_out = now
        if b"\x1b]11;?" in data: os.write(fd, b"\x1b]11;rgb:0000/0000/0000\x1b\\")
        if b"\x1b[c" in data: os.write(fd, b"\x1b[?62;c")
        if b"\x1b[6n" in data: os.write(fd, b"\x1b[1;1R")
    if sent == 0 and now - start >= settle:
        os.write(fd, b"q"); sent = 1; t_q = now
    elif sent == 1 and now - t_q >= 1.5:
        os.write(fd, b"\x1bq"); sent = 2
    elif sent == 2 and now - t_q >= 4:
        os.kill(pid, signal.SIGKILL); killed = "yes"; break
try:
    os.waitpid(pid, 0)
except OSError:
    pass
print("pty_bytes=%d last_frame_before_quit_ms=%.0f forced_kill=%s" % (nbytes, (last_out - start) * 1000, killed))
PYEOF
  printf 'git:\n  autoFetch: false\nupdate:\n  method: never\ndisableStartupPopups: true\n' > "$LGTMP/probe.yml"
  usercfg="$(lazygit --print-config-dir 2>/dev/null)/config.yml"
  cfg="$LGTMP/probe.yml"
  [ -f "$usercfg" ] && cfg="$usercfg,$LGTMP/probe.yml"
  for run in 1 2; do
    tr="$LGTMP/trace$run.json"
    launch=$(perl -MTime::HiRes=time -e 'printf "%.6f", time')
    printf 'lazygit.run%d.tui ' "$run"
    GIT_TRACE2_EVENT="$tr" LG_CONFIG_FILE="$cfg" python3 "$LGTMP/ptyrun.py" "$SETTLE" lazygit 2>/dev/null || echo "pty_failed"
    perl -MJSON::PP -MTime::Local -e '
      my ($file, $launch, $run) = @ARGV; my (%p, @order);
      open my $fh, "<", $file or do { print "lazygit.run$run=trace_missing\n"; exit };
      my $epoch = sub { $_[0] =~ /^(\d+)-(\d+)-(\d+)T(\d+):(\d+):(\d+)(\.\d+)?Z/ or return 0; timegm($6, $5, $4, $3, $2 - 1, $1) + ($7 // 0) };
      while (<$fh>) {
        my $e = eval { decode_json($_) } or next; my $sid = $e->{sid} // next; next if $sid =~ m{/};
        if ($e->{event} eq "start") { push @order, $sid; $p{$sid} = { t0 => $epoch->($e->{time}), argv => $e->{argv} } }
        elsif ($e->{event} eq "atexit" && $p{$sid}) { $p{$sid}{dur} = $e->{t_abs} }
      }
      my ($sum, $end, $first) = (0, 0, 0);
      for my $sid (@order) { my $q = $p{$sid}; $q->{dur} //= 0; $sum += $q->{dur}; $first ||= $q->{t0};
        $end = $q->{t0} + $q->{dur} if $q->{t0} + $q->{dur} > $end }
      printf "lazygit.run%d git_processes=%d git_cpu_wall_sum_ms=%.0f launch_to_first_git_ms=%.0f launch_to_last_git_exit_ms=%.0f\n",
        $run, scalar @order, $sum * 1000, @order ? ($first - $launch) * 1000 : 0, @order ? ($end - $launch) * 1000 : 0;
      for my $sid (@order) { my $q = $p{$sid}; my @a = @{ $q->{argv} || [] }; shift @a; my @keep; my $sub;
        while (@a) { my $x = shift @a;
          if (!$sub && $x eq "-C") { shift @a; next }
          if (!$sub && $x eq "-c") { push @keep, "-c", shift @a; next }
          if ($x =~ /^-/) { $x =~ s{=[/~].*}{=<path>}; push @keep, $x; next }
          if (!$sub) { $sub = $x; push @keep, $x; next }
        }
        printf "lazygit.run%d.git start_ms=%.0f dur_ms=%.1f cmd=%s\n", $run, ($q->{t0} - $launch) * 1000, $q->{dur} * 1000, join " ", @keep;
      }
    ' "$tr" "$launch" "$run"
  done
  rm -rf "$LGTMP"
fi

# ------------------------------------------------------------ write timings
if [ "$WITH_WRITES" = 1 ]; then
  section timings_writes
  TMPROOT=$(mktemp -d)
  WT="$TMPROOT/wt"
  ORIG=$(git rev-parse HEAD)
  cleanup() { git worktree remove --force "$WT" >/dev/null 2>&1; rm -rf "$TMPROOT"; git worktree prune >/dev/null 2>&1; }
  trap cleanup EXIT INT TERM
  G="git -C $WT -c core.hooksPath=/dev/null -c commit.gpgsign=false -c user.name=probe -c user.email=probe@invalid"

  tm worktree_add 1 git worktree add --detach "$WT" HEAD
  if [ -d "$WT" ]; then
    tm wt.status_first 1 git -C "$WT" status --porcelain=v2 --branch
    for target in "HEAD~1" "HEAD~100" "$MAXSHA"; do
      [ -n "$target" ] || continue
      sha=$(git rev-parse -q --verify "$target^{commit}" 2>/dev/null) || continue
      label=$( [ "$target" = "$MAXSHA" ] && echo most_divergent_branch || echo "$target" | tr 'A-Z' 'a-z')
      kv "wt.checkout.$label.files" "$(git diff --name-only "$ORIG" "$sha" | count)"
      tm "wt.checkout.$label.there" 1 $G checkout -q --detach "$sha"
      tm "wt.status_after_checkout.$label" 1 git -C "$WT" status --porcelain=v2 --branch
      tm "wt.checkout.$label.back" 1 $G checkout -q --detach "$ORIG"
    done
    for n in 1 2 3; do
      echo "probe $n" > "$WT/lazyergit-probe-$n.txt"
      tm "wt.add.$n" 1 $G add "lazyergit-probe-$n.txt"
      tm "wt.commit.$n" 1 $G commit -q -m "lazyergit probe $n"
    done
    echo "amend" >> "$WT/lazyergit-probe-3.txt"
    $G add lazyergit-probe-3.txt
    tm wt.commit_amend_head 1 $G commit -q --amend --no-edit
    echo "fixup" >> "$WT/lazyergit-probe-1.txt"
    $G add lazyergit-probe-1.txt
    tm wt.commit_fixup 1 $G commit -q --fixup=HEAD~2
    tm wt.rebase_autosquash_3_commits 1 env GIT_SEQUENCE_EDITOR=true $G rebase -q -i --autosquash HEAD~4
    kv wt.rebase_result_commits "$(git -C "$WT" rev-list --count "$ORIG..HEAD")"
    tm wt.reset_hard_orig 1 $G reset -q --hard "$ORIG"
    tm worktree_remove 1 git worktree remove --force "$WT"
  fi
fi

echo
echo "# done"
