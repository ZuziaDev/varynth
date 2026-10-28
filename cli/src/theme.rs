//! Named terminal palettes plus `custom:<name>` role files.
//!
//! `purple`, `cyberpunk`, and `minimal` still load; they canonicalize to
//! `magenta`, `dark`, and `safe` and are rewritten on the next save.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use ratatui::style::{Color, Style};
use serde_json::{json, Value};
use std::cell::Cell;

use crate::config::{valid_profile_id, Config};

// Deep obsidian backgrounds
pub const BG: Color = Color::Rgb(0x08, 0x08, 0x0C);
pub const BG_OBSIDIAN: Color = Color::Rgb(0x08, 0x08, 0x0C);
pub const BG_PANEL: Color = Color::Rgb(0x0D, 0x0D, 0x14);
pub const BG_ELEVATED: Color = Color::Rgb(0x13, 0x13, 0x1F);
pub const BG_INPUT: Color = Color::Rgb(0x0B, 0x0B, 0x12);

// Electric purple primary, deep purple, neon purple glow
pub const ACCENT: Color = Color::Rgb(0xA8, 0x55, 0xF7);
pub const PURPLE_PRIMARY: Color = Color::Rgb(0xA8, 0x55, 0xF7);
pub const ACCENT_DEEP: Color = Color::Rgb(0x93, 0x33, 0xEA);
pub const PURPLE_DEEP: Color = Color::Rgb(0x93, 0x33, 0xEA);
pub const ACCENT_NEON: Color = Color::Rgb(0xC0, 0x84, 0xFC);
pub const PURPLE_GLOW: Color = Color::Rgb(0xC0, 0x84, 0xFC);

// Foreground and muted text
pub const FG: Color = Color::Rgb(0xE2, 0xE8, 0xF0);
pub const DIM: Color = Color::Rgb(0x94, 0xA3, 0xB8);

// Emerald OK
pub const OK: Color = Color::Rgb(0x10, 0xB9, 0x81);
pub const EMERALD_OK: Color = Color::Rgb(0x10, 0xB9, 0x81);
pub const OK_LIGHT: Color = Color::Rgb(0x6E, 0xE7, 0xB7);
pub const EMERALD_LIGHT: Color = Color::Rgb(0x6E, 0xE7, 0xB7);

// Amber warning / skill
pub const WARN: Color = Color::Rgb(0xF5, 0x9E, 0x0B);
pub const AMBER_WARN: Color = Color::Rgb(0xF5, 0x9E, 0x0B);
pub const WARN_LIGHT: Color = Color::Rgb(0xFC, 0xD3, 0x4D);
pub const AMBER_LIGHT: Color = Color::Rgb(0xFC, 0xD3, 0x4D);

// Crimson error / diff-del
pub const ERROR: Color = Color::Rgb(0xEF, 0x44, 0x44);
pub const CRIMSON_ERR: Color = Color::Rgb(0xEF, 0x44, 0x44);
pub const ERROR_LIGHT: Color = Color::Rgb(0xFC, 0xA5, 0xA5);
pub const CRIMSON_LIGHT: Color = Color::Rgb(0xFC, 0xA5, 0xA5);

// Info blue
pub const INFO: Color = Color::Rgb(0x3B, 0x82, 0xF6);
pub const BLUE_INFO: Color = Color::Rgb(0x3B, 0x82, 0xF6);
pub const INFO_BRIGHT: Color = Color::Rgb(0x60, 0xA5, 0xFA);
pub const BLUE_BRIGHT: Color = Color::Rgb(0x60, 0xA5, 0xFA);
pub const INFO_LIGHT: Color = Color::Rgb(0x93, 0xC5, 0xFD);
pub const BLUE_LIGHT: Color = Color::Rgb(0x93, 0xC5, 0xFD);

// Subtle and purple-tinted borders
pub const BORDER: Color = Color::Rgb(0x26, 0x26, 0x3A);
pub const BORDER_SUBTLE: Color = Color::Rgb(0x1E, 0x1E, 0x2D);
pub const BORDER_PURPLE: Color = Color::Rgb(0x4C, 0x28, 0x78);

// Semantic pill tokens
pub const PILL_AGENT_FG: Color = Color::Rgb(0xD8, 0xB4, 0xFE);
pub const PILL_AGENT_BG: Color = Color::Rgb(0x24, 0x14, 0x38);
pub const PILL_FILE_FG: Color = Color::Rgb(0x93, 0xC5, 0xFD);
pub const PILL_FILE_BG: Color = Color::Rgb(0x10, 0x20, 0x3B);
pub const PILL_SKILL_FG: Color = Color::Rgb(0xFC, 0xD3, 0x4D);
pub const PILL_SKILL_BG: Color = Color::Rgb(0x2E, 0x21, 0x0B);
pub const PILL_CMD_FG: Color = Color::Rgb(0x6E, 0xE7, 0xB7);
pub const PILL_CMD_BG: Color = Color::Rgb(0x0B, 0x29, 0x20);

