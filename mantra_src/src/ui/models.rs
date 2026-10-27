//! `/models`: the registry as two grids — models and providers — edited in place (`ui::form`):
//! type into a cell and ⏎ saves models.toml. ctrl+n adds an API-key provider, a model, a Codex
//! account model by ID, or a Claude Code subscription; Delete twice removes, ctrl+t tests a model,
//! ctrl+d discovers provider models.

use super::{theme, *};
use crate::app::{App, Choice, EditTarget, KeyFlow, Overlay, Select, SelectThen, ADD_CLAUDE, ADD_CLAUDE_KEY, ADD_CODEX, ADD_KEY, ADD_MODEL};
use crate::config::{ModelEntry, ProviderEntry, ProviderKind, Registry};
use crate::ui::form::{self, Edit, Outcome, Start};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Constraint, Direction, Layout};

const MODEL_COLS: &[&str] = &["alias", "provider", "model", "context", "compact", "effort", "efforts", "vision", "note / test"];
/// The models grid's column widths, `MODEL_COLS` order (an 11-character alias, `astra-codex`, fits).
const MODEL_WIDTHS: [usize; 9] = [12, 9, 18, 14, 14, 8, 30, 6, 30];
const ALIAS_COL: usize = 0;
const PROVIDER_COL: usize = 1;
const MODEL_COL: usize = 2;
pub(crate) const CONTEXT_COL: usize = 3;
const COMPACT_COL: usize = 4;
const EFFORT_COL: usize = 5;
const EFFORTS_COL: usize = 6;
/// Where `vision` sits in `MODEL_COLS`: space, ⏎ or +/- flips it.
pub(crate) const VISION_COL: usize = 7;
/// Where the note (or the last ctrl+t result) sits in `MODEL_COLS`.
pub(crate) const NOTE_COL: usize = 8;

const PROV_COLS: &[&str] = &["id", "name", "base_url", "env_key", "api_key", "kind", "auth", "wire"];
/// The providers grid's column widths, `PROV_COLS` order.
const PROV_WIDTHS: [usize; 8] = [10, 12, 26, 14, 8, 10, 12, 9];
const PROV_ID_COL: usize = 0;
const PROV_NAME_COL: usize = 1;
const PROV_URL_COL: usize = 2;
const PROV_ENV_COL: usize = 3;
pub(crate) const PROV_KEY_COL: usize = 4;
/// Where `kind`, `auth` and `wire` sit in `PROV_COLS` — the columns +/- cycles.
pub(crate) const PROV_KIND_COL: usize = 5;
pub(crate) const PROV_AUTH_COL: usize = 6;
pub(crate) const PROV_WIRE_COL: usize = 7;
/// Display strings for `ProviderEntry.kind` — cycled with +/- like any choice. Kept here rather
/// than a `Display` impl on `ProviderKind` since it's presentation-only.
const PROV_KINDS: &[&str] = &["Codex", "ClaudeCode", "Bija"];
/// `ProviderEntry.auth` (ClaudeCode only, but editable on any row so a provider can be flipped to
/// `ClaudeCode` and given `auth` in one place).
const PROV_AUTHS: &[&str] = &["subscription", "api_key"];
/// `ProviderEntry.wire` (Bija only, editable on any row for the same reason).
const PROV_WIRES: &[&str] = &["openai", "anthropic"];
/// The context windows +/- steps through.
const CONTEXT_STEPS: &[u64] = &[128_000, 200_000, 272_000, 400_000, 1_000_000];

/// How a cell takes keys (`ui::form`'s rules).
#[derive(Clone, Copy, PartialEq, Debug)]
enum Kind {
    /// Typed: a printable key overwrites, ⏎ edits, Backspace clears.
    Text,
    /// Typed and drawn masked (an API key): ⏎ starts from nothing — a key is pasted whole.
    Secret,
    /// Typed (a number; `400k` and `1m` too), or stepped with +/-.
    Step,
    /// One of a list: +/-, space and ⏎ step through it, a letter jumps.
    Choice,
    /// Yes or no: space, ⏎ and +/- flip it.
    Toggle,
    /// Shown, not edited: the Codex account's row.
    Fixed,
}

fn model_kind(col: usize) -> Kind {
    match col {
        ALIAS_COL | MODEL_COL | EFFORTS_COL | NOTE_COL => Kind::Text,
        CONTEXT_COL | COMPACT_COL => Kind::Step,
        PROVIDER_COL | EFFORT_COL => Kind::Choice,
        VISION_COL => Kind::Toggle,
        _ => Kind::Fixed,
    }
}

fn prov_kind(row: ProvRow, col: usize) -> Kind {
    match (row, col) {
        (ProvRow::Codex, _) => Kind::Fixed,
        (ProvRow::Entry(_), PROV_KIND_COL) => Kind::Choice,
        (_, PROV_KEY_COL) => Kind::Secret,
        (_, PROV_AUTH_COL | PROV_WIRE_COL) => Kind::Choice,
        _ => Kind::Text,
    }
}

/// A row of the providers grid: Codex's own account — shown while a model is on it, since it has
/// no `[[provider]]` entry of its own — or a configured provider by its index in `providers`.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) enum ProvRow {
    Codex,
    Entry(usize),
}

pub(crate) fn prov_rows(reg: &Registry) -> Vec<ProvRow> {
    let mut v = vec![];
    if !reg.codex_account_models().is_empty() {
        v.push(ProvRow::Codex);
    }
    v.extend((0..reg.providers.len()).map(ProvRow::Entry));
    v
}

/// The grid row a provider is drawn on.
fn prov_row_of(reg: &Registry, id: &str) -> Option<usize> {
    prov_rows(reg).iter().position(|r| matches!(r, ProvRow::Entry(i) if reg.providers[*i].id == id))
}

/// Which rows and columns the grid on view has.
fn dims(app: &App) -> (usize, usize) {
    if app.models_ui.providers {
        (prov_rows(&app.registry).len(), PROV_COLS.len())
    } else {
        (app.registry.models.len(), MODEL_COLS.len())
    }
}

/// The kind of the selected cell.
fn sel_kind(app: &App) -> Kind {
    let ui = &app.models_ui;
    if ui.providers {
        prov_rows(&app.registry).get(ui.row).map(|r| prov_kind(*r, ui.col)).unwrap_or(Kind::Fixed)
    } else {
        model_kind(ui.col)
    }
}

fn kind_str(k: ProviderKind) -> &'static str {
    match k {
        ProviderKind::Codex => "Codex",
        ProviderKind::ClaudeCode => "ClaudeCode",
        ProviderKind::Bija => "Bija",
    }
}

fn parse_kind(t: &str) -> ProviderKind {
    let t = t.trim().to_ascii_lowercase().replace(['_', '-', ' '], "");
    match t.as_str() {
        "claudecode" | "claude" => ProviderKind::ClaudeCode,
        "bija" | "seed" => ProviderKind::Bija,
        _ => ProviderKind::Codex,
    }
}

/// What a Bija provider's `wire` cell shows: an empty wire is Chat Completions. Other kinds have
/// no wire to choose.
fn wire_str(p: &ProviderEntry) -> String {
    match p.kind {
        ProviderKind::Bija if p.wire.trim().is_empty() => "openai".into(),
        ProviderKind::Bija => p.wire.clone(),
        _ => "—".into(),
    }
}

/// A typed `wire`: Chat Completions is the default and is stored as nothing at all; anything
/// unrecognised is kept as typed, so the row reads as a draft that names the typo
/// (`ProviderEntry::wire_problem`) instead of silently becoming Chat Completions.
fn parse_wire(t: &str) -> String {
    let t = t.trim().to_ascii_lowercase();
    match t.as_str() {
        "" | "openai" | "openai-chat" | "openai_chat" | "chat" => String::new(),
        "anthropic" | "messages" => "anthropic".into(),
        _ => t,
    }
}

fn parse_auth(t: &str) -> String {
    let t = t.trim().to_ascii_lowercase().replace(['-', ' '], "_");
    if t == "api_key" || t == "apikey" || t == "key" {
        "api_key".into()
    } else {
        "subscription".into()
    }
}

/// A token count as typed: `200000`, `200k`, `1.5m`; nothing / `none` / `default` is none.
pub(crate) fn parse_tokens(t: &str) -> Option<u64> {
    let t = t.trim().to_lowercase().replace([',', '_'], "");
    if t.is_empty() || t == "none" || t == "default" {
        return None;
    }
    if let Some(k) = t.strip_suffix('k') {
        return k.parse::<f64>().ok().map(|v| (v * 1000.0) as u64);
    }
    if let Some(m) = t.strip_suffix('m') {
        return m.parse::<f64>().ok().map(|v| (v * 1_000_000.0) as u64);
    }
    t.parse().ok()
}

/// A model cell's value as an edit starts from it.
fn model_value(m: &ModelEntry, col: usize) -> String {
    match col {
        ALIAS_COL => m.alias.clone(),
        PROVIDER_COL => m.provider.clone(),
        MODEL_COL => m.model.clone(),
        CONTEXT_COL => m.context_window.map(|c| c.to_string()).unwrap_or_default(),
        COMPACT_COL => m.auto_compact_percent.map(|c| c.to_string()).unwrap_or_default(),
        EFFORT_COL => m.default_effort.clone(),
        EFFORTS_COL => m.efforts().join(", "),
        VISION_COL => if m.vision { "yes" } else { "no" }.into(),
        _ => m.note.clone(),
    }
}

fn prov_value(p: &ProviderEntry, col: usize) -> String {
    match col {
        PROV_ID_COL => p.id.clone(),
        PROV_NAME_COL => p.name.clone(),
        PROV_URL_COL => p.base_url.clone(),
        PROV_ENV_COL => p.env_key.clone(),
        PROV_KEY_COL => p.api_key.clone().unwrap_or_default(),
        PROV_KIND_COL => kind_str(p.kind).into(),
        PROV_AUTH_COL => p.auth.clone(),
        _ => wire_str(p),
    }
}

