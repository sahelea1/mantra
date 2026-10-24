#!/usr/bin/env bash
# End-to-end check of MCP servers and the screen tools: two whole Mandala runs in `--demo`, one on
# the real `bija` binary (mantra-default), one on the Claude Code mock (a pattern file of the run's
# own, `on-claude`, written below), with the demo's stand-in MCP server (`mantra mock-mcp`, registered as `demo` for every role) and
# a stand-in screen capture. No API calls, no key, no cost, no real screen. A sibling of
# bija-e2e.sh, which checks the plain bija run.
#
#   docs/tools/mcp-e2e.sh                    # release build, then both runs, then PASS/FAIL
#   MCP_E2E_SPEED=2 docs/tools/mcp-e2e.sh    # slower mock (the default is 8× demo speed)
#   MCP_E2E_KEEP=1 docs/tools/mcp-e2e.sh     # keep the temp dir (both MANTRA_HOMEs, the demo repos)
#
# The capture: `MANTRA_SCREENSHOT_CMD` names a script that copies a PNG made here to `{out}`, so
# `mantra_screenshot` works on a machine with no display and no capture tool. The mock models call
# it only when that variable is set — a plain `--demo` never takes a screenshot.
#
# What it proves, and how:
#   bija run
#   - the run reaches Done                    `"stage": "Done"` in the run's state.json
#   - bija's MCP client called the server     a bija transcript holds an mcp__demo__scene_info
#                                              call, and the scene's text came back ("3 objects")
#   - a screenshot was taken and delivered    a PNG under the run's screenshots/, and an image file
#                                              in some bija session's images/
#   Claude Code run
#   - the run reaches Done                    as above
#   - the mock spawned the server and called  mantra.log holds the mock's mcp__demo__scene_info
#     it, and Mantra read the call             tool_use and the scene in its tool_result, as
#                                              `claude <id>: …` lines (every stream line Mantra
#                                              reads from an agent is logged that way)
#   - a screenshot was taken through the      a PNG under the run's screenshots/, the mock's
#     bridge and came back as an image         mcp__mantra__mantra_screenshot tool_use, and its
#                                              tool_result with an image block (the mock puts the
#                                              image first, so the log's 200-character line has it)
#   both: nothing crashed                     no panic, no snapshot timeout
#
# This script is written from the contract of the MCP and vision work (Mantra 0.7.0) and passes
# once all of it is in: `mantra mock-mcp` and `--demo`'s `demo` server, bija's `--mcp-file`, the
# Claude Code backend's servers in `--mcp-config`, and `mantra_screenshot` for image-taking models.
set -euo pipefail
cd "$(dirname "$0")/../../mantra_src"

target="${CARGO_TARGET_DIR:-$PWD/target}"
echo "building the workspace (release: mantra + bija)…"
cargo build --release -q
bin="$target/release/mantra"
for b in mantra bija; do
  [ -x "$target/release/$b" ] || { echo "FAIL: $target/release/$b was not built"; exit 1; }
done

tmp="$(mktemp -d)"
if [ -z "${MCP_E2E_KEEP:-}" ]; then
  trap 'rm -rf "$tmp"' EXIT
else
  echo "keeping $tmp"
fi

# The "screen": a 1×1 PNG, and a capture command that copies it where it is asked to.
base64 -d >"$tmp/screen.png" <<<'iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg=='
printf '#!/bin/sh\ncp "%s" "$1"\n' "$tmp/screen.png" >"$tmp/shot.sh"
export MANTRA_SCREENSHOT_CMD="sh $tmp/shot.sh {out}"
export MANTRA_MOCK_SPEED="${MCP_E2E_SPEED:-8}" LANG="${LANG:-en_US.UTF-8}"

fails=0
pass() { echo "PASS: $*"; }
fail() { echo "FAIL: $*"; fails=$((fails + 1)); }
outs=""

# run_demo <name> <pattern>: one whole run with a MANTRA_HOME (and, through TMPDIR, a demo repo)
# of its own under $tmp/<name>, so the two runs' logs and sessions never mix. Sets $home, $run_dir.
run_demo() {
  local name="$1" pattern="$2" out code state
  home="$tmp/$name/home"
  mkdir -p "$home"
  echo "running a Mandala with the demo MCP server ($pattern)…"
  local script="wait:0.5;until:plan review@120;key:a;until:run complete@600;snap:done"
  set +e
  out="$(MANTRA_HOME="$home" TMPDIR="$tmp/$name" "$bin" --demo --pattern "$pattern" --snapshot "$script" run "build a tiny todo CLI" 2>&1)"
  code=$?
  set -e
  outs+=$'\n'"── $pattern ──"$'\n'"$(tail -40 <<<"$out")"
  [ "$code" -eq 0 ] && pass "$name: mantra exited 0" || fail "$name: mantra exited $code"
  # Here-strings, not `echo | grep -q`: under pipefail an early match would SIGPIPE the echo and
  # read as no match.
  if grep -qi 'panicked' <<<"$out"; then fail "$name: a panic: $(grep -i -m1 'panicked' <<<"$out")"; else pass "$name: no panic"; fi
  if grep -q '!! timeout' <<<"$out"; then fail "$name: the snapshot timed out: $(grep -m1 '!! timeout' <<<"$out")"; else pass "$name: no snapshot timeout"; fi
  state="$(ls "$home"/runs/*/*/state.json 2>/dev/null | head -1 || true)"
  run_dir=""
  if [ -z "$state" ]; then
    fail "$name: no run state under $home/runs"
    return
  fi
  run_dir="$(dirname "$state")"
  if grep -q '"stage": "Done"' "$state"; then
    pass "$name: the run reached Done ($state)"
  else
    fail "$name: the run did not reach Done: $(grep -m1 '"stage"' "$state" || echo 'no stage')"
  fi
}

