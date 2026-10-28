//! Renders an agent's items (messages, reasoning, commands, edits, tools) as a scrollable log.

use super::{anim, md, theme};
use crate::agent::{Agent, Item, Kind, Level};
use crate::util::{trunc, width as w_of};
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

fn pad(n: usize) -> Span<'static> {
    Span::raw(" ".repeat(n))
}

fn diff_line(l: &str, width: usize) -> Line<'static> {
    let body = trunc(l, width);
    let st = if l.starts_with('+') && !l.starts_with("+++") {
        theme::fg(theme::GREEN)
    } else if l.starts_with('-') && !l.starts_with("---") {
        theme::fg(theme::RED)
    } else if l.starts_with("@@") {
        theme::fg(theme::VIOLET)
    } else {
        theme::dim()
    };
    Line::from(vec![pad(4), Span::styled(body, st)])
}

/// Lines for one item. In-progress items animate, so they aren't cached.
pub fn item_lines(it: &mut Item, width: u16, verbose: bool) -> Vec<Line<'static>> {
    let cacheable = it.done;
    if cacheable {
        if let Some((w, v, vb, lines)) = &it.cache {
            if *w == width && *v == it.version && *vb == verbose {
                return lines.clone();
            }
        }
    }
    let lines = build(it, width as usize, verbose || it.expanded);
    if cacheable {
        it.cache = Some((width, it.version, verbose, lines.clone()));
    }
    lines
}

