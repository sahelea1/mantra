//! Colours and glyphs with graceful fallback (truecolor → 256 → 16, Unicode → ASCII).

use ratatui::style::{Color, Modifier, Style};
use std::sync::OnceLock;

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Depth {
    True,
    C256,
    C16,
}

struct T {
    depth: Depth,
    ascii: bool,
    motion: bool,
    italic: bool,
}

static TH: OnceLock<T> = OnceLock::new();

pub fn init(s: &crate::config::Settings) {
    let term = std::env::var("TERM").unwrap_or_default();
    let colorterm = std::env::var("COLORTERM").unwrap_or_default().to_lowercase();
    let prog = std::env::var("TERM_PROGRAM").unwrap_or_default();
    let depth = match s.colors.as_str() {
        "truecolor" => Depth::True,
        "256" => Depth::C256,
        "16" => Depth::C16,
        _ => {
            if colorterm.contains("truecolor") || colorterm.contains("24bit") {
                Depth::True
            } else if prog == "Apple_Terminal" || term.contains("256") {
                Depth::C256
            } else if term == "linux" || term == "dumb" || term.is_empty() {
                Depth::C16
            } else {
                Depth::C256
            }
        }
    };
    let lang = format!("{}{}{}", std::env::var("LC_ALL").unwrap_or_default(), std::env::var("LC_CTYPE").unwrap_or_default(), std::env::var("LANG").unwrap_or_default()).to_lowercase();
    let ascii = match s.glyphs.as_str() {
        "ascii" => true,
        "unicode" => false,
        _ => term == "linux" || (!lang.is_empty() && !lang.contains("utf")),
    };
    let italic = !(term.starts_with("screen") || term == "linux");
    let _ = TH.set(T { depth, ascii, motion: !s.reduce_motion, italic });
}

fn th() -> &'static T {
    TH.get_or_init(|| T { depth: Depth::C256, ascii: false, motion: true, italic: true })
}

pub fn depth() -> Depth {
    th().depth
}
pub fn ascii() -> bool {
    th().ascii
}
pub fn motion() -> bool {
    th().motion
}
/// Italic where it renders reliably (not TERM=screen*/linux, where it can show as reverse video).
pub fn italic() -> Modifier {
    if th().italic {
        Modifier::ITALIC
    } else {
        Modifier::empty()
    }
}

/// Pick a glyph with an ASCII fallback.
pub fn g(uni: &'static str, asc: &'static str) -> &'static str {
    if ascii() {
        asc
    } else {
        uni
    }
}

/// Role glyph with ASCII fallback.
pub fn role_glyph(s: &str) -> String {
    if !ascii() {
        return s.to_string();
    }
    match s {
        "✦" | "★" => "*",
        "◉" => "@",
        "◇" | "●" | "○" => "o",
        "◆" | "■" => "#",
        "◎" => "O",
        "▲" => "^",
        _ if s.is_ascii() => s,
        _ => "?",
    }
    .to_string()
}

pub fn rgb(r: u8, g: u8, b: u8) -> Color {
    match depth() {
        Depth::True => Color::Rgb(r, g, b),
        Depth::C256 => Color::Indexed(to_256(r, g, b)),
        Depth::C16 => to_16(r, g, b),
    }
}

fn to_256(r: u8, g: u8, b: u8) -> u8 {
    let q = |v: u8| -> u8 {
        if v < 48 {
            0
        } else if v < 115 {
            1
        } else {
            ((v as u16 - 35) / 40) as u8
        }
    };
    let (qr, qg, qb) = (q(r), q(g), q(b));
    let cube = 16 + 36 * qr + 6 * qg + qb;
    // greys are better on the grayscale ramp
    if (r as i16 - g as i16).abs() < 12 && (g as i16 - b as i16).abs() < 12 {
        let avg = (r as u16 + g as u16 + b as u16) / 3;
        if avg < 8 {
            return 16;
        }
        if avg > 238 {
            return 231;
        }
        return (232 + (avg - 8) / 10).min(255) as u8;
    }
    cube
}

fn to_16(r: u8, g: u8, b: u8) -> Color {
    let bright = (r as u16 + g as u16 + b as u16) > 380;
    let (hr, hg, hb) = (r > 120, g > 120, b > 120);
    match (hr, hg, hb, bright) {
        (false, false, false, _) => Color::DarkGray,
        (true, false, false, _) => Color::Red,
        (false, true, false, _) => Color::Green,
        (true, true, false, _) => Color::Yellow,
        (false, false, true, _) => Color::Blue,
        (true, false, true, _) => Color::Magenta,
        (false, true, true, _) => Color::Cyan,
        (true, true, true, true) => Color::White,
        (true, true, true, false) => Color::Gray,
    }
}

