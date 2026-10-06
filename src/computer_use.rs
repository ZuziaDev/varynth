//! Opt-in desktop computer-use tools. The tools layer supplies a Jail via
//! `dispatch_in_jail`; the two-argument wrapper uses a conservative read-only
//! cwd jail. This implementation uses foreground desktop input, not stealth
//! or background-window injection.

use anyhow::{bail, Context, Result};
use enigo::{Button, Coordinate, Direction, Enigo, Key, Keyboard, Mouse, Settings};
use serde_json::{json, Value};
use std::fs;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::sandbox::{Jail, SandboxMode};

const ENABLE_ENV: &str = "VARYNTH_COMPUTER_USE";
const MAX_COORDINATE: i32 = 100_000;
const MAX_TEXT_BYTES: usize = 64 * 1024;
const MAX_KEY_BYTES: usize = 128;
const MAX_WINDOWS: usize = 256;
const MAX_SCREEN_WIDTH: u32 = 32_768;
const MAX_SCREEN_HEIGHT: u32 = 32_768;
const MAX_SCREEN_PIXELS: u64 = 32 * 1024 * 1024;
const GRID_MINOR: u32 = 25;
const GRID_MAJOR: u32 = 100;

static STOP_REQUESTED: AtomicBool = AtomicBool::new(false);
static ACTION_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
static ATTACHMENT_SEQUENCE: AtomicU64 = AtomicU64::new(1);

const TOOL_NAMES: &[&str] = &[
    "computer_screenshot",
    "computer_click",
    "computer_drag",
    "computer_type",
    "computer_key",
    "computer_windows",
    "computer_focus_window",
    "computer_resize_window",
    "computer_stop",
];

/// Return the schemas for all computer-use tools.
pub fn schemas() -> Vec<Value> {
    vec![
        tool(
            "computer_screenshot",
            "Capture a real monitor frame, overlay a coordinate grid, and save it as a workspace attachment. Requires VARYNTH_COMPUTER_USE=1.",
            json!({
                "type": "object",
                "properties": {
                    "monitor": {"type": "integer", "minimum": 0, "maximum": 32},
                    "target_window": {"type": "string", "maxLength": 256},
                    "grid": {"type": "boolean"}
                },
                "additionalProperties": false
            }),
        ),
        tool(
            "computer_click",
            "Click a bounded raster coordinate in an explicitly selected window. Requires VARYNTH_COMPUTER_USE=1.",
            json!({
                "type": "object",
                "properties": {
                    "x": {"type": "integer", "minimum": -100000, "maximum": 100000},
                    "y": {"type": "integer", "minimum": -100000, "maximum": 100000},
                    "button": {"type": "string", "enum": ["left", "middle", "right"]},
                    "target_window": {"type": "string", "maxLength": 256}
                },
                "required": ["x", "y", "target_window"],
                "additionalProperties": false
            }),
        ),
        tool(
            "computer_drag",
            "Drag between two bounded raster coordinates with a bounded number of checked steps. Requires VARYNTH_COMPUTER_USE=1.",
            json!({
                "type": "object",
                "properties": {
                    "from": {"type": "array", "items": {"type": "integer"}, "minItems": 2, "maxItems": 2},
                    "to": {"type": "array", "items": {"type": "integer"}, "minItems": 2, "maxItems": 2},
                    "button": {"type": "string", "enum": ["left", "middle", "right"]},
                    "duration_ms": {"type": "integer", "minimum": 0, "maximum": 30000},
                    "target_window": {"type": "string", "maxLength": 256}
                },
                "required": ["from", "to", "target_window"],
                "additionalProperties": false
            }),
        ),
        tool(
            "computer_type",
            "Type bounded UTF-8 text in checked chunks into an explicitly selected window. Requires VARYNTH_COMPUTER_USE=1.",
            json!({
                "type": "object",
                "properties": {
                    "text": {"type": "string", "maxLength": 65536},
                    "target_window": {"type": "string", "maxLength": 256}
                },
                "required": ["text", "target_window"],
                "additionalProperties": false
            }),
        ),
        tool(
            "computer_key",
            "Press a normalized whitelist key or combo such as Ctrl+C, Alt+Tab, or Enter. Requires VARYNTH_COMPUTER_USE=1.",
            json!({
                "type": "object",
                "properties": {
                    "key": {"type": "string", "maxLength": 128},
                    "target_window": {"type": "string", "maxLength": 256}
                },
                "required": ["key", "target_window"],
                "additionalProperties": false
            }),
        ),
        tool(
            "computer_windows",
            "Enumerate visible titled Windows windows without process arguments. Requires VARYNTH_COMPUTER_USE=1.",
            json!({
                "type": "object",
                "properties": {"query": {"type": "string", "maxLength": 128}},
                "additionalProperties": false
            }),
        ),
        tool(
            "computer_focus_window",
            "Focus one existing Windows window by exact title or hwnd:<number>. Requires VARYNTH_COMPUTER_USE=1.",
            json!({
                "type": "object",
                "properties": {"target_window": {"type": "string", "maxLength": 256}},
                "required": ["target_window"],
                "additionalProperties": false
            }),
        ),
        tool(
            "computer_resize_window",
            "Resize one existing Windows window. Requires VARYNTH_COMPUTER_USE=1.",
            json!({
                "type": "object",
                "properties": {
                    "target_window": {"type": "string", "maxLength": 256},
                    "width": {"type": "integer", "minimum": 1, "maximum": 16384},
                    "height": {"type": "integer", "minimum": 1, "maximum": 16384}
                },
                "required": ["target_window", "width", "height"],
                "additionalProperties": false
            }),
        ),
        tool(
            "computer_stop",
            "Set the process-wide computer-use stop flag. This is always available, including when computer use is disabled.",
            json!({"type": "object", "additionalProperties": false}),
        ),
    ]
}

fn tool(name: &str, description: &str, parameters: Value) -> Value {
    json!({
        "type": "function",
        "function": {
            "name": name,
            "description": description,
            "parameters": parameters
        }
    })
}

