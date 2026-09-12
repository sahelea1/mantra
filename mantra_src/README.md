# ✦ Mantra

One terminal for OpenAI Codex agents — work with a single agent (**Solo**), or hand a goal to a whole team that plans it, builds it in parallel phases and checks it (**Mandala**).

Mantra is a single ~3.5 MB Rust binary. It drives the official `codex app-server` (JSON-RPC over stdio), one process per agent, so every model/provider Codex supports works, crashes stay isolated, and your Codex login, sandbox and approvals keep working exactly as in Codex.

```
 ✦ mantra  mandala  89735-build-a-todo-api  ◈ mantra-default                  ⏱ 10s  Σ 33.5k tok  ● 2 active
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

**Requirements:** [Codex CLI](https://github.com/openai/codex) (`npm i -g @openai/codex`, then `codex login`), `git`, and a Rust toolchain ≥ 1.80 to build.

```bash
tar xzf mantra-src.tar.gz && cd mantra
cargo build --release              # ~3 min
cp target/release/mantra ~/.local/bin/    # or: cargo install --path .
mantra doctor                      # checks codex, login, git, terminal colours/glyphs
```

A prebuilt **Linux x86_64** binary is included (`mantra-linux-x86_64`, needs glibc ≥ 2.39: Ubuntu 24.04+, Debian 13, Fedora 40+ — otherwise build from source). Install it with `chmod +x mantra-linux-x86_64 && mv mantra-linux-x86_64 ~/.local/bin/mantra`. On **macOS** build from source (same commands; Apple Silicon and Intel both work with rustup's toolchain).

## Quick start

```bash
mantra --demo          # simulated agents, throwaway repo, zero API cost — explore everything safely
cd your-project && mantra                      # Solo mode in this repo
mantra run "add OAuth login to the API"        # straight into a Mandala run
```

If `codex` isn't installed, Mantra tells you and starts in demo mode instead of failing.

---

## Solo mode

A Claude-Code-style single agent, with the things you actually want visible:

- **Model + reasoning effort always in the header** (`sol · high ▰▰▰▱▱▱ · ctx 23% · 41k tok`)
- **Live changes panel** — every file the agent touched with `+/-` counts; `ctrl+d` opens a full diff viewer
- **Plan checklist** (the agent's own plan, live), **context gauge** with the auto-compact threshold marked, session stats
- Streaming answers, collapsible reasoning (`ctrl+e` = verbose), commands with output tails, inline diffs
- Thinking indicator: breathing glyph + shimmering activity (`Thinking…`, `$ cargo test`, `editing src/x.rs`) + elapsed + tokens
- Approval card for commands/patches/permissions (`y` / `a` session / `n` / `esc`); `shift+tab` cycles the approval mode (see below)
- Type while it works → your message **steers the running turn**

Commands (`/` shows them, `tab` completes): `/model` `/effort` `/approvals` `/new` `/compact` `/diff` `/mandala` `/run` `/pattern` `/plan` `/pause` `/land` `/studio` `/models` `/inbox` `/verbose` `/help` `/quit` — and `!cmd` runs a shell command.

### Approval modes

`shift+tab` (Solo, the stage, or a zoomed agent — even while an approval card is showing) or `/approvals` cycles **untrusted → on-request → never ask**:

- **untrusted** asks before anything that isn't known-safe; **on-request** asks when the agent wants to leave the sandbox.
- **never ask** means you're never prompted. The change applies to the live session immediately (Codex `thread/settings/update`); anything that still asks — a turn that started under another mode, a waiting card, or a Mandala agent — is approved automatically and noted in the log (`✓ auto-approved (never ask): $ cargo test`). The sandbox (`sandbox` in `settings.toml`, default `workspace-write`) still applies.
- Mandala agents never stop to ask by design; in the other modes, anything they do ask lands in the inbox (`ctrl+g`).

## Mandala mode (`ctrl+o`)

You describe the goal. The default pattern (`mantra-default`) runs this workflow:

1. **Planner** (astra · max) explores the repo and submits a **phased plan** through a tool: phases run in sequence; tasks inside a phase run **in parallel** with declared file scopes; each phase has a gate (shell checks + QA focus). Mantra validates the plan (unique ids, valid roles, **no overlapping scopes between parallel tasks**, size limits) and bounces errors back to the planner. You review it (`a` approve, or type feedback).
2. **Orchestrator** (astra · high) spawns the phase's workers (**worker-small** = luna, **worker-big** = sol), each in its **own git worktree**. It sleeps until something happens (worker finished/failed/out of scope/stalled) and then decides: accept, steer (`mantra_prompt`), or respawn with a better prompt.
3. **Gate**: Mantra merges the worker branches, runs the checks, and the **QA agent** (terra · high) makes the merged result coherent and fixes what the checks found — up to N rounds. Worker outputs and exit status are saved; worker threads are archived; the orchestrator's context is reset (or compacted) with a handoff note before the next phase.
4. **Finale**: **heavy QA** (sol · xhigh) → **security sweep** (sol · max — point it at GLM or any other model in the Studio) → **planner verification**, which can spawn ad-hoc fix workers.
5. `/land` merges the run branch into your branch. Everything is journaled in `~/.mantra/runs/<project>-<hash>/<id>/` (plan, per-phase outputs, merge logs, journal) — nothing is written into your project.

**Talking to a running team:** plain text re-prompts the **planner** (it can pause agents, revise the plan, or brief the orchestrator); `@p2-api use axum` messages one agent directly.

**Stage keys:** `tab` switches between typing and navigating · `←→↑↓` (or `alt+←→` while typing) select · `⏎` zoom into an agent (full log, tools, thinking, diff; `esc` back) · `space` pause/resume everything · `r` retry worker / restart crashed agent · `x` interrupt · `c` compact · `+/-` effort · `p` plan · `d` diff · `s` studio · `ctrl+t` hide/show the pulse feed · `ctrl+g` inbox (approvals + alerts from all agents).

**Everywhere:** `ctrl+k` model picker · `alt+↑/↓` effort · `ctrl+d` diff · `ctrl+e` verbose · `ctrl+t` (or `F2`) side panel · `ctrl+l` redraw · `ctrl+c` clear → interrupt → quit.

## Models, effort and context (`/models`)

`~/.mantra/models.toml` maps short aliases to models. Every field is editable in the TUI (`⏎` edit, `+/-` step context/compact%/default effort, `t` test a model live, `D` discover models, `ctrl+s` save):

```toml
[[model]]
alias = "sol"
provider = "openai"
model = "gpt-5.6-sol"
context_window = 272000          # passed to Codex as model_context_window
auto_compact_percent = 85        # → model_auto_compact_token_limit
default_effort = "medium"
efforts = ["low", "medium", "high", "xhigh", "max", "ultra"]
```

Effort is set **per model** (default), **per role** (pattern), and **per agent at runtime** (`alt+↑/↓`, `+/-` on the stage, `/effort`, `ctrl+k` picker) — changes apply from the agent's next turn.

**Other providers** (e.g. GLM for the security sweep): in `/models`, `tab` to the providers table, `n` to add one, and fill in four fields:

| field | meaning |
|---|---|
| `id` | short name you'll see next to its models (e.g. `zai`) |
| `name` | display name |
| `base_url` | the provider's OpenAI-compatible API root, usually ending in `/v1`. Codex calls `…/responses` (Codex requires the OpenAI **Responses** API); discovery reads `…/models` |
| `env_key` | the **name** of the environment variable that holds the API key (e.g. `ZAI_API_KEY`). The key itself is never stored; `✓ key set` shows when it's present |

Then press **`D`** on that provider (or `D` on the models table to discover everything, including Codex's own catalog). Mantra fetches `GET {base_url}/models` with your key and opens a picker (type to filter, `space` select, `tab` all/none, `⏎` add & save):

- **Context:** read from whatever the provider reports (`context_length`, `context_window`, `max_model_len`, `max_input_tokens`, `inputTokenLimit`, …). If it reports none, **200k is assumed** (shown as "assumed"; conservative so auto-compaction never aims past a real limit) — edit it any time. New models get auto-compaction at 85%.
- **Reasoning effort:** enabled (low/medium/high) only when the provider says the model supports reasoning; otherwise no effort is sent at all, so strict gateways don't reject requests. You can type efforts in yourself later.
- Embedding/audio/image models are filtered out; big catalogues (OpenRouter…) start unselected so you can filter and pick; models you already have are marked `configured` and never duplicated. Errors (wrong key, unreachable URL) are shown per provider.

The same thing by hand:

```toml
[[provider]]
id = "zai"
name = "Z.ai"
base_url = "https://your-gateway.example/v1"
env_key = "ZAI_API_KEY"

