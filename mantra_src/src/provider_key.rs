//! A provider from nothing but its API key, and the add-on backends (Codex, Claude Code): what
//! `/models` and the web UI's models page both call, so "paste the key, Mantra does the rest"
//! has one set of rules.
//!
//! The flow: `provider_from_key` guesses the API from the key's prefix (`ProviderDraft`, with the
//! base URL to confirm, or none to ask for); `add_provider` puts it in the registry — or the key
//! into the provider already at that URL; `probe` lists its models (network: callers run it off
//! the UI task); `candidates` turns the list into the discovery picker's rows. `probe_and_import`
//! is all three for a caller that may block.

use crate::config::{ModelEntry, ProviderEntry, ProviderKind, Registry};
use crate::discover::{CState, Candidate, Found};

/// What a key's prefix says about where it belongs: the API's base URL and the wire bija speaks
/// to it. Longest prefixes first (`sk-ant-` before `sk-`). The one table that names vendors;
/// everything else about a provider (its id, its name) is derived from the URL's host.
const KEY_PREFIXES: &[(&str, &str, &str)] = &[
    ("sk-ant-", "https://api.anthropic.com/v1", WIRE_ANTHROPIC),
    ("sk-or-", "https://openrouter.ai/api/v1", WIRE_OPENAI),
    ("sk-proj-", "https://api.openai.com/v1", WIRE_OPENAI),
    ("gsk_", "https://api.groq.com/openai/v1", WIRE_OPENAI),
    ("xai-", "https://api.x.ai/v1", WIRE_OPENAI),
    ("AIza", "https://generativelanguage.googleapis.com/v1beta/openai", WIRE_OPENAI),
    ("csk-", "https://api.cerebras.ai/v1", WIRE_OPENAI),
    ("fw_", "https://api.fireworks.ai/inference/v1", WIRE_OPENAI),
    ("sk-", "https://api.openai.com/v1", WIRE_OPENAI),
];

const WIRE_OPENAI: &str = "openai";
const WIRE_ANTHROPIC: &str = "anthropic";

/// The host whose API is the Messages one, whatever the key looks like.
const ANTHROPIC_HOST: &str = "api.anthropic.com";

/// Discovery pre-selects every new model up to this many; a bigger catalogue starts with none
/// selected (filter and pick).
const PRESELECT_UP_TO: usize = 12;

/// A provider made from a key, before it is in the registry: what the UI shows for the user to
/// confirm (the URL above all) and `add_provider` takes.
#[derive(Clone, PartialEq)]
pub struct ProviderDraft {
    pub key: String,
    /// The API's base URL: given, or guessed from the key's prefix; "" when neither — then the
    /// UI asks for it (`needs_url`).
    pub base_url: String,
    /// `openai` (Chat Completions) or `anthropic` (Messages).
    pub wire: String,
    /// From the URL's host (`api.groq.com` → `groq`); made unique when it is added.
    pub id: String,
    /// The host (`api.groq.com`), what the provider is called until the user renames it.
    pub name: String,
    /// The URL came from the key's prefix, not from the caller: the UI shows it as a guess.
    pub guessed: bool,
}

/// Never prints the key: drafts end up in test failures and logs.
impl std::fmt::Debug for ProviderDraft {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProviderDraft").field("key", &"••••").field("base_url", &self.base_url).field("wire", &self.wire).field("id", &self.id).field("name", &self.name).field("guessed", &self.guessed).finish()
    }
}

impl ProviderDraft {
    /// Whether the URL must still be asked for: the key's prefix named no provider and none was given.
    pub fn needs_url(&self) -> bool {
        self.base_url.trim().is_empty()
    }
}