/// Whether `name` is one of the computer-use tools.
pub fn handles(name: &str) -> bool {
    TOOL_NAMES.contains(&name)
}

/// Public context-free dispatch. Normal tool calls should use
/// `crate::tools::dispatch`, which supplies the jail-aware entry point.
pub fn dispatch(name: &str, args: &Value) -> Result<String> {
    if name == "computer_stop" {
        return stop();
    }
    let cwd = std::env::current_dir().context("cannot determine current workspace")?;
    let jail = Jail::new(cwd, Vec::new(), SandboxMode::ReadOnly, Vec::new());
    dispatch_in_jail(name, args, &jail)
}

/// Dispatch with explicit sandbox context. Runtime approval is separate from
/// this fail-closed opt-in/sandbox check.
pub fn dispatch_in_jail(name: &str, args: &Value, jail: &Jail) -> Result<String> {
    dispatch_with_policy(name, args, jail, computer_use_enabled())
}

fn stop() -> Result<String> {
    STOP_REQUESTED.store(true, Ordering::SeqCst);
    Ok(json!({"stopped": true, "message": "computer-use stop flag set; restart the process to resume"}).to_string())
}

fn dispatch_with_policy(name: &str, args: &Value, jail: &Jail, enabled: bool) -> Result<String> {
    if !handles(name) {
        bail!("unknown computer tool: {name}");
    }
    if name == "computer_stop" {
        return stop();
    }
    if !enabled {
        bail!("computer use is disabled; set {ENABLE_ENV}=1 for explicit opt-in");
    }
    if jail.mode == SandboxMode::DockerIsolated {
        bail!("desktop computer use is blocked in docker-isolated sandbox mode");
    }
    if is_input_tool(name) && jail.mode == SandboxMode::ReadOnly {
        bail!("desktop input is blocked in read-only sandbox mode");
    }
    if !args.is_object() {
        bail!("computer tool arguments must be an object");
    }
    ensure_not_stopped()?;
    let _action = ACTION_LOCK
        .try_lock()
        .map_err(|_| anyhow::anyhow!("another computer-use action is running"))?;
    ensure_not_stopped()?;
    let _dpi = DesktopDpiGuard::new()?;

    match name {
        "computer_screenshot" => screenshot(args, jail),
        "computer_click" => click(args),
        "computer_drag" => drag(args),
        "computer_type" => type_text(args),
        "computer_key" => key(args),
        "computer_windows" => windows(args),
        "computer_focus_window" => focus_window(args),
        "computer_resize_window" => resize_window(args),
        "computer_stop" => unreachable!(),
        _ => bail!("unknown computer tool: {name}"),
    }
}

fn computer_use_enabled() -> bool {
    std::env::var(ENABLE_ENV)
        .map(|value| value.trim() == "1")
        .unwrap_or(false)
}

fn is_input_tool(name: &str) -> bool {
    matches!(
        name,
        "computer_click"
            | "computer_drag"
            | "computer_type"
            | "computer_key"
            | "computer_focus_window"
            | "computer_resize_window"
    )
}

fn arg_str(args: &Value, key: &str) -> Result<String> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| anyhow::anyhow!("missing string arg `{key}`"))
}

fn bounded_string(args: &Value, key: &str, max: usize) -> Result<String> {
    let value = arg_str(args, key)?;
    if value.is_empty() {
        bail!("`{key}` must not be empty");
    }
    if value.len() > max {
        bail!("`{key}` exceeds {max} bytes");
    }
    Ok(value)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Point {
    x: i32,
    y: i32,
}

fn integer_arg(args: &Value, key: &str) -> Result<i32> {
    let value = args
        .get(key)
        .and_then(Value::as_i64)
        .ok_or_else(|| anyhow::anyhow!("missing integer arg `{key}`"))?;
    i32::try_from(value).map_err(|_| anyhow::anyhow!("`{key}` is outside i32 range"))
}

fn checked_point(x: i32, y: i32) -> Result<Point> {
    if !(-MAX_COORDINATE..=MAX_COORDINATE).contains(&x)
        || !(-MAX_COORDINATE..=MAX_COORDINATE).contains(&y)
    {
        bail!("raster coordinate exceeds +/-{MAX_COORDINATE}");
    }
    Ok(Point { x, y })
}

fn point_arg(args: &Value, key: &str) -> Result<Point> {
    let value = args
        .get(key)
        .ok_or_else(|| anyhow::anyhow!("missing point arg `{key}`"))?;
    if let Some(values) = value.as_array() {
        if values.len() != 2 {
            bail!("`{key}` must contain exactly two integer coordinates");
        }
        let x = values[0]
            .as_i64()
            .ok_or_else(|| anyhow::anyhow!("`{key}[0]` must be an integer"))?;
        let y = values[1]
            .as_i64()
            .ok_or_else(|| anyhow::anyhow!("`{key}[1]` must be an integer"))?;
        return checked_point(
            i32::try_from(x).map_err(|_| anyhow::anyhow!("`{key}[0]` is outside i32 range"))?,
            i32::try_from(y).map_err(|_| anyhow::anyhow!("`{key}[1]` is outside i32 range"))?,
        );
    }
    if let Some(object) = value.as_object() {
        let x = object
            .get("x")
            .and_then(Value::as_i64)
            .ok_or_else(|| anyhow::anyhow!("`{key}.x` must be an integer"))?;
        let y = object
            .get("y")
            .and_then(Value::as_i64)
            .ok_or_else(|| anyhow::anyhow!("`{key}.y` must be an integer"))?;
        return checked_point(
            i32::try_from(x).map_err(|_| anyhow::anyhow!("`{key}.x` is outside i32 range"))?,
            i32::try_from(y).map_err(|_| anyhow::anyhow!("`{key}.y` is outside i32 range"))?,
        );
    }
    bail!("`{key}` must be a two-element integer array or object");
}

fn optional_str<'a>(args: &'a Value, key: &str, default: &'a str) -> Result<&'a str> {
    match args.get(key) {
        None => Ok(default),
        Some(value) => value
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("`{key}` must be a string")),
    }
}

