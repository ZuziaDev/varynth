//! Terminal mark for the magenta V. The image asset lives under
//! `assets/brand`; this module only paints a bounded text stand-in.

use std::io::IsTerminal;
use std::path::{Path, PathBuf};

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::BorderType;

use crate::config::Config;

pub const ACCENT_HEX: &str = "#E91E8C";
pub const PURPLE_PRIMARY_HEX: &str = "#A855F7";
pub const PURPLE_GLOW_HEX: &str = "#C084FC";
pub const BLUE_INFO_HEX: &str = "#60A5FA";

pub const WORDMARK_GRADIENT_START: (u8, u8, u8) = (192, 132, 252); // #c084fc
pub const WORDMARK_GRADIENT_MID: (u8, u8, u8) = (168, 85, 247); // #a855f7
pub const WORDMARK_GRADIENT_END: (u8, u8, u8) = (96, 165, 250); // #60a5fa

pub const TAGLINE_SUBTITLE: &str =
    "Next-Gen Autonomous CLI Agent • Subagent Swarm • AST Patch Engine • Native MCP Runtime";
pub const TAGLINE_SUBTITLE_ASCII: &str =
    "Next-Gen Autonomous CLI Agent | Subagent Swarm | AST Patch Engine | Native MCP Runtime";

pub const BANNER_HEADER: &str = "╭─── VARYNTH AUTONOMOUS MULTIMODAL CLI AGENT ───╮";
pub const BANNER_HEADER_ASCII: &str = "+--- VARYNTH AUTONOMOUS MULTIMODAL CLI AGENT ---+";

pub fn accent_rgb() -> (u8, u8, u8) {
    (0xE9, 0x1E, 0x8C)
}

pub fn tagline_subtitle() -> &'static str {
    if ascii_only() {
        TAGLINE_SUBTITLE_ASCII
    } else {
        TAGLINE_SUBTITLE
    }
}

pub fn banner_header() -> &'static str {
    if ascii_only() {
        BANNER_HEADER_ASCII
    } else {
        BANNER_HEADER
    }
}

pub fn splash_path(home: &Path) -> PathBuf {
    home.join("splash.seen")
}

pub fn splash_once_path() -> PathBuf {
    splash_path(&Config::home_dir())
}

pub fn splash_seen() -> bool {
    splash_once_path().is_file()
}

pub fn mark_splash_seen() -> std::io::Result<()> {
    let path = splash_once_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, b"seen\n")
}

/// True when the process may paint color. `NO_COLOR` wins over a TTY.
pub fn color_allowed() -> bool {
    std::env::var_os("NO_COLOR").is_none()
}

pub fn stdout_is_tty() -> bool {
    std::io::stdout().is_terminal()
}

/// Skip splash on non-TTY, `--plain` callers, and `NO_COLOR`.
pub fn splash_allowed() -> bool {
    color_allowed() && stdout_is_tty()
}

pub fn ascii_mode() -> bool {
    std::env::var("VARYNTH_ASCII").ok().as_deref() == Some("1")
        || std::env::var("TERM").ok().as_deref() == Some("dumb")
}

/// Alias for [`ascii_mode`] used by TUI components and tests checking ASCII fallback.
pub fn ascii_only() -> bool {
    ascii_mode()
}

pub fn border_type() -> BorderType {
    if ascii_mode() {
        BorderType::Plain
    } else {
        BorderType::Rounded
    }
}

pub fn mark_char() -> &'static str {
    "V"
}

pub const MARK_W: u16 = 22;
pub const MARK_H: u16 = 10;

pub const WORDMARK_W: u16 = 61;
pub const WORDMARK_H: u16 = 6;

/// 6-line `VARYNTH` block-character wordmark from the reference design.
pub const WORDMARK_SOLID: [&str; 6] = [
    "██╗   ██╗ █████╗ ██████╗ ██╗   ██╗███╗   ██╗████████╗██╗  ██╗",
    "██║   ██║██╔══██╗██╔══██╗╚██╗ ██╔╝████╗  ██║╚══██╔══╝██║  ██║",
    "██║   ██║███████║██████╔╝ ╚████╔╝ ██╔██╗ ██║   ██║   ███████║",
    "╚██╗ ██╔╝██╔══██║██╔══██╗  ╚██╔╝  ██║╚██╗██║   ██║   ██╔══██║",
    " ╚████╔╝ ██║  ██║██║  ██║   ██║   ██║ ╚████║   ██║   ██║  ██║",
    "  ╚═══╝  ╚═╝  ╚═╝╚═╝  ╚═╝   ╚═╝   ╚═╝  ╚═══╝   ╚═╝   ╚═╝  ╚═╝",
];

