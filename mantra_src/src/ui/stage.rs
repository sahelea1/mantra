//! The Mandala stage: the overview of a multi-agent run.

use super::{theme, *};
use crate::agent::Agent;
use crate::app::App;
use crate::engine::pattern::Role;
use crate::engine::plan::Task;
use crate::engine::run::{PhaseStep, Run, Stage, WState, Worker};
use crate::hub::AgentId;
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Direction, Layout, Position};
use std::time::Duration;

// ───────────────────────────── low-level canvas ─────────────────────────────

struct Cv<'a> {
    buf: &'a mut Buffer,
    clip: Rect,
}

impl Cv<'_> {
    fn ch(&mut self, x: i32, y: i32, s: &str, st: Style) {
        if x < self.clip.x as i32 || y < self.clip.y as i32 || x >= (self.clip.x + self.clip.width) as i32 || y >= (self.clip.y + self.clip.height) as i32 {
            return;
        }
        if let Some(c) = self.buf.cell_mut(Position { x: x as u16, y: y as u16 }) {
            c.set_symbol(s);
            c.set_style(st);
        }
    }
    fn put(&mut self, x: i32, y: i32, s: &str, st: Style) -> i32 {
        let mut cx = x;
        for c in s.chars() {
            let w = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0) as i32;
            if w == 0 {
                continue;
            }
            let mut b = [0u8; 4];
            self.ch(cx, y, c.encode_utf8(&mut b), st);
            if w == 2 {
                self.ch(cx + 1, y, "", st);
            }
            cx += w;
        }
        cx
    }
    fn spans(&mut self, x: i32, y: i32, spans: &[Span], maxw: i32) {
        let mut cx = x;
        for sp in spans {
            let room = (x + maxw - cx).max(0) as usize;
            if room == 0 {
                break;
            }
            let t = trunc(&sp.content, room);
            cx = self.put(cx, y, &t, sp.style);
        }
    }
    fn hline(&mut self, x1: i32, x2: i32, y: i32, s: &str, st: Style) {
        for x in x1.min(x2)..=x1.max(x2) {
            self.ch(x, y, s, st);
        }
    }
    fn vline(&mut self, x: i32, y1: i32, y2: i32, s: &str, st: Style) {
        for y in y1.min(y2)..=y1.max(y2) {
            self.ch(x, y, s, st);
        }
    }
    fn boxed(&mut self, r: Rect, st: Style, heavy: bool, dotted: bool) {
        let (tl, tr, bl, br, h, v) = if theme::ascii() {
            ("+", "+", "+", "+", "-", "|")
        } else if heavy {
            ("┏", "┓", "┗", "┛", "━", "┃")
        } else if dotted {
            ("╭", "╮", "╰", "╯", "┄", "┆")
        } else {
            ("╭", "╮", "╰", "╯", "─", "│")
        };
        let (x0, y0) = (r.x as i32, r.y as i32);
        let (x1, y1) = (x0 + r.width as i32 - 1, y0 + r.height as i32 - 1);
        self.hline(x0 + 1, x1 - 1, y0, h, st);
        self.hline(x0 + 1, x1 - 1, y1, h, st);
        self.vline(x0, y0 + 1, y1 - 1, v, st);
        self.vline(x1, y0 + 1, y1 - 1, v, st);
        self.ch(x0, y0, tl, st);
        self.ch(x1, y0, tr, st);
        self.ch(x0, y1, bl, st);
        self.ch(x1, y1, br, st);
    }
}

// ───────────────────────────── visual state ─────────────────────────────

#[derive(PartialEq, Clone, Copy)]
enum V {
    NotStarted,
    Queued,
    Running,
    Waiting,
    Retrying,
    Done,
    Failed,
    Paused,
}

fn vstate(w: Option<&Worker>, a: Option<&Agent>) -> V {
    let Some(w) = w else { return V::NotStarted };
    if w.paused {
        return V::Paused;
    }
    match &w.state {
        WState::Queued | WState::Preparing => V::Queued,
        WState::Retrying(_) => V::Retrying,
        WState::Done => V::Done,
        WState::Failed(_) | WState::Cancelled => V::Failed,
        WState::Running => match a.map(|a| &a.status) {
            Some(Status::Waiting) => V::Waiting,
            Some(Status::Retrying(_)) => V::Retrying,
            Some(Status::Crashed(_)) | Some(Status::Failed(_)) => V::Failed,
            _ => V::Running,
        },
    }
}

fn prompting(run: &Run, a: Option<AgentId>) -> bool {
    a.and_then(|a| run.edges.get(&a)).map(|t| t.elapsed() < Duration::from_millis(2500)).unwrap_or(false)
}

fn role_col(run: &Run, role: &str) -> (u8, u8, u8) {
    run.pattern.role(role).map(|r| theme::named(&r.color)).unwrap_or(theme::TEAL)
}

// ───────────────────────────── entry ─────────────────────────────

pub fn draw(f: &mut Frame, app: &mut App) {
    let area = f.area();
    let has_run = app.run.is_some();
    // Small terminals (tmux splits!) lose the peek strip first, then the rail's second line.
    let peek_h = if has_run && !app.stage_nodes().is_empty() && area.height >= 26 { 4 } else { 0 };
    let rail_h = if !has_run { 0 } else if area.height >= 20 { 2 } else { 1 };
    let sel_agent = app.stage_nodes().get(app.sel).copied();
    let chip_h = super::queue_chip_height(app, sel_agent);
    let in_h = input_height(app, area.width).min(area.height.saturating_sub(8).max(3));
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Length(rail_h), Constraint::Min(4), Constraint::Length(peek_h), Constraint::Length(chip_h), Constraint::Length(in_h), Constraint::Length(1)])
        .split(area);
    draw_header(f, rows[0], app);
    super::draw_screen_flash(f, rows[0], app, theme::MUTED);
    draw_rail(f, rows[1], app);
    let show_pulse = app.pulse_panel && has_run && rows[2].width >= 110;
    let body = if show_pulse {
        Layout::default().direction(Direction::Horizontal).constraints([Constraint::Min(60), Constraint::Length(if rows[2].width >= 140 { 48 } else { 42 })]).split(rows[2])
    } else {
        Layout::default().direction(Direction::Horizontal).constraints([Constraint::Min(40)]).split(rows[2])
    };
    draw_canvas(f, body[0], app);
    if show_pulse {
        draw_pulse(f, body[1], app);
    }
    if peek_h > 0 {
        draw_peek(f, rows[3], app);
    }
    super::draw_queue_chip(f, rows[4], app, sel_agent);
    let placeholder = if app.canvas_focus {
        "navigating — ←→↑↓ select · ⏎ zoom · 1-9 jump · tab to type".to_string()
    } else {
        match app.run.as_ref().map(|r| &r.stage) {
            None | Some(Stage::Done) | Some(Stage::Failed(_)) => "describe what to build — the planner takes it from here…".to_string(),
            Some(Stage::Review) => "type feedback for the planner, or tab → a to approve the plan".to_string(),
            Some(_) if app.run.as_ref().map(|r| r.question.is_some()).unwrap_or(false) => "answer the planner's question…".to_string(),
            Some(Stage::Planning) | Some(Stage::Setup) => "add details for the planner…".to_string(),
            _ => "re-prompt the planner · @agent to talk to one agent directly".to_string(),
        }
    };
    draw_input(f, rows[5], app, &placeholder, theme::VIOLET, !app.canvas_focus);
    draw_suggestions(f, rows[5], app);
    let hints: Vec<(&str, &str)> = if app.canvas_focus {
        vec![("←→", "select"), ("1-9", "jump"), ("⏎", "zoom"), ("space", "pause/resume"), ("r", "retry"), ("m", "model"), ("x", "interrupt"), ("+/-", "effort"), ("p", "plan"), ("d", "diff"), ("s", "studio"), ("tab", "type")]
    } else {
        let mode: &'static str = match app.settings.approval_mode.as_str() {
            "never" => "approvals: never ask",
            "untrusted" => "approvals: untrusted",
            _ => "approvals: on-request",
        };
        vec![("⏎", "queue"), ("ctrl+f", "send now"), ("@name", "direct"), ("⇧⇥", mode), ("tab", "navigate"), ("alt+←→", "select"), ("⏎ empty", "zoom"), ("ctrl+t", "pulse"), ("ctrl+o", "solo"), ("?", "help")]
    };
    footer(f, rows[6], &hints);
}

