//! The Conductor: a deterministic state machine that runs a pattern.
//!
//! Mechanical work (spawning, retrying API errors, saving outputs, merging, gates, handoffs)
//! happens here — never inside an LLM loop. LLM agents are woken only for judgment calls.

use super::git::{self, CheckResult, Workspace};
use super::state::{AgentState, PhaseHistory, RunState, WorkerState};
use super::pattern::{Pattern, Role};
use super::plan::{Phase, Plan, Task};
use super::tools;
use crate::agent::{Agent, ErrKind, Status};
use crate::hub::AgentId;
use crate::util::{clock, fmt_dur, fmt_tokens, trunc};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::time::{Duration, Instant};

#[derive(Debug)]
pub enum JobTag {
    Setup,
    WorkerWt { task: String, attempt: u32 },
    Merge { phase: usize },
    Checks { phase: usize, round: u32 },
    Cleanup { phase: usize },
    Finish,
    Land,
}

pub enum JobOut {
    Setup(Result<Workspace, String>),
    Wt(Result<(PathBuf, String), String>),
    Merge(git::MergeResult),
    Checks(Vec<CheckResult>),
    Text(Result<String, String>),
}

pub struct SpawnReq {
    pub name: String,
    pub role_name: String,
    pub role: Role,
    pub cwd: PathBuf,
    pub instructions: String,
    pub tools: Vec<Value>,
    pub effort: Option<String>,
    pub extra_writable: Vec<PathBuf>,
    /// Explicit context-window override for this spawn (set after a ContextFull halving); `None`
    /// means "use the model's own effective context".
    pub context_override: Option<u64>,
}

/// How a message reaches an agent that might already be mid-turn. `Auto` is the engine's own
/// steering behaviour (immediate `turn/steer`) and is what every `ctx.prompt(...)` call in this
/// file uses. The UI uses `Queue` for a plain Enter — it waits behind the agent's current turn,
/// shown as a chip — and `Force` for ctrl+f, which delivers it (plus anything already queued)
/// into the running turn right away. Named `Send` per the design note; the two spots in this
/// codebase that need `std::marker::Send` instead spell it out to avoid shadowing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Send {
    Auto,
    Queue,
    Force,
}

/// What the engine needs from the app. Keeps the engine free of UI/process details (and testable).
pub trait Ctx {
    fn spawn(&mut self, req: SpawnReq) -> AgentId;
    /// Spawn an agent re-attached to a saved thread/session (`mantra runs resume`). The default
    /// just spawns fresh, which is always a correct (if more expensive) fallback.
    fn spawn_resumed(&mut self, req: SpawnReq, _thread: String) -> AgentId {
        self.spawn(req)
    }
    fn prompt(&mut self, a: AgentId, text: String);
    /// Like `prompt`, but lets a UI-originated message (queued or forced) say how it should
    /// reach an agent that is already busy. Engine call sites always use plain `prompt`.
    fn prompt_mode(&mut self, a: AgentId, text: String, mode: Send);
    fn interrupt(&mut self, a: AgentId);
    fn compact(&mut self, a: AgentId);
    fn stop(&mut self, a: AgentId, archive: bool);
    fn tool_result(&mut self, a: AgentId, req: Value, text: String, ok: bool);
    fn set_effort(&mut self, a: AgentId, effort: &str) -> String;
    fn agent(&self, a: AgentId) -> Option<&Agent>;
    fn job(&mut self, tag: JobTag, f: Box<dyn FnOnce() -> JobOut + std::marker::Send>);
    fn notify(&mut self, text: &str);
}

#[derive(Debug, Clone, PartialEq, Default, serde::Serialize, serde::Deserialize)]
pub enum PhaseStep {
    #[default]
    Orchestrating,
    Merging,
    Checks { round: u32 },
    Gate { round: u32 },
    Handoff,
}

#[derive(Debug, Clone, PartialEq, Default, serde::Serialize, serde::Deserialize)]
pub enum Stage {
    #[default]
    Setup,
    Planning,
    Review,
    Phase { idx: usize, step: PhaseStep },
    Finale { idx: usize },
    Done,
    Failed(String),
}

#[derive(Debug, Clone, PartialEq)]
pub enum WState {
    Queued,
    Preparing,
    Running,
    Retrying(Instant),
    Done,
    Failed(String),
    Cancelled,
}

pub struct Worker {
    pub task: Task,
    pub prompt: String,
    pub effort: Option<String>,
    pub agent: Option<AgentId>,
    pub attempt: u32,
    pub state: WState,
    pub report: String,
    pub wt: Option<PathBuf>,
    pub branch: String,
    pub spawned: Instant,
    pub finished: Option<Instant>,
    pub tripwires: Vec<String>,
    pub adhoc: bool,
    pub paused: bool,
    pub stall_flagged: bool,
    pub budget_flagged: bool,
    /// Set once a ContextFull error is seen while the assumed context was in effect; carried
    /// forward across attempts of the same task (see `spawn_task`).
    pub context_override: Option<u64>,
    /// Timestamps of `mantra_prompt` calls the orchestrator made at this worker while it was idle
    /// (turn ended but `WState` is still `Running` — an interrupted/failed turn the state machine
    /// hasn't otherwise reclassified). F3 loop protection (WP7.2): three within five minutes with
    /// no `completed` turn in between means the orchestrator is stuck nagging a dead worker —
    /// respawn it instead of forwarding another prompt.
    pub idle_prompts: Vec<Instant>,
}

/// What an agent is expected to be doing right now, per `Run::expected_active` (WP7.1) — the
/// watchdog's model of "who must be busy and why". `Stage::Review` and the mechanical phase steps
/// (`Merging`/`Checks`/`Handoff`) expect nobody and simply don't appear here.
#[derive(Debug, Clone, PartialEq)]
pub enum Expect {
    Planning,
    Orchestrating,
    Working(String),
    Gating,
    Finale(usize),
}

/// Per-agent watchdog progress: how far up the escalation ladder (WP7.2) this agent's *current*
/// idle spell has gone, and the `last_event` we last saw from it (any new event resets the ladder
/// back to the bottom — the plan text "cleared on any event from that agent").
struct WatchState {
    stage: u8,
    last_seen: Instant,
}

#[derive(Clone)]
pub struct Pulse {
    pub at: Instant,
    pub t: String,
    pub glyph: String,
    pub color: &'static str,
    pub text: String,
}

/// Why a run stopped making progress on its own and needs a person.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum HaltReason {
    /// The user pressed `space`.
    User,
    Auth,
    UsageLimit,
    /// A deterministic provider/model incompatibility (HTTP 400/422, "unexpected message role", …)
    /// — retrying cannot help; the role's model must change.
    ProviderRejected,
    /// The host environment itself can't run agents (WP12.4/L1): a command an agent ran failed
    /// naming `bwrap` / user namespaces, so the sandbox cannot start.
    Environment,
    GateExhausted,
    AttemptsExhausted,
    AgentTurnFailed,
}

/// A run-level halt: `Run.halt.is_some()` replaces the old bare `paused` flag with a reason, the
/// agent it's about (if any) and a message — so the UI can show *why* and *what to do*.
pub struct Halt {
    pub reason: HaltReason,
    pub agent: Option<AgentId>,
    pub message: String,
    pub since: Instant,
}

pub struct PhaseRecord {
    pub name: String,
    pub workers: Vec<(String, String, String, bool)>, // task id, title, glyph, ok
    pub duration: Duration,
    pub ended: Instant,
}

pub struct Run {
    pub id: String,
    pub dir: PathBuf,
    pub project: PathBuf,
    pub pattern: Pattern,
    pub brief: String,
    pub stage: Stage,
    /// `Some` while the run is halted (see `HaltReason`); `None` while running normally.
    pub halt: Option<Halt>,
    pub plan: Option<Plan>,
    pub plan_version: u32,
    pub planner: Option<AgentId>,
    pub orchestrator: Option<AgentId>,
    pub gate_agent: Option<AgentId>,
    pub finale_agent: Option<AgentId>,
    pub workers: Vec<Worker>,
    pub history: Vec<PhaseRecord>,
    pub checks: Vec<CheckResult>,
    pub conflicts: Vec<String>,
    pub gate_report: Option<(bool, String)>,
    pub pulse: VecDeque<Pulse>,
    pub started: Instant,
    pub phase_started: Instant,
    pub ws: Option<Workspace>,
    pub handoff: String,
    pub alerts: Vec<String>,
    pub edges: HashMap<AgentId, Instant>,
    pub want_review: bool,
    /// What `state.json` needs to re-attach agents after a resume: filled in `on_ready` (once the
    /// thread/session id is known) for every agent of this run.
    pub agent_meta: HashMap<AgentId, AgentState>,
    pub started_unix: u64,
    orch_inbox: Vec<String>,
    planner_nudges: u32,
    pub(super) handoff_note_done: bool,
    pub(super) cleanup_done: bool,
    continue_queue: Vec<(AgentId, Instant, String)>,
    retry_counts: HashMap<AgentId, u32>,
    paused_agents: Vec<AgentId>,
    adhoc_seq: u32,
    last_tick: Instant,
    tokens_prev: u64,
    /// Watchdog escalation state per agent currently expected active (WP7.2).
    watch: HashMap<AgentId, WatchState>,
    /// Roles ("planner" | "orchestrator" | "gate" | "finale") that have already been given one
    /// free respawn after a turn failure (WP7.3) — a second failure of the same role halts
    /// instead of looping forever. Cleared when that role completes a turn successfully.
    turn_fail_retried: HashSet<String>,
    /// First-80-chars signature of the last *failing* gate report's reason (L4/WP12.4): two
    /// consecutive identical signatures halt immediately instead of spending the remaining rounds.
    last_gate_blocker: Option<String>,
    /// Set when the watchdog wakes the planner about another agent's idleness (ladder step 3); if
    /// the planner produces no event by the deadline, step 4 halts the run.
    planner_watchdog: Option<(Instant, Instant)>,
}

impl Run {
    pub fn new(project: PathBuf, pattern: Pattern, brief: String) -> Run {
        let id = format!("{}-{}", crate::util::unix_secs() % 1_000_000, crate::util::slug(&brief).chars().take(24).collect::<String>().trim_matches('-'));
        let dir = crate::config::runs_dir(&project).join(&id);
        Run {
            id,
            dir,
            project,
            pattern,
            brief,
            stage: Stage::Setup,
            halt: None,
            plan: None,
            plan_version: 0,
            planner: None,
            orchestrator: None,
            gate_agent: None,
            finale_agent: None,
            workers: vec![],
            history: vec![],
            checks: vec![],
            conflicts: vec![],
            gate_report: None,
            pulse: VecDeque::new(),
            started: Instant::now(),
            phase_started: Instant::now(),
            ws: None,
            handoff: String::new(),
            alerts: vec![],
            edges: HashMap::new(),
            want_review: false,
            agent_meta: HashMap::new(),
            started_unix: crate::util::unix_secs(),
            orch_inbox: vec![],
            planner_nudges: 0,
            handoff_note_done: false,
            cleanup_done: false,
            continue_queue: vec![],
            retry_counts: HashMap::new(),
            paused_agents: vec![],
            adhoc_seq: 0,
            last_tick: Instant::now(),
            tokens_prev: 0,
            watch: HashMap::new(),
            turn_fail_retried: HashSet::new(),
            last_gate_blocker: None,
            planner_watchdog: None,
        }
    }

    // ───────────────────────────── bookkeeping ─────────────────────────────

    pub fn log(&mut self, glyph: &str, color: &'static str, text: impl Into<String>) {
        // Defensive: crash reasons and agent-supplied text can carry ANSI from a subprocess's
        // stderr; the journal and pulse feed must never show raw escape codes.
        let text = crate::util::strip_ansi(&text.into());
        let p = Pulse { at: Instant::now(), t: clock(), glyph: glyph.to_string(), color, text };
        crate::mlog!("[run {}] {} {}", self.id, p.glyph, p.text);
        let line = json!({"t": crate::util::unix_secs(), "glyph": p.glyph, "text": p.text}).to_string();
        let _ = std::fs::create_dir_all(&self.dir);
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(self.dir.join("journal.jsonl")) {
            use std::io::Write;
            let _ = writeln!(f, "{line}");
        }
        self.pulse.push_back(p);
        while self.pulse.len() > 300 {
            self.pulse.pop_front();
        }
    }

    fn save_plan(&self) {
        if let Some(p) = &self.plan {
            let _ = std::fs::create_dir_all(&self.dir);
            let _ = std::fs::write(self.dir.join("plan.json"), serde_json::to_string_pretty(p).unwrap_or_default());
            let _ = std::fs::write(self.dir.join("plan.md"), p.to_markdown());
        }
    }

    /// Persist everything a later `mantra runs resume` needs (see `engine::state`). Cheap: a few
    /// KB rewritten atomically at every transition.
    pub(super) fn save_state(&self) {
        let slot_of = |a: AgentId| -> String {
            if Some(a) == self.planner {
                "planner".into()
            } else if Some(a) == self.orchestrator {
                "orchestrator".into()
            } else if Some(a) == self.gate_agent {
                "gate".into()
            } else if let Some(w) = self.workers.iter().find(|w| w.agent == Some(a)) {
                format!("worker:{}#{}", w.task.id, w.attempt)
            } else if Some(a) == self.finale_agent {
                "finale".into()
            } else {
                String::new()
            }
        };
        let workers = self
            .workers
            .iter()
            .map(|w| {
                let (state, error) = match &w.state {
                    WState::Queued => ("queued", String::new()),
                    WState::Preparing => ("preparing", String::new()),
                    WState::Running => ("running", String::new()),
                    WState::Retrying(_) => ("retrying", String::new()),
                    WState::Done => ("done", String::new()),
                    WState::Failed(e) => ("failed", e.clone()),
                    WState::Cancelled => ("cancelled", String::new()),
                };
                let mut report = w.report.clone();
                crate::util::tail_bytes(&mut report, 6000);
                WorkerState {
                    task: w.task.id.clone(),
                    title: w.task.title.clone(),
                    role: w.task.role.clone(),
                    state: state.into(),
                    error,
                    attempt: w.attempt,
                    branch: w.branch.clone(),
                    worktree: w.wt.clone(),
                    adhoc: w.adhoc,
                    prompt: w.prompt.clone(),
                    effort: w.effort.clone(),
                    report,
                }
            })
            .collect();
        let live = self.all_agents();
        let agents = live
            .iter()
            .filter_map(|a| self.agent_meta.get(a).map(|m| AgentState { slot: slot_of(*a), ..m.clone() }))
            .filter(|m| !m.slot.is_empty())
            .collect();
        RunState {
            format: super::state::FORMAT,
            id: self.id.clone(),
            project: self.project.clone(),
            brief: self.brief.clone(),
            pattern: self.pattern.name.clone(),
            plan_version: self.plan_version,
            stage: self.stage.clone(),
            halted: self.halt.as_ref().map(|h| h.message.clone()),
            started_unix: self.started_unix,
            updated_unix: crate::util::unix_secs(),
            ws: self.ws.clone(),
            handoff: self.handoff.clone(),
            workers,
            agents,
            history: self.history.iter().map(|h| PhaseHistory { name: h.name.clone(), workers: h.workers.clone(), secs: h.duration.as_secs() }).collect(),
        }
        .save(&self.dir);
    }

