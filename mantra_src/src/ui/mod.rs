//! UI root: screen dispatch + shared widgets.

pub mod anim;
pub mod convo;
pub mod form;
pub mod input;
pub mod md;
pub mod models;
pub mod overlays;
pub mod settings;
pub mod solo;
pub mod stage;
pub mod studio;
pub mod theme;

use crate::agent::{Agent, Level, Status};
use crate::app::{App, Approval, Screen};
use crate::util::{fmt_dur, fmt_tokens, trunc, width as w_of};
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};
use ratatui::Frame;
use std::time::Duration;

pub fn draw(f: &mut Frame, app: &mut App) {
    let area = f.area();
    f.render_widget(Block::default().style(theme::surface()), area);
    if area.width < 40 || area.height < 12 {
        f.render_widget(Paragraph::new(format!("Mantra needs at least 40×12 (now {}×{})", area.width, area.height)), area);
        return;
    }
    match app.screen {
        Screen::Solo => solo::draw(f, app, None),
        Screen::Zoom(a) => solo::draw(f, app, Some(a)),
        Screen::Stage => stage::draw(f, app),
        Screen::Studio => studio::draw_studio(f, app),
        Screen::Models => models::draw_models(f, app),
        Screen::Settings => settings::draw_settings(f, app),
    }
    overlays::draw(f, app);
    draw_toast(f, app);
    if theme::ascii() {
        asciify(f.buffer_mut());
    }
}

/// Final pass for ASCII terminals / non-UTF-8 locales: every cell becomes pure ASCII.
fn asciify(buf: &mut ratatui::buffer::Buffer) {
    let mut wide_tail = false;
    for cell in buf.content.iter_mut() {
        let sym = cell.symbol();
        if wide_tail && sym.is_empty() {
            cell.set_symbol(" ");
            wide_tail = false;
            continue;
        }
        wide_tail = false;
        if sym.is_ascii() {
            continue;
        }
        let c = sym.chars().next().unwrap_or(' ');
        if unicode_width::UnicodeWidthChar::width(c).unwrap_or(1) > 1 {
            wide_tail = true;
        }
        let r = match c {
            '─' | '━' | '┄' | '┈' | '═' | '—' | '–' | '▱' => "-",
            '│' | '┃' | '┆' | '┊' | '║' | '‖' | '⎿' | '▎' => "|",
            '╭' | '╮' | '╰' | '╯' | '┌' | '┐' | '└' | '┘' | '┏' | '┓' | '┗' | '┛' | '┬' | '┴' | '┼' | '├' | '┤' | '╔' | '╗' | '╚' | '╝' => "+",
            '▸' | '▶' | '→' | '⇢' | '›' | '»' => ">",
            '◂' | '←' | '‹' | '«' => "<",
            '▼' | '↓' | '⇣' => "v",
            '▲' | '↑' => "^",
            '▰' | '█' | '■' | '▣' => "#",
            '·' | '…' => ".",
            '•' | '●' | '✦' | '★' | '◈' => "*",
            '◉' | '◎' | '○' | '◇' | '◆' | '◐' => "o",
            '✓' => "v",
            '✗' | '×' => "x",
            '↻' => "r",
            '⚙' => "%",
            '∴' => ":",
            '⚑' | '⚠' => "!",
            '✎' => "~",
            '⎇' => "@",
            '⏱' => "t",
            'Σ' => "S",
            '⌕' => "?",
            '☰' => "=",
            '⏎' => "<",
            '⇧' => "^",
            '⇥' => ">",
            '“' | '”' => "\"",
            '‘' | '’' => "'",
            // the backends' tags in the model pickers (`◌ codex`, `✧ claude`, `• bija`) and the
            // Akasha pulse lines stay three different marks
            '◌' | '⬡' | '⎔' => "o",
            '✧' => "+",
            '⛔' => "!",
            '♦' => "#",
            '≤' => "<",
            '≥' => ">",
            '⇄' => "~",
            '↳' => "-",
            // the rest of the pulse feed's marks
            '⏰' | '⏳' => "t",
            '⚒' => "%",
            'ℹ' => "i",
            '⌫' => "x",
            '⇲' => "v",
            '\u{2800}'..='\u{28FF}' => "*",
            _ => "?",
        };
        cell.set_symbol(r);
    }
}