fn draw_header(f: &mut Frame, area: Rect, app: &App) {
    let mut crumbs = vec![Span::styled("  mandala", theme::bold(theme::fg(theme::VIOLET)))];
    crumbs.push(Span::styled(format!("  {} overview", theme::g("›", ">")), theme::faint()));
    let mut right = web_badges(app);
    if let Some(b) = app.inbox_badge() {
        right.push(Span::styled(b, theme::bold(theme::fg(theme::AMBER))));
    }
    match &app.run {
        Some(r) => {
            crumbs.push(Span::styled(format!("  {}", trunc(&r.id, if area.width < 110 { 18 } else { 30 })), theme::dim()));
            crumbs.push(Span::styled(format!("  {} {}", theme::g("◈", "#"), r.pattern.name), theme::faint()));
            if app.demo {
                crumbs.insert(2, Span::styled("  DEMO", theme::bold(theme::fg(theme::ROSE))));
            }
            let active = r.all_agents().iter().filter(|a| app.agents.get(a).map(|x| x.busy()).unwrap_or(false)).count();
            let tokens = r.total_tokens(|a| app.agents.get(&a).map(|x| x.tokens_total).unwrap_or(0));
            // The halt band (drawn in the rail, just under this header) already says why and
            // what to do — no separate badge needed here.
            right.push(Span::styled(format!("{} {}", theme::g("⏱", "t"), fmt_dur(r.elapsed())), theme::muted()));
            right.push(Span::styled(format!("  Σ {} tok", fmt_tokens(tokens)), theme::muted()));
            right.push(Span::styled(format!("  {} {active} active ", theme::g("●", "*")), if active > 0 { theme::fg(theme::GREEN) } else { theme::dim() }));
        }
        None => {
            crumbs.push(Span::styled(format!("  pattern {} {}", theme::g("◈", "#"), app.pattern_name), theme::dim()));
        }
    }
    header(f, area, crumbs, right);
}

fn draw_rail(f: &mut Frame, area: Rect, app: &App) {
    let Some(r) = &app.run else {
        return;
    };
    if let Some(h) = &r.halt {
        let hint = r.halt_hint();
        let text = format!(" {} halted {} · {} · {}", theme::g("⛔", "X"), fmt_dur(h.since.elapsed()), h.message, hint);
        let st = Style::default().fg(theme::c(theme::AMBER)).add_modifier(Modifier::BOLD | Modifier::REVERSED);
        let line = Line::from(Span::styled(format!("{:<w$}", trunc(&text, area.width as usize), w = area.width as usize), st));
        f.render_widget(Paragraph::new(vec![line]), area);
        return;
    }
    if let Some(q) = &r.question {
        // The planner is asking the user something: the run keeps going, the band stays until
        // the next typed message answers it.
        let text = format!(" {} the planner asks ({}): {} · type your answer below", theme::g("?", "?"), fmt_dur(q.since.elapsed()), q.text);
        let st = Style::default().fg(theme::c(theme::SAFFRON)).add_modifier(Modifier::BOLD | Modifier::REVERSED);
        let line = Line::from(Span::styled(format!("{:<w$}", trunc(&text, area.width as usize), w = area.width as usize), st));
        f.render_widget(Paragraph::new(vec![line]), area);
        return;
    }
    let names: Vec<String> = r.plan.as_ref().map(|p| p.phases.iter().map(|x| x.name.clone()).collect()).unwrap_or_default();
    // state per rail item: 0 pending, 1 active, 2 done
    let mut items: Vec<(String, u8)> = vec![];
    let plan_state = match r.stage {
        Stage::Setup | Stage::Planning | Stage::Review => 1,
        _ => 2,
    };
    items.push(("Plan".into(), plan_state));
    for (i, n) in names.iter().enumerate() {
        let st = match &r.stage {
            Stage::Phase { idx, .. } if *idx == i => 1,
            Stage::Phase { idx, .. } if *idx > i => 2,
            Stage::Finale { .. } | Stage::Done => 2,
            _ => 0,
        };
        items.push((n.clone(), st));
    }
    let fin = match r.stage {
        Stage::Finale { .. } => 1,
        Stage::Done => 2,
        _ => 0,
    };
    if !r.pattern.flow.finale.is_empty() {
        items.push(("Finale".into(), fin));
    }
    let n = items.len().max(1);
    let seg = (area.width as usize).saturating_sub(4) / n;
    let label_w = seg.saturating_sub(6).max(3);
    let mut spans = vec![Span::raw(" ")];
    let b = anim::breath(1400);
    for (i, (name, st)) in items.iter().enumerate() {
        let (g, style) = match st {
            2 => (theme::g("✓", "v"), theme::fg(theme::GREEN)),
            1 => (theme::g("◉", "@"), Style::default().fg(theme::mix(theme::SAFFRON, theme::TEXT, b * 0.5)).add_modifier(Modifier::BOLD)),
            _ => (theme::g("○", "o"), theme::faint()),
        };
        let name_style = match st {
            1 => theme::bold(theme::text()),
            2 => theme::muted(),
            _ => theme::dim(),
        };
        spans.push(Span::styled(format!("{g} "), style));
        let label = trunc(name, label_w);
        let used = w_of(&label) + 2;
        spans.push(Span::styled(label, name_style));
        if i + 1 < n {
            let link = seg.saturating_sub(used).clamp(2, 12).saturating_sub(2);
            let bar = theme::g("━", "-");
            // item i ≥ 1 is phase i-1: sweep its link green for a moment after it completes
            let k = if i >= 1 { r.history.get(i - 1).map(|h| (h.ended.elapsed().as_millis() as f32 / 700.0).min(1.0)).unwrap_or(1.0) } else { 1.0 };
            if *st == 2 && k < 1.0 && theme::motion() {
                let g = ((link as f32) * k).round() as usize;
                spans.push(Span::raw(" "));
                spans.push(Span::styled(bar.repeat(g), theme::bold(theme::fg(theme::GREEN))));
                spans.push(Span::styled(bar.repeat(link - g), theme::faint()));
                spans.push(Span::raw(" "));
            } else {
                let lst = if *st == 2 { theme::fg(theme::GREEN) } else { theme::faint() };
                spans.push(Span::styled(format!(" {} ", bar.repeat(link)), lst));
            }
        }
    }
    let mut lines = vec![Line::from(spans)];
    if let Stage::Phase { step, .. } = &r.stage {
        let s = match step {
            PhaseStep::Orchestrating => {
                let done = r.workers.iter().filter(|w| w.state == WState::Done).count();
                let total = r.current_phase().map(|p| p.tasks.len()).unwrap_or(0);
                format!("building · {done}/{total} tasks done")
            }
            PhaseStep::Merging => "merging worker branches".into(),
            PhaseStep::Checks { round } => format!("running gate checks{}", if *round > 0 { " (verify)" } else { "" }),
            PhaseStep::Gate { round } => format!("QA gate · round {round}"),
            PhaseStep::Handoff => "handoff → next phase".into(),
        };
        lines.push(Line::from(Span::styled(format!("   {s}"), theme::faint())));
    }
    f.render_widget(Paragraph::new(lines), area);
}