/// A typed model cell into the registry, or why not (nothing changes then).
fn set_model_cell(reg: &mut Registry, row: usize, col: usize, text: &str) -> Result<(), String> {
    let t = text.trim();
    if col == ALIAS_COL {
        let alias = crate::util::slug(t);
        if alias.is_empty() {
            return Err("an alias needs a name".into());
        }
        if reg.models.iter().enumerate().any(|(i, m)| i != row && m.alias == alias) {
            return Err(format!("{alias} is taken — an alias names one model"));
        }
    }
    let m = reg.models.get_mut(row).ok_or("that model is gone")?;
    if col == MODEL_COL && m.provider == "openai" && t.is_empty() {
        return Err("a Codex model needs a model ID".into());
    }
    match col {
        ALIAS_COL => m.alias = crate::util::slug(t),
        PROVIDER_COL => {
            if t == "openai" && m.model.is_empty() {
                return Err("a Codex model needs a model ID".into());
            }
            m.provider = t.to_string();
        }
        MODEL_COL => {
            let was_empty = m.model.is_empty();
            m.model = t.to_string();
            if m.provider == "openai" && was_empty && !t.is_empty() {
                m.vision = crate::config::takes_images(t);
            }
        }
        CONTEXT_COL => {
            let n = parse_tokens(t);
            if n.is_none() && !t.is_empty() && !["none", "default"].contains(&t.to_ascii_lowercase().as_str()) {
                return Err("context: a number of tokens — 200000, 400k or 1m (empty: inherit the default model window)".into());
            }
            m.context_window = n;
        }
        COMPACT_COL => {
            m.auto_compact_percent = match t.trim_end_matches('%').trim() {
                "" => None,
                n => Some(n.parse::<u8>().ok().filter(|p| (10..=99).contains(p)).ok_or("compact: a percent from 10 to 99 (empty: the default 85%)")?),
            };
        }
        EFFORTS_COL => m.efforts = t.split([',', ' ']).map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect(),
        VISION_COL => m.vision = matches!(t.to_ascii_lowercase().as_str(), "y" | "yes" | "on" | "true" | "1" | "◉"),
        NOTE_COL => m.note = t.to_string(),
        _ => {}
    }
    Ok(())
}

/// A typed provider cell into the registry, or why not. A new id carries the provider's models
/// with it; `openai` stays Codex's own account.
fn set_prov_cell(reg: &mut Registry, idx: usize, col: usize, text: &str) -> Result<(), String> {
    let t = text.trim().to_string();
    if idx >= reg.providers.len() {
        return Err("that provider is gone".into());
    }
    if col == PROV_ID_COL {
        let id = crate::util::slug(&t).replace('-', "_");
        if id.is_empty() {
            return Err("a provider needs an id".into());
        }
        if id == "openai" {
            return Err("openai is Codex's own account — pick another id".into());
        }
        if reg.providers.iter().enumerate().any(|(i, p)| i != idx && p.id == id) {
            return Err(format!("{id} is taken — an id names one provider"));
        }
        let old = std::mem::replace(&mut reg.providers[idx].id, id.clone());
        for m in reg.models.iter_mut().filter(|m| m.provider == old) {
            m.provider = id.clone();
        }
        return Ok(());
    }
    let p = &mut reg.providers[idx];
    match col {
        PROV_NAME_COL => p.name = t,
        PROV_URL_COL => p.base_url = t,
        PROV_ENV_COL => p.env_key = t,
        PROV_KEY_COL => p.api_key = if t.is_empty() { None } else { Some(t) },
        PROV_KIND_COL => {
            let normalized = t.to_ascii_lowercase().replace(['_', '-', ' '], "");
            if !matches!(normalized.as_str(), "codex" | "claude" | "claudecode" | "bija" | "seed") {
                return Err("provider kind: Codex, ClaudeCode or Bija".into());
            }
            p.kind = parse_kind(&t)
        }
        PROV_AUTH_COL => p.auth = parse_auth(&t),
        PROV_WIRE_COL => p.wire = parse_wire(&t),
        _ => {}
    }
    Ok(())
}

/// The provider ids a model can be put on: Codex's account while it has models (or this one is
/// on it), then every configured provider.
fn provider_choices(reg: &Registry) -> Vec<String> {
    let mut v = vec!["openai".to_string()];
    v.extend(reg.providers.iter().map(|p| p.id.clone()));
    v
}

/// +/- (`d`), space or ⏎ on the selected cell: the next choice, the next step, the flipped flag.
/// Returns whether anything changed.
fn step(app: &mut App, d: i32) -> bool {
    let (row, col) = (app.models_ui.row, app.models_ui.col);
    if app.models_ui.providers {
        let Some(ProvRow::Entry(i)) = prov_rows(&app.registry).get(row).copied() else { return false };
        let p = &mut app.registry.providers[i];
        match col {
            PROV_KIND_COL => p.kind = parse_kind(&form::cycle(PROV_KINDS, kind_str(p.kind), d)),
            PROV_AUTH_COL => p.auth = form::cycle(PROV_AUTHS, &p.auth, d),
            PROV_WIRE_COL => {
                let cur = if p.wire.trim().is_empty() { "openai".to_string() } else { p.wire.clone() };
                p.wire = parse_wire(&form::cycle(PROV_WIRES, &cur, d));
            }
            _ => return false,
        }
        return true;
    }
    let providers = app.registry.models.get(row).map(|_| provider_choices(&app.registry)).unwrap_or_default();
    let Some(m) = app.registry.models.get_mut(row) else { return false };
    match col {
        PROVIDER_COL => m.provider = form::cycle(&providers, &m.provider, d),
        CONTEXT_COL => {
            let cur = m.context_window.unwrap_or(0);
            let i = CONTEXT_STEPS.iter().position(|x| *x >= cur).unwrap_or(CONTEXT_STEPS.len()) as i32;
            m.context_window = Some(CONTEXT_STEPS[(i + d).clamp(0, CONTEXT_STEPS.len() as i32 - 1) as usize]);
        }
        COMPACT_COL => m.auto_compact_percent = Some((m.auto_compact_percent.unwrap_or(85) as i32 + 5 * d).clamp(10, 99) as u8),
        EFFORT_COL if m.efforts().is_empty() => {
            let alias = m.alias.clone();
            app.toast(format!("{alias} has no efforts to pick from — list them in its efforts cell"), crate::agent::Level::Info);
            return false;
        }
        EFFORT_COL => m.default_effort = m.step_effort(&m.default_effort.clone(), d),
        VISION_COL => m.vision = !m.vision,
        _ => return false,
    }
    true
}

/// A letter on a choice cell: the first option starting with it.
fn jump(app: &mut App, c: char) -> bool {
    let (row, col) = (app.models_ui.row, app.models_ui.col);
    if app.models_ui.providers {
        let Some(ProvRow::Entry(i)) = prov_rows(&app.registry).get(row).copied() else { return false };
        let p = &mut app.registry.providers[i];
        let (list, cur): (&[&str], String) = match col {
            PROV_KIND_COL => (PROV_KINDS, kind_str(p.kind).into()),
            PROV_AUTH_COL => (PROV_AUTHS, p.auth.clone()),
            PROV_WIRE_COL => (PROV_WIRES, wire_str(p)),
            _ => return false,
        };
        let Some(v) = form::jump(list, &cur, c) else { return false };
        match col {
            PROV_KIND_COL => p.kind = parse_kind(&v),
            PROV_AUTH_COL => p.auth = v,
            _ => p.wire = parse_wire(&v),
        }
        return true;
    }
    let providers = app.registry.models.get(row).map(|_| provider_choices(&app.registry)).unwrap_or_default();
    let Some(m) = app.registry.models.get_mut(row) else { return false };
    let (list, cur) = match col {
        PROVIDER_COL => (providers, m.provider.clone()),
        EFFORT_COL => (m.efforts(), m.default_effort.clone()),
        _ => return false,
    };
    let Some(v) = form::jump(&list, &cur, c) else { return false };
    if col == PROVIDER_COL {
        m.provider = v;
    } else {
        m.default_effort = v;
    }
    true
}

/// models.toml written after a change, or the reason it was not, said.
fn save(app: &mut App) {
    if let Err(e) = app.save_registry() {
        app.toast(e, crate::agent::Level::Error);
    } else if app.registry.alias_problem(&app.settings.default_model).is_some() {
        if let Some(alias) = app.registry.models.iter().find(|m| app.demo || app.registry.alias_problem(&m.alias).is_none()).map(|m| m.alias.clone()) {
            app.settings.default_model = alias;
            app.save_settings();
        }
    }
}

/// Starts editing the selected cell the way `start` says (a typed `c` for `Overwrite`).
fn begin(app: &mut App, start: Start, c: Option<char>) {
    let (row, col) = (app.models_ui.row, app.models_ui.col);
    let (target, value, kind) = if app.models_ui.providers {
        let Some(ProvRow::Entry(i)) = prov_rows(&app.registry).get(row).copied() else { return };
        (EditTarget::ProviderCell(i, col), prov_value(&app.registry.providers[i], col), prov_kind(ProvRow::Entry(i), col))
    } else {
        let Some(m) = app.registry.models.get(row) else { return };
        (EditTarget::ModelCell(row, col), model_value(m, col), model_kind(col))
    };
    // a key is pasted whole: ⏎ on it starts from nothing rather than around the old one
    let start = if kind == Kind::Secret && start == Start::Append { Start::Clear } else { start };
    let mut e = match (start, c) {
        (Start::Overwrite, Some(c)) => Edit::overwrite(target, &value, c),
        (s, _) => Edit::new(target, &value, s),
    };
    if kind == Kind::Secret {
        e = e.secret();
    }
    app.edit = Some(e);
}

/// The models grid at `row`, its `col` cell being edited — what the model picker's `c` (edit the
/// context window) opens.
pub fn edit_model_cell(app: &mut App, row: usize, col: usize) {
    app.models_ui.providers = false;
    app.models_ui.row = row.min(app.registry.models.len().saturating_sub(1));
    app.models_ui.col = col.min(MODEL_COLS.len() - 1);
    begin(app, Start::Append, None);
}

/// The add menu's rows (`SelectThen::AddMenu`).
pub fn add_menu(app: &App) -> Select {
    let codex = if app.demo || crate::config::codex_available() { "" } else { " · codex is not on PATH" };
    let claude = if app.demo || crate::config::claude_available() { "" } else { " · claude is not on PATH" };
    let row = |key: &str, label: &str, detail: String| Choice { key: key.into(), label: label.into(), detail, on: false };
    Select {
        title: "add".into(),
        items: vec![
            row(ADD_KEY, "API key provider", "paste a key and API URL; Mantra can discover its models".into()),
            row(ADD_MODEL, "Add a model", "enter an alias, provider and model ID".into()),
            row(ADD_CODEX, "Codex account model", format!("add a model ID for your ChatGPT login{codex}")),
            row(ADD_CLAUDE, "Claude Code subscription", format!("connect its subscription login{claude}")),
            row(ADD_CLAUDE_KEY, "Claude Code API key", format!("paste an API key or use $ANTHROPIC_API_KEY{claude}")),
        ],
        sel: 0,
        multi: false,
        then: SelectThen::AddMenu,
    }
}

