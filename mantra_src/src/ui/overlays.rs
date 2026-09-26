//! Modal overlays.

use super::{theme, *};
use crate::app::{App, EditTarget, Overlay, Screen};
use crate::engine::run::Stage;
use crate::ui::input::Act;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

pub fn draw(f: &mut Frame, app: &mut App) {
    let n = app.overlays.len();
    for i in 0..n {
        draw_one(f, app, i);
    }
}

fn draw_one(f: &mut Frame, app: &App, i: usize) {
    let area = f.area();
    match &app.overlays[i] {
        Overlay::Help => help(f, area),
        Overlay::ModelPicker { sel, target } => model_picker(f, area, app, *sel, *target),
        Overlay::Diff { agent, file, scroll } => diff(f, area, app, *agent, *file, *scroll),
        Overlay::Inbox { sel } => inbox(f, area, app, *sel),
        Overlay::Plan { scroll } => plan(f, area, app, *scroll),
        Overlay::Edit { title, input, target } => edit(f, area, title, input, target),
        Overlay::Patterns { sel, list } => patterns(f, area, app, *sel, list),
        Overlay::Discover(st) => discover(f, area, st),
        Overlay::Runs { sel, list, confirm, others } => runs(f, area, app, *sel, list, *confirm, *others),
        Overlay::Web => web(f, area, app),
        Overlay::Remote => remote(f, area, app),
    }
}

/// Wrapped lines of `text` in `width` columns, each styled `st` (the link is ~110 chars).
fn wrapped(text: &str, width: usize, indent: &str, st: ratatui::style::Style) -> Vec<Line<'static>> {
    let chars: Vec<char> = text.chars().collect();
    let w = width.saturating_sub(indent.chars().count()).max(8);
    chars.chunks(w).map(|c| Line::from(vec![Span::raw(indent.to_string()), Span::styled(c.iter().collect::<String>(), st)])).collect()
}

/// `/web`: where the web UI is and how it is protected.
fn web(f: &mut Frame, area: Rect, app: &App) {
    let w = 84u16.min(area.width.saturating_sub(2));
    let inner = w.saturating_sub(4) as usize;
    let k = |a: &str| Span::styled(format!("  {a:<12}"), theme::bold(theme::accent()));
    let mut l: Vec<Line> = vec![];
    match app.web.as_ref().map(|x| &x.info).filter(|i| i.listen.is_some()) {
        None => {
            l.push(Line::from(Span::styled("  The web UI is off.", theme::text())));
            l.push(Line::default());
            l.push(Line::from(Span::styled("  mantra --web                                   this machine (http://127.0.0.1:7777)", theme::muted())));
            l.push(Line::from(Span::styled("  mantra --web 0.0.0.0:7777 --web-tls            your phone on the LAN (needs a password:", theme::muted())));
            l.push(Line::from(Span::styled("                                                 MANTRA_WEB_PASSWORD or --web-password)", theme::muted())));
            l.push(Line::from(Span::styled("  mantra --remote                                from anywhere, end-to-end encrypted", theme::muted())));
        }
        Some(i) => {
            for (n, u) in i.urls.iter().enumerate() {
                l.push(Line::from(vec![k(if n == 0 { "open" } else { "" }), Span::styled(u.clone(), theme::bold(theme::fg(theme::CYAN)))]));
            }
            let tls = if i.self_signed {
                format!("on — Mantra's own certificate; install its CA on each device from {}/cert.pem", i.urls.first().cloned().unwrap_or_default())
            } else if i.tls {
                "on — your certificate".to_string()
            } else {
                "off — phones can't install the app or get notifications over http (--web-tls)".to_string()
            };
            let mut first = true;
            for line in wrapped(&tls, inner.saturating_sub(14), "", if i.tls { theme::fg(theme::GREEN) } else { theme::fg(theme::AMBER) }) {
                let mut spans = vec![k(if first { "https" } else { "" })];
                spans.extend(line.spans);
                l.push(Line::from(spans));
                first = false;
            }
            let pw = if i.password { "on".to_string() } else { "off — localhost only".to_string() };
            l.push(Line::from(vec![k("password"), Span::styled(pw, if i.password { theme::fg(theme::GREEN) } else { theme::muted() })]));
            let subs = app.web.as_ref().and_then(|x| x.push.as_ref()).filter(|_| i.push).map(|p| p.store.lock().map(|s| s.list().len()).unwrap_or(0));
            let push = match subs {
                Some(n) => format!("on · {n} device{} subscribed", if n == 1 { "" } else { "s" }),
                None => "off".into(),
            };
            l.push(Line::from(vec![k("push"), Span::styled(push, theme::muted())]));
            let n = app.web.as_ref().map(|x| x.reg.count(crate::web::ConnKind::Local)).unwrap_or(0);
            l.push(Line::from(vec![k("browsers"), Span::styled(format!("{n} connected"), theme::muted())]));
        }
    }
    l.push(Line::default());
    l.push(Line::from(Span::styled("  /remote for the relay link · esc", theme::faint())));
    let r = centered(area, w, l.len() as u16 + 2);
    f.render_widget(Clear, r);
    f.render_widget(Paragraph::new(l).block(block("web", theme::TEAL)), r);
}