fn draw_canvas(f: &mut Frame, area: Rect, app: &App) {
    let nodes = app.stage_nodes();
    let Some(run) = &app.run else {
        welcome(f, area, app);
        return;
    };
    let buf = f.buffer_mut();
    let mut cv = Cv { buf, clip: area };
    match &run.stage {
        Stage::Setup => {
            let y = area.y as i32 + area.height as i32 / 2;
            let t = format!("{} preparing workspace…", anim::spinner());
            cv.put(area.x as i32 + (area.width as i32 - w_of(&t) as i32) / 2, y, &t, theme::muted());
        }
        Stage::Planning | Stage::Review => planning(&mut cv, area, app, run, &nodes),
        Stage::Phase { .. } => phase(&mut cv, area, app, run, &nodes),
        Stage::Finale { idx } => finale(&mut cv, area, app, run, *idx, &nodes),
        Stage::Done | Stage::Failed(_) => done(&mut cv, area, app, run),
    }
}

fn welcome(f: &mut Frame, area: Rect, app: &App) {
    let p = &app.studio.pattern;
    let mut l = vec![
        Line::default(),
        Line::from(Span::styled(format!("   {}  M A N D A L A", theme::g("✦", "*")), theme::bold(theme::fg(theme::VIOLET)))),
        Line::from(Span::styled("   describe what to build; a whole team of agents plans it, builds it in parallel phases, and checks it", theme::dim())),
        Line::default(),
        Line::from(Span::styled(format!("   pattern  {}", p.name), theme::bold(theme::text()))),
        Line::from(Span::styled(format!("   {}", p.description), theme::muted())),
        Line::default(),
    ];
    for (name, r) in p.ordered_roles() {
        let m = app.registry.resolve(&r.model);
        l.push(Line::from(vec![
            Span::styled(format!("   {} ", theme::role_glyph(&r.glyph)), theme::bold(theme::fg(theme::named(&r.color)))),
            Span::styled(format!("{name:<14}"), theme::text()),
            Span::styled(format!("{:<12}", r.kind), theme::faint()),
            Span::styled(format!("{} · ", m.alias), theme::muted()),
            Span::styled(r.effort.clone(), theme::fg(effort_color(&r.effort))),
            Span::styled(format!("   {}", trunc(&r.description, 50)), theme::faint()),
        ]));
    }
    l.push(Line::default());
    let chain: Vec<String> = p.flow.finale.iter().map(|s| format!("{} {}", p.role(&s.role).map(|r| theme::role_glyph(&r.glyph)).unwrap_or_default(), s.role)).collect();
    l.push(Line::from(Span::styled(format!("   flow  plan → phases (parallel workers → {} gate) → {}", p.flow.phase_gate, chain.join(" → ")), theme::dim())));
    l.push(Line::from(Span::styled("   /pattern to switch · s or /studio to edit · /models for models & effort", theme::faint())));
    if !app.unfinished_runs.is_empty() {
        let n = app.unfinished_runs.len();
        l.push(Line::default());
        l.push(Line::from(Span::styled(format!("   {} {n} unfinished run{} — /runs to resume or delete", theme::g("↻", "~"), if n == 1 { "" } else { "s" }), theme::fg(theme::AMBER))));
    }
    if let Some(w) = &app.sandbox_warning {
        l.push(Line::default());
        let width = area.width.saturating_sub(16) as usize;
        l.push(Line::from(Span::styled(format!("   {} sandbox: {}", theme::g("⚠", "!"), trunc(w, width.max(20))), theme::fg(theme::AMBER))));
        l.push(Line::from(Span::styled("     workers' commands would fail under Codex's sandbox — mantra doctor shows the fix", theme::faint())));
    }
    f.render_widget(Paragraph::new(l), area);
}

// ───────────────────────────── cards ─────────────────────────────

fn card_border(base: (u8, u8, u8), v: V, a: Option<&Agent>) -> (Style, bool) {
    let b = anim::breath(1800);
    let born = a.map(|a| anim::fade(Some(a.created), 500)).unwrap_or(0.0);
    let done_flash = a.and_then(|a| a.finished).map(|t| anim::fade(Some(t), 1200)).unwrap_or(0.0);
    let (col, dotted) = match v {
        V::NotStarted | V::Queued => (theme::c(theme::FAINT), true),
        V::Running => (theme::mix(theme::mix_rgb(base, theme::FAINT), base, 0.4 + 0.6 * b), false),
        V::Waiting => (theme::mix(theme::AMBER, theme::TEXT, b * 0.4), false),
        V::Retrying => (theme::c(theme::AMBER), true),
        V::Done => (theme::mix(theme::mix_rgb(theme::GREEN, theme::FAINT), theme::TEXT, done_flash), false),
        V::Failed => (theme::c(theme::RED), false),
        V::Paused => (theme::c(theme::AMBER), true),
    };
    let col = if born > 0.0 { theme::mix(theme::rgb_of(col), theme::TEXT, born) } else { col };
    (Style::default().fg(col), dotted)
}

