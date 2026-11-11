# Milestone 5 — Terminal and web UI

Covers: U-1, U-3 (the visible half), U-6 and U-7 (the visible half), the display parts of
M-06, M-09, M-10, M-16 and M-17, and the UI faults the monitoring session ran into.

This file is deliberately explicit. Every layout is drawn, every string is given, every colour
is named. **Do not design; build what is drawn.** Where a width does not fit, the rule for what
gives way is stated.

Files touched: `src/config.rs`, `src/ui/theme.rs`, `src/ui/mod.rs`, `src/ui/overlays.rs`,
`src/ui/studio.rs`, `src/ui/stage.rs`, `src/ui/models.rs`, `src/ui/solo.rs` (header chip only),
`src/app.rs`, `src/engine/run.rs` (small accessors), `src/web/snapshot.rs`,
`src/web/commands.rs`, `src/web/protocol.rs`, `src/web/assets/**`, `src/main.rs` (doctor text),
tests, `scripts/stress.sh` expectations if a matched string changes.

## 0. The visual language that exists (keep it)

- Colours come only from `ui/theme.rs`. Text: `TEXT`, `MUTED`, `DIM`, `FAINT`. Status:
  `GREEN` running/done, `AMBER` waiting/warning, `RED` failed, `ROSE` "being prompted right
  now", `SIGNAL_RED`/`ACCENT` selection and focus. Role identity: the role's own colour
  (`theme::named(&role.color)`).
- A selected stage card has a heavy border (`┏━┓`) in `SIGNAL_RED`, bold. A selected row in a
  list has the `BAND` background (`form::band_line`).
- Motion: `anim::spinner()`, `anim::march(ms)` (marching arrows on an edge), `anim::breath(ms)`
  (slow pulse), `anim::shimmer` (activity text). All must respect `theme::motion()`:
  when motion is off, draw the static form (no marching, a plain arrow).
- Every glyph has an ASCII fallback via `theme::g(unicode, ascii)`.
- Footers are `(key, what)` pairs drawn by `ui::footer`; at most eight pairs.
- Nothing may overflow: use `trunc`, `w_of`, saturating arithmetic, and draw through `Cv`
  (which clips) on the stage.

New glyphs introduced by this milestone (unicode, ascii): provider mark `■` `#`; fix badge
`⚒` `F`; arrows on edges `▴` `^`, `▾` `v`, `▸` `>`, `◂` `<`; fallback mark `↪` `>`; scope
marks `◉` `(o)` and `○` `( )`.

---

## WP-5.1 Every provider has a colour, shown with its name

### Data

`config.rs`, `struct ProviderEntry`: add

```rust
/// One of `engine::pattern::COLORS`: the colour this provider is marked with wherever a model
/// is shown or chosen. Assigned automatically when empty (`Registry::assign_provider_colors`).
#[serde(default, skip_serializing_if = "String::is_empty")]
pub color: String,
```

```rust
/// Colours providers are given, in order. Red and gray are left out (failure, disabled).
pub const PROVIDER_COLORS: &[&str] = &["cyan", "violet", "saffron", "teal", "rose", "green", "amber", "blue"];
/// Codex's own account has no [[provider]] entry; its colour is fixed.
pub const CODEX_ACCOUNT_COLOR: &str = "blue";

impl Registry {
    /// Gives every provider without a (valid) colour the first palette colour no other
    /// provider uses; when all are used, the palette repeats by position. True when changed.
    pub fn assign_provider_colors(&mut self) -> bool
    /// The colour name for a provider id ("" or "openai" = the Codex account).
    pub fn provider_color(&self, id: &str) -> &str
}
```

`assign_provider_colors` skips `blue` for the first seven providers (it is the Codex
account's) — which the palette order above already does. Call it in `Registry::load` (both
`Ok` arms, before `restore_secrets_and_migrate`; pass `changed || colours_changed` so the file
is written once) and at the top of `App::save_registry`. An unknown provider id resolves to
`"gray"`.

### Drawing helpers (`ui/mod.rs`)

```rust
/// "■ Riti": the provider's mark and name, both in the provider's colour. `max` = columns for
/// the name. The one way a provider is named next to a model.
pub fn provider_chip(reg: &Registry, provider_id: &str, max: usize) -> Vec<Span<'static>>
/// "■" alone in the provider's colour, for places with no room for the name.
pub fn provider_mark(reg: &Registry, provider_id: &str) -> Span<'static>
```

`provider_chip` = `[Span::styled(format!("{} ", theme::g("■", "#")), fg(col)), Span::styled(trunc(name, max), fg(col))]`
with `col = theme::named(reg.provider_color(id))` and `name = reg.provider_name(id)`.
On a selected (band) row keep the same foreground; do not bold it.

### Where it appears (all of these, nothing else)