/// `/remote`: link, code, password and a scannable QR code; `r` rotates the identity.
fn remote(f: &mut Frame, area: Rect, app: &App) {
    let Some(rem) = app.web.as_ref().and_then(|x| x.remote.as_ref()) else {
        let l = vec![
            Line::from(Span::styled("  Remote access is off.", theme::text())),
            Line::default(),
            Line::from(Span::styled("  mantra --remote      reach this session from anywhere through remote.mantra.codes;", theme::muted())),
            Line::from(Span::styled("                       end-to-end encrypted — the relay only ever sees ciphertext", theme::muted())),
            Line::default(),
            Line::from(Span::styled("  esc", theme::faint())),
        ];
        let r = centered(area, 90, l.len() as u16 + 2);
        f.render_widget(Clear, r);
        f.render_widget(Paragraph::new(l).block(block("remote", theme::VIOLET)), r);
        return;
    };
    let info = rem.info();
    let qr: Option<Vec<String>> = info.qr.as_ref().filter(|_| !theme::ascii()).map(|rows| {
        let m: Vec<Vec<bool>> = rows.iter().map(|r| r.chars().map(|c| c == '1').collect()).collect();
        crate::web::qr::half_blocks(&m, 2)
    });
    let qr_w = qr.as_ref().and_then(|q| q.first()).map(|r| r.chars().count() as u16).unwrap_or(0);
    let w = 86u16.max(qr_w + 6).min(area.width.saturating_sub(2));
    let inner = w.saturating_sub(4) as usize;
    let k = |a: &str| Span::styled(format!("  {a:<10}"), theme::bold(theme::accent()));
    let mut l: Vec<Line> = vec![];
    let (state, col) = match (info.connected, &info.last_error) {
        (true, _) => (format!("connected · {} client{}", info.clients, if info.clients == 1 { "" } else { "s" }), theme::GREEN),
        (false, Some(e)) => (format!("reconnecting — {e}"), theme::AMBER),
        (false, None) => ("connecting…".to_string(), theme::AMBER),
    };
    let relay_txt = format!("{}  ", info.relay);
    // The relay address and label are fixed-ish; the status (an error message) is the part
    // that can run long, so it alone gets truncated to whatever room is left on the line.
    let state_budget = inner.saturating_sub(12 + crate::util::width(&relay_txt)).max(8);
    l.push(Line::from(vec![k("relay"), Span::styled(relay_txt, theme::muted()), Span::styled(crate::util::trunc(&state, state_budget), theme::fg(col))]));
    match &info.link {
        Some(link) => {
            let mut first = true;
            for line in wrapped(link, inner.saturating_sub(12), "", theme::bold(theme::fg(theme::CYAN))) {
                let mut spans = vec![k(if first { "link" } else { "" })];
                spans.extend(line.spans);
                l.push(Line::from(spans));
                first = false;
            }
        }
        None => l.push(Line::from(vec![k("link"), Span::styled("deriving the key…", theme::dim())])),
    }
    l.push(Line::from(vec![k("code"), Span::styled(info.code.clone().unwrap_or_default(), theme::bold(theme::text())), Span::styled("  + the password, at remote.mantra.codes", theme::faint())]));
    l.push(Line::from(vec![k("password"), Span::styled(info.password.clone().unwrap_or_default(), theme::bold(theme::fg(theme::SAFFRON)))]));
    // The QR only when it fits whole: a clipped code doesn't scan.
    let room = area.height.saturating_sub(l.len() as u16 + 6);
    if let Some(q) = qr.filter(|q| q.len() as u16 <= room && qr_w + 4 <= w) {
        l.push(Line::default());
        let pad = " ".repeat(((inner as u16).saturating_sub(qr_w) / 2) as usize);
        let st = ratatui::style::Style::default().fg(ratatui::style::Color::White).bg(ratatui::style::Color::Black);
        for row in q {
            l.push(Line::from(vec![Span::raw(pad.clone()), Span::styled(row, st)]));
        }
    } else if info.link.is_some() {
        l.push(Line::from(Span::styled("  (enlarge the terminal to show the QR code)", theme::faint())));
    }
    l.push(Line::default());
    l.push(Line::from(Span::styled("  the link — or the code + password — opens this session: share it like a key", theme::faint())));
    l.push(Line::from(Span::styled("  r new link (the old one stops working) · esc", theme::faint())));
    let r = centered(area, w, l.len() as u16 + 2);
    f.render_widget(Clear, r);
    f.render_widget(Paragraph::new(l).block(block("remote", theme::VIOLET)), r);
}

