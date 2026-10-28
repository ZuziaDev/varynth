//! Varynth account login. Tokens live in `{home}/auth.env`, never in chat logs.

use std::fs;
use std::io;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::time::timeout;
use uuid::Uuid;

use crate::config::Config;
use crate::control_bus;

const CALLBACK_PATH: &str = "/oauth/callback";
const SKEW_SECS: u64 = 60;
const AUTH_TIMEOUT: Duration = Duration::from_secs(180);
const READ_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Debug)]
pub struct Tokens {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at: u64,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: Option<String>,
    refresh_token: Option<String>,
    expires_in: Option<u64>,
    error: Option<String>,
}

pub fn auth_path() -> PathBuf {
    Config::home_dir().join("auth.env")
}

pub fn load_auth_env() -> Result<Option<Tokens>> {
    let tokens = read_file().or_else(|_| read_keyring());
    let Ok(Some(tokens)) = tokens else {
        return Ok(None);
    };
    export_tokens(&tokens);
    Ok(Some(tokens))
}

pub fn signed_in() -> bool {
    load_auth_env().ok().flatten().is_some()
}

pub fn status_detail() -> &'static str {
    if read_file().ok().flatten().is_some() {
        "signed-in"
    } else if read_keyring().ok().flatten().is_some() {
        "signed-in (keyring)"
    } else {
        "signed-out"
    }
}

pub async fn login(api_url: &str, web_url: &str, no_browser: bool) -> Result<()> {
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .await
        .map_err(|_| anyhow!("!! login\n   could not bind the local callback"))?;
    let port = listener
        .local_addr()
        .map_err(|_| anyhow!("!! login\n   could not read the callback port"))?
        .port();
    let redirect_uri = format!("http://127.0.0.1:{port}{CALLBACK_PATH}");
    let verifier = pkce_verifier();
    let challenge = pkce_challenge(&verifier);
    let state = pkce_verifier();
    let authorize = format!(
        "response_type=code&client_id=varynth-cli&redirect_uri={}&state={}&code_challenge={}&code_challenge_method=S256",
        encode(&redirect_uri),
        encode(&state),
        encode(&challenge)
    );
    let mut open = reqwest::Url::parse(&format!("{}/login", web_url.trim_end_matches('/')))
        .context("!! login\n   VARYNTH_WEB_URL is invalid")?;
    open.query_pairs_mut()
        .append_pair("cli", "1")
        .append_pair("redirect", &authorize);
    let open = open.to_string();
    println!("{open}");
    eprintln!(".. waiting  browser sign-in");
    if !no_browser {
        let _ = open_browser(&open);
    }
    let code = match timeout(AUTH_TIMEOUT, accept_code(listener, &state)).await {
        Ok(Ok(code)) => code,
        Ok(Err(error)) => return Err(error),
        Err(_) => bail!("!! login timed out\n   try: varynth login"),
    };
    let tokens = exchange(
        api_url,
        "authorization_code",
        &[
            ("code", code.as_str()),
            ("redirect_uri", redirect_uri.as_str()),
            ("client_id", "varynth-cli"),
            ("code_verifier", verifier.as_str()),
        ],
    )
    .await?;
    store(tokens)?;
    eprintln!("ok login   signed-in");
    Ok(())
}

pub async fn refresh() -> Result<Tokens> {
    let current = load_auth_env()?.context("!! refresh failed\n   try: varynth login")?;
    let api = std::env::var("VARYNTH_API_URL").unwrap_or_else(|_| "http://127.0.0.1:8080".into());
    let tokens = exchange(
        &api,
        "refresh_token",
        &[("refresh_token", current.refresh_token.as_str())],
    )
    .await?;
    store(tokens.clone())?;
    Ok(tokens)
}

pub async fn logout(api_url: &str) -> Result<()> {
    if let Ok(Some(tokens)) = read_file().or_else(|_| read_keyring()) {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .build()?;
        let url = format!("{}/v1/oauth/revoke", api_url.trim_end_matches('/'));
        let _ = client
            .post(url)
            .json(&serde_json::json!({ "refresh_token": tokens.refresh_token }))
            .send()
            .await;
    }
    if auth_path().exists() {
        fs::remove_file(auth_path()).context("!! logout\n   could not remove auth.env")?;
    }
    let _ = delete_keyring();
    std::env::remove_var("VARYNTH_ACCESS_TOKEN");
    std::env::remove_var("VARYNTH_REFRESH_TOKEN");
    std::env::remove_var("VARYNTH_TOKEN_EXPIRES_AT");
    eprintln!("ok logout  signed-out");
    Ok(())
}

