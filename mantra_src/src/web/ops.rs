//! The web's configuration commands (0.8, additive to protocol 1): settings and MCP servers, the
//! model registry and its providers, patterns, stopping a run — through the APIs the TUI's
//! `/settings`, `/models` and `/studio` use (`settings_ops`, `provider_key`, `Pattern`), so the
//! browser keeps no rules of its own and shows the error strings it gets back.
//!
//! What a reply never carries: a provider's key (`api_key` is `••••` and its last four, `env_key`
//! a variable's name), the web password ("set" / "not set"), an MCP server's `env` values.
//!
//! Three commands wait on something slow and are answered later, never blocking the App task:
//! `provider_from_key` (the provider's model list, on a blocking task), `mcp_test` (the server's
//! handshake, on a task) and `model_test` (a probe agent's turn, settled by `settle` on the next
//! publish once `App::probes` has let go of it).

use super::protocol::{Command, Secret, ServerMsg};
use super::{ConnId, WebRegistryHandle};
use crate::app::App;
use crate::config::{McpServer, ModelEntry, ProviderKind, Registry, Settings, ALL_EFFORTS};
use crate::discover::{CState, Candidate};
use crate::engine::pattern::{self, Pattern};
use crate::hub::AgentId;
use crate::provider_key::{self, Backend, ProviderDraft};
use crate::settings_ops::{self, FieldKind};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

type Res = Result<Value, String>;

/// How long a `model_test` waits for its probe agent before the page is told so. The `/models`
/// row keeps waiting; a model that never answers is its provider's trouble, not the page's.
const TEST_WAIT: Duration = Duration::from_secs(120);

/// The Codex account's provider id: its models have no `[[provider]]` entry (`codex_account_models`).
const CODEX_ACCOUNT: &str = "openai";

/// What the App task and the event loop's `Web` share for these commands (`Link::shared`).
#[derive(Default)]
pub struct Shared {
    /// `model_test`s waiting for their probe agent's turn to end.
    tests: Mutex<Vec<PendingTest>>,
    /// Bumped by every pattern save or delete: the publisher's cached pattern list is stale.
    patterns_gen: AtomicU64,
}

impl Shared {
    /// The pattern generation, for the publisher to tell whether its list is still current.
    pub fn patterns_gen(&self) -> u64 {
        self.patterns_gen.load(Ordering::Relaxed)
    }
}

struct PendingTest {
    conn: ConnId,
    req: u64,
    agent: AgentId,
    alias: String,
    since: Instant,
}

/// Answer the `model_test`s whose probe agent is done (`App::on_turn_done` took it out of
/// `probes` and left the result in the `/models` row's status), or that waited too long.
pub fn settle(shared: &Shared, reg: &WebRegistryHandle, app: &App) {
    let mut tests = shared.tests.lock().unwrap_or_else(|e| e.into_inner());
    tests.retain(|t| {
        let done = !app.probes.contains_key(&t.agent);
        if !done && t.since.elapsed() < TEST_WAIT {
            return true;
        }
        let status = app.models_ui.status.get(&t.alias).cloned().unwrap_or_default();
        let msg = if !done {
            ServerMsg::err(t.req, format!("{} has not answered in {} s — the row shows the result when it does", t.alias, TEST_WAIT.as_secs()))
        } else if status.starts_with("ok") {
            ServerMsg::ok(t.req, json!({"status": status}))
        } else {
            ServerMsg::err(t.req, status.strip_prefix("error: ").unwrap_or(if status.is_empty() { "the test ended without an answer" } else { &status }).to_string())
        };
        crate::mlog!("web: model_test {} → {}", t.alias, if matches!(&msg, ServerMsg::Result(r) if r.ok) { "ok" } else { "error" });
        reg.send(t.conn, &msg);
        false
    });
}

/// What the UIs show of a stored key: dots, and its last four characters once it is long enough
/// that four of them only say which key it is.
pub fn mask(key: &str) -> String {
    let k = key.trim();
    let n = k.chars().count();
    if n > 8 {
        format!("••••{}", k.chars().skip(n - 4).collect::<String>())
    } else {
        "••••".into()
    }
}

/// One discovered model as the add-provider checklist shows it, and as `models_import` gets it
/// back (`discover::Candidate` is not serializable; its state is worked out again on import).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default)]
#[serde(default)]
pub struct CandidateView {
    pub model: String,
    pub context: Option<u64>,
    /// The context window came from the provider's list, not from Mantra's assumption.
    pub context_known: bool,
    pub efforts: Vec<String>,
    pub default_effort: String,
    pub note: String,
    pub vision: bool,
    /// "new" | "update" | "configured" (`discover::CState`).
    pub state: String,
    /// Pre-selected (`provider_key::candidates`: a short list is taken whole).
    pub selected: bool,
}

impl From<&Candidate> for CandidateView {
    fn from(c: &Candidate) -> Self {
        let state = match c.state {
            CState::New => "new",
            CState::Update => "update",
            CState::Configured => "configured",
        };
        CandidateView { model: c.model.clone(), context: c.context, context_known: c.context_known, efforts: c.efforts.clone(), default_effort: c.default_effort.clone(), note: c.note.clone(), vision: c.vision, state: state.into(), selected: c.selected }
    }
}

