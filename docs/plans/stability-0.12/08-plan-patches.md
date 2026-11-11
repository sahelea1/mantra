# Milestone 8 — Small plan revisions without re-sending the plan

Covers: M-03 and the part of M-13 it causes. Recommended, not required for the stability
milestones; do it last.

Files touched: `src/engine/plan.rs`, `src/engine/run.rs`, `src/engine/tools.rs`, tests.

## Why

Every `mantra_submit_plan` / `mantra_revise_plan` call carries the whole plan. In the run this
plan comes from, that was 15 calls of 86–107 KB each: about 678,000 output tokens (81% of the
planner's output), 6.5–7.3 minutes of generation per call, and therefore 6.5–8 minutes for
every halt that needed the planner — even when the revision changed one gate check.
`mantra_read_plan` returned 100 KB and overflowed the client's tool-result limit both times it
was called.

The fix is a second, small door into the same validated path. Nothing about how a plan is
validated, stored or applied changes.

---

## WP-8.1 `mantra_patch_plan`

### Tool (`engine/tools.rs`, in `planner_tools`, after `mantra_revise_plan`)

```rust
tool(
    "mantra_patch_plan",
    "Change parts of the plan without sending it again. Give a list of operations; Mantra applies them to the current plan (or to the plan it just rejected), validates the result exactly like mantra_revise_plan and, if it is valid, makes it live. Use this for every small change — one gate check, one task prompt, one added task. Use mantra_revise_plan only when most of the plan changes.",
    json!({
        "ops": {"type": "array", "items": {"type": "object", "properties": {
            "op": {"type": "string", "enum": ["set_task", "add_task", "remove_task", "set_gate", "set_check", "add_check", "remove_check", "set_phase", "set_plan"]},
            "phase": {"type": "string", "description": "phase id (as in the plan), or its 1-based number"},
            "task": {"type": "string", "description": "task id"},
            "index": {"type": "integer", "description": "1-based position of a gate check"},
            "value": {"description": "see the operation"}
        }, "required": ["op"]}},
        "reason": {"type": "string"}
    }),
    &["ops", "reason"],
),
```

Operations (`value` shapes):

| op | needs | `value` | effect |
|---|---|---|---|
| `set_task` | `task` | object with any of `title`, `role`, `prompt`, `scope`, `acceptance`, `effort` | replaces those fields of the task |
| `add_task` | `phase` | a whole task object (`id`, `role`, `prompt`, …) | appends it to the phase |
| `remove_task` | `task` | — | removes it |
| `set_gate` | `phase` | object with any of `checks` (array), `focus`, `criteria` | replaces those fields of the phase's gate |
| `set_check` | `phase`, `index` | string | replaces one check |
| `add_check` | `phase` | string | appends a check |
| `remove_check` | `phase`, `index` | — | removes one check |
| `set_phase` | `phase` | object with any of `name`, `goal` | replaces those fields |
| `set_plan` | — | object with any of `title`, `summary`, `orchestrator_brief`, `final_checks` | replaces those top-level fields |

### Applying (`engine/plan.rs`)

```rust
/// Apply `ops` to a copy of `plan`. The first operation that cannot be applied stops it with a
/// message that names the operation's position and what is wrong.
pub fn apply_ops(plan: &Plan, ops: &Value) -> Result<Plan, String>
```

- `phase` resolves by id first, then by 1-based number; unknown →
  `op {i} ({name}): no phase '{x}' (phases: {ids})`.
- `task` resolves across all phases; unknown → `op {i} ({name}): no task '{x}'`.
- `index` out of range → `op {i} ({name}): phase '{p}' has {n} check(s)`.
- A `value` of the wrong type → `op {i} ({name}): value must be {what}`.
- `set_task` with a new `id` in `value` is refused (`a task's id cannot change — remove it and add a new one`).
- Accept `value` given as a JSON string of the object (the same model quirk `Plan::from_value`
  tolerates): if an object is expected and a string arrives, try to parse it.

### Engine (`engine/run.rs`)

1. **Keep a rejected plan as a draft.** Add `plan_draft: Option<Plan>` to `Run`. In
   `handle_tool`, `"mantra_submit_plan" | "mantra_revise_plan"`: when validation fails, store
   the normalised plan in `plan_draft` before returning the rejection, and append to the
   rejection text:
   `\nThe plan you sent is kept as a draft: fix it with mantra_patch_plan (only the changed parts) instead of sending it again.`
   Clear `plan_draft` when a plan is accepted.
2. **Factor the acceptance path.** Move everything in that arm from
   `plan.normalize();` to the end into
   `fn accept_plan(&mut self, ctx: &mut dyn Ctx, mut plan: Plan, revising: bool) -> (String, bool)`
   (behaviour identical: merge with completed phases when revising, validate, store, version
   bump, `save_plan`, `retitle`, loop-planner branch, `apply_revision`).
3. **The new arm.**
   ```rust
   "mantra_patch_plan" => {
       // same guards as revise: planner only, not Auto, loop-planner rules, live-plan lock
       let reason = s("reason");
       if reason.trim().is_empty() { return ("REJECTED: say why the plan changes".into(), false); }
       let base = match self.plan_draft.clone().or_else(|| self.plan.clone()) {
           Some(p) => p,
           None => return ("REFUSED: there is no plan yet — submit one with mantra_submit_plan".into(), false),
       };
       let patched = match super::plan::apply_ops(&base, args.get("ops").unwrap_or(&Value::Null)) {
           Ok(p) => p,
           Err(e) => return (format!("REJECTED: {e}. Nothing was changed."), false),
       };
       let revising = self.phase_idx().is_some();
       let n = args.get("ops").and_then(|o| o.as_array()).map(|a| a.len()).unwrap_or(0);
       let (text, ok) = self.accept_plan(ctx, patched, revising);
       if ok { self.log("☰", "saffron", format!("plan patched: {n} operation(s) — {}", trunc(&reason, 60))); }
       (text, ok)
   }
   ```
   A patch may not touch a completed phase: in `apply_ops`' caller, after patching and before
   `accept_plan`, if `revising` and any phase with index `< current` differs between `base` and
   `patched` → `REJECTED: phase '{id}' is already finished and cannot be changed`.
   When the patched draft fails validation again, keep the patched version as the new draft
   (so fixes accumulate) and return the errors.
4. **Protocol.** `PLANNER_PROTOCOL`: replace
   `- Mantra validates the plan and returns errors; fix them and resubmit.` by

   ```text
   - Mantra validates the plan and returns errors. Fix them with `mantra_patch_plan` — send only the operations that change something; the rejected plan is kept as a draft, so never send the whole plan a second time.
   - Later changes (a gate check, a task prompt, an added task) also go through `mantra_patch_plan`. `mantra_revise_plan` re-sends everything and is only for a plan that changes in most places.
   ```
   and in the escalation prompt (`escalate_to_planner`) and its reminder, where
   `mantra_revise_plan` is offered, write `mantra_patch_plan (or mantra_revise_plan for a large change)`.
   The manager's and the gate's texts that mention "the planner can revise the plan" stay.

**Tests** (`plan.rs` and `run/tests.rs`):

- `apply_ops`: each operation; unknown phase/task/index errors name the position; a
  stringified object value is accepted.
- A rejected `mantra_submit_plan` (overlapping scopes) followed by
  `mantra_patch_plan [{op:"set_task", task:"b", value:{scope:["src/b/**"]}}]` → `ACCEPTED`,
  stage goes to `Review` as after a normal submit, `plan_version == 1`.
- At the gate, halted and escalated: `mantra_patch_plan [{op:"set_check", phase:"p2", index:3, value:"true"}]`
  → the run resumes, the checks re-run with the new command (WP-2.7), `plan_version` is +1,
  the journal has `plan patched: 1 operation(s)`.
- A patch that touches a finished phase is rejected and changes nothing.
- A patch whose result is invalid is rejected with the validator's messages; a following
  corrective patch succeeds.

---

## WP-8.2 `mantra_read_plan` that fits in a tool result

`mantra_read_plan` returns the whole plan as pretty JSON (100 KB in the run above; the client
truncated it to a stub both times).

Tool schema: add optional properties
`"phase": {"type": "string", "description": "phase id or 1-based number: return only that phase, in full"}`
and `"full": {"type": "boolean", "description": "return the whole plan even when it is large"}`.

Behaviour in `handle_tool`:

- `phase` given → that phase as pretty JSON (unknown → an error listing the phase ids).
- else if `full` is true, or the pretty JSON is at most 30,000 characters → the whole plan.
- else → an **outline** followed by how to read more:

  ```text
  Plan v{n}: {title}
  {summary}

  orchestrator_brief: {chars} characters
  final_checks: {k}
  Phases:
  - {id} · {name} — {done | CURRENT | pending} · {t} task(s): {task ids, comma separated} · {c} gate check(s)
  …

  The whole plan is {size} characters. Read one phase with mantra_read_plan(phase="<id>"), or pass full=true.
  ```

Update the tool's description:
`Read the plan as last accepted. A large plan comes back as an outline; pass phase="<id>" for one phase in full.`
and the respawn prompt in `respawn_planner` / `planner_context_pack` (WP-3.3), which says
"call mantra_read_plan first if you must revise other phases": make it
`call mantra_read_plan(phase=…) for another phase`.

**Tests.** A plan over 30,000 characters returns the outline (contains `Phases:` and every
phase id, is under 6,000 characters); `phase: "p2"` and `phase: "2"` return that phase's JSON;
`full: true` returns everything; a small plan is returned whole as before.