// Unified diff line tokens
pub const DIFF_ADD_FG: Color = Color::Rgb(0x6E, 0xE7, 0xB7);
pub const DIFF_ADD_BG: Color = Color::Rgb(0x09, 0x22, 0x1B);
pub const DIFF_ADD_BAR: Color = Color::Rgb(0x10, 0xB9, 0x81);
pub const DIFF_DEL_FG: Color = Color::Rgb(0xFC, 0xA5, 0xA5);
pub const DIFF_DEL_BG: Color = Color::Rgb(0x28, 0x11, 0x16);
pub const DIFF_DEL_BAR: Color = Color::Rgb(0xEF, 0x44, 0x44);
pub const DIFF_CTX_FG: Color = Color::Rgb(0x94, 0xA3, 0xB8);

const REQUIRED_ROLES: [&str; 10] = [
    "accent",
    "accent_neon",
    "bg",
    "fg",
    "muted",
    "success",
    "warning",
    "error",
    "info",
    "border",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Roles {
    pub accent: Color,
    pub accent_neon: Color,
    pub bg: Color,
    pub fg: Color,
    pub muted: Color,
    pub success: Color,
    pub warning: Color,
    pub error: Color,
    pub info: Color,
    pub border: Color,
    pub truecolor: bool,
    pub gradient_from: (u8, u8, u8),
    pub gradient_to: (u8, u8, u8),
}

const MAGENTA_ROLES: Roles = Roles {
    accent: ACCENT,
    accent_neon: ACCENT_NEON,
    bg: BG,
    fg: FG,
    muted: DIM,
    success: OK,
    warning: WARN,
    error: ERROR,
    info: INFO,
    border: BORDER,
    truecolor: true,
    gradient_from: (0xA8, 0x55, 0xF7),
    gradient_to: (0x08, 0x08, 0x0C),
};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Theme {
    #[default]
    Magenta,
    System,
    Dark,
    Light,
    Safe,
}

impl Theme {
    pub const ALL: [Self; 5] = [
        Self::Magenta,
        Self::System,
        Self::Dark,
        Self::Light,
        Self::Safe,
    ];

    pub fn parse(name: &str) -> Option<Self> {
        match canonical(name).as_str() {
            "magenta" => Some(Self::Magenta),
            "system" => Some(Self::System),
            "dark" => Some(Self::Dark),
            "light" => Some(Self::Light),
            "safe" => Some(Self::Safe),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Magenta => "magenta",
            Self::System => "system",
            Self::Dark => "dark",
            Self::Light => "light",
            Self::Safe => "safe",
        }
    }

    pub fn roles(self) -> Roles {
        match self {
            Self::Magenta => MAGENTA_ROLES,
            Self::System => Roles {
                accent: Color::Magenta,
                accent_neon: Color::LightMagenta,
                bg: Color::Reset,
                fg: Color::White,
                muted: Color::DarkGray,
                success: Color::Green,
                warning: Color::Yellow,
                error: Color::Red,
                info: Color::Cyan,
                border: Color::Gray,
                truecolor: false,
                gradient_from: (0xE9, 0x1E, 0x8C),
                gradient_to: (0, 0, 0),
            },
            Self::Dark => Roles {
                accent: Color::Rgb(88, 161, 225),
                accent_neon: Color::Rgb(146, 198, 242),
                bg: Color::Rgb(20, 22, 25),
                fg: Color::Rgb(224, 226, 230),
                muted: Color::Rgb(128, 132, 139),
                success: Color::Rgb(126, 198, 150),
                warning: Color::Rgb(234, 186, 103),
                error: Color::Rgb(234, 128, 133),
                info: Color::Rgb(162, 185, 219),
                border: Color::Rgb(44, 49, 56),
                truecolor: true,
                gradient_from: (88, 161, 225),
                gradient_to: (20, 22, 25),
            },
            Self::Light => Roles {
                accent: ACCENT,
                accent_neon: Color::Rgb(194, 24, 106),
                bg: Color::Rgb(247, 242, 246),
                fg: Color::Rgb(28, 18, 24),
                muted: Color::Rgb(110, 92, 104),
                success: Color::Rgb(31, 122, 77),
                warning: Color::Rgb(138, 90, 0),
                error: Color::Rgb(163, 32, 58),
                info: Color::Rgb(29, 78, 137),
                border: Color::Rgb(217, 201, 212),
                truecolor: true,
                gradient_from: (0xE9, 0x1E, 0x8C),
                gradient_to: (247, 242, 246),
            },
            Self::Safe => Roles {
                accent: Color::Magenta,
                accent_neon: Color::LightMagenta,
                bg: Color::Black,
                fg: Color::White,
                muted: Color::DarkGray,
                success: Color::Green,
                warning: Color::Yellow,
                error: Color::Red,
                info: Color::Blue,
                border: Color::Gray,
                truecolor: false,
                gradient_from: (0xE9, 0x1E, 0x8C),
                gradient_to: (0, 0, 0),
            },
        }
    }
}

#[derive(Clone, Copy)]
struct ActiveTheme(Roles);

thread_local! {
    static ACTIVE: Cell<ActiveTheme> = const { Cell::new(ActiveTheme(MAGENTA_ROLES)) };
}

pub fn with_theme<T>(name: &str, draw: impl FnOnce() -> T) -> T {
    struct Restore(ActiveTheme);
    impl Drop for Restore {
        fn drop(&mut self) {
            ACTIVE.with(|active| active.set(self.0));
        }
    }
    let resolved = resolve(name).unwrap_or(MAGENTA_ROLES);
    let previous = ACTIVE.with(|active| active.replace(ActiveTheme(resolved)));
    let _restore = Restore(previous);
    draw()
}

pub fn canonical(name: &str) -> String {
    let trimmed = name.trim();
    let lower = trimmed.to_ascii_lowercase();
    match lower.as_str() {
        "purple" => "magenta".into(),
        "cyberpunk" => "dark".into(),
        "minimal" => "safe".into(),
        "magenta" | "system" | "dark" | "light" | "safe" => lower,
        _ => {
            if let Some(slug) = lower.strip_prefix("custom:") {
                if valid_slug(slug) {
                    format!("custom:{}", slug.to_ascii_lowercase())
                } else {
                    trimmed.to_string()
                }
            } else {
                trimmed.to_string()
            }
        }
    }
}

pub fn ensure_valid(name: &str) -> Result<()> {
    let name = canonical(name);
    if Theme::parse(&name).is_some() {
        return Ok(());
    }
    if let Some(slug) = name.strip_prefix("custom:") {
        if !valid_slug(slug) {
            bail!("!! theme `{name}` has an invalid name\ntry: varynth theme list");
        }
        load_custom(slug).with_context(|| format!("!! theme {name}"))?;
        return Ok(());
    }
    bail!("!! invalid theme `{name}`\ntry: varynth theme list");
}

pub fn resolve(name: &str) -> Result<Roles> {
    let name = canonical(name);
    if let Some(theme) = Theme::parse(&name) {
        return Ok(theme.roles());
    }
    if let Some(slug) = name.strip_prefix("custom:") {
        return Ok(load_custom(slug)?.0);
    }
    bail!("!! invalid theme `{name}`\ntry: varynth theme list");
}

pub fn choices() -> Vec<String> {
    let mut names: Vec<String> = Theme::ALL
        .iter()
        .map(|theme| theme.name().to_string())
        .collect();
    names.extend(discover().into_iter().map(|slug| format!("custom:{slug}")));
    names
}

pub fn themes_dir() -> PathBuf {
    Config::home_dir().join("themes")
}

pub fn custom_path(slug: &str) -> Result<PathBuf> {
    let slug = normalize_slug(slug)?;
    Ok(themes_dir().join(format!("{slug}.json")))
}

pub fn discover() -> Vec<String> {
    discover_in(&themes_dir())
}

pub fn discover_in(dir: &Path) -> Vec<String> {
    let mut names = Vec::new();
    let Ok(entries) = fs::read_dir(dir) else {
        return names;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        if valid_slug(stem) {
            names.push(stem.to_ascii_lowercase());
        }
    }
    names.sort();
    names.dedup();
    names
}

pub fn load_custom(slug: &str) -> Result<(Roles, Value)> {
    load_custom_at(&themes_dir(), slug)
}

pub fn load_custom_at(dir: &Path, slug: &str) -> Result<(Roles, Value)> {
    let slug = normalize_slug(slug)?;
    let path = dir.join(format!("{slug}.json"));
    let text = fs::read_to_string(&path).with_context(|| {
        format!("!! theme custom:{slug} missing file\ntry: varynth theme get magenta")
    })?;
    let value: Value = serde_json::from_str(&text)
        .with_context(|| format!("!! theme custom:{slug} is not JSON"))?;
    let roles = roles_from_value(&value, &format!("custom:{slug}"))?;
    Ok((roles, value))
}

pub fn save_custom(slug: &str, value: &Value) -> Result<PathBuf> {
    save_custom_at(&themes_dir(), slug, value)
}

pub fn save_custom_at(dir: &Path, slug: &str, value: &Value) -> Result<PathBuf> {
    let slug = normalize_slug(slug)?;
    let normalized = normalize_custom_value(value, &format!("custom:{slug}"))?;
    fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let path = dir.join(format!("{slug}.json"));
    let text = serde_json::to_string_pretty(&normalized).context("serialize theme")?;
    crate::control_bus::atomic_write(&path, text.as_bytes())?;
    Ok(path)
}

pub fn write_roles(
    slug: &str,
    roles: Roles,
    gradient_from: (u8, u8, u8),
    gradient_to: (u8, u8, u8),
) -> Result<PathBuf> {
    save_custom(slug, &roles_value(roles, gradient_from, gradient_to))
}

pub fn set_gradient(slug: &str, from: (u8, u8, u8), to: (u8, u8, u8)) -> Result<PathBuf> {
    let (roles, mut value) = load_custom(slug)?;
    let object = value
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("!! theme custom:{slug} is not an object"))?;
    object.insert("gradient_from".into(), Value::String(rgb_hex(from)));
    object.insert("gradient_to".into(), Value::String(rgb_hex(to)));
    let _ = roles;
    save_custom(slug, &value)
}

