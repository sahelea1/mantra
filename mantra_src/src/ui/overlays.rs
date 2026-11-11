//! Modal overlays.

use super::{theme, *};
use crate::app::{App, Overlay, PatternChoice, PickRow, PickTarget, Screen, Select, SelectThen};
use crate::engine::run::{HaltReason, Stage};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

pub fn draw(f: &mut Frame, app: &mut App) {
    let n = app.overlays.len();
    for i in 0..n {
        // The scrolling ones say how far they can scroll: the stored offset is clamped to it, so
        // ↑ after over-scrolling (or a shrunk terminal) moves the view on the first press.
        if let Some(max) = draw_one(f, app, i) {
            if let Overlay::Help { scroll } | Overlay::Diff { scroll, .. } | Overlay::Plan { scroll } | Overlay::Halt { scroll } | Overlay::Akasha { scroll, .. } = &mut app.overlays[i] {
                *scroll = (*scroll).min(max);
            }
        }
    }
}

/// Clears an overlay's box and a gutter of up to three columns on either side of it, so what
/// the screen under it had in those columns (a box's border, the last letters of a row cut by
/// the overlay) does not read as stray marks beside the overlay's own border.
fn clear(f: &mut Frame, area: Rect, r: Rect) {
    let x0 = r.x.saturating_sub(3).max(area.x);
    let x1 = (r.x + r.width + 3).min(area.x + area.width);
    let gutter = Rect { x: x0, width: x1.saturating_sub(x0), ..r };
    f.render_widget(Clear, gutter);
    f.render_widget(Block::default().style(theme::surface()), gutter);
}

/// Draws overlay `i`; a scrolling one returns its largest useful scroll offset.
fn draw_one(f: &mut Frame, app: &App, i: usize) -> Option<usize> {
    let area = f.area();
    match &app.overlays[i] {
        Overlay::Help { scroll } => return Some(help(f, area, *scroll)),
        Overlay::ModelPicker { sel, target, role_scope, filter } => model_picker(f, area, app, *sel, target, *role_scope, filter),
        Overlay::Diff { agent, file, scroll } => return diff(f, area, app, *agent, *file, *scroll),
        Overlay::Inbox { sel } => inbox(f, area, app, *sel),
        Overlay::Plan { scroll } => return Some(plan(f, area, app, *scroll)),
        Overlay::Halt { scroll } => return halt(f, area, app, *scroll),
        Overlay::NewRun { sel, list, goal } => new_run(f, area, app, *sel, list, goal.as_ref()),
        Overlay::Select(s) => select(f, area, s),
        Overlay::Discover(st) => discover(f, area, app, st),
        Overlay::Runs { sel, list, confirm, others } => runs(f, area, app, *sel, list, *confirm, *others),
        Overlay::Qr => super::settings::draw_qr(f, area, app),
        Overlay::Akasha { scroll, group } => return Some(akasha(f, area, app, group.as_deref(), *scroll)),
    }
    None
}

/// The keys of the screens, grouped as they work: everywhere, in a chat (Solo, a zoom, the
/// stage's prompt), on the stage, and on the config screens — whose one editing model is said
/// once, for all three.
fn help(f: &mut Frame, area: Rect, scroll: usize) -> usize {
    let w = 104u16.min(area.width.saturating_sub(2));
    // keys in a column as wide as the longest one; descriptions wrap under themselves, a
    // column short of the border
    let tw = w.saturating_sub(3) as usize;
    let k = |a: &str, b: &str| md::wrap(vec![Span::styled(b.to_string(), theme::text())], tw, vec![Span::styled(format!("  {a:<21}"), theme::bold(theme::accent()))], vec![Span::raw(" ".repeat(23))]);
    let h = |t: &str| vec![Line::from(Span::styled(format!(" {t}"), theme::bold(theme::fg(theme::VIOLET))))];
    let gap = || vec![Line::default()];
    let l: Vec<Line<'static>> = [
        h("everywhere"),
        k("ctrl+o", "switch Solo ⇄ Mandala"),
        k("ctrl+k", "model picker: type to filter · tab agent/role · ←→ effort"),
        k("ctrl+g", "inbox: approvals & alerts from all agents"),
        k("ctrl+e", "verbose log (full reasoning, output, diffs)"),
        k("ctrl+c", "close overlay / clear input / interrupt the turn · three presses quit"),
        k("pgup / pgdn · ctrl+l", "scroll (mouse wheel too) · redraw the screen"),
        k("/ · F1", "commands — tab completes them and their arguments (groups, agents) · this help"),
        gap(),
        h("chat (solo, a zoomed agent, the stage's prompt)"),
        k("⏎ / alt+⏎", "send / newline (or end a line with \\) · ! runs a shell command in Solo"),
        k("⏎ while it works", "queue behind the current turn · ctrl+f sends the queue and the input now"),
        k("backspace · ctrl+x", "on an empty input: the last queued message back to edit · drop the whole queue"),
        k("alt+↑ / alt+↓", "raise / lower reasoning effort (shift+↑↓ works too)"),
        k("shift+tab", "cycle approvals: untrusted → on-request → never"),
        k("ctrl+d · ctrl+t (F2)", "diff viewer for the focused agent · side panel (Solo, zoom) / pulse feed (stage)"),
        k("esc", "clear the input; in a zoom, back to the overview (it never interrupts)"),
        gap(),
        h("stage (tab: typing ⇄ navigating the canvas)"),
        k("←→↑↓ · 1-9 · ⏎", "select an agent · jump to the nth and zoom in · zoom into the selected (esc back)"),
        k("space · e", "pause / resume the run (a halted run resumes only on space) · a halt's whole message"),
        k("r · x · c", "retry (or restart a crashed agent) · interrupt · compact"),
        k("m · +/-", "switch the selected agent's model, running or not (fixes a rejected model) · its effort"),
        k("p · a", "plan · approve the plan (during review)"),
        k("message", "all run messages go to the planner; agent views inspect each role’s work"),
        k("/stop · /runs", "stop the run for good · this project's runs: resume, land or delete"),
        gap(),
        h("config screens (/models, /studio, /settings)"),
        k("type · ⏎", "type over the selected field, or ⏎ to edit it; ⏎ again saves the file, esc puts it back"),
        k("←→ · space", "change a choice, flip an on/off (in the models grid ←→ move: +/- and space change)"),
        k("ctrl+n", "add: a provider from its key, a model, Codex, Claude Code or Antigravity · a role · an MCP server"),
        k("del del", "delete the row (press twice)"),
        k("ctrl+t · ctrl+d", "test a model or an MCP server · ask a provider for its models"),
        k("tab · esc", "the next pane or grid · back to where you came from"),
    ]
    .concat();
    let r = centered(area, w, l.len() as u16 + 3);
    clear(f, area, r);
    let blk = block("keys", theme::SIGNAL_RED);
    let inner = blk.inner(r);
    f.render_widget(blk, r);
    // the list outgrows a 36-row terminal: it scrolls, the footer stays put
    let vh = inner.height.saturating_sub(1) as usize;
    let more = l.len() > vh;
    let max = l.len().saturating_sub(vh);
    let view: Vec<Line> = l.into_iter().skip(scroll.min(max)).take(vh).collect();
    f.render_widget(Paragraph::new(view), Rect { height: vh as u16, ..inner });
    let hint = if more { " ↑↓ scroll · esc close " } else { " esc close " };
    f.render_widget(Paragraph::new(Span::styled(hint, theme::faint())), Rect { x: inner.x + 1, y: inner.y + inner.height.saturating_sub(1), width: inner.width.saturating_sub(1), height: 1 });
    max
}

/// Which runtime a provider's models run through (WP10): `codex app-server`, `claude -p` or
/// `bija`.
fn backend_glyph(reg: &crate::config::Registry, provider: &str) -> &'static str {
    match reg.providers.iter().find(|p| p.id == provider).map(|p| p.kind) {
        Some(crate::config::ProviderKind::ClaudeCode) => "✧ claude",
        Some(crate::config::ProviderKind::Bija) => "• bija",
        Some(crate::config::ProviderKind::Antigravity) => "✦ agy",
        _ => "◌ codex",
    }
}

/// The role a picker target is about, with its pattern entry: the run's copy for an agent of the
/// run or a `RunRole`, the Studio's pattern for a field.
fn picker_role(app: &App, target: &PickTarget) -> Option<(String, crate::engine::pattern::Role)> {
    match target {
        PickTarget::Agent(a) => {
            let run = app.run.as_ref().filter(|_| app.in_run(*a))?;
            let name = run.role_name_of(*a)?;
            run.pattern.roles.get(&name).cloned().map(|r| (name, r))
        }
        PickTarget::RunRole(r) => app.run.as_ref()?.pattern.roles.get(r).cloned().map(|x| (r.clone(), x)),
        PickTarget::StudioField { role, .. } => app.studio.pattern.roles.get(role).cloned().map(|x| (role.clone(), x)),
        PickTarget::NewSessions => None,
    }
}

/// Is the scope switch (this agent / its whole role) offered: an agent of the active run.
fn picker_has_scope(app: &App, target: &PickTarget) -> bool {
    matches!(target, PickTarget::Agent(_)) && picker_role(app, target).is_some()
}