[[model]]
alias = "glm"
provider = "zai"
model = "glm-5.2"
context_window = 200000
auto_compact_percent = 85
default_effort = "high"
efforts = ["low", "medium", "high"]
```
Then in the Studio: select `security` → `model` → `←/→` to `glm`.

## Claude Code agents

Mantra can run any role — planner, orchestrator, workers, gates — on a locally installed `claude` (Claude Code) instead of Codex, with the same everything: status, tokens, stop/resume, crash restart, and the `mantra_*` tools workers and orchestrators use (exposed to Claude as MCP tools, `mcp__mantra__mantra_*`, over a small bridge Mantra spawns automatically — nothing to configure).

**Install.** `npm i -g @anthropic-ai/claude-code`, then either `claude` once to log into your subscription, or use a third-party gateway with an API key (below). When `claude` is on `PATH`, Mantra adds a built-in **`claude`** provider automatically with six models:

| alias | model | | alias | model |
|---|---|---|---|---|
| `opus46` | `claude-opus-4-6` | | `sonnet5` | `claude-sonnet-5` |
| `opus48` | `claude-opus-4-8` | | `fable5` | `claude-fable-5` |
| `opus5` | `claude-opus-5` | | `fable51` | `claude-fable-5-1` |

plus `opus5-1m` / `sonnet5-1m` (1M context — needs an eligible plan). Point any role at one of these in the Studio (`model` field) exactly like a Codex model.

**Subscription vs. API key.** In `/models` → `tab` to providers, the `claude` row has two extra columns, `kind` and `auth` (cycle either with `+`/`-`):

- `auth = subscription` (the default): no key needed — `claude` uses your OAuth login. The provider row shows "subscription (OAuth login)" instead of a key check.
- `auth = api_key`: set `base_url` (a third-party Anthropic-compatible gateway, e.g. `https://api.libertai.io` — no trailing `/v1`, unlike Codex's custom providers) and `env_key` (the environment variable holding the key). Press **`D`** on that provider: the six defaults above are always listed (there's no `/models` endpoint for a subscription), plus whatever `GET {base_url}/v1/models` returns — so a third-party model like `qwen3.8-27b` via Claude Code is one keypress away.