fn optional_u64(args: &Value, key: &str, default: u64, max: u64) -> Result<u64> {
    let value = match args.get(key) {
        None => default,
        Some(value) => value
            .as_u64()
            .ok_or_else(|| anyhow::anyhow!("`{key}` must be a nonnegative integer"))?,
    };
    if value > max {
        bail!("`{key}` exceeds {max}");
    }
    Ok(value)
}

fn parse_button(args: &Value) -> Result<Button> {
    match optional_str(args, "button", "left")?
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "left" => Ok(Button::Left),
        "middle" => Ok(Button::Middle),
        "right" => Ok(Button::Right),
        other => bail!("unsupported mouse button `{other}`; use left, middle, or right"),
    }
}

fn target_window_arg(args: &Value) -> Result<String> {
    bounded_string(args, "target_window", 256)
}

fn new_enigo() -> Result<Enigo> {
    Enigo::new(&Settings::default())
        .map_err(|error| anyhow::anyhow!("cannot connect to desktop input: {error}"))
}

fn native_failsafe_keys_down() -> bool {
    #[cfg(windows)]
    {
        return windows_failsafe_keys_down();
    }
    #[cfg(not(windows))]
    {
        false
    }
}

struct DesktopDpiGuard;

impl DesktopDpiGuard {
    fn new() -> Result<Self> {
        // Native coordinate APIs are used directly; no process-wide DPI
        // mutation is required here. Keep a guard-shaped boundary so future
        // platform setup can be added without changing dispatch flow.
        Ok(Self)
    }
}

fn ensure_not_stopped() -> Result<()> {
    if STOP_REQUESTED.load(Ordering::SeqCst) {
        bail!("computer-use stop flag is set; restart the process to resume");
    }
    Ok(())
}

fn check_safety_state(location: (i32, i32), keys_down: bool) -> Result<()> {
    ensure_not_stopped()?;
    if keys_down {
        STOP_REQUESTED.store(true, Ordering::SeqCst);
        bail!("computer-use failsafe key combination detected");
    }
    if location == (0, 0) {
        STOP_REQUESTED.store(true, Ordering::SeqCst);
        bail!("computer-use failsafe cursor corner detected at (0,0)");
    }
    Ok(())
}

fn check_failsafe(enigo: &Enigo) -> Result<()> {
    ensure_not_stopped()?;
    let location = enigo
        .location()
        .map_err(|error| anyhow::anyhow!("cannot read cursor for failsafe: {error}"))?;
    check_safety_state(location, native_failsafe_keys_down())
}

fn prepare_target(args: &Value, enigo: &Enigo) -> Result<WindowInfo> {
    let target = target_window_arg(args)?;
    let info = resolve_window(&target)?;
    check_failsafe(enigo)?;
    focus_window_info(&info)?;
    if foreground_window_handle() != Some(info.handle) {
        bail!("target window `{target}` was not focused; refusing unrelated foreground input");
    }
    check_failsafe(enigo)?;
    Ok(info)
}

fn check_target(enigo: &Enigo, target: &WindowInfo) -> Result<()> {
    check_failsafe(enigo)?;
    if foreground_window_handle() != Some(target.handle) {
        bail!("target window lost foreground focus; refusing unrelated input");
    }
    Ok(())
}

fn ensure_point_in_target(point: Point, target: &WindowInfo) -> Result<()> {
    let rect = target.rect;
    if point.x < rect.left || point.x >= rect.right || point.y < rect.top || point.y >= rect.bottom
    {
        bail!("coordinate is outside the selected target window");
    }
    if point == (Point { x: 0, y: 0 }) {
        bail!("(0,0) is reserved for the corner failsafe");
    }
    Ok(())
}

fn click(args: &Value) -> Result<String> {
    let point = checked_point(integer_arg(args, "x")?, integer_arg(args, "y")?)?;
    let button = parse_button(args)?;
    target_window_arg(args)?;
    ensure_point_on_screen(point)?;
    let mut enigo = new_enigo()?;
    let target = prepare_target(args, &enigo)?;
    ensure_point_in_target(point, &target)?;
    check_target(&enigo, &target)?;
    enigo
        .move_mouse(point.x, point.y, Coordinate::Abs)
        .context("move failed")?;
    check_target(&enigo, &target)?;
    let pressed = enigo.button(button, Direction::Press);
    let released = enigo.button(button, Direction::Release);
    pressed.context("click press failed")?;
    released.context("click release failed")?;
    Ok(
        json!({"clicked": {"x": point.x, "y": point.y}, "button": format_button(button)})
            .to_string(),
    )
}

fn drag(args: &Value) -> Result<String> {
    let from = point_arg(args, "from")?;
    let to = point_arg(args, "to")?;
    let button = parse_button(args)?;
    target_window_arg(args)?;
    let duration_ms = optional_u64(args, "duration_ms", 400, 30_000)?;
    ensure_point_on_screen(from)?;
    ensure_point_on_screen(to)?;
    let mut enigo = new_enigo()?;
    let target = prepare_target(args, &enigo)?;
    ensure_point_in_target(from, &target)?;
    ensure_point_in_target(to, &target)?;
    check_target(&enigo, &target)?;
    enigo
        .move_mouse(from.x, from.y, Coordinate::Abs)
        .context("drag start move failed")?;
    check_target(&enigo, &target)?;
    if let Err(error) = enigo.button(button, Direction::Press) {
        let _ = enigo.button(button, Direction::Release);
        bail!("drag press failed: {error}");
    }
    let distance = (to.x - from.x).abs().max((to.y - from.y).abs());
    let steps = distance.clamp(1, 120) as usize;
    let result = (|| -> Result<()> {
        for step in 1..=steps {
            check_target(&enigo, &target)?;
            let x = from.x + ((i64::from(to.x - from.x) * step as i64) / steps as i64) as i32;
            let y = from.y + ((i64::from(to.y - from.y) * step as i64) / steps as i64) as i32;
            ensure_point_on_screen(Point { x, y })?;
            ensure_point_in_target(Point { x, y }, &target)?;
            enigo
                .move_mouse(x, y, Coordinate::Abs)
                .context("drag move failed")?;
            let mut remaining = duration_ms / steps as u64;
            while remaining > 0 {
                let interval = remaining.min(20);
                std::thread::sleep(Duration::from_millis(interval));
                remaining -= interval;
                check_target(&enigo, &target)?;
            }
        }
        Ok(())
    })();
    // Release even when input was cancelled, lost focus, or a native call failed.
    let released = enigo
        .button(button, Direction::Release)
        .context("drag release failed");
    if let Err(error) = result {
        if let Err(release_error) = released {
            bail!("{error}; {release_error}");
        }
        return Err(error);
    }
    released?;
    Ok(json!({"dragged": {"from": [from.x, from.y], "to": [to.x, to.y]}, "button": format_button(button), "steps": steps}).to_string())
}