impl CandidateView {
    /// Back to a discovery row on `provider`, chosen; `classify` says again whether it is new.
    fn to_candidate(&self, provider: &str) -> Candidate {
        Candidate {
            provider: provider.to_string(),
            model: self.model.trim().to_string(),
            context: self.context,
            context_known: self.context_known,
            efforts: self.efforts.clone(),
            default_effort: self.default_effort.clone(),
            note: self.note.clone(),
            vision: self.vision,
            state: CState::New,
            selected: true,
        }
    }
}

/// What a key looks like, for the add-provider sheet — never the key itself — and `existing`: the
/// provider already at that URL, which the key would go into instead of a new one.
fn draft_view(d: &ProviderDraft, reg: &Registry) -> Value {
    let existing = provider_key::provider_at(reg, &d.base_url).map(|p| p.id.clone());
    json!({"base_url": d.base_url, "wire": d.wire, "id": d.id, "name": d.name, "guessed": d.guessed, "needs_url": d.needs_url(), "existing": existing})
}

fn kind_str(k: ProviderKind) -> &'static str {
    super::snapshot::backend_str(k)
}

fn parse_kind(v: &str) -> Result<ProviderKind, String> {
    match v.trim().to_ascii_lowercase().replace(['_', ' '], "-").as_str() {
        "bija" | "seed" => Ok(ProviderKind::Bija),
        "codex" => Ok(ProviderKind::Codex),
        "claude-code" | "claudecode" | "claude" => Ok(ProviderKind::ClaudeCode),
        "antigravity" | "agy" => Ok(ProviderKind::Antigravity),
        _ => Err("kind: bija, codex, claude-code or antigravity".into()),
    }
}

fn parse_bool(field: &str, v: &str) -> Result<bool, String> {
    match v.trim().to_ascii_lowercase().as_str() {
        "on" | "yes" | "true" | "1" => Ok(true),
        "off" | "no" | "false" | "0" => Ok(false),
        _ => Err(format!("{field}: on or off")),
    }
}

/// A context window as people write it: `200000`, `200k`, `1m`, `1,000,000`; empty (or "none",
/// "default") is no window of its own — Mantra assumes 200k. `None` for anything else.
fn parse_context(v: &str) -> Option<Option<u64>> {
    let t = v.trim().to_ascii_lowercase().replace([',', '_', ' '], "");
    if t.is_empty() || t == "none" || t == "default" {
        return Some(None);
    }
    let (num, mult) = match t.strip_suffix('k') {
        Some(n) => (n, 1_000.0),
        None => match t.strip_suffix('m') {
            Some(n) => (n, 1_000_000.0),
            None => (t.as_str(), 1.0),
        },
    };
    let n: f64 = num.parse().ok()?;
    let tokens = (n * mult).round();
    (1_000.0..=100_000_000.0).contains(&tokens).then_some(Some(tokens as u64))
}

/// One cell of `/models`' model grid from the web: validated, then set. Returns the model's alias
/// afterwards (an `alias` cell renames it). Nothing changes on a refusal.
pub fn set_model_field(reg: &mut Registry, alias: &str, field: &str, value: &str) -> Result<String, String> {
    let i = reg.models.iter().position(|m| m.alias == alias).ok_or_else(|| format!("no model named {alias}"))?;
    let v = value.trim();
    let m = &reg.models[i];
    if matches!(field, "context" | "compact") && reg.backend_of(m) == ProviderKind::Antigravity {
        return Err("Antigravity manages context and compaction in agy".into());
    }
    let mut next = m.clone();
    match field {
        "alias" => {
            let a = crate::util::slug(v);
            if a.is_empty() {
                return Err("alias: give the model a short name".into());
            }
            if a != alias && reg.models.iter().any(|x| x.alias == a) {
                return Err(format!("alias: {a} is taken"));
            }
            next.alias = a;
        }
        "provider" => {
            let p = if v.is_empty() { CODEX_ACCOUNT } else { v };
            if p != CODEX_ACCOUNT && !reg.providers.iter().any(|x| x.id == p) {
                let have: Vec<&str> = std::iter::once(CODEX_ACCOUNT).chain(reg.providers.iter().map(|x| x.id.as_str())).collect();
                return Err(format!("provider: no provider named {p} (there is: {})", have.join(", ")));
            }
            next.provider = p.to_string();
        }
        "model" => {
            if v.is_empty() {
                return Err("model: the id the provider knows it by".into());
            }
            next.model = v.to_string();
        }
        "context" => next.context_window = parse_context(v).ok_or("context: a number of tokens — 200000, 200k or 1m (empty: assume 200k)")?,
        "compact" => {
            let t = v.trim_end_matches('%').trim();
            next.auto_compact_percent = if t.is_empty() {
                None
            } else {
                Some(t.parse::<u8>().ok().filter(|p| (10..=99).contains(p)).ok_or("compact: a percent from 10 to 99 (empty: 85)")?)
            };
        }
        "effort" => {
            let list = m.efforts();
            if list.is_empty() {
                if !v.is_empty() {
                    return Err("effort: this model has no effort levels — give it some under efforts first".into());
                }
            } else if !list.iter().any(|e| e == v) {
                return Err(format!("effort: one of {}", list.join(", ")));
            }
            next.default_effort = v.to_string();
        }
        "efforts" => {
            let words: Vec<String> = v.split([',', ' ']).map(|w| w.trim().to_ascii_lowercase()).filter(|w| !w.is_empty()).collect();
            if let Some(bad) = words.iter().find(|w| !ALL_EFFORTS.contains(&w.as_str())) {
                return Err(format!("efforts: '{bad}' is none of {}", ALL_EFFORTS.join(", ")));
            }
            next.efforts = words;
            // the default follows the list: the nearest level it still has
            let list = next.efforts();
            if !list.is_empty() && !list.contains(&next.default_effort) {
                next.default_effort = next.resolve_effort(&m.default_effort);
            }
        }
        "vision" => next.vision = parse_bool("vision", v)?,
        "note" => next.note = v.to_string(),
        _ => return Err(format!("no model field named {field}")),
    }
    let a = next.alias.clone();
    reg.models[i] = next;
    Ok(a)
}