/// The provider a key belongs to: the API from its prefix (`KEY_PREFIXES`), or `base_url` when
/// the caller gives one (it wins: a `sk-` key for another OpenAI-compatible host keeps that host).
/// The wire follows the key — a Messages-API key speaks Messages at any URL — or the host. An
/// unknown prefix without a URL gives a draft that `needs_url`.
pub fn provider_from_key(key: &str, base_url: Option<&str>) -> ProviderDraft {
    let key = key.trim();
    let guess = KEY_PREFIXES.iter().find(|(prefix, _, _)| key.starts_with(prefix));
    let given = base_url.map(str::trim).filter(|u| !u.is_empty()).map(|u| u.trim_end_matches('/').to_string());
    let guessed = given.is_none() && guess.is_some();
    let base_url = given.or_else(|| guess.map(|(_, url, _)| url.to_string())).unwrap_or_default();
    let authority = ProviderEntry { base_url: base_url.clone(), ..Default::default() }.host().unwrap_or_default();
    let host = bare_host(&authority);
    let wire = if guess.is_some_and(|(_, _, w)| *w == WIRE_ANTHROPIC) || host == ANTHROPIC_HOST { WIRE_ANTHROPIC } else { WIRE_OPENAI };
    let id = if authority.is_empty() { String::new() } else { id_from_host(&host) };
    ProviderDraft { key: key.to_string(), base_url, wire: wire.into(), id, name: authority.to_ascii_lowercase(), guessed }
}

/// `host[:port]` without the port and IPv6 brackets, lower-cased.
fn bare_host(authority: &str) -> String {
    let a = authority.to_ascii_lowercase();
    if let Some(inner) = a.strip_prefix('[') {
        return inner.split(']').next().unwrap_or("").to_string();
    }
    a.split(':').next().unwrap_or("").to_string()
}

/// A provider id from a host: the label before the public suffix (`api.groq.com` → `groq`,
/// `openrouter.ai` → `openrouter`, `api.x.ai` → `x`, `api.example.co.uk` → `example`); an address
/// or a one-label name (a local server) is `local`.
fn id_from_host(host: &str) -> String {
    let labels: Vec<&str> = host.split('.').filter(|l| !l.is_empty()).collect();
    if labels.len() < 2 || host.parse::<std::net::IpAddr>().is_ok() {
        return "local".into();
    }
    // A country code under a generic second level (`co.uk`, `com.au`) is one suffix, not two.
    let mut suffix = 1;
    if labels.len() >= 3 && labels[labels.len() - 1].len() == 2 && ["co", "com", "net", "org", "ac", "gov", "edu"].contains(&labels[labels.len() - 2]) {
        suffix = 2;
    }
    let id = crate::util::slug(labels[labels.len() - 1 - suffix]);
    if id.is_empty() {
        "provider".into()
    } else {
        id
    }
}

/// `want`, or `want-2`, `want-3`… — the first id no provider has. `openai` is never free: it is
/// Codex's own account, which has no `[[provider]]` entry (`ModelEntry::is_custom_provider`).
fn unique_id(reg: &Registry, want: &str) -> String {
    let taken = |id: &str| id == "openai" || reg.providers.iter().any(|p| p.id == id);
    if !taken(want) {
        return want.to_string();
    }
    (2..).map(|n| format!("{want}-{n}")).find(|id| !taken(id)).unwrap_or_else(|| want.to_string())
}

/// The URL with what does not change where a request goes trimmed off: case, a trailing slash.
fn same_url(a: &str, b: &str) -> bool {
    let norm = |u: &str| u.trim().trim_end_matches('/').to_ascii_lowercase();
    norm(a) == norm(b)
}

/// The Bija provider already at `base_url`, if any — where a key for that URL goes instead of a
/// second provider (a fresh install's `bija` row is at the OpenAI API, so a pasted `sk-` key
/// completes it), and where a caller finds the provider after a failed `probe_and_import`.
pub fn provider_at<'a>(reg: &'a Registry, base_url: &str) -> Option<&'a ProviderEntry> {
    reg.providers.iter().find(|p| p.kind == ProviderKind::Bija && same_url(&p.base_url, base_url))
}

