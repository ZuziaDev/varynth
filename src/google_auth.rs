//! Official Google desktop OAuth 2.0 authorization-code flow with PKCE.
//!
//! Credentials are serialized only into the encrypted OS keyring entry
//! `varynth/google-oauth`; this module never reads an auth file or config file.
//! Access-token callers must be wired explicitly by the application. There is
//! no automatic login path.

use anyhow::{anyhow, bail, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use reqwest::Url;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io::{self, Write};
use std::process::Command;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::Mutex as AsyncMutex;
use tokio::time::{timeout, Instant};
use tokio_stream::StreamExt;
use uuid::Uuid;

const KEYRING_SERVICE: &str = "varynth";
const KEYRING_ENTRY: &str = "varynth/google-oauth";

const AUTHORIZATION_ENDPOINT: &str = "https://accounts.google.com/o/oauth2/v2/auth";
const TOKEN_ENDPOINT: &str = "https://oauth2.googleapis.com/token";
const CALLBACK_PATH: &str = "/oauth/callback";
// Keep this to the documented broad Google Cloud scope until a dedicated
// generative-language OAuth scope is established by official documentation.
const OAUTH_SCOPE: &str = "https://www.googleapis.com/auth/cloud-platform";

const AUTHORIZATION_TIMEOUT: Duration = Duration::from_secs(180);
const TOKEN_TIMEOUT: Duration = Duration::from_secs(30);
const CALLBACK_READ_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_CALLBACK_REQUEST_BYTES: usize = 16 * 1024;
const MAX_CALLBACK_HEADER_BYTES: usize = 8 * 1024;
const MAX_CALLBACK_HEADERS: usize = 64;
const MAX_CALLBACK_TARGET_BYTES: usize = 8 * 1024;
const MAX_TOKEN_RESPONSE_BYTES: usize = 64 * 1024;
const MAX_TOKEN_BYTES: usize = 64 * 1024;
const ACCESS_TOKEN_REFRESH_SKEW_SECS: i64 = 60;
const MAX_TOKEN_LIFETIME_SECS: u64 = 365 * 24 * 60 * 60;

static AUTH_OPERATION_LOCK: OnceLock<AsyncMutex<()>> = OnceLock::new();
static KEYRING_OPERATION_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredCredential {
    client_id: String,
    client_secret: Option<String>,
    refresh_token: String,
    access_token: String,
    expires_at: i64,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: Option<String>,
    expires_in: Option<u64>,
    refresh_token: Option<String>,
    error: Option<String>,
}

struct TokenMaterial {
    access_token: String,
    refresh_token: Option<String>,
    expires_at: i64,
}

/// Run Google's desktop authorization-code flow with S256 PKCE.
///
/// The authorization URL is printed to stdout. Unless `no_browser` is true,
/// the system default browser is started with that URL. The client secret is
/// optional for installed/public desktop clients and is kept only in keyring
/// storage when supplied.
pub async fn login(client_id: &str, client_secret: Option<&str>, no_browser: bool) -> Result<()> {
    let client_id = validate_client_id(client_id)?;
    let client_secret = client_secret.map(validate_client_secret).transpose()?;
    let _operation = auth_operation_lock().lock().await;

    let listener = TcpListener::bind(("127.0.0.1", 0))
        .await
        .map_err(|_| anyhow!("could not start the local Google OAuth callback listener"))?;
    let port = listener
        .local_addr()
        .map_err(|_| anyhow!("could not determine the local Google OAuth callback port"))?
        .port();
    let redirect_uri = format!("http://127.0.0.1:{port}{CALLBACK_PATH}");

    // The verifier and state are independent values. The verifier is never
    // placed in state or otherwise exposed to the authorization endpoint.
    let verifier = generate_pkce_verifier();
    let state = Uuid::new_v4().to_string();
    let challenge = pkce_challenge(&verifier);
    let authorize_url = build_authorize_url(&client_id, &redirect_uri, &state, &challenge)?;

    print_authorize_url(&authorize_url)?;
    if !no_browser {
        let _ = open_default_browser(&authorize_url);
    }

    let code = receive_authorization_code(listener, port, &state).await?;
    let token = exchange_authorization_code(
        &client_id,
        client_secret.as_deref(),
        &redirect_uri,
        &code,
        &verifier,
    )
    .await?;
    let refresh_token = token
        .refresh_token
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            anyhow!(
                "Google did not return a refresh token; run `varynth google login --client-id <CLIENT_ID>` again"
            )
        })?;

    let credential = StoredCredential {
        client_id,
        client_secret,
        refresh_token,
        access_token: token.access_token,
        expires_at: token.expires_at,
    };
    save_credential(&credential)?;
    Ok(())
}

