use chrono::{DateTime, Utc};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::error::ApiError;

/// OpenCode Zen's free tier is 100 requests/day. Anonymous stays below that.
/// Signed-in free matches 100/day. Pro is the subscription tier.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Quota {
    pub chat_per_day: i32,
    pub chat_per_minute: i32,
    pub search_per_day: i32,
}

pub fn quota(signed_in: bool, plan: &str) -> Quota {
    if !signed_in {
        return Quota {
            chat_per_day: 20,
            chat_per_minute: 5,
            search_per_day: 0,
        };
    }
    if plan == "pro" {
        return Quota {
            chat_per_day: 2000,
            chat_per_minute: 60,
            search_per_day: 500,
        };
    }
    Quota {
        chat_per_day: 100,
        chat_per_minute: 20,
        search_per_day: 50,
    }
}

pub fn day_bucket(now: DateTime<Utc>) -> String {
    now.format("day:%Y-%m-%d").to_string()
}

pub fn minute_bucket(now: DateTime<Utc>) -> String {
    now.format("min:%Y-%m-%dT%H:%M").to_string()
}

pub async fn consume(
    pool: &sqlx::PgPool,
    subject: &str,
    kind: &str,
    bucket: &str,
    limit: i32,
) -> Result<i32, ApiError> {
    if limit <= 0 {
        return Err(ApiError::RateLimited(
            "sign in with varynth login to use search".into(),
        ));
    }
    consume_message(pool, subject, kind, bucket, limit, None).await
}

/// Same counter as [`consume`], with a caller-supplied message for the client.
pub async fn consume_message(
    pool: &sqlx::PgPool,
    subject: &str,
    kind: &str,
    bucket: &str,
    limit: i32,
    message: Option<&str>,
) -> Result<i32, ApiError> {
    if limit <= 0 {
        return Err(ApiError::RateLimited(
            message.unwrap_or("limit reached").to_string(),
        ));
    }
    let count = sqlx::query_scalar::<_, i32>(
        "INSERT INTO usage_counters (subject, kind, bucket, count)
         VALUES ($1, $2, $3, 1)
         ON CONFLICT (subject, kind, bucket)
         DO UPDATE SET count = usage_counters.count + 1
         RETURNING count",
    )
    .bind(subject)
    .bind(kind)
    .bind(bucket)
    .fetch_one(pool)
    .await?;
    if count > limit {
        Err(ApiError::RateLimited(match message {
            Some(message) => message.to_string(),
            None => format!("{kind} limit reached ({limit} per window)"),
        }))
    } else {
        Ok(count)
    }
}

#[derive(Clone, Copy, Debug)]
pub struct LockoutPolicy {
    pub max_failures: u32,
    pub base: Duration,
    pub cap: Duration,
}

impl Default for LockoutPolicy {
    fn default() -> Self {
        Self {
            max_failures: 5,
            base: Duration::from_millis(500),
            cap: Duration::from_secs(300),
        }
    }
}

struct Attempt {
    failures: u32,
    locked_until: Option<Instant>,
    seen: Instant,
}

const AUTH_ENTRY_TTL: Duration = Duration::from_secs(3600);
const AUTH_MAX_ENTRIES: usize = 4096;

/// In-process failure lockout keyed by IP + account. After
/// `max_failures` consecutive failures the key is locked with exponential
/// backoff (base, 2x, 4x, ... up to `cap`), so rotating accounts does not
/// bypass it and a successful sign-in resets the counter.
pub struct AuthLimiter {
    policy: LockoutPolicy,
    entries: Mutex<HashMap<String, Attempt>>,
}

impl Default for AuthLimiter {
    fn default() -> Self {
        Self::new(LockoutPolicy::default())
    }
}

impl AuthLimiter {
    pub fn new(policy: LockoutPolicy) -> Self {
        Self {
            policy,
            entries: Mutex::new(HashMap::new()),
        }
    }

    /// Remaining lockout for `key`, if the key is currently locked.
    pub fn retry_after(&self, key: &str) -> Option<Duration> {
        let now = Instant::now();
        let entries = self.entries.lock().unwrap_or_else(|error| error.into_inner());
        match entries.get(key).and_then(|attempt| attempt.locked_until) {
            Some(until) if until > now => Some(until - now),
            _ => None,
        }
    }

