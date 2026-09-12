# Mantra v0.2 — implementation plan

This file is the complete, ordered work plan for Mantra **v0.2.0**. It was written after a full read of the
codebase (every pointer below is `file:line` in `mantra_src/src/` as of commit `6cb3dc0`) and after live
verification of the external tools (Codex 0.154.0, Claude Code 2.1.269, the LibertAI API). It is meant to be
executed top to bottom by an implementer who has not seen the codebase before. Read §0 fully before touching code.

---

## 0. Ground rules for the implementer

### 0.1 Non-negotiables

1. **One seam, no parallel paths.** The engine talks to agents only through the `Ctx` trait
   (`engine/run.rs:49-60`) and `prompt_agent` (`app.rs:231-252`). The Hub talks to processes only through
   `Cmd` (`hub.rs:31`) / `HubEvent` (`hub.rs:48`). The Claude backend (§WP10) plugs in *below* the Hub and
   must not add a second way to spawn, prompt or read an agent. If you find yourself special-casing a backend
   in `app.rs` or `engine/`, stop and push the difference down into `hub.rs`.
2. **Selection by identity, never by index into a re-sorted list** (§WP3).
3. **Every failure the run cannot handle by itself becomes a `Halt` with a reason and a resume hint** (§WP6).
   No bare `paused = true`.
4. **No secrets on argv, in logs, in the journal, or in toasts.** Keys go into the child's environment only
   (§WP5). `rpc.rs:128` logs raw stderr lines — keep it that way but never print env.
5. **ANSI escape sequences never reach the UI or the journal** (§WP1.3).
6. Keep `cargo build` warning-free at the end (§WP1.4), keep all 25 existing tests green, add the tests listed
   in each WP, and keep `scripts/stress.sh` passing after every WP that touches the UI.
7. Commit per work package with the WP number in the subject (`WP7: watchdog and escalation`).
8. Bump `Cargo.toml` `version` to `0.2.0` in WP1; every other version string derives from it
   (`main.rs:88,128,460`, `rpc.rs:213`).

### 0.2 How to build and test

```bash
cd mantra_src
cargo build                      # debug: overflow checks ON, this is what stress.sh uses
cargo test                       # bin-only crate: plain `cargo test` (NOT --lib)
./scripts/stress.sh              # renders every screen at 13 sizes through a simulated run; must print no 'panicked'/'timeout'
./target/debug/mantra --demo     # simulated agents, zero cost
./target/debug/mantra --demo --snapshot "wait:1;snap:hello"   # headless: prints a frame
```

Snapshot grammar (`main.rs:351-441`): `wait:<s>`, `until:<text>@<timeout>`, `type:<text>`, `key:<spec>`
(`ctrl+x`, `alt+x`, `shift+x`, `enter`, `esc`, `tab`, `backtab`, `up`, `down`, `left`, `right`, `space`,
`pgup`), `resize:<W>x<H>`, `sweep:<label>` (13 sizes, prints `sweep ok`), `snap:<label>` (prints the frame).
Use `snap` when you want to *look* at a screen; `sweep` only checks for panics.

### 0.3 Live test environment (real models, cheap)

The environment variable `API_KEY` holds a key for **LibertAI** (`https://api.libertai.io/v1`). Verified in
this session:

| endpoint | works | used by |
|---|---|---|
| `GET /v1/models` | yes (17 ids) | Mantra discovery |
| `POST /v1/responses` | yes | Codex (`wire_api = responses`) |
| `POST /v1/messages` (Anthropic format) | yes | Claude Code via `ANTHROPIC_BASE_URL=https://api.libertai.io` |
| `POST /v1/chat/completions` | yes | not needed |

