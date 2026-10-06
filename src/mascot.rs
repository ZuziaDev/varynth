//! Varynth's mascot: a small octopus.
//!
//! The header shows the resting pose. A click plays one random trick
//! (spin, arms, blink) and then returns to that pose. Every frame is
//! 13 columns wide and 7 lines tall so the header text never shifts.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

pub const TEAL: Color = Color::Rgb(232, 255, 71);
pub const CORAL: Color = Color::Rgb(242, 163, 101);
pub const DIM: Color = Color::Rgb(142, 138, 128);
pub const LIME: Color = Color::Rgb(232, 255, 71);
const EYE: Color = Color::Rgb(231, 228, 220);

pub const GLYPH_WIDTH: usize = 13;
pub const GLYPH_HEIGHT: usize = 7;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Trick {
    Spin,
    Arms,
    Blink,
}

const REST: [&str; 7] = [
    "   ▄█████▄   ",
    " ▄█████████▄ ",
    " ▐███ █ ███▌ ",
    "  ▜███████▛  ",
    " ▗▛ ▐▌ ▐▌ ▜▖ ",
    " ▐▌ ▐▌ ▐▌ ▐▌ ",
    " ▝▀ ▝▀ ▀▘ ▀▘ ",
];

const SPIN: [[&str; 7]; 4] = [
    REST,
    [
        "   ▄█████▄   ",
        " ▄███▀███▀██ ",
        " ▐██  █  ██▌ ",
        "  ▜██▄█▄██▛  ",
        " ▝▀ ▐▌ ▐▌ ▀▘ ",
        " ▐▌ ▜▖ ▗▛ ▐▌ ",
        " ▗▛ ▝▀ ▀▘ ▜▖ ",
    ],
    [
        "   ▄█████▄   ",
        " ▄█████████▄ ",
        " ▐█▀▀ █ ▀▀█▌ ",
        "  ▜███████▛  ",
        " ▐▌ ▝▀ ▀▘ ▐▌ ",
        " ▗▛ ▐▌ ▐▌ ▜▖ ",
        " ▝▀ ▜▖ ▗▛ ▀▘ ",
    ],
    [
        "   ▄█████▄   ",
        " ▄██▀███▀██▄ ",
        " ▐██  █  ██▌ ",
        "  ▜██▀█▀██▛  ",
        " ▗▛ ▜▖ ▗▛ ▜▖ ",
        " ▐▌ ▝▀ ▀▘ ▐▌ ",
        " ▝▀ ▐▌ ▐▌ ▀▘ ",
    ],
];

const ARMS: [[&str; 7]; 4] = [
    REST,
    [
        "   ▄█████▄   ",
        " ▄█████████▄ ",
        " ▐███ █ ███▌ ",
        "  ▜███████▛  ",
        " ▗▛ ▐▌ ▐▌ ▜▖ ",
        " ▜▖ ▐▌ ▐▌ ▗▛ ",
        " ▝  ▝▀ ▀▘  ▘ ",
    ],
    [
        "   ▄█████▄   ",
        " ▄█████████▄ ",
        " ▐███ █ ███▌ ",
        "  ▜███████▛  ",
        " ▝  ▐▌ ▐▌  ▘ ",
        " ▐▌ ▜▖ ▗▛ ▐▌ ",
        " ▗▛ ▝▀ ▀▘ ▜▖ ",
    ],
    [
        "   ▄█████▄   ",
        " ▄█████████▄ ",
        " ▐███ █ ███▌ ",
        "  ▜███████▛  ",
        " ▗▛ ▜▖ ▗▛ ▜▖ ",
        " ▐▌ ▐▌ ▐▌ ▐▌ ",
        " ▝▀ ▝  ▘ ▀▘  ",
    ],
];

const BLINK: [[&str; 7]; 4] = [
    REST,
    [
        "   ▄█████▄   ",
        " ▄█████████▄ ",
        " ▐██─ █ ─██▌ ",
        "  ▜███████▛  ",
        " ▗▛ ▐▌ ▐▌ ▜▖ ",
        " ▐▌ ▐▌ ▐▌ ▐▌ ",
        " ▝▀ ▝▀ ▀▘ ▀▘ ",
    ],
    [
        "   ▄█████▄   ",
        " ▄█████████▄ ",
        " ▐██▄ █ ▄██▌ ",
        "  ▜███████▛  ",
        " ▗▛ ▐▌ ▐▌ ▜▖ ",
        " ▐▌ ▐▌ ▐▌ ▐▌ ",
        " ▝▀ ▝▀ ▀▘ ▀▘ ",
    ],
    REST,
];