/// ASCII-safe 6-line `VARYNTH` wordmark fallback when `ascii_only()` is active.
pub const WORDMARK_ASCII: [&str; 6] = [
    "##+   ##+ #####+ ######+ ##+   ##+###+   ##+########+##+  ##+",
    "##|   ##|##+==##+##+==##++##+ ##++####+  ##|+==##+==+##|  ##|",
    "##|   ##|#######|######++ +####++ ##+##+ ##|   ##|   #######|",
    "+##+ ##++##+==##|##+==##+  +##++  ##|+#####|   ##|   ##+==##|",
    " +####++ ##|  ##|##|  ##|   ##|   ##| +####|   ##|   ##|  ##|",
    "  +===+  +=+  +=++=+  +=+   +=+   +=+  +===+   +=+   +=+  +=+",
];

/// Filled V sampled from `assets/brand/varynth-mark.png`. Digits are shades, spaces stay clear.
pub const MARK_SOLID: &[&str] = &[
    "2223              3344",
    "233343           34432",
    " 343223        443222 ",
    "  223344       32233  ",
    "   344323    222333   ",
    "     23334  223344    ",
    "      3432233344      ",
    "       223334322      ",
    "        344322        ",
    "         3223         ",
];

/// ASCII silhouette fallback for the downward V mark.
pub const MARK_ASCII: &[&str] = &[
    "####              ####",
    "######           #####",
    " ######        ###### ",
    "  ######       #####  ",
    "   ######    ######   ",
    "     #####  ######    ",
    "      ##########      ",
    "       #########      ",
    "        ######        ",
    "         ####         ",
];

/// Horizontal 3-stop gradient (`#c084fc` -> `#a855f7` -> `#60a5fa`) across `width` columns.
pub fn wordmark_gradient_color(col: usize, width: usize) -> Color {
    let max_col = width.saturating_sub(1);
    if max_col == 0 {
        let (r, g, b) = WORDMARK_GRADIENT_START;
        return Color::Rgb(r, g, b);
    }
    let t = (col.min(max_col) as f32 / max_col as f32).clamp(0.0, 1.0);
    let mix = |a: u8, b: u8, local_t: f32| -> u8 {
        (a as f32 + (b as f32 - a as f32) * local_t).round() as u8
    };
    let (from, to, local_t) = if t <= 0.5 {
        (WORDMARK_GRADIENT_START, WORDMARK_GRADIENT_MID, t / 0.5)
    } else {
        (
            WORDMARK_GRADIENT_MID,
            WORDMARK_GRADIENT_END,
            (t - 0.5) / 0.5,
        )
    };
    Color::Rgb(
        mix(from.0, to.0, local_t),
        mix(from.1, to.1, local_t),
        mix(from.2, to.2, local_t),
    )
}

/// Render the 6-line `VARYNTH` wordmark with a horizontal gradient when `truecolor`
/// and Unicode are allowed, or a flat accent / ASCII fallback otherwise.
pub fn wordmark_lines(truecolor: bool) -> Vec<Line<'static>> {
    let is_ascii = ascii_only();
    let allow_color = color_allowed();
    let use_gradient = truecolor && allow_color && !is_ascii;
    let source = if is_ascii {
        &WORDMARK_ASCII
    } else {
        &WORDMARK_SOLID
    };
    let flat_style = if allow_color {
        Style::default()
            .fg(crate::theme::Palette::accent())
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };

    source
        .iter()
        .map(|row| {
            if !use_gradient {
                return Line::from(Span::styled((*row).to_string(), flat_style));
            }
            let chars: Vec<char> = row.chars().collect();
            let width = chars.len();
            let spans: Vec<Span<'static>> = chars
                .into_iter()
                .enumerate()
                .map(|(col, ch)| {
                    if ch == ' ' {
                        Span::raw(" ")
                    } else {
                        let color = wordmark_gradient_color(col, width);
                        Span::styled(
                            ch.to_string(),
                            Style::default().fg(color).add_modifier(Modifier::BOLD),
                        )
                    }
                })
                .collect();
            Line::from(spans)
        })
        .collect()
}

/// Paint the 6-line `VARYNTH` wordmark using the active theme's truecolor capability.
/// Falls back to flat accent / `WORDMARK_ASCII` when `ascii_only()` is active or in `Safe` theme.
pub fn paint_wordmark() -> Vec<Line<'static>> {
    wordmark_lines(crate::theme::active_truecolor())
}

