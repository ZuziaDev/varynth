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

    let proxy_healthy = proxy_ok(&cfg.proxy_url).await;
    checks.push(json!({
        "name": "proxy_health",
        "ok": proxy_healthy,
        "detail": cfg.proxy_url,
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
    fn docker_probe_returns_bounded_structured_result() {
        let (binary, binary_detail, daemon, daemon_detail) = docker_checks();
        assert!(!binary_detail.is_empty());
        assert!(!daemon_detail.is_empty());
        if !binary {
            assert!(!daemon);
        }
    }
}
