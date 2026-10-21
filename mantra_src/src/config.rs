//! Settings and the model registry (models.toml).
//!
//! Everything lives in `$MANTRA_HOME` (default `~/.mantra` on Linux *and* macOS).
//! Nothing is ever written into your projects.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Every reasoning effort Codex knows about, ordered from lightest to heaviest.
pub const ALL_EFFORTS: &[&str] = &["minimal", "low", "medium", "high", "xhigh", "max", "ultra"];

pub fn home() -> PathBuf {
    if let Ok(h) = std::env::var("MANTRA_HOME") {
        if !h.trim().is_empty() {
            return PathBuf::from(h);
        }
    }
    static HOME: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    HOME.get_or_init(|| {
        let user = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
        let dir = user.join(".mantra");
        migrate_old_home(&user.join(".config").join("mantra"), &dir);
        dir
    })
    .clone()
}

/// One-time, non-destructive move of settings, models and patterns from the old
/// `~/.config/mantra`. The old directory is left in place (it may hold worktrees of an old run).
fn migrate_old_home(old: &Path, new: &Path) {
    if new.exists() || !old.is_dir() {
        return;
    }
    let _ = std::fs::create_dir_all(new);
    for f in ["settings.toml", "models.toml"] {
        if old.join(f).is_file() {
            let _ = std::fs::copy(old.join(f), new.join(f));
        }
    }
    if let Ok(rd) = std::fs::read_dir(old.join("patterns")) {
        let _ = std::fs::create_dir_all(new.join("patterns"));
        for e in rd.flatten() {
            if e.path().is_file() {
                let _ = std::fs::copy(e.path(), new.join("patterns").join(e.file_name()));
            }
        }
    }
    let _ = std::fs::write(new.join("MIGRATED.txt"), format!("Settings, models and patterns were copied from {} on first start.\nThe old directory can be deleted once no old runs need it.\n", old.display()));
}

/// Per-project run journals: `~/.mantra/runs/<project>-<hash>/` (never inside the project).
pub fn runs_dir(project: &Path) -> PathBuf {
    let abs = project.canonicalize().unwrap_or_else(|_| project.to_path_buf());
    let name = abs.file_name().map(|n| crate::util::slug(&n.to_string_lossy())).filter(|s| !s.is_empty()).unwrap_or_else(|| "project".into());
    // FNV-1a of the full path keeps two projects with the same folder name apart.
    let mut h: u64 = 0xcbf29ce484222325;
    for b in abs.to_string_lossy().bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    home().join("runs").join(format!("{name}-{:08x}", h as u32))
}

pub fn patterns_dir() -> PathBuf {
    home().join("patterns")
}

pub fn worktrees_dir() -> PathBuf {
    home().join("worktrees")
}

pub fn log_path() -> PathBuf {
    home().join("logs").join("mantra.log")
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Command used to launch one Codex app-server per agent.
    pub codex_command: Vec<String>,
    /// Command used to launch one `claude` process per Claude Code agent (WP10.3). A `--demo`
    /// override to a mock is WP10.6 territory, not wired up yet.
    pub claude_command: Vec<String>,
    /// Command used to launch one `bija` process per Bija agent. The default bare name is resolved
    /// at startup (`resolve_bija_command`): PATH, then the `bija` installed beside `mantra`, then
    /// `$MANTRA_BIJA` — the resolved path is used, never written back here.
    pub bija_command: Vec<String>,
    /// Model alias used by Solo mode.
    pub default_model: String,
    /// Approval mode for Solo: "untrusted" | "on-request" | "never".
    pub approval_mode: String,
    /// Sandbox for Solo: "read-only" | "workspace-write" | "danger-full-access".
    pub sandbox: String,
    /// Default pattern for Mandala runs.
    pub default_pattern: String,
    /// "auto" | "unicode" | "ascii"
    pub glyphs: String,
    /// "auto" | "truecolor" | "256" | "16"
    pub colors: String,
    pub reduce_motion: bool,
    pub mouse: bool,
    pub side_panel: bool,
    /// Frames per second while something animates (idle = 0 fps).
    pub fps: u32,
    /// Send desktop notifications (OSC 9 / bell) on run milestones.
    pub notify: bool,
    /// `[web]`: defaults for `--web` / `--remote` (flags override).
    pub web: WebSettings,
    /// Experimental features switched on (`mantra experimental enable <name>`); see `EXPERIMENTAL`.
    pub experimental: Vec<String>,
    /// `[mcp_servers.<name>]`: MCP servers agents may be given (Solo via `solo_mcp`, roles via
    /// `Role.mcp`). Nothing reaches an agent unless asked for.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub mcp_servers: BTreeMap<String, McpServer>,
    /// Which configured servers Solo agents get: `["*"]` every enabled one (the default), names, or none.
    pub solo_mcp: Vec<String>,
}

/// One stdio MCP server as `settings.toml` configures it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct McpServer {
    pub command: String,
    pub args: Vec<String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    pub enabled: bool,
}

impl Default for McpServer {
    fn default() -> Self {
        McpServer { command: String::new(), args: vec![], env: BTreeMap::new(), enabled: true }
    }
}

/// Akasha: shared notes for agent groups (`engine::akasha`).
pub const EXPERIMENTAL_AKASHA: &str = "akasha";
/// Every experimental feature there is, with one line each — what `mantra experimental` lists.
pub const EXPERIMENTAL: &[(&str, &str)] = &[(EXPERIMENTAL_AKASHA, "shared notes for agent groups (`[[groups]]` in a pattern)")];

impl Settings {
    pub fn experimental_on(&self, name: &str) -> bool {
        self.experimental.iter().any(|f| f == name)
    }

    /// The servers `wanted` asks for (a role's `mcp`, `solo_mcp`): `"*"` is every enabled one,
    /// names pick some. Only enabled servers, each once, in name order — so every backend gets
    /// the same list in the same order whatever the role wrote.
    ///
    /// A name `mcp_name_problem` refuses (one typed into settings.toml by hand: a dot, `__`,
    /// `mantra`) is never given — it would break Codex's `-c` key, the `mcp__<server>__<tool>`
    /// split or Mantra's own server; `mcp_refused` names those for the journal and the toast.
    pub fn mcp_for(&self, wanted: &[String]) -> Vec<(String, McpServer)> {
        self.mcp_asked(wanted).filter(|(name, _)| mcp_name_problem(name).is_none()).map(|(name, s)| (name.clone(), s.clone())).collect()
    }

    /// The enabled servers `wanted` asks for whose name cannot work, each with why.
    pub fn mcp_refused(&self, wanted: &[String]) -> Vec<(String, String)> {
        self.mcp_asked(wanted).filter_map(|(name, _)| mcp_name_problem(name).map(|why| (name.clone(), why))).collect()
    }

    /// The enabled servers `wanted` names or `"*"` covers, in name order.
    fn mcp_asked<'a>(&'a self, wanted: &'a [String]) -> impl Iterator<Item = (&'a String, &'a McpServer)> + 'a {
        let all = wanted.iter().any(|w| w.trim() == "*");
        self.mcp_servers.iter().filter(move |(name, s)| s.enabled && (all || wanted.iter().any(|w| w.trim() == name.as_str())))
    }

    /// The names `wanted` asks for that `[mcp_servers]` does not have, each once (`"*"` never is).
    /// A disabled server is configured, so it is not missing — it is just not launched.
    pub fn mcp_missing(&self, wanted: &[String]) -> Vec<String> {
        let mut out: Vec<String> = vec![];
        for w in wanted.iter().map(|w| w.trim()).filter(|w| !w.is_empty() && *w != "*") {
            if !self.mcp_servers.contains_key(w) && !out.iter().any(|o| o == w) {
                out.push(w.to_string());
            }
        }
        out
    }

    /// settings.toml as it is on disk, for a change that must be saved without the session's own
    /// (`--demo` points at its mocks, a missing Codex moves Solo to bija): the defaults when there
    /// is no file yet, and an error for one that does not parse — `load` would fall back to the
    /// defaults, and saving those would wipe every setting in it.
    pub fn load_on_disk() -> std::result::Result<Settings, String> {
        let path = home().join("settings.toml");
        match std::fs::read_to_string(&path) {
            Ok(text) => toml::from_str::<Settings>(&text).map_err(|e| format!("{} does not parse, fix it first: {e}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Settings::default()),
            Err(e) => Err(format!("{}: {e}", path.display())),
        }
    }
}

/// Why `name` cannot name an MCP server, or `None`. Stricter than "no `/` or spaces": it becomes a
/// TOML key in Codex's `-c mcp_servers.<name>…` (bare keys are letters, digits, `-` and `_`), the
/// `<server>` in `mcp__<server>__<tool>` (so no `__` inside it), and a key beside Mantra's own
/// `mantra` server in Claude Code's `--mcp-config`.
pub fn mcp_name_problem(name: &str) -> Option<String> {
    if name.is_empty() {
        return Some("a server needs a name".into());
    }
    if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        return Some(format!("'{name}': use letters, digits, - and _ only (no /, spaces or dots)"));
    }
    if name.contains("__") {
        return Some(format!("'{name}': no double underscore — tools are named mcp__<server>__<tool>"));
    }
    if name.starts_with('-') {
        return Some(format!("'{name}': a name cannot start with - (is a flag or `--` in the wrong place?)"));
    }
    if name == "mantra" {
        return Some("'mantra' is Mantra's own server — pick another name".into());
    }
    None
}

/// `[web]` in settings.toml: defaults for `--web` / `--remote`. Flags override.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct WebSettings {
    /// Listen address; "" → 127.0.0.1:7777.
    pub listen: String,
    /// Web UI password ("" → none). `MANTRA_WEB_PASSWORD` is the preferred way; when one is stored
    /// here settings.toml is written 0600.
    pub password: String,
    /// Self-signed TLS (ignored when `cert`/`key` are set).
    pub tls: bool,
    /// Your own PEM certificate + key (implies TLS).
    pub cert: String,
    pub key: String,
    /// Relay for `--remote`; "" → wss://remote.mantra.codes.
    pub relay: String,
    /// The website remote links open (it serves the web UI; the relay only forwards bytes), so the
    /// two can live on different hosts. "" → https://remote.mantra.codes for the default relay,
    /// else the relay's own origin.
    pub remote_site: String,
    /// Extra DNS names / IPs for the self-signed certificate.
    pub sans: Vec<String>,
    /// Web Push notifications to subscribed devices.
    pub push: bool,
    /// VAPID `sub` claim (mailto:… or https://…); "" → https://mantra.codes.
    pub contact: String,
}

impl Default for WebSettings {
    fn default() -> Self {
        Self { listen: String::new(), password: String::new(), tls: false, cert: String::new(), key: String::new(), relay: String::new(), remote_site: String::new(), sans: vec![], push: true, contact: String::new() }
    }
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            codex_command: vec!["codex".into(), "app-server".into()],
            claude_command: vec!["claude".into()],
            bija_command: vec!["bija".into()],
            // Solo runs on bija (SPEC §9.1): it ships beside mantra, so it is always there — Codex
            // need not be. A fresh install signed in to Codex with ChatGPT still picks `sol`.
            default_model: "sol-api".into(),
            approval_mode: "on-request".into(),
            sandbox: "workspace-write".into(),
            default_pattern: "mantra-default".into(),
            glyphs: "auto".into(),
            colors: "auto".into(),
            reduce_motion: false,
            mouse: true,
            side_panel: true,
            fps: 12,
            notify: true,
            web: WebSettings::default(),
            experimental: vec![],
            mcp_servers: BTreeMap::new(),
            solo_mcp: vec!["*".into()],
        }
    }
}

