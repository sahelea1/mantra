# Mantra — design notes

This is the plan behind the code: what Mantra is for, how it is built, and why each decision was made. It covers the engine, protocol, resilience and configuration model; the README covers usage.

## 1. Goals and non-goals

**Goals.** One app that feels unified whether you run one Codex agent or a team of them; every configured model available everywhere; model, effort and context tunable live; multi-agent workflows that are *configurable* (by hand or by an agent) rather than hard-coded; performance and robustness good enough to leave running for an hour against flaky APIs; works in ordinary Linux and macOS terminals.

**Non-goals.** Re-implementing an agent loop (Codex already has a good one); a web UI; Windows; hiding the git workflow from the user (runs land as an ordinary branch you can inspect).

## 2. Architecture

```
┌──────────────────────── mantra (one process, tokio, 2 worker threads) ────────────────────────┐
│  input thread ──► AppEvent ◄── Hub events ◄── one task per agent ──► codex app-server (child)   │
│                     │                                                (JSON-RPC over stdio)      │
│                     ▼                                                                          │
│   App (state, routing, approvals, commands)                                                    │
│     ├── Agent reducers: protocol notifications → renderable items, tokens, plan, file stats   │
│     ├── Run = the Conductor (deterministic state machine for Mandala)                          │
│     │     acts only through the `Ctx` trait: spawn / prompt / interrupt / compact / stop /    │
│     │     tool_result / set_effort / job (blocking git work off-thread) / notify              │
│     └── UI: pure draw functions over App state (ratatui), redrawn only when dirty/animating    │
└────────────────────────────────────────────────────────────────────────────────────────────────┘
```

Source map: `rpc.rs` (JSON-RPC transport), `hub.rs` (process supervision), `agent.rs` (per-agent state + reducer), `engine/` (`pattern`, `plan`, `tools`, `git`, `run` = Conductor), `app.rs` (glue), `ui/` (screens), `mock.rs` (simulated Codex), `config.rs`, `util.rs`.

## 3. Key decisions

**Rust, single binary.** Needed for "performant, not laggy, not bloated": no runtime, ~3.5 MB, starts instantly. ratatui + crossterm cover every mainstream Linux/macOS terminal.

**Drive `codex app-server`, one process per agent.** The app-server protocol (verified against Codex 0.154.0) gives threads, turns, steering, interrupts, compaction, approvals, token usage, diffs and plan updates — everything a UI needs — while keeping Codex's own agent loop, sandbox, auth and provider support. One process per agent costs some memory (~30–60 MB each) but buys crash isolation: a wedged or crashed agent is restarted alone (exponential backoff, max 5 restarts per 10 min, then it parks until you press `r`) and **resumes its thread**, while the rest of the team keeps working. Per-agent config is passed as `-c` flags (`model_context_window`, `model_auto_compact_token_limit`, `sandbox_workspace_write.writable_roots`), each verified live.

**Mechanics in code, judgment in models ("the Conductor").** Spawning, waiting, retrying API errors, saving outputs, merging, running checks, archiving threads and advancing phases are done by a deterministic Rust state machine, not by an LLM looping over "check status" calls. LLM agents are only woken when judgment is needed, and they act through **dynamic tools** that Mantra executes: `mantra_submit_plan`, `mantra_revise_plan`, `mantra_spawn`, `mantra_prompt`, `mantra_retry`, `mantra_interrupt`, `mantra_set_effort`, `mantra_pause_agents`, `mantra_resume_agents`, `mantra_status`, `mantra_log`, `mantra_read_phase`, `mantra_brief_orchestrator`, `mantra_spawn_adhoc`, `mantra_gate_report`, `mantra_wait` (plus `mantra_read_pattern`/`mantra_write_pattern` for the architect). This makes runs cheaper (an idle orchestrator costs nothing), more reliable (no "the orchestrator forgot to merge"), and debuggable (every transition is journaled). Safety nets back up the models, e.g. tasks the orchestrator forgot to spawn are started automatically.

**The plan is a validated contract.** The planner submits JSON matching a schema: phases → tasks (`id`, `title`, `role`, `scope` globs, self-contained `prompt`, `acceptance`, optional `effort`) + a per-phase gate (`checks`, `focus`, `criteria`) + `final_checks` + `orchestrator_brief`. Mantra validates it (unique ids, known worker roles, non-trivial prompts, per-phase task caps, and **pairwise-disjoint scopes for parallel tasks**) and returns human-readable rejections so the planner fixes them itself. Revisions are diffed against the running phase so completed work is kept.

