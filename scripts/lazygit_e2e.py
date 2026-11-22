#!/usr/bin/env python3
"""End-to-end latency of lazygit on a benchmark fixture.

For each scenario the fixture is brought into the scenario's start state
(`lazyer-bench prepare`), lazygit is started in a pseudo-terminal, the keys of
the operation are sent, and the time is taken from the confirming key until
lazygit is idle again: no git child process running and no terminal output
for a short while. That is what a person waits for.

    python3 scripts/lazygit_e2e.py [--profile work] [--iters 3] [--filter text]

Needs target/release/lazyer-bench (cargo build --release -p lazyer-bench).
"""
import argparse
import fcntl
import json
import os
import pty
import select
import signal
import struct
import subprocess
import sys
import tempfile
import termios
import time
from datetime import datetime, timezone

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BENCH = os.path.join(ROOT, "target", "release", "lazyer-bench")
QUIET = 0.35  # seconds without output or git activity that count as idle
LAZYGIT_BIN = "lazygit"

CONFIG = """git:
  autoFetch: false
  autoRefresh: false
update:
  method: never
disableStartupPopups: true
gui:
  showRandomTip: false
  showFileTree: false
"""


def trace_time(s):
    return datetime.strptime(s, "%Y-%m-%dT%H:%M:%S.%fZ").replace(tzinfo=timezone.utc).timestamp()


class Lazygit:
    def __init__(self, cwd, real_git_dir=None):
        self.tmp = tempfile.mkdtemp(prefix="lazyer-e2e-")
        self.trace = os.path.join(self.tmp, "trace.json")
        open(self.trace, "w").close()
        cfg = os.path.join(self.tmp, "config.yml")
        with open(cfg, "w") as f:
            f.write(CONFIG)
        env = dict(os.environ)
        env.update(GIT_TRACE2_EVENT=self.trace, LG_CONFIG_FILE=cfg, TERM="xterm-256color",
                   GIT_CONFIG_GLOBAL="/dev/null", GIT_CONFIG_NOSYSTEM="1")
        if real_git_dir:
            env["PATH"] = real_git_dir + os.pathsep + env["PATH"]
        self.launched = time.time()
        self.pid, self.fd = pty.fork()
        if self.pid == 0:
            fcntl.ioctl(0, termios.TIOCSWINSZ, struct.pack("HHHH", 50, 160, 0, 0))
            os.chdir(cwd)
            os.execvpe(LAZYGIT_BIN, [LAZYGIT_BIN], env)
        self.trace_pos = 0
        self.trace_buf = b""
        self.running = set()
        self.last_activity = self.launched
        self.git_count = 0
        self.git_ms = 0.0
        self.screen = b""
        self.cpu_seen = {}
        self.out_bytes = 0

    def pump(self, timeout=0.02):
        r, _, _ = select.select([self.fd], [], [], timeout)
        if r:
            try:
                data = os.read(self.fd, 1 << 16)
            except OSError:
                data = b""
            if data:
                self.last_activity = time.time()
                self.out_bytes += len(data)
                self.screen = (self.screen + data)[-200000:]
                if b"\x1b]11;?" in data:
                    os.write(self.fd, b"\x1b]11;rgb:0000/0000/0000\x1b\\")
                if b"\x1b[c" in data:
                    os.write(self.fd, b"\x1b[?62;c")
                if b"\x1b[6n" in data:
                    os.write(self.fd, b"\x1b[1;1R")
        with open(self.trace, "rb") as f:
            f.seek(self.trace_pos)
            chunk = f.read()
        self.trace_pos += len(chunk)
        self.trace_buf += chunk
        *lines, self.trace_buf = self.trace_buf.split(b"\n")
        for line in lines:
            try:
                ev = json.loads(line)
            except ValueError:
                continue
            sid = ev.get("sid", "")
            if "/" in sid:
                continue
            if ev.get("event") == "start":
                self.running.add(sid)
                self.git_count += 1
                self.last_activity = max(self.last_activity, trace_time(ev["time"]))
            elif ev.get("event") == "atexit":
                self.running.discard(sid)
                self.git_ms += ev.get("t_abs", 0) * 1000
                self.last_activity = max(self.last_activity, trace_time(ev["time"]))

    def settle(self, max_wait=120.0, after_bytes=-1):
        """Wait until idle; returns the time of the last activity.

        Idle is only declared once the terminal has shown something beyond
        `after_bytes` (the output counter when the key was sent): work done
        inside the process, without a git child, is otherwise invisible. If
        nothing appears for a few seconds, the action had no visible effect.
        """
        started = time.time()
        deadline = started + max_wait
        while time.time() < deadline:
            self.pump()
            if self.out_bytes <= after_bytes and time.time() - started < 4.0:
                continue
            if self.running and time.time() - self.last_activity > QUIET:
                self.forget_dead()
            if not self.running and time.time() - self.last_activity > QUIET:
                return self.last_activity
        raise TimeoutError("lazygit did not become idle")

    def forget_dead(self):
        """Drop git processes that will not report an exit: killed ones
        (lazygit cancels commands whose result it no longer needs) and ones
        that sit blocked on a full pipe (lazygit reads the log for its main
        view only as far as it displays it). The process id is in the session
        id (`...-P<hex pid>`); blocked means its CPU time stopped moving."""
        for sid in list(self.running):
            try:
                pid = int(sid.rsplit("-P", 1)[1], 16)
                os.kill(pid, 0)
            except (OSError, ValueError, IndexError):
                self.running.discard(sid)
                continue
            cpu = subprocess.run(["ps", "-o", "cputime=", "-p", str(pid)], capture_output=True, text=True).stdout.strip()
            seen = self.cpu_seen.get(sid)
            if seen is None or seen[0] != cpu:
                self.cpu_seen[sid] = (cpu, time.time())
            elif time.time() - seen[1] > QUIET:
                self.running.discard(sid)

    def keys(self, data, pause=0.15):
        """Send keys that only move the UI, then wait for it."""
        before = self.out_bytes
        os.write(self.fd, data.encode())
        time.sleep(pause)
        self.settle(after_bytes=before)

    def measure(self, data):
        """Send the confirming key; milliseconds until idle, git processes, git time."""
        self.settle()
        count0, ms0, before = self.git_count, self.git_ms, self.out_bytes
        t0 = time.time()
        self.last_activity = t0
        os.write(self.fd, data.encode())
        done = self.settle(after_bytes=before)
        return (done - t0) * 1000, self.git_count - count0, self.git_ms - ms0

    def text(self):
        """Roughly de-escaped recent output, for debugging key sequences."""
        import re
        s = self.screen.decode("utf-8", "replace")
        s = re.sub(r"\x1b\[[0-9;?]*[A-Za-z]|\x1b\][^\x07\x1b]*(\x07|\x1b\\)|\x1b[=>()][0-9A-B]?", " ", s)
        return re.sub(r"[ \t]+", " ", s)

    def close(self):
        try:
            os.write(self.fd, b"\x1bq")
            time.sleep(0.2)
            os.kill(self.pid, signal.SIGKILL)
        except OSError:
            pass
        # Close our end first: on macOS a process cannot finish exiting while
        # output it wrote to the terminal is still unread.
        try:
            os.close(self.fd)
        except OSError:
            pass
        try:
            os.waitpid(self.pid, 0)
        except OSError:
            pass