fn model_picker(f: &mut Frame, area: Rect, app: &App, sel: usize, target: &PickTarget, role_scope: bool, filter: &str) {
    let reg = &app.registry;
    let picks = app.picker_rows(target, filter);
    let n = picks.len();
    // Box: as many rows as there are, up to the room above the composer and the footer (four rows
    // at the bottom stay as they are: a box that ended inside the composer left its bottom border
    // under the overlay); beyond that the rows scroll and the selection stays in view.
    let room = if area.height >= 16 { Rect { height: area.height - 4, ..area } } else { area };
    let r = centered(room, 132, n as u16 + 7);
    clear(f, area, r);
    let iw = r.width.saturating_sub(2) as usize;
    let vis = (r.height.saturating_sub(2) as usize).saturating_sub(5).max(1);
    let start = if n <= vis { 0 } else { sel.min(n - 1).saturating_sub(vis / 2).min(n - vis) };
    let window = if n > vis { format!(" · {}-{} of {n}", start + 1, start + vis) } else { String::new() };
    let narrow = iw < 98;
    let bar = iw >= 110;
    let cur_alias = app.picker_current(target);
    let agent = if let PickTarget::Agent(a) = target { app.agents.get(a) } else { None };
    let alive = if let PickTarget::Agent(a) = target { app.agent_alive(*a) } else { true };
    let role = picker_role(app, target);
    let models = || picks.iter().filter_map(|p| if let PickRow::Model(i) = p { Some(&reg.models[*i]) } else { None });

    // ── line 1: the target, and the scope switch
    let role_name = role.as_ref().map(|(n, _)| n.clone());
    let (label_full, label_short) = match target {
        PickTarget::NewSessions => ("new Solo sessions".to_string(), "new Solo sessions".to_string()),
        PickTarget::Agent(_) => {
            let name = agent.map(|a| a.name.clone()).unwrap_or_default();
            let (full, short) = match &role_name {
                Some(r) if r != &name => (format!("{name} · role {r}"), name.clone()),
                _ => (name.clone(), name.clone()),
            };
            if alive {
                (full, short)
            } else {
                (format!("{name} (not running) — its next start"), format!("{name} (not running) — its next start"))
            }
        }
        PickTarget::RunRole(r) => (format!("role {r} — this run"), format!("role {r} — this run")),
        PickTarget::StudioField { role, field } => {
            let what = match field.as_str() {
                "model" => "model",
                "fallback" => "fallback 1",
                "fallback2" => "fallback 2",
                _ => "fallback 3",
            };
            let l = format!("{role} · {what} — pattern {}", app.studio.pattern.name);
            (l.clone(), l)
        }
    };
    let mut line1 = vec![];
    let scope = picker_has_scope(app, target).then(|| role_name.clone().unwrap_or_default());
    let fits = |left: &str, right: usize| 2 + 5 + w_of(left) + w_of(&window) + 3 + right <= iw;
    if let Some(rn) = scope {
        let a_on = !role_scope;
        let long = (" this agent ".to_string(), format!(" role {rn} (all its agents) "), "apply to:  ".to_string());
        let short = (" agent ".to_string(), " role ".to_string(), "apply to:  ".to_string());
        let wide = |t: &(String, String, String)| w_of(&t.0) + w_of(&t.1) + w_of(&t.2) + 4 + 3;
        let (left, parts) = if fits(&label_full, wide(&long)) {
            (label_full.clone(), Some(long))
        } else if fits(&label_short, wide(&long)) {
            (label_short.clone(), Some(long))
        } else if fits(&label_short, wide(&short)) {
            (label_short.clone(), Some(short))
        } else {
            (trunc(&label_short, iw.saturating_sub(4 + w_of(&window))), None)
        };
        line1.push(Span::styled(format!("  for: {left}{window}"), theme::dim()));
        if let Some((mine, role_s, pre)) = parts {
            let used = Line::from(line1.clone()).width() + w_of(&pre) + w_of(&mine) + w_of(&role_s) + 4 + 3;
            line1.push(Span::raw(" ".repeat(iw.saturating_sub(used))));
            line1.push(Span::styled(pre, theme::faint()));
            let (on, off) = (theme::bold(theme::accent()), theme::dim());
            line1.push(Span::styled(if a_on { format!("[{mine}]") } else { format!(" {mine} ") }, if a_on { on } else { off }));
            line1.push(Span::raw("  "));
            line1.push(Span::styled(if role_scope { format!("[{role_s}]") } else { format!(" {role_s} ") }, if role_scope { on } else { off }));
        }
    } else {
        let left = if fits(&label_full, 0) { label_full.clone() } else { trunc(&label_full, iw.saturating_sub(4 + w_of(&window))) };
        line1.push(Span::styled(format!("  for: {left}{window}"), theme::dim()));
        if matches!(target, PickTarget::RunRole(_)) {
            let note = "applies to every agent of the role from now on";
            let used = Line::from(line1.clone()).width();
            if used + w_of(note) + 3 <= iw {
                line1.push(Span::raw(" ".repeat(iw - used - w_of(note) - 1)));
                line1.push(Span::styled(note, theme::faint()));
            }
        }
    }
    let mut l = vec![Line::from(line1)];

    // ── line 2: the filter (always reserved)
    l.push(if filter.is_empty() { Line::default() } else { Line::from(Span::styled(format!("  filter: {filter}{}", theme::g("▏", "|")), theme::accent())) });

    // ── column widths
    let alias_max = models().map(|m| w_of(&m.alias)).max().unwrap_or(0).clamp(8, 24);
    let model_max = models().map(|m| w_of(&m.model)).max().unwrap_or(0).clamp(12, 36);
    let aw = if iw < 84 { 12 } else { alias_max + 2 };
    let mw = if narrow { 14 } else { model_max + 2 };
    // the provider column fits the widest name (mark and space included), a space after it
    let pname = models().map(|m| w_of(&reg.provider_name(&m.provider))).max().unwrap_or(0);
    let pw = (pname + 2).clamp(10, 18);
    let bw = if narrow { 0 } else { 9 };
    let ew = 8;
    let head = |s: &str, w: usize| format!("{s:<w$}");
    l.push(Line::from(Span::styled(format!("   {}{}{}{}{}{}{}", head("alias", aw), head("model", mw), head("provider", pw + 1), head("", bw), head("effort", ew), head("", if bar { 8 } else { 0 }), "context"), theme::bold(theme::muted()))));

    // ── rows
    let role_model = role.as_ref().map(|(_, r)| r.model.clone());
    let chain = role.as_ref().map(|(_, r)| r.fallback_chain()).unwrap_or_default();
    let on_pin = matches!(target, PickTarget::Agent(_)) && role_model.is_some() && cur_alias != role_model;
    let mut band = None;
    for (i, pick) in picks.iter().enumerate().skip(start).take(vis) {
        let is_sel = i == sel;
        let st = if is_sel { theme::bold(theme::accent()) } else { theme::text() };
        if is_sel {
            band = Some(l.len());
        }
        let cursor = Span::styled(format!(" {} ", if is_sel { theme::g("▶", ">") } else { " " }), st);
        let m = match pick {
            PickRow::None => {
                l.push(Line::from(vec![cursor, Span::styled(format!("{:<aw$}", "none"), st), Span::styled(format!("{} no fallback at this position", theme::g("—", "-")), theme::dim())]));
                continue;
            }
            PickRow::Model(i) => &reg.models[*i],
        };
        let is_cur = cur_alias.as_deref() == Some(m.alias.as_str());
        let eff = if m.efforts().is_empty() { "—".to_string() } else if is_cur && agent.is_some() { agent.map(|a| a.effort.clone()).unwrap_or_default() } else { m.default_effort.clone() };
        let ctx = if reg.backend_of(m) == crate::config::ProviderKind::Antigravity { "agy context".into() } else { m.context_window.map(|c| format!("{}k ctx", c / 1000)).unwrap_or_else(|| "default ctx".into()) };
        let mut row = vec![cursor, Span::styled(format!("{:<aw$}", trunc(&m.alias, aw - 1)), st), Span::styled(format!("{:<mw$}", crate::util::trunc_start(&m.model, mw - 1)), theme::muted())];
        // the provider's chip (its colour), padded to the column and a space: the backend tag never touches it
        let chip = crate::ui::provider_chip(reg, &m.provider, pw - 2);
        let chip_w = Line::from(chip.clone()).width();
        row.extend(chip);
        row.push(Span::raw(" ".repeat((pw + 1).saturating_sub(chip_w))));
        if bw > 0 {
            row.push(Span::styled(format!("{:<bw$}", backend_glyph(reg, &m.provider)), theme::faint()));
        }
        row.push(Span::styled(format!("{eff:<ew$}"), theme::fg(effort_color(&eff))));
        if bar {
            row.push(Span::styled(format!("{:<8}", theme::effort_bar(&eff, &m.efforts())), theme::fg(effort_color(&eff))));
        }
        row.push(Span::styled(format!("{ctx:<11}"), theme::dim()));
        // what is left goes to marks, in this order, each whole or not at all
        let mut left = iw.saturating_sub(Line::from(row.clone()).width() + 1);
        let mut marks: Vec<(String, ratatui::style::Style)> = vec![];
        if is_cur {
            marks.push((format!(" {} current", theme::g("●", "*")), theme::fg(theme::GREEN)));
        }
        if on_pin && role_model.as_deref() == Some(m.alias.as_str()) {
            marks.push((format!(" {} role", theme::g("◆", "*")), theme::faint()));
        }
        if let Some(p) = chain.iter().position(|x| *x == m.alias) {
            marks.push((format!(" {} fallback {}", theme::g("↪", ">"), p + 1), theme::faint()));
        }
        let note = match (m.note.is_empty(), m.vision) {
            (_, false) => m.note.clone(),
            (true, true) => "vision".to_string(),
            (false, true) => format!("vision · {}", m.note),
        };
        for (s, st) in marks {
            if w_of(&s) <= left {
                left -= w_of(&s);
                row.push(Span::styled(s, st));
            }
        }
        // the note whole, or cut when there is room for a useful part, or just "vision"
        if !note.is_empty() && w_of(&note) < left {
            row.push(Span::styled(format!(" {note}"), theme::faint()));
        } else if left >= 16 {
            row.push(Span::styled(format!(" {}", trunc(&note, left - 1)), theme::faint()));
        } else if m.vision && left >= 7 {
            row.push(Span::styled(" vision", theme::faint()));
        }
        l.push(Line::from(row));
    }
    if n == 0 {
        l.push(Line::from(Span::styled("   no model matches the filter", theme::dim())));
    }

    // ── keys
    l.push(Line::default());
    let mut keys: Vec<&str> = vec![if narrow { "↑↓" } else { "↑↓ model" }, if narrow { "type filters" } else { "type to filter" }];
    if picker_has_scope(app, target) {
        keys.push("tab scope");
    }
    if matches!(target, PickTarget::Agent(_)) && alive {
        keys.push("←→ effort");
    }
    if !narrow {
        keys.push("+/- ctx");
    }
    keys.push("⏎ apply");
    if !narrow {
        keys.push("ctrl+e edit models");
    }
    keys.push("esc");
    l.push(Line::from(Span::styled(trunc(&format!("  {}", keys.join(" · ")), iw), theme::faint())));

    let blk = block("model & effort", theme::SIGNAL_RED);
    let inner = blk.inner(r);
    f.render_widget(Paragraph::new(l).block(blk), r);
    if let Some(y) = band {
        form::band_line(f.buffer_mut(), inner, y, 0);
    }
}

/// `/run` and `/pattern`: the patterns (name, tag, description; the one new runs use marked), and
/// for `/run` the goal to start on the selected one.
fn new_run(f: &mut Frame, area: Rect, app: &App, sel: usize, list: &[PatternChoice], goal: Option<&crate::ui::input::Input>) {
    let w = 104u16.min(area.width.saturating_sub(2));
    let iw = w.saturating_sub(2) as usize;
    let goal_rows = goal.map(|g| g.layout(iw.saturating_sub(9)).0.len().clamp(1, 4)).unwrap_or(0);
    // the list, a blank line and the keys inside the border — and the goal with a blank line after it
    let h = list.len() as u16 + 4 + if goal.is_some() { goal_rows as u16 + 1 } else { 0 };
    let r = centered(area, w, h);
    clear(f, area, r);
    let title = if goal.is_some() { "new run" } else { "pattern for new runs" };
    let blk = block(title, theme::SIGNAL_RED);
    let inner = blk.inner(r);
    f.render_widget(blk, r);
    let nw = list.iter().map(|p| w_of(&p.name)).max().unwrap_or(0).max(14) + 2;
    let mark = format!("  {} for new runs", theme::g("●", "*"));
    let mut l: Vec<Line> = vec![];
    for (i, p) in list.iter().enumerate() {
        let is = i == sel;
        let on = p.name == app.pattern_name;
        let st = if is { theme::bold(theme::accent()) } else { theme::bold(theme::text()) };
        let tag_st = match p.tag {
            "default" => theme::fg(theme::GREEN),
            "yours" | "edited" => theme::fg(theme::VIOLET),
            _ => theme::faint(),
        };
        // the description takes what the name, the tag and the mark leave, a column short of the border
        let dw = iw.saturating_sub(3 + nw + 10 + if on { w_of(&mark) } else { 0 });
        let (about, about_st) = match &p.about {
            Ok(d) => (trunc(d, dw), theme::muted()),
            Err(e) => (trunc(&format!("{} does not load: {e}", theme::g("⚠", "!")), dw), theme::fg(theme::RED)),
        };
        l.push(Line::from(vec![
            Span::styled(format!("  {:<nw$}", if p.name == "auto" { "Auto" } else { &p.name }), st),
            Span::styled(format!("{:<10}", p.tag), tag_st),
            Span::styled(format!("{about:<dw$}"), about_st),
            Span::styled(if on { mark.clone() } else { String::new() }, theme::fg(theme::GREEN)),
        ]));
    }
    l.push(Line::default());
    let mut cursor = None;
    if let Some(g) = goal {
        let gw = iw.saturating_sub(9);
        let (rows, cr, cc) = g.layout(gw);
        let start = cr.saturating_sub(goal_rows - 1);
        if g.is_empty() {
            l.push(Line::from(vec![Span::styled("  goal › ", theme::bold(theme::accent())), Span::styled("describe what to build — the planner takes it from there", theme::faint())]));
        }
        for (k, row) in rows.iter().enumerate().skip(start).take(goal_rows).filter(|_| !g.is_empty()) {
            let lead = if k == start { "  goal › " } else { "         " };
            l.push(Line::from(vec![Span::styled(lead, theme::bold(theme::accent())), Span::styled(row.clone(), theme::text())]));
        }
        cursor = Some((inner.x + 9 + cc as u16, inner.y + (list.len() + 1 + cr - start) as u16));
        l.push(Line::default());
    }
    let keys = match form::armed(&app.confirm) {
        Some(what) => format!("  del again to delete {what} · esc keeps it"),
        // with a goal typed, del is the goal's
        None if goal.is_some_and(|g| !g.text().is_empty()) => "  ↑↓ pattern · ⏎ start the run · ctrl+e edit it in the studio · esc".into(),
        None if goal.is_some() => "  ↑↓ pattern · ⏎ start the run · ctrl+e edit it in the studio · del delete yours · esc".into(),
        None => "  ↑↓ pattern · ⏎ use it for new runs · e edit it in the studio · del delete yours · esc".into(),
    };
    l.push(Line::from(Span::styled(keys, theme::faint())));
    f.render_widget(Paragraph::new(l), inner);
    form::band_line(f.buffer_mut(), inner, sel, 0);
    if let Some(c) = cursor {
        f.set_cursor_position(c);
    }
}

