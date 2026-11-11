# Milestone 2 — A gate that cannot race, lie or truncate

Covers: M-04, M-05, M-07, M-11, M-12, and the gate part of M-01.

Files touched: `src/engine/git.rs`, `src/engine/run.rs`, `src/engine/run/helpers.rs`,
`src/engine/resume.rs`, `src/engine/tools.rs`, `src/engine/plan.rs`, `src/app.rs`, tests.

Read before starting, in `engine/run.rs`: `launch_checks` (≈2165), `check_summary` (≈2203),
`spawn_gate_at_round` (≈2235), `note_gate` (≈2307), `start_fix` (≈2333), `on_fix_done` (≈2368),
`resume_fix` (≈2379), `begin_handoff` (≈2406), `on_job` (≈2646, arms `Merge`, `Checks`,
`FixChecks`), the gate branch of `on_turn_done` (≈3114–3181), `on_worker_done` (≈3383),
`resume_core` (≈1061), `apply_revision` (≈5259), `return_to_orchestrating` (≈5327).

How the gate works today, in one paragraph: all tasks done → `Merging` → `Checks{round:0}` →
QA agent (`Gate{round}`) → on *pass*: `Checks{round}` again ("verify") → all green → handoff;
on *fail*: a fix worker (`Fix{round}`) → `FixChecks` → next QA round. A halt can occur at any
point; `resume_core` then re-enters according to `self.stage` and `self.pending`.

---

## WP-2.1 The check runner: heredocs, bash, and kept output

File `src/engine/git.rs`, `run_checks` (≈554).

1. **Do not append to the command text.** Replace
   `.arg(format!("{cmd} 2>&1"))` by `.arg(format!("exec 2>&1\n{cmd}"))`. The redirect is now
   its own first line, so a heredoc terminator at the end of `cmd` stays alone on its line.
2. **Shell.** Use `bash` when it exists, else `sh`. Add
   ```rust
   fn check_shell() -> &'static str   // "bash" if /bin/bash or /usr/bin/bash exists or `bash` is on PATH, else "sh"; cached in a OnceLock
   ```
   and `Command::new(check_shell()).arg("-c")…`.
3. **A failing check with no output gets a trace.** After the loop body computed `code` and
   `output`: if `code != Some(0)`, `code.is_some()` (not a timeout) and
   `output.trim().is_empty()`, run the same command once more with
   `Command::new(check_shell()).arg("-x").arg("-c").arg(format!("exec 2>&1\n{cmd}"))` under the
   same timeout and store the last 2,000 bytes of *that* output, prefixed with the line
   `[mantra] no output — trace of a second run (bash -x):`. (19 of 22 failing checks that day
   printed nothing; QA could not see which clause failed.) Checks must be re-runnable anyway —
   Mantra re-runs them every round.
4. **Keep the output.** Add a parameter `log_dir: Option<&Path>` and `label: &str` to
   `run_checks`. When `log_dir` is `Some`, create it and write one file
   `{label}.log` containing, per check: `$ {cmd}`, `exit {code or "timeout"}`, `{secs}s`, the
   full captured output (not the 6,000-byte tail), and a blank line. `Run::launch_checks`
   passes `Some(&self.dir.join("checks"))` and a label
   `phase-{idx+1}-{kind}-{n}` where `kind` is `checks`, `verify` or `after-fix` and `n` is the
   run-wide counter from WP-2.2 (`checks_gen`), so files are never overwritten.

In `engine/run.rs`:

- `check_summary`: print the exit code plainly —
  `FAIL (exit {n})`, or `FAIL (timed out)` when `c.code` is `None` (today:
  `FAIL (exit Some(1))`). After the list add one line:
  `(full output of every check: {run dir}/checks/{label}.log)`. Store the last label in a new
  field `checks_label: String`.
- `record_checks`: for a failed check add the first non-empty line of its output to the
  journal line: `check `{cmd}` failed ({secs}s, exit {n}): {first line, max 80 chars}`.

In `engine/tools.rs`, `PLANNER_PROTOCOL`, replace the sentence
`Gate `checks` must run as-is on this machine; prefer what exists (`python3 -m pytest`, `cargo test`) over bootstrapping.`
by:

```text
Gate `checks` run with `bash -c` from the workspace root, exactly as written (several lines and heredocs are fine) and are re-run every gate round, so they must be repeatable. Prefer what exists (`python3 -m pytest`, `cargo test`) over bootstrapping. Every check must be able to pass at the end of its own phase: never check for something a later phase produces. Checks state facts about the work, not about the current workflow or model names. A zero-hit audit needs an allow-list for real external identifiers; never ask workers to paraphrase real names to satisfy a check. Evidence a check points at must be newer than the last change to what it proves.
```

**Tests to add** (`git.rs` tests, using a temp dir):
- A check `cat <<'EOS'\nhello\nEOS` passes and its output is `hello\n`.
- A check `python3 - <<'EOS'\nprint(1)\nEOS` passes (skip the test when `python3` is missing).
- `false` fails with code 1 and the output contains `trace of a second run`.
- With `log_dir`, the file exists and contains both commands.
- `check_summary` shows `FAIL (exit 1)`.

---

## WP-2.2 Only the newest checks run counts

**State.** Add to `Run`: `checks_gen: u64` (starts 0). Add a field `gen: u64` to both
`JobTag::Checks` and `JobTag::FixChecks`.

**`launch_checks`.** At the top of the part that actually starts a job:
`self.checks_gen += 1; let gen = self.checks_gen;` and put `gen` into the tag.

**`on_job`.** In both arms, first line:

```rust
if gen != self.checks_gen {
    self.log("⎔", "gray", "ignored the result of a checks run that was superseded");
    return;
}
```

and for `JobTag::Checks { phase, round, .. }` / `FixChecks` also ignore (same line) when
`self.phase_idx() != Some(phase)` or the stage is not `PhaseStep::Checks { .. }`.

Any path that makes a running checks job meaningless must bump `checks_gen`:
`return_to_orchestrating`, `start_phase`, `stop`, and the new `abandon_gate_round` of WP-2.7.

**Tests.** Launch checks twice (the second supersedes), deliver the first job's result: no
stage change, the journal has `superseded`. Deliver the second: handled.

---

## WP-2.3 Nothing starts while halted — including checks and held transitions

1. **`launch_checks` respects the halt.** At its very top:
   ```rust
   if self.halted() {
       self.defer(Pending::Checks { idx, round, after_fix });
       return;
   }
   ```
   Add the variant to `enum Pending`:
   ```rust
   /// The gate checks of phase `idx` (`after_fix`: the run that follows a fix worker).
   Checks { idx: usize, round: u32, #[serde(default)] after_fix: bool },
   ```
   with label `phase {idx+1} gate checks`. `perform_pending` handles it with
   `self.launch_checks(ctx, idx, round, after_fix)`.
2. **`resume_core`.** Its "pick up the stage" block treats `Pending::Gate | Pending::Fix` as
   superseded by an explicit restart (`pickup == false`). Add `Pending::Checks` to that
   pattern.
3. **`resume_boot`** (`engine/resume.rs`): `Pending::Checks` belongs with the pendings that
   "re-running merge/checks reaches instead" — it falls into the existing `other` branch; no
   new code, but add it to the comment.