impl Settings {
    pub fn load() -> Settings {
        let p = home().join("settings.toml");
        match std::fs::read_to_string(&p) {
            Ok(s) => toml::from_str(&s).unwrap_or_else(|e| {
                crate::mlog!("settings.toml invalid ({e}); using defaults");
                Settings::default()
            }),
            Err(_) => {
                let s = Settings::default();
                let _ = s.save();
                s
            }
        }
    }
    /// Whether settings.toml holds a secret: a web password, or any value in an MCP server's
    /// `env` (a token, typically) — what makes `save` write it 0600.
    pub fn holds_secrets(&self) -> bool {
        !self.web.password.is_empty() || self.mcp_servers.values().any(|s| !s.env.is_empty())
    }

    pub fn save(&self) -> Result<()> {
        std::fs::create_dir_all(home())?;
        let body = toml::to_string_pretty(self)?;
        let body = format!("# Mantra settings\n{body}");
        // A secret in the file makes it restricted (same rule as a key in models.toml).
        if !self.holds_secrets() {
            atomic_write(&home().join("settings.toml"), &body)
        } else {
            atomic_write_restricted(&home().join("settings.toml"), &body)
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct ModelEntry {
    /// Short name used everywhere in Mantra (patterns, pickers).
    pub alias: String,
    /// Provider id ("openai" is built into Codex; others come from [[provider]]).
    pub provider: String,
    /// The model id sent to the provider.
    pub model: String,
    /// Override the context window Codex assumes (tokens).
    pub context_window: Option<u64>,
    /// Auto-compact when this % of the context window is used.
    pub auto_compact_percent: Option<u8>,
    /// Effort used when nothing else specifies one.
    pub default_effort: String,
    /// Efforts this model accepts (lightest → heaviest). Empty = the usual OpenAI set for
    /// OpenAI models, and *no effort control* for custom-provider models (none is sent).
    pub efforts: Vec<String>,
    pub note: String,
    /// The model takes images: the screenshot/view-image tools are offered only then. Always
    /// written, `false` too: a saved file with no `vision` key at all is one from before the key
    /// existed, which `Registry::add_missing_vision` switches on for the families that take images —
    /// a "no" the user chose in `/models` must not look like that.
    #[serde(default)]
    pub vision: bool,
}

impl Default for ModelEntry {
    fn default() -> Self {
        Self {
            alias: String::new(),
            provider: "openai".into(),
            model: String::new(),
            context_window: None,
            auto_compact_percent: None,
            default_effort: "medium".into(),
            efforts: vec![],
            note: String::new(),
            vision: false,
        }
    }
}

impl ModelEntry {
    pub fn is_custom_provider(&self) -> bool {
        !self.provider.is_empty() && self.provider != "openai"
    }
    /// The context window Mantra tells Codex to assume for this model: the configured value, or a
    /// conservative 200k when none is set (so auto-compaction always has something to aim at).
    pub fn effective_context(&self) -> u64 {
        self.context_window.unwrap_or(crate::discover::ASSUMED_CONTEXT)
    }
    /// The auto-compact threshold as a percent of `effective_context`: the configured value, or 85%.
    pub fn effective_compact_percent(&self) -> u8 {
        self.auto_compact_percent.unwrap_or(85)
    }
    pub fn efforts(&self) -> Vec<String> {
        if self.efforts.is_empty() && !self.is_custom_provider() {
            ["low", "medium", "high", "xhigh", "max"].iter().map(|s| s.to_string()).collect()
        } else {
            self.efforts.clone()
        }
    }
    /// Resolve an effort request ("max", "min", "high", …) against what the model supports.
    pub fn resolve_effort(&self, want: &str) -> String {
        let list = self.efforts();
        if list.is_empty() {
            return String::new(); // model has no reasoning-effort setting
        }
        let want = want.trim().to_lowercase();
        if want == crate::engine::pattern::EFFORT_AUTO {
            // "auto" is resolved per spawn by the engine (`pattern::auto_effort`) before it gets
            // here; reaching this point (a hand-typed `/effort auto`, a Studio edit) means "no
            // opinion", which is the middle of the road.
            return self.resolve_effort("medium");
        }
        if want == "max" && !list.iter().any(|e| e == "max") {
            // "max" means "the heaviest this model has" (ultra excluded: it changes behaviour).
            return list.iter().rev().find(|e| *e != "ultra").cloned().unwrap_or_else(|| "high".into());
        }
        if want == "min" {
            return list.first().cloned().unwrap_or_else(|| "low".into());
        }
        if list.iter().any(|e| *e == want) {
            return want;
        }
        // nearest by global order
        let rank = |e: &str| ALL_EFFORTS.iter().position(|x| *x == e).unwrap_or(2) as i32;
        let w = rank(&want);
        list.iter()
            .min_by_key(|e| (rank(e) - w).abs())
            .cloned()
            .unwrap_or_else(|| self.default_effort.clone())
    }
    /// `resolve_effort` plus whether the request had to be lowered: ("high", true) for "max" on a
    /// model whose `efforts` stop at high. The clamp is right, but callers surface it once so a
    /// role configured with an effort its model doesn't offer isn't silently running lighter.
    pub fn resolve_effort_lowered(&self, want: &str) -> (String, bool) {
        let used = self.resolve_effort(want);
        let want = want.trim().to_lowercase();
        let rank = |e: &str| ALL_EFFORTS.iter().position(|x| *x == e);
        let lowered = !used.is_empty() && used != want && matches!((rank(&want), rank(&used)), (Some(w), Some(u)) if w > u);
        (used, lowered)
    }
    pub fn step_effort(&self, cur: &str, delta: i32) -> String {
        let list = self.efforts();
        if list.is_empty() {
            return String::new();
        }
        let i = list.iter().position(|e| e == cur).unwrap_or(0) as i32;
        let j = (i + delta).clamp(0, list.len().saturating_sub(1) as i32) as usize;
        list.get(j).cloned().unwrap_or_else(|| cur.to_string())
    }
}

/// Which agent runtime a provider's models are launched through. `Codex` (the default, and the
/// only kind before WP10) covers `openai` plus any custom OpenAI-Responses-compatible provider;
/// `ClaudeCode` providers are launched via `hub::claude` (`run_claude_process`), `Bija` providers
/// via `hub::bija` — Mantra's own harness, for any Chat Completions or Messages API.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "kebab-case")]
pub enum ProviderKind {
    #[default]
    Codex,
    /// Written as `kind = "claude-code"`; `claude_code` and `claudecode` are accepted too.
    #[serde(alias = "claude_code", alias = "claudecode")]
    ClaudeCode,
    /// Written as `kind = "bija"` (`seed`, what the word means, is accepted too).
    #[serde(alias = "seed")]
    Bija,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct ProviderEntry {
    pub id: String,
    pub name: String,
    pub base_url: String,
    /// Name of the environment variable holding the API key (preferred: never touches disk).
    pub env_key: String,
    /// The key itself, stored directly — optional, alongside `env_key`. Written to `models.toml`
    /// (0600 on Unix) only when the user pastes one in here; never logged or put on argv.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    /// Codex only speaks the Responses API since Feb 2026.
    /// Legacy field: Codex only supports the Responses API, so Mantra always sends "responses".
    /// Still read from old files, never written.
    #[serde(skip_serializing)]
    pub wire_api: String,
    /// Codex (default) or ClaudeCode (WP10).
    pub kind: ProviderKind,
    /// ClaudeCode only: "subscription" (OAuth login, no `--bare`) | "api_key" (`--bare` +
    /// `ANTHROPIC_API_KEY`, `ANTHROPIC_BASE_URL` for third parties). Irrelevant for `Codex`.
    pub auth: String,
    /// Bija only: the API `base_url` speaks — `""`/`"openai"` (Chat Completions) or `"anthropic"`
    /// (Messages). Written only when set, so a models.toml without Bija providers never shows it.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub wire: String,
}

impl Default for ProviderEntry {
    fn default() -> Self {
        ProviderEntry { id: String::new(), name: String::new(), base_url: String::new(), env_key: String::new(), api_key: None, wire_api: String::new(), kind: ProviderKind::default(), auth: "subscription".into(), wire: String::new() }
    }
}

impl ProviderEntry {
    /// The environment variable name Codex is told (via `-c model_providers.<id>.env_key=`) to
    /// read this provider's key from: the configured `env_key`, or — when a key is pasted
    /// directly into `api_key` with no `env_key` set — a synthesized `MANTRA_<ID>_API_KEY`.
    pub fn env_var_name(&self) -> String {
        let e = self.env_key.trim();
        if !e.is_empty() {
            e.to_string()
        } else {
            format!("MANTRA_{}_API_KEY", self.id.trim().to_uppercase().replace(['-', '.'], "_"))
        }
    }
    /// Resolve the actual key value: the named environment variable if it names one and it is
    /// set and non-empty, else a key stored directly in `api_key`.
    pub fn resolve_key(&self) -> Option<String> {
        let e = self.env_key.trim();
        if !e.is_empty() {
            if let Ok(v) = std::env::var(e) {
                if !v.trim().is_empty() {
                    return Some(v);
                }
            }
        }
        self.api_key.clone().filter(|s| !s.trim().is_empty())
    }
    /// Whether a key is expected at all: always for Codex; for ClaudeCode only under `api_key`
    /// (a subscription login lives in `claude` itself); never for Bija, whose key is optional —
    /// a local Ollama or vLLM takes none, and a hosted API says so itself (HTTP 401).
    pub fn needs_key(&self) -> bool {
        match self.kind {
            ProviderKind::Codex => true,
            ProviderKind::ClaudeCode => self.auth == "api_key",
            ProviderKind::Bija => false,
        }
    }
    /// The `--wire` a Bija process is launched with: the Messages API for `anthropic`, else Chat
    /// Completions (`""` or `openai`; anything else is a `wire_problem` and never launched).
    pub fn bija_wire(&self) -> &'static str {
        if self.wire.trim().eq_ignore_ascii_case("anthropic") {
            "anthropic"
        } else {
            "openai-chat"
        }
    }
    /// Why a Bija provider's `wire` cannot be used: a typo would otherwise fall back to Chat
    /// Completions and fail against a Messages API with a confusing 404. `None` for every other
    /// kind, which has no wire to choose.
    pub fn wire_problem(&self) -> Option<String> {
        if self.kind != ProviderKind::Bija {
            return None;
        }
        let w = self.wire.trim().to_ascii_lowercase();
        (!matches!(w.as_str(), "" | "openai" | "anthropic")).then(|| format!("wire '{}' is neither openai nor anthropic", crate::util::trunc(self.wire.trim(), 20)))
    }
    /// Whether Test / Discover would put a request on the network for this provider: every Codex
    /// provider does; a ClaudeCode one only when a gateway `base_url` is set (otherwise discovery
    /// answers from the built-in catalog and needs no URL at all).
    pub fn needs_url(&self) -> bool {
        self.kind != ProviderKind::ClaudeCode || !self.base_url.trim().is_empty()
    }
    /// `host[:port]` a request to `base_url` goes to — shown next to Test / Discover so the user
    /// sees where the key is sent. `None` when the URL doesn't parse.
    pub fn host(&self) -> Option<String> {
        parse_base_url(&self.base_url).map(|u| u.authority)
    }
    /// Why Test / Discover are disabled for this provider — it is still a *draft* — or `None`
    /// when both may run. A draft is saved like any other row; it just never sends a request:
    /// the `base_url` is the `n` placeholder, malformed or empty (`url_problem`), or no
    /// credential is configured (`env_key` unset in the environment and no `api_key` stored).
    pub fn draft_reason(&self) -> Option<String> {
        if let Some(why) = self.url_problem().or_else(|| self.wire_problem()) {
            return Some(why);
        }
        if self.needs_key() && self.resolve_key().is_none() {
            let var = self.env_var_name();
            return Some(format!("no key: ${var} is not set and no api_key is stored"));
        }
        None
    }
    /// Why no request may be built for this provider's `base_url` — not by Test / Discover, not
    /// by a run: it is empty (Codex would default to api.openai.com), not a URL, or an RFC 2606
    /// placeholder (the `n` row's example.com). Plain http is the user's call: a LAN inference
    /// box (`http://192.168.1.20:8000/v1`) is the usual vLLM / Ollama setup. `None` = real.
    pub fn url_problem(&self) -> Option<String> {
        if !self.needs_url() {
            return None;
        }
        let raw = self.base_url.trim();
        if raw.is_empty() {
            return Some("base_url is empty".into());
        }
        let Some(u) = parse_base_url(raw) else {
            return Some(format!("base_url is not a URL: {}", crate::util::trunc(raw, 40)));
        };
        if u.is_placeholder() {
            return Some(format!("base_url is still the {} placeholder", u.host));
        }
        None
    }
}

/// The parts of a `base_url` that decide whether a request may go out (`ProviderEntry::url_problem`).
struct BaseUrl {
    /// `host[:port]`, as shown to the user.
    authority: String,
    /// Lower-cased host without port or IPv6 brackets.
    host: String,
}

impl BaseUrl {
    /// RFC 2606 reserved names — the `n` placeholder and anything else that can never be a provider.
    fn is_placeholder(&self) -> bool {
        let h = self.host.as_str();
        ["example.com", "example.net", "example.org"].iter().any(|d| h == *d || h.ends_with(&format!(".{d}"))) || h.ends_with(".example") || h.ends_with(".invalid")
    }
}

/// Minimal `scheme://host[:port][/path]` parse — enough to know where a request goes without
/// pulling in a URL crate. `None` for anything that isn't http(s) with a plausible host.
fn parse_base_url(raw: &str) -> Option<BaseUrl> {
    let (scheme, rest) = raw.trim().split_once("://")?;
    let scheme = scheme.to_ascii_lowercase();
    if scheme != "http" && scheme != "https" {
        return None;
    }
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    // Never accept credentials embedded in the URL: they'd end up on argv and in logs.
    if authority.is_empty() || authority.contains('@') || authority.chars().any(char::is_whitespace) {
        return None;
    }
    let host = if let Some(inner) = authority.strip_prefix('[') {
        inner.split(']').next().unwrap_or("").to_string()
    } else if let Some((h, port)) = authority.rsplit_once(':') {
        // `host:` and a non-numeric port are refused, not read as part of the host
        if port.is_empty() || !port.chars().all(|c| c.is_ascii_digit()) {
            return None;
        }
        h.to_string()
    } else {
        authority.to_string()
    };
    let host = host.to_ascii_lowercase();
    let plausible = !host.is_empty() && host.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | ':' | '_'));
    plausible.then(|| BaseUrl { authority: authority.to_string(), host })
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Registry {
    #[serde(rename = "model")]
    pub models: Vec<ModelEntry>,
    #[serde(rename = "provider")]
    pub providers: Vec<ProviderEntry>,
}

const REGISTRY_HEADER: &str = r#"# Mantra model registry.
# Each [[model]] gets a short alias you use in patterns and pickers.
# context_window / auto_compact_percent are passed to every agent's backend.
# A provider's `kind` picks the backend: "codex" (the default; the provider must speak the OpenAI
# *Responses* API), "claude-code", or "bija" — Mantra's built-in harness, for any Chat Completions
# API (wire = "openai", the default) or Messages API (wire = "anthropic"). A bija key is optional,
# so a local server works as is:
#
# [[provider]]
# id = "ollama"
# name = "Ollama"
# kind = "bija"
# base_url = "http://127.0.0.1:11434/v1"
#
# Example (Z.ai GLM through a Responses-compatible gateway):
#
# [[provider]]
# id = "zai"
# name = "Z.ai"
# base_url = "https://your-gateway.example/v1"
# env_key = "ZAI_API_KEY"
#
# [[model]]
# alias = "glm"
# provider = "zai"
# model = "glm-5.2"
# default_effort = "high"
# efforts = ["low", "medium", "high", "max"]

"#;

/// Cheap, side-effect-free check for a `claude` executable on `PATH` (no subprocess spawn).
/// Cached after the first call — `Registry::defaults()` may run more than once per process (a
/// missing/invalid `models.toml` falls back to it, and tests call it directly).
pub fn claude_available() -> bool {
    static AVAILABLE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *AVAILABLE.get_or_init(|| {
        std::env::var_os("PATH")
            .map(|paths| std::env::split_paths(&paths).any(|dir| dir.join("claude").is_file()))
            .unwrap_or(false)
    })
}

/// The default Claude Code models: alias -> model id, context window and note. Every Opus,
/// Sonnet and Fable model runs with a 1M window on Claude Code (only Haiku is 200k), and
/// `--autocompact` is derived from this figure; the gauge itself follows the window the CLI
/// reports for the account (`hub/claude.rs`), so an ineligible plan still reads right.
const CLAUDE_MODELS: &[(&str, &str, u64, &str)] = &[
    ("opus46", "claude-opus-4-6", 1_000_000, ""),
    ("opus48", "claude-opus-4-8", 1_000_000, ""),
    ("opus5", "claude-opus-5", 1_000_000, ""),
    ("sonnet5", "claude-sonnet-5", 1_000_000, ""),
    ("fable5", "claude-fable-5", 1_000_000, ""),
    ("fable51", "claude-fable-5-1", 1_000_000, ""),
    ("haiku45", "claude-haiku-4-5-20251001", 200_000, "fast & cheap"),
];

/// The default Claude Code models as full `ModelEntry`s for
/// `provider_id` (`v02plan.md` WP10.2). Used by `Registry::defaults()` (only when `claude` is on
/// PATH) and by `discover::claude_defaults` (WP10.5, any `ClaudeCode`-kind provider's `D` key), and
/// injected in memory by `--demo` (WP10.6) regardless of `claude_available()` so
/// `mantra --demo --pattern mantra-default-claude` needs no real `claude` install.
pub fn claude_default_model_entries(provider_id: &str) -> Vec<ModelEntry> {
    let claude_efforts: Vec<String> = ["low", "medium", "high", "xhigh", "max"].iter().map(|s| s.to_string()).collect();
    CLAUDE_MODELS
        .iter()
        .map(|(alias, model, context_window, note)| ModelEntry { vision: true,
            alias: (*alias).into(),
            provider: provider_id.to_string(),
            model: (*model).into(),
            context_window: Some(*context_window),
            auto_compact_percent: Some(85),
            default_effort: "high".into(),
            efforts: claude_efforts.clone(),
            note: (*note).into(),
        })
        .collect()
}

/// The one line of `codex login status` output that says whether we are logged in. Codex prints
/// warnings first (`WARNING: proceeding, even though we could not create PATH aliases …`), on the
/// same stream, so "the first line" used to show the warning instead of the verdict. Shared by
/// `doctor` (`main.rs`) and `codex_chatgpt_signed_in` below — one filter, not two to keep in sync.
pub fn login_status_line(out: &str) -> String {
    let meaningful = |l: &str| {
        let t = l.trim();
        let low = t.to_lowercase();
        !t.is_empty() && !low.starts_with("warning") && !low.starts_with("warn:") && !low.starts_with("error") && !low.contains("bubblewrap")
    };
    out.lines().filter(|l| meaningful(l)).last().or_else(|| out.lines().find(|l| !l.trim().is_empty())).unwrap_or("").trim().to_string()
}

/// True when the status line names a ChatGPT subscription rather than an API key ("Logged in
/// using ChatGPT" vs "Logged in using an API key").
fn is_chatgpt_login(line: &str) -> bool {
    let low = line.to_lowercase();
    low.contains("chatgpt") && !low.contains("api key")
}

/// Is Codex signed in through a ChatGPT subscription? Runs `<codex> login status` with a 5s hard
/// cap (a thread + `recv_timeout`, same shape as `main.rs`'s doctor probes — there is no shared
/// helper for it in this crate to reuse) and classifies the meaningful line. Never panics; a
/// missing binary, a non-zero exit or a timeout all read as "false" — a fresh install just keeps
/// the ordinary Codex-account defaults.
pub fn codex_chatgpt_signed_in(codex_command: &[String]) -> bool {
    let Some(cmd0) = codex_command.first().cloned() else { return false };
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(std::process::Command::new(&cmd0).args(["login", "status"]).output());
    });
    let Ok(Ok(o)) = rx.recv_timeout(std::time::Duration::from_secs(5)) else { return false };
    let text = format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
    is_chatgpt_login(&login_status_line(&text))
}