pub fn border_type() -> BorderType {
    if theme::ascii() {
        BorderType::Plain
    } else {
        BorderType::Rounded
    }
}

pub fn block(title: &str, color: (u8, u8, u8)) -> Block<'static> {
    let b = Block::default().style(theme::surface()).borders(Borders::ALL).border_type(border_type()).border_style(theme::fg(color));
    if title.is_empty() {
        b
    } else {
        b.title(Span::styled(format!(" {title} "), theme::bold(if color == theme::SIGNAL_RED { theme::accent() } else { theme::fg(color) })))
    }
}

pub fn centered(area: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(area.width.saturating_sub(2));
    let h = h.min(area.height.saturating_sub(2));
    Rect { x: area.x + (area.width - w) / 2, y: area.y + (area.height - h) / 2, width: w, height: h }
}

pub fn home_rel(p: &std::path::Path) -> String {
    let s = p.to_string_lossy().to_string();
    match dirs::home_dir() {
        Some(h) => {
            let h = h.to_string_lossy().to_string();
            if s.starts_with(&h) {
                format!("~{}", &s[h.len()..])
            } else {
                s
            }
        }
        None => s,
    }
}

/// One-line header: brand + breadcrumb on the left, `right` spans right-aligned.
pub fn header(f: &mut Frame, area: Rect, crumbs: Vec<Span<'static>>, right: Vec<Span<'static>>) {
    let mut left = vec![Span::styled(format!(" {} mantra", theme::g("✦", "*")), theme::bold(theme::accent()))];
    left.extend(crumbs);
    let width = area.width as usize;
    let mut right = right;
    let sum = |v: &Vec<Span<'static>>| v.iter().map(|s| w_of(&s.content)).sum::<usize>();
    while sum(&left) + sum(&right) + 1 > width && left.len() > 2 {
        left.pop();
    }
    while sum(&left) + sum(&right) + 1 > width && !right.is_empty() {
        right.remove(0);
    }
    let lw = sum(&left);
    let rw = sum(&right);
    let mut spans = left;
    let gap = (area.width as usize).saturating_sub(lw + rw + 1);
    if gap > 0 {
        spans.push(Span::raw(" ".repeat(gap)));
        spans.extend(right);
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// Header crumbs for the web layer: `⌂ web` while the local UI is up, `⇄ remote` green when the
/// relay is connected, amber while (re)connecting.
pub fn web_badges(app: &App) -> Vec<Span<'static>> {
    let Some(w) = &app.web else { return vec![] };
    let mut v = vec![];
    if w.info.listen.is_some() {
        v.push(Span::styled(format!("{} web  ", theme::g("⌂", "~")), theme::fg(theme::TEAL)));
    }
    if let Some(r) = &w.remote {
        let up = r.inner().lock().connected;
        v.push(Span::styled(format!("{} remote  ", theme::g("⇄", "<>")), theme::fg(if up { theme::GREEN } else { theme::AMBER })));
    }
    v
}

/// "sol · high ▰▰▰▱▱ · ctx 23%" chip for an agent.
pub fn model_chip(a: &Agent, app: &App) -> Vec<Span<'static>> {
    let m = app.registry.resolve(&a.model_alias);
    let efforts = m.efforts();
    let mut v = vec![Span::styled(a.model_alias.clone(), theme::bold(theme::text()))];
    if !a.effort.is_empty() && !efforts.is_empty() {
        v.push(Span::styled(" · ", theme::faint()));
        v.push(Span::styled(effort_label(&a.effort), theme::fg(effort_color(&a.effort))));
        v.push(Span::raw(" "));
        v.push(Span::styled(theme::effort_bar(&a.effort, &efforts), theme::fg(effort_color(&a.effort))));
    }
    // Only worth naming the provider once there's more than one to confuse it with (the built-in
    // "openai" is implicit and never shown on its own).
    if !app.registry.providers.is_empty() {
        v.push(Span::styled(" · via ", theme::faint()));
        v.push(Span::styled(app.registry.provider_name(&m.provider), theme::dim()));
    }
    if let Some(p) = a.ctx_percent() {
        let col = if p >= 85 { theme::RED } else if p >= 65 { theme::AMBER } else { theme::MUTED };
        v.push(Span::styled(" · ", theme::faint()));
        v.push(Span::styled(format!("ctx {p}%"), theme::fg(col)));
    }
    if a.tokens_total > 0 {
        v.push(Span::styled(" · ", theme::faint()));
        v.push(Span::styled(format!("{} tok", fmt_tokens(a.tokens_total)), theme::dim()));
        v.extend(cached_suffix(a.cached_share()));
    }
    v.push(Span::raw(" "));
    v
}