/// A short list to choose from (`Overlay::Select`): label and detail, ticks when it takes several.
fn select(f: &mut Frame, area: Rect, s: &Select) {
    let lw = s.items.iter().map(|c| w_of(&c.label)).max().unwrap_or(0) + 2;
    let dw = s.items.iter().map(|c| w_of(&c.detail)).max().unwrap_or(0);
    let tick = if s.multi { 4 } else { 0 };
    // a column of space each side of the text, inside the border
    let w = ((2 + tick + lw + dw + 4) as u16).clamp(44, 100);
    let r = centered(area, w, s.items.len() as u16 + 4);
    clear(f, area, r);
    let blk = block(&s.title, theme::SIGNAL_RED);
    let inner = blk.inner(r);
    f.render_widget(blk, r);
    let room = (inner.width as usize).saturating_sub(2 + tick + lw);
    let vis = (inner.height as usize).saturating_sub(2).max(1);
    let start = form::scroll_for(s.sel, 0, vis, s.items.len());
    let mut l: Vec<Line> = vec![];
    for (i, c) in s.items.iter().enumerate().skip(start).take(vis) {
        let st = if i == s.sel { theme::bold(theme::accent()) } else { theme::text() };
        let mut spans = vec![Span::raw("  ")];
        if s.multi {
            spans.push(Span::styled(if c.on { "[x] " } else { "[ ] " }, st));
        }
        spans.push(Span::styled(format!("{:<lw$}", c.label), st));
        spans.push(Span::styled(trunc(&c.detail, room), theme::muted()));
        l.push(Line::from(spans));
    }
    l.push(Line::default());
    let keys = if s.multi { "  ↑↓ · space tick · ⏎ take the ticked ones · esc" } else { "  ↑↓ · ⏎ choose · esc" };
    l.push(Line::from(Span::styled(keys, theme::faint())));
    f.render_widget(Paragraph::new(l), inner);
    form::band_line(f.buffer_mut(), inner, s.sel, start);
}

fn diff(f: &mut Frame, area: Rect, app: &App, agent: crate::hub::AgentId, file: usize, scroll: usize) -> Option<usize> {
    let r = Rect { x: area.x + 2, y: area.y + 1, width: area.width.saturating_sub(4), height: area.height.saturating_sub(2) };
    clear(f, area, r);
    let a = app.agents.get(&agent)?;
    let blk = block(&format!("changes · {}", a.name), theme::AMBER);
    let inner = blk.inner(r);
    f.render_widget(blk, r);
    if a.files.is_empty() {
        f.render_widget(Paragraph::new(Span::styled("  no changes yet", theme::dim())), inner);
        return Some(0);
    }
    let list_w = 34.min(inner.width / 3);
    let files: Vec<(&String, &crate::agent::FileStat)> = a.files.iter().collect();
    let file = file.min(files.len() - 1);
    let fl: Vec<Line> = files
        .iter()
        .enumerate()
        .map(|(i, (p, s))| {
            let st = if i == file { theme::bold(theme::accent()) } else { theme::text() };
            Line::from(vec![Span::styled(format!(" {:<w$}", trunc(p, (list_w as usize).saturating_sub(12)), w = (list_w as usize).saturating_sub(11)), st), Span::styled(format!("+{}", s.adds), theme::fg(theme::GREEN)), Span::styled(format!(" -{}", s.dels), theme::fg(theme::RED))])
        })
        .collect();
    let lr = Rect { width: list_w, ..inner };
    f.render_widget(Paragraph::new(fl), lr);
    let dr = Rect { x: inner.x + list_w + 1, width: inner.width.saturating_sub(list_w + 1), ..inner };
    let blk2 = Block::default().borders(Borders::LEFT).border_style(theme::faint());
    let di = blk2.inner(dr);
    f.render_widget(blk2, dr);
    let (path, stat) = files[file];
    let mut lines = vec![Line::from(Span::styled(format!(" {path}"), theme::bold(theme::text()))), Line::default()];
    let w = di.width as usize;
    let mut old_n = 0usize;
    let mut new_n = 0usize;
    // the offset counts the diff's own lines: the last screenful (under the path) is as far as it goes
    let max = stat.diff.lines().count().saturating_sub((di.height as usize).saturating_sub(2));
    for l in stat.diff.lines().skip(scroll.min(max)) {
        if l.starts_with("diff --git") || l.starts_with("index ") || l.starts_with("---") || l.starts_with("+++") || l.starts_with("new file") {
            continue;
        }
        if let Some(h) = l.strip_prefix("@@ ") {
            // @@ -a,b +c,d @@
            let nums: Vec<usize> = h.split(|c: char| !c.is_ascii_digit()).filter_map(|x| x.parse().ok()).collect();
            if nums.len() >= 3 {
                old_n = nums[0];
                new_n = nums[2];
            }
            lines.push(Line::from(Span::styled(format!(" {}", trunc(l, w.saturating_sub(2))), theme::fg(theme::VIOLET))));
            continue;
        }
        let (st, num) = if l.starts_with('+') {
            new_n += 1;
            (theme::fg(theme::GREEN), format!("{:>4} ", new_n.saturating_sub(1).max(1)))
        } else if l.starts_with('-') {
            old_n += 1;
            (theme::fg(theme::RED), format!("{:>4} ", old_n.saturating_sub(1).max(1)))
        } else {
            old_n += 1;
            new_n += 1;
            (theme::dim(), format!("{:>4} ", new_n.saturating_sub(1).max(1)))
        };
        lines.push(Line::from(vec![Span::styled(num, theme::faint()), Span::styled(trunc(l, w.saturating_sub(6)), st)]));
        if lines.len() > di.height as usize {
            break;
        }
    }
    f.render_widget(Paragraph::new(lines), di);
    let hint = " ↑↓ scroll · tab/←→ file · esc close ";
    f.render_widget(Paragraph::new(Span::styled(hint, theme::faint())), Rect { x: r.x + 2, y: r.y + r.height.saturating_sub(1), width: (w_of(hint) as u16).min(r.width.saturating_sub(4)), height: 1 });
    Some(max)
}

fn inbox(f: &mut Frame, area: Rect, app: &App, sel: usize) {
    let alerts: Vec<String> = app.run.as_ref().map(|r| r.alerts.clone()).unwrap_or_default();
    let r = centered(area, 90, (app.approvals.len() as u16 * 3 + alerts.len() as u16 + 8).min(area.height.saturating_sub(2)));
    clear(f, area, r);
    let mut l = vec![];
    if app.approvals.is_empty() && alerts.is_empty() {
        l.push(Line::from(Span::styled("  all clear — nothing needs you right now", theme::dim())));
    }
    for (i, ap) in app.approvals.iter().enumerate() {
        let name = app.agents.get(&ap.agent).map(|a| format!("{} {}", theme::role_glyph(&a.glyph), a.name)).unwrap_or_default();
        let st = if i == sel { theme::bold(theme::accent()) } else { theme::text() };
        l.push(Line::from(vec![Span::styled(format!(" {} ", if i == sel { theme::g("▶", ">") } else { " " }), st), Span::styled(format!("{name}  "), theme::bold(theme::text())), Span::styled(ap.title.clone(), st), Span::styled(format!("  {}", fmt_dur(ap.at.elapsed())), theme::faint())]));
        for d in ap.detail.lines().take(2) {
            l.push(Line::from(Span::styled(format!("     {}", trunc(d, (r.width as usize).saturating_sub(8))), theme::muted())));
        }
    }
    if !alerts.is_empty() {
        l.push(Line::default());
        l.push(Line::from(Span::styled(" alerts", theme::bold(theme::fg(theme::AMBER)))));
        for a in alerts.iter().rev().take(6) {
            l.push(Line::from(Span::styled(format!("  {} {}", theme::g("⚑", "!"), trunc(&a.replace("\r\n", "\n").replace('\n', &format!(" {} ", theme::g("⏎", "|"))), (r.width as usize).saturating_sub(8))), theme::fg(theme::AMBER))));
        }
    }
    l.push(Line::default());
    l.push(Line::from(Span::styled("  ↑↓ select · y approve · a approve for session · n decline · ⏎ zoom to agent · esc", theme::faint())));
    f.render_widget(Paragraph::new(l).block(block("inbox", theme::AMBER)), r);
}

fn plan(f: &mut Frame, area: Rect, app: &App, scroll: usize) -> usize {
    let r = Rect { x: area.x + 3, y: area.y + 1, width: area.width.saturating_sub(6), height: area.height.saturating_sub(2) };
    clear(f, area, r);
    let review = app.run.as_ref().map(|x| x.stage == Stage::Review).unwrap_or(false);
    let title = if review { "plan review" } else { "plan" };
    let blk = block(title, theme::SIGNAL_RED);
    let inner = blk.inner(r);
    f.render_widget(blk, r);
    let body = match app.run.as_ref().and_then(|x| x.plan.as_ref()) {
        Some(p) => p.to_markdown(),
        None => "No plan yet — the planner is still working.".into(),
    };
    let lines = md::render(&body, inner.width.saturating_sub(2) as usize, theme::text());
    let h = inner.height.saturating_sub(1) as usize;
    let max = lines.len().saturating_sub(h);
    let view: Vec<Line> = lines.into_iter().skip(scroll.min(max)).take(h).collect();
    f.render_widget(Paragraph::new(view), Rect { x: inner.x + 1, width: inner.width.saturating_sub(1), height: h as u16, ..inner });
    let workflow = app.run.as_ref().filter(|r| r.is_active() && !r.pattern.is_auto()).map(|r| {
        if r.pattern.settings.allow_pattern_changes { " F3 workflow: allowed ·" } else { " F3 workflow: locked ·" }
    }).unwrap_or("");
    let controls = if review { " a approve & build · f give feedback · ↑↓ scroll · esc " } else { " ↑↓ scroll · esc " };
    let hint = format!("{workflow}{controls}");
    f.render_widget(Paragraph::new(Span::styled(hint, theme::bold(theme::accent()))), Rect { x: inner.x + 1, y: inner.y + inner.height.saturating_sub(1), width: inner.width.saturating_sub(1), height: 1 });
    max
}

