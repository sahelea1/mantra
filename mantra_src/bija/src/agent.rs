//! The turn loop (SPEC §3): prompts, steering, interrupts, compaction, built-in and external tools.
//!
//! One task does everything. Whatever the agent is waiting on — a completion streaming in, a
//! command running, an external tool's answer — it waits through `drive`, which `select!`s that
//! future against the inbox, so every op is answered within one loop iteration and an interrupt
//! simply drops the future (the api client and the tool runtime are both cancel-safe). That shape
//! dictates the borrows: the future holds the client or the runtime and the message list, so the op
//! handler (`on_busy_input`) may only touch what is left — `Ctl` (queue, steer buffer, flags, event
//! sink), the session meta and the session dir — and reports jobs and message counts from a
//! snapshot taken before the wait.
//!
//! The transcript is kept valid for the API at every step that can end a turn: a tool call that
//! never got its result (interrupt, shutdown, round cap) is answered by `Session::repair` before the
//! note and the `turn` record are written, because providers want the results right after the call.
//! The one stop that ends no turn is a signal (`Input::Terminate`): whoever sent it wants the process
//! gone, not the work, so the turn is left open exactly as a SIGKILL would leave it, and the next
//! `--resume` continues it.

use crate::api::Client;
use crate::control::Fanout;
use crate::proto::{Event, Op, TurnStatus};
use crate::session::{self, Fsync, Meta, OpenTurn, Session};
use crate::tools::{args, Runtime};
use crate::types::{estimate_tokens, prefix, sent_chars, str_at, suffix, ApiError, Delta, ErrorKind, Message, Request, Response, RetryNote, ToolCall, ToolDef, ToolOutcome, Usage};
use serde_json::{json, Value};
use std::cell::Cell;
use std::collections::{HashSet, VecDeque};
use std::future::Future;
use std::io::{self, Write};
use std::path::Path;
use std::time::Instant;
use tokio::sync::{mpsc, oneshot};

/// Model calls with tool calls per turn before the turn is failed (a model stuck in a loop).
pub const ROUND_CAP: u32 = 200;

/// The user message that asks for a compaction summary (SPEC §3).
const SUMMARY_REQUEST: &str = "Your context is about to be compacted: everything above will be replaced by what you write now. \
Write a hand-off summary for your future self, who continues this work with nothing but the summary. Be concrete: \
the goal; what is done; what is in progress; key file paths, commands and decisions (with the reasons); \
open problems and errors; what to do next, step by step. Do not call tools — reply with the summary only.";

/// What a compaction request keeps of each message, in bytes: a tool's output and each string in a
/// call's arguments are cut to their head and tail, a long prompt or reply likewise. A summary needs
/// the gist; the full outputs are what overflowed the window in the first place.
const CLIP_TOOL: usize = 4 * 1024;
const CLIP_ARG: usize = 2 * 1024;
const CLIP_TEXT: usize = 16 * 1024;
/// The share of the window a compaction request is cut down to (the rest is room for the summary
/// and for the error of a chars/4 estimate).
const COMPACT_FIT_PERCENT: u64 = 80;
/// Retries of a compaction the provider still found too long, each with half the budget, before
/// the history is cut mechanically instead.
const COMPACT_RETRIES: u32 = 2;

/// Between bija's preamble and the caller's own system prompt in `Meta::system`, so a resume can
/// rebuild either side alone.
const SYSTEM_SEPARATOR: &str = "\n\n---\n\n";

const PREAMBLE_HEAD: &str = "You are a coding agent running in bija, a minimal harness. You work through the tools below and carry the task you are given through to the end.";

const PREAMBLE_TOOLS: &str = "\
How the tools work:
- Paths are relative to the working directory; `~` is not expanded.
- read_file returns numbered lines; read a file before you edit it.
- edit_file replaces an exact substring: `old` must match exactly once (copy it without the line numbers), or set replace_all.
- write_file writes a whole file; delete removes files (a directory needs recursive: true).
- bash runs a command in the working directory; long output is cut to its tail.
- Start long-running commands (servers, watchers, long builds) with background: true, then use job to wait, read output or kill.
- Prefer the file tools over shell commands for reading and editing files.
When you are done, reply with a short report of what you did and what is left; that reply is your final answer.";

/// One line from stdin or a control client.
#[derive(Debug)]
pub enum Input {
    Op(Op),
    Bad(String),
    /// stdin closed: the parent is gone, which is a shutdown (SPEC §4).
    Eof,
    /// SIGTERM or SIGHUP (the signal number): something outside wants the process gone — a user's
    /// `kill`, a supervisor, a container stop. Never Mantra, which sends `shutdown`. A crash-safe
    /// stop (SPEC §3): the turn stays open for the next `--resume`, the exit code is 128 + signal.
    Terminate(i32),
}

impl Input {
    /// `None` for a blank line (ignored, not an error).
    pub fn from_line(line: &str) -> Option<Input> {
        let line = line.trim();
        if line.is_empty() {
            return None;
        }
        Some(match Op::parse(line) {
            Ok(op) => Input::Op(op),
            Err(e) => Input::Bad(e),
        })
    }
}

pub type Inbox = mpsc::UnboundedReceiver<Input>;

/// Where event lines go: stdout in serve mode; stderr (`-v`) or nowhere for `bija run`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sink {
    Stdout,
    Stderr,
    Discard,
}

/// The event sink. Cheap to clone: the streaming callbacks, the progress forwarder and the op
/// handler each hold one while a future is in flight.
#[derive(Clone)]
pub struct Out {
    sink: Sink,
    fanout: Option<Fanout>,
}

impl Out {
    pub fn new(sink: Sink, fanout: Option<Fanout>) -> Out {
        Out { sink, fanout }
    }

    /// Writes the event as one line and flushes it, then queues it to control clients. A write to
    /// a closed stdout is dropped: the parent is gone and stdin EOF is about to shut us down.
    pub fn emit(&self, ev: &Event) {
        let mut line = ev.line();
        if let Some(f) = &self.fanout {
            f.send(&line);
        }
        line.push('\n');
        let _ = match self.sink {
            Sink::Stdout => write_line(&mut io::stdout().lock(), &line),
            Sink::Stderr => write_line(&mut io::stderr().lock(), &line),
            Sink::Discard => Ok(()),
        };
    }
}

fn write_line(w: &mut impl Write, line: &str) -> io::Result<()> {
    w.write_all(line.as_bytes())?;
    w.flush()
}

/// How a turn ended, for `bija run`.
#[derive(Debug, Clone, PartialEq)]
pub struct TurnReport {
    pub status: TurnStatus,
    /// The text of the turn's last assistant message (may be empty).
    pub text: String,
    pub error: Option<ApiError>,
}

/// How `serve` begins and ends.
pub struct Start {
    pub resumed: bool,
    /// The transcript ended inside a turn: continue it before anything else.
    pub pending: bool,
    /// `bija run`: return once the queue is worked off instead of waiting for ops.
    pub once: bool,
    /// Emitted as `warning` events right after `ready`.
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Idle,
    Turn,
    /// An idle compaction (no turn).
    Compacting,
    /// A user's `$ shell` command (no turn).
    Shell,
}

/// What a completion is for.
enum Ask<'a> {
    /// A turn's model call: the history and every tool, streamed under these ids.
    Round(&'a RoundIds),
    /// A compaction: this history (ending in the summary request), no tools, no effort, nothing
    /// streamed.
    Summary(Vec<Message>),
}

/// The stream ids of one model call: the K of `t<N>-m<K>` / `t<N>-r<K>`, and whether a delta went
/// out under it yet. A retry after streamed output moves on to the next K, so a consumer never
/// sees one id stream the same text twice, and the `message` that ends the call names the id its
/// deltas went out under. Cells, because the delta and the retry callbacks share it while the
/// completion holds both.
struct RoundIds {
    k: Cell<usize>,
    used: Cell<bool>,
}

impl RoundIds {
    fn new(k: usize) -> RoundIds {
        RoundIds { k: Cell::new(k), used: Cell::new(false) }
    }

    /// The K a delta goes out under now.
    fn streaming(&self) -> usize {
        self.used.set(true);
        self.k.get()
    }

    /// Before a retry: ids that carried part of the failed attempt are given up for the next K.
    /// Their partial text never gets a `message` (it is not the reply).
    fn abandon_streamed(&self) {
        if self.used.replace(false) {
            self.k.set(self.k.get() + 1);
        }
    }

    /// The last K the call took up: the reply's when it answered, else the last one it streamed
    /// under (so the next call starts past every id already seen).
    fn last(&self, answered: bool) -> usize {
        let k = self.k.get();
        if answered || self.used.get() {
            k
        } else {
            k - 1
        }
    }
}

/// Why a wait was cut short.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stop {
    Interrupt,
    Shutdown,
    /// `Input::Terminate`: stop without ending the turn.
    Terminate,
}

