# Changelog

All notable changes to Mantra are recorded here.

## v0.6.2 — 2026-10-17

An outside tester drove Mantra for hours on an OpenAI-compatible gateway and wrote up what went
wrong. This release answers that list.

- **A gateway outage wakes nobody.** A fix worker that died on a 5xx past its free retries used
  to count as "could not fix what QA found" and woke the manager, then the planner — both on the
  same dead gateway. Any worker the outage took now halts the run with the gateway wording and
  no escalation; `space` / Resume respawns every such worker where it was, fix workers included,
  and the halt band names that one key (no `r` a halted run would refuse).
- **One owner per run.** A run that is open in a live Mantra carries `owner.json`; a second
  process refuses to resume or delete it ("open in another Mantra (pid …) — quit it first, or
  `--take-over`"). A dead owner is a crash and resumes normally; a zombie counts as dead. And
  `--resume-last` picks from this project's runs only, not every project on the machine.
- **A model you switched stays switched.** Changing one agent's model (`m`, `ctrl+k`, the web
  sheet) is now that attempt's choice, saved with the run: a respawn keeps it, a restart brings
  the agent back on it, and other agents of the role are untouched (`set_role_model` still
  changes the role). The first-token fallback still wins over a switched model that went silent.
  A finished card names the model its attempt ran on, not the role's current one.
- **Tokens say what they cost.** Codex counts cached input into every total; a worker that cycles
  tools in a big context reads "6.8M tokens" of which almost all was cache. Cards, headers, the
  run total and the web chips now show the cached share ("6.8M tok · 5.9M cached", "87% cached"
  on a narrow card).
- **A context cap.** `settings.context_cap` (200k in the built-in patterns, 0 = the model's full
  window) is the context every agent is told it has: a 500k window let an agent cycle tools for
  hours at ever-growing cost. The Studio edits it; it takes `200k`, `1m`, `off`.
- **An empty repo gets its first commit.** `git init` with no commit used to put workers in the
  shared project directory; Mantra now makes an empty initial commit (hooks skipped, staged
  files left alone) so every worker gets an isolated worktree.
- **One card per task.** A task with several attempts drew one card but had one selectable node
  per attempt, so the arrows and `1`–`9` could land on a cancelled attempt. The selection follows
  the card, and `r` keeps the respawned task selected.
- **Smaller fixes:** Codex housekeeping warnings ("couldn't save diagnostic logs") no longer pose
  as the agent's activity line; a done worker that answers a follow-up logs "done again" instead
  of a second completion and does not wake the orchestrator twice; the demo honours the context
  cap; `MANTRA_MOCK_GATEWAY_DOWN=<file>` and `MANTRA_MOCK_BIGCTX=1` reproduce the outage and the
  big-context scenarios in `--demo`.

## v0.6.1 — 2026-10-12

Both interfaces catch up with 0.6.0, and were checked frame by frame (the TUI at 120×36 and
80×24, the web UI on desktop and phone, light and dark).

- **Runs are called by their names.** The run title leads in the TUI header (it survives
  narrow terminals; the id and pattern give way first), in the web run cards, the Team screen's
  unfinished runs, and halt / run-complete push notifications; the id stays visible but
  secondary. A run resumed from an older save takes its plan's title instead of the goal's
  first words.
- **Studio edits everything 0.6.0 added.** Roles get a `fallback` model (⏎ / ←→ cycle through
  none and the other aliases); settings get `network` and `writable_roots`; ⏎ on an enum or
  on/off setting steps it instead of opening a text editor that ignored the input. The
  selected field shows a short hint (what `auto` effort picks, what a fallback is for, …), the
  list scrolls on short terminals, and the flow preview wraps instead of clipping.
- **Halts explain themselves.** The TUI halt band reads `⛔ halted · why` over the keys that
  help, and `e` opens the full error, word-wrapped and scrollable, with what to do next. A halt
  about one agent selects it, so `m` and `r` act on the right one. The web halt band can show
  the whole error (wrapped, with Copy) and both say that a model change does not resume the
  run: space / Resume does. Headers say "halted" or "paused" while the run is stopped.
- **Pattern pickers list the built-ins first.** `mantra-default` leads and is marked default,
  then the other built-ins, then your own patterns, each with its one-line description. A
  pattern that does not load is marked and cannot be picked.
- **Effort `auto` and fallbacks are visible.** `auto` gets its own colour in the TUI and the
  web roles screen, which also shows each role's fallback.
- **One clear action per halt.** The web hero and halt band offer the same primary button for
  each halt kind (Resume after a model change; Retry for a gate or a failed turn), the hint
  names that button, and long "Retry <task>" labels end in … instead of running off a phone.
  A stopped run reads "phase 1 · halted" / "paused" on its card, headers and agents instead of
  "working". The browser tab shows the run title.
