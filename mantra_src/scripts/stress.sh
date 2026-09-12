#!/usr/bin/env bash
# Render every screen and overlay at 13 terminal sizes (40x12 … 320x90) during a simulated run.
# Uses the debug build so arithmetic overflows panic instead of wrapping. No API calls.
set -euo pipefail
cd "$(dirname "$0")/.."
cargo build -q
export MANTRA_HOME="$(mktemp -d)" MANTRA_MOCK_SPEED=4 LANG=${LANG:-en_US.UTF-8}
bin=./target/debug/mantra
echo "solo + overlays…"
$bin --demo --snapshot "wait:1;sweep:welcome;type:add a helper;key:enter;until:Run this command@30;sweep:approval;key:y;until:Want me to wire@30;sweep:done;key:ctrl+k;sweep:picker;key:esc;key:ctrl+d;sweep:diff;key:esc;key:?;sweep:help;key:esc;key:ctrl+g;sweep:inbox;key:esc;type:/compact;key:enter;wait:0.2;sweep:compacting;wait:2;type:/studio;key:enter;sweep:studio;key:esc;type:/models;key:enter;sweep:models;key:D;until:gpt-5.5@15;sweep:discover-picker;key:esc;key:ctrl+o;type:hello;key:enter;until:Thinking@30;type:also do X;key:enter;sweep:queued;key:ctrl+f;wait:0.3;sweep:forced" | grep -E 'sweep ok|panicked|timeout'
echo "mandala…"
$bin --demo --snapshot "wait:0.5;sweep:planning;until:plan review@60;sweep:review;key:a;until:Foundations@60;sweep:phase1;until:Features@90;wait:1;sweep:phase2;key:right;key:enter;sweep:zoom;key:esc;key:space;sweep:paused;key:space;until:finale 2/3@150;sweep:finale;until:run complete@150;sweep:done" run "build a todo API with auth" | grep -E 'sweep ok|panicked|timeout'
echo "mandala on Claude Code (mock-claude + mcp-bridge, mixed with Codex gates)…"
$bin --demo --pattern mantra-default-claude --snapshot "wait:0.5;sweep:cc-planning;until:plan review@60;sweep:cc-review;key:a;until:Foundations@60;sweep:cc-phase1;until:Features@90;wait:1;sweep:cc-phase2;until:run complete@180;sweep:cc-done" run "build a tiny todo CLI" | grep -E 'sweep ok|panicked|timeout'
echo "halt band (WP6: ProviderRejected)…"
MANTRA_MOCK_BADMODEL=1 $bin --demo --snapshot "wait:0.5;until:plan review@60;key:a;until:halted@60;sweep:halt-band;snap:halt-band" run "trigger a bad model" | grep -E 'sweep ok|panicked|timeout|halted'
echo "watchdog (WP7.2/WP7.6: idle-agent nudge, real wall-clock wait ~watchdog_seconds)…"
MANTRA_MOCK_LAZY_ORCH=1 $bin --demo --snapshot "wait:0.5;until:plan review@60;key:a;until:Foundations@60;wait:1;until:watchdog@150;sweep:watchdog;snap:watchdog" run "build a todo API with auth" | grep -E 'sweep ok|panicked|timeout|watchdog'
echo "zoom vs overview (WP8)…"
out=$($bin --demo --snapshot "until:zoomed@10;snap:start-zoomed;until:plan review@60;snap:review-in-zoom;key:a;wait:0.3;until:overview@10;snap:overview-after-approve" run "build a tiny CLI" 2>&1)
echo "$out" | grep -qi 'panicked' && { echo "$out"; echo "PANIC during zoom-vs-overview snapshot"; exit 1; }
echo "$out" | grep -q '!! timeout' && { echo "$out"; echo "TIMEOUT during zoom-vs-overview snapshot"; exit 1; }
echo "$out" | grep -q 'zoomed' || { echo "$out"; echo "MISSING: run should start zoomed on the planner"; exit 1; }
echo "$out" | grep -qi 'plan review' || { echo "$out"; echo "MISSING: plan review overlay should open while zoomed"; exit 1; }
echo "$out" | grep -q 'overview' || { echo "$out"; echo "MISSING: approving the plan should land on the overview"; exit 1; }
echo "zoom vs overview ok"