fn type_text(args: &Value) -> Result<String> {
    let text = bounded_string(args, "text", MAX_TEXT_BYTES)?;
    if text
        .chars()
        .any(|ch| ch.is_control() && ch != '\n' && ch != '\t' && ch != '\r')
    {
        bail!("`text` contains unsupported control characters");
    }
    target_window_arg(args)?;
    let mut enigo = new_enigo()?;
    let target = prepare_target(args, &enigo)?;
    let mut chunk = String::new();
    let mut chunks = 0usize;
    for character in text.chars() {
        chunk.push(character);
        if chunk.len() >= 16 {
            check_target(&enigo, &target)?;
            enigo.text(&chunk).context("text input failed")?;
            chunks += 1;
            chunk.clear();
        }
    }
    if !chunk.is_empty() {
        check_target(&enigo, &target)?;
        enigo.text(&chunk).context("text input failed")?;
        chunks += 1;
    }
    Ok(json!({"typed_bytes": text.len(), "chunks": chunks}).to_string())
}

fn key(args: &Value) -> Result<String> {
    let combo = bounded_string(args, "key", MAX_KEY_BYTES)?;
    let keys = parse_key_combo(&combo)?;
    target_window_arg(args)?;
    let mut enigo = new_enigo()?;
    let target = prepare_target(args, &enigo)?;
    let mut pressed = Vec::new();
    let result = (|| -> Result<()> {
        for key in keys {
            check_target(&enigo, &target)?;
            pressed.push(key);
            enigo
                .key(key, Direction::Press)
                .context("key press failed")?;
        }
        Ok(())
    })();
    let mut release_error = None;
    for key in pressed.into_iter().rev() {
        if let Err(error) = enigo.key(key, Direction::Release) {
            release_error = Some(anyhow::anyhow!("key release failed: {error}"));
        }
    }
    if let Err(error) = result {
        if let Some(release_error) = release_error {
            bail!("{error}; {release_error}");
        }
        return Err(error);
    }
    if let Some(error) = release_error {
        return Err(error);
    }
    Ok(json!({"key": normalize_combo(&combo)}).to_string())
}

fn parse_key_combo(combo: &str) -> Result<Vec<Key>> {
    if combo.trim().is_empty() || combo.len() > MAX_KEY_BYTES {
        bail!("key combo is empty or too long");
    }
    let mut result = Vec::new();
    let mut main_count = 0usize;
    for raw in combo.split('+') {
        let token = raw.trim();
        if token.is_empty() {
            bail!("key combo contains an empty component");
        }
        let (key, modifier) = parse_key_token(token)?;
        if modifier && main_count != 0 {
            bail!("modifiers must precede the main key");
        }
        if result.contains(&key) {
            bail!("duplicate key component");
        }
        if !modifier {
            main_count += 1;
        }
        result.push(key);
    }
    if main_count != 1 {
        bail!("key combo must contain exactly one non-modifier key");
    }
    if result.len() > 4 {
        bail!("key combo has too many components");
    }
    Ok(result)
}

fn normalize_combo(combo: &str) -> String {
    combo
        .split('+')
        .map(|part| part.trim().to_ascii_uppercase())
        .collect::<Vec<_>>()
        .join("+")
}

fn parse_key_token(token: &str) -> Result<(Key, bool)> {
    let normalized = token.trim().to_ascii_uppercase();
    let key = match normalized.as_str() {
        "CTRL" | "CONTROL" => return Ok((Key::Control, true)),
        "ALT" | "OPTION" => return Ok((Key::Alt, true)),
        "SHIFT" => return Ok((Key::Shift, true)),
        "META" | "WIN" | "WINDOWS" | "CMD" | "COMMAND" => return Ok((Key::Meta, true)),
        "BACKSPACE" | "BACK" => Key::Backspace,
        "DELETE" | "DEL" => Key::Delete,
        "DOWN" | "ARROWDOWN" => Key::DownArrow,
        "END" => Key::End,
        "ENTER" | "RETURN" => Key::Return,
        "ESC" | "ESCAPE" => Key::Escape,
        "HOME" => Key::Home,
        "LEFT" | "ARROWLEFT" => Key::LeftArrow,
        "RIGHT" | "ARROWRIGHT" => Key::RightArrow,
        "PAGEDOWN" => Key::PageDown,
        "PAGEUP" => Key::PageUp,
        "SPACE" => Key::Space,
        "TAB" => Key::Tab,
        "UP" | "ARROWUP" => Key::UpArrow,
        name if name.len() == 2
            && name.starts_with('F')
            && name[1..].parse::<u8>().ok().is_some() =>
        {
            match name[1..].parse::<u8>().unwrap() {
                1 => Key::F1,
                2 => Key::F2,
                3 => Key::F3,
                4 => Key::F4,
                5 => Key::F5,
                6 => Key::F6,
                7 => Key::F7,
                8 => Key::F8,
                9 => Key::F9,
                _ => bail!("unsupported function key `{token}`"),
            }
        }
        name if name.len() == 3
            && name.starts_with('F')
            && name[1..].parse::<u8>().ok().is_some() =>
        {
            match name[1..].parse::<u8>().unwrap() {
                10 => Key::F10,
                11 => Key::F11,
                12 => Key::F12,
                13 => Key::F13,
                14 => Key::F14,
                15 => Key::F15,
                16 => Key::F16,
                17 => Key::F17,
                18 => Key::F18,
                19 => Key::F19,
                20 => Key::F20,
                _ => bail!("unsupported function key `{token}`"),
            }
        }
        name if name.len() == 1 && name.as_bytes()[0].is_ascii_alphanumeric() => {
            return Ok((
                Key::Unicode(name.chars().next().unwrap().to_ascii_lowercase()),
                false,
            ));
        }
        _ => bail!("key `{token}` is not on the whitelist"),
    };
    Ok((key, false))
}