/// Puts the draft in the registry and returns the provider's id: the key goes into the Bija
/// provider already at that URL (replacing any `env_key`: a pasted key is the user's say), else a
/// new Bija provider under a unique id with the key stored in `api_key` — never in `env_key`, and
/// `Registry::save` writes it to owner-only secrets.toml. A draft with no usable URL is refused.
pub fn add_provider(reg: &mut Registry, draft: &ProviderDraft) -> Result<String, String> {
    if draft.needs_url() {
        return Err("give the API's base URL — this key's prefix names no provider".into());
    }
    let key = Some(draft.key.trim().to_string()).filter(|k| !k.is_empty());
    let wire = if draft.wire == WIRE_ANTHROPIC { WIRE_ANTHROPIC.to_string() } else { String::new() };
    let entry = ProviderEntry { id: String::new(), name: draft.name.clone(), base_url: draft.base_url.trim().to_string(), api_key: key.clone(), kind: ProviderKind::Bija, wire: wire.clone(), ..Default::default() };
    if let Some(why) = entry.url_problem() {
        return Err(why);
    }
    let existing = provider_at(reg, &entry.base_url).map(|p| p.id.clone());
    if let Some(p) = existing.as_ref().and_then(|id| reg.providers.iter_mut().find(|p| p.id == *id)) {
        p.wire = wire;
        if key.is_some() {
            p.api_key = key;
            p.env_key = String::new();
        }
        return Ok(p.id.clone());
    }
    let want = if draft.id.trim().is_empty() { id_from_host(&bare_host(&draft.name)) } else { draft.id.trim().to_string() };
    let id = unique_id(reg, &want);
    reg.providers.push(ProviderEntry { id: id.clone(), ..entry });
    Ok(id)
}

/// Lists a provider's models (`GET {base}/models`, the key presented the way its API wants it).
/// Blocks on the network: the UIs run it off their task. The errors a user can act on are said
/// plainly: a refused key, a URL with no model list (add models by hand), the network's own words.
pub fn probe(p: &ProviderEntry) -> Result<Vec<Found>, String> {
    if p.kind == ProviderKind::Antigravity {
        return Err("run `agy models`, then add an available model ID by hand".into());
    }
    if let Some(why) = p.url_problem().or_else(|| p.wire_problem()) {
        return Err(why);
    }
    crate::discover::list_models_with_headers(&p.base_url, p.resolve_key().as_deref(), crate::discover::key_header(p), &p.headers).map_err(|e| probe_error(&e))
}

fn probe_error(e: &str) -> String {
    if e.contains("HTTP 401") || e.contains("HTTP 403") {
        format!("the key was refused ({e})")
    } else if e.contains("HTTP 404") {
        "no model list at this URL — add models by hand".into()
    } else {
        e.to_string()
    }
}

/// The discovery picker's rows for what `probe` found on `provider`: each classified against the
/// registry (already there, or with something to fill in), the new ones pre-selected when there
/// are few enough to take all (`PRESELECT_UP_TO`).
pub fn candidates(reg: &Registry, provider: &str, found: &[Found]) -> Vec<Candidate> {
    let mut out: Vec<Candidate> = found.iter().map(|f| crate::discover::from_provider(provider, f)).collect();
    for c in &mut out {
        crate::discover::classify(reg, c);
    }
    let fresh = out.iter().filter(|c| c.state != CState::Configured).count();
    for c in &mut out {
        c.selected = c.state != CState::Configured && fresh <= PRESELECT_UP_TO;
    }
    out
}

/// `add_provider`, `probe` and `candidates` in one go, for a caller that may block. The provider
/// is added before the probe, so a failed probe still leaves it there to add models to by hand
/// (`provider_at` finds it); the caller imports the chosen candidates (`discover::apply`) and saves.
// `/models` takes the three steps apart — the probe runs off the event loop (`App::probe_provider`)
// — so only a caller that may block uses this one.
#[cfg_attr(not(test), allow(dead_code))]
pub fn probe_and_import(reg: &mut Registry, draft: &ProviderDraft) -> Result<Vec<Candidate>, String> {
    let id = add_provider(reg, draft)?;
    let entry = reg.providers.iter().find(|p| p.id == id).cloned().ok_or_else(|| format!("provider {id} vanished"))?;
    let found = probe(&entry)?;
    Ok(candidates(reg, &id, &found))
}

