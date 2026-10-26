//! Pattern Studio: roles, flow and settings of a pattern, edited in place (`ui::form`) — a commit
//! validates the whole pattern and saves it when it is valid — plus an architect agent that edits
//! it from a description.

use super::{theme, *};
use crate::app::{App, EditTarget, StudioSel};
use crate::engine::pattern::{FinaleStep, PatternSettings, Role, COLORS, EFFORT_AUTO, KINDS, PARALLEL_RANGE, PERMISSIONS};
use crate::ui::form::{self, Edit, Outcome, Start};
use crate::ui::input::Act;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Constraint, Direction, Layout};

const GLYPHS: &[&str] = &["✦", "◉", "◇", "◆", "◎", "▲", "■", "●", "★", "◈", "▣", "⬡", "♦", "▼"];
/// Sane upper bound (24h) for `watchdog_seconds`/`watchdog_escalate_seconds`: keeps
/// `es_secs.saturating_mul(2)` in `Run::watchdog_tick` well clear of `u64::MAX` and rules out a
/// typo (or a pasted huge number) leaving the watchdog effectively disabled.
const WATCHDOG_SECONDS_MAX: u64 = 24 * 3600;
/// Floors for `watchdog_seconds`/`watchdog_escalate_seconds`, shared by the nudge (+/-) and
/// free-text edit paths so the two don't disagree on what's a valid value.
const WATCHDOG_SECONDS_MIN: u64 = 15;
const WATCHDOG_ESCALATE_MIN: u64 = 30;
const SANDBOXES: &[&str] = &["read-only", "workspace-write", "danger-full-access"];
const ROLE_FIELDS: &[&str] = &["kind", "glyph", "color", "model", "effort", "fallback", "sandbox", "permission", "swarm", "parallel", "max_tokens", "mcp", "description", "instructions"];
/// The pattern's own fields, first in its settings entry: a new name saves a copy under it.
const PATTERN_FIELDS: &[&str] = &["name", "description"];
const SETTING_FIELDS: &[&str] = &["isolation", "network", "writable_roots", "max_parallel", "worker_retries", "review_plan", "orchestrator_context", "stall_minutes", "gate_max_rounds", "check_timeout_secs", "max_tasks_per_phase", "watchdog_seconds", "watchdog_escalate_seconds", "review_minutes", "manager_minutes", "first_token_minutes", "context_cap"];
const ADD_STEP: &str = "+ add finale step";

/// How a field takes keys (`ui::form`'s rules).
#[derive(Clone, Copy, PartialEq, Debug)]
enum Kind {
    /// Typed in place: a printable key overwrites, ⏎ edits, Backspace clears.
    Text,
    /// Typed in place over several lines (a role's instructions, a finale step's task).
    Lines,
    /// Typed (a number, a glyph), or stepped with ←→.
    Step,
    /// One of a list: ←→, space and ⏎ step through it, a letter jumps.
    Choice,
    /// On or off: ←→, space and ⏎ flip it.
    Toggle,
    /// A row that does something on ⏎ (`+ add finale step`).
    Action,
}

fn field_kind(entry: &str, field: &str) -> Kind {
    match entry {
        "⚙ settings" => match field {
            "name" | "description" | "writable_roots" => Kind::Text,
            "isolation" | "orchestrator_context" => Kind::Choice,
            "network" | "review_plan" => Kind::Toggle,
            _ => Kind::Step,
        },
        "⇢ flow" => match field {
            ADD_STEP => Kind::Action,
            f if f.ends_with(".task") => Kind::Lines,
            f if f.ends_with(".may_spawn") => Kind::Toggle,
            _ => Kind::Choice,
        },
        _ => match field {
            "instructions" => Kind::Lines,
            "description" | "mcp" => Kind::Text,
            "swarm" => Kind::Toggle,
            "glyph" | "parallel" | "max_tokens" => Kind::Step,
            _ => Kind::Choice,
        },
    }
}

/// Entries in the left list: roles…, settings, flow
fn entries(app: &App) -> Vec<String> {
    let mut v: Vec<String> = app.studio.pattern.ordered_roles().into_iter().map(|(n, _)| n).collect();
    v.push("⚙ settings".into());
    v.push("⇢ flow".into());
    v
}

fn sel_for_entry(e: &str) -> StudioSel {
    match e {
        "⚙ settings" => StudioSel::Settings,
        "⇢ flow" => StudioSel::Flow,
        name => StudioSel::Role(name.to_string()),
    }
}

/// `app.studio.sel`'s position in the current (re-sorted) list — for drawing and for ↑/↓, which
/// step by index but must land back on an identity so a later re-sort can't move the highlight.
fn sel_index(app: &App) -> usize {
    let es = entries(app);
    let i = match &app.studio.sel {
        StudioSel::Settings => es.iter().position(|e| e == "⚙ settings"),
        StudioSel::Flow => es.iter().position(|e| e == "⇢ flow"),
        StudioSel::Role(name) => es.iter().position(|e| e == name),
    };
    i.unwrap_or(0).min(es.len().saturating_sub(1))
}

/// Select the `i`th entry of the current list by identity.
fn select_by_index(app: &mut App, i: usize) {
    let es = entries(app);
    if es.is_empty() {
        return;
    }
    let i = i.min(es.len() - 1);
    app.studio.sel = sel_for_entry(&es[i]);
}

fn flow_fields(app: &App) -> Vec<String> {
    let mut v = vec!["planner".to_string(), "manager".into(), "orchestrator".into(), "phase_gate".into(), "on_reprompt".into()];
    for i in 0..app.studio.pattern.flow.finale.len() {
        v.push(format!("finale[{i}].role"));
        v.push(format!("finale[{i}].task"));
        v.push(format!("finale[{i}].may_spawn"));
    }
    v.push(ADD_STEP.into());
    v
}

fn fields(app: &App, entry: &str) -> Vec<String> {
    match entry {
        "⚙ settings" => PATTERN_FIELDS.iter().chain(SETTING_FIELDS).map(|s| s.to_string()).collect(),
        "⇢ flow" => flow_fields(app),
        role => {
            // only workers run many at a time: on any other kind `parallel` would be a dead row
            let worker = app.studio.pattern.roles.get(role).map(|r| r.kind == "worker").unwrap_or(false);
            ROLE_FIELDS.iter().filter(|f| worker || **f != "parallel").map(|s| s.to_string()).collect()
        }
    }
}

/// The finale step a `finale[i].…` field is about.
fn finale_index(field: &str) -> Option<usize> {
    field.strip_prefix("finale[")?.split(']').next()?.parse().ok()
}

fn get_value(app: &App, entry: &str, field: &str) -> String {
    let p = &app.studio.pattern;
    match entry {
        "⚙ settings" => {
            let s = &p.settings;
            match field {
                "name" => p.name.clone(),
                "description" => p.description.clone(),
                "isolation" => s.isolation.clone(),
                "network" => if s.network { "on".into() } else { "off".into() },
                "writable_roots" => if s.writable_roots.is_empty() { "none".into() } else { s.writable_roots.join(", ") },
                "max_parallel" => s.max_parallel.to_string(),
                "worker_retries" => s.worker_retries.to_string(),
                "review_plan" => if s.review_plan { "on".into() } else { "off".into() },
                "orchestrator_context" => s.orchestrator_context.clone(),
                "stall_minutes" => s.stall_minutes.to_string(),
                "gate_max_rounds" => s.gate_max_rounds.to_string(),
                "check_timeout_secs" => s.check_timeout_secs.to_string(),
                "max_tasks_per_phase" => s.max_tasks_per_phase.to_string(),
                "watchdog_seconds" => s.watchdog_seconds.to_string(),
                "watchdog_escalate_seconds" => s.watchdog_escalate_seconds.to_string(),
                "review_minutes" => if s.review_minutes == 0 { "off".into() } else { s.review_minutes.to_string() },
                "manager_minutes" => if s.manager_minutes == 0 { "off (escalations only)".into() } else { s.manager_minutes.to_string() },
                "first_token_minutes" => if s.first_token_minutes == 0 { "off".into() } else { s.first_token_minutes.to_string() },
                "context_cap" => if s.context_cap == 0 { "0 (the model's full window)".into() } else { s.context_cap.to_string() },
                _ => String::new(),
            }
        }
        "⇢ flow" => {
            let f = &p.flow;
            match field {
                "planner" => f.planner.clone(),
                "manager" => if f.manager.trim().is_empty() { "none".into() } else { f.manager.clone() },
                "orchestrator" => f.orchestrator.clone(),
                "phase_gate" => f.phase_gate.clone(),
                "on_reprompt" => f.on_reprompt.clone(),
                x if x.starts_with("finale[") => {
                    let st = finale_index(x).and_then(|i| f.finale.get(i)).cloned().unwrap_or_default();
                    if x.ends_with(".role") {
                        st.role
                    } else if x.ends_with(".task") {
                        st.task
                    } else if st.may_spawn {
                        "on".into()
                    } else {
                        "off".into()
                    }
                }
                _ => String::new(),
            }
        }
        role => {
            let Some(r) = p.roles.get(role) else { return String::new() };
            match field {
                "kind" => r.kind.clone(),
                "glyph" => r.glyph.clone(),
                "color" => r.color.clone(),
                "model" => {
                    let m = app.registry.resolve(&r.model);
                    if r.model.is_empty() {
                        String::new()
                    } else {
                        format!("{} · {} · via {}", r.model, m.model, app.registry.provider_name(&m.provider))
                    }
                }
                "effort" => r.effort.clone(),
                "fallback" => r.fallback.clone().unwrap_or_else(|| "none".into()),
                "sandbox" => r.sandbox.clone(),
                "permission" => r.permission.clone(),
                "swarm" => if r.can_spawn_swarm { "on".into() } else { "off".into() },
                "parallel" => r.parallel.map(|n| n.to_string()).unwrap_or_else(|| "none".into()),
                "max_tokens" => r.max_tokens.map(|t| t.to_string()).unwrap_or_else(|| "none".into()),
                "mcp" => if r.mcp.is_empty() { "none".into() } else { r.mcp.join(", ") },
                "description" => r.description.clone(),
                "instructions" => r.instructions.clone(),
                _ => String::new(),
            }
        }
    }
}