def bench(*args):
    out = subprocess.run([BENCH, *args], cwd=ROOT, check=True, capture_output=True, text=True).stdout
    return [l for l in out.splitlines() if l]


def git(cwd, *args):
    return subprocess.run(["git", *args], cwd=cwd, check=True, capture_output=True, text=True).stdout.strip()


# Each driver gets a started, idle lazygit and the scenario's arguments and
# returns the measurement of the confirming key.
def drive_startup(lg, args):
    return None  # the startup time itself is the measurement


def drive_switch(lg, args):
    lg.keys("3")        # branches panel
    lg.keys("c")        # check out by name
    lg.keys(args[0])
    return lg.measure("\r")


def drive_commit(lg, args):
    lg.keys("2")        # files panel
    lg.keys("c")        # commit
    lg.keys("bench commit")
    return lg.measure("\r")


def drive_stage_file(lg, args):
    lg.keys("2")        # files panel
    return lg.measure(" ")  # stage (or unstage) the selected entry


def drive_stage_all(lg, args):
    lg.keys("2")
    return lg.measure("a")  # stage all


def drive_stage_line(lg, args):
    lg.keys("2")        # files panel; the changed file is the first entry
    lg.keys("\r")       # open it in the staging view
    return lg.measure(" ")  # stage the selected line


def drive_move_line_out(lg, args):
    lg.keys("4")        # commits panel
    lg.keys("jjj")      # the commit 3 below HEAD
    lg.keys("\r")       # its files
    lg.keys("\r")       # first file: patch building view
    lg.keys(" ")        # add the selected line to the custom patch
    lg.keys("\x10")     # ctrl+p: patch options
    return lg.measure("i")  # move patch out into index


def drive_amend_head(lg, args):
    lg.keys("2")
    lg.keys("A")        # amend last commit, asks for confirmation
    return lg.measure("\r")