fn build(it: &Item, width: usize, verbose: bool) -> Vec<Line<'static>> {
    let inner = width.saturating_sub(2).max(10);
    match &it.kind {
        Kind::User if crate::agent::engine_sent(&it.text) => {
            let first = it.text.lines().next().unwrap_or("");
            let (tag, rest) = match first.find(']') {
                Some(i) => (first[1..i].replace("mantra:", "mantra · "), first[i + 1..].trim().to_string()),
                None => ("mantra".to_string(), first.to_string()),
            };
            let mut out = vec![Line::from(vec![
                Span::styled(format!("{} ", theme::g("⇢", ">")), theme::fg(theme::VIOLET)),
                Span::styled(tag, theme::fg(theme::VIOLET).add_modifier(Modifier::BOLD)),
                Span::styled(if rest.is_empty() { String::new() } else { format!("  {}", trunc(&rest, inner.saturating_sub(20))) }, theme::dim()),
            ])];
            let body: Vec<&str> = it.text.lines().skip(1).filter(|l| !l.trim().is_empty()).collect();
            let keep = if verbose { body.len() } else { 2.min(body.len()) };
            for l in &body[..keep] {
                out.extend(md::wrap(vec![Span::styled(l.to_string(), theme::dim())], width, vec![pad(2)], vec![pad(2)]));
            }
            if body.len() > keep {
                out.push(Line::from(vec![pad(2), Span::styled(format!("… {} more lines (ctrl+e)", body.len() - keep), theme::faint())]));
            }
            out
        }
        Kind::User => {
            let mut text = it.text.clone();
            let total = text.lines().count();
            if !verbose && total > 8 {
                text = text.lines().take(6).collect::<Vec<_>>().join("\n") + &format!("\n… {} more lines (ctrl+e expands)", total - 6);
            }
            let st = theme::bold(theme::text());
            let mut out = vec![];
            for (i, l) in text.lines().enumerate() {
                let first = if i == 0 { vec![Span::styled(format!("{} ", theme::g("›", ">")), theme::fg(theme::ROSE).add_modifier(Modifier::BOLD))] } else { vec![pad(2)] };
                out.extend(md::wrap(vec![Span::styled(l.to_string(), st)], width, first, vec![pad(2)]));
            }
            out
        }
        Kind::Agent => {
            let body = md::render(&it.text, inner, theme::text());
            let mut out = vec![];
            for (i, l) in body.into_iter().enumerate() {
                let mut spans = vec![if i == 0 { Span::styled(format!("{} ", theme::g("●", "*")), theme::text()) } else { pad(2) }];
                spans.extend(l.spans);
                out.push(Line::from(spans));
            }
            if out.is_empty() && !it.done {
                out.push(Line::from(vec![Span::styled(format!("{} ", theme::g("●", "*")), theme::text()), Span::styled(anim::spinner().to_string(), theme::dim())]));
            }
            out
        }
        Kind::Reasoning => {
            let text = it.text.trim();
            let (title, body) = match text.strip_prefix("**").and_then(|r| r.split_once("**")) {
                Some((t, rest)) => (t.to_string(), rest.trim().to_string()),
                None => ("Thinking".to_string(), text.to_string()),
            };
            let head_style = theme::dim().add_modifier(theme::italic());
            let mut out = vec![Line::from(vec![Span::styled(format!("{} ", theme::g("∴", ":")), theme::dim()), Span::styled(title, head_style)])];
            if (verbose || !it.done) && !body.is_empty() {
                let lines = md::render(&body, inner, theme::dim().add_modifier(theme::italic()));
                let n = lines.len();
                let keep = if verbose { n } else { 3.min(n) };
                for l in lines.into_iter().skip(n - keep) {
                    let mut spans = vec![pad(2)];
                    spans.extend(l.spans);
                    out.push(Line::from(spans));
                }
            }
            out
        }
        Kind::Plan => {
            let mut out = vec![Line::from(vec![Span::styled(format!("{} Plan", theme::g("☰", "=")), theme::bold(theme::accent()))])];
            for l in md::render(&it.text, inner, theme::text()) {
                let mut s = vec![pad(2)];
                s.extend(l.spans);
                out.push(Line::from(s));
            }
            out
        }
        Kind::Command { cmd, output, exit, status, dur_ms } => {
            let (mark, mst) = match (status.as_str(), exit) {
                ("inProgress", _) => (anim::spinner().to_string(), theme::fg(theme::AMBER)),
                ("declined", _) => (theme::g("⊘", "x").to_string(), theme::fg(theme::AMBER)),
                (_, Some(0)) => (theme::g("✓", "ok").to_string(), theme::fg(theme::GREEN)),
                (_, Some(c)) => (format!("{} {c}", theme::g("✗", "x")), theme::fg(theme::RED)),
                _ => (theme::g("✓", "ok").to_string(), theme::fg(theme::GREEN)),
            };
            let first = cmd.lines().next().unwrap_or("");
            let dur = dur_ms.map(|d| if d >= 1000 { format!(" {:.1}s", d as f64 / 1000.0) } else { format!(" {d}ms") }).unwrap_or_default();
            let mut out = vec![Line::from(vec![
                Span::styled("$ ", theme::fg(theme::VIOLET)),
                Span::styled(trunc(first, inner.saturating_sub(12)), theme::bold(theme::text())),
                Span::raw(" "),
                Span::styled(mark, mst),
                Span::styled(dur, theme::faint()),
            ])];
            let lines: Vec<&str> = output.lines().collect();
            let keep = if verbose { 60 } else if status == "inProgress" { 4 } else if exit.map(|c| c != 0).unwrap_or(false) { 6 } else { 2 };
            let start = lines.len().saturating_sub(keep);
            if start > 0 {
                out.push(Line::from(vec![pad(2), Span::styled(format!("… {} lines", start), theme::faint())]));
            }
            for l in &lines[start..] {
                out.push(Line::from(vec![pad(2), Span::styled(trunc(l, inner.saturating_sub(2)), theme::dim())]));
            }
            out
        }
        Kind::Files { changes, status } => {
            let mut out = vec![];
            for c in changes {
                let (verb, st) = match c.kind.as_str() {
                    "add" => ("Add", theme::fg(theme::GREEN)),
                    "delete" => ("Delete", theme::fg(theme::RED)),
                    _ => ("Edit", theme::fg(theme::AMBER)),
                };
                let is_diff = c.diff.lines().any(|l| l.starts_with("@@"));
                let (a, d) = if c.kind == "add" && !is_diff { (c.diff.lines().count(), 0) } else { crate::util::diff_stats(&c.diff) };
                let mark = if status == "inProgress" { anim::spinner().to_string() } else if status == "failed" || status == "declined" { format!(" {status}") } else { String::new() };
                out.push(Line::from(vec![
                    Span::styled(format!("{} {verb} ", theme::g("✎", "~")), st),
                    Span::styled(trunc(&c.path, inner.saturating_sub(20)), theme::bold(theme::text())),
                    Span::styled(format!("  +{a}"), theme::fg(theme::GREEN)),
                    Span::styled(format!(" -{d}"), theme::fg(theme::RED)),
                    Span::styled(mark, theme::dim()),
                ]));
                let body: Vec<String> = if c.kind == "add" && !is_diff { c.diff.lines().map(|l| format!("+{l}")).collect() } else { c.diff.lines().filter(|l| !l.starts_with("---") && !l.starts_with("+++")).map(|l| l.to_string()).collect() };
                let keep = if verbose { 400 } else { 6 };
                for l in body.iter().take(keep) {
                    out.push(diff_line(l, inner.saturating_sub(4)));
                }
                if body.len() > keep {
                    out.push(Line::from(vec![pad(4), Span::styled(format!("… {} more (ctrl+d for the diff viewer)", body.len() - keep), theme::faint())]));
                }
            }
            out
        }
        Kind::Tool { name, args, result, status, image } => {
            let mark = match status.as_str() {
                "inProgress" => anim::spinner().to_string(),
                "failed" => theme::g("✗", "x").to_string(),
                _ => theme::g("✓", "ok").to_string(),
            };
            let short = name.trim_start_matches("mantra_");
            let mut out = vec![Line::from(vec![
                Span::styled(format!("{} ", theme::g("⚙", "%")), theme::fg(theme::VIOLET)),
                Span::styled(short.to_string(), theme::fg(theme::VIOLET).add_modifier(Modifier::BOLD)),
                Span::styled(format!("({})", trunc(args, inner.saturating_sub(w_of(short) + 8))), theme::dim()),
                Span::raw(" "),
                Span::styled(mark, if status == "failed" { theme::fg(theme::RED) } else { theme::dim() }),
            ])];
            if !result.is_empty() {
                let keep = if verbose { 30 } else { 2 };
                let path = image.as_deref().unwrap_or("");
                for l in result.lines().filter_map(|l| beside_image(l, path)).take(keep) {
                    let l = l.as_str();
                    out.push(Line::from(vec![pad(2), Span::styled(format!("{} ", theme::g("⎿", "|")), theme::faint()), Span::styled(trunc(l, inner.saturating_sub(4)), theme::dim())]));
                }
            }
            // an image went to the model: say so and where it is (nothing here draws it)
            if let Some(path) = image {
                out.push(Line::from(vec![pad(2), Span::styled(image_marker(path, inner.saturating_sub(2)), theme::fg(theme::BLUE))]));
            }
            out
        }
        Kind::Web { query } => vec![Line::from(vec![Span::styled(format!("{} ", theme::g("⌕", "?")), theme::fg(theme::BLUE)), Span::styled(format!("searched: {query}"), theme::muted())])],
        Kind::Notice { level } => {
            let (g, st) = match level {
                Level::Error => (theme::g("✗", "x"), theme::fg(theme::RED)),
                Level::Warn => (theme::g("!", "!"), theme::fg(theme::AMBER)),
                Level::Ok => (theme::g("✓", "ok"), theme::fg(theme::GREEN)),
                Level::Info => (theme::g("ℹ", "i"), theme::dim()),
            };
            md::wrap(vec![Span::styled(it.text.clone(), st)], width, vec![Span::styled(format!("{g} "), st)], vec![pad(2)])
        }
        Kind::Compaction { from, to } => {
            let g = theme::g("⇣", "v");
            let (label, st) = if !it.done {
                (format!(" {g} compacting context {} ", anim::spinner()), theme::fg(theme::AMBER))
            } else {
                let t = match (*from, *to) {
                    (f, Some(t)) if f > 0 => format!(" {g} context compacted · {} → {} tokens ", crate::util::fmt_tokens(f), crate::util::fmt_tokens(t)),
                    (f, None) if f > 0 => format!(" {g} context compacted (was {}) ", crate::util::fmt_tokens(f)),
                    _ => format!(" {g} context compacted "),
                };
                (t, theme::muted())
            };
            let lw = w_of(&label);
            let side = width.saturating_sub(lw) / 2;
            let rule = theme::g("─", "-");
            vec![Line::from(vec![
                Span::styled(rule.repeat(side.min(24)), theme::faint()),
                Span::styled(label, st),
                Span::styled(rule.repeat(side.min(24)), theme::faint()),
            ])]
        }
    }
}

