//! Application state and event routing. The UI draws from this; the engine acts through `Ctxt`.

use crate::agent::{Agent, Level, Signal, Status};
use crate::config::{Registry, Settings};
use crate::engine::pattern::Pattern;
use crate::engine::run::{Ctx, JobOut, JobTag, Run, Send, SpawnReq, Stage};
use crate::engine::tools;
use crate::hub::{AgentId, Cmd, Hub, HubEvent, SpawnSpec};
use crate::ui::input::{Act, Input};
use ratatui::crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEventKind};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tokio::sync::mpsc::UnboundedSender;

pub enum AppEvent {
    Term(Event),
    Hub(HubEvent),
    Job(JobTag, JobOut),
    /// One discovery source answered (Codex catalog or a provider's /models).
    Discovered { source: String, result: Result<Vec<crate::discover::Candidate>, String> },
    /// A command from a web client (`--web` / `--remote`), executed by `App::web_command`.
    Web(crate::web::Inbound),
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Screen {
    Solo,
    Stage,
    Zoom(AgentId),
    Studio,
    Models,
}

#[derive(Clone, Debug)]
pub enum EditTarget {
    RoleField(String, String),
    Setting(String),
    FlowStep(usize, String),
    ModelCell(usize, usize),
    ProviderCell(usize, usize),
    NewRole,
    NewPattern,
}

pub enum Overlay {
    Help,
    ModelPicker { sel: usize, target: Option<AgentId> },
    Diff { agent: AgentId, file: usize, scroll: usize },
    Inbox { sel: usize },
    Plan { scroll: usize },
    Edit { title: String, input: Input, target: EditTarget },
    Patterns { sel: usize, list: Vec<String> },
    Discover(DiscoverState),
    /// `/runs`: this project's runs — resume (⏎) or delete (D, then y).
    Runs { sel: usize, list: Vec<crate::engine::state::RunSummary>, confirm: bool, others: usize },
    /// `/web`: the local web UI's addresses and security.
    Web,
    /// `/remote`: the relay link, code, password and QR (`r` rotates).
    Remote,
}

/// The model-discovery picker.
pub struct DiscoverState {
    pub items: Vec<crate::discover::Candidate>,
    pub loading: Vec<String>,
    pub errors: Vec<String>,
    pub sel: usize,
    pub filter: Input,
}

impl DiscoverState {
    /// Indices of items matching the filter.
    pub fn visible(&self) -> Vec<usize> {
        let f = self.filter.text().to_lowercase();
        (0..self.items.len()).filter(|i| f.is_empty() || format!("{} {}", self.items[*i].provider, self.items[*i].model).to_lowercase().contains(&f)).collect()
    }
}

pub struct Approval {
    pub agent: AgentId,
    pub id: Value,
    pub method: String,
    pub title: String,
    pub detail: String,
    pub params: Value,
    pub at: Instant,
}

/// Selection in the Studio's left list, by identity rather than by index into a list that gets
/// re-sorted on every draw (`ui::studio::entries`/`ordered_roles`) — so changing a role's `kind`
/// (which moves it in the sort order) never lands the highlight on a different role.
#[derive(Clone, Debug, PartialEq)]
pub enum StudioSel {
    Role(String),
    Settings,
    Flow,
}

pub struct StudioState {
    pub pattern: Pattern,
    pub sel: StudioSel,
    pub field: usize,
    pub focus: u8, // 0 list, 1 fields, 2 architect input
    pub errors: Vec<String>,
    pub dirty: bool,
    pub architect: Option<AgentId>,
    pub input: Input,
    pub flash: Option<Instant>,
}

pub struct ModelsState {
    pub row: usize,
    pub col: usize,
    pub providers: bool,
    pub status: HashMap<String, String>,
    pub dirty: bool,
}

pub const COMMANDS: &[(&str, &str)] = &[
    ("/model", "switch model (picker)"),
    ("/effort", "set reasoning effort: low|medium|high|xhigh|max"),
    ("/approvals", "approval mode: untrusted|on-request|never"),
    ("/new", "start a fresh Solo session"),
    ("/compact", "compact the current agent's context"),
    ("/diff", "open the changes viewer"),
    ("/mandala", "open the Mandala stage"),
    ("/run", "start a Mandala run: /run <goal>"),
    ("/pattern", "choose the pattern for new runs"),
    ("/runs", "this project's runs: resume or delete"),
    ("/plan", "show the run's plan"),
    ("/pause", "pause / resume the run"),
    ("/respawn", "respawn the focused agent in place (planner/orchestrator/gate/finale/worker)"),
    ("/land", "merge the finished run branch into your branch"),
    ("/studio", "pattern studio (roles, flow, architect agent)"),
    ("/models", "model registry (context, efforts, providers)"),
    ("/inbox", "approvals & alerts"),
    ("/web", "web UI: address, TLS, password (mantra --web)"),
    ("/remote", "remote link, code, password & QR (mantra --remote)"),
    ("/verbose", "toggle verbose log (ctrl+e)"),
    ("/help", "keys & commands"),
    ("/quit", "exit Mantra"),
];

pub struct App {
    pub settings: Settings,
    pub registry: Registry,
    pub project: PathBuf,
    pub hub: Hub,
    pub agents: BTreeMap<AgentId, Agent>,
    pub screen: Screen,
    /// The screen `/studio` or `/models` was opened from. Leaving them returns exactly there, so a
    /// zoom (or Solo while a run is up) survives a trip through the Studio instead of being
    /// guessed at from whether a run exists.
    screen_before: Option<Screen>,
    pub overlays: Vec<Overlay>,
    pub input: Input,
    pub solo: Option<AgentId>,
    pub run: Option<Run>,
    pub approvals: Vec<Approval>,
    pub toast: Option<(String, Instant, Level)>,
    pub sel: usize,
    pub canvas_focus: bool,
    pub verbose: bool,
    pub side_panel: bool,
    pub pulse_panel: bool,
    pub quit: bool,
    /// When ctrl+c was last pressed and how many times in a row (within 2s of each other):
    /// the first press does the contextual thing (close an overlay, clear the input, interrupt
    /// the focused agent's turn), the third always quits.
    ctrl_c: Option<Instant>,
    ctrl_c_count: u8,
    pub tx: UnboundedSender<AppEvent>,
    pub demo: bool,
    pub studio: StudioState,
    pub models_ui: ModelsState,
    pub suggest: usize,
    pub notes: Vec<String>,
    pub pattern_name: String,
    pub branch: String,
    /// Ids of this project's unfinished runs at startup (welcome-screen notice); kept in sync by
    /// `/runs` resume/delete.
    pub unfinished_runs: Vec<String>,
    /// `util::sandbox_probe` failure at startup (Linux without user namespaces): shown on the
    /// welcome screens, in the Solo log and in every run's pulse, so nobody burns gate rounds on
    /// `bwrap` errors (L1).
    pub sandbox_warning: Option<String>,
    pub probes: HashMap<AgentId, (String, Instant)>,
    pub pulse_scroll: usize,
    pub force_clear: bool,
    /// Set on every Stage↔Zoom switch; drives a brief header tint so a jump never feels silent.
    pub flash_screen: Option<Instant>,
    /// The web layer, when `--web` / `--remote` is on: replies to web commands, the relay
    /// identity for `/remote`, push subscriptions.
    pub web: Option<crate::web::Link>,
}

/// Options for creating any agent (Solo, run agents, architect, probes).
pub struct AgentOpts {
    pub name: String,
    pub role: String,
    pub glyph: String,
    pub color: String,
    pub model_alias: String,
    pub effort: Option<String>,
    pub cwd: PathBuf,
    pub approval: String,
    pub sandbox: String,
    pub instructions: String,
    pub tools: Vec<Value>,
    pub extra_writable: Vec<PathBuf>,
    /// Explicit context-window override (from a halved assumption); `None` uses the model's own
    /// effective context.
    pub context_override: Option<u64>,
    /// Re-attach to a saved Codex thread / Claude session instead of starting a new one
    /// (`mantra runs resume`).
    pub resume_thread: Option<String>,
}

fn toml_str(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

pub fn spawn_agent(hub: &mut Hub, agents: &mut BTreeMap<AgentId, Agent>, reg: &Registry, o: AgentOpts) -> AgentId {
    let id = hub.alloc_id();
    let m = reg.resolve(&o.model_alias);
    let effort = m.resolve_effort(o.effort.as_deref().filter(|e| !e.is_empty()).unwrap_or(&m.default_effort));
    let backend = reg.backend_of(&m);
    // Always tell the backend a context window and compaction limit — an assumed 200k / 85% when
    // the model doesn't have its own, so auto-compaction never relies on a provider's own defaults
    // (Codex: `-c model_context_window=…`/`model_auto_compact_token_limit=…`; Claude: `--autocompact`).
    let cw = o.context_override.unwrap_or_else(|| m.effective_context());
    let compact_pct = m.effective_compact_percent();
    let (extra, envs, claude) = if backend == crate::hub::Backend::ClaudeCode {
        // WP10: no Codex `-c` args at all; auth/base_url/key come from the model's own provider
        // entry, resolved here (not in `hub::claude`, which stays decoupled from `config::Registry`
        // — see `SpawnSpec::claude`/`SpawnSpec::envs`).
        let provider = reg.providers.iter().find(|p| p.id == m.provider);
        let auth = provider.map(|p| p.auth.clone()).filter(|a| !a.is_empty()).unwrap_or_else(|| "subscription".into());
        let base_url = provider.map(|p| p.base_url.clone()).unwrap_or_default();
        let mut envs = vec![];
        if auth == "api_key" {
            if let Some(key) = provider.and_then(|p| p.resolve_key()) {
                envs.push(("ANTHROPIC_API_KEY".to_string(), key));
            }
        }
        (vec![], envs, Some(crate::hub::ClaudeSpawn { auth, base_url, autocompact: cw, system_prompt: o.instructions.clone(), mcp_sock: hub.bridge_sock() }))
    } else {
        let mut extra = reg.provider_args();
        extra.push("-c".into());
        extra.push(format!("model_context_window={cw}"));
        extra.push("-c".into());
        extra.push(format!("model_auto_compact_token_limit={}", cw * compact_pct.min(99) as u64 / 100));
        if m.is_custom_provider() && m.efforts().is_empty() {
            // No effort control → don't send any reasoning block (strict gateways reject it).
            // (Models *with* efforts need nothing extra: Codex ≥ 0.150 sends `reasoning.effort`
            // to any model once `turn/start` carries an effort, and the old
            // `model_supports_reasoning_summaries` key is now unrecognised — passing it put a
            // "Codex is ignoring 1 unrecognized configuration setting" banner in every log.)
            extra.push("-c".into());
            extra.push("model_reasoning_summary=\"none\"".into());
        }
        if !o.extra_writable.is_empty() {
            let list: Vec<String> = o.extra_writable.iter().map(|p| toml_str(&p.to_string_lossy())).collect();
            extra.push("-c".into());
            extra.push(format!("sandbox_workspace_write.writable_roots=[{}]", list.join(",")));
        }
        // Deliver a key stored directly in the provider (not just named by env_key) into the
        // child's own environment — never on argv, never logged.
        let envs: Vec<(String, String)> = reg
            .providers
            .iter()
            .find(|p| p.id == m.provider)
            .and_then(|p| p.resolve_key().map(|k| (p.env_var_name(), k)))
            .into_iter()
            .collect();
        (extra, envs, None)
    };
    let spec = SpawnSpec {
        cwd: o.cwd.clone(),
        model: m.model.clone(),
        provider: m.provider.clone(),
        effort: effort.clone(),
        approval: o.approval.clone(),
        sandbox: if o.sandbox.is_empty() { "workspace-write".into() } else { o.sandbox.clone() },
        developer_instructions: o.instructions.clone(),
        dynamic_tools: o.tools.clone(),
        config: serde_json::Map::new(),
        extra_args: extra,
        resume_thread: o.resume_thread.clone(),
        backend,
        envs,
        claude,
    };
    let mut a = Agent::new(id, &o.name, &o.role, o.cwd);
    a.glyph = o.glyph;
    a.color = o.color;
    a.model_alias = m.alias.clone();
    a.model = m.model.clone();
    a.provider = m.provider.clone();
    a.backend = backend;
    a.effort = effort;
    a.ctx_window = Some(cw);
    a.approval = o.approval.clone();
    agents.insert(id, a);
    hub.spawn(id, spec);
    id
}

/// Send a prompt to an agent. `mode` decides what happens when it's already busy:
/// - `Auto` (the engine's own steering): new turn if idle, steer immediately if busy, queue if a
///   turn is only starting (unchanged pre-WP9 behaviour).
/// - `Queue` (the UI's plain Enter): never steers — a message typed mid-turn is appended to
///   `Agent.queued` and shown as a chip; it is delivered once the turn ends (or, if it arrived
///   while the turn was only starting, once `turn/started` drains the queue).
/// - `Force` (ctrl+f): delivers the queue plus this message into a running turn right away
///   (`turn/steer`), without interrupting it. Against an agent that's only starting a turn, the
///   force is deferred the same way `Queue` is — `turn/started` drains it moments later. Against
///   an idle agent it behaves exactly like `Auto`/Enter.
pub fn prompt_agent(hub: &Hub, agents: &mut BTreeMap<AgentId, Agent>, id: AgentId, text: String, echo: bool, mode: Send) {
    let Some(a) = agents.get_mut(&id) else { return };
    // Any message — from the user, the orchestrator or the engine — ends a hand stop: talking to
    // an agent *is* telling it to carry on. Every send funnels through here, so this one line is
    // the whole lifecycle of the flag.
    a.stopped_by_user = false;
    if echo {
        a.push_user(&text);
    }
    a.follow = true;
    a.scroll = 0;
    if matches!(a.status, Status::Stopped) {
        a.notice(Level::Warn, "agent is stopped");
        return;
    }
    if mode == Send::Force && a.turn_active {
        let mut joined = std::mem::take(&mut a.queued);
        if !text.trim().is_empty() {
            joined.push(text);
        }
        if !joined.is_empty() {
            hub.send(id, Cmd::Steer { text: joined.join("\n\n") });
        }
        return;
    }
    if text.trim().is_empty() {
        return;
    }
    if a.awaiting_start {
        a.queued.push(text);
    } else if a.turn_active {
        if mode == Send::Queue {
            a.queued.push(text);
        } else {
            hub.send(id, Cmd::Steer { text });
        }
    } else {
        a.awaiting_start = true;
        a.status = Status::Busy;
        a.activity = "starting turn".into();
        hub.send(id, Cmd::Turn { text });
    }
}

/// The engine's window into the app.
pub struct Ctxt<'a> {
    pub hub: &'a mut Hub,
    pub agents: &'a mut BTreeMap<AgentId, Agent>,
    pub registry: &'a Registry,
    pub tx: &'a UnboundedSender<AppEvent>,
    pub notes: &'a mut Vec<String>,
}

impl Ctx for Ctxt<'_> {
    fn spawn(&mut self, r: SpawnReq) -> AgentId {
        let sandbox = if r.role.sandbox.is_empty() { "workspace-write".to_string() } else { r.role.sandbox.clone() };
        spawn_agent(
            self.hub,
            self.agents,
            self.registry,
            AgentOpts {
                name: r.name,
                role: r.role_name,
                glyph: r.role.glyph.clone(),
                color: r.role.color.clone(),
                model_alias: r.role.model.clone(),
                effort: r.effort.or(Some(r.role.effort.clone())),
                cwd: r.cwd,
                approval: r.role.permission.clone(),
                sandbox,
                instructions: r.instructions,
                tools: r.tools,
                extra_writable: r.extra_writable,
                context_override: r.context_override,
                resume_thread: None,
            },
        )
    }
    fn spawn_resumed(&mut self, r: SpawnReq, thread: String) -> AgentId {
        let sandbox = if r.role.sandbox.is_empty() { "workspace-write".to_string() } else { r.role.sandbox.clone() };
        spawn_agent(
            self.hub,
            self.agents,
            self.registry,
            AgentOpts {
                name: r.name,
                role: r.role_name,
                glyph: r.role.glyph.clone(),
                color: r.role.color.clone(),
                model_alias: r.role.model.clone(),
                effort: r.effort.or(Some(r.role.effort.clone())),
                cwd: r.cwd,
                approval: r.role.permission.clone(),
                sandbox,
                instructions: r.instructions,
                tools: r.tools,
                extra_writable: r.extra_writable,
                context_override: r.context_override,
                resume_thread: Some(thread),
            },
        )
    }
    fn prompt(&mut self, a: AgentId, text: String) {
        prompt_agent(self.hub, self.agents, a, text, true, Send::Auto);
    }
    fn prompt_mode(&mut self, a: AgentId, text: String, mode: Send) {
        prompt_agent(self.hub, self.agents, a, text, true, mode);
    }
    fn interrupt(&mut self, a: AgentId) {
        self.hub.send(a, Cmd::Interrupt);
    }
    fn compact(&mut self, a: AgentId) {
        if let Some(ag) = self.agents.get_mut(&a) {
            ag.compacting = true;
        }
        self.hub.send(a, Cmd::Compact);
    }
    fn stop(&mut self, a: AgentId, archive: bool) {
        if archive {
            self.hub.send(a, Cmd::Archive);
        }
        self.hub.shutdown(a);
        if let Some(ag) = self.agents.get_mut(&a) {
            ag.status = Status::Stopped;
            ag.turn_active = false;
            ag.awaiting_start = false;
            ag.finished.get_or_insert(Instant::now());
            ag.activity = "archived".into();
        }
    }
    fn tool_result(&mut self, a: AgentId, req: Value, text: String, ok: bool) {
        self.hub.send(a, Cmd::Respond { id: req, result: json!({"contentItems": [{"type": "inputText", "text": text}], "success": ok}) });
    }
    fn set_effort(&mut self, a: AgentId, effort: &str) -> String {
        let Some(ag) = self.agents.get_mut(&a) else { return effort.to_string() };
        let e = self.registry.resolve(&ag.model_alias).resolve_effort(effort);
        ag.effort = e.clone();
        self.hub.send(a, Cmd::SetEffort(e.clone()));
        e
    }
    fn agent(&self, a: AgentId) -> Option<&Agent> {
        self.agents.get(&a)
    }
    fn job(&mut self, tag: JobTag, f: Box<dyn FnOnce() -> JobOut + std::marker::Send>) {
        let tx = self.tx.clone();
        tokio::task::spawn_blocking(move || {
            let out = f();
            let _ = tx.send(AppEvent::Job(tag, out));
        });
    }
    fn notify(&mut self, text: &str) {
        self.notes.push(text.to_string());
    }
}