**Isolation by git worktrees.** Each run gets an integration worktree on branch `mantra/<run>`; each parallel worker gets its own worktree on `mantra-w/<run>/<task>-<attempt>` (a separate namespace, since git can't nest refs under an existing branch). At the gate Mantra commits and merges each worker branch (`--no-ff`); conflicts go to the QA gate agent, whose sandbox is widened to the integration worktree via `writable_roots`. Out-of-scope edits trip a wire that wakes the orchestrator. `isolation = auto` falls back to a shared directory for non-git projects; if a single worktree can't be created, that worker runs on the integration copy (safe because scopes are disjoint). `.mantra/` is added to `.git/info/exclude`. `/land` merges the run branch into your branch only when your tree is clean.

**Context management.** Compaction is shown honestly: Codex reports it as a `contextCompaction` item (plus a deprecated `thread/compacted` twin, which Mantra ignores when the item is present). Mantra records the context size before, amends the item with the size after from the next token-usage update, and treats a turn that contains *only* a compaction as housekeeping, not as "the agent finished" — otherwise the Conductor would mistake a compaction for a completed planner/orchestrator/worker turn. A `/compact` sent mid-turn is deferred to the end of the turn. Workers are short-lived and archived after their phase (their report and exit status are saved first). The orchestrator is reset per phase with a compact handoff (or compacted in place: `orchestrator_context = fresh|compact`). Every agent has a context gauge; `auto_compact_percent` per model maps to Codex's own auto-compaction; context-full errors trigger a compaction and a retry.

## 4. The default workflow as a state machine

```
Setup ─► Planning ─► Review ─► Phase[i]: Orchestrating ─► Merging ─► Checks ─► Gate(round ≤ N) ─► Handoff ─┐
            ▲  (user feedback)                                                                             │
            └──────────── re-prompt: planner may pause agents / revise plan / brief orchestrator ◄─────────┤
                                                                                   next phase ◄────────────┘
                                                               last phase ─► Finale[step…] ─► Done | Failed
```

The finale is a list in the pattern (default: heavy QA → security → planner verification with `may_spawn = true`). The whole flow is data: roles, which role plans/orchestrates/gates/handles re-prompts, and the finale chain are all editable in the Studio.

## 5. Resilience and cost control

| failure | handling |
|---|---|
| stream drop / overload / 429 / 5xx | Codex retries the stream; Mantra then retries the turn with backoff (4s, 8s, …), up to `worker_retries + 1` |
| context window exceeded | compact, then continue |
| 401 / 403 / usage limit | **pause the run**, alert + desktop notification, resume with `space` |
| planner / orchestrator / gate turn fails otherwise | pause + alert (never nag in a loop) |
| worker fails for good | reported to the orchestrator, who may steer or respawn; **hard cap** of `worker_retries + 3` attempts per task across every path, then the run pauses |
| codex process crash | restart that agent only, resume thread; parks after 5 crashes / 10 min |
| worktree creation fails | fall back to the shared integration copy |
| stall / token budget | tripwire event to the orchestrator |

Unrecognised errors are classified from the message (401/403/forbidden → auth; 429/502/503/disconnect → transient). Every turn Mantra starts is tracked (`awaiting_start`), so prompts sent during a turn start are queued and delivered as steering; failed steers are re-queued for the next turn.

## 6. Performance

Rendering is event-driven: the loop redraws only when state changed or something is animating (capped at `fps`, default 12); an idle Solo session renders at 0 fps. Bursts of streaming deltas are drained (up to 512 events) before a single redraw. Rendered lines are cached per item (invalidated by version, width, verbosity), and the log renders bottom-up so only visible items are laid out. Item history is capped per agent (3000, oldest trimmed). Animations are pure functions of time. Blocking git work runs on `spawn_blocking`.

**Terminal robustness.** Drawing happens inside `catch_unwind` (the panic hook skips terminal restoration while drawing), so a layout bug shows a one-line notice instead of killing every agent. Layout arithmetic is saturating throughout and verified by rendering all screens at 13 sizes in a debug build. Frames are wrapped in synchronized-output sequences. Under tmux, Mantra avoids the prefix key (`ctrl+t` toggles panels), skips the kitty keyboard query, wraps notifications in tmux passthrough plus a bell, warns about a slow `escape-time`, and keeps the pane title updated.

## 7. UI principles

- **Everything in a terminal, nothing fake:** box-drawing, braille spinners, colour blending; no images or unsupported escape tricks.
- **Consistent vocabulary** across modes: role glyph + colour identify an agent everywhere (`✦` planner, `◉` orchestrator, `◇`/`◆` small/big worker, `◎` QA, `▲` security); cards, peek strip and zoom all show model·effort.
- **Edges mean something:** rose animated = being prompted right now; green = running and watched; amber dotted = retrying/paused/waiting for approval; red = failed; faint dotted = not runnable yet.
- **One input everywhere**, with the same `/` commands; the stage input re-prompts the planner, `@name` targets an agent.
- **Zoom is Solo:** zooming into any agent reuses the Solo view (log, diffs, plan, context), so there's one thing to learn.
- Graceful degradation: layout adapts from 40 columns to ultrawide; ASCII and 16-colour fallbacks; `reduce_motion`.

## 8. Configuration model

Three layers, each editable in the TUI and as TOML: **settings** (codex command, defaults, UI), **models** (aliases → provider/model, context, compaction, efforts; custom providers via the Responses API with keys read from an env var, or pasted directly into `api_key` and stored `0600`), **patterns** (roles, settings, flow). Effort resolves agent override → role → model default, clamped to what the model supports. Everything lives in `~/.mantra` (`$MANTRA_HOME` overrides): settings, models, patterns, worktrees, logs and run journals (`runs/<project>-<hash>/<run>/`); nothing is written into projects, and an old `~/.config/mantra` is copied over non-destructively on first start. Patterns load from a repo-shipped `<repo>/.mantra/patterns/` (read only if present) → `~/.mantra/patterns/` → built-in, and are validated before use (unknown roles, missing flow roles, wrong kinds, bad colours/sandboxes are reported in plain language).

**Model discovery.** Codex's own catalog comes from `model/list` (Codex knows those models' context windows, so none is stored). Custom providers are queried with `GET {base_url}/models` via the system `curl`, with the key passed on stdin (never visible in `ps`). Context is read from a dozen vendor-specific fields (top level and one nested level); if absent, 200k is assumed and marked as such. Reasoning effort is enabled only when the provider advertises reasoning support; for those models Mantra passes `model_supports_reasoning_summaries=true` so Codex actually sends the effort, and for the rest `model_reasoning_summary="none"` and no effort, which keeps strict gateways happy (verified by recording the requests real Codex sends). Discovery never overwrites values the user set, and `wire_api` is always `responses` (the only value Codex accepts), so it isn't shown or stored.