/// Return a usable access token, refreshing it when it is near expiry.
///
/// Refresh operations are serialized process-wide. A credential read occurs
/// again after taking the async lock so concurrent callers reuse a completed
/// refresh instead of issuing duplicate refresh requests.
pub async fn access_token() -> Result<String> {
    let initial = load_credential()?.ok_or_else(not_authenticated_error)?;
    if credential_access_token_is_fresh(&initial)? {
        return Ok(initial.access_token);
    }

    let _operation = auth_operation_lock().lock().await;
    let current = load_credential()?.ok_or_else(not_authenticated_error)?;
    if credential_access_token_is_fresh(&current)? {
        return Ok(current.access_token);
    }

    let token = refresh_access_token(&current).await?;
    let refreshed = StoredCredential {
        client_id: current.client_id,
        client_secret: current.client_secret,
        refresh_token: token.refresh_token.unwrap_or(current.refresh_token),
        access_token: token.access_token,
        expires_at: token.expires_at,
    };

    // Persist the complete updated credential before returning the new access
    // token. A keyring failure therefore fails closed, including token rotation.
    save_credential(&refreshed)?;
    Ok(refreshed.access_token)
}

/// Report whether a refresh-capable credential exists in the OS keyring.
///
/// An expired access token still reports authenticated because the stored
/// refresh token can obtain a new access token on demand.
pub fn is_authenticated() -> Result<bool> {
    Ok(load_credential()?.is_some_and(|credential| {
        !credential.client_id.is_empty() && !credential.refresh_token.is_empty()
    }))
}

/// Async preflight used before a model-using command or runtime change when
/// a Google OAuth provider is selected.
///
/// Returns `Ok(true)` when a stored credential already exists (nothing was
/// started), `Ok(false)` when the browser login flow ran and completed with
/// a stored credential, and an error when the flow failed or was declined.
/// No token material is ever included in the returned error text; failure
/// messages come from the bounded allowlist in this module.
pub async fn ensure_authenticated(client_id: &str, no_browser: bool) -> Result<bool> {
    if is_authenticated()? {
        return Ok(true);
    }
    login(client_id, None, no_browser).await?;
    if is_authenticated()? {
        Ok(false)
    } else {
        bail!("Google sign-in completed but no credential was stored")
    }
}

/// Runs the automatic login decision for `cfg` before a model-using command
/// starts. `Ok(())` means the configuration is ready to run (credential
/// already present, provider not Google OAuth, or sign-in completed now);
/// the error carries the explicit, actionable message otherwise. The flow
/// mirrors `ensure_authenticated` but consults
/// [`crate::config::GoogleAutoLoginPlan`] so callers share one decision path.
pub async fn preflight(cfg: &crate::config::Config, no_browser: bool) -> Result<()> {
    match cfg.google_auto_login_plan() {
        crate::config::GoogleAutoLoginPlan::Ready => Ok(()),
        crate::config::GoogleAutoLoginPlan::Login { client_id } => {
            match ensure_authenticated(&client_id, no_browser).await {
                Ok(_) => Ok(()),
                Err(error) => Err(anyhow!(
                    "Google OAuth is required for this configuration but sign-in did not complete: {error}; run `varynth google login --client-id <CLIENT_ID>`"
                )),
            }
        }
        crate::config::GoogleAutoLoginPlan::Error(message) => Err(anyhow!("{message}")),
    }
}

/// Remove the Google OAuth credential from the encrypted OS keyring.
pub fn logout() -> Result<()> {
    let _operation = auth_operation_lock()
        .try_lock()
        .map_err(|_| anyhow!("a Google OAuth operation is already in progress; try again"))?;
    let _keyring = keyring_operation_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let entry = keyring_entry()?;
    match entry.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(_) => Err(anyhow!(
            "could not remove the Google OAuth credential from the OS keyring"
        )),
    }
}