/// What the add menu's row `key` does.
pub fn add_chosen(app: &mut App, key: &str) {
    use crate::provider_key::Backend;
    match key {
        ADD_KEY => {
            let items = [
                ("openai", "OpenAI API", "https://api.openai.com/v1"),
                ("anthropic", "Anthropic API", "https://api.anthropic.com/v1"),
                ("openrouter", "OpenRouter", "https://openrouter.ai/api/v1"),
                ("groq", "Groq", "https://api.groq.com/openai/v1"),
                ("xai", "xAI", "https://api.x.ai/v1"),
                ("gemini", "Gemini API", "https://generativelanguage.googleapis.com/v1beta/openai"),
                ("cerebras", "Cerebras", "https://api.cerebras.ai/v1"),
                ("fireworks", "Fireworks", "https://api.fireworks.ai/inference/v1"),
                ("custom", "Local / custom API", "enter its base URL; a local server can use no key"),
            ].into_iter().map(|(key, label, detail)| Choice { key: key.into(), label: label.into(), detail: detail.into(), on: false }).collect();
            app.overlays.push(Overlay::Select(Select { title: "choose an API provider".into(), items, sel: 0, multi: false, then: SelectThen::ProviderPreset }));
        }
        ADD_MODEL => {
            let mut items: Vec<Choice> = vec![];
            items.push(Choice { key: "openai".into(), label: "openai".into(), detail: "Codex account (ChatGPT login)".into(), on: false });
            items.extend(app.registry.providers.iter().map(|p| Choice { key: p.id.clone(), label: p.id.clone(), detail: app.registry.provider_name(&p.id), on: false }));
            match items.len() {
                0 => app.toast("add a provider first — from its API key", crate::agent::Level::Info),
                1 => add_model_on(app, &items[0].key),
                _ => app.overlays.push(Overlay::Select(Select { title: "a model on which provider".into(), items, sel: 0, multi: false, then: SelectThen::AddModelOn })),
            }
        }
        ADD_CODEX | ADD_CLAUDE | ADD_CLAUDE_KEY => {
            if key == ADD_CODEX {
                if !app.demo && !crate::config::codex_available() {
                    app.toast("codex is not on PATH", crate::agent::Level::Warn);
                } else {
                    add_model_on(app, "openai");
                }
            } else {
                match app.add_backend(Backend::ClaudeCode) {
                    Ok(n) if key == ADD_CLAUDE_KEY => {
                        if let Some((idx, p)) = app.registry.providers.iter_mut().enumerate().find(|(_, p)| p.kind == ProviderKind::ClaudeCode) {
                            p.auth = "api_key".into();
                            p.env_key = "ANTHROPIC_API_KEY".into();
                            save(app);
                            app.models_ui.providers = true;
                            app.models_ui.row = prov_row_of(&app.registry, &app.registry.providers[idx].id).unwrap_or(0);
                            app.models_ui.col = PROV_KEY_COL;
                            app.edit = Some(Edit::new(EditTarget::ProviderCell(idx, PROV_KEY_COL), "", Start::Clear).secret());
                            app.toast(format!("Claude Code: {n} model(s). Paste an API key, or press esc to use $ANTHROPIC_API_KEY"), crate::agent::Level::Info);
                        }
                    }
                    Ok(n) => {
                        if let Some(p) = app.registry.providers.iter_mut().find(|p| p.kind == ProviderKind::ClaudeCode) {
                            p.auth = "subscription".into();
                            save(app);
                        }
                        app.toast(format!("Claude Code subscription: {n} model{} added", if n == 1 { "" } else { "s" }), crate::agent::Level::Info);
                    }
                    Err(e) => app.toast(e, crate::agent::Level::Warn),
                }
            }
        }
        _ => {}
    }
}

/// A choice prefills only the next form; nothing is written until the URL is confirmed.
pub fn provider_preset_chosen(app: &mut App, key: &str) {
    let url = match key {
        "openai" => "https://api.openai.com/v1",
        "anthropic" => "https://api.anthropic.com/v1",
        "openrouter" => "https://openrouter.ai/api/v1",
        "groq" => "https://api.groq.com/openai/v1",
        "xai" => "https://api.x.ai/v1",
        "gemini" => "https://generativelanguage.googleapis.com/v1beta/openai",
        "cerebras" => "https://api.cerebras.ai/v1",
        "fireworks" => "https://api.fireworks.ai/inference/v1",
        "custom" => "",
        _ => return,
    };
    app.models_ui.providers = true;
    app.models_ui.keyflow = Some(KeyFlow { guess: url.into(), ..Default::default() });
    app.edit = Some(Edit::new(EditTarget::KeyFlowKey, "", Start::Clear).secret());
}

/// A new model row on `provider`, saved, its model id cell open for typing.
pub fn add_model_on(app: &mut App, provider: &str) {
    let n = app.registry.models.len();
    let alias = (n + 1..).map(|i| format!("model{i}")).find(|a| app.registry.get(a).is_none()).unwrap_or_default();
    app.registry.models.push(ModelEntry { alias, provider: provider.to_string(), ..Default::default() });
    if provider != "openai" {
        save(app);
    }
    edit_model_cell(app, n, MODEL_COL);
    app.edit = Some(Edit::new(EditTarget::ModelCell(n, MODEL_COL), "", Start::Clear));
}

/// A key typed while a field is being edited.
fn edit_key(app: &mut App, k: KeyEvent) {
    let Some(e) = app.edit.as_mut() else { return };
    let outcome = e.key(k);
    let target = e.target.clone();
    match outcome {
        Outcome::Editing => {}
        Outcome::Revert => {
            app.edit = None;
            if let EditTarget::ModelCell(row, MODEL_COL) = &target {
                if app.registry.models.get(*row).is_some_and(|m| m.provider == "openai" && m.model.is_empty()) {
                    app.registry.models.remove(*row);
                    fix_rows(app);
                }
            }
            if matches!(target, EditTarget::KeyFlowKey | EditTarget::KeyFlowUrl) {
                app.models_ui.keyflow = None;
            }
        }
        Outcome::Commit | Outcome::CommitMove(_) if matches!(target, EditTarget::KeyFlowKey | EditTarget::KeyFlowUrl) => {
            // ↑↓ do not leave the two-field row: it is one thing being added
            if outcome == Outcome::Commit {
                keyflow_next(app);
            }
        }
        Outcome::Commit | Outcome::CommitMove(_) => {
            let Some(e) = app.edit.take() else { return };
            let incomplete_codex_model = matches!(&e.target, EditTarget::ModelCell(row, MODEL_COL)
                if app.registry.models.get(*row).is_some_and(|m| m.provider == "openai" && m.model.is_empty()));
            if e.changed() || incomplete_codex_model {
                let text = e.text();
                let done = match &e.target {
                    EditTarget::ModelCell(row, col) => set_model_cell(&mut app.registry, *row, *col, &text),
                    EditTarget::ProviderCell(i, col) => set_prov_cell(&mut app.registry, *i, *col, &text),
                    _ => Ok(()),
                };
                match done {
                    Ok(()) => save(app),
                    Err(why) => {
                        app.toast(why, crate::agent::Level::Warn);
                        app.edit = Some(e);
                        return;
                    }
                }
            }
            if let Outcome::CommitMove(d) = outcome {
                let (n, _) = dims(app);
                app.models_ui.row = (app.models_ui.row as i64 + d as i64).clamp(0, n.saturating_sub(1) as i64) as usize;
            }
        }
    }
}

/// ⏎ in the key flow: the key → its base URL (guessed from the prefix, or asked for) → the
/// provider added and asked for its models.
fn keyflow_next(app: &mut App) {
    let Some(e) = app.edit.take() else { return };
    let text = e.text().trim().to_string();
    match e.target {
        EditTarget::KeyFlowKey => {
            let preset = app.models_ui.keyflow.as_ref().map(|k| k.guess.clone()).unwrap_or_default();
            let guess = if preset.is_empty() { crate::provider_key::provider_from_key(&text, None).base_url } else { preset };
            if let Some(k) = app.models_ui.keyflow.as_mut() {
                k.key = text;
                k.guess = guess.clone();
            }
            app.edit = Some(Edit::new(EditTarget::KeyFlowUrl, &guess, Start::Append));
        }
        EditTarget::KeyFlowUrl if text.is_empty() => {
            app.toast("give the API's base URL — this key's prefix names no provider", crate::agent::Level::Info);
            app.edit = Some(e);
        }
        EditTarget::KeyFlowUrl => {
            let key = app.models_ui.keyflow.as_ref().map(|k| k.key.clone()).unwrap_or_default();
            match app.add_provider_from_key(&key, &text) {
                Ok(id) => {
                    let host = app.probe_provider(&id).unwrap_or_default();
                    if let Some(k) = app.models_ui.keyflow.as_mut() {
                        k.key.clear();
                        k.probing = Some((id.clone(), host));
                    }
                    app.models_ui.row = prov_row_of(&app.registry, &id).unwrap_or(0);
                    app.models_ui.col = PROV_ID_COL;
                }
                Err(why) => {
                    app.toast(why, crate::agent::Level::Warn);
                    app.edit = Some(e);
                }
            }
        }
        _ => {}
    }
}

/// Delete on the selected row: the first press arms, the second deletes and saves. A provider
/// takes its models with it (they could not run without it), Codex's row every model on its
/// account. Removing the last model returns Solo to its setup state.
fn delete(app: &mut App) {
    let row = app.models_ui.row;
    if !app.models_ui.providers {
        let Some(alias) = app.registry.models.get(row).map(|m| m.alias.clone()) else { return };
        if form::confirm(&mut app.confirm, &alias) {
            app.registry.models.remove(row);
            app.models_ui.row = row.min(app.registry.models.len().saturating_sub(1));
            save(app);
            reconcile_default_model(app);
            app.toast(format!("deleted {alias}"), crate::agent::Level::Ok);
        }
        return;
    }
    match prov_rows(&app.registry).get(row).copied() {
        Some(ProvRow::Codex) => {
            let n = app.registry.codex_account_models().len();
            if form::confirm(&mut app.confirm, &format!("the Codex account and its {n} model{}", if n == 1 { "" } else { "s" })) {
                match app.remove_codex_account() {
                    Ok(n) => app.toast(format!("removed the Codex account: {n} model{}", if n == 1 { "" } else { "s" }), crate::agent::Level::Ok),
                    Err(e) => app.toast(e, crate::agent::Level::Error),
                }
                reconcile_default_model(app);
                fix_rows(app);
            }
        }
        Some(ProvRow::Entry(i)) => {
            let id = app.registry.providers[i].id.clone();
            let n = app.registry.models.iter().filter(|m| m.provider == id).count();
            let what = if n == 0 { id.clone() } else { format!("{id} and its {n} model{}", if n == 1 { "" } else { "s" }) };
            if form::confirm(&mut app.confirm, &what) {
                app.registry.providers.remove(i);
                app.registry.models.retain(|m| m.provider != id);
                save(app);
                fix_rows(app);
                app.toast(format!("deleted {what}"), crate::agent::Level::Ok);
            }
        }
        None => {}
    }
}

