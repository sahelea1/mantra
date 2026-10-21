# Bija — the seed harness

*bīja*: a seed; a seed mantra is the smallest mantra there is. Bija is Mantra's own agent harness: one
small, fast Rust binary that turns any OpenAI-compatible (or Anthropic-compatible) API into a working
coding agent — read/write/edit/delete files, run commands, wait on them — with the resilience and
the bookkeeping a swarm needs: every API error retried with backoff, every message written to disk as
it happens, a crashed process resumed from its transcript, a running session steerable and inspectable
from outside. It ships beside `mantra` and is selectable as a third backend (`kind = "bija"`) for any
role, or runs on its own.

Design goals, in order: **coherent and correct**, **resilient**, **small footprint** (one thread, a few
MB of RSS idle — 80 of them on a laptop), **fast** (streams, never blocks on disk in the hot path),
**resumable** (host crash → `bija --resume <id>` carries on), **inspectable** (`bija show`, a control
socket). Non-goals: a TUI (Mantra is the UI), MCP servers, web search, a plugin system.

This document is bija's design: the contract between its modules and between bija and Mantra. The
code follows it, and where the two disagree one of them has a bug; what it leaves open is kept simple.

---

## 1. Crate layout

```
mantra_src/
  Cargo.toml          # root package `mantra` + [workspace] members = ["bija"], default-members = [".", "bija"]
  bija/
    Cargo.toml        # package `bija`, bin `bija`, version = mantra's version
    SPEC.md           # this file
    src/
      main.rs         # tokio current_thread runtime; dispatches `cli::run()`
      cli.rs          # argv parsing (hand-rolled, like mantra), subcommands, help text
      types.rs        # shared vocabulary (Message, ToolCall, ToolDef, Usage, ApiError, …) and the helpers several modules use
      proto.rs        # the stdio protocol: `Op` (in) and `Event` (out) — the shapes Mantra speaks
      session.rs      # on-disk session: session.json + transcript.jsonl, replay/repair on resume
      agent.rs        # the turn loop: prompt → API → tools → … → turn_done; steer, interrupt, compaction, external tools
      control.rs      # optional Unix control socket (`bija send`)
      api/mod.rs      # `Client`: one completion with retries, wire dispatch
      api/http.rs     # minimal HTTP/1.1 client: http/https, proxy CONNECT, SSL_CERT_FILE, chunked, SSE
      api/openai.rs   # Chat Completions request/stream parsing
      api/anthropic.rs# Messages API request/stream parsing
      tools/mod.rs    # `Runtime`: tool defs, dispatch, sandbox policy
      tools/fs.rs     # read_file, write_file, edit_file, delete, list_dir, glob, grep
      tools/shell.rs  # bash (+timeout, +background jobs), job
      tools/sandbox.rs# path policy + optional bwrap wrapper
    tests/
      e2e.rs          # spawns the real binary against an in-test mock Chat Completions server
```

Dependencies: only crates already in Mantra's lockfile — `tokio` (rt, macros, process, io-util, io-std,
sync, time, net, fs, signal), `serde`, `serde_json`, `rustls`, `tokio-rustls`, `webpki-roots`, `ring`
(random bytes), `libc` (killpg). No reqwest/hyper, no regex, no clap. Release profile is inherited from
the workspace root (`strip`, thin LTO).

---

## 2. CLI

```
bija [OPTIONS]                       serve the stdio protocol (§4) — what Mantra runs
bija run [OPTIONS] <prompt>          one turn on a (new or resumed) session; prints the final text; exit 0/1
bija sessions [--home DIR]           list sessions (id, name, model, cwd, turns, last activity)
bija show <id> [--home DIR] [-f]     print a session's transcript, human-readable; -f follows (tail)
bija send <id> <text> | --json <op>  deliver an op to a running session over its control socket
bija --version | --help
```

Options (serve and run):

| flag | meaning | default |
|---|---|---|
| `--home DIR` | where sessions live (`DIR/sessions/<id>/`) | `$BIJA_HOME` or `~/.bija` |
| `--session ID` | create this session id (fails if it exists, unless `--resume`) | random uuid v4 |
| `--resume ID` | reopen an existing session and continue it (§5.3) | — |
| `--cwd DIR`, `-C` | the agent's working directory | current dir |
| `--provider-url URL` | API root, e.g. `https://api.openai.com/v1`, `http://127.0.0.1:11434/v1`, `https://api.anthropic.com` | required (or `BIJA_PROVIDER_URL`) |
| `--wire openai-chat\|anthropic` | which API the URL speaks | `openai-chat` |
| `--api-key-env NAME` | env var holding the key; the key itself never touches argv | `BIJA_API_KEY` |
| `--model ID` | model id sent to the provider | required (or `BIJA_MODEL`) |
| `--effort LEVEL` | `low\|medium\|high\|xhigh\|max`; `""`/`none` sends no reasoning field | none |
| `--context-window N` | the window to manage against | 200000 |
| `--compact-at PCT` | auto-compact when the last request's input tokens exceed PCT of the window | 85 |
| `--max-output-tokens N` | `max_tokens` per reply | the provider's; 16384 on `anthropic`, which requires one |
| `--system TEXT` / `--system-file PATH` | system prompt appended after bija's own preamble | none |
| `--tools-file PATH` | JSON array of external tool definitions (§4.4) | none |
| `--sandbox MODE` | `read-only\|workspace-write\|full` (§6.3) | `workspace-write` |
| `--writable DIR` | extra writable root (repeatable) | — |
| `--network` | allow network inside sandboxed commands (only matters with bwrap) | off |
| `--no-bwrap` | never wrap commands in bubblewrap | auto-detect |
| `--max-retries N` | retries per API call (§7.2) | 8 |
| `--timeout-secs N` | time budget of one request (connect + stream); each retry gets its own | 600 |
| `--no-stream` | non-streaming requests (for gateways that break SSE) | stream |
| `--name TEXT` | display name stored in session.json | — |
| `--control` | open `<session dir>/control.sock` (§4.5) | off in serve mode unless given; `run` never |
| `--fsync MODE` | `turn\|always\|never` — when transcript appends are fsynced | `turn` |
| `-v`, `--verbose` | `run` only: print the events to stderr | off |

