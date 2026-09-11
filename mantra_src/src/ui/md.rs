//! Minimal markdown → styled, wrapped lines (headings, bullets, code blocks, **bold**, `code`).

use super::theme;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

pub fn render(text: &str, width: usize, base: Style) -> Vec<Line<'static>> {
    let width = width.max(10);
    let mut out = vec![];
    let mut in_code = false;
    for raw in text.lines() {
        let line = raw.trim_end();
        if line.trim_start().starts_with("```") {
            in_code = !in_code;
            continue;
        }
        if in_code {
            let bar = Span::styled(format!("{} ", theme::g("│", "|")), theme::faint());
            let body = crate::util::trunc(&line.replace('\t', "    "), width.saturating_sub(2));
            out.push(Line::from(vec![bar, Span::styled(body, theme::fg(theme::CYAN))]));
            continue;
        }
        if line.trim().is_empty() {
            out.push(Line::default());
            continue;
        }
        let t = line.trim_start();
        let indent = line.len() - t.len();
        if let Some(h) = t.strip_prefix("### ").or_else(|| t.strip_prefix("## ")).or_else(|| t.strip_prefix("# ")) {
            let st = theme::bold(theme::accent());
            out.extend(wrap(inline(h, st), width, vec![], vec![]));
            continue;
        }
        let bullet = ["- ", "* ", "• "].iter().find_map(|b| t.strip_prefix(b));
        if let Some(b) = bullet {
            let pad = " ".repeat(indent.min(8));
            let first = vec![Span::raw(pad.clone()), Span::styled(format!("{} ", theme::g("•", "*")), theme::dim())];
            let rest = vec![Span::raw(format!("{pad}  "))];
            out.extend(wrap(inline(b, base), width, first, rest));
            continue;
        }
        let num = t.split_once(". ").filter(|(n, _)| !n.is_empty() && n.len() <= 3 && n.chars().all(|c| c.is_ascii_digit()));
        if let Some((n, rest_t)) = num {
            let first = vec![Span::styled(format!("{n}. "), theme::dim())];
            let rest = vec![Span::raw(" ".repeat(n.len() + 2))];
            out.extend(wrap(inline(rest_t, base), width, first, rest));
            continue;
        }
        if let Some(q) = t.strip_prefix("> ") {
            out.extend(wrap(inline(q, theme::muted()), width, vec![Span::styled(format!("{} ", theme::g("▎", "|")), theme::dim())], vec![Span::styled(format!("{} ", theme::g("▎", "|")), theme::dim())]));
            continue;
        }
        out.extend(wrap(inline(t, base), width, vec![], vec![]));
    }
    while out.last().map(|l| l.spans.is_empty()).unwrap_or(false) {
        out.pop();
    }
    out
}

/// Parse **bold** and `code` spans.
pub fn inline(s: &str, base: Style) -> Vec<Span<'static>> {
    let mut out = vec![];
    let mut cur = String::new();
    let mut bold = false;
    let mut code = false;
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;
    let style = |bold: bool, code: bool| {
        if code {
            theme::fg(theme::CYAN)
        } else if bold {
            base.add_modifier(Modifier::BOLD)
        } else {
            base
        }
    };
    while i < chars.len() {
        let c = chars[i];
        if c == '`' {
            if !cur.is_empty() {
                out.push(Span::styled(std::mem::take(&mut cur), style(bold, code)));
            }
            code = !code;
            i += 1;
            continue;
        }
        if !code && c == '*' && chars.get(i + 1) == Some(&'*') {
            if !cur.is_empty() {
                out.push(Span::styled(std::mem::take(&mut cur), style(bold, code)));
            }
            bold = !bold;
            i += 2;
            continue;
        }
        cur.push(c);
        i += 1;
    }
    if !cur.is_empty() {
        out.push(Span::styled(cur, style(bold, code)));
    }
    out
}

/// Word-wrap styled spans. `first`/`rest` are prefixes for the first and continuation lines.
pub fn wrap(spans: Vec<Span<'static>>, width: usize, first: Vec<Span<'static>>, rest: Vec<Span<'static>>) -> Vec<Line<'static>> {
    let pw = |p: &Vec<Span<'static>>| p.iter().map(|s| s.content.width()).sum::<usize>();
    let mut lines = vec![];
    let mut line: Vec<Span<'static>> = first.clone();
    let mut w = pw(&first);
    let mut avail = width;
    let rest_w = pw(&rest);
    // tokens: (text, style) split at spaces, keeping the space with the word
    let mut toks: Vec<(String, Style)> = vec![];
    for sp in spans {
        let st = sp.style;
        let mut word = String::new();
        for ch in sp.content.chars() {
            word.push(ch);
            if ch == ' ' {
                toks.push((std::mem::take(&mut word), st));
            }
        }
        if !word.is_empty() {
            toks.push((word, st));
        }
    }
    let empty_line = |w: usize, first_w: usize, lines: &Vec<Line>| -> bool { w == if lines.is_empty() { first_w } else { rest_w } };
    let first_w = w;
    for (tok, st) in toks {
        let tw = tok.width();
        let trimmed_w = tok.trim_end().width();
        if w + trimmed_w > avail && !empty_line(w, first_w, &lines) {
            lines.push(Line::from(std::mem::take(&mut line)));
            line = rest.clone();
            w = rest_w;
            avail = width;
            if tok.trim().is_empty() {
                continue;
            }
        }
        if w + tw > avail && tw > avail.saturating_sub(w) {
            // hard-break a long token
            let mut chunk = String::new();
            for ch in tok.chars() {
                let cw = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
                if w + cw > avail {
                    line.push(Span::styled(std::mem::take(&mut chunk), st));
                    lines.push(Line::from(std::mem::take(&mut line)));
                    line = rest.clone();
                    w = rest_w;
                }
                chunk.push(ch);
                w += cw;
            }
            if !chunk.is_empty() {
                line.push(Span::styled(chunk, st));
            }
            continue;
        }
        line.push(Span::styled(tok, st));
        w += tw;
    }
    if !line.is_empty() {
        lines.push(Line::from(line));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn wrap_never_exceeds_width() {
        let text = "hello wonderful world of terminals averyveryverylongwordthatmustbreak end";
        for w in 3..60 {
            let lines = wrap(vec![Span::raw(text)], w, vec![Span::raw("• ")], vec![Span::raw("  ")]);
            assert!(!lines.is_empty());
            for l in &lines {
                assert!(l.width() <= w.max(3), "width {w}: {:?}", l);
            }
            let joined: String = lines.iter().flat_map(|l| l.spans.iter().map(|s| s.content.to_string())).collect::<String>().replace(' ', "").replace('•', "");
            assert_eq!(joined, text.replace(' ', ""), "no text lost at width {w}");
        }
    }
    #[test]
    fn render_handles_everything_at_tiny_widths() {
        let md = "# Title\n\n- bullet **bold** and `code`\n1. numbered item that is long\n> quote\n```\nlet x = 1;\n```\nplain";
        for w in 0..30 {
            let _ = render(md, w, Style::default());
        }
    }
}