/// Keep Solo's configured model on an entry that still exists after a removal. An empty registry
/// keeps an empty setting so startup can remain in setup until the user adds a model.
fn reconcile_default_model(app: &mut App) {
    if app.registry.alias_problem(&app.settings.default_model).is_some() {
        app.settings.default_model = app.registry.models.iter().find(|m| app.demo || app.registry.alias_problem(&m.alias).is_none()).map(|m| m.alias.clone()).unwrap_or_default();
        app.save_settings();
    }
}

/// The selection back inside the grids after rows went.
fn fix_rows(app: &mut App) {
    let (n, _) = dims(app);
    app.models_ui.row = app.models_ui.row.min(n.saturating_sub(1));
}

pub fn models_key(app: &mut App, k: KeyEvent) {
    if app.edit.is_some() {
        return edit_key(app, k);
    }
    let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
    let (nrows, ncols) = dims(app);
    let kind = sel_kind(app);
    if k.code != KeyCode::Delete {
        app.confirm = None;
    }
    match k.code {
        KeyCode::Char('s') if ctrl => match app.save_registry() {
            Ok(()) if app.demo => app.toast("the demo keeps its models in memory — they point at its mocks", crate::agent::Level::Info),
            Ok(()) => app.toast("models saved — new agents use these settings", crate::agent::Level::Ok),
            Err(e) => app.toast(e, crate::agent::Level::Error),
        },
        KeyCode::Char('n') if ctrl => {
            let menu = add_menu(app);
            app.overlays.push(Overlay::Select(menu));
        }
        KeyCode::Char('t') if ctrl => {
            if app.models_ui.providers {
                app.toast("ctrl+t tests a model — tab to the models", crate::agent::Level::Info);
            } else if let Some(alias) = app.registry.models.get(app.models_ui.row).map(|m| m.alias.clone()) {
                app.probe_model(&alias);
            }
        }
        KeyCode::Char('d') if ctrl => {
            if !app.models_ui.providers {
                return app.discover(None);
            }
            match prov_rows(&app.registry).get(app.models_ui.row).copied() {
                Some(ProvRow::Entry(i)) => {
                    let p = &app.registry.providers[i];
                    match p.draft_reason() {
                        // A draft never goes on the network — say why instead (P2, test report).
                        Some(why) => {
                            let id = p.id.clone();
                            app.toast(format!("{id} is a draft, not discovered — {why}"), crate::agent::Level::Warn)
                        }
                        None => {
                            let id = p.id.clone();
                            app.discover(Some(id))
                        }
                    }
                }
                Some(ProvRow::Codex) => app.discover(None),
                None => app.toast("add a provider first (ctrl+n)", crate::agent::Level::Info),
            }
        }
        _ if ctrl => {}
        KeyCode::Esc => app.leave_screen(),
        KeyCode::Tab | KeyCode::BackTab => {
            app.models_ui.providers = !app.models_ui.providers;
            app.models_ui.row = 0;
            app.models_ui.col = 0;
        }
        KeyCode::Up => app.models_ui.row = app.models_ui.row.saturating_sub(1),
        KeyCode::Down => app.models_ui.row = (app.models_ui.row + 1).min(nrows.saturating_sub(1)),
        KeyCode::Left => app.models_ui.col = app.models_ui.col.saturating_sub(1),
        KeyCode::Right => app.models_ui.col = (app.models_ui.col + 1).min(ncols - 1),
        KeyCode::Home => app.models_ui.col = 0,
        KeyCode::End => app.models_ui.col = ncols - 1,
        KeyCode::Delete => delete(app),
        _ if kind == Kind::Fixed => {
            if matches!(k.code, KeyCode::Enter | KeyCode::Backspace | KeyCode::Char(_)) {
                app.toast("the Codex account is your codex login: nothing to edit — Delete removes its models", crate::agent::Level::Info);
            }
        }
        KeyCode::Enter => match kind {
            Kind::Choice | Kind::Toggle => {
                if step(app, 1) {
                    save(app);
                }
            }
            _ => begin(app, Start::Append, None),
        },
        KeyCode::Backspace if matches!(kind, Kind::Text | Kind::Secret | Kind::Step) => begin(app, Start::Clear, None),
        KeyCode::Char(c @ ('+' | '-' | '=')) if matches!(kind, Kind::Choice | Kind::Toggle | Kind::Step) => {
            if step(app, if c == '-' { -1 } else { 1 }) {
                save(app);
            }
        }
        KeyCode::Char(' ') if matches!(kind, Kind::Choice | Kind::Toggle) => {
            if step(app, 1) {
                save(app);
            }
        }
        KeyCode::Char(c) if kind == Kind::Toggle && "ynYN".contains(c) => {
            if let Some(m) = app.registry.models.get_mut(app.models_ui.row) {
                m.vision = c.eq_ignore_ascii_case(&'y');
                save(app);
            }
        }
        KeyCode::Char(c) if kind == Kind::Choice && c.is_alphanumeric() => {
            if jump(app, c) {
                save(app);
            }
        }
        KeyCode::Char(c) if kind == Kind::Step && !c.is_ascii_digit() => {}
        KeyCode::Char(c) if matches!(kind, Kind::Text | Kind::Secret | Kind::Step) && !c.is_control() && c != ' ' => begin(app, Start::Overwrite, Some(c)),
        _ => {}
    }
}

// ───────────────────────────── drawing ─────────────────────────────

/// The columns of a grid to draw in `full` cells with column `sel` among them: the first column
/// always, then from the leftmost one that lets `sel` fit within `avail` (`full` less what the
/// row needs after its cells), up to the last that fits whole — a column is shown entire or not
/// at all. Each column takes its width + 1.
fn shown_cols(ws: &[usize], sel: usize, avail: usize, full: usize) -> Vec<usize> {
    let sel = sel.min(ws.len().saturating_sub(1));
    let w = |i: usize| ws[i] + 1;
    let mut start = 1;
    while start < sel && w(0) + (start..=sel).map(w).sum::<usize>() > avail {
        start += 1;
    }
    let mut used = w(0);
    let mut out = vec![0];
    for i in start.max(1)..ws.len() {
        if used + w(i) > full && i != sel {
            break;
        }
        used += w(i);
        out.push(i);
    }
    out
}

/// One grid row: each shown cell padded to its width after a space, the selected one bold and
/// accent-coloured — or, being edited, the edit with the cursor at its place. Returns the spans
/// and the cursor's column within the row when a cell is being edited.
fn grid_cells(vals: &[String], ws: &[usize], show: &[usize], style: impl Fn(usize, &str) -> Style, sel: Option<usize>, edit: Option<&Edit>) -> (Vec<Span<'static>>, Option<usize>) {
    let mut spans = vec![];
    let (mut x, mut cursor) = (0usize, None);
    for &ci in show {
        let w = ws[ci];
        spans.push(Span::raw(" "));
        x += 1;
        let is = sel == Some(ci);
        match edit.filter(|_| is) {
            Some(e) => {
                let (shown, cur) = e.view(w);
                cursor = Some(x + cur);
                spans.push(Span::styled(format!("{shown:<w$}"), theme::bold(theme::text()).add_modifier(Modifier::UNDERLINED)));
            }
            None => {
                let st = if is { theme::bold(theme::accent()) } else { style(ci, &vals[ci]) };
                spans.push(Span::styled(format!("{:<w$}", trunc(&vals[ci], w)), st));
            }
        }
        x += w;
    }
    (spans, cursor)
}

/// The first row of a list of `n` shown in `room` lines so that `sel` is in view.
fn first_row(sel: usize, n: usize, room: usize) -> usize {
    if n <= room {
        0
    } else {
        form::scroll_for(sel, 0, room.max(1), n)
    }
}