pub async fn access_token_fresh(api_url: &str) -> Result<Option<String>> {
    let Some(tokens) = load_auth_env()? else {
        return Ok(None);
    };
    if token_fresh(tokens.expires_at) {
        return Ok(Some(tokens.access_token));
    }
    std::env::set_var("VARYNTH_API_URL", api_url);
    match refresh().await {
        Ok(tokens) => Ok(Some(tokens.access_token)),
        Err(_) => Ok(None),
    }
}

fn store(tokens: Tokens) -> Result<()> {
    export_tokens(&tokens);
    let body = format!(
        "VARYNTH_ACCESS_TOKEN={}\nVARYNTH_REFRESH_TOKEN={}\nVARYNTH_TOKEN_EXPIRES_AT={}\n",
        tokens.access_token, tokens.refresh_token, tokens.expires_at
    );
    if control_bus::atomic_write(&auth_path(), body.as_bytes()).is_ok() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(auth_path(), fs::Permissions::from_mode(0o600));
        }
        return Ok(());
    }
    write_keyring(&tokens).context("!! login\n   could not write auth.env or the keyring")
}

fn export_tokens(tokens: &Tokens) {
    std::env::set_var("VARYNTH_ACCESS_TOKEN", &tokens.access_token);
    std::env::set_var("VARYNTH_REFRESH_TOKEN", &tokens.refresh_token);
    std::env::set_var("VARYNTH_TOKEN_EXPIRES_AT", tokens.expires_at.to_string());
}

fn read_file() -> Result<Option<Tokens>> {
    let text = match fs::read_to_string(auth_path()) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let mut access = None;
    let mut refresh = None;
    let mut expires = None;
    for line in text.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        match key.trim() {
            "VARYNTH_ACCESS_TOKEN" => access = Some(value.trim().to_string()),
            "VARYNTH_REFRESH_TOKEN" => refresh = Some(value.trim().to_string()),
            "VARYNTH_TOKEN_EXPIRES_AT" => expires = value.trim().parse().ok(),
            _ => {}
        }
    }
    match (access, refresh, expires) {
        (Some(access_token), Some(refresh_token), Some(expires_at))
            if !access_token.is_empty() && !refresh_token.is_empty() =>
        {
            Ok(Some(Tokens {
                access_token,
                refresh_token,
                expires_at,
            }))
        }
        _ => Ok(None),
    }
}

fn token_fresh(expires_at: u64) -> bool {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    expires_at > now.saturating_add(SKEW_SECS)
}

async fn exchange(api_url: &str, grant_type: &str, fields: &[(&str, &str)]) -> Result<Tokens> {
    let mut body = serde_json::Map::new();
    body.insert(
        "grant_type".into(),
        serde_json::Value::String(grant_type.into()),
    );
    for (key, value) in fields {
        body.insert((*key).into(), serde_json::Value::String((*value).into()));
    }
    let url = format!("{}/v1/oauth/token", api_url.trim_end_matches('/'));
    let response = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()?
        .post(url)
        .json(&body)
        .send()
        .await
        .map_err(|_| anyhow!("!! network error\n   try: varynth doctor"))?;
    let parsed: TokenResponse = response
        .json()
        .await
        .map_err(|_| anyhow!("!! login\n   token response was not valid"))?;
    if parsed.error.is_some() || parsed.access_token.is_none() {
        bail!("!! login denied\n   try: varynth login");
    }
    let expires_in = parsed.expires_in.unwrap_or(3600);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    Ok(Tokens {
        access_token: parsed.access_token.unwrap_or_default(),
        refresh_token: parsed
            .refresh_token
            .filter(|value| !value.is_empty())
            .context("!! login\n   refresh token missing")?,
        expires_at: now.saturating_add(expires_in),
    })
}