/// What an edit of the field starts from: its value, with the words that stand for "nothing"
/// (`none`, a cap of `0 (the model's full window)`) left out — they are how empty reads, not text
/// to edit around.
fn edit_value(app: &App, entry: &str, field: &str) -> String {
    let v = get_value(app, entry, field);
    match field {
        "max_tokens" | "parallel" | "writable_roots" | "mcp" if v == "none" => String::new(),
        "context_cap" if app.studio.pattern.settings.context_cap == 0 => "0".into(),
        "manager_minutes" if app.studio.pattern.settings.manager_minutes == 0 => "off".into(),
        _ => v,
    }
}

/// The options of a choice field, as its value reads (`none` for no fallback or no manager).
fn choices(app: &App, entry: &str, field: &str) -> Vec<String> {
    let p = &app.studio.pattern;
    let aliases = || app.registry.models.iter().map(|m| m.alias.clone()).collect::<Vec<_>>();
    let roles = || p.roles.keys().cloned().collect::<Vec<_>>();
    let list = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    match entry {
        "⚙ settings" => match field {
            "isolation" => list(&["auto", "worktree", "shared"]),
            "orchestrator_context" => list(&["fresh", "compact"]),
            _ => vec![],
        },
        "⇢ flow" => match field {
            // the manager is optional: none, or a role of kind manager
            "manager" => std::iter::once("none".to_string()).chain(p.roles.iter().filter(|(_, r)| r.kind == "manager").map(|(n, _)| n.clone())).collect(),
            _ => roles(),
        },
        role => {
            let Some(r) = p.roles.get(role) else { return vec![] };
            match field {
                "kind" => list(KINDS),
                "color" => list(COLORS),
                "model" => aliases(),
                // "auto" leads: Mantra picks the effort per spawn from the role's job
                "effort" => std::iter::once(EFFORT_AUTO.to_string()).chain(app.registry.resolve(&r.model).efforts()).collect(),
                // none leads, then every model alias but the role's own
                "fallback" => std::iter::once("none".to_string()).chain(aliases().into_iter().filter(|a| *a != r.model)).collect(),
                "sandbox" => list(SANDBOXES),
                "permission" => list(PERMISSIONS),
                _ => vec![],
            }
        }
    }
}

/// A choice field's value as `choices` lists it.
fn choice_value(app: &App, entry: &str, field: &str) -> String {
    match (entry, field) {
        ("⇢ flow", "manager") => get_value(app, entry, field),
        (role, "model") => app.studio.pattern.roles.get(role).map(|r| r.model.clone()).unwrap_or_default(),
        _ => get_value(app, entry, field),
    }
}

/// Sets a choice field to `v`, one of its `choices`. A new model re-clamps a named effort to what
/// it offers (`auto` stays auto).
fn set_choice(app: &mut App, entry: &str, field: &str, v: &str) {
    let reg = app.registry.clone();
    let p = &mut app.studio.pattern;
    match entry {
        "⚙ settings" => match field {
            "isolation" => p.settings.isolation = v.into(),
            "orchestrator_context" => p.settings.orchestrator_context = v.into(),
            _ => return,
        },
        "⇢ flow" => {
            let f = &mut p.flow;
            match field {
                "planner" => f.planner = v.into(),
                "manager" => f.manager = if v == "none" { String::new() } else { v.into() },
                "orchestrator" => f.orchestrator = v.into(),
                "phase_gate" => f.phase_gate = v.into(),
                "on_reprompt" => f.on_reprompt = v.into(),
                x if x.ends_with(".role") => match finale_index(x).and_then(|i| f.finale.get_mut(i)) {
                    Some(st) => st.role = v.into(),
                    None => return,
                },
                _ => return,
            }
        }
        role => {
            let Some(r) = p.roles.get_mut(role) else { return };
            match field {
                "swarm" => r.can_spawn_swarm = !r.can_spawn_swarm,
                "kind" => r.kind = v.into(),
                "color" => r.color = v.into(),
                "model" => {
                    r.model = v.into();
                    if r.effort != EFFORT_AUTO {
                        r.effort = reg.resolve(&r.model).resolve_effort(&r.effort);
                    }
                }
                "effort" => r.effort = v.into(),
                "fallback" => r.fallback = if v == "none" { None } else { Some(v.into()) },
                "sandbox" => r.sandbox = v.into(),
                "permission" => r.permission = v.into(),
                _ => return,
            }
        }
    }
    app.studio.dirty = true;
}

/// Enforces the floors and the `escalate > seconds` invariant that `Pattern::validate` requires
/// (`engine/pattern.rs`), so neither the nudge (+/-) nor the free-text edit path can leave the
/// two watchdog fields in a combination validate() would only reject later at save time.
fn clamp_watchdog(s: &mut PatternSettings) {
    s.watchdog_seconds = s.watchdog_seconds.clamp(WATCHDOG_SECONDS_MIN, WATCHDOG_SECONDS_MAX);
    s.watchdog_escalate_seconds = s.watchdog_escalate_seconds.clamp(WATCHDOG_ESCALATE_MIN, WATCHDOG_SECONDS_MAX);
    if s.watchdog_escalate_seconds <= s.watchdog_seconds {
        s.watchdog_escalate_seconds = (s.watchdog_seconds + 1).min(WATCHDOG_SECONDS_MAX);
    }
}

/// ←/→ on a field: the next / previous choice, a number stepped, an on/off flipped.
fn nudge(app: &mut App, entry: &str, field: &str, d: i32) {
    if field_kind(entry, field) == Kind::Choice {
        let next = form::cycle(&choices(app, entry, field), &choice_value(app, entry, field), d);
        return set_choice(app, entry, field, &next);
    }
    let p = &mut app.studio.pattern;
    let step_u = |v: &mut usize, lo: usize| *v = ((*v as i64 + d as i64).max(lo as i64)) as usize;
    match entry {
        "⚙ settings" => {
            let s = &mut p.settings;
            match field {
                "network" => s.network = !s.network,
                "max_parallel" => step_u(&mut s.max_parallel, 1),
                "worker_retries" => s.worker_retries = (s.worker_retries as i32 + d).max(0) as u32,
                "review_plan" => s.review_plan = !s.review_plan,
                "stall_minutes" => s.stall_minutes = (s.stall_minutes as i64 + d as i64).max(1) as u64,
                "gate_max_rounds" => s.gate_max_rounds = (s.gate_max_rounds as i32 + d).max(1) as u32,
                "check_timeout_secs" => s.check_timeout_secs = (s.check_timeout_secs as i64 + 60 * d as i64).max(60) as u64,
                "max_tasks_per_phase" => step_u(&mut s.max_tasks_per_phase, 1),
                "watchdog_seconds" => {
                    s.watchdog_seconds = (s.watchdog_seconds as i64 + 15 * d as i64).clamp(WATCHDOG_SECONDS_MIN as i64, WATCHDOG_SECONDS_MAX as i64) as u64;
                    clamp_watchdog(s);
                }
                "watchdog_escalate_seconds" => {
                    s.watchdog_escalate_seconds = (s.watchdog_escalate_seconds as i64 + 30 * d as i64).clamp(WATCHDOG_ESCALATE_MIN as i64, WATCHDOG_SECONDS_MAX as i64) as u64;
                    clamp_watchdog(s);
                }
                "review_minutes" => s.review_minutes = (s.review_minutes as i64 + d as i64).clamp(0, 240) as u64,
                "manager_minutes" => s.manager_minutes = (s.manager_minutes as i64 + d as i64).clamp(0, 240) as u64,
                "first_token_minutes" => s.first_token_minutes = (s.first_token_minutes as i64 + d as i64).clamp(0, 240) as u64,
                "context_cap" => s.context_cap = (s.context_cap as i64 + 25_000 * d as i64).clamp(0, 4_000_000) as u64,
                _ => return,
            }
        }
        "⇢ flow" => match finale_index(field).and_then(|i| p.flow.finale.get_mut(i)) {
            Some(st) if field.ends_with(".may_spawn") => st.may_spawn = !st.may_spawn,
            _ => return,
        },
        role => {
            let Some(r) = p.roles.get_mut(role) else { return };
            match field {
                "glyph" => r.glyph = form::cycle(GLYPHS, &r.glyph, d),
                "swarm" => r.can_spawn_swarm = !r.can_spawn_swarm,
                // none (shares max_parallel) sits below 1, and 128 is the top
                "parallel" => {
                    let next = r.parallel.unwrap_or(0) as i64 + d as i64;
                    r.parallel = if next <= 0 { None } else { Some((next as usize).min(*PARALLEL_RANGE.end())) };
                }
                "max_tokens" => {
                    let cur = r.max_tokens.unwrap_or(0) as i64;
                    let next = cur + d as i64 * 250_000;
                    r.max_tokens = if next <= 0 { None } else { Some(next as u64) };
                }
                _ => return,
            }
        }
    }
    app.studio.dirty = true;
}