Add a second provider of `kind = ClaudeCode` (`n` on the providers table, then cycle `kind`) to mix a subscription and a gateway, or several gateways.

**Which roles.** Any role can use a Claude model — planner and orchestrator are the best fit today (they lean on judgment and tool calls); workers and gates work too. A pattern can mix backends freely: `mantra-default-claude` (a built-in pattern, same shape as `mantra-default`) runs planner/orchestrator on `fable51` and workers on `sonnet5` while gates stay on Codex, to show the mix works.

**Permission and sandbox.** Mantra maps a role's `permission`/`sandbox` (Studio) onto Claude's own flags: `permission = never` → `--dangerously-skip-permissions` (the only mode fully supported today — every Claude agent runs unattended); `sandbox = read-only` restricts to `Read,Glob,Grep,WebFetch`, `workspace-write` adds the agent's own cwd as a writable root, `danger-full-access` adds `/`. Claude agents can't be asked for approval mid-turn the way Codex agents can — a denied action is reported in the turn's `permission_denials` instead, so keep `permission = never` unless you're prepared to read that back.

**Running as root.** `--dangerously-skip-permissions` is refused for uid 0 unless `IS_SANDBOX=1` is set — Mantra sets it on the child automatically when it detects it's running as root, and `mantra doctor` prints a note when it does.

`mantra doctor` also checks `claude --version` (5s timeout — a `claude` stuck waiting on a subscription login in a sandboxed environment must never hang doctor), and per Claude provider: a subscription shows `claude auth status` (when that subcommand exists) or a reminder to log in; an `api_key` provider shows the usual `$VAR set/NOT set`.

## Context & compaction

Every agent shows how full its context window is (side panel gauge, `ctx %` in the header and on cards). The gauge marks the model's auto-compact threshold (`┊`), turns amber/red as it fills, and Mantra warns you once at 85%.

- **Automatic, always on:** Mantra always tells Codex a `model_context_window` and `model_auto_compact_token_limit` for every agent, so compaction never depends on a provider's own (unreliable) defaults. Set `context_window` + `auto_compact_percent` per model (`/models`, `+/-` to step) to be exact; leave them unset and Mantra assumes a conservative 200k / 85% (shown dim as `(assumed)` / `(default)` in `/models`).
- If a worker still hits a context-full error while running on the *assumed* 200k, Mantra halves the assumption for that task's next attempt and logs why — set the model's real `context_window` in `/models` to stop the guessing.
- **Manual:** `/compact` (Solo, zoom) or `c` on the stage. If the agent is mid-turn, it compacts right after the turn instead of erroring.
- Each compaction appears once in the log as `── ⇣ context compacted · 182k → 31k tokens ──`, and the gauge drains smoothly.
- In Mandala runs: context-full errors trigger a compaction and a retry; the orchestrator is reset (or compacted) between phases; workers are short-lived and archived. Compaction turns never count as "the agent finished its work".

