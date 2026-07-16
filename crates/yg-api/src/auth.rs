//! Authentication and the admin scope gate. Authorization itself lives
//! in the route table (issue #38): these middlewares decide *who* is
//! calling, the router's shape decides what they may reach.
//!
//! Failed member-token authentication uses a bounded negative cache keyed by
//! the token's SHA-256 digest. Repeats of a rejected token are throttled before
//! database access without sharing a bucket with other tokens. A server-wide
//! sliding window still bounds database work from floods of distinct tokens;
//! previously accepted token digests bypass that failure guard but are still
//! revalidated by the database on every request, preserving revocation.
//! Bootstrap tokens remain recognizable without the database and bypass it.

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use sha2::{Digest, Sha256};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, watch};

use crate::AppState;
use crate::MetricsServerState;
use crate::error::{ApiError, error_json};
use crate::rate_limit::{
    MemberRateLimitKey, TokenRateLimitKey, rate_limited, rate_limited_with_message,
};

const MAX_AUTH_FAILURES_PER_MINUTE: usize = 60;
const AUTH_FAILURE_CACHE_CAPACITY: usize = 1_024;
const AUTH_ACCEPTED_TOKEN_CACHE_CAPACITY: usize = 1_024;
const MAX_ACCEPTED_TOKEN_LOOKUPS_IN_FLIGHT: usize = 8;
const AUTH_FAILURE_WINDOW: Duration = Duration::from_secs(60);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct AuthTokenHash([u8; 32]);

impl AuthTokenHash {
    fn new(token: &str) -> Self {
        Self(Sha256::digest(token.as_bytes()).into())
    }
}

#[derive(Clone, Default)]
pub(crate) struct AuthFailureLimiter {
    state: Arc<Mutex<AuthFailureState>>,
}

#[derive(Default)]
struct AuthFailureState {
    failures: VecDeque<Instant>,
    rejected_tokens: VecDeque<(AuthTokenHash, Instant)>,
    accepted_tokens: VecDeque<AcceptedToken>,
    lookups_in_flight: HashMap<AuthTokenHash, watch::Receiver<()>>,
}

struct AcceptedToken {
    token_hash: AuthTokenHash,
    permits: Arc<Semaphore>,
}

enum AuthLookupBlocked {
    RateLimited(Duration),
    Pending(watch::Receiver<()>),
    AcceptedAtCapacity(Arc<Semaphore>),
}

impl AuthFailureLimiter {
    fn reserve_lookup(
        &self,
        token_hash: AuthTokenHash,
    ) -> Result<AuthLookupReservation, AuthLookupBlocked> {
        self.reserve_lookup_at(token_hash, Instant::now())
    }

    fn reserve_lookup_at(
        &self,
        token_hash: AuthTokenHash,
        now: Instant,
    ) -> Result<AuthLookupReservation, AuthLookupBlocked> {
        let mut state = self.state.lock().expect("auth failure lock poisoned");
        Self::prune(&mut state, now);

        if let Some(index) = state
            .rejected_tokens
            .iter()
            .position(|(cached, _)| *cached == token_hash)
        {
            let entry = state
                .rejected_tokens
                .remove(index)
                .expect("the rejected token index came from this queue");
            let retry_after =
                AUTH_FAILURE_WINDOW.saturating_sub(now.saturating_duration_since(entry.1));
            state.rejected_tokens.push_back(entry);
            return Err(AuthLookupBlocked::RateLimited(retry_after));
        }
        if let Some(index) = state
            .accepted_tokens
            .iter()
            .position(|cached| cached.token_hash == token_hash)
        {
            let cached = state
                .accepted_tokens
                .remove(index)
                .expect("the accepted token index came from this queue");
            let permits = cached.permits.clone();
            state.accepted_tokens.push_back(cached);
            let Ok(permit) = permits.clone().try_acquire_owned() else {
                return Err(AuthLookupBlocked::AcceptedAtCapacity(permits));
            };
            return Ok(AuthLookupReservation {
                limiter: self.clone(),
                token_hash,
                _completion: None,
                _accepted_permit: Some(permit),
                guarded: false,
                active: true,
            });
        }
        if let Some(completion) = state.lookups_in_flight.get(&token_hash) {
            return Err(AuthLookupBlocked::Pending(completion.clone()));
        }
        if state.failures.len() + state.lookups_in_flight.len() >= MAX_AUTH_FAILURES_PER_MINUTE {
            return Err(AuthLookupBlocked::RateLimited(Self::retry_after(
                &state, now,
            )));
        }
        let (completion, receiver) = watch::channel(());
        state.lookups_in_flight.insert(token_hash, receiver);
        Ok(AuthLookupReservation {
            limiter: self.clone(),
            token_hash,
            _completion: Some(completion),
            _accepted_permit: None,
            guarded: true,
            active: true,
        })
    }