pub fn roles_value(roles: Roles, gradient_from: (u8, u8, u8), gradient_to: (u8, u8, u8)) -> Value {
    json!({
        "accent": color_text(roles.accent),
        "accent_neon": color_text(roles.accent_neon),
        "bg": color_text(roles.bg),
        "fg": color_text(roles.fg),
        "muted": color_text(roles.muted),
        "success": color_text(roles.success),
        "warning": color_text(roles.warning),
        "error": color_text(roles.error),
        "info": color_text(roles.info),
        "border": color_text(roles.border),
        "gradient_from": rgb_hex(gradient_from),
        "gradient_to": rgb_hex(gradient_to),
    })
}

pub fn role_rows(name: &str) -> Result<Vec<(String, String)>> {
    let roles = resolve(name)?;
    Ok(vec![
        ("name".into(), canonical(name)),
        ("accent".into(), color_text(roles.accent)),
        ("accent_neon".into(), color_text(roles.accent_neon)),
        ("bg".into(), color_text(roles.bg)),
        ("fg".into(), color_text(roles.fg)),
        ("muted".into(), color_text(roles.muted)),
        ("success".into(), color_text(roles.success)),
        ("warning".into(), color_text(roles.warning)),
        ("error".into(), color_text(roles.error)),
        ("info".into(), color_text(roles.info)),
        ("border".into(), color_text(roles.border)),
        ("truecolor".into(), roles.truecolor.to_string()),
    ])
}

