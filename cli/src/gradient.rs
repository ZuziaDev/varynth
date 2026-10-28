//! Bottom-left to top-right gradient used only by the first-run setup frame.

use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Gradient {
    pub from: (u8, u8, u8),
    pub to: (u8, u8, u8),
}

pub fn cell_color(g: Gradient, x: u16, y: u16, width: u16, height: u16) -> Color {
    let w = width.max(1);
    let h = height.max(1);
    let nx = if w == 1 {
        0.0
    } else {
        x.min(w - 1) as f32 / (w - 1) as f32
    };
    let ny = if h == 1 {
        0.0
    } else {
        y.min(h - 1) as f32 / (h - 1) as f32
    };
    let t = ((nx + (1.0 - ny)) / 2.0).clamp(0.0, 1.0);
    let mix = |a: u8, b: u8| -> u8 { (a as f32 + (b as f32 - a as f32) * t).round() as u8 };
    Color::Rgb(
        mix(g.from.0, g.to.0),
        mix(g.from.1, g.to.1),
        mix(g.from.2, g.to.2),
    )
}

/// Paint `area`. When `truecolor` is false, fill `fallback` and return false
/// so the caller can draw an accent rule (color is never the only signal).
pub fn fill(
    frame: &mut ratatui::Frame,
    area: Rect,
    g: Gradient,
    truecolor: bool,
    fallback: Color,
) -> bool {
    if area.width == 0 || area.height == 0 {
        return false;
    }
    if !truecolor {
        frame.render_widget(
            Paragraph::new("").style(Style::default().bg(fallback)),
            area,
        );
        return false;
    }
    let mut rows = Vec::with_capacity(area.height as usize);
    for row in 0..area.height {
        let mut spans = Vec::with_capacity(area.width as usize);
        for col in 0..area.width {
            let color = cell_color(g, col, row, area.width, area.height);
            spans.push(Span::styled(" ", Style::default().bg(color)));
        }
        rows.push(Line::from(spans));
    }
    frame.render_widget(Paragraph::new(rows), area);
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bottom_left_is_from_and_top_right_is_to() {
        let g = Gradient {
            from: (1, 2, 3),
            to: (201, 202, 203),
        };
        assert_eq!(cell_color(g, 0, 9, 10, 10), Color::Rgb(1, 2, 3));
        assert_eq!(cell_color(g, 9, 0, 10, 10), Color::Rgb(201, 202, 203));
        let mid = cell_color(g, 0, 0, 10, 10);
        assert!(matches!(mid, Color::Rgb(a, _, _) if a > 1 && a < 201));
    }

    #[test]
    fn single_cell_blends_from_and_to() {
        let g = Gradient {
            from: (9, 8, 7),
            to: (1, 1, 1),
        };
        assert_eq!(cell_color(g, 0, 0, 1, 1), Color::Rgb(5, 5, 4));
    }
}
