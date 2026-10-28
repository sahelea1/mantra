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
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tokio::sync::mpsc::UnboundedSender;

pub(crate) mod swarm;

pub enum AppEvent {
    Term(Event),
    Hub(HubEvent),
    Job(JobTag, JobOut),
    /// One discovery source answered (Codex catalog or a provider's /models).
    Discovered { source: String, result: Result<Vec<crate::discover::Candidate>, String> },
    /// A command from a web client (`--web` / `--remote`), executed by `App::web_command`.
    Web(crate::web::Inbound),
    /// The answer to an agent's tool call worked out off the event loop (Solo's screenshot), for
    /// `on_event` to hand on as `Cmd::Respond`.
    ToolAnswer { agent: AgentId, id: Value, result: Value },
    /// A provider added from its key answered `GET /models` (`App::probe_provider`). Carries the
    /// provider's id and host, never its key.
    Probed { provider: String, host: String, result: Result<Vec<crate::discover::Found>, String> },
    /// An MCP server was started, asked for its tools and stopped (`App::mcp_test_server`): one
    /// line for its row in `/settings`.
    McpTested { name: String, result: Result<String, String> },
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Screen {
    Solo,
    Stage,
    Zoom(AgentId),
    Studio,
    Models,
    Settings,
}

/// Input belongs to a conversation, while config screens keep their own field editors. Stage
/// has one run prompt even as its selected card changes; each zoom and Solo session has a draft.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum ChatDraft {
    Solo(Option<AgentId>),
    Stage,
    Zoom(AgentId),
}

impl ChatDraft {
    fn for_screen(screen: Screen, solo: Option<AgentId>) -> Option<Self> {
        match screen {
            Screen::Solo => Some(Self::Solo(solo)),
            Screen::Stage => Some(Self::Stage),
            Screen::Zoom(id) if Some(id) == solo => Some(Self::Solo(Some(id))),
            Screen::Zoom(id) => Some(Self::Zoom(id)),
            Screen::Studio | Screen::Models | Screen::Settings => None,
        }
    }
}

impl Screen {
    /// The screens that edit configuration (`ui::form`'s rules): leaving one returns to where it
    /// was opened from, and ctrl+t / ctrl+d mean the screen's own row actions there.
    pub fn is_config(self) -> bool {
        matches!(self, Screen::Studio | Screen::Models | Screen::Settings)
    }
}

/// What a field being edited in place (`ui::form::Edit`) belongs to.
#[derive(Clone, Debug, PartialEq)]
pub enum EditTarget {
    /// The Studio: a role's field.
    RoleField(String, String),
    /// The Studio: one of the pattern's `[settings]`.
    Setting(String),
    /// The Studio: the pattern's own `name` or `description`.
    PatternField(String),
    FlowStep(usize, String),
    /// The Studio's list: the name of a role being added (ctrl+n).
    NewRole,
    /// `/models`: a cell of the models grid (row into `registry.models`, column).
    ModelCell(usize, usize),
    /// `/models`: a cell of the providers grid (index into `registry.providers`, column).
    ProviderCell(usize, usize),
    /// `/models`, a provider from its key: the key, then the base URL.
    KeyFlowKey,
    KeyFlowUrl,
    /// `/settings`: a field by its `settings_ops::Field::key`.
    SettingField(String),
    /// `/settings`, a new MCP server: its name, then its command line.
    McpName,
    McpCommand,
}

pub enum Overlay {
    Help { scroll: usize },
    ModelPicker { sel: usize, target: Option<AgentId> },
    Diff { agent: AgentId, file: usize, scroll: usize },
    Inbox { sel: usize },
    Plan { scroll: usize },
    /// `e` on a halted stage: the halt's whole message (the band shows one line of it).
    Halt { scroll: usize },
    /// `/run` and `/pattern`: every pattern new runs can use (read once when it opens), and for
    /// `/run` the goal to start it on (`goal: Some`).
    NewRun { sel: usize, list: Vec<PatternChoice>, goal: Option<Input> },
    /// A short list to pick one (or, `multi`, several) from: the add menu of `/models`, an
    /// Akasha group, an agent to respawn, Solo's MCP servers.
    Select(Select),
    Discover(DiscoverState),
    /// `/runs`: this project's runs — resume (⏎) or delete (D, then y).
    Runs { sel: usize, list: Vec<crate::engine::state::RunSummary>, confirm: bool, others: usize },
    /// The remote link's QR code, as large as the terminal allows (⏎ on the link in `/settings`).
    Qr,
    /// `/akasha [group]`: shared run or scoped swarm notes, all groups or the one named.
    Akasha { scroll: usize, group: Option<String> },
}

/// A selector's rows and what choosing does.
pub struct Select {
    pub title: String,
    pub items: Vec<Choice>,
    pub sel: usize,
    /// Space ticks rows, ⏎ takes the ticked ones; otherwise ⏎ takes the selected row.
    pub multi: bool,
    pub then: SelectThen,
}

pub struct Choice {
    /// What choosing passes on (a group's name, an agent's id, a provider's id).
    pub key: String,
    pub label: String,
    pub detail: String,
    /// Ticked, in a `multi` selector.
    pub on: bool,
}

/// What a `Select` does with the row (or rows) chosen.
#[derive(Clone, Debug, PartialEq)]
pub enum SelectThen {
    /// `/models` ctrl+n: a provider from its key, a model, Codex, Claude Code.
    AddMenu,
    /// Choose a known API endpoint, or a custom/local URL, before entering a key.
    ProviderPreset,
    /// "A model on an existing provider": the provider it goes on.
    AddModelOn,
    /// `/akasha` with several groups: the one to show.
    Akasha,
    /// `/respawn` with no agent in view: the run agent to respawn.
    Respawn,
    /// `/settings`: the MCP servers Solo gets (`solo_mcp`).
    SoloMcp,
}

/// The rows of `/models`' add menu (`SelectThen::AddMenu`), by key.
pub const ADD_KEY: &str = "key";
pub const ADD_MODEL: &str = "model";
pub const ADD_CODEX: &str = "codex";
pub const ADD_ANTIGRAVITY: &str = "antigravity";
pub const ADD_CLAUDE: &str = "claude";
pub const ADD_CLAUDE_KEY: &str = "claude-api-key";

/// One row of the new-run overlay's pattern list.
pub struct PatternChoice {
    pub name: String,
    /// "default", "built-in", "edited" (a file shadows a built-in) or "yours" (`Pattern::tag`).
    pub tag: &'static str,
    /// The pattern's description, or why its file does not load.
    pub about: Result<String, String>,
}

impl PatternChoice {
    /// Every pattern `Pattern::list` finds (the built-ins first, `mantra-default` leading), as
    /// `Pattern::summaries` describes them.
    pub fn all(project: &std::path::Path) -> Vec<PatternChoice> {
        Pattern::summaries(project).into_iter().map(|s| PatternChoice { about: s.error.map_or(Ok(s.description), Err), name: s.name, tag: s.tag }).collect()
    }
}

/// The model-discovery picker.
pub struct DiscoverState {
    pub items: Vec<crate::discover::Candidate>,
    pub loading: Vec<String>,
    /// Provider id → destination host of the `/models` request in flight, so the "asking …" line
    /// shows exactly where each key goes (P2: placeholder requests must be impossible to miss).
    pub hosts: Vec<(String, String)>,
    pub errors: Vec<String>,
    /// Drafts that were not asked (`ProviderEntry::draft_reason`) — shown apart from failures.
    pub skipped: Vec<String>,
    pub sel: usize,
    pub filter: Input,
    /// The host of a provider just added from its key: the import then says "<host>: N models
    /// added" — the end of that flow — rather than counting what changed.
    pub origin: Option<String>,
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
    /// When the pattern was last saved: the header says so for a moment (a commit saves).
    pub saved: Option<Instant>,
}

pub struct ModelsState {
    pub row: usize,
    pub col: usize,
    pub providers: bool,
    pub status: HashMap<String, String>,
    pub dirty: bool,
    /// When models.toml was last saved: the header says so for a moment (a commit saves).
    pub saved: Option<Instant>,
    /// A provider being added from its API key, between the key and the models it lists.
    pub keyflow: Option<KeyFlow>,
}

/// `/models`' "provider from an API key", step by step: the key (an `Edit` while typed), the base
/// URL its prefix suggests, then the probe of the provider's model list.
#[derive(Default)]
pub struct KeyFlow {
    /// The key once typed; kept here only until the provider holds it.
    pub key: String,
    /// The URL `provider_key::provider_from_key` guessed for the key ("" when it could not).
    pub guess: String,
    /// The provider added and the host being asked for its models: the spinner line.
    pub probing: Option<(String, String)>,
}

/// `/settings`: the selected row and what the screen keeps between frames.
#[derive(Default)]
pub struct SettingsState {
    pub sel: usize,
    /// The patterns `default_pattern` cycles through, read when the screen opens (`Pattern::list`
    /// reads directories — not something to do every frame).
    pub patterns: Vec<String>,
    /// An MCP server's last ctrl+t ("testing…", "ok · 3 tools", "error: …").
    pub mcp_status: BTreeMap<String, String>,
    /// A new MCP server's name, between its name field and its command line field (ctrl+n).
    pub mcp_name: Option<String>,
}

/// The commands, as `/` lists and completes them. Settings that were commands are rows of
/// `/settings`; effort, approvals and the screens have keys (`MOVED`).
pub const COMMANDS: &[(&str, &str)] = &[
    ("/run", "start a run: pick a pattern, give the goal"),
    ("/runs", "this project's runs: resume, land or delete"),
    ("/stop", "stop the active run (/runs keeps it)"),
    ("/pause", "pause / resume the run"),
    ("/plan", "show the run's plan"),
    ("/land", "merge the finished run into your branch"),
    ("/respawn", "respawn a run agent in place: /respawn [agent]"),
    ("/akasha", "shared run / swarm notes: /akasha [group]"),
    ("/inbox", "approvals & alerts"),
    ("/pattern", "choose the pattern for new runs"),
    ("/studio", "pattern studio: roles, flow, architect"),
    ("/models", "models & providers: add from a key, test"),
    ("/settings", "Solo, runs, MCP, web & remote, appearance"),
    ("/model", "switch model (picker; ←→ effort)"),
    ("/new", "start a fresh Solo session"),
    ("/compact", "compact the focused agent's context"),
    ("/diff", "open the changes viewer"),
    ("/help", "keys & commands"),
    ("/quit", "exit Mantra"),
];

/// Commands 0.8 folded into `/settings` and keys, and where each went: typed by hand, one says
/// so (for one release) instead of "unknown command".
pub const MOVED: &[(&str, &str)] = &[
    ("/swarm", "/swarm is gone: worker-swarm is a class of every pattern (/studio)"),
    ("/experimental", "experimental features are in /settings (Features)"),
    ("/mcp", "MCP servers are in /settings"),
    ("/web", "the web UI's address and security are in /settings (Web & remote)"),
    ("/remote", "the remote link, code and QR are in /settings (Web & remote)"),
    ("/effort", "effort: alt+↑↓, or ←→ in the model picker (ctrl+k)"),
    ("/approvals", "approvals: shift+tab cycles them; /settings sets them"),
    ("/verbose", "the verbose log is ctrl+e"),
    ("/mandala", "the Mandala stage is ctrl+o"),
];

/// One line of the `/` suggestions: what tab puts in the input, and what it is.
#[derive(Clone, Debug, PartialEq)]
pub struct Suggestion {
    pub text: String,
    pub label: String,
    pub about: String,
}

pub struct App {
    pub settings: Settings,
    pub registry: Registry,
    pub project: PathBuf,
    pub hub: Hub,
    pub agents: BTreeMap<AgentId, Agent>,
    pub screen: Screen,
    /// The screen a config screen (`Screen::is_config`) was opened from. Leaving it returns
    /// exactly there, so a zoom (or Solo while a run is up) survives a trip through the Studio
    /// instead of being guessed at from whether a run exists.
    screen_before: Option<Screen>,
    pub overlays: Vec<Overlay>,
    pub input: Input,
    drafts: HashMap<ChatDraft, Input>,
    /// The field being edited in place on a config screen (`ui::form`): one at a time, wherever
    /// it is — the screens draw it in their cell and send it the keys.
    pub edit: Option<crate::ui::form::Edit>,
    /// A Delete waiting for its second press (`ui::form::confirm`).
    pub confirm: Option<crate::ui::form::Confirm>,
    pub solo: Option<AgentId>,
    /// The MCP servers the current Solo agent was started with (`/settings` says which).
    pub solo_mcp: Vec<String>,
    /// Standalone Solo subagents, keyed by child id; only the Solo session that started them may
    /// message, inspect, or wait for them.
    solo_children: HashMap<AgentId, AgentId>,
    /// Tool calls awaiting completion of a Solo subagent.
    solo_waiters: HashMap<AgentId, Vec<(AgentId, Value)>>,
    pub(crate) swarms: HashMap<AgentId, swarm::SwarmGroup>,
    swarm_owners: HashMap<AgentId, AgentId>,
    swarm_waiters: HashMap<AgentId, Vec<Value>>,
    swarm_deferred: HashSet<AgentId>,
    pub run: Option<Run>,
    pub approvals: Vec<Approval>,
    pub toast: Option<(String, Instant, Level)>,
    pub sel: usize,
    /// Centers of the cards actually visible in the last overview frame.
    pub stage_nav: Vec<(AgentId, u16, u16)>,
    /// When the halt the stage selection last jumped to began: a new halt about one agent
    /// selects it once, so the band's `m` / `r` act on the agent it names.
    halt_seen: Option<Instant>,
    /// The task whose card the selection follows across an `r` respawn: the new attempt has no
    /// agent, so no node, until its worktree is ready, and the selection lands on it when it
    /// does instead of staying wherever the shorter node list left it.
    sel_follow: Option<String>,
    pub canvas_focus: bool,
    pub verbose: bool,
    pub side_panel: bool,
    pub pulse_panel: bool,
    pub quit: bool,
    /// When ctrl+c was last pressed and how many times in a row (within 2s of each other):
    /// the first press does the contextual thing (close an overlay, interrupt a busy Solo turn,
    /// clear the input, or interrupt another focused agent), the third always quits.
    ctrl_c: Option<Instant>,
    ctrl_c_count: u8,
    pub tx: UnboundedSender<AppEvent>,
    pub demo: bool,
    pub studio: StudioState,
    pub models_ui: ModelsState,
    pub settings_ui: SettingsState,
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
    /// identity `/settings` shows, push subscriptions.
    pub web: Option<crate::web::Link>,
}

/// Options for creating any agent (Solo, run agents, architect, probes).
#[derive(Clone)]
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
    /// Whether a workspace-write agent may use the network (the run pattern's `settings.network`).
    pub network: bool,
    /// The pattern's `settings.writable_roots` (`~` unexpanded): writable by a workspace-write agent.
    pub writable_roots: Vec<String>,
    /// Explicit context-window override (from a halved assumption); `None` uses the model's own
    /// effective context.
    pub context_override: Option<u64>,
    /// Re-attach to a saved Codex thread / Claude session instead of starting a new one
    /// (`mantra runs resume`).
    pub resume_thread: Option<String>,
    /// What the backend calls this agent's thread (`<run title> · <role name>`), where it can.
    pub thread_name: Option<String>,
    /// The MCP servers this agent gets, resolved by the caller (`Settings::mcp_for` on Solo's
    /// `solo_mcp` or the role's `mcp`); empty for everything else (architect, probes).
    pub mcp: Vec<(String, crate::config::McpServer)>,
}

/// The name `--demo` registers its stand-in MCP server under (`mantra mock-mcp`).
pub const DEMO_MCP: &str = "demo";

/// One journal line per role and MCP server it asks for that settings.toml does not have, or
/// names in a way that cannot work (`Settings::mcp_refused`): the role runs without it, as it
/// would if it had never asked — never a halt over a missing extra.
fn mcp_missing_lines(settings: &Settings, pattern: &Pattern) -> Vec<String> {
    let mut out = vec![];
    for (name, role) in pattern.ordered_roles() {
        for missing in settings.mcp_missing(&role.mcp) {
            out.push(format!("mcp: role {name} asks for `{missing}`, which is not configured — skipped"));
        }
        for (server, why) in settings.mcp_refused(&role.mcp) {
            out.push(format!("mcp: role {name} gets `{server}`, whose name in settings.toml cannot work — skipped ({why})"));
        }
    }
    out
}

/// What every Claude Code agent goes without when the Hub's MCP-bridge socket could not be made
/// (`Hub::bridge_problem`): the toast at startup, the journal line of a run that has one.
fn bridge_problem_line(why: &str) -> String {
    format!("Claude Code / Antigravity agents have no Mantra tools: {why}")
}

/// Whether any role of `pattern` runs on Claude Code — its model, or the fallback it may switch to.
fn pattern_uses_cli_bridge(registry: &Registry, pattern: &Pattern) -> bool {
    let on_claude = |alias: &str| matches!(registry.backend_of(&registry.resolve(alias)), crate::config::ProviderKind::ClaudeCode | crate::config::ProviderKind::Antigravity);
    pattern.roles.values().any(|r| on_claude(&r.model) || r.fallback.as_deref().is_some_and(|f| !f.trim().is_empty() && on_claude(f)))
}

/// settings.toml's `[mcp_servers]` when the file is there and parses (`App::refresh_mcp_servers`).
fn on_disk_mcp_servers() -> Option<BTreeMap<String, crate::config::McpServer>> {
    if !crate::config::home().join("settings.toml").exists() {
        return None;
    }
    Settings::load_on_disk().ok().map(|s| s.mcp_servers)
}

/// Solo's toast for the servers `solo_mcp` would give it whose names cannot work, if any.
fn mcp_refused_toast(settings: &Settings) -> Option<String> {
    let refused = settings.mcp_refused(&settings.solo_mcp);
    let first = refused.first()?;
    let more = if refused.len() > 1 { format!(" (and {} more)", refused.len() - 1) } else { String::new() };
    Some(format!("MCP server `{}` skipped: {}{more} — rename it in settings.toml", first.0, first.1))
}

/// `mantra experimental`: every experimental feature, on or off, with its line.
pub fn experimental_list(settings: &Settings) -> Vec<String> {
    crate::config::EXPERIMENTAL.iter().map(|(name, about)| format!("{name:<8} {:<3}  {about}", if settings.experimental_on(name) { "on" } else { "off" })).collect()
}

pub fn is_experimental(name: &str) -> bool {
    crate::config::EXPERIMENTAL.iter().any(|(n, _)| *n == name)
}

/// Switches an experimental feature in settings.toml as it is on disk — not in the session's
/// settings, which `--demo` has pointed at its mocks — and returns the settings now saved. A
/// settings.toml that does not parse is refused rather than saved over: `Settings::load` would
/// have fallen back to the defaults, and saving those would wipe every setting in the file.
pub fn save_experimental(name: &str, on: bool) -> Result<Settings, String> {
    if !is_experimental(name) {
        let known: Vec<&str> = crate::config::EXPERIMENTAL.iter().map(|(n, _)| *n).collect();
        return Err(format!("unknown experimental feature '{name}' (there is: {})", known.join(", ")));
    }
    let path = crate::config::home().join("settings.toml");
    let mut s = match std::fs::read_to_string(&path) {
        Ok(text) => toml::from_str::<Settings>(&text).map_err(|e| format!("{} does not parse, fix it first: {e}", path.display()))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Settings::default(),
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    s.experimental.retain(|f| f != name);
    if on {
        s.experimental.push(name.to_string());
    }
    s.save().map_err(|e| format!("could not save {}: {e}", path.display()))?;
    Ok(s)
}

/// How long a config screen's header says "saved" after a commit wrote its file.
pub const SAVED_SHOWN: Duration = Duration::from_millis(2000);

/// Settings `settings_ops::takes_effect` says apply at the next start but this app applies the
/// moment they change (`App::apply_settings`, the event loop's frame rate and mouse capture).
const APPLIED_LIVE: &[&str] = &["glyphs", "colors", "reduce_motion", "mouse", "fps"];

/// Whether this app applies setting `key` the moment it changes, whatever `takes_effect` says.
pub fn applied_live(key: &str) -> bool {
    APPLIED_LIVE.contains(&key)
}

/// `/settings`' ctrl+t on an MCP server: started, handshake, tool list, stopped — within
/// `MCP_TEST_BUDGET` — and one line on how that went ("ok · 4 tools · blender-mcp 1.1", or the
/// error with the last thing the server wrote on stderr).
async fn mcp_probe(server: &crate::config::McpServer) -> Result<String, String> {
    let mut c = crate::mcp::Client::launch(server)?;
    let outcome = tokio::time::timeout(crate::MCP_TEST_BUDGET, async {
        c.initialize().await?;
        c.list_tools().await
    })
    .await;
    let why = match outcome {
        Ok(Ok(tools)) => {
            let n = tools.len();
            let who = if c.server.is_empty() { String::new() } else { format!(" · {}", c.server) };
            return Ok(format!("ok · {n} tool{}{who}", if n == 1 { "" } else { "s" }));
        }
        Ok(Err(e)) => e,
        Err(_) => format!("no answer within {} s", crate::MCP_TEST_BUDGET.as_secs()),
    };
    let tail = c.last_words().await;
    Err(match tail.lines().last() {
        Some(last) => format!("{why} — {}", last.trim()),
        None => why,
    })
}

fn toml_str(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// The Codex `-c` overrides for one agent's sandbox. The spawn's own extra roots always go in. A
/// workspace-write agent also gets the pattern's roots (existing directories, `~` expanded), the
/// toolchain caches, and the network switch when the pattern allows it. Other modes keep only the
/// extra roots, as before. The roots are one deduplicated list, emitted once.
fn sandbox_overrides(sandbox: &str, network: bool, extra: &[PathBuf], pattern: &[String], toolchain: &[PathBuf], home: Option<&Path>) -> Vec<String> {
    let workspace = sandbox == "workspace-write";
    let mut roots: Vec<PathBuf> = extra.to_vec();
    if workspace {
        roots.extend(pattern.iter().map(|p| crate::util::expand_tilde(p, home)).filter(|p| crate::util::sandbox_can_own(p)));
        roots.extend(toolchain.iter().cloned());
    }
    let mut unique: Vec<PathBuf> = vec![];
    for r in roots {
        if !unique.contains(&r) {
            unique.push(r);
        }
    }
    let mut out = vec![];
    if !unique.is_empty() {
        let list: Vec<String> = unique.iter().map(|p| toml_str(&p.to_string_lossy())).collect();
        out.push("-c".to_string());
        out.push(format!("sandbox_workspace_write.writable_roots=[{}]", list.join(",")));
    }
    if workspace && network {
        out.push("-c".to_string());
        out.push("sandbox_workspace_write.network_access=true".to_string());
    }
    out
}

/// The Codex `-c` overrides for MCP servers. Mantra's own tools reach the agent as
/// `dynamic_tools` (`SpawnSpec.dynamic_tools`); the user's own `~/.codex/config.toml` MCP servers
/// (`user_servers`) are never on that path, so the model can only call them with a name the
/// app-server can't route — the log's "unsupported call: mcp__…". `-c mcp_servers={}` doesn't
/// clear an already-populated table (`-c` merges at dotted leaf paths, never replaces a table
/// wholesale), so each of the user's servers is disabled by name instead — all but the ones this
/// agent is given (`given`, from settings.toml), which come with their command, args and env,
/// and which Codex runs itself and reports as `mcpToolCall` items.
fn codex_mcp_overrides(user_servers: &[String], given: &[(String, crate::config::McpServer)]) -> Vec<String> {
    let mut out = vec![];
    for name in user_servers.iter().filter(|n| !given.iter().any(|(g, _)| g == *n)) {
        out.push("-c".into());
        out.push(format!("mcp_servers.{name}.enabled=false"));
    }
    out.extend(crate::hub::codex_mcp_args(given));
    out
}

/// The extra writable roots a Bija agent is launched with: for `workspace-write`, the spawn's own
/// roots and the pattern's (existing directories, `~` expanded) — the set Codex gets, minus the
/// toolchain caches, which bija opens by itself. `full` writes everywhere and `read-only`
/// nowhere, so no roots apply to them.
fn bija_writable_roots(sandbox: &str, extra: &[PathBuf], pattern: &[String], home: Option<&Path>) -> Vec<PathBuf> {
    if sandbox != "workspace-write" {
        return vec![];
    }
    let mut roots: Vec<PathBuf> = vec![];
    let pattern_roots = pattern.iter().map(|p| crate::util::expand_tilde(p, home)).filter(|p| crate::util::sandbox_can_own(p));
    for r in extra.iter().cloned().chain(pattern_roots) {
        if !roots.contains(&r) {
            roots.push(r);
        }
    }
    roots
}

/// Whether a Claude Code agent runs under `--strict-mcp-config` (`ClaudeSpawn::strict_mcp`): one
/// with a toolset of Mantra's beyond Solo's delegation and vision tools (a run's agents, the
/// architect) or with servers of Mantra's. Solo keeps the user's own `claude mcp` servers.
fn claude_strict_mcp(o: &AgentOpts) -> bool {
    !o.mcp.is_empty() || o.tools.iter().any(|t| {
        !t.get("name").and_then(|n| n.as_str()).is_some_and(|n| {
            tools::VISION_TOOLS.contains(&n) || n.starts_with("mantra_subagent_") || n.starts_with("mantra_swarm_") || n.starts_with("mantra_akasha_")
        })
    })
}

/// A Bija agent's spawn config and child environment. Like the Claude arm of `spawn_agent`,
/// everything comes from the model's own provider entry and the role, resolved here so
/// `hub::bija` never reads the registry; the key travels in the child's environment under the
/// name `--api-key-env` gives — never on argv, never logged.
fn bija_spawn(reg: &Registry, m: &crate::config::ModelEntry, o: &AgentOpts, cw: u64, compact_pct: u8) -> (crate::hub::BijaSpawn, Vec<(String, String)>) {
    let provider = reg.providers.iter().find(|p| p.id == m.provider);
    let key = provider.and_then(|p| p.resolve_key().map(|k| (p.env_var_name(), k)));
    let spawn = crate::hub::BijaSpawn {
        base_url: provider.map(|p| p.base_url.trim().to_string()).unwrap_or_default(),
        wire: provider.map(|p| p.bija_wire()).unwrap_or("openai-chat").to_string(),
        api_key_env: key.as_ref().map(|(name, _)| name.clone()),
        context_window: cw,
        compact_pct,
        system_prompt: o.instructions.clone(),
        home: crate::config::home().join("bija"),
        sandbox: crate::hub::bija_sandbox(&o.sandbox).into(),
        writable_roots: bija_writable_roots(&o.sandbox, &o.extra_writable, &o.writable_roots, dirs::home_dir().as_deref()),
        network: o.network,
        vision: reg.vision(&m.alias),
    };
    let mut envs: Vec<(String, String)> = key.into_iter().collect();
    let headers = provider.map(|p| &p.headers).cloned().unwrap_or_default();
    envs.push(("BIJA_PROVIDER_HEADERS".into(), serde_json::to_string(&headers).unwrap_or_else(|_| "{}".into())));
    (spawn, envs)
}

fn agent_spec(hub: &mut Hub, reg: &Registry, o: &AgentOpts) -> (SpawnSpec, String, bool) {
    let m = reg.resolve(&o.model_alias);
    let wanted = o.effort.as_deref().filter(|e| !e.is_empty()).unwrap_or(&m.default_effort).to_string();
    let (effort, lowered) = m.resolve_effort_lowered(&wanted);
    let backend = reg.backend_of(&m);
    // Always tell the backend a context window and compaction limit — an assumed 200k / 85% when
    // the model doesn't have its own, so auto-compaction never relies on a provider's own defaults
    // (Codex: `-c model_context_window=…`/`model_auto_compact_token_limit=…`; Claude: `--autocompact`;
    // bija: `--context-window`/`--compact-at`).
    let cw = o.context_override.unwrap_or_else(|| m.effective_context());
    let compact_pct = m.effective_compact_percent();
    let (extra, envs, claude, bija, antigravity) = if backend == crate::hub::Backend::ClaudeCode {
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
        let strict_mcp = claude_strict_mcp(&o);
        (vec![], envs, Some(crate::hub::ClaudeSpawn { auth, base_url, autocompact: cw, system_prompt: o.instructions.clone(), mcp_sock: hub.ensure_bridge(), strict_mcp }), None, None)
    } else if backend == crate::hub::Backend::Bija {
        let (spawn, envs) = bija_spawn(reg, &m, &o, cw, compact_pct);
        (vec![], envs, None, Some(spawn), None)
    } else if backend == crate::hub::Backend::Antigravity {
        (vec![], vec![], None, None, Some(crate::hub::AntigravitySpawn { system_prompt: o.instructions.clone(), mcp_sock: hub.ensure_bridge() }))
    } else {
        let mut extra = reg.provider_args();
        extra.extend(codex_mcp_overrides(&crate::config::codex_user_mcp_server_names(), &o.mcp));
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
        // Codex's workspace-write sandbox writes only inside the cwd and has no network unless told to.
        // `o.sandbox` is already the fallback-adjusted mode (`role_sandbox`), so the toolchain caches
        // are looked up only for a sandbox that will use them.
        let toolchain = if o.sandbox == "workspace-write" { crate::util::toolchain_writable_roots() } else { vec![] };
        let home = dirs::home_dir();
        extra.extend(sandbox_overrides(&o.sandbox, o.network, &o.extra_writable, &o.writable_roots, &toolchain, home.as_deref()));
        // Deliver a key stored directly in the provider (not just named by env_key) into the
        // child's own environment — never on argv, never logged.
        let envs: Vec<(String, String)> = reg
            .providers
            .iter()
            .find(|p| p.id == m.provider)
            .and_then(|p| p.resolve_key().map(|k| (p.env_var_name(), k)))
            .into_iter()
            .collect();
        (extra, envs, None, None, None)
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
        thread_name: o.thread_name.clone(),
        backend,
        envs,
        claude,
        bija,
        antigravity,
        mcp: o.mcp.clone(),
    };
    (spec, wanted, lowered)
}

pub fn spawn_agent(hub: &mut Hub, agents: &mut BTreeMap<AgentId, Agent>, reg: &Registry, mut o: AgentOpts) -> AgentId {
    // Resolve once, before building any backend's sandbox configuration. The original permission
    // stays on the agent/spec so the UI and Claude's explicit sandbox opt-out remain accurate.
    if o.approval == crate::config::BYPASS_SANDBOX_DANGER {
        o.sandbox = "danger-full-access".into();
    }
    let id = hub.alloc_id();
    let (spec, wanted, lowered) = agent_spec(hub, reg, &o);
    let m = reg.resolve(&o.model_alias);
    let effort = spec.effort.clone();
    let backend = spec.backend;
    let cw = o.context_override.unwrap_or_else(|| m.effective_context());
    let launch_options = o.clone();
    let mut a = Agent::new(id, &o.name, &o.role, o.cwd);
    a.launch_options = Some(Box::new(launch_options));
    a.glyph = o.glyph;
    a.color = o.color;
    a.model_alias = m.alias.clone();
    a.model = m.model.clone();
    a.provider = m.provider.clone();
    a.backend = backend;
    a.effort = effort.clone();
    a.ctx_window = (backend != crate::config::ProviderKind::Antigravity).then_some(cw);
    a.approval = o.approval.clone();
    a.sandbox = o.sandbox.clone();
    if backend == crate::config::ProviderKind::Antigravity {
        a.notice(Level::Info, "Antigravity headless mode soft-denies native permission requests instead of showing Mantra prompts; explicit Never or BYPASS permissions enable unattended tools. Context and compaction are managed by agy.");
    }
    if lowered {
        // The clamp is right (the model has no such setting) — but say so once, or a role
        // configured for max quietly runs at high.
        a.notice(Level::Info, format!("effort {wanted} is not offered by {} — using {effort}", m.alias));
    }
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
    // A process explicitly ended by the user cannot accept messages. Preserve its stop latch,
    // including when a late orchestrator message arrives after the process was removed.
    if matches!(a.status, Status::Stopped) {
        a.notice(Level::Warn, "agent is stopped — respawn it or start a new Solo session");
        return;
    }
    // Any message — from the user, the orchestrator or the engine — ends a hand stop: talking to
    // an agent *is* telling it to carry on. Every send funnels through here, so this one line is
    // the whole lifecycle of the flag.
    a.stopped_by_user = false;
    if echo {
        a.push_user(&text);
    }
    a.follow = true;
    a.scroll = 0;
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
        a.turn_wait_since = Some(Instant::now());
        a.status = Status::Busy;
        a.activity = "starting turn".into();
        hub.send(id, Cmd::Turn { text });
    }
}

/// What the toast says after a model change on a halted run: the change applied, the run did not
/// resume (see `App::model_switched_for_run_agent`).
pub const MODEL_CHANGED_HALTED: &str = "model changed — resume the run to continue (space / Resume)";

/// The engine's window into the app.
pub struct Ctxt<'a> {
    pub hub: &'a mut Hub,
    pub agents: &'a mut BTreeMap<AgentId, Agent>,
    pub registry: &'a Registry,
    /// For what a spawn resolves from settings: the role's MCP servers.
    pub settings: &'a Settings,
    pub tx: &'a UnboundedSender<AppEvent>,
    pub notes: &'a mut Vec<String>,
}

