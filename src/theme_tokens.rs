//! Secure parsing and storage for user-supplied shadcn-style theme tokens.
//!
//! The module is intentionally self-contained so it can be wired into the
//! library later without adding a CSS parser dependency. Only the bounded
//! token allowlist below is persisted, and values are accepted only when they
//! match one of the small color/length grammars implemented here.

use anyhow::{bail, Context, Result};
use serde_json::{Map, Value};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::config::Config;

/// Maximum UTF-8 input or serialized output size for a theme.
pub const MAX_THEME_BYTES: usize = 64 * 1024;

/// Maximum size of one token value before syntax validation.
const MAX_TOKEN_VALUE_BYTES: usize = 4096;

/// The fixed filename keeps theme storage inside `Config::home_dir()`.
pub const THEME_FILE_NAME: &str = "theme.json";

const COLOR_TOKEN_NAMES: &[&str] = &[
    "bg",
    "rail",
    "panel",
    "raised",
    "line",
    "line-soft",
    "ink",
    "quiet",
    "accent-light",
    "accent-surface",
    "green",
    "amber",
    "red",
    "cyan",
    "background",
    "foreground",
    "card",
    "card-foreground",
    "popover",
    "popover-foreground",
    "primary",
    "primary-foreground",
    "secondary",
    "secondary-foreground",
    "muted",
    "muted-foreground",
    "accent",
    "accent-foreground",
    "destructive",
    "destructive-foreground",
    "border",
    "input",
    "ring",
    "chart-1",
    "chart-2",
    "chart-3",
    "chart-4",
    "chart-5",
    "sidebar",
    "sidebar-foreground",
    "sidebar-primary",
    "sidebar-primary-foreground",
    "sidebar-accent",
    "sidebar-accent-foreground",
    "sidebar-border",
    "sidebar-ring",
    "success",
    "success-foreground",
    "warning",
    "warning-foreground",
    "info",
    "info-foreground",
    "error",
    "error-foreground",
];

const RADIUS_TOKEN_NAMES: &[&str] = &["radius", "radius-sm", "radius-md", "radius-lg", "radius-xl"];

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Return the only path used by theme storage.
pub fn theme_path() -> PathBuf {
    Config::home_dir().join(THEME_FILE_NAME)
}

/// Validate and normalize a JSON theme object.
///
/// Unknown keys are dropped. Recognized keys are returned without a leading
/// CSS `--` prefix, with whitespace around values removed. A recognized key
/// with an invalid type or value is an error rather than silently disappearing.
pub fn validate_theme_json(value: &Value) -> Result<Value> {
    let encoded = serde_json::to_vec(value).context("serialize theme input")?;
    ensure_size(encoded.len())?;

    let object = value
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("theme must be a JSON object"))?;
    let mut normalized = Map::new();

    for (raw_key, raw_value) in object {
        let Some(key) = canonical_token_name(raw_key) else {
            continue;
        };
        if normalized.contains_key(key) {
            bail!("duplicate theme token `{key}`");
        }

        let raw_value = raw_value.as_str().ok_or_else(|| {
            anyhow::anyhow!("invalid theme token `{key}`: value must be a string")
        })?;
        let normalized_value = validate_token_value(key, raw_value)?;
        normalized.insert(key.to_string(), Value::String(normalized_value));
    }

    let normalized = Value::Object(normalized);
    let encoded = serde_json::to_vec(&normalized).context("serialize normalized theme")?;
    ensure_size(encoded.len())?;
    Ok(normalized)
}

/// Parse either a JSON object or a small CSS custom-property declaration set.
///
/// CSS may be a bare declaration list or one `:root { ... }` block. The
/// parser extracts only allowlisted custom properties; it never retains CSS.
pub fn parse_theme_text(text: &str) -> Result<Value> {
    ensure_size(text.len())?;
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Ok(empty_theme());
    }

    if trimmed.starts_with('{') {
        let value: Value = serde_json::from_str(trimmed).context("parse theme JSON")?;
        return validate_theme_json(&value);
    }

    parse_css_text(trimmed)
}