/// One cell of `/models`' provider grid from the web, validated, then set; a new `id` carries
/// the provider's models along. Returns the id afterwards. The Codex account has no cells.
pub fn set_provider_field(reg: &mut Registry, id: &str, field: &str, value: &str) -> Result<String, String> {
    let Some(i) = reg.providers.iter().position(|p| p.id == id) else {
        return Err(if id == CODEX_ACCOUNT { "the Codex account has no settings of its own — its models are edited in the table".into() } else { format!("no provider named {id}") });
    };
    let v = value.trim();
    let mut p = reg.providers[i].clone();
    match field {
        "id" => {
            let new = crate::util::slug(v);
            if new.is_empty() {
                return Err("id: give the provider a short name".into());
            }
            if new == CODEX_ACCOUNT {
                return Err("id: openai is the Codex account's — pick another".into());
            }
            if new != id && reg.providers.iter().any(|x| x.id == new) {
                return Err(format!("id: {new} is taken"));
            }
            p.id = new;
        }
        "name" => p.name = v.to_string(),
        "base_url" => p.base_url = v.to_string(),
        "env_key" => {
            let ok = v.chars().next().map(|c| c.is_ascii_alphabetic() || c == '_').unwrap_or(true) && v.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
            if !ok {
                return Err("env_key: the name of an environment variable (letters, digits and _)".into());
            }
            p.env_key = v.to_string();
        }
        "api_key" => p.api_key = Some(v.to_string()).filter(|k| !k.is_empty()),
        "kind" => p.kind = parse_kind(v)?,
        "auth" => {
            p.auth = match v.to_ascii_lowercase().replace(['-', ' '], "_").as_str() {
                "subscription" => "subscription".into(),
                "api_key" | "apikey" | "key" => "api_key".into(),
                _ => return Err("auth: subscription or api_key".into()),
            }
        }
        "wire" => {
            p.wire = match v.to_ascii_lowercase().as_str() {
                "" | "openai" | "openai-chat" | "chat" => String::new(),
                "anthropic" | "messages" => "anthropic".into(),
                _ => return Err("wire: openai (Chat Completions) or anthropic (Messages)".into()),
            }
        }
        _ => return Err(format!("no provider field named {field}")),
    }
    let new = p.id.clone();
    if new != id {
        for m in reg.models.iter_mut().filter(|m| m.provider == id) {
            m.provider = new.clone();
        }
    }
    reg.providers[i] = p;
    Ok(new)
}

/// A model on `provider`: the given id and alias, or a placeholder row to fill in (`modelN`).
fn add_model(reg: &mut Registry, provider: &str, model: Option<&str>, alias: Option<&str>) -> Result<String, String> {
    if provider != CODEX_ACCOUNT && !reg.providers.iter().any(|p| p.id == provider) {
        return Err(format!("no provider named {provider}"));
    }
    let taken: Vec<String> = reg.models.iter().map(|m| m.alias.clone()).collect();
    let model = model.map(str::trim).filter(|m| !m.is_empty());
    let alias = match alias.map(crate::util::slug).filter(|a| !a.is_empty()) {
        Some(a) if taken.contains(&a) => return Err(format!("alias: {a} is taken")),
        Some(a) => a,
        None => match model {
            Some(m) => crate::discover::unique_alias(m, provider, &taken),
            None => (reg.models.len() + 1..).map(|n| format!("model{n}")).find(|a| !taken.contains(a)).unwrap_or_default(),
        },
    };
    let model = model.unwrap_or("model-id").to_string();
    let mut entry = ModelEntry { vision: crate::config::takes_images(&model), alias: alias.clone(), provider: provider.to_string(), model, ..Default::default() };
    if reg.providers.iter().any(|p| p.id == provider && p.kind == ProviderKind::Antigravity) {
        entry.efforts = ["low", "medium", "high", "xhigh", "max"].into_iter().map(String::from).collect();
    }
    reg.models.push(entry);
    Ok(alias)
}

/// The one field kind a UI renders, as the web gets it.
fn field_view(f: settings_ops::Field) -> Value {
    let (kind, options, range) = match &f.kind {
        FieldKind::Text => ("text", None, None),
        FieldKind::Secret => ("secret", None, None),
        FieldKind::Enum(o) => ("enum", Some(o.clone()), None),
        FieldKind::Bool => ("bool", None, None),
        FieldKind::Number { min, max } => ("number", None, Some((*min, *max))),
    };
    // a secret is only ever said to be there
    let value = if kind == "secret" { String::from(if f.value.is_empty() { "not set" } else { "set" }) } else { f.value };
    json!({
        "section": f.section, "key": f.key, "label": f.label, "kind": kind, "options": options,
        "min": range.map(|r| r.0), "max": range.map(|r| r.1), "value": value, "hint": f.hint,
        "effect": settings_ops::takes_effect(&f.key),
    })
}