/// The part of the agent an op may touch while a completion or a tool holds the rest.
struct Ctl {
    out: Out,
    phase: Phase,
    turn: Option<String>,
    queue: VecDeque<String>,
    steer: Vec<String>,
    compact_requested: bool,
    shutdown: bool,
    /// The signal a crash-safe stop is for (`Input::Terminate`); `shutdown` is set with it.
    terminated: Option<i32>,
    shell_seq: u64,
    /// How many leading messages the last measured input-token count covers; the rest is estimated.
    measured: usize,
    /// A failed session write was reported and none has succeeded since (one warning, not a flood).
    disk_warned: bool,
}

impl Ctl {
    fn warn(&self, message: impl Into<String>) {
        self.out.emit(&Event::Warning { message: message.into() });
    }

    fn error(&self, message: impl Into<String>) {
        self.out.emit(&Event::Error { message: message.into() });
    }

    /// What a wait cut short means beyond the wait itself (`terminated` is set where the signal
    /// arrived, `on_busy_input`).
    fn on_stop(&mut self, stop: Stop) {
        match stop {
            Stop::Interrupt => {}
            Stop::Shutdown | Stop::Terminate => self.shutdown = true,
        }
    }

    /// Memory stays right when the disk fails; say so once instead of failing the turn.
    fn persisted<T>(&mut self, r: io::Result<T>) {
        match r {
            Ok(_) => self.disk_warned = false,
            Err(e) if !self.disk_warned => {
                self.disk_warned = true;
                self.warn(format!("session write failed (the transcript may be incomplete): {e}"));
            }
            Err(_) => {}
        }
    }
}

/// What an op answered mid-wait can report about the borrowed parts of the agent.
#[derive(Debug, Clone, Copy)]
struct Snapshot {
    messages: usize,
    jobs: usize,
}

/// One turn in progress.
struct Turn {
    id: String,
    /// The last K of `t<N>-m<K>` taken: one per assistant message, plus any a retry gave up.
    messages: usize,
    /// Tool calls so far: the K of a fallback `t<N>-c<K>`.
    calls: usize,
    rounds: u32,
    last_text: String,
    compacted_this_round: bool,
    auto_compact_failed: bool,
    /// The call whose `tool_started`/`tool_call` went out without its `tool_done` yet.
    in_flight: Option<InFlight>,
}

struct InFlight {
    id: String,
    name: String,
    started: Instant,
}

impl Turn {
    fn new(n: u64, so_far: OpenTurn) -> Turn {
        Turn {
            id: turn_id(n),
            messages: so_far.assistant_messages,
            calls: so_far.tool_calls,
            rounds: so_far.assistant_messages as u32,
            last_text: String::new(),
            compacted_this_round: false,
            auto_compact_failed: false,
            in_flight: None,
        }
    }
}

struct End {
    status: TurnStatus,
    error: Option<ApiError>,
}

impl End {
    fn completed() -> End {
        End { status: TurnStatus::Completed, error: None }
    }
    fn failed(e: ApiError) -> End {
        End { status: TurnStatus::Failed, error: Some(e) }
    }
    fn interrupted() -> End {
        End { status: TurnStatus::Interrupted, error: None }
    }
}

pub struct Agent {
    session: Session,
    client: Client,
    runtime: Runtime,
    /// Everything offered to the model: the built-ins, then the external tools.
    tools: Vec<ToolDef>,
    external: HashSet<String>,
    ctl: Ctl,
    warnings: Vec<String>,
    report: Option<TurnReport>,
}

impl Agent {
    pub fn new(session: Session, client: Client, runtime: Runtime, external: Vec<ToolDef>, out: Out) -> Agent {
        let mut tools = runtime.defs();
        let mut names: HashSet<String> = tools.iter().map(|d| d.name.clone()).collect();
        let mut ext = HashSet::new();
        let mut warnings = Vec::new();
        for d in external {
            if runtime.is_builtin(&d.name) || names.contains(&d.name) {
                warnings.push(format!("external tool {} ignored: the name is already taken", d.name));
                continue;
            }
            names.insert(d.name.clone());
            ext.insert(d.name.clone());
            tools.push(d);
        }
        let measured = session.messages.len();
        let ctl = Ctl { out, phase: Phase::Idle, turn: None, queue: VecDeque::new(), steer: Vec::new(), compact_requested: false, shutdown: false, terminated: None, shell_seq: 0, measured, disk_warned: false };
        Agent { session, client, runtime, tools, external: ext, ctl, warnings, report: None }
    }

    /// Queues a prompt to run as soon as the agent is idle (`bija run`).
    pub fn queue_prompt(&mut self, text: String) {
        self.ctl.queue.push_back(text);
    }

    /// The last finished turn, if any.
    pub fn report(&self) -> Option<&TurnReport> {
        self.report.as_ref()
    }

    /// Emits `ready`, continues a pending turn, then serves ops until shutdown (or, with `once`,
    /// until the queue is worked off). Ends with the `exit` event; returns the exit code.
    pub async fn serve(&mut self, inbox: &mut Inbox, start: Start) -> i32 {
        let m = &self.session.meta;
        self.ctl.out.emit(&Event::Ready {
            session: m.id.clone(),
            dir: self.session.dir.display().to_string(),
            model: m.model.clone(),
            wire: m.wire.as_str().to_string(),
            resumed: start.resumed,
            pid: std::process::id(),
            pending: start.pending,
        });
        if let Some(w) = self.runtime.startup_warning() {
            self.ctl.warn(w);
        }
        for w in start.warnings.into_iter().chain(std::mem::take(&mut self.warnings)) {
            self.ctl.warn(w);
        }
        if start.pending {
            self.run_turn(inbox, None).await;
        }
        self.idle(inbox, start.once).await;
        // A signal's stop exits non-zero, so a supervisor sees the process did not end by request.
        let code = self.ctl.terminated.map_or(0, |sig| 128 + sig);
        self.shutdown(code).await;
        code
    }

    async fn idle(&mut self, inbox: &mut Inbox, once: bool) {
        while !self.ctl.shutdown {
            // What arrived while busy is answered first, as the idle ops it now is: an interrupt
            // that raced the end of the last turn is a no-op rather than the end of the next one,
            // and a shutdown or a compact goes before the queue.
            if let Ok(input) = inbox.try_recv() {
                self.on_idle_input(input, inbox).await;
                continue;
            }
            if std::mem::take(&mut self.ctl.compact_requested) {
                self.compact_idle(inbox).await;
                continue;
            }
            if let Some(text) = self.ctl.queue.pop_front() {
                self.run_turn(inbox, Some(text)).await;
                continue;
            }
            if once {
                return;
            }
            let input = inbox.recv().await.unwrap_or(Input::Eof);
            self.on_idle_input(input, inbox).await;
        }
    }

    async fn on_idle_input(&mut self, input: Input, inbox: &mut Inbox) {
        let op = match input {
            Input::Op(op) => op,
            Input::Bad(message) => return self.ctl.error(message),
            Input::Eof => {
                self.ctl.shutdown = true;
                return;
            }
            Input::Terminate(sig) => {
                self.ctl.terminated.get_or_insert(sig);
                self.ctl.shutdown = true;
                return;
            }
        };
        match op {
            // Idle, a steer is just a prompt.
            Op::Prompt { text } | Op::Steer { text } => {
                if text.trim().is_empty() {
                    self.ctl.error("empty prompt");
                } else {
                    self.ctl.queue.push_back(text);
                }
            }
            Op::Interrupt => {}
            Op::Compact => self.ctl.compact_requested = true,
            Op::Set { model, effort, name } => apply_set(&mut self.ctl, &mut self.session.meta, &self.session.dir, self.session.fsync, model, effort, name),
            Op::Shell { command } => self.run_shell(command, inbox).await,
            Op::ToolResult { id, .. } => self.ctl.warn(format!("tool_result for unknown call id {id}: ignored")),
            Op::Status => {
                let snap = self.snapshot();
                self.ctl.out.emit(&status_event(&self.ctl, &self.session.meta, snap));
            }
            Op::Shutdown => self.ctl.shutdown = true,
        }
    }

    /// A new turn for `prompt`, or (with `None`) the continuation of the pending turn on resume.
    async fn run_turn(&mut self, inbox: &mut Inbox, prompt: Option<String>) {
        let so_far = if prompt.is_some() { OpenTurn::default() } else { self.session.open_turn() };
        if prompt.is_some() || self.session.meta.turns == 0 {
            // Counted (and saved) before the prompt is written, so a crash can skip a number but
            // never reuse one.
            self.session.meta.turns += 1;
            let r = self.session.save_meta();
            self.ctl.persisted(r);
        }
        let mut t = Turn::new(self.session.meta.turns, so_far);
        self.ctl.phase = Phase::Turn;
        self.ctl.turn = Some(t.id.clone());
        if let Some(text) = prompt {
            let r = self.session.push(Message::User { text });
            self.ctl.persisted(r);
        }
        self.ctl.out.emit(&Event::TurnStarted { turn: t.id.clone() });
        let end = self.turn_loop(&mut t, inbox).await;
        if self.ctl.terminated.is_some() {
            // Left open on purpose: no repair, no note, no `turn` record, no `turn_done`. The
            // transcript ends inside the turn (the call without its result, or the user message),
            // so `--resume` reports it pending and finishes it. Steers not yet delivered are
            // dropped, as a SIGKILL would drop them.
            self.ctl.phase = Phase::Idle;
            self.ctl.turn = None;
            return;
        }
        self.finish_turn(&mut t, end);
    }