- **Fallbacks from the web.** Roles & models can set or clear a role's fallback model
  (`set_role_fallback`, refused for an unknown alias or the role's own model); `auto` effort
  is shown muted rather than as an accent on every chip.
- **The TUI never hides a halt.** The zoomed view's header carries the run title and a
  `⛔ halted` / `‖ paused` marker (esc lands on the keys that help); in typing focus the band
  says `tab for keys` instead of advertising keys that would type letters. A provider or
  environment halt says `space resumes` instead of offering `r`, which a halted run refuses.
- **80 columns work.** The model picker drops columns before its key line, the welcome tagline
  wraps at a word, Studio text rows use their full width, and help, halt details, plan and
  diff clamp their scroll so ↑ moves at once after over-scrolling.
- **Smaller fixes:** the help overlay scrolls instead of losing its last rows; a word that
  exactly fills a line no longer pushes a stray space onto the next; the fix-round step reads
  "QA gate · fix r<n>"; the web run card no longer wraps its state label in the sidebar; the
  snapshot script language gains `pgdn`.

## v0.6.0 — 2026-10-12

- **Agents that wait really stop.** `mantra_wait` ends the turn: any further tool call in it is
  refused and the turn interrupted once; a repeated `mantra_status` with nothing new (stage, worker
  states, activity — not the clocks) answers "no change, wait". An orchestrator used to loop on
  the two and burn half a million tokens in three minutes.
- **Sandboxed agents can build.** Workspace-write agents get the network (`settings.network`,
  on by default), the toolchain caches (`~/.pub-cache`, `~/.gradle`, `~/.cargo`, `~/.npm`, the
  Flutter SDK on PATH, …) and the pattern's `writable_roots`. Cache directories a tool on PATH will
  want are created ahead of time, and only directories the user owns are bound (inside the
  sandbox's user namespace another user's directory cannot take Codex's marker and bwrap dies).
  A bwrap failure on such a root names the root instead of blaming user namespaces.
- **Fix workers on the integration copy run unsandboxed.** Codex keeps every `.git` read-only
  in workspace-write, so a fix worker could not unstage a file or merge; the worktree still
  isolates it. An agent's own "I cannot … in this sandbox" question is a question, not a dead
  machine.
- **A gateway outage is not the worker's fault.** A turn that fails on the transport (HTTP 5xx,
  "upstream provider returned an error", a reset connection, a timeout) no longer costs an attempt:
  a running worker retries with a short backoff, and one that failed while the run was halted (or
  outlasted the backoff) is respawned automatically in its own worktree on resume and after a
  restart, logged as `↻ respawning <task> after the gateway outage`. The halt message for a dead
  gateway says to press resume.
- **A rejected reasoning effort is remapped.** A 400 naming `reasoning_effort` / "effort must be …"
  sets the nearest effort the model lists (the lower one on a tie) and retries the turn once.
- **Example gateway config** in `docs/examples/riti`: a `models.toml` for one OpenAI-compatible
  gateway fronting two model families, and the patterns the Android app build ran with.

Mantra should produce coherent code by default without taking unnecessarily long or burning
tokens. A real three-file task cost 760k tokens: the QA gate alone spent 197k re-reading
everything after four passing checks, edited code itself and kept one stale session across rounds.

- **Fresh read-only QA every gate round; Mantra routes the fixes.** Each round spawns a new QA
  agent (the old one is archived) with the check results, the phase's `git diff --stat` and diff
  (capped) and the previous round's verdict. The gate role is read-only and never fixes; a failing
  report (or checks that still fail after a pass) starts a `fix-<round>` worker — `worker-small`,
  `worker-big` when the report sets `big` — scoped to the files the findings name, then the checks
  and the next fresh round run. Merge conflicts go to a fix worker too. The "Route the fix to a
  worker." message to the orchestrator and the `[mantra:gate-round N]` same-session prompts are
  gone. `gate_max_rounds` still bounds the loop.
- **Three built-in patterns.** `mantra-default` (balanced: one `qa-heavy` pass on the diff with a
  security checklist, then planner verification; manager every 10 minutes), `mantra-lean` (no
  manager, planner verification only, `max_parallel = 3`) and `mantra-thorough` (the previous heavy
  flow: QA, security sweep, verification). `mantra-default-claude` derives from the default.
  `Pattern::names()` lists them; fresh ChatGPT-subscription installs save all three.
- **Lean prompts.** Every built-in role is short and budget-aware: what it owns, what it never
  does, when to stop; no re-reading files, no repeated full test suites. Finale agents start from
  the run's diff and the gate reports, which are put in their prompt.
- **`effort = "auto"`.** Resolved per spawn from the role's job (planner, manager, orchestrator,
  gate `medium`; `worker-small` `low`, other workers `medium`; finale roles `high`); a named effort
  is used as it is. The Studio cycles it, `/effort auto` and the web `set_effort` accept it.
- **Recognizable runs.** A run has a `title` (the plan's title, else the goal's first six words)
  saved in `state.json` and shown in `mantra runs`, `/runs`, the TUI header, the web run header and
  `runs_list`; the id stays the key. Codex threads are named `<title> · <role>` via
  `thread/name/set`.
- **No-first-token deadline, with a fallback model.** An agent whose active turn produces nothing at
  all (no tokens, no text, no tool activity — a silent gateway or an endless "Reconnecting… n/5")
  for `settings.first_token_minutes` (default 4, `0` = off) is restarted once; the second time in a
  row its role's new optional `fallback = "<alias>"` model takes over, or — without one — the
  manager is asked. A completed turn clears the count.
