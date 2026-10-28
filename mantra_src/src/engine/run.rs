//! The Conductor: a deterministic state machine that runs a pattern.
//!
//! Mechanical work (spawning, retrying API errors, saving outputs, merging, gates, handoffs)
//! happens here — never inside an LLM loop. LLM agents are woken only for judgment calls.

use super::git::{self, CheckResult, Workspace};
use super::state::{AgentState, PhaseHistory, RunState, WorkerState};
use super::pattern::{auto_effort, Pattern, Role, EFFORT_AUTO};
use super::plan::{Phase, Plan, Task};
use super::tools;
use crate::agent::{looks_like_broken_sandbox, Agent, ErrKind, Status};
use crate::hub::AgentId;
use crate::util::{clock, fmt_dur, fmt_tokens, trunc};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

mod helpers;
use helpers::{failing_cmds, failing_sig, fnv1a, join_note, parse_gate_from_text, paths_in, project_is_empty, reason_label, worker_brief_context};
pub use helpers::{tidy_title, title_from_goal};

/// How much of a phase's diff the QA agent is shown (the rest is one `git diff` away).
const GATE_DIFF_BYTES: usize = 30_000;
/// How much of the run's diff a finale agent is shown.
const FINALE_DIFF_BYTES: usize = 24_000;

#[derive(Debug)]
pub enum JobTag {
    Setup,
    WorkerWt { task: String, attempt: u32 },
    Merge { phase: usize },
    Checks { phase: usize, round: u32 },
    /// The checks re-run after a fix worker finished a gate round (`round` is the round it fixed).
    FixChecks { phase: usize, round: u32 },
    Cleanup { phase: usize },
    Finish,
    Land,
    /// A vision tool call (`mantra_screenshot` / `mantra_view_image`), answered when the capture
    /// or the read is done — never on the event loop, where a capture tool that hangs would
    /// freeze every agent for its whole timeout.
    Vision { agent: AgentId, req: Value, tool: String, name: String },
}

pub enum JobOut {
    Setup(Result<Workspace, String>),
    Wt(Result<(PathBuf, String), String>),
    Merge(git::MergeResult),
    /// The results, the paths the checks left behind that were discarded before any commit, and
    /// the phase's diff for the gate agent that follows.
    Checks(Vec<CheckResult>, Vec<String>, git::ReviewDiff),
    /// The phase commit's outcome, plus the run's diff so far (what the finale reviews).
    Cleanup(Result<String, String>, git::ReviewDiff),
    Text(Result<String, String>),
    /// A vision tool's `contentItems` and whether it worked (`tools::vision_call`).
    Vision(Vec<Value>, bool),
}

/// The aliases of `reg` whose model takes images — what `Run::set_vision_models` is handed —
/// in both spellings `Registry::get` answers to.
pub fn vision_aliases(reg: &crate::config::Registry) -> HashSet<String> {
    reg.vision_names()
}

/// What every halt for a dead model gateway ends with: what to do (nothing but wait and resume)
/// and what happens then. One text, so the band, the web hint and the journal always agree.
pub const GATEWAY_DOWN_HINT: &str = "the model gateway looks down — the run resumes where it was once you press resume (space / Resume); failed workers are respawned automatically";

/// The same for a task that ran out of attempts on the outage: there is no failed worker left
/// for a resume to respawn, so the way back is one more attempt.
pub const GATEWAY_DOWN_RETRY_HINT: &str = "the model gateway looks down — nobody was asked; once it is back, r (Retry) gives the task one more attempt";

pub struct SpawnReq {
    pub name: String,
    pub role_name: String,
    pub role: Role,
    pub cwd: PathBuf,
    pub instructions: String,
    pub tools: Vec<Value>,
    pub effort: Option<String>,
    pub extra_writable: Vec<PathBuf>,
    /// Whether a workspace-write agent may use the network: the run pattern's `settings.network`.
    pub network: bool,
    /// The run pattern's `settings.writable_roots` (`~` unexpanded), writable by a workspace-write agent.
    pub writable_roots: Vec<String>,
    /// Explicit context-window override for this spawn (set after a ContextFull halving); `None`
    /// means "use the model's own effective context".
    pub context_override: Option<u64>,
    /// What the agent's session is called in the backend (`<run title> · <role name>`), where the
    /// backend can name a session.
    pub thread_name: Option<String>,
    /// The MCP servers the agent asks for: the role's `mcp` (names, or `"*"`), set by `Run::role_req`
    /// — the one funnel every spawn goes through. The app resolves them against its settings.
    pub mcp: Vec<String>,
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
    /// A tool result with more than text: the `contentItems` as they are (`inputText`, and an
    /// `inputImage` for the vision tools). The default keeps the text alone, for a `Ctx` that has
    /// nowhere to send an image.
    fn tool_result_items(&mut self, a: AgentId, req: Value, items: Vec<Value>, ok: bool) {
        let text: Vec<&str> = items.iter().filter(|i| i.get("type").and_then(|t| t.as_str()) == Some("inputText")).filter_map(|i| i.get("text").and_then(|t| t.as_str())).collect();
        let text = text.join("\n");
        self.tool_result(a, req, text, ok);
    }
    fn set_effort(&mut self, a: AgentId, effort: &str) -> String;
    fn agent(&self, a: AgentId) -> Option<&Agent>;
    fn job(&mut self, tag: JobTag, f: Box<dyn FnOnce() -> JobOut + std::marker::Send>);
    fn notify(&mut self, text: &str);
    /// Rename a live agent's session (the run's title changed once the plan came in). Best effort;
    /// the default does nothing.
    fn set_thread_name(&mut self, _a: AgentId, _name: &str) {}
}

#[derive(Debug, Clone, PartialEq, Default, serde::Serialize, serde::Deserialize)]
pub enum PhaseStep {
    #[default]
    Orchestrating,
    Merging,
    Checks { round: u32 },
    Gate { round: u32 },
    /// A fix worker (`fix-<round>`) is working on what gate round `round` found.
    Fix { round: u32 },
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
    /// `start_finale(idx)` — past the last step this completes the run. `note` is a respawn's
    /// note for the fresh agent (`respawn_finale` during a halt).
    Finale {
        idx: usize,
        #[serde(default)]
        note: Option<String>,
    },
    /// The gate of phase `idx` at `round`: the gate agent's next round, or a fresh gate agent
    /// (`note` as for `Finale`).
    Gate {
        idx: usize,
        round: u32,
        #[serde(default)]
        note: Option<String>,
    },
    /// `begin_handoff(idx)` — phase `idx` passed its gate.
    Handoff { idx: usize },
    /// The fix worker for what gate round `round` of phase `idx` found (`findings`, `big` as the
    /// report flagged it).
    Fix {
        idx: usize,
        round: u32,
        findings: String,
        #[serde(default)]
        big: bool,
    },
}

/// Why a run the user stopped (`Run::stop`) is `Failed` — `state::stage_label` shows exactly this
/// one as "stopped" rather than "failed".
pub const STOPPED_BY_USER: &str = "stopped by you";

/// What `respawn` reports when it went ahead, and when the pause boundary deferred it.
pub const RESPAWNED: &str = "respawned with a fresh thread";
pub const RESPAWN_DEFERRED: &str = "respawn deferred — a fresh thread starts when the run resumes";