// Palette (RGB tuples so animations can blend).
pub const SAFFRON: (u8, u8, u8) = (242, 165, 65);
pub const VIOLET: (u8, u8, u8) = (150, 130, 250);
pub const TEAL: (u8, u8, u8) = (60, 207, 180);
pub const CYAN: (u8, u8, u8) = (90, 190, 240);
pub const GREEN: (u8, u8, u8) = (110, 214, 138);
pub const ROSE: (u8, u8, u8) = (242, 104, 138);
pub const RED: (u8, u8, u8) = (240, 90, 90);
pub const AMBER: (u8, u8, u8) = (232, 197, 71);
pub const BLUE: (u8, u8, u8) = (110, 150, 255);
pub const GRAY: (u8, u8, u8) = (120, 126, 140);
pub const TEXT: (u8, u8, u8) = (226, 230, 236);
pub const MUTED: (u8, u8, u8) = (160, 166, 178);
pub const DIM: (u8, u8, u8) = (96, 102, 116);
pub const FAINT: (u8, u8, u8) = (62, 66, 78);

pub fn c(t: (u8, u8, u8)) -> Color {
    rgb(t.0, t.1, t.2)
}

pub fn named(name: &str) -> (u8, u8, u8) {
    match name {
        "saffron" => SAFFRON,
        "violet" => VIOLET,
        "teal" => TEAL,
        "cyan" => CYAN,
        "green" => GREEN,
        "rose" => ROSE,
        "red" => RED,
        "amber" => AMBER,
        "blue" => BLUE,
        "gray" => GRAY,
        _ => TEXT,
    }
}

pub fn fg(t: (u8, u8, u8)) -> Style {
    Style::default().fg(c(t))
}
pub fn text() -> Style {
    fg(TEXT)
}
pub fn muted() -> Style {
    fg(MUTED)
}
pub fn dim() -> Style {
    fg(DIM)
}
pub fn faint() -> Style {
    fg(FAINT)
}
pub fn accent() -> Style {
    fg(SAFFRON)
}
pub fn bold(s: Style) -> Style {
    s.add_modifier(Modifier::BOLD)
}

/// Blend two colours (used for breathing/shimmer effects; degrades to a hard switch without truecolor).
pub fn mix(a: (u8, u8, u8), b: (u8, u8, u8), f: f32) -> Color {
    let f = f.clamp(0.0, 1.0);
    if depth() == Depth::C16 {
        return c(if f > 0.5 { b } else { a });
    }
    let l = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * f).round() as u8;
    rgb(l(a.0, b.0), l(a.1, b.1), l(a.2, b.2))
}

pub fn effort_bar(effort: &str, efforts: &[String]) -> String {
    let n = efforts.len().max(1);
    let i = efforts.iter().position(|e| e == effort).map(|i| i + 1).unwrap_or(1);
    let (on, off) = if ascii() { ("#", "-") } else { ("▰", "▱") };
    format!("{}{}", on.repeat(i), off.repeat(n.saturating_sub(i)))
}

pub fn gauge(pct: u8, width: usize) -> (String, String) {
    let filled = ((pct as usize * width) + 50) / 100;
    let (on, off) = if ascii() { ("=", ".") } else { ("━", "─") };
    (on.repeat(filled.min(width)), off.repeat(width.saturating_sub(filled)))
}

/// Average two palette colours (for dimmed/tinted variants).
pub fn mix_rgb(a: (u8, u8, u8), b: (u8, u8, u8)) -> (u8, u8, u8) {
    (((a.0 as u16 + b.0 as u16) / 2) as u8, ((a.1 as u16 + b.1 as u16) / 2) as u8, ((a.2 as u16 + b.2 as u16) / 2) as u8)
}

/// Best-effort RGB of a Color (for blending).
pub fn rgb_of(c: Color) -> (u8, u8, u8) {
    match c {
        Color::Rgb(r, g, b) => (r, g, b),
        Color::Red => RED,
        Color::Green => GREEN,
        Color::Yellow => AMBER,
        Color::Blue => BLUE,
        Color::Magenta => VIOLET,
        Color::Cyan => CYAN,
        Color::DarkGray => FAINT,
        Color::Gray => MUTED,
        _ => TEXT,
    }
}