    async fn turn_loop(&mut self, t: &mut Turn, inbox: &mut Inbox) -> End {
        loop {
            self.deliver_steer();
            if !t.auto_compact_failed && self.over_threshold() {
                match self.compact(inbox, Some(&t.id)).await {
                    Ok(done) => t.auto_compact_failed = !done,
                    Err(stop) => return self.stopped(stop),
                }
            }
            let ids = RoundIds::new(t.messages + 1);
            let result = self.complete(inbox, &t.id, Ask::Round(&ids)).await;
            t.messages = ids.last(matches!(result, Ok(Ok(_))));
            let resp = match result {
                Err(stop) => return self.stopped(stop),
                Ok(Ok(resp)) => resp,
                // Once per round: compact and ask again; a second overflow fails the turn.
                Ok(Err(e)) if e.kind == ErrorKind::Context && !t.compacted_this_round => {
                    t.compacted_this_round = true;
                    match self.compact(inbox, Some(&t.id)).await {
                        Ok(_) => continue,
                        Err(stop) => return self.stopped(stop),
                    }
                }
                Ok(Err(e)) => return End::failed(e),
            };
            t.compacted_this_round = false;
            let calls = self.record_reply(t, resp.message);
            self.record_usage(&t.id, resp.usage);
            if calls.is_empty() {
                // A steer that arrived during the final answer still gets an answer.
                if self.ctl.steer.is_empty() {
                    return End::completed();
                }
                continue;
            }
            t.rounds += 1;
            if t.rounds > ROUND_CAP {
                return End::failed(ApiError::internal(format!("tool round cap: {ROUND_CAP} rounds of tool calls in one turn")));
            }
            for call in calls {
                if let Err(stop) = self.run_call(t, call, inbox).await {
                    return self.stopped(stop);
                }
            }
        }
    }

    fn stopped(&mut self, stop: Stop) -> End {
        self.ctl.on_stop(stop);
        End::interrupted()
    }

    fn finish_turn(&mut self, t: &mut Turn, end: End) {
        let interrupted = end.status == TurnStatus::Interrupted;
        let unfinished = if interrupted { "interrupted" } else { "not run: the turn ended before this tool ran" };
        if let Some(f) = t.in_flight.take() {
            let outcome = ToolOutcome { duration_ms: f.started.elapsed().as_millis() as u64, ..ToolOutcome::err(unfinished) };
            self.ctl.out.emit(&tool_done(&t.id, f.id, f.name, outcome));
        }
        let r = self.session.repair(unfinished);
        self.ctl.persisted(r);
        // Steers that never reached a model call stay in the history for the next turn.
        self.deliver_steer();
        if interrupted {
            let r = self.session.note("turn interrupted");
            self.ctl.persisted(r);
        }
        let r = self.session.end_turn(end.status.as_str());
        self.ctl.persisted(r);
        self.ctl.phase = Phase::Idle;
        self.ctl.turn = None;
        self.ctl.out.emit(&Event::TurnDone { turn: t.id.clone(), status: end.status.clone(), error: end.error.clone() });
        self.report = Some(TurnReport { status: end.status, text: std::mem::take(&mut t.last_text), error: end.error });
    }

    fn deliver_steer(&mut self) {
        // Not echoed: whoever steered already shows it (SPEC §3).
        for text in std::mem::take(&mut self.ctl.steer) {
            let r = self.session.push(Message::User { text });
            self.ctl.persisted(r);
        }
    }

    /// One completion while serving ops (see `Ask`). A retry that follows streamed output moves the
    /// round to fresh stream ids (`RoundIds`). An adaptation resend (the api layer dropping an
    /// optional field a provider refused) keeps them: providers refuse a request before they
    /// stream any of it.
    async fn complete(&mut self, inbox: &mut Inbox, turn: &str, ask: Ask<'_>) -> Result<Result<Response, ApiError>, Stop> {
        let meta = &self.session.meta;
        let (model, system, max_output_tokens) = (meta.model.clone(), meta.system.clone(), meta.max_output_tokens);
        let (messages, tools, ids, effort): (&[Message], &[ToolDef], _, _) = match &ask {
            Ask::Round(ids) => (&self.session.messages, &self.tools, Some(*ids), meta.effort.clone()),
            Ask::Summary(history) => (history, &[], None, String::new()),
        };
        let out = self.ctl.out.clone();
        let mut on_delta = |d: Delta| {
            let Some(ids) = ids else { return };
            match d {
                Delta::Text(text) if !text.is_empty() => out.emit(&Event::TextDelta { turn: turn.to_string(), id: message_id(turn, ids.streaming()), text }),
                Delta::Reasoning(text) if !text.is_empty() => out.emit(&Event::ReasoningDelta { turn: turn.to_string(), id: reasoning_id(turn, ids.streaming()), text }),
                _ => {}
            }
        };
        let mut on_retry = |n: &RetryNote| {
            if let Some(ids) = ids {
                ids.abandon_streamed();
            }
            out.emit(&Event::Retry { turn: turn.to_string(), attempt: n.attempt, max: n.max, error: n.error.to_string(), wait_ms: n.wait.as_millis() as u64 });
        };
        let req = Request { model: &model, effort: &effort, system: &system, messages, tools, max_output_tokens };
        let snap = self.snapshot();
        let call = self.client.complete(&req, &mut on_delta, &mut on_retry);
        let (ctl, meta, dir, fsync) = (&mut self.ctl, &mut self.session.meta, self.session.dir.as_path(), self.session.fsync);
        drive(call, inbox, |input| on_busy_input(ctl, meta, dir, fsync, snap, input)).await
    }

    /// Pushes the assistant reply (call ids made usable first), emits its `message`, and returns
    /// its tool calls.
    fn record_reply(&mut self, t: &mut Turn, reply: Message) -> Vec<ToolCall> {
        let (text, reasoning, mut tool_calls) = match reply {
            Message::Assistant { text, reasoning, tool_calls } => (text, reasoning, tool_calls),
            // The api layer promises an assistant message; anything else counts as an empty one.
            _ => (String::new(), String::new(), Vec::new()),
        };
        normalize_calls(&t.id, &mut t.calls, &mut tool_calls);
        t.last_text = text.clone();
        let calls = tool_calls.clone();
        let r = self.session.push(Message::Assistant { text: text.clone(), reasoning, tool_calls });
        self.ctl.persisted(r);
        if !text.trim().is_empty() {
            self.ctl.out.emit(&Event::Message { turn: t.id.clone(), id: message_id(&t.id, t.messages), text });
        }
        calls
    }

    async fn run_call(&mut self, t: &mut Turn, mut call: ToolCall, inbox: &mut Inbox) -> Result<(), Stop> {
        if let Some(problem) = bad_arguments(&call) {
            self.emit_started(&t.id, &call);
            self.finish_call(&t.id, call, ToolOutcome::err(problem));
            return Ok(());
        }
        // After `record_reply` stored the call: the transcript and the history sent back keep what
        // the model wrote, the tool (and its `tool_started`/`tool_call` event) gets the schema's types.
        if let Some(def) = self.tools.iter().find(|d| d.name == call.name) {
            args::coerce(&def.parameters, &mut call.args);
        }
        let started = Instant::now();
        t.in_flight = Some(InFlight { id: call.id.clone(), name: call.name.clone(), started });
        let outcome = if self.external.contains(&call.name) {
            self.ctl.out.emit(&Event::ToolCall { turn: t.id.clone(), id: call.id.clone(), name: call.name.clone(), args: call.args.clone() });
            let (ok, text) = self.await_external(&call.id, inbox).await?;
            ToolOutcome { ok, text, duration_ms: started.elapsed().as_millis() as u64, ..ToolOutcome::default() }
        } else if self.runtime.is_builtin(&call.name) {
            self.emit_started(&t.id, &call);
            self.run_builtin(&t.id, &call, inbox).await?
        } else {
            self.emit_started(&t.id, &call);
            let available: Vec<&str> = self.tools.iter().map(|d| d.name.as_str()).collect();
            ToolOutcome::err(format!("unknown tool {}; available: {}", call.name, available.join(", ")))
        };
        t.in_flight = None;
        self.finish_call(&t.id, call, outcome);
        Ok(())
    }

    fn emit_started(&self, turn: &str, call: &ToolCall) {
        self.ctl.out.emit(&Event::ToolStarted { turn: turn.to_string(), id: call.id.clone(), name: call.name.clone(), args: call.args.clone() });
    }

