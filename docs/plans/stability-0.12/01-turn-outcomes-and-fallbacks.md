# Milestone 1 — Turns that fail honestly, and fallback chains

Covers: M-01, M-02, M-09 (part), M-23 (part), U-6, and the message-loss part of M-06.

Files touched: `src/agent.rs`, `src/hub/claude.rs`, `src/engine/pattern.rs`, `src/engine/run.rs`,
`src/engine/resume.rs`, `src/engine/state.rs`, `src/config.rs`, `src/app.rs`,
`bija/src/api/openai.rs`, `bija/src/api/mod.rs`, tests.

Read before starting: `Run::on_turn_done` (`engine/run.rs` ≈2839–3222),
`Run::fallback_after_usage_limit` (≈3295), `Run::on_first_token_deadline` (≈3829),
`Run::respawn*` (≈4471–4631), `agent::is_usage_limit_error` (`agent.rs` ≈1152),
`Translator::on_rate_limit` / `on_result` / `classify_claude_error` (`hub/claude.rs` ≈812–1014).

---

## WP-1.1 Recognise a usage limit as a usage limit

**Problem.** `You've hit your 5-hour limit · resets 9:20pm (UTC)` was classified as a
transient error: 8 `[mantra:retry]` prompts in 44 s, a respawn on the same model, then a halt.

**Change 1 — `src/agent.rs`.** Add next to `is_usage_limit_error`:

```rust
/// Claude Code's subscription wording ("You've hit your 5-hour limit · resets 9:20pm",
/// "… session limit …", "… weekly limit …", "… Opus limit …") and its close relatives.
/// `m` is lower-cased by the caller.
pub fn subscription_limit_text(m: &str) -> bool {
    (m.contains("hit your") && m.contains("limit"))
        || m.contains("limit reached")
        || m.contains("out of extra usage")
        || m.contains("credits_required")
}
```

In `is_usage_limit_error`, after `let m = msg.to_lowercase();` add
`if subscription_limit_text(&m) { return true; }`.

**Change 2 — `src/hub/claude.rs`.**

- Add a field `rate_limit_rejected: bool` to `Translator` (initialise `false` in `new`).
- In `on_rate_limit`: right after `status` is read, set
  `self.rate_limit_rejected = status == "rejected";` (so a later `allowed` clears it).
- Change the signature `fn classify_claude_error(v: &Value, msg: &str)` to take a third
  argument `rate_limited: bool`. First lines of the body become:
  ```rust
  let m = msg.to_lowercase();
  if rate_limited || crate::agent::subscription_limit_text(&m) {
      return Some("usageLimitExceeded");
  }
  ```
  (keep the existing context/budget checks after it). Pass `self.rate_limit_rejected` at the
  one call site in `on_result`.

**Nothing else changes here**: `agent::classify` already maps `usageLimitExceeded` to
`ErrKind::UsageLimit`, and `Run::on_turn_done` already tries the fallback first for that kind
and otherwise halts with `HaltReason::UsageLimit` without retrying.

**Tests to add.**
- `agent.rs` tests: `is_usage_limit_error(None, "You've hit your 5-hour limit · resets 9:20pm (UTC)")`,
  the same with `session limit` and `weekly limit`, are `true`; `"hit the limit of 3 retries"`
  is `false`; `is_transport_error(None, <limit text>)` is `false`.
- `hub/claude.rs` tests (next to `a_hot_rate_limit_warns_once_and_only_while_it_lasts`): a
  `rate_limit_event` with `status: "rejected"` followed by a `result` with `is_error: true`
  and `result: "anything"` yields `turn/completed` whose error has
  `codexErrorInfo == "usageLimitExceeded"`; after a later `status: "allowed"` event the same
  failing result is no longer classified that way.

---

## WP-1.2 Fallback chain in the data model (up to three)

**Change — `src/engine/pattern.rs`, `struct Role`.** Keep `fallback: Option<String>`. Add:

```rust
/// Second and third fallback, tried in order after `fallback`. Each is optional; a later one
/// without the earlier ones is moved up on load (`normalize_fallbacks`).
#[serde(default, skip_serializing_if = "Option::is_none")]
pub fallback2: Option<String>,
#[serde(default, skip_serializing_if = "Option::is_none")]
pub fallback3: Option<String>,
```

