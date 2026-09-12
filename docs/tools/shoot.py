#!/usr/bin/env python3
"""Capture every screenshot used by the README.

    python3 docs/tools/shoot.py            # all scenes
    python3 docs/tools/shoot.py solo runs  # just these

Everything runs against `mantra --demo` (the simulated backend — no API calls, no
cost), in a throwaway `$MANTRA_HOME`, so the frames are reproducible and contain
nothing personal. Requires a debug build: `cd mantra_src && cargo build`.
"""

from __future__ import annotations

import os
import shutil
import sys
import subprocess
import tempfile

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from termshot import Term, shoot  # noqa: E402

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
BIN = os.path.join(ROOT, "mantra_src", "target", "debug", "mantra")
OUT = os.path.join(ROOT, "docs", "img")
SPEED = "2"


def home() -> str:
    """A throwaway $HOME holding a small git project, so the frames show friendly
    paths (`~/code/todo-api`, `~/.mantra/models.toml`) instead of temp directories."""
    d = tempfile.mkdtemp(prefix="mantra-shot-")
    proj = os.path.join(d, "code", "todo-api")
    os.makedirs(os.path.join(proj, "src"), exist_ok=True)
    open(os.path.join(proj, "Cargo.toml"), "w").write(
        '[package]\nname = "todo-api"\nversion = "0.1.0"\nedition = "2021"\n')
    open(os.path.join(proj, "src", "lib.rs"), "w").write(
        "pub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n")
    open(os.path.join(proj, "README.md"), "w").write("# todo-api\n\nA small service.\n")
    for cmd in (["git", "init", "-q"], ["git", "add", "-A"],
                ["git", "-c", "user.name=You", "-c", "user.email=you@example.com",
                 "commit", "-qm", "initial"]):
        subprocess.run(cmd, cwd=proj, check=False,
                       stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    return d


def start(cols=110, rows=34, args="--demo", extra_env=None, mhome=None):
    h = mhome or home()
    env = {"HOME": h, "MANTRA_HOME": os.path.join(h, ".mantra"),
           "MANTRA_DEMO_PROJECT": os.path.join(h, "code", "todo-api"),
           "MANTRA_MOCK_SPEED": SPEED, "COLORTERM": "truecolor",
           "TERM": "tmux-256color", "LANG": "en_US.UTF-8"}
    env.update(extra_env or {})
    t = Term(f"{BIN} {args}", cols=cols, rows=rows, env=env)
    t.sleep(1.2)
    return t


# ── scenes ──────────────────────────────────────────────────────────────────────
def solo():
    """Solo mode mid-answer: streamed reply, a command, the changes panel."""
    t = start(cols=108, rows=32)
    t.type("add a greet helper to the library and test it")
    t.send("Enter")
    if t.wait("Run this command", 60):
        shoot(t, f"{OUT}/solo-approval.png", "mantra — solo · approval")
        t.send("y")
    t.wait("Want me to wire", 60)
    t.sleep(1.0)
    shoot(t, f"{OUT}/solo.png", "mantra — solo")
    t.kill()


def mandala():
    """The hero shot: phase 2 in flight with three parallel workers, then a zoomed agent."""
    t = start(cols=118, rows=36)
    t.send("C-o")
    t.type("build a todo API with auth")
    t.send("Enter")
    if t.wait("plan review", 90):
        shoot(t, f"{OUT}/plan-review.png", "mantra — plan review")
        t.send("a")
    # phase 2 runs three workers in parallel — the view worth showing
    t.wait("phase 2/3", 150)
    t.sleep(2.5)
    shoot(t, f"{OUT}/mandala.png", "mantra — mandala · a phase in flight")
    # zoom into a worker (the stage is in navigating mode right after approval)
    t.send("3")
    t.sleep(2.0)
    shoot(t, f"{OUT}/zoom.png", "mantra — zoomed into one agent")
    t.send("Escape")
    t.wait("run complete", 240)
    t.sleep(0.8)
    shoot(t, f"{OUT}/done.png", "mantra — run complete")
    t.kill()


def halt():
    """A typed halt: the amber band with the reason and the key that fixes it."""
    t = start(cols=130, rows=30, args='--demo run "trigger a bad model"',
              extra_env={"MANTRA_MOCK_BADMODEL": "1"})
    if t.wait("plan review", 90):
        t.send("a")
    t.wait("halted", 90)
    t.sleep(0.8)
    shoot(t, f"{OUT}/halt.png", "mantra — a halted run")
    t.kill()


def runs():
    """/runs: one finished run and one left mid-phase, listed in a later session."""
    h = home()
    # run 1: all the way through, so the list has a finished run to show
    t = start(cols=108, rows=30, args='--demo run "add rate limiting to the API"', mhome=h)
    if t.wait("plan review", 90):
        t.send("a")
    t.wait("run complete", 240)
    t.kill()
    # run 2: approved, then the session goes away mid-phase
    t = start(cols=108, rows=30, args='--demo run "build a todo API with auth"', mhome=h)
    if t.wait("plan review", 90):
        t.send("a")
    t.sleep(4.0)  # far enough into phase 1 that the run is genuinely unfinished
    t.kill()
    # a later session: the welcome screen says there is something to pick up
    t = start(cols=108, rows=30, mhome=h)
    t.send("C-o")
    t.sleep(0.8)
    shoot(t, f"{OUT}/welcome-unfinished.png", "mantra — unfinished run")
    t.type("/runs")
    t.send("Enter")
    t.sleep(0.8)
    shoot(t, f"{OUT}/runs.png", "mantra — /runs")
    t.kill()
    shutil.rmtree(h, ignore_errors=True)


def studio():
    """The Studio: roles, models, per-role permission, flow preview."""
    t = start(cols=112, rows=32)
    t.send("C-o")
    t.sleep(0.5)
    t.type("/studio")
    t.send("Enter")
    t.sleep(1.0)
    shoot(t, f"{OUT}/studio.png", "mantra — pattern studio")
    t.kill()


def models():
    """/models: aliases, providers, context windows, compaction."""
    t = start(cols=136, rows=30)
    t.type("/models")
    t.send("Enter")
    t.sleep(1.0)
    shoot(t, f"{OUT}/models.png", "mantra — models & providers")
    t.send("Escape")
    t.sleep(0.4)
    t.send("C-k")
    t.sleep(0.8)
    shoot(t, f"{OUT}/picker.png", "mantra — model picker (ctrl+k)")
    t.kill()


SCENES = {"solo": solo, "mandala": mandala, "halt": halt, "runs": runs,
          "studio": studio, "models": models}

if __name__ == "__main__":
    # A tmux server started earlier keeps its own global environment, which new sessions
    # inherit; start from a clean one so nothing from the shell leaks into a frame.
    subprocess.run(["tmux", "kill-server"], check=False,
                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    if not os.path.exists(BIN):
        sys.exit(f"build first: cd mantra_src && cargo build   (missing {BIN})")
    os.makedirs(OUT, exist_ok=True)
    names = sys.argv[1:] or list(SCENES)
    for n in names:
        if n not in SCENES:
            sys.exit(f"unknown scene {n!r}; known: {', '.join(SCENES)}")
        print(f"● {n}")
        SCENES[n]()
    print("done")
