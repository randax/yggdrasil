//! In-process per-credential request limiting at the authentication seam.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::http::{HeaderValue, StatusCode, header};
use axum::response::Response;

use crate::config::TokenRateLimitConfig;
use crate::error::error_json;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum TokenRateLimitKey {
    Bootstrap,
    Member(MemberRateLimitKey),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct MemberRateLimitKey(String);

impl MemberRateLimitKey {
    pub(crate) fn new(id: &yg_control::MemberTokenId) -> Self {
        Self(id.as_str().to_owned())
    }

    #[cfg(test)]
    fn fixture(id: &str) -> Self {
        Self(id.to_owned())
    }
}

#[derive(Clone)]
pub(crate) struct TokenRateLimiter {
    inner: Arc<Mutex<LimiterState>>,
    requests: usize,
    window: Duration,
}

#[derive(Default)]
struct LimiterState {
    requests_by_token: HashMap<TokenRateLimitKey, VecDeque<Instant>>,
    checks_since_prune: usize,
}

impl TokenRateLimiter {
    pub(crate) fn new(config: TokenRateLimitConfig) -> Self {
        Self {
            inner: Arc::new(Mutex::new(LimiterState::default())),
            requests: usize::try_from(config.requests.get()).expect("u32 fits in usize"),
            window: config.window.max(Duration::from_millis(1)),
        }
    }

    pub(crate) fn check(&self, key: TokenRateLimitKey) -> Result<(), Duration> {
        self.check_at(key, Instant::now())
    }

    fn check_at(&self, key: TokenRateLimitKey, now: Instant) -> Result<(), Duration> {
        let cutoff = now.checked_sub(self.window).unwrap_or(now);
        let mut state = self.inner.lock().expect("rate limiter lock poisoned");
        state.checks_since_prune += 1;
        if state.checks_since_prune >= 1024 {
            state
                .requests_by_token
                .retain(|_, requests| requests.back().is_some_and(|last| *last > cutoff));
            state.checks_since_prune = 0;
        }

        let requests = state.requests_by_token.entry(key).or_default();
        while requests.front().is_some_and(|instant| *instant <= cutoff) {
            requests.pop_front();
        }
        if requests.len() < self.requests {
            requests.push_back(now);
            return Ok(());
        }

        let oldest = requests
            .front()
            .copied()
            .expect("full window has a request");
        Err(self
            .window
            .saturating_sub(now.saturating_duration_since(oldest)))
    }
}

pub(crate) fn rate_limited(retry_after: Duration) -> Response {
    let retry_after_seconds =
        (retry_after.as_secs() + u64::from(retry_after.subsec_nanos() != 0)).max(1);
    let mut response = error_json(StatusCode::TOO_MANY_REQUESTS, "token rate limit exceeded");
    response.headers_mut().insert(
        header::RETRY_AFTER,
        HeaderValue::from_str(&retry_after_seconds.to_string())
            .expect("integer Retry-After is a valid header value"),
    );
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotas_are_isolated_by_token_and_recover_after_the_window() {
        let limiter = TokenRateLimiter::new(TokenRateLimitConfig {
            requests: std::num::NonZeroU32::new(1).expect("one is nonzero"),
            window: Duration::from_secs(10),
        });
        let start = Instant::now();

        assert_eq!(
            limiter.check_at(TokenRateLimitKey::Bootstrap, start),
            Ok(())
        );
        assert_eq!(
            limiter.check_at(TokenRateLimitKey::Bootstrap, start),
            Err(Duration::from_secs(10))
        );

        let member = MemberRateLimitKey::fixture("mtok_0123456789abcdef01234567");
        assert_eq!(
            limiter.check_at(TokenRateLimitKey::Member(member), start),
            Ok(())
        );
        assert_eq!(
            limiter.check_at(
                TokenRateLimitKey::Bootstrap,
                start + Duration::from_secs(10)
            ),
            Ok(())
        );
    }
}
