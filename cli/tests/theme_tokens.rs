use serde_json::json;
use std::ffi::OsString;
use std::fs;
use std::path::PathBuf;
use std::sync::Mutex;

use varynth::theme_tokens::{
    load_theme, parse_theme_text, save_theme, theme_path, validate_theme_json, MAX_THEME_BYTES,
    THEME_FILE_NAME,
};

static HOME_LOCK: Mutex<()> = Mutex::new(());

struct HomeOverride {
    previous: Option<OsString>,
}

impl HomeOverride {
    fn set(path: PathBuf) -> Self {
        let previous = std::env::var_os("VARYNTH_HOME");
        std::env::set_var("VARYNTH_HOME", path);
        Self { previous }
    }
}

impl Drop for HomeOverride {
    fn drop(&mut self) {
        match self.previous.take() {
            Some(value) => std::env::set_var("VARYNTH_HOME", value),
            None => std::env::remove_var("VARYNTH_HOME"),
        }
    }
}

#[test]
fn json_validation_normalizes_css_names_and_drops_unknown_keys() {
    let value = json!({
        "--background": " 222.2 47.4% 11.2% ",
        "foreground": "#fff",
        "radius": "0.5rem",
        "not-allowed": "url(https://example.test/secret)"
    });

    let normalized = validate_theme_json(&value).unwrap();
    assert_eq!(
        normalized,
        json!({
            "background": "222.2 47.4% 11.2%",
            "foreground": "#fff",
            "radius": "0.5rem"
        })
    );
}

#[test]
fn css_parser_accepts_root_and_bare_custom_properties() {
    let root = parse_theme_text(
        r#":root {
            --background: hsl(222 47% 11% / 0.9);
            --primary: oklch(0.62 0.2 264 / 50%);
            --chart-1: color(display-p3 1 0 0 / .8);
            --radius: 0.625rem;
            --ignored: red;
        }"#,
    )
    .unwrap();
    assert_eq!(root["background"], "hsl(222 47% 11% / 0.9)");
    assert_eq!(root["primary"], "oklch(0.62 0.2 264 / 50%)");
    assert_eq!(root["chart-1"], "color(display-p3 1 0 0 / .8)");
    assert_eq!(root["radius"], "0.625rem");
    assert!(root.get("ignored").is_none());

    let bare = parse_theme_text("--foreground: rgb(240, 240, 240, 0.9); --border: #abc;").unwrap();
    assert_eq!(bare["foreground"], "rgb(240, 240, 240, 0.9)");
    assert_eq!(bare["border"], "#abc");
}

#[test]
fn supported_color_families_and_shadcn_hsl_channels_are_accepted() {
    let value = json!({
        "background": "#abcdef12",
        "card": "rgb(12 34 56 / 50%)",
        "primary": "rgba(12, 34, 56, .5)",
        "secondary": "hsl(10deg 20% 30% / .5)",
        "accent": "hsla(10, 20%, 30%, 50%)",
        "muted": "220 15% 20%",
        "border": "oklch(.5 .2 20)",
        "ring": "color(srgb 1 0 0)",
        "radius-lg": "8px"
    });

    let normalized = validate_theme_json(&value).unwrap();
    assert_eq!(normalized.as_object().unwrap().len(), 9);
}

#[test]
fn malformed_or_dangerous_theme_input_is_rejected() {
    assert!(validate_theme_json(&json!(["not", "an", "object"])).is_err());
    assert!(validate_theme_json(&json!({ "background": 12 })).is_err());
    assert!(validate_theme_json(&json!({ "background": "red" })).is_err());
    assert!(validate_theme_json(&json!({ "radius": "calc(1rem + 2px)" })).is_err());

    for text in [
        ":root { --background: url(https://example.test/x); }",
        "@import url(https://example.test/theme.css);",
        ":root { --background: javascript:alert(1); }",
        ":root { --background: #fff; } }",
        ":root { --background: rgb(1 2 3; }",
        ":root { --background: #fff; --unknown: { color: red; }; }",
    ] {
        assert!(
            parse_theme_text(text).is_err(),
            "accepted unsafe input: {text}"
        );
    }
}

#[test]
fn parser_rejects_oversized_input_and_invalid_css_shapes() {
    let oversized = "x".repeat(MAX_THEME_BYTES + 1);
    assert!(parse_theme_text(&oversized).is_err());
    assert!(parse_theme_text("[]").is_err());
    assert!(parse_theme_text("null").is_err());
    assert!(parse_theme_text(":root { --background: #fff;").is_err());
    assert!(parse_theme_text(".theme { --background: #fff; }").is_err());
    assert!(parse_theme_text("--background: #fff; color: red;").is_err());
}

#[test]
fn storage_uses_fixed_path_and_round_trips_atomically() {
    let _lock = HOME_LOCK.lock().unwrap();
    let home = tempfile::tempdir().unwrap();
    let _home = HomeOverride::set(home.path().to_path_buf());

    assert_eq!(theme_path(), home.path().join(THEME_FILE_NAME));
    assert_eq!(load_theme().unwrap(), json!({}));

    save_theme(&json!({
        "--background": "222 47% 11%",
        "foreground": "#fff",
        "unknown": "#000"
    }))
    .unwrap();

    let path = home.path().join(THEME_FILE_NAME);
    assert!(path.is_file());
    let loaded = load_theme().unwrap();
    assert_eq!(loaded["background"], "222 47% 11%");
    assert_eq!(loaded["foreground"], "#fff");
    assert!(loaded.get("unknown").is_none());

    let leftovers: Vec<_> = fs::read_dir(home.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .filter(|name| name.to_string_lossy().contains("theme.json."))
        .collect();
    assert!(
        leftovers.is_empty(),
        "temporary files left behind: {leftovers:?}"
    );
}

#[test]
fn empty_storage_is_default_but_malformed_and_oversized_storage_errors() {
    let _lock = HOME_LOCK.lock().unwrap();
    let home = tempfile::tempdir().unwrap();
    let _home = HomeOverride::set(home.path().to_path_buf());
    let path = home.path().join(THEME_FILE_NAME);

    fs::create_dir_all(home.path()).unwrap();
    fs::write(&path, b" \n\t ").unwrap();
    assert_eq!(load_theme().unwrap(), json!({}));

    fs::write(&path, b"{not-json}").unwrap();
    assert!(load_theme().is_err());

    fs::write(&path, " ".repeat(MAX_THEME_BYTES + 1)).unwrap();
    assert!(load_theme().is_err());
}

#[cfg(unix)]
#[test]
fn load_rejects_a_symlinked_theme_path() {
    use std::os::unix::fs::symlink;

    let _lock = HOME_LOCK.lock().unwrap();
    let home = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let _home = HomeOverride::set(home.path().to_path_buf());
    fs::write(
        outside.path().join("outside.json"),
        b"{\"background\":\"#fff\"}",
    )
    .unwrap();
    symlink(
        outside.path().join("outside.json"),
        home.path().join(THEME_FILE_NAME),
    )
    .unwrap();

    assert!(load_theme().is_err());
}