/// Codex's own config dir: `$CODEX_HOME`, else `~/.codex` — same resolution Codex itself uses.
fn codex_home() -> PathBuf {
    if let Ok(h) = std::env::var("CODEX_HOME") {
        if !h.trim().is_empty() {
            return PathBuf::from(h);
        }
    }
    dirs::home_dir().unwrap_or_else(|| PathBuf::from(".")).join(".codex")
}

/// Names of every MCP server the user's own `~/.codex/config.toml` declares. A Codex agent Mantra
/// spawns must not inherit these (it only knows the tools Mantra gave it, so a call routed to one
/// of the user's servers just fails as "unsupported call") — but `-c mcp_servers={}` is a no-op
/// against a config.toml that already populates the key (`-c` merges at dotted leaf paths, it does
/// not replace a table), so each name found here gets its own `-c mcp_servers.<name>.enabled=false`
/// instead. Never panics; a missing file, unreadable file or bad TOML all read as "none".
pub fn codex_user_mcp_server_names() -> Vec<String> {
    let Ok(s) = std::fs::read_to_string(codex_home().join("config.toml")) else { return vec![] };
    let Ok(v) = s.parse::<toml::Value>() else { return vec![] };
    v.get("mcp_servers").and_then(|t| t.as_table()).map(|t| t.keys().cloned().collect()).unwrap_or_default()
}

/// The name of the bija binary that ships beside `mantra` (SPEC §9.3).
pub fn bija_exe_name() -> String {
    format!("bija{}", std::env::consts::EXE_SUFFIX)
}

/// Where `resolve_bija_command` found bija — `doctor` says which.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BijaFound {
    /// `bija_command[0]` as configured: a path, or a name on PATH.
    Configured,
    /// The `bija` installed beside the running `mantra`.
    BesideMantra,
    /// `$MANTRA_BIJA`.
    Env,
    /// Nowhere: the configured command is kept, and a spawn of it says what is missing.
    Missing,
}

impl BijaFound {
    pub fn describe(self) -> &'static str {
        match self {
            BijaFound::Configured => "as configured",
            BijaFound::BesideMantra => "beside mantra",
            BijaFound::Env => "from $MANTRA_BIJA",
            BijaFound::Missing => "not found",
        }
    }
}