Add to `Default for Role`: both `None`. Add methods:

```rust
impl Role {
    /// The fallbacks in order: non-empty, without duplicates, without the role's own model.
    pub fn fallback_chain(&self) -> Vec<String> { /* from fallback, fallback2, fallback3 */ }
    /// Rewrites the three fields from `fallback_chain()` (so gaps close and duplicates go).
    pub fn normalize_fallbacks(&mut self) { /* assign chain[0], chain[1], chain[2] or None */ }
    /// Sets position `i` (0..=2) to `alias` (None clears it), then normalizes.
    pub fn set_fallback(&mut self, i: usize, alias: Option<String>) { ... }
}
```

Call `normalize_fallbacks()` for every role at the end of `Pattern::from_toml` (after the
parse, before `validate`). This is also what silently repairs "fallback equals the model",
which the validator accepted during the run.

**Every reader of `role.fallback` must use the chain.** Find them with:

```bash
grep -rn "\.fallback" mantra_src/src --include=*.rs
```

Expected sites and what to do:

| site | change |
|---|---|
| `engine/run.rs` `fallback_after_usage_limit`, `on_first_token_deadline` | replaced by WP-1.3 |
| `engine/run.rs` `validate_pattern_replacement` | loop over `role.fallback_chain()`; error text `role '{name}' names an unconfigured fallback model alias '{alias}' (not in /models of this running Mantra)`; use the same suffix for the primary model |
| `config.rs` `Registry::preflight` | both loops iterate `role.fallback_chain()` (label `fallback`, `fallback 2`, `fallback 3`) |
| `app.rs` `start_run_with` (re-binding of built-in aliases) | apply to all three fields, then `normalize_fallbacks()` |
| `ui/studio.rs` | WP-5.3 |
| `web/commands.rs` `SetRoleFallback`, `web/snapshot.rs`, web assets | WP-5.7 |

Until WP-5.3 lands, the Studio keeps editing only the first field; that is fine.

**Tests to add** (`pattern.rs` tests): a TOML with `fallback2` but no `fallback` loads with it
moved to `fallback`; `fallback = model` is dropped; three distinct fallbacks round-trip through
`to_toml`/`from_toml`; an old file without the keys loads unchanged.

---

## WP-1.3 One fallback routine, used by every "this model cannot go on" case

Today there are two different fallbacks: the usage-limit one pins the *agent*
(`set_agent_model`), the first-token one rewrites the *role* (`r.model = alias`). Replace both
by one routine and add two more triggers.

### State (add to `struct Run`, initialise in `Run::new`)

```rust
/// alias → provider id, from the app beside `known_models` (`set_known_providers`).
known_providers: HashMap<String, String>,
/// Providers that just failed for a reason that is the provider's, not the task's: usage limit
/// (60 min), silent twice or empty twice (15 min), rejected the request shape (the whole run).
/// provider id → (until, why). Runtime only.
provider_trouble: HashMap<String, (Instant, &'static str)>,
/// Per slot (`first_token_key`): aliases already tried and given up on in this run.
fallback_used: HashMap<String, Vec<String>>,
/// Per slot: consecutive empty turns (WP-1.4).
empty_strikes: HashMap<String, u8>,
```

Add `pub fn set_known_providers(&mut self, m: HashMap<String, String>)`. In `app.rs`, at the
three places that call `run.set_known_models(...)` (`start_run_with`, `resume_run`,
`model_switched_for_run_agent`), also call it with a new
`Registry::alias_providers(&self) -> HashMap<String, String>` (same key set as
`context_windows_for_default`: use `self.names()`, value `m.provider.clone()`, `"openai"` when
empty). WP-3.1 adds a fourth call site.

### Helpers (private, in `impl Run`)

```rust
fn provider_of(&self, alias: &str) -> Option<&str>          // known_providers lookup
fn troubled(&self, alias: &str) -> Option<&'static str>     // Some(why) if its provider has an unexpired entry
fn mark_trouble(&mut self, alias: &str, secs: u64, why: &'static str)
```

`troubled` returns `None` when `known_providers` is empty (tests) or the alias is unknown.

### The model a spawn uses

Add one function and use it at **every** spawn site:

```rust
/// The alias a spawn into `slot_key` runs on: the user's pin if any; else the role's model —
/// unless that model's provider is in trouble right now and the chain has one that is not.
fn model_for_spawn(&mut self, slot_key: &str, role: &Role, pin: Option<&str>) -> String
```

Rules, in order:
1. `pin` is `Some(p)` and `self.knows_model(p)` → `p` (a pin is the user's decision; if
   `troubled(p)` is `Some(why)`, journal once per slot:
   `⚠ {slot}: runs on {p} as you chose, although its provider {why}`).
2. `role.model` is not troubled → `role.model`.
3. First alias of `role.fallback_chain()` that is known, not troubled and not in
   `fallback_used[slot_key]` → that alias; journal
   `⇄ {slot} starts on fallback {alias} — {role.model}'s provider {why}`.
4. Otherwise `role.model`.

Spawn sites to change (each currently takes the model from `role_for_slot` or `role.model`):
`spawn_planner_with` (slot `"planner"`), `spawn_manager_with` (`"manager"`),
`spawn_orchestrator` (`"orchestrator"`), `spawn_gate_at_round` (`"gate"`), `start_finale`
(`"finale"`), `launch_worker_with` (`"worker:<task id>"`, pin = `w.model`). The simplest way:
inside `role_for_slot` (and in `launch_worker_with` where `w.model` is applied) call
`model_for_spawn` and assign the result to `role.model`. `role_for_slot` then needs `&mut self`;
adjust its five callers.

### The routine

```rust
#[derive(Clone, Copy, PartialEq)]
enum FallbackWhy { UsageLimit, Silent, Empty, Rejected }

/// Move agent `a` to the next model of its role's chain. `false` when the run is halted, the
/// agent has no role, or nothing in the chain is left — the caller then does what it did before.
fn try_fallback(&mut self, ctx: &mut dyn Ctx, a: AgentId, why: FallbackWhy, error: &str) -> bool
```

Body (adapt the existing `fallback_after_usage_limit`, which already has the right shape):

1. `if self.halted() { return false; }`; `role_name = self.role_name_of(a)?`;
   `key = self.first_token_key(a)?`.