**Approvals.** Codex fixes a turn's approval policy when the turn starts. Mantra therefore (1) sends every mode change to the live thread with `thread/settings/update` and on every `turn/start` (including the steer→new-turn race path), and (2) enforces "never ask" itself: any approval request that still arrives — from a turn started under another mode, a waiting card, or a Mandala agent — is answered automatically (`acceptForSession`, or a best-judgment reply to agent questions) and logged. The mode switch is handled before the approval card sees the key, so it works while a card is showing.

## 9. Security

Agents run inside Codex's sandbox (planner/orchestrator read-only; workers workspace-write in their own worktree; gate agents get the integration worktree as an extra writable root). Parallel workers use approval policy `never` — they can't escalate outside their sandbox; Solo uses your approval mode. API keys are never written unless you paste one into a provider's `api_key` field, which is then stored `0600` (Unix) and delivered to each agent's process environment only — never on argv, in logs, or in the journal. Plans and patterns are data, never executed, except gate `checks`, which are shell commands the planner proposes and **you approve as part of the plan review** (they run with a timeout in the integration worktree).

## 10. Testing

- Unit tests (25): effort resolution, registry round-trip, pattern validation (built-in + error cases), plan parsing/validation, protocol reducer (streaming, completion, diffs, compaction turns), error classification and provider-error unwrapping, notice de-duplication, provider model-list parsing (OpenAI, OpenRouter, vLLM, Groq, Gemini, bare lists), discovery add/fill-without-clobbering, effort-less custom models, run-dir placement, markdown wrapping at every width, input layout/cursor, glob matching.
- `scripts/stress.sh`: every screen and overlay rendered at 13 terminal sizes through a whole simulated run (debug build, overflow checks on); plus ASCII/16-colour runs verified to emit zero non-ASCII bytes; plus real tmux 3.4 sessions driven with `send-keys`.
- `mantra mock-codex`: a protocol-faithful fake app-server with scripted planner/orchestrator/worker/gate/architect behaviours (including a deliberate 502 and an out-of-scope edit), used by `--demo` and by headless end-to-end runs (`--snapshot "until:…;key:…;snap"` renders to a text buffer).
- Real Codex 0.154.0: process start, thread creation with dynamic tools and `-c` overrides, turn start, API-error paths.

## 11. Next steps

Tune prompts on real runs (plan quality, orchestrator brevity); per-run cost estimates from token usage; a resume-run picker for crashed Mantra sessions (state is already persisted in `state.json`); MCP elicitation UI; saved layouts; optional parallel phases (DAG instead of a chain) once the linear flow has proven itself.