4. A state file written by this version can contain `Pending::Checks`, which 0.11 cannot read.
   Say so in the changelog ("a run halted during gate checks cannot be opened by an older
   Mantra").

**Tests.** A fix worker completes while the run is halted (`run.halt` set by hand as existing
tests do): no checks job in `TestCtx`, `run.pending == Some(Pending::Checks{after_fix:true,..})`;
`run.resume(ctx)` starts exactly one checks job.

---

## WP-2.4 No handoff without a QA pass

**State.** Add to `Run`: `qa_ok: bool` (default `false`).

Set `true` in exactly one place: the gate branch of `on_turn_done`, arm
`Some((true, summary))`, before `self.run_checks(ctx, idx, round)`.

Set `false` in: `start_phase`, `return_to_orchestrating`, `start_fix`,
`spawn_gate_at_round` (a new round is open), `abandon_gate_round` (WP-2.7).

**`on_job`, arm `JobTag::Checks`, for `round > 0`.** Insert as the first statement of the
`round > 0` handling (i.e. before `else if all_ok`):

```rust
} else if !self.qa_ok {
    // these checks did not follow a QA pass (a resume, a revision): QA decides, not the checks alone
    self.spawn_gate_at_round(ctx, phase, round.max(1));
```

The rest (`all_ok` → `begin_handoff`, same failure twice → halt, otherwise `start_fix`) now
only runs after a real QA pass, which is what its texts ("The QA agent passed the phase but
these gate checks still fail") already assume.

`resume_core` keeps calling `self.run_checks(ctx, idx, 1)` for `Gate | Checks` ("verify
first"); with the rule above its result leads to a fresh QA round unless QA had passed.

`qa_ok` is not persisted: after a process restart `resume_boot` re-runs merge → checks
(round 0) → QA anyway.

**Tests.**
- Stage `Gate{round:2}`, last report *fail*, run halted (`GateExhausted`); `mantra_resume_run`
  → checks job; deliver all-green → a gate agent is spawned (round 1), **no** handoff.
- Stage `Gate{round:1}`, report *pass* → checks → green → handoff (unchanged).
- The 22:14 race: fix worker done during halt (deferred, WP-2.3), resume → one checks run →
  green → QA round (not handoff).

---

## WP-2.5 A third verdict: the check is wrong, not the work

**Tool (`engine/tools.rs`, `gate_report_tool`).** Add a property:

```json
"plan_defect": {"type": "boolean", "description": "fail only: nothing is wrong with the work — a gate check itself is wrong, or cannot pass in this phase or on this machine. No fix worker is sent; the planner is asked to amend the check. Name the check and say why in the summary."}
```

Update the tool description to:
`Report the gate verdict. pass=true only when the criteria are met and checks are green. On fail, `summary` lists every finding as file, what is wrong, what to change — or, when the only problem is a wrong check, set plan_defect=true.`

**Protocol (`GATE_PROTOCOL`).** Replace the last bullet (`Mantra re-runs the gate checks after a pass. …and wait.`) by:

```text
- Mantra re-runs the gate checks after a pass. Never report pass while a gate check fails.
- A check that is itself wrong, or cannot pass in this phase or on this machine, is a plan problem, not a failure of the work: report it with mantra_gate_report(pass=false, plan_defect=true) and name the check. Do not use mantra_ask for that and do not wait — the planner gets your report and amends the check.
- Use mantra_ask only for a real question about the work. After asking, end your turn; you are woken with the answer and report then.
```

**Engine.** Add `gate_plan_defect: bool` to `Run` beside `gate_big`; reset it in
`spawn_gate_at_round` where `gate_big` is reset. In `handle_tool` `"mantra_gate_report"` set it
from `args["plan_defect"]` with the same bool-or-string parsing as `big`. Also set it when
`pass == false` and the summary, trimmed and lower-cased, starts with `plan defect` (models
that put it in the text).

In `on_turn_done`, gate branch, arm `Some((false, why))`, **before** the repeated-blocker
check:

```rust
if self.gate_plan_defect {
    self.note_gate(idx, round, false, &why);
    self.log("◎", "amber", format!("gate round {round}: plan defect — the planner is asked, no fix worker"));
    self.halt_and_escalate_full(ctx, HaltReason::GateExhausted, Some(a),
        format!("phase {} gate: QA reports a plan defect — {}", idx + 1, trunc(&why, 80)),
        format!("phase {} gate: QA reports a plan defect (a gate check is wrong or cannot pass here; the work itself is not at fault). Amend the check with mantra_revise_plan.\n\n{why}", idx + 1),
        None);
    return;
}
```

The planner's escalation prompt already offers `mantra_revise_plan`; after the revision
WP-2.7 re-runs the checks and WP-2.4 starts a fresh QA round.

**Tests.** `mantra_gate_report {pass:false, plan_defect:true, summary:"check 3 …"}` then the
gate's turn end → halted `GateExhausted`, escalation prompt to the planner contains the whole
summary, **no** worker named `fix-1` was spawned. Summary starting with `PLAN DEFECT:` and no
flag → same.

---

## WP-2.6 Waiting is not failing

**(a) Gate agent.** In `on_turn_done`, gate branch, the condition
`if report.is_none() && self.pending_questions.contains_key(&a)` becomes:

```rust
let chain_busy = self.orchestrator.is_some_and(|o| self.pending_questions.contains_key(&o));
if report.is_none() && (self.pending_questions.contains_key(&a) || chain_busy) {
    self.log("?", "amber", format!("{} waits for an answer{}", self.name_of(a), if chain_busy { " (the question is with the planner)" } else { "" }));
    return;
}
```

In `expected_active`, `PhaseStep::Gate`: do not list the gate agent while that `chain_busy`
condition holds (the planner is already listed as `Expect::Answering`). Otherwise the watchdog
nudges a QA agent that is rightly waiting.

**(b) Workers.** In `on_worker_done`, `"completed"` arm, replace

```rust
if waiting && status_line.is_none() {
```
by
```rust
let says_done = status_line.as_deref().map(|l| l.contains("done")).unwrap_or(false);
if waiting && !says_done {
```

so a worker that asked and then wrote `STATUS: blocked` *waits* instead of failing (for a gate
fix worker that used to be an instant halt). When it does say `done` while a question is still
open, remove its entry from `pending_questions` before continuing.

**Tests.** QA asks (pending), orchestrator answers it and asks the planner
(`pending_questions` has the orchestrator), QA's turn ends without a report → no
`[mantra:gate] You ended your turn…` prompt, no gate nudge counted. Fix worker: ask +
`STATUS: blocked` → no halt, journal `waits for an answer`; after the answer and a
`STATUS: done` turn → `on_fix_done` runs.

---

## WP-2.7 A plan revision at the gate restarts the gate cleanly

**New function.**

```rust
/// The gate round in progress no longer stands (its checks or tasks changed): stop the QA
/// agent and any fix worker, forget their results and whatever was due for them.
fn abandon_gate_round(&mut self, ctx: &mut dyn Ctx, why: &str)
```

Body: retire the gate agent (`retire_gate_agent`); stop and mark `Cancelled` every ad-hoc
worker of this phase that is not `Done`/`Cancelled` (same two loops as in
`return_to_orchestrating`, but **also** include workers in `WState::Failed(_)` — the blocked
fix worker); `self.checks.clear()`; `self.gate_report = None`; `self.prev_gate = None`;
`self.last_gate_blocker = None`; `self.last_verify_sig = None`; `self.qa_ok = false`;
`self.checks_gen += 1`; `self.resume_note = None`; and drop a pending
`Pending::Gate | Pending::Fix | Pending::Checks` with
`self.drop_pending(why)` (only those three kinds; keep others). Journal:
`◎ gate round abandoned: {why}`.

**`apply_revision` changes.**

1. `past_build` must include the fix step:
   `PhaseStep::Merging | PhaseStep::Checks { .. } | PhaseStep::Gate { .. } | PhaseStep::Fix { .. } | PhaseStep::Handoff`.
2. Compute `let gate_changed = op.gate != np.gate;` (already compared for the note).
3. After the notes are built and before `let next = …`:
   - `needs_work && past_build` → as today (`return_to_orchestrating`).
   - `!needs_work && past_build && gate_changed` and the step is `Checks`, `Gate` or `Fix`:
     ```rust
     self.abandon_gate_round(ctx, "the plan's gate was revised");
     self.stage = Stage::Phase { idx: cur, step: PhaseStep::Checks { round: 1 } };
     ```
     and remember `let rerun = true;`.
4. At the end, where the function resumes or wakes:
   ```rust
   if escalated { self.resume_by(ctx, planner); }      // resume_core sees Checks → run_checks(idx, 1)
   else if rerun { self.run_checks(ctx, cur, 1); }
   else { self.check_phase_done(ctx); self.wake_orch(ctx); }
   ```
   The text returned for this case stays `the gate runs again with the revised checks` — and
   is now true.
5. A revision that changes nothing in the current phase leaves the gate alone (as today).

**`resume_fix`** (`PhaseStep::Fix` on resume): unchanged for the case where the fix worker is
still valid. Because of (3) a revised gate never reaches it any more.

**`kick_orchestrator` ordering.** The orchestrator was told "these tasks have no attempt yet:
X" before the event that said X was added. In `kick_orchestrator`, when it is about to prompt:
`let notes = std::mem::take(&mut self.orch_inbox);` and, if not empty, prepend
`"{notes joined by \n}\n\n"` to the prompt text.

**Tests.**
- Stage `Fix{round:1}` with a fix worker `Failed("blocked")`, run halted and escalated; the
  planner calls `mantra_revise_plan` changing only `gate.checks` of the current phase → the
  fix worker is `Cancelled`, **not** respawned; one checks job is started; after its result a
  QA round 1 starts whose prompt contains the *new* check command.
- Stage `Gate{round:1}` not halted, QA agent live; revise the gate → that QA agent was
  stopped, a checks job started.
- A `Pending::Gate` deferred during a halt + a gate revision → the pending is dropped
  (journal `dropped`), the QA prompt after the re-run shows fresh results.
- Revision touching only a later phase → nothing at the gate changes.

---

## WP-2.8 Reports in full, on disk, and readable by the agents that need them

**Files.** Add `fn gate_dir(&self) -> PathBuf { self.dir.join("gate") }` and a counter
`gate_seq: u32` (in `Run`, persisted in `RunState` with `#[serde(default)]`).

`note_gate(idx, round, pass, summary)`: additionally `gate_seq += 1`, write the **whole**
summary to `gate/phase-{idx+1}-qa-{gate_seq:02}-round-{round}-{pass|fail}.md` (first line:
`# phase {idx+1} · QA round {round} · {pass|fail} · plan v{plan_version}`), and store the path
in a new field `last_gate_file: Option<PathBuf>`. The one-line entry appended to
`gate-reports.md` gets ` → gate/{file name}` at its end. `gate_notes` (state.json) keeps its
400-char lines.

**No truncation in what agents get.** Introduce one helper in `engine/run/helpers.rs`:

```rust
/// `text` whole when it fits `max` bytes; else cut at the last line break before `max`, with
/// one closing line that says where the rest is. Never cuts mid-line, never silently.
pub fn fit_or_point(text: &str, max: usize, file: Option<&Path>) -> String
```

The closing line: `[cut here — the full text is in {path}]` (or `[cut here]` without a file).

Use it:

| place | today | new |
|---|---|---|
| `start_fix`: findings in the prompt | `trunc(findings.trim(), 6000)` | `fit_or_point(findings.trim(), 48_000, last_gate_file)` |
| `start_fix` while halted: `Pending::Fix { findings }` | `trunc(&findings, 6000)` | the whole `findings` |
| `spawn_gate_at_round`: previous round's summary | `trunc(sum, 1200)` | `fit_or_point(sum, 12_000, last_gate_file)` |
| `spawn_gate_at_round`: each worker report | `trunc(&w.report, 600)` | `fit_or_point(&w.report, 2_000, None)` |
| `spawn_gate_at_round`: each fix worker report | `trunc(&w.report, 600)` | `fit_or_point(&w.report, 4_000, None)` |
| `escalation_context`: last gate report | `trunc(s, 400)` | `fit_or_point(s, 6_000, last_gate_file)` |
| `on_worker_done`: blocked report to the orchestrator | `trunc(&report, 1200)` | `fit_or_point(&report, 4_000, None)` |
| `on_worker_done`: blocked fix worker halt message (full form) | `trunc(&report, 300)` | keep 300 for the short form; pass the whole report as `full` via `halt_and_escalate_full` |

`save_state` cuts `WorkerState.report` at 6,000 bytes (`tail_bytes`): raise to 24,000.

**The fix prompt shows results, not just commands.** In `start_fix`, replace

```rust
let checks = if phase.gate.checks.is_empty() { String::new() } else { format!("\nThe gate will re-run: {}", phase.gate.checks.join(" ; ")) };
```
by
```rust
let checks = if phase.gate.checks.is_empty() { String::new() } else { format!("\n\nThe gate checks as they stand (Mantra re-runs all of them after you):\n{}", self.check_summary()) };
```

**The fix worker's scope is only the files inside the workspace.** `paths_in(&findings, &self.workspace_dir())`
also returns paths QA merely cited (files under `~/.mantra/runs/…`, archives). In
`helpers.rs::paths_in`, drop every path that is absolute and not under `root`, and every path
starting with `~`. (Tripwires for fix workers then stop firing on files they were never meant
to own.)

**`mantra_log` can read a retired gate agent and honours `lines`.**

- Add `last_gate_agent: Option<AgentId>` to `Run`; set it in `retire_gate_agent` and in
  `begin_handoff` (before `self.gate_agent = None`). In `Run::resolve`, the arm
  `"gate" | "qa"` becomes `self.gate_agent.or(self.finale_agent).or(self.last_gate_agent)`;
  the final `if n == self.pattern.flow.phase_gate` likewise falls back to `last_gate_agent`.
- `"mantra_log"` in `handle_tool`: the byte cap for the non-`full` case is applied even when
  the caller asked for few lines, so `lines` looked ignored and the text began mid-sentence.
  Change the cap to `if full { 40_000 } else { 16_000 }` and, when the cap cuts, cut at the
  next `\n[` boundary (start of a log entry) and prefix `[earlier entries omitted]\n`.

**Tell the orchestrator what the gate decided.** In the gate branch of `on_turn_done`, after a
verdict is known, push one line to `orch_inbox` (do not wake it for a pass):
- pass: `Gate round {round}: QA passed — Mantra verifies the checks.`
- fail → fix: `Gate round {round}: QA failed the phase — the fix goes to {who}. Full report: {path}.`
  (`{who}` = `fix-{round} ({role})`, or the task id once Milestone 6 routes to an owner.)
- plan defect: `Gate round {round}: QA reports a plan defect — the planner is asked. Full report: {path}.`

**Tests.** A 15,000-char gate report: the fix prompt contains its last line; the file exists
and holds all of it; the next QA prompt contains the previous report whole (≤12k) or the
pointer line (>12k). `fit_or_point` unit tests (fits; cut at a line; pointer line present).
`mantra_log {"agent":"gate"}` after `start_fix` returns the retired agent's log.
`paths_in` drops `/etc/x`, `~/a/b`, keeps `src/x.rs` and `<root>/src/x.rs`.

---

## WP-2.9 Tell QA when a fix changed nothing

Depends on WP-7.4's repo list only for the non-git case; implement the git case here.

1. In the checks job closure (`launch_checks`), compute a fingerprint of the workspace after
   the checks ran: for a git workspace `git rev-parse HEAD` + `\n` +
   `git status --porcelain` + `\n` + hash of `git diff` (use `helpers::fnv1a`); `None` when the
   directory is not a git work tree. Add it as a fourth field of `JobOut::Checks`
   (`Option<String>`), store it in `Run.tree_print: Option<String>` in `record_checks`.
2. In `start_fix`: `self.fix_base_print = self.tree_print.clone();` (new field).
3. In the `FixChecks` arm of `on_job`, after `record_checks`: if both prints are `Some` and
   equal, journal `⚠ fix-{round} changed no file in the workspace` (amber) and set
   `self.resume_note = Some("Note from Mantra: the fix worker of the last round changed no file in the workspace. If your findings required a change, they still stand.".into())`
   before `spawn_gate_at_round` (which appends `resume_note` to the QA prompt).

No halt: a fix can legitimately be an action outside the tree. WP-1.4 already catches the
empty-turn case that caused this during the run.

**Test.** Equal prints → journal line and the note in the QA prompt; different prints → neither.

---

## WP-2.10 Fix workers: the sandbox the role declares, and a reserved id

**(a) Sandbox.** `launch_worker_with` (≈2053) forces `danger-full-access` for every ad-hoc
worker. The reason (Codex's workspace-write keeps `.git` read-only, a merge fix needs the
index) only exists for **Codex** agents **in worktree mode**.

- Add `pub git_write: bool` to `SpawnReq` (default `false` at every other construction site).
  In `launch_worker_with`: delete the `if w.adhoc && role.sandbox != "read-only" { … }` block
  and set `git_write: w.adhoc && self.ws.as_ref().is_some_and(|ws| ws.worktree)`.
- In `app.rs`, both `Ctxt::spawn` and `Ctxt::spawn_resumed`: after
  `let sandbox = role_sandbox(&r.role.sandbox, backend);` add
  ```rust
  let sandbox = if r.git_write && backend == crate::config::ProviderKind::Codex && sandbox == "workspace-write" { "danger-full-access".to_string() } else { sandbox };
  ```
- In `launch_worker_with`, when `git_write` is true, journal once per run (guard with a bool
  field `git_write_said`):
  `ℹ fix workers on Codex run without Codex's sandbox (it keeps .git read-only, and a merge fix must write it); other backends keep the role's sandbox`.

**(b) Reserved ids.** `spawn_task` decides `adhoc` by `task.id.starts_with("fix-")`. A plan
task called `fix-login` would silently run on the integration copy. Add to `helpers.rs`:

```rust
/// `fix-<digits>`: the ids Mantra gives gate and finale fix workers.
pub fn is_fix_id(id: &str) -> bool
```

Use it in `spawn_task` (`let adhoc = is_fix_id(&task.id);`). In `Plan::validate`
(`engine/plan.rs`), for each task: `if is_fix_id(&t.id)` push
`task id '{id}' is reserved for Mantra's fix workers — choose another id`.

**Tests.** A bija-backed role with `workspace-write`: the fix worker's `SpawnReq.role.sandbox`
is `workspace-write` and `git_write` is false in shared mode. `is_fix_id("fix-1")` true,
`"fix-login"` false, `"fix-"` false. A plan with task id `fix-2` is rejected; `fix-login` is
accepted and is **not** ad-hoc when spawned.
