//! Solo mode (one agent, Claude-Code-like) — also used to zoom into any Mandala agent.

use super::{theme, *};
use crate::app::App;
use crate::engine::run::WState;
use crate::hub::AgentId;
use ratatui::layout::{Constraint, Direction, Layout};

pub fn draw(f: &mut Frame, app: &mut App, zoom: Option<AgentId>) {
    let area = f.area();
    let id = zoom.or(app.solo);
    let approval = app.approval_for(id);
    let ap_h = approval.map(|i| approval_height(&app.approvals[i])).unwrap_or(0);
    let chip_h = super::queue_chip_height(app, id);
    let in_h = input_height(app, area.width).min(area.height.saturating_sub(8).max(3));
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(3), Constraint::Length(ap_h), Constraint::Length(chip_h), Constraint::Length(in_h), Constraint::Length(1)])
        .split(area);

    // header — zoomed gets a solid role-coloured band (unmistakably "inside one agent"); Solo
    // keeps its plain brand+breadcrumb header.
    match (zoom, id.and_then(|a| app.agents.get(&a))) {
        (Some(_), Some(a)) => {
            zoomed_header(f, rows[0], a);
            super::draw_screen_flash(f, rows[0], app, theme::named(&a.color));
        }
        (Some(_), None) => header(f, rows[0], vec![Span::styled(format!(" {} mandala {} zoomed", theme::g("›", ">"), theme::g("›", ">")), theme::faint())], vec![]),
        (None, Some(a)) => {
            let mut c = vec![Span::styled("  solo  ", theme::muted())];
            c.push(Span::styled(home_rel(&app.project), theme::dim()));
            if !app.branch.is_empty() {
                c.push(Span::styled(format!("  {} {}", theme::g("⎇", "@"), app.branch), theme::faint()));
            }
            if app.demo {
                c.push(Span::styled("  DEMO", theme::bold(theme::fg(theme::ROSE))));
            }
            let mut r = super::web_badges(app);
            if let Some(b) = app.inbox_badge().filter(|_| app.approval_for(id).is_none()) {
                r.push(Span::styled(b, theme::bold(theme::fg(theme::AMBER))));
            }
            r.extend(model_chip(a, app));
            header(f, rows[0], c, r);
        }
        (None, None) => header(f, rows[0], vec![Span::styled("  solo", theme::muted())], vec![]),
    }

    // body: spine (zoomed only) | log | side panel
    let spine_w: u16 = if zoom.is_some() { 1 } else { 0 };
    let has_activity = id.and_then(|a| app.agents.get(&a)).map(|a| !a.items.is_empty()).unwrap_or(false);
    let show_side = app.side_panel && rows[1].width >= 100 && (has_activity || zoom.is_some());
    let body = if show_side {
        Layout::default().direction(Direction::Horizontal).constraints([Constraint::Length(spine_w), Constraint::Min(40), Constraint::Length(36)]).split(rows[1])
    } else {
        Layout::default().direction(Direction::Horizontal).constraints([Constraint::Length(spine_w), Constraint::Min(40)]).split(rows[1])
    };
    if let Some(a) = id.and_then(|a| app.agents.get(&a)) {
        if zoom.is_some() && body[0].width > 0 {
            let col = theme::c(theme::named(&a.color));
            let buf = f.buffer_mut();
            for y in body[0].y..body[0].y + body[0].height {
                if let Some(cell) = buf.cell_mut((body[0].x, y)) {
                    cell.set_symbol(theme::g("▎", "|"));
                    cell.set_fg(col);
                }
            }
        }
    }
    let verbose = app.verbose;
    let empty = id.and_then(|a| app.agents.get(&a)).map(|a| a.items.is_empty()).unwrap_or(true);
    if empty && zoom.is_none() {
        welcome(f, body[1], app);
    } else if let Some(a) = id.and_then(|a| app.agents.get_mut(&a)) {
        let footer: Vec<Line<'static>> = busy_line(a).into_iter().collect();
        convo::draw(f, body[1], a, verbose, footer);
    }
    if show_side {
        side_panel(f, body[2], app, id);
    }
    if let Some(i) = approval {
        let name = app.agents.get(&app.approvals[i].agent).map(|a| a.name.clone()).unwrap_or_default();
        draw_approval(f, rows[2], &app.approvals[i], &name);
    }
    super::draw_queue_chip(f, rows[3], app, id);

    // input
    let busy = id.and_then(|a| app.agents.get(&a)).map(|a| a.busy()).unwrap_or(false);
    let placeholder = match (zoom, id.and_then(|a| app.agents.get(&a))) {
        (Some(_), Some(a)) if busy => format!("message {} {} …  (⏎ queue · ctrl+f send now · esc overview)", theme::role_glyph(&a.glyph), a.role),
        (Some(_), _) => "message this agent…  (esc back to overview)".to_string(),
        (None, _) if busy => "type to steer the running turn…".to_string(),
        (None, _) => "ask anything · / commands · ! shell · ctrl+o mandala".to_string(),
    };
    let color = id.and_then(|a| app.agents.get(&a)).map(|a| theme::named(&a.color)).unwrap_or(theme::SAFFRON);
    let focused = approval.map(|i| app.approvals[i].method == "item/tool/requestUserInput").unwrap_or(true);
    draw_input(f, rows[4], app, &placeholder, color, focused);
    draw_suggestions(f, rows[4], app);

    // footer
    let hints: Vec<(&str, &str)> = if zoom.is_some() {
        vec![("⏎", "queue"), ("ctrl+f", "send now"), ("ctrl+c", "interrupt"), ("alt+↑↓", "effort"), ("ctrl+k", "model"), ("ctrl+d", "diff"), ("esc", "▸ overview")]
    } else {
        let mode = app.settings.approval_mode.clone();
        let m: &'static str = match mode.as_str() {
            "never" => "approvals: never ask",
            "untrusted" => "approvals: untrusted",
            _ => "approvals: on-request",
        };
        vec![("⏎", "send"), ("ctrl+c", "interrupt"), ("⇧⇥", m), ("alt+↑↓", "effort"), ("ctrl+k", "model"), ("ctrl+d", "diff"), ("ctrl+o", "mandala"), ("?", "help")]
    };
    footer(f, rows[5], &hints);
}