| place | before | after |
|---|---|---|
| model picker rows | `via Riti • bija` (dim) | `■ Riti` chip, then the backend tag in `faint()` — see WP-5.2 |
| Studio, role fields `model`, `fallback 1–3` | `opus55 · claude-opus-5-5 · via Claude Code` | `opus55 · claude-opus-5-5 · ` + chip |
| Studio, roles list (left) | `fable51·high` (faint) | mark, then `fable51·high` (faint) — no space between mark and alias |
| Studio, flow preview (right, wide only) | ` fable51·high` | ` ` + mark + `fable51·high` |
| stage worker card, line 1 | `opus48·high` | mark + space + `opus48·high` |
| stage agent card title | `  opus55·medium ` | `  ` + mark + ` opus55·medium ` (dropped first when the title does not fit, as today) |
| zoom / Solo header (`ui::model_chip`) | ` · via Claude Code` | ` · ` + chip (always shown when the registry has any provider entry, as today) |
| `/models`, models grid, `provider` column | plain | the cell text in the provider's colour |
| `/models`, providers grid, `id` column | plain | mark + space + id, in the provider's colour |
| stage welcome role table | `{alias} · ` | mark + space + `{alias} · ` |
| discover overlay rows | provider name plain | provider name in its colour |

Tests: `assign_provider_colors` gives distinct colours to eight providers, keeps a colour that
is already set, repairs an invalid one, and returns `false` the second time; `provider_chip`
renders `#` in ASCII mode; the model-picker 80-column test (`the_model_picker_fits_80_columns`)
still passes.

---

## WP-5.2 One model picker, for agents, roles and Studio fields

### State

`app.rs`:

```rust
#[derive(Clone, Debug, PartialEq)]
pub enum PickTarget {
    /// `/model` with no agent in view: the default model of new Solo sessions.
    NewSessions,
    /// A run or Solo agent (live or not).
    Agent(AgentId),
    /// A role of the active run with no agent to hang it on (an empty slot, a task not started).
    RunRole(String),
    /// A field of the pattern open in the Studio: `field` is "model", "fallback", "fallback2" or "fallback3".
    StudioField { role: String, field: String },
}

Overlay::ModelPicker { sel: usize, target: PickTarget, role_scope: bool, filter: String }
```

Replace `target: Option<AgentId>` everywhere (`None` → `NewSessions`, `Some(a)` → `Agent(a)`).

### Rows

`fn picker_rows(app, target, filter) -> Vec<PickRow>` where
`enum PickRow { None, Model(usize /* index into registry.models */) }`:

- `StudioField` with a fallback field: first row `PickRow::None`; then every model except the
  role's own `model` and the aliases used by the *other* fallback fields of that role.
- Otherwise: every model.
- `filter` (lower-cased) keeps models whose alias, model id or provider name contains it.
  `PickRow::None` stays while the filter is empty.

`sel` indexes this list; clamp it after every filter change. When the picker opens, `sel` is
the row of the target's current value (agent's alias, role's model, field's value) or 0.

### Layout

Box: `centered(area, 132, rows + 8)`, title `model & effort`, border `SIGNAL_RED`
(unchanged). Inside, top to bottom:

```
  for: qa · role qa                                   apply to:  [ this agent ]   role qa (all its agents)
  filter: gl▏
   alias            model                     provider                effort        context
 ▶ glm-5.3          z/glm-5.3                 ■ Riti        • bija    —             200k ctx    vision
   glm-5.3-flash    z/glm-5.3-flash           ■ Riti        • bija    —             128k ctx
   gpt-6.1-sol      gpt-6.1-sol               ■ OpenAI (Codex) ◌ codex medium ▰▰▱▱▱  400k ctx   ↪ fallback 1
   opus55           claude-opus-5-5           ■ Claude Code ✧ claude  high   ▰▰▰▱▱  1000k ctx   ● current

  ↑↓ model · type to filter · tab scope · ←→ effort · +/- ctx · ⏎ apply · ctrl+e edit models · esc
```

Line by line:

1. **Target line.** Left: `  for: {label}` in `dim()`. Labels:

   | target | label |
   |---|---|
   | `NewSessions` | `new Solo sessions` |
   | `Agent(a)`, process alive | `{agent name} · role {role}` (Solo: just the name) |
   | `Agent(a)`, not alive (status `Stopped`/`Crashed`, or a worker whose attempt is `Done`/`Failed`/`Cancelled`) | `{agent name} (not running) — its next start` |
   | `RunRole(r)` | `role {r} — this run` |
   | `StudioField` | `{role} · {model \| fallback 1 \| fallback 2 \| fallback 3} — pattern {name}` |

   Right-aligned on the same line, only for `Agent(a)` where `a` is part of the active run:
   the scope switch `apply to:  [ this agent ]   role {r} (all its agents)`. The chosen side is
   wrapped in `[ ]` and drawn `bold(accent())`; the other side `dim()`; `apply to:` `faint()`.
   `role_scope == false` selects the left side. For `RunRole` draw, right-aligned and `faint()`,
   `applies to every agent of the role from now on`. If the line is too narrow for both halves,
   drop the label's `· role …` part first, then abbreviate the right half to `[ agent ]  role`.
2. **Filter line.** Only when `filter` is non-empty: `  filter: {filter}` + a `▏` cursor mark,
   `accent()`. (When empty, this line is blank; do not shift rows as the user types the first
   letter — always reserve it.)