impl App {
    pub fn new(settings: Settings, registry: Registry, project: PathBuf, hub: Hub, tx: UnboundedSender<AppEvent>, demo: bool) -> App {
        let pattern_name = settings.default_pattern.clone();
        let pattern = Pattern::load(&pattern_name, &project).unwrap_or_else(|_| Pattern::builtin());
        let sel0 = pattern.ordered_roles().first().map(|(n, _)| StudioSel::Role(n.clone())).unwrap_or(StudioSel::Settings);
        let unfinished_runs: Vec<String> = crate::engine::state::unfinished(&project).into_iter().map(|r| r.id).collect();
        let branch = std::process::Command::new("git")
            .args(["rev-parse", "--abbrev-ref", "HEAD"])
            .current_dir(&project)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_default();
        App {
            side_panel: settings.side_panel,
            settings,
            registry,
            project,
            hub,
            agents: BTreeMap::new(),
            screen: Screen::Solo,
            screen_before: None,
            overlays: vec![],
            input: Input::default(),
            solo: None,
            run: None,
            approvals: vec![],
            toast: None,
            sel: 0,
            canvas_focus: false,
            verbose: false,
            pulse_panel: true,
            quit: false,
            ctrl_c: None,
            ctrl_c_count: 0,
            tx,
            demo,
            studio: StudioState { pattern, sel: sel0, field: 0, focus: 0, errors: vec![], dirty: false, architect: None, input: Input::default(), flash: None },
            models_ui: ModelsState { row: 0, col: 0, providers: false, status: HashMap::new(), dirty: false },
            suggest: 0,
            notes: vec![],
            pattern_name,
            branch,
            unfinished_runs,
            sandbox_warning: None,
            probes: HashMap::new(),
            pulse_scroll: 0,
            force_clear: false,
            flash_screen: None,
            web: None,
        }
    }

    pub fn toast(&mut self, text: impl Into<String>, level: Level) {
        self.toast = Some((text.into(), Instant::now(), level));
    }

    pub fn animating(&self) -> bool {
        self.agents.values().any(|a| a.busy() || matches!(a.status, Status::Starting | Status::Retrying(_)))
            || self.toast.as_ref().map(|t| t.1.elapsed().as_millis() < crate::ui::toast_life(&t.0) + 100).unwrap_or(false)
            || self.agents.values().any(|a| a.compacting || a.ctx_anim.map(|(_, t)| t.elapsed().as_millis() < 950).unwrap_or(false))
            || self.run.as_ref().map(|r| (r.is_active() && !r.halted() && r.stage != Stage::Review) || r.pulse.back().map(|p| p.at.elapsed().as_millis() < 1600).unwrap_or(false)).unwrap_or(false)
            || self.studio.flash.map(|f| f.elapsed() < Duration::from_millis(900)).unwrap_or(false)
            || self.flash_screen.map(|f| f.elapsed() < Duration::from_millis(450)).unwrap_or(false)
    }

    pub(crate) fn with_run<R>(&mut self, f: impl FnOnce(&mut Run, &mut Ctxt) -> R) -> Option<R> {
        let mut run = self.run.take()?;
        let r = {
            let mut ctx = Ctxt { hub: &mut self.hub, agents: &mut self.agents, registry: &self.registry, tx: &self.tx, notes: &mut self.notes };
            f(&mut run, &mut ctx)
        };
        self.run = Some(run);
        Some(r)
    }

    pub(crate) fn in_run(&self, a: AgentId) -> bool {
        self.run.as_ref().map(|r| r.all_agents().contains(&a)).unwrap_or(false)
    }

    // ─────────────────────────── agents ───────────────────────────

    pub fn start_solo(&mut self) {
        if let Some(old) = self.solo.take() {
            self.hub.shutdown(old);
            self.agents.remove(&old);
        }
        let id = spawn_agent(
            &mut self.hub,
            &mut self.agents,
            &self.registry,
            AgentOpts {
                name: "solo".into(),
                role: "solo".into(),
                glyph: "●".into(),
                color: "saffron".into(),
                model_alias: self.settings.default_model.clone(),
                effort: None,
                cwd: self.project.clone(),
                approval: self.settings.approval_mode.clone(),
                sandbox: self.settings.sandbox.clone(),
                instructions: String::new(),
                tools: vec![],
                extra_writable: vec![],
                context_override: None,
                resume_thread: None,
            },
        );
        self.solo = Some(id);
        if let Some(why) = self.registry.alias_problem(&self.settings.default_model) {
            if let Some(a) = self.agents.get_mut(&id) {
                a.notice(Level::Warn, &why);
            }
        }
    }

