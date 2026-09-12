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