fn help(f: &mut Frame, area: Rect) {
    let r = centered(area, 86, 32);
    f.render_widget(Clear, r);
    let k = |a: &str, b: &str| Line::from(vec![Span::styled(format!("  {a:<16}"), theme::bold(theme::accent())), Span::styled(b.to_string(), theme::text())]);
    let h = |t: &str| Line::from(Span::styled(format!(" {t}"), theme::bold(theme::fg(theme::VIOLET))));
    let mut l = vec![
        h("everywhere"),
        k("ctrl+o", "switch Solo ⇄ Mandala"),
        k("ctrl+k", "model picker (←→ effort)"),
        k("alt+↑ / alt+↓", "raise / lower reasoning effort (shift+↑↓ works too)"),
        k("ctrl+d", "diff viewer for the focused agent"),
        k("ctrl+e", "verbose log (full reasoning, output, diffs)"),
        k("ctrl+g", "inbox: approvals & alerts from all agents"),
        k("ctrl+t · F2", "toggle side panel / pulse feed"),
        k("ctrl+l", "redraw the screen"),
        k("/web · /remote", "web UI address & security · remote link, code, password, QR"),
        k("ctrl+c", "close overlay / clear input / interrupt the turn · three presses quit"),
        k("pgup / pgdn", "scroll (mouse wheel too)"),
        k("/", "commands (tab completes) · ! runs a shell command"),
        Line::default(),
        h("solo"),
        k("⏎ / alt+⏎", "send / newline (or end a line with \\)"),
        k("esc", "clear the input (never interrupts)"),
        k("shift+tab", "cycle approvals: untrusted → on-request → never"),
        Line::default(),
        h("busy-agent chat (solo, zoom, @name)"),
        k("⏎", "queue a message behind the current turn (shown as a chip above the input)"),
        k("ctrl+f", "force-send: deliver the queue + input into the running turn right now"),
        k("backspace", "on an empty input, pops the last queued message back in to edit"),
        k("ctrl+x", "on an empty input, discards the whole queue"),
        Line::default(),
        h("mandala stage"),
        k("tab", "toggle between typing and navigating the canvas"),
        k("←→↑↓ · alt+←→ · 1-9", "select an agent · jump straight to the nth and zoom in"),
        k("⏎", "zoom into the selected agent (esc to come back to the overview)"),
        k("space", "pause / resume the whole run"),
        k("r · x · c · +/-", "retry (or restart crashed) · interrupt · compact · effort"),
        k("m", "switch model for the selected agent (fixes a halted ProviderRejected)"),
        k("p · a", "plan · approve plan (during review)"),
        k("@name msg", "message one agent directly; plain text re-prompts the planner"),
        k("s", "open the pattern studio"),
        k("/runs", "this project's runs: ⏎ resume where it stopped · D delete (branches, worktrees, journal)"),
    ];
    l.push(Line::default());
    l.push(Line::from(Span::styled("  esc to close", theme::faint())));
    f.render_widget(Paragraph::new(l).block(block("keys", theme::SAFFRON)), r);
}