    /// Switch screens, flashing the header briefly on a Stage↔Zoom jump (skipped when
    /// `reduce_motion` is on) so the overview/zoom switch never feels silent.
    pub(crate) fn set_screen(&mut self, s: Screen) {
        let jump = matches!((self.screen, s), (Screen::Stage, Screen::Zoom(_)) | (Screen::Zoom(_), Screen::Stage));
        if jump && crate::ui::theme::motion() {
            self.flash_screen = Some(Instant::now());
        }
        self.screen = s;
    }

    /// Open Studio/Models remembering the screen underneath them, so `leave_screen` can put the
    /// user back. A Studio→picker→Models hop must not record Studio as the way back (leaving
    /// would strand the user on a screen they already left), so only a returnable screen is kept.
    pub fn enter_screen(&mut self, s: Screen) {
        if !matches!(self.screen, Screen::Studio | Screen::Models) {
            self.screen_before = Some(self.screen);
        }
        self.set_screen(s);
    }

    /// Leave Studio/Models for wherever they were opened from. Two cases have nothing to return
    /// to and keep the old rule (the stage while a run exists, Solo otherwise): nothing was
    /// recorded (state restored from before this existed), or the remembered zoom is on an agent
    /// that has since gone, which would draw an empty view with no way to tell why.
    pub fn leave_screen(&mut self) {
        let back = self.screen_before.take();
        let back = back.filter(|s| match s {
            Screen::Zoom(id) => self.agents.contains_key(id),
            _ => true,
        });
        self.set_screen(back.unwrap_or(if self.run.is_some() { Screen::Stage } else { Screen::Solo }));
    }

    /// After the plan is approved: land on the animated overview with the orchestrator selected.
    pub fn land_on_overview(&mut self) {
        self.set_screen(Screen::Stage);
        self.canvas_focus = true;
        let orch = self.run.as_ref().and_then(|r| r.orchestrator);
        self.sel = self.stage_nodes().iter().position(|a| Some(*a) == orch).unwrap_or(0);
    }

    /// The agent the current view is about (Solo agent, zoomed agent, or stage selection).
    pub fn focus_agent(&self) -> Option<AgentId> {
        match self.screen {
            Screen::Solo => self.solo,
            Screen::Zoom(a) => Some(a),
            Screen::Stage => self.stage_nodes().get(self.sel).copied(),
            Screen::Studio => self.studio.architect,
            Screen::Models => None,
        }
    }

    pub fn stage_nodes(&self) -> Vec<AgentId> {
        let Some(r) = &self.run else { return vec![] };
        let mut v = vec![];
        v.extend(r.planner);
        v.extend(r.manager);
        v.extend(r.orchestrator);
        for w in &r.workers {
            if let Some(a) = w.agent {
                if !v.contains(&a) {
                    v.push(a);
                }
            }
        }
        v.extend(r.gate_agent);
        if let Some(f) = r.finale_agent {
            if !v.contains(&f) {
                v.push(f);
            }
        }
        v
    }

    pub fn set_effort(&mut self, a: AgentId, e: &str) {
        let Some(ag) = self.agents.get_mut(&a) else { return };
        let m = self.registry.resolve(&ag.model_alias);
        let e = m.resolve_effort(e);
        ag.effort = e.clone();
        self.hub.send(a, Cmd::SetEffort(e.clone()));
        let name = ag.name.clone();
        self.toast(format!("{name}: effort → {e} (next turn)"), Level::Info);
    }

    pub fn step_effort(&mut self, a: AgentId, delta: i32) {
        let Some(ag) = self.agents.get(&a) else { return };
        let m = self.registry.resolve(&ag.model_alias);
        if m.efforts().is_empty() {
            let alias = m.alias.clone();
            self.toast(format!("{alias} has no reasoning-effort setting — add efforts for it in /models if the provider supports them"), Level::Info);
            return;
        }
        let e = m.step_effort(&ag.effort, delta);
        self.set_effort(a, &e);
    }

    pub fn set_model(&mut self, a: AgentId, alias: &str) {
        let Some(ag) = self.agents.get(&a) else { return };
        let old = self.registry.resolve(&ag.model_alias);
        let new = self.registry.resolve(alias);
        if Some(a) == self.solo && old.provider != new.provider {
            self.settings.default_model = new.alias.clone();
            let _ = self.settings.save();
            self.start_solo();
            self.toast(format!("new session on {} (provider changed)", new.alias), Level::Info);
            return;
        }
        let effort = new.resolve_effort(&ag.effort);
        self.hub.send(a, Cmd::SetModel(new.model.clone()));
        self.hub.send(a, Cmd::SetEffort(effort.clone()));
        if let Some(ag) = self.agents.get_mut(&a) {
            ag.model_alias = new.alias.clone();
            ag.model = new.model.clone();
            ag.effort = effort;
            ag.ctx_window = new.context_window.or(ag.ctx_window);
            ag.notice(Level::Info, format!("model → {} ({})", new.alias, new.model));
        }
        if Some(a) == self.solo {
            self.settings.default_model = new.alias.clone();
            let _ = self.settings.save();
        }
    }

    /// After switching a run agent's model (e.g. from the halt-band `m` picker), persist the
    /// choice into this run's own pattern copy so a later spawn of that role picks it up, and —
    /// if the run was halted — resume it so the newly-configured agent gets going again.
    pub fn model_switched_for_run_agent(&mut self, a: AgentId) {
        if !self.run.as_ref().map(|r| r.all_agents().contains(&a)).unwrap_or(false) {
            return;
        }
        let alias = self.agents.get(&a).map(|x| x.model_alias.clone());
        if let (Some(run), Some(alias)) = (self.run.as_mut(), alias) {
            if let Some(role_name) = run.role_name_of(a) {
                if let Some(r) = run.pattern.roles.get_mut(&role_name) {
                    r.model = alias;
                }
            }
        }
        if self.run.as_ref().map(|r| r.halted()).unwrap_or(false) {
            self.with_run(|r, c| r.resume(c));
        }
    }

    /// Compact an agent's context now, or right after its current turn.
    pub fn compact_agent(&mut self, a: AgentId) {
        let Some(ag) = self.agents.get_mut(&a) else { return };
        if ag.compacting {
            return;
        }
        if ag.busy() {
            ag.compact_pending = true;
            let n = ag.name.clone();
            self.toast(format!("{n}: will compact when this turn ends"), Level::Info);
        } else {
            ag.compacting = true;
            ag.activity = "compacting context".into();
            self.hub.send(a, Cmd::Compact);
        }
    }

    /// WP7.4: respawn a run agent in place (stage `r` on a non-crashed node, `ctrl+r` anywhere,
    /// `/respawn`). A no-op with a toast for an agent that isn't part of the active run.
    pub fn respawn_agent(&mut self, a: AgentId) {
        if !self.in_run(a) {
            self.toast("that agent isn't part of the active run", Level::Info);
            return;
        }
        match self.with_run(|r, c| r.respawn(c, a, None)) {
            Some(Ok(())) => self.toast("respawned", Level::Info),
            Some(Err(msg)) => self.toast(msg, Level::Warn),
            None => {}
        }
    }

    /// "⚑ 2" when approvals are waiting anywhere.
    pub fn inbox_badge(&self) -> Option<String> {
        let n = self.approvals.len();
        (n > 0).then(|| format!("{} {n} waiting · ctrl+g ", crate::ui::theme::g("⚑", "!")))
    }

    /// Terminal / tmux pane title reflecting what's going on.
    pub fn title(&self) -> String {
        let busy = self.agents.values().filter(|a| a.busy()).count();
        if !self.approvals.is_empty() {
            return format!("⚑ mantra — {} waiting for you", self.approvals.len());
        }
        match (&self.run, self.screen) {
            (Some(r), Screen::Stage | Screen::Zoom(_)) => {
                let st = match &r.stage {
                    Stage::Setup | Stage::Planning => "planning".to_string(),
                    Stage::Review => "plan review".to_string(),
                    Stage::Phase { idx, .. } => format!("phase {}/{}", idx + 1, r.plan.as_ref().map(|p| p.phases.len()).unwrap_or(0)),
                    Stage::Finale { .. } => "finale".to_string(),
                    Stage::Done => "done".to_string(),
                    Stage::Failed(_) => "stopped".to_string(),
                };
                format!("{} mantra — {st}{}", if r.halted() { "⛔" } else { "◉" }, if busy > 0 { format!(" · {busy} active") } else { String::new() })
            }
            _ => format!("✦ mantra — {}{}", self.project.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default(), if busy > 0 { " · working" } else { "" }),
        }
    }

    pub fn start_run(&mut self, goal: &str) {
        if self.run.as_ref().map(|r| r.is_active()).unwrap_or(false) {
            self.toast("a run is already active — re-prompt it from the stage, or /pause", Level::Warn);
            return;
        }
        let pattern = match Pattern::load(&self.pattern_name, &self.project) {
            Ok(p) => p,
            Err(e) => {
                self.toast(format!("pattern error: {e}"), Level::Error);
                return;
            }
        };
        let problems = self.registry.preflight(&pattern);
        if !problems.is_empty() {
            // L3: fail before spending a single turn, naming the role, provider and variable.
            // Shown where the user is looking: as a toast, and in the Solo log if one exists.
            if let Some(a) = self.solo.and_then(|s| self.agents.get_mut(&s)) {
                for p in &problems {
                    a.notice(Level::Warn, &format!("cannot start the run — {p}"));
                }
            }
            self.toast(format!("cannot start: {}", problems[0]), Level::Error);
            return;
        }
        let mut run = Run::new(self.project.clone(), pattern, goal.to_string());
        {
            let mut ctx = Ctxt { hub: &mut self.hub, agents: &mut self.agents, registry: &self.registry, tx: &self.tx, notes: &mut self.notes };
            run.start(&mut ctx);
        }
        if let Some(w) = &self.sandbox_warning {
            run.log("⚠", "amber", format!("sandbox: {}", crate::util::trunc(w, 140)));
        }
        self.run = Some(run);
        // The planner isn't spawned yet (workspace setup is an async job) — zoom to it once it
        // exists, in on_event's JobTag::Setup handling below, if the user hasn't navigated away.
        self.screen = Screen::Stage;
        self.sel = 0;
    }

    /// Record a failed sandbox probe (done once at startup, never in demo mode). The welcome
    /// screens show it while they're up; every run's pulse repeats it (`start_run`) so it is on
    /// screen exactly where a `bwrap` failure would otherwise be puzzling.
    pub fn set_sandbox_warning(&mut self, why: String) {
        self.sandbox_warning = Some(why);
    }

    /// `/runs`: this project's runs, newest first. Runs of other projects are only counted — a
    /// run is resumed from its own project (`mantra runs resume <id>` switches there).
    pub fn open_runs(&mut self) {
        let all = crate::engine::state::list_all();
        let key = crate::engine::state::project_key(&self.project);
        let (list, other): (Vec<_>, Vec<_>) = all.into_iter().partition(|r| r.project_key == key);
        self.overlays.push(Overlay::Runs { sel: 0, list, confirm: false, others: other.len() });
    }