#[allow(clippy::too_many_arguments)]
fn worker_card(cv: &mut Cv, r: Rect, run: &Run, t: &Task, w: Option<&Worker>, a: Option<&Agent>, selected: bool, app: &App) {
    let role: Option<&Role> = run.pattern.role(&t.role);
    let base = role.map(|r| theme::named(&r.color)).unwrap_or(theme::TEAL);
    let v = vstate(w, a);
    let (mut st, mut dotted) = card_border(base, v, a);
    // WP7.6: the watchdog escalation ladder has fired on this agent — override the border amber
    // dotted regardless of its ordinary visual state, and (below) its activity line.
    let watchdog = a.and_then(|ag| run.watchdog_idle(ag.id));
    if watchdog.is_some() {
        st = theme::fg(theme::AMBER);
        dotted = true;
    }
    if selected {
        st = st.fg(theme::c(theme::TEXT)).add_modifier(Modifier::BOLD);
    }
    cv.boxed(r, st, selected, dotted && !selected);
    let (x, y, iw) = (r.x as i32 + 2, r.y as i32, r.width as i32 - 4);
    let dimmed = matches!(v, V::NotStarted | V::Queued);
    let gst = if dimmed { theme::faint() } else { theme::bold(theme::fg(base)) };
    let glyph = role.map(|r| theme::role_glyph(&r.glyph)).unwrap_or_else(|| "◇".into());
    let title_end = cv.spans(x - 1, y, &[Span::raw(" "), Span::styled(format!("{glyph} "), gst), Span::styled(trunc(&t.id, (iw - 6).max(4) as usize), if dimmed { theme::dim() } else { theme::bold(theme::text()) }), Span::raw(" ")], iw);
    let _ = title_end;
    let mark = match v {
        V::Running => anim::spinner().to_string(),
        V::Waiting => theme::g("⚑", "!").to_string(),
        V::Retrying => theme::g("↻", "r").to_string(),
        V::Done => theme::g("✓", "v").to_string(),
        V::Failed => theme::g("✗", "x").to_string(),
        V::Paused => theme::g("‖", "=").to_string(),
        V::Queued => theme::g("…", ".").to_string(),
        V::NotStarted => theme::g("○", "o").to_string(),
    };
    let mcol = match v {
        V::Done => theme::GREEN,
        V::Failed => theme::RED,
        V::Waiting | V::Retrying | V::Paused => theme::AMBER,
        V::Running => base,
        _ => theme::FAINT,
    };
    cv.put(r.x as i32 + r.width as i32 - 3, y, &mark, theme::bold(theme::fg(mcol)));
    let h = r.height as i32;
    // line 1: model · effort · progress
    let m_alias = a.map(|a| a.model_alias.clone()).or_else(|| role.map(|r| r.model.clone())).unwrap_or_default();
    let eff = a.map(|a| a.effort.clone()).or_else(|| t.effort.clone()).or_else(|| role.map(|r| r.effort.clone())).unwrap_or_default();
    let (d, tot) = a.map(|a| a.plan_progress()).unwrap_or((0, 0));
    let eff = if app.registry.resolve(&m_alias).efforts().is_empty() { String::new() } else { eff };
    let mut l1 = vec![Span::styled(if eff.is_empty() { m_alias.clone() } else { format!("{m_alias}·") }, if dimmed { theme::faint() } else { theme::muted() }), Span::styled(eff.clone(), if dimmed { theme::faint() } else { theme::fg(effort_color(&eff)) })];
    if tot > 0 {
        let bar: String = (0..tot).map(|i| if i < d { theme::g("▰", "#") } else { theme::g("▱", "-") }).collect();
        l1.push(Span::styled(format!("  {bar} {d}/{tot}"), theme::dim()));
    }
    if h >= 3 {
        cv.spans(x, y + 1, &l1, iw);
    }
    // line 2: activity
    if h >= 4 {
        let line: Vec<Span> = if let Some(idle) = watchdog {
            vec![Span::styled(format!("idle {}m · watchdog", (idle.as_secs() / 60).max(1)), theme::fg(theme::AMBER))]
        } else {
            match (v, a) {
                (V::NotStarted, _) => vec![Span::styled(trunc(&t.title, iw as usize), theme::faint())],
                (V::Queued, _) => vec![Span::styled("waiting for a free slot", theme::faint())],
                (V::Done, _) => {
                    let s = w.map(|w| w.report.lines().find(|l| l.to_lowercase().starts_with("summary:")).map(|l| l[8..].trim().to_string()).unwrap_or_else(|| t.title.clone())).unwrap_or_default();
                    vec![Span::styled(trunc(&s, iw as usize), theme::dim())]
                }
                (V::Failed, _) => {
                    let m = match w.map(|w| &w.state) {
                        Some(WState::Failed(m)) => m.clone(),
                        _ => a.map(|a| a.activity.clone()).unwrap_or_default(),
                    };
                    vec![Span::styled(trunc(&m, iw as usize), theme::fg(theme::RED))]
                }
                (V::Retrying, _) => {
                    let t = match w.map(|w| &w.state) {
                        Some(WState::Retrying(at)) => format!("retrying in {}s", at.saturating_duration_since(std::time::Instant::now()).as_secs() + 1),
                        _ => format!("retrying · {}", a.map(|a| a.activity.clone()).unwrap_or_default()),
                    };
                    vec![Span::styled(trunc(&t, iw as usize), theme::fg(theme::AMBER))]
                }
                (V::Waiting, _) => vec![Span::styled("needs approval · ctrl+g", theme::fg(theme::AMBER))],
                (_, Some(a)) if !a.busy() && a.stopped_by_user => vec![Span::styled("stopped by you · r respawn", theme::fg(theme::AMBER))],
                (_, Some(a)) if !a.busy() && run.waiting_for_answer(a.id) => vec![Span::styled("asked a question · waiting for the answer", theme::fg(theme::AMBER))],
                // The shimmer only proves the frame is repainting; a `last_event` that has stopped
                // moving is what actually answers "is it stuck?". The suffix takes its columns off
                // the activity rather than pushing the line past the card's inner width.
                (_, Some(a)) if a.busy() => match quiet_for(a) {
                    Some(q) => {
                        let tail = format!(" · quiet {}", fmt_dur(q));
                        let room = (iw as usize).saturating_sub(w_of(&tail));
                        let mut v = anim::shimmer(&trunc(&a.activity, room), theme::mix_rgb(base, theme::MUTED), theme::TEXT);
                        v.push(Span::styled(tail, theme::fg(theme::AMBER)));
                        v
                    }
                    None => anim::shimmer(&trunc(&a.activity, iw as usize), theme::mix_rgb(base, theme::MUTED), theme::TEXT),
                },
                (_, Some(a)) => vec![Span::styled(trunc(&a.activity, iw as usize), theme::dim())],
                _ => vec![],
            }
        };
        cv.spans(x, y + 2, &line, iw);
    }
    // line 3: time · tokens · badges
    if h >= 5 {
        let mut l3 = vec![];
        if let (Some(w), Some(a)) = (w, a) {
            let el = w.finished.map(|f| f.duration_since(w.spawned)).unwrap_or_else(|| w.spawned.elapsed());
            l3.push(Span::styled(format!("{} · {}", fmt_dur(el), fmt_tokens(a.tokens_total)), theme::faint()));
            if w.attempt > 1 {
                l3.push(Span::styled(format!("  {}{}", theme::g("↻", "r"), w.attempt), theme::fg(theme::AMBER)));
            }
            if !w.tripwires.is_empty() {
                l3.push(Span::styled(format!("  {} scope", theme::g("⚠", "!")), theme::fg(theme::AMBER)));
            }
            if prompting(run, w.agent) {
                l3.push(Span::styled(format!("  {} prompted", theme::g("◂", "<")), theme::fg(theme::ROSE)));
            }
        } else if let Some(sc) = t.scope.first() {
            l3.push(Span::styled(trunc(sc, iw as usize), theme::faint()));
        }
        cv.spans(x, y + 3, &l3, iw);
    }
    let _ = app;
}