/// A typed field into the pattern (the Studio's `EditTarget`s). A value that cannot be one is
/// said or left as it was; the pattern as a whole is validated when it is saved.
pub fn apply_edit(app: &mut App, target: &EditTarget, text: &str) {
    match target {
        // a number out of range is said, not swallowed: the field keeps what it had
        EditTarget::RoleField(role, field) if field == "parallel" => match parse_role_parallel(text) {
            Ok(n) => {
                if let Some(r) = app.studio.pattern.roles.get_mut(role) {
                    r.parallel = n;
                    app.studio.dirty = true;
                }
            }
            Err(e) => app.toast(format!("parallel: {e}"), crate::agent::Level::Warn),
        },
        EditTarget::RoleField(role, field) => {
            if let Some(r) = app.studio.pattern.roles.get_mut(role) {
                match field.as_str() {
                    "description" => r.description = text.to_string(),
                    "instructions" => r.instructions = text.to_string(),
                    "glyph" => r.glyph = text.trim().chars().next().map(|c| c.to_string()).unwrap_or_default(),
                    "model" => r.model = text.to_string(),
                    "effort" => r.effort = text.to_string(),
                    "fallback" => r.fallback = parse_fallback(text),
                    "max_tokens" => r.max_tokens = text.replace(['_', ','], "").trim().parse().ok(),
                    "mcp" => r.mcp = parse_mcp(text),
                    _ => {}
                }
                app.studio.dirty = true;
            }
        }
        EditTarget::PatternField(field) if field == "description" => {
            app.studio.pattern.description = text.trim().to_string();
            app.studio.dirty = true;
        }
        EditTarget::Setting(field) => {
            let s = &mut app.studio.pattern.settings;
            let n: Option<u64> = text.trim().parse().ok();
            match (field.as_str(), n) {
                ("max_parallel", Some(v)) => s.max_parallel = v.max(1) as usize,
                ("worker_retries", Some(v)) => s.worker_retries = v as u32,
                ("stall_minutes", Some(v)) => s.stall_minutes = v.max(1),
                ("gate_max_rounds", Some(v)) => s.gate_max_rounds = v.max(1) as u32,
                ("check_timeout_secs", Some(v)) => s.check_timeout_secs = v.max(10),
                ("max_tasks_per_phase", Some(v)) => s.max_tasks_per_phase = v.max(1) as usize,
                ("watchdog_seconds", Some(v)) => {
                    s.watchdog_seconds = v.clamp(WATCHDOG_SECONDS_MIN, WATCHDOG_SECONDS_MAX);
                    clamp_watchdog(s);
                }
                ("watchdog_escalate_seconds", Some(v)) => {
                    s.watchdog_escalate_seconds = v.clamp(WATCHDOG_ESCALATE_MIN, WATCHDOG_SECONDS_MAX);
                    clamp_watchdog(s);
                }
                ("review_minutes", Some(v)) => s.review_minutes = v.min(240),
                ("review_minutes", None) if text.trim().eq_ignore_ascii_case("off") => s.review_minutes = 0,
                ("manager_minutes", Some(v)) => s.manager_minutes = v.min(240),
                ("manager_minutes", None) if text.trim().to_ascii_lowercase().starts_with("off") => s.manager_minutes = 0,
                ("writable_roots", _) => s.writable_roots = parse_roots(text),
                ("first_token_minutes", Some(v)) => s.first_token_minutes = v.min(240),
                ("first_token_minutes", None) if text.trim().eq_ignore_ascii_case("off") => s.first_token_minutes = 0,
                // `200000`, `200k`, `1m` (the role field's token syntax); off / full = no cap
                ("context_cap", _) => {
                    let t = text.trim();
                    if t.eq_ignore_ascii_case("off") || t.eq_ignore_ascii_case("full") {
                        s.context_cap = 0;
                    } else if let Some(v) = super::models::parse_tokens(text) {
                        s.context_cap = v;
                    }
                }
                _ => {}
            }
            app.studio.dirty = true;
        }
        EditTarget::FlowStep(i, field) => {
            if let Some(st) = app.studio.pattern.flow.finale.get_mut(*i) {
                if field == "task" {
                    st.task = text.to_string();
                } else if field == "role" {
                    st.role = text.to_string();
                }
                app.studio.dirty = true;
            }
        }
        _ => {}
    }
}

/// A role's `parallel` from the editor: a number of agents at once (`PARALLEL_RANGE`), or none —
/// "none" or an empty field — to share `max_parallel` with the other roles again.
fn parse_role_parallel(t: &str) -> Result<Option<usize>, String> {
    let t = t.trim();
    if t.is_empty() || t.eq_ignore_ascii_case("none") {
        return Ok(None);
    }
    crate::engine::pattern::parse_parallel(t).map(Some)
}

/// A role's fallback model from the editor: an alias, or nothing for "none" / an empty field.
fn parse_fallback(t: &str) -> Option<String> {
    let t = t.trim();
    if t.is_empty() || t.eq_ignore_ascii_case("none") {
        None
    } else {
        Some(t.to_string())
    }
}

/// A role's `mcp` from the editor: server names (or `*`), comma- or space-separated; "none" (or
/// nothing) clears it. A name is kept as typed: one not in settings.toml is reported at run start.
fn parse_mcp(t: &str) -> Vec<String> {
    if t.trim().eq_ignore_ascii_case("none") {
        return vec![];
    }
    let mut v: Vec<String> = vec![];
    for n in t.split([',', ' ', '\n']).map(str::trim).filter(|n| !n.is_empty()) {
        if !v.iter().any(|x| x == n) {
            v.push(n.to_string());
        }
    }
    v
}

/// `writable_roots` from the editor: comma- or line-separated paths; "none" (or nothing) clears it.
fn parse_roots(t: &str) -> Vec<String> {
    if t.trim().eq_ignore_ascii_case("none") {
        return vec![];
    }
    t.split([',', '\n']).map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()
}

/// The pattern validated and, when valid, saved — what every commit ends in. Invalid, it stays
/// unsaved ("● not saved" in the header, the first reason above the footer) until a later
/// commit or ctrl+s finds it valid. Returns whether it was saved.
fn save_pattern(app: &mut App) -> bool {
    match app.studio.pattern.validate() {
        Ok(()) => match app.studio.pattern.save() {
            Ok(_) => {
                app.studio.dirty = false;
                app.studio.saved = Some(std::time::Instant::now());
                app.pattern_name = app.studio.pattern.name.clone();
                true
            }
            Err(e) => {
                app.studio.dirty = true;
                app.toast(format!("could not save the pattern: {e}"), crate::agent::Level::Error);
                false
            }
        },
        Err(errs) => {
            app.studio.dirty = true;
            app.studio.errors = errs;
            false
        }
    }
}

/// A finished edit into the pattern, then saved. Returns false — the edit stays open — when it
/// cannot be taken at all: a new role's name that is taken, a copy's name that is refused.
fn commit(app: &mut App, e: &Edit) -> bool {
    let text = e.text();
    match &e.target {
        EditTarget::NewRole => {
            let name = crate::util::slug(&text);
            if name.is_empty() {
                return true;
            }
            if app.studio.pattern.roles.contains_key(&name) {
                app.toast(format!("there is a role {name} already"), crate::agent::Level::Warn);
                return false;
            }
            let model = app.settings.default_model.clone();
            app.studio.pattern.roles.insert(name.clone(), Role { description: "new role".into(), model, ..Default::default() });
            app.studio.sel = StudioSel::Role(name);
            app.studio.focus = 1;
            app.studio.field = 0;
            app.studio.dirty = true;
            save_pattern(app);
            true
        }
        EditTarget::PatternField(f) if f == "name" => {
            let name = crate::util::slug(&text);
            if name.is_empty() || name == app.studio.pattern.name {
                return true;
            }
            match app.studio.pattern.save_as(&name, &app.project) {
                Ok((copy, path)) => {
                    app.studio.pattern = copy;
                    app.studio.dirty = false;
                    app.studio.saved = Some(std::time::Instant::now());
                    app.pattern_name = name.clone();
                    app.toast(format!("saved a copy as {name} ({}) — new runs use it", home_rel(&path)), crate::agent::Level::Ok);
                    true
                }
                Err(errs) => {
                    app.toast(errs.first().cloned().unwrap_or_default(), crate::agent::Level::Warn);
                    false
                }
            }
        }
        target => {
            if e.changed() {
                apply_edit(app, target, text.trim_end());
                save_pattern(app);
            }
            true
        }
    }
}

// ───────────────────────────── studio ─────────────────────────────