/// The halt in full: the band cuts the message to one line, the backend's error can be long.
fn halt(f: &mut Frame, area: Rect, app: &App, scroll: usize) -> Option<usize> {
    let run = app.run.as_ref()?;
    let w = 100u16.min(area.width.saturating_sub(2));
    let tw = w.saturating_sub(4) as usize;
    let mut l: Vec<Line<'static>> = vec![];
    match &run.halt {
        Some(h) => {
            let why = match h.reason {
                HaltReason::User => "paused by you",
                HaltReason::Auth => "the provider refused the credentials",
                HaltReason::UsageLimit => "a usage limit was hit",
                HaltReason::ProviderRejected => "the provider rejected this model for the role",
                HaltReason::Environment => "this machine can't run the agents",
                HaltReason::GateExhausted => "the phase gate ran out of rounds",
                HaltReason::AttemptsExhausted => "a task ran out of attempts",
                HaltReason::AgentTurnFailed => "an agent's turn kept failing",
            };
            l.push(Line::from(vec![Span::styled(format!(" {} halted {} · ", theme::g("⛔", "X"), fmt_dur(h.since.elapsed())), theme::bold(theme::fg(theme::AMBER))), Span::styled(why, theme::text())]));
            l.push(Line::default());
            let pad = || vec![Span::raw(" ")];
            for para in h.full.lines() {
                l.extend(md::wrap(vec![Span::styled(para.to_string(), theme::text())], tw, pad(), pad()));
            }
            l.push(Line::default());
            l.extend(md::wrap(vec![Span::styled(run.halt_hint(), theme::bold(theme::accent()))], tw, vec![Span::styled(" what now  ", theme::faint())], vec![Span::raw("           ")]));
            l.extend(md::wrap(vec![Span::styled("a model change (m) takes effect at the next spawn but does not resume the run: press space when you are ready.", theme::muted())], tw, vec![Span::raw("           ")], vec![Span::raw("           ")]));
        }
        None => l.push(Line::from(Span::styled(" the run is no longer halted", theme::fg(theme::GREEN)))),
    }
    let r = centered(area, w, l.len() as u16 + 3);
    clear(f, area, r);
    let blk = block("halt", theme::AMBER);
    let inner = blk.inner(r);
    f.render_widget(blk, r);
    let h = inner.height.saturating_sub(1) as usize;
    let more = if l.len() > h { " · ↑↓ scroll" } else { "" };
    let max = l.len().saturating_sub(h);
    let view: Vec<Line> = l.into_iter().skip(scroll.min(max)).take(h).collect();
    f.render_widget(Paragraph::new(view), Rect { height: h as u16, ..inner });
    let hint = if run.halt.is_some() { format!(" space resume · m model{more} · esc ") } else { " esc ".into() };
    f.render_widget(Paragraph::new(Span::styled(hint, theme::bold(theme::accent()))), Rect { x: inner.x + 1, y: inner.y + inner.height.saturating_sub(1), width: inner.width.saturating_sub(1), height: 1 });
    Some(max)
}

/// Shared notes, one section per run or scoped swarm group. Notes are the block exactly as its
/// agents read it (`Block::render`); a swarm adds its parent and children's current status.
fn akasha(f: &mut Frame, area: Rect, app: &App, group: Option<&str>, scroll: usize) -> usize {
    let r = Rect { x: area.x + 3, y: area.y + 1, width: area.width.saturating_sub(6), height: area.height.saturating_sub(2) };
    clear(f, area, r);
    let blk = block(&group.map(|g| format!("akasha · {g}")).unwrap_or_else(|| "akasha".into()), theme::SIGNAL_RED);
    let inner = blk.inner(r);
    f.render_widget(blk, r);
    let width = inner.width.saturating_sub(2) as usize;
    let mut lines: Vec<Line<'static>> = vec![];
    for b in app.run.iter().flat_map(|r| r.akasha.values()).filter(|b| group.map(|g| g == App::akasha_run_key(&b.group)).unwrap_or(true)) {
        if !lines.is_empty() {
            lines.push(Line::default());
        }
        lines.extend(akasha_lines(&b.render(), width));
    }
    let mut swarms: Vec<_> = app.swarms.values().filter(|s| group.map(|g| g == format!("swarm:{}", s.owner)).unwrap_or(true)).collect();
    swarms.sort_by_key(|s| s.owner);
    for s in swarms {
        if !lines.is_empty() {
            lines.push(Line::default());
        }
        lines.push(Line::from(Span::styled(trunc(&format!("swarm · {} · {} agents", s.name, s.children.len()), width), theme::bold(theme::fg(theme::VIOLET)))));
        lines.push(swarm_member_line(app, s.owner, "owner", width));
        for &child in &s.children {
            lines.push(swarm_member_line(app, child, "  ↳", width));
        }
        lines.push(Line::default());
        lines.extend(akasha_lines(&s.notes.render(), width));
    }
    // the run can be closed while this is open: say so instead of drawing an empty box
    if lines.is_empty() {
        lines.push(Line::from(Span::styled("no akasha groups in this run", theme::muted())));
    }
    let h = inner.height.saturating_sub(1) as usize;
    let max = lines.len().saturating_sub(h);
    let view: Vec<Line> = lines.into_iter().skip(scroll.min(max)).take(h).collect();
    f.render_widget(Paragraph::new(view), Rect { x: inner.x + 1, width: inner.width.saturating_sub(1), height: h as u16, ..inner });
    let hint = if max > 0 { " ↑↓ scroll · esc " } else { " esc " };
    f.render_widget(Paragraph::new(Span::styled(hint, theme::faint())), Rect { x: inner.x + 1, y: inner.y + inner.height.saturating_sub(1), width: inner.width.saturating_sub(1), height: 1 });
    max
}

fn swarm_member_line(app: &App, id: crate::hub::AgentId, role: &str, width: usize) -> Line<'static> {
    let Some(a) = app.agents.get(&id) else {
        return Line::from(Span::styled(trunc(&format!("{role} #{id} · closed"), width), theme::faint()));
    };
    let (status, color) = if a.stopped_by_user {
        ("stopped by you".to_string(), theme::AMBER)
    } else {
        match &a.status {
            Status::Failed(why) | Status::Crashed(why) => (format!("failed: {why}"), theme::RED),
            Status::Retrying(why) => (Status::retrying_text(why, 40), theme::AMBER),
            Status::Waiting => ("waiting for approval".to_string(), theme::AMBER),
            Status::Starting => ("starting".to_string(), theme::MUTED),
            Status::Stopped => ("stopped".to_string(), theme::MUTED),
            _ if a.swarm_waiting => ("waiting for Akasha swarm".to_string(), theme::AMBER),
            _ if producing(a) => (if a.activity.is_empty() { "working".to_string() } else { a.activity.clone() }, theme::TEAL),
            _ => ("idle".to_string(), theme::MUTED),
        }
    };
    Line::from(Span::styled(trunc(&format!("{role} {} · {status}", a.name), width), theme::fg(color)))
}

/// A rendered block, styled by what each line is: the `## akasha · …` header, a note's
/// `[7 · w2 · 14:02]` head (amber after a `!` when urgent), and the indented lines a note's text
/// continues on. A long line wraps under the note's text, not under its head.
fn akasha_lines(rendered: &str, width: usize) -> Vec<Line<'static>> {
    let mut out = vec![];
    for line in rendered.lines() {
        if let Some(head) = line.strip_prefix("## ") {
            out.push(Line::from(Span::styled(trunc(head, width), theme::bold(theme::fg(theme::VIOLET)))));
            continue;
        }
        let (urgent, rest) = match line.strip_prefix('!') {
            Some(r) => (true, r),
            None => (false, line),
        };
        // neither a note's head nor its next line: the empty block's "(no notes yet)"
        if !rest.starts_with('[') && !rest.starts_with("    ") {
            out.push(Line::from(vec![Span::raw("  "), Span::styled(rest.to_string(), theme::muted())]));
            continue;
        }
        let mark = if urgent { Span::styled("! ", theme::bold(theme::fg(theme::AMBER))) } else { Span::raw("  ") };
        let (meta, text) = match rest.split_once("] ").filter(|_| rest.starts_with('[')) {
            Some((m, t)) => (format!("{m}] "), t),
            // a note's next line: under its text, keeping its own indentation past the render's
            None => ("  ".to_string(), rest.strip_prefix("    ").unwrap_or(rest)),
        };
        let meta_st = if urgent { theme::fg(theme::AMBER) } else { theme::faint() };
        let first = vec![mark, Span::styled(meta, meta_st)];
        out.extend(md::wrap(vec![Span::styled(text.to_string(), theme::text())], width, first, vec![Span::raw("    ")]));
    }
    out
}