#[allow(clippy::too_many_arguments)]
fn agent_card(cv: &mut Cv, r: Rect, run: &Run, a: Option<&Agent>, name: &str, glyph: &str, color: (u8, u8, u8), line2: Vec<Span<'static>>, selected: bool) {
    let v = match a {
        None => V::NotStarted,
        Some(a) => match &a.status {
            Status::Busy => V::Running,
            Status::Waiting => V::Waiting,
            Status::Retrying(_) => V::Retrying,
            Status::Failed(_) | Status::Crashed(_) => V::Failed,
            Status::Stopped => V::Done,
            _ => V::Queued,
        },
    };
    let (mut st, _) = card_border(color, if v == V::Queued { V::Done } else { v }, a);
    if v == V::Queued {
        st = theme::fg(theme::mix_rgb(color, theme::FAINT));
    }
    // WP7.6: watched-idle agent (planner/orchestrator/gate/finale) — amber dotted border + an
    // "idle Nm · watchdog" label overriding the ordinary line2 content, same treatment as
    // `worker_card`.
    let watchdog = a.and_then(|ag| run.watchdog_idle(ag.id));
    let mut dotted = a.is_none();
    if watchdog.is_some() {
        st = theme::fg(theme::AMBER);
        dotted = true;
    }
    if selected {
        st = st.fg(theme::c(theme::TEXT)).add_modifier(Modifier::BOLD);
    }
    cv.boxed(r, st, selected, dotted && !selected);
    let (x, y, iw) = (r.x as i32 + 2, r.y as i32, r.width as i32 - 4);
    let mut title = vec![Span::raw(" "), Span::styled(format!("{} ", theme::role_glyph(glyph)), theme::bold(theme::fg(color))), Span::styled(name.to_string(), theme::bold(theme::text()))];
    if let Some(a) = a {
        let m = if a.effort.is_empty() { format!("  {} ", a.model_alias) } else { format!("  {}·{} ", a.model_alias, a.effort) };
        if (w_of(name) + w_of(&m) + 6) as i32 <= iw {
            title.push(Span::styled(m, theme::dim()));
        }
    }
    title.push(Span::raw(" "));
    cv.spans(x - 1, y, &title, iw);
    if let Some(a) = a {
        if a.busy() {
            cv.put(r.x as i32 + r.width as i32 - 3, y, anim::spinner(), theme::bold(theme::fg(color)));
        }
        let act: Vec<Span> = if a.busy() { anim::shimmer(&trunc(&a.activity, iw as usize), theme::mix_rgb(color, theme::MUTED), theme::TEXT) } else { vec![Span::styled(trunc(&a.activity, iw as usize), theme::dim())] };
        if r.height >= 3 {
            cv.spans(x, y + 1, &act, iw);
        }
    }
    if r.height >= 4 {
        let line2 = match watchdog {
            Some(idle) => vec![Span::styled(format!("idle {}m · watchdog", (idle.as_secs() / 60).max(1)), theme::fg(theme::AMBER))],
            None => line2,
        };
        cv.spans(x, y + 2, &line2, iw);
    }
}

// ───────────────────────────── stages ─────────────────────────────

fn planning(cv: &mut Cv, area: Rect, app: &App, run: &Run, nodes: &[AgentId]) {
    let a = run.planner.and_then(|p| app.agents.get(&p));
    let role = run.pattern.role(&run.pattern.flow.planner).cloned().unwrap_or_default();
    let w = (area.width as i32 - 4).clamp(30, 76);
    let h = (area.height as i32 - 2).clamp(5, 14);
    let r = Rect { x: (area.x as i32 + (area.width as i32 - w) / 2) as u16, y: area.y + 1, width: w as u16, height: h as u16 };
    let line2 = match (run.stage == Stage::Review, a.map(|a| &a.status)) {
        (true, _) => vec![Span::styled(format!("{} plan ready · p to review · tab → a to approve · or type feedback", theme::g("☰", "=")), theme::bold(theme::accent()))],
        (_, _) if run.halted() => vec![Span::styled(format!("{} halted — {}", theme::g("⛔", "X"), run.halt_hint()), theme::fg(theme::AMBER))],
        (_, Some(Status::Failed(m))) | (_, Some(Status::Crashed(m))) => vec![Span::styled(trunc(m, (w as usize).saturating_sub(4)), theme::fg(theme::RED))],
        (_, Some(Status::Retrying(m))) => vec![Span::styled(format!("retrying: {}", trunc(m, (w as usize).saturating_sub(14))), theme::fg(theme::AMBER))],
        (_, Some(Status::Starting)) | (_, None) => vec![Span::styled("starting codex…", theme::faint())],
        _ => vec![Span::styled("exploring the repo & designing phases…", theme::faint())],
    };
    let sel = nodes.first().copied() == run.planner && app.sel == 0;
    agent_card(cv, r, run, a, "planner", &role.glyph, theme::named(&role.color), line2, sel);
    if let Some(a) = a {
        let lines = a.tail_lines((h as usize).saturating_sub(5));
        for (i, l) in lines.iter().enumerate() {
            cv.put(r.x as i32 + 3, r.y as i32 + 4 + i as i32, &trunc(l, (w as usize).saturating_sub(6)), theme::dim());
        }
    }
    if let Some(p) = &run.plan {
        let y = r.y as i32 + h + 1;
        let ntasks: usize = p.phases.iter().map(|x| x.tasks.len()).sum();
        let s = format!("{}  ·  {} phases · {} tasks · v{}", trunc(&p.title, 40), p.phases.len(), ntasks, run.plan_version);
        cv.put(area.x as i32 + (area.width as i32 - w_of(&s) as i32) / 2, y, &s, theme::muted());
    }
}