fn format_button(button: Button) -> &'static str {
    match button {
        Button::Left => "left",
        Button::Middle => "middle",
        Button::Right => "right",
        _ => "other",
    }
}

fn ensure_point_on_screen(point: Point) -> Result<()> {
    let screens = screenshots::Screen::all().context("cannot enumerate screens")?;
    // Enigo 0.2 on Windows maps absolute input onto the primary screen only.
    // Refuse secondary-monitor input rather than mis-targeting a coordinate.
    if screens.iter().any(|screen| {
        let info = screen.display_info;
        #[cfg(windows)]
        if !info.is_primary {
            return false;
        }
        point.x >= info.x
            && point.y >= info.y
            && point.x
                < info
                    .x
                    .saturating_add(info.width.min(i32::MAX as u32) as i32)
            && point.y
                < info
                    .y
                    .saturating_add(info.height.min(i32::MAX as u32) as i32)
    }) {
        Ok(())
    } else {
        bail!(
            "raster coordinate ({}, {}) is outside supported input screen bounds",
            point.x,
            point.y
        );
    }
}

fn screenshot(args: &Value, jail: &Jail) -> Result<String> {
    let target = args
        .get("target_window")
        .map(|_| target_window_arg(args))
        .transpose()?;
    let monitor_index = optional_u64(args, "monitor", 0, 32)?;
    let grid = match args.get("grid") {
        None => true,
        Some(value) => value
            .as_bool()
            .ok_or_else(|| anyhow::anyhow!("`grid` must be a boolean"))?,
    };
    let target_info = target.as_deref().map(resolve_window).transpose()?;
    let directory = attachment_directory(jail)?;
    let screens = screenshots::Screen::all().context("cannot enumerate screens")?;
    let selected = if let Some(info) = target_info.as_ref() {
        let center_x = (i64::from(info.rect.left) + i64::from(info.rect.right)) / 2;
        let center_y = (i64::from(info.rect.top) + i64::from(info.rect.bottom)) / 2;
        screens
            .iter()
            .find(|screen| {
                let display = screen.display_info;
                center_x >= i64::from(display.x)
                    && center_x < i64::from(display.x) + i64::from(display.width)
                    && center_y >= i64::from(display.y)
                    && center_y < i64::from(display.y) + i64::from(display.height)
            })
            .ok_or_else(|| anyhow::anyhow!("target window is not on an available screen"))?
    } else {
        screens
            .get(monitor_index as usize)
            .ok_or_else(|| anyhow::anyhow!("monitor index {monitor_index} is unavailable"))?
    };
    let display = selected.display_info;
    if !display.scale_factor.is_finite() || display.scale_factor <= 0.0 {
        bail!("monitor has an invalid scale factor");
    }
    let predicted_width = (f64::from(display.width) * f64::from(display.scale_factor)).round();
    let predicted_height = (f64::from(display.height) * f64::from(display.scale_factor)).round();
    if predicted_width > f64::from(MAX_SCREEN_WIDTH)
        || predicted_height > f64::from(MAX_SCREEN_HEIGHT)
        || predicted_width * predicted_height > MAX_SCREEN_PIXELS as f64
    {
        bail!("monitor capture would exceed the pixel limit");
    }
    let enigo = new_enigo()?;
    check_failsafe(&enigo)?;
    let mut image = selected.capture().context("screen capture failed")?;
    validate_image(&image)?;
    let mut origin_x = display.x;
    let mut origin_y = display.y;
    if let Some(info) = target_info.as_ref() {
        image = crop_to_window(image, display, info.rect)?;
        origin_x = info.rect.left.max(display.x);
        origin_y = info.rect.top.max(display.y);
    }
    if grid {
        overlay_coordinate_grid(&mut image, GRID_MINOR, GRID_MAJOR);
    }
    validate_image(&image)?;
    check_failsafe(&enigo)?;
    let path = save_attachment(jail, &directory, &image)?;
    Ok(json!({
        "attachment": {"type": "image", "mime_type": "image/png", "path": path},
        "media": [{"type": "image", "mime_type": "image/png", "path": path}],
        "monitor": {"id": display.id, "x": display.x, "y": display.y, "width": display.width, "height": display.height, "scale_factor": display.scale_factor},
        "window": target_info.map(|info| info.to_json()),
        "raster": {"origin_x": origin_x, "origin_y": origin_y, "scale": display.scale_factor, "width": image.width(), "height": image.height()},
        "coordinate_mapping": "desktop_x = origin_x + raster_x / scale; desktop_y = origin_y + raster_y / scale",
        "grid_minor": if grid {GRID_MINOR} else {0},
        "grid_major": if grid {GRID_MAJOR} else {0}
    }).to_string())
}

fn validate_image(image: &screenshots::image::RgbaImage) -> Result<()> {
    let (width, height) = image.dimensions();
    if width == 0
        || height == 0
        || width > MAX_SCREEN_WIDTH
        || height > MAX_SCREEN_HEIGHT
        || u64::from(width) * u64::from(height) > MAX_SCREEN_PIXELS
    {
        bail!("captured image dimensions are invalid or exceed the pixel limit: {width}x{height}");
    }
    let expected = usize::try_from(u64::from(width) * u64::from(height) * 4)
        .map_err(|_| anyhow::anyhow!("captured image size overflow"))?;
    if image.as_raw().len() != expected {
        bail!("captured image pixel buffer length is invalid");
    }
    Ok(())
}

