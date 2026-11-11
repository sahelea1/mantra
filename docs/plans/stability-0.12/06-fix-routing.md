# Milestone 6 — A gate fix goes back to the worker that wrote the code

Covers: U-3 (engine half), M-14. The canvas for it is `05-ui-ux.md` WP-5.5 case B.

Files touched: `src/engine/run.rs`, `src/engine/run/helpers.rs`, `src/engine/git.rs`,
`src/engine/pattern.rs`, `src/engine/resume.rs`, `src/engine/state.rs`, `src/engine/tools.rs`,
`src/ui/studio.rs` (one setting row), tests.

Do this milestone only after Milestones 2 and 5 are merged and green: it adds a second path
through the gate and must not be mixed with the repairs to the first.

## The idea

Today every failed gate round goes to a fresh `fix-N` worker that has never seen the code.
When all findings are about files one task owns, the agent that wrote those files is still
alive (workers are archived only at the handoff), knows why it wrote them, and can repair
them faster and better. When findings span tasks, name no file, or the owner is not
available, the generic fix worker stays the right tool.

```
QA round r: FAIL
   │
   ├─ plan_defect ───────────────► planner (WP-2.5)            no fix at all
   ├─ every named file belongs to one task T,
   │  T's worker is alive and usable ─► T's own worker fixes   "owner fix"
   └─ anything else ─────────────► fix-r worker                "generic fix" (as today)
        owner fix fails / is blocked / cannot sync ──► falls back to the generic fix, same round
```

A setting turns the owner path off.

---

## WP-6.1 Decide who fixes

**Setting.** `PatternSettings` gains

```rust
/// Who repairs what a gate round found: "owner" = the task's own worker when every finding is
/// in one task's files (else a fix worker); "fix-worker" = always a fresh fix worker.
pub gate_fix: String,          // default "owner"
```

`Default`: `"owner"`. `validate`: must be `owner` or `fix-worker`. Old pattern files have no
key and get the default. Studio: add `"gate_fix"` to `SETTING_FIELDS` after
`"gate_max_rounds"`, kind `Choice` with the two values, hint:
`owner = a gate finding goes back to the task's own worker when it is all in that task's files; anything broader goes to a fix worker. fix-worker = always a fresh fix worker.`

**Owner resolution** (`engine/run.rs`):

```rust
/// The task whose worker should repair these findings, if exactly one task of the current
/// phase owns every file the findings name and its worker can take the job.
fn fix_owner_for(&self, ctx: &dyn Ctx, findings: &str) -> Option<usize /* index into self.workers */>
```

Returns `None` unless **all** hold:

