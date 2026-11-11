# Milestone 3 — Resume and configuration behave like a fresh run

Covers: U-4, U-8, M-10, parts of M-09 and M-17.

Files touched: `src/app.rs`, `src/config.rs`, `src/engine/pattern.rs`, `src/engine/run.rs`,
`src/engine/resume.rs`, `src/engine/state.rs`, `src/ui/studio.rs`, `src/web/commands.rs`,
`src/web/ops.rs`, `src/main.rs`, tests.

What went wrong, in the owner's words: *"I changed / added some models and a provider and
changed config in studio and even killed mantra and resumed the paused run, but the change
isnt updated on the current pattern / run."* The restart re-read `models.toml` (new process)
but not the pattern: a run only ever reads its own copy. He ended up starting a new run and
losing plan, handoff and notes.

The rule after this milestone:

> **A run follows the saved pattern it was started from.** Whenever the saved pattern is
> changed — in the Studio, in the web editor, or by editing the file — the run adopts the
> changed fields: at once when the change is made in this Mantra, and on the next resume
> otherwise. Fields the *run* changed for itself (a `--parallel` override, a workflow revision
> by the planner, the `m` picker in role scope) are kept unless the user changed the very same
> field in the saved pattern. Everything adopted is listed in the journal.

---

## WP-3.1 The registry the run sees is the registry there is

**(a) One hook.** Add to `App`:

```rust
/// The registry changed (an edit here, a discovery, a reload from disk): tell the run.
pub(crate) fn registry_changed(&mut self) {
    let known = self.registry.context_windows_for_default(&self.settings.default_model);
    let provs = self.registry.alias_providers();           // WP-1.3
    let sees = crate::engine::run::vision_aliases(&self.registry);
    if let Some(run) = self.run.as_mut() {
        run.set_known_models(known);
        run.set_known_providers(provs);
        run.set_vision_models(sees);
    }
}
```

Call it at the end of `App::save_registry` (every edit in `/models`, discovery and the web
goes through it — confirm with `grep -n "save_registry" mantra_src/src -r`; any path that
mutates `self.registry` and saves another way must call the hook too), and replace the three
existing blocks that set known models by hand in `model_switched_for_run_agent` with a call to
it.

**(b) Reload when the file changes on disk.** Add to `App`:
`registry_mtime: Option<std::time::SystemTime>` (set at start-up and after every own save
from `std::fs::metadata(Registry::path())`, and the same for the secrets file if
`config.rs` exposes its path — it has a `Secrets` type; add `Secrets::path()` if missing).

In `App::tick`, at most every 2 seconds (keep an `Instant`): if either mtime differs from the
stored one **and** nothing is being edited on the models screen
(`!self.models_ui.dirty && self.models_ui.keyflow.is_none()` and `self.edit` is not a
`ModelCell`/`ProviderCell` target) and `!self.demo`:
`self.registry = Registry::load(); self.registry_changed();` store the new mtimes, and
`self.toast("models.toml changed on disk — reloaded", Level::Info)`. If a run is active also
`run.log("⚙", "blue", "models reloaded from disk")`.

**Tests.** `registry_changed` with a run: an alias added to `app.registry` afterwards passes
`validate_pattern_replacement` as a fallback (it was `REJECTED … unconfigured` before). The
mtime path: write a modified `models.toml` in the test home, force the tick interval, assert
the new alias is in `app.registry` (follow the pattern of
`servers_changed_on_disk_while_the_app_runs_are_seen_and_kept` in `app.rs` tests).

---

## WP-3.2 A run adopts changes to its saved pattern

### (a) Remember what the run was started from

- `Run` gets `pub pattern_base: Option<String>` (TOML text of the saved pattern as last
  synchronised). Not part of `state.json`; it lives in `<run dir>/pattern.base.toml`.