/// Return only authentication state and the stored access-token expiry.
///
/// The returned JSON never contains a token, client identifier, client secret,
/// refresh token, account identity, scope, or provider response data.
pub fn status() -> Result<serde_json::Value> {
    let credential = load_credential()?;
    let (authenticated, expires_at) = match credential {
        Some(credential) => (
            !credential.client_id.is_empty() && !credential.refresh_token.is_empty(),
            Some(credential.expires_at),
        ),
        None => (false, None),
    };
    Ok(serde_json::json!({
        "authenticated": authenticated,
        "expires_at": expires_at,
    }))
}

fn auth_operation_lock() -> &'static AsyncMutex<()> {
    AUTH_OPERATION_LOCK.get_or_init(|| AsyncMutex::new(()))
}

fn keyring_operation_lock() -> &'static Mutex<()> {
    KEYRING_OPERATION_LOCK.get_or_init(|| Mutex::new(()))
}

fn keyring_entry() -> Result<keyring::Entry> {
    keyring::Entry::new(KEYRING_SERVICE, KEYRING_ENTRY)
        .map_err(|_| anyhow!("could not open the Google OAuth entry in the OS keyring"))
}

fn load_credential() -> Result<Option<StoredCredential>> {
    let _keyring = keyring_operation_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let entry = keyring_entry()?;
    let raw = match entry.get_password() {
        Ok(value) => value,
        Err(keyring::Error::NoEntry) => return Ok(None),
        Err(_) => {
            return Err(anyhow!(
                "could not read the Google OAuth credential from the OS keyring"
            ))
        }
    };
    let credential: StoredCredential = serde_json::from_str(&raw).map_err(|_| {
        anyhow!(
            "the stored Google OAuth credential is invalid; run `varynth google login --client-id <CLIENT_ID>`"
        )
    })?;
    validate_stored_credential(&credential)?;
    Ok(Some(credential))
}

fn save_credential(credential: &StoredCredential) -> Result<()> {
    validate_stored_credential(credential)?;
    let raw = serde_json::to_string(credential)
        .map_err(|_| anyhow!("could not serialize the Google OAuth credential"))?;
    let _keyring = keyring_operation_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let entry = keyring_entry()?;
    entry
        .set_password(&raw)
        .map_err(|_| anyhow!("could not persist the Google OAuth credential in the OS keyring"))
}

fn validate_stored_credential(credential: &StoredCredential) -> Result<()> {
    validate_client_id(&credential.client_id)?;
    if let Some(secret) = credential.client_secret.as_deref() {
        validate_client_secret(secret)?;
    }
    validate_token_value(&credential.refresh_token)?;
    validate_token_value(&credential.access_token)?;
    if credential.expires_at <= 0 {
        bail!("the stored Google OAuth credential has an invalid expiry");
    }
    Ok(())
}

fn validate_client_id(value: &str) -> Result<String> {
    let value = value.trim();
    if value.is_empty()
        || value.len() > 512
        || value.chars().any(|c| c.is_control() || c.is_whitespace())
    {
        bail!("Google OAuth client id is invalid");
    }
    Ok(value.to_string())
}

fn validate_client_secret(value: &str) -> Result<String> {
    if value.is_empty() || value.len() > MAX_TOKEN_BYTES || value.chars().any(char::is_control) {
        bail!("Google OAuth client secret is invalid");
    }
    Ok(value.to_string())
}

fn validate_token_value(value: &str) -> Result<()> {
    if value.is_empty() || value.len() > MAX_TOKEN_BYTES || value.chars().any(char::is_control) {
        bail!("stored Google OAuth credential contains an invalid token");
    }
    Ok(())
}

fn not_authenticated_error() -> anyhow::Error {
    anyhow!("not authenticated; run `varynth google login --client-id <CLIENT_ID>`")
}

fn generate_pkce_verifier() -> String {
    format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple())
}

fn pkce_challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

