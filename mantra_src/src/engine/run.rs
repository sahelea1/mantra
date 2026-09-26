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

/// A transition that became due while the run was halted — the pause boundary. Results that
/// arrive mid-pause (a QA report, gate checks, a worker's output, the cleanup commit) are still
/// recorded, but nothing that *starts* an agent runs until the run resumes: the transition is
/// remembered here, saved with the state (so a run restored from disk sees it too), and performed
/// exactly once by `resume` (`perform_pending`).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum Pending {
    /// `start_phase(idx)` — past the last phase this means the finale.
    Phase { idx: usize },
    /// `start_finale(idx)` — past the last step this completes the run.
    Finale { idx: usize },
    /// The gate of phase `idx` at `round`: the gate agent's next round, or a fresh gate agent.
    Gate { idx: usize, round: u32 },
    /// `begin_handoff(idx)` — phase `idx` passed its gate.
    Handoff { idx: usize },
}

impl Pending {
    fn label(&self) -> String {
        match self {
            Pending::Phase { idx } => format!("phase {}", idx + 1),
            Pending::Finale { idx } => format!("finale step {}", idx + 1),
            Pending::Gate { idx, round } => format!("phase {} gate round {round}", idx + 1),
            Pending::Handoff { idx } => format!("phase {} handoff", idx + 1),
        }
    }
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

/// Stable name of a worker state — the saved `state.json` and the web protocol both use it.
pub fn worker_state_str(s: &WState) -> &'static str {
    match s {
        WState::Queued => "queued",
        WState::Preparing => "preparing",
        WState::Running => "running",
        WState::Retrying(_) => "retrying",
        WState::Done => "done",
        WState::Failed(_) => "failed",
        WState::Cancelled => "cancelled",
    }
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
    /// The planner, while the orchestrator (or the manager) waits on a question it passed up
    /// (`mantra_ask`).
    Answering,
    /// The manager, while it holds an escalated halt or a watchdog case nobody else could clear.
    Managing,
}

/// A halt handed to the manager (`flow.manager`) before the planner: when, by when it must have
/// acted (else the planner gets it), whether it has had its one reminder, and the halt's message
/// so the planner's escalation can carry the same text.
struct ManagerEscalation {
    deadline: Instant,
    reminded: bool,
    message: String,
    /// Who is next if the manager does not act: the planner (gate/attempts exhausted — it can
    /// revise the plan) or nobody but the user (an agent's turn failed: the planner may be the
    /// very agent that is failing, and there is no plan change that fixes a broken process).
    planner_next: bool,
}

/// An idle agent the watchdog handed to the manager (ladder step 3): who, the deadline by which
/// the manager must have finished a turn about it (else the planner is woken, as without a
/// manager), and whether the ladder has already respawned a silent manager for it — once only, so
/// a manager that never gets going cannot be respawned forever.
struct ManagerCase {
    deadline: Instant,
    about: String,
    respawned: bool,
}

