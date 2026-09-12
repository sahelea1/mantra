# Changelog

All notable changes to Mantra are recorded here.

## v0.3.1

Four things a real run got wrong, fixed at the root. Driven by a second support bundle.

- **A hand stop sticks.** `ctrl+c` (or `x`) on an agent used to be undone within seconds: the
  interrupted turn read as "the planner ended without a plan", so Mantra nudged it; that nudge
  raced the process restart, so the transient-retry ladder sent it again; and a restart re-prompted
  "continue exactly where you left off". Three self-healing paths, each overriding the user. An
  agent stopped by hand is now marked as such — no nudge, no gate round, no retry, no resume
  prompt, and the watchdog leaves it alone — until someone messages it again, which is the one way
  to start it back up (`r` respawns it instead). The card and `mantra_status` say *stopped by you*,
  so the orchestrator stops reading it as a worker that merely went quiet.
- **You can see it is alive.** A Claude agent's thinking tick now carries its own running count, so
  the label reads `thinking · 9.4k` and visibly climbs instead of sitting at `thinking` for two
  minutes (captured from a live subscription session: the CLI ticks every 1–2s). When a turn
  really does go silent, both the status line and the stage card append an amber `· quiet 1m20s`,
  and a planner/orchestrator/gate agent that is connected but silent past `stall_minutes` gets one
  journal line (`⏳ … no output for 6m00s — still connected`) — workers keep the existing tripwire,
  which also wakes the orchestrator.
- **The context gauge is right from the first turn.** Claude Code announces the account's real
  window a second after spawn (`autocompact_state`, e.g. 980k), long before any `result` could.
  Mantra now takes it: a subscription login trusts it over the configured guess, an `api_key`
  gateway still keeps the window the user configured. A rate limit that is rejected or past 90% of
  a window raises one warning per episode rather than one per turn.
- **`mantra doctor` names the file.** `✗ claude: Permission denied (os error 13)` told nobody
  anything. The probe now resolves the command on `$PATH` itself and says which file is wrong and
  what to do: *is a directory — something else on your PATH shadows the real binary*, *is not
  executable — chmod +x …*, *is a broken symlink — reinstall the CLI*, or *`claude` not found on
  PATH*. `codex` gets the same treatment, and the same 5s timeout it was missing.
- **The Studio remembers where you were.** Leaving `/studio` or `/models` returned to the stage
  whenever a run existed, so a zoom — or Solo during a run — was lost every time. It now returns
  exactly where it was opened from, falling back to the old rule only when there is nothing to
  return to (or the remembered agent is gone).
- 114 unit tests (12 new), and `scripts/stress.sh` gains the hand stop end to end: ctrl+c during
  planning must leave the run stopped and un-nudged, and a typed message must start it again.

## v0.3.0

Coordination release: agents ask instead of guessing, halts travel up the chain of command before
they reach you, a revised plan restarts the phase it changed, and the orchestrator re-reads its
workers' work on a clock. Driven by a real run's support bundle (`support-bundle.sh`, new).

- **Chain of command.** Every worker, gate and finale agent has `mantra_ask`: a question goes to
  the orchestrator, who answers with `mantra_prompt` or passes it up with its own `mantra_ask` to
  the planner, who decides — or, only when the answer changes what is being built, asks you with
  `mantra_ask_user`. A question to you is a saffron band on the stage, not a halt: the run keeps
  going and your next message answers it. An agent that stops to wait is shown as *asked a
  question · waiting for the answer*, is never mistaken for "done", and the rung above it is the
  one the watchdog expects to act.
- **Halts escalate to the planner first.** *Gate exhausted* and *attempts exhausted* hand the
  planner the failing checks, their output and the last gate report, and exactly three ways out:
  `mantra_revise_plan` (the fix — the run resumes by itself), `mantra_resume_run(note)` (the plan
  is right; a hint for the stuck agent, and one more attempt), or `mantra_ask_user`. One reminder
  if it does nothing; then the band is yours. `r` on a task that exhausted its attempts resumes
  and retries in one go.