- `App::start_run_with`: right after `Pattern::load(...)` succeeded and **before** the
  re-binding of built-in aliases, `set_parallel` and the demo tweaks:
  `let base_toml = pattern.to_toml();`. After `Run::new(...)`: `run.pattern_base = Some(base_toml);`.
- `Run::start`: after writing `pattern.toml`, write `pattern.base.toml` from `pattern_base`.
- Add `fn save_pattern_base(&self)` that writes the file; call it whenever `pattern_base`
  changes.

### (b) Three-way merge at field level

In `engine/pattern.rs`:

```rust
/// What `theirs` changed relative to `base`, applied on top of `ours`. Tables are merged key by
/// key; arrays and scalars are taken whole. A key `theirs` removed is removed; a key only `ours`
/// has (or changed) is kept.
pub fn merge3(base: &toml::Value, ours: &toml::Value, theirs: &toml::Value) -> toml::Value
```

Algorithm for two tables (anything else is a leaf):
for every key in the union of the three tables' keys:
- `b = base.get(k)`, `o = ours.get(k)`, `t = theirs.get(k)`.
- If all present values are tables → recurse (missing ones count as empty tables), keep the
  result unless it is an empty table that `ours` did not have.
- Else if `t == b` (both absent counts as equal) → take `o` (absent → omit the key).
- Else → take `t` (absent → omit the key).

Leaves at the top level: if `theirs == base` take `ours`, else `theirs`.

```rust
pub struct Adoption { pub pattern: Pattern, pub changes: Vec<String>, pub model_roles: Vec<String> }

/// The run copy `ours` after adopting what changed in the saved pattern (`theirs`) since `base`.
/// `Ok(None)` when nothing is to adopt. The name always stays `ours.name`.
pub fn adopt(base_toml: &str, ours: &Pattern, theirs: &Pattern) -> Result<Option<Adoption>, String>
```

- Parse `base_toml` to `toml::Value` (error → `Err("the run's pattern base is unreadable")`).
  `ours`/`theirs` → `toml::Value` via `toml::Value::try_from`.
- If `theirs` value == base value → `Ok(None)`.
- `merged = merge3(...)`; force `merged["name"] = ours.name`; `Pattern` from it via
  `toml::Value::try_into`; call `normalize_fallbacks` on every role; `validate()` — errors →
  `Err(joined)`.
- `changes`: walk `ours` vs `merged` values; for every differing leaf path produce
  `"{path}: {old} → {new}"` with the path dotted (`roles.qa.model`, `settings.context_cap`),
  values rendered with `to_string()` and cut to 40 chars; a missing side renders as `—`.
  Long text fields (`instructions`, `description`, `task`) render as `changed`.
- `model_roles`: role names whose `model` differs between `ours` and `merged`.
- If `changes` is empty → `Ok(None)`.

**Tests (pattern.rs).** base=A; ours = A with `max_parallel` 8 (a `--parallel` run);
theirs = A with `roles.qa.model = "x"` → merged has both; `changes == ["roles.qa.model: … → x"]`.
theirs changes the same field ours changed → theirs wins. theirs deletes a role ours still has
→ merged lacks it (the engine-side validation in (c) decides whether that is acceptable).
theirs == base → `None`. A role added in theirs appears.

### (c) Applying it in the engine

Make `Run::validate_pattern_replacement` `pub(crate)`. Add:

```rust
/// Replace the run copy by an adopted pattern (`pattern::adopt`). Returns the roles whose model
/// changed, for the app to switch live agents. Journals every change.
pub fn adopt_pattern(&mut self, ctx: &mut dyn Ctx, a: Adoption, new_base: String, origin: &str) -> Result<Vec<String>, String>
```

1. `self.validate_pattern_replacement(&a.pattern)?` (refuses a changed `isolation`, a removed
   role a live worker or the remaining plan needs, an unknown alias, …).