/// Which runtime a provider's models run through (WP10): `codex app-server` or `claude -p`.
fn backend_glyph(reg: &crate::config::Registry, provider: &str) -> &'static str {
    match reg.providers.iter().find(|p| p.id == provider).map(|p| p.kind) {
        Some(crate::config::ProviderKind::ClaudeCode) => "✧ claude",
        _ => "◌ codex",
    }
}

fn model_picker(f: &mut Frame, area: Rect, app: &App, sel: usize, target: Option<crate::hub::AgentId>) {
    let rows = app.registry.models.len() as u16;
    // Wide enough for every column (alias … note … "also via"); `centered` clamps it down on
    // narrower terminals, where the trailing note/duplicate columns simply get cut.
    let r = centered(area, 132, rows + 7);
    f.render_widget(Clear, r);
    let cur = target.and_then(|a| app.agents.get(&a));
    let mut l = vec![Line::from(Span::styled(format!("  for: {}", cur.map(|a| a.name.clone()).unwrap_or_else(|| "new sessions".into())), theme::dim())), Line::default()];
    for (i, m) in app.registry.models.iter().enumerate() {
        let is_cur = cur.map(|a| a.model_alias == m.alias).unwrap_or(false);
        let eff = if m.efforts().is_empty() { "—".to_string() } else if is_cur { cur.map(|a| a.effort.clone()).unwrap_or_default() } else { m.default_effort.clone() };
        let st = if i == sel { Style::default().fg(theme::c(theme::SAFFRON)).add_modifier(Modifier::BOLD) } else { theme::text() };
        let ctx = m.context_window.map(|c| format!("{}k ctx", c / 1000)).unwrap_or_else(|| "default ctx".into());
        let via = format!("via {} {}", app.registry.provider_name(&m.provider), backend_glyph(&app.registry, &m.provider));
        // Two aliases can point at the same model id through different providers — the provider
        // column above already distinguishes them; call it out too so it isn't missed.
        let also_via: Vec<String> = app.registry.models.iter().filter(|o| o.model == m.model && o.provider != m.provider).map(|o| app.registry.provider_name(&o.provider)).collect();
        let dup = if also_via.is_empty() { String::new() } else { format!(" (also via {})", also_via.join(", ")) };
        l.push(Line::from(vec![
            Span::styled(format!(" {} ", if i == sel { theme::g("▶", ">") } else { " " }), st),
            // width + 1: a 10-char alias must not touch the model id next to it
            Span::styled(format!("{:<11}", trunc(&m.alias, 10)), st),
            Span::styled(format!("{:<18}", trunc(&m.model, 17)), theme::muted()),
            Span::styled(format!(" {:<27}", trunc(&via, 26)), theme::dim()),
            Span::styled(format!("{:<7}", eff), theme::fg(effort_color(&eff))),
            Span::styled(format!("{:<8}", theme::effort_bar(&eff, &m.efforts())), theme::fg(effort_color(&eff))),
            Span::styled(format!(" {:<12}", ctx), theme::dim()),
            Span::styled(if is_cur { format!(" {} current", theme::g("●", "*")) } else { format!(" {}", trunc(&m.note, 18)) }, if is_cur { theme::fg(theme::GREEN) } else { theme::faint() }),
            Span::styled(trunc(&dup, 26), theme::faint()),
        ]));
    }
    l.push(Line::default());
    l.push(Line::from(Span::styled("  ↑↓ model · +/- ctx · c edit ctx · ←→ effort (current model) · ⏎ switch · e edit models · esc", theme::faint())));
    f.render_widget(Paragraph::new(l).block(block("model & effort", theme::SAFFRON)), r);
}

