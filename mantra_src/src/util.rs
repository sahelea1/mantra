//! Small shared helpers: file logger, glob matching, formatting.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

static LOG: OnceLock<Mutex<File>> = OnceLock::new();

pub fn init_log(path: &Path) {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(f) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = LOG.set(Mutex::new(f));
    }
}

pub fn log_line(s: &str) {
    if let Some(m) = LOG.get() {
        if let Ok(mut f) = m.lock() {
            let _ = writeln!(f, "{} {}", unix_secs(), s);
        }
    }
}

#[macro_export]
macro_rules! mlog {
    ($($t:tt)*) => { $crate::util::log_line(&format!($($t)*)) };
}

pub fn unix_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Local-ish clock string HH:MM:SS (UTC offset not applied; good enough for relative event feeds).
pub fn clock() -> String {
    let s = unix_secs() + tz_offset_secs();
    format!("{:02}:{:02}:{:02}", (s / 3600) % 24, (s / 60) % 60, s % 60)
}

fn tz_offset_secs() -> u64 {
    // Best effort: honour MANTRA_TZ_OFFSET (hours), else UTC.
    std::env::var("MANTRA_TZ_OFFSET")
        .ok()
        .and_then(|v| v.parse::<i64>().ok())
        .map(|h| (h.rem_euclid(24) * 3600) as u64)
        .unwrap_or(0)
}

pub fn fmt_dur(d: Duration) -> String {
    let s = d.as_secs();
    if s < 60 {
        format!("{}s", s)
    } else if s < 3600 {
        format!("{}m{:02}s", s / 60, s % 60)
    } else {
        format!("{}h{:02}m", s / 3600, (s / 60) % 60)
    }
}

pub fn fmt_tokens(n: u64) -> String {
    if n < 1000 {
        format!("{}", n)
    } else if n < 1_000_000 {
        format!("{:.1}k", n as f64 / 1000.0)
    } else {
        format!("{:.2}M", n as f64 / 1_000_000.0)
    }
}

/// Truncate to a display width, adding an ellipsis.
pub fn trunc(s: &str, width: usize) -> String {
    use unicode_width::UnicodeWidthChar;
    if width == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut w = 0;
    let total: usize = s.chars().map(|c| c.width().unwrap_or(0)).sum();
    if total <= width {
        return s.to_string();
    }
    for c in s.chars() {
        let cw = c.width().unwrap_or(0);
        if w + cw + 1 > width {
            break;
        }
        out.push(c);
        w += cw;
    }
    out.push('…');
    out
}

pub fn width(s: &str) -> usize {
    unicode_width::UnicodeWidthStr::width(s)
}

/// Keep only the last `max` bytes of a string (on a char boundary).
pub fn tail_bytes(s: &mut String, max: usize) {
    if s.len() > max {
        let mut cut = s.len() - max;
        while !s.is_char_boundary(cut) {
            cut += 1;
        }
        s.drain(..cut);
    }
}

/// Glob match supporting `*` (within a segment), `**` (any depth) and `?`.
pub fn glob_match(pattern: &str, path: &str) -> bool {
    let pattern = pattern.trim_start_matches("./");
    let path = path.trim_start_matches("./");
    let p: Vec<&str> = pattern.split('/').filter(|s| !s.is_empty()).collect();
    let s: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    glob_segs(&p, &s)
}

fn glob_segs(p: &[&str], s: &[&str]) -> bool {
    if p.is_empty() {
        return s.is_empty();
    }
    if p[0] == "**" {
        // match zero or more segments
        for i in 0..=s.len() {
            if glob_segs(&p[1..], &s[i..]) {
                return true;
            }
        }
        return false;
    }
    if s.is_empty() {
        return false;
    }
    seg_match(p[0].as_bytes(), s[0].as_bytes()) && glob_segs(&p[1..], &s[1..])
}

fn seg_match(p: &[u8], s: &[u8]) -> bool {
    if p.is_empty() {
        return s.is_empty();
    }
    match p[0] {
        b'*' => (0..=s.len()).any(|i| seg_match(&p[1..], &s[i..])),
        b'?' => !s.is_empty() && seg_match(&p[1..], &s[1..]),
        c => !s.is_empty() && s[0] == c && seg_match(&p[1..], &s[1..]),
    }
}

