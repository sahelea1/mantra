# Mantra 0.12 — stability plan

Status: **plan only, nothing here is implemented yet.**
Written against commit `1185739` (v0.11.0 + 3 CI commits). Line numbers are approximate
("≈"); always locate code by the function or constant name given.

This plan comes from one long production run of the `mantra-lean` pattern (two runs back to
back, ~6 h, 8 halts, a usage-limit outage and a faulty inference proxy in the middle), the
review a monitoring session wrote about it (bug ids `M-01 … M-24`), the owner's own list of
wishes (ids `U-1 … U-7` below), and a read of the code that pins each symptom to its cause.

The goal is **stability of what exists**. New surface is added only where the owner asked for
it or where a fault cannot be fixed without it. Everything else is in "Deferred" at the bottom.

---

## 1. How to work through this plan (read first)

You are implementing this plan one **work package (WP)** at a time, in the order given in §3.

Rules:

1. **One WP = one commit.** Commit message: `WP-x.y: <title>`. Do not mix WPs.
2. After every WP run, from `mantra_src/`:
   ```bash
   cargo build --locked
   ```
   ```bash
   cargo test --locked --workspace --no-fail-fast -- --test-threads=1
   ```
   Both must pass before the next WP starts. At the end of every milestone also run
   `bash mantra_src/scripts/stress.sh` (terminal rendering at 13 sizes) from the repo root.
3. **Do not run `cargo fmt`.** The code base is not rustfmt-formatted (long lines are
   deliberate); formatting would rewrite every file. Match the style of the surrounding code.
4. **Do not refactor** beyond what a WP says. Do not rename existing functions or journal
   strings unless the WP says so: tests, the web UI and `scripts/stress.sh` match on them.
5. Every WP lists **"Tests to add"**. Add them where the WP says (most engine tests go into
   `mantra_src/src/engine/run/tests.rs`, which has a fake `Ctx` called `TestCtx` — read the
   first 110 lines of that file once to learn `TestCtx::new`, `add_busy`, `add_idle`,
   `settle`, `spawned`, `mk_worker`, and how existing tests call `run.on_turn_done(...)`,
   `run.on_job(...)` and `run.handle_tool(...)` through `on_tool_call`).
6. If an existing test asserts the old behaviour that a WP changes **on purpose**, update that
   test and name it in the commit message. If an existing test fails for any other reason,
   your change is wrong: fix the change, not the test.
7. Text in `code font` inside "exact text" blocks is to be used **verbatim** (journal lines,
   prompts, toasts, footers). The owner reads these every day; consistency matters.
8. Glyphs: every new glyph must go through `theme::g("unicode", "ascii")` so the ASCII mode
   keeps working (`scripts/stress.sh` checks for zero non-ASCII bytes in ASCII mode).
9. When a WP changes a tool's schema or a role protocol text (`engine/tools.rs`), keep old
   arguments working (old sessions may be re-attached after an upgrade).
10. At the end of each milestone: add a section to `CHANGELOG.md` under a new top heading
    `## Unreleased` (create it once), and update the tables in `DESIGN.md` §5 ("Resilience")
    and the paragraphs the WP names. Do not bump the version.
11. If something in this plan contradicts what you find in the code, **stop and write a note
    in the PR description** rather than guessing. Do not invent behaviour.

Vocabulary used below:

| term | meaning |
|---|---|
| run copy | `<run dir>/pattern.toml`, the pattern a run actually uses (`Run.pattern`) |
| saved pattern | the pattern file in `~/.mantra/patterns/` or `<repo>/.mantra/patterns/` (what the Studio edits) |
| slot | a fixed place in a run: `planner`, `manager`, `orchestrator`, `gate`, `finale`, or `worker:<task>` |
| pin | a per-agent model choice: `Run.slot_models[slot]` or `Worker.model` |
| chain | a role's model followed by its fallbacks, in order |
| halt | `Run.halt.is_some()` — the run starts nothing until `resume` |

---

## 2. What the owner asked for (U-items) and what the review found (M-items)

Owner's list:

| id | wish | where |
|---|---|---|
| U-1 | select and act on inactive / failed / not-yet-started agents (at least change their model) | WP-5.4, WP-5.2 |
| U-2 | `ctrl+d` detects the models Codex and Claude Code really offer (Fable 5.2, Opus 5.5 were missing) | WP-4.2 |
| U-3 | a gate fix can go back to the worker that wrote the code; a general problem still goes to a fix worker; **both visible on the animated canvas** | WP-6.1–6.3 (engine), WP-5.5 (canvas) |
| U-4 | Studio changes made in the meantime are active when a run is resumed — and in the current phase, not only the next | WP-3.1, WP-3.2 |
| U-5 | optional max context per agent role; the smallest of {pattern cap, role cap, active model's window} always wins, also on a fallback model | WP-4.1 |
| U-6 | optional second and third fallback model per role | WP-1.2, WP-1.3, WP-5.3 |
| U-7 | the fallback selector shows the provider like the main model does; every provider has its own colour next to its name wherever a model is chosen | WP-5.1, WP-5.2, WP-5.3 |
| U-8 | resuming a run must behave as cleanly as starting a new one | Milestone 3 |

Review items → work packages:

| review id | short | WP |
|---|---|---|
| M-01 | an empty turn counts as "done" | 1.4, 1.5, 2.9 |
| M-02 | quota error retried; fallbacks unused | 1.1, 1.2, 1.3, 1.7 |
| M-03 | every plan revision re-emits the whole plan | 8.1, 8.2 |
| M-04 | gate FAIL always spawns a fix worker; blocked fix halts at once | 2.5, 2.6 |
| M-05 | stale state after revision/resume; double verify; gate passed without QA | 2.2, 2.3, 2.4, 2.7 |
| M-06 | messages lost or late | 1.6, 5.6 |
| M-07 | QA findings truncated | 2.8 |
| M-08 | no diff for QA when the project root is not a git repo | 7.4 |
| M-09 | a halt stops healthy workers; halt not visible | 1.3 (soft halt), 3.4, 5.6 |
| M-10 | configuration changes do not reach a running run | 3.1, 3.2, 3.3 |
| M-11 | sandbox not what the pattern declares | 2.10 |
| M-12 | heredoc checks fail in the runner; check output not kept | 2.1 |
| M-13 | planner is a serial bottleneck | 8.1 (partly); rest deferred |
| M-14 | fix worker outside the orchestrator's view | 6.2 |
| M-15 | the owner's global Claude Code setup leaks into agents | 7.5 (opt-in) |
| M-16 | watchdog lines and compaction on healthy agents | 7.1, 5.4 (confirm keys), 7.2 |
| M-17 | state/journal wrong after stops, revisions, restarts | 3.4, 7.2, 7.6 |
| M-18 | logs truncate what a diagnosis needs; false tripwires | 7.2, 7.3 |
| M-19 | gate accepts stale evidence | deferred (plan-quality; protocol line in 2.5) |
| M-20 | zero-hit check made a worker blur facts | deferred (plan-quality; protocol line in 2.5) |
| M-21 | no binding channel for rulings | deferred |
| M-22 | pattern values not validated (`context_cap`) | 4.1 |
| M-23 | broken model/provider combinations found only inside the run | 1.5 (bija fails loudly); preflight deferred |
| M-24 | process clean-up noise | 7.6 |

### Root causes found in the code (the review could only infer these)

These answer the review's "open questions" and are the basis of the WPs.

1. **Usage limit not recognised.** `agent::is_usage_limit_error` (`src/agent.rs` ≈1152) has no
   pattern for Claude Code's subscription wording (`You've hit your 5-hour limit · resets …`,
   `… session limit …`, `… weekly limit …`), and `hub/claude.rs::classify_claude_error` maps it
   to nothing or to `rateLimitExceeded` (transient). So the turn went down the *transient
   retry* ladder. `Run::fallback_after_usage_limit` exists and works for every role — it was
   simply never reached. → WP-1.1.
2. **Heredoc checks.** `engine/git.rs::run_checks` builds `sh -c "<cmd> 2>&1"`. For a command
   ending in a heredoc the suffix lands on the terminator line (`EOS 2>&1`), so the heredoc
   never closes and the body gets a garbage last line. → WP-2.1.
3. **`danger-full-access` for fix workers is deliberate but too broad.**
   `Run::launch_worker_with` (`engine/run.rs` ≈2053) forces it for every ad-hoc worker because
   Codex's workspace-write sandbox keeps `.git` read-only and a merge fix needs the index. It
   is applied for every backend and also when there is no worktree. → WP-2.10.
4. **The gate did pass without its second QA read.** Two checks jobs were in flight: one from
   a fix worker that finished *during* a halt (`on_fix_done` → `launch_checks` is not gated on
   the halt), one from `resume_core` ("verify first"). The first result spawned QA round 2,
   the second (`JobTag::Checks { round: 1 }`, all green) called `begin_handoff`, which
   archived that QA agent three seconds later. Nothing ties a checks result to the run of
   checks that is current, and nothing requires a QA pass before a handoff. → WP-2.2, 2.3, 2.4.
5. **Stale fix brief after a revision.** `Run::apply_revision` does not treat
   `PhaseStep::Fix` as "past build", so a revised gate leaves the stage in `Fix`; `resume_core`
   then calls `resume_fix`, which restarts the blocked fix worker with its old prompt. → WP-2.7.
6. **Stale check results in a QA prompt.** A `Pending::Gate` deferred during a halt is
   performed on resume even when the revision changed the checks; `spawn_gate_at_round`
   renders `self.checks` from before. → WP-2.7.
7. **Findings truncated.** `start_fix` cuts the findings at 6,000 chars
   (`trunc(findings.trim(), 6000)`), `spawn_gate_at_round` cuts the previous report at 1,200
   and worker reports at 600, `escalation_context` cuts the last gate report at 400. → WP-2.8.
8. **`mantra_log {"agent":"gate"}` → "unknown agent".** `Run::resolve("gate")` only knows the
   live gate agent; `start_fix` has already retired it. → WP-2.8.
9. **QA "no report" nudges while it waits.** The orchestrator answers the QA agent's question
   (which clears `pending_questions[qa]`) and *then* passes the question up to the planner; the
   QA agent's next idle turn end has no pending question any more, so it is told to report and
   reports FAIL, which spawns a fix worker. → WP-2.5, 2.6.
10. **`STATUS: blocked` after `mantra_ask` halts at once.** `on_worker_done` only treats a
    worker as waiting when there is *no* STATUS line. → WP-2.6.
11. **Messages lost on a failing agent.** `App::on_hub` flushes `Agent.queued` after *every*
    `turn/completed`, failed ones included, and a respawn creates a new agent without the old
    one's queue. → WP-1.6.
12. **Studio never touches a run.** `ui/studio.rs::save_pattern` writes the saved pattern only.
    `App::resume_run` reads the run copy. Only the web command `set_role_model` and the
    planner's `mantra_revise_pattern` change a live run. → WP-3.2.
13. **Planner context lost on restart.** `Run::resume_boot` re-attaches the planner only in
    `Stage::Planning`; in a phase it is spawned lazily and fresh, with only the question that
    woke it. → WP-3.3.
14. **A workflow revision respawned the planner** because `install_pattern` compares the whole
    planner `Role` (any re-serialised whitespace differs). → WP-3.3.
15. **`known_models` goes stale.** The engine's copy of the registry is refreshed at run start,
    resume and a live switch only; an alias added later is "unconfigured". → WP-3.1.
16. **"no output for 40m" on an idle agent.** `watchdog_tick` measures `last_event.elapsed()`
    for a busy agent; an agent that *just* became busy still has its old `last_event`. → WP-7.1.
17. **`m` on a finished worker relaunches a process that is never prompted.** `App::set_model`
    does not check that the agent is live. → WP-5.2.
18. **Claude models are a hard-coded table** (`config.rs::CLAUDE_MODELS`). → WP-4.2.
19. **bija reads tool-call deltas from `choices[0]` only** (`bija/src/api/openai.rs` ≈218), and
    a `finish_reason: "tool_calls"` with no parsed call is a normal empty answer. → WP-1.5.
20. **Task counters count attempts.** "building · 2/1 tasks done": `workers.filter(Done)` counts
    every attempt and fix worker against `phase.tasks.len()`. → WP-7.6.
21. **Unknown: who compacted an idle planner and a finished worker.** The engine compacts only
    on a context-full error and (in `compact` mode) the orchestrator. The stage's `c` key
    compacts the selected agent at once, with no confirmation and no journal line; with two
    operators on one screen this is the likely source. → WP-5.4 (confirmation) and WP-7.2
    (journal every user-triggered compaction).

Smaller answers: "Σ 121" in the header is the run's token total (`Σ 121.22M tok`); the fix
worker's lower effort is `effort = auto` resolving per role (`auto_effort`), not a bug; the
journal does not record who changed a model — WP-7.2 adds "(by you)".

---

## 3. Order of work

Milestones are independent PRs. Do them in this order; inside a milestone do the WPs in order.

| # | file | milestone | why this position |
|---|---|---|---|
| 1 | `01-turn-outcomes-and-fallbacks.md` | Turns that fail honestly; fallback chains | every later recovery rule builds on it |
| 2 | `02-gate.md` | A gate that cannot race, lie or truncate | the source of 6 of the 8 halts |
| 3 | `03-resume-and-config.md` | Resume and configuration behave like a fresh run | the reason the owner restarted runs |
| 4 | `04-models-and-context.md` | Context limits and model discovery | small, needs 1 and 3 |
| 5 | `05-ui-ux.md` | Terminal and web UI | needs the data model of 1, 3, 4 |
| 6 | `06-fix-routing.md` | Gate fixes go back to the task's own worker | needs 2 and 5.5 |
| 7 | `07-observability-and-small-fixes.md` | Journal, watchdog, tripwires, nested repos | independent, low risk |
| 8 | `08-plan-patches.md` | Small plan revisions without re-emitting the plan | optional but recommended; needs 2 |

If time is short, milestones 1–3 alone remove seven of the day's eight halts.

---

## 4. Acceptance scenarios (run these by hand or as tests at the very end)

Each scenario is one of the day's failures. "Then" is what must happen after this plan.

| # | scenario | then |
|---|---|---|
| A1 | planner on Claude Code gets `You've hit your 5-hour limit · resets 9:20pm (UTC)` | no retry; journal `⇄ planner: fable51 → fallback <alias> (usage limit)`; the planner continues on the fallback with its queued user messages; running workers are not interrupted |
| A2 | every model in a role's chain is exhausted | one halt, reason `usage_limit`; workers that were running finish their turn; band says the run is halted and what key resumes |
| A3 | a worker's turn ends with no text and no tool call | journal `⚠ … empty turn …`; one retry; then the next fallback; never `✓ done in 2s` |
| A4 | gate check with a heredoc (`python3 - <<'EOS' … EOS`) | passes or fails exactly as it does in a shell; its output is in `<run dir>/checks/` |
| A5 | QA reports that a check itself is wrong | no fix worker; the planner gets the report in full; after its revision the checks re-run and a fresh QA round starts |
| A6 | planner revises the gate while a fix worker is blocked | the fix worker is cancelled, not restarted; checks re-run with the new gate |
| A7 | a fix worker finishes while the run is halted and the planner then resumes | exactly one checks run; no handoff without a QA pass |
| A8 | QA report of 15,000 characters | the fix worker receives all of it; `<run dir>/gate/` has the file |
| A9 | Studio: change `qa.model` while a run of that pattern is in phase 2, press esc | journal lists the change; the next QA spawn uses the new model; a live agent of a changed role switches |
| A10 | quit Mantra, edit the saved pattern, `mantra runs resume <id>` | journal lists what was adopted; pins on changed roles are dropped; the planner is re-attached to its session |
| A11 | select the dashed gate card, press `m`, pick a model | journal `⚙ role qa: model → … (this run)`; next QA uses it; no process is started |
| A12 | `m` on a finished worker | no process relaunch; journal says the next attempt uses the model |
| A13 | role `context = 250k`, pattern `context_cap = 500k`, model window 1M; then the role falls back to a 128k model | agents are told 250k, then 128k |
| A14 | `/models` → `ctrl+d` with Claude Code logged in | Opus 5.5 and Fable 5.2 are offered (from the CLI when it answers, else from the built-in list) |
| A15 | gate round fails on files owned by one task | that task's worker gets the fix; its card shows `⚒ r1`; an edge runs from the gate back to it |
| A16 | type a message on the overview while the planner is mid-turn | a queue chip appears; journal says when it was delivered |

---

## 5. Deferred on purpose (do not implement)

| item | why not now |
|---|---|
| Preflight turn per role model before a run (M-23) | new feature; WP-1.5 makes the failing combination fail loudly instead of silently |
| Rulings list for QA, commit-trailer hook (M-21) | new feature; plan-quality topic |
| Evidence freshness (M-19), allow-lists for zero-hit audits (M-20) | plan-quality; two protocol lines are added in WP-2.5 |
| Planner inbox / batching of questions (M-13) | needs design; WP-8.1 removes most of the waiting |
| `mantra status --json`, `mantra send`, notification webhook (review §7.6) | new surface |
| Cost/time report per run (review §7.7) | new feature |
| Running every gate check once at plan submission | checks are shell commands the user approves at plan review; running them earlier breaks that rule |
| `--continue-from <run>` | new feature |
| Provider colour override in the UI | the colour is assigned automatically and stored; hand-editing `models.toml` is enough for now |