/// Draw a grid over the captured pixels. No pixels are synthesized outside
/// the supplied image; every line is blended into an existing pixel.
pub(crate) fn overlay_coordinate_grid(
    image: &mut screenshots::image::RgbaImage,
    minor_spacing: u32,
    major_spacing: u32,
) {
    if minor_spacing == 0 || major_spacing == 0 {
        return;
    }
    let (width, height) = image.dimensions();
    for y in 0..height {
        for x in 0..width {
            let minor = x % minor_spacing == 0 || y % minor_spacing == 0;
            if !minor {
                continue;
            }
            let major = x % major_spacing == 0 || y % major_spacing == 0;
            let pixel = image.get_pixel_mut(x, y);
            let overlay: [u8; 4] = if major {
                [255, 64, 64, 210]
            } else {
                [255, 160, 64, 150]
            };
            let alpha = u16::from(overlay[3]);
            for channel in 0..3 {
                pixel[channel] = ((u16::from(pixel[channel]) * (255 - alpha)
                    + u16::from(overlay[channel]) * alpha)
                    / 255) as u8;
            }
            pixel[3] = 255;
        }
    }
}

fn attachment_directory(jail: &Jail) -> Result<std::path::PathBuf> {
    let directory = jail.assert_write(".varynth/attachments")?;
    let root = jail
        .cwd
        .canonicalize()
        .context("cannot resolve workspace attachment root")?;
    let normalized_root = root
        .to_string_lossy()
        .trim_start_matches(r"\\?\")
        .to_owned();
    if !directory.starts_with(std::path::Path::new(&normalized_root)) {
        bail!("screenshot attachment directory escapes the workspace");
    }
    Ok(directory)
}

fn save_attachment(
    jail: &Jail,
    directory: &std::path::Path,
    image: &screenshots::image::RgbaImage,
) -> Result<String> {
    let safe_directory = attachment_directory(jail)?;
    if safe_directory != directory {
        bail!("screenshot attachment directory changed during capture");
    }
    fs::create_dir_all(&safe_directory)?;
    let sequence = ATTACHMENT_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let filename = format!("computer-{millis}-{sequence}.png");
    let path = jail.assert_write(&format!(".varynth/attachments/{filename}"))?;
    if path.parent() != Some(safe_directory.as_path()) {
        bail!("screenshot attachment path escapes the workspace");
    }
    let file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)?;
    let mut encoder = png::Encoder::new(file, image.width(), image.height());
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.write_header()?.write_image_data(image.as_raw())?;
    Ok(path.to_string_lossy().into_owned())
}

#[derive(Debug, Clone, Copy)]
struct Rect {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}

#[derive(Debug, Clone)]
struct WindowInfo {
    handle: isize,
    title: String,
    rect: Rect,
    process_id: u32,
}

impl WindowInfo {
    fn to_json(&self) -> Value {
        json!({
            "handle": format!("hwnd:{}", self.handle),
            "title": self.title,
            "rect": {
                "left": self.rect.left,
                "top": self.rect.top,
                "right": self.rect.right,
                "bottom": self.rect.bottom
            },
            "process_id": self.process_id
        })
    }
}

fn windows(args: &Value) -> Result<String> {
    let query = optional_str(args, "query", "")?.trim().to_owned();
    if query.len() > 128 {
        bail!("`query` exceeds 128 bytes");
    }
    let mut values = enumerate_windows()?;
    if !query.is_empty() {
        let query = query.to_ascii_lowercase();
        values.retain(|window| window.title.to_ascii_lowercase().contains(&query));
    }
    values.truncate(MAX_WINDOWS);
    Ok(json!({"windows": values.iter().map(WindowInfo::to_json).collect::<Vec<_>>()}).to_string())
}

fn focus_window(args: &Value) -> Result<String> {
    let target = target_window_arg(args)?;
    let info = resolve_window(&target)?;
    let enigo = new_enigo()?;
    check_failsafe(&enigo)?;
    focus_window_info(&info)?;
    check_failsafe(&enigo)?;
    Ok(json!({"focused": info.to_json()}).to_string())
}

fn resize_window(args: &Value) -> Result<String> {
    let target = target_window_arg(args)?;
    let width = args
        .get("width")
        .and_then(Value::as_u64)
        .ok_or_else(|| anyhow::anyhow!("missing integer arg `width`"))?;
    let height = args
        .get("height")
        .and_then(Value::as_u64)
        .ok_or_else(|| anyhow::anyhow!("missing integer arg `height`"))?;
    if !(1..=16_384).contains(&width) || !(1..=16_384).contains(&height) {
        bail!("window dimensions must be between 1 and 16384");
    }
    let info = resolve_window(&target)?;
    let enigo = new_enigo()?;
    check_failsafe(&enigo)?;
    resize_window_info(&info, width as i32, height as i32)?;
    check_failsafe(&enigo)?;
    Ok(json!({"resized": info.to_json(), "width": width, "height": height}).to_string())
}

#[cfg(windows)]
fn enumerate_windows() -> Result<Vec<WindowInfo>> {
    win32::enumerate_windows()
}

#[cfg(not(windows))]
fn enumerate_windows() -> Result<Vec<WindowInfo>> {
    bail!("window enumeration/focus/resize is only supported on Windows")
}

#[cfg(windows)]
fn resolve_window(target: &str) -> Result<WindowInfo> {
    let windows = enumerate_windows()?;
    let by_handle = target
        .strip_prefix("hwnd:")
        .or_else(|| target.parse::<isize>().ok().map(|_| target));
    if let Some(handle) = by_handle {
        let handle = handle
            .parse::<isize>()
            .map_err(|_| anyhow::anyhow!("invalid target window handle `{target}`"))?;
        return windows
            .into_iter()
            .find(|window| window.handle == handle)
            .ok_or_else(|| anyhow::anyhow!("target window `{target}` does not exist"));
    }
    let matches: Vec<_> = windows
        .into_iter()
        .filter(|window| window.title == target)
        .collect();
    match matches.len() {
        0 => bail!("target window `{target}` does not exist"),
        1 => Ok(matches.into_iter().next().unwrap()),
        _ => bail!("target window title `{target}` is ambiguous; use hwnd:<number>"),
    }
}

