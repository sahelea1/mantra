//! Settings and the model registry (models.toml).
//!
//! Everything lives in `$MANTRA_HOME` (default `~/.mantra` on Linux *and* macOS).
//! Nothing is ever written into your projects.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
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
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            codex_command: vec!["codex".into(), "app-server".into()],
            claude_command: vec!["claude".into()],
            default_model: "sol".into(),
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
    pub fn save(&self) -> Result<()> {
        std::fs::create_dir_all(home())?;
        let body = toml::to_string_pretty(self)?;
        atomic_write(&home().join("settings.toml"), &format!("# Mantra settings\n{body}"))
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
/// `ClaudeCode` providers are launched via `hub::claude` (`run_claude_process`) instead.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    #[default]
    Codex,
    ClaudeCode,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct ProviderEntry {
    pub id: String,
    pub name: String,
    pub base_url: String,
    /// Environment variable holding the API key (never store keys in files).
    pub env_key: String,
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
}

impl Default for ProviderEntry {
    fn default() -> Self {
        ProviderEntry { id: String::new(), name: String::new(), base_url: String::new(), env_key: String::new(), wire_api: String::new(), kind: ProviderKind::default(), auth: "subscription".into() }
    }
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
# context_window / auto_compact_percent are passed to Codex per agent.
# Custom providers must speak the OpenAI *Responses* API (Codex requirement).
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

/// The six default Claude Code models plus their `[1m]` long-context variants, alias -> model id,
/// context window and note. (`v02plan.md` WP10.2.)
const CLAUDE_MODELS: &[(&str, &str, u64, &str)] = &[
    ("opus46", "claude-opus-4-6", 200_000, ""),
    ("opus48", "claude-opus-4-8", 200_000, ""),
    ("opus5", "claude-opus-5", 200_000, ""),
    ("sonnet5", "claude-sonnet-5", 200_000, ""),
    ("fable5", "claude-fable-5", 200_000, ""),
    ("fable51", "claude-fable-5-1", 200_000, ""),
    ("opus5-1m", "claude-opus-5[1m]", 1_000_000, "1M context; needs an eligible plan"),
    ("sonnet5-1m", "claude-sonnet-5[1m]", 1_000_000, "1M context; needs an eligible plan"),
];

/// The six default Claude Code models plus their `[1m]` variants as full `ModelEntry`s for
/// `provider_id` (`v02plan.md` WP10.2). Used by `Registry::defaults()` (only when `claude` is on
/// PATH) and by `discover::claude_defaults` (WP10.5, any `ClaudeCode`-kind provider's `D` key), and
/// injected in memory by `--demo` (WP10.6) regardless of `claude_available()` so
/// `mantra --demo --pattern mantra-default-claude` needs no real `claude` install.
pub fn claude_default_model_entries(provider_id: &str) -> Vec<ModelEntry> {
    let claude_efforts: Vec<String> = ["low", "medium", "high", "xhigh", "max"].iter().map(|s| s.to_string()).collect();
    CLAUDE_MODELS
        .iter()
        .map(|(alias, model, context_window, note)| ModelEntry {
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

impl Registry {
    pub fn defaults() -> Registry {
        let all6 = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let mut models = vec![
                ModelEntry {
                    alias: "astra".into(),
                    model: "gpt-6-astra".into(),
                    context_window: Some(272_000),
                    auto_compact_percent: Some(85),
                    default_effort: "high".into(),
                    efforts: all6(&["low", "medium", "high", "xhigh", "max", "ultra"]),
                    note: "most capable — planning & hard problems".into(),
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
                    ..Default::default()
                },
            ];
        let mut providers = vec![];
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

    pub fn load() -> Registry {
        match std::fs::read_to_string(Self::path()) {
            Ok(s) => match toml::from_str::<Registry>(&s) {
                Ok(r) if !r.models.is_empty() => r,
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
        atomic_write(&Self::path(), &format!("{REGISTRY_HEADER}{body}"))
    }

    pub fn get(&self, alias: &str) -> Option<&ModelEntry> {
        self.models
            .iter()
            .find(|m| m.alias == alias)
            .or_else(|| self.models.iter().find(|m| m.model == alias))
    }

    /// Resolve an alias; unknown names are treated as raw model ids on the default provider.
    pub fn resolve(&self, alias: &str) -> ModelEntry {
        self.get(alias).cloned().unwrap_or_else(|| ModelEntry {
            alias: alias.to_string(),
            model: alias.to_string(),
            ..Default::default()
        })
    }

    /// `-c key=value` arguments that register custom providers with a Codex process.
    pub fn provider_args(&self) -> Vec<String> {
        let mut args = vec![];
        for p in &self.providers {
            if p.id.is_empty() || p.id == "openai" || p.kind == ProviderKind::ClaudeCode {
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
            if !p.env_key.is_empty() {
                args.push("-c".into());
                args.push(format!("{base}.env_key={}", q(&p.env_key)));
            }
            args.push("-c".into());
            // Codex only speaks the Responses API to providers; any other value would be rejected.
            args.push(format!("{base}.wire_api={}", q("responses")));
        }
        args
    }
}

/// Write via temp file + rename so a crash never leaves a half-written config.
pub fn atomic_write(path: &Path, body: &str) -> Result<()> {
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
    }
    let tmp = path.with_extension("tmp~");
    std::fs::write(&tmp, body)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
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
        let d = runs_dir(Path::new("/tmp/some project"));
        assert!(d.starts_with(home().join("runs")));
        assert!(d.file_name().unwrap().to_string_lossy().starts_with("some-project-"));
        assert_ne!(runs_dir(Path::new("/a/app")), runs_dir(Path::new("/b/app")));
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
        // when present the models/provider shape matches WP10.2 (kind, model ids, [1m] variants).
        let r = Registry::defaults();
        let has_claude_provider = r.providers.iter().any(|p| p.kind == ProviderKind::ClaudeCode);
        assert_eq!(has_claude_provider, claude_available());
        if has_claude_provider {
            let p = r.providers.iter().find(|p| p.id == "claude").expect("built-in provider id must be `claude`");
            assert_eq!(p.auth, "subscription");
            let sonnet = r.get("sonnet5").expect("sonnet5 alias");
            assert_eq!(sonnet.model, "claude-sonnet-5");
            assert_eq!(r.backend_of(sonnet), ProviderKind::ClaudeCode);
            let sonnet_1m = r.get("sonnet5-1m").expect("sonnet5-1m alias");
            assert_eq!(sonnet_1m.model, "claude-sonnet-5[1m]");
            assert_eq!(sonnet_1m.effective_context(), 1_000_000);
            assert!(sonnet_1m.note.contains("1M context"));
        }
    }
    #[test]
    fn provider_args_never_mention_claude_code_providers() {
        let r = Registry { models: vec![], providers: vec![ProviderEntry { id: "claude".into(), name: "Claude Code".into(), kind: ProviderKind::ClaudeCode, ..Default::default() }] };
        assert!(r.provider_args().is_empty(), "ClaudeCode providers must not generate Codex model_providers.* args");
    }
}
