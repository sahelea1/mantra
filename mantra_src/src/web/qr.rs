//! QR codes for the remote link: the module matrix (sent to the web UI as '0'/'1' rows, which it
//! draws on a canvas) and a half-block rendering for the TUI's `/remote` overlay.

use qrcode::{Color, EcLevel, QrCode};

/// The link's QR modules, `true` = dark, without the quiet zone. `None` if the data is too long
/// for any QR version (a link never is).
pub fn matrix(data: &str) -> Option<Vec<Vec<bool>>> {
    let code = QrCode::with_error_correction_level(data.as_bytes(), EcLevel::M).ok()?;
    let w = code.width();
    let colors = code.to_colors();
    Some(colors.chunks(w).map(|row| row.iter().map(|c| *c == Color::Dark).collect()).collect())
}

/// Rows of '0'/'1' (the protocol's `RemoteInfo.qr`).
pub fn rows01(m: &[Vec<bool>]) -> Vec<String> {
    m.iter().map(|r| r.iter().map(|d| if *d { '1' } else { '0' }).collect()).collect()
}

/// Terminal rendering: two module rows per text row using `▀▄█`, with a `quiet`-module border.
/// Filled cells are *light* modules — the caller draws them light-on-dark (fg white, bg black) so
/// the code scans as dark-on-light on any terminal theme.
pub fn half_blocks(m: &[Vec<bool>], quiet: usize) -> Vec<String> {
    let n = m.len();
    let size = n + 2 * quiet;
    let light = |y: usize, x: usize| -> bool {
        if y < quiet || x < quiet || y >= quiet + n || x >= quiet + n {
            return true;
        }
        !m[y - quiet][x - quiet]
    };
    (0..size)
        .step_by(2)
        .map(|y| {
            (0..size)
                .map(|x| {
                    let top = light(y, x);
                    let bottom = if y + 1 < size { light(y + 1, x) } else { false };
                    match (top, bottom) {
                        (true, true) => '█',
                        (true, false) => '▀',
                        (false, true) => '▄',
                        (false, false) => ' ',
                    }
                })
                .collect()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn finder_at(m: &[Vec<bool>], y: usize, x: usize) -> bool {
        // 7×7: dark ring, light ring, 3×3 dark centre
        (0..7).all(|i| (0..7).all(|j| {
            let ring = i == 0 || i == 6 || j == 0 || j == 6;
            let inner = (2..=4).contains(&i) && (2..=4).contains(&j);
            m[y + i][x + j] == (ring || inner)
        }))
    }

    #[test]
    fn a_link_makes_a_square_code_with_three_finder_patterns() {
        let link = "https://remote.mantra.codes/s/abcdefghijklmnopqrstuvwxyz#k=AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8";
        let m = matrix(link).unwrap();
        let n = m.len();
        assert!(m.iter().all(|r| r.len() == n), "square");
        assert!(n >= 21 && (n - 17) % 4 == 0, "a valid QR size, got {n}");
        assert!(finder_at(&m, 0, 0) && finder_at(&m, 0, n - 7) && finder_at(&m, n - 7, 0));
        let rows = rows01(&m);
        assert_eq!(rows.len(), n);
        assert!(rows[0].starts_with("1111111"));
        let hb = half_blocks(&m, 2);
        assert_eq!(hb.len(), (n + 4).div_ceil(2));
        assert!(hb.iter().all(|r| r.chars().count() == n + 4));
        assert!(hb[0].chars().all(|c| c == '█'), "the quiet zone is light");
    }
}