impl Pending {
    pub(super) fn label(&self) -> String {
        match self {
            Pending::Phase { idx } => format!("phase {}", idx + 1),
            Pending::Finale { idx, .. } => format!("finale step {}", idx + 1),
            Pending::Gate { idx, round, .. } => format!("phase {} gate round {round}", idx + 1),
            Pending::Handoff { idx } => format!("phase {} handoff", idx + 1),
            Pending::Fix { idx, round, .. } => format!("phase {} fix for gate round {round}", idx + 1),
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

/// Whether a worker holds one of its class's slots: preparing its worktree, running, or waiting
/// out a retry delay — what the per-class caps count (`Run::slots_taken`).
fn holds_slot(w: &Worker) -> bool {
    matches!(w.state, WState::Preparing | WState::Running | WState::Retrying(_))
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
    /// The worker is `Failed` because the model gateway was down, not because of anything it did:
    /// the next resume respawns it (same worktree), and it costs no attempt.
    pub transient: bool,
    /// The alias this attempt runs on when the user switched it live (`set_agent_model`);
    /// `None` = the role's model. The next attempt of the same task keeps it (`spawn_task`), and
    /// so does a resume — a switch made for one task is not a switch for its whole role.
    pub model: Option<String>,
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

/// What a health digest would have to report on: the journal's length, every worker's state and
/// every agent's status. Unchanged since the last digest means there is nothing new to say.
#[derive(Debug, Clone, PartialEq)]
struct HealthPrint {
    journal: u64,
    workers: Vec<(String, &'static str)>,
    agents: Vec<(AgentId, bool, Status)>,
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

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PatternProposal {
    pub pattern: Pattern,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PlannerWait {
    pub due_unix: u64,
    pub condition: String,
    pub on_reentry: String,
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
    /// looking like a broken sandbox (`agent::looks_like_broken_sandbox`), so it cannot start.
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
    /// The short form: what the pulse line, the stage band and the alert show.
    pub message: String,
    /// The same halt with nothing cut off (the backend's whole error): what `state.json`, the
    /// journal and the manager get. Equal to `message` when nothing was shortened.
    pub full: String,
    pub since: Instant,
}

pub struct PhaseRecord {
    pub name: String,
    pub workers: Vec<(String, String, String, bool)>, // task id, title, glyph, ok
    pub duration: Duration,
    pub ended: Instant,
}

pub struct Run {
    /// Akasha blocks by group name (experimental; empty unless the feature is on and the pattern has
    /// `[[groups]]`). See `engine::akasha`.
    pub akasha: std::collections::BTreeMap<String, super::akasha::Block>,
    /// `experimental = ["akasha"]` was on when the app made this run (`enable_akasha`). Off, a
    /// pattern's groups are inert: no tools, no notes in prompts, no pushes, no entries.
    pub akasha_enabled: bool,
    /// Pushes waiting for their member and the Conductor's own bookkeeping. Runtime-only.
    pub(super) akasha_live: super::akasha::Live,
    pub id: String,
    /// What the run is called for people: the plan's title once the plan is submitted, the goal's
    /// first words before that. The id stays the stable key.
    pub title: String,
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
    /// Successful worker processes retained in the app after their phase's worker list is cleared.
    /// Keyed by agent id so a failed earlier attempt of the same task is never called complete.
    pub completed_agents: HashSet<AgentId>,
    pub history: Vec<PhaseRecord>,
    pub checks: Vec<CheckResult>,
    pub conflicts: Vec<String>,
    pub gate_report: Option<(bool, String)>,
    /// What the planner/manager/user said when they resumed a halted gate or fix: it goes to the
    /// fresh QA agent / the restarted fix worker that replaces the stuck one.
    resume_note: Option<String>,
    /// How often the current QA agent was told to report (it ended its turn without).
    gate_nudges: u32,
    /// The latest gate report said the fixes are big (`mantra_gate_report big=true`): the fix
    /// worker is then `worker-big`.
    gate_big: bool,
    /// One line per gate report and finale report of the run so far — what later reviewers start
    /// from instead of re-deriving it. Saved with the state.
    pub gate_notes: Vec<String>,
    /// The previous gate round's verdict for the phase in progress, shown to the next round's
    /// fresh agent (`pass|fail` and the summary).
    prev_gate: Option<(u32, bool, String)>,
    /// The merged phase's diff as of the last checks run: what the gate agent starts from.
    phase_diff: git::ReviewDiff,
    /// The run's diff as of the last phase commit: what the finale starts from.
    run_diff: Option<git::ReviewDiff>,
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
    /// Active (not halted, not finished) run time — see `elapsed`. `active_secs` is the sum of
    /// the closed active spans; `active_since` is the start of the open one (unix secs), `None`
    /// while halted or after the run ends.
    active_secs: u64,
    active_since: Option<u64>,
    /// The planner's open question to the user, if any.
    pub question: Option<Question>,
    pub(super) pending_pattern: Option<PatternProposal>,
    pub(super) planner_wait: Option<PlannerWait>,
    /// Refresh after the current tool result and turn have completed.
    planner_refresh_pending: bool,
    /// Emergency stops are deliberate; late process events must not auto-restart them.
    killed_agents: HashSet<AgentId>,
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
    pub(super) extra_attempts: u32,
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
    /// What the run looked like at the last health digest (`health_fingerprint`), so a quiet
    /// stretch costs the manager nothing.
    last_health_print: Option<HealthPrint>,
    /// Something happened (a journal line was written) since `state.json` was last flushed by
    /// `flush_state` — the event-driven half of persistence, debounced to the tick cadence.
    state_dirty: bool,
    /// Per slot (`first_token_key`): how many times its turn sat through `first_token_minutes`
    /// without any output since it last completed a turn. The slot, not the `AgentId`, because the
    /// first strike respawns the agent.
    first_token_strikes: HashMap<String, u8>,
    /// Agents already acted on for their current silent spell, so one spell is one strike.
    first_token_flagged: HashSet<AgentId>,
    /// When each agent last failed a turn on the model gateway (a transport error, see
    /// `agent::is_transport_error`) — what tells a dead gateway from one agent's bad luck.
    transport_errs: HashMap<AgentId, Instant>,
    /// Per worker: consecutive gateway failures retried without costing an attempt.
    gateway_streak: HashMap<AgentId, u32>,
    /// Agents whose rejected reasoning effort was already remapped once (one retry, not a loop).
    effort_retried: HashSet<AgentId>,
    /// A halt currently in the manager's hands (see `escalate_to_manager`).
    manager_escalation: Option<ManagerEscalation>,
    /// The watchdog woke the manager about an idle agent (ladder step 3). Cleared when the manager
    /// completes a turn; past the deadline the planner gets it as before.
    manager_watchdog: Option<ManagerCase>,
    /// Agents whose turn ended with `mantra_wait` (or a `mantra_status` with nothing new): a tool
    /// call they make in that same turn is refused. Runtime-only; cleared when the turn ends.
    pub(super) yielded: HashSet<AgentId>,
    /// Yielded agents already interrupted for a refused tool call this turn, so the interrupt goes
    /// out once. Runtime-only.
    yield_interrupted: HashSet<AgentId>,
    /// The last `mantra_status` text each agent was given this turn. Runtime-only.
    last_status: HashMap<AgentId, String>,
    /// Models the user switched live on a fixed agent, by slot (`planner`, `manager`,
    /// `orchestrator`, `gate`, `finale`): a respawn of that slot and a resume keep them. Saved
    /// through `agent_meta` (`AgentState.model_alias`), where a resume reads them back.
    pub(super) slot_models: HashMap<String, String>,
    /// What the registry knows, alias → effective context window (`Registry::context_windows`),
    /// handed in by the app (`set_known_models`): the engine has no registry of its own. Tells a
    /// saved alias that still exists from one that was deleted, and gives `settings.context_cap`
    /// a window to be measured against. Empty = never told (tests): every alias is trusted.
    known_models: HashMap<String, u64>,
    /// The aliases whose model takes images (`ModelEntry.vision`), from the app beside
    /// `known_models` (`set_vision_models`). Empty = none: unlike `known_models`, never trusted
    /// blindly — a screenshot tool reaches an agent only when its model is known to see.
    known_vision: HashSet<String>,
    /// Hash of the pattern as last written to the run dir's `pattern.toml` — a resume reloads from
    /// there, so a role-level model change (`set_role_model all`) is written back when it differs.
    pattern_hash: std::sync::atomic::AtomicU64,
    /// This process, as the run's owner (`<run dir>/owner.json`, see `state::Owner`): written
    /// with every state flush while the run is live, released when it ends or this `Run` is
    /// dropped (a clean quit, `/leave`), left behind by a crash — where a dead pid lets the next
    /// resume go ahead and a live one refuses it.
    owner: super::state::Owner,
}

impl Run {
    pub fn new(project: PathBuf, pattern: Pattern, brief: String) -> Run {
        let id = format!("{}-{}", crate::util::unix_secs() % 1_000_000, crate::util::slug(&brief).chars().take(24).collect::<String>().trim_matches('-'));
        let dir = crate::config::runs_dir(&project).join(&id);
        let title = title_from_goal(&brief);
        Run {
            akasha: std::collections::BTreeMap::new(),
            akasha_enabled: false,
            akasha_live: Default::default(),
            id,
            title,
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
            completed_agents: HashSet::new(),
            history: vec![],
            checks: vec![],
            conflicts: vec![],
            gate_report: None,
            gate_big: false,
            gate_nudges: 0,
            resume_note: None,
            gate_notes: vec![],
            prev_gate: None,
            phase_diff: git::ReviewDiff::default(),
            run_diff: None,
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
            active_secs: 0,
            active_since: Some(crate::util::unix_secs()),
            question: None,
            pending_pattern: None,
            planner_wait: None,
            planner_refresh_pending: false,
            killed_agents: HashSet::new(),
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
            last_health_print: None,
            state_dirty: false,
            first_token_strikes: HashMap::new(),
            first_token_flagged: HashSet::new(),
            transport_errs: HashMap::new(),
            gateway_streak: HashMap::new(),
            effort_retried: HashSet::new(),
            manager_escalation: None,
            manager_watchdog: None,
            yielded: HashSet::new(),
            yield_interrupted: HashSet::new(),
            last_status: HashMap::new(),
            slot_models: HashMap::new(),
            known_models: HashMap::new(),
            known_vision: HashSet::new(),
            pattern_hash: std::sync::atomic::AtomicU64::new(0),
            owner: super::state::Owner::this_process(),
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
        self.journal_line(&p.glyph, &p.text);
        self.pulse.push_back(p);
        while self.pulse.len() > 300 {
            self.pulse.pop_front();
        }
        self.state_dirty = true;
    }

    /// Append one line to `journal.jsonl` only (no pulse entry): for text too long for the feed
    /// but worth keeping whole, like the backend's complete error behind a halt.
    fn journal_line(&self, glyph: &str, text: &str) {
        let line = json!({"t": crate::util::unix_secs(), "glyph": glyph, "text": text}).to_string();
        let _ = std::fs::create_dir_all(&self.dir);
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(self.dir.join("journal.jsonl")) {
            use std::io::Write;
            let _ = writeln!(f, "{line}");
        }
    }

    fn save_plan(&self) {
        if let Some(p) = &self.plan {
            let _ = std::fs::create_dir_all(&self.dir);
            let _ = std::fs::write(self.dir.join("plan.json"), serde_json::to_string_pretty(p).unwrap_or_default());
            let _ = std::fs::write(self.dir.join("plan.md"), p.to_markdown());
        }
    }

    /// An agent's place in the run as `state.json` names it: `planner | manager | orchestrator |
    /// gate | worker:<task>#<attempt> | finale`, empty for an agent that is not part of the run.
    pub fn slot_of(&self, a: AgentId) -> String {
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
    }

    /// Persist everything a later `mantra runs resume` needs (see `engine::state`). Cheap: a few
    /// KB rewritten atomically at every transition. The owner file goes with it — refreshed while
    /// the run is live, released once it is over — and so does the run's `pattern.toml` when a
    /// role-level model change made it differ from the copy on disk.
    pub(crate) fn save_state(&self) {
        let slot_of = |a: AgentId| self.slot_of(a);
        if self.is_active() {
            self.owner.write(&self.dir);
        } else {
            self.owner.release(&self.dir);
        }
        let toml = self.pattern.to_toml();
        let hash = fnv1a(&toml);
        if self.pattern_hash.swap(hash, std::sync::atomic::Ordering::Relaxed) != hash {
            let _ = std::fs::create_dir_all(&self.dir);
            let _ = std::fs::write(self.dir.join("pattern.toml"), toml);
        }
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
                    transient: w.transient,
                    model: w.model.clone(),
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
            title: self.title.clone(),
            project: self.project.clone(),
            brief: self.brief.clone(),
            pattern: self.pattern.name.clone(),
            plan_version: self.plan_version,
            stage: self.stage.clone(),
            halted: self.halt.as_ref().map(|h| h.full.clone()),
            pending: self.pending.clone(),
            pending_pattern: self.pending_pattern.clone(),
            planner_wait: self.planner_wait.clone(),
            started_unix: self.started_unix,
            finished_unix: self.finished_unix,
            active_secs: Some(self.elapsed().as_secs()),
            updated_unix: crate::util::unix_secs(),
            ws: self.ws.clone(),
            handoff: self.handoff.clone(),
            gate_notes: self.gate_notes.clone(),
            workers,
            agents,
            history: self.history.iter().map(|h| PhaseHistory { name: h.name.clone(), workers: h.workers.clone(), secs: h.duration.as_secs() }).collect(),
            akasha: self.akasha.clone(),
        }
        .save(&self.dir);
    }

    /// What the registry knows (`Registry::context_windows`), from the app: at a run's start, on
    /// resume and after every live model switch — the only times an alias can enter the run.
    pub fn set_known_models(&mut self, models: HashMap<String, u64>) {
        self.known_models = models;
    }

    /// Which aliases take images (`vision_aliases`), from the app at the same moments as
    /// `set_known_models`.
    pub fn set_vision_models(&mut self, aliases: HashSet<String>) {
        self.known_vision = aliases;
    }

    /// Does `alias`'s model take images? Only when the app said so.
    pub(super) fn sees(&self, alias: &str) -> bool {
        self.known_vision.contains(alias)
    }

    /// The last step of every spawn (fresh, respawned, re-attached on resume): the role's MCP
    /// servers, the Akasha tools of a group member, and the vision tools when the model takes
    /// images. Tools are fixed at spawn — a live switch to a model that sees gets them with the
    /// agent's next respawn, not mid-thread.
    pub(super) fn role_req(&self, req: SpawnReq) -> SpawnReq {
        let mut req = self.akasha_req(req);
        req.mcp = req.role.mcp.clone();
        if req.role.can_spawn_swarm {
            req.tools.extend(tools::swarm_tools());
            req.instructions.push_str(tools::SWARM_OWNER_PROTOCOL);
        }
        if self.sees(&req.role.model) {
            req.tools.extend(tools::vision_tools());
        }
        req
    }

    /// Is `alias` still in the registry? Trusted blindly when the app never said what it knows.
    pub(super) fn knows_model(&self, alias: &str) -> bool {
        self.known_models.is_empty() || self.known_models.contains_key(alias)
    }

    /// The user switched a run agent's model live (the `m` picker, `ctrl+k`, the web `set_model`):
    /// remember it for *that agent* — the task's next attempt, the slot's next respawn and a
    /// resume keep it — and nothing else: the role's other workers go on with the role's model.
    /// A switch back to the role's model forgets the choice. `set_role_model` (one role, or
    /// `all`) is the role-level change and writes the pattern copy instead.
    pub fn set_agent_model(&mut self, a: AgentId, alias: &str) {
        let slot = self.slot_of(a);
        if slot.is_empty() {
            return;
        }
        let role_model = self.role_name_of(a).map(|r| self.role(&r).model).unwrap_or_default();
        let own = (alias != role_model).then(|| alias.to_string());
        let pinned = own.is_some();
        if let Some(wi) = self.worker_idx(a) {
            self.workers[wi].model = own;
        } else {
            match own {
                Some(m) => self.slot_models.insert(slot, m),
                None => self.slot_models.remove(&slot),
            };
        }
        if let Some(m) = self.agent_meta.get_mut(&a) {
            m.model_alias = alias.to_string();
        }
        let name = self.name_of(a);
        let scope = if pinned { "this agent only — its respawns and a resume keep it" } else { "the role's model" };
        self.log("⚙", "blue", format!("{name}: model → {alias} ({scope})"));
        self.save_state();
    }

    /// Persist the fresh-session boundary before replacing a backend. A crash between the
    /// model change and its Ready event must never resume a foreign backend's session id.
    pub fn forget_agent_session(&mut self, a: AgentId) {
        if let Some(meta) = self.agent_meta.get_mut(&a) {
            meta.thread_id = None;
            self.save_state();
        }
    }

    /// The role as a spawn into `slot` should see it: with the model the user chose live for
    /// that slot, if any, in place of the role's (`set_agent_model`).
    fn role_for_slot(&self, slot: &str, role_name: &str) -> Role {
        let mut role = self.role(role_name);
        if let Some(m) = self.slot_models.get(slot) {
            role.model = m.clone();
        }
        role
    }

    /// The context window a spawn is told it has: the attempt's own override (a halved assumed
    /// window) or the model's, and never more than `settings.context_cap` (0 = no cap) — the
    /// backend then compacts at the cap instead of letting an agent carry a 500k-token context
    /// through hours of tool cycling. An alias the registry does not know is capped as it is.
    fn context_for(&self, alias: &str, over: Option<u64>) -> Option<u64> {
        let cap = self.pattern.settings.context_cap;
        self.known_models.get(alias).copied().into_iter()
            .chain(over)
            .chain((cap > 0).then_some(cap))
            .filter(|w| *w > 0)
            .min()
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

    /// Active run time: the wall clock minus every halt (a user pause included), frozen once the
    /// run is done or stopped. Survives a resume (`from_state` carries the total over; the time
    /// the process was not running is not active time either).
    pub fn elapsed(&self) -> Duration {
        self.elapsed_at(crate::util::unix_secs())
    }

    pub(super) fn elapsed_at(&self, now: u64) -> Duration {
        Duration::from_secs(self.active_secs + self.active_since.map(|t| now.saturating_sub(t)).unwrap_or(0))
    }

    /// Close the open active span (halt, finish, fail): the clock stands still from here.
    pub(super) fn clock_stop_at(&mut self, now: u64) {
        if let Some(t) = self.active_since.take() {
            self.active_secs += now.saturating_sub(t);
        }
    }

    /// Open a new active span (resume) — a no-op on a finished run, which stays frozen.
    pub(super) fn clock_start_at(&mut self, now: u64) {
        if self.active_since.is_none() && self.is_active() {
            self.active_since = Some(now);
        }
    }

    pub(super) fn set_clock(&mut self, active_secs: u64, active_since: Option<u64>) {
        self.active_secs = active_secs;
        self.active_since = active_since;
    }

    fn clock_stop(&mut self) {
        self.clock_stop_at(crate::util::unix_secs());
    }

    fn clock_start(&mut self) {
        self.clock_start_at(crate::util::unix_secs());
    }

    pub fn is_active(&self) -> bool {
        !matches!(self.stage, Stage::Done | Stage::Failed(_))
    }

    pub fn halted(&self) -> bool {
        self.halt.is_some()
    }

    /// `halt_full` with a message that needs no shortening.
    #[cfg(test)]
    fn halt(&mut self, ctx: &mut dyn Ctx, reason: HaltReason, agent: Option<AgentId>, message: String) {
        self.halt_full(ctx, reason, agent, message.clone(), message);
    }

    /// Halt the run: interrupt whatever is busy, remember why, journal and alert. Every failure
    /// the run cannot handle by itself goes through here instead of a bare pause flag. The
    /// message comes in two lengths: `message` for the pulse line, the band and the alert; `full`
    /// — the backend's whole error, nothing cut off — for the journal, `state.json` and whoever
    /// the halt is escalated to.
    fn halt_full(&mut self, ctx: &mut dyn Ctx, reason: HaltReason, agent: Option<AgentId>, message: String, full: String) {
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
        if full != message {
            self.journal_line("⛔", &format!("halted (full): {}", crate::util::strip_ansi(&full)));
        }
        self.alerts.push(message.clone());
        ctx.notify(&format!("Mantra needs you: {message}"));
        self.halt = Some(Halt { reason, agent, message, full, since: Instant::now() });
        self.clock_stop();
        self.save_state();
    }

    /// Un-halt: re-prompt every agent that was interrupted for this, and pick the run back up
    /// wherever it left off. Always an explicit act — `space` / Resume, a retry, or the
    /// manager/planner acting on an escalation; changing a model never calls it.
    pub fn resume(&mut self, ctx: &mut dyn Ctx) {
        self.resume_by(ctx, None);
    }

    /// `resume`, with the agent that caused it (the planner acting on an escalation) left out of
    /// the "continue where you left off" re-prompts — it is mid-turn and knows.
    pub fn resume_by(&mut self, ctx: &mut dyn Ctx, except: Option<AgentId>) {
        self.resume_core(ctx, except, true, true);
    }

    /// Lift a halt so that a restart of `target` (a retry, a respawn) can go ahead. When that
    /// restart replaces the agent the stage is waiting on (the QA agent of a gate round, the fix
    /// worker), the stage's own pick-up is skipped: the restart *is* the pick-up, and doing both
    /// would start the agent twice.
    fn resume_for_restart(&mut self, ctx: &mut dyn Ctx, except: Option<AgentId>, target: AgentId) {
        let in_gate = matches!(self.stage, Stage::Phase { step: PhaseStep::Gate { .. } | PhaseStep::Checks { .. } | PhaseStep::Fix { .. }, .. });
        let fix_worker = self.worker_idx(target).map(|i| self.workers[i].adhoc).unwrap_or(false);
        let replaces = in_gate && (Some(target) == self.gate_agent || fix_worker);
        self.resume_core(ctx, except, !replaces, false);
    }

    /// `respawn`: also put the workers the gateway outage failed back to work (not for a restart of
    /// one specific agent, which is its own answer).
    fn resume_core(&mut self, ctx: &mut dyn Ctx, except: Option<AgentId>, pickup: bool, respawn: bool) {
        if self.halt.is_none() {
            return;
        }
        let resolved = self.halt.take().unwrap();
        self.clock_start();
        self.escalation_open = false;
        self.manager_escalation = None;
        self.last_verify_sig = None;
        self.alerts.retain(|alert| *alert != resolved.message);
        let paused = std::mem::take(&mut self.paused_agents);
        self.log("▶", "green", "run resumed");
        self.save_state();
        let t0 = Instant::now();
        if respawn {
            self.respawn_after_outage(ctx);
        }
        let pending = self.pending.take();
        if let (false, Some(p @ (Pending::Gate { .. } | Pending::Fix { .. }))) = (pickup, &pending) {
            // the restart that follows replaces the very agent this was waiting to start
            self.log("‖", "amber", format!("{} was due — superseded by the restart", p.label()));
        } else if let Some(p) = pending {
            // A transition that became due during the halt (the pause boundary): now, once.
            self.perform_pending(ctx, p);
        } else if !pickup {
            // the explicit restart of the agent this stage waits on takes it from here
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
                    } else {
                        // no checks to verify with — a fresh QA agent is the gate
                        self.spawn_gate_at_round(ctx, idx, 1);
                    }
                }
                PhaseStep::Fix { round } => self.resume_fix(ctx, idx, round),
                PhaseStep::Handoff if !self.cleanup_done && resolved.message == format!("phase {} commit failed", idx + 1) => self.launch_phase_commit(ctx, idx),
                _ => {}
            }
        }
        self.resume_note = None;
        // Re-prompt what the halt interrupted — unless the pick-up above already gave it its
        // next prompt (`edges` is stamped at every engine prompt) or archived it.
        let live = self.all_agents();
        for a in paused {
            if Some(a) == except || !live.contains(&a) || self.edges.get(&a).map(|t| *t >= t0).unwrap_or(false) {
                continue;
            }
            ctx.prompt(a, self.with_akasha(a, "[mantra:resume] The run was paused and is resuming now. Continue where you left off.".into()));
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
            Pending::Finale { idx, note } => {
                self.start_finale(ctx, idx);
                if let (Some(n), Some(a)) = (note, self.finale_agent) {
                    ctx.prompt(a, format!("[mantra:respawn] {n}"));
                }
            }
            Pending::Handoff { idx } => self.begin_handoff(ctx, idx),
            Pending::Gate { idx, round, note } => {
                self.spawn_gate_at_round(ctx, idx, round);
                if let (Some(n), Some(g)) = (note, self.gate_agent) {
                    ctx.prompt(g, format!("[mantra:respawn] {n}"));
                }
            }
            Pending::Fix { idx, round, findings, big } => self.start_fix(ctx, idx, round, findings, big),
        }
    }

    /// A stage change made during a halt (a plan revision, a stop) outdates the transition the
    /// pause boundary remembered: forget it, or `resume` would perform it against the new stage.
    fn drop_pending(&mut self, why: &str) {
        if let Some(p) = self.pending.take() {
            self.log("‖", "amber", format!("{} was due — dropped: {why}", p.label()));
            self.save_state();
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
        // one waiting on the planner's answer is woken by that answer, not by a kick
        if paused.contains(&o) || self.waiting_for_answer(o) || ctx.agent(o).map(|a| a.busy()).unwrap_or(false) {
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
        self.halt_and_escalate_full(ctx, reason, agent, message.clone(), message, None);
    }

    /// `halt_and_escalate` with the untruncated text (see `halt_full`): the escalation prompt
    /// carries `full`, so the manager or planner reads the real error. `error`: the failed turn's
    /// own error when the halt is about one (a task out of attempts, a fix worker whose turn
    /// failed) — only that is read for an outage, never the prose around it: a blocked report
    /// or a task title that says "timeout" is not the gateway.
    fn halt_and_escalate_full(&mut self, ctx: &mut dyn Ctx, reason: HaltReason, agent: Option<AgentId>, message: String, full: String, error: Option<&str>) {
        if self.halted() {
            return;
        }
        // A failure that is the gateway's (a 5xx, a reset connection — `is_transport_error`) is
        // nobody's to resolve: the manager and the planner run through the same gateway, so
        // waking them is just more failed turns.
        if error.map(|e| crate::agent::is_transport_error(None, e)).unwrap_or(false) {
            // no agent: a task out of attempts (`spawn_task`), which no resume respawns
            let hint = if agent.is_none() && reason == HaltReason::AttemptsExhausted { GATEWAY_DOWN_RETRY_HINT } else { GATEWAY_DOWN_HINT };
            self.halt_for_outage(ctx, reason, agent, message, full, hint);
            return;
        }
        self.halt_full(ctx, reason, agent, message, full.clone());
        let message = full;
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
        self.halt_turn_failed_full(ctx, agent, message.clone(), message);
    }

    /// `halt_turn_failed` with the untruncated text (see `halt_full`).
    fn halt_turn_failed_full(&mut self, ctx: &mut dyn Ctx, agent: Option<AgentId>, message: String, full: String) {
        if self.halted() {
            return;
        }
        self.halt_full(ctx, HaltReason::AgentTurnFailed, agent, message, full.clone());
        let message = full;
        if self.has_manager() && agent != self.manager {
            self.escalate_to_manager(ctx, message, false);
        }
    }

    /// The model gateway is down: halt for the user and nobody else, with the dead-gateway
    /// wording (`hint`: `GATEWAY_DOWN_HINT`, so the band and the web hint already fit, or
    /// `GATEWAY_DOWN_RETRY_HINT` for a task with no attempt left). `reason` is the
    /// halt's own — `AgentTurnFailed` for a worker past its gateway retries or for every working
    /// agent failing on the transport at once, the exhausted-attempts one when it came through
    /// `halt_and_escalate` — but no rung of the chain of command is woken: the manager and the
    /// planner would only fail the same way, and `resume` respawns every worker the outage took
    /// (`respawn_after_outage`).
    fn halt_for_outage(&mut self, ctx: &mut dyn Ctx, reason: HaltReason, agent: Option<AgentId>, message: String, full: String, hint: &str) {
        if self.halted() {
            return;
        }
        self.halt_full(ctx, reason, agent, format!("{message} — {hint}"), format!("{full} — {hint}"));
        self.log("◈", "amber", "not escalated: the manager and the planner share the outage — resume when the gateway is back");
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
        // a gateway halt (`halt_for_outage`) is lifted by resuming, whatever its reason: a
        // respawn would only be refused while the run is halted, and `r` on a task with no
        // attempt left is the one case its own hint already names
        if h.message.contains("the model gateway looks down") && !h.message.contains(GATEWAY_DOWN_RETRY_HINT) {
            return "space resumes once the gateway is back — the workers the outage took are respawned for free".into();
        }
        match h.reason {
            HaltReason::User => "space resume".into(),
            HaltReason::Auth | HaltReason::UsageLimit => "fix credentials or quota, then space".into(),
            HaltReason::ProviderRejected => {
                let who = h.agent.map(|a| self.name_of(a)).unwrap_or_else(|| "the agent".into());
                if h.message.to_lowercase().contains("message role") {
                    format!("this provider rejects Codex's developer messages — use it through bija (kind = bija) or Claude Code (kind = claude-code), or m switch model for {who}")
                } else {
                    format!("m switch model for {who}, then space resumes")
                }
            }
            HaltReason::Environment => "fix the environment (see the message above), then space resumes".into(),
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

    /// The effort to hand a spawn. An explicit request (the orchestrator's `mantra_spawn`, a
    /// task's own) wins; `auto` — asked for or set on the role — becomes a concrete level from the
    /// role's job (`pattern::auto_effort`; `finale` when the role runs a finale step); anything
    /// else is `None`, which leaves the role's own concrete effort in force.
    pub(super) fn effort_for(&self, requested: Option<String>, role_name: &str, role: &Role, finale: bool) -> Option<String> {
        let requested = requested.map(|e| e.trim().to_string()).filter(|e| !e.is_empty());
        let auto = requested.as_deref().map(|e| e == EFFORT_AUTO).unwrap_or_else(|| role.effort.trim() == EFFORT_AUTO);
        if auto {
            Some(auto_effort(&role.kind, role_name, finale).to_string())
        } else {
            requested
        }
    }

    /// The plan came in (or was revised): its title becomes the run's name, and the sessions of the
    /// agents already running are renamed to match.
    fn retitle(&mut self, ctx: &mut dyn Ctx, title: &str) {
        let title = tidy_title(title);
        if title.is_empty() || title == self.title {
            return;
        }
        self.title = title;
        let live: Vec<AgentId> = [self.planner, self.manager, self.orchestrator, self.gate_agent].into_iter().flatten().collect();
        let named: Vec<(AgentId, String)> = live.into_iter().filter_map(|a| self.role_name_of(a).map(|r| (a, r))).collect();
        for (a, role) in named {
            if let Some(n) = self.thread_name(&role) {
                ctx.set_thread_name(a, &n);
            }
        }
        self.save_state();
    }

    /// What an agent's backend session is called: `<run title> · <role name>`.
    pub(super) fn thread_name(&self, role_name: &str) -> Option<String> {
        let title = self.title.trim();
        Some(if title.is_empty() { role_name.to_string() } else { format!("{title} · {role_name}") })
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

    pub(super) fn worker_idx(&self, a: AgentId) -> Option<usize> {
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

    /// What a `mantra_status` reader would act on: stage, worker states, activity, step progress and
    /// the waiting/paused/stopped flags. Clocks and token counts are left out on purpose.
    pub(super) fn status_fingerprint(&self, ctx: &dyn Ctx) -> String {
        let mut s = format!("{:?}|", self.stage);
        for w in &self.workers {
            let a = w.agent.and_then(|a| ctx.agent(a));
            let (act, prog, stopped) = match a {
                Some(a) => (a.activity.clone(), a.plan_progress(), a.stopped_by_user),
                None => (String::new(), (0, 0), false),
            };
            let waiting = w.agent.map(|a| self.pending_questions.contains_key(&a)).unwrap_or(false);
            s.push_str(&format!("{}:{:?}:{act}:{prog:?}:{waiting}:{}:{stopped};", w.task.id, w.state, w.paused));
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
        let asking = |a: AgentId| self.pending_questions.contains_key(&a) || self.question.as_ref().map(|q| q.from == a).unwrap_or(false) || (Some(a) == self.planner && (self.planner_wait.is_some() || self.pending_pattern.is_some()));
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
                PhaseStep::Fix { .. } => {
                    for w in &self.workers {
                        if w.adhoc && w.state == WState::Running {
                            if let Some(a) = w.agent.filter(|a| !asking(*a)) {
                                v.push((a, Expect::Working(w.task.id.clone())));
                            }
                        }
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
        let toml = self.pattern.to_toml();
        self.pattern_hash.store(fnv1a(&toml), std::sync::atomic::Ordering::Relaxed);
        let _ = std::fs::write(self.dir.join("pattern.toml"), toml);
        // the run is ours from here: another Mantra must not resume it while this one lives
        self.owner.write(&self.dir);
        self.log("◈", "saffron", format!("run {} started with pattern {}", self.id, self.pattern.name));
        self.akasha_start_note();
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
        self.planner_refresh_pending = false;
        let name = self.pattern.flow.planner.clone();
        let role = self.role_for_slot("planner", &name);
        let wr = self.pattern.worker_roles();
        let req = SpawnReq {
            name: "planner".into(),
            effort: self.effort_for(None, &name, &role, false),
            thread_name: self.thread_name(&name),
            role_name: name,
            instructions: format!("{}\n{}", role.instructions, tools::PLANNER_PROTOCOL),
            cwd: self.workspace_dir(),
            tools: tools::planner_tools(&wr),
            extra_writable: vec![],
            context_override: self.context_for(&role.model, None),
            network: self.pattern.settings.network,
            writable_roots: self.pattern.settings.writable_roots.clone(),
            mcp: vec![],
            role,
        };
        let req = self.role_req(req);
        let id = match resume {
            Some(t) => ctx.spawn_resumed(req, t),
            None => ctx.spawn(req),
        };
        self.planner = Some(id);
        id
    }

    pub(super) fn planning_prompt(&self) -> String {
        if self.pattern.is_auto() {
            let mut sample = Pattern::builtin_lean();
            let preferred = self.pattern.roles.get(&self.pattern.flow.planner).map(|r| r.model.clone()).unwrap_or_else(|| "sol".into());
            let mut available: Vec<_> = self.known_models.keys().cloned().collect();
            available.sort();
            let model = if self.known_models.is_empty() || self.known_models.contains_key(&preferred) { preferred } else { available[0].clone() };
            for role in sample.roles.values_mut() {
                if !self.known_models.is_empty() && !self.known_models.contains_key(&role.model) { role.model = model.clone(); }
            }
            return format!("[mantra:auto]\nUSER REQUEST:\n{}\n\nProject directory: {}\nAuto has no fixed workflow yet. Work with the user to choose a proportional topology and roles, then call mantra_revise_pattern with full valid TOML and your rationale. Wait for approval before planning work. Configured model aliases: {}. Use only these when the list is supplied. Here is a valid starting schema (adapt it, including its name, to the task):\n```toml\n{}\n```", self.brief, self.workspace_dir().display(), available.join(", "), sample.to_toml());
        }
        let s = &self.pattern.settings;
        let workers = self.pattern.worker_roles();
        // Each role with how many of it run at once — its own class, or the pool it shares — so
        // the planner cuts a phase to the width it will actually run at.
        let pool: Vec<&str> = workers.iter().map(String::as_str).filter(|n| !self.pattern.has_own_cap(n)).collect();
        let mut roles = String::new();
        for n in &workers {
            let r = self.role(n);
            let cap = match r.parallel {
                Some(c) => format!("up to {c} at once"),
                None => match pool.iter().filter(|o| *o != n).copied().collect::<Vec<_>>() {
                    others if others.is_empty() => format!("up to {} at once", s.max_parallel),
                    others => format!("up to {} at once, shared with {}", s.max_parallel, others.join(", ")),
                },
            };
            roles.push_str(&format!("- {n} — {cap}: {} (model {}, effort {})\n", r.description, r.model, r.effort));
        }
        let wide: Vec<String> = workers
            .iter()
            .map(|n| (n, self.pattern.phase_task_limit([n.as_str()])))
            .filter(|(_, limit)| *limit > s.max_tasks_per_phase)
            .map(|(n, limit)| format!("{limit} in a phase that uses {n}"))
            .collect();
        let per_phase = if wide.is_empty() { format!("at most {} tasks per phase", s.max_tasks_per_phase) } else { format!("at most {} tasks per phase ({})", s.max_tasks_per_phase, wide.join(", ")) };
        // A resumed/respawned planner calls this same function, so an empty project skips the
        // "explore first" line for it too — nothing new appears after a respawn to explore.
        let explore = if project_is_empty(&self.workspace_dir()) {
            "The project directory is empty (greenfield): skip exploration and submit the plan in this turn."
        } else {
            "Explore the project, then call mantra_submit_plan."
        };
        format!(
            "[mantra:plan]\nUSER REQUEST:\n{}\n\nProject directory: {}\nWorker roles you can assign:\n{roles}\nConstraints: {per_phase}; tasks in one phase run in parallel on isolated copies and must use disjoint scopes; each phase ends with gate checks (shell commands) + a QA agent.\n\n{explore}",
            self.brief,
            self.workspace_dir().display(),
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
        self.prev_gate = None;
        self.phase_diff = git::ReviewDiff::default();
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
            self.with_akasha(orch, format!(
                "[mantra:phase] Phase {}/{}: {}\nGoal: {}\nOverall plan: {}\n\n{handoff}Current phase:\n```json\n{phase_json}\n```\nSpawn the tasks with mantra_spawn (sharpen prompts if useful), then call mantra_wait.",
                idx + 1,
                plan.phases.len(),
                phase.name,
                phase.goal,
                plan.summary
            )),
        );
        self.save_state();
    }

    pub(super) fn spawn_task(&mut self, ctx: &mut dyn Ctx, task: Task, prompt: Option<String>, effort: Option<String>) -> String {
        if let Some(h) = &self.halt {
            // Nothing starts while the run is halted — otherwise a respawned orchestrator
            // happily re-runs a phase the user or the planner is still sorting out.
            return format!("REFUSED: the run is halted ({}) — it must be resumed first (planner: mantra_resume_run; user: space)", trunc(&h.message, 80));
        }
        let prompt = prompt.filter(|p| !p.trim().is_empty()).unwrap_or_else(|| task.prompt.clone());
        let attempt = self.workers.iter().filter(|w| w.task.id == task.id).map(|w| w.attempt).max().unwrap_or(0) + 1;
        let cap = self.max_attempts();
        if attempt > cap {
            let failure = self.workers.iter().rev().find(|w| w.task.id == task.id).and_then(|w| match &w.state {
                WState::Failed(m) => Some(m.clone()),
                _ => None,
            });
            // the pulse line and the band keep the short form; the journal, `state.json` and the
            // escalation get the whole last failure
            let tail = |m: Option<String>| m.map(|m| format!(" Last failure: {m}.")).unwrap_or_default();
            let msg = format!("{} has used all {cap} attempts — not respawning.{}", task.id, tail(failure.as_ref().map(|m| trunc(m, 160))));
            let full = format!("{} has used all {cap} attempts — not respawning.{}", task.id, tail(failure.clone()));
            self.halt_and_escalate_full(ctx, HaltReason::AttemptsExhausted, None, msg.clone(), full, failure.as_deref());
            return format!("REFUSED: {msg}");
        }
        let queued = self.slots_taken(&task.role) >= self.pattern.cap_of(&task.role);
        let tid = task.id.clone();
        let role = task.role.clone();
        let adhoc = task.id.starts_with("fix-");
        // A halved context from an earlier attempt of the same task carries forward, and so does
        // the model the user switched that attempt to: a retry or a respawn is the same task.
        let previous = self.workers.iter().rev().find(|w| w.task.id == tid);
        let context_override = previous.and_then(|w| w.context_override);
        let model = previous.and_then(|w| w.model.clone());
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
            transient: false,
            model,
        });
        if queued {
            self.log("…", "gray", format!("{tid} queued ({})", self.class_cap_text(&role)));
            return format!("{tid} queued — it starts when a slot frees up");
        }
        self.prepare_worker(ctx, &tid, attempt, adhoc);
        format!("{tid} starting (attempt {attempt})")
    }

    /// The slots of `role`'s class taken right now. A role with a cap of its own
    /// (`Role::parallel`, the swarm) counts only its own workers; every other role — and a name
    /// the pattern does not have — counts the whole shared pool against `max_parallel`.
    fn slots_taken(&self, role: &str) -> usize {
        let own = self.pattern.has_own_cap(role);
        self.workers.iter().filter(|w| holds_slot(w) && if own { w.task.role == role } else { !self.pattern.has_own_cap(&w.task.role) }).count()
    }

    /// How a journal line names `role`'s cap: `worker-swarm: 16 at once` for a class of its own,
    /// `max_parallel = 4` for the shared pool.
    fn class_cap_text(&self, role: &str) -> String {
        if self.pattern.has_own_cap(role) {
            format!("{role}: {} at once", self.pattern.cap_of(role))
        } else {
            format!("max_parallel = {}", self.pattern.settings.max_parallel)
        }
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
        let mut role = self.role(&w.task.role);
        // The model the user switched this task to (`set_agent_model`) outranks the role's.
        if let Some(m) = &w.model {
            role.model = m.clone();
        }
        // Codex's workspace-write sandbox keeps every `.git` read-only whatever the writable roots
        // say, so a fix worker on the integration copy (merging conflicted branches, unstaging a
        // file) could not touch the index. It runs unsandboxed; the worktree still isolates it.
        if w.adhoc && role.sandbox != "read-only" {
            role.sandbox = "danger-full-access".into();
        }
        let scope = if w.task.scope.is_empty() { "(not restricted)".to_string() } else { w.task.scope.join(", ") };
        let instructions = format!(
            "{}\n{}\n## Your task: {} — {}\nScope: {scope}\nAcceptance: {}\n",
            role.instructions,
            tools::WORKER_PROTOCOL,
            w.task.id,
            w.task.title,
            if w.task.acceptance.is_empty() { "the task is fully done and verified" } else { &w.task.acceptance }
        );
        let brief_context = worker_brief_context(&self.brief);
        let prompt = match &resume {
            Some(_) => format!("[mantra:resume] Mantra restarted while you were working on this task; your worktree still holds your changes. Continue exactly where you left off — do not start over.\n\nThe task again:\n{}\n\n(Context — overall project goal: {plan_summary})\n\n{brief_context}", w.prompt),
            None => format!("{}\n\n(Context — overall project goal: {plan_summary})\n\n{brief_context}", w.prompt),
        };
        let requested = w.effort.clone().or(w.task.effort.clone());
        let name = w.task.id.clone();
        let rname = w.task.role.clone();
        let glyph = role.glyph.clone();
        let context_override = self.context_for(&role.model, w.context_override);
        let resumed = resume.is_some();
        // A fix worker shares the integration copy; it needs the git dir writable only to merge
        // branches that conflicted.
        let extra_writable = if w.adhoc { self.integ_writable() } else { vec![] };
        let effort = self.effort_for(requested, &rname, &role, false);
        let thread_name = self.thread_name(&rname);
        let req = SpawnReq { name, role_name: rname.clone(), role, cwd: dir.clone(), instructions, tools: tools::worker_tools(), effort, extra_writable, context_override, thread_name, network: self.pattern.settings.network, writable_roots: self.pattern.settings.writable_roots.clone(), mcp: vec![] };
        let req = self.role_req(req);
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
        ctx.prompt(id, self.with_akasha(id, prompt));
        // how full its class is now, this one included: the width a run actually reaches
        let (taken, cap) = (self.slots_taken(&rname), self.pattern.cap_of(&rname));
        self.log(&glyph, "violet", format!("{} {tid} ({rname} · {taken}/{cap} at once)", if resumed { "re-attached" } else { "spawned" }));
        self.save_state();
    }

    pub(super) fn fill_slots(&mut self, ctx: &mut dyn Ctx) {
        if self.halted() {
            return;
        }
        // The whole queue in order, each task against its own class's cap: a queued swarm task
        // behind a big one that has to wait still starts. Class key: the role for a class of its
        // own, `None` for the shared pool.
        let mut taken: HashMap<Option<&str>, usize> = HashMap::new();
        let mut start: Vec<(String, u32, bool)> = vec![];
        for w in self.workers.iter().filter(|w| w.state == WState::Queued) {
            let role = w.task.role.as_str();
            let class = self.pattern.has_own_cap(role).then_some(role);
            let n = taken.entry(class).or_insert_with(|| self.slots_taken(role));
            if *n < self.pattern.cap_of(role) {
                *n += 1;
                start.push((w.task.id.clone(), w.attempt, w.adhoc));
            }
        }
        for (t, a, adhoc) in start {
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

    /// The gate checks of phase `idx` (a background job that also collects the phase's diff for
    /// the QA agent that follows). `round` 0 is the run before any QA; later rounds verify what a
    /// QA pass cleared.
    fn run_checks(&mut self, ctx: &mut dyn Ctx, idx: usize, round: u32) {
        self.launch_checks(ctx, idx, round, false);
    }

    /// The same checks after a fix worker finished gate round `round`: the next QA round follows
    /// whatever they say.
    fn run_checks_after_fix(&mut self, ctx: &mut dyn Ctx, idx: usize, round: u32) {
        self.launch_checks(ctx, idx, round, true);
    }

    fn launch_checks(&mut self, ctx: &mut dyn Ctx, idx: usize, round: u32, after_fix: bool) {
        let checks = self.current_phase().map(|p| p.gate.checks.clone()).unwrap_or_default();
        // Nothing to run and nothing to diff (no workspace) — or a verify with no checks at all,
        // which needs no diff either: straight on to what the result decides.
        if checks.is_empty() && (self.ws.is_none() || (round > 0 && !after_fix)) {
            self.checks.clear();
            if after_fix {
                self.spawn_gate_at_round(ctx, idx, round + 1);
            } else if round == 0 {
                self.spawn_gate(ctx, idx);
            } else {
                self.begin_handoff(ctx, idx);
            }
            return;
        }
        self.stage = Stage::Phase { idx, step: PhaseStep::Checks { round } };
        if !checks.is_empty() {
            self.log("⎔", "blue", format!("running {} gate check(s){}", checks.len(), if round > 0 || after_fix { " (verify)" } else { "" }));
        }
        let dir = self.workspace_dir();
        let timeout = Duration::from_secs(self.pattern.settings.check_timeout_secs);
        let ws = self.ws.clone();
        // Checks leave things behind (`__pycache__`, build scratch, a formatter's edits); only in
        // worktree mode does Mantra commit afterwards, so only there is the working copy tidied.
        let tidy = ws.as_ref().map(|w| w.worktree).unwrap_or(false);
        let tag = if after_fix { JobTag::FixChecks { phase: idx, round } } else { JobTag::Checks { phase: idx, round } };
        ctx.job(
            tag,
            Box::new(move || {
                let before = if tidy { git::status_snapshot(&dir) } else { None };
                let results = if checks.is_empty() { vec![] } else { git::run_checks(&dir, &checks, timeout) };
                let dropped = before.map(|b| git::discard_check_artifacts(&dir, &b)).unwrap_or_default();
                let diff = ws.as_ref().map(|w| git::review_diff(w, git::DiffSpan::Phase, GATE_DIFF_BYTES)).unwrap_or_default();
                JobOut::Checks(results, dropped, diff)
            }),
        );
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

    /// Archive the gate agent: every QA round is a fresh agent, so the one that reported is done.
    fn retire_gate_agent(&mut self, ctx: &mut dyn Ctx) {
        if let Some(g) = self.gate_agent.take() {
            self.tokens_prev += ctx.agent(g).map(|x| x.tokens_total).unwrap_or(0);
            ctx.stop(g, true);
        }
    }

    /// Gate round `round` of phase `idx`: a FRESH QA agent (new session, the previous one is
    /// archived) that gets the check results, the phase's diff and the previous round's verdict,
    /// and only verifies and reports — fixes go to a fix worker (`start_fix`). Also what
    /// `respawn_gate` (WP7.4) calls with the round it replaces, so a respawn does not reset the
    /// round counter: `gate_max_rounds` and the identical-blocker halt both key off `round`.
    fn spawn_gate_at_round(&mut self, ctx: &mut dyn Ctx, idx: usize, round: u32) {
        let Some(phase) = self.current_phase().cloned() else { return };
        if self.halted() {
            self.defer(Pending::Gate { idx, round, note: None });
            return;
        }
        self.retire_gate_agent(ctx);
        self.stage = Stage::Phase { idx, step: PhaseStep::Gate { round } };
        self.gate_report = None;
        self.gate_big = false;
        self.gate_nudges = 0;
        if round <= 1 {
            // loop protection counts across rounds; only the first round of a run of rounds starts it over
            self.last_gate_blocker = None;
            self.last_verify_sig = None;
        }
        let name = self.pattern.flow.phase_gate.clone();
        let role = self.role_for_slot("gate", &name);
        // Whatever the pattern's gate role says, Mantra never lets the gate agent fix: a writable
        // sandbox is told to hold back (a read-only one cannot write at all).
        let sandbox_note = if role.sandbox == "read-only" {
            "Your role cannot write files: do not try to edit or create anything."
        } else {
            "Do not edit anything: you verify and report, Mantra routes every fix to a worker."
        };
        let reports: String = self.workers.iter().filter(|w| w.state == WState::Done && !w.adhoc).map(|w| format!("### {} — {}\n{}\n", w.task.id, w.task.title, trunc(&w.report, 600))).collect();
        let fixes: String = self.workers.iter().filter(|w| w.state == WState::Done && w.adhoc).map(|w| format!("- {}: {}\n", w.task.id, trunc(&w.report, 600))).collect();
        let previous = match &self.prev_gate {
            Some((r, pass, sum)) => format!("\nPrevious QA round ({r}) said {}: {}\n", if *pass { "pass" } else { "fail" }, trunc(sum, 1200)),
            None => String::new(),
        };
        let previous = if fixes.is_empty() { previous } else { format!("{previous}{}\n{fixes}", if self.prev_gate.is_some() { "Fix workers since then:" } else { "\nFix workers (before this round):" }) };
        let prompt = format!(
            "[mantra:gate] Phase {}: {} — QA round {round}/{}\nGoal: {}\nGate criteria: {}\nQA focus: {}\n\nGate checks (run by Mantra just now):\n{}\n\nWorker reports:\n{reports}{previous}\nThe phase's diff (stat, then the diff — start here, read a file in full only if the diff cannot answer):\n{}\n\nVerify the result against the goal and the criteria, then call mantra_gate_report. You do not fix anything: report each problem with file, what is wrong and what to change.",
            idx + 1,
            phase.name,
            self.pattern.settings.gate_max_rounds,
            phase.goal,
            if phase.gate.criteria.is_empty() { "build + tests pass, code coherent" } else { &phase.gate.criteria },
            if phase.gate.focus.is_empty() { "integration and consistency between the parallel changes" } else { &phase.gate.focus },
            self.check_summary(),
            self.phase_diff.render()
        );
        let prompt = format!("{prompt}\n\n{}", worker_brief_context(&self.brief));
        let prompt = match self.resume_note.take() {
            Some(n) => format!("{prompt}\n\n{n}"),
            None => prompt,
        };
        let id = ctx.spawn(self.role_req(SpawnReq {
            name: name.clone(),
            effort: self.effort_for(None, &name, &role, false),
            thread_name: self.thread_name(&name),
            role_name: name.clone(),
            instructions: format!("{}\n{}\n{sandbox_note}", role.instructions, tools::GATE_PROTOCOL),
            cwd: self.workspace_dir(),
            tools: tools::gate_tools(false, &self.pattern.worker_roles()),
            extra_writable: self.integ_writable(),
            context_override: self.context_for(&role.model, None),
            network: self.pattern.settings.network,
            writable_roots: self.pattern.settings.writable_roots.clone(),
            mcp: vec![],
            role: role.clone(),
        }));
        self.gate_agent = Some(id);
        self.mark_edge(id);
        ctx.prompt(id, self.with_akasha(id, prompt));
        self.log(&role.glyph, "green", format!("{name} gate started (round {round}, fresh agent)"));
        self.save_state();
    }

    /// Remember a gate verdict: the journal line later reviewers start from, and the previous-round
    /// note the next fresh QA agent is shown.
    fn note_gate(&mut self, idx: usize, round: u32, pass: bool, summary: &str) {
        let line = format!("phase {} · QA round {round}: {} — {}", idx + 1, if pass { "pass" } else { "fail" }, trunc(summary.replace('\n', " ").trim(), 400));
        self.gate_notes.push(line.clone());
        self.prev_gate = Some((round, pass, summary.to_string()));
        let _ = std::fs::create_dir_all(&self.dir);
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(self.dir.join("gate-reports.md")) {
            use std::io::Write;
            let _ = writeln!(f, "- {line}");
        }
    }

    /// The worker role a fix goes to: `worker-big` when the QA report flagged the fixes as big,
    /// `worker-small` otherwise; a custom pattern without those names gets the closest worker role.
    fn fix_role(&self, big: bool) -> String {
        let want = if big { "worker-big" } else { "worker-small" };
        let roles = self.pattern.worker_roles();
        if roles.iter().any(|r| r == want) {
            return want.into();
        }
        let hint = if big { "big" } else { "small" };
        roles.iter().find(|r| r.to_lowercase().contains(hint)).or_else(|| roles.first()).cloned().unwrap_or_else(|| want.into())
    }

    /// Route what gate round `round` found to a fix worker (`fix-<round>`, scoped to the files the
    /// findings name) and wait for it: when it is done Mantra re-runs the checks and starts the
    /// next QA round (`on_fix_done`). The caller has checked `round < gate_max_rounds`.
    fn start_fix(&mut self, ctx: &mut dyn Ctx, idx: usize, round: u32, findings: String, big: bool) {
        if self.halted() {
            self.defer(Pending::Fix { idx, round, findings: trunc(&findings, 6000), big });
            return;
        }
        let Some(phase) = self.current_phase().cloned() else { return };
        self.retire_gate_agent(ctx);
        let role = self.fix_role(big);
        let scope = paths_in(&findings, &self.workspace_dir());
        let checks = if phase.gate.checks.is_empty() { String::new() } else { format!("\nThe gate will re-run: {}", phase.gate.checks.join(" ; ")) };
        let prompt = format!(
            "[mantra:fix] Phase {} ({}), {}. The gate found the problems below. Fix exactly these — no refactors, no extras — and stop.\n\nFindings:\n{}\n\nYou work in the integrated copy of the phase: every worker's changes are already merged here. Stay with the files the findings name{}. Before you finish run only what covers what you touched, once.{checks}\nEnd with the STATUS and SUMMARY lines.",
            idx + 1,
            phase.name,
            if round == 0 { "before the first QA round".to_string() } else { format!("gate round {round}") },
            trunc(findings.trim(), 6000),
            if scope.is_empty() { String::new() } else { format!(" ({})", scope.join(", ")) }
        );
        let what = if round == 0 { "Merge the conflicting branches".to_string() } else { format!("Fix what gate round {round} found") };
        let task = Task { id: format!("fix-{round}"), title: what, role: role.clone(), prompt, scope, acceptance: "every finding is resolved and nothing else changed".into(), effort: None };
        self.stage = Stage::Phase { idx, step: PhaseStep::Fix { round } };
        if round == 0 {
            self.log("⚒", "amber", format!("branches did not merge — fix-0 ({role}) merges them before QA"));
        } else {
            self.log("⚒", "amber", format!("gate round {round} found problems — fix-{round} ({role}) takes them"));
        }
        let r = self.spawn_task(ctx, task, None, None);
        if r.starts_with("REFUSED") {
            self.log("!", "amber", format!("fix-{round}: {}", trunc(&r, 100)));
        }
        self.save_state();
    }

    /// The fix worker of gate round `round` finished: re-run the checks (the next QA round
    /// follows the result, `JobTag::FixChecks`).
    fn on_fix_done(&mut self, ctx: &mut dyn Ctx, idx: usize, round: u32) {
        let report = self.workers.iter().rev().find(|w| w.task.id == format!("fix-{round}")).map(|w| w.report.clone()).unwrap_or_default();
        if !report.trim().is_empty() {
            self.gate_notes.push(format!("phase {} · fix-{round}: {}", idx + 1, trunc(report.replace('\n', " ").trim(), 300)));
        }
        // a merge-conflict fix has merged the branches by hand
        self.conflicts.clear();
        self.run_checks_after_fix(ctx, idx, round);
    }

    /// A halt was lifted while a fix worker owned the phase: pick up where it is.
    fn resume_fix(&mut self, ctx: &mut dyn Ctx, idx: usize, round: u32) {
        let id = format!("fix-{round}");
        let Some(w) = self.workers.iter().rev().find(|w| w.task.id == id) else {
            // never got a worker: nothing to wait for, check what is there
            self.run_checks_after_fix(ctx, idx, round);
            return;
        };
        match w.state {
            WState::Done => self.on_fix_done(ctx, idx, round),
            WState::Failed(_) | WState::Cancelled => {
                let (agent, task, prompt, effort) = (w.agent, w.task.clone(), w.prompt.clone(), w.effort.clone());
                self.log("↻", "violet", format!("{id} starts over"));
                match agent {
                    Some(a) => {
                        let prompt = self.resume_note.take().map(|n| join_note(Some(n), self.workers.iter().rev().find(|w| w.agent == Some(a)).map(|w| w.prompt.clone()).unwrap_or_default()));
                        self.retry_worker(ctx, a, prompt);
                    }
                    None => {
                        self.spawn_task(ctx, task, Some(prompt), effort);
                    }
                }
            }
            // still working: the halt's "continue where you left off" re-prompt reaches it
            _ => {}
        }
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
        self.completed_agents.extend(self.workers.iter().filter(|w| w.state == WState::Done).filter_map(|w| w.agent));
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
        self.launch_phase_commit(ctx, idx);
        match self.orchestrator {
            Some(o) => {
                self.mark_edge(o);
                ctx.prompt(o, "[mantra:handoff] The phase passed its gate. Write the handoff note for the next phase (≤10 lines). Don't call tools.".into());
            }
            None => self.handoff_note_done = true,
        }
        self.save_state();
    }

    /// Also used on explicit resume after a refused phase commit; no repeated archiving.
    pub(super) fn launch_phase_commit(&mut self, ctx: &mut dyn Ctx, idx: usize) {
        let ws = self.ws.clone();
        let wts: Vec<(PathBuf, String)> = self.workers.iter().filter(|w| !w.adhoc).filter_map(|w| w.wt.clone().map(|d| (d, w.branch.clone()))).collect();
        let phase_name = self.current_phase().map(|p| p.name.clone()).unwrap_or_default();
        let msg = format!("mantra: phase {} — {}", idx + 1, phase_name);
        ctx.job(JobTag::Cleanup { phase: idx }, Box::new(move || {
            let Some(ws) = ws else { return JobOut::Cleanup(Ok(String::new()), git::ReviewDiff::default()) };
            let committed = git::phase_commit(&ws, &msg);
            if committed.is_ok() {
                for (d, b) in wts { git::remove_worker(&ws, &d, &b, false); }
            }
            let diff = git::review_diff(&ws, git::DiffSpan::Run, FINALE_DIFF_BYTES);
            JobOut::Cleanup(committed.map(|c| if c { "committed".into() } else { "nothing to commit".into() }), diff)
        }));
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
            self.defer(Pending::Finale { idx: k, note: None });
            return;
        }
        self.workers.retain(|w| w.adhoc && !matches!(w.state, WState::Done | WState::Cancelled));
        let steps = self.pattern.flow.finale.clone();
        let Some(step) = steps.get(k).cloned() else {
            self.finish(ctx);
            return;
        };
        // A model switched live on a finale agent was a choice for that step's role: the next
        // step (another role) starts on its own model, a respawn of the same step keeps it.
        if !matches!(self.stage, Stage::Finale { idx } if idx == k) {
            self.slot_models.remove("finale");
        }
        self.stage = Stage::Finale { idx: k };
        self.gate_report = None;
        self.phase_started = Instant::now();
        let role = self.role_for_slot("finale", &step.role);
        // A read-only finale role (the built-in "planner" step included) needs to be told it cannot
        // write; with `may_spawn` its defects go to fix workers, otherwise into the report. A role
        // that can write is left to its own instructions.
        let sandbox_note = match (role.sandbox == "read-only", step.may_spawn) {
            (true, true) => "Your role cannot write files: do not try to edit or create anything. Have each real defect fixed by a fix worker (mantra_spawn_adhoc), then mantra_wait.",
            (true, false) => "Your role cannot write files: do not try to edit or create anything. Report every needed change precisely (file, what, why) in mantra_gate_report as fail with a fix list.",
            (false, _) => "",
        };
        let plan = self.plan.clone().unwrap_or_default();
        let final_checks = if plan.final_checks.is_empty() { "(none defined)".to_string() } else { plan.final_checks.join("\n") };
        let reports = if self.gate_notes.is_empty() { "(none)".to_string() } else { self.gate_notes.iter().map(|n| format!("- {n}")).collect::<Vec<_>>().join("\n") };
        let diff = match &self.run_diff {
            Some(d) => d.render(),
            None => "(not captured — read it from git: `git log --stat` and `git diff` against the commit the run started from)".into(),
        };
        let prompt = format!(
            "[mantra:finale] Step {}/{}: {}\n\nOriginal request:\n{}\n\nPlan summary: {}\nFinal checks:\n{final_checks}\n\nGate and finale reports so far:\n{reports}\n\nThe run's diff as of the last phase commit (stat, then the diff — start here, read a file in full only if the diff cannot answer; later fixes show in `git diff` and `git status`):\n{diff}\n\nWork in: {}\n{}When done, call mantra_gate_report.",
            k + 1,
            steps.len(),
            step.task,
            self.brief,
            plan.summary,
            self.workspace_dir().display(),
            if step.may_spawn { "You may spawn ad-hoc workers with mantra_spawn_adhoc, then mantra_wait until they finish.\n" } else { "" }
        );
        // The planner verifies with its full context of the plan; other roles get a fresh agent.
        let effort = self.effort_for(None, &step.role, &role, true);
        let agent = if step.role == self.pattern.flow.planner && self.planner.is_some() {
            let p = self.planner.unwrap_or_default();
            if let Some(e) = &effort {
                // the planner is already running at its planning effort: a finale step is a harder look
                ctx.set_effort(p, e);
            }
            p
        } else {
            ctx.spawn(self.role_req(SpawnReq {
                name: step.role.clone(),
                effort,
                thread_name: self.thread_name(&step.role),
                role_name: step.role.clone(),
                instructions: format!("{}\n{}\n{sandbox_note}", role.instructions, tools::GATE_PROTOCOL),
                cwd: self.workspace_dir(),
                tools: tools::gate_tools(step.may_spawn, &self.pattern.worker_roles()),
                extra_writable: self.integ_writable(),
                context_override: self.context_for(&role.model, None),
                network: self.pattern.settings.network,
                writable_roots: self.pattern.settings.writable_roots.clone(),
                mcp: vec![],
                role: role.clone(),
            }))
        };
        if let Some(prev) = self.finale_agent {
            if Some(prev) != self.planner && prev != agent {
                self.tokens_prev += ctx.agent(prev).map(|x| x.tokens_total).unwrap_or(0);
                ctx.stop(prev, true);
            }
        }
        self.finale_agent = Some(agent);
        self.mark_edge(agent);
        ctx.prompt(agent, self.with_akasha(agent, prompt));
        self.log(&role.glyph, "saffron", format!("finale {}/{} — {}", k + 1, steps.len(), step.role));
        self.save_state();
    }

    fn finish(&mut self, ctx: &mut dyn Ctx) {
        self.stage = Stage::Done;
        self.finished_unix = Some(crate::util::unix_secs());
        self.clock_stop();
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
        self.end_failed(why);
    }

    /// Stop the run for good, by the user's hand (`/stop`, the web's Stop run): interrupt what is
    /// busy and shut down every agent of the run, cancel the workers still queued or in flight,
    /// and end it as `Failed(STOPPED_BY_USER)` — shown as "stopped", saved and journaled, and no
    /// longer active, so a new run can start straight away. Its branch and worktrees stay: like
    /// any failed run it reopens read-only from `/runs`, to land or delete. No notification —
    /// the user pressed the key.
    pub fn stop(&mut self, ctx: &mut dyn Ctx) {
        if !self.is_active() {
            return;
        }
        for a in self.all_agents() {
            if ctx.agent(a).map(|x| x.busy()).unwrap_or(false) {
                ctx.interrupt(a);
            }
            // not archived: the threads stay readable from the stopped run
            ctx.stop(a, false);
        }
        for w in self.workers.iter_mut().filter(|w| matches!(w.state, WState::Queued | WState::Preparing | WState::Running | WState::Retrying(_))) {
            w.state = WState::Cancelled;
        }
        // a halt the user stopped instead of resuming is over, and nothing is waiting on anyone
        self.halt = None;
        self.paused_agents.clear();
        self.alerts.clear();
        self.log("■", "amber", "run stopped by you");
        self.end_failed(STOPPED_BY_USER.into());
    }

    /// Where every failed run ends: the stage, the time it ended, nothing left pending, the clock
    /// stopped, the state saved (which releases the run's owner file too).
    fn end_failed(&mut self, why: String) {
        self.stage = Stage::Failed(why);
        self.finished_unix = Some(crate::util::unix_secs());
        self.drop_pending("the run stopped");
        self.clock_stop();
        self.save_state();
    }

    fn alert(&mut self, ctx: &mut dyn Ctx, text: String) {
        self.log("⚑", "amber", text.clone());
        self.alerts.push(text.clone());
        ctx.notify(&format!("Mantra needs you: {text}"));
    }

    // ───────────────────────────── inputs ─────────────────────────────

    pub fn on_job(&mut self, ctx: &mut dyn Ctx, tag: JobTag, out: JobOut) {
        // A failed or stopped run starts nothing more: a setup, merge or checks job still in
        // flight when it ended would spawn a planner or a gate into it. `/land` on it still works,
        // and a vision call is answered (to an agent that is shut down anyway).
        if matches!(self.stage, Stage::Failed(_)) && !matches!(tag, JobTag::Land | JobTag::Vision { .. }) {
            return;
        }
        match (tag, out) {
            (JobTag::Setup, JobOut::Setup(r)) => match r {
                Ok(ws) => {
                    if let Some(n) = &ws.note {
                        self.log("ℹ", "amber", n.clone());
                    }
                    if ws.initial_commit {
                        self.log("⎇", "blue", "created the initial commit so workers get isolated worktrees");
                    }
                    self.log("⎇", "blue", if ws.worktree { format!("isolated worktrees on branch {}", ws.branch) } else { "shared workspace".into() });
                    self.ws = Some(ws);
                    self.stage = Stage::Planning;
                    let p = self.spawn_planner(ctx);
                    self.mark_edge(p);
                    let prompt = self.with_akasha(p, self.planning_prompt());
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
                let _ = std::fs::write(self.dir.join(format!("phase-{}-merge.log", phase + 1)), &m.log);
                if !m.blocked.is_empty() {
                    self.stage = Stage::Phase { idx: phase, step: PhaseStep::Orchestrating };
                    let detail = format!("phase {} integration blocked:\n{}\nRestore protected decision headings or fix the project commit policy before explicitly resuming. Worker branches and worktrees are retained; do not bypass the safeguard with a manual merge.", phase + 1, m.blocked.join("\n"));
                    self.halt_and_escalate_full(ctx, HaltReason::GateExhausted, None, format!("phase {} integration blocked by commit/policy failure", phase + 1), detail, None);
                    return;
                }
                if !m.merged.is_empty() {
                    self.log("⇲", "green", format!("merged {}", m.merged.join(", ")));
                }
                if !m.conflicts.is_empty() {
                    self.log("⚠", "amber", format!("{} merge conflict(s) → handed to the gate agent", m.conflicts.len()));
                }
                self.conflicts = m.conflicts;
                self.run_checks(ctx, phase, 0);
            }
            (JobTag::Checks { phase, round }, JobOut::Checks(results, dropped, diff)) => {
                let all_ok = self.record_checks(results, dropped, diff);
                if round == 0 {
                    // Merge conflicts are not for a read-only QA agent to resolve: a fix worker
                    // merges the branches, then the gate runs on the result.
                    if !self.conflicts.is_empty() {
                        let list = self.conflicts.join("\n");
                        let findings = format!("These branches could NOT be merged automatically (conflicts). Merge each into the integrated copy yourself with `git merge <branch>`, resolve the conflicts so the result builds and keeps both sides' intent, and commit:\n{list}");
                        // round 0: the merge fix comes before QA round 1, it does not use one up
                        self.start_fix(ctx, phase, 0, findings, true);
                    } else {
                        self.spawn_gate(ctx, phase);
                    }
                } else if all_ok {
                    self.begin_handoff(ctx, phase);
                } else if round < self.pattern.settings.gate_max_rounds {
                    // The very same failure as last round means the fix worker isn't fixing it —
                    // typically a check that cannot pass on this machine. Don't burn the
                    // remaining rounds: escalate now.
                    let sig = failing_sig(&self.checks);
                    if self.last_verify_sig.as_deref() == Some(sig.as_str()) {
                        let gate = self.gate_agent;
                        let cmds = failing_cmds(&self.checks);
                        self.halt_and_escalate_full(
                            ctx,
                            HaltReason::GateExhausted,
                            gate,
                            format!("phase {} gate: the same check(s) fail identically after two gate rounds — {}", phase + 1, trunc(&cmds, 80)),
                            format!("phase {} gate: the same check(s) fail identically after two gate rounds — {cmds}", phase + 1),
                            None,
                        );
                        return;
                    }
                    self.last_verify_sig = Some(sig);
                    let findings = format!("The QA agent passed the phase but these gate checks still fail:\n{}", self.check_summary());
                    self.start_fix(ctx, phase, round, findings, false);
                } else {
                    let gate = self.gate_agent;
                    let cmds = failing_cmds(&self.checks);
                    self.halt_and_escalate_full(
                        ctx,
                        HaltReason::GateExhausted,
                        gate,
                        format!("phase {} gate still failing after {} rounds: {}", phase + 1, round, trunc(&cmds, 80)),
                        format!("phase {} gate still failing after {} rounds: {cmds}", phase + 1, round),
                        None,
                    );
                }
            }
            (JobTag::FixChecks { phase, round }, JobOut::Checks(results, dropped, diff)) => {
                let all_ok = self.record_checks(results, dropped, diff);
                if !all_ok {
                    // A fix that leaves the very same failure behind did nothing: stop here.
                    let sig = failing_sig(&self.checks);
                    if self.last_verify_sig.as_deref() == Some(sig.as_str()) {
                        self.halt_and_escalate(ctx, HaltReason::GateExhausted, self.gate_agent, format!("phase {} gate: the same check(s) fail identically after fix-{round} — {}", phase + 1, trunc(&failing_cmds(&self.checks), 80)));
                        return;
                    }
                    self.last_verify_sig = Some(sig);
                }
                self.spawn_gate_at_round(ctx, phase, round + 1);
            }
            (JobTag::Cleanup { phase }, JobOut::Cleanup(r, diff)) => {
                if let Err(e) = r {
                    self.cleanup_done = false;
                    self.halt_and_escalate_full(ctx, HaltReason::GateExhausted, None, format!("phase {} commit failed", phase + 1), format!("phase {} commit failed: {e}\nWorker branches/worktrees are retained. Correct the commit policy or restore protected decision headings, then explicitly resume to retry the phase commit.", phase + 1), None);
                    return;
                }
                self.run_diff = Some(diff);
                self.cleanup_done = true;
                self.maybe_next_phase(ctx);
            }
            (JobTag::Land, JobOut::Text(r)) => match r {
                Ok(m) => self.log("⇲", "green", m),
                Err(e) => self.alert(ctx, e),
            },
            (JobTag::Vision { agent, req, tool, name }, JobOut::Vision(items, ok)) => {
                let what = if tool == "mantra_screenshot" { "took a screenshot" } else { "looked at an image" };
                self.log("▣", "blue", format!("{name} {what}{}", if ok { "" } else { " — failed" }));
                ctx.tool_result_items(agent, req, items, ok);
            }
            (JobTag::Finish, _) => {}
            _ => {}
        }
    }

    /// Take a checks run's results: journal them, keep them and the phase diff for the next gate
    /// agent. True when every check passed (or there were none).
    fn record_checks(&mut self, results: Vec<CheckResult>, dropped: Vec<String>, diff: git::ReviewDiff) -> bool {
        for c in &results {
            self.log(if c.ok { "✓" } else { "✗" }, if c.ok { "green" } else { "red" }, format!("check `{}` {} ({}s)", trunc(&c.cmd, 40), if c.ok { "passed" } else { "failed" }, c.secs));
        }
        if !dropped.is_empty() {
            self.log("⌫", "blue", format!("dropped {} file(s) left behind by checks", dropped.len()));
        }
        let all_ok = results.iter().all(|c| c.ok);
        self.checks = results;
        self.phase_diff = diff;
        all_ok
    }

    /// A worker that hit ContextFull while running on the *assumed* 200k window (no explicit
    /// `context_window` set for its model) gets the window it was told halved for its next attempt
    /// — the assumed one, or `context_cap` when that is smaller. A model with an explicit setting
    /// is left alone — the user made that choice deliberately.
    fn shrink_assumed_context(&mut self, ctx: &mut dyn Ctx, a: AgentId) {
        let Some(wi) = self.worker_idx(a) else { return };
        if self.workers[wi].context_override.is_some() {
            return; // already shrunk once for this attempt's live process
        }
        let Some(agent) = ctx.agent(a) else { return };
        let Some(told) = agent.ctx_window else { return };
        let model = agent.model_alias.clone();
        // What the registry says decides "assumed", not the window told: a capped 1M model is
        // told 200k without assuming anything, and an assumed one under a 150k cap is told 150k.
        // Never told (tests): the window itself is the only reading.
        let assumed = match self.known_models.get(&model) {
            Some(w) => *w == crate::discover::ASSUMED_CONTEXT,
            None => told == crate::discover::ASSUMED_CONTEXT,
        };
        if !assumed {
            return; // an explicit context_window is in effect — leave it to the user
        }
        let halved = told / 2;
        self.workers[wi].context_override = Some(halved);
        self.log("⚠", "amber", format!("context window assumed {}k was too large for {model}; using {}k — set it in /models", told / 1000, halved / 1000));
    }

    pub fn on_turn_done(&mut self, ctx: &mut dyn Ctx, a: AgentId, status: &str, error: Option<String>, kind: Option<ErrKind>) {
        // A turn that ended after `mantra_wait` is a finished turn, even when the interrupt that
        // refused a follow-up tool call made it report "interrupted": it is neither logged nor
        // counted as one. The turn's yield state ends here, before anything else looks at the turn.
        let was_yielded = self.yielded.remove(&a);
        self.yield_interrupted.remove(&a);
        self.last_status.remove(&a);
        let status = if was_yielded && status == "interrupted" { "completed" } else { status };
        if !self.is_active() {
            return;
        }
        // A stopped process may deliver its final error after its replacement has started.
        // It no longer owns a live slot and must not restore the failure we just recovered.
        if self.killed_agents.contains(&a) || !self.all_agents().contains(&a) || self.worker_idx(a).map(|wi| matches!(self.workers[wi].state, WState::Cancelled)).unwrap_or(false) {
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
            // The process is already relaunching on its own (hub.rs's crash-backoff raced this
            // turn before it could even start) — a free retry, not a fault of this attempt's
            // budget, so it must not spend one of `worker_retries`.
            // A resumed Claude session found unusable (hub/claude.rs's dead-resume detection): the
            // hub is already relaunching a *fresh* process, which has none of the old thread's
            // context — so re-brief the agent as a respawn instead of counting a failure or halting.
            if msg.starts_with("resumed session is unusable") {
                let name = self.name_of(a);
                self.log("↻", "amber", format!("{name}: resumed session unusable — respawning with a fresh brief"));
                self.retry_counts.remove(&a);
                if let Err(e) = self.respawn(ctx, a, Some("Your previous process lost its session; this is a fresh start with the same task.".into())) {
                    self.log("!", "amber", format!("{name}: could not respawn ({e})"));
                }
                return;
            }
            // The model rejected the requested reasoning effort and says which ones it offers: the
            // nearest allowed one is set and the turn retried — once, so a model that rejects
            // that too falls through to the ordinary handling below.
            if let Some(allowed) = crate::agent::effort_rejection(&msg) {
                if self.remap_effort(ctx, a, &allowed) {
                    return;
                }
            }
            // Quota and billing errors are sometimes reported as transient or bad-request
            // failures. A configured replacement gets a fresh process before any retry/halt.
            let kind = if crate::agent::is_usage_limit_error(kind, &msg) { Some(ErrKind::UsageLimit) } else { kind };
            if kind == Some(ErrKind::UsageLimit) && self.fallback_after_usage_limit(ctx, a, &msg) {
                return;
            }
            // A dead or flaky gateway is nobody's fault: it never costs an attempt, so it is
            // handled here, before the counted retry ladder.
            let kind = if crate::agent::is_transport_error(kind, &msg) { Some(ErrKind::Transient) } else { kind };
            if kind == Some(ErrKind::Transient) {
                self.transport_errs.insert(a, Instant::now());
                if let Some(wi) = self.worker_idx(a) {
                    self.on_gateway_failure(ctx, wi, &msg);
                    return;
                }
            }
            if msg == "agent restarting" {
                let wait = Duration::from_secs(4);
                let name = self.name_of(a);
                if let Some(wi) = self.worker_idx(a) {
                    self.workers[wi].state = WState::Retrying(Instant::now() + wait);
                }
                self.log("↻", "amber", format!("{name}: relaunching — retrying automatically"));
                self.continue_queue.push((a, Instant::now() + wait, "[mantra:retry] Your previous turn failed to start while the process was restarting. Continue exactly where you left off.".into()));
                return;
            }
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
                if reason == HaltReason::AgentTurnFailed && kind == Some(ErrKind::Transient) && self.gateway_down(ctx, a) {
                    // every agent that was working failed on the transport within the minute: the
                    // gateway is down, for the manager too — nobody but the user is told
                    self.halt_for_outage(ctx, reason, Some(a), format!("{n}: {}", trunc(&msg, 120)), format!("{n}: {msg}"), GATEWAY_DOWN_HINT);
                    self.retry_counts.remove(&a);
                    return;
                }
                // The pulse line keeps the short form; the journal, `state.json` and the escalation
                // get the backend's whole error.
                let (detail, full) = if reason == HaltReason::ProviderRejected {
                    let (alias, provider) = ctx.agent(a).map(|ag| (ag.model_alias.clone(), ag.provider.clone())).unwrap_or_default();
                    let role = self.role_name_of(a).unwrap_or_default();
                    (format!("{n} [{role}] ({alias} via {provider}): {}", trunc(&msg, 120)), format!("{n} [{role}] ({alias} via {provider}): {msg}"))
                } else {
                    (format!("{n}: {}", trunc(&msg, 120)), format!("{n}: {msg}"))
                };
                if reason == HaltReason::AgentTurnFailed {
                    self.halt_turn_failed_full(ctx, Some(a), detail, full);
                } else if !self.halted() {
                    self.halt_full(ctx, reason, Some(a), detail, full);
                }
                self.retry_counts.remove(&a);
                return;
            }
            // worker with a non-retryable error or out of retries: falls through → reported to the orchestrator
        } else if status == "completed" {
            self.retry_counts.remove(&a);
            self.gateway_streak.remove(&a);
            self.effort_retried.remove(&a);
            self.transport_errs.remove(&a);
            if let Some(tag) = self.watchdog_role_tag(a) {
                self.turn_fail_retried.remove(&tag);
            }
            if let Some(key) = self.first_token_key(a) {
                self.first_token_strikes.remove(&key);
            }
        }

        if Some(a) == self.planner && self.planner_refresh_pending && status == "completed" {
            self.planner_refresh_pending = false;
            self.respawn_planner(ctx, Some("The workflow changed; continue with its updated role instructions and tools.".into()));
            return;
        }
        if Some(a) == self.planner && (self.planner_wait.is_some() || self.pending_pattern.is_some()) {
            return; // Includes the planner's finale slot: waiting is never a verification pass.
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
            if self.planner_wait.is_some() || self.pending_pattern.is_some() { return; }
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
                if self.question.is_some() || self.pending_pattern.is_some() || self.planner_wait.is_some() {
                    return; // it asked the user something first; the answer restarts it
                }
                self.planner_nudges += 1;
                if self.planner_nudges > 3 {
                    self.fail_run(ctx, "planner did not submit a valid plan".into());
                } else {
                    self.mark_edge(a);
                    ctx.prompt(a, self.with_akasha(a, "[mantra] You haven't submitted a valid plan yet. Call mantra_submit_plan with the full plan now.".into()));
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
                        self.note_gate(idx, round, true, &summary);
                        self.log("◎", "green", format!("gate report: pass — {}", trunc(&summary, 70)));
                        self.run_checks(ctx, idx, round);
                    }
                    // the halt interrupted the round before a report: the resume picks the same
                    // round back up (`resume_by`), it is not a round that failed
                    None if self.halted() => self.log("‖", "amber", format!("gate round {round} interrupted by the halt — it goes on when the run resumes")),
                    // The QA agent ended its turn without its verdict: ask for it (twice at most) —
                    // there is nothing to hand a fix worker yet.
                    None => {
                        if self.gate_nudges < 2 {
                            self.gate_nudges += 1;
                            self.log("◎", "amber", format!("gate round {round}: no report — asking the QA agent for its verdict"));
                            self.mark_edge(a);
                            ctx.prompt(a, self.with_akasha(a, "[mantra:gate] You ended your turn without calling mantra_gate_report. Call it now with your verdict (pass, or fail with every finding: file, what is wrong, what to change).".into()));
                        } else {
                            self.halt_and_escalate(ctx, HaltReason::GateExhausted, Some(a), format!("phase {} gate round {round}: the QA agent ended its turn without a report, twice", idx + 1));
                        }
                    }
                    Some((false, why)) => {
                        let big = self.gate_big;
                        self.note_gate(idx, round, false, &why);
                        // L4 loop protection (WP12.4/WP6): two consecutive gate reports blocked on
                        // the same thing will never resolve themselves — halt now instead of
                        // spending the remaining rounds repeating the same failed fix.
                        let sig = trunc(why.trim(), 80);
                        let repeated = !sig.is_empty() && self.last_gate_blocker.as_deref() == Some(sig.as_str());
                        self.last_gate_blocker = Some(sig);
                        if repeated {
                            self.halt_and_escalate_full(
                                ctx,
                                HaltReason::GateExhausted,
                                Some(a),
                                format!("phase {} gate stuck on the same blocker twice: {}", idx + 1, trunc(&why, 80)),
                                format!("phase {} gate stuck on the same blocker twice: {why}", idx + 1),
                                None,
                            );
                        } else if round < self.pattern.settings.gate_max_rounds {
                            self.gate_report = None;
                            self.log("◎", "amber", format!("gate round {} not passed: {}", round, trunc(&why, 60)));
                            // The fix goes to a worker; Mantra re-runs the checks and starts a
                            // fresh QA round when it is done (deferred if the run is halted).
                            self.start_fix(ctx, idx, round, why, big);
                        } else {
                            self.halt_and_escalate_full(
                                ctx,
                                HaltReason::GateExhausted,
                                Some(a),
                                format!("phase {} gate not passed after {round} rounds: {}", idx + 1, trunc(&why, 80)),
                                format!("phase {} gate not passed after {round} rounds: {why}", idx + 1),
                                None,
                            );
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
                    let who = self.pattern.flow.finale.get(idx).map(|st| st.role.clone()).unwrap_or_default();
                    self.gate_notes.push(format!("finale {} · {who}: {} — {}", idx + 1, if pass { "pass" } else { "fail" }, trunc(s.replace('\n', " ").trim(), 400)));
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

    /// How many gateway failures in a row a worker rides out on its own before its failure is
    /// reported (and, in the meantime, none of them costs an attempt).
    const GATEWAY_RETRIES: u32 = 10;

    /// A worker's turn failed on the model gateway (HTTP 5xx, "upstream provider returned an
    /// error", a reset connection, …) — an outage, not something the worker did. It never spends
    /// one of `worker_retries`: while the run is going it retries with a short backoff; when the
    /// run is halted (or the outage outlasts the backoff) it is marked failed *because of the
    /// gateway* — which halts the run for the user, escalated to nobody (`on_worker_done`) — and
    /// the next resume respawns it (`respawn_after_outage`).
    fn on_gateway_failure(&mut self, ctx: &mut dyn Ctx, wi: usize, msg: &str) {
        let (a, tid) = match (self.workers[wi].agent, self.workers[wi].paused, &self.workers[wi].state) {
            (Some(a), false, s) if *s != WState::Cancelled => (a, self.workers[wi].task.id.clone()),
            _ => return,
        };
        if self.halted() {
            self.workers[wi].state = WState::Failed(msg.to_string());
            self.workers[wi].transient = true;
            self.log("✗", "red", format!("{tid} failed on the model gateway: {} — it is respawned when the run resumes", trunc(msg, 60)));
            self.save_state();
            return;
        }
        let n = {
            let n = self.gateway_streak.entry(a).or_insert(0);
            *n += 1;
            *n
        };
        if n <= Self::GATEWAY_RETRIES {
            let wait = Duration::from_secs((4 * n as u64).min(30));
            self.workers[wi].state = WState::Retrying(Instant::now() + wait);
            self.log("↻", "amber", format!("{tid}: gateway error ({}) → retrying in {}s — not counted as an attempt", trunc(msg, 50), wait.as_secs()));
            self.continue_queue.push((a, Instant::now() + wait, format!("[mantra:retry] Your previous turn failed on the model gateway ({msg}) — not your fault. Continue exactly where you left off.")));
            return;
        }
        // the outage outlasted the backoff: report it, but a resume (or a retry) is still free
        self.gateway_streak.remove(&a);
        self.workers[wi].transient = true;
        self.on_worker_done(ctx, wi, "failed", Some(msg.to_string()));
    }

    /// `resume`: every worker whose last failure was the gateway's goes back to work in its own
    /// worktree (a fix worker: on the integration copy, where `spawn_task` puts every `fix-*`).
    /// The live agent is simply told to carry on; one whose process is gone is started again —
    /// for free, that outage was not an attempt.
    fn respawn_after_outage(&mut self, ctx: &mut dyn Ctx) {
        let down: Vec<usize> = self.workers.iter().enumerate().filter(|(_, w)| w.transient && matches!(w.state, WState::Failed(_))).map(|(i, _)| i).collect();
        for wi in down {
            let tid = self.workers[wi].task.id.clone();
            self.log("↻", "violet", format!("respawning {tid} after the gateway outage"));
            self.workers[wi].transient = false;
            let alive = self.workers[wi].agent.filter(|a| ctx.agent(*a).map(|x| !matches!(x.status, Status::Crashed(_) | Status::Stopped)).unwrap_or(false));
            match alive {
                Some(a) => {
                    self.gateway_streak.remove(&a);
                    self.workers[wi].state = WState::Running;
                    self.workers[wi].finished = None;
                    self.mark_edge(a);
                    ctx.prompt(a, self.with_akasha(a, format!("[mantra:resume] The model gateway was down and your last turn failed; the run is resuming now. Your worktree still holds your changes. Continue exactly where you left off — do not start over.\n\nThe task again:\n{}", self.workers[wi].prompt)));
                }
                None => {
                    let (task, prompt, effort) = (self.workers[wi].task.clone(), self.workers[wi].prompt.clone(), self.workers[wi].effort.clone());
                    self.extra_attempts += 1;
                    self.workers[wi].state = WState::Cancelled;
                    self.spawn_task(ctx, task, Some(prompt), effort);
                }
            }
        }
    }

    /// A provider's exhausted balance is not a failed implementation attempt. Replace just
    /// this agent, keeping a worker's current worktree, task, and attempt budget intact.
    fn fallback_after_usage_limit(&mut self, ctx: &mut dyn Ctx, a: AgentId, error: &str) -> bool {
        if self.halted() {
            return false;
        }
        let Some(role_name) = self.role_name_of(a) else { return false };
        let running = self.worker_idx(a).and_then(|wi| self.workers[wi].model.clone())
            .or_else(|| self.slot_models.get(&self.slot_of(a)).cloned())
            .or_else(|| ctx.agent(a).map(|ag| ag.model_alias.clone()).filter(|m| !m.is_empty()))
            .unwrap_or_else(|| self.role_for_slot(&self.slot_of(a), &role_name).model);
        let fallback = self.pattern.role(&role_name).and_then(|role| role.fallback.as_ref())
            .filter(|alias| !alias.trim().is_empty() && **alias != running && self.knows_model(alias)).cloned();
        let Some(alias) = fallback else { return false };
        let name = self.name_of(a);
        let first_token_key = self.first_token_key(a);
        let note = format!("[mantra:fallback] The previous provider hit its billing or usage limit ({}). You are its replacement on {alias}. Your current files contain the work so far. Continue where it left off.", trunc(error, 240));
        self.forget_agent_session(a);
        self.set_agent_model(a, &alias);
        let restarted = if let Some(wi) = self.worker_idx(a) {
            let dir = self.workers[wi].wt.clone().or_else(|| ctx.agent(a).map(|ag| ag.cwd.clone()));
            if let Some(dir) = dir {
                self.tokens_prev += ctx.agent(a).map(|ag| ag.tokens_total).unwrap_or(0);
                ctx.stop(a, true);
                let branch = self.workers[wi].branch.clone();
                self.workers[wi].finished = None;
                self.workers[wi].transient = false;
                let task_prompt = self.workers[wi].prompt.clone();
                self.workers[wi].prompt = join_note(Some(note), task_prompt.clone());
                self.launch_worker(ctx, wi, dir, branch);
                self.workers[wi].prompt = task_prompt;
                true
            } else {
                false
            }
        } else {
            self.respawn(ctx, a, Some(note)).is_ok()
        };
        if !restarted {
            return false;
        }
        self.clear_replaced_agent(a);
        if let Some(key) = first_token_key {
            self.first_token_strikes.remove(&key);
        }
        self.alert(ctx, format!("{name}: {running} hit a provider billing or usage limit — continuing on fallback {alias}. {}", trunc(error, 160)));
        self.save_state();
        true
    }

    fn clear_replaced_agent(&mut self, a: AgentId) {
        self.retry_counts.remove(&a);
        self.gateway_streak.remove(&a);
        self.transport_errs.remove(&a);
        self.effort_retried.remove(&a);
        self.continue_queue.retain(|(agent, _, _)| *agent != a);
        self.paused_agents.retain(|agent| *agent != a);
    }

    /// Did every agent that is working fail on the gateway within the last minute (and `a` too)?
    /// Then it is the gateway, not an agent.
    fn gateway_down(&self, ctx: &dyn Ctx, a: AgentId) -> bool {
        let recent = |x: &AgentId| self.transport_errs.get(x).map(|t| t.elapsed() < Duration::from_secs(60)).unwrap_or(false);
        recent(&a) && self.all_agents().iter().all(|x| *x == a || recent(x) || !ctx.agent(*x).map(|g| g.busy()).unwrap_or(false))
    }

    /// The model refused the requested reasoning effort and named the values it takes: set the
    /// nearest allowed one on the agent and retry its turn, once. `false` when nothing could be
    /// parsed (or it was already tried), and the failure is handled as before.
    fn remap_effort(&mut self, ctx: &mut dyn Ctx, a: AgentId, allowed: &[String]) -> bool {
        let Some((want, alias)) = ctx.agent(a).map(|x| (x.effort.clone(), x.model_alias.clone())) else { return false };
        let Some(pick) = crate::agent::nearest_effort(&want, allowed).filter(|p| *p != want) else { return false };
        if !self.effort_retried.insert(a) {
            return false;
        }
        let used = match ctx.set_effort(a, &pick) {
            u if u.is_empty() => pick,
            u => u,
        };
        let name = self.name_of(a);
        self.log("⚙", "amber", format!("{name}: effort {want} is not offered by {alias} — using {used}"));
        let wait = Duration::from_secs(1);
        if let Some(wi) = self.worker_idx(a) {
            self.workers[wi].effort = Some(used.clone());
            self.workers[wi].state = WState::Retrying(Instant::now() + wait);
        }
        self.continue_queue.push((a, Instant::now() + wait, format!("[mantra:retry] Your previous turn was rejected: the model does not offer that reasoning effort; it now runs at {used}. Continue exactly where you left off.")));
        true
    }

    fn on_worker_done(&mut self, ctx: &mut dyn Ctx, wi: usize, status: &str, error: Option<String>) {
        // The fix worker of a gate round (`start_fix`) is Mantra's to follow, not the orchestrator's.
        let fix_round = match &self.stage {
            Stage::Phase { step: PhaseStep::Fix { round }, .. } if self.workers[wi].adhoc => Some(*round),
            _ => None,
        };
        let w = &mut self.workers[wi];
        if w.paused || w.state == WState::Cancelled {
            return;
        }
        let before = w.state.clone();
        let tid = w.task.id.clone();
        let agent = w.agent;
        let mut fix_done: Option<(usize, u32)> = None;
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
                    if let Some(round) = fix_round {
                        self.halt_and_escalate(ctx, HaltReason::AttemptsExhausted, agent, format!("{tid} could not fix what gate round {round} found — it is blocked: {}", trunc(&report, 300)));
                    } else {
                        self.orch_event(ctx, format!("{tid} FAILED (blocked). Its report:\n{}\nUse mantra_retry with a better prompt, or mantra_prompt to unblock it.", trunc(&report, 1200)));
                    }
                } else {
                    // A done worker the user (or the orchestrator) messaged finishes again: that is
                    // a follow-up answered, not the task completing a second time.
                    let again = w.state == WState::Done;
                    w.state = WState::Done;
                    let el = fmt_dur(w.spawned.elapsed());
                    let adhoc = w.adhoc;
                    let summary = report.lines().find(|l| l.trim_start().to_lowercase().starts_with("summary:")).map(|l| l.trim().to_string()).unwrap_or_else(|| trunc(report.lines().last().unwrap_or(""), 90));
                    // Who asked for the follow-up: the user (typed in the zoomed view, or `@name …`,
                    // which arrives as "[from the user] …"), or the orchestrator / manager / Mantra.
                    // An Akasha push that landed in the answering turn asked nothing: the ask is
                    // the last message before it.
                    let last_ask = agent.and_then(|a| ctx.agent(a)).and_then(|a| a.items.iter().rev().find(|i| matches!(i.kind, crate::agent::Kind::User) && !crate::agent::is_akasha_push(&i.text))).map(|i| i.text.clone()).unwrap_or_default();
                    let by_you = again && (last_ask.starts_with("[from the user]") || !crate::agent::engine_sent(&last_ask));
                    let what = match (again, by_you) {
                        (false, _) => "done",
                        (true, true) => "done again (after your message)",
                        (true, false) => "done again",
                    };
                    self.log("✓", "green", format!("{tid} {what} in {el}"));
                    if again && by_you {
                        // the orchestrator (or the finale) already had this task's completion: the
                        // user's side conversation wakes nobody, and the new report is on the
                        // worker for whoever reads it next
                    } else if again {
                        // the orchestrator (the finale step, in the finale) asked a done worker
                        // something: it gets the answer, as an answer rather than the task
                        // completing a second time
                        self.orch_event(ctx, format!("{tid} answered the follow-up (it was already done). {summary}"));
                    } else if let (true, Stage::Phase { idx, .. }, Some(round)) = (adhoc, &self.stage, fix_round) {
                        // a gate round's fix: Mantra re-runs the checks and starts the next QA round
                        fix_done = Some((*idx, round));
                    } else if adhoc {
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
                if fix_round.is_none() {
                    self.orch_event(ctx, format!("{tid} was interrupted (it is idle now). mantra_prompt it to continue or mantra_retry."));
                }
            }
            _ => {
                let e = error.unwrap_or_else(|| "turn failed".into());
                self.workers[wi].state = WState::Failed(e.clone());
                self.log("✗", "red", format!("{tid} failed: {}", trunc(&e, 70)));
                if self.workers[wi].transient {
                    // The gateway outlasted `GATEWAY_RETRIES`: it is down for everyone, so neither
                    // the orchestrator (a retry would fail the same way) nor — for a fix worker —
                    // the manager and the planner are asked. The run waits for the gateway.
                    self.halt_for_outage(ctx, HaltReason::AgentTurnFailed, agent, format!("{tid}: {}", trunc(&e, 120)), format!("{tid}: {e}"), GATEWAY_DOWN_HINT);
                } else if let Some(round) = fix_round {
                    let msg = format!("{tid} could not fix what gate round {round} found — its turn failed: {}", trunc(&e, 200));
                    self.halt_and_escalate_full(ctx, HaltReason::AttemptsExhausted, agent, msg.clone(), msg, Some(&e));
                } else {
                    self.orch_event(ctx, format!("{tid} FAILED: {e}\nUse mantra_retry (optionally with a better prompt)."));
                }
            }
        }
        self.akasha_task_ended(ctx, wi, &before);
        if let Some((idx, round)) = fix_done {
            self.on_fix_done(ctx, idx, round);
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
        let fix = w.adhoc && matches!(self.stage, Stage::Phase { step: PhaseStep::Fix { .. }, .. });
        self.log("⚠", "amber", format!("tripwire: {tid} edited {} (scope: {scope})", outside.join(", ")));
        if fix {
            // The scope of a fix worker is guessed from the QA findings; nobody is woken for it.
            return;
        }
        self.orch_event(ctx, format!("TRIPWIRE: {tid} edited files outside its scope ({scope}): {}. Decide whether that's OK; steer it with mantra_prompt if not.", outside.join(", ")));
    }

    /// WP12.4/L1: a command an agent ran failed in a way that looks like a broken sandbox
    /// (`agent::looks_like_broken_sandbox`) — it cannot start, so no agent can run any command.
    /// Never spend gate rounds or retries on this: halt immediately, on the first occurrence, with
    /// the sandbox fix hint.
    pub fn on_environment_broken(&mut self, ctx: &mut dyn Ctx, a: AgentId, msg: String) {
        if self.halted() {
            return;
        }
        let who = self.name_of(a);
        let hint = crate::util::writable_root_fix_hint(&msg).unwrap_or_else(|| crate::util::sandbox_fix_hint().to_string());
        self.halt_full(
            ctx,
            HaltReason::Environment,
            Some(a),
            format!("{who}: a command failed — the sandbox can't run commands here ({}). {hint}", trunc(&msg, 200)),
            format!("{who}: a command failed — the sandbox can't run commands here ({msg}). {hint}"),
        );
    }

    pub fn on_crash(&mut self, ctx: &mut dyn Ctx, a: AgentId, reason: &str, restarting: bool) {
        let n = self.name_of(a);
        // A relaunch Mantra itself asked for (new effort/model, "restart requested") or a fresh
        // process after a dead resume ("resumed session …") is not a crash: never alert, and never
        // say "crashed" for something that was asked for.
        if reason.starts_with("restart") || reason.starts_with("resumed session") {
            self.log("↻", "amber", format!("{n}: {reason}"));
            return;
        }
        if restarting {
            self.log("↻", "amber", format!("{n}: {} — restarting & resuming", trunc(reason, 50)));
        } else {
            // Keep the backend's own error line (`codex exited (code 1): …/proc/self/exe…`): it is
            // what tells installation apart from protocol, provider or sandbox trouble.
            self.alert(ctx, format!("{n} keeps crashing: {} (select it and press r to restart)", trunc(reason, 240)));
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
            ctx.prompt(a, self.with_akasha(a, "[mantra:resume] Your process restarted mid-task. Continue exactly where you left off.".into()));
        }
        self.wake_orch(ctx);
    }

    /// A remote process stop also cancels retries already scheduled before the stop arrived.
    pub fn agent_stopped_by_user(&mut self, a: AgentId) {
        self.continue_queue.retain(|(id, _, _)| *id != a);
        self.retry_counts.remove(&a);
        self.last_status.remove(&a);
        self.log("■", "amber", format!("{} process stopped by you — respawn to continue", self.name_of(a)));
    }

    pub fn tick(&mut self, ctx: &mut dyn Ctx) {
        if self.last_tick.elapsed() < Duration::from_millis(900) {
            return;
        }
        self.last_tick = Instant::now();
        // Persistence is event-driven: every journal line marks the state dirty, and this flush —
        // at most once per tick, which is the debounce — writes it out, halted or not.
        if self.state_dirty {
            self.state_dirty = false;
            self.save_state();
        }
        if !self.is_active() {
            return;
        }
        // The manager's deadlines and inbox run while halted too: it is who resolves halts.
        self.manager_tick(ctx);
        if self.halted() {
            return;
        }
        self.planner_wait_tick(ctx);
        let now = Instant::now();
        // continue after backoff
        let due: Vec<(AgentId, String)> = self.continue_queue.iter().filter(|(_, t, _)| *t <= now).map(|(a, _, m)| (*a, m.clone())).collect();
        self.continue_queue.retain(|(_, t, _)| *t > now);
        for (a, m) in due {
            if let Some(wi) = self.worker_idx(a) {
                self.workers[wi].state = WState::Running;
            }
            self.mark_edge(a);
            ctx.prompt(a, self.with_akasha(a, m));
        }
        // stall + budget tripwires
        let stall = Duration::from_secs(self.pattern.settings.stall_minutes * 60);
        let mut events = vec![];
        for w in &mut self.workers {
            let Some(a) = w.agent.and_then(|a| ctx.agent(a)) else { continue };
            if w.state == WState::Running && a.busy() && !a.swarm_waiting && a.last_event.elapsed() > stall && !w.stall_flagged {
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
        self.akasha_tick(ctx);
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
        self.first_token_tick(ctx);
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
            if agent.busy() || agent.stopped_by_user || agent.swarm_waiting {
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

    /// The no-first-token deadline (`settings.first_token_minutes`). The ladder above only sees
    /// idle agents and the stall line only busy-but-quiet ones, but a gateway that takes the turn
    /// and never answers — or loops on "Reconnecting… n/5" — keeps moving `last_event` with its
    /// own retry notices. What it never does is produce output, which `Agent::first_token_wait`
    /// measures: an active turn with no tokens, text or tool activity since it began.
    fn first_token_tick(&mut self, ctx: &mut dyn Ctx) {
        let mins = self.pattern.settings.first_token_minutes;
        if mins == 0 {
            return;
        }
        let limit = Duration::from_secs(mins * 60);
        let live = self.all_agents();
        let mut over = vec![];
        for a in &live {
            match ctx.agent(*a).and_then(|x| x.first_token_wait()) {
                // one silent spell is one strike, however many ticks it lasts
                Some(w) if w >= limit => {
                    if self.first_token_flagged.insert(*a) {
                        over.push((*a, w));
                    }
                }
                _ => {
                    self.first_token_flagged.remove(a);
                }
            }
        }
        self.first_token_flagged.retain(|a| live.contains(a));
        for (a, waited) in over {
            self.on_first_token_deadline(ctx, a, waited);
        }
    }

    /// The slot an agent fills, which — unlike its `AgentId` — survives the respawn the first
    /// strike causes.
    fn first_token_key(&self, a: AgentId) -> Option<String> {
        self.watchdog_role_tag(a).or_else(|| self.workers.iter().find(|w| w.agent == Some(a)).map(|w| format!("worker:{}", w.task.id)))
    }

    /// An agent sat through the deadline with nothing back. First time in a row for its slot: a
    /// fresh process and thread, told to carry on (the interrupt-and-resend). Second time: the
    /// role's `fallback` model if it has one, else the manager is asked (or, with nobody above
    /// to ask, the run halts for the user). A completed turn clears the slot's count.
    fn on_first_token_deadline(&mut self, ctx: &mut dyn Ctx, a: AgentId, waited: Duration) {
        let Some(key) = self.first_token_key(a) else { return };
        let name = self.name_of(a);
        let strikes = {
            let n = self.first_token_strikes.entry(key.clone()).or_insert(0);
            *n += 1;
            *n
        };
        let quiet = fmt_dur(waited);
        if strikes == 1 {
            self.log("↻", "amber", format!("{name}: no output for {quiet} — restarting its turn (first-token deadline)"));
            let note = format!("[mantra:watchdog] Your previous turn produced no output for {quiet} (the model connection looked stalled) and was restarted. Continue where you left off.");
            if let Err(e) = self.respawn(ctx, a, Some(note)) {
                self.log("!", "amber", format!("{name}: could not restart ({e})"));
            }
            return;
        }
        let role_name = self.role_name_of(a);
        let running = ctx.agent(a).map(|x| x.model_alias.clone()).unwrap_or_default();
        let fallback = role_name
            .as_ref()
            .and_then(|r| self.pattern.role(r))
            .and_then(|r| r.fallback.clone().filter(|f| !f.trim().is_empty() && *f != r.model && *f != running));
        if let (Some(alias), Some(role_name)) = (fallback, role_name) {
            self.log("↻", "amber", format!("{name}: no output twice — falling back to {alias}"));
            if let Some(r) = self.pattern.roles.get_mut(&role_name) {
                r.model = alias;
            }
            // The fallback is the role's model from now on (a resume rebuilds the pattern from the
            // run's `pattern.toml`, which `save_state` rewrites below); a model the user had
            // switched this very agent to is what went silent, so it no longer outranks the role.
            match self.worker_idx(a) {
                Some(wi) => self.workers[wi].model = None,
                None => {
                    let slot = self.slot_of(a);
                    self.slot_models.remove(&slot);
                }
            }
            // the new model gets its own restart-then-escalate cycle
            self.first_token_strikes.remove(&key);
            let note = format!("[mantra:watchdog] Your previous model produced no output for {quiet}, twice in a row; you are its replacement. Continue where you left off.");
            if let Err(e) = self.respawn(ctx, a, Some(note)) {
                self.log("!", "amber", format!("{name}: could not respawn on the fallback ({e})"));
            }
            self.save_state();
            return;
        }
        // The manager is a supervisor, not a step of the run: a fresh one, never a halt.
        if Some(a) == self.manager {
            self.log("↻", "amber", format!("manager: no output for {quiet} again — respawning it (first-token deadline)"));
            self.respawn_manager(ctx, Some(format!("[mantra:watchdog] The previous manager produced no output for {quiet}, twice in a row. Pick up where it left off.")));
            return;
        }
        let es = self.pattern.settings.watchdog_escalate_seconds.max(1);
        if self.has_manager() && self.manager_watchdog.is_none() {
            if let Some(m) = self.ensure_manager(ctx) {
                self.log("⏰", "amber", format!("{name}: no output for {quiet} twice — waking the manager (first-token deadline)"));
                self.mark_edge(m);
                ctx.prompt(m, format!("[mantra:watchdog] {name}'s turn has produced no output (no tokens, no text) for {quiet}, twice in a row, even after a restart: the model gateway behind it is probably down or looping on reconnects. Find out (mantra_log {name}, mantra_journal) and get it moving: mantra_respawn, mantra_retry, or mantra_brief_orchestrator. If you do nothing within {es}s the planner is woken instead."));
                self.manager_watchdog = Some(ManagerCase { deadline: Instant::now() + Duration::from_secs(es), about: name, respawned: false });
                return;
            }
        }
        self.halt_turn_failed(ctx, Some(a), format!("{name}: no output for {quiet}, twice in a row — the model gateway may be down (set a `fallback` model for its role, or respawn it)"));
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
                ctx.prompt(a, self.with_akasha(a, format!("[mantra:watchdog] You are expected to be {what} but have been idle for {secs}s. Continue, or call mantra_wait/mantra_log to say why you are waiting.")));
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
        self.halt = Some(Halt { reason: HaltReason::User, agent: None, message: "paused by you".into(), full: "paused by you".into(), since: Instant::now() });
        self.clock_stop();
        self.save_state();
    }

    /// User typed into the Mandala prompt (not addressed to a specific agent).
    pub fn user_input(&mut self, ctx: &mut dyn Ctx, text: &str) {
        self.planner_wait = None;
        if let Some(proposal) = self.pending_pattern.take() {
            self.question = None;
            self.alerts.retain(|a| !a.starts_with("Approve workflow change"));
            let answer = text.trim().to_lowercase();
            if matches!(answer.as_str(), "yes" | "y" | "approve" | "approved" | "yes, approve") {
                self.install_pattern(ctx, proposal);
            } else if let Some(p) = self.planner {
                self.mark_edge(p);
                ctx.prompt(p, format!("[from the user] Workflow proposal was not approved. Feedback: {text}\nKeep the active workflow; revise your proposal if needed."));
            }
            self.save_state();
            return;
        }
        if let Some(q) = self.question.take() {
            // The planner (or the manager) asked; this is the answer — straight back to it,
            // whatever the stage.
            let who = self.name_of(q.from);
            let asked = format!("the {who} asks: {}", q.text);
            self.alerts.retain(|a| *a != asked);
            if Some(q.from) != self.planner {
                // Even an answer to a supervisor enters through the planner. It decides how
                // to brief the requester and retains the ordinary chain of command.
                self.pending_questions.insert(q.from, q.text.clone());
                let p = match self.planner { Some(p) => p, None => self.spawn_planner(ctx) };
                self.mark_edge(p);
                self.log("›", "rose", format!("you → planner (answer for {who}): {}", trunc(text, 60)));
                ctx.prompt(p, format!("[from the user] Answering the {who}'s question ({}):\n{text}\nDecide the next action and reply to the {who} with mantra_prompt; brief the orchestrator or resume the run as needed.", q.text));
                return;
            }
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
        if text.trim().is_empty() {
            if mode == Send::Force {
                if let Some(p) = self.planner.filter(|p| ctx.agent(*p).is_some_and(|ag| !ag.queued.is_empty())) {
                    ctx.prompt_mode(p, String::new(), mode);
                    return true;
                }
            }
            return false;
        }
        // Every user message enters through the planner, including a composer focused on a
        // worker. Emergency direct control belongs to the planner's tools.
        if self.pending_pattern.is_some() || self.question.is_some() || matches!(self.stage, Stage::Setup | Stage::Review) {
            self.user_input(ctx, text);
            return true;
        }
        self.planner_wait = None;
        let _ = name;
        let target = self.planner.or_else(|| self.is_active().then(|| self.spawn_planner(ctx)));
        match target {
            Some(a) => {
                self.mark_edge(a);
                ctx.prompt_mode(a, format!("[from the user] {text}"), mode);
                self.log("›", "rose", format!("you → planner: {}", trunc(text, 60)));
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
        let role = self.role_for_slot("manager", &name);
        let req = SpawnReq {
            name: "manager".into(),
            effort: self.effort_for(None, &name, &role, false),
            thread_name: self.thread_name(&name),
            role_name: name,
            instructions: format!("{}\n{}", role.instructions, tools::MANAGER_PROTOCOL),
            cwd: self.workspace_dir(),
            tools: tools::manager_tools(),
            extra_writable: vec![],
            context_override: self.context_for(&role.model, None),
            network: self.pattern.settings.network,
            writable_roots: self.pattern.settings.writable_roots.clone(),
            mcp: vec![],
            role,
        };
        let req = self.role_req(req);
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
        let brief = self.with_akasha(id, self.manager_brief("You manage this run."));
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
            "[mantra:manage] {lead}\nGoal:\n{}\n\n{plan}\nRun settings: tasks in parallel: {}; each task gets {} attempts in total; a gate gets {} rounds; the watchdog nudges an idle agent after {}s and escalates after {}s; you get a health digest every {}.\n\nNothing needs you yet: call mantra_wait. Mantra wakes you when something does.",
            self.brief,
            self.pattern.parallel_summary(),
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
            s.push_str(&format!("HALTED ({}) for {}: {}\n", reason_label(h.reason), fmt_dur(h.since.elapsed()), h.full));
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
                    // a manager in a group gets its notes on a retry like every member (`tick`)
                    ctx.prompt(m, self.with_akasha(m, msg));
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
    /// overview and is asked whether anything needs unsticking — but only if something happened
    /// since the last digest (a journal line, a worker or agent changing state): an unchanged run
    /// has nothing new to say, and each digest costs the manager a whole turn. A quiet interval is
    /// skipped silently and re-checked every tick, so the first change after it gets its digest.
    /// Skipped (not postponed) while a halt is open — the escalation already has its attention.
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
        let print = self.health_fingerprint(ctx);
        if self.last_health_print.as_ref() == Some(&print) {
            return;
        }
        self.last_health = Instant::now();
        self.log("◈", "blue", "manager health check");
        // Taken after the line above, so the digest's own journal entry is not "a change".
        self.last_health_print = Some(HealthPrint { journal: self.pulse_seq, ..print });
        self.manager_event(ctx, format!("[mantra:health] Periodic health check (every {every} min). Is the run healthy — is everyone who should be working working, is anyone stuck, looping, failing repeatedly or drifting from the goal? Intervene only where something is actually off (at most one message per agent); otherwise just call mantra_wait."));
    }

    /// What `health_tick` compares between digests. The manager itself is left out: its own turns
    /// flip its status all the time and say nothing about the run.
    fn health_fingerprint(&self, ctx: &dyn Ctx) -> HealthPrint {
        HealthPrint {
            journal: self.pulse_seq,
            workers: self.workers.iter().map(|w| (w.task.id.clone(), worker_state_str(&w.state))).collect(),
            agents: self.all_agents().into_iter().filter(|a| Some(*a) != self.manager).filter_map(|a| ctx.agent(a).map(|x| (a, x.busy(), x.status.clone()))).collect(),
        }
    }

    /// The manager restarting an agent during an escalated halt *is* its decision to go on: lift
    /// the halt first (an exhausted task gets its extra attempt), so the retry/respawn is accepted.
    /// Returns whether a halt was lifted.
    fn lift_halt_for_restart(&mut self, ctx: &mut dyn Ctx, by: AgentId, target: AgentId) -> bool {
        let Some(h) = &self.halt else { return false };
        if !matches!(h.reason, HaltReason::GateExhausted | HaltReason::AttemptsExhausted | HaltReason::AgentTurnFailed) {
            return false;
        }
        if h.reason == HaltReason::AgentTurnFailed && h.agent != Some(target) {
            return false;
        }
        if h.reason == HaltReason::AttemptsExhausted && h.agent != Some(target) {
            let owns_task = self.worker_idx(target).map(|wi| h.agent.is_none()
                && h.message.starts_with(&format!("{} has used all ", self.workers[wi].task.id))).unwrap_or(false);
            if !owns_task {
                return false;
            }
        }
        if h.reason == HaltReason::AttemptsExhausted {
            self.extra_attempts += 1;
        }
        self.log("▶", "green", format!("{} lifted the halt to restart an agent", self.name_of(by)));
        self.resume_for_restart(ctx, Some(by), target);
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
        ctx.prompt(id, self.with_akasha(id, prompt));
        self.log("↻", "violet", "manager respawned (watchdog/manual)");
    }

    /// Spawn a fresh orchestrator agent for the current phase (extracted out of `start_phase` so
    /// `respawn` can reuse it, WP7.4). Does not touch `self.stage` or prompt the new agent.
    pub(super) fn spawn_orchestrator(&mut self, ctx: &mut dyn Ctx) -> AgentId {
        let plan = self.plan.clone().unwrap_or_default();
        let name = self.pattern.flow.orchestrator.clone();
        let role = self.role_for_slot("orchestrator", &name);
        let id = ctx.spawn(self.role_req(SpawnReq {
            name: "orchestrator".into(),
            effort: self.effort_for(None, &name, &role, false),
            thread_name: self.thread_name(&name),
            role_name: name,
            instructions: format!("{}\n{}\n## Project-specific brief from the planner\n{}", role.instructions, tools::ORCHESTRATOR_PROTOCOL, plan.orchestrator_brief),
            cwd: self.workspace_dir(),
            tools: tools::orchestrator_tools(),
            extra_writable: vec![],
            context_override: self.context_for(&role.model, None),
            network: self.pattern.settings.network,
            writable_roots: self.pattern.settings.writable_roots.clone(),
            mcp: vec![],
            role,
        }));
        self.orchestrator = Some(id);
        id
    }

    /// Respawn any run agent in place (WP7.4): the planner, the orchestrator, the phase gate, the
    /// finale agent, or a worker (delegates to `retry_worker`). `note` is an extra line prepended
    /// to the resume prompt (used by the watchdog and by the `r` / `ctrl+r` / `/respawn` UI paths).
    /// `Ok` says what happened: `RESPAWNED`, or `RESPAWN_DEFERRED` for a gate/finale agent while
    /// the run is halted (the pause boundary — the fresh agent, note included, starts on resume).
    pub fn respawn(&mut self, ctx: &mut dyn Ctx, a: AgentId, note: Option<String>) -> Result<&'static str, String> {
        // Replacing the failed agent resolves its old provider/turn failure. A pause or a
        // failure belonging to another agent remains active, including its owner alert.
        let valid = self.worker_idx(a).is_some()
            || Some(a) == self.planner || Some(a) == self.orchestrator || Some(a) == self.manager
            || (Some(a) == self.gate_agent && self.phase_idx().is_some())
            || (Some(a) == self.finale_agent && matches!(self.stage, Stage::Finale { .. }));
        let resolves = valid && self.halt.as_ref().map(|h| {
            h.agent == Some(a) && matches!(h.reason, HaltReason::Auth | HaltReason::UsageLimit | HaltReason::ProviderRejected | HaltReason::AgentTurnFailed)
        }).unwrap_or(false);
        if resolves {
            self.resume_for_restart(ctx, Some(a), a);
        }
        let result = self.respawn_inner(ctx, a, note);
        if matches!(result, Ok(RESPAWNED)) {
            self.clear_replaced_agent(a);
            self.save_state();
        }
        result
    }

    fn respawn_inner(&mut self, ctx: &mut dyn Ctx, a: AgentId, note: Option<String>) -> Result<&'static str, String> {
        if let Some(wi) = self.worker_idx(a) {
            // `r` on the task that exhausted its attempts *is* the decision to try once more.
            if self.halt.as_ref().map(|h| h.reason == HaltReason::AttemptsExhausted
                && (h.agent == Some(a) || (h.agent.is_none() && h.message.starts_with(&format!("{} has used all ", self.workers[wi].task.id))))).unwrap_or(false) {
                self.extra_attempts += 1;
                self.resume_for_restart(ctx, None, a);
            }
            // A note goes *on top of* the task prompt — `retry_worker`'s argument replaces it.
            let prompt = note.map(|n| join_note(Some(n), self.workers[wi].prompt.clone()));
            let msg = self.retry_worker(ctx, a, prompt);
            return if msg.starts_with("REFUSED") { Err(msg) } else { Ok(RESPAWNED) };
        }
        // The built-in default pattern's last finale step reuses `self.planner` as the finale
        // agent id (`start_finale`), so this ambiguous case must be checked before the plain
        // planner check below — mirrors the disambiguation `on_turn_done` already does.
        if Some(a) == self.finale_agent || (Some(a) == self.planner && matches!(self.stage, Stage::Finale { .. })) {
            return self.respawn_finale(ctx, note).ok_or_else(|| "that agent's stage has already moved on".into());
        }
        if Some(a) == self.planner {
            self.respawn_planner(ctx, note);
            return Ok(RESPAWNED);
        }
        if Some(a) == self.orchestrator {
            self.respawn_orchestrator(ctx, note);
            return Ok(RESPAWNED);
        }
        if Some(a) == self.manager {
            self.respawn_manager(ctx, note);
            return Ok(RESPAWNED);
        }
        if Some(a) == self.gate_agent {
            return self.respawn_gate(ctx, note).ok_or_else(|| "that agent's stage has already moved on".into());
        }
        Err("that agent is no longer part of the run".into())
    }

    fn respawn_planner(&mut self, ctx: &mut dyn Ctx, note: Option<String>) {
        let shared_finale = self.planner.is_some() && self.finale_agent == self.planner && matches!(self.stage, Stage::Finale { .. });
        if let Some(old) = self.planner.take() {
            self.tokens_prev += ctx.agent(old).map(|x| x.tokens_total).unwrap_or(0);
            ctx.stop(old, true);
        }
        let id = self.spawn_planner(ctx);
        if shared_finale {
            self.finale_agent = Some(id);
            if let Stage::Finale { idx } = self.stage {
                self.start_finale(ctx, idx);
                return;
            }
        }
        self.mark_edge(id);
        let stage_txt = super::state::stage_label(&self.stage);
        // The whole plan used to be attached here, which is exactly the huge context a respawn is
        // meant to avoid — the current phase (like `respawn_orchestrator`) plus `mantra_read_phase`
        // for live task states covers what a resuming planner needs; the full plan is one call away.
        let resume = if let Some(phase) = self.current_phase() {
            let phase_json = serde_json::to_string_pretty(phase).unwrap_or_default();
            format!(
                "[mantra:respawn] Continue from stage {stage_txt} (plan v{}).\nCurrent phase:\n```json\n{phase_json}\n```\nCall mantra_read_phase for live task states; call mantra_read_plan first if you must revise other phases.",
                self.plan_version
            )
        } else if let Some(plan) = &self.plan {
            // No active phase (e.g. `Stage::Review`, right after the plan was submitted): there is
            // no current phase to show, so attach the whole plan instead of an empty block.
            let plan_json = serde_json::to_string_pretty(plan).unwrap_or_default();
            format!(
                "[mantra:respawn] Continue from stage {stage_txt} (plan v{}).\nThe plan:\n```json\n{plan_json}\n```",
                self.plan_version
            )
        } else {
            self.planning_prompt()
        };
        ctx.prompt(id, self.with_akasha(id, join_note(note, resume)));
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
        ctx.prompt(id, self.with_akasha(id, join_note(note, resume)));
        self.log("↻", "violet", "orchestrator respawned (watchdog/manual)");
    }

    fn respawn_gate(&mut self, ctx: &mut dyn Ctx, note: Option<String>) -> Option<&'static str> {
        let idx = self.phase_idx()?;
        let round = match self.stage {
            Stage::Phase { step: PhaseStep::Gate { round }, .. } => round,
            _ => 1,
        };
        if let Some(old) = self.gate_agent.take() {
            self.tokens_prev += ctx.agent(old).map(|x| x.tokens_total).unwrap_or(0);
            ctx.stop(old, true);
        }
        if self.halted() {
            // the pause boundary: the fresh gate agent starts on resume, with the note
            self.log("↻", "violet", "gate respawn deferred until the run resumes");
            self.defer(Pending::Gate { idx, round, note });
            return Some(RESPAWN_DEFERRED);
        }
        self.spawn_gate_at_round(ctx, idx, round);
        if let (Some(n), Some(g)) = (note, self.gate_agent) {
            ctx.prompt(g, format!("[mantra:respawn] {n}"));
        }
        self.log("↻", "violet", "gate respawned (watchdog/manual)");
        Some(RESPAWNED)
    }

    fn respawn_finale(&mut self, ctx: &mut dyn Ctx, note: Option<String>) -> Option<&'static str> {
        let Stage::Finale { idx } = self.stage else { return None };
        if let Some(old) = self.finale_agent.take() {
            if Some(old) != self.planner {
                self.tokens_prev += ctx.agent(old).map(|x| x.tokens_total).unwrap_or(0);
                ctx.stop(old, true);
            }
        }
        if self.halted() {
            self.log("↻", "violet", "finale agent respawn deferred until the run resumes");
            self.defer(Pending::Finale { idx, note });
            return Some(RESPAWN_DEFERRED);
        }
        self.start_finale(ctx, idx);
        if let (Some(n), Some(a)) = (note, self.finale_agent) {
            ctx.prompt(a, format!("[mantra:respawn] {n}"));
        }
        self.log("↻", "violet", "finale agent respawned (watchdog/manual)");
        Some(RESPAWNED)
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
        // a worker the gateway outage failed did not use up an attempt
        if std::mem::take(&mut self.workers[wi].transient) {
            self.extra_attempts += 1;
        }
        self.workers[wi].state = WState::Cancelled;
        let effort = self.workers[wi].effort.clone();
        let p = prompt.or(Some(self.workers[wi].prompt.clone()));
        self.log("↻", "violet", format!("respawning {}", task.id));
        self.spawn_task(ctx, task, p, effort)
    }

    // ───────────────────────────── tools ─────────────────────────────

    pub fn on_tool_call(&mut self, ctx: &mut dyn Ctx, a: AgentId, req: Value, tool: &str, args: &Value) {
        if tools::VISION_TOOLS.contains(&tool) && !self.yielded.contains(&a) {
            self.vision_tool(ctx, a, req, tool, args);
            return;
        }
        let (text, ok) = self.handle_tool(ctx, a, tool, args);
        ctx.tool_result(a, req, text, ok);
    }

    /// `mantra_screenshot` / `mantra_view_image`: only for an agent whose model takes images (the
    /// tools are offered to no one else, so a call from any other is a thread that outlived a
    /// model switch — refused at once). Captures go to `<run dir>/screenshots/<agent>-<n>.png`;
    /// an image path is read relative to the agent's own working directory. The capture or the
    /// read runs as a job (`JobTag::Vision`), and `on_job` answers the call.
    fn vision_tool(&mut self, ctx: &mut dyn Ctx, a: AgentId, req: Value, tool: &str, args: &Value) {
        let model = ctx.agent(a).map(|x| x.model_alias.clone()).unwrap_or_default();
        if !self.sees(&model) {
            ctx.tool_result_items(a, req, vec![tools::text_item(&format!("{model} does not take images here, so {tool} is not available."))], false);
            return;
        }
        let name = self.name_of(a);
        let cwd = ctx.agent(a).map(|x| x.cwd.clone()).unwrap_or_else(|| self.workspace_dir());
        let (call, args, shots, stem) = (tool.to_string(), args.clone(), self.dir.join("screenshots"), name.clone());
        ctx.job(
            JobTag::Vision { agent: a, req, tool: tool.to_string(), name },
            Box::new(move || {
                let (items, ok) = tools::vision_call(&call, &args, &cwd, &shots, &stem);
                JobOut::Vision(items, ok)
            }),
        );
    }

    fn handle_tool(&mut self, ctx: &mut dyn Ctx, a: AgentId, tool: &str, args: &Value) -> (String, bool) {
        // A turn that already ended with `mantra_wait` takes no more tools: the first call after the
        // yield interrupts the turn, and any further call in it is only refused.
        if self.yielded.contains(&a) {
            if self.yield_interrupted.insert(a) {
                ctx.interrupt(a);
            }
            return ("Your turn already ended when you called mantra_wait. Do not call any more tools; Mantra wakes you with the next event.".into(), true);
        }
        let s = |k: &str| args.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
        let names = |k: &str| -> Vec<String> {
            match args.get(k) {
                Some(Value::Array(v)) => v.iter().filter_map(|x| x.as_str().map(|s| s.to_string())).collect(),
                Some(Value::String(s)) => s.split(',').map(|x| x.trim().to_string()).collect(),
                _ => vec![],
            }
        };
        if matches!(tool, "mantra_revise_pattern" | "mantra_read_pattern" | "mantra_kill" | "mantra_wait_for_external_condition" | "mantra_set_task_scope") && Some(a) != self.planner {
            return ("REFUSED: only the planner controls the workflow and scheduled re-entry".into(), false);
        }
        match tool {
            "mantra_read_pattern" => (self.pattern.to_toml(), true),
            "mantra_revise_pattern" => {
                if self.pending_pattern.is_some() || self.question.is_some() { return ("REFUSED: wait for the open user decision first".into(), false); }
                let reason = s("reason");
                if reason.trim().is_empty() { return ("REJECTED: give the reason and impact of changing the workflow".into(), false); }
                let pattern = match Pattern::from_toml(&s("toml")) { Ok(p) => p, Err(e) => return (format!("REJECTED: {e}"), false) };
                if let Err(e) = self.validate_pattern_replacement(&pattern) { return (format!("REJECTED: {e}"), false); }
                let proposal = PatternProposal { pattern, reason };
                let bypass = ctx.agent(a).map(|ag| crate::config::approval_policy(&ag.approval) == "never").unwrap_or(false);
                if bypass {
                    self.install_pattern(ctx, proposal);
                    ("ACCEPTED: workflow installed under the user's don't-ask approval mode. Read the active pattern and submit/revise the plan using its roles.".into(), true)
                } else {
                    let question = format!("Approve workflow change {} → {}? {}\nReply 'approve' or send feedback. Proposed workflow:\n{}", self.pattern.name, proposal.pattern.name, proposal.reason, proposal.pattern.to_toml());
                    self.pending_pattern = Some(proposal);
                    self.question = Some(Question { from: a, text: question.clone(), since: Instant::now() });
                    self.alerts.push(question);
                    self.log("?", "saffron", "workflow proposal awaits your approval");
                    ctx.notify("Mantra: workflow change awaits your approval");
                    self.save_state();
                    ("PROPOSED: waiting for explicit user approval. End your turn; the active workflow is unchanged.".into(), true)
                }
            }
            "mantra_wait_for_external_condition" => {
                let Some(seconds) = args.get("seconds").and_then(Value::as_u64).filter(|s| (1..=86_400).contains(s)) else { return ("REJECTED: seconds must be an integer from 1 to 86400".into(), false) };
                let (condition, on_reentry) = (s("condition"), s("on_reentry"));
                if condition.trim().is_empty() || on_reentry.trim().is_empty() { return ("REJECTED: provide the condition to inspect and instructions for re-entry".into(), false); }
                if self.planner_wait.is_some() { return ("REFUSED: one scheduled re-entry is already pending".into(), false); }
                self.planner_wait = Some(PlannerWait { due_unix: crate::util::unix_secs().saturating_add(seconds), condition, on_reentry });
                self.yielded.insert(a);
                self.log("◷", "teal", format!("planner scheduled external-condition check in {seconds}s"));
                self.save_state();
                (format!("SCHEDULED: end your turn now. Mantra will wake you once after {seconds}s to inspect the condition; the event loop remains free."), true)
            }
            "mantra_kill" => {
                let reason = s("reason");
                if reason.trim().is_empty() { return ("REJECTED: give the emergency reason".into(), false); }
                let Some(t) = self.resolve(&s("agent")) else { return ("unknown agent".into(), false) };
                if t == a { return ("REFUSED: the planner cannot kill itself".into(), false); }
                ctx.stop(t, true);
                self.killed_agents.insert(t);
                self.continue_queue.retain(|(id, _, _)| *id != t);
                self.pending_questions.remove(&t);
                if let Some(wi) = self.worker_idx(t) { self.workers[wi].state = WState::Cancelled; self.workers[wi].paused = true; }
                self.paused_agents.retain(|id| *id != t);
                if self.worker_idx(t).is_none() {
                    // Keep the caller's turn alive so it receives this tool result and can
                    // recover the stopped role immediately. A normal halt interrupts it too.
                    for other in self.all_agents().into_iter().filter(|id| *id != a && Some(*id) != self.manager) {
                        if ctx.agent(other).is_some_and(|ag| ag.busy()) {
                            ctx.interrupt(other);
                            if !self.paused_agents.contains(&other) { self.paused_agents.push(other); }
                        }
                    }
                    let message = format!("planner stopped {} for emergency recovery", s("agent"));
                    self.halt = Some(Halt { reason: HaltReason::AgentTurnFailed, agent: Some(t), message: message.clone(), full: format!("{message}: {reason}. Restart it with mantra_respawn after repairing the cause."), since: Instant::now() });
                    self.clock_stop();
                    self.alerts.push(message);
                } else {
                    self.orch_inbox.push(format!("[from the planner] Emergency stopped {}: {reason}. Revisit the task; use mantra_retry only after addressing the cause.", s("agent")));
                    self.wake_orch(ctx);
                }
                self.log("■", "amber", format!("planner killed {}: {}", s("agent"), reason));
                self.save_state();
                ("stopped; repair the plan or brief the orchestrator to recover".into(), true)
            }
            "mantra_set_task_scope" => {
                let tid = s("task_id");
                let reason = s("reason");
                let Some(scope) = args.get("scope_globs").and_then(Value::as_array).filter(|v| v.iter().all(|x| x.as_str().is_some_and(|s| !s.trim().is_empty()))).map(|v| v.iter().map(|x| x.as_str().unwrap().to_string()).collect::<Vec<_>>()) else { return ("REJECTED: scope_globs must be an array of nonempty strings".into(), false) };
                if scope.is_empty() || reason.trim().is_empty() { return ("REJECTED: provide explicit scope globs and a reason".into(), false); }
                let Some(idx) = self.phase_idx() else { return ("REFUSED: there is no current phase".into(), false) };
                let Some(mut plan) = self.plan.clone() else { return ("REFUSED: no plan".into(), false) };
                let Some(task) = plan.phases.get_mut(idx).and_then(|p| p.tasks.iter_mut().find(|t| t.id == tid)) else { return ("REJECTED: task is not in the current phase".into(), false) };
                task.scope = scope.clone();
                let mut remaining = plan.clone();
                remaining.phases = remaining.phases.into_iter().skip(idx).collect();
                if let Err(e) = remaining.validate(&self.pattern) { return (format!("REJECTED: {}", e.join("; ")), false); }
                self.plan = Some(plan);
                self.plan_version += 1;
                for w in self.workers.iter_mut().filter(|w| w.task.id == tid) { w.task.scope = scope.clone(); w.tripwires.retain(|p| !crate::util::in_scope(&scope, p)); }
                self.save_plan();
                self.log("☰", "saffron", format!("planner updated {tid} scope: {} ({reason})", scope.join(", ")));
                self.orch_inbox.push(format!("[from the planner] Task {tid} scope is now {}. Reason: {reason}", scope.join(", ")));
                self.wake_orch(ctx);
                self.save_state();
                ("ACCEPTED: live plan and worker scopes updated; matching tripwires cleared, QA still required".into(), true)
            }
            "mantra_submit_plan" | "mantra_revise_plan" => {
                if Some(a) != self.planner { return ("REFUSED: only the planner may submit or revise the plan".into(), false); }
                if self.pattern.is_auto() { return ("REJECTED: Auto needs a concrete approved workflow first; use mantra_revise_pattern".into(), false); }
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
                let mut validation_plan = plan.clone();
                if revising { validation_plan.phases = validation_plan.phases.into_iter().skip(self.phase_idx().unwrap_or(0)).collect(); }
                if let Err(errs) = validation_plan.validate(&self.pattern) {
                    self.log("✗", "amber", format!("plan rejected ({} issue(s))", errs.len()));
                    return (format!("REJECTED — fix these and resubmit:\n- {}", errs.join("\n- ")), false);
                }
                let old = self.plan.replace(plan.clone());
                self.plan_version += 1;
                self.save_plan();
                self.retitle(ctx, &plan.title);
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
                        ctx.prompt(t, self.with_akasha(t, "[mantra:resume] Continue your task.".into()));
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
            "mantra_status" => {
                let text = if Some(a) == self.manager { self.overview_text(ctx) } else { self.status_text(ctx) };
                // Compare a fingerprint, not the text: elapsed times and token counts tick every
                // second, which would let an idle agent poll forever and never see "no change".
                let key = self.status_fingerprint(ctx);
                if self.last_status.get(&a) == Some(&key) {
                    // nothing has changed since this turn's last look: the only thing left is to wait
                    self.yielded.insert(a);
                    ("No change since your last mantra_status. Call mantra_wait now — Mantra wakes you when something happens.".into(), true)
                } else {
                    self.last_status.insert(a, key);
                    (text, true)
                }
            }
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
                        let lifted = Some(a) == self.manager && self.lift_halt_for_restart(ctx, a, t);
                        match self.respawn(ctx, t, note) {
                            Ok(what) => (format!("{n} {what}{}", if lifted { " — the run resumed" } else { "" }), true),
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
            "mantra_read_plan" => match &self.plan {
                Some(p) => (serde_json::to_string_pretty(p).unwrap_or_default(), true),
                None => ("no plan submitted yet".into(), false),
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
                        if self.killed_agents.contains(&t) { return ("REFUSED: this agent was emergency-stopped; use mantra_respawn to recover it".into(), false); }
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
                                        Ok(_) => (format!("{tid} was stuck (prompted 3× with no progress) — respawned instead of forwarding this message"), true),
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
                        let lifted = Some(a) == self.manager && self.lift_halt_for_restart(ctx, a, t);
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
            "mantra_wait" => {
                self.yielded.insert(a);
                ("OK — end your turn now. Mantra will wake you with the next event.".into(), true)
            }
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
                    return (format!("this halt ({}) needs the user, not a resume", h.full), false);
                }
                let (reason, target) = (h.reason, h.agent);
                if reason == HaltReason::AttemptsExhausted {
                    self.extra_attempts += 1;
                }
                let who = self.name_of(a);
                self.log(if Some(a) == self.manager { "◈" } else { "✦" }, "saffron", format!("{who} resumed the run: {}", trunc(&note, 60)));
                if !note.trim().is_empty() {
                    // A stuck QA agent or fix worker is replaced by a fresh one on resume: the note
                    // goes to that one instead of waking the old one for a turn it will not finish.
                    let replaced = matches!(self.stage, Stage::Phase { step: PhaseStep::Gate { .. } | PhaseStep::Checks { .. } | PhaseStep::Fix { .. }, .. });
                    if replaced {
                        self.resume_note = Some(format!("[from the {who}] {note}"));
                    } else if let Some(t) = target {
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
                self.gate_big = match args.get("big") {
                    Some(Value::Bool(b)) => *b,
                    Some(Value::String(t)) => t.trim().eq_ignore_ascii_case("true"),
                    _ => false,
                };
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
            t if t.starts_with("mantra_akasha_") => self.akasha_tool(ctx, a, t, args),
            other => (format!("tool '{other}' is not available here"), false),
        }
    }

    fn validate_pattern_replacement(&self, pattern: &Pattern) -> Result<(), String> {
        pattern.validate().map_err(|e| e.join("; "))?;
        if pattern.is_auto() { return Err("a running workflow cannot be replaced with the Auto bootstrap".into()); }
        if !self.known_models.is_empty() {
            for (name, role) in &pattern.roles {
                if !self.known_models.contains_key(&role.model) { return Err(format!("role '{name}' names an unconfigured model alias '{}'", role.model)); }
                if let Some(fallback) = &role.fallback {
                    if !self.known_models.contains_key(fallback) { return Err(format!("role '{name}' names an unconfigured fallback model alias '{fallback}'")); }
                }
            }
        }
        if let Some(plan) = self.plan.as_ref().filter(|_| !matches!(self.stage, Stage::Finale { .. })) {
            let mut remaining = plan.clone();
            if let Some(idx) = self.phase_idx() { remaining.phases = remaining.phases.into_iter().skip(idx).collect(); }
            remaining.validate(pattern).map_err(|e| format!("the remaining plan must stay valid: {}", e.join("; ")))?;
        }
        for w in self.workers.iter().filter(|w| !matches!(w.state, WState::Cancelled | WState::Done)) {
            if !pattern.roles.get(&w.task.role).is_some_and(|r| r.kind == "worker") { return Err(format!("retain worker role '{}' until its task is retired", w.task.role)); }
        }
        if let Stage::Finale { idx } = self.stage {
            if pattern.flow.finale.get(idx).is_none() { return Err("retain the current finale step until it finishes".into()); }
        }
        // Isolation belongs to the already-created workspace and cannot be changed retroactively.
        if self.ws.is_some() && pattern.settings.isolation != self.pattern.settings.isolation { return Err("workspace isolation cannot change after setup".into()); }
        Ok(())
    }

    fn install_pattern(&mut self, ctx: &mut dyn Ctx, proposal: PatternProposal) {
        if let Err(e) = self.validate_pattern_replacement(&proposal.pattern) {
            self.log("✗", "amber", format!("workflow proposal is no longer applicable: {e}"));
            if let Some(p) = self.planner { ctx.prompt(p, format!("[mantra:pattern] Proposal could not be applied: {e}. Re-read state and propose a correction.")); }
            return;
        }
        let previous = self.pattern.clone();
        self.pattern = proposal.pattern;
        // Existing workers keep their live process, scope and worktree. New spawns use the new
        // role definitions; fixed supervisors are refreshed only when their definition changed.
        if previous.flow.manager != self.pattern.flow.manager || previous.roles.get(&previous.flow.manager) != self.pattern.roles.get(&self.pattern.flow.manager) {
            if let Some(m) = self.manager.take() { ctx.stop(m, true); }
            if self.phase_idx().is_some() { self.ensure_manager(ctx); }
        }
        if previous.flow.orchestrator != self.pattern.flow.orchestrator || previous.roles.get(&previous.flow.orchestrator) != self.pattern.roles.get(&self.pattern.flow.orchestrator) {
            if matches!(self.stage, Stage::Phase { step: PhaseStep::Orchestrating, .. }) { self.respawn_orchestrator(ctx, Some(format!("Workflow changed: {}", proposal.reason))); }
            else if let Some(o) = self.orchestrator.take() { ctx.stop(o, true); }
        }
        if previous.flow.phase_gate != self.pattern.flow.phase_gate || previous.roles.get(&previous.flow.phase_gate) != self.pattern.roles.get(&self.pattern.flow.phase_gate) {
            if self.gate_agent.is_some() { self.respawn_gate(ctx, Some(format!("Workflow changed: {}", proposal.reason))); }
        }
        if let Stage::Finale { idx } = self.stage {
            if previous.flow.finale.get(idx) != self.pattern.flow.finale.get(idx) { self.respawn_finale(ctx, Some(format!("Workflow changed: {}", proposal.reason))); }
        }
        self.log("✦", "saffron", format!("workflow {} → {}: {}", previous.name, self.pattern.name, proposal.reason));
        self.save_state();
        if let Some(p) = self.planner {
            let changed = previous.flow.planner != self.pattern.flow.planner || previous.roles.get(&previous.flow.planner) != self.pattern.roles.get(&self.pattern.flow.planner) || previous.worker_roles() != self.pattern.worker_roles();
            if changed {
                if ctx.agent(p).is_some_and(|ag| ag.busy()) { self.planner_refresh_pending = true; }
                else { self.respawn_planner(ctx, Some("Workflow approved; use the updated role instructions and tools.".into())); }
                return;
            }
            self.mark_edge(p);
            ctx.prompt(p, format!("[mantra:pattern] Workflow {} is now active. Read mantra_read_pattern; use its current roles for all future plan changes. {}", self.pattern.name, if self.plan.is_none() { "Submit the phased plan now." } else { "Brief the orchestrator with any resulting change." }));
        }
    }

    pub(super) fn planner_wait_tick(&mut self, ctx: &mut dyn Ctx) {
        let due = self.planner_wait.as_ref().is_some_and(|w| w.due_unix <= crate::util::unix_secs());
        if !due || self.halted() || !self.is_active() { return; }
        let p = match self.planner { Some(p) => p, None => self.spawn_planner(ctx) };
        if ctx.agent(p).is_some_and(|ag| ag.busy()) { return; }
        let wait = self.planner_wait.take().unwrap();
        // Clear and persist before dispatch so subsequent ticks cannot enqueue duplicates.
        self.log("◷", "teal", "planner external-condition check is due");
        self.save_state();
        self.mark_edge(p);
        ctx.prompt(p, format!("[mantra:external-condition] Scheduled re-entry is due. Inspect this condition once: {}\nInstructions: {}\nThe condition has not been evaluated by Mantra. Report the observed result; if still pending schedule a bounded re-entry instead of a sleep/poll loop.", wait.condition, wait.on_reentry));
    }

    pub fn pending_pattern_review(&self) -> bool { self.pending_pattern.is_some() }

    pub fn planner_wait_info(&self) -> Option<&PlannerWait> { self.planner_wait.as_ref() }

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
        // a fix worker still at it is working on a gate that no longer stands
        let fixers: Vec<AgentId> = self.workers.iter().filter(|w| w.adhoc && matches!(w.state, WState::Preparing | WState::Running | WState::Retrying(_) | WState::Queued)).filter_map(|w| w.agent).collect();
        for a in fixers {
            self.tokens_prev += ctx.agent(a).map(|x| x.tokens_total).unwrap_or(0);
            ctx.stop(a, true);
        }
        for w in self.workers.iter_mut().filter(|w| w.adhoc && matches!(w.state, WState::Preparing | WState::Running | WState::Retrying(_) | WState::Queued)) {
            w.state = WState::Cancelled;
        }
        self.stage = Stage::Phase { idx, step: PhaseStep::Orchestrating };
        self.checks.clear();
        self.conflicts.clear();
        self.gate_report = None;
        self.prev_gate = None;
        self.last_gate_blocker = None;
        self.last_verify_sig = None;
        self.drop_pending("the phase went back to building");
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
        // A sandbox/environment error is never a decision to route up the chain: it halts the
        // whole run at once, on whichever agent first reports it, instead of bouncing between
        // orchestrator/planner as "transient" while every agent's commands stay dead (WP: engine
        // roles stick to their job).
        if looks_like_broken_sandbox(&q) {
            self.on_environment_broken(ctx, a, q);
            return ("environment error: the run is halted for the user; do nothing more".into(), true);
        }
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

impl Drop for Run {
    /// A `Run` that goes away with its process still alive — a clean quit, `/leave`, a delete —
    /// gives the run back: only its own claim is removed (`Owner::release`), never one another
    /// Mantra took over in the meantime. A crash skips this, which is what the pid check is for.
    fn drop(&mut self) {
        self.owner.release(&self.dir);
    }
}

#[cfg(test)]
#[path = "run/tests.rs"]
mod halt_tests;