3. **Column heads** in `bold(muted())`: `alias`, `model`, `provider`, `effort`, `context`.
4. **Rows.** Columns and widths:

   | column | width | content | style |
   |---|---|---|---|
   | cursor | 3 | ` ▶ ` on the selected row, else spaces | row style |
   | alias | `aw` = longest alias among the rows, min 8, max 24, +2 | alias, cut at the end with `…` | selected `bold(accent())`, else `text()` |
   | model | `mw` = longest model id, min 12, max 36, +2 | model id; when cut, **keep the end**: `…opus-5-5` (new helper `trunc_start`) | `muted()` |
   | provider | 16 | `provider_chip(reg, id, 14)` padded | provider colour |
   | backend | 9 | `✧ claude`, `• bija`, `◌ codex`, `✦ agy` (existing `backend_glyph`) | `faint()` |
   | effort | 7 | effort, or `—` | `effort_color` |
   | bar | 8 | `theme::effort_bar` | `effort_color` |
   | context | 11 | `{n}k ctx`, `default ctx`, or `agy context` | `dim()` |
   | marks | rest | see below | — |

   Marks, in this priority, each only if it fits whole: `● current` (`GREEN`) — the target's
   present value; `◆ role` (`faint()`) — the role's model when the target agent runs on a pin;
   `↪ fallback N` (`faint()`) — the alias is in the role's chain; `vision` and the model's note
   (`faint()`).

   When the inner width is under 110: drop the bar; under 98: drop the backend column and
   shrink `mw` to 14; under 84: shrink `aw` to 12. The alias column is the last to shrink.

   The `PickRow::None` row reads ` ▶ none            — no fallback at this position` (`dim()`
   after the word).

   More rows than fit: a window that keeps the selection centred (as today) and
   ` · {a}-{b} of {n}` after the target label.
5. **Blank line, then keys** (`faint()`). Full form above. Under 98 columns:
   `  ↑↓ · type filters · tab scope · ←→ effort · ⏎ apply · esc`. Leave out `tab scope` when
   there is no scope switch, `←→ effort` for `StudioField`/`RunRole`, `+/- ctx` under 98.

### Keys

| key | action |
|---|---|
| `↑` `↓` | move; `PageUp`/`PageDown` move by 10 |
| printable character other than `+` `-` | append to `filter`, `sel = 0` |
| `Backspace` | remove the last filter character |
| `Esc` | filter non-empty → clear it; else close |
| `Tab` | toggle `role_scope` (only `Agent` in a run) |
| `←` `→` | effort of the target agent (as today; only `Agent`/`NewSessions`) |
| `+` `-` | step the selected model's context window (as today) |
| `ctrl+e` | open `/models` at this model (was `e`) |
| `Enter` | apply (below) and close |

(`e` and `c` no longer act: they type into the filter. Update the help overlay text that
mentions them.)

### Apply

- `NewSessions`: as today (`settings.default_model`).
- `Agent(a)`, **alive**, agent scope: as today — `app.set_model(a, alias)` then
  `app.model_switched_for_run_agent(a)`.
- `Agent(a)`, **not alive**: never call `set_model` (today it relaunches a process nobody
  prompts). Instead add `Run::pin_next_model(&mut self, a: AgentId, alias: &str)`:
  a worker → `workers[wi].model = Some(alias)` (the newest attempt of that task); a fixed slot →
  `slot_models.insert(slot, alias)`; journal
  `⚙ {name}: its next start uses {alias} (by you)`; toast the same without the glyph. For an
  agent that is in no run slot any more: toast `that agent is finished — pick the role instead (tab)`.
- `Agent(a)` with `role_scope`, or `RunRole(role)`: add
  `App::set_run_role_model(&mut self, role: &str, alias: &str) -> Result<(), String>`:
  1. refuse an unknown alias / `registry.alias_problem(alias)` (return the text).
  2. `with_run`: `run.set_role_model(role, alias)` — new engine method: set
     `pattern.roles[role].model`, re-clamp a non-`auto` effort to what the model offers, drop
     the pins of that role (same code as `adopt_pattern` step 3 — share it), clear
     `fallback_used` for its slots, journal
     `⚙ role {role}: model → {alias} for this run (by you) — the saved pattern is unchanged`,
     `save_state()` (which rewrites the run copy).
  3. `self.registry_changed()`; `self.switch_live_agents_of_role(role, alias)` (WP-3.2).
  4. toast `role {role} → {alias} (this run)`; when the run is halted append
     ` — still halted: space resumes`.
- `StudioField { role, field }`: write the alias (or `None` for the none row) into
  `app.studio.pattern.roles[role]` — `model` through the existing `set_choice`, fallbacks
  through `Role::set_fallback(i, …)` — then `save_pattern(app)` exactly as a committed edit
  does.

Also journal the actor for the existing per-agent path: `Run::set_agent_model` gains a
parameter `by: &str` (`"by you"`, `"fallback"`, `"pattern"`), appended in parentheses to its
journal line; callers: the picker and web `set_model` pass `by you`, `try_fallback` passes
`fallback`.

### Tests

Picker rows: fallback target excludes the role's model and the other fallbacks and starts with
the none row; the filter matches a provider name; a 24-character alias is shown whole at 132
columns; `trunc_start("anthropic-creds/claude-opus-5-5", 14)` ends with `opus-5-5`.
Apply: on a `Done` worker no hub command is sent and `workers[..].model` is set; role scope
changes the run copy, leaves the saved pattern file untouched and switches a live agent;
`RunRole("qa")` with no gate agent sets `pattern.roles["qa"].model`.

---

## WP-5.3 Studio: fallback rows, the context row, and the picker

### Role form

`ROLE_FIELDS` becomes:

```rust
["kind", "glyph", "color", "model", "effort", "fallback", "fallback2", "fallback3", "context",
 "sandbox", "permission", "swarm", "parallel", "max_tokens", "mcp", "description", "instructions"]
```

`fields(app, role)` hides `fallback2` while `fallback` is unset and `fallback3` while
`fallback2` is unset (so an empty chain shows one row, not three).

Labels: the form prints the field key; add `fn field_label(f: &str) -> &str` mapping
`fallback → "fallback 1"`, `fallback2 → "fallback 2"`, `fallback3 → "fallback 3"`, everything
else to itself, and use it where the name column is drawn and where its width `nw` is
computed.

How the rows read (role `worker-big`, focus on `fallback 1`):

```
 ◆ worker-big  the heavy lifter

  kind                worker
  glyph               ◆
  color               teal
  model               opus48 · claude-opus-4-8 · ■ Claude Code
  effort              auto
  fallback 1          gpt-6-sol · gpt-6-sol · ■ OpenAI (Codex)                 ◂ ▸ · ⏎ list
   Tried in order when this role's model cannot go on: a usage limit, no output twice,
   empty turns twice, or a request its provider rejects. none = the run halts instead.
  fallback 2          glm-5.3 · z/glm-5.3 · ■ Riti
  fallback 3          none
  context             250k
  sandbox             workspace-write
  …
```

- Value of `model` and of a set fallback: `{alias} · {model id} · ` + `provider_chip`. The
  value is several spans now: build `Vec<Span>` for these four fields instead of one string
  (the generic path keeps using `get_value`). An alias that is not in the registry renders
  `{alias} · not in /models` with the second part in `RED`.
- Unset fallback: `none` in `dim()`.
- The mark after the selected value, for `model` and the three fallbacks: ` ◂ ▸ · ⏎ list`
  (`faint()`); other choice fields keep ` ◂ ▸`.
- `context`: `none` (`dim()`) or `{n}k` / `{n}M` (use the existing short token format of the
  models screen: whole thousands as `250k`).

### Keys on `model` / `fallback*`

- `←` `→`: cycle as today. For fallbacks the cycle is: none → every alias that is neither the
  role's model nor used by another fallback of this role → none.
- `Enter` or `space`: open the picker
  `Overlay::ModelPicker { target: PickTarget::StudioField { role, field }, sel: <current>, role_scope: false, filter: "" }`.
- typing a letter: unchanged behaviour of choice fields (jump to the next alias starting with
  it).
- `Backspace`/`Delete` on a fallback: clear it (the chain closes up: `normalize_fallbacks`).

### Hints (`field_hint`)

| field | text |
|---|---|
| `fallback`, `fallback2`, `fallback3` | `Tried in order when this role's model cannot go on: a usage limit, no output twice, empty turns twice, or a request its provider rejects. none = the run halts instead.` |
| `context` | `Ceiling on the context of this role's agents. The smallest of this, the pattern's context_cap and the running model's own window wins — also on a fallback model. none = no role limit.` |
| `context_cap` (settings) | `Run-wide context ceiling. Each agent gets the smallest of this, its role's context and its model's own window. 0 = no run-wide ceiling.` |

Under the `context` hint, while that row is focused, one more `faint()` line computed live:

```
   now: opus48 250k · gpt-6-sol 250k · glm-5.3 128k
```

= for the role's model and each fallback: `min(registry.effective_context_window(alias), role.context, settings.context_cap)`
(ignoring unset ones), formatted short. Show the same line under `context_cap` for the role
list's first role only if it fits; otherwise omit it there.

### Roles list and flow preview

Left list row: ` {glyph} {name:<13}` then `provider_mark` + `{model}·{effort}` (`faint()`), cut
to the column. Flow preview rows likewise.

### When a run follows this pattern

If `app.run` is active and `run.pattern.name == app.studio.pattern.name`, add to the header
crumbs, after the saved/not-saved mark:

```
  ◈ the run "{run title, max 24}" follows this pattern — changes reach it when you leave (esc) or save (ctrl+s)
```

in `fg(TEAL)`; under 110 columns: `  ◈ a run follows this pattern — applied on esc / ctrl+s`.
After `sync_run_pattern` (WP-3.2) the toast is `run updated: {n} change(s) from the pattern`
or, when nothing differed, no toast.

The architect box title says "it edits the pattern live": change `live` to `as you watch`
(it never meant the run).

Tests: `fields()` shows `fallback2` only after `fallback` is set; the fallback cycle skips the
model and the other fallbacks; `Enter` on `fallback` pushes a `ModelPicker` with the
`StudioField` target; choosing a row writes and saves; the 13-size render test still passes.

---

## WP-5.4 Stage: every card can be selected; keys say what they will do

### Nodes

`app.rs`:

```rust
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum StageNode {
    Agent(AgentId),
    /// A fixed slot with no agent right now: "planner" | "manager" | "orchestrator" | "gate" | "finale".
    Slot(&'static str),
    /// A task of the current phase whose newest attempt has no agent yet (not started, queued).
    Task(String),
}
```

`App::stage_nodes()` returns `Vec<StageNode>`, in the same card order as today:

1. planner: `Agent(p)` or — when the run is active and past `Setup` — `Slot("planner")`.
2. (planner-loop patterns: the swarm children, as today, all `Agent`.)
3. manager, only if the pattern has one: `Agent(m)` or `Slot("manager")`.
4. orchestrator, only in `Stage::Phase` of a non-loop pattern: `Agent(o)` or `Slot("orchestrator")`.
5. tasks (current phase in plan order, then others in start order): `Agent(newest attempt's agent)` or `Task(id)`.
6. gate, only in `Stage::Phase` of a pattern with a gate: `Agent(g)` or `Slot("gate")`.
7. finale: `Agent(f)`; in `Stage::Finale` with no agent `Slot("finale")`.

`Cv.nodes` becomes `Vec<(StageNode, u16, u16)>`; `Cv::node(&mut self, n: StageNode, r: Rect)`.
In `ui/stage.rs` every place that draws a card registers a node — including the cards that
today register none: `worker_card` with no agent → `StageNode::Task(t.id.clone())`;
`agent_card` with `a == None` → the caller passes the slot; the dashed `gate_card` →
`Slot("gate")`; the compact list fallback likewise. "Selected" is decided by comparing the
card's node with `nodes.get(app.sel)`; replace the `selected: bool` arguments' call sites
accordingly.

`App::focus_agent()` on `Screen::Stage`:
`Agent(a)` → `Some(a)`; `Slot("gate")` → `run.last_gate_agent` (WP-2.8); `Slot("orchestrator")`
→ a new `run.last_orchestrator: Option<AgentId>` (set where the orchestrator is taken/stopped);
other slots and `Task` → `None`.

Add `App::node_role(&self, n: &StageNode) -> Option<String>` (the pattern role behind a node:
for agents `run.role_name_of`, for slots the flow's role, for tasks the task's role) and
`App::node_name(&self, n) -> String` (`planner`, `gate · qa`, the task id, …).

Everything that used `stage_nodes()` as agent ids must be adapted: `land_on_overview`,
`follow_respawned_task`, `select_halted_agent`, `run_agent_choices` (agents only), the web
snapshot if it uses it, and the tests `arrows_follow_visible_card_geometry_and_skip_hidden_agents`,
`stage_nodes_count_one_node_per_task_the_attempt_its_card_draws`,
`the_selection_follows_a_respawned_task_to_its_new_attempt`.

### What a selected card without an agent looks like

Same heavy `SIGNAL_RED` border as any selected card, drawn over the dashed/faint one. Line 2
of an empty slot or task card (the activity line) reads, in `faint()`:

| node | line |
|---|---|
| `Task`, not started | the task title (as today) |
| `Task`, queued | `waiting for a free slot` (as today) |
| `Slot("gate")` | as today (`gate opens when all tasks are done (d/t)` …) |
| `Slot("orchestrator")` | `starts with the phase` |
| `Slot("planner")` | `woken when it is needed` |
| `Slot("manager")` | `woken when something needs it` |

The peek strip under the canvas (`draw_peek`), for a node without an agent, shows one line:
`{glyph} {name} · role {role} · {model}` + provider chip + ` · not running — m sets the role's model for this run`.

### Keys (canvas focus)

| key | `Agent`, alive | `Agent`, not alive | `Slot` | `Task` |
|---|---|---|---|---|
| `⏎` | zoom | zoom (its log is read-only) | zoom into the slot's last agent if there is one; else toast `{name} has not started yet` | toast `{id} has not started yet — p shows it in the plan` |
| `m` | picker `Agent(a)` | picker `Agent(a)` (→ "its next start") | picker `RunRole(role)` | picker `RunRole(role)` |
| `r` | respawn — **press twice** | respawn — press twice | `gate`/`finale` while the stage waits on it: respawn it (press twice); else toast `{name} is not running — nothing to restart` | toast `the orchestrator starts {id}; r works once it has run` |
| `x` | interrupt — press twice | toast `{name} is not running` | same toast | same toast |
| `c` | compact — press twice | toast `{name} is not running` | same toast | same toast |
| `+` `-` | effort | toast `{name} is not running — its role's effort is set in the Studio` | same | same |
| `1`–`9` | select the nth node and zoom if it has an agent | | | |

**Press twice.** `r`, `x` and `c` act on a selected agent at once today; a stray key (two
people on one screen, or text typed while the canvas has the focus) respawns, stops or
compacts an agent. Add `app.armed: Option<(char, StageNode, Instant)>`. First press:
store it and toast (level `Warn`, shown 2.5 s):

- `r` → `press r again to respawn {name} (it starts over with a fresh session)`
- `x` → `press x again to interrupt {name}`
- `c` → `press c again to compact {name}'s context`

A second press of the same key on the same node within 2.5 s performs the action; any other
key, a selection change or the timeout disarms. On a crashed agent (`Status::Crashed`) `r`
restarts at the first press (nothing to lose). The footer keeps `r retry` etc.; the halt
band's `r respawn planner` hint stays (the first press explains itself).

When the action is performed, journal it with the actor:
`↻ {name} respawned (by you)` (via `respawn_why` → `None` case), `■ {name} interrupted (by you)`,
`⇣ {name}: context compaction requested (by you)`.

Tests: `stage_nodes` contains `Slot("gate")` while building and `Task(id)` for an unspawned
task; arrows reach the gate card; `m` on `Slot("gate")` opens a `RunRole` picker; `r` once
does nothing but arm, twice respawns; `x` then `r` does not respawn; `⏎` on `Slot("gate")`
after a QA round zooms into the retired QA agent.