pub fn draw_studio(f: &mut Frame, app: &mut App) {
    let area = f.area();
    // the architect gives up rows first on a short terminal, so the fields keep their room
    let arch_h = if area.height < 30 { 5 } else { 9 };
    let rows = Layout::default().direction(Direction::Vertical).constraints([Constraint::Length(1), Constraint::Min(8), Constraint::Length(arch_h), Constraint::Length(1)]).split(area);
    let errs = app.studio.pattern.validate().err().unwrap_or_default();
    app.studio.errors = errs.clone();
    let mut crumbs = vec![Span::styled(format!("  {} studio  ", theme::g("›", ">")), theme::faint()), Span::styled(app.studio.pattern.name.clone(), theme::bold(theme::fg(theme::VIOLET)))];
    if app.studio.dirty {
        crumbs.push(Span::styled(format!("  {} not saved", theme::g("●", "*")), theme::fg(theme::AMBER)));
    } else if app.studio.saved.is_some_and(|t| t.elapsed() < crate::app::SAVED_SHOWN) {
        crumbs.push(Span::styled(format!("  {} saved", theme::g("✓", "ok")), theme::fg(theme::GREEN)));
    }
    let right = if errs.is_empty() { vec![Span::styled(format!("{} valid ", theme::g("✓", "ok")), theme::fg(theme::GREEN))] } else { vec![Span::styled(format!("{} {} issue(s) ", theme::g("✗", "x"), errs.len()), theme::fg(theme::RED))] };
    header(f, rows[0], crumbs, right);

    let wide = rows[1].width >= 120;
    let cols = if wide {
        Layout::default().direction(Direction::Horizontal).constraints([Constraint::Length(34), Constraint::Min(40), Constraint::Length(44)]).split(rows[1])
    } else {
        Layout::default().direction(Direction::Horizontal).constraints([Constraint::Length(29), Constraint::Min(40)]).split(rows[1])
    };
    let es = entries(app);
    let sel = sel_index(app);
    let flash = anim::fade(app.studio.flash, 900);
    let new_role = app.edit.as_ref().filter(|e| e.target == EditTarget::NewRole);
    let mut cursor = None;
    // list: the selected entry is the band while the list has the focus, bold and accent always
    let lblk = block("roles", if flash > 0.0 { theme::SAFFRON } else if app.studio.focus == 0 { theme::VIOLET } else { theme::FAINT });
    let linner = lblk.inner(cols[0]);
    f.render_widget(lblk, cols[0]);
    let mut l = vec![];
    let mut band = None;
    for (i, e) in es.iter().enumerate() {
        let is = i == sel && new_role.is_none();
        let st = if is { theme::bold(theme::accent()) } else { theme::text() };
        match app.studio.pattern.roles.get(e) {
            Some(r) => l.push(Line::from(vec![
                Span::raw(" "),
                Span::styled(format!("{} ", theme::role_glyph(&r.glyph)), theme::bold(theme::fg(theme::named(&r.color)))),
                Span::styled(format!("{:<13}", trunc(e, 13)), st),
                // what the space, the glyph and the name leave, a column short of the border
                Span::styled(trunc(&format!("{}·{}", r.model, r.effort), (linner.width as usize).saturating_sub(17)), theme::faint()),
            ])),
            None => {
                if i > 0 && app.studio.pattern.roles.contains_key(&es[i - 1]) {
                    // the role being added (ctrl+n) is typed where it will be
                    if let Some(ed) = new_role {
                        let w = (linner.width as usize).saturating_sub(4);
                        let (shown, cur) = ed.view(w);
                        cursor = Some((linner.x + 3 + cur as u16, linner.y + l.len() as u16));
                        band = Some(l.len());
                        l.push(Line::from(vec![Span::styled(" + ", theme::bold(theme::accent())), Span::styled(format!("{shown:<w$}"), theme::bold(theme::text()).add_modifier(Modifier::UNDERLINED))]));
                    }
                    l.push(Line::default());
                }
                l.push(Line::from(vec![Span::raw(" "), Span::styled(e.clone(), st)]));
            }
        }
        if is && app.studio.focus == 0 {
            band = Some(l.len() - 1);
        }
    }
    f.render_widget(Paragraph::new(l), linner);
    if let Some(y) = band {
        form::band_line(f.buffer_mut(), linner, y, 0);
    }

    // fields
    let entry = es[sel].clone();
    let fs = fields(app, &entry);
    let fsel = app.studio.field.min(fs.len().saturating_sub(1));
    let fcol = if app.studio.focus == 1 { theme::VIOLET } else { theme::FAINT };
    let fblk = block(&entry, fcol);
    let finner = fblk.inner(cols[1]);
    f.render_widget(fblk, cols[1]);
    // name column as wide as the longest name (settings has a 25-char one); the value column gets
    // the box's inner width less the name, and on the selected row its marker too
    let nw = fs.iter().map(|f| w_of(f)).max().unwrap_or(0).max(19) + 1;
    let fw_all = (finner.width as usize).saturating_sub(2 + nw);
    let editing = app.edit.as_ref().filter(|e| e.target != EditTarget::NewRole);
    // the cursor of a field being edited, as (column, line of `fl`): the scroll is known only later
    let mut fcursor: Option<(u16, usize)> = None;
    let mut fl = vec![];
    if let Some(r) = app.studio.pattern.roles.get(&entry) {
        let dw = (finner.width as usize).saturating_sub(5 + w_of(&entry));
        fl.push(Line::from(vec![Span::styled(format!(" {} ", theme::role_glyph(&r.glyph)), theme::bold(theme::fg(theme::named(&r.color)))), Span::styled(entry.clone(), theme::bold(theme::text())), Span::styled(format!("  {}", trunc(&r.description, dw)), theme::dim())]));
        fl.push(Line::default());
    }
    // the selected field's line and the last line of what goes with it, which the scroll keeps in view
    let (mut sel_line, mut sel_end) = (0, 0);
    for (i, fname) in fs.iter().enumerate() {
        let v = get_value(app, &entry, fname);
        let kind = field_kind(&entry, fname);
        let is = i == fsel && app.studio.focus == 1;
        let st = if is { theme::bold(theme::accent()) } else { theme::muted() };
        if i == fsel {
            sel_line = fl.len();
        }
        let name = Span::styled(format!("  {fname:<nw$}"), st);
        match editing.filter(|_| is) {
            // several lines: the field opens under its name, as many lines as there is room for
            Some(e) if e.multiline => {
                fl.push(Line::from(vec![name, Span::styled("alt+⏎ new line · ⏎ saves", theme::faint())]));
                let w = (finner.width as usize).saturating_sub(4);
                let (lines, cr, cc) = e.input.layout(w);
                let room = (finner.height as usize).saturating_sub(6).clamp(3, 14);
                let start = cr.saturating_sub(room - 1);
                for (k, line) in lines.iter().enumerate().skip(start).take(room) {
                    if k == cr {
                        fcursor = Some((finner.x + 3 + cc as u16, fl.len()));
                    }
                    fl.push(Line::from(vec![Span::raw("   "), Span::styled(line.clone(), theme::text())]));
                }
            }
            Some(e) => {
                let (shown, cur) = e.view(fw_all);
                fcursor = Some((finner.x + 2 + nw as u16 + cur as u16, fl.len()));
                fl.push(Line::from(vec![name, Span::styled(format!("{shown:<fw_all$}"), theme::bold(theme::text()).add_modifier(Modifier::UNDERLINED))]));
            }
            None => {
                // what ←→ or ⏎ does on the selected field, after its value
                let mark = match kind {
                    _ if !is => String::new(),
                    Kind::Choice | Kind::Toggle | Kind::Step => format!(" {}", theme::g("◂ ▸", "< >")),
                    Kind::Text | Kind::Lines => format!(" {}", theme::g("⏎", "<enter>")),
                    Kind::Action => String::new(),
                };
                // the value gets what the marker after it leaves (` ⏎` is narrower than ` ◂ ▸`)
                let fw = fw_all.saturating_sub(w_of(&mark));
                let shown = if kind == Kind::Lines {
                    let first = v.lines().find(|l| !l.trim().is_empty()).unwrap_or("").to_string();
                    format!("{} ({} lines)", trunc(&first, fw.saturating_sub(12)), v.lines().count())
                } else {
                    trunc(&v, fw)
                };
                let vstyle = match fname.as_str() {
                    "effort" => theme::fg(effort_color(&v)),
                    "color" => theme::fg(theme::named(&v)),
                    ADD_STEP => if is { theme::bold(theme::accent()) } else { theme::muted() },
                    _ => theme::text(),
                };
                if kind == Kind::Action {
                    fl.push(Line::from(vec![Span::styled(format!("  {fname}"), vstyle)]));
                } else {
                    fl.push(Line::from(vec![name, Span::styled(shown, vstyle), Span::styled(mark, theme::faint())]));
                }
            }
        }
        // what the focused field means, right under it
        if let Some(h) = field_hint(fname).filter(|_| is && editing.is_none()) {
            let pad = || vec![Span::raw("   ")];
            fl.extend(md::wrap(vec![Span::styled(h, theme::faint())], (finner.width as usize).saturating_sub(3), pad(), pad()));
        }
        if i == fsel {
            sel_end = fl.len() - 1;
        }
    }
    fl.push(Line::default());
    let pad = || vec![Span::raw(" ")];
    if !wide && entry == "⚙ settings" {
        for g in groups_lines(&app.studio.pattern) {
            fl.extend(md::wrap(vec![Span::styled(g, theme::faint())], (finner.width as usize).saturating_sub(1), pad(), vec![Span::raw("   ")]));
        }
    }
    // invalid: why it is not saved, where the eye is
    if let Some(e) = errs.first().filter(|_| app.studio.dirty) {
        fl.extend(md::wrap(vec![Span::styled(format!("{} not saved: {e} — fix it (ctrl+s saves once it is valid)", theme::g("✗", "x")), theme::fg(theme::RED))], (finner.width as usize).saturating_sub(1), pad(), pad()));
    }
    // A list longer than the box (settings on a short terminal) scrolls to keep the selection in view.
    let fh = finner.height as usize;
    let scroll = (sel_end + 1).saturating_sub(fh).min(sel_line);
    if let Some((x, line)) = fcursor.filter(|(_, line)| *line >= scroll && *line - scroll < fh) {
        cursor = Some((x, finner.y + (line - scroll) as u16));
    }
    f.render_widget(Paragraph::new(fl).scroll((scroll as u16, 0)), finner);
    if app.studio.focus == 1 {
        form::band_line(f.buffer_mut(), finner, sel_line, scroll);
    }

    // flow preview + validation
    if wide {
        let p = &app.studio.pattern;
        let rg = |name: &str| -> Vec<Span<'static>> {
            match p.roles.get(name) {
                Some(r) => vec![Span::styled(format!("{} ", theme::role_glyph(&r.glyph)), theme::bold(theme::fg(theme::named(&r.color)))), Span::styled(name.to_string(), theme::text()), Span::styled(format!(" {}·{}", r.model, r.effort), theme::faint())],
                None => vec![Span::styled(format!("? {name}"), theme::fg(theme::RED))],
            }
        };
        let arrow = |t: &str| Line::from(Span::styled(format!("   {} {t}", theme::g("↓", "v")), theme::faint()));
        let mut pl = vec![];
        let row = |v: Vec<Span<'static>>| -> Line<'static> {
            let mut s = vec![Span::raw(" ")];
            s.extend(v);
            Line::from(s)
        };
        pl.push(row(rg(&p.flow.planner)));
        pl.push(arrow("writes the phased plan"));
        if let Some(m) = p.manager_role() {
            let mut v = rg(m);
            // short enough to stay on the role's line in the 120-column preview
            v.push(Span::styled(" oversees the run", theme::faint()));
            pl.push(row(v));
            pl.push(arrow("unsticks agents, resolves halts"));
        }
        pl.push(row(rg(&p.flow.orchestrator)));
        // the shared pool, then each worker class with a cap of its own (the swarm's)
        let own: Vec<String> = p.worker_roles().into_iter().filter_map(|w| p.roles[&w].parallel.map(|n| format!(" · {w} ≤{n}"))).collect();
        pl.push(arrow(&if own.is_empty() { format!("spawns ≤{} in parallel / phase", p.settings.max_parallel) } else { format!("spawns ≤{}{} / phase", p.settings.max_parallel, own.concat()) }));
        let mut ws = vec![];
        for w in p.worker_roles() {
            ws.extend(rg(&w));
            ws.push(Span::raw("  "));
        }
        // side by side while they fit, else one worker role per row
        if Line::from(ws.clone()).width() < cols[2].width.saturating_sub(2) as usize {
            pl.push(row(ws));
        } else {
            pl.extend(p.worker_roles().iter().map(|w| row(rg(w))));
        }
        pl.push(arrow(if p.settings.isolation == "shared" { "checks" } else { "merge + checks" }));
        pl.push(row(rg(&p.flow.phase_gate)));
        if !p.flow.finale.is_empty() {
            pl.push(arrow("after the last phase"));
            for s in &p.flow.finale {
                let mut v = rg(&s.role);
                if s.may_spawn {
                    v.push(Span::styled(" +spawn", theme::fg(theme::ROSE)));
                }
                pl.push(row(v));
            }
        }
        pl.push(Line::default());
        pl.push(Line::from(vec![Span::styled(" re-prompt → ", theme::faint()), Span::styled(p.flow.on_reprompt.clone(), theme::text())]));
        for g in groups_lines(p) {
            pl.push(Line::from(Span::styled(format!(" {g}"), theme::faint())));
        }
        pl.push(Line::default());
        for e in errs.iter().take(6) {
            pl.push(Line::from(Span::styled(format!(" {} {}", theme::g("✗", "x"), trunc(e, 42)), theme::fg(theme::RED))));
        }
        // a row wider than the column (the manager's note, three or more worker roles) wraps
        // under itself instead of being cut at the border
        let pw = cols[2].width.saturating_sub(2) as usize;
        let pl: Vec<Line> = pl
            .into_iter()
            .flat_map(|l| if l.width() <= pw { vec![l] } else { md::wrap(l.spans, pw, vec![], vec![Span::raw("   ")]) })
            .collect();
        f.render_widget(Paragraph::new(pl).block(block("flow", theme::FAINT)), cols[2]);
    }

    // architect
    let acol = if app.studio.focus == 2 { theme::SAFFRON } else { theme::FAINT };
    let blk = block(&format!("{} architect — describe a change, it edits the pattern live", theme::g("★", "*")), acol);
    let inner = blk.inner(rows[2]);
    f.render_widget(blk, rows[2]);
    let mut al: Vec<Line> = vec![];
    if let Some(a) = app.studio.architect.and_then(|a| app.agents.get(&a)) {
        for t in a.tail_lines(inner.height.saturating_sub(2) as usize) {
            al.push(Line::from(Span::styled(format!(" {}", trunc(&t, (inner.width as usize).saturating_sub(2))), theme::dim())));
        }
        if let Some(b) = busy_line(a) {
            al.push(b);
        }
    } else {
        al.push(Line::from(Span::styled(" e.g. \"add a docs writer after security\" · \"make workers cheaper\" · \"two QA gates per phase\"", theme::faint())));
    }
    let keep = inner.height.saturating_sub(1) as usize;
    let start = al.len().saturating_sub(keep);
    f.render_widget(Paragraph::new(al[start..].to_vec()), Rect { height: inner.height.saturating_sub(1), ..inner });
    let ir = Rect { y: inner.y + inner.height.saturating_sub(1), height: 1, ..inner };
    let (rws, _, cc) = app.studio.input.layout(ir.width.saturating_sub(3) as usize);
    let txt = rws.last().cloned().unwrap_or_default();
    let shown = if app.studio.input.is_empty() && app.studio.focus != 2 { Span::styled("tab here to talk to the architect", theme::faint()) } else { Span::styled(txt, theme::text()) };
    f.render_widget(Paragraph::new(Line::from(vec![Span::styled(format!("{} ", theme::g("›", ">")), theme::bold(theme::accent())), shown])), ir);
    if app.studio.focus == 2 {
        f.set_cursor_position((ir.x + 2 + cc as u16, ir.y));
    } else if let Some(c) = cursor {
        f.set_cursor_position(c);
    }
    let hints = studio_footer(app, &entry, fs.get(fsel).map(String::as_str).unwrap_or(""));
    footer(f, rows[3], &hints.iter().map(|(k, d)| (*k, d.as_str())).collect::<Vec<_>>());
}