fn build_authorize_url(
    client_id: &str,
    redirect_uri: &str,
    state: &str,
    challenge: &str,
) -> Result<String> {
    let mut url = Url::parse(AUTHORIZATION_ENDPOINT)
        .map_err(|_| anyhow!("Google OAuth authorization endpoint is invalid"))?;
    url.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", client_id)
        .append_pair("redirect_uri", redirect_uri)
        .append_pair("scope", OAUTH_SCOPE)
        .append_pair("code_challenge", challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", state)
        .append_pair("access_type", "offline")
        .append_pair("prompt", "consent");
    Ok(url.to_string())
}

fn print_authorize_url(url: &str) -> Result<()> {
    let mut stdout = io::stdout().lock();
    writeln!(stdout, "{url}")
        .and_then(|_| stdout.flush())
        .map_err(|_| anyhow!("could not print the Google OAuth authorization URL"))
}

fn open_default_browser(url: &str) -> Result<()> {
    #[cfg(target_os = "windows")]
    {
        // URL is passed as a distinct argument; it is never interpolated into
        // a shell command string.
        Command::new("cmd.exe")
            .args(["/C", "start", ""])
            .arg(url)
            .spawn()
            .map_err(|_| anyhow!("could not open the default browser"))?;
    }
    #[cfg(target_os = "macos")]
    {
        Command::new("open")
            .arg(url)
            .spawn()
            .map_err(|_| anyhow!("could not open the default browser"))?;
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        Command::new("xdg-open")
            .arg(url)
            .spawn()
            .map_err(|_| anyhow!("could not open the default browser"))?;
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos", unix)))]
    {
        let _ = url;
        bail!("automatic browser opening is not supported on this platform");
    }
    Ok(())
}

async fn receive_authorization_code(
    listener: TcpListener,
    port: u16,
    expected_state: &str,
) -> Result<String> {
    let expected_host = format!("127.0.0.1:{port}");
    let deadline = Instant::now() + AUTHORIZATION_TIMEOUT;

    loop {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .unwrap_or_default();
        if remaining.is_zero() {
            bail!(
                "Google OAuth authorization timed out; run `varynth google login --client-id <CLIENT_ID>` again"
            );
        }
        let accepted = timeout(remaining, listener.accept())
            .await
            .map_err(|_| anyhow!("Google OAuth authorization timed out; try logging in again"))?
            .map_err(|_| anyhow!("the local Google OAuth callback listener failed"))?;
        let (mut stream, _) = accepted;
        let read_timeout = remaining.min(CALLBACK_READ_TIMEOUT);
        let request = timeout(read_timeout, read_callback_request(&mut stream)).await;
        let parsed = match request {
            Ok(Ok(bytes)) => parse_callback_request(&bytes, &expected_host, expected_state),
            Ok(Err(_)) | Err(_) => Err(anyhow!("invalid Google OAuth callback request")),
        };

        match parsed {
            Ok(code) => {
                let _ = write_callback_response(&mut stream, true).await;
                return Ok(code);
            }
            Err(_) => {
                // Invalid requests receive a generic response, then this
                // connection closes while the listener remains available for
                // another browser callback until the bounded deadline.
                let _ = write_callback_response(&mut stream, false).await;
            }
        }
    }
}

async fn read_callback_request<S>(stream: &mut S) -> Result<Vec<u8>>
where
    S: AsyncRead + Unpin,
{
    let mut request = Vec::with_capacity(1024);
    let mut chunk = [0_u8; 1024];
    loop {
        if request.len() >= MAX_CALLBACK_REQUEST_BYTES {
            bail!("callback request is too large");
        }
        let read = stream
            .read(&mut chunk)
            .await
            .map_err(|_| anyhow!("could not read the Google OAuth callback request"))?;
        if read == 0 {
            break;
        }
        if request.len().saturating_add(read) > MAX_CALLBACK_REQUEST_BYTES {
            bail!("callback request is too large");
        }
        request.extend_from_slice(&chunk[..read]);
        if let Some(end) = find_header_end(&request) {
            if end > MAX_CALLBACK_HEADER_BYTES {
                bail!("callback headers are too large");
            }
            return Ok(request);
        }
    }
    bail!("callback request ended before its headers were complete")
}

fn parse_callback_request(raw: &[u8], expected_host: &str, expected_state: &str) -> Result<String> {
    let header_end = find_header_end(raw).ok_or_else(|| anyhow!("invalid callback headers"))?;
    if header_end > MAX_CALLBACK_HEADER_BYTES || raw.len() > MAX_CALLBACK_REQUEST_BYTES {
        bail!("callback request is too large");
    }
    let headers = std::str::from_utf8(&raw[..header_end])
        .map_err(|_| anyhow!("callback headers are not valid HTTP"))?;
    let mut lines = headers.split("\r\n");
    let request_line = lines
        .next()
        .ok_or_else(|| anyhow!("callback request line is missing"))?;
    let mut request_parts = request_line.split_whitespace();
    let method = request_parts.next();
    let target = request_parts.next();
    let version = request_parts.next();
    if method != Some("GET") || version != Some("HTTP/1.1") || request_parts.next().is_some() {
        bail!("callback request line is invalid");
    }
    let target = target.ok_or_else(|| anyhow!("callback target is missing"))?;
    if target.len() > MAX_CALLBACK_TARGET_BYTES
        || !target.starts_with('/')
        || target.starts_with("//")
    {
        bail!("callback target is invalid");
    }

    let mut host = None;
    let mut header_count = 0;
    for line in lines {
        if line.is_empty() {
            break;
        }
        header_count += 1;
        if header_count > MAX_CALLBACK_HEADERS {
            bail!("callback has too many headers");
        }
        let (name, value) = line
            .split_once(':')
            .ok_or_else(|| anyhow!("callback header is invalid"))?;
        if name.is_empty() || !name.bytes().all(|byte| byte.is_ascii()) {
            bail!("callback header is invalid");
        }
        if name.eq_ignore_ascii_case("host") {
            if host.is_some() {
                bail!("callback has duplicate host headers");
            }
            host = Some(value.trim());
        }
    }
    let host = host.ok_or_else(|| anyhow!("callback host header is missing"))?;
    if !host.eq_ignore_ascii_case(expected_host) {
        bail!("callback host is not the local callback host");
    }

    // Url::query_pairs performs the required percent decoding and query
    // separator handling; no hand-rolled query parser is used here.
    let callback_url = Url::parse(&format!("http://127.0.0.1{target}"))
        .map_err(|_| anyhow!("callback URL is invalid"))?;
    if callback_url.path() != CALLBACK_PATH || callback_url.fragment().is_some() {
        bail!("callback path is invalid");
    }
    let state = query_parameter(&callback_url, "state")?
        .ok_or_else(|| anyhow!("callback state is missing"))?;
    if !constant_time_equal(state.as_bytes(), expected_state.as_bytes()) {
        bail!("callback state did not match");
    }

    if let Some(error) = query_parameter(&callback_url, "error")? {
        let error = sanitize_oauth_error_code(&error);
        bail!("Google OAuth authorization failed ({error})");
    }
    let code = query_parameter(&callback_url, "code")?
        .ok_or_else(|| anyhow!("callback authorization code is missing"))?;
    validate_token_value(&code)?;
    Ok(code)
}

fn query_parameter(url: &Url, name: &str) -> Result<Option<String>> {
    let mut found = None;
    for (key, value) in url.query_pairs() {
        if key == name {
            if found.is_some() {
                bail!("callback query contains duplicate parameters");
            }
            found = Some(value.into_owned());
        }
    }
    Ok(found)
}

fn find_header_end(raw: &[u8]) -> Option<usize> {
    raw.windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|position| position + 4)
}

async fn write_callback_response<S>(stream: &mut S, success: bool) -> Result<()>
where
    S: AsyncWrite + Unpin,
{
    let (status, body) = if success {
        (
            "200 OK",
            "Authorization received. You may return to Varynth.",
        )
    } else {
        ("400 Bad Request", "Authorization request was invalid.")
    };
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream
        .write_all(response.as_bytes())
        .await
        .map_err(|_| anyhow!("could not write the Google OAuth callback response"))?;
    let _ = stream.shutdown().await;
    Ok(())
}

fn constant_time_equal(left: &[u8], right: &[u8]) -> bool {
    let mut difference: usize = left.len() ^ right.len();
    for index in 0..left.len().max(right.len()) {
        let byte = left.get(index).copied().unwrap_or(0) ^ right.get(index).copied().unwrap_or(0);
        difference |= usize::from(byte);
    }
    difference == 0
}

fn sanitize_oauth_error_code(code: &str) -> &'static str {
    match code {
        "access_denied" => "access_denied",
        "invalid_request" => "invalid_request",
        "invalid_client" => "invalid_client",
        "invalid_grant" => "invalid_grant",
        "invalid_scope" => "invalid_scope",
        "unauthorized_client" => "unauthorized_client",
        "unsupported_grant_type" => "unsupported_grant_type",
        "unsupported_response_type" => "unsupported_response_type",
        "temporarily_unavailable" => "temporarily_unavailable",
        "server_error" => "server_error",
        "interaction_required" => "interaction_required",
        "login_required" => "login_required",
        "consent_required" => "consent_required",
        _ => "provider_error",
    }
}