pub fn draw_models(f: &mut Frame, app: &mut App) {
    let area = f.area();
    let prows = prov_rows(&app.registry);
    let flow = app.models_ui.keyflow.is_some();
    // header + rows + the key flow's line + a blank and four hints, inside the border
    let prov_h = (prows.len() as u16 + 1 + u16::from(flow) + 5 + 2).min(area.height.saturating_sub(10).max(8));
    let rows = Layout::default().direction(Direction::Vertical).constraints([Constraint::Length(1), Constraint::Min(6), Constraint::Length(prov_h), Constraint::Length(1)]).split(area);
    let ui = &app.models_ui;
    let mut crumbs = vec![Span::styled(format!("  {} models", theme::g("›", ">")), theme::faint())];
    if app.demo {
        crumbs.push(Span::styled("  demo · changes stay in memory", theme::dim()));
    } else if ui.dirty {
        crumbs.push(Span::styled(format!("  {} not saved", theme::g("●", "*")), theme::fg(theme::AMBER)));
    } else if ui.saved.is_some_and(|t| t.elapsed() < crate::app::SAVED_SHOWN) {
        crumbs.push(Span::styled(format!("  {} saved", theme::g("✓", "ok")), theme::fg(theme::GREEN)));
    }
    // whether it is saved matters more than where: the path goes first when both do not fit
    let path = home_rel(&Registry::path());
    let left_w = 9 + crumbs.iter().map(|s| crate::util::width(&s.content)).sum::<usize>();
    let right = if left_w + crate::util::width(&path) + 2 <= area.width as usize { vec![Span::styled(path, theme::faint()), Span::raw(" ")] } else { vec![] };
    header(f, rows[0], crumbs, right);
    let editing_model = app.edit.as_ref().filter(|e| matches!(e.target, EditTarget::ModelCell(..)));
    let editing_prov = app.edit.as_ref().filter(|e| matches!(e.target, EditTarget::ProviderCell(..)));
    // A table wider than the screen scrolls sideways to the selected column, the first column
    // (alias / id) kept, so every cell can be seen and edited at 80 columns too.
    let (mshow, pshow) = {
        let (msel, psel) = if ui.providers { (0, ui.col) } else { (ui.col, 0) };
        // a provider row ends in its status (" ○ no key" at the least): room for it when scrolled
        let (mw, pfull) = (rows[1].width.saturating_sub(2) as usize, rows[2].width.saturating_sub(2) as usize);
        // the models grid keeps a cut last column: the start of a ctrl+t result ("ok 0.4s") still reads
        (shown_cols(&MODEL_WIDTHS, msel, mw, usize::MAX), shown_cols(&PROV_WIDTHS, psel, pfull.saturating_sub(10), pfull))
    };
    let mk_head = |cols: &[&str], ws: &[usize], show: &[usize]| Line::from(show.iter().map(|&i| Span::styled(format!(" {:<w$}", cols[i], w = ws[i]), theme::bold(theme::muted()))).collect::<Vec<_>>());

    // ── models ──
    let mblk = block("models", if ui.providers { theme::FAINT } else { theme::SAFFRON });
    let minner = mblk.inner(rows[1]);
    f.render_widget(mblk, rows[1]);
    let hints = [
        " context + compact % are always passed to each agent (model_context_window, model_auto_compact_token_limit)",
        " dim \"(default)\" = not set here; context inherits the default model, compact uses 85% · +/- steps values",
        " vision = the model takes images: its agents get the screenshot and view-image tools · space flips it",
    ];
    // the hints (and a blank line above them) only while every model row fits beside them
    let hint_h = if (minner.height as usize) > app.registry.models.len() + 1 + hints.len() { hints.len() + 1 } else { 0 };
    let room = (minner.height as usize).saturating_sub(1 + hint_h);
    let mrow = if ui.providers { usize::MAX } else { ui.row };
    let mstart = first_row(ui.row.min(app.registry.models.len().saturating_sub(1)), app.registry.models.len(), room);
    // Whole-thousands tokens print as "200k" (no decimal); anything else falls back to fmt_tokens.
    let short_tokens = |n: u64| if n > 0 && n % 1000 == 0 { format!("{}k", n / 1000) } else { fmt_tokens(n) };
    let mut l = vec![mk_head(MODEL_COLS, &MODEL_WIDTHS, &mshow)];
    let mut cursor = None;
    for (ri, m) in app.registry.models.iter().enumerate().skip(mstart).take(room) {
        let status = ui.status.get(&m.alias).cloned().unwrap_or_else(|| m.note.clone());
        let vals = [
            m.alias.clone(),
            m.provider.clone(),
            m.model.clone(),
            match m.context_window {
                Some(cw) => short_tokens(cw),
                None => format!("{} (default)", short_tokens(app.registry.effective_context_window(&m.alias, &app.settings.default_model))),
            },
            match m.auto_compact_percent {
                Some(p) => format!("{p}%"),
                None => format!("{}% (default)", m.effective_compact_percent()),
            },
            if m.efforts().is_empty() { "—".into() } else { m.default_effort.clone() },
            if m.efforts().is_empty() { "none (not sent)".into() } else { m.efforts().join(" ") },
            theme::g(if m.vision { "◉" } else { "·" }, if m.vision { "y" } else { "-" }).to_string(),
            status,
        ];
        let this = ri == mrow;
        let style = |ci: usize, v: &str| {
            if (ci == CONTEXT_COL && m.context_window.is_none()) || (ci == COMPACT_COL && m.auto_compact_percent.is_none()) {
                theme::dim()
            } else if ci == EFFORT_COL {
                theme::fg(effort_color(v))
            } else if ci == VISION_COL && m.vision {
                theme::fg(theme::BLUE)
            } else if ci == NOTE_COL && v.starts_with("ok") {
                theme::fg(theme::GREEN)
            } else if ci == NOTE_COL && v.starts_with("error") {
                theme::fg(theme::RED)
            } else if this {
                theme::text()
            } else {
                theme::muted()
            }
        };
        let (spans, cur) = grid_cells(&vals, &MODEL_WIDTHS, &mshow, style, this.then_some(ui.col), editing_model.filter(|_| this));
        if let Some(c) = cur {
            cursor = Some((minner.x + c as u16, minner.y + l.len() as u16));
        }
        l.push(Line::from(spans));
    }
    let band_at = (mrow >= mstart && mrow < mstart + room).then(|| 1 + mrow - mstart);
    if hint_h > 0 {
        l.resize(minner.height as usize - hints.len(), Line::default());
        l.extend(hints.iter().map(|h| Line::from(Span::styled(*h, theme::faint()))));
    }
    f.render_widget(Paragraph::new(l), minner);
    if let Some(y) = band_at {
        form::band_line(f.buffer_mut(), minner, y, 0);
    }

    // ── providers ──
    let pblk = block("providers", if ui.providers { theme::SAFFRON } else { theme::FAINT });
    let pinner = pblk.inner(rows[2]);
    f.render_widget(pblk, rows[2]);
    let pw = PROV_WIDTHS;
    let mut pl = vec![mk_head(PROV_COLS, &pw, &pshow)];
    // while a provider is being added, its row is the selected one: the grid's own selection rests
    let prow = if ui.providers && !flow { ui.row } else { usize::MAX };
    let proom = (pinner.height as usize).saturating_sub(1 + usize::from(flow) + 5).max(1);
    let pstart = first_row(ui.row.min(prows.len().saturating_sub(1)), prows.len(), proom);
    let room = |spans: &Vec<Span>| (pinner.width as usize).saturating_sub(Line::from(spans.clone()).width());
    let mut pband = None;
    for (ri, r) in prows.iter().enumerate().skip(pstart).take(proom) {
        let this = ri == prow;
        let (vals, status) = match r {
            ProvRow::Codex => {
                let n = app.registry.codex_account_models().len();
                let vals = ["openai", "Codex", "(ChatGPT login via codex)", "", "", "Codex", "—", "—"].map(String::from);
                let missing = !app.demo && !crate::config::codex_available();
                let status = if missing {
                    (format!(" {} codex is not on PATH", theme::g("⚠", "!")), format!(" {} no codex", theme::g("⚠", "!")), theme::fg(theme::AMBER))
                } else {
                    (format!(" {} codex login", theme::g("○", "-")), format!(" {} login", theme::g("○", "-")), theme::dim())
                };
                (vals, (status, n))
            }
            ProvRow::Entry(i) => {
                let p = &app.registry.providers[*i];
                // auth means something for ClaudeCode only (the hint below says so): any other
                // kind shows "—" there, as the Codex row does, not a `subscription` it lacks.
                let auth = if p.kind == ProviderKind::ClaudeCode { p.auth.clone() } else { "—".to_string() };
                let vals = [p.id.clone(), p.name.clone(), p.base_url.clone(), p.env_key.clone(), p.api_key.as_deref().map(form::mask).unwrap_or_default(), kind_str(p.kind).to_string(), auth, wire_str(p)];
                // A draft (placeholder/malformed URL, or no credential — `draft_reason`) says why
                // ctrl+t/ctrl+d are off for it; a ready row shows how its key is held. A
                // `ClaudeCode` + `subscription` row needs no key (OAuth), and a Bija key is
                // optional (a local server takes none): only a key actually expected is missing.
                let status = if let Some(why) = p.draft_reason() {
                    (format!(" {} draft — {why}", theme::g("✗", "x")), format!(" {} draft", theme::g("✗", "x")), theme::fg(theme::AMBER))
                } else if p.kind == ProviderKind::Bija && p.resolve_key().is_none() {
                    (format!(" {} no key (optional)", theme::g("○", "-")), format!(" {} no key", theme::g("○", "-")), theme::dim())
                } else if !p.needs_key() {
                    (format!(" {} subscription (OAuth login)", theme::g("○", "-")), format!(" {} OAuth", theme::g("○", "-")), theme::dim())
                } else {
                    let key_msg = if !p.env_key.trim().is_empty() && std::env::var(p.env_key.trim()).map(|v| !v.trim().is_empty()).unwrap_or(false) { "key set (env)" } else { "key stored in models.toml" };
                    (format!(" {} {key_msg}", theme::g("✓", "ok")), format!(" {} key", theme::g("✓", "ok")), theme::fg(theme::GREEN))
                };
                let n = app.registry.models.iter().filter(|m| m.provider == p.id).count();
                (vals, (status, n))
            }
        };
        let codex = *r == ProvRow::Codex;
        let style = |_: usize, _: &str| if codex { theme::muted() } else { theme::text() };
        let (mut spans, cur) = grid_cells(&vals, &pw, &pshow, style, this.then_some(ui.col), editing_prov.filter(|_| this));
        if let Some(c) = cur {
            cursor = Some((pinner.x + c as u16, pinner.y + pl.len() as u16));
        }
        // Room is short at 80-120 columns: the status always shows (whole, or its short form),
        // then the model count and the host as far as they fit — never a word cut by the border.
        let ((status, short, st), n) = status;
        let left = room(&spans);
        spans.push(Span::styled(if w_of(&status) <= left { status } else { trunc(&short, left) }, st));
        let count = format!("  {n} model{}", if n == 1 { "" } else { "s" });
        // the host, unless the base_url column already shows the whole URL (a draft says why
        // nothing is sent instead)
        let host = match r {
            ProvRow::Entry(i) => {
                let p = &app.registry.providers[*i];
                p.host().filter(|_| p.draft_reason().is_none() && (!pshow.contains(&PROV_URL_COL) || w_of(&p.base_url) > pw[PROV_URL_COL])).map(|h| format!(" {} {h}", theme::g("→", "->")))
            }
            ProvRow::Codex => None,
        };
        if let Some(h) = host.filter(|h| w_of(h) + w_of(&count) <= room(&spans)) {
            spans.push(Span::styled(h, theme::dim()));
        }
        if w_of(&count) <= room(&spans) {
            spans.push(Span::styled(count, if n == 0 { theme::fg(theme::AMBER) } else { theme::dim() }));
        }
        if this {
            pband = Some(pl.len());
        }
        pl.push(Line::from(spans));
    }
    if let Some(k) = &app.models_ui.keyflow {
        let (line, cur) = keyflow_line(app, k, pinner.width as usize);
        if let Some(c) = cur {
            cursor = Some((pinner.x + c as u16, pinner.y + pl.len() as u16));
        }
        pband = Some(pl.len());
        pl.push(line);
    }
    if !app.registry.providers.iter().any(|p| p.kind == ProviderKind::Bija) {
        pl.push(Line::from(Span::styled(" ○ Bija inactive · ctrl+n adds an API provider", theme::dim())));
    }
    pl.push(Line::default());
    let hint_rows = (pinner.height as usize).saturating_sub(pl.len());
    for h in provider_hints(pinner.width as usize, ui.providers && ui.col >= PROV_KIND_COL).iter().take(hint_rows) {
        pl.push(Line::from(Span::styled(*h, theme::faint())));
    }
    f.render_widget(Paragraph::new(pl), pinner);
    if let Some(y) = pband {
        form::band_line(f.buffer_mut(), pinner, y, 0);
    }
    if let Some(c) = cursor {
        f.set_cursor_position(c);
    }
    let hints = models_footer(app, rows[3].width);
    footer(f, rows[3], &hints.iter().map(|(k, d)| (*k, d.as_str())).collect::<Vec<_>>());
}