pub fn parse_rgb(raw: &str) -> Result<(u8, u8, u8)> {
    parse_color(raw).and_then(|color| match color {
        Color::Rgb(r, g, b) => Ok((r, g, b)),
        _ => bail!("!! color `{raw}` is not a hex or rgb() value"),
    })
}

fn normalize_custom_value(value: &Value, label: &str) -> Result<Value> {
    let roles = roles_from_value(value, label)?;
    Ok(roles_value(roles, roles.gradient_from, roles.gradient_to))
}

fn roles_from_value(value: &Value, label: &str) -> Result<Roles> {
    let object = value
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("!! theme {label} must be a JSON object"))?;
    for role in REQUIRED_ROLES {
        if !object.contains_key(role) {
            bail!("!! theme {label} missing role {role}\ntry: varynth theme get magenta");
        }
    }
    let color_of = |role: &str| -> Result<Color> {
        let raw = object
            .get(role)
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("!! theme {label} role {role} must be a string"))?;
        parse_color(raw).with_context(|| format!("!! theme {label} role {role}"))
    };
    let accent = color_of("accent")?;
    let bg = color_of("bg")?;
    let gradient_from = match object.get("gradient_from").and_then(Value::as_str) {
        Some(raw) => parse_rgb(raw)?,
        None => rgb_of(accent).unwrap_or((0xE9, 0x1E, 0x8C)),
    };
    let gradient_to = match object.get("gradient_to").and_then(Value::as_str) {
        Some(raw) => parse_rgb(raw)?,
        None => rgb_of(bg).unwrap_or((0x14, 0x0E, 0x16)),
    };
    Ok(Roles {
        accent,
        accent_neon: color_of("accent_neon")?,
        bg,
        fg: color_of("fg")?,
        muted: color_of("muted")?,
        success: color_of("success")?,
        warning: color_of("warning")?,
        error: color_of("error")?,
        info: color_of("info")?,
        border: color_of("border")?,
        truecolor: true,
        gradient_from,
        gradient_to,
    })
}