fn oauth_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .connect_timeout(TOKEN_TIMEOUT)
        .timeout(TOKEN_TIMEOUT)
        .build()
        .map_err(|_| anyhow!("could not create the Google OAuth HTTP client"))
}

async fn exchange_authorization_code(
    client_id: &str,
    client_secret: Option<&str>,
    redirect_uri: &str,
    code: &str,
    verifier: &str,
) -> Result<TokenMaterial> {
    let client = oauth_client()?;
    let mut form = vec![
        ("grant_type", "authorization_code".to_string()),
        ("code", code.to_string()),
        ("redirect_uri", redirect_uri.to_string()),
        ("client_id", client_id.to_string()),
        ("code_verifier", verifier.to_string()),
    ];
    if let Some(client_secret) = client_secret {
        form.push(("client_secret", client_secret.to_string()));
    }
    let response = client
        .post(TOKEN_ENDPOINT)
        .form(&form)
        .send()
        .await
        .map_err(|_| anyhow!("Google OAuth token exchange failed; try logging in again"))?;
    let response = parse_token_response(response).await?;
    token_material(response, true)
}

async fn refresh_access_token(credential: &StoredCredential) -> Result<TokenMaterial> {
    let client = oauth_client()?;
    let mut form = vec![
        ("grant_type", "refresh_token".to_string()),
        ("refresh_token", credential.refresh_token.clone()),
        ("client_id", credential.client_id.clone()),
    ];
    if let Some(client_secret) = credential.client_secret.as_deref() {
        form.push(("client_secret", client_secret.to_string()));
    }
    let response = client
        .post(TOKEN_ENDPOINT)
        .form(&form)
        .send()
        .await
        .map_err(|_| {
            anyhow!(
                "Google OAuth token refresh failed; run `varynth google login --client-id <CLIENT_ID>` again"
            )
        })?;
    let response = parse_token_response(response).await?;
    token_material(response, false)
}