fn discover(f: &mut Frame, area: Rect, app: &App, st: &crate::app::DiscoverState) {
    use crate::discover::CState;
    let r = Rect { x: area.x + 2, y: area.y + 1, width: area.width.saturating_sub(4), height: area.height.saturating_sub(2) };
    clear(f, area, r);
    let title = st.origin.as_ref().map(|h| format!("models of {h}")).unwrap_or_else(|| "discover models".into());
    let blk = block(&title, theme::SIGNAL_RED);
    let inner = blk.inner(r);
    f.render_widget(blk, r);
    let w = inner.width as usize;
    let mut l: Vec<Line> = vec![];
    if !st.loading.is_empty() {
        // Each network source names its destination host ("riti → api.riti.dev"), so it is
        // visible where every key is being sent.
        let arrow = theme::g("→", "->");
        let who: Vec<String> = st.loading.iter().map(|s| match st.hosts.iter().find(|(id, _)| id == s) { Some((_, h)) => format!("{s} {arrow} {h}"), None => s.clone() }).collect();
        l.push(Line::from(vec![Span::styled(format!(" {} ", anim::spinner()), theme::fg(theme::SAFFRON)), Span::styled(format!("asking {}…", who.join(", ")), theme::muted())]));
    }
    for e in &st.errors {
        l.push(Line::from(Span::styled(format!(" {} {}", theme::g("✗", "x"), trunc(e, w.saturating_sub(4))), theme::fg(theme::RED))));
    }
    for s in &st.skipped {
        l.push(Line::from(Span::styled(format!(" {} {}", theme::g("‖", "-"), trunc(s, w.saturating_sub(4))), theme::fg(theme::AMBER))));
    }
    let vis = st.visible();
    let nsel = st.items.iter().filter(|c| c.selected).count();
    let ftxt = st.filter.text();
    let left = format!(" filter: {}{}", ftxt, theme::g("▏", "_"));
    let right = format!("{} shown · {} selected ", vis.len(), nsel);
    l.push(Line::from(vec![Span::styled(left.clone(), if ftxt.is_empty() { theme::faint() } else { theme::bold(theme::text()) }), Span::raw(" ".repeat(w.saturating_sub(w_of(&left) + w_of(&right)))), Span::styled(right, theme::muted())]));
    // Columns as wide as what they hold ("200.0k assumed", "minimal…xhigh", "fill missing"), the
    // model taking the rest, so the status still reads whole at 80 columns.
    let mw = w.saturating_sub(58).max(12);
    l.push(Line::from(Span::styled(format!("     {:<10} {:<mw$} {:<14} {:<13} {}", "provider", "model", "context", "effort", "status"), theme::bold(theme::muted()))));
    let head = l.len();
    let room = (inner.height as usize).saturating_sub(head + 2).max(1);
    let sel = st.sel.min(vis.len().saturating_sub(1));
    let start = sel.saturating_sub(room.saturating_sub(1));
    let mut band = None;
    for (row, &i) in vis.iter().enumerate().skip(start).take(room) {
        let c = &st.items[i];
        let is = row == sel;
        if is {
            band = Some(l.len());
        }
        let mark = match (c.state, c.selected) {
            (CState::Configured, _) => format!(" {} ", theme::g("✓", "v")),
            (_, true) => "[x]".to_string(),
            _ => "[ ]".to_string(),
        };
        let ctx = match (c.context, c.context_known) {
            (Some(n), true) => fmt_tokens(n),
            (Some(n), false) => format!("{} assumed", fmt_tokens(n)),
            (None, _) => "codex knows".to_string(),
        };
        let eff = match (c.efforts.first(), c.efforts.last()) {
            (Some(a), Some(b)) => format!("{a}…{b}"),
            _ => "—".to_string(),
        };
        let (status, scol) = match c.state {
            CState::New => ("new", theme::GREEN),
            CState::Update => ("fill missing", theme::AMBER),
            CState::Configured => ("configured", theme::FAINT),
        };
        let base = if c.state == CState::Configured { theme::dim() } else { theme::text() };
        let st_row = if is { theme::bold(theme::accent()) } else { base };
        l.push(Line::from(vec![
            Span::styled(format!(" {} ", mark), st_row),
            Span::styled(format!("{:<10} ", trunc(&c.provider, 10)), theme::fg(theme::named(app.registry.provider_color(&c.provider)))),
            Span::styled(format!("{:<mw$} ", trunc(&c.model, mw)), st_row),
            Span::styled(format!("{:<14} ", trunc(&ctx, 14)), if c.context_known { theme::muted() } else { theme::fg(theme::AMBER) }),
            Span::styled(format!("{:<13} ", trunc(&eff, 13)), theme::muted()),
            Span::styled(status, theme::fg(scol)),
        ]));
    }
    if vis.is_empty() && st.loading.is_empty() {
        l.push(Line::from(Span::styled(if st.items.is_empty() { "  no models found" } else { "  nothing matches the filter" }, theme::dim())));
    }
    f.render_widget(Paragraph::new(l), inner);
    if let Some(y) = band {
        form::band_line(f.buffer_mut(), inner, y, 0);
    }
    let (note, keys) = if w >= 112 {
        (" unknown context → assumed 200k (edit in /models) · effort only where the provider reports reasoning support", " type to filter · ↑↓ move · space select · tab all/none · ⏎ add selected & save · esc close")
    } else {
        (" unknown context → 200k assumed · effort where the provider reports it", " type to filter · ↑↓ · space select · tab all · ⏎ add & save · esc")
    };
    let foot = vec![Line::from(Span::styled(note, theme::faint())), Line::from(Span::styled(keys, theme::bold(theme::accent())))];
    f.render_widget(Paragraph::new(foot), Rect { y: inner.y + inner.height.saturating_sub(2), height: 2.min(inner.height), ..inner });
}

fn runs(f: &mut Frame, area: Rect, app: &App, sel: usize, list: &[crate::engine::state::RunSummary], confirm: bool, others: usize) {
    use crate::engine::state::fmt_ago;
    let r = centered(area, 100, (list.len() as u16).max(1) + 5);
    clear(f, area, r);
    let inner_w = r.width.saturating_sub(4) as usize;
    let id_w = inner_w.saturating_sub(2 + 26 + 1 + 30 + 1 + 9 + 2).max(8);
    let mut l: Vec<Line> = vec![Line::from(Span::styled(format!("   {:<26} {:<30} {:>9}  {}", "run", "stage", "updated", "id"), theme::faint()))];
    if list.is_empty() {
        l.push(Line::from(Span::styled("   no runs for this project yet — /run starts one", theme::dim())));
    }
    for (i, run) in list.iter().enumerate() {
        let cur = app.run.as_ref().map(|x| x.id == run.id).unwrap_or(false);
        let unfinished = run.unfinished();
        let st = if i == sel { theme::bold(theme::accent()) } else if unfinished { theme::text() } else { theme::muted() };
        let stage_style = if run.state.is_err() {
            theme::fg(theme::ROSE)
        } else if unfinished {
            theme::fg(theme::AMBER)
        } else if run.stage_label() == "stopped" {
            theme::muted()
        } else {
            theme::fg(theme::GREEN)
        };
        l.push(Line::from(vec![
            Span::styled(format!("   {:<26}", trunc(&run.title(), 26)), st),
            Span::styled(format!(" {:<30}", trunc(&run.stage_label(), 30)), stage_style),
            Span::styled(format!(" {:>9}", fmt_ago(run.updated_unix)), theme::faint()),
            Span::styled(format!("  {}", trunc(&run.id, id_w)), theme::faint()),
            Span::styled(if cur { "  (open)".to_string() } else { String::new() }, theme::fg(theme::GREEN)),
        ]));
    }
    l.push(Line::default());
    if confirm {
        let id = list.get(sel).map(|r| r.title()).unwrap_or_default();
        l.push(Line::from(Span::styled(format!("  delete {id} — its branches, worktrees and journal? y / n"), theme::bold(theme::fg(theme::ROSE)))));
    } else {
        let mut hint = "  ⏎ open (resume where it stopped) · D delete · esc".to_string();
        if others > 0 {
            hint.push_str(&format!("   ({others} run{} in other projects: mantra runs)", if others == 1 { "" } else { "s" }));
        }
        l.push(Line::from(Span::styled(hint, theme::faint())));
    }
    let blk = block("runs", theme::SIGNAL_RED);
    let inner = blk.inner(r);
    f.render_widget(Paragraph::new(l).block(blk), r);
    if !list.is_empty() {
        form::band_line(f.buffer_mut(), inner, 1 + sel.min(list.len() - 1), 0);
    }
}

// ───────────────────────────── keys ─────────────────────────────

/// Delete on the new-run overlay's pattern: a file of yours goes on the second press (an edited
/// built-in is the built-in again); a built-in has none to delete. Returns the list as it is now.
fn delete_pattern(app: &mut App, list: Vec<PatternChoice>, sel: usize) -> Vec<PatternChoice> {
    let Some((name, tag)) = list.get(sel).map(|p| (p.name.clone(), p.tag)) else { return list };
    if !matches!(tag, "yours" | "edited") {
        app.toast(format!("{name} is built in — there is no file to delete"), crate::agent::Level::Info);
        return list;
    }
    if !form::confirm(&mut app.confirm, &name) {
        return list;
    }
    match crate::engine::pattern::Pattern::delete_file(&name, &app.project) {
        Ok(path) => {
            let after = if tag == "edited" { format!(" — {name} is the built-in again") } else { String::new() };
            app.toast(format!("deleted {}{after}", home_rel(&path)), crate::agent::Level::Ok);
            // new runs and the Studio leave a pattern that is gone
            if tag == "yours" && app.pattern_name == name {
                app.pattern_name = crate::engine::pattern::NAME_DEFAULT.into();
            }
            if app.studio.pattern.name == name && !app.studio.dirty {
                if let Ok(p) = crate::engine::pattern::Pattern::load(&app.pattern_name, &app.project) {
                    app.studio.pattern = p;
                }
            }
            PatternChoice::all(&app.project)
        }
        Err(e) => {
            app.toast(e, crate::agent::Level::Warn);
            list
        }
    }
}

/// What choosing in a `Select` does.
fn chosen(app: &mut App, s: &Select) {
    let key = s.items.get(s.sel).map(|c| c.key.clone()).unwrap_or_default();
    match s.then {
        SelectThen::AddMenu => super::models::add_chosen(app, &key),
        SelectThen::ProviderPreset => super::models::provider_preset_chosen(app, &key),
        SelectThen::AddModelOn => super::models::add_model_on(app, &key),
        SelectThen::Akasha => app.overlays.push(Overlay::Akasha { scroll: 0, group: Some(key) }),
        SelectThen::Respawn => {
            if let Ok(a) = key.parse() {
                app.respawn_agent(a);
            }
        }
        SelectThen::SoloMcp => {
            let ticked: Vec<String> = s.items.iter().filter(|c| c.on).map(|c| c.key.clone()).collect();
            super::settings::solo_mcp_chosen(app, &ticked);
        }
    }
}