/// " · 5.9M cached", muted, after a token count — nothing when the share is not worth a word
/// (`agent::cached_share`). The suffix is quieter than the count it qualifies: the total is
/// the fact, this is the reading of it.
pub fn cached_suffix(share: Option<String>) -> Vec<Span<'static>> {
    share.map(|s| vec![Span::styled(" · ", theme::faint()), Span::styled(s, theme::faint())]).unwrap_or_default()
}

/// A run's token total and how much of it was cached input. The total is `Run::total_tokens`:
/// the agents on the run plus what retired ones (past QA rounds, a respawned planner) left in
/// its carry-over. The cached share is summed over every agent the run ever had —
/// `agent_meta` keeps each one that got a thread, and a retired agent stays in `app.agents`,
/// stopped, with its counts — so it covers the same agents as the total without the engine
/// keeping a second carry-over.
pub fn run_token_totals(app: &App, run: &crate::engine::run::Run) -> (u64, u64) {
    let total = run.total_tokens(|a| app.agents.get(&a).map(|x| x.tokens_total).unwrap_or(0));
    let mut ever: Vec<crate::hub::AgentId> = run.agent_meta.keys().copied().chain(run.all_agents()).collect();
    ever.sort_unstable();
    ever.dedup();
    let cached = ever.into_iter().map(|a| app.agents.get(&a).map(|x| x.tokens_cached).unwrap_or(0)).sum::<u64>();
    (total, cached.min(total))
}

pub fn effort_label(e: &str) -> String {
    e.to_string()
}

pub fn effort_color(e: &str) -> (u8, u8, u8) {
    match e {
        "minimal" | "low" => theme::MUTED,
        "medium" => theme::TEAL,
        "high" => theme::BLUE,
        "xhigh" => theme::VIOLET,
        "max" => theme::SAFFRON,
        "ultra" => theme::ROSE,
        // not a level: Mantra picks one per spawn
        crate::engine::pattern::EFFORT_AUTO => theme::CYAN,
        _ => theme::TEXT,
    }
}

/// How long a toast stays up: longer messages get more reading time.
pub fn toast_life(text: &str) -> u128 {
    (3000 + text.chars().count() as u128 * 35).min(9000)
}

/// Below this a gap is just the model thinking between ticks; above it the user starts wondering
/// whether the process died, which is exactly when the label earns its space.
const QUIET_AFTER: Duration = Duration::from_secs(20);

/// How long an agent that is *supposed* to be producing has been silent. The CLIs tick while a
/// model reasons, so a `last_event` that stops moving is the one honest "is it stuck?" signal we
/// have — shown only past a threshold, so a normal turn never gains noise.
pub(crate) fn quiet_for(a: &Agent) -> Option<Duration> {
    let d = a.last_event.elapsed();
    (producing(a) && d >= QUIET_AFTER).then_some(d)
}