/// Mirrors `App::sandbox_warning.is_some()` — set once, at startup, by `set_sandbox_warning`.
/// `role_sandbox` reads it from wherever a role gets spawned (`Ctxt::spawn`/`spawn_resumed`,
/// `start_solo`), which have no other way to reach the App that isn't already threading a new
/// field through call sites this work package doesn't own (the TUI's own `Ctxt` construction).
static SANDBOX_FALLBACK: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// A role's sandbox request, downgraded for a Codex agent when this host can't create the user
/// namespaces `bwrap` needs (`SANDBOX_FALLBACK`): every sandbox mode goes through the same
/// machinery, so keeping `workspace-write` or `read-only` would just trade one `bwrap` failure for
/// another, while `danger-full-access` skips `bwrap` entirely. Claude Code agents never sandbox
/// through `bwrap`, and bija probes it itself and runs commands plainly when it can't, so both
/// keep the role's own mode.
fn role_sandbox(sandbox: &str, backend: crate::config::ProviderKind) -> String {
    if SANDBOX_FALLBACK.load(std::sync::atomic::Ordering::Relaxed) && backend == crate::config::ProviderKind::Codex {
        return "danger-full-access".into();
    }
    if sandbox.is_empty() { "workspace-write".into() } else { sandbox.into() }
}

/// The default test harness runs every `#[test]` as a thread in one shared process, so a test
/// that trips `SANDBOX_FALLBACK` would otherwise leave it set for every test that runs after it in
/// the same binary. A test that sets it must reset it (ideally via a guard that restores the prior
/// value on drop) before returning.
#[cfg(test)]
fn reset_sandbox_fallback_for_test() {
    SANDBOX_FALLBACK.store(false, std::sync::atomic::Ordering::Relaxed);
}

impl Ctx for Ctxt<'_> {
    fn spawn(&mut self, r: SpawnReq) -> AgentId {
        let backend = self.registry.backend_of(&self.registry.resolve(&r.role.model));
        let sandbox = role_sandbox(&r.role.sandbox, backend);
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
                network: r.network,
                writable_roots: r.writable_roots,
                context_override: Some(r.context_override.filter(|n| *n > 0).unwrap_or(u64::MAX).min(self.registry.effective_context_window(&r.role.model, &self.settings.default_model))),
                resume_thread: None,
                thread_name: r.thread_name,
                mcp: self.settings.mcp_for(&r.mcp),
            },
        )
    }
    fn spawn_resumed(&mut self, r: SpawnReq, thread: String) -> AgentId {
        let backend = self.registry.backend_of(&self.registry.resolve(&r.role.model));
        let sandbox = role_sandbox(&r.role.sandbox, backend);
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
                network: r.network,
                writable_roots: r.writable_roots,
                context_override: Some(r.context_override.filter(|n| *n > 0).unwrap_or(u64::MAX).min(self.registry.effective_context_window(&r.role.model, &self.settings.default_model))),
                resume_thread: Some(thread),
                thread_name: r.thread_name,
                mcp: self.settings.mcp_for(&r.mcp),
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
            ag.turn_wait_since = None;
            ag.finished.get_or_insert(Instant::now());
            ag.activity = "archived".into();
        }
    }
    fn tool_result(&mut self, a: AgentId, req: Value, text: String, ok: bool) {
        self.hub.send(a, Cmd::Respond { id: req, result: json!({"contentItems": [{"type": "inputText", "text": text}], "success": ok}) });
    }
    fn tool_result_items(&mut self, a: AgentId, req: Value, items: Vec<Value>, ok: bool) {
        self.hub.send(a, Cmd::Respond { id: req, result: json!({"contentItems": items, "success": ok}) });
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
    fn set_thread_name(&mut self, a: AgentId, name: &str) {
        self.hub.send(a, Cmd::SetName(name.to_string()));
    }
}

impl App {
    pub fn new(mut settings: Settings, registry: Registry, project: PathBuf, hub: Hub, tx: UnboundedSender<AppEvent>, demo: bool) -> App {
        // A settings.toml from 0.7 may name a built-in that is gone (`pattern::RETIRED`): it reads
        // as the default, and the next save of the settings writes that.
        let pattern_name = crate::engine::pattern::current_name(&settings.default_pattern, &project);
        settings.default_pattern = pattern_name.clone();
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
        let bridge_problem = hub.bridge_problem();
        let mut app = App {
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
            drafts: HashMap::new(),
            edit: None,
            confirm: None,
            solo: None,
            solo_mcp: vec![],
            solo_children: HashMap::new(),
            solo_waiters: HashMap::new(),
            swarms: HashMap::new(),
            swarm_owners: HashMap::new(),
            swarm_waiters: HashMap::new(),
            swarm_deferred: HashSet::new(),
            run: None,
            approvals: vec![],
            toast: None,
            sel: 0,
            stage_nav: vec![],
            halt_seen: None,
            sel_follow: None,
            canvas_focus: false,
            verbose: false,
            pulse_panel: true,
            quit: false,
            ctrl_c: None,
            ctrl_c_count: 0,
            tx,
            demo,
            studio: StudioState { pattern, sel: sel0, field: 0, focus: 0, errors: vec![], dirty: false, architect: None, input: Input::default(), flash: None, saved: None },
            models_ui: ModelsState { row: 0, col: 0, providers: false, status: HashMap::new(), dirty: false, saved: None, keyflow: None },
            settings_ui: SettingsState::default(),
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
        };
        // Said once, here: only mantra.log would know otherwise, and a Claude Code agent without
        // its tools just looks like one that will not use them.
        if let Some(why) = bridge_problem {
            app.toast(bridge_problem_line(&why), Level::Warn);
        }
        app
    }

    pub fn toast(&mut self, text: impl Into<String>, level: Level) {
        self.toast = Some((text.into(), Instant::now(), level));
    }

    pub fn animating(&self) -> bool {
        self.agents.values().any(|a| crate::ui::producing(a) || a.status == Status::Starting)
            || self.toast.as_ref().map(|t| t.1.elapsed().as_millis() < crate::ui::toast_life(&t.0) + 100).unwrap_or(false)
            || self.agents.values().any(|a| a.compacting || a.ctx_anim.map(|(_, t)| t.elapsed().as_millis() < 950).unwrap_or(false))
            || self.run.as_ref().map(|r| (r.is_active() && !r.halted() && r.stage != Stage::Review) || r.pulse.back().map(|p| p.at.elapsed().as_millis() < 1600).unwrap_or(false)).unwrap_or(false)
            || self.studio.flash.map(|f| f.elapsed() < Duration::from_millis(900)).unwrap_or(false)
            || self.flash_screen.map(|f| f.elapsed() < Duration::from_millis(450)).unwrap_or(false)
            // the config screens: a probe's or a test's spinner, "del again" until it lapses,
            // "saved" until it fades
            || self.models_ui.keyflow.as_ref().is_some_and(|k| k.probing.is_some())
            || self.settings_ui.mcp_status.values().any(|s| s == "testing…")
            || self.confirm.as_ref().is_some_and(|c| c.at.elapsed() < crate::ui::form::CONFIRM_WITHIN + Duration::from_millis(100))
            || [self.models_ui.saved, self.studio.saved].iter().flatten().any(|t| t.elapsed() < SAVED_SHOWN + Duration::from_millis(100))
    }

    /// Something on screen shows a running clock (the run timer, "halted for", agent ages), so an
    /// idle TUI still needs a redraw about once a second — far cheaper than `animating`.
    pub fn clocks_visible(&self) -> bool {
        self.run.is_some() || !self.agents.is_empty()
    }

    pub(crate) fn with_run<R>(&mut self, f: impl FnOnce(&mut Run, &mut Ctxt) -> R) -> Option<R> {
        let mut run = self.run.take()?;
        let r = {
            let mut ctx = Ctxt { hub: &mut self.hub, agents: &mut self.agents, registry: &self.registry, settings: &self.settings, tx: &self.tx, notes: &mut self.notes };
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
            self.drafts.remove(&ChatDraft::Solo(Some(old)));
            if self.screen == Screen::Solo {
                self.input = Input::default();
                self.suggest = 0;
            }
            self.stop_swarm_children(old);
            self.stop_solo_children(old);
            self.hub.shutdown(old);
            self.agents.remove(&old);
        }
        // A removed/stale default must never be resolved into a phantom raw model id. Use the
        // configured default when it is runnable, otherwise the first runnable registry entry.
        let usable = self.registry.models.iter().filter(|m| self.demo || self.registry.alias_problem(&m.alias).is_none()).map(|m| m.alias.clone()).collect::<Vec<_>>();
        let chosen = if self.demo || self.registry.alias_problem(&self.settings.default_model).is_none() {
            self.registry.get(&self.settings.default_model).map(|m| m.alias.clone())
        } else {
            None
        }.or_else(|| usable.first().cloned());
        let Some(chosen) = chosen else {
            self.toast("No usable model is configured. Add or fix a model in /models, then start Solo again.", Level::Warn);
            return;
        };
        if chosen != self.settings.default_model {
            self.settings.default_model = chosen.clone();
            self.save_settings();
            self.toast(format!("Solo default updated to {chosen} because the previous model is unavailable"), Level::Info);
        }
        self.refresh_mcp_servers();
        let backend = self.registry.backend_of(&self.registry.resolve(&chosen));
        let sandbox = role_sandbox(&self.settings.sandbox, backend);
        let mcp = self.settings.mcp_for(&self.settings.solo_mcp);
        let mcp_names: Vec<String> = mcp.iter().map(|(name, _)| name.clone()).collect();
        let id = spawn_agent(
            &mut self.hub,
            &mut self.agents,
            &self.registry,
            AgentOpts {
                name: "solo".into(),
                role: "solo".into(),
                glyph: "●".into(),
                color: "saffron".into(),
                model_alias: chosen.clone(),
                effort: None,
                cwd: self.project.clone(),
                approval: self.settings.approval_mode.clone(),
                sandbox,
                instructions: tools::SWARM_OWNER_PROTOCOL.into(),
                // the vision tools when Solo's model takes images (`on_request` runs them)
                tools: {
                    let mut t = tools::solo_tools();
                    t.extend(tools::swarm_tools());
                    t.extend(tools::akasha_tools());
                    if self.registry.vision(&chosen) { t.extend(tools::vision_tools()); }
                    t
                },
                extra_writable: vec![],
                network: true,
                writable_roots: vec![],
                context_override: Some(self.registry.effective_context_window(&chosen, &self.settings.default_model)),
                resume_thread: None,
                thread_name: None,
                mcp,
            },
        );
        if self.screen == Screen::Solo && self.solo.is_none() {
            // The welcome prompt's draft belongs to the newly created Solo conversation.
            self.drafts.remove(&ChatDraft::Solo(None));
        }
        self.solo = Some(id);
        self.solo_mcp = mcp_names;
        if let Some(t) = mcp_refused_toast(&self.settings) {
            self.toast(t, Level::Warn);
        }
        if let Some(why) = self.model_problem(&chosen) {
            if let Some(a) = self.agents.get_mut(&id) {
                a.notice(Level::Warn, &why);
            }
        }
        if self.sandbox_warning.is_some() && backend == crate::config::ProviderKind::Codex {
            if let Some(a) = self.agents.get_mut(&id) {
                a.notice(Level::Warn, "sandbox unavailable here: Codex agents run with danger-full-access; git worktrees still keep workers apart");
            }
        }
        // the startup toast goes away; Solo's own log keeps it where its missing tools show
        if let Some(why) = self.hub.bridge_problem().filter(|_| matches!(backend, crate::config::ProviderKind::ClaudeCode | crate::config::ProviderKind::Antigravity)) {
            if let Some(a) = self.agents.get_mut(&id) {
                a.notice(Level::Warn, bridge_problem_line(&why));
            }
        }
    }

    /// The run's journal line for a lost bridge, when one of its roles runs on Claude Code.
    fn journal_bridge_problem(&self, run: &mut Run) {
        if let Some(why) = self.hub.bridge_problem().filter(|_| pattern_uses_cli_bridge(&self.registry, &run.pattern)) {
            run.log("⚠", "amber", bridge_problem_line(&why));
        }
    }

    /// `Registry::alias_problem`, except in demo mode: the mock backends never contact a
    /// provider, so a missing key for whatever models.toml configures must not block the demo
    /// (the finding was `--demo` refusing to start over an unset custom-provider variable).
    fn model_problem(&self, alias: &str) -> Option<String> {
        if self.demo {
            return None;
        }
        self.registry.alias_problem(alias)
    }

    /// Saves the session's settings to settings.toml — except in demo mode, whose settings point
    /// at its mocks and have Akasha switched on in memory: a shift+tab or a model pick in the demo
    /// must not leave `mock-codex` or `experimental = ["akasha"]` behind for every real session.
    ///
    /// `[mcp_servers]` is written as the file has it now, not as this session read it: `mantra mcp
    /// add` from another terminal while the app is open must survive the next model pick.
    pub(crate) fn save_settings(&mut self) {
        if self.demo {
            return;
        }
        let mut s = self.settings.clone();
        if let Some(servers) = on_disk_mcp_servers() {
            s.mcp_servers = servers;
        }
        // A settings.toml that does not parse is never written over (`Settings::save`): the
        // change holds for this session, and the toast says what to fix.
        if let Err(e) = s.save() {
            self.toast(format!("{e}"), Level::Error);
        }
    }

    /// Takes `[mcp_servers]` from settings.toml as it is now, so a `mantra mcp add|remove|enable|
    /// disable` made while the app is open reaches the next Solo (`/new`), the next run and
    /// `/settings`. The demo keeps its own (the stand-in server, in memory only); no file, or one that
    /// does not parse, keeps what the session has.
    pub(crate) fn refresh_mcp_servers(&mut self) {
        if self.demo {
            return;
        }
        if let Some(servers) = on_disk_mcp_servers() {
            self.settings.mcp_servers = servers;
        }
    }

    /// `Registry::preflight` with the same demo-mode exemption as `model_problem`.
    fn preflight(&self, pattern: &Pattern) -> Vec<String> {
        if self.demo {
            return vec![];
        }
        self.registry.preflight(pattern)
    }

    /// Switch screens, flashing the header briefly on a Stage↔Zoom jump (skipped when
    /// `reduce_motion` is on) so the overview/zoom switch never feels silent.
    pub(crate) fn set_screen(&mut self, s: Screen) {
        let old_draft = ChatDraft::for_screen(self.screen, self.solo);
        let new_draft = ChatDraft::for_screen(s, self.solo);
        if old_draft != new_draft {
            if let Some(key) = old_draft {
                self.drafts.insert(key, std::mem::take(&mut self.input));
            }
            self.input = new_draft.and_then(|key| self.drafts.remove(&key)).unwrap_or_default();
            self.suggest = 0;
        }
        self.drafts.retain(|key, _| match key {
            ChatDraft::Solo(Some(id)) => Some(*id) == self.solo,
            ChatDraft::Zoom(id) => self.agents.contains_key(id),
            _ => true,
        });
        let jump = matches!((self.screen, s), (Screen::Stage, Screen::Zoom(_)) | (Screen::Zoom(_), Screen::Stage));
        if jump && crate::ui::theme::motion() {
            self.flash_screen = Some(Instant::now());
        }
        if s != self.screen {
            // an edit and a first Delete belong to the screen they were made on; so does a key
            // still being typed (a probe already under way delivers its models wherever one is)
            self.edit = None;
            self.confirm = None;
            if self.models_ui.keyflow.as_ref().is_some_and(|k| k.probing.is_none()) {
                self.models_ui.keyflow = None;
            }
        }
        self.screen = s;
    }

    /// Open a config screen remembering the screen underneath it, so `leave_screen` can put the
    /// user back. A Studio→picker→Models hop must not record Studio as the way back (leaving
    /// would strand the user on a screen they already left), so only a returnable screen is kept.
    pub fn enter_screen(&mut self, s: Screen) {
        if !self.screen.is_config() {
            self.screen_before = Some(self.screen);
        }
        self.set_screen(s);
    }

    /// `/settings`: what the screen offers is read as it opens — the patterns there are, and the
    /// MCP servers as settings.toml has them now (`mantra mcp` may have changed them meanwhile).
    pub fn open_settings(&mut self) {
        self.refresh_mcp_servers();
        self.settings_ui.patterns = Pattern::list(&self.project);
        self.enter_screen(Screen::Settings);
    }