/// The footer for what the Studio is doing: editing, a Delete armed, or the keys of the focused
/// pane — eight at the most.
fn studio_footer(app: &App, entry: &str, field: &str) -> Vec<(&'static str, String)> {
    if let Some(e) = &app.edit {
        return if e.multiline {
            vec![("⏎", "save".into()), ("alt+⏎", "new line".into()), ("esc", "revert".into())]
        } else if e.target == EditTarget::NewRole {
            vec![("⏎", "add the role".into()), ("esc", "cancel".into())]
        } else {
            vec![("⏎", "save".into()), ("esc", "revert".into()), ("↑↓", "save and move".into())]
        };
    }
    if let Some(what) = form::armed(&app.confirm) {
        return vec![("del", format!("again to delete {what}")), ("esc", "keep it".into())];
    }
    match app.studio.focus {
        2 => vec![("⏎", "send to the architect".into()), ("tab", "focus".into()), ("esc", "back to the list".into())],
        0 => vec![("↑↓", "select".into()), ("⏎ →", "its fields".into()), ("ctrl+n", "new role".into()), ("del", "delete".into()), ("tab", "focus".into()), ("ctrl+s", "save".into()), ("esc", "back".into())],
        _ => {
            let edit = match field_kind(entry, field) {
                Kind::Choice | Kind::Toggle => ("←→", "change"),
                Kind::Step => ("type ←→", "change"),
                Kind::Action => ("⏎", "add"),
                _ => ("type ⏎", "edit"),
            };
            vec![(edit.0, edit.1.into()), ("↑↓", "field".into()), ("ctrl+n", "new role".into()), ("del", "delete".into()), ("tab", "focus".into()), ("ctrl+s", "save".into()), ("esc", "back".into())]
        }
    }
}

/// The pattern's Akasha groups, one line each — "groups: builders (worker-big, worker-small) · 16k
/// · push urgent", the size in k of 1024 as the block's own header reads it. Read-only: groups are edited in the pattern's file (or by the architect) for
/// now; the line is here so a pattern that has them never looks as if it had none.
fn groups_lines(p: &crate::engine::pattern::Pattern) -> Vec<String> {
    p.groups
        .iter()
        .enumerate()
        .map(|(i, g)| {
            let auto = if g.auto { "" } else { " · no auto entries" };
            format!("{}{} ({}) · {} · push {}{auto}", if i == 0 { "groups: " } else { "        " }, g.name, g.roles.join(", "), crate::engine::akasha::fmt_k(g.shared_chars), g.push)
        })
        .collect()
}

/// One line on what the focused field means, for the fields that need more than their name.
fn field_hint(field: &str) -> Option<&'static str> {
    Some(match field {
        "name" => "A new name saves a copy of this pattern under it, and new runs use the copy.",
        "effort" => "auto = Mantra picks per spawn: medium to plan, supervise and gate, low for small and swarm workers, high for finale steps.",
        "parallel" => "How many workers of this role run at once, counted apart from the rest (1-128; worker-swarm has 16). none = it shares the pattern's max_parallel with the other roles without one.",
        "fallback" => "The model that takes over when this role's model twice produces nothing within first_token_minutes. none = the manager is asked instead.",
        "permission" => "never keeps the sandbox and skips prompts. BYPASS-SANDBOX-DANGER skips prompts AND the sandbox. on-request / untrusted asks you (ctrl+g).",
        "swarm" => "Lets this role start a temporary swarm in Solo. Swarm children cannot spawn another swarm.",
        "network" => "on = workspace-write agents may use the network (package managers, docs).",
        "writable_roots" => "Extra directories sandboxed agents may write, comma-separated (~ = your home). Toolchain caches are added automatically.",
        "mcp" => "MCP servers this role's agents get, by their name in /settings (e.g. blender), comma-separated. * = every enabled one, none = no server.",
        "first_token_minutes" => "Minutes a turn may produce nothing before it is restarted; the second time the role's fallback model takes over. off = no deadline.",
        "context_cap" => "Tokens of context every agent is told it has, whatever its model's window (the smaller of the two): a huge window lets an agent cycle tools for hours. 0 = the model's full window; 200k is the built-in default.",
        _ => return None,
    })
}

/// Starts editing the selected field (`start`; a typed `c` for `Overwrite`).
fn begin(app: &mut App, entry: &str, field: &str, start: Start, c: Option<char>) {
    let target = match entry {
        "⚙ settings" if PATTERN_FIELDS.contains(&field) => EditTarget::PatternField(field.to_string()),
        "⚙ settings" => EditTarget::Setting(field.to_string()),
        "⇢ flow" => match finale_index(field) {
            Some(i) if field.ends_with(".task") => EditTarget::FlowStep(i, "task".into()),
            _ => return,
        },
        role => EditTarget::RoleField(role.to_string(), field.to_string()),
    };
    let value = edit_value(app, entry, field);
    let mut e = match (start, c) {
        (Start::Overwrite, Some(c)) => Edit::overwrite(target, &value, c),
        (s, _) => Edit::new(target, &value, s),
    };
    if field_kind(entry, field) == Kind::Lines {
        e = e.multiline();
    }
    app.edit = Some(e);
}

/// A key while a field (or a new role's name) is being edited.
fn edit_key(app: &mut App, k: KeyEvent) {
    let Some(e) = app.edit.as_mut() else { return };
    let outcome = e.key(k);
    match outcome {
        Outcome::Editing => {}
        Outcome::Revert => app.edit = None,
        Outcome::Commit | Outcome::CommitMove(_) => {
            let Some(e) = app.edit.take() else { return };
            if !commit(app, &e) {
                app.edit = Some(e);
                return;
            }
            if let Outcome::CommitMove(d) = outcome {
                let n = fields(app, &entries(app)[sel_index(app)]).len();
                app.studio.field = (app.studio.field as i64 + d as i64).clamp(0, n.saturating_sub(1) as i64) as usize;
            }
        }
    }
}