async fn parse_token_response(response: reqwest::Response) -> Result<TokenResponse> {
    let status = response.status();
    let body = read_bounded_response(response).await?;
    let parsed: TokenResponse = serde_json::from_slice(&body)
        .map_err(|_| anyhow!("Google OAuth token response was invalid"))?;
    if let Some(error) = parsed.error.as_deref() {
        let error = sanitize_oauth_error_code(error);
        bail!("Google OAuth token request failed ({error})");
    }
    if !status.is_success() {
        bail!("Google OAuth token request failed (provider_error)");
    }
    Ok(parsed)
}

async fn read_bounded_response(response: reqwest::Response) -> Result<Vec<u8>> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_TOKEN_RESPONSE_BYTES as u64)
    {
        bail!("Google OAuth token response was too large");
    }
    let mut bytes = Vec::new();
    let mut stream = std::pin::pin!(response.bytes_stream());
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| anyhow!("Google OAuth token response could not be read"))?;
        if bytes.len().saturating_add(chunk.len()) > MAX_TOKEN_RESPONSE_BYTES {
            bail!("Google OAuth token response was too large");
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

fn token_material(response: TokenResponse, require_refresh_token: bool) -> Result<TokenMaterial> {
    let access_token = response
        .access_token
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow!("Google OAuth response did not contain an access token"))?;
    validate_token_value(&access_token)?;
    let expires_in = response
        .expires_in
        .filter(|value| (1..=MAX_TOKEN_LIFETIME_SECS).contains(value))
        .ok_or_else(|| anyhow!("Google OAuth response did not contain a valid expiry"))?;
    let expires_at = now_unix_seconds()?
        .checked_add(
            i64::try_from(expires_in).map_err(|_| anyhow!("Google OAuth expiry is invalid"))?,
        )
        .ok_or_else(|| anyhow!("Google OAuth expiry is invalid"))?;
    let refresh_token = response.refresh_token.filter(|value| !value.is_empty());
    if require_refresh_token && refresh_token.is_none() {
        bail!("Google OAuth response did not contain a refresh token");
    }
    if let Some(refresh_token) = refresh_token.as_deref() {
        validate_token_value(refresh_token)?;
    }
    Ok(TokenMaterial {
        access_token,
        refresh_token,
        expires_at,
    })
}