/// A turn can remain active while its tool approval or swarm children block it. Only a turn
/// actually producing work gets motion and a silence warning.
pub(crate) fn producing(a: &Agent) -> bool {
    a.busy() && !matches!(a.status, Status::Waiting | Status::Retrying(_)) && !a.swarm_waiting && !a.stopped_by_user
}

/// Animated "thinking" status line shown under the log while an agent works.
pub fn busy_line(a: &Agent) -> Option<Line<'static>> {
    let elapsed = a.turn_started.map(|t| fmt_dur(t.elapsed())).unwrap_or_default();
    match &a.status {
        Status::Waiting => Some(Line::from(Span::styled(format!("{} waiting for approval · ctrl+g to review", theme::g("⚑", "!")), theme::fg(theme::AMBER)))),
        Status::Retrying(m) => Some(Line::from(vec![
            Span::styled(format!("{} ", theme::g("↻", "r")), theme::fg(theme::AMBER)),
            Span::styled(Status::retrying_text(m, 60), theme::fg(theme::AMBER)),
            Span::styled(format!("  {}", a.retry_note.clone().map(|n| trunc(&n, 60)).unwrap_or_default()), theme::faint()),
        ])),
        Status::Starting => Some(Line::from(vec![Span::styled(format!("{} ", anim::spinner()), theme::dim()), Span::styled("starting agent…", theme::dim())])),
        _ if a.swarm_waiting => Some(Line::from(Span::styled(format!("{} waiting for Akasha swarm", theme::g("◉", "@")), theme::fg(theme::AMBER)))),
        _ if producing(a) || a.compacting => {
            let b = anim::breath(1600);
            let col = theme::named(&a.color);
            let glyph = Span::styled(format!("{} ", theme::g("✦", "*")), Style::default().fg(theme::mix(theme::FAINT, col, 0.35 + 0.65 * b)).add_modifier(Modifier::BOLD));
            let label = match a.activity.as_str() {
                _ if a.compacting => "Compacting context…".to_string(),
                "thinking" => "Thinking…".to_string(),
                "writing" => "Writing…".to_string(),
                "starting turn" => "Starting…".to_string(),
                other => trunc(other, 50),
            };
            let mut spans = vec![glyph];
            spans.extend(anim::shimmer(&label, theme::mix_rgb(col, theme::MUTED), theme::TEXT));
            spans.push(Span::styled(format!("  {elapsed} · {} tok · {} · ctrl+c to interrupt", fmt_tokens(a.tokens_total), a.effort), theme::faint()));
            // Its own span, in amber: the shimmer proves the frame is repainting, not that the
            // agent is still saying anything. This is the part that warrants a second look.
            if let Some(q) = quiet_for(a) {
                spans.push(Span::styled(format!(" · quiet {}", fmt_dur(q)), theme::fg(theme::AMBER)));
            }
            Some(Line::from(spans))
        }
        _ => None,
    }
}

pub fn input_height(app: &App, width: u16) -> u16 {
    let (rows, _, _) = app.input.layout(width.saturating_sub(4) as usize);
    (rows.len() as u16).clamp(1, 8) + 2
}