/// Delete on the list's role (or a finale step in the flow): the first press arms, the second
/// deletes and saves. A role the flow uses stays; one a group lists leaves the group with it.
fn delete(app: &mut App, entry: &str, field: Option<&str>) {
    if entry == "⇢ flow" {
        let Some(i) = field.and_then(finale_index) else { return };
        if form::confirm(&mut app.confirm, &format!("finale step {}", i + 1)) {
            app.studio.pattern.flow.finale.remove(i);
            app.studio.dirty = true;
            app.studio.field = 0;
            save_pattern(app);
        }
        return;
    }
    if !app.studio.pattern.roles.contains_key(entry) {
        return;
    }
    let fl = &app.studio.pattern.flow;
    let used = [&fl.planner, &fl.manager, &fl.orchestrator, &fl.phase_gate, &fl.on_reprompt].iter().any(|x| **x == entry) || fl.finale.iter().any(|s| s.role == entry);
    if used {
        return app.toast(format!("{entry} is used in the flow — change the flow first"), crate::agent::Level::Warn);
    }
    if !form::confirm(&mut app.confirm, entry) {
        return;
    }
    let i = sel_index(app);
    app.studio.pattern.roles.remove(entry);
    // Groups are read-only here, so a group left naming the deleted role would be an error the
    // Studio could never clear: the role leaves its group with it.
    let mut left = vec![];
    for g in app.studio.pattern.groups.iter_mut().filter(|g| g.roles.iter().any(|r| r == entry)) {
        g.roles.retain(|r| r != entry);
        left.push(g.name.clone());
    }
    if !left.is_empty() {
        app.toast(format!("deleted {entry}, and took it out of group {}", left.join(", ")), crate::agent::Level::Info);
    }
    app.studio.dirty = true;
    select_by_index(app, i.saturating_sub(1));
    save_pattern(app);
}

