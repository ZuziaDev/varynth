use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use chrono::{DateTime, Duration, Utc};
use jsonwebtoken::{decode, encode, DecodingKey, EncodingKey, Header, Validation};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use url::Url;

use crate::error::ApiError;

pub const ACCESS_TTL_SECS: i64 = 3600;
pub const REFRESH_TTL_SECS: i64 = 30 * 24 * 3600;
pub const WEB_TTL_SECS: i64 = 7 * 24 * 3600;
pub const CODE_TTL_SECS: i64 = 300;

#[derive(Debug, Serialize, Deserialize)]
pub struct Claims {
    pub sub: String,
    pub exp: usize,
    pub iat: usize,
    pub scope: String,
}

pub fn random_token() -> String {
    let mut bytes = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

pub fn sha256(bytes: &[u8]) -> Vec<u8> {
    Sha256::digest(bytes).to_vec()
}

pub fn pkce_s256(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

pub fn sign_token(secret: &str, user_id: &str, scope: &str, ttl: i64) -> Result<String, ApiError> {
    let now = Utc::now().timestamp();
    let claims = Claims {
        sub: user_id.to_string(),
        iat: now as usize,
        exp: (now + ttl) as usize,
        scope: scope.to_string(),
    };
    encode(
        &Header::default(),
        &claims,
        &EncodingKey::from_secret(secret.as_bytes()),
    )
    .map_err(|error| {
        tracing::error!(%error, "jwt encode");
        ApiError::Internal
    })
}

pub fn read_token(secret: &str, token: &str) -> Result<Claims, ApiError> {
    let mut validation = Validation::default();
    validation.validate_exp = true;
    validation.required_spec_claims.clear();
    validation.set_required_spec_claims(&["exp", "sub"]);
    decode::<Claims>(
        token,
        &DecodingKey::from_secret(secret.as_bytes()),
        &validation,
    )
    .map(|data| data.claims)
    .map_err(|_| ApiError::Unauthorized)
}

pub fn expiry_from_now(secs: i64) -> DateTime<Utc> {
    Utc::now() + Duration::seconds(secs)
}

/// Loopback authorization-code redirect. Path must be `/oauth/callback`.
pub fn valid_loopback_redirect(uri: &str) -> bool {
    let Ok(url) = Url::parse(uri) else {
        return false;
    };
    if url.scheme() != "http" || url.path() != "/oauth/callback" {
        return false;
    }
    if url.query().is_some() || url.fragment().is_some() || url.username() != "" || url.port().is_none()
    {
        return false;
    }
    matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "[::1]"))
        || url.host().is_some_and(|host| host.to_string() == "::1")
}

pub fn append_query(redirect: &str, pairs: &[(&str, &str)]) -> Result<String, ApiError> {
    let mut url = Url::parse(redirect).map_err(|_| ApiError::bad("redirect_uri is invalid"))?;
    {
        let mut query = url.query_pairs_mut();
        for (key, value) in pairs {
            query.append_pair(key, value);
        }
    }
    Ok(url.into())
}

pub fn valid_state(state: &str) -> bool {
    !state.is_empty()
        && state.len() <= 256
        && state
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '~'))
}

pub fn valid_challenge(challenge: &str) -> bool {
    (43..=128).contains(&challenge.len())
        && challenge
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
}

pub fn scrub_upstream_error(body: &str) -> String {
    let lower = body.to_ascii_lowercase();
    if lower.contains("cloudflare")
        || lower.contains("workers")
        || body.contains("@cf/")
        || lower.contains("api.cloudflare.com")
        || lower.contains("hrmn")
    {
        return "upstream request failed".into();
    }
    let mut out = String::new();
    for ch in body.chars().take(240) {
        if ch.is_control() {
            continue;
        }
        out.push(ch);
    }
    if out.is_empty() {
        "upstream request failed".into()
    } else {
        out
    }
}

pub fn public_json_is_clean(body: &str) -> bool {
    let lower = body.to_ascii_lowercase();
    !lower.contains("cloudflare")
        && !lower.contains("workers")
        && !body.contains("@cf/")
        && !lower.contains("api.cloudflare.com")
        && !lower.contains("api.hrmn.tr")
        && !lower.contains("hrmnapi")
        && !lower.contains("upstream_model")
        && !lower.contains("upstream_base_url")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_s256_is_stable() {
        assert_eq!(pkce_s256("verifier-value-verifier-value-verifier"), pkce_s256("verifier-value-verifier-value-verifier"));
        assert_ne!(pkce_s256("verifier-value-verifier-value-verifier"), pkce_s256("other-verifier-value-other-verifier-value"));
    }

    #[test]
    fn redirect_accepts_only_loopback_callback() {
        assert!(valid_loopback_redirect("http://127.0.0.1:4312/oauth/callback"));
        assert!(valid_loopback_redirect("http://localhost:4312/oauth/callback"));
        assert!(valid_loopback_redirect("http://[::1]:4312/oauth/callback"));
        assert!(!valid_loopback_redirect("https://127.0.0.1:4312/oauth/callback"));
        assert!(!valid_loopback_redirect("http://127.0.0.1:4312/oauth/callback?x=1"));
        assert!(!valid_loopback_redirect("http://evil.example/oauth/callback"));
        assert!(!valid_loopback_redirect("http://127.0.0.1/oauth/callback"));
    }

    #[test]
    fn scrub_strips_worker_names() {
        assert_eq!(scrub_upstream_error("Cloudflare Workers @cf/meta"), "upstream request failed");
        assert!(public_json_is_clean(r#"{"id":"varynth/public-1","owned_by":"varynth"}"#));
        assert!(!public_json_is_clean(r#"{"upstream_model":"@cf/meta"}"#));
    }
}