---

## WP-5.5 Stage: a gate fix is drawn, not just written

Needs from the engine (add in this WP if Milestone 6 is not done yet; Milestone 6 fills
`owner`):

```rust
/// What the canvas needs to draw the fix of a gate round.
pub struct FixView { pub round: u32, pub owner: Option<String> /* task id when a task's own worker fixes */, pub agent: Option<AgentId>, pub returning: bool }
impl Run { pub fn fix_view(&self) -> Option<FixView> }
```

- `Some` while the stage is `Fix { round }`: `owner` from `Run.fix_owner` (Milestone 6; `None`
  until then), `agent` = the fixing worker's agent, `returning = false`.
- `Some` with `returning = true` for the checks run that follows a fix (stage `Checks` and the
  current checks run was launched with `after_fix`): keep `round`, `owner`, `agent` from the
  fix that just ended (store them in a field when `on_fix_done` runs; clear it when the next
  gate round spawns or the phase leaves the gate).
- Otherwise `None`.

Also `pub fn gate_rounds(&self) -> (u32, u32)` = (current round or 0, `gate_max_rounds`).

All drawing is in `ui/stage.rs::phase` after the gate card rect `gr` is known, and in
`gate_card`.

### A. The generic fix worker (`owner == None`)

A fix worker card next to the gate, joined by an edge.

Placement of the card rect `fr` (width `fw = 30`, height 4, or 5 when `card_h == 5`):
1. **Right of the gate** if `gr.x + gr.width + 9 + fw <= area.x + area.width`:
   `fr.x = gr.x + gr.width + 9`, `fr.y = gr.y`.
2. Else **below the gate** if `gr.y + gr.height + 1 + fr.height <= area.y + area.height`:
   `fr.x = cx - fw / 2`, `fr.y = gr.y + gr.height + 2`.
3. Else no card (the gate card's text is all there is — today's behaviour).

Draw it with `worker_card` (the fix worker is in `run.workers` as `fix-{round}`), so it shows
model, activity, time and tokens like any worker, registers a node and can be selected and
zoomed.

```
     ╭ ◎ gate · qa ───────────────────────────────╮         ╭ ◆ fix-1 ────────────────⠋─╮
     │ round 1/3 · ✗ 4 findings                    │ ━━▸━━▸━ │ ■ gpt-5.6-luna·low        │
     │ ⚒ fix-1 repairs what QA found               │         │ editing tools/audit.sh    │
     ╰─────────────────────────────────────────────╯         │ 1m04s · 212k              │
                                                             ╰───────────────────────────╯
```

The edge (case 1) runs on row `gr.y + 1` from `gr.x + gr.width + 1` to `fr.x - 2`:

| state | glyphs | colour |
|---|---|---|
| the fix worker was prompted in the last 2.5 s (`prompting(run, agent)`) | `━` with `▸` every 4th cell, marching right (`anim::march(80)`) | `ROSE`, bold |
| working | `━` with `▸` every 4th cell, static | `AMBER` |
| `returning` (checks after the fix) | `━` with `◂` every 4th cell, marching left | `GREEN` |
| fix worker failed / blocked | `┄` | `RED` |

Case 2 (card below): the edge is vertical at `cx`, rows `gr.y + gr.height .. fr.y - 1`, with
`▾` marching down / static, and `▴` marching up when returning; same colours.
Without motion (`!theme::motion()`): no marching, arrows at fixed cells.

### B. The task's own worker fixes (`owner == Some(task)`) — used from Milestone 6 on

No extra card. The owner's existing worker card changes, and an edge runs from the gate back
up to it.

```
     ╭ ◆ p2-api ──────── ⚒ r1 ─⠋─╮  ╭ ◆ p2-ui ───────────────✓─╮
     │ ■ opus48·high              │  │ ■ opus48·high            │
     │ fixing what QA found       │  │ Added the settings page  │
     │ 3m10s · 1.2M  ↻ fix        │  │ 6m02s · 2.1M             │
     ╰────────────────────────────╯  ╰──────────────────────────╯
            ▴                                 │
            ▴━━━━━━━━━━━━━━━━┳────────────────┴
                             ▴
     ╭ ◎ gate · qa ───────────────────────────────╮
     │ round 1/3 · ✗ 4 findings                    │
     │ ⚒ back to p2-api — it repairs its own work  │
     ╰─────────────────────────────────────────────╯
```