/// The command Bija agents are launched with (SPEC §9.1): `bija_command` as configured when its
/// first word is a path or a name on PATH; else the `bija` installed beside this `mantra` (a
/// release install and `cargo build` both put it there); else `$MANTRA_BIJA`. The words after
/// the first are kept in every case.
pub fn resolve_bija_command(configured: &[String]) -> (Vec<String>, BijaFound) {
    let beside = crate::util::self_exe().ok().and_then(|exe| exe.parent().map(|d| d.join(bija_exe_name())));
    let env = std::env::var("MANTRA_BIJA").ok();
    resolve_bija_from(configured, |name| crate::util::which(name).map(|p| p.is_file()).unwrap_or(false), beside.as_deref(), env.as_deref())
}

/// `resolve_bija_command` with its three lookups as parameters, so tests never touch PATH or the
/// process environment (shared by every test in the binary).
fn resolve_bija_from(configured: &[String], on_path: impl Fn(&str) -> bool, beside: Option<&Path>, env: Option<&str>) -> (Vec<String>, BijaFound) {
    let default = [bija_exe_name()];
    let cmd = if configured.first().map(|c| !c.trim().is_empty()).unwrap_or(false) { configured } else { &default[..] };
    let (first, rest) = (&cmd[0], &cmd[1..]);
    let with_rest = |prog: String| std::iter::once(prog).chain(rest.iter().cloned()).collect::<Vec<_>>();
    if first.contains(['/', std::path::MAIN_SEPARATOR]) || on_path(first) {
        return (cmd.to_vec(), BijaFound::Configured);
    }
    if let Some(p) = beside.filter(|p| p.is_file()) {
        return (with_rest(p.to_string_lossy().into_owned()), BijaFound::BesideMantra);
    }
    if let Some(e) = env.map(str::trim).filter(|e| !e.is_empty()) {
        return (with_rest(e.to_string()), BijaFound::Env);
    }
    (cmd.to_vec(), BijaFound::Missing)
}

/// The provider every registry ships for bija (SPEC §9.1): OpenAI's Chat Completions API with
/// `$OPENAI_API_KEY`. Pointing it anywhere else is a `/models` edit (URL, wire, key).
pub fn bija_default_provider() -> ProviderEntry {
    ProviderEntry { id: "bija".into(), name: "Bija".into(), base_url: "https://api.openai.com/v1".into(), env_key: "OPENAI_API_KEY".into(), kind: ProviderKind::Bija, ..Default::default() }
}

/// The alias a default model goes by through bija: `sol` → `sol-api`. `pattern::builtin_bija`
/// maps every role with it, so the two can never disagree on the naming.
pub fn bija_alias(alias: &str) -> String {
    format!("{alias}-api")
}

/// The default models again, reached through bija on `provider_id` with an API key instead of
/// Codex's own login: the same model ids, windows, efforts and notes under `-api` aliases.
pub fn bija_default_model_entries(provider_id: &str) -> Vec<ModelEntry> {
    codex_default_models()
        .into_iter()
        .map(|m| ModelEntry { vision: true, alias: bija_alias(&m.alias), provider: provider_id.to_string(), note: format!("{} — with an API key", m.note), ..m })
        .collect()
}

/// Whether a model id is one of a family that takes images: an id of the built-in default tables,
/// or one shaped like them (`gpt-…`, `claude-…`) that the tables have not caught up with. The
/// open-weight `gpt-oss-…` ids people run locally read text only, so they are not.
fn takes_images(model: &str) -> bool {
    let id = model.trim();
    if CLAUDE_MODELS.iter().any(|(_, m, _, _)| *m == id) || codex_default_models().iter().any(|m| m.model == id) {
        return true;
    }
    (id.starts_with("gpt-") && !id.starts_with("gpt-oss")) || id.starts_with("claude-")
}

/// The four models Codex's own account runs (provider `openai`).
fn codex_default_models() -> Vec<ModelEntry> {
    let all6 = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    vec![
        ModelEntry {
            alias: "astra".into(),
            model: "gpt-6-astra".into(),
            context_window: Some(272_000),
            auto_compact_percent: Some(85),
            default_effort: "high".into(),
            efforts: all6(&["low", "medium", "high", "xhigh", "max", "ultra"]),
            note: "most capable — planning & hard problems".into(),
            vision: true,
            ..Default::default()
        },
        ModelEntry {
            alias: "sol".into(),
            model: "gpt-5.6-sol".into(),
            context_window: Some(272_000),
            auto_compact_percent: Some(85),
            default_effort: "medium".into(),
            efforts: all6(&["low", "medium", "high", "xhigh", "max", "ultra"]),
            note: "frontier agentic coding".into(),
            vision: true,
            ..Default::default()
        },
        ModelEntry {
            alias: "terra".into(),
            model: "gpt-5.6-terra".into(),
            context_window: Some(272_000),
            auto_compact_percent: Some(85),
            default_effort: "medium".into(),
            efforts: all6(&["low", "medium", "high", "xhigh", "max", "ultra"]),
            note: "balanced everyday coding".into(),
            vision: true,
            ..Default::default()
        },
        ModelEntry {
            alias: "luna".into(),
            model: "gpt-5.6-luna".into(),
            context_window: Some(272_000),
            auto_compact_percent: Some(85),
            default_effort: "medium".into(),
            efforts: all6(&["low", "medium", "high", "xhigh", "max"]),
            note: "fast & affordable workers".into(),
            vision: true,
            ..Default::default()
        },
    ]
}

impl Registry {
    pub fn defaults() -> Registry {
        let mut models = codex_default_models();
        // bija ships beside mantra, so its provider and the default models through it are always
        // offered — they are what Solo runs on (`Settings::default_model`).
        let bija = bija_default_provider();
        models.extend(bija_default_model_entries(&bija.id));
        let mut providers = vec![bija];
        // Only offered when `claude` is actually installed (WP10.2); `doctor` reports this check.
        if claude_available() {
            providers.push(ProviderEntry { id: "claude".into(), name: "Claude Code".into(), kind: ProviderKind::ClaudeCode, auth: "subscription".into(), ..Default::default() });
            models.extend(claude_default_model_entries("claude"));
        }
        Registry { models, providers }
    }

    /// Which backend a model's provider launches through (`openai`/no provider = Codex).
    pub fn backend_of(&self, m: &ModelEntry) -> ProviderKind {
        self.providers.iter().find(|p| p.id == m.provider).map(|p| p.kind).unwrap_or_default()
    }

    pub fn path() -> PathBuf {
        home().join("models.toml")
    }

    /// A registry written before bija shipped gets its provider and the `-api` twins of the default
    /// models — what `mantra-default-bija` and the run-without-Codex fallback are built on. Only a
    /// registry with no Bija provider at all is touched (one the user set up stays as it is), and
    /// only the aliases it lacks are added. True when anything was added.
    pub fn add_missing_bija(&mut self) -> bool {
        if self.providers.iter().any(|p| p.kind == ProviderKind::Bija) {
            return false;
        }
        // An id of the user's own under the default's name is theirs; the twins then go nowhere.
        if self.providers.iter().any(|p| p.id == bija_default_provider().id) {
            return false;
        }
        let provider = bija_default_provider();
        let twins: Vec<ModelEntry> = bija_default_model_entries(&provider.id).into_iter().filter(|m| self.get(&m.alias).is_none()).collect();
        self.providers.push(provider);
        self.models.extend(twins);
        true
    }

    /// A registry written before models had a `vision` key reads `false` everywhere, so nobody
    /// would get the screenshot tools until they found the column. When `text` (the file as read)
    /// has no `vision` key on any model, the entries whose model id belongs to a family that takes
    /// images (`takes_images`) are switched on; a file with any `vision` key — even one `false`
    /// written by hand — is the user's say and is left alone. True when anything changed.
    pub fn add_missing_vision(&mut self, text: &str) -> bool {
        let Ok(doc) = text.parse::<toml::Value>() else { return false };
        let has_key = doc.get("model").and_then(|m| m.as_array()).is_some_and(|models| models.iter().any(|m| m.get("vision").is_some()));
        if has_key {
            return false;
        }
        let mut changed = false;
        for m in self.models.iter_mut().filter(|m| !m.vision && takes_images(&m.model)) {
            m.vision = true;
            changed = true;
        }
        changed
    }

    pub fn load() -> Registry {
        match std::fs::read_to_string(Self::path()) {
            Ok(s) => match toml::from_str::<Registry>(&s) {
                Ok(mut r) if !r.models.is_empty() => {
                    let bija = r.add_missing_bija();
                    if bija {
                        crate::mlog!("models.toml: added the bija provider and the default models through it");
                    }
                    let vision = r.add_missing_vision(&s);
                    if vision {
                        crate::mlog!("models.toml: vision switched on for the models whose family takes images");
                    }
                    // One save for both: every entry now carries `vision`, so neither runs again.
                    if bija || vision {
                        let _ = r.save();
                    }
                    r
                }
                Ok(_) => Registry::defaults(),
                Err(e) => {
                    crate::mlog!("models.toml invalid ({e}); using defaults");
                    Registry::defaults()
                }
            },
            Err(_) => {
                let r = Registry::defaults();
                let _ = r.save();
                r
            }
        }
    }

    pub fn save(&self) -> Result<()> {
        std::fs::create_dir_all(home())?;
        let body = toml::to_string_pretty(self).context("serialize models")?;
        let text = format!("{REGISTRY_HEADER}{body}");
        // A registry holding a pasted-in key is written 0600 (Unix) so it isn't world-readable.
        if self.providers.iter().any(|p| p.api_key.as_ref().map(|k| !k.trim().is_empty()).unwrap_or(false)) {
            atomic_write_restricted(&Self::path(), &text)
        } else {
            atomic_write(&Self::path(), &text)
        }
    }

    /// Display name for a provider id, for pickers and fields that show "via <name>": the built-in
    /// `openai` (Codex's own account, no `[[provider]]` entry) is always "OpenAI (Codex)"; a
    /// configured provider shows its `name` (falling back to the id if that's blank); an unknown id
    /// falls back to itself.
    pub fn provider_name(&self, id: &str) -> String {
        if id.is_empty() || id == "openai" {
            return "OpenAI (Codex)".into();
        }
        self.providers.iter().find(|p| p.id == id).map(|p| if p.name.trim().is_empty() { p.id.clone() } else { p.name.clone() }).unwrap_or_else(|| id.to_string())
    }

    pub fn get(&self, alias: &str) -> Option<&ModelEntry> {
        self.models
            .iter()
            .find(|m| m.alias == alias)
            .or_else(|| self.models.iter().find(|m| m.model == alias))
    }

    /// Whether the alias's model takes images — what offers the screenshot and view-image tools,
    /// and `--vision` to bija. An unknown alias does not.
    pub fn vision(&self, alias: &str) -> bool {
        self.get(alias).map(|m| m.vision).unwrap_or(false)
    }

    /// Resolve an alias; unknown names are treated as raw model ids on the default provider.
    pub fn resolve(&self, alias: &str) -> ModelEntry {
        self.get(alias).cloned().unwrap_or_else(|| ModelEntry {
            alias: alias.to_string(),
            model: alias.to_string(),
            ..Default::default()
        })
    }