## Patterns & the Studio (`/studio` or `s`)

A pattern is a TOML file: roles (kind, glyph, colour, model, effort, sandbox, token budget, instructions) + settings (isolation, max parallel, retries, plan review, stall timeout, gate rounds…) + flow (who plans, orchestrates, gates, the finale chain, who handles re-prompts). The Studio edits all of it with live validation and a flow preview — or **tell the architect agent** what you want ("add a docs writer after security", "make workers cheaper") and it edits the pattern through tools while you watch. `ctrl+s` saves to `~/.mantra/patterns/`. (A repo can also ship patterns in `<repo>/.mantra/patterns/`, which take precedence — Mantra reads that folder if it exists but never creates it.) `/pattern` switches the pattern for new runs.

## Resilience

- One `codex app-server` per agent: a crash restarts only that agent (backoff, max 5 per 10 min) and **resumes its thread**.
- Codex already retries dropped streams; Mantra adds turn-level retries for transient failures (overload, disconnects, rate limits) with backoff, compacts on context-full, and **pauses the run with an alert** on auth/usage-limit/forbidden errors or when a planner/orchestrator/gate turn fails — it never loops on errors. Every task has a hard attempt cap across all retry paths.
- Tripwires: out-of-scope edits, stalls and token budgets wake the orchestrator.
- If a worktree can't be created, the worker falls back to the shared integration copy instead of failing.

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

Linux and macOS terminals (kitty, WezTerm, Ghostty, Alacritty, iTerm2, Terminal.app, GNOME Terminal/VTE, Konsole, foot, tmux/screen, the Linux console). Colour depth is detected (truecolor → 256 → 16) and glyphs fall back to ASCII where Unicode isn't available. Override in `settings.toml`: `colors = "truecolor"|"256"|"16"`, `glyphs = "unicode"|"ascii"`, `reduce_motion = true`, `mouse = false`, `fps = 12`. Frames use synchronized output (no tearing in terminals/tmux that support it; ignored elsewhere). Idle UI renders at 0 fps, and nothing animates while a run is paused or waiting for plan review. In ASCII mode every cell is guaranteed pure ASCII (safe for non-UTF-8 locales). Desktop notifications use OSC 9 (`notify = false` to disable). Minimum size 40×12. A rendering bug can't take the app down: a panic inside drawing is caught and logged while agents keep running.

## Files

| path | what |
|---|---|
| `~/.mantra/settings.toml` | codex command, default model/pattern, approvals, UI options (`$MANTRA_HOME` overrides the dir) |
| `~/.mantra/models.toml` | models, context, efforts, providers |
| `~/.mantra/patterns/*.toml` | your patterns |
| `~/.mantra/runs/<project>-<hash>/<id>/` | plan, journal, per-phase outputs |
| `~/.mantra/worktrees/` | per-run worktrees (cleaned up as phases complete) |
| `~/.mantra/logs/mantra.log` | debug log |

Everything lives in your home directory; nothing is written into your projects. Upgrading from an older build: settings, models and patterns in `~/.config/mantra` are copied to `~/.mantra` on first start (the old folder is left alone — delete it when you like).

## Status & known limitations (honest)

- **Tested:** 25 unit tests; end-to-end approval-mode switching (with a card showing, mid-turn, and back again); provider discovery against a local fake provider with the **real Codex 0.154.0** sending `/responses` requests to it (effort sent only to the reasoning model, key taken from the env var); home-dir migration; `scripts/stress.sh` renders every screen and overlay at 13 sizes (40×12 … 320×90) through a full simulated run, in the debug build where arithmetic overflow panics (it found and fixed 2 real crashes); real tmux 3.4 sessions; ASCII/16-colour mode; full end-to-end runs (Solo, Mandala through all phases + finale, re-prompt, `@agent`, pause/resume, zoom, retry, tripwire, `/land`, Studio + architect, Models + live test) against a protocol-faithful simulated Codex; and against the **real Codex 0.154.0** binary for process startup, thread creation with dynamic tools and `-c` config, and API-error handling. My build machine had no API access, so **no real model turns were completed** — expect some prompt tuning on your first real runs (planner JSON quality, orchestrator verbosity).
- macOS: builds from source; not run on a Mac by me.
- MCP elicitation requests are auto-declined (noted in the agent log). Windows is not supported.
- The attempt-cap and pause-on-failure paths are covered by logic + the real-Codex 403 test, but not by the demo script.