/// The key flow's row under the providers: the key (masked) being typed, then the base URL, then
/// the probe's spinner. Returns the line and the cursor's column while a field is edited.
fn keyflow_line(app: &App, k: &KeyFlow, width: usize) -> (Line<'static>, Option<usize>) {
    let label = |t: &str| Span::styled(format!("{t} "), theme::bold(theme::muted()));
    let mut spans = vec![Span::styled(format!(" {} new provider   ", theme::g("+", "+")), theme::bold(theme::accent()))];
    if let Some((id, host)) = &k.probing {
        spans.push(Span::styled(format!("{} ", anim::spinner()), theme::fg(theme::SAFFRON)));
        spans.push(Span::styled(format!("{id}: asking {host} for its models…"), theme::muted()));
        return (Line::from(spans), None);
    }
    let edit = app.edit.as_ref();
    let mut cursor = None;
    match edit.map(|e| &e.target) {
        Some(EditTarget::KeyFlowUrl) => {
            spans.push(label("key"));
            let shown = if k.key.is_empty() { "none".to_string() } else { form::mask(&k.key) };
            spans.push(Span::styled(format!("{shown}   "), theme::dim()));
            spans.push(label("base URL"));
            let x = Line::from(spans.clone()).width();
            // the URL first: the note after it shortens, then goes, before the URL is cut
            let want = edit.map(|e| w_of(&e.text()) + 2).unwrap_or(0).clamp(24, 60);
            let notes: [&str; 3] = if k.guess.is_empty() { ["  (required: this key's prefix names no provider)", "  (required)", ""] } else { ["  (guessed from the key — ⏎ takes it)", "  (guessed)", ""] };
            let after = notes.iter().find(|n| width.saturating_sub(x + w_of(n) + 1) >= want).copied().unwrap_or("");
            // a column short of the border
            let room = width.saturating_sub(x + w_of(after) + 1).max(16);
            if let Some(e) = edit {
                let (shown, cur) = e.view(room);
                cursor = Some(x + cur);
                spans.push(Span::styled(format!("{shown:<room$}"), theme::bold(theme::text()).add_modifier(Modifier::UNDERLINED)));
            }
            spans.push(Span::styled(after, theme::faint()));
        }
        _ => {
            spans.push(label("API key"));
            let x = Line::from(spans.clone()).width();
            let after = if width >= x + 70 { "  (paste it — it is stored in models.toml, 0600)" } else { "  (paste it)" };
            let room = width.saturating_sub(x + w_of(after) + 1).max(16);
            if let Some(e) = edit {
                let (shown, cur) = e.view(room);
                cursor = Some(x + cur);
                spans.push(Span::styled(format!("{shown:<room$}"), theme::bold(theme::text()).add_modifier(Modifier::UNDERLINED)));
            }
            spans.push(Span::styled(after, theme::faint()));
        }
    }
    (Line::from(spans), cursor)
}

