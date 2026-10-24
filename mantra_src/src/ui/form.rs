//! The one way the config screens — `/models`, `/studio`, `/settings` and the lists the overlays
//! pick from — edit a value and show which row is selected, so the three screens cannot drift.
//!
//! - **Type to edit.** A printable key on a text field starts editing it in place with that key as
//!   the new content (a spreadsheet's overwrite); ⏎ edits the value as it is, cursor at its end;
//!   Backspace starts from nothing. While editing, ⏎ keeps the text (the screen saves its file),
//!   esc brings the old value back, ↑/↓ keep it and move. No popup: the field becomes the `Input`.
//! - **Delete twice.** A row goes with two Delete presses within `CONFIRM_WITHIN` (`confirm`);
//!   the footer says "del again" in between (`armed`).
//! - **Choices.** ←→ / space / +- step through an option list (`cycle`), a letter jumps to the
//!   first option it starts (`jump`).
//! - **The selected row** is a band across the whole inner width (`band`, `theme::row_sel`), the
//!   selected cell bold and accent-coloured on top of it — so the cells at a row's right end read
//!   as that row's.

use super::theme;
use crate::app::EditTarget;
use crate::ui::input::{Act, Input};
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;
use std::time::{Duration, Instant};
use unicode_width::UnicodeWidthChar;

/// How long the first Delete press stays armed.
pub const CONFIRM_WITHIN: Duration = Duration::from_secs(3);

/// How an edit began, which decides what an unchanged commit means.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Start {
    /// ⏎ on the field: the value as it is, the cursor at its end.
    Append,
    /// A printable key: that key is the new content.
    Overwrite,
    /// Backspace: from nothing — committing the empty text clears the field.
    Clear,
}

/// A field being edited where it is drawn.
pub struct Edit {
    pub target: EditTarget,
    pub input: Input,
    /// What the field held: esc puts it back, and a commit that leaves it so is no change.
    pub orig: String,
    pub start: Start,
    /// Drawn as dots, the last four characters showing (an API key, the web password).
    pub secret: bool,
    /// Several lines (a role's instructions): ⏎ still keeps it, alt+⏎ or a trailing `\` is a
    /// newline as in the chat input, and ↑↓ move between the lines instead of leaving.
    pub multiline: bool,
}

/// What a key did to an `Edit`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Outcome {
    Editing,
    /// ⏎: keep the text.
    Commit,
    /// ↑ (-1) / ↓ (+1): keep the text and move to the row above / below.
    CommitMove(i32),
    /// esc: the old value back.
    Revert,
}

impl Edit {
    /// An edit of `value` begun by `start`: `Append` holds the value, the others start empty
    /// (`overwrite` puts the key that began it in).
    pub fn new(target: EditTarget, value: &str, start: Start) -> Edit {
        let mut input = Input::default();
        if start == Start::Append {
            input.set(value);
        }
        Edit { target, input, orig: value.to_string(), start, secret: false, multiline: false }
    }

    /// The edit a printable key `c` starts: `c` alone.
    pub fn overwrite(target: EditTarget, value: &str, c: char) -> Edit {
        let mut e = Edit::new(target, value, Start::Overwrite);
        e.input.insert_str(&c.to_string());
        e
    }

    pub fn secret(mut self) -> Edit {
        self.secret = true;
        self
    }

    pub fn multiline(mut self) -> Edit {
        self.multiline = true;
        self
    }

    pub fn key(&mut self, k: KeyEvent) -> Outcome {
        let plain = !k.modifiers.intersects(KeyModifiers::ALT | KeyModifiers::CONTROL | KeyModifiers::SHIFT);
        match k.code {
            KeyCode::Esc => Outcome::Revert,
            KeyCode::Enter if !self.multiline && plain => Outcome::Commit,
            KeyCode::Up if !self.multiline => Outcome::CommitMove(-1),
            KeyCode::Down if !self.multiline => Outcome::CommitMove(1),
            // a field's own history would only bring back other fields' values
            KeyCode::Up | KeyCode::Down if !self.input.buf.contains(&'\n') => Outcome::Editing,
            KeyCode::Tab | KeyCode::BackTab => Outcome::Editing,
            // one line: shift/alt+⏎ would make it two
            KeyCode::Enter if !self.multiline => Outcome::Editing,
            _ => match self.input.key(k) {
                Act::Submit => Outcome::Commit,
                _ => Outcome::Editing,
            },
        }
    }