    fn finish_call(&mut self, turn: &str, call: ToolCall, outcome: ToolOutcome) {
        let r = self.session.push(Message::Tool { call_id: call.id.clone(), name: call.name.clone(), text: outcome.text.clone(), is_error: !outcome.ok });
        self.ctl.persisted(r);
        self.ctl.out.emit(&tool_done(turn, call.id, call.name, outcome));
    }

    async fn run_builtin(&mut self, turn: &str, call: &ToolCall, inbox: &mut Inbox) -> Result<ToolOutcome, Stop> {
        let snap = self.snapshot();
        let out = self.ctl.out.clone();
        let (tx, rx) = mpsc::unbounded_channel();
        let work = stream_output(&out, turn, &call.id, rx, self.runtime.call(&call.name, &call.args, Some(tx)));
        let (ctl, meta, dir, fsync) = (&mut self.ctl, &mut self.session.meta, self.session.dir.as_path(), self.session.fsync);
        drive(work, inbox, |input| on_busy_input(ctl, meta, dir, fsync, snap, input)).await
    }

    /// Waits for the `tool_result` of an external call while serving every other op.
    async fn await_external(&mut self, call_id: &str, inbox: &mut Inbox) -> Result<(bool, String), Stop> {
        let snap = self.snapshot();
        let (tx, rx) = oneshot::channel();
        let mut answer = Some(tx);
        let (ctl, meta, dir, fsync) = (&mut self.ctl, &mut self.session.meta, self.session.dir.as_path(), self.session.fsync);
        let got = drive(rx, inbox, |input| match input {
            Input::Op(Op::ToolResult { id, ok, text }) if id == call_id => {
                if let Some(tx) = answer.take() {
                    let _ = tx.send((ok, text));
                }
                None
            }
            other => on_busy_input(ctl, meta, dir, fsync, snap, other),
        })
        .await?;
        // The sender lives in the closure until `drive` returns, so this cannot be a closed channel.
        Ok(got.unwrap_or_else(|_| (false, "no result arrived".to_string())))
    }

    /// `Op::Shell`: the user's own command, idle only, seen by the model as a note.
    async fn run_shell(&mut self, command: String, inbox: &mut Inbox) {
        self.ctl.shell_seq += 1;
        let id = format!("shell-{}", self.ctl.shell_seq);
        self.ctl.phase = Phase::Shell;
        self.ctl.out.emit(&Event::ToolStarted { turn: String::new(), id: id.clone(), name: "bash".into(), args: json!({ "command": command }) });
        let started = Instant::now();
        let snap = self.snapshot();
        let out = self.ctl.out.clone();
        let (tx, rx) = mpsc::unbounded_channel();
        let work = stream_output(&out, "", &id, rx, self.runtime.shell(&command, Some(tx)));
        let (ctl, meta, dir, fsync) = (&mut self.ctl, &mut self.session.meta, self.session.dir.as_path(), self.session.fsync);
        let outcome = match drive(work, inbox, |input| on_busy_input(ctl, meta, dir, fsync, snap, input)).await {
            Ok(outcome) => outcome,
            Err(stop) => {
                self.ctl.on_stop(stop);
                ToolOutcome { duration_ms: started.elapsed().as_millis() as u64, ..ToolOutcome::err("interrupted") }
            }
        };
        let note = format!("$ {command}\n{}", outcome.text);
        self.ctl.out.emit(&tool_done("", id, "bash".into(), outcome));
        let r = self.session.note(&note);
        self.ctl.persisted(r);
        self.ctl.phase = Phase::Idle;
    }

    async fn compact_idle(&mut self, inbox: &mut Inbox) {
        if self.session.messages.len() < 2 {
            return self.ctl.warn("nothing to compact");
        }
        self.ctl.phase = Phase::Compacting;
        if let Err(stop) = self.compact(inbox, None).await {
            // Said out loud: whoever asked shows "compacting" until it hears how it ended.
            self.ctl.warn("compaction interrupted");
            self.ctl.on_stop(stop);
        }
        self.ctl.phase = Phase::Idle;
    }

    /// Replaces the history with a model-written summary (SPEC §3). The request is cut to fit the
    /// window first (`compaction_history`); when the provider still says it is too long, it is cut
    /// to half and asked again, and after that the history is cut mechanically — the one outcome a
    /// session must never have is a history too big to compact, which no later turn could get past.
    /// `Ok(false)` when it did not happen (nothing to compact, or the call failed for another
    /// reason — reported as a warning).
    async fn compact(&mut self, inbox: &mut Inbox, turn: Option<&str>) -> Result<bool, Stop> {
        if self.session.messages.len() < 2 {
            return Ok(false);
        }
        let from = self.estimated_input();
        let mut budget = compaction_budget(&self.session.meta);
        let mut summary = None;
        for _ in 0..=COMPACT_RETRIES {
            let history = compaction_history(&self.session.messages, budget);
            let sent = estimate_tokens(sent_chars(&history));
            match self.complete(inbox, turn.unwrap_or(""), Ask::Summary(history)).await? {
                Ok(resp) => {
                    self.record_usage(turn.unwrap_or(""), resp.usage);
                    summary = Some(match &resp.message {
                        Message::Assistant { text, .. } => text.trim().to_string(),
                        _ => String::new(),
                    });
                    break;
                }
                Err(e) if e.kind == ErrorKind::Context => budget = budget.min(sent) / 2,
                Err(e) => {
                    self.ctl.warn(format!("compaction failed: {e}"));
                    return Ok(false);
                }
            }
        }
        let summary = match summary {
            Some(s) if s.is_empty() => {
                self.ctl.warn("compaction failed: the model returned no summary");
                return Ok(false);
            }
            Some(s) => s,
            None => {
                // Worded without "compact": what follows is a real compaction (`compacted`), not
                // the failed one Mantra reads such a warning as.
                self.ctl.warn("the summary request stayed too long even cut down: the history was replaced by the first prompt and the last reply");
                mechanical_summary(&self.session.messages)
            }
        };
        let to = estimate_tokens(session::summary_message(&summary).sent_chars());
        let r = self.session.compact(&summary, from, to);
        self.ctl.persisted(r);
        self.session.meta.last_in = to;
        self.ctl.measured = self.session.messages.len();
        let r = self.session.save_meta();
        self.ctl.persisted(r);
        self.ctl.out.emit(&Event::Compacted { turn: turn.map(str::to_string), from, to });
        Ok(true)
    }

    /// Running totals into meta (saved), then the `usage` event. Called after the reply is pushed;
    /// `u` is the provider's count, or the api layer's estimate (`estimated`) when it gave none.
    fn record_usage(&mut self, turn: &str, u: Usage) {
        let m = &mut self.session.meta;
        m.usage_total.input = m.usage_total.input.saturating_add(u.input);
        m.usage_total.output = m.usage_total.output.saturating_add(u.output);
        m.usage_total.cached = m.usage_total.cached.saturating_add(u.cached);
        m.usage_total.estimated |= u.estimated;
        m.last_in = u.input;
        // The request covered everything but the reply just pushed.
        self.ctl.measured = self.session.messages.len().saturating_sub(1);
        self.ctl.out.emit(&Event::Usage {
            turn: turn.to_string(),
            last_in: u.input,
            last_out: u.output,
            total: m.usage_total.input.saturating_add(m.usage_total.output),
            cached: m.usage_total.cached,
            window: m.context_window,
            estimated: u.estimated,
        });
        let r = self.session.save_meta();
        self.ctl.persisted(r);
    }

    /// The input tokens the next request would cost: the last measurement plus an estimate of what
    /// was added since (or a full estimate when nothing was ever measured).
    fn estimated_input(&self) -> u64 {
        let m = &self.session.meta;
        let msgs = &self.session.messages;
        if m.last_in == 0 {
            return estimate_tokens(m.system.len() + sent_chars(msgs));
        }
        m.last_in + estimate_tokens(sent_chars(&msgs[self.ctl.measured.min(msgs.len())..]))
    }

    fn over_threshold(&self) -> bool {
        let m = &self.session.meta;
        self.session.messages.len() >= 2 && threshold(m.context_window, m.compact_at).is_some_and(|limit| self.estimated_input() > limit)
    }

    fn snapshot(&self) -> Snapshot {
        Snapshot { messages: self.session.messages.len(), jobs: self.runtime.running_jobs() }
    }

    /// Durable first: the transcript and the meta are on disk before anything else is touched
    /// (Mantra's quit path SIGKILLs bija a moment after `shutdown`). Then background jobs and
    /// leftover command groups are ended — SIGTERM, a 1 s grace, SIGKILL — and `exit` is the last
    /// line. The same for a graceful shutdown and a signal's crash-safe stop; only `code` differs.
    async fn shutdown(&mut self, code: i32) {
        let r = self.session.close();
        self.ctl.persisted(r);
        self.runtime.shutdown().await;
        self.ctl.out.emit(&Event::Exit { code });
    }
}