/// The footer for what the screen is doing: editing (save / revert), a Delete armed, the key
/// flow, or the grid's own keys — eight at the most, the ones that matter first (`footer` drops
/// whole hints from the end on a narrow terminal).
fn models_footer(app: &App, width: u16) -> Vec<(&'static str, String)> {
    let ui = &app.models_ui;
    if let Some(e) = &app.edit {
        return match e.target {
            EditTarget::KeyFlowKey => vec![("⏎", "next: the base URL".into()), ("esc", "cancel".into())],
            EditTarget::KeyFlowUrl => vec![("⏎", "add it and list its models".into()), ("esc", "cancel".into())],
            _ => vec![("⏎", "save".into()), ("esc", "revert".into()), ("↑↓", "save and move".into())],
        };
    }
    let mut v: Vec<(&'static str, String)> = vec![];
    if let Some(what) = form::armed(&app.confirm) {
        v.push(("del", format!("again to delete {what}")));
        v.push(("esc", "keep it".into()));
        return v;
    }
    let wide = width >= 110;
    // ctrl+t / ctrl+d name the host the selected row's request goes to, or say they are off for
    // a draft (the row itself carries the reason)
    let action = |plain: &str, verb: &str, p: Option<&ProviderEntry>| match p {
        Some(p) if p.draft_reason().is_some() => format!("{verb} · off (draft)"),
        Some(p) => p.host().filter(|_| wide).map(|h| format!("{verb} {} {h}", theme::g("→", "->"))).unwrap_or_else(|| plain.to_string()),
        None => plain.to_string(),
    };
    let kind = sel_kind(app);
    v.push(match kind {
        Kind::Choice => ("+/-", "change".into()),
        Kind::Toggle => ("space", "flip".into()),
        Kind::Fixed => ("del", "remove the Codex account".into()),
        _ => ("type ⏎", "edit".into()),
    });
    v.push(("ctrl+n", "add".into()));
    if kind != Kind::Fixed {
        v.push(("del", "delete".into()));
    }
    if ui.providers {
        let p = match prov_rows(&app.registry).get(ui.row) {
            Some(ProvRow::Entry(i)) => app.registry.providers.get(*i),
            _ => None,
        };
        v.push(("ctrl+d", action("discover", "discover", p)));
        v.push(("tab", "models".into()));
    } else {
        let p = app.registry.models.get(ui.row).and_then(|m| app.registry.provider_of(&m.alias));
        v.push(("ctrl+t", action("test", "test", p)));
        v.push(("ctrl+d", "discover".into()));
        v.push(("tab", "providers".into()));
    }
    v.push(("esc", "back".into()));
    v
}

/// The four lines under the providers grid. Wide, they explain every column; narrower, the
/// ones about the column group being edited (kind/auth/wire, or the rest), each within `w`.
fn provider_hints(w: usize, enums: bool) -> [&'static str; 4] {
    if w >= 118 {
        return [
            " base_url = the API root: Codex adds /responses, Bija /chat/completions or /messages, ClaudeCode wants it without /v1",
            " env_key = an env var holding the key · api_key = paste one (stored 0600) · either works · ctrl+d discovers …/models",
            " ctrl+n chooses an API endpoint or local URL, a Codex model, or Claude Code auth",
            " kind = Codex | ClaudeCode | Bija · auth (ClaudeCode) = subscription | api_key · wire (Bija) = openai | anthropic",
        ];
    }
    if enums {
        [" kind = Codex | ClaudeCode | Bija · +/- cycles kind, auth and wire", " auth (ClaudeCode only) = subscription | api_key", " wire (Bija only) = openai (…/chat/completions) | anthropic (…/messages)", " ctrl+n chooses an API, Codex model or Claude Code auth"]
    } else {
        [" base_url = the API root (Codex …/v1/responses · Bija …/chat/completions)", " env_key = an env var holding the key · api_key = paste one (0600)", " ctrl+d discovers …/models · → reaches kind, auth and wire", " ctrl+n chooses an API, Codex model or Claude Code auth"]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(app: &mut App, code: KeyCode) {
        models_key(app, KeyEvent::new(code, KeyModifiers::NONE));
    }

    fn ctrl(app: &mut App, c: char) {
        models_key(app, KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL));
    }

    fn typed(app: &mut App, s: &str) {
        for c in s.chars() {
            press(app, KeyCode::Char(c));
        }
    }

    /// The test app outside the demo, whose registry is saved to models.toml (the demo's never is).
    fn saving_app() -> App {
        let mut app = crate::web::snapshot::tests::test_app();
        app.demo = false;
        app
    }

    fn screen(app: &mut App, w: u16, h: u16) -> (Vec<String>, ratatui::buffer::Buffer) {
        let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(w, h)).unwrap();
        term.draw(|f| draw_models(f, app)).unwrap();
        let b = term.backend().buffer().clone();
        ((0..b.area.height).map(|y| (0..b.area.width).map(|x| b[(x, y)].symbol()).collect()).collect(), b)
    }

    /// The grid scrolls sideways to the selected column, keeping the first, and shows columns
    /// whole or not at all.
    #[test]
    fn a_wide_grid_shows_the_selected_column() {
        let ws = PROV_WIDTHS;
        assert_eq!(shown_cols(&ws, 0, 200, 200), (0..8).collect::<Vec<_>>(), "everything fits");
        assert_eq!(shown_cols(&ws, 0, 68, 78), vec![0, 1, 2, 3, 4], "kind would be cut: left out");
        assert_eq!(shown_cols(&ws, 7, 68, 78), vec![0, 4, 5, 6, 7], "wire selected: from api_key on");
        assert_eq!(shown_cols(&ws, 2, 68, 78), vec![0, 1, 2, 3, 4]);
    }

    /// Type to edit: a letter overwrites the cell in place, ⏎ saves models.toml, esc brings the
    /// old value back, ⏎ on a cell edits what is there, ↑↓ save and move — and no popup opens.
    #[test]
    fn a_cell_is_typed_into_and_enter_saves_models_toml() {
        crate::web::snapshot::tests::with_mantra_home(|| {
            let mut app = saving_app();
            app.enter_screen(crate::app::Screen::Models);
            app.models_ui.col = NOTE_COL;
            typed(&mut app, "fast");
            assert!(matches!(&app.edit, Some(e) if e.target == EditTarget::ModelCell(0, NOTE_COL) && e.text() == "fast"), "the cell is the editor");
            assert!(app.overlays.is_empty(), "no popup");
            press(&mut app, KeyCode::Enter);
            assert!(app.edit.is_none());
            assert_eq!(app.registry.models[0].note, "fast");
            assert_eq!(Registry::load().models[0].note, "fast", "saved on ⏎");
            // ⏎ edits the value as it is; esc puts it back
            press(&mut app, KeyCode::Enter);
            assert_eq!(app.edit.as_ref().map(|e| e.text()), Some("fast".into()));
            typed(&mut app, "er");
            press(&mut app, KeyCode::Esc);
            assert_eq!((app.registry.models[0].note.as_str(), app.edit.is_none()), ("fast", true), "reverted");
            assert_eq!(app.screen, crate::app::Screen::Models, "esc while editing never leaves the screen");
            // ↓ keeps the text and moves on
            typed(&mut app, "x");
            press(&mut app, KeyCode::Down);
            assert_eq!((app.registry.models[0].note.as_str(), app.models_ui.row), ("x", 1));
            // backspace starts from nothing; an alias that is taken is refused and stays open
            app.models_ui.col = ALIAS_COL;
            press(&mut app, KeyCode::Backspace);
            typed(&mut app, "astra");
            press(&mut app, KeyCode::Enter);
            assert!(app.toast.as_ref().is_some_and(|t| t.0.contains("astra is taken")), "{:?}", app.toast);
            assert!(app.edit.is_some(), "still editing, nothing changed");
            assert_eq!(app.registry.models[1].alias, "sol");
            press(&mut app, KeyCode::Esc);
            press(&mut app, KeyCode::Esc);
            assert_eq!(app.screen, crate::app::Screen::Solo, "esc on the grid leaves it");
        });
    }

    /// The demo keeps what is typed into the grid in memory: models.toml is never written (its
    /// models point at the demo's mocks) and the header says so.
    #[test]
    fn the_demo_keeps_its_models_in_memory() {
        crate::web::snapshot::tests::with_mantra_home(|| {
            let mut app = crate::web::snapshot::tests::test_app();
            assert!(app.demo);
            app.enter_screen(crate::app::Screen::Models);
            app.models_ui.col = NOTE_COL;
            typed(&mut app, "fast");
            press(&mut app, KeyCode::Enter);
            assert_eq!(app.registry.models[0].note, "fast", "changed in memory");
            assert!(!Registry::path().exists(), "models.toml never written in the demo");
            assert!(!app.models_ui.dirty);
            ctrl(&mut app, 's');
            assert!(!Registry::path().exists());
            assert!(app.toast.as_ref().is_some_and(|t| t.0.contains("in memory")), "{:?}", app.toast);
            for w in [120, 64] {
                let (lines, _) = screen(&mut app, w, 30);
                assert!(lines[0].contains("demo · changes stay in memory"), "kept over the path at {w}: {}", lines[0]);
            }
        });
    }

    /// Choice and step cells take +/-, space and letters instead of typing: a letter on the
    /// effort jumps to the first effort it starts, digits on the context type a number.
    #[test]
    fn choices_step_and_letters_jump() {
        crate::web::snapshot::tests::with_mantra_home(|| {
            let mut app = saving_app();
            app.models_ui.col = EFFORT_COL;
            let before = app.registry.models[0].default_effort.clone();
            typed(&mut app, "h");
            assert_eq!(app.registry.models[0].default_effort, "high", "from {before}");
            assert!(app.edit.is_none(), "a choice is not typed into");
            press(&mut app, KeyCode::Char('+'));
            assert_ne!(app.registry.models[0].default_effort, "high");
            app.models_ui.col = CONTEXT_COL;
            typed(&mut app, "4");
            typed(&mut app, "00k");
            press(&mut app, KeyCode::Enter);
            assert_eq!(app.registry.models[0].context_window, Some(400_000));
            press(&mut app, KeyCode::Char('-'));
            assert_eq!(app.registry.models[0].context_window, Some(272_000));
            press(&mut app, KeyCode::Char('x'));
            assert!(app.edit.is_none(), "a letter on a number does nothing");
            app.models_ui.col = VISION_COL;
            let v = app.registry.models[0].vision;
            press(&mut app, KeyCode::Char(' '));
            assert_eq!(app.registry.models[0].vision, !v);
            press(&mut app, KeyCode::Enter);
            assert_eq!(app.registry.models[0].vision, v, "⏎ flips it back");
            assert_eq!(Registry::load().models[0].vision, v, "each change saved");
        });
    }

    /// Delete twice within three seconds removes a model (saved); one press only arms it and the
    /// footer says what a second would delete.
    #[test]
    fn delete_twice_removes_a_row() {
        crate::web::snapshot::tests::with_mantra_home(|| {
            let mut app = saving_app();
            let n = app.registry.models.len();
            press(&mut app, KeyCode::Delete);
            assert_eq!(app.registry.models.len(), n, "armed only");
            let (lines, _) = screen(&mut app, 120, 30);
            assert!(lines.last().is_some_and(|l| l.contains("del again to delete astra")), "{:?}", lines.last());
            press(&mut app, KeyCode::Delete);
            assert_eq!(app.registry.models.len(), n - 1);
            assert!(!app.registry.models.iter().any(|m| m.alias == "astra"));
            assert_eq!(Registry::load().models.len(), n - 1, "saved");
            // another key in between disarms
            press(&mut app, KeyCode::Delete);
            press(&mut app, KeyCode::Right);
            press(&mut app, KeyCode::Delete);
            assert_eq!(app.registry.models.len(), n - 1);
        });
    }

    /// The Codex account shows as the providers grid's first row while a model is on it; its
    /// cells are read-only, and Delete twice removes every model on it in one go — the other
    /// providers' rows keep their own indices.
    #[test]
    fn the_codex_row_removes_the_accounts_models() {
        crate::web::snapshot::tests::with_mantra_home(|| {
            let mut app = saving_app();
            app.registry = Registry::defaults();
            assert!(prov_rows(&app.registry).is_empty(), "an empty registry has no provider rows");
            app.registry.add_codex_model("my-model", "gpt-custom").unwrap();
            assert_eq!(prov_rows(&app.registry), vec![ProvRow::Codex]);
            press(&mut app, KeyCode::Tab);
            let (lines, _) = screen(&mut app, 140, 34);
            assert!(lines.iter().any(|l| l.contains("openai") && l.contains("(ChatGPT login via codex)")), "{}", lines.join("\n"));
            typed(&mut app, "x");
            assert!(app.edit.is_none(), "read-only");
            press(&mut app, KeyCode::Delete);
            assert_eq!(app.registry.codex_account_models().len(), 1, "armed");
            press(&mut app, KeyCode::Delete);
            assert!(app.registry.codex_account_models().is_empty());
            assert_eq!(app.registry.models.len(), 0, "account disable removes its models");
            assert!(Registry::load().codex_account_models().is_empty(), "saved");
            assert!(prov_rows(&app.registry).is_empty());
            assert!(app.toast.as_ref().is_some_and(|t| t.0 == "removed the Codex account: 1 model"), "{:?}", app.toast);
        });
    }

    /// The add menu offers the four ways to add; a model on the only provider opens its id cell.
    #[test]
    fn ctrl_n_offers_the_add_menu() {
        crate::web::snapshot::tests::with_mantra_home(|| {
            let mut app = saving_app();
            app.registry = Registry::defaults();
            ctrl(&mut app, 'n');
            let Some(Overlay::Select(s)) = app.overlays.last() else { panic!("the add menu") };
            assert_eq!(s.items.iter().map(|c| c.key.as_str()).collect::<Vec<_>>(), vec![ADD_KEY, ADD_MODEL, ADD_CODEX, ADD_CLAUDE, ADD_CLAUDE_KEY]);
            app.overlays.clear();
            add_chosen(&mut app, ADD_MODEL);
            let n = app.registry.models.len();
            assert_eq!((app.registry.models[n - 1].provider.as_str(), app.registry.models[n - 1].alias.as_str()), ("openai", "model1"));
            assert!(matches!(&app.edit, Some(e) if e.target == EditTarget::ModelCell(n - 1, MODEL_COL)));
            typed(&mut app, "my-model");
            press(&mut app, KeyCode::Enter);
            assert_eq!(Registry::load().models[n - 1].model, "my-model");
            app.demo = true;
            add_chosen(&mut app, ADD_CODEX);
            assert_eq!(app.registry.models.len(), n + 1, "Codex can be extended after its first model");
            press(&mut app, KeyCode::Esc);
            assert_eq!(app.registry.models.len(), n, "cancelling an empty Codex model removes its draft");
            app.registry.models.clear();
            add_chosen(&mut app, ADD_CODEX);
            assert!(matches!(&app.edit, Some(e) if matches!(&e.target, EditTarget::ModelCell(_, MODEL_COL))));
            assert_eq!(app.registry.models.last().map(|m| (m.provider.as_str(), m.model.as_str())), Some(("openai", "")));
            typed(&mut app, "gpt-custom");
            press(&mut app, KeyCode::Enter);
            assert_eq!(app.registry.models.last().map(|m| m.model.as_str()), Some("gpt-custom"));
            add_chosen(&mut app, ADD_CODEX);
            assert_eq!(app.registry.models.len(), 2, "the direct Codex action opens another model");
            press(&mut app, KeyCode::Esc);
            assert_eq!(app.registry.models.len(), 1);
        });
    }

    /// The key flow: an inline row with the key field already being edited (masked), ⏎ → the
    /// base URL its prefix suggests, ⏎ → the provider added (the key saved privately) and its
    /// models asked for off the event loop; esc cancels. The key never shows on screen.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_key_flow_adds_a_provider_and_probes_it() {
        crate::web::snapshot::tests::with_mantra_home(|| {
            let mut app = saving_app();
            add_chosen(&mut app, ADD_KEY);
            let Some(Overlay::Select(menu)) = app.overlays.pop() else { panic!("API choices") };
            assert_eq!(menu.then, SelectThen::ProviderPreset);
            assert!(menu.items.iter().any(|c| c.key == "groq") && menu.items.iter().any(|c| c.key == "custom"));
            provider_preset_chosen(&mut app, "custom");
            assert!(app.models_ui.providers && app.models_ui.keyflow.is_some());
            assert!(matches!(&app.edit, Some(e) if e.target == EditTarget::KeyFlowKey && e.secret));
            typed(&mut app, "gsk_live_secret_9f3a");
            let (lines, buf) = screen(&mut app, 140, 34);
            let all = lines.join("\n");
            assert!(!all.contains("gsk_live_secret") && all.contains("••••9f3a"), "masked: {all}");
            let row = lines.iter().position(|l| l.contains("new provider")).expect("the inline row");
            assert_eq!(buf[(5, row as u16)].bg, theme::row_sel().bg.unwrap(), "the flow's row is the selected one");
            press(&mut app, KeyCode::Enter);
            assert!(matches!(&app.edit, Some(e) if e.target == EditTarget::KeyFlowUrl && e.text() == "https://api.groq.com/openai/v1"), "the guess");
            // an unknown prefix has no guess and needs the URL typed
            press(&mut app, KeyCode::Esc);
            assert!(app.models_ui.keyflow.is_none() && app.edit.is_none(), "esc cancels");
            add_chosen(&mut app, ADD_KEY);
            app.overlays.pop();
            provider_preset_chosen(&mut app, "custom");
            typed(&mut app, "zz-unknown-1234");
            press(&mut app, KeyCode::Enter);
            assert_eq!(app.edit.as_ref().map(|e| e.text()), Some(String::new()));
            press(&mut app, KeyCode::Enter);
            assert!(app.toast.as_ref().is_some_and(|t| t.0.contains("base URL")), "required");
            typed(&mut app, "http://127.0.0.1:9/v1");
            press(&mut app, KeyCode::Enter);
            let p = app.registry.providers.iter().find(|p| p.id == "local").expect("added");
            assert_eq!(p.api_key.as_deref(), Some("zz-unknown-1234"));
            assert!(!std::fs::read_to_string(Registry::path()).unwrap().contains("zz-unknown-1234"), "models.toml stays shareable");
            assert!(std::fs::read_to_string(crate::config::home().join("secrets.toml")).unwrap().contains("zz-unknown-1234"), "saved privately");
            assert!(app.models_ui.keyflow.as_ref().is_some_and(|k| k.probing.as_ref().is_some_and(|(id, host)| id == "local" && host == "127.0.0.1:9") && k.key.is_empty()));
            assert!(app.animating(), "the spinner turns");
            // the probe's answer: a refusal keeps the provider and says why, without the key
            app.on_event(crate::app::AppEvent::Probed { provider: "local".into(), host: "127.0.0.1:9".into(), result: Err("couldn't reach http://127.0.0.1:9 with zz-unknown-1234".into()) });
            let t = app.toast.as_ref().map(|t| t.0.clone()).unwrap_or_default();
            assert!(t.starts_with("127.0.0.1:9: couldn't reach") && t.contains("the provider is kept") && !t.contains("zz-unknown-1234"), "{t}");
            assert!(app.models_ui.keyflow.is_none());
            // and a list: the picker, a short one all selected; importing names the host
            let found = vec![crate::discover::Found { id: "alpha".into(), context: None, reasoning: None, vision: false }];
            app.on_event(crate::app::AppEvent::Probed { provider: "local".into(), host: "127.0.0.1:9".into(), result: Ok(found) });
            let Some(Overlay::Discover(st)) = app.overlays.pop() else { panic!("the picker") };
            assert!(st.items.iter().all(|c| c.selected));
            app.apply_discover(&st);
            assert_eq!(app.toast.as_ref().map(|t| t.0.as_str()), Some("127.0.0.1:9: 1 model added"));
            assert!(app.registry.get("alpha").is_some_and(|m| m.provider == "local"));
        });
    }

    /// At 80 columns an explicitly configured Bija provider's kind and wire remain visible,
    /// and its discovery/test actions stay available.
    #[test]
    fn the_providers_grid_at_80_columns_shows_kind_and_wire() {
        let mut app = crate::web::snapshot::tests::test_app();
        app.registry.providers = vec![ProviderEntry { id: "local".into(), name: "Local vLLM".into(), base_url: "http://127.0.0.1:8000/v1".into(), kind: ProviderKind::Bija, wire: "anthropic".into(), ..Default::default() }];
        app.models_ui.providers = true;
        app.models_ui.row = 0;
        app.models_ui.col = PROV_WIRE_COL;
        let (lines, _) = screen(&mut app, 80, 24);
        let row = lines.iter().find(|l| l.contains("local")).cloned().unwrap_or_default();
        assert!(row.contains("Bija") && row.contains("anthropic") && !row.contains("unavailable"), "{row}");
        assert!(lines.iter().any(|l| l.contains("wire (Bija only)")), "{}", lines.join("\n"));
        let foot = lines.last().cloned().unwrap_or_default();
        assert!(foot.contains("+/- change") && foot.contains("ctrl+n add"), "the row keys come first: {foot}");
    }

    /// An explicitly configured provider may use Bija; cycling kind and auth remains editable.
    #[test]
    fn provider_kind_and_auth_cycle_with_explicit_bija() {
        crate::web::snapshot::tests::with_mantra_home(|| {
            let mut app = crate::web::snapshot::tests::test_app();
            app.registry.providers = vec![ProviderEntry { id: "gw".into(), base_url: "https://gw.dev/v1".into(), kind: ProviderKind::Codex, ..Default::default() }];
            app.models_ui.providers = true;
            app.models_ui.row = 0;
            app.models_ui.col = PROV_KIND_COL;
            for want in [ProviderKind::ClaudeCode, ProviderKind::Bija, ProviderKind::Codex] {
                press(&mut app, KeyCode::Char('+'));
                assert_eq!(app.registry.providers[0].kind, want);
            }
            typed(&mut app, "b");
            assert_eq!(app.registry.providers[0].kind, ProviderKind::Bija);
            app.models_ui.col = PROV_AUTH_COL;
            press(&mut app, KeyCode::Char('+'));
            assert_eq!(app.registry.providers[0].auth, "api_key");
            app.models_ui.col = PROV_KEY_COL;
            press(&mut app, KeyCode::Char('+'));
            assert!(matches!(&app.edit, Some(e) if e.secret && e.text() == "+"), "the key is typed, not cycled");
            press(&mut app, KeyCode::Esc);

            let mut reg = app.registry.clone();
            set_prov_cell(&mut reg, 0, PROV_KIND_COL, "seed").unwrap();
            reg.providers[0].kind = ProviderKind::Bija;
            set_prov_cell(&mut reg, 0, PROV_WIRE_COL, " Messages ").unwrap();
            assert_eq!(reg.providers[0].wire, "anthropic");
            set_prov_cell(&mut reg, 0, PROV_WIRE_COL, "grpc").unwrap();
            assert_eq!(wire_str(&reg.providers[0]), "grpc");
            assert!(reg.providers[0].draft_reason().is_some_and(|w| w.contains("grpc")), "a typo stays visible, as a draft");
            set_prov_cell(&mut reg, 0, PROV_WIRE_COL, "openai").unwrap();
            assert_eq!((reg.providers[0].wire.as_str(), wire_str(&reg.providers[0]).as_str()), ("", "openai"));
            assert_eq!(wire_str(&ProviderEntry::default()), "—", "a Codex provider has no wire");
            // a provider's new id carries its models; openai and a taken id are refused
            reg.models.push(ModelEntry { alias: "g".into(), provider: "gw".into(), model: "m".into(), ..Default::default() });
            set_prov_cell(&mut reg, 0, PROV_ID_COL, "gateway").unwrap();
            assert_eq!(reg.get("g").map(|m| m.provider.as_str()), Some("gateway"));
            assert!(set_prov_cell(&mut reg, 0, PROV_ID_COL, "openai").unwrap_err().contains("Codex"));
        });
    }

    /// `/models` has a vision column: ◉ (y) for a model that takes images, · (-) for one that does
    /// not; space, ⏎ or +/- on it flips it, a typed y/n sets it, and the note is the last column.
    #[test]
    fn the_models_grid_has_a_vision_column_that_toggles() {
        crate::web::snapshot::tests::with_mantra_home(|| {
            let mut app = crate::web::snapshot::tests::test_app();
            app.registry.models.insert(0, ModelEntry { alias: "local".into(), model: "qwen".into(), note: "on my box".into(), ..Default::default() });
            assert_eq!((MODEL_COLS[VISION_COL], MODEL_COLS[NOTE_COL]), ("vision", "note / test"));
            let (lines, _) = screen(&mut app, 220, 30);
            assert!(lines.iter().any(|l| l.contains("vision") && l.contains("note / test")), "{}", lines.join("\n"));
            let row = |alias: &str| lines.iter().find(|l| l.contains(&format!(" {alias} "))).cloned().unwrap_or_default();
            let (on, off) = (theme::g("◉", "y"), theme::g("·", "-"));
            assert!(row("local").contains(&format!(" {off} ")) && row("local").contains("on my box"), "{}", row("local"));
            assert!(row("astra").contains(&format!(" {on} ")), "the built-in aliases see: {}", row("astra"));
            app.models_ui.row = 0;
            app.models_ui.col = VISION_COL;
            press(&mut app, KeyCode::Enter);
            assert!(app.registry.models[0].vision);
            assert!(app.overlays.is_empty() && app.edit.is_none(), "a yes/no opens no editor");
            press(&mut app, KeyCode::Char('-'));
            assert!(!app.registry.models[0].vision);
            press(&mut app, KeyCode::Char('y'));
            assert!(app.registry.models[0].vision);
            app.models_ui.col = NOTE_COL;
            press(&mut app, KeyCode::Enter);
            assert_eq!(app.edit.as_ref().map(|e| e.text()), Some("on my box".into()), "⏎ on the note edits it");
        });
    }

    /// The selected row is a band across the whole inner width of the grid — the cells at its
    /// right end (efforts, vision, note) are on it too — and only that row is.
    #[test]
    fn the_highlight_band_spans_the_models_grid() {
        let mut app = crate::web::snapshot::tests::test_app();
        app.models_ui.row = 1;
        app.models_ui.col = ALIAS_COL;
        let (lines, buf) = screen(&mut app, 200, 30);
        let y = lines.iter().position(|l| l.contains(" sol ") && l.contains("gpt-")).expect("sol's row") as u16;
        let band = theme::row_sel().bg.unwrap();
        assert!((1..199).all(|x| buf[(x, y)].bg == band), "from border to border");
        assert_ne!(buf[(0, y)].bg, band, "not over the border");
        let other = lines.iter().position(|l| l.contains(" terra ")).unwrap() as u16;
        assert!((1..199).all(|x| buf[(x, other)].bg != band), "the other rows have none");
        // the border, a space, then the alias cell
        assert_eq!(buf[(2, y)].symbol(), "s");
        assert!(buf[(2, y)].modifier.contains(Modifier::BOLD), "the selected cell is bold on top of the band");
        assert!(!buf[(30, y)].modifier.contains(Modifier::BOLD), "the rest of the row is not");
    }
}