async fn accept_code(listener: TcpListener, expected_state: &str) -> Result<String> {
    let (mut socket, _) = listener
        .accept()
        .await
        .map_err(|_| anyhow!("!! login\n   callback failed"))?;
    let mut buf = vec![0u8; 8192];
    let n = timeout(READ_TIMEOUT, socket.read(&mut buf))
        .await
        .map_err(|_| anyhow!("!! login timed out\n   try: varynth login"))?
        .map_err(|_| anyhow!("!! login\n   callback failed"))?;
    let text = String::from_utf8_lossy(&buf[..n]);
    let target = text
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or("");
    let url = reqwest::Url::parse(&format!("http://127.0.0.1{target}"))
        .map_err(|_| anyhow!("!! login denied\n   callback was invalid"))?;
    if url.path() != CALLBACK_PATH {
        bail!("!! login denied\n   callback was invalid");
    }
    let mut code = None;
    let mut state = None;
    let mut error = None;
    for (key, value) in url.query_pairs() {
        match key.as_ref() {
            "code" => code = Some(value.into_owned()),
            "state" => state = Some(value.into_owned()),
            "error" => error = Some(value.into_owned()),
            _ => {}
        }
    }
    let page = if error.is_some() || state.as_deref() != Some(expected_state) {
        "denied\n"
    } else {
        "signed in\n"
    };
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{page}",
        page.len()
    );
    let _ = socket.write_all(response.as_bytes()).await;
    if error.is_some() || state.as_deref() != Some(expected_state) {
        bail!("!! login denied\n   try: varynth login");
    }
    code.context("!! login denied\n   try: varynth login")
}

fn pkce_verifier() -> String {
    format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple())
}

fn pkce_challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

fn encode(value: &str) -> String {
    let mut out = String::new();
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

fn open_browser(url: &str) -> Result<()> {
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::ffi::OsStrExt;
        let wide_url: Vec<u16> = std::ffi::OsStr::new(url).encode_wide().chain(Some(0)).collect();
        let wide_op: Vec<u16> = std::ffi::OsStr::new("open").encode_wide().chain(Some(0)).collect();
        let ret = unsafe {
            windows_sys::Win32::UI::Shell::ShellExecuteW(
                std::ptr::null_mut(),
                wide_op.as_ptr(),
                wide_url.as_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL,
            )
        };
        if (ret as usize) <= 32 {
            let status = std::process::Command::new("cmd")
                .args(["/c", "start", "", url])
                .spawn();
            if status.is_err() {
                std::process::Command::new("powershell")
                    .args(["-NoProfile", "-NonInteractive", "-Command", &format!("Start-Process '{}'", url.replace('\'', "''"))])
                    .spawn()
                    .context("!! login\n   could not open a browser")?;
            }
        }
    }
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open")
            .arg(url)
            .spawn()
            .context("!! login\n   could not open a browser")?;
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        std::process::Command::new("xdg-open")
            .arg(url)
            .spawn()
            .context("!! login\n   could not open a browser")?;
    }
    Ok(())
}

fn keyring_entry() -> Result<keyring::Entry> {
    keyring::Entry::new("varynth", "varynth/varynth-oauth")
        .map_err(|_| anyhow!("!! login\n   keyring is unavailable"))
}

fn write_keyring(tokens: &Tokens) -> Result<()> {
    let json = serde_json::json!({
        "access_token": tokens.access_token,
        "refresh_token": tokens.refresh_token,
        "expires_at": tokens.expires_at,
    });
    keyring_entry()?
        .set_password(&json.to_string())
        .map_err(|_| anyhow!("!! login\n   keyring write failed"))
}

fn read_keyring() -> Result<Option<Tokens>> {
    let entry = keyring_entry()?;
    match entry.get_password() {
        Ok(raw) => {
            let value: serde_json::Value = serde_json::from_str(&raw).unwrap_or_default();
            let access = value
                .get("access_token")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let refresh = value
                .get("refresh_token")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let expires = value
                .get("expires_at")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            if access.is_empty() || refresh.is_empty() {
                return Ok(None);
            }
            Ok(Some(Tokens {
                access_token: access.into(),
                refresh_token: refresh.into(),
                expires_at: expires,
            }))
        }
        Err(_) => Ok(None),
    }
}

fn delete_keyring() -> Result<()> {
    match keyring_entry()?.delete_credential() {
        Ok(()) => Ok(()),
        Err(_) => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_challenge_changes_with_verifier() {
        let one = pkce_challenge("verifier-value-verifier-value-verifier");
        let two = pkce_challenge("other-verifier-value-other-verifier-value");
        assert_ne!(one, two);
        assert!(!one.contains("verifier-value"));
    }
}