2. `let previous = std::mem::replace(&mut self.pattern, a.pattern);`
   keep the run-level permission: `self.pattern.settings.allow_pattern_changes = previous.settings.allow_pattern_changes;`
   `self.sync_akasha_groups();`
3. For every role in `a.model_roles`: drop the pins that would outrank the new model —
   `self.slot_models` entries whose slot's role is that role (planner → `flow.planner`,
   manager, orchestrator, gate → `flow.phase_gate`, finale → the current step's role), and
   `w.model = None` for workers of that role that are not `Done`/`Cancelled`. Also clear
   `fallback_used` for those slots. Journal for each pin dropped:
   `⚙ {slot}: your per-agent model {old} gives way to the role's new model {new}`.
4. Journal, colour `"blue"`:
   `⚙ pattern updated from {origin} ({n} change(s)): {first six changes joined with " · "}{" · …" if more}`
   and write all changes, one per line, with `journal_line("⚙", …)`.
   `origin` is `the Studio`, `the saved pattern (on resume)` or `the web editor`.
5. `self.pattern_base = Some(new_base); self.save_pattern_base(); self.save_state();`
6. Supervisors whose **spawn-relevant** definition changed (see `role_spawn_eq` in WP-3.3) and
   that are *not* in `a.model_roles` (the app switches those live): orchestrator — if the
   stage is `Orchestrating` call `respawn_orchestrator(ctx, Some("The workflow was updated by the user; continue the phase."))`,
   else just `take()` and stop it (the next phase spawns a fresh one); gate agent — nothing
   (the next round is a fresh agent anyway); manager — stop it and `ensure_manager` when a
   phase is active; planner — only if its `instructions`, `sandbox`, `permission` or `mcp`
   changed: set `self.planner_refresh_pending = true` when busy, else `respawn_planner`.
7. Return `a.model_roles`.

### (d) When it happens

**On resume — `App::resume_run`.** After the run copy is loaded into `pattern` and before
`Run::from_state`:

```rust
let saved = Pattern::load(&st.pattern, &self.project).ok();            // may be a built-in
let base = std::fs::read_to_string(r.dir.join("pattern.base.toml")).ok()
    .unwrap_or_else(|| pattern.to_toml());                              // runs from before 0.12
```

Build the run as today, set `run.pattern_base = Some(base.clone())`, set known
models/providers, and then — **before `run.resume_boot`** —:

```rust
if let Some(saved) = saved {
    match crate::engine::pattern::adopt(&base, &run.pattern, &saved) {
        Ok(Some(a)) => {
            let problems = self.preflight(&a.pattern);
            if let Some(p) = problems.first() {
                run.log("⚠", "amber", format!("the saved pattern changed but cannot run here ({p}) — this run keeps its own copy"));
            } else {
                let mut ctx = Ctxt { … };
                match run.adopt_pattern(&mut ctx, a, saved.to_toml(), "the saved pattern (on resume)") {
                    Ok(_) => {}                       // nothing is live yet: resume_boot spawns everything on the new roles
                    Err(e) => run.log("⚠", "amber", format!("the saved pattern changed but does not fit this run ({e}) — it keeps its own copy")),
                }
            }
        }
        Ok(None) => {}
        Err(e) => run.log("⚠", "amber", format!("could not compare with the saved pattern: {e}")),
    }
}
```

Because step 3 of `adopt_pattern` runs before `resume_boot`, `restore_slot_models` and
`restore_workers` must not put the dropped pins back: add `pub(super) adopted_roles: HashSet<String>`
to `Run` (filled in `adopt_pattern` from `model_roles`), and in `restore_slot_models` /
`restore_workers` skip the saved alias when the slot's/task's role is in it.

For step 6 on this path nothing is live, so guard step 6 with `if !self.all_agents().is_empty()`.

**Live — after a save in this Mantra.** Add to `App`:

```rust
/// The saved pattern `name` was just written. If the active run was started from it, adopt.
pub(crate) fn sync_run_pattern(&mut self, origin: &str) -> Option<usize>
```

