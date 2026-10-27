#!/usr/bin/env bash
# Render every screen and overlay at 13 terminal sizes (40x12 … 320x90) during a simulated run.
# Uses the debug build so arithmetic overflows panic instead of wrapping. No API calls.
set -euo pipefail
cd "$(dirname "$0")/.."
cargo build -q
export MANTRA_HOME="$(mktemp -d)" MANTRA_MOCK_SPEED=4 LANG=${LANG:-en_US.UTF-8}
bin=./target/debug/mantra
# Snapshot `until` reports a timeout in its output; it is not a process error. Never let a
# rendered sweep after that timeout turn a failed scenario into a passing stress run.
checked_snapshot() {
    local out
    if ! out=$("$@" 2>&1); then
        echo "$out"
        return 1
    fi
    if grep -qiE 'panicked|!! timeout' <<<"$out"; then
        echo "$out"
        echo "FAILED: snapshot panicked or timed out"
        return 1
    fi
    grep -E 'sweep ok|halted|watchdog' <<<"$out" || true
}
echo "solo + overlays…"
checked_snapshot $bin --demo --snapshot "wait:1;sweep:welcome;type:add a helper;key:enter;until:Run this command@30;sweep:approval;key:y;until:Want me to wire@30;sweep:done;key:ctrl+k;sweep:picker;key:esc;key:ctrl+d;sweep:diff;key:esc;key:?;sweep:help;key:esc;key:ctrl+g;sweep:inbox;key:esc;type:/compact;key:enter;wait:0.2;sweep:compacting;wait:2;type:/studio;key:enter;sweep:studio;key:right;key:down;key:down;key:down;key:down;key:down;key:down;key:down;key:down;key:down;key:down;type:x;sweep:studio-edit;key:esc;key:esc;type:/models;key:enter;sweep:models;type:x;sweep:models-edit;key:esc;key:ctrl+n;sweep:add-menu;key:enter;type:gsk_stress;sweep:key-flow;key:esc;key:tab;sweep:providers;key:tab;key:ctrl+d;until:gpt-5.5@15;sweep:discover-picker;key:esc;key:esc;type:/settings;key:enter;sweep:settings;key:end;sweep:settings-end;key:esc;type:/run;key:enter;sweep:new-run;key:esc;key:ctrl+o;type:hello;key:enter;until:Thinking@30;type:also do X;key:enter;sweep:queued;key:ctrl+f;wait:0.3;sweep:forced"
echo "mandala…"
checked_snapshot $bin --demo --snapshot "wait:0.5;sweep:planning;until:plan review@60;sweep:review;key:a;until:Foundations@60;sweep:phase1;until:Features@90;wait:1;sweep:phase2;key:right;key:enter;sweep:zoom;key:esc;key:space;sweep:paused;key:space;until:finale 2/2@150;sweep:finale;until:run complete@150;sweep:done;type:/stop;key:enter;type:/runs;key:enter;sweep:runs-after" run "build a todo API with auth"
# BADMODEL, LAZY_ORCH and ASK are Codex mock scenarios. Built-ins intentionally run Bija,
# so save a copy of the just-tested default pattern with explicit demo Codex model aliases.
# Keep this template outside MANTRA_HOME: later scenarios deliberately create a fresh home.
codex_pattern=$(mktemp)
trap 'rm -f "$codex_pattern"' EXIT
python3 - "$MANTRA_HOME" "$codex_pattern" <<'PYCODEX'
import pathlib, re, sys
root = pathlib.Path(sys.argv[1])
source = next(root.glob("runs/*/*/pattern.toml")).read_text()
source = re.sub(r'^name = "mantra-default"$', 'name = "stress-codex"', source, count=1, flags=re.M)
source = re.sub(r'^(model|fallback) = "([^"]+)"$', lambda m: f'{m[1]} = "{m[2]}-codex"', source, flags=re.M)
pathlib.Path(sys.argv[2]).write_text(source)
PYCODEX
install_codex_pattern() {
    mkdir -p "$MANTRA_HOME/patterns"
    cp "$codex_pattern" "$MANTRA_HOME/patterns/stress-codex.toml"
}
install_codex_pattern
echo "halt band (WP6: ProviderRejected)…"
MANTRA_MOCK_BADMODEL=1 checked_snapshot $bin --demo --pattern stress-codex --snapshot "wait:0.5;until:plan review@60;key:a;until:halted@60;sweep:halt-band;snap:halt-band" run "trigger a bad model"
echo "watchdog (WP7.2/WP7.6: idle-agent nudge, real wall-clock wait ~watchdog_seconds)…"
MANTRA_MOCK_LAZY_ORCH=1 checked_snapshot $bin --demo --pattern stress-codex --snapshot "wait:0.5;until:plan review@60;key:a;until:Foundations@60;wait:1;until:watchdog@150;sweep:watchdog;snap:watchdog" run "build a todo API with auth"
echo "zoom vs overview (WP8)…"
out=$($bin --demo --pattern stress-codex --snapshot "until:zoomed@10;snap:start-zoomed;until:plan review@60;snap:review-in-zoom;key:a;wait:0.3;until:focus:agents@10;snap:overview-after-approve" run "build a tiny CLI" 2>&1)
echo "$out" | grep -qi 'panicked' && { echo "$out"; echo "PANIC during zoom-vs-overview snapshot"; exit 1; }
echo "$out" | grep -q '!! timeout' && { echo "$out"; echo "TIMEOUT during zoom-vs-overview snapshot"; exit 1; }
echo "$out" | grep -q 'zoomed' || { echo "$out"; echo "MISSING: run should start zoomed on the planner"; exit 1; }
echo "$out" | grep -qi 'plan review' || { echo "$out"; echo "MISSING: plan review overlay should open while zoomed"; exit 1; }
echo "$out" | grep -q 'focus:agents' || { echo "$out"; echo "MISSING: approving the plan should focus the overview's agent canvas"; exit 1; }
echo "zoom vs overview ok"

