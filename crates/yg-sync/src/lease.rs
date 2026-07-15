//! Lease heartbeating for long-running leased work.

use std::time::Duration;

use crate::shutdown::Shutdown;

/// Result of shutdown-aware leased work.
pub enum LeaseShutdown<T> {
    /// Work finished while the lease was still held and may be settled.
    Finished(T),
    /// The cutoff elapsed, the lease was returned, and cleanup finished.
    Released,
}

/// Drive long-running leased work while heartbeating its lease: one
/// fenced renewal every third of the lease, each attempt bounded by
/// that same period so a wedged control-plane connection never stalls
/// the work's supervision. The work always runs to completion; the
/// heartbeat only decides whether the lease is still ours when the
/// caller settles:
///
/// - a renewal *error or timeout* is retried at the next tick —
///   settlement is fenced anyway, so a control-plane blip must not
///   doom an hours-long clone that is still making progress;
/// - a *fenced* renewal (the job was reclaimed — or our own renewal
///   committed but its response was lost, leaving us with the stale
///   token) stops the heartbeat and lets the work finish. The run is
///   already lost to the fenced settle, and cancelling it here would
///   be worse: the index path's blocking sections (`git archive | tar`,
///   the parse) outlive a dropped future, which would release the
///   mirror lock and delete the scratch checkout under still-running
///   subprocesses.
pub async fn with_lease_heartbeat<T>(
    lease: Duration,
    renew: impl AsyncFn() -> anyhow::Result<bool>,
    work: impl Future<Output = T>,
) -> T {
    // Guard the degenerate lease: a zero period would panic interval_at.
    let period = (lease / 3).max(Duration::from_millis(1));
    let start = tokio::time::Instant::now() + period;
    let mut ticks = tokio::time::interval_at(start, period);
    ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut work = std::pin::pin!(work);
    let mut held = true;
    loop {
        // Between heartbeats there is nothing but the work to drive.
        tokio::select! {
            output = &mut work => return output,
            _ = ticks.tick(), if held => {}
        }
        // A heartbeat is due. The renewal runs concurrently with the
        // work — an in-flight renewal must not stop the work being
        // polled (async git IO stalls when its future sits unpolled) —
        // and each attempt is bounded by the tick period, so a wedged
        // control-plane connection surfaces as a retried timeout, never
        // a stall.
        let mut renewal = std::pin::pin!(tokio::time::timeout(period, renew()));
        let output = tokio::select! {
            output = &mut work => output,
            outcome = &mut renewal => {
                match outcome {
                    Ok(Ok(true)) => {}
                    Ok(Ok(false)) => {
                        held = false;
                        tracing::warn!(
                            "lease renewal fenced — the job was reclaimed; the fenced settle will discard this run"
                        );
                    }
                    Ok(Err(e)) => {
                        tracing::warn!(error = format!("{e:#}"), "lease renewal failed; retrying at the next heartbeat");
                    }
                    Err(_) => {
                        tracing::warn!("lease renewal timed out; retrying at the next heartbeat");
                    }
                }
                continue;
            }
        };
        // The work landed mid-renewal. Drain the attempt (still bounded
        // by its timeout) before returning: dropped here it could commit
        // server-side after the settle reads the token, fencing this
        // worker's own result.
        let _ = renewal.await;
        return output;
    }
}

/// Drive leased work with heartbeats until it finishes or shutdown's
/// work cutoff elapses. At shutdown, no new renewal is started. Any
/// renewal already in flight is drained before the fenced release, so
/// the release always uses the latest known token. After releasing, the
/// work is still driven to completion: index blocking tasks may outlive
/// a dropped future and must retain their mirror lock and scratch tree.
pub async fn with_lease_heartbeat_until_shutdown<T>(
    lease: Duration,
    renew: impl AsyncFn() -> anyhow::Result<bool>,
    release: impl AsyncFn() -> anyhow::Result<bool>,
    mut shutdown: Shutdown,
    work: impl Future<Output = T>,
) -> anyhow::Result<LeaseShutdown<T>> {
    let period = (lease / 3).max(Duration::from_millis(1));
    let start = tokio::time::Instant::now() + period;
    let mut ticks = tokio::time::interval_at(start, period);
    ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut work = std::pin::pin!(work);
    let mut held = true;
    loop {
        tokio::select! {
            output = &mut work => return Ok(LeaseShutdown::Finished(output)),
            deadline = shutdown.requested() => {
                return finish_or_release(deadline, work.as_mut(), &release).await;
            }
            _ = ticks.tick(), if held => {}
        }

        let mut renewal = std::pin::pin!(tokio::time::timeout(period, renew()));
        tokio::select! {
            output = &mut work => {
                let _ = renewal.await;
                return Ok(LeaseShutdown::Finished(output));
            }
            deadline = shutdown.requested() => {
                // The token may change when this completes. Drain it
                // before release even if the work cutoff passes; the CLI
                // owns the separate hard termination deadline.
                let output = tokio::select! {
                    output = &mut work => Some(output),
                    _ = &mut renewal => None,
                };
                if let Some(output) = output {
                    let _ = renewal.await;
                    return Ok(LeaseShutdown::Finished(output));
                }
                return finish_or_release(deadline, work.as_mut(), &release).await;
            }
            outcome = &mut renewal => {
                match outcome {
                    Ok(Ok(true)) => {}
                    Ok(Ok(false)) => {
                        held = false;
                        tracing::warn!(
                            "lease renewal fenced — the job was reclaimed; the fenced settle will discard this run"
                        );
                    }
                    Ok(Err(e)) => {
                        tracing::warn!(error = format!("{e:#}"), "lease renewal failed; retrying at the next heartbeat");
                    }
                    Err(_) => {
                        tracing::warn!("lease renewal timed out; retrying at the next heartbeat");
                    }
                }
            }
        }
    }
}