    /// Every alias with its effective context window (its own, or the assumed one) — what the
    /// engine, which has no registry of its own, is handed at a run's start, resume and every live
    /// model switch (`Run::set_known_models`): it tells a saved alias that still exists from one
    /// that was deleted, and gives the pattern's `context_cap` a window to be measured against.
    pub fn context_windows(&self) -> std::collections::HashMap<String, u64> {
        self.models.iter().map(|m| (m.alias.clone(), m.effective_context())).collect()
    }

    /// The custom provider an alias runs on; `None` for Codex's own account or an unknown alias.
    pub fn provider_of(&self, alias: &str) -> Option<&ProviderEntry> {
        let m = self.get(alias).filter(|m| m.is_custom_provider())?;
        self.providers.iter().find(|p| p.id == m.provider)
    }

    /// Why the `/models` `t` test must not send anything for an alias: everything
    /// `alias_problem` catches, plus a provider that is still a draft (`ProviderEntry::draft_reason`
    /// — placeholder URL, no credential). `None` = go.
    pub fn probe_problem(&self, alias: &str) -> Option<String> {
        let Some(p) = self.provider_of(alias) else {
            return self.alias_problem(alias); // unknown alias/provider, or Codex's own account (fine)
        };
        p.draft_reason().map(|why| format!("{alias} via {} — {why} (/models to fix)", self.provider_name(&p.id)))
    }

    /// Why an alias cannot run right now: unknown alias, unknown provider, a custom provider whose
    /// `base_url` is still the placeholder (`ProviderEntry::url_problem` — a run would send the
    /// key there) or with no usable key (neither the `env_key` variable nor a stored `api_key`).
    /// `None` = fine.
    pub fn alias_problem(&self, alias: &str) -> Option<String> {
        let Some(m) = self.get(alias) else {
            return Some(format!("model alias '{alias}' is not in /models"));
        };
        if !m.is_custom_provider() {
            return None; // Codex's own account: `codex login` is checked by doctor, not here
        }
        let Some(p) = self.providers.iter().find(|p| p.id == m.provider) else {
            return Some(format!("{alias} uses provider '{}' which is not in /models", m.provider));
        };
        if let Some(why) = p.url_problem().or_else(|| p.wire_problem()) {
            return Some(format!("{alias} via {} — {why} (/models to fix)", self.provider_name(&p.id)));
        }
        if !p.needs_key() {
            // A subscription login (`claude` holds the credentials) or a Bija provider, whose key
            // is optional: nothing to check here.
            return None;
        }
        if p.resolve_key().is_none() {
            let var = p.env_var_name();
            return Some(format!("{alias} via {} — ${var} is not set and no api_key is stored (/models to fix)", self.provider_name(&p.id)));
        }
        None
    }

    /// Preflight every role of a pattern before a run starts: one message per role whose model
    /// cannot run (L3 in v02plan.md — the orchestrator's provider used to fail only after the plan
    /// was approved). Empty = go.
    pub fn preflight(&self, pattern: &crate::engine::pattern::Pattern) -> Vec<String> {
        let mut out = vec![];
        for (name, role) in pattern.ordered_roles() {
            if let Some(why) = self.alias_problem(&role.model) {
                out.push(format!("{name}: {why}"));
            }
            // The fallback is only ever used when the primary has already failed twice, so a
            // typo in it must surface here, not at 3 a.m. in the middle of a run.
            if let Some(why) = role.fallback.as_deref().filter(|f| !f.trim().is_empty()).and_then(|f| self.alias_problem(f)) {
                out.push(format!("{name}: fallback {why}"));
            }
        }
        out
    }

    /// `-c key=value` arguments that register custom providers with a Codex process.
    pub fn provider_args(&self) -> Vec<String> {
        let mut args = vec![];
        for p in &self.providers {
            if p.id.is_empty() || p.id == "openai" || p.kind != ProviderKind::Codex {
                continue;
            }
            let q = |s: &str| format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""));
            let base = format!("model_providers.{}", p.id);
            args.push("-c".into());
            args.push(format!("{base}.name={}", q(if p.name.is_empty() { &p.id } else { &p.name })));
            if !p.base_url.is_empty() {
                args.push("-c".into());
                args.push(format!("{base}.base_url={}", q(&p.base_url)));
            }
            let has_key = !p.env_key.trim().is_empty() || p.api_key.as_ref().map(|k| !k.trim().is_empty()).unwrap_or(false);
            if has_key {
                args.push("-c".into());
                // Only the variable NAME is ever passed — the value goes into the child's
                // environment (see `spawn_agent`), never on argv, in logs, or here.
                args.push(format!("{base}.env_key={}", q(&p.env_var_name())));
            }
            args.push("-c".into());
            // Codex only speaks the Responses API to providers; any other value would be rejected.
            args.push(format!("{base}.wire_api={}", q("responses")));
        }
        args
    }
}

/// A built-in pattern with every `astra` role moved to `sol`: what a fresh install saves when Codex
/// is signed in through a ChatGPT subscription, which has no access to `astra` (it stays in
/// `models.toml` as a plain alias — nothing defaults to it any more). An `auto` effort stays `auto`.
pub fn chatgpt_variant(mut p: crate::engine::pattern::Pattern) -> crate::engine::pattern::Pattern {
    let sol = Registry::defaults().resolve("sol");
    for role in p.roles.values_mut() {
        if role.model == "astra" {
            role.model = "sol".into();
            if role.effort != crate::engine::pattern::EFFORT_AUTO {
                role.effort = sol.resolve_effort(&role.effort);
            }
        }
    }
    p.description = format!("{} — ChatGPT subscription models", p.description);
    p
}

/// Write via temp file + rename so a crash never leaves a half-written config.
pub fn atomic_write(path: &Path, body: &str) -> Result<()> {
    write_atomic(path, body, false)
}

/// Like `atomic_write`, but the file is 0600 (Unix) from the moment it exists — used when the
/// content holds a secret (an API key pasted into `models.toml`, the web layer's keys and tokens).
pub fn atomic_write_restricted(path: &Path, body: &str) -> Result<()> {
    write_atomic(path, body, true)
}