- Owner card title: before the state mark at the right edge, the badge `⚒ r{round}` in
  `ROSE` bold (3–5 cells; drop it if the title would not fit the task id's first 6 characters).
- Owner card activity line: the agent's live activity as for any running worker; when it has
  none yet, `fixing what QA found` (`AMBER`).
- The edge replaces the normal join lines on its path: from the gate card's top centre
  (`cx`, `yb + 2`) up to the join bus row (`yb + 1`), along the bus to the owner's column `c`,
  then up the owner's drop (`c`, `yb`). Glyphs: vertical cells `▴` every 2nd cell else `┃`;
  horizontal cells `━` with `◂` or `▸` (pointing towards the owner) every 4th cell; the corner
  cells keep the join glyphs (`┳`, `┻`, `┛`, `┗` — or `+` in ASCII). Colour and motion as in
  table A (prompted → `ROSE` marching towards the owner; working → `AMBER` static; returning →
  `GREEN`, arrows reversed, marching towards the gate).
- When the owner's card is in an upper row of a multi-row phase, draw only the part of the
  edge below the last row (gate → bus → the bus end nearest the trunk) and mark the owner card
  with the badge; do not route through other cards.

### C. The gate card's own lines (both cases)

Line 1 (replaces `round {n} · `): `round {r}/{max} · ` in the role colour, then:

| situation | text | colour |
|---|---|---|
| QA agent working | its activity (as today) | as today |
| last report pass, verifying | `✓ QA passed — verifying checks` | `GREEN` |
| last report fail | `✗ {k} finding(s)` — `k` = number of lines in the report that start with a digit+`.`, `-`, `*` or `F`+digit; if 0, just `✗ failed` | `RED` |
| plan defect | `✗ a check is wrong — the planner is asked` | `AMBER` |

Line 2:

| situation | text (`AMBER`, spinner in front when not halted) |
|---|---|
| `Fix`, generic, round 0 | `⚒ fix-0 merges the conflicting branches` |
| `Fix`, generic | `⚒ fix-{r} repairs what QA found` |
| `Fix`, owner | `⚒ back to {task} — it repairs its own work` |
| returning | `checks after the fix…` (`BLUE`) |
| halted in any of these | the same text without spinner, prefixed `paused · ` |

The existing check marks (`✓ cmd  ✗ cmd`) stay on line 2 for the plain `Checks`/`Gate` steps.

The rail line under the header (`draw_rail`) for `PhaseStep::Fix` becomes
`QA gate · round {r}/{max} · fix by {fix-{r} | task id}`.

Tests (render into a `TestBackend` buffer as the existing stage tests do): in `Fix{1}` with a
generic fix worker and a 140-column area the buffer contains `fix-1` inside a box right of the
gate and at least one `▸` on the gate's second row; at 70 columns the card is below; at 60×20
there is no card and no panic. With `owner = Some("p2-api")` the owner's title row contains
`⚒ r1` and the gate card `back to p2-api`. All 13 sizes of `scripts/stress.sh` still render.

---

## WP-5.6 Halt band, composer, counters and wording

**(a) The band says the run is stopped.** `draw_rail`, second line. Today:
`   ↳ r respawn planner · space retries the turn`. New: `   ↳ the run is stopped — {keys}`,
and when a model was changed during this halt (new `Run.halt_touched: bool`, set by
`set_agent_model`, `pin_next_model`, `set_role_model` and `adopt_pattern` while halted,
cleared by `resume_core`): `   ↳ the run is still stopped — model changed ✓ · space resumes`.
Engine hints (`Run::halt_hint`) change wording, not keys:

| reason | hint |
|---|---|
| `User` | `space resumes` |
| `Auth`, `UsageLimit` | `fix the credentials or quota, or m picks another model — then space resumes` |
| `ProviderRejected` | unchanged |
| `Environment` | unchanged |
| `GateExhausted` | `the planner has been asked · type feedback for it · space retries the gate` |
| `AttemptsExhausted` | `r gives {who} one more attempt · type feedback for the planner` |
| `AgentTurnFailed` | `r respawns {who} · space retries its turn` |

**(b) No lost line breaks.** The band prints `h.message` raw, so a multi-line message runs
together (`<<'EOS'import json`, `blockedSUMMARY:`). In `draw_rail` and in the alert/toast that
show a halt message, replace every `\n` (and `\r\n`) by ` ⏎ ` (`theme::g("⏎", "|")`) before
truncating. The `e` details overlay keeps real line breaks.

**(c) One way a typed message reaches the planner.** Today a message typed on the overview is
sent at once as `[mantra:reprompt] …` (journal `re-prompt → planner`), while the same message
typed in a zoomed view is queued as `[from the user] …` (journal `you → planner`). The footer
promises `⏎ queue · ctrl+f send now` in both. Make both views do what the footer says:

- `Run::user_input` gains a parameter `mode: Send`. In its branches that prompt the planner
  (`Stage::Planning`, `Stage::Phase | Finale`, and the question/pattern answers) use
  `ctx.prompt_mode(p, text, mode)` instead of `ctx.prompt`. Journal line for the
  `Phase | Finale` branch becomes `you → planner: {text}` (same as `direct`).
- `App::submit` / `App::force_send` (`app.rs` ≈3525 / ≈3577): pass `Send::Queue` for `⏎` and
  `Send::Force` for `ctrl+f` on the stage, as they already do for the zoomed view.
- The `[mantra:reprompt]` wrapper (status + orchestrator log) stays for the overview path —
  it is useful context — but it is built when the message is *delivered* only if that is cheap;
  otherwise build it at submit time as today.

With WP-1.6's journal lines the pulse then shows `you → planner: …` when typed and
`delivered to planner: …` when it reaches the agent.

**(d) Honest task counts.** Add `Run::phase_progress(&self) -> (usize, usize)`: tasks of the
current phase whose newest attempt is `Done` / tasks in the current phase. Use it in
`draw_rail` (`building · {d}/{t} tasks done`), in `gate_card`
(`gate opens when all tasks are done ({d}/{t})`) and in the orchestrator card's
`watching {n} worker(s)` (count workers of the current phase's tasks that are `Running`).
(Today: `2/1 tasks done`.)

**(e) `mantra_status` shows each task once.** `Run::status_text` lists every entry of
`self.workers`, so a task with a cancelled first attempt appears twice with different states
and no attempt number. List only the newest attempt per task id, append ` (attempt {n})` when
`n > 1`, print `steps {d}/{t}` only when the agent has a plan (drop the constant `steps -`),
and for a worker that is `Done`/`Failed`/`Cancelled` print the state alone without
`activity: …`. Apply the same to `status_fingerprint`.

**(f) Small render faults.**
- Peek strip / detail line: an empty effort renders `claude-opus-5-5· · idle`. Where the
  detail line joins `model·effort` (`draw_peek` and `ui::model_chip`), omit the separator when
  the effort is empty.
- `mantra doctor` prints `key stored in models.toml` for a provider key; keys live in
  `secrets.toml`. Change the wording to `key stored (secrets.toml)` (`main.rs`, search
  `stored in models.toml`; the `/models` provider row says the same — change both).
- Pulse panel: a line that is exactly as wide as the panel leaves a stray `…` on the next row
  (`☰revision1:1`, `▶ run resumed    …`). In `draw_pulse` build each line with
  `trunc(text, inner_width - prefix_width)` and render with `Paragraph` **without** wrap; do
  not pad with a trailing ellipsis.
- The resume-instant watchdog line is fixed in WP-7.1.

Tests: band text for a halted run starts with `↳ the run is stopped`; a halt message with
`\n` shows ` ⏎ `; `phase_progress` with two attempts of one task counts one; `status_text`
lists a twice-attempted task once with `(attempt 2)`; overview `⏎` while the planner is busy
adds a queue chip and sends no steer.

---

## WP-5.7 Web UI parity

The web UI renders the same App state; it must not fall behind the terminal.

Server side:

- `web/snapshot.rs`: every model entry in the app info gains `provider_color` (the colour
  name); every provider entry gains `color`. Every role in the pattern/run payloads gains
  `fallbacks: [alias, …]` (the chain) **in addition to** the existing `fallback` (first of the
  chain, kept for older bundles) and `context` (number or null).
- `web/protocol.rs` + `web/commands.rs`: `SetRoleFallback { role, alias }` gains an optional
  `index: Option<u8>` (0–2, default 0) and calls `Role::set_fallback(index, alias)`.
  `SetRoleModel` gains optional `scope: Option<String>` — `"run"` applies
  `App::set_run_role_model` only (this run, saved pattern untouched); absent/`"pattern"` keeps
  today's behaviour (save the pattern, then `sync_run_pattern`).
  New command `set_role_context { role, tokens: Option<u64> }` (saves the pattern like
  `SetRoleFallback`, then `sync_run_pattern`). Add the three to the protocol round-trip test.
- `pattern_get` / `pattern_save` JSON carries `fallback2`, `fallback3`, `context` (they are
  serde fields of `Role`, so this is automatic — add a test).
- The run payload gains `fix: { round, owner, agent, returning } | null` from `Run::fix_view`
  and `gate_round: [r, max]`.

Assets (`src/web/assets`):

- `app.css`: `.prov { display:inline-flex; align-items:center; gap:.35em; } .prov::before { content:""; width:.62em; height:.62em; border-radius:2px; background: currentColor; }`
  and `.prov-cyan { color: var(--cyan); }` … one class per colour in `COLORS`.
- `app.js` `modelPickList`: the sub line starts with
  `h('span', { class: 'prov prov-' + m.provider_color }, m.provider_name)` instead of the bare
  provider name. Add a text filter input above the list when there are more than 12 models
  (filter on alias, model id, provider name).
- Role sheet (`app.js` ≈549): the single "Fallback model" entry becomes up to three entries
  `Fallback 1`, `Fallback 2`, `Fallback 3` (2 shown when 1 is set, 3 when 2 is set), each
  opening the `role-fallback` sheet with its `index`; each shows `alias` + provider chip or
  `none`. Add an entry `Context limit` opening a small sheet with the choices
  `none, 64k, 128k, 200k, 272k, 400k, 500k, 1M`.
  In the role-model sheet add a two-way toggle at the top: `This run only` / `Save to the
  pattern` (default: `This run only` when a run is active), sent as `scope`.
- `ui/config.js` (pattern editor, ≈813): the `Fallback` native `<select>` of bare aliases
  becomes three pick buttons (`Fallback 1…3`, same progressive rule) that open the model sheet
  and write into the draft (`role.fallback`, `role.fallback2`, `role.fallback3`), and the
  role's `Model` control becomes the same pick button; each button shows
  `alias · model id` and the provider chip. Add a `Context limit` select with the choices
  above writing `role.context`.
- `ui/screens.js` (≈421): `↪ falls back to X` becomes `↪ falls back to X → Y → Z` for the
  chain.
- Agent list: an agent row for a gate fix shows a `⚒ fix r{n}` mini-chip
  (`.mini-chip.warn`), and the owner task's row shows it when `fix.owner` names it.
- Halt card text follows WP-5.6(a).

Run `node docs/tools/web-view-regression.mjs` and the `web-e2e.mjs` flows after the change;
update their expectations only where a string in this plan changed.