    /// Pick a saved run back up (see `engine::resume`). Refuses while another run is active.
    pub fn resume_run(&mut self, r: crate::engine::state::RunSummary) {
        if self.run.as_ref().map(|x| x.is_active()).unwrap_or(false) {
            self.toast("a run is already active — finish or /pause it first", Level::Warn);
            return;
        }
        let st = match &r.state {
            Ok(s) => s.clone(),
            Err(e) => {
                self.toast(format!("cannot resume {}: {e}", r.id), Level::Error);
                return;
            }
        };
        // The pattern the run started with (saved copy first: the user may have edited theirs).
        let pattern = std::fs::read_to_string(r.dir.join("pattern.toml"))
            .ok()
            .and_then(|s| Pattern::from_toml(&s).ok())
            .or_else(|| Pattern::load(&st.pattern, &self.project).ok())
            .unwrap_or_else(Pattern::builtin);
        let problems = self.registry.preflight(&pattern);
        if let Some(p) = problems.first() {
            self.toast(format!("cannot resume: {p}"), Level::Error);
            return;
        }
        let plan = std::fs::read_to_string(r.dir.join("plan.json")).ok().and_then(|s| serde_json::from_str::<crate::engine::plan::Plan>(&s).ok());
        let label = crate::engine::state::stage_label(&st.stage);
        let mut run = Run::from_state(&st, pattern, plan, r.dir.clone());
        {
            let mut ctx = Ctxt { hub: &mut self.hub, agents: &mut self.agents, registry: &self.registry, tx: &self.tx, notes: &mut self.notes };
            run.resume_boot(&mut ctx, &st);
        }
        self.run = Some(run);
        self.unfinished_runs.retain(|id| *id != r.id);
        self.screen = Screen::Stage;
        self.sel = 0;
        self.toast(format!("resumed {} at {label}", r.id), Level::Ok);
    }

    /// Delete a saved run (branches, worktrees, journal). The run that is open right now can
    /// only be deleted once it is finished.
    pub fn delete_run(&mut self, r: &crate::engine::state::RunSummary) -> bool {
        if let Some(cur) = &self.run {
            if cur.id == r.id {
                if cur.is_active() {
                    self.toast("this run is open and active — /pause won't do, it has to finish or fail first", Level::Warn);
                    return false;
                }
                self.run = None;
            }
        }
        let log = crate::engine::state::delete(r);
        crate::mlog!("deleted run {}: {}", r.id, log.join("; "));
        self.unfinished_runs.retain(|id| *id != r.id);
        self.toast(format!("deleted {} ({} step{})", r.id, log.len(), if log.len() == 1 { "" } else { "s" }), Level::Ok);
        true
    }

    fn start_architect(&mut self) -> AgentId {
        if let Some(a) = self.studio.architect {
            return a;
        }
        let model = if self.registry.get("astra").is_some() { "astra".to_string() } else { self.settings.default_model.clone() };
        let id = spawn_agent(
            &mut self.hub,
            &mut self.agents,
            &self.registry,
            AgentOpts {
                name: "architect".into(),
                role: "architect".into(),
                glyph: "★".into(),
                color: "saffron".into(),
                model_alias: model,
                effort: Some("high".into()),
                cwd: self.project.clone(),
                approval: "never".into(),
                sandbox: "read-only".into(),
                instructions: tools::ARCHITECT_PROMPT.into(),
                tools: tools::architect_tools(),
                extra_writable: vec![],
                context_override: None,
                resume_thread: None,
            },
        );
        self.studio.architect = Some(id);
        id
    }

    /// The `/models` `t` live test. Dispatched per backend so a provider that can't actually run a
    /// role (F4: a model rejecting Codex's `developer` role) fails here, before a real run — not the
    /// job the user is running it for.
    pub fn probe_model(&mut self, alias: &str) {
        // Every provider today speaks Codex's wire protocol (`kind = ClaudeCode` and its own test
        // path — `--append-system-prompt` — arrive with WP10). Keep the dispatch explicit so that
        // arm can be added here without touching the caller.
        self.probe_model_codex(alias);
    }

    fn probe_model_codex(&mut self, alias: &str) {
        let id = spawn_agent(
            &mut self.hub,
            &mut self.agents,
            &self.registry,
            AgentOpts {
                name: format!("probe:{alias}"),
                role: "probe".into(),
                glyph: "·".into(),
                color: "gray".into(),
                model_alias: alias.to_string(),
                effort: Some("low".into()),
                cwd: self.project.clone(),
                approval: "never".into(),
                sandbox: "read-only".into(),
                // Non-empty developer instructions become Codex's `developerInstructions`
                // (`thread/start`) — the same channel a real run's phase/task briefs use, and the
                // one some providers (F4) reject with an HTTP 400. Sending it here means the test
                // catches that before the model is picked for a role.
                instructions: "You are a compatibility probe. Developer-role messages like this one must be accepted.".into(),
                tools: vec![],
                extra_writable: vec![],
                context_override: None,
                resume_thread: None,
            },
        );
        self.probes.insert(id, (alias.to_string(), Instant::now()));
        self.models_ui.status.insert(alias.to_string(), "testing…".into());
        prompt_agent(&self.hub, &mut self.agents, id, "Reply with exactly: OK".into(), true, Send::Auto);
    }

    /// Discover models: Codex's catalog + every custom provider (`only` = one provider id).
    pub fn discover(&mut self, only: Option<String>) {
        let mut sources: Vec<String> = vec![];
        if only.is_none() {
            sources.push("codex".into());
        }
        let provs: Vec<crate::config::ProviderEntry> = self.registry.providers.iter().filter(|p| !p.id.is_empty() && p.id != "openai" && only.as_ref().map(|o| *o == p.id).unwrap_or(true)).cloned().collect();
        // ClaudeCode providers have no `/models` endpoint for a subscription; the built-in defaults
        // (config::CLAUDE_MODELS) are always known locally and answer instantly, with no loading spinner needed. A
        // `base_url` (a third-party gateway) additionally gets queried live, exactly like a Codex
        // custom provider (§10.5).
        let net_provs: Vec<crate::config::ProviderEntry> = provs.iter().filter(|p| p.kind != crate::config::ProviderKind::ClaudeCode || !p.base_url.trim().is_empty()).cloned().collect();
        sources.extend(net_provs.iter().map(|p| p.id.clone()));
        self.overlays.push(Overlay::Discover(DiscoverState { items: vec![], loading: sources, errors: vec![], sel: 0, filter: Input::default() }));
        for p in provs.iter().filter(|p| p.kind == crate::config::ProviderKind::ClaudeCode) {
            self.on_discovered(p.id.clone(), Ok(crate::discover::claude_defaults(&p.id)));
        }
        if only.is_none() {
            let cmd = self.hub.codex_cmd.clone();
            let args = self.registry.provider_args();
            let cwd = self.project.clone();
            let tx = self.tx.clone();
            tokio::spawn(async move {
                let r = async {
                    let (conn, _inc, mut child) = crate::rpc::spawn(&cmd, &args, &cwd, &[]).map_err(|e| e.to_string())?;
                    crate::rpc::handshake(&conn).await.map_err(|e| e.to_string())?;
                    let v = conn.request_timeout("model/list", json!({"limit": 100}), Duration::from_secs(20)).await.map_err(|e| e.message)?;
                    let _ = child.kill().await;
                    Ok::<_, String>(v.get("data").and_then(|d| d.as_array()).map(|a| a.iter().filter_map(crate::discover::from_codex).collect()).unwrap_or_default())
                }
                .await;
                let _ = tx.send(AppEvent::Discovered { source: "codex".into(), result: r });
            });
        }
        for p in net_provs {
            let tx = self.tx.clone();
            tokio::task::spawn_blocking(move || {
                let r = crate::discover::list_models(&p.base_url, p.resolve_key().as_deref()).map(|found| found.iter().map(|f| crate::discover::from_provider(&p.id, f)).collect());
                let _ = tx.send(AppEvent::Discovered { source: p.id.clone(), result: r });
            });
        }
    }

    fn on_discovered(&mut self, source: String, result: Result<Vec<crate::discover::Candidate>, String>) {
        let reg = &self.registry;
        let Some(Overlay::Discover(st)) = self.overlays.iter_mut().rev().find(|o| matches!(o, Overlay::Discover(_))) else { return };
        st.loading.retain(|s| *s != source);
        match result {
            Err(e) => st.errors.push(format!("{source}: {e}")),
            Ok(mut cands) => {
                for c in &mut cands {
                    crate::discover::classify(reg, c);
                }
                st.items.extend(cands);
                st.items.sort_by(|a, b| (a.state == crate::discover::CState::Configured, &a.provider, &a.model).cmp(&(b.state == crate::discover::CState::Configured, &b.provider, &b.model)));
                // Pre-select when the list is small; big catalogues (OpenRouter…) start empty — filter & pick.
                let n_new = st.items.iter().filter(|c| c.state != crate::discover::CState::Configured).count();
                for c in &mut st.items {
                    c.selected = c.state != crate::discover::CState::Configured && n_new <= 25;
                }
            }
        }
    }

    pub fn apply_discover(&mut self, st: &DiscoverState) {
        let (added, updated) = crate::discover::apply(&mut self.registry, &st.items);
        if added + updated == 0 {
            self.toast("nothing selected", Level::Info);
            return;
        }
        match self.registry.save() {
            Ok(()) => {
                self.models_ui.dirty = false;
                self.toast(format!("added {added}, updated {updated} model(s) — saved to models.toml"), Level::Ok);
            }
            Err(e) => {
                self.models_ui.dirty = true;
                self.toast(format!("added {added}, updated {updated}, but saving failed: {e}"), Level::Error);
            }
        }
    }

    // ─── approvals ───

    /// App-wide approval mode. Solo's thread is updated live; anything that still asks while the
    /// mode is "never" (e.g. mid-turn, or a run agent) is approved automatically.
    pub fn set_approval_mode(&mut self, mode: &str) {
        self.settings.approval_mode = mode.to_string();
        let _ = self.settings.save();
        if let Some(s) = self.solo {
            self.hub.send(s, Cmd::SetApproval(mode.to_string()));
            if let Some(a) = self.agents.get_mut(&s) {
                a.approval = mode.to_string();
            }
        }
        let label = match mode {
            "never" => "never ask",
            "untrusted" => "ask for anything untrusted",
            _ => "ask when needed",
        };
        let pending = self.approvals.len();
        if mode == "never" && pending > 0 {
            self.auto_approve_pending();
            self.toast(format!("approvals: {label} — {pending} waiting request(s) approved"), Level::Info);
        } else {
            self.toast(format!("approvals: {label}"), Level::Info);
        }
    }

    fn auto_approve_pending(&mut self) {
        while !self.approvals.is_empty() {
            let q = self.approvals[0].method == "item/tool/requestUserInput";
            let answer = q.then(|| "No human is available right now (approval mode: never ask). Proceed with your best judgment and state any assumptions.".to_string());
            self.resolve_approval_ex(0, if q { 0 } else { 1 }, answer, true);
        }
    }

    // ─────────────────────────── events ───────────────────────────

    pub fn on_event(&mut self, ev: AppEvent) {
        match ev {
            AppEvent::Term(e) => self.on_term(e),
            AppEvent::Hub(h) => self.on_hub(h),
            AppEvent::Job(tag, out) => {
                // A fresh run's planner is spawned inside this call (once workspace setup
                // finishes); zoom to it so the user starts in the planner's own view — but only
                // if they're still on the default overview (haven't navigated away meanwhile).
                let is_setup = matches!(tag, JobTag::Setup);
                self.with_run(|r, c| r.on_job(c, tag, out));
                if is_setup && self.screen == Screen::Stage {
                    if let Some(p) = self.run.as_ref().and_then(|r| r.planner) {
                        self.set_screen(Screen::Zoom(p));
                    }
                }
            }
            AppEvent::Discovered { source, result } => self.on_discovered(source, result),
            AppEvent::Web(i) => self.web_command(i),
        }
    }