/// Zoomed-in header: a solid band tinted with the agent's role colour so the two views — overview
/// and zoom — are never mistaken for each other, even mid-scroll.
fn zoomed_header(f: &mut Frame, area: Rect, a: &Agent) {
    let base = theme::named(&a.color);
    let band = Style::default().bg(theme::mix(theme::FAINT, base, 0.25));
    let text_st = band.fg(theme::c(theme::TEXT)).add_modifier(Modifier::BOLD);
    let dim_st = band.fg(theme::c(theme::MUTED));
    let mut left = format!(" {} {}", theme::role_glyph(&a.glyph), a.name);
    if !a.model_alias.is_empty() {
        left.push_str(&format!(" · {}", a.model_alias));
    }
    if !a.effort.is_empty() {
        left.push_str(&format!(" · {}", a.effort));
    }
    let right = "zoomed · esc back to overview ";
    let gap = (area.width as usize).saturating_sub(w_of(&left) + w_of(right)).max(2);
    let line = Line::from(vec![Span::styled(left, text_st), Span::styled(" ".repeat(gap), dim_st), Span::styled(right, dim_st)]);
    // `.style(band)` fills the whole row's background first, so the tint stays solid even where
    // no span reaches (a narrow terminal, or the trailing cell after `right`).
    f.render_widget(Paragraph::new(line).style(band), area);
}

fn welcome(f: &mut Frame, area: Rect, app: &App) {
    let m = app.registry.resolve(&app.settings.default_model);
    let eff = m.default_effort.clone();
    let b = anim::breath(2600);
    let star = Style::default().fg(theme::mix(theme::SAFFRON, theme::TEXT, b * 0.35)).add_modifier(Modifier::BOLD);
    let key = |k: &str, d: &str| -> Line<'static> { Line::from(vec![Span::styled(format!("    {k:<18}"), theme::muted()), Span::styled(d.to_string(), theme::dim())]) };
    let mut lines = vec![
        Line::from(vec![Span::styled(format!("  {}  ", theme::g("✦", "*")), star), Span::styled("M A N T R A", theme::bold(theme::accent()))]),
        Line::from(Span::styled("     one terminal for Codex agents — solo, or as a whole team", theme::dim())),
        Line::default(),
        Line::from(vec![
            Span::styled("     model  ", theme::faint()),
            Span::styled(m.alias.clone(), theme::bold(theme::text())),
            Span::styled(format!(" ({}) ", m.model), theme::dim()),
            Span::styled(eff.clone(), theme::fg(effort_color(&eff))),
            Span::styled(format!("   approvals  {}", app.settings.approval_mode), theme::faint()),
        ]),
        Line::from(Span::styled(format!("     project {}{}", home_rel(&app.project), if app.branch.is_empty() { String::new() } else { format!("  {} {}", theme::g("⎇", "@"), app.branch) }), theme::faint())),
        Line::default(),
        key("just type", "ask or instruct the agent · type while it works to steer"),
        key("/", "commands  (/model /effort /compact /diff /run …)"),
        key("ctrl+o", "Mandala: planner → orchestrator → parallel workers → gates"),
        key("ctrl+k · alt+↑↓", "switch model · change reasoning effort"),
        key("?", "all keys"),
    ];
    if !app.unfinished_runs.is_empty() {
        let n = app.unfinished_runs.len();
        lines.push(Line::default());
        lines.push(Line::from(Span::styled(format!("     {} {n} unfinished run{} — /runs to resume or delete", theme::g("↻", "~"), if n == 1 { "" } else { "s" }), theme::fg(theme::AMBER))));
    }
    if let Some(w) = &app.sandbox_warning {
        lines.push(Line::default());
        let width = area.width.saturating_sub(18) as usize;
        lines.push(Line::from(Span::styled(format!("     {} sandbox: {}", theme::g("⚠", "!"), trunc(w, width.max(20))), theme::fg(theme::AMBER))));
        lines.push(Line::from(Span::styled("       (mantra doctor shows the full fix)", theme::faint())));
    }
    if app.demo {
        lines.push(Line::default());
        lines.push(Line::from(Span::styled("     demo mode — simulated agents, nothing is sent to an API", theme::fg(theme::ROSE))));
    }
    let h = lines.len() as u16;
    let top = area.y + area.height.saturating_sub(h) / 3;
    f.render_widget(Paragraph::new(lines), Rect { y: top, height: area.height.saturating_sub(top - area.y), ..area });
}