/// Draw the agent's log into `area`, bottom-anchored, honouring agent.scroll (lines from bottom).
pub fn draw(f: &mut Frame, area: Rect, agent: &mut Agent, verbose: bool, footer: Vec<Line<'static>>) {
    if area.height == 0 || area.width < 8 {
        return;
    }
    let width = area.width.saturating_sub(1);
    let need = area.height as usize + agent.scroll + 1;
    // Pass 1: make sure enough items are rendered (walk backwards).
    let mut count = footer.len();
    let mut first = agent.items.len();
    for i in (0..agent.items.len()).rev() {
        let n = item_lines(&mut agent.items[i], width, verbose).len();
        count += n + 1;
        first = i;
        if count >= need {
            break;
        }
    }
    // Pass 2: collect.
    let mut lines: Vec<Line<'static>> = vec![];
    for i in first..agent.items.len() {
        let it = &mut agent.items[i];
        let tight = matches!(it.kind, Kind::Tool { .. } | Kind::Notice { .. }) && i > first && agent_kind_prev(&lines);
        if !lines.is_empty() && !tight {
            lines.push(Line::default());
        }
        lines.extend(item_lines(it, width, verbose));
    }
    if !footer.is_empty() {
        if !lines.is_empty() {
            lines.push(Line::default());
        }
        lines.extend(footer);
    }
    let total = lines.len();
    let h = area.height as usize;
    let max_scroll = total.saturating_sub(h);
    if agent.scroll > max_scroll {
        agent.scroll = max_scroll;
    }
    let end = total - agent.scroll.min(total);
    let start = end.saturating_sub(h);
    let view: Vec<Line> = lines[start..end].to_vec();
    f.render_widget(Paragraph::new(view), Rect { x: area.x + 1, width: area.width.saturating_sub(1), ..area });
    if agent.scroll > 0 {
        let tag = format!(" {} {} lines below ", theme::g("↓", "v"), agent.scroll);
        let tw = w_of(&tag) as u16;
        if area.width > tw + 2 {
            f.render_widget(Paragraph::new(Span::styled(tag, theme::action())), Rect { x: area.x + area.width.saturating_sub(tw + 1), y: area.y + area.height.saturating_sub(1), width: tw, height: 1 });
        }
    }
}

