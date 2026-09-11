//! Multi-line text input with history and readline-ish keys.

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use unicode_width::UnicodeWidthChar;

#[derive(Default)]
pub struct Input {
    pub buf: Vec<char>,
    pub cur: usize,
    hist: Vec<String>,
    hist_i: Option<usize>,
    stash: String,
}

pub enum Act {
    None,
    Submit,
    Changed,
    Unhandled,
}

impl Input {
    pub fn text(&self) -> String {
        self.buf.iter().collect()
    }
    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }
    pub fn set(&mut self, s: &str) {
        self.buf = s.chars().collect();
        self.cur = self.buf.len();
    }
    pub fn clear(&mut self) {
        self.buf.clear();
        self.cur = 0;
    }
    pub fn take(&mut self) -> String {
        let s = self.text();
        if !s.trim().is_empty() && self.hist.last() != Some(&s) {
            self.hist.push(s.clone());
            if self.hist.len() > 200 {
                self.hist.remove(0);
            }
        }
        self.hist_i = None;
        self.clear();
        s
    }
    pub fn insert_str(&mut self, s: &str) {
        for c in s.chars() {
            if c == '\r' {
                continue;
            }
            self.buf.insert(self.cur, c);
            self.cur += 1;
        }
    }
    fn word_left(&self) -> usize {
        let mut i = self.cur;
        while i > 0 && self.buf[i - 1].is_whitespace() {
            i -= 1;
        }
        while i > 0 && !self.buf[i - 1].is_whitespace() {
            i -= 1;
        }
        i
    }
    fn word_right(&self) -> usize {
        let mut i = self.cur;
        while i < self.buf.len() && self.buf[i].is_whitespace() {
            i += 1;
        }
        while i < self.buf.len() && !self.buf[i].is_whitespace() {
            i += 1;
        }
        i
    }
    fn line_start(&self) -> usize {
        let mut i = self.cur;
        while i > 0 && self.buf[i - 1] != '\n' {
            i -= 1;
        }
        i
    }
    fn line_end(&self) -> usize {
        let mut i = self.cur;
        while i < self.buf.len() && self.buf[i] != '\n' {
            i += 1;
        }
        i
    }
    fn history(&mut self, up: bool) {
        if self.hist.is_empty() {
            return;
        }
        let i = match (self.hist_i, up) {
            (None, true) => {
                self.stash = self.text();
                Some(self.hist.len() - 1)
            }
            (None, false) => None,
            (Some(i), true) => Some(i.saturating_sub(1)),
            (Some(i), false) if i + 1 < self.hist.len() => Some(i + 1),
            (Some(_), false) => None,
        };
        self.hist_i = i;
        let s = match i {
            Some(i) => self.hist[i].clone(),
            None => self.stash.clone(),
        };
        self.set(&s);
    }

    pub fn key(&mut self, k: KeyEvent) -> Act {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let alt = k.modifiers.contains(KeyModifiers::ALT);
        let shift = k.modifiers.contains(KeyModifiers::SHIFT);
        match k.code {
            KeyCode::Enter if shift || alt || ctrl => {
                self.insert_str("\n");
                Act::Changed
            }
            KeyCode::Char('j') if ctrl => {
                self.insert_str("\n");
                Act::Changed
            }
            KeyCode::Enter => {
                // trailing backslash = continue on next line
                if self.cur > 0 && self.buf.get(self.cur - 1) == Some(&'\\') {
                    self.buf.remove(self.cur - 1);
                    self.cur -= 1;
                    self.insert_str("\n");
                    return Act::Changed;
                }
                Act::Submit
            }
            KeyCode::Char(c) if !ctrl && !alt => {
                self.buf.insert(self.cur, c);
                self.cur += 1;
                Act::Changed
            }
            KeyCode::Char('a') if ctrl => {
                self.cur = self.line_start();
                Act::None
            }
            KeyCode::Char('e') if ctrl => {
                self.cur = self.line_end();
                Act::None
            }
            KeyCode::Char('u') if ctrl => {
                let s = self.line_start();
                self.buf.drain(s..self.cur);
                self.cur = s;
                Act::Changed
            }
            KeyCode::Char('k') if ctrl => {
                let e = self.line_end();
                self.buf.drain(self.cur..e);
                Act::Changed
            }
            KeyCode::Char('w') if ctrl => {
                let s = self.word_left();
                self.buf.drain(s..self.cur);
                self.cur = s;
                Act::Changed
            }
            KeyCode::Backspace if alt || ctrl => {
                let s = self.word_left();
                self.buf.drain(s..self.cur);
                self.cur = s;
                Act::Changed
            }
            KeyCode::Backspace => {
                if self.cur > 0 {
                    self.cur -= 1;
                    self.buf.remove(self.cur);
                }
                Act::Changed
            }
            KeyCode::Delete => {
                if self.cur < self.buf.len() {
                    self.buf.remove(self.cur);
                }
                Act::Changed
            }
            KeyCode::Left if ctrl || alt => {
                self.cur = self.word_left();
                Act::None
            }
            KeyCode::Right if ctrl || alt => {
                self.cur = self.word_right();
                Act::None
            }
            KeyCode::Left => {
                self.cur = self.cur.saturating_sub(1);
                Act::None
            }
            KeyCode::Right => {
                self.cur = (self.cur + 1).min(self.buf.len());
                Act::None
            }
            KeyCode::Home => {
                self.cur = self.line_start();
                Act::None
            }
            KeyCode::End => {
                self.cur = self.line_end();
                Act::None
            }
            KeyCode::Up if !self.buf.contains(&'\n') => {
                self.history(true);
                Act::Changed
            }
            KeyCode::Down if !self.buf.contains(&'\n') => {
                self.history(false);
                Act::Changed
            }
            KeyCode::Up => {
                let s = self.line_start();
                if s == 0 {
                    return Act::None;
                }
                let col = self.cur - s;
                let prev_end = s - 1;
                let mut ps = prev_end;
                while ps > 0 && self.buf[ps - 1] != '\n' {
                    ps -= 1;
                }
                self.cur = (ps + col).min(prev_end);
                Act::None
            }
            KeyCode::Down => {
                let e = self.line_end();
                if e >= self.buf.len() {
                    return Act::None;
                }
                let col = self.cur - self.line_start();
                let ns = e + 1;
                let mut ne = ns;
                while ne < self.buf.len() && self.buf[ne] != '\n' {
                    ne += 1;
                }
                self.cur = (ns + col).min(ne);
                Act::None
            }
            _ => Act::Unhandled,
        }
    }

    /// Wrap into rows of `width` cells. Returns (rows, cursor_row, cursor_col).
    pub fn layout(&self, width: usize) -> (Vec<String>, usize, usize) {
        let width = width.max(4);
        let mut rows = vec![String::new()];
        let mut w = 0;
        let (mut cr, mut cc) = (0, 0);
        for (i, ch) in self.buf.iter().enumerate() {
            if i == self.cur {
                cr = rows.len() - 1;
                cc = w;
            }
            if *ch == '\n' {
                rows.push(String::new());
                w = 0;
                continue;
            }
            let cw = ch.width().unwrap_or(0);
            if w + cw > width {
                rows.push(String::new());
                w = 0;
                if i == self.cur {
                    cr = rows.len() - 1;
                    cc = 0;
                }
            }
            rows.last_mut().unwrap().push(*ch);
            w += cw;
        }
        if self.cur >= self.buf.len() {
            if w >= width {
                rows.push(String::new());
                w = 0;
            }
            cr = rows.len() - 1;
            cc = w;
        }
        (rows, cr, cc)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn layout_wraps_and_tracks_cursor() {
        let mut i = Input::default();
        assert_eq!(i.layout(10), (vec![String::new()], 0, 0));
        i.set("hello world");
        let (rows, r, c) = i.layout(5);
        assert_eq!(rows.concat(), "hello world");
        assert!(rows.iter().all(|x| x.chars().count() <= 5));
        assert_eq!((r, c), (rows.len() - 1, rows.last().unwrap().chars().count()));
        i.set("ab\ncd");
        let (rows, r, c) = i.layout(10);
        assert_eq!(rows, vec!["ab".to_string(), "cd".to_string()]);
        assert_eq!((r, c), (1, 2));
        for w in 0..6 {
            let _ = i.layout(w);
        }
    }
}