fn parse_color(raw: &str) -> Result<Color> {
    let value = raw.trim();
    if value.is_empty() || value.len() > 64 || !value.is_ascii() {
        bail!("expected #RGB, #RRGGBB, or rgb()/rgba()");
    }
    if let Some(hex) = value.strip_prefix('#') {
        let expanded;
        let digits = match hex.len() {
            3 | 4 => {
                expanded = hex
                    .chars()
                    .take(3)
                    .flat_map(|ch| [ch, ch])
                    .collect::<String>();
                expanded.as_str()
            }
            6 | 8 => &hex[..6],
            _ => bail!("expected #RGB, #RRGGBB, or rgb()/rgba()"),
        };
        if !digits.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            bail!("expected #RGB, #RRGGBB, or rgb()/rgba()");
        }
        let r = u8::from_str_radix(&digits[0..2], 16).unwrap_or(0);
        let g = u8::from_str_radix(&digits[2..4], 16).unwrap_or(0);
        let b = u8::from_str_radix(&digits[4..6], 16).unwrap_or(0);
        return Ok(Color::Rgb(r, g, b));
    }
    let lower = value.to_ascii_lowercase();
    let Some(inner) = lower
        .strip_prefix("rgb(")
        .or_else(|| lower.strip_prefix("rgba("))
        .and_then(|rest| rest.strip_suffix(')'))
    else {
        bail!("expected #RGB, #RRGGBB, or rgb()/rgba()");
    };
    let parts: Vec<&str> = if inner.contains(',') {
        inner
            .split(',')
            .map(str::trim)
            .filter(|part| !part.is_empty())
            .collect()
    } else {
        inner
            .split_whitespace()
            .filter(|part| *part != "/")
            .collect()
    };
    if parts.len() < 3 {
        bail!("expected #RGB, #RRGGBB, or rgb()/rgba()");
    }
    Ok(Color::Rgb(
        rgb_component(parts[0])?,
        rgb_component(parts[1])?,
        rgb_component(parts[2])?,
    ))
}

fn rgb_component(token: &str) -> Result<u8> {
    if let Some(number) = token.strip_suffix('%') {
        let value: f32 = number
            .trim()
            .parse()
            .map_err(|_| anyhow::anyhow!("expected #RGB, #RRGGBB, or rgb()/rgba()"))?;
        if !(0.0..=100.0).contains(&value) {
            bail!("expected #RGB, #RRGGBB, or rgb()/rgba()");
        }
        return Ok((value / 100.0 * 255.0).round() as u8);
    }
    let value: u16 = token
        .trim()
        .parse()
        .map_err(|_| anyhow::anyhow!("expected #RGB, #RRGGBB, or rgb()/rgba()"))?;
    if value > 255 {
        bail!("expected #RGB, #RRGGBB, or rgb()/rgba()");
    }
    Ok(value as u8)
}

pub fn color_text(color: Color) -> String {
    match color {
        Color::Rgb(r, g, b) => rgb_hex((r, g, b)),
        Color::Reset => "ansi:reset".into(),
        Color::Black => "ansi:black".into(),
        Color::Red => "ansi:red".into(),
        Color::Green => "ansi:green".into(),
        Color::Yellow => "ansi:yellow".into(),
        Color::Blue => "ansi:blue".into(),
        Color::Magenta => "ansi:magenta".into(),
        Color::Cyan => "ansi:cyan".into(),
        Color::Gray => "ansi:gray".into(),
        Color::DarkGray => "ansi:dark-gray".into(),
        Color::White => "ansi:white".into(),
        Color::LightRed => "ansi:light-red".into(),
        Color::LightGreen => "ansi:light-green".into(),
        Color::LightYellow => "ansi:light-yellow".into(),
        Color::LightBlue => "ansi:light-blue".into(),
        Color::LightMagenta => "ansi:light-magenta".into(),
        Color::LightCyan => "ansi:light-cyan".into(),
        Color::Indexed(index) => format!("ansi:indexed-{index}"),
    }
}

fn rgb_hex(rgb: (u8, u8, u8)) -> String {
    format!("#{:02X}{:02X}{:02X}", rgb.0, rgb.1, rgb.2)
}

fn rgb_of(color: Color) -> Option<(u8, u8, u8)> {
    match color {
        Color::Rgb(r, g, b) => Some((r, g, b)),
        _ => None,
    }
}

fn valid_slug(slug: &str) -> bool {
    valid_profile_id(slug)
}

fn normalize_slug(slug: &str) -> Result<String> {
    let slug = slug.trim().to_ascii_lowercase();
    if !valid_slug(&slug) {
        bail!("!! theme custom:{slug} has an invalid name\ntry: varynth theme list");
    }
    Ok(slug)
}

pub fn active_truecolor() -> bool {
    ACTIVE.with(|active| active.get().0.truecolor) && crate::brand::color_allowed()
}

pub fn active_gradient() -> crate::gradient::Gradient {
    let roles = ACTIVE.with(|active| active.get().0);
    crate::gradient::Gradient {
        from: roles.gradient_from,
        to: roles.gradient_to,
    }
}

pub const CLAP_STYLES: clap::builder::Styles = clap::builder::Styles::styled()
    .header(
        clap::builder::styling::AnsiColor::Magenta
            .on_default()
            .bold(),
    )
    .usage(
        clap::builder::styling::AnsiColor::Magenta
            .on_default()
            .bold(),
    )
    .literal(
        clap::builder::styling::AnsiColor::Magenta
            .on_default()
            .bold(),
    )
    .placeholder(clap::builder::styling::AnsiColor::White.on_default())
    .error(clap::builder::styling::AnsiColor::Red.on_default().bold())
    .valid(clap::builder::styling::AnsiColor::Green.on_default().bold())
    .invalid(
        clap::builder::styling::AnsiColor::Yellow
            .on_default()
            .bold(),
    );

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Palette;

