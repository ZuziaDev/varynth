//! Selectable terminal palettes. Purple is the default and the exported
//! constants preserve the palette used by callers without a render scope.

use ratatui::style::Color;
use std::cell::Cell;

pub const ACCENT: Color = Color::Rgb(126, 87, 194);
pub const ACCENT_NEON: Color = Color::Rgb(179, 136, 255);
pub const BG: Color = Color::Rgb(24, 20, 34);
pub const FG: Color = Color::Rgb(226, 220, 240);
pub const DIM: Color = Color::Rgb(122, 112, 152);
pub const OK: Color = Color::Rgb(129, 216, 178);
pub const WARN: Color = Color::Rgb(255, 158, 128);
pub const ERROR: Color = Color::Rgb(240, 128, 148);
pub const INFO: Color = Color::Rgb(128, 178, 235);

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Theme {
    #[default]
    Purple,
    Cyberpunk,
    Dark,
    Minimal,
}

impl Theme {
    pub const ALL: [Self; 4] = [Self::Purple, Self::Cyberpunk, Self::Dark, Self::Minimal];

    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|theme| theme.name().eq_ignore_ascii_case(name))
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Purple => "purple",
            Self::Cyberpunk => "cyberpunk",
            Self::Dark => "dark",
            Self::Minimal => "minimal",
        }
    }

    fn colors(self) -> [Color; 9] {
        match self {
            Self::Purple => [ACCENT, ACCENT_NEON, BG, FG, DIM, OK, WARN, ERROR, INFO],
            Self::Cyberpunk => [
                Color::Rgb(0, 198, 196),
                Color::Rgb(244, 234, 75),
                Color::Rgb(12, 17, 20),
                Color::Rgb(231, 245, 244),
                Color::Rgb(114, 141, 142),
                Color::Rgb(122, 220, 147),
                Color::Rgb(255, 178, 87),
                Color::Rgb(255, 99, 141),
                Color::Rgb(116, 190, 255),
            ],
            Self::Dark => [
                Color::Rgb(88, 161, 225),
                Color::Rgb(146, 198, 242),
                Color::Rgb(20, 22, 25),
                Color::Rgb(224, 226, 230),
                Color::Rgb(128, 132, 139),
                Color::Rgb(126, 198, 150),
                Color::Rgb(234, 186, 103),
                Color::Rgb(234, 128, 133),
                Color::Rgb(162, 185, 219),
            ],
            Self::Minimal => [
                Color::Rgb(180, 184, 190),
                Color::Rgb(248, 248, 248),
                Color::Rgb(12, 12, 12),
                Color::Rgb(224, 224, 224),
                Color::Rgb(112, 112, 112),
                Color::Rgb(154, 199, 167),
                Color::Rgb(205, 184, 143),
                Color::Rgb(219, 154, 154),
                Color::Rgb(171, 192, 214),
            ],
        }
    }
}

thread_local! {
    static ACTIVE: Cell<Theme> = const { Cell::new(Theme::Purple) };
}

/// A render-local theme scope. Restoring on drop also handles panicking draw
/// callbacks; parallel TestBackend tests never change one another's palette.
pub fn with_theme<T>(name: &str, draw: impl FnOnce() -> T) -> T {
    struct Restore(Theme);
    impl Drop for Restore {
        fn drop(&mut self) {
            ACTIVE.with(|active| active.set(self.0));
        }
    }
    let previous = ACTIVE.with(|active| active.replace(Theme::parse(name).unwrap_or_default()));
    let _restore = Restore(previous);
    draw()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Palette;

impl Palette {
    fn color(index: usize) -> Color {
        ACTIVE.with(|active| active.get().colors()[index])
    }
    pub fn accent() -> Color {
        Self::color(0)
    }
    pub fn accent_neon() -> Color {
        Self::color(1)
    }
    pub fn bg() -> Color {
        Self::color(2)
    }
    pub fn fg() -> Color {
        Self::color(3)
    }
    pub fn dim() -> Color {
        Self::color(4)
    }
    pub fn ok() -> Color {
        Self::color(5)
    }
    pub fn warn() -> Color {
        Self::color(6)
    }
    pub fn error() -> Color {
        Self::color(7)
    }
    pub fn info() -> Color {
        Self::color(8)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn palette_colors_are_distinct_and_not_default() {
        for theme in Theme::ALL {
            let all = theme.colors();
            for c in all {
                assert_ne!(c, Color::Reset);
            }
            for i in 0..all.len() {
                for j in i + 1..all.len() {
                    assert_ne!(all[i], all[j]);
                }
            }
        }
        assert_eq!(Palette::accent(), ACCENT);
    }

    #[test]
    fn selectable_palettes_restore_after_a_draw() {
        for theme in Theme::ALL {
            with_theme(theme.name(), || {
                assert_eq!(Palette::bg(), theme.colors()[2])
            });
            assert_eq!(Palette::bg(), BG);
        }
        assert_eq!(Theme::parse("Cyberpunk"), Some(Theme::Cyberpunk));
        assert_eq!(Theme::parse("unknown"), None);
    }
}