/// A question the planner put to the user (`mantra_ask_user`) — the top rung of the chain of
/// command. Soft: the run keeps going; the stage shows the question until the user's next
/// message answers it (`Run::user_input`).
pub struct Question {
    pub from: AgentId,
    pub text: String,
    pub since: Instant,
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
    /// 1-based sequence number of this line within the run (`Run::pulse_seq` when it was logged):
    /// the web UI sends only lines newer than the last one it sent.
    pub n: u64,
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
    /// A transition deferred by the pause boundary, performed once on resume (see `Pending`).
    pub pending: Option<Pending>,
    pub plan: Option<Plan>,
    pub plan_version: u32,
    pub planner: Option<AgentId>,
    /// The run-wide supervisor (`flow.manager`), spawned when the first phase starts and re-spawned
    /// on demand; `None` when the pattern has no manager role.
    pub manager: Option<AgentId>,
    pub orchestrator: Option<AgentId>,
    pub gate_agent: Option<AgentId>,
    pub finale_agent: Option<AgentId>,
    pub workers: Vec<Worker>,
    pub history: Vec<PhaseRecord>,
    pub checks: Vec<CheckResult>,
    pub conflicts: Vec<String>,
    pub gate_report: Option<(bool, String)>,
    pub pulse: VecDeque<Pulse>,
    /// Total pulse lines ever logged by this run (the ring above keeps only the last 300).
    pub pulse_seq: u64,
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
    /// When the run completed or stopped (`Stage::Done` / `Failed`): `elapsed` stops here, so
    /// the duration a finished run shows stays put.
    pub finished_unix: Option<u64>,
    /// The planner's open question to the user, if any.
    pub question: Option<Question>,
    /// Agents waiting on an answer from the rung above them (worker/gate/finale → orchestrator,
    /// orchestrator → planner), keyed by the asker. Cleared when the answer is delivered.
    pending_questions: HashMap<AgentId, String>,
    /// When the orchestrator last got a periodic coherence review (`settings.review_minutes`).
    last_review: Instant,
    /// Signature of the failing gate checks at the last verify round: the same failure twice in a
    /// row means the gate agent isn't fixing it (it may even keep reporting pass) — escalate.
    last_verify_sig: Option<String>,
    /// Attempts granted on top of `worker_retries + 3` by the planner (`mantra_resume_run`) or the
    /// user (`r`) after an `AttemptsExhausted` halt, so "try once more" is actually possible.
    extra_attempts: u32,
    /// A halt was escalated to the planner and it hasn't acted yet (`escalate_to_planner`).
    escalation_open: bool,
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
    /// Expected-active agents already reported as silent-but-busy in `watchdog_tick`, so one long
    /// quiet spell costs the pulse feed exactly one line. An entry is dropped the moment the agent
    /// speaks again, which is what lets a *later* spell be reported afresh.
    silent_flagged: HashSet<AgentId>,
    /// Messages for the manager, delivered together the next time it is idle (`wake_manager`).
    manager_inbox: Vec<String>,
    /// When the manager last got a health digest (`settings.manager_minutes`).
    last_health: Instant,
    /// A halt currently in the manager's hands (see `escalate_to_manager`).
    manager_escalation: Option<ManagerEscalation>,
    /// The watchdog woke the manager about an idle agent (ladder step 3). Cleared when the manager
    /// completes a turn; past the deadline the planner gets it as before.
    manager_watchdog: Option<ManagerCase>,
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
            pending: None,
            plan: None,
            plan_version: 0,
            planner: None,
            manager: None,
            orchestrator: None,
            gate_agent: None,
            finale_agent: None,
            workers: vec![],
            history: vec![],
            checks: vec![],
            conflicts: vec![],
            gate_report: None,
            pulse: VecDeque::new(),
            pulse_seq: 0,
            phase_started: Instant::now(),
            ws: None,
            handoff: String::new(),
            alerts: vec![],
            edges: HashMap::new(),
            want_review: false,
            agent_meta: HashMap::new(),
            started_unix: crate::util::unix_secs(),
            finished_unix: None,
            question: None,
            pending_questions: HashMap::new(),
            last_review: Instant::now(),
            last_verify_sig: None,
            extra_attempts: 0,
            escalation_open: false,
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
            silent_flagged: HashSet::new(),
            manager_inbox: vec![],
            last_health: Instant::now(),
            manager_escalation: None,
            manager_watchdog: None,
        }
    }

    // ───────────────────────────── bookkeeping ─────────────────────────────

    pub fn log(&mut self, glyph: &str, color: &'static str, text: impl Into<String>) {
        // Defensive: crash reasons and agent-supplied text can carry ANSI from a subprocess's
        // stderr; the journal and pulse feed must never show raw escape codes.
        let text = crate::util::strip_ansi(&text.into());
        self.pulse_seq += 1;
        let p = Pulse { n: self.pulse_seq, at: Instant::now(), t: clock(), glyph: glyph.to_string(), color, text };
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
            } else if Some(a) == self.manager {
                "manager".into()
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
                let state = worker_state_str(&w.state);
                let error = match &w.state {
                    WState::Failed(e) => e.clone(),
                    _ => String::new(),
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
            pending: self.pending.clone(),
            started_unix: self.started_unix,
            finished_unix: self.finished_unix,
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

    /// Wall-clock time since the run was first started — survives a resume (unlike `started`,
    /// which is this process's `Instant`). Frozen at `finished_unix` once the run is over: the
    /// completion time is a result, not a clock.
    pub fn elapsed(&self) -> Duration {
        let end = self.finished_unix.unwrap_or_else(crate::util::unix_secs);
        Duration::from_secs(end.saturating_sub(self.started_unix))
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
            // The manager is not part of the work being stopped — it is who gets told about the
            // halt next, mid-turn or not (a steer into its running turn is exactly right), and it
            // must not be re-prompted to "continue where you left off" on resume either.
            if Some(a) == self.manager && Some(a) != agent {
                continue;
            }
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
        self.resume_by(ctx, None);
    }

    /// `resume`, with the agent that caused it (the planner acting on an escalation) left out of
    /// the "continue where you left off" re-prompts — it is mid-turn and knows.
    pub fn resume_by(&mut self, ctx: &mut dyn Ctx, except: Option<AgentId>) {
        if self.halt.is_none() {
            return;
        }
        self.halt = None;
        self.escalation_open = false;
        self.manager_escalation = None;
        self.last_verify_sig = None;
        self.alerts.clear();
        let paused = std::mem::take(&mut self.paused_agents);
        self.log("▶", "green", "run resumed");
        self.save_state();
        let t0 = Instant::now();
        if let Some(p) = self.pending.take() {
            // A transition that became due during the halt (the pause boundary): now, once.
            self.perform_pending(ctx, p);
        } else if let Stage::Phase { idx, step } = self.stage.clone() {
            match step {
                PhaseStep::Orchestrating => {
                    self.fill_slots(ctx);
                    self.check_phase_done(ctx);
                    self.kick_orchestrator(ctx, &paused);
                }
                PhaseStep::Gate { .. } | PhaseStep::Checks { .. } => {
                    let has_checks = self.current_phase().map(|p| !p.gate.checks.is_empty()).unwrap_or(false);
                    if has_checks {
                        // verify first: a revised/fixed check may already be green
                        self.run_checks(ctx, idx, 1);
                    } else if let Some(g) = self.gate_agent {
                        // no checks to verify with — the gate agent itself is the gate
                        self.stage = Stage::Phase { idx, step: PhaseStep::Gate { round: 1 } };
                        self.gate_report = None;
                        self.mark_edge(g);
                        ctx.prompt(g, "[mantra:gate-round 1] The run resumed. Continue the gate: make the result coherent and green, then call mantra_gate_report.".into());
                    } else {
                        self.spawn_gate_at_round(ctx, idx, 1);
                    }
                }
                _ => {}
            }
        }
        // Re-prompt what the halt interrupted — unless the pick-up above already gave it its
        // next prompt (`edges` is stamped at every engine prompt) or archived it.
        let live = self.all_agents();
        for a in paused {
            if Some(a) == except || !live.contains(&a) || self.edges.get(&a).map(|t| *t >= t0).unwrap_or(false) {
                continue;
            }
            ctx.prompt(a, "[mantra:resume] The run was paused and is resuming now. Continue where you left off.".into());
        }
        self.wake_orch(ctx);
    }

    /// The pause boundary: `what` would start agents, but the run is halted. Remember it (on disk
    /// too) for `resume` to perform — see `Pending`.
    fn defer(&mut self, what: Pending) {
        self.log("‖", "amber", format!("{} is due — it starts when the run resumes", what.label()));
        self.pending = Some(what);
        self.save_state();
    }

    /// Perform a transition the pause boundary deferred (`defer`). The caller has taken it out of
    /// `self.pending`, so it happens once.
    pub(super) fn perform_pending(&mut self, ctx: &mut dyn Ctx, what: Pending) {
        self.log("▶", "green", format!("{} was due while halted — starting it", what.label()));
        match what {
            Pending::Phase { idx } => self.start_phase(ctx, idx),
            Pending::Finale { idx } => self.start_finale(ctx, idx),
            Pending::Handoff { idx } => self.begin_handoff(ctx, idx),
            Pending::Gate { idx, round } => match self.gate_agent {
                Some(g) => {
                    self.stage = Stage::Phase { idx, step: PhaseStep::Gate { round } };
                    self.gate_report = None;
                    self.mark_edge(g);
                    let s = self.check_summary();
                    ctx.prompt(g, format!("[mantra:gate-round {round}] The run resumed. Continue the gate: fix what's left so it passes, then call mantra_gate_report.\nGate checks:\n{s}"));
                }
                None => self.spawn_gate_at_round(ctx, idx, round),
            },
        }
    }

    /// Resume's safety net for an idle orchestrator with tasks nobody has started: one that was
    /// (re)spawned during the halt had every `mantra_spawn` refused and ended its turn with no
    /// event ahead to wake it. `paused` are the agents the resume re-prompts anyway.
    fn kick_orchestrator(&mut self, ctx: &mut dyn Ctx, paused: &[AgentId]) {
        if !matches!(self.stage, Stage::Phase { step: PhaseStep::Orchestrating, .. }) {
            return;
        }
        let Some(o) = self.orchestrator else { return };
        if paused.contains(&o) || ctx.agent(o).map(|a| a.busy()).unwrap_or(false) {
            return;
        }
        let missing: Vec<String> = self.current_phase().map(|p| p.tasks.iter().filter(|t| !self.workers.iter().any(|w| w.task.id == t.id)).map(|t| t.id.clone()).collect()).unwrap_or_default();
        if missing.is_empty() {
            return;
        }
        self.log("◉", "violet", format!("orchestrator idle with {} unstarted task(s) — waking it", missing.len()));
        self.mark_edge(o);
        ctx.prompt(o, format!("[mantra:resume] The run resumed. These tasks have no attempt yet: {}. Spawn them with mantra_spawn (spawns were refused while the run was paused), then call mantra_wait.", missing.join(", ")));
    }

    /// Chain of command, top rung before the user: a halt the run could not sort out by itself
    /// (gate exhausted, attempts exhausted) is handed to the planner with everything it needs to
    /// decide — revise the phase, give the stuck agent a hint and resume, or ask the user.
    fn halt_and_escalate(&mut self, ctx: &mut dyn Ctx, reason: HaltReason, agent: Option<AgentId>, message: String) {
        if self.halted() {
            return;
        }
        self.halt(ctx, reason, agent, message.clone());
        // With a manager in the pattern it is first in line — it has the operational tools
        // (hint, retry, respawn, resume) and hands a plan problem up itself. Without one, or if
        // it does not act in time (`manager_tick`), the planner gets it as before.
        if self.has_manager() {
            self.escalate_to_manager(ctx, message, true);
        } else {
            self.escalate_to_planner(ctx, message);
        }
    }

    /// An agent's turn failed past its retries (or the planner sat on a watchdog escalation): the
    /// run halts for the user — unless a manager is there to respawn the agent first. Nobody
    /// else is asked: the planner may be the failing agent itself. The manager is never its own
    /// case here (its failures drop it, see `on_turn_done`).
    fn halt_turn_failed(&mut self, ctx: &mut dyn Ctx, agent: Option<AgentId>, message: String) {
        if self.halted() {
            return;
        }
        self.halt(ctx, HaltReason::AgentTurnFailed, agent, message.clone());
        if self.has_manager() && agent != self.manager {
            self.escalate_to_manager(ctx, message, false);
        }
    }

    /// What every escalation prompt carries: the workers, the last gate report, the last checks.
    fn escalation_context(&self, ctx: &dyn Ctx) -> (String, String) {
        let status = self.status_text(ctx);
        let checks = self.check_summary();
        let phase = self.phase_idx().map(|i| format!("phase {}", i + 1)).unwrap_or_else(|| "the finale".into());
        let report = self.gate_report.as_ref().map(|(ok, s)| format!("Last gate report: {} — {}\n", if *ok { "pass" } else { "fail" }, trunc(s, 400))).unwrap_or_default();
        (phase, format!("Workers:\n{status}{report}Gate checks (last run):\n{checks}"))
    }

    /// The manager's rung of an escalated halt: it gets the same facts as the planner would, the
    /// operational moves it has, and a deadline (`watchdog_escalate_seconds`) after which the
    /// planner is asked instead — so a silent manager never keeps a run halted.
    fn escalate_to_manager(&mut self, ctx: &mut dyn Ctx, message: String, planner_next: bool) {
        let Some(m) = self.ensure_manager(ctx) else {
            if planner_next {
                self.escalate_to_planner(ctx, message);
            }
            return;
        };
        let secs = self.pattern.settings.watchdog_escalate_seconds.max(120);
        let (phase, facts) = self.escalation_context(ctx);
        self.manager_escalation = Some(ManagerEscalation { deadline: Instant::now() + Duration::from_secs(secs), reminded: false, message: message.clone(), planner_next });
        self.log("◈", "blue", format!("escalated to the manager: {}", trunc(&message, 60)));
        self.mark_edge(m);
        let moves = if planner_next {
            format!("- mantra_resume_run(note) — the plan is right; give the stuck agent a concrete hint and let it try again (an exhausted task gets one more attempt).\n- mantra_retry(task_id, prompt) or mantra_respawn(agent, note) — a fresh start with a sharper brief; during this halt that also resumes the run.\n- mantra_ask(question) — when the tasks or the gate checks themselves are wrong (a check that cannot pass on this machine is a plan bug, not a QA failure): the planner can revise the plan, which resumes the run by itself.\n- mantra_ask_user(question) — only if nobody in the team can decide.\nIf you do nothing within {secs}s the planner gets it.")
        } else {
            format!("- mantra_respawn(agent, note) — a fresh process and thread for the failed agent, briefed with the current state plus your note; that also resumes the run. This is almost always the move.\n- mantra_resume_run(note) — retry the same agent's turn once more with a hint.\n- mantra_ask_user(question) — if the failure is about the machine or the credentials (read its log first: mantra_log).\nIf you do nothing within {secs}s the halt is left for the user.")
        };
        ctx.prompt(m, format!("[mantra:escalation] {message}\n\nWe are in {phase}; the run is halted until someone acts, and you are first in line.\n{facts}\n\nFind out what actually went wrong (mantra_log on the agent involved, mantra_journal), then act with one of:\n{moves} Then summarize in 2-4 lines."));
    }

    /// The manager did not resolve an escalated halt: the planner is next when it could help
    /// (gate/attempts exhausted), otherwise the band is the user's.
    fn manager_gave_up(&mut self, ctx: &mut dyn Ctx, e: ManagerEscalation, why: &str) {
        if e.planner_next {
            self.log("◈", "amber", format!("the manager did not resolve the halt{why} — the planner gets it"));
            self.escalate_to_planner(ctx, e.message);
        } else {
            self.log("◈", "amber", format!("the manager did not resolve the halt{why} — {}", self.halt_hint()));
        }
    }

    /// Chain of command, top rung before the user: a halt the run could not sort out by itself
    /// (gate exhausted, attempts exhausted) is handed to the planner with everything it needs to
    /// decide — revise the phase, give the stuck agent a hint and resume, or ask the user.
    fn escalate_to_planner(&mut self, ctx: &mut dyn Ctx, message: String) {
        let p = match self.planner {
            Some(p) => p,
            None => self.spawn_planner(ctx),
        };
        let (phase, facts) = self.escalation_context(ctx);
        self.escalation_open = true;
        self.log("✦", "saffron", format!("escalated to the planner: {}", trunc(&message, 60)));
        self.mark_edge(p);
        ctx.prompt(
            p,
            format!(
                "[mantra:escalation] {message}

We are in {phase}; the run is halted until you act.
{facts}

Decide, then act with exactly one of:
- mantra_revise_plan — fix this phase's tasks or gate checks (a check that cannot pass on this machine is a plan bug, not a QA failure); the run resumes by itself with the revised phase.
- mantra_resume_run(note) — the plan is right; give the stuck agent a concrete hint and let it try again.
- mantra_ask_user(question) — only if this needs a decision that changes what is being built.
Then summarize in 2-4 lines.",
            ),
        );
    }

    /// Is this agent idle only because it is waiting for an answer to a question it asked up the
    /// chain? (`mantra_ask`; the stage card and `status_text` say so instead of "idle".)
    pub fn waiting_for_answer(&self, a: AgentId) -> bool {
        self.pending_questions.contains_key(&a)
    }

    /// What an agent asked up the chain and is waiting to have answered (`mantra_ask`), if anything.
    pub fn question_of(&self, a: AgentId) -> Option<&str> {
        self.pending_questions.get(&a).map(|s| s.as_str())
    }

    /// Stable machine name of the halt reason (the web protocol's `halted.reason`).
    pub fn halt_reason_str(&self) -> Option<&'static str> {
        self.halt.as_ref().map(|h| match h.reason {
            HaltReason::User => "user",
            HaltReason::Auth => "auth",
            HaltReason::UsageLimit => "usage_limit",
            HaltReason::ProviderRejected => "provider_rejected",
            HaltReason::Environment => "environment",
            HaltReason::GateExhausted => "gate_exhausted",
            HaltReason::AttemptsExhausted => "attempts_exhausted",
            HaltReason::AgentTurnFailed => "agent_turn_failed",
        })
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
            HaltReason::GateExhausted => "the planner has been asked to sort it out · or type feedback · space retries the gate".into(),
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
        if Some(a) == self.manager {
            return Some("manager".into());
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
        if Some(a) == self.manager {
            return Some(self.pattern.flow.manager.clone());
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
        let mut v: Vec<AgentId> = [self.planner, self.manager, self.orchestrator, self.gate_agent, self.finale_agent].into_iter().flatten().collect();
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
            "manager" => return self.manager,
            "orchestrator" | "orch" => return self.orchestrator,
            "gate" | "qa" => return self.gate_agent.or(self.finale_agent),
            "finale" => return self.finale_agent,
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
        if Some(a) == self.manager {
            return "manager".into();
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
            let waiting = w.agent.map(|a| self.pending_questions.contains_key(&a)).unwrap_or(false);
            // The user stopped this one by hand: say so, or the orchestrator reads a live worker
            // that has simply gone quiet and keeps prompting it.
            let stopped = w.agent.and_then(|a| ctx.agent(a)).map(|a| a.stopped_by_user).unwrap_or(false);
            s.push_str(&format!(
                "- {} [{}] {}: {state}, activity: {act}, steps {prog}, tokens {tok}, {el}{}{}{}\n",
                w.task.id,
                w.task.role,
                trunc(&w.task.title, 40),
                if w.paused { " (paused)" } else { "" },
                if waiting { " (WAITING for an answer to its question)" } else { "" },
                if stopped { " (STOPPED by the user)" } else { "" }
            ));
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
        // An agent waiting on an answer (from the rung above it, or — for the planner — from the
        // user) is idle on purpose; the rung above is the one expected to act.
        let asking = |a: AgentId| self.pending_questions.contains_key(&a) || self.question.as_ref().map(|q| q.from == a).unwrap_or(false);
        match &self.stage {
            Stage::Planning => {
                if let Some(p) = self.planner.filter(|p| !asking(*p)) {
                    v.push((p, Expect::Planning));
                }
            }
            Stage::Phase { step, .. } => match step {
                PhaseStep::Orchestrating => {
                    for w in &self.workers {
                        if w.state == WState::Running {
                            if let Some(a) = w.agent.filter(|a| !asking(*a)) {
                                v.push((a, Expect::Working(w.task.id.clone())));
                            }
                        }
                    }
                    if self.orchestrator_needed() {
                        if let Some(o) = self.orchestrator.filter(|o| !asking(*o)) {
                            v.push((o, Expect::Orchestrating));
                        }
                    }
                }
                PhaseStep::Gate { .. } => {
                    if let Some(g) = self.gate_agent.filter(|g| !asking(*g)) {
                        v.push((g, Expect::Gating));
                    }
                }
                PhaseStep::Merging | PhaseStep::Checks { .. } | PhaseStep::Handoff => {}
            },
            Stage::Finale { idx } => {
                if let Some(f) = self.finale_agent.filter(|f| !asking(*f)) {
                    v.push((f, Expect::Finale(*idx)));
                }
            }
            Stage::Setup | Stage::Review | Stage::Done | Stage::Failed(_) => {}
        }
        // The orchestrator (or the manager) passed a question up: now the planner is the one who
        // must act (unless it, in turn, is waiting on the user).
        if let Some(p) = self.planner {
            let asked = [self.orchestrator, self.manager].into_iter().flatten().any(|q| self.pending_questions.contains_key(&q));
            if asked && !asking(p) && !v.iter().any(|(a, _)| *a == p) {
                v.push((p, Expect::Answering));
            }
        }
        // The manager holds a watchdog case about an idle agent: it must act. (An escalated halt
        // is governed by its own deadline in `manager_tick` — the ladder does not run while halted.)
        if let Some(m) = self.manager {
            if self.manager_watchdog.is_some() && !asking(m) && !v.iter().any(|(a, _)| *a == m) {
                v.push((m, Expect::Managing));
            }
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
        // someone below it is waiting for an answer (the manager asks the planner, not it)
        if self.pending_questions.keys().any(|a| Some(*a) != self.orchestrator && Some(*a) != self.manager) {
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
        if self.halted() {
            self.defer(Pending::Phase { idx });
            return;
        }
        let Some(plan) = self.plan.clone() else { return };
        let Some(phase) = plan.phases.get(idx).cloned() else {
            self.start_finale(ctx, 0);
            return;
        };
        self.stage = Stage::Phase { idx, step: PhaseStep::Orchestrating };
        self.phase_started = Instant::now();
        self.last_review = Instant::now();
        self.workers.clear();
        self.checks.clear();
        self.conflicts.clear();
        self.gate_report = None;
        self.handoff_note_done = false;
        self.cleanup_done = false;
        self.log("◆", "saffron", format!("phase {}/{} — {}", idx + 1, plan.phases.len(), phase.name));

        if idx == 0 {
            // The manager watches the whole build: it comes in with the first phase, briefed
            // once, and stays until the run ends.
            self.ensure_manager(ctx);
        }
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
        if let Some(h) = &self.halt {
            // Nothing starts while the run is halted — otherwise a respawned orchestrator
            // happily re-runs a phase the user or the planner is still sorting out.
            return format!("REFUSED: the run is halted ({}) — it must be resumed first (planner: mantra_resume_run; user: space)", trunc(&h.message, 80));
        }
        let running = self.workers.iter().filter(|w| matches!(w.state, WState::Preparing | WState::Running | WState::Retrying(_))).count();
        let prompt = prompt.filter(|p| !p.trim().is_empty()).unwrap_or_else(|| task.prompt.clone());
        let attempt = self.workers.iter().filter(|w| w.task.id == task.id).map(|w| w.attempt).max().unwrap_or(0) + 1;
        let cap = self.max_attempts();
        if attempt > cap {
            let last = self.workers.iter().rev().find(|w| w.task.id == task.id).map(|w| match &w.state {
                WState::Failed(m) => format!(" Last failure: {}.", trunc(m, 160)),
                _ => String::new(),
            }).unwrap_or_default();
            let msg = format!("{} has used all {cap} attempts — not respawning.{last}", task.id);
            self.halt_and_escalate(ctx, HaltReason::AttemptsExhausted, None, msg.clone());
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
        self.pattern.settings.worker_retries + 3 + self.extra_attempts
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
        let req = SpawnReq { name, role_name: rname.clone(), role, cwd: dir.clone(), instructions, tools: tools::worker_tools(), effort, extra_writable: vec![], context_override };
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
        if self.halted() {
            self.defer(Pending::Gate { idx, round });
            return;
        }
        self.stage = Stage::Phase { idx, step: PhaseStep::Gate { round } };
        self.gate_report = None;
        self.last_gate_blocker = None;
        self.last_verify_sig = None;
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
        if matches!(self.stage, Stage::Phase { idx: i, step: PhaseStep::Handoff } if i == idx) {
            return; // already handing off (a second verify result for the same round)
        }
        if self.halted() {
            self.defer(Pending::Handoff { idx });
            return;
        }
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
        if self.halted() {
            self.defer(Pending::Finale { idx: k });
            return;
        }
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
        self.finished_unix = Some(crate::util::unix_secs());
        let ws = self.ws.clone();
        ctx.job(JobTag::Finish, Box::new(move || JobOut::Text(ws.map(|w| git::phase_commit(&w, "mantra: finale").map(|_| "ok".to_string())).unwrap_or(Ok(String::new())))));
        for a in self.all_agents() {
            ctx.stop(a, false);
        }
        let branch = self.ws.as_ref().filter(|w| w.worktree).map(|w| format!(" — branch {} is ready; /land merges it into {}", w.branch, w.base_branch)).unwrap_or_default();
        self.log("✦", "saffron", format!("run complete in {}{branch}", fmt_dur(self.elapsed())));
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
        self.finished_unix = Some(crate::util::unix_secs());
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
                    // The very same failure as last round means the gate agent isn't fixing it —
                    // typically a check that cannot pass on this machine while the agent keeps
                    // reporting "pass". Don't burn the remaining rounds: escalate now.
                    let sig = failing_sig(&self.checks);
                    if self.last_verify_sig.as_deref() == Some(sig.as_str()) {
                        let gate = self.gate_agent;
                        self.halt_and_escalate(ctx, HaltReason::GateExhausted, gate, format!("phase {} gate: the same check(s) fail identically after two gate rounds — {}", phase + 1, trunc(&failing_cmds(&self.checks), 80)));
                        return;
                    }
                    self.last_verify_sig = Some(sig);
                    self.stage = Stage::Phase { idx: phase, step: PhaseStep::Gate { round: round + 1 } };
                    self.gate_report = None;
                    match self.gate_agent {
                        // results are recorded while halted; the next round waits for the resume
                        _ if self.halted() => self.defer(Pending::Gate { idx: phase, round: round + 1 }),
                        Some(g) => {
                            self.mark_edge(g);
                            let s = self.check_summary();
                            ctx.prompt(g, format!("[mantra:gate-round {}] Gate checks still fail after your fixes:\n{s}\n\nFix them, then call mantra_gate_report again. If a check is wrong or cannot pass on this machine, do not report pass — say so with mantra_ask.", round + 1));
                        }
                        // the gate agent is gone (stopped by a revision or a respawn): a fresh one
                        None => self.spawn_gate_at_round(ctx, phase, round + 1),
                    }
                } else {
                    let gate = self.gate_agent;
                    self.halt_and_escalate(ctx, HaltReason::GateExhausted, gate, format!("phase {} gate still failing after {} rounds: {}", phase + 1, round, trunc(&failing_cmds(&self.checks), 80)));
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
        // The user stopped this agent by hand (ctrl+c, or `x`), and that outranks every
        // self-healing path below — the planner nudge, the next gate round, the worker's report to
        // the orchestrator, the transient retry — each of which would put it straight back to work
        // seconds after the stop. The flag clears itself as soon as anyone messages the agent
        // again, so restarting it is always somebody's decision. `status` here is "interrupted",
        // or "failed"/"agent restarting" when the kill raced the turn, so the flag — never the
        // status string — is what decides.
        if ctx.agent(a).map(|x| x.stopped_by_user).unwrap_or(false) {
            self.log("■", "amber", format!("{} stopped by you — type to continue, r to respawn", self.name_of(a)));
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
                // The manager is a supervisor, not a step of the run: its turn failing past the
                // retry cap must never halt the build it watches. Drop it (a fresh one comes with
                // the next event) and hand anything it held to the planner.
                if reason == HaltReason::AgentTurnFailed && Some(a) == self.manager {
                    self.log("◈", "amber", format!("manager: turn failed ({}) — a fresh manager comes with the next event", trunc(&msg, 60)));
                    self.tokens_prev += ctx.agent(a).map(|x| x.tokens_total).unwrap_or(0);
                    ctx.stop(a, true);
                    self.manager = None;
                    self.retry_counts.remove(&a);
                    if let Some(c) = self.manager_watchdog.take() {
                        self.wake_planner_for_idle(ctx, &c.about);
                    }
                    if let Some(e) = self.manager_escalation.take() {
                        self.manager_gave_up(ctx, e, " (it failed itself)");
                    }
                    return;
                }
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
                if reason == HaltReason::AgentTurnFailed {
                    self.halt_turn_failed(ctx, Some(a), detail);
                } else if !self.halted() {
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

        if Some(a) == self.manager {
            if status == "completed" {
                self.manager_watchdog = None;
            }
            // An escalated halt it was handed but did not act on (no resume, no restart, no
            // question up or to the user): one reminder, then the planner gets it.
            let waiting = self.pending_questions.contains_key(&a) || self.question.as_ref().map(|q| q.from == a).unwrap_or(false);
            if self.halted() && status == "completed" && !waiting {
                if let Some(e) = &mut self.manager_escalation {
                    if !e.reminded {
                        e.reminded = true;
                        self.mark_edge(a);
                        ctx.prompt(a, "[mantra:escalation] The run is still halted and nothing changed. Act now: mantra_resume_run(note), mantra_retry / mantra_respawn, mantra_ask the planner, or mantra_ask_user.".into());
                    } else if let Some(e) = self.manager_escalation.take() {
                        self.manager_gave_up(ctx, e, "");
                    }
                    return;
                }
            }
            self.wake_manager(ctx);
            return;
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
                if self.question.is_some() {
                    return; // it asked the user something first; the answer restarts it
                }
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
                if report.is_none() && self.pending_questions.contains_key(&a) {
                    self.log("?", "amber", format!("{} waits for an answer", self.name_of(a)));
                    return;
                }
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
                            self.halt_and_escalate(ctx, HaltReason::GateExhausted, Some(a), format!("phase {} gate stuck on the same blocker twice: {}", idx + 1, trunc(&why, 80)));
                        } else if round < self.pattern.settings.gate_max_rounds {
                            self.stage = Stage::Phase { idx, step: PhaseStep::Gate { round: round + 1 } };
                            self.gate_report = None;
                            self.log("◎", "amber", format!("gate round {} not passed: {}", round, trunc(&why, 60)));
                            if self.halted() {
                                // the report is recorded; the next round waits for the resume
                                self.defer(Pending::Gate { idx, round: round + 1 });
                            } else {
                                self.mark_edge(a);
                                ctx.prompt(a, format!("[mantra:gate-round {}] Keep going: fix what's left so the gate passes, then call mantra_gate_report.", round + 1));
                            }
                        } else {
                            self.halt_and_escalate(ctx, HaltReason::GateExhausted, Some(a), format!("phase {} gate not passed after {round} rounds: {}", idx + 1, trunc(&why, 80)));
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
                let asking = self.pending_questions.contains_key(&a) || self.question.as_ref().map(|q| q.from == a).unwrap_or(false);
                if rep.is_none() && asking {
                    self.log("?", "amber", format!("{} waits for an answer", self.name_of(a)));
                    return;
                }
                if let Some((pass, s)) = rep {
                    self.log(if pass { "✓" } else { "⚠" }, if pass { "green" } else { "amber" }, format!("finale step {} report: {}", idx + 1, trunc(&s, 70)));
                }
                self.start_finale(ctx, idx + 1);
            }
            return;
        }

        if Some(a) == self.planner {
            // An escalation it was handed but did not act on (no revision, no resume, no question
            // to the user): one reminder, then the halt band is the user's.
            if self.escalation_open && self.halted() && self.question.is_none() && status == "completed" {
                self.escalation_open = false;
                self.mark_edge(a);
                ctx.prompt(a, "[mantra:escalation] The run is still halted and nothing changed. Act now with exactly one of mantra_revise_plan, mantra_resume_run(note) or mantra_ask_user(question).".into());
                return;
            }
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
        let waiting = w.agent.map(|a| self.pending_questions.contains_key(&a)).unwrap_or(false);
        match status {
            "completed" => {
                let status_line = report.lines().map(|l| l.trim().to_lowercase()).find(|l| l.starts_with("status:"));
                // It asked the orchestrator something and stopped to wait: that is not "done".
                if waiting && status_line.is_none() {
                    self.log("?", "amber", format!("{tid} waits for an answer"));
                    return;
                }
                let blocked = status_line.map(|l| l.contains("blocked")).unwrap_or(false);
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
        // A hand stop outlives the process: the user stopped the work, not the pipe, so a restart
        // is no reason to tell it to carry on where it left off.
        let stopped = ctx.agent(a).map(|x| x.stopped_by_user).unwrap_or(false);
        if resumed && was_busy && self.is_active() && !stopped {
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
        if !self.is_active() {
            return;
        }
        // The manager's deadlines and inbox run while halted too: it is who resolves halts.
        self.manager_tick(ctx);
        if self.halted() {
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
        self.review_tick(ctx);
        self.watchdog_tick(ctx);
        self.wake_orch(ctx);
    }

    /// Every `settings.review_minutes` while workers build, the idle orchestrator gets a digest of
    /// what each of them has actually been doing (activity, files touched, recent log) and is
    /// asked to check that the parallel work stays coherent — with each other and with the phase
    /// goal — and to steer only where something is off. Events still wake it in between.
    fn review_tick(&mut self, ctx: &mut dyn Ctx) {
        let every = self.pattern.settings.review_minutes;
        if every == 0 || !matches!(self.stage, Stage::Phase { step: PhaseStep::Orchestrating, .. }) {
            return;
        }
        if self.last_review.elapsed() < Duration::from_secs(every * 60) {
            return;
        }
        let Some(o) = self.orchestrator else { return };
        if ctx.agent(o).map(|a| a.busy() || a.thread_id.is_none()).unwrap_or(true) {
            return; // mid-event; try again next tick
        }
        let mut digest = String::new();
        let mut n = 0;
        for w in &self.workers {
            if w.state != WState::Running {
                continue;
            }
            let Some(a) = w.agent.and_then(|a| ctx.agent(a)) else { continue };
            n += 1;
            let mut recent = a.log_text(8);
            crate::util::tail_bytes(&mut recent, 900);
            let files: Vec<&String> = a.files.keys().take(12).collect();
            digest.push_str(&format!(
                "### {} — {} [{}] · {} · {}\nscope: {}\nfiles touched ({}): {}\nrecent:\n{}\n\n",
                w.task.id,
                w.task.title,
                w.task.role,
                a.activity,
                fmt_dur(a.created.elapsed()),
                if w.task.scope.is_empty() { "(not restricted)".to_string() } else { w.task.scope.join(", ") },
                a.files.len(),
                if files.is_empty() { "none yet".to_string() } else { files.iter().map(|f| f.as_str()).collect::<Vec<_>>().join(", ") },
                recent.trim()
            ));
        }
        self.last_review = Instant::now();
        if n == 0 {
            return;
        }
        let goal = self.current_phase().map(|p| p.goal.clone()).unwrap_or_default();
        self.log("◉", "violet", format!("orchestrator review of {n} worker(s)"));
        self.orch_event(
            ctx,
            format!("[mantra:review] Periodic coherence check (every {every} min). Phase goal: {goal}\nRead each worker's recent work below and compare them with each other and with the goal: overlapping or conflicting edits, interfaces or naming drifting apart, scope creep, anyone stuck or looping. Steer with mantra_prompt only where something is actually wrong (at most one message per worker); otherwise just mantra_wait.\n\n{digest}"),
        );
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
                    self.halt_turn_failed(ctx, p, "the planner did not act on a watchdog escalation — respawn it (r)".into());
                    return;
                }
            }
        }
        if let Some(c) = &self.manager_watchdog {
            // The case is closed by a completed manager turn (`on_turn_done`); past the deadline
            // with the manager not even mid-turn, the planner is next, exactly as it would have
            // been without a manager.
            let busy = self.manager.and_then(|m| ctx.agent(m)).map(|a| a.busy() || matches!(a.status, Status::Starting)).unwrap_or(false);
            if Instant::now() >= c.deadline && !busy {
                let about = c.about.clone();
                self.manager_watchdog = None;
                self.log("⏰", "amber", format!("the manager did not act on {about} — waking the planner (watchdog)"));
                self.wake_planner_for_idle(ctx, &about);
            }
        }
        let ws_secs = self.pattern.settings.watchdog_seconds.max(1);
        let es_secs = self.pattern.settings.watchdog_escalate_seconds.max(ws_secs + 1);
        // The stall tripwire in `tick` only walks `self.workers`, and the ladder below only ever
        // sees *idle* agents — so a planner (or orchestrator/gate/finale) that is connected and
        // busy but has emitted nothing for minutes falls through both. That is precisely the state
        // that reads as a crash from the outside, so it gets a journal line of its own (workers
        // keep the tripwire, which also wakes the orchestrator — they must not get both).
        let stall = Duration::from_secs(self.pattern.settings.stall_minutes * 60);
        let mut seen = vec![];
        let mut actions: Vec<(AgentId, Expect, u8, Duration)> = vec![];
        for (a, expect) in self.expected_active() {
            seen.push(a);
            let Some(agent) = ctx.agent(a) else { continue };
            let quiet = agent.last_event.elapsed();
            if quiet < stall {
                // It spoke: the spell is over, and the next one is worth reporting again.
                self.silent_flagged.remove(&a);
            } else if agent.busy() && !agent.stopped_by_user && self.worker_idx(a).is_none() && self.silent_flagged.insert(a) {
                // Workers are deliberately left out: `tick`'s stall tripwire already reports them,
                // and does more with it (the orchestrator is told, so it can steer or respawn).
                let name = self.name_of(a);
                self.log("⏳", "amber", format!("{name}: no output for {} — still connected", fmt_dur(quiet)));
            }
            // A deliberate stop is not idleness: an agent the user stopped by hand is waiting for
            // them, so it is never nudged, respawned or escalated on. Its ladder is dropped too —
            // when the stop is lifted it starts again from the bottom rung.
            if agent.busy() || agent.stopped_by_user {
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
        self.silent_flagged.retain(|a| seen.contains(a));
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
                    Expect::Answering => "answering the question passed up to you (mantra_brief_orchestrator or mantra_prompt, or mantra_ask_user)".into(),
                    Expect::Managing => "resolving the halt / the idle agent handed to you (mantra_resume_run, mantra_retry, mantra_respawn, mantra_prompt, or mantra_ask the planner)".into(),
                };
                self.log("⏰", "amber", format!("{name} idle {secs}s — nudging (watchdog)"));
                self.mark_edge(a);
                ctx.prompt(a, format!("[mantra:watchdog] You are expected to be {what} but have been idle for {secs}s. Continue, or call mantra_wait/mantra_log to say why you are waiting."));
            }
            2 => {
                if Some(a) == self.orchestrator {
                    self.log("⏰", "amber", format!("orchestrator idle {secs}s — respawning (watchdog)"));
                    let _ = self.respawn(ctx, a, Some(format!("[mantra:watchdog] You were idle for {secs}s and were respawned. Pick up the phase.")));
                } else if Some(a) == self.manager {
                    // The manager itself went quiet on a case in its hands: one fresh manager, told
                    // what is open (`respawn_manager` carries it), with a new deadline; a second
                    // silence hands the case to the planner instead of respawning forever.
                    let again = self.manager_watchdog.as_ref().map(|c| c.respawned).unwrap_or(true);
                    if again {
                        if let Some(c) = self.manager_watchdog.take() {
                            self.log("⏰", "amber", format!("manager idle {secs}s again — waking the planner about {} (watchdog)", c.about));
                            self.wake_planner_for_idle(ctx, &c.about);
                        }
                    } else {
                        let es = self.pattern.settings.watchdog_escalate_seconds.max(1);
                        if let Some(c) = self.manager_watchdog.as_mut() {
                            c.respawned = true;
                            c.deadline = Instant::now() + Duration::from_secs(es);
                        }
                        self.log("⏰", "amber", format!("manager idle {secs}s — respawning (watchdog)"));
                        self.respawn_manager(ctx, Some(format!("[mantra:watchdog] The previous manager was idle for {secs}s on this. Deal with it now.")));
                    }
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
                    self.halt_turn_failed(ctx, p, format!("the planner has been unresponsive for {secs}s (watchdog)"));
                    return;
                }
                if Some(a) == self.manager {
                    // Even a respawned manager did nothing: the planner takes over what it held.
                    if let Some(c) = self.manager_watchdog.take() {
                        self.log("⏰", "amber", format!("manager still idle {secs}s — waking the planner about {} (watchdog)", c.about));
                        self.wake_planner_for_idle(ctx, &c.about);
                    }
                    return;
                }
                // With a manager, it gets the case first (once per case); the planner only if it
                // does not react by the deadline (`watchdog_tick`).
                if self.has_manager() && self.manager_watchdog.is_none() {
                    if let Some(m) = self.ensure_manager(ctx) {
                        self.log("⏰", "amber", format!("{name} still idle {secs}s — waking the manager (watchdog)"));
                        self.mark_edge(m);
                        let es = self.pattern.settings.watchdog_escalate_seconds.max(1);
                        ctx.prompt(m, format!("[mantra:watchdog] {name} has been idle for {secs}s and neither a nudge nor the orchestrator got it moving. Find out why (mantra_log {name}, mantra_journal) and get it moving: mantra_prompt, mantra_respawn / mantra_retry, mantra_set_effort, or mantra_brief_orchestrator. If the task itself is the problem, mantra_ask the planner. If you do nothing within {es}s the planner is woken instead."));
                        self.manager_watchdog = Some(ManagerCase { deadline: Instant::now() + Duration::from_secs(es), about: name, respawned: false });
                        return;
                    }
                }
                self.log("⏰", "amber", format!("{name} still idle {secs}s — waking the planner (watchdog)"));
                self.wake_planner_for_idle(ctx, &name);
            }
            _ => {}
        }
    }

    /// Ladder step 3 without (or after) the manager: the planner is told about the idle agent
    /// and given a deadline of its own (`planner_watchdog`), past which the run halts.
    fn wake_planner_for_idle(&mut self, ctx: &mut dyn Ctx, name: &str) {
        let Some(p) = self.planner else { return };
        self.mark_edge(p);
        ctx.prompt(p, format!("[mantra:watchdog] The orchestrator did not act on {name} (idle). Decide: mantra_brief_orchestrator, mantra_revise_plan, mantra_spawn_adhoc, or mantra_pause_agents."));
        self.planner_watchdog = Some((Instant::now(), Instant::now() + Duration::from_secs(self.pattern.settings.watchdog_escalate_seconds.max(1))));
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
        if let Some(q) = self.question.take() {
            // The planner (or the manager) asked; this is the answer — straight back to it,
            // whatever the stage.
            let who = self.name_of(q.from);
            let asked = format!("the {who} asks: {}", q.text);
            self.alerts.retain(|a| *a != asked);
            self.log("›", "rose", format!("you → {who} (answer): {}", trunc(text, 60)));
            self.mark_edge(q.from);
            let next = if Some(q.from) == self.manager {
                "Act on it — resume the run (mantra_resume_run), restart an agent (mantra_retry / mantra_respawn), brief the orchestrator, or hand a plan change to the planner (mantra_ask) — then summarize in 2-4 lines."
            } else {
                "Act on it — brief the orchestrator (mantra_brief_orchestrator), revise the plan (mantra_revise_plan) or resume the run (mantra_resume_run) as needed — then summarize in 2-4 lines."
            };
            ctx.prompt(q.from, format!("[from the user] (answering your question: {})\n{text}\n\n{next}", trunc(&q.text, 200)));
            return;
        }
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

    // ───────────────────────────── manager ─────────────────────────────

    /// The pattern has a manager role (`flow.manager` names a role of kind `manager`).
    pub fn has_manager(&self) -> bool {
        self.pattern.manager_role().is_some()
    }

    /// Spawn the manager agent (no brief yet). `resume`: a saved thread/session id to re-attach to.
    pub(super) fn spawn_manager_with(&mut self, ctx: &mut dyn Ctx, resume: Option<String>) -> AgentId {
        let name = self.pattern.flow.manager.trim().to_string();
        let role = self.role(&name);
        let req = SpawnReq {
            name: "manager".into(),
            role_name: name,
            instructions: format!("{}\n{}", role.instructions, tools::MANAGER_PROTOCOL),
            cwd: self.workspace_dir(),
            tools: tools::manager_tools(),
            effort: None,
            extra_writable: vec![],
            context_override: None,
            role,
        };
        let id = match resume {
            Some(t) => ctx.spawn_resumed(req, t),
            None => ctx.spawn(req),
        };
        self.manager = Some(id);
        id
    }

    /// The manager, spawned and briefed on first need. `None` when the pattern has no manager.
    fn ensure_manager(&mut self, ctx: &mut dyn Ctx) -> Option<AgentId> {
        if !self.has_manager() {
            return None;
        }
        if let Some(m) = self.manager {
            return Some(m);
        }
        let id = self.spawn_manager_with(ctx, None);
        let glyph = self.role(&self.pattern.flow.manager.clone()).glyph;
        self.log(&glyph, "blue", "manager started — it supervises the whole run");
        self.mark_edge(id);
        let brief = self.manager_brief("You manage this run.");
        ctx.prompt(id, brief);
        Some(id)
    }

    /// The manager's opening (or respawn) brief: the goal, the plan's shape, the settings that
    /// decide when Mantra gives up on something, and what will wake it.
    pub(super) fn manager_brief(&self, lead: &str) -> String {
        let s = &self.pattern.settings;
        let plan = match &self.plan {
            Some(p) => {
                let cur = self.phase_idx();
                let phases: Vec<String> = p
                    .phases
                    .iter()
                    .enumerate()
                    .map(|(i, ph)| {
                        let mark = match cur {
                            Some(c) if i < c => "done",
                            Some(c) if i == c => "CURRENT",
                            _ => "pending",
                        };
                        format!("- phase {}: {} ({} task{}) — {mark}", i + 1, ph.name, ph.tasks.len(), if ph.tasks.len() == 1 { "" } else { "s" })
                    })
                    .collect();
                let finale: Vec<&str> = self.pattern.flow.finale.iter().map(|f| f.role.as_str()).collect();
                format!("Plan: {} — {}\n{}\nFinale: {}\n", p.title, trunc(&p.summary, 400), phases.join("\n"), if finale.is_empty() { "none".to_string() } else { finale.join(" → ") })
            }
            None => "Plan: not written yet.\n".into(),
        };
        format!(
            "[mantra:manage] {lead}\nGoal:\n{}\n\n{plan}\nRun settings: up to {} tasks in parallel; each task gets {} attempts in total; a gate gets {} rounds; the watchdog nudges an idle agent after {}s and escalates after {}s; you get a health digest every {}.\n\nNothing needs you yet: call mantra_wait. Mantra wakes you when something does.",
            self.brief,
            s.max_parallel,
            self.max_attempts(),
            s.gate_max_rounds,
            s.watchdog_seconds,
            s.watchdog_escalate_seconds,
            if s.manager_minutes == 0 { "never (escalations only)".to_string() } else { format!("{} min", s.manager_minutes) }
        )
    }

    /// Queue a message for the manager; delivered (with a run overview) as soon as it is idle.
    fn manager_event(&mut self, ctx: &mut dyn Ctx, text: String) {
        if !self.has_manager() {
            return;
        }
        self.manager_inbox.push(text);
        self.wake_manager(ctx);
    }

    /// Deliver the manager's inbox when it can take it. Unlike `wake_orch` this also runs while
    /// the run is halted — the manager is who resolves halts.
    fn wake_manager(&mut self, ctx: &mut dyn Ctx) {
        if self.manager_inbox.is_empty() || !self.is_active() {
            return;
        }
        let Some(m) = self.ensure_manager(ctx) else {
            self.manager_inbox.clear();
            return;
        };
        let Some(a) = ctx.agent(m) else { return };
        if a.busy() || a.thread_id.is_none() || matches!(a.status, Status::Starting) || a.stopped_by_user {
            return;
        }
        let msgs = std::mem::take(&mut self.manager_inbox);
        let overview = self.overview_text(ctx);
        self.mark_edge(m);
        ctx.prompt(m, format!("{}\n\nRun overview:\n{overview}\nDecide what (if anything) to do, then call mantra_wait.", msgs.join("\n\n")));
    }

    /// The whole run on one screen, for the manager: stage and halt, the plan's progress, every
    /// non-worker agent with what it is doing and for how long it has been quiet, the workers
    /// (`status_text`), and the tail of the journal.
    pub(super) fn overview_text(&self, ctx: &dyn Ctx) -> String {
        let mut s = format!("Stage: {} · running for {}\n", super::state::stage_label(&self.stage), fmt_dur(self.elapsed()));
        if let Some(h) = &self.halt {
            s.push_str(&format!("HALTED ({}) for {}: {}\n", reason_label(h.reason), fmt_dur(h.since.elapsed()), h.message));
        }
        if let Some(q) = &self.question {
            s.push_str(&format!("Open question to the user from the {}: {}\n", self.name_of(q.from), trunc(&q.text, 160)));
        }
        if let Some(p) = &self.plan {
            let cur = self.phase_idx();
            for (i, ph) in p.phases.iter().enumerate() {
                let mark = match cur {
                    Some(c) if i < c => "done",
                    Some(c) if i == c => "CURRENT",
                    _ if matches!(self.stage, Stage::Finale { .. } | Stage::Done) => "done",
                    _ => "pending",
                };
                s.push_str(&format!("- phase {}: {} — {mark}\n", i + 1, ph.name));
            }
        }
        s.push_str("Agents:\n");
        let mut listed: Vec<AgentId> = vec![];
        for (label, id) in [("planner", self.planner), ("manager", self.manager), ("orchestrator", self.orchestrator), ("gate", self.gate_agent), ("finale", self.finale_agent)] {
            let Some(a) = id else { continue };
            if listed.contains(&a) {
                continue;
            }
            listed.push(a);
            let Some(ag) = ctx.agent(a) else { continue };
            let state = if ag.stopped_by_user {
                "STOPPED by the user".to_string()
            } else if self.pending_questions.contains_key(&a) {
                "waiting for an answer to its question".to_string()
            } else if ag.busy() {
                format!("busy (last output {} ago)", fmt_dur(ag.last_event.elapsed()))
            } else {
                format!("idle for {}", fmt_dur(ag.last_event.elapsed()))
            };
            s.push_str(&format!("- {label} [{}]: {state}, activity: {}, tokens {}\n", ag.role, trunc(&ag.activity, 60), fmt_tokens(ag.tokens_total)));
        }
        s.push_str("Workers:\n");
        s.push_str(&self.status_text(ctx));
        s.push_str("Recent journal:\n");
        s.push_str(&self.journal_text(14));
        s
    }

    /// The last `n` journal lines, oldest first.
    fn journal_text(&self, n: usize) -> String {
        let lines: Vec<String> = self.pulse.iter().rev().take(n).map(|p| format!("{} {} {}", p.t, p.glyph, p.text)).collect();
        let mut out = String::new();
        for l in lines.iter().rev() {
            out.push_str(l);
            out.push('\n');
        }
        out
    }

    /// The manager's share of `tick`. Runs while halted too: a halt in the manager's hands has a
    /// deadline, past which (and once the manager is idle and not waiting on the planner or the
    /// user) the planner gets it — a silent manager never keeps a run halted.
    fn manager_tick(&mut self, ctx: &mut dyn Ctx) {
        if !self.has_manager() {
            return;
        }
        // The manager's own transient-failure retries (`continue_queue`) must not wait for the
        // halt to lift — the rest of the queue is drained by `tick` once the run is running again.
        if let Some(m) = self.manager {
            let now = Instant::now();
            let due: Vec<String> = self.continue_queue.iter().filter(|(a, t, _)| *a == m && *t <= now).map(|(_, _, msg)| msg.clone()).collect();
            if !due.is_empty() {
                self.continue_queue.retain(|(a, t, _)| !(*a == m && *t <= now));
                self.mark_edge(m);
                for msg in due {
                    ctx.prompt(m, msg);
                }
            }
        }
        if let Some(e) = &self.manager_escalation {
            if Instant::now() >= e.deadline {
                let m = self.manager;
                let busy = m.and_then(|m| ctx.agent(m)).map(|a| a.busy() || matches!(a.status, Status::Starting)).unwrap_or(false);
                let waiting = m.map(|m| self.pending_questions.contains_key(&m) || self.question.as_ref().map(|q| q.from == m).unwrap_or(false)).unwrap_or(false);
                if !self.halted() {
                    self.manager_escalation = None;
                } else if !busy && !waiting {
                    if let Some(e) = self.manager_escalation.take() {
                        self.manager_gave_up(ctx, e, " in time");
                    }
                }
            }
        }
        self.health_tick(ctx);
        self.wake_manager(ctx);
    }

    /// Every `settings.manager_minutes` while building or in the finale, the manager gets the run
    /// overview and is asked whether anything needs unsticking. Skipped (not postponed) while a
    /// halt is open — the escalation already has its attention.
    fn health_tick(&mut self, ctx: &mut dyn Ctx) {
        let every = self.pattern.settings.manager_minutes;
        if every == 0 || self.halted() || !matches!(self.stage, Stage::Phase { .. } | Stage::Finale { .. }) {
            return;
        }
        if self.last_health.elapsed() < Duration::from_secs(every * 60) {
            return;
        }
        if self.manager.and_then(|m| ctx.agent(m)).map(|a| a.busy() || a.thread_id.is_none()).unwrap_or(false) {
            return; // mid-turn: try again next tick (no manager at all: the wake below spawns one)
        }
        self.last_health = Instant::now();
        self.log("◈", "blue", "manager health check");
        self.manager_event(ctx, format!("[mantra:health] Periodic health check (every {every} min). Is the run healthy — is everyone who should be working working, is anyone stuck, looping, failing repeatedly or drifting from the goal? Intervene only where something is actually off (at most one message per agent); otherwise just call mantra_wait."));
    }

    /// The manager restarting an agent during an escalated halt *is* its decision to go on: lift
    /// the halt first (an exhausted task gets its extra attempt), so the retry/respawn is accepted.
    /// Returns whether a halt was lifted.
    fn lift_halt_for_restart(&mut self, ctx: &mut dyn Ctx, by: AgentId) -> bool {
        let Some(h) = &self.halt else { return false };
        if !matches!(h.reason, HaltReason::GateExhausted | HaltReason::AttemptsExhausted | HaltReason::AgentTurnFailed) {
            return false;
        }
        if h.reason == HaltReason::AttemptsExhausted {
            self.extra_attempts += 1;
        }
        self.log("▶", "green", format!("{} lifted the halt to restart an agent", self.name_of(by)));
        self.resume_by(ctx, Some(by));
        true
    }

    fn respawn_manager(&mut self, ctx: &mut dyn Ctx, note: Option<String>) {
        if let Some(old) = self.manager.take() {
            self.tokens_prev += ctx.agent(old).map(|x| x.tokens_total).unwrap_or(0);
            ctx.stop(old, true);
        }
        let id = self.spawn_manager_with(ctx, None);
        self.mark_edge(id);
        let brief = self.manager_brief("[mantra:respawn] You are the new manager of this run (the previous one was respawned).");
        let mut prompt = join_note(note, brief);
        if let Some(e) = &self.manager_escalation {
            prompt.push_str(&format!("\n\nOPEN ESCALATION — the run is halted and it is yours to resolve: {}\nAct with mantra_resume_run(note), mantra_retry/mantra_respawn, or mantra_ask the planner.", e.message));
        }
        if let Some(c) = &self.manager_watchdog {
            prompt.push_str(&format!("\n\nOPEN WATCHDOG CASE — {} has been idle and nobody got it moving. Find out why (mantra_log) and get it moving: mantra_prompt, mantra_respawn / mantra_retry, mantra_set_effort, or mantra_brief_orchestrator.", c.about));
        }
        ctx.prompt(id, prompt);
        self.log("↻", "violet", "manager respawned (watchdog/manual)");
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
        if let Some(wi) = self.worker_idx(a) {
            // `r` on the task that exhausted its attempts *is* the decision to try once more.
            if self.halt.as_ref().map(|h| h.reason == HaltReason::AttemptsExhausted).unwrap_or(false) {
                self.extra_attempts += 1;
                self.resume(ctx);
            }
            // A note goes *on top of* the task prompt — `retry_worker`'s argument replaces it.
            let prompt = note.map(|n| join_note(Some(n), self.workers[wi].prompt.clone()));
            let msg = self.retry_worker(ctx, a, prompt);
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
        if Some(a) == self.manager {
            self.respawn_manager(ctx, note);
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
        if let Some(h) = &self.halt {
            return format!("REFUSED: the run is halted ({}) — resume it first", trunc(&h.message, 80));
        }
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
                    let next = self.apply_revision(ctx, old);
                    return (format!("ACCEPTED. The revision is live; completed phases were kept. What happens next: {next}. Brief the orchestrator if it needs more than the diff Mantra gives it, then summarize."), true);
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
                self.log("‖", "amber", format!("{} paused {}", self.name_of(a), done.join(", ")));
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
                let from = self.name_of(a);
                // Only the planner's brief answers a question the orchestrator passed up to it;
                // the manager briefing the orchestrator meanwhile must not swallow that question.
                let answering = Some(a) == self.planner && self.orchestrator.map(|o| self.pending_questions.remove(&o).is_some()).unwrap_or(false);
                self.log("✦", "saffron", format!("{from} → orchestrator{}: {}", if answering { " (answer)" } else { "" }, trunc(&m, 60)));
                if let Some(o) = self.orchestrator {
                    self.mark_edge(o);
                }
                self.orch_inbox.push(format!("[from the {from}] {m}"));
                ("delivered — the orchestrator acts on it right after your turn".into(), true)
            }
            "mantra_status" => (if Some(a) == self.manager { self.overview_text(ctx) } else { self.status_text(ctx) }, true),
            "mantra_journal" => {
                let n = args.get("lines").and_then(|v| v.as_u64()).unwrap_or(40).clamp(1, 200) as usize;
                let t = self.journal_text(n);
                (if t.is_empty() { "(the journal is empty)".to_string() } else { t }, true)
            }
            "mantra_respawn" => {
                let n = s("agent");
                let note = Some(s("note")).filter(|x| !x.trim().is_empty());
                match self.resolve(&n) {
                    Some(t) if t == a => ("you cannot respawn yourself".into(), false),
                    Some(t) => {
                        if let Some(refusal) = self.worker_done_guard(t) {
                            return (refusal.into(), false);
                        }
                        let lifted = Some(a) == self.manager && self.lift_halt_for_restart(ctx, a);
                        match self.respawn(ctx, t, note) {
                            Ok(()) => (format!("{n} respawned with a fresh thread{}", if lifted { " — the run resumed" } else { "" }), true),
                            Err(e) => (e, false),
                        }
                    }
                    None => (format!("unknown agent '{n}'"), false),
                }
            }
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
                if let Some(h) = &self.halt {
                    return (format!("REFUSED: the run is halted ({}) — nothing starts until it is resumed", trunc(&h.message, 80)), false);
                }
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
                        // An answer to a question the target asked up the chain: it is waiting
                        // for exactly this, so none of the idle-worker guards apply.
                        let answering = self.pending_questions.remove(&t).is_some();
                        if answering {
                            self.mark_edge(t);
                            let from = self.name_of(a);
                            ctx.prompt(t, format!("[from the {from}] (answering your question) {m}"));
                            self.log("›", "rose", format!("{from} → {n} (answer): {}", trunc(&m, 60)));
                            return (format!("answer sent to {n}"), true);
                        }
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
                        if self.worker_idx(t).is_none() {
                            return (format!("'{tid}' is not a worker task"), false);
                        }
                        // The manager restarting a task during an escalated halt is its decision
                        // to go on — lifted only now that the retry is known to be accepted.
                        let lifted = Some(a) == self.manager && self.lift_halt_for_restart(ctx, a);
                        let p = Some(s("prompt")).filter(|x| !x.is_empty());
                        let r = self.retry_worker(ctx, t, p);
                        let ok = !r.starts_with("REFUSED");
                        (if lifted { format!("{r} — the run resumed") } else { r }, ok)
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
            "mantra_ask" => {
                let q = s("question");
                if q.trim().is_empty() {
                    return ("REJECTED: say what you need decided".into(), false);
                }
                self.ask_up(ctx, a, q)
            }
            "mantra_ask_user" => {
                let q = s("question");
                if q.trim().is_empty() {
                    return ("REJECTED: the question is empty".into(), false);
                }
                if let Some(open) = &self.question {
                    return (format!("REJECTED: your earlier question is still open ({}) — wait for that answer", trunc(&open.text, 60)), false);
                }
                let who = self.name_of(a);
                self.question = Some(Question { from: a, text: q.clone(), since: Instant::now() });
                self.log("?", "saffron", format!("{who} asks you: {}", trunc(&q, 70)));
                self.alerts.push(format!("the {who} asks: {q}"));
                ctx.notify(&format!("Mantra: the {who} has a question — {}", trunc(&q, 80)));
                ("the user has been asked; their answer arrives as a [from the user] message. End your turn now — the run continues meanwhile.".into(), true)
            }
            "mantra_resume_run" => {
                let note = s("note");
                let Some(h) = &self.halt else { return ("the run is not halted".into(), false) };
                if !matches!(h.reason, HaltReason::GateExhausted | HaltReason::AttemptsExhausted | HaltReason::AgentTurnFailed) {
                    return (format!("this halt ({}) needs the user, not a resume", h.message), false);
                }
                let (reason, target) = (h.reason, h.agent);
                if reason == HaltReason::AttemptsExhausted {
                    self.extra_attempts += 1;
                }
                let who = self.name_of(a);
                self.log(if Some(a) == self.manager { "◈" } else { "✦" }, "saffron", format!("{who} resumed the run: {}", trunc(&note, 60)));
                if !note.trim().is_empty() {
                    if let Some(t) = target {
                        self.mark_edge(t);
                        ctx.prompt(t, format!("[from the {who}] {note}"));
                    }
                    self.orch_inbox.push(format!("[from the {who}] The run was halted ({}) and is resuming. {note}", trunc(&reason_label(reason), 40)));
                }
                self.resume_by(ctx, Some(a));
                (format!("resumed ({}). End your turn.", if note.trim().is_empty() { "no note" } else { "your note was delivered" }), true)
            }
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

    /// Fold a revised plan into the running phase. Returns a one-line "what happens next" for the
    /// planner. A task that changed after its worker finished is re-opened (its old worker is
    /// cancelled, so `mantra_spawn` accepts it again); if the phase was already past building
    /// (merging, checks, gate) and now has work to do, it goes back to building; and a halted run
    /// (gate/attempts exhausted) resumes — the revision *is* the fix.
    fn apply_revision(&mut self, ctx: &mut dyn Ctx, old: Option<Plan>) -> String {
        let (Some(old), Some(new), Some(cur)) = (old, self.plan.clone(), self.phase_idx()) else { return "completed phases were kept".into() };
        let (Some(op), Some(np)) = (old.phases.get(cur), new.phases.get(cur)) else { return "completed phases were kept".into() };
        let mut notes = vec![];
        let mut needs_work = false;
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
                Some(n) if n.prompt != t.prompt || n.scope != t.scope || n.role != t.role => {
                    let done = self.workers.iter().rev().find(|w| w.task.id == t.id).map(|w| w.state == WState::Done).unwrap_or(false);
                    if done {
                        for w in self.workers.iter_mut().filter(|w| w.task.id == t.id) {
                            w.state = WState::Cancelled;
                        }
                        needs_work = true;
                        notes.push(format!("task {} CHANGED after it was done — spawn it again with mantra_spawn (the workspace keeps its earlier result; the new prompt says what to do now)", t.id));
                    } else {
                        notes.push(format!("task {} CHANGED — steer it with mantra_prompt or respawn with mantra_retry", t.id));
                    }
                }
                _ => {}
            }
        }
        for n in &np.tasks {
            if !op.tasks.iter().any(|t| t.id == n.id) {
                needs_work = true;
                notes.push(format!("task {} was ADDED — spawn it with mantra_spawn", n.id));
            }
        }
        if op.gate != np.gate {
            notes.push("the gate (checks/criteria/focus) CHANGED — Mantra runs the new checks".into());
        }
        if !notes.is_empty() {
            self.log("☰", "saffron", format!("revision: {}", notes.len()));
            self.orch_inbox.push(format!("The planner revised the current phase:\n- {}", notes.join("\n- ")));
        }
        let past_build = matches!(self.stage, Stage::Phase { step: PhaseStep::Merging | PhaseStep::Checks { .. } | PhaseStep::Gate { .. } | PhaseStep::Handoff, .. });
        let next = if needs_work && past_build {
            self.return_to_orchestrating(ctx);
            "the phase goes back to building — the orchestrator spawns the changed/added tasks, then Mantra merges and runs the gate again"
        } else if past_build {
            "the gate runs again with the revised checks"
        } else {
            "the orchestrator has been told what changed"
        };
        let escalated = self.halt.as_ref().map(|h| matches!(h.reason, HaltReason::GateExhausted | HaltReason::AttemptsExhausted | HaltReason::AgentTurnFailed)).unwrap_or(false);
        if escalated {
            let planner = self.planner;
            self.resume_by(ctx, planner);
        } else {
            self.check_phase_done(ctx);
            self.wake_orch(ctx);
        }
        next.into()
    }

    /// Back from merging/checks/gate to building: the gate agent is stopped (it will be spawned
    /// afresh once the re-opened tasks are done), the phase's check state is cleared.
    fn return_to_orchestrating(&mut self, ctx: &mut dyn Ctx) {
        let Some(idx) = self.phase_idx() else { return };
        if let Some(g) = self.gate_agent.take() {
            self.tokens_prev += ctx.agent(g).map(|x| x.tokens_total).unwrap_or(0);
            ctx.stop(g, true);
        }
        self.stage = Stage::Phase { idx, step: PhaseStep::Orchestrating };
        self.checks.clear();
        self.conflicts.clear();
        self.gate_report = None;
        self.last_gate_blocker = None;
        self.last_verify_sig = None;
        if self.orchestrator.is_none() {
            self.spawn_orchestrator(ctx);
        }
        self.log("◆", "saffron", format!("phase {} back to building (plan revised)", idx + 1));
        self.save_state();
    }

    /// `mantra_ask`: a question travels one rung up — worker / gate / finale agent → orchestrator,
    /// orchestrator → planner (the planner, in turn, has `mantra_ask_user`). The asker is marked as
    /// waiting, so its idle turn end is not read as "done", and the rung above is expected to act.
    fn ask_up(&mut self, ctx: &mut dyn Ctx, a: AgentId, q: String) -> (String, bool) {
        let from = self.name_of(a);
        let to_planner = Some(a) == self.orchestrator || Some(a) == self.manager || matches!(self.stage, Stage::Finale { .. }) || self.orchestrator.is_none();
        self.pending_questions.insert(a, q.clone());
        if to_planner {
            let p = match self.planner {
                Some(p) => p,
                None => self.spawn_planner(ctx),
            };
            let answer_with = if Some(a) == self.orchestrator { "mantra_brief_orchestrator" } else { "mantra_prompt" };
            self.log("?", "amber", format!("{from} asks the planner: {}", trunc(&q, 70)));
            let phase = self.phase_idx().map(|i| format!("phase {}", i + 1)).unwrap_or_else(|| "the finale".into());
            let status = self.status_text(ctx);
            self.mark_edge(p);
            ctx.prompt(
                p,
                format!("[mantra:question] The {from} asks:\n{q}\n\nWe are in {phase}. Workers:\n{status}\nYou are the top of the chain of command: decide yourself whenever the answer keeps the end product and the plan's intent, and answer with {answer_with}. Ask the user (mantra_ask_user) only if it changes what is being built, its scope, or is a trade-off only they can make."),
            );
            ("forwarded to the planner — its answer arrives as a message. Call mantra_wait if you have nothing else to do meanwhile.".into(), true)
        } else {
            self.log("?", "amber", format!("{from} asks the orchestrator: {}", trunc(&q, 70)));
            self.orch_event(ctx, format!("QUESTION from {from}: {q}\nIt is waiting for you. Answer with mantra_prompt(\"{from}\", …). If the decision is beyond your brief (it changes the plan, the scope or the product), pass it up with mantra_ask instead."));
            ("forwarded to the orchestrator — the answer arrives as a [from the orchestrator] message. If you cannot continue without it, end your turn now (no STATUS line); you will be woken with the answer.".into(), true)
        }
    }
}

/// One line per failing check, for halt messages.
fn failing_cmds(checks: &[CheckResult]) -> String {
    checks.iter().filter(|c| !c.ok).map(|c| c.cmd.as_str()).collect::<Vec<_>>().join(" ; ")
}

/// Identity of a verify round's failure: which checks failed, how, and the end of their output —
/// equal across two rounds means nothing the gate agent did touched the failure.
fn failing_sig(checks: &[CheckResult]) -> String {
    checks
        .iter()
        .filter(|c| !c.ok)
        .map(|c| {
            let mut o = c.output.trim().to_string();
            crate::util::tail_bytes(&mut o, 240);
            format!("{}|{:?}|{}", c.cmd, c.code, o)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn reason_label(r: HaltReason) -> String {
    match r {
        HaltReason::GateExhausted => "gate exhausted".into(),
        HaltReason::AttemptsExhausted => "attempts exhausted".into(),
        HaltReason::AgentTurnFailed => "an agent's turn failed".into(),
        other => format!("{other:?}").to_lowercase(),
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
        /// Back-date `last_event` while the turn stays live: a busy agent that has gone quiet,
        /// which is the state the ladder ignores and `watchdog_tick` only journals about.
        fn set_silent_secs(&mut self, a: AgentId, secs: u64) {
            if let Some(ag) = self.agents.get_mut(&a) {
                ag.last_event = Instant::now() - Duration::from_secs(secs);
                ag.turn_active = true;
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

    /// The built-in pattern with its manager removed: the chain of command then ends at the
    /// planner, which is what the planner-escalation tests below pin down.
    fn no_manager() -> Pattern {
        let mut p = Pattern::builtin();
        p.flow.manager = String::new();
        p
    }

    fn one_task_phase() -> Plan {
        Plan { phases: vec![Phase { id: "p1".into(), name: "p1".into(), tasks: vec![Task { id: "t1".into(), role: "worker-small".into(), ..Default::default() }], ..Default::default() }], ..Default::default() }
    }

    // WP7.2 watchdog tests ---------------------------------------------------------------------

    #[test]
    fn watchdog_nudges_then_respawns_then_wakes_planner() {
        let mut ctx = TestCtx::new();
        let mut run = Run::new(PathBuf::from("."), no_manager(), "test".into());
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

    /// The case the ladder structurally cannot see: a planner that is *busy* — connected, turn
    /// live — but has emitted nothing for minutes. From the outside that is indistinguishable
    /// from a crash, so it earns one pulse line and nothing else: no nudge, no respawn, no halt.
    #[test]
    fn a_busy_but_silent_agent_is_journalled_once_per_spell() {
        let mut ctx = TestCtx::new();
        let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "test".into());
        let planner = ctx.add_busy();
        ready(&mut ctx, planner);
        run.planner = Some(planner);
        run.stage = Stage::Planning;
        let past_stall = run.pattern.settings.stall_minutes * 60 + 30;
        let lines = |r: &Run| r.pulse.iter().filter(|p| p.text.contains("no output for")).count();

        ctx.set_silent_secs(planner, past_stall);
        run.watchdog_tick(&mut ctx);
        assert_eq!(lines(&run), 1, "a long silent planning turn must say so once: {:?}", run.pulse.iter().map(|p| p.text.clone()).collect::<Vec<_>>());
        assert!(ctx.prompts.is_empty(), "a working agent is never nudged for being quiet: {:?}", ctx.prompts);
        assert!(!run.halted(), "quiet is informational — it must never stop the run");

        // Still silent on the next tick: the feed must not fill up with the same observation.
        run.watchdog_tick(&mut ctx);
        assert_eq!(lines(&run), 1, "one line per spell, not one per tick");

        // It speaks, then goes quiet again — a second spell, reported afresh.
        ctx.agents.get_mut(&planner).unwrap().last_event = Instant::now();
        run.watchdog_tick(&mut ctx);
        ctx.set_silent_secs(planner, past_stall);
        run.watchdog_tick(&mut ctx);
        assert_eq!(lines(&run), 2, "a later silent spell is news again");
        assert!(ctx.prompts.is_empty(), "{:?}", ctx.prompts);
        assert!(!run.halted());
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

    // v0.3 chain of command -------------------------------------------------------------------

    /// An agent with a thread, so `wake_orch`/`review_tick` treat it as reachable.
    fn ready(ctx: &mut TestCtx, a: AgentId) {
        if let Some(ag) = ctx.agents.get_mut(&a) {
            ag.thread_id = Some(format!("thread-{a}"));
            ag.status = Status::Idle;
        }
    }

    #[test]
    fn worker_question_travels_up_and_the_answer_comes_back() {
        let mut ctx = TestCtx::new();
        let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "test".into());
        let orch = ctx.add_idle();
        ready(&mut ctx, orch);
        let worker_agent = ctx.add_busy();
        run.orchestrator = Some(orch);
        run.plan = Some(one_task_phase());
        run.stage = Stage::Phase { idx: 0, step: PhaseStep::Orchestrating };
        run.workers.push(mk_worker("t1", "worker-small", worker_agent, WState::Running));

        let (msg, ok) = run.handle_tool(&mut ctx, worker_agent, "mantra_ask", &json!({"question": "offset or cursor pagination?"}));
        assert!(ok, "{msg}");
        assert!(run.waiting_for_answer(worker_agent));
        let orch_prompt = ctx.prompts.iter().find(|(a, _)| *a == orch).map(|(_, t)| t.clone()).expect("the idle orchestrator is woken with the question");
        assert!(orch_prompt.contains("QUESTION from t1") && orch_prompt.contains("cursor pagination"), "{orch_prompt}");

        // the worker stops to wait: its idle turn end is not "done"
        ctx.agents.get_mut(&worker_agent).unwrap().turn_active = false;
        ctx.agents.get_mut(&worker_agent).unwrap().final_message = Some("Waiting for the decision.".into());
        run.on_turn_done(&mut ctx, worker_agent, "completed", None, None);
        assert_eq!(run.workers[0].state, WState::Running, "a waiting worker stays running");
        assert!(run.status_text(&ctx).contains("WAITING"), "{}", run.status_text(&ctx));
        let expected = run.expected_active();
        assert!(!expected.iter().any(|(a, _)| *a == worker_agent), "a waiting worker is not the watchdog's problem");
        assert!(expected.iter().any(|(a, e)| *a == orch && *e == Expect::Orchestrating), "the orchestrator is expected to answer: {expected:?}");

        // the orchestrator answers — no done-guard, no F3 counting, the question is closed
        ctx.prompts.clear();
        let (msg, ok) = run.handle_tool(&mut ctx, orch, "mantra_prompt", &json!({"agent": "t1", "message": "cursor, page size 50"}));
        assert!(ok && msg.contains("answer"), "{msg}");
        assert!(!run.waiting_for_answer(worker_agent));
        assert!(ctx.prompts.iter().any(|(a, t)| *a == worker_agent && t.contains("(answering your question)") && t.contains("cursor")));

        // and now a real completion is a completion
        ctx.agents.get_mut(&worker_agent).unwrap().final_message = Some("Done.\nSTATUS: done\nSUMMARY: cursor pagination".into());
        run.on_turn_done(&mut ctx, worker_agent, "completed", None, None);
        assert_eq!(run.workers[0].state, WState::Done);
    }

    #[test]
    fn orchestrator_question_goes_to_the_planner_and_a_brief_answers_it() {
        let mut ctx = TestCtx::new();
        let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "test".into());
        let planner = ctx.add_idle();
        let orch = ctx.add_idle();
        ready(&mut ctx, orch);
        run.planner = Some(planner);
        run.orchestrator = Some(orch);
        run.plan = Some(one_task_phase());
        run.stage = Stage::Phase { idx: 0, step: PhaseStep::Orchestrating };

        let (msg, ok) = run.handle_tool(&mut ctx, orch, "mantra_ask", &json!({"question": "may I drop the CSV export?"}));
        assert!(ok, "{msg}");
        let p = ctx.prompts.iter().find(|(a, _)| *a == planner).map(|(_, t)| t.clone()).expect("planner is asked");
        assert!(p.contains("[mantra:question]") && p.contains("mantra_brief_orchestrator") && p.contains("mantra_ask_user"), "{p}");
        assert!(run.expected_active().iter().any(|(a, e)| *a == planner && *e == Expect::Answering), "the planner must answer: {:?}", run.expected_active());

        let (msg, ok) = run.handle_tool(&mut ctx, planner, "mantra_brief_orchestrator", &json!({"message": "Keep the CSV export."}));
        assert!(ok, "{msg}");
        assert!(!run.waiting_for_answer(orch));
        assert!(!run.expected_active().iter().any(|(_, e)| *e == Expect::Answering));
        // briefs are delivered when the planner's turn ends (so one turn can send several)
        run.on_turn_done(&mut ctx, planner, "completed", None, None);
        assert!(ctx.prompts.iter().any(|(a, t)| *a == orch && t.contains("Keep the CSV export")), "the brief reaches the idle orchestrator: {:?}", ctx.prompts);
    }

    #[test]
    fn ask_user_shows_a_question_and_the_reply_answers_it() {
        let mut ctx = TestCtx::new();
        let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "test".into());
        let planner = ctx.add_idle();
        run.planner = Some(planner);
        run.stage = Stage::Planning;

        let (msg, ok) = run.handle_tool(&mut ctx, planner, "mantra_ask_user", &json!({"question": "Postgres or SQLite?"}));
        assert!(ok, "{msg}");
        assert_eq!(run.question.as_ref().map(|q| q.text.as_str()), Some("Postgres or SQLite?"));
        assert!(run.alerts.iter().any(|a| a.contains("Postgres")), "the inbox shows it");
        assert!(run.expected_active().is_empty(), "a planner waiting on the user is not idle");
        // its turn ends without a plan: no nudge, no failure
        run.on_turn_done(&mut ctx, planner, "completed", None, None);
        assert!(!ctx.prompts.iter().any(|(_, t)| t.contains("haven't submitted")));
        assert!(run.is_active());

        run.user_input(&mut ctx, "SQLite, keep it simple");
        assert!(run.question.is_none());
        assert!(run.alerts.is_empty());
        let (a, t) = ctx.prompts.last().expect("the answer is a prompt");
        assert_eq!(*a, planner);
        assert!(t.contains("[from the user] (answering your question") && t.contains("SQLite"), "{t}");
    }

    #[test]
    fn gate_exhaustion_escalates_to_the_planner_and_resume_run_lets_it_continue() {
        let mut ctx = TestCtx::new();
        let mut run = Run::new(PathBuf::from("."), no_manager(), "test".into());
        let planner = ctx.add_idle();
        let gate = ctx.add_idle();
        run.planner = Some(planner);
        run.gate_agent = Some(gate);
        run.plan = Some(one_task_phase());
        run.stage = Stage::Phase { idx: 0, step: PhaseStep::Gate { round: 1 } };
        let blocked = "GATE: fail — the venv is gone and uv cannot recreate it";
        for _ in 0..2 {
            ctx.agents.get_mut(&gate).unwrap().final_message = Some(blocked.into());
            run.on_turn_done(&mut ctx, gate, "completed", None, None);
        }
        assert!(run.halted());
        assert_eq!(run.halt.as_ref().unwrap().reason, HaltReason::GateExhausted);
        let esc = ctx.prompts.iter().find(|(a, t)| *a == planner && t.contains("[mantra:escalation]")).map(|(_, t)| t.clone()).expect("the planner is handed the halt");
        assert!(esc.contains("mantra_revise_plan") && esc.contains("mantra_resume_run") && esc.contains("mantra_ask_user"), "{esc}");
        assert!(run.halt_hint().contains("planner"));

        // the planner: plan is right, here's a hint — the run resumes, the gate gets the note,
        // and the planner itself is not told to "continue where you left off"
        ctx.prompts.clear();
        let (msg, ok) = run.handle_tool(&mut ctx, planner, "mantra_resume_run", &json!({"note": "the venv is at .venv — do not recreate it"}));
        assert!(ok, "{msg}");
        assert!(!run.halted());
        assert!(ctx.prompts.iter().any(|(a, t)| *a == gate && t.contains("[from the planner]") && t.contains(".venv")));
        assert!(ctx.prompts.iter().any(|(a, t)| *a == gate && t.contains("[mantra:gate-round 1]")), "no checks in this phase: the gate agent itself continues: {:?}", ctx.prompts);
        assert!(!ctx.prompts.iter().any(|(a, t)| *a == planner && t.contains("[mantra:resume]")));
        assert!(matches!(run.stage, Stage::Phase { step: PhaseStep::Gate { round: 1 }, .. }), "{:?}", run.stage);
    }

    #[test]
    fn identical_verify_failures_escalate_before_the_rounds_run_out() {
        let mut ctx = TestCtx::new();
        let mut run = Run::new(PathBuf::from("."), no_manager(), "test".into());
        let planner = ctx.add_idle();
        let gate = ctx.add_idle();
        run.planner = Some(planner);
        run.gate_agent = Some(gate);
        run.plan = Some(one_task_phase());
        run.stage = Stage::Phase { idx: 0, step: PhaseStep::Gate { round: 1 } };
        let fail = || vec![CheckResult { cmd: "uv venv .venv".into(), ok: false, code: Some(2), output: "error: Permission denied".into(), secs: 0 }];
        run.on_job(&mut ctx, JobTag::Checks { phase: 0, round: 1 }, JobOut::Checks(fail()));
        assert!(!run.halted());
        assert!(ctx.prompts.iter().any(|(a, t)| *a == gate && t.contains("[mantra:gate-round 2]") && t.contains("Permission denied") && t.contains("mantra_ask")), "round 2 with the output and the escape hatch: {:?}", ctx.prompts);
        run.on_job(&mut ctx, JobTag::Checks { phase: 0, round: 2 }, JobOut::Checks(fail()));
        assert!(run.halted(), "the identical failure twice means nobody is fixing it");
        assert!(run.halt.as_ref().unwrap().message.contains("identically"));
        assert!(ctx.prompts.iter().any(|(a, t)| *a == planner && t.contains("[mantra:escalation]") && t.contains("uv venv")));
    }

    #[test]
    fn revising_the_plan_while_halted_reopens_changed_tasks_and_resumes() {
        let mut ctx = TestCtx::new();
        let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "test".into());
        let planner = ctx.add_idle();
        let orch = ctx.add_idle();
        ready(&mut ctx, orch);
        let gate = ctx.add_idle();
        let worker_agent = ctx.add_idle();
        run.planner = Some(planner);
        run.orchestrator = Some(orch);
        run.gate_agent = Some(gate);
        let mut plan = one_task_phase();
        plan.phases[0].tasks[0].prompt = "build it, then delete the venv".into();
        run.plan = Some(plan.clone());
        run.plan_version = 1;
        run.stage = Stage::Phase { idx: 0, step: PhaseStep::Gate { round: 3 } };
        run.workers.push(mk_worker("t1", "worker-small", worker_agent, WState::Done));
        run.halt(&mut ctx, HaltReason::GateExhausted, Some(gate), "phase 1 gate not passed after 3 rounds".into());

        // while halted nothing may start
        let (msg, ok) = run.handle_tool(&mut ctx, orch, "mantra_spawn", &json!({"task_id": "t1"}));
        assert!(!ok && msg.contains("halted"), "{msg}");

        // the planner fixes the task's prompt
        plan.phases[0].tasks[0].prompt = "build it; leave the venv in place".into();
        ctx.prompts.clear();
        let (msg, ok) = run.handle_tool(&mut ctx, planner, "mantra_revise_plan", &json!({"plan": serde_json::to_value(&plan).unwrap(), "reason": "keep the venv"}));
        assert!(ok, "{msg}");
        assert!(msg.contains("back to building"), "{msg}");
        assert!(!run.halted(), "the revision is the fix — the run resumes by itself");
        assert!(matches!(run.stage, Stage::Phase { idx: 0, step: PhaseStep::Orchestrating }), "{:?}", run.stage);
        assert_eq!(run.workers[0].state, WState::Cancelled, "the done worker of a changed task is re-opened");
        assert!(run.gate_agent.is_none(), "the gate is spawned afresh once the phase is rebuilt");
        let note = ctx.prompts.iter().find(|(a, _)| *a == orch).map(|(_, t)| t.clone()).expect("the orchestrator is told");
        assert!(note.contains("t1 CHANGED after it was done") && note.contains("mantra_spawn"), "{note}");
        // and spawning the re-opened task is accepted again
        let (msg, ok) = run.handle_tool(&mut ctx, orch, "mantra_spawn", &json!({"task_id": "t1"}));
        assert!(ok, "{msg}");
        assert_eq!(run.workers.len(), 2);
    }

    #[test]
    fn review_tick_hands_the_orchestrator_a_digest_of_running_workers() {
        let mut ctx = TestCtx::new();
        let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "test".into());
        let orch = ctx.add_idle();
        ready(&mut ctx, orch);
        let worker_agent = ctx.add_busy();
        ctx.agents.get_mut(&worker_agent).unwrap().activity = "$ cargo test".into();
        run.orchestrator = Some(orch);
        run.plan = Some(one_task_phase());
        run.stage = Stage::Phase { idx: 0, step: PhaseStep::Orchestrating };
        run.workers.push(mk_worker("t1", "worker-small", worker_agent, WState::Running));
        run.pattern.settings.review_minutes = 1;

        run.review_tick(&mut ctx);
        assert!(ctx.prompts.is_empty(), "not due yet");
        run.last_review = Instant::now() - Duration::from_secs(61);
        run.review_tick(&mut ctx);
        let (a, t) = ctx.prompts.last().expect("the review wakes the orchestrator");
        assert_eq!(*a, orch);
        assert!(t.contains("[mantra:review]") && t.contains("### t1") && t.contains("$ cargo test"), "{t}");
        assert!(run.last_review.elapsed().as_secs() < 5, "the clock restarts");
        run.pattern.settings.review_minutes = 0;
        run.last_review = Instant::now() - Duration::from_secs(600);
        ctx.prompts.clear();
        run.review_tick(&mut ctx);
        assert!(ctx.prompts.is_empty(), "0 turns the review off");
    }

    // A user's hand stop (ctrl+c / `x`) --------------------------------------------------------

    /// The bug this guards: a planner stopped by hand ends its turn "interrupted" with no plan,
    /// which used to walk straight into the planner-nudge branch and prompt it again — the user's
    /// stop undone a second later.
    #[test]
    fn hand_stopped_planner_is_not_nudged_back_to_work() {
        let mut ctx = TestCtx::new();
        let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "test".into());
        let planner = ctx.add_busy();
        run.planner = Some(planner);
        run.stage = Stage::Planning;
        // exactly what `App::interrupt_by_user` leaves behind: the flag set, the turn killed
        ctx.agents.get_mut(&planner).unwrap().stopped_by_user = true;
        ctx.agents.get_mut(&planner).unwrap().turn_active = false;

        run.on_turn_done(&mut ctx, planner, "interrupted", None, None);

        assert!(!ctx.prompts.iter().any(|(_, t)| t.contains("haven't submitted")), "a hand-stopped planner must not be nudged: {:?}", ctx.prompts);
        assert!(ctx.prompts.is_empty(), "nothing at all may be sent to it: {:?}", ctx.prompts);
        assert_eq!(run.planner_nudges, 0);
        assert!(run.is_active(), "the run keeps going — only this agent stopped");
        assert!(run.pulse.iter().any(|p| p.text.contains("stopped by you")), "the journal says why nothing happened: {:?}", run.pulse.iter().map(|p| p.text.clone()).collect::<Vec<_>>());
    }

    /// The other restart path: `Cmd::Interrupt` can race the turn, so the stop can surface as a
    /// failed turn ("agent restarting") instead of an interrupted one — the flag, not the status
    /// string, must be what stops the retry ladder.
    #[test]
    fn hand_stop_also_swallows_a_racing_transient_failure() {
        let mut ctx = TestCtx::new();
        let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "test".into());
        let planner = ctx.add_idle();
        run.planner = Some(planner);
        run.stage = Stage::Planning;
        ctx.agents.get_mut(&planner).unwrap().stopped_by_user = true;

        run.on_turn_done(&mut ctx, planner, "failed", Some("agent restarting".into()), Some(ErrKind::Transient));

        assert!(ctx.prompts.is_empty(), "no backoff retry for a hand-stopped agent: {:?}", ctx.prompts);
        assert!(run.continue_queue.is_empty(), "nothing queued to re-prompt it later");
        assert!(!run.halted());
    }

    #[test]
    fn watchdog_leaves_a_hand_stopped_planner_alone() {
        let mut ctx = TestCtx::new();
        let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "test".into());
        let planner = ctx.add_idle();
        run.planner = Some(planner);
        run.stage = Stage::Planning;
        ctx.set_idle_secs(planner, 600); // well past watchdog_seconds and both escalations
        ctx.agents.get_mut(&planner).unwrap().stopped_by_user = true;

        run.watchdog_tick(&mut ctx);
        assert!(ctx.prompts.is_empty(), "a deliberate stop is not idleness: {:?}", ctx.prompts);
        assert!(!run.halted(), "and it must never escalate into a halt");

        // the user types to it again (`prompt_agent` clears the flag): the ladder starts over
        ctx.agents.get_mut(&planner).unwrap().stopped_by_user = false;
        ctx.set_idle_secs(planner, 100);
        run.watchdog_tick(&mut ctx);
        assert!(ctx.prompts.iter().any(|(a, t)| *a == planner && t.contains("[mantra:watchdog]")), "the watchdog resumes its job once the stop is lifted: {:?}", ctx.prompts);
    }

    #[test]
    fn hand_stopped_worker_is_not_reported_to_the_orchestrator() {
        let mut ctx = TestCtx::new();
        let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "test".into());
        let orch = ctx.add_idle();
        ready(&mut ctx, orch);
        let worker_agent = ctx.add_busy();
        run.orchestrator = Some(orch);
        run.plan = Some(one_task_phase());
        run.stage = Stage::Phase { idx: 0, step: PhaseStep::Orchestrating };
        run.workers.push(mk_worker("t1", "worker-small", worker_agent, WState::Running));
        ctx.agents.get_mut(&worker_agent).unwrap().stopped_by_user = true;
        ctx.agents.get_mut(&worker_agent).unwrap().turn_active = false;

        run.on_turn_done(&mut ctx, worker_agent, "interrupted", None, None);

        assert!(ctx.prompts.is_empty(), "the orchestrator must not be told to prompt/retry it: {:?}", ctx.prompts);
        assert_eq!(run.workers[0].state, WState::Running, "the task is still this worker's — it just stopped");
        assert!(run.status_text(&ctx).contains("(STOPPED by the user)"), "{}", run.status_text(&ctx));
    }

    #[test]
    fn messaging_a_hand_stopped_worker_again_lifts_the_stop() {
        let mut ctx = TestCtx::new();
        let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "test".into());
        let orch = ctx.add_idle();
        ready(&mut ctx, orch);
        let worker_agent = ctx.add_busy();
        run.orchestrator = Some(orch);
        run.plan = Some(one_task_phase());
        run.stage = Stage::Phase { idx: 0, step: PhaseStep::Orchestrating };
        run.workers.push(mk_worker("t1", "worker-small", worker_agent, WState::Running));
        ctx.agents.get_mut(&worker_agent).unwrap().stopped_by_user = true;
        ctx.agents.get_mut(&worker_agent).unwrap().turn_active = false;
        run.on_turn_done(&mut ctx, worker_agent, "interrupted", None, None);
        assert!(ctx.prompts.is_empty());

        // someone messages it again — `app::prompt_agent` clears the flag — and a later interrupted
        // turn is handled normally again.
        ctx.agents.get_mut(&worker_agent).unwrap().stopped_by_user = false;
        run.on_turn_done(&mut ctx, worker_agent, "interrupted", None, None);
        assert!(ctx.prompts.iter().any(|(a, t)| *a == orch && t.contains("was interrupted")), "normal handling is back: {:?}", ctx.prompts);
        assert!(!run.status_text(&ctx).contains("STOPPED"));
    }

    /// A hand-stopped agent whose process happens to restart must not be told to carry on.
    #[test]
    fn hand_stop_survives_a_process_restart() {
        let mut ctx = TestCtx::new();
        let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "test".into());
        let planner = ctx.add_idle();
        run.planner = Some(planner);
        run.stage = Stage::Planning;
        ctx.agents.get_mut(&planner).unwrap().stopped_by_user = true;

        run.on_ready(&mut ctx, planner, true, true);
        assert!(!ctx.prompts.iter().any(|(_, t)| t.contains("[mantra:resume]")), "{:?}", ctx.prompts);
        assert!(run.agent_meta.contains_key(&planner), "the bookkeeping still happens");

        ctx.agents.get_mut(&planner).unwrap().stopped_by_user = false;
        run.on_ready(&mut ctx, planner, true, true);
        assert!(ctx.prompts.iter().any(|(a, t)| *a == planner && t.contains("[mantra:resume]")), "a plain crash+resume still gets its prompt: {:?}", ctx.prompts);
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

    // Manager tests (v0.4) ---------------------------------------------------------------------

    /// A run halted on gate exhaustion, with the built-in pattern (which has a manager).
    fn halted_gate_run(ctx: &mut TestCtx) -> (Run, AgentId, AgentId) {
        let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "test".into());
        let planner = ctx.add_idle();
        let gate = ctx.add_idle();
        run.planner = Some(planner);
        run.gate_agent = Some(gate);
        run.plan = Some(one_task_phase());
        run.stage = Stage::Phase { idx: 0, step: PhaseStep::Gate { round: 1 } };
        let blocked = "GATE: fail — the venv is gone";
        for _ in 0..2 {
            ctx.agents.get_mut(&gate).unwrap().final_message = Some(blocked.into());
            run.on_turn_done(ctx, gate, "completed", None, None);
        }
        assert!(run.halted());
        (run, planner, gate)
    }

    #[test]
    fn escalation_goes_to_the_manager_first_and_to_the_planner_when_it_does_nothing() {
        let mut ctx = TestCtx::new();
        let (mut run, planner, _gate) = halted_gate_run(&mut ctx);
        let manager = run.manager.expect("the halt spawns the manager");
        let esc = ctx.prompts.iter().find(|(a, t)| *a == manager && t.contains("[mantra:escalation]")).map(|(_, t)| t.clone()).expect("the manager is handed the halt");
        assert!(esc.contains("mantra_resume_run") && esc.contains("mantra_retry") && esc.contains("mantra_respawn") && esc.contains("mantra_ask"), "{esc}");
        assert!(esc.contains("first in line"), "{esc}");
        assert!(!ctx.prompts.iter().any(|(a, t)| *a == planner && t.contains("[mantra:escalation]")), "the planner is not bothered while the manager has it");
        assert!(run.manager_escalation.is_some(), "the halt is in the manager's hands, with a deadline");

        // The manager ends its turn without acting: one reminder…
        ctx.prompts.clear();
        run.on_turn_done(&mut ctx, manager, "completed", None, None);
        assert!(ctx.prompts.iter().any(|(a, t)| *a == manager && t.contains("still halted")), "{:?}", ctx.prompts);
        assert!(!ctx.prompts.iter().any(|(a, _)| *a == planner));
        // …then the planner gets it, with the same message.
        ctx.prompts.clear();
        run.on_turn_done(&mut ctx, manager, "completed", None, None);
        let esc = ctx.prompts.iter().find(|(a, t)| *a == planner && t.contains("[mantra:escalation]")).map(|(_, t)| t.clone()).expect("the planner is next");
        assert!(esc.contains("mantra_revise_plan") && esc.contains("gate stuck"), "{esc}");
        assert!(run.manager_escalation.is_none());
        assert!(run.halted(), "still halted: now it is the planner's");
    }

    #[test]
    fn manager_escalation_deadline_hands_the_halt_to_the_planner() {
        let mut ctx = TestCtx::new();
        let (mut run, planner, _gate) = halted_gate_run(&mut ctx);
        let manager = run.manager.unwrap();
        ready(&mut ctx, manager);
        run.manager_escalation.as_mut().unwrap().deadline = Instant::now() - Duration::from_secs(1);
        ctx.prompts.clear();
        run.manager_tick(&mut ctx);
        assert!(ctx.prompts.iter().any(|(a, t)| *a == planner && t.contains("[mantra:escalation]")), "{:?}", ctx.prompts);
        assert!(run.manager_escalation.is_none());
        // a manager that is waiting on the planner (it asked) is not overtaken by the deadline
        let (mut run2, planner2, _) = halted_gate_run(&mut ctx);
        let m2 = run2.manager.unwrap();
        ready(&mut ctx, m2);
        let (_, ok) = run2.handle_tool(&mut ctx, m2, "mantra_ask", &json!({"question": "the check needs a venv — amend the gate?"}));
        assert!(ok);
        assert!(ctx.prompts.iter().any(|(a, t)| *a == planner2 && t.contains("[mantra:question]") && t.contains("mantra_prompt")));
        ctx.prompts.clear();
        run2.manager_escalation.as_mut().unwrap().deadline = Instant::now() - Duration::from_secs(1);
        run2.manager_tick(&mut ctx);
        assert!(!ctx.prompts.iter().any(|(a, t)| *a == planner2 && t.contains("[mantra:escalation]")), "the planner already has the question: {:?}", ctx.prompts);
        // the planner answers the manager by name, and the answer comes back marked as such
        let (_, ok) = run2.handle_tool(&mut ctx, planner2, "mantra_prompt", &json!({"agent": "manager", "message": "yes — I am revising the gate"}));
        assert!(ok);
        assert!(ctx.prompts.iter().any(|(a, t)| *a == m2 && t.contains("[from the planner] (answering your question)")));
        assert!(!run2.waiting_for_answer(m2));
    }

    #[test]
    fn manager_resume_run_lifts_the_halt_and_the_note_reaches_the_stuck_agent() {
        let mut ctx = TestCtx::new();
        let (mut run, planner, gate) = halted_gate_run(&mut ctx);
        let manager = run.manager.unwrap();
        ctx.prompts.clear();
        let (msg, ok) = run.handle_tool(&mut ctx, manager, "mantra_resume_run", &json!({"note": "the venv is at .venv — do not recreate it"}));
        assert!(ok, "{msg}");
        assert!(!run.halted());
        assert!(run.manager_escalation.is_none());
        assert!(ctx.prompts.iter().any(|(a, t)| *a == gate && t.contains("[from the manager]") && t.contains(".venv")), "{:?}", ctx.prompts);
        assert!(!ctx.prompts.iter().any(|(a, t)| *a == manager && t.contains("[mantra:resume]")), "the manager itself is not told to continue");
        assert!(!ctx.prompts.iter().any(|(a, _)| *a == planner));
        // and a later idle turn of the manager is nothing special any more
        ctx.prompts.clear();
        run.on_turn_done(&mut ctx, manager, "completed", None, None);
        assert!(ctx.prompts.is_empty(), "{:?}", ctx.prompts);
    }

    #[test]
    fn manager_retry_during_an_attempts_exhausted_halt_resumes_and_respawns_the_task() {
        let mut ctx = TestCtx::new();
        let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "test".into());
        let planner = ctx.add_idle();
        let orch = ctx.add_idle();
        let w = ctx.add_idle();
        run.planner = Some(planner);
        run.orchestrator = Some(orch);
        run.plan = Some(one_task_phase());
        run.stage = Stage::Phase { idx: 0, step: PhaseStep::Orchestrating };
        let mut wk = mk_worker("t1", "worker-small", w, WState::Failed("tests never pass".into()));
        wk.attempt = 5;
        run.workers.push(wk);
        run.halt_and_escalate(&mut ctx, HaltReason::AttemptsExhausted, Some(w), "t1 has used all 5 attempts — not respawning.".into());
        let manager = run.manager.expect("spawned for the escalation");
        assert!(ctx.prompts.iter().any(|(a, t)| *a == manager && t.contains("[mantra:escalation]") && t.contains("t1 has used all")));
        let before = run.max_attempts();
        ctx.prompts.clear();
        let (msg, ok) = run.handle_tool(&mut ctx, manager, "mantra_retry", &json!({"task_id": "t1", "prompt": "run the tests with -x and fix the first failure only"}));
        assert!(ok, "{msg}");
        assert!(msg.contains("the run resumed"), "{msg}");
        assert!(!run.halted());
        assert_eq!(run.max_attempts(), before + 1, "the retry is the extra attempt");
        let newest = run.workers.iter().rev().find(|x| x.task.id == "t1").unwrap();
        assert_eq!(newest.attempt, 6);
        assert!(matches!(newest.state, WState::Preparing | WState::Queued), "{:?}", newest.state);
        assert_eq!(newest.prompt, "run the tests with -x and fix the first failure only");
    }

    #[test]
    fn manager_respawn_restarts_any_agent_and_never_itself() {
        let mut ctx = TestCtx::new();
        let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "test".into());
        let planner = ctx.add_idle();
        let orch = ctx.add_idle();
        run.planner = Some(planner);
        run.orchestrator = Some(orch);
        run.plan = Some(one_task_phase());
        run.stage = Stage::Phase { idx: 0, step: PhaseStep::Orchestrating };
        let manager = run.ensure_manager(&mut ctx).unwrap();
        assert!(ctx.prompts.iter().any(|(a, t)| *a == manager && t.contains("[mantra:manage]") && t.contains("mantra_wait")));
        let (msg, ok) = run.handle_tool(&mut ctx, manager, "mantra_respawn", &json!({"agent": "orchestrator", "note": "you kept prompting a finished worker"}));
        assert!(ok, "{msg}");
        let new_orch = run.orchestrator.unwrap();
        assert_ne!(new_orch, orch);
        assert!(ctx.prompts.iter().any(|(a, t)| *a == new_orch && t.contains("finished worker") && t.contains("[mantra:respawn]")));
        let (msg, ok) = run.handle_tool(&mut ctx, manager, "mantra_respawn", &json!({"agent": "manager"}));
        assert!(!ok, "{msg}");
        // the user can respawn the manager, and the fresh one is briefed as a manager
        run.respawn(&mut ctx, manager, None).unwrap();
        let m2 = run.manager.unwrap();
        assert_ne!(m2, manager);
        assert!(ctx.prompts.iter().any(|(a, t)| *a == m2 && t.contains("new manager")));
        assert_eq!(run.name_of(m2), "manager");
        assert_eq!(run.role_name_of(m2).as_deref(), Some("manager"));
    }

    #[test]
    fn watchdog_rung_three_wakes_the_manager_before_the_planner() {
        let mut ctx = TestCtx::new();
        let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "test".into());
        let planner = ctx.add_idle();
        let orch = ctx.add_idle();
        run.planner = Some(planner);
        run.orchestrator = Some(orch);
        run.plan = Some(one_task_phase());
        run.stage = Stage::Phase { idx: 0, step: PhaseStep::Orchestrating };
        ctx.set_idle_secs(orch, 90);
        run.watchdog_tick(&mut ctx);
        ctx.set_idle_secs(orch, 240);
        run.watchdog_tick(&mut ctx);
        let new_orch = run.orchestrator.unwrap();
        ctx.prompts.clear();
        ctx.set_idle_secs(new_orch, 480);
        run.watchdog_tick(&mut ctx);
        let manager = run.manager.expect("rung 3 brings in the manager");
        assert!(ctx.prompts.iter().any(|(a, t)| *a == manager && t.contains("[mantra:watchdog]") && t.contains("orchestrator")), "{:?}", ctx.prompts);
        assert!(!ctx.prompts.iter().any(|(a, t)| *a == planner && t.contains("did not act")), "the planner waits its turn");
        assert!(run.expected_active().iter().any(|(a, e)| *a == manager && *e == Expect::Managing));
        // the manager finishes no turn about it by its deadline: the planner, as before
        ready(&mut ctx, manager);
        ctx.set_idle_secs(manager, 30);
        run.manager_watchdog.as_mut().unwrap().deadline = Instant::now() - Duration::from_secs(1);
        ctx.prompts.clear();
        run.watchdog_tick(&mut ctx);
        assert!(ctx.prompts.iter().any(|(a, t)| *a == planner && t.contains("did not act")), "{:?}", ctx.prompts);
        assert!(run.manager_watchdog.is_none());
    }

    #[test]
    fn a_completed_manager_turn_closes_its_watchdog_case_and_a_busy_one_is_waited_for() {
        let mut ctx = TestCtx::new();
        let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "test".into());
        let planner = ctx.add_idle();
        let orch = ctx.add_idle();
        run.planner = Some(planner);
        run.orchestrator = Some(orch);
        run.plan = Some(one_task_phase());
        run.stage = Stage::Phase { idx: 0, step: PhaseStep::Orchestrating };
        let manager = run.ensure_manager(&mut ctx).unwrap();
        ready(&mut ctx, manager);
        run.manager_watchdog = Some(ManagerCase { deadline: Instant::now() - Duration::from_secs(1), about: "orchestrator".into(), respawned: false });
        // busy past the deadline: not overtaken
        ctx.agents.get_mut(&manager).unwrap().turn_active = true;
        run.watchdog_tick(&mut ctx);
        assert!(run.manager_watchdog.is_some());
        assert!(!ctx.prompts.iter().any(|(a, _)| *a == planner));
        // its turn completes: the case is closed, the planner never hears of it
        run.on_turn_done(&mut ctx, manager, "completed", None, None);
        assert!(run.manager_watchdog.is_none());
        run.watchdog_tick(&mut ctx);
        assert!(!ctx.prompts.iter().any(|(a, _)| *a == planner));
    }

    #[test]
    fn a_silent_manager_is_respawned_once_for_a_case_then_the_planner_gets_it() {
        let mut ctx = TestCtx::new();
        let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "test".into());
        let planner = ctx.add_idle();
        let orch = ctx.add_busy();
        run.planner = Some(planner);
        run.orchestrator = Some(orch);
        run.plan = Some(one_task_phase());
        run.stage = Stage::Phase { idx: 0, step: PhaseStep::Orchestrating };
        let manager = run.ensure_manager(&mut ctx).unwrap();
        ready(&mut ctx, manager);
        run.manager_watchdog = Some(ManagerCase { deadline: Instant::now() + Duration::from_secs(600), about: "t1".into(), respawned: false });
        ctx.set_idle_secs(manager, 240);
        ctx.prompts.clear();
        run.watchdog_tick(&mut ctx);
        let m2 = run.manager.unwrap();
        assert_ne!(m2, manager, "rung 2: a fresh manager");
        assert!(ctx.prompts.iter().any(|(a, t)| *a == m2 && t.contains("OPEN WATCHDOG CASE") && t.contains("t1")), "{:?}", ctx.prompts);
        assert!(run.manager_watchdog.as_ref().map(|c| c.respawned).unwrap_or(false));
        // the fresh one is just as silent: the planner, not a third manager
        ready(&mut ctx, m2);
        ctx.set_idle_secs(m2, 240);
        ctx.prompts.clear();
        run.watchdog_tick(&mut ctx);
        assert_eq!(run.manager, Some(m2), "no respawn loop");
        assert!(ctx.prompts.iter().any(|(a, t)| *a == planner && t.contains("did not act") && t.contains("t1")), "{:?}", ctx.prompts);
        assert!(run.manager_watchdog.is_none());
    }

    #[test]
    fn a_dropped_manager_hands_its_watchdog_case_to_the_planner() {
        let mut ctx = TestCtx::new();
        let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "test".into());
        let planner = ctx.add_idle();
        let orch = ctx.add_idle();
        run.planner = Some(planner);
        run.orchestrator = Some(orch);
        run.plan = Some(one_task_phase());
        run.stage = Stage::Phase { idx: 0, step: PhaseStep::Orchestrating };
        let manager = run.ensure_manager(&mut ctx).unwrap();
        run.manager_watchdog = Some(ManagerCase { deadline: Instant::now() + Duration::from_secs(600), about: "orchestrator".into(), respawned: false });
        ctx.prompts.clear();
        run.on_turn_done(&mut ctx, manager, "failed", Some("stream closed".into()), Some(ErrKind::Other));
        assert!(run.manager.is_none());
        assert!(run.manager_watchdog.is_none());
        assert!(ctx.prompts.iter().any(|(a, t)| *a == planner && t.contains("did not act") && t.contains("orchestrator")), "{:?}", ctx.prompts);
        assert!(!run.halted());
        // a health digest brings a fresh manager back
        run.last_health = Instant::now() - Duration::from_secs(6 * 60);
        run.health_tick(&mut ctx);
        assert!(run.manager.is_some(), "the next digest spawns a fresh manager");
    }

    #[test]
    fn manager_retry_refused_by_the_done_guard_does_not_lift_the_halt() {
        let mut ctx = TestCtx::new();
        let (mut run, planner, gate) = halted_gate_run(&mut ctx);
        let manager = run.manager.unwrap();
        // the phase is gating: every task is done, so a retry of one is refused…
        let w = ctx.add_idle();
        run.workers.push(mk_worker("t1", "worker-small", w, WState::Done));
        ctx.prompts.clear();
        let (msg, ok) = run.handle_tool(&mut ctx, manager, "mantra_retry", &json!({"task_id": "t1"}));
        assert!(!ok, "{msg}");
        assert!(run.halted(), "…and a refused tool must not resume the run");
        assert!(run.manager_escalation.is_some());
        assert!(!ctx.prompts.iter().any(|(a, _)| *a == gate || *a == planner), "{:?}", ctx.prompts);
        // a respawn of the gate itself is fine and does resume
        let (msg, ok) = run.handle_tool(&mut ctx, manager, "mantra_respawn", &json!({"agent": "gate", "note": "the venv is at .venv"}));
        assert!(ok, "{msg}");
        assert!(!run.halted());
        let g2 = run.gate_agent.unwrap();
        assert_ne!(g2, gate);
        assert!(ctx.prompts.iter().any(|(a, t)| *a == g2 && t.contains(".venv")), "{:?}", ctx.prompts);
    }

    #[test]
    fn a_failed_turn_goes_to_the_manager_and_then_to_the_user_not_the_planner() {
        let mut ctx = TestCtx::new();
        let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "test".into());
        let planner = ctx.add_idle();
        let gate = ctx.add_idle();
        run.planner = Some(planner);
        run.gate_agent = Some(gate);
        run.plan = Some(one_task_phase());
        run.stage = Stage::Phase { idx: 0, step: PhaseStep::Gate { round: 1 } };
        // first failure past retries: the free respawn (WP7.3); the second: the halt
        run.on_turn_done(&mut ctx, gate, "failed", Some("boom".into()), Some(ErrKind::Other));
        let gate2 = run.gate_agent.unwrap();
        assert_ne!(gate2, gate);
        assert!(!run.halted());
        run.on_turn_done(&mut ctx, gate2, "failed", Some("boom again".into()), Some(ErrKind::Other));
        assert!(run.halted());
        assert_eq!(run.halt.as_ref().unwrap().reason, HaltReason::AgentTurnFailed);
        let manager = run.manager.expect("the manager gets the failed turn");
        let esc = ctx.prompts.iter().find(|(a, t)| *a == manager && t.contains("[mantra:escalation]")).map(|(_, t)| t.clone()).unwrap();
        assert!(esc.contains("mantra_respawn") && esc.contains("left for the user") && !esc.contains("mantra_revise_plan"), "{esc}");
        assert!(!ctx.prompts.iter().any(|(a, t)| *a == planner && t.contains("[mantra:escalation]")));
        // the manager respawns the gate: that lifts the halt
        ctx.prompts.clear();
        let (msg, ok) = run.handle_tool(&mut ctx, manager, "mantra_respawn", &json!({"agent": "gate", "note": "the earlier turns died on a huge diff — work file by file"}));
        assert!(ok, "{msg}");
        assert!(!run.halted() && run.manager_escalation.is_none());
        let gate3 = run.gate_agent.unwrap();
        assert_ne!(gate3, gate2);
        assert!(ctx.prompts.iter().any(|(a, t)| *a == gate3 && t.contains("file by file")));
        // …and when it does nothing instead, the planner is NOT dragged in: the band is the user's
        let mut ctx2 = TestCtx::new();
        let mut run2 = Run::new(PathBuf::from("."), Pattern::builtin(), "test".into());
        let planner2 = ctx2.add_idle();
        let g = ctx2.add_idle();
        run2.planner = Some(planner2);
        run2.gate_agent = Some(g);
        run2.plan = Some(one_task_phase());
        run2.stage = Stage::Phase { idx: 0, step: PhaseStep::Gate { round: 1 } };
        run2.halt_turn_failed(&mut ctx2, Some(g), "qa: boom".into());
        let m2 = run2.manager.unwrap();
        run2.on_turn_done(&mut ctx2, m2, "completed", None, None);
        run2.on_turn_done(&mut ctx2, m2, "completed", None, None);
        assert!(run2.halted() && run2.manager_escalation.is_none());
        assert!(!ctx2.prompts.iter().any(|(a, t)| *a == planner2 && t.contains("[mantra:escalation]")), "{:?}", ctx2.prompts);
        // without a manager, exactly the old behaviour: a plain halt for the user
        let mut ctx3 = TestCtx::new();
        let mut run3 = Run::new(PathBuf::from("."), no_manager(), "test".into());
        let p3 = ctx3.add_idle();
        run3.planner = Some(p3);
        run3.stage = Stage::Planning;
        run3.halt_turn_failed(&mut ctx3, Some(p3), "the planner has been unresponsive".into());
        assert!(run3.halted() && run3.manager.is_none());
        assert!(ctx3.prompts.iter().all(|(_, t)| !t.contains("[mantra:escalation]")));
    }

    #[test]
    fn respawning_a_worker_with_a_note_keeps_its_task_prompt() {
        let mut ctx = TestCtx::new();
        let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "test".into());
        let orch = ctx.add_idle();
        let w = ctx.add_idle();
        run.orchestrator = Some(orch);
        run.plan = Some(one_task_phase());
        run.stage = Stage::Phase { idx: 0, step: PhaseStep::Orchestrating };
        let mut wk = mk_worker("t1", "worker-small", w, WState::Running);
        wk.prompt = "Build the parser module with tests.".into();
        run.workers.push(wk);
        let manager = run.ensure_manager(&mut ctx).unwrap();
        let (msg, ok) = run.handle_tool(&mut ctx, manager, "mantra_respawn", &json!({"agent": "t1", "note": "do not touch the lexer"}));
        assert!(ok, "{msg}");
        let newest = run.workers.iter().rev().find(|x| x.task.id == "t1").unwrap();
        assert_eq!(newest.attempt, 2);
        assert!(newest.prompt.contains("Build the parser module with tests.") && newest.prompt.contains("do not touch the lexer"), "{}", newest.prompt);
    }

    #[test]
    fn a_halt_does_not_interrupt_the_manager_nor_resume_prompt_it() {
        let mut ctx = TestCtx::new();
        let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "test".into());
        let planner = ctx.add_idle();
        let w = ctx.add_busy();
        run.planner = Some(planner);
        run.plan = Some(one_task_phase());
        run.stage = Stage::Phase { idx: 0, step: PhaseStep::Orchestrating };
        run.workers.push(mk_worker("t1", "worker-small", w, WState::Running));
        let manager = run.ensure_manager(&mut ctx).unwrap();
        ctx.agents.get_mut(&manager).unwrap().turn_active = true; // mid-turn: it just called a tool
        run.halt_and_escalate(&mut ctx, HaltReason::AttemptsExhausted, Some(w), "t1 has used all attempts".into());
        assert!(ctx.interrupted.contains(&w));
        assert!(!ctx.interrupted.contains(&manager), "the manager keeps its turn");
        assert!(ctx.prompts.iter().any(|(a, t)| *a == manager && t.contains("[mantra:escalation]")));
        ctx.prompts.clear();
        run.resume(&mut ctx);
        assert!(ctx.prompts.iter().any(|(a, t)| *a == w && t.contains("[mantra:resume]")));
        assert!(!ctx.prompts.iter().any(|(a, t)| *a == manager && t.contains("[mantra:resume]")), "{:?}", ctx.prompts);
    }

    #[test]
    fn a_failing_manager_is_dropped_not_halted_on() {
        let mut ctx = TestCtx::new();
        let (mut run, planner, _gate) = halted_gate_run(&mut ctx);
        let manager = run.manager.unwrap();
        ctx.prompts.clear();
        // a non-transient failure (past Codex's own retries) drops it at once
        run.on_turn_done(&mut ctx, manager, "failed", Some("stream closed".into()), Some(ErrKind::Other));
        assert!(run.manager.is_none(), "the manager is dropped");
        assert!(run.halted(), "the original halt stands…");
        assert_eq!(run.halt.as_ref().unwrap().reason, HaltReason::GateExhausted, "…and is not replaced by an AgentTurnFailed about the manager");
        assert!(ctx.prompts.iter().any(|(a, t)| *a == planner && t.contains("[mantra:escalation]")), "what it held goes to the planner: {:?}", ctx.prompts);
        // the next event brings a fresh manager
        run.resume(&mut ctx);
        run.manager_inbox.push("[mantra:health] test".into());
        run.wake_manager(&mut ctx);
        assert!(run.manager.is_some());
    }

    #[test]
    fn health_digest_reaches_an_idle_manager_with_the_overview() {
        let mut ctx = TestCtx::new();
        let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "test".into());
        let planner = ctx.add_idle();
        let orch = ctx.add_busy();
        let w = ctx.add_busy();
        run.planner = Some(planner);
        run.orchestrator = Some(orch);
        run.plan = Some(one_task_phase());
        run.stage = Stage::Phase { idx: 0, step: PhaseStep::Orchestrating };
        run.workers.push(mk_worker("t1", "worker-small", w, WState::Running));
        let manager = run.ensure_manager(&mut ctx).unwrap();
        ready(&mut ctx, manager);
        run.last_health = Instant::now() - Duration::from_secs(6 * 60);
        run.last_tick = Instant::now() - Duration::from_secs(2);
        ctx.prompts.clear();
        run.tick(&mut ctx);
        let d = ctx.prompts.iter().find(|(a, t)| *a == manager && t.contains("[mantra:health]")).map(|(_, t)| t.clone()).expect("digest");
        assert!(d.contains("Run overview") && d.contains("- t1 [worker-small]") && d.contains("orchestrator [") && d.contains("Recent journal"), "{d}");
        // not again right away, and never while halted
        ctx.prompts.clear();
        run.last_tick = Instant::now() - Duration::from_secs(2);
        run.tick(&mut ctx);
        assert!(ctx.prompts.is_empty());
        let mut off = Pattern::builtin();
        off.settings.manager_minutes = 0;
        let mut quiet = Run::new(PathBuf::from("."), off, "test".into());
        quiet.stage = Stage::Phase { idx: 0, step: PhaseStep::Orchestrating };
        quiet.plan = Some(one_task_phase());
        quiet.last_health = Instant::now() - Duration::from_secs(60 * 60);
        quiet.health_tick(&mut ctx);
        assert!(quiet.manager.is_none(), "manager_minutes = 0: no digest, no manager spawned for one");
    }

    #[test]
    fn manager_status_journal_and_ask_user() {
        let mut ctx = TestCtx::new();
        let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "test".into());
        let planner = ctx.add_idle();
        run.planner = Some(planner);
        run.plan = Some(one_task_phase());
        run.stage = Stage::Phase { idx: 0, step: PhaseStep::Orchestrating };
        let manager = run.ensure_manager(&mut ctx).unwrap();
        run.log("✓", "green", "something happened");
        let (st, ok) = run.handle_tool(&mut ctx, manager, "mantra_status", &json!({}));
        assert!(ok && st.contains("Stage:") && st.contains("Agents:") && st.contains("- manager ["), "{st}");
        let (j, ok) = run.handle_tool(&mut ctx, manager, "mantra_journal", &json!({"lines": 5}));
        assert!(ok && j.contains("something happened"), "{j}");
        let (_, ok) = run.handle_tool(&mut ctx, manager, "mantra_ask_user", &json!({"question": "the key expired — which one should I use?"}));
        assert!(ok);
        assert!(run.alerts.iter().any(|a| a.starts_with("the manager asks:")));
        ctx.prompts.clear();
        run.user_input(&mut ctx, "use OPENROUTER_API_KEY_2");
        assert!(ctx.prompts.iter().any(|(a, t)| *a == manager && t.contains("[from the user]") && t.contains("mantra_resume_run")), "{:?}", ctx.prompts);
        assert!(run.question.is_none() && run.alerts.is_empty());
    }

    #[test]
    fn brief_from_the_manager_does_not_answer_a_question_the_orchestrator_asked_the_planner() {
        let mut ctx = TestCtx::new();
        let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "test".into());
        let planner = ctx.add_idle();
        let orch = ctx.add_idle();
        run.planner = Some(planner);
        run.orchestrator = Some(orch);
        run.plan = Some(one_task_phase());
        run.stage = Stage::Phase { idx: 0, step: PhaseStep::Orchestrating };
        let manager = run.ensure_manager(&mut ctx).unwrap();
        let (_, ok) = run.handle_tool(&mut ctx, orch, "mantra_ask", &json!({"question": "rename the module?"}));
        assert!(ok && run.waiting_for_answer(orch));
        let (_, ok) = run.handle_tool(&mut ctx, manager, "mantra_brief_orchestrator", &json!({"message": "keep going on t1"}));
        assert!(ok);
        assert!(run.waiting_for_answer(orch), "the manager's brief is not the planner's answer");
        assert!(run.orch_inbox.iter().any(|m| m.starts_with("[from the manager]")));
        let (_, ok) = run.handle_tool(&mut ctx, planner, "mantra_brief_orchestrator", &json!({"message": "yes, rename it"}));
        assert!(ok && !run.waiting_for_answer(orch));
    }

    // Pause boundary --------------------------------------------------------------------------

    fn two_phase_plan() -> Plan {
        let mut p = one_task_phase();
        p.phases.push(Phase { id: "p2".into(), name: "p2".into(), tasks: vec![Task { id: "t2".into(), role: "worker-small".into(), ..Default::default() }], ..Default::default() });
        p
    }

    /// The pause race from the field: paused during the phase-1 gate with the QA report already
    /// in, the run used to verify, hand off and start phase 2 during the pause — the fresh
    /// orchestrator's spawns were refused and it sat idle after the resume until a manual retry.
    #[test]
    fn pause_at_the_gate_defers_the_handoff_and_phase_two_starts_on_resume() {
        let mut ctx = TestCtx::new();
        let mut run = Run::new(PathBuf::from("."), no_manager(), "test".into());
        let orch = ctx.add_idle();
        ready(&mut ctx, orch);
        let gate = ctx.add_busy();
        let w = ctx.add_idle();
        run.orchestrator = Some(orch);
        run.gate_agent = Some(gate);
        run.plan = Some(two_phase_plan());
        run.workers.push(mk_worker("t1", "worker-small", w, WState::Done));
        run.stage = Stage::Phase { idx: 0, step: PhaseStep::Gate { round: 1 } };

        // the QA report lands, then the pause, then the interrupted turn ends
        let (_, ok) = run.handle_tool(&mut ctx, gate, "mantra_gate_report", &json!({"pass": true, "summary": "coherent"}));
        assert!(ok);
        run.toggle_pause(&mut ctx);
        let agents_before = ctx.next;
        ctx.prompts.clear();
        run.on_turn_done(&mut ctx, gate, "interrupted", None, None);

        assert!(run.halted());
        assert_eq!(run.pending, Some(Pending::Handoff { idx: 0 }), "the pass is recorded; the handoff waits for the resume");
        assert_eq!(run.stage, Stage::Phase { idx: 0, step: PhaseStep::Gate { round: 1 } }, "the phase must not advance while paused");
        assert_eq!(ctx.next, agents_before, "no agent may be spawned while paused");
        assert!(ctx.prompts.is_empty(), "no agent may be started while paused: {:?}", ctx.prompts);
        assert_eq!(run.gate_agent, Some(gate));

        // resume: the handoff happens (once), and the phase moves on by itself from there
        run.toggle_pause(&mut ctx);
        assert!(!run.halted() && run.pending.is_none());
        assert_eq!(run.stage, Stage::Phase { idx: 0, step: PhaseStep::Handoff });
        assert!(ctx.prompts.iter().any(|(a, t)| *a == orch && t.contains("[mantra:handoff]")), "{:?}", ctx.prompts);
        assert!(!ctx.prompts.iter().any(|(a, _)| *a == gate), "the archived gate agent gets no 'continue' prompt: {:?}", ctx.prompts);
        ctx.agents.get_mut(&orch).unwrap().final_message = Some("phase 1 built t1".into());
        run.on_turn_done(&mut ctx, orch, "completed", None, None);
        run.on_job(&mut ctx, JobTag::Cleanup { phase: 0 }, JobOut::Text(Ok("committed".into())));
        assert_eq!(run.stage, Stage::Phase { idx: 1, step: PhaseStep::Orchestrating });
        let orch2 = run.orchestrator.expect("phase 2 has an orchestrator");
        assert!(ctx.prompts.iter().any(|(a, t)| *a == orch2 && t.contains("[mantra:phase] Phase 2/2")), "{:?}", ctx.prompts);
        // …whose spawn is accepted: phase-2 workers start without anyone retrying anything
        let (msg, ok) = run.handle_tool(&mut ctx, orch2, "mantra_spawn", &json!({"task_id": "t2"}));
        assert!(ok, "{msg}");
        assert!(run.workers.iter().any(|w| w.task.id == "t2" && w.state == WState::Preparing), "{msg}");
    }

    #[test]
    fn a_phase_start_due_while_paused_happens_once_on_resume() {
        let mut ctx = TestCtx::new();
        let mut run = Run::new(PathBuf::from("."), no_manager(), "test".into());
        let orch = ctx.add_idle();
        run.orchestrator = Some(orch);
        run.plan = Some(two_phase_plan());
        run.stage = Stage::Phase { idx: 0, step: PhaseStep::Handoff };
        run.handoff_note_done = true;
        run.toggle_pause(&mut ctx);
        let agents_before = ctx.next;
        // the phase's cleanup commit finishes during the pause: phase 2 is due
        run.on_job(&mut ctx, JobTag::Cleanup { phase: 0 }, JobOut::Text(Ok("committed".into())));
        assert_eq!(run.pending, Some(Pending::Phase { idx: 1 }));
        assert_eq!(run.stage, Stage::Phase { idx: 0, step: PhaseStep::Handoff });
        assert_eq!(ctx.next, agents_before, "no orchestrator is spawned while paused");
        assert!(!ctx.prompts.iter().any(|(_, t)| t.contains("[mantra:phase]")), "{:?}", ctx.prompts);
        let started = |c: &TestCtx| c.prompts.iter().filter(|(_, t)| t.contains("[mantra:phase] Phase 2/2")).count();

        run.toggle_pause(&mut ctx);
        assert_eq!(run.stage, Stage::Phase { idx: 1, step: PhaseStep::Orchestrating });
        assert_eq!(started(&ctx), 1, "{:?}", ctx.prompts);
        assert!(run.pending.is_none());
        // a later pause/resume does not start it again
        run.toggle_pause(&mut ctx);
        run.toggle_pause(&mut ctx);
        assert_eq!(started(&ctx), 1, "{:?}", ctx.prompts);
    }

    #[test]
    fn a_finale_step_reported_while_paused_waits_for_the_resume() {
        let mut ctx = TestCtx::new();
        let mut run = Run::new(PathBuf::from("."), no_manager(), "test".into());
        let planner = ctx.add_idle();
        let finale = ctx.add_busy();
        run.planner = Some(planner);
        run.finale_agent = Some(finale);
        run.plan = Some(one_task_phase());
        run.stage = Stage::Finale { idx: 0 };
        let (_, ok) = run.handle_tool(&mut ctx, finale, "mantra_gate_report", &json!({"pass": true, "summary": "all green"}));
        assert!(ok);
        run.toggle_pause(&mut ctx);
        let agents_before = ctx.next;
        ctx.prompts.clear();
        run.on_turn_done(&mut ctx, finale, "interrupted", None, None);
        assert_eq!(run.pending, Some(Pending::Finale { idx: 1 }));
        assert_eq!(run.stage, Stage::Finale { idx: 0 });
        assert_eq!(ctx.next, agents_before);
        assert!(ctx.prompts.is_empty(), "{:?}", ctx.prompts);

        run.toggle_pause(&mut ctx);
        assert_eq!(run.stage, Stage::Finale { idx: 1 });
        assert!(run.pending.is_none());
        assert_eq!(ctx.prompts.iter().filter(|(_, t)| t.contains("[mantra:finale] Step 2/")).count(), 1, "{:?}", ctx.prompts);
    }

    /// An orchestrator (re)spawned during a pause had its spawns refused and went idle with no
    /// event ahead to wake it: the resume must get it going, not the watchdog minutes later.
    #[test]
    fn resume_wakes_an_idle_orchestrator_that_has_tasks_to_spawn() {
        let mut ctx = TestCtx::new();
        let mut run = Run::new(PathBuf::from("."), no_manager(), "test".into());
        let orch = ctx.add_idle();
        run.orchestrator = Some(orch);
        run.plan = Some(one_task_phase());
        run.stage = Stage::Phase { idx: 0, step: PhaseStep::Orchestrating };
        run.toggle_pause(&mut ctx);
        run.respawn(&mut ctx, orch, None).unwrap();
        let orch2 = run.orchestrator.unwrap();
        let (msg, ok) = run.handle_tool(&mut ctx, orch2, "mantra_spawn", &json!({"task_id": "t1"}));
        assert!(!ok && msg.contains("REFUSED"), "{msg}");
        ctx.prompts.clear();

        run.toggle_pause(&mut ctx);
        assert!(ctx.prompts.iter().any(|(a, t)| *a == orch2 && t.contains("[mantra:resume]") && t.contains("t1") && t.contains("mantra_spawn")), "{:?}", ctx.prompts);
        let (msg, ok) = run.handle_tool(&mut ctx, orch2, "mantra_spawn", &json!({"task_id": "t1"}));
        assert!(ok, "{msg}");
        // nothing to wake it about once every task has an attempt
        ctx.prompts.clear();
        run.toggle_pause(&mut ctx);
        run.toggle_pause(&mut ctx);
        assert!(!ctx.prompts.iter().any(|(a, _)| *a == orch2), "{:?}", ctx.prompts);
    }

    #[test]
    fn a_finished_run_keeps_its_duration() {
        let mut ctx = TestCtx::new();
        let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "test".into());
        let now = crate::util::unix_secs();
        run.started_unix = now - 129;
        assert!(run.elapsed() >= Duration::from_secs(129), "a running run's clock keeps going");
        run.finish(&mut ctx);
        assert_eq!(run.stage, Stage::Done);
        let at = run.finished_unix.expect("completion is stamped");
        assert!((now..=now + 5).contains(&at), "{at} vs {now}");
        let done = run.elapsed();
        assert!((129..=134).contains(&done.as_secs()), "{done:?}");
        assert!(run.pulse.iter().any(|p| p.text.contains(&format!("run complete in {}", fmt_dur(done)))), "the journal and the screen agree");
        // an hour later the result screen still reads the same
        run.started_unix = now - 3600 - 129;
        run.finished_unix = Some(now - 3600);
        assert_eq!(run.elapsed(), Duration::from_secs(129));
        // a stopped run is frozen the same way
        let mut failed = Run::new(PathBuf::from("."), Pattern::builtin(), "test".into());
        failed.fail_run(&mut ctx, "planner did not submit a valid plan".into());
        assert!(failed.finished_unix.is_some());
    }
}
