# Milestone 7 — Journal, watchdog, tripwires, nested repositories, noise

Covers: M-08, M-15, M-16, M-17, M-18, M-24. Each WP is small and independent.

Files touched: `src/engine/run.rs`, `src/engine/git.rs`, `src/engine/run/helpers.rs`,
`src/app.rs`, `src/hub.rs`, `src/hub/claude.rs`, `src/config.rs`, `src/util.rs`, tests.

---

## WP-7.1 "No output for …" only measures a turn that is running

`Run::watchdog_tick` (≈3741) uses `agent.last_event.elapsed()` for a busy agent. An agent that
was idle for 40 minutes and was just prompted is "busy" with a 40-minute-old `last_event`, so
the journal says `planner: no output for 40m19s — still connected` in the second it starts
(and `orchestrator: no output for 15m32s` at the instant of a resume).

Add to `Agent` (`agent.rs`):

```rust
/// How long the turn in flight has been silent: since its last event, but never longer than
/// the turn itself has been running. None when no turn is in flight.
pub fn turn_silence(&self) -> Option<Duration> {
    if !self.busy() { return None; }
    let since_turn = self.turn_wait_since.or(self.turn_started).map(|t| t.elapsed());
    let since_event = self.last_event.elapsed();
    Some(since_turn.map(|t| t.min(since_event)).unwrap_or(since_event))
}
```

(`turn_started` is set at `turn/started`; `turn_wait_since` when the prompt goes out. Make sure
`prompt_agent` sets `turn_wait_since` — it does — so `awaiting_start` turns are covered.)

In `watchdog_tick`, replace `let quiet = agent.last_event.elapsed();` by
`let quiet = agent.turn_silence().unwrap_or_default();` for the *silent-but-busy* line only
(the idle ladder below it keeps `last_event`). Do the same in the stall tripwire of `tick`
(`a.last_event.elapsed() > stall` for running workers → `a.turn_silence().is_some_and(|q| q > stall)`).

`ui::quiet_for` (the `quiet 6m41s` suffix on cards) should use the same value.

A long tool call that streams nothing (a planner writing a 100 KB plan) is still reported —
that is true silence — but the wording should not suggest a hang. Change the line to:
`⏳ {name}: no output for {dur} — its turn is still running`.

**Tests.** With `TestCtx::add_idle` + back-dated `last_event` (see `set_idle_secs`), then
making the agent busy with a fresh `turn_wait_since`: no `no output` line. A busy agent whose
turn started `stall + 1` minutes ago with no event since: one line.

---

## WP-7.2 The journal keeps whole texts and names the actor

**(a) Whole texts.** `Run::log` writes the same (already shortened) text to the pulse and to
`journal.jsonl`; 39 of 150 journal lines of one run ended in `…`, all questions among them.
Add:

```rust
/// A pulse line with its unshortened form for the journal file (questions, answers, reports).
pub fn log_full(&mut self, glyph: &str, color: &'static str, short: impl Into<String>, full: &str)
```

It behaves like `log(glyph, color, short)` but the JSON line gets an extra key
`"full": "<full text>"` when `full` differs from `short` (`journal_line` gains an optional
parameter). Use it at the call sites that shorten *content someone wrote*:
`ask_up` (both branches), `mantra_prompt` (both journal lines), `mantra_brief_orchestrator`,
`user_input` / `direct` (`you → planner`), `plan_feedback`, `mantra_ask_user`,
`mantra_resume_run`, `note`-carrying respawns, `install_pattern` (the reason), gate report
lines (`gate report: pass — …`, `gate round N not passed: …`), `fix-N is blocked`.
Readers of the journal (`Run::journal_text`, the pulse, the web) keep using `text`.

**(b) The actor.** Journal lines for things a person or an agent did must say who:

| event | line |
|---|---|
| model picker / web `set_model` | `⚙ {name}: model → {alias} ({scope}) (by you)` — via the `by` parameter of WP-5.2 |
| `space` pause / resume | `‖ run paused (by you)`, `▶ run resumed (by you)`; a resume by the planner/manager already logs `… resumed the run`; `resume_core`'s own `▶ run resumed` stays for those |
| stage `r` / `/respawn` | `↻ {name} respawned (by you)` (WP-3.3e) |
| stage `x` | `■ {name} interrupted (by you)` — today only `stopped by you` appears at turn end; add this at the key press |
| stage `c` / `/compact` on a run agent | `⇣ {name}: context compaction requested (by you)` |
| engine compaction after a context-full error | `⇣ {name}: context full — compacting` |
| orchestrator compaction at a phase start (`orchestrator_context = compact`) | `⇣ orchestrator: compacting for the next phase` |
| a backend compacting by itself (the agent reducer sees a `Kind::Compaction` item) | `⇣ {name}: context compacted {from}k → {to}k` — add a `Signal::Compacted { from, to }` emitted from `Agent::apply` when the compaction item gets its `to`, handled in `app.rs` by `run.log` for run agents |
| plan approval (`a`) | `✓ plan approved (by you)` |
| `F3` workflow toggle | `⚙ live workflow changes {allowed|locked} (by you)` |

For the pause/resume lines: `Run::toggle_pause` is only called from user keys/commands, so the
suffix can be added inside it (it calls `resume()`; give `resume_core` a `by: Option<&str>`
used in its log line, `None` for engine/agent-initiated resumes).

**(c) Alias fallback noise.** `Registry::get` logs `models: no alias 'X-api' — using 'X'` once
per name per process, but `names()` → `lookup` is called for every alias at every run start
and resume (97 lines in a day). `names()` uses `lookup`, which does not log; the lines come
from `get` being called with the `-api` spelling by `context_windows_for_default`
(`effective_context_window` → `get`). Make `effective_context_window` and `alias_providers`
use `lookup` (no log) and keep the log for genuine user-facing resolutions.

**Tests.** `log_full` writes `full`; `journal_text` output is unchanged. A compaction signal
journals one line. `context_windows_for_default` on a registry with only `sol` and a request
for `sol-api` logs nothing (assert through a counter or by checking the `SAID` list is
untouched).

---

## WP-7.3 Tripwires compare like with like