    /// Records one failed attempt and returns the resulting lockout length
    /// (`Duration::ZERO` while still below the failure threshold).
    pub fn record_failure(&self, key: &str) -> Duration {
        let now = Instant::now();
        let mut entries = self.entries.lock().unwrap_or_else(|error| error.into_inner());
        prune(&mut entries, now);
        let attempt = entries.entry(key.to_string()).or_insert(Attempt {
            failures: 0,
            locked_until: None,
            seen: now,
        });
        attempt.failures = attempt.failures.saturating_add(1);
        attempt.seen = now;
        if attempt.failures < self.policy.max_failures {
            return Duration::ZERO;
        }
        let exponent = (attempt.failures - self.policy.max_failures).min(16);
        let backoff = self
            .policy
            .base
            .saturating_mul(1u32 << exponent)
            .min(self.policy.cap);
        attempt.locked_until = Some(now + backoff);
        backoff
    }

    pub fn record_success(&self, key: &str) {
        let mut entries = self.entries.lock().unwrap_or_else(|error| error.into_inner());
        entries.remove(key);
    }

    #[cfg(test)]
    fn failure_count(&self, key: &str) -> u32 {
        let entries = self.entries.lock().unwrap_or_else(|error| error.into_inner());
        entries.get(key).map(|attempt| attempt.failures).unwrap_or(0)
    }
}

fn prune(entries: &mut HashMap<String, Attempt>, now: Instant) {
    if entries.len() < AUTH_MAX_ENTRIES {
        entries.retain(|_, attempt| now.duration_since(attempt.seen) < AUTH_ENTRY_TTL);
        return;
    }
    entries.retain(|_, attempt| now.duration_since(attempt.seen) < AUTH_ENTRY_TTL);
    if entries.len() >= AUTH_MAX_ENTRIES {
        // Still saturated: drop unlocked counters only, so active lockouts
        // keep their backoff.
        entries.retain(|_, attempt| attempt.locked_until.is_some_and(|until| until > now));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotas_match_opencode_shape() {
        assert_eq!(quota(false, "free").chat_per_day, 20);
        assert_eq!(quota(false, "free").search_per_day, 0);
        assert_eq!(quota(true, "free").chat_per_day, 100);
        assert_eq!(quota(true, "pro").search_per_day, 500);
    }

    fn fast_policy() -> LockoutPolicy {
        LockoutPolicy {
            max_failures: 3,
            base: Duration::from_millis(20),
            cap: Duration::from_millis(200),
        }
    }

    #[test]
    fn lockout_after_repeated_failures_and_success_resets() {
        let limiter = AuthLimiter::new(fast_policy());
        let key = "203.0.113.9|user@example.com";
        assert_eq!(limiter.record_failure(key), Duration::ZERO);
        assert_eq!(limiter.record_failure(key), Duration::ZERO);
        assert!(limiter.retry_after(key).is_none());
        assert!(!limiter.record_failure(key).is_zero());
        assert!(limiter.retry_after(key).is_some());
        limiter.record_success(key);
        assert!(limiter.retry_after(key).is_none());
        assert_eq!(limiter.failure_count(key), 0);
    }

    #[test]
    fn lockout_keys_are_independent_per_ip_and_account() {
        let limiter = AuthLimiter::new(fast_policy());
        for _ in 0..3 {
            limiter.record_failure("203.0.113.9|a@example.com");
        }
        assert!(limiter.retry_after("203.0.113.9|a@example.com").is_some());
        assert!(limiter.retry_after("203.0.113.9|b@example.com").is_none());
        assert!(limiter.retry_after("198.51.100.4|a@example.com").is_none());
    }

    #[test]
    fn backoff_grows_and_is_capped() {
        let limiter = AuthLimiter::new(fast_policy());
        let key = "203.0.113.9|user@example.com";
        let first = {
            limiter.record_failure(key);
            limiter.record_failure(key);
            limiter.record_failure(key)
        };
        let second = limiter.record_failure(key);
        let capped = {
            for _ in 0..10 {
                limiter.record_failure(key);
            }
            limiter.record_failure(key)
        };
        assert!(second > first, "{second:?} !> {first:?}");
        assert_eq!(capped, fast_policy().cap);
    }

    #[test]
    fn wait_shrinks_after_lock_expires() {
        let limiter = AuthLimiter::new(LockoutPolicy {
            max_failures: 1,
            base: Duration::from_millis(30),
            cap: Duration::from_millis(30),
        });
        let key = "203.0.113.9|user@example.com";
        limiter.record_failure(key);
        assert!(limiter.retry_after(key).is_some());
        std::thread::sleep(Duration::from_millis(60));
        assert!(limiter.retry_after(key).is_none());
    }
}