fn phase(cv: &mut Cv, area: Rect, app: &App, run: &Run, nodes: &[AgentId]) {
    let Some(ph) = run.current_phase().cloned() else { return };
    let sel = nodes.get(app.sel).copied();
    let (x0, y0, w, h) = (area.x as i32, area.y as i32, area.width as i32, area.height as i32);
    let cx = x0 + w / 2;
    let n = ph.tasks.len().max(1) as i32;
    let gap = 2;
    let cw_min = 22;
    let cols = ((w - 2 + gap) / (cw_min + gap)).clamp(1, n);
    let cw = ((w - 2 - (cols - 1) * gap) / cols).min(32);
    let rows = (n + cols - 1) / cols;
    let need = |ch: i32, gate: bool| 7 + rows * ch + (rows - 1) * 2 + if gate { 8 } else { 0 };
    let (card_h, show_gate) = if need(5, true) <= h {
        (5, true)
    } else if need(4, true) <= h {
        (4, true)
    } else if need(4, false) <= h {
        (4, false)
    } else if need(3, false) <= h {
        (3, false)
    } else {
        (0, false)
    };
    let worker_of = |t: &Task| run.workers.iter().rev().find(|w| w.task.id == t.id);
    if card_h == 0 || w < 30 {
        // compact list fallback
        let mut y = y0;
        for t in &ph.tasks {
            let wk = worker_of(t);
            let a = wk.and_then(|w| w.agent).and_then(|a| app.agents.get(&a));
            let v = vstate(wk, a);
            let col = role_col(run, &t.role);
            let glyph = run.pattern.role(&t.role).map(|r| theme::role_glyph(&r.glyph)).unwrap_or_default();
            let is_sel = wk.and_then(|w| w.agent).is_some() && wk.and_then(|w| w.agent) == sel;
            let act = a.map(|a| a.activity.clone()).unwrap_or_else(|| if v == V::NotStarted { "not started".into() } else { "queued".into() });
            let line = format!("{} {glyph} {:<14} {}", if is_sel { theme::g("▶", ">") } else { " " }, t.id, act);
            cv.put(x0 + 1, y, &trunc(&line, (w as usize).saturating_sub(2)), if matches!(v, V::NotStarted | V::Queued) { theme::faint() } else { theme::fg(col) });
            y += 1;
            if y >= y0 + h {
                break;
            }
        }
        return;
    }

    // ── top row: planner mini-node + orchestrator (+ the manager mini-node on the right)
    let show_planner = w >= 84;
    let show_manager = show_planner && run.manager.is_some() && w >= 90;
    // The orchestrator stays centred over the fork, but shifts to clear the side nodes.
    let orch_w = if show_manager {
        (w - 60).clamp(30, 56)
    } else if show_planner {
        (w - 32).clamp(34, 56)
    } else {
        (w - 4).clamp(30, 62)
    };
    let mut ox = cx - orch_w / 2;
    if show_planner {
        let right = if show_manager { x0 + w - 28 - orch_w } else { x0 + w - orch_w - 1 };
        ox = ox.max(x0 + 29).min(right);
    }
    let orch = Rect { x: ox as u16, y: y0 as u16, width: orch_w as u16, height: 4 };
    let oa = run.orchestrator.and_then(|o| app.agents.get(&o));
    let ocol = role_col(run, &run.pattern.flow.orchestrator);
    let oglyph = run.pattern.role(&run.pattern.flow.orchestrator).map(|r| r.glyph.clone()).unwrap_or_else(|| "◉".into());
    let running = run.workers.iter().filter(|w| w.state == WState::Running).count();
    let watching = if running > 0 && oa.map(|a| !a.busy()).unwrap_or(false) {
        vec![Span::styled(format!("{} watching {running} worker{}", theme::g("◉", "@"), if running == 1 { "" } else { "s" }), theme::fg(theme::GREEN))]
    } else {
        let last = oa.and_then(|a| a.tail_lines(1).pop()).unwrap_or_default();
        vec![Span::styled(trunc(&last, (orch_w as usize).saturating_sub(4)), theme::faint())]
    };
    agent_card(cv, orch, run, oa, "orchestrator", &oglyph, ocol, watching, run.orchestrator.is_some() && run.orchestrator == sel);
    if show_planner {
        let pr = Rect { x: (x0 + 1) as u16, y: y0 as u16, width: 26, height: 3 };
        let pa = run.planner.and_then(|p| app.agents.get(&p));
        let pcol = role_col(run, &run.pattern.flow.planner);
        let pglyph = run.pattern.role(&run.pattern.flow.planner).map(|r| r.glyph.clone()).unwrap_or_else(|| "✦".into());
        agent_card(cv, pr, run, pa, "planner", &pglyph, pcol, vec![], run.planner.is_some() && run.planner == sel);
        // planner → orchestrator edge when briefing
        let ey = y0 + 1;
        let (ex1, ex2) = (x0 + 27, orch.x as i32 - 1);
        if ex2 > ex1 {
            if prompting(run, run.orchestrator) && pa.map(|a| a.busy() || a.last_event.elapsed() < Duration::from_secs(3)).unwrap_or(false) {
                let ph = anim::march(90) as i32;
                for x in ex1..=ex2 {
                    let c = if (x - ph).rem_euclid(4) == 0 { theme::g("▸", ">") } else { theme::g("─", "-") };
                    cv.ch(x, ey, c, theme::fg(theme::ROSE));
                }
            } else {
                cv.hline(ex1, ex2, ey, theme::g("┄", "."), theme::faint());
            }
        }
    }
    if show_manager {
        // The manager watches from the right: a busy manager is intervening somewhere, so its
        // edge to the orchestrator marches like a prompt; otherwise it is a quiet dotted line.
        let mr = Rect { x: (x0 + w - 27) as u16, y: y0 as u16, width: 26, height: 3 };
        let ma = run.manager.and_then(|m| app.agents.get(&m));
        let mcol = role_col(run, &run.pattern.flow.manager);
        let mglyph = run.pattern.role(&run.pattern.flow.manager).map(|r| r.glyph.clone()).unwrap_or_else(|| "◈".into());
        agent_card(cv, mr, run, ma, "manager", &mglyph, mcol, vec![], run.manager.is_some() && run.manager == sel);
        let ey = y0 + 1;
        let (ex1, ex2) = (orch.x as i32 + orch.width as i32, mr.x as i32 - 1);
        if ex2 > ex1 {
            if ma.map(|a| a.busy()).unwrap_or(false) {
                let ph = anim::march(90) as i32;
                for x in ex1..=ex2 {
                    let c = if (x + ph).rem_euclid(4) == 0 { theme::g("◂", "<") } else { theme::g("─", "-") };
                    cv.ch(x, ey, c, theme::fg(mcol));
                }
            } else {
                cv.hline(ex1, ex2, ey, theme::g("┄", "."), theme::faint());
            }
        }
    }

    // ── fork: stem, bus, drops
    let line = |s: &'static str, a: &'static str| theme::g(s, a);
    let any_prompt = run.workers.iter().any(|wk| prompting(run, wk.agent));
    let obusy = oa.map(|a| a.busy()).unwrap_or(false);
    let stem_st = if any_prompt { theme::fg(theme::ROSE) } else if obusy { theme::fg(ocol) } else if running > 0 { theme::fg(theme::GREEN) } else { theme::faint() };
    let y_stem = y0 + 4;
    let y_bus = y0 + 5;
    cv.ch(cx, y_stem, line("│", "|"), stem_st);
    let bus_st = theme::fg(theme::mix_rgb(ocol, theme::FAINT));
    let mut trunk_x = i32::MAX;
    let mut centers: Vec<Vec<(i32, usize)>> = vec![];
    for r in 0..rows {
        let k = (n - r * cols).min(cols);
        let tw = k * cw + (k - 1) * gap;
        let xs = cx - tw / 2;
        trunk_x = trunk_x.min(xs - 2);
        centers.push((0..k).map(|i| (xs + i * (cw + gap) + cw / 2, (r * cols + i) as usize)).collect());
    }
    trunk_x = trunk_x.max(x0);
    let y_card = |r: i32| y0 + 7 + r * (card_h + 2);
    for r in 0..rows {
        let row = &centers[r as usize];
        let by = if r == 0 { y_bus } else { y_card(r) - 2 };
        let (first, last) = (row.first().unwrap().0, row.last().unwrap().0);
        let (l, rr) = if r == 0 { (first.min(cx), last.max(cx)) } else { (trunk_x, last) };
        let l = if r == 0 && rows > 1 { trunk_x } else { l };
        cv.hline(l, rr, by, line("─", "-"), bus_st);
        // prompting segments light up from the orchestrator stem to the card
        for (c, ti) in row {
            let t = &ph.tasks[*ti];
            let wk = worker_of(t);
            if prompting(run, wk.and_then(|w| w.agent)) && r == 0 {
                let ph_ = anim::march(80) as i32;
                let (a, b) = (cx.min(*c), cx.max(*c));
                for x in a..=b {
                    let fwd = *c >= cx;
                    let arrow = if fwd { theme::g("▸", ">") } else { theme::g("◂", "<") };
                    let k = if fwd { x - ph_ } else { x + ph_ };
                    cv.ch(x, by, if k.rem_euclid(4) == 0 { arrow } else { line("━", "=") }, theme::fg(theme::ROSE));
                }
            }
        }
        for (c, _) in row {
            cv.ch(*c, by, line("┬", "+"), bus_st);
        }
        if r == 0 {
            let j = if row.iter().any(|(c, _)| *c == cx) { line("┼", "+") } else { line("┴", "+") };
            cv.ch(cx, by, j, stem_st);
            if rows > 1 {
                cv.ch(trunk_x, by, line("┌", "+"), bus_st);
            }
        } else {
            cv.vline(trunk_x, y_card(r - 1) - 1, by - 1, line("│", "|"), bus_st);
            cv.ch(trunk_x, by, if r + 1 < rows { line("├", "+") } else { line("└", "+") }, bus_st);
        }
        // drops + cards
        for (c, ti) in row {
            let t = &ph.tasks[*ti];
            let wk = worker_of(t);
            let aid = wk.and_then(|w| w.agent);
            let a = aid.and_then(|x| app.agents.get(&x));
            let v = vstate(wk, a);
            let (dc, dst) = if prompting(run, aid) {
                (line("▼", "v"), theme::bold(theme::fg(theme::ROSE)))
            } else {
                match v {
                    V::Running => (line("│", "|"), Style::default().fg(theme::mix(theme::mix_rgb(theme::GREEN, theme::FAINT), theme::GREEN, 0.45 + 0.55 * anim::breath(2000)))),
                    V::Waiting | V::Retrying | V::Paused => (line("┆", ":"), theme::fg(theme::AMBER)),
                    V::Done => (line("│", "|"), theme::fg(theme::mix_rgb(theme::GREEN, theme::FAINT))),
                    V::Failed => (line("│", "|"), theme::fg(theme::RED)),
                    _ => (line("┆", ":"), theme::faint()),
                }
            };
            cv.ch(*c, y_card(r) - 1, dc, dst);
            let rect = Rect { x: (*c - cw / 2) as u16, y: y_card(r) as u16, width: cw as u16, height: card_h as u16 };
            worker_card(cv, rect, run, t, wk, a, aid.is_some() && aid == sel, app);
        }
    }

    // ── join + gate
    if !show_gate {
        return;
    }
    let last_row = &centers[(rows - 1) as usize];
    let yb = y_card(rows - 1) + card_h;
    let all_done = ph.tasks.iter().all(|t| worker_of(t).map(|w| w.state == WState::Done).unwrap_or(false));
    let jst = if all_done { theme::fg(theme::GREEN) } else { theme::faint() };
    for (c, ti) in last_row {
        let done = worker_of(&ph.tasks[*ti]).map(|w| w.state == WState::Done).unwrap_or(false);
        cv.ch(*c, yb, if done { line("│", "|") } else { line("┆", ":") }, if done { theme::fg(theme::GREEN) } else { theme::faint() });
    }
    let (first, last) = (last_row.first().unwrap().0, last_row.last().unwrap().0);
    cv.hline(first.min(cx), last.max(cx), yb + 1, line("─", "-"), jst);
    for (c, _) in last_row {
        cv.ch(*c, yb + 1, line("┴", "+"), jst);
    }
    cv.ch(cx, yb + 1, if last_row.iter().any(|(c, _)| *c == cx) { line("┼", "+") } else { line("┬", "+") }, jst);
    cv.ch(cx, yb + 2, line("│", "|"), jst);
    let gw = (w - 4).clamp(30, 56);
    let gr = Rect { x: (cx - gw / 2) as u16, y: (yb + 3) as u16, width: gw as u16, height: 4 };
    gate_card(cv, gr, app, run, sel);
}

