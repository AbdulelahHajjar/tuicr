#!/usr/bin/env python3
"""Headless smoke test for tuicr's Local forge.

Drives a tuicr binary through a local pull-request review inside a pty, in a
scratch HOME (so the real session/local-forge stores are untouched), and
checks the persisted results with `tuicr review …` and the store files.

    smoke-local-pr.py [--tuicr /path/to/tuicr] [--keep]

Scenario (mirrors docs/LOCAL_FORGE.md "Judged by"):
  1. temp repo: develop with a base commit; feature branch with two commits
  2. `tuicr pr` on the feature branch opens local PR #1 (session slug local:…)
  3. add a line comment                        -> threads.json on save (no review record);
     `:submit approve`                         -> reviews.json with the verdict
  4. amend the branch tip in another process   -> auto-follow reloads at the new head
  5. thread still listed (re-anchored)         -> `:resolve`
  6. `:comments unresolved` hides it, `:comments all` shows it
  7. plain `tuicr` -> Pull Requests tab -> `l` -> local branch row -> Enter opens local PR #1
  8. `tuicr review list --repo <repo>` lists the local PR session
Exit status 0 when every check passes; each failed check is printed.
"""
import argparse
import fcntl
import json
import os
import pty
import re
import select
import shutil
import signal
import struct
import subprocess
import sys
import tempfile
import termios
import time

FEATURE = "feature/smoke-local-pr"
TARGET = "src/lib.rs"