    pub fn tick(&mut self) {
        self.with_run(|r, c| r.tick(c));
        if self.run.as_ref().map(|r| r.want_review).unwrap_or(false)
            && !self.overlays.iter().any(|o| matches!(o, Overlay::Plan { .. }))
            && matches!(self.screen, Screen::Stage | Screen::Zoom(_) | Screen::Solo)
        {
            if let Some(r) = self.run.as_mut() {
                r.want_review = false;
            }
            self.overlays.push(Overlay::Plan { scroll: 0 });
        }
        if let Some(r) = &self.run {
            let n = self.stage_nodes().len();
            if n > 0 && self.sel >= n {
                self.sel = n - 1;
            }
            let _ = r;
        }
    }

    fn on_hub(&mut self, ev: HubEvent) {
        match ev {
            HubEvent::Ready { agent, thread_id, model, resumed } => {
                let mut was_busy = false;
                if let Some(a) = self.agents.get_mut(&agent) {
                    was_busy = a.turn_active || a.awaiting_start;
                    a.thread_id = Some(thread_id);
                    if !model.is_empty() {
                        a.model = model;
                    }
                    if resumed {
                        a.turn_active = false;
                        a.awaiting_start = false;
                        a.notice(Level::Ok, "process restarted — conversation resumed");
                    }
                    if matches!(a.status, Status::Starting | Status::Crashed(_) | Status::Retrying(_)) || resumed {
                        a.status = if a.awaiting_start { Status::Busy } else { Status::Idle };
                        if !a.awaiting_start {
                            a.activity = "idle".into();
                        }
                    }
                }
                if self.in_run(agent) {
                    self.with_run(|r, c| r.on_ready(c, agent, resumed, was_busy));
                } else if resumed && was_busy {
                    if let Some(a) = self.agents.get_mut(&agent) {
                        a.notice(Level::Warn, "the turn in progress was lost in the crash — send a message to continue");
                    }
                }
            }
            HubEvent::Notif { agent, method, params } => {
                let signals = match self.agents.get_mut(&agent) {
                    Some(a) => a.apply(&method, &params),
                    None => return,
                };
                if method == "turn/started" {
                    if let Some(a) = self.agents.get_mut(&agent) {
                        for q in std::mem::take(&mut a.queued) {
                            self.hub.send(agent, Cmd::Steer { text: q });
                        }
                    }
                }
                if method == "thread/tokenUsage/updated" && (Some(agent) == self.solo || self.screen == Screen::Zoom(agent)) {
                    let mut warn = None;
                    if let Some(a) = self.agents.get_mut(&agent) {
                        let p = a.ctx_percent().unwrap_or(0);
                        if p >= 85 && !a.ctx_warned && !a.compacting {
                            a.ctx_warned = true;
                            warn = Some(format!("context {p}% full — /compact frees space (Codex also auto-compacts)"));
                        } else if p < 70 {
                            a.ctx_warned = false;
                        }
                    }
                    if let Some(w) = warn {
                        self.toast(w, Level::Warn);
                    }
                }
                if method == "turn/completed" {
                    let pending = self.agents.get_mut(&agent).map(|a| std::mem::take(&mut a.compact_pending)).unwrap_or(false);
                    if pending {
                        self.compact_agent(agent);
                    }
                    let queued = self.agents.get_mut(&agent).map(|a| std::mem::take(&mut a.queued)).unwrap_or_default();
                    if !queued.is_empty() {
                        prompt_agent(&self.hub, &mut self.agents, agent, queued.join("\n\n"), false, Send::Auto);
                    }
                }
                for s in signals {
                    match s {
                        Signal::TurnDone { status, error, kind } => self.on_turn_done(agent, &status, error, kind),
                        Signal::FilesChanged(paths) => {
                            if self.in_run(agent) {
                                self.with_run(|r, c| r.on_files_changed(c, agent, &paths));
                            }
                        }
                        Signal::Activity => {}
                        Signal::EnvironmentBroken(msg) => {
                            if self.in_run(agent) {
                                self.with_run(|r, c| r.on_environment_broken(c, agent, msg));
                            }
                        }
                    }
                }
            }
            HubEvent::Request { agent, id, method, params } => self.on_request(agent, id, &method, params),
            HubEvent::CmdFailed { agent, what, error, text } => match what {
                "steer" => {
                    if let (Some(a), Some(t)) = (self.agents.get_mut(&agent), text) {
                        a.queued.push(t);
                    }
                }
                "turn" => {
                    if let Some(a) = self.agents.get_mut(&agent) {
                        a.awaiting_start = false;
                        a.status = Status::Failed(error.clone());
                        a.notice(Level::Error, format!("couldn't start the turn: {error}"));
                    }
                    if self.in_run(agent) {
                        self.with_run(|r, c| r.on_turn_done(c, agent, "failed", Some(error), Some(crate::agent::ErrKind::Transient)));
                    }
                }
                "settings" => crate::mlog!("agent {agent}: thread/settings/update not supported ({error}); new policy applies from the next turn"),
                other => {
                    if let Some(a) = self.agents.get_mut(&agent) {
                        if other == "compact" {
                            a.compacting = false;
                        }
                        a.notice(Level::Warn, format!("{other} failed: {error}"));
                    }
                }
            },
            HubEvent::Crashed { agent, reason, restarting, attempt } => {
                if let Some(a) = self.agents.get_mut(&agent) {
                    a.status = if restarting { Status::Retrying(format!("restarting ({attempt})")) } else { Status::Crashed(reason.clone()) };
                    a.activity = if restarting { "restarting".into() } else { "crashed".into() };
                    let what = if a.backend == crate::config::ProviderKind::ClaudeCode { "claude process" } else { "codex process" };
                    a.notice(Level::Error, format!("{what} {}: {reason}", if restarting { "crashed — restarting" } else { "keeps crashing — press r (stage) or /new" }));
                }
                if self.in_run(agent) {
                    self.with_run(|r, c| r.on_crash(c, agent, &reason, restarting));
                }
                if Some(agent) == self.solo && !restarting {
                    self.toast("Solo agent is down — check the CLI is installed & logged in / the provider key (see log), /new to retry", Level::Error);
                }
            }
            HubEvent::Exited { agent } => {
                if let Some(a) = self.agents.get_mut(&agent) {
                    a.status = Status::Stopped;
                    a.turn_active = false;
                }
            }
            HubEvent::BridgeConn { agent, stream } => {
                // Routing by agent id is `Hub::send`'s job already (WP10.4) — a bridge connection
                // for an agent that's since exited is just dropped, same as any other stale `Cmd`.
                self.hub.send(agent, Cmd::Bridge(stream));
            }
        }
    }

    fn on_turn_done(&mut self, agent: AgentId, status: &str, error: Option<String>, kind: Option<crate::agent::ErrKind>) {
        if let Some((alias, t0)) = self.probes.remove(&agent) {
            let msg = match (status, &error) {
                ("completed", _) => {
                    let reply = self.agents.get(&agent).and_then(|a| a.final_message.clone()).unwrap_or_default();
                    format!("ok {:.1}s · \"{}\"", t0.elapsed().as_secs_f32(), crate::util::trunc(reply.trim(), 16))
                }
                (_, Some(e)) => format!("error: {}", crate::util::trunc(e, 40)),
                _ => status.to_string(),
            };
            self.models_ui.status.insert(alias, msg);
            self.hub.shutdown(agent);
            self.agents.remove(&agent);
            return;
        }
        if self.in_run(agent) {
            let st = status.to_string();
            self.with_run(|r, c| r.on_turn_done(c, agent, &st, error, kind));
        }
        if Some(agent) == self.solo {
            let long = self.agents.get(&agent).and_then(|a| a.turn_started).map(|t| t.elapsed() > Duration::from_secs(30)).unwrap_or(false);
            if long {
                self.notes.push(format!("Mantra: Solo turn {status}"));
            }
        }
    }

    fn on_request(&mut self, agent: AgentId, id: Value, method: &str, params: Value) {
        match method {
            "item/tool/call" => {
                let tool = params.get("tool").and_then(|t| t.as_str()).unwrap_or("").to_string();
                let args = params.get("arguments").cloned().unwrap_or(Value::Null);
                if Some(agent) == self.studio.architect {
                    let (text, ok) = self.architect_tool(&tool, &args);
                    self.hub.send(agent, Cmd::Respond { id, result: json!({"contentItems": [{"type": "inputText", "text": text}], "success": ok}) });
                } else if self.in_run(agent) {
                    self.with_run(|r, c| r.on_tool_call(c, agent, id, &tool, &args));
                } else {
                    self.hub.send(agent, Cmd::Respond { id, result: json!({"contentItems": [{"type": "inputText", "text": "tool not available"}], "success": false}) });
                }
            }
            "mcpServer/elicitation/request" => {
                self.hub.send(agent, Cmd::Respond { id, result: json!({"action": "decline", "content": null, "_meta": null}) });
                if let Some(a) = self.agents.get_mut(&agent) {
                    a.notice(Level::Warn, "declined an MCP elicitation request (not supported in Mantra yet)");
                }
            }
            "item/commandExecution/requestApproval" | "execCommandApproval" | "item/fileChange/requestApproval" | "applyPatchApproval" | "item/permissions/requestApproval" | "item/tool/requestUserInput" => {
                let (title, detail) = match method {
                    "item/commandExecution/requestApproval" | "execCommandApproval" => {
                        let cmd = params.get("command").map(|c| match c {
                            Value::Array(a) => a.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>().join(" "),
                            v => v.as_str().unwrap_or("").to_string(),
                        }).unwrap_or_default();
                        ("Run this command?".to_string(), format!("$ {cmd}{}", params.get("reason").and_then(|r| r.as_str()).map(|r| format!("\n  reason: {r}")).unwrap_or_default()))
                    }
                    "item/fileChange/requestApproval" | "applyPatchApproval" => ("Apply these file changes?".into(), params.get("reason").and_then(|r| r.as_str()).unwrap_or("the agent wants to write files").to_string()),
                    "item/permissions/requestApproval" => ("Grant extra permissions?".into(), format!("{}", crate::util::trunc(&params.get("permissions").map(|p| p.to_string()).unwrap_or_default(), 200))),
                    _ => {
                        let q = params.pointer("/questions/0/question").and_then(|q| q.as_str()).unwrap_or("The agent has a question");
                        ("Agent question (type your answer, ⏎ to send)".into(), q.to_string())
                    }
                };
                let name = self.agents.get(&agent).map(|a| a.name.clone()).unwrap_or_default();
                if Some(agent) != self.solo && Some(self.screen) != Some(Screen::Zoom(agent)) {
                    self.toast(format!("{name} needs approval — ctrl+g"), Level::Warn);
                    self.notes.push(format!("Mantra: {name} needs approval"));
                }
                self.approvals.push(Approval { agent, id, method: method.to_string(), title, detail, params, at: Instant::now() });
                // Codex fixes a turn's policy when the turn starts, so a turn begun under another
                // mode can still ask — honour the *agent's own* approval policy here (not the
                // global Solo mode), so a Mandala role set to "never" auto-resolves even while
                // Solo's mode is something else, and vice versa.
                if self.agents.get(&agent).map(|a| a.approval == "never").unwrap_or(false) {
                    self.auto_approve_pending();
                    self.toast = self.toast.take().filter(|t| !t.0.contains("needs approval"));
                    self.notes.retain(|n| !n.contains("needs approval"));
                }
            }
            _ => {
                self.hub.send(agent, Cmd::RespondErr { id, message: format!("{method} is not supported by Mantra") });
            }
        }
    }

