//! Authentication and the admin scope gate. Authorization itself lives
//! in the route table (issue #38): these middlewares decide *who* is
//! calling, the router's shape decides what they may reach.
//!
//! Failed authentication uses one server-wide sliding window rather than
//! attacker-controlled per-token state. Once full, member-token candidates
//! are rejected before their database lookup. That deliberately means a
//! failure flood can briefly reject a valid member token too; bootstrap tokens
//! remain recognizable without the database and are unaffected.

use std::collections::VecDeque;
use std::future::Future;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use crate::AppState;
use crate::MetricsServerState;
use crate::error::{ApiError, error_json};
use crate::rate_limit::{
    MemberRateLimitKey, TokenRateLimitKey, rate_limited, rate_limited_with_message,
};

const MAX_AUTH_FAILURES_PER_MINUTE: usize = 60;
const AUTH_FAILURE_WINDOW: Duration = Duration::from_secs(60);

#[derive(Clone, Default)]
pub(crate) struct AuthFailureLimiter {
    state: Arc<Mutex<AuthFailureState>>,
}

#[derive(Default)]
struct AuthFailureState {
    failures: VecDeque<Instant>,
    lookups_in_flight: usize,
}

impl AuthFailureLimiter {
    fn reserve_lookup(&self) -> Result<AuthLookupReservation, Duration> {
        self.reserve_lookup_at(Instant::now())
    }

    fn reserve_lookup_at(&self, now: Instant) -> Result<AuthLookupReservation, Duration> {
        let mut state = self.state.lock().expect("auth failure lock poisoned");
        Self::prune(&mut state, now);
        if state.failures.len() + state.lookups_in_flight >= MAX_AUTH_FAILURES_PER_MINUTE {
            return Err(Self::retry_after(&state, now));
        }
        state.lookups_in_flight += 1;
        Ok(AuthLookupReservation {
            limiter: self.clone(),
            active: true,
        })
    }

    fn record_direct_failure(&self) -> Result<(), Duration> {
        self.record_direct_failure_at(Instant::now())
    }

    fn record_direct_failure_at(&self, now: Instant) -> Result<(), Duration> {
        let mut state = self.state.lock().expect("auth failure lock poisoned");
        Self::prune(&mut state, now);
        if state.failures.len() + state.lookups_in_flight >= MAX_AUTH_FAILURES_PER_MINUTE {
            return Err(Self::retry_after(&state, now));
        }
        state.failures.push_back(now);
        Ok(())
    }

    fn prune(state: &mut AuthFailureState, now: Instant) {
        let cutoff = now.checked_sub(AUTH_FAILURE_WINDOW).unwrap_or(now);
        while state
            .failures
            .front()
            .is_some_and(|failure| *failure <= cutoff)
        {
            state.failures.pop_front();
        }
    }

    fn retry_after(state: &AuthFailureState, now: Instant) -> Duration {
        state
            .failures
            .front()
            .copied()
            .map(|oldest| AUTH_FAILURE_WINDOW.saturating_sub(now.saturating_duration_since(oldest)))
            // Every occupied slot can instead be an in-flight lookup. Those
            // slots are short-lived and have no window timestamp yet.
            .unwrap_or(Duration::from_secs(1))
    }
}

struct AuthLookupReservation {
    limiter: AuthFailureLimiter,
    active: bool,
}

impl AuthLookupReservation {
    fn record_failure(mut self) {
        let mut state = self
            .limiter
            .state
            .lock()
            .expect("auth failure lock poisoned");
        state.lookups_in_flight -= 1;
        state.failures.push_back(Instant::now());
        self.active = false;
    }
}

impl Drop for AuthLookupReservation {
    fn drop(&mut self) {
        if self.active {
            let mut state = self
                .limiter
                .state
                .lock()
                .expect("auth failure lock poisoned");
            state.lookups_in_flight -= 1;
        }
    }
}

enum GuardedMemberLookup<T, E> {
    Authenticated(T),
    Rejected,
    Failed(E),
}