fn write_atomic(path: &Path, body: &str, restrict: bool) -> Result<()> {
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
    }
    let tmp = path.with_extension("tmp~");
    if restrict {
        write_restricted(&tmp, body)?;
    } else {
        std::fs::write(&tmp, body)?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// The secret must never sit on disk at the umask's mode, not even between write and chmod: the
/// file is created 0600, and a stale temp file left by a crash is narrowed (fchmod) before any
/// byte of the new content lands in it.
fn write_restricted(tmp: &Path, body: &str) -> Result<()> {
    use std::io::Write;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
    let mut f = opts.open(tmp)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    f.write_all(body.as_bytes())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    #[test]
    fn restricted_writes_never_exist_world_readable() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("mantra-restricted-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("secret.json");
        // a stale, world-readable temp file from a crash is narrowed before the secret goes in
        let tmp = path.with_extension("tmp~");
        std::fs::write(&tmp, "old").unwrap();
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o644)).unwrap();
        atomic_write_restricted(&path, "{\"k\":\"s3cret\"}").unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{\"k\":\"s3cret\"}");
        assert!(!tmp.exists());
        // the temp file itself is created 0600 (checked directly, no rename in between)
        write_restricted(&tmp, "x").unwrap();
        assert_eq!(std::fs::metadata(&tmp).unwrap().permissions().mode() & 0o777, 0o600);
        let _ = std::fs::remove_dir_all(dir);
    }
    #[test]
    fn effort_resolution() {
        let r = Registry::defaults();
        let luna = r.get("luna").unwrap();
        assert_eq!(luna.resolve_effort("max"), "max");
        assert_eq!(luna.resolve_effort("ultra"), "max");
        let m = ModelEntry { efforts: vec!["low".into(), "medium".into(), "high".into()], ..Default::default() };
        assert_eq!(m.resolve_effort("max"), "high");
        assert_eq!(m.resolve_effort("minimal"), "low");
        assert_eq!(m.step_effort("low", 1), "medium");
        assert_eq!(m.step_effort("high", 1), "high");
    }
    #[test]
    fn effort_resolution_reports_a_lowered_request() {
        let m = ModelEntry { efforts: vec!["low".into(), "medium".into(), "high".into()], ..Default::default() };
        assert_eq!(m.resolve_effort_lowered("max"), ("high".into(), true));
        assert_eq!(m.resolve_effort_lowered("xhigh"), ("high".into(), true));
        assert_eq!(m.resolve_effort_lowered("high"), ("high".into(), false));
        assert_eq!(m.resolve_effort_lowered("min"), ("low".into(), false), "aliases aren't a downgrade");
        let luna = Registry::defaults().get("luna").unwrap().clone();
        assert_eq!(luna.resolve_effort_lowered("max").1, false);
        let none = ModelEntry { provider: "zai".into(), ..Default::default() };
        assert_eq!(none.resolve_effort_lowered("max"), (String::new(), false), "no effort setting at all isn't a clamp");
    }
    #[test]
    fn custom_models_without_efforts_send_none() {
        let m = ModelEntry { provider: "zai".into(), ..Default::default() };
        assert!(m.efforts().is_empty());
        assert_eq!(m.resolve_effort("max"), "");
        assert_eq!(m.step_effort("", 1), "");
        let o = ModelEntry::default();
        assert!(!o.efforts().is_empty(), "OpenAI entries keep the usual set");
    }
    #[test]
    fn runs_live_in_home_not_project() {
        // under the lock the tests that move `$MANTRA_HOME` take, so `home()` holds still
        crate::web::snapshot::tests::with_mantra_home(|| {
            let d = runs_dir(Path::new("/tmp/some project"));
            assert!(d.starts_with(home().join("runs")));
            assert!(d.file_name().unwrap().to_string_lossy().starts_with("some-project-"));
            assert_ne!(runs_dir(Path::new("/a/app")), runs_dir(Path::new("/b/app")));
        });
    }
    #[test]
    fn effective_context_and_compact_percent() {
        let assumed = ModelEntry::default();
        assert_eq!(assumed.effective_context(), crate::discover::ASSUMED_CONTEXT);
        assert_eq!(assumed.effective_compact_percent(), 85);
        let explicit = ModelEntry { context_window: Some(16_000), auto_compact_percent: Some(70), ..Default::default() };
        assert_eq!(explicit.effective_context(), 16_000);
        assert_eq!(explicit.effective_compact_percent(), 70);
        // shipped built-ins are honest: explicit, not assumed
        let r = Registry::defaults();
        let sol = r.get("sol").unwrap();
        assert_eq!(sol.effective_context(), 272_000);
        assert_eq!(sol.effective_compact_percent(), 85);
    }
    /// A temp `$CODEX_HOME` for a test, restored afterwards. Serialized against other tests here
    /// that do the same (the env var is process-wide).
    fn with_codex_home<R>(f: impl FnOnce(&Path) -> R) -> R {
        use std::sync::Mutex;
        static LOCK: Mutex<()> = Mutex::new(());
        let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("mantra-test-codex-home-{}-{}", std::process::id(), crate::web::snapshot::now_ms()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let prev = std::env::var("CODEX_HOME").ok();
        std::env::set_var("CODEX_HOME", &dir);
        let r = f(&dir);
        match prev {
            Some(v) => std::env::set_var("CODEX_HOME", v),
            None => std::env::remove_var("CODEX_HOME"),
        }
        let _ = std::fs::remove_dir_all(&dir);
        r
    }
    #[test]
    fn codex_user_mcp_servers_read_from_config_toml() {
        let names = with_codex_home(|dir| {
            std::fs::write(dir.join("config.toml"), "[mcp_servers.node_repl]\ncommand = \"node\"\n").unwrap();
            codex_user_mcp_server_names()
        });
        assert_eq!(names, vec!["node_repl".to_string()]);
    }
    #[test]
    fn codex_user_mcp_servers_none_when_config_missing() {
        let names = with_codex_home(|_| codex_user_mcp_server_names());
        assert!(names.is_empty());
    }
    #[test]
    fn provider_display_names() {
        let mut r = Registry::defaults();
        assert_eq!(r.provider_name("openai"), "OpenAI (Codex)");
        assert_eq!(r.provider_name(""), "OpenAI (Codex)");
        r.providers.push(ProviderEntry { id: "zai".into(), name: "Z.ai".into(), ..Default::default() });
        assert_eq!(r.provider_name("zai"), "Z.ai");
        r.providers.push(ProviderEntry { id: "nameless".into(), ..Default::default() });
        assert_eq!(r.provider_name("nameless"), "nameless");
        assert_eq!(r.provider_name("unconfigured"), "unconfigured");
    }
    #[test]
    fn preflight_names_missing_keys_per_role() {
        let mut r = Registry::defaults();
        assert!(r.preflight(&crate::engine::pattern::Pattern::builtin()).is_empty(), "built-in models run on Codex's account");
        r.providers.push(ProviderEntry { id: "zai".into(), name: "Z.ai".into(), base_url: "https://api.z.ai/v1".into(), env_key: "MANTRA_TEST_NO_SUCH_VAR".into(), ..Default::default() });
        r.models.push(ModelEntry { alias: "glm".into(), provider: "zai".into(), model: "glm-5.2".into(), ..Default::default() });
        let mut p = crate::engine::pattern::Pattern::builtin_thorough();
        let sec = p.roles.keys().find(|k| k.contains("security")).cloned().expect("built-in has a security role");
        p.roles.get_mut(&sec).unwrap().model = "glm".into();
        let msgs = r.preflight(&p);
        assert_eq!(msgs.len(), 1, "{msgs:?}");
        assert!(msgs[0].starts_with(&format!("{sec}: glm via Z.ai")) && msgs[0].contains("MANTRA_TEST_NO_SUCH_VAR"), "{msgs:?}");
        assert!(msgs[0].contains("/models"));
        // a stored key satisfies it (find zai by id: defaults() may also carry a `claude` provider)
        r.providers.iter_mut().find(|p| p.id == "zai").unwrap().api_key = Some("k".into());
        assert!(r.preflight(&p).is_empty());
        assert!(r.alias_problem("nope").unwrap().contains("not in /models"));
        // the `n` placeholder row with a key set: a run would send that key to example.com
        r.providers.push(ProviderEntry { id: "myprovider".into(), base_url: "https://example.com/v1".into(), api_key: Some("k".into()), ..Default::default() });
        r.models.push(ModelEntry { alias: "draft".into(), provider: "myprovider".into(), model: "x".into(), ..Default::default() });
        p.roles.get_mut(&sec).unwrap().model = "draft".into();
        let msgs = r.preflight(&p);
        assert_eq!(msgs.len(), 1, "{msgs:?}");
        assert!(msgs[0].contains("example.com placeholder") && msgs[0].contains("/models"), "{msgs:?}");
        // the fallback model is checked like the model itself
        let mut f = crate::engine::pattern::Pattern::builtin_thorough();
        f.roles.get_mut(&sec).unwrap().fallback = Some("sol".into());
        assert!(r.preflight(&f).is_empty(), "a known fallback passes");
        f.roles.get_mut(&sec).unwrap().fallback = Some("no-such-alias".into());
        let msgs = r.preflight(&f);
        assert_eq!(msgs.len(), 1, "{msgs:?}");
        assert!(msgs[0].starts_with(&format!("{sec}: fallback model alias 'no-such-alias' is not in /models")), "{msgs:?}");
        // plain http is not a placeholder: a LAN box runs
        r.providers.iter_mut().find(|p| p.id == "myprovider").unwrap().base_url = "http://192.168.1.20:8000/v1".into();
        assert!(r.preflight(&p).is_empty(), "{:?}", r.preflight(&p));
    }
    #[test]
    fn provider_key_resolution_and_no_leak_on_argv() {
        // no env_key, no api_key → synthesized name, no key
        let p = ProviderEntry { id: "zai".into(), ..Default::default() };
        assert_eq!(p.env_var_name(), "MANTRA_ZAI_API_KEY");
        assert_eq!(p.resolve_key(), None);
        // api_key set directly, no env_key → synthesized name carries the value
        let p2 = ProviderEntry { id: "zai".into(), api_key: Some("sk-secret-123".into()), ..Default::default() };
        assert_eq!(p2.resolve_key().as_deref(), Some("sk-secret-123"));
        let mut r = Registry { models: vec![], providers: vec![p2] };
        r.models.push(ModelEntry { alias: "glm".into(), provider: "zai".into(), model: "glm-5.2".into(), ..Default::default() });
        let args = r.provider_args();
        assert!(args.iter().any(|a| a.contains("MANTRA_ZAI_API_KEY")), "the env var name must be registered: {args:?}");
        assert!(!args.iter().any(|a| a.contains("sk-secret-123")), "the raw key must never appear in generated args: {args:?}");
    }
    /// The `t` test uses strictly the tested model's own provider entry — its key, its host — and
    /// refuses to run while that provider is a draft, rather than borrowing another key.
    #[test]
    fn probe_uses_only_the_models_own_provider() {
        let mut r = Registry { models: vec![], providers: vec![] };
        r.providers.push(ProviderEntry { id: "riti".into(), name: "Riti".into(), base_url: "https://api.riti.dev/v1".into(), api_key: Some("sk-riti".into()), ..Default::default() });
        r.providers.push(ProviderEntry { id: "myprovider".into(), base_url: "https://example.com/v1".into(), env_key: "MANTRA_TEST_NO_SUCH_VAR".into(), ..Default::default() });
        r.models.push(ModelEntry { alias: "testing".into(), provider: "riti".into(), model: "testing".into(), ..Default::default() });
        r.models.push(ModelEntry { alias: "draft-model".into(), provider: "myprovider".into(), model: "x".into(), ..Default::default() });
        r.models.push(ModelEntry { alias: "gpt".into(), model: "gpt-5".into(), ..Default::default() });
        assert_eq!(r.probe_problem("testing"), None);
        let p = r.provider_of("testing").expect("riti");
        assert_eq!((p.id.as_str(), p.resolve_key().as_deref(), p.host().as_deref()), ("riti", Some("sk-riti"), Some("api.riti.dev")));
        // the draft's model is refused with the URL reason — riti's key is never a fallback
        let why = r.probe_problem("draft-model").expect("draft is refused");
        assert!(why.contains("myprovider") && why.contains("placeholder") && !why.contains("riti"), "{why}");
        assert_eq!(r.provider_of("draft-model").and_then(|p| p.resolve_key()), None);
        // Codex's own account has no custom provider and nothing to check here
        assert_eq!(r.provider_of("gpt"), None);
        assert_eq!(r.probe_problem("gpt"), None);
        assert!(r.probe_problem("nope").unwrap().contains("not in /models"));
    }
    /// P2 (test report): a freshly added provider must never send a request to its placeholder —
    /// it stays a draft until the URL is real and a credential is configured.
    #[test]
    fn draft_until_url_and_credential_are_real() {
        let key = Some("sk-1".to_string());
        // the `n` row exactly as studio.rs creates it: placeholder URL, env var that isn't set
        let p = ProviderEntry { id: "myprovider".into(), base_url: "https://example.com/v1".into(), env_key: "MANTRA_TEST_NO_SUCH_VAR".into(), ..Default::default() };
        assert!(p.draft_reason().unwrap().contains("example.com placeholder"), "{:?}", p.draft_reason());
        assert_eq!(p.host().as_deref(), Some("example.com"), "the destination is still shown for a draft");
        // a real URL, but no credential yet → still a draft, with the key as the reason
        let p = ProviderEntry { base_url: "https://api.riti.dev/v1".into(), ..p };
        let why = p.draft_reason().unwrap();
        assert!(why.starts_with("no key") && why.contains("MANTRA_TEST_NO_SUCH_VAR"), "{why}");
        // a stored key completes it
        let p = ProviderEntry { api_key: key.clone(), ..p };
        assert_eq!(p.draft_reason(), None);
        assert_eq!(p.host().as_deref(), Some("api.riti.dev"));
        // URL shapes
        let with = |url: &str| ProviderEntry { id: "x".into(), base_url: url.into(), api_key: key.clone(), ..Default::default() };
        assert_eq!(with("http://localhost:8000/v1").draft_reason(), None, "plain http to loopback is fine");
        assert_eq!(with("http://127.0.0.1:11434").host().as_deref(), Some("127.0.0.1:11434"));
        assert_eq!(with("http://192.168.1.20:8000/v1").draft_reason(), None, "a LAN inference box (vLLM, Ollama) is plain http");
        assert_eq!(with("http://192.168.1.20:8000/v1").host().as_deref(), Some("192.168.1.20:8000"));
        assert_eq!(with("http://api.riti.dev/v1").draft_reason(), None, "plain http is the user's call, not a draft");
        assert!(with("https://api.example.org").draft_reason().unwrap().contains("placeholder"));
        assert!(with("https://gateway.invalid/v1").draft_reason().unwrap().contains("placeholder"));
        assert!(with("").draft_reason().unwrap().contains("empty"), "Codex providers must name a URL — Codex would otherwise default to api.openai.com");
        assert!(with("api.riti.dev/v1").draft_reason().unwrap().contains("not a URL"));
        assert!(with("ftp://api.riti.dev").draft_reason().unwrap().contains("not a URL"));
        assert!(with("https://user:pw@api.riti.dev").draft_reason().unwrap().contains("not a URL"), "credentials in the URL are refused");
        assert!(with("https://api riti.dev").draft_reason().unwrap().contains("not a URL"));
        assert!(with("https://api.riti.dev:").draft_reason().unwrap().contains("not a URL"), "an empty port is not part of the host");
        assert!(with("https://api.riti.dev:port").draft_reason().unwrap().contains("not a URL"));
        assert_eq!(with("HTTPS://Api.Riti.dev:8443/v1/").host().as_deref(), Some("Api.Riti.dev:8443"));
        assert_eq!(with("http://[::1]:8080/v1").draft_reason(), None);
        // ClaudeCode: a subscription needs neither URL nor key; a gateway URL is still checked
        let cc = ProviderEntry { id: "claude2".into(), kind: ProviderKind::ClaudeCode, ..Default::default() };
        assert_eq!(cc.draft_reason(), None);
        assert_eq!(cc.host(), None);
        let cc = ProviderEntry { base_url: "https://example.com".into(), ..cc };
        assert!(cc.draft_reason().unwrap().contains("placeholder"));
        let cc = ProviderEntry { base_url: String::new(), auth: "api_key".into(), ..cc };
        assert!(cc.draft_reason().unwrap().starts_with("no key"));
    }
    #[test]
    fn registry_roundtrip() {
        let r = Registry::defaults();
        let s = toml::to_string_pretty(&r).unwrap();
        let back: Registry = toml::from_str(&s).unwrap();
        assert_eq!(back.models.len(), r.models.len());
    }
    #[test]
    fn backend_of_is_codex_unless_provider_says_claude_code() {
        let mut r = Registry { models: vec![], providers: vec![] };
        let openai_model = ModelEntry { provider: "openai".into(), ..Default::default() };
        assert_eq!(r.backend_of(&openai_model), ProviderKind::Codex);
        let unknown_model = ModelEntry { provider: "nope".into(), ..Default::default() };
        assert_eq!(r.backend_of(&unknown_model), ProviderKind::Codex, "an unresolved provider id must never be treated as Claude Code");
        r.providers.push(ProviderEntry { id: "claude".into(), kind: ProviderKind::ClaudeCode, ..Default::default() });
        let claude_model = ModelEntry { provider: "claude".into(), ..Default::default() };
        assert_eq!(r.backend_of(&claude_model), ProviderKind::ClaudeCode);
    }
    #[test]
    fn claude_defaults_only_appear_when_claude_kind_provider_exists() {
        // Registry::defaults() only adds the built-in `claude` provider when the `claude` binary is
        // on PATH (config::claude_available()); assert the two are consistent either way, and that
        // when present the models/provider shape matches WP10.2 (kind, model ids, 1M windows).
        let r = Registry::defaults();
        let has_claude_provider = r.providers.iter().any(|p| p.kind == ProviderKind::ClaudeCode);
        assert_eq!(has_claude_provider, claude_available());
        if has_claude_provider {
            let p = r.providers.iter().find(|p| p.id == "claude").expect("built-in provider id must be `claude`");
            assert_eq!(p.auth, "subscription");
            let sonnet = r.get("sonnet5").expect("sonnet5 alias");
            assert_eq!(sonnet.model, "claude-sonnet-5");
            assert_eq!(r.backend_of(sonnet), ProviderKind::ClaudeCode);
            assert_eq!(sonnet.effective_context(), 1_000_000, "Opus/Sonnet/Fable default to the 1M window");
            let haiku = r.get("haiku45").expect("haiku45 alias");
            assert_eq!(haiku.effective_context(), 200_000, "Haiku keeps 200k");
        }
    }
    #[test]
    fn provider_args_never_mention_claude_code_providers() {
        let r = Registry { models: vec![], providers: vec![ProviderEntry { id: "claude".into(), name: "Claude Code".into(), kind: ProviderKind::ClaudeCode, ..Default::default() }] };
        assert!(r.provider_args().is_empty(), "ClaudeCode providers must not generate Codex model_providers.* args");
    }
    #[test]
    fn chatgpt_login_line_classifier() {
        assert!(is_chatgpt_login("Logged in using ChatGPT"));
        assert!(!is_chatgpt_login("Logged in using an API key"));
        assert!(!is_chatgpt_login("Not logged in"));
    }
    #[test]
    fn chatgpt_default_pattern_moves_astra_roles_to_sol() {
        let p = chatgpt_variant(crate::engine::pattern::Pattern::builtin());
        assert_eq!(p.name, "mantra-default");
        assert!(p.description.ends_with(" — ChatGPT subscription models"), "{}", p.description);
        let reg = Registry::defaults();
        for (n, r) in &p.roles {
            assert_ne!(r.model, "astra", "role '{n}' still defaults to astra");
            assert!(!reg.resolve(&r.model).is_custom_provider(), "role '{n}' must resolve to a Codex-account model");
        }
        assert!(p.validate().is_ok(), "{:?}", p.validate());
    }
    #[test]
    fn chatgpt_variants_cover_every_built_in_and_keep_auto_efforts() {
        use crate::engine::pattern::{Pattern, EFFORT_AUTO};
        let reg = Registry::defaults();
        for n in ["mantra-default", "mantra-lean", "mantra-thorough"] {
            let src = Pattern::builtin_named(n).unwrap();
            let v = chatgpt_variant(src.clone());
            assert_eq!(v.name, n);
            assert!(v.roles.values().all(|r| r.model != "astra"), "{n}");
            assert!(v.validate().is_ok(), "{n}: {:?}", v.validate());
            for (rn, r) in &v.roles {
                if src.roles[rn].effort == EFFORT_AUTO {
                    assert_eq!(r.effort, EFFORT_AUTO, "{n}/{rn}: an auto effort must not be baked into a level");
                }
                assert!(!reg.resolve(&r.model).is_custom_provider());
            }
        }
    }
    #[test]
    fn auto_effort_resolves_like_medium_when_it_reaches_the_registry() {
        let sol = Registry::defaults().resolve("sol");
        assert_eq!(sol.resolve_effort("auto"), "medium");
        assert_eq!(sol.resolve_effort_lowered("auto"), ("medium".into(), false));
        let m = ModelEntry { efforts: vec!["low".into(), "high".into()], ..Default::default() };
        assert_eq!(m.resolve_effort("AUTO"), "low", "nearest to medium on a model without it");
        let none = ModelEntry { provider: "gw".into(), ..Default::default() };
        assert_eq!(none.resolve_effort("auto"), "", "a model without effort control stays without");
    }
    /// A Bija provider needs a real URL and a wire it knows, never a key: a local server takes
    /// none, and a hosted API answers HTTP 401 itself.
    #[test]
    fn a_bija_provider_needs_a_url_and_a_wire_but_no_key() {
        let p = ProviderEntry { id: "ollama".into(), kind: ProviderKind::Bija, base_url: "http://127.0.0.1:11434/v1".into(), ..Default::default() };
        assert!(!p.needs_key() && p.needs_url());
        assert_eq!(p.draft_reason(), None, "a missing key does not make a bija provider a draft");
        assert_eq!(p.bija_wire(), "openai-chat");
        let messages = ProviderEntry { wire: "Anthropic".into(), ..p.clone() };
        assert_eq!((messages.bija_wire(), messages.draft_reason()), ("anthropic", None));
        let typo = ProviderEntry { wire: "anthropc".into(), ..p.clone() };
        assert!(typo.draft_reason().is_some_and(|w| w.contains("anthropc")), "{:?}", typo.draft_reason());
        assert!(ProviderEntry { base_url: "https://example.com/v1".into(), ..p.clone() }.draft_reason().is_some_and(|w| w.contains("placeholder")));
        assert!(ProviderEntry { base_url: String::new(), ..p.clone() }.draft_reason().is_some_and(|w| w.contains("empty")));
        let codex = ProviderEntry { kind: ProviderKind::Codex, wire: "x".into(), ..p.clone() };
        assert_eq!(codex.wire_problem(), None, "Codex has no wire to choose, so none can be wrong");
        // an alias through it runs without any key check — unless the provider itself is wrong
        let mut r = Registry { models: vec![ModelEntry { alias: "local".into(), provider: "ollama".into(), model: "local-model".into(), ..Default::default() }], providers: vec![p] };
        assert_eq!(r.alias_problem("local"), None);
        assert_eq!(r.probe_problem("local"), None);
        assert!(r.provider_args().is_empty(), "a Bija provider is never a Codex model_providers entry");
        r.providers[0].wire = "grpc".into();
        assert!(r.alias_problem("local").is_some_and(|w| w.contains("wire 'grpc'") && w.contains("/models")), "{:?}", r.alias_problem("local"));
    }
    #[test]
    fn defaults_ship_bija_and_the_default_models_through_it() {
        let r = Registry::defaults();
        let p = r.providers.iter().find(|p| p.id == "bija").expect("the bija provider always ships");
        assert_eq!((p.kind, p.name.as_str(), p.base_url.as_str(), p.env_key.as_str(), p.wire.as_str()), (ProviderKind::Bija, "Bija", "https://api.openai.com/v1", "OPENAI_API_KEY", ""));
        for alias in ["astra", "sol", "terra", "luna"] {
            let codex = r.get(alias).expect("the Codex alias stays");
            let api = r.get(&bija_alias(alias)).expect("its -api twin ships");
            assert_eq!(api.alias, format!("{alias}-api"));
            assert_eq!((&api.model, api.context_window, &api.efforts, &api.default_effort), (&codex.model, codex.context_window, &codex.efforts, &codex.default_effort));
            assert_eq!(api.note, format!("{} — with an API key", codex.note));
            assert_eq!(r.backend_of(api), ProviderKind::Bija);
            assert_eq!(r.backend_of(codex), ProviderKind::Codex);
        }
        let s = Settings::default();
        assert_eq!(s.default_model, "sol-api", "Solo runs on bija");
        assert_eq!(r.backend_of(&r.resolve(&s.default_model)), ProviderKind::Bija);
        assert_eq!(s.bija_command, vec!["bija".to_string()]);
        assert!(r.provider_args().is_empty(), "nothing in the defaults is a Codex custom provider");
    }
    /// A models.toml from before bija gains the provider and the `-api` twins once; one that has a
    /// Bija provider (or a provider of its own under the name) is left as it is.
    #[test]
    fn an_old_registry_gains_bija_once_and_a_set_up_one_is_left_alone() {
        let mut old = Registry { models: codex_default_models(), providers: vec![] };
        old.models.push(ModelEntry { alias: "luna-api".into(), model: "my-own-luna".into(), ..Default::default() });
        assert!(old.add_missing_bija());
        assert_eq!(old.providers.iter().filter(|p| p.kind == ProviderKind::Bija).count(), 1);
        for alias in ["astra-api", "sol-api", "terra-api"] {
            assert_eq!(old.backend_of(&old.resolve(alias)), ProviderKind::Bija, "{alias}");
        }
        assert_eq!(old.get("luna-api").map(|m| m.model.as_str()), Some("my-own-luna"), "an alias of the user's own is kept");
        let problems = old.preflight(&crate::engine::pattern::Pattern::builtin_bija());
        assert!(problems.iter().all(|w| !w.contains("not in /models")), "every role's alias exists now: {problems:?}");
        let n = old.models.len();
        assert!(!old.add_missing_bija(), "once");
        assert_eq!(old.models.len(), n, "nothing doubled");
        let mut set_up = Registry::defaults();
        set_up.models.retain(|m| !m.alias.ends_with("-api"));
        assert!(!set_up.add_missing_bija(), "a Bija provider means the user decides its models");
        let mut taken = Registry { models: codex_default_models(), providers: vec![ProviderEntry { id: "bija".into(), base_url: "http://mine".into(), ..Default::default() }] };
        assert!(!taken.add_missing_bija());
        assert_eq!(taken.providers.len(), 1, "no second provider under the same id");
    }

    /// A models.toml from before the `vision` key gains it once, on the entries whose family takes
    /// images (the default tables, or an id shaped like them) and only those, in the one save that
    /// also adds bija; a file with any `vision` key — `false` by hand, or every one switched off in
    /// `/models` — is left as it says.
    #[test]
    fn an_old_registry_gains_vision_once_and_a_said_no_stays_no() {
        crate::web::snapshot::tests::with_mantra_home(|| {
            let model = |alias: &str, provider: &str, id: &str| format!("[[model]]\nalias = \"{alias}\"\nprovider = \"{provider}\"\nmodel = \"{id}\"\n\n");
            let old = [model("astra", "openai", "gpt-6-astra"), model("opus5", "claude", "claude-opus-5"), model("next", "openai", "gpt-7-nova"), model("local", "ollama", "qwen3-coder"), model("oss", "ollama", "gpt-oss-20b")].concat();
            let old = format!("{old}[[provider]]\nid = \"claude\"\nkind = \"claude-code\"\n");
            std::fs::write(Registry::path(), &old).unwrap();
            let r = Registry::load();
            let on: Vec<&str> = ["astra", "opus5", "next", "local", "oss"].into_iter().filter(|a| r.vision(a)).collect();
            assert_eq!(on, vec!["astra", "opus5", "next"]);
            assert!(r.vision("sol-api"), "the bija twins added in the same load take images too");
            let saved = std::fs::read_to_string(Registry::path()).unwrap();
            assert_eq!(saved.matches("vision = true").count(), r.models.iter().filter(|m| m.vision).count(), "{saved}");
            assert!(saved.contains("vision = false"), "a no is written too, so the next load knows: {saved}");
            // saved once: a second load finds the keys and writes nothing
            std::fs::write(Registry::path(), format!("{saved}# mine\n")).unwrap();
            let again = Registry::load();
            assert!(std::fs::read_to_string(Registry::path()).unwrap().ends_with("# mine\n"), "not saved a second time");
            assert_eq!(again.models.iter().filter(|m| m.vision).count(), r.models.iter().filter(|m| m.vision).count());
            // every one switched off in /models stays off
            let mut off = again;
            off.models.iter_mut().for_each(|m| m.vision = false);
            off.save().unwrap();
            assert!(Registry::load().models.iter().all(|m| !m.vision), "an all-off registry is not an old one");
            // a hand-written `vision = false` on one entry: the user has said; nothing is switched on
            let by_hand = old.replacen("model = \"gpt-6-astra\"\n", "model = \"gpt-6-astra\"\nvision = false\n", 1);
            std::fs::write(Registry::path(), format!("{by_hand}[[provider]]\nid = \"bija\"\nkind = \"bija\"\n")).unwrap();
            let hand = Registry::load();
            assert!(hand.models.iter().all(|m| !m.vision), "{:?}", hand.models.iter().map(|m| (&m.alias, m.vision)).collect::<Vec<_>>());
            assert!(std::fs::read_to_string(Registry::path()).unwrap().contains("[[provider]]\nid = \"bija\"\nkind = \"bija\"\n"), "left as written, not re-saved");
        });
    }

    #[test]
    fn bija_kind_and_wire_round_trip_through_models_toml() {
        let r: Registry = toml::from_str("[[provider]]\nid = \"a\"\nkind = \"bija\"\nwire = \"anthropic\"\n[[provider]]\nid = \"b\"\nkind = \"seed\"\n[[provider]]\nid = \"c\"\n").unwrap();
        assert_eq!(r.providers.iter().map(|p| p.kind).collect::<Vec<_>>(), vec![ProviderKind::Bija, ProviderKind::Bija, ProviderKind::Codex]);
        let out = toml::to_string_pretty(&r).unwrap();
        assert!(out.contains("kind = \"bija\"") && out.contains("wire = \"anthropic\""), "{out}");
        assert_eq!(out.matches("wire =").count(), 1, "an empty wire is never written: {out}");
    }
    #[test]
    fn the_bija_command_resolves_path_then_beside_mantra_then_the_env() {
        let dir = std::env::temp_dir().join(format!("mantra-bija-resolve-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let beside = dir.join("bija");
        let cmd = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let nowhere = |_: &str| false;
        // a configured path, or a name on PATH, is used as it is
        assert_eq!(resolve_bija_from(&cmd(&["/opt/bija", "--x"]), nowhere, None, None), (cmd(&["/opt/bija", "--x"]), BijaFound::Configured));
        assert_eq!(resolve_bija_from(&cmd(&["bija"]), |n| n == "bija", Some(&beside), Some("/env/bija")).1, BijaFound::Configured);
        // not on PATH: the bija beside mantra — only once it is really there — then the env;
        // the configured arguments come along either way
        assert_eq!(resolve_bija_from(&cmd(&["bija", "--x"]), nowhere, Some(&beside), Some("/env/bija")), (cmd(&["/env/bija", "--x"]), BijaFound::Env));
        std::fs::write(&beside, "#!/bin/sh\n").unwrap();
        assert_eq!(resolve_bija_from(&cmd(&["bija", "--x"]), nowhere, Some(&beside), Some("/env/bija")), (vec![beside.to_string_lossy().into_owned(), "--x".into()], BijaFound::BesideMantra));
        // nowhere at all: the configured command stays (its spawn error names it); empty = default
        assert_eq!(resolve_bija_from(&cmd(&["bija"]), nowhere, None, Some("  ")), (cmd(&["bija"]), BijaFound::Missing));
        assert_eq!(resolve_bija_from(&Vec::new(), nowhere, None, None).0, vec![bija_exe_name()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn server(command: &str, args: &[&str], enabled: bool) -> McpServer {
        McpServer { command: command.into(), args: args.iter().map(|a| a.to_string()).collect(), env: BTreeMap::new(), enabled }
    }

    /// `[mcp_servers.<name>]` survives a save and a load; an old settings.toml without the table
    /// comes back with none and Solo asking for every enabled one, and saving it adds nothing.
    #[test]
    fn mcp_servers_round_trip_and_old_settings_are_unchanged() {
        let mut s = Settings::default();
        let mut blender = server("uvx", &["blender-mcp"], true);
        blender.env.insert("BLENDER_PORT".into(), "9876".into());
        s.mcp_servers.insert("blender".into(), blender);
        s.mcp_servers.insert("files".into(), server("npx", &["-y", "@modelcontextprotocol/server-filesystem", "/tmp/a b"], false));
        s.solo_mcp = vec!["blender".into()];
        let text = toml::to_string_pretty(&s).unwrap();
        assert!(text.contains("[mcp_servers.blender]") && text.contains("enabled = false"), "{text}");
        let back: Settings = toml::from_str(&text).unwrap();
        assert_eq!(back.mcp_servers, s.mcp_servers);
        assert_eq!(back.solo_mcp, vec!["blender".to_string()]);
        // the documented form, with `enabled` and `env` left out
        let hand: Settings = toml::from_str("[mcp_servers.blender]\ncommand = \"uvx\"\nargs = [\"blender-mcp\"]\n").unwrap();
        assert_eq!(hand.mcp_servers["blender"], server("uvx", &["blender-mcp"], true));
        let old: Settings = toml::from_str("default_model = \"sol\"\n").unwrap();
        assert!(old.mcp_servers.is_empty());
        assert_eq!(old.solo_mcp, vec!["*".to_string()]);
        assert!(!toml::to_string_pretty(&old).unwrap().contains("mcp_servers"), "no empty table written");
    }

    /// `"*"` is every enabled server, names pick some; disabled ones never come back, the list is
    /// in name order whatever was asked, and only unconfigured names are missing.
    #[test]
    fn mcp_for_resolves_star_and_names_and_mcp_missing_names_the_rest() {
        let mut s = Settings::default();
        s.mcp_servers.insert("blender".into(), server("uvx", &["blender-mcp"], true));
        s.mcp_servers.insert("files".into(), server("npx", &["fs"], true));
        s.mcp_servers.insert("off".into(), server("x", &[], false));
        let names = |v: Vec<(String, McpServer)>| v.into_iter().map(|(n, _)| n).collect::<Vec<_>>();
        let w = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(names(s.mcp_for(&w(&["*"]))), vec!["blender", "files"]);
        assert_eq!(names(s.mcp_for(&w(&["files", "blender", "files"]))), vec!["blender", "files"]);
        assert_eq!(names(s.mcp_for(&w(&["off", "nope"]))), Vec::<String>::new());
        assert_eq!(names(s.mcp_for(&[])), Vec::<String>::new(), "empty = none");
        assert_eq!(names(s.mcp_for(&w(&[" blender "]))), vec!["blender"]);
        assert_eq!(s.mcp_for(&w(&["blender"]))[0].1.args, vec!["blender-mcp".to_string()]);
        assert_eq!(s.mcp_missing(&w(&["*", "blender", "nope", "off", "nope", ""])), vec!["nope".to_string()]);
        assert!(Settings::default().mcp_for(&w(&["*"])).is_empty(), "nothing configured, nothing given");
    }

    /// An MCP server's env (a token, typically) makes settings.toml a secret like a web password
    /// does: written 0600, whatever the umask.
    #[test]
    #[cfg(unix)]
    fn settings_with_an_mcp_env_are_written_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        crate::web::snapshot::tests::with_mantra_home(|| {
            let mode = || std::fs::metadata(home().join("settings.toml")).unwrap().permissions().mode() & 0o777;
            let mut s = Settings::default();
            s.mcp_servers.insert("blender".into(), server("uvx", &["blender-mcp"], true));
            assert!(!s.holds_secrets(), "a server without env is no secret");
            s.mcp_servers.get_mut("blender").unwrap().env.insert("TOKEN".into(), "s3cret".into());
            assert!(s.holds_secrets());
            s.save().unwrap();
            assert_eq!(mode(), 0o600);
            assert!(Settings { web: WebSettings { password: "pw".into(), ..Default::default() }, ..Settings::default() }.holds_secrets());
        });
    }

    /// A name typed into settings.toml by hand that `mantra mcp add` would refuse (a dot, `__`,
    /// `mantra`, a leading dash) is never handed to an agent — not by name, not through `*` — and
    /// `mcp_refused` says why, for the journal and the toast.
    #[test]
    fn hand_written_server_names_that_cannot_work_are_skipped_and_named() {
        let mut s = Settings::default();
        for name in ["blender", "blender.v2", "my__srv", "mantra", "--"] {
            s.mcp_servers.insert(name.into(), server("uvx", &["x"], true));
        }
        let w = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let names = |v: Vec<(String, McpServer)>| v.into_iter().map(|(n, _)| n).collect::<Vec<_>>();
        assert_eq!(names(s.mcp_for(&w(&["*"]))), vec!["blender"]);
        assert_eq!(names(s.mcp_for(&w(&["blender.v2", "my__srv", "mantra", "--"]))), Vec::<String>::new());
        let refused = s.mcp_refused(&w(&["*"]));
        assert_eq!(refused.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>(), vec!["--", "blender.v2", "mantra", "my__srv"]);
        assert!(refused.iter().all(|(n, why)| why.contains(n.as_str())), "{refused:?}");
        assert_eq!(s.mcp_refused(&w(&["blender", "my__srv"])).len(), 1, "only what was asked for");
        assert!(s.mcp_refused(&[]).is_empty());
        assert!(s.mcp_missing(&w(&["blender.v2"])).is_empty(), "configured, so not missing — refused instead");
    }

    #[test]
    fn mcp_server_names_must_work_as_toml_keys_and_tool_prefixes() {
        for ok in ["blender", "my-files", "fs_2"] {
            assert_eq!(mcp_name_problem(ok), None, "{ok}");
        }
        for (bad, why) in [("", "needs a name"), ("a/b", "letters"), ("a b", "letters"), ("a.b", "letters"), ("a__b", "double underscore"), ("mantra", "Mantra's own"), ("--", "start with -"), ("-x", "start with -")] {
            assert!(mcp_name_problem(bad).is_some_and(|e| e.contains(why)), "{bad:?}");
        }
    }

    #[test]
    fn vision_is_read_from_the_model_entry() {
        let mut r = Registry { models: vec![ModelEntry { alias: "eye".into(), model: "m-eye".into(), vision: true, ..Default::default() }, ModelEntry { alias: "plain".into(), ..Default::default() }], providers: vec![] };
        assert!(r.vision("eye") && r.vision("m-eye"));
        assert!(!r.vision("plain") && !r.vision("unknown"));
        r.models[0].vision = false;
        assert!(!r.vision("eye"));
    }
}