- **A revision restarts what it changed.** A task changed after its worker finished is re-opened
  (its old worker is cancelled, `mantra_spawn` accepts it again, the workspace keeps the earlier
  result); a phase already merging or gating goes back to building when tasks were added or
  changed; a gate that changed simply re-runs its checks. Nothing starts while the run is halted
  — `mantra_spawn`/`mantra_retry` say so instead of quietly re-running a phase you are sorting
  out. The identical failing check twice in a row escalates immediately instead of spending the
  remaining rounds while the gate agent keeps reporting "pass".
- **Periodic coherence review.** `settings.review_minutes` (default 3, `0` off, in the Studio):
  while workers build, the idle orchestrator gets each one's activity, files touched and recent
  log, and is asked to check the parallel work stays coherent with each other and the phase goal
  — steering only where something is off.
- **Claude Code backend.** The context gauge follows every API call's own usage (context = input
  + cache reads + cache writes), so it moves while a turn runs instead of staying at 0 and then
  showing "211k / 200k" from the turn's summed `result.usage`. A subscription login trusts the
  window the CLI reports for the account; an `api_key` gateway keeps the configured one. The
  CLI's once-a-second thinking ticks and tool progress count as activity, so a three-minute
  `pip install` never reads as an idle agent — and they no longer flood `mantra.log`. Default
  Claude models are now `opus46 opus48 opus5 sonnet5 fable5 fable51` at **1M** context and
  `haiku45` at 200k (the `[1m]` aliases are gone; an existing `models.toml` keeps its own
  `context_window` — set it to `1000000` or delete the file to regenerate).
- **Keys.** `ctrl+c` interrupts the focused agent's turn (or closes an overlay / clears the input);
  three presses within two seconds always quit, whatever state the app is in. `esc` only
  navigates — back to the overview from a zoom, otherwise clear the input — and never interrupts,
  so the two hints stop colliding in a zoomed Mandala agent.
- **Protocols and the default pattern.** Planner: keep the tooling later gates need until the last
  phase (cleanup belongs in the final phase), gate checks must run as-is on this machine. Gate:
  never report pass while a check fails — a check that cannot pass is a plan problem, say so with
  `mantra_ask`. Orchestrator: answer questions promptly, pass up what is above your brief, use the
  review. Worker: ask instead of guessing; end the turn without a STATUS line to wait.
- `support-bundle.sh`: one command that writes versions, `mantra doctor`, login state, terminal
  and sandbox facts, settings/models/patterns, the newest runs and the log tail into one file,
  with anything that looks like a key redacted. README gains *Reporting a problem*.
- `--demo` gains `MANTRA_MOCK_ASK=1` (a worker asks, the orchestrator answers), and the mock
  planner handles questions and escalations; 92 unit tests.

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
- WP3: the Studio selects roles by name instead of by index into a list that re-sorts on every
  draw, so changing a role's `kind` (or anything else that moves it) never lands the highlight on a
  different role. Roles gain a `permission` field (`never` by default, `on-request`, `untrusted`)
  cycled with ←/→ next to `sandbox`; a Mandala role's own permission — not the global Solo approval
  mode — now decides whether its requests auto-resolve or land in the inbox.