async fn lookup_member_with_guard<T, E, Lookup, LookupFuture>(
    limiter: &AuthFailureLimiter,
    lookup: Lookup,
) -> Result<GuardedMemberLookup<T, E>, Duration>
where
    Lookup: FnOnce() -> LookupFuture,
    LookupFuture: Future<Output = Result<Option<T>, E>>,
{
    let reservation = limiter.reserve_lookup()?;
    match lookup().await {
        Ok(Some(authenticated)) => Ok(GuardedMemberLookup::Authenticated(authenticated)),
        Ok(None) => {
            reservation.record_failure();
            Ok(GuardedMemberLookup::Rejected)
        }
        Err(error) => Ok(GuardedMemberLookup::Failed(error)),
    }
}

fn auth_failure_rate_limited(retry_after: Duration) -> Response {
    rate_limited_with_message(retry_after, "authentication failure rate limit exceeded")
}

fn presented_bearer(req: &Request) -> Option<&str> {
    req.headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        // RFC 9110: the scheme is case-insensitive.
        .and_then(|value| value.split_once(' '))
        .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("bearer"))
        // RFC 9110 allows 1*SP between scheme and credentials.
        .map(|(_, presented)| presented.trim_start_matches(' '))
}

fn is_bootstrap_token(presented: &str, expected: &str) -> bool {
    use subtle::ConstantTimeEq;

    presented.as_bytes().ct_eq(expected.as_bytes()).into()
}

/// What a bearer token is scoped to, decided once by [`authenticate`] and
/// carried in request extensions. Authorization itself lives in the route
/// table: admin routes check for [`TokenScope::Admin`] via [`require_admin`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum TokenScope {
    Admin,
    Member,
}

/// Every route behind the auth layer requires a bearer token. The
/// bootstrap token is Admin-scoped; a stored member token is
/// Member-scoped. This middleware only authenticates and records the
/// scope — which routes a scope may reach is the router's shape.
pub(crate) async fn authenticate(
    State(state): State<Arc<AppState>>,
    mut req: Request,
    next: Next,
) -> Response {
    let Some(presented) = presented_bearer(&req) else {
        if let Err(retry_after) = state.auth_failure_limiter.record_direct_failure() {
            return auth_failure_rate_limited(retry_after);
        }
        return error_json(StatusCode::UNAUTHORIZED, "missing or invalid bearer token");
    };

    let (scope, rate_limit_key) = if is_bootstrap_token(presented, &state.bootstrap_token) {
        (TokenScope::Admin, TokenRateLimitKey::Bootstrap)
    } else {
        match lookup_member_with_guard(&state.auth_failure_limiter, || {
            state.control.authenticate_member_token(presented)
        })
        .await
        {
            Err(retry_after) => return auth_failure_rate_limited(retry_after),
            Ok(GuardedMemberLookup::Authenticated(authenticated)) => (
                TokenScope::Member,
                TokenRateLimitKey::Member(MemberRateLimitKey::new(&authenticated.id)),
            ),
            Ok(GuardedMemberLookup::Rejected) => {
                return error_json(StatusCode::UNAUTHORIZED, "missing or invalid bearer token");
            }
            Ok(GuardedMemberLookup::Failed(e)) => {
                return ApiError::internal(e.context("auth lookup failed")).into_response();
            }
        }
    };
    // Metrics scrapes are separately Admin-gated and must not consume the
    // bootstrap token's ordinary API request budget.
    let is_admin_metrics_scrape = req.uri().path() == "/metrics" && scope == TokenScope::Admin;
    if !is_admin_metrics_scrape && let Err(retry_after) = state.rate_limiter.check(rate_limit_key) {
        return rate_limited(retry_after);
    }
    req.extensions_mut().insert(scope);
    next.run(req).await
}

