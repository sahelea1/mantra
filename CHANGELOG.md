# Changelog

All notable changes to Mantra are recorded here.

## v0.2.0

- WP1: bumped version to 0.2.0; removed the 7 dead-code warnings (`cargo build` is now clean); crash
  reasons and journal lines are ANSI-stripped and include the process exit code and the last
  non-warning stderr line; RPC errors during thread start keep their own message and append the
  stderr tail; simultaneous agent spawns are staggered by 300ms to avoid startup crash storms.
- WP2: context safety by default — Codex is always given an explicit `model_context_window` and
  `model_auto_compact_token_limit` for every agent (a conservative 200k / 85% assumed when a model
  doesn't set its own, shown dim as `(assumed)` / `(default)` in `/models`); the shipped built-in
  models (astra/sol/terra/luna) now ship honest explicit 272k / 85% defaults; a worker that still
  hits a context-full error while running on the assumed 200k gets that assumption halved for its
  next attempt, logged so you know to set the model's real context window.
- WP10 (partial: 10.1-10.3): Claude Code as a second agent backend, launched via `hub::claude`
  alongside the existing `codex app-server` backend, with no engine/UI-visible seam beyond the
  existing `HubEvent`/`Cmd` shapes. `ProviderEntry` gains a `kind` (`Codex` | `ClaudeCode`) and,
  for `ClaudeCode`, an `auth` (`subscription` | `api_key`); a built-in `claude` provider with its
  six default models (`opus46`/`opus48`/`opus5`/`sonnet5`/`fable5`/`fable51`, plus `[1m]`
  variants) is offered only when a `claude` binary is on `PATH`. `hub::claude::run_claude_process`
  spawns one long-lived `claude -p --input-format stream-json …` process per agent (session id /
  `--resume`, `--effort`, `--autocompact`, `--append-system-prompt`, sandbox-derived tool flags,
  `--bare` + `ANTHROPIC_API_KEY`/`ANTHROPIC_BASE_URL` for `api_key` auth, every inherited
  `CLAUDE*`/`ANTHROPIC_*` env var stripped first, `IS_SANDBOX=1` set automatically under root) and
  translates its NDJSON event stream into the same Codex-shaped `turn/started`, `item/started`,
  `item/completed`, `thread/tokenUsage/updated` and `turn/completed` notifications `Agent::apply`
  already understands — verified against a real recorded Claude Code 2.1.269 + LibertAI session
  fixture (`src/testdata/claude-stream.jsonl`) covering plain text, a bash tool call, a manual
  `/compact`, and a user-interrupted turn. The MCP tool bridge (10.4), doctor/discovery/docs
  (10.5) and the mock backend (10.6) are deliberately left for a later batch; the `Cmd::Respond`/
  `RespondErr` hook point for the bridge is in place but unwired.
- WP10 review fixes: `Cmd::Interrupt` now only fires (and only arms `interrupt_pending`) when a
  Claude turn is actually open, correlated to the specific `control_response` it caused — an
  idle-time interrupt (e.g. the 'x' keybinding, now also gated on `.busy()`) can no longer bleed
  into a later, unrelated turn's completion status; a turn whose only assistant content is a
  `thinking` block (no visible text/tool call) now still opens and closes in lockstep with its
  `result` instead of permanently wedging `turn_open` and swallowing every later turn. The Studio
  `providers` grid gained `kind`/`auth` columns (cycled with +/- like `sandbox`/`permission`
  elsewhere in Studio), so a third-party `ClaudeCode`/`api_key` provider can be configured entirely
  from the app, as §10.2 describes.
- WP10.4: the MCP tool bridge. `Hub` opens one Unix socket at `$MANTRA_HOME/run/<pid>.sock`
  whenever a `ClaudeCode` provider exists; every Claude agent gets `--strict-mcp-config
  --mcp-config '…'` pointing at `mantra mcp-bridge --sock <path> --agent <id>` (a new hidden
  subcommand, spawned by `claude` itself as an MCP stdio server), plus an `--append-system-prompt`
  note that its `mantra_*` tools are exposed as `mcp__mantra__mantra_*`. `Hub`'s accept loop reads
  each connection's one-line hello and hands it to that agent's own command channel as the new
  `Cmd::Bridge`, never a shared acceptor a per-agent task can't reach; `hub::claude` answers
  `tools/list` from its own `SpawnSpec.dynamic_tools` (the same JSON schemas Codex's `dynamicTools`
  gets) and turns `tools/call` into the same `HubEvent::Request{method:"item/tool/call"}` the Codex
  path already produces, so `App`/`Run::on_tool_call` need no changes beyond accepting the
  `cc-call-<n>` id prefix on `Cmd::Respond`/`RespondErr`.
- WP10.5: `mantra doctor` now checks `claude --version` (5s timeout on a thread — a `claude` stuck
  on a subscription login must never hang doctor) and, per Claude provider, either `claude auth
  status` (subscription) or the usual `$VAR set/NOT set` (`api_key`), with an `IS_SANDBOX` note
  under root. Pressing `D` on any `ClaudeCode` provider now always lists its six default models
  instantly (there's no `/models` endpoint for a subscription) instead of erroring "set the
  provider's base URL first"; a `base_url` still additionally queries `GET {base_url}/v1/models`
  live, same as a Codex custom provider. README gained a "Claude Code agents" section (install,
  subscription vs. API key, third-party gateways, which roles, permission/sandbox mapping, the root
  note); DESIGN.md §2 gained the backend-seam and MCP-bridge diagrams.
- WP10.6: `mantra mock-claude`, a fake `claude -p …stream-json` that — when given `--mcp-config` —
  spawns the real `mantra mcp-bridge` and speaks real MCP to it, so a demo run exercises the entire
  WP10.4 chain rather than a shortcut around it; shares its plan JSON and task-parsing helpers with
  `mock.rs`. `--demo` now overrides `Settings.claude_command` to `mock-claude` the same way it
  already overrides `codex_command`, and injects the built-in Claude provider/models in memory when
  `claude` isn't actually installed, so `mantra --demo --pattern mantra-default-claude` needs no
  real `claude`. Added the built-in `mantra-default-claude` pattern (planner/orchestrator on
  `fable51`, workers on `sonnet5`, gates left on their Codex models — a genuinely mixed-backend
  run) and a third `scripts/stress.sh` run driving it through plan, phase and finale. Added
  `scripts/live-claude.sh`, paste-ready live tests against a real `claude` + Anthropic-compatible
  API (solo turn, a mixed-backend Mandala run, kill-mid-turn restart, a bad key, and steering during
  a long tool call).