/// Load and validate `Config::home_dir()/theme.json`.
///
/// A missing, zero-byte, or whitespace-only file is treated as an empty theme.
/// Existing malformed, unsafe, oversized, or non-regular files are errors.
pub fn load_theme() -> Result<Value> {
    let path = theme_path();
    let link_metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(empty_theme()),
        Err(error) => return Err(error).with_context(|| format!("inspect {}", path.display())),
    };
    if link_metadata.file_type().is_symlink() {
        bail!("theme path must not be a symlink");
    }
    if !link_metadata.is_file() {
        bail!("theme path must be a regular file");
    }
    if link_metadata.len() > MAX_THEME_BYTES as u64 {
        bail!("theme file exceeds the {} byte limit", MAX_THEME_BYTES);
    }

    let mut file = File::open(&path).with_context(|| format!("read {}", path.display()))?;
    let mut bytes = Vec::with_capacity((link_metadata.len() as usize).min(MAX_THEME_BYTES + 1));
    std::io::Read::by_ref(&mut file)
        .take((MAX_THEME_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .with_context(|| format!("read {}", path.display()))?;
    if bytes.len() > MAX_THEME_BYTES {
        bail!("theme file exceeds the {} byte limit", MAX_THEME_BYTES);
    }
    if bytes.iter().all(u8::is_ascii_whitespace) {
        return Ok(empty_theme());
    }

    let text = std::str::from_utf8(&bytes).context("theme file is not valid UTF-8")?;
    parse_theme_text(text)
}

/// Validate and atomically save a theme to `Config::home_dir()/theme.json`.
pub fn save_theme(value: &Value) -> Result<()> {
    let normalized = validate_theme_json(value)?;
    let encoded = serde_json::to_vec_pretty(&normalized).context("serialize theme")?;
    ensure_size(encoded.len())?;

    let path = theme_path();
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("theme path has no parent directory"))?;
    fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    if fs::symlink_metadata(parent)?.file_type().is_symlink() {
        bail!("theme directory must not be a symlink");
    }
    match fs::symlink_metadata(&path) {
        Ok(meta) if meta.file_type().is_symlink() || !meta.is_file() => {
            bail!("theme path must be a regular file")
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let _lock = crate::control_bus::FileLock::acquire(parent.join(".theme.lock"))?;

    let (mut file, temp_path) = create_temp_file(parent)?;
    let guard = TempPathGuard::new(temp_path.clone());
    if let Err(error) = write_temp_file(&mut file, &encoded) {
        return Err(error).with_context(|| format!("write {}", temp_path.display()));
    }
    drop(file);

    atomic_replace(&temp_path, &path).with_context(|| format!("replace {}", path.display()))?;
    guard.disarm();
    sync_parent_directory(parent).with_context(|| format!("sync {}", parent.display()))?;
    Ok(())
}

fn empty_theme() -> Value {
    Value::Object(Map::new())
}

fn ensure_size(size: usize) -> Result<()> {
    if size > MAX_THEME_BYTES {
        bail!("theme input exceeds the {} byte limit", MAX_THEME_BYTES);
    }
    Ok(())
}

fn canonical_token_name(raw: &str) -> Option<&'static str> {
    let name = raw.strip_prefix("--").unwrap_or(raw);
    COLOR_TOKEN_NAMES
        .iter()
        .chain(RADIUS_TOKEN_NAMES.iter())
        .copied()
        .find(|candidate| *candidate == name)
}

fn validate_token_value(key: &str, raw: &str) -> Result<String> {
    if raw.len() > MAX_TOKEN_VALUE_BYTES {
        bail!("invalid theme token `{key}`: value is too large");
    }
    let value = raw.trim();
    if value.is_empty() {
        bail!("invalid theme token `{key}`: value is empty");
    }
    ensure_safe_value_text(value, key)?;

    if RADIUS_TOKEN_NAMES.contains(&key) {
        if !valid_radius(value) {
            bail!("invalid theme token `{key}`: expected a bounded CSS length");
        }
    } else if !valid_color(value) {
        bail!("invalid theme token `{key}`: expected a safe color value");
    }
    Ok(value.to_string())
}

fn ensure_safe_source_text(text: &str) -> Result<()> {
    if !text.is_ascii()
        || text
            .chars()
            .any(|ch| ch.is_control() && !matches!(ch, '\n' | '\r' | '\t' | '\u{0c}'))
    {
        bail!("unsafe theme CSS: control or non-ASCII text is not allowed");
    }
    let lower = text.to_ascii_lowercase();
    for marker in [
        "url",
        "@import",
        "import(",
        "script",
        "expression",
        "javascript:",
        "data:",
        "/*",
        "*/",
        "\\",
        "<",
        ">",
    ] {
        if lower.contains(marker) {
            bail!("unsafe theme CSS: forbidden content");
        }
    }
    Ok(())
}

fn ensure_safe_value_text(value: &str, key: &str) -> Result<()> {
    if !value.is_ascii() || value.chars().any(char::is_control) {
        bail!("invalid theme token `{key}`: unsafe characters");
    }
    if value
        .chars()
        .any(|character| matches!(character, ';' | '{' | '}' | '"' | '\'' | '\\'))
    {
        bail!("invalid theme token `{key}`: unsafe characters");
    }
    let lower = value.to_ascii_lowercase();
    for marker in [
        "url",
        "@import",
        "import(",
        "script",
        "expression",
        "javascript:",
        "data:",
        "/*",
        "*/",
    ] {
        if lower.contains(marker) {
            bail!("invalid theme token `{key}`: forbidden content");
        }
    }
    Ok(())
}

fn parse_css_text(text: &str) -> Result<Value> {
    ensure_safe_source_text(text)?;
    let body = css_body(text)?;
    if body.trim().is_empty() {
        return Ok(empty_theme());
    }

    let declarations = split_declarations(body)?;
    let mut object = Map::new();
    for declaration in declarations {
        let declaration = declaration.trim();
        if declaration.is_empty() {
            continue;
        }
        let Some(colon) = declaration.find(':') else {
            bail!("invalid theme CSS: declaration is missing a colon");
        };
        let raw_key = declaration[..colon].trim();
        let raw_value = declaration[colon + 1..].trim();
        if !raw_key.starts_with("--") {
            bail!("invalid theme CSS: only custom properties are accepted");
        }
        let Some(key) = canonical_token_name(raw_key) else {
            continue;
        };
        let normalized_value = validate_token_value(key, raw_value)?;
        object.insert(key.to_string(), Value::String(normalized_value));
    }

    validate_theme_json(&Value::Object(object))
}

fn css_body(text: &str) -> Result<&str> {
    let has_open = text.contains('{');
    let has_close = text.contains('}');
    if !has_open && !has_close {
        return Ok(text);
    }
    if !has_open || !has_close {
        bail!("invalid theme CSS: unmatched braces");
    }
    if text.matches('{').count() != 1 || text.matches('}').count() != 1 {
        bail!("invalid theme CSS: nested or repeated braces are not allowed");
    }

    let open = text.find('{').expect("checked above");
    let close = text.rfind('}').expect("checked above");
    if text[..open].trim() != ":root" || text[close + 1..].trim() != "" {
        bail!("invalid theme CSS: only a single :root block is accepted");
    }
    Ok(&text[open + 1..close])
}

fn split_declarations(body: &str) -> Result<Vec<&str>> {
    let mut declarations = Vec::new();
    let mut start = 0;
    let mut parentheses = 0usize;

    for (index, character) in body.char_indices() {
        match character {
            '(' => parentheses = parentheses.saturating_add(1),
            ')' => {
                if parentheses == 0 {
                    bail!("invalid theme CSS: unmatched closing parenthesis");
                }
                parentheses -= 1;
            }
            ';' if parentheses == 0 => {
                declarations.push(&body[start..index]);
                start = index + character.len_utf8();
            }
            _ => {}
        }
    }
    if parentheses != 0 {
        bail!("invalid theme CSS: unmatched opening parenthesis");
    }
    declarations.push(&body[start..]);
    Ok(declarations)
}

fn valid_color(value: &str) -> bool {
    valid_hex(value) || valid_bare_hsl(value) || valid_color_function(value)
}

fn valid_hex(value: &str) -> bool {
    let Some(rest) = value.strip_prefix('#') else {
        return false;
    };
    matches!(rest.len(), 3 | 4 | 6 | 8) && rest.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn valid_bare_hsl(value: &str) -> bool {
    let tokens: Vec<&str> = value.split_whitespace().collect();
    if tokens.len() != 3 && tokens.len() != 5 {
        return false;
    }
    if tokens.len() == 5 && tokens[3] != "/" {
        return false;
    }
    valid_hue(tokens[0])
        && valid_percentage_or_none(tokens[1])
        && valid_percentage_or_none(tokens[2])
        && (tokens.len() == 3 || valid_alpha(tokens[4]))
}

fn valid_color_function(value: &str) -> bool {
    let Some((name, inner)) = function_parts(value) else {
        return false;
    };
    match name.as_str() {
        "rgb" | "rgba" => valid_rgb_function(&name, inner),
        "hsl" | "hsla" => valid_hsl_function(&name, inner),
        "oklch" => valid_oklch_function(inner),
        "color" => valid_color_space_function(inner),
        _ => false,
    }
}

fn function_parts(value: &str) -> Option<(String, &str)> {
    let open = value.find('(')?;
    if !value.ends_with(')') {
        return None;
    }
    let name = value[..open].trim();
    if name.is_empty() || !name.bytes().all(|byte| byte.is_ascii_alphabetic()) {
        return None;
    }
    let inner = &value[open + 1..value.len() - 1];
    if inner.contains('(') || inner.contains(')') {
        return None;
    }
    Some((name.to_ascii_lowercase(), inner))
}

fn valid_rgb_function(name: &str, inner: &str) -> bool {
    let components = if inner.contains(',') {
        if inner.contains('/') {
            return false;
        }
        let parts: Vec<&str> = inner.split(',').map(str::trim).collect();
        if parts.iter().any(|part| part.is_empty()) {
            return false;
        }
        parts
    } else {
        match space_components(inner, 3) {
            Some(parts) => parts,
            None => return false,
        }
    };

    if name == "rgba" && components.len() != 4 {
        return false;
    }
    if name == "rgb" && !(components.len() == 3 || components.len() == 4) {
        return false;
    }
    components[..3].iter().all(|part| valid_rgb_component(part))
        && (components.len() == 3 || valid_alpha(components[3]))
}

fn valid_hsl_function(name: &str, inner: &str) -> bool {
    let components = if inner.contains(',') {
        if inner.contains('/') {
            return false;
        }
        let parts: Vec<&str> = inner.split(',').map(str::trim).collect();
        if parts.iter().any(|part| part.is_empty()) {
            return false;
        }
        parts
    } else {
        match space_components(inner, 3) {
            Some(parts) => parts,
            None => return false,
        }
    };

    if name == "hsla" && components.len() != 4 {
        return false;
    }
    if name == "hsl" && !(components.len() == 3 || components.len() == 4) {
        return false;
    }
    valid_hue(components[0])
        && valid_percentage_or_none(components[1])
        && valid_percentage_or_none(components[2])
        && (components.len() == 3 || valid_alpha(components[3]))
}

fn valid_oklch_function(inner: &str) -> bool {
    let Some(components) = space_components(inner, 3) else {
        return false;
    };
    valid_lightness(components[0])
        && valid_chroma(components[1])
        && valid_hue(components[2])
        && (components.len() == 3 || valid_alpha(components[3]))
}

fn valid_color_space_function(inner: &str) -> bool {
    let tokens: Vec<&str> = inner.split_whitespace().collect();
    if tokens.len() < 4 {
        return false;
    }
    let space = tokens[0].to_ascii_lowercase();
    if !matches!(
        space.as_str(),
        "srgb"
            | "srgb-linear"
            | "display-p3"
            | "a98-rgb"
            | "prophoto-rgb"
            | "rec2020"
            | "xyz"
            | "xyz-d50"
            | "xyz-d65"
    ) {
        return false;
    }
    let components: Vec<&str> = if tokens.len() == 4 {
        tokens[1..].to_vec()
    } else if tokens.len() == 6 && tokens[4] == "/" {
        vec![tokens[1], tokens[2], tokens[3], tokens[5]]
    } else {
        return false;
    };
    components[..3]
        .iter()
        .all(|part| valid_color_component(part))
        && (components.len() == 3 || valid_alpha(components[3]))
}

fn space_components<'a>(inner: &'a str, required: usize) -> Option<Vec<&'a str>> {
    if inner.contains(',') {
        return None;
    }
    let tokens: Vec<&str> = inner.split_whitespace().collect();
    if tokens.len() == required {
        return Some(tokens);
    }
    if tokens.len() == required + 2 && tokens[required] == "/" {
        let mut components = tokens[..required].to_vec();
        components.push(tokens[required + 1]);
        return Some(components);
    }
    None
}