    fn prune(state: &mut AuthFailureState, now: Instant) {
        Self::prune_before(state, now.checked_sub(AUTH_FAILURE_WINDOW));
    }

    fn prune_before(state: &mut AuthFailureState, cutoff: Option<Instant>) {
        let Some(cutoff) = cutoff else {
            return;
        };
        while state
            .failures
            .front()
            .is_some_and(|failure| *failure <= cutoff)
        {
            state.failures.pop_front();
        }
        state
            .rejected_tokens
            .retain(|(_, rejected_at)| *rejected_at > cutoff);
    }

    fn remember_accepted(state: &mut AuthFailureState, token_hash: AuthTokenHash) {
        if let Some(index) = state
            .accepted_tokens
            .iter()
            .position(|cached| cached.token_hash == token_hash)
        {
            let cached = state
                .accepted_tokens
                .remove(index)
                .expect("the accepted token index came from this queue");
            state.accepted_tokens.push_back(cached);
            return;
        }
        if state.accepted_tokens.len() >= AUTH_ACCEPTED_TOKEN_CACHE_CAPACITY {
            let Some(index) = state.accepted_tokens.iter().position(|cached| {
                cached.permits.available_permits() == MAX_ACCEPTED_TOKEN_LOOKUPS_IN_FLIGHT
            }) else {
                return;
            };
            state.accepted_tokens.remove(index);
        }
        state.accepted_tokens.push_back(AcceptedToken {
            token_hash,
            permits: Arc::new(Semaphore::new(MAX_ACCEPTED_TOKEN_LOOKUPS_IN_FLIGHT)),
        });
    }

    fn reserve_waiting_accepted(
        &self,
        token_hash: AuthTokenHash,
        permits: &Arc<Semaphore>,
        permit: OwnedSemaphorePermit,
    ) -> Result<AuthLookupReservation, OwnedSemaphorePermit> {
        let state = self.state.lock().expect("auth failure lock poisoned");
        let still_accepted = state
            .accepted_tokens
            .iter()
            .any(|cached| cached.token_hash == token_hash && Arc::ptr_eq(&cached.permits, permits));
        drop(state);
        if !still_accepted {
            return Err(permit);
        }
        Ok(AuthLookupReservation {
            limiter: self.clone(),
            token_hash,
            _completion: None,
            _accepted_permit: Some(permit),
            guarded: false,
            active: true,
        })
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
    token_hash: AuthTokenHash,
    _completion: Option<watch::Sender<()>>,
    _accepted_permit: Option<OwnedSemaphorePermit>,
    guarded: bool,
    active: bool,
}

impl AuthLookupReservation {
    fn record_success(mut self) {
        let mut state = self
            .limiter
            .state
            .lock()
            .expect("auth failure lock poisoned");
        if self.guarded {
            state.lookups_in_flight.remove(&self.token_hash);
        }
        AuthFailureLimiter::remember_accepted(&mut state, self.token_hash);
        self.active = false;
    }