2. `running` = exactly as today (worker pin → slot pin → agent's alias → role model).
3. `mark_trouble(&running, secs, text)` with
   `UsageLimit → (3600, "hit its usage limit")`, `Silent → (900, "gave no output")`,
   `Empty → (900, "returned empty turns")`, `Rejected → (86_400, "rejected the request")`.
4. `self.fallback_used.entry(key.clone()).or_default().push(running.clone());`
5. `next` = first alias in `role.fallback_chain()` with: not in `fallback_used[key]`,
   `!= running`, `self.knows_model(alias)`, `self.troubled(alias).is_none()`.
   `None` → `return false`.
6. Pin and restart **exactly as `fallback_after_usage_limit` does today**
   (`forget_agent_session`, `set_agent_model(a, &next)`, worker: relaunch in the same worktree
   with the note joined to the prompt; non-worker: `self.respawn(ctx, a, Some(note))`).
   The note:
   ```text
   [mantra:fallback] The previous model ({running}) {reason}. You are its replacement on {next}. Your current files contain the work so far. Continue where it left off.
   ```
   `{reason}`: `hit its provider's usage limit ({trunc(error,240)})` · `produced no output twice in a row`
   · `returned an empty turn twice in a row` · `was rejected by its provider ({trunc(error,240)})`.
7. `clear_replaced_agent(a)`; clear `first_token_strikes[key]` and `empty_strikes[key]`.
8. Journal and alert (one call to `self.alert`):
   ```text
   ⇄ {name}: {running} → fallback {next} ({usage limit | no output | empty turns | rejected})
   ```
   Use glyph `⇄` (`theme::g("⇄", "<>")` is applied by the frame's ASCII pass; just log `"⇄"`),
   colour `"amber"`.
9. `self.save_state(); true`.

Delete `fallback_after_usage_limit`. In `on_turn_done` replace its call by
`self.try_fallback(ctx, a, FallbackWhy::UsageLimit, &msg)`.

In `on_first_token_deadline`, replace the whole `if let (Some(alias), Some(role_name)) = (fallback, role_name) { … }`
block by `if self.try_fallback(ctx, a, FallbackWhy::Silent, "") { return; }`. (The role's
model is no longer rewritten; the trouble mark makes later spawns of that role start on the
fallback through `model_for_spawn`.)

New trigger for a rejected request: in `on_turn_done`, just before the line
`let reason = match kind { … }`, add:

```rust
if kind == Some(ErrKind::ProviderRejected) && self.try_fallback(ctx, a, FallbackWhy::Rejected, &msg) {
    return;
}
```

### Soft halt: a halt about one agent's provider does not stop the others (M-09)

In `halt_full`, the loop `for a in self.all_agents() { … ctx.interrupt(a) … }` interrupts every
busy agent. Change it so that when `reason` is one of
`Auth | UsageLimit | ProviderRejected | AgentTurnFailed` **and** `agent.is_some()`, only that
agent is interrupted (if busy). After the loop, when at least one *other* agent was busy and
left alone, journal:

```text
‖ {n} agent(s) keep working until their turn ends — the halt is about {name} only
```

The existing pause boundary already handles turns that end during a halt (`defer`, `Pending`),
and WP-2.3 closes the one gap. Do **not** change `HaltReason::User`, `Environment`,
`GateExhausted`, `AttemptsExhausted`: those still interrupt everyone.

### Persistence

`fallback_used` must survive a resume, or a resumed run retries a dead model. Add to
`RunState` (`engine/state.rs`) `#[serde(default)] pub fallback_used: BTreeMap<String, Vec<String>>`,
write it in `save_state`, read it in `Run::from_state`. `provider_trouble` is deliberately not
saved (a restart is a fair moment to try the primary again).

### Tests to add (`engine/run/tests.rs`)

1. Planner role with `fallback = "b"`, `fallback2 = "c"`: a failed turn with
   `ErrKind::UsageLimit` respawns the planner on `b` (check `ctx.spawned("planner")` last
   model), no `[mantra:retry]` prompt was sent, the run is not halted. A second usage-limit
   failure goes to `c`. A third halts with `HaltReason::UsageLimit`.
2. With `known_providers = {a: P1, b: P1, c: P2}`: a usage limit on `a` skips `b` (same
   provider) and lands on `c`.
3. After (1), `spawn_gate_at_round` for a gate role whose model is on the troubled provider
   starts on its fallback (journal contains `starts on fallback`).
4. A worker on a pinned model (`w.model = Some("x")`) keeps `x` at respawn even when `x`'s
   provider is troubled, and the warning line is journalled.
5. `ProviderRejected` with a chain → fallback; without → the existing halt (existing tests
   must still pass).
6. Soft halt: with one busy worker and a planner whose turn fails with `Auth` and no chain,
   the worker was not interrupted (`TestCtx` records interrupts), the run is halted.
7. `fallback_used` round-trips through `RunState`.

Update the existing first-token fallback test(s) that assert `pattern.roles[..].model` was
rewritten: they must now assert the slot pin / agent model instead.

---

## WP-1.4 An empty turn is not a finished turn

**Definition.** A turn is *empty* when it completed without error and produced no agent text,
no command, no file change, no tool call and no plan update. (Reasoning alone does not count
as output.)

**Change 1 — `src/agent.rs`.** Add `pub last_turn_empty: bool` to `Agent` (default `false`).
In `apply`, branch `"turn/completed"`, where `compact_turn` is computed from `turn_items`, also
compute:

```rust
self.last_turn_empty = status == "completed"
    && !turn_items.iter().any(|i| match &i.kind {
        Kind::Agent | Kind::Plan => !i.text.trim().is_empty(),
        Kind::Command { .. } | Kind::Files { .. } | Kind::Tool { .. } | Kind::Web { .. } => true,
        _ => false,
    });
```

(Compute it before `out.push(Signal::TurnDone …)`; a compaction-only turn sends no `TurnDone`
and is unaffected.)

**Change 2 — `Run::on_turn_done`.** In the `else if status == "completed"` branch, *before*
the counters are cleared, add:

```rust
if ctx.agent(a).map(|x| x.last_turn_empty).unwrap_or(false) && !was_yielded {
    if self.on_empty_turn(ctx, a) { return; }
}
```

and clear `self.empty_strikes` for the slot when the turn was not empty.

```rust
/// `true` = handled (retry scheduled, fallback started, or failure reported).
fn on_empty_turn(&mut self, ctx: &mut dyn Ctx, a: AgentId) -> bool
```

- `key = first_token_key(a)` (return `false` if none); `n = ++empty_strikes[key]`;
  `alias` = the agent's `model_alias`; `name = name_of(a)`.
- `n == 1`: journal `⚠ {name}: empty turn from {alias} (no text, no tool call) — asking again`
  (amber). If it is a worker set `WState::Retrying(now + 2s)`. Push to `continue_queue`
  (2 s):
  ```text
  [mantra:retry] Your previous turn ended without any output or tool call. Continue the task. If it is finished, end with your report.
  ```
  Return `true`.
- `n >= 2`: `empty_strikes.remove(key)`. If `try_fallback(ctx, a, FallbackWhy::Empty, "")` →
  `true`. Otherwise report a failure with the error text
  `empty turn twice from {alias} (no text, no tool call)`:
  worker → `self.on_worker_done(ctx, wi, "failed", Some(text))` (this tells the orchestrator,
  or — for a gate fix worker — escalates, exactly like any failed turn); non-worker, not the
  manager → `self.halt_turn_failed(ctx, Some(a), format!("{name}: {text}"))`; the manager →
  drop it as the existing manager failure branch does. Return `true`.

**Change 3 — a worker turn that ends without a `STATUS:` line.** In `on_worker_done`,
`"completed"` arm: when `status_line.is_none()`, the worker is not waiting on a question, and
`self.workers[wi].state != WState::Done` (not a follow-up), and this worker has not been asked
before (add `pub status_asked: bool` to `Worker`, default `false`): set `status_asked = true`,
journal `? {tid} ended its turn without a STATUS line — asking for it`, prompt

```text
[mantra] Your turn ended without the closing lines. If the task is finished, reply with exactly: STATUS: done (or blocked) and SUMMARY: <2-5 lines>. If you are waiting for a decision, call mantra_ask and end your turn.
```

and `return` (do not mark it done). A second turn without the line is treated as today
(done), so this can never loop.

**Tests to add.** Worker: empty turn → not `Done`, journal has `empty turn`, one retry prompt;
second empty turn with a chain → respawned on the fallback; without a chain → the orchestrator
gets `FAILED`. Gate fix worker (stage `Fix`): two empty turns without chain → halted and
escalated, and **no** checks job was started. Planner in `Planning`: empty turn → retry prompt,
not the "You haven't submitted a valid plan" nudge. Worker without STATUS: first time asked,
second time done.

---

## WP-1.5 bija: tool calls under any choice index; a promised tool call that never arrives is an error

File `bija/src/api/openai.rs`.

1. **Streaming (`on_event`, ≈218–240).** Today only the choice with `index == 0` is read.
   Change to: text and reasoning deltas from choice 0 as now; **tool-call deltas from every
   choice** in the chunk; `finish_reason` from the first choice that has one. Because two
   choices can both carry `tool_calls[].index == 0`, key the accumulating calls by
   `(choice_index, call_index)` and emit them in order of first appearance.
2. **End of stream (`finish`, ≈285–299) and the non-streaming parser (≈309–331).** If the
   provider's finish reason was `tool_calls` / `function_call` and no tool call was parsed,
   return `Err(ApiError::new(ErrorKind::Transient, "the provider announced tool calls but none arrived in the response"))`
   instead of an answer. The retry loop in `bija/src/api/mod.rs` (≈87) then retries it and,
   after the retries, the turn **fails** — which Mantra handles — instead of "completing" empty.

**Tests to add** (next to the existing SSE tests): a stream with text under choice 0 and the
tool call under choice 1 yields one tool call; `finish_reason: "tool_calls"` with no call →
`Err` of kind `Transient`; the existing tests stay green.

---

## WP-1.6 Messages survive a failing or replaced agent

**Change 1 — `app.rs`, `on_hub`, `HubEvent::Notif`, `method == "turn/completed"`.** The queue
is flushed after every completed turn. Read the status first:
`let failed = params.pointer("/turn/status").and_then(|s| s.as_str()) == Some("failed");`
and skip the `queued` flush when `failed` (leave `a.queued` as it is; the chips stay visible;
the existing `turn/started` drain delivers them into the next turn that starts — the retry or
the fallback's first turn).

**Change 2 — carry the queue to a replacement.** Add to `trait Ctx` (`engine/run.rs`):

```rust
/// `to` replaces `from` in its slot (a respawn, a fallback): what was queued for `from` is
/// now `to`'s. Default: nothing to carry.
fn carry_over(&mut self, _from: AgentId, _to: AgentId) {}
```

Implement in `impl Ctx for Ctxt` (`app.rs`): move `agents[from].queued` to the end of
`agents[to].queued`. Call it in `Run::respawn_planner` right after the new planner is spawned
(`ctx.carry_over(old, id)` — capture `old` before `self.planner.take()`). The planner is the
only agent users talk to, so no other site needs it.

Also record replacements for the UI: add `pub replaced: Vec<(AgentId, AgentId)>` to `Run`,
push `(old, id)` in `respawn_planner`, `respawn_orchestrator`, `respawn_manager`,
`respawn_gate`, and in `try_fallback`'s worker branch. `App::tick` drains it: if
`self.screen == Screen::Zoom(old)` → `self.set_screen(Screen::Zoom(new))`.

**Change 3 — re-deliver agent-to-agent messages a halt may have swallowed.** Add to `Run`:

```rust
recent_prompts: HashMap<AgentId, VecDeque<(Instant, String)>>,   // last 5 per agent
redeliver: HashMap<AgentId, Vec<String>>,
```

- In `handle_tool` `"mantra_prompt"` (both the answering and the plain path): after
  `ctx.prompt(t, …)` push `(now, text_as_sent)` to `recent_prompts[t]` (cap 5).
- In `halt_full`, for every agent that gets `ctx.interrupt(a)`: move the entries of
  `recent_prompts[a]` younger than 20 s into `redeliver[a]`.
- In `resume_core`, in the final loop that sends
  `[mantra:resume] The run was paused and is resuming now. Continue where you left off.`,
  append when `redeliver` has entries for that agent:
  ```text

  Sent to you just before the pause — it may not have reached you (ignore what you already acted on):
  - {message 1}
  - {message 2}
  ```
- **Hold orchestrator → worker messages during a halt.** In `"mantra_prompt"`: if
  `self.halted()`, the caller is the orchestrator, and the target is a worker, the gate agent
  or the finale agent: do not prompt. Push the text to `redeliver[t]`, keep a pending question
  answered (`pending_questions.remove(&t)` as now) and return
  `("held — the run is halted; {n} gets this when it resumes".into(), true)`.
  (The planner and the manager keep prompting during a halt; that is how halts get resolved.)

**Change 4 — say in the journal when a user message is delivered or taken back.** In `app.rs`:
where the queue is flushed for an agent that is `in_run` (both the `turn/started` drain and the
`turn/completed` flush) call `run.log("›", "rose", format!("delivered to {name}: {}", trunc(first, 60)))`
once per flush (`first` = the first queued text; add `(+{n} more)` when there are several).
Where backspace on an empty input pops the last queued message back for editing (search for
`queued.pop()` in `app.rs`), log `↩ a queued message was taken back for editing` (gray).

**Tests to add.** `app.rs` tests (see `enter_queues_behind_a_busy_turn_without_steering` for
the harness): a failed `turn/completed` leaves `queued` intact; a completed one flushes.
Engine: `respawn_planner` calls `carry_over(old, new)`; after a halt that interrupted a worker
which had been prompted 5 s earlier, the resume prompt contains the "Sent to you just before
the pause" block; an orchestrator `mantra_prompt` during a halt returns `held —` and sends
nothing until resume.

---

## WP-1.7 The quota warning reaches the journal

`Translator::on_rate_limit` already emits one `warning` notification per episode
(`provider rate limit: the five_hour window is 91% used`). It only becomes an agent notice.

In `app.rs` `on_hub`, `HubEvent::Notif`: when `method == "warning"`, the message starts with
`provider rate limit:` and `self.in_run(agent)`, also journal it:

```text
⚠ {agent name}: {message}{suffix}
```

`{suffix}` is ` — role {role} has no fallback model (Studio → fallback)` when that agent's role
has an empty `fallback_chain()`, else empty. Colour `"amber"`.

**Test.** Feeding a `warning` notification with that prefix to an app with a run adds exactly
one pulse line; a second identical notification from the translator does not exist (the
translator dedups), so no extra guard is needed.
