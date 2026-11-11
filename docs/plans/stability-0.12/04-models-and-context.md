# Milestone 4 — Context limits and model discovery

Covers: U-2, U-5, M-22.

Files touched: `src/engine/pattern.rs`, `src/engine/run.rs`, `src/app.rs`, `src/config.rs`,
`src/discover.rs`, `src/hub/claude.rs` (read only), `src/ui/studio.rs` (field plumbing only;
the visible design is in `05-ui-ux.md`), tests.

---

## WP-4.1 A context limit per role; the smallest limit always wins

**The rule (state it in `DESIGN.md` §3 "Context management" too):**

> The context window an agent is told is the **minimum** of: the window of the model it
> actually runs on (the role's model, a pinned model, or a fallback), the role's `context`
> (if set), the pattern's `context_cap` (if not 0), and a halved window after a context-full
> error. It is recomputed whenever the agent's model changes.

**Data model — `engine/pattern.rs`, `struct Role`.**

```rust
/// Optional ceiling (tokens) on the context window of this role's agents, on top of the
/// pattern's `context_cap` and the model's own window: the smallest wins. None = no role limit.
#[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "deserialize_role_context")]
pub context: Option<u64>,
```

`deserialize_role_context`: accept an integer or text like `200k` / `1M` (reuse
`crate::util::parse_tokens`); `0`, `"off"`, `"none"`, `""` → `None`. `Default for Role`:
`None`.

`Pattern::validate`: for each role, `Some(c)` with `c < 16_000` →
`role '{n}': context must be at least 16k tokens (got {c})`. For the pattern setting:
`context_cap` of 1..=15_999 → `settings.context_cap must be 0 (no cap) or at least 16k tokens`.
(This is the validation M-22 asked for: a cap of 25,000 crashed Claude Code in 0.9; since then
the Claude argument is clamped to 100k–1M in `hub/claude.rs::build_args`, which stays.)

**Engine — `Run::context_for`.** Change the signature to take the role:

```rust
fn context_for(&self, role: &Role, over: Option<u64>) -> Option<u64> {
    let cap = self.pattern.settings.context_cap;
    self.known_models.get(&role.model).copied().into_iter()
        .chain(role.context)
        .chain(over)
        .chain((cap > 0).then_some(cap))
        .filter(|w| *w > 0)
        .min()
}
```

`role.model` here is the alias the spawn really uses: every caller already passes the role
*after* `role_for_slot` / the worker pin / `model_for_spawn` replaced `role.model`. Update the
six call sites (`spawn_planner_with`, `spawn_manager_with`, `spawn_orchestrator`,
`spawn_gate_at_round`, `start_finale`, `launch_worker_with`) from `&role.model` to `&role`.

Add for the app:

```rust
/// The ceiling the run puts on agent `a`'s context, whatever model it runs on: the smaller of
/// the pattern's cap and its role's `context` (and a worker's halved window). None = no ceiling.
pub fn context_ceiling(&self, a: AgentId) -> Option<u64>
```

**App — live model switch.** `App::set_model` (≈1491) computes
`let cap = self.run … .map(|r| r.pattern.settings.context_cap).filter(|c| *c > 0);`
Replace by `let cap = self.run.as_ref().filter(|r| r.all_agents().contains(&a)).and_then(|r| r.context_ceiling(a));`
so a role limit also holds after a live switch.

**Claude Code's floor.** `build_args` clamps `--autocompact` to at least 100k. When the
requested window is below 100,000 for a Claude Code agent, say so once on the agent: in
`app.rs::spawn_agent`, after `a.ctx_window` is set, if
`backend == ProviderKind::ClaudeCode && cw < 100_000`:
`a.notice(Level::Info, format!("context {}k is below Claude Code's minimum — it compacts at 100k", cw / 1000));`

**Check that the cap reaches Codex.** During the run a Codex process was seen with
`model_context_window=1000000` under a 500k cap. `Ctxt::spawn` computes
`min(run's value, registry.effective_context_window(model))` and `agent_spec` passes it as
`-c model_context_window=…`, which looks right. Add the test below; if it fails, the fault is
in that chain and must be fixed here.

**Tests.**
- `context_for`: model window 1,000,000, role `context = 250_000`, cap 500,000 → 250,000;
  the same role with `role.model` set to an alias whose window is 128,000 → 128,000; no role
  limit, cap 0 → the model's window; unknown alias → the smaller of role limit and cap.
- `agent_spec` for a Codex model with `context_override = Some(500_000)`: `extra_args`
  contains `model_context_window=500000` and `model_auto_compact_token_limit=425000` (85%).
- `Pattern::from_toml` reads `context = "250k"`, writes `context = 250000`, and rejects `8000`.
- `set_model` on a run agent whose role has `context = 250_000` to a 1M model leaves
  `agent.ctx_window == Some(250_000)`.

Studio field plumbing (so the milestone is usable before the UI milestone): add `"context"` to
`ROLE_FIELDS` after `"fallback"`, kind `Step` (typed, or ←→ through
`[none, 64k, 128k, 200k, 272k, 400k, 500k, 1M]`); `get_value` shows `none` or e.g. `250k`;
`apply_edit` parses with `parse_tokens`, empty/`none`/`0` clears. The final look is specified
in `05-ui-ux.md`.

---

## WP-4.2 `ctrl+d` finds the Claude Code and Codex models that exist now

### (a) Refresh the built-in list (the fallback for everything below)

`config.rs::CLAUDE_MODELS` — add, keeping every existing row:

```rust
("opus55",   "claude-opus-5-5",   1_000_000, ""),
("sonnet55", "claude-sonnet-5-5", 1_000_000, ""),
("fable52",  "claude-fable-5-2",  1_000_000, ""),
("haiku55",  "claude-haiku-5-5",  1_000_000, "fast & cheap"),
```

Order the table newest first within each family (Fable 5.2, 5.1, 5; Opus 5.5, 5, 4.8, 4.6;
Sonnet 5.5, 5; Haiku 5.5, 4.5). `--demo` injects this table; check the demo tests that count
Claude models.

### (b) Ask the installed Claude Code what the account offers

There is no documented `claude models` command. The Agent SDK's `supportedModels()` is served
by the CLI's stream-json control protocol: the host sends an `initialize` control request and
the response carries a `models` array of `{ value, displayName, description, resolvedModel?,
supportsEffort?, supportedEffortLevels? }`. **The wire format is not documented**, so this
step must be verified against the installed CLI before relying on it, and must degrade to (a).

Add to `src/discover.rs`:

```rust
/// The models the logged-in Claude Code offers, from its own `initialize` answer. Blocking
/// (spawns `claude`, waits at most 12 s); call it off the UI task. `Err` = could not ask.
pub fn claude_cli_models(claude_cmd: &[String], provider_id: &str) -> Result<Vec<Candidate>, String>
```

1. Spawn `claude_cmd` + `["-p", "--input-format", "stream-json", "--output-format", "stream-json", "--verbose"]`
   with the same environment scrubbing as `hub/claude.rs::apply_env` (remove inherited
   `CLAUDE*` / `ANTHROPIC_*`; set `IS_SANDBOX=1` when root), stdin and stdout piped, stderr
   null, cwd = a fresh empty temp dir (so no project settings load).
2. Write one line to stdin:
   `{"type":"control_request","request_id":"mantra-models","request":{"subtype":"initialize"}}`
3. Read stdout lines until one parses as JSON with `"type":"control_response"`, or 12 s pass.
   Then kill the child and wait for it.
4. Parse tolerantly — `fn models_from_initialize(v: &Value) -> Vec<(String, Option<String>, Vec<String>, String)>`:
   search the response recursively for the first array under a key named `models` whose
   elements are objects with a string `value`. For each element: `value`,
   `resolvedModel` (string, optional), `supportedEffortLevels` (array of strings, optional),
   `displayName` (optional).
5. Map each element to a `Candidate`:
   - model id = `resolvedModel` if present, else `value`. Skip `value == "default"` and ids
     that do not start with `claude-` after resolution **unless** no `resolvedModel` exists at
     all in the answer (older CLIs list aliases only: then keep `value` as the model id, e.g.
     `opus`, `sonnet[1m]` — the CLI accepts aliases for `--model`).
   - `context`: the window from `CLAUDE_MODELS` when the id is in the table; else
     `claude_window_guess(id)`: 200,000 for ids containing `haiku-4`, 1,000,000 otherwise.
     `context_known: true`.
   - `efforts`: `supportedEffortLevels` when non-empty, else `["low","medium","high","xhigh","max"]`;
     `default_effort: "high"`; `vision: true`; `note`: the display name when it differs from
     the id, else empty.
   - Deduplicate by model id.
6. No `models` array found, a timeout, or a spawn error → `Err(short reason)`.

**Verification the implementer must do** (and record in the PR description): run the command
by hand with the installed CLI, send the line from step 2, save the raw response line as
`src/testdata/claude-initialize.jsonl`, and write the parser test against that file. If the
installed CLI does not answer an `initialize` request in this mode, keep the function, make it
return `Err("this Claude Code version does not list its models")`, and still ship (a). If no
`claude` binary is available in the build environment, write the parser test against a
hand-written fixture of the documented `ModelInfo` shape and mark the live check as not done.

### (c) Wire it into discovery

`App::discover` (`app.rs` ≈1965) answers Claude Code providers instantly from the built-in
table. Change the loop
`for p in provs.iter().filter(|p| p.kind == ProviderKind::ClaudeCode) { self.on_discovered(p.id.clone(), Ok(claude_defaults(&p.id))); }`
to:

- For a Claude Code provider with a `base_url` (a gateway): unchanged (built-ins now, plus the
  network query that already runs for it).
- For a subscription provider (no `base_url`): push its id into the overlay's `loading` list
  and spawn a blocking task (`tokio::task::spawn_blocking`) that calls
  `claude_cli_models(&claude_cmd, &id)` and sends `AppEvent::Discovered`:
  - `Ok(found)` → `found` followed by every built-in candidate whose model id is not in
    `found` (so nothing a user already relies on disappears from the list).
  - `Err(why)` → the built-ins, and push a line to the overlay's `skipped` list (not `errors`:
    the list is still usable): `{id}: built-in model list ({why})`. To carry that, extend the
    event's result type or send a second small event — the simplest is a new variant
    `AppEvent::DiscoverNote { source: String, note: String }` handled by pushing to `skipped`.