#[cfg(not(windows))]
fn resolve_window(_target: &str) -> Result<WindowInfo> {
    bail!("window targeting is unavailable on this operating system")
}

#[cfg(windows)]
fn foreground_window_handle() -> Option<isize> {
    win32::foreground_window_handle()
}

#[cfg(not(windows))]
fn foreground_window_handle() -> Option<isize> {
    None
}

#[cfg(windows)]
fn focus_window_info(info: &WindowInfo) -> Result<()> {
    win32::focus_window(info.handle)
}

#[cfg(not(windows))]
fn focus_window_info(_info: &WindowInfo) -> Result<()> {
    bail!("window focus is unavailable on this operating system")
}

#[cfg(windows)]
fn resize_window_info(info: &WindowInfo, width: i32, height: i32) -> Result<()> {
    win32::resize_window(info.handle, info.rect.left, info.rect.top, width, height)
}

#[cfg(not(windows))]
fn resize_window_info(_info: &WindowInfo, _width: i32, _height: i32) -> Result<()> {
    bail!("window resize is unavailable on this operating system")
}

#[cfg(windows)]
fn crop_to_window(
    image: screenshots::image::RgbaImage,
    display: screenshots::display_info::DisplayInfo,
    rect: Rect,
) -> Result<screenshots::image::RgbaImage> {
    let scale = if display.scale_factor.is_finite() && display.scale_factor > 0.0 {
        f64::from(display.scale_factor)
    } else {
        1.0
    };
    let left = ((rect.left - display.x) as f64 * scale).round().max(0.0) as u32;
    let top = ((rect.top - display.y) as f64 * scale).round().max(0.0) as u32;
    let right = ((rect.right - display.x) as f64 * scale).round().max(0.0) as u32;
    let bottom = ((rect.bottom - display.y) as f64 * scale).round().max(0.0) as u32;
    let right = right.min(image.width());
    let bottom = bottom.min(image.height());
    if left >= right || top >= bottom {
        bail!("target window rectangle does not intersect captured pixels");
    }
    Ok(
        screenshots::image::imageops::crop_imm(&image, left, top, right - left, bottom - top)
            .to_image(),
    )
}

#[cfg(not(windows))]
fn crop_to_window(
    _image: screenshots::image::RgbaImage,
    _display: screenshots::display_info::DisplayInfo,
    _rect: Rect,
) -> Result<screenshots::image::RgbaImage> {
    bail!("window screenshot targeting is unavailable on this operating system")
}

#[cfg(windows)]
fn windows_failsafe_keys_down() -> bool {
    win32::failsafe_keys_down()
}

#[cfg(windows)]
mod win32 {
    use super::{Rect, WindowInfo, MAX_WINDOWS};
    use anyhow::bail;
    use anyhow::Result;

    type Hwnd = isize;
    type Bool = i32;

    #[repr(C)]
    struct NativeRect {
        left: i32,
        top: i32,
        right: i32,
        bottom: i32,
    }

    #[link(name = "user32")]
    extern "system" {
        fn EnumWindows(
            callback: Option<unsafe extern "system" fn(Hwnd, isize) -> Bool>,
            lparam: isize,
        ) -> Bool;
        fn GetWindowTextLengthW(hwnd: Hwnd) -> i32;
        fn GetWindowTextW(hwnd: Hwnd, text: *mut u16, max_count: i32) -> i32;
        fn IsWindowVisible(hwnd: Hwnd) -> Bool;
        fn GetWindowRect(hwnd: Hwnd, rect: *mut NativeRect) -> Bool;
        fn GetWindowThreadProcessId(hwnd: Hwnd, process_id: *mut u32) -> u32;
        fn GetForegroundWindow() -> Hwnd;
        fn SetForegroundWindow(hwnd: Hwnd) -> Bool;
        fn MoveWindow(hwnd: Hwnd, x: i32, y: i32, width: i32, height: i32, repaint: Bool) -> Bool;
        fn GetAsyncKeyState(key: i32) -> i16;
    }

    pub fn enumerate_windows() -> Result<Vec<WindowInfo>> {
        let mut windows = Vec::with_capacity(MAX_WINDOWS.min(32));
        let result = unsafe { EnumWindows(Some(enum_callback), &mut windows as *mut _ as isize) };
        if result == 0 {
            bail!("EnumWindows failed");
        }
        Ok(windows)
    }

    unsafe extern "system" fn enum_callback(hwnd: Hwnd, lparam: isize) -> Bool {
        let windows = &mut *(lparam as *mut Vec<WindowInfo>);
        if windows.len() >= MAX_WINDOWS || IsWindowVisible(hwnd) == 0 {
            return if windows.len() >= MAX_WINDOWS { 0 } else { 1 };
        }
        let length = GetWindowTextLengthW(hwnd).max(0).min(4096) as usize;
        if length == 0 {
            return 1;
        }
        let mut buffer = vec![0u16; length + 1];
        let written = GetWindowTextW(hwnd, buffer.as_mut_ptr(), buffer.len() as i32);
        if written <= 0 {
            return 1;
        }
        let title = String::from_utf16_lossy(&buffer[..written as usize]);
        let mut native_rect = NativeRect {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        };
        if GetWindowRect(hwnd, &mut native_rect) == 0 {
            return 1;
        }
        let mut process_id = 0;
        GetWindowThreadProcessId(hwnd, &mut process_id);
        windows.push(WindowInfo {
            handle: hwnd,
            title,
            rect: Rect {
                left: native_rect.left,
                top: native_rect.top,
                right: native_rect.right,
                bottom: native_rect.bottom,
            },
            process_id,
        });
        1
    }