impl Palette {
    fn roles() -> Roles {
        ACTIVE.with(|active| active.get().0)
    }
    pub fn accent() -> Color {
        Self::roles().accent
    }
    pub fn accent_deep() -> Color {
        let roles = Self::roles();
        if roles.truecolor {
            ACCENT_DEEP
        } else {
            roles.accent
        }
    }
    pub fn accent_neon() -> Color {
        Self::roles().accent_neon
    }
    pub fn purple_primary() -> Color {
        Self::accent()
    }
    pub fn purple_deep() -> Color {
        Self::accent_deep()
    }
    pub fn purple_glow() -> Color {
        Self::accent_neon()
    }
    pub fn bg() -> Color {
        Self::roles().bg
    }
    pub fn bg_obsidian() -> Color {
        let roles = Self::roles();
        if roles.truecolor && roles.bg == BG {
            BG_OBSIDIAN
        } else {
            roles.bg
        }
    }
    pub fn bg_panel() -> Color {
        let roles = Self::roles();
        if roles.truecolor && roles.bg == BG {
            BG_PANEL
        } else {
            roles.bg
        }
    }
    pub fn bg_elevated() -> Color {
        let roles = Self::roles();
        if roles.truecolor && roles.bg == BG {
            BG_ELEVATED
        } else {
            roles.bg
        }
    }
    pub fn bg_input() -> Color {
        let roles = Self::roles();
        if roles.truecolor && roles.bg == BG {
            BG_INPUT
        } else {
            roles.bg
        }
    }
    pub fn fg() -> Color {
        Self::roles().fg
    }
    pub fn dim() -> Color {
        Self::roles().muted
    }
    pub fn ok() -> Color {
        Self::roles().success
    }
    pub fn ok_light() -> Color {
        let roles = Self::roles();
        if roles.truecolor {
            OK_LIGHT
        } else {
            roles.success
        }
    }
    pub fn warn() -> Color {
        Self::roles().warning
    }
    pub fn warn_light() -> Color {
        let roles = Self::roles();
        if roles.truecolor {
            WARN_LIGHT
        } else {
            roles.warning
        }
    }
    pub fn error() -> Color {
        Self::roles().error
    }
    pub fn error_light() -> Color {
        let roles = Self::roles();
        if roles.truecolor {
            ERROR_LIGHT
        } else {
            roles.error
        }
    }
    pub fn info() -> Color {
        Self::roles().info
    }
    pub fn info_bright() -> Color {
        let roles = Self::roles();
        if roles.truecolor {
            INFO_BRIGHT
        } else {
            roles.info
        }
    }
    pub fn info_light() -> Color {
        let roles = Self::roles();
        if roles.truecolor {
            INFO_LIGHT
        } else {
            roles.info
        }
    }
    pub fn border() -> Color {
        Self::roles().border
    }
    pub fn border_subtle() -> Color {
        let roles = Self::roles();
        if roles.truecolor && roles.bg == BG {
            BORDER_SUBTLE
        } else {
            roles.border
        }
    }
    pub fn border_purple() -> Color {
        let roles = Self::roles();
        if roles.truecolor && roles.bg == BG {
            BORDER_PURPLE
        } else {
            roles.accent
        }
    }

    // Semantic @agent pill
    pub fn pill_agent() -> (Color, Color) {
        let roles = Self::roles();
        if roles.truecolor {
            (PILL_AGENT_FG, PILL_AGENT_BG)
        } else {
            (roles.accent_neon, roles.bg)
        }
    }
    pub fn pill_agent_fg() -> Color {
        Self::pill_agent().0
    }
    pub fn pill_agent_bg() -> Color {
        Self::pill_agent().1
    }
    pub fn pill_agent_style() -> Style {
        let (fg, bg) = Self::pill_agent();
        Style::default().fg(fg).bg(bg)
    }

    // Semantic #file pill
    pub fn pill_file() -> (Color, Color) {
        let roles = Self::roles();
        if roles.truecolor {
            (PILL_FILE_FG, PILL_FILE_BG)
        } else {
            (roles.info, roles.bg)
        }
    }
    pub fn pill_file_fg() -> Color {
        Self::pill_file().0
    }
    pub fn pill_file_bg() -> Color {
        Self::pill_file().1
    }
    pub fn pill_file_style() -> Style {
        let (fg, bg) = Self::pill_file();
        Style::default().fg(fg).bg(bg)
    }

    // Semantic $skill pill
    pub fn pill_skill() -> (Color, Color) {
        let roles = Self::roles();
        if roles.truecolor {
            (PILL_SKILL_FG, PILL_SKILL_BG)
        } else {
            (roles.warning, roles.bg)
        }
    }
    pub fn pill_skill_fg() -> Color {
        Self::pill_skill().0
    }
    pub fn pill_skill_bg() -> Color {
        Self::pill_skill().1
    }
    pub fn pill_skill_style() -> Style {
        let (fg, bg) = Self::pill_skill();
        Style::default().fg(fg).bg(bg)
    }