`Run::on_files_changed` checks `crate::util::in_scope(&w.task.scope, p)` with `p` as the
backend reported it (relative to the agent's cwd, or absolute) and the scope as the planner
wrote it (often relative to a repository that sits *inside* the cwd). Three of five warnings
in one run were false: `repo/tools/x.sh` against scope `tools/x.sh`.

Add to `util.rs`:

```rust
/// Is `path` (as an agent reported it, relative to `cwd` or absolute) inside one of `scope`'s
/// globs, when both are read against `cwd`? Also true when the glob matches the path's tail
/// starting at any directory boundary below `cwd` that is a git repository root or the first
/// path component (a scope written relative to a nested repository).
pub fn in_scope_from(scope: &[String], path: &str, cwd: &Path) -> bool
```

Implementation: normalise `path` to cwd-relative (strip `cwd` when absolute; a path outside
`cwd` is out of scope unless a glob is absolute/`~`-based and matches it after `~` expansion).
Then accept if `in_scope(scope, rel)`; else for each prefix of `rel`'s components
(1 to 3 components) where `cwd.join(prefix).join(".git").exists()`, accept if
`in_scope(scope, rest_after_prefix)`. Scope globs that start with the same prefix are already
covered by the first test.

Use it in `on_files_changed` (the worker's `wt` or the agent's `cwd` is the base) and in
`mantra_set_task_scope`'s `tripwires.retain`.

Keep the fix-worker rule (its tripwires are journalled, nobody is woken).

**Tests.** `in_scope_from(["tools/x.sh"], "repo/tools/x.sh", cwd)` is true when
`cwd/repo/.git` exists and false when it does not; an absolute path under cwd works; a path
outside cwd is out of scope.

---

## WP-7.4 QA gets a diff when the project root is not a git repository

`git::setup` falls back to a shared workspace for a directory that is not a repository;
`review_diff` then returns nothing, and every QA prompt of the day (15 of 15) said
`(no changes in the working tree, or no git diff available)` although all work was committed
in a repository one level down. Each QA round rebuilt the change set by hand (18–26 shell
calls).

**Discover nested repositories.** `engine/git.rs`:

```rust
/// Git work trees directly inside a non-repository project directory, at most two levels deep
/// (`a/.git`, `a/b/.git`), sorted; hidden directories and anything over 200 entries are skipped.
pub fn nested_repos(project: &Path) -> Vec<PathBuf>
```

`Workspace` gains `#[serde(default)] pub nested: Vec<PathBuf>` (relative to `integ`). In
`setup`, on the "not a git repo" path: `nested = nested_repos(project)`, and the note becomes

- none found: `not a git repo — workers share the project directory` (as today)
- some: `not a git repo — workers share the project directory; {n} repositories inside it ({names, max 4}) are diffed for QA`

**Phase bases.** `Run` gains `nested_base: Vec<(PathBuf, String)>` (repo → `HEAD` at phase
start), persisted in `RunState` (`#[serde(default)]`). Set it in `start_phase` by a small
blocking job (or synchronously: `git rev-parse HEAD` per repo is a few milliseconds — do it
through `ctx.job` anyway, the engine never runs git on the event loop; a new
`JobTag::PhaseBase { phase }` with `JobOut::Text` carrying `repo\tsha` lines is the least
code). A repository that appears later (cloned by a worker) is added with base = its first
seen `HEAD` and diffs as empty until the next phase — acceptable.

**The diff.** `review_diff(ws, span, max_bytes)`: when `!ws.worktree` and the directory is not
a repository and `ws.nested` is not empty, build the review from the nested repositories:
for each repo with a known base `b`: `git diff --stat=100 b` and `git diff -U3 b` (working
tree against the base: committed and uncommitted work of the phase), each under a heading
line `## repository {relative path} (since {b[..8]})`; split `max_bytes` evenly between
repositories that have changes; untracked files listed as today. For `DiffSpan::Run` use the
bases recorded at the *first* phase (keep `nested_run_base` separately, set once).
The function needs the bases: add a parameter `bases: &[(PathBuf, String)]` and pass
`self.nested_base` / `nested_run_base` from the two callers (`launch_checks`,
`launch_phase_commit`).

**Fingerprint (WP-2.9)** for this case: concatenate, per nested repo, `HEAD` + status hash.

**Agents are told.** In `worker_brief_context` (helpers) or the worker prompt tail, when
`ws.nested` is not empty add one line:
`The project directory is not a git repository; these repositories are inside it: {list}. Run git commands inside the one you work on.`
The gate prompt gets the same line before the diff.

**Tests** (temp dirs): `nested_repos` finds `a/.git` and `a/b/.git`, not `a/b/c/.git`, not
`.hidden/.git`. With one nested repo, a commit made after the base shows in
`review_diff`'s output under its heading; a clean repo contributes nothing; no nested repos →
the old message.

---

## WP-7.5 Optional: Claude Code agents without the user's global setup

M-15: every Claude Code agent loaded the owner's twelve rule files and memory index, ran his
hooks (204 runs, 33 crashing, ~10 s per spawn), and a QA verdict cited his private rules.
Claude Code's `--setting-sources` flag chooses which settings load; leaving out `user` drops
user-level settings and hooks while the subscription login keeps working (`--bare` would drop
the login too and is **not** to be used here).

Add a pattern setting (default keeps today's behaviour):

```rust
/// "inherit" = Claude Code agents load your user-level settings, hooks and memory as your own
/// sessions do. "project" = only the project's and local settings load.
pub claude_user_config: String,     // default "inherit"
```

`validate`: `inherit` or `project`. `SpawnReq` gains `claude_user_config: String` (filled in
`role_req`, the one funnel every run spawn goes through); `AgentOpts` and `ClaudeSpawn` carry
it; `hub/claude.rs::build_args` adds `--setting-sources project,local` when it is
`"project"`. Solo and the architect are unaffected.

Studio: `"claude_user_config"` in `SETTING_FIELDS` (choice), hint:
`inherit = Claude Code agents load your own user settings, hooks and memory. project = only this project's settings — faster starts, and your private rules stay out of the team.`

Before relying on it, the implementer verifies with the installed CLI that
`claude -p --setting-sources project,local …` starts and answers under a subscription login,
and notes in the PR whether user-level `CLAUDE.md` is also excluded (the documentation does
not say). If the flag is rejected by the installed version, ship the setting with a journal
warning at run start when it is `project` and the CLI refused it — or drop this WP.

**Tests.** `build_args` contains `--setting-sources project,local` only for `project`.

---

## WP-7.6 Noise and small truths

1. **Archive after exit.** `thread/archive failed: codex process exited (-1)` appeared nine
   times, each after a task had ended. In `app.rs` `HubEvent::CmdFailed`, the `other =>` arm:
   when `other` is the archive command (check the `what` string the hub sends for
   `Cmd::Archive` — search `hub.rs` for `"archive"`) and the agent's status is already
   `Stopped`, do nothing (no notice, log at most once to `mlog!`).
2. **bija lingering after its task.** A bija process lived 44 minutes after a 3-second task:
   workers are stopped at the handoff, and that phase sat in the gate. No change to the
   lifecycle (an owner fix needs the process alive — Milestone 6); but an *empty or failed*
   attempt that was replaced (`WState::Cancelled`) must be stopped at once. In `retry_worker`
   the old agent is already stopped; check `try_fallback`'s worker branch and
   `on_empty_turn` do the same (they do via `ctx.stop`). Add a test that a cancelled
   attempt's agent received `stop`.
3. **Durations.** `✓ {task} done in {el}` uses `w.spawned.elapsed()`, which restarts at a
   re-attach and includes halts ("done in 28m06s" with a 14-minute halt inside; "done again in
   7m11s" measured from the first spawn). Track active time per worker: add
   `active: Duration` and `active_since: Option<Instant>` to `Worker`; start the span when its
   turn starts being expected (`launch_worker_with`, a retry/continue prompt, an owner-fix
   prompt), close it at turn end and at a halt (`halt_full` for interrupted workers), reopen on
   resume. Use `active` in the `done in` lines and on the card's time line. Persist
   `active_secs` in `WorkerState` (`#[serde(default)]`) so a resume carries it.
4. **"Cancelled" in phase reports.** `begin_handoff` writes
   `phase-N/<task>.md` for every entry of `self.workers`, so an earlier, cancelled attempt
   overwrites or sits beside the finished one (`state: Cancelled` for work that was done).
   Write one file per task from its **newest** attempt; when that attempt is `Cancelled`
   because a revision removed the task after it had finished, write `state: Done (superseded by a plan revision)`
   — track that with a `superseded: bool` on `Worker`, set in `apply_revision` where a `Done`
   worker is cancelled.
5. **Exit codes.** Anywhere else that prints `{:?}` of an `Option<i32>` exit code to an agent
   or the journal (`log_text`'s `[cmd] $ … (exit Some(0))` in `agent.rs` ≈1012): print
   `exit 0` / `exit ?`.
6. **Codex spawns in the log.** `mantra.log` has no line that ties a Codex agent to its
   thread. In `app.rs` `HubEvent::Ready`, log once per agent:
   `mlog!("agent {agent} ({name}, {backend}) ready: session {thread_id}")`.
7. **Token totals.** The header's `Σ` already shows the cached share. In the journal line
   `run complete in …` add the totals in the same form:
   ` · {total} tok, {cached} cached`.
8. **Akasha notes over 1,000 characters** were refused three times; the agent then had to
   rewrite. Keep the limit, but return the first 1,000 characters' cut point in the refusal so
   the agent can split: append to the refusal text
   ` The first 1000 characters end at: "…{last 40 chars before the limit}".`

**Tests** for 3, 4 and 5 (unit level); 1 and 6 need none beyond compiling.