/// The two CLI backends. Codex models are added individually from its live catalogue or by id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    /// Codex's own account (ChatGPT login): models on provider `openai`.
    Codex,
    /// The `claude` CLI: the `claude` provider and its default models.
    ClaudeCode,
    /// Google subscription login held by `agy`. Models are entered from `agy models`.
    Antigravity,
}

/// Adds the Claude Code backend's known models once its CLI is on PATH. Codex has no built-in
/// model list: its models are chosen from `model/list` or entered by id in `/models`.
pub fn add_backend(reg: &mut Registry, backend: Backend) -> Result<usize, String> {
    let installed = match backend {
        Backend::Codex => crate::config::codex_available(),
        Backend::ClaudeCode => crate::config::claude_available(),
        Backend::Antigravity => crate::config::antigravity_available(),
    };
    add_backend_with(reg, backend, installed)
}

/// `add_backend` with the PATH check as a parameter (tests; `--demo`, whose backends are mocks).
/// A Claude model goes under its own alias when free, else `<alias>-claude`; one already on that
/// backend is not added twice. Codex's catalogue is deliberately never guessed here.
pub fn add_backend_with(reg: &mut Registry, backend: Backend, installed: bool) -> Result<usize, String> {
    let (models, tag) = match backend {
        Backend::Antigravity => {
            if !installed {
                return Err("`agy` is not installed — install the official Antigravity CLI, sign in, and run `agy models`".into());
            }
            if !reg.providers.iter().any(|p| p.kind == ProviderKind::Antigravity) {
                let id = unique_id(reg, "antigravity");
                reg.providers.push(ProviderEntry { id, name: "Antigravity".into(), kind: ProviderKind::Antigravity, auth: "subscription".into(), ..Default::default() });
            }
            // The subscription catalogue is account-dependent. Never invent model IDs.
            return Ok(0);
        }
        Backend::Codex => {
            if !installed {
                return Err("`codex` is not on PATH — install Codex and run `codex login`, then add it again".into());
            }
            return Err("Codex has no preset models — choose one from Discover or add its model id in /models".into());
        }
        Backend::ClaudeCode => {
            if !installed {
                return Err("`claude` is not on PATH — install Claude Code and log in once, then add it again".into());
            }
            let id = match reg.providers.iter().find(|p| p.kind == ProviderKind::ClaudeCode) {
                Some(p) => p.id.clone(),
                None => {
                    let id = unique_id(reg, "claude");
                    reg.providers.push(ProviderEntry { id: id.clone(), name: "Claude Code".into(), kind: ProviderKind::ClaudeCode, auth: "subscription".into(), ..Default::default() });
                    id
                }
            };
            (crate::config::claude_default_model_entries(&id), "claude")
        }
    };
    let mut added = 0;
    for m in models {
        let there = reg.models.iter().any(|x| x.model == m.model && (x.provider == m.provider || (m.provider == "openai" && !x.is_custom_provider())));
        if there {
            continue;
        }
        let alias = free_alias(reg, &m.alias, tag);
        reg.models.push(ModelEntry { alias, ..m });
        added += 1;
    }
    Ok(added)
}