fn valid_rgb_component(token: &str) -> bool {
    if token.eq_ignore_ascii_case("none") {
        return true;
    }
    if let Some(number) = token.strip_suffix('%') {
        return parse_css_number(number).is_some_and(|value| (0.0..=100.0).contains(&value));
    }
    parse_css_number(token).is_some_and(|value| (0.0..=255.0).contains(&value))
}

fn valid_color_component(token: &str) -> bool {
    if token.eq_ignore_ascii_case("none") {
        return true;
    }
    if let Some(number) = token.strip_suffix('%') {
        return parse_css_number(number).is_some_and(|value| (0.0..=100.0).contains(&value));
    }
    parse_css_number(token).is_some_and(|value| (-4.0..=4.0).contains(&value))
}

fn valid_percentage_or_none(token: &str) -> bool {
    token.eq_ignore_ascii_case("none")
        || token
            .strip_suffix('%')
            .and_then(parse_css_number)
            .is_some_and(|value| (0.0..=100.0).contains(&value))
}

fn valid_alpha(token: &str) -> bool {
    if token.eq_ignore_ascii_case("none") {
        return true;
    }
    if let Some(number) = token.strip_suffix('%') {
        return parse_css_number(number).is_some_and(|value| (0.0..=100.0).contains(&value));
    }
    parse_css_number(token).is_some_and(|value| (0.0..=1.0).contains(&value))
}

