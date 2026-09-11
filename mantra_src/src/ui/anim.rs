//! Tiny animation helpers. All animations are pure functions of time, so an idle UI costs nothing.

use super::theme::{self, mix};
use ratatui::style::{Modifier, Style};
use ratatui::text::Span;
use std::sync::OnceLock;
use std::time::Instant;

static T0: OnceLock<Instant> = OnceLock::new();

pub fn ms() -> u128 {
    T0.get_or_init(Instant::now).elapsed().as_millis()
}

pub fn spinner() -> &'static str {
    if theme::ascii() {
        const F: [&str; 4] = ["|", "/", "-", "\\"];
        return F[(ms() / 120) as usize % 4];
    }
    const F: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
    if !theme::motion() {
        return "⠿";
    }
    F[(ms() / 80) as usize % 10]
}

/// 0..1..0 over `period` ms.
pub fn breath(period: u128) -> f32 {
    if !theme::motion() {
        return 0.6;
    }
    let p = (ms() % period) as f32 / period as f32;
    0.5 - 0.5 * (p * std::f32::consts::TAU).cos()
}

/// Text with a bright band sweeping across it.
pub fn shimmer(text: &str, base: (u8, u8, u8), hi: (u8, u8, u8)) -> Vec<Span<'static>> {
    let chars: Vec<char> = text.chars().collect();
    if !theme::motion() || chars.is_empty() {
        return vec![Span::styled(text.to_string(), theme::fg(base))];
    }
    let n = chars.len() as f32;
    let span = n + 8.0;
    let pos = ((ms() % 1800) as f32 / 1800.0) * span - 4.0;
    chars
        .iter()
        .enumerate()
        .map(|(i, ch)| {
            let d = (i as f32 - pos).abs();
            let f = (1.0 - d / 3.5).max(0.0);
            let mut st = Style::default().fg(mix(base, hi, f));
            if theme::depth() == theme::Depth::C16 && f > 0.5 {
                st = st.add_modifier(Modifier::BOLD);
            }
            Span::styled(ch.to_string(), st)
        })
        .collect()
}

/// Phase for marching dashes along an edge.
pub fn march(speed_ms: u128) -> usize {
    if !theme::motion() {
        return 0;
    }
    (ms() / speed_ms) as usize
}

/// 1.0 right after `t`, fading to 0 over `dur_ms`.
pub fn fade(t: Option<Instant>, dur_ms: u128) -> f32 {
    match t {
        Some(t) => {
            let e = t.elapsed().as_millis();
            if e >= dur_ms {
                0.0
            } else {
                1.0 - e as f32 / dur_ms as f32
            }
        }
        None => 0.0,
    }
}