/// `alias` when no model has it, else `alias-<tag>`, `alias-<tag>-2`…
fn free_alias(reg: &Registry, alias: &str, tag: &str) -> String {
    let taken = |a: &str| reg.models.iter().any(|m| m.alias == a);
    if !taken(alias) {
        return alias.to_string();
    }
    let base = format!("{alias}-{tag}");
    std::iter::once(base.clone()).chain((2..).map(|n| format!("{base}-{n}"))).find(|a| !taken(a)).unwrap_or(base)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ModelEntry;

    /// Every prefix of the table lands on its URL and wire; an unknown one guesses nothing, and a
    /// URL the caller gives always wins over the guess.
    #[test]
    fn the_key_prefix_picks_the_api() {
        for (key, url, wire, id) in [
            ("sk-ant-api03-x", "https://api.anthropic.com/v1", "anthropic", "anthropic"),
            ("sk-or-v1-x", "https://openrouter.ai/api/v1", "openai", "openrouter"),
            ("gsk_x", "https://api.groq.com/openai/v1", "openai", "groq"),
            ("xai-x", "https://api.x.ai/v1", "openai", "x"),
            ("AIzaSyx", "https://generativelanguage.googleapis.com/v1beta/openai", "openai", "googleapis"),
            ("csk-x", "https://api.cerebras.ai/v1", "openai", "cerebras"),
            ("fw_x", "https://api.fireworks.ai/inference/v1", "openai", "fireworks"),
            ("sk-proj-x", "https://api.openai.com/v1", "openai", "openai"),
            ("sk-x", "https://api.openai.com/v1", "openai", "openai"),
        ] {
            let d = provider_from_key(&format!("  {key}\n"), None);
            assert_eq!((d.base_url.as_str(), d.wire.as_str(), d.id.as_str(), d.guessed), (url, wire, id, true), "{key}");
            assert_eq!(d.key, key, "trimmed");
            assert!(!d.needs_url());
        }
        assert_eq!(provider_from_key("gsk_x", None).name, "api.groq.com");
        let unknown = provider_from_key("ip_test_x", None);
        assert!(unknown.needs_url() && !unknown.guessed && unknown.id.is_empty() && unknown.wire == "openai", "{unknown:?}");
        // the caller's URL wins; the wire still follows the key (or the host)
        let gw = provider_from_key("sk-x", Some("https://inference.example-gw.dev/v1/"));
        assert_eq!((gw.base_url.as_str(), gw.id.as_str(), gw.name.as_str(), gw.wire.as_str(), gw.guessed), ("https://inference.example-gw.dev/v1", "example-gw", "inference.example-gw.dev", "openai", false));
        assert_eq!(provider_from_key("sk-ant-x", Some("https://gw.corp.dev/anthropic")).wire, "anthropic");
        assert_eq!(provider_from_key("key", Some("https://api.anthropic.com/v1")).wire, "anthropic");
        assert_eq!(provider_from_key("sk-x", Some("  ")).base_url, "https://api.openai.com/v1", "a blank URL is no URL");
        assert!(!format!("{gw:?}").contains("sk-x"), "Debug never shows the key");
    }

    #[test]
    fn the_id_is_the_hosts_name_before_its_public_suffix() {
        for (host, id) in [("api.groq.com", "groq"), ("openrouter.ai", "openrouter"), ("api.x.ai", "x"), ("inference.riti.dev", "riti"), ("api.example.co.uk", "example"), ("example.com.au", "example"), ("my-box.lan", "my-box"), ("localhost", "local"), ("127.0.0.1", "local"), ("::1", "local"), ("Big_Host.io", "big-host")] {
            assert_eq!(id_from_host(host), id, "{host}");
        }
        assert_eq!(provider_from_key("k", Some("http://127.0.0.1:11434/v1")).id, "local");
        assert_eq!(provider_from_key("k", Some("http://[::1]:8000/v1")).name, "[::1]:8000");
    }

    /// A new provider gets a unique id (never `openai`, Codex's account), the key in `api_key`, the
    /// wire when it is Messages; a key for a URL a Bija provider already has goes into that one.
    #[test]
    fn adding_a_provider_is_unique_and_reuses_the_one_at_the_same_url() {
        let mut reg = Registry::defaults();
        let groq = provider_from_key("gsk_one", None);
        assert_eq!(add_provider(&mut reg, &groq).unwrap(), "groq");
        let p = reg.providers.iter().find(|p| p.id == "groq").unwrap();
        assert_eq!((p.kind, p.api_key.as_deref(), p.env_key.as_str(), p.name.as_str(), p.wire.as_str()), (ProviderKind::Bija, Some("gsk_one"), "", "api.groq.com", ""));
        // the same URL again: no second provider, the new key replaces the old
        assert_eq!(add_provider(&mut reg, &provider_from_key("gsk_two", Some("https://API.groq.com/openai/v1/"))).unwrap(), "groq");
        assert_eq!(reg.providers.iter().filter(|p| p.id.starts_with("groq")).count(), 1);
        assert_eq!(reg.providers.iter().find(|p| p.id == "groq").unwrap().api_key.as_deref(), Some("gsk_two"));
        // another URL on the same host's name is a second provider
        let other = provider_from_key("k", Some("https://eu.groq.com/v1"));
        assert_eq!(add_provider(&mut reg, &other).unwrap(), "groq-2");
        // No Bija provider is preconfigured. The Codex account reserves `openai` as an id.
        assert_eq!(add_provider(&mut reg, &provider_from_key("sk-proj-k", None)).unwrap(), "openai-2");
        let openai_api = reg.providers.iter().find(|p| p.id == "openai-2").unwrap();
        assert_eq!((openai_api.api_key.as_deref(), openai_api.env_key.as_str()), (Some("sk-proj-k"), ""));
        assert_eq!(add_provider(&mut reg, &provider_from_key("sk-proj-next", None)).unwrap(), "openai-2", "same endpoint updates the explicit provider");
        let mut bare = Registry { models: vec![], providers: vec![] };
        assert_eq!(add_provider(&mut bare, &provider_from_key("sk-k", None)).unwrap(), "openai-2");
        // Messages API: the wire is written; a URL that cannot be used is refused, nothing added
        let ant = add_provider(&mut bare, &provider_from_key("sk-ant-k", None)).unwrap();
        assert_eq!(bare.providers.iter().find(|p| p.id == ant).unwrap().wire, "anthropic");
        bare.providers.iter_mut().find(|p| p.id == ant).unwrap().wire.clear();
        assert_eq!(add_provider(&mut bare, &provider_from_key("sk-ant-next", None)).unwrap(), ant);
        assert_eq!(bare.providers.iter().find(|p| p.id == ant).unwrap().wire, "anthropic", "re-pasting a key repairs a previously wrong wire");
        let n = bare.providers.len();
        assert!(add_provider(&mut bare, &provider_from_key("ip_x", None)).unwrap_err().contains("base URL"));
        assert!(add_provider(&mut bare, &provider_from_key("ip_x", Some("https://example.com/v1"))).unwrap_err().contains("placeholder"));
        assert!(add_provider(&mut bare, &provider_from_key("ip_x", Some("not a url"))).unwrap_err().contains("not a URL"));
        assert_eq!(bare.providers.len(), n);
        // a local server takes no key at all
        let local = add_provider(&mut bare, &provider_from_key("", Some("http://127.0.0.1:11434/v1"))).unwrap();
        assert_eq!((local.as_str(), bare.providers.last().unwrap().api_key.clone()), ("local", None));
    }

    /// A probe's failures, in words the user can act on.
    #[test]
    fn probe_errors_say_what_to_do() {
        assert!(probe_error("HTTP 401 from https://x/v1/models: invalid key").starts_with("the key was refused"));
        assert!(probe_error("HTTP 403 from https://x/v1/models: no").starts_with("the key was refused"));
        assert_eq!(probe_error("HTTP 404 from https://x/v1/models: not found"), "no model list at this URL — add models by hand");
        assert_eq!(probe_error("couldn't reach https://x: timeout"), "couldn't reach https://x: timeout");
        let draft = ProviderEntry { id: "d".into(), kind: ProviderKind::Bija, base_url: "https://example.com/v1".into(), ..Default::default() };
        assert!(probe(&draft).unwrap_err().contains("placeholder"), "a draft never goes on the network");
    }

    /// The picker's rows: new models pre-selected when there are a dozen or fewer, none when more;
    /// one already configured is never selected.
    #[test]
    fn candidates_preselect_a_short_list_only() {
        let mut reg = Registry::defaults();
        reg.models.push(ModelEntry { alias: "sol".into(), provider: "bija".into(), model: "gpt-5.6-sol".into(), context_window: Some(272_000), vision: true, ..Default::default() });
        let found = |n: usize| (0..n).map(|i| Found { id: format!("m{i}"), context: None, reasoning: None, vision: false }).collect::<Vec<_>>();
        assert!(candidates(&reg, "groq", &found(12)).iter().all(|c| c.selected && c.state == CState::New));
        assert!(candidates(&reg, "groq", &found(13)).iter().all(|c| !c.selected));
        let mut known = found(2);
        known.push(Found { id: "gpt-5.6-sol".into(), context: Some(272_000), reasoning: None, vision: true });
        let c = candidates(&reg, "bija", &known);
        let sol = c.iter().find(|c| c.model == "gpt-5.6-sol").unwrap();
        assert_eq!((sol.state, sol.selected), (CState::Configured, false));
        assert!(c.iter().filter(|c| c.model != "gpt-5.6-sol").all(|c| c.selected));
    }

    /// Codex never injects a built-in catalogue; Claude Code adds its known models once.
    #[test]
    fn add_backend_is_refused_without_the_cli_and_idempotent_with_it() {
        let mut reg = Registry::defaults();
        assert!(add_backend_with(&mut reg, Backend::Codex, false).unwrap_err().contains("codex"));
        assert!(add_backend_with(&mut reg, Backend::ClaudeCode, false).unwrap_err().contains("claude"));
        assert!(reg.models.is_empty(), "nothing added without the CLI");
        assert!(add_backend_with(&mut reg, Backend::Codex, true).unwrap_err().contains("no preset models"));
        reg.add_codex_model("personal", "a-model-in-the-account").unwrap();
        assert_eq!(reg.codex_account_models(), vec!["personal"]);
        assert_eq!(reg.backend_of(reg.get("personal").unwrap()), ProviderKind::Codex);
        assert_eq!(reg.remove_codex_account(), 1);
        // Claude Code: the provider and its models, once
        let n = add_backend_with(&mut reg, Backend::ClaudeCode, true).unwrap();
        assert_eq!(n, crate::config::claude_default_model_entries("claude").len());
        let p = reg.providers.iter().find(|p| p.kind == ProviderKind::ClaudeCode).unwrap();
        assert_eq!((p.id.as_str(), p.auth.as_str()), ("claude", "subscription"));
        assert_eq!(reg.backend_of(&reg.resolve("sonnet5")), ProviderKind::ClaudeCode);
        assert_eq!(add_backend_with(&mut reg, Backend::ClaudeCode, true), Ok(0));
        assert_eq!(reg.providers.iter().filter(|p| p.kind == ProviderKind::ClaudeCode).count(), 1);
        // an alias of the user's own under a Claude name pushes that one model aside
        let mut taken = Registry { models: vec![ModelEntry { alias: "opus5".into(), provider: "zai".into(), model: "x".into(), ..Default::default() }], providers: vec![] };
        add_backend_with(&mut taken, Backend::ClaudeCode, true).unwrap();
        assert_eq!(taken.get("opus5-claude").map(|m| m.model.as_str()), Some("claude-opus-5"));
        assert_eq!(taken.get("opus5").map(|m| m.model.as_str()), Some("x"));
    }

    /// Live, by hand only — the whole flow against a real provider, nothing sent unless both are
    /// set: `MANTRA_LIVE_KEY=… MANTRA_LIVE_URL=https://…/v1 cargo test -p mantra live_key -- --ignored`
    /// (leave the URL out for a key whose prefix names its API).
    #[test]
    #[ignore]
    fn live_key_flow() {
        let Ok(key) = std::env::var("MANTRA_LIVE_KEY") else { return };
        let url = std::env::var("MANTRA_LIVE_URL").ok();
        let mut reg = Registry::defaults();
        let draft = provider_from_key(&key, url.as_deref());
        let cands = probe_and_import(&mut reg, &draft).unwrap_or_else(|e| panic!("{}: {e}", draft.name));
        assert!(!cands.is_empty(), "{} listed no models", draft.name);
        let p = provider_at(&reg, &draft.base_url).expect("the provider was added");
        eprintln!("{} ({}): {} models, e.g. {:?}", p.name, p.id, cands.len(), cands.iter().take(4).map(|c| c.model.as_str()).collect::<Vec<_>>());
    }

    /// The whole flow against a local stand-in for a provider: the key is sent, the list comes
    /// back as rows, and a refused key still leaves the provider for models by hand.
    #[tokio::test]
    async fn probe_and_import_lists_a_providers_models_and_keeps_it_when_refused() {
        use axum::routing::get;
        let app = axum::Router::new().route(
            "/v1/models",
            get(|h: axum::http::HeaderMap| async move {
                let auth = h.get("authorization").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
                if auth == "Bearer good-key" {
                    (axum::http::StatusCode::OK, r#"{"data":[{"id":"alpha","context_length":64000},{"id":"beta"}]}"#)
                } else {
                    (axum::http::StatusCode::UNAUTHORIZED, r#"{"error":{"message":"bad key"}}"#)
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move { axum::serve(listener, app).await.ok() });
        let url = format!("http://127.0.0.1:{port}/v1");
        let (ok, refused) = tokio::task::spawn_blocking(move || {
            let mut reg = Registry::defaults();
            let ok = probe_and_import(&mut reg, &provider_from_key("good-key", Some(&url))).map(|c| (c, reg.clone()));
            let mut reg2 = Registry::defaults();
            let refused = probe_and_import(&mut reg2, &provider_from_key("bad-key", Some(&url))).map(|_| ()).map_err(|e| (e, provider_at(&reg2, &url).map(|p| p.id.clone())));
            (ok, refused)
        })
        .await
        .unwrap();
        let (cands, reg) = ok.expect("the list");
        assert_eq!(cands.iter().map(|c| (c.model.as_str(), c.provider.as_str(), c.context, c.selected)).collect::<Vec<_>>(), vec![("alpha", "local", Some(64_000), true), ("beta", "local", Some(crate::discover::ASSUMED_CONTEXT), true)]);
        let mut reg = reg;
        assert_eq!(crate::discover::apply(&mut reg, &cands), (2, 0));
        assert_eq!(reg.backend_of(&reg.resolve("alpha")), ProviderKind::Bija);
        let (why, kept) = refused.unwrap_err();
        assert!(why.starts_with("the key was refused") && why.contains("bad key"), "{why}");
        assert_eq!(kept.as_deref(), Some("local"), "the provider stays for models by hand");
    }
    #[test]
    fn adding_antigravity_preserves_the_account_dependent_catalogue() {
        let mut reg = Registry { models: vec![], providers: vec![] };
        assert!(add_backend_with(&mut reg, Backend::Antigravity, false).unwrap_err().contains("agy"));
        assert!(reg.providers.is_empty());
        assert_eq!(add_backend_with(&mut reg, Backend::Antigravity, true), Ok(0));
        assert_eq!(add_backend_with(&mut reg, Backend::Antigravity, true), Ok(0));
        assert_eq!(reg.providers.len(), 1);
        assert_eq!(reg.providers[0].kind, ProviderKind::Antigravity);
        assert!(reg.models.is_empty(), "only actual model IDs from the user's account may be added");
        assert!(probe(&reg.providers[0]).unwrap_err().contains("agy models"));
    }

}
