//! Release check and `varynth update`.
//!
//! The startup check is cheap: the latest published version is cached in
//! `~/.varynth/update-check.json` and refreshed at most every 12 hours with a
//! short timeout, so an offline machine never waits on the network. Set
//! `VARYNTH_NO_UPDATE_CHECK=1` to disable it entirely.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::time::Duration;

use crate::config::Config;

pub const CURRENT: &str = env!("CARGO_PKG_VERSION");
const CRATE_NAME: &str = "varynth";
const NPM_PACKAGE: &str = "@zuziadev/varynth";
const CHECK_INTERVAL_SECS: i64 = 12 * 60 * 60;

static AVAILABLE: OnceLock<String> = OnceLock::new();

#[derive(Debug, Default, Serialize, Deserialize)]
struct Cache {
    checked_at: i64,
    latest: Option<String>,
}

fn cache_path() -> PathBuf {
    Config::home_dir().join("update-check.json")
}

fn load_cache() -> Option<Cache> {
    let raw = std::fs::read_to_string(cache_path()).ok()?;
    serde_json::from_str(&raw).ok()
}

fn save_cache(latest: Option<&str>) {
    let cache = Cache {
        checked_at: chrono::Utc::now().timestamp(),
        latest: latest.map(str::to_string),
    };
    if let Ok(raw) = serde_json::to_string(&cache) {
        let _ = std::fs::write(cache_path(), raw);
    }
}

/// Newer version found by the startup check, if any.
pub fn available() -> Option<&'static str> {
    AVAILABLE.get().map(String::as_str)
}

/// One-line notice shown at startup when a newer release exists.
pub fn notice() -> Option<String> {
    available().map(|latest| {
        format!("▲ Yeni bir sürüm var: v{latest} (mevcut: v{CURRENT})! `varynth update` yazarak güncelleyebilirsiniz.")
    })
}

/// Startup check. Uses the cached result when it is fresh, otherwise asks the
/// registry with a short timeout. Never fails; errors just mean "no notice".
pub async fn check_on_startup() -> Option<&'static str> {
    if std::env::var("VARYNTH_NO_UPDATE_CHECK").is_ok_and(|v| !v.is_empty() && v != "0") {
        return None;
    }
    let now = chrono::Utc::now().timestamp();
    let latest = match load_cache() {
        Some(cache) if now - cache.checked_at < CHECK_INTERVAL_SECS => cache.latest,
        _ => {
            let fetched = fetch_latest(Duration::from_millis(1500)).await.ok();
            save_cache(fetched.as_deref());
            fetched
        }
    }?;
    if is_newer(&latest, CURRENT) {
        let _ = AVAILABLE.set(latest);
    }
    available()
}

/// Latest published version: crates.io first, npm as a fallback.
pub async fn fetch_latest(timeout: Duration) -> Result<String> {
    let client = reqwest::Client::builder()
        .timeout(timeout)
        .user_agent(concat!("varynth/", env!("CARGO_PKG_VERSION"), " (update-check)"))
        .build()?;
    let crates = async {
        let body: serde_json::Value = client
            .get(format!("https://crates.io/api/v1/crates/{CRATE_NAME}"))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let krate = body.get("crate").context("crates.io: missing crate")?;
        krate
            .get("max_stable_version")
            .and_then(|v| v.as_str())
            .or_else(|| krate.get("max_version").and_then(|v| v.as_str()))
            .map(str::to_string)
            .context("crates.io: missing version")
    };
    match crates.await {
        Ok(v) => Ok(v),
        Err(_) => {
            let body: serde_json::Value = client
                .get(format!("https://registry.npmjs.org/{NPM_PACKAGE}/latest"))
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;
            body.get("version")
                .and_then(|v| v.as_str())
                .map(str::to_string)
                .context("npm: missing version")
        }
    }
}

fn parse(v: &str) -> Option<((u64, u64, u64), bool)> {
    let v = v.trim().trim_start_matches('v');
    let (core, pre) = match v.split_once(['-', '+']) {
        Some((core, _)) => (core, true),
        None => (v, false),
    };
    let mut it = core.split('.').map(|p| p.parse::<u64>().ok());
    let triple = (it.next()??, it.next().flatten().unwrap_or(0), it.next().flatten().unwrap_or(0));
    Some((triple, pre))
}

/// `true` when `latest` is a strictly newer release than `current`.
pub fn is_newer(latest: &str, current: &str) -> bool {
    match (parse(latest), parse(current)) {
        (Some((l, l_pre)), Some((c, c_pre))) => l > c || (l == c && c_pre && !l_pre),
        _ => false,
    }
}

/// Remove `varynth.exe.old` left behind by a previous self-update.
pub fn cleanup_old_binaries() {
    if let Ok(exe) = std::env::current_exe() {
        let _ = std::fs::remove_file(old_path(&exe));
    }
    let _ = std::fs::remove_file(old_path(&local_bin()));
}

fn old_path(exe: &Path) -> PathBuf {
    let mut name = exe.file_name().unwrap_or_default().to_os_string();
    name.push(".old");
    exe.with_file_name(name)
}

fn exe_name() -> &'static str {
    if cfg!(windows) {
        "varynth.exe"
    } else {
        "varynth"
    }
}

fn local_bin() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".local")
        .join("bin")
        .join(exe_name())
}

fn cargo_bin() -> PathBuf {
    std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|h| h.join(".cargo")))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("bin")
        .join(exe_name())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Cargo,
    Npm,
}

impl Method {
    pub fn parse(s: &str) -> Result<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "cargo" => Ok(Self::Cargo),
            "npm" => Ok(Self::Npm),
            other => bail!("unknown update method `{other}` (use cargo or npm)"),
        }
    }
}