    /// Leave a config screen for wherever it was opened from. Two cases have nothing to return
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
            Screen::Models | Screen::Settings => None,
        }
    }

    /// The text composer belongs to the planner throughout a run; card focus still controls
    /// inspection, model changes and interrupts for the selected agent.
    pub fn composer_agent(&self) -> Option<AgentId> {
        match self.screen {
            Screen::Stage => self.run.as_ref().and_then(|r| r.planner),
            Screen::Zoom(id) if self.in_run(id) => self.run.as_ref().and_then(|r| r.planner),
            _ => self.focus_agent(),
        }
    }

    /// The selectable agents of the stage, in card order: the fixed roles, one node per task,
    /// then the gate and the finale step. `sel` indexes this, so it must count exactly what the
    /// overview draws: a task retried twice has one card (the newest attempt — `stage::phase`'s
    /// `worker_of`), and the selection must land on the attempt the card shows, never on a
    /// cancelled earlier one that would zoom into the wrong log or make ↓ step through
    /// same-named cards. Earlier attempts stay reachable through the zoomed view's log and the
    /// pulse. Tasks of the current phase come in plan order; tasks outside it (ad-hoc fix workers,
    /// the finale's fixes) follow in the order they were started.
    pub fn stage_nodes(&self) -> Vec<AgentId> {
        let Some(r) = &self.run else { return vec![] };
        let mut v = vec![];
        v.extend(r.planner);
        v.extend(r.manager);
        v.extend(r.orchestrator);
        let mut tasks: Vec<&str> = r.current_phase().map(|p| p.tasks.iter().map(|t| t.id.as_str()).collect()).unwrap_or_default();
        for w in &r.workers {
            if !tasks.contains(&w.task.id.as_str()) {
                tasks.push(&w.task.id);
            }
        }
        for id in tasks {
            // a newest attempt still queued (no agent yet) has no node, like its card
            let newest = r.workers.iter().rev().find(|w| w.task.id == id).and_then(|w| w.agent);
            if let Some(a) = newest {
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

    /// What `effort = "auto"` means for this agent right now: its role's job in the open run (a
    /// finale step is `high`), else `medium`.
    pub fn auto_effort_for(&self, a: AgentId) -> &'static str {
        if let Some(r) = &self.run {
            if let Some(role) = r.role_name_of(a) {
                let kind = r.pattern.role(&role).map(|x| x.kind.clone()).unwrap_or_default();
                let finale = matches!(r.stage, Stage::Finale { .. }) && Some(a) == r.finale_agent;
                return crate::engine::pattern::auto_effort(&kind, &role, finale);
            }
        }
        "medium"
    }

    pub fn set_effort(&mut self, a: AgentId, e: &str) {
        // "auto" is resolved for this agent first, then clamped to what its model offers like any other
        let e = if e.trim().eq_ignore_ascii_case(crate::engine::pattern::EFFORT_AUTO) { self.auto_effort_for(a) } else { e };
        let Some(ag) = self.agents.get_mut(&a) else { return };
        let m = self.registry.resolve(&ag.model_alias);
        let (e, lowered) = m.resolve_effort_lowered(e);
        ag.effort = e.clone();
        self.hub.send(a, Cmd::SetEffort(e.clone()));
        let name = ag.name.clone();
        if lowered {
            self.toast(format!("{name}: {} tops out at {e} — effort → {e} (next turn)", m.alias), Level::Info);
        } else {
            self.toast(format!("{name}: effort → {e} (next turn)"), Level::Info);
        }
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

    /// Switch a live agent in place. A provider/backend change gets a fresh compatible session
    /// and an explicit context handoff; the logical agent, launch policy and pending text survive.
    pub fn set_model(&mut self, a: AgentId, alias: &str) -> Result<(), String> {
        let Some(ag) = self.agents.get(&a) else { return Ok(()) };
        if self.registry.get(alias).is_none() {
            return Err("unknown model".into());
        }
        if let Some(p) = self.registry.alias_problem(alias) {
            return Err(p);
        }
        let new = self.registry.resolve(alias);
        let old = self.registry.resolve(&ag.model_alias);
        let new_backend = self.registry.backend_of(&new);
        let effort = new.resolve_effort(&ag.effort);
        let context_window = self.registry.effective_context_window(&new.alias, &self.settings.default_model);
        // a run agent keeps the run's context cap (the backend was told it at spawn): the gauge
        // shows the window it really has, not the new model's whole one
        let cap = self.run.as_ref().filter(|r| r.all_agents().contains(&a)).map(|r| r.pattern.settings.context_cap).filter(|c| *c > 0);
        let context_window = context_window.min(cap.unwrap_or(u64::MAX));
        // Context limits are launch flags on each backend, so a change must reach the process
        // as well as its gauge. Reuse the fresh-session handoff for that model configuration.
        let incompatible = old.provider != new.provider || new_backend != ag.backend || (new_backend != crate::hub::Backend::Antigravity && ag.ctx_window != Some(context_window));
        if incompatible {
            let mut options = ag.launch_options.as_deref().cloned().ok_or_else(|| "agent launch policy is unavailable; respawn it before changing providers".to_string())?;
            options.model_alias = new.alias.clone();
            options.effort = Some(effort.clone());
            options.approval = ag.approval.clone();
            options.sandbox = ag.sandbox.clone();
            options.resume_thread = None;
            options.context_override = Some(context_window);
            let (mut spec, _, _) = agent_spec(&mut self.hub, &self.registry, &options);
            let handoff = ag.handoff_context();
            spec.developer_instructions.push_str(&format!("\n\n{handoff}"));
            if let Some(c) = spec.claude.as_mut() { c.system_prompt = spec.developer_instructions.clone(); }
            if let Some(b) = spec.bija.as_mut() { b.system_prompt = spec.developer_instructions.clone(); }
            if let Some(a) = spec.antigravity.as_mut() { a.system_prompt = spec.developer_instructions.clone(); }
            let command = match new_backend {
                crate::hub::Backend::Codex => self.hub.codex_cmd.clone(),
                crate::hub::Backend::ClaudeCode => self.hub.claude_cmd.clone(),
                crate::hub::Backend::Bija => self.hub.bija_cmd.clone(),
                crate::hub::Backend::Antigravity => self.hub.antigravity_cmd.clone(),
            };
            let continuing = ag.busy() && !ag.stopped_by_user;
            let resume_text = continuing.then(|| "[mantra:backend-switch] Continue the active task using the handoff in your instructions. Preserve completed work and pending user requests.".to_string());
            if let Some(run) = self.run.as_mut() { run.forget_agent_session(a); }
            self.hub.send(a, Cmd::Reconfigure { spec: Box::new(spec), command, resume_text });
            if let Some(ag) = self.agents.get_mut(&a) {
                ag.abandon_backend_turn();
                ag.launch_options = Some(Box::new(options));
                ag.backend = new_backend;
                ag.provider = new.provider.clone();
                ag.thread_id = None;
                ag.relaunching = true;
                ag.switching_backend = true;
                ag.turn_active = false;
                ag.awaiting_start = continuing;
                ag.turn_wait_since = continuing.then(Instant::now);
                ag.compacting = false;
                ag.compact_pending = false;
                ag.ctx_used = 0;
                ag.ctx_window = (new_backend != crate::hub::Backend::Antigravity).then_some(context_window);
                ag.ctx_warned = false;
                ag.retry_note = None;
                ag.status = Status::relaunching();
                ag.activity = "applying model configuration".into();
            }
            self.approvals.retain(|approval| approval.agent != a);
        } else {
            self.hub.send(a, Cmd::SetModel(new.model.clone()));
            self.hub.send(a, Cmd::SetEffort(effort.clone()));
            self.hub.send(a, Cmd::SetVision(self.registry.vision(&new.alias)));
        }
        if let Some(ag) = self.agents.get_mut(&a) {
            ag.model_alias = new.alias.clone();
            ag.model = new.model.clone();
            ag.effort = effort.clone();
            if let Some(options) = ag.launch_options.as_mut() {
                options.model_alias = new.alias.clone();
                options.effort = Some(effort);
                options.context_override = Some(context_window);
                options.approval = ag.approval.clone();
                options.sandbox = ag.sandbox.clone();
            }
            ag.ctx_window = (new_backend != crate::hub::Backend::Antigravity).then_some(context_window);
            ag.notice(Level::Info, format!("model → {} ({})", new.alias, new.model));
        }
        if Some(a) == self.solo {
            self.settings.default_model = new.alias.clone();
            self.save_settings();
        }
        Ok(())
    }

    /// After switching a run agent's model (e.g. from the halt-band `m` picker), persist the
    /// choice for *that agent* in the run's state (`Run::set_agent_model`): its next attempt or
    /// respawn and a resume keep it, the role's other agents are not touched. (A role-level
    /// change is `set_role_model`, one role or `all`, which writes the run's pattern copy.) A
    /// halted run stays halted: the user may want to change more than one model (or look at the
    /// error again) before going on, and resuming as a side effect of a settings change is how a
    /// run restarts unnoticed — so the toast says what is left to do.
    pub fn model_switched_for_run_agent(&mut self, a: AgentId) {
        if !self.run.as_ref().map(|r| r.all_agents().contains(&a)).unwrap_or(false) {
            return;
        }
        let alias = self.agents.get(&a).map(|x| x.model_alias.clone());
        let known = self.registry.context_windows_for_default(&self.settings.default_model);
        let sees = crate::engine::run::vision_aliases(&self.registry);
        if let (Some(run), Some(alias)) = (self.run.as_mut(), alias) {
            // the switch may bring an alias the run has not seen: refresh what it knows first
            run.set_known_models(known);
            run.set_vision_models(sees);
            run.set_agent_model(a, &alias);
        }
        if self.run.as_ref().map(|r| r.halted()).unwrap_or(false) {
            self.toast(MODEL_CHANGED_HALTED, Level::Info);
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
            Some(Ok(what)) => self.toast(what, Level::Info),
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
        self.start_run_with(goal, None, None);
    }

    /// `start_run` on `pattern` instead of the session's, and with `max_parallel` overriding the
    /// pattern's width for this run only (`--parallel N`): one number for every worker class —
    /// the shared pool and each role with a cap of its own (`Pattern::set_parallel`). It goes into
    /// the run's own copy of the pattern — its `pattern.toml`, which a resume reads back — and
    /// never into the saved pattern.
    pub fn start_run_with(&mut self, goal: &str, pattern: Option<&str>, max_parallel: Option<usize>) {
        if self.run.as_ref().map(|r| r.is_active()).unwrap_or(false) {
            self.toast("a run is already active — re-prompt it from the stage, or /pause", Level::Warn);
            return;
        }
        self.refresh_mcp_servers();
        let mut pattern = match Pattern::load(pattern.unwrap_or(&self.pattern_name), &self.project) {
            Ok(p) => p,
            Err(e) => {
                self.toast(format!("pattern error: {e}"), Level::Error);
                return;
            }
        };
        // The shipped patterns keep their familiar role aliases for existing installations.
        // A new installation has no model presets, so bind only missing built-in aliases to
        // the user's chosen model in this run's saved pattern copy. User-edited patterns retain
        // exact role names and still get a clear preflight error for a typo.
        let mut rebound_roles = 0;
        if !self.demo && matches!(pattern.tag(), "default" | "built-in") {
            let usable = self.registry.models.iter()
                .find(|m| m.alias == self.settings.default_model && self.registry.alias_problem(&m.alias).is_none())
                .or_else(|| self.registry.models.iter().find(|m| self.registry.alias_problem(&m.alias).is_none()))
                .map(|m| m.alias.clone());
            if let Some(alias) = usable {
                for role in pattern.roles.values_mut() {
                    if self.registry.get(&role.model).is_none() {
                        role.model = alias.clone();
                        rebound_roles += 1;
                    }
                    if role.fallback.as_deref().is_some_and(|f| !f.is_empty() && self.registry.get(f).is_none()) {
                        role.fallback = Some(alias.clone());
                    }
                }
            }
        }
        // what the journal says about an override: only when it changes a number
        let mut width_note = None;
        if let Some(n) = max_parallel {
            let (said, tasks_said) = (pattern.parallel_summary(), pattern.settings.max_tasks_per_phase);
            pattern.set_parallel(n);
            if pattern.parallel_summary() != said {
                let tasks = if n > tasks_said { format!(", and up to {n} tasks a phase (it says {tasks_said})") } else { String::new() };
                width_note = Some(format!("{n} at once in every worker class for this run (the pattern says {said}){tasks}"));
            }
        }
        if self.demo {
            // Every demo role gets the stand-in server (`mantra mock-mcp`, registered by main.rs)
            // and nothing else — a pattern of the user's that names a real one never starts it.
            for role in pattern.roles.values_mut() {
                role.mcp = vec![DEMO_MCP.to_string()];
            }
        }
        let problems = self.preflight(&pattern);
        if !problems.is_empty() {
            // L3: fail before spending a single turn, naming the role, provider and variable.
            // Shown where the user is looking: as a toast, and in the Solo log if one exists.
            if let Some(a) = self.solo.and_then(|s| self.agents.get_mut(&s)) {
                for p in &problems {
                    a.notice(Level::Warn, format!("cannot start the run — {p}"));
                }
            }
            self.toast(format!("cannot start: {}", problems[0]), Level::Error);
            return;
        }
        let mut run = Run::new(self.project.clone(), pattern, goal.to_string());
        if rebound_roles > 0 {
            run.log("⚙", "blue", format!("{rebound_roles} built-in roles use configured models; see this run's pattern copy"));
        }
        // the engine has no registry: what it knows about models (windows, which aliases exist)
        run.set_known_models(self.registry.context_windows_for_default(&self.settings.default_model));
        run.set_vision_models(crate::engine::run::vision_aliases(&self.registry));
        run.enable_akasha(self.settings.experimental_on(crate::config::EXPERIMENTAL_AKASHA));
        {
            let mut ctx = Ctxt { hub: &mut self.hub, agents: &mut self.agents, registry: &self.registry, settings: &self.settings, tx: &self.tx, notes: &mut self.notes };
            run.start(&mut ctx);
        }
        if let Some(w) = &self.sandbox_warning {
            run.log("⚠", "amber", format!("sandbox: {}", crate::util::trunc(w, 140)));
            run.log("⚠", "amber", "sandbox unavailable here: Codex agents run with danger-full-access; git worktrees still keep workers apart");
        }
        for line in mcp_missing_lines(&self.settings, &run.pattern) {
            run.log("⚠", "amber", line);
        }
        self.journal_bridge_problem(&mut run);
        if let Some(note) = width_note {
            run.log("⚙", "blue", note);
        }
        if let Some(n) = max_parallel.filter(|n| *n > crate::engine::pattern::PARALLEL_NOTE_ABOVE) {
            run.log("⚙", "amber", format!("{n} at once: the provider's rate limits are the ceiling now, not Mantra"));
        }
        self.run = Some(run);
        // The planner isn't spawned yet (workspace setup is an async job) — zoom to it once it
        // exists, in on_event's JobTag::Setup handling below, if the user hasn't navigated away.
        self.set_screen(Screen::Stage);
        self.sel = 0;
        self.sel_follow = None;
    }

    /// Stop the active run for good (`Run::stop`): its agents shut down, the run saved as
    /// "stopped" and openable read-only from `/runs` (to land or delete), and the way clear for
    /// the next `/run`. Says so when there is nothing to stop; returns whether it stopped one.
    pub fn stop_run(&mut self) -> bool {
        if !self.run.as_ref().map(|r| r.is_active()).unwrap_or(false) {
            self.toast("no active run to stop", Level::Info);
            return false;
        }
        let owners = self.run.as_ref().map(|r| r.all_agents()).unwrap_or_default();
        for owner in owners { self.stop_swarm_children(owner); }
        self.with_run(|r, c| r.stop(c));
        self.toast("run stopped — /run starts a new one; /runs still has this one", Level::Ok);
        true
    }

    /// Record a failed sandbox probe (done once at startup, never in demo mode). The welcome
    /// screens show it while they're up; every run's pulse repeats it (`start_run`) so it is on
    /// screen exactly where a `bwrap` failure would otherwise be puzzling.
    pub fn set_sandbox_warning(&mut self, why: String) {
        self.sandbox_warning = Some(why);
        SANDBOX_FALLBACK.store(true, std::sync::atomic::Ordering::Relaxed);
    }

    /// `/runs`: this project's runs, newest first. Runs of other projects are only counted — a
    /// run is resumed from its own project (`mantra runs resume <id>` switches there).
    pub fn open_runs(&mut self) {
        let all = crate::engine::state::list_all();
        let key = crate::engine::state::project_key(&self.project);
        let (list, other): (Vec<_>, Vec<_>) = all.into_iter().partition(|r| r.project_key == key);
        self.overlays.push(Overlay::Runs { sel: 0, list, confirm: false, others: other.len() });
    }

    /// Pick a saved run back up (see `engine::resume`). Refuses while another run is active, and
    /// a run another live Mantra has open (`state::live_owner`): two owners of one run would
    /// each save `state.json` over the other's. A dead owner is a crash, and resumes normally.
    pub fn resume_run(&mut self, r: crate::engine::state::RunSummary) {
        if self.run.as_ref().map(|x| x.is_active()).unwrap_or(false) {
            self.toast("a run is already active — finish or /pause it first", Level::Warn);
            return;
        }
        if let Some(owner) = crate::engine::state::live_owner(&r.dir) {
            self.toast(owner.refusal(&r.id), Level::Error);
            return;
        }
        self.refresh_mcp_servers();
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
        let problems = self.preflight(&pattern);
        if let Some(p) = problems.first() {
            self.toast(format!("cannot resume: {p}"), Level::Error);
            return;
        }
        let plan = std::fs::read_to_string(r.dir.join("plan.json")).ok().and_then(|s| serde_json::from_str::<crate::engine::plan::Plan>(&s).ok());
        let label = crate::engine::state::stage_label(&st.stage);
        let mut run = Run::from_state(&st, pattern, plan, r.dir.clone());
        // before the boot: it decides which saved per-agent models still exist in the registry
        run.set_known_models(self.registry.context_windows_for_default(&self.settings.default_model));
        run.set_vision_models(crate::engine::run::vision_aliases(&self.registry));
        // before the boot too: the members it re-attaches get the akasha tools back
        run.enable_akasha(self.settings.experimental_on(crate::config::EXPERIMENTAL_AKASHA));
        {
            let mut ctx = Ctxt { hub: &mut self.hub, agents: &mut self.agents, registry: &self.registry, settings: &self.settings, tx: &self.tx, notes: &mut self.notes };
            run.resume_boot(&mut ctx, &st);
        }
        self.journal_bridge_problem(&mut run);
        self.run = Some(run);
        self.unfinished_runs.retain(|id| *id != r.id);
        self.set_screen(Screen::Stage);
        self.sel = 0;
        self.sel_follow = None;
        self.toast(format!("resumed {} at {label}", r.id), Level::Ok);
    }

    /// Delete a saved run (branches, worktrees, journal). The run that is open right now can
    /// only be deleted once it is finished, and one another live Mantra has open not at all:
    /// its worktrees are that process's working copies.
    pub fn delete_run(&mut self, r: &crate::engine::state::RunSummary) -> bool {
        if let Some(owner) = crate::engine::state::live_owner(&r.dir) {
            self.toast(owner.delete_refusal(&r.id), Level::Error);
            return false;
        }
        if let Some(cur) = &self.run {
            if cur.id == r.id {
                if cur.is_active() {
                    self.toast("this run is still active — stop it first, then delete it", Level::Warn);
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

    fn start_architect(&mut self) -> Option<AgentId> {
        if let Some(a) = self.studio.architect {
            return Some(a);
        }
        let model = if self.registry.get("astra").is_some() { "astra".to_string() } else { self.settings.default_model.clone() };
        if self.registry.get(&model).is_some_and(|m| self.registry.backend_of(m) == crate::config::ProviderKind::Antigravity) {
            self.toast("Studio's architect requires read-only access, which Antigravity cannot enforce. Choose another backend for astra or the default model; Antigravity remains available for writable pattern roles.", Level::Warn);
            return None;
        }
        let id = spawn_agent(
            &mut self.hub,
            &mut self.agents,
            &self.registry,
            AgentOpts {
                name: "architect".into(),
                role: "architect".into(),
                glyph: "★".into(),
                color: "saffron".into(),
                model_alias: model.clone(),
                effort: Some("high".into()),
                cwd: self.project.clone(),
                approval: "never".into(),
                sandbox: "read-only".into(),
                instructions: tools::ARCHITECT_PROMPT.into(),
                tools: tools::architect_tools(),
                extra_writable: vec![],
                network: false,
                writable_roots: vec![],
                context_override: Some(self.registry.effective_context_window(&model, &self.settings.default_model)),
                resume_thread: None,
                thread_name: None,
                mcp: vec![],
            },
        );
        self.studio.architect = Some(id);
        Some(id)
    }

    /// The `/models` `t` live test. Dispatched per backend so a provider that can't actually run a
    /// role (F4: a model rejecting Codex's `developer` role) fails here, before a real run — not the
    /// job the user is running it for.
    pub fn probe_model(&mut self, alias: &str) {
        if self.registry.get(alias).is_some_and(|m| self.registry.backend_of(m) == crate::config::ProviderKind::Antigravity) {
            let why = "Mantra model tests require read-only access, which Antigravity cannot enforce; check the account with `agy models` and use an available model in a writable role";
            self.models_ui.status.insert(alias.to_string(), format!("error: {why}"));
            self.toast(format!("not tested — {why}"), Level::Warn);
            return;
        }
        // A draft provider (placeholder URL, no credential — `Registry::probe_problem`) never
        // gets a request: the row says why instead of a Codex process failing with a key that
        // isn't there (or, worse, succeeding against the placeholder). Not in demo mode: the
        // mock backend never touches the network (the same exemption as `model_problem`).
        if let Some(why) = (!self.demo).then(|| self.registry.probe_problem(alias)).flatten() {
            self.models_ui.status.insert(alias.to_string(), format!("error: {why}"));
            self.toast(format!("not tested — {why}"), Level::Warn);
            return;
        }
        // Every backend is tested the same way: a throwaway agent on the alias, given a role-like
        // system prompt (Codex's developer instructions, Claude's appended prompt, bija's
        // `system.md`), asked for one word — `spawn_agent` picks the process for the backend.
        self.probe_model_with_agent(alias);
    }

    fn probe_model_with_agent(&mut self, alias: &str) {
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
                network: false,
                writable_roots: vec![],
                context_override: Some(self.registry.effective_context_window(&alias, &self.settings.default_model)),
                resume_thread: None,
                thread_name: None,
                mcp: vec![],
            },
        );
        self.probes.insert(id, (alias.to_string(), Instant::now()));
        // Name the destination while the request is out, so the user sees where the key went.
        let dest = self.registry.provider_of(alias).and_then(|p| p.host()).map(|h| format!(" {} {h}", crate::ui::theme::g("→", "->"))).unwrap_or_default();
        self.models_ui.status.insert(alias.to_string(), format!("testing{dest}…"));
        prompt_agent(&self.hub, &mut self.agents, id, "Reply with exactly: OK".into(), true, Send::Auto);
    }

    /// Discover models: every custom provider, and Codex's catalog when its account has models
    /// (`only` = one provider id). Without Codex nothing asks for it: it is an add-on backend.
    pub fn discover(&mut self, only: Option<String>) {
        let mut sources: Vec<String> = vec![];
        let codex = only.is_none() && !self.registry.codex_account_models().is_empty();
        if codex {
            sources.push("codex".into());
        }
        let provs: Vec<crate::config::ProviderEntry> = self.registry.providers.iter().filter(|p| !p.id.is_empty() && p.id != "openai" && only.as_ref().map(|o| *o == p.id).unwrap_or(true)).cloned().collect();
        // ClaudeCode providers have no `/models` endpoint for a subscription; the built-in defaults
        // (config::CLAUDE_MODELS) are always known locally and answer instantly, with no loading spinner needed. A
        // `base_url` (a third-party gateway) additionally gets queried live, exactly like a Codex
        // custom provider (§10.5) — and so does every Bija provider, with its key when it has one.
        // Drafts (placeholder/malformed URL, or no credential — `ProviderEntry::draft_reason`)
        // never go on the network: they are listed as skipped instead of queried.
        let (drafts, net_provs): (Vec<_>, Vec<_>) = provs.iter().filter(|p| p.needs_url()).cloned().partition(|p| p.draft_reason().is_some());
        sources.extend(net_provs.iter().map(|p| p.id.clone()));
        let hosts = net_provs.iter().filter_map(|p| p.host().map(|h| (p.id.clone(), h))).collect();
        let mut skipped: Vec<String> = drafts.iter().map(|p| format!("{}: skipped (draft) — {}", p.id, p.draft_reason().unwrap_or_default())).collect();
        skipped.extend(provs.iter().filter(|p| p.kind == crate::config::ProviderKind::Antigravity).map(|p| format!("{}: run `agy models`, then add an available model ID by hand", p.id)));
        self.overlays.push(Overlay::Discover(DiscoverState { items: vec![], loading: sources, hosts, errors: vec![], skipped, sel: 0, filter: Input::default(), origin: None }));
        for p in provs.iter().filter(|p| p.kind == crate::config::ProviderKind::ClaudeCode) {
            self.on_discovered(p.id.clone(), Ok(crate::discover::claude_defaults(&p.id)));
        }
        if codex {
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
                // The key is strictly this provider's own (`resolve_key` on the entry being
                // queried) — never another provider's, never a global one.
                let r = crate::discover::list_models_with_headers(&p.base_url, p.resolve_key().as_deref(), crate::discover::key_header(&p), &p.headers).map(|found| found.iter().map(|f| crate::discover::from_provider(&p.id, f)).collect());
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
        match self.save_registry() {
            Ok(()) => match &st.origin {
                Some(host) => self.toast(format!("{host}: {added} model{} added", if added == 1 { "" } else { "s" }), Level::Ok),
                None => self.toast(format!("added {added}, updated {updated} model(s) — saved to models.toml"), Level::Ok),
            },
            Err(e) => self.toast(format!("added {added}, updated {updated}, but {e}"), Level::Error),
        }
    }

    // ─── approvals ───

    /// Save the Solo approval preference. Codex applies it to native tools; all backends
    /// apply it to Mantra requests. Run agents retain their own role permission.
    pub fn set_approval_mode(&mut self, mode: &str) {
        self.settings.approval_mode = mode.to_string();
        self.save_settings();
        if let Some(s) = self.solo {
            let live = self.agents.get(&s).map(|a| a.approval.as_str()).unwrap_or("");
            if self.agents.get(&s).is_some_and(|a| a.backend == crate::config::ProviderKind::Antigravity && mode != live) {
                self.toast(format!("permission saved: {mode} · /new applies Antigravity's native permission flags; this session keeps {live}"), Level::Warn);
                return;
            }
            if (mode == crate::config::BYPASS_SANDBOX_DANGER) != (live == crate::config::BYPASS_SANDBOX_DANGER) {
                self.toast(format!("permission saved: {mode} · /new applies the sandbox change; this session keeps {live}"), Level::Warn);
                return;
            }
            if self.agents.get(&s).is_some_and(|a| a.backend == crate::config::ProviderKind::Codex) {
                self.hub.send(s, Cmd::SetApproval(mode.to_string()));
            }
            if let Some(a) = self.agents.get_mut(&s) {
                a.approval = mode.to_string();
            }
        }
        let label = match mode {
            "never" => "never ask",
            crate::config::BYPASS_SANDBOX_DANGER => "BYPASS-SANDBOX-DANGER · no sandbox, never ask",
            "untrusted" => "ask for anything untrusted",
            _ => "ask when needed",
        };
        let pending = self.approvals.iter().filter(|p| self.agents.get(&p.agent).is_some_and(|a| crate::config::approval_policy(&a.approval) == "never")).count();
        if crate::config::approval_policy(mode) == "never" && pending > 0 {
            self.auto_approve_pending();
            self.toast(format!("approvals: {label} — {pending} waiting request(s) approved"), Level::Info);
        } else {
            self.toast(format!("approvals: {label}"), Level::Info);
        }
    }

    fn auto_approve_pending(&mut self) {
        // Each request keeps its own agent's policy. One unattended worker must never
        // grant permissions or answer a question pending for a different agent.
        while let Some(idx) = self.approvals.iter().position(|p| self.agents.get(&p.agent).is_some_and(|a| crate::config::approval_policy(&a.approval) == "never")) {
            let q = self.approvals[idx].method == "item/tool/requestUserInput";
            let answer = q.then(|| "No human is available right now (approval mode: never ask). Proceed with your best judgment and state any assumptions.".to_string());
            self.resolve_approval_ex(idx, if q { 0 } else { 1 }, answer, true);
        }
    }

    // ─── settings ───

    /// One change from `/settings` or the web's settings page (`Field::key`, the value as text):
    /// `Settings::set_field` validates it, `apply_settings` saves and applies it. `Ok` says when
    /// it takes effect ("" = now), `Err` why it was refused — nothing changed then. The MCP servers
    /// are read from settings.toml first: `solo_mcp` may name one `mantra mcp add` just made.
    pub fn set_setting(&mut self, key: &str, value: &str) -> Result<&'static str, String> {
        self.refresh_mcp_servers();
        let mut s = self.settings.clone();
        s.set_field(key, value)?;
        self.apply_settings(s)?;
        if key == "approval_mode" && self.solo.and_then(|id| self.agents.get(&id)).is_some_and(|a| a.approval != self.settings.approval_mode) {
            return Ok("Solo's next session (/new) uses it; this session keeps its current permission");
        }
        Ok(if applied_live(key) { "" } else { crate::settings_ops::takes_effect(key) })
    }

    /// Makes `s` the session's settings — refused when its model is not in /models or its
    /// pattern does not load — saves them (not in the demo, `save_settings`) and applies what
    /// can apply live: the approval mode to Solo's thread, the pattern for new runs, the side
    /// panel, the glyphs, colours and motion (`theme::init` again, the screen redrawn whole; the
    /// event loop picks up `fps` and `mouse` itself). The rest is read where it is used: the next
    /// Solo session, the next run, the next start.
    pub fn apply_settings(&mut self, s: Settings) -> Result<(), String> {
        if self.registry.get(&s.default_model).is_none() {
            return Err(format!("model alias '{}' is not in /models", s.default_model));
        }
        if s.default_pattern != self.settings.default_pattern {
            Pattern::load(&s.default_pattern, &self.project).map_err(|e| e.to_string())?;
            self.pattern_name = s.default_pattern.clone();
        }
        if s.side_panel != self.settings.side_panel {
            self.side_panel = s.side_panel;
        }
        let looks = (s.glyphs != self.settings.glyphs || s.colors != self.settings.colors || s.reduce_motion != self.settings.reduce_motion).then(|| s.clone());
        let approvals = (s.approval_mode != self.settings.approval_mode).then(|| s.approval_mode.clone());
        self.settings = s;
        if let Some(s) = looks {
            crate::ui::theme::init(&s);
            self.force_clear = true;
        }
        match approvals {
            Some(mode) => self.set_approval_mode(&mode),
            None => self.save_settings(),
        }
        Ok(())
    }

    /// The effort a new Solo session starts its model on: the model's `default_effort` in
    /// models.toml, which `/settings` edits beside the model.
    pub fn set_default_effort(&mut self, effort: &str) -> Result<(), String> {
        let alias = self.registry.resolve(&self.settings.default_model).alias;
        let Some(m) = self.registry.models.iter_mut().find(|m| m.alias == alias) else { return Err(format!("model alias '{alias}' is not in /models")) };
        if !m.efforts().iter().any(|e| e == effort) {
            return Err(format!("{}: one of {}", m.alias, m.efforts().join(", ")));
        }
        m.default_effort = effort.to_string();
        self.save_registry()
    }

    // ─── MCP servers ───

    /// Changes the MCP servers the way `mantra mcp` does: in settings.toml as it is on disk —
    /// `save_settings` writes the file's table, not the session's — then takes the session's copy
    /// (and `solo_mcp`, which a removal may shorten) from what was saved. The demo's servers live
    /// in memory only, and change there.
    pub(crate) fn change_mcp(&mut self, f: impl FnOnce(&mut Settings) -> Result<(), String>) -> Result<(), String> {
        if self.demo {
            return f(&mut self.settings);
        }
        let mut disk = Settings::load_on_disk()?;
        f(&mut disk)?;
        disk.save().map_err(|e| format!("could not save settings.toml: {e}"))?;
        self.settings.mcp_servers = disk.mcp_servers;
        self.settings.solo_mcp = disk.solo_mcp;
        Ok(())
    }

    /// A new MCP server from its name and its command line as typed (`uvx blender-mcp`).
    pub fn mcp_add_server(&mut self, name: &str, command_line: &str) -> Result<(), String> {
        let words = crate::settings_ops::parse_command_line(command_line)?;
        let Some((command, args)) = words.split_first() else { return Err(format!("{}: a server needs a command", name.trim())) };
        let server = crate::config::McpServer { command: command.clone(), args: args.to_vec(), ..Default::default() };
        self.change_mcp(|s| s.mcp_add(name.trim(), server))
    }

    /// Removes a server, and its name from `solo_mcp` with it.
    pub fn mcp_remove_server(&mut self, name: &str) -> Result<(), String> {
        self.settings_ui.mcp_status.remove(name);
        self.change_mcp(|s| {
            s.mcp_remove(name)?;
            s.solo_mcp.retain(|n| n != name);
            Ok(())
        })
    }

    pub fn mcp_set_server_enabled(&mut self, name: &str, on: bool) -> Result<(), String> {
        self.change_mcp(|s| s.mcp_set_enabled(name, on))
    }

    /// Starts the server, asks for its tools and stops it, off the event loop; the line for its
    /// row arrives as `AppEvent::McpTested`.
    pub fn mcp_test_server(&mut self, name: &str) {
        let Some(server) = self.settings.mcp_servers.get(name).cloned() else { return };
        self.settings_ui.mcp_status.insert(name.to_string(), "testing…".into());
        let (tx, name) = (self.tx.clone(), name.to_string());
        tokio::spawn(async move {
            let result = mcp_probe(&server).await;
            let _ = tx.send(AppEvent::McpTested { name, result });
        });
    }

    // ─── providers & backends ───

    /// A provider from its API key and base URL (`provider_key::provider_from_key`): added — or
    /// the key put into the provider already at that URL — and models.toml saved. Returns its id.
    /// The key goes into models.toml (0600) and nowhere else: not into a toast, not into the log.
    pub fn add_provider_from_key(&mut self, key: &str, base_url: &str) -> Result<String, String> {
        let draft = crate::provider_key::provider_from_key(key, Some(base_url));
        let id = crate::provider_key::add_provider(&mut self.registry, &draft)?;
        self.save_registry()?;
        Ok(id)
    }

    /// Asks provider `id` for its models off the event loop; `AppEvent::Probed` brings the answer
    /// (`on_probed` offers them for import). Returns the host asked, for the spinner line.
    pub fn probe_provider(&mut self, id: &str) -> Option<String> {
        let p = self.registry.providers.iter().find(|p| p.id == id)?.clone();
        let host = p.host().unwrap_or_else(|| p.base_url.clone());
        let (tx, provider, h) = (self.tx.clone(), p.id.clone(), host.clone());
        tokio::task::spawn_blocking(move || {
            let result = crate::provider_key::probe(&p);
            let _ = tx.send(AppEvent::Probed { provider, host: h, result });
        });
        Some(host)
    }

    /// A provider's models are in: the discovery picker with them (a short list pre-selected), or
    /// why there are none. The provider stays either way — models can be added to it by hand.
    fn on_probed(&mut self, provider: String, host: String, result: Result<Vec<crate::discover::Found>, String>) {
        self.models_ui.keyflow = None;
        match result {
            Ok(found) if found.is_empty() => self.toast(format!("{host} lists no models — add them by hand (ctrl+n)"), Level::Info),
            Ok(found) => {
                let items = crate::provider_key::candidates(&self.registry, &provider, &found);
                self.overlays.push(Overlay::Discover(DiscoverState { items, loading: vec![], hosts: vec![], errors: vec![], skipped: vec![], sel: 0, filter: Input::default(), origin: Some(host) }));
            }
            Err(e) => {
                // a provider's error page may quote the key it was sent
                let key = self.registry.providers.iter().find(|p| p.id == provider).and_then(|p| p.api_key.clone()).unwrap_or_default();
                let e = if key.trim().len() >= 8 { e.replace(key.trim(), &crate::ui::form::mask(&key)) } else { e };
                self.toast(format!("{host}: {e} — the provider is kept"), Level::Warn);
            }
        }
    }

    /// Codex or Claude Code as an add-on backend: its default models, once its CLI is on PATH (in
    /// the demo the mocks stand in for both). Saved; returns how many models it added.
    pub fn add_backend(&mut self, backend: crate::provider_key::Backend) -> Result<usize, String> {
        let added = if backend == crate::provider_key::Backend::Antigravity {
            crate::provider_key::add_backend_with(&mut self.registry, backend, crate::config::antigravity_command_available(&self.settings.antigravity_command))?
        } else if self.demo { crate::provider_key::add_backend_with(&mut self.registry, backend, true)? } else { crate::provider_key::add_backend(&mut self.registry, backend)? };
        // A subscription backend can add its provider before any model is chosen.
        self.save_registry()?;
        Ok(added)
    }

    /// Every model on Codex's own account, gone in one go — the Codex row's Delete. Saved.
    pub fn remove_codex_account(&mut self) -> Result<usize, String> {
        let n = self.registry.remove_codex_account();
        if n > 0 {
            self.save_registry()?;
        }
        Ok(n)
    }

    /// models.toml written, the `/models` header told — except in the demo, whose registry points
    /// bija at its mock server and holds the mock backends' models: like its settings it is never
    /// saved, and a change there lives until the demo ends.
    pub fn save_registry(&mut self) -> Result<(), String> {
        if self.demo {
            self.models_ui.dirty = false;
            return Ok(());
        }
        match self.registry.save() {
            Ok(()) => {
                self.models_ui.dirty = false;
                self.models_ui.saved = Some(Instant::now());
                Ok(())
            }
            Err(e) => {
                self.models_ui.dirty = true;
                Err(format!("could not save models.toml: {e}"))
            }
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
            AppEvent::ToolAnswer { agent, id, result } => self.hub.send(agent, Cmd::Respond { id, result }),
            AppEvent::Probed { provider, host, result } => self.on_probed(provider, host, result),
            AppEvent::McpTested { name, result } => {
                let line = match result {
                    Ok(ok) => ok,
                    Err(e) => format!("error: {e}"),
                };
                if self.settings.mcp_servers.contains_key(&name) {
                    self.settings_ui.mcp_status.insert(name, line);
                }
            }
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
        if self.run.is_some() {
            self.follow_respawned_task();
            let n = self.stage_nodes().len();
            if n > 0 && self.sel >= n {
                self.sel = n - 1;
            }
            self.select_halted_agent();
        }
    }

    /// `sel_follow`: once the respawned task's newest attempt has an agent, select its node.
    fn follow_respawned_task(&mut self) {
        let Some(task) = &self.sel_follow else { return };
        let Some(r) = &self.run else { return };
        let Some(newest) = r.workers.iter().rev().find(|w| &w.task.id == task) else {
            self.sel_follow = None;
            return;
        };
        if let Some(a) = newest.agent {
            if let Some(i) = self.stage_nodes().iter().position(|n| *n == a) {
                self.sel = i;
            }
            self.sel_follow = None;
        }
    }

    /// A new halt about one agent selects that agent on the stage, once: the band's
    /// "m switch model for <agent>" (and `r`) then act on the agent it names, not on whatever
    /// was selected before. Moving the selection afterwards is left alone.
    fn select_halted_agent(&mut self) {
        let Some(h) = self.run.as_ref().and_then(|r| r.halt.as_ref()) else { return };
        if self.halt_seen == Some(h.since) {
            return;
        }
        let (since, agent) = (h.since, h.agent);
        if let Some(i) = agent.and_then(|a| self.stage_nodes().iter().position(|n| *n == a)) {
            self.sel = i;
        }
        self.halt_seen = Some(since);
    }

    fn on_hub(&mut self, ev: HubEvent) {
        let id = match &ev {
            HubEvent::Ready { agent, .. } | HubEvent::Notif { agent, .. }
            | HubEvent::Request { agent, .. } | HubEvent::CmdFailed { agent, .. }
            | HubEvent::Crashed { agent, .. } | HubEvent::Exited { agent }
            | HubEvent::BridgeConn { agent, .. } => *agent,
        };
        // Shutdown races messages already in the event queue. They must not turn a deliberately
        // ended process back into a busy/restarting card or execute a late tool request.
        if !self.hub.has_agent(id) && self.agents.get(&id).is_some_and(|a| a.stopped_by_user && a.status == Status::Stopped) {
            return;
        }
        // The old backend may already have queued a failure or tool request when a switch
        // was selected. Its replacement owns the same logical id; those events are obsolete.
        if self.agents.get(&id).is_some_and(|a| a.switching_backend)
            && matches!(&ev, HubEvent::Notif { .. } | HubEvent::Request { .. } | HubEvent::CmdFailed { .. }) {
            return;
        }
        match ev {
            HubEvent::Ready { agent, thread_id, model, resumed } => {
                let mut was_busy = false;
                if let Some(a) = self.agents.get_mut(&agent) {
                    // A relaunch Mantra asked for lost nothing (the hub replays what it held), so
                    // it never earns the "continue where you left off" note below.
                    was_busy = (a.turn_active || a.awaiting_start) && !a.relaunching;
                    a.relaunching = false;
                    a.switching_backend = false;
                    a.thread_id = Some(thread_id);
                    if !model.is_empty() {
                        a.model = model;
                    }
                    if resumed {
                        a.turn_active = false;
                        a.awaiting_start = false;
                        a.turn_wait_since = None;
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
                            warn = Some(format!("context {p}% full — /compact frees space (the agent also auto-compacts)"));
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
                        a.turn_wait_since = None;
                        a.status = Status::Failed(error.clone());
                        a.notice(Level::Error, format!("couldn't start the turn: {error}"));
                    }
                    if self.in_run(agent) {
                        self.with_run(|r, c| r.on_turn_done(c, agent, "failed", Some(error.clone()), Some(crate::agent::ErrKind::Transient)));
                    }
                    if self.solo_children.contains_key(&agent) {
                        self.on_turn_done(agent, "failed", Some(error), Some(crate::agent::ErrKind::Transient));
                    } else if self.swarm_owners.contains_key(&agent) {
                        self.swarm_child_ended(agent, "failed", Some(&error));
                    }
                }
                "settings" => crate::mlog!("agent {agent}: thread/settings/update not supported ({error}); new policy applies from the next turn"),
                // naming a thread is a nicety: an older app-server without `thread/name/set` is no reason to bother anyone
                "name" => crate::mlog!("agent {agent}: thread/name/set not supported ({error})"),
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
                // `attempt == 0` with `restarting` is the hub's relaunch notice (`Exit::Restarting`):
                // Mantra asked for it, nothing crashed.
                let relaunch = restarting && attempt == 0;
                if let Some(a) = self.agents.get_mut(&agent) {
                    a.status = if relaunch { Status::relaunching() } else if restarting { Status::restarting(attempt) } else { Status::Crashed(reason.clone()) };
                    a.activity = if restarting { "restarting".into() } else { "crashed".into() };
                    let what = match a.backend {
                        crate::config::ProviderKind::ClaudeCode => "claude process",
                        crate::config::ProviderKind::Bija => "bija process",
                        crate::config::ProviderKind::Codex => "codex process",
                        crate::config::ProviderKind::Antigravity => "agy process",
                    };
                    if relaunch {
                        // A model/effort change waits for the turn to end and the hub replays any
                        // held turn itself; only a plain mid-turn restart loses work worth a nudge.
                        a.relaunching = a.switching_backend || reason.starts_with("restarting to apply");
                        a.notice(Level::Info, format!("{what} relaunching: {reason}"));
                    } else {
                        // The hub's reason already names the backend ("bija exited (code 0)"):
                        // say it once, then what happens next.
                        let next = if restarting { "restarting" } else { "keeps crashing — press r (stage) or /new" };
                        let named = reason.starts_with(&format!("{} exited", what.trim_end_matches(" process")));
                        a.notice(Level::Error, if named { format!("{reason} — {next}") } else { format!("{what} {}: {reason}", if restarting { "crashed — restarting" } else { "keeps crashing — press r (stage) or /new" }) });
                    }
                }
                if self.in_run(agent) {
                    self.with_run(|r, c| r.on_crash(c, agent, &reason, restarting));
                }
                if self.solo_children.contains_key(&agent) && !(restarting && attempt == 0) {
                    self.fail_solo_waiters(agent, &format!("Subagent {agent} crashed: {reason}"));
                }
                if self.swarm_owners.contains_key(&agent) && !restarting {
                    self.swarm_child_ended(agent, "crashed", Some(&reason));
                }
                if Some(agent) == self.solo && !restarting {
                    self.toast(format!("Solo agent is down: {} — /new to retry (full error in the log)", crate::util::trunc(&reason, 160)), Level::Error);
                }
            }
            HubEvent::Exited { agent } => {
                if let Some(a) = self.agents.get_mut(&agent) {
                    a.status = Status::Stopped;
                    a.turn_active = false;
                }
                if self.solo_children.contains_key(&agent) {
                    self.fail_solo_waiters(agent, &format!("Subagent {agent} exited."));
                }
                if self.swarm_owners.contains_key(&agent) {
                    self.swarm_child_ended(agent, "exited", None);
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
            // A worker's workspace can be merged immediately after its final turn. Hold that
            // completion while delegated members write; wake the owner to review all results.
            let short_lived = self.run.as_ref().and_then(|r| r.role_name_of(agent).and_then(|n| r.pattern.role(&n).map(|role| role.kind == "worker" || role.kind == "gate"))).unwrap_or(false);
            if short_lived && status == "completed" && self.swarm_pending(agent) {
                self.swarm_deferred.insert(agent);
                if let Some(a) = self.agents.get_mut(&agent) { a.swarm_waiting = true; a.activity = "waiting for Akasha swarm".into(); }
                return;
            }
            if short_lived && self.swarms.contains_key(&agent) { self.stop_swarm_children(agent); }
            let st = status.to_string();
            self.with_run(|r, c| r.on_turn_done(c, agent, &st, error.clone(), kind));
            if self.run.as_ref().is_some_and(|r| !r.is_active()) {
                let owners = self.run.as_ref().map(|r| r.all_agents()).unwrap_or_default();
                for owner in owners { self.stop_swarm_children(owner); }
            }
        }
        self.swarm_child_ended(agent, status, error.as_deref());
        if self.solo_children.contains_key(&agent) {
            if let Some(waiters) = self.solo_waiters.remove(&agent) {
                let answer = self.solo_child_answer(agent, status, error.as_deref());
                for (parent, request) in waiters {
                    self.respond_tool_text(parent, request, &answer, status == "completed");
                }
            }
        }
        if Some(agent) == self.solo {
            let long = self.agents.get(&agent).and_then(|a| a.turn_started).map(|t| t.elapsed() > Duration::from_secs(30)).unwrap_or(false);
            if long {
                self.notes.push(format!("Mantra: Solo turn {status}"));
            }
        }
    }

    fn respond_tool_text(&self, agent: AgentId, id: Value, text: &str, success: bool) {
        self.hub.send(agent, Cmd::Respond { id, result: json!({"contentItems": [{"type": "inputText", "text": text}], "success": success}) });
    }

    fn fail_solo_waiters(&mut self, child: AgentId, reason: &str) {
        if let Some(waiters) = self.solo_waiters.remove(&child) {
            for (parent, request) in waiters {
                self.respond_tool_text(parent, request, reason, false);
            }
        }
    }

    fn solo_child_answer(&self, child: AgentId, status: &str, error: Option<&str>) -> String {
        let answer = self.agents.get(&child).and_then(|a| a.final_message.as_deref()).unwrap_or("").trim();
        if status == "completed" {
            if answer.is_empty() { format!("Subagent {child} completed without a final text answer.") } else { answer.to_string() }
        } else if let Some(error) = error {
            format!("Subagent {child} {status}: {error}")
        } else {
            format!("Subagent {child} {status}.")
        }
    }

    fn stop_solo_children(&mut self, parent: AgentId) {
        let children = self.solo_children.iter().filter_map(|(child, owner)| (*owner == parent).then_some(*child)).collect::<Vec<_>>();
        for child in children {
            self.hub.shutdown(child);
            self.agents.remove(&child);
            self.solo_waiters.remove(&child);
            self.solo_children.remove(&child);
        }
    }

    /// Handle the Solo-only subagent API. `false` means the tool name is not one of ours.
    fn solo_tool_call(&mut self, parent: AgentId, request: Value, name: &str, args: &Value) -> bool {
        if !matches!(name, "mantra_subagent_models" | "mantra_subagent_start" | "mantra_subagent_message" | "mantra_subagent_wait" | "mantra_subagent_inspect" | "mantra_subagent_stop") {
            return false;
        }
        let text_arg = |key: &str| args.get(key).and_then(Value::as_str).unwrap_or("").trim();
        let result = match name {
            "mantra_subagent_models" => {
                let models = self.registry.models.iter().map(|m| {
                    let backend = self.registry.backend_of(m);
                    let problem = if self.demo { None } else { self.registry.alias_problem(&m.alias) };
                    format!("{} ({backend:?}){}", m.alias, problem.map(|p| format!(": unavailable — {p}")).unwrap_or_default())
                }).collect::<Vec<_>>();
                Some((if models.is_empty() { "No models configured. Add one in /models.".into() } else { models.join("\n") }, true))
            }
            "mantra_subagent_start" => {
                let task = text_arg("task");
                let alias = text_arg("model");
                if task.is_empty() || alias.is_empty() {
                    Some(("mantra_subagent_start requires a task and a configured model alias.".to_string(), false))
                } else if self.registry.get(alias).is_none() || (!self.demo && self.registry.alias_problem(alias).is_some()) {
                    Some((format!("Model '{alias}' is not usable. Choose a configured alias from /models."), false))
                } else {
                    let model = self.registry.resolve(alias);
                    let backend = self.registry.backend_of(&model);
                    let mcp = self.settings.mcp_for(&self.settings.solo_mcp);
                    let approval = self.agents.get(&parent).map(|a| a.approval.clone()).unwrap_or_else(|| self.settings.approval_mode.clone());
                    let child = spawn_agent(&mut self.hub, &mut self.agents, &self.registry, AgentOpts {
                        name: format!("solo subagent · {alias}"), role: "solo-subagent".into(), glyph: "◇".into(), color: "blue".into(),
                        model_alias: alias.to_string(), effort: args.get("effort").and_then(Value::as_str).map(str::to_string), cwd: self.project.clone(),
                        approval, sandbox: role_sandbox(&self.settings.sandbox, backend),
                        instructions: "You are an independent subagent working for a Solo session. Complete the assigned task, keep changes within the user's project, and return a concise result.".into(),
                        tools: if model.vision { tools::vision_tools() } else { vec![] }, extra_writable: vec![], network: true, writable_roots: vec![],
                        context_override: Some(self.registry.effective_context_window(alias, &self.settings.default_model)), resume_thread: None, thread_name: None, mcp,
                    });
                    self.solo_children.insert(child, parent);
                    prompt_agent(&self.hub, &mut self.agents, child, task.to_string(), true, Send::Auto);
                    Some((format!("Started subagent {child} on model alias '{alias}'. Use mantra_subagent_wait, mantra_subagent_message, or mantra_subagent_inspect with agent='{child}'."), true))
                }
            }
            "mantra_subagent_message" | "mantra_subagent_wait" | "mantra_subagent_inspect" | "mantra_subagent_stop" => {
                let child = text_arg("agent").parse::<AgentId>().ok().filter(|child| self.solo_children.get(child) == Some(&parent));
                if let Some(child) = child {
                    match name {
                        "mantra_subagent_message" => {
                            let message = text_arg("message");
                            if message.is_empty() { Some(("mantra_subagent_message requires a message.".into(), false)) }
                            else {
                                prompt_agent(&self.hub, &mut self.agents, child, message.to_string(), true, Send::Auto);
                                Some((format!("Message sent to subagent {child}."), true))
                            }
                        }
                        "mantra_subagent_wait" => {
                            let busy = self.agents.get(&child).is_some_and(Agent::busy);
                            if busy {
                                self.solo_waiters.entry(child).or_default().push((parent, request));
                                return true;
                            }
                            let (status, error) = match self.agents.get(&child).map(|a| &a.status) {
                                Some(Status::Failed(e)) | Some(Status::Crashed(e)) => ("failed", Some(e.as_str())),
                                Some(Status::Stopped) => ("stopped", None),
                                _ => ("completed", None),
                            };
                            Some((self.solo_child_answer(child, status, error), status == "completed"))
                        }
                        "mantra_subagent_stop" => {
                            self.fail_solo_waiters(child, &format!("Subagent {child} was stopped."));
                            self.hub.shutdown(child);
                            self.agents.remove(&child);
                            self.solo_children.remove(&child);
                            Some((format!("Stopped subagent {child}."), true))
                        }
                        _ => {
                            if let Some(a) = self.agents.get(&child) {
                                let status = match &a.status { Status::Failed(e) => format!("failed: {e}"), Status::Crashed(e) => format!("crashed: {e}"), s => format!("{s:?}") };
                                let n = args.get("lines").and_then(Value::as_u64).unwrap_or(8).clamp(1, 20) as usize;
                                let tail = a.items.iter().rev().take(n).collect::<Vec<_>>().into_iter().rev().map(|i| i.text.trim()).filter(|s| !s.is_empty()).collect::<Vec<_>>().join("\n");
                                Some((format!("Subagent {child} · {} · model {} · {status}\n{}", a.name, a.model_alias, tail), true))
                            } else { None }
                        }
                    }
                } else {
                    Some(("Unknown subagent id. Use an id returned by mantra_subagent_start in this Solo session.".into(), false))
                }
            }
            _ => None,
        };
        if let Some((text, success)) = result {
            self.respond_tool_text(parent, request, &text, success);
        }
        true
    }

    fn on_request(&mut self, agent: AgentId, id: Value, method: &str, params: Value) {
        match method {
            "item/tool/call" => {
                let tool = params.get("tool").and_then(|t| t.as_str()).unwrap_or("").to_string();
                let args = params.get("arguments").cloned().unwrap_or(Value::Null);
                if tool == "mantra_gate_report" && self.swarm_pending(agent) {
                    self.respond_tool_text(agent, id, "Wait for all Akasha swarm members with mantra_swarm_wait, review their work, then report the gate verdict.", false);
                } else if self.swarm_tool_call(agent, id.clone(), &tool, &args) {
                    // A group tool is always checked against its owner, including stale calls.
                } else if Some(agent) == self.solo && self.solo_tool_call(agent, id.clone(), &tool, &args) {
                    // `mantra_subagent_wait` may keep this request open until the child finishes.
                } else if Some(agent) == self.studio.architect {
                    let (text, ok) = self.architect_tool(&tool, &args);
                    self.hub.send(agent, Cmd::Respond { id, result: json!({"contentItems": [{"type": "inputText", "text": text}], "success": ok}) });
                } else if self.in_run(agent) {
                    self.with_run(|r, c| r.on_tool_call(c, agent, id, &tool, &args));
                } else if let Some(a) = self.agents.get(&agent).filter(|_| tools::VISION_TOOLS.contains(&tool.as_str())) {
                    // Solo's vision tools: captures under ~/.mantra/screenshots, for a model that
                    // (still) takes images — a live switch to one that does not keeps the tools.
                    // The capture runs off the event loop (a capture tool can take seconds, or
                    // hang to its timeout) and comes back as `AppEvent::ToolAnswer`.
                    if self.registry.vision(&a.model_alias) {
                        let (cwd, name, tx, shots) = (a.cwd.clone(), a.name.clone(), self.tx.clone(), crate::config::home().join("screenshots"));
                        tokio::task::spawn_blocking(move || {
                            let (items, ok) = tools::vision_call(&tool, &args, &cwd, &shots, &name);
                            let _ = tx.send(AppEvent::ToolAnswer { agent, id, result: json!({"contentItems": items, "success": ok}) });
                        });
                    } else {
                        let items = vec![tools::text_item(&format!("{} does not take images here, so {tool} is not available.", a.model_alias))];
                        self.hub.send(agent, Cmd::Respond { id, result: json!({"contentItems": items, "success": false}) });
                    }
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
                    "item/permissions/requestApproval" => ("Grant extra permissions?".into(), crate::util::trunc(&params.get("permissions").map(|p| p.to_string()).unwrap_or_default(), 200).to_string()),
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
                if self.agents.get(&agent).map(|a| crate::config::approval_policy(&a.approval) == "never").unwrap_or(false) {
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
            Event::Paste(s) => self.paste(&s),
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
            Some(Overlay::Diff { scroll, .. }) | Some(Overlay::Plan { scroll }) | Some(Overlay::Halt { scroll }) | Some(Overlay::Help { scroll }) => {
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

    /// A paste goes where typing would: the field being edited (one line, unless it takes
    /// several), the new-run goal, the discovery filter, the Studio's architect, the chat input.
    /// On a config screen with nothing being edited it starts editing the selected field, as
    /// typing its first character would.
    fn paste(&mut self, s: &str) {
        let one_line = |s: &str| s.trim_end_matches(['\r', '\n']).replace("\r\n", " ").replace(['\r', '\n'], " ");
        match self.overlays.last_mut() {
            Some(Overlay::NewRun { goal: Some(goal), .. }) => return goal.insert_str(s),
            Some(Overlay::Discover(st)) => return st.filter.insert_str(&one_line(s)),
            None if self.edit.is_none() && self.screen.is_config() && !(self.screen == Screen::Studio && self.studio.focus == 2) => {
                let Some(first) = s.chars().find(|c| !c.is_control() && !c.is_whitespace()) else { return };
                self.config_key(KeyEvent::new(KeyCode::Char(first), KeyModifiers::NONE));
                let Some(e) = self.edit.as_mut() else { return };
                let rest: String = s.chars().skip_while(|c| *c != first).skip(1).collect();
                return e.input.insert_str(&if e.multiline { rest } else { one_line(&rest) });
            }
            _ => {}
        }
        if let Some(e) = self.edit.as_mut() {
            let s = if e.multiline { s.to_string() } else { one_line(s) };
            e.input.insert_str(&s);
        } else if self.screen == Screen::Studio && self.studio.focus == 2 {
            self.studio.input.insert_str(s);
        } else {
            self.input.insert_str(s);
        }
    }

    /// The keys of the config screen on view.
    fn config_key(&mut self, k: KeyEvent) {
        match self.screen {
            Screen::Studio => crate::ui::studio::studio_key(self, k),
            Screen::Models => crate::ui::models::models_key(self, k),
            Screen::Settings => crate::ui::settings::settings_key(self, k),
            _ => {}
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
        // A field being edited takes every other key: ⏎ keeps it, esc brings the old value back
        // (`ui::form`) — nothing global fires in the middle of a word.
        if self.edit.is_some() && self.overlays.is_empty() && self.screen.is_config() {
            return self.config_key(k);
        }
        if ctrl && k.code == KeyCode::Char('o') {
            self.overlays.clear();
            self.set_screen(match self.screen {
                Screen::Solo => Screen::Stage,
                _ => Screen::Solo,
            });
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
        // ctrl+t and ctrl+d belong to the chat screens; a config screen's own are row actions
        // (test, discover), handled by the screen below
        if ((ctrl && k.code == KeyCode::Char('t')) || k.code == KeyCode::F(2)) && !self.screen.is_config() {
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
        if ctrl && k.code == KeyCode::Char('d') && self.overlays.is_empty() && !self.screen.is_config() {
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
            self.overlays.push(Overlay::Help { scroll: 0 });
            return;
        }
        // page keys scroll the log in view, unless what is in front pages through its own list
        let pages_itself = matches!(self.overlays.last(), Some(Overlay::Diff { .. } | Overlay::Plan { .. } | Overlay::Halt { .. } | Overlay::Help { .. } | Overlay::Discover(_))) || (self.overlays.is_empty() && self.screen.is_config());
        if matches!(k.code, KeyCode::PageUp | KeyCode::PageDown) && !pages_itself {
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
        if self.screen.is_config() {
            return self.config_key(k);
        }
        // effort / approvals shortcuts
        if (alt || k.modifiers.contains(KeyModifiers::SHIFT)) && matches!(k.code, KeyCode::Up | KeyCode::Down) && !(self.screen == Screen::Stage && self.canvas_focus) {
            if let Some(a) = self.focus_agent() {
                self.step_effort(a, if k.code == KeyCode::Up { 1 } else { -1 });
            }
            return;
        }
        if self.screen == Screen::Stage && self.stage_key(k) {
            return;
        }
        // Backspace on an empty input pops the last queued message back into the input to edit.
        if k.code == KeyCode::Backspace && self.input.is_empty() {
            if let Some(a) = self.composer_agent() {
                if let Some(text) = self.agents.get_mut(&a).and_then(|ag| ag.queued.pop()) {
                    self.input.set(&text);
                    return;
                }
            }
        }
        // suggestions: ↑↓ pick, tab completes, ⏎ completes a command or its argument and runs it
        // (an @name only completes: the message comes after it)
        let sugg = self.suggestions();
        if !sugg.is_empty() {
            let pick = &sugg[self.suggest.min(sugg.len() - 1)];
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
                    self.input.set(&pick.text);
                    self.suggest = 0;
                    return;
                }
                KeyCode::Enter if !pick.text.starts_with('@') => {
                    if self.input.text().trim() != pick.text.trim() {
                        self.input.set(pick.text.trim_end());
                    }
                }
                _ => {}
            }
        }
        // esc only navigates: back to the overview from a zoom, otherwise clear the input.
        // Interrupting is ctrl+c's job (it used to be both, and the two hints collided).
        if k.code == KeyCode::Esc {
            match self.screen {
                Screen::Zoom(_) => {
                    self.set_screen(Screen::Stage);
                    // back onto a halted run with the keys live: the zoom header's "esc ▸ e
                    // details" and the halt band's e / space / m / r then work on the next key
                    if self.run.as_ref().map(|r| r.halted()).unwrap_or(false) {
                        self.canvas_focus = true;
                    }
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
            self.overlays.push(Overlay::Help { scroll: 0 });
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
                KeyCode::Left | KeyCode::Up | KeyCode::Right | KeyCode::Down => {
                    self.sel_follow = None;
                    let visible: Vec<_> = self.stage_nav.iter().filter(|(id, _, _)| nodes.contains(id)).copied().collect();
                    if !visible.is_empty() {
                        let selected = nodes.get(self.sel).copied();
                        let origin = visible.iter().find(|(id, _, _)| Some(*id) == selected);
                        let next = origin.and_then(|(_, x, y)| {
                            visible.iter().filter_map(|(id, nx, ny)| {
                                let dx = *nx as i32 - *x as i32;
                                let dy = *ny as i32 - *y as i32;
                                let (forward, sideways) = match k.code {
                                    KeyCode::Left => (-dx, dy.abs()),
                                    KeyCode::Right => (dx, dy.abs()),
                                    KeyCode::Up => (-dy, dx.abs()),
                                    _ => (dy, dx.abs()),
                                };
                                let distance = if matches!(k.code, KeyCode::Up | KeyCode::Down) { (forward, sideways) } else { (sideways, forward) };
                                (forward > 0).then_some((distance, *id))
                            }).min_by_key(|(distance, _)| *distance).map(|(_, id)| id)
                        }).or_else(|| origin.is_none().then(|| visible[0].0));
                        if let Some(id) = next {
                            if let Some(i) = nodes.iter().position(|a| *a == id) { self.sel = i; }
                        }
                    } else if matches!(k.code, KeyCode::Left | KeyCode::Up) {
                        self.sel = self.sel.saturating_sub(1);
                    } else if !nodes.is_empty() {
                        self.sel = (self.sel + 1).min(nodes.len() - 1);
                    }
                    return true;
                }
                KeyCode::Char('h') | KeyCode::Char('k') => {
                    self.sel_follow = None;
                    self.sel = self.sel.saturating_sub(1);
                    return true;
                }
                KeyCode::Char('l') | KeyCode::Char('j') => {
                    self.sel_follow = None;
                    if !nodes.is_empty() { self.sel = (self.sel + 1).min(nodes.len() - 1); }
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
            KeyCode::Char('e') if self.run.as_ref().map(|r| r.halted()).unwrap_or(false) => self.overlays.push(Overlay::Halt { scroll: 0 }),
            KeyCode::Char('a') => {
                if self.run.as_ref().map(|r| r.stage == Stage::Review).unwrap_or(false) {
                    self.with_run(|r, c| r.approve_plan(c));
                    self.land_on_overview();
                }
            }
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
                            Ok(what) => {
                                // a worker's new attempt has no node until its agent exists
                                self.sel_follow = self.run.as_ref().and_then(|r| r.workers.iter().find(|w| w.agent == Some(a))).map(|w| w.task.id.clone());
                                self.toast(what, Level::Info);
                            }
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
            KeyCode::Char('?') => self.overlays.push(Overlay::Help { scroll: 0 }),
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
                    // the user chose a card: a respawned task no longer pulls the selection back
                    self.sel_follow = None;
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

    /// What the input could become: a command for `/…`, a command's argument for `/akasha …`
    /// (the run's groups) and `/respawn …` (its agents), an agent for `@…` on the stage. A
    /// parameter is never to be typed out: this completes it, a selector asks for a missing one.
    pub fn suggestions(&self) -> Vec<Suggestion> {
        let t = self.input.text();
        if t.contains('\n') {
            return vec![];
        }
        if let Some(name) = t.strip_prefix('@') {
            if self.screen != Screen::Stage || name.contains(char::is_whitespace) {
                return vec![];
            }
            let planner = self.run.as_ref().and_then(|r| r.planner.map(|id| r.name_of(id)));
            return self.run_agent_choices().into_iter().filter(|c| Some(c.label.as_str()) == planner.as_deref() && c.label.starts_with(name)).map(|c| Suggestion { text: format!("@{} ", c.label), label: format!("@{}", c.label), about: c.detail }).collect();
        }
        if !t.starts_with('/') {
            return vec![];
        }
        let Some((cmd, arg)) = t.split_once(' ') else {
            return COMMANDS.iter().filter(|(c, _)| c.starts_with(t.as_str())).map(|(c, d)| Suggestion { text: format!("{c} "), label: c.to_string(), about: d.to_string() }).collect();
        };
        if arg.contains(' ') {
            return vec![];
        }
        let options: Vec<(String, String)> = match cmd {
            "/akasha" => self.akasha_choices().into_iter().map(|c| (c.label, c.detail)).collect(),
            "/respawn" => self.run_agent_choices().into_iter().map(|c| (c.label, c.detail)).collect(),
            _ => vec![],
        };
        options.into_iter().filter(|(o, _)| o.starts_with(arg) && o != arg).map(|(o, about)| Suggestion { text: format!("{cmd} {o}"), label: o, about }).collect()
    }

    /// Reserve the swarm key namespace without taking an existing run group's notes away.
    pub(crate) fn akasha_run_key(name: &str) -> String {
        if name.starts_with("swarm:") || name.starts_with("run:") { format!("run:{name}") } else { name.to_string() }
    }

    /// Run groups keep ordinary names; scoped swarms use a key distinct from their friendly name.
    pub fn akasha_choices(&self) -> Vec<Choice> {
        let mut choices: Vec<Choice> = self.run.iter().flat_map(|r| {
            r.akasha.values().map(|b| {
                let roles = r.pattern.groups.iter().find(|g| g.name == b.group).map(|g| g.roles.join(", ")).unwrap_or_default();
                let n = b.entries.len();
                let key = Self::akasha_run_key(&b.group);
                Choice { key: key.clone(), label: key, detail: format!("{n} note{} · {roles}", if n == 1 { "" } else { "s" }), on: false }
            })
        }).collect();
        let mut swarms: Vec<_> = self.swarms.values().collect();
        swarms.sort_by_key(|s| s.owner);
        choices.extend(swarms.into_iter().map(|s| {
            let key = format!("swarm:{}", s.owner);
            let owner = self.agents.get(&s.owner).map(|a| a.name.as_str()).unwrap_or("closed owner");
            Choice { key: key.clone(), label: key, detail: format!("{} · {owner} · {} agents · {} notes", s.name, s.children.len(), s.notes.entries.len()), on: false }
        }));
        choices
    }

    /// The run's agents as the stage shows them (card order): name, then role and model.
    pub fn run_agent_choices(&self) -> Vec<Choice> {
        let Some(r) = &self.run else { return vec![] };
        self.stage_nodes()
            .into_iter()
            .map(|a| {
                let role = r.role_name_of(a).unwrap_or_default();
                let model = self.agents.get(&a).map(|x| x.model_alias.clone()).unwrap_or_default();
                Choice { key: a.to_string(), label: r.name_of(a), detail: if role.is_empty() { model } else { format!("{role} · {model}") }, on: false }
            })
            .collect()
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
        // a field being edited goes back to what it was, as esc does there
        if self.edit.is_some() && self.screen.is_config() {
            self.config_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
            self.edit = None;
            return;
        }
        // In Solo, ctrl+c means stop the live model turn even when the user has already started
        // drafting the next message; preserve that draft for after the interruption.
        if self.screen == Screen::Solo {
            if let Some(a) = self.focus_agent() {
                if self.interrupt_by_user(a) {
                    self.toast(format!("stopping… it stays stopped ({more})"), Level::Warn);
                    return;
                }
            }
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

    /// End the actual process, not just its current turn. Keep the transcript and make the
    /// user-stop latch outlive late notifications and queued engine retries.
    pub(crate) fn stop_agent_by_user(&mut self, id: AgentId) -> bool {
        if !self.hub.has_agent(id) {
            return false;
        }
        self.stop_swarm_children(id);
        let children: Vec<_> = self.solo_children.iter().filter_map(|(child, parent)| (*parent == id).then_some(*child)).collect();
        for child in children {
            self.stop_agent_by_user(child);
        }
        self.fail_solo_waiters(id, &format!("Subagent {id} was stopped by you."));
        self.approvals.retain(|a| a.agent != id);
        self.hub.shutdown(id);
        if let Some(a) = self.agents.get_mut(&id) {
            a.stopped_by_user = true;
            a.queued.clear();
            a.status = Status::Stopped;
            a.turn_active = false;
            a.awaiting_start = false;
            a.turn_wait_since = None;
            a.compacting = false;
            a.compact_pending = false;
            a.finished.get_or_insert(Instant::now());
            a.activity = "stopped by you".into();
            a.notice(Level::Warn, "process stopped by you — respawn it or start a new Solo session");
        }
        if self.in_run(id) {
            if let Some(run) = self.run.as_mut() {
                run.agent_stopped_by_user(id);
            }
        }
        if self.swarm_owners.contains_key(&id) {
            self.swarm_child_ended(id, "stopped", None);
        }
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

    /// Send the composer's draft and queued messages immediately. A run always targets its planner.
    fn force_send(&mut self) {
        let in_run = self.screen == Screen::Stage || matches!(self.screen, Screen::Zoom(id) if self.in_run(id));
        if in_run {
            let Some(planner) = self.composer_agent() else { return };
            let text = self.input.text().trim_end().to_string();
            let empty_queue = self.agents.get(&planner).map(|a| a.queued.is_empty()).unwrap_or(true);
            if text.trim().is_empty() && empty_queue { return; }
            self.input.take();
            let text = text.strip_prefix('@').and_then(|rest| rest.split_once(char::is_whitespace)).map(|(_, body)| body.trim().to_string()).unwrap_or(text);
            self.with_run(|r, c| r.direct(c, "planner", &text, Send::Force));
        } else if let Some(id) = self.composer_agent() {
            self.force_prompt(id);
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
        if let Some(a) = self.composer_agent() {
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
            "/help" => self.overlays.push(Overlay::Help { scroll: 0 }),
            "/model" => {
                if arg.is_empty() {
                    let cur = target.and_then(|a| self.agents.get(&a)).map(|a| a.model_alias.clone()).unwrap_or_default();
                    let sel = self.registry.models.iter().position(|m| m.alias == cur).unwrap_or(0);
                    self.overlays.push(Overlay::ModelPicker { sel, target });
                } else if let Some(a) = target {
                    if let Err(e) = self.set_model(a, arg) {
                        self.toast(e, Level::Warn);
                    }
                }
            }
            "/new" | "/clear" => {
                self.start_solo();
                self.set_screen(Screen::Solo);
                self.toast("fresh session", Level::Ok);
            }
            "/compact" => match target {
                Some(a) => self.compact_agent(a),
                None => self.toast("no agent in view to compact", Level::Info),
            },
            "/diff" => {
                if let Some(a) = target {
                    self.overlays.push(Overlay::Diff { agent: a, file: 0, scroll: 0 });
                }
            }
            "/stage" => self.set_screen(Screen::Stage),
            "/solo" => self.set_screen(Screen::Solo),
            "/run" if arg.is_empty() => self.open_new_run(true),
            "/run" => self.start_run(arg),
            "/pattern" if arg.is_empty() => self.open_new_run(false),
            "/pattern" => self.choose_pattern(arg),
            "/runs" => self.open_runs(),
            "/stop" => {
                self.stop_run();
            }
            "/plan" => self.overlays.push(Overlay::Plan { scroll: 0 }),
            "/akasha" => self.open_akasha(arg),
            "/pause" => {
                self.with_run(|r, c| r.toggle_pause(c));
            }
            "/respawn" => self.respawn_command(arg, target),
            "/land" => {
                if self.run.as_ref().map(|r| r.stage == Stage::Done).unwrap_or(false) {
                    self.with_run(|r, c| r.land(c));
                } else {
                    self.toast("no finished run to land", Level::Warn);
                }
            }
            "/studio" => self.open_studio(),
            "/models" => self.enter_screen(Screen::Models),
            "/settings" => self.open_settings(),
            "/inbox" => self.overlays.push(Overlay::Inbox { sel: 0 }),
            _ => match MOVED.iter().find(|(c, _)| *c == cmd) {
                Some((_, there)) => self.toast(*there, Level::Info),
                None => self.toast(format!("unknown command {cmd} — /help"), Level::Warn),
            },
        }
    }

    /// The new-run overlay: every pattern, the one new runs use selected — with a goal to type
    /// (`/run`, ⏎ starts) or without (`/pattern`, ⏎ chooses).
    pub fn open_new_run(&mut self, with_goal: bool) {
        let list = PatternChoice::all(&self.project);
        let sel = list.iter().position(|p| p.name == self.pattern_name).unwrap_or(0);
        self.overlays.push(Overlay::NewRun { sel, list, goal: with_goal.then(Input::default) });
    }

    /// `--pattern <name>` for this session: a built-in 0.8 retired reads as the one that runs in
    /// its place (`pattern::current_name`), so the stage never names a pattern that is gone.
    pub fn session_pattern(&mut self, name: &str) {
        self.pattern_name = crate::engine::pattern::current_name(name, &self.project);
    }

    /// The pattern new runs use from now on (this session; `/settings` saves a default). The
    /// Studio opens on it, unless it has unsaved changes of its own.
    pub fn choose_pattern(&mut self, name: &str) {
        let name = crate::engine::pattern::current_name(name, &self.project);
        match Pattern::load(&name, &self.project) {
            Ok(p) => {
                if !self.studio.dirty {
                    self.studio.pattern = p;
                }
                self.toast(format!("new runs use pattern {name}"), Level::Ok);
                self.pattern_name = name;
            }
            Err(e) => self.toast(format!("pattern error: {e}"), Level::Warn),
        }
    }

    /// `/akasha [group]`: the notes of the group named; with none, the only group straight away,
    /// or a selector of run and scoped swarm groups — and a toast when there is nothing to show.
    pub fn open_akasha(&mut self, group: &str) {
        let groups = self.akasha_choices();
        // Older run groups with a reserved-looking name still open by that name when it is
        // unambiguous; selectors and completions always use their explicit key.
        let run_alias = (!groups.iter().any(|g| g.key == group))
            .then(|| self.run.as_ref().and_then(|r| r.akasha.get(group)).map(|b| Self::akasha_run_key(&b.group)))
            .flatten();
        let group = run_alias.as_deref().unwrap_or(group);
        if groups.is_empty() {
            self.toast("no akasha groups in this run", Level::Info);
        } else if !group.is_empty() && !groups.iter().any(|g| g.key == group) {
            let have: Vec<String> = groups.into_iter().map(|g| g.key).collect();
            let scope = if self.swarms.is_empty() { "this run" } else { "this session" };
            self.toast(format!("no akasha group '{group}' in {scope} (it has: {})", have.join(", ")), Level::Info);
        } else if !group.is_empty() || groups.len() == 1 {
            let group = if group.is_empty() { groups[0].key.clone() } else { group.to_string() };
            self.overlays.push(Overlay::Akasha { scroll: 0, group: Some(group) });
        } else {
            self.overlays.push(Overlay::Select(Select { title: "akasha · which group".into(), items: groups, sel: 0, multi: false, then: SelectThen::Akasha }));
        }
    }

    /// `/respawn [agent]`: the agent named, else the run agent in view, else a selector of the
    /// run's agents.
    fn respawn_command(&mut self, name: &str, target: Option<AgentId>) {
        let agents = self.run_agent_choices();
        if !name.is_empty() {
            match agents.iter().find(|c| c.label.eq_ignore_ascii_case(name.trim_start_matches('@'))).and_then(|c| c.key.parse::<AgentId>().ok()) {
                Some(a) => self.respawn_agent(a),
                None => self.toast(format!("no agent named {name} in this run"), Level::Warn),
            }
        } else if let Some(a) = target.filter(|a| self.in_run(*a)) {
            self.respawn_agent(a);
        } else if agents.is_empty() {
            self.toast("no run agent to respawn", Level::Info);
        } else {
            self.overlays.push(Overlay::Select(Select { title: "respawn · which agent".into(), items: agents, sel: 0, multi: false, then: SelectThen::Respawn }));
        }
    }

    pub fn studio_architect_send(&mut self, text: String) {
        let Some(a) = self.start_architect() else { return };
        prompt_agent(&self.hub, &mut self.agents, a, text, true, Send::Auto);
    }

    /// Stops every agent; the caller awaits the result before the runtime goes (`hub::Stopping`).
    pub fn shutdown(&mut self) -> crate::hub::Stopping {
        self.hub.shutdown_all()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn configured_fixture() -> Registry {
        let mut reg = Registry::defaults();
        reg.providers.push(crate::config::bija_default_provider());
        reg.models.extend(crate::config::bija_default_model_entries("bija"));
        reg
    }

    #[tokio::test]
    async fn built_in_run_binds_missing_role_aliases_to_one_configured_model() {
        crate::web::snapshot::tests::with_mantra_home(|| {
            let project = crate::config::home().join("one-model-project");
            std::fs::create_dir_all(&project).unwrap();
            let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
            let (ev_tx, _ev_rx) = tokio::sync::mpsc::unbounded_channel();
            let mut reg = Registry::defaults();
            reg.add_codex_model("my-codex", "model-chosen-by-user").unwrap();
            let settings = Settings { default_model: "my-codex".into(), ..Settings::default() };
            let mut app = App::new(settings, reg, project, Hub::new(vec![], vec![], vec![], vec![], ev_tx, false), tx, false);
            app.start_run("check the repository");
            let run = app.run.as_ref().expect("one configured model suffices for a built-in run");
            assert!(run.pattern.roles.values().all(|role| role.model == "my-codex"));
            assert!(run.pulse.iter().any(|line| line.text.contains("built-in roles use configured models")));
            app.shutdown();
        });
    }

    /// `/pattern` lists the built-ins first, `mantra-default` leading and marked as the default,
    /// each with the one-line description the picker shows.
    #[test]
    fn the_pattern_picker_leads_with_the_default_builtin() {
        let list = PatternChoice::all(Path::new("/nonexistent"));
        let n = Pattern::names().len();
        let names: Vec<&str> = list.iter().take(n).map(|p| p.name.as_str()).collect();
        assert_eq!(names, Pattern::names());
        // "edited" when this machine's ~/.mantra/patterns shadows one with a file
        assert!(["default", "edited"].contains(&list[0].tag), "{}", list[0].tag);
        assert!(list[1..n].iter().all(|p| ["built-in", "edited"].contains(&p.tag)));
        assert!(list[..n].iter().all(|p| p.about.as_ref().map(|d| !d.is_empty()).unwrap_or(false)));
    }

    /// `--parallel N` (`start_run_with(…, Some(N))`) sets every worker class of the run's own copy
    /// of the pattern — the shared pool and the swarm's own cap — and journals it; the saved
    /// pattern, the session's pattern and a plain `/run` stay as they were. `/swarm` is gone.
    #[tokio::test]
    async fn parallel_sets_every_class_in_the_runs_pattern_copy() {
        crate::web::snapshot::tests::with_mantra_home(|| {
            let project = std::env::temp_dir().join(format!("mantra-parallel-{}-{}", std::process::id(), crate::web::snapshot::now_ms()));
            std::fs::create_dir_all(&project).unwrap();
            let mut app = crate::web::snapshot::tests::test_app();
            app.project = project.clone();
            let session_pattern = app.pattern_name.clone();
            assert!(!COMMANDS.iter().any(|(c, _)| *c == "/swarm"), "the swarm is a worker class now, not a command");
            app.command("/swarm 24 build it");
            assert!(app.run.is_none(), "/swarm starts nothing");
            app.start_run_with("build the parser", None, Some(24));
            let run = app.run.as_ref().expect("the run started");
            assert_eq!((run.pattern.name.as_str(), run.pattern.settings.max_parallel, run.pattern.cap_of("worker-swarm")), (crate::engine::pattern::NAME_DEFAULT, 24, 24));
            let copy = Pattern::from_toml(&std::fs::read_to_string(run.dir.join("pattern.toml")).unwrap()).unwrap();
            assert_eq!((copy.settings.max_parallel, copy.cap_of("worker-swarm"), copy.settings.max_tasks_per_phase), (24, 24, 24), "a resume reads the run's copy back");
            assert_eq!(copy.roles["worker-big"].parallel, None, "the big worker still shares the pool");
            let said = run.pulse.iter().map(|p| p.text.clone()).collect::<Vec<_>>();
            assert!(said.iter().any(|t| t == "24 at once in every worker class for this run (the pattern says 4 at once · worker-swarm 16), and up to 24 tasks a phase (it says 8)"), "journaled: {said:?}");
            assert!(!said.iter().any(|t| t.contains("rate limits")), "24 is within what Mantra handles");
            assert!(!Pattern::path_for(crate::engine::pattern::NAME_DEFAULT).exists(), "the saved pattern is never written");
            assert_eq!(Pattern::load(crate::engine::pattern::NAME_DEFAULT, &project).unwrap().cap_of("worker-swarm"), 16);
            assert_eq!(app.pattern_name, session_pattern, "the session's pattern for /run is unchanged");
            // past 32 the run says what the ceiling is now
            app.run = None;
            app.start_run_with("build the parser", None, Some(64));
            let run = app.run.as_ref().expect("the run started");
            assert_eq!((run.pattern.settings.max_parallel, run.pattern.cap_of("worker-swarm"), run.pattern.settings.max_tasks_per_phase), (64, 64, 64));
            assert!(run.pulse.iter().any(|p| p.text.contains("rate limits")), "the note above 32");
            // and a plain run keeps the pattern's own numbers, without a word about them
            app.run = None;
            app.start_run("build the parser");
            let run = app.run.as_ref().expect("the run started");
            let saved = Pattern::load(&session_pattern, &project).unwrap();
            assert_eq!((run.pattern.settings.max_parallel, run.pattern.cap_of("worker-swarm")), (saved.settings.max_parallel, saved.cap_of("worker-swarm")));
            assert!(!run.pulse.iter().any(|p| p.text.contains("in every worker class")));
            app.run = None;
            let _ = std::fs::remove_dir_all(&project);
        });
    }

    /// `stop_run` ends the active run for good — saved as "stopped", finished, its owner file
    /// released — so a second `/run` that was refused before now starts, and the stopped run can
    /// be deleted from `/runs` (it was refused while active).
    #[tokio::test]
    async fn a_stopped_run_makes_way_for_the_next_and_can_be_deleted() {
        crate::web::snapshot::tests::with_mantra_home(|| {
            let project = crate::config::home().join("stop-project");
            std::fs::create_dir_all(&project).unwrap();
            let mut app = crate::web::snapshot::tests::test_app();
            app.project = project.clone();
            assert!(!app.stop_run(), "nothing to stop yet");
            assert_eq!(app.toast.as_ref().map(|t| t.0.as_str()), Some("no active run to stop"));
            app.start_run("build the parser");
            let (id, dir) = app.run.as_ref().map(|r| (r.id.clone(), r.dir.clone())).unwrap();
            app.start_run("build something else");
            assert_eq!(app.run.as_ref().map(|r| r.id.as_str()), Some(id.as_str()), "one active run at a time");
            let row = || crate::engine::state::list_all().into_iter().find(|r| r.id == id).expect("the run is listed");
            assert!(!app.delete_run(&row()), "an active run is not deleted");
            assert!(app.toast.as_ref().map(|t| t.0.contains("stop it first")).unwrap_or(false), "{:?}", app.toast);
            assert!(app.stop_run());
            let run = app.run.as_ref().unwrap();
            assert!(!run.is_active());
            assert_eq!(run.stage, crate::engine::run::Stage::Failed(crate::engine::run::STOPPED_BY_USER.into()));
            assert!(app.toast.as_ref().map(|t| t.0.starts_with("run stopped")).unwrap_or(false), "{:?}", app.toast);
            assert_eq!((row().stage_label(), row().unfinished()), ("stopped".to_string(), false), "/runs shows it stopped, not unfinished");
            assert!(crate::engine::state::Owner::load(&dir).is_none(), "the owner file is released");
            assert!(!app.stop_run(), "stopping twice does nothing");
            assert!(app.delete_run(&row()), "a stopped run can be deleted");
            assert!(app.run.is_none() && !dir.exists());
            app.start_run("build something else");
            assert!(app.run.as_ref().map(|r| r.is_active() && r.id != id).unwrap_or(false), "the next run starts");
            app.run = None;
        });
    }

    /// A built-in 0.8 retired reads as `mantra-default` wherever its name comes from: settings.toml
    /// (`App::new`), `--pattern` (`session_pattern`), a name that reaches a run's start anyway, and
    /// a run's state with no `pattern.toml` beside it — while a file of that name is the user's own
    /// pattern.
    #[tokio::test]
    async fn retired_pattern_names_read_as_the_default() {
        use crate::engine::pattern::{NAME_DEFAULT, RETIRED};
        crate::web::snapshot::tests::with_mantra_home(|| {
            let project = crate::config::home().join("retired-project");
            std::fs::create_dir_all(&project).unwrap();
            let app_with = |default_pattern: &str| {
                let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
                let (ev_tx, _ev_rx) = tokio::sync::mpsc::unbounded_channel();
                let hub = Hub::new(vec![], vec![], vec![], vec![], ev_tx, false);
                App::new(Settings { default_pattern: default_pattern.into(), ..Settings::default() }, configured_fixture(), project.clone(), hub, tx, true)
            };
            for old in RETIRED {
                let app = app_with(old);
                assert_eq!((app.pattern_name.as_str(), app.studio.pattern.name.as_str(), app.settings.default_pattern.as_str()), (NAME_DEFAULT, NAME_DEFAULT, NAME_DEFAULT), "settings.toml naming {old}");
            }
            // --pattern mantra-swarm: the session takes the default's name
            let mut app = app_with(NAME_DEFAULT);
            app.session_pattern("mantra-swarm");
            assert_eq!(app.pattern_name, NAME_DEFAULT);
            // and a retired name that reaches a run's start anyway runs the default
            app.pattern_name = "mantra-swarm".into();
            app.start_run("build the parser");
            let (id, dir) = app.run.as_ref().map(|r| (r.id.clone(), r.dir.clone())).unwrap();
            assert_eq!(app.run.as_ref().map(|r| r.pattern.name.as_str()), Some(NAME_DEFAULT));
            // a run's state naming one, its pattern.toml gone: it reopens on the default
            app.stop_run();
            app.run = None;
            std::fs::remove_file(dir.join("pattern.toml")).unwrap();
            let state = std::fs::read_to_string(dir.join("state.json")).unwrap().replace("\"pattern\": \"mantra-default\"", "\"pattern\": \"mantra-default-claude\"");
            assert!(state.contains("mantra-default-claude"), "the fixture names the retired pattern");
            std::fs::write(dir.join("state.json"), state).unwrap();
            let row = crate::engine::state::list_all().into_iter().find(|r| r.id == id).unwrap();
            app.resume_run(row);
            assert_eq!(app.run.as_ref().map(|r| r.pattern.name.as_str()), Some(NAME_DEFAULT));
            app.run = None;
            // a file of the user's under a retired name is theirs
            Pattern { name: "mantra-swarm".into(), description: "mine".into(), ..Pattern::builtin_lean() }.save().unwrap();
            let app = app_with("mantra-swarm");
            assert_eq!((app.pattern_name.as_str(), app.studio.pattern.description.as_str()), ("mantra-swarm", "mine"));
        });
    }

    /// The demo's settings point at its mocks and have Akasha on in memory: a shift+tab, a model
    /// pick or a Solo switch there saves nothing, so none of it leaks into the real sessions after.
    /// Outside the demo the same changes are saved as always.
    #[test]
    fn the_demo_never_writes_its_settings_to_disk() {
        crate::web::snapshot::tests::with_mantra_home(|| {
            let path = crate::config::home().join("settings.toml");
            let mut app = crate::web::snapshot::tests::test_app();
            assert!(app.demo);
            app.settings.codex_command = vec!["mock-codex".into()];
            app.settings.experimental.push(crate::config::EXPERIMENTAL_AKASHA.into());
            app.set_approval_mode("on-request");
            assert_eq!(app.settings.approval_mode, "on-request", "the session has it");
            assert!(!path.exists(), "the demo saved {:?}", std::fs::read_to_string(&path));
            let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
            let (ev_tx, _ev_rx) = tokio::sync::mpsc::unbounded_channel();
            let mut real = App::new(Settings::default(), Registry::defaults(), PathBuf::from("."), Hub::new(vec![], vec![], vec![], vec![], ev_tx, false), tx, false);
            real.set_approval_mode("on-request");
            let saved = Settings::load();
            assert_eq!(saved.approval_mode, "on-request");
            assert!(!saved.experimental_on(crate::config::EXPERIMENTAL_AKASHA));
        });
    }

    /// An experimental feature is switched in `/settings` (Features): the session and
    /// settings.toml both — the demo's session only, its settings point at its mocks. `/experimental`
    /// says where the switch went. `mantra experimental` (`save_experimental`) leaves a settings.toml
    /// that does not parse alone rather than replacing it with the defaults.
    #[test]
    fn experimental_switches_save_the_file_and_the_session() {
        crate::web::snapshot::tests::with_mantra_home(|| {
            let mut demo = crate::web::snapshot::tests::test_app();
            demo.command("/experimental akasha on");
            assert_eq!(demo.toast.as_ref().map(|t| t.0.as_str()), Some("experimental features are in /settings (Features)"));
            assert!(!demo.settings.experimental_on("akasha"), "typed by hand, it switches nothing");
            assert_eq!(demo.set_setting("experimental.akasha", "on"), Ok("new runs use it"));
            assert!(demo.settings.experimental_on("akasha"));
            assert!(!crate::config::home().join("settings.toml").exists(), "the demo saves nothing");
            let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
            let (ev_tx, _ev_rx) = tokio::sync::mpsc::unbounded_channel();
            let mut app = App::new(Settings::default(), configured_fixture(), PathBuf::from("."), Hub::new(vec![], vec![], vec![], vec![], ev_tx, false), tx, false);
            app.set_setting("experimental.akasha", "on").unwrap();
            assert!(app.settings.experimental_on("akasha") && Settings::load().experimental_on("akasha"));
            app.set_setting("experimental.akasha", "off").unwrap();
            assert!(!app.settings.experimental_on("akasha") && !Settings::load().experimental_on("akasha"));
            let e = app.set_setting("experimental.warp", "on").unwrap_err();
            assert!(e.contains("unknown experimental feature 'warp'") && e.contains("akasha"), "{e}");
            // a settings.toml that does not parse is left alone, not replaced by the defaults
            let path = crate::config::home().join("settings.toml");
            std::fs::write(&path, "default_model = [broken").unwrap();
            assert!(save_experimental("akasha", true).unwrap_err().contains("does not parse"));
            assert_eq!(std::fs::read_to_string(&path).unwrap(), "default_model = [broken");
        });
    }

    /// The command table: the 19 commands of 0.8, each with its line; the ones folded into
    /// `/settings` and keys say where they went when typed, and are not offered.
    #[test]
    fn the_command_table_is_the_nineteen_and_the_moved_say_where() {
        let names: Vec<&str> = COMMANDS.iter().map(|(c, _)| *c).collect();
        assert_eq!(names, vec!["/run", "/runs", "/stop", "/pause", "/plan", "/land", "/respawn", "/akasha", "/inbox", "/pattern", "/studio", "/models", "/settings", "/model", "/new", "/compact", "/diff", "/help", "/quit"]);
        assert!(COMMANDS.iter().all(|(_, d)| !d.is_empty() && crate::util::width(d) <= 48), "each line fits the suggestions");
        let mut app = crate::web::snapshot::tests::test_app();
        for (cmd, there) in MOVED {
            assert!(!names.contains(cmd), "{cmd} is not offered");
            app.command(cmd);
            assert_eq!(app.toast.as_ref().map(|t| t.0.as_str()), Some(*there), "{cmd}");
            assert!(app.overlays.is_empty(), "{cmd} opens nothing");
            assert!(crate::util::width(there) <= 70, "{cmd}: fits a toast at 80 columns");
        }
        app.command("/frobnicate");
        assert!(app.toast.as_ref().is_some_and(|t| t.0.contains("unknown command")));
        // the new ones
        app.command("/settings");
        assert_eq!(app.screen, Screen::Settings);
        app.command("/stop");
        assert_eq!(app.toast.as_ref().map(|t| t.0.as_str()), Some("no active run to stop"));
        app.input.set("/s");
        assert_eq!(app.suggestions().iter().map(|s| s.label.as_str()).collect::<Vec<_>>(), vec!["/stop", "/studio", "/settings"]);
    }

    /// `--pattern mantra-swarm`: the session takes the name of the pattern that runs in its place
    /// (the default), so the stage's crumb never names one 0.8 retired; another name is kept.
    #[test]
    fn a_retired_pattern_name_never_reaches_the_crumb() {
        let mut app = screen_app();
        app.project = PathBuf::from("/nonexistent");
        app.set_screen(Screen::Stage);
        for old in crate::engine::pattern::RETIRED {
            app.session_pattern(old);
            let rows = frame(&mut app, 120, 30);
            assert!(rows[0].contains("pattern ◈ mantra-default") && !rows[0].contains(old), "{old}: {}", rows[0]);
        }
        app.session_pattern("mine");
        assert_eq!(app.pattern_name, "mine");
    }

    /// `set_setting` round-trips each kind of field through settings.toml, refuses what cannot
    /// be (a model that is not in /models) without changing anything, and says when it applies.
    #[test]
    fn set_setting_round_trips_through_settings_toml() {
        crate::web::snapshot::tests::with_mantra_home(|| {
            let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
            let (ev_tx, _ev_rx) = tokio::sync::mpsc::unbounded_channel();
            let mut app = App::new(Settings::default(), configured_fixture(), PathBuf::from("/nonexistent"), Hub::new(vec![], vec![], vec![], vec![], ev_tx, false), tx, false);
            assert_eq!(app.set_setting("default_model", "luna"), Ok("Solo's next session (/new) uses it"));
            assert_eq!(app.set_setting("approval_mode", "never"), Ok(""));
            assert_eq!(app.set_setting("default_pattern", "mantra-lean"), Ok(""));
            assert_eq!(app.pattern_name, "mantra-lean", "new runs use it at once");
            assert_eq!(app.set_setting("side_panel", "off"), Ok(""));
            assert!(!app.side_panel, "applied live");
            assert_eq!(app.set_setting("fps", "24"), Ok(""), "the event loop reads it live");
            assert_eq!(app.set_setting("web.tls", "on"), Ok("applies at the next start"));
            let disk = Settings::load_on_disk().unwrap();
            assert_eq!((disk.default_model.as_str(), disk.approval_mode.as_str(), disk.default_pattern.as_str(), disk.side_panel, disk.fps, disk.web.tls), ("luna", "never", "mantra-lean", false, 24, true));
            let e = app.set_setting("default_model", "nonesuch").unwrap_err();
            assert!(e.contains("not in /models"), "{e}");
            assert!(app.set_setting("default_pattern", "nonesuch").is_err());
            assert_eq!((app.settings.default_model.as_str(), app.settings.default_pattern.as_str()), ("luna", "mantra-lean"), "a refusal changes nothing");
            assert_eq!(Settings::load_on_disk().unwrap().default_model, "luna");
        });
    }

    /// The architect writes `[[groups]]` like any other part of a pattern, through the same
    /// validation: a good group lands in the Studio, a broken one comes back as INVALID.
    #[test]
    fn the_architect_writes_groups_through_the_pattern_validation() {
        let mut app = crate::web::snapshot::tests::test_app();
        let swarm = Pattern::builtin();
        assert!(!swarm.groups.is_empty(), "the built-in has its builders group");
        let (msg, ok) = app.architect_tool("mantra_write_pattern", &json!({"toml": swarm.to_toml()}));
        assert!(ok, "{msg}");
        assert_eq!(app.studio.pattern.groups, swarm.groups);
        let broken = swarm.to_toml().replace("push = \"urgent\"", "push = \"loud\"");
        let (msg, ok) = app.architect_tool("mantra_write_pattern", &json!({"toml": broken}));
        assert!(!ok && msg.starts_with("INVALID") && msg.contains("push"), "{msg}");
        assert_eq!(app.studio.pattern.groups, swarm.groups, "the Studio keeps the last good pattern");
    }

    #[tokio::test]
    async fn automatic_approval_never_grants_another_agents_requests() {
        let mut app = crate::web::snapshot::tests::test_app();
        let mut ask = Agent::new(1, "ask", "worker", PathBuf::new());
        ask.approval = "on-request".into();
        let mut unattended = Agent::new(2, "unattended", "worker", PathBuf::new());
        unattended.approval = "never".into();
        app.agents.insert(1, ask);
        app.agents.insert(2, unattended);
        let mut ask_rx = app.hub.test_register(1);
        let mut auto_rx = app.hub.test_register(2);
        for (agent, method) in [(1, "item/commandExecution/requestApproval"), (2, "item/commandExecution/requestApproval"), (1, "item/tool/requestUserInput")] {
            app.approvals.push(Approval { agent, id: json!(format!("{agent}-{method}")), method: method.into(), title: "test".into(), detail: "test".into(), params: json!({}), at: Instant::now() });
        }
        app.auto_approve_pending();
        assert_eq!(app.approvals.len(), 2);
        assert!(app.approvals.iter().all(|p| p.agent == 1));
        assert!(ask_rx.try_recv().is_err());
        assert!(matches!(auto_rx.try_recv(), Ok(Cmd::Respond { .. })));
    }

    #[tokio::test]
    async fn host_approval_changes_do_not_send_unsupported_backend_commands() {
        crate::web::snapshot::tests::with_mantra_home(|| {
            for backend in [crate::config::ProviderKind::Bija, crate::config::ProviderKind::ClaudeCode, crate::config::ProviderKind::Codex] {
                let mut app = crate::web::snapshot::tests::test_app();
                let mut agent = Agent::new(1, "solo", "solo", PathBuf::new());
                agent.backend = backend;
                agent.approval = "on-request".into();
                app.agents.insert(1, agent);
                app.solo = Some(1);
                let mut rx = app.hub.test_register(1);
                app.set_approval_mode("never");
                assert_eq!(app.agents[&1].approval, "never");
                assert_eq!(matches!(rx.try_recv(), Ok(Cmd::SetApproval(_))), backend == crate::config::ProviderKind::Codex);
            }
        });
    }

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
        let mut hub = Hub::new(vec![], vec![], vec![], vec![], ev_tx, false);
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
    fn solo_ctrl_c_interrupts_a_live_turn_without_discarding_a_draft() {
        let mut app = crate::web::snapshot::tests::test_app();
        let id = 41;
        app.solo = Some(id);
        let mut agent = Agent::new(id, "solo", "solo", PathBuf::new());
        agent.status = Status::Busy;
        agent.turn_active = true;
        app.agents.insert(id, agent);
        let mut rx = app.hub.test_register(id);
        app.input.set("next question");
        app.ctrl_c_press();
        assert_eq!(app.input.text(), "next question");
        assert!(app.agents[&id].stopped_by_user);
        assert!(matches!(rx.try_recv(), Ok(Cmd::Interrupt)));
    }

    #[test]
    fn solo_ctrl_f_sends_the_draft_into_the_live_turn_now() {
        let mut app = crate::web::snapshot::tests::test_app();
        let id = 42;
        app.solo = Some(id);
        let mut agent = Agent::new(id, "solo", "solo", PathBuf::new());
        agent.status = Status::Busy;
        agent.turn_active = true;
        app.agents.insert(id, agent);
        let mut rx = app.hub.test_register(id);
        app.input.set("please check this now");
        app.on_key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL));
        assert!(app.input.is_empty());
        assert!(matches!(rx.try_recv(), Ok(Cmd::Steer { text }) if text == "please check this now"));
    }

    #[test]
    fn solo_without_a_usable_model_does_not_spawn_a_phantom_default() {
        let mut app = crate::web::snapshot::tests::test_app();
        app.registry = Registry::defaults();
        app.settings.default_model = "sol".into();
        app.start_solo();
        assert!(app.solo.is_none());
        assert!(app.agents.is_empty());
        assert!(app.toast.as_ref().is_some_and(|(t, _, _)| t.contains("/models")));
    }

    #[test]
    fn solo_subagent_wait_returns_the_childs_final_answer_to_the_parent_tool_call() {
        let mut app = crate::web::snapshot::tests::test_app();
        app.solo = Some(1);
        let mut parent = Agent::new(1, "solo", "solo", PathBuf::new());
        parent.status = Status::Busy;
        parent.turn_active = true;
        let mut child = Agent::new(2, "subagent", "solo-subagent", PathBuf::new());
        child.status = Status::Busy;
        child.turn_active = true;
        child.final_message = Some("found the fix".into());
        app.agents.insert(1, parent);
        app.agents.insert(2, child);
        app.solo_children.insert(2, 1);
        let mut parent_rx = app.hub.test_register(1);
        let _child_rx = app.hub.test_register(2);
        assert!(app.solo_tool_call(1, json!("wait-1"), "mantra_subagent_wait", &json!({"agent": "2"})));
        assert!(parent_rx.try_recv().is_err(), "wait remains open while child is busy");
        app.on_turn_done(2, "completed", None, None);
        match parent_rx.try_recv() {
            Ok(Cmd::Respond { id, result }) => {
                assert_eq!(id, json!("wait-1"));
                assert_eq!(result.pointer("/contentItems/0/text").and_then(Value::as_str), Some("found the fix"));
                assert_eq!(result["success"], true);
            }
            other => panic!("expected child result response, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn bypass_permission_requires_a_new_session_and_children_inherit_the_live_permission() {
        crate::web::snapshot::tests::with_mantra_home(|| {
            let mut app = crate::web::snapshot::tests::test_app();
            app.start_solo();
            let first = app.solo.unwrap();
            let original = app.agents[&first].approval.clone();
            let note = app.set_setting("approval_mode", crate::config::BYPASS_SANDBOX_DANGER).unwrap();
            assert!(note.contains("/new"));
            assert_eq!(app.agents[&first].approval, original, "saving a setting cannot unsandbox a running process");
            app.start_solo();
            let parent = app.solo.unwrap();
            assert_eq!(app.agents[&parent].approval, crate::config::BYPASS_SANDBOX_DANGER);
            assert!(app.set_setting("approval_mode", "on-request").unwrap().contains("/new"));
            assert_eq!(app.agents[&parent].approval, crate::config::BYPASS_SANDBOX_DANGER, "leaving bypass also requires a fresh sandbox");
            let alias = app.settings.default_model.clone();
            assert!(app.solo_tool_call(parent, json!("spawn"), "mantra_subagent_start", &json!({"task": "check", "model": alias})));
            let child = *app.solo_children.keys().next().unwrap();
            assert_eq!(app.agents[&child].approval, crate::config::BYPASS_SANDBOX_DANGER, "a child inherits the running parent's choice, not a pending /new change");
            app.start_solo();
            assert_eq!(app.agents[&app.solo.unwrap()].approval, "on-request");
            assert!(app.solo_children.is_empty());
        });
    }

    /// Exercise the actual Codex JSON-RPC wire, including a resumed pattern agent. Never-ask
    /// alone keeps its sandbox; the explicit bypass changes both settings before thread creation.
    #[cfg(unix)]
    #[tokio::test]
    async fn codex_bypass_reaches_start_and_resume_without_changing_ordinary_never() {
        let dir = std::env::temp_dir().join(format!("mantra-bypass-wire-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("requests.jsonl");
        let script = r#"while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/^{"id":\([0-9]*\),.*/\1/p')
  case "$line" in
    *'"method":"initialize"'*) printf '{"id":%s,"result":{}}\n' "$id";;
    *'"method":"thread/start"'*|*'"method":"thread/resume"'*) printf '%s\n' "$line" > "$1"; printf '{"id":%s,"result":{"thread":{"id":"test-thread"},"model":"test"}}\n' "$id";;
  esac
done"#;
        let (ev_tx, mut ev_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut hub = Hub::new(vec!["sh".into(), "-c".into(), script.into(), "test".into(), log.to_string_lossy().into_owned()], vec![], vec![], vec![], ev_tx, false);
        let mut agents = BTreeMap::new();
        let mut reg = Registry::defaults();
        reg.add_codex_model("test-codex", "test").unwrap();
        for (permission, resume) in [("never", false), (crate::config::BYPASS_SANDBOX_DANGER, false), (crate::config::BYPASS_SANDBOX_DANGER, true)] {
            let id = spawn_agent(&mut hub, &mut agents, &reg, AgentOpts {
                name: "test".into(), role: "worker".into(), glyph: "◇".into(), color: "teal".into(),
                model_alias: "test-codex".into(), effort: None, cwd: dir.clone(),
                approval: permission.into(), sandbox: "read-only".into(), instructions: String::new(), tools: vec![],
                extra_writable: vec![], context_override: None, resume_thread: resume.then(|| "test-thread".into()),
                thread_name: None, network: false, writable_roots: vec![], mcp: vec![],
            });
            tokio::time::timeout(std::time::Duration::from_secs(10), async {
                loop {
                    match ev_rx.recv().await.expect("hub alive") {
                        HubEvent::Ready { agent, .. } if agent == id => break,
                        HubEvent::Crashed { agent, reason, .. } if agent == id => panic!("Codex wire mock failed: {reason}"),
                        _ => {}
                    }
                }
            }).await.expect("Codex handshake completes");
            let req: Value = serde_json::from_str(&std::fs::read_to_string(&log).unwrap()).unwrap();
            assert_eq!(req["method"], if resume { "thread/resume" } else { "thread/start" });
            assert_eq!(req["params"]["approvalPolicy"], "never");
            assert_eq!(req["params"]["sandbox"], if permission == "never" { "read-only" } else { "danger-full-access" });
            assert_eq!(agents[&id].approval, permission);
            hub.shutdown(id);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The agent list shows what an agent really runs at: an `auto` effort that reaches the spawn is
    /// resolved before anything is sent, and an engine-resolved level passes through unchanged.
    #[tokio::test]
    async fn a_spawned_agent_shows_its_resolved_effort_never_auto() {
        let (ev_tx, _ev_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut hub = Hub::new(vec!["true".into()], vec![], vec![], vec![], ev_tx, false);
        let mut agents = BTreeMap::new();
        let reg = Registry::defaults();
        let opts = |effort: Option<&str>| AgentOpts {
            name: "w".into(),
            role: "worker-small".into(),
            glyph: "◇".into(),
            color: "teal".into(),
            model_alias: "luna".into(),
            effort: effort.map(str::to_string),
            cwd: PathBuf::from("."),
            approval: "never".into(),
            sandbox: "workspace-write".into(),
            instructions: String::new(),
            tools: vec![],
            extra_writable: vec![],
            context_override: None,
            resume_thread: None,
            thread_name: Some("Notes sync · worker-small".into()),
            network: true,
            writable_roots: vec![],
            mcp: vec![],
        };
        let a = spawn_agent(&mut hub, &mut agents, &reg, opts(Some("auto")));
        assert_eq!(agents[&a].effort, "medium", "auto that reaches the spawn means the middle of the road");
        let b = spawn_agent(&mut hub, &mut agents, &reg, opts(Some("low")));
        assert_eq!(agents[&b].effort, "low", "the engine's resolved level is used as it is");
        let c = spawn_agent(&mut hub, &mut agents, &reg, opts(None));
        assert_eq!(agents[&c].effort, "medium", "no request: the model's default");
        assert!(agents.values().all(|x| x.effort != "auto"));
    }

    #[test]
    fn demo_mode_skips_provider_key_checks() {
        // The demo's mock backends never reach a provider: a default model on a custom provider
        // whose key variable is unset must neither warn about the key nor block a run.
        let mut reg = Registry::defaults();
        reg.providers.push(crate::config::ProviderEntry { id: "riti".into(), name: "Riti".into(), env_key: "MANTRA_TEST_NO_SUCH_VAR".into(), ..Default::default() });
        reg.models.push(crate::config::ModelEntry { alias: "testing".into(), provider: "riti".into(), model: "testing".into(), ..Default::default() });
        let settings = Settings { default_model: "testing".into(), ..Default::default() };
        let mut pattern = Pattern::builtin();
        for r in pattern.roles.values_mut() {
            r.model = "testing".into();
        }
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let (ev_tx, _ev_rx) = tokio::sync::mpsc::unbounded_channel();
        let dir = std::env::temp_dir().join(format!("mantra-demo-keys-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mk = |demo| App::new(settings.clone(), reg.clone(), dir.clone(), Hub::new(vec![], vec![], vec![], vec![], ev_tx.clone(), false), tx.clone(), demo);
        let real = mk(false);
        assert!(real.model_problem("testing").is_some(), "sanity: the key really is missing");
        assert!(!real.preflight(&pattern).is_empty());
        let demo = mk(true);
        assert_eq!(demo.model_problem("testing"), None);
        assert!(demo.preflight(&pattern).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
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
    fn cross_backend_switch_keeps_agent_task_queue_tools_and_permissions() {
        for backend in [crate::config::ProviderKind::ClaudeCode, crate::config::ProviderKind::Antigravity] {
        let mut reg = Registry::defaults();
        reg.providers.push(crate::config::ProviderEntry { id: "cc".into(), name: "Claude".into(), kind: backend, auth: "subscription".into(), ..Default::default() });
        reg.models.push(crate::config::ModelEntry { alias: "claude-model".into(), model: "claude-model".into(), provider: "cc".into(), ..Default::default() });
        let id = 1;
        let (mut hub, mut commands) = test_hub_with(id);
        hub.claude_cmd = vec!["mock-claude".into()];
        hub.antigravity_cmd = vec!["mock-agy".into()];
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(Settings::default(), reg, PathBuf::from("."), hub, tx, true);
        let mut a = Agent::new(id, "worker", "worker", PathBuf::from("."));
        a.model_alias = "sol".into();
        a.backend = crate::config::ProviderKind::Codex;
        a.thread_id = Some("incompatible-codex-id".into());
        a.turn_active = true;
        a.queued.push("also check tests".into());
        a.push_user("Build the parser without changing the wire format");
        a.launch_options = Some(Box::new(AgentOpts {
            name: "worker".into(), role: "worker".into(), glyph: "x".into(), color: "teal".into(),
            model_alias: "sol".into(), effort: Some("medium".into()), cwd: PathBuf::from("."),
            approval: "never".into(), sandbox: "workspace-write".into(), instructions: "Keep the public API stable".into(),
            tools: vec![json!({"name":"mantra_status"})], extra_writable: vec![], network: false,
            writable_roots: vec![], context_override: None, resume_thread: None,
            thread_name: Some("Run · worker".into()), mcp: vec![],
        }));
        app.agents.insert(id, a);
        assert_eq!(app.set_model(id, "no-such-alias"), Err("unknown model".into()));
        assert!(commands.try_recv().is_err());
        assert_eq!(app.set_model(id, "claude-model"), Ok(()));
        assert_eq!(app.agents.len(), 1);
        assert_eq!(app.agents[&id].backend, backend);
        assert_eq!(app.agents[&id].queued, ["also check tests"]);
        assert!(app.agents[&id].thread_id.is_none());
        assert!(app.agents[&id].awaiting_start, "messages arriving during the switch must queue for the continued turn");
        app.on_hub(HubEvent::Notif { agent: id, method: "turn/completed".into(), params: json!({"turn":{"status":"failed","error":{"message":"obsolete provider error"}}}) });
        assert_eq!(app.agents[&id].status, Status::relaunching());
        app.on_hub(HubEvent::Crashed { agent: id, reason: "new backend launch refused".into(), restarting: false, attempt: 3 });
        assert_eq!(app.agents[&id].status, Status::Crashed("new backend launch refused".into()), "new launch failures must stay visible before Ready");
        app.on_hub(HubEvent::Ready { agent: id, thread_id: "new-claude-session".into(), model: "claude-model".into(), resumed: false });
        app.on_hub(HubEvent::Notif { agent: id, method: "turn/started".into(), params: json!({}) });
        app.on_hub(HubEvent::Notif { agent: id, method: "turn/completed".into(), params: json!({"turn":{"status":"completed"}}) });
        assert_eq!(app.agents[&id].status, Status::Idle);
        assert!(app.agents[&id].retry_note.is_none());

        match commands.try_recv().unwrap() {
            Cmd::Reconfigure { spec, command, resume_text } => {
                assert_eq!(command, if backend == crate::config::ProviderKind::Antigravity { vec!["mock-agy"] } else { vec!["mock-claude"] });
                assert!(spec.resume_thread.is_none());
                assert_eq!(spec.approval, "never");
                assert_eq!(spec.sandbox, "workspace-write");
                assert_eq!(spec.dynamic_tools, vec![json!({"name":"mantra_status"})]);
                assert!(spec.developer_instructions.contains("Keep the public API stable"));
                assert!(spec.developer_instructions.contains("Build the parser without changing the wire format"));
                if backend == crate::config::ProviderKind::Antigravity {
                    assert_eq!(spec.antigravity.unwrap().system_prompt, spec.developer_instructions);
                    assert!(spec.extra_args.is_empty());
                    assert_eq!(app.agents[&id].ctx_window, None);
                } else {
                    assert_eq!(spec.claude.unwrap().system_prompt, spec.developer_instructions);
                }
                assert!(resume_text.unwrap().contains("Continue the active task"));
            }
            other => panic!("expected backend reconfiguration, got {other:?}"),
        }
        while commands.try_recv().is_ok() {}
        // The same backend's context limit is a process flag, so choosing a smaller model
        // must update its actual launch configuration as well as the displayed gauge.
        app.registry.models.push(crate::config::ModelEntry { alias: "claude-small".into(), model: "small".into(), provider: "cc".into(), context_window: Some(16_000), ..Default::default() });
        assert_eq!(app.set_model(id, "claude-small"), Ok(()));
        if backend == crate::config::ProviderKind::Antigravity {
            assert!(matches!(commands.try_recv(), Ok(Cmd::SetModel(model)) if model == "small"));
            assert!(matches!(commands.try_recv(), Ok(Cmd::SetEffort(_))));
            assert!(matches!(commands.try_recv(), Ok(Cmd::SetVision(_))));
            assert_eq!(app.agents[&id].ctx_window, None, "unsupported context overrides must never populate the gauge");
        } else {
            match commands.try_recv().unwrap() {
                Cmd::Reconfigure { spec, resume_text, .. } => {
                    assert_eq!(spec.backend, crate::hub::Backend::ClaudeCode);
                    assert_eq!(spec.claude.unwrap().autocompact, 16_000);
                    assert!(resume_text.is_none(), "an idle agent must stay idle");
                }
                other => panic!("expected context limit relaunch, got {other:?}"),
            }
            assert_eq!(app.agents[&id].ctx_window, Some(16_000));
        }
        assert_eq!(app.agents[&id].launch_options.as_ref().unwrap().model_alias, "claude-small");
        }
    }

    /// Solo keeps its logical identity and transcript when the backend changes.
    #[tokio::test]
    async fn solo_changes_backend_in_place_with_a_fresh_backend_session() {
        crate::web::snapshot::tests::with_mantra_home(|| {
            let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
            let (ev_tx, _ev_rx) = tokio::sync::mpsc::unbounded_channel();
            let hub = Hub::new(vec![], vec![], vec![], vec![], ev_tx, false);
            let mut reg = Registry::defaults();
            reg.providers.push(crate::config::bija_default_provider());
            reg.models.extend(crate::config::bija_default_model_entries("bija"));
            reg.add_codex_model("sol-codex", "account-sol").unwrap();
            let mut app = App::new(Settings::default(), reg, PathBuf::from("."), hub, tx, true);
            app.start_solo();
            let first = app.solo.expect("Solo is up");
            assert_eq!(app.agents[&first].backend, crate::config::ProviderKind::Bija, "Solo defaults to bija");
            assert_eq!(app.set_model(first, "sol-codex"), Ok(()));
            let second = app.solo.expect("a new Solo");
            assert_eq!(first, second);
            assert_eq!(app.agents[&second].backend, crate::config::ProviderKind::Codex);
            assert_eq!(app.settings.default_model, "sol-codex");
            assert_eq!(app.set_model(second, "luna-api"), Ok(()), "and back, through a 0.7 spelling");
            assert_eq!(app.agents[&app.solo.expect("Solo")].backend, crate::config::ProviderKind::Bija);
            assert_eq!(app.settings.default_model, "luna", "saved under the alias it resolved to");
        });
    }

    /// A settings change goes through one door: validated, applied live where it can be, refused
    /// whole when the model or pattern it names does not exist.
    #[tokio::test]
    async fn settings_are_set_validated_and_applied_through_one_door() {
        crate::web::snapshot::tests::with_mantra_home(|| {
            let mut app = crate::web::snapshot::tests::test_app();
            app.start_solo();
            let solo = app.solo.expect("Solo is up");
            assert_eq!(app.set_setting("approval_mode", "never"), Ok(""));
            assert_eq!((app.settings.approval_mode.as_str(), app.agents[&solo].approval.as_str()), ("never", "never"), "Solo's thread follows at once");
            assert_eq!(app.set_setting("default_model", "luna"), Ok("Solo's next session (/new) uses it"));
            assert_eq!(app.settings.default_model, "luna");
            assert!(app.set_setting("default_model", "nope").unwrap_err().contains("not in /models"));
            assert_eq!(app.settings.default_model, "luna", "a refusal changes nothing");
            assert!(app.set_setting("default_pattern", "no-such-pattern").unwrap_err().contains("not found"));
            assert_eq!(app.set_setting("default_pattern", "mantra-lean"), Ok(""));
            assert_eq!(app.pattern_name, "mantra-lean", "new runs use it");
            let panel = app.side_panel;
            assert_eq!(app.set_setting("side_panel", if panel { "off" } else { "on" }), Ok(""));
            assert_eq!(app.side_panel, !panel);
            assert!(app.set_setting("fps", "1000").is_err());
        });
    }

    /// `SANDBOX_FALLBACK` is a process-wide static (the `Ctx`/`start_solo` spawn sites have no
    /// other way to reach `App`) — a test that trips it must reset it, or every test afterward in
    /// this binary silently spawns Codex agents `danger-full-access`.
    #[test]
    fn sandbox_warning_downgrades_codex_to_danger_full_access_only() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let (ev_tx, _ev_rx) = tokio::sync::mpsc::unbounded_channel();
        let hub = Hub::new(vec![], vec![], vec![], vec![], ev_tx, false);
        let mut app = App::new(Settings::default(), Registry::defaults(), PathBuf::from("."), hub, tx, true);
        app.set_sandbox_warning("bwrap: setting up uid map: Permission denied".into());
        assert_eq!(role_sandbox("workspace-write", crate::config::ProviderKind::Codex), "danger-full-access");
        assert_eq!(role_sandbox("workspace-write", crate::config::ProviderKind::ClaudeCode), "workspace-write", "Claude never sandboxes through bwrap");
        assert_eq!(role_sandbox("workspace-write", crate::config::ProviderKind::Bija), "workspace-write", "bija probes bwrap itself and runs commands plainly without it");
        reset_sandbox_fallback_for_test();
        assert_eq!(role_sandbox("workspace-write", crate::config::ProviderKind::Codex), "workspace-write");
    }

    /// A Bija agent gets its provider's URL, wire and key name and the role's window, prompt and
    /// sandbox in bija's words — and the key itself only in the child's environment.
    #[test]
    fn a_bija_spawn_carries_provider_and_role_and_the_key_only_in_the_environment() {
        let bija = crate::config::ProviderKind::Bija;
        let mut reg = Registry { models: vec![], providers: vec![] };
        reg.providers.push(crate::config::ProviderEntry { id: "gw".into(), kind: bija, base_url: " https://gw.dev/v1 ".into(), wire: "anthropic".into(), api_key: Some("sk-secret".into()), ..Default::default() });
        reg.providers.push(crate::config::ProviderEntry { id: "local".into(), kind: bija, base_url: "http://127.0.0.1:11434/v1".into(), ..Default::default() });
        reg.models.push(crate::config::ModelEntry { alias: "big".into(), provider: "gw".into(), model: "big-1".into(), ..Default::default() });
        reg.models.push(crate::config::ModelEntry { alias: "small".into(), provider: "local".into(), model: "local-model".into(), ..Default::default() });
        let o = AgentOpts {
            name: "w".into(),
            role: "worker".into(),
            glyph: "·".into(),
            color: "gray".into(),
            model_alias: "big".into(),
            effort: None,
            cwd: PathBuf::from("/work"),
            approval: "never".into(),
            sandbox: "danger-full-access".into(),
            instructions: "do the task".into(),
            tools: vec![],
            extra_writable: vec![PathBuf::from("/integ")],
            network: true,
            writable_roots: vec![],
            context_override: None,
            resume_thread: None,
            thread_name: None,
            mcp: vec![],
        };
        let (s, envs) = bija_spawn(&reg, &reg.resolve("big"), &o, 128_000, 80);
        assert_eq!((s.base_url.as_str(), s.wire.as_str(), s.api_key_env.as_deref()), ("https://gw.dev/v1", "anthropic", Some("MANTRA_GW_API_KEY")));
        assert_eq!(envs, vec![("MANTRA_GW_API_KEY".to_string(), "sk-secret".to_string()), ("BIJA_PROVIDER_HEADERS".to_string(), "{}".to_string())]);
        assert_eq!((s.context_window, s.compact_pct, s.system_prompt.as_str(), s.sandbox.as_str(), s.network), (128_000, 80, "do the task", "full", true));
        assert!(s.writable_roots.is_empty(), "full access needs no roots");
        assert_eq!(s.home, crate::config::home().join("bija"));
        let o = AgentOpts { model_alias: "small".into(), sandbox: "workspace-write".into(), ..o };
        let (s, envs) = bija_spawn(&reg, &reg.resolve("small"), &o, 200_000, 85);
        assert_eq!((s.wire.as_str(), s.api_key_env.as_deref(), envs.len()), ("openai-chat", None, 1), "a keyless local server still clears inherited provider headers");
        assert_eq!((s.sandbox.as_str(), s.writable_roots.clone()), ("workspace-write", vec![PathBuf::from("/integ")]));
    }

    #[test]
    fn bija_writable_roots_are_one_list_for_workspace_write_only() {
        let root = std::env::temp_dir().join(format!("mantra-bija-roots-{}", std::process::id()));
        let home = root.join("home");
        std::fs::create_dir_all(home.join("data")).unwrap();
        let extra = vec![root.clone(), root.clone()];
        let pattern = vec!["~/data".to_string(), "/no/such/dir".to_string()];
        assert_eq!(bija_writable_roots("workspace-write", &extra, &pattern, Some(&home)), vec![root.clone(), home.join("data")], "deduplicated, ~ expanded, missing dirs dropped");
        assert!(bija_writable_roots("read-only", &extra, &pattern, Some(&home)).is_empty());
        assert!(bija_writable_roots("danger-full-access", &extra, &pattern, Some(&home)).is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_workspace_write_agent_gets_one_writable_roots_list_and_network_when_allowed() {
        let root = std::env::temp_dir().join(format!("mantra-sandbox-overrides-{}", std::process::id()));
        let home = root.join("home");
        let cargo = home.join(".cargo");
        let extra = root.join("git-common");
        let pattern_root = home.join("pattern-root");
        for d in [&cargo, &extra, &pattern_root] {
            std::fs::create_dir_all(d).unwrap();
        }
        let pattern = vec!["~/.cargo".to_string(), "~/pattern-root".to_string(), "~/not-there".to_string()];
        let (cargo_s, extra_s, pattern_s) = (cargo.to_string_lossy().into_owned(), extra.to_string_lossy().into_owned(), pattern_root.to_string_lossy().into_owned());

        let args = sandbox_overrides("workspace-write", true, std::slice::from_ref(&extra), &pattern, std::slice::from_ref(&cargo), Some(&home));
        assert!(args.contains(&"sandbox_workspace_write.network_access=true".to_string()), "{args:?}");
        let lists: Vec<&String> = args.iter().filter(|a| a.starts_with("sandbox_workspace_write.writable_roots=")).collect();
        assert_eq!(lists.len(), 1, "one list, however many sources feed it: {args:?}");
        let list = lists[0];
        assert_eq!(list.matches(cargo_s.as_str()).count(), 1, "~/.cargo is both a pattern root and a toolchain root, listed once: {list}");
        assert!(list.contains(&pattern_s) && list.contains(&extra_s), "{list}");
        assert!(!list.contains("not-there"), "a pattern root that does not exist is left out: {list}");

        let read_only = sandbox_overrides("read-only", true, std::slice::from_ref(&extra), &pattern, std::slice::from_ref(&cargo), Some(&home));
        assert!(!read_only.iter().any(|a| a.contains("network_access")), "{read_only:?}");
        assert!(read_only.iter().any(|a| a.contains(&extra_s)), "the spawn's own roots still go in: {read_only:?}");
        assert!(!read_only.iter().any(|a| a.contains(&cargo_s)), "toolchain and pattern roots are for workspace-write only: {read_only:?}");

        let no_network = sandbox_overrides("workspace-write", false, &[], &pattern, &[], Some(&home));
        assert!(!no_network.iter().any(|a| a.contains("network_access")), "{no_network:?}");
        assert!(no_network.iter().any(|a| a.contains(&pattern_s)), "the pattern's roots do not depend on network: {no_network:?}");
        let _ = std::fs::remove_dir_all(&root);
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
        let hub = Hub::new(vec![], vec![], vec![], vec![], ev_tx, false);
        App::new(Settings::default(), Registry::default(), PathBuf::from("."), hub, tx, true)
    }

    fn a_run() -> Run {
        Run::new(PathBuf::from("."), Pattern::builtin(), "build a thing".into())
    }

    /// The reported bug: a paused run's header ("⏱ 4m20s · halted 1s") stayed frozen until a
    /// keypress, because nothing animates while halted and the event loop only drew on events.
    /// The loop now ticks once a second whenever a clock is on screen — and that is only then.
    #[test]
    fn a_halted_run_still_wants_a_clock_tick() {
        let mut app = screen_app();
        assert!(!app.clocks_visible(), "nothing on screen shows a clock");
        assert!(!app.animating());
        let mut run = a_run();
        let mut ctx = Ctxt { hub: &mut app.hub, agents: &mut app.agents, registry: &app.registry, settings: &app.settings, tx: &app.tx, notes: &mut app.notes };
        run.toggle_pause(&mut ctx);
        // the "run paused" pulse line fades for a moment; the bug shows once that is over
        for p in run.pulse.iter_mut() {
            p.at = Instant::now() - Duration::from_secs(5);
        }
        app.run = Some(run);
        assert!(!app.animating(), "a paused run is not an animation");
        assert!(app.clocks_visible(), "but its clocks still tick");
    }

    /// A halt about one agent selects it once, so the band's `m` / `r` reach the agent it
    /// names; the user moving the selection afterwards is not undone, and a new halt moves it again.
    #[test]
    fn a_halt_selects_the_agent_it_names_once() {
        let mut app = screen_app();
        let mut run = a_run();
        run.planner = Some(1);
        run.orchestrator = Some(3);
        let halt = |agent| crate::engine::run::Halt { reason: crate::engine::run::HaltReason::ProviderRejected, agent, message: "rejected".into(), full: "rejected".into(), since: Instant::now() };
        run.halt = Some(halt(Some(3)));
        app.run = Some(run);
        app.select_halted_agent();
        assert_eq!(app.stage_nodes()[app.sel], 3, "the halted agent is selected");
        app.sel = 0;
        app.select_halted_agent();
        assert_eq!(app.sel, 0, "the same halt does not pull the selection back");
        app.run.as_mut().unwrap().halt = Some(halt(None));
        app.select_halted_agent();
        assert_eq!(app.sel, 0, "a halt about no one agent leaves the selection");
        std::thread::sleep(Duration::from_millis(2));
        app.run.as_mut().unwrap().halt = Some(halt(Some(3)));
        app.select_halted_agent();
        assert_eq!(app.stage_nodes()[app.sel], 3, "a new halt moves it again");
    }

    #[test]
    fn run_composer_queue_controls_target_the_planner_from_every_view() {
        use crate::web::snapshot::tests::{add_agent, worker};
        use crate::engine::run::WState;
        let mut app = screen_app();
        let mut run = a_run();
        run.planner = Some(1);
        run.workers.push(worker("task", 1, Some(2), WState::Running));
        run.stage = Stage::Planning;
        app.run = Some(run);
        add_agent(&mut app, 1, "planner");
        add_agent(&mut app, 2, "task");
        app.agents.get_mut(&1).unwrap().turn_active = true;
        app.agents.get_mut(&1).unwrap().status = Status::Busy;
        let mut planner_rx = app.hub.test_register(1);
        let mut worker_rx = app.hub.test_register(2);
        app.agents.get_mut(&2).unwrap().queued.push("worker internal queue".into());
        for screen in [Screen::Zoom(2), Screen::Stage] {
            app.screen = screen;
            assert_eq!(app.composer_agent(), Some(1));
            app.agents.get_mut(&1).unwrap().queued.push("queued feedback".into());
            app.input.set("new feedback");
            app.force_send();
            assert!(app.input.is_empty());
            assert!(matches!(planner_rx.try_recv(), Ok(Cmd::Steer { text }) if text.contains("queued feedback") && text.contains("new feedback")));
            assert!(worker_rx.try_recv().is_err());
            app.agents.get_mut(&1).unwrap().queued.push("edit feedback".into());
            app.on_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
            assert_eq!(app.input.text(), "edit feedback");
            app.input.clear();
            app.agents.get_mut(&1).unwrap().queued.push("discard feedback".into());
            app.discard_queue();
            assert!(app.agents[&1].queued.is_empty());
            assert_eq!(app.agents[&2].queued, vec!["worker internal queue".to_string()]);
        }
    }

    #[test]
    fn arrows_follow_visible_card_geometry_and_skip_hidden_agents() {
        use crate::engine::run::WState;
        use crate::web::snapshot::tests::worker;
        let mut app = screen_app();
        let mut run = a_run();
        run.planner = Some(1);
        run.manager = Some(2);
        run.orchestrator = Some(3);
        run.workers.push(worker("left", 1, Some(10), WState::Running));
        run.workers.push(worker("right", 1, Some(11), WState::Running));
        run.gate_agent = Some(20);
        app.run = Some(run);
        app.screen = Screen::Stage;
        // The list is planner, manager, orchestrator, workers, gate. The screen is different:
        // planner -> orchestrator -> manager across the top, then workers, then the gate.
        app.stage_nav = vec![(1, 10, 0), (3, 40, 0), (2, 70, 0), (10, 25, 7), (11, 55, 7), (20, 40, 15)];
        app.canvas_focus = true;
        app.stage_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        assert_eq!(app.focus_agent(), Some(3), "right follows the screen, not the role list");
        app.stage_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(app.focus_agent(), Some(10), "down reaches the nearest worker row before the centered gate");
        app.stage_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        assert_eq!(app.focus_agent(), Some(11));
        app.stage_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(app.focus_agent(), Some(20));
        // A narrow frame only shows the worker list. Fixed roles and offscreen cards are skipped.
        app.stage_nav = vec![(10, 20, 0), (11, 20, 1)];
        app.stage_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(app.focus_agent(), Some(10));
        app.stage_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(app.focus_agent(), Some(11));
        app.stage_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(app.focus_agent(), Some(11), "bottom edge stays on a visible card");
    }

    /// The reported bug: a task retried once had one card on the overview but two entries in
    /// `stage_nodes`, so the arrows and `1`–`9` counted a card that was not there — selecting the
    /// visible card zoomed into the cancelled first attempt, and ↓ "moved" to a same-named card.
    /// One node per task, the attempt the card draws, in plan order.
    #[test]
    fn stage_nodes_count_one_node_per_task_the_attempt_its_card_draws() {
        use crate::engine::plan::{Phase, Plan, Task};
        use crate::engine::run::{PhaseStep, WState};
        use crate::web::snapshot::tests::worker as attempt;
        let mut app = screen_app();
        let mut run = a_run();
        run.planner = Some(1);
        run.orchestrator = Some(3);
        let task = |id: &str| Task { id: id.into(), role: "worker-small".into(), ..Default::default() };
        run.plan = Some(Plan { phases: vec![Phase { id: "p1".into(), tasks: vec![task("p1-tooling"), task("p1-api"), task("p1-ui")], ..Default::default() }], ..Default::default() });
        run.stage = Stage::Phase { idx: 0, step: PhaseStep::Orchestrating };
        // p1-api was spawned first, then p1-tooling, whose first attempt was cancelled and retried
        run.workers.push(attempt("p1-api", 1, Some(12), WState::Running));
        run.workers.push(attempt("p1-tooling", 1, Some(10), WState::Cancelled));
        run.workers.push(attempt("p1-tooling", 2, Some(11), WState::Running));
        run.workers.push(attempt("p1-ui", 1, None, WState::Queued));
        app.run = Some(run);
        assert_eq!(app.stage_nodes(), vec![1, 3, 11, 12], "one node per task, the newest attempt, in plan order; a queued task has no card and no node");
        assert_eq!(app.stage_nodes().iter().position(|a| *a == 11), Some(2), "p1-tooling's node is where its card is: right after the fixed roles");
        // ↓ from p1-tooling lands on p1-api, not on p1-tooling's cancelled attempt
        app.sel = 2;
        app.canvas_focus = true;
        app.stage_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(app.stage_nodes()[app.sel], 12);
        // a newest attempt still queued hides the task's older agent as its card would
        app.run.as_mut().unwrap().workers.push(attempt("p1-api", 2, None, WState::Preparing));
        assert_eq!(app.stage_nodes(), vec![1, 3, 11]);
        // a fix worker outside the phase's tasks keeps a node after them, so it stays reachable
        app.run.as_mut().unwrap().workers.push(attempt("fix-1", 1, Some(20), WState::Running));
        app.run.as_mut().unwrap().gate_agent = Some(30);
        assert_eq!(app.stage_nodes(), vec![1, 3, 11, 20, 30]);
    }

    /// `r` on a worker: its new attempt has no node until its agent exists, so the shorter
    /// list would leave the selection on a neighbour. It follows the task and lands on the new
    /// attempt's card when that attempt has an agent; moving the selection meanwhile cancels that.
    #[test]
    fn the_selection_follows_a_respawned_task_to_its_new_attempt() {
        use crate::engine::plan::{Phase, Plan, Task};
        use crate::engine::run::{PhaseStep, WState};
        use crate::web::snapshot::tests::worker as attempt;
        let mut app = screen_app();
        let mut run = a_run();
        run.orchestrator = Some(3);
        let task = |id: &str| Task { id: id.into(), role: "worker-small".into(), ..Default::default() };
        run.plan = Some(Plan { phases: vec![Phase { id: "p1".into(), tasks: vec![task("p1-api"), task("p1-ui")], ..Default::default() }], ..Default::default() });
        run.stage = Stage::Phase { idx: 0, step: PhaseStep::Orchestrating };
        run.workers.push(attempt("p1-api", 1, Some(10), WState::Running));
        run.workers.push(attempt("p1-ui", 1, Some(11), WState::Cancelled));
        run.workers.push(attempt("p1-ui", 2, None, WState::Preparing));
        app.run = Some(run);
        app.sel = 2;
        app.sel_follow = Some("p1-ui".into());
        app.tick();
        assert_eq!(app.stage_nodes(), vec![3, 10]);
        assert_eq!(app.sel, 1, "clamped to the last card while the new attempt has none");
        app.run.as_mut().unwrap().workers[2].agent = Some(12);
        app.tick();
        assert_eq!(app.stage_nodes()[app.sel], 12, "and back on p1-ui once it has");
        assert_eq!(app.sel_follow, None);
        // an arrow key in between is the user choosing: nothing follows any more
        app.run.as_mut().unwrap().workers.push(attempt("p1-ui", 3, None, WState::Preparing));
        app.sel_follow = Some("p1-ui".into());
        app.canvas_focus = true;
        app.stage_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
        app.run.as_mut().unwrap().workers[3].agent = Some(13);
        app.tick();
        assert_eq!(app.stage_nodes()[app.sel], 10, "the selection stays on p1-api, where ← put it");
        // and so is a 1–9 jump
        app.run.as_mut().unwrap().workers.push(attempt("p1-ui", 4, None, WState::Preparing));
        app.sel_follow = Some("p1-ui".into());
        app.canvas_focus = true;
        app.stage_key(KeyEvent::new(KeyCode::Char('2'), KeyModifiers::NONE));
        assert_eq!(app.stage_nodes()[app.sel], 10);
        app.run.as_mut().unwrap().workers[4].agent = Some(14);
        app.tick();
        assert_eq!(app.stage_nodes()[app.sel], 10, "the jump is the user choosing too");
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

    #[test]
    fn chat_drafts_follow_their_conversation_across_switches_and_zoom_escape() {
        let mut app = screen_app();
        app.solo = Some(1);
        for id in [1, 10, 11] { app.agents.insert(id, Agent::new(id, "draft test", "worker", PathBuf::new())); }
        app.input.set("solo draft");
        app.input.cur = 4;
        app.set_screen(Screen::Stage);
        assert!(app.input.is_empty(), "the stage must not inherit Solo's prompt");
        app.input.set("stage draft");
        app.set_screen(Screen::Zoom(10));
        assert!(app.input.is_empty(), "a worker must not inherit the run's prompt");
        app.input.set("worker 10 draft");
        app.set_screen(Screen::Zoom(11));
        assert!(app.input.is_empty(), "a different worker must start with its own input");
        app.input.set("worker 11 draft");
        app.set_screen(Screen::Zoom(10));
        assert_eq!(app.input.text(), "worker 10 draft");
        app.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(app.screen, Screen::Stage);
        assert_eq!(app.input.text(), "stage draft", "Escape keeps the zoom's draft for its return");
        app.set_screen(Screen::Zoom(10));
        assert_eq!(app.input.text(), "worker 10 draft");
        app.set_screen(Screen::Solo);
        assert_eq!(app.input.text(), "solo draft");
        assert_eq!(app.input.cur, 4, "the cursor stays with the draft");
        app.set_screen(Screen::Zoom(1));
        assert_eq!(app.input.text(), "solo draft", "zooming Solo is the same conversation");
        app.set_screen(Screen::Solo);
        app.agents.remove(&10);
        app.set_screen(Screen::Stage);
        assert!(!app.drafts.contains_key(&ChatDraft::Zoom(10)), "removed agents release their drafts");
        app.set_screen(Screen::Solo);
        app.enter_screen(Screen::Settings);
        assert!(app.input.is_empty());
        app.leave_screen();
        assert_eq!(app.input.text(), "solo draft", "config screens cannot take the chat input");
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

    /// Every row of a frame of `app` at `w`×`h`, trimmed.
    fn frame(app: &mut App, w: u16, h: u16) -> Vec<String> {
        let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(w, h)).unwrap();
        term.draw(|f| crate::ui::draw(f, app)).unwrap();
        let buf = term.backend().buffer();
        (0..buf.area.height).map(|y| (0..buf.area.width).map(|x| buf[(x, y)].symbol().to_string()).collect::<String>().trim_end().to_string()).collect()
    }

    fn halted_app() -> App {
        let mut app = screen_app();
        let mut run = a_run();
        run.title = "Ship the feature flags".into();
        run.planner = Some(1);
        run.halt = Some(crate::engine::run::Halt { reason: crate::engine::run::HaltReason::ProviderRejected, agent: Some(1), message: "rejected".into(), full: "rejected".into(), since: Instant::now() });
        app.agents.insert(1, Agent::new(1, "planner", "planner", PathBuf::new()));
        app.run = Some(run);
        app
    }

    /// A run that halts while zoomed in used to be invisible there: the zoomed header names the
    /// run and says it is halted, and esc lands on the overview with the band's keys live.
    #[test]
    fn a_halt_shows_in_the_zoomed_header_and_esc_makes_its_keys_live() {
        for w in [120, 80] {
            let mut app = halted_app();
            app.screen = Screen::Zoom(1);
            let top = frame(&mut app, w, 24).remove(0);
            assert!(top.contains("halted") && top.contains("e details"), "{w}: {top:?}");
            assert!(top.contains("Ship the feature"), "{w}: {top:?}");
        }
        // a long task id is cut so the marker still fits at 80 columns
        let mut app = halted_app();
        app.screen = Screen::Zoom(1);
        let a = app.agents.get_mut(&1).unwrap();
        (a.name, a.model_alias) = ("p2-session-store-and-auth-middleware-rewrite".into(), "luna".into());
        let top = frame(&mut app, 80, 24).remove(0);
        assert!(top.contains("halted") && top.ends_with("e details"), "{top:?}");
        let mut app = halted_app();
        app.screen = Screen::Zoom(1);
        app.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(app.screen, Screen::Stage);
        assert!(app.canvas_focus, "the band's e / space / m / r work on the next key");
    }

    /// In typing focus the halt band's keys would only type: it says how to reach them instead.
    #[test]
    fn the_halt_band_is_honest_about_typing_focus() {
        let mut app = halted_app();
        app.screen = Screen::Stage;
        app.canvas_focus = false;
        let rows = frame(&mut app, 120, 36);
        assert!(rows[2].contains("tab for keys") && !rows[2].contains("e details"), "{:?}", rows[2]);
        app.input.set("a draft");
        app.on_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert!(app.canvas_focus && app.input.text() == "a draft", "tab reaches the keys and keeps the draft");
        let rows = frame(&mut app, 120, 36);
        assert!(rows[2].contains("e details") && !rows[2].contains("tab for keys"), "{:?}", rows[2]);
    }

    /// Over-scrolling an overlay used to leave the stored offset past the end, so ↑ needed as
    /// many presses before the view moved: drawing clamps it.
    #[test]
    fn overlay_scroll_offsets_are_clamped_when_drawn() {
        let mut app = halted_app();
        app.overlays.push(Overlay::Help { scroll: 999 });
        frame(&mut app, 80, 24);
        let Some(Overlay::Help { scroll }) = app.overlays.last() else { panic!("help closed") };
        let max = *scroll;
        assert!(max > 0 && max < 999, "{max}");
        let last = frame(&mut app, 80, 24);
        app.overlays = vec![Overlay::Help { scroll: max - 1 }];
        assert_ne!(last, frame(&mut app, 80, 24), "one ↑ moves the view");
        app.overlays = vec![Overlay::Halt { scroll: 999 }];
        frame(&mut app, 80, 24);
        assert!(matches!(app.overlays.last(), Some(Overlay::Halt { scroll: 0 })), "a halt that fits does not scroll");
    }

    /// At 80 columns the model picker keeps effort, context and its whole key line.
    #[test]
    fn the_model_picker_fits_80_columns() {
        let mut app = screen_app();
        let efforts = vec!["low".to_string(), "medium".into(), "high".into()];
        app.registry.models.push(crate::config::ModelEntry { alias: "longalias9".into(), model: "a-rather-long-model-identifier".into(), efforts, default_effort: "medium".into(), ..Default::default() });
        app.overlays.push(Overlay::ModelPicker { sel: 0, target: None });
        let rows = frame(&mut app, 80, 30);
        let keys = rows.iter().find(|l| l.contains("model ") && l.contains("switch")).expect("key line");
        assert!(keys.contains("esc"), "{keys:?}");
        let row = rows.iter().find(|l| l.contains("longalias9")).expect("model row");
        assert!(row.contains("medium") && row.contains("ctx"), "{row:?}");
    }

    fn mcp_server(command: &str, args: &[&str], enabled: bool) -> crate::config::McpServer {
        crate::config::McpServer { command: command.into(), args: args.iter().map(|a| a.to_string()).collect(), env: Default::default(), enabled }
    }

    /// The user's own Codex servers stay off as before — all but one this agent is given, which
    /// gets its command and args instead of `enabled=false`.
    #[test]
    fn codex_keeps_the_users_servers_off_but_the_ones_the_agent_is_given() {
        let user = vec!["node_repl".to_string(), "blender".to_string()];
        assert_eq!(codex_mcp_overrides(&user, &[]), vec!["-c", "mcp_servers.node_repl.enabled=false", "-c", "mcp_servers.blender.enabled=false"], "no servers given: exactly as before");
        let out = codex_mcp_overrides(&user, &[("blender".into(), mcp_server("uvx", &["blender-mcp"], true))]);
        assert_eq!(out, vec!["-c", "mcp_servers.node_repl.enabled=false", "-c", "mcp_servers.blender.command=\"uvx\"", "-c", "mcp_servers.blender.args=[\"blender-mcp\"]", "-c", "mcp_servers.blender.enabled=true"], "switched on even when config.toml has it off");
    }

    /// Solo starts with `solo_mcp`'s servers — on the real spawn path, read back from the argv a
    /// stand-in Codex was started with — and `/settings` lists them; its MCP servers field changes
    /// `solo_mcp` on disk for the next Solo, refusing a server that is not configured.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn solo_gets_its_mcp_servers_and_settings_switch_them() {
        crate::web::snapshot::tests::with_mantra_home(|| {
            let dir = crate::config::home().join("solo-mcp");
            std::fs::create_dir_all(&dir).unwrap();
            let argv = |n: u32| dir.join(format!("argv-{n}"));
            // each launch writes its arguments to the next free argv-<n>, then waits to be killed
            let script = format!("n=1; while [ -e '{0}/argv-'$n ]; do n=$((n+1)); done; printf '%s\\n' \"$@\" > '{0}/argv-'$n; exec sleep 30", dir.display());
            let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
            let (ev_tx, _ev_rx) = tokio::sync::mpsc::unbounded_channel();
            let hub = Hub::new(vec!["sh".into(), "-c".into(), script, "sh".into()], vec![], vec![], vec![], ev_tx, false);
            let mut settings = Settings { default_model: "sol-codex".into(), ..Settings::default() };
            settings.mcp_servers.insert("blender".into(), mcp_server("uvx", &["blender-mcp"], true));
            settings.mcp_servers.insert("off".into(), mcp_server("x", &[], false));
            // on disk too, as outside the demo they always are: the app reads them back from there
            settings.save().unwrap();
            let mut reg = Registry::defaults();
            reg.add_codex_model("sol-codex", "account-sol").unwrap();
            let mut app = App::new(settings, reg, dir.clone(), hub, tx, false);
            let read = |n: u32| {
                for _ in 0..200 {
                    if let Ok(t) = std::fs::read_to_string(argv(n)) {
                        if !t.is_empty() {
                            return t;
                        }
                    }
                    std::thread::sleep(Duration::from_millis(25));
                }
                panic!("launch {n} never wrote its argv");
            };
            app.start_solo();
            assert_eq!(app.solo_mcp, vec!["blender".to_string()], "every enabled server, the disabled one not");
            let first = read(1);
            assert!(first.lines().any(|l| l == "mcp_servers.blender.command=\"uvx\""), "{first}");
            assert!(!first.contains("mcp_servers.off."), "{first}");

            app.open_settings();
            let listed: Vec<(String, bool)> = crate::ui::settings::rows(&app).into_iter().filter_map(|(_, r)| match r { crate::ui::settings::Row::Mcp { name, enabled, .. } => Some((name, enabled)), _ => None }).collect();
            assert_eq!(listed, vec![("blender".to_string(), true), ("off".to_string(), false)]);
            let e = app.set_setting("solo_mcp", "nope").unwrap_err();
            assert!(e.contains("no MCP server named nope"), "{e}");
            assert_eq!(app.settings.solo_mcp, vec!["*".to_string()], "refused, unchanged");
            assert_eq!(app.set_setting("solo_mcp", "none"), Ok("Solo's next session (/new) uses it"));
            assert!(Settings::load_on_disk().unwrap().solo_mcp.is_empty(), "saved");
            assert_eq!(app.solo_mcp, vec!["blender".to_string()], "the running Solo keeps what it started with");
            app.command("/new");
            assert!(app.solo_mcp.is_empty());
            assert!(!read(2).contains("mcp_servers.blender"), "the next Solo has none");
            app.set_setting("solo_mcp", "blender, off").unwrap();
            assert_eq!(app.settings.solo_mcp, vec!["blender".to_string(), "off".to_string()]);
            app.command("/mcp solo none");
            assert_eq!(app.toast.as_ref().map(|t| t.0.as_str()), Some("MCP servers are in /settings"));
            assert_eq!(app.settings.solo_mcp, vec!["blender".to_string(), "off".to_string()], "the old command changes nothing");
            let _ = app.shutdown();
        });
    }

    /// A server added with `mantra mcp add` while the app is open (written to settings.toml behind
    /// its back here) survives the app's next settings save — a model pick — instead of being
    /// written over with the table the app started with, and Solo's MCP servers field takes it;
    /// `/settings` lists it, and one removed on disk is gone from there too.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn servers_changed_on_disk_while_the_app_runs_are_seen_and_kept() {
        crate::web::snapshot::tests::with_mantra_home(|| {
            let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
            let (ev_tx, _ev_rx) = tokio::sync::mpsc::unbounded_channel();
            let hub = Hub::new(vec!["true".into()], vec![], vec![], vec![], ev_tx, false);
            Settings::default().save().unwrap();
            let mut app = App::new(Settings::load_on_disk().unwrap(), configured_fixture(), PathBuf::from("."), hub, tx, false);
            let listed = |app: &App| crate::ui::settings::rows(app).into_iter().filter_map(|(_, r)| match r { crate::ui::settings::Row::Mcp { name, .. } => Some(name), _ => None }).collect::<Vec<_>>();
            // another terminal: mantra mcp add blender -- uvx blender-mcp
            let mut disk = Settings::load_on_disk().unwrap();
            disk.mcp_servers.insert("blender".into(), mcp_server("uvx", &["blender-mcp"], true));
            disk.save().unwrap();
            app.settings.default_model = "luna".into();
            app.save_settings();
            let saved = Settings::load_on_disk().unwrap();
            assert_eq!(saved.default_model, "luna", "the app's own change is saved");
            assert!(saved.mcp_servers.contains_key("blender"), "and the server added meanwhile kept");
            app.set_setting("solo_mcp", "blender").expect("the server added meanwhile is known");
            assert_eq!(app.settings.solo_mcp, vec!["blender".to_string()]);
            app.open_settings();
            assert_eq!(listed(&app), vec!["blender".to_string()]);
            // mantra mcp remove blender
            let mut disk = Settings::load_on_disk().unwrap();
            disk.mcp_servers.clear();
            disk.save().unwrap();
            app.leave_screen();
            app.open_settings();
            assert!(listed(&app).is_empty());
            let _ = app.shutdown();
        });
    }

    /// Solo's vision tools run off the event loop: the request returns at once, and the answer
    /// comes back as `AppEvent::ToolAnswer` for `on_event` to hand on.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn solo_looks_at_an_image_off_the_event_loop() {
        let dir = std::env::temp_dir().join(format!("mantra-solo-vision-{}", crate::util::uuid_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("pic.png"), crate::util::tests::tiny_png(2, 2)).unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let (ev_tx, _ev_rx) = tokio::sync::mpsc::unbounded_channel();
        let hub = Hub::new(vec!["true".into()], vec![], vec![], vec![], ev_tx, false);
        let mut reg = Registry::defaults();
        reg.providers.push(crate::config::bija_default_provider());
        reg.models.extend(crate::config::bija_default_model_entries("bija"));
        let eye = reg.models.iter().find(|m| m.vision).map(|m| m.alias.clone()).expect("a configured alias that sees");
        let mut app = App::new(Settings::default(), reg, dir.clone(), hub, tx, true);
        let mut a = Agent::new(7, "solo", "solo", dir.clone());
        a.model_alias = eye;
        app.agents.insert(7, a);
        let call = json!({"tool": "mantra_view_image", "arguments": {"path": "pic.png"}});
        app.on_event(AppEvent::Hub(crate::hub::HubEvent::Request { agent: 7, id: json!(1), method: "item/tool/call".into(), params: call }));
        let answer = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let Some(AppEvent::ToolAnswer { agent, id, result }) = rx.recv().await {
                    return (agent, id, result);
                }
            }
        })
        .await
        .expect("an answer from off the loop");
        assert_eq!((answer.0, answer.1.clone()), (7, json!(1)));
        assert_eq!(answer.2["success"], json!(true), "{}", answer.2);
        assert_eq!(answer.2["contentItems"][1]["type"], json!("inputImage"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Claude Code runs strict (`--strict-mcp-config`) for a toolset of Mantra's — a run's agents,
    /// the architect — and for servers of Mantra's; a Solo agent with only the vision tools keeps
    /// the user's own `claude mcp` servers, as it had them before the vision tools came.
    #[test]
    fn claude_is_strict_except_for_solo_with_the_vision_tools_alone() {
        let o = |tools: Vec<Value>, mcp: Vec<(String, crate::config::McpServer)>| AgentOpts {
            name: "solo".into(),
            role: "solo".into(),
            glyph: "●".into(),
            color: "saffron".into(),
            model_alias: "sonnet".into(),
            effort: None,
            cwd: PathBuf::from("."),
            approval: "never".into(),
            sandbox: "workspace-write".into(),
            instructions: String::new(),
            tools,
            extra_writable: vec![],
            context_override: None,
            resume_thread: None,
            thread_name: None,
            network: true,
            writable_roots: vec![],
            mcp,
        };
        assert!(!claude_strict_mcp(&o(vec![], vec![])), "nothing of Mantra's at all");
        assert!(!claude_strict_mcp(&o(tools::vision_tools(), vec![])), "Solo with the vision tools");
        assert!(!claude_strict_mcp(&o(tools::solo_tools(), vec![])), "Solo's delegation tools keep the user's own Claude MCP servers");
        assert!(claude_strict_mcp(&o(tools::vision_tools(), vec![("blender".into(), mcp_server("uvx", &["blender-mcp"], true))])), "servers of Mantra's");
        let mut run_tools = tools::vision_tools();
        run_tools.extend(tools::worker_tools());
        assert!(claude_strict_mcp(&o(run_tools, vec![])), "a run's worker");
    }

    /// A server whose hand-written name cannot work reaches nobody: the journal names it for each
    /// role it would have reached, Solo's toast for Solo.
    #[test]
    fn a_server_name_that_cannot_work_is_journaled_and_toasted() {
        let mut s = Settings::default();
        s.mcp_servers.insert("blender.v2".into(), mcp_server("uvx", &["blender-mcp"], true));
        let mut p = Pattern::builtin();
        let first = p.roles.keys().next().cloned().unwrap();
        p.roles.get_mut(&first).unwrap().mcp = vec!["*".into()];
        let lines = mcp_missing_lines(&s, &p);
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(lines[0].contains(&format!("role {first} gets `blender.v2`")) && lines[0].contains("no /, spaces or dots"), "{}", lines[0]);
        assert!(mcp_refused_toast(&s).is_some_and(|t| t.contains("`blender.v2` skipped")), "Solo asks for * by default");
        s.solo_mcp = vec![];
        assert_eq!(mcp_refused_toast(&s), None);
    }

    /// A role asking for a server settings.toml does not have gets one journal line at the run's
    /// start and runs without it; in the demo every role gets the stand-in server, and only it.
    #[tokio::test]
    async fn a_role_asking_for_an_unconfigured_server_is_journaled_once_and_the_demo_gives_its_own() {
        crate::web::snapshot::tests::with_mantra_home(|| {
            let project = crate::config::home().join("mcp-project");
            std::fs::create_dir_all(&project).unwrap();
            let mut p = Pattern::builtin();
            p.name = "mcp-asks".into();
            let first = p.roles.keys().next().cloned().unwrap();
            for (name, r) in p.roles.iter_mut() {
                r.mcp = if *name == first { vec!["blender".into(), "nope".into(), "nope".into()] } else { vec!["*".into()] };
            }
            p.save().unwrap();
            let mut app = crate::web::snapshot::tests::test_app();
            app.project = project.clone();
            app.demo = false;
            app.settings.mcp_servers.insert("blender".into(), mcp_server("uvx", &["blender-mcp"], true));
            app.start_run_with("build it", Some("mcp-asks"), None);
            let run = app.run.as_ref().expect("the run started");
            let lines: Vec<String> = run.pulse.iter().filter(|l| l.text.starts_with("mcp:")).map(|l| l.text.clone()).collect();
            assert_eq!(lines, vec![format!("mcp: role {first} asks for `nope`, which is not configured — skipped")]);
            assert_eq!(run.pattern.roles[&first].mcp, vec!["blender".to_string(), "nope".into(), "nope".into()], "the run's pattern is the pattern");
            app.run = None;
            app.demo = true;
            app.start_run_with("build it", Some("mcp-asks"), None);
            let run = app.run.as_ref().expect("the demo run started");
            assert!(run.pattern.roles.values().all(|r| r.mcp == vec![DEMO_MCP.to_string()]), "{:?}", run.pattern.roles.values().map(|r| r.mcp.clone()).collect::<Vec<_>>());
            app.run = None;
            let _ = std::fs::remove_dir_all(&project);
        });
    }

    /// A Hub that could not make its bridge socket says so: a toast when the app starts, and one
    /// journal line for a run with a role on Claude Code — none for a run without one.
    #[tokio::test]
    async fn a_lost_bridge_is_toasted_at_startup_and_journaled_by_a_run_on_claude_code() {
        crate::web::snapshot::tests::with_mantra_home(|| {
            let project = crate::config::home().join("bridge-project");
            std::fs::create_dir_all(&project).unwrap();
            let file = crate::config::home().join("not-a-dir");
            std::fs::write(&file, "").unwrap();
            let home = std::env::var("MANTRA_HOME").unwrap();
            std::env::set_var("MANTRA_HOME", file.join("home"));
            let (ev_tx, _ev_rx) = tokio::sync::mpsc::unbounded_channel();
            let hub = Hub::new(vec![], vec![], vec![], vec![], ev_tx, true);
            std::env::set_var("MANTRA_HOME", home);
            let mut reg = Registry::defaults();
            reg.providers.push(crate::config::ProviderEntry { id: "cc".into(), name: "Claude Code".into(), kind: crate::config::ProviderKind::ClaudeCode, auth: "subscription".into(), ..Default::default() });
            reg.models.push(crate::config::ModelEntry { alias: "cc-model".into(), provider: "cc".into(), model: "cc-model".into(), ..Default::default() });
            let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
            let mut app = App::new(Settings::default(), reg, project.clone(), hub, tx, true);
            let toast = app.toast.as_ref().map(|t| t.0.clone()).unwrap_or_default();
            assert!(toast.starts_with("Claude Code / Antigravity agents have no Mantra tools: couldn't create") && toast.contains("not-a-dir"), "{toast}");
            let journaled = |app: &App| app.run.as_ref().expect("the run started").pulse.iter().filter(|l| l.text.starts_with("Claude Code / Antigravity agents have no Mantra tools")).count();
            let mut p = Pattern::builtin();
            p.name = "no-claude".into();
            p.save().unwrap();
            app.start_run_with("build it", Some("no-claude"), None);
            assert_eq!(journaled(&app), 0, "no role on Claude Code, nothing it lacks");
            app.run = None;
            let first = p.roles.keys().next().cloned().unwrap();
            p.roles.get_mut(&first).unwrap().fallback = Some("cc-model".into());
            p.name = "claude-fallback".into();
            p.save().unwrap();
            app.start_run_with("build it", Some("claude-fallback"), None);
            assert_eq!(journaled(&app), 1, "a fallback on Claude Code counts");
            app.run = None;
            let _ = std::fs::remove_dir_all(&project);
        });
    }
    #[tokio::test]
    async fn antigravity_spec_keeps_subscription_credentials_and_context_flags_out_of_codex() {
        let mut reg = Registry::defaults();
        reg.providers.push(crate::config::ProviderEntry { id: "agy".into(), kind: crate::config::ProviderKind::Antigravity, api_key: Some("must-not-leak".into()), ..Default::default() });
        reg.models.push(crate::config::ModelEntry { alias: "google".into(), provider: "agy".into(), model: "account-model".into(), efforts: vec!["low".into(), "medium".into(), "high".into()], ..Default::default() });
        let (mut hub, _commands) = test_hub_with(1);
        let opts = AgentOpts {
            name: "worker".into(), role: "worker".into(), glyph: "x".into(), color: "teal".into(), model_alias: "google".into(), effort: Some("max".into()), cwd: PathBuf::from("."), approval: "on-request".into(), sandbox: "workspace-write".into(), instructions: "Keep the public API stable".into(), tools: vec![json!({"name":"mantra_status"})], extra_writable: vec![], network: false, writable_roots: vec![], context_override: Some(123456), resume_thread: None, thread_name: None, mcp: vec![],
        };
        let (spec, _, lowered) = agent_spec(&mut hub, &reg, &opts);
        assert_eq!(spec.backend, crate::config::ProviderKind::Antigravity);
        assert_eq!(spec.model, "account-model");
        assert_eq!(spec.effort, "high");
        assert!(lowered);
        assert!(spec.extra_args.is_empty());
        assert!(spec.envs.is_empty());
        assert!(spec.claude.is_none() && spec.bija.is_none());
        assert_eq!(spec.antigravity.unwrap().system_prompt, opts.instructions);
    }

    #[test]
    fn antigravity_permission_changes_wait_for_a_new_native_process() {
        for (live, requested) in [("never", "on-request"), ("on-request", "never")] {
            let mut app = crate::web::snapshot::tests::test_app();
            let mut agent = Agent::new(1, "solo", "solo", PathBuf::new());
            agent.backend = crate::config::ProviderKind::Antigravity;
            agent.approval = live.into();
            app.agents.insert(1, agent);
            app.solo = Some(1);
            let mut rx = app.hub.test_register(1);
            app.approvals.push(Approval { agent: 1, id: json!("pending"), method: "item/commandExecution/requestApproval".into(), title: "test".into(), detail: "test".into(), params: json!({}), at: Instant::now() });
            app.set_approval_mode(requested);
            assert_eq!(app.settings.approval_mode, requested);
            assert_eq!(app.agents[&1].approval, live, "display must describe the process that is actually running");
            assert!(rx.try_recv().is_err(), "no unsupported control message or premature auto-approval");
            assert_eq!(app.approvals.len(), 1);
            assert!(app.toast.as_ref().unwrap().0.contains("/new"));
        }
    }

    #[test]
    fn antigravity_read_only_probe_and_architect_refuse_before_spawning() {
        let mut app = crate::web::snapshot::tests::test_app();
        app.registry = Registry { providers: vec![crate::config::ProviderEntry { id: "agy".into(), kind: crate::config::ProviderKind::Antigravity, ..Default::default() }], models: vec![crate::config::ModelEntry { alias: "google".into(), provider: "agy".into(), model: "account-model".into(), ..Default::default() }] };
        app.settings.default_model = "google".into();
        app.probe_model("google");
        assert!(app.probes.is_empty() && app.agents.is_empty());
        assert!(app.models_ui.status["google"].contains("read-only"));
        app.studio_architect_send("Create a pattern".into());
        assert!(app.studio.architect.is_none() && app.agents.is_empty());
        assert!(app.toast.as_ref().unwrap().0.contains("Choose another backend"));
        assert!(app.registry.alias_problem("google").is_none(), "the model remains selectable for supported roles");
    }

}