fn side_panel(f: &mut Frame, area: Rect, app: &App, id: Option<AgentId>) {
    let blk = Block::default().borders(Borders::LEFT).border_style(theme::faint());
    let inner = blk.inner(area);
    f.render_widget(blk, area);
    let Some(a) = id.and_then(|a| app.agents.get(&a)) else { return };
    let w = inner.width.saturating_sub(2) as usize;
    let mut l: Vec<Line> = vec![];
    let sec = |t: &str, extra: Vec<Span<'static>>| {
        let mut s = vec![Span::styled(format!(" {t}"), theme::bold(theme::muted()))];
        s.extend(extra);
        Line::from(s)
    };

    // task info when zoomed into a run worker
    if let Some(r) = &app.run {
        if let Some(wk) = r.workers.iter().find(|x| x.agent == Some(a.id)) {
            let st = match &wk.state {
                WState::Failed(m) => format!("failed: {}", trunc(m, 20)),
                WState::Retrying(at) => format!("retrying in {}s", at.saturating_duration_since(std::time::Instant::now()).as_secs()),
                WState::Queued => "queued".into(),
                WState::Preparing => "preparing workspace".into(),
                WState::Running => "running".into(),
                WState::Done => "done".into(),
                WState::Cancelled => "cancelled".into(),
            };
            l.push(sec("Task", vec![Span::styled(format!("  {}", wk.task.id), theme::dim())]));
            l.push(Line::from(Span::styled(format!(" {}", trunc(&wk.task.title, w)), theme::text())));
            l.push(Line::from(Span::styled(format!(" {st} · attempt {}", wk.attempt), theme::dim())));
            if !wk.task.scope.is_empty() {
                l.push(Line::from(Span::styled(format!(" scope {}", trunc(&wk.task.scope.join(", "), w.saturating_sub(6))), theme::faint())));
            }
            if !wk.tripwires.is_empty() {
                l.push(Line::from(Span::styled(format!(" {} out of scope: {}", theme::g("⚠", "!"), trunc(&wk.tripwires.join(", "), w.saturating_sub(16))), theme::fg(theme::AMBER))));
            }
            l.push(Line::default());
        }
    }

    // changes
    let (ta, td) = a.files.values().fold((0, 0), |(x, y), s| (x + s.adds, y + s.dels));
    l.push(sec("Changes", vec![Span::styled(format!("  +{ta}", ), theme::fg(theme::GREEN)), Span::styled(format!(" -{td}"), theme::fg(theme::RED))]));
    if a.files.is_empty() {
        l.push(Line::from(Span::styled(" no changes yet", theme::faint())));
    }
    let max_files = 10;
    for (p, s) in a.files.iter().rev().take(max_files) {
        let (k, col) = match s.kind.as_str() {
            "add" => ("A", theme::GREEN),
            "delete" => ("D", theme::RED),
            _ => ("M", theme::AMBER),
        };
        let stats = format!("+{} -{}", s.adds, s.dels);
        let pw = w.saturating_sub(stats.len() + 3);
        l.push(Line::from(vec![Span::styled(format!(" {k} "), theme::fg(col)), Span::styled(format!("{:<pw$}", trunc(p, pw)), theme::text()), Span::styled(stats, theme::dim())]));
    }
    if a.files.len() > max_files {
        l.push(Line::from(Span::styled(format!(" … {} more · ctrl+d", a.files.len() - max_files), theme::faint())));
    }
    l.push(Line::default());

    // plan
    if !a.plan.is_empty() {
        let (d, t) = a.plan_progress();
        l.push(sec("Plan", vec![Span::styled(format!("  {d}/{t}"), theme::dim())]));
        for (step, st) in &a.plan {
            let (g, col) = match st.as_str() {
                "completed" => (theme::g("✓", "x"), theme::GREEN),
                "inProgress" => (if a.busy() { anim::spinner() } else { theme::g("◐", "~") }, theme::SAFFRON),
                _ => (theme::g("○", "o"), theme::FAINT),
            };
            let tst = if st == "completed" { theme::dim().add_modifier(Modifier::CROSSED_OUT) } else if st == "inProgress" { theme::text() } else { theme::muted() };
            l.push(Line::from(vec![Span::styled(format!(" {g} "), theme::fg(col)), Span::styled(trunc(step, w.saturating_sub(3)), tst)]));
        }
        l.push(Line::default());
    }

    // context — gauge with the auto-compact threshold marked; drains smoothly after a compaction
    let m = app.registry.resolve(&a.model_alias);
    let extra = if a.compacting { vec![Span::styled(format!("  {} compacting", anim::spinner()), theme::fg(theme::AMBER))] } else { vec![] };
    l.push(sec("Context", extra));
    match (a.ctx_percent(), a.ctx_window) {
        (Some(p), Some(win)) => {
            let shown = match a.ctx_anim {
                Some((from, t0)) if t0.elapsed().as_millis() < 900 => {
                    let k = t0.elapsed().as_millis() as f32 / 900.0;
                    let ease = 1.0 - (1.0 - k).powi(3);
                    let fp = a.pct_of(from).unwrap_or(p) as f32;
                    (fp + (p as f32 - fp) * ease).round() as u8
                }
                _ => p,
            };
            let gw = w.saturating_sub(6).clamp(8, 26);
            let filled = (shown as usize * gw + 50) / 100;
            let tick = m.auto_compact_percent.map(|c| (c as usize * gw / 100).min(gw.saturating_sub(1)));
            let col = if shown >= 85 { theme::RED } else if shown >= 65 { theme::AMBER } else { theme::TEAL };
            let mut spans = vec![Span::raw(" ")];
            for i in 0..gw {
                if Some(i) == tick {
                    spans.push(Span::styled(theme::g("┊", "|"), theme::fg(theme::AMBER)));
                } else if i < filled {
                    spans.push(Span::styled(theme::g("━", "="), theme::fg(col)));
                } else {
                    spans.push(Span::styled(theme::g("─", "."), theme::faint()));
                }
            }
            spans.push(Span::styled(format!(" {shown}%"), theme::fg(col)));
            l.push(Line::from(spans));
            l.push(Line::from(Span::styled(format!(" {} / {} tokens", fmt_tokens(a.ctx_used), fmt_tokens(win)), theme::dim())));
            let note = match m.auto_compact_percent {
                Some(c) if m.context_window.is_some() => format!(" auto-compact at {c}% {}", theme::g("┊", "|")),
                _ => " auto-compact: Codex default".to_string(),
            };
            l.push(Line::from(Span::styled(note, theme::faint())));
            if shown >= 50 && !a.compacting {
                l.push(Line::from(Span::styled(" /compact frees space now", theme::fg(theme::AMBER))));
            }
        }
        _ => l.push(Line::from(Span::styled(" (after the first turn)", theme::faint()))),
    }
    l.push(Line::default());
    l.push(sec("Session", vec![]));
    l.push(Line::from(Span::styled(format!(" {} turns · {} tok · {}", a.turn_count, fmt_tokens(a.tokens_total), fmt_dur(a.created.elapsed())), theme::dim())));
    l.push(Line::from(Span::styled(format!(" {}", trunc(&a.model, w)), theme::faint())));
    let status = match &a.status {
        Status::Crashed(r) => format!("crashed: {}", trunc(r, w.saturating_sub(10))),
        Status::Failed(r) => format!("last turn failed: {}", trunc(r, w.saturating_sub(18))),
        _ => String::new(),
    };
    if !status.is_empty() {
        l.push(Line::from(Span::styled(format!(" {status}"), theme::fg(theme::RED))));
    }
    f.render_widget(Paragraph::new(l), Rect { x: inner.x + 1, width: inner.width.saturating_sub(1), ..inner });
}
