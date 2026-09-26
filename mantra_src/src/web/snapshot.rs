//! The web UI's view of the App (design §4): plain serializable structs built on the App task,
//! and the `Publisher` that diffs one build against the last and emits the smallest delta.
//!
//! Nothing here holds `&App` beyond one call. Transcript items and pulse lines are not part of
//! `Snapshot`'s `PartialEq` — they are diffed by ordinal / sequence number instead, so an
//! unchanged transcript costs a length compare per item, not a string compare.

use super::protocol::{AgentWithItems, DeltaMsg, SnapshotMsg};
use crate::agent::{Agent, Item, Kind, Level, Status};
use crate::app::{App, Approval, Screen};
use crate::engine::run::{PhaseStep, Run, Stage};
use crate::hub::AgentId;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// Transcript items per agent that are diffed each publish (and cached for new connections).
const ITEM_WINDOW: u64 = 300;
/// Items per agent in a full snapshot; older ones come via `fetch_items`.
const SNAPSHOT_ITEMS: usize = 150;
/// Pulse lines in a full snapshot.
const SNAPSHOT_PULSE: usize = 100;

// ───────────────────────────── time ─────────────────────────────

/// One (Instant, unix ms) pair taken once: every `Instant` is converted through it, so the same
/// Instant always maps to the same millisecond. (`now_ms - elapsed` jitters by a millisecond
/// between calls, which would make every agent look changed on every publish.)
fn anchor() -> &'static (Instant, u64) {
    static A: OnceLock<(Instant, u64)> = OnceLock::new();
    A.get_or_init(|| {
        let ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0);
        (Instant::now(), ms)
    })
}

/// Unix milliseconds of an `Instant`.
pub fn ms(t: Instant) -> u64 {
    let (i0, m0) = *anchor();
    if t >= i0 {
        m0.saturating_add((t - i0).as_millis() as u64)
    } else {
        m0.saturating_sub((i0 - t).as_millis() as u64)
    }
}

pub fn now_ms() -> u64 {
    ms(Instant::now())
}

/// Durations that tick while you watch (quiet time, watchdog idle, run elapsed) are reported in
/// whole seconds so they change at most once a second, not on every 66 ms publish.
fn secs_ms(d: Duration) -> u64 {
    d.as_secs().saturating_mul(1000)
}

fn level_str(l: &Level) -> &'static str {
    match l {
        Level::Info => "info",
        Level::Warn => "warn",
        Level::Error => "error",
        Level::Ok => "ok",
    }
}

fn tail_chars(s: &str, n: usize) -> String {
    let count = s.chars().count();
    if count <= n {
        return s.to_string();
    }
    s.chars().skip(count - n).collect()
}