fn agent_kind_prev(_lines: &[Line]) -> bool {
    false
}

/// A result line as the card shows it under an image marker that names `path`: the marker says
/// where the file is, so a line that names it keeps only what follows — `(image/png, 1920x1080
/// px, 812 KiB, via grim)` reads `image/png, 1920x1080 px, 812 KiB, via grim` — and a line that
/// only named it (bija's `[image: …]`) goes. The model was told the whole sentence; this is the card.
fn beside_image(line: &str, path: &str) -> Option<String> {
    let Some(at) = (!path.is_empty()).then(|| line.find(path)).flatten() else {
        return Some(line.to_string());
    };
    let rest = line[at + path.len()..].trim_start_matches(']').trim();
    // `(what, size, via) more.` → `what, size, via more`; a following sentence joins with ` · `
    let (inside, after) = rest.strip_prefix('(').and_then(|r| r.split_once(')')).unwrap_or(("", rest));
    let after = after.trim_end_matches('.');
    let after = match after.strip_prefix(". ") {
        Some(next) => format!(" · {next}"),
        None => after.to_string(),
    };
    let shown = format!("{inside}{after}").trim().to_string();
    (!shown.is_empty()).then_some(shown)
}

/// `▣ image  /run/screenshots/solo-1.png` (ASCII `[img] …`): a tool result that carried an
/// image. A long path keeps its end — the file name is what tells two captures apart. (Not 🖼:
/// half the terminals paint it two cells wide, and the side panel's border moves over by one.)
pub fn image_marker(path: &str, width: usize) -> String {
    let mark = theme::g("▣ image", "[img]");
    if path.is_empty() {
        return mark.to_string();
    }
    // "..." in ASCII: a lone "." (the frame's ASCII pass for "…") would read as a hidden file
    let more = theme::g("…", "...");
    let room = width.saturating_sub(w_of(mark) + 2);
    let shown = if w_of(path) <= room {
        path.to_string()
    } else {
        let mut tail: Vec<char> = vec![];
        let mut w = w_of(more);
        for c in path.chars().rev() {
            w += w_of(c.encode_utf8(&mut [0; 4]));
            if w > room {
                break;
            }
            tail.push(c);
        }
        format!("{more}{}", tail.into_iter().rev().collect::<String>())
    };
    format!("{mark}  {shown}")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Under an image marker a result line does not name the file again: Mantra's screenshot and
    /// view-image sentences keep their details, bija's `[image: …]` line goes, others stay.
    #[test]
    fn a_result_line_beside_an_image_keeps_what_the_marker_does_not_say() {
        let p = "/home/me/.mantra/runs/x/screenshots/p1-ui-1.png";
        let shot = |tail: &str| format!("Screenshot of the screen of the machine Mantra runs on, saved to {p} (image/png, 1x1 px, 67 B, via grim){tail}");
        assert_eq!(beside_image(&shot("."), p).as_deref(), Some("image/png, 1x1 px, 67 B, via grim"));
        assert_eq!(beside_image(&shot(", region 0,0,10,10."), p).as_deref(), Some("image/png, 1x1 px, 67 B, via grim, region 0,0,10,10"));
        assert_eq!(beside_image(&shot(". Note: the region was ignored."), p).as_deref(), Some("image/png, 1x1 px, 67 B, via grim · Note: the region was ignored"));
        assert_eq!(beside_image(&format!("{p} (image/png, 5x3 px, 76 B)"), p).as_deref(), Some("image/png, 5x3 px, 76 B"));
        assert_eq!(beside_image(&format!("[image: {p}]"), p), None);
        assert_eq!(beside_image("Rendered a 64×64 preview.", p).as_deref(), Some("Rendered a 64×64 preview."));
        assert_eq!(beside_image(&shot("."), "").as_deref(), Some(shot(".").as_str()), "no path known: the line as it is");
    }

    #[test]
    fn an_image_marker_names_the_file_and_keeps_its_end() {
        let m = image_marker("/home/me/.mantra/screenshots/solo-1.png", 80);
        assert!(m.ends_with("  /home/me/.mantra/screenshots/solo-1.png") && (m.starts_with("▣ image") || m.starts_with("[img]")), "{m}");
        let short = image_marker("/home/me/.mantra/screenshots/solo-1.png", 24);
        assert!(short.ends_with("solo-1.png") && (short.contains('…') || short.contains("...")) && w_of(&short) <= 24, "{short}");
        assert!(["▣ image", "[img]"].contains(&image_marker("", 80).as_str()));
    }
}