/// A path is in scope if any glob matches, or if the glob is a directory prefix.
pub fn in_scope(scope: &[String], rel_path: &str) -> bool {
    if scope.is_empty() {
        return true;
    }
    scope.iter().any(|g| {
        let g = g.trim();
        if g.is_empty() {
            return false;
        }
        if glob_match(g, rel_path) {
            return true;
        }
        let dir = g.trim_end_matches('/');
        !dir.contains('*') && (rel_path == dir || rel_path.starts_with(&format!("{}/", dir)))
    })
}

/// Count added/removed lines in a unified diff.
pub fn diff_stats(diff: &str) -> (usize, usize) {
    let mut a = 0;
    let mut d = 0;
    for l in diff.lines() {
        if l.starts_with("+++") || l.starts_with("---") {
            continue;
        }
        if l.starts_with('+') {
            a += 1;
        } else if l.starts_with('-') {
            d += 1;
        }
    }
    (a, d)
}

/// Strip ANSI escape sequences: CSI (`ESC [ … final-byte`), OSC (`ESC ] … BEL|ST`), and lone `ESC`.
/// Codex/subprocess stderr is sometimes colourized; this keeps journal lines and crash reasons plain.
pub fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        match chars.peek() {
            Some('[') => {
                chars.next(); // consume '['
                // parameter bytes 0x30-0x3F, then intermediate bytes 0x20-0x2F
                while let Some(&nc) = chars.peek() {
                    if ('0'..='?').contains(&nc) || (' '..='/').contains(&nc) {
                        chars.next();
                    } else {
                        break;
                    }
                }
                chars.next(); // consume the final byte, if any
            }
            Some(']') => {
                chars.next(); // consume ']'
                loop {
                    match chars.next() {
                        Some('\u{7}') | None => break,
                        Some('\u{1b}') => {
                            if chars.peek() == Some(&'\\') {
                                chars.next();
                            }
                            break;
                        }
                        Some(_) => continue,
                    }
                }
            }
            _ => {} // lone ESC: drop it, nothing else consumed
        }
    }
    out
}

pub fn slug(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.ends_with('-') {
            out.push('-');
        }
        if out.len() > 40 {
            break;
        }
    }
    out.trim_matches('-').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn globs() {
        assert!(glob_match("src/**", "src/a/b.rs"));
        assert!(glob_match("src/**/*.rs", "src/a/b.rs"));
        assert!(glob_match("src/*.rs", "src/b.rs"));
        assert!(!glob_match("src/*.rs", "src/a/b.rs"));
        assert!(glob_match("**/test_*.py", "a/b/test_x.py"));
        assert!(in_scope(&["src/auth".into()], "src/auth/mod.rs"));
        assert!(!in_scope(&["src/auth/**".into()], "src/orders/mod.rs"));
        assert!(in_scope(&[], "anything"));
    }
    #[test]
    fn stats() {
        assert_eq!(diff_stats("--- a\n+++ b\n+x\n-y\n+z\n"), (2, 1));
    }
    #[test]
    fn truncation() {
        assert_eq!(trunc("hello world", 6), "hello…");
        assert_eq!(trunc("hi", 6), "hi");
    }
    #[test]
    fn strip_ansi_matches_the_f1_capture() {
        // Exact stderr tail from the F1 finding (a dim-styled timestamp, then a truncated colour
        // sequence cut off mid-escape by the old byte-limited tail).
        let input = "planner: process crashed (codex exited: \u{1b}[2m2026-09-11T22:51:26.627070Z\u{1b}[0m \u{1b}[…) — restarting & resuming";
        let out = strip_ansi(input);
        assert!(!out.contains('\u{1b}'), "no ESC byte must survive: {out:?}");
        assert_eq!(out, "planner: process crashed (codex exited: 2026-09-11T22:51:26.627070Z ) — restarting & resuming");
    }
    #[test]
    fn strip_ansi_handles_osc_and_lone_esc() {
        assert_eq!(strip_ansi("a\u{1b}]0;title\u{7}b"), "ab");
        assert_eq!(strip_ansi("a\u{1b}]0;title\u{1b}\\b"), "ab");
        assert_eq!(strip_ansi("a\u{1b}b"), "ab");
        assert_eq!(strip_ansi("plain text"), "plain text");
    }
}