- Return `None` unless `self.run` is active and `run.pattern.name == <saved name>` and
  `run.pattern_base.is_some()`.
- `saved = Pattern::load(name, &self.project)`; `adopt(base, &run.pattern, &saved)`;
  preflight as above (a problem → toast it, `Level::Warn`, return `None`).
- `roles = with_run(|r, c| r.adopt_pattern(c, a, saved.to_toml(), origin))`; an `Err(e)` →
  `self.toast(format!("the run keeps its own pattern: {e}"), Level::Warn)`.
- `self.registry_changed();`
- For every role in `roles`, switch the live agents of that role **the way
  `Command::SetRoleModel` does today**. Move that loop out of `web/commands.rs` into a new
  `App::switch_live_agents_of_role(&mut self, role: &str, alias: &str)` (the `over` closure,
  `set_model`, `model_switched_for_run_agent`) and call it from both places. For roles whose
  `effort` changed, call `self.set_effort(id, &new_effort)` on their live agents.
- Toast: `run updated: {n} change(s) from the pattern` and return `Some(n)`.

Call sites:
1. **Studio:** not on every keystroke-save. In `App::leave_screen`, when the screen being left
   is `Screen::Studio` and the Studio's pattern is saved (`!self.studio.dirty`):
   `self.sync_run_pattern("the Studio");`. Also on `ctrl+s` in the Studio (explicit save).
2. **Web:** at the end of `pattern_save` (`web/ops.rs`), and in `Command::SetRoleModel` /
   `Command::SetRoleFallback` (`web/commands.rs`) **replace** their hand-written patching of
   `run.pattern` by `self.sync_run_pattern("the web editor")` (they already save the pattern
   first). Keep their return values.

The planner's `mantra_revise_pattern` (`install_pattern`) changes the run copy only and leaves
`pattern_base` alone — it is a run-local change, kept by the merge.

### (e) Tests

- Resume: state + run copy + base on disk, saved pattern changed `qa.model` and
  `settings.context_cap` → after `resume_run` the gate spawns on the new model
  (use the fake hub the `app.rs` tests use; `a_stopped_run_makes_way…` shows the set-up), the
  journal has `pattern updated from the saved pattern (on resume) (2 change(s))`.
- Resume without `pattern.base.toml` (legacy): differences between run copy and saved pattern
  are adopted.
- Resume where the saved pattern removed a worker role the plan still uses → the run keeps its
  copy and the journal says why.
- Resume with a saved per-agent pin on the planner and a changed `planner.model` → the planner
  comes back on the new role model, journal line about the pin.
- Live: `sync_run_pattern` after changing `orchestrator.model` → the live orchestrator agent's
  `model_alias` is the new one; a finished worker keeps its alias.
- `--parallel 8` run + Studio change of an unrelated field → `max_parallel` stays 8.
- Existing `set_role_model_*` web tests still pass (they now go through `sync_run_pattern`).

---

## WP-3.3 The planner keeps its memory across restarts and respawns

**(a) Re-attach on a phase or finale resume.** `resume_boot` re-attaches the planner only in
`Stage::Planning`. Add `planner_thread: Option<String>` to `Run`. In `resume_boot`, for
`Stage::Phase` and `Stage::Finale` (before the `match`), set it from
`st.agents.iter().find(|a| a.slot == "planner").and_then(|a| a.thread_id.clone())` — but only
when that saved agent's `model_alias` still equals the alias the planner would spawn on now
(`self.model_for_spawn("planner", …)`); a session cannot be resumed on another backend.

**(b) One way to reach the planner.** Add:

```rust
/// The planner, spawned when there is none: re-attached to its saved session when there is one,
/// otherwise fresh with a context pack. Returns its id and the preface its next prompt must
/// start with ("" for a planner that was already there).
fn planner_with_preface(&mut self, ctx: &mut dyn Ctx) -> (AgentId, String)
```

