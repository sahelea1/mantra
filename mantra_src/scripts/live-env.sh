#!/usr/bin/env bash
# Prepare a throwaway Mantra home wired to LibertAI (https://api.libertai.io) for live tests,
# and print paste-ready commands. Needs: API_KEY (a LibertAI key), codex and/or claude on PATH.
#
#   API_KEY=... ./scripts/live-env.sh            # writes $MANTRA_HOME (default /tmp/mantrahome)
#   MANTRA_HOME=~/.mantra-live ./scripts/live-env.sh
set -euo pipefail
: "${API_KEY:?set API_KEY to a LibertAI key}"
export MANTRA_HOME="${MANTRA_HOME:-/tmp/mantrahome}"
export CODEX_HOME="${CODEX_HOME:-$HOME/.codex-mantra}"
mkdir -p "$MANTRA_HOME" "$CODEX_HOME"
cat > "$MANTRA_HOME/settings.toml" <<'T'
codex_command = ["codex", "app-server"]
default_model = "qwen"
approval_mode = "never"
sandbox = "workspace-write"
default_pattern = "mantra-default"
glyphs = "unicode"
colors = "256"
reduce_motion = true
mouse = false
side_panel = true
fps = 12
notify = false
T
# Aliases match the built-in pattern's roles (astra = planner, sol = big worker, luna = small
# worker, terra = QA). qwen3.5-122b-a10b is deliberately absent: it rejects Codex's developer role.
cat > "$MANTRA_HOME/models.toml" <<'T'
[[provider]]
id = "libertai"
name = "LibertAI"
base_url = "https://api.libertai.io/v1"
env_key = "LIBERTAI_API_KEY"

[[provider]]
id = "libertai-claude"
name = "LibertAI (Claude Code)"
kind = "claude-code"
auth = "api_key"
base_url = "https://api.libertai.io"
env_key = "LIBERTAI_API_KEY"

[[model]]
alias = "qwen"
provider = "libertai"
model = "qwen3.8-27b"
context_window = 262144
auto_compact_percent = 85
default_effort = "medium"
efforts = []

[[model]]
alias = "astra"
provider = "libertai"
model = "glm-5.3"
context_window = 262144
auto_compact_percent = 85
default_effort = "medium"
efforts = []

[[model]]
alias = "sol"
provider = "libertai"
model = "qwen3.8-27b"
context_window = 262144
auto_compact_percent = 85
default_effort = "medium"
efforts = []

[[model]]
alias = "luna"
provider = "libertai"
model = "qwen3.8-27b"
context_window = 262144
auto_compact_percent = 85
default_effort = "medium"
efforts = []

[[model]]
alias = "terra"
provider = "libertai"
model = "deepseek-v4-flash"
context_window = 200000
auto_compact_percent = 85
default_effort = "medium"
efforts = []

# 'tiny' forces compaction: the real window is 262k, Mantra tells Codex 16k so it compacts at ~13k.
[[model]]
alias = "tiny"
provider = "libertai"
model = "qwen3.8-27b"
context_window = 16000
auto_compact_percent = 85
default_effort = "medium"
efforts = []

[[model]]
alias = "cc-qwen"
provider = "libertai-claude"
model = "qwen3.8-27b"
context_window = 262144
auto_compact_percent = 85
default_effort = "medium"
efforts = []
T
export LIBERTAI_API_KEY="$API_KEY"
if [ "$(id -u)" = 0 ]; then export IS_SANDBOX=1; fi
bin="$(cd "$(dirname "$0")/.." && pwd)/target/debug/mantra"
cat <<EOT
Mantra home: $MANTRA_HOME   (codex home: $CODEX_HOME)
Exported: MANTRA_HOME CODEX_HOME LIBERTAI_API_KEY${IS_SANDBOX:+ IS_SANDBOX=1}
Run these in the same shell (source this script: '. scripts/live-env.sh'):

  # 1. doctor
  $bin doctor

  # 2. Solo turn through Codex + LibertAI (creates hello.txt in a scratch repo)
  d=\$(mktemp -d) && git -C "\$d" init -q && cd "\$d" && $bin --snapshot "wait:2;snap:start;type:Create a file hello.txt containing the word hello and tell me when done.;key:enter;until:hello.txt@150;snap:turn;wait:6;snap:after" ; ls "\$d"

  # 3. Full Mandala run (plan -> worker -> gate -> finale)
  d=\$(mktemp -d) && git -C "\$d" init -q && cd "\$d" && echo '# demo' > README.md && git add -A && git -c user.email=t@t -c user.name=t commit -qm init && $bin --size 140x42 --snapshot "wait:3;snap:planning;until:plan review@900;snap:review;key:a;wait:5;snap:approved;until:finale 1/3@1500;snap:finale;until:run complete@1500;snap:done" run "Create a tiny Python CLI greet.py that prints a greeting for a name passed as argument, with test_greet.py using unittest. Keep it minimal."

  # 4. Solo turn through Claude Code + LibertAI (needs the Claude backend, WP10)
  d=\$(mktemp -d) && git -C "\$d" init -q && cd "\$d" && $bin --snapshot "wait:2;type:/model;key:enter;snap:picker;key:esc;type:Create a file hi.txt containing hi.;key:enter;until:hi.txt@150;snap:turn" 
EOT
