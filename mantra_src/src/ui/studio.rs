//! Pattern Studio (roles, flow, settings + an architect agent) and the Models screen.

use super::{theme, *};
use crate::app::{App, EditTarget, Overlay, StudioSel};
use crate::config::ProviderEntry;
use crate::engine::pattern::{FinaleStep, PatternSettings, Role, COLORS, EFFORT_AUTO, KINDS, PERMISSIONS};
use crate::ui::input::{Act, Input};
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
const ROLE_FIELDS: &[&str] = &["kind", "glyph", "color", "model", "effort", "fallback", "sandbox", "permission", "max_tokens", "description", "instructions"];
const SETTING_FIELDS: &[&str] = &["isolation", "network", "writable_roots", "max_parallel", "worker_retries", "review_plan", "orchestrator_context", "stall_minutes", "gate_max_rounds", "check_timeout_secs", "max_tasks_per_phase", "watchdog_seconds", "watchdog_escalate_seconds", "review_minutes", "manager_minutes", "first_token_minutes", "context_cap"];

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
    v.push("+ add finale step".into());
    v
}

fn fields(app: &App, entry: &str) -> Vec<String> {
    match entry {
        "⚙ settings" => SETTING_FIELDS.iter().map(|s| s.to_string()).collect(),
        "⇢ flow" => flow_fields(app),
        _ => ROLE_FIELDS.iter().map(|s| s.to_string()).collect(),
    }
}