/// Click dance length. The last step is the resting pose.
pub const DANCE_LEN: usize = 4;

/// `kind` 0 spin, 1 arms, 2 blink. `step` is 0..DANCE_LEN.
pub fn dance(kind: usize, step: usize) -> [Line<'static>; GLYPH_HEIGHT] {
    let trick = match kind % 3 {
        0 => Trick::Spin,
        1 => Trick::Arms,
        _ => Trick::Blink,
    };
    glyph(Some((trick, step)), false)
}

pub fn trick_from_seed(seed: u64) -> Trick {
    match seed % 3 {
        0 => Trick::Spin,
        1 => Trick::Arms,
        _ => Trick::Blink,
    }
}

/// Header glyph. `trick_frame` is `Some((trick, frame))` while a click
/// animation is playing. `None` is the resting octopus; `busy` only
/// changes the eye color.
pub fn glyph(trick_frame: Option<(Trick, usize)>, busy: bool) -> [Line<'static>; GLYPH_HEIGHT] {
    let rows = match trick_frame {
        Some((Trick::Spin, frame)) => SPIN[frame % SPIN.len()],
        Some((Trick::Arms, frame)) => ARMS[frame % ARMS.len()],
        Some((Trick::Blink, frame)) => BLINK[frame % BLINK.len()],
        None => REST,
    };
    std::array::from_fn(|i| colorize(rows[i], busy))
}

pub fn splash(tick: usize) -> Vec<Line<'static>> {
    let t = tick.min(8);
    let mut lines = vec![Line::from("")];
    let shown = t.min(GLYPH_HEIGHT);
    for row in REST.iter().take(shown) {
        let row = if *row == REST[2] && t < 4 {
            " ▐██─ █ ─██▌ "
        } else {
            *row
        };
        lines.push(colorize(row, false));
    }
    if t >= 7 {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "    varynth",
            Style::default().fg(EYE).add_modifier(Modifier::BOLD),
        )));
    }
    lines
}

fn colorize(s: &str, busy: bool) -> Line<'static> {
    let body = if busy { TEAL } else { CORAL };
    let spans: Vec<Span> = s
        .chars()
        .map(|ch| {
            let style = match ch {
                '─' => Style::default().fg(if busy { LIME } else { EYE }),
                '█' | '▄' | '▀' | '▐' | '▌' | '▜' | '▛' | '▗' | '▖' | '▝' | '▘' => {
                    Style::default().fg(body)
                }
                _ => Style::default().fg(DIM),
            };
            Span::styled(ch.to_string(), style)
        })
        .collect();
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(l: &Line) -> String {
        l.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn every_frame_is_fixed_size() {
        let frames = [
            None,
            Some((Trick::Spin, 0)),
            Some((Trick::Arms, 2)),
            Some((Trick::Blink, 1)),
        ];
        for frame in frames {
            let g = glyph(frame, false);
            assert_eq!(g.len(), GLYPH_HEIGHT);
            for line in g {
                assert_eq!(text(&line).chars().count(), GLYPH_WIDTH);
            }
        }
    }

    #[test]
    fn tricks_leave_the_rest_pose() {
        let rest = glyph(None, false).map(|l| text(&l));
        assert_ne!(glyph(Some((Trick::Spin, 1)), false).map(|l| text(&l)), rest);
        assert_ne!(glyph(Some((Trick::Arms, 1)), false).map(|l| text(&l)), rest);
        assert_eq!(
            glyph(Some((Trick::Blink, 3)), false).map(|l| text(&l)),
            rest
        );
    }

    #[test]
    fn splash_names_varynth_only() {
        assert!(splash(0).len() <= splash(8).len());
        let all: String = splash(8).iter().map(text).collect();
        assert!(all.contains("varynth"));
        for brand in ["OpenClaw", "Claude", "Codex"] {
            assert!(!all.contains(brand), "{brand}");
        }
    }
}
