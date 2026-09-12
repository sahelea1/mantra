#!/usr/bin/env bash
# Live tests for the Claude Code backend (WP10.6) against a real `claude` binary + a real
# Anthropic-compatible API. Paste-ready, NOT run in CI (needs a real key/subscription, costs
# tokens, and one step needs interactive-ish timing) — run by hand when touching hub/claude.rs.
#
# Setup (mirrors v02plan.md §0.3's LibertAI test environment):
#   export MANTRA_HOME=/tmp/mantrahome-claude
#   export LIBERTAI_API_KEY="$API_KEY"          # a key for https://api.libertai.io
#   $MANTRA_HOME/models.toml must have a [[provider]] of kind = "claude_code" (see below) —
#   easiest is: run `mantra`, /models, add a provider, kind -> ClaudeCode, auth -> api_key,
#   base_url = https://api.libertai.io, env_key = LIBERTAI_API_KEY, press D to pull qwen3.8-27b in.
#   This sandbox runs as root, so `mantra` sets IS_SANDBOX=1 for you automatically (§10.1) —
#   nothing to do here.
set -euo pipefail
cd "$(dirname "$0")/.."
cargo build -q
bin=./target/debug/mantra
export MANTRA_HOME="${MANTRA_HOME:-/tmp/mantrahome-claude}"

need() { command -v "$1" >/dev/null || { echo "missing: $1"; exit 1; }; }
need claude
: "${LIBERTAI_API_KEY:?set LIBERTAI_API_KEY first (export LIBERTAI_API_KEY=\"\$API_KEY\")}"

echo "══ (1) Solo turn on a ClaudeCode provider — ask for a file, expect it to exist ══"
work=$(mktemp -d)
(cd "$work" && git init -q && git commit --allow-empty -qm init -c user.name=t -c user.email=t@t)
"$bin" --cwd "$work" --snapshot "wait:1;type:Create a file named hello.txt containing the word hi, then stop.;key:enter;until:hello.txt@60;snap:solo" | tail -30
test -f "$work/hello.txt" && echo "PASS: hello.txt exists" || echo "FAIL: hello.txt missing"
echo "   → also check the snapshot above shows a header like \"via LibertAI (claude)\" and tokens > 0"

echo "══ (2) A Mandala run: planner+orchestrator on the ClaudeCode provider, workers on Codex ══"
echo "   (edit ~/.mantra/patterns to point a copy of mantra-default's planner/orchestrator at your"
echo "    ClaudeCode model alias, or just use mantra-default-claude and swap workers back to Codex)"
"$bin" --cwd "$work" --pattern mantra-default-claude --snapshot "wait:1;until:plan review@120;key:a;until:Foundations@60;until:run complete@600;snap:done" run "add a --version flag" | tail -40

echo "══ (3) Kill the claude process mid-turn — expect a restart with --resume, agent continues ══"
echo "   Manual: start \`mantra --cwd $work\`, send a long-running prompt, then in another shell:"
echo "     pkill -9 -f 'claude .*--session-id'"
echo "   Expect: a toast/journal line showing the agent crashed and restarted, then it keeps going."
echo "   Journal: \$MANTRA_HOME/runs/<project>/<run>/journal.jsonl"

echo "══ (4) Bad key — expect a halt classified Auth within ~5s, not the CLI's own 10 retries ══"
bad_home=$(mktemp -d)
LIBERTAI_API_KEY="not-a-real-key" MANTRA_HOME="$bad_home" "$bin" --cwd "$work" --snapshot "wait:1;type:hello;key:enter;until:auth@30;snap:auth-halt" | tail -20

echo "══ (5) ctrl+f during a long Bash tool — expect the extra instruction to land after the tool returns ══"
"$bin" --cwd "$work" --snapshot "wait:1;type:Run \`sleep 5 && echo done\` in the shell.;key:enter;wait:1;type:also print the date when you finish;key:ctrl+f;until:date@30;snap:steered" | tail -30

echo "done — read each PASS/FAIL line and snapshot above; this script doesn't assert automatically."
