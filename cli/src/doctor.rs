use anyhow::Result;
use serde_json::json;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::config::Config;
use crate::providers;

async fn proxy_ok(url: &str) -> bool {
    reqwest::Client::new()
        .get(format!("{}/health", url.trim_end_matches('/')))
        .timeout(Duration::from_secs(3))
        .send()
        .await
        .map(|r| r.status().is_success())
        .unwrap_or(false)
}

fn proxy_is_loopback(url: &str) -> bool {
    match reqwest::Url::parse(url) {
        Ok(parsed) => match parsed.host_str() {
            Some(host) => {
                let host = host.trim_matches(|c| c == '[' || c == ']');
                matches!(host, "127.0.0.1" | "localhost" | "::1")
            }
            None => false,
        },
        Err(_) => false,
    }
}

pub async fn try_start_proxy(cfg: &Config) -> Result<bool> {
    if proxy_ok(&cfg.proxy_url).await {
        return Ok(true);
    }
    let guidance =
        "start your local proxy yourself; doctor --fix only starts varynth-proxy from PATH";
    if !proxy_is_loopback(&cfg.proxy_url) {
        anyhow::bail!("proxy at {} is not reachable; {guidance}", cfg.proxy_url);
    }
    let exe = which::which("varynth-proxy")
        .map_err(|_| anyhow::anyhow!("varynth-proxy not found on PATH; {guidance}"))?;
    Command::new(exe)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    for _ in 0..30 {
        tokio::time::sleep(Duration::from_millis(250)).await;
        if proxy_ok(&cfg.proxy_url).await {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Probe Docker without leaving a child behind when the daemon or named pipe
/// is unavailable. The binary check remains separate from daemon health.
fn docker_checks() -> (bool, String, bool, String) {
    let binary = which::which("docker").is_ok();
    if !binary {
        return (
            false,
            "missing docker executable".into(),
            false,
            "not checked because docker is missing".into(),
        );
    }
    let mut child = match Command::new("docker")
        .arg("info")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(error) => {
            return (
                true,
                "docker executable present".into(),
                false,
                format!("failed to start docker info: {error}"),
            )
        }
    };
    let deadline = Instant::now() + Duration::from_secs(3);
    let daemon = loop {
        match child.try_wait() {
            Ok(Some(status)) => break (status.success(), format!("docker info exited {status}")),
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                break (false, "docker info timed out after 3s".into());
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                break (false, format!("docker info wait failed: {error}"));
            }
        }
    };
    (true, "docker executable present".into(), daemon.0, daemon.1)
}

fn computer_use_check() -> (bool, String) {
    let binary = crate::computer_use::ocu_binary();
    let has_entry = crate::mcp::load_mcp_config(&Config::home_dir().join("mcp.json"))
        .map(|servers| servers.contains_key("open-computer-use"))
        .unwrap_or(false);
    let adapter = binary
        .as_deref()
        .map_or("adapter missing".to_string(), |path| {
            format!("adapter {path}")
        });
    let entry = if has_entry {
        "mcp entry present"
    } else {
        "mcp entry missing"
    };
    (
        binary.is_some() && has_entry,
        format!("{adapter}; {entry}; probe with `varynth mcp probe open-computer-use`"),
    )
}

fn keyring_check() -> (bool, String) {
    let mut missing = 0usize;
    for field in crate::config::KEYRING_FIELDS {
        match crate::config::keyring_get(field) {
            Ok(Some(_)) => {}
            Ok(None) => missing += 1,
            Err(error) => {
                return (
                    false,
                    format!("backend unavailable while reading {field}: {error}"),
                )
            }
        }
    }
    (
        true,
        format!("backend available; {missing} configured fields are empty"),
    )
}

/// The OS sandbox backend actually in effect for the configured mode. Derived
/// from the same `enforcement()` result `doctor` reports in JSON, so this line
/// can never claim a boundary the JSON calls degraded or unconfined.
pub fn sandbox_backend(sandbox: &str) -> String {
    let mode = crate::sandbox::SandboxMode::parse(sandbox);
    sandbox_backend_label(mode, crate::sandbox::enforcement(mode), cfg!(windows))
}

fn sandbox_backend_label(
    mode: crate::sandbox::SandboxMode,
    enforcement: crate::sandbox::Enforcement,
    windows: bool,
) -> String {
    match enforcement {
        crate::sandbox::Enforcement::Unconfined => "not-enforced".to_string(),
        crate::sandbox::Enforcement::Degraded(reason) => format!("degraded: {reason}"),
        crate::sandbox::Enforcement::Enforced => {
            if mode == crate::sandbox::SandboxMode::DockerIsolated {
                "docker".to_string()
            } else if windows {
                "windows".to_string()
            } else {
                "degraded".to_string()
            }
        }
    }
}

pub async fn run(cfg: &Config) -> Result<serde_json::Value> {
    Config::ensure_home()?;
    let mut checks = Vec::new();

    checks.push(json!({
        "name": "home",
        "ok": Config::home_dir().exists(),
        "detail": Config::home_dir().display().to_string(),
    }));
    checks.push(json!({
        "name": "config",
        "ok": Config::config_path().exists(),
        "detail": Config::config_path().display().to_string(),
    }));
    checks.push(json!({
        "name": "provider",
        "ok": true,
        "detail": cfg.provider,
    }));
    checks.push(json!({
        "name": "model",
        "ok": true,
        "detail": cfg.model,
    }));

    let (docker_binary, docker_binary_detail, docker_daemon, docker_daemon_detail) =
        tokio::task::spawn_blocking(docker_checks).await.unwrap_or((
            false,
            "docker probe task failed".into(),
            false,
            "docker probe task failed".into(),
        ));
    checks.push(json!({
        "name": "docker",
        "ok": docker_binary,
        "detail": docker_binary_detail,
    }));
    checks.push(json!({
        "name": "docker_daemon",
        "ok": docker_daemon,
        "detail": docker_daemon_detail,
    }));

    let (keyring_available, keyring_detail) = tokio::task::spawn_blocking(keyring_check)
        .await
        .unwrap_or((false, "keyring probe task failed".into()));
    checks.push(json!({
        "name": "keyring_backend",
        "ok": keyring_available,
        "detail": keyring_detail,
    }));

    let (computer_use_ok, computer_use_detail) = tokio::task::spawn_blocking(computer_use_check)
        .await
        .unwrap_or((false, "computer_use probe task failed".into()));
    checks.push(json!({
        "name": "computer_use",
        "ok": computer_use_ok,
        "detail": computer_use_detail,
    }));

    // Notification capability is reported even when the feature is off, so a
    // user can see which OS mechanism would be used (and whether a fallback
    // is in effect) before enabling it.
    let notify_cfg = cfg.clone();
    let capability =
        tokio::task::spawn_blocking(move || crate::notify::doctor_capability(&notify_cfg))
            .await
            .unwrap_or_else(|_| crate::notify::capability(cfg));
    checks.push(json!({
        "name": "notifications",
        "ok": capability["ok"],
        "mode": capability["mode"],
        "enabled": capability["enabled"],
        "backend": capability["backend"],
        "mechanism": capability["mechanism"],
        "winrt_probe": capability["winrt_probe"],
        "delay_secs": capability["delay_secs"],
        "env_override": capability["env_override"],
        "detail": capability["detail"],
    }));

    let proxy_healthy = proxy_ok(&cfg.proxy_url).await;
    checks.push(json!({
        "name": "proxy_health",
        "ok": proxy_healthy,
        "detail": cfg.proxy_url,
    }));
    checks.push(json!({
        "name": "varynth_auth",
        "ok": true,
        "detail": crate::varynth_auth::status_detail(),
    }));

    let decisions = crate::decisions::status(cfg).await;
    checks.push(json!({
        "name": "decisions",
        // Reachable now, or startable on demand by `varynth decide`.
        "ok": decisions.reachable || decisions.can_autostart(),
        "detail": decisions.detail(),
        "url": decisions.url,
        "reachable": decisions.reachable,
        "autostart": decisions.autostart,
        "service_path": decisions.server_path,
        "service_found": decisions.service_found,
        "python_found": decisions.python_found,
        "managed_pid": decisions.managed_pid,
        "log": decisions.log,
    }));

    let mut models = 0usize;
    if let Ok(p) = providers::build(cfg) {
        if let Ok(list) = p.list_models().await {
            models = list.len();
        }
    }
    checks.push(json!({
        "name": "model_catalog",
        "ok": models > 0,
        "detail": format!("{models} models"),
    }));

    let rustc = which::which("rustc").is_ok();
    checks.push(json!({
        "name": "rustc",
        "ok": rustc,
        "detail": if rustc { "present" } else { "missing" },
    }));

    // One row per sandbox mode so a mode that cannot establish its OS
    // boundary is visible instead of silently running the path jail alone.
    let configured = crate::sandbox::SandboxMode::parse(&cfg.sandbox);
    for mode in [
        crate::sandbox::SandboxMode::ReadOnly,
        crate::sandbox::SandboxMode::WorkspaceWrite,
        crate::sandbox::SandboxMode::DangerFullAccess,
        crate::sandbox::SandboxMode::DockerIsolated,
    ] {
        let enforcement = crate::sandbox::enforcement(mode);
        checks.push(json!({
            "name": format!("sandbox {}", mode.as_str()),
            "ok": !matches!(enforcement, crate::sandbox::Enforcement::Degraded(_)),
            "sandbox": mode.as_str(),
            "enforcement": enforcement.status(),
            "configured": mode == configured,
        }));
    }

    Ok(json!({
        "ok": checks.iter().all(|c| c["ok"].as_bool() == Some(true) || c["name"] == "rustc"),
        "checks": checks,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proxy_loopback_detection_is_strict() {
        assert!(proxy_is_loopback("http://127.0.0.1:8787"));
        assert!(proxy_is_loopback("http://[::1]:8787"));
        assert!(!proxy_is_loopback("https://example.com"));
    }

    #[test]
    fn sandbox_backend_is_honest_per_platform_and_mode() {
        use crate::sandbox::{Enforcement, SandboxMode};
        let enforced = Enforcement::Enforced;
        assert_eq!(
            sandbox_backend_label(SandboxMode::WorkspaceWrite, enforced.clone(), true),
            "windows"
        );
        assert_eq!(
            sandbox_backend_label(SandboxMode::WorkspaceWrite, enforced.clone(), false),
            "degraded"
        );
        assert_eq!(
            sandbox_backend_label(SandboxMode::DockerIsolated, enforced, false),
            "docker"
        );
        assert_eq!(
            sandbox_backend_label(SandboxMode::DangerFullAccess, Enforcement::Unconfined, true),
            "not-enforced"
        );
        assert_eq!(
            sandbox_backend_label(
                SandboxMode::WorkspaceWrite,
                Enforcement::Degraded("probe failed".into()),
                true
            ),
            "degraded: probe failed"
        );
    }

    #[test]
    fn docker_probe_returns_bounded_structured_result() {
        let (binary, binary_detail, daemon, daemon_detail) = docker_checks();
        assert!(!binary_detail.is_empty());
        assert!(!daemon_detail.is_empty());
        if !binary {
            assert!(!daemon);
        }
    }
}
