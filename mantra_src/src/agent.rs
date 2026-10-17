//! Agent state, built by folding Codex app-server notifications.

use crate::hub::AgentId;
use ratatui::text::Line;
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::time::{Duration, Instant};

const MAX_ITEMS: usize = 3000;
const MAX_OUTPUT: usize = 24_000;

#[derive(Debug, Clone, PartialEq)]
pub enum Level {
    Info,
    Warn,
    Error,
    Ok,
}

#[derive(Debug, Clone)]
pub struct FileChange {
    pub path: String,
    pub kind: String, // add | update | delete
    pub diff: String,
}

#[derive(Debug, Clone)]
pub enum Kind {
    User,
    Agent,
    Reasoning,
    Plan,
    Command { cmd: String, output: String, exit: Option<i64>, status: String, dur_ms: Option<u64> },
    Files { changes: Vec<FileChange>, status: String },
    Tool { name: String, args: String, result: String, status: String },
    Web { query: String },
    Notice { level: Level },
    Compaction { from: u64, to: Option<u64> },
}

pub struct Item {
    pub id: String,
    pub kind: Kind,
    pub text: String,
    pub done: bool,
    pub version: u64,
    pub expanded: bool,
    /// When the item was first seen in progress — a command's duration when the backend's
    /// completion event doesn't carry one (or carries 0).
    pub started: Option<Instant>,
    pub cache: Option<(u16, u64, bool, Vec<Line<'static>>)>,
}

impl Item {
    fn new(id: impl Into<String>, kind: Kind, text: impl Into<String>) -> Item {
        Item { id: id.into(), kind, text: text.into(), done: false, version: 0, expanded: false, started: None, cache: None }
    }
    fn touch(&mut self) {
        self.version += 1;
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Status {
    Starting,
    Idle,
    Busy,
    Waiting,
    Retrying(String),
    Failed(String),
    Crashed(String),
    Stopped,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ErrKind {
    Transient,
    ContextFull,
    UsageLimit,
    Auth,
    /// Deterministic provider/model incompatibility (HTTP 400/422, "unexpected message role", …).
    /// Never retried — the same request will fail again.
    ProviderRejected,
    Other,
}

#[derive(Debug, Clone, Default)]
pub struct FileStat {
    pub adds: usize,
    pub dels: usize,
    pub kind: String,
    pub diff: String,
}

#[derive(Debug, Clone)]
pub enum Signal {
    TurnDone { status: String, error: Option<String>, kind: Option<ErrKind> },
    FilesChanged(Vec<String>),
    Activity,
    /// A finished `commandExecution` item's output looks like a broken sandbox (WP12.4/L1, see
    /// `looks_like_broken_sandbox`): the sandbox itself cannot run commands on this host. Carries
    /// the offending output, truncated.
    EnvironmentBroken(String),
}

/// True when `text` names a sandbox that failed to start at all — bubblewrap, user namespaces,
/// AppArmor's restriction knob, or "sandbox" paired with a failure word — as opposed to a role's
/// own read-only limit refusing a write ("failed to write file"), which is not a broken
/// environment but the role's own scope. Shared by the `EnvironmentBroken` signal below and by
/// `ask_up`'s pre-routing check, so both halt on the same, single definition of "broken".
pub fn looks_like_broken_sandbox(text: &str) -> bool {
    let hay = text.to_lowercase();
    // Only the sandbox's own failure markers count: an agent that merely says "I cannot do X in
    // this sandbox" is asking a question, not reporting a machine that cannot run commands.
    hay.contains("bwrap") || hay.contains("user namespace") || hay.contains("unprivileged_userns") || hay.contains("apparmor_restrict")
}

pub struct Agent {
    pub id: AgentId,
    pub name: String,
    pub role: String,
    pub glyph: String,
    pub color: String,
    pub model_alias: String,
    pub model: String,
    /// Provider id this agent's model runs on (set at spawn) — used to name the provider in
    /// halt messages (e.g. `ProviderRejected`).
    pub provider: String,
    /// Which runtime this agent's process is (Codex app-server or Claude Code) — for wording only;
    /// every protocol-level difference is handled in `hub`.
    pub backend: crate::config::ProviderKind,
    pub effort: String,
    pub cwd: PathBuf,
    /// Codex approval policy this agent was spawned with ("never" | "on-request" | "untrusted").
    /// Drives auto-approval independently of the global Solo mode (`Settings.approval_mode`).
    pub approval: String,
    pub thread_id: Option<String>,
    pub status: Status,
    pub items: Vec<Item>,
    index: HashMap<String, usize>,
    pub turn_active: bool,
    pub awaiting_start: bool,
    /// A compaction is running (manual or automatic).
    pub compacting: bool,
    /// /compact was requested mid-turn; runs when the turn ends.
    pub compact_pending: bool,
    compact_from: u64,
    compact_item: Option<String>,
    /// (context tokens before, when) — drives the draining-gauge animation.
    pub ctx_anim: Option<(u64, Instant)>,
    pub ctx_warned: bool,
    pub turn_started: Option<Instant>,
    /// When the turn now in flight began waiting for its first output — the prompt going out, or
    /// `turn/started` for a turn Mantra did not send itself. Cleared by the first real output
    /// (a token count that grew, a delta, a non-echo item) and when the turn ends, so `Some` means
    /// "active turn, nothing back yet": the silent / "Reconnecting… n/5" state `last_event` cannot
    /// show, since the retry notices themselves move it. See `first_token_wait`.
    pub turn_wait_since: Option<Instant>,
    pub turn_count: u32,
    pub plan: Vec<(String, String)>,
    pub files: BTreeMap<String, FileStat>,
    /// Codex's `total.totalTokens`: every request's whole input plus its output, summed over the
    /// thread — so a worker that cycles tools in a big context reads millions.
    pub tokens_total: u64,
    /// How much of `tokens_total` was input served from the cache (`total.cachedInputTokens`):
    /// the same prefix re-read on every request. Shown next to the total when it is most of it,
    /// so a big number is read as what it is (see `cached_share`).
    pub tokens_cached: u64,
    pub ctx_used: u64,
    pub ctx_window: Option<u64>,
    pub last_event: Instant,
    pub activity: String,
    pub final_message: Option<String>,
    pub queued: Vec<String>,
    /// The user interrupted this agent's turn by hand (ctrl+c, or `x` on the stage). A deliberate
    /// stop: nothing may restart it on its own — no planner nudge, no gate round, no transient
    /// retry, no "continue where you left off" after a process restart. Cleared the moment any
    /// message is sent to it again (`app::prompt_agent`), which is the user's way of saying carry on.
    pub stopped_by_user: bool,
    /// The hub is relaunching this agent's process at Mantra's own request (a model/effort change,
    /// a plain restart): the hub holds and replays any turn sent meanwhile, so the next `Ready`
    /// must not also send a "continue where you left off" note. Cleared on `Ready`.
    pub relaunching: bool,
    pub created: Instant,
    pub finished: Option<Instant>,
    pub scroll: usize,
    pub follow: bool,
    pub retry_note: Option<String>,
    turn_first_item: usize,
    notice_seq: u64,
    /// Items dropped from the front of `items` so far (the MAX_ITEMS trim). `trimmed + i` is a
    /// stable ordinal for `items[i]` that survives trims — the web UI keys transcripts by it.
    pub trimmed: u64,
}

impl Agent {
    pub fn new(id: AgentId, name: &str, role: &str, cwd: PathBuf) -> Agent {
        Agent {
            id,
            name: name.to_string(),
            role: role.to_string(),
            glyph: "●".into(),
            color: "text".into(),
            model_alias: String::new(),
            model: String::new(),
            provider: String::new(),
            backend: crate::config::ProviderKind::Codex,
            effort: "medium".into(),
            cwd,
            approval: "never".into(),
            thread_id: None,
            status: Status::Starting,
            items: vec![],
            index: HashMap::new(),
            turn_active: false,
            awaiting_start: false,
            compacting: false,
            compact_pending: false,
            compact_from: 0,
            compact_item: None,
            ctx_anim: None,
            ctx_warned: false,
            turn_started: None,
            turn_wait_since: None,
            turn_count: 0,
            plan: vec![],
            files: BTreeMap::new(),
            tokens_total: 0,
            tokens_cached: 0,
            ctx_used: 0,
            ctx_window: None,
            last_event: Instant::now(),
            activity: "starting".into(),
            final_message: None,
            queued: vec![],
            stopped_by_user: false,
            relaunching: false,
            created: Instant::now(),
            finished: None,
            scroll: 0,
            follow: true,
            retry_note: None,
            turn_first_item: 0,
            notice_seq: 0,
            trimmed: 0,
        }
    }

    /// Ordinal of `items[0]` (see `trimmed`).
    pub fn items_first_ord(&self) -> u64 {
        self.trimmed
    }

    /// One past the ordinal of the last item.
    pub fn items_total_ord(&self) -> u64 {
        self.trimmed + self.items.len() as u64
    }

    pub fn busy(&self) -> bool {
        self.turn_active || self.awaiting_start
    }

    /// How long the turn in flight has produced nothing at all (no tokens, no text, no tool
    /// activity) — `None` when there is no such turn, or when silence is expected: a hand stop,
    /// an approval being waited on, a compaction.
    pub fn first_token_wait(&self) -> Option<Duration> {
        if !self.busy() || self.stopped_by_user || self.compacting || self.status == Status::Waiting {
            return None;
        }
        self.turn_wait_since.map(|t| t.elapsed())
    }

    /// Percent of the context window a token count represents.
    pub fn pct_of(&self, tokens: u64) -> Option<u8> {
        let w = self.ctx_window.filter(|w| *w > 0)?;
        Some(((tokens.saturating_mul(100)) / w).min(100) as u8)
    }

    pub fn ctx_percent(&self) -> Option<u8> {
        let w = self.ctx_window?;
        if w == 0 {
            return None;
        }
        Some(((self.ctx_used as f64 / w as f64) * 100.0).min(100.0) as u8)
    }

    /// "5.9M cached" when the cached share of this agent's total is worth a word (`cached_share`).
    pub fn cached_share(&self) -> Option<String> {
        cached_share(self.tokens_total, self.tokens_cached)
    }

    pub fn plan_progress(&self) -> (usize, usize) {
        let done = self.plan.iter().filter(|(_, s)| s == "completed").count();
        (done, self.plan.len())
    }

    pub fn push_user(&mut self, text: &str) {
        let id = format!("local-user-{}", self.items.len());
        self.push(Item::new(id, Kind::User, text));
    }

    pub fn notice(&mut self, level: Level, text: impl Into<String>) {
        let text = text.into();
        // Codex reports a failure twice (the `error` event, then the failed turn): keep one.
        if let Some(last) = self.items.last_mut() {
            if let Kind::Notice { level: l } = &last.kind {
                if *l == level && !last.text.is_empty() && (text.contains(last.text.as_str()) || last.text.contains(text.as_str())) {
                    if text.len() > last.text.len() {
                        last.text = text;
                        last.version += 1;
                    }
                    return;
                }
            }
        }
        self.notice_seq += 1;
        let id = format!("notice-{}", self.notice_seq);
        let mut it = Item::new(id, Kind::Notice { level }, text);
        it.done = true;
        self.push(it);
    }

    /// A muted transcript line about the backend's own housekeeping rather than this agent's
    /// work (`is_housekeeping_warning`, the fallback-metadata note): an info notice that the
    /// card and peek lines skip (`tail_lines`), said once per session however often the backend
    /// repeats it. Its id prefix is what marks it.
    fn footnote(&mut self, text: String) {
        if self.items.iter().any(|i| i.id.starts_with(FOOTNOTE) && i.text == text) {
            return;
        }
        self.notice_seq += 1;
        let mut it = Item::new(format!("{FOOTNOTE}{}", self.notice_seq), Kind::Notice { level: Level::Info }, text);
        it.done = true;
        self.push(it);
    }

    fn push(&mut self, item: Item) {
        if self.items.len() >= MAX_ITEMS {
            let drop = MAX_ITEMS / 5;
            self.items.drain(..drop);
            self.trimmed += drop as u64;
            self.turn_first_item = self.turn_first_item.saturating_sub(drop);
            self.index.clear();
            for (i, it) in self.items.iter().enumerate() {
                self.index.insert(it.id.clone(), i);
            }
        }
        self.index.insert(item.id.clone(), self.items.len());
        self.items.push(item);
    }

    fn get_or_insert(&mut self, id: &str, kind: Kind) -> &mut Item {
        if let Some(&i) = self.index.get(id) {
            if i < self.items.len() {
                return &mut self.items[i];
            }
        }
        self.push(Item::new(id, kind, ""));
        let i = self.items.len() - 1;
        &mut self.items[i]
    }

    fn rel(&self, p: &str) -> String {
        let cwd = self.cwd.to_string_lossy();
        p.strip_prefix(cwd.as_ref()).map(|s| s.trim_start_matches('/').to_string()).unwrap_or_else(|| p.to_string())
    }

    /// Fold one notification into state. Returns signals the app/engine care about.
    pub fn apply(&mut self, method: &str, p: &Value) -> Vec<Signal> {
        self.last_event = Instant::now();
        let mut out = vec![];
        let s = |v: &Value, k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string();
        match method {
            "thread/status/changed" => {
                let st = p.pointer("/status/type").and_then(|x| x.as_str()).unwrap_or("");
                let waiting = p
                    .pointer("/status/activeFlags")
                    .and_then(|f| f.as_array())
                    .map(|a| a.iter().any(|x| x.as_str().map(|s| s.starts_with("waiting")).unwrap_or(false)))
                    .unwrap_or(false);
                if st == "active" && waiting {
                    self.status = Status::Waiting;
                    self.activity = "waiting for approval".into();
                } else if st == "active" && self.status == Status::Waiting {
                    self.status = Status::Busy;
                } else if st == "systemError" {
                    self.status = Status::Failed("system error".into());
                }
            }
            "turn/started" => {
                // A turn we sent has been waiting since the prompt went out; one we did not send
                // (a replay, a steer that became a turn) starts waiting now.
                self.turn_wait_since = if self.awaiting_start { self.turn_wait_since.or_else(|| Some(Instant::now())) } else { Some(Instant::now()) };
                self.turn_active = true;
                self.awaiting_start = false;
                self.turn_started = Some(Instant::now());
                self.turn_count += 1;
                self.status = Status::Busy;
                self.activity = "thinking".into();
                self.final_message = None;
                self.retry_note = None;
                self.turn_first_item = self.items.len();
                self.compact_item = None;
            }
            "item/started" | "item/completed" => {
                // The echo of our own prompt is not the model speaking.
                if p.pointer("/item/type").and_then(|x| x.as_str()) != Some("userMessage") {
                    self.turn_wait_since = None;
                }
                if let Some(item) = p.get("item") {
                    let done = method == "item/completed";
                    if let Some(sig) = self.apply_item(item, done) {
                        out.push(sig);
                    }
                }
                out.push(Signal::Activity);
            }
            // Claude backend liveness tick (`hub/claude.rs`): `last_event` moved above; only the
            // label, if any, changes here.
            "mantra/activity" => {
                if let Some(l) = p.get("activity").and_then(|x| x.as_str()) {
                    self.activity = l.into();
                }
                out.push(Signal::Activity);
            }
            "item/agentMessage/delta" => {
                self.turn_wait_since = None;
                let id = s(p, "itemId");
                let d = s(p, "delta");
                let it = self.get_or_insert(&id, Kind::Agent);
                it.text.push_str(&d);
                it.touch();
                self.activity = "writing".into();
            }
            "item/plan/delta" => {
                self.turn_wait_since = None;
                let id = s(p, "itemId");
                let d = s(p, "delta");
                let it = self.get_or_insert(&id, Kind::Plan);
                it.text.push_str(&d);
                it.touch();
            }
            "item/reasoning/summaryTextDelta" => {
                self.turn_wait_since = None;
                let id = s(p, "itemId");
                let d = s(p, "delta");
                let it = self.get_or_insert(&id, Kind::Reasoning);
                it.text.push_str(&d);
                it.touch();
                self.activity = "thinking".into();
            }
            "item/reasoning/summaryPartAdded" => {
                let id = s(p, "itemId");
                let it = self.get_or_insert(&id, Kind::Reasoning);
                if !it.text.is_empty() {
                    it.text.push_str("\n\n");
                }
                it.touch();
            }
            "item/commandExecution/outputDelta" => {
                self.turn_wait_since = None;
                let id = s(p, "itemId");
                let d = s(p, "delta");
                let it = self.get_or_insert(&id, Kind::Command { cmd: String::new(), output: String::new(), exit: None, status: "inProgress".into(), dur_ms: None });
                it.started.get_or_insert_with(Instant::now);
                if let Kind::Command { output, .. } = &mut it.kind {
                    output.push_str(&d);
                    crate::util::tail_bytes(output, MAX_OUTPUT);
                }
                it.touch();
            }
            "turn/plan/updated" => {
                if let Some(arr) = p.get("plan").and_then(|x| x.as_array()) {
                    self.plan = arr.iter().map(|st| (s(st, "step"), s(st, "status"))).collect();
                }
            }
            "turn/diff/updated" => {
                let diff = s(p, "diff");
                for (path, chunk) in split_diff(&diff) {
                    let rel = self.rel(&path);
                    let (a, d) = crate::util::diff_stats(&chunk);
                    let e = self.files.entry(rel).or_default();
                    e.adds = a;
                    e.dels = d;
                    e.diff = chunk;
                    if e.kind.is_empty() {
                        e.kind = "update".into();
                    }
                }
            }
            "thread/tokenUsage/updated" => {
                if let Some(t) = p.get("tokenUsage") {
                    let before = self.tokens_total;
                    self.tokens_total = t.pointer("/total/totalTokens").and_then(|x| x.as_u64()).unwrap_or(self.tokens_total);
                    self.tokens_cached = t.pointer("/total/cachedInputTokens").and_then(|x| x.as_u64()).unwrap_or(self.tokens_cached);
                    if self.tokens_total > before {
                        self.turn_wait_since = None;
                    }
                    let last_in = t.pointer("/last/inputTokens").and_then(|x| x.as_u64()).unwrap_or(0);
                    let last_out = t.pointer("/last/outputTokens").and_then(|x| x.as_u64()).unwrap_or(0);
                    if last_in + last_out > 0 {
                        self.ctx_used = last_in + last_out;
                    }
                    if let Some(w) = t.get("modelContextWindow").and_then(|x| x.as_u64()) {
                        self.ctx_window = Some(w);
                    }
                    if let Some(cid) = self.compact_item.clone() {
                        let now_ctx = self.ctx_used;
                        if let Some(it) = self.items.iter_mut().rev().find(|i| i.id == cid) {
                            if let Kind::Compaction { from, to } = &mut it.kind {
                                if now_ctx < *from {
                                    *to = Some(now_ctx);
                                    let f = *from;
                                    it.touch();
                                    self.ctx_anim = Some((f, Instant::now()));
                                    self.compact_item = None;
                                }
                            }
                        }
                    }
                }
            }
            "error" => {
                let msg = pretty_err(p.pointer("/error/message").and_then(|x| x.as_str()).unwrap_or("error"));
                let details = p.pointer("/error/additionalDetails").and_then(|x| x.as_str()).unwrap_or("");
                let will_retry = p.get("willRetry").and_then(|x| x.as_bool()).unwrap_or(false);
                if will_retry {
                    self.status = Status::Retrying(msg.clone());
                    self.retry_note = Some(format!("{msg} {}", crate::util::trunc(details, 120)));
                    self.activity = msg;
                } else {
                    let text = if details.is_empty() { msg } else { format!("{msg} — {}", crate::util::trunc(details, 300)) };
                    self.notice(Level::Error, text);
                }
            }
            "warning" | "guardianWarning" | "deprecationNotice" => {
                let msg = p.get("message").or_else(|| p.get("summary")).and_then(|x| x.as_str()).unwrap_or("");
                if msg.contains("Defaulting to fallback metadata") {
                    // Codex ≥ 0.157 says this for every model tag outside its own catalog (any
                    // custom provider). Mantra already passes the context window / compaction limit
                    // itself, so it's expected — one muted line, not an amber alert.
                    self.footnote(fallback_metadata_note(msg));
                } else if is_housekeeping_warning(msg) {
                    // Codex's own housekeeping (its diagnostic-log database, say) is not this
                    // agent's work going wrong: a muted footnote in the transcript, never the
                    // card's line, never amber. The activity label is left alone.
                    self.footnote(crate::util::trunc(msg, 400));
                } else if !msg.is_empty() {
                    self.notice(Level::Warn, crate::util::trunc(msg, 400));
                }
            }
            "model/rerouted" => {
                self.notice(Level::Info, format!("model rerouted: {}", crate::util::trunc(&p.to_string(), 160)));
            }
            "thread/compacted" => {
                // Deprecated twin of the `contextCompaction` item: only used by older Codex builds.
                let seen = self.items.iter().rev().take(8).any(|i| matches!(i.kind, Kind::Compaction { .. }));
                if !seen {
                    self.notice_seq += 1;
                    let id = format!("compact-{}", self.notice_seq);
                    let mut it = Item::new(id.clone(), Kind::Compaction { from: self.ctx_used, to: None }, "");
                    it.done = true;
                    self.push(it);
                    self.compact_item = Some(id);
                }
                self.compacting = false;
            }
            "turn/completed" => {
                self.turn_active = false;
                self.turn_wait_since = None;
                let status = p.pointer("/turn/status").and_then(|x| x.as_str()).unwrap_or("completed").to_string();
                let err = p.pointer("/turn/error");
                let (emsg, kind) = match err {
                    Some(e) if !e.is_null() => {
                        let m = pretty_err(e.get("message").and_then(|x| x.as_str()).unwrap_or("turn failed"));
                        let k = refine(classify(e.get("codexErrorInfo").unwrap_or(&Value::Null)), &m);
                        (Some(m), Some(k))
                    }
                    _ => (None, None),
                };
                // last agent message of this turn = the agent's report
                let start = self.turn_first_item.min(self.items.len());
                self.final_message = self.items[start..]
                    .iter()
                    .rev()
                    .find(|i| matches!(i.kind, Kind::Agent) && !i.text.trim().is_empty())
                    .map(|i| i.text.clone());
                for it in &mut self.items[start..] {
                    if !it.done {
                        it.done = true;
                        it.touch();
                    }
                }
                match status.as_str() {
                    "failed" => {
                        let m = emsg.clone().unwrap_or_else(|| "turn failed".into());
                        self.status = Status::Failed(m.clone());
                        self.notice(Level::Error, format!("turn failed: {m}"));
                    }
                    "interrupted" => {
                        self.status = Status::Idle;
                        self.notice(Level::Warn, "interrupted");
                    }
                    _ => self.status = Status::Idle,
                }
                self.activity = match status.as_str() {
                    "failed" => "failed".into(),
                    "interrupted" => "interrupted".into(),
                    _ => "idle".into(),
                };
                let turn_items = &self.items[start.min(self.items.len())..];
                let compact_turn = turn_items.iter().any(|i| matches!(i.kind, Kind::Compaction { .. }))
                    && !turn_items.iter().any(|i| matches!(i.kind, Kind::Agent | Kind::Reasoning | Kind::Command { .. } | Kind::Files { .. } | Kind::Tool { .. }));
                self.compacting = false;
                if compact_turn {
                    self.turn_count = self.turn_count.saturating_sub(1);
                } else {
                    out.push(Signal::TurnDone { status, error: emsg, kind });
                }
            }
            _ => {}
        }
        out
    }

    fn apply_item(&mut self, item: &Value, done: bool) -> Option<Signal> {
        let s = |k: &str| item.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string();
        let id = s("id");
        let ty = s("type");
        let mut signal = None;
        match ty.as_str() {
            "userMessage" => {
                // We already echo user input locally; only show messages we didn't send (e.g. steer from engine).
                let text: String = item
                    .get("content")
                    .and_then(|c| c.as_array())
                    .map(|a| a.iter().filter_map(|x| x.get("text").and_then(|t| t.as_str())).collect::<Vec<_>>().join("\n"))
                    .unwrap_or_default();
                let dup = self.items.iter().rev().take(6).any(|i| matches!(i.kind, Kind::User) && i.text.trim() == text.trim());
                if !dup && !text.trim().is_empty() {
                    let it = self.get_or_insert(&id, Kind::User);
                    it.text = text;
                    it.done = true;
                    it.touch();
                }
            }
            "agentMessage" => {
                let text = s("text");
                let it = self.get_or_insert(&id, Kind::Agent);
                if !text.is_empty() || done {
                    if !text.is_empty() {
                        it.text = text;
                    }
                }
                it.done = done;
                it.touch();
                self.activity = "writing".into();
            }
            "plan" => {
                let it = self.get_or_insert(&id, Kind::Plan);
                let t = s("text");
                if !t.is_empty() {
                    it.text = t;
                }
                it.done = done;
                it.touch();
            }
            "reasoning" => {
                let summary: Vec<String> = item
                    .get("summary")
                    .and_then(|x| x.as_array())
                    .map(|a| a.iter().filter_map(|x| x.as_str().map(|s| s.to_string())).collect())
                    .unwrap_or_default();
                let it = self.get_or_insert(&id, Kind::Reasoning);
                if !summary.is_empty() {
                    it.text = summary.join("\n\n");
                }
                it.done = done;
                it.touch();
                if !done {
                    self.activity = "thinking".into();
                }
            }
            "commandExecution" => {
                let cmd = s("command");
                let status = s("status");
                let exit = item.get("exitCode").and_then(|x| x.as_i64());
                let dur = item.get("durationMs").and_then(|x| x.as_u64());
                let agg = item.get("aggregatedOutput").and_then(|x| x.as_str()).map(|x| x.to_string());
                let short = crate::util::trunc(cmd.lines().next().unwrap_or(""), 60);
                let it = self.get_or_insert(&id, Kind::Command { cmd: cmd.clone(), output: String::new(), exit: None, status: status.clone(), dur_ms: None });
                if !done {
                    it.started.get_or_insert_with(Instant::now);
                }
                // The backend's own figure when it has one; else what we timed from item/started;
                // else nothing — never a made-up "0ms". (Claude Code never sends durationMs, and
                // Codex sends 0 for commands it didn't time.)
                let dur = dur.filter(|d| *d > 0).or_else(|| if done { it.started.map(|t| t.elapsed().as_millis() as u64) } else { None });
                let mut out_for_probe = String::new();
                if let Kind::Command { cmd: c, output, exit: e, status: st, dur_ms } = &mut it.kind {
                    if !cmd.is_empty() {
                        *c = cmd;
                    }
                    if let Some(a) = agg {
                        if !a.is_empty() {
                            *output = a;
                            crate::util::tail_bytes(output, MAX_OUTPUT);
                        }
                    }
                    *e = exit.or(*e);
                    *st = status;
                    *dur_ms = dur.or(*dur_ms);
                    out_for_probe = output.clone();
                }
                it.done = done;
                it.touch();
                if !done {
                    self.activity = format!("$ {short}");
                } else {
                    // WP12.4/L1: a broken sandbox means every agent's shell commands are dead on
                    // this host — surface it once as a run-level signal.
                    if looks_like_broken_sandbox(&out_for_probe) {
                        signal = Some(Signal::EnvironmentBroken(crate::util::trunc(&out_for_probe, 300)));
                    }
                }
            }
            "fileChange" => {
                let status = s("status");
                let changes: Vec<FileChange> = item
                    .get("changes")
                    .and_then(|x| x.as_array())
                    .map(|a| {
                        a.iter()
                            .map(|c| FileChange {
                                path: self.rel(c.get("path").and_then(|x| x.as_str()).unwrap_or("")),
                                kind: c.pointer("/kind/type").and_then(|x| x.as_str()).unwrap_or("update").to_string(),
                                diff: c.get("diff").and_then(|x| x.as_str()).unwrap_or("").to_string(),
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                if done && status == "completed" {
                    let mut paths = vec![];
                    for c in &changes {
                        let (a, d) = if c.kind == "add" && !looks_like_diff(&c.diff) {
                            (c.diff.lines().count(), 0)
                        } else {
                            crate::util::diff_stats(&c.diff)
                        };
                        let e = self.files.entry(c.path.clone()).or_default();
                        e.adds += a;
                        e.dels += d;
                        e.kind = c.kind.clone();
                        if e.diff.is_empty() {
                            e.diff = if c.kind == "add" && !looks_like_diff(&c.diff) {
                                c.diff.lines().map(|l| format!("+{l}")).collect::<Vec<_>>().join("\n")
                            } else {
                                c.diff.clone()
                            };
                        }
                        paths.push(c.path.clone());
                    }
                    signal = Some(Signal::FilesChanged(paths));
                }
                let first = changes.first().map(|c| c.path.clone()).unwrap_or_default();
                let it = self.get_or_insert(&id, Kind::Files { changes: vec![], status: String::new() });
                if let Kind::Files { changes: ch, status: st } = &mut it.kind {
                    if !changes.is_empty() {
                        *ch = changes;
                    }
                    *st = status;
                }
                it.done = done;
                it.touch();
                if !done {
                    self.activity = format!("editing {}", crate::util::trunc(&first, 40));
                }
            }
            "mcpToolCall" | "dynamicToolCall" => {
                let name = if ty == "mcpToolCall" { format!("{}.{}", s("server"), s("tool")) } else { s("tool") };
                let args = item.get("arguments").map(|a| compact_json(a, 400)).unwrap_or_default();
                let status = s("status");
                let result = if ty == "dynamicToolCall" {
                    item.get("contentItems")
                        .and_then(|c| c.as_array())
                        .map(|a| a.iter().filter_map(|x| x.get("text").and_then(|t| t.as_str())).collect::<Vec<_>>().join("\n"))
                        .unwrap_or_default()
                } else {
                    item.get("result").map(|r| compact_json(r, 400)).unwrap_or_default()
                };
                let it = self.get_or_insert(&id, Kind::Tool { name: name.clone(), args: String::new(), result: String::new(), status: String::new() });
                if let Kind::Tool { name: n, args: a, result: r, status: st } = &mut it.kind {
                    *n = name.clone();
                    if !args.is_empty() {
                        *a = args;
                    }
                    if !result.is_empty() {
                        *r = result;
                    }
                    *st = status;
                }
                it.done = done;
                it.touch();
                if !done {
                    self.activity = format!("⚙ {}", name.trim_start_matches("mantra_"));
                }
            }
            "webSearch" => {
                let q = s("query");
                let it = self.get_or_insert(&id, Kind::Web { query: q.clone() });
                if !q.is_empty() {
                    it.kind = Kind::Web { query: q };
                }
                it.done = done;
                it.touch();
                self.activity = "searching the web".into();
            }
            "contextCompaction" => {
                if self.compact_from == 0 {
                    self.compact_from = self.ctx_used;
                }
                self.compacting = !done;
                let from = self.compact_from;
                let it = self.get_or_insert(&id, Kind::Compaction { from, to: None });
                if let Kind::Compaction { from: f, .. } = &mut it.kind {
                    if *f == 0 {
                        *f = from;
                    }
                }
                it.done = done;
                it.touch();
                if done {
                    self.compact_item = Some(id.clone());
                    self.compact_from = 0;
                    self.activity = "context compacted".into();
                } else {
                    self.activity = "compacting context".into();
                }
            }
            "enteredReviewMode" | "exitedReviewMode" => {
                let t = s("review");
                let it = self.get_or_insert(&id, Kind::Agent);
                it.text = t;
                it.done = done;
                it.touch();
            }
            _ => {}
        }
        signal
    }

    /// One-line summaries of the latest activity (for peek strips and logs). Footnotes — the
    /// backend's housekeeping (`footnote`) — are not activity: an idle card's line shows what
    /// the agent last did or the last real notice, not that Codex could not write its own logs.
    pub fn tail_lines(&self, n: usize) -> Vec<String> {
        let mut out = vec![];
        for it in self.items.iter().rev() {
            if out.len() >= n {
                break;
            }
            if it.id.starts_with(FOOTNOTE) {
                continue;
            }
            let line = match &it.kind {
                Kind::User if it.text.starts_with("[mantra") || it.text.starts_with("[from ") => {
                    let first = it.text.lines().next().unwrap_or("");
                    let tag = first.split(']').next().unwrap_or("").trim_start_matches('[').replace("mantra:", "mantra · ");
                    format!("⇢ {tag}")
                }
                Kind::User => format!("› {}", first_line(&it.text)),
                Kind::Agent => format!("● {}", last_line(&it.text)),
                Kind::Reasoning => format!("∴ {}", last_line(&it.text)),
                Kind::Plan => format!("☰ {}", first_line(&it.text)),
                Kind::Command { cmd, exit, status, .. } => {
                    let mark = match (status.as_str(), exit) {
                        ("inProgress", _) => "…".to_string(),
                        (_, Some(0)) => "✓".to_string(),
                        (_, Some(c)) => format!("✗{c}"),
                        _ => "".to_string(),
                    };
                    format!("$ {} {mark}", first_line(cmd))
                }
                Kind::Files { changes, .. } => {
                    let names: Vec<String> = changes.iter().map(|c| c.path.clone()).collect();
                    format!("✎ {}", names.join(", "))
                }
                Kind::Tool { name, status, .. } => format!("⚙ {} {}", name.trim_start_matches("mantra_"), if status == "inProgress" { "…" } else { "" }),
                Kind::Web { query } => format!("⌕ {query}"),
                Kind::Notice { .. } => format!("! {}", first_line(&it.text)),
                Kind::Compaction { .. } => "⇣ context compacted".into(),
            };
            out.push(line);
        }
        out.reverse();
        out
    }

    /// Full plain-text log (used by the orchestrator's mantra_log tool).
    pub fn log_text(&self, last_n_items: usize) -> String {
        let start = self.items.len().saturating_sub(last_n_items);
        let mut s = String::new();
        for it in &self.items[start..] {
            match &it.kind {
                Kind::User => s.push_str(&format!("[user] {}\n", it.text)),
                Kind::Agent => s.push_str(&format!("[agent] {}\n", it.text)),
                Kind::Reasoning => s.push_str(&format!("[thinking] {}\n", crate::util::trunc(&it.text, 400))),
                Kind::Plan => s.push_str(&format!("[plan] {}\n", it.text)),
                Kind::Command { cmd, output, exit, .. } => {
                    let mut o = output.clone();
                    crate::util::tail_bytes(&mut o, 1200);
                    s.push_str(&format!("[cmd] $ {cmd} (exit {:?})\n{o}\n", exit));
                }
                Kind::Files { changes, status } => {
                    for c in changes {
                        s.push_str(&format!("[edit:{status}] {} {}\n", c.kind, c.path));
                    }
                }
                Kind::Tool { name, args, result, .. } => s.push_str(&format!("[tool] {name}({}) -> {}\n", crate::util::trunc(args, 200), crate::util::trunc(result, 200))),
                Kind::Web { query } => s.push_str(&format!("[web] {query}\n")),
                Kind::Notice { .. } => s.push_str(&format!("[notice] {}\n", it.text)),
                Kind::Compaction { .. } => s.push_str("[compacted]\n"),
            }
        }
        s
    }
}

pub fn classify(info: &Value) -> ErrKind {
    let key = match info {
        Value::String(s) => s.clone(),
        Value::Object(o) => o.keys().next().cloned().unwrap_or_default(),
        _ => String::new(),
    };
    match key.as_str() {
        "serverOverloaded" | "rateLimitExceeded" | "internalServerError" | "httpConnectionFailed" | "responseStreamConnectionFailed" | "responseStreamDisconnected" | "responseTooManyFailedAttempts" => ErrKind::Transient,
        "contextWindowExceeded" => ErrKind::ContextFull,
        "usageLimitExceeded" | "sessionBudgetExceeded" => ErrKind::UsageLimit,
        "unauthorized" => ErrKind::Auth,
        "badRequest" => ErrKind::ProviderRejected,
        _ => ErrKind::Other,
    }
}

/// A Codex warning about its own housekeeping, not about this agent's turn: it could not write
/// its diagnostic-log database ("Codex couldn't save diagnostic logs to its local database. Use
/// /feedback …"). Nothing Mantra or the user can act on mid-run, so it never shows as the
/// agent's line. (The fallback-metadata warning has its own, reworded note.)
pub fn is_housekeeping_warning(msg: &str) -> bool {
    msg.to_lowercase().contains("diagnostic log")
}

/// The item-id prefix of a footnote (`Agent::footnote`).
const FOOTNOTE: &str = "footnote-";

/// "5.9M cached" for a token total of which `cached` was cached input, when that is worth a
/// word: Codex's `total.totalTokens` counts every request's whole input, so a worker that
/// cycles tools in a big context reads "6.8M" of which most was the same prefix served from the
/// cache. Below a fifth of the total, or under 10k tokens altogether, it is noise rather than a
/// correction, and nothing is shown.
pub fn cached_share(total: u64, cached: u64) -> Option<String> {
    cached_material(total, cached).then(|| format!("{} cached", crate::util::fmt_tokens(cached.min(total))))
}

/// The same reading as a share, "80% cached", for a card too narrow for the count twice.
pub fn cached_share_short(total: u64, cached: u64) -> Option<String> {
    cached_material(total, cached).then(|| format!("{}% cached", cached.min(total).saturating_mul(100) / total))
}

fn cached_material(total: u64, cached: u64) -> bool {
    let cached = cached.min(total);
    cached > 0 && total >= 10_000 && cached.saturating_mul(5) >= total
}

/// Codex's "Model metadata for `x` not found. Defaulting to fallback metadata; this can degrade
/// performance and cause issues." as one short line: the model tag plus what Mantra does about it.
pub fn fallback_metadata_note(msg: &str) -> String {
    let tag = msg.split('`').nth(1).unwrap_or("this model");
    format!("codex has no built-in metadata for `{tag}` — using the context window from models.toml")
}

/// Providers often return `{"error":{"message":"…"}}` bodies: show just the message.
pub fn pretty_err(msg: &str) -> String {
    let t = msg.trim();
    if let Some(i) = t.find('{') {
        if let Ok(v) = serde_json::from_str::<Value>(&t[i..]) {
            let m = v.pointer("/error/message").or_else(|| v.get("message")).or_else(|| v.get("detail")).and_then(|x| x.as_str());
            if let Some(m) = m {
                let prefix = t[..i].trim().trim_end_matches(':').trim();
                return if prefix.is_empty() { m.to_string() } else { format!("{prefix}: {m}") };
            }
        }
    }
    t.to_string()
}

/// Text of a gateway/backend outage rather than of a request that is wrong: a 5xx, "upstream
/// provider returned an error", "all upstream candidates", a reset or timed-out connection.
/// `m` is lower-cased by the caller.
fn transport_text(m: &str) -> bool {
    const WORDS: &[&str] = &["timed out", "timeout", "disconnected", "upstream", "bad gateway", "service unavailable", "gateway time", "internal server error", "overloaded", "connection reset", "connection refused", "connection closed", "connection error", "broken pipe", "error sending request", "temporarily unavailable"];
    if WORDS.iter().any(|w| m.contains(w)) {
        return true;
    }
    // a bare 5xx status, not a digit run inside a larger number
    let b = m.as_bytes();
    (0..b.len().saturating_sub(2)).any(|i| {
        b[i] == b'5'
            && b[i + 1].is_ascii_digit()
            && b[i + 2].is_ascii_digit()
            && (i == 0 || !b[i - 1].is_ascii_digit())
            && b.get(i + 3).map(|c| !c.is_ascii_digit()).unwrap_or(true)
            && matches!(&m[i..i + 3], "500" | "502" | "503" | "504" | "520" | "521" | "522" | "523" | "524")
    })
}

/// Is this failed turn the model gateway's outage (not the agent's fault, not worth an attempt)?
/// Codex's own classification wins; otherwise the text decides, except for errors that are
/// plainly something else (credentials, quota, a full context, a rejected effort).
pub fn is_transport_error(kind: Option<ErrKind>, msg: &str) -> bool {
    match kind {
        Some(ErrKind::Transient) => true,
        Some(ErrKind::Auth | ErrKind::UsageLimit | ErrKind::ContextFull) => false,
        _ => !rejects_effort(msg) && transport_text(&msg.to_lowercase()),
    }
}

fn rejects_effort(msg: &str) -> bool {
    let m = msg.to_lowercase();
    m.contains("reasoning_effort") || m.contains("effort must be")
}

/// A 400 that names the reasoning effort ("reasoning_effort must be low, high, xhigh, max"): the
/// values it allows, as far as the message lists them (empty when it only names the parameter).
/// `None` for any other error.
pub fn effort_rejection(msg: &str) -> Option<Vec<String>> {
    if !rejects_effort(msg) {
        return None;
    }
    let m = msg.to_lowercase();
    let tail = m.find("must be").map(|i| &m[i + "must be".len()..]).unwrap_or("");
    let mut out: Vec<String> = vec![];
    for w in tail.split(|c: char| !c.is_ascii_alphanumeric()).filter(|w| !w.is_empty()) {
        if crate::config::ALL_EFFORTS.contains(&w) && !out.iter().any(|o| o == w) {
            out.push(w.to_string());
        }
    }
    Some(out)
}

/// The allowed effort nearest to `want` on the ladder (`config::ALL_EFFORTS`), the lower one on a
/// tie. `None` when nothing is allowed or `want` is not on the ladder.
pub fn nearest_effort(want: &str, allowed: &[String]) -> Option<String> {
    let rank = |e: &str| crate::config::ALL_EFFORTS.iter().position(|x| *x == e);
    let w = rank(&want.trim().to_lowercase())? as i32;
    allowed.iter().filter_map(|a| rank(a).map(|r| (r as i32, a))).min_by_key(|(r, _)| ((r - w).abs(), *r)).map(|(_, a)| a.clone())
}

/// Use the message when Codex doesn't classify the error (e.g. HTTP 401/403 from a proxy).
pub fn refine(k: ErrKind, msg: &str) -> ErrKind {
    if k != ErrKind::Other {
        return k;
    }
    let m = msg.to_lowercase();
    if m.contains("401") || m.contains("403") || m.contains("forbidden") || m.contains("unauthorized") || m.contains("api key") {
        ErrKind::Auth
    } else if m.contains("429") || transport_text(&m) {
        ErrKind::Transient
    } else if m.contains("400") || m.contains("422") || m.contains("unexpected message role") || m.contains("unsupported") || m.contains("invalid_request_error") {
        ErrKind::ProviderRejected
    } else {
        k
    }
}

fn looks_like_diff(s: &str) -> bool {
    s.starts_with("@@") || s.starts_with("---") || s.starts_with("diff --git") || s.lines().any(|l| l.starts_with("@@ "))
}

/// Split an aggregated unified diff into per-file chunks.
pub fn split_diff(diff: &str) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = vec![];
    let mut cur: Option<(String, String)> = None;
    for line in diff.lines() {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            if let Some(c) = cur.take() {
                out.push(c);
            }
            let path = rest.split(" b/").nth(1).unwrap_or(rest).to_string();
            cur = Some((path, String::new()));
            continue;
        }
        if cur.is_none() {
            if let Some(p) = line.strip_prefix("+++ b/") {
                cur = Some((p.to_string(), String::new()));
                continue;
            }
        }
        if let Some((_, body)) = cur.as_mut() {
            body.push_str(line);
            body.push('\n');
        }
    }
    if let Some(c) = cur {
        out.push(c);
    }
    out
}

fn compact_json(v: &Value, max: usize) -> String {
    crate::util::trunc(&v.to_string(), max)
}

fn first_line(s: &str) -> String {
    s.lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim().to_string()
}

fn last_line(s: &str) -> String {
    s.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("").trim().to_string()
}

#[cfg(test)]
mod tests {
    /// `first_token_wait` is the engine's "active turn, nothing back yet": it runs from the prompt
    /// (or `turn/started`) and ends with the first token, text or tool activity — never with the
    /// echo of our own prompt or the retry notices a flaky gateway produces.
    #[test]
    fn first_token_wait_ends_only_with_real_output() {
        use super::*;
        use serde_json::json;
        let mut a = Agent::new(1, "t", "solo", std::path::PathBuf::from("."));
        assert!(a.first_token_wait().is_none(), "no turn, no wait");
        // a turn Mantra sent: the clock started when the prompt went out, before turn/started
        a.awaiting_start = true;
        a.turn_wait_since = Some(Instant::now() - Duration::from_secs(90));
        assert!(a.first_token_wait().unwrap() >= Duration::from_secs(90));
        a.apply("turn/started", &json!({"turn": {"id": "t1"}}));
        assert!(a.first_token_wait().unwrap() >= Duration::from_secs(90), "turn/started must not restart the clock of a turn we sent");
        // the echo of our prompt, a status change and "Reconnecting… 2/5" are not output
        a.apply("item/started", &json!({"item": {"type": "userMessage", "id": "u1", "content": [{"text": "go"}]}}));
        a.apply("thread/status/changed", &json!({"status": {"type": "active"}}));
        a.apply("error", &json!({"error": {"message": "Reconnecting... 2/5"}, "willRetry": true}));
        assert!(matches!(a.status, Status::Retrying(_)));
        assert!(a.first_token_wait().is_some(), "retry notices move last_event but are not output");
        a.apply("thread/tokenUsage/updated", &json!({"tokenUsage": {"total": {"totalTokens": 0}, "last": {"inputTokens": 0, "outputTokens": 0}}}));
        assert!(a.first_token_wait().is_some(), "a token count that did not grow is not output");
        // silence is expected while an approval is open, after a hand stop, during a compaction
        a.status = Status::Waiting;
        assert!(a.first_token_wait().is_none());
        a.status = Status::Busy;
        a.stopped_by_user = true;
        assert!(a.first_token_wait().is_none());
        a.stopped_by_user = false;
        a.compacting = true;
        assert!(a.first_token_wait().is_none());
        a.compacting = false;
        assert!(a.first_token_wait().is_some());
        // the first token ends it
        a.apply("thread/tokenUsage/updated", &json!({"tokenUsage": {"total": {"totalTokens": 120}, "last": {"inputTokens": 100, "outputTokens": 20}}}));
        assert!(a.first_token_wait().is_none());
        // a turn that starts by itself (not sent by us) waits from turn/started; a delta ends it
        a.apply("turn/completed", &json!({"turn": {"id": "t1", "status": "completed"}}));
        a.apply("turn/started", &json!({"turn": {"id": "t2"}}));
        let w = a.first_token_wait().expect("a fresh wait");
        assert!(w < Duration::from_secs(5), "{w:?}");
        a.apply("item/agentMessage/delta", &json!({"itemId": "m1", "delta": "hi"}));
        assert!(a.first_token_wait().is_none());
        // a command or reasoning item counts as well, and a finished turn never leaves a wait behind
        a.apply("turn/completed", &json!({"turn": {"id": "t2", "status": "completed"}}));
        a.apply("turn/started", &json!({"turn": {"id": "t3"}}));
        a.apply("item/started", &json!({"item": {"type": "commandExecution", "id": "c1", "command": "ls", "status": "inProgress"}}));
        assert!(a.first_token_wait().is_none());
        a.apply("turn/started", &json!({"turn": {"id": "t4"}}));
        a.apply("turn/completed", &json!({"turn": {"id": "t4", "status": "interrupted"}}));
        assert!(a.first_token_wait().is_none() && a.turn_wait_since.is_none());
    }

    /// Codex ≥ 0.157 warns for every custom model tag; Mantra supplies the metadata that matters
    /// (context window) itself, so the transcript shows one muted info line, not an amber alert.
    #[test]
    fn fallback_metadata_warning_is_a_muted_info_line() {
        use super::*;
        use serde_json::json;
        let mut a = Agent::new(1, "t", "solo", std::path::PathBuf::from("."));
        a.apply("warning", &json!({"message": "Model metadata for `testing` not found. Defaulting to fallback metadata; this can degrade performance and cause issues."}));
        let it = a.items.last().expect("a notice");
        assert!(matches!(it.kind, Kind::Notice { level: Level::Info }), "{:?}", it.kind);
        assert_eq!(it.text, "codex has no built-in metadata for `testing` — using the context window from models.toml");
        assert!(!it.text.contains('\n'));
        // other warnings keep their level
        a.apply("warning", &json!({"message": "something else"}));
        assert!(matches!(a.items.last().unwrap().kind, Kind::Notice { level: Level::Warn }));
    }
    /// A Codex housekeeping warning (it could not write its own diagnostic logs) is not the
    /// agent's activity: a muted info line in the transcript, nothing on the card — whose idle
    /// line shows the last real notice — and the activity label untouched.
    #[test]
    fn housekeeping_warnings_are_footnotes_not_the_agents_line() {
        use super::*;
        use serde_json::json;
        let mut a = Agent::new(1, "t", "orchestrator", std::path::PathBuf::from("."));
        a.apply("item/completed", &json!({"item": {"type": "commandExecution", "id": "c1", "command": "cargo test", "status": "completed", "exitCode": 0}}));
        a.activity = "idle".into();
        a.apply("warning", &json!({"message": "Codex couldn't save diagnostic logs to its local database. Use /feedback to report this issue."}));
        let it = a.items.last().expect("a notice");
        assert!(matches!(it.kind, Kind::Notice { level: Level::Info }), "{:?}", it.kind);
        assert_eq!(a.activity, "idle", "the activity line is not replaced");
        assert_eq!(a.tail_lines(1), vec!["$ cargo test ✓".to_string()], "the card's line is the last real thing, not the footnote");
        // Codex repeats it every turn: the transcript says it once
        let n = a.items.len();
        a.apply("warning", &json!({"message": "Codex couldn't save diagnostic logs to its local database. Use /feedback to report this issue."}));
        assert_eq!(a.items.len(), n);
        // the fallback-metadata note is a footnote too
        a.apply("warning", &json!({"message": "Model metadata for `testing` not found. Defaulting to fallback metadata; this can degrade performance and cause issues."}));
        assert_eq!(a.tail_lines(1), vec!["$ cargo test ✓".to_string()]);
        // a real warning still shows, in amber, and is what the card's idle line picks up
        a.apply("warning", &json!({"message": "tests were skipped"}));
        assert!(matches!(a.items.last().unwrap().kind, Kind::Notice { level: Level::Warn }));
        assert_eq!(a.tail_lines(1), vec!["! tests were skipped".to_string()]);
        // and Mantra's own info notices (a model switch) are real notices, not footnotes
        a.notice(Level::Info, "model → luna (luna-1)");
        assert_eq!(a.tail_lines(1), vec!["! model → luna (luna-1)".to_string()]);
    }

    /// `total.totalTokens` counts every request's whole input, so a worker in a big context
    /// reads millions of which most was cached: the agent keeps the cached share, and the
    /// label says so only when that share is material.
    #[test]
    fn cached_input_is_tracked_and_named_when_it_is_most_of_the_total() {
        use super::*;
        use serde_json::json;
        let mut a = Agent::new(1, "t", "worker", std::path::PathBuf::from("."));
        a.apply("thread/tokenUsage/updated", &json!({"tokenUsage": {"total": {"totalTokens": 6_800_000, "inputTokens": 6_500_000, "cachedInputTokens": 5_900_000, "outputTokens": 300_000}, "last": {"inputTokens": 180_000, "outputTokens": 2_000}}}));
        assert_eq!((a.tokens_total, a.tokens_cached), (6_800_000, 5_900_000));
        assert_eq!(a.cached_share().as_deref(), Some("5.90M cached"));
        // an update without the field keeps the last known share rather than zeroing it
        a.apply("thread/tokenUsage/updated", &json!({"tokenUsage": {"total": {"totalTokens": 6_900_000}, "last": {"inputTokens": 1, "outputTokens": 1}}}));
        assert_eq!(a.tokens_cached, 5_900_000);
        // too small a total, or too small a share, is not worth a word; a share can't exceed the total
        assert_eq!(cached_share(9_000, 8_000), None);
        assert_eq!(cached_share(100_000, 19_000), None);
        assert_eq!(cached_share(100_000, 20_000).as_deref(), Some("20.0k cached"));
        assert_eq!(cached_share(100_000, 0), None);
        assert_eq!(cached_share(50_000, 80_000).as_deref(), Some("50.0k cached"));
        // the short form for a narrow card follows the same rule
        assert_eq!(cached_share_short(6_800_000, 5_900_000).as_deref(), Some("86% cached"));
        assert_eq!(cached_share_short(100_000, 19_000), None);
    }

    #[test]
    fn compaction_turn_is_tracked_and_not_counted_as_work() {
        use super::*;
        use serde_json::json;
        let mut a = Agent::new(1, "t", "solo", std::path::PathBuf::from("."));
        let usage = |a: &mut Agent, ctx: u64| {
            a.apply("thread/tokenUsage/updated", &json!({"tokenUsage": {"total": {"totalTokens": ctx}, "last": {"inputTokens": ctx, "outputTokens": 0}, "modelContextWindow": 272000}}));
        };
        usage(&mut a, 180_000);
        a.apply("turn/started", &json!({"turn": {"id": "t1"}}));
        a.apply("item/started", &json!({"item": {"type": "contextCompaction", "id": "c1"}}));
        assert!(a.compacting);
        a.apply("item/completed", &json!({"item": {"type": "contextCompaction", "id": "c1"}}));
        usage(&mut a, 30_000);
        let sig = a.apply("turn/completed", &json!({"turn": {"id": "t1", "status": "completed"}}));
        assert!(sig.iter().all(|s| !matches!(s, Signal::TurnDone { .. })), "a compaction-only turn must not look like finished work");
        assert!(!a.compacting);
        let it = a.items.iter().find(|i| matches!(i.kind, Kind::Compaction { .. })).unwrap();
        assert!(matches!(it.kind, Kind::Compaction { from: 180_000, to: Some(30_000) }));
        assert!(a.ctx_anim.is_some());
        a.apply("thread/compacted", &json!({"threadId": "x"}));
        assert_eq!(a.items.iter().filter(|i| matches!(i.kind, Kind::Compaction { .. })).count(), 1, "deprecated twin event must be ignored");
        // a normal turn with an auto-compaction inside still counts as work
        a.push_user("do it");
        a.apply("turn/started", &json!({"turn": {"id": "t2"}}));
        a.apply("item/completed", &json!({"item": {"type": "contextCompaction", "id": "c2"}}));
        a.apply("item/completed", &json!({"item": {"type": "agentMessage", "id": "m1", "text": "done"}}));
        let sig = a.apply("turn/completed", &json!({"turn": {"id": "t2", "status": "completed"}}));
        assert!(sig.iter().any(|s| matches!(s, Signal::TurnDone { .. })));
    }

    #[test]
    fn pretty_errors_unwrap_provider_json() {
        use super::*;
        assert_eq!(pretty_err(r#"{"error":{"message":"invalid api key","type":"x"}}"#), "invalid api key");
        assert_eq!(pretty_err(r#"unexpected status 400: {"error":{"message":"bad model"}}"#), "unexpected status 400: bad model");
        assert_eq!(pretty_err("plain text"), "plain text");
    }

    #[test]
    fn gateway_outages_are_told_from_rejected_requests() {
        use super::*;
        assert!(is_transport_error(None, "unexpected status 503 Service Unavailable"));
        assert!(is_transport_error(Some(ErrKind::ProviderRejected), "all upstream candidates failed"));
        assert!(is_transport_error(Some(ErrKind::Transient), "anything"));
        assert!(!is_transport_error(Some(ErrKind::Auth), "401 upstream"));
        assert!(!is_transport_error(None, "unexpected status 400: reasoning_effort must be low, high"));
        assert!(!is_transport_error(None, "Unexpected message role."));
    }

    #[test]
    fn a_rejected_effort_is_parsed_and_mapped_to_the_nearest_allowed_one() {
        use super::*;
        let m = "unexpected status 400: DeepSeek V4.1 reasoning_effort must be low, high, xhigh, max";
        let allowed = effort_rejection(m).expect("an effort error");
        assert_eq!(allowed, ["low", "high", "xhigh", "max"]);
        assert_eq!(nearest_effort("medium", &allowed).as_deref(), Some("low"), "a tie goes to the lower one");
        assert_eq!(nearest_effort("high", &allowed).as_deref(), Some("high"));
        assert_eq!(nearest_effort("ultra", &allowed).as_deref(), Some("max"));
        assert_eq!(effort_rejection("reasoning_effort is invalid"), Some(vec![]));
        assert_eq!(effort_rejection("bad request"), None);
        assert_eq!(nearest_effort("medium", &[]), None);
    }

    #[test]
    fn refines_unclassified_errors() {
        use super::*;
        assert_eq!(refine(ErrKind::Other, "unexpected status 403 Forbidden: Host not in allowlist"), ErrKind::Auth);
        assert_eq!(refine(ErrKind::Other, "stream disconnected before completion"), ErrKind::Transient);
        assert_eq!(refine(ErrKind::Transient, "403"), ErrKind::Transient);
        assert_eq!(refine(ErrKind::Other, "something odd"), ErrKind::Other);
        assert_eq!(refine(ErrKind::Other, "unexpected status 502 Bad Gateway: upstream provider returned an error"), ErrKind::Transient);
        assert_eq!(refine(ErrKind::Other, "connection reset by peer"), ErrKind::Transient);
        assert_eq!(refine(ErrKind::Other, "used 15000 tokens"), ErrKind::Other, "a 5xx is a whole number, not a digit run");
        assert_eq!(refine(ErrKind::Other, "Unexpected message role."), ErrKind::ProviderRejected);
        assert_eq!(refine(ErrKind::Other, "unexpected status 422: invalid_request_error"), ErrKind::ProviderRejected);
    }

    #[test]
    fn dedupes_repeated_error_notices() {
        use super::*;
        let mut a = Agent::new(1, "t", "solo", std::path::PathBuf::from("."));
        a.notice(Level::Error, "boom 403");
        a.notice(Level::Error, "turn failed: boom 403");
        assert_eq!(a.items.len(), 1);
        assert_eq!(a.items[0].text, "turn failed: boom 403");
        a.notice(Level::Warn, "other");
        assert_eq!(a.items.len(), 2);
    }

    use super::*;
    use serde_json::json;

    #[test]
    fn reducer_streams_and_completes() {
        let mut a = Agent::new(1, "t", "solo", PathBuf::from("/p"));
        a.apply("turn/started", &json!({"turn": {"id": "t1"}}));
        assert!(a.busy());
        a.apply("item/agentMessage/delta", &json!({"itemId": "m1", "delta": "Hel"}));
        a.apply("item/agentMessage/delta", &json!({"itemId": "m1", "delta": "lo"}));
        a.apply("item/started", &json!({"item": {"type": "commandExecution", "id": "c1", "command": "ls", "status": "inProgress"}}));
        a.apply("item/commandExecution/outputDelta", &json!({"itemId": "c1", "delta": "a\nb\n"}));
        a.apply("item/completed", &json!({"item": {"type": "commandExecution", "id": "c1", "command": "ls", "status": "completed", "exitCode": 0}}));
        a.apply("item/completed", &json!({"item": {"type": "fileChange", "id": "f1", "status": "completed", "changes": [{"path": "/p/src/x.rs", "kind": {"type": "add"}, "diff": "a\nb"}]}}));
        let sig = a.apply("turn/completed", &json!({"turn": {"id": "t1", "status": "completed", "error": null}}));
        assert!(!a.busy());
        assert_eq!(a.final_message.as_deref(), Some("Hello"));
        assert!(a.files.contains_key("src/x.rs"));
        assert!(matches!(sig[0], Signal::TurnDone { .. }));
    }

    #[test]
    fn command_duration_is_timed_from_start_when_the_backend_sends_none() {
        use super::*;
        use serde_json::json;
        let dur_of = |a: &Agent, id: &str| match &a.items.iter().find(|i| i.id == id).unwrap().kind {
            Kind::Command { dur_ms, .. } => *dur_ms,
            _ => panic!("not a command"),
        };
        let mut a = Agent::new(1, "t", "solo", std::path::PathBuf::from("."));
        a.apply("turn/started", &json!({"turn": {"id": "t1"}}));
        a.apply("item/started", &json!({"item": {"type": "commandExecution", "id": "c1", "command": "sleep 1", "status": "inProgress"}}));
        assert_eq!(dur_of(&a, "c1"), None, "in progress: nothing to show yet");
        std::thread::sleep(std::time::Duration::from_millis(250));
        a.apply("item/completed", &json!({"item": {"type": "commandExecution", "id": "c1", "command": "sleep 1", "status": "completed", "exitCode": 0, "durationMs": 0}}));
        assert!(dur_of(&a, "c1").unwrap() >= 250, "timed from item/started, not the backend's 0");
        // A backend figure wins when there is one.
        a.apply("item/started", &json!({"item": {"type": "commandExecution", "id": "c2", "command": "ls", "status": "inProgress"}}));
        a.apply("item/completed", &json!({"item": {"type": "commandExecution", "id": "c2", "command": "ls", "status": "completed", "exitCode": 0, "durationMs": 1234}}));
        assert_eq!(dur_of(&a, "c2"), Some(1234));
        // Completed out of nowhere (no start seen, nothing from the backend): no duration, not "0ms".
        a.apply("item/completed", &json!({"item": {"type": "commandExecution", "id": "c3", "command": "ls", "status": "completed", "exitCode": 0}}));
        assert_eq!(dur_of(&a, "c3"), None);
    }

    #[test]
    fn classifies_errors() {
        assert_eq!(classify(&json!("serverOverloaded")), ErrKind::Transient);
        assert_eq!(classify(&json!({"responseStreamDisconnected": {"httpStatusCode": 502}})), ErrKind::Transient);
        assert_eq!(classify(&json!("unauthorized")), ErrKind::Auth);
        assert_eq!(classify(&json!("contextWindowExceeded")), ErrKind::ContextFull);
        assert_eq!(classify(&json!({"contextWindowExceeded": {}})), ErrKind::ContextFull);
        assert_eq!(classify(&json!("badRequest")), ErrKind::ProviderRejected);
    }

    #[test]
    fn splits_diffs() {
        let d = "diff --git a/x b/x\n--- a/x\n+++ b/x\n+1\ndiff --git a/y b/y\n+2\n";
        let s = split_diff(d);
        assert_eq!(s.len(), 2);
        assert_eq!(s[1].0, "y");
    }

    #[test]
    fn broken_sandbox_detection_is_specific() {
        assert!(super::looks_like_broken_sandbox("bwrap: loopback: Failed RTM_NEWADDR"));
        assert!(super::looks_like_broken_sandbox("Codex's Linux sandbox uses bubblewrap and needs access to create USER NAMESPACES."));
        assert!(super::looks_like_broken_sandbox("kernel.unprivileged_userns_clone = 0"));
        assert!(super::looks_like_broken_sandbox("kernel.apparmor_restrict_unprivileged_userns = 1"));
        // an agent saying it cannot do something in its sandbox is asking, not reporting a dead machine
        assert!(!super::looks_like_broken_sandbox("I cannot resolve finding #1 myself: the sandbox denied writing .git/index"));
        // a read-only role refusing a write is its own scope, not a broken environment
        assert!(!super::looks_like_broken_sandbox("Failed to write file: README.md"));
        assert!(!super::looks_like_broken_sandbox("tests failed: 2 assertions"));
    }
}