fn gate_card(cv: &mut Cv, r: Rect, app: &App, run: &Run, sel: Option<AgentId>) {
    let gname = run.pattern.flow.phase_gate.clone();
    let role = run.pattern.role(&gname).cloned().unwrap_or_default();
    let col = theme::named(&role.color);
    let ga = run.gate_agent.and_then(|g| app.agents.get(&g));
    let step = match &run.stage {
        Stage::Phase { step, .. } => step.clone(),
        _ => PhaseStep::Orchestrating,
    };
    let mut line2: Vec<Span<'static>> = vec![];
    match &step {
        PhaseStep::Orchestrating => {
            let done = run.workers.iter().filter(|w| w.state == WState::Done).count();
            let total = run.current_phase().map(|p| p.tasks.len()).unwrap_or(0);
            line2.push(Span::styled(format!("gate opens when all tasks are done ({done}/{total})"), theme::faint()));
        }
        PhaseStep::Merging => line2.push(Span::styled(format!("{} merging branches…", anim::spinner()), theme::fg(theme::AMBER))),
        _ => {
            if run.checks.is_empty() && matches!(step, PhaseStep::Checks { .. }) {
                line2.push(Span::styled(format!("{} running checks…", anim::spinner()), theme::fg(theme::BLUE)));
            }
            for c in &run.checks {
                line2.push(Span::styled(format!("{} ", if c.ok { theme::g("✓", "v") } else { theme::g("✗", "x") }), theme::fg(if c.ok { theme::GREEN } else { theme::RED })));
                line2.push(Span::styled(format!("{}  ", trunc(&c.cmd, 18)), theme::dim()));
            }
            if !run.conflicts.is_empty() {
                line2.push(Span::styled(format!("{} {} conflict(s)", theme::g("⚠", "!"), run.conflicts.len()), theme::fg(theme::AMBER)));
            }
        }
    }
    if ga.is_none() {
        let mut st = theme::fg(if matches!(step, PhaseStep::Orchestrating) { theme::FAINT } else { theme::mix_rgb(col, theme::FAINT) });
        if matches!(step, PhaseStep::Handoff) {
            st = theme::fg(theme::GREEN);
        }
        cv.boxed(r, st, false, matches!(step, PhaseStep::Orchestrating));
        cv.spans(r.x as i32 + 1, r.y as i32, &[Span::raw(" "), Span::styled(format!("{} ", theme::role_glyph(&role.glyph)), theme::bold(theme::fg(col))), Span::styled(format!("gate · {gname} "), theme::bold(theme::text()))], r.width as i32 - 2);
        let l1 = if matches!(step, PhaseStep::Handoff) { format!("{} passed — handing off", theme::g("✓", "v")) } else { format!("{} · {}", role.model, role.effort) };
        cv.put(r.x as i32 + 2, r.y as i32 + 1, &trunc(&l1, (r.width as usize).saturating_sub(4)), theme::dim());
        cv.spans(r.x as i32 + 2, r.y as i32 + 2, &line2, r.width as i32 - 4);
    } else {
        let round = if let PhaseStep::Gate { round } = step { format!("round {round} · ") } else { String::new() };
        let mut l = vec![Span::styled(round, theme::fg(col))];
        l.extend(line2);
        agent_card(cv, r, run, ga, &format!("gate · {gname}"), &role.glyph, col, l, run.gate_agent.is_some() && run.gate_agent == sel);
    }
}