pub fn draw_input(f: &mut Frame, area: Rect, app: &App, placeholder: &str, color: (u8, u8, u8), focused: bool) {
    let border = if focused { theme::SIGNAL_RED } else { theme::FAINT };
    let blk = Block::default().borders(Borders::ALL).border_type(border_type()).border_style(theme::fg(border))
        .title(if focused { " input · FOCUSED " } else { " input · tab to focus " })
        .title_style(if focused { theme::accent() } else { theme::muted() });
    let inner = blk.inner(area);
    f.render_widget(blk, area);
    let prompt = Span::styled(format!("{} ", theme::g("›", ">")), theme::bold(theme::fg(color)));
    let w = inner.width.saturating_sub(2) as usize;
    let (rows, cr, cc) = app.input.layout(w);
    let h = inner.height as usize;
    if h == 0 || inner.width < 3 {
        return;
    }
    let start = if cr >= h { cr + 1 - h } else { 0 };
    let mut lines = vec![];
    if app.input.is_empty() {
        lines.push(Line::from(vec![prompt, Span::styled(placeholder.to_string(), theme::faint())]));
    } else {
        for (i, r) in rows.iter().enumerate().skip(start).take(h) {
            let p = if i == 0 { prompt.clone() } else { Span::raw("  ") };
            lines.push(Line::from(vec![p, Span::styled(r.clone(), theme::text())]));
        }
    }
    f.render_widget(Paragraph::new(lines), inner);
    if focused {
        f.set_cursor_position((inner.x + 2 + cc as u16, inner.y + (cr - start) as u16));
    }
}

/// Height (0 or 1) of the queued-message chip row for the given agent — 0 collapses the row
/// entirely when nothing is queued.
pub fn queue_chip_height(app: &App, id: Option<crate::hub::AgentId>) -> u16 {
    if id.and_then(|a| app.agents.get(&a)).map(|a| !a.queued.is_empty()).unwrap_or(false) {
        1
    } else {
        0
    }
}

/// "⏳ queued 2 · "…" · ctrl+f send now · backspace on empty input to edit" — shown directly
/// above the input box while the given agent (the focused agent in Solo/Zoom, the selected node
/// on the stage) has messages waiting behind its current turn.
pub fn draw_queue_chip(f: &mut Frame, area: Rect, app: &App, id: Option<crate::hub::AgentId>) {
    if area.height == 0 {
        return;
    }
    let Some(a) = id.and_then(|a| app.agents.get(&a)) else { return };
    if a.queued.is_empty() {
        return;
    }
    let n = a.queued.len();
    let preview = a.queued.last().map(|s| trunc(&s.replace('\n', " "), 40)).unwrap_or_default();
    let line = Line::from(vec![
        Span::styled(format!(" {} queued {n}", theme::g("⏳", "...")), theme::bold(theme::fg(theme::AMBER))),
        Span::styled(format!(" · \"{preview}\""), theme::dim()),
        Span::styled("  ctrl+f send now · backspace on empty input to edit", theme::faint()),
    ]);
    f.render_widget(Paragraph::new(line), area);
}

/// A brief tint across the header right after switching between the overview and a zoomed
/// agent, so the jump never feels silent. Never armed when `reduce_motion` is set (see
/// `App::set_screen`), so this naturally does nothing in that case.
pub fn draw_screen_flash(f: &mut Frame, area: Rect, app: &App, color: (u8, u8, u8)) {
    let k = anim::fade(app.flash_screen, 400);
    if k <= 0.0 {
        return;
    }
    let bg = theme::mix(theme::FAINT, color, k);
    let buf = f.buffer_mut();
    for x in area.x..area.x + area.width {
        if let Some(cell) = buf.cell_mut((x, area.y)) {
            cell.set_bg(bg);
        }
    }
}

/// Slash-command suggestions above the input.
pub fn draw_suggestions(f: &mut Frame, input_area: Rect, app: &App) {
    let s = app.suggestions();
    if s.is_empty() {
        return;
    }
    let h = (s.len() as u16).min(10) + 2;
    let lw = s.iter().map(|x| w_of(&x.label)).max().unwrap_or(0).max(10) + 2;
    let w = (lw as u16 + 50).min(input_area.width);
    let area = Rect { x: input_area.x, y: input_area.y.saturating_sub(h), width: w, height: h };
    f.render_widget(Clear, area);
    let sel = app.suggest.min(s.len() - 1);
    let start = sel.saturating_sub(9);
    let lines: Vec<Line> = s
        .iter()
        .enumerate()
        .skip(start)
        .take(10)
        .map(|(i, x)| {
            let st = if i == sel { theme::bold(theme::accent()) } else { theme::text() };
            Line::from(vec![Span::styled(format!(" {:<lw$}", x.label), st), Span::styled(trunc(&x.about, (w as usize).saturating_sub(lw + 3)), theme::dim())])
        })
        .collect();
    let blk = block("", theme::FAINT);
    let inner = blk.inner(area);
    f.render_widget(Paragraph::new(lines).block(blk), area);
    form::band_line(f.buffer_mut(), inner, sel - start, 0);
}

