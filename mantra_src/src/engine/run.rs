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
        if cap == 0 {
            return over;
        }
        let window = over.or_else(|| self.known_models.get(alias).copied());
        Some(window.map_or(cap, |w| w.min(cap)))
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
        self.halt = None;
        self.clock_start();
        self.escalation_open = false;
        self.manager_escalation = None;
        self.last_verify_sig = None;
        self.alerts.clear();
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
        let prompt = match &resume {
            Some(_) => format!("[mantra:resume] Mantra restarted while you were working on this task; your worktree still holds your changes. Continue exactly where you left off — do not start over.\n\nThe task again:\n{}\n\n(Context — overall project goal: {plan_summary})", w.prompt),
            None => format!("{}\n\n(Context — overall project goal: {plan_summary})", w.prompt),
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
        let ws = self.ws.clone();
        let wts: Vec<(PathBuf, String)> = self.workers.iter().filter(|w| !w.adhoc).filter_map(|w| w.wt.clone().map(|d| (d, w.branch.clone()))).collect();
        let msg = format!("mantra: phase {} — {}", idx + 1, phase_name);
        ctx.job(
            JobTag::Cleanup { phase: idx },
            Box::new(move || {
                let Some(ws) = ws else { return JobOut::Cleanup(Ok(String::new()), git::ReviewDiff::default()) };
                let committed = git::phase_commit(&ws, &msg);
                for (d, b) in wts {
                    git::remove_worker(&ws, &d, &b, false);
                }
                let diff = git::review_diff(&ws, git::DiffSpan::Run, FINALE_DIFF_BYTES);
                JobOut::Cleanup(committed.map(|c| if c { "committed".into() } else { "nothing to commit".into() }), diff)
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
                    self.log("⚠", "amber", format!("phase {} commit: {e}", phase + 1));
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
        if text.trim().is_empty() {
            return false;
        }
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
        if let Some(wi) = self.worker_idx(a) {
            // `r` on the task that exhausted its attempts *is* the decision to try once more.
            if self.halt.as_ref().map(|h| h.reason == HaltReason::AttemptsExhausted).unwrap_or(false) {
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
        if let Some(old) = self.planner.take() {
            self.tokens_prev += ctx.agent(old).map(|x| x.tokens_total).unwrap_or(0);
            ctx.stop(old, true);
        }
        let id = self.spawn_planner(ctx);
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

/// FNV-1a of a string: enough to tell "the pattern changed since it was written" without keeping
/// a second copy of the TOML around (`Run::pattern_hash`).
fn fnv1a(s: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// A plan title as a run name: one line, single spaces, at most 60 characters.
pub fn tidy_title(title: &str) -> String {
    trunc(title.split_whitespace().collect::<Vec<_>>().join(" ").trim(), 60)
}

/// A run's name for people before it has a plan: the goal's first six words.
pub fn title_from_goal(brief: &str) -> String {
    let words: Vec<&str> = brief.split_whitespace().take(6).collect();
    let t = words.join(" ");
    let t = t.trim_matches(|c: char| matches!(c, '.' | ',' | ';' | ':' | '-' | '"' | '\'' | '`')).trim();
    if t.is_empty() {
        "new run".into()
    } else {
        trunc(t, 60)
    }
}

/// Files a QA report or a check's output names: tokens that exist under `root`, or that look like
/// the path of a file (a `/` and an extension) a worker is to create. In order of first mention,
/// at most 12 — the scope of the fix worker the findings go to. Never a path that leaves `root`.
fn paths_in(text: &str, root: &Path) -> Vec<String> {
    let mut out: Vec<String> = vec![];
    let is_sep = |c: char| c.is_whitespace() || matches!(c, '`' | '"' | '\'' | '(' | ')' | '[' | ']' | '<' | '>' | ',' | ';' | '{' | '}' | '=' | '|');
    for raw in text.split(is_sep) {
        // `src/x.rs:12:5`, `src/x.rs#L10`, a trailing sentence dot
        let t = raw.split([':', '#']).next().unwrap_or("").trim_matches(|c: char| matches!(c, '.' | '*' | '!' | '?' | '-'));
        let t = t.strip_prefix("./").unwrap_or(t);
        if t.len() < 3 || t.len() > 200 || t.starts_with('/') || t.starts_with('-') || t.contains("://") || t.split('/').any(|seg| seg == "..") {
            continue;
        }
        // a diff header names `a/src/x.rs`; the file is `src/x.rs`
        let candidates = [t.strip_prefix("a/").or_else(|| t.strip_prefix("b/")).unwrap_or(t).to_string(), t.to_string()];
        let mut pick: Option<String> = None;
        for c in candidates {
            let looks_like_a_file = c.contains('/') && c.rsplit('/').next().and_then(|f| f.rsplit_once('.')).map(|(stem, ext)| !stem.is_empty() && (1..=6).contains(&ext.len()) && ext.chars().all(|ch| ch.is_ascii_alphanumeric())).unwrap_or(false);
            let exists = (c.contains('.') || c.contains('/')) && root.join(&c).exists();
            if exists || looks_like_a_file {
                pick = Some(c);
                break;
            }
        }
        if let Some(p) = pick {
            if !out.contains(&p) {
                out.push(p);
                if out.len() == 12 {
                    break;
                }
            }
        }
    }
    out
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

/// True when `dir` holds nothing but `.git`, `.mantra` and dotfiles — a fresh/greenfield project
/// the planner does not need to explore before submitting a plan. An unreadable directory is
/// never treated as empty (the old "explore first" behaviour is the safe default).
fn project_is_empty(dir: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else { return false };
    !entries.filter_map(|e| e.ok()).any(|e| {
        let name = e.file_name();
        let name = name.to_string_lossy();
        name != ".git" && name != ".mantra" && !name.starts_with('.')
    })
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
        /// Every `ctx.spawn`'s instructions, by the id it was given — for tests that check what a
        /// freshly spawned agent was actually told (`spawn` itself ignores the `SpawnReq`).
        spawned_instructions: Vec<(AgentId, String)>,
        /// Every `ctx.spawn`: who it was for and with what effort / thread name / working dir.
        spawns: Vec<Spawned>,
        /// The tag of every background job the engine asked for (the closures are never run, but
        /// for the vision tools': see `finished`).
        jobs: Vec<String>,
        stopped: Vec<AgentId>,
        efforts_set: Vec<(AgentId, String)>,
        renamed: Vec<(AgentId, String)>,
        /// Every `ctx.spawn`'s role model alias, by the id it was given.
        spawned_models: Vec<(AgentId, String)>,
        /// What `ctx.notify` was told, in order.
        notified: Vec<String>,
        /// Every `ctx.tool_result_items`: to whom, the `contentItems`, whether it worked.
        items: Vec<(AgentId, Vec<Value>, bool)>,
        /// The vision jobs, run as they were asked for (on this thread), waiting for `settle`.
        finished: Vec<(JobTag, JobOut)>,
    }
    #[derive(Clone, Debug)]
    struct Spawned {
        name: String,
        role_name: String,
        effort: Option<String>,
        thread_name: Option<String>,
        cwd: PathBuf,
        sandbox: String,
        model: String,
        context_override: Option<u64>,
        /// The names of the tools it was offered.
        tools: Vec<String>,
        /// The MCP servers it asked for.
        mcp: Vec<String>,
    }
    impl TestCtx {
        fn new() -> TestCtx {
            TestCtx { agents: HashMap::new(), next: 1, interrupted: vec![], prompts: vec![], spawned_instructions: vec![], spawns: vec![], jobs: vec![], stopped: vec![], efforts_set: vec![], renamed: vec![], spawned_models: vec![], notified: vec![], items: vec![], finished: vec![] }
        }
        /// Hands every finished vision job back to `run`, as the app's event loop would.
        fn settle(&mut self, run: &mut Run) {
            for (tag, out) in std::mem::take(&mut self.finished) {
                run.on_job(self, tag, out);
            }
        }
        fn spawned(&self, name: &str) -> Vec<Spawned> {
            self.spawns.iter().filter(|s| s.name == name).cloned().collect()
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
            transient: false,
            model: None,
        }
    }
    impl Ctx for TestCtx {
        fn spawn(&mut self, r: SpawnReq) -> AgentId {
            let id = self.next;
            self.next += 1;
            self.agents.insert(id, Agent::new(id, "a", "worker", PathBuf::from(".")));
            let tools = r.tools.iter().filter_map(|t| t.get("name").and_then(|n| n.as_str()).map(String::from)).collect();
            self.spawns.push(Spawned { name: r.name.clone(), role_name: r.role_name.clone(), effort: r.effort.clone(), thread_name: r.thread_name.clone(), cwd: r.cwd.clone(), sandbox: r.role.sandbox.clone(), model: r.role.model.clone(), context_override: r.context_override, tools, mcp: r.mcp.clone() });
            self.spawned_instructions.push((id, r.instructions));
            self.spawned_models.push((id, r.role.model));
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
        fn stop(&mut self, a: AgentId, _archive: bool) {
            self.stopped.push(a);
        }
        fn tool_result(&mut self, _a: AgentId, _req: Value, _text: String, _ok: bool) {}
        fn tool_result_items(&mut self, a: AgentId, _req: Value, items: Vec<Value>, ok: bool) {
            self.items.push((a, items, ok));
        }
        fn set_effort(&mut self, a: AgentId, effort: &str) -> String {
            self.efforts_set.push((a, effort.to_string()));
            effort.to_string()
        }
        fn set_thread_name(&mut self, a: AgentId, name: &str) {
            self.renamed.push((a, name.to_string()));
        }
        fn agent(&self, a: AgentId) -> Option<&Agent> {
            self.agents.get(&a)
        }
        fn job(&mut self, tag: JobTag, f: Box<dyn FnOnce() -> JobOut + std::marker::Send>) {
            self.jobs.push(format!("{tag:?}"));
            if matches!(tag, JobTag::Vision { .. }) {
                self.finished.push((tag, f()));
            }
        }
        fn notify(&mut self, text: &str) {
            self.notified.push(text.to_string());
        }
    }

    /// Akasha (experimental) in the Conductor: the block table, tool dispatch, what members are
    /// told, pushes to busy members, the Conductor's own entries, and the feature switched off.
    mod akasha {
        use super::*;
        use crate::engine::pattern::Group;
        use crate::engine::state::RunState;

        fn grouped(push: &str, auto: bool, cap: usize) -> Pattern {
            let mut p = Pattern::builtin();
            p.groups = vec![Group { name: "builders".into(), roles: vec!["worker-big".into(), "worker-small".into()], shared_chars: cap, push: push.into(), auto }];
            p
        }

        /// A pattern without groups (the built-ins have `builders`; a file from before groups has none).
        fn ungrouped() -> Pattern {
            Pattern { groups: vec![], ..Pattern::builtin() }
        }

        /// A run of `pattern` in its first phase, Akasha `on`, writing into a scratch dir of its own.
        fn run_of(pattern: Pattern, on: bool, tag: &str) -> Run {
            let mut run = Run::new(PathBuf::from("."), pattern, "build it".into());
            run.dir = scratch_dir(&format!("akasha-{tag}"));
            run.enable_akasha(on);
            run.stage = Stage::Phase { idx: 0, step: PhaseStep::Orchestrating };
            run
        }

        /// A worker of a member role, running, its agent mid-turn.
        fn member(ctx: &mut TestCtx, run: &mut Run, id: &str) -> AgentId {
            let a = ctx.add_busy();
            run.workers.push(mk_worker(id, "worker-small", a, WState::Running));
            a
        }

        /// A worker of a member role started the way the Conductor starts one (spawn + brief).
        fn spawn_member(ctx: &mut TestCtx, run: &mut Run, id: &str) -> AgentId {
            let mut w = mk_worker(id, "worker-small", 0, WState::Preparing);
            w.agent = None;
            run.workers.push(w);
            let wi = run.workers.len() - 1;
            run.launch_worker(ctx, wi, PathBuf::from("."), String::new());
            run.workers[wi].agent.expect("spawned")
        }

        fn call(ctx: &mut TestCtx, run: &mut Run, a: AgentId, tool: &str, args: Value) -> (String, bool) {
            run.handle_tool(ctx, a, tool, &args)
        }

        fn last_prompt(ctx: &TestCtx, a: AgentId) -> String {
            ctx.prompts.iter().rev().find(|(x, _)| *x == a).map(|(_, t)| t.clone()).unwrap_or_default()
        }

        fn instructions_of(ctx: &TestCtx, a: AgentId) -> String {
            ctx.spawned_instructions.iter().find(|(x, _)| *x == a).map(|(_, i)| i.clone()).unwrap_or_default()
        }

        fn pushes(ctx: &TestCtx, to: AgentId) -> Vec<String> {
            ctx.prompts.iter().filter(|(x, t)| *x == to && t.starts_with("[akasha · ")).map(|(_, t)| t.clone()).collect()
        }

        fn texts(run: &Run) -> Vec<String> {
            run.akasha["builders"].entries.iter().map(|e| e.text.clone()).collect()
        }

        #[test]
        fn blocks_come_from_the_groups_and_only_members_are_in_one() {
            let mut ctx = TestCtx::new();
            let mut run = run_of(grouped("urgent", true, 4096), true, "table");
            assert_eq!(run.akasha.keys().map(String::as_str).collect::<Vec<_>>(), vec!["builders"]);
            assert_eq!((run.akasha["builders"].cap, run.akasha["builders"].entries.len()), (4096, 0));
            run.akasha.get_mut("builders").unwrap().note("p1-a", "kept", false, 0).unwrap();
            run.enable_akasha(true);
            assert_eq!(run.akasha["builders"].entries.len(), 1, "turning it on again keeps what is there");
            let w = member(&mut ctx, &mut run, "p1-a");
            let o = ctx.add_idle();
            run.orchestrator = Some(o);
            assert_eq!(run.group_of(w), Some("builders"));
            assert_eq!(run.group_of(o), None, "the orchestrator's role is in no group");
            assert_eq!(run.group_of(999), None, "nor is an agent outside the run");
            run.enable_akasha(false);
            assert_eq!(run.group_of(w), None, "off, nobody is in a group");
            assert_eq!(run.akasha["builders"].entries.len(), 1, "but the notes are kept for when it is back on");
            assert!(run_of(ungrouped(), true, "table-plain").akasha.is_empty(), "no groups, no blocks");
            let _ = std::fs::remove_dir_all(&run.dir);
        }

        #[test]
        fn with_the_feature_off_a_grouped_pattern_runs_as_if_it_had_none() {
            let mut ctx = TestCtx::new();
            let mut run = run_of(grouped("urgent", true, 4096), false, "off");
            run.stage = Stage::Setup;
            run.start(&mut ctx);
            let said: Vec<String> = pulse_texts(&run).into_iter().filter(|l| l.starts_with("akasha")).collect();
            assert_eq!(said, vec!["akasha: 1 group configured but the feature is off — `mantra experimental enable akasha`".to_string()]);
            assert!(run.akasha.is_empty(), "no blocks");
            run.stage = Stage::Phase { idx: 0, step: PhaseStep::Orchestrating };
            let a = spawn_member(&mut ctx, &mut run, "p1-a");
            assert!(!ctx.spawned("p1-a")[0].tools.iter().any(|t| t.starts_with("mantra_akasha_")), "no tools: {:?}", ctx.spawned("p1-a")[0].tools);
            assert!(!instructions_of(&ctx, a).contains("Shared notes"));
            let (r, ok) = call(&mut ctx, &mut run, a, "mantra_akasha_note", json!({"text": "x"}));
            assert!(!ok && r.contains("not in a group"), "{r}");
            // on, or without groups, the start says nothing about it
            for (pattern, on, tag) in [(grouped("urgent", true, 4096), true, "off-on"), (ungrouped(), false, "off-plain")] {
                let mut other = run_of(pattern, on, tag);
                other.stage = Stage::Setup;
                other.start(&mut TestCtx::new());
                assert!(!pulse_texts(&other).iter().any(|l| l.starts_with("akasha")), "{tag}");
                let _ = std::fs::remove_dir_all(&other.dir);
            }
            let _ = std::fs::remove_dir_all(&run.dir);
        }

        #[test]
        fn members_get_the_tools_and_their_prompts_end_with_the_notes() {
            let mut ctx = TestCtx::new();
            let mut run = run_of(grouped("urgent", true, 16_384), true, "inject");
            let a = spawn_member(&mut ctx, &mut run, "p1-a");
            let tools = ctx.spawned("p1-a")[0].tools.clone();
            for t in ["mantra_akasha_read", "mantra_akasha_note", "mantra_akasha_amend", "mantra_akasha_forget", "mantra_ask"] {
                assert!(tools.iter().any(|x| x == t), "{t} in {tools:?}");
            }
            assert!(instructions_of(&ctx, a).contains("## Shared notes (akasha)"), "the spawn brief introduces the tools");
            assert!(!last_prompt(&ctx, a).contains("## Shared notes (akasha ·"), "an empty block is not shown");
            call(&mut ctx, &mut run, a, "mantra_akasha_note", json!({"text": "models live in src/models"}));
            // the next member's spawn brief ends with the block
            let b = spawn_member(&mut ctx, &mut run, "p1-b");
            let brief = last_prompt(&ctx, b);
            let tail = brief.split("\n\n## Shared notes (akasha · builders)\n").nth(1).unwrap_or_else(|| panic!("no notes in: {brief}"));
            assert!(tail.starts_with("## akasha · builders · 1 note · ") && tail.ends_with("] models live in src/models"), "{tail}");
            // and so does a retry prompt
            run.continue_queue.push((a, Instant::now(), "[mantra:retry] Your previous turn failed (x). Continue exactly where you left off.".into()));
            run.last_tick = Instant::now() - Duration::from_secs(2);
            run.tick(&mut ctx);
            let retry = last_prompt(&ctx, a);
            assert!(retry.starts_with("[mantra:retry]") && retry.contains("\n\n## Shared notes (akasha · builders)\n## akasha · builders · 1 note"), "{retry}");
            // the orchestrator is in no group: its tools and its prompts stay as they were
            let o = run.spawn_orchestrator(&mut ctx);
            assert!(!ctx.spawned("orchestrator")[0].tools.iter().any(|t| t.starts_with("mantra_akasha_")));
            assert_eq!(run.with_akasha(o, "[mantra:watchdog] nudge".into()), "[mantra:watchdog] nudge");
            let _ = std::fs::remove_dir_all(&run.dir);
        }

        #[test]
        fn the_tools_act_on_the_members_block_and_refuse_everyone_else() {
            let mut ctx = TestCtx::new();
            let mut run = run_of(grouped("none", true, 16_384), true, "tools");
            let a = member(&mut ctx, &mut run, "p1-a");
            let (r, ok) = call(&mut ctx, &mut run, a, "mantra_akasha_note", json!({"text": "  the config loader lives in src/config.rs ", "urgent": true}));
            assert!(ok && r.starts_with("noted #1 (") && r.ends_with("/16k)"), "{r}");
            assert!(call(&mut ctx, &mut run, a, "mantra_akasha_note", json!({"text": "temporary"})).0.starts_with("noted #2"));
            let (r, ok) = call(&mut ctx, &mut run, a, "mantra_akasha_amend", json!({"id": "#1", "text": "the config loader lives in src/cfg.rs"}));
            assert!(ok && r.starts_with("amended #1"), "{r}");
            let (r, ok) = call(&mut ctx, &mut run, a, "mantra_akasha_forget", json!({"id": 2}));
            assert!(ok && r.starts_with("forgot #2"), "{r}");
            let (r, ok) = call(&mut ctx, &mut run, a, "mantra_akasha_forget", json!({"id": 2}));
            assert!(!ok && r.contains("no note #2"), "{r}");
            let (r, ok) = call(&mut ctx, &mut run, a, "mantra_akasha_amend", json!({"text": "no id"}));
            assert!(!ok && r.contains("id"), "{r}");
            let (r, ok) = call(&mut ctx, &mut run, a, "mantra_akasha_read", json!({}));
            assert!(ok && r.starts_with("## akasha · builders · 1 note · ") && r.contains("\n![1 · p1-a · ") && r.ends_with("the config loader lives in src/cfg.rs"), "{r}");
            // one journal line for each change, and not one toast
            let said: Vec<String> = pulse_texts(&run).into_iter().filter(|l| l.starts_with("akasha ·")).collect();
            assert_eq!(said, vec!["akasha · p1-a noted #1 (urgent): the config loader lives in src/config.rs", "akasha · p1-a noted #2: temporary", "akasha · p1-a amended #1: the config loader lives in src/cfg.rs", "akasha · p1-a forgot #2"]);
            assert!(ctx.notified.is_empty());
            call(&mut ctx, &mut run, a, "mantra_akasha_note", json!({"text": "a long one\n".repeat(30)}));
            let last = pulse_texts(&run).pop().unwrap_or_default();
            assert!(last.chars().count() <= 100 && !last.contains('\n'), "{last}");
            // outside the group: the orchestrator, and an agent the run does not know
            let o = ctx.add_idle();
            run.orchestrator = Some(o);
            for who in [o, 999] {
                let (r, ok) = call(&mut ctx, &mut run, who, "mantra_akasha_read", json!({}));
                assert!(!ok && r.contains("not in a group"), "{r}");
            }
            let _ = std::fs::remove_dir_all(&run.dir);
        }

        #[test]
        fn urgent_notes_reach_busy_members_only_one_delivery_per_window() {
            let mut ctx = TestCtx::new();
            let mut run = run_of(grouped("urgent", true, 16_384), true, "push");
            let a = member(&mut ctx, &mut run, "p1-a");
            let b = member(&mut ctx, &mut run, "p1-b");
            let c = member(&mut ctx, &mut run, "p1-c");
            ctx.set_idle_secs(c, 0);
            let o = ctx.add_busy();
            run.orchestrator = Some(o);
            call(&mut ctx, &mut run, a, "mantra_akasha_note", json!({"text": "a plain fact"}));
            call(&mut ctx, &mut run, a, "mantra_akasha_note", json!({"text": "Config::load now returns Result", "urgent": true}));
            run.akasha_tick(&mut ctx);
            assert_eq!(pushes(&ctx, b), vec!["[akasha · p1-a] Config::load now returns Result"], "the urgent one only");
            assert!(pushes(&ctx, a).is_empty() && pushes(&ctx, c).is_empty() && pushes(&ctx, o).is_empty(), "not the author, not an idle member, not an outsider");
            // two more inside the window wait, then go as one
            call(&mut ctx, &mut run, a, "mantra_akasha_note", json!({"text": "second", "urgent": true}));
            call(&mut ctx, &mut run, a, "mantra_akasha_note", json!({"text": "third", "urgent": true}));
            run.akasha_tick(&mut ctx);
            assert_eq!(pushes(&ctx, b).len(), 1, "at most one delivery per 20 s");
            *run.akasha_live.pushed.get_mut(&b).unwrap() -= Duration::from_secs(21);
            run.akasha_tick(&mut ctx);
            assert_eq!(pushes(&ctx, b)[1], "[akasha · p1-a] second\n\n[akasha · p1-a] third");
            // a member that went idle before its window opened gets nothing (its next prompt has it)
            call(&mut ctx, &mut run, a, "mantra_akasha_note", json!({"text": "fourth", "urgent": true}));
            ctx.set_idle_secs(b, 0);
            *run.akasha_live.pushed.get_mut(&b).unwrap() -= Duration::from_secs(21);
            run.akasha_tick(&mut ctx);
            assert_eq!(pushes(&ctx, b).len(), 2);
            assert!(run.akasha_live.outbox.is_empty());
            // nor does one whose turn already yielded with mantra_wait
            ctx.agents.get_mut(&b).unwrap().turn_active = true;
            run.yielded.insert(b);
            call(&mut ctx, &mut run, a, "mantra_akasha_note", json!({"text": "fifth", "urgent": true}));
            *run.akasha_live.pushed.get_mut(&b).unwrap() -= Duration::from_secs(21);
            run.akasha_tick(&mut ctx);
            assert_eq!(pushes(&ctx, b).len(), 2);
            assert!(pushes(&ctx, c).is_empty(), "the idle member was never prompted");
            let _ = std::fs::remove_dir_all(&run.dir);
        }

        /// A member being stopped — by hand (the interrupt is on its way, the turn not over yet) or
        /// paused by the orchestrator — is not busy for a push: the steer would land in the turn
        /// being stopped, and a message clears the hand stop, putting the agent back to work.
        #[test]
        fn a_member_being_stopped_or_paused_gets_no_push() {
            let mut ctx = TestCtx::new();
            let mut run = run_of(grouped("urgent", true, 16_384), true, "stopping");
            let a = member(&mut ctx, &mut run, "p1-a");
            let b = member(&mut ctx, &mut run, "p1-b");
            let c = member(&mut ctx, &mut run, "p1-c");
            call(&mut ctx, &mut run, a, "mantra_akasha_note", json!({"text": "queued before the stop", "urgent": true}));
            assert!(run.akasha_live.outbox.contains_key(&b) && run.akasha_live.outbox.contains_key(&c));
            ctx.agents.get_mut(&b).unwrap().stopped_by_user = true;
            let wc = run.worker_idx(c).unwrap();
            run.workers[wc].paused = true;
            assert!(ctx.agents[&b].busy() && ctx.agents[&c].busy(), "both turns are still open");
            run.akasha_tick(&mut ctx);
            assert!(pushes(&ctx, b).is_empty() && pushes(&ctx, c).is_empty(), "what waited is not delivered after the stop");
            assert!(run.akasha_live.outbox.is_empty());
            call(&mut ctx, &mut run, a, "mantra_akasha_note", json!({"text": "noted during the stop", "urgent": true}));
            assert!(run.akasha_live.outbox.is_empty(), "nor queued for them");
            assert!(ctx.agents[&b].stopped_by_user, "the hand stop stands");
            // talked to again (the stop is over) and resumed: pushes reach them as before
            ctx.agents.get_mut(&b).unwrap().stopped_by_user = false;
            run.workers[wc].paused = false;
            call(&mut ctx, &mut run, a, "mantra_akasha_note", json!({"text": "after", "urgent": true}));
            run.akasha_tick(&mut ctx);
            assert_eq!((pushes(&ctx, b), pushes(&ctx, c)), (vec!["[akasha · p1-a] after".to_string()], vec!["[akasha · p1-a] after".to_string()]));
            let _ = std::fs::remove_dir_all(&run.dir);
        }

        /// A push goes as the note reads when it is delivered: amended in the window, with its new
        /// text; forgotten (retracted as wrong), not at all — and with nothing left, no push.
        #[test]
        fn a_push_goes_as_the_note_reads_at_delivery() {
            let mut ctx = TestCtx::new();
            let mut run = run_of(grouped("urgent", true, 16_384), true, "push-now");
            let a = member(&mut ctx, &mut run, "p1-a");
            let b = member(&mut ctx, &mut run, "p1-b");
            call(&mut ctx, &mut run, a, "mantra_akasha_note", json!({"text": "first", "urgent": true}));
            run.akasha_tick(&mut ctx);
            assert_eq!(pushes(&ctx, b).len(), 1, "the window is open now");
            call(&mut ctx, &mut run, a, "mantra_akasha_note", json!({"text": "Config::load returns Option", "urgent": true}));
            call(&mut ctx, &mut run, a, "mantra_akasha_note", json!({"text": "the tests live in tests/ (wrong)", "urgent": true}));
            call(&mut ctx, &mut run, a, "mantra_akasha_amend", json!({"id": 2, "text": "Config::load returns Result"}));
            call(&mut ctx, &mut run, a, "mantra_akasha_forget", json!({"id": 3}));
            *run.akasha_live.pushed.get_mut(&b).unwrap() -= Duration::from_secs(21);
            run.akasha_tick(&mut ctx);
            assert_eq!(pushes(&ctx, b)[1], "[akasha · p1-a] Config::load returns Result");
            call(&mut ctx, &mut run, a, "mantra_akasha_note", json!({"text": "oops", "urgent": true}));
            call(&mut ctx, &mut run, a, "mantra_akasha_forget", json!({"id": 4}));
            *run.akasha_live.pushed.get_mut(&b).unwrap() -= Duration::from_secs(21);
            let window = run.akasha_live.pushed[&b];
            run.akasha_tick(&mut ctx);
            assert_eq!(pushes(&ctx, b).len(), 2, "everything that waited was forgotten");
            assert!(run.akasha_live.outbox.is_empty());
            assert_eq!(run.akasha_live.pushed[&b], window, "and no window was spent on nothing");
            let _ = std::fs::remove_dir_all(&run.dir);
        }

        /// A push that lands while a done worker answers the orchestrator's follow-up asked nothing:
        /// the orchestrator still gets its answer, and the push reads as Mantra's, not the user's.
        #[test]
        fn a_push_into_a_follow_up_does_not_pass_for_the_users_message() {
            let mut ctx = TestCtx::new();
            let mut run = run_of(grouped("urgent", true, 16_384), true, "follow-up");
            let o = ctx.add_idle();
            run.orchestrator = Some(o);
            let a = member(&mut ctx, &mut run, "p1-a");
            let b = member(&mut ctx, &mut run, "p1-b");
            run.on_turn_done(&mut ctx, a, "completed", None, None);
            assert_eq!(run.workers[0].state, WState::Done);
            // the orchestrator asks the done worker something; it is answering when p1-b's urgent note comes
            ctx.agents.get_mut(&a).unwrap().push_user("[from the orchestrator] which file holds the parser?");
            assert!(ctx.agents[&a].busy());
            call(&mut ctx, &mut run, b, "mantra_akasha_note", json!({"text": "the parser moved to src/parse.rs", "urgent": true}));
            run.akasha_tick(&mut ctx);
            let push = pushes(&ctx, a).pop().expect("pushed into the answering turn");
            // what `app::prompt_agent` echoes into the transcript
            ctx.agents.get_mut(&a).unwrap().push_user(&push);
            assert_eq!(ctx.agents[&a].tail_lines(1), vec!["⇢ akasha · p1-b".to_string()], "Mantra's line, not one the user typed");
            run.on_turn_done(&mut ctx, a, "completed", None, None);
            let last = pulse_texts(&run).into_iter().rev().find(|l| l.contains("p1-a done")).unwrap_or_default();
            assert!(last.contains("p1-a done again in"), "{last}");
            let heard = ctx.prompts.iter().filter(|(to, t)| *to == o && t.contains("p1-a answered the follow-up")).count() + run.orch_inbox.iter().filter(|t| t.contains("p1-a answered the follow-up")).count();
            assert_eq!(heard, 1, "{:?} {:?}", ctx.prompts, run.orch_inbox);
            let _ = std::fs::remove_dir_all(&run.dir);
        }

        /// The manager's own retries, which `manager_tick` sends ahead of `tick`, end with the
        /// notes too when its role is in a group — like every member's retry prompt.
        #[test]
        fn a_grouped_managers_retries_carry_the_notes() {
            let mut ctx = TestCtx::new();
            let mut pattern = grouped("urgent", true, 16_384);
            pattern.groups[0].roles.push("manager".into());
            let mut run = run_of(pattern, true, "manager");
            let m = ctx.add_idle();
            run.manager = Some(m);
            assert_eq!(run.group_of(m), Some("builders"));
            run.akasha.get_mut("builders").unwrap().note("p1-a", "models live in src/models", false, 0).unwrap();
            run.continue_queue.push((m, Instant::now(), "[mantra:retry] Your previous turn failed (x). Continue exactly where you left off.".into()));
            run.manager_tick(&mut ctx);
            let retry = ctx.prompts.iter().find(|(x, t)| *x == m && t.starts_with("[mantra:retry]")).map(|(_, t)| t.clone()).expect("the retry went out");
            assert!(retry.contains("\n\n## Shared notes (akasha · builders)\n## akasha · builders · 1 note"), "{retry}");
            let _ = std::fs::remove_dir_all(&run.dir);
        }

        #[test]
        fn push_all_sends_every_note_and_push_none_sends_none() {
            for (push, urgent, expect) in [("all", false, 1), ("none", true, 0), ("urgent", false, 0)] {
                let mut ctx = TestCtx::new();
                let mut run = run_of(grouped(push, true, 16_384), true, &format!("push-{push}"));
                let a = member(&mut ctx, &mut run, "p1-a");
                let b = member(&mut ctx, &mut run, "p1-b");
                call(&mut ctx, &mut run, a, "mantra_akasha_note", json!({"text": "a fact", "urgent": urgent}));
                run.akasha_tick(&mut ctx);
                assert_eq!(pushes(&ctx, b).len(), expect, "push = {push}, urgent = {urgent}");
                let _ = std::fs::remove_dir_all(&run.dir);
            }
        }

        #[test]
        fn the_conductor_notes_what_members_finished_failed_and_changed() {
            let mut ctx = TestCtx::new();
            let mut run = run_of(grouped("all", true, 16_384), true, "auto");
            let o = ctx.add_idle();
            run.orchestrator = Some(o);
            let a = member(&mut ctx, &mut run, "p1-a");
            let b = member(&mut ctx, &mut run, "p1-b");
            if let Some(ag) = ctx.agents.get_mut(&a) {
                ag.final_message = Some("Implemented it.\n\nSTATUS: done\nSUMMARY: added Config with a loader; tests pass".into());
                for f in ["src/a.rs", "src/b.rs", "src/c.rs", "src/d.rs", "src/e.rs"] {
                    ag.files.insert(f.into(), Default::default());
                }
            }
            run.on_turn_done(&mut ctx, a, "completed", None, None);
            assert_eq!(texts(&run), vec!["[p1-a · done] added Config with a loader; tests pass", "[p1-a · changed] src/a.rs, src/b.rs, src/c.rs, +2 more"]);
            assert!(run.akasha["builders"].entries.iter().all(|e| e.author == "mantra" && !e.urgent));
            run.akasha_tick(&mut ctx);
            assert!(pushes(&ctx, b).is_empty(), "the Conductor's entries are never pushed, even with push = all");
            // a follow-up the done worker answers is not a second completion
            if let Some(ag) = ctx.agents.get_mut(&a) {
                ag.push_user("[from the orchestrator] which file?");
                ag.final_message = Some("STATUS: done\nSUMMARY: src/config.rs".into());
            }
            run.on_turn_done(&mut ctx, a, "completed", None, None);
            assert_eq!(texts(&run).len(), 2);
            // failed: the error's first line and the attempt; blocked: the report's head
            run.on_turn_done(&mut ctx, b, "failed", Some("the build script exited 101\nstack backtrace".into()), None);
            assert_eq!(texts(&run)[2], "[p1-b · failed, attempt 1] the build script exited 101");
            let c = member(&mut ctx, &mut run, "p1-c");
            if let Some(ag) = ctx.agents.get_mut(&c) {
                ag.final_message = Some("STATUS: blocked\nSUMMARY: the spec names no database".into());
            }
            run.on_turn_done(&mut ctx, c, "completed", None, None);
            assert_eq!(texts(&run)[3], "[p1-c · failed, attempt 1] blocked: the spec names no database");
            assert_eq!(texts(&run).len(), 4);
            assert!(!pulse_texts(&run).iter().any(|l| l.starts_with("akasha ·")), "they mirror lines the journal already has");
            let _ = std::fs::remove_dir_all(&run.dir);
        }

        #[test]
        fn a_full_block_drops_the_conductors_entries_and_says_so_once() {
            let mut ctx = TestCtx::new();
            let mut run = run_of(grouped("urgent", true, 1024), true, "full");
            let o = ctx.add_idle();
            run.orchestrator = Some(o);
            let a = member(&mut ctx, &mut run, "p1-a");
            let (r, ok) = call(&mut ctx, &mut run, a, "mantra_akasha_note", json!({"text": "x".repeat(950)}));
            assert!(ok, "{r}");
            for (who, id) in [(a, "p1-a"), (member(&mut ctx, &mut run, "p1-b"), "p1-b")] {
                if let Some(ag) = ctx.agents.get_mut(&who) {
                    ag.final_message = Some(format!("STATUS: done\nSUMMARY: {id} finished a task whose summary does not fit in what is left"));
                }
                run.on_turn_done(&mut ctx, who, "completed", None, None);
            }
            assert_eq!(run.akasha["builders"].entries.len(), 1, "the agent's note stays, the Conductor's entries are dropped");
            let full: Vec<String> = pulse_texts(&run).into_iter().filter(|l| l.contains("is full")).collect();
            assert_eq!(full, vec!["akasha · builders is full — Mantra's own entries are dropped until notes are forgotten".to_string()], "once, not per drop");
            // room again: they come back
            call(&mut ctx, &mut run, a, "mantra_akasha_forget", json!({"id": 1}));
            let c = member(&mut ctx, &mut run, "p1-c");
            if let Some(ag) = ctx.agents.get_mut(&c) {
                ag.final_message = Some("STATUS: done\nSUMMARY: fits now".into());
            }
            run.on_turn_done(&mut ctx, c, "completed", None, None);
            assert_eq!(texts(&run), vec!["[p1-c · done] fits now"]);
            let _ = std::fs::remove_dir_all(&run.dir);
        }

        #[test]
        fn the_blocks_survive_a_save_and_a_resume() {
            let mut run = run_of(grouped("urgent", true, 16_384), true, "state");
            run.akasha.get_mut("builders").unwrap().note("p1-a", "kept across a restart", true, 7).unwrap();
            run.save_state();
            let st = RunState::load(&run.dir).unwrap();
            assert_eq!(st.akasha, run.akasha);
            let mut back = Run::from_state(&st, run.pattern.clone(), None, run.dir.clone());
            assert_eq!(back.akasha, run.akasha);
            assert_eq!(back.group_of_role("worker-small"), None, "off until the app says otherwise");
            back.enable_akasha(true);
            assert_eq!(back.akasha, run.akasha, "the restored block is kept, not rebuilt empty");
            assert_eq!(back.group_of_role("worker-small"), Some("builders"));
            let _ = std::fs::remove_dir_all(&run.dir);
        }
    }

    /// The reported bug: a done worker the user messaged finished again and the pulse read
    /// "✓ p1-a done in …" twice, the orchestrator woken for a second completion. The second
    /// one says what it is, and the orchestrator hears about the task once.
    #[test]
    fn a_done_worker_finishing_again_is_a_follow_up_not_a_second_completion() {
        use crate::engine::plan::{Phase, Plan};
        let mut ctx = TestCtx::new();
        let o = ctx.add_idle();
        let a = ctx.add_busy();
        let b = ctx.add_busy();
        let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "test".into());
        let task = |id: &str| Task { id: id.into(), role: "worker-small".into(), ..Default::default() };
        run.plan = Some(Plan { phases: vec![Phase { id: "p1".into(), tasks: vec![task("p1-a"), task("p1-b")], ..Default::default() }], ..Default::default() });
        run.stage = Stage::Phase { idx: 0, step: PhaseStep::Orchestrating };
        run.orchestrator = Some(o);
        run.workers.push(mk_worker("p1-a", "worker-small", a, WState::Running));
        run.workers.push(mk_worker("p1-b", "worker-small", b, WState::Running));
        run.on_turn_done(&mut ctx, a, "completed", None, None);
        assert!(run.pulse.back().unwrap().text.contains("p1-a done in"), "{:?}", run.pulse.back().map(|p| &p.text));
        ctx.agents.get_mut(&a).unwrap().push_user("also add a test for the empty case");
        run.on_turn_done(&mut ctx, a, "completed", None, None);
        assert!(run.pulse.back().unwrap().text.contains("p1-a done again (after your message) in"), "{:?}", run.pulse.back().map(|p| &p.text));
        // `@p1-a …` from the stage arrives as "[from the user] …": the user's message too
        ctx.agents.get_mut(&a).unwrap().push_user("[from the user] and document it");
        run.on_turn_done(&mut ctx, a, "completed", None, None);
        assert!(run.pulse.back().unwrap().text.contains("p1-a done again (after your message) in"), "{:?}", run.pulse.back().map(|p| &p.text));
        let heard = |run: &Run, ctx: &TestCtx, what: &str| ctx.prompts.iter().filter(|(to, t)| *to == o && t.contains(what)).count() + run.orch_inbox.iter().filter(|t| t.contains(what)).count();
        assert_eq!(heard(&run, &ctx, "p1-a finished"), 1, "{:?}", ctx.prompts);
        assert_eq!(heard(&run, &ctx, "p1-a answered"), 0, "the user's side conversation wakes nobody");
        // the orchestrator's own follow-up to a done worker gets its answer — as an answer
        ctx.agents.get_mut(&a).unwrap().push_user("[from the orchestrator] which file holds the parser?");
        run.on_turn_done(&mut ctx, a, "completed", None, None);
        assert!(run.pulse.back().unwrap().text.contains("p1-a done again in"), "{:?}", run.pulse.back().map(|p| &p.text));
        assert_eq!(heard(&run, &ctx, "p1-a finished"), 1, "{:?}", ctx.prompts);
        assert_eq!(heard(&run, &ctx, "p1-a answered the follow-up"), 1, "{:?} {:?}", ctx.prompts, run.orch_inbox);
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

    #[test]
    fn run_clock_counts_active_time_only() {
        let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "test".into());
        run.set_clock(0, Some(1000));
        assert_eq!(run.elapsed_at(1005).as_secs(), 5);
        // a 10s pause inside a 30s run → 20s of active time
        run.clock_stop_at(1010);
        assert_eq!(run.elapsed_at(1015).as_secs(), 10, "frozen while halted");
        run.clock_start_at(1020);
        assert_eq!(run.elapsed_at(1030).as_secs(), 20);
        // a stopped run is frozen the same way, and a resume does not restart it
        run.stage = Stage::Done;
        run.clock_stop_at(1030);
        run.clock_start_at(1100);
        assert_eq!(run.elapsed_at(1200).as_secs(), 20, "a finished run keeps its duration");
        // stopping twice is harmless
        run.clock_stop_at(1300);
        assert_eq!(run.elapsed_at(1400).as_secs(), 20);
    }

    #[test]
    fn user_pause_freezes_the_run_clock() {
        let mut ctx = TestCtx::new();
        let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "test".into());
        run.set_clock(12, Some(crate::util::unix_secs()));
        run.toggle_pause(&mut ctx);
        assert!(run.active_since.is_none(), "space stops the clock");
        assert_eq!(run.elapsed().as_secs(), 12);
        run.toggle_pause(&mut ctx);
        assert!(run.active_since.is_some(), "resume starts a new active span");
        assert_eq!(run.active_secs, 12);
    }

    #[test]
    fn halt_and_resume_bracket_an_active_span() {
        let mut ctx = TestCtx::new();
        let busy = ctx.add_busy();
        let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "test".into());
        run.orchestrator = Some(busy);
        run.halt(&mut ctx, HaltReason::AgentTurnFailed, Some(busy), "boom".into());
        assert!(run.active_since.is_none());
        run.resume(&mut ctx);
        assert!(run.active_since.is_some());
        run.fail_run(&mut ctx, "gave up".into());
        assert!(run.active_since.is_none(), "a stopped run is frozen");
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

    /// Let the fix worker of gate round `round` do its job the way the engine sees it: its
    /// workspace job returns, it starts, its turn completes (which re-runs the checks, and with no
    /// checks and no workspace goes straight on to the next QA round).
    fn finish_fix(ctx: &mut TestCtx, run: &mut Run, round: u32) {
        let id = format!("fix-{round}");
        let attempt = run.workers.iter().rev().find(|w| w.task.id == id).map(|w| w.attempt).expect("a fix worker was created");
        run.on_job(ctx, JobTag::WorkerWt { task: id.clone(), attempt }, JobOut::Wt(Ok((PathBuf::from("."), String::new()))));
        let agent = run.workers.iter().rev().find(|w| w.task.id == id).and_then(|w| w.agent).expect("the fix worker started");
        ctx.agents.get_mut(&agent).unwrap().final_message = Some("STATUS: done\nSUMMARY: fixed what was listed".into());
        run.on_turn_done(ctx, agent, "completed", None, None);
    }

    /// A QA agent's turn ends with this verdict.
    fn gate_says(ctx: &mut TestCtx, run: &mut Run, gate: AgentId, text: &str) {
        ctx.agents.get_mut(&gate).unwrap().final_message = Some(text.into());
        run.on_turn_done(ctx, gate, "completed", None, None);
    }

    /// Two identical failing QA verdicts with a fix in between: the loop protection halts. Returns
    /// the second (fresh) QA agent. `run` must be at gate round 1 with `gate` as its agent.
    fn drive_gate_to_halt(ctx: &mut TestCtx, run: &mut Run, gate: AgentId, blocked: &str) -> AgentId {
        gate_says(ctx, run, gate, blocked);
        assert!(!run.halted(), "the first blocked round goes to a fix worker, it does not halt");
        finish_fix(ctx, run, 1);
        let second = run.gate_agent.expect("round 2 has a fresh QA agent");
        assert_ne!(second, gate, "every round is a fresh agent");
        gate_says(ctx, run, second, blocked);
        assert!(run.halted());
        second
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

    #[test]
    fn watchdog_does_not_nudge_an_idle_worker_waiting_for_its_swarm() {
        let mut ctx = TestCtx::new();
        let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "test".into());
        let worker = ctx.add_idle();
        run.plan = Some(one_task_phase());
        run.stage = Stage::Phase { idx: 0, step: PhaseStep::Orchestrating };
        run.workers.push(mk_worker("t1", "worker-small", worker, WState::Running));
        ctx.set_idle_secs(worker, 500);
        ctx.agents.get_mut(&worker).unwrap().swarm_waiting = true;
        run.watchdog_tick(&mut ctx);
        assert!(ctx.prompts.iter().all(|(a, _)| *a != worker));
        assert!(run.watchdog_idle(worker).is_none());
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

    /// `Stage::Review` (right after the plan is submitted) has no current phase for
    /// `current_phase()` to show — the respawned planner must still get the plan it just wrote,
    /// not an empty `Current phase:` block it can't recover from.
    #[test]
    fn respawn_planner_during_review_attaches_the_plan_not_an_empty_phase() {
        let mut ctx = TestCtx::new();
        let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "test".into());
        let planner = ctx.add_idle();
        run.planner = Some(planner);
        run.stage = Stage::Review;
        run.plan = Some(one_task_phase());
        run.plan_version = 1;

        run.respawn(&mut ctx, planner, None).unwrap();

        let new_planner = run.planner.expect("planner still set after respawn");
        let prompt = ctx.prompts.iter().find(|(a, t)| *a == new_planner && t.contains("[mantra:respawn]")).map(|(_, t)| t.clone()).expect("planner briefed");
        assert!(prompt.contains("\"p1\""), "the plan just submitted must be attached, not an empty phase block: {prompt}");
        assert!(!prompt.contains("Current phase:\n```json\n\n```"), "{prompt}");
    }

    /// The planner can read the full plan on demand — the tool the respawn prompt now points to
    /// instead of promising something no tool ever delivered.
    #[test]
    fn mantra_read_plan_returns_the_full_plan() {
        let mut ctx = TestCtx::new();
        let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "test".into());
        let planner = ctx.add_idle();
        run.planner = Some(planner);
        let (msg, ok) = run.handle_tool(&mut ctx, planner, "mantra_read_plan", &json!({}));
        assert!(!ok, "{msg}");
        let mut plan = one_task_phase();
        plan.phases.push(Phase { id: "p2".into(), name: "two".into(), tasks: vec![Task { id: "t2".into(), role: "worker-small".into(), ..Default::default() }], ..Default::default() });
        run.plan = Some(plan);

        let (msg, ok) = run.handle_tool(&mut ctx, planner, "mantra_read_plan", &json!({}));
        assert!(ok, "{msg}");
        assert!(msg.contains("\"p1\"") && msg.contains("\"p2\""), "every phase must be in it, not just the current one: {msg}");
    }

    /// The finale steps used `spawn_gate_at_round`'s sandbox-aware note too; a read-only finale
    /// role must be told it cannot write, exactly like a per-phase gate.
    #[test]
    fn start_finale_gives_a_read_only_role_the_sandbox_note() {
        let mut ctx = TestCtx::new();
        let mut pattern = Pattern::builtin();
        pattern.roles.get_mut("qa-heavy").unwrap().sandbox = "read-only".into();
        let mut run = Run::new(PathBuf::from("."), pattern, "test".into());
        run.plan = Some(one_task_phase());

        run.start_finale(&mut ctx, 0);

        let agent = run.finale_agent.expect("a finale agent must be spawned");
        let instructions = ctx.spawned_instructions.iter().find(|(id, _)| *id == agent).map(|(_, i)| i.clone()).expect("instructions captured");
        assert!(instructions.contains("Your role cannot write files"), "{instructions}");
    }

    /// A respawned planner used to be handed the whole plan (every phase, every task) — exactly
    /// the huge context a respawn is meant to save. It gets only the current phase now, plus
    /// `mantra_read_phase` for live task states; the full plan is one call away if it needs it.
    #[test]
    fn respawn_planner_sends_only_the_current_phase() {
        let mut ctx = TestCtx::new();
        let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "test".into());
        let planner = ctx.add_idle();
        run.planner = Some(planner);
        let mut plan = one_task_phase();
        plan.phases.push(Phase { id: "p2".into(), name: "two".into(), tasks: vec![Task { id: "t2".into(), role: "worker-small".into(), ..Default::default() }], ..Default::default() });
        run.plan = Some(plan);
        run.plan_version = 3;
        run.stage = Stage::Phase { idx: 0, step: PhaseStep::Orchestrating };

        run.respawn(&mut ctx, planner, None).unwrap();

        let new_planner = run.planner.expect("planner still set after respawn");
        let prompt = ctx.prompts.iter().find(|(a, t)| *a == new_planner && t.contains("[mantra:respawn]")).map(|(_, t)| t.clone()).expect("planner briefed");
        assert!(prompt.contains("plan v3"), "{prompt}");
        assert!(prompt.contains("\"p1\""), "the current phase must be included: {prompt}");
        assert!(!prompt.contains("\"p2\""), "a later phase must not be attached — that is what mantra_read_phase / mantra_revise_plan are for: {prompt}");
        assert!(prompt.contains("mantra_read_phase"), "{prompt}");
    }

    // Worker classes ------------------------------------------------------------------------------

    /// One phase of `big` worker-big tasks, then `swarm` worker-swarm tasks, disjoint scopes.
    fn class_phase(big: usize, swarm: usize) -> Plan {
        let task = |id: String, role: &str| Task { scope: vec![format!("src/{id}/**")], id, role: role.into(), prompt: "do one small thing well".into(), ..Default::default() };
        let mut tasks: Vec<Task> = (0..big).map(|i| task(format!("b{i}"), "worker-big")).collect();
        tasks.extend((0..swarm).map(|i| task(format!("s{i}"), "worker-swarm")));
        Plan { phases: vec![Phase { id: "p1".into(), name: "p1".into(), tasks, ..Default::default() }], ..Default::default() }
    }

    /// A run in phase 1 of `plan` with every task of it spawned, in plan order; its directory is
    /// a scratch one of its own (`tag`).
    fn spawned_phase(ctx: &mut TestCtx, plan: Plan, tag: &str) -> Run {
        let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "test".into());
        run.dir = scratch_dir(tag);
        run.stage = Stage::Phase { idx: 0, step: PhaseStep::Orchestrating };
        for t in plan.phases[0].tasks.clone() {
            run.spawn_task(ctx, t, None, None);
        }
        run.plan = Some(plan);
        run
    }

    fn holding(run: &Run, role: &str) -> usize {
        run.workers.iter().filter(|w| w.task.role == role && holds_slot(w)).count()
    }

    fn queued(run: &Run) -> Vec<&str> {
        run.workers.iter().filter(|w| w.state == WState::Queued).map(|w| w.task.id.as_str()).collect()
    }

    /// Each class fills its own slots: under `max_parallel` = 4 and worker-swarm's own 16, the big
    /// workers take the shared pool (all 3 of them, or 4 of 6) and 16 swarm tasks run beside them;
    /// what is left queues in its own class, and the journal names that class's cap.
    #[test]
    fn each_worker_class_fills_its_own_slots() {
        for (big, running_big, queued_big) in [(3, 3, 0), (6, 4, 2)] {
            let mut ctx = TestCtx::new();
            let run = spawned_phase(&mut ctx, class_phase(big, 20), &format!("classes-fill-{big}"));
            assert_eq!((run.pattern.settings.max_parallel, run.pattern.cap_of("worker-swarm")), (4, 16));
            assert_eq!((holding(&run, "worker-big"), holding(&run, "worker-swarm")), (running_big, 16), "{big} big tasks: {running_big} + 16 at once");
            let q = queued(&run);
            assert_eq!(q.iter().filter(|t| t.starts_with('b')).count(), queued_big);
            assert_eq!(q.iter().filter(|t| t.starts_with('s')).copied().collect::<Vec<_>>(), vec!["s16", "s17", "s18", "s19"]);
            let said: Vec<&str> = run.pulse.iter().map(|p| p.text.as_str()).collect();
            assert!(said.contains(&"s16 queued (worker-swarm: 16 at once)"), "{said:?}");
            if queued_big > 0 {
                assert!(said.contains(&"b4 queued (max_parallel = 4)"), "{said:?}");
            }
            let _ = std::fs::remove_dir_all(&run.dir);
        }
    }

    /// `fill_slots` walks the whole queue in order and admits each task against its own class:
    /// a queued swarm task behind a big one that has to wait still starts when a swarm slot frees,
    /// and the big one starts when a slot of the pool does — never the other way round.
    #[test]
    fn a_queued_swarm_task_behind_a_waiting_big_one_starts() {
        let mut ctx = TestCtx::new();
        let mut run = spawned_phase(&mut ctx, class_phase(5, 17), "classes-queue");
        assert_eq!(queued(&run), vec!["b4", "s16"], "the big task queued first");
        let done = |run: &mut Run, id: &str| run.workers.iter_mut().find(|w| w.task.id == id).unwrap().state = WState::Done;
        done(&mut run, "s0");
        run.fill_slots(&mut ctx);
        assert_eq!(queued(&run), vec!["b4"], "the swarm task went past the waiting big one");
        assert_eq!((holding(&run, "worker-big"), holding(&run, "worker-swarm")), (4, 16));
        done(&mut run, "s1");
        run.fill_slots(&mut ctx);
        assert_eq!(queued(&run), vec!["b4"], "a free swarm slot is not the pool's");
        done(&mut run, "b0");
        run.fill_slots(&mut ctx);
        assert!(queued(&run).is_empty());
        assert_eq!((holding(&run, "worker-big"), holding(&run, "worker-swarm")), (4, 15));
        let _ = std::fs::remove_dir_all(&run.dir);
    }

    /// A fix worker (`fix-…`, a big or small worker) and a task naming a role the pattern does
    /// not have count in the shared pool; `--parallel N` widens every class to N.
    #[test]
    fn the_shared_pool_takes_fix_workers_and_unknown_roles_and_parallel_widens_every_class() {
        let mut ctx = TestCtx::new();
        let mut run = spawned_phase(&mut ctx, class_phase(3, 0), "classes-pool");
        run.spawn_task(&mut ctx, Task { id: "fix-1".into(), role: "worker-small".into(), prompt: "fix it".into(), ..Default::default() }, None, None);
        run.spawn_task(&mut ctx, Task { id: "odd".into(), role: "worker-gone".into(), prompt: "do it".into(), ..Default::default() }, None, None);
        assert_eq!(queued(&run), vec!["odd"], "3 big + the fix worker fill the pool of 4");
        assert_eq!(run.slots_taken("worker-gone"), 4);
        let mut ctx = TestCtx::new();
        let mut p = Pattern::builtin();
        p.set_parallel(8);
        let mut run = Run::new(PathBuf::from("."), p, "test".into());
        run.dir = scratch_dir("classes-wide");
        run.stage = Stage::Phase { idx: 0, step: PhaseStep::Orchestrating };
        for t in class_phase(10, 10).phases[0].tasks.clone() {
            run.spawn_task(&mut ctx, t, None, None);
        }
        assert_eq!((holding(&run, "worker-big"), holding(&run, "worker-swarm")), (8, 8), "--parallel 8: 8 in every class");
        let _ = std::fs::remove_dir_all(&run.dir);
    }

    /// The spawn line says how full the worker's class is, this one included — the width a run
    /// actually reached, which the swarm e2e reads back from the journal.
    #[test]
    fn the_spawn_line_says_how_full_the_class_is() {
        let mut ctx = TestCtx::new();
        let mut run = spawned_phase(&mut ctx, class_phase(2, 3), "classes-spawn-line");
        for (i, id) in ["b0", "s0", "s1"].iter().enumerate() {
            run.on_job(&mut ctx, JobTag::WorkerWt { task: id.to_string(), attempt: 1 }, JobOut::Wt(Ok((PathBuf::from("."), format!("br{i}")))));
        }
        let said: Vec<&str> = run.pulse.iter().map(|p| p.text.as_str()).collect();
        for want in ["spawned b0 (worker-big · 2/4 at once)", "spawned s0 (worker-swarm · 3/16 at once)", "spawned s1 (worker-swarm · 3/16 at once)"] {
            assert!(said.contains(&want), "{want}: {said:?}");
        }
        let _ = std::fs::remove_dir_all(&run.dir);
    }

    /// The run brief lists each worker role with how many of it run at once — the swarm its own
    /// 16, the others the pool they share — and the wider task limit of a swarm phase.
    #[test]
    fn the_run_brief_lists_each_worker_role_with_its_cap() {
        let run = Run::new(PathBuf::from("."), Pattern::builtin(), "test".into());
        let brief = run.planning_prompt();
        for want in ["- worker-swarm — up to 16 at once: Many tiny, independent tasks at once (model luna, effort auto)", "- worker-big — up to 4 at once, shared with worker-small: ", "- worker-small — up to 4 at once, shared with worker-big: ", "Constraints: at most 8 tasks per phase (32 in a phase that uses worker-swarm); "] {
            assert!(brief.contains(want), "{want}\n{brief}");
        }
        assert!(!brief.contains("run at once;"), "the old single number is gone: {brief}");
        let mut p = Pattern::builtin();
        p.roles.retain(|n, _| n != "worker-swarm" && n != "worker-small");
        p.groups.clear();
        let brief = Run::new(PathBuf::from("."), p, "test".into()).planning_prompt();
        assert!(brief.contains("- worker-big — up to 4 at once: ") && brief.contains("Constraints: at most 8 tasks per phase; "), "{brief}");
        let manager = Run::new(PathBuf::from("."), Pattern::builtin(), "test".into()).manager_brief("lead");
        assert!(manager.contains("tasks in parallel: 4 at once · worker-swarm 16;"), "{manager}");
    }

    // Stopping a run -------------------------------------------------------------------------------

    /// `stop` ends the run for good: busy agents interrupted, every agent shut down, the workers
    /// still queued or running cancelled, a halt and its alerts gone, `Failed(STOPPED_BY_USER)`
    /// saved — "stopped" on disk — without a notification; a job still in flight starts nothing,
    /// and the stopped run reopens read-only, spawning nothing.
    #[test]
    fn stop_shuts_down_every_agent_and_ends_the_run_stopped() {
        let mut ctx = TestCtx::new();
        let mut run = spawned_phase(&mut ctx, class_phase(5, 0), "stop");
        let (orch, idle_worker, busy_worker) = (ctx.add_busy(), ctx.add_idle(), ctx.add_busy());
        run.orchestrator = Some(orch);
        run.workers[0].agent = Some(idle_worker);
        run.workers[0].state = WState::Running;
        run.workers[1].agent = Some(busy_worker);
        run.workers[1].state = WState::Running;
        run.workers[2].state = WState::Done;
        run.halt(&mut ctx, HaltReason::AgentTurnFailed, Some(busy_worker), "the gateway is down".into());
        // the halt interrupted them; busy again, as an agent mid-turn when the user stops looks
        for a in [orch, busy_worker] {
            ctx.agents.get_mut(&a).unwrap().turn_active = true;
        }
        ctx.interrupted.clear();
        ctx.notified.clear();
        let spawns = ctx.spawns.len();
        run.stop(&mut ctx);
        assert_eq!(ctx.interrupted, vec![orch, busy_worker], "the busy ones are interrupted");
        let mut stopped = ctx.stopped.clone();
        stopped.sort();
        assert_eq!(stopped, vec![orch, idle_worker, busy_worker], "every agent of the run is shut down");
        let states: Vec<&str> = run.workers.iter().map(|w| worker_state_str(&w.state)).collect();
        assert_eq!(states, vec!["cancelled", "cancelled", "done", "cancelled", "cancelled"], "b3 and b4 were queued or preparing");
        assert!(!run.halted() && run.alerts.is_empty() && !run.is_active());
        assert_eq!(run.stage, Stage::Failed(STOPPED_BY_USER.into()));
        assert!(run.pulse.iter().any(|p| p.text == "run stopped by you"));
        assert!(ctx.notified.is_empty(), "no notification for the user's own key: {:?}", ctx.notified);
        let st = RunState::load(&run.dir).expect("saved");
        assert_eq!((st.stage_label(), st.finished(), st.halted.is_none()), ("stopped".to_string(), true, true));
        assert!(st.finished_unix.is_some());
        // a merge that was in flight does not start a gate in the stopped run
        run.on_job(&mut ctx, JobTag::Merge { phase: 0 }, JobOut::Merge(git::MergeResult { merged: vec!["b2".into()], conflicts: vec![], log: String::new() }));
        assert_eq!((ctx.spawns.len(), &run.stage), (spawns, &Stage::Failed(STOPPED_BY_USER.into())));
        // stopping twice changes nothing
        let lines = run.pulse.len();
        run.stop(&mut ctx);
        assert_eq!(run.pulse.len(), lines);
        // reopened from `/runs`: read-only, nothing spawned
        let mut ctx2 = TestCtx::new();
        let mut again = Run::from_state(&st, Pattern::builtin(), run.plan.clone(), run.dir.clone());
        again.resume_boot(&mut ctx2, &st);
        assert!(ctx2.spawns.is_empty() && !again.is_active());
        assert!(again.pulse.iter().any(|p| p.text.starts_with("you stopped this run — opened read-only")), "{:?}", again.pulse.iter().map(|p| p.text.clone()).collect::<Vec<_>>());
        let _ = std::fs::remove_dir_all(&run.dir);
    }

    #[test]
    fn successful_handoff_remembers_the_done_agent_not_an_earlier_failed_attempt() {
        let mut ctx = TestCtx::new();
        let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "completion".into());
        run.dir = scratch_dir("retired-worker-completion");
        run.plan = Some(class_phase(1, 0));
        run.stage = Stage::Phase { idx: 0, step: PhaseStep::Gate { round: 0 } };
        let failed = ctx.add_idle();
        let done = ctx.add_idle();
        run.workers.push(mk_worker("b0", "worker-big", failed, WState::Failed("previous attempt".into())));
        let mut latest = mk_worker("b0", "worker-big", done, WState::Done);
        latest.attempt = 2;
        run.workers.push(latest);
        run.begin_handoff(&mut ctx, 0);
        assert!(run.completed_agents.contains(&done));
        assert!(!run.completed_agents.contains(&failed));
        run.workers.clear();
        assert!(run.completed_agents.contains(&done), "the completion survives the next phase clearing its worker list");
        let _ = std::fs::remove_dir_all(&run.dir);
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
        run.stage = Stage::Finale { idx: 1 }; // built-in pattern's last finale step (1) is the planner
        run.plan = Some(Plan { title: "x".into(), ..Default::default() });

        run.respawn(&mut ctx, planner, None).unwrap();

        assert!(matches!(run.stage, Stage::Finale { idx: 1 }), "must stay on the finale step, got {:?}", run.stage);
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

        match &run.stage {
            Stage::Phase { step: PhaseStep::Gate { round }, .. } => assert_eq!(*round, 2, "a respawned gate must keep its round, not reset to 1"),
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
        drive_gate_to_halt(&mut ctx, &mut run, gate, blocked);
        assert_eq!(run.halt.as_ref().unwrap().reason, HaltReason::GateExhausted);
        assert!(run.halt.as_ref().unwrap().message.contains("same blocker twice"), "two consecutive identical blockers must halt, not spend the remaining rounds");
    }

    /// The gate never fixes: a failing verdict goes to a fix worker named `fix-<round>`, scoped to
    /// the files the findings name, with the findings as its prompt — not to the orchestrator and
    /// not back to the gate agent.
    #[test]
    fn a_failing_gate_report_goes_to_a_fix_worker_not_the_orchestrator() {
        let dir = std::env::temp_dir().join(format!("mantra-fixscope-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/lib.rs"), "").unwrap();
        let mut ctx = TestCtx::new();
        let mut run = Run::new(dir.clone(), Pattern::builtin(), "test".into());
        let gate = ctx.add_idle();
        let orch = ctx.add_idle();
        ready(&mut ctx, orch);
        run.gate_agent = Some(gate);
        run.orchestrator = Some(orch);
        run.plan = Some(one_task_phase());
        run.stage = Stage::Phase { idx: 0, step: PhaseStep::Gate { round: 1 } };
        gate_says(&mut ctx, &mut run, gate, "GATE: fail — src/lib.rs:12 exports `foo` but main.rs calls `bar`; rename it. Also docs/new-file.md should describe it.");

        assert!(!run.halted());
        assert!(matches!(run.stage, Stage::Phase { step: PhaseStep::Fix { round: 1 }, .. }), "{:?}", run.stage);
        assert!(ctx.stopped.contains(&gate), "the QA agent is done — archived, never prompted again");
        assert_eq!(run.gate_agent, None);
        assert!(!ctx.prompts.iter().any(|(a, _)| *a == gate), "the gate agent is not told to keep going: {:?}", ctx.prompts);
        assert!(!ctx.prompts.iter().any(|(a, _)| *a == orch), "the orchestrator is not involved: {:?}", ctx.prompts);
        let w = run.workers.iter().find(|w| w.task.id == "fix-1").expect("fix-1 was created");
        assert_eq!(w.task.role, "worker-small");
        assert!(w.adhoc, "a fix worker works on the integrated copy");
        assert!(w.prompt.contains("src/lib.rs:12 exports `foo`"), "the findings are its prompt: {}", w.prompt);
        assert_eq!(w.task.scope, vec!["src/lib.rs".to_string(), "docs/new-file.md".to_string()], "scoped to the files the findings name (existing, or a new file with an extension)");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_big_flag_on_the_report_sends_the_fix_to_worker_big() {
        let mut ctx = TestCtx::new();
        let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "test".into());
        let gate = ctx.add_idle();
        run.gate_agent = Some(gate);
        run.plan = Some(one_task_phase());
        run.stage = Stage::Phase { idx: 0, step: PhaseStep::Gate { round: 1 } };
        let (_, ok) = run.handle_tool(&mut ctx, gate, "mantra_gate_report", &json!({"pass": false, "summary": "the auth flow is wired in three places inconsistently", "big": true}));
        assert!(ok);
        run.on_turn_done(&mut ctx, gate, "completed", None, None);
        assert_eq!(run.workers.iter().find(|w| w.task.id == "fix-1").unwrap().task.role, "worker-big");
        // a pattern without those worker names falls back to its closest worker role
        let mut p = Pattern::builtin();
        let small = p.roles.remove("worker-small").unwrap();
        p.roles.insert("tiny-fixer".into(), small);
        let r2 = Run::new(PathBuf::from("."), p, "test".into());
        assert_eq!(r2.fix_role(false), "tiny-fixer", "no small role left: the first worker role it has");
        assert_eq!(r2.fix_role(true), "worker-big");
    }

    /// A pass is verified by the checks; when they still fail the fix goes to a worker too.
    #[test]
    fn checks_failing_after_a_qa_pass_go_to_a_fix_worker() {
        let mut ctx = TestCtx::new();
        let mut run = Run::new(PathBuf::from("."), no_manager(), "test".into());
        let orch = ctx.add_idle();
        ready(&mut ctx, orch);
        let gate = ctx.add_idle();
        run.orchestrator = Some(orch);
        run.gate_agent = Some(gate);
        run.plan = Some(one_task_phase());
        run.stage = Stage::Phase { idx: 0, step: PhaseStep::Gate { round: 1 } };
        let fail = || vec![CheckResult { cmd: "uv venv .venv".into(), ok: false, code: Some(2), output: "error: Permission denied".into(), secs: 0 }];
        run.on_job(&mut ctx, JobTag::Checks { phase: 0, round: 1 }, JobOut::Checks(fail(), vec![], git::ReviewDiff::default()));
        assert!(!run.halted());
        assert!(matches!(run.stage, Stage::Phase { step: PhaseStep::Fix { round: 1 }, .. }), "{:?}", run.stage);
        let w = run.workers.iter().find(|w| w.task.id == "fix-1").expect("a fix worker takes the failing checks");
        assert!(w.prompt.contains("Permission denied") && w.prompt.contains("uv venv .venv"), "{}", w.prompt);
        assert!(!ctx.prompts.iter().any(|(a, _)| *a == gate || *a == orch), "neither the gate nor the orchestrator is asked: {:?}", ctx.prompts);
    }

    /// After the fix worker finishes: checks again, then a FRESH QA agent that sees the previous
    /// verdict, the fix worker's report and the check results.
    #[test]
    fn after_the_fix_the_checks_rerun_and_a_fresh_qa_round_follows() {
        let mut ctx = TestCtx::new();
        let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "test".into());
        let gate = ctx.add_idle();
        run.gate_agent = Some(gate);
        run.plan = Some(one_task_phase());
        run.stage = Stage::Phase { idx: 0, step: PhaseStep::Gate { round: 1 } };
        gate_says(&mut ctx, &mut run, gate, "GATE: fail — README needs a usage section");
        // the fix worker, through the job/turn cycle
        let attempt = run.workers.iter().find(|w| w.task.id == "fix-1").unwrap().attempt;
        run.on_job(&mut ctx, JobTag::WorkerWt { task: "fix-1".into(), attempt }, JobOut::Wt(Ok((PathBuf::from("."), String::new()))));
        let fixer = run.workers.iter().find(|w| w.task.id == "fix-1").and_then(|w| w.agent).unwrap();
        assert!(ctx.spawned("fix-1").iter().any(|s| s.role_name == "worker-small"));
        ctx.agents.get_mut(&fixer).unwrap().final_message = Some("STATUS: done\nSUMMARY: added the usage section".into());
        run.on_turn_done(&mut ctx, fixer, "completed", None, None);
        let second = run.gate_agent.expect("round 2 starts");
        assert_ne!(second, gate);
        assert!(matches!(run.stage, Stage::Phase { step: PhaseStep::Gate { round: 2 }, .. }), "{:?}", run.stage);
        let (_, prompt) = ctx.prompts.iter().rev().find(|(a, _)| *a == second).expect("the fresh agent is briefed");
        assert!(prompt.contains("QA round 2/3"), "{prompt}");
        assert!(prompt.contains("Previous QA round (1) said fail: GATE: fail — README needs a usage section"), "{prompt}");
        assert!(prompt.contains("fix-1: STATUS: done") && prompt.contains("added the usage section"), "the fix worker's report is shown: {prompt}");
        assert!(prompt.contains("Gate checks") && prompt.contains("diff"), "{prompt}");
        assert_eq!(ctx.spawned("qa").len(), 1, "one fresh QA agent for round 2 (the first was seeded by the test)");
    }

    /// The first QA round of a phase (a real spawn) is a fresh read-only agent whose instructions
    /// say it never fixes.
    #[test]
    fn the_phase_gate_agent_is_fresh_read_only_and_told_not_to_fix() {
        let mut ctx = TestCtx::new();
        let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "test".into());
        run.plan = Some(one_task_phase());
        run.stage = Stage::Phase { idx: 0, step: PhaseStep::Checks { round: 0 } };
        run.spawn_gate(&mut ctx, 0);
        let g = run.gate_agent.unwrap();
        let sp = &ctx.spawned("qa")[0];
        assert_eq!(sp.sandbox, "read-only");
        let instr = &ctx.spawned_instructions.iter().find(|(id, _)| *id == g).unwrap().1;
        assert!(instr.contains("never fix") && instr.contains("Your role cannot write files"), "{instr}");
        // a second round replaces it: the first is archived, the second is a different agent
        run.spawn_gate_at_round(&mut ctx, 0, 2);
        let g2 = run.gate_agent.unwrap();
        assert_ne!(g, g2);
        assert!(ctx.stopped.contains(&g));
    }

    #[test]
    fn merge_conflicts_go_to_a_fix_worker_because_the_gate_cannot_merge() {
        let mut ctx = TestCtx::new();
        let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "test".into());
        run.plan = Some(one_task_phase());
        run.stage = Stage::Phase { idx: 0, step: PhaseStep::Checks { round: 0 } };
        run.conflicts = vec!["mantra-w/r/p1-a-1".into()];
        run.on_job(&mut ctx, JobTag::Checks { phase: 0, round: 0 }, JobOut::Checks(vec![], vec![], git::ReviewDiff::default()));
        assert!(run.gate_agent.is_none(), "no QA agent reads a tree that is not merged yet");
        let w = run.workers.iter().find(|w| w.task.id == "fix-0").expect("a worker merges the branches, before QA round 1");
        assert_eq!(w.task.role, "worker-big");
        assert!(w.prompt.contains("git merge") && w.prompt.contains("mantra-w/r/p1-a-1"), "{}", w.prompt);
        assert!(matches!(run.stage, Stage::Phase { step: PhaseStep::Fix { round: 0 }, .. }), "{:?}", run.stage);
        finish_fix(&mut ctx, &mut run, 0);
        assert!(run.conflicts.is_empty(), "the fix worker resolved them");
        let g = run.gate_agent.expect("the QA round follows");
        assert!(matches!(run.stage, Stage::Phase { step: PhaseStep::Gate { round: 1 }, .. }), "the merge fix does not use up a QA round: {:?}", run.stage);
        let (_, p) = ctx.prompts.iter().rev().find(|(a, _)| *a == g).unwrap();
        assert!(p.contains("QA round 1/3") && p.contains("fix-0: STATUS: done"), "the first QA agent sees what the merge fix did: {p}");
    }

    #[test]
    fn a_qa_agent_that_ends_without_a_report_is_asked_twice_then_the_run_halts() {
        let mut ctx = TestCtx::new();
        let mut run = Run::new(PathBuf::from("."), no_manager(), "test".into());
        let planner = ctx.add_idle();
        let gate = ctx.add_idle();
        run.planner = Some(planner);
        run.gate_agent = Some(gate);
        run.plan = Some(one_task_phase());
        run.stage = Stage::Phase { idx: 0, step: PhaseStep::Gate { round: 1 } };
        for n in 1..=2 {
            ctx.prompts.clear();
            run.on_turn_done(&mut ctx, gate, "completed", None, None);
            assert!(!run.halted(), "nudge {n}");
            assert!(ctx.prompts.iter().any(|(a, t)| *a == gate && t.contains("mantra_gate_report")), "{:?}", ctx.prompts);
        }
        run.on_turn_done(&mut ctx, gate, "completed", None, None);
        assert!(run.halted());
        assert_eq!(run.halt.as_ref().unwrap().reason, HaltReason::GateExhausted);
        assert!(run.workers.is_empty(), "nothing to hand a fix worker");
    }

    /// A fix worker that cannot do its job halts the run (the orchestrator cannot drive this
    /// loop); the same task retried after the halt picks the round back up.
    #[test]
    fn a_blocked_fix_worker_halts_and_the_resume_starts_it_over() {
        let mut ctx = TestCtx::new();
        let mut run = Run::new(PathBuf::from("."), no_manager(), "test".into());
        let planner = ctx.add_idle();
        let orch = ctx.add_idle();
        ready(&mut ctx, orch);
        let gate = ctx.add_idle();
        run.planner = Some(planner);
        run.orchestrator = Some(orch);
        run.gate_agent = Some(gate);
        run.plan = Some(one_task_phase());
        run.stage = Stage::Phase { idx: 0, step: PhaseStep::Gate { round: 1 } };
        gate_says(&mut ctx, &mut run, gate, "GATE: fail — src/a.rs is wrong");
        let attempt = run.workers.iter().find(|w| w.task.id == "fix-1").unwrap().attempt;
        run.on_job(&mut ctx, JobTag::WorkerWt { task: "fix-1".into(), attempt }, JobOut::Wt(Ok((PathBuf::from("."), String::new()))));
        let fixer = run.workers[0].agent.unwrap();
        ctx.agents.get_mut(&fixer).unwrap().final_message = Some("STATUS: blocked\nSUMMARY: cannot write src/a.rs".into());
        ctx.prompts.clear();
        run.on_turn_done(&mut ctx, fixer, "completed", None, None);
        assert!(run.halted());
        assert_eq!(run.halt.as_ref().unwrap().reason, HaltReason::AttemptsExhausted);
        assert!(!ctx.prompts.iter().any(|(a, t)| *a == orch && t.contains("FAILED")), "not the orchestrator's: {:?}", ctx.prompts);
        run.resume(&mut ctx);
        assert!(run.workers.iter().filter(|w| w.task.id == "fix-1").count() == 2, "a second attempt of the same fix task");
        assert!(matches!(run.stage, Stage::Phase { step: PhaseStep::Fix { round: 1 }, .. }), "{:?}", run.stage);
    }

    /// The QA report arrives while the run is halted: the fix is remembered (and saved), not started.
    #[test]
    fn a_fail_report_during_a_pause_defers_the_fix_until_the_resume() {
        let mut ctx = TestCtx::new();
        let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "test".into());
        let gate = ctx.add_idle();
        run.gate_agent = Some(gate);
        run.plan = Some(one_task_phase());
        run.stage = Stage::Phase { idx: 0, step: PhaseStep::Gate { round: 1 } };
        run.toggle_pause(&mut ctx);
        assert!(run.halted());
        gate_says(&mut ctx, &mut run, gate, "GATE: fail — fix src/a.rs");
        match &run.pending {
            Some(Pending::Fix { idx: 0, round: 1, findings, big: false }) => assert!(findings.contains("src/a.rs")),
            other => panic!("expected a deferred fix, got {other:?}"),
        }
        assert!(run.workers.is_empty());
        run.toggle_pause(&mut ctx);
        assert!(run.workers.iter().any(|w| w.task.id == "fix-1"), "the resume starts the fix worker");
        assert!(matches!(run.stage, Stage::Phase { step: PhaseStep::Fix { round: 1 }, .. }), "{:?}", run.stage);
        assert!(run.pending.is_none());
    }

    /// The watchdog watches the fix worker like any other worker.
    #[test]
    fn the_watchdog_expects_the_fix_worker_to_be_busy() {
        let mut ctx = TestCtx::new();
        let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "test".into());
        run.plan = Some(one_task_phase());
        run.stage = Stage::Phase { idx: 0, step: PhaseStep::Fix { round: 1 } };
        let a = ctx.add_busy();
        let mut w = mk_worker("fix-1", "worker-small", a, WState::Running);
        w.adhoc = true;
        run.workers.push(w);
        assert!(run.expected_active().iter().any(|(id, e)| *id == a && *e == Expect::Working("fix-1".into())));
    }

    /// A fix worker's out-of-scope edit is logged, not sent to the orchestrator.
    #[test]
    fn a_fix_workers_scope_miss_does_not_wake_the_orchestrator() {
        let mut ctx = TestCtx::new();
        let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "test".into());
        let orch = ctx.add_idle();
        ready(&mut ctx, orch);
        run.orchestrator = Some(orch);
        run.plan = Some(one_task_phase());
        run.stage = Stage::Phase { idx: 0, step: PhaseStep::Fix { round: 1 } };
        let a = ctx.add_busy();
        let mut w = mk_worker("fix-1", "worker-small", a, WState::Running);
        w.adhoc = true;
        w.task.scope = vec!["src/a.rs".into()];
        run.workers.push(w);
        run.on_files_changed(&mut ctx, a, &["src/b.rs".into()]);
        assert!(run.orch_inbox.is_empty() && ctx.prompts.is_empty(), "{:?}", ctx.prompts);
        assert!(run.pulse.iter().any(|p| p.text.contains("tripwire")));
    }

    /// A plan revision sends the phase back to building: a fix worker of the gate that no longer
    /// stands is stopped, not left editing the shared copy.
    #[test]
    fn a_plan_revision_stops_a_running_fix_worker() {
        let mut ctx = TestCtx::new();
        let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "test".into());
        run.plan = Some(one_task_phase());
        run.stage = Stage::Phase { idx: 0, step: PhaseStep::Fix { round: 1 } };
        let orch = ctx.add_idle();
        run.orchestrator = Some(orch);
        let a = ctx.add_busy();
        let mut w = mk_worker("fix-1", "worker-small", a, WState::Running);
        w.adhoc = true;
        run.workers.push(w);
        run.return_to_orchestrating(&mut ctx);
        assert!(ctx.stopped.contains(&a));
        assert_eq!(run.workers[0].state, WState::Cancelled);
        assert_eq!(run.stage, Stage::Phase { idx: 0, step: PhaseStep::Orchestrating });
    }

    #[test]
    fn the_big_flag_is_read_leniently() {
        let mut ctx = TestCtx::new();
        let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "test".into());
        let g = ctx.add_idle();
        for (v, want) in [(json!(true), true), (json!("true"), true), (json!("no"), false), (json!(null), false)] {
            run.handle_tool(&mut ctx, g, "mantra_gate_report", &json!({"pass": false, "summary": "x", "big": v}));
            assert_eq!(run.gate_big, want, "{v}");
        }
        run.handle_tool(&mut ctx, g, "mantra_gate_report", &json!({"pass": false, "summary": "x"}));
        assert!(!run.gate_big);
    }

    #[test]
    fn paths_in_finds_the_files_a_report_names() {
        let dir = std::env::temp_dir().join(format!("mantra-paths-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("src/web")).unwrap();
        std::fs::write(dir.join("src/web/app.js"), "").unwrap();
        std::fs::write(dir.join("Makefile"), "").unwrap();
        let t = "Problems: `src/web/app.js:40:5` calls a removed helper (see b/src/web/app.js too); the e.g. v1.2 and and/or are prose; ../etc/passwd and /etc/hosts are not ours; \
                 a brand new tests/new_test.py should exist; https://example.com/x.html is a link. Makefile has a typo. Also src/web.";
        let got = paths_in(t, &dir);
        assert_eq!(got, vec!["src/web/app.js".to_string(), "tests/new_test.py".to_string(), "src/web".to_string()], "{got:?}");
        assert!(paths_in("nothing to see, v2.0 and and/or", &dir).is_empty());
        let many: String = (0..30).map(|i| format!("src/f{i}.rs ")).collect();
        assert_eq!(paths_in(&many, &dir).len(), 12, "capped");
        std::fs::remove_dir_all(&dir).ok();
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
        let gate = drive_gate_to_halt(&mut ctx, &mut run, gate, blocked);
        assert_eq!(run.halt.as_ref().unwrap().reason, HaltReason::GateExhausted);
        let esc = ctx.prompts.iter().find(|(a, t)| *a == planner && t.contains("[mantra:escalation]")).map(|(_, t)| t.clone()).expect("the planner is handed the halt");
        assert!(esc.contains("mantra_revise_plan") && esc.contains("mantra_resume_run") && esc.contains("mantra_ask_user"), "{esc}");
        assert!(run.halt_hint().contains("planner"));

        // the planner: plan is right, here's a hint — the run resumes with a fresh QA agent that
        // gets the note, and the planner itself is not told to "continue where you left off"
        ctx.prompts.clear();
        let (msg, ok) = run.handle_tool(&mut ctx, planner, "mantra_resume_run", &json!({"note": "the venv is at .venv — do not recreate it"}));
        assert!(ok, "{msg}");
        assert!(!run.halted());
        let fresh = run.gate_agent.expect("no checks in this phase: a fresh QA agent is the gate");
        assert_ne!(fresh, gate, "the stuck agent is replaced, not nudged");
        assert!(ctx.prompts.iter().any(|(a, t)| *a == fresh && t.contains("[mantra:gate]") && t.contains("QA round 1/") && t.contains("[from the planner]") && t.contains(".venv")), "{:?}", ctx.prompts);
        assert!(!ctx.prompts.iter().any(|(a, _)| *a == gate), "the old agent is not woken for a turn it will not finish: {:?}", ctx.prompts);
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
        // QA passed, the checks still fail: fix-1 takes the output…
        run.on_job(&mut ctx, JobTag::Checks { phase: 0, round: 1 }, JobOut::Checks(fail(), vec![], git::ReviewDiff::default()));
        assert!(!run.halted());
        let w = run.workers.iter().find(|w| w.task.id == "fix-1").expect("a fix worker");
        assert!(w.prompt.contains("Permission denied"), "{}", w.prompt);
        // …and when the very same failure is still there afterwards, nobody is fixing it
        run.on_job(&mut ctx, JobTag::FixChecks { phase: 0, round: 1 }, JobOut::Checks(fail(), vec![], git::ReviewDiff::default()));
        assert!(run.halted(), "the identical failure twice means nobody is fixing it");
        assert!(run.halt.as_ref().unwrap().message.contains("identically"));
        assert!(ctx.prompts.iter().any(|(a, t)| *a == planner && t.contains("[mantra:escalation]") && t.contains("uv venv")));
    }

    /// A fix that changes the failure moves on to a fresh QA round that sees the new check output.
    #[test]
    fn a_changed_check_failure_after_a_fix_goes_to_the_next_qa_round() {
        let mut ctx = TestCtx::new();
        let mut run = Run::new(PathBuf::from("."), no_manager(), "test".into());
        let gate = ctx.add_idle();
        run.gate_agent = Some(gate);
        run.plan = Some(one_task_phase());
        run.stage = Stage::Phase { idx: 0, step: PhaseStep::Gate { round: 1 } };
        let fail = |out: &str| vec![CheckResult { cmd: "cargo test".into(), ok: false, code: Some(101), output: out.into(), secs: 0 }];
        run.on_job(&mut ctx, JobTag::Checks { phase: 0, round: 1 }, JobOut::Checks(fail("first failure"), vec![], git::ReviewDiff::default()));
        run.on_job(&mut ctx, JobTag::FixChecks { phase: 0, round: 1 }, JobOut::Checks(fail("a different failure"), vec![], git::ReviewDiff { stat: " src/a.rs | 2 +-".into(), diff: "+new line".into(), truncated: false }));
        assert!(!run.halted());
        let g2 = run.gate_agent.expect("round 2");
        assert!(matches!(run.stage, Stage::Phase { step: PhaseStep::Gate { round: 2 }, .. }), "{:?}", run.stage);
        let (_, p) = ctx.prompts.iter().rev().find(|(a, _)| *a == g2).unwrap();
        assert!(p.contains("a different failure") && p.contains("src/a.rs | 2 +-") && p.contains("+new line"), "the fresh QA agent sees the checks and the diff: {p}");
        // the last round has no fix after it: a failure there halts
        let mut r2 = Run::new(PathBuf::from("."), no_manager(), "test".into());
        r2.plan = Some(one_task_phase());
        r2.stage = Stage::Phase { idx: 0, step: PhaseStep::Gate { round: 3 } };
        r2.on_job(&mut ctx, JobTag::Checks { phase: 0, round: 3 }, JobOut::Checks(fail("still"), vec![], git::ReviewDiff::default()));
        assert!(r2.halted());
        assert_eq!(r2.halt.as_ref().unwrap().reason, HaltReason::GateExhausted);
        assert!(r2.workers.is_empty(), "no fix worker after the last round");
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

    /// A resumed Claude session found unusable (hub/claude.rs's dead-resume detection) surfaces as
    /// a failed turn while the hub relaunches a *fresh* process with no history: the engine must
    /// re-brief the agent as a respawn — never spend a retry, never halt on "agent turn failed".
    #[test]
    fn a_dead_resumed_session_respawns_the_agent_instead_of_failing_it() {
        let mut ctx = TestCtx::new();
        let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "test".into());
        let planner = ctx.add_idle();
        run.planner = Some(planner);
        run.stage = Stage::Planning;

        run.on_turn_done(&mut ctx, planner, "failed", Some("resumed session is unusable - relaunching fresh".into()), None);

        assert!(!run.halted(), "a dead session is never a halt: {:?}", run.halt.as_ref().map(|h| h.message.clone()));
        let new_planner = run.planner.expect("planner still set");
        assert_ne!(new_planner, planner, "a fresh agent is briefed, not the one whose session is gone");
        assert!(ctx.prompts.iter().any(|(a, t)| *a == new_planner && t.contains("[mantra:plan]")), "the planner is re-briefed with its planning prompt: {:?}", ctx.prompts);
        assert!(run.continue_queue.is_empty(), "no [mantra:retry] continuation — the old thread has nothing to continue");
        assert!(!run.retry_counts.contains_key(&planner), "not a retry");
    }

    #[test]
    fn process_stop_cancels_a_previously_scheduled_retry_only_for_that_agent() {
        let mut ctx = TestCtx::new();
        let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "test".into());
        let planner = ctx.add_idle();
        let other = ctx.add_idle();
        run.planner = Some(planner);
        run.stage = Stage::Planning;
        run.continue_queue.push((planner, Instant::now(), "retry stopped planner".into()));
        run.continue_queue.push((other, Instant::now(), "retry other agent".into()));
        run.retry_counts.insert(planner, 2);
        run.agent_stopped_by_user(planner);
        assert!(!run.retry_counts.contains_key(&planner));
        assert_eq!(run.continue_queue.len(), 1);
        assert_eq!(run.continue_queue[0].0, other);
        assert!(run.is_active(), "an individual stop keeps the rest of the run");
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

    /// A worker's `mantra_ask` naming a broken sandbox must halt the run right there — never
    /// bounce up to the orchestrator as an ordinary question ("transient, retry", a respawn, …).
    #[test]
    fn worker_environment_question_halts_instead_of_reaching_the_orchestrator() {
        let mut ctx = TestCtx::new();
        let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "test".into());
        let orch = ctx.add_idle();
        ready(&mut ctx, orch);
        let worker_agent = ctx.add_busy();
        run.orchestrator = Some(orch);
        run.plan = Some(one_task_phase());
        run.stage = Stage::Phase { idx: 0, step: PhaseStep::Orchestrating };
        run.workers.push(mk_worker("t1", "worker-small", worker_agent, WState::Running));

        let (msg, ok) = run.handle_tool(&mut ctx, worker_agent, "mantra_ask", &json!({"question": "bwrap: loopback: Failed RTM_NEWADDR"}));
        assert!(ok, "{msg}");
        assert!(run.halted());
        assert_eq!(run.halt.as_ref().unwrap().reason, HaltReason::Environment);
        assert!(!run.waiting_for_answer(worker_agent), "this is a halt, not an open question");
        assert!(ctx.prompts.is_empty(), "the orchestrator inbox must never see a sandbox failure: {:?}", ctx.prompts);
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
        let gate = drive_gate_to_halt(ctx, &mut run, gate, "GATE: fail — the venv is gone");
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
        let g2 = run.gate_agent.expect("a fresh QA agent takes over the gate");
        assert_ne!(g2, gate);
        assert!(ctx.prompts.iter().any(|(a, t)| *a == g2 && t.contains("[from the manager]") && t.contains(".venv")), "the note reaches the agent that replaces the stuck one: {:?}", ctx.prompts);
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
        run.last_health = Instant::now() - Duration::from_secs(11 * 60);
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
        run.last_health = Instant::now() - Duration::from_secs(11 * 60);
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

    // Effort, titles, thread names, finale prompts ---------------------------------------------

    fn mixed_phase() -> Plan {
        Plan {
            phases: vec![Phase {
                id: "p1".into(),
                name: "p1".into(),
                tasks: vec![Task { id: "s1".into(), role: "worker-small".into(), ..Default::default() }, Task { id: "b1".into(), role: "worker-big".into(), ..Default::default() }],
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    /// Start `task` and let its workspace come back, so its agent is spawned (in `/tmp/wt-<id>`).
    fn launch(ctx: &mut TestCtx, run: &mut Run, task: Task, effort: Option<String>) {
        let id = task.id.clone();
        run.spawn_task(ctx, task, None, effort);
        let wi = run.workers.iter().position(|w| w.task.id == id).unwrap();
        run.launch_worker(ctx, wi, PathBuf::from(format!("/tmp/wt-{id}")), String::new());
    }

    #[test]
    fn auto_effort_is_resolved_per_spawn_from_the_roles_job() {
        let mut ctx = TestCtx::new();
        let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "test".into());
        run.plan = Some(mixed_phase());
        run.stage = Stage::Phase { idx: 0, step: PhaseStep::Orchestrating };
        // the built-in default sets `auto` on every role
        run.spawn_planner(&mut ctx);
        run.spawn_orchestrator(&mut ctx);
        run.ensure_manager(&mut ctx);
        for who in ["planner", "orchestrator", "manager"] {
            assert_eq!(ctx.spawned(who)[0].effort.as_deref(), Some("medium"), "{who}");
        }
        let tasks = mixed_phase().phases[0].tasks.clone();
        launch(&mut ctx, &mut run, tasks[0].clone(), None);
        launch(&mut ctx, &mut run, tasks[1].clone(), None);
        assert_eq!(ctx.spawned("s1")[0].effort.as_deref(), Some("low"), "worker-small");
        assert_eq!(ctx.spawned("b1")[0].effort.as_deref(), Some("medium"), "worker-big");
        assert_eq!(ctx.spawned("s1")[0].cwd, PathBuf::from("/tmp/wt-s1"));
        // an explicit request still wins: the orchestrator's override, a task's own effort
        let mut t = Task { id: "s2".into(), role: "worker-small".into(), ..Default::default() };
        launch(&mut ctx, &mut run, t.clone(), Some("xhigh".into()));
        assert_eq!(ctx.spawned("s2")[0].effort.as_deref(), Some("xhigh"));
        t.id = "s3".into();
        t.effort = Some("high".into());
        launch(&mut ctx, &mut run, t.clone(), None);
        assert_eq!(ctx.spawned("s3")[0].effort.as_deref(), Some("high"));
        t.id = "s4".into();
        t.effort = None;
        launch(&mut ctx, &mut run, t, Some("auto".into()));
        assert_eq!(ctx.spawned("s4")[0].effort.as_deref(), Some("low"), "an orchestrator asking for auto gets the role's auto");
        // the gate is a medium job
        run.spawn_gate(&mut ctx, 0);
        assert_eq!(ctx.spawned("qa")[0].effort.as_deref(), Some("medium"));
    }

    #[test]
    fn a_role_with_a_named_effort_is_left_alone_and_finale_roles_think_hard() {
        let mut ctx = TestCtx::new();
        let mut run = Run::new(PathBuf::from("."), Pattern::builtin_thorough(), "test".into());
        run.plan = Some(mixed_phase());
        run.spawn_planner(&mut ctx);
        assert_eq!(ctx.spawned("planner")[0].effort, None, "the role's own effort applies downstream, untouched");
        // the default's finale: the qa-heavy role is `auto`, which in a finale step is `high`
        let mut ctx = TestCtx::new();
        let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "test".into());
        run.plan = Some(mixed_phase());
        let planner = ctx.add_idle();
        run.planner = Some(planner);
        run.start_finale(&mut ctx, 0);
        assert_eq!(ctx.spawned("qa-heavy")[0].effort.as_deref(), Some("high"));
        // the planner's own verification step reuses the planner agent: its effort is raised
        run.start_finale(&mut ctx, 1);
        assert_eq!(ctx.efforts_set, vec![(planner, "high".to_string())]);
        // while a named planner effort stays what it was
        let mut ctx = TestCtx::new();
        let mut run = Run::new(PathBuf::from("."), Pattern::builtin_thorough(), "test".into());
        run.plan = Some(mixed_phase());
        let planner = ctx.add_idle();
        run.planner = Some(planner);
        run.start_finale(&mut ctx, 2);
        assert!(ctx.efforts_set.is_empty());
        assert_eq!(ctx.spawned("security").len(), 0);
    }

    #[test]
    fn the_run_has_a_title_and_every_thread_carries_it() {
        let mut ctx = TestCtx::new();
        let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "Ich benutze schon seit langer Zeit ein Tool für Notizen".into());
        assert_eq!(run.title, "Ich benutze schon seit langer Zeit", "the goal's first six words");
        let id = run.id.clone();
        assert!(id.ends_with("-ich-benutze-schon-seit-l"), "{id}");
        let planner = run.spawn_planner(&mut ctx);
        assert_eq!(ctx.spawned("planner")[0].thread_name.as_deref(), Some("Ich benutze schon seit langer Zeit · planner"));
        run.spawn_orchestrator(&mut ctx);
        assert_eq!(ctx.spawned("orchestrator")[0].thread_name.as_deref(), Some("Ich benutze schon seit langer Zeit · orchestrator"));

        // the plan comes in: its title names the run, the id stays, live threads are renamed
        let plan = json!({"title": "Notes sync", "summary": "S", "phases": [{"name": "A", "tasks": [
            {"id": "a1", "title": "x", "role": "worker-small", "prompt": "do the first thing well", "scope": ["src/a/**"]}], "gate": {"checks": ["true"]}}]});
        let (msg, ok) = run.handle_tool(&mut ctx, planner, "mantra_submit_plan", &json!({"plan": plan}));
        assert!(ok, "{msg}");
        assert_eq!(run.title, "Notes sync");
        assert_eq!(run.id, id, "the id is the stable key");
        assert!(ctx.renamed.contains(&(planner, "Notes sync · planner".to_string())), "{:?}", ctx.renamed);
        assert_eq!(ctx.renamed.len(), 2, "planner and orchestrator: {:?}", ctx.renamed);
        // agents spawned from here on are named after it
        run.stage = Stage::Phase { idx: 0, step: PhaseStep::Checks { round: 0 } };
        run.spawn_gate(&mut ctx, 0);
        assert_eq!(ctx.spawned("qa")[0].thread_name.as_deref(), Some("Notes sync · qa"));
        // the same title again renames nothing
        let n = ctx.renamed.len();
        let (_, ok) = run.handle_tool(&mut ctx, planner, "mantra_submit_plan", &json!({"plan": plan}));
        assert!(ok);
        assert_eq!(ctx.renamed.len(), n);
        // an empty title leaves the old one
        assert_eq!(title_from_goal("   "), "new run");
        assert_eq!(title_from_goal("\"fix the parser,\" he said."), "fix the parser,\" he said");
        assert_eq!(title_from_goal(&"word ".repeat(40)), "word word word word word word");
    }

    #[test]
    fn the_title_and_gate_notes_survive_a_save_and_a_resume() {
        let dir = std::env::temp_dir().join(format!("mantra-title-{}", std::process::id()));
        let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "build a notes app with sync".into());
        run.dir = dir.clone();
        run.title = "Notes sync".into();
        run.gate_notes.push("phase 1 · QA round 1: pass — fine".into());
        run.save_state();
        let st = crate::engine::state::RunState::load(&dir).unwrap();
        assert_eq!(st.title, "Notes sync");
        assert_eq!(st.gate_notes, vec!["phase 1 · QA round 1: pass — fine".to_string()]);
        let back = Run::from_state(&st, Pattern::builtin(), None, dir.clone());
        assert_eq!(back.title, "Notes sync");
        assert_eq!(back.gate_notes, run.gate_notes);
        // a state file from before titles: the goal's first words stand in
        let mut old = st.clone();
        old.title = String::new();
        assert_eq!(old.display_title(), "build a notes app with sync");
        assert_eq!(Run::from_state(&old, Pattern::builtin(), None, dir.clone()).title, "build a notes app with sync");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_finale_starts_from_the_diff_and_the_reports_not_the_whole_repo() {
        let mut ctx = TestCtx::new();
        let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "make a thing".into());
        run.plan = Some(Plan { title: "T".into(), summary: "a thing".into(), ..Default::default() });
        run.note_gate(0, 1, false, "src/a.rs misses a test");
        run.note_gate(0, 2, true, "all good now");
        run.run_diff = Some(git::ReviewDiff { stat: " src/a.rs | 4 ++--".into(), diff: "+let x = 1;".into(), truncated: false });
        run.start_finale(&mut ctx, 0);
        let qa = run.finale_agent.expect("qa-heavy started");
        let (_, prompt) = ctx.prompts.iter().find(|(a, _)| *a == qa).unwrap();
        assert!(prompt.contains("phase 1 · QA round 1: fail — src/a.rs misses a test") && prompt.contains("phase 1 · QA round 2: pass — all good now"), "{prompt}");
        assert!(prompt.contains("src/a.rs | 4 ++--") && prompt.contains("+let x = 1;"), "{prompt}");
        assert!(prompt.contains("start here, read a file in full only if the diff cannot answer"), "{prompt}");
        let instr = &ctx.spawned_instructions.iter().find(|(id, _)| *id == qa).unwrap().1;
        assert!(!instr.contains("You may edit files"), "the QA pass verifies and has defects fixed by workers: {instr}");
        assert!(instr.contains("run's diff") && instr.contains("spawn a fix worker"), "{instr}");
        // its report joins the notes the next step starts from
        ctx.agents.get_mut(&qa).unwrap().final_message = Some("GATE: pass — nothing real left".into());
        run.on_turn_done(&mut ctx, qa, "completed", None, None);
        assert!(run.gate_notes.iter().any(|n| n.starts_with("finale 1 · qa-heavy: pass — GATE: pass")), "{:?}", run.gate_notes);
        assert!(run.gate_notes.len() == 3);
        // without a captured diff the prompt says where to read it
        let mut r2 = Run::new(PathBuf::from("."), Pattern::builtin(), "make a thing".into());
        r2.start_finale(&mut ctx, 0);
        let q2 = r2.finale_agent.unwrap();
        assert!(ctx.prompts.iter().any(|(a, t)| *a == q2 && t.contains("not captured")));
    }

    #[test]
    fn the_planner_finale_names_what_it_may_do_when_read_only() {
        let mut ctx = TestCtx::new();
        let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "make a thing".into());
        let planner = ctx.add_idle();
        run.planner = Some(planner);
        run.start_finale(&mut ctx, 1);
        assert_eq!(run.finale_agent, Some(planner));
        let (_, p) = ctx.prompts.iter().find(|(a, _)| *a == planner).unwrap();
        assert!(p.contains("mantra_spawn_adhoc"), "{p}");
        assert!(run.pattern.flow.finale[1].task.contains("gate and finale reports"));
    }

    #[test]
    fn checks_are_a_background_job_that_also_brings_the_phase_diff() {
        let dir = std::env::temp_dir().join(format!("mantra-jobs-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let ws = crate::engine::git::Workspace { worktree: false, repo: dir.clone(), integ: dir.clone(), branch: String::new(), base_branch: String::new(), git_common_dir: None, note: None, base_commit: None, initial_commit: false };
        let mut ctx = TestCtx::new();
        let mut run = Run::new(dir.clone(), Pattern::builtin(), "test".into());
        run.ws = Some(ws);
        let mut plan = one_task_phase();
        plan.phases[0].gate.checks = vec!["true".into()];
        run.plan = Some(plan);
        run.stage = Stage::Phase { idx: 0, step: PhaseStep::Merging };
        run.run_checks(&mut ctx, 0, 0);
        run.run_checks(&mut ctx, 0, 1);
        run.run_checks_after_fix(&mut ctx, 0, 1);
        assert_eq!(ctx.jobs, vec!["Checks { phase: 0, round: 0 }", "Checks { phase: 0, round: 1 }", "FixChecks { phase: 0, round: 1 }"]);
        // with no checks defined: the first round still needs the diff (a job); a verify has nothing to do
        run.plan.as_mut().unwrap().phases[0].gate.checks.clear();
        ctx.jobs.clear();
        run.run_checks(&mut ctx, 0, 0);
        assert_eq!(ctx.jobs, vec!["Checks { phase: 0, round: 0 }"]);
        run.stage = Stage::Phase { idx: 0, step: PhaseStep::Gate { round: 1 } };
        run.run_checks(&mut ctx, 0, 1);
        assert_eq!(run.stage, Stage::Phase { idx: 0, step: PhaseStep::Handoff }, "a verify with no checks hands off");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn checks_and_diff_job_runs_the_commands_and_reviews_the_workspace() {
        // the real closure, on a real (git) directory
        let dir = std::env::temp_dir().join(format!("mantra-jobrun-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let sh = |args: &[&str]| assert!(std::process::Command::new("git").arg("-C").arg(&dir).args(args).output().unwrap().status.success());
        sh(&["init", "-q"]);
        std::fs::write(dir.join("a.txt"), "one\n").unwrap();
        sh(&["add", "-A"]);
        sh(&["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "-m", "init"]);
        std::fs::write(dir.join("a.txt"), "one\ntwo\n").unwrap();
        let ws = crate::engine::git::Workspace { worktree: false, repo: dir.clone(), integ: dir.clone(), branch: String::new(), base_branch: String::new(), git_common_dir: None, note: None, base_commit: None, initial_commit: false };
        struct Capture(Vec<JobOut>);
        impl Capture {
            fn run(&mut self, f: Box<dyn FnOnce() -> JobOut + std::marker::Send>) {
                self.0.push(f());
            }
        }
        // drive launch_checks through a ctx that runs the closure
        struct RunJobs(TestCtx, Capture);
        impl Ctx for RunJobs {
            fn spawn(&mut self, r: SpawnReq) -> AgentId { self.0.spawn(r) }
            fn prompt(&mut self, a: AgentId, t: String) { self.0.prompt(a, t) }
            fn prompt_mode(&mut self, a: AgentId, t: String, m: Send) { self.0.prompt_mode(a, t, m) }
            fn interrupt(&mut self, a: AgentId) { self.0.interrupt(a) }
            fn compact(&mut self, a: AgentId) { self.0.compact(a) }
            fn stop(&mut self, a: AgentId, x: bool) { self.0.stop(a, x) }
            fn tool_result(&mut self, a: AgentId, r: Value, t: String, ok: bool) { self.0.tool_result(a, r, t, ok) }
            fn set_effort(&mut self, a: AgentId, e: &str) -> String { self.0.set_effort(a, e) }
            fn agent(&self, a: AgentId) -> Option<&Agent> { self.0.agent(a) }
            fn job(&mut self, _tag: JobTag, f: Box<dyn FnOnce() -> JobOut + std::marker::Send>) { self.1.run(f) }
            fn notify(&mut self, t: &str) { self.0.notify(t) }
        }
        let mut ctx = RunJobs(TestCtx::new(), Capture(vec![]));
        let mut run = Run::new(dir.clone(), Pattern::builtin(), "test".into());
        run.ws = Some(ws);
        let mut plan = one_task_phase();
        plan.phases[0].gate.checks = vec!["test -f a.txt".into(), "exit 3".into()];
        run.plan = Some(plan);
        run.stage = Stage::Phase { idx: 0, step: PhaseStep::Merging };
        run.run_checks(&mut ctx, 0, 0);
        match ctx.1 .0.pop() {
            Some(JobOut::Checks(results, _, diff)) => {
                assert_eq!(results.iter().map(|c| c.ok).collect::<Vec<_>>(), vec![true, false]);
                assert!(diff.stat.contains("a.txt") && diff.diff.contains("+two"), "{diff:?}");
            }
            _ => panic!("expected the checks job's result"),
        }
        std::fs::remove_dir_all(&dir).ok();
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
        run.on_job(&mut ctx, JobTag::Cleanup { phase: 0 }, JobOut::Cleanup(Ok("committed".into()), git::ReviewDiff::default()));
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
        run.on_job(&mut ctx, JobTag::Cleanup { phase: 0 }, JobOut::Cleanup(Ok("committed".into()), git::ReviewDiff::default()));
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
        assert_eq!(run.pending, Some(Pending::Finale { idx: 1, note: None }));
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
        // one that is waiting on the planner's answer is woken by the answer, not kicked
        run.workers.clear();
        run.pending_questions.insert(orch2, "which lexer?".into());
        run.toggle_pause(&mut ctx);
        run.toggle_pause(&mut ctx);
        assert!(!ctx.prompts.iter().any(|(a, t)| *a == orch2 && t.contains("[mantra:resume]")), "{:?}", ctx.prompts);
    }

    /// The stage moved on during the halt (the planner added a task, so the phase went back to
    /// building): the handoff that was due before is stale and must not run on resume.
    #[test]
    fn a_plan_revision_during_the_pause_drops_the_deferred_handoff() {
        let mut ctx = TestCtx::new();
        let mut run = Run::new(PathBuf::from("."), no_manager(), "test".into());
        let planner = ctx.add_idle();
        let orch = ctx.add_idle();
        ready(&mut ctx, orch);
        let gate = ctx.add_busy();
        let w = ctx.add_idle();
        run.planner = Some(planner);
        run.orchestrator = Some(orch);
        run.gate_agent = Some(gate);
        let mut plan = one_task_phase();
        plan.phases[0].tasks[0].prompt = "build the slugify function".into();
        run.plan = Some(plan.clone());
        run.plan_version = 1;
        run.workers.push(mk_worker("t1", "worker-small", w, WState::Done));
        run.stage = Stage::Phase { idx: 0, step: PhaseStep::Gate { round: 1 } };
        let (_, ok) = run.handle_tool(&mut ctx, gate, "mantra_gate_report", &json!({"pass": true, "summary": "coherent"}));
        assert!(ok);
        run.toggle_pause(&mut ctx);
        run.on_turn_done(&mut ctx, gate, "interrupted", None, None);
        assert_eq!(run.pending, Some(Pending::Handoff { idx: 0 }));

        plan.phases[0].tasks.push(Task { id: "t1b".into(), role: "worker-small".into(), prompt: "add the unit tests for slugify".into(), ..Default::default() });
        let (msg, ok) = run.handle_tool(&mut ctx, planner, "mantra_revise_plan", &json!({"plan": serde_json::to_value(&plan).unwrap(), "reason": "one more"}));
        assert!(ok && msg.contains("back to building"), "{msg}");
        assert!(run.halted(), "a user pause is not lifted by a revision");
        assert_eq!(run.stage, Stage::Phase { idx: 0, step: PhaseStep::Orchestrating });
        assert!(run.pending.is_none(), "the handoff is stale: {:?}", run.pending);

        ctx.prompts.clear();
        run.toggle_pause(&mut ctx);
        assert_eq!(run.stage, Stage::Phase { idx: 0, step: PhaseStep::Orchestrating }, "the stale handoff must not run");
        assert!(!ctx.prompts.iter().any(|(_, t)| t.contains("[mantra:handoff]")), "{:?}", ctx.prompts);
        let (msg, ok) = run.handle_tool(&mut ctx, orch, "mantra_spawn", &json!({"task_id": "t1b"}));
        assert!(ok, "{msg}");
    }

    /// A gate respawn during a pause starts nothing (the pause boundary) — and says so, keeping
    /// the note for the fresh agent that starts on resume.
    #[test]
    fn a_gate_respawn_during_the_pause_is_deferred_with_its_note() {
        let mut ctx = TestCtx::new();
        let mut run = Run::new(PathBuf::from("."), no_manager(), "test".into());
        let gate = ctx.add_busy();
        run.gate_agent = Some(gate);
        run.plan = Some(one_task_phase());
        run.stage = Stage::Phase { idx: 0, step: PhaseStep::Gate { round: 2 } };
        run.toggle_pause(&mut ctx);
        let agents_before = ctx.next;
        ctx.prompts.clear();
        let what = run.respawn(&mut ctx, gate, Some("work file by file".into())).unwrap();
        assert_eq!(what, RESPAWN_DEFERRED);
        assert_eq!(ctx.next, agents_before, "no agent may be spawned while paused");
        assert!(ctx.prompts.is_empty(), "{:?}", ctx.prompts);
        assert_eq!(run.pending, Some(Pending::Gate { idx: 0, round: 2, note: Some("work file by file".into()) }));
        assert_eq!(run.gate_agent, None);

        run.toggle_pause(&mut ctx);
        let g2 = run.gate_agent.expect("the fresh gate agent");
        assert_ne!(g2, gate);
        assert_eq!(run.stage, Stage::Phase { idx: 0, step: PhaseStep::Gate { round: 2 } }, "the round is kept");
        assert!(ctx.prompts.iter().any(|(a, t)| *a == g2 && t.starts_with("[mantra:gate]")), "{:?}", ctx.prompts);
        assert!(ctx.prompts.iter().any(|(a, t)| *a == g2 && t == "[mantra:respawn] work file by file"), "{:?}", ctx.prompts);
        assert!(run.pending.is_none());
        // outside a pause it is immediate, as before
        assert_eq!(run.respawn(&mut ctx, g2, None).unwrap(), RESPAWNED);
        assert!(run.gate_agent.is_some_and(|g| g != g2));
    }

    /// A pause that interrupts the gate agent before it reports is not a failed round.
    #[test]
    fn a_gate_round_interrupted_by_the_pause_is_not_a_failed_round() {
        let mut ctx = TestCtx::new();
        let mut run = Run::new(PathBuf::from("."), no_manager(), "test".into());
        let gate = ctx.add_busy();
        run.gate_agent = Some(gate);
        run.plan = Some(one_task_phase());
        run.stage = Stage::Phase { idx: 0, step: PhaseStep::Gate { round: 1 } };
        run.toggle_pause(&mut ctx);
        run.on_turn_done(&mut ctx, gate, "interrupted", None, None);
        assert_eq!(run.stage, Stage::Phase { idx: 0, step: PhaseStep::Gate { round: 1 } }, "the round is not burnt");
        assert!(run.pending.is_none() && run.last_gate_blocker.is_none());
        // a second such pause does not trip the identical-blocker halt either
        run.toggle_pause(&mut ctx);
        run.toggle_pause(&mut ctx);
        run.on_turn_done(&mut ctx, gate, "interrupted", None, None);
        assert_eq!(run.halt.as_ref().map(|h| h.reason), Some(HaltReason::User), "{:?}", run.halt.as_ref().map(|h| &h.message));
        ctx.prompts.clear();
        run.toggle_pause(&mut ctx);
        let fresh = run.gate_agent.expect("the interrupted round is taken over by a fresh QA agent");
        assert_ne!(fresh, gate);
        assert!(ctx.stopped.contains(&gate));
        assert!(ctx.prompts.iter().any(|(a, t)| *a == fresh && t.contains("[mantra:gate]") && t.contains("QA round 1/")), "{:?}", ctx.prompts);
        assert!(!ctx.prompts.iter().any(|(a, _)| *a == gate), "the interrupted agent is not prompted again: {:?}", ctx.prompts);
        assert!(matches!(run.stage, Stage::Phase { step: PhaseStep::Gate { round: 1 }, .. }));
    }

    #[test]
    fn a_finished_run_keeps_its_duration() {
        let mut ctx = TestCtx::new();
        let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "test".into());
        let now = crate::util::unix_secs();
        run.started_unix = now - 129;
        run.set_clock(0, Some(now - 129));
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

    /// A planner spending minutes exploring nothing (an empty project) is exactly the waste this
    /// checks for: `planning_prompt` — used both for a fresh planner and a resumed/respawned one —
    /// must tell it up front there is nothing to explore.
    #[test]
    fn planning_prompt_skips_exploration_for_an_empty_project() {
        let dir = std::env::temp_dir().join(format!("mantra-empty-prompt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        std::fs::create_dir_all(dir.join(".mantra")).unwrap();
        std::fs::write(dir.join(".gitignore"), "").unwrap();

        let run = Run::new(dir.clone(), Pattern::builtin(), "test".into());
        let prompt = run.planning_prompt();
        assert!(prompt.contains("The project directory is empty (greenfield)"), "{prompt}");
        assert!(!prompt.contains("Explore the project"), "{prompt}");

        std::fs::write(dir.join("main.rs"), "fn main() {}").unwrap();
        let prompt = run.planning_prompt();
        assert!(prompt.contains("Explore the project, then call mantra_submit_plan."), "{prompt}");
        assert!(!prompt.contains("greenfield"), "{prompt}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn direct_ignores_a_whitespace_only_message() {
        let mut ctx = TestCtx::new();
        let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "test".into());
        let orch = ctx.add_idle();
        run.orchestrator = Some(orch);

        assert!(!run.direct(&mut ctx, "orchestrator", "   ", Send::Auto), "an empty message is not delivered");
        assert!(ctx.prompts.is_empty(), "nothing must be sent: {:?}", ctx.prompts);
        assert!(run.pulse.is_empty(), "and nothing logged for it");

        assert!(run.direct(&mut ctx, "orchestrator", "status?", Send::Auto), "a real message still goes through");
        assert!(ctx.prompts.iter().any(|(a, t)| *a == orch && t.contains("status?")));
    }

    // mantra_wait really ends the turn ----------------------------------------------------------

    /// After `mantra_wait` the turn is over: the first tool call in it is refused and interrupts the
    /// turn, and any later call in the same turn is refused again without a second interrupt.
    #[test]
    fn a_tool_call_after_mantra_wait_is_refused_and_interrupts_the_turn_once() {
        let mut ctx = TestCtx::new();
        let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "test".into());
        let orch = ctx.add_idle();
        ready(&mut ctx, orch);
        run.orchestrator = Some(orch);

        let (text, ok) = run.handle_tool(&mut ctx, orch, "mantra_wait", &json!({}));
        assert!(ok && text.starts_with("OK"), "{text}");

        let (text, ok) = run.handle_tool(&mut ctx, orch, "mantra_status", &json!({}));
        assert!(ok && text.starts_with("Your turn already ended when you called mantra_wait"), "{text}");
        assert_eq!(ctx.interrupted, vec![orch], "the turn is interrupted at the first refused call");

        let (text, _) = run.handle_tool(&mut ctx, orch, "mantra_wait", &json!({}));
        assert!(text.starts_with("Your turn already ended"), "{text}");
        let (text, _) = run.handle_tool(&mut ctx, orch, "mantra_status", &json!({}));
        assert!(text.starts_with("Your turn already ended"), "{text}");
        assert_eq!(ctx.interrupted, vec![orch], "one interrupt per turn, however many calls follow");
    }

    /// A yielded turn that ends "interrupted" (by the refusal above) is a finished turn: nothing is
    /// logged as interrupted, it counts as completed (so the manager's watchdog case closes), and the
    /// yield is cleared.
    #[test]
    fn a_yielded_turn_that_ends_interrupted_counts_as_completed() {
        let mut ctx = TestCtx::new();
        let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "test".into());
        let manager = ctx.add_idle();
        ready(&mut ctx, manager);
        run.manager = Some(manager);
        run.manager_watchdog = Some(ManagerCase { deadline: Instant::now(), about: "t1".into(), respawned: false });

        run.handle_tool(&mut ctx, manager, "mantra_wait", &json!({}));
        run.handle_tool(&mut ctx, manager, "mantra_status", &json!({})); // refused: interrupts the turn
        run.on_turn_done(&mut ctx, manager, "interrupted", None, None);

        assert!(!run.pulse.iter().any(|p| p.text.contains("interrupted")), "{:?}", run.pulse.iter().map(|p| p.text.clone()).collect::<Vec<_>>());
        assert!(!run.yielded.contains(&manager), "the yield ends with the turn");
        assert!(run.manager_watchdog.is_none(), "the turn counts as completed, so the manager's watchdog case closes");
    }

    /// Two `mantra_status` calls in one turn with nothing changed between them: the second says so and
    /// asks for `mantra_wait`. A new turn starts from a fresh look, and a change shows up in full.
    #[test]
    fn a_repeated_status_with_nothing_new_says_so_and_yields() {
        let mut ctx = TestCtx::new();
        let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "test".into());
        let orch = ctx.add_idle();
        ready(&mut ctx, orch);
        run.orchestrator = Some(orch);

        let (first, _) = run.handle_tool(&mut ctx, orch, "mantra_status", &json!({}));
        assert!(!first.contains("No change"), "{first}");
        let (second, _) = run.handle_tool(&mut ctx, orch, "mantra_status", &json!({}));
        assert!(second.starts_with("No change since your last mantra_status. Call mantra_wait now"), "{second}");
        assert!(run.yielded.contains(&orch), "a further call in this turn is refused");

        run.on_turn_done(&mut ctx, orch, "completed", None, None);
        let worker_agent = ctx.add_busy();
        run.workers.push(mk_worker("t1", "worker-small", worker_agent, WState::Running));
        let (third, _) = run.handle_tool(&mut ctx, orch, "mantra_status", &json!({}));
        assert!(third.contains("t1") && !third.contains("No change"), "{third}");
    }

    // First-token deadline ---------------------------------------------------------------------

    /// A fresh scratch run directory (journal, state and pattern files land here, not in `~/.mantra`).
    fn scratch_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("mantra-run-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    /// Make `a` look like it has an active turn that has produced nothing for `secs`.
    fn go_silent(ctx: &mut TestCtx, a: AgentId, secs: u64) {
        let ag = ctx.agents.get_mut(&a).unwrap();
        ag.turn_active = true;
        ag.status = Status::Busy;
        ag.thread_id = Some(format!("thread-{a}"));
        ag.turn_wait_since = Some(Instant::now() - Duration::from_secs(secs));
    }

    #[test]
    fn first_token_deadline_restarts_once_then_falls_back_to_the_roles_model() {
        let dir = scratch_dir("fallback");
        let mut ctx = TestCtx::new();
        let mut pat = no_manager();
        pat.roles.get_mut("planner").unwrap().fallback = Some("luna".into());
        let primary = pat.roles["planner"].model.clone();
        let mut run = Run::new(PathBuf::from("."), pat, "test".into());
        run.dir = dir.clone();
        let planner = ctx.add_idle();
        run.planner = Some(planner);
        run.stage = Stage::Planning;

        // under the 4-minute default: nothing happens, however silent it is
        go_silent(&mut ctx, planner, 3 * 60);
        run.watchdog_tick(&mut ctx);
        assert_eq!(run.planner, Some(planner), "3 minutes is inside the deadline");
        assert!(ctx.prompts.is_empty(), "{:?}", ctx.prompts);

        // first strike: the turn is restarted on a fresh process, same model
        go_silent(&mut ctx, planner, 4 * 60 + 5);
        run.watchdog_tick(&mut ctx);
        let second = run.planner.unwrap();
        assert_ne!(second, planner, "the silent planner is replaced");
        assert_eq!(ctx.spawned_models.last().unwrap().1, primary, "the first restart stays on the same model");
        assert!(ctx.prompts.iter().any(|(a, t)| *a == second && t.contains("produced no output") && t.contains("restarted")), "{:?}", ctx.prompts);
        assert!(run.pulse.iter().any(|p| p.text.contains("planner: no output for") && p.text.contains("restarting")), "{:?}", run.pulse.iter().map(|p| p.text.clone()).collect::<Vec<_>>());
        assert!(!run.halted());

        // the very next tick does not strike again: the new agent has no silent spell yet
        run.watchdog_tick(&mut ctx);
        assert_eq!(run.planner, Some(second));

        // second strike on the same slot: the role's fallback takes over, and the log says so
        go_silent(&mut ctx, second, 4 * 60 + 5);
        run.watchdog_tick(&mut ctx);
        let third = run.planner.unwrap();
        assert_ne!(third, second);
        assert_eq!(ctx.spawned_models.last().unwrap().1, "luna", "respawned on the fallback alias");
        assert!(run.pulse.iter().any(|p| p.glyph == "↻" && p.text == "planner: no output twice — falling back to luna"), "{:?}", run.pulse.iter().map(|p| p.text.clone()).collect::<Vec<_>>());
        assert_eq!(run.pattern.roles["planner"].model, "luna", "later spawns of the role stay on it");
        let saved = std::fs::read_to_string(dir.join("pattern.toml")).unwrap();
        assert!(saved.contains("model = \"luna\""), "a resume must see the switch: {saved}");
        assert!(!run.halted());

        // the fallback gets its own restart; if it is silent too there is nobody left to fall back to
        go_silent(&mut ctx, third, 4 * 60 + 5);
        run.watchdog_tick(&mut ctx);
        let fourth = run.planner.unwrap();
        assert_ne!(fourth, third, "the new model is restarted once before anything else");
        assert!(!run.halted());
        go_silent(&mut ctx, fourth, 4 * 60 + 5);
        run.watchdog_tick(&mut ctx);
        assert!(run.halted(), "no fallback left and no manager: the run halts for the user");
        assert_eq!(run.halt.as_ref().unwrap().reason, HaltReason::AgentTurnFailed);
        assert!(run.halt.as_ref().unwrap().message.contains("twice in a row"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn first_token_deadline_without_a_fallback_escalates_to_the_manager() {
        let mut ctx = TestCtx::new();
        let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "test".into());
        assert!(run.pattern.roles["planner"].fallback.is_none());
        let planner = ctx.add_idle();
        run.planner = Some(planner);
        run.stage = Stage::Planning;

        go_silent(&mut ctx, planner, 5 * 60);
        run.watchdog_tick(&mut ctx);
        let second = run.planner.unwrap();
        assert_ne!(second, planner, "first strike restarts");
        assert!(run.manager.is_none() && !run.halted());

        ctx.prompts.clear();
        go_silent(&mut ctx, second, 5 * 60);
        run.watchdog_tick(&mut ctx);
        assert_eq!(run.planner, Some(second), "no fallback: the silent agent is left for the manager to deal with");
        let manager = run.manager.expect("the manager is woken");
        assert!(ctx.prompts.iter().any(|(a, t)| *a == manager && t.contains("[mantra:watchdog]") && t.contains("planner") && t.contains("no output")), "{:?}", ctx.prompts);
        assert!(run.manager_watchdog.is_some(), "a case is open, with the usual deadline");
        assert!(!run.halted(), "an escalation, not a halt");

        // one silent spell is one strike: the next ticks add nothing
        let n = ctx.prompts.len();
        run.watchdog_tick(&mut ctx);
        run.watchdog_tick(&mut ctx);
        assert_eq!(ctx.prompts.len(), n, "{:?}", ctx.prompts);
    }

    #[test]
    fn first_token_deadline_is_only_for_active_silent_turns() {
        let mut ctx = TestCtx::new();
        let mut run = Run::new(PathBuf::from("."), no_manager(), "test".into());
        let planner = ctx.add_idle();
        run.planner = Some(planner);
        run.stage = Stage::Planning;

        // already produced something (the wait was cleared by the first output): never a strike
        go_silent(&mut ctx, planner, 30 * 60);
        ctx.agents.get_mut(&planner).unwrap().turn_wait_since = None;
        run.watchdog_tick(&mut ctx);
        assert_eq!(run.planner, Some(planner), "a turn that has spoken is not silent, however long it runs");

        // waiting on an approval is expected silence
        go_silent(&mut ctx, planner, 30 * 60);
        ctx.agents.get_mut(&planner).unwrap().status = Status::Waiting;
        run.watchdog_tick(&mut ctx);
        assert_eq!(run.planner, Some(planner));

        // no active turn: the idle ladder owns it, not this deadline
        go_silent(&mut ctx, planner, 30 * 60);
        {
            let ag = ctx.agents.get_mut(&planner).unwrap();
            ag.turn_active = false;
            ag.awaiting_start = false;
        }
        run.watchdog_tick(&mut ctx);
        assert_eq!(run.planner, Some(planner));

        // switched off
        run.pattern.settings.first_token_minutes = 0;
        go_silent(&mut ctx, planner, 30 * 60);
        run.watchdog_tick(&mut ctx);
        assert_eq!(run.planner, Some(planner), "first_token_minutes = 0 turns the deadline off");
        assert!(!run.halted());
    }

    #[test]
    fn a_completed_turn_clears_the_first_token_strikes_and_workers_are_keyed_by_task() {
        let mut ctx = TestCtx::new();
        let mut run = Run::new(PathBuf::from("."), no_manager(), "test".into());
        let planner = ctx.add_idle();
        run.planner = Some(planner);
        run.stage = Stage::Planning;
        go_silent(&mut ctx, planner, 5 * 60);
        run.watchdog_tick(&mut ctx);
        assert_eq!(run.first_token_strikes.get("planner"), Some(&1));
        let second = run.planner.unwrap();
        run.on_turn_done(&mut ctx, second, "completed", None, None);
        assert!(run.first_token_strikes.is_empty(), "a turn that finished means the gateway answered");

        let w = ctx.add_busy();
        run.workers.push(mk_worker("t1", "worker-small", w, WState::Running));
        assert_eq!(run.first_token_key(w).as_deref(), Some("worker:t1"), "a worker's slot is its task, which survives the respawn");
    }

    // Quiet manager digests --------------------------------------------------------------------

    #[test]
    fn health_digest_is_skipped_when_nothing_changed_since_the_last_one() {
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
        let digests = |ctx: &TestCtx| ctx.prompts.iter().filter(|(a, t)| *a == manager && t.contains("[mantra:health]")).count();
        let rearm = |run: &mut Run| {
            run.last_health = Instant::now() - Duration::from_secs((run.pattern.settings.manager_minutes + 1) * 60);
            run.last_tick = Instant::now() - Duration::from_secs(2);
        };

        // the first digest has nothing to compare with
        rearm(&mut run);
        run.tick(&mut ctx);
        assert_eq!(digests(&ctx), 1);

        // nothing happened: the interval elapses, no digest, no journal line, no manager turn
        ctx.prompts.clear();
        let seq = run.pulse_seq;
        for _ in 0..3 {
            rearm(&mut run);
            run.tick(&mut ctx);
        }
        assert_eq!(digests(&ctx), 0, "an unchanged run costs the manager nothing: {:?}", ctx.prompts);
        assert_eq!(run.pulse_seq, seq, "and logs nothing about the skipped digest");

        // a journal entry
        run.log("✓", "green", "something happened");
        rearm(&mut run);
        run.tick(&mut ctx);
        assert_eq!(digests(&ctx), 1, "a journal entry earns a digest");

        // a worker changing state
        ctx.prompts.clear();
        rearm(&mut run);
        run.tick(&mut ctx);
        assert_eq!(digests(&ctx), 0);
        run.workers[0].state = WState::Done;
        rearm(&mut run);
        run.tick(&mut ctx);
        assert_eq!(digests(&ctx), 1, "a worker state change earns a digest");

        // an agent changing state
        ctx.prompts.clear();
        rearm(&mut run);
        run.tick(&mut ctx);
        assert_eq!(digests(&ctx), 0);
        ctx.agents.get_mut(&orch).unwrap().turn_active = false;
        ctx.agents.get_mut(&orch).unwrap().status = Status::Idle;
        rearm(&mut run);
        run.tick(&mut ctx);
        assert_eq!(digests(&ctx), 1, "an agent state change earns a digest");

        // the manager's own comings and goings are not news
        ctx.prompts.clear();
        rearm(&mut run);
        run.tick(&mut ctx);
        assert_eq!(digests(&ctx), 0);
        ctx.agents.get_mut(&manager).unwrap().status = Status::Busy;
        rearm(&mut run);
        run.tick(&mut ctx);
        assert_eq!(digests(&ctx), 0, "{:?}", ctx.prompts);
    }

    // Full error text --------------------------------------------------------------------------

    #[test]
    fn halt_keeps_the_full_error_in_the_journal_and_state_but_the_short_form_on_the_pulse() {
        let dir = scratch_dir("fullerr");
        let mut ctx = TestCtx::new();
        let mut run = Run::new(PathBuf::from("."), no_manager(), "test".into());
        run.dir = dir.clone();
        let gate = ctx.add_busy();
        run.gate_agent = Some(gate);
        let err = format!("harness said: {}END", "the model produced a reply the harness cannot parse; ".repeat(12));
        assert!(err.chars().count() > 400);

        // the first failure of a gate gets its free respawn; the second halts
        run.on_turn_done(&mut ctx, gate, "failed", Some(err.clone()), Some(ErrKind::Other));
        assert!(!run.halted());
        let gate2 = run.gate_agent.unwrap();
        run.on_turn_done(&mut ctx, gate2, "failed", Some(err.clone()), Some(ErrKind::Other));
        let h = run.halt.as_ref().expect("halted");
        assert!(h.full.contains(&err), "the halt record keeps the whole error");
        assert!(h.message.ends_with('…') && h.message.chars().count() < err.chars().count(), "the band and the pulse keep the short form: {}", h.message);

        let pulse = run.pulse.iter().find(|p| p.glyph == "⛔").expect("a halted line").text.clone();
        assert!(pulse.ends_with('…') && !pulse.contains("END"), "pulse: {pulse}");
        let journal = std::fs::read_to_string(dir.join("journal.jsonl")).unwrap();
        assert!(journal.lines().any(|l| l.contains("halted (full)") && l.contains(&err)), "the journal has the whole text");
        let saved = RunState::load(&dir).unwrap();
        assert_eq!(saved.halted.as_deref(), Some(h.full.as_str()), "state.json carries the full message");
        assert!(run.overview_text(&ctx).contains(&err), "and so does what the manager reads");
        assert!(ctx.notified.iter().all(|n| !n.contains("END")), "the desktop notification stays short");

        // the sandbox halt keeps the whole command error too
        let dir2 = scratch_dir("fullenv");
        let mut env = Run::new(PathBuf::from("."), no_manager(), "test".into());
        env.dir = dir2.clone();
        let w = ctx.add_busy();
        env.on_environment_broken(&mut ctx, w, format!("bwrap: {}TAIL", "no permissions to create a new namespace; ".repeat(10)));
        let eh = env.halt.as_ref().unwrap();
        assert!(eh.full.contains("TAIL") && !eh.message.contains("TAIL"), "{} / {}", eh.message, eh.full);
        assert!(std::fs::read_to_string(dir2.join("journal.jsonl")).unwrap().contains("TAIL"));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&dir2);
    }

    #[test]
    fn a_short_halt_message_is_not_journalled_twice() {
        let dir = scratch_dir("shorterr");
        let mut ctx = TestCtx::new();
        let mut run = Run::new(PathBuf::from("."), no_manager(), "test".into());
        run.dir = dir.clone();
        run.halt(&mut ctx, HaltReason::GateExhausted, None, "phase 1 gate not passed".into());
        let journal = std::fs::read_to_string(dir.join("journal.jsonl")).unwrap();
        assert_eq!(journal.lines().filter(|l| l.contains("phase 1 gate not passed")).count(), 1, "{journal}");
        assert_eq!(run.halt.as_ref().unwrap().full, "phase 1 gate not passed");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // Persistence ------------------------------------------------------------------------------

    #[test]
    fn state_is_saved_on_a_user_pause_a_dropped_pending_and_by_the_tick_after_any_journal_line() {
        let dir = scratch_dir("persist");
        let mut ctx = TestCtx::new();
        let mut run = Run::new(PathBuf::from("."), no_manager(), "test".into());
        run.dir = dir.clone();

        // a user pause is a halt like any other: a crash right after it must reload as paused
        run.toggle_pause(&mut ctx);
        assert_eq!(RunState::load(&dir).unwrap().halted.as_deref(), Some("paused by you"));
        run.toggle_pause(&mut ctx);
        assert_eq!(RunState::load(&dir).unwrap().halted, None);

        // a transition the pause boundary dropped must not come back on the next resume
        run.pending = Some(Pending::Phase { idx: 1 });
        run.save_state();
        assert!(RunState::load(&dir).unwrap().pending.is_some());
        run.drop_pending("the plan changed");
        assert!(RunState::load(&dir).unwrap().pending.is_none());

        // everything else that journals is flushed by the next tick (once, not per line)
        run.stage = Stage::Planning;
        run.log("☰", "saffron", "plan v1");
        assert_eq!(RunState::load(&dir).unwrap().stage, Stage::Setup, "not written per line");
        run.last_tick = Instant::now() - Duration::from_secs(2);
        run.tick(&mut ctx);
        assert_eq!(RunState::load(&dir).unwrap().stage, Stage::Planning, "the tick flushed it");
        assert!(!run.state_dirty);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn attempts_exhausted_halt_keeps_the_workers_whole_last_failure() {
        let mut ctx = TestCtx::new();
        let mut run = Run::new(PathBuf::from("."), no_manager(), "test".into());
        run.dir = scratch_dir("attempts");
        let w = ctx.add_idle();
        let err = format!("upstream said: {}END", "connection reset by the gateway; ".repeat(15));
        let mut worker = mk_worker("t1", "worker-small", w, WState::Failed(err.clone()));
        worker.attempt = run.max_attempts();
        run.workers.push(worker);
        let task = Task { id: "t1".into(), role: "worker-small".into(), ..Default::default() };
        let out = run.spawn_task(&mut ctx, task, None, None);
        assert!(out.starts_with("REFUSED"), "{out}");
        let h = run.halt.as_ref().expect("halted");
        assert_eq!(h.reason, HaltReason::AttemptsExhausted);
        assert!(h.full.contains(&err) && h.full.starts_with("t1 has used all "), "{}", h.full);
        assert!(!h.message.contains("END") && h.message.starts_with("t1 has used all "), "the short form keeps the task-id prefix the web view parses: {}", h.message);
        assert_eq!(RunState::load(&run.dir).unwrap().halted.as_deref(), Some(h.full.as_str()));
        let _ = std::fs::remove_dir_all(&run.dir);
    }

    fn pulse_texts(run: &Run) -> Vec<String> {
        run.pulse.iter().map(|p| p.text.clone()).collect()
    }

    #[test]
    fn a_gateway_outage_retries_a_worker_without_costing_an_attempt() {
        let mut ctx = TestCtx::new();
        let mut run = Run::new(PathBuf::from("."), no_manager(), "test".into());
        let w = ctx.add_idle();
        run.workers.push(mk_worker("t1", "worker-small", w, WState::Running));
        for _ in 0..(run.pattern.settings.worker_retries + 5) {
            run.on_turn_done(&mut ctx, w, "failed", Some("unexpected status 502 Bad Gateway: upstream provider returned an error".into()), Some(ErrKind::Other));
            assert!(matches!(run.workers[0].state, WState::Retrying(_)), "{:?}", run.workers[0].state);
        }
        assert!(!run.halted());
        assert_eq!(run.workers[0].attempt, 1);
        assert!(!run.retry_counts.contains_key(&w), "the counted ladder was never touched");
        assert_eq!(run.continue_queue.len() as u32, run.pattern.settings.worker_retries + 5);
        assert!(run.continue_queue.iter().all(|(a, _, m)| *a == w && m.contains("not your fault")));
    }

    #[test]
    fn workers_failed_by_a_dead_gateway_respawn_on_resume() {
        let mut ctx = TestCtx::new();
        let mut run = Run::new(PathBuf::from("."), no_manager(), "test".into());
        let orch = ctx.add_idle();
        let w = ctx.add_idle();
        run.orchestrator = Some(orch);
        run.workers.push(mk_worker("t1", "worker-small", w, WState::Running));
        run.workers[0].prompt = "build the thing".into();
        run.halt(&mut ctx, HaltReason::AgentTurnFailed, Some(orch), "orchestrator: HTTP 503".into());
        // the worker's own turn dies on the gateway while the run is halted
        run.on_turn_done(&mut ctx, w, "failed", Some("503 Service Unavailable".into()), Some(ErrKind::Transient));
        assert!(matches!(run.workers[0].state, WState::Failed(_)) && run.workers[0].transient);
        assert!(run.continue_queue.is_empty(), "nothing retries into a dead gateway while halted");
        // a restart keeps the flag on disk
        run.dir = scratch_dir("outage");
        run.save_state();
        let saved = RunState::load(&run.dir).unwrap();
        assert!(saved.workers[0].transient && saved.workers[0].state == "failed");
        let _ = std::fs::remove_dir_all(&run.dir);

        run.toggle_pause(&mut ctx);
        assert!(!run.halted());
        assert_eq!(run.workers[0].state, WState::Running);
        assert!(!run.workers[0].transient);
        assert_eq!(run.workers[0].agent, Some(w), "the same agent, in the same worktree, carries on");
        assert_eq!(run.workers[0].attempt, 1);
        let prompts: Vec<&String> = ctx.prompts.iter().filter(|(a, _)| *a == w).map(|(_, t)| t).collect();
        assert_eq!(prompts.len(), 1, "{prompts:?}");
        assert!(prompts[0].starts_with("[mantra:resume]") && prompts[0].contains("Continue exactly where you left off") && prompts[0].contains("build the thing"));
        assert!(pulse_texts(&run).iter().any(|t| t == "respawning t1 after the gateway outage"), "{:?}", pulse_texts(&run));
    }

    #[test]
    fn retrying_a_gateway_failed_worker_does_not_spend_an_attempt() {
        let mut ctx = TestCtx::new();
        let mut run = Run::new(PathBuf::from("."), no_manager(), "test".into());
        let w = ctx.add_idle();
        let before = run.max_attempts();
        run.workers.push(mk_worker("t1", "worker-small", w, WState::Failed("502".into())));
        run.workers[0].transient = true;
        run.retry_worker(&mut ctx, w, None);
        assert_eq!(run.max_attempts(), before + 1);
    }

    #[test]
    fn a_rejected_effort_is_remapped_and_the_turn_retried_once() {
        let mut ctx = TestCtx::new();
        let mut run = Run::new(PathBuf::from("."), no_manager(), "test".into());
        let w = ctx.add_idle();
        run.workers.push(mk_worker("t1", "worker-small", w, WState::Running));
        {
            let ag = ctx.agents.get_mut(&w).unwrap();
            ag.effort = "medium".into();
            ag.model_alias = "deepseek".into();
        }
        let err = "unexpected status 400: DeepSeek V4.1 reasoning_effort must be low, high, xhigh, max";
        run.on_turn_done(&mut ctx, w, "failed", Some(err.into()), Some(ErrKind::ProviderRejected));
        assert_eq!(ctx.efforts_set, vec![(w, "low".to_string())], "medium sits between low and high: the lower one wins the tie");
        assert!(!run.halted());
        assert_eq!(run.continue_queue.len(), 1);
        assert!(pulse_texts(&run).iter().any(|t| t == "a: effort medium is not offered by deepseek — using low" || t.ends_with("effort medium is not offered by deepseek — using low")), "{:?}", pulse_texts(&run));
        // a model that rejects the new one too is the ordinary 400 again
        ctx.agents.get_mut(&w).unwrap().effort = "low".into();
        run.on_turn_done(&mut ctx, w, "failed", Some(err.into()), Some(ErrKind::ProviderRejected));
        assert_eq!(ctx.efforts_set.len(), 1);
        assert!(run.halted() && run.halt.as_ref().unwrap().reason == HaltReason::ProviderRejected);
    }

    #[test]
    fn an_effort_error_that_lists_nothing_falls_through() {
        let mut ctx = TestCtx::new();
        let mut run = Run::new(PathBuf::from("."), no_manager(), "test".into());
        let w = ctx.add_idle();
        run.workers.push(mk_worker("t1", "worker-small", w, WState::Running));
        run.on_turn_done(&mut ctx, w, "failed", Some("unexpected status 400: invalid reasoning_effort".into()), Some(ErrKind::ProviderRejected));
        assert!(ctx.efforts_set.is_empty());
        assert!(run.halted() && run.halt.as_ref().unwrap().reason == HaltReason::ProviderRejected);
    }

    #[test]
    fn the_halt_for_a_dead_gateway_says_what_to_do() {
        let mut ctx = TestCtx::new();
        let mut run = Run::new(PathBuf::from("."), no_manager(), "test".into());
        let planner = ctx.add_idle();
        let w = ctx.add_busy();
        run.planner = Some(planner);
        run.stage = Stage::Planning;
        run.turn_fail_retried.insert("planner".into());
        run.retry_counts.insert(planner, 99);
        run.workers.push(mk_worker("t1", "worker-small", w, WState::Running));
        // a busy agent that has not failed: that is not "the gateway is down"
        run.on_turn_done(&mut ctx, planner, "failed", Some("502 Bad Gateway".into()), Some(ErrKind::Transient));
        assert!(run.halted());
        assert!(!run.halt.as_ref().unwrap().message.contains("gateway looks down"), "{}", run.halt.as_ref().unwrap().message);

        let mut run = Run::new(PathBuf::from("."), no_manager(), "test".into());
        run.planner = Some(planner);
        run.stage = Stage::Planning;
        run.turn_fail_retried.insert("planner".into());
        run.retry_counts.insert(planner, 99);
        run.workers.push(mk_worker("t1", "worker-small", w, WState::Running));
        run.transport_errs.insert(w, Instant::now());
        run.on_turn_done(&mut ctx, planner, "failed", Some("502 Bad Gateway".into()), Some(ErrKind::Transient));
        let h = run.halt.as_ref().expect("halted");
        assert!(h.message.ends_with("the model gateway looks down — the run resumes where it was once you press resume (space / Resume); failed workers are respawned automatically"), "{}", h.message);
        assert_eq!(run.halt_hint(), "space resumes once the gateway is back — the workers the outage took are respawned for free", "the band offers no key a halted run refuses");
        assert!(h.full.ends_with("failed workers are respawned automatically"));
    }

    /// The dead-gateway halt of a fixed agent is not handed to the manager either: it would
    /// fail its turn on the same gateway. A planner failure that is not the gateway still is.
    #[test]
    fn a_dead_gateway_halt_does_not_wake_the_manager() {
        for (outage, err) in [(true, "502 Bad Gateway"), (false, "the model returned an empty response")] {
            let mut ctx = TestCtx::new();
            let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "test".into());
            run.dir = scratch_dir("gateway-manager");
            assert!(run.has_manager());
            let planner = ctx.add_idle();
            let w = ctx.add_busy();
            run.planner = Some(planner);
            run.stage = Stage::Planning;
            run.turn_fail_retried.insert("planner".into());
            run.retry_counts.insert(planner, 99);
            run.workers.push(mk_worker("t1", "worker-small", w, WState::Running));
            run.transport_errs.insert(w, Instant::now());
            run.on_turn_done(&mut ctx, planner, "failed", Some(err.into()), if outage { Some(ErrKind::Transient) } else { None });
            let h = run.halt.as_ref().expect("halted");
            assert_eq!(h.reason, HaltReason::AgentTurnFailed);
            assert_eq!(h.message.ends_with(GATEWAY_DOWN_HINT), outage, "{}", h.message);
            assert_eq!(ctx.spawns.iter().any(|s| s.name == "manager"), !outage, "{err}: the manager is asked only when it is not the gateway");
            let _ = std::fs::remove_dir_all(&run.dir);
        }
    }

    // A gateway outage under a fix worker ---------------------------------------------------------

    /// A `TestCtx` whose jobs run: the closure's result is kept with its tag, for the test to
    /// hand back through `on_job` — the way the app's job runner would.
    struct JobCtx(TestCtx, Vec<(JobTag, JobOut)>);
    impl Ctx for JobCtx {
        fn spawn(&mut self, r: SpawnReq) -> AgentId {
            self.0.spawn(r)
        }
        fn prompt(&mut self, a: AgentId, t: String) {
            self.0.prompt(a, t)
        }
        fn prompt_mode(&mut self, a: AgentId, t: String, m: Send) {
            self.0.prompt_mode(a, t, m)
        }
        fn interrupt(&mut self, a: AgentId) {
            self.0.interrupt(a)
        }
        fn compact(&mut self, a: AgentId) {
            self.0.compact(a)
        }
        fn stop(&mut self, a: AgentId, x: bool) {
            self.0.stop(a, x)
        }
        fn tool_result(&mut self, a: AgentId, r: Value, t: String, ok: bool) {
            self.0.tool_result(a, r, t, ok)
        }
        fn set_effort(&mut self, a: AgentId, e: &str) -> String {
            self.0.set_effort(a, e)
        }
        fn agent(&self, a: AgentId) -> Option<&Agent> {
            self.0.agent(a)
        }
        fn job(&mut self, tag: JobTag, f: Box<dyn FnOnce() -> JobOut + std::marker::Send>) {
            self.1.push((tag, f()));
        }
        fn notify(&mut self, t: &str) {
            self.0.notify(t)
        }
    }

    /// Phase 1 at gate round 1's fix: a planner that is idle, a fix worker `fix-1` on the
    /// integration copy `integ`, and a pattern with a manager that has not been spawned yet.
    fn run_in_fix_round(ctx: &mut TestCtx, integ: &Path) -> (Run, AgentId, AgentId) {
        let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "test".into());
        run.dir = scratch_dir("fix-outage");
        let planner = ctx.add_idle();
        ready(ctx, planner);
        run.planner = Some(planner);
        run.plan = Some(one_task_phase());
        run.stage = Stage::Phase { idx: 0, step: PhaseStep::Fix { round: 1 } };
        run.ws = Some(crate::engine::git::Workspace { worktree: true, repo: PathBuf::from("."), integ: integ.to_path_buf(), branch: "mantra/r".into(), base_branch: "main".into(), git_common_dir: None, note: None, base_commit: None, initial_commit: false });
        let w = ctx.add_idle();
        let mut fix = mk_worker("fix-1", "worker-small", w, WState::Running);
        fix.adhoc = true;
        fix.wt = Some(integ.to_path_buf());
        run.workers.push(fix);
        (run, planner, w)
    }

    /// The gateway outlasted the fix worker's retries: the run halts for the user with the
    /// gateway wording, and neither the manager nor the planner — who run through the same
    /// gateway — is woken. The resume respawns the fix worker where it was: on the integration
    /// copy, for free.
    #[test]
    fn a_fix_worker_the_gateway_failed_halts_without_waking_anyone_and_is_respawned_on_resume() {
        let integ = scratch_dir("fix-outage-integ");
        std::fs::create_dir_all(&integ).unwrap();
        let mut ctx = TestCtx::new();
        let (mut run, planner, w) = run_in_fix_round(&mut ctx, &integ);
        run.gateway_streak.insert(w, Run::GATEWAY_RETRIES);
        let attempts_before = run.max_attempts();
        run.on_turn_done(&mut ctx, w, "failed", Some("unexpected status 503 Service Unavailable".into()), Some(ErrKind::Transient));

        let h = run.halt.as_ref().expect("halted");
        assert_eq!(h.reason, HaltReason::AgentTurnFailed, "the dead-gateway halt, whose band and web hint already fit");
        assert!(h.message.starts_with("fix-1: unexpected status 503") && h.message.ends_with(GATEWAY_DOWN_HINT), "{}", h.message);
        assert!(run.workers[0].transient && matches!(run.workers[0].state, WState::Failed(_)));
        assert!(!ctx.spawns.iter().any(|s| s.name == "manager"), "no manager is spawned for an outage: {:?}", ctx.spawns.iter().map(|s| s.name.clone()).collect::<Vec<_>>());
        assert!(run.manager_escalation.is_none() && !run.escalation_open, "nobody is escalated to");
        assert!(ctx.prompts.iter().all(|(a, _)| *a != planner), "the planner is not woken: {:?}", ctx.prompts);
        assert!(pulse_texts(&run).iter().any(|t| t.starts_with("not escalated")), "{:?}", pulse_texts(&run));

        // the gateway is back; the user presses space — the fix worker's process is gone
        ctx.agents.get_mut(&w).unwrap().status = Status::Stopped;
        let mut jobs = JobCtx(ctx, vec![]);
        run.resume(&mut jobs);
        assert!(!run.halted());
        assert_eq!(run.max_attempts(), attempts_before + 1, "the outage's attempt was not one");
        assert!(pulse_texts(&run).iter().any(|t| t == "respawning fix-1 after the gateway outage"), "{:?}", pulse_texts(&run));
        let fresh = run.workers.iter().rev().find(|w| w.task.id == "fix-1").expect("a new attempt");
        assert!(fresh.attempt == 2 && fresh.adhoc && fresh.state == WState::Preparing, "attempt {} {:?}", fresh.attempt, fresh.state);
        assert_eq!(run.workers[0].state, WState::Cancelled, "the dead attempt is closed");
        assert!(matches!(run.stage, Stage::Phase { step: PhaseStep::Fix { round: 1 }, .. }), "still the same fix round: {:?}", run.stage);
        // its workspace job answers with the integration copy, not a worker worktree
        let (tag, out) = jobs.1.pop().expect("the fix worker's workspace job ran");
        assert!(matches!(&tag, JobTag::WorkerWt { task, attempt: 2 } if task == "fix-1"), "{tag:?}");
        assert!(matches!(&out, JobOut::Wt(Ok((dir, branch))) if *dir == integ && branch.is_empty()), "the integration copy");
        run.on_job(&mut jobs, tag, out);
        let spawned = jobs.0.spawned("fix-1");
        assert_eq!(spawned.len(), 1, "{:?}", jobs.0.spawns.iter().map(|s| s.name.clone()).collect::<Vec<_>>());
        assert_eq!(spawned[0].cwd, integ);
        assert_eq!(spawned[0].sandbox, "danger-full-access", "a fix worker on the integration copy runs unsandboxed");
        let fresh = run.workers.iter().rev().find(|w| w.task.id == "fix-1").unwrap();
        assert!(fresh.state == WState::Running && fresh.wt.as_ref() == Some(&integ));
        assert!(jobs.0.prompts.iter().any(|(a, _)| Some(*a) == fresh.agent), "the fresh fix worker is given its task");
        let _ = std::fs::remove_dir_all(&run.dir);
        let _ = std::fs::remove_dir_all(&integ);
    }

    /// A fix worker that fails for its own reasons is still the chain of command's problem: the
    /// manager gets it first, as before.
    #[test]
    fn a_fix_worker_that_fails_for_its_own_reasons_still_escalates() {
        let integ = scratch_dir("fix-fail-integ");
        let mut ctx = TestCtx::new();
        let (mut run, _planner, w) = run_in_fix_round(&mut ctx, &integ);
        run.on_turn_done(&mut ctx, w, "failed", Some("the model returned an empty response".into()), None);
        let h = run.halt.as_ref().expect("halted");
        assert_eq!(h.reason, HaltReason::AttemptsExhausted);
        assert!(h.message.contains("fix-1 could not fix what gate round 1 found"), "{}", h.message);
        assert!(!h.message.contains("gateway looks down"));
        assert!(!run.workers[0].transient);
        assert!(ctx.spawns.iter().any(|s| s.name == "manager"), "the manager is spawned and asked");
        assert!(run.manager_escalation.is_some());
        let _ = std::fs::remove_dir_all(&run.dir);
    }

    /// Only a failed turn's own error is read for an outage: a blocked fix worker whose report
    /// talks about timeouts and a 503 from the code under test is still escalated.
    #[test]
    fn a_blocked_report_that_mentions_timeouts_is_not_an_outage() {
        let integ = scratch_dir("fix-blocked-integ");
        let mut ctx = TestCtx::new();
        let (mut run, _planner, w) = run_in_fix_round(&mut ctx, &integ);
        ctx.agents.get_mut(&w).unwrap().final_message = Some("status: blocked\nthe API test gets a timeout and a 503 from the stub server; the gate's check cannot pass here".into());
        run.on_turn_done(&mut ctx, w, "completed", None, None);
        let h = run.halt.as_ref().expect("halted");
        assert_eq!(h.reason, HaltReason::AttemptsExhausted);
        assert!(h.message.contains("it is blocked") && !h.message.contains("gateway looks down"), "{}", h.message);
        assert!(run.manager_escalation.is_some(), "the chain of command is asked");
        let _ = std::fs::remove_dir_all(&run.dir);
    }

    /// `halt_and_escalate` itself, handed a transport error (a task out of attempts whose last
    /// failure was a 503): the halt stands, with the gateway wording, and nobody is woken — the
    /// manager and the planner share the outage.
    #[test]
    fn an_escalated_halt_over_a_transport_error_wakes_nobody() {
        for pattern in [Pattern::builtin(), no_manager()] {
            let mut ctx = TestCtx::new();
            let mut run = Run::new(PathBuf::from("."), pattern, "test".into());
            run.dir = scratch_dir("escalate-503");
            let planner = ctx.add_idle();
            run.planner = Some(planner);
            let w = ctx.add_idle();
            let mut worker = mk_worker("t1", "worker-small", w, WState::Failed("unexpected status 503 Service Unavailable".into()));
            worker.attempt = run.max_attempts();
            run.workers.push(worker);
            let out = run.spawn_task(&mut ctx, Task { id: "t1".into(), role: "worker-small".into(), ..Default::default() }, None, None);
            assert!(out.starts_with("REFUSED"), "{out}");
            let h = run.halt.as_ref().expect("halted");
            assert_eq!(h.reason, HaltReason::AttemptsExhausted, "the halt keeps its own reason (the web view offers Retry t1 from it)");
            assert!(h.message.ends_with(GATEWAY_DOWN_RETRY_HINT), "no worker is left to respawn: {}", h.message);
            assert!(!run.escalation_open && run.manager_escalation.is_none());
            assert!(!ctx.spawns.iter().any(|s| s.name == "manager"));
            assert!(ctx.prompts.iter().all(|(a, _)| *a != planner), "{:?}", ctx.prompts);
            let _ = std::fs::remove_dir_all(&run.dir);
        }
    }

    // Per-agent model switches ------------------------------------------------------------------

    fn known(aliases: &[(&str, u64)]) -> HashMap<String, u64> {
        aliases.iter().map(|(a, w)| (a.to_string(), *w)).collect()
    }

    /// `set_agent_model` is a switch for that agent: the task's next attempt keeps it, the role's
    /// other tasks do not see it, the pattern copy is untouched, and switching back forgets it.
    #[test]
    fn a_live_model_switch_sticks_to_that_task_and_not_to_its_role() {
        let mut ctx = TestCtx::new();
        let mut run = Run::new(PathBuf::from("."), no_manager(), "test".into());
        run.dir = scratch_dir("agent-model");
        run.set_known_models(known(&[("luna", 200_000), ("nova", 128_000)]));
        let role_model = run.role("worker-small").model.clone();
        run.plan = Some(one_task_phase());
        run.stage = Stage::Phase { idx: 0, step: PhaseStep::Orchestrating };
        let w = ctx.add_idle();
        ready(&mut ctx, w);
        run.workers.push(mk_worker("t1", "worker-small", w, WState::Running));
        run.on_ready(&mut ctx, w, false, false);
        run.set_agent_model(w, "nova");
        assert_eq!(run.workers[0].model.as_deref(), Some("nova"));
        assert_eq!(run.pattern.role("worker-small").unwrap().model, role_model, "the role is not rewritten");
        assert_eq!(run.agent_meta[&w].model_alias, "nova", "state.json carries it");
        // a respawn of the same task runs on the switched model …
        run.retry_worker(&mut ctx, w, None);
        let again = run.workers.iter().rev().find(|w| w.task.id == "t1").map(|w| (w.attempt, w.model.clone())).unwrap();
        assert_eq!(again, (2, Some("nova".to_string())));
        run.on_job(&mut ctx, JobTag::WorkerWt { task: "t1".into(), attempt: 2 }, JobOut::Wt(Ok((PathBuf::from("."), String::new()))));
        assert_eq!(ctx.spawned("t1").last().unwrap().model, "nova");
        // … another task of the same role does not
        run.spawn_task(&mut ctx, Task { id: "t2".into(), role: "worker-small".into(), ..Default::default() }, None, None);
        run.on_job(&mut ctx, JobTag::WorkerWt { task: "t2".into(), attempt: 1 }, JobOut::Wt(Ok((PathBuf::from("."), String::new()))));
        assert_eq!(ctx.spawned("t2").last().unwrap().model, role_model);
        // switching back to the role's model is no longer a choice of its own
        let a2 = run.workers.iter().rev().find(|w| w.task.id == "t1").and_then(|w| w.agent).unwrap();
        run.set_agent_model(a2, &role_model);
        assert_eq!(run.workers.iter().rev().find(|w| w.task.id == "t1").unwrap().model, None);
        let _ = std::fs::remove_dir_all(&run.dir);
    }

    /// A fixed agent's switch belongs to its slot: the planner respawned keeps the model, and
    /// so does the next orchestrator of the run — not the role's model from the pattern.
    #[test]
    fn a_fixed_agents_model_switch_survives_its_respawn() {
        let mut ctx = TestCtx::new();
        let mut run = Run::new(PathBuf::from("."), no_manager(), "test".into());
        run.dir = scratch_dir("slot-model");
        run.set_known_models(known(&[("astra", 200_000), ("nova", 128_000)]));
        run.stage = Stage::Planning;
        let p = run.spawn_planner(&mut ctx);
        let role_model = run.role(&run.pattern.flow.planner.clone()).model.clone();
        assert_eq!(ctx.spawned("planner")[0].model, role_model);
        run.set_agent_model(p, "nova");
        run.respawn(&mut ctx, p, None).unwrap();
        assert_eq!(ctx.spawned("planner").last().unwrap().model, "nova", "the respawned planner keeps the switch");
        // the orchestrator slot is its own
        run.plan = Some(one_task_phase());
        run.stage = Stage::Phase { idx: 0, step: PhaseStep::Orchestrating };
        let o = run.spawn_orchestrator(&mut ctx);
        assert_eq!(ctx.spawned("orchestrator")[0].model, run.role("orchestrator").model, "untouched by the planner's switch");
        run.set_agent_model(o, "nova");
        run.respawn(&mut ctx, o, None).unwrap();
        assert_eq!(ctx.spawned("orchestrator").last().unwrap().model, "nova");
        let _ = std::fs::remove_dir_all(&run.dir);
    }

    /// A finale agent's switch is for its step: a respawn of that step keeps it, the next step —
    /// another role — starts on that role's own model.
    #[test]
    fn a_finale_switch_does_not_carry_over_to_the_next_step() {
        let mut ctx = TestCtx::new();
        let mut pat = no_manager();
        pat.flow.finale = vec![
            crate::engine::pattern::FinaleStep { role: "qa".into(), task: "review".into(), may_spawn: false },
            crate::engine::pattern::FinaleStep { role: "worker-big".into(), task: "docs".into(), may_spawn: false },
        ];
        let mut run = Run::new(PathBuf::from("."), pat, "test".into());
        run.dir = scratch_dir("finale-model");
        run.plan = Some(one_task_phase());
        run.start_finale(&mut ctx, 0);
        let f = run.finale_agent.expect("a finale agent");
        run.set_agent_model(f, "nova");
        run.respawn(&mut ctx, f, None).unwrap();
        assert_eq!(ctx.spawned("qa").last().unwrap().model, "nova", "the respawned step keeps the switch");
        run.start_finale(&mut ctx, 1);
        assert_eq!(ctx.spawned("worker-big").last().unwrap().model, run.role("worker-big").model, "the next step is another role's");
        assert!(run.slot_models.is_empty());
        let _ = std::fs::remove_dir_all(&run.dir);
    }

    /// A switched agent that goes silent twice falls back like any other: the role's fallback
    /// replaces the model the user picked, which no longer outranks the role.
    #[test]
    fn the_fallback_replaces_a_switched_model_that_went_silent() {
        let mut ctx = TestCtx::new();
        let mut pat = no_manager();
        pat.roles.get_mut("planner").unwrap().fallback = Some("luna".into());
        let mut run = Run::new(PathBuf::from("."), pat, "test".into());
        run.dir = scratch_dir("slot-fallback");
        run.stage = Stage::Planning;
        let p = run.spawn_planner(&mut ctx);
        run.set_agent_model(p, "nova");
        ctx.agents.get_mut(&p).unwrap().model_alias = "nova".into();
        let key = run.first_token_key(p).unwrap();
        run.first_token_strikes.insert(key, 1);
        run.on_first_token_deadline(&mut ctx, p, Duration::from_secs(300));
        assert_eq!(ctx.spawned("planner").last().unwrap().model, "luna", "on the fallback, not back on the switch");
        assert!(run.slot_models.is_empty());
        let _ = std::fs::remove_dir_all(&run.dir);
    }

    // The context cap ---------------------------------------------------------------------------

    /// Every spawn is told `min(model window, context_cap)`: a big model is capped, a small one
    /// keeps its own window, a halved assumption is capped too, and `0` leaves the window alone.
    #[test]
    fn the_context_cap_bounds_the_window_a_spawn_is_told() {
        let mut ctx = TestCtx::new();
        let mut run = Run::new(PathBuf::from("."), no_manager(), "test".into());
        run.dir = scratch_dir("context-cap");
        assert_eq!(run.pattern.settings.context_cap, 200_000, "the built-in default");
        let planner_model = run.role(&run.pattern.flow.planner.clone()).model.clone();
        let worker_model = run.role("worker-small").model.clone();
        run.set_known_models(known(&[(planner_model.as_str(), 1_000_000), (worker_model.as_str(), 128_000)]));
        run.stage = Stage::Planning;
        run.spawn_planner(&mut ctx);
        assert_eq!(ctx.spawned("planner")[0].context_override, Some(200_000), "a 1M model is told 200k");
        run.plan = Some(one_task_phase());
        run.stage = Stage::Phase { idx: 0, step: PhaseStep::Orchestrating };
        run.spawn_task(&mut ctx, Task { id: "t1".into(), role: "worker-small".into(), ..Default::default() }, None, None);
        run.on_job(&mut ctx, JobTag::WorkerWt { task: "t1".into(), attempt: 1 }, JobOut::Wt(Ok((PathBuf::from("."), String::new()))));
        assert_eq!(ctx.spawned("t1")[0].context_override, Some(128_000), "a 128k model keeps its window");
        // a halved assumption from an earlier attempt is still capped
        assert_eq!(run.context_for(&planner_model, Some(100_000)), Some(100_000));
        assert_eq!(run.context_for(&planner_model, Some(400_000)), Some(200_000));
        assert_eq!(run.context_for("unknown-alias", None), Some(200_000), "nothing known: the cap stands");
        // 0 = the model's own window, whatever it is
        run.pattern.settings.context_cap = 0;
        assert_eq!(run.context_for(&planner_model, None), None);
        assert_eq!(run.context_for(&planner_model, Some(100_000)), Some(100_000));
        run.respawn_orchestrator(&mut ctx, None);
        assert_eq!(ctx.spawned("orchestrator")[0].context_override, None);
        let _ = std::fs::remove_dir_all(&run.dir);
    }

    /// A model with a big window of its own, told 200k by the cap, that still fills its context
    /// is not on an *assumed* window: it is not halved. One the registry only assumes 200k for is
    /// — the window it was told, which a smaller cap makes smaller than 200k.
    #[test]
    fn a_capped_window_is_not_mistaken_for_an_assumed_one() {
        // (the model's window in the registry, the window the agent was told, what it is halved to)
        let assumed = crate::discover::ASSUMED_CONTEXT;
        for (window, told, halved) in [(1_000_000u64, 200_000u64, None), (assumed, assumed, Some(assumed / 2)), (assumed, 150_000, Some(75_000)), (128_000, 128_000, None)] {
            let mut ctx = TestCtx::new();
            let mut run = Run::new(PathBuf::from("."), no_manager(), "test".into());
            run.dir = scratch_dir("cap-shrink");
            run.set_known_models(known(&[("big", window)]));
            let w = ctx.add_idle();
            let ag = ctx.agents.get_mut(&w).unwrap();
            ag.model_alias = "big".into();
            ag.ctx_window = Some(told);
            run.workers.push(mk_worker("t1", "worker-small", w, WState::Running));
            run.shrink_assumed_context(&mut ctx, w);
            assert_eq!(run.workers[0].context_override, halved, "window {window}, told {told}: an assumed window under a smaller cap is halved too");
            let _ = std::fs::remove_dir_all(&run.dir);
        }
    }

    // The owner file ----------------------------------------------------------------------------

    /// A run claims its directory while it is live, refreshes the claim with every flush, and
    /// gives it back when it ends — or when the `Run` is dropped by a process that is still alive.
    #[test]
    fn a_run_owns_its_directory_while_it_is_live() {
        use crate::engine::state::Owner;
        let mut ctx = TestCtx::new();
        let mut run = Run::new(PathBuf::from("."), no_manager(), "test".into());
        run.dir = scratch_dir("owner");
        let dir = run.dir.clone();
        run.start(&mut ctx);
        let o = Owner::load(&dir).expect("owner.json written at start");
        assert_eq!(o.pid, std::process::id());
        assert!(o.alive(), "a live owner: another Mantra's resume of it would be refused");
        run.stage = Stage::Planning;
        run.save_state();
        assert!(Owner::load(&dir).is_some(), "refreshed with the flush");
        run.fail_run(&mut ctx, "boom".into());
        assert!(Owner::load(&dir).is_none(), "released once the run is over");
        // a live run dropped by its process (a clean quit) releases it too
        let mut run = Run::new(PathBuf::from("."), no_manager(), "test".into());
        run.dir = dir.clone();
        run.start(&mut ctx);
        assert!(Owner::load(&dir).is_some());
        drop(run);
        assert!(Owner::load(&dir).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    // MCP servers and vision ------------------------------------------------------------------

    /// Every way an agent of a role comes to be hands the role's `mcp` on: planner, orchestrator,
    /// manager, workers, the phase gate, a finale step, and one re-attached on resume.
    #[test]
    fn every_spawn_carries_its_roles_mcp_servers() {
        let mut ctx = TestCtx::new();
        let mut p = Pattern::builtin();
        for (name, r) in p.roles.iter_mut() {
            r.mcp = vec![format!("{name}-tools"), "*".into()];
        }
        let mut run = Run::new(PathBuf::from("."), p, "test".into());
        run.plan = Some(mixed_phase());
        run.stage = Stage::Phase { idx: 0, step: PhaseStep::Orchestrating };
        run.spawn_planner(&mut ctx);
        run.spawn_orchestrator(&mut ctx);
        run.ensure_manager(&mut ctx);
        for t in mixed_phase().phases[0].tasks.clone() {
            launch(&mut ctx, &mut run, t, None);
        }
        run.spawn_gate(&mut ctx, 0);
        run.planner = Some(ctx.add_idle());
        run.start_finale(&mut ctx, 0);
        run.spawn_planner_with(&mut ctx, Some("thread-1".into()));
        let names: Vec<&str> = ctx.spawns.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["planner", "orchestrator", "manager", "s1", "b1", "qa", "qa-heavy", "planner"]);
        for s in &ctx.spawns {
            assert_eq!(s.mcp, vec![format!("{}-tools", s.role_name), "*".to_string()], "{}", s.name);
        }
        // a role that asks for none (every built-in one) gets none
        let mut ctx = TestCtx::new();
        let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "test".into());
        run.spawn_planner(&mut ctx);
        assert!(ctx.spawned("planner")[0].mcp.is_empty());
    }

    /// The screenshot and view-image tools reach an agent only when the app said its model takes
    /// images — never by default, never for the other roles.
    #[test]
    fn the_vision_tools_go_only_to_a_model_that_sees() {
        let has_vision = |s: &Spawned| tools::VISION_TOOLS.iter().all(|t| s.tools.iter().any(|x| x == t));
        let has_any = |s: &Spawned| s.tools.iter().any(|x| tools::VISION_TOOLS.contains(&x.as_str()));
        let mut p = Pattern::builtin();
        p.roles.get_mut("worker-small").expect("worker-small").model = "eye".into();
        let mut ctx = TestCtx::new();
        let mut run = Run::new(PathBuf::from("."), p, "test".into());
        run.plan = Some(mixed_phase());
        run.stage = Stage::Phase { idx: 0, step: PhaseStep::Orchestrating };
        // never told what sees: nobody gets them
        launch(&mut ctx, &mut run, Task { id: "s0".into(), role: "worker-small".into(), ..Default::default() }, None);
        assert!(!has_any(&ctx.spawned("s0")[0]), "{:?}", ctx.spawned("s0")[0].tools);
        run.set_vision_models(["eye".to_string()].into_iter().collect());
        for t in mixed_phase().phases[0].tasks.clone() {
            launch(&mut ctx, &mut run, t, None);
        }
        assert!(has_vision(&ctx.spawned("s1")[0]), "{:?}", ctx.spawned("s1")[0].tools);
        assert!(ctx.spawned("s1")[0].tools.contains(&"mantra_ask".to_string()), "beside the role's own tools");
        assert!(!has_any(&ctx.spawned("b1")[0]), "worker-big's model does not see");
        run.spawn_planner(&mut ctx);
        assert!(!has_any(&ctx.spawned("planner")[0]));
        // a role whose model sees gets them on every path, the planner's included
        let planner_model = run.role("planner").model;
        run.set_vision_models([planner_model].into_iter().collect());
        run.spawn_planner_with(&mut ctx, Some("thread-1".into()));
        assert!(has_vision(ctx.spawned("planner").last().unwrap()));
    }

    /// `mantra_screenshot` and `mantra_view_image` answer with the text and an `inputImage`: the
    /// capture via `$MANTRA_SCREENSHOT_CMD` (here a script copying a generated PNG), saved as
    /// `<run dir>/screenshots/<agent>-<n>.png`; an image read from the agent's cwd within its caps.
    #[test]
    fn a_screenshot_and_an_image_come_back_as_input_images() {
        let dir = scratch_dir("vision");
        std::fs::create_dir_all(&dir).unwrap();
        let png = crate::util::tests::tiny_png(5, 3);
        std::fs::write(dir.join("src.png"), &png).unwrap();
        let script = dir.join("shot.sh");
        std::fs::write(&script, format!("#!/bin/sh\ncp {} \"$1\"\n", dir.join("src.png").display())).unwrap();
        // the only test that sets it: every other capture test passes its template explicitly
        std::env::set_var(crate::util::SCREENSHOT_VAR, format!("sh {} {{out}}", script.display()));
        let mut ctx = TestCtx::new();
        let mut run = Run::new(dir.clone(), Pattern::builtin(), "test".into());
        run.dir = dir.join("run");
        run.set_vision_models(["eye".to_string()].into_iter().collect());
        let a = ctx.add_idle();
        if let Some(ag) = ctx.agents.get_mut(&a) {
            ag.model_alias = "eye".into();
            ag.cwd = dir.clone();
        }
        run.workers.push(mk_worker("p1-ui", "worker-small", a, WState::Running));
        let want_url = format!("data:image/png;base64,{}", crate::util::b64(&png));

        run.on_tool_call(&mut ctx, a, json!(1), "mantra_screenshot", &json!({}));
        assert!(ctx.items.is_empty() && ctx.jobs.iter().any(|j| j.starts_with("Vision")), "captured as a job, off the event loop: {:?}", ctx.jobs);
        ctx.settle(&mut run);
        let (who, items, ok) = ctx.items.last().cloned().expect("an answer");
        assert!(ok && who == a, "{items:?}");
        let text = items[0]["text"].as_str().unwrap_or_default().to_string();
        let shot = run.dir.join("screenshots").join("p1-ui-1.png");
        assert!(text.contains(&format!("{}", std::fs::canonicalize(&shot).unwrap().display())) && text.contains("image/png, 5x3 px") && text.contains("via MANTRA_SCREENSHOT_CMD"), "{text}");
        assert_eq!(items[1], json!({"type": "inputImage", "imageUrl": want_url}));
        assert_eq!(std::fs::read(&shot).unwrap(), png);
        {
            // the host's screen is nobody else's to read: an owner-only directory and file
            use std::os::unix::fs::PermissionsExt;
            let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
            assert_eq!((mode(&run.dir.join("screenshots")), mode(&shot)), (0o700, 0o600));
        }
        run.on_tool_call(&mut ctx, a, json!(2), "mantra_screenshot", &json!({"region": "0,0,5,3"}));
        ctx.settle(&mut run);
        assert!(run.dir.join("screenshots").join("p1-ui-2.png").exists(), "the next capture gets the next number");
        assert!(ctx.items.last().unwrap().0 == a && ctx.items.last().unwrap().2);
        assert!(run.pulse.iter().any(|l| l.text.contains("p1-ui took a screenshot")), "journaled");

        // view_image: relative to the agent's cwd, by its bytes, within 8 MB
        run.on_tool_call(&mut ctx, a, json!(3), "mantra_view_image", &json!({"path": "src.png"}));
        ctx.settle(&mut run);
        let (_, items, ok) = ctx.items.last().cloned().unwrap();
        assert!(ok && items[0]["text"].as_str().unwrap_or_default().contains("src.png (image/png, 5x3 px"), "{items:?}");
        assert_eq!(items[1]["imageUrl"], json!(want_url));
        std::fs::write(dir.join("notes.png"), "not an image").unwrap();
        let big = std::fs::File::create(dir.join("big.png")).unwrap();
        big.set_len(crate::util::IMAGE_MAX_BYTES + 1).unwrap();
        for (args, says) in [(json!({"path": "notes.png"}), "is not a PNG, JPEG, GIF or WebP image"), (json!({"path": "big.png"}), "images go up to 8.0 MiB"), (json!({}), "path is required"), (json!({"path": "gone.png"}), "cannot read")] {
            run.on_tool_call(&mut ctx, a, json!(4), "mantra_view_image", &args);
            ctx.settle(&mut run);
            let (_, items, ok) = ctx.items.last().cloned().unwrap();
            assert!(!ok && items.len() == 1 && items[0]["text"].as_str().unwrap_or_default().contains(says), "{args}: {items:?}");
        }

        // an agent whose model does not see is refused, and nothing is captured
        let b = ctx.add_idle();
        run.workers.push(mk_worker("p1-api", "worker-small", b, WState::Running));
        let jobs = ctx.jobs.len();
        run.on_tool_call(&mut ctx, b, json!(5), "mantra_screenshot", &json!({}));
        assert_eq!(ctx.jobs.len(), jobs, "refused at once, no job");
        let (_, items, ok) = ctx.items.last().cloned().unwrap();
        assert!(!ok && items[0]["text"].as_str().unwrap_or_default().contains("does not take images"), "{items:?}");
        assert!(!run.dir.join("screenshots").join("p1-api-1.png").exists());
    }
}