fn credential_access_token_is_fresh(credential: &StoredCredential) -> Result<bool> {
    Ok(credential.expires_at > now_unix_seconds()?.saturating_add(ACCESS_TOKEN_REFRESH_SKEW_SECS))
}

fn now_unix_seconds() -> Result<i64> {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| anyhow!("system clock is before the Unix epoch"))?;
    i64::try_from(duration.as_secs()).map_err(|_| anyhow!("system clock value is invalid"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_challenge_matches_rfc_7636_s256_vector() {
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        assert_eq!(
            pkce_challenge(verifier),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn generated_verifier_is_two_uuid_hex_values() {
        let verifier = generate_pkce_verifier();
        assert_eq!(verifier.len(), 64);
        assert!(verifier.bytes().all(|byte| byte.is_ascii_hexdigit()));
    }

    #[test]
    fn callback_rejects_state_mismatch_without_echoing_code_or_description() {
        let raw = b"GET /oauth/callback?code=secret-code&state=wrong&error_description=secret-description HTTP/1.1\r\nHost: 127.0.0.1:43123\r\n\r\n";
        let error = parse_callback_request(raw, "127.0.0.1:43123", "expected-state")
            .expect_err("mismatched state must be rejected")
            .to_string();
        assert!(!error.contains("secret-code"));
        assert!(!error.contains("secret-description"));
    }

    #[test]
    fn callback_rejects_foreign_host() {
        let raw =
            b"GET /oauth/callback?code=code&state=state HTTP/1.1\r\nHost: attacker.example\r\n\r\n";
        assert!(parse_callback_request(raw, "127.0.0.1:43123", "state").is_err());
    }

    #[test]
    fn callback_requires_exact_path() {
        let raw = b"GET /oauth/callback/extra?code=code&state=state HTTP/1.1\r\nHost: 127.0.0.1:43123\r\n\r\n";
        assert!(parse_callback_request(raw, "127.0.0.1:43123", "state").is_err());
    }

    #[test]
    fn callback_error_is_allowlisted_and_does_not_echo_description() {
        let raw = b"GET /oauth/callback?error=access_denied&error_description=secret-description&state=state HTTP/1.1\r\nHost: 127.0.0.1:43123\r\n\r\n";
        let error = parse_callback_request(raw, "127.0.0.1:43123", "state")
            .expect_err("provider error must be rejected")
            .to_string();
        assert_eq!(error, "Google OAuth authorization failed (access_denied)");
        assert!(!error.contains("secret-description"));
    }

    #[test]
    fn callback_query_is_decoded_by_url_parser() {
        let raw = b"GET /oauth/callback?code=encoded%2Fcode&state=state HTTP/1.1\r\nHost: 127.0.0.1:43123\r\n\r\n";
        assert_eq!(
            parse_callback_request(raw, "127.0.0.1:43123", "state").unwrap(),
            "encoded/code"
        );
    }

    #[test]
    fn callback_response_has_no_credential_body() {
        let body = "Authorization received. You may return to Varynth.";
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        assert!(!response.contains("access_token"));
        assert!(!response.contains("refresh_token"));
        assert!(!response.contains("secret-code"));
    }

    #[test]
    fn state_comparison_is_exact() {
        assert!(constant_time_equal(b"state", b"state"));
        assert!(!constant_time_equal(b"state", b"state-extra"));
        assert!(!constant_time_equal(b"state", b"statu"));
    }

    #[test]
    fn status_shape_contains_only_boolean_and_expiry_keys() {
        let value = serde_json::json!({
            "authenticated": false,
            "expires_at": serde_json::Value::Null,
        });
        let object = value.as_object().expect("status is an object");
        assert_eq!(object.len(), 2);
        assert!(object.contains_key("authenticated"));
        assert!(object.contains_key("expires_at"));
    }
}