fn diff(f: &mut Frame, area: Rect, app: &App, agent: crate::hub::AgentId, file: usize, scroll: usize) {
    let r = Rect { x: area.x + 2, y: area.y + 1, width: area.width.saturating_sub(4), height: area.height.saturating_sub(2) };
    f.render_widget(Clear, r);
    let Some(a) = app.agents.get(&agent) else { return };
    let blk = block(&format!("changes · {}", a.name), theme::AMBER);
    let inner = blk.inner(r);
    f.render_widget(blk, r);
    if a.files.is_empty() {
        f.render_widget(Paragraph::new(Span::styled("  no changes yet", theme::dim())), inner);
        return;
    }
    let list_w = 34.min(inner.width / 3);
    let files: Vec<(&String, &crate::agent::FileStat)> = a.files.iter().collect();
    let file = file.min(files.len() - 1);
    let fl: Vec<Line> = files
        .iter()
        .enumerate()
        .map(|(i, (p, s))| {
            let st = if i == file { Style::default().fg(theme::c(theme::SAFFRON)).add_modifier(Modifier::BOLD) } else { theme::text() };
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
    for l in stat.diff.lines().skip(scroll) {
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
}

fn inbox(f: &mut Frame, area: Rect, app: &App, sel: usize) {
    let alerts: Vec<String> = app.run.as_ref().map(|r| r.alerts.clone()).unwrap_or_default();
    let r = centered(area, 90, (app.approvals.len() as u16 * 3 + alerts.len() as u16 + 8).min(area.height.saturating_sub(2)));
    f.render_widget(Clear, r);
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
            l.push(Line::from(Span::styled(format!("  {} {}", theme::g("⚑", "!"), trunc(a, (r.width as usize).saturating_sub(8))), theme::fg(theme::AMBER))));
        }
    }
    l.push(Line::default());
    l.push(Line::from(Span::styled("  ↑↓ select · y approve · a approve for session · n decline · ⏎ zoom to agent · esc", theme::faint())));
    f.render_widget(Paragraph::new(l).block(block("inbox", theme::AMBER)), r);
}

fn plan(f: &mut Frame, area: Rect, app: &App, scroll: usize) {
    let r = Rect { x: area.x + 3, y: area.y + 1, width: area.width.saturating_sub(6), height: area.height.saturating_sub(2) };
    f.render_widget(Clear, r);
    let review = app.run.as_ref().map(|x| x.stage == Stage::Review).unwrap_or(false);
    let title = if review { "plan review" } else { "plan" };
    let blk = block(title, theme::SAFFRON);
    let inner = blk.inner(r);
    f.render_widget(blk, r);
    let body = match app.run.as_ref().and_then(|x| x.plan.as_ref()) {
        Some(p) => p.to_markdown(),
        None => "No plan yet — the planner is still working.".into(),
    };
    let lines = md::render(&body, inner.width.saturating_sub(2) as usize, theme::text());
    let h = inner.height.saturating_sub(1) as usize;
    let view: Vec<Line> = lines.into_iter().skip(scroll).take(h).collect();
    f.render_widget(Paragraph::new(view), Rect { x: inner.x + 1, width: inner.width.saturating_sub(1), height: h as u16, ..inner });
    let hint = if review { " a approve & build · f give feedback · ↑↓ scroll · esc " } else { " ↑↓ scroll · esc " };
    f.render_widget(Paragraph::new(Span::styled(hint, theme::bold(theme::accent()))), Rect { x: inner.x + 1, y: inner.y + inner.height.saturating_sub(1), width: inner.width.saturating_sub(1), height: 1 });
}

fn is_multiline(t: &EditTarget) -> bool {
    matches!(t, EditTarget::RoleField(_, f) if f == "instructions" || f == "description") || matches!(t, EditTarget::FlowStep(_, f) if f == "task")
}

fn edit(f: &mut Frame, area: Rect, title: &str, input: &crate::ui::input::Input, target: &EditTarget) {
    let ml = is_multiline(target);
    let h = if ml { area.height.saturating_sub(6).min(24) } else { 5 };
    let r = centered(area, 90, h);
    f.render_widget(Clear, r);
    let blk = block(title, theme::VIOLET);
    let inner = blk.inner(r);
    f.render_widget(blk, r);
    let (rows, cr, cc) = input.layout(inner.width.saturating_sub(2) as usize);
    let vh = inner.height.saturating_sub(1) as usize;
    let start = if cr >= vh { cr + 1 - vh } else { 0 };
    let lines: Vec<Line> = rows.iter().skip(start).take(vh).map(|s| Line::from(Span::styled(format!(" {s}"), theme::text()))).collect();
    f.render_widget(Paragraph::new(lines), inner);
    f.set_cursor_position((inner.x + 1 + cc as u16, inner.y + (cr - start) as u16));
    let hint = if ml { " ⏎ newline · ctrl+s apply · esc cancel " } else { " ⏎ apply · esc cancel " };
    f.render_widget(Paragraph::new(Span::styled(hint, theme::faint())), Rect { x: inner.x, y: inner.y + inner.height.saturating_sub(1), width: inner.width, height: 1 });
}

fn discover(f: &mut Frame, area: Rect, st: &crate::app::DiscoverState) {
    use crate::discover::CState;
    let r = Rect { x: area.x + 2, y: area.y + 1, width: area.width.saturating_sub(4), height: area.height.saturating_sub(2) };
    f.render_widget(Clear, r);
    let blk = block("discover models", theme::SAFFRON);
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
    let mw = w.saturating_sub(62).max(16);
    l.push(Line::from(Span::styled(format!("      {:<10} {:<mw$} {:<16} {:<14} {}", "provider", "model", "context", "effort", "status"), theme::bold(theme::muted()))));
    let head = l.len();
    let room = (inner.height as usize).saturating_sub(head + 2).max(1);
    let sel = st.sel.min(vis.len().saturating_sub(1));
    let start = sel.saturating_sub(room.saturating_sub(1));
    for (row, &i) in vis.iter().enumerate().skip(start).take(room) {
        let c = &st.items[i];
        let is = row == sel;
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
        let eff = if c.efforts.is_empty() { "—".to_string() } else { format!("{}…{}", c.efforts.first().unwrap(), c.efforts.last().unwrap()) };
        let (status, scol) = match c.state {
            CState::New => ("new", theme::GREEN),
            CState::Update => ("fill missing", theme::AMBER),
            CState::Configured => ("configured", theme::FAINT),
        };
        let base = if c.state == CState::Configured { theme::dim() } else { theme::text() };
        let st_row = if is { Style::default().fg(theme::c(theme::SAFFRON)).add_modifier(Modifier::BOLD) } else { base };
        l.push(Line::from(vec![
            Span::styled(format!("{}{} ", if is { theme::g("▶", ">") } else { " " }, mark), st_row),
            Span::styled(format!("{:<10} ", trunc(&c.provider, 10)), if is { st_row } else { theme::muted() }),
            Span::styled(format!("{:<mw$} ", trunc(&c.model, mw)), st_row),
            Span::styled(format!("{:<16} ", ctx), if c.context_known { theme::muted() } else { theme::fg(theme::AMBER) }),
            Span::styled(format!("{:<14} ", trunc(&eff, 14)), theme::muted()),
            Span::styled(status, theme::fg(scol)),
        ]));
    }
    if vis.is_empty() && st.loading.is_empty() {
        l.push(Line::from(Span::styled(if st.items.is_empty() { "  no models found" } else { "  nothing matches the filter" }, theme::dim())));
    }
    f.render_widget(Paragraph::new(l), inner);
    let foot = vec![
        Line::from(Span::styled(" unknown context → assumed 200k (edit in /models) · effort only where the provider reports reasoning support", theme::faint())),
        Line::from(Span::styled(" type to filter · ↑↓ move · space select · tab all/none · ⏎ add selected & save · esc close", theme::bold(theme::accent()))),
    ];
    f.render_widget(Paragraph::new(foot), Rect { y: inner.y + inner.height.saturating_sub(2), height: 2.min(inner.height), ..inner });
}

fn patterns(f: &mut Frame, area: Rect, app: &App, sel: usize, list: &[String]) {
    let r = centered(area, 60, list.len() as u16 + 5);
    f.render_widget(Clear, r);
    let mut l: Vec<Line> = list
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let st = if i == sel { theme::bold(theme::accent()) } else { theme::text() };
            Line::from(vec![Span::styled(format!(" {} {p}", if i == sel { theme::g("▶", ">") } else { " " }), st), Span::styled(if *p == app.pattern_name { "  (active)".to_string() } else { String::new() }, theme::fg(theme::GREEN))])
        })
        .collect();
    l.push(Line::default());
    l.push(Line::from(Span::styled("  ⏎ use for new runs · e edit in studio · esc", theme::faint())));
    f.render_widget(Paragraph::new(l).block(block("patterns", theme::VIOLET)), r);
}