    pub fn foreground_window_handle() -> Option<Hwnd> {
        let hwnd = unsafe { GetForegroundWindow() };
        (hwnd != 0).then_some(hwnd)
    }

    pub fn focus_window(hwnd: Hwnd) -> Result<()> {
        if unsafe { SetForegroundWindow(hwnd) } == 0 {
            bail!("SetForegroundWindow failed for hwnd:{hwnd}");
        }
        Ok(())
    }

    pub fn resize_window(hwnd: Hwnd, x: i32, y: i32, width: i32, height: i32) -> Result<()> {
        if unsafe { MoveWindow(hwnd, x, y, width, height, 1) } == 0 {
            bail!("MoveWindow failed for hwnd:{hwnd}");
        }
        Ok(())
    }

    pub fn failsafe_keys_down() -> bool {
        const VK_ESCAPE: i32 = 0x1b;
        const VK_CONTROL: i32 = 0x11;
        const VK_MENU: i32 = 0x12;
        const VK_Q: i32 = 0x51;
        unsafe {
            (GetAsyncKeyState(VK_ESCAPE) as u16 & 0x8000) != 0
                || ((GetAsyncKeyState(VK_CONTROL) as u16 & 0x8000) != 0
                    && (GetAsyncKeyState(VK_MENU) as u16 & 0x8000) != 0
                    && (GetAsyncKeyState(VK_Q) as u16 & 0x8000) != 0)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn clear_stop() {
        STOP_REQUESTED.store(false, Ordering::SeqCst);
    }

    #[test]
    fn schemas_cover_every_tool() {
        let schema_values = schemas();
        let names: Vec<_> = schema_values
            .iter()
            .filter_map(|schema| schema["function"]["name"].as_str())
            .collect();
        assert_eq!(names.len(), TOOL_NAMES.len());
        for name in TOOL_NAMES {
            assert!(names.contains(name));
            assert!(handles(name));
        }
        assert!(!handles("computer_unknown"));
    }

    #[test]
    fn disabled_dispatch_has_no_attachment_side_effect() {
        let _guard = TEST_LOCK.lock().unwrap();
        clear_stop();
        let dir = tempfile::tempdir().unwrap();
        let jail = Jail::new(
            dir.path().to_path_buf(),
            Vec::new(),
            SandboxMode::WorkspaceWrite,
            Vec::new(),
        );
        let error =
            dispatch_with_policy("computer_screenshot", &json!({}), &jail, false).unwrap_err();
        assert!(error.to_string().contains("disabled"));
        assert!(!dir.path().join(".varynth").exists());
    }

    #[test]
    fn stop_is_available_when_disabled_and_blocks_future_actions() {
        let _guard = TEST_LOCK.lock().unwrap();
        clear_stop();
        let dir = tempfile::tempdir().unwrap();
        let jail = Jail::new(
            dir.path().to_path_buf(),
            Vec::new(),
            SandboxMode::WorkspaceWrite,
            Vec::new(),
        );
        let output = dispatch_with_policy("computer_stop", &json!({}), &jail, false).unwrap();
        assert!(output.contains("stopped"));
        let error = dispatch_with_policy(
            "computer_type",
            &json!({"text": "x", "target_window": "x"}),
            &jail,
            true,
        )
        .unwrap_err();
        assert!(error.to_string().contains("stop flag"));
        clear_stop();
    }

    #[test]
    fn parses_buttons_points_and_whitelisted_keys_without_desktop_io() {
        assert_eq!(format_button(parse_button(&json!({})).unwrap()), "left");
        assert_eq!(
            format_button(parse_button(&json!({"button": "RIGHT"})).unwrap()),
            "right"
        );
        assert_eq!(
            point_arg(&json!({"p": [1, -2]}), "p").unwrap(),
            Point { x: 1, y: -2 }
        );
        assert!(point_arg(&json!({"p": [1.5, 2]}), "p").is_err());
        assert!(checked_point(MAX_COORDINATE + 1, 0).is_err());
        assert_eq!(parse_key_combo("Ctrl+C").unwrap().len(), 2);
        assert_eq!(parse_key_combo("Alt+Tab").unwrap().len(), 2);
        assert_eq!(parse_key_combo("Enter").unwrap().len(), 1);
        assert!(parse_key_combo("Ctrl+arbitrary").is_err());
        assert!(parse_key_combo("Ctrl+Alt+Q+Enter").is_err());
    }

    #[test]
    fn failsafe_state_is_checked_without_desktop_io() {
        let _guard = TEST_LOCK.lock().unwrap();
        clear_stop();
        assert!(check_safety_state((0, 0), false).is_err());
        clear_stop();
        assert!(check_safety_state((1, 1), true).is_err());
        clear_stop();
        assert!(check_safety_state((1, 1), false).is_ok());
    }

    #[test]
    fn grid_changes_only_existing_pixels_and_uses_major_lines() {
        let mut image = screenshots::image::RgbaImage::from_pixel(
            6,
            6,
            screenshots::image::Rgba([10, 20, 30, 255]),
        );
        let original = image.clone();
        overlay_coordinate_grid(&mut image, 2, 4);
        assert_ne!(image, original);
        assert_eq!(image.dimensions(), original.dimensions());
        assert_eq!(image.as_raw().len(), original.as_raw().len());
        assert_ne!(image.get_pixel(0, 0), original.get_pixel(1, 1));
        assert_ne!(image.get_pixel(4, 1), original.get_pixel(1, 1));
        assert_eq!(image.get_pixel(1, 1), original.get_pixel(1, 1));
    }

    #[test]
    fn read_only_blocks_input_before_platform_access() {
        let dir = tempfile::tempdir().unwrap();
        let jail = Jail::new(
            dir.path().to_path_buf(),
            Vec::new(),
            SandboxMode::ReadOnly,
            Vec::new(),
        );
        let error = dispatch_with_policy(
            "computer_click",
            &json!({"x": 1, "y": 1, "target_window": "x"}),
            &jail,
            true,
        )
        .unwrap_err();
        assert!(error.to_string().contains("read-only"));
    }
}