fn shade(ch: char) -> Option<(u8, u8, u8)> {
    match ch {
        '1' => Some((0x9B, 0x15, 0x58)),
        '2' => Some((0xC4, 0x18, 0x72)),
        '3' => Some((0xE9, 0x1E, 0x8C)),
        '4' => Some((0xFF, 0x6B, 0xB5)),
        '5' => Some((0xFF, 0xC2, 0xE0)),
        _ => None,
    }
}

/// Downward filled V, 10 rows by 22 columns. Color paints cells from the logo;
/// otherwise the same silhouette is `#`.
pub fn mark_lines(width: u16, truecolor: bool) -> Vec<Line<'static>> {
    let _ = width;
    let paint = truecolor && color_allowed() && !ascii_mode();
    MARK_SOLID
        .iter()
        .map(|row| {
            let mut spans = Vec::new();
            let mut chars = row.chars().peekable();
            while let Some(ch) = chars.next() {
                let solid = shade(ch).is_some();
                let mut run = String::from(if paint || !solid { ' ' } else { '#' });
                let tone = shade(ch);
                while chars.peek().copied().is_some_and(|next| {
                    shade(next).is_some() == solid && (!paint || shade(next) == tone)
                }) {
                    let next = chars.next().unwrap_or(' ');
                    run.push(if paint || shade(next).is_none() {
                        ' '
                    } else {
                        '#'
                    });
                }
                if let Some((r, g, b)) = tone.filter(|_| paint) {
                    let ink = Color::Rgb(r, g, b);
                    spans.push(Span::styled(run, Style::default().fg(ink).bg(ink)));
                } else {
                    spans.push(Span::raw(run));
                }
            }
            Line::from(spans)
        })
        .collect()
}

/// Stamp the logo onto existing cells. Empty cells are left alone, so a gradient underneath stays.
pub fn paint_mark(frame: &mut ratatui::Frame, x: u16, y: u16, truecolor: bool) {
    let paint = truecolor && color_allowed() && !ascii_mode();
    let area = frame.area();
    let buf = frame.buffer_mut();
    for (dy, row) in MARK_SOLID.iter().enumerate() {
        for (dx, ch) in row.chars().enumerate() {
            let cx = x.saturating_add(dx as u16);
            let cy = y.saturating_add(dy as u16);
            if cx >= area.width || cy >= area.height {
                continue;
            }
            if paint {
                if let Some((r, g, b)) = shade(ch) {
                    let ink = Color::Rgb(r, g, b);
                    buf[(cx, cy)]
                        .set_symbol("█")
                        .set_style(Style::default().fg(ink).bg(ink));
                }
            } else if shade(ch).is_some() {
                buf[(cx, cy)].set_symbol("#");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mark_stays_inside_the_splash_budget() {
        for line in mark_lines(80, true) {
            let text: String = line
                .spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect();
            assert!(text.chars().count() <= 22, "{text}");
        }
        assert!(mark_lines(80, true).len() <= 11);
        let ascii = mark_lines(40, false);
        let flat: String = ascii
            .iter()
            .flat_map(|line| line.spans.iter().map(|span| span.content.clone()))
            .collect();
        assert!(flat.contains('#'));
    }

    #[test]
    fn splash_path_is_under_the_home_dir() {
        assert_eq!(
            splash_path(Path::new("/tmp/varynth-home")),
            PathBuf::from("/tmp/varynth-home/splash.seen")
        );
    }

    #[test]
    fn wordmark_has_six_lines_and_three_stop_gradient() {
        assert_eq!(WORDMARK_SOLID.len(), 6);
        assert_eq!(WORDMARK_ASCII.len(), 6);
        for (solid, ascii) in WORDMARK_SOLID.iter().zip(WORDMARK_ASCII.iter()) {
            assert_eq!(solid.chars().count(), WORDMARK_W as usize);
            assert_eq!(ascii.chars().count(), WORDMARK_W as usize);
            assert!(ascii.is_ascii());
        }
        assert_eq!(wordmark_gradient_color(0, 61), Color::Rgb(192, 132, 252));
        assert_eq!(wordmark_gradient_color(30, 61), Color::Rgb(168, 85, 247));
        assert_eq!(wordmark_gradient_color(60, 61), Color::Rgb(96, 165, 250));

        let lines = wordmark_lines(true);
        assert_eq!(lines.len(), 6);
        let safe_lines = crate::theme::with_theme("safe", paint_wordmark);
        assert_eq!(safe_lines.len(), 6);
        assert!(TAGLINE_SUBTITLE.contains("Subagent Swarm"));
        assert!(TAGLINE_SUBTITLE_ASCII.is_ascii());
    }
}