    /// decision: 0 = yes once, 1 = yes for session, 2 = no, 3 = cancel turn
    pub fn resolve_approval(&mut self, idx: usize, decision: u8, answer: Option<String>) {
        self.resolve_approval_ex(idx, decision, answer, false)
    }

    pub fn resolve_approval_ex(&mut self, idx: usize, decision: u8, answer: Option<String>, auto: bool) {
        if idx >= self.approvals.len() {
            return;
        }
        let ap = self.approvals.remove(idx);
        let result = match ap.method.as_str() {
            "execCommandApproval" | "applyPatchApproval" => json!({"decision": match decision { 0 => json!("approved"), 1 => json!("approved_for_session"), 3 => json!("abort"), _ => json!({"denied": {"rejection": "declined by the user"}}) }}),
            "item/permissions/requestApproval" => {
                if decision <= 1 {
                    json!({"permissions": ap.params.get("permissions").cloned().unwrap_or(json!({})), "scope": if decision == 1 { "session" } else { "turn" }})
                } else {
                    json!({"permissions": {}, "scope": "turn"})
                }
            }
            "item/tool/requestUserInput" => {
                let mut answers = serde_json::Map::new();
                if let Some(qs) = ap.params.get("questions").and_then(|q| q.as_array()) {
                    for (i, q) in qs.iter().enumerate() {
                        let qid = q.get("id").and_then(|x| x.as_str()).unwrap_or("").to_string();
                        let a = if i == 0 { answer.clone().unwrap_or_default() } else { String::new() };
                        answers.insert(qid, json!({"answers": if a.is_empty() { vec![] } else { vec![a] }}));
                    }
                }
                json!({"answers": answers})
            }
            _ => {
                let d = ["accept", "acceptForSession", "decline", "cancel"][decision.min(3) as usize];
                json!({ "decision": d })
            }
        };
        self.hub.send(ap.agent, Cmd::Respond { id: ap.id, result });
        if let Some(a) = self.agents.get_mut(&ap.agent) {
            let verb = if auto { "auto-approved (never ask)" } else { ["approved", "approved for this session", "declined", "cancelled"][decision.min(3) as usize] };
            a.notice(if decision <= 1 { Level::Ok } else { Level::Warn }, format!("{verb}: {}", crate::util::trunc(ap.detail.lines().next().unwrap_or(""), 80)));
        }
    }

    pub fn approval_for(&self, agent: Option<AgentId>) -> Option<usize> {
        let a = agent?;
        self.approvals.iter().position(|x| x.agent == a)
    }

    fn architect_tool(&mut self, tool: &str, args: &Value) -> (String, bool) {
        match tool {
            "mantra_read_pattern" => (self.studio.pattern.to_toml(), true),
            "mantra_write_pattern" => {
                let t = args.get("toml").and_then(|t| t.as_str()).unwrap_or("");
                match Pattern::from_toml(t) {
                    Ok(p) => {
                        self.studio.pattern = p;
                        self.studio.dirty = true;
                        self.studio.errors.clear();
                        self.studio.flash = Some(Instant::now());
                        ("OK — pattern updated live in the Studio (the user saves it with ctrl+s)".into(), true)
                    }
                    Err(e) => (format!("INVALID: {e}"), false),
                }
            }
            _ => ("unknown tool".into(), false),
        }
    }

    // ─────────────────────────── input ───────────────────────────

    fn on_term(&mut self, e: Event) {
        match e {
            Event::Key(k) if k.kind != KeyEventKind::Release => self.on_key(k),
            Event::Paste(s) => {
                if let Some(Overlay::Edit { input, .. }) = self.overlays.last_mut() {
                    input.insert_str(&s);
                } else if self.screen == Screen::Studio && self.studio.focus == 2 {
                    self.studio.input.insert_str(&s);
                } else {
                    self.input.insert_str(&s);
                }
            }
            Event::Mouse(m) => {
                let delta = match m.kind {
                    MouseEventKind::ScrollUp => 3i32,
                    MouseEventKind::ScrollDown => -3,
                    _ => 0,
                };
                if delta != 0 {
                    self.scroll(delta);
                }
            }
            _ => {}
        }
    }

    fn scroll(&mut self, delta: i32) {
        match self.overlays.last_mut() {
            Some(Overlay::Diff { scroll, .. }) | Some(Overlay::Plan { scroll }) => {
                *scroll = (*scroll as i32 - delta).max(0) as usize;
                return;
            }
            _ => {}
        }
        if self.screen == Screen::Stage {
            self.pulse_scroll = (self.pulse_scroll as i32 + delta).max(0) as usize;
            return;
        }
        if let Some(a) = self.focus_agent().and_then(|a| self.agents.get_mut(&a)) {
            a.scroll = (a.scroll as i32 + delta).max(0) as usize;
            a.follow = a.scroll == 0;
        }
    }

    fn on_key(&mut self, k: KeyEvent) {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let alt = k.modifiers.contains(KeyModifiers::ALT);
        // Global keys
        if ctrl && k.code == KeyCode::Char('c') {
            self.ctrl_c_press();
            return;
        }
        if ctrl && k.code == KeyCode::Char('o') {
            self.overlays.clear();
            self.screen = match self.screen {
                Screen::Solo => Screen::Stage,
                _ => Screen::Solo,
            };
            return;
        }
        if ctrl && k.code == KeyCode::Char('e') && self.overlays.is_empty() {
            self.verbose = !self.verbose;
            self.toast(if self.verbose { "verbose log on" } else { "verbose log off" }, Level::Info);
            return;
        }
        if ctrl && k.code == KeyCode::Char('l') {
            self.force_clear = true;
            return;
        }
        if (ctrl && k.code == KeyCode::Char('t')) || k.code == KeyCode::F(2) {
            if self.screen == Screen::Stage {
                self.pulse_panel = !self.pulse_panel;
            } else {
                self.side_panel = !self.side_panel;
            }
            return;
        }
        if ctrl && k.code == KeyCode::Char('g') {
            self.overlays.push(Overlay::Inbox { sel: 0 });
            return;
        }
        if ctrl && k.code == KeyCode::Char('k') && self.overlays.is_empty() {
            let target = self.focus_agent();
            let cur = target.and_then(|a| self.agents.get(&a)).map(|a| a.model_alias.clone()).unwrap_or_default();
            let sel = self.registry.models.iter().position(|m| m.alias == cur).unwrap_or(0);
            self.overlays.push(Overlay::ModelPicker { sel, target });
            return;
        }
        if ctrl && k.code == KeyCode::Char('d') && self.overlays.is_empty() {
            if let Some(a) = self.focus_agent() {
                self.overlays.push(Overlay::Diff { agent: a, file: 0, scroll: 0 });
            }
            return;
        }
        // ctrl+f / ctrl+x are unconditional (checked before overlays dispatch) so they reach the
        // chat input even while the plan-review overlay is open on top of a zoomed agent — see
        // the `!ctrl` guard on that overlay's own 'f' arm in overlays.rs.
        if ctrl && k.code == KeyCode::Char('f') {
            self.force_send();
            return;
        }
        if ctrl && k.code == KeyCode::Char('x') {
            self.discard_queue();
            return;
        }
        // WP7.4: respawn the focused run agent in place, from anywhere (not just the stage nav 'r').
        if ctrl && k.code == KeyCode::Char('r') && self.overlays.is_empty() {
            if let Some(a) = self.focus_agent() {
                self.respawn_agent(a);
            }
            return;
        }
        if k.code == KeyCode::F(1) {
            self.overlays.push(Overlay::Help);
            return;
        }
        if matches!(k.code, KeyCode::PageUp | KeyCode::PageDown) && !matches!(self.overlays.last(), Some(Overlay::Diff { .. }) | Some(Overlay::Plan { .. })) {
            self.scroll(if k.code == KeyCode::PageUp { 15 } else { -15 });
            return;
        }
        if !self.overlays.is_empty() {
            crate::ui::overlays::key(self, k);
            return;
        }
        // Approval mode (shift+tab) works even while an approval card is showing —
        // that's exactly when you want to flip to "never ask".
        if k.code == KeyCode::BackTab && matches!(self.screen, Screen::Solo | Screen::Stage | Screen::Zoom(_)) {
            let modes = ["untrusted", "on-request", "never"];
            let i = modes.iter().position(|m| *m == self.settings.approval_mode).unwrap_or(1);
            let next = modes[(i + 1) % 3];
            self.set_approval_mode(next);
            return;
        }
        // Approval card for the focused agent
        if let Some(idx) = self.approval_for(self.focus_agent()) {
            if matches!(self.screen, Screen::Solo | Screen::Zoom(_)) {
                let is_question = self.approvals[idx].method == "item/tool/requestUserInput";
                if is_question {
                    if k.code == KeyCode::Enter {
                        let ans = self.input.take();
                        self.resolve_approval(idx, 0, Some(ans));
                        return;
                    }
                    if k.code == KeyCode::Esc {
                        self.resolve_approval(idx, 2, None);
                        return;
                    }
                } else {
                    match k.code {
                        KeyCode::Char('y') | KeyCode::Enter => return self.resolve_approval(idx, 0, None),
                        KeyCode::Char('a') => return self.resolve_approval(idx, 1, None),
                        KeyCode::Char('n') => return self.resolve_approval(idx, 2, None),
                        KeyCode::Esc => return self.resolve_approval(idx, 3, None),
                        _ => return,
                    }
                }
            }
        }
        match self.screen {
            Screen::Studio => return crate::ui::studio::studio_key(self, k),
            Screen::Models => return crate::ui::studio::models_key(self, k),
            _ => {}
        }
        // effort / approvals shortcuts
        if (alt || k.modifiers.contains(KeyModifiers::SHIFT)) && matches!(k.code, KeyCode::Up | KeyCode::Down) && !(self.screen == Screen::Stage && self.canvas_focus) {
            if let Some(a) = self.focus_agent() {
                self.step_effort(a, if k.code == KeyCode::Up { 1 } else { -1 });
            }
            return;
        }
        if self.screen == Screen::Stage {
            if self.stage_key(k) {
                return;
            }
        }
        // Backspace on an empty input pops the last queued message back into the input to edit.
        if k.code == KeyCode::Backspace && self.input.is_empty() {
            if let Some(a) = self.focus_agent() {
                if let Some(text) = self.agents.get_mut(&a).and_then(|ag| ag.queued.pop()) {
                    self.input.set(&text);
                    return;
                }
            }
        }
        // slash suggestions navigation
        let sugg = self.suggestions();
        if !sugg.is_empty() {
            match k.code {
                KeyCode::Up => {
                    self.suggest = self.suggest.saturating_sub(1);
                    return;
                }
                KeyCode::Down => {
                    self.suggest = (self.suggest + 1).min(sugg.len() - 1);
                    return;
                }
                KeyCode::Tab => {
                    let c = sugg[self.suggest.min(sugg.len() - 1)].0;
                    self.input.set(&format!("{c} "));
                    return;
                }
                KeyCode::Enter => {
                    let c = sugg[self.suggest.min(sugg.len() - 1)].0;
                    if self.input.text().trim() != c {
                        self.input.set(c);
                    }
                }
                _ => {}
            }
        }
        // esc only navigates: back to the overview from a zoom, otherwise clear the input.
        // Interrupting is ctrl+c's job (it used to be both, and the two hints collided).
        if k.code == KeyCode::Esc {
            match self.screen {
                Screen::Zoom(_) if self.input.is_empty() => {
                    self.set_screen(Screen::Stage);
                }
                _ => {
                    self.input.clear();
                    if self.screen == Screen::Stage {
                        self.canvas_focus = true;
                    }
                }
            }
            return;
        }
        if k.code == KeyCode::Char('?') && self.input.is_empty() {
            self.overlays.push(Overlay::Help);
            return;
        }
        match self.input.key(k) {
            Act::Submit => {
                let text = self.input.take();
                self.suggest = 0;
                self.submit(text.trim_end().to_string());
            }
            Act::Changed => self.suggest = 0,
            _ => {}
        }
    }