fn valid_hue(token: &str) -> bool {
    if token.eq_ignore_ascii_case("none") {
        return true;
    }
    for suffix in ["deg", "grad", "rad", "turn"] {
        if let Some(number) = token.strip_suffix(suffix) {
            return parse_css_number(number).is_some_and(|value| value.abs() <= 1_000_000.0);
        }
    }
    parse_css_number(token).is_some_and(|value| value.abs() <= 1_000_000.0)
}

fn valid_lightness(token: &str) -> bool {
    if token.eq_ignore_ascii_case("none") {
        return true;
    }
    if let Some(number) = token.strip_suffix('%') {
        return parse_css_number(number).is_some_and(|value| (0.0..=100.0).contains(&value));
    }
    parse_css_number(token).is_some_and(|value| (0.0..=1.0).contains(&value))
}

fn valid_chroma(token: &str) -> bool {
    if token.eq_ignore_ascii_case("none") {
        return true;
    }
    if let Some(number) = token.strip_suffix('%') {
        return parse_css_number(number).is_some_and(|value| (0.0..=100.0).contains(&value));
    }
    parse_css_number(token).is_some_and(|value| (0.0..=2.0).contains(&value))
}

fn valid_radius(value: &str) -> bool {
    if value == "0" {
        return true;
    }
    if let Some(number) = value.strip_suffix('%') {
        return parse_css_number(number).is_some_and(|value| (0.0..=100.0).contains(&value));
    }
    const UNITS: &[&str] = &[
        "px", "rem", "em", "ch", "ex", "cm", "mm", "in", "pt", "pc", "q", "vh", "vw", "vmin",
        "vmax", "vi", "vb", "svh", "svw", "lvh", "lvw", "dvh", "dvw",
    ];
    UNITS.iter().any(|unit| {
        value
            .strip_suffix(unit)
            .and_then(parse_css_number)
            .is_some_and(|number| (0.0..=1000.0).contains(&number))
    })
}