- **Quiet manager digests.** The periodic health digest is skipped when no journal line, worker
  state or agent state changed since the previous one.
- **Full halt errors.** The pulse line and the stage band keep the short form; the journal,
  `state.json` and the manager's escalation get the backend's whole error.
- **Changing a model never resumes a halted run.** The change applies and a toast says to resume
  (`space` / Resume). A user pause and a dropped pending transition are now saved at once, and
  every journal line marks the state for a flush on the next tick.

## v0.5.0 — 2026-09-26

Mantra leaves the terminal. `--web` serves the same running session as a small, installable web
app — chat with every agent, approve or answer from your phone, watch the pulse feed — and
`--remote` reaches it from anywhere through a relay that is end-to-end encrypted and never sees a
plaintext byte. `--headless` runs either with no terminal at all.

- **`--web`: an installable web UI, on this machine or your LAN.** Optional `--web [ADDR:PORT]` /
  `--web-listen ADDR:PORT` (default `127.0.0.1:7777`, loopback needing no password); binding
  anything else refuses to start without one (`--web-password`, `MANTRA_WEB_PASSWORD` — preferred,
  flags show up in `ps` — or `[web] password`). The app is a PWA: installable from Chrome's
  *Install* prompt or iOS's Share › Add to Home Screen, offline shell caching, safe-area-aware
  layout, and a bottom tab bar on the phone / three columns on desktop, mirroring everything Solo
  and Mandala already do from the keyboard — send, queue, force-send, approvals, plan review,
  respawn, model/effort, compact, runs, land, diffs, the pulse feed. New `/web` overlay shows the
  listener address(es), TLS and password state, and connected browsers.
- **TLS with a certificate Mantra owns.** `--web-tls` makes a small private certificate authority
  once (`$MANTRA_HOME/web/tls/`) and signs a leaf for every name the machine currently answers to
  (localhost, its LAN IPv4, its hostname, plus `[web] sans`), reissuing the leaf — never the CA —
  only when that set changes or it is close to expiring; `/cert.pem` hands out the CA, so trusting
  it once on a device covers every certificate this Mantra issues afterwards, even across an IP
  change. `--web-cert FILE --web-key FILE` uses a certificate you already have.