1. `self.pattern.settings.gate_fix == "owner"` and the round is not 0 (a merge-conflict fix is
   never an owner's).
2. `paths = paths_in(findings, &self.workspace_dir())` is not empty (after WP-2.8's filter).
3. For every path there is a task that owns it, and it is the same task for all of them. A
   task *owns* a path when its newest attempt is a non-ad-hoc worker in state `Done` and
   either `crate::util::in_scope(&w.task.scope, path)` (a task with an empty scope owns
   nothing) or the worker's agent changed that file
   (`ctx.agent(a).map(|ag| ag.files.contains_key(path))`).
4. That worker's agent exists and its process is alive: `ctx.agent(a)` is `Some`, its status
   is not `Stopped` / `Crashed(_)` / `Failed(_)`, it is not `stopped_by_user`, and it is not
   busy.
5. The worker has not already done an owner fix that failed in this phase
   (`w.owner_fix_failed`, new `bool` on `Worker`, default `false`).
6. The gate report did not ask for a big fix (`!self.gate_big`): design-level rework goes to
   `worker-big` as before.

QA may also say it directly: `mantra_gate_report` gains an optional string property

```json
"fix_by": {"type": "string", "description": "fail only, optional: the task id whose worker should repair this (when every finding is in that task's work), or \"fix-worker\" for a fresh fix worker. Leave out to let Mantra decide from the files you name."}
```

Store it in `Run.gate_fix_by: Option<String>` (reset where `gate_big` is reset). In
`fix_owner_for`: `Some("fix-worker")` → `None`; `Some(task_id)` → that task replaces the
"same task for all paths" derivation of rule 3 (rules 1, 4, 5, 6 still apply; rule 2 is not
required).

Add one line to `GATE_PROTOCOL` after the `big=true` bullet:

```text
- If every finding is in one task's work, you may name it with fix_by=<task id>: its own worker repairs it. Otherwise leave fix_by out.
```

**Tests.** Findings naming `src/a.rs` (scope of task `t1`) → owner `t1`; naming files of `t1`
and `t2` → `None`; naming no file → `None`; `gate_fix = "fix-worker"` → `None`;
`fix_by: "t2"` overrides the path derivation; a stopped owner agent → `None`; `big` → `None`.

---

## WP-6.2 The owner fix flow

**State.** `Run.fix_owner: Option<String>` (the task id fixing the current round; `None` for a
generic fix). Persist it: add `#[serde(default)] owner: Option<String>` to
`PhaseStep::Fix { round }`? **No** — changing the enum shape breaks old state files. Instead
add `#[serde(default)] pub fix_owner: Option<String>` to `RunState` and to `Pending::Fix`
(`#[serde(default)] owner: Option<String>`).

**Git.** `engine/git.rs`:

```rust
/// Bring a worker's worktree up to the integration branch before it repairs its own work
/// (other tasks' merges, an earlier fix). Commits what the worker left uncommitted first.
/// `Err` = it does not merge cleanly (the merge is aborted; the worktree is as before).
pub fn sync_worker(ws: &Workspace, dir: &Path) -> Result<(), String>
```

- `!ws.worktree` or `dir == ws.integ` → `Ok(())` (shared workspace: nothing to sync).
- `commit_all(dir, "mantra: before fix")?`
- `git -C dir merge --no-edit <ws.branch>` using the same identity handling as
  `guarded_merge` (use `git_with_identity`). On failure: `git -C dir merge --abort`, return
  `Err("the task's worktree does not merge with the integrated copy: {first line of the error}")`.

**`start_fix` branches.** After the halt check and `retire_gate_agent`, before the generic
path:

```rust
if let Some(wi) = self.fix_owner_for(ctx, &findings) {
    self.start_owner_fix(ctx, idx, round, wi, findings);
    return;
}
self.fix_owner = None;
// … generic path as today
```

```rust
fn start_owner_fix(&mut self, ctx: &mut dyn Ctx, idx: usize, round: u32, wi: usize, findings: String)
```

1. `self.stage = Stage::Phase { idx, step: PhaseStep::Fix { round } }`;
   `self.fix_owner = Some(task id)`; `self.qa_ok = false`;
   `self.fix_base_print = self.tree_print.clone()` (WP-2.9).
2. Journal: `⚒ gate round {round} found problems in {task}'s work — back to its own worker`
   (amber). Orchestrator inbox line (WP-2.8) names the task.
3. Sync job: `ctx.job(JobTag::OwnerSync { phase: idx, round, task }, …)` running
   `git::sync_worker(&ws, &dir)` (`dir` = the worker's `wt`; no workspace → skip straight to
   step 4). New `JobTag::OwnerSync { phase, round, task: String }`; its result is
   `JobOut::Text(Result<String, String>)`.
4. On `Ok` (in `on_job`): mark the worker `WState::Running`, `finished = None`,
   `owner_fixing = Some(round)` (new `Option<u32>` on `Worker`); `mark_edge(agent)`; prompt its
   existing agent:
   ```text
   [mantra:fix] The QA gate reviewed the merged result of this phase (round {round}) and found problems in your task's work. Fix exactly these — no refactors, no extras — and stop.

   Findings:
   {fit_or_point(findings, 48_000, last_gate_file)}

   Your working copy has been updated to the integrated state of the phase (the other tasks' changes are merged in). Stay within your task's scope ({scope}). Before you finish run only what covers what you touched, once.

   The gate checks as they stand (Mantra re-runs all of them after you):
   {check_summary}

   End with the STATUS and SUMMARY lines.
   ```
   (through `self.with_akasha`). Save state.
5. On `Err(e)` from the sync, or when the stage is no longer this `Fix` round when the job
   returns (halt, revision): journal `⚒ {task}'s worker cannot take the fix ({e}) — a fix worker takes it`
   and call the generic path (`self.start_generic_fix(ctx, idx, round, findings, self.gate_big)` —
   move today's body of `start_fix` after the halt/retire lines into that function). Keep the
   findings for this in a field `fix_findings: String` set in `start_fix`.

**The owner finishes.** `on_worker_done` → `"completed"`:

- Compute, near the top: `let owner_round = self.workers[wi].owner_fixing.filter(|_| matches!(self.stage, Stage::Phase { step: PhaseStep::Fix { .. }, .. }));`
- In the *blocked* branch: if `owner_round` is `Some(round)`: set `owner_fix_failed = true`,
  `owner_fixing = None`, state back to `WState::Done` (its original work is still merged),
  journal `⚒ {task} could not repair it (blocked) — a fix worker takes it`, clear
  `self.fix_owner`, and call `start_generic_fix(ctx, idx, round, fix_findings + "\n\nThe task's own worker tried first and reported:\n" + report, big)`.
  Do not send the usual `FAILED (blocked)` event to the orchestrator; do not halt.
- In the *done* branch: the existing `again` logic would log "done again" and wake the
  orchestrator. Before it: if `owner_round` is `Some(round)`: set `w.state = WState::Done`,
  `owner_fixing = None`, keep `w.report` (the new report), journal
  `✓ {task} repaired its work (gate round {round}) in {elapsed since the fix prompt}`, push
  `phase {n} · {task} (owner fix r{round}): {summary}` to `gate_notes`, and start the merge of
  that one worker:
  `ctx.job(JobTag::OwnerMerge { phase: idx, round }, … git::merge_workers(&ws, &[(task, dir, branch)]))`
  (no workspace/worktree → go straight to `on_fix_done(ctx, idx, round)`).
  New `JobTag::OwnerMerge { phase, round }` with `JobOut::Merge`.
- `on_job`, `OwnerMerge`: `blocked` not empty → the same integration-blocked halt the `Merge`
  arm raises; `conflicts` not empty → `start_generic_fix` with round 0 semantics is wrong here —
  instead journal the conflict and call
  `start_generic_fix(ctx, phase, round, "The task's own worker repaired its files, but its branch no longer merges into the integrated copy. Merge it by hand (`git merge <branch>`), keep both sides' intent, and commit:\n{branches}", true)`;
  otherwise `self.on_fix_done(ctx, phase, round)`.
- In the *failed turn* arm (`_ =>`): if `owner_round` is `Some(round)` → same as blocked
  (generic fix takes over), unless the failure is a gateway outage (`transient`), which keeps
  its existing path.
- `"interrupted"` with `owner_round`: no orchestrator event (the gate owns it, as for fix
  workers).

**Everything that looks for "the fix worker of round r"** must also find an owner:

| place | change |
|---|---|
| `on_fix_done` | report text: from the owner's `w.report` when `fix_owner` is set, else from `fix-{round}` |
| `resume_fix` (halt lifted in `Fix`) | if `fix_owner` is set: owner `Running` → nothing (the resume prompt reaches it); owner `Done` and `owner_fixing == None` → `on_fix_done`; otherwise call `start_fix` again with `fix_findings` |
| `expected_active`, `PhaseStep::Fix` | also expect the owner worker (`owner_fixing.is_some()` and `Running`) |
| `resume_for_restart` | `fix_worker` is true for an owner too |
| `abandon_gate_round` (WP-2.7) | an owner mid-fix: interrupt it (`ctx.interrupt`), set it back to `Done`, `owner_fixing = None`; clear `fix_owner` |
| `return_to_orchestrating` | same |
| `spawn_gate_at_round` "Fix workers since then" | add the owner's report: `- {task} (its own worker, round {r}): {report}` |
| `begin_handoff`, `start_phase` | clear `fix_owner`, `fix_findings` |
| `engine/resume.rs`, stage `Fix` on a process restart | unchanged: the gate sequence restarts from the merge; clear `fix_owner` |
| `Run::fix_view` (WP-5.5) | `owner = self.fix_owner.clone()`, `agent` = the owner's agent |
| tripwires (`on_files_changed`) | an owner keeps its real scope, so its tripwires go to the orchestrator as for any worker — no change |

**Why this is safe.** Parallel tasks have disjoint scopes (plan validation), the owner works
in its own worktree, the integration copy is only touched by the merge job, and every failure
of the owner path ends in the generic path that exists today.

**Tests** (fake `Ctx`; for the git function a temp repo as in `git.rs` tests):

1. Owner path: gate fail naming a file in `t1`'s scope → no `fix-1` worker; `OwnerSync` job;
   deliver `Ok` → `t1`'s agent got a `[mantra:fix]` prompt, `t1` is `Running`; `t1` completes
   with `STATUS: done` → `OwnerMerge` job; deliver a clean merge → a checks job tagged
   after-fix; then QA round 2 whose prompt lists `t1 (its own worker, round 1)`.
2. Owner blocked → `fix-1` is spawned with the original findings plus the owner's report; no
   halt.
3. Sync error → `fix-1` is spawned.
4. `gate_fix = "fix-worker"` → today's behaviour exactly (existing gate tests stay green
   unchanged — run them with the default flipped in a helper to prove both paths).
5. A plan revision of the gate during an owner fix → the owner is interrupted and `Done`
   again; checks re-run (WP-2.7).
6. `git::sync_worker`: a worktree behind the integration branch gets the other task's file;
   a conflicting one returns `Err` and is left clean (`git status --porcelain` empty,
   no `MERGE_HEAD`).

---

## WP-6.3 The orchestrator knows about fixes

M-14: the orchestrator said "fix-1 has finished, but I haven't received its report".

- When a fix (owner or generic) finishes, push to `orch_inbox` (no wake):
  `Gate fix r{round} by {who} finished: {summary line}. Mantra re-runs the checks and QA.`
- When a fix is blocked or fails: `Gate fix r{round} by {who} could not be completed: {first 200 chars}.`
- `ORCHESTRATOR_PROTOCOL`: add one bullet:
  ```text
  - Gate fixes are Mantra's: after a failed QA round the fix goes to the task's own worker or to a fix worker, and you are told who and what came of it. Do not spawn or retry tasks for it; answer the fixing agent's questions like any worker's.
  ```
