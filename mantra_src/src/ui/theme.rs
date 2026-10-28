//! Colours and glyphs with graceful fallback (truecolor → 256 → 16, Unicode → ASCII).

use ratatui::style::{Color, Modifier, Style};
use std::sync::RwLock;

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Depth {
    True,
    C256,
    C16,
}

#[derive(Clone, Copy)]
struct T {
    depth: Depth,
    ascii: bool,
    motion: bool,
    italic: bool,
}

/// Read on every style made, written by `init` — at startup and again when `/settings` changes
/// glyphs, colours or motion, so those apply without a restart.
static TH: RwLock<T> = RwLock::new(T { depth: Depth::C256, ascii: false, motion: true, italic: true });

/// Works out the terminal's colours and glyphs from the settings and the environment. Called
/// again after a change in `/settings`; the next frame is drawn with the new ones.
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
    let t = T { depth, ascii, motion: !s.reduce_motion, italic };
    match TH.write() {
        Ok(mut th) => *th = t,
        Err(poisoned) => *poisoned.into_inner() = t,
    }
}

fn th() -> T {
    *TH.read().unwrap_or_else(|e| e.into_inner())
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
        // anything else is left to the frame's ASCII pass (`ui::asciify`), which knows the pulse
        // and journal marks (✓ ✗ ↻ ⎇ ☰ …) — a "?" here would hide which line is which
        _ => s,
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

// Brandbook v1.1 core palette. Keep action/chrome colours separate from the named
// role/status palette below: an agent's configured colour remains its identity.
pub const MANTRA_RED: (u8, u8, u8) = (173, 22, 57); // #AD1639
pub const SIGNAL_RED: (u8, u8, u8) = (227, 58, 97); // #E33A61, dark-surface focus/chrome
pub const GRAPHITE: (u8, u8, u8) = (45, 46, 41); // #2D2E29
pub const CANVAS: (u8, u8, u8) = (239, 239, 235); // #EFEFEB
/// A Canvas tint of Signal Red for small text on Graphite and selected rows.
pub const ACCENT: (u8, u8, u8) = (245, 139, 163); // #F58BA3

// Role/status palette (RGB tuples so animations can blend).
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
pub const TEXT: (u8, u8, u8) = CANVAS;
pub const MUTED: (u8, u8, u8) = (198, 199, 188);
pub const DIM: (u8, u8, u8) = (168, 169, 159);
pub const FAINT: (u8, u8, u8) = (145, 146, 135);
/// The selected row's background in the config screens' lists and grids: dark enough for every
/// text colour to read on it, light enough to see on a dark terminal.
pub const BAND: (u8, u8, u8) = (73, 50, 56);

/// The selected-row band (`ui::form::band`). Sixteen colours have no shade dark enough to keep
/// faint text readable on, so there it is reverse video instead.
pub fn row_sel() -> Style {
    match depth() {
        Depth::C16 => Style::default().add_modifier(Modifier::REVERSED),
        // The generic cube quantizer makes BAND a much lighter grey, which hides
        // muted text. Use the dark red ANSI cube entry for this surface instead.
        Depth::C256 => Style::default().bg(Color::Indexed(52)),
        Depth::True => Style::default().bg(c(BAND)),
    }
}

pub fn c(t: (u8, u8, u8)) -> Color {
    if depth() == Depth::C16 {
        match t {
            MANTRA_RED => return Color::Red,
            SIGNAL_RED | ACCENT => return Color::LightRed,
            GRAPHITE => return Color::Black,
            CANVAS => return Color::White,
            MUTED | DIM => return Color::Gray,
            FAINT => return Color::DarkGray,
            _ => {}
        }
    }
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
    fg(ACCENT)
}

/// Canvas text on the primary brand red for actionable chips and prompts.
pub fn action() -> Style {
    text().bg(c(MANTRA_RED))
}

/// Paint windows consistently, including overlay clears. ANSI terminals use their
/// darkest standard background; truecolor uses the exact brand Graphite.
pub fn surface() -> Style {
    let bg = if depth() == Depth::C16 { Color::Black } else { c(GRAPHITE) };
    text().bg(bg)
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