- **Web Push, implemented by hand.** RFC 8291 payload encryption and a VAPID JWT (`web/push.rs`,
  no push-specific dependency — the relay's own `p256`/HKDF/AES-GCM cover it) for halts, questions,
  approvals, plan review, a finished run and a finished Solo turn, each togglable per device from
  Settings, with a "send a test" button. Notifications need HTTPS; a device kept on plain HTTP is
  told so once, plainly.
- **`--remote`: the same session from anywhere, end-to-end encrypted.** Dials out to a relay
  (`wss://remote.mantra.codes` by default) so nothing needs a port opened inward. Reached by a
  link (`https://remote.mantra.codes/s/<sid>#k=…` — the key lives only in the URL fragment, which
  a browser never sends to a server), a QR code, or a short code + password. No password set →
  Mantra generates a four-word one (`amber-kite-river-nine`, from a fixed 256-word list) and shows
  it in the new `/remote` overlay alongside the link, QR and connection state; `r` rotates the
  whole identity (old link/code/QR stop working at once). The relay only ever forwards
  AES-256-GCM-encrypted frames between the host and a browser — the key is derived from the
  password with PBKDF2 and never leaves the two ends, so the relay sees IPs and ciphertext, never a
  password, a key, or a byte of what an agent says. A wrong password or code fails to decrypt the
  first frame ("Wrong password or code") instead of connecting into someone else's session.
- **`--remote-site`, so the relay and the website need not be the same host.** The relay only
  forwards bytes; the page a remote link opens can be served from anywhere else, by anyone else.
  `--remote-site URL` (or `[web] remote_site`) names that site explicitly; left unset it defaults
  to the relay's own origin. Point `--remote` at a self-hosted relay and Mantra depends on
  `remote.mantra.codes` for nothing.
- **One hosted session per IPv4 address.** `remote.mantra.codes` allows one `--remote` session per
  source IPv4 address at a time (a relay policy, `--max-hosts-per-ip`): a second one dialling in
  from behind the same address is refused (HTTP 409) until the first disconnects, shown as a plain
  sentence rather than a raw status code, and only ever dialled over IPv4 in the first place (an
  IPv6-only relay address is rejected before it tries to connect, since the rule is keyed on the
  IPv4 address the relay observes).
- **`--headless`.** No terminal at all — no raw mode, no input thread, nothing drawn — for a
  machine reached only through the web UI or `--remote`; requires at least one of them ("nothing
  would be reachable" otherwise), a password whenever `--web` is on, and `ctrl+c`/`SIGTERM` shut
  it down cleanly. The one-time startup
  lines (URL, link, code, password) print to stderr, since there is no overlay to show them in.
  `event_loop` now takes an `Option<&mut Terminal<B>>`, so the terminal and headless paths are one
  function, not two to keep in sync.
- **`[web]` settings and `$MANTRA_HOME/web/`.** A new `WebSettings` (`listen`, `password`, `tls`,
  `cert`/`key`, `relay`, `remote_site`, `sans`, `push`, `contact`) mirrors every flag above — flags
  still win — and everything the web core needs on disk (TLS certificate, push keys and
  subscriptions, web sessions, the remote identity) lives under `$MANTRA_HOME/web/`, `0600`.
- **The reference relay stays out of this repository.** It is a separate deliverable (a Docker
  image), so this binary's compatibility with any relay is defined entirely by the wire protocol,
  not by shared code.
- **Roles & models from the web UI, with validated switches.** The web settings screen gets a
  per-role model picker, backed by the pattern file. `ctrl+k`/the web picker's switch is validated
  against the agent's actual backend first, so it is refused rather than sent to an agent that
  cannot run it — and whatever a model or effort switch, or a respawn, interrupts, nothing typed is
  lost: a message queued at the time is held and replayed once the agent is ready again.
- **A relaunch holds messages instead of failing them.** A self-inflicted restart (an effort/model
  change, `Cmd::Restart`, or a resumed Claude session found unusable) no longer counts as a crash —
  no backoff, no counters — and any turn or steer sent during it is held and replayed once the next
  process is `Ready`, instead of being thrown away.
- **Environment errors halt once, right away.** A command that dies in the sandbox itself (`bwrap`,
  user namespaces) is reported once and Mantra halts immediately instead of retrying blind or
  bouncing the failure between the orchestrator and the planner as if it were transient.
- **QA respects a read-only role.** A gate agent only edits when its role's sandbox allows writes —
  a read-only QA reports what is wrong (file, what, why) instead of trying to touch files it can't.
- **Empty projects skip exploration.** A fresh, empty project (nothing checked in yet) skips the
  planner's exploration step — there is nothing to read.
- **Sandbox fallback.** When this host can't create the user namespaces `bwrap` needs, Codex agents
  fall back to `danger-full-access` automatically (git worktrees still keep workers apart) after one
  up-front warning, instead of failing every command.
- **No user MCP servers.** A Codex agent Mantra spawns never picks up your own `~/.codex/config.toml`
  MCP servers — only Mantra's own tools reach it, so a personal MCP setup never changes what a team
  run can do.
- **ChatGPT-subscription default on a fresh install.** If Codex is already signed in with a ChatGPT
  subscription on a fresh install, every role defaults to a model that subscription can actually run
  (no `astra`, which needs API access) instead of failing on the ordinary Codex-account defaults.
- **Tests.** 50 new unit tests (183 total): PBKDF2/HKDF/AES-GCM known-answer and round-trip
  vectors including tamper detection and fragmentation, every WebSocket protocol message round-
  tripping, snapshot/delta diffing (upsert/remove, append-vs-full items, pulse by sequence number,
  plan by version, nothing-changed → no delta), login and its rate limiting, TLS certificate
  reissue rules, and the relay (identity persistence/rotation, IPv4-only dialling, the
  one-session-per-IPv4 refusal shown and retried, per-IP handshake throttling, a fake relay
  carrying a real browser handshake end to end).

## v0.4.0 — 2026-09-24

The manager: a role whose job is the whole run. Until now every agent owned a slice — a plan, a
phase, a task, a gate — and whatever none of them could clear went straight to the planner, or to
you. Plus two field fixes, and a second provider verified end to end.

- **The manager.** A new role kind, `manager`, named in the pattern by `flow.manager` (`""` = none;
  pattern files written before this release load unchanged, without one). The built-in
  `mantra-default` gains `[roles.manager]` (`◈`, blue, `sol` at high effort, read-only sandbox) and
  `flow.manager = "manager"`; `mantra-default-claude` puts it on `fable51` like the planner and the
  orchestrator. It is spawned when the first phase starts (after you approve the plan), briefed once
  with the goal, the plan's phases and the run's settings, and stays through every phase and the
  finale; `mantra runs resume` re-attaches it to its own thread; `r` respawns it like any agent. It
  never edits code: it steers the agents that do.
- **Chain of command: manager → planner → you.** *Gate exhausted* and *attempts exhausted* now go to
  the manager first, with the same facts the planner used to get. It has `watchdog_escalate_seconds`
  (240 s by default, never less than 120) to act, one reminder if it ends a turn without acting, and
  then the planner gets the escalation exactly as before. *Agent turn failed* (a planner, orchestrator,
  gate or finale turn that failed past its retries and its one free respawn), which used to stop for
  you at once, goes to the manager too — its move is `mantra_respawn` — and if it does nothing the
  band is yours, never the planner's (it may be the agent that is failing). Its tools: `mantra_status` (the whole run,
  for it), `mantra_log`, `mantra_journal` (new: the recent journal), `mantra_prompt` to any agent,
  `mantra_interrupt`, `mantra_set_effort`, `mantra_respawn` (new: restart any agent in place, with a
  note), `mantra_retry`, `mantra_pause_agents`/`mantra_resume_agents`, `mantra_brief_orchestrator`,
  `mantra_resume_run`, `mantra_ask` (→ the planner, for plan or gate-check changes; the planner
  answers with `mantra_prompt("manager", …)` or a `mantra_revise_plan`, which resumes the run),
  `mantra_ask_user` (only when nobody in the team can decide — the band reads *the manager asks: …*
  and your next message answers it) and `mantra_wait`. Restarting an agent during an escalated halt
  is the decision to go on: `mantra_retry`/`mantra_respawn` lift the halt, and an exhausted task gets
  one more attempt. A pattern without a manager behaves exactly as v0.3.1 did.
- **Watchdog: rung 3 wakes the manager.** After the nudge and the orchestrator rung, an agent still
  idle goes to the manager; if it has not reacted by `watchdog_escalate_seconds`, the planner is
  woken as before. A manager that sits on a case is itself on the ladder: nudge → respawn (the open
  escalation carried into the new brief) → the planner takes over what it held. If the manager's own
  turn fails past its retries it is dropped, never halting the run — whatever it held goes to the
  next rung (the planner, or you for a failed-turn halt) and a fresh manager comes with the next
  escalation or health digest.
- **Health digest.** `settings.manager_minutes` (default 5, `0` = escalations only, in the Studio):
  while building or in the finale — not while halted — the manager gets a run overview (stage, halt,
  plan progress, every non-worker agent with busy/idle-for and tokens, all workers, the last journal
  lines) and is asked to intervene only where something is off, otherwise `mantra_wait`.
- **On screen.** A manager mini-card at the top right of the stage, mirroring the planner on the
  left (from 90 columns; above the ad-hoc fixes during the finale), selectable with the arrows or
  `1`–`9`, zoomable, `@manager …` messages it. The Studio: `flow.manager` cycles through *none* and
  every role of kind manager, `manager_minutes` is a settings field, the flow panel shows the manager
  between the planner and the orchestrator, and the architect agent knows the kind. The journal
  reads `◈ escalated to the manager: …`.
- **Codex ≥ 0.150 and custom providers.** Codex no longer recognises the
  `model_supports_reasoning_summaries` config key, and Mantra passing it for custom-provider models
  put a *Codex is ignoring 1 unrecognized configuration setting* banner at the top of every agent
  log. Gone: Codex sends the reasoning effort to any model on its own now (a model without an
  `efforts` list still gets `model_reasoning_summary = "none"` and no effort).
- **`mantra doctor`'s login line.** It printed the first line of `codex login status`, which on some
  machines is a WARNING about PATH aliases rather than the verdict. It now prints the verdict — and
  when every configured model (the Solo default and the default pattern's roles) runs on custom
  providers, it says the Codex login is *not needed* instead of showing ✗.
- **OpenRouter, verified end to end.** `openai/gpt-5.6-luna` through Codex's Responses API
  (`base_url = "https://openrouter.ai/api/v1"`, `env_key = "OPENROUTER_API_KEY"`): Solo turns and
  full Mandala runs, plan → phases → gates → finale. The README's custom-provider section gains the
  paste-ready `[[provider]]` / `[[model]]` blocks.
- 133 unit tests (19 new: escalation to the manager first and to the planner when it does nothing,
  the deadline hand-over, halts lifted by `mantra_resume_run`/`mantra_retry`/`mantra_respawn`, the
  watchdog rungs around the manager, a failing manager dropped rather than halting, re-attachment on
  resume, `flow.manager` validation and old pattern files); the mock gains a scripted manager, so
  `--demo` shows it at work.

## v0.3.1

Four things a real run got wrong, fixed at the root. Driven by a second support bundle.

- **A hand stop sticks.** `ctrl+c` (or `x`) on an agent used to be undone within seconds: the
  interrupted turn read as "the planner ended without a plan", so Mantra nudged it; that nudge
  raced the process restart, so the transient-retry ladder sent it again; and a restart re-prompted
  "continue exactly where you left off". Three self-healing paths, each overriding the user. An
  agent stopped by hand is now marked as such — no nudge, no gate round, no retry, no resume
  prompt, and the watchdog leaves it alone — until someone messages it again, which is the one way
  to start it back up (`r` respawns it instead). The card and `mantra_status` say *stopped by you*,
  so the orchestrator stops reading it as a worker that merely went quiet.
- **You can see it is alive.** A Claude agent's thinking tick now carries its own running count, so
  the label reads `thinking · 9.4k` and visibly climbs instead of sitting at `thinking` for two
  minutes (captured from a live subscription session: the CLI ticks every 1–2s). When a turn
  really does go silent, both the status line and the stage card append an amber `· quiet 1m20s`,
  and a planner/orchestrator/gate agent that is connected but silent past `stall_minutes` gets one
  journal line (`⏳ … no output for 6m00s — still connected`) — workers keep the existing tripwire,
  which also wakes the orchestrator.
- **The context gauge is right from the first turn.** Claude Code announces the account's real
  window a second after spawn (`autocompact_state`, e.g. 980k), long before any `result` could.
  Mantra now takes it: a subscription login trusts it over the configured guess, an `api_key`
  gateway still keeps the window the user configured. A rate limit that is rejected or past 90% of
  a window raises one warning per episode rather than one per turn.
- **`mantra doctor` names the file.** `✗ claude: Permission denied (os error 13)` told nobody
  anything. The probe now resolves the command on `$PATH` itself and says which file is wrong and
  what to do: *is a directory — something else on your PATH shadows the real binary*, *is not
  executable — chmod +x …*, *is a broken symlink — reinstall the CLI*, or *`claude` not found on
  PATH*. `codex` gets the same treatment, and the same 5s timeout it was missing.
- **The Studio remembers where you were.** Leaving `/studio` or `/models` returned to the stage
  whenever a run existed, so a zoom — or Solo during a run — was lost every time. It now returns
  exactly where it was opened from, falling back to the old rule only when there is nothing to
  return to (or the remembered agent is gone).
- 114 unit tests (12 new), and `scripts/stress.sh` gains the hand stop end to end: ctrl+c during
  planning must leave the run stopped and un-nudged, and a typed message must start it again.

## v0.3.0

Coordination release: agents ask instead of guessing, halts travel up the chain of command before
they reach you, a revised plan restarts the phase it changed, and the orchestrator re-reads its
workers' work on a clock. Driven by a real run's support bundle (`support-bundle.sh`, new).

- **Chain of command.** Every worker, gate and finale agent has `mantra_ask`: a question goes to
  the orchestrator, who answers with `mantra_prompt` or passes it up with its own `mantra_ask` to
  the planner, who decides — or, only when the answer changes what is being built, asks you with
  `mantra_ask_user`. A question to you is a saffron band on the stage, not a halt: the run keeps
  going and your next message answers it. An agent that stops to wait is shown as *asked a
  question · waiting for the answer*, is never mistaken for "done", and the rung above it is the
  one the watchdog expects to act.
- **Halts escalate to the planner first.** *Gate exhausted* and *attempts exhausted* hand the
  planner the failing checks, their output and the last gate report, and exactly three ways out:
  `mantra_revise_plan` (the fix — the run resumes by itself), `mantra_resume_run(note)` (the plan
  is right; a hint for the stuck agent, and one more attempt), or `mantra_ask_user`. One reminder
  if it does nothing; then the band is yours. `r` on a task that exhausted its attempts resumes
  and retries in one go.
- **A revision restarts what it changed.** A task changed after its worker finished is re-opened
  (its old worker is cancelled, `mantra_spawn` accepts it again, the workspace keeps the earlier
  result); a phase already merging or gating goes back to building when tasks were added or
  changed; a gate that changed simply re-runs its checks. Nothing starts while the run is halted
  — `mantra_spawn`/`mantra_retry` say so instead of quietly re-running a phase you are sorting
  out. The identical failing check twice in a row escalates immediately instead of spending the
  remaining rounds while the gate agent keeps reporting "pass".
- **Periodic coherence review.** `settings.review_minutes` (default 3, `0` off, in the Studio):
  while workers build, the idle orchestrator gets each one's activity, files touched and recent
  log, and is asked to check the parallel work stays coherent with each other and the phase goal
  — steering only where something is off.
- **Claude Code backend.** The context gauge follows every API call's own usage (context = input
  + cache reads + cache writes), so it moves while a turn runs instead of staying at 0 and then
  showing "211k / 200k" from the turn's summed `result.usage`. A subscription login trusts the
  window the CLI reports for the account; an `api_key` gateway keeps the configured one. The
  CLI's once-a-second thinking ticks and tool progress count as activity, so a three-minute
  `pip install` never reads as an idle agent — and they no longer flood `mantra.log`. Default
  Claude models are now `opus46 opus48 opus5 sonnet5 fable5 fable51` at **1M** context and
  `haiku45` at 200k (the `[1m]` aliases are gone; an existing `models.toml` keeps its own
  `context_window` — set it to `1000000` or delete the file to regenerate).
- **Keys.** `ctrl+c` interrupts the focused agent's turn (or closes an overlay / clears the input);
  three presses within two seconds always quit, whatever state the app is in. `esc` only
  navigates — back to the overview from a zoom, otherwise clear the input — and never interrupts,
  so the two hints stop colliding in a zoomed Mandala agent.
- **Protocols and the default pattern.** Planner: keep the tooling later gates need until the last
  phase (cleanup belongs in the final phase), gate checks must run as-is on this machine. Gate:
  never report pass while a check fails — a check that cannot pass is a plan problem, say so with
  `mantra_ask`. Orchestrator: answer questions promptly, pass up what is above your brief, use the
  review. Worker: ask instead of guessing; end the turn without a STATUS line to wait.
- `support-bundle.sh`: one command that writes versions, `mantra doctor`, login state, terminal
  and sandbox facts, settings/models/patterns, the newest runs and the log tail into one file,
  with anything that looks like a key redacted. README gains *Reporting a problem*.
- `--demo` gains `MANTRA_MOCK_ASK=1` (a worker asks, the orchestrator answers), and the mock
  planner handles questions and escalations; 92 unit tests.

## v0.2.0

- WP1: bumped version to 0.2.0; removed the 7 dead-code warnings (`cargo build` is now clean); crash
  reasons and journal lines are ANSI-stripped and include the process exit code and the last
  non-warning stderr line; RPC errors during thread start keep their own message and append the
  stderr tail; simultaneous agent spawns are staggered by 300ms to avoid startup crash storms.
- WP2: context safety by default — Codex is always given an explicit `model_context_window` and
  `model_auto_compact_token_limit` for every agent (a conservative 200k / 85% assumed when a model
  doesn't set its own, shown dim as `(assumed)` / `(default)` in `/models`); the shipped built-in
  models (astra/sol/terra/luna) now ship honest explicit 272k / 85% defaults; a worker that still
  hits a context-full error while running on the assumed 200k gets that assumption halved for its
  next attempt, logged so you know to set the model's real context window.
- WP3: the Studio selects roles by name instead of by index into a list that re-sorts on every
  draw, so changing a role's `kind` (or anything else that moves it) never lands the highlight on a
  different role. Roles gain a `permission` field (`never` by default, `on-request`, `untrusted`)
  cycled with ←/→ next to `sandbox`; a Mandala role's own permission — not the global Solo approval
  mode — now decides whether its requests auto-resolve or land in the inbox.
- WP4: the model picker (ctrl+k) and the Studio's role `model` field now show which provider a
  model runs through (`via OpenAI (Codex)`, or a configured provider's name), flag aliases that
  share a model id through different providers, and let you bump or edit a model's context window
  right from the picker (`+`/`-`, `c`) without opening `/models`. The `/models` `t` live test now
  sends a developer-role message, so a provider that rejects it (as some third-party gateways do)
  fails the test before it fails a real run.
- WP5: providers can now hold an API key directly (`api_key`, alongside `env_key`) — stored
  `0600` on Unix, delivered to each agent's own process environment (never on argv, never logged),
  with a synthesized `MANTRA_<ID>_API_KEY` variable name when `env_key` is left empty; the
  `/models` providers table gained a masked `api_key` column and shows whether a key is set via
  the environment or stored in `models.toml`.
- WP6: `Run.paused` became a typed `Halt { reason, agent, message, since }` (`HaltReason::User |
  Auth | UsageLimit | ProviderRejected | Environment | GateExhausted | AttemptsExhausted |
  AgentTurnFailed`) with a reason-specific resume hint; the opaque `‖ PAUSED` badge is replaced by
  a full-width amber `⛔ halted · … · …` band under the stage header. A new `ErrKind::ProviderRejected`
  (HTTP 400/422, "unexpected message role", …) is never retried and halts immediately, naming the
  role, model alias and provider. `m` on a selected stage agent opens the model picker to fix a
  `ProviderRejected` halt and resumes the run once a new model is picked.
- WP7: a watchdog now makes sure every agent that should be working is working. `Run::expected_active`
  says who must be busy right now and why (planning, orchestrating, working a task, gating, a finale
  step); an idle agent gets an escalation ladder — nudge it (`watchdog_seconds`, default 90s), then
  respawn the orchestrator or wake it about another idle agent (`watchdog_escalate_seconds`, default
  240s), then wake the planner (2×), then halt (`AgentTurnFailed`) if the planner itself doesn't
  respond — journaled with `⏰`. `Run::respawn` can now restart any run agent in place (planner,
  orchestrator, phase gate, finale step, or a worker), reachable from the stage `r` key (not just
  crashed processes), `ctrl+r` anywhere, and `/respawn`. A planner/orchestrator/gate/finale turn that
  fails past its retry cap gets one free respawn before the run halts. Tool guards refuse
  `mantra_prompt`/`mantra_interrupt`/`mantra_retry`/`mantra_set_effort` on a worker whose task is
  already `Done` once the phase has moved past orchestrating ("wait for the handoff"), and the
  orchestrator prompting the same stuck idle worker three times in five minutes respawns it instead
  of relaying a fourth message into the void (the F3 loop from real runs). Also from the v0.1 field
  reports: a `commandExecution` whose output names `bwrap`/user namespaces now halts the run
  immediately with `HaltReason::Environment` and the sandbox fix hint instead of burning gate rounds
  or retries (L1); two consecutive gate reports blocked on the same thing halt with `GateExhausted`
  right away instead of spending the remaining rounds (L4). The Studio's settings panel exposes the
  new `watchdog_seconds`/`watchdog_escalate_seconds` pattern settings (old saved patterns still load
  — they default to 90/240 via serde).
- WP8: zoom vs overview are now unmistakable — a zoomed agent gets a solid role-coloured header
  band and a coloured spine down the log; a run starts zoomed into the planner (once it's spawned);
  the plan-review overlay now opens over any screen (Solo, Zoom or Stage), not just the overview;
  approving a plan always lands you back on the overview with the orchestrator selected; `1`-`9`
  jump straight to the nth stage node and zoom in; every Stage↔Zoom switch flashes the header
  briefly (skipped when `reduce_motion` is set).
- WP9: busy-agent chat gets Claude-Code-style queueing — `⏎` while an agent is mid-turn appends
  your message to a visible `⏳ queued N` chip above the input instead of steering immediately;
  `ctrl+f` force-delivers the queue plus the current input into the running turn right away;
  `backspace` on an empty input restores the last queued message for editing, `ctrl+x` discards
  the whole queue. Applies to Solo, Zoom and `@name` messages from the stage; the engine's own
  prompting (steering workers, waking the orchestrator, etc.) is unaffected.
- WP10: Claude Code is a second agent backend, for any role. A provider now has a `kind`
  (`codex` | `claude-code`) and, for Claude, an `auth` (`subscription` | `api_key`); when a
  `claude` binary is on `PATH` a built-in `claude` provider appears with `opus46`/`opus48`/`opus5`/
  `sonnet5`/`fable5`/`fable51` (+ `[1m]` variants). `hub::claude` runs one long-lived `claude -p
  --input-format stream-json --output-format stream-json --verbose --dangerously-skip-permissions
  --session-id|--resume … --model … [--effort] --autocompact … --append-system-prompt …` process
  per agent (every inherited `CLAUDE*`/`ANTHROPIC_*` variable stripped, `IS_SANDBOX=1` set under
  root, `--bare` + `ANTHROPIC_API_KEY`/`ANTHROPIC_BASE_URL` for `api_key` auth through a gateway)
  and translates its NDJSON stream into the Codex-shaped notifications the reducer already
  understands, so the engine and UI have no backend branches; steering lands at the next tool
  boundary, `x` sends a real `control_request` interrupt only while a turn is open, `/compact`
  is the literal `/compact` line, a crash restarts with `--resume`, and a bad key halts as `Auth`
  within seconds instead of the CLI's own ten retries. Mantra's tools reach Claude through MCP:
  `Hub` opens `$MANTRA_HOME/run/<pid>.sock`, each Claude agent gets `--strict-mcp-config
  --mcp-config` pointing at `mantra mcp-bridge --sock … --agent <id>` (spawned by `claude` itself),
  `tools/list` is answered from the role's own tool schemas and `tools/call` becomes the same
  `item/tool/call` request the Codex path produces. `mantra doctor` checks `claude --version` (5 s
  timeout) and each Claude provider's auth; `D` on a Claude provider lists the six defaults (plus a
  gateway's `/v1/models`); the Studio's providers grid gained `kind`/`auth` columns; the built-in
  `mantra-default-claude` pattern runs planner/orchestrator on `fable51` and workers on `sonnet5`
  with Codex gates; `mantra mock-claude` + `--demo --pattern mantra-default-claude` drive the whole
  bridge chain in `scripts/stress.sh`; `scripts/live-claude.sh` holds the paste-ready live tests.
- WP11: runs can be listed, resumed and deleted. `state.json` is now a typed snapshot (stage,
  workspace, workers with their worktrees/branches/reports, agents with their thread ids) rewritten
  atomically at every transition. `mantra runs` lists every run of every project (id, stage, when,
  goal); `mantra runs resume <id>` reopens one from any directory at the nearest safe boundary
  (planner re-attached to its thread, a fresh orchestrator briefed with the phase status, running
  workers re-attached when their worktree still exists — otherwise re-spawned — merges/checks/gates
  simply run again, finished runs open read-only for `/land`); `mantra runs delete <id>` removes
  its worktrees, `mantra/<id>` + `mantra-w/<id>/*` branches and journal after a `y/N` (`--yes`
  skips it). In the TUI, `/runs` opens the same list for the current project (`⏎` resume, `D`
  delete), and the welcome screens say `↻ N unfinished runs — /runs`. `--resume-last` reopens the
  most recent unfinished run (with `--demo`, the last demo run).
- WP12 (real-run robustness): a `ProviderRejected` halt caused by a rejected `developer` message
  now says so and points at running that model through Claude Code (`kind = claude-code`);
  discovery marks `*-thinking` model ids as reasoning-capable when the catalogue says nothing
  (LibertAI/OpenRouter shapes); the orchestrator protocol tells it never to prompt a finished
  worker and to answer events with at most one `mantra_prompt` (3 wasted prompts per phase were
  measured); Mantra probes the Linux sandbox once at startup (`unprivileged_userns_clone`,
  AppArmor, `unshare -U`) and, when it can't work, says so on both welcome screens and in every
  run's pulse, and `mantra run`/`runs resume` ask `y/N` first (`--no-sandbox-check` skips it).

- Docs: the README is rebuilt around real screenshots — every frame is captured from `--demo` by
  `docs/tools/shoot.py`, which drives the binary inside tmux and paints the captured cells into a
  PNG — plus a generated mandala logo (`docs/tools/logo.py`), a plain-language explanation and
  collapsible sections for the depth. Two things the screenshots exposed are fixed with them: the
  model picker labelled every model `◌ codex` regardless of its provider (it now reads `✧ claude`
  for Claude Code providers, and the picker uses the full terminal width when there is room), and
  a 10-character alias ran into the model id beside it. Added the MIT `LICENSE` file the manifest
  has always pointed at.