# v0.3.1: a hand stop sticks. ctrl+c on the (zoomed) planner must stop it and leave it stopped —
# no planner nudge, no transient retry, no "continue where you left off" — until the user says so.
echo "hand stop (ctrl+c) sticks, and a message lifts it…"
export MANTRA_HOME="$(mktemp -d)"
install_codex_pattern
out=$(MANTRA_MOCK_SPEED=1 $bin --demo --pattern stress-codex --snapshot "wait:1.2;key:ctrl+c;wait:4;snap:stopped" run "build a todo API with auth" 2>&1)
echo "$out" | grep -qi 'panicked' && { echo "$out"; echo "PANIC during the hand stop"; exit 1; }
echo "$out" | grep -q 'stopped by you' || { echo "$out"; echo "MISSING: the card should say the agent was stopped by you"; exit 1; }
j=$(ls "$MANTRA_HOME"/runs/*/*/journal.jsonl)
grep -q 'stopped by you' "$j" || { cat "$j"; echo "MISSING: the journal should record the hand stop"; exit 1; }
grep -q "haven't submitted" "$j" && { cat "$j"; echo "REGRESSION: the planner was nudged back to work after a hand stop"; exit 1; }
grep -q 'plan v1' "$j" && { cat "$j"; echo "REGRESSION: the run carried on after the user stopped it"; exit 1; }
# …and talking to it is what starts it again
export MANTRA_HOME="$(mktemp -d)"
install_codex_pattern
out=$(MANTRA_MOCK_SPEED=1 $bin --demo --pattern stress-codex --snapshot "wait:1.2;key:ctrl+c;wait:3;type:ok carry on;key:enter;until:plan review@60;snap:resumed" run "build a todo API with auth" 2>&1)
echo "$out" | grep -q '!! timeout' && { echo "$out"; echo "MISSING: a message to a stopped planner should get it planning again"; exit 1; }
j=$(ls "$MANTRA_HOME"/runs/*/*/journal.jsonl)
grep -q 'stopped by you' "$j" && grep -q 'plan v1' "$j" || { cat "$j"; echo "MISSING: stop then resume should both appear in the journal"; exit 1; }
echo "hand stop ok"