def sh(cwd, *args, env=None, check=True):
    result = subprocess.run(list(args), cwd=cwd, env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    if check and result.returncode != 0:
        raise SystemExit("command failed: %s\n%s" % (" ".join(args), result.stderr))
    return result.stdout.strip()


def make_repo(root):
    sh(root, "git", "init", "-q", "-b", "develop")
    sh(root, "git", "config", "user.name", "Smoke Tester")
    sh(root, "git", "config", "user.email", "smoke@example.invalid")
    sh(root, "git", "remote", "add", "origin", "https://github.com/smoke-org/smoke-repo")
    os.makedirs(os.path.join(root, "src"), exist_ok=True)
    with open(os.path.join(root, TARGET), "w") as handle:
        handle.write("fn one() {}\nfn two() {}\nfn three() {}\n")
    sh(root, "git", "add", "-A")
    sh(root, "git", "commit", "-q", "-m", "base")
    sh(root, "git", "checkout", "-q", "-b", FEATURE)
    with open(os.path.join(root, TARGET), "w") as handle:
        handle.write("fn one() {}\nfn two() { changed(); }\nfn three() {}\n")
    sh(root, "git", "commit", "-q", "-am", "feat: change two")
    with open(os.path.join(root, "README.md"), "w") as handle:
        handle.write("smoke\n")
    sh(root, "git", "add", "-A")
    sh(root, "git", "commit", "-q", "-m", "docs: add readme")


ANSI = re.compile(r"\x1b\[[0-9;?]*[A-Za-z]|\x1b\][^\x07]*\x07|\x1b[=>]")


def screen_text(raw):
    return ANSI.sub("", raw.decode(errors="replace"))


def load_threads(path):
    if not os.path.exists(path):
        return []
    return json.load(open(path)).get("threads", [])


class Tui:
    def __init__(self, binary, cwd, env, args):
        self.pid, self.fd = pty.fork()
        if self.pid == 0:
            os.chdir(cwd)
            os.execvpe(binary, ["tuicr"] + args, env)
        fcntl.ioctl(self.fd, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 160, 0, 0))
        self.output = b""

    def pump(self, seconds):
        end = time.time() + seconds
        while time.time() < end:
            ready, _, _ = select.select([self.fd], [], [], 0.2)
            if ready:
                try:
                    self.output += os.read(self.fd, 65536)
                except OSError:
                    return

    def keys(self, text, settle=1.0):
        os.write(self.fd, text.encode())
        self.pump(settle)

    def stop(self):
        try:
            os.kill(self.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        os.waitpid(self.pid, 0)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--tuicr", default=shutil.which("tuicr"))
    parser.add_argument("--keep", action="store_true")
    options = parser.parse_args()
    if not options.tuicr:
        raise SystemExit("no tuicr binary found")
    options.tuicr = os.path.abspath(options.tuicr)

    work = tempfile.mkdtemp(prefix="tuicr-smoke-")
    home = os.path.join(work, "home")
    repo = os.path.join(work, "repo")
    os.makedirs(home)
    os.makedirs(repo)
    env = dict(os.environ, HOME=home, TERM="xterm-256color", XDG_CONFIG_HOME=os.path.join(home, ".config"))
    env.pop("TUICR_PROFILE", None)
    failures = []

    def check(condition, label):
        print(("  ok   " if condition else "  FAIL ") + label)
        if not condition:
            failures.append(label)

    try:
        make_repo(repo)
        print("== version"); version = sh(repo, options.tuicr, "--version", env=env)
        print("  " + version)
        check("+local-forge" in version, "fork build marker in --version")

        print("== open local PR")
        tui = Tui(options.tuicr, repo, env, ["pr", "--no-update-check"])
        tui.pump(6)
        announce = [l for l in screen_text(tui.output).splitlines() if "tuicr-session:" in l]
        check(any("local:smoke-org/smoke-repo/pr/1" in l for l in announce), "session slug local:smoke-org/smoke-repo/pr/1 announced")

        print("== line comment opens a thread on save; :submit records the verdict")
        tui.keys("}", 0.5)           # next file (keep the cursor inside the diff)
        tui.keys("]", 0.5)           # next hunk
        tui.keys("j", 0.3)
        tui.keys("c", 0.8)
        tui.keys("please rename changed()", 0.3)
        tui.keys("\x13", 1.5)        # Ctrl-S saves the comment -> thread
        store = os.path.join(home, "Library", "Application Support", "tuicr", "local-forge", "smoke-org__smoke-repo", "pulls", "1")
        threads_path = os.path.join(store, "threads.json")
        reviews_path = os.path.join(store, "reviews.json")
        threads = load_threads(threads_path)
        check(len(threads) == 1 and threads[0]["comments"][0]["body"] == "please rename changed()", "one thread written when the comment is saved")
        check(bool(threads) and threads[0].get("review_id") is None, "direct thread carries no review_id")
        check(not os.path.exists(reviews_path), "no review record minted for the saved comment")
        tui.keys(":submit approve\r", 2.0)
        tui.keys("\r", 2.0)          # confirm modal, if shown
        reviews = json.load(open(reviews_path)).get("reviews", []) if os.path.exists(reviews_path) else []
        check(len(reviews) == 1 and reviews[0]["event"] == "APPROVE", "reviews.json written by :submit approve")
        check(len(load_threads(threads_path)) == 1, "the verdict left the thread alone")

        print("== amend the branch tip while tuicr is open (auto-follow)")
        head_before = sh(repo, "git", "rev-parse", "HEAD")
        with open(os.path.join(repo, "README.md"), "a") as handle:
            handle.write("more\n")
        sh(repo, "git", "commit", "-q", "-a", "--amend", "--no-edit")
        head_after = sh(repo, "git", "rev-parse", "HEAD")
        pulls_path = os.path.join(home, "Library", "Application Support", "tuicr", "local-forge", "smoke-org__smoke-repo", "pulls.json")
        followed = False
        for _ in range(16):                      # auto-follow polls every 1s; reload refreshes last_head_sha
            tui.pump(0.5)
            pulls = json.load(open(pulls_path)).get("pulls", [])
            if pulls and pulls[0].get("last_head_sha") == head_after:
                followed = True
                break
        check(head_before != head_after and followed, "auto-follow reloaded at the new head (last_head_sha advanced)")
        tui.pump(2)

        print("== resolve")
        tui.keys(":comments all\r", 1.0)
        tui.keys("gg", 0.5)
        tui.keys("}", 0.5)
        tui.keys("]", 0.5)
        tui.keys("j", 0.3)
        tui.keys(":resolve\r", 1.5)
        threads = load_threads(threads_path)
        check(bool(threads) and threads[0]["is_resolved"] is True, ":resolve persisted is_resolved")
        # leave an unsubmitted file-level draft (line comments are threads now), save, quit;
        # commit; relaunch -> the draft must carry to the new head
        tui.keys("gg", 0.5)
        tui.keys("}", 0.5)
        tui.keys("]", 0.5)
        tui.keys("j", 0.3)
        tui.keys("C", 0.8)
        tui.keys("second draft", 0.3)
        tui.keys("\x13", 1.0)
        tui.keys(":w\r", 1.5)
        sessions_dir = os.path.join(home, "Library", "Application Support", "tuicr", "reviews", "sessions")
        head_now = sh(repo, "git", "rev-parse", "HEAD")
        saved = any(
            "second draft" in [c["content"] for review in data["files"].values() for c in review["file_comments"]]
            for data in (json.load(open(os.path.join(sessions_dir, name))) for name in os.listdir(sessions_dir))
            if (data.get("pr_session_key") or {}).get("head_sha") == head_now
        )
        check(saved, "unsubmitted draft saved in the current-head session before quitting")
        tui.keys(":q\r", 1.5)
        tui.stop()

        print("== relaunch after a new commit (startup carry-forward)")
        with open(os.path.join(repo, "README.md"), "a") as handle:
            handle.write("third\n")
        sh(repo, "git", "commit", "-q", "-am", "docs: third")
        head_third = sh(repo, "git", "rev-parse", "HEAD")
        tui = Tui(options.tuicr, repo, env, ["pr", "--no-update-check"])
        tui.pump(6)
        tui.keys(":w\r", 1.5)
        carried = False
        for path in os.listdir(sessions_dir):
            data = json.load(open(os.path.join(sessions_dir, path)))
            key = data.get("pr_session_key") or {}
            if key.get("head_sha") != head_third:
                continue
            bodies = [c["content"] for review in data["files"].values() for c in review["file_comments"]]
            carried = "second draft" in bodies
        check(carried, "relaunch at the new head carried the unsubmitted draft")
        tui.keys(":q!\r", 1.0)
        tui.stop()

        print("== Pull Requests tab: l switches to local branches, Enter opens one")
        tui = Tui(options.tuicr, repo, env, ["--no-update-check"])   # commit selector
        tui.pump(4)
        tui.keys("\t", 3.0)          # Pull Requests tab (forge source loads first and fails offline)
        tui.keys("l", 3.0)            # local source
        tab_text = screen_text(tui.output)
        # ratatui redraws only changed cells, so look for the Local-only footer hint and the branch row
        check("l forge" in tab_text and FEATURE in tab_text, "PR tab lists the local branch under the Local source")
        tui.keys("\r", 2.0)          # open the highlighted local pull request (background fetch)
        opened = False
        for _ in range(20):
            if "PR #1" in screen_text(tui.output):
                opened = True
                break
            tui.pump(1.0)
        check(opened, "opening the local row enters PR mode for local pull request #1")
        tui.keys(":q!\r", 1.0)
        tui.stop()

        print("== tuicr review list")
        listing = sh(repo, options.tuicr, "review", "list", "--repo", repo, env=env)
        sessions = json.loads(listing) if listing else []
        local_prs = [s for s in sessions if s.get("slug", "").startswith("local:smoke-org/smoke-repo/pr/1")]
        check(len(local_prs) >= 1 and all(s.get("kind") == "pr" for s in local_prs), "review list shows the local PR session as kind pr")
        comments = sh(repo, options.tuicr, "review", "comments", "--session", "local:smoke-org/smoke-repo/pr/1", "--repo", repo, env=env, check=False)
        check(comments.startswith("["), "review comments --session local:… resolves")
    finally:
        try:
            with open(os.path.join(work, "tui-output.txt"), "wb") as handle:
                handle.write(tui.output)
        except NameError:
            pass
        if options.keep:
            print("kept " + work)
        else:
            shutil.rmtree(work, ignore_errors=True)

    if failures:
        print("\n%d check(s) failed" % len(failures))
        sys.exit(1)
    print("\nall checks passed")


if __name__ == "__main__":
    main()
