# ✦ Mantra

One terminal for coding agents. Work with a single agent (**Solo**), or hand a goal to a whole team that plans it, builds it in parallel phases and checks it (**Mandala**). Agents run on **OpenAI Codex** or **Claude Code** — per role, mixed freely — with any model either of them can reach: your subscription, the vendors' APIs, or an OpenAI/Anthropic-compatible gateway such as OpenRouter or LibertAI.

Mantra is a single ~4 MB Rust binary. It drives the official `codex app-server` (JSON-RPC over stdio) and `claude -p` (stream-json over stdio), one process per agent, so crashes stay isolated and your logins, sandboxes and approvals keep working exactly as in the CLIs.

```
 ✦ mantra  mandala › overview  89735-build-a-todo-api  ◈ mantra-default             ⏱ 10s  Σ 33.5k tok  ● 2 active
 ✓ Plan ━━━━━━━━ ✓ Foundations ━━━━━━━━ ◉ Features ━━━━━━━━ ○ Integration ━━━━━━━━ ○ Finale
 ┏ ✦ planner  astra·max ━━┓  ╭ ◉ orchestrator  astra·high  ────────────────────╮     │ pulse
 ┃ idle                   ┃┄┄│ ◉ watching 2 workers                             │     │ 22:23 ◆ spawned p2-api
 ┗━━━━━━━━━━━━━━━━━━━━━━━━┛  ╰─────────────────────────────────────────────────╯     │ 22:24 ◇ spawned p2-ui
               ┬━━━◂━━━◂━━━◂━━━◂━━━◂━━━◂━┼━━━▸━━━▸━━━▸━━━▸━━━▸━━━▸━┬                 │ 22:25 ↻ p2-auth: stream…
               ▼                         ▼                         ▼                 │ 22:25 ⚠ tripwire: p2-ui…
 ╭ ◆ p2-api ──────────⠴─╮  ╭ ◆ p2-auth ┄┄┄┄┄┄┄┄┄↻┄╮  ╭ ◇ p2-ui ───────────⠴─╮
 │ sol·high  ▰▱▱ 1/3    │  ┆ sol·high  ▰▱▱ 1/3    ┆  │ luna·medium  ▱▱▱ 0/3 │
 │ editing src/api/…    │  ┆ retrying in 4s       ┆  │ $ rg -n "fn" web     │
 ╰──────────────────────╯  ╰┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄╯  ╰──────────────────────╯
               ┆                         ┆                         ┆
               ┴─────────────────────────┼─────────────────────────┴
                 ╭ ◎ gate · qa ┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄╮
                 ┆ gate opens when all tasks are done (0/3)       ┆
```
*(real render from the headless test mode; in a terminal it's coloured and animated — rose arrows = the orchestrator is prompting a worker, green lines = being watched, dotted grey = can't run yet)*

---

## Install

One line, Linux or macOS, no `sudo`:

```bash
curl -fsSL https://raw.githubusercontent.com/sahelea1/mantra/master/install.sh | sh
# no curl? use wget:
wget -qO- https://raw.githubusercontent.com/sahelea1/mantra/master/install.sh | sh
```

It downloads and verifies (sha256) a prebuilt binary for Linux (x86_64/aarch64) or macOS (arm64/x86_64) from the latest release if one matches; otherwise it builds from source (installing a minimal `rustup` toolchain first if `cargo` is missing). Installs to `~/.local/bin` (override with `MANTRA_INSTALL_DIR`), adds that to your `PATH` if it isn't there, and finishes with `mantra doctor`. Safe to re-run to upgrade. `MANTRA_VERSION=v0.2.0` pins a release, `MANTRA_FROM_SOURCE=1` forces a build.

**You also need at least one agent CLI**, logged in:

- [Codex CLI](https://github.com/openai/codex): `npm i -g @openai/codex && codex login` — the default backend.
- [Claude Code](https://docs.anthropic.com/en/docs/claude-code): `npm i -g @anthropic-ai/claude-code && claude` (log in once) — optional, for Claude models.

Plus `git` (for isolated worktrees). From a clone: `cd mantra_src && cargo build --release && cp target/release/mantra ~/.local/bin/` (Rust ≥ 1.80). A prebuilt **Linux x86_64** binary also sits at the repo root (`mantra-linux-x86_64`, glibc ≥ 2.39).

## Quick start

```bash
mantra --demo                          # simulated agents, throwaway repo, zero API cost — explore everything safely
cd your-project && mantra              # Solo mode in this repo
mantra run "add OAuth login to the API" # straight into a Mandala run
mantra runs                            # every run: id, stage, when, goal — resume or delete from here
mantra doctor                          # codex + claude found and logged in? git? sandbox? terminal?
```

If neither CLI is installed, Mantra tells you and starts in demo mode instead of failing.

---

## Solo mode

A Claude-Code-style single agent, with the things you actually want visible:

- **Model, provider and reasoning effort always in the header** (`sol · high ▰▰▰▱▱▱ · via OpenAI (Codex) · ctx 23% · 41k tok`)
- **Live changes panel** — every file the agent touched with `+/-` counts; `ctrl+d` opens a full diff viewer
- **Plan checklist** (the agent's own plan, live), **context gauge** with the auto-compact threshold marked, session stats
- Streaming answers, collapsible reasoning (`ctrl+e` = verbose), commands with output tails, inline diffs
- Thinking indicator: breathing glyph + shimmering activity (`Thinking…`, `$ cargo test`, `editing src/x.rs`) + elapsed + tokens
- Approval card for commands/patches/permissions (`y` / `a` session / `n` / `esc`); `shift+tab` cycles the approval mode
- **Type while it works:** `⏎` **queues** your message behind the current turn (a `⏳ queued N` chip shows above the input; `backspace` on an empty input takes the last one back for editing, `ctrl+x` clears the queue) — `ctrl+f` **force-sends** it into the running turn right now, Claude-Code style

Commands (`/` shows them, `tab` completes): `/model` `/effort` `/approvals` `/new` `/compact` `/diff` `/mandala` `/run` `/runs` `/pattern` `/plan` `/pause` `/respawn` `/land` `/studio` `/models` `/inbox` `/verbose` `/help` `/quit` — and `!cmd` runs a shell command.

### Approval modes

`shift+tab` (Solo, the stage, or a zoomed agent — even while an approval card is showing) or `/approvals` cycles **untrusted → on-request → never ask** for the Solo agent:

- **untrusted** asks before anything that isn't known-safe; **on-request** asks when the agent wants to leave the sandbox.
- **never ask** means you're never prompted. The change applies to the live session immediately; anything that still asks is approved automatically and noted in the log (`✓ auto-approved (never ask): $ cargo test`). The sandbox (`sandbox` in `settings.toml`, default `workspace-write`) still applies.
- **Mandala roles have their own `permission`** (Studio; `never` by default so a team never stops to ask). A role set to `on-request`/`untrusted` sends its questions to the inbox (`ctrl+g`).
- Claude Code agents always run with `--dangerously-skip-permissions` (Claude's own permission prompts can't be answered through a pipe); their isolation comes from git worktrees and read-only roles.

## Mandala mode (`ctrl+o`)

You describe the goal. The default pattern (`mantra-default`) runs this workflow:

1. **Planner** (astra · max) explores the repo and submits a **phased plan** through a tool: phases run in sequence; tasks inside a phase run **in parallel** with declared file scopes; each phase has a gate (shell checks + QA focus). Mantra validates the plan (unique ids, valid roles, **no overlapping scopes between parallel tasks**, size limits) and bounces errors back to the planner. You review it (`a` approve, or type feedback).
2. **Orchestrator** (astra · high) spawns the phase's workers (**worker-small** = luna, **worker-big** = sol), each in its **own git worktree**. It sleeps until something happens (worker finished/failed/out of scope/stalled) and then decides: accept, steer (`mantra_prompt`), or respawn with a better prompt.
3. **Gate**: Mantra merges the worker branches, runs the checks, and the **QA agent** (terra · high) makes the merged result coherent and fixes what the checks found — up to N rounds (two identical failing reports in a row halt the run instead of burning the rest). Worker outputs and exit status are saved; worker threads are archived; the orchestrator's context is reset (or compacted) with a handoff note before the next phase.
4. **Finale**: **heavy QA** (sol · xhigh) → **security sweep** (sol · max — point it at GLM, Claude, or any other model in the Studio) → **planner verification**, which can spawn ad-hoc fix workers.
5. `/land` merges the run branch into your branch. Everything is journaled in `~/.mantra/runs/<project>-<hash>/<id>/` (plan, per-phase outputs, merge logs, journal, `state.json`) — nothing is written into your project.

**Zoomed vs overview.** A run starts **zoomed into the planner** so you watch it explore and plan: a solid role-coloured header band (`✦ planner · astra · max  zoomed · esc back to overview`) and a coloured spine down the log make it unmistakable that you're inside one agent. `esc` goes back to the **overview** (`mandala › overview`), where the whole team is visible; `⏎` on a node zooms in, `1`–`9` jump straight to the nth agent. Every switch flashes the header briefly (skipped with `reduce_motion`). The plan-review overlay opens wherever you are; approving lands you on the overview with the orchestrator selected.

**Talking to a running team:** plain text re-prompts the **planner** (it can pause agents, revise the plan, or brief the orchestrator); `@p2-api use axum` messages one agent directly — `⏎` queues it behind that agent's turn, `ctrl+f` forces it in right away, same as Solo.

**Stage keys:** `tab` typing ⇄ navigating · `←→↑↓` (or `alt+←→` while typing) select · `1`-`9` jump + zoom · `⏎` zoom (`esc` back) · `space` pause/resume the run · `r` respawn the selected agent in place (planner, orchestrator, gate, finale or worker — `ctrl+r` does it from anywhere, `/respawn` too) · `m` switch the selected agent's model · `x` interrupt · `c` compact · `+/-` effort · `p` plan · `a` approve · `d` diff · `s` studio · `ctrl+t` hide/show the pulse feed · `ctrl+g` inbox (approvals + alerts from all agents).

**Everywhere:** `ctrl+k` model picker · `alt+↑/↓` effort · `ctrl+d` diff · `ctrl+e` verbose · `ctrl+f` force-send · `ctrl+t` (or `F2`) side panel · `ctrl+l` redraw · `ctrl+c` clear → interrupt → quit.

### Halts (there is no "pause" badge any more)

When a run can't continue by itself it **halts** with a reason, shown as a full-width amber band under the stage header — `⛔ halted 12s · qa (sol via zai): Unexpected message role · m switch model for qa · r retry` — and a desktop notification. Reasons and what fixes them:

| halt | what it means | the band tells you to |
|---|---|---|
| `paused by you` | `space` | `space` to resume |
| auth / usage limit | 401/403/quota from the provider | fix credentials or quota, then `space` |
| provider rejected | HTTP 400/422, e.g. a gateway that refuses Codex's `developer` messages | `m` switch that role's model (the run resumes by itself), or run the model through Claude Code |
| environment | a worker command died in Codex's sandbox (`bwrap`, user namespaces) | fix the machine (`mantra doctor` prints the exact sysctl) or set `sandbox = "danger-full-access"`, then `r` |
| gate exhausted | the QA gate failed its rounds, or kept reporting the same blocker | type feedback for the planner, or `space` to retry the gate |
| attempts exhausted | a task used every attempt | `r` retry it, or feedback for the planner |
| agent turn failed | a planner/orchestrator/gate turn failed past its retries and one free respawn | `r` respawn · `space` retries the turn |

### The watchdog

A run must never sit idle. Mantra knows who *should* be busy at every moment (planning, orchestrating, working a task, gating, a finale step) and escalates when they aren't:

1. idle for `watchdog_seconds` (90 s by default): the agent gets a nudge (`[mantra:watchdog]`);
2. idle for `watchdog_escalate_seconds` (240 s): the **orchestrator is respawned** (or woken about another idle agent);
3. twice that: the **planner is woken** to sort it out;
4. still nothing: the run halts (`agent turn failed`) instead of pretending.

Every step is journaled with `⏰`. Tripwires (out-of-scope edits, stalls, token budgets) wake the orchestrator as before; the orchestrator prompting the same stuck worker three times in five minutes gets that worker respawned instead of a fourth message into the void; and a worker whose task is already done can't be prompted at all once the phase has moved on. Both timeouts are pattern settings (Studio → settings).

## Runs: list, resume, delete

Every run writes a typed `state.json` at each transition, so a Mantra that was killed, crashed or closed can pick a run back up:

```bash
mantra runs                        # id · project · stage · updated · goal, newest first (all projects)
mantra runs resume 175917          # an id prefix is enough; works from any directory
mantra runs delete 175917 [--yes]  # worktrees, mantra/<id> + mantra-w/<id>/* branches, journal
mantra --resume-last               # the most recent unfinished run (with --demo: the last demo run)
```

Inside Mantra, `/runs` lists this project's runs (`⏎` resume, `D` delete with a confirmation), and both welcome screens say `↻ 1 unfinished run — /runs` when there is something to pick up. A resume restarts at the nearest safe boundary: the planner is re-attached to its thread while planning; a saved plan goes straight back to review; during a phase a **fresh orchestrator** is briefed with the current status and running workers are **re-attached to their threads** when their worktree still exists (otherwise re-spawned with the same prompt); merges, checks and gates simply run again; finale steps start over; finished runs open read-only so `/land` still works.

## Backends, models, effort and context (`/models`)

`~/.mantra/models.toml` maps short aliases to a model **and the provider it runs through**. The same model id can exist twice — say `claude-sonnet-5` through your Claude subscription and through OpenRouter — and every picker shows which is which (`sonnet5 · claude-sonnet-5 · via Claude Code`). Every field is editable in the TUI (`⏎` edit, `+/-` step context/compact%/default effort, `t` test a model live, `D` discover models, `ctrl+s` save):

```toml
[[model]]
alias = "sol"
provider = "openai"              # a [[provider]] id; "openai" = Codex's own login
model = "gpt-5.6-sol"
context_window = 272000          # → Codex model_context_window / Claude --autocompact
auto_compact_percent = 85        # → Codex model_auto_compact_token_limit
default_effort = "medium"
efforts = ["low", "medium", "high", "xhigh", "max", "ultra"]
```

Effort is set **per model** (default), **per role** (pattern), and **per agent at runtime** (`alt+↑/↓`, `+/-` on the stage, `/effort`, `ctrl+k` picker) — changes apply from the agent's next turn. The `ctrl+k` picker also lets you bump a model's context window on the spot (`+/-`, `c` to type it) without opening `/models`.

### Claude Code as a backend

If `claude` is on your `PATH`, Mantra adds a `claude` provider (kind `claude-code`, auth `subscription`) and these aliases: `opus46` claude-opus-4-6 · `opus48` claude-opus-4-8 · `opus5` claude-opus-5 · `sonnet5` claude-sonnet-5 · `fable5` claude-fable-5 · `fable51` claude-fable-5-1, plus `opus5-1m`/`sonnet5-1m` for the `[1m]` long-context variants. Give any role one of them in the Studio (`model` → `←/→`) — planner and orchestrator are the natural fit — or pick one in Solo with `ctrl+k`.

How it runs: one `claude -p --input-format stream-json --output-format stream-json --verbose --dangerously-skip-permissions --session-id <uuid> --model <m> [--effort e] --autocompact <ctx>` process per agent, prompts on stdin, events translated into the same shapes the UI already renders (streaming text, tool calls, compaction, token usage). Mid-turn messages arrive at the next tool boundary (that's `ctrl+f`), `x` sends a real interrupt, `/compact` compacts the session, a crashed process is restarted with `--resume`, and Mantra's own tools (`mantra_submit_plan`, `mantra_spawn`, …) reach the agent through a tiny MCP server (`mantra mcp-bridge`) that Mantra starts for it. Running as root needs `IS_SANDBOX=1` in the environment (Claude Code's rule for `--dangerously-skip-permissions`); `mantra doctor` says so.

**Third-party Anthropic-compatible providers** (LibertAI, OpenRouter, a company gateway) also go through Claude Code: add a provider with `kind = "claude-code"`, `auth = "api_key"`, the gateway's base URL and a key. Claude then talks to `ANTHROPIC_BASE_URL` with `ANTHROPIC_API_KEY`, in `--bare` mode (no login, no managed settings):

```toml
[[provider]]
id = "libertai-claude"
name = "LibertAI (via Claude Code)"
kind = "claude-code"
auth = "api_key"
base_url = "https://api.libertai.io"     # bare host; Mantra strips a trailing /v1
env_key = "LIBERTAI_API_KEY"             # or api_key = "…" to store it (0600)

[[model]]
alias = "cc-qwen"
provider = "libertai-claude"
model = "qwen3.8-27b"
context_window = 262000
auto_compact_percent = 85
```

This is also the way around gateways that reject Codex's `developer` role: the same model, same key, through Claude Code instead.

### Custom OpenAI-compatible providers (Codex)

In `/models`, `tab` to the providers table, `n` to add one:

| field | meaning |
|---|---|
| `id` | short name you'll see next to its models (e.g. `zai`) |
| `name` | display name |
| `kind` | `codex` (default: the provider must speak the OpenAI **Responses** API — Codex calls `…/responses`) or `claude-code` (above) |
| `base_url` | the API root, usually ending in `/v1` for Codex; discovery reads `…/models` |
| `env_key` | the **name** of the environment variable holding the key (e.g. `ZAI_API_KEY`); `✓ key set (env)` shows when it's present |
| `api_key` | *(optional, alongside `env_key`)* paste the key directly instead — written to `models.toml` (`chmod 0600`), delivered to each agent's process environment only, never on argv or in logs; shown masked (`••••1234`) |

Then press **`D`** on that provider (or `D` on the models table to discover everything, including Codex's own catalog and the Claude defaults). Mantra fetches `GET {base_url}/models` with your key and opens a picker (type to filter, `space` select, `tab` all/none, `⏎` add & save):

- **Context:** read from whatever the provider reports (`context_length`, `context_window`, `max_model_len`, `max_input_tokens`, …). If it reports none, **200k is assumed** (shown as "assumed"; conservative so auto-compaction never aims past a real limit) — edit it any time. New models get auto-compaction at 85%.
- **Reasoning effort:** enabled (low/medium/high) when the provider says the model supports reasoning, or when the id says so (`glm-5.3-thinking`); otherwise no effort is sent at all, so strict gateways don't reject requests.
- Embedding/audio/image models are filtered out; big catalogues start unselected so you can filter and pick; models you already have are marked `configured` and never duplicated. Errors (wrong key, unreachable URL) are shown per provider.
- **`t` tests a model live** before you trust it with a role — including a `developer` message, so a gateway that rejects that role fails here, not mid-run. Starting a run also pre-flights every role's provider (missing key, unknown alias) and refuses with the exact variable name instead of spending a turn.

## Context & compaction

Every agent shows how full its context window is (side panel gauge, `ctx %` in the header and on cards). The gauge marks the model's auto-compact threshold (`┊`), turns amber/red as it fills, and Mantra warns you once at 85%.

- **Always on:** every agent is launched with an explicit context window and compaction threshold — the model's own numbers, or an assumed 200k / 85% (shown dim as `(assumed)`/`(default)` in `/models`) when it has none. Codex gets `model_context_window` + `model_auto_compact_token_limit`; Claude gets `--autocompact`. A worker that still hits a context-full error while running on the assumed 200k gets that assumption halved for its next attempt, and the log tells you to set the real window.
- **Manual:** `/compact` (Solo, zoom) or `c` on the stage. If the agent is mid-turn, it compacts right after the turn instead of erroring.
- Each compaction appears once in the log as `── ⇣ context compacted · 182k → 31k tokens ──`, and the gauge drains smoothly.
- In Mandala runs: the orchestrator is reset (or compacted) between phases; workers are short-lived and archived. Compaction turns never count as "the agent finished its work".

## Patterns & the Studio (`/studio` or `s`)

A pattern is a TOML file: roles (kind, glyph, colour, model, effort, sandbox, **permission**, token budget, instructions) + settings (isolation, max parallel, retries, plan review, stall timeout, gate rounds, **watchdog timeouts**…) + flow (who plans, orchestrates, gates, the finale chain, who handles re-prompts). The Studio edits all of it with live validation and a flow preview — selection follows the role, not its position, so changing a role's kind never lands the cursor on another role — or **tell the architect agent** what you want ("add a docs writer after security", "make workers cheaper", "put the planner on Claude") and it edits the pattern through tools while you watch. `ctrl+s` saves to `~/.mantra/patterns/`. (A repo can also ship patterns in `<repo>/.mantra/patterns/`, which take precedence.) `/pattern` switches the pattern for new runs.

## Resilience

- One process per agent: a crash restarts only that agent (backoff, max 5 per 10 min) and **resumes its thread/session**; the journal line carries the exit code and the last useful stderr line (Codex's known chatter is filtered).
- Codex/Claude already retry dropped streams; Mantra adds turn-level retries for transient failures (overload, disconnects, rate limits) with backoff, compacts on context-full, and **halts with a reason** on auth/usage-limit/provider-rejection/environment errors or when a planner/orchestrator/gate turn keeps failing — it never loops on errors. Every task has a hard attempt cap across all retry paths.
- The watchdog (above) restarts what stopped; tripwires (out-of-scope edits, stalls, token budgets) wake the orchestrator.
- If a worktree can't be created, the worker falls back to the shared integration copy instead of failing.
- Mantra itself dying is not fatal: `mantra runs resume <id>`.
- Startup checks the Linux sandbox (unprivileged user namespaces) once; if Codex's bubblewrap can't run here, both welcome screens say so, every run's pulse repeats it, and `mantra run` asks before starting (`--no-sandbox-check` skips the question). At runtime the first `bwrap` failure halts the run instead of feeding gate rounds.

## tmux

Mantra is tested inside tmux 3.4 (real sessions: typing, approvals, `alt`/`ctrl` keys, live pane resizing, clean exit). Recommended `~/.tmux.conf` lines, and `mantra doctor` checks them for you:

```tmux
set -sg escape-time 10                  # otherwise Esc (interrupt / back) lags by 500 ms — Mantra warns at startup
set -g default-terminal "tmux-256color"
set -as terminal-features ",*:RGB"      # truecolor (256 colours work fine without it)
set -g allow-passthrough on             # desktop notifications from inside tmux
```

- The side-panel toggle is `ctrl+t`, so it doesn't collide with tmux's `ctrl+b` prefix.
- Notifications inside tmux also ring the bell, so tmux flags the window in your status bar even without passthrough.
- The terminal/pane title shows live progress (`◉ mantra — phase 2/3 · 3 active`, `⚑ … waiting for you`).
- With `mouse = true` (default) Mantra receives the wheel; hold `shift` to select text with the mouse, or set `mouse = false` in `settings.toml` to keep tmux's own mouse handling.
- Small panes work: the stage drops the peek strip and compacts cards, down to 40×12.

## Terminal compatibility

Linux and macOS terminals (kitty, WezTerm, Ghostty, Alacritty, iTerm2, Terminal.app, GNOME Terminal/VTE, Konsole, foot, tmux/screen, the Linux console). Colour depth is detected (truecolor → 256 → 16) and glyphs fall back to ASCII where Unicode isn't available. Override in `settings.toml`: `colors = "truecolor"|"256"|"16"`, `glyphs = "unicode"|"ascii"`, `reduce_motion = true`, `mouse = false`, `fps = 12`. Frames use synchronized output (no tearing in terminals/tmux that support it; ignored elsewhere). Idle UI renders at 0 fps, and nothing animates while a run is halted or waiting for plan review. In ASCII mode every cell is guaranteed pure ASCII (safe for non-UTF-8 locales). Desktop notifications use OSC 9 (`notify = false` to disable). Minimum size 40×12. A rendering bug can't take the app down: a panic inside drawing is caught and logged while agents keep running.

## Files

| path | what |
|---|---|
| `~/.mantra/settings.toml` | `codex_command`, `claude_command`, default model/pattern, approvals, sandbox, UI options (`$MANTRA_HOME` overrides the dir) |
| `~/.mantra/models.toml` | models, context, efforts, providers (`0600` when a key is stored in it) |
| `~/.mantra/patterns/*.toml` | your patterns |
| `~/.mantra/runs/<project>-<hash>/<id>/` | `state.json`, plan, journal, per-phase outputs, merge logs |
| `~/.mantra/worktrees/<id>/` | per-run worktrees (cleaned up as phases complete, or by `mantra runs delete`) |
| `~/.mantra/run/` | Unix sockets for the Claude Code MCP bridge (per Mantra process) |
| `~/.mantra/logs/mantra.log` | debug log |

Everything lives in your home directory; nothing is written into your projects. Upgrading from an older build: settings, models and patterns in `~/.config/mantra` are copied to `~/.mantra` on first start. v0.1 run journals still list in `mantra runs` (marked `v0.1 format`) and can be deleted, but not resumed.

## Status & known limitations (honest)

- **Tested for v0.2:** 60+ unit tests (engine state machine, halts, watchdog ladder, resume at every stage, queueing, registry/providers/keys, discovery, Claude event translation against a recorded stream, MCP bridge round-trip, layout); `scripts/stress.sh` renders every screen and overlay at 13 sizes through a whole simulated run, the halt band, zoom-vs-overview, leave → `runs` → resume → delete, and the sandbox notice, in the debug build where arithmetic overflow panics; real tmux 3.4 sessions; ASCII/16-colour mode. **Real runs completed** against LibertAI (`scripts/live-env.sh` sets it up): Solo and full Mandala runs (planner → parallel workers → gate → finale) with **Codex 0.154.0**, compaction on a 16k-context model, and Claude Code **2.1.269** driven headlessly (multi-turn, mid-turn steering, interrupt, `/compact`, `--resume`, MCP tools).
- Claude Code with a **subscription** login was verified by the author on their machine only (this build machine can't hold a login); `api_key` mode was verified live against LibertAI.
- macOS: builds and is used from source by the author; not part of the automated matrix. Windows is not supported.
- MCP elicitation requests are auto-declined (noted in the agent log).
- Prompt tuning on your first real runs is normal (planner JSON quality, orchestrator brevity) — the watchdog and halts are there so a bad turn costs a nudge, not an evening.
