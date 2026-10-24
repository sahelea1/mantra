#!/usr/bin/env bash
# End-to-end check of the Bija backend: a whole Mandala run — planner, orchestrator, workers,
# gates, finale — on the real `bija` binary, against the in-process mock OpenAI server that
# `mantra --demo` starts (src/mock_openai.rs). No API calls, no key, no cost.
#
#   docs/tools/bija-e2e.sh                     # release build, then the run, then PASS/FAIL
#   BIJA_E2E_SPEED=2 docs/tools/bija-e2e.sh    # slower mock (the default is 8× demo speed)
#   MANTRA_MOCK_FLAKY=7 docs/tools/bija-e2e.sh # every 7th model request answers 503: bija retries
#   BIJA_E2E_KEEP=1 docs/tools/bija-e2e.sh     # keep the temp dir (MANTRA_HOME, demo repo) to look at
#
# What it proves, and how:
#   - the run reaches Done                   `"stage": "Done"` in the run's state.json
#   - the agents ran on bija, not mock-codex  one bija session per agent under $MANTRA_HOME/bija/sessions
#   - the tools went through bija            the planner's transcript holds mantra_submit_plan,
#                                             a worker's holds write_file
#   - workers' files reached the run branch   the demo repo's history has src/models/p1_models.rs
#   - nothing crashed                         no panic, no snapshot timeout
set -euo pipefail
cd "$(dirname "$0")/../../mantra_src"

target="${CARGO_TARGET_DIR:-$PWD/target}"
echo "building the workspace (release: mantra + bija)…"
cargo build --release -q
bin="$target/release/mantra"
for b in mantra bija; do
  [ -x "$target/release/$b" ] || { echo "FAIL: $target/release/$b was not built"; exit 1; }
done

# One temp dir holds everything: MANTRA_HOME, and — through TMPDIR, which the demo repo is
# created under — the demo project, so the script can find it afterwards.
tmp="$(mktemp -d)"
if [ -z "${BIJA_E2E_KEEP:-}" ]; then
  trap 'rm -rf "$tmp"' EXIT
else
  echo "keeping $tmp"
fi
export MANTRA_HOME="$tmp/home" TMPDIR="$tmp" MANTRA_MOCK_SPEED="${BIJA_E2E_SPEED:-8}" LANG="${LANG:-en_US.UTF-8}"
mkdir -p "$MANTRA_HOME"

echo "running a Mandala on bija (mantra-default)…"
script="wait:0.5;until:plan review@120;key:a;until:run complete@600;snap:done"
set +e
out="$("$bin" --demo --pattern mantra-default --snapshot "$script" run "build a tiny todo CLI" 2>&1)"
code=$?
set -e

fails=0
pass() { echo "PASS: $*"; }
fail() { echo "FAIL: $*"; fails=$((fails + 1)); }

[ "$code" -eq 0 ] && pass "mantra exited 0" || fail "mantra exited $code"
# Here-strings, not `echo | grep -q`: under pipefail an early match would SIGPIPE the echo and
# read as no match.
if grep -qi 'panicked' <<<"$out"; then fail "a panic: $(grep -i -m1 'panicked' <<<"$out")"; else pass "no panic"; fi
if grep -q '!! timeout' <<<"$out"; then fail "the snapshot timed out: $(grep -m1 '!! timeout' <<<"$out")"; else pass "no snapshot timeout"; fi

state="$(ls "$MANTRA_HOME"/runs/*/*/state.json 2>/dev/null | head -1 || true)"
if [ -z "$state" ]; then
  fail "no run state under $MANTRA_HOME/runs"
elif grep -q '"stage": "Done"' "$state"; then
  pass "the run reached Done ($state)"
else
  fail "the run did not reach Done: $(grep -m1 '"stage"' "$state" || echo 'no stage')"
fi

sessions="$MANTRA_HOME/bija/sessions"
n="$( (ls -d "$sessions"/*/ 2>/dev/null || true) | wc -l | tr -d ' ')"
# planner + orchestrator + 7 workers + 3 phase gates + the finale's QA, at the least
if [ "$n" -ge 10 ]; then pass "$n bija sessions on disk"; else fail "only $n bija sessions under $sessions (expected ≥ 10)"; fi
if grep -lq '"mantra_submit_plan"' "$sessions"/*/transcript.jsonl 2>/dev/null; then pass "the planner submitted its plan through bija"; else fail "no bija transcript holds a mantra_submit_plan call"; fi
if grep -lq '"write_file"' "$sessions"/*/transcript.jsonl 2>/dev/null; then pass "workers wrote files through bija"; else fail "no bija transcript holds a write_file call"; fi
if grep -Eq '"t": ?"turn"' "$sessions"/*/transcript.jsonl 2>/dev/null; then pass "turns were recorded in the transcripts"; else fail "no turn records in any transcript"; fi

proj="$(ls -d "$tmp"/mantra-demo-* 2>/dev/null | head -1 || true)"
if [ -z "$proj" ]; then
  fail "no demo project under $tmp"
elif grep -q 'src/models/p1_models.rs' <<<"$(git -C "$proj" log --all --name-only --format=)"; then
  pass "a worker's file reached the run's history ($proj)"
else
  fail "src/models/p1_models.rs is not in the demo repo's history"
fi

if [ "$fails" -ne 0 ]; then
  tail -60 <<<"$out"
  echo "FAIL ($fails)"
  exit 1
fi
echo "PASS"