    fn record_failure(mut self) {
        let mut state = self
            .limiter
            .state
            .lock()
            .expect("auth failure lock poisoned");
        if self.guarded {
            state.lookups_in_flight.remove(&self.token_hash);
        }
        if let Some(index) = state
            .accepted_tokens
            .iter()
            .position(|cached| cached.token_hash == self.token_hash)
        {
            state.accepted_tokens.remove(index);
        }
        let now = Instant::now();
        if state.failures.len() < MAX_AUTH_FAILURES_PER_MINUTE {
            state.failures.push_back(now);
        }
        if let Some(index) = state
            .rejected_tokens
            .iter()
            .position(|(cached, _)| *cached == self.token_hash)
        {
            state.rejected_tokens.remove(index);
        }
        state.rejected_tokens.push_back((self.token_hash, now));
        if state.rejected_tokens.len() > AUTH_FAILURE_CACHE_CAPACITY {
            state.rejected_tokens.pop_front();
        }
        self.active = false;
    }
}

impl Drop for AuthLookupReservation {
    fn drop(&mut self) {
        if self.active && self.guarded {
            let mut state = self
                .limiter
                .state
                .lock()
                .expect("auth failure lock poisoned");
            state.lookups_in_flight.remove(&self.token_hash);
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
    presented: &str,
    lookup: Lookup,
) -> Result<GuardedMemberLookup<T, E>, Duration>
where
    Lookup: FnOnce() -> LookupFuture,
    LookupFuture: Future<Output = Result<Option<T>, E>>,
{
    let token_hash = AuthTokenHash::new(presented);
    let reservation = loop {
        match limiter.reserve_lookup(token_hash) {
            Ok(reservation) => break reservation,
            Err(AuthLookupBlocked::RateLimited(retry_after)) => return Err(retry_after),
            Err(AuthLookupBlocked::Pending(mut completion)) => {
                let _ = completion.changed().await;
            }
            Err(AuthLookupBlocked::AcceptedAtCapacity(permits)) => {
                let permit = permits
                    .clone()
                    .acquire_owned()
                    .await
                    .expect("accepted-token semaphore is never closed");
                match limiter.reserve_waiting_accepted(token_hash, &permits, permit) {
                    Ok(reservation) => break reservation,
                    Err(permit) => drop(permit),
                }
            }
        }
    };
    match lookup().await {
        Ok(Some(authenticated)) => {
            reservation.record_success();
            Ok(GuardedMemberLookup::Authenticated(authenticated))
        }
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
        return error_json(StatusCode::UNAUTHORIZED, "missing or invalid bearer token");
    };

    let (scope, rate_limit_key) = if is_bootstrap_token(presented, &state.bootstrap_token) {
        (TokenScope::Admin, TokenRateLimitKey::Bootstrap)
    } else {
        match lookup_member_with_guard(&state.auth_failure_limiter, presented, || {
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
    async fn valid_token_authenticates_mid_repeated_garbage_flood() {
        let limiter = AuthFailureLimiter::default();
        let lookups = AtomicUsize::new(0);

        let first = lookup_member_with_guard(&limiter, "garbage", || async {
            lookups.fetch_add(1, Ordering::SeqCst);
            Ok::<Option<()>, ()>(None)
        })
        .await;
        assert!(matches!(first, Ok(GuardedMemberLookup::Rejected)));

        for _ in 0..MAX_AUTH_FAILURES_PER_MINUTE * 2 {
            let repeated = lookup_member_with_guard(&limiter, "garbage", || async {
                lookups.fetch_add(1, Ordering::SeqCst);
                Ok::<Option<()>, ()>(None)
            })
            .await;
            assert!(repeated.is_err());
        }

        let valid = lookup_member_with_guard(&limiter, "valid-member-token", || async {
            lookups.fetch_add(1, Ordering::SeqCst);
            Ok::<Option<()>, ()>(Some(()))
        })
        .await;
        assert!(matches!(valid, Ok(GuardedMemberLookup::Authenticated(()))));
        assert_eq!(lookups.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn distinct_garbage_tokens_stop_database_work_at_the_global_ceiling() {
        let limiter = AuthFailureLimiter::default();
        let lookups = AtomicUsize::new(0);

        for index in 0..MAX_AUTH_FAILURES_PER_MINUTE {
            let token = format!("garbage-{index}");
            let result = lookup_member_with_guard(&limiter, &token, || async {
                lookups.fetch_add(1, Ordering::SeqCst);
                Ok::<Option<()>, ()>(None)
            })
            .await;
            assert!(matches!(result, Ok(GuardedMemberLookup::Rejected)));
        }

        let blocked = lookup_member_with_guard(&limiter, "one-too-many", || async {
            lookups.fetch_add(1, Ordering::SeqCst);
            Ok::<Option<()>, ()>(None)
        })
        .await;
        assert!(blocked.is_err());
        assert_eq!(lookups.load(Ordering::SeqCst), MAX_AUTH_FAILURES_PER_MINUTE);
    }

    #[tokio::test]
    async fn concurrent_valid_token_requests_are_not_treated_as_failures() {
        let limiter = AuthFailureLimiter::default();
        let lookups = Arc::new(AtomicUsize::new(0));
        let release = Arc::new(tokio::sync::Semaphore::new(0));
        let mut tasks = Vec::new();

        for _ in 0..3 {
            let limiter = limiter.clone();
            let lookups = lookups.clone();
            let release = release.clone();
            tasks.push(tokio::spawn(async move {
                lookup_member_with_guard(&limiter, "valid-member-token", || async move {
                    lookups.fetch_add(1, Ordering::SeqCst);
                    let permit = release.acquire().await.expect("test semaphore stays open");
                    drop(permit);
                    Ok::<Option<()>, ()>(Some(()))
                })
                .await
            }));
        }

        tokio::time::timeout(Duration::from_secs(1), async {
            while lookups.load(Ordering::SeqCst) < 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the first valid-token lookup starts");
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(lookups.load(Ordering::SeqCst), 1);
        release.add_permits(1);
        tokio::time::timeout(Duration::from_secs(1), async {
            while lookups.load(Ordering::SeqCst) < 3 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("waiting valid-token lookups start together after the first succeeds");
        release.add_permits(2);

        for task in tasks {
            assert!(matches!(
                task.await.expect("auth attempt task completes"),
                Ok(GuardedMemberLookup::Authenticated(()))
            ));
        }
    }

    #[tokio::test]
    async fn concurrent_repeated_garbage_uses_one_lookup_without_blocking_a_valid_token() {
        let limiter = AuthFailureLimiter::default();
        let lookups = Arc::new(AtomicUsize::new(0));
        let release = Arc::new(tokio::sync::Semaphore::new(0));

        let first_limiter = limiter.clone();
        let first_lookups = lookups.clone();
        let first_release = release.clone();
        let first_garbage = tokio::spawn(async move {
            lookup_member_with_guard(&first_limiter, "garbage", || async move {
                first_lookups.fetch_add(1, Ordering::SeqCst);
                let permit = first_release
                    .acquire()
                    .await
                    .expect("test semaphore stays open");
                drop(permit);
                Ok::<Option<()>, ()>(None)
            })
            .await
        });

        tokio::time::timeout(Duration::from_secs(1), async {
            while lookups.load(Ordering::SeqCst) < 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the first garbage lookup starts");

        let repeated_limiter = limiter.clone();
        let repeated_lookups = lookups.clone();
        let repeated_garbage = tokio::spawn(async move {
            lookup_member_with_guard(&repeated_limiter, "garbage", || async move {
                repeated_lookups.fetch_add(1, Ordering::SeqCst);
                Ok::<Option<()>, ()>(None)
            })
            .await
        });
        let valid_lookups = lookups.clone();
        let valid = tokio::spawn(async move {
            lookup_member_with_guard(&limiter, "valid-member-token", || async move {
                valid_lookups.fetch_add(1, Ordering::SeqCst);
                Ok::<Option<()>, ()>(Some(()))
            })
            .await
        });

        assert!(matches!(
            valid.await.expect("valid auth task completes"),
            Ok(GuardedMemberLookup::Authenticated(()))
        ));
        release.add_permits(1);
        assert!(matches!(
            first_garbage.await.expect("first garbage task completes"),
            Ok(GuardedMemberLookup::Rejected)
        ));
        assert!(
            repeated_garbage
                .await
                .expect("repeated garbage task completes")
                .is_err()
        );
        assert_eq!(lookups.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn revoked_accepted_token_has_bounded_revalidation_concurrency() {
        let limiter = AuthFailureLimiter::default();
        let warm = lookup_member_with_guard(&limiter, "formerly-valid", || async {
            Ok::<Option<()>, ()>(Some(()))
        })
        .await;
        assert!(matches!(warm, Ok(GuardedMemberLookup::Authenticated(()))));

        let lookups = Arc::new(AtomicUsize::new(0));
        let release = Arc::new(tokio::sync::Semaphore::new(0));
        let mut tasks = Vec::new();
        for _ in 0..MAX_ACCEPTED_TOKEN_LOOKUPS_IN_FLIGHT * 2 {
            let limiter = limiter.clone();
            let lookups = lookups.clone();
            let release = release.clone();
            tasks.push(tokio::spawn(async move {
                lookup_member_with_guard(&limiter, "formerly-valid", || async move {
                    lookups.fetch_add(1, Ordering::SeqCst);
                    let permit = release.acquire().await.expect("test semaphore stays open");
                    drop(permit);
                    Ok::<Option<()>, ()>(None)
                })
                .await
            }));
        }

        tokio::time::timeout(Duration::from_secs(1), async {
            while lookups.load(Ordering::SeqCst) < MAX_ACCEPTED_TOKEN_LOOKUPS_IN_FLIGHT {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the bounded accepted-token lookup set starts");
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(
            lookups.load(Ordering::SeqCst),
            MAX_ACCEPTED_TOKEN_LOOKUPS_IN_FLIGHT
        );

        release.add_permits(MAX_ACCEPTED_TOKEN_LOOKUPS_IN_FLIGHT);
        for task in tasks {
            let _ = task.await.expect("revoked-token auth task completes");
        }
        let blocked = lookup_member_with_guard(&limiter, "formerly-valid", || async {
            lookups.fetch_add(1, Ordering::SeqCst);
            Ok::<Option<()>, ()>(None)
        })
        .await;
        assert!(blocked.is_err());
        assert_eq!(
            lookups.load(Ordering::SeqCst),
            MAX_ACCEPTED_TOKEN_LOOKUPS_IN_FLIGHT
        );
    }

    #[tokio::test]
    async fn accepted_token_waiters_use_each_released_permit() {
        let limiter = AuthFailureLimiter::default();
        let warm = lookup_member_with_guard(&limiter, "busy-valid", || async {
            Ok::<Option<()>, ()>(Some(()))
        })
        .await;
        assert!(matches!(warm, Ok(GuardedMemberLookup::Authenticated(()))));

        let lookups = Arc::new(AtomicUsize::new(0));
        let release = Arc::new(tokio::sync::Semaphore::new(0));
        let task_count = MAX_ACCEPTED_TOKEN_LOOKUPS_IN_FLIGHT + 2;
        let mut tasks = Vec::new();
        for _ in 0..task_count {
            let limiter = limiter.clone();
            let lookups = lookups.clone();
            let release = release.clone();
            tasks.push(tokio::spawn(async move {
                lookup_member_with_guard(&limiter, "busy-valid", || async move {
                    lookups.fetch_add(1, Ordering::SeqCst);
                    let permit = release.acquire().await.expect("test semaphore stays open");
                    drop(permit);
                    Ok::<Option<()>, ()>(Some(()))
                })
                .await
            }));
        }

        wait_for_lookup_count(&lookups, MAX_ACCEPTED_TOKEN_LOOKUPS_IN_FLIGHT).await;
        release.add_permits(1);
        wait_for_lookup_count(&lookups, MAX_ACCEPTED_TOKEN_LOOKUPS_IN_FLIGHT + 1).await;
        release.add_permits(1);
        wait_for_lookup_count(&lookups, task_count).await;
        release.add_permits(MAX_ACCEPTED_TOKEN_LOOKUPS_IN_FLIGHT);

        for task in tasks {
            assert!(matches!(
                task.await.expect("accepted-token auth task completes"),
                Ok(GuardedMemberLookup::Authenticated(()))
            ));
        }
    }

    #[tokio::test]
    async fn concurrent_distinct_token_flood_bounds_lookup_starts_and_stored_state() {
        let limiter = AuthFailureLimiter::default();
        let lookups = Arc::new(AtomicUsize::new(0));
        let release = Arc::new(tokio::sync::Semaphore::new(0));
        let mut tasks = Vec::new();

        for index in 0..MAX_AUTH_FAILURES_PER_MINUTE * 2 {
            let limiter = limiter.clone();
            let lookups = lookups.clone();
            let release = release.clone();
            tasks.push(tokio::spawn(async move {
                let token = format!("concurrent-garbage-{index}");
                lookup_member_with_guard(&limiter, &token, || async move {
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
        assert!(state.lookups_in_flight.is_empty());
        assert_eq!(state.failures.len(), MAX_AUTH_FAILURES_PER_MINUTE);
        assert_eq!(state.rejected_tokens.len(), MAX_AUTH_FAILURES_PER_MINUTE);
    }

    #[test]
    fn unrepresentable_prune_cutoff_keeps_failures_cached() {
        let now = Instant::now();
        let token_hash = AuthTokenHash::new("garbage");
        let mut state = AuthFailureState::default();
        state.failures.push_back(now);
        state.rejected_tokens.push_back((token_hash, now));

        AuthFailureLimiter::prune_before(&mut state, None);

        assert_eq!(state.failures, VecDeque::from([now]));
        assert_eq!(state.rejected_tokens, VecDeque::from([(token_hash, now)]));
    }

    #[test]
    fn accepted_token_cache_does_not_evict_active_semaphores() {
        let active_hash = AuthTokenHash::new("active-token");
        let active_permits = Arc::new(Semaphore::new(MAX_ACCEPTED_TOKEN_LOOKUPS_IN_FLIGHT));
        let _active = active_permits
            .clone()
            .try_acquire_owned()
            .expect("fixture permit is available");
        let mut state = AuthFailureState::default();
        state.accepted_tokens.push_back(AcceptedToken {
            token_hash: active_hash,
            permits: active_permits,
        });
        for index in 1..AUTH_ACCEPTED_TOKEN_CACHE_CAPACITY {
            AuthFailureLimiter::remember_accepted(
                &mut state,
                AuthTokenHash::new(&format!("idle-token-{index}")),
            );
        }

        let new_hash = AuthTokenHash::new("new-token");
        AuthFailureLimiter::remember_accepted(&mut state, new_hash);

        assert_eq!(
            state.accepted_tokens.len(),
            AUTH_ACCEPTED_TOKEN_CACHE_CAPACITY
        );
        assert!(
            state
                .accepted_tokens
                .iter()
                .any(|cached| cached.token_hash == active_hash)
        );
        assert!(
            state
                .accepted_tokens
                .iter()
                .any(|cached| cached.token_hash == new_hash)
        );
    }

    #[test]
    fn bootstrap_token_check_is_independent_of_member_failure_state() {
        assert!(is_bootstrap_token("bootstrap-secret", "bootstrap-secret"));
    }

    async fn wait_for_lookup_count(lookups: &AtomicUsize, expected: usize) {
        tokio::time::timeout(Duration::from_secs(1), async {
            while lookups.load(Ordering::SeqCst) < expected {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the expected bounded lookup set starts");
    }
}