pub fn approval_height(ap: &Approval) -> u16 {
    (ap.detail.lines().count() as u16).min(4) + 3
}

pub fn draw_approval(f: &mut Frame, area: Rect, ap: &Approval, agent_name: &str) {
    let blk = block(&format!("{} {} · {}", theme::g("⚑", "!"), ap.title, agent_name), theme::AMBER);
    let detail_room = area.height.saturating_sub(3) as usize;
    let mut lines: Vec<Line> = ap.detail.lines().take(detail_room.min(4)).map(|l| Line::from(Span::styled(trunc(l, area.width.saturating_sub(4) as usize), theme::bold(theme::text())))).collect();
    if ap.method == "item/tool/requestUserInput" {
        let hint = if area.width < 55 { "answer below · ⏎ send · esc skip" } else { "type your answer below and press ⏎ · esc to skip" };
        lines.push(Line::from(Span::styled(hint, theme::fg(theme::AMBER))));
    } else if area.width < 60 {
        lines.push(Line::from(Span::styled("y yes · a session · n no · esc cancel", theme::fg(theme::AMBER))));
    } else {
        lines.push(Line::from(vec![
            Span::styled("[y]", theme::bold(theme::fg(theme::GREEN))),
            Span::styled(" yes  ", theme::text()),
            Span::styled("[a]", theme::bold(theme::fg(theme::GREEN))),
            Span::styled(" yes, for this session  ", theme::text()),
            Span::styled("[n]", theme::bold(theme::fg(theme::RED))),
            Span::styled(" no  ", theme::text()),
            Span::styled("[esc]", theme::bold(theme::dim())),
            Span::styled(" cancel turn", theme::dim()),
        ]));
    }
    f.render_widget(Clear, area);
    f.render_widget(Paragraph::new(lines).block(blk), area);
}