/// Polls `fut` while answering every input that arrives; `Err` when `on_input` asks to stop.
/// A closed inbox counts as EOF.
async fn drive<T>(fut: impl Future<Output = T>, inbox: &mut Inbox, mut on_input: impl FnMut(Input) -> Option<Stop>) -> Result<T, Stop> {
    tokio::pin!(fut);
    loop {
        tokio::select! {
            biased;
            out = &mut fut => return Ok(out),
            input = inbox.recv() => {
                if let Some(stop) = on_input(input.unwrap_or(Input::Eof)) {
                    return Err(stop);
                }
            }
        }
    }
}

/// Runs a tool future, turning its progress chunks into `tool_output` events; chunks still queued
/// when it finishes go out before its result.
async fn stream_output(out: &Out, turn: &str, id: &str, mut progress: mpsc::UnboundedReceiver<String>, work: impl Future<Output = ToolOutcome>) -> ToolOutcome {
    tokio::pin!(work);
    let emit = |text: String| out.emit(&Event::ToolOutput { turn: turn.to_string(), id: id.to_string(), text });
    loop {
        tokio::select! {
            biased;
            Some(chunk) = progress.recv() => emit(chunk),
            outcome = &mut work => {
                while let Ok(chunk) = progress.try_recv() {
                    emit(chunk);
                }
                return outcome;
            }
        }
    }
}

/// Answers an op that arrived while something is in flight (SPEC §3 rules).
fn on_busy_input(ctl: &mut Ctl, meta: &mut Meta, dir: &Path, fsync: Fsync, snap: Snapshot, input: Input) -> Option<Stop> {
    let op = match input {
        Input::Op(op) => op,
        Input::Bad(message) => {
            ctl.error(message);
            return None;
        }
        Input::Eof => return Some(Stop::Shutdown),
        Input::Terminate(sig) => {
            ctl.terminated.get_or_insert(sig);
            return Some(Stop::Terminate);
        }
    };
    match op {
        Op::Prompt { text } if text.trim().is_empty() => ctl.error("empty prompt"),
        Op::Prompt { text } => ctl.queue.push_back(text),
        // Outside a turn (an idle compaction, a `$ shell`) there is nothing to steer: it waits as a prompt.
        Op::Steer { text } if ctl.phase == Phase::Turn => ctl.steer.push(text),
        Op::Steer { text } => ctl.queue.push_back(text),
        Op::Interrupt => return Some(Stop::Interrupt),
        Op::Compact if ctl.phase == Phase::Compacting => {}
        Op::Compact => ctl.compact_requested = true,
        Op::Set { model, effort, name } => apply_set(ctl, meta, dir, fsync, model, effort, name),
        Op::Shell { .. } => ctl.error("busy"),
        Op::ToolResult { id, .. } => ctl.warn(format!("tool_result for unknown call id {id}: ignored")),
        Op::Status => ctl.out.emit(&status_event(ctl, meta, snap)),
        Op::Shutdown => return Some(Stop::Shutdown),
    }
    None
}

/// `Op::Set`: takes effect at the next model call; persisted right away.
fn apply_set(ctl: &mut Ctl, meta: &mut Meta, dir: &Path, fsync: Fsync, model: Option<String>, effort: Option<String>, name: Option<String>) {
    if let Some(model) = model {
        match model.trim() {
            "" => ctl.error("set: model must not be empty"),
            m => meta.model = m.to_string(),
        }
    }
    if let Some(effort) = effort {
        meta.effort = normalize_effort(&effort);
    }
    if let Some(name) = name {
        meta.name = name.trim().to_string();
    }
    let r = Session::save_meta_at(dir, meta, fsync == Fsync::Always);
    ctl.persisted(r);
}

fn status_event(ctl: &Ctl, meta: &Meta, snap: Snapshot) -> Event {
    Event::Status {
        session: meta.id.clone(),
        busy: ctl.phase != Phase::Idle,
        turn: ctl.turn.clone(),
        turns: meta.turns,
        model: meta.model.clone(),
        effort: meta.effort.clone(),
        messages: snap.messages,
        last_in: meta.last_in,
        total: meta.usage_total.input.saturating_add(meta.usage_total.output),
        cached: meta.usage_total.cached,
        window: meta.context_window,
        queued: ctl.queue.len(),
        jobs: snap.jobs,
    }
}

fn tool_done(turn: &str, id: String, name: String, o: ToolOutcome) -> Event {
    Event::ToolDone { turn: turn.to_string(), id, name, ok: o.ok, result: o.text, exit_code: o.exit_code, duration_ms: o.duration_ms, changes: o.changes }
}

pub fn turn_id(n: u64) -> String {
    format!("t{n}")
}

pub fn message_id(turn: &str, k: usize) -> String {
    format!("{turn}-m{k}")
}

pub fn reasoning_id(turn: &str, k: usize) -> String {
    format!("{turn}-r{k}")
}

pub fn fallback_call_id(turn: &str, k: usize) -> String {
    format!("{turn}-c{k}")
}

/// The auto-compaction limit in input tokens; `None` (never) for a zero window or percentage.
pub fn threshold(window: u64, compact_at: u8) -> Option<u64> {
    if window == 0 || compact_at == 0 {
        return None;
    }
    Some(window.saturating_mul(u64::from(compact_at.min(100))) / 100)
}

/// `""`/`none` mean "send no reasoning field"; anything else goes to the provider as given.
pub fn normalize_effort(effort: &str) -> String {
    match effort.trim().to_ascii_lowercase().as_str() {
        "" | "none" => String::new(),
        e => e.to_string(),
    }
}

/// Gives every call an id that is unique within its message (providers omit ids or reuse `call_0`)
/// and an empty object for "no arguments at all".
fn normalize_calls(turn: &str, counter: &mut usize, calls: &mut [ToolCall]) {
    let mut seen = HashSet::new();
    for c in calls.iter_mut() {
        *counter += 1;
        if c.id.trim().is_empty() || seen.contains(&c.id) {
            c.id = fallback_call_id(turn, *counter);
        }
        seen.insert(c.id.clone());
        if c.args.is_null() && c.raw.trim().is_empty() {
            c.args = json!({});
        }
    }
}

/// The model's argument text did not parse: it gets told, the tool never runs.
fn bad_arguments(call: &ToolCall) -> Option<String> {
    if !call.args.is_null() {
        return None;
    }
    Some(format!("{}: the arguments are not valid JSON; call the tool again with a JSON object. Received: {}", call.name, prefix(&call.raw, 300)))
}

/// The tokens a compaction request's messages may take: `COMPACT_FIT_PERCENT` of the window less
/// the system prompt, the request itself and the reply's own limit. Unlimited for an unknown window
/// (only the clipping applies then, and a `Context` error halves it from what was sent).
fn compaction_budget(meta: &Meta) -> u64 {
    if meta.context_window == 0 {
        return u64::MAX;
    }
    let fixed = estimate_tokens(meta.system.len() + SUMMARY_REQUEST.len()) + u64::from(meta.max_output_tokens.unwrap_or(0));
    (meta.context_window.saturating_mul(COMPACT_FIT_PERCENT) / 100).saturating_sub(fixed)
}

/// The history a compaction request sends: every message clipped (`for_summary`), then the oldest
/// left out until it fits `budget` — all but the first prompt, which is the goal — and the summary
/// request at the end. Sent without tools, so both wires render calls and results as plain text and
/// no cut can leave a result without its call; one that would open the kept part is dropped anyway.
fn compaction_history(messages: &[Message], budget: u64) -> Vec<Message> {
    let clipped: Vec<Message> = messages.iter().map(for_summary).collect();
    let head = usize::from(matches!(clipped.first(), Some(Message::User { .. })));
    let mut size = sent_chars(&clipped);
    let mut start = head;
    while start < clipped.len() && estimate_tokens(size) > budget {
        size -= clipped[start].sent_chars();
        start += 1;
    }
    while start < clipped.len() && start > head && matches!(clipped[start], Message::Tool { .. }) {
        start += 1;
    }
    let mut v = Vec::with_capacity(clipped.len() - start + head + 2);
    v.extend_from_slice(&clipped[..head]);
    if start > head {
        v.push(Message::User { text: format!("[{} earlier messages were left out here to fit the context window]", start - head) });
    }
    v.extend_from_slice(&clipped[start..]);
    v.push(Message::User { text: SUMMARY_REQUEST.to_string() });
    v
}

/// A message as a compaction request carries it: tool output, argument strings and long texts cut
/// to head and tail. Reasoning is never sent back, so it goes.
fn for_summary(m: &Message) -> Message {
    match m {
        Message::User { text } => Message::User { text: head_tail(text, CLIP_TEXT) },
        Message::Assistant { text, tool_calls, .. } => Message::Assistant {
            text: head_tail(text, CLIP_TEXT),
            reasoning: String::new(),
            tool_calls: tool_calls
                .iter()
                .map(|c| {
                    // Unparsable arguments only exist as `raw`: clip that text instead.
                    let (args, raw) = if c.args.is_null() { (Value::Null, head_tail(&c.raw, CLIP_ARG)) } else { (clip_strings(&c.args), String::new()) };
                    ToolCall { id: c.id.clone(), name: c.name.clone(), args, raw }
                })
                .collect(),
        },
        Message::Tool { call_id, name, text, is_error } => Message::Tool { call_id: call_id.clone(), name: name.clone(), text: head_tail(text, CLIP_TOOL), is_error: *is_error },
    }
}