# ── bija ────────────────────────────────────────────────────────────────────────────────────
run_demo bija mantra-default
sessions="$home/bija/sessions"
if grep -lq '"mcp__demo__scene_info"' "$sessions"/*/transcript.jsonl 2>/dev/null; then
  pass "bija: a worker called mcp__demo__scene_info through bija's MCP client"
else
  fail "bija: no bija transcript holds an mcp__demo__scene_info call"
fi
if grep -lq '3 objects' "$sessions"/*/transcript.jsonl 2>/dev/null; then
  pass "bija: the server's scene came back into a transcript"
else
  fail "bija: no transcript holds the scene's text (\"3 objects\")"
fi
if [ -n "$run_dir" ] && [ -n "$(ls "$run_dir"/screenshots/*.png 2>/dev/null | head -1)" ]; then
  pass "bija: mantra_screenshot saved $(ls "$run_dir"/screenshots/*.png | wc -l | tr -d ' ') capture(s) under the run's screenshots/"
else
  fail "bija: no PNG under ${run_dir:-the run dir}/screenshots/"
fi
images="$(ls "$sessions"/*/images/* 2>/dev/null | head -1 || true)"
if [ -n "$images" ]; then
  pass "bija: a result's image reached a session ($images)"
else
  fail "bija: no file in any $sessions/*/images/"
fi

# ── Claude Code (the mock) ──────────────────────────────────────────────────────────────────
# No built-in runs on Claude Code, so the run gets a pattern of its own: the default's shape with
# the planner, orchestrator and workers on the demo's Claude Code models, the gate on the default's.
mkdir -p "$tmp/claude/home/patterns"
cat >"$tmp/claude/home/patterns/on-claude.toml" <<'TOML'
name = "on-claude"
description = "planner, orchestrator and workers on Claude Code (mcp-e2e)"

[roles.planner]
kind = "planner"
model = "fable51"
effort = "auto"
sandbox = "read-only"

[roles.orchestrator]
kind = "orchestrator"
model = "fable51"
effort = "auto"
sandbox = "read-only"

[roles.worker-small]
model = "sonnet5"
effort = "auto"

[roles.worker-big]
model = "sonnet5"
effort = "auto"

[roles.qa]
kind = "gate"
model = "terra"
effort = "auto"
sandbox = "read-only"
TOML
run_demo claude on-claude
log="$home/logs/mantra.log"
if grep -Eq 'claude [0-9]+: .*"mcp__demo__scene_info"' "$log" 2>/dev/null; then
  pass "claude: the agent's scene_info call reached Mantra (mantra.log)"
else
  fail "claude: no 'claude <id>: …mcp__demo__scene_info…' line in $log"
fi
# The log keeps a line's first 200 characters; the scene's first line is well inside them.
if grep -Eq 'claude [0-9]+: .*3 objects' "$log" 2>/dev/null; then
  pass "claude: the server answered through the mock (the scene came back)"
else
  fail "claude: no 'claude <id>: …' line with the scene (\"3 objects\") in $log"
fi
if [ -n "$run_dir" ] && [ -n "$(ls "$run_dir"/screenshots/*.png 2>/dev/null | head -1)" ]; then
  pass "claude: mantra_screenshot saved $(ls "$run_dir"/screenshots/*.png | wc -l | tr -d ' ') capture(s) under the run's screenshots/"
else
  fail "claude: no PNG under ${run_dir:-the run dir}/screenshots/"
fi
if grep -Eq 'claude [0-9]+: .*"mcp__mantra__mantra_screenshot"' "$log" 2>/dev/null; then
  pass "claude: a worker called mcp__mantra__mantra_screenshot through the bridge"
else
  fail "claude: no 'claude <id>: …mcp__mantra__mantra_screenshot…' line in $log"
fi
if grep -Eq 'claude [0-9]+: \{"message":\{"content":\[\{"content":\[\{"source":\{"data":"[A-Za-z0-9+/=]+","media_type":"image/png"' "$log" 2>/dev/null; then
  pass "claude: the screenshot reached the agent as an image block of its tool_result"
else
  fail "claude: no 'claude <id>: …' tool_result line with an image block in $log"
fi

if [ "$fails" -ne 0 ]; then
  echo "$outs"
  echo "FAIL ($fails)"
  exit 1
fi
echo "PASS"