Models and context windows (from the provider's page; `/v1/models` does not report context):

| model id | context | notes |
|---|---|---|
| `qwen3.8-27b` | 262,144 | fast, tools ok — use for workers and quick tests |
| `glm-5.3` | 262,144 | good planner; supports reasoning; `-thinking` variant exists |
| `qwen3.6-35b-a3b` | 262,144 | cheap |
| `qwen3.5-122b-a10b` | 262,144 | **rejects the `developer` role that Codex sends → HTTP 400 "Unexpected message role."** Do not use under Codex. Fine under Claude Code. |
| `deepseek-v4-flash` | 200,000 | ok |
| `glm-5.3-flash` | 524,288 | ok |
| `hermes-3-8b-tee` | 16,000 | too small; good for testing context-full handling |

A ready-made test home was used in this session and should be re-created by `scripts/live-env.sh` (WP13):

```toml
# $MANTRA_HOME/models.toml  (MANTRA_HOME=/tmp/mantrahome)
[[provider]]
id = "libertai"
name = "LibertAI"
base_url = "https://api.libertai.io/v1"
env_key = "LIBERTAI_API_KEY"          # export LIBERTAI_API_KEY="$API_KEY"

[[model]]
alias = "astra"   # planner
provider = "libertai"
model = "glm-5.3"
context_window = 262144
auto_compact_percent = 85
default_effort = "medium"
efforts = []
# sol = qwen3.8-27b (NOT qwen3.5-122b, see above), luna = qwen3.8-27b, terra = deepseek-v4-flash, same shape
```

Codex is installed at `/opt/node22/bin/codex` (0.154.0). It needs `CODEX_HOME` set to a writable dir that is
**not** under `/tmp` if you want its helper binaries (a warning otherwise, harmless). Claude Code is at
`/opt/node22/bin/claude` (2.1.269). **This sandbox runs as root**, so every `claude` invocation needs
`IS_SANDBOX=1`, otherwise `--dangerously-skip-permissions` is refused (§WP10.3).

What was verified end to end in this session (so you know the baseline works):

- `mantra --snapshot` Solo turn against real Codex + LibertAI created `hello.txt`.
- A full Mandala run (`mantra run "Create a tiny Python CLI …"`) planned (glm-5.3), spawned a worker,
  merged, passed both gate checks, and reached the finale — where it halted because the finale role used
  `qwen3.5-122b-a10b` (the developer-role 400 above). Journal at
  `/tmp/mantrahome/runs/mandalaproj-00e98afb/167086-create-a-tiny-python-cli/journal.jsonl` if still present.

### 0.4 Real-run findings to fix (observed in the run above)

| # | observation | root cause / action | WP |
|---|---|---|---|
| F1 | Journal line `planner: process crashed (codex exited: [2m2026-… ` — ANSI garbage in the journal and the log; the shown "reason" was the last stderr line (a bubblewrap warning), not the real cause | `hub.rs` reports `stderr_tail` verbatim; strip ANSI, include exit code, and prefer the RPC error (`thread start failed: …`) over stderr | WP1.3 |
| F2 | Both planner and orchestrator processes "crashed" in the first second of the run and were restarted successfully | Two `codex app-server` starting simultaneously against a fresh `CODEX_HOME`; Mantra recovered. Add the exit code to the message (F1) and stagger simultaneous spawns by 300 ms | WP1.3 |
| F3 | After the worker finished and the gate had started, the orchestrator kept doing `mantra_prompt` → worker `interrupted` → `mantra_prompt` again (3 cycles) | Orchestrator tools are still live during Merging/Checks/Gate and target a worker whose task is Done; `mantra_prompt` to an idle done worker starts a new turn on a thread that is about to be archived. Refuse `mantra_prompt`/`mantra_interrupt`/`mantra_retry` on Done workers once `PhaseStep != Orchestrating` (return a clear tool error "task is done and the phase is in gate; wait for the handoff") | WP7.5 |
| F4 | `finale: Unexpected message role. — run paused (check the error, then press space to resume)` | Provider incompatibility. Resuming cannot help. Halt with a reason that names the role/model and offers `m` (switch model for that role) and `r` (retry). Also make the `/models` `t` test send a `developer` message so incompatible models are caught before a run | WP6, WP4.4 |
| F5 | Stage text `Finale` in the phase rail matched `until:Finale` immediately | Test-harness only: use `finale 1/3` as the until-text | WP13 |

### 0.5 User-reported logs from v0.1 on a real machine (`~/.mantra/logs/mantra.log`)

| # | observation | root cause / action | WP |
|---|---|---|---|
| L1 | `ERROR codex_app_server: Codex's Linux sandbox uses bubblewrap and needs access to create user namespaces.` on every spawn; planner reported "repository inspection is blocked: every exec_command…"; the worker was "blocked"; the QA gate failed 4 rounds in a row with "shell sandbox fails before any command runs (bwrap…)" | The host forbids unprivileged user namespaces, so Codex's sandbox cannot start and **no** agent can run a command. Mantra must (a) detect it in `doctor` and at startup with a cheap probe, (b) halt the run at the first command failure that names `bwrap`/`user namespaces` with `HaltReason::Environment` and a fix hint, and (c) offer the fallback `sandbox = "danger-full-access"` (workers stay isolated by git worktrees) | WP12.4, WP6 |
| L2 | Thousands of `ERROR codex_core::util: OutputTextDelta without active item` lines (and `unsupported call: multi_agent_v1`, `cannot update goal because this thread has no goal`) copied into `mantra.log` | Mantra logs every codex stderr line verbatim (`rpc.rs:128`). Collapse repeats and drop known noise | WP12.5 |
| L3 | `orchestrator: Missing environment variable: API. — run paused` after the plan was approved (the planner ran on another provider) | The key check happens only when Codex tries the request. Preflight every role's provider before a run starts, and halt with a hint naming the provider, the variable and `/models` | WP12.6, WP5 |
| L4 | Gate rounds 1–4 each reported the same blocker; the run then paused with an opaque message | Add loop protection: if two consecutive gate reports share the same blocker signature (first 80 chars after "Blocked:"/"blocked"), halt with `Environment`/`GateExhausted` immediately instead of spending the remaining rounds | WP6, WP12.4 |

---

## 1. Work packages

Order matters: WP1→WP5 are small and unblock the rest; WP6→WP9 are the engine/UX core; WP10 is the Claude
backend; WP11→WP13 close out.

---

### WP1 — Hygiene, version, error text

**1.1 Version.** `Cargo.toml` → `version = "0.2.0"`. Add `CHANGELOG.md` at repo root with a `## v0.2.0`
section; append a line to it in every later WP.

**1.2 Warnings.** Remove the 7 dead-code warnings: `agent.rs:136 flash` (delete the field and its writer or
use it for the card flash), `engine/plan.rs:133 Plan::task` (delete), `engine/run.rs:24 JobTag::Cleanup{phase}`
(use it in the log line), `engine/run.rs:99 Worker.auto_retries` (delete or use in WP7), `ui/mod.rs:198
status_color` (delete), `ui/theme.rs:226 gauge` (delete), `util.rs:37 unix_millis` (delete).

**1.3 Process exit reporting (F1, F2).**
- `util.rs`: add `pub fn strip_ansi(s: &str) -> String` (remove `ESC [ … final-byte` CSI sequences, `ESC ] … BEL/ST`
  OSC sequences, and lone `ESC`). Unit test with the exact string from F1.
- `rpc.rs:138-172` reader task only owns `stdout`; the `Child` handle is returned to the caller
  (`rpc.rs:174`) and lives in `run_process`. So the exit code is captured in **`hub.rs:322-326`** (the
  `Incoming::Closed` arm of `run_process`, which already calls `child.kill()`): first `child.try_wait()`, else
  `tokio::time::timeout(2s, child.wait())`, else kill. `Incoming::Closed { stderr_tail }` keeps its shape; the
  tail is ANSI-stripped and limited to the last 5 lines in `rpc.rs` where `tail` is collected.
- `hub.rs:185-337 run_process`: the crash reason becomes `"codex exited (code {code}): {last non-warning
  stderr line}"` (a line containing `WARNING`/`bubblewrap` is skipped when a later line exists); when the failure
  was an RPC error (`thread start failed: …`, `hub.rs:246-249`) keep that text first and append the stderr
  tail after ` — `.
- `engine/run.rs:214-227 Run::log`: apply `strip_ansi` to every journal line defensively.
- `hub.rs:75-81 Hub::spawn`: keep a `last_spawn: Instant` on `Hub`; if the previous spawn was < 300 ms ago,
  sleep the difference inside `agent_task` before `run_process` (staggers F2 without changing callers).

**1.4 `.gitignore`** — done (`mantra_src/.gitignore` = `target/`).

Acceptance: `cargo build` prints zero warnings; `cargo test` includes `util::tests::strip_ansi`; a crash of a
real codex shows `code N` in the journal.

---

### WP2 — Context safety by default (the "worker hit 200k and failed" bug)

Today `-c model_context_window` and `-c model_auto_compact_token_limit` are only sent when both
`ModelEntry.context_window` and `auto_compact_percent` are `Some` (`app.rs:182-188`), and the four built-in
models (`config.rs:272-312`) plus every Codex-catalog model have both `None`. Codex's own auto-compaction
default is not reliable across providers, so a worker runs until the provider rejects the request.

Changes (`config.rs`, `app.rs`, `discover.rs`, `ui/studio.rs`):

1. `ModelEntry` gets two derived accessors, no new stored fields:
   - `fn effective_context(&self) -> u64` → `context_window.unwrap_or(discover::ASSUMED_CONTEXT)` (200k).
   - `fn effective_compact_percent(&self) -> u8` → `auto_compact_percent.unwrap_or(85)`.
2. `app.rs:182-188`: **always** push `-c model_context_window=<effective_context>` and
   `-c model_auto_compact_token_limit=<effective_context * effective_compact_percent / 100>` for every agent,
   Solo included. Remove the nesting.
3. Built-in defaults (`config.rs:272-312`): set explicit `context_window` for astra/sol/terra/luna
   (272_000 for the gpt-5.x family per the README example) and `auto_compact_percent = Some(85)`, so the
   shipped `models.toml` is honest.
4. `ui/studio.rs:609-610`: the models table shows `200k (assumed) · 85% (default)` in dim text when the
   values are derived, instead of `codex` / the ⚠. Remove the ⚠ path (the coupling it warned about is gone).
5. Context-full handling in `engine/run.rs:951-970` stays; additionally, when `ErrKind::ContextFull` is seen
   for a model whose `context_window` was *assumed*, halve the assumed value for that agent's next attempt and
   log `context window assumed 200k was too large for <model>; using 100k — set it in /models`. Implement as a
   two-hop field: `SpawnReq.context_override: Option<u64>` (`engine/run.rs:37-46`) is copied by `Ctxt::spawn`
   (`app.rs:264-285`) into `AgentOpts.context_override` (`app.rs:158-171`), and `spawn_agent` (`app.rs:182`)
   uses `o.context_override.unwrap_or(m.effective_context())`. `Run` remembers the halved value per task in
   `Worker` so `retry_worker`/`respawn` reuse it.
6. README "Context & compaction": replace the "Without them, Codex's own defaults apply" sentence with the
   new rule (200k assumed, 85%, always sent).

Tests: unit test in `config::tests` for both accessors; extend `agent::tests` for `classify` on
`contextWindowExceeded`; a snapshot run in `--demo` still passes. Live: set `context_window = 16000` on a
`hermes-3-8b-tee` alias and run a Solo prompt that reads a large file — the log must show a `⇣ context
compacted` line instead of a failure.

---

### WP3 — Studio: stable selection, per-role permission mode

**3.1 Selection by key (`ui/studio.rs`, `app.rs:83-93`).**
`StudioState.sel: usize` indexes a list rebuilt and re-sorted on every draw (`studio.rs:17-22`,
`pattern.rs:149-154`), so changing a role's `kind` moves it and the highlight lands on a different role
(`studio.rs:157`). Replace `sel: usize` with `sel: StudioSel` where
`enum StudioSel { Role(String /*name*/), Settings, Flow }`. Provide `fn sel_index(app) -> usize` for drawing
and `fn select_by_index(app, i)` for ↑/↓. Every mutation site that used `sel` numerically
(`studio.rs:230` new-role select, `:310` and `:337` draw, `:484` key dispatch, `:492`/`:500` Up/Down,
`:575` role delete) goes through these two helpers. Renaming a role
updates `StudioSel::Role` to the new name.

**3.2 Per-role `permission` field (`engine/pattern.rs:11-27`, `ui/studio.rs:13`, `app.rs:278`).**
- `Role.permission: String` with values `"never"` (default, = codex approval policy `never`),
  `"on-request"`, `"untrusted"`. Serde default `"never"`. Validate in `pattern.rs` `validate()` next to
  `sandbox`.
- `app.rs:278` `Ctxt::spawn`: `approval: r.role.permission.clone()` instead of the literal `"never"`.
- Studio: add `permission` to `ROLE_FIELDS`, cycle with ←/→ like `sandbox`. Field help text: *"off = the agent
  never asks (default for Mandala). Turn on only for roles you want to approve by hand; requests land in the
  inbox (ctrl+g)."*
- `app.rs:971-977` auto-approve fallback must consult the *agent's* policy, not the global Solo mode: store
  `approval` on `Agent` (set at spawn) and auto-resolve when it is `never`.
- Built-in pattern (`pattern.rs:249-368`): no `permission` lines (all default off), which matches the request
  "off by default in Mandala, can only be enabled manually in the Studio".

Tests: `pattern::tests` — a pattern with `permission = "sometimes"` fails validation with a plain-language
message; built-in pattern validates; Studio snapshot: `type:/studio;key:enter;key:down;key:right` (change kind
of the first role) then `snap` — the highlighted name in the list must equal the name in the fields panel.

---

### WP4 — Model picker and model fields show the provider; inline context override

Rows in the ctrl+k picker already carry `m.provider` (`ui/overlays.rs:84`) but only the provider *id*.

1. `config.rs`: `Registry::provider_name(&self, id) -> String` (display name, `"OpenAI (Codex)"` for the
   built-in `openai`, and for WP10 `"Claude Code"` variants). `Registry::backend_of(&ModelEntry) -> Backend`.
2. Picker row (`overlays.rs:69-94`) becomes: `alias  model-id  via <provider name> [<backend glyph>]  <ctx>
   <effort bar>`. Backend glyphs: `◌ codex`, `✧ claude`. If two aliases map to the same `model` string through
   different providers (the "luna through OpenAI subscription vs luna via OpenRouter" case), both rows show and
   the provider column makes them distinguishable; additionally dim-suffix `(also via X)` on each.
3. Inline context override in the picker: keys `+`/`-` step `context_window` through
   `[16k, 32k, 64k, 128k, 200k, 262k, 272k, 400k, 524k, 1m]` for the highlighted row, `c` opens the
   number-edit overlay (reuse `Overlay::Edit`, `overlays.rs:446-453`) for an exact value; both save via
   `app.registry.save()` and re-spawn nothing (applies from the next spawn; show toast "applies to new
   agents"). Footer hint line lists `+/- ctx · c edit ctx · ←→ effort · ⏎ pick · e /models`.
4. Studio role `model` field (`studio.rs`, the arm that cycles `r.model` with ←/→): render as
   `sol · gpt-5.6-sol · via OpenAI (Codex)`; same for the Solo header `model_chip` (`ui/mod.rs:159`): append
   ` · via <provider name>` when the registry has more than one provider.
5. **4.4 Provider test sends a developer message (F4).** The `/models` `t` test (`studio.rs` → the live test
   path in `app.rs`) currently starts a plain turn. Make the test prompt include developer instructions
   (`developerInstructions` in `thread/start`, `hub.rs:207-243`, already supported) so a provider that rejects
   the `developer` role fails the test with the provider's message shown in the `note / test` column. This
   probe is Codex-specific (`developerInstructions` is a Codex `thread/start` param); the live test is
   dispatched per backend — for a `ClaudeCode` provider (WP10.5) the test spawns the Claude process with
   `--append-system-prompt` and a one-line prompt instead.

Tests: snapshot `key:ctrl+k;snap:picker` at 120×36 shows `via`; `key:+` then reopen — the row shows the new
context and `models.toml` on disk changed.

---

### WP5 — API key directly on the provider (optional, alongside `env_key`)

Today only the env var *name* is stored (`config.rs:227-240`) and the child inherits the whole environment
(`rpc.rs:91-101`, no `.env()`).

1. `ProviderEntry.api_key: Option<String>` with `#[serde(default, skip_serializing_if = "Option::is_none")]`.
2. `ProviderEntry::resolve_key(&self) -> Option<String>`: env var named by `env_key` if set and non-empty,
   else `api_key`. Use it in `discover.rs:38-42` (replacing the direct `std::env::var`) and in `doctor`
   (`main.rs:490-492`) and the `✓ key set` line (`studio.rs:655-656`, show `✓ key set (env)` / `✓ key stored
   in models.toml` / `✗ no key`).
3. Delivery to Codex **without argv**: `rpc::spawn` gains `envs: Vec<(String,String)>` and calls
   `.envs(...)`. `spawn_agent` (`app.rs:177-228`) passes `[(env_key_name, key)]` when the key came from
   `api_key`, so the existing `-c model_providers.<id>.env_key=<NAME>` mechanism keeps working unchanged
   (Codex reads `$NAME` from its own env). If `env_key` is empty, synthesize the name
   `MANTRA_<ID>_API_KEY` for this purpose.
4. `config.rs:386-394 atomic_write`: when the registry contains any `api_key`, `set_permissions(0o600)` on
   Unix before rename.
5. Studio `/models` providers table: new column `api_key` rendered masked (`••••` + last 4 chars) with `⏎` to
   edit (Edit overlay, single line, masked while typing is not required). The `n` (new provider) flow
   (`studio.rs:728-731`) then walks the user through `id → name → base_url → env_key or api_key` using the
   existing per-cell Edit overlay; the placeholder text says *"either name an environment variable or paste a
   key"*.
6. README providers table: add the `api_key` row; DESIGN.md §8/§9: amend the "never written to files"
   invariant to "never written unless you paste it into the api_key field, which is stored 0600".
7. Never log or toast the value; `provider_args()` must not include it (assert in a unit test that the
   generated args contain no `api_key` string).

---

### WP6 — Replace "Pause" with a typed Halt

`Run.paused` (`engine/run.rs:136`; **not** `Worker.paused` at `run.rs:108`, which is the planner's per-agent
`mantra_pause_agents` flag and stays untouched) is set by four sites with two different behaviours (`run.rs:1224-1252`
interrupts everything; `run.rs:923` and `run.rs:1052` just flip the flag), and the UI shows an opaque
`‖ PAUSED` badge (`ui/stage.rs:196-199`) that the user does not understand.

1. `engine/run.rs`: replace `pub paused: bool` with
   ```rust
   pub halt: Option<Halt>;
   pub struct Halt { pub reason: HaltReason, pub agent: Option<AgentId>, pub message: String, pub since: Instant }
   pub enum HaltReason { User, Auth, UsageLimit, ProviderRejected, Environment, GateExhausted, AttemptsExhausted, AgentTurnFailed }
   ```
   `fn halted(&self) -> bool`. One function `halt(ctx, reason, agent, message)` (interrupts busy agents into
   `paused_agents`, journals `⛔ halted: …`, notifies) and one `resume(ctx)` (the current un-pause body).
   `toggle_pause` stays only as the user's `space` (`HaltReason::User`, message "paused by you"). All four
   sites call `halt`.
2. Resume hints per reason (shown in the stage header band and the alert): User → `space resume`; Auth /
   UsageLimit → `fix credentials or quota, then space`; ProviderRejected → `m switch model for <role> · r
   retry`; Environment → the fix hint from WP12.4 (sandbox) or the missing-variable hint (WP12.6), plus `r retry`; GateExhausted → `type feedback for the planner or space to give the gate N more rounds`;
   AttemptsExhausted → `r retry <task> · type feedback`; AgentTurnFailed → `r respawn <agent> · space retry
   the turn`.
3. Stage header (`stage.rs:181-209`): replace the badge with a full-width amber band on the line under the
   header while halted: `⛔ halted · <message> · <hint>`. The `‖ PAUSED` reverse-video badge is deleted.
   Nothing animates while halted (already true).
4. `stage.rs:519` footer line and `overlays.rs:58` help text: update wording (`space` = "pause / resume
   everything").
5. `ErrKind` classification (`agent.rs:773-816`): add `ErrKind::ProviderRejected` for HTTP 400/422 and
   messages containing `Unexpected message role`, `unsupported`, `invalid_request_error`; do **not** retry
   those (they are deterministic) — halt with `ProviderRejected` immediately, naming role, alias and provider.
6. `m` on a halted run (stage nav mode): opens the model picker with `target = that agent's role`; picking a
   model sets `role.model` for this run's pattern copy and respawns the agent (WP7.4).

Tests: `engine::run` unit test that constructs a `Run` with the mock `Ctx` used in existing tests (add a minimal
`struct NoCtx` if none exists), calls `halt(..ProviderRejected..)` and checks that `halted()`, that busy agents
were interrupted, and that `resume` re-prompts them. Snapshot: `--demo` run has a scripted 502 on `-auth`; add a
scripted 400 path in `mock.rs` (`mock.rs:644-647` style, trigger on task id suffix `-badmodel`) and `snap`
the halt band.

---

### WP7 — Watchdog: every agent that should be working is working

Stall detection (`engine/run.rs:1178-1222`) only scans `self.workers`; `wake_orch` (`run.rs:343-355`) is
reactive; planner/orchestrator/gate/finale have no liveness at all; retries are worker-only
(`run.rs:1297-1309`, `app.rs:1344-1353`).

**7.1 Expected-activity model.** Add to `Run`:
```rust
fn expected_active(&self) -> Vec<(AgentId, Expect)>   // who must be busy right now and why
enum Expect { Planning, Reviewing /*never expected busy*/, Orchestrating, Working(task_id), Gating, Finale(idx) }
```
Rules:
- `Stage::Planning` → planner. `Stage::Review` → nobody.
- `Phase{Orchestrating}` → each worker with `WState::Running`; the orchestrator **iff** (`orch_inbox` non-empty)
  or (no worker Running/Preparing/Retrying **and** some task is Queued/Failed/not spawned). An orchestrator
  sleeping via `mantra_wait` while workers run is legitimate.
- `Phase{Merging|Checks}` → nobody (jobs). `Phase{Gate}` → gate agent. `Handoff` → nobody.
- `Finale{idx}` → finale agent.

**7.2 Liveness tick.** In `tick`, for each expected agent compute `idle_for = agent.last_event.elapsed()`
when `!agent.busy()`, and `silent_for` when busy but no protocol event for `stall_minutes` (existing rule).
New pattern settings (`pattern.rs:47-76`, editable in the Studio settings panel):
`watchdog_seconds = 90` (idle grace), `watchdog_escalate_seconds = 240`. Per agent keep a
`WatchState { nudges: u8, last_action: Instant }` in `Run` (map keyed by AgentId; cleared on any event from
that agent).

Escalation ladder, executed at most once per `watchdog_seconds` per agent, journaled with `⏰`:
1. **Nudge the agent itself**: `ctx.prompt(agent, "[mantra:watchdog] You are expected to be <Expect> but have
   been idle for Ns. Continue, or call mantra_wait/mantra_log to say why you are waiting.")`. Workers are
   nudged with their task reminder.
2. **Wake the orchestrator** with the same event text (`orch_event`), for any non-orchestrator agent. For the
   orchestrator itself, step 2 is a *respawn* (7.4) with the phase brief and a `[mantra:watchdog]` prefix.
3. **Wake the planner** (`ctx.prompt(planner, "[mantra:watchdog] The orchestrator did not act on <agent> … Decide:
   mantra_brief_orchestrator, mantra_revise_plan, mantra_spawn_adhoc, or mantra_pause_agents")`; the planner's
   existing re-prompt tools apply.
4. If the planner is itself unresponsive after step 3 → `halt(AgentTurnFailed, planner, …)` with hint `r respawn
   planner`.

A worker whose turn ended with `interrupted` or `failed` and that is still `WState::Running` is treated as idle
(F3 loop protection: after the orchestrator has prompted the same idle worker 3 times within 5 minutes without a
new `turn/completed` "completed" from it, the watchdog respawns the worker instead — `retry_worker` with a note).

**7.3 Turn-failure fallthrough.** `run.rs:972-985`: a planner/orchestrator/gate/finale turn that fails past
the retry cap currently halts. Before halting, run the ladder once (respawn that agent with resume text) and only
halt if the respawned agent fails again.

**7.4 Respawn any agent.** Generalize `retry_worker` into
`pub fn respawn(&mut self, ctx, a: AgentId, note: Option<String>) -> Result<(), String>`:
- worker → existing path (`run.rs:1300-1308`).
- planner → `ctx.stop(old, archive=true)`, `spawn_planner` (`run.rs:394-410`) then prompt with the current
  brief plus `plan.json` if a plan exists (`[mantra:resume] A plan v{n} exists (attached). Continue from stage
  {stage}.`); if `Stage::Review`, stay in Review.
- orchestrator → the orchestrator branch of `start_phase` (`run.rs:462-484`) extracted into
  `spawn_orchestrator(ctx, phase_idx)`, then re-send the phase JSON with the current worker states appended.
- gate → `spawn_gate` (`run.rs:664-698`) for the current round.
- finale agent → the spawn branch of `start_finale` (`run.rs:792-806`) extracted into `spawn_finale(idx)`.
Keys: stage nav `r` (`app.rs:1344-1353`) = if crashed → `Cmd::Restart` (unchanged) else `respawn`. Zoomed
view: `/respawn` command and `ctrl+r`. Both work for every node in `stage_nodes()`. A `[mantra:respawn]`
journal line records who/why.

**7.5 Tool guards (F3).** In `on_tool_call` (`run.rs:1313`): `mantra_prompt`, `mantra_interrupt`,
`mantra_retry`, `mantra_set_effort` return `Err("…task is done; the phase is merging/gating — wait for the
handoff")` when the target worker is `Done` and `PhaseStep != Orchestrating`, or when the worker was archived.

**7.6 Journal + UI.** Watchdog actions show on the pulse feed with `⏰` and the stage card border of a
watched-idle agent turns amber dotted with the text `idle 2m · watchdog`.

Tests (unit, with a fake `Ctx` that records calls): (a) orchestrator idle with an unspawned task → nudge at
90 s, respawn at 240 s, planner at 480 s; (b) orchestrator sleeping while a worker runs → no action;
(c) F3 loop → worker respawn after 3 prompts; (d) `respawn(planner)` during Review keeps `Stage::Review`.
Demo: extend `mock.rs` orchestrator script with a `[mantra:watchdog]` branch that logs and calls `mantra_wait`,
and a `MANTRA_MOCK_LAZY_ORCH=1` env that makes the mock orchestrator ignore the first phase prompt so the
watchdog path is exercised by `stress.sh`.

---

### WP8 — Zoom vs overview: make the two views unmistakable; planner zoomed at start; review overlay in zoom

Facts: `Screen::Zoom(id)` reuses `solo::draw` (`ui/solo.rs:9-46`) and differs only by a breadcrumb; runs always
start on `Screen::Stage` (`app.rs:580-582`); the plan-review overlay auto-opens only when
`screen == Screen::Stage` (`app.rs:757-772`); approving sets `Screen::Stage` (`overlays.rs:417-436`).

1. **Zoomed frame.** In `solo::draw` when `zoom.is_some()`:
   - header line gets a solid background band in the agent's role colour (`theme::named(&a.color)`,
     `theme::mix` to 25% for the band, bold text on it): `▐ ✦ planner · astra · high ▌  zoomed · esc back to
     overview`. Solo keeps its plain header.
   - a 1-cell left "spine" in the role colour down the full height of the log area (draw `▎` per row), so any
     frame is recognisable as "inside one agent" even when scrolled.
   - the input placeholder: `message ✦ planner …  (⏎ queue · ctrl+f send now · esc overview)` (WP9 keys).
   - the footer hint line ends with `esc ▸ overview`.
2. **Overview frame.** `stage.rs:181-209` header crumbs: `mandala › overview` then run id, pattern; when
   `canvas_focus` is on, draw the input box with a dim border and the text `navigating — ←→↑↓ select · ⏎ zoom
   · 1-9 jump · tab to type`; when off, the selected card keeps its heavy border and the peek strip keeps the
   `⏎ zoom` cue (already there, `stage.rs:896-917`). Add number keys `1`..`9` in nav mode = select the nth
   node of `stage_nodes()` and zoom immediately.
3. **Planner zoomed by default.** `app.rs:580-582`: `self.screen = run.planner.map(Screen::Zoom).unwrap_or(Screen::Stage)`
   (capture `run.planner` before the move). Also when the planner is respawned during Planning (WP7.4) keep the
   current screen.
4. **Review overlay opens everywhere.** `app.rs:757-772`: drop the `self.screen == Screen::Stage` condition;
   keep the "not already open" check; do not open it over Studio/Models (`matches!(self.screen, Screen::Stage |
   Screen::Zoom(_) | Screen::Solo)`).
5. **Approve → overview.** Both approve paths (`overlays.rs:417-436` and `app.rs:1333-1337`) end with
   `self.screen = Screen::Stage; self.canvas_focus = true; self.sel = index of orchestrator`, so the user lands
   on the animated overview with the orchestrator selected. Typed feedback in the overlay keeps the current
   screen (the planner is being re-prompted; if the user was zoomed on it they stay).
6. **Transition flash.** On every Stage↔Zoom switch set `app.flash_screen = Some(Instant::now())`; for 400 ms
   draw the header band at full role colour (Zoom) or a full-width dim `overview` band (Stage), fading via
   `anim::fade`. Respect `reduce_motion` (no flash).

Tests: snapshot script `wait:0.5;snap:start-zoomed;until:plan review@60;snap:review-in-zoom;key:a;wait:0.3;snap:overview-after-approve`
in `--demo` — assert (by reading the frames) that the first frame contains `zoomed`, the second contains the
review overlay title, the third contains `overview`. Add to `stress.sh`.

---

### WP9 — Queued messages with force-send (Claude-Code style) in every agent chat

Today: Enter while an agent is busy sends `turn/steer` immediately (`app.rs:231-252`), and messages typed during
the `awaiting_start` window go to `Agent.queued` (`agent.rs:133`), which is invisible.

Design (applies to Solo, Zoom and `@name` messages from the stage; `!cmd` is unchanged):

- **Enter while the agent is busy → queue.** The message is appended to `Agent.queued` and drawn as a chip
  row directly above the input box: `⏳ queued 1 · "use axum instead…" · ctrl+f send now · backspace on empty
  input to edit`. Multiple queued messages show `⏳ queued 2`. On `turn/completed` the queue is drained into
  a new turn (existing behaviour, `app.rs:831-840`) — this is the "delivered after the current work" path.
- **ctrl+f = force-send now.** Delivers the queued messages *and* the current input (if any) into the running
  turn without interrupting it: Codex → `Cmd::Steer` (`turn/steer`); Claude backend → an NDJSON user message,
  which Claude Code injects at the next tool boundary (verified). The agent keeps working with the extra
  message, exactly the requested behaviour. If the agent is idle, ctrl+f behaves like Enter. If a turn is only
  `awaiting_start`, the force is deferred to `turn/started` (existing drain at `app.rs:809-815` already sends
  `Cmd::Steer`).
- **Backspace on an empty input** pops the last queued message back into the input for editing.
  **ctrl+x on an empty input** discards the whole queue (toast `queue cleared`).
- Esc keeps all its current meanings (`app.rs:1257-1277`): back from zoom with empty input, interrupt when busy,
  clear input. No new Esc semantics.
- Why ctrl+f: it is unbound in `app.rs:1098-1291`, `studio.rs` and `input.rs`. One collision must be fixed:
  the plan-review overlay's `f` ("focus canvas", `overlays.rs:437`) matches `KeyCode::Char('f')` with any
  modifier and overlays are dispatched first (`app.rs:1176-1179`); since WP8 opens that overlay while zoomed,
  guard that arm with `!k.modifiers.contains(KeyModifiers::CONTROL)` so ctrl+f reaches the input. Also ctrl+s is
  taken by the Edit overlay and is XOFF in some terminals; ctrl+j / alt+⏎ insert newlines (`input.rs:119-126`).
  Add ctrl+f to the help overlay (`overlays.rs:50-60`), the README key lists, and the footer hints.

Implementation notes:
- `prompt_agent` gains a `mode: Send` parameter, `enum Send { Auto, Queue, Force }`. `Auto` is used by the
  engine (`Ctxt::prompt`, `app.rs:286-288`) and keeps today's semantics (engine steers immediately). The UI's
  Enter uses `Queue` when `a.busy()`, ctrl+f uses `Force`.
- `Run::direct` (`run.rs:1285-1295`, `@name` from the stage) also takes `Send` and the stage input honours the
  same keys (Enter queues if the target is busy, ctrl+f forces).
- Draw the chip in `draw_input` (`ui/mod.rs:249-275`) from the *focused* agent's queue (`focus_agent`,
  `app.rs:438-446`); on the stage use the selected node.
- The queue survives a crash/restart of the agent (it is on `Agent`, not on the process).

Tests: `agent::tests`/`app` unit test: busy agent + Enter → `queued.len()==1`, no `Cmd::Steer` sent; ctrl+f →
one `Cmd::Steer` whose text joins queue + input; backspace on empty input restores the text. Demo snapshot:
`type:hello;key:enter;until:Thinking@30;type:also do X;key:enter;snap:queued;key:ctrl+f;wait:0.3;snap:forced`.

---

### WP10 — Claude Code as a backend for any role

Goal: a locally installed `claude` (subscription **or** any Anthropic-compatible API such as LibertAI) can run
any role, mainly planner and orchestrator, with no permission prompts ever, with output, status, stop and
resume handled by Mantra like a Codex agent.

#### 10.1 Verified CLI contract (Claude Code 2.1.269) — this is the wire protocol to implement

Spawn (one long-lived process per agent, in the agent's cwd/worktree):
```
claude -p --input-format stream-json --output-format stream-json --verbose \
       --dangerously-skip-permissions --session-id <uuid v4> --model <model> [--effort <e>] \
       [--autocompact <tokens>] [--append-system-prompt <role instructions>] \
       [--strict-mcp-config --mcp-config '<json>'] [--bare]
```
- stdin, one JSON per line: `{"type":"user","message":{"role":"user","content":"<text>"}}`. Multiple messages
  over the life of the process are fine (verified: memory persisted across two messages). A message written
  while a turn is running is injected at the next tool boundary of that turn (verified) — this is "steer".
- interrupt: `{"type":"control_request","request_id":"<id>","request":{"subtype":"interrupt"}}` → the process
  answers `{"type":"control_response","response":{"subtype":"success","request_id":"<id>", …}}`, the running tool
  gets a rejected `tool_result`, a `user` message `[Request interrupted by user for tool use]` follows and a
  `result` closes the turn (verified).
- stdout events (one JSON per line): `system/init` (`session_id`, `model`, `tools`, `mcp_servers[].status`,
  `permissionMode`, `apiKeySource`, `capabilities`), `autocompact_state` (`value.effective_window`,
  `value.threshold`), `assistant` (`message.content[]` of `text` / `tool_use{id,name,input}`; `message.usage`),
  `user` (`message.content[]` `tool_result{tool_use_id,content,is_error}`; `tool_use_result{stdout,stderr}` for
  Bash), `system/task_summary`, `system/post_turn_summary`, `system/api_retry` (`error_status`, `attempt`),
  `system/commands_changed` (ignore), and `result` (`subtype` `success|error_*`, `is_error`, `num_turns`,
  `terminal_reason` `completed|api_error|…`, `api_error_status`, `usage.{input_tokens,output_tokens,
  cache_read_input_tokens,cache_creation_input_tokens}`, `modelUsage.<model>.contextWindow`, `total_cost_usd`,
  `result` text, `session_id`, `permission_denials`).
- exit codes: 0 success, 1 failure (bad model gives a `result` with `terminal_reason: api_error` and exit 1), 124
  only from an outer `timeout`. **A 401 is retried by the CLI 10 times (~1 s apart) before it gives up** —
  Mantra must kill the process on the first `system/api_retry` with `error_status` 401/403 and classify Auth.
- resume after a crash: `claude -p … --resume <session-id>` (verified: history restored). Session files live in
  `~/.claude/projects/<cwd-slug>/<session-id>.jsonl`.
- root: `--dangerously-skip-permissions` is refused for uid 0 unless `IS_SANDBOX=1` is in the environment
  (verified). Mantra sets it automatically when `geteuid()==0` and prints a doctor note.
- env hygiene: the child inherits the parent's `CLAUDE_*` variables; in particular `CLAUDE_CODE_SESSION_ID`
  overrides `--session-id` (observed). Mantra must **remove every env var starting with `CLAUDE` and
  `CLAUDECODE`** from the child, then set its own.
- auth modes:
  - `subscription`: no `--bare`, no `ANTHROPIC_*` set (the CLI uses its OAuth login). Cannot be tested in this
    sandbox (it hangs because the sandbox provider is host-managed) — mark as "manually verified by the user".
  - `api_key`: `--bare` + `ANTHROPIC_API_KEY=<key>` (+ `ANTHROPIC_BASE_URL=<base>` for third parties, **without**
    `/v1`: `https://api.libertai.io`). Verified end to end with LibertAI incl. tools and MCP.
- MCP tools: `--strict-mcp-config --mcp-config '{"mcpServers":{"mantra":{"command":"<path to mantra>",
  "args":["mcp-bridge","--sock","<path>","--agent","<id>"]}}}'`; the tool appears as
  `mcp__mantra__<tool name>` and is called like a native tool (verified with a stdio server: `initialize` →
  `notifications/initialized` → `tools/list` → `tools/call` with `params.name` and `params.arguments`;
  protocol version `2024-11-05` accepted).
- `--autocompact N` sets an effective window of `N-20000` and compacts at 80 % of that (observed: 100000 →
  effective 80000, threshold 64000). Pass `N = effective_context` from WP2; the Mantra gauge marks
  `autocompact_state.value.threshold`.
- `--effort` accepts `low|medium|high|xhigh|max` (verified accepted with a third-party model; effect depends on
  the model).

#### 10.2 Configuration model

- `ProviderEntry` (`config.rs:227-240`) gains `kind: ProviderKind` (`#[serde(default)]`, `Codex` |
  `ClaudeCode`) and for `ClaudeCode`: `auth: "subscription" | "api_key"` (default `subscription`),
  `env_key` / `api_key` as in WP5 (used only for `api_key`), `base_url` optional (empty = Anthropic).
  `wire_api` stays irrelevant for this kind.
- Built-in provider `claude` (`kind = ClaudeCode`, `auth = subscription`, name `Claude Code`) is added by
  `Registry::defaults()` **only when `claude` is on PATH** (check once at startup; `doctor` reports it). Its
  default models (added the same way, alias → model, context 200k, compact 85, efforts
  `low,medium,high,xhigh,max`, `default_effort = "high"`):

  | alias | model |
  |---|---|
  | `opus46` | `claude-opus-4-6` |
  | `opus48` | `claude-opus-4-8` |
  | `opus5` | `claude-opus-5` |
  | `sonnet5` | `claude-sonnet-5` |
  | `fable5` | `claude-fable-5` |
  | `fable51` | `claude-fable-5-1` |

  Plus `[1m]` variants (`opus5-1m` → `claude-opus-5[1m]`, `sonnet5-1m`, context 1,000,000) added but marked
  `note = "1M context; needs an eligible plan"`. Users with a third-party gateway add a second provider of
  kind `ClaudeCode` with `auth = api_key`, their `base_url`, and press `D`: discovery for this kind lists the
  six defaults above (there is no `/models` endpoint for Claude subscriptions) **plus**, when `base_url` is set,
  whatever `GET {base_url}/v1/models` returns (LibertAI does), so `qwen3.8-27b` via Claude Code is one keypress.
- `Backend` is derived: `registry.backend_of(model) = provider.kind`. `SpawnSpec` (`hub.rs:16-28`) gets
  `backend: Backend` and `envs: Vec<(String,String)>`, `claude: Option<ClaudeSpawn { auth, base_url,
  session_id, resume: bool, mcp_sock: PathBuf, autocompact: u64, system_prompt: String }>`.
- Studio: the role `model` field shows `via Claude Code ✧`; `permission` (WP3) for a Claude agent maps to
  `never` → `--dangerously-skip-permissions`, `on-request`/`untrusted` → `--permission-mode acceptEdits
  --permission-prompts none` **plus** a warning in the Studio that Claude agents cannot ask (denied actions are
  reported in `permission_denials`); keep it simple and documented.
- `sandbox` for Claude agents: `read-only` → `--tools "Read,Glob,Grep,WebFetch"` (and `--disallowedTools
  "Edit,Write,Bash"`); `workspace-write` → `--add-dir <cwd>` only; `danger-full-access` → `--add-dir /`.
  The worktree isolation from git still applies (Claude runs in the worker's worktree cwd).

#### 10.3 Hub: `run_claude_process`

Add `hub::claude` module; `agent_task` (`hub.rs:104`) dispatches on `spec.backend` to `run_process` (Codex,
unchanged) or `run_claude_process`. Both return the same `Exit` and emit the same `HubEvent`s. The Claude task:

1. Build the command (10.1). Environment: start from the parent env, remove all `CLAUDE*` keys, remove
   `ANTHROPIC_*`, then set: `api_key` mode → `ANTHROPIC_API_KEY`, `ANTHROPIC_BASE_URL` (if any);
   `IS_SANDBOX=1` if euid==0; `CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1`. Spawn with piped stdio,
   `kill_on_drop`, cwd = `spec.cwd`. Reuse the line reader pattern from `rpc.rs:138-172` (NDJSON; unknown
   lines are logged at debug and ignored; non-JSON lines such as `[claude-code:unrecognized_model] …` are
   logged and ignored).
2. On `system/init`: send `HubEvent::Ready { thread_id: session_id, model, resumed }`. Keep `mcp_servers`
   status; if the `mantra` server is not `connected`, treat as `Exit::Crashed("mcp bridge failed")`.
3. `Cmd` mapping: `Turn{text}` / `Steer{text}` → write one user line (Claude's own queueing does the rest;
   Mantra's `turn_active` is set when the first `assistant` event of a turn arrives, and `awaiting_start` while
   waiting for it). `Interrupt` → `control_request interrupt`. `Compact` → write the user line `/compact` (verified: the
   process emits `system/status {status:"compacting"}`, then `system/status {compact_result:"success"}`, a
   repeated `system/init` (ignore repeats — never emit a second `Ready`), `system/compact_boundary` with
   `compact_metadata.{trigger,pre_tokens,post_tokens}`, a synthetic `user` summary message, and a `result`
   with `num_turns: 0`). Map `compact_boundary` → `Kind::Compaction{from: pre_tokens, to: post_tokens}` and
   treat a `result` with `num_turns == 0` that follows a compaction as housekeeping: **no** `turn/completed`,
   no `TurnDone` (the same rule Codex compaction turns already follow, `agent.rs:434-485`). Automatic
   compactions arrive the same way with `trigger: "auto"`. `Archive` → nothing (session file stays; the process is killed
   on `Shutdown`). `Shell{command}` → not supported for this backend → `CmdFailed`. `SetEffort(e)` /
   `SetModel(m)` → store, then restart the process with `--resume <session_id>` at the next idle moment (never
   mid-turn; queue it). `SetApproval` → same restart rule. `Respond{id,result}` / `RespondErr` → answer the
   pending MCP bridge call with that id (10.4). `Restart` → kill and return `Exit::Crashed("restart requested")`
   so `agent_task` resumes with `--resume`. `Shutdown` → interrupt if a turn is active, then kill.
4. **Translate events to the existing Codex-shaped notifications** so `Agent::apply` (`agent.rs:270-489`)
   needs only small additions. Emit `HubEvent::Notif` with these synthetic methods:
   - first `assistant` of a turn → `turn/started`;
   - `assistant` text block → `item/started`+`item/completed` with `type: agentMessage` (one item per block,
     ids `cc-<n>`); if `--include-partial-messages` is later enabled, map `stream_event` deltas to
     `item/agentMessage/delta` — not required for v0.2;
   - `assistant` `tool_use` → `item/started` with `type: dynamicToolCall` for `mcp__mantra__*` (name stripped
     to `mantra_*`), `type: commandExecution` for `Bash` (`command` from `input.command`), `type: fileChange`
     for `Edit|Write|MultiEdit|NotebookEdit` (`path` from `input.file_path`; the diff is not available — mark
     `kind: "modified"` and let the git-based file stats fill `+/-`), `type: mcpToolCall` for everything else;
   - `user` `tool_result` → `item/completed` for the matching id, `status: completed|failed` from `is_error`,
     `output`/`result` from `content` (string or first text block), Bash `stdout+stderr` from
     `tool_use_result`;
   - `result` → `thread/tokenUsage/updated` (`total = usage.input_tokens + output_tokens`, `ctx_used =
     usage.input_tokens + cache_read + cache_creation` of the *last* assistant message, `ctx_window =
     modelUsage.*.contextWindow` or the spawn `autocompact`), then `turn/completed` with `status`
     `completed` when `!is_error`, `interrupted` when the turn ended after an interrupt control, `failed`
     otherwise with `error` = `result` text and `errorInfo` derived from `terminal_reason`/`api_error_status`
     (`401|403` → `Auth`, `429|529|5xx` → `Transient`, `400|422` → `ProviderRejected`, `context`/`prompt is
     too long` → `ContextFull`, `error_max_budget_usd` → `UsageLimit`);
   - `autocompact_state` → remember `value.threshold` for the gauge marker (no item); `system/compact_boundary`
     → the `Kind::Compaction` item as described in step 3;
   - `system/api_retry` with 401/403 → kill the child, `Exit::Crashed("auth: <error>")`, and let the App
     classify Auth (halt, WP6); other statuses → `Notice` "retrying (attempt n)".
   Mantra-side reducer changes: `agent.rs` accepts `type: dynamicToolCall` items without a codex `turnId`, and
   `thread/tokenUsage/updated` with the fields above. Add a unit test that feeds the exact recorded event lines
   from this session (put them in `src/testdata/claude-stream.jsonl`) through the translator and asserts the
   `Item` sequence and `Signal::TurnDone`.
5. Crash/restart: `Exit::Crashed` triggers the existing backoff in `agent_task`; the respawn passes
   `resume: true` so the command uses `--resume <session_id>` instead of `--session-id`.

#### 10.4 Dynamic tools via `mantra mcp-bridge`

- **DONE (src/bridge.rs, verified against the real CLI):** subcommand `mantra mcp-bridge --sock <path> --agent
  <id>`, intercepted like `mock-codex` (`main.rs`). Wire protocol: every request carries a `call` id —
  `{"agent","call","list":true}` → `{"call","tools":[…]}` and `{"agent","call","tool","args"}` →
  `{"call","ok","text"}`; the first line after connecting is `{"agent","hello":true}`. What remains is the
  Mantra side below. It speaks MCP over its stdio (`initialize` → capabilities `{tools:{}}`, `tools/list`, `tools/call`;
  answer `ping`; ignore notifications) and forwards every `tools/call` over a Unix socket to the running
  Mantra process as one JSON line `{"agent":<id>,"call":"<n>","tool":"mantra_submit_plan","args":{…}}`,
  then blocks until the line `{"call":"<n>","ok":true|false,"text":"…"}` comes back and returns it as
  `content:[{type:text,text}]` (`isError` when `!ok`).
- Tool list: the bridge asks Mantra `{"agent":<id>,"list":true}` at `tools/list` time; Mantra answers with the
  role's tool array from `engine/tools.rs` (the same JSON schemas, `inputSchema` = the codex `inputSchema`).
- Mantra side: `hub::claude` owns a `tokio::net::UnixListener` at `$MANTRA_HOME/run/<mantra-pid>.sock`
  (created at startup when any Claude provider exists; removed at exit). Each bridge connection is tagged by
  `agent`; a `tools/call` becomes `HubEvent::Request { agent, id: "cc-call-<n>", method: "item/tool/call",
  params: {"name", "arguments"} }` — exactly the shape `App::on_request` (`app.rs:929-983`) already dispatches
  to the engine (`Run::on_tool_call`). Reply routing: `Cmd::Respond{id,result}` / `RespondErr` are delivered to
  the *per-agent* `run_claude_process` task (`Hub.agents` is one `mpsc::Sender<Cmd>` per agent, `hub.rs:59,75-81`),
  so the bridge must not be a single shared acceptor that the per-agent task cannot reach. Design: the
  listener lives in `Hub` (one socket for the process), and its accept loop reads the first line of every
  connection (`{"agent":<id>,"hello":true}`) and then **hands the connection to that agent's task** through a
  new `Cmd::Bridge(UnixStream)`; from then on `run_claude_process` owns the stream, reads `tools/call`
  lines from it, keeps `pending_calls: HashMap<String /*call id*/, ()>`, and writes the reply line on
  `Cmd::Respond`/`RespondErr` whose `id` starts with `cc-call-`. If the agent task has no bridge stream yet
  when a `Respond` arrives (restart race), the reply is dropped with a log line and the agent is re-prompted
  by the existing `CmdFailed` path. No engine or app code changes beyond accepting the id prefix.
- Role instructions (`engine/tools.rs:111,123,134,145,153` `mantra-role:` blocks and the developer
  instructions) are passed with `--append-system-prompt`; the text tells the agent that the `mantra_*` tools
  appear as `mcp__mantra__mantra_*`.

#### 10.5 Doctor, discovery, docs

- `doctor` (`main.rs:457-495`): `claude --version` (with a 5 s timeout: run the `Command` on a thread and
  `recv_timeout`), print `✓ claude 2.1.269`, `IS_SANDBOX note: running as root — Mantra sets IS_SANDBOX=1 for
  claude agents`, and for each Claude provider: `subscription` → `claude auth status` if that subcommand exists
  else skip; `api_key` → `✓ key set` per WP5.
- `D` on a ClaudeCode provider (10.2). Codex-catalog discovery is unchanged.
- README: new section "Claude Code agents" (install, subscription vs API key, third-party base URL, which roles,
  what `permission`/`sandbox` mean for Claude agents, the root note, the six default models). DESIGN.md §2/§3:
  the backend seam and the MCP bridge diagram.

#### 10.6 Mock and tests

- `mantra mock-claude` (new module `mock_claude.rs`, intercepted like `mock-codex`): a fake `claude` that reads
  the NDJSON stdin and emits the event lines of 10.1 with the same role scripts as `mock.rs` (`planner` submits
  the demo plan via a `tool_use` named `mcp__mantra__mantra_submit_plan`, `orchestrator` spawns, `worker` edits
  a file with an `Edit` tool_use, etc.). Role selection: the `mantra-role:` marker is inside
  `--append-system-prompt`; the mock parses argv for it. `MANTRA_MOCK_SPEED` applies. In `--demo`, the
  Registry gets a `claude-demo` provider whose command is `mantra mock-claude` (add `Settings.claude_command:
  Vec<String>` default `["claude"]`, mirroring `codex_command`, and let the demo override it).
- `stress.sh`: add a third run with `--demo --pattern mantra-default-claude` (a built-in second pattern
  identical to the default but with planner+orchestrator on `fable51` and workers on `sonnet5`) through plan,
  phase and finale; sweep every screen.
- Live tests (paste-ready, in `scripts/live-claude.sh`): (1) Solo turn with a `ClaudeCode` provider
  `auth=api_key base_url=https://api.libertai.io model=qwen3.8-27b`: ask for a file to be created → file
  exists, header shows `via LibertAI (claude)`, tokens > 0. (2) `mantra run` with planner/orchestrator on that
  provider and workers on Codex+LibertAI → plan submitted through the MCP bridge, phase completes. (3) Kill the
  claude process mid-turn (`kill -9`) → journal shows restart with `--resume`, agent continues. (4) Bad key →
  halt `auth` within 5 s (not 10 retries). (5) `ctrl+f` during a long Bash tool → the extra instruction is
  followed after the tool returns.

---

### WP11 — Runs: list, resume, delete

Run state is write-only today (`engine/run.rs:237-247`; `stage` is a `{:?}` string).

1. `engine/run.rs`: make `Stage`, `PhaseStep`, `WState` `Serialize/Deserialize` and introduce
   `#[derive(Serialize, Deserialize)] struct RunState { id, brief, pattern, plan_version, branch, base_branch,
   stage, started_unix, updated_unix, workers: Vec<WorkerState{task, state, attempt, branch, worktree}>,
   agents: Vec<AgentState{role, name, backend, model_alias, effort, thread_id, cwd}> }` written by
   `save_state` (same file) and read by `RunState::load(dir)`. Update `thread_id`s in `on_ready`.
2. `mantra runs` subcommand (`main.rs`): table of all runs under `~/.mantra/runs/**` (project, id, stage,
   updated, brief), `mantra runs delete <id>` (removes the run dir, its worktrees under
   `~/.mantra/worktrees/<id>/`, and the `mantra/<id>` + `mantra-w/<id>/*` branches via the helpers in
   `engine/git.rs`; asks `y/N` unless `--yes`), `mantra runs resume <id>`.
3. `/runs` command + `ctrl+shift+r`… no: keep keys simple — `/runs` opens `Overlay::Runs` (list; `⏎` resume,
   `D` delete with a confirm line, `esc`). Also shown on the welcome screen when the project has unfinished
   runs: `↻ 1 unfinished run — /runs`.
4. Resume semantics (`Run::resume(state, ctx)`): recreate `Workspace` from `branch`/`base_branch`, re-attach
   agents whose `thread_id` is known via `SpawnSpec.resume_thread` (Codex) / `--resume` (Claude), and restart
   the state machine **at a safe boundary**: `Planning`/`Review` → re-prompt the planner with the saved plan;
   `Phase{Orchestrating}` → respawn the orchestrator (WP7.4) and re-attach or respawn workers whose worktrees
   still exist (missing worktree → `mantra_retry` path); `Merging|Checks|Gate` → re-run from `Merging`;
   `Finale{i}` → restart step `i`; `Done|Failed` → open read-only (journal + `/land` allowed for Done). Journal
   `↻ resumed at <stage>`.
5. Unit tests: `RunState` round-trip; resume of a `Phase{Orchestrating}` state with a fake `Ctx` spawns the
   orchestrator and the still-running workers. Demo: `--demo`, start a run, quit with ctrl+c at phase 1, restart
   with `--demo --resume-last`, snapshot shows the phase continuing.

---

### WP12 — Model/provider robustness found in real runs

1. **Developer-role probe** — done in WP4.4. Additionally, when `ErrKind::ProviderRejected` names `message
   role`, the halt hint says `this provider rejects Codex's developer messages; use it through Claude Code
   (kind = claude-code) or pick another model`.
2. **Reasoning effort on custom models**: LibertAI models with `-thinking` suffix accept `reasoning`; keep the
   current rule (effort only when the provider advertises it) but let discovery mark `*-thinking` ids as
   reasoning-capable for the LibertAI and OpenRouter shapes.
3. **Orchestrator verbosity** (README known limitation): add to the orchestrator instructions in
   `engine/tools.rs`: *"Never prompt a worker whose task is done. After spawning, call mantra_wait. One
   mantra_prompt per event."* Measured on the LibertAI run above: 3 unnecessary prompts in one phase.
4. **Sandbox preflight (L1).** New `util::sandbox_probe() -> Result<(), String>` (Linux only; Ok on macOS):
   read `/proc/sys/kernel/unprivileged_userns_clone` (exists and `0` → Err) and
   `/proc/sys/kernel/apparmor_restrict_unprivileged_userns` (`1` → Err), then try `unshare -U true` with a 2 s
   timeout when the binary exists (non-zero → Err). `doctor` prints the result with the fix:
   *"Codex's sandbox needs unprivileged user namespaces. Enable them (`sudo sysctl -w
   kernel.unprivileged_userns_clone=1`, or on Ubuntu 24.04+ `sudo sysctl -w
   kernel.apparmor_restrict_unprivileged_userns=0`), or set `sandbox = "danger-full-access"` in
   `~/.mantra/settings.toml` and in the worker roles (Studio) — workers stay isolated by git worktrees."*
   At startup Mantra runs the same probe once; when it fails, the welcome screen and the stage show a one-line
   amber notice with the same hint, and `mantra run` asks `y/N` before starting (skip the question with
   `--no-sandbox-check`). At runtime, a `commandExecution` item whose output contains `bwrap` or `user
   namespaces` (`agent.rs:491` `apply_item`, `Kind::Command`) raises `Signal::EnvironmentBroken(msg)`; the run
   halts with `HaltReason::Environment` on the first one (WP6) — never spend gate rounds on it (L4). The same
   signature check applies to gate reports: two consecutive reports with the same blocker text halt the run.
5. **stderr noise (L2).** In `rpc.rs` where stderr lines are logged (`rpc.rs:128`): keep a per-process
   `(last_line, count)`; identical consecutive lines are not logged again — when a different line arrives, emit
   `… previous line repeated N×`. Drop lines matching a small deny-list (`OutputTextDelta without active
   item`, `unsupported call: multi_agent_v1`, `cannot update goal because this thread has no goal`,
   `resources/read failed for codex_apps`) entirely except for one debug-level count per process. The crash
   `stderr_tail` (WP1.3) also skips deny-listed lines.
6. **Provider preflight (L3).** `Registry::preflight(pattern) -> Vec<String>` returns one message per role
   whose provider has no usable key (`ProviderEntry::resolve_key()` is None, WP5) or whose alias is unknown.
   `start_run` (`app.rs:563-583`) refuses to start when the list is non-empty and shows the messages in the
   stage input area (`⚠ orchestrator uses astra via zai — $ZAI_API_KEY is not set (/models to fix)`); Solo
   start (`app.rs:410-435`) does the same for the Solo model. `Ctxt::spawn` keeps a last-line defence: a
   spawn for a role with a missing key halts the run with `HaltReason::Auth` and the same message instead of
   letting Codex fail the first request.

---

### WP13 — Docs, scripts, release

1. `scripts/live-env.sh`: writes the §0.3 `models.toml`/`settings.toml` into `$MANTRA_HOME` (default
   `/tmp/mantrahome`), exports `LIBERTAI_API_KEY="$API_KEY"`, `CODEX_HOME=$HOME/.codex-mantra`,
   `IS_SANDBOX=1` when root, and prints the three paste-ready commands: Solo snapshot, Mandala run, Claude
   Solo. Use `until:finale 1/3@…` (F5) and `snap` (not `sweep`) so frames are visible.
2. README: update key lists (ctrl+f, 1-9, r/respawn, m on halt, /runs, /respawn), "Status & known
   limitations" (real runs completed against LibertAI with Codex 0.154.0 and Claude Code 2.1.269; subscription
   auth for Claude verified by the user only), the new sections from WP5/WP10/WP11, and the version.
3. `CHANGELOG.md` v0.2.0 entries for every WP. Tag `v0.2.0` on the merge commit; attach the Linux x86_64
   release binary (`cargo build --release`, replace `mantra-linux-x86_64` at repo root).
4. **One-line installer** (`install.sh` at the repo root, POSIX sh, Linux + macOS):
   `curl -fsSL https://raw.githubusercontent.com/sahelea1/mantra/master/install.sh | sh`.
   Behaviour: detect `uname -s`/`uname -m`; try to download the matching asset from the latest GitHub release
   (`mantra-linux-x86_64`, `mantra-linux-aarch64`, `mantra-macos-arm64`, `mantra-macos-x86_64`; verify the
   `.sha256` next to it); if no asset matches, build from source: ensure `git` and `cargo` (install rustup
   non-interactively with `-y --profile minimal` if `cargo` is missing, then source `$HOME/.cargo/env`), clone
   the repo to a temp dir, `cargo build --release`, copy the binary. Install to `$HOME/.local/bin/mantra`
   (or `$MANTRA_INSTALL_DIR`), add that dir to PATH in the user's shell rc if missing (print what was added),
   then run `mantra doctor`. Idempotent (re-running upgrades). Never needs sudo. README "Install" leads with
   this line; the tar/cargo instructions move below it. Release workflow: `.github/workflows/release.yml`
   builds the four assets on tag push and uploads them with sha256 files.
5. Final gate before tagging: `cargo build` (0 warnings) · `cargo test` (all green incl. the new tests) ·
   `./scripts/stress.sh` (3 runs, no panics) · `scripts/live-env.sh` Solo + Mandala + Claude-Solo all complete ·
   manual review of the six `snap` frames listed in WP8/WP9 for visual coherence (band colours, hints, no
   overlap at 80×24 and 120×36).

---

## 2. Definition of done (checklist for the final reviewer)

- [ ] `Cargo.toml` 0.2.0, `CHANGELOG.md` present, README/DESIGN updated for every behaviour change.
- [ ] `Run.paused` is gone (`Worker.paused` stays); every halt has a reason and a hint; `space` still pauses/resumes.
- [ ] A run whose orchestrator goes idle recovers without the user (WP7 demo scenario) and a wedged planner is
      respawned with `r`.
- [ ] Zoom frames carry the role-colour band + spine; runs start zoomed on the planner; the review overlay opens
      while zoomed; approve lands on the overview.
- [ ] Enter queues while busy, ctrl+f force-sends into the running turn, on Codex and on Claude agents.
- [ ] Studio: editing a role's kind keeps it selected; `permission` exists per role, default off.
- [ ] Model picker shows `via <provider>`, supports `+/-`/`c` context override; Solo header shows the provider.
- [ ] Provider `api_key` stored 0600, never on argv/logs; env var precedence kept.
- [ ] Every agent gets `model_context_window`/`model_auto_compact_token_limit` (Codex) or `--autocompact`
      (Claude); a 16k-context model compacts instead of failing.
- [ ] Claude backend: six default models present when `claude` is installed; api_key mode verified live with
      LibertAI through tools and the MCP bridge; interrupt, resume after kill, and auth failure paths covered by
      tests; `--dangerously-skip-permissions` always passed (with `IS_SANDBOX=1` as root).
- [ ] `mantra runs` list/resume/delete and `/runs` overlay work in `--demo`.
- [ ] Journal and logs contain no ANSI escapes; crash lines carry the exit code.

## 3. Quick reference — what the engine already gives you

| need | use |
|---|---|
| send text to an agent | `Ctx::prompt` → `prompt_agent` (`app.rs:231`) |
| know if an agent should be busy | `Agent::busy()` (`agent.rs:190`), `Run::expected_active()` (WP7) |
| wake the orchestrator with an event | `Run::orch_event` (`run.rs:332`) |
| respawn any agent | `Run::respawn` (WP7.4) |
| halt with a reason | `Run::halt` (WP6) |
| add a dynamic tool | `engine/tools.rs` schema + `Run::on_tool_call` (`run.rs:1313`); Claude agents get it automatically through the bridge |
| add a screen element | `ui/*.rs` pure draw functions over `App`; keep layout arithmetic saturating |
| headless check of a screen | `--snapshot "…;snap:label"` |
| record a claude event fixture | `IS_SANDBOX=1 ANTHROPIC_BASE_URL=https://api.libertai.io ANTHROPIC_API_KEY=$API_KEY claude -p --bare --dangerously-skip-permissions --input-format stream-json --output-format stream-json --verbose --session-id $(uuidgen) --model qwen3.8-27b` and paste NDJSON lines on stdin |