# v0.3 chain of command: a worker stops to ask (MANTRA_MOCK_ASK), the orchestrator answers, the worker finishes.
echo "chain of command (a worker asks, the orchestrator answers)…"
export MANTRA_HOME="$(mktemp -d)"
install_codex_pattern
out=$(MANTRA_MOCK_ASK=1 $bin --demo --pattern stress-codex --snapshot "wait:0.5;until:plan review@60;key:a;until:run complete@200;snap:done" run "build a todo API with auth" 2>&1)
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
install_codex_pattern
out=$($bin --demo --pattern stress-codex --snapshot "wait:0.5;until:plan review@60;key:a;until:spawned p1-config@60;wait:0.3" run "build a todo API with auth" 2>&1)
echo "$out" | grep -qi 'panicked' && { echo "$out"; echo "PANIC before leaving the run"; exit 1; }
echo "$out" | grep -q '!! timeout' && { echo "$out"; echo "TIMEOUT before leaving the run"; exit 1; }
$bin runs | grep -q 'phase 1' || { $bin runs; echo "MISSING: mantra runs should list the unfinished run at phase 1"; exit 1; }
proj=$(python3 -c "import json,glob,os;print(json.load(open(glob.glob(os.environ['MANTRA_HOME']+'/runs/*/*/state.json')[0]))['project'])")
out=$(MANTRA_DEMO_PROJECT="$proj" $bin --demo --pattern stress-codex --snapshot "wait:0.3;snap:welcome;type:/runs;key:enter;sweep:runs-overlay;key:D;snap:confirm;key:n;key:esc" 2>&1)
echo "$out" | grep -E 'sweep ok|panicked|timeout'
echo "$out" | grep -qi 'panicked' && { echo "$out"; echo "PANIC in the /runs overlay"; exit 1; }
echo "$out" | grep -q 'unfinished run' || { echo "$out"; echo "MISSING: welcome screen should mention the unfinished run"; exit 1; }
echo "$out" | grep -q 'branches, worktrees and journal' || { echo "$out"; echo "MISSING: D should ask before deleting"; exit 1; }
out=$($bin --demo --pattern stress-codex --resume-last --snapshot "wait:1;snap:resumed;until:run complete@200;snap:done" 2>&1)
echo "$out" | grep -qi 'panicked' && { echo "$out"; echo "PANIC during resume"; exit 1; }
echo "$out" | grep -q '!! timeout' && { echo "$out"; echo "TIMEOUT: resumed run did not complete"; exit 1; }
echo "$out" | grep -q 're-attached\|re-spawned' || { echo "$out"; echo "MISSING: resume should re-attach or re-spawn the phase-1 workers"; exit 1; }
# `runs` is a human-facing table whose first column is the title, not the stable run id.
id=$(python3 -c "import json,glob,os;print(json.load(open(glob.glob(os.environ['MANTRA_HOME']+'/runs/*/*/state.json')[0]))['id'])")
$bin runs delete "$id" --yes | grep -q '^deleted' || { echo "MISSING: mantra runs delete should remove the run"; exit 1; }
$bin runs | grep -q 'no runs yet' || { $bin runs; echo "MISSING: the deleted run should be gone"; exit 1; }
git -C "$proj" branch --list 'mantra*' | grep -q . && { git -C "$proj" branch --list 'mantra*'; echo "MISSING: run branches should be deleted"; exit 1; }
echo "runs ok"

# WP12.4: the startup sandbox notice (forced via MANTRA_SANDBOX_WARNING; the real probe never runs in demo mode).
echo "sandbox notice…"
export MANTRA_HOME="$(mktemp -d)"
install_codex_pattern
out=$(MANTRA_SANDBOX_WARNING="user namespaces are blocked (test)" $bin --demo --pattern stress-codex --snapshot "wait:0.3;sweep:welcome-sandbox;snap:welcome-sandbox;key:ctrl+o;sweep:stage-sandbox;snap:stage-sandbox;type:build a thing;key:enter;until:plan review@60" 2>&1)
echo "$out" | grep -E 'sweep ok|panicked|timeout'
echo "$out" | grep -qi 'panicked' && { echo "$out"; echo "PANIC with the sandbox notice"; exit 1; }
echo "$out" | grep -q '!! timeout' && { echo "$out"; echo "TIMEOUT with the sandbox notice"; exit 1; }
echo "$out" | grep -q 'sandbox: user namespaces are blocked' || { echo "$out"; echo "MISSING: welcome screens should show the sandbox notice"; exit 1; }
grep -q 'sandbox: user namespaces are blocked' "$MANTRA_HOME"/runs/*/*/journal.jsonl || { echo "MISSING: a run's journal should carry the sandbox notice"; exit 1; }
echo "sandbox notice ok"
echo "all sizes rendered without panics"