    pub fn text(&self) -> String {
        self.input.text()
    }

    /// Whether committing would change anything: the text differs from the old value, or the
    /// edit began by clearing it (Backspace, then ⏎ on nothing clears a password on purpose).
    pub fn changed(&self) -> bool {
        self.start == Start::Clear || self.text() != self.orig
    }

    /// What one line `width` columns wide shows of the text, and the cursor's column in it: the
    /// part around the cursor when the text is longer (a URL in a 26-column cell), a secret as
    /// dots with its last four characters, newlines as `⏎`.
    pub fn view(&self, width: usize) -> (String, usize) {
        let chars: Vec<char> = self.input.buf.iter().map(|c| if *c == '\n' { '⏎' } else { *c }).collect();
        let shown: Vec<char> = if self.secret { mask_chars(&chars) } else { chars };
        let width = width.max(2);
        let cur = self.input.cur.min(shown.len());
        let w = |cs: &[char]| cs.iter().map(|c| c.width().unwrap_or(0)).sum::<usize>();
        // the window ends at the cursor (and one cell for it) when the text before it does not fit
        let mut start = 0;
        while start < cur && w(&shown[start..cur]) + 1 > width {
            start += 1;
        }
        let mut out = String::new();
        let mut used = 0;
        for c in &shown[start..] {
            let cw = c.width().unwrap_or(0);
            if used + cw > width {
                break;
            }
            out.push(*c);
            used += cw;
        }
        (out, w(&shown[start..cur]))
    }
}

/// `••••` and the last four characters — how a stored API key shows anywhere on screen.
pub fn mask(key: &str) -> String {
    let k: Vec<char> = key.trim().chars().collect();
    if k.is_empty() {
        return String::new();
    }
    mask_chars(&k).into_iter().collect()
}

fn mask_chars(k: &[char]) -> Vec<char> {
    if k.len() <= 4 {
        return vec!['•'; k.len().max(4)];
    }
    let mut out = vec!['•'; 4];
    out.extend(&k[k.len() - 4..]);
    out
}

/// A Delete press waiting for its second.
#[derive(Clone, Debug)]
pub struct Confirm {
    /// What the second press deletes, as the footer names it ("sol", "the Codex account").
    pub what: String,
    pub at: Instant,
}

/// The first Delete on `what` arms it and returns false; the second within `CONFIRM_WITHIN`
/// returns true (delete now) and disarms. A press on something else re-arms for that.
pub fn confirm(slot: &mut Option<Confirm>, what: &str) -> bool {
    if slot.as_ref().is_some_and(|c| c.what == what && c.at.elapsed() < CONFIRM_WITHIN) {
        *slot = None;
        return true;
    }
    *slot = Some(Confirm { what: what.to_string(), at: Instant::now() });
    false
}

/// What a second Delete would delete right now, while the first is armed.
pub fn armed(slot: &Option<Confirm>) -> Option<&str> {
    slot.as_ref().filter(|c| c.at.elapsed() < CONFIRM_WITHIN).map(|c| c.what.as_str())
}

/// `cur` stepped `d` places through `list`, wrapping; a value not in the list starts before
/// the first, so ▸ on it lands on the first option.
pub fn cycle<T: AsRef<str>>(list: &[T], cur: &str, d: i32) -> String {
    let n = list.len() as i32;
    if n == 0 {
        return cur.to_string();
    }
    let i = list.iter().position(|x| x.as_ref() == cur).map(|i| i as i32).unwrap_or(-1);
    list[((i + d).rem_euclid(n)) as usize].as_ref().to_string()
}