- In demo mode (`self.demo`) keep the instant built-in answer and do not spawn `claude`.

**Codex.** Its catalogue is already live (`model/list`). Two gaps to close:
- `discover` only asks Codex when `only.is_none()`. `models_key` (`ui/models.rs` ≈748) calls
  `discover(Some(id))` for a provider row; for the synthetic Codex row
  (`ProvRow::Codex`) make it call `app.discover(Some("openai".into()))`, and in `discover`
  compute `let codex = (only.is_none() || only.as_deref() == Some("openai")) && …`.
- The request uses `{"limit": 100}` and reads `data` once. If the answer carries a
  `nextCursor` (or `next_cursor`), request again with `{"limit": 100, "cursor": …}` until it
  is absent (cap at 5 pages).

**Stale notes.** `discover::from_provider` writes the context's provenance into the model's
note (`{provider} · ctx from API` / `ctx assumed 200k`), which then sits next to a window the
user has since edited ("ctx assumed 200k" beside "1000k ctx"). Stop writing provenance into
`note`: the note of a discovered model is `vision` when it takes images, else empty. The
picker and `/models` already show the window and mark an unset one `(default)`.

**Tests.**
- `models_from_initialize` on the fixture: finds the ids; tolerates the array at
  `response.models`, `response.response.models` and one level deeper; ignores `default`.
- `claude_window_guess`.
- Discovery merge: CLI result containing `claude-opus-5-5` plus built-ins → one row per model
  id, `claude-opus-5-5` present once.
- `from_provider` note no longer contains `ctx`.
- Existing discovery tests (`discovery add/fill-without-clobbering`) stay green.

### (d) What the user sees

After `ctrl+d` in `/models` the discover overlay lists, under the Claude Code provider, every
model the CLI reported (new ones pre-selected when there are twelve or fewer, as for any
provider), each as `New`, `Update` or `Configured`. Aliases for new entries come from
`unique_alias` (e.g. `claude-opus-5-5`); when the model id is in `CLAUDE_MODELS` use that
table's short alias (`opus55`) instead, if it is free.