    /// Stage keys. Returns true if consumed.
    fn stage_key(&mut self, k: KeyEvent) -> bool {
        let nodes = self.stage_nodes();
        let alt = k.modifiers.contains(KeyModifiers::ALT);
        if k.code == KeyCode::Tab && self.suggestions().is_empty() {
            self.canvas_focus = !self.canvas_focus;
            return true;
        }
        let nav = self.canvas_focus || alt;
        if nav {
            match k.code {
                KeyCode::Left | KeyCode::Up | KeyCode::Char('h') | KeyCode::Char('k') => {
                    self.sel = self.sel.saturating_sub(1);
                    return true;
                }
                KeyCode::Right | KeyCode::Down | KeyCode::Char('l') | KeyCode::Char('j') => {
                    if !nodes.is_empty() {
                        self.sel = (self.sel + 1).min(nodes.len() - 1);
                    }
                    return true;
                }
                _ => {}
            }
        }
        if k.code == KeyCode::Enter && (self.canvas_focus || self.input.is_empty()) {
            if let Some(a) = nodes.get(self.sel) {
                self.set_screen(Screen::Zoom(*a));
                self.canvas_focus = false;
            }
            return true;
        }
        if !self.canvas_focus {
            return false;
        }
        let selected = nodes.get(self.sel).copied();
        match k.code {
            KeyCode::Char(' ') => {
                self.with_run(|r, c| r.toggle_pause(c));
            }
            KeyCode::Char('p') => self.overlays.push(Overlay::Plan { scroll: 0 }),
            KeyCode::Char('a') => {
                if self.run.as_ref().map(|r| r.stage == Stage::Review).unwrap_or(false) {
                    self.with_run(|r, c| r.approve_plan(c));
                    self.land_on_overview();
                }
            }
            KeyCode::Char('i') => self.overlays.push(Overlay::Inbox { sel: 0 }),
            KeyCode::Char('x') => {
                if let Some(a) = selected {
                    self.interrupt_by_user(a);
                }
            }
            KeyCode::Char('r') => {
                if let Some(a) = selected {
                    let crashed = self.agents.get(&a).map(|x| matches!(x.status, Status::Crashed(_))).unwrap_or(false);
                    if crashed {
                        self.hub.send(a, Cmd::Restart);
                    } else if let Some(res) = self.with_run(|r, c| r.respawn(c, a, None)) {
                        match res {
                            Ok(()) => self.toast("respawned", Level::Info),
                            Err(msg) => self.toast(msg, Level::Warn),
                        }
                    }
                }
            }
            KeyCode::Char('+') | KeyCode::Char('=') => {
                if let Some(a) = selected {
                    self.step_effort(a, 1);
                }
            }
            KeyCode::Char('-') => {
                if let Some(a) = selected {
                    self.step_effort(a, -1);
                }
            }
            KeyCode::Char('d') => {
                if let Some(a) = selected {
                    self.overlays.push(Overlay::Diff { agent: a, file: 0, scroll: 0 });
                }
            }
            KeyCode::Char('c') => {
                if let Some(a) = selected {
                    self.compact_agent(a);
                }
            }
            KeyCode::Char('m') => {
                // On a halted run this is the fix for `ProviderRejected`: pick a different model
                // for the affected agent (WP6.6). Works on any selected agent, halted or not.
                if let Some(a) = selected {
                    let cur = self.agents.get(&a).map(|x| x.model_alias.clone()).unwrap_or_default();
                    let sel = self.registry.models.iter().position(|m| m.alias == cur).unwrap_or(0);
                    self.overlays.push(Overlay::ModelPicker { sel, target: Some(a) });
                }
            }
            KeyCode::Char('s') => self.open_studio(),
            KeyCode::Char('?') => self.overlays.push(Overlay::Help),
            KeyCode::Char('/') | KeyCode::Char('@') => {
                self.canvas_focus = false;
                if let KeyCode::Char(c) = k.code {
                    self.input.insert_str(&c.to_string());
                }
            }
            KeyCode::Esc => {}
            KeyCode::Char(c @ '1'..='9') => {
                // 1-9 jump: select the nth node and zoom straight in.
                let idx = (c as usize) - ('1' as usize);
                if let Some(a) = nodes.get(idx) {
                    self.sel = idx;
                    self.set_screen(Screen::Zoom(*a));
                    self.canvas_focus = false;
                }
            }
            KeyCode::Char(c) if c.is_alphanumeric() => {
                // start typing
                self.canvas_focus = false;
                self.input.insert_str(&c.to_string());
            }
            _ => return false,
        }
        true
    }

    pub fn suggestions(&self) -> Vec<(&'static str, &'static str)> {
        let t = self.input.text();
        if !t.starts_with('/') || t.contains(' ') || t.contains('\n') {
            return vec![];
        }
        COMMANDS.iter().filter(|(c, _)| c.starts_with(t.as_str())).copied().collect()
    }

    pub fn open_studio(&mut self) {
        if !self.studio.dirty {
            if let Ok(p) = Pattern::load(&self.pattern_name, &self.project) {
                self.studio.pattern = p;
            }
        }
        self.enter_screen(Screen::Studio);
    }

    /// ctrl+c: one press interrupts (or closes an overlay / clears the input), three presses in
    /// a row quit — whatever state the app is in, so there is always a way out.
    fn ctrl_c_press(&mut self) {
        let again = self.ctrl_c.map(|t| t.elapsed() < Duration::from_secs(2)).unwrap_or(false);
        self.ctrl_c_count = if again { self.ctrl_c_count.saturating_add(1) } else { 1 };
        self.ctrl_c = Some(Instant::now());
        if self.ctrl_c_count >= 3 {
            self.quit = true;
            return;
        }
        let more = if self.ctrl_c_count == 1 { "ctrl+c ×2 more quits" } else { "ctrl+c once more quits" };
        if !self.overlays.is_empty() {
            self.overlays.pop();
            return;
        }
        if !self.input.is_empty() {
            self.input.clear();
            return;
        }
        if let Some(a) = self.focus_agent() {
            if self.interrupt_by_user(a) {
                self.toast(format!("stopping… it stays stopped ({more})"), Level::Warn);
                return;
            }
        }
        self.toast(more, Level::Info);
    }

    /// Interrupt an agent because the user asked for it (ctrl+c, `x`). Unlike an engine-side
    /// interrupt (a halt, `mantra_interrupt`), this one *sticks*: `Agent::stopped_by_user` tells
    /// the engine to stand down — no planner nudge, no watchdog, no retry, no resume prompt — until
    /// someone messages the agent again. Returns whether there was a live turn to stop.
    pub(crate) fn interrupt_by_user(&mut self, a: AgentId) -> bool {
        let Some(ag) = self.agents.get_mut(&a) else { return false };
        if !ag.busy() {
            return false;
        }
        ag.stopped_by_user = true;
        // A queued message would be delivered the instant the turn ends and start the agent right
        // back up — the opposite of what the user just pressed.
        ag.queued.clear();
        ag.notice(Level::Warn, "stopped by you — type to continue, or r to respawn");
        self.hub.send(a, Cmd::Interrupt);
        true
    }

    pub fn submit(&mut self, text: String) {
        if text.trim().is_empty() {
            return;
        }
        if text.starts_with('/') {
            return self.command(&text);
        }
        match self.screen {
            Screen::Solo => {
                if self.solo.is_none() {
                    self.start_solo();
                }
                let Some(s) = self.solo else { return };
                if let Some(cmd) = text.strip_prefix('!') {
                    if let Some(a) = self.agents.get_mut(&s) {
                        a.push_user(&text);
                    }
                    self.hub.send(s, Cmd::Shell { command: cmd.trim().to_string() });
                    return;
                }
                prompt_agent(&self.hub, &mut self.agents, s, text, true, Send::Queue);
            }
            Screen::Zoom(a) => {
                if self.in_run(a) {
                    let name = self.run.as_ref().map(|r| r.name_of(a)).unwrap_or_default();
                    let t = text.clone();
                    self.with_run(|r, c| r.direct(c, &name, &t, Send::Queue));
                } else {
                    prompt_agent(&self.hub, &mut self.agents, a, text, true, Send::Queue);
                }
            }
            Screen::Stage => {
                if self.run.is_none() || !self.run.as_ref().map(|r| r.is_active()).unwrap_or(false) {
                    self.start_run(&text);
                    return;
                }
                if let Some(rest) = text.strip_prefix('@') {
                    let (name, msg) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
                    let (name, msg) = (name.to_string(), msg.trim().to_string());
                    let ok = self.with_run(|r, c| r.direct(c, &name, &msg, Send::Queue)).unwrap_or(false);
                    if !ok {
                        self.toast(format!("no agent named @{name}"), Level::Warn);
                    }
                    return;
                }
                self.with_run(|r, c| r.user_input(c, &text));
            }
            _ => {}
        }
    }

    /// ctrl+f: deliver the focused agent's queued messages plus the current input into its
    /// running turn right away. Applies to Solo, Zoom and `@name` messages from the stage — the
    /// same places Enter can queue. Against an idle agent this just behaves like Enter.
    fn force_send(&mut self) {
        match self.screen {
            Screen::Solo => {
                if let Some(s) = self.solo {
                    self.force_prompt(s);
                }
            }
            Screen::Zoom(a) => {
                if self.in_run(a) {
                    let text = self.input.text().trim_end().to_string();
                    let empty_queue = self.agents.get(&a).map(|x| x.queued.is_empty()).unwrap_or(true);
                    if text.trim().is_empty() && empty_queue {
                        return;
                    }
                    if !text.trim().is_empty() {
                        self.input.take();
                    }
                    let name = self.run.as_ref().map(|r| r.name_of(a)).unwrap_or_default();
                    self.with_run(|r, c| r.direct(c, &name, &text, Send::Force));
                } else {
                    self.force_prompt(a);
                }
            }
            Screen::Stage => {
                let raw = self.input.text();
                if let Some(rest) = raw.strip_prefix('@') {
                    let (name, msg) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
                    let (name, msg) = (name.to_string(), msg.trim().to_string());
                    self.input.take();
                    let ok = self.with_run(|r, c| r.direct(c, &name, &msg, Send::Force)).unwrap_or(false);
                    if !ok {
                        self.toast(format!("no agent named @{name}"), Level::Warn);
                    }
                }
            }
            _ => {}
        }
    }

    pub(crate) fn force_prompt(&mut self, id: AgentId) {
        let text = self.input.text().trim_end().to_string();
        let empty_queue = self.agents.get(&id).map(|a| a.queued.is_empty()).unwrap_or(true);
        if text.trim().is_empty() && empty_queue {
            return; // nothing queued and nothing typed — there's nothing to force
        }
        let echo = !text.trim().is_empty();
        if echo {
            self.input.take();
        }
        prompt_agent(&self.hub, &mut self.agents, id, text, echo, Send::Force);
    }

    /// ctrl+x on an empty input: discard the focused agent's whole queue.
    pub(crate) fn discard_queue(&mut self) {
        if !self.input.is_empty() {
            return;
        }
        if let Some(a) = self.focus_agent() {
            if let Some(ag) = self.agents.get_mut(&a) {
                if !ag.queued.is_empty() {
                    ag.queued.clear();
                    self.toast("queue cleared", Level::Info);
                }
            }
        }
    }