/// The first option after `cur` that starts with `c` (case-insensitive), wrapping — "h" on an
/// effort gives "high", and "m" twice steps from "medium" to "max". `None` when none does.
pub fn jump<T: AsRef<str>>(list: &[T], cur: &str, c: char) -> Option<String> {
    let c = c.to_lowercase().next()?;
    let n = list.len();
    let at = list.iter().position(|x| x.as_ref() == cur).map(|i| i + 1).unwrap_or(0);
    (0..n).map(|k| &list[(at + k) % n]).find(|o| o.as_ref().to_lowercase().starts_with(c)).map(|o| o.as_ref().to_string())
}

/// Paints the selected-row band across `area` (one row, the inner width of its box) on what is
/// already drawn there: the background only, so each cell keeps its own colour and weight.
pub fn band(buf: &mut Buffer, area: Rect) {
    let area = area.intersection(buf.area);
    let st = theme::row_sel();
    for y in area.y..area.y + area.height {
        for x in area.x..area.x + area.width {
            if let Some(cell) = buf.cell_mut((x, y)) {
                cell.set_style(st);
            }
        }
    }
}

/// The band for line `line` of a list drawn in `inner` scrolled by `scroll` lines — nothing when
/// that line is scrolled out of view.
pub fn band_line(buf: &mut Buffer, inner: Rect, line: usize, scroll: usize) {
    if line < scroll || line - scroll >= inner.height as usize {
        return;
    }
    band(buf, Rect { y: inner.y + (line - scroll) as u16, height: 1, ..inner });
}