// ───────────────────────────── views ─────────────────────────────

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct ModelInfo {
    pub alias: String,
    pub model: String,
    pub provider: String,
    pub provider_name: String,
    /// "codex" | "claude-code"
    pub backend: String,
    pub efforts: Vec<String>,
    pub default_effort: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,
    pub compact_percent: u8,
    pub note: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub problem: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct AppInfo {
    pub version: String,
    pub protocol: u32,
    pub project: String,
    pub project_name: String,
    pub branch: String,
    pub demo: bool,
    pub pattern_name: String,
    pub patterns: Vec<String>,
    pub approval_mode: String,
    pub default_model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub solo: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox_warning: Option<String>,
    pub unfinished_runs: Vec<String>,
    pub title: String,
    pub started_at: u64,
    /// "solo" | "stage" | "zoom:<id>" | "studio" | "models"
    pub tui_screen: String,
    pub models: Vec<ModelInfo>,
    pub push_enabled: bool,
    pub tls: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub listen: Option<String>,
    pub headless: bool,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct PlanStep {
    pub text: String,
    pub status: String,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct FileView {
    pub path: String,
    pub adds: usize,
    pub dels: usize,
    pub kind: String,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct WorkerView {
    pub task_id: String,
    pub title: String,
    pub role: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<u32>,
    pub attempt: u32,
    pub state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub branch: String,
    pub adhoc: bool,
    pub paused: bool,
    pub tripwires: Vec<String>,
    pub spawned_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub report_tail: Option<String>,
}

/// Agent metadata — no transcript (that travels as `ItemView`s).
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct AgentView {
    pub id: u32,
    pub name: String,
    pub role: String,
    pub glyph: String,
    pub color: String,
    pub model_alias: String,
    pub model: String,
    pub provider: String,
    pub provider_name: String,
    pub backend: String,
    pub effort: String,
    pub efforts: Vec<String>,
    /// "starting" | "idle" | "busy" | "waiting" | "retrying" | "failed" | "crashed" | "stopped"
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_detail: Option<String>,
    pub busy: bool,
    pub activity: String,
    pub turn_active: bool,
    pub awaiting_start: bool,
    pub compacting: bool,
    pub compact_pending: bool,
    pub stopped_by_user: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_started_at: Option<u64>,
    pub last_event_at: u64,
    pub created_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quiet_ms: Option<u64>,
    pub turn_count: u32,
    pub tokens_total: u64,
    pub ctx_used: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ctx_window: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ctx_percent: Option<u8>,
    pub compact_percent: u8,
    pub ctx_assumed: bool,
    pub plan: Vec<PlanStep>,
    pub plan_done: usize,
    pub plan_total: usize,
    pub files: Vec<FileView>,
    pub files_adds: usize,
    pub files_dels: usize,
    pub queued: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_note: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub final_message: Option<String>,
    pub in_run: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_name: Option<String>,
    /// "solo" | "planner" | "manager" | "orchestrator" | "worker" | "gate" | "finale" | "probe" | "architect" | other role name
    pub role_kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker: Option<WorkerView>,
    pub expected: bool,
    pub waiting_answer: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub asked: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub watchdog_idle_ms: Option<u64>,
    pub approval_pending: bool,
    pub items_first: u64,
    pub items_total: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
    pub cwd: String,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct ChangeView {
    pub path: String,
    pub kind: String,
    pub adds: usize,
    pub dels: usize,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct ToolView {
    pub name: String,
    pub args: String,
    pub result: String,
    pub status: String,
}

/// One transcript item. `ord` is stable for the agent's lifetime (`Agent::trimmed + index`).
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct ItemView {
    pub ord: u64,
    pub id: String,
    /// "user" | "agent" | "reasoning" | "plan" | "command" | "files" | "tool" | "web" | "notice" | "compaction"
    pub kind: String,
    pub text: String,
    pub done: bool,
    pub v: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cmd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dur_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub changes: Option<Vec<ChangeView>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<ToolView>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub level: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<u64>,
}

/// Growth of a streaming item: `append` goes on the end of `text` (for a command: of `output`).
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct ItemAppend {
    pub ord: u64,
    pub id: String,
    pub v: u64,
    pub done: bool,
    pub append: String,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(untagged)]
pub enum ItemDelta {
    Full(Box<ItemView>),
    Append(ItemAppend),
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct StageView {
    /// "setup" | "planning" | "review" | "phase" | "finale" | "done" | "failed"
    pub kind: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<usize>,
    /// "orchestrating" | "merging" | "checks" | "gate" | "handoff"
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub round: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finale: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct HaltView {
    pub reason: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_name: Option<String>,
    pub message: String,
    pub since_at: u64,
    pub hint: String,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct QuestionView {
    pub from: u32,
    pub from_name: String,
    pub text: String,
    pub since_at: u64,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct HistoryTask {
    pub id: String,
    pub title: String,
    pub glyph: String,
    pub ok: bool,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct HistoryView {
    pub phase: String,
    pub tasks_done: usize,
    pub tasks_total: usize,
    /// Gate rounds are not recorded per phase; always 0 (kept for the documented shape).
    pub rounds: u32,
    pub summary: String,
    pub duration_ms: u64,
    pub ended_at: u64,
    pub tasks: Vec<HistoryTask>,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct CheckView {
    pub cmd: String,
    pub ok: bool,
    pub output_tail: String,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct RunView {
    pub id: String,
    pub pattern: String,
    pub brief: String,
    pub stage: StageView,
    pub active: bool,
    pub halted: Option<HaltView>,
    pub question: Option<QuestionView>,
    pub want_review: bool,
    pub plan_version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub planner: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manager: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub orchestrator: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gate_agent: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finale_agent: Option<u32>,
    pub workers: Vec<WorkerView>,
    pub history: Vec<HistoryView>,
    pub checks: Vec<CheckView>,
    pub conflicts: Vec<String>,
    pub alerts: Vec<String>,
    pub started_at: u64,
    pub elapsed_ms: u64,
    pub total_tokens: u64,
    pub busy_count: usize,
    pub workspace: String,
    pub branch: String,
    pub landable: bool,
    pub pulse_seq: u64,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct TaskView {
    pub id: String,
    pub title: String,
    pub role: String,
    pub scope: Vec<String>,
    pub acceptance: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct GateView {
    pub checks: Vec<String>,
    pub focus: String,
    pub criteria: String,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct PhaseView {
    pub id: String,
    pub name: String,
    pub goal: String,
    pub tasks: Vec<TaskView>,
    pub gate: GateView,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct PlanView {
    pub version: u32,
    pub title: String,
    pub summary: String,
    pub orchestrator_brief: String,
    pub phases: Vec<PhaseView>,
    pub final_checks: Vec<String>,
    pub markdown: String,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct PulseView {
    pub n: u64,
    pub at: u64,
    pub t: String,
    pub glyph: String,
    pub color: String,
    pub text: String,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct QuestionItem {
    pub id: String,
    pub prompt: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub options: Option<Vec<String>>,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct ApprovalView {
    /// `"<agent>:<json of the request id>"` — stable while the approval is pending.
    pub key: String,
    pub agent: u32,
    pub agent_name: String,
    pub method: String,
    /// "command" | "patch" | "permission" | "question" | "other"
    pub kind: String,
    pub title: String,
    pub detail: String,
    pub at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub questions: Option<Vec<QuestionItem>>,
    pub decisions: Vec<String>,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct ToastView {
    pub text: String,
    pub level: String,
    pub at: u64,
    pub ttl_ms: u64,
}

/// The relay side of the house (design §4.4). Built by `remote::Remote::info`; never sent to a
/// relay connection (it carries the password and the key-bearing link).
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug, Default)]
pub struct RemoteInfo {
    pub enabled: bool,
    pub relay: String,
    pub connected: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sid: Option<String>,
    /// The sid grouped `xxxx-xxxx-xxxx-xxxx-xxxx-xxxxxx`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
    /// QR modules of the link, one string of '0'/'1' per row.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub qr: Option<Vec<String>>,
    pub clients: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

// ───────────────────────────── builders ─────────────────────────────

/// Facts about the web layer itself that the App does not know.
#[derive(Clone, Debug, Default)]
pub struct Env {
    pub tls: bool,
    pub listen: Option<String>,
    pub headless: bool,
    pub push: bool,
}

pub fn item_kind(k: &Kind) -> &'static str {
    match k {
        Kind::User => "user",
        Kind::Agent => "agent",
        Kind::Reasoning => "reasoning",
        Kind::Plan => "plan",
        Kind::Command { .. } => "command",
        Kind::Files { .. } => "files",
        Kind::Tool { .. } => "tool",
        Kind::Web { .. } => "web",
        Kind::Notice { .. } => "notice",
        Kind::Compaction { .. } => "compaction",
    }
}

pub fn item_view(ord: u64, it: &Item) -> ItemView {
    let mut v = ItemView {
        ord,
        id: it.id.clone(),
        kind: item_kind(&it.kind).into(),
        text: it.text.clone(),
        done: it.done,
        v: it.version,
        cmd: None,
        output: None,
        exit: None,
        status: None,
        dur_ms: None,
        changes: None,
        tool: None,
        query: None,
        level: None,
        from: None,
        to: None,
    };
    match &it.kind {
        Kind::Command { cmd, output, exit, status, dur_ms } => {
            v.cmd = Some(cmd.clone());
            v.output = Some(output.clone());
            v.exit = *exit;
            v.status = Some(status.clone());
            v.dur_ms = *dur_ms;
        }
        Kind::Files { changes, status } => {
            v.changes = Some(
                changes
                    .iter()
                    .map(|c| {
                        let (adds, dels) = crate::util::diff_stats(&c.diff);
                        ChangeView { path: c.path.clone(), kind: c.kind.clone(), adds, dels }
                    })
                    .collect(),
            );
            v.status = Some(status.clone());
        }
        Kind::Tool { name, args, result, status } => v.tool = Some(ToolView { name: name.clone(), args: args.clone(), result: result.clone(), status: status.clone() }),
        Kind::Web { query } => v.query = Some(query.clone()),
        Kind::Notice { level } => v.level = Some(level_str(level).into()),
        Kind::Compaction { from, to } => {
            v.from = Some(*from);
            v.to = *to;
        }
        _ => {}
    }
    v
}

/// Cheap change detector for an item: equal fingerprints mean "nothing to send" without building
/// the view. (`version` is bumped on most mutations, the lengths catch the rest.)
#[derive(Clone, Copy, PartialEq, Debug)]
struct Fp(u64, bool, usize, usize);

impl Fp {
    fn of(it: &Item) -> Fp {
        let extra = match &it.kind {
            Kind::Command { output, status, exit, .. } => output.len().wrapping_add(status.len() << 20).wrapping_add(exit.map(|e| e as usize).unwrap_or(7) << 40),
            Kind::Files { changes, status } => changes.len().wrapping_add(status.len() << 20),
            Kind::Tool { result, status, .. } => result.len().wrapping_add(status.len() << 20),
            Kind::Compaction { to, .. } => to.map(|t| t as usize).unwrap_or(0),
            _ => 0,
        };
        Fp(it.version, it.done, it.text.len(), extra)
    }
}

/// The smallest update from `old` to `new`: an append when only the streamed text grew.
pub fn item_delta(old: Option<&ItemView>, new: &ItemView) -> ItemDelta {
    if let Some(o) = old {
        let streaming = matches!(new.kind.as_str(), "agent" | "reasoning" | "plan" | "user" | "notice");
        if streaming && o.kind == new.kind && o.id == new.id && new.text.len() > o.text.len() && new.text.starts_with(&o.text) {
            let mut rest = new.clone();
            rest.text = o.text.clone();
            rest.v = o.v;
            rest.done = o.done;
            if &rest == o {
                return ItemDelta::Append(ItemAppend { ord: new.ord, id: new.id.clone(), v: new.v, done: new.done, append: new.text[o.text.len()..].to_string() });
            }
        }
        if new.kind == "command" && o.kind == "command" {
            if let (Some(oo), Some(no)) = (&o.output, &new.output) {
                if no.len() > oo.len() && no.starts_with(oo.as_str()) {
                    let mut rest = new.clone();
                    rest.output = o.output.clone();
                    rest.v = o.v;
                    rest.done = o.done;
                    if &rest == o {
                        return ItemDelta::Append(ItemAppend { ord: new.ord, id: new.id.clone(), v: new.v, done: new.done, append: no[oo.len()..].to_string() });
                    }
                }
            }
        }
    }
    ItemDelta::Full(Box::new(new.clone()))
}

fn status_str(s: &Status) -> (&'static str, Option<String>) {
    match s {
        Status::Starting => ("starting", None),
        Status::Idle => ("idle", None),
        Status::Busy => ("busy", None),
        Status::Waiting => ("waiting", None),
        Status::Retrying(m) => ("retrying", Some(m.clone())),
        Status::Failed(m) => ("failed", Some(m.clone())),
        Status::Crashed(m) => ("crashed", Some(m.clone())),
        Status::Stopped => ("stopped", None),
    }
}

fn backend_str(k: crate::config::ProviderKind) -> &'static str {
    match k {
        crate::config::ProviderKind::Codex => "codex",
        crate::config::ProviderKind::ClaudeCode => "claude-code",
    }
}

fn worker_views(r: &Run) -> Vec<WorkerView> {
    // Latest attempt per task id, in first-seen order.
    let mut order: Vec<String> = vec![];
    let mut by_id: HashMap<String, WorkerView> = HashMap::new();
    for w in &r.workers {
        let report = w.report.trim();
        let view = WorkerView {
            task_id: w.task.id.clone(),
            title: w.task.title.clone(),
            role: w.task.role.clone(),
            agent: w.agent,
            attempt: w.attempt,
            state: crate::engine::run::worker_state_str(&w.state).into(),
            error: match &w.state {
                crate::engine::run::WState::Failed(e) => Some(e.clone()),
                _ => None,
            },
            branch: w.branch.clone(),
            adhoc: w.adhoc,
            paused: w.paused,
            tripwires: w.tripwires.clone(),
            spawned_at: ms(w.spawned),
            finished_at: w.finished.map(ms),
            report_tail: (!report.is_empty()).then(|| tail_chars(report, 600)),
        };
        if !by_id.contains_key(&w.task.id) {
            order.push(w.task.id.clone());
        }
        by_id.insert(w.task.id.clone(), view);
    }
    order.into_iter().filter_map(|id| by_id.remove(&id)).collect()
}

fn role_kind(app: &App, a: AgentId) -> String {
    if Some(a) == app.solo {
        return "solo".into();
    }
    if Some(a) == app.studio.architect {
        return "architect".into();
    }
    if app.probes.contains_key(&a) {
        return "probe".into();
    }
    if let Some(r) = &app.run {
        if matches!(r.stage, Stage::Finale { .. }) && (Some(a) == r.finale_agent) {
            return "finale".into();
        }
        if Some(a) == r.planner {
            return "planner".into();
        }
        if Some(a) == r.manager {
            return "manager".into();
        }
        if Some(a) == r.orchestrator {
            return "orchestrator".into();
        }
        if Some(a) == r.gate_agent {
            return "gate".into();
        }
        if r.workers.iter().any(|w| w.agent == Some(a)) {
            return "worker".into();
        }
        if Some(a) == r.finale_agent {
            return "finale".into();
        }
    }
    app.agents.get(&a).map(|x| if x.role.is_empty() { "other".to_string() } else { x.role.clone() }).unwrap_or_else(|| "other".into())
}

/// Per-publish facts about the run that every agent view needs (computed once, not per agent).
struct RunFacts {
    members: HashSet<AgentId>,
    expected: HashSet<AgentId>,
    workers: HashMap<AgentId, WorkerView>,
}

fn run_facts(app: &App) -> RunFacts {
    match &app.run {
        Some(r) => RunFacts {
            members: r.all_agents().into_iter().collect(),
            expected: r.expected_active().into_iter().map(|(a, _)| a).collect(),
            workers: worker_views(r).into_iter().filter_map(|w| Some((w.agent?, w))).collect(),
        },
        None => RunFacts { members: HashSet::new(), expected: HashSet::new(), workers: HashMap::new() },
    }
}

fn agent_view_with(app: &App, a: &Agent, facts: &RunFacts) -> AgentView {
    let m = app.registry.resolve(&a.model_alias);
    let (status, status_detail) = status_str(&a.status);
    let in_run = facts.members.contains(&a.id);
    let run = app.run.as_ref().filter(|_| in_run);
    let (plan_done, plan_total) = a.plan_progress();
    let files: Vec<FileView> = a.files.iter().map(|(p, f)| FileView { path: p.clone(), adds: f.adds, dels: f.dels, kind: f.kind.clone() }).collect();
    let worker = facts.workers.get(&a.id).cloned();
    AgentView {
        id: a.id,
        name: a.name.clone(),
        role: a.role.clone(),
        glyph: a.glyph.clone(),
        color: a.color.clone(),
        model_alias: a.model_alias.clone(),
        model: a.model.clone(),
        provider: a.provider.clone(),
        provider_name: app.registry.provider_name(&a.provider),
        backend: backend_str(a.backend).into(),
        effort: a.effort.clone(),
        efforts: m.efforts(),
        status: status.into(),
        status_detail,
        busy: a.busy(),
        activity: a.activity.clone(),
        turn_active: a.turn_active,
        awaiting_start: a.awaiting_start,
        compacting: a.compacting,
        compact_pending: a.compact_pending,
        stopped_by_user: a.stopped_by_user,
        turn_started_at: a.turn_started.map(ms),
        last_event_at: ms(a.last_event),
        created_at: ms(a.created),
        finished_at: a.finished.map(ms),
        quiet_ms: crate::ui::quiet_for(a).map(secs_ms),
        turn_count: a.turn_count,
        tokens_total: a.tokens_total,
        ctx_used: a.ctx_used,
        ctx_window: a.ctx_window,
        ctx_percent: a.ctx_percent(),
        compact_percent: m.effective_compact_percent(),
        ctx_assumed: m.context_window.is_none(),
        plan: a.plan.iter().map(|(t, s)| PlanStep { text: t.clone(), status: s.clone() }).collect(),
        plan_done,
        plan_total,
        files_adds: files.iter().map(|f| f.adds).sum(),
        files_dels: files.iter().map(|f| f.dels).sum(),
        files,
        queued: a.queued.clone(),
        retry_note: a.retry_note.clone(),
        final_message: a.final_message.as_ref().map(|f| crate::util::trunc(f, 2000)),
        in_run,
        run_name: run.map(|r| r.name_of(a.id)),
        role_kind: role_kind(app, a.id),
        worker,
        expected: facts.expected.contains(&a.id),
        waiting_answer: run.map(|r| r.waiting_for_answer(a.id)).unwrap_or(false),
        asked: run.and_then(|r| r.question_of(a.id)).map(|s| s.to_string()),
        watchdog_idle_ms: run.and_then(|r| r.watchdog_idle(a.id)).map(secs_ms),
        approval_pending: app.approvals.iter().any(|x| x.agent == a.id),
        items_first: a.items_first_ord(),
        items_total: a.items_total_ord(),
        thread_id: a.thread_id.clone(),
        cwd: a.cwd.to_string_lossy().to_string(),
    }
}

fn stage_view(s: &Stage) -> StageView {
    let label = crate::engine::state::stage_label(s);
    let mut v = StageView { kind: String::new(), label, phase: None, step: None, round: None, finale: None, error: None };
    v.kind = match s {
        Stage::Setup => "setup",
        Stage::Planning => "planning",
        Stage::Review => "review",
        Stage::Phase { idx, step } => {
            v.phase = Some(*idx);
            let (name, round) = match step {
                PhaseStep::Orchestrating => ("orchestrating", None),
                PhaseStep::Merging => ("merging", None),
                PhaseStep::Checks { round } => ("checks", Some(*round)),
                PhaseStep::Gate { round } => ("gate", Some(*round)),
                PhaseStep::Handoff => ("handoff", None),
            };
            v.step = Some(name.into());
            v.round = round;
            "phase"
        }
        Stage::Finale { idx } => {
            v.finale = Some(*idx);
            "finale"
        }
        Stage::Done => "done",
        Stage::Failed(e) => {
            v.error = Some(e.clone());
            "failed"
        }
    }
    .into();
    v
}

/// The TUI's key names in the halt hint, turned into the web UI's button names.
fn web_hint(hint: &str) -> String {
    let mut h = hint.replace("space ", "Resume ").replace("then space", "then Resume");
    if h == "space resume" || h == "Resume resume" {
        h = "Resume".into();
    }
    h.replace("r respawn ", "Respawn ").replace("r retry", "Retry").replace("m switch model ", "Switch model ").replace("space retries", "Resume retries")
}

pub fn run_view(app: &App, r: &Run) -> RunView {
    let halted = r.halt.as_ref().map(|h| HaltView {
        reason: r.halt_reason_str().unwrap_or("user").into(),
        agent: h.agent,
        agent_name: h.agent.map(|a| r.name_of(a)),
        message: h.message.clone(),
        since_at: ms(h.since),
        hint: web_hint(&r.halt_hint()),
    });
    let question = r.question.as_ref().map(|q| QuestionView { from: q.from, from_name: r.name_of(q.from), text: q.text.clone(), since_at: ms(q.since) });
    let history = r
        .history
        .iter()
        .map(|h| {
            let done = h.workers.iter().filter(|w| w.3).count();
            HistoryView {
                phase: h.name.clone(),
                tasks_done: done,
                tasks_total: h.workers.len(),
                rounds: 0,
                summary: format!("{done}/{} tasks ok · {}", h.workers.len(), crate::util::fmt_dur(h.duration)),
                duration_ms: h.duration.as_millis() as u64,
                ended_at: ms(h.ended),
                tasks: h.workers.iter().map(|w| HistoryTask { id: w.0.clone(), title: w.1.clone(), glyph: w.2.clone(), ok: w.3 }).collect(),
            }
        })
        .collect();
    let members = r.all_agents();
    RunView {
        id: r.id.clone(),
        pattern: r.pattern.name.clone(),
        brief: r.brief.clone(),
        stage: stage_view(&r.stage),
        active: r.is_active(),
        halted,
        question,
        want_review: r.want_review || r.stage == Stage::Review,
        plan_version: r.plan_version,
        planner: r.planner,
        manager: r.manager,
        orchestrator: r.orchestrator,
        gate_agent: r.gate_agent,
        finale_agent: r.finale_agent,
        workers: worker_views(r),
        history,
        checks: r.checks.iter().map(|c| CheckView { cmd: c.cmd.clone(), ok: c.ok, output_tail: tail_chars(&c.output, 600) }).collect(),
        conflicts: r.conflicts.clone(),
        alerts: r.alerts.clone(),
        started_at: r.started_unix.saturating_mul(1000),
        elapsed_ms: secs_ms(r.elapsed()),
        total_tokens: r.total_tokens(|a| app.agents.get(&a).map(|x| x.tokens_total).unwrap_or(0)),
        busy_count: members.iter().filter(|a| app.agents.get(a).map(|x| x.busy()).unwrap_or(false)).count(),
        workspace: r.workspace_dir().to_string_lossy().to_string(),
        branch: format!("mantra/{}", r.id),
        landable: r.stage == Stage::Done,
        pulse_seq: r.pulse_seq,
    }
}

pub fn plan_view(r: &Run) -> Option<PlanView> {
    let p = r.plan.as_ref()?;
    Some(PlanView {
        version: r.plan_version,
        title: p.title.clone(),
        summary: p.summary.clone(),
        orchestrator_brief: p.orchestrator_brief.clone(),
        phases: p
            .phases
            .iter()
            .map(|ph| PhaseView {
                id: ph.id.clone(),
                name: ph.name.clone(),
                goal: ph.goal.clone(),
                tasks: ph.tasks.iter().map(|t| TaskView { id: t.id.clone(), title: t.title.clone(), role: t.role.clone(), scope: t.scope.clone(), acceptance: t.acceptance.clone(), effort: t.effort.clone() }).collect(),
                gate: GateView { checks: ph.gate.checks.clone(), focus: ph.gate.focus.clone(), criteria: ph.gate.criteria.clone() },
            })
            .collect(),
        final_checks: p.final_checks.clone(),
        markdown: p.to_markdown(),
    })
}

pub fn approval_key(ap: &Approval) -> String {
    format!("{}:{}", ap.agent, serde_json::to_string(&ap.id).unwrap_or_default())
}

pub fn approval_view(app: &App, ap: &Approval) -> ApprovalView {
    let kind = match ap.method.as_str() {
        "item/commandExecution/requestApproval" | "execCommandApproval" => "command",
        "item/fileChange/requestApproval" | "applyPatchApproval" => "patch",
        "item/permissions/requestApproval" => "permission",
        "item/tool/requestUserInput" => "question",
        _ => "other",
    };
    let questions = (kind == "question").then(|| {
        ap.params
            .get("questions")
            .and_then(|q| q.as_array())
            .map(|qs| {
                qs.iter()
                    .map(|q| {
                        let s = |k: &str| q.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string();
                        let prompt = [s("question"), s("prompt"), s("header")].into_iter().find(|x| !x.is_empty()).unwrap_or_default();
                        let options = q.get("options").and_then(|o| o.as_array()).map(|os| {
                            os.iter().filter_map(|o| o.as_str().map(|s| s.to_string()).or_else(|| o.get("label").and_then(|l| l.as_str()).map(|s| s.to_string()))).collect::<Vec<_>>()
                        });
                        QuestionItem { id: s("id"), prompt, options: options.filter(|o| !o.is_empty()) }
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
    });
    ApprovalView {
        key: approval_key(ap),
        agent: ap.agent,
        agent_name: app.agents.get(&ap.agent).map(|a| a.name.clone()).unwrap_or_default(),
        method: ap.method.clone(),
        kind: kind.into(),
        title: ap.title.clone(),
        detail: ap.detail.clone(),
        at: ms(ap.at),
        questions,
        decisions: ["yes", "session", "no", "cancel"].iter().map(|s| s.to_string()).collect(),
    }
}

fn screen_str(s: Screen) -> String {
    match s {
        Screen::Solo => "solo".into(),
        Screen::Stage => "stage".into(),
        Screen::Zoom(a) => format!("zoom:{a}"),
        Screen::Studio => "studio".into(),
        Screen::Models => "models".into(),
    }
}

pub fn app_info(app: &App, env: &Env, patterns: &[String]) -> AppInfo {
    AppInfo {
        version: env!("CARGO_PKG_VERSION").into(),
        protocol: super::protocol::PROTOCOL,
        project: app.project.to_string_lossy().to_string(),
        project_name: app.project.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default(),
        branch: app.branch.clone(),
        demo: app.demo,
        pattern_name: app.pattern_name.clone(),
        patterns: patterns.to_vec(),
        approval_mode: app.settings.approval_mode.clone(),
        default_model: app.settings.default_model.clone(),
        solo: app.solo,
        sandbox_warning: app.sandbox_warning.clone(),
        unfinished_runs: app.unfinished_runs.clone(),
        title: app.title(),
        started_at: anchor().1,
        tui_screen: screen_str(app.screen),
        models: app
            .registry
            .models
            .iter()
            .map(|m| ModelInfo {
                alias: m.alias.clone(),
                model: m.model.clone(),
                provider: m.provider.clone(),
                provider_name: app.registry.provider_name(&m.provider),
                backend: backend_str(app.registry.backend_of(m)).into(),
                efforts: m.efforts(),
                default_effort: m.default_effort.clone(),
                context_window: m.context_window,
                compact_percent: m.effective_compact_percent(),
                note: m.note.clone(),
                problem: app.registry.alias_problem(&m.alias),
            })
            .collect(),
        push_enabled: env.push,
        tls: env.tls,
        listen: env.listen.clone(),
        headless: env.headless,
    }
}

pub fn toast_view(app: &App) -> Option<ToastView> {
    let (text, at, level) = app.toast.as_ref()?;
    let ttl = crate::ui::toast_life(text);
    (at.elapsed().as_millis() < ttl).then(|| ToastView { text: text.clone(), level: level_str(level).into(), at: ms(*at), ttl_ms: ttl as u64 })
}

// ───────────────────────────── publisher ─────────────────────────────

/// Everything a new connection's snapshot contains apart from items and pulse.
#[derive(Clone, PartialEq, Debug)]
pub struct Snapshot {
    pub app: AppInfo,
    pub agents: BTreeMap<AgentId, AgentView>,
    pub run: Option<RunView>,
    pub plan: Option<PlanView>,
    pub approvals: Vec<ApprovalView>,
    pub toast: Option<ToastView>,
    pub remote: Option<RemoteInfo>,
}

#[derive(Default)]
struct ItemCache {
    /// Contiguous, ascending by `ord`, at most `ITEM_WINDOW` long.
    views: VecDeque<(Fp, ItemView)>,
}

pub struct Publisher {
    pub env: Env,
    seq: u64,
    prev: Option<Snapshot>,
    items: HashMap<AgentId, ItemCache>,
    pulse: VecDeque<PulseView>,
    pulse_sent: u64,
    pulse_run: Option<String>,
    plan_cache: Option<(String, u32, Option<PlanView>)>,
    patterns: Option<(Instant, Vec<String>)>,
}

impl Publisher {
    pub fn new(env: Env) -> Publisher {
        let _ = anchor();
        Publisher { env, seq: 0, prev: None, items: HashMap::new(), pulse: VecDeque::new(), pulse_sent: 0, pulse_run: None, plan_cache: None, patterns: None }
    }

    #[cfg(test)]
    pub fn seq(&self) -> u64 {
        self.seq
    }

    /// `Pattern::list` reads the disk; ten seconds of staleness is fine for a picker.
    fn patterns(&mut self, app: &App) -> Vec<String> {
        let fresh = self.patterns.as_ref().map(|(t, _)| t.elapsed() < Duration::from_secs(10)).unwrap_or(false);
        if !fresh {
            self.patterns = Some((Instant::now(), crate::engine::pattern::Pattern::list(&app.project)));
        }
        self.patterns.as_ref().map(|(_, p)| p.clone()).unwrap_or_default()
    }

    fn plan(&mut self, r: Option<&Run>) -> Option<PlanView> {
        let r = r?;
        let fresh = self.plan_cache.as_ref().map(|(id, v, _)| *id == r.id && *v == r.plan_version).unwrap_or(false);
        if !fresh {
            self.plan_cache = Some((r.id.clone(), r.plan_version, plan_view(r)));
        }
        self.plan_cache.as_ref().and_then(|(_, _, p)| p.clone())
    }

    pub fn build(&mut self, app: &App, remote: Option<RemoteInfo>) -> Snapshot {
        let patterns = self.patterns(app);
        let facts = run_facts(app);
        let plan = self.plan(app.run.as_ref());
        Snapshot {
            app: app_info(app, &self.env, &patterns),
            agents: app.agents.values().map(|a| (a.id, agent_view_with(app, a, &facts))).collect(),
            run: app.run.as_ref().map(|r| run_view(app, r)),
            plan,
            approvals: app.approvals.iter().map(|ap| approval_view(app, ap)).collect(),
            toast: toast_view(app),
            remote,
        }
    }

    /// Diff the App against the last publish. `None` when nothing changed (or on the very first
    /// call, which only primes the caches — no connection can have joined before it).
    pub fn publish(&mut self, app: &App, remote: Option<RemoteInfo>) -> Option<DeltaMsg> {
        let snap = self.build(app, remote);
        let items = self.diff_items(app);
        let pulse = self.diff_pulse(app.run.as_ref());
        let prev = self.prev.replace(snap)?;
        let snap = self.prev.as_ref()?;
        let mut d = DeltaMsg::default();
        if snap.app != prev.app {
            d.app = Some(snap.app.clone());
        }
        let changed: Vec<AgentView> = snap.agents.iter().filter(|(id, v)| prev.agents.get(id) != Some(v)).map(|(_, v)| v.clone()).collect();
        if !changed.is_empty() {
            d.agents = Some(changed);
        }
        let removed: Vec<u32> = prev.agents.keys().filter(|id| !snap.agents.contains_key(id)).copied().collect();
        if !removed.is_empty() {
            d.agents_removed = Some(removed);
        }
        if !items.is_empty() {
            d.items = Some(items);
        }
        if snap.run != prev.run {
            d.run = Some(snap.run.clone());
        }
        if snap.plan != prev.plan {
            d.plan = Some(snap.plan.clone());
        }
        if !pulse.is_empty() {
            d.pulse = Some(pulse);
        }
        if snap.approvals != prev.approvals {
            d.approvals = Some(snap.approvals.clone());
        }
        if snap.toast != prev.toast {
            d.toast = Some(snap.toast.clone());
        }
        if snap.remote != prev.remote {
            d.remote = Some(snap.remote.clone());
        }
        if d.is_empty() {
            return None;
        }
        self.seq += 1;
        d.seq = self.seq;
        Some(d)
    }

    fn diff_items(&mut self, app: &App) -> BTreeMap<String, Vec<ItemDelta>> {
        let mut out = BTreeMap::new();
        self.items.retain(|id, _| app.agents.contains_key(id));
        for a in app.agents.values() {
            let first = a.items_first_ord();
            let total = a.items_total_ord();
            let start = first.max(total.saturating_sub(ITEM_WINDOW));
            let cache = self.items.entry(a.id).or_default();
            // Keep the cache contiguous with the window; anything else (a burst of more than a
            // window between publishes) just starts over — those items go out in full.
            let front = cache.views.front().map(|(_, v)| v.ord);
            let back = cache.views.back().map(|(_, v)| v.ord);
            if matches!((front, back), (Some(f), Some(b)) if f > start || b.saturating_add(1) < start || b >= total) {
                cache.views.clear();
            }
            while cache.views.front().map(|(_, v)| v.ord < start).unwrap_or(false) {
                cache.views.pop_front();
            }
            let mut deltas = vec![];
            for ord in start..total {
                let Some(it) = a.items.get((ord - first) as usize) else { break };
                let fp = Fp::of(it);
                let pos = cache.views.front().map(|(_, v)| ord.checked_sub(v.ord)).unwrap_or(None).map(|p| p as usize);
                match pos.and_then(|p| cache.views.get_mut(p)) {
                    Some(slot) if slot.0 == fp => {}
                    Some(slot) => {
                        let view = item_view(ord, it);
                        if view != slot.1 {
                            deltas.push(item_delta(Some(&slot.1), &view));
                        }
                        *slot = (fp, view);
                    }
                    None => {
                        let view = item_view(ord, it);
                        deltas.push(ItemDelta::Full(Box::new(view.clone())));
                        cache.views.push_back((fp, view));
                    }
                }
            }
            if !deltas.is_empty() {
                out.insert(a.id.to_string(), deltas);
            }
        }
        out
    }

    fn diff_pulse(&mut self, run: Option<&Run>) -> Vec<PulseView> {
        let id = run.map(|r| r.id.clone());
        if id != self.pulse_run {
            self.pulse_run = id;
            self.pulse.clear();
            self.pulse_sent = 0;
        }
        let Some(r) = run else { return vec![] };
        let new: Vec<PulseView> = r
            .pulse
            .iter()
            .filter(|p| p.n > self.pulse_sent)
            .map(|p| PulseView { n: p.n, at: ms(p.at), t: p.t.clone(), glyph: p.glyph.clone(), color: p.color.to_string(), text: p.text.clone() })
            .collect();
        if let Some(last) = new.last() {
            self.pulse_sent = last.n;
        }
        for p in &new {
            self.pulse.push_back(p.clone());
        }
        while self.pulse.len() > SNAPSHOT_PULSE {
            self.pulse.pop_front();
        }
        new
    }

    /// A full snapshot for a (re)connecting client, consistent with `seq`: the next delta it will
    /// see is `seq + 1`. `with_remote = false` for relay connections.
    pub fn full(&self, with_remote: bool) -> Option<SnapshotMsg> {
        let s = self.prev.as_ref()?;
        let agents = s
            .agents
            .values()
            .map(|v| {
                let items = self.items.get(&v.id).map(|c| c.views.iter().skip(c.views.len().saturating_sub(SNAPSHOT_ITEMS)).map(|(_, i)| i.clone()).collect()).unwrap_or_default();
                AgentWithItems { agent: v.clone(), items }
            })
            .collect();
        Some(SnapshotMsg {
            seq: self.seq,
            app: s.app.clone(),
            agents,
            run: s.run.clone(),
            plan: s.plan.clone(),
            approvals: s.approvals.clone(),
            toast: s.toast.clone(),
            remote: if with_remote { s.remote.clone() } else { None },
            pulse: self.pulse.iter().cloned().collect(),
        })
    }
}

/// `fetch_items`: up to `count` items older than `before`, ascending.
pub fn items_before(a: &Agent, before: u64, count: usize) -> Vec<ItemView> {
    let first = a.items_first_ord();
    let end = before.min(a.items_total_ord());
    if end <= first {
        return vec![];
    }
    let start = end.saturating_sub(count as u64).max(first);
    (start..end).filter_map(|ord| a.items.get((ord - first) as usize).map(|it| item_view(ord, it))).collect()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::config::{Registry, Settings};
    use crate::engine::pattern::Pattern;
    use crate::hub::Hub;
    use std::path::PathBuf;

    pub(crate) fn item(ord: u64) -> ItemView {
        ItemView { ord, id: format!("i{ord}"), kind: "agent".into(), text: "hello".into(), done: false, v: 1, cmd: None, output: None, exit: None, status: None, dur_ms: None, changes: None, tool: None, query: None, level: None, from: None, to: None }
    }

    pub(crate) fn test_app() -> App {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let (ev_tx, _ev_rx) = tokio::sync::mpsc::unbounded_channel();
        let hub = Hub::new(vec![], vec![], ev_tx, false);
        App::new(Settings::default(), Registry::defaults(), PathBuf::from("."), hub, tx, true)
    }

    pub(crate) fn add_agent(app: &mut App, id: AgentId, name: &str) {
        let mut a = Agent::new(id, name, "worker", PathBuf::from("."));
        a.status = Status::Idle;
        app.agents.insert(id, a);
    }

    /// Feed an agent a streamed agent message (the path Codex notifications take).
    pub(crate) fn stream(app: &mut App, id: AgentId, item_id: &str, delta: &str) {
        let a = app.agents.get_mut(&id).unwrap();
        a.apply("item/agentMessage/delta", &serde_json::json!({"itemId": item_id, "delta": delta}));
    }

    pub(crate) fn sample_messages() -> (SnapshotMsg, DeltaMsg) {
        let mut app = test_app();
        add_agent(&mut app, 1, "solo");
        app.agents.get_mut(&1).unwrap().push_user("hi");
        let mut p = Publisher::new(Env::default());
        p.publish(&app, Some(RemoteInfo { enabled: true, relay: "wss://r".into(), qr: Some(vec!["101".into()]), ..Default::default() }));
        stream(&mut app, 1, "m1", "hello");
        let d = p.publish(&app, None).expect("delta");
        (p.full(true).unwrap(), d)
    }

    #[test]
    fn nothing_changed_means_no_delta() {
        let mut app = test_app();
        add_agent(&mut app, 1, "solo");
        let mut p = Publisher::new(Env::default());
        assert!(p.publish(&app, None).is_none(), "the first publish only primes");
        assert!(p.publish(&app, None).is_none());
        assert_eq!(p.seq(), 0);
    }

    #[test]
    fn agents_are_upserted_and_removed() {
        let mut app = test_app();
        add_agent(&mut app, 1, "a");
        let mut p = Publisher::new(Env::default());
        p.publish(&app, None);
        add_agent(&mut app, 2, "b");
        let d = p.publish(&app, None).unwrap();
        assert_eq!(d.seq, 1);
        assert_eq!(d.agents.as_ref().map(|v| v.iter().map(|a| a.id).collect::<Vec<_>>()), Some(vec![2]));
        app.agents.get_mut(&1).unwrap().activity = "thinking".into();
        app.agents.remove(&2);
        let d = p.publish(&app, None).unwrap();
        assert_eq!(d.seq, 2);
        assert_eq!(d.agents.as_ref().unwrap()[0].id, 1);
        assert_eq!(d.agents_removed, Some(vec![2]));
    }

    #[test]
    fn streamed_text_goes_out_as_an_append_after_the_first_full_item() {
        let mut app = test_app();
        add_agent(&mut app, 1, "a");
        let mut p = Publisher::new(Env::default());
        p.publish(&app, None);
        stream(&mut app, 1, "m1", "Hel");
        let d = p.publish(&app, None).unwrap();
        let it = &d.items.as_ref().unwrap()["1"];
        assert!(matches!(&it[0], ItemDelta::Full(v) if v.text == "Hel" && v.ord == 0), "{it:?}");
        stream(&mut app, 1, "m1", "lo");
        let d = p.publish(&app, None).unwrap();
        let it = &d.items.as_ref().unwrap()["1"];
        assert!(matches!(&it[0], ItemDelta::Append(a) if a.append == "lo" && a.ord == 0), "{it:?}");
        // a changed non-prefix text is sent whole
        let a = app.agents.get_mut(&1).unwrap();
        a.items[0].text = "Bye".into();
        a.items[0].version += 1;
        let d = p.publish(&app, None).unwrap();
        assert!(matches!(&d.items.as_ref().unwrap()["1"][0], ItemDelta::Full(v) if v.text == "Bye"));
    }

    #[test]
    fn ordinals_survive_the_front_trim() {
        let mut a = Agent::new(1, "a", "w", PathBuf::from("."));
        for i in 0..3001 {
            a.push_user(&format!("m{i}"));
        }
        assert_eq!(a.items_first_ord(), 600);
        assert_eq!(a.items_total_ord(), 3001);
        let older = items_before(&a, 700, 50);
        assert_eq!(older.len(), 50);
        assert_eq!(older[0].ord, 650);
        assert_eq!(older[0].text, "m650");
        assert!(items_before(&a, 600, 10).is_empty(), "trimmed items are gone");
    }

    #[test]
    fn pulse_lines_are_sent_once_by_sequence_number() {
        let mut app = test_app();
        let mut p = Publisher::new(Env::default());
        let mut run = crate::engine::run::Run::new(PathBuf::from("."), Pattern::builtin(), "goal".into());
        run.dir = std::env::temp_dir().join(format!("mantra-web-pulse-{}", std::process::id()));
        run.log("✦", "saffron", "one");
        app.run = Some(run);
        p.publish(&app, None);
        app.run.as_mut().unwrap().log("✦", "saffron", "two");
        let d = p.publish(&app, None).unwrap();
        let pulse = d.pulse.unwrap();
        assert_eq!(pulse.iter().map(|l| l.text.as_str()).collect::<Vec<_>>(), vec!["two"]);
        assert_eq!(pulse[0].n, 2);
        assert!(d.run.is_some(), "pulse_seq moved, so the run view changed too");
        assert!(p.publish(&app, None).map(|d| d.pulse.is_none()).unwrap_or(true));
        assert_eq!(p.full(true).unwrap().pulse.len(), 2);
        let _ = std::fs::remove_dir_all(&app.run.as_ref().unwrap().dir);
    }

    #[test]
    fn the_plan_is_resent_only_when_its_version_changes() {
        let mut app = test_app();
        let mut p = Publisher::new(Env::default());
        let mut run = crate::engine::run::Run::new(PathBuf::from("."), Pattern::builtin(), "goal".into());
        run.plan = Some(crate::engine::plan::Plan { title: "T".into(), ..Default::default() });
        run.plan_version = 1;
        app.run = Some(run);
        p.publish(&app, None);
        app.agents.clear();
        assert!(p.publish(&app, None).is_none());
        app.run.as_mut().unwrap().plan.as_mut().unwrap().title = "T2".into();
        app.run.as_mut().unwrap().plan_version = 2;
        let d = p.publish(&app, None).unwrap();
        assert_eq!(d.plan.unwrap().unwrap().title, "T2");
        app.run = None;
        let d = p.publish(&app, None).unwrap();
        assert_eq!(d.plan, Some(None), "gone is an explicit null");
        assert_eq!(d.run, Some(None));
    }

    #[test]
    fn a_full_snapshot_carries_recent_items_and_strips_remote_for_relay() {
        let (snap, _) = sample_messages();
        assert_eq!(snap.agents[0].items.iter().map(|i| i.kind.as_str()).collect::<Vec<_>>(), vec!["user", "agent"]);
        let mut app = test_app();
        add_agent(&mut app, 1, "a");
        let mut p = Publisher::new(Env::default());
        p.publish(&app, Some(RemoteInfo { enabled: true, ..Default::default() }));
        assert!(p.full(true).unwrap().remote.is_some());
        assert!(p.full(false).unwrap().remote.is_none());
    }
}