    pub(crate) fn command(&mut self, line: &str) {
        let (cmd, arg) = line.split_once(char::is_whitespace).map(|(a, b)| (a, b.trim())).unwrap_or((line, ""));
        let target = self.focus_agent();
        match cmd {
            "/quit" | "/exit" | "/q" => self.quit = true,
            "/help" => self.overlays.push(Overlay::Help),
            "/model" => {
                if arg.is_empty() {
                    let cur = target.and_then(|a| self.agents.get(&a)).map(|a| a.model_alias.clone()).unwrap_or_default();
                    let sel = self.registry.models.iter().position(|m| m.alias == cur).unwrap_or(0);
                    self.overlays.push(Overlay::ModelPicker { sel, target });
                } else if let Some(a) = target {
                    self.set_model(a, arg);
                }
            }
            "/effort" => match target {
                Some(a) if !arg.is_empty() => self.set_effort(a, arg),
                _ => self.toast("usage: /effort low|medium|high|xhigh|max", Level::Info),
            },
            "/approvals" => {
                if ["untrusted", "on-request", "never"].contains(&arg) {
                    self.set_approval_mode(arg);
                } else {
                    self.toast("usage: /approvals untrusted|on-request|never", Level::Info);
                }
            }
            "/new" | "/clear" => {
                self.start_solo();
                self.screen = Screen::Solo;
                self.toast("fresh session", Level::Ok);
            }
            "/compact" => {
                if let Some(a) = target {
                    self.compact_agent(a);
                }
            }
            "/diff" => {
                if let Some(a) = target {
                    self.overlays.push(Overlay::Diff { agent: a, file: 0, scroll: 0 });
                }
            }
            "/mandala" | "/stage" => self.screen = Screen::Stage,
            "/solo" => self.screen = Screen::Solo,
            "/run" => {
                if arg.is_empty() {
                    self.screen = Screen::Stage;
                    self.toast("describe what to build in the stage prompt", Level::Info);
                } else {
                    self.start_run(arg);
                }
            }
            "/pattern" => {
                if arg.is_empty() {
                    let list = Pattern::list(&self.project);
                    let sel = list.iter().position(|p| *p == self.pattern_name).unwrap_or(0);
                    self.overlays.push(Overlay::Patterns { sel, list });
                } else {
                    self.pattern_name = arg.to_string();
                    self.toast(format!("pattern for new runs: {arg}"), Level::Info);
                }
            }
            "/runs" => self.open_runs(),
            "/plan" => self.overlays.push(Overlay::Plan { scroll: 0 }),
            "/pause" => {
                self.with_run(|r, c| r.toggle_pause(c));
            }
            "/respawn" => match target {
                Some(a) => self.respawn_agent(a),
                None => self.toast("no agent focused", Level::Info),
            },
            "/land" => {
                if self.run.as_ref().map(|r| r.stage == Stage::Done).unwrap_or(false) {
                    self.with_run(|r, c| r.land(c));
                } else {
                    self.toast("no finished run to land", Level::Warn);
                }
            }
            "/studio" => self.open_studio(),
            "/models" => self.enter_screen(Screen::Models),
            "/inbox" => self.overlays.push(Overlay::Inbox { sel: 0 }),
            "/web" => self.overlays.push(Overlay::Web),
            "/remote" => self.overlays.push(Overlay::Remote),
            "/verbose" => self.verbose = !self.verbose,
            _ => self.toast(format!("unknown command {cmd} — /help"), Level::Warn),
        }
    }

    pub fn studio_architect_send(&mut self, text: String) {
        let a = self.start_architect();
        prompt_agent(&self.hub, &mut self.agents, a, text, true, Send::Auto);
    }

    pub fn shutdown(&mut self) {
        self.hub.shutdown_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn busy_agent(turn_active: bool, awaiting_start: bool) -> (BTreeMap<AgentId, Agent>, AgentId) {
        let mut agents = BTreeMap::new();
        let id: AgentId = 1;
        let mut a = Agent::new(id, "worker", "worker", PathBuf::new());
        a.status = Status::Busy;
        a.turn_active = turn_active;
        a.awaiting_start = awaiting_start;
        agents.insert(id, a);
        (agents, id)
    }

    fn test_hub_with(id: AgentId) -> (Hub, tokio::sync::mpsc::UnboundedReceiver<Cmd>) {
        let (ev_tx, _ev_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut hub = Hub::new(vec![], vec![], ev_tx, false);
        let rx = hub.test_register(id);
        (hub, rx)
    }

    #[test]
    fn enter_queues_behind_a_busy_turn_without_steering() {
        let (mut agents, id) = busy_agent(true, false);
        let (hub, mut rx) = test_hub_with(id);
        prompt_agent(&hub, &mut agents, id, "use axum instead".into(), true, Send::Queue);
        assert_eq!(agents[&id].queued.len(), 1);
        assert!(rx.try_recv().is_err(), "Queue must not steer a busy turn");
    }

    #[test]
    fn force_send_joins_the_queue_and_the_new_message_into_one_steer() {
        let (mut agents, id) = busy_agent(true, false);
        let (hub, mut rx) = test_hub_with(id);
        agents.get_mut(&id).unwrap().queued.push("first queued".into());
        prompt_agent(&hub, &mut agents, id, "and now this".into(), true, Send::Force);
        assert!(agents[&id].queued.is_empty(), "Force must drain the queue");
        match rx.try_recv() {
            Ok(Cmd::Steer { text }) => assert_eq!(text, "first queued\n\nand now this"),
            other => panic!("expected exactly one Cmd::Steer, got {other:?}"),
        }
        assert!(rx.try_recv().is_err(), "exactly one Cmd::Steer, not one per message");
    }

    #[test]
    fn force_send_on_a_starting_turn_defers_to_the_queue() {
        // awaiting_start (turn hasn't produced its first event yet): Force can't steer a turn
        // that doesn't exist, so it queues — the existing turn/started drain delivers it.
        let (mut agents, id) = busy_agent(false, true);
        let (hub, mut rx) = test_hub_with(id);
        prompt_agent(&hub, &mut agents, id, "one more thing".into(), true, Send::Force);
        assert_eq!(agents[&id].queued, vec!["one more thing".to_string()]);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn force_send_on_an_idle_agent_behaves_like_enter() {
        let (mut agents, id) = busy_agent(false, false);
        agents.get_mut(&id).unwrap().status = Status::Idle;
        let (hub, mut rx) = test_hub_with(id);
        prompt_agent(&hub, &mut agents, id, "go".into(), true, Send::Force);
        assert!(agents[&id].awaiting_start);
        assert!(matches!(rx.try_recv(), Ok(Cmd::Turn { text }) if text == "go"));
    }

    /// The single choke point that makes a hand stop self-clearing: any message — user,
    /// orchestrator or engine — means carry on.
    #[test]
    fn any_message_lifts_a_hand_stop() {
        let (mut agents, id) = busy_agent(false, false);
        agents.get_mut(&id).unwrap().status = Status::Idle;
        agents.get_mut(&id).unwrap().stopped_by_user = true;
        let (hub, mut rx) = test_hub_with(id);
        prompt_agent(&hub, &mut agents, id, "carry on".into(), true, Send::Auto);
        assert!(!agents[&id].stopped_by_user, "talking to a stopped agent is how the user restarts it");
        assert!(matches!(rx.try_recv(), Ok(Cmd::Turn { text }) if text == "carry on"));
    }

    #[test]
    fn backspace_on_empty_input_restores_the_last_queued_message_for_editing() {
        let mut agent = Agent::new(1, "worker", "worker", PathBuf::new());
        agent.queued.push("keep this queued".into());
        agent.queued.push("edit me".into());
        let mut input = Input::default();
        // Mirrors App::on_key's Backspace-on-empty-input arm.
        if input.is_empty() {
            if let Some(text) = agent.queued.pop() {
                input.set(&text);
            }
        }
        assert_eq!(input.text(), "edit me");
        assert_eq!(agent.queued, vec!["keep this queued".to_string()]);
    }

    /// A throwaway App on the current directory: screen bookkeeping only reads `screen`, `run`
    /// and `agents`, so nothing here has to be wired up beyond those.
    fn screen_app() -> App {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let (ev_tx, _ev_rx) = tokio::sync::mpsc::unbounded_channel();
        let hub = Hub::new(vec![], vec![], ev_tx, false);
        App::new(Settings::default(), Registry::default(), PathBuf::from("."), hub, tx, true)
    }

    fn a_run() -> Run {
        Run::new(PathBuf::from("."), Pattern::builtin(), "build a thing".into())
    }

    /// The reported bug: Solo with a run up used to come back to the stage, because leaving the
    /// Studio guessed from `run.is_some()` instead of remembering where it was opened from.
    #[test]
    fn leaving_the_studio_returns_to_solo_even_while_a_run_exists() {
        let mut app = screen_app();
        app.run = Some(a_run());
        app.screen = Screen::Solo;
        app.enter_screen(Screen::Studio);
        assert_eq!(app.screen, Screen::Studio);
        app.leave_screen();
        assert_eq!(app.screen, Screen::Solo);
    }

    #[test]
    fn leaving_the_studio_restores_the_zoom_it_was_opened_from() {
        let mut app = screen_app();
        let (agents, id) = busy_agent(false, false);
        app.agents = agents;
        app.run = Some(a_run());
        app.screen = Screen::Zoom(id);
        app.enter_screen(Screen::Studio);
        app.leave_screen();
        assert_eq!(app.screen, Screen::Zoom(id));
    }

    /// The agent can be gone by the time the Studio is closed (respawned, stopped, run torn
    /// down); restoring that zoom would draw an empty view, so the default rule takes over.
    #[test]
    fn a_zoom_on_a_vanished_agent_falls_back_to_the_default_screen() {
        let mut app = screen_app();
        let (agents, id) = busy_agent(false, false);
        app.agents = agents;
        app.screen = Screen::Zoom(id);
        app.enter_screen(Screen::Studio);
        app.agents.clear();
        app.leave_screen();
        assert_eq!(app.screen, Screen::Solo, "no run: the default is Solo");

        app.run = Some(a_run());
        app.agents = busy_agent(false, false).0;
        app.screen = Screen::Zoom(id);
        app.enter_screen(Screen::Studio);
        app.agents.clear();
        app.leave_screen();
        assert_eq!(app.screen, Screen::Stage, "a run is up: the default is the stage");
    }

    /// Studio → model picker → Models (the picker's `e` key) must not record the Studio as the
    /// way back, or leaving would land on the screen the user already left.
    #[test]
    fn hopping_from_the_studio_to_models_still_returns_to_the_original_screen() {
        let mut app = screen_app();
        app.run = Some(a_run());
        app.screen = Screen::Solo;
        app.enter_screen(Screen::Studio);
        app.enter_screen(Screen::Models);
        app.leave_screen();
        assert_eq!(app.screen, Screen::Solo);
    }

    /// Nothing recorded (a screen entered before this existed, or restored state): the old
    /// stage-if-a-run-exists guess is still the fallback.
    #[test]
    fn with_nothing_remembered_leaving_keeps_the_old_rule() {
        let mut app = screen_app();
        app.screen = Screen::Studio;
        app.leave_screen();
        assert_eq!(app.screen, Screen::Solo);

        app.run = Some(a_run());
        app.screen = Screen::Models;
        app.leave_screen();
        assert_eq!(app.screen, Screen::Stage);
    }
}