`--resume` reuses the stored meta (model, wire, url, system prompt, sandbox…); flags given alongside
override and are saved. `BIJA_PROVIDER_URL`, `BIJA_MODEL`, `BIJA_HOME` are the only env inputs
besides the key variable.

Exit codes: 0 clean; 1 runtime failure (printed to stderr, one line); 2 usage error; 128 + N after
signal N's crash-safe stop (143 SIGTERM, 129 SIGHUP — §3).

---

## 3. The turn loop (agent.rs)

State: `Session` (messages + meta), `api::Client`, `tools::Runtime`, a queue of pending prompts, a
`steer` buffer, the turn number, and a phase (idle, in a turn, compacting while idle, running a `shell`
op) — anything but idle is "busy".

```
on Op::Prompt(text):  if busy → push to queue; else run_turn(text)
run_turn(text):
  session.append(User{text}); emit TurnStarted{turn}
  rounds = 0
  loop:
    if steer buffer non-empty: append each as User{text} (emit nothing — Mantra already echoed it); clear
    if estimated_input() > window*compact_at/100: compact()
    resp = client.complete(Request{model, effort, messages, tools = runtime.defs() + external, …})
           with on_delta → emit TextDelta / ReasoningDelta; on_retry → emit Retry
           (a retry after streamed output moves the call to fresh stream ids, §4.2)
    on Err(e):
       if e.kind == Context && !compacted_this_round: compact(); continue   (once per round)
       else: finish_turn(Failed, e); return
    session.append(resp.message); emit Message{id, text} for the text (if non-empty); emit Usage
    if resp.message.tool_calls is empty:
       if a steer is waiting: continue (it gets an answer); else finish_turn(Completed); return
    rounds += 1; if rounds > 200: finish_turn(Failed{Internal, "tool round cap"}); return
    for call in tool_calls (in order, one at a time):
       if runtime.is_builtin(call.name):
            emit ToolStarted; outcome = runtime.call(name, args, progress→ToolOutput)
            session.append(Tool{call_id, name, text: outcome.text, is_error: !outcome.ok}); emit ToolDone
       else (external, i.e. in --tools-file):
            emit ToolCall{id, name, args}; wait for Op::ToolResult{id} (keep serving other ops meanwhile;
            Interrupt aborts the wait with an "interrupted" result and ends the turn)
            session.append(Tool{…}); emit ToolDone
       unknown name: Tool{…, text: "unknown tool <name>; available: …", is_error: true}
finish_turn(status, error?):
  answer every call still without a result (repair); append steers that never reached a model call;
  append Note{"turn interrupted"} if interrupted; session.end_turn(status) — the `turn` record and the
  meta, fsynced unless --fsync never — then busy = false; emit TurnDone{turn, status, error}
  if queue non-empty: run_turn(queue.pop_front())
```

Rules:

- **Steer** (`Op::Steer`) while busy = appended at the next tool boundary (before the next API call);
  while idle = a prompt. **Prompt** while busy = queued for after this turn.