    // Semantic /cmd pill
    pub fn pill_cmd() -> (Color, Color) {
        let roles = Self::roles();
        if roles.truecolor {
            (PILL_CMD_FG, PILL_CMD_BG)
        } else {
            (roles.success, roles.bg)
        }
    }
    pub fn pill_cmd_fg() -> Color {
        Self::pill_cmd().0
    }
    pub fn pill_cmd_bg() -> Color {
        Self::pill_cmd().1
    }
    pub fn pill_cmd_style() -> Style {
        let (fg, bg) = Self::pill_cmd();
        Style::default().fg(fg).bg(bg)
    }

    // Diff add line: (fg, bg, bar)
    pub fn diff_add() -> (Color, Color, Color) {
        let roles = Self::roles();
        if roles.truecolor {
            (DIFF_ADD_FG, DIFF_ADD_BG, DIFF_ADD_BAR)
        } else {
            (roles.success, roles.bg, roles.success)
        }
    }
    pub fn diff_add_fg() -> Color {
        Self::diff_add().0
    }
    pub fn diff_add_bg() -> Color {
        Self::diff_add().1
    }
    pub fn diff_add_bar() -> Color {
        Self::diff_add().2
    }
    pub fn diff_add_style() -> Style {
        let (fg, bg, _) = Self::diff_add();
        Style::default().fg(fg).bg(bg)
    }

    // Diff del line: (fg, bg, bar)
    pub fn diff_del() -> (Color, Color, Color) {
        let roles = Self::roles();
        if roles.truecolor {
            (DIFF_DEL_FG, DIFF_DEL_BG, DIFF_DEL_BAR)
        } else {
            (roles.error, roles.bg, roles.error)
        }
    }
    pub fn diff_del_fg() -> Color {
        Self::diff_del().0
    }
    pub fn diff_del_bg() -> Color {
        Self::diff_del().1
    }
    pub fn diff_del_bar() -> Color {
        Self::diff_del().2
    }
    pub fn diff_del_style() -> Style {
        let (fg, bg, _) = Self::diff_del();
        Style::default().fg(fg).bg(bg)
    }

    pub fn diff_ctx_fg() -> Color {
        let roles = Self::roles();
        if roles.truecolor {
            DIFF_CTX_FG
        } else {
            roles.muted
        }
    }
}

pub fn pill_agent() -> (Color, Color) {
    Palette::pill_agent()
}

pub fn pill_file() -> (Color, Color) {
    Palette::pill_file()
}

pub fn pill_skill() -> (Color, Color) {
    Palette::pill_skill()
}

pub fn pill_cmd() -> (Color, Color) {
    Palette::pill_cmd()
}

pub fn diff_add() -> (Color, Color, Color) {
    Palette::diff_add()
}

pub fn diff_del() -> (Color, Color, Color) {
    Palette::diff_del()
}

pub fn pill_agent_style() -> Style {
    Palette::pill_agent_style()
}

pub fn pill_file_style() -> Style {
    Palette::pill_file_style()
}

pub fn pill_skill_style() -> Style {
    Palette::pill_skill_style()
}

pub fn pill_cmd_style() -> Style {
    Palette::pill_cmd_style()
}

pub fn diff_add_style() -> Style {
    Palette::diff_add_style()
}