pub fn key(app: &mut App, k: KeyEvent) {
    let Some(top) = app.overlays.pop() else { return };
    let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
    let keep = match top {
        Overlay::Help { scroll } => match k.code {
            KeyCode::Up | KeyCode::Char('k') => Some(Overlay::Help { scroll: scroll.saturating_sub(1) }),
            KeyCode::Down | KeyCode::Char('j') => Some(Overlay::Help { scroll: scroll + 1 }),
            KeyCode::PageUp => Some(Overlay::Help { scroll: scroll.saturating_sub(10) }),
            KeyCode::PageDown => Some(Overlay::Help { scroll: scroll + 10 }),
            _ => None,
        },
        Overlay::Qr => match k.code {
            KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') => None,
            _ => Some(Overlay::Qr),
        },
        Overlay::ModelPicker { mut sel, target, mut role_scope, mut filter } => {
            let rows = app.picker_rows(&target, &filter);
            let n = rows.len().max(1);
            let picked = |app: &App, sel: usize| match rows.get(sel) {
                Some(PickRow::Model(i)) => app.registry.models.get(*i).map(|m| m.alias.clone()),
                _ => None,
            };
            let scoped = matches!(target, PickTarget::Agent(_)) && app.picker_has_scope(&target);
            let mut stay = true;
            match k.code {
                KeyCode::Esc if !filter.is_empty() => {
                    filter.clear();
                    sel = 0;
                }
                KeyCode::Esc => stay = false,
                KeyCode::Up => sel = sel.saturating_sub(1),
                KeyCode::Down => sel = (sel + 1).min(n - 1),
                KeyCode::PageUp => sel = sel.saturating_sub(10),
                KeyCode::PageDown => sel = (sel + 10).min(n - 1),
                KeyCode::Tab if scoped => role_scope = !role_scope,
                KeyCode::Left | KeyCode::Right => {
                    if let PickTarget::Agent(a) = target {
                        if app.agent_alive(a) && !(role_scope && scoped) {
                            if let Some(alias) = picked(app, sel) {
                                if app.agents.get(&a).map(|x| x.model_alias != alias).unwrap_or(false) {
                                    match app.set_model(a, &alias) {
                                        Ok(()) => app.model_switched_for_run_agent(a),
                                        Err(e) => app.toast(e, crate::agent::Level::Warn),
                                    }
                                }
                            }
                            app.step_effort(a, if k.code == KeyCode::Right { 1 } else { -1 });
                        }
                    }
                }
                KeyCode::Enter if rows.is_empty() => {}
                KeyCode::Enter => {
                    let alias = picked(app, sel);
                    match (&target, alias) {
                        (PickTarget::NewSessions, Some(alias)) => {
                            app.settings.default_model = alias;
                            app.save_settings();
                        }
                        (PickTarget::Agent(a), Some(alias)) => {
                            let a = *a;
                            if scoped && role_scope {
                                let role = app.run.as_ref().and_then(|r| r.role_name_of(a)).unwrap_or_default();
                                if let Err(e) = app.set_run_role_model(&role, &alias) {
                                    app.toast(e, crate::agent::Level::Warn);
                                }
                            } else if !app.agent_alive(a) {
                                let name = app.agents.get(&a).map(|x| x.name.clone()).unwrap_or_default();
                                match app.with_run(|r, _| r.pin_next_model(a, &alias)) {
                                    Some(Ok(())) => app.toast(format!("{name}: its next start uses {alias}"), crate::agent::Level::Ok),
                                    Some(Err(e)) => app.toast(e, crate::agent::Level::Warn),
                                    None => app.toast(format!("{name} is not running"), crate::agent::Level::Warn),
                                }
                            } else {
                                match app.set_model(a, &alias) {
                                    Ok(()) => app.model_switched_for_run_agent(a),
                                    Err(e) => app.toast(e, crate::agent::Level::Warn),
                                }
                            }
                        }
                        (PickTarget::RunRole(role), Some(alias)) => {
                            if let Err(e) = app.set_run_role_model(role, &alias) {
                                app.toast(e, crate::agent::Level::Warn);
                            }
                        }
                        (PickTarget::StudioField { role, field }, alias) => {
                            // the none row clears a fallback; a model goes in through the Studio's own setters
                            if field == "model" {
                                if let Some(alias) = alias {
                                    super::studio::set_choice(app, role, "model", &alias);
                                }
                            } else if let Some(r) = app.studio.pattern.roles.get_mut(role) {
                                let i = match field.as_str() {
                                    "fallback" => 0,
                                    "fallback2" => 1,
                                    _ => 2,
                                };
                                r.set_fallback(i, alias);
                                app.studio.dirty = true;
                            }
                            if super::studio::save_pattern(app) {
                                app.sync_run_pattern("the Studio");
                            }
                        }
                        _ => {}
                    }
                    stay = false;
                }
                KeyCode::Char('e') if ctrl => {
                    app.enter_screen(Screen::Models);
                    app.models_ui.providers = false;
                    app.models_ui.row = match rows.get(sel) {
                        Some(PickRow::Model(i)) => *i,
                        _ => 0,
                    };
                    stay = false;
                }
                KeyCode::Char(c @ ('+' | '-')) => {
                    const STEPS: &[u64] = &[16_000, 32_000, 64_000, 128_000, 200_000, 262_000, 272_000, 400_000, 524_000, 1_000_000];
                    let d: i64 = if c == '+' { 1 } else { -1 };
                    if let Some(PickRow::Model(mi)) = rows.get(sel) {
                        if let Some(m) = app.registry.models.get_mut(*mi) {
                            let cur = m.context_window.unwrap_or(0);
                            let i = STEPS.iter().position(|x| *x >= cur).unwrap_or(STEPS.len() - 1) as i64;
                            let j = (i + d).clamp(0, STEPS.len() as i64 - 1) as usize;
                            m.context_window = Some(STEPS[j]);
                            match app.save_registry() {
                                Ok(()) => app.toast("context window changed — applies to new agents", crate::agent::Level::Info),
                                Err(e) => app.toast(e, crate::agent::Level::Error),
                            }
                        }
                    }
                }
                KeyCode::Backspace => {
                    filter.pop();
                    sel = 0;
                }
                KeyCode::Char(c) if !ctrl && !k.modifiers.contains(KeyModifiers::ALT) && !c.is_control() => {
                    filter.push(c);
                    sel = 0;
                }
                _ => {}
            }
            // the selection stays on a row after the list shrank
            sel = sel.min(app.picker_rows(&target, &filter).len().saturating_sub(1));
            stay.then_some(Overlay::ModelPicker { sel, target, role_scope, filter })
        }
        Overlay::Diff { agent, mut file, mut scroll } => {
            let nf = app.agents.get(&agent).map(|a| a.files.len()).unwrap_or(0).max(1);
            match k.code {
                KeyCode::Esc | KeyCode::Char('q') => None,
                KeyCode::Up | KeyCode::Char('k') => {
                    scroll = scroll.saturating_sub(1);
                    Some(Overlay::Diff { agent, file, scroll })
                }
                KeyCode::Down | KeyCode::Char('j') => Some(Overlay::Diff { agent, file, scroll: scroll + 1 }),
                KeyCode::PageUp => Some(Overlay::Diff { agent, file, scroll: scroll.saturating_sub(20) }),
                KeyCode::PageDown | KeyCode::Char(' ') => Some(Overlay::Diff { agent, file, scroll: scroll + 20 }),
                KeyCode::Tab | KeyCode::Right | KeyCode::Char('l') => {
                    file = (file + 1) % nf;
                    Some(Overlay::Diff { agent, file, scroll: 0 })
                }
                KeyCode::BackTab | KeyCode::Left | KeyCode::Char('h') => {
                    file = (file + nf - 1) % nf;
                    Some(Overlay::Diff { agent, file, scroll: 0 })
                }
                _ => Some(Overlay::Diff { agent, file, scroll }),
            }
        }
        Overlay::Inbox { mut sel } => {
            let n = app.approvals.len();
            match k.code {
                KeyCode::Esc => None,
                KeyCode::Up => {
                    sel = sel.saturating_sub(1);
                    Some(Overlay::Inbox { sel })
                }
                KeyCode::Down => Some(Overlay::Inbox { sel: (sel + 1).min(n.saturating_sub(1)) }),
                KeyCode::Char(c @ ('y' | 'a' | 'n')) if sel < n => {
                    if app.approvals[sel].method == "item/tool/requestUserInput" {
                        app.toast("answer questions from the agent's view (⏎)", crate::agent::Level::Info);
                    } else {
                        app.resolve_approval(sel, match c {
                            'y' => 0,
                            'a' => 1,
                            _ => 2,
                        }, None);
                    }
                    Some(Overlay::Inbox { sel: sel.min(app.approvals.len().saturating_sub(1)) })
                }
                KeyCode::Enter if sel < n => {
                    let a = app.approvals[sel].agent;
                    app.set_screen(if Some(a) == app.solo { Screen::Solo } else { Screen::Zoom(a) });
                    None
                }
                _ => Some(Overlay::Inbox { sel }),
            }
        }
        Overlay::Akasha { scroll, group } => match k.code {
            KeyCode::Esc | KeyCode::Char('q') => None,
            KeyCode::Up | KeyCode::Char('k') => Some(Overlay::Akasha { scroll: scroll.saturating_sub(1), group }),
            KeyCode::Down | KeyCode::Char('j') => Some(Overlay::Akasha { scroll: scroll + 1, group }),
            KeyCode::PageUp => Some(Overlay::Akasha { scroll: scroll.saturating_sub(20), group }),
            KeyCode::PageDown | KeyCode::Char(' ') => Some(Overlay::Akasha { scroll: scroll + 20, group }),
            _ => Some(Overlay::Akasha { scroll, group }),
        },
        Overlay::Plan { scroll } => match k.code {
            KeyCode::Esc | KeyCode::Char('q') => None,
            KeyCode::Up | KeyCode::Char('k') => Some(Overlay::Plan { scroll: scroll.saturating_sub(1) }),
            KeyCode::Down | KeyCode::Char('j') => Some(Overlay::Plan { scroll: scroll + 1 }),
            KeyCode::PageUp => Some(Overlay::Plan { scroll: scroll.saturating_sub(20) }),
            KeyCode::PageDown | KeyCode::Char(' ') => Some(Overlay::Plan { scroll: scroll + 20 }),
            KeyCode::Char('a') => {
                if app.run.as_ref().map(|r| r.stage == Stage::Review).unwrap_or(false) {
                    let mut run = app.run.take();
                    if let Some(r) = run.as_mut() {
                        let mut ctx = crate::app::Ctxt { hub: &mut app.hub, agents: &mut app.agents, registry: &app.registry, settings: &app.settings, tx: &app.tx, notes: &mut app.notes };
                        r.approve_plan(&mut ctx);
                    }
                    app.run = run;
                    app.land_on_overview();
                    None
                } else {
                    Some(Overlay::Plan { scroll })
                }
            }
            // Plain 'f' focuses the canvas; ctrl+f is force-send (WP9) and must reach the chat
            // input instead — it's handled globally in App::on_key, before overlays are dispatched.
            KeyCode::Char('f') if !k.modifiers.contains(KeyModifiers::CONTROL) => {
                app.canvas_focus = false;
                app.set_screen(Screen::Stage);
                None
            }
            _ => Some(Overlay::Plan { scroll }),
        },
        Overlay::Halt { scroll } => match k.code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('e') => None,
            KeyCode::Up | KeyCode::Char('k') => Some(Overlay::Halt { scroll: scroll.saturating_sub(1) }),
            KeyCode::Down | KeyCode::Char('j') => Some(Overlay::Halt { scroll: scroll + 1 }),
            KeyCode::PageUp => Some(Overlay::Halt { scroll: scroll.saturating_sub(10) }),
            KeyCode::PageDown => Some(Overlay::Halt { scroll: scroll + 10 }),
            KeyCode::Char(' ') => {
                if app.run.as_ref().map(|r| r.halted()).unwrap_or(false) {
                    app.with_run(|r, c| r.toggle_pause(c));
                }
                None
            }
            // the model picker for the agent the halt is about (the fix for a rejected model)
            KeyCode::Char('m') => {
                let a = app.run.as_ref().and_then(|r| r.halt.as_ref()).and_then(|h| h.agent);
                match a {
                    Some(a) => {
                        app.open_model_picker(PickTarget::Agent(a));
                        app.overlays.pop()
                    }
                    None => {
                        app.toast("this halt is not about one agent — select one on the stage and press m", crate::agent::Level::Info);
                        Some(Overlay::Halt { scroll })
                    }
                }
            }
            _ => Some(Overlay::Halt { scroll }),
        },
        Overlay::NewRun { mut sel, list, mut goal } => {
            let n = list.len();
            let broken = list.get(sel).filter(|p| p.about.is_err()).map(|p| p.name.clone());
            let name = list.get(sel).map(|p| p.name.clone()).unwrap_or_default();
            // e edits the pattern in the Studio — ctrl+e while there is a goal to type an e into
            let edit = k.code == KeyCode::Char('e') && (goal.is_none() || ctrl);
            match k.code {
                KeyCode::Esc => None,
                KeyCode::Up => {
                    sel = sel.saturating_sub(1);
                    Some(Overlay::NewRun { sel, list, goal })
                }
                KeyCode::Down => {
                    sel = (sel + 1).min(n.saturating_sub(1));
                    Some(Overlay::NewRun { sel, list, goal })
                }
                // a pattern whose file does not load would only fail the run (and the Studio
                // cannot open it): say so, stay here
                KeyCode::Enter | KeyCode::Char('e') if (k.code == KeyCode::Enter || edit) && broken.is_some() => {
                    app.toast(format!("{} does not load — fix its .toml file first", broken.unwrap_or_default()), crate::agent::Level::Warn);
                    Some(Overlay::NewRun { sel, list, goal })
                }
                KeyCode::Char('e') if edit => {
                    app.pattern_name = name;
                    app.studio.dirty = false;
                    app.open_studio();
                    None
                }
                // with a goal being typed, delete is the goal's (forward delete)
                KeyCode::Delete if !goal.as_ref().is_some_and(|g| !g.text().is_empty()) => {
                    let list = delete_pattern(app, list, sel);
                    sel = sel.min(list.len().saturating_sub(1));
                    Some(Overlay::NewRun { sel, list, goal })
                }
                KeyCode::Enter if !k.modifiers.intersects(KeyModifiers::ALT | KeyModifiers::SHIFT) => match goal {
                    None => {
                        app.choose_pattern(&name);
                        None
                    }
                    Some(g) if g.text().trim().is_empty() => {
                        app.toast("describe what to build first", crate::agent::Level::Info);
                        Some(Overlay::NewRun { sel, list, goal: Some(g) })
                    }
                    Some(g) => {
                        app.choose_pattern(&name);
                        app.start_run(g.text().trim());
                        None
                    }
                },
                _ => {
                    if let Some(g) = goal.as_mut() {
                        g.key(k);
                    }
                    Some(Overlay::NewRun { sel, list, goal })
                }
            }
        }
        Overlay::Select(mut s) => {
            let n = s.items.len();
            match k.code {
                KeyCode::Esc => None,
                KeyCode::Up => {
                    s.sel = s.sel.saturating_sub(1);
                    Some(Overlay::Select(s))
                }
                KeyCode::Down => {
                    s.sel = (s.sel + 1).min(n.saturating_sub(1));
                    Some(Overlay::Select(s))
                }
                KeyCode::Char(' ') if s.multi => {
                    if let Some(c) = s.items.get_mut(s.sel) {
                        c.on = !c.on;
                    }
                    Some(Overlay::Select(s))
                }
                KeyCode::Enter if n > 0 => {
                    chosen(app, &s);
                    None
                }
                // a letter: the next row whose label starts with it
                KeyCode::Char(c) if !ctrl && c.is_alphanumeric() => {
                    let c = c.to_ascii_lowercase();
                    if let Some(i) = (1..=n).map(|d| (s.sel + d) % n).find(|i| s.items[*i].label.to_lowercase().starts_with(c)) {
                        s.sel = i;
                    }
                    Some(Overlay::Select(s))
                }
                _ => Some(Overlay::Select(s)),
            }
        }
        Overlay::Discover(mut st) => {
            use crate::discover::CState;
            let vis = st.visible();
            match k.code {
                KeyCode::Esc if !st.filter.is_empty() => {
                    st.filter.clear();
                    st.sel = 0;
                    Some(Overlay::Discover(st))
                }
                KeyCode::Esc => None,
                KeyCode::Up => {
                    st.sel = st.sel.saturating_sub(1);
                    Some(Overlay::Discover(st))
                }
                KeyCode::Down => {
                    st.sel = (st.sel + 1).min(vis.len().saturating_sub(1));
                    Some(Overlay::Discover(st))
                }
                KeyCode::PageUp => {
                    st.sel = st.sel.saturating_sub(10);
                    Some(Overlay::Discover(st))
                }
                KeyCode::PageDown => {
                    st.sel = (st.sel + 10).min(vis.len().saturating_sub(1));
                    Some(Overlay::Discover(st))
                }
                KeyCode::Char(' ') => {
                    if let Some(&i) = vis.get(st.sel) {
                        if st.items[i].state != CState::Configured {
                            st.items[i].selected = !st.items[i].selected;
                        }
                    }
                    Some(Overlay::Discover(st))
                }
                KeyCode::Tab => {
                    let any_off = vis.iter().any(|&i| st.items[i].state != CState::Configured && !st.items[i].selected);
                    for &i in &vis {
                        if st.items[i].state != CState::Configured {
                            st.items[i].selected = any_off;
                        }
                    }
                    Some(Overlay::Discover(st))
                }
                KeyCode::Enter => {
                    if st.items.iter().any(|c| c.selected) {
                        app.apply_discover(&st);
                        None
                    } else {
                        app.toast(if st.loading.is_empty() { "select models with space (tab = all)" } else { "still asking providers…" }, crate::agent::Level::Info);
                        Some(Overlay::Discover(st))
                    }
                }
                KeyCode::Char(_) | KeyCode::Backspace if !ctrl => {
                    st.filter.key(k);
                    st.sel = 0;
                    Some(Overlay::Discover(st))
                }
                _ => Some(Overlay::Discover(st)),
            }
        }
        Overlay::Runs { mut sel, mut list, confirm, others } => {
            let n = list.len();
            if confirm {
                match k.code {
                    KeyCode::Char('y') | KeyCode::Char('Y') => {
                        if let Some(r) = list.get(sel).cloned() {
                            if app.delete_run(&r) {
                                list.retain(|x| x.id != r.id);
                            }
                        }
                        sel = sel.min(list.len().saturating_sub(1));
                        Some(Overlay::Runs { sel, list, confirm: false, others })
                    }
                    KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('N') => Some(Overlay::Runs { sel, list, confirm: false, others }),
                    _ => Some(Overlay::Runs { sel, list, confirm, others }),
                }
            } else {
                match k.code {
                    KeyCode::Esc => None,
                    KeyCode::Up => {
                        sel = sel.saturating_sub(1);
                        Some(Overlay::Runs { sel, list, confirm, others })
                    }
                    KeyCode::Down => {
                        sel = (sel + 1).min(n.saturating_sub(1));
                        Some(Overlay::Runs { sel, list, confirm, others })
                    }
                    KeyCode::Enter => {
                        if let Some(r) = list.get(sel).cloned() {
                            app.resume_run(r);
                        }
                        None
                    }
                    KeyCode::Char('D') | KeyCode::Char('d') | KeyCode::Delete => {
                        if n > 0 {
                            Some(Overlay::Runs { sel, list, confirm: true, others })
                        } else {
                            Some(Overlay::Runs { sel, list, confirm, others })
                        }
                    }
                    _ => Some(Overlay::Runs { sel, list, confirm, others }),
                }
            }
        }
    };
    if let Some(o) = keep {
        app.overlays.push(o);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn text_of(lines: &[Line]) -> Vec<String> {
        lines.iter().map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect()).collect()
    }

    fn toast(app: &App) -> String {
        app.toast.as_ref().map(|t| t.0.clone()).unwrap_or_default()
    }

    fn press(app: &mut App, c: KeyCode) {
        key(app, KeyEvent::new(c, KeyModifiers::NONE));
    }

    fn screen_of(app: &mut App, w: u16, h: u16) -> String {
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| super::super::draw(f, app)).unwrap();
        let buf = term.backend().buffer();
        (0..buf.area.height).map(|y| (0..buf.area.width).map(|x| buf[(x, y)].symbol().to_string()).collect::<String>() + "\n").collect()
    }

    /// The overlay is the block as the agents read it: the header, then each note on its own line
    /// with its head, an urgent one marked, a note over several lines kept together under its text.
    #[test]
    fn a_rendered_block_reads_note_by_note_with_the_urgent_ones_marked() {
        let rendered = "## akasha · builders · 3 notes · 312/16k\n[1 · w2 · 14:02] models live in src/models — reuse `Config` from p1-config\n![2 · w3 · 14:05] the API returns 201 on create now\n[3 · mantra · 14:06] trait Store {\n    fn get(&self) -> u32;\n    }";
        let lines = akasha_lines(rendered, 100);
        let text = text_of(&lines);
        assert_eq!(text[0], "akasha · builders · 3 notes · 312/16k");
        assert_eq!(text[1], "  [1 · w2 · 14:02] models live in src/models — reuse `Config` from p1-config");
        assert_eq!(text[2], "! [2 · w3 · 14:05] the API returns 201 on create now");
        assert_eq!(lines[2].spans[0].style, theme::bold(theme::fg(theme::AMBER)), "urgent is marked");
        assert_eq!(lines[1].spans[0].content, "  ", "the others are not");
        assert_eq!(text[4].trim(), "fn get(&self) -> u32;", "a note's later lines stay with it");
        // a long note wraps under its text, never past the width
        let long = format!("[4 · w5 · 14:10] {}", "word ".repeat(40));
        let lines = akasha_lines(&long, 40);
        assert!(lines.len() > 3 && lines.iter().all(|l| l.width() <= 40), "{:?}", text_of(&lines));
        assert!(text_of(&lines)[1].starts_with("    word"), "{:?}", text_of(&lines));
        // an empty block, as the engine renders it: its one line is muted, not a note's next line
        let empty = akasha_lines(&crate::engine::akasha::Block::new("builders", 16_384).render(), 100);
        assert_eq!(text_of(&empty), vec!["akasha · builders · 0 notes · 54/16k".to_string(), "  (no notes yet)".to_string()]);
        assert_eq!(empty[1].spans[1].style, theme::muted());
    }

    /// `/akasha` with nothing to show is a toast, not an empty box; an unknown group names the
    /// groups there are; one group opens straight away, several ask which (a selector), and the
    /// overlay scrolls and closes on esc.
    #[test]
    fn akasha_opens_the_one_group_or_asks_which() {
        let mut app = crate::web::snapshot::tests::test_app();
        app.command("/akasha");
        assert!(app.overlays.is_empty());
        assert_eq!(toast(&app), "no akasha groups in this run", "no run");
        let mut pattern = crate::engine::pattern::Pattern::builtin();
        pattern.groups.clear();
        app.run = Some(crate::engine::run::Run::new(std::path::PathBuf::from("."), pattern, "goal".into()));
        app.toast = None;
        app.command("/akasha");
        assert!(app.overlays.is_empty());
        assert_eq!(toast(&app), "no akasha groups in this run", "a run without groups");
        if let Some(r) = app.run.as_mut() {
            r.akasha.insert("builders".into(), crate::engine::akasha::Block::new("builders", 16_384));
        }
        app.command("/akasha testers");
        assert!(app.overlays.is_empty());
        assert_eq!(toast(&app), "no akasha group 'testers' in this run (it has: builders)");
        app.command("/akasha");
        assert!(matches!(app.overlays.pop(), Some(Overlay::Akasha { group: Some(g), .. }) if g == "builders"), "the only group, straight in");
        if let Some(r) = app.run.as_mut() {
            r.akasha.insert("reviewers".into(), crate::engine::akasha::Block::new("reviewers", 4_096));
        }
        app.command("/akasha");
        let Some(Overlay::Select(s)) = app.overlays.last() else { panic!("a selector of the groups") };
        assert_eq!(s.items.iter().map(|c| c.key.as_str()).collect::<Vec<_>>(), vec!["builders", "reviewers"]);
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Enter);
        assert!(matches!(app.overlays.last(), Some(Overlay::Akasha { scroll: 0, group: Some(name) }) if name == "reviewers"), "the one chosen");
        press(&mut app, KeyCode::Down);
        assert!(matches!(app.overlays.last(), Some(Overlay::Akasha { scroll: 1, .. })));
        press(&mut app, KeyCode::Esc);
        assert!(app.overlays.is_empty());
        app.command("/akasha builders");
        assert!(matches!(app.overlays.last(), Some(Overlay::Akasha { group: Some(name), .. }) if name == "builders"), "the one named");
    }

    /// Left open after the run is gone, the overlay says there is nothing rather than drawing an
    /// empty box, and its scroll is clamped to what there is.
    #[test]
    fn the_overlay_without_blocks_says_so() {
        let mut app = crate::web::snapshot::tests::test_app();
        app.overlays.push(Overlay::Akasha { scroll: 7, group: None });
        let screen = screen_of(&mut app, 100, 30);
        assert!(screen.contains("no akasha groups in this run") && screen.contains("akasha"), "{screen}");
        assert!(matches!(app.overlays.last(), Some(Overlay::Akasha { scroll: 0, .. })), "clamped");
    }

    #[test]
    fn scoped_swarm_notes_open_without_a_run_and_show_member_status() {
        let mut app = crate::web::snapshot::tests::test_app();
        let owner = 101;
        let child = 102;
        let mut parent = crate::agent::Agent::new(owner, "solo", "solo", std::path::PathBuf::from("."));
        parent.status = Status::Idle;
        let mut member = crate::agent::Agent::new(child, "scout", "swarm", std::path::PathBuf::from("."));
        member.status = Status::Busy;
        member.turn_active = true;
        member.activity = "reading config".into();
        app.agents.insert(owner, parent);
        app.agents.insert(child, member);
        let mut notes = crate::engine::akasha::Block::new("research", 8_192);
        notes.note("scout", "Reuse the existing config parser", false, 0).unwrap();
        app.swarms.insert(owner, crate::app::swarm::SwarmGroup { owner, name: "research".into(), notes, children: vec![child] });

        app.input.set("/akasha sw");
        assert_eq!(app.suggestions().iter().map(|s| s.text.as_str()).collect::<Vec<_>>(), vec!["/akasha swarm:101"]);
        app.input.clear();
        app.command("/akasha");
        assert!(matches!(app.overlays.last(), Some(Overlay::Akasha { group: Some(name), .. }) if name == "swarm:101"));
        let screen = screen_of(&mut app, 100, 30);
        assert!(screen.contains("owner solo · idle"), "{screen}");
        assert!(screen.contains("scout · reading config"), "{screen}");
        assert!(screen.contains("Reuse the existing config parser"), "{screen}");
        assert!(swarm_member_line(&app, child, "member", 20).width() <= 20);

        app.overlays.clear();
        let mut run = crate::engine::run::Run::new(std::path::PathBuf::from("."), crate::engine::pattern::Pattern::builtin(), "goal".into());
        run.akasha.insert("research".into(), crate::engine::akasha::Block::new("research", 8_192));
        app.run = Some(run);
        app.command("/akasha");
        let Some(Overlay::Select(s)) = app.overlays.last() else { panic!("run and swarm groups need a selector") };
        assert_eq!(s.items.iter().map(|c| c.key.as_str()).collect::<Vec<_>>(), vec!["builders", "research", "swarm:101"]);
        app.run.as_mut().unwrap().akasha.insert("swarm:101".into(), crate::engine::akasha::Block::new("swarm:101", 8_192));
        let keys: Vec<_> = app.akasha_choices().into_iter().map(|c| c.key).collect();
        assert!(keys.contains(&"run:swarm:101".to_string()) && keys.contains(&"swarm:101".to_string()));
    }

    /// `/run` without a goal opens the new-run overlay: the patterns, the one for new runs
    /// selected and marked, a goal typed under them; ⏎ on an empty goal says what is missing, on a
    /// goal starts the run on the selected pattern. `/pattern` is the same list without a goal.
    #[tokio::test]
    async fn run_without_a_goal_picks_a_pattern_and_asks_for_the_goal() {
        crate::web::snapshot::tests::with_mantra_home(|| {
            let project = crate::config::home().join("newrun-project");
            std::fs::create_dir_all(&project).unwrap();
            let mut app = crate::web::snapshot::tests::test_app();
            app.project = project;
            app.command("/run");
            let Some(Overlay::NewRun { sel: 0, list, goal: Some(_) }) = app.overlays.last() else { panic!("the new-run overlay") };
            assert_eq!(list.iter().take(crate::engine::pattern::Pattern::names().len()).map(|p| p.name.as_str()).collect::<Vec<_>>(), crate::engine::pattern::Pattern::names());
            let s = screen_of(&mut app, 120, 30);
            assert!(s.contains("mantra-default") && s.contains("default") && s.contains("for new runs") && s.contains("goal ›"), "{s}");
            press(&mut app, KeyCode::Enter);
            assert_eq!(toast(&app), "describe what to build first");
            press(&mut app, KeyCode::Down);
            for c in "e2e: build it".chars() {
                press(&mut app, KeyCode::Char(c));
            }
            assert!(matches!(app.overlays.last(), Some(Overlay::NewRun { goal: Some(g), .. }) if g.text() == "e2e: build it"), "an e is typed into the goal, not an edit");
            assert!(!screen_of(&mut app, 120, 30).contains("del delete"), "with a goal typed, del is the goal's");
            press(&mut app, KeyCode::Delete);
            assert!(app.confirm.is_none() && !toast(&app).contains("built in"), "no pattern delete while typing");
            press(&mut app, KeyCode::Enter);
            assert!(app.overlays.is_empty());
            let run = app.run.as_ref().expect("started");
            assert_eq!((run.pattern.name.as_str(), run.brief.as_str()), (crate::engine::pattern::NAME_LEAN, "e2e: build it"));
            assert_eq!(app.pattern_name, crate::engine::pattern::NAME_LEAN, "and it is the pattern for new runs");
            app.run = None;
            app.command("/pattern");
            assert!(matches!(app.overlays.last(), Some(Overlay::NewRun { sel: 1, goal: None, .. })), "the one for new runs selected");
            press(&mut app, KeyCode::Up);
            press(&mut app, KeyCode::Enter);
            assert_eq!(app.pattern_name, crate::engine::pattern::NAME_DEFAULT);
            assert!(app.run.is_none(), "choosing starts nothing");
        });
    }

    /// Delete on the pattern list removes a file of yours on the second press (new runs go back
    /// to the default); a built-in has no file and says so.
    #[test]
    fn delete_twice_on_the_pattern_list_removes_a_pattern_of_yours() {
        crate::web::snapshot::tests::with_mantra_home(|| {
            let project = crate::config::home().join("delete-project");
            std::fs::create_dir_all(&project).unwrap();
            crate::engine::pattern::Pattern::builtin().save_as("mine", &project).unwrap();
            let mut app = crate::web::snapshot::tests::test_app();
            app.project = project;
            app.pattern_name = "mine".into();
            app.command("/pattern");
            let Some(Overlay::NewRun { sel, list, goal: None }) = app.overlays.last() else { panic!("the pattern list") };
            assert_eq!(list[*sel].name, "mine", "the one for new runs selected");
            press(&mut app, KeyCode::Delete);
            assert!(matches!(app.overlays.last(), Some(Overlay::NewRun { list, .. }) if list.iter().any(|p| p.name == "mine")), "armed only");
            assert!(screen_of(&mut app, 120, 30).contains("del again to delete mine"));
            press(&mut app, KeyCode::Delete);
            assert!(toast(&app).starts_with("deleted ") && toast(&app).contains("mine.toml"), "{}", toast(&app));
            let Some(Overlay::NewRun { list, .. }) = app.overlays.last() else { panic!("still open") };
            assert!(list.iter().all(|p| p.name != "mine"), "gone from the list");
            assert!(!crate::config::patterns_dir().join("mine.toml").exists());
            assert_eq!(app.pattern_name, crate::engine::pattern::NAME_DEFAULT, "new runs leave a pattern that is gone");
            for _ in 0..5 {
                press(&mut app, KeyCode::Up);
            }
            press(&mut app, KeyCode::Delete);
            assert!(toast(&app).contains("is built in"), "{}", toast(&app));
        });
    }

    /// `/respawn` with no agent named and none of the run's in view asks which (a selector of the
    /// stage's agents); with one named it respawns that one; with no run it says so.
    #[tokio::test]
    async fn respawn_without_a_target_asks_which_agent() {
        crate::web::snapshot::tests::with_mantra_home(|| {
            let mut app = crate::web::snapshot::tests::test_app();
            app.command("/respawn");
            assert_eq!(toast(&app), "no run agent to respawn");
            let mut run = crate::engine::run::Run::new(std::path::PathBuf::from("."), crate::engine::pattern::Pattern::builtin(), "goal".into());
            run.planner = Some(41);
            run.orchestrator = Some(42);
            app.agents.insert(41, crate::agent::Agent::new(41, "planner", "planner", std::path::PathBuf::from(".")));
            app.agents.insert(42, crate::agent::Agent::new(42, "orchestrator", "orchestrator", std::path::PathBuf::from(".")));
            app.run = Some(run);
            app.command("/respawn");
            let Some(Overlay::Select(s)) = app.overlays.last() else { panic!("an agent selector") };
            assert_eq!(s.items.iter().map(|c| c.label.as_str()).collect::<Vec<_>>(), vec!["planner", "orchestrator"]);
            assert_eq!(s.then, SelectThen::Respawn);
            app.overlays.clear();
            app.command("/respawn nobody");
            assert_eq!(toast(&app), "no agent named nobody in this run");
        });
    }

    /// Tab completes a command, then its argument: the run's Akasha groups, its agents; `@` on the
    /// stage completes an agent's name.
    #[test]
    fn tab_completes_commands_and_their_arguments() {
        let mut app = crate::web::snapshot::tests::test_app();
        app.input.set("/se");
        assert_eq!(app.suggestions().iter().map(|s| s.label.as_str()).collect::<Vec<_>>(), vec!["/settings"]);
        let mut run = crate::engine::run::Run::new(std::path::PathBuf::from("."), crate::engine::pattern::Pattern::builtin(), "goal".into());
        run.akasha.insert("builders".into(), crate::engine::akasha::Block::new("builders", 16_384));
        run.planner = Some(41);
        app.agents.insert(41, crate::agent::Agent::new(41, "planner", "planner", std::path::PathBuf::from(".")));
        app.run = Some(run);
        app.input.set("/akasha b");
        assert_eq!(app.suggestions().iter().map(|s| s.text.as_str()).collect::<Vec<_>>(), vec!["/akasha builders"]);
        app.input.set("/respawn pl");
        assert_eq!(app.suggestions().iter().map(|s| s.text.as_str()).collect::<Vec<_>>(), vec!["/respawn planner"]);
        app.input.set("/respawn planner");
        assert!(app.suggestions().is_empty(), "typed out: nothing more to offer");
        app.input.set("@pl");
        assert!(app.suggestions().is_empty(), "@names belong to the stage");
        app.screen = Screen::Stage;
        assert_eq!(app.suggestions().iter().map(|s| s.text.as_str()).collect::<Vec<_>>(), vec!["@planner "]);
    }

    /// The model picker's alias column fits an 11-character alias (the `-codex` ones) and keeps a
    /// space before the model id.
    #[test]
    fn the_picker_fits_an_eleven_character_alias() {
        let mut app = crate::web::snapshot::tests::test_app();
        // Codex models are explicit user entries; the backend helper no longer invents a
        // catalogue on a fresh install.
        app.registry.add_codex_model("astra-codex", "gpt-custom").unwrap();
        app.overlays.push(Overlay::ModelPicker { sel: 0, target: PickTarget::NewSessions, role_scope: false, filter: String::new() });
        let s = screen_of(&mut app, 140, 30);
        let row = s.lines().find(|l| l.contains("astra-codex")).expect("its row");
        let after = &row[row.find("astra-codex").unwrap() + "astra-codex".len()..];
        assert!(after.starts_with(' ') && !after.starts_with("…"), "{row}");
    }
}