pub fn footer(f: &mut Frame, area: Rect, hints: &[(&str, &str)]) {
    let mut spans = vec![Span::raw(" ")];
    let mut used = 1;
    for (i, (k, d)) in hints.iter().enumerate() {
        let w = w_of(k) + w_of(d) + 1 + if i > 0 { 3 } else { 0 };
        if used + w > area.width as usize {
            break; // drop whole hints rather than cutting one mid-word
        }
        used += w;
        if i > 0 {
            spans.push(Span::styled(" · ", theme::faint()));
        }
        spans.push(Span::styled(k.to_string(), theme::muted()));
        spans.push(Span::styled(format!(" {d}"), theme::faint()));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_toast(f: &mut Frame, app: &App) {
    let Some((text, t, level)) = &app.toast else { return };
    let age = t.elapsed().as_millis();
    let life = toast_life(text);
    if age > life {
        return;
    }
    let col = match level {
        Level::Error => theme::RED,
        Level::Warn => theme::AMBER,
        Level::Ok => theme::GREEN,
        Level::Info => theme::VIOLET,
    };
    let area = f.area();
    let msg = trunc(text, (area.width as usize).saturating_sub(10));
    let full = w_of(&msg) as u16 + 4;
    // slide in over 160ms (ease-out), fade over the last 600ms
    let k = if theme::motion() { (age as f32 / 160.0).min(1.0) } else { 1.0 };
    let ease = 1.0 - (1.0 - k).powi(3);
    let w = ((full as f32) * ease).round().max(3.0) as u16;
    let fade = if age + 600 > life { (age + 600 - life) as f32 / 600.0 } else { 0.0 };
    let c = theme::mix(col, theme::FAINT, fade);
    let r = Rect { x: area.x + area.width.saturating_sub(w + 1), y: area.y + 1, width: w.min(area.width), height: 3 };
    f.render_widget(Clear, r);
    let blk = Block::default().style(theme::surface()).borders(Borders::ALL).border_type(border_type()).border_style(Style::default().fg(c));
    f.render_widget(Paragraph::new(Span::styled(msg, Style::default().fg(c).add_modifier(Modifier::BOLD))).block(blk), r);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn agent() -> Agent {
        Agent::new(1, "a", "worker", PathBuf::from("."))
    }

    /// The threshold exists so an ordinary turn never grows a warning, and the `busy()` guard so a
    /// finished agent — whose `last_event` only gets staler from here — never looks stuck.
    #[test]
    fn quiet_for_only_fires_on_a_busy_agent_past_the_threshold() {
        let mut a = agent();
        a.last_event = std::time::Instant::now() - Duration::from_secs(3600);
        assert_eq!(quiet_for(&a), None, "an idle agent is not quiet, however stale");

        a.turn_active = true;
        a.last_event = std::time::Instant::now() - Duration::from_secs(5);
        assert_eq!(quiet_for(&a), None, "a ticking turn must stay unannotated");

        a.last_event = std::time::Instant::now() - Duration::from_secs(90);
        assert!(quiet_for(&a).is_some_and(|d| d.as_secs() >= 90), "a busy agent silent past the threshold reports how long");
        a.status = Status::Waiting;
        assert_eq!(quiet_for(&a), None, "approval is expected silence");
        let waiting = busy_line(&a).unwrap();
        let text = waiting.spans.iter().map(|s| s.content.as_ref()).collect::<String>();
        assert!(text.contains("waiting for approval") && !text.contains("quiet"), "{text}");
        a.status = Status::Busy;
        a.swarm_waiting = true;
        assert_eq!(quiet_for(&a), None, "swarm children are expected silence");
        let waiting = busy_line(&a).unwrap();
        let text = waiting.spans.iter().map(|s| s.content.as_ref()).collect::<String>();
        assert!(text.contains("waiting for Akasha swarm") && !text.contains("quiet"), "{text}");
        a.swarm_waiting = false;
        a.status = Status::Retrying("rate limited".into());
        assert!(!producing(&a));
        assert_eq!(quiet_for(&a), None);
        assert!(busy_line(&a).unwrap().spans.iter().any(|s| s.content.contains("retrying: rate limited")));
    }

    #[test]
    fn ascii_pass_keeps_narrow_status_cells_printable() {
        use ratatui::buffer::Buffer;
        let mut buf = Buffer::empty(Rect::new(0, 0, 40, 12));
        buf.set_string(0, 0, "⚑ waiting for approval · ctrl+g", Style::default());
        asciify(&mut buf);
        assert!(buf.content.iter().all(|c| c.symbol().is_ascii()));
        assert_eq!(buf[(0, 0)].symbol(), "!");
    }

    #[test]
    fn compact_approval_keeps_actions_when_detail_is_long() {
        use ratatui::{backend::TestBackend, Terminal};
        let ap = Approval {
            agent: 1, id: serde_json::json!("tool-1"), method: "item/commandExecution/requestApproval".into(),
            title: "Run command".into(), detail: "first detail\nsecond detail\nthird detail\nfourth detail".into(),
            params: serde_json::json!({}), at: std::time::Instant::now(),
        };
        let mut terminal = Terminal::new(TestBackend::new(40, 3)).unwrap();
        terminal.draw(|f| draw_approval(f, f.area(), &ap, "worker")).unwrap();
        let row = (0..40).map(|x| terminal.backend().buffer()[(x, 1)].symbol().to_string()).collect::<String>();
        assert!(row.contains("y yes") && row.contains("n no") && row.contains("esc cancel"), "{row}");
    }
}
