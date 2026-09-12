<div align="center">

<img src="docs/img/banner.png" alt="Mantra" width="620">

**Run one coding agent — or a whole team of them — from a single terminal.**

Works with **OpenAI Codex** and **Claude Code**, mixed freely, on any model either of them can reach.
One Rust binary, no server, no web UI, nothing written into your project.

[Install](#install) · [Try it in one minute](#try-it-in-one-minute) · [Solo](#solo-one-agent-done-properly) · [Mandala](#mandala-a-team-that-plans-builds-and-checks) · [Under the hood](#under-the-hood)

<img src="docs/img/mandala.png" alt="A Mantra run: three workers building one phase in parallel, a gate waiting, the live pulse feed" width="920">

</div>

---

## What it is

You already have a coding agent in your terminal. Mantra is the room you put it in — and the room fits more than one.

|  | what you get |
|---|---|
| **Solo** | One agent, with everything you actually want on screen: the diff it is writing, its plan, how full its context is, what it is running right now. Type while it works. |
| **Mandala** | A whole team for one goal: a planner writes a phased plan, an orchestrator runs each phase, workers build **in parallel** in their own git worktrees, a QA gate merges and checks their work, and a finale does heavy QA, a security sweep and a final verification. |

Mantra does not reimplement an agent loop. It drives the official CLIs — `codex app-server` and `claude -p` — one process per agent. Your logins, sandboxes and model access keep working exactly as they do today; you just get a room where several of them can work at once without stepping on each other.

**Why you might want it**

- **Parallelism that is actually safe.** Every worker gets its own git worktree and a declared file scope. Mantra rejects a plan whose parallel tasks overlap, and trips a wire if a worker edits outside its scope.
- **Nothing silently stalls.** A watchdog knows who should be busy and nudges, respawns or escalates when they are not. Failures stop the run with a reason and the key that fixes it, instead of looping.
- **It survives being closed.** Runs are saved as they go; `mantra runs resume <id>` picks one back up where it stopped.
- **One place for models.** Aliases map to a model *and* the backend it runs through, so `sonnet5` via your Claude subscription and the same model via a gateway are two different, clearly labelled things.

---

## Install

One line, Linux or macOS, no `sudo`:

```bash
curl -fsSL https://raw.githubusercontent.com/sahelea1/mantra/master/install.sh | sh
```

<details>
<summary>No curl, pinning a version, or building from source</summary>

```bash
# wget instead of curl
wget -qO- https://raw.githubusercontent.com/sahelea1/mantra/master/install.sh | sh

# pin a release, or force a source build
MANTRA_VERSION=v0.2.0 sh -c "$(curl -fsSL https://raw.githubusercontent.com/sahelea1/mantra/master/install.sh)"
MANTRA_FROM_SOURCE=1  sh -c "$(curl -fsSL https://raw.githubusercontent.com/sahelea1/mantra/master/install.sh)"

# from a clone (Rust ≥ 1.80)
cd mantra_src && cargo build --release && cp target/release/mantra ~/.local/bin/
```

The installer verifies a sha256, installs to `~/.local/bin` (override with `MANTRA_INSTALL_DIR`), adds that directory to your `PATH` if it is missing, and finishes by running `mantra doctor`. Re-running it upgrades in place. If no release binary matches your platform it builds from source, installing a minimal `rustup` toolchain first when `cargo` is absent.

</details>

**You also need at least one agent CLI, logged in:**

| CLI | install | notes |
|---|---|---|
| [Codex](https://github.com/openai/codex) | `npm i -g @openai/codex` then `codex login` | the default backend |
| [Claude Code](https://docs.anthropic.com/en/docs/claude-code) | `npm i -g @anthropic-ai/claude-code` then `claude` once | optional — for Claude models |

Plus `git`, for the isolated worktrees. `mantra doctor` checks all of it and tells you exactly what is missing.

---

## Try it in one minute

```bash
mantra --demo
```

Demo mode replaces both CLIs with a simulated one: real UI, real state machine, scripted agents, **no API calls and no cost**. Press `ctrl+o`, type a goal, and watch a full team run play out in about twenty seconds. Every screenshot in this README is demo mode, captured by `docs/tools/shoot.py`.

```bash
cd your-project && mantra                       # Solo, in this repo
mantra run "add OAuth login to the API"         # straight into a team run
mantra runs                                     # every run: resume or delete
mantra doctor                                   # what's installed, what's missing
```

---

## Solo: one agent, done properly

<img src="docs/img/solo.png" alt="Solo mode: the agent's diff, an approved command, its plan, the context gauge and the changes panel" width="920">

- **The diff as it happens** — every file touched with `+/-` counts, inline hunks, `ctrl+d` for a full viewer.
- **Its plan, live** — the agent's own checklist, ticked as it goes.
- **Context you can see** — a gauge with the auto-compaction threshold marked, so "it forgot everything" stops being a surprise.
- **Type while it works** — `⏎` queues your message behind the running turn (shown as a `⏳ queued` chip); `ctrl+f` shoves it into the turn right now.
- **Model and provider always in the header** — `sol · medium · via OpenAI (Codex) · ctx 8%`.

<details>
<summary>Approvals — who gets asked, and when</summary>

<img src="docs/img/solo-approval.png" alt="An approval card for a shell command" width="820">

`shift+tab` (or `/approvals`) cycles **untrusted → on-request → never ask** for the Solo agent. The change applies to the live session immediately, even while a card is on screen.

- **untrusted** asks before anything that is not known-safe.
- **on-request** asks when the agent wants to leave its sandbox.
- **never ask** never prompts; anything that would have asked is auto-approved and written to the log (`✓ auto-approved (never ask): $ cargo test`). The sandbox still applies.

Team roles have their own `permission` field in the Studio, `never` by default, so a run does not stop to ask. A role set to ask sends its questions to the inbox (`ctrl+g`). Claude Code agents always run with `--dangerously-skip-permissions` — a pipe cannot answer Claude's own prompts — so their containment is the worktree and the role's sandbox instead.

</details>

---

## Mandala: a team that plans, builds and checks

Press `ctrl+o`, describe the goal, and this happens:

**1. The planner writes a phased plan** and you approve it. Phases run in order; tasks inside a phase run in parallel with declared file scopes. Mantra validates the plan before you ever see it — unique ids, known roles, real prompts, and **no overlapping scopes between parallel tasks** — and sends errors back to the planner to fix.

<img src="docs/img/plan-review.png" alt="The plan review overlay: phases, tasks, scopes, acceptance criteria and gate checks" width="920">

**2. The orchestrator runs the phase.** It spawns each task's worker into its own git worktree, then sleeps until something happens — a worker finished, failed, stalled or edited outside its scope — and decides what to do about it.

**3. The gate merges and checks.** Mantra merges the worker branches, runs the phase's shell checks, and hands the merged result to a QA agent to make coherent. Two identical failures in a row stop the run instead of burning every round.

**4. The finale**: heavy QA → a security sweep → the planner verifying its own plan, able to spawn ad-hoc fixers.

**5. `/land`** merges the run branch into yours. Everything else lives in `~/.mantra` — plan, journal, per-phase outputs, merge logs. Nothing is written into your project.

<img src="docs/img/done.png" alt="A finished run: phases, durations, token total and the branch that is ready to land" width="920">

<details>
<summary>Zoom into any agent — it is just Solo again</summary>

<img src="docs/img/zoom.png" alt="Zoomed into one worker: role-coloured band and spine, its task, diffs, plan and context" width="920">

`⏎` zooms into the selected agent, `1`–`9` jump straight to the *n*th, `esc` goes back. A zoomed agent gets a solid role-coloured header band and a coloured spine down its log, so you always know whether you are looking at one agent or the whole team. A run **starts** zoomed into the planner; approving the plan lands you back on the overview.

You can talk to anyone: plain text on the stage re-prompts the planner, `@p2-api use axum` messages one worker directly.

</details>

<details>
<summary>When something goes wrong: halts</summary>

<img src="docs/img/halt.png" alt="A halted run: an amber band naming the agent, the provider error and the key that fixes it" width="920">

There is no vague "paused" state. A run that cannot continue **halts** with a typed reason, an amber band naming the agent and the error, and the key that fixes it:

| halt | what happened | what the band tells you |
|---|---|---|
| paused by you | you pressed `space` | `space` to resume |
| auth / usage limit | 401, 403 or quota | fix credentials, then `space` |
| provider rejected | HTTP 400/422 — e.g. a gateway that refuses Codex's `developer` messages | `m` switches that role's model and resumes |
| environment | a command died in the sandbox (`bwrap`, user namespaces) | fix the machine — `mantra doctor` prints the sysctl |
| gate exhausted | QA kept failing, or repeated the same blocker | feedback for the planner, or retry the gate |
| attempts exhausted | a task used every attempt | `r` retries it |
| agent turn failed | a role's turn failed past its retries and one free respawn | `r` respawns it |

</details>

<details>
<summary>The watchdog: nothing sits idle</summary>

A long run should never quietly stop. Mantra knows who *should* be working at any moment — planning, orchestrating, building a task, gating, running a finale step — and escalates when they are not:

1. idle for `watchdog_seconds` (90 s): nudge the agent;
2. idle for `watchdog_escalate_seconds` (240 s): respawn the orchestrator, or wake it about the idle agent;
3. twice that: wake the planner to sort it out;
4. still nothing: halt, rather than pretend.

Every rung is journaled with `⏰`, and the idle agent's card turns amber. Any agent can also be respawned by hand with `r` (or `ctrl+r`, or `/respawn`) — planner, orchestrator, gate, finale step or worker, each restarted with a prompt carrying the current state.

There are guards for the silly failures too: a worker whose task is already done cannot be prompted, and an orchestrator that prompts the same stuck worker three times in five minutes gets that worker respawned instead.

</details>

---

## Models, providers and backends

<img src="docs/img/picker.png" alt="The model picker: alias, model, which backend it runs through, effort, context window" width="960">

An alias maps to a model **and the runtime it goes through**, and every picker says which: `via OpenAI (Codex) ◌ codex`, `via Claude Code ✧ claude`. The same model reached two ways is two clearly-labelled rows. `ctrl+k` switches the model for the focused agent; `+`/`-` adjust its context window right there.

Effort resolves per model → per role → per agent at runtime (`alt+↑/↓`), clamped to what the model actually supports.

<details>
<summary>Claude Code as a backend for any role</summary>

If `claude` is on your `PATH`, Mantra adds a `claude` provider with six models — `opus46`, `opus48`, `opus5`, `sonnet5`, `fable5`, `fable51` — plus `opus5-1m` and `sonnet5-1m` for the long-context variants. Point any role at one in the Studio, exactly like a Codex model. The built-in `mantra-default-claude` pattern does it for you: planner and orchestrator on Claude, workers on Claude, gates on Codex.

Under the hood each agent is one long-lived `claude -p --input-format stream-json …` process. Mantra translates its event stream into the same shapes the UI already renders, so nothing else in the app knows the difference: steering lands at the next tool boundary, `x` sends a real interrupt, `/compact` compacts the session, and a crashed process restarts with `--resume`.

Mantra's own tools (`mantra_spawn`, `mantra_submit_plan`, …) reach Claude through MCP: Mantra opens a socket and passes `--mcp-config` pointing at `mantra mcp-bridge`, which `claude` starts itself. Nothing to configure.

**Third-party gateways** work through the same backend — set `kind = "claude-code"`, `auth = "api_key"`, a base URL and a key:

```toml
[[provider]]
id = "libertai-claude"
name = "LibertAI (via Claude Code)"
kind = "claude-code"
auth = "api_key"
base_url = "https://api.libertai.io"   # bare host; Mantra strips a trailing /v1
env_key = "LIBERTAI_API_KEY"           # or api_key = "…" to store it (chmod 0600)
```

This is also the way around gateways that reject Codex's `developer` role: same model, same key, different runtime.

</details>

<details>
<summary>Custom OpenAI-compatible providers, and discovery</summary>

<img src="docs/img/models.png" alt="The models registry: aliases, context windows, compaction thresholds, providers" width="960">

`/models` edits everything live (`⏎` edit, `+/-` step, `t` test a model for real, `D` discover, `ctrl+s` save). Add a provider with `n` and fill in four fields:

| field | meaning |
|---|---|
| `id` | short name shown next to its models |
| `base_url` | the API root — Codex calls `…/responses`, discovery reads `…/models` |
| `env_key` | **name** of the environment variable holding the key |
| `api_key` | *or* paste the key directly: stored `0600`, delivered only through the child process's environment, never on argv or in logs |

Press `D` on a provider and Mantra fetches its catalogue: context windows are read from whatever field the provider uses (and marked *assumed* at 200k when it reports none), reasoning effort is enabled only when the model actually supports it, and models you already have are never duplicated.

`t` runs a real test turn including a `developer` message — so a gateway that rejects that role fails here, not three minutes into a run. Starting a run also pre-flights every role's provider and refuses with the exact missing variable name.

</details>

<details>
<summary>Context and compaction</summary>

Every agent is launched with an explicit context window and compaction threshold — its own numbers, or a conservative assumed 200k / 85% — passed as `model_context_window` + `model_auto_compact_token_limit` to Codex and `--autocompact` to Claude. A 16k model compacts instead of dying. A worker that still overflows on an assumed window gets it halved for the next attempt, with a log line telling you to set the real one.

`/compact` compacts by hand (deferred to the end of the turn if one is running), each compaction appears once in the log as `⇣ context compacted · 182k → 31k`, and the orchestrator is reset or compacted between phases. A turn that contains only a compaction never counts as "the agent finished".

</details>

---

## Runs you can walk away from

<img src="docs/img/runs.png" alt="The /runs overlay listing a working run and a finished one" width="900">

Every run writes a typed snapshot as it goes, so closing Mantra — or losing the machine — is not fatal.

```bash
mantra runs                        # id · project · stage · when · goal
mantra runs resume 238330          # an id prefix is enough, from any directory
mantra runs delete 238330          # worktrees, branches and journal, after a y/N
mantra --resume-last               # the most recent unfinished run
```

Inside the app, `/runs` lists this project's runs (`⏎` resume, `D` delete), and the welcome screen says `↻ 1 unfinished run` when there is something to pick up.

<details>
<summary>What "resume" actually does</summary>

A resume never replays what the old process was in the middle of. It restarts at the nearest safe boundary:

| saved stage | on resume |
|---|---|
| planning | the planner is re-attached to its thread and asked to finish the plan |
| plan review | the saved plan goes straight back up for review |
| a phase in progress | a fresh orchestrator, briefed with the current status; running workers re-attach to their own threads when their worktree still exists, otherwise they are re-spawned with the same prompt |
| merging / checks / gate | the merge → checks → gate sequence simply runs again (it is idempotent) |
| finale | that step starts over |
| finished | opens read-only, so `/land` still works |

</details>

---

## Patterns and the Studio

<img src="docs/img/studio.png" alt="The Studio: roles with model, effort, sandbox and permission, plus the architect agent" width="920">

A pattern is the shape of a team: roles (model, effort, sandbox, permission, instructions), settings (parallelism, retries, gate rounds, watchdog timeouts) and flow (who plans, who orchestrates, who gates, what the finale chain is). It is a TOML file, and the Studio edits all of it with live validation.

Or describe what you want and let the **architect agent** edit the pattern while you watch: *"add a docs writer after security"*, *"make workers cheaper"*, *"put the planner on Claude"*.

---

## Keys

<details>
<summary>Everywhere</summary>

| key | does |
|---|---|
| `ctrl+o` | switch Solo ⇄ Mandala |
| `ctrl+k` | model picker (`←→` effort, `+/-` context) |
| `alt+↑/↓` | raise / lower reasoning effort |
| `ctrl+f` | force-send a queued message into the running turn |
| `ctrl+x` | discard the queue |
| `ctrl+d` | diff viewer · `ctrl+e` verbose log |
| `ctrl+t` | side panel / pulse feed · `ctrl+g` inbox |
| `ctrl+r` | respawn the focused agent |
| `ctrl+l` | redraw · `ctrl+c` clear → interrupt → quit · `?` help |

</details>

<details>
<summary>The Mandala stage</summary>

| key | does |
|---|---|
| `tab` | switch between typing and navigating |
| `←→↑↓` | select an agent · `1`–`9` jump to the *n*th and zoom |
| `⏎` | zoom in · `esc` back to the overview |
| `space` | pause / resume the run |
| `r` | respawn the selected agent · `m` switch its model |
| `x` | interrupt · `c` compact · `+/-` effort |
| `p` | plan · `a` approve · `d` diff · `s` studio |
| `@name …` | message one agent; plain text re-prompts the planner |

</details>

<details>
<summary>Slash commands</summary>

`/model` `/effort` `/approvals` `/new` `/compact` `/diff` `/mandala` `/run` `/pattern` `/runs` `/plan` `/pause` `/respawn` `/land` `/studio` `/models` `/inbox` `/verbose` `/help` `/quit` — and `!cmd` runs a shell command.

</details>

---

## Under the hood

<details>
<summary>Architecture in one paragraph</summary>

One process per agent, supervised by a hub: `codex app-server` speaks JSON-RPC over stdio, `claude -p` speaks NDJSON, and both are translated into the same internal events, so the engine and UI have no backend-specific branches. The Mandala flow is a deterministic Rust state machine — spawning, retrying, merging, running checks, advancing phases — and agents are woken only for judgment calls, which they make through tools Mantra executes. Crashes are isolated: one agent restarts (with its thread resumed) while the rest keep working.

[`DESIGN.md`](DESIGN.md) is the long version: the seam between backends, the MCP bridge, halts, the watchdog, resumable runs, the security model and the testing strategy.

</details>

<details>
<summary>Files it writes</summary>

| path | what |
|---|---|
| `~/.mantra/settings.toml` | CLI commands, defaults, approvals, UI options (`$MANTRA_HOME` moves the lot) |
| `~/.mantra/models.toml` | models, context windows, efforts, providers (`0600` when it stores a key) |
| `~/.mantra/patterns/*.toml` | your patterns |
| `~/.mantra/runs/<project>/<id>/` | state, plan, journal, per-phase outputs, merge logs |
| `~/.mantra/worktrees/<id>/` | per-run worktrees, cleaned up as phases complete |
| `~/.mantra/logs/mantra.log` | debug log |

Nothing is written into your project. A repo may *ship* patterns in `<repo>/.mantra/patterns/`, which Mantra reads but never creates.

</details>

<details>
<summary>Terminal support, tmux, accessibility</summary>

Linux and macOS terminals: kitty, WezTerm, Ghostty, Alacritty, iTerm2, Terminal.app, GNOME Terminal, Konsole, foot, tmux/screen, the Linux console. Colour depth is detected (truecolor → 256 → 16) and glyphs fall back to ASCII where Unicode is not available — in ASCII mode every cell is guaranteed pure ASCII. Minimum size 40×12.

In `settings.toml`: `colors`, `glyphs`, `reduce_motion`, `mouse`, `fps`, `notify`. Idle screens render at 0 fps, and a panic inside drawing is caught and logged instead of taking your agents down.

Recommended tmux lines — `mantra doctor` checks them:

```tmux
set -sg escape-time 10                  # otherwise Esc lags by half a second
set -g default-terminal "tmux-256color"
set -as terminal-features ",*:RGB"      # truecolor
set -g allow-passthrough on             # desktop notifications from inside tmux
```

</details>

<details>
<summary>How this is tested, and what is not covered</summary>

- `cargo build` with zero warnings, 82 unit tests, and `scripts/stress.sh`: every screen and overlay rendered at 13 terminal sizes through a whole simulated run, plus a mixed Codex/Claude run over the real MCP bridge, the halt band, the watchdog, zoom-vs-overview, leave → list → resume → delete, and the sandbox notice — in a debug build where arithmetic overflow panics.
- Real runs against a live provider with **Codex 0.154.0** and **Claude Code 2.1.269**: Solo turns, full team runs through gates and finale, compaction on a 16k window, a run left mid-phase and resumed with its worker re-attached, and the bad-key path halting in seconds.
- **Not covered:** Claude Code with a *subscription* login (this build machine has none — API-key mode is verified), macOS in an automated matrix, and Windows, which is not supported.

</details>

<details>
<summary>Regenerating the screenshots and the logo</summary>

```bash
cd mantra_src && cargo build
pip install Pillow fonttools            # plus tmux
python3 docs/tools/shoot.py             # every screenshot, from demo mode
python3 docs/tools/logo.py              # the mark and the banner
```

`docs/tools/termshot.py` drives the binary in tmux at a fixed size, captures the frame with its colours and paints it into a PNG — so the images in this README are real frames, not mock-ups.

</details>

---

<div align="center">

<img src="docs/img/logo.png" alt="" width="72">

**Mantra v0.2.0** · MIT · [changelog](CHANGELOG.md) · [design notes](DESIGN.md)

</div>