/// Every string inside a JSON value cut to `CLIP_ARG` (a write_file's whole file content, say).
fn clip_strings(v: &Value) -> Value {
    match v {
        Value::String(s) => Value::String(head_tail(s, CLIP_ARG)),
        Value::Array(items) => Value::Array(items.iter().map(clip_strings).collect()),
        Value::Object(map) => Value::Object(map.iter().map(|(k, v)| (k.clone(), clip_strings(v))).collect()),
        other => other.clone(),
    }
}

/// `text` when it is at most `max` bytes, else its first and last `max / 2` bytes (on char
/// boundaries) around a note saying how much was left out.
fn head_tail(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let (head, tail) = (prefix(text, max / 2), suffix(text, max / 2));
    format!("{head}\n[… {} bytes left out …]\n{tail}", text.len() - head.len() - tail.len())
}

/// The summary when no request was short enough for one: the first prompt (the goal, or an
/// earlier summary) and the agent's last reply, each clipped, with a note saying the rest is gone.
fn mechanical_summary(messages: &[Message]) -> String {
    let first = messages.iter().find_map(|m| match m {
        Message::User { text } => Some(text.as_str()),
        _ => None,
    });
    let last = messages.iter().rev().find_map(|m| match m {
        Message::Assistant { text, .. } if !text.trim().is_empty() => Some(text.as_str()),
        _ => None,
    });
    format!(
        "The conversation grew too long to summarize, so it was cut mechanically: only the first prompt and your last reply are kept below. Check the state of the work (files, git status, running jobs) before you go on.\n\nFirst prompt:\n{}\n\nYour last reply:\n{}",
        head_tail(first.unwrap_or("(none)"), CLIP_TEXT),
        head_tail(last.unwrap_or("(none)"), CLIP_TEXT)
    )
}

/// bija's own part of the system prompt (SPEC §3). `notes` is `Runtime::system_notes()`: the
/// working directory, the sandbox mode and whether commands are wrapped.
pub fn preamble(notes: &str) -> String {
    let mut p = String::from(PREAMBLE_HEAD);
    p.push('\n');
    if !notes.trim().is_empty() {
        p.push_str(notes.trim());
        p.push('\n');
    }
    p.push_str(PREAMBLE_TOOLS);
    p
}

/// The full system prompt stored in meta: the preamble, then the caller's own part.
pub fn compose_system(preamble: &str, user: &str) -> String {
    match user.trim() {
        "" => preamble.trim_end().to_string(),
        user => format!("{}{SYSTEM_SEPARATOR}{user}", preamble.trim_end()),
    }
}

/// The caller's part of a stored system prompt (empty when there is none).
pub fn user_system(system: &str) -> &str {
    system.split_once(SYSTEM_SEPARATOR).map(|(_, user)| user).unwrap_or("")
}

/// Parses a `--tools-file` (SPEC §4.4): Mantra's dynamic-tool shape, `parameters` accepted for
/// `inputSchema`, and the nested `{"type":"function","function":{…}}` shape as well.
pub fn parse_tools(text: &str) -> Result<Vec<ToolDef>, String> {
    let v: Value = serde_json::from_str(text).map_err(|e| format!("not valid JSON: {e}"))?;
    let Value::Array(items) = v else {
        return Err("expected a JSON array of tool definitions".into());
    };
    items.iter().enumerate().map(|(i, item)| tool_def(item).ok_or_else(|| format!("entry {i} has no name"))).collect()
}