pub fn diff_del_style() -> Style {
    Palette::diff_del_style()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_names_canonicalize() {
        assert_eq!(canonical("purple"), "magenta");
        assert_eq!(canonical("Cyberpunk"), "dark");
        assert_eq!(canonical("minimal"), "safe");
        assert_eq!(Theme::parse("purple"), Some(Theme::Magenta));
        assert_eq!(Theme::parse("unknown"), None);
        assert!(ensure_valid("magenta").is_ok());
        assert!(ensure_valid("nope").is_err());
    }

    #[test]
    fn truecolor_palettes_are_distinct_rgb() {
        for theme in [Theme::Magenta, Theme::Dark, Theme::Light] {
            let roles = theme.roles();
            let all = [
                roles.accent,
                roles.accent_neon,
                roles.bg,
                roles.fg,
                roles.muted,
                roles.success,
                roles.warning,
                roles.error,
                roles.info,
                roles.border,
            ];
            for color in all {
                assert!(matches!(color, Color::Rgb(_, _, _)), "{theme:?} {color:?}");
            }
            for i in 0..all.len() {
                for j in (i + 1)..all.len() {
                    assert_ne!(all[i], all[j], "{theme:?}");
                }
            }
        }
        assert!(!Theme::Safe.roles().truecolor);
        assert!(!Theme::System.roles().truecolor);
        assert_eq!(Palette::accent(), ACCENT);
    }

    #[test]
    fn selectable_palettes_restore_after_a_draw() {
        for theme in Theme::ALL {
            with_theme(theme.name(), || assert_eq!(Palette::bg(), theme.roles().bg));
            assert_eq!(Palette::bg(), BG);
        }
    }

    #[test]
    fn custom_file_requires_roles_and_reads_gradient() {
        let dir = tempfile::tempdir().unwrap();
        let err = load_custom_at(dir.path(), "demo").unwrap_err().to_string();
        assert!(err.contains("!!"), "{err}");
        let value = json!({
            "accent": "#E91E8C",
            "accent_neon": "rgb(255, 92, 181)",
            "bg": "#140E16",
            "fg": "#F2E8F0",
            "muted": "#9A8A9C",
            "success": "#9BDEBE",
            "warning": "#EFD38F",
            "error": "#FFA1AD",
            "info": "#80B2EB",
            "border": "#3A2A38",
            "gradient_from": "#112233",
            "gradient_to": "#445566",
            "ignored": "drop-me"
        });
        let missing = json!({"accent": "#E91E8C"});
        let err = roles_from_value(&missing, "custom:demo")
            .unwrap_err()
            .to_string();
        assert!(err.contains("missing role"), "{err}");
        assert!(err.contains("try:"), "{err}");
        save_custom_at(dir.path(), "demo", &value).unwrap();
        let (roles, _) = load_custom_at(dir.path(), "Demo").unwrap();
        assert_eq!(roles.accent, Color::Rgb(0xE9, 0x1E, 0x8C));
        assert_eq!(roles.gradient_from, (0x11, 0x22, 0x33));
        assert_eq!(roles.gradient_to, (0x44, 0x55, 0x66));
        assert_eq!(discover_in(dir.path()), vec!["demo".to_string()]);
    }

    #[test]
    fn reference_html_palette_tokens_and_pills_match_spec() {
        assert_eq!(Palette::bg_obsidian(), Color::Rgb(0x08, 0x08, 0x0C));
        assert_eq!(Palette::bg_panel(), Color::Rgb(0x0D, 0x0D, 0x14));
        assert_eq!(Palette::bg_elevated(), Color::Rgb(0x13, 0x13, 0x1F));
        assert_eq!(Palette::accent(), Color::Rgb(0xA8, 0x55, 0xF7));
        assert_eq!(Palette::accent_deep(), Color::Rgb(0x93, 0x33, 0xEA));
        assert_eq!(Palette::accent_neon(), Color::Rgb(0xC0, 0x84, 0xFC));
        assert_eq!(Palette::info(), Color::Rgb(0x3B, 0x82, 0xF6));
        assert_eq!(Palette::info_bright(), Color::Rgb(0x60, 0xA5, 0xFA));
        assert_eq!(Palette::info_light(), Color::Rgb(0x93, 0xC5, 0xFD));
        assert_eq!(Palette::ok(), Color::Rgb(0x10, 0xB9, 0x81));
        assert_eq!(Palette::ok_light(), Color::Rgb(0x6E, 0xE7, 0xB7));
        assert_eq!(Palette::warn(), Color::Rgb(0xF5, 0x9E, 0x0B));
        assert_eq!(Palette::warn_light(), Color::Rgb(0xFC, 0xD3, 0x4D));
        assert_eq!(Palette::error(), Color::Rgb(0xEF, 0x44, 0x44));
        assert_eq!(Palette::error_light(), Color::Rgb(0xFC, 0xA5, 0xA5));
        assert_eq!(Palette::border(), Color::Rgb(0x26, 0x26, 0x3A));
        assert_eq!(Palette::border_subtle(), Color::Rgb(0x1E, 0x1E, 0x2D));
        assert_eq!(Palette::border_purple(), Color::Rgb(0x4C, 0x28, 0x78));

        assert_eq!(
            Palette::pill_agent(),
            (Color::Rgb(0xD8, 0xB4, 0xFE), Color::Rgb(0x24, 0x14, 0x38))
        );
        assert_eq!(
            Palette::pill_file(),
            (Color::Rgb(0x93, 0xC5, 0xFD), Color::Rgb(0x10, 0x20, 0x3B))
        );
        assert_eq!(
            Palette::pill_skill(),
            (Color::Rgb(0xFC, 0xD3, 0x4D), Color::Rgb(0x2E, 0x21, 0x0B))
        );
        assert_eq!(
            Palette::pill_cmd(),
            (Color::Rgb(0x6E, 0xE7, 0xB7), Color::Rgb(0x0B, 0x29, 0x20))
        );
        assert_eq!(
            Palette::diff_add(),
            (
                Color::Rgb(0x6E, 0xE7, 0xB7),
                Color::Rgb(0x09, 0x22, 0x1B),
                Color::Rgb(0x10, 0xB9, 0x81)
            )
        );
        assert_eq!(
            Palette::diff_del(),
            (
                Color::Rgb(0xFC, 0xA5, 0xA5),
                Color::Rgb(0x28, 0x11, 0x16),
                Color::Rgb(0xEF, 0x44, 0x44)
            )
        );
    }
}