# v0.3 chain of command: a worker stops to ask (MANTRA_MOCK_ASK), the orchestrator answers, the worker finishes.
echo "chain of command (a worker asks, the orchestrator answers)…"
export MANTRA_HOME="$(mktemp -d)"
out=$(MANTRA_MOCK_ASK=1 $bin --demo --snapshot "wait:0.5;until:plan review@60;key:a;until:run complete@200;snap:done" run "build a todo API with auth" 2>&1)
echo "$out" | grep -qi 'panicked' && { echo "$out"; echo "PANIC in the ask scenario"; exit 1; }
echo "$out" | grep -q '!! timeout' && { echo "$out"; echo "TIMEOUT: the run with a question did not complete"; exit 1; }
j=$(ls "$MANTRA_HOME"/runs/*/*/journal.jsonl)
grep -q 'p2-api asks the orchestrator' "$j" || { cat "$j"; echo "MISSING: the worker should ask the orchestrator"; exit 1; }
grep -q 'p2-api waits for an answer' "$j" || { cat "$j"; echo "MISSING: an asking worker waits instead of finishing"; exit 1; }
grep -q 'orchestrator → p2-api (answer)' "$j" || { cat "$j"; echo "MISSING: the orchestrator should answer"; exit 1; }
grep -q 'p2-api done' "$j" || { cat "$j"; echo "MISSING: the answered worker should finish"; exit 1; }
echo "chain of command ok"

# WP11: leave a run mid-phase, list it, resume it headlessly to completion, drive the /runs overlay, delete it.
echo "runs: list / resume / delete…"
export MANTRA_HOME="$(mktemp -d)"
out=$($bin --demo --snapshot "wait:0.5;until:plan review@60;key:a;until:spawned p1-config@60;wait:0.3" run "build a todo API with auth" 2>&1)
echo "$out" | grep -qi 'panicked' && { echo "$out"; echo "PANIC before leaving the run"; exit 1; }
$bin runs | grep -q 'phase 1' || { $bin runs; echo "MISSING: mantra runs should list the unfinished run at phase 1"; exit 1; }
proj=$(python3 -c "import json,glob,os;print(json.load(open(glob.glob(os.environ['MANTRA_HOME']+'/runs/*/*/state.json')[0]))['project'])")
out=$(MANTRA_DEMO_PROJECT="$proj" $bin --demo --snapshot "wait:0.3;snap:welcome;type:/runs;key:enter;sweep:runs-overlay;key:D;snap:confirm;key:n;key:esc" 2>&1)
echo "$out" | grep -E 'sweep ok|panicked|timeout'
echo "$out" | grep -qi 'panicked' && { echo "$out"; echo "PANIC in the /runs overlay"; exit 1; }
echo "$out" | grep -q 'unfinished run' || { echo "$out"; echo "MISSING: welcome screen should mention the unfinished run"; exit 1; }
echo "$out" | grep -q 'branches, worktrees and journal' || { echo "$out"; echo "MISSING: D should ask before deleting"; exit 1; }
out=$($bin --demo --resume-last --snapshot "wait:1;snap:resumed;until:run complete@200;snap:done" 2>&1)
echo "$out" | grep -qi 'panicked' && { echo "$out"; echo "PANIC during resume"; exit 1; }
echo "$out" | grep -q '!! timeout' && { echo "$out"; echo "TIMEOUT: resumed run did not complete"; exit 1; }
echo "$out" | grep -q 're-attached\|re-spawned' || { echo "$out"; echo "MISSING: resume should re-attach or re-spawn the phase-1 workers"; exit 1; }
id=$($bin runs | awk 'NR==2 {print $1}' | sed 's/…$//')
$bin runs delete "$id" --yes | grep -q '^deleted' || { echo "MISSING: mantra runs delete should remove the run"; exit 1; }
$bin runs | grep -q 'no runs yet' || { $bin runs; echo "MISSING: the deleted run should be gone"; exit 1; }
git -C "$proj" branch --list 'mantra*' | grep -q . && { git -C "$proj" branch --list 'mantra*'; echo "MISSING: run branches should be deleted"; exit 1; }
echo "runs ok"

# WP12.4: the startup sandbox notice (forced via MANTRA_SANDBOX_WARNING; the real probe never runs in demo mode).
echo "sandbox notice…"
export MANTRA_HOME="$(mktemp -d)"
out=$(MANTRA_SANDBOX_WARNING="user namespaces are blocked (test)" $bin --demo --snapshot "wait:0.3;sweep:welcome-sandbox;snap:welcome-sandbox;key:ctrl+o;sweep:stage-sandbox;snap:stage-sandbox;type:build a thing;key:enter;until:plan review@60" 2>&1)
echo "$out" | grep -E 'sweep ok|panicked|timeout'
echo "$out" | grep -qi 'panicked' && { echo "$out"; echo "PANIC with the sandbox notice"; exit 1; }
echo "$out" | grep -q 'sandbox: user namespaces are blocked' || { echo "$out"; echo "MISSING: welcome screens should show the sandbox notice"; exit 1; }
grep -q 'sandbox: user namespaces are blocked' "$MANTRA_HOME"/runs/*/*/journal.jsonl || { echo "MISSING: a run's journal should carry the sandbox notice"; exit 1; }
echo "sandbox notice ok"
echo "all sizes rendered without panics"