fn runs(f: &mut Frame, area: Rect, app: &App, sel: usize, list: &[crate::engine::state::RunSummary], confirm: bool, others: usize) {
    use crate::engine::state::fmt_ago;
    let r = centered(area, 100, (list.len() as u16).max(1) + 5);
    f.render_widget(Clear, r);
    let inner_w = r.width.saturating_sub(4) as usize;
    let brief_w = inner_w.saturating_sub(2 + 22 + 1 + 18 + 1 + 9 + 2).max(8);
    let mut l: Vec<Line> = vec![Line::from(Span::styled(format!("   {:<22} {:<18} {:>9}  {}", "run", "stage", "updated", "goal"), theme::faint()))];
    if list.is_empty() {
        l.push(Line::from(Span::styled("   no runs for this project yet — ctrl+o starts one", theme::dim())));
    }
    for (i, run) in list.iter().enumerate() {
        let cur = app.run.as_ref().map(|x| x.id == run.id).unwrap_or(false);
        let unfinished = run.unfinished();
        let st = if i == sel { theme::bold(theme::accent()) } else if unfinished { theme::text() } else { theme::muted() };
        let stage_style = if run.state.is_err() {
            theme::fg(theme::ROSE)
        } else if unfinished {
            theme::fg(theme::AMBER)
        } else {
            theme::fg(theme::GREEN)
        };
        l.push(Line::from(vec![
            Span::styled(format!(" {} {:<22}", if i == sel { theme::g("▶", ">") } else { " " }, trunc(&run.id, 22)), st),
            Span::styled(format!(" {:<18}", trunc(&run.stage_label(), 18)), stage_style),
            Span::styled(format!(" {:>9}", fmt_ago(run.updated_unix)), theme::faint()),
            Span::styled(format!("  {}", trunc(&run.brief(), brief_w)), theme::dim()),
            Span::styled(if cur { "  (open)".to_string() } else { String::new() }, theme::fg(theme::GREEN)),
        ]));
    }
    l.push(Line::default());
    if confirm {
        let id = list.get(sel).map(|r| r.id.clone()).unwrap_or_default();
        l.push(Line::from(Span::styled(format!("  delete {id} — its branches, worktrees and journal? y / n"), theme::bold(theme::fg(theme::ROSE)))));
    } else {
        let mut hint = "  ⏎ resume where it stopped · D delete · esc".to_string();
        if others > 0 {
            hint.push_str(&format!("   ({others} run{} in other projects: mantra runs)", if others == 1 { "" } else { "s" }));
        }
        l.push(Line::from(Span::styled(hint, theme::faint())));
    }
    f.render_widget(Paragraph::new(l).block(block("runs", theme::VIOLET)), r);
}

