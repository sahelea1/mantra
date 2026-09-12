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
