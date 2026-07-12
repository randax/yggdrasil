//! The per-forge poll rate budget.

use std::time::{Duration, Instant};

/// How long a forge stays cooled down after it signals a rate limit or
/// abuse detection: no poll spends a request against it until this
/// passes (the bucket withholds tokens), and the repo that tripped it is
/// rescheduled past the cooldown. Generous — a forge that is pushing
/// back wants real breathing room, and poll is best-effort.
pub(crate) const RATE_LIMIT_COOLDOWN: Duration = Duration::from_secs(5 * 60);

/// A per-forge token bucket bounding how often the poll loop spends a
/// conditional request against one forge (RFC 0001 §2–3 rate budget).
/// Tokens refill continuously at the forge's configured rate and a poll
/// takes one; a rate-limit or abuse signal drops the forge into a
/// cooldown that withholds tokens until it passes. Driven by an explicit
/// monotonic clock so the schedule is testable without real time.
pub(crate) struct TokenBucket {
    /// Maximum tokens held — one minute's budget, the opening burst.
    capacity: f64,
    /// Tokens regained per second (the per-minute budget over 60).
    refill_per_sec: f64,
    /// Tokens available as of `last`.
    tokens: f64,
    /// When `tokens` was last reconciled against the clock.
    last: Instant,
    /// While set and still in the future, no token is granted however
    /// full the bucket — the forge asked us to back off.
    cooldown_until: Option<Instant>,
}

impl TokenBucket {
    /// A bucket for `rate_per_minute` conditional requests a minute,
    /// starting full so a freshly seen forge polls immediately.
    pub(crate) fn per_minute(rate_per_minute: i32, now: Instant) -> Self {
        let capacity = rate_per_minute.max(1) as f64;
        Self {
            capacity,
            refill_per_sec: capacity / 60.0,
            tokens: capacity,
            last: now,
            cooldown_until: None,
        }
    }

    /// Reconcile `tokens` with the clock: add what has refilled since
    /// `last`, capped at capacity.
    fn refill(&mut self, now: Instant) {
        let elapsed = now.saturating_duration_since(self.last).as_secs_f64();
        self.tokens = (self.tokens + elapsed * self.refill_per_sec).min(self.capacity);
        self.last = now;
    }

    /// Apply a new per-minute rate from the control plane without
    /// restarting the worker. Existing tokens are first reconciled under
    /// the previous rate, then capped to the new capacity.
    pub(crate) fn update_rate(&mut self, rate_per_minute: i32, now: Instant) {
        let capacity = rate_per_minute.max(1) as f64;
        if self.capacity == capacity {
            return;
        }
        self.refill(now);
        self.capacity = capacity;
        self.refill_per_sec = capacity / 60.0;
        self.tokens = self.tokens.min(capacity);
    }

    /// Spend one token if one is available and the forge is not cooling
    /// down. Returns whether the poll may proceed.
    pub(crate) fn try_take(&mut self, now: Instant) -> bool {
        self.refill(now);
        if self.cooling_down(now) {
            return false;
        }
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }

    /// Back the forge off until at least `until` (extending, never
    /// shortening, an existing cooldown).
    pub(crate) fn cooldown(&mut self, until: Instant) {
        self.cooldown_until = Some(match self.cooldown_until {
            Some(existing) if existing > until => existing,
            _ => until,
        });
    }

    fn cooling_down(&self, now: Instant) -> bool {
        self.cooldown_until.is_some_and(|until| now < until)
    }

    /// How long until this bucket would next grant a token: the longer of
    /// the remaining cooldown and the time to refill one token. The repo
    /// that was denied is rescheduled by this, so it retries no sooner.
    pub(crate) fn retry_after(&self, now: Instant) -> Duration {
        let refill_wait = if self.tokens >= 1.0 {
            0.0
        } else {
            (1.0 - self.tokens) / self.refill_per_sec
        };
        let cooldown_wait = self
            .cooldown_until
            .map(|until| until.saturating_duration_since(now).as_secs_f64())
            .unwrap_or(0.0);
        Duration::from_secs_f64(refill_wait.max(cooldown_wait))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_bucket_grants_a_full_burst_then_refills_over_time() {
        let t0 = Instant::now();
        // 60 requests/min: a 60-token burst, refilling one token per second.
        let mut bucket = TokenBucket::per_minute(60, t0);
        for i in 0..60 {
            assert!(bucket.try_take(t0), "take {i} is within the opening burst");
        }
        assert!(
            !bucket.try_take(t0),
            "the bucket is empty once the burst is spent"
        );

        // A second later, exactly one token has refilled.
        let t1 = t0 + Duration::from_secs(1);
        assert!(bucket.try_take(t1), "one token refills after a second");
        assert!(!bucket.try_take(t1), "but only one");
    }

    #[test]
    fn token_bucket_never_exceeds_its_capacity() {
        let t0 = Instant::now();
        let mut bucket = TokenBucket::per_minute(60, t0);
        // Idle for an hour: refill is capped at capacity, not unbounded.
        let later = t0 + Duration::from_secs(3600);
        for i in 0..60 {
            assert!(bucket.try_take(later), "take {i} from a capped-full bucket");
        }
        assert!(
            !bucket.try_take(later),
            "a long idle must not bank more than one burst's worth of tokens"
        );
    }

    #[test]
    fn token_bucket_applies_rate_budget_changes_without_restart() {
        let t0 = Instant::now();
        let mut bucket = TokenBucket::per_minute(60, t0);
        for _ in 0..60 {
            assert!(bucket.try_take(t0));
        }
        assert!(!bucket.try_take(t0), "the original budget is empty");

        bucket.update_rate(120, t0 + Duration::from_secs(120));
        for i in 0..60 {
            assert!(
                bucket.try_take(t0 + Duration::from_secs(120)),
                "new capacity grants token {i}"
            );
        }
        assert!(
            !bucket.try_take(t0 + Duration::from_secs(120)),
            "tokens are capped to the new capacity"
        );

        bucket.update_rate(1, t0 + Duration::from_secs(180));
        assert!(bucket.try_take(t0 + Duration::from_secs(180)));
        assert!(
            !bucket.try_take(t0 + Duration::from_secs(180)),
            "lowering the budget caps already-held tokens"
        );
    }

    #[test]
    fn token_bucket_withholds_tokens_during_a_cooldown() {
        let t0 = Instant::now();
        let mut bucket = TokenBucket::per_minute(60, t0);
        assert!(bucket.try_take(t0), "a fresh bucket grants");

        // A rate-limit/abuse signal cools the whole forge down for 30s —
        // no token is granted meanwhile, however full the bucket.
        bucket.cooldown(t0 + Duration::from_secs(30));
        assert!(
            !bucket.try_take(t0 + Duration::from_secs(10)),
            "no token is granted mid-cooldown"
        );
        assert!(
            bucket.retry_after(t0 + Duration::from_secs(10)) >= Duration::from_secs(19),
            "retry_after reflects the remaining cooldown"
        );
        assert!(
            bucket.try_take(t0 + Duration::from_secs(31)),
            "tokens flow again once the cooldown passes"
        );
    }
}