fn finale(cv: &mut Cv, area: Rect, app: &App, run: &Run, idx: usize, nodes: &[AgentId]) {
    let sel = nodes.get(app.sel).copied();
    let steps = &run.pattern.flow.finale;
    let w = (area.width as i32 - 4).clamp(30, 60);
    let x = area.x as i32 + 2;
    let mut y = area.y as i32;
    for (i, s) in steps.iter().enumerate() {
        let role = run.pattern.role(&s.role).cloned().unwrap_or_default();
        let col = theme::named(&role.color);
        let r = Rect { x: x as u16, y: y as u16, width: w as u16, height: 3 };
        if i == idx {
            let a = run.finale_agent.and_then(|a| app.agents.get(&a));
            agent_card(cv, Rect { height: 4, ..r }, run, a, &s.role, &role.glyph, col, vec![Span::styled(trunc(&s.task, (w as usize).saturating_sub(4)), theme::faint())], run.finale_agent.is_some() && run.finale_agent == sel);
            y += 4;
        } else {
            let (st, mark) = if i < idx { (theme::fg(theme::mix_rgb(theme::GREEN, theme::FAINT)), theme::g("✓", "v")) } else { (theme::faint(), theme::g("○", "o")) };
            cv.boxed(r, st, false, i > idx);
            cv.spans(x + 1, y, &[Span::raw(" "), Span::styled(format!("{} ", theme::role_glyph(&role.glyph)), theme::fg(col)), Span::styled(format!("{} ", s.role), if i < idx { theme::muted() } else { theme::dim() })], w - 2);
            cv.put(x + w - 3, y, mark, st);
            cv.put(x + 2, y + 1, &trunc(&s.task, (w as usize).saturating_sub(4)), theme::faint());
            y += 3;
        }
        if i + 1 < steps.len() {
            cv.ch(x + w / 2, y, theme::g("│", "|"), if i < idx { theme::fg(theme::GREEN) } else { theme::faint() });
            y += 1;
        }
    }
    // the manager (if the pattern has one) keeps watching through the finale
    let ax = x + w + 4;
    let aw = (area.x as i32 + area.width as i32 - ax - 1).min(34);
    let mut ay = area.y as i32;
    if aw >= 20 {
        if let Some(m) = run.manager {
            let ma = app.agents.get(&m);
            let mcol = role_col(run, &run.pattern.flow.manager);
            let mglyph = run.pattern.role(&run.pattern.flow.manager).map(|r| r.glyph.clone()).unwrap_or_else(|| "◈".into());
            let r = Rect { x: ax as u16, y: ay as u16, width: aw as u16, height: 3 };
            agent_card(cv, r, run, ma, "manager", &mglyph, mcol, vec![], Some(m) == sel);
            ay += 4;
        }
    }
    // ad-hoc fix workers spawned by the verifier
    let adhoc: Vec<&Worker> = run.workers.iter().filter(|w| w.adhoc).collect();
    if !adhoc.is_empty() {
        if aw >= 20 {
            ay += 1;
            cv.put(ax, ay, "ad-hoc fixes", theme::dim());
            ay += 1;
            for wk in adhoc {
                let a = wk.agent.and_then(|a| app.agents.get(&a));
                let r = Rect { x: ax as u16, y: ay as u16, width: aw as u16, height: 4 };
                worker_card(cv, r, run, &wk.task, Some(wk), a, wk.agent.is_some() && wk.agent == sel, app);
                ay += 5;
            }
        }
    }
}

fn done(cv: &mut Cv, area: Rect, app: &App, run: &Run) {
    let x = area.x as i32 + 3;
    let mut y = area.y as i32 + 1;
    match &run.stage {
        Stage::Failed(why) => {
            cv.put(x, y, &format!("{} run stopped", theme::g("✗", "x")), theme::bold(theme::fg(theme::RED)));
            y += 1;
            cv.put(x, y, &trunc(why, (area.width as usize).saturating_sub(6)), theme::fg(theme::RED));
            y += 2;
        }
        _ => {
            let title = format!("{} run complete in {}", theme::g("✦", "*"), fmt_dur(run.elapsed()));
            let fresh = run.pulse.back().map(|p| p.at.elapsed().as_millis() < 3000).unwrap_or(false);
            if fresh {
                cv.spans(x, y, &anim::shimmer(&title, theme::SAFFRON, theme::TEXT), area.width as i32 - 6);
            } else {
                cv.put(x, y, &title, theme::bold(theme::accent()));
            }
            y += 2;
        }
    }
    for (i, p) in run.history.iter().enumerate() {
        let glyphs: String = p.workers.iter().map(|w| theme::role_glyph(&w.2)).collect::<Vec<_>>().join(" ");
        cv.put(x, y, &format!("{} phase {} · {:<20} {:>7}   {}", theme::g("✓", "v"), i + 1, trunc(&p.name, 20), fmt_dur(p.duration), glyphs), theme::fg(theme::GREEN));
        y += 1;
    }
    y += 1;
    let tokens = run.total_tokens(|a| app.agents.get(&a).map(|x| x.tokens_total).unwrap_or(0));
    cv.put(x, y, &format!("Σ {} tokens across all agents", fmt_tokens(tokens)), theme::muted());
    y += 1;
    if let (Some(ws), Stage::Done) = (&run.ws, &run.stage) {
        if ws.worktree {
            cv.put(x, y, &format!("branch {} is ready — /land merges it into {}", ws.branch, ws.base_branch), theme::text());
        } else {
            cv.put(x, y, "changes are in your project directory (shared mode)", theme::text());
        }
        y += 1;
    }
    cv.put(x, y, &format!("run log: {}", home_rel(&run.dir)), theme::faint());
    y += 2;
    cv.put(x, y, "type a new goal below to start another run", theme::dim());
}

fn draw_pulse(f: &mut Frame, area: Rect, app: &App) {
    let Some(run) = &app.run else { return };
    let blk = Block::default().borders(Borders::LEFT).border_style(theme::faint()).title(Span::styled(" pulse ", theme::bold(theme::muted())));
    let inner = blk.inner(area);
    f.render_widget(blk, area);
    let h = inner.height as usize;
    let total = run.pulse.len();
    let scroll = app.pulse_scroll.min(total.saturating_sub(h));
    let end = total - scroll;
    let start = end.saturating_sub(h);
    let full_t = inner.width >= 40;
    let w = inner.width.saturating_sub(if full_t { 15 } else { 12 }) as usize;
    let lines: Vec<Line> = run
        .pulse
        .iter()
        .skip(start)
        .take(end - start)
        .map(|p| {
            Line::from(vec![
                Span::styled(format!(" {} ", if full_t { p.t.as_str() } else { p.t.get(3..).unwrap_or(&p.t) }), theme::faint()),
                Span::styled(format!("{} ", theme::role_glyph(&p.glyph)), if anim::fade(Some(p.at), 1500) > 0.0 { theme::bold(theme::fg(theme::named(p.color))) } else { theme::fg(theme::named(p.color)) }),
                Span::styled(trunc(&p.text, w), Style::default().fg(theme::mix(theme::MUTED, theme::TEXT, anim::fade(Some(p.at), 1500)))),
            ])
        })
        .collect();
    f.render_widget(Paragraph::new(lines), inner);
}

fn draw_peek(f: &mut Frame, area: Rect, app: &App) {
    let nodes = app.stage_nodes();
    let Some(a) = nodes.get(app.sel).and_then(|id| app.agents.get(id)) else { return };
    let col = theme::named(&a.color);
    let st = match &a.status {
        Status::Busy => format!("running {}", a.turn_started.map(|t| fmt_dur(t.elapsed())).unwrap_or_default()),
        Status::Retrying(m) => format!("retrying: {}", trunc(m, 30)),
        Status::Failed(m) => format!("failed: {}", trunc(m, 30)),
        Status::Crashed(m) => format!("crashed: {}", trunc(m, 30)),
        s => format!("{s:?}").to_lowercase(),
    };
    let title = Line::from(vec![
        Span::styled(format!(" {} {} ", theme::role_glyph(&a.glyph), a.name), theme::bold(theme::fg(col))),
        Span::styled(format!("{} · {}·{} · {} · {} tok ", a.role, a.model_alias, a.effort, st, fmt_tokens(a.tokens_total)), theme::dim()),
        Span::styled("⏎ zoom ", theme::faint()),
    ]);
    let blk = Block::default().borders(Borders::TOP).border_style(theme::faint()).title(title);
    let inner = blk.inner(area);
    f.render_widget(blk, area);
    let lines: Vec<Line> = a.tail_lines(inner.height as usize).into_iter().map(|l| Line::from(Span::styled(format!("  {}", trunc(&l, (inner.width as usize).saturating_sub(3))), theme::dim()))).collect();
    f.render_widget(Paragraph::new(lines), inner);
}