def drive_amend_older(depth):
    def drive(lg, args):
        lg.keys("4")    # commits panel
        lg.keys("j" * depth)
        lg.keys("A")    # amend commit with staged changes, asks for confirmation
        return lg.measure("\r")
    return drive


SCENARIOS = [
    # name, fixture scenario to prepare, driver
    ("startup_main", "status_clean", drive_startup),
    ("startup_other_worktree", "view_other_worktree", drive_startup),
    ("switch_near", "switch_near", drive_switch),
    ("switch_far", "switch_far", drive_switch),
    ("stage_file", "status_dirty", drive_stage_file),
    ("stage_all", "status_dirty", drive_stage_all),
    ("stage_line", "status_two_hunks", drive_stage_line),
    ("move_line_out_of_commit", "move_patch_d3", drive_move_line_out),
    ("commit_staged", "commit_staged", drive_commit),
    ("amend_head", "amend_head", drive_amend_head),
    ("amend_older_d3", "amend_older_d3", drive_amend_older(3)),
    ("amend_older_d20", "amend_older_d20", drive_amend_older(20)),
]


def state_summary(wt):
    """What an operation leaves behind, without anything time-dependent:
    the trees and subjects of recent commits, the index, and the status."""
    return "\n".join([
        git(wt, "log", "--format=%T %s", "-12"),
        git(wt, "ls-files", "-s"),
        git(wt, "status", "--porcelain"),
        git(wt, "diff", "--cached"),
    ])


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--profile", default="work")
    ap.add_argument("--iters", type=int, default=3)
    ap.add_argument("--filter", default="")
    ap.add_argument("--shim", action="store_true", help="let lazygit use /usr/bin/git instead of the real binary")
    ap.add_argument("--debug", action="store_true", help="print the terminal text after each run")
    ap.add_argument("--summary-dir", default="", help="write the repository state after each scenario there, to compare two binaries")
    ap.add_argument("--bin", default="lazygit", help="the lazygit binary to drive, e.g. target/lazygit-lazyer")
    a = ap.parse_args()
    global LAZYGIT_BIN
    LAZYGIT_BIN = os.path.abspath(a.bin) if os.sep in a.bin else a.bin

    real_git_dir = None
    if not a.shim:
        try:
            real_git_dir = os.path.dirname(subprocess.run(["xcrun", "-f", "git"], capture_output=True, text=True, check=True).stdout.strip())
        except (OSError, subprocess.CalledProcessError):
            pass

    print(f"binary: {a.bin}   profile: {a.profile}   iterations: {a.iters}   git: {'shim' if a.shim else 'real binary'}")
    print("ms = confirming key until lazygit is idle; for startup: launch until idle\n")
    print(f"| {'scenario':<24} | {'ms':>8} | {'git procs':>9} | {'git ms sum':>10} | {'took effect':>10} |")
    print(f"|{'-'*26}|{'-'*10}|{'-'*11}|{'-'*12}|{'-'*12}|")
    for name, prep, drive in SCENARIOS:
        if a.filter and a.filter not in name:
            continue
        rows = []
        effect = "-"
        for _ in range(a.iters):
            bench("restore", "--profile", a.profile)
            wt, *args = bench("prepare", prep, "--profile", a.profile)
            time.sleep(1.1)
            git(wt, "update-index", "-q", "--refresh", "--force-write-index")
            before = git(wt, "rev-parse", "HEAD") + git(wt, "symbolic-ref", "-q", "HEAD") + git(wt, "status", "--porcelain")
            lg = Lazygit(wt, real_git_dir)
            try:
                idle = lg.settle(after_bytes=0)
                if drive is drive_startup:
                    rows.append(((idle - lg.launched) * 1000, lg.git_count, lg.git_ms))
                else:
                    rows.append(drive(lg, args))
                    after = git(wt, "rev-parse", "HEAD") + git(wt, "symbolic-ref", "-q", "HEAD") + git(wt, "status", "--porcelain")
                    effect = "yes" if after != before else "NO"
                if a.summary_dir:
                    os.makedirs(a.summary_dir, exist_ok=True)
                    with open(os.path.join(a.summary_dir, name + ".state"), "w") as f:
                        f.write(state_summary(wt))
                if a.debug:
                    print(lg.text()[-3000:], file=sys.stderr)
            finally:
                lg.close()
        rows.sort()
        ms, procs, gitms = rows[len(rows) // 2]
        print(f"| {name:<24} | {ms:>8.0f} | {procs:>9} | {gitms:>10.0f} | {effect:>10} |", flush=True)
    bench("restore", "--profile", a.profile)


if __name__ == "__main__":
    main()