fn parse_css_number(value: &str) -> Option<f64> {
    if value.is_empty() {
        return None;
    }
    let bytes = value.as_bytes();
    let mut index = 0;
    if matches!(bytes.first(), Some(b'+') | Some(b'-')) {
        index = 1;
    }
    let mut digits = 0;
    let mut dots = 0;
    for byte in &bytes[index..] {
        match byte {
            b'0'..=b'9' => digits += 1,
            b'.' => {
                dots += 1;
                if dots > 1 {
                    return None;
                }
            }
            _ => return None,
        }
    }
    if digits == 0 {
        return None;
    }
    value
        .parse::<f64>()
        .ok()
        .filter(|number| number.is_finite())
}

fn create_temp_file(parent: &Path) -> Result<(File, PathBuf)> {
    for _ in 0..32 {
        let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let name = format!(".{THEME_FILE_NAME}.{}.{}.tmp", std::process::id(), counter);
        let path = parent.join(name);
        match OpenOptions::new().create_new(true).write(true).open(&path) {
            Ok(file) => return Ok((file, path)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error).with_context(|| format!("create {}", path.display())),
        }
    }
    bail!("could not allocate a temporary theme file")
}

fn write_temp_file(file: &mut File, bytes: &[u8]) -> io::Result<()> {
    file.write_all(bytes)?;
    file.sync_all()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = file.metadata()?.permissions();
        permissions.set_mode(0o600);
        file.set_permissions(permissions)?;
    }
    Ok(())
}