- **Interrupt**: drop the in-flight API future (cancel-safe), kill the running tool's process group,
  append `Note{"turn interrupted"}`, repair the transcript (every tool_call without a result gets a
  `Tool{text:"interrupted", is_error:true}`), finish the turn with status `interrupted`. The queue is
  kept. An interrupt while idle is a no-op. Ops that arrived while busy and were not taken by the wait
  (an interrupt racing the turn's last completion) are answered as idle ops before a queued prompt
  starts — so such an interrupt is that no-op, not the end of the next turn.
- **Compact** (`Op::Compact`): idle → compact now; busy → compact at the end of the turn. The
  automatic compaction is the same procedure: call the API once (no tools, effort none) with the
  history plus a user message asking for a hand-off summary for its future self (goal, done, in
  progress, key paths and decisions, open problems, what to do next — concrete); replace `messages` with
  `[User{"[context summary — the conversation so far was compacted]\n<summary>"}]`; `session.compact(…)`
  records it; emit `Compacted{from, to}` where `from` is the last known input tokens and `to` the summary's
  estimated size. The system prompt is never part of `messages` (it is in meta). The request is cut to
  fit before it is sent: tool outputs (4 KiB), call argument strings (2 KiB) and long texts (16 KiB) are
  clipped to their head and tail, and the oldest messages after the first prompt are left out (with a
  note) until the estimate is under 80 % of the window less the system prompt and the reply limit. When
  the provider still says Context, the budget is halved and it is asked again (twice); after that the
  history is cut mechanically — the first prompt and the last reply become the summary, with a `Warning`
  — so a session is never left with a history too big to compact. A compaction that fails otherwise
  (API error) is logged as a `Warning` and the turn goes on uncompacted (the next round tries again only
  if the API said Context). An idle compaction cut short by an interrupt or a shutdown emits the
  `Warning` "compaction interrupted".
- **Set** (`Op::Set`): `model`, `effort`, `name` take effect at the next API call; persisted in meta.
- **Shell** (`Op::Shell`): runs the command through the runtime (no sandbox restriction, `full`),
  emits ToolStarted/ToolOutput/ToolDone with `name:"bash"` and an id `shell-<n>`, and appends
  `Note{"$ <cmd>\n<output>"}` so the model sees it. Allowed while idle only; while busy answered with
  `Error{"busy"}`.
- **Status** (`Op::Status`): emit `Status{…}` (§4.2).
- **Shutdown** (`Op::Shutdown`, stdin EOF): a running turn is interrupted first (transcript
  repaired), so the next `--resume` finds nothing pending. Then durable first: the transcript is fsynced
  and the meta saved before anything else, because Mantra's quit path SIGKILLs bija a moment after
  `shutdown`. Only then are background jobs and leftover command groups ended — all signalled at once,
  SIGTERM, a 1 s grace, SIGKILL — and `exit` is the last line; the socket is removed, exit code 0.
- **Signals** (SIGTERM, SIGHUP): a *crash-safe stop*, not a shutdown. Mantra ends bija only with the
  `shutdown` op, so a signal is something outside wanting the process gone (a user's `kill`, a
  supervisor, a container stop) — not the work. The in-flight API call is dropped and the running
  command's group killed as for an interrupt, but the turn is **not** ended: no repair, no
  "turn interrupted" note, no `turn` record, no `turn_done`. The transcript ends exactly as a SIGKILL
  would leave it (the assistant message with its open tool calls, the tool results so far, or the user
  message), so the next `--resume` reports `pending: true` and finishes the turn, the cut-off calls
  answered as §5.3 describes. Steers not yet delivered are dropped, as with a SIGKILL. Then the same
  durable-first exit as a shutdown (transcript fsynced, meta saved, jobs and leftover groups ended),
  `{"ev":"exit","code":128+N}` as the last line and exit code 128 + N (143 for SIGTERM, 129 for SIGHUP),
  so the parent sees an exit that nobody asked for (stderr says `bija: terminated by a signal`). Between turns there is nothing to leave open: the
  same stop, nothing pending afterwards. In `bija run` the code is the same and stderr says how to
  resume.
- Every op is answered within one event loop iteration — reading stdin never waits on the API or a tool.
  Implementation: `tokio::select!` over `ops.recv()`, the pinned completion future, and the running tool.
- Token estimate when the API gave no usage: `chars / 4` over what was sent (system prompt, tool
  definitions, the messages without their reasoning) and what came back, flagged `estimated: true`. The
  api layer makes it, so the agent never sees an unexplained zero.
- The final text of a turn = the text of the last assistant message of that turn (may be empty). It is
  the agent's report to whoever prompted it (Mantra's Conductor reads a worker's `STATUS:` line from it);
  a failed turn reports the error instead, through `turn_done`.

Preamble (bija's own part of the system prompt, before `--system`): who it is ("You are a coding agent
running in bija, a minimal harness."), the cwd, the sandbox mode in one line, how tools work (edit_file
needs an exact unique match; bash output is truncated to the tail; use background jobs for long
commands), and `runtime.system_notes()`. It stays under 25 lines.

---

## 4. The stdio protocol (proto.rs)

NDJSON both ways. stdin: one `Op` per line; its EOF means the parent is gone, which is a shutdown.
stdout: one `Event` per line, nothing else ever (logs go to stderr, prefixed `bija: `). Unknown fields
are ignored; an unparsable line produces `Event::Error` and is otherwise ignored. Every event carries no
envelope beyond `ev`. SIGTERM and SIGHUP are a crash-safe stop that leaves a running turn open for
`--resume` (§3); SIGINT is a shutdown in serve mode and an interrupt for `bija run`.

### 4.1 Ops (stdin → bija)

```json
{"op":"prompt","text":"…"}
{"op":"steer","text":"…"}
{"op":"interrupt"}
{"op":"compact"}
{"op":"set","model":"…"}            // any subset of: model, effort, name
{"op":"shell","command":"…"}
{"op":"tool_result","id":"<call id>","ok":true,"text":"…"}
{"op":"status"}
{"op":"shutdown"}
```

### 4.2 Events (bija → stdout)

```json
{"ev":"ready","session":"<id>","dir":"<session dir>","model":"…","wire":"openai_chat","resumed":false,"pid":1234,"pending":false}
{"ev":"turn_started","turn":"t3"}
{"ev":"text_delta","turn":"t3","id":"t3-m1","text":"…"}                  // streamed assistant text
{"ev":"reasoning_delta","turn":"t3","id":"t3-r1","text":"…"}             // when the API streams reasoning
{"ev":"message","turn":"t3","id":"t3-m1","text":"<full text>"}            // one per assistant message with non-empty text
{"ev":"tool_started","turn":"t3","id":"<call id>","name":"bash","args":{…}}
{"ev":"tool_output","turn":"t3","id":"<call id>","text":"<chunk>"}        // streamed command output
{"ev":"tool_done","turn":"t3","id":"<call id>","name":"bash","ok":true,"result":"<text the model got>","exit_code":0,"duration_ms":812,"changes":[{"path":"src/a.rs","kind":"modified","diff":"--- …"}]}
{"ev":"tool_call","turn":"t3","id":"<call id>","name":"mantra_spawn","args":{…}}   // external: answer with tool_result
{"ev":"usage","turn":"t3","last_in":12345,"last_out":210,"total":98765,"cached":40000,"window":200000,"estimated":false}
{"ev":"retry","turn":"t3","attempt":2,"max":8,"error":"HTTP 503 …","wait_ms":2000}
{"ev":"compacted","turn":"t3","from":171000,"to":4100}                      // turn absent for an idle compaction
{"ev":"turn_done","turn":"t3","status":"completed","error":null}
{"ev":"turn_done","turn":"t3","status":"failed","error":{"kind":"transient","status":503,"message":"…"}}
{"ev":"warning","message":"…"}
{"ev":"status","session":"…","busy":true,"turn":"t3","turns":3,"model":"…","effort":"high","messages":41,"last_in":12345,"total":98765,"cached":…,"window":200000,"queued":0,"jobs":1}
{"ev":"error","message":"bad op: …"}
{"ev":"exit","code":0}                                                     // last line before exiting (143/129 after a signal)
```

`turn_done.status` ∈ `completed | failed | interrupted`. `error.kind` ∈ `transient | rate_limit | auth |
context | bad_request | usage | interrupted | internal` (`types::ErrorKind`). `tool_done.changes[].kind`
∈ `add | modified | delete`. Ids: turns `t<N>` (N counts from 1 across resumes), messages `t<N>-m<K>`,
reasoning `t<N>-r<K>`; tool ids are the provider's call ids (fallback `t<N>-c<K>`). K counts the turn's
model calls from 1, and the `message` that ends a call carries the id its deltas went out under. A call
that is retried after it already streamed text or reasoning moves on to the next K: the retry streams
under fresh ids, and the abandoned ones never get a `message` — what they carried was not the reply, so
a consumer drops (or closes, without its partial text) a streamed item that a `retry` left without its
`message`.

### 4.3 Ready

The first line, always. `pending: true` means the resumed transcript ended inside a turn (a user message
or tool results without a final assistant text) and bija is continuing that turn by itself right now
(a `turn_started` follows immediately). `resumed` is whether `--resume` was used.

### 4.4 External tools

`--tools-file` holds a JSON array in Mantra's dynamic-tool shape
`{"type":"function","name":"…","description":"…","inputSchema":{…}}` (`parameters` accepted as an alias
of `inputSchema`). They are offered to the model beside the built-ins; a call to one is emitted as
`tool_call` and the turn waits for `tool_result` with the same id. A result arriving for an unknown id
is ignored with a `warning`.

### 4.5 Control socket

With `--control`, bija listens on `<session dir>/control.sock`; each connection may send ops (same
NDJSON) and receives every event the session emits from then on (fan-out alongside stdout), one per
line, until it disconnects. `bija send <id> <text>` connects, sends `{"op":"prompt"}`, prints events
until `turn_done`, exits. `bija send <id> --json '{…}'` sends one op and exits after the first event
(or immediately for `shutdown`). The socket file is removed on exit; a stale one is replaced at start.
Each client's event queue is bounded (1024 lines): a client that stays connected but stops reading is
dropped once it is full, rather than growing bija's memory.

---

## 5. Sessions on disk (session.rs)

```
<home>/sessions/<id>/
  session.json       meta, rewritten atomically (tmp + rename) whenever it changes
  transcript.jsonl   append-only records (below)
  control.sock       only while --control is running
  jobs/<n>.log       background job output
```

### 5.1 session.json

```json
{"id":"…","name":"…","created_unix":…,"updated_unix":…,"cwd":"/abs","provider_url":"…","wire":"openai_chat",
 "api_key_env":"BIJA_API_KEY","model":"…","effort":"high","context_window":200000,"compact_at":85,
 "max_output_tokens":16384,"system":"<full system prompt incl. preamble>","tools_file":"<abs path or null>",
 "sandbox":"workspace-write","writable":["…"],"network":false,"turns":3,"usage_total":{"input":…,"output":…,"cached":…},
 "last_in":12345,"archived":false,"version":1}
```

### 5.2 transcript.jsonl records

```json
{"t":"msg","role":"user","text":"…","at":<unix>}
{"t":"msg","role":"assistant","text":"…","reasoning":"…","tool_calls":[{"id":"…","name":"…","args":{…}}],"at":…}
{"t":"msg","role":"tool","call_id":"…","name":"…","text":"…","is_error":false,"at":…}
{"t":"compact","summary":"…","from":171000,"to":4100,"at":…}   // everything before this line is history
{"t":"note","text":"turn interrupted","at":…}                   // becomes a User message "[note] …" for the model
{"t":"turn","n":3,"status":"completed","at":…}                  // written by finish_turn
```

Records are `types::Message` plus the `t`/`at` fields (serde: `Message` is `#[serde(tag="role")]`, the
transcript wraps it). Each record is one `write` of one whole line, so a crash tears at most the last
line. With `--fsync turn` the file is fsynced at the end of each turn (`end_turn`), after a compaction
and at shutdown; with `always` also after every assistant and tool record; with `never` never.

### 5.3 Resume and repair

`Session::open` reads `session.json`, then replays `transcript.jsonl` from the **last** `compact`
record (its summary becomes the first User message) to the end. A line that fails to parse is skipped
with a stderr warning (a torn last line after a crash is normal). Then the tail is repaired so the
message list is always valid for the API:

- an assistant message whose tool_calls lack results → the missing results are appended as
  `Tool{text:"interrupted: the process was restarted before this tool finished", is_error:true}` and the
  turn is **pending** (bija continues it);
- a trailing User message with no assistant after it → pending (bija calls the API for it);
- a trailing assistant message with text and no tool_calls → not pending (the turn had finished; `turn`
  records are informational).

`turns` continues from meta. The repaired records are appended to the transcript (so the file is as
consistent as memory).

---

## 6. Tools (tools/)

### 6.1 Built-in definitions (`Runtime::defs()`)

| name | args | notes |
|---|---|---|
| `read_file` | `path`, `offset?` (1-based line), `limit?` (lines) | returns `cat -n` style numbered lines; default limit 2000 lines / 64 KiB, says when truncated (`of ≥N` lines when counting the rest would scan more than 8 MiB); binary → size only |
| `write_file` | `path`, `content` | creates parent dirs; change kind `add` or `modified` |
| `edit_file` | `path`, `old`, `new`, `replace_all?` | exact substring; refuses with a clear message when 0 or (without replace_all) >1 matches |
| `delete` | `path`, `recursive?` | files, empty dirs; dirs need `recursive: true` |
| `list_dir` | `path?`, `depth?` (default 1, max 4) | `name/` for dirs, sizes for files, `name -> target` for links; `.git`, `target` and `node_modules` are listed but never expanded; stops at 1000 entries |
| `glob` | `pattern`, `path?` | `*`, `**`, `?`, `[abc]`, `{a,b}`; max 500 results; skips `.git`, and enters `target`/`node_modules` only when the pattern names them |
| `grep` | `pattern`, `path?`, `glob?`, `max?` (default 200, max 2000) | shells out to `rg` (preferred, honours `.gitignore`) or `grep -rnHEI`; skips `.git`, `target`, `node_modules` (unless `path` names one) and binary files; results `path:line: text` |
| `bash` | `command`, `timeout_secs?` (default 120, max 1800), `background?` | `bash -c` in cwd (`sh -c` when there is no `bash`); stdin closed; merged stdout+stderr; tail-truncated at 48 KiB with a `[… N lines truncated]` head note; `background: true` returns `{"job": n, "log": "<path>"}` at once |
| `job` | `action` ∈ `wait\|output\|kill\|list`, `id?`, `timeout_secs?` | `wait` blocks up to timeout (default 300) and returns exit code + output tail; `output` returns the tail so far. The tail reads at most the last 96 KiB of the log (its note then counts bytes, not lines) |

Argument validation errors are returned as `ok:false` text results for the model ("edit_file: `old`
is required"), never as a crashed turn. Paths are resolved against cwd; `~` is not expanded.

Before any call runs — built-in or external — its arguments are shaped to the tool's schema
(`tools/args.rs`): where the schema names one type (`properties`, `items`, `anyOf`/`oneOf` branches
that agree), a string holding JSON of exactly that object or array type is parsed, a numeric string
becomes the integer/number, `"true"`/`"false"` the boolean, and a number or boolean wanted as a
string is written out. Nothing else changes, and a value that does not convert exactly stays as
sent. The transcript and the history sent back keep the arguments as the model wrote them; the
tool, its `tool_started` and its `tool_call` event get the shaped ones.

### 6.2 Outcomes

`ToolOutcome { ok, text, exit_code, duration_ms, changes }`. `changes` carries one entry per touched
file for write/edit/delete, with a unified diff from `diff -u <old> <new>` (empty when `diff` is not
installed; for `add` the diff is the file's content, for `delete` empty).

### 6.3 Sandbox (tools/sandbox.rs)

- `read-only`: `write_file`/`edit_file`/`delete` are **not offered** (left out of `defs()`) and refused
  if called by name anyway; bash runs (tests must run) but under bwrap with no writable binds except
  `/tmp` and `/dev/shm`, when bwrap works.
- `workspace-write`: writes allowed under cwd, each `--writable` root, `/tmp`, `$TMPDIR`; a write
  outside is refused with the path and the allowed roots. A path is judged where the bytes would land:
  resolved component by component, every symlink followed — a dangling one too, since a write through
  it creates its target — and `..` taken from where the walk really is; a symlink loop is refused. The
  file tools then write that resolved path, opened with `O_NOFOLLOW`. `delete` never removes a root or
  a directory holding one (`..` from a workspace nested in `/tmp` included). bash runs under bwrap with
  exactly those roots writable (plus `$HOME/.cache`, `$HOME/.cargo`, `$HOME/.npm`, `$HOME/.rustup` when
  they exist — the toolchain caches Codex also opens); the network namespace is shared only with
  `--network`.
- `full`: no path policy, no bwrap.
- bwrap: Linux only, used when `bwrap` is on PATH, `--no-bwrap` not given, and a one-time probe
  (`bwrap --ro-bind / / --dev /dev --proc /proc --unshare-pid true`, 5 s timeout) succeeds; otherwise
  commands run plainly and `system_notes()` says so (`Warning` event once at start: "bwrap unavailable —
  commands run unsandboxed"). The wrapper: `bwrap --ro-bind / / --bind <root> <root>… --dev /dev
  --proc /proc --bind /dev/shm /dev/shm --chdir <cwd> --unshare-pid --die-with-parent --new-session
  [--unshare-net] -- bash -c <cmd>`. A mount hides what earlier ones put beneath it, so the order is the
  design: the read-only root; the writable roots over it, ancestors first (`/tmp` is one); then a fresh
  `/dev` and `/proc` over those — a root that holds them (`--cwd /`) must not bring back the host's
  devices and processes; last the writable places inside `/dev`. `--new-session` puts the command in a
  session (and group) of its own, which a `killpg` of bwrap's group never reaches; `--unshare-pid` makes
  the pid namespace the unit instead — bwrap dies of the SIGTERM, `--die-with-parent` then kills the
  namespace's init, and the kernel kills everything in it (also when bija itself is SIGKILLed). The
  sandbox tests check all of this against a real bwrap where one is installed.
- Every command runs in its own process group (`process_group(0)`); a timeout, an interrupt and
  `job kill` end the group with `SIGTERM`, a 2 s grace, then `libc::killpg(SIGKILL)`. A group that may
  still have members afterwards (a command cut short, or one that exited leaving `server &` behind) is
  remembered and ended with the background jobs at shutdown — all at once, with a 1 s grace (§3).
- The variable named by `--api-key-env` (and `BIJA_API_KEY`) is removed from every command's
  environment — the model's, a job's, the user's `shell` op — so no command can print the key into the
  transcript.

Mantra's worktree isolation is the real boundary between workers; the sandbox is belt and braces,
as it is for Codex.

---

## 7. API layer (api/)

### 7.1 Client

```rust
pub struct ClientConfig { base_url: String, wire: Wire, api_key: Option<String>, max_retries: u32,
                          timeout: Duration, stream: bool, user_agent: String }
impl Client {
    pub fn new(cfg: ClientConfig) -> Client;
    /// One completion. Retries transient failures inside (backoff below), calling `on_retry` before
    /// each wait. Streams pieces through `on_delta`. Cancel-safe: dropping the future aborts the
    /// request and discards the connection.
    pub async fn complete(&mut self, req: &Request<'_>, on_delta: &mut dyn FnMut(Delta), on_retry: &mut dyn FnMut(&RetryNote)) -> Result<Response, ApiError>;
}
```

### 7.2 Retry policy

Retry (attempt n of `max_retries`, wait `min(60s, 1s·2^n)` ± 25 % jitter, or `Retry-After` when
given, capped at 120 s): connect/DNS/TLS errors, timeouts, EOF or broken stream mid-response, HTTP
408 409 425 429 500 502 503 504 529, an SSE `error` event whose message looks transient ("overloaded",
"rate", "timeout", "temporarily", "try again", "upstream"), an empty completion (no text, no tool calls,
no usage). Never retry: 400 (→ `Context` when the message says context/too long/maximum … tokens, else
`BadRequest`), 401/403 (`Auth`), 402 (`Usage`; a 400/429 about quota, billing or credit too — waiting
will not refill it), 404 (`BadRequest`: "model or endpoint not found at …"), 413 (`Context`), 422
(`BadRequest`). After the last attempt the error is returned with the last status and message,
`message` prefixed by `"after N attempts: "`.

Besides retries there is *adaptation*: when a 400 names an optional part of the request (§7.3, §7.4),
the wire switches that part off for the rest of the session and the request is sent again at once — no
wait, no `retry` event. Each adaptation turns something off for good, so they are bounded.

Timeouts: connect 20 s; total per attempt `--timeout-secs`; idle (no bytes for 180 s on a stream) → treated
as a broken stream (retry).

### 7.3 Wire: openai-chat (`POST {base}/chat/completions`)

Request: `model`, `messages` (system → `{"role":"system"}`; assistant tool calls →
`tool_calls:[{id,type:"function",function:{name,arguments:<string>}}]`; tool → `{"role":"tool",
"tool_call_id","content"}`; reasoning never sent back), `tools:[{"type":"function","function":{name,
description,parameters}}]` when non-empty, `stream:true` + `stream_options:{include_usage:true}`,
`reasoning_effort` when effort non-empty, `max_tokens` only when `--max-output-tokens` was given
explicitly (providers' defaults otherwise). Headers: `Authorization: Bearer <key>` when a key exists,
`Content-Type: application/json`, `Accept: text/event-stream`, `User-Agent: bija/<ver>`. Adaptations
(§7.2): `stream_options` dropped (streaming then goes without usage), `reasoning_effort` dropped,
`max_tokens` renamed `max_completion_tokens`, `content: null` on tool-call turns sent as `""`.

Stream: `data: <json>` lines, `[DONE]` ends; `choices[0].delta.content` → `Delta::Text`,
`delta.reasoning_content` or `delta.reasoning` → `Delta::Reasoning`, `delta.tool_calls[]` by `index`
(`id`+`function.name` start a call; `function.arguments` chunks are concatenated and parsed at the end
— unparsable → `args: Null`, `raw` kept; the agent then returns an error result to the model),
`usage` from any chunk that has it (`prompt_tokens`, `completion_tokens`,
`prompt_tokens_details.cached_tokens`), `finish_reason`. A `{"error":{…}}` object in a chunk or as the
non-200 body → `ApiError` classified by status/message. Non-streaming (`--no-stream`): the same from
`choices[0].message`.

### 7.4 Wire: anthropic (`POST {base}/v1/messages`; `{base}` already ending in `/v1` → `{base}/messages`)

Headers: `x-api-key`, `anthropic-version: 2023-06-01`. Request: `model`, `max_tokens`, `system`
(string), `messages` (user/assistant alternate — consecutive same-role messages are merged; tool
results are `user` messages with `tool_result` blocks; assistant tool calls are `tool_use` blocks),
`tools:[{name,description,input_schema}]`, `stream:true`, and automatic prompt caching
(`cache_control:{type:"ephemeral"}` at the top level — an agent loop resends its whole history).
Reasoning, when effort is set, is a ladder that starts with what current models require and steps down
one rung each time the provider refuses it (an adaptation, §7.2): `thinking:{type:"adaptive",
display:"summarized"}` with `output_config:{effort}` (`display` alone is dropped if that is what is
refused); then `thinking:{type:"enabled",budget_tokens}` for effort low 2048 / medium 8192 / high 16384
/ xhigh,max 32768; then `output_config.effort` alone; then nothing. `max_tokens` is raised to at least
the effort's budget + 4096 unless the provider caps it lower. Thinking blocks come back with their
signature and are replayed unmodified, in place, when their assistant message is sent again (dropped
once, all of them, if the provider refuses a signature). Stream events: `message_start` (usage input,
`cache_read_input_tokens`, `cache_creation_input_tokens` → input includes both, cached = cache_read),
`content_block_start` (`text` / `tool_use{id,name}` / `thinking`), `content_block_delta` (`text_delta`,
`input_json_delta`, `thinking_delta`), `message_delta` (`stop_reason`, `usage.output_tokens`), `error`
events, `message_stop`.

### 7.5 http.rs

A minimal HTTP/1.1 client over `tokio::net::TcpStream` + `tokio_rustls` (roots: webpki + `SSL_CERT_FILE`
PEM when set — same as mantra's `web/remote.rs::client_tls`). `http://` plain. Proxies, scheme by scheme:
`HTTPS_PROXY` (then `https_proxy`) for https targets, `HTTP_PROXY` (then `http_proxy`) for http ones,
both tunnelled with `CONNECT` (basic auth from the proxy URL if present); a proxy variable that does not
parse is an error, never a silent direct connection, and no error repeats its credentials.
`NO_PROXY`/`no_proxy`: exact hosts, `.domain`/`*.domain`, `*`, CIDR ranges; loopback and `localhost`
are never proxied. Request body sent with
`Content-Length`; response: status line, headers (case-insensitive), `Transfer-Encoding: chunked` and
`Content-Length` bodies, `Connection: close`. Yields the body as a stream of byte chunks; an SSE
splitter turns chunks into events (`data:` lines joined by `\n`, blank line ends an event, `:` comments
ignored, CRLF tolerated). One idle keep-alive connection is kept per `Client` and reused when the
server didn't say `Connection: close`; any error discards it. No gzip (`Accept-Encoding: identity`).

---

## 8. Resource rules

- `tokio::runtime::Builder::new_current_thread()`. An fsync in the middle of a turn goes through
  `spawn_blocking` (the pool is lazy); the one at shutdown is inline, on purpose. `diff`, `rg` and every
  command are child processes, awaited in the reactor.
- No per-event allocations beyond the JSON line itself; a transcript record is one `write` of its line.
- Command output is streamed in ≤ 4 KiB reads and capped (§6.1); file reads capped at 64 KiB per call.
- Idle RSS target: < 8 MB; a turn with a 150k-token context: < 40 MB (the message list dominates).
- **It idles.** Between turns, while a command runs, while a background job runs and while an external
  tool result is awaited, the process is parked in the reactor (epoll on stdin, the child's pipes, the
  socket): no polling loops and no periodic timers; the short waits there are one-shot and bounded (a
  kill's grace, the drain after a stream or a command ends). The two short polls bija has run outside
  the idle states: at open, waiting up to 2 s for the lock of a predecessor that is still dying; at
  shutdown, watching leftover command groups (no longer our children, so nothing reports their end)
  within the grace. The e2e tests hold it to this: bija's own CPU time (utime + stime) stays under
  50 ms through a `bash` `sleep 3`, and through 2 s waiting on stdin.

---

## 9. Mantra integration

### 9.1 Config (`config.rs`)

- `ProviderKind::Bija` (`kind = "bija"`); `ProviderEntry.wire: String` — `""`/`"openai"` →
  openai-chat, `"anthropic"` → anthropic (serialized only when non-empty). A Bija provider's key is
  optional (`needs_key` false; `draft_reason` only checks the URL): a local Ollama/vLLM takes none.
- `Settings.bija_command: Vec<String>` (default `["bija"]`); resolution at startup
  (`config::resolve_bija_command`): if `bija_command[0]` has no path separator and is not on PATH, a
  `bija` beside the running `mantra` executable is used (`util::self_exe()`), then `$MANTRA_BIJA`;
  `doctor` prints which.
- **Bija is the default runner for Solo.** `Registry::defaults()` always ships a provider `bija`
  (`name = "Bija"`, `kind = bija`, `base_url = "https://api.openai.com/v1"`, `env_key = "OPENAI_API_KEY"`,
  `wire = ""`) — bija is installed beside mantra, so it is always available — and four aliases that are the
  default models reached through it: `astra-api`, `sol-api`, `terra-api`, `luna-api` (same model ids,
  windows, efforts and notes as the Codex ones, note suffixed " — through bija with an API key"). The
  Codex aliases and the `claude` provider stay exactly as they are. `Settings.default_model` defaults to
  `"sol-api"`; the existing fresh-install branch for a Codex ChatGPT login keeps setting `"sol"`. The
  `--help`/README text that says what Solo runs on says bija. Everything else about a Bija provider is
  configured in `/models` like a custom provider (flip `kind`, set `wire`, URL, key; `D` discovers via
  `/models`). A `models.toml` with no Bija provider at all (written before bija shipped) gets the `bija`
  provider and whichever `-api` aliases it lacks once, at load, and is saved; one with a Bija provider is
  left as the user set it up. Without Codex, `mantra-default-bija` becomes the run default only when every
  one of its aliases exists on a Bija provider.
- Built-in pattern `mantra-default-bija` (`NAME_BIJA`) = `mantra-default` with every role's alias
  suffixed `-api` (planner `astra-api`, workers `sol-api`/`luna-api`, …: whatever the base pattern uses,
  mapped `x → x-api`). It works on a real install and in the demo alike.
- `--demo`: an in-process mock OpenAI server (`mock_openai.rs`, §9.5) is started on 127.0.0.1:<random
  port>; the in-memory registry's `bija` provider is re-pointed at it (`base_url =
  http://127.0.0.1:<port>/v1`, no key, `env_key = ""`), so the same `*-api` aliases drive the real `bija`
  binary against the mock. The demo's Solo default stays `mock-codex` (the existing snapshot and web
  e2e scripts rely on it); `--demo --pattern mantra-default-bija` runs a full Mandala on bija. In the
  demo `bija_command` is the `bija` beside the running executable (`target/<profile>/bija`).

### 9.2 Hub (`hub.rs`, `hub/bija.rs`)

- `Backend::Bija` arm in `Hub::spawn`/`agent_task` → `bija::run_bija_process(…)` with the same
  signature family as `run_claude_process` (`thread_id` = the bija session id; a relaunch passes
  `--resume <id>`).
- `SpawnSpec.bija: Option<BijaSpawn>`: `{ base_url, wire, api_key_env: Option<String> (the var name; the
  value rides in `SpawnSpec.envs`), context_window, compact_pct, system_prompt, home: PathBuf
  (`$MANTRA_HOME/bija`), sandbox, writable_roots, network }`.
- Spawn: Mantra creates `<home>/sessions/<sid>/` and writes `tools.json` (the role's
  `dynamic_tools`) and `system.md` there before launching
  `bija --home <home> --session <sid> --cwd <cwd> --provider-url … --wire … --api-key-env … --model …
  --effort … --context-window … --compact-at … --system-file …/system.md --tools-file …/tools.json
  --sandbox … [--writable …]… [--network] --name <thread_name>`; a resume launches with `--resume <sid>`
  and the same flags (so a changed model/effort is picked up).
- Translation (`Translator::on_event`, pure, unit-tested on a recorded fixture
  `testdata/bija-events.jsonl`): `ready` → `HubEvent::Ready{thread_id: session, model, resumed}`;
  `turn_started` → `turn/started {turn:{id}}`; `text_delta` → `item/agentMessage/delta {itemId, delta}`
  (preceded once per id by `item/started {item:{id,type:"agentMessage",text:""}}`); `message` →
  `item/completed {item:{id,type:"agentMessage",text}}`; `reasoning_delta` →
  `item/reasoning/summaryTextDelta`; `tool_started` → `item/started` with `commandExecution
  {command}` for `bash`/`job`, `fileChange {changes:[{path,kind:{type:"modified"},diff:""}]}` for
  write/edit/delete, `dynamicToolCall {tool, arguments}` for the other built-ins (shown under their
  bare name, `read_file`, as the role's `mantra_*` tools are);
  `tool_output` → `item/commandExecution/outputDelta`; `tool_done` → `item/completed` with
  `status: completed|failed`, `aggregatedOutput`/`exitCode`/`durationMs` for commands, `changes`
  (`kind.type` ∈ `add|modified|delete`, `diff`) for file changes, `contentItems:[{type,text}]` (the
  result's first 400 chars for a built-in) for the rest;
  `tool_call` → `HubEvent::Request{id: Value::String(call id), method:"item/tool/call",
  params:{tool, arguments}}` and `item/started {dynamicToolCall}`; a later `Cmd::Respond{id,result}` →
  `Op::ToolResult{id, ok: result.success, text: contentItems[].text joined}` and
  `item/completed {dynamicToolCall, contentItems}`; `Cmd::RespondErr` → `ok:false`;
  `usage` → `thread/tokenUsage/updated {tokenUsage:{last:{inputTokens,outputTokens},
  total:{totalTokens,cachedInputTokens}, modelContextWindow}}`; `retry` → `error {error:{message},
  willRetry:true}` (what Codex sends; `Agent` shows "retrying"), and the message and reasoning items
  still streaming are closed without their partial text — the retry streams under fresh ids (§4.2)
  and the abandoned ones never get a `message`. `Agent` cannot remove an item, so each is completed
  with its text replaced by "⟲ (retried — partial output dropped)": a partial that stayed could
  become `final_message`, where the engine reads `STATUS:`/`GATE:`; `compacted` → the same
  `contextCompaction` item dance `hub/claude.rs` does (tokenUsage before, item started+completed,
  tokenUsage after); `turn_done` → `turn/completed {turn:{status, error:{message, codexErrorInfo}}}`
  with `codexErrorInfo` = transient→`serverOverloaded`, rate_limit→`rateLimitExceeded`,
  auth→`unauthorized`, context→`contextWindowExceeded`, bad_request→`badRequest`, usage→
  `usageLimitExceeded`, interrupted/internal→ none; `warning` → `warning {message}`; `exit` → the
  process is ending (the crash/exit path handles the rest).
- Cmds: `Turn`→`prompt`, `Steer`→`steer`, `Interrupt`→`interrupt`, `Compact`→`compact`,
  `SetEffort`/`SetModel`→`set` (no restart — bija switches live), `SetName`→`set{name}`,
  `Shell`→`shell`, `Archive`→ nothing (session stays on disk), `Restart`→ `shutdown` → `Exit::Restarting`
  (resume), `Shutdown`→`shutdown`. One process stopping on its own (a restart, an agent shut down) is
  given ≤ 5 s after `shutdown`, then killed; quitting Mantra gives every bija agent together
  `hub::QUIT_GRACE` (3 s) — both well over bija's own 1 s grace, and bija's transcript is durable
  before that grace starts (§3). A process exit before `ready` → `Exit::LaunchFailed` with the last
  stderr line; after → `Exit::Crashed`. Until
  `ready`, `Turn`/`Steer` text is held in `replay` (delivered at `ready`, this launch's or the next's) and
  `Shell`/`Compact` are refused as `CmdFailed` ("bija is still starting"). After `ready{pending:true}`
  and until that turn's `turn_done`, held and new `Turn` text goes out as `steer`, folding into the turn
  bija continues instead of queueing a second one. A process that ended (crash, signal) while a turn
  was open and whose relaunch says `ready{resumed:true, pending:false}` (an older bija that closed the
  turn on SIGTERM, a transcript that lost its tail) gets the last `Turn` text again as a `prompt` —
  once, logged — and its `Ready` is reported as no resume, so the engine does not prompt a second
  time. `--compact-at` is clamped to 100. bija's
  "tool_result for unknown call id" warning (the engine answering a call it just interrupted) is not
  forwarded.
- `Agent.backend == Bija` paths: `web/snapshot.rs` `backend_str` → `"bija"`; `ui/overlays.rs`
  glyph `"• bija"`; Studio `PROV_KINDS` += `"Bija"` (+ `parse_kind`), provider editor shows `wire`
  for Bija; web assets label `Bija`; `doctor` prints the resolved bija command and version; the
  README picker text.

### 9.3 Release and install

`release.yml` builds the workspace (`cargo build --release --target …` builds both default members)
and uploads a second asset per platform, `bija-<os>-<arch>` + `.sha256`; `install.sh` downloads it
beside `mantra` (optional: a missing bija asset logs a note and continues) and, on a source build,
copies `target/release/bija` too.

### 9.4 Docs

Where bija is documented for users: the README's "Bija: the built-in harness" section under *Models,
providers and backends* (what it is, a 6-line `models.toml`, the demo pattern, standalone use, swarms),
the backends paragraph under *Under the hood* and the files table (`~/.mantra/bija/sessions/`);
DESIGN.md §2 (diagram and the "Bija" decision); the CHANGELOG's `## v0.7.0` (both manifests carry
0.7.0). Code, comments and docs name no AI model beyond what the registry already lists.

### 9.5 mock_openai.rs (mantra, demo + e2e)

An axum (already a dependency) `POST /v1/chat/completions` on a random loopback port, streaming SSE in
the OpenAI shape, scripted by the `mantra-role:` marker in the system message exactly like `mock.rs`
scripts Codex: planner → `mantra_submit_plan` (the shared plan JSON), orchestrator → one
`mantra_spawn` per task then `mantra_wait`, worker → one `edit_file`/`write_file` on the task's file
then a final text with the `STATUS:` line, gate → `mantra_gate_report` pass, manager → `mantra_wait`,
solo/probe → "OK"/a short text. It decides what to send from the message list (how many tool results it
already sees for the current turn), is stateless otherwise, and reports plausible `usage`.