    pub fn phase_idx(&self) -> Option<usize> {
        match &self.stage {
            Stage::Phase { idx, .. } => Some(*idx),
            _ => None,
        }
    }

    pub fn current_phase(&self) -> Option<&Phase> {
        let i = self.phase_idx()?;
        self.plan.as_ref()?.phases.get(i)
    }

    pub fn is_active(&self) -> bool {
        !matches!(self.stage, Stage::Done | Stage::Failed(_))
    }

    pub fn halted(&self) -> bool {
        self.halt.is_some()
    }

    /// Halt the run: interrupt whatever is busy, remember why, journal and alert. Every failure
    /// the run cannot handle by itself goes through here instead of a bare pause flag.
    fn halt(&mut self, ctx: &mut dyn Ctx, reason: HaltReason, agent: Option<AgentId>, message: String) {
        for a in self.all_agents() {
            if ctx.agent(a).map(|x| x.busy()).unwrap_or(false) {
                ctx.interrupt(a);
                if !self.paused_agents.contains(&a) {
                    self.paused_agents.push(a);
                }
            }
        }
        // The agent this halt is about may already be idle (its turn just failed) — still queue
        // it for a "continue where you left off" prompt once the run resumes.
        if let Some(a) = agent {
            if !self.paused_agents.contains(&a) {
                self.paused_agents.push(a);
            }
        }
        self.log("⛔", "red", format!("halted: {message}"));
        self.alerts.push(message.clone());
        ctx.notify(&format!("Mantra needs you: {message}"));
        self.halt = Some(Halt { reason, agent, message, since: Instant::now() });
        self.save_state();
    }

    /// Un-halt: re-prompt every agent that was interrupted for this, and pick the run back up
    /// wherever it left off. Used by `toggle_pause`'s resume path and by `m` (switch model) on a
    /// `ProviderRejected` halt.
    pub fn resume(&mut self, ctx: &mut dyn Ctx) {
        if self.halt.is_none() {
            return;
        }
        self.halt = None;
        self.alerts.clear();
        for a in std::mem::take(&mut self.paused_agents) {
            ctx.prompt(a, "[mantra:resume] The run was paused and is resuming now. Continue where you left off.".into());
        }
        self.log("▶", "green", "run resumed");
        self.save_state();
        if let Stage::Phase { idx, step } = self.stage.clone() {
            match step {
                PhaseStep::Orchestrating => {
                    self.fill_slots(ctx);
                    self.check_phase_done(ctx);
                }
                PhaseStep::Gate { .. } | PhaseStep::Checks { .. } => self.run_checks(ctx, idx, 1),
                _ => {}
            }
        }
        self.wake_orch(ctx);
    }

    /// Short "what to do" text for the halted state, shown in the stage header band and the alert.
    pub fn halt_hint(&self) -> String {
        let Some(h) = &self.halt else { return String::new() };
        match h.reason {
            HaltReason::User => "space resume".into(),
            HaltReason::Auth | HaltReason::UsageLimit => "fix credentials or quota, then space".into(),
            HaltReason::ProviderRejected => {
                let who = h.agent.map(|a| self.name_of(a)).unwrap_or_else(|| "the agent".into());
                if h.message.to_lowercase().contains("message role") {
                    format!("this provider rejects Codex's developer messages — use it through Claude Code (kind = claude-code) or m switch model for {who}")
                } else {
                    format!("m switch model for {who} · r retry")
                }
            }
            HaltReason::Environment => "fix the environment (see the message above), then r retry".into(),
            HaltReason::GateExhausted => "type feedback for the planner, or space to retry the gate".into(),
            HaltReason::AttemptsExhausted => {
                let who = h.agent.map(|a| self.name_of(a)).unwrap_or_else(|| "the task".into());
                format!("r retry {who} · type feedback for the planner")
            }
            HaltReason::AgentTurnFailed => {
                let who = h.agent.map(|a| self.name_of(a)).unwrap_or_else(|| "the agent".into());
                format!("r respawn {who} · space retries the turn")
            }
        }
    }

    /// Fixed tag for the non-worker roles the WP7.3 "one free respawn before halting" rule tracks
    /// — stable across a respawn (which changes the `AgentId`) since it's keyed by the *slot*
    /// (`self.planner`/`self.orchestrator`/…), not the id.
    fn watchdog_role_tag(&self, a: AgentId) -> Option<String> {
        // Same ambiguity as `respawn`: the shared planner/finale-agent id must resolve to
        // "finale" while a finale step is in progress, not "planner".
        if Some(a) == self.finale_agent || (Some(a) == self.planner && matches!(self.stage, Stage::Finale { .. })) {
            return Some("finale".into());
        }
        if Some(a) == self.planner {
            return Some("planner".into());
        }
        if Some(a) == self.orchestrator {
            return Some("orchestrator".into());
        }
        if Some(a) == self.gate_agent {
            return Some("gate".into());
        }
        None
    }

    /// The pattern role name behind an agent (not `name_of`'s task id / flow-slot label) — used to
    /// persist a model switch back into this run's pattern copy.
    pub fn role_name_of(&self, a: AgentId) -> Option<String> {
        if Some(a) == self.planner {
            return Some(self.pattern.flow.planner.clone());
        }
        if Some(a) == self.orchestrator {
            return Some(self.pattern.flow.orchestrator.clone());
        }
        if Some(a) == self.gate_agent {
            return Some(self.pattern.flow.phase_gate.clone());
        }
        if let Some(w) = self.workers.iter().find(|w| w.agent == Some(a)) {
            return Some(w.task.role.clone());
        }
        if Some(a) == self.finale_agent {
            if let Stage::Finale { idx } = self.stage {
                return self.pattern.flow.finale.get(idx).map(|s| s.role.clone());
            }
        }
        None
    }

    pub fn workspace_dir(&self) -> PathBuf {
        self.ws.as_ref().map(|w| w.integ.clone()).unwrap_or_else(|| self.project.clone())
    }

    pub(super) fn integ_writable(&self) -> Vec<PathBuf> {
        self.ws.as_ref().and_then(|w| w.git_common_dir.clone()).into_iter().collect()
    }

    pub fn all_agents(&self) -> Vec<AgentId> {
        let mut v: Vec<AgentId> = [self.planner, self.orchestrator, self.gate_agent, self.finale_agent].into_iter().flatten().collect();
        v.extend(self.workers.iter().filter_map(|w| w.agent));
        v.dedup();
        v
    }

    pub fn total_tokens(&self, ctx_tokens: impl Fn(AgentId) -> u64) -> u64 {
        self.tokens_prev + self.all_agents().into_iter().map(ctx_tokens).sum::<u64>()
    }

    pub(super) fn role(&self, name: &str) -> Role {
        self.pattern.role(name).cloned().unwrap_or_default()
    }

    fn resolve(&self, name: &str) -> Option<AgentId> {
        let n = name.trim().trim_start_matches('@').to_lowercase();
        match n.as_str() {
            "planner" => return self.planner,
            "orchestrator" | "orch" => return self.orchestrator,
            "gate" | "qa" => return self.gate_agent.or(self.finale_agent),
            _ => {}
        }
        if let Some(w) = self.workers.iter().rev().find(|w| w.task.id.to_lowercase() == n) {
            return w.agent;
        }
        if n == self.pattern.flow.phase_gate {
            return self.gate_agent;
        }
        None
    }

    pub fn name_of(&self, a: AgentId) -> String {
        if Some(a) == self.planner {
            return "planner".into();
        }
        if Some(a) == self.orchestrator {
            return "orchestrator".into();
        }
        if Some(a) == self.gate_agent {
            return self.pattern.flow.phase_gate.clone();
        }
        if let Some(w) = self.workers.iter().find(|w| w.agent == Some(a)) {
            return w.task.id.clone();
        }
        if Some(a) == self.finale_agent {
            return "finale".into();
        }
        format!("agent-{a}")
    }

    fn worker_idx(&self, a: AgentId) -> Option<usize> {
        self.workers.iter().position(|w| w.agent == Some(a))
    }