fn detect_method(exe: &Path) -> Option<Method> {
    let path = exe.to_string_lossy().to_ascii_lowercase();
    if path.contains("node_modules") && which::which("npm").is_ok() {
        return Some(Method::Npm);
    }
    if which::which("cargo").is_ok() {
        return Some(Method::Cargo);
    }
    if which::which("npm").is_ok() {
        return Some(Method::Npm);
    }
    None
}

fn npm_command() -> Command {
    if cfg!(windows) {
        let mut c = Command::new("cmd");
        c.args(["/C", "npm"]);
        c
    } else {
        Command::new("npm")
    }
}

fn npm_installed_binary() -> Option<PathBuf> {
    let out = npm_command().args(["root", "-g"]).output().ok()?;
    let root = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let candidate = Path::new(&root)
        .join("@zuziadev")
        .join("varynth")
        .join("bin")
        .join(exe_name());
    candidate.exists().then_some(candidate)
}

fn same_file(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

/// A running binary cannot be overwritten on Windows, but it can be renamed.
/// Move every copy out of the way before the installer writes new files.
fn park(paths: &[PathBuf]) -> Vec<(PathBuf, PathBuf)> {
    let mut parked = Vec::new();
    for path in paths {
        if !path.exists() {
            continue;
        }
        let old = old_path(path);
        let _ = std::fs::remove_file(&old);
        if std::fs::rename(path, &old).is_ok() {
            parked.push((path.clone(), old));
        }
    }
    parked
}

fn unpark(parked: &[(PathBuf, PathBuf)]) {
    for (path, old) in parked {
        if !path.exists() {
            let _ = std::fs::rename(old, path);
        }
    }
}

/// `varynth update [--check] [--via cargo|npm]`
pub async fn run(check_only: bool, via: Option<String>) -> Result<()> {
    println!("varynth v{CURRENT}  ·  güncellemeler denetleniyor…");
    let latest = fetch_latest(Duration::from_secs(10))
        .await
        .context("crates.io veya npm kayıt defterine erişilemedi")?;
    save_cache(Some(&latest));
    if !is_newer(&latest, CURRENT) {
        println!("✓ En son sürümü kullanıyorsunuz (v{CURRENT})");
        return Ok(());
    }
    println!("↑ Yeni bir sürüm mevcut: v{CURRENT} → v{latest}");
    if check_only {
        println!("  Güncellemek için `varynth update` komutunu çalıştırabilirsiniz.");
        return Ok(());
    }

    let exe = std::env::current_exe().context("current_exe")?;
    let method = match via {
        Some(v) => Method::parse(&v)?,
        None => detect_method(&exe).context(
            "neither cargo nor npm was found on PATH\n   install one and re-run, or download a release from https://varynth.zuzia.dev",
        )?,
    };

    // Every copy the user might launch: the running binary, ~/.local/bin and
    // the installer's own target.
    let installer_target = match method {
        Method::Cargo => Some(cargo_bin()),
        Method::Npm => npm_installed_binary(),
    };
    let mut targets = vec![exe.clone()];
    let local = local_bin();
    if local.exists() && !targets.iter().any(|t| same_file(t, &local)) {
        targets.push(local);
    }
    if let Some(t) = &installer_target {
        if t.exists() && !targets.iter().any(|x| same_file(x, t)) {
            targets.push(t.clone());
        }
    }
    let parked = park(&targets);

    let status = match method {
        Method::Cargo => {
            println!("==> cargo install {CRATE_NAME} --locked --force  (this compiles from source, a few minutes)");
            Command::new("cargo")
                .args(["install", CRATE_NAME, "--locked", "--force"])
                .status()
        }
        Method::Npm => {
            println!("==> npm install -g {NPM_PACKAGE}@latest");
            npm_command()
                .args(["install", "-g", &format!("{NPM_PACKAGE}@latest")])
                .status()
        }
    };
    let ok = matches!(&status, Ok(s) if s.success());
    if !ok {
        unpark(&parked);
        match status {
            Ok(s) => bail!("update failed ({s}); the previous binary was restored"),
            Err(e) => bail!("update failed: {e}; the previous binary was restored"),
        }
    }

    // Refresh copies the installer did not write itself.
    let fresh = match method {
        Method::Cargo => Some(cargo_bin()),
        Method::Npm => npm_installed_binary(),
    }
    .filter(|p| p.exists());
    if let Some(fresh) = &fresh {
        for (path, _) in &parked {
            if !path.exists() {
                if let Err(e) = std::fs::copy(fresh, path) {
                    eprintln!("!! could not refresh {}: {e}", path.display());
                }
            }
        }
    }
    unpark(&parked);
    save_cache(Some(&latest));
    println!("✓ Varynth başarıyla v{latest} sürümüne güncellendi. Çalışan oturumları yeniden başlatın.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compares_versions() {
        assert!(is_newer("0.1.3", "0.1.2"));
        assert!(is_newer("0.2.0", "0.1.9"));
        assert!(is_newer("1.0.0", "0.9.9"));
        assert!(is_newer("0.1.2", "0.1.2-alpha"));
        assert!(!is_newer("0.1.2", "0.1.2"));
        assert!(!is_newer("0.1.1", "0.1.2"));
        assert!(!is_newer("0.1.2-alpha", "0.1.2"));
        assert!(!is_newer("garbage", "0.1.2"));
    }

    #[test]
    fn old_path_appends_suffix() {
        let p = old_path(Path::new("/x/varynth.exe"));
        assert_eq!(p, PathBuf::from("/x/varynth.exe.old"));
    }
}