pub fn studio_key(app: &mut App, k: KeyEvent) {
    if app.edit.is_some() {
        return edit_key(app, k);
    }
    let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
    if k.code != KeyCode::Delete {
        app.confirm = None;
    }
    if ctrl && k.code == KeyCode::Char('s') {
        match app.studio.pattern.validate() {
            Ok(()) => {
                if save_pattern(app) {
                    let path = app.studio.pattern.source_dir.clone().unwrap_or_else(crate::config::patterns_dir).join(format!("{}.toml", crate::util::slug(&app.studio.pattern.name)));
                    app.toast(format!("saved {}", home_rel(&path)), crate::agent::Level::Ok);
                }
            }
            Err(errs) => app.toast(format!("fix first: {}", errs[0]), crate::agent::Level::Error),
        }
        return;
    }
    if ctrl && k.code == KeyCode::Char('n') {
        app.studio.focus = 0;
        app.edit = Some(Edit::new(EditTarget::NewRole, "", Start::Clear));
        return;
    }
    if k.code == KeyCode::Tab {
        app.studio.focus = (app.studio.focus + 1) % 3;
        return;
    }
    if k.code == KeyCode::BackTab {
        app.studio.focus = (app.studio.focus + 2) % 3;
        return;
    }
    if app.studio.focus == 2 {
        if k.code == KeyCode::Esc {
            app.studio.focus = 0;
            return;
        }
        if let Act::Submit = app.studio.input.key(k) {
            let t = app.studio.input.take();
            if !t.trim().is_empty() {
                app.studio_architect_send(t);
            }
        }
        return;
    }
    let es = entries(app);
    let entry = es[sel_index(app)].clone();
    let fs = fields(app, &entry);
    let field = fs.get(app.studio.field.min(fs.len().saturating_sub(1))).cloned();
    let list = app.studio.focus == 0;
    let kind = field.as_deref().map(|f| field_kind(&entry, f)).unwrap_or(Kind::Action);
    // a change to a field saves the pattern when it is valid
    let changed = |app: &mut App, d: i32| {
        if let Some(f) = &field {
            nudge(app, &entry, f, d);
            save_pattern(app);
        }
    };
    match k.code {
        _ if ctrl => {}
        KeyCode::Esc => app.leave_screen(),
        KeyCode::Up | KeyCode::Char('k') if list || k.code == KeyCode::Up => {
            if list {
                let i = sel_index(app);
                select_by_index(app, i.saturating_sub(1));
                app.studio.field = 0;
            } else {
                app.studio.field = app.studio.field.saturating_sub(1);
            }
        }
        KeyCode::Down | KeyCode::Char('j') if list || k.code == KeyCode::Down => {
            if list {
                let i = sel_index(app);
                select_by_index(app, (i + 1).min(es.len() - 1));
                app.studio.field = 0;
            } else {
                app.studio.field = (app.studio.field + 1).min(fs.len().saturating_sub(1));
            }
        }
        KeyCode::Right | KeyCode::Enter if list => app.studio.focus = 1,
        KeyCode::Delete => delete(app, &entry, field.as_deref()),
        _ if list => {}
        KeyCode::Left | KeyCode::Right if matches!(kind, Kind::Choice | Kind::Toggle | Kind::Step) => changed(app, if k.code == KeyCode::Left { -1 } else { 1 }),
        KeyCode::Char(c @ ('+' | '-')) if matches!(kind, Kind::Choice | Kind::Toggle) || (kind == Kind::Step && field.as_deref() != Some("glyph")) => changed(app, if c == '-' { -1 } else { 1 }),
        KeyCode::Char(' ') | KeyCode::Enter if matches!(kind, Kind::Choice | Kind::Toggle) => changed(app, 1),
        KeyCode::Enter if kind == Kind::Action => {
            let first_gate = app.studio.pattern.roles.iter().find(|(_, r)| r.kind == "gate").map(|(n, _)| n.clone()).unwrap_or_default();
            app.studio.pattern.flow.finale.push(FinaleStep { role: first_gate, task: "Describe what this step should do.".into(), may_spawn: false });
            app.studio.dirty = true;
            save_pattern(app);
        }
        KeyCode::Enter => {
            if let Some(f) = &field {
                begin(app, &entry, f, Start::Append, None);
            }
        }
        KeyCode::Backspace if matches!(kind, Kind::Text | Kind::Lines | Kind::Step) => {
            if let Some(f) = &field {
                begin(app, &entry, f, Start::Clear, None);
            }
        }
        KeyCode::Char(c) if kind == Kind::Choice && c.is_alphanumeric() => {
            if let Some(f) = &field {
                if let Some(v) = form::jump(&choices(app, &entry, f), &choice_value(app, &entry, f), c) {
                    set_choice(app, &entry, f, &v);
                    save_pattern(app);
                }
            }
        }
        // a number takes digits (and `200k`'s letters after them); a glyph any character
        KeyCode::Char(c) if kind == Kind::Step && !(c.is_ascii_digit() || field.as_deref() == Some("glyph")) => {}
        KeyCode::Char(c) if matches!(kind, Kind::Text | Kind::Lines | Kind::Step) && !c.is_control() => {
            if let Some(f) = &field {
                begin(app, &entry, f, Start::Overwrite, Some(c));
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(app: &mut App, code: KeyCode) {
        studio_key(app, KeyEvent::new(code, KeyModifiers::NONE));
    }

    fn typed(app: &mut App, s: &str) {
        for c in s.chars() {
            press(app, KeyCode::Char(c));
        }
    }

    fn on_field(app: &mut App, role: &str, field: &str) {
        app.studio.sel = StudioSel::Role(role.into());
        app.studio.focus = 1;
        app.studio.field = fields(app, role).iter().position(|f| f == field).expect("the field");
    }

    /// Groups show as one read-only line each; a pattern without them shows none.
    #[test]
    fn the_overview_lists_akasha_groups_read_only() {
        let mut p = crate::engine::pattern::Pattern { groups: vec![], ..crate::engine::pattern::Pattern::builtin() };
        assert!(groups_lines(&p).is_empty(), "every pattern from before groups");
        assert_eq!(groups_lines(&crate::engine::pattern::Pattern::builtin()), vec!["groups: builders (worker-big, worker-small, worker-swarm) · 16k · push urgent".to_string()], "the built-ins have one");
        let group = |name: &str, roles: &[&str], shared_chars: usize, push: &str, auto: bool| crate::engine::pattern::Group { name: name.into(), roles: roles.iter().map(|r| r.to_string()).collect(), shared_chars, push: push.into(), auto };
        p.groups = vec![group("builders", &["worker-big", "worker-small"], 16_384, "urgent", true), group("reviewers", &["qa"], 2_000, "none", false)];
        assert_eq!(groups_lines(&p), vec!["groups: builders (worker-big, worker-small) · 16k · push urgent".to_string(), "        reviewers (qa) · 2k · push none · no auto entries".to_string()]);
    }

    /// Deleting a role a group lists takes it out of the group too: groups are read-only here, so
    /// a group left naming it would keep the pattern from saving with an error the Studio cannot
    /// clear. Delete arms on the first press and deletes on the second.
    #[test]
    fn deleting_a_grouped_role_takes_it_out_of_its_group() {
        crate::web::snapshot::tests::with_mantra_home(|| {
            let mut app = crate::web::snapshot::tests::test_app();
            app.studio.pattern = crate::engine::pattern::Pattern::builtin();
            assert!(app.studio.pattern.groups[0].roles.contains(&"worker-small".to_string()));
            app.studio.focus = 0;
            app.studio.sel = StudioSel::Role("worker-small".into());
            press(&mut app, KeyCode::Delete);
            assert!(app.studio.pattern.roles.contains_key("worker-small"), "armed only");
            press(&mut app, KeyCode::Delete);
            assert!(!app.studio.pattern.roles.contains_key("worker-small"), "deleted");
            assert_eq!(app.studio.pattern.groups[0].roles, vec!["worker-big".to_string(), "worker-swarm".to_string()]);
            assert!(app.studio.pattern.validate().is_ok(), "{:?}", app.studio.pattern.validate());
            assert!(!app.studio.dirty, "and saved");
            let t = app.toast.as_ref().map(|t| t.0.clone()).unwrap_or_default();
            assert!(t.contains("worker-small") && t.contains("group builders"), "{t}");
            // a role no group lists is deleted as before, groups untouched
            app.studio.pattern.roles.insert("helper".into(), app.studio.pattern.roles["worker-big"].clone());
            app.studio.sel = StudioSel::Role("helper".into());
            app.toast = None;
            press(&mut app, KeyCode::Delete);
            press(&mut app, KeyCode::Delete);
            assert!(!app.studio.pattern.roles.contains_key("helper"));
            assert_eq!(app.studio.pattern.groups[0].roles, vec!["worker-big".to_string(), "worker-swarm".to_string()]);
            assert!(app.toast.is_none(), "nothing to say");
        });
    }

    /// Type to edit in the Studio: a letter replaces the field's text where it is, ⏎ validates
    /// and saves the pattern; a value that makes it invalid stays in it unsaved, with the reason
    /// on screen, until it is fixed. ←→ change a choice and save too.
    #[test]
    fn a_commit_saves_a_valid_pattern_and_keeps_an_invalid_one_unsaved() {
        crate::web::snapshot::tests::with_mantra_home(|| {
            let mut app = crate::web::snapshot::tests::test_app();
            app.studio.pattern = crate::engine::pattern::Pattern::builtin();
            on_field(&mut app, "worker-big", "description");
            typed(&mut app, "Big jobs");
            assert!(matches!(&app.edit, Some(e) if e.text() == "Big jobs" && e.target == EditTarget::RoleField("worker-big".into(), "description".into())));
            assert!(app.overlays.is_empty(), "no popup");
            press(&mut app, KeyCode::Enter);
            assert_eq!(app.studio.pattern.roles["worker-big"].description, "Big jobs");
            assert!(!app.studio.dirty);
            let saved = crate::engine::pattern::Pattern::load(crate::engine::pattern::NAME_DEFAULT, std::path::Path::new("/nonexistent")).unwrap();
            assert_eq!(saved.roles["worker-big"].description, "Big jobs", "saved on ⏎");
            // a planner that is not of kind planner makes the pattern invalid: the change stays in
            // the Studio, unsaved, and says why; the next change that makes it valid saves again
            app.studio.sel = StudioSel::Flow;
            app.studio.focus = 1;
            app.studio.field = 0;
            press(&mut app, KeyCode::Right);
            assert_ne!(app.studio.pattern.flow.planner, "planner");
            assert!(app.studio.dirty, "invalid, so not saved");
            let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 40)).unwrap();
            term.draw(|f| draw_studio(f, &mut app)).unwrap();
            let b = term.backend().buffer();
            let screen: String = (0..b.area.height).map(|y| (0..b.area.width).map(|x| b[(x, y)].symbol()).collect::<String>() + "\n").collect();
            assert!(screen.contains("not saved: flow.planner") && screen.contains("not saved"), "{screen}");
            assert_eq!(crate::engine::pattern::Pattern::load(crate::engine::pattern::NAME_DEFAULT, std::path::Path::new("/nonexistent")).unwrap().flow.planner, "planner", "the file keeps the valid one");
            press(&mut app, KeyCode::Left);
            assert!(!app.studio.dirty, "valid again: saved");
            on_field(&mut app, "worker-big", "sandbox");
            let before = app.studio.pattern.roles["worker-big"].sandbox.clone();
            press(&mut app, KeyCode::Right);
            assert_ne!(app.studio.pattern.roles["worker-big"].sandbox, before);
            assert!(!app.studio.dirty);
            // a letter on a choice jumps
            on_field(&mut app, "worker-big", "permission");
            typed(&mut app, "u");
            assert_eq!(app.studio.pattern.roles["worker-big"].permission, "untrusted");
            assert!(app.edit.is_none());
            on_field(&mut app, "worker-big", "swarm");
            assert_eq!(get_value(&app, "worker-big", "swarm"), "off");
            press(&mut app, KeyCode::Right);
            assert!(app.studio.pattern.roles["worker-big"].can_spawn_swarm);
            assert_eq!(get_value(&app, "worker-big", "swarm"), "on");
            assert!(field_hint("swarm").is_some());
        });
    }

    /// The pattern's name is the first field of its settings: a new name saves a copy under it
    /// and the Studio (and new runs) switch to the copy; a built-in's name is refused.
    #[test]
    fn a_new_name_saves_a_copy_and_switches_to_it() {
        crate::web::snapshot::tests::with_mantra_home(|| {
            let mut app = crate::web::snapshot::tests::test_app();
            app.studio.pattern = crate::engine::pattern::Pattern::builtin();
            app.studio.sel = StudioSel::Settings;
            app.studio.focus = 1;
            app.studio.field = 0;
            assert_eq!(fields(&app, "⚙ settings")[0], "name");
            typed(&mut app, "mine");
            press(&mut app, KeyCode::Enter);
            assert_eq!((app.studio.pattern.name.as_str(), app.pattern_name.as_str()), ("mine", "mine"));
            assert!(crate::engine::pattern::Pattern::path_for("mine").exists());
            assert!(app.toast.as_ref().is_some_and(|t| t.0.starts_with("saved a copy as mine")), "{:?}", app.toast);
            press(&mut app, KeyCode::Backspace);
            typed(&mut app, "mantra-lean");
            press(&mut app, KeyCode::Enter);
            assert!(app.toast.as_ref().is_some_and(|t| t.0.contains("built-in")), "{:?}", app.toast);
            assert!(app.edit.is_some(), "refused: still editing");
            assert_eq!(app.studio.pattern.name, "mine");
        });
    }

    /// ctrl+n types a new role's name into the list where it will be; ⏎ adds it on Solo's model
    /// and opens its fields, a name taken is refused, esc adds nothing.
    #[test]
    fn ctrl_n_adds_a_role_in_place() {
        crate::web::snapshot::tests::with_mantra_home(|| {
            let mut app = crate::web::snapshot::tests::test_app();
            app.studio.pattern = crate::engine::pattern::Pattern::builtin();
            studio_key(&mut app, KeyEvent::new(KeyCode::Char('n'), KeyModifiers::CONTROL));
            assert!(matches!(&app.edit, Some(e) if e.target == EditTarget::NewRole));
            typed(&mut app, "planner");
            press(&mut app, KeyCode::Enter);
            assert!(app.toast.as_ref().is_some_and(|t| t.0.contains("already")), "taken");
            press(&mut app, KeyCode::Esc);
            assert!(app.edit.is_none());
            studio_key(&mut app, KeyEvent::new(KeyCode::Char('n'), KeyModifiers::CONTROL));
            typed(&mut app, "Docs Writer");
            press(&mut app, KeyCode::Enter);
            let r = app.studio.pattern.roles.get("docs-writer").expect("added");
            assert_eq!(r.model, app.settings.default_model);
            assert_eq!((app.studio.sel.clone(), app.studio.focus), (StudioSel::Role("docs-writer".into()), 1));
            assert!(!app.studio.dirty, "valid, so saved");
        });
    }

    /// Every field the editor lists shows something: a name added to `ROLE_FIELDS` /
    /// `SETTING_FIELDS` without its `get_value` arm would otherwise read as an empty, dead row.
    #[test]
    fn every_listed_field_has_a_value() {
        let mut app = crate::web::snapshot::tests::test_app();
        app.studio.pattern = crate::engine::pattern::Pattern::builtin();
        for f in fields(&app, "⚙ settings") {
            assert!(!get_value(&app, "⚙ settings", &f).is_empty(), "settings field {f} shows nothing");
        }
        for f in ROLE_FIELDS {
            assert!(!get_value(&app, "planner", f).is_empty(), "role field {f} shows nothing");
        }
    }

    /// A role's `parallel` shows "none" until it has one; ←→ steps it (below 1 is none again,
    /// 128 the top), and the editor takes a number in range or none — anything else is said and
    /// leaves the value as it was.
    #[test]
    fn a_roles_parallel_steps_and_edits_within_its_range() {
        let mut app = crate::web::snapshot::tests::test_app();
        app.studio.pattern = crate::engine::pattern::Pattern::builtin();
        assert_eq!((get_value(&app, "worker-big", "parallel").as_str(), get_value(&app, "worker-swarm", "parallel").as_str()), ("none", "16"));
        assert!(fields(&app, "worker-big").contains(&"parallel".to_string()));
        assert!(!fields(&app, "planner").contains(&"parallel".to_string()), "only workers run many at a time");
        nudge(&mut app, "worker-big", "parallel", 1);
        assert_eq!(app.studio.pattern.roles["worker-big"].parallel, Some(1));
        assert!(app.studio.dirty);
        nudge(&mut app, "worker-big", "parallel", -1);
        assert_eq!(app.studio.pattern.roles["worker-big"].parallel, None, "below 1 shares the pool again");
        let edit = |app: &mut App, text: &str| apply_edit(app, &EditTarget::RoleField("worker-swarm".into(), "parallel".into()), text);
        edit(&mut app, " 32 ");
        assert_eq!(app.studio.pattern.roles["worker-swarm"].parallel, Some(32));
        nudge(&mut app, "worker-swarm", "parallel", 200);
        assert_eq!(app.studio.pattern.roles["worker-swarm"].parallel, Some(128), "the top of the range");
        for bad in ["0", "129", "lots"] {
            app.toast = None;
            edit(&mut app, bad);
            assert_eq!(app.studio.pattern.roles["worker-swarm"].parallel, Some(128), "{bad} changes nothing");
            assert!(app.toast.as_ref().map(|t| t.0.contains("from 1 to 128")).unwrap_or(false), "{bad}: {:?}", app.toast);
        }
        edit(&mut app, "none");
        assert_eq!(app.studio.pattern.roles["worker-swarm"].parallel, None);
        assert!(app.studio.pattern.validate().is_ok());
    }

    /// The fallback model cycles none → every other alias → none, and the editor's "none" clears it.
    #[test]
    fn the_fallback_cycles_through_none_and_the_other_models() {
        let mut app = crate::web::snapshot::tests::test_app();
        app.studio.pattern = crate::engine::pattern::Pattern::builtin();
        let own = app.studio.pattern.roles["planner"].model.clone();
        assert_eq!(get_value(&app, "planner", "fallback"), "none");
        let n = app.registry.models.iter().filter(|m| m.alias != own).count();
        let mut seen = vec![];
        for _ in 0..n {
            nudge(&mut app, "planner", "fallback", 1);
            let fb = app.studio.pattern.roles["planner"].fallback.clone().expect("a model after none");
            assert_ne!(fb, own, "a role never falls back to its own model");
            seen.push(fb);
        }
        assert_eq!(seen.len(), n);
        nudge(&mut app, "planner", "fallback", 1);
        assert_eq!(app.studio.pattern.roles["planner"].fallback, None, "the cycle wraps back to none");
        nudge(&mut app, "planner", "fallback", -1);
        assert_eq!(app.studio.pattern.roles["planner"].fallback.as_ref(), seen.last(), "and steps back the other way");
        apply_edit(&mut app, &EditTarget::RoleField("planner".into(), "fallback".into()), " none ");
        assert_eq!(app.studio.pattern.roles["planner"].fallback, None);
        apply_edit(&mut app, &EditTarget::RoleField("planner".into(), "fallback".into()), "luna");
        assert_eq!(app.studio.pattern.roles["planner"].fallback.as_deref(), Some("luna"));
    }

    /// `network` toggles; `writable_roots` takes a comma/line list, and "none" or nothing clears it.
    #[test]
    fn network_toggles_and_writable_roots_parse() {
        let mut app = crate::web::snapshot::tests::test_app();
        app.studio.pattern = crate::engine::pattern::Pattern::builtin();
        assert!(app.studio.pattern.settings.network);
        assert_eq!(get_value(&app, "⚙ settings", "network"), "on");
        nudge(&mut app, "⚙ settings", "network", 1);
        assert!(!app.studio.pattern.settings.network);
        assert_eq!(get_value(&app, "⚙ settings", "network"), "off");
        nudge(&mut app, "⚙ settings", "network", -1);
        assert!(app.studio.pattern.settings.network);

        let roots = EditTarget::Setting("writable_roots".into());
        assert_eq!(get_value(&app, "⚙ settings", "writable_roots"), "none");
        apply_edit(&mut app, &roots, "~/.gradle, /opt/sdk\n ~/cache ,, ");
        assert_eq!(app.studio.pattern.settings.writable_roots, vec!["~/.gradle", "/opt/sdk", "~/cache"]);
        assert_eq!(get_value(&app, "⚙ settings", "writable_roots"), "~/.gradle, /opt/sdk, ~/cache");
        apply_edit(&mut app, &roots, "None");
        assert!(app.studio.pattern.settings.writable_roots.is_empty());
        apply_edit(&mut app, &roots, "/x");
        apply_edit(&mut app, &roots, "  ");
        assert!(app.studio.pattern.settings.writable_roots.is_empty(), "an emptied field clears the list");
    }

    /// `manager_minutes` is editable as text like `review_minutes`, including its own "off" display.
    #[test]
    fn manager_minutes_accepts_numbers_and_its_off_label() {
        let mut app = crate::web::snapshot::tests::test_app();
        app.studio.pattern = crate::engine::pattern::Pattern::builtin();
        let t = EditTarget::Setting("manager_minutes".into());
        apply_edit(&mut app, &t, "7");
        assert_eq!(app.studio.pattern.settings.manager_minutes, 7);
        apply_edit(&mut app, &t, "off (escalations only)");
        assert_eq!(app.studio.pattern.settings.manager_minutes, 0);
        assert_eq!(edit_value(&app, "⚙ settings", "manager_minutes"), "off", "an edit starts from the word, not the label");
    }

    /// The context cap is listed, shown, stepped and typed — as a number, `200k`, or off.
    #[test]
    fn the_context_cap_is_a_setting_field_that_takes_k_and_off() {
        let mut app = crate::web::snapshot::tests::test_app();
        app.studio.pattern = crate::engine::pattern::Pattern::builtin();
        assert!(SETTING_FIELDS.contains(&"context_cap"));
        assert_eq!(get_value(&app, "⚙ settings", "context_cap"), "200000", "the built-in default");
        assert!(field_hint("context_cap").is_some());
        let t = EditTarget::Setting("context_cap".into());
        apply_edit(&mut app, &t, "128k");
        assert_eq!(app.studio.pattern.settings.context_cap, 128_000);
        apply_edit(&mut app, &t, "1m");
        assert_eq!(app.studio.pattern.settings.context_cap, 1_000_000);
        apply_edit(&mut app, &t, "off");
        assert_eq!(app.studio.pattern.settings.context_cap, 0);
        assert_eq!(get_value(&app, "⚙ settings", "context_cap"), "0 (the model's full window)");
        apply_edit(&mut app, &t, "not a number");
        assert_eq!(app.studio.pattern.settings.context_cap, 0, "garbage changes nothing");
        nudge(&mut app, "⚙ settings", "context_cap", 1);
        assert_eq!(app.studio.pattern.settings.context_cap, 25_000, "←→ step by 25k");
        nudge(&mut app, "⚙ settings", "context_cap", -2);
        assert_eq!(app.studio.pattern.settings.context_cap, 0, "never below 0");
    }

    /// `auto` leads the effort list, and changing a role's model must not bake it into a level.
    #[test]
    fn the_effort_cycle_offers_auto_and_a_model_change_keeps_it() {
        let mut app = crate::web::snapshot::tests::test_app();
        app.studio.pattern = crate::engine::pattern::Pattern::builtin();
        assert_eq!(app.studio.pattern.roles["worker-big"].effort, EFFORT_AUTO);
        // stepping back from auto wraps to the model's heaviest effort; forward from there returns to auto
        nudge(&mut app, "worker-big", "effort", -1);
        assert_ne!(app.studio.pattern.roles["worker-big"].effort, EFFORT_AUTO);
        nudge(&mut app, "worker-big", "effort", 1);
        assert_eq!(app.studio.pattern.roles["worker-big"].effort, EFFORT_AUTO);
        nudge(&mut app, "worker-big", "effort", 1);
        assert_eq!(app.studio.pattern.roles["worker-big"].effort, "low", "after auto comes the model's first effort");
        // a model change re-clamps a named effort, but leaves auto alone
        app.studio.pattern.roles.get_mut("worker-big").unwrap().effort = EFFORT_AUTO.into();
        nudge(&mut app, "worker-big", "model", 1);
        assert_eq!(app.studio.pattern.roles["worker-big"].effort, EFFORT_AUTO);
        app.studio.pattern.roles.get_mut("worker-big").unwrap().effort = "ultra".into();
        nudge(&mut app, "worker-big", "model", 1);
        assert_ne!(app.studio.pattern.roles["worker-big"].effort, EFFORT_AUTO);
    }

    /// Review fix: `nudge()` and `apply_edit()` used to enforce different floors (15/30 vs 5/10)
    /// and neither checked `escalate > watchdog_seconds`, so a value `Pattern::validate` would
    /// reject was only caught at save time. `clamp_watchdog` is the single place both paths now
    /// go through.
    #[test]
    fn clamp_watchdog_enforces_floor_and_escalate_gt_seconds() {
        let mut s = PatternSettings { watchdog_seconds: 1, watchdog_escalate_seconds: 1, ..Default::default() };
        clamp_watchdog(&mut s);
        assert_eq!(s.watchdog_seconds, WATCHDOG_SECONDS_MIN);
        assert!(s.watchdog_escalate_seconds > s.watchdog_seconds);

        // Both individually within their own floors, but the combination validate() forbids.
        let mut s = PatternSettings { watchdog_seconds: 90, watchdog_escalate_seconds: 30, ..Default::default() };
        clamp_watchdog(&mut s);
        assert!(s.watchdog_escalate_seconds > s.watchdog_seconds, "escalate must never end up <= seconds");
    }

    /// A role's `mcp` is a list field like `writable_roots`: names are comma- or space-separated
    /// and kept once each, "none" or nothing clears it; ⏎ on it edits in place, from nothing
    /// ("none" is how empty reads, not text to type around).
    #[test]
    fn a_roles_mcp_servers_are_a_list_field() {
        let mut app = crate::web::snapshot::tests::test_app();
        app.studio.pattern = crate::engine::pattern::Pattern::builtin();
        assert!(ROLE_FIELDS.contains(&"mcp") && field_hint("mcp").is_some());
        assert_eq!(get_value(&app, "worker-big", "mcp"), "none", "the built-ins ask for none");
        let t = EditTarget::RoleField("worker-big".into(), "mcp".into());
        apply_edit(&mut app, &t, " blender, filesystem  blender\n");
        assert_eq!(app.studio.pattern.roles["worker-big"].mcp, vec!["blender", "filesystem"]);
        assert_eq!(get_value(&app, "worker-big", "mcp"), "blender, filesystem");
        assert!(app.studio.dirty);
        apply_edit(&mut app, &t, "*");
        assert_eq!(app.studio.pattern.roles["worker-big"].mcp, vec!["*"]);
        apply_edit(&mut app, &t, "None");
        assert!(app.studio.pattern.roles["worker-big"].mcp.is_empty());
        apply_edit(&mut app, &t, "blender");
        apply_edit(&mut app, &t, "  ");
        assert!(app.studio.pattern.roles["worker-big"].mcp.is_empty(), "an emptied field clears the list");
        on_field(&mut app, "worker-big", "mcp");
        press(&mut app, KeyCode::Enter);
        assert!(matches!(&app.edit, Some(e) if e.target == t && e.text().is_empty()), "⏎ edits it in place");
        assert!(app.overlays.is_empty());
    }

    /// The instructions open in place over several lines: alt+⏎ adds a line, ⏎ keeps them.
    #[test]
    fn the_instructions_are_edited_in_place_over_several_lines() {
        crate::web::snapshot::tests::with_mantra_home(|| {
            let mut app = crate::web::snapshot::tests::test_app();
            app.studio.pattern = crate::engine::pattern::Pattern::builtin();
            on_field(&mut app, "worker-small", "instructions");
            press(&mut app, KeyCode::Backspace);
            assert!(app.edit.as_ref().is_some_and(|e| e.multiline));
            typed(&mut app, "first");
            studio_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT));
            typed(&mut app, "second");
            let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 40)).unwrap();
            term.draw(|f| draw_studio(f, &mut app)).unwrap();
            let b = term.backend().buffer();
            let screen: String = (0..b.area.height).map(|y| (0..b.area.width).map(|x| b[(x, y)].symbol()).collect::<String>() + "\n").collect();
            assert!(screen.contains("   first") && screen.contains("   second") && screen.contains("alt+⏎ new line"), "{screen}");
            press(&mut app, KeyCode::Enter);
            assert_eq!(app.studio.pattern.roles["worker-small"].instructions, "first\nsecond");
            assert!(!app.studio.dirty);
        });
    }
}