async fn finish_or_release<T>(
    deadline: tokio::time::Instant,
    work: std::pin::Pin<&mut impl Future<Output = T>>,
    release: &impl AsyncFn() -> anyhow::Result<bool>,
) -> anyhow::Result<LeaseShutdown<T>> {
    let mut work = work;
    match tokio::time::timeout_at(deadline, &mut work).await {
        Ok(output) => Ok(LeaseShutdown::Finished(output)),
        Err(_) => {
            let released = release().await?;
            tracing::info!(released, "released unfinished lease for shutdown");
            let _ = work.await;
            Ok(LeaseShutdown::Released)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    /// Paused-time heartbeat harness: work that "runs" for `busy` while
    /// each renewal answers from `answers` in order (sticking on the
    /// last), counting calls into `renewals`.
    async fn heartbeat_with(
        lease: Duration,
        busy: Duration,
        answers: &[anyhow::Result<bool>],
        renewals: &std::sync::atomic::AtomicUsize,
    ) -> &'static str {
        let renew = async || {
            let n = renewals.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            match answers[n.min(answers.len() - 1)] {
                Ok(held) => Ok(held),
                Err(ref e) => Err(anyhow::anyhow!("{e:#}")),
            }
        };
        with_lease_heartbeat(lease, renew, async move {
            tokio::time::sleep(busy).await;
            "synced"
        })
        .await
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_releases_at_cutoff_but_drives_work_cleanup_to_completion() {
        let released = std::sync::Arc::new(AtomicUsize::new(0));
        let cleaned_up = std::sync::Arc::new(AtomicBool::new(false));
        let (trigger, shutdown) = crate::shutdown_channel();
        let released_in_task = released.clone();
        let cleanup_in_task = cleaned_up.clone();
        let task = tokio::spawn(with_lease_heartbeat_until_shutdown(
            Duration::from_secs(60),
            async || Ok(true),
            async move || {
                released_in_task.fetch_add(1, Ordering::SeqCst);
                Ok(true)
            },
            shutdown,
            async move {
                tokio::time::sleep(Duration::from_secs(20)).await;
                cleanup_in_task.store(true, Ordering::SeqCst);
            },
        ));
        tokio::task::yield_now().await;
        trigger.request(tokio::time::Instant::now() + Duration::from_secs(5));

        tokio::time::advance(Duration::from_secs(5)).await;
        tokio::task::yield_now().await;
        assert_eq!(released.load(Ordering::SeqCst), 1);
        assert!(!cleaned_up.load(Ordering::SeqCst));

        tokio::time::advance(Duration::from_secs(15)).await;
        assert!(matches!(
            task.await.unwrap().unwrap(),
            LeaseShutdown::Released
        ));
        assert!(cleaned_up.load(Ordering::SeqCst));
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_drains_an_in_flight_renewal_before_releasing() {
        let renewed = std::sync::Arc::new(AtomicBool::new(false));
        let release_saw_renewal = std::sync::Arc::new(AtomicBool::new(false));
        let renewal_started = std::sync::Arc::new(tokio::sync::Notify::new());
        let (trigger, shutdown) = crate::shutdown_channel();
        let renewed_in_task = renewed.clone();
        let renewed_at_release = renewed.clone();
        let release_saw_renewal_in_task = release_saw_renewal.clone();
        let renewal_started_in_task = renewal_started.clone();
        let task = tokio::spawn(with_lease_heartbeat_until_shutdown(
            Duration::from_secs(9),
            async move || {
                renewal_started_in_task.notify_one();
                tokio::time::sleep(Duration::from_secs(2)).await;
                renewed_in_task.store(true, Ordering::SeqCst);
                Ok(true)
            },
            async move || {
                release_saw_renewal_in_task
                    .store(renewed_at_release.load(Ordering::SeqCst), Ordering::SeqCst);
                Ok(true)
            },
            shutdown,
            async { tokio::time::sleep(Duration::from_secs(10)).await },
        ));
        renewal_started.notified().await;
        trigger.request(tokio::time::Instant::now() + Duration::from_secs(1));

        tokio::time::advance(Duration::from_secs(1)).await;
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(1)).await;
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(5)).await;
        assert!(matches!(
            task.await.unwrap().unwrap(),
            LeaseShutdown::Released
        ));
        assert!(release_saw_renewal.load(Ordering::SeqCst));
    }

    #[tokio::test(start_paused = true)]
    async fn heartbeat_lets_work_that_outlives_the_lease_finish() {
        let renewals = std::sync::atomic::AtomicUsize::new(0);
        let lease = Duration::from_secs(60);
        // Three lease-lengths of work: without renewals the job would
        // have been reclaimed long before the fetch lands.
        let out = heartbeat_with(lease, lease * 3, &[Ok(true)], &renewals).await;
        assert_eq!(out, "synced");
        assert!(
            renewals.load(std::sync::atomic::Ordering::SeqCst) >= 3,
            "the lease must have been renewed while the work ran"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn heartbeat_does_not_renew_for_work_that_finishes_early() {
        let renewals = std::sync::atomic::AtomicUsize::new(0);
        let lease = Duration::from_secs(60);
        let out = heartbeat_with(lease, Duration::from_secs(1), &[Ok(true)], &renewals).await;
        assert_eq!(out, "synced");
        assert_eq!(
            renewals.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "work inside the base lease needs no heartbeat"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_fenced_renewal_stops_the_heartbeat_but_lets_the_work_finish() {
        let renewals = std::sync::atomic::AtomicUsize::new(0);
        let lease = Duration::from_secs(60);
        // Nine tick periods of work: the first renewal is fenced, so the
        // remaining eight must never fire — cancelling mid-run would
        // orphan blocking subprocesses, so the work runs out its clock
        // and the (fenced) settle discards the result instead.
        let out = heartbeat_with(lease, lease * 3, &[Ok(false)], &renewals).await;
        assert_eq!(out, "synced", "fenced work must still run to completion");
        assert_eq!(
            renewals.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the fenced heartbeat must be the last"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_renewal_error_is_retried_not_fatal() {
        let renewals = std::sync::atomic::AtomicUsize::new(0);
        let lease = Duration::from_secs(60);
        // The control plane blips on the first heartbeat, then recovers:
        // the work must survive to completion.
        let out = heartbeat_with(
            lease,
            lease * 2,
            &[Err(anyhow::anyhow!("connection refused")), Ok(true)],
            &renewals,
        )
        .await;
        assert_eq!(out, "synced");
        assert!(renewals.load(std::sync::atomic::Ordering::SeqCst) >= 2);
    }

    #[tokio::test(start_paused = true)]
    async fn work_landing_mid_renewal_drains_the_attempt_before_settling() {
        let completed = std::sync::atomic::AtomicUsize::new(0);
        let lease = Duration::from_secs(60);
        // The renewal is still in flight when the work finishes one
        // second after the first tick. Dropped mid-flight it could
        // commit after the settle reads the token and fence this
        // worker's own result — so it must be driven to completion.
        let renew = async || {
            tokio::time::sleep(Duration::from_secs(5)).await;
            completed.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(true)
        };
        let out = with_lease_heartbeat(lease, renew, async {
            tokio::time::sleep(Duration::from_secs(21)).await;
            "synced"
        })
        .await;
        assert_eq!(out, "synced");
        assert_eq!(
            completed.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the in-flight renewal must land before the caller settles"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_hung_renewal_times_out_instead_of_stalling_the_work() {
        let lease = Duration::from_secs(60);
        // Every renewal black-holes (a dead control-plane connection
        // that never errors): each attempt must be cut off at the tick
        // period so the work is polled again and can finish.
        let renew = async || std::future::pending::<anyhow::Result<bool>>().await;
        let out = with_lease_heartbeat(lease, renew, async {
            tokio::time::sleep(lease * 2).await;
            "synced"
        })
        .await;
        assert_eq!(out, "synced", "a wedged renewal must not stall the work");
    }
}