    /// F3 tool guard (WP7.5): once a worker's task is `Done`, refuse further steering tools on it
    /// unless the phase is still `Orchestrating` — once merging/checks/gate has begun the worker
    /// is about to be archived, and prompting it just starts a turn nobody will read.
    fn worker_done_guard(&self, t: AgentId) -> Option<&'static str> {
        let wi = self.worker_idx(t)?;
        let orchestrating = matches!(self.stage, Stage::Phase { step: PhaseStep::Orchestrating, .. });
        if self.workers[wi].state == WState::Done && !orchestrating {
            Some("task is done; the phase is merging/gating — wait for the handoff")
        } else {
            None
        }
    }

    pub(super) fn mark_edge(&mut self, a: AgentId) {
        self.edges.insert(a, Instant::now());
    }

    fn orch_event(&mut self, ctx: &mut dyn Ctx, text: String) {
        if matches!(self.stage, Stage::Finale { .. }) {
            if let Some(f) = self.finale_agent {
                ctx.prompt(f, format!("[mantra:event] {text}"));
            }
            return;
        }
        self.orch_inbox.push(text);
        self.wake_orch(ctx);
    }

    pub(super) fn wake_orch(&mut self, ctx: &mut dyn Ctx) {
        let Some(o) = self.orchestrator else { return };
        if self.orch_inbox.is_empty() || self.halted() {
            return;
        }
        let Some(a) = ctx.agent(o) else { return };
        if a.busy() || a.thread_id.is_none() || matches!(a.status, Status::Starting) {
            return;
        }
        let msgs = std::mem::take(&mut self.orch_inbox);
        let status = self.status_text(ctx);
        ctx.prompt(o, format!("[mantra:event]\n{}\n\nCurrent status:\n{status}\nDecide what (if anything) to do, then call mantra_wait.", msgs.join("\n")));
    }

    pub(super) fn status_text(&self, ctx: &dyn Ctx) -> String {
        let mut s = String::new();
        for w in &self.workers {
            let a = w.agent.and_then(|a| ctx.agent(a));
            let (act, prog, tok, el) = match a {
                Some(a) => {
                    let (d, t) = a.plan_progress();
                    (a.activity.clone(), if t > 0 { format!("{d}/{t}") } else { "-".into() }, fmt_tokens(a.tokens_total), fmt_dur(a.created.elapsed()))
                }
                None => ("-".into(), "-".into(), "0".into(), "-".into()),
            };
            let state = match &w.state {
                WState::Failed(m) => format!("FAILED ({})", trunc(m, 80)),
                WState::Retrying(_) => "retrying".into(),
                s => format!("{s:?}").to_lowercase(),
            };
            s.push_str(&format!("- {} [{}] {}: {state}, activity: {act}, steps {prog}, tokens {tok}, {el}{}\n", w.task.id, w.task.role, trunc(&w.task.title, 40), if w.paused { " (paused)" } else { "" }));
        }
        if s.is_empty() {
            s.push_str("- no workers yet\n");
        }
        s
    }

    /// Who must be busy right now, and why (WP7.1) — the watchdog's ground truth. An agent not in
    /// this list is never nudged no matter how long it has been idle (e.g. `Stage::Review`, where
    /// nobody is expected to act until the user does).
    pub fn expected_active(&self) -> Vec<(AgentId, Expect)> {
        let mut v = vec![];
        match &self.stage {
            Stage::Planning => {
                if let Some(p) = self.planner {
                    v.push((p, Expect::Planning));
                }
            }
            Stage::Phase { step, .. } => match step {
                PhaseStep::Orchestrating => {
                    for w in &self.workers {
                        if w.state == WState::Running {
                            if let Some(a) = w.agent {
                                v.push((a, Expect::Working(w.task.id.clone())));
                            }
                        }
                    }
                    if self.orchestrator_needed() {
                        if let Some(o) = self.orchestrator {
                            v.push((o, Expect::Orchestrating));
                        }
                    }
                }
                PhaseStep::Gate { .. } => {
                    if let Some(g) = self.gate_agent {
                        v.push((g, Expect::Gating));
                    }
                }
                PhaseStep::Merging | PhaseStep::Checks { .. } | PhaseStep::Handoff => {}
            },
            Stage::Finale { idx } => {
                if let Some(f) = self.finale_agent {
                    v.push((f, Expect::Finale(*idx)));
                }
            }
            Stage::Setup | Stage::Review | Stage::Done | Stage::Failed(_) => {}
        }
        v
    }

    /// WP7.6: the watchdog state to show on an agent's stage card, if any. `Some(idle)` once the
    /// escalation ladder has fired at least once for this agent's current idle spell (`stage >=
    /// 1` in `self.watch`) — the UI turns the card's border amber dotted and labels it `idle Nm ·
    /// watchdog`. Cleared the moment the agent produces a new event (`watchdog_tick` resets the
    /// entry's `stage` to 0), so a genuinely busy or freshly-active agent never shows it.
    pub fn watchdog_idle(&self, a: AgentId) -> Option<Duration> {
        let w = self.watch.get(&a)?;
        if w.stage == 0 {
            return None;
        }
        Some(w.last_seen.elapsed())
    }

    /// Is the orchestrator expected to act right now? Either it has queued events waiting
    /// (`orch_inbox`), or nothing is running/starting and some task of the current phase is
    /// queued, failed, or was never spawned — someone needs to decide what happens next. An
    /// orchestrator legitimately asleep in `mantra_wait` while workers run is not "idle".
    fn orchestrator_needed(&self) -> bool {
        if !self.orch_inbox.is_empty() {
            return true;
        }
        if self.workers.iter().any(|w| matches!(w.state, WState::Running | WState::Preparing | WState::Retrying(_))) {
            return false;
        }
        let Some(phase) = self.current_phase() else { return false };
        phase.tasks.iter().any(|t| match self.workers.iter().rev().find(|w| w.task.id == t.id) {
            None => true,
            Some(w) => matches!(w.state, WState::Queued | WState::Failed(_)),
        })
    }

    // ───────────────────────────── lifecycle ─────────────────────────────

    pub fn start(&mut self, ctx: &mut dyn Ctx) {
        let _ = std::fs::create_dir_all(&self.dir);
        let _ = std::fs::write(self.dir.join("brief.md"), &self.brief);
        let _ = std::fs::write(self.dir.join("pattern.toml"), self.pattern.to_toml());
        self.log("◈", "saffron", format!("run {} started with pattern {}", self.id, self.pattern.name));
        let project = self.project.clone();
        let id = self.id.clone();
        let iso = self.pattern.settings.isolation.clone();
        ctx.job(JobTag::Setup, Box::new(move || JobOut::Setup(git::setup(&project, &id, &iso))));
    }

    pub(super) fn spawn_planner(&mut self, ctx: &mut dyn Ctx) -> AgentId {
        self.spawn_planner_with(ctx, None)
    }

    /// `resume`: a saved thread/session id to re-attach to (`mantra runs resume`).
    pub(super) fn spawn_planner_with(&mut self, ctx: &mut dyn Ctx, resume: Option<String>) -> AgentId {
        let name = self.pattern.flow.planner.clone();
        let role = self.role(&name);
        let wr = self.pattern.worker_roles();
        let req = SpawnReq {
            name: "planner".into(),
            role_name: name,
            instructions: format!("{}\n{}", role.instructions, tools::PLANNER_PROTOCOL),
            cwd: self.workspace_dir(),
            tools: tools::planner_tools(&wr),
            effort: None,
            extra_writable: vec![],
            context_override: None,
            role,
        };
        let id = match resume {
            Some(t) => ctx.spawn_resumed(req, t),
            None => ctx.spawn(req),
        };
        self.planner = Some(id);
        id
    }

    pub(super) fn planning_prompt(&self) -> String {
        let s = &self.pattern.settings;
        let mut roles = String::new();
        for n in self.pattern.worker_roles() {
            let r = self.role(&n);
            roles.push_str(&format!("- {n}: {} (model {}, effort {})\n", r.description, r.model, r.effort));
        }
        format!(
            "[mantra:plan]\nUSER REQUEST:\n{}\n\nProject directory: {}\nWorker roles you can assign:\n{roles}\nConstraints: at most {} tasks per phase; up to {} run at once; tasks in one phase run in parallel on isolated copies and must use disjoint scopes; each phase ends with gate checks (shell commands) + a QA agent.\n\nExplore the project, then call mantra_submit_plan.",
            self.brief,
            self.workspace_dir().display(),
            s.max_tasks_per_phase,
            s.max_parallel
        )
    }

    pub fn approve_plan(&mut self, ctx: &mut dyn Ctx) {
        if self.stage != Stage::Review {
            return;
        }
        self.want_review = false;
        self.log("✓", "green", "plan approved");
        self.start_phase(ctx, 0);
    }

    pub fn plan_feedback(&mut self, ctx: &mut dyn Ctx, text: &str) {
        let Some(p) = self.planner else { return };
        self.want_review = false;
        self.stage = Stage::Planning;
        self.log("✎", "saffron", format!("plan feedback: {}", trunc(text, 80)));
        self.mark_edge(p);
        ctx.prompt(p, format!("[mantra:revise] The user reviewed your plan and says:\n{text}\n\nSubmit the updated plan with mantra_submit_plan."));
    }

    pub(super) fn start_phase(&mut self, ctx: &mut dyn Ctx, idx: usize) {
        let Some(plan) = self.plan.clone() else { return };
        let Some(phase) = plan.phases.get(idx).cloned() else {
            self.start_finale(ctx, 0);
            return;
        };
        self.stage = Stage::Phase { idx, step: PhaseStep::Orchestrating };
        self.phase_started = Instant::now();
        self.workers.clear();
        self.checks.clear();
        self.conflicts.clear();
        self.gate_report = None;
        self.handoff_note_done = false;
        self.cleanup_done = false;
        self.log("◆", "saffron", format!("phase {}/{} — {}", idx + 1, plan.phases.len(), phase.name));

        let fresh = self.pattern.settings.orchestrator_context != "compact";
        let orch = match self.orchestrator {
            Some(o) if !fresh => {
                ctx.compact(o);
                o
            }
            _ => self.spawn_orchestrator(ctx),
        };
        let handoff = if self.handoff.is_empty() { String::new() } else { format!("Handoff from the previous phase's orchestrator:\n{}\n\n", self.handoff) };
        let phase_json = serde_json::to_string_pretty(&phase).unwrap_or_default();
        self.mark_edge(orch);
        ctx.prompt(
            orch,
            format!(
                "[mantra:phase] Phase {}/{}: {}\nGoal: {}\nOverall plan: {}\n\n{handoff}Current phase:\n```json\n{phase_json}\n```\nSpawn the tasks with mantra_spawn (sharpen prompts if useful), then call mantra_wait.",
                idx + 1,
                plan.phases.len(),
                phase.name,
                phase.goal,
                plan.summary
            ),
        );
        self.save_state();
    }

    pub(super) fn spawn_task(&mut self, ctx: &mut dyn Ctx, task: Task, prompt: Option<String>, effort: Option<String>) -> String {
        let running = self.workers.iter().filter(|w| matches!(w.state, WState::Preparing | WState::Running | WState::Retrying(_))).count();
        let prompt = prompt.filter(|p| !p.trim().is_empty()).unwrap_or_else(|| task.prompt.clone());
        let attempt = self.workers.iter().filter(|w| w.task.id == task.id).map(|w| w.attempt).max().unwrap_or(0) + 1;
        let cap = self.max_attempts();
        if attempt > cap {
            let msg = format!("{} has used all {cap} attempts — not respawning.", task.id);
            if !self.halted() {
                self.halt(ctx, HaltReason::AttemptsExhausted, None, msg.clone());
            }
            return format!("REFUSED: {msg}");
        }
        let queued = running >= self.pattern.settings.max_parallel;
        let tid = task.id.clone();
        let adhoc = task.id.starts_with("fix-");
        // A halved context from an earlier attempt of the same task carries forward.
        let context_override = self.workers.iter().rev().find(|w| w.task.id == tid).and_then(|w| w.context_override);
        self.workers.push(Worker {
            task,
            prompt,
            effort,
            agent: None,
            attempt,
            state: if queued { WState::Queued } else { WState::Preparing },
            report: String::new(),
            wt: None,
            branch: String::new(),
            spawned: Instant::now(),
            finished: None,
            tripwires: vec![],
            adhoc,
            paused: false,
            stall_flagged: false,
            context_override,
            budget_flagged: false,
            idle_prompts: vec![],
        });
        if queued {
            self.log("…", "gray", format!("{tid} queued (max_parallel = {})", self.pattern.settings.max_parallel));
            return format!("{tid} queued — it starts when a slot frees up");
        }
        self.prepare_worker(ctx, &tid, attempt, adhoc);
        format!("{tid} starting (attempt {attempt})")
    }

    /// Total attempts per task across every retry path (auto-retry, orchestrator, user).
    fn max_attempts(&self) -> u32 {
        self.pattern.settings.worker_retries + 3
    }

    fn prepare_worker(&mut self, ctx: &mut dyn Ctx, task: &str, attempt: u32, adhoc: bool) {
        let Some(ws) = self.ws.clone() else { return };
        let run_id = self.id.clone();
        let task_s = task.to_string();
        if adhoc {
            // ad-hoc fixes during the finale work directly on the integration copy
            let dir = ws.integ.clone();
            ctx.job(JobTag::WorkerWt { task: task_s, attempt }, Box::new(move || JobOut::Wt(Ok((dir, String::new())))));
        } else {
            ctx.job(JobTag::WorkerWt { task: task_s.clone(), attempt }, Box::new(move || JobOut::Wt(git::add_worker(&ws, &run_id, &task_s, attempt))));
        }
    }

    pub(super) fn launch_worker(&mut self, ctx: &mut dyn Ctx, wi: usize, dir: PathBuf, branch: String) {
        self.launch_worker_with(ctx, wi, dir, branch, None)
    }

    /// `resume`: re-attach to the worker's saved thread/session instead of starting fresh — its
    /// worktree still holds its edits, so the prompt tells it to continue rather than restart.
    pub(super) fn launch_worker_with(&mut self, ctx: &mut dyn Ctx, wi: usize, dir: PathBuf, branch: String, resume: Option<String>) {
        let plan_summary = self.plan.as_ref().map(|p| p.summary.clone()).unwrap_or_default();
        let w = &self.workers[wi];
        let role = self.role(&w.task.role);
        let scope = if w.task.scope.is_empty() { "(not restricted)".to_string() } else { w.task.scope.join(", ") };
        let instructions = format!(
            "{}\n{}\n## Your task: {} — {}\nScope: {scope}\nAcceptance: {}\n",
            role.instructions,
            tools::WORKER_PROTOCOL,
            w.task.id,
            w.task.title,
            if w.task.acceptance.is_empty() { "the task is fully done and verified" } else { &w.task.acceptance }
        );
        let prompt = match &resume {
            Some(_) => format!("[mantra:resume] Mantra restarted while you were working on this task; your worktree still holds your changes. Continue exactly where you left off — do not start over.\n\nThe task again:\n{}\n\n(Context — overall project goal: {plan_summary})", w.prompt),
            None => format!("{}\n\n(Context — overall project goal: {plan_summary})", w.prompt),
        };
        let effort = w.effort.clone().or(w.task.effort.clone());
        let name = w.task.id.clone();
        let rname = w.task.role.clone();
        let glyph = role.glyph.clone();
        let context_override = w.context_override;
        let resumed = resume.is_some();
        let req = SpawnReq { name, role_name: rname.clone(), role, cwd: dir.clone(), instructions, tools: vec![], effort, extra_writable: vec![], context_override };
        let id = match resume {
            Some(t) => ctx.spawn_resumed(req, t),
            None => ctx.spawn(req),
        };
        let w = &mut self.workers[wi];
        w.agent = Some(id);
        w.wt = Some(dir);
        w.branch = branch;
        w.state = WState::Running;
        w.spawned = Instant::now();
        let tid = w.task.id.clone();
        self.mark_edge(id);
        ctx.prompt(id, prompt);
        self.log(&glyph, "violet", format!("{} {tid} ({rname})", if resumed { "re-attached" } else { "spawned" }));
        self.save_state();
    }

    pub(super) fn fill_slots(&mut self, ctx: &mut dyn Ctx) {
        if self.halted() {
            return;
        }
        let running = self.workers.iter().filter(|w| matches!(w.state, WState::Preparing | WState::Running | WState::Retrying(_))).count();
        let free = self.pattern.settings.max_parallel.saturating_sub(running);
        let queued: Vec<(String, u32, bool)> = self.workers.iter().filter(|w| w.state == WState::Queued).take(free).map(|w| (w.task.id.clone(), w.attempt, w.adhoc)).collect();
        for (t, a, adhoc) in queued {
            if let Some(w) = self.workers.iter_mut().find(|w| w.task.id == t && w.attempt == a) {
                w.state = WState::Preparing;
            }
            self.prepare_worker(ctx, &t, a, adhoc);
        }
    }

    pub(super) fn check_phase_done(&mut self, ctx: &mut dyn Ctx) {
        let Some(idx) = self.phase_idx() else { return };
        if !matches!(self.stage, Stage::Phase { step: PhaseStep::Orchestrating, .. }) || self.halted() {
            return;
        }
        let Some(phase) = self.current_phase().cloned() else { return };
        let all_done = phase.tasks.iter().all(|t| self.workers.iter().rev().find(|w| w.task.id == t.id).map(|w| w.state == WState::Done).unwrap_or(false));
        if !all_done {
            return;
        }
        self.stage = Stage::Phase { idx, step: PhaseStep::Merging };
        self.log("⇲", "green", format!("all {} tasks done — merging for the gate", phase.tasks.len()));
        let Some(ws) = self.ws.clone() else { return };
        let list: Vec<(String, PathBuf, String)> = self
            .workers
            .iter()
            .filter(|w| w.state == WState::Done && !w.adhoc)
            .filter_map(|w| w.wt.clone().map(|d| (w.task.id.clone(), d, w.branch.clone())))
            .collect();
        ctx.job(JobTag::Merge { phase: idx }, Box::new(move || JobOut::Merge(git::merge_workers(&ws, &list))));
        self.save_state();
    }

    fn run_checks(&mut self, ctx: &mut dyn Ctx, idx: usize, round: u32) {
        let checks = self.current_phase().map(|p| p.gate.checks.clone()).unwrap_or_default();
        if checks.is_empty() {
            if round == 0 {
                self.spawn_gate(ctx, idx);
            } else {
                self.begin_handoff(ctx, idx);
            }
            return;
        }
        self.stage = Stage::Phase { idx, step: PhaseStep::Checks { round } };
        self.log("⎔", "blue", format!("running {} gate check(s){}", checks.len(), if round > 0 { " (verify)" } else { "" }));
        let dir = self.workspace_dir();
        let timeout = Duration::from_secs(self.pattern.settings.check_timeout_secs);
        ctx.job(JobTag::Checks { phase: idx, round }, Box::new(move || JobOut::Checks(git::run_checks(&dir, &checks, timeout))));
    }

    fn check_summary(&self) -> String {
        if self.checks.is_empty() {
            return "(no gate checks defined)".into();
        }
        self.checks
            .iter()
            .map(|c| {
                let mut o = c.output.clone();
                crate::util::tail_bytes(&mut o, 1500);
                format!("$ {} → {}\n{}", c.cmd, if c.ok { "PASS".to_string() } else { format!("FAIL (exit {:?})", c.code) }, if c.ok { String::new() } else { o })
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn spawn_gate(&mut self, ctx: &mut dyn Ctx, idx: usize) {
        self.spawn_gate_at_round(ctx, idx, 1);
    }

    /// Same as `spawn_gate` but for a caller-supplied round (WP7.4 `respawn_gate`), so respawning
    /// a stuck gate agent does not silently reset the round counter — `gate_max_rounds` and the L4
    /// identical-blocker halt are both keyed off `round`.
    fn spawn_gate_at_round(&mut self, ctx: &mut dyn Ctx, idx: usize, round: u32) {
        let Some(phase) = self.current_phase().cloned() else { return };
        self.stage = Stage::Phase { idx, step: PhaseStep::Gate { round } };
        self.gate_report = None;
        self.last_gate_blocker = None;
        let name = self.pattern.flow.phase_gate.clone();
        let role = self.role(&name);
        let reports: String = self.workers.iter().filter(|w| w.state == WState::Done).map(|w| format!("### {} — {}\n{}\n", w.task.id, w.task.title, trunc(&w.report, 1500))).collect();
        let conflicts = if self.conflicts.is_empty() {
            String::new()
        } else {
            format!("\nThese branches could NOT be merged automatically (conflicts). Merge each yourself with `git merge <branch>`, resolve, and commit:\n{}\n", self.conflicts.join("\n"))
        };
        let prompt = format!(
            "[mantra:gate] Phase: {}\nGoal: {}\nGate criteria: {}\nQA focus: {}\n\nWorker reports:\n{reports}{conflicts}\nGate checks:\n{}\n\nMake the result coherent and green, then call mantra_gate_report.",
            phase.name,
            phase.goal,
            if phase.gate.criteria.is_empty() { "build + tests pass, code coherent" } else { &phase.gate.criteria },
            if phase.gate.focus.is_empty() { "integration and consistency between the parallel changes" } else { &phase.gate.focus },
            self.check_summary()
        );
        let id = ctx.spawn(SpawnReq {
            name: name.clone(),
            role_name: name.clone(),
            instructions: format!("{}\n{}", role.instructions, tools::GATE_PROTOCOL),
            cwd: self.workspace_dir(),
            tools: tools::gate_tools(false, &self.pattern.worker_roles()),
            effort: None,
            extra_writable: self.integ_writable(),
            context_override: None,
            role: role.clone(),
        });
        self.gate_agent = Some(id);
        self.mark_edge(id);
        ctx.prompt(id, prompt);
        self.log(&role.glyph, "green", format!("{name} gate started"));
    }

    fn begin_handoff(&mut self, ctx: &mut dyn Ctx, idx: usize) {
        self.stage = Stage::Phase { idx, step: PhaseStep::Handoff };
        self.log("✓", "green", "gate passed — archiving workers, writing handoff");
        // archive workers + gate agent
        let agents: Vec<AgentId> = self.workers.iter().filter_map(|w| w.agent).chain(self.gate_agent).collect();
        for a in agents {
            self.tokens_prev += ctx.agent(a).map(|x| x.tokens_total).unwrap_or(0);
            ctx.stop(a, true);
        }
        self.gate_agent = None;
        let phase_name = self.current_phase().map(|p| p.name.clone()).unwrap_or_default();
        self.history.push(PhaseRecord {
            name: phase_name.clone(),
            workers: self.workers.iter().filter(|w| w.state == WState::Done).map(|w| (w.task.id.clone(), w.task.title.clone(), self.role(&w.task.role).glyph, true)).collect(),
            duration: self.phase_started.elapsed(),
            ended: Instant::now(),
        });
        // write worker outputs
        let pdir = self.dir.join(format!("phase-{}", idx + 1));
        let _ = std::fs::create_dir_all(&pdir);
        for w in &self.workers {
            let _ = std::fs::write(pdir.join(format!("{}.md", w.task.id)), format!("# {} — {}\nstate: {:?}\nattempt: {}\n\n{}\n", w.task.id, w.task.title, w.state, w.attempt, w.report));
        }
        let ws = self.ws.clone();
        let wts: Vec<(PathBuf, String)> = self.workers.iter().filter(|w| !w.adhoc).filter_map(|w| w.wt.clone().map(|d| (d, w.branch.clone()))).collect();
        let msg = format!("mantra: phase {} — {}", idx + 1, phase_name);
        ctx.job(
            JobTag::Cleanup { phase: idx },
            Box::new(move || {
                let Some(ws) = ws else { return JobOut::Text(Ok(String::new())) };
                let committed = git::phase_commit(&ws, &msg);
                for (d, b) in wts {
                    git::remove_worker(&ws, &d, &b, false);
                }
                JobOut::Text(committed.map(|c| if c { "committed".into() } else { "nothing to commit".into() }))
            }),
        );
        match self.orchestrator {
            Some(o) => {
                self.mark_edge(o);
                ctx.prompt(o, "[mantra:handoff] The phase passed its gate. Write the handoff note for the next phase (≤10 lines). Don't call tools.".into());
            }
            None => self.handoff_note_done = true,
        }
        self.save_state();
    }

    pub(super) fn maybe_next_phase(&mut self, ctx: &mut dyn Ctx) {
        let Some(idx) = self.phase_idx() else { return };
        if !matches!(self.stage, Stage::Phase { step: PhaseStep::Handoff, .. }) || !self.handoff_note_done || !self.cleanup_done {
            return;
        }
        if self.pattern.settings.orchestrator_context != "compact" {
            if let Some(o) = self.orchestrator.take() {
                self.tokens_prev += ctx.agent(o).map(|x| x.tokens_total).unwrap_or(0);
                ctx.stop(o, true);
            }
        }
        let n = self.plan.as_ref().map(|p| p.phases.len()).unwrap_or(0);
        if idx + 1 < n {
            self.start_phase(ctx, idx + 1);
        } else {
            if let Some(o) = self.orchestrator.take() {
                ctx.stop(o, true);
            }
            self.start_finale(ctx, 0);
        }
    }

    pub(super) fn start_finale(&mut self, ctx: &mut dyn Ctx, k: usize) {
        self.workers.retain(|w| w.adhoc && !matches!(w.state, WState::Done | WState::Cancelled));
        let steps = self.pattern.flow.finale.clone();
        let Some(step) = steps.get(k).cloned() else {
            self.finish(ctx);
            return;
        };
        self.stage = Stage::Finale { idx: k };
        self.gate_report = None;
        self.phase_started = Instant::now();
        let role = self.role(&step.role);
        let plan = self.plan.clone().unwrap_or_default();
        let final_checks = if plan.final_checks.is_empty() { "(none defined)".to_string() } else { plan.final_checks.join("\n") };
        let prompt = format!(
            "[mantra:finale] Step {}/{}: {}\n\nOriginal request:\n{}\n\nPlan summary: {}\nFinal checks:\n{final_checks}\n\nWork in: {}\n{}When done, call mantra_gate_report.",
            k + 1,
            steps.len(),
            step.task,
            self.brief,
            plan.summary,
            self.workspace_dir().display(),
            if step.may_spawn { "You may spawn ad-hoc workers with mantra_spawn_adhoc, then mantra_wait until they finish.\n" } else { "" }
        );
        // The planner verifies with its full context of the plan; other roles get a fresh agent.
        let agent = if step.role == self.pattern.flow.planner && self.planner.is_some() {
            self.planner.unwrap_or_default()
        } else {
            ctx.spawn(SpawnReq {
                name: step.role.clone(),
                role_name: step.role.clone(),
                instructions: format!("{}\n{}", role.instructions, tools::GATE_PROTOCOL),
                cwd: self.workspace_dir(),
                tools: tools::gate_tools(step.may_spawn, &self.pattern.worker_roles()),
                effort: None,
                extra_writable: self.integ_writable(),
                context_override: None,
                role: role.clone(),
            })
        };
        if let Some(prev) = self.finale_agent {
            if Some(prev) != self.planner && prev != agent {
                self.tokens_prev += ctx.agent(prev).map(|x| x.tokens_total).unwrap_or(0);
                ctx.stop(prev, true);
            }
        }
        self.finale_agent = Some(agent);
        self.mark_edge(agent);
        ctx.prompt(agent, prompt);
        self.log(&role.glyph, "saffron", format!("finale {}/{} — {}", k + 1, steps.len(), step.role));
        self.save_state();
    }

    fn finish(&mut self, ctx: &mut dyn Ctx) {
        self.stage = Stage::Done;
        let ws = self.ws.clone();
        ctx.job(JobTag::Finish, Box::new(move || JobOut::Text(ws.map(|w| git::phase_commit(&w, "mantra: finale").map(|_| "ok".to_string())).unwrap_or(Ok(String::new())))));
        for a in self.all_agents() {
            ctx.stop(a, false);
        }
        let branch = self.ws.as_ref().filter(|w| w.worktree).map(|w| format!(" — branch {} is ready; /land merges it into {}", w.branch, w.base_branch)).unwrap_or_default();
        self.log("✦", "saffron", format!("run complete in {}{branch}", fmt_dur(self.started.elapsed())));
        ctx.notify(&format!("Mantra: run complete{branch}"));
        self.save_state();
    }

    pub fn land(&mut self, ctx: &mut dyn Ctx) {
        let Some(ws) = self.ws.clone() else { return };
        ctx.job(JobTag::Land, Box::new(move || JobOut::Text(git::land(&ws))));
    }

    pub(super) fn fail_run(&mut self, ctx: &mut dyn Ctx, why: String) {
        self.log("✗", "red", format!("run stopped: {why}"));
        self.alerts.push(why.clone());
        ctx.notify(&format!("Mantra: {why}"));
        self.stage = Stage::Failed(why);
        self.save_state();
    }

    fn alert(&mut self, ctx: &mut dyn Ctx, text: String) {
        self.log("⚑", "amber", text.clone());
        self.alerts.push(text.clone());
        ctx.notify(&format!("Mantra needs you: {text}"));
    }

    // ───────────────────────────── inputs ─────────────────────────────

    pub fn on_job(&mut self, ctx: &mut dyn Ctx, tag: JobTag, out: JobOut) {
        match (tag, out) {
            (JobTag::Setup, JobOut::Setup(r)) => match r {
                Ok(ws) => {
                    if let Some(n) = &ws.note {
                        self.log("ℹ", "amber", n.clone());
                    }
                    self.log("⎇", "blue", if ws.worktree { format!("isolated worktrees on branch {}", ws.branch) } else { "shared workspace".into() });
                    self.ws = Some(ws);
                    self.stage = Stage::Planning;
                    let p = self.spawn_planner(ctx);
                    self.mark_edge(p);
                    let prompt = self.planning_prompt();
                    ctx.prompt(p, prompt);
                    self.log("✦", "saffron", "planner is exploring and planning");
                }
                Err(e) => self.fail_run(ctx, format!("workspace setup failed: {e}")),
            },
            (JobTag::WorkerWt { task, attempt }, JobOut::Wt(r)) => {
                let Some(wi) = self.workers.iter().position(|w| w.task.id == task && w.attempt == attempt) else { return };
                if self.workers[wi].state == WState::Cancelled {
                    return;
                }
                match r {
                    Ok((dir, branch)) => self.launch_worker(ctx, wi, dir, branch),
                    Err(e) => match self.ws.as_ref().map(|w| w.integ.clone()) {
                        Some(integ) if integ.exists() => {
                            crate::mlog!("worktree for {task} failed ({e}); using shared integration copy");
                            self.log("⚠", "amber", format!("{task}: no worktree ({}) — running on the shared copy", crate::util::trunc(&e, 60)));
                            self.launch_worker(ctx, wi, integ, String::new());
                        }
                        _ => {
                            self.workers[wi].state = WState::Failed(e.clone());
                            self.log("✗", "red", format!("{task}: couldn't prepare a workspace: {}", crate::util::trunc(&e, 80)));
                            self.orch_event(ctx, format!("{task} FAILED to start: {e}"));
                        }
                    },
                }
            }
            (JobTag::Merge { phase }, JobOut::Merge(m)) => {
                if !m.merged.is_empty() {
                    self.log("⇲", "green", format!("merged {}", m.merged.join(", ")));
                }
                if !m.conflicts.is_empty() {
                    self.log("⚠", "amber", format!("{} merge conflict(s) → handed to the gate agent", m.conflicts.len()));
                }
                self.conflicts = m.conflicts;
                let _ = std::fs::write(self.dir.join(format!("phase-{}-merge.log", phase + 1)), m.log);
                self.run_checks(ctx, phase, 0);
            }
            (JobTag::Checks { phase, round }, JobOut::Checks(results)) => {
                for c in &results {
                    self.log(if c.ok { "✓" } else { "✗" }, if c.ok { "green" } else { "red" }, format!("check `{}` {} ({}s)", trunc(&c.cmd, 40), if c.ok { "passed" } else { "failed" }, c.secs));
                }
                let all_ok = results.iter().all(|c| c.ok);
                self.checks = results;
                if round == 0 {
                    self.spawn_gate(ctx, phase);
                } else if all_ok {
                    self.begin_handoff(ctx, phase);
                } else if round < self.pattern.settings.gate_max_rounds {
                    self.stage = Stage::Phase { idx: phase, step: PhaseStep::Gate { round: round + 1 } };
                    self.gate_report = None;
                    if let Some(g) = self.gate_agent {
                        self.mark_edge(g);
                        let s = self.check_summary();
                        ctx.prompt(g, format!("[mantra:gate-round {}] Gate checks still fail after your fixes:\n{s}\n\nFix them, then call mantra_gate_report again.", round + 1));
                    }
                } else {
                    let gate = self.gate_agent;
                    self.halt(ctx, HaltReason::GateExhausted, gate, format!("phase {} gate still failing after {} rounds", phase + 1, round));
                }
            }
            (JobTag::Cleanup { phase }, JobOut::Text(r)) => {
                if let Err(e) = r {
                    self.log("⚠", "amber", format!("phase {} commit: {e}", phase + 1));
                }
                self.cleanup_done = true;
                self.maybe_next_phase(ctx);
            }
            (JobTag::Land, JobOut::Text(r)) => match r {
                Ok(m) => self.log("⇲", "green", m),
                Err(e) => self.alert(ctx, e),
            },
            (JobTag::Finish, _) => {}
            _ => {}
        }
    }

    /// A worker that hit ContextFull while running on the *assumed* 200k window (no explicit
    /// `context_window` set for its model) gets that assumption halved for its next attempt.
    /// A model with an explicit setting is left alone — the user made that choice deliberately.
    fn shrink_assumed_context(&mut self, ctx: &mut dyn Ctx, a: AgentId) {
        let Some(wi) = self.worker_idx(a) else { return };
        if self.workers[wi].context_override.is_some() {
            return; // already shrunk once for this attempt's live process
        }
        let Some(agent) = ctx.agent(a) else { return };
        if agent.ctx_window != Some(crate::discover::ASSUMED_CONTEXT) {
            return; // an explicit context_window is in effect — leave it to the user
        }
        let halved = crate::discover::ASSUMED_CONTEXT / 2;
        let model = agent.model_alias.clone();
        self.workers[wi].context_override = Some(halved);
        self.log("⚠", "amber", format!("context window assumed 200k was too large for {model}; using {}k — set it in /models", halved / 1000));
    }

    pub fn on_turn_done(&mut self, ctx: &mut dyn Ctx, a: AgentId, status: &str, error: Option<String>, kind: Option<ErrKind>) {
        if !self.is_active() {
            return;
        }
        // Failed turns: retry blips with backoff (Codex already retried the stream itself);
        // anything else pauses the run instead of burning tokens.
        if status == "failed" {
            let msg = error.clone().unwrap_or_default();
            if matches!(kind, Some(ErrKind::Transient) | Some(ErrKind::ContextFull)) {
                let count = {
                    let n = self.retry_counts.entry(a).or_insert(0);
                    *n += 1;
                    *n
                };
                let max = self.pattern.settings.worker_retries.max(1) + 1;
                if count <= max {
                    let wait = Duration::from_secs(4 * count as u64);
                    if kind == Some(ErrKind::ContextFull) {
                        ctx.compact(a);
                        self.shrink_assumed_context(ctx, a);
                    }
                    let name = self.name_of(a);
                    if let Some(wi) = self.worker_idx(a) {
                        self.workers[wi].state = WState::Retrying(Instant::now() + wait);
                    }
                    self.log("↻", "amber", format!("{name}: {} → retry {count}/{max} in {}s", trunc(&msg, 60), wait.as_secs()));
                    self.continue_queue.push((a, Instant::now() + wait, format!("[mantra:retry] Your previous turn failed ({msg}). Continue exactly where you left off.")));
                    return;
                }
            }
            // ProviderRejected is deterministic (bad model/role for this provider) — never retried,
            // and halts immediately even for a worker (unlike other worker errors, which are
            // reported to the orchestrator below).
            let reason = match kind {
                Some(ErrKind::Auth) => Some(HaltReason::Auth),
                Some(ErrKind::UsageLimit) => Some(HaltReason::UsageLimit),
                Some(ErrKind::ProviderRejected) => Some(HaltReason::ProviderRejected),
                _ if self.worker_idx(a).is_none() => Some(HaltReason::AgentTurnFailed),
                _ => None,
            };
            if let Some(reason) = reason {
                // WP7.3: a planner/orchestrator/gate/finale turn that fails past the retry cap
                // gets one free respawn (with a resume prompt) before the run gives up on it.
                if reason == HaltReason::AgentTurnFailed {
                    if let Some(tag) = self.watchdog_role_tag(a) {
                        if self.turn_fail_retried.insert(tag) {
                            let n = self.name_of(a);
                            self.log("⏰", "amber", format!("{n}: turn failed ({}) — respawning once before halting (watchdog)", trunc(&msg, 60)));
                            let _ = self.respawn(ctx, a, Some(format!("[mantra:watchdog] Your previous turn failed ({}). Continue where you left off.", trunc(&msg, 200))));
                            self.retry_counts.remove(&a);
                            return;
                        }
                    }
                }
                let n = self.name_of(a);
                let detail = if reason == HaltReason::ProviderRejected {
                    let (alias, provider) = ctx.agent(a).map(|ag| (ag.model_alias.clone(), ag.provider.clone())).unwrap_or_default();
                    let role = self.role_name_of(a).unwrap_or_default();
                    format!("{n} [{role}] ({alias} via {provider}): {}", trunc(&msg, 120))
                } else {
                    format!("{n}: {}", trunc(&msg, 120))
                };
                if !self.halted() {
                    self.halt(ctx, reason, Some(a), detail);
                }
                self.retry_counts.remove(&a);
                return;
            }
            // worker with a non-retryable error or out of retries: falls through → reported to the orchestrator
        } else if status == "completed" {
            self.retry_counts.remove(&a);
            if let Some(tag) = self.watchdog_role_tag(a) {
                self.turn_fail_retried.remove(&tag);
            }
        }

        if Some(a) == self.planner && matches!(self.stage, Stage::Planning) {
            if self.plan.is_some() && self.plan_version > 0 && self.planner_nudges < 100 {
                if self.pattern.settings.review_plan {
                    self.stage = Stage::Review;
                    self.want_review = true;
                    self.log("☰", "saffron", "plan ready — review it (p) and approve (a) or type feedback");
                    ctx.notify("Mantra: plan ready for review");
                } else {
                    self.start_phase(ctx, 0);
                }
            } else {
                self.planner_nudges += 1;
                if self.planner_nudges > 3 {
                    self.fail_run(ctx, "planner did not submit a valid plan".into());
                } else {
                    self.mark_edge(a);
                    ctx.prompt(a, "[mantra] You haven't submitted a valid plan yet. Call mantra_submit_plan with the full plan now.".into());
                }
            }
            return;
        }

        if Some(a) == self.orchestrator {
            if let Stage::Phase { step: PhaseStep::Handoff, .. } = self.stage {
                self.handoff = ctx.agent(a).and_then(|x| x.final_message.clone()).unwrap_or_default();
                self.handoff_note_done = true;
                self.maybe_next_phase(ctx);
                return;
            }
            // Safety net: tasks the orchestrator forgot to spawn are started automatically.
            if let Some(phase) = self.current_phase().cloned() {
                if matches!(self.stage, Stage::Phase { step: PhaseStep::Orchestrating, .. }) && !self.halted() {
                    for t in phase.tasks {
                        if !self.workers.iter().any(|w| w.task.id == t.id) {
                            self.log("◉", "violet", format!("auto-spawning {} (orchestrator skipped it)", t.id));
                            self.spawn_task(ctx, t, None, None);
                        }
                    }
                }
            }
            self.wake_orch(ctx);
            return;
        }

        if Some(a) == self.gate_agent {
            if let Stage::Phase { idx, step: PhaseStep::Gate { round } } = self.stage.clone() {
                let report = self.gate_report.clone().or_else(|| parse_gate_from_text(ctx.agent(a).and_then(|x| x.final_message.clone()).as_deref()));
                match report {
                    Some((true, summary)) => {
                        self.last_gate_blocker = None;
                        self.log("◎", "green", format!("gate report: pass — {}", trunc(&summary, 70)));
                        self.run_checks(ctx, idx, round);
                    }
                    other => {
                        let why = other.map(|(_, s)| s).unwrap_or_else(|| "no gate report".into());
                        // L4 loop protection (WP12.4/WP6): two consecutive gate reports blocked on
                        // the same thing will never resolve themselves — halt now instead of
                        // spending the remaining rounds repeating the same failed fix.
                        let sig = trunc(why.trim(), 80);
                        let repeated = !sig.is_empty() && self.last_gate_blocker.as_deref() == Some(sig.as_str());
                        self.last_gate_blocker = Some(sig);
                        if repeated {
                            self.halt(ctx, HaltReason::GateExhausted, Some(a), format!("phase {} gate stuck on the same blocker twice: {}", idx + 1, trunc(&why, 80)));
                        } else if round < self.pattern.settings.gate_max_rounds {
                            self.stage = Stage::Phase { idx, step: PhaseStep::Gate { round: round + 1 } };
                            self.gate_report = None;
                            self.log("◎", "amber", format!("gate round {} not passed: {}", round, trunc(&why, 60)));
                            self.mark_edge(a);
                            ctx.prompt(a, format!("[mantra:gate-round {}] Keep going: fix what's left so the gate passes, then call mantra_gate_report.", round + 1));
                        } else {
                            self.halt(ctx, HaltReason::GateExhausted, Some(a), format!("phase {} gate not passed after {round} rounds: {}", idx + 1, trunc(&why, 80)));
                        }
                    }
                }
            }
            return;
        }

        if let Some(wi) = self.worker_idx(a) {
            self.on_worker_done(ctx, wi, status, error);
            return;
        }

        if Some(a) == self.finale_agent || (Some(a) == self.planner && matches!(self.stage, Stage::Finale { .. })) {
            if let Stage::Finale { idx } = self.stage {
                let running_adhoc = self.workers.iter().any(|w| w.adhoc && matches!(w.state, WState::Preparing | WState::Running | WState::Retrying(_) | WState::Queued));
                if running_adhoc {
                    return; // it called mantra_wait; we'll wake it when fixes finish
                }
                let rep = self.gate_report.clone().or_else(|| parse_gate_from_text(ctx.agent(a).and_then(|x| x.final_message.clone()).as_deref()));
                if let Some((pass, s)) = rep {
                    self.log(if pass { "✓" } else { "⚠" }, if pass { "green" } else { "amber" }, format!("finale step {} report: {}", idx + 1, trunc(&s, 70)));
                }
                self.start_finale(ctx, idx + 1);
            }
            return;
        }

        if Some(a) == self.planner {
            // reprompt handled — forward any queued orchestrator notes
            self.wake_orch(ctx);
        }
    }

    fn on_worker_done(&mut self, ctx: &mut dyn Ctx, wi: usize, status: &str, error: Option<String>) {
        let w = &mut self.workers[wi];
        if w.paused || w.state == WState::Cancelled {
            return;
        }
        let tid = w.task.id.clone();
        let report = w.agent.and_then(|a| ctx.agent(a)).and_then(|a| a.final_message.clone()).unwrap_or_default();
        match status {
            "completed" => {
                let blocked = report.lines().any(|l| {
                    let l = l.trim().to_lowercase();
                    l.starts_with("status:") && l.contains("blocked")
                });
                let w = &mut self.workers[wi];
                w.report = report.clone();
                w.finished = Some(Instant::now());
                if blocked {
                    w.state = WState::Failed("blocked".into());
                    self.log("⚠", "amber", format!("{tid} is blocked"));
                    self.orch_event(ctx, format!("{tid} FAILED (blocked). Its report:\n{}\nUse mantra_retry with a better prompt, or mantra_prompt to unblock it.", trunc(&report, 1200)));
                } else {
                    w.state = WState::Done;
                    let el = fmt_dur(w.spawned.elapsed());
                    let adhoc = w.adhoc;
                    let summary = report.lines().find(|l| l.trim_start().to_lowercase().starts_with("summary:")).map(|l| l.trim().to_string()).unwrap_or_else(|| trunc(report.lines().last().unwrap_or(""), 90));
                    self.log("✓", "green", format!("{tid} done in {el}"));
                    if adhoc {
                        // finale fixes report straight to the finale agent
                        let fa = if let Stage::Finale { .. } = self.stage { self.finale_agent } else { None };
                        let still = self.workers.iter().any(|w| w.adhoc && matches!(w.state, WState::Preparing | WState::Running | WState::Retrying(_) | WState::Queued));
                        if let (Some(f), false) = (fa, still) {
                            let reps: String = self.workers.iter().filter(|w| w.adhoc && w.state == WState::Done).map(|w| format!("- {}: {}\n", w.task.id, trunc(&w.report, 600))).collect();
                            self.mark_edge(f);
                            ctx.prompt(f, format!("[mantra:event] Your ad-hoc workers finished:\n{reps}\nVerify, then call mantra_gate_report (or spawn more fixes)."));
                        }
                    } else {
                        self.orch_event(ctx, format!("✓ {tid} finished in {el}. {summary}"));
                    }
                }
            }
            "interrupted" => {
                self.log("■", "amber", format!("{tid} interrupted"));
                self.orch_event(ctx, format!("{tid} was interrupted (it is idle now). mantra_prompt it to continue or mantra_retry."));
            }
            _ => {
                let e = error.unwrap_or_else(|| "turn failed".into());
                self.workers[wi].state = WState::Failed(e.clone());
                self.log("✗", "red", format!("{tid} failed: {}", trunc(&e, 70)));
                self.orch_event(ctx, format!("{tid} FAILED: {e}\nUse mantra_retry (optionally with a better prompt)."));
            }
        }
        self.fill_slots(ctx);
        self.check_phase_done(ctx);
        self.save_state();
    }

    pub fn on_files_changed(&mut self, ctx: &mut dyn Ctx, a: AgentId, paths: &[String]) {
        let Some(wi) = self.worker_idx(a) else { return };
        let w = &mut self.workers[wi];
        if w.task.scope.is_empty() {
            return;
        }
        let outside: Vec<String> = paths.iter().filter(|p| !crate::util::in_scope(&w.task.scope, p)).filter(|p| !w.tripwires.contains(p)).cloned().collect();
        if outside.is_empty() {
            return;
        }
        w.tripwires.extend(outside.clone());
        let tid = w.task.id.clone();
        let scope = w.task.scope.join(", ");
        self.log("⚠", "amber", format!("tripwire: {tid} edited {} (scope: {scope})", outside.join(", ")));
        self.orch_event(ctx, format!("TRIPWIRE: {tid} edited files outside its scope ({scope}): {}. Decide whether that's OK; steer it with mantra_prompt if not.", outside.join(", ")));
    }

    /// WP12.4/L1: a command an agent ran failed in a way that names `bwrap`/user namespaces — the
    /// sandbox itself cannot start, so no agent can run any command. Never spend gate rounds or
    /// retries on this: halt immediately, on the first occurrence, with the sandbox fix hint.
    pub fn on_environment_broken(&mut self, ctx: &mut dyn Ctx, a: AgentId, msg: String) {
        if self.halted() {
            return;
        }
        let who = self.name_of(a);
        self.halt(ctx, HaltReason::Environment, Some(a), format!("{who}: a command failed — the sandbox can't run commands here ({}). {}", trunc(&msg, 200), crate::util::sandbox_fix_hint()));
    }

    pub fn on_crash(&mut self, ctx: &mut dyn Ctx, a: AgentId, reason: &str, restarting: bool) {
        let n = self.name_of(a);
        if restarting {
            self.log("↻", "amber", format!("{n}: process crashed ({}) — restarting & resuming", trunc(reason, 50)));
        } else {
            self.alert(ctx, format!("{n} keeps crashing: {} (select it and press r to restart)", trunc(reason, 80)));
        }
    }

    /// Called when an agent's thread is ready. After a crash+resume, continue its unfinished turn.
    pub fn on_ready(&mut self, ctx: &mut dyn Ctx, a: AgentId, resumed: bool, was_busy: bool) {
        if let Some(ag) = ctx.agent(a) {
            self.agent_meta.insert(
                a,
                AgentState { slot: String::new(), name: ag.name.clone(), role: ag.role.clone(), provider: ag.provider.clone(), model_alias: ag.model_alias.clone(), effort: ag.effort.clone(), thread_id: ag.thread_id.clone(), cwd: ag.cwd.clone() },
            );
            self.save_state();
        }
        if resumed && was_busy && self.is_active() {
            self.mark_edge(a);
            ctx.prompt(a, "[mantra:resume] Your process restarted mid-task. Continue exactly where you left off.".into());
        }
        self.wake_orch(ctx);
    }

    pub fn tick(&mut self, ctx: &mut dyn Ctx) {
        if self.last_tick.elapsed() < Duration::from_millis(900) {
            return;
        }
        self.last_tick = Instant::now();
        if !self.is_active() || self.halted() {
            return;
        }
        let now = Instant::now();
        // continue after backoff
        let due: Vec<(AgentId, String)> = self.continue_queue.iter().filter(|(_, t, _)| *t <= now).map(|(a, _, m)| (*a, m.clone())).collect();
        self.continue_queue.retain(|(_, t, _)| *t > now);
        for (a, m) in due {
            if let Some(wi) = self.worker_idx(a) {
                self.workers[wi].state = WState::Running;
            }
            self.mark_edge(a);
            ctx.prompt(a, m);
        }
        // stall + budget tripwires
        let stall = Duration::from_secs(self.pattern.settings.stall_minutes * 60);
        let mut events = vec![];
        for w in &mut self.workers {
            let Some(a) = w.agent.and_then(|a| ctx.agent(a)) else { continue };
            if w.state == WState::Running && a.busy() && a.last_event.elapsed() > stall && !w.stall_flagged {
                w.stall_flagged = true;
                events.push(format!("TRIPWIRE: {} has shown no activity for {} minutes (last: {}).", w.task.id, stall.as_secs() / 60, a.activity));
            }
            if a.last_event.elapsed() < stall {
                w.stall_flagged = false;
            }
            let limit = self.pattern.role(&w.task.role).and_then(|r| r.max_tokens);
            if let Some(l) = limit {
                if a.tokens_total > l && !w.budget_flagged {
                    w.budget_flagged = true;
                    events.push(format!("TRIPWIRE: {} used {} tokens (role budget {}).", w.task.id, fmt_tokens(a.tokens_total), fmt_tokens(l)));
                }
            }
        }
        for e in events {
            self.log("⚠", "amber", e.clone());
            self.orch_event(ctx, e);
        }
        self.watchdog_tick(ctx);
        self.wake_orch(ctx);
    }

    /// WP7.2: make sure every agent that should be working is working. Runs on the same 900ms
    /// cadence as the rest of `tick`. An agent's ladder resets to the bottom the moment it produces
    /// any new event (`Agent::last_event` moves past what we last saw), so a genuinely busy agent
    /// is never escalated on.
    fn watchdog_tick(&mut self, ctx: &mut dyn Ctx) {
        if self.halted() {
            return;
        }
        if let Some((set_at, deadline)) = self.planner_watchdog {
            if Instant::now() >= deadline {
                self.planner_watchdog = None;
                let responded = self.planner.and_then(|p| ctx.agent(p)).map(|a| a.last_event > set_at || a.busy()).unwrap_or(true);
                if !responded && !self.halted() {
                    let p = self.planner;
                    self.halt(ctx, HaltReason::AgentTurnFailed, p, "the planner did not act on a watchdog escalation — respawn it (r)".into());
                    return;
                }
            }
        }
        let ws_secs = self.pattern.settings.watchdog_seconds.max(1);
        let es_secs = self.pattern.settings.watchdog_escalate_seconds.max(ws_secs + 1);
        let mut seen = vec![];
        let mut actions: Vec<(AgentId, Expect, u8, Duration)> = vec![];
        for (a, expect) in self.expected_active() {
            seen.push(a);
            let Some(agent) = ctx.agent(a) else { continue };
            if agent.busy() {
                self.watch.remove(&a);
                continue;
            }
            let last_event = agent.last_event;
            let idle = last_event.elapsed();
            let entry = self.watch.entry(a).or_insert(WatchState { stage: 0, last_seen: last_event });
            if last_event > entry.last_seen {
                entry.last_seen = last_event;
                entry.stage = 0;
            }
            let secs = idle.as_secs();
            let target = if secs >= es_secs.saturating_mul(2) {
                3
            } else if secs >= es_secs {
                2
            } else if secs >= ws_secs {
                1
            } else {
                0
            };
            if target > entry.stage {
                entry.stage = target;
                actions.push((a, expect, target, idle));
            }
        }
        self.watch.retain(|a, _| seen.contains(a));
        for (a, expect, stage, idle) in actions {
            self.watchdog_act(ctx, a, expect, stage, idle);
        }
    }

    fn watchdog_act(&mut self, ctx: &mut dyn Ctx, a: AgentId, expect: Expect, stage: u8, idle: Duration) {
        let name = self.name_of(a);
        let secs = idle.as_secs();
        match stage {
            1 => {
                let what = match &expect {
                    Expect::Working(tid) => format!("working on {tid}"),
                    Expect::Orchestrating => "supervising the current phase".into(),
                    Expect::Planning => "planning".into(),
                    Expect::Gating => "clearing the phase gate".into(),
                    Expect::Finale(_) => "finishing the finale step".into(),
                };
                self.log("⏰", "amber", format!("{name} idle {secs}s — nudging (watchdog)"));
                self.mark_edge(a);
                ctx.prompt(a, format!("[mantra:watchdog] You are expected to be {what} but have been idle for {secs}s. Continue, or call mantra_wait/mantra_log to say why you are waiting."));
            }
            2 => {
                if Some(a) == self.orchestrator {
                    self.log("⏰", "amber", format!("orchestrator idle {secs}s — respawning (watchdog)"));
                    let _ = self.respawn(ctx, a, Some(format!("[mantra:watchdog] You were idle for {secs}s and were respawned. Pick up the phase.")));
                } else if Some(a) == self.planner {
                    // No orchestrator exists yet (the planner itself is the idle agent, i.e.
                    // `Stage::Planning`) — pushing into `orch_event`/`orch_inbox` here would just
                    // sit undelivered until the Phase-1 orchestrator spawns. Stage 1 already
                    // nudged the planner directly; stage 3 escalates straight to a halt if it is
                    // still unresponsive.
                    self.log("⏰", "amber", format!("planner idle {secs}s — awaiting further escalation (watchdog)"));
                } else {
                    self.log("⏰", "amber", format!("{name} idle {secs}s — waking the orchestrator (watchdog)"));
                    self.orch_event(ctx, format!("[mantra:watchdog] {name} has been idle for {secs}s. Decide what (if anything) to do."));
                }
            }
            3 => {
                if Some(a) == self.planner {
                    let p = self.planner;
                    self.halt(ctx, HaltReason::AgentTurnFailed, p, format!("the planner has been unresponsive for {secs}s (watchdog)"));
                    return;
                }
                let Some(p) = self.planner else { return };
                self.log("⏰", "amber", format!("{name} still idle {secs}s — waking the planner (watchdog)"));
                self.mark_edge(p);
                ctx.prompt(p, format!("[mantra:watchdog] The orchestrator did not act on {name} (idle {secs}s). Decide: mantra_brief_orchestrator, mantra_revise_plan, mantra_spawn_adhoc, or mantra_pause_agents."));
                self.planner_watchdog = Some((Instant::now(), Instant::now() + Duration::from_secs(self.pattern.settings.watchdog_escalate_seconds.max(1))));
            }
            _ => {}
        }
    }

    /// `space`: the user's own pause/resume, distinct from a failure halt only by `HaltReason`
    /// and by staying quiet (no alert, no desktop notification) — resuming works the same for
    /// every reason, via `resume()`.
    pub fn toggle_pause(&mut self, ctx: &mut dyn Ctx) {
        if self.halted() {
            self.resume(ctx);
            return;
        }
        for a in self.all_agents() {
            if ctx.agent(a).map(|x| x.busy()).unwrap_or(false) {
                ctx.interrupt(a);
                if !self.paused_agents.contains(&a) {
                    self.paused_agents.push(a);
                }
            }
        }
        self.log("‖", "amber", "run paused");
        self.halt = Some(Halt { reason: HaltReason::User, agent: None, message: "paused by you".into(), since: Instant::now() });
    }

    /// User typed into the Mandala prompt (not addressed to a specific agent).
    pub fn user_input(&mut self, ctx: &mut dyn Ctx, text: &str) {
        match self.stage.clone() {
            Stage::Setup => self.brief.push_str(&format!("\n{text}")),
            Stage::Planning => {
                if let Some(p) = self.planner {
                    self.mark_edge(p);
                    ctx.prompt(p, format!("[user] {text}"));
                }
            }
            Stage::Review => self.plan_feedback(ctx, text),
            Stage::Phase { .. } | Stage::Finale { .. } => {
                let p = match self.planner {
                    Some(p) => p,
                    None => self.spawn_planner(ctx),
                };
                let orch_last = self.orchestrator.and_then(|o| ctx.agent(o)).map(|o| o.log_text(12)).unwrap_or_default();
                let status = self.status_text(ctx);
                let phase = self.phase_idx().map(|i| format!("phase {}", i + 1)).unwrap_or_else(|| "finale".into());
                self.log("✦", "saffron", format!("re-prompt → planner: {}", trunc(text, 70)));
                self.mark_edge(p);
                ctx.prompt(
                    p,
                    format!("[mantra:reprompt] The user says:\n{text}\n\nWe are in {phase}.\nWorkers:\n{status}\nOrchestrator's recent log:\n{}\nDecide: pause agents if needed, revise the plan if needed (mantra_revise_plan), brief the orchestrator (mantra_brief_orchestrator). Then summarize.", trunc(&orch_last, 3000)),
                );
            }
            Stage::Done | Stage::Failed(_) => self.log("ℹ", "gray", "this run is finished — start a new one with /run <goal>"),
        }
    }

    /// `@agent message` from the user. `mode` is `Queue` for a plain Enter (waits behind a busy
    /// agent's current turn) or `Force` for ctrl+f (delivered right away).
    pub fn direct(&mut self, ctx: &mut dyn Ctx, name: &str, text: &str, mode: Send) -> bool {
        match self.resolve(name) {
            Some(a) => {
                self.mark_edge(a);
                ctx.prompt_mode(a, format!("[from the user] {text}"), mode);
                self.log("›", "rose", format!("you → {name}: {}", trunc(text, 60)));
                true
            }
            None => false,
        }
    }

    /// Spawn a fresh orchestrator agent for the current phase (extracted out of `start_phase` so
    /// `respawn` can reuse it, WP7.4). Does not touch `self.stage` or prompt the new agent.
    pub(super) fn spawn_orchestrator(&mut self, ctx: &mut dyn Ctx) -> AgentId {
        let plan = self.plan.clone().unwrap_or_default();
        let name = self.pattern.flow.orchestrator.clone();
        let role = self.role(&name);
        let id = ctx.spawn(SpawnReq {
            name: "orchestrator".into(),
            role_name: name,
            instructions: format!("{}\n{}\n## Project-specific brief from the planner\n{}", role.instructions, tools::ORCHESTRATOR_PROTOCOL, plan.orchestrator_brief),
            cwd: self.workspace_dir(),
            tools: tools::orchestrator_tools(),
            effort: None,
            extra_writable: vec![],
            context_override: None,
            role,
        });
        self.orchestrator = Some(id);
        id
    }

    /// Respawn any run agent in place (WP7.4): the planner, the orchestrator, the phase gate, the
    /// finale agent, or a worker (delegates to `retry_worker`). `note` is an extra line prepended
    /// to the resume prompt (used by the watchdog and by the `r` / `ctrl+r` / `/respawn` UI paths).
    pub fn respawn(&mut self, ctx: &mut dyn Ctx, a: AgentId, note: Option<String>) -> Result<(), String> {
        if self.worker_idx(a).is_some() {
            let msg = self.retry_worker(ctx, a, note);
            return if msg.starts_with("REFUSED") { Err(msg) } else { Ok(()) };
        }
        // The built-in default pattern's last finale step reuses `self.planner` as the finale
        // agent id (`start_finale`), so this ambiguous case must be checked before the plain
        // planner check below — mirrors the disambiguation `on_turn_done` already does.
        if Some(a) == self.finale_agent || (Some(a) == self.planner && matches!(self.stage, Stage::Finale { .. })) {
            return if self.respawn_finale(ctx, note) { Ok(()) } else { Err("that agent's stage has already moved on".into()) };
        }
        if Some(a) == self.planner {
            self.respawn_planner(ctx, note);
            return Ok(());
        }
        if Some(a) == self.orchestrator {
            self.respawn_orchestrator(ctx, note);
            return Ok(());
        }
        if Some(a) == self.gate_agent {
            return if self.respawn_gate(ctx, note) { Ok(()) } else { Err("that agent's stage has already moved on".into()) };
        }
        Err("that agent is no longer part of the run".into())
    }

    fn respawn_planner(&mut self, ctx: &mut dyn Ctx, note: Option<String>) {
        if let Some(old) = self.planner.take() {
            self.tokens_prev += ctx.agent(old).map(|x| x.tokens_total).unwrap_or(0);
            ctx.stop(old, true);
        }
        let id = self.spawn_planner(ctx);
        self.mark_edge(id);
        let stage_txt = format!("{:?}", self.stage);
        let resume = match &self.plan {
            Some(p) => format!("[mantra:respawn] A plan v{} exists (attached). Continue from stage {stage_txt}.\n\n{}", self.plan_version, serde_json::to_string_pretty(p).unwrap_or_default()),
            None => format!("[mantra:respawn] Continue from stage {stage_txt}.\n\n{}", self.planning_prompt()),
        };
        ctx.prompt(id, join_note(note, resume));
        self.log("↻", "violet", "planner respawned (watchdog/manual)");
    }

    fn respawn_orchestrator(&mut self, ctx: &mut dyn Ctx, note: Option<String>) {
        if let Some(old) = self.orchestrator.take() {
            self.tokens_prev += ctx.agent(old).map(|x| x.tokens_total).unwrap_or(0);
            ctx.stop(old, true);
        }
        let id = self.spawn_orchestrator(ctx);
        self.mark_edge(id);
        let phase_json = self.current_phase().map(|p| serde_json::to_string_pretty(p).unwrap_or_default()).unwrap_or_default();
        let status = self.status_text(ctx);
        let (idx, total) = (self.phase_idx().unwrap_or(0), self.plan.as_ref().map(|p| p.phases.len()).unwrap_or(0));
        let resume = format!(
            "[mantra:respawn] Phase {}/{}: resuming after a respawn.\nCurrent phase:\n```json\n{phase_json}\n```\nCurrent worker states:\n{status}\nSpawn any tasks that still need spawning, steer running ones if needed, then call mantra_wait.",
            idx + 1,
            total
        );
        ctx.prompt(id, join_note(note, resume));
        self.log("↻", "violet", "orchestrator respawned (watchdog/manual)");
    }

    fn respawn_gate(&mut self, ctx: &mut dyn Ctx, note: Option<String>) -> bool {
        let Some(idx) = self.phase_idx() else { return false };
        let round = match self.stage {
            Stage::Phase { step: PhaseStep::Gate { round }, .. } => round,
            _ => 1,
        };
        if let Some(old) = self.gate_agent.take() {
            self.tokens_prev += ctx.agent(old).map(|x| x.tokens_total).unwrap_or(0);
            ctx.stop(old, true);
        }
        self.spawn_gate_at_round(ctx, idx, round);
        if let (Some(n), Some(g)) = (note, self.gate_agent) {
            ctx.prompt(g, format!("[mantra:respawn] {n}"));
        }
        self.log("↻", "violet", "gate respawned (watchdog/manual)");
        true
    }

    fn respawn_finale(&mut self, ctx: &mut dyn Ctx, note: Option<String>) -> bool {
        let Stage::Finale { idx } = self.stage else { return false };
        if let Some(old) = self.finale_agent.take() {
            if Some(old) != self.planner {
                self.tokens_prev += ctx.agent(old).map(|x| x.tokens_total).unwrap_or(0);
                ctx.stop(old, true);
            }
        }
        self.start_finale(ctx, idx);
        if let (Some(n), Some(a)) = (note, self.finale_agent) {
            ctx.prompt(a, format!("[mantra:respawn] {n}"));
        }
        self.log("↻", "violet", "finale agent respawned (watchdog/manual)");
        true
    }

    pub fn retry_worker(&mut self, ctx: &mut dyn Ctx, a: AgentId, prompt: Option<String>) -> String {
        let Some(wi) = self.worker_idx(a) else { return "not a worker".into() };
        let task = self.workers[wi].task.clone();
        if let Some(old) = self.workers[wi].agent {
            self.tokens_prev += ctx.agent(old).map(|x| x.tokens_total).unwrap_or(0);
            ctx.stop(old, true);
        }
        self.workers[wi].state = WState::Cancelled;
        let effort = self.workers[wi].effort.clone();
        let p = prompt.or(Some(self.workers[wi].prompt.clone()));
        self.log("↻", "violet", format!("respawning {}", task.id));
        self.spawn_task(ctx, task, p, effort)
    }

    // ───────────────────────────── tools ─────────────────────────────

    pub fn on_tool_call(&mut self, ctx: &mut dyn Ctx, a: AgentId, req: Value, tool: &str, args: &Value) {
        let (text, ok) = self.handle_tool(ctx, a, tool, args);
        ctx.tool_result(a, req, text, ok);
    }

    fn handle_tool(&mut self, ctx: &mut dyn Ctx, a: AgentId, tool: &str, args: &Value) -> (String, bool) {
        let s = |k: &str| args.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
        let names = |k: &str| -> Vec<String> {
            match args.get(k) {
                Some(Value::Array(v)) => v.iter().filter_map(|x| x.as_str().map(|s| s.to_string())).collect(),
                Some(Value::String(s)) => s.split(',').map(|x| x.trim().to_string()).collect(),
                _ => vec![],
            }
        };
        match tool {
            "mantra_submit_plan" | "mantra_revise_plan" => {
                let mut plan = match Plan::from_value(args) {
                    Ok(p) => p,
                    Err(e) => return (format!("REJECTED: {e}"), false),
                };
                plan.normalize();
                let revising = tool == "mantra_revise_plan" && self.phase_idx().is_some();
                if revising {
                    // keep completed phases exactly as they were
                    let cur = self.phase_idx().unwrap_or(0);
                    if let Some(old) = &self.plan {
                        let done_ids: Vec<String> = old.phases[..cur.min(old.phases.len())].iter().map(|p| p.id.clone()).collect();
                        let mut phases: Vec<Phase> = old.phases[..cur.min(old.phases.len())].to_vec();
                        phases.extend(plan.phases.iter().filter(|p| !done_ids.contains(&p.id)).cloned());
                        plan.phases = phases;
                    }
                }
                if let Err(errs) = plan.validate(&self.pattern) {
                    self.log("✗", "amber", format!("plan rejected ({} issue(s))", errs.len()));
                    return (format!("REJECTED — fix these and resubmit:\n- {}", errs.join("\n- ")), false);
                }
                let old = self.plan.replace(plan.clone());
                self.plan_version += 1;
                self.save_plan();
                let ntasks: usize = plan.phases.iter().map(|p| p.tasks.len()).sum();
                self.log("☰", "saffron", format!("plan v{}: {} phases, {ntasks} tasks — {}", self.plan_version, plan.phases.len(), trunc(&plan.title, 50)));
                if revising {
                    self.apply_revision(ctx, old);
                    return ("ACCEPTED. The revision is live; completed phases were kept. Now brief the orchestrator if needed and summarize.".into(), true);
                }
                ("ACCEPTED. Reply with a 2-4 line summary of the plan and end your turn.".into(), true)
            }
            "mantra_pause_agents" => {
                let mut done = vec![];
                for n in names("agents") {
                    if let Some(t) = self.resolve(&n) {
                        ctx.interrupt(t);
                        if let Some(wi) = self.worker_idx(t) {
                            self.workers[wi].paused = true;
                        }
                        done.push(n);
                    }
                }
                self.log("‖", "amber", format!("planner paused {}", done.join(", ")));
                (format!("paused: {}", done.join(", ")), true)
            }
            "mantra_resume_agents" => {
                let mut done = vec![];
                for n in names("agents") {
                    if let Some(t) = self.resolve(&n) {
                        if let Some(wi) = self.worker_idx(t) {
                            self.workers[wi].paused = false;
                        }
                        self.mark_edge(t);
                        ctx.prompt(t, "[mantra:resume] Continue your task.".into());
                        done.push(n);
                    }
                }
                (format!("resumed: {}", done.join(", ")), true)
            }
            "mantra_brief_orchestrator" => {
                let m = s("message");
                self.log("✦", "saffron", format!("planner → orchestrator: {}", trunc(&m, 60)));
                if let Some(o) = self.orchestrator {
                    self.mark_edge(o);
                }
                self.orch_inbox.push(format!("[from the planner] {m}"));
                ("delivered — the orchestrator acts on it right after your turn".into(), true)
            }
            "mantra_status" => (self.status_text(ctx), true),
            "mantra_log" => {
                let n = s("agent");
                let full = args.get("full").and_then(|v| v.as_bool()).unwrap_or(false);
                let lines = args.get("lines").and_then(|v| v.as_u64()).unwrap_or(30) as usize;
                match self.resolve(&n).and_then(|t| ctx.agent(t)) {
                    Some(ag) => {
                        let mut t = ag.log_text(if full { 400 } else { lines });
                        crate::util::tail_bytes(&mut t, if full { 40_000 } else { 8_000 });
                        (t, true)
                    }
                    None => (format!("unknown agent '{n}'"), false),
                }
            }
            "mantra_read_phase" => match self.current_phase() {
                Some(p) => {
                    let states: Vec<String> = p.tasks.iter().map(|t| format!("{}: {}", t.id, self.workers.iter().rev().find(|w| w.task.id == t.id).map(|w| format!("{:?}", w.state)).unwrap_or_else(|| "not spawned".into()))).collect();
                    (format!("{}\n\nTask states: {}", serde_json::to_string_pretty(p).unwrap_or_default(), states.join(", ")), true)
                }
                None => ("no phase is active".into(), false),
            },
            "mantra_spawn" => {
                let tid = s("task_id");
                let Some(phase) = self.current_phase().cloned() else { return ("no phase is active".into(), false) };
                let Some(task) = phase.tasks.iter().find(|t| t.id == tid).cloned() else {
                    return (format!("task '{tid}' is not in the current phase (tasks: {})", phase.tasks.iter().map(|t| t.id.clone()).collect::<Vec<_>>().join(", ")), false);
                };
                if self.workers.iter().any(|w| w.task.id == tid && !matches!(w.state, WState::Failed(_) | WState::Cancelled)) {
                    return (format!("{tid} is already spawned — use mantra_prompt or mantra_retry"), false);
                }
                let p = Some(s("prompt")).filter(|x| !x.is_empty());
                let e = Some(s("effort")).filter(|x| !x.is_empty());
                (self.spawn_task(ctx, task, p, e), true)
            }
            "mantra_prompt" => {
                let n = s("agent");
                let m = s("message");
                match self.resolve(&n) {
                    Some(t) if t != a => {
                        if let Some(refusal) = self.worker_done_guard(t) {
                            return (refusal.into(), false);
                        }
                        // F3 loop protection (WP7.2): the orchestrator nagging the same idle
                        // (interrupted/failed but still `Running`) worker three times in five
                        // minutes with no progress means it's stuck — respawn instead of relaying
                        // another prompt into the void.
                        if let Some(wi) = self.worker_idx(t) {
                            let idle = self.workers[wi].state == WState::Running && ctx.agent(t).map(|ag| !ag.busy()).unwrap_or(false);
                            if idle {
                                let now = Instant::now();
                                let w = &mut self.workers[wi];
                                w.idle_prompts.retain(|t0| now.duration_since(*t0) < Duration::from_secs(300));
                                w.idle_prompts.push(now);
                                if w.idle_prompts.len() >= 3 {
                                    let tid = w.task.id.clone();
                                    w.idle_prompts.clear();
                                    self.log("⏰", "amber", format!("{tid}: orchestrator prompted 3× without progress — respawning (watchdog)"));
                                    return match self.respawn(ctx, t, Some("[mantra:watchdog] Respawned after repeated idle prompts without progress.".into())) {
                                        Ok(()) => (format!("{tid} was stuck (prompted 3× with no progress) — respawned instead of forwarding this message"), true),
                                        Err(e) => (e, false),
                                    };
                                }
                            }
                        }
                        self.mark_edge(t);
                        let from = self.name_of(a);
                        ctx.prompt(t, format!("[from the {from}] {m}"));
                        self.log("›", "rose", format!("{from} → {n}: {}", trunc(&m, 60)));
                        (format!("sent to {n}"), true)
                    }
                    _ => (format!("unknown agent '{n}'"), false),
                }
            }
            "mantra_interrupt" => match self.resolve(&s("agent")) {
                Some(t) => match self.worker_done_guard(t) {
                    Some(refusal) => (refusal.into(), false),
                    None => {
                        ctx.interrupt(t);
                        ("interrupted".into(), true)
                    }
                },
                None => ("unknown agent".into(), false),
            },
            "mantra_set_effort" => match self.resolve(&s("agent")) {
                Some(t) => match self.worker_done_guard(t) {
                    Some(refusal) => (refusal.into(), false),
                    None => {
                        let e = ctx.set_effort(t, &s("effort"));
                        (format!("effort set to {e} (applies from the next turn)"), true)
                    }
                },
                None => ("unknown agent".into(), false),
            },
            "mantra_retry" => {
                let tid = s("task_id");
                match self.resolve(&tid) {
                    Some(t) => {
                        if let Some(refusal) = self.worker_done_guard(t) {
                            return (refusal.into(), false);
                        }
                        let p = Some(s("prompt")).filter(|x| !x.is_empty());
                        (self.retry_worker(ctx, t, p), true)
                    }
                    None => {
                        // never spawned (or worktree failed): spawn fresh
                        let task = self.current_phase().and_then(|p| p.tasks.iter().find(|t| t.id == tid).cloned());
                        match task {
                            Some(task) => (self.spawn_task(ctx, task, Some(s("prompt")).filter(|x| !x.is_empty()), None), true),
                            None => (format!("unknown task '{tid}'"), false),
                        }
                    }
                }
            }
            "mantra_wait" => ("OK — end your turn now. Mantra will wake you with the next event.".into(), true),
            "mantra_gate_report" => {
                let pass = args.get("pass").and_then(|v| v.as_bool()).unwrap_or(false);
                let summary = s("summary");
                self.gate_report = Some((pass, summary));
                (format!("recorded: {}. End your turn now.", if pass { "pass" } else { "fail" }), true)
            }
            "mantra_spawn_adhoc" => {
                if !matches!(self.stage, Stage::Finale { .. }) {
                    return ("ad-hoc workers are only available in the final verification step".into(), false);
                }
                let role = s("role");
                if !self.pattern.worker_roles().contains(&role) {
                    return (format!("role must be one of {}", self.pattern.worker_roles().join(", ")), false);
                }
                self.adhoc_seq += 1;
                let task = Task {
                    id: format!("fix-{}", self.adhoc_seq),
                    title: s("title"),
                    role,
                    prompt: s("prompt"),
                    scope: names("scope"),
                    acceptance: String::new(),
                    effort: None,
                };
                let r = self.spawn_task(ctx, task, None, None);
                (format!("{r}. Call mantra_wait; you'll be woken when fixes finish."), true)
            }
            other => (format!("tool '{other}' is not available here"), false),
        }
    }

    fn apply_revision(&mut self, ctx: &mut dyn Ctx, old: Option<Plan>) {
        let (Some(old), Some(new), Some(cur)) = (old, self.plan.clone(), self.phase_idx()) else { return };
        let (Some(op), Some(np)) = (old.phases.get(cur), new.phases.get(cur)) else { return };
        let mut notes = vec![];
        for t in &op.tasks {
            match np.tasks.iter().find(|n| n.id == t.id) {
                None => {
                    // removed → cancel its worker
                    let agents: Vec<AgentId> = self.workers.iter().filter(|w| w.task.id == t.id).filter_map(|w| w.agent).collect();
                    for a in agents {
                        ctx.stop(a, true);
                    }
                    for w in self.workers.iter_mut().filter(|w| w.task.id == t.id) {
                        w.state = WState::Cancelled;
                    }
                    notes.push(format!("task {} was REMOVED (its worker is stopped)", t.id));
                }
                Some(n) if n.prompt != t.prompt || n.scope != t.scope => notes.push(format!("task {} CHANGED — steer it with mantra_prompt or respawn with mantra_retry", t.id)),
                _ => {}
            }
        }
        for n in &np.tasks {
            if !op.tasks.iter().any(|t| t.id == n.id) {
                notes.push(format!("task {} was ADDED — spawn it with mantra_spawn", n.id));
            }
        }
        if !notes.is_empty() {
            self.log("☰", "saffron", format!("revision: {}", notes.len()));
            self.orch_inbox.push(format!("The planner revised the current phase:\n- {}", notes.join("\n- ")));
        }
        self.check_phase_done(ctx);
    }
}

/// Prepend an optional watchdog/manual note to a resume prompt.
fn join_note(note: Option<String>, base: String) -> String {
    match note {
        Some(n) => format!("{n}\n\n{base}"),
        None => base,
    }
}

fn parse_gate_from_text(t: Option<&str>) -> Option<(bool, String)> {
    let t = t?;
    let low = t.to_lowercase();
    if low.contains("gate: pass") || low.contains("gate passed") {
        return Some((true, trunc(t, 200)));
    }
    if low.contains("gate: fail") {
        return Some((false, trunc(t, 200)));
    }
    None
}

#[cfg(test)]
mod halt_tests {
    use super::*;
    use crate::agent::Agent;
    use std::collections::HashMap;

    /// A minimal fake `Ctx` for unit-testing `Run`'s halt/resume behaviour without a real hub.
    struct TestCtx {
        agents: HashMap<AgentId, Agent>,
        next: AgentId,
        interrupted: Vec<AgentId>,
        prompts: Vec<(AgentId, String)>,
    }
    impl TestCtx {
        fn new() -> TestCtx {
            TestCtx { agents: HashMap::new(), next: 1, interrupted: vec![], prompts: vec![] }
        }
        /// Register a fake agent that looks busy (a live turn in progress).
        fn add_busy(&mut self) -> AgentId {
            let id = self.next;
            self.next += 1;
            let mut a = Agent::new(id, "a", "worker", PathBuf::from("."));
            a.turn_active = true;
            self.agents.insert(id, a);
            id
        }
        /// Register a fake agent that is idle (no turn in progress) — the watchdog's target state.
        fn add_idle(&mut self) -> AgentId {
            let id = self.next;
            self.next += 1;
            self.agents.insert(id, Agent::new(id, "a", "worker", PathBuf::from(".")));
            id
        }
        /// Back-date an agent's `last_event` so it looks idle for `secs` (the watchdog's only
        /// signal), without an actual sleep.
        fn set_idle_secs(&mut self, a: AgentId, secs: u64) {
            if let Some(ag) = self.agents.get_mut(&a) {
                ag.last_event = Instant::now() - Duration::from_secs(secs);
                ag.turn_active = false;
                ag.awaiting_start = false;
            }
        }
    }

    /// A minimal `Worker` for tests that don't need a real workspace/prepare cycle.
    fn mk_worker(id: &str, role: &str, agent: AgentId, state: WState) -> Worker {
        Worker {
            task: Task { id: id.into(), role: role.into(), ..Default::default() },
            prompt: String::new(),
            effort: None,
            agent: Some(agent),
            attempt: 1,
            state,
            report: String::new(),
            wt: None,
            branch: String::new(),
            spawned: Instant::now(),
            finished: None,
            tripwires: vec![],
            adhoc: false,
            paused: false,
            stall_flagged: false,
            budget_flagged: false,
            context_override: None,
            idle_prompts: vec![],
        }
    }
    impl Ctx for TestCtx {
        fn spawn(&mut self, _r: SpawnReq) -> AgentId {
            let id = self.next;
            self.next += 1;
            self.agents.insert(id, Agent::new(id, "a", "worker", PathBuf::from(".")));
            id
        }
        fn prompt(&mut self, a: AgentId, text: String) {
            self.prompts.push((a, text));
        }
        fn prompt_mode(&mut self, a: AgentId, text: String, _mode: Send) {
            self.prompts.push((a, text));
        }
        fn interrupt(&mut self, a: AgentId) {
            self.interrupted.push(a);
            if let Some(ag) = self.agents.get_mut(&a) {
                ag.turn_active = false;
            }
        }
        fn compact(&mut self, _a: AgentId) {}
        fn stop(&mut self, _a: AgentId, _archive: bool) {}
        fn tool_result(&mut self, _a: AgentId, _req: Value, _text: String, _ok: bool) {}
        fn set_effort(&mut self, _a: AgentId, effort: &str) -> String {
            effort.to_string()
        }
        fn agent(&self, a: AgentId) -> Option<&Agent> {
            self.agents.get(&a)
        }
        fn job(&mut self, _tag: JobTag, _f: Box<dyn FnOnce() -> JobOut + std::marker::Send>) {}
        fn notify(&mut self, _text: &str) {}
    }

    #[test]
    fn halt_interrupts_busy_agents_and_resume_reprompts_them() {
        let mut ctx = TestCtx::new();
        let busy = ctx.add_busy();
        let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "test".into());
        run.orchestrator = Some(busy); // must be a run agent to be found & interrupted
        run.halt(&mut ctx, HaltReason::ProviderRejected, Some(busy), "qa (sol via zai): Unexpected message role.".into());
        assert!(run.halted());
        assert!(ctx.interrupted.contains(&busy), "the busy agent must be interrupted");
        assert!(run.alerts.iter().any(|a| a.contains("Unexpected message role")));
        assert_eq!(run.halt.as_ref().unwrap().reason, HaltReason::ProviderRejected);

        run.resume(&mut ctx);
        assert!(!run.halted());
        assert!(ctx.prompts.iter().any(|(a, t)| *a == busy && t.contains("[mantra:resume]")), "resume must re-prompt the halted agent");
    }

    #[test]
    fn user_pause_is_quiet_but_still_a_halt() {
        let mut ctx = TestCtx::new();
        let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "test".into());
        run.toggle_pause(&mut ctx);
        assert!(run.halted());
        assert_eq!(run.halt.as_ref().unwrap().reason, HaltReason::User);
        assert!(run.alerts.is_empty(), "a manual pause is not an alert");
        run.toggle_pause(&mut ctx);
        assert!(!run.halted(), "space toggles a manual pause back off");
    }

    fn one_task_phase() -> Plan {
        Plan { phases: vec![Phase { id: "p1".into(), name: "p1".into(), tasks: vec![Task { id: "t1".into(), role: "worker-small".into(), ..Default::default() }], ..Default::default() }], ..Default::default() }
    }

    // WP7.2 watchdog tests ---------------------------------------------------------------------

    #[test]
    fn watchdog_nudges_then_respawns_then_wakes_planner() {
        let mut ctx = TestCtx::new();
        let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "test".into());
        let planner = ctx.add_idle();
        let orch = ctx.add_idle();
        run.planner = Some(planner);
        run.orchestrator = Some(orch);
        run.plan = Some(one_task_phase());
        run.stage = Stage::Phase { idx: 0, step: PhaseStep::Orchestrating };

        // 90s idle (default watchdog_seconds): nudge the orchestrator itself.
        ctx.set_idle_secs(orch, 90);
        run.watchdog_tick(&mut ctx);
        assert!(ctx.prompts.iter().any(|(a, t)| *a == orch && t.contains("[mantra:watchdog]")), "expected a nudge at 90s, got {:?}", ctx.prompts);

        // 240s idle (default watchdog_escalate_seconds): respawn the orchestrator.
        ctx.prompts.clear();
        ctx.set_idle_secs(orch, 240);
        run.watchdog_tick(&mut ctx);
        let new_orch = run.orchestrator.expect("orchestrator still set after a respawn");
        assert_ne!(new_orch, orch, "240s idle must respawn the orchestrator (a new agent id)");
        assert!(ctx.prompts.iter().any(|(a, t)| *a == new_orch && t.contains("[mantra:respawn]")));

        // 480s idle (2x escalate) on the respawned orchestrator: wake the planner instead.
        ctx.prompts.clear();
        ctx.set_idle_secs(new_orch, 480);
        run.watchdog_tick(&mut ctx);
        assert!(ctx.prompts.iter().any(|(a, t)| *a == planner && t.contains("did not act")), "expected the planner to be woken at 480s, got {:?}", ctx.prompts);
    }

    #[test]
    fn watchdog_leaves_a_legitimately_sleeping_orchestrator_alone() {
        let mut ctx = TestCtx::new();
        let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "test".into());
        let orch = ctx.add_idle();
        let worker_agent = ctx.add_busy();
        run.orchestrator = Some(orch);
        run.plan = Some(one_task_phase());
        run.stage = Stage::Phase { idx: 0, step: PhaseStep::Orchestrating };
        run.workers.push(mk_worker("t1", "worker-small", worker_agent, WState::Running));
        ctx.set_idle_secs(orch, 500); // long idle, but a worker is running — this is legitimate
        run.watchdog_tick(&mut ctx);
        assert!(ctx.prompts.is_empty(), "an orchestrator asleep in mantra_wait while a worker runs must not be nudged: {:?}", ctx.prompts);
    }

    #[test]
    fn f3_loop_protection_respawns_a_repeatedly_prompted_idle_worker() {
        let mut ctx = TestCtx::new();
        let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "test".into());
        let orch = ctx.add_idle();
        // "idle" per the F3 rule: the turn ended (interrupted) but the worker is still `Running`.
        let worker_agent = ctx.add_idle();
        run.orchestrator = Some(orch);
        run.stage = Stage::Phase { idx: 0, step: PhaseStep::Orchestrating };
        run.workers.push(mk_worker("t1", "worker-small", worker_agent, WState::Running));

        for n in 0..2 {
            let (msg, ok) = run.handle_tool(&mut ctx, orch, "mantra_prompt", &json!({"agent": "t1", "message": "keep going"}));
            assert!(ok, "prompt #{n} should be forwarded normally: {msg}");
            assert!(msg.starts_with("sent to"), "prompt #{n}: {msg}");
        }
        let (msg, ok) = run.handle_tool(&mut ctx, orch, "mantra_prompt", &json!({"agent": "t1", "message": "keep going"}));
        assert!(ok, "{msg}");
        assert!(msg.contains("respawned"), "the third idle prompt within 5 minutes must respawn the worker instead of forwarding: {msg}");
        assert_eq!(run.workers.len(), 2, "the F3 respawn must produce a new worker attempt");
        assert_eq!(run.workers[0].state, WState::Cancelled);
        assert_eq!(run.workers[1].attempt, 2);
    }

    #[test]
    fn tool_guard_refuses_steering_a_done_worker_outside_orchestrating() {
        let mut ctx = TestCtx::new();
        let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "test".into());
        let orch = ctx.add_idle();
        let worker_agent = ctx.add_idle();
        run.orchestrator = Some(orch);
        run.stage = Stage::Phase { idx: 0, step: PhaseStep::Gate { round: 1 } }; // merging/gating, not Orchestrating
        run.workers.push(mk_worker("t1", "worker-small", worker_agent, WState::Done));

        let (msg, ok) = run.handle_tool(&mut ctx, orch, "mantra_prompt", &json!({"agent": "t1", "message": "hi"}));
        assert!(!ok);
        assert!(msg.contains("wait for the handoff"), "{msg}");

        let (msg, ok) = run.handle_tool(&mut ctx, orch, "mantra_interrupt", &json!({"agent": "t1"}));
        assert!(!ok, "{msg}");
    }

    #[test]
    fn respawn_planner_during_review_keeps_stage_review() {
        let mut ctx = TestCtx::new();
        let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "test".into());
        let planner = ctx.add_idle();
        run.planner = Some(planner);
        run.stage = Stage::Review;
        run.plan = Some(Plan { title: "x".into(), ..Default::default() });
        run.plan_version = 1;

        run.respawn(&mut ctx, planner, None).unwrap();

        assert_eq!(run.stage, Stage::Review, "respawning the planner mid-review must not change the stage");
        let new_planner = run.planner.expect("planner still set after respawn");
        assert_ne!(new_planner, planner);
        assert!(ctx.prompts.iter().any(|(a, t)| *a == new_planner && t.contains("[mantra:respawn]")));
    }

    /// Review fix: the built-in default pattern's last finale step reuses `self.planner` as the
    /// finale agent id (`start_finale`), so `respawn`/`watchdog_role_tag` must disambiguate via
    /// `Stage::Finale` before falling back to the plain planner check — otherwise this shared id
    /// silently misroutes to `respawn_planner` (wrong resume prompt, dangling `finale_agent`).
    #[test]
    fn respawn_shared_planner_finale_agent_routes_to_finale_not_planner() {
        let mut ctx = TestCtx::new();
        let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "test".into());
        let planner = ctx.add_idle();
        run.planner = Some(planner);
        run.finale_agent = Some(planner); // shared id, exactly as `start_finale` leaves it
        run.stage = Stage::Finale { idx: 2 }; // built-in pattern's finale[2].role == "planner"
        run.plan = Some(Plan { title: "x".into(), ..Default::default() });

        run.respawn(&mut ctx, planner, None).unwrap();

        assert!(matches!(run.stage, Stage::Finale { idx: 2 }), "must stay on the finale step, got {:?}", run.stage);
        assert_eq!(run.finale_agent, Some(planner), "the shared id keeps serving as the finale agent");
        assert!(ctx.prompts.iter().any(|(a, t)| *a == planner && t.contains("[mantra:finale]")), "expected the finale step's own prompt, got {:?}", ctx.prompts);
        assert!(!ctx.prompts.iter().any(|(_, t)| t.contains("Continue from stage")), "must not fall through to the planner respawn's resume prompt");
    }

    /// Review fix: `respawn_gate` must reuse the gate's *current* round (via
    /// `spawn_gate_at_round`), not silently reset it to 1 — `gate_max_rounds` and the L4
    /// identical-blocker halt are both keyed off `round`.
    #[test]
    fn respawn_gate_preserves_the_current_round() {
        let mut ctx = TestCtx::new();
        let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "test".into());
        let gate = ctx.add_idle();
        run.gate_agent = Some(gate);
        run.plan = Some(one_task_phase());
        run.stage = Stage::Phase { idx: 0, step: PhaseStep::Gate { round: 2 } };

        run.respawn(&mut ctx, gate, None).unwrap();

        match run.stage {
            Stage::Phase { step: PhaseStep::Gate { round }, .. } => assert_eq!(round, 2, "a respawned gate must keep its round, not reset to 1"),
            other => panic!("expected Phase{{Gate}}, got {other:?}"),
        }
        let new_gate = run.gate_agent.expect("gate agent still set after respawn");
        assert_ne!(new_gate, gate);
    }

    /// Review fix: if the stage has already moved on (gate agent still registered but the run
    /// left `Stage::Phase{Gate}`, e.g. the round just finished), `respawn` must surface an error
    /// instead of silently no-op'ing and letting the caller show a false "respawned" toast.
    #[test]
    fn respawn_gate_after_stage_moved_on_is_an_error() {
        let mut ctx = TestCtx::new();
        let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "test".into());
        let gate = ctx.add_idle();
        run.gate_agent = Some(gate);
        run.stage = Stage::Review; // no longer Phase{Gate} — phase_idx() is None

        let err = run.respawn(&mut ctx, gate, None).unwrap_err();
        assert!(err.contains("already moved on"), "{err}");
        assert_eq!(run.gate_agent, Some(gate), "no-op must not tear down the still-registered agent");
    }

    /// Same guard for the finale agent: if `stage` is no longer `Stage::Finale`, `respawn` must
    /// return an error rather than `Ok(())` with nothing having happened.
    #[test]
    fn respawn_finale_after_stage_moved_on_is_an_error() {
        let mut ctx = TestCtx::new();
        let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "test".into());
        let finale = ctx.add_idle();
        run.finale_agent = Some(finale);
        run.stage = Stage::Review; // no longer Stage::Finale

        let err = run.respawn(&mut ctx, finale, None).unwrap_err();
        assert!(err.contains("already moved on"), "{err}");
        assert_eq!(run.finale_agent, Some(finale), "no-op must not tear down the still-registered agent");
    }

    // L4 gate-loop protection -------------------------------------------------------------------

    #[test]
    fn gate_loop_protection_halts_on_two_identical_blockers() {
        let mut ctx = TestCtx::new();
        let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "test".into());
        let gate = ctx.add_idle();
        run.gate_agent = Some(gate);
        run.plan = Some(one_task_phase());
        run.stage = Stage::Phase { idx: 0, step: PhaseStep::Gate { round: 1 } };
        let blocked = "GATE: fail — Blocked: shell sandbox fails before any command runs (bwrap)";
        ctx.agents.get_mut(&gate).unwrap().final_message = Some(blocked.into());

        run.on_turn_done(&mut ctx, gate, "completed", None, None);
        assert!(!run.halted(), "the first blocked round should just try again");

        ctx.agents.get_mut(&gate).unwrap().final_message = Some(blocked.into());
        run.on_turn_done(&mut ctx, gate, "completed", None, None);
        assert!(run.halted(), "two consecutive identical blockers must halt immediately, not spend the remaining rounds");
        assert_eq!(run.halt.as_ref().unwrap().reason, HaltReason::GateExhausted);
    }

    // WP12.4/L1: EnvironmentBroken -------------------------------------------------------------

    #[test]
    fn environment_broken_halts_once_with_the_sandbox_hint() {
        let mut ctx = TestCtx::new();
        let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "test".into());
        let worker_agent = ctx.add_busy();
        run.workers.push(mk_worker("t1", "worker-small", worker_agent, WState::Running));

        run.on_environment_broken(&mut ctx, worker_agent, "bwrap: setting up uid map: Permission denied".into());
        assert!(run.halted());
        assert_eq!(run.halt.as_ref().unwrap().reason, HaltReason::Environment);
        assert!(run.alerts.iter().any(|a| a.to_lowercase().contains("sandbox")));

        let first = run.halt.as_ref().unwrap().message.clone();
        run.on_environment_broken(&mut ctx, worker_agent, "bwrap: a different failure".into());
        assert_eq!(run.halt.as_ref().unwrap().message, first, "a second EnvironmentBroken must not overwrite the first halt");
    }
}