/// Bootstrap-token authentication for the worker's metrics-only listener.
/// It shares bearer parsing and constant-time comparison with the full API,
/// but deliberately has no member-token database fallback.
pub(crate) async fn authenticate_metrics_admin(
    State(state): State<Arc<MetricsServerState>>,
    req: Request,
    next: Next,
) -> Response {
    let authenticated = presented_bearer(&req)
        .is_some_and(|presented| is_bootstrap_token(presented, &state.bootstrap_token));
    if !authenticated {
        return error_json(StatusCode::UNAUTHORIZED, "missing or invalid bearer token");
    }
    next.run(req).await
}

/// The admin router's gate: any authenticated caller reaches it, only an
/// Admin token passes.
pub(crate) async fn require_admin(req: Request, next: Next) -> Response {
    match req.extensions().get::<TokenScope>() {
        Some(TokenScope::Admin) => next.run(req).await,
        _ => error_json(
            StatusCode::FORBIDDEN,
            "this operation requires an Admin token; member tokens may call \
             Verbs and the read-only status route",
        ),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    #[tokio::test]
    async fn failure_flood_blocks_the_next_lookup_and_recovers_after_the_window() {
        let limiter = AuthFailureLimiter::default();
        let lookups = AtomicUsize::new(0);

        for _ in 0..MAX_AUTH_FAILURES_PER_MINUTE {
            let result = lookup_member_with_guard(&limiter, || async {
                lookups.fetch_add(1, Ordering::SeqCst);
                Ok::<Option<()>, ()>(None)
            })
            .await;
            assert!(matches!(result, Ok(GuardedMemberLookup::Rejected)));
        }

        let result = lookup_member_with_guard(&limiter, || async {
            lookups.fetch_add(1, Ordering::SeqCst);
            Ok::<Option<()>, ()>(None)
        })
        .await;
        assert!(result.is_err());
        assert_eq!(lookups.load(Ordering::SeqCst), MAX_AUTH_FAILURES_PER_MINUTE);
        let reservation = limiter
            .reserve_lookup_at(Instant::now() + AUTH_FAILURE_WINDOW)
            .expect("the failures expire after the window");
        drop(reservation);
    }

    #[tokio::test]
    async fn concurrent_failure_flood_bounds_lookup_starts_and_stored_state() {
        let limiter = AuthFailureLimiter::default();
        let lookups = Arc::new(AtomicUsize::new(0));
        let release = Arc::new(tokio::sync::Semaphore::new(0));
        let mut tasks = Vec::new();

        for _ in 0..MAX_AUTH_FAILURES_PER_MINUTE * 2 {
            let limiter = limiter.clone();
            let lookups = lookups.clone();
            let release = release.clone();
            tasks.push(tokio::spawn(async move {
                lookup_member_with_guard(&limiter, || async move {
                    lookups.fetch_add(1, Ordering::SeqCst);
                    let permit = release.acquire().await.expect("test semaphore stays open");
                    drop(permit);
                    Ok::<Option<()>, ()>(None)
                })
                .await
            }));
        }

        tokio::time::timeout(Duration::from_secs(1), async {
            while lookups.load(Ordering::SeqCst) < MAX_AUTH_FAILURES_PER_MINUTE {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the bounded lookup set starts");
        assert_eq!(lookups.load(Ordering::SeqCst), MAX_AUTH_FAILURES_PER_MINUTE);

        release.add_permits(MAX_AUTH_FAILURES_PER_MINUTE);
        for task in tasks {
            let _ = task.await.expect("auth attempt task completes");
        }

        let state = limiter.state.lock().expect("auth failure lock poisoned");
        assert_eq!(state.lookups_in_flight, 0);
        assert_eq!(state.failures.len(), MAX_AUTH_FAILURES_PER_MINUTE);
    }

    #[test]
    fn bootstrap_token_check_is_independent_of_the_failure_window() {
        let limiter = AuthFailureLimiter::default();
        let now = Instant::now();
        for _ in 0..MAX_AUTH_FAILURES_PER_MINUTE {
            limiter
                .record_direct_failure_at(now)
                .expect("fixture failure fits in the window");
        }

        assert!(limiter.reserve_lookup_at(now).is_err());
        assert!(is_bootstrap_token("bootstrap-secret", "bootstrap-secret"));
    }
}