- `Some(p)` already → `(p, String::new())`.
- Else `thread = self.planner_thread.take()`; `p = self.spawn_planner_with(ctx, thread.clone())`;
  preface:
  - re-attached:
    `[mantra:resume] Mantra restarted; you are re-attached as this run's planner and your earlier conversation still holds. The run is at {stage label}, plan v{n}.\n\n`
  - fresh: `self.planner_context_pack()` + `\n\n`.

```rust
/// Everything a planner that starts mid-run needs: the brief, the plan in outline, the current
/// phase in full, the recent journal.
fn planner_context_pack(&self) -> String
```

Text:

```text
[mantra:respawn] You are this run's planner, starting with a fresh session (the earlier planner's session is not available). Continue from stage {stage label}, plan v{n}.

Run brief:
{self.brief}

Plan: {title} — {summary cut to 1,500 chars}
{one line per phase: "- phase {i} [{id}]: {name} ({k} tasks) — done | CURRENT | pending"}

Current phase:
```json
{current phase, pretty JSON}
```
Recent journal:
{self.journal_text(25)}

Call mantra_read_phase for live task states and mantra_read_plan for another phase.
```

(Leave out the "Current phase" block when no phase is active; when there is no plan yet use
`self.planning_prompt()` instead of the whole pack.)

Replace every lazy spawn of the planner by it. Find them with
`grep -n "spawn_planner(ctx)" mantra_src/src/engine/run.rs` — expected in
`escalate_to_planner`, `user_input` (two places), `direct`, `ask_up`, `planner_wait_tick`,
`resume_core` (loop planner), `start_phase` (loop planner) and `resume_boot`'s pending-pattern
block. At each site: `let (p, pre) = self.planner_with_preface(ctx);` and put `{pre}` at the
front of the prompt text sent in that same function. (`respawn_planner` keeps calling
`spawn_planner` directly.)

**(c) A respawned planner gets its predecessor's last words.** In `respawn_planner`, before
the old agent is stopped: `let handoff = ctx.agent(old).map(|a| a.handoff_context());`
(`Agent::handoff_context` exists, `agent.rs` ≈360, bounded to ~24 KB). Append to the respawn
prompt: `\n\nFor continuity, what the previous planner session held:\n{handoff}` cut with
`fit_or_point(.., 16_000, None)`. Do the same in `respawn_orchestrator` with a 6,000 cap.

**(d) Do not respawn the planner for a change that does not concern it.** Add to
`engine/pattern.rs`:

```rust
/// Do two definitions of a role start the same process? (model, effort, sandbox, permission,
/// instructions, tools-relevant flags) — description, glyph, colour, budget and fallbacks
/// change nothing about a running agent.
pub fn role_spawn_eq(a: &Role, b: &Role) -> bool {
    a.kind == b.kind && a.model == b.model && a.effort == b.effort && a.sandbox == b.sandbox
        && a.permission == b.permission && a.instructions.trim() == b.instructions.trim()
        && a.mcp == b.mcp && a.can_spawn_swarm == b.can_spawn_swarm && a.parallel == b.parallel
        && a.context == b.context          // field added in WP-4.1; leave this line out until then
}
```

In `install_pattern`, replace the four comparisons of the form
`previous.roles.get(X) != self.pattern.roles.get(Y)` (manager, orchestrator, gate, planner) by
`!matches!((previous.roles.get(X), self.pattern.roles.get(Y)), (Some(a), Some(b)) if role_spawn_eq(a, b))`.

**(e) Say why an agent was respawned.** `respawn_planner`, `respawn_orchestrator`,
`respawn_manager`, `respawn_gate`, `respawn_finale` all log `… respawned (watchdog/manual)`.
Add a helper:

```rust
fn respawn_why(note: Option<&str>) -> &'static str {
    match note {
        Some(n) if n.starts_with("[mantra:watchdog]") => "watchdog",
        Some(n) if n.starts_with("[mantra:fallback]") => "fallback model",
        Some(n) if n.starts_with("The workflow") || n.starts_with("Workflow") => "workflow changed",
        Some(n) if n.contains("lost its session") => "session lost",
        Some(_) => "requested",
        None => "by you",
    }
}
```

Compute it at the top of each function (before `note` is consumed) and log
`{who} respawned ({why})`. Update tests that match the old string.

**Tests.** Resume in `Phase{Orchestrating}` with a saved planner thread: no planner is spawned
at boot; the first `escalate_to_planner` spawns it with `spawn_resumed` (the fake ctx records
the thread) and the prompt starts with `[mantra:resume] Mantra restarted; you are re-attached`.
Without a saved thread: the prompt starts with `[mantra:respawn] You are this run's planner`
and contains the brief and the current phase JSON. `install_pattern` with a pattern whose
planner differs only in `description` → the planner agent id is unchanged.
`respawn_planner` passes the queue on (WP-1.6) and includes `For continuity`.

---

## WP-3.4 A halted, quit or resumed run says so everywhere

**(a) Agents see the halt.** `Run::status_text` is what `mantra_status` returns to the planner
and the orchestrator; only the manager's overview mentions a halt. At the start of
`status_text` add, when halted:

```text
RUN HALTED ({reason label}) for {duration}: {halt message, first 300 chars}
Nothing starts until it is resumed (planner: mantra_revise_plan or mantra_resume_run; user: space).
```

(`reason_label` is in `helpers.rs`.) Because `status_fingerprint` drives the "no change since
your last mantra_status" answer, also add `self.halted()` to the fingerprint string.

**(b) Quitting leaves a trace.** `App::shutdown` (≈3764) is the clean-quit path
(`/quit`, three `ctrl+c`). Before `self.hub.shutdown_all()`: if a run is active,

```rust
run.log("⏻", "gray", format!("Mantra was closed with the run at {} — reopen it with: mantra runs resume {}", stage_label, run.id));
run.save_state();
```

and add `#[serde(default)] pub closed_unix: Option<u64>` to `RunState`, set in `save_state`
from a new `Run.closed_unix` that `shutdown` sets to now and `resume_boot` clears.

**(c) `mantra runs` and `/runs` tell live from closed from halted.**
`RunSummary::stage_label` (`engine/state.rs` ≈304) appends ` · halted`; the CLI then cuts the
column at 18 characters, which hid it on every halted sample of the day. Change the label so
the state comes **first**:

| condition | label |
|---|---|
| finished | as today (`done`, `stopped`, `failed`) |
| halted | `halted · {stage}` |
| not finished, no live owner (`state::live_owner(dir)` is `None`) | `closed · {stage}` |
| otherwise | `{stage}` |

In `main.rs` (≈861) widen the stage column from 18 to 30 (`{:<30}` and `trunc(.., 30)`); take
the width from the title column (36 → 28).

**(d) A pause is not a "halt" in the resume line.** `resume_boot` logs
`resumed at {label} (it was halted: paused by you)`. When the saved halt text is exactly
`paused by you`, log `resumed at {label} (you had paused it)` instead.

**(e) What changed since the run was saved.** At the end of `resume_boot`, one journal line
that lists the roles and the model each will run on now, so a resume is never silent about
models:
`⚙ roles: planner {alias} · orchestrator {alias} · {gate role} {alias} · {worker roles…}`
(use `model_for_spawn`-free plain values: pin if any, else the role's model; mark a pin with
`*`, and add ` (* = your per-agent choice)` when any is marked).

**Tests.** `status_text` of a halted run starts with `RUN HALTED`. A `RunSummary` for a halted
state labels `halted · phase 2 · working`; for an unfinished state without `owner.json`
`closed · …`. `resume_boot` on a state whose `halted` is `paused by you` logs
`you had paused it`.