- WP4: the model picker (ctrl+k) and the Studio's role `model` field now show which provider a
  model runs through (`via OpenAI (Codex)`, or a configured provider's name), flag aliases that
  share a model id through different providers, and let you bump or edit a model's context window
  right from the picker (`+`/`-`, `c`) without opening `/models`. The `/models` `t` live test now
  sends a developer-role message, so a provider that rejects it (as some third-party gateways do)
  fails the test before it fails a real run.
- WP5: providers can now hold an API key directly (`api_key`, alongside `env_key`) — stored
  `0600` on Unix, delivered to each agent's own process environment (never on argv, never logged),
  with a synthesized `MANTRA_<ID>_API_KEY` variable name when `env_key` is left empty; the
  `/models` providers table gained a masked `api_key` column and shows whether a key is set via
  the environment or stored in `models.toml`.
- WP6: `Run.paused` became a typed `Halt { reason, agent, message, since }` (`HaltReason::User |
  Auth | UsageLimit | ProviderRejected | Environment | GateExhausted | AttemptsExhausted |
  AgentTurnFailed`) with a reason-specific resume hint; the opaque `‖ PAUSED` badge is replaced by
  a full-width amber `⛔ halted · … · …` band under the stage header. A new `ErrKind::ProviderRejected`
  (HTTP 400/422, "unexpected message role", …) is never retried and halts immediately, naming the
  role, model alias and provider. `m` on a selected stage agent opens the model picker to fix a
  `ProviderRejected` halt and resumes the run once a new model is picked.
- WP7: a watchdog now makes sure every agent that should be working is working. `Run::expected_active`
  says who must be busy right now and why (planning, orchestrating, working a task, gating, a finale
  step); an idle agent gets an escalation ladder — nudge it (`watchdog_seconds`, default 90s), then
  respawn the orchestrator or wake it about another idle agent (`watchdog_escalate_seconds`, default
  240s), then wake the planner (2×), then halt (`AgentTurnFailed`) if the planner itself doesn't
  respond — journaled with `⏰`. `Run::respawn` can now restart any run agent in place (planner,
  orchestrator, phase gate, finale step, or a worker), reachable from the stage `r` key (not just
  crashed processes), `ctrl+r` anywhere, and `/respawn`. A planner/orchestrator/gate/finale turn that
  fails past its retry cap gets one free respawn before the run halts. Tool guards refuse
  `mantra_prompt`/`mantra_interrupt`/`mantra_retry`/`mantra_set_effort` on a worker whose task is
  already `Done` once the phase has moved past orchestrating ("wait for the handoff"), and the
  orchestrator prompting the same stuck idle worker three times in five minutes respawns it instead
  of relaying a fourth message into the void (the F3 loop from real runs). Also from the v0.1 field
  reports: a `commandExecution` whose output names `bwrap`/user namespaces now halts the run
  immediately with `HaltReason::Environment` and the sandbox fix hint instead of burning gate rounds
  or retries (L1); two consecutive gate reports blocked on the same thing halt with `GateExhausted`
  right away instead of spending the remaining rounds (L4). The Studio's settings panel exposes the
  new `watchdog_seconds`/`watchdog_escalate_seconds` pattern settings (old saved patterns still load
  — they default to 90/240 via serde).
- WP8: zoom vs overview are now unmistakable — a zoomed agent gets a solid role-coloured header
  band and a coloured spine down the log; a run starts zoomed into the planner (once it's spawned);
  the plan-review overlay now opens over any screen (Solo, Zoom or Stage), not just the overview;
  approving a plan always lands you back on the overview with the orchestrator selected; `1`-`9`
  jump straight to the nth stage node and zoom in; every Stage↔Zoom switch flashes the header
  briefly (skipped when `reduce_motion` is set).
- WP9: busy-agent chat gets Claude-Code-style queueing — `⏎` while an agent is mid-turn appends
  your message to a visible `⏳ queued N` chip above the input instead of steering immediately;
  `ctrl+f` force-delivers the queue plus the current input into the running turn right away;
  `backspace` on an empty input restores the last queued message for editing, `ctrl+x` discards
  the whole queue. Applies to Solo, Zoom and `@name` messages from the stage; the engine's own
  prompting (steering workers, waking the orchestrator, etc.) is unaffected.
- WP10: Claude Code is a second agent backend, for any role. A provider now has a `kind`
  (`codex` | `claude-code`) and, for Claude, an `auth` (`subscription` | `api_key`); when a
  `claude` binary is on `PATH` a built-in `claude` provider appears with `opus46`/`opus48`/`opus5`/
  `sonnet5`/`fable5`/`fable51` (+ `[1m]` variants). `hub::claude` runs one long-lived `claude -p
  --input-format stream-json --output-format stream-json --verbose --dangerously-skip-permissions
  --session-id|--resume … --model … [--effort] --autocompact … --append-system-prompt …` process
  per agent (every inherited `CLAUDE*`/`ANTHROPIC_*` variable stripped, `IS_SANDBOX=1` set under
  root, `--bare` + `ANTHROPIC_API_KEY`/`ANTHROPIC_BASE_URL` for `api_key` auth through a gateway)
  and translates its NDJSON stream into the Codex-shaped notifications the reducer already
  understands, so the engine and UI have no backend branches; steering lands at the next tool
  boundary, `x` sends a real `control_request` interrupt only while a turn is open, `/compact`
  is the literal `/compact` line, a crash restarts with `--resume`, and a bad key halts as `Auth`
  within seconds instead of the CLI's own ten retries. Mantra's tools reach Claude through MCP:
  `Hub` opens `$MANTRA_HOME/run/<pid>.sock`, each Claude agent gets `--strict-mcp-config
  --mcp-config` pointing at `mantra mcp-bridge --sock … --agent <id>` (spawned by `claude` itself),
  `tools/list` is answered from the role's own tool schemas and `tools/call` becomes the same
  `item/tool/call` request the Codex path produces. `mantra doctor` checks `claude --version` (5 s
  timeout) and each Claude provider's auth; `D` on a Claude provider lists the six defaults (plus a
  gateway's `/v1/models`); the Studio's providers grid gained `kind`/`auth` columns; the built-in
  `mantra-default-claude` pattern runs planner/orchestrator on `fable51` and workers on `sonnet5`
  with Codex gates; `mantra mock-claude` + `--demo --pattern mantra-default-claude` drive the whole
  bridge chain in `scripts/stress.sh`; `scripts/live-claude.sh` holds the paste-ready live tests.
- WP11: runs can be listed, resumed and deleted. `state.json` is now a typed snapshot (stage,
  workspace, workers with their worktrees/branches/reports, agents with their thread ids) rewritten
  atomically at every transition. `mantra runs` lists every run of every project (id, stage, when,
  goal); `mantra runs resume <id>` reopens one from any directory at the nearest safe boundary
  (planner re-attached to its thread, a fresh orchestrator briefed with the phase status, running
  workers re-attached when their worktree still exists — otherwise re-spawned — merges/checks/gates
  simply run again, finished runs open read-only for `/land`); `mantra runs delete <id>` removes
  its worktrees, `mantra/<id>` + `mantra-w/<id>/*` branches and journal after a `y/N` (`--yes`
  skips it). In the TUI, `/runs` opens the same list for the current project (`⏎` resume, `D`
  delete), and the welcome screens say `↻ N unfinished runs — /runs`. `--resume-last` reopens the
  most recent unfinished run (with `--demo`, the last demo run).
- WP12 (real-run robustness): a `ProviderRejected` halt caused by a rejected `developer` message
  now says so and points at running that model through Claude Code (`kind = claude-code`);
  discovery marks `*-thinking` model ids as reasoning-capable when the catalogue says nothing
  (LibertAI/OpenRouter shapes); the orchestrator protocol tells it never to prompt a finished
  worker and to answer events with at most one `mantra_prompt` (3 wasted prompts per phase were
  measured); Mantra probes the Linux sandbox once at startup (`unprivileged_userns_clone`,
  AppArmor, `unshare -U`) and, when it can't work, says so on both welcome screens and in every
  run's pulse, and `mantra run`/`runs resume` ask `y/N` first (`--no-sandbox-check` skips it).

- Docs: the README is rebuilt around real screenshots — every frame is captured from `--demo` by
  `docs/tools/shoot.py`, which drives the binary inside tmux and paints the captured cells into a
  PNG — plus a generated mandala logo (`docs/tools/logo.py`), a plain-language explanation and
  collapsible sections for the depth. Two things the screenshots exposed are fixed with them: the
  model picker labelled every model `◌ codex` regardless of its provider (it now reads `✧ claude`
  for Claude Code providers, and the picker uses the full terminal width when there is room), and
  a 10-character alias ran into the model id beside it. Added the MIT `LICENSE` file the manifest
  has always pointed at.