impl App {
    /// Keep Solo's saved selection valid when a web edit adds or removes models. An empty
    /// registry is a supported setup state; `start_solo` then shows the /models hint.
    fn web_refresh_model_selection(&mut self) {
        let valid = self.registry.alias_problem(&self.settings.default_model).is_none();
        if valid { return; }
        self.settings.default_model = self.registry.models.iter()
            .find(|m| self.demo || self.registry.alias_problem(&m.alias).is_none())
            .map(|m| m.alias.clone()).unwrap_or_default();
        self.save_settings();
        self.start_solo();
    }

    /// A pattern was saved or deleted: the next publish reads the pattern list again.
    fn patterns_changed(&self) {
        if let Some(l) = &self.web {
            l.shared.patterns_gen.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// `settings_get`'s reply: every field, the MCP servers, what the web layer serves.
    pub(super) fn settings_view(&self) -> Value {
        let patterns = Pattern::list(&self.project);
        let fields: Vec<Value> = self.settings.fields(&self.registry, &patterns).into_iter().map(field_view).collect();
        let solo: Vec<String> = self.settings.mcp_for(&self.settings.solo_mcp).into_iter().map(|(n, _)| n).collect();
        let mcp: Vec<Value> = self
            .settings
            .mcp_servers
            .iter()
            .map(|(name, s)| {
                json!({
                    "name": name, "command": s.command, "args": s.args, "line": crate::shown_command(s),
                    // the names only: an env value is a token, typically
                    "env": s.env.keys().collect::<Vec<_>>(),
                    "enabled": s.enabled, "solo": solo.contains(name), "problem": crate::config::mcp_name_problem(name),
                })
            })
            .collect();
        let web = self.web.as_ref().map(|l| json!({"urls": l.info.urls, "tls": l.info.tls, "self_signed": l.info.self_signed, "password": l.info.password, "push": l.info.push}));
        json!({"fields": fields, "mcp": mcp, "web": web, "demo": self.demo})
    }

    /// `models_get`'s reply. The Codex account leads the providers as a read-only row whenever a
    /// model is on it — deleting that row removes them all, the counterpart of "Add Codex".
    pub(super) fn registry_view(&self) -> Value {
        let reg = &self.registry;
        let models: Vec<Value> = reg
            .models
            .iter()
            .map(|m| {
                json!({
                    "alias": m.alias, "provider": if m.is_custom_provider() { m.provider.as_str() } else { CODEX_ACCOUNT }, "provider_name": reg.provider_name(&m.provider),
                    "model": m.model, "backend": kind_str(reg.backend_of(m)), "context_window": if reg.backend_of(m) == ProviderKind::Antigravity { None } else { m.context_window }, "effective_context_window": if reg.backend_of(m) == ProviderKind::Antigravity { None } else { Some(reg.effective_context_window(&m.alias, &self.settings.default_model)) }, "compact_percent": if reg.backend_of(m) == ProviderKind::Antigravity { None } else { m.auto_compact_percent },
                    "default_effort": m.default_effort, "efforts": m.efforts(), "vision": m.vision, "note": m.note,
                    "problem": reg.alias_problem(&m.alias), "test": self.models_ui.status.get(&m.alias), "default": m.alias == self.settings.default_model,
                })
            })
            .collect();
        let mut providers = vec![];
        let codex = reg.codex_account_models();
        if !codex.is_empty() {
            providers.push(json!({"id": CODEX_ACCOUNT, "name": reg.provider_name(CODEX_ACCOUNT), "kind": "codex", "account": true, "models": codex.len(), "key_set": true}));
        }
        for p in &reg.providers {
            providers.push(json!({
                "id": p.id, "name": p.name, "base_url": p.base_url, "env_key": p.env_key,
                "api_key": p.api_key.as_deref().filter(|k| !k.trim().is_empty()).map(mask),
                "key_set": p.resolve_key().is_some(), "kind": kind_str(p.kind), "auth": p.auth,
                "wire": if p.kind == ProviderKind::Bija { if p.wire.trim().is_empty() { "openai" } else { p.wire.as_str() } } else { "" },
                "host": p.host(), "draft": p.draft_reason(), "models": reg.models.iter().filter(|m| m.provider == p.id).count(), "account": false,
            }));
        }
        // the demo's backends are mocks, there whatever is installed
        let installed = |found: bool| found || self.demo;
        json!({
            "models": models, "providers": providers,
            "backends": {
                "codex": {"installed": installed(crate::config::codex_available()), "added": !codex.is_empty()},
                "claude": {"installed": installed(crate::config::claude_available()), "added": reg.providers.iter().any(|p| p.kind == ProviderKind::ClaudeCode)},
                "antigravity": {"installed": crate::config::antigravity_command_available(&self.settings.antigravity_command), "added": reg.providers.iter().any(|p| p.kind == ProviderKind::Antigravity)},
            },
            "options": {"kinds": ["bija", "codex", "claude-code", "antigravity"], "wires": ["openai", "anthropic"], "auths": ["subscription", "api_key"], "efforts": ALL_EFFORTS},
        })
    }

    /// The configuration commands answered on the spot.
    pub(super) fn web_config(&mut self, cmd: Command, from_relay: bool) -> Res {
        match cmd {
            Command::RunStop => {
                let Some(id) = self.run.as_ref().filter(|r| r.is_active()).map(|r| r.id.clone()) else { return Err("no active run to stop".into()) };
                self.stop_run();
                Ok(json!({"run": id}))
            }
            Command::PatternsList => Ok(json!({"patterns": Pattern::summaries(&self.project), "current": self.pattern_name, "default": self.settings.default_pattern})),
            Command::PatternGet { name } => self.pattern_get(&name),
            Command::PatternSave { name, pattern } => self.pattern_save(&name, pattern),
            Command::PatternDelete { name } => {
                let path = Pattern::delete_file(&name, &self.project)?;
                self.patterns_changed();
                // a pattern that no longer loads cannot be the one new runs use
                if Pattern::load(&self.settings.default_pattern, &self.project).is_err() {
                    let _ = self.set_setting("default_pattern", pattern::NAME_DEFAULT);
                }
                if Pattern::load(&self.pattern_name, &self.project).is_err() {
                    self.pattern_name = pattern::NAME_DEFAULT.into();
                }
                Ok(json!({"path": path.display().to_string()}))
            }
            Command::SettingsGet => Ok(self.settings_view()),
            Command::SettingsSet { key, value } => {
                // It derives the remote link's key: changed from a remote session, the next start
                // would lock that very session out — the same reason `remote_rotate` is refused.
                if from_relay && key == "web.password" {
                    return Err("the web password is changed at this machine, not from a remote session".into());
                }
                let note = self.set_setting(&key, &value.0)?;
                Ok(json!({"note": note, "settings": self.settings_view()}))
            }
            Command::McpSet { name, command, args, env, enabled } => {
                self.change_mcp(|s| mcp_set(s, &name, command.as_deref(), args.as_ref(), env.as_ref(), enabled))?;
                Ok(json!({"settings": self.settings_view()}))
            }
            Command::McpRemove { name } => {
                self.change_mcp(|s| s.mcp_remove(&name))?;
                Ok(json!({"settings": self.settings_view()}))
            }
            Command::ModelsGet => Ok(self.registry_view()),
            Command::ModelSet { alias, field, value } => {
                let renamed = set_model_field(&mut self.registry, &alias, &field, &value)?;
                // Solo's default model follows its alias, or every later settings change is refused
                if renamed != alias && self.settings.default_model == alias {
                    let mut s = self.settings.clone();
                    s.default_model = renamed.clone();
                    self.apply_settings(s)?;
                }
                self.models_ui.status.remove(&alias);
                self.save_registry()?;
                Ok(json!({"alias": renamed, "registry": self.registry_view()}))
            }
            Command::ModelAdd { provider, model, alias } => {
                let alias = add_model(&mut self.registry, &provider, model.as_deref(), alias.as_deref())?;
                self.save_registry()?;
                self.web_refresh_model_selection();
                Ok(json!({"alias": alias, "registry": self.registry_view()}))
            }
            Command::ModelDelete { alias } => {
                let i = self.registry.models.iter().position(|m| m.alias == alias).ok_or_else(|| format!("no model named {alias}"))?;
                self.registry.models.remove(i);
                self.models_ui.status.remove(&alias);
                self.save_registry()?;
                self.web_refresh_model_selection();
                Ok(json!({"registry": self.registry_view()}))
            }
            Command::ProviderSet { id, field, value } => {
                let id = set_provider_field(&mut self.registry, &id, &field, &value.0)?;
                self.save_registry()?;
                Ok(json!({"id": id, "registry": self.registry_view()}))
            }
            Command::ProviderDelete { id } => {
                let account = id == CODEX_ACCOUNT && !self.registry.providers.iter().any(|p| p.id == id);
                let on: Vec<String> = if account { self.registry.codex_account_models() } else { self.registry.models.iter().filter(|m| m.provider == id).map(|m| m.alias.clone()).collect() };
                if !account && !self.registry.providers.iter().any(|p| p.id == id) {
                    return Err(format!("no provider named {id}"));
                }
                if account && on.is_empty() {
                    return Err("there are no Codex models to remove".into());
                }
                let n = if account {
                    self.registry.remove_codex_account()
                } else {
                    self.registry.providers.retain(|p| p.id != id);
                    let before = self.registry.models.len();
                    self.registry.models.retain(|m| m.provider != id);
                    before - self.registry.models.len()
                };
                self.save_registry()?;
                self.web_refresh_model_selection();
                Ok(json!({"models": n, "registry": self.registry_view()}))
            }
            Command::ModelsImport { provider, candidates } => {
                if provider != CODEX_ACCOUNT && !self.registry.providers.iter().any(|p| p.id == provider) {
                    return Err(format!("no provider named {provider}"));
                }
                let mut cands: Vec<Candidate> = candidates.iter().filter(|c| !c.model.trim().is_empty()).map(|c| c.to_candidate(&provider)).collect();
                if cands.is_empty() {
                    return Err("nothing selected".into());
                }
                for c in &mut cands {
                    crate::discover::classify(&self.registry, c);
                }
                let (added, updated) = crate::discover::apply(&mut self.registry, &cands);
                if added + updated == 0 {
                    return Err("nothing to add — those models are there already".into());
                }
                self.save_registry()?;
                self.web_refresh_model_selection();
                Ok(json!({"added": added, "updated": updated, "registry": self.registry_view()}))
            }
            Command::AddBackend { kind } => {
                let backend = match kind.as_str() {
                    "codex" => Backend::Codex,
                    "claude-code" | "claude" => Backend::ClaudeCode,
                    "antigravity" | "agy" => Backend::Antigravity,
                    _ => return Err("kind: codex, claude-code or antigravity".into()),
                };
                // the demo's backends are mocks: added whatever is installed, as main.rs does
                let added = self.add_backend(backend)?;
                if added > 0 {
                    self.web_refresh_model_selection();
                }
                Ok(json!({"added": added, "registry": self.registry_view()}))
            }
            other => Err(format!("internal: {} is not a configuration command", other.name())),
        }
    }

    /// The commands answered later — `provider_from_key` (its probe), `mcp_test`, `model_test` —
    /// started here; `false` for any other command.
    pub(super) fn web_config_later(&mut self, conn: ConnId, req: u64, cmd: &Command) -> bool {
        let Some(link) = self.web.clone() else { return matches!(cmd, Command::ProviderFromKey { .. } | Command::McpTest { .. } | Command::ModelTest { .. }) };
        let reply = |msg: ServerMsg| link.reply(conn, &msg);
        match cmd {
            Command::ProviderFromKey { key, base_url, probe } => {
                let draft = provider_key::provider_from_key(&key.0, base_url.as_deref());
                if *probe == Some(false) {
                    reply(ServerMsg::ok(req, json!({"draft": draft_view(&draft, &self.registry)})));
                    return true;
                }
                let id = match provider_key::add_provider(&mut self.registry, &draft).and_then(|id| self.save_registry().map(|()| id)) {
                    Ok(id) => id,
                    Err(e) => {
                        reply(ServerMsg::err(req, e));
                        return true;
                    }
                };
                let Some(entry) = self.registry.providers.iter().find(|p| p.id == id).cloned() else {
                    reply(ServerMsg::err(req, format!("provider {id} vanished")));
                    return true;
                };
                // what the reply carries is the registry as it is now, the provider in it; the
                // candidates are classified against a copy (the probe takes seconds)
                let (reg, view, dv) = (self.registry.clone(), self.registry_view(), draft_view(&draft, &self.registry));
                tokio::task::spawn_blocking(move || {
                    let (candidates, probe_error) = match provider_key::probe(&entry) {
                        Ok(found) => (provider_key::candidates(&reg, &id, &found).iter().map(CandidateView::from).collect(), None),
                        Err(e) => (vec![], Some(e)),
                    };
                    crate::mlog!("web: provider_from_key {} → {}", entry.name, probe_error.as_deref().unwrap_or("ok"));
                    link.reply(conn, &ServerMsg::ok(req, json!({"provider": id, "draft": dv, "candidates": candidates, "probe_error": probe_error, "registry": view})));
                });
                true
            }
            Command::McpTest { name } => {
                let Some(server) = self.settings.mcp_servers.get(name).cloned() else {
                    reply(ServerMsg::err(req, self.settings.mcp_known(name).err().unwrap_or_default()));
                    return true;
                };
                let name = name.clone();
                tokio::spawn(async move {
                    let msg = match crate::mcp_test(&name, &server, crate::MCP_TEST_BUDGET).await {
                        Ok(report) => ServerMsg::ok(req, json!({"report": report, "enabled": server.enabled})),
                        Err(e) => ServerMsg::err(req, e),
                    };
                    crate::mlog!("web: mcp_test {name} → {}", if matches!(&msg, ServerMsg::Result(r) if r.ok) { "ok" } else { "error" });
                    link.reply(conn, &msg);
                });
                true
            }
            Command::ModelTest { alias } => {
                if !self.registry.models.iter().any(|m| m.alias == *alias) {
                    reply(ServerMsg::err(req, "unknown model"));
                    return true;
                }
                let before: Vec<AgentId> = self.probes.keys().copied().collect();
                self.probe_model(alias);
                match self.probes.iter().find(|(id, (a, _))| a == alias && !before.contains(id)).map(|(id, _)| *id) {
                    Some(agent) => link.shared.tests.lock().unwrap_or_else(|e| e.into_inner()).push(PendingTest { conn, req, agent, alias: alias.clone(), since: Instant::now() }),
                    // refused before any request (a draft provider): the row says why
                    None => {
                        let why = self.models_ui.status.get(alias).cloned().unwrap_or_else(|| "not tested".into());
                        reply(ServerMsg::err(req, why.strip_prefix("error: ").unwrap_or(&why).to_string()));
                    }
                }
                true
            }
            _ => false,
        }
    }

    fn pattern_get(&self, name: &str) -> Res {
        let p = Pattern::load(name, &self.project).map_err(|e| e.to_string())?;
        let tag = p.tag();
        let path = p.source_dir.as_ref().map(|d| d.join(format!("{}.toml", crate::util::slug(&p.name))).display().to_string());
        let models: Vec<Value> = self.registry.models.iter().map(|m| json!({"alias": m.alias, "efforts": m.efforts(), "problem": self.registry.alias_problem(&m.alias)})).collect();
        let options = json!({
            "kinds": pattern::KINDS, "colors": pattern::COLORS, "permissions": pattern::PERMISSIONS, "sandboxes": ["read-only", "workspace-write"],
            "push_modes": pattern::PUSH_MODES, "isolation": ["auto", "worktree", "shared"], "orchestrator_context": ["fresh", "compact"],
            "models": models, "mcp": self.settings.mcp_servers.keys().collect::<Vec<_>>(),
            "parallel": [pattern::PARALLEL_RANGE.start(), pattern::PARALLEL_RANGE.end()], "shared_chars": [pattern::SHARED_CHARS_MIN, pattern::SHARED_CHARS_MAX],
        });
        Ok(json!({
            "pattern": p, "toml": p.to_toml(), "tag": tag, "builtin": Pattern::builtin_named(&p.name).is_some(), "edited": tag == "edited", "yours": tag == "yours",
            "path": path, "current": p.name == self.pattern_name, "default": p.name == self.settings.default_pattern, "options": options,
        }))
    }

    /// `pattern_save`: the edited JSON back into a `Pattern`, saved where the original lives (a
    /// project's own pattern stays in the project) or as a copy (`Pattern::save_as` and its
    /// refusals); every reason it is refused, one per line.
    fn pattern_save(&mut self, name: &str, pattern: Value) -> Res {
        let mut p: Pattern = serde_json::from_value(pattern).map_err(|e| format!("not a pattern: {e}"))?;
        p.source_dir = Pattern::load(&p.name, &self.project).ok().and_then(|o| o.source_dir);
        let (saved, path) = p.save_as(name, &self.project).map_err(|errs| errs.join("\n"))?;
        self.patterns_changed();
        let tag = Pattern::load(&saved.name, &self.project).map(|p| p.tag()).unwrap_or("yours");
        Ok(json!({"name": saved.name, "path": path.display().to_string(), "tag": tag}))
    }
}

/// `mcp_set` on one settings: a server added (a command is needed then), or changed — its
/// command line, its `env` (replaced whole), whether it is on.
fn mcp_set(s: &mut Settings, name: &str, command: Option<&str>, args: Option<&Vec<String>>, env: Option<&BTreeMap<String, Secret>>, enabled: Option<bool>) -> Result<(), String> {
    let split = |c: &str| -> Result<(String, Vec<String>), String> {
        match args {
            Some(a) => Ok((c.trim().to_string(), a.clone())),
            None => {
                let mut words = settings_ops::parse_command_line(c)?;
                if words.is_empty() {
                    return Err(format!("{name}: a server needs a command"));
                }
                let first = words.remove(0);
                Ok((first, words))
            }
        }
    };
    let env = env.map(|e| e.iter().map(|(k, v)| (k.clone(), v.0.clone())).collect::<BTreeMap<_, _>>());
    match s.mcp_servers.get(name).cloned() {
        None => {
            let Some(c) = command else { return s.mcp_known(name) };
            let (command, args) = split(c)?;
            s.mcp_add(name, McpServer { command, args, env: env.unwrap_or_default(), enabled: enabled.unwrap_or(true) })
        }
        Some(mut server) => {
            if let Some(c) = command {
                (server.command, server.args) = split(c)?;
            }
            if let Some(e) = env {
                server.env = e;
            }
            if let Some(on) = enabled {
                server.enabled = on;
            }
            if server.command.trim().is_empty() {
                return Err(format!("{name}: a server needs a command"));
            }
            s.mcp_servers.insert(name.to_string(), server);
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn configured_registry() -> Registry {
        let mut reg = Registry::defaults();
        reg.providers.push(crate::config::bija_default_provider());
        reg.models.extend(crate::config::bija_default_model_entries("bija"));
        reg
    }

    #[test]
    fn a_key_shows_its_last_four_only_when_that_says_nothing_more() {
        assert_eq!(mask("sk-proj-abcdefgh1234"), "••••1234");
        assert_eq!(mask("  key_0123456789  "), "••••6789");
        assert_eq!(mask("short123"), "••••", "eight characters: four would be half of it");
        assert_eq!(mask(""), "••••");
    }

    #[test]
    fn context_windows_read_the_way_people_write_them() {
        assert_eq!(parse_context("200k"), Some(Some(200_000)));
        assert_eq!(parse_context("1m"), Some(Some(1_000_000)));
        assert_eq!(parse_context("1.5M"), Some(Some(1_500_000)));
        assert_eq!(parse_context("272,000"), Some(Some(272_000)));
        assert_eq!(parse_context(" "), Some(None), "empty: the assumed window");
        assert_eq!(parse_context("none"), Some(None));
        assert_eq!(parse_context("big"), None);
        assert_eq!(parse_context("12"), None, "a dozen tokens is no window");
    }

    /// Every cell checks what it takes and refuses without changing anything; the alias and the
    /// provider id carry what hangs off them.
    #[test]
    fn model_and_provider_cells_validate_and_carry_their_references() {
        let mut reg = configured_registry();
        let before = format!("{:?}", reg.models);
        for (field, value, words) in [
            ("alias", "  ", "short name"),
            ("alias", "luna", "luna is taken"),
            ("provider", "nowhere", "no provider named nowhere (there is: openai, bija)"),
            ("model", "", "the id"),
            ("context", "lots", "200k"),
            ("compact", "120", "10 to 99"),
            ("effort", "sideways", "one of"),
            ("efforts", "low, turbo", "'turbo'"),
            ("vision", "maybe", "on or off"),
            ("colour", "x", "no model field named colour"),
        ] {
            let e = set_model_field(&mut reg, "sol", field, value).expect_err(field);
            assert!(e.contains(words), "{field}={value}: {e}");
        }
        assert_eq!(format!("{:?}", reg.models), before, "a refusal changes nothing");
        assert!(set_model_field(&mut reg, "nope", "note", "x").unwrap_err().contains("no model named nope"));
        assert_eq!(set_model_field(&mut reg, "sol", "context", "400k"), Ok("sol".into()));
        assert_eq!(reg.get("sol").and_then(|m| m.context_window), Some(400_000));
        assert_eq!(set_model_field(&mut reg, "sol", "alias", "Sun Model"), Ok("sun-model".into()));
        assert!(reg.get("sun-model").is_some());
        // efforts narrowed: the default follows to the nearest level left
        set_model_field(&mut reg, "sun-model", "effort", "high").unwrap();
        set_model_field(&mut reg, "sun-model", "efforts", "low medium").unwrap();
        assert_eq!(reg.get("sun-model").map(|m| m.default_effort.as_str()), Some("medium"));

        assert!(set_provider_field(&mut reg, "openai", "name", "x").unwrap_err().contains("Codex account"));
        for (field, value, words) in [("id", "openai", "Codex account's"), ("env_key", "MY KEY", "environment variable"), ("kind", "rpc", "bija, codex"), ("auth", "magic", "subscription"), ("wire", "grpc", "openai (Chat")] {
            let e = set_provider_field(&mut reg, "bija", field, value).expect_err(field);
            assert!(e.contains(words), "{field}={value}: {e}");
        }
        assert_eq!(set_provider_field(&mut reg, "bija", "id", "main"), Ok("main".into()));
        assert!(reg.models.iter().all(|m| m.provider == "main"), "the models came along");
        set_provider_field(&mut reg, "main", "wire", "Messages").unwrap();
        set_provider_field(&mut reg, "main", "api_key", " sk-abc ").unwrap();
        let p = &reg.providers[0];
        assert_eq!((p.wire.as_str(), p.api_key.as_deref()), ("anthropic", Some("sk-abc")));
        set_provider_field(&mut reg, "main", "api_key", "").unwrap();
        assert_eq!(reg.providers[0].api_key, None, "an empty key clears it");
    }

    #[test]
    fn a_new_model_gets_a_free_alias() {
        let mut reg = configured_registry();
        assert_eq!(add_model(&mut reg, "bija", None, None), Ok("model5".into()));
        assert_eq!(add_model(&mut reg, "bija", Some("mx-5.2"), None), Ok("mx-5.2".into()));
        assert!(add_model(&mut reg, "bija", Some("x"), Some("luna")).unwrap_err().contains("taken"));
        assert!(add_model(&mut reg, "nowhere", None, None).unwrap_err().contains("no provider named nowhere"));
        assert_eq!(reg.models.len(), 6);
    }

    #[test]
    fn mcp_set_adds_from_a_command_line_and_changes_in_place() {
        let mut s = Settings::default();
        assert!(mcp_set(&mut s, "blender", None, None, None, Some(false)).unwrap_err().contains("no MCP server named blender"));
        mcp_set(&mut s, "blender", Some("uvx blender-mcp --port '9 1'"), None, Some(&[("TOKEN".to_string(), Secret("t".into()))].into()), None).unwrap();
        let b = &s.mcp_servers["blender"];
        assert_eq!((b.command.as_str(), b.args.clone(), b.enabled, b.env["TOKEN"].as_str()), ("uvx", vec!["blender-mcp".to_string(), "--port".into(), "9 1".into()], true, "t"));
        mcp_set(&mut s, "blender", None, None, None, Some(false)).unwrap();
        assert!(!s.mcp_servers["blender"].enabled && s.mcp_servers["blender"].env.contains_key("TOKEN"), "only what was sent changes");
        assert!(mcp_set(&mut s, "blender", Some("  "), None, None, None).unwrap_err().contains("needs a command"));
        assert!(mcp_set(&mut s, "x", Some("uvx 'open"), None, None, None).unwrap_err().contains("quote"));
        assert!(mcp_set(&mut s, "a b", Some("uvx"), None, None, None).unwrap_err().contains("letters"));
    }
    #[test]
    fn antigravity_models_use_account_ids_and_supported_efforts() {
        let mut reg = Registry { models: vec![], providers: vec![crate::config::ProviderEntry { id: "google".into(), kind: ProviderKind::Antigravity, ..Default::default() }] };
        let alias = add_model(&mut reg, "google", Some("account/model-slug"), Some("google-main")).unwrap();
        let m = reg.get(&alias).unwrap();
        assert_eq!(reg.backend_of(m), ProviderKind::Antigravity);
        assert_eq!(m.model, "account/model-slug");
        assert_eq!(m.efforts(), ["low", "medium", "high", "xhigh", "max"]);
        assert_eq!(m.context_window, None);
        assert_eq!(m.auto_compact_percent, None);
        assert_eq!(parse_kind("agy"), Ok(ProviderKind::Antigravity));
        assert!(set_model_field(&mut reg, &alias, "context", "1m").unwrap_err().contains("agy"));
        assert!(set_model_field(&mut reg, &alias, "compact", "90").unwrap_err().contains("agy"));
    }

}
