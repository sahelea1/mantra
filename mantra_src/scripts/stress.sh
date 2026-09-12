#!/usr/bin/env bash
# Render every screen and overlay at 13 terminal sizes (40x12 … 320x90) during a simulated run.
# Uses the debug build so arithmetic overflows panic instead of wrapping. No API calls.
set -euo pipefail
cd "$(dirname "$0")/.."
cargo build -q
export MANTRA_HOME="$(mktemp -d)" MANTRA_MOCK_SPEED=4 LANG=${LANG:-en_US.UTF-8}
bin=./target/debug/mantra
echo "solo + overlays…"
$bin --demo --snapshot "wait:1;sweep:welcome;type:add a helper;key:enter;until:Run this command@30;sweep:approval;key:y;until:Want me to wire@30;sweep:done;key:ctrl+k;sweep:picker;key:esc;key:ctrl+d;sweep:diff;key:esc;key:?;sweep:help;key:esc;key:ctrl+g;sweep:inbox;key:esc;type:/compact;key:enter;wait:0.2;sweep:compacting;wait:2;type:/studio;key:enter;sweep:studio;key:esc;type:/models;key:enter;sweep:models;key:D;until:gpt-5.5@15;sweep:discover-picker;key:esc" | grep -E 'sweep ok|panicked|timeout'
echo "mandala…"
$bin --demo --snapshot "wait:0.5;sweep:planning;until:plan review@60;sweep:review;key:a;until:Foundations@60;sweep:phase1;until:Features@90;wait:1;sweep:phase2;key:tab;key:right;key:enter;sweep:zoom;key:esc;key:space;sweep:paused;key:space;until:finale 2/3@150;sweep:finale;until:run complete@150;sweep:done" run "build a todo API with auth" | grep -E 'sweep ok|panicked|timeout'
echo "halt band (WP6: ProviderRejected)…"
MANTRA_MOCK_BADMODEL=1 $bin --demo --snapshot "wait:0.5;until:plan review@60;key:a;until:halted@60;sweep:halt-band;snap:halt-band" run "trigger a bad model" | grep -E 'sweep ok|panicked|timeout|halted'
echo "watchdog (WP7.2/WP7.6: idle-agent nudge, real wall-clock wait ~watchdog_seconds)…"
MANTRA_MOCK_LAZY_ORCH=1 $bin --demo --snapshot "wait:0.5;until:plan review@60;key:a;until:Foundations@60;wait:1;until:watchdog@150;sweep:watchdog;snap:watchdog" run "build a todo API with auth" | grep -E 'sweep ok|panicked|timeout|watchdog'
echo "all sizes rendered without panics"