fn tool_def(item: &Value) -> Option<ToolDef> {
    let f = item.get("function").filter(|f| f.is_object()).unwrap_or(item);
    let name = f.get("name")?.as_str()?.trim();
    if name.is_empty() {
        return None;
    }
    let description = str_at(f, "description").to_string();
    let parameters = ["inputSchema", "parameters", "input_schema"]
        .iter()
        .find_map(|k| f.get(*k).filter(|p| p.is_object()))
        .cloned()
        .unwrap_or_else(|| json!({"type": "object", "properties": {}}));
    Some(ToolDef { name: name.to_string(), description, parameters })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::uuid_v4;
    use std::path::PathBuf;
    use std::sync::Arc;

    fn call(id: &str, name: &str, args: Value, raw: &str) -> ToolCall {
        ToolCall { id: id.into(), name: name.into(), args, raw: raw.into() }
    }

    /// A `Ctl` whose events can be read back, and a scratch dir for meta writes.
    fn ctl(phase: Phase) -> (Ctl, mpsc::Receiver<Arc<str>>, PathBuf) {
        let fanout = Fanout::default();
        let rx = fanout.subscribe();
        let dir = std::env::temp_dir().join(format!("bija-agent-test-{}", uuid_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let c = Ctl { out: Out::new(Sink::Discard, Some(fanout)), phase, turn: (phase == Phase::Turn).then(|| "t1".to_string()), queue: VecDeque::new(), steer: Vec::new(), compact_requested: false, shutdown: false, terminated: None, shell_seq: 0, measured: 0, disk_warned: false };
        (c, rx, dir)
    }

    fn events(rx: &mut mpsc::Receiver<Arc<str>>) -> Vec<Value> {
        let mut v = Vec::new();
        while let Ok(line) = rx.try_recv() {
            v.push(serde_json::from_str(&line).unwrap());
        }
        v
    }

    fn op(line: &str) -> Input {
        Input::from_line(line).unwrap()
    }

    const SNAP: Snapshot = Snapshot { messages: 4, jobs: 1 };

    #[test]
    fn ids_follow_the_protocol() {
        assert_eq!(turn_id(3), "t3");
        assert_eq!(message_id("t3", 1), "t3-m1");
        assert_eq!(reasoning_id("t3", 2), "t3-r2");
        assert_eq!(fallback_call_id("t3", 4), "t3-c4");
        let t = Turn::new(5, OpenTurn { assistant_messages: 2, tool_calls: 3 });
        assert_eq!((t.id.as_str(), t.messages, t.calls), ("t5", 2, 3), "a resumed turn numbers past what was emitted");
    }

    #[test]
    fn a_retry_after_streamed_output_moves_to_fresh_ids() {
        // Nothing streamed before the retry: the call keeps its id.
        let quiet = RoundIds::new(3);
        quiet.abandon_streamed();
        assert_eq!((quiet.streaming(), quiet.last(true)), (3, 3));
        // Text streamed, then the stream broke: the retry streams under the next id.
        let cut = RoundIds::new(3);
        assert_eq!(cut.streaming(), 3);
        cut.abandon_streamed();
        assert_eq!(cut.last(true), 4, "the reply takes the fresh id even when it streams nothing");
        assert_eq!(cut.last(false), 3, "a failed call ends past the last id it streamed under");
        assert_eq!(RoundIds::new(3).last(false), 2, "a call that failed silently takes no id");
    }

    #[test]
    fn threshold_is_a_percentage_of_the_window() {
        assert_eq!(threshold(200_000, 85), Some(170_000));
        assert_eq!(threshold(1000, 85), Some(850));
        assert_eq!(threshold(1000, 100), Some(1000));
        assert_eq!(threshold(1000, 250), Some(1000), "clamped to the window");
        assert_eq!(threshold(1000, 0), None, "0 % never compacts");
        assert_eq!(threshold(0, 85), None);
        assert_eq!(threshold(u64::MAX, 85), Some(u64::MAX / 100), "saturates instead of overflowing");
    }

    #[test]
    fn the_preamble_is_short_and_carries_the_runtime_notes() {
        let notes = "Working directory: /work\nSandbox: workspace-write — file tools may write only under /work.\nCommands run unsandboxed.";
        let p = preamble(notes);
        assert!(p.starts_with("You are a coding agent running in bija, a minimal harness."));
        assert!(p.contains("Working directory: /work") && p.contains("Sandbox: workspace-write"));
        assert!(p.contains("edit_file") && p.contains("background: true") && p.contains("tail"));
        assert!(p.lines().count() < 25, "{} lines", p.lines().count());
        assert!(!preamble("").contains("\n\n"), "no blank line where the notes would be");
    }

    #[test]
    fn the_system_prompt_splits_back_into_its_parts() {
        let pre = preamble("Working directory: /w");
        let full = compose_system(&pre, "  You are the planner.\n\n---\n\nKeep this rule.  ");
        assert!(full.starts_with(&pre));
        assert_eq!(user_system(&full), "You are the planner.\n\n---\n\nKeep this rule.");
        assert_eq!(compose_system(&pre, " "), pre);
        assert_eq!(user_system(&pre), "");
        // A new preamble with the old caller part: what a resume with a new --cwd does.
        let rebuilt = compose_system(&preamble("Working directory: /elsewhere"), user_system(&full));
        assert!(rebuilt.contains("/elsewhere") && rebuilt.ends_with("Keep this rule."));
    }

    #[test]
    fn calls_get_unique_ids_and_empty_arguments_become_an_object() {
        let mut n = 0;
        let mut calls = vec![call("", "bash", json!({"command": "ls"}), ""), call("call_0", "read_file", Value::Null, ""), call("call_0", "glob", json!({}), ""), call("x", "grep", Value::Null, "{\"pattern\":")];
        normalize_calls("t2", &mut n, &mut calls);
        let ids: Vec<&str> = calls.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, vec!["t2-c1", "call_0", "t2-c3", "x"]);
        assert_eq!(calls[1].args, json!({}));
        assert!(calls[3].args.is_null(), "unparsable arguments stay null so the model is told");
        assert_eq!(n, 4);
    }

    #[test]
    fn unparsable_arguments_become_an_error_for_the_model() {
        assert_eq!(bad_arguments(&call("c", "bash", json!({"command": "ls"}), "")), None);
        let msg = bad_arguments(&call("c", "edit_file", Value::Null, "{\"path\": \"a.rs\", \"old\": ")).unwrap();
        assert!(msg.starts_with("edit_file: the arguments are not valid JSON") && msg.contains("\"path\""), "{msg}");
        let long = "é".repeat(400);
        assert!(bad_arguments(&call("c", "x", Value::Null, &long)).unwrap().len() < 450, "clipped on a char boundary");
    }

    #[test]
    fn effort_none_means_no_reasoning_field() {
        assert_eq!(normalize_effort("none"), "");
        assert_eq!(normalize_effort(" "), "");
        assert_eq!(normalize_effort("High"), "high");
        assert_eq!(normalize_effort("minimal"), "minimal", "unknown levels pass through to the provider");
    }

    #[test]
    fn tools_files_parse_in_every_accepted_shape() {
        let text = r#"[
            {"type":"function","name":"mantra_spawn","description":"spawn","inputSchema":{"type":"object","properties":{"task":{"type":"string"}}}},
            {"type":"function","name":"mantra_wait","parameters":{"type":"object"}},
            {"type":"function","function":{"name":"nested","description":"d","parameters":{"type":"object","required":["a"]}}},
            {"name":"bare"}
        ]"#;
        let tools = parse_tools(text).unwrap();
        let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, vec!["mantra_spawn", "mantra_wait", "nested", "bare"]);
        assert_eq!(tools[0].parameters["properties"]["task"]["type"], "string");
        assert_eq!(tools[1].description, "");
        assert_eq!(tools[2].parameters["required"][0], "a");
        assert_eq!(tools[3].parameters, json!({"type": "object", "properties": {}}));
        assert!(parse_tools("{}").unwrap_err().contains("array"));
        assert!(parse_tools("[{\"description\":\"x\"}]").unwrap_err().contains("entry 0"));
        assert!(parse_tools("[oops").unwrap_err().contains("JSON"));
    }

    #[test]
    fn input_lines_parse_into_ops_or_errors() {
        assert!(Input::from_line("  \n").is_none());
        assert!(matches!(op(r#"{"op":"interrupt"}"#), Input::Op(Op::Interrupt)));
        assert!(matches!(op("{\"op\":\"prompt\"}"), Input::Bad(m) if m.starts_with("bad op")));
    }

    #[test]
    fn busy_ops_queue_steer_and_stop() {
        let (mut c, mut rx, dir) = ctl(Phase::Turn);
        let mut meta = Meta { id: "s".into(), model: "m1".into(), ..Meta::default() };
        let busy = |c: &mut Ctl, meta: &mut Meta, line: &str| on_busy_input(c, meta, &dir, Fsync::Never, SNAP, op(line));
        assert_eq!(busy(&mut c, &mut meta, r#"{"op":"prompt","text":"later"}"#), None);
        assert_eq!(busy(&mut c, &mut meta, r#"{"op":"steer","text":"now"}"#), None);
        assert_eq!(busy(&mut c, &mut meta, r#"{"op":"compact"}"#), None);
        assert_eq!((c.queue.len(), c.steer.as_slice(), c.compact_requested), (1, &["now".to_string()][..], true));
        assert_eq!(busy(&mut c, &mut meta, r#"{"op":"interrupt"}"#), Some(Stop::Interrupt));
        assert_eq!(busy(&mut c, &mut meta, r#"{"op":"shutdown"}"#), Some(Stop::Shutdown));
        assert_eq!(on_busy_input(&mut c, &mut meta, &dir, Fsync::Never, SNAP, Input::Eof), Some(Stop::Shutdown));
        assert_eq!(c.terminated, None, "a shutdown is no signal");
        // A signal stops too, but as itself: the first one's number is what the exit code reports.
        assert_eq!(on_busy_input(&mut c, &mut meta, &dir, Fsync::Never, SNAP, Input::Terminate(libc::SIGHUP)), Some(Stop::Terminate));
        assert_eq!(on_busy_input(&mut c, &mut meta, &dir, Fsync::Never, SNAP, Input::Terminate(libc::SIGTERM)), Some(Stop::Terminate));
        assert_eq!(c.terminated, Some(libc::SIGHUP));
        assert!(events(&mut rx).is_empty(), "none of these is echoed");

        // Outside a turn a steer has nothing to steer and waits as a prompt.
        c.phase = Phase::Shell;
        busy(&mut c, &mut meta, r#"{"op":"steer","text":"queued"}"#);
        assert_eq!(c.queue.back().map(String::as_str), Some("queued"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn busy_ops_that_answer_with_an_event() {
        let (mut c, mut rx, dir) = ctl(Phase::Turn);
        let mut meta = Meta { id: "s".into(), model: "m1".into(), turns: 1, context_window: 1000, ..Meta::default() };
        for line in [r#"{"op":"shell","command":"ls"}"#, r#"{"op":"tool_result","id":"nope","text":"x"}"#, r#"{"op":"status"}"#, "garbage", r#"{"op":"prompt","text":"  "}"#] {
            assert_eq!(on_busy_input(&mut c, &mut meta, &dir, Fsync::Never, SNAP, op(line)), None);
        }
        let ev = events(&mut rx);
        assert_eq!(ev[0], json!({"ev": "error", "message": "busy"}));
        assert_eq!(ev[1]["ev"], "warning");
        assert!(ev[1]["message"].as_str().unwrap().contains("nope"));
        assert_eq!((ev[2]["ev"].as_str(), ev[2]["busy"].as_bool(), ev[2]["turn"].as_str()), (Some("status"), Some(true), Some("t1")));
        assert_eq!((ev[2]["messages"].as_u64(), ev[2]["jobs"].as_u64(), ev[2]["window"].as_u64()), (Some(4), Some(1), Some(1000)));
        assert_eq!(ev[3]["ev"], "error");
        assert_eq!(ev[4], json!({"ev": "error", "message": "empty prompt"}));
        assert!(c.queue.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn set_applies_to_meta_and_is_saved_at_once() {
        let (mut c, mut rx, dir) = ctl(Phase::Turn);
        let mut meta = Meta { id: "s".into(), model: "m1".into(), effort: "high".into(), ..Meta::default() };
        on_busy_input(&mut c, &mut meta, &dir, Fsync::Never, SNAP, op(r#"{"op":"set","model":"m2","effort":"none","name":" worker 3 "}"#));
        assert_eq!((meta.model.as_str(), meta.effort.as_str(), meta.name.as_str()), ("m2", "", "worker 3"));
        let saved: Meta = serde_json::from_str(&std::fs::read_to_string(dir.join("session.json")).unwrap()).unwrap();
        assert_eq!(saved.model, "m2");
        on_busy_input(&mut c, &mut meta, &dir, Fsync::Never, SNAP, op(r#"{"op":"set","model":" "}"#));
        assert_eq!(meta.model, "m2", "an empty model is refused");
        assert_eq!(events(&mut rx)[0]["ev"], "error");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failed_write_warns_once_until_one_succeeds() {
        let (mut c, mut rx, dir) = ctl(Phase::Idle);
        c.persisted::<()>(Err(io::Error::other("disk full")));
        c.persisted::<()>(Err(io::Error::other("disk full")));
        assert_eq!(events(&mut rx).len(), 1);
        c.persisted(Ok(()));
        c.persisted::<()>(Err(io::Error::other("disk full")));
        assert_eq!(events(&mut rx).len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_history_that_fits_is_sent_whole_with_the_summary_request() {
        let m = vec![
            Message::User { text: "abcd".into() },
            Message::Assistant { text: "ef".into(), reasoning: "not sent back".into(), tool_calls: vec![call("c", "bash", json!({"a": 1}), "")] },
            Message::Tool { call_id: "c".into(), name: "bash".into(), text: "xyz".into(), is_error: false },
        ];
        let summary = compaction_history(&m, u64::MAX);
        assert_eq!(summary.len(), 4);
        assert!(matches!(summary.last(), Some(Message::User { text }) if text.contains("hand-off summary")));
    }

    #[test]
    fn head_tail_keeps_both_ends_on_char_boundaries() {
        assert_eq!(head_tail("short", 10), "short");
        let cut = head_tail(&format!("{}{}", "é".repeat(100), "z".repeat(100)), 50);
        assert!(cut.starts_with("éé") && cut.ends_with("zzz") && cut.contains("bytes left out"), "{cut}");
        assert!(cut.len() < 120, "{}", cut.len());
    }

    /// A history that overflowed the window — one round of big tool outputs — still makes a
    /// compaction request that fits: outputs and argument strings clipped, the oldest messages
    /// left out (never the first prompt), and the summary request last.
    #[test]
    fn the_compaction_request_is_cut_to_fit_the_window() {
        let big = "x".repeat(60 * 1024);
        let mut m = vec![Message::User { text: "the goal".into() }];
        for i in 0..6 {
            m.push(Message::Assistant { text: format!("step {i}"), reasoning: "r".repeat(10_000), tool_calls: vec![call(&format!("c{i}"), "write_file", json!({"path": "a.rs", "content": big}), "")] });
            m.push(Message::Tool { call_id: format!("c{i}"), name: "read_file".into(), text: big.clone(), is_error: false });
        }
        assert!(estimate_tokens(sent_chars(&m)) > 180_000);
        let all = compaction_history(&m, u64::MAX);
        assert_eq!(all.len(), m.len() + 1, "nothing left out when it fits");
        assert!(estimate_tokens(sent_chars(&all)) < 10_000, "clipped: {}", estimate_tokens(sent_chars(&all)));
        match &all[1] {
            Message::Assistant { reasoning, tool_calls, .. } => {
                assert!(reasoning.is_empty());
                let content = tool_calls[0].args["content"].as_str().unwrap();
                assert!(content.len() < CLIP_ARG + 100 && content.contains("bytes left out"), "{}", content.len());
                assert_eq!(tool_calls[0].args["path"], "a.rs");
            }
            other => panic!("{other:?}"),
        }
        // A budget for about four messages: the first prompt stays, the oldest go, no result is
        // left without its call, and the cut says so.
        let budget = estimate_tokens(all[1].sent_chars() + all[2].sent_chars()) * 2 + 10;
        let fit = compaction_history(&m, budget);
        assert!(estimate_tokens(sent_chars(&fit)) <= budget + estimate_tokens(SUMMARY_REQUEST.len() + 100), "{}", estimate_tokens(sent_chars(&fit)));
        assert!(matches!(&fit[0], Message::User { text } if text == "the goal"));
        assert!(matches!(&fit[1], Message::User { text } if text.contains("earlier messages were left out")));
        assert!(matches!(&fit[2], Message::Assistant { .. }), "{:?}", fit[2]);
        assert!(matches!(fit.last(), Some(Message::User { text }) if text == SUMMARY_REQUEST));
        // The budget follows the window, the system prompt and the reply limit.
        let meta = Meta { context_window: 100_000, system: "s".repeat(4000), max_output_tokens: Some(8000), ..Meta::default() };
        assert_eq!(compaction_budget(&meta), 80_000 - estimate_tokens(4000 + SUMMARY_REQUEST.len()) - 8000);
        assert_eq!(compaction_budget(&Meta { context_window: 0, ..Meta::default() }), u64::MAX);
    }

    #[test]
    fn the_mechanical_summary_keeps_the_goal_and_the_last_reply() {
        let m = vec![
            Message::User { text: "build the parser".into() },
            Message::Assistant { text: "first".into(), reasoning: String::new(), tool_calls: vec![] },
            Message::Assistant { text: "parser done, tests next".into(), reasoning: String::new(), tool_calls: vec![call("c", "bash", json!({}), "")] },
            Message::Tool { call_id: "c".into(), name: "bash".into(), text: "ok".into(), is_error: false },
        ];
        let s = mechanical_summary(&m);
        assert!(s.contains("build the parser") && s.contains("parser done, tests next") && !s.contains("first\n"), "{s}");
        assert!(mechanical_summary(&[]).contains("(none)"));
    }

    /// An interrupt that raced the end of a turn waits in the inbox: idle answers it (a no-op)
    /// before the queued prompt starts, instead of the next turn's first wait taking it as its own.
    #[tokio::test]
    async fn a_stale_interrupt_does_not_end_the_next_queued_turn() {
        let home = std::env::temp_dir().join(format!("bija-agent-idle-{}", uuid_v4()));
        // A provider that accepts and never answers: the queued turn stays in its model call.
        let silent = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/v1", silent.local_addr().unwrap());
        let meta = Meta { id: "idle".into(), cwd: home.clone(), provider_url: url.clone(), model: "m".into(), ..Meta::default() };
        std::fs::create_dir_all(&home).unwrap();
        let session = Session::create(&home, meta, Fsync::Never).unwrap();
        let client = Client::new(crate::api::ClientConfig { base_url: url, wire: Default::default(), api_key: None, max_retries: 0, timeout: std::time::Duration::from_secs(30), stream: true, user_agent: "t".into() });
        let cfg = crate::tools::RuntimeConfig { cwd: home.clone(), sandbox: crate::tools::Sandbox::WorkspaceWrite, writable: vec![], network: false, no_bwrap: true, jobs_dir: home.join("jobs"), scrub_env: vec![] };
        let runtime = Runtime::new(cfg).await;
        let fanout = Fanout::default();
        let mut lines = fanout.subscribe();
        let mut agent = Agent::new(session, client, runtime, vec![], Out::new(Sink::Discard, Some(fanout)));
        agent.queue_prompt("next".into());
        let (tx, mut inbox) = mpsc::unbounded_channel();
        tx.send(op(r#"{"op":"interrupt"}"#)).unwrap();
        tx.send(op(r#"{"op":"status"}"#)).unwrap();
        // Once the turn is under way, end it the normal way.
        let watcher = tokio::spawn(async move {
            let mut seen = Vec::new();
            while let Some(line) = lines.recv().await {
                let ev: Value = serde_json::from_str(&line).unwrap();
                if ev["ev"] == "turn_started" {
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    tx.send(op(r#"{"op":"interrupt"}"#)).unwrap();
                }
                let done = ev["ev"] == "turn_done";
                seen.push(ev);
                if done {
                    return seen;
                }
            }
            seen
        });
        tokio::time::timeout(std::time::Duration::from_secs(10), agent.idle(&mut inbox, true)).await.expect("idle returns once the queue is worked off");
        let seen = tokio::time::timeout(std::time::Duration::from_secs(5), watcher).await.unwrap().unwrap();
        let names: Vec<&str> = seen.iter().map(|e| e["ev"].as_str().unwrap_or("")).collect();
        assert_eq!(names, ["status", "turn_started", "turn_done"], "{seen:?}");
        assert_eq!(seen[0]["busy"], false, "the status was answered while idle, before the turn");
        assert_eq!(seen[2]["status"], "interrupted");
        drop(silent);
        let _ = std::fs::remove_dir_all(&home);
    }

    #[tokio::test]
    async fn drive_answers_ops_while_the_future_runs() {
        let (tx, mut inbox) = mpsc::unbounded_channel();
        tx.send(op(r#"{"op":"status"}"#)).unwrap();
        tx.send(op(r#"{"op":"interrupt"}"#)).unwrap();
        let mut seen = 0;
        let r = drive(std::future::pending::<()>(), &mut inbox, |i| {
            seen += 1;
            matches!(i, Input::Op(Op::Interrupt)).then_some(Stop::Interrupt)
        })
        .await;
        assert_eq!((r, seen), (Err(Stop::Interrupt), 2));
        // A finished future wins over queued input, and a closed inbox is EOF.
        tx.send(op(r#"{"op":"status"}"#)).unwrap();
        assert_eq!(drive(async { 7 }, &mut inbox, |_| None).await, Ok(7));
        drop(tx);
        let r = drive(std::future::pending::<()>(), &mut inbox, |i| matches!(i, Input::Eof).then_some(Stop::Shutdown)).await;
        assert_eq!(r, Err(Stop::Shutdown));
    }

    #[tokio::test]
    async fn tool_output_reaches_the_sink_before_the_result() {
        let fanout = Fanout::default();
        let mut lines = fanout.subscribe();
        let out = Out::new(Sink::Discard, Some(fanout));
        let (tx, rx) = mpsc::unbounded_channel();
        let work = async move {
            tx.send("a".to_string()).unwrap();
            tokio::task::yield_now().await;
            tx.send("b".to_string()).unwrap();
            ToolOutcome::ok("done")
        };
        let outcome = stream_output(&out, "t1", "c1", rx, work).await;
        assert_eq!(outcome.text, "done");
        let texts: Vec<String> = std::iter::from_fn(|| lines.try_recv().ok()).map(|l| serde_json::from_str::<Value>(&l).unwrap()["text"].as_str().unwrap().to_string()).collect();
        assert_eq!(texts, vec!["a", "b"]);
    }
}