fn get_value(app: &App, entry: &str, field: &str) -> String {
    let p = &app.studio.pattern;
    match entry {
        "⚙ settings" => {
            let s = &p.settings;
            match field {
                "isolation" => s.isolation.clone(),
                "network" => if s.network { "on".into() } else { "off".into() },
                "writable_roots" => if s.writable_roots.is_empty() { "none".into() } else { s.writable_roots.join(", ") },
                "max_parallel" => s.max_parallel.to_string(),
                "worker_retries" => s.worker_retries.to_string(),
                "review_plan" => s.review_plan.to_string(),
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
                    let i: usize = x[7..].split(']').next().and_then(|n| n.parse().ok()).unwrap_or(0);
                    let st = f.finale.get(i).cloned().unwrap_or_default();
                    if x.ends_with(".role") {
                        st.role
                    } else if x.ends_with(".task") {
                        st.task
                    } else {
                        st.may_spawn.to_string()
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
                "max_tokens" => r.max_tokens.map(|t| t.to_string()).unwrap_or_else(|| "none".into()),
                "description" => r.description.clone(),
                "instructions" => r.instructions.clone(),
                _ => String::new(),
            }
        }
    }
}

fn cycle<T: AsRef<str>>(list: &[T], cur: &str, d: i32) -> String {
    let n = list.len() as i32;
    if n == 0 {
        return cur.to_string();
    }
    let i = list.iter().position(|x| x.as_ref() == cur).map(|i| i as i32).unwrap_or(-1);
    list[((i + d).rem_euclid(n)) as usize].as_ref().to_string()
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

/// ←/→ on a field: cycle enumerations / step numbers / toggle booleans.
fn nudge(app: &mut App, entry: &str, field: &str, d: i32) {
    let aliases: Vec<String> = app.registry.models.iter().map(|m| m.alias.clone()).collect();
    let roles: Vec<String> = app.studio.pattern.roles.keys().cloned().collect();
    let p = &mut app.studio.pattern;
    let step_u = |v: &mut usize, lo: usize| *v = ((*v as i64 + d as i64).max(lo as i64)) as usize;
    match entry {
        "⚙ settings" => {
            let s = &mut p.settings;
            match field {
                "isolation" => s.isolation = cycle(&["auto", "worktree", "shared"], &s.isolation, d),
                "network" => s.network = !s.network,
                "max_parallel" => step_u(&mut s.max_parallel, 1),
                "worker_retries" => s.worker_retries = (s.worker_retries as i32 + d).max(0) as u32,
                "review_plan" => s.review_plan = !s.review_plan,
                "orchestrator_context" => s.orchestrator_context = cycle(&["fresh", "compact"], &s.orchestrator_context, d),
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
        "⇢ flow" => {
            // The manager is optional: its options are "" (none) plus every role of kind manager.
            let mut managers: Vec<String> = vec![String::new()];
            managers.extend(p.roles.iter().filter(|(_, r)| r.kind == "manager").map(|(n, _)| n.clone()));
            let f = &mut p.flow;
            match field {
                "planner" => f.planner = cycle(&roles, &f.planner, d),
                "manager" => f.manager = cycle(&managers, &f.manager, d),
                "orchestrator" => f.orchestrator = cycle(&roles, &f.orchestrator, d),
                "phase_gate" => f.phase_gate = cycle(&roles, &f.phase_gate, d),
                "on_reprompt" => f.on_reprompt = cycle(&roles, &f.on_reprompt, d),
                x if x.starts_with("finale[") => {
                    let i: usize = x[7..].split(']').next().and_then(|n| n.parse().ok()).unwrap_or(0);
                    if let Some(st) = f.finale.get_mut(i) {
                        if x.ends_with(".role") {
                            st.role = cycle(&roles, &st.role, d);
                        } else if x.ends_with(".may_spawn") {
                            st.may_spawn = !st.may_spawn;
                        } else {
                            return;
                        }
                    }
                }
                _ => return,
            }
        }
        role => {
            let reg = app.registry.clone();
            let Some(r) = p.roles.get_mut(role) else { return };
            match field {
                "kind" => r.kind = cycle(KINDS, &r.kind, d),
                "glyph" => r.glyph = cycle(GLYPHS, &r.glyph, d),
                "color" => r.color = cycle(COLORS, &r.color, d),
                "model" => {
                    r.model = cycle(&aliases, &r.model, d);
                    if r.effort != EFFORT_AUTO {
                        r.effort = reg.resolve(&r.model).resolve_effort(&r.effort);
                    }
                }
                "effort" => {
                    // "auto" leads the list: Mantra picks the effort per spawn from the role's job.
                    let mut effs = reg.resolve(&r.model).efforts();
                    effs.insert(0, EFFORT_AUTO.to_string());
                    r.effort = cycle(&effs, &r.effort, d);
                }
                "fallback" => {
                    // "" (none) leads the list, then every model alias but the role's own.
                    let mut opts = vec![String::new()];
                    opts.extend(aliases.iter().filter(|a| **a != r.model).cloned());
                    let next = cycle(&opts, r.fallback.as_deref().unwrap_or(""), d);
                    r.fallback = if next.is_empty() { None } else { Some(next) };
                }
                "sandbox" => r.sandbox = cycle(SANDBOXES, &r.sandbox, d),
                "permission" => r.permission = cycle(PERMISSIONS, &r.permission, d),
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

pub fn apply_edit(app: &mut App, target: &EditTarget, text: &str) {
    match target {
        EditTarget::RoleField(role, field) => {
            if let Some(r) = app.studio.pattern.roles.get_mut(role) {
                match field.as_str() {
                    "description" => r.description = text.to_string(),
                    "instructions" => r.instructions = text.to_string(),
                    "glyph" => r.glyph = text.chars().next().map(|c| c.to_string()).unwrap_or_default(),
                    "model" => r.model = text.to_string(),
                    "effort" => r.effort = text.to_string(),
                    "fallback" => r.fallback = parse_fallback(text),
                    "max_tokens" => r.max_tokens = text.replace(['_', ','], "").parse().ok(),
                    _ => {}
                }
                app.studio.dirty = true;
            }
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
                    } else if let Some(v) = parse_tokens(text) {
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
        EditTarget::NewRole => {
            let name = crate::util::slug(text);
            if name.is_empty() || app.studio.pattern.roles.contains_key(&name) {
                app.toast("role name empty or taken", crate::agent::Level::Warn);
                return;
            }
            app.studio.pattern.roles.insert(name.clone(), Role { description: "new role".into(), ..Default::default() });
            app.studio.dirty = true;
            app.studio.sel = StudioSel::Role(name);
            app.studio.focus = 1;
        }
        EditTarget::NewPattern => {
            let name = crate::util::slug(text);
            if name.is_empty() {
                return;
            }
            app.studio.pattern.name = name.clone();
            app.studio.dirty = true;
            app.toast(format!("renamed to {name} — ctrl+s saves it as a new pattern"), crate::agent::Level::Info);
        }
        EditTarget::ModelCell(row, col) => {
            if let Some(m) = app.registry.models.get_mut(*row) {
                let t = text.trim().to_string();
                match col {
                    0 => m.alias = crate::util::slug(&t),
                    1 => m.provider = t,
                    2 => m.model = t,
                    3 => m.context_window = parse_tokens(&t),
                    4 => m.auto_compact_percent = t.trim_end_matches('%').parse().ok().map(|v: u8| v.clamp(10, 99)),
                    5 => m.default_effort = t,
                    6 => m.efforts = t.split([',', ' ']).map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect(),
                    7 => m.note = t,
                    _ => {}
                }
                app.models_ui.dirty = true;
            }
        }
        EditTarget::ProviderCell(row, col) => {
            if let Some(p) = app.registry.providers.get_mut(*row) {
                let t = text.trim().to_string();
                match col {
                    0 => p.id = crate::util::slug(&t).replace('-', "_"),
                    1 => p.name = t,
                    2 => p.base_url = t,
                    3 => p.env_key = t,
                    4 => p.api_key = if t.is_empty() { None } else { Some(t) },
                    5 => p.kind = parse_kind(&t),
                    6 => p.auth = parse_auth(&t),
                    _ => {}
                }
                app.models_ui.dirty = true;
            }
        }
    }
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

/// `writable_roots` from the editor: comma- or line-separated paths; "none" (or nothing) clears it.
fn parse_roots(t: &str) -> Vec<String> {
    if t.trim().eq_ignore_ascii_case("none") {
        return vec![];
    }
    t.split([',', '\n']).map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()
}

fn parse_tokens(t: &str) -> Option<u64> {
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
        crumbs.push(Span::styled(format!("  {} modified", theme::g("●", "*")), theme::fg(theme::AMBER)));
    }
    let right = if errs.is_empty() { vec![Span::styled(format!("{} valid ", theme::g("✓", "ok")), theme::fg(theme::GREEN))] } else { vec![Span::styled(format!("{} {} issue(s) ", theme::g("✗", "x"), errs.len()), theme::fg(theme::RED))] };
    header(f, rows[0], crumbs, right);

    let wide = rows[1].width >= 120;
    let cols = if wide {
        Layout::default().direction(Direction::Horizontal).constraints([Constraint::Length(34), Constraint::Min(40), Constraint::Length(44)]).split(rows[1])
    } else {
        Layout::default().direction(Direction::Horizontal).constraints([Constraint::Length(28), Constraint::Min(40)]).split(rows[1])
    };
    let es = entries(app);
    let sel = sel_index(app);
    let flash = anim::fade(app.studio.flash, 900);
    // list
    let mut l = vec![];
    for (i, e) in es.iter().enumerate() {
        let is = i == sel;
        let st = if is { theme::bold(theme::accent()) } else { theme::text() };
        match app.studio.pattern.roles.get(e) {
            Some(r) => l.push(Line::from(vec![
                Span::styled(if is { format!(" {} ", theme::g("▶", ">")) } else { "   ".into() }, st),
                Span::styled(format!("{} ", theme::role_glyph(&r.glyph)), theme::bold(theme::fg(theme::named(&r.color)))),
                Span::styled(format!("{:<13}", trunc(e, 13)), st),
                Span::styled(trunc(&format!("{}·{}", r.model, r.effort), (cols[0].width as usize).saturating_sub(22)), theme::faint()),
            ])),
            None => {
                if i > 0 && app.studio.pattern.roles.contains_key(&es[i - 1]) {
                    l.push(Line::default());
                }
                l.push(Line::from(vec![Span::styled(if is { format!(" {} ", theme::g("▶", ">")) } else { "   ".into() }, st), Span::styled(e.clone(), st)]));
            }
        }
    }
    let lcol = if app.studio.focus == 0 { theme::VIOLET } else { theme::FAINT };
    let lcol_blk = if flash > 0.0 { theme::SAFFRON } else { lcol };
    f.render_widget(Paragraph::new(l).block(block("roles", lcol_blk)), cols[0]);

    // fields
    let entry = es[sel].clone();
    let fs = fields(app, &entry);
    let fsel = app.studio.field.min(fs.len().saturating_sub(1));
    // name column as wide as the longest name (settings has a 25-char one); the value column gets
    // the box's inner width less the " ▶" + name prefix, and on the selected row the arrows too
    let nw = fs.iter().map(|f| w_of(f)).max().unwrap_or(0).max(19) + 1;
    let fw_all = (cols[1].width as usize).saturating_sub(2 + 2 + nw);
    let mut fl = vec![];
    if let Some(r) = app.studio.pattern.roles.get(&entry) {
        let dw = (cols[1].width as usize).saturating_sub(7 + w_of(&entry));
        fl.push(Line::from(vec![Span::styled(format!(" {} ", theme::role_glyph(&r.glyph)), theme::bold(theme::fg(theme::named(&r.color)))), Span::styled(entry.clone(), theme::bold(theme::text())), Span::styled(format!("  {}", trunc(&r.description, dw)), theme::dim())]));
        fl.push(Line::default());
    }
    // the selected field's line and the last line of its hint, which the scroll keeps in view
    let (mut sel_line, mut sel_end) = (0, 0);
    for (i, fname) in fs.iter().enumerate() {
        let v = get_value(app, &entry, fname);
        let is = i == fsel && app.studio.focus == 1;
        let st = if is { theme::bold(theme::accent()) } else { theme::muted() };
        // free text has nothing to step through: ⏎ opens the editor
        let text_only = ["description", "instructions", "writable_roots"].contains(&fname.as_str()) || fname.ends_with(".task");
        let arrows = if !is { String::new() } else if text_only { format!(" {}", theme::g("⏎", "<enter>")) } else { format!(" {}", theme::g("◂ ▸", "< >")) };
        // the value gets what the marker after it leaves (` ⏎` is narrower than ` ◂ ▸`)
        let fw = fw_all.saturating_sub(w_of(&arrows));
        let shown = if fname == "instructions" || fname.ends_with(".task") {
            let first = v.lines().find(|l| !l.trim().is_empty()).unwrap_or("").to_string();
            format!("{} ({} lines)", trunc(&first, fw.saturating_sub(12)), v.lines().count())
        } else {
            trunc(&v, fw)
        };
        let vstyle = match fname.as_str() {
            "effort" => theme::fg(effort_color(&v)),
            "color" => theme::fg(theme::named(&v)),
            _ => theme::text(),
        };
        if i == fsel {
            sel_line = fl.len();
        }
        fl.push(Line::from(vec![Span::styled(format!(" {}{:<nw$}", if is { theme::g("▶", ">") } else { " " }, fname), st), Span::styled(shown, vstyle), Span::styled(arrows, theme::faint())]));
        // what the focused field means, right under it
        if let Some(h) = field_hint(fname).filter(|_| is) {
            let pad = || vec![Span::raw("   ")];
            fl.extend(md::wrap(vec![Span::styled(h, theme::faint())], (cols[1].width as usize).saturating_sub(3), pad(), pad()));
        }
        if i == fsel {
            sel_end = fl.len() - 1;
        }
    }
    fl.push(Line::default());
    let pad = || vec![Span::raw(" ")];
    fl.extend(md::wrap(vec![Span::styled("←→ change · ⏎ edit text · n new role · x delete role · r rename pattern", theme::faint())], (cols[1].width as usize).saturating_sub(3), pad(), pad()));
    let fcol = if app.studio.focus == 1 { theme::VIOLET } else { theme::FAINT };
    // A list longer than the box (settings on a short terminal) scrolls to keep the selection in view.
    let fh = cols[1].height.saturating_sub(2) as usize;
    let scroll = (sel_end + 1).saturating_sub(fh).min(sel_line) as u16;
    f.render_widget(Paragraph::new(fl).scroll((scroll, 0)).block(block(&entry, fcol)), cols[1]);

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
        pl.push(arrow(&format!("spawns ≤{} in parallel / phase", p.settings.max_parallel)));
        let mut ws = vec![];
        for w in p.worker_roles() {
            ws.extend(rg(&w));
            ws.push(Span::raw("  "));
        }
        // side by side while they fit, else one worker role per row
        if Line::from(ws.clone()).width() + 1 <= cols[2].width.saturating_sub(2) as usize {
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
    }
    footer(f, rows[3], &[("tab", "focus"), ("↑↓", "select"), ("←→", "change"), ("⏎", "edit"), ("ctrl+s", "save"), ("esc", "back")]);
}

/// One line on what the focused field means, for the fields that need more than their name.
fn field_hint(field: &str) -> Option<&'static str> {
    Some(match field {
        "effort" => "auto = Mantra picks per spawn: medium to plan, supervise and gate, low for small workers, high for finale steps.",
        "fallback" => "The model that takes over when this role's model twice produces nothing within first_token_minutes. none = the manager is asked instead.",
        "permission" => "never = the agent never asks (the default). on-request / untrusted: you approve by hand; requests land in the inbox (ctrl+g).",
        "network" => "on = workspace-write agents may use the network (package managers, docs).",
        "writable_roots" => "Extra directories sandboxed agents may write, comma-separated (~ = your home). Toolchain caches are added automatically.",
        "first_token_minutes" => "Minutes a turn may produce nothing before it is restarted; the second time the role's fallback model takes over. off = no deadline.",
        "context_cap" => "Tokens of context every agent is told it has, whatever its model's window (the smaller of the two): a huge window lets an agent cycle tools for hours. 0 = the model's full window; 200k is the built-in default.",
        _ => return None,
    })
}

pub fn studio_key(app: &mut App, k: KeyEvent) {
    let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
    if ctrl && k.code == KeyCode::Char('s') {
        match app.studio.pattern.validate() {
            Ok(()) => match app.studio.pattern.save() {
                Ok(p) => {
                    app.studio.dirty = false;
                    app.pattern_name = app.studio.pattern.name.clone();
                    app.toast(format!("saved {}", home_rel(&p)), crate::agent::Level::Ok);
                }
                Err(e) => app.toast(format!("save failed: {e}"), crate::agent::Level::Error),
            },
            Err(errs) => app.toast(format!("fix first: {}", errs[0]), crate::agent::Level::Error),
        }
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
    match k.code {
        KeyCode::Esc => {
            app.leave_screen();
        }
        KeyCode::Up | KeyCode::Char('k') => {
            if app.studio.focus == 0 {
                let i = sel_index(app);
                select_by_index(app, i.saturating_sub(1));
                app.studio.field = 0;
            } else {
                app.studio.field = app.studio.field.saturating_sub(1);
            }
        }
        KeyCode::Down | KeyCode::Char('j') => {
            if app.studio.focus == 0 {
                let i = sel_index(app);
                select_by_index(app, (i + 1).min(es.len() - 1));
                app.studio.field = 0;
            } else {
                app.studio.field = (app.studio.field + 1).min(fs.len().saturating_sub(1));
            }
        }
        KeyCode::Left | KeyCode::Right | KeyCode::Char('h') | KeyCode::Char('l') => {
            if app.studio.focus == 0 {
                app.studio.focus = 1;
                return;
            }
            let d = if matches!(k.code, KeyCode::Left | KeyCode::Char('h')) { -1 } else { 1 };
            if let Some(fname) = fs.get(app.studio.field) {
                nudge(app, &entry, fname, d);
            }
        }
        KeyCode::Enter => {
            if app.studio.focus == 0 {
                app.studio.focus = 1;
                return;
            }
            let Some(fname) = fs.get(app.studio.field).cloned() else { return };
            if fname == "+ add finale step" {
                let first_gate = app.studio.pattern.roles.iter().find(|(_, r)| r.kind == "gate").map(|(n, _)| n.clone()).unwrap_or_default();
                app.studio.pattern.flow.finale.push(FinaleStep { role: first_gate, task: "Describe what this step should do.".into(), may_spawn: false });
                app.studio.dirty = true;
                return;
            }
            let target = match entry.as_str() {
                "⚙ settings" if ["isolation", "network", "review_plan", "orchestrator_context"].contains(&fname.as_str()) => return nudge(app, &entry, &fname, 1),
                "⚙ settings" => EditTarget::Setting(fname.clone()),
                "⇢ flow" if fname.starts_with("finale[") => {
                    let i: usize = fname[7..].split(']').next().and_then(|n| n.parse().ok()).unwrap_or(0);
                    if fname.ends_with(".task") {
                        EditTarget::FlowStep(i, "task".into())
                    } else {
                        return nudge(app, &entry, &fname, 1);
                    }
                }
                "⇢ flow" => return nudge(app, &entry, &fname, 1),
                role => {
                    if ["kind", "color", "sandbox", "permission", "effort", "model", "fallback"].contains(&fname.as_str()) {
                        return nudge(app, &entry, &fname, 1);
                    }
                    EditTarget::RoleField(role.to_string(), fname.clone())
                }
            };
            let mut input = Input::default();
            // "none" is how an empty value reads, not text to edit around
            let v = get_value(app, &entry, &fname);
            input.set(if v == "none" && ["max_tokens", "writable_roots"].contains(&fname.as_str()) { "" } else { &v });
            app.overlays.push(Overlay::Edit { title: format!("{entry} · {fname}"), input, target });
        }
        KeyCode::Char('n') => app.overlays.push(Overlay::Edit { title: "new role name".into(), input: Input::default(), target: EditTarget::NewRole }),
        KeyCode::Char('r') => {
            let mut input = Input::default();
            input.set(&app.studio.pattern.name);
            app.overlays.push(Overlay::Edit { title: "pattern name (saving under a new name creates a copy)".into(), input, target: EditTarget::NewPattern });
        }
        KeyCode::Char('x') | KeyCode::Delete => {
            if entry == "⇢ flow" {
                if let Some(fname) = fs.get(app.studio.field) {
                    if let Some(i) = fname.strip_prefix("finale[").and_then(|x| x.split(']').next()).and_then(|n| n.parse::<usize>().ok()) {
                        app.studio.pattern.flow.finale.remove(i);
                        app.studio.dirty = true;
                        app.studio.field = 0;
                    }
                }
                return;
            }
            if app.studio.pattern.roles.contains_key(&entry) {
                let fl = &app.studio.pattern.flow;
                let used = [&fl.planner, &fl.manager, &fl.orchestrator, &fl.phase_gate, &fl.on_reprompt].iter().any(|x| **x == entry) || fl.finale.iter().any(|s| s.role == entry);
                if used {
                    app.toast(format!("{entry} is used in the flow — change the flow first"), crate::agent::Level::Warn);
                } else {
                    let i = sel_index(app);
                    app.studio.pattern.roles.remove(&entry);
                    app.studio.dirty = true;
                    select_by_index(app, i.saturating_sub(1));
                }
            }
        }
        _ => {}
    }
}

// ───────────────────────────── models ─────────────────────────────

const MODEL_COLS: &[&str] = &["alias", "provider", "model", "context", "compact", "default", "efforts", "note / test"];
const PROV_COLS: &[&str] = &["id", "name", "base_url", "env_key", "api_key", "kind", "auth"];
/// Display strings for `ProviderEntry.kind` (WP10.2) — cycled with +/- on the providers grid the
/// same way `sandbox`/`permission` cycle elsewhere in Studio (`nudge`, above). Kept here rather
/// than a `Display` impl on `ProviderKind` since it's presentation-only.
const PROV_KINDS: &[&str] = &["Codex", "ClaudeCode"];
/// Display strings for `ProviderEntry.auth` (WP10.2; irrelevant for `Codex`-kind providers, but
/// still editable so a provider can be flipped to `ClaudeCode` and given `auth` in one place).
const PROV_AUTHS: &[&str] = &["subscription", "api_key"];

fn kind_str(k: crate::config::ProviderKind) -> &'static str {
    match k {
        crate::config::ProviderKind::Codex => "Codex",
        crate::config::ProviderKind::ClaudeCode => "ClaudeCode",
    }
}

fn parse_kind(t: &str) -> crate::config::ProviderKind {
    let t = t.trim().to_ascii_lowercase().replace(['_', '-', ' '], "");
    if t == "claudecode" || t == "claude" {
        crate::config::ProviderKind::ClaudeCode
    } else {
        crate::config::ProviderKind::Codex
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

pub fn draw_models(f: &mut Frame, app: &mut App) {
    let area = f.area();
    let np = app.registry.providers.len() as u16;
    let rows = Layout::default().direction(Direction::Vertical).constraints([Constraint::Length(1), Constraint::Min(6), Constraint::Length(np + 8), Constraint::Length(1)]).split(area);
    let mut crumbs = vec![Span::styled(format!("  {} models", theme::g("›", ">")), theme::faint())];
    if app.models_ui.dirty {
        crumbs.push(Span::styled(format!("  {} modified", theme::g("●", "*")), theme::fg(theme::AMBER)));
    }
    header(f, rows[0], crumbs, vec![Span::styled(home_rel(&crate::config::Registry::path()), theme::faint()), Span::raw(" ")]);
    let widths = [10usize, 9, 18, 14, 14, 8, 30, 30];
    let mk_head = |cols: &[&str], ws: &[usize]| Line::from(cols.iter().zip(ws).map(|(c, w)| Span::styled(format!(" {:<w$}", c, w = *w), theme::bold(theme::muted()))).collect::<Vec<_>>());
    let mut l = vec![mk_head(MODEL_COLS, &widths)];
    // Whole-thousands tokens print as "200k" (no decimal); anything else falls back to fmt_tokens.
    let short_tokens = |n: u64| if n > 0 && n % 1000 == 0 { format!("{}k", n / 1000) } else { fmt_tokens(n) };
    for (ri, m) in app.registry.models.iter().enumerate() {
        let status = app.models_ui.status.get(&m.alias).cloned().unwrap_or_else(|| m.note.clone());
        let vals = [
            m.alias.clone(),
            m.provider.clone(),
            m.model.clone(),
            match m.context_window {
                Some(cw) => short_tokens(cw),
                None => format!("{} (assumed)", short_tokens(m.effective_context())),
            },
            match m.auto_compact_percent {
                Some(p) => format!("{p}%"),
                None => format!("{}% (default)", m.effective_compact_percent()),
            },
            if m.efforts().is_empty() { "—".into() } else { m.default_effort.clone() },
            if m.efforts().is_empty() { "none (not sent)".into() } else { m.efforts().join(" ") },
            status,
        ];
        let mut spans = vec![];
        for (ci, v) in vals.iter().enumerate() {
            let is = !app.models_ui.providers && ri == app.models_ui.row && ci == app.models_ui.col;
            let derived = (ci == 3 && m.context_window.is_none()) || (ci == 4 && m.auto_compact_percent.is_none());
            let st = if is {
                Style::default().fg(theme::c(theme::SAFFRON)).add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
            } else if derived {
                theme::dim()
            } else if ci == 5 {
                theme::fg(effort_color(v))
            } else if ci == 7 && v.starts_with("ok") {
                theme::fg(theme::GREEN)
            } else if ci == 7 && v.starts_with("error") {
                theme::fg(theme::RED)
            } else if ri == app.models_ui.row && !app.models_ui.providers {
                theme::text()
            } else {
                theme::muted()
            };
            spans.push(Span::raw(" "));
            spans.push(Span::styled(format!("{:<w$}", trunc(v, widths[ci]), w = widths[ci]), st));
        }
        l.push(Line::from(spans));
    }
    l.push(Line::default());
    l.push(Line::from(Span::styled(" context + compact % are always passed to each agent (model_context_window, model_auto_compact_token_limit)", theme::faint())));
    l.push(Line::from(Span::styled(" dim \"(assumed)\" / \"(default)\" = not set here, using Mantra's built-in 200k / 85% · +/- steps values", theme::faint())));
    let mcol = if app.models_ui.providers { theme::FAINT } else { theme::SAFFRON };
    f.render_widget(Paragraph::new(l).block(block("models", mcol)), rows[1]);

    let pw = [10usize, 12, 26, 14, 10, 11, 12];
    let mut pl = vec![mk_head(PROV_COLS, &pw)];
    let openai_vals = ["openai", "OpenAI", "(built into Codex — uses your codex login)", "", "Codex", "—"];
    let mut ospans = vec![];
    for (ci, v) in openai_vals.iter().enumerate() {
        ospans.push(Span::raw(" "));
        ospans.push(Span::styled(format!("{:<w$}", trunc(v, pw[ci]), w = pw[ci]), theme::faint()));
    }
    pl.push(Line::from(ospans));
    for (ri, p) in app.registry.providers.iter().enumerate() {
        let masked = match &p.api_key {
            Some(k) if !k.trim().is_empty() => {
                let k = k.trim();
                if k.len() > 4 { format!("••••{}", &k[k.len() - 4..]) } else { "••••".to_string() }
            }
            _ => String::new(),
        };
        let vals = [p.id.clone(), p.name.clone(), p.base_url.clone(), p.env_key.clone(), masked, kind_str(p.kind).to_string(), p.auth.clone()];
        let mut spans = vec![];
        for (ci, v) in vals.iter().enumerate() {
            let is = app.models_ui.providers && ri == app.models_ui.row && ci == app.models_ui.col;
            let st = if is { Style::default().fg(theme::c(theme::SAFFRON)).add_modifier(Modifier::BOLD | Modifier::UNDERLINED) } else { theme::text() };
            spans.push(Span::raw(" "));
            spans.push(Span::styled(format!("{:<w$}", trunc(v, pw[ci]), w = pw[ci]), st));
        }
        // A draft (placeholder/malformed URL, or no credential — `ProviderEntry::draft_reason`)
        // says why `t`/`D` are off for it; a ready row shows where its key is sent and how it is
        // held. `ClaudeCode` + `subscription` needs no key at all (OAuth login) — a red "no key"
        // for it would be misleading, so only a key that is actually expected is reported.
        if let Some(why) = p.draft_reason() {
            spans.push(Span::styled(format!(" {} draft — {why}", theme::g("✗", "x")), theme::fg(theme::AMBER)));
        } else {
            if !p.needs_key() {
                spans.push(Span::styled(format!(" {} subscription (OAuth login)", theme::g("○", "-")), theme::dim()));
            } else {
                let key_msg = if !p.env_key.trim().is_empty() && std::env::var(p.env_key.trim()).map(|v| !v.trim().is_empty()).unwrap_or(false) { "key set (env)" } else { "key stored in models.toml" };
                spans.push(Span::styled(format!(" {} {key_msg}", theme::g("✓", "ok")), theme::fg(theme::GREEN)));
            }
            if let Some(h) = p.host() {
                spans.push(Span::styled(format!(" {} {h}", theme::g("→", "->")), theme::dim()));
            }
        }
        let n = app.registry.models.iter().filter(|m| m.provider == p.id).count();
        spans.push(Span::styled(format!("  {n} model{}", if n == 1 { "" } else { "s" }), if n == 0 { theme::fg(theme::AMBER) } else { theme::dim() }));
        pl.push(Line::from(spans));
    }
    pl.push(Line::from(Span::styled(" base_url = the provider's API root: Codex reads …/v1/responses + …/v1/models; ClaudeCode uses it as ANTHROPIC_BASE_URL (bare host, no /v1) + …/v1/models for D", theme::faint())));
    pl.push(Line::from(Span::styled(" env_key = name of an env var holding the key · api_key = paste one directly (stored 0600) · either works · D here = discover this provider", theme::faint())));
    pl.push(Line::from(Span::styled(" a new row (n) is a draft: saved, but t/D send nothing until base_url is real (not the example.com placeholder) and its own key is available", theme::faint())));
    pl.push(Line::from(Span::styled(" kind = Codex | ClaudeCode · auth (ClaudeCode only) = subscription | api_key · +/- on either cycles it", theme::faint())));
    let pcol = if app.models_ui.providers { theme::SAFFRON } else { theme::FAINT };
    f.render_widget(Paragraph::new(pl).block(block("providers", pcol)), rows[2]);
    // `t`/`D` name the host the selected row's request goes to, or say they are off for a draft
    // (the row itself carries the reason).
    let action = |plain: &str, verb: &str, p: Option<&ProviderEntry>| match p {
        Some(p) if p.draft_reason().is_some() => format!("{verb} · off (draft)"),
        Some(p) => p.host().map(|h| format!("{verb} {} {h}", theme::g("→", "->"))).unwrap_or_else(|| plain.to_string()),
        None => plain.to_string(),
    };
    let (t_hint, d_hint) = if app.models_ui.providers {
        ("test model".to_string(), action("discover models", "discover", app.registry.providers.get(app.models_ui.row)))
    } else {
        (action("test model", "test", app.registry.models.get(app.models_ui.row).and_then(|m| app.registry.provider_of(&m.alias))), "discover models".to_string())
    };
    footer(f, rows[3], &[("↑↓←→", "cell"), ("⏎", "edit"), ("t", &t_hint), ("D", &d_hint), ("n", "new"), ("x", "delete"), ("tab", "models/providers"), ("ctrl+s", "save"), ("esc", "back")]);
}

pub fn models_key(app: &mut App, k: KeyEvent) {
    let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
    let ui = &mut app.models_ui;
    let (nrows, ncols) = if ui.providers { (app.registry.providers.len(), PROV_COLS.len()) } else { (app.registry.models.len(), MODEL_COLS.len()) };
    if ctrl && k.code == KeyCode::Char('s') {
        match app.registry.save() {
            Ok(()) => {
                app.models_ui.dirty = false;
                app.toast("models saved — new agents use these settings", crate::agent::Level::Ok);
            }
            Err(e) => app.toast(format!("save failed: {e}"), crate::agent::Level::Error),
        }
        return;
    }
    match k.code {
        KeyCode::Esc => app.leave_screen(),
        KeyCode::Tab => {
            ui.providers = !ui.providers;
            ui.row = 0;
            ui.col = 0;
        }
        KeyCode::Up => ui.row = ui.row.saturating_sub(1),
        KeyCode::Down => ui.row = (ui.row + 1).min(nrows.saturating_sub(1)),
        KeyCode::Left => ui.col = ui.col.saturating_sub(1),
        KeyCode::Right => ui.col = (ui.col + 1).min(ncols - 1),
        KeyCode::Char('+') | KeyCode::Char('-') if ui.providers => {
            // Cycle `kind`/`auth` the same way +/- cycles enumerations on the models grid below,
            // and the way `sandbox`/`permission` cycle elsewhere in Studio (`nudge`, above) — the
            // only in-app way to configure a third-party ClaudeCode provider (§10.2).
            let d: i32 = if k.code == KeyCode::Char('+') { 1 } else { -1 };
            if let Some(p) = app.registry.providers.get_mut(ui.row) {
                match ui.col {
                    4 => p.kind = parse_kind(&cycle(PROV_KINDS, kind_str(p.kind), d)),
                    5 => p.auth = cycle(PROV_AUTHS, &p.auth, d),
                    _ => {}
                }
                app.models_ui.dirty = true;
            }
        }
        KeyCode::Char('+') | KeyCode::Char('-') if !ui.providers => {
            let d: i64 = if k.code == KeyCode::Char('+') { 1 } else { -1 };
            if let Some(m) = app.registry.models.get_mut(ui.row) {
                match ui.col {
                    3 => {
                        const P: &[u64] = &[128_000, 200_000, 272_000, 400_000, 1_000_000];
                        let cur = m.context_window.unwrap_or(0);
                        let i = P.iter().position(|x| *x >= cur).unwrap_or(P.len()) as i64;
                        let j = (i + d).clamp(0, P.len() as i64 - 1) as usize;
                        m.context_window = Some(P[j]);
                    }
                    4 => m.auto_compact_percent = Some((m.auto_compact_percent.unwrap_or(85) as i64 + 5 * d).clamp(10, 99) as u8),
                    5 => {
                        let e = m.step_effort(&m.default_effort.clone(), d as i32);
                        m.default_effort = e;
                    }
                    _ => {}
                }
                app.models_ui.dirty = true;
            }
        }
        KeyCode::Enter => {
            let (title, val, target) = if ui.providers {
                let Some(p) = app.registry.providers.get(ui.row) else { return };
                let v = [p.id.clone(), p.name.clone(), p.base_url.clone(), p.env_key.clone(), p.api_key.clone().unwrap_or_default(), kind_str(p.kind).to_string(), p.auth.clone()][ui.col.min(6)].clone();
                let hint = if ui.col == 4 { " (either name an env var or paste a key)" } else { "" };
                (format!("provider · {}{hint}", PROV_COLS[ui.col]), v, EditTarget::ProviderCell(ui.row, ui.col))
            } else {
                let Some(m) = app.registry.models.get(ui.row) else { return };
                let v = [m.alias.clone(), m.provider.clone(), m.model.clone(), m.context_window.map(|c| c.to_string()).unwrap_or_default(), m.auto_compact_percent.map(|c| c.to_string()).unwrap_or_default(), m.default_effort.clone(), m.efforts().join(", "), m.note.clone()][ui.col].clone();
                (format!("{} · {}  (context accepts 400k / 1m)", m.alias, MODEL_COLS[ui.col]), v, EditTarget::ModelCell(ui.row, ui.col))
            };
            let mut input = Input::default();
            input.set(&val);
            app.overlays.push(Overlay::Edit { title, input, target });
        }
        KeyCode::Char('n') => {
            if ui.providers {
                app.registry.providers.push(ProviderEntry { id: "myprovider".into(), name: "My provider".into(), base_url: "https://example.com/v1".into(), env_key: "MYPROVIDER_API_KEY".into(), wire_api: "responses".into(), ..Default::default() });
                ui.row = app.registry.providers.len() - 1;
            } else {
                app.registry.models.push(crate::config::ModelEntry { alias: format!("model{}", app.registry.models.len() + 1), model: "model-id".into(), ..Default::default() });
                ui.row = app.registry.models.len() - 1;
            }
            app.models_ui.dirty = true;
        }
        KeyCode::Char('x') | KeyCode::Delete => {
            if ui.providers {
                if ui.row < app.registry.providers.len() {
                    app.registry.providers.remove(ui.row);
                }
            } else if ui.row < app.registry.models.len() && app.registry.models.len() > 1 {
                app.registry.models.remove(ui.row);
            }
            app.models_ui.row = app.models_ui.row.saturating_sub(1);
            app.models_ui.dirty = true;
        }
        KeyCode::Char('t') if !ui.providers => {
            if let Some(m) = app.registry.models.get(ui.row) {
                let alias = m.alias.clone();
                app.probe_model(&alias);
            }
        }
        KeyCode::Char('D') | KeyCode::Char('d') => {
            if ui.providers {
                match app.registry.providers.get(ui.row).map(|p| (p.id.clone(), p.draft_reason())) {
                    // A draft never goes on the network — say why instead (P2, test report).
                    Some((id, Some(why))) => app.toast(format!("{id} is a draft, not discovered — {why}"), crate::agent::Level::Warn),
                    Some((id, None)) => app.discover(Some(id)),
                    None => app.toast("add a provider first (n)", crate::agent::Level::Info),
                }
            } else {
                app.discover(None);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every field the editor lists shows something: a name added to `ROLE_FIELDS` /
    /// `SETTING_FIELDS` without its `get_value` arm would otherwise read as an empty, dead row.
    #[test]
    fn every_listed_field_has_a_value() {
        let mut app = crate::web::snapshot::tests::test_app();
        app.studio.pattern = crate::engine::pattern::Pattern::builtin();
        for f in SETTING_FIELDS {
            assert!(!get_value(&app, "⚙ settings", f).is_empty(), "settings field {f} shows nothing");
        }
        for f in ROLE_FIELDS {
            assert!(!get_value(&app, "planner", f).is_empty(), "role field {f} shows nothing");
        }
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
}