// ───────────────────────────── keys ─────────────────────────────

pub fn key(app: &mut App, k: KeyEvent) {
    let Some(top) = app.overlays.pop() else { return };
    let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
    let keep = match top {
        Overlay::Help => None,
        Overlay::Web => match k.code {
            KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') => None,
            _ => Some(Overlay::Web),
        },
        Overlay::Remote => match k.code {
            KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') => None,
            KeyCode::Char('r') if !ctrl => {
                match app.web.as_ref().and_then(|w| w.remote.clone()) {
                    Some(r) => {
                        r.rotate();
                        app.toast("new remote link — the old link, code and QR no longer work", crate::agent::Level::Ok);
                    }
                    None => app.toast("remote access is off (mantra --remote)", crate::agent::Level::Info),
                }
                Some(Overlay::Remote)
            }
            _ => Some(Overlay::Remote),
        },
        Overlay::ModelPicker { mut sel, target } => {
            let n = app.registry.models.len().max(1);
            match k.code {
                KeyCode::Esc => None,
                KeyCode::Up => {
                    sel = sel.saturating_sub(1);
                    Some(Overlay::ModelPicker { sel, target })
                }
                KeyCode::Down => {
                    sel = (sel + 1).min(n - 1);
                    Some(Overlay::ModelPicker { sel, target })
                }
                KeyCode::Left | KeyCode::Right => {
                    if let Some(a) = target {
                        let alias = app.registry.models.get(sel).map(|m| m.alias.clone()).unwrap_or_default();
                        if app.agents.get(&a).map(|x| x.model_alias != alias).unwrap_or(false) {
                            if let Err(e) = app.set_model(a, &alias) {
                                app.toast(e, crate::agent::Level::Warn);
                            }
                        }
                        app.step_effort(a, if k.code == KeyCode::Right { 1 } else { -1 });
                    }
                    Some(Overlay::ModelPicker { sel, target })
                }
                KeyCode::Enter => {
                    let alias = app.registry.models.get(sel).map(|m| m.alias.clone()).unwrap_or_default();
                    match target {
                        Some(a) => {
                            if let Err(e) = app.set_model(a, &alias) {
                                app.toast(e, crate::agent::Level::Warn);
                            }
                            app.model_switched_for_run_agent(a);
                        }
                        None => {
                            app.settings.default_model = alias;
                            let _ = app.settings.save();
                        }
                    }
                    None
                }
                KeyCode::Char('e') => {
                    app.enter_screen(Screen::Models);
                    app.models_ui.row = sel;
                    None
                }
                KeyCode::Char('+') | KeyCode::Char('-') => {
                    const STEPS: &[u64] = &[16_000, 32_000, 64_000, 128_000, 200_000, 262_000, 272_000, 400_000, 524_000, 1_000_000];
                    let d: i64 = if k.code == KeyCode::Char('+') { 1 } else { -1 };
                    if let Some(m) = app.registry.models.get_mut(sel) {
                        let cur = m.context_window.unwrap_or(0);
                        let i = STEPS.iter().position(|x| *x >= cur).unwrap_or(STEPS.len() - 1) as i64;
                        let j = (i + d).clamp(0, STEPS.len() as i64 - 1) as usize;
                        m.context_window = Some(STEPS[j]);
                        let _ = app.registry.save();
                        app.toast("context window changed — applies to new agents", crate::agent::Level::Info);
                    }
                    Some(Overlay::ModelPicker { sel, target })
                }
                KeyCode::Char('c') => {
                    if let Some(m) = app.registry.models.get(sel) {
                        let mut input = crate::ui::input::Input::default();
                        input.set(&m.context_window.map(|c| c.to_string()).unwrap_or_default());
                        let title = format!("{} · context (accepts 400k / 1m)", m.alias);
                        app.overlays.push(Overlay::ModelPicker { sel, target });
                        app.overlays.push(Overlay::Edit { title, input, target: EditTarget::ModelCell(sel, 3) });
                    }
                    None
                }
                _ => Some(Overlay::ModelPicker { sel, target }),
            }
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
                    app.screen = if Some(a) == app.solo { Screen::Solo } else { Screen::Zoom(a) };
                    None
                }
                _ => Some(Overlay::Inbox { sel }),
            }
        }
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
                        let mut ctx = crate::app::Ctxt { hub: &mut app.hub, agents: &mut app.agents, registry: &app.registry, tx: &app.tx, notes: &mut app.notes };
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
                app.screen = Screen::Stage;
                None
            }
            _ => Some(Overlay::Plan { scroll }),
        },
        Overlay::Edit { title, mut input, target } => {
            let ml = is_multiline(&target);
            if k.code == KeyCode::Esc {
                None
            } else if (ctrl && k.code == KeyCode::Char('s')) || (!ml && k.code == KeyCode::Enter) {
                let text = input.text();
                crate::ui::studio::apply_edit(app, &target, text.trim_end());
                None
            } else {
                if ml && k.code == KeyCode::Enter {
                    input.insert_str("\n");
                } else if let Act::Submit = input.key(k) {
                }
                Some(Overlay::Edit { title, input, target })
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
        Overlay::Patterns { mut sel, list } => match k.code {
            KeyCode::Esc => None,
            KeyCode::Up => {
                sel = sel.saturating_sub(1);
                Some(Overlay::Patterns { sel, list })
            }
            KeyCode::Down => {
                sel = (sel + 1).min(list.len().saturating_sub(1));
                Some(Overlay::Patterns { sel, list })
            }
            KeyCode::Enter => {
                if let Some(p) = list.get(sel) {
                    app.pattern_name = p.clone();
                    app.studio.dirty = false;
                    if let Ok(pt) = crate::engine::pattern::Pattern::load(p, &app.project) {
                        app.studio.pattern = pt;
                    }
                    app.toast(format!("new runs use pattern {p}"), crate::agent::Level::Ok);
                }
                None
            }
            KeyCode::Char('e') => {
                if let Some(p) = list.get(sel) {
                    app.pattern_name = p.clone();
                    app.studio.dirty = false;
                    app.open_studio();
                }
                None
            }
            _ => Some(Overlay::Patterns { sel, list }),
        },
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