/// How far a list of `len` lines shown `height` at a time scrolls so that line `sel` (and
/// `after` lines below it, a hint) is in view.
pub fn scroll_for(sel: usize, after: usize, height: usize, len: usize) -> usize {
    let height = height.max(1);
    let end = (sel + after + 1).min(len.max(sel + 1));
    end.saturating_sub(height).min(sel)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(c: KeyCode) -> KeyEvent {
        KeyEvent::new(c, KeyModifiers::NONE)
    }

    fn target() -> EditTarget {
        EditTarget::ModelCell(0, 0)
    }

    /// A printable key replaces the value, ⏎ appends to it, Backspace clears it; ⏎ commits, esc
    /// reverts, ↑↓ commit and move — and the chat input's history never leaks into a field.
    #[test]
    fn an_edit_overwrites_appends_or_clears_and_ends_on_enter_esc_or_a_move() {
        let mut e = Edit::overwrite(target(), "sol", 'l');
        assert_eq!((e.text().as_str(), e.changed()), ("l", true));
        e.key(key(KeyCode::Char('u')));
        assert_eq!(e.text(), "lu");
        assert_eq!(e.key(key(KeyCode::Enter)), Outcome::Commit);
        let mut e = Edit::new(target(), "sol", Start::Append);
        assert_eq!((e.text().as_str(), e.input.cur, e.changed()), ("sol", 3, false), "nothing typed is no change");
        e.key(key(KeyCode::Char('2')));
        assert_eq!(e.text(), "sol2");
        assert_eq!(e.key(key(KeyCode::Up)), Outcome::CommitMove(-1));
        assert_eq!(e.key(key(KeyCode::Down)), Outcome::CommitMove(1));
        assert_eq!(e.key(key(KeyCode::Esc)), Outcome::Revert);
        let e = Edit::new(target(), "secret", Start::Clear);
        assert_eq!((e.text().as_str(), e.changed()), ("", true), "clearing is a change even of nothing");
        // shift+⏎ never makes a one-line field two lines
        let mut e = Edit::new(target(), "a", Start::Append);
        assert_eq!(e.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT)), Outcome::Editing);
        assert_eq!(e.text(), "a");
    }

    /// Several lines: alt+⏎ is a newline and ↑↓ move within the text; plain ⏎ still keeps it.
    #[test]
    fn a_multiline_edit_takes_newlines_and_keeps_on_enter() {
        let mut e = Edit::new(target(), "first", Start::Append).multiline();
        assert_eq!(e.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT)), Outcome::Editing);
        e.key(key(KeyCode::Char('x')));
        assert_eq!(e.text(), "first\nx");
        assert_eq!(e.key(key(KeyCode::Up)), Outcome::Editing);
        assert_eq!(e.input.cur, 1, "up a line, same column");
        assert_eq!(e.key(key(KeyCode::Enter)), Outcome::Commit);
    }

    /// The visible part keeps the cursor in view; a secret shows dots and its last four.
    #[test]
    fn the_view_follows_the_cursor_and_masks_a_secret() {
        let mut e = Edit::new(target(), "https://api.example.dev/v1", Start::Append);
        assert_eq!(e.view(10), ("le.dev/v1".to_string(), 9), "the end, and a cell for the cursor");
        e.input.cur = 0;
        assert_eq!(e.view(10), ("https://ap".to_string(), 0));
        let mut s = Edit::new(target(), "", Start::Clear).secret();
        s.input.insert_str("gsk_abcdefgh1234");
        assert_eq!(s.view(30).0, "••••1234");
        assert_eq!(mask("sk-proj-abcd9876"), "••••9876");
        assert_eq!(mask("abc"), "••••");
        assert_eq!(mask(""), "");
    }

    #[test]
    fn delete_needs_two_presses_on_the_same_row() {
        let mut slot = None;
        assert!(!confirm(&mut slot, "sol"));
        assert_eq!(armed(&slot), Some("sol"));
        assert!(!confirm(&mut slot, "luna"), "another row re-arms");
        assert!(confirm(&mut slot, "luna"));
        assert_eq!(armed(&slot), None);
        slot = Some(Confirm { what: "sol".into(), at: Instant::now() - CONFIRM_WITHIN });
        assert_eq!(armed(&slot), None, "expired");
        assert!(!confirm(&mut slot, "sol"), "an expired press is a first press again");
    }

    #[test]
    fn cycle_wraps_and_a_letter_jumps() {
        let e = ["low", "medium", "high", "max"];
        assert_eq!(cycle(&e, "max", 1), "low");
        assert_eq!(cycle(&e, "low", -1), "max");
        assert_eq!(cycle(&e, "ultra", 1), "low", "a value not listed starts before the first");
        assert_eq!(jump(&e, "low", 'h').as_deref(), Some("high"));
        assert_eq!(jump(&e, "low", 'M').as_deref(), Some("medium"));
        assert_eq!(jump(&e, "medium", 'm').as_deref(), Some("max"), "the same letter steps on");
        assert_eq!(jump(&e, "low", 'z'), None);
    }

    #[test]
    fn the_scroll_keeps_the_selection_in_view() {
        assert_eq!(scroll_for(0, 0, 10, 40), 0);
        assert_eq!(scroll_for(12, 0, 10, 40), 3);
        assert_eq!(scroll_for(12, 2, 10, 40), 5, "and its hint under it");
        assert_eq!(scroll_for(39, 3, 10, 40), 30, "never past the end");
    }

    /// The band changes the background of every cell of the row, nothing else.
    #[test]
    fn the_band_spans_the_row_and_keeps_each_cells_colour() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 20, 3));
        buf[(3, 1)].set_style(theme::fg(theme::RED));
        band(&mut buf, Rect::new(1, 1, 18, 1));
        let bg = theme::row_sel().bg.expect("a background band");
        assert!((1..19).all(|x| buf[(x, 1)].bg == bg), "the whole row");
        assert_eq!(buf[(3, 1)].fg, theme::c(theme::RED), "the cell keeps its colour");
        assert_eq!((buf[(0, 1)].bg, buf[(19, 1)].bg, buf[(1, 0)].bg), (ratatui::style::Color::Reset, ratatui::style::Color::Reset, ratatui::style::Color::Reset), "and nothing outside it");
        band_line(&mut buf, Rect::new(0, 0, 20, 3), 7, 2);
        assert_eq!(buf[(0, 2)].bg, ratatui::style::Color::Reset, "a line scrolled out of view gets none");
    }
}