fn atomic_replace(temp: &Path, target: &Path) -> io::Result<()> {
    #[cfg(windows)]
    {
        use std::iter::once;
        use std::os::windows::ffi::OsStrExt;

        #[link(name = "kernel32")]
        extern "system" {
            fn MoveFileExW(
                existing_file_name: *const u16,
                new_file_name: *const u16,
                flags: u32,
            ) -> i32;
        }

        const MOVEFILE_REPLACE_EXISTING: u32 = 0x1;
        const MOVEFILE_WRITE_THROUGH: u32 = 0x8;
        let source: Vec<u16> = temp.as_os_str().encode_wide().chain(once(0)).collect();
        let destination: Vec<u16> = target.as_os_str().encode_wide().chain(once(0)).collect();
        let replaced = unsafe {
            MoveFileExW(
                source.as_ptr(),
                destination.as_ptr(),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
        };
        if replaced == 0 {
            return Err(io::Error::last_os_error());
        }
        return Ok(());
    }

    #[cfg(not(windows))]
    {
        fs::rename(temp, target)
    }
}

fn sync_parent_directory(parent: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        File::open(parent)?.sync_all()
    }
    #[cfg(not(unix))]
    {
        let _ = parent;
        Ok(())
    }
}

struct TempPathGuard {
    path: Option<PathBuf>,
}

impl TempPathGuard {
    fn new(path: PathBuf) -> Self {
        Self { path: Some(path) }
    }

    fn disarm(mut self) {
        self.path = None;
    }
}

impl Drop for TempPathGuard {
    fn drop(&mut self) {
        if let Some(path) = self.path.take() {
            let _ = fs::remove_file(path);
        }
    }
}
