//! One server-wide bound around every `Engine::search` execution.

use std::future::Future;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Semaphore;

const SEARCH_EXECUTION_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug)]
pub(crate) struct SearchExecutionTimeout;

#[derive(Clone)]
pub(crate) struct SearchLimiter {
    permits: Arc<Semaphore>,
}

impl SearchLimiter {
    pub(crate) fn new(limit: NonZeroUsize) -> Self {
        Self {
            // Semaphore::new panics above MAX_PERMITS; an accepted config
            // must not crash the boot, so clamp with a warning instead.
            permits: Arc::new(Semaphore::new(if limit.get() > Semaphore::MAX_PERMITS {
                tracing::warn!(
                    configured = limit.get(),
                    clamped = Semaphore::MAX_PERMITS,
                    "search concurrency clamped to the runtime maximum"
                );
                Semaphore::MAX_PERMITS
            } else {
                limit.get()
            })),
        }
    }

    pub(crate) async fn run<T, Work, WorkFuture>(
        &self,
        work: Work,
    ) -> Result<T, SearchExecutionTimeout>
    where
        T: Send + 'static,
        Work: FnOnce() -> WorkFuture + Send + 'static,
        WorkFuture: Future<Output = T> + Send + 'static,
    {
        self.run_with_execution_timeout(SEARCH_EXECUTION_TIMEOUT, work)
            .await
    }

    async fn run_with_execution_timeout<T, Work, WorkFuture>(
        &self,
        execution_timeout: Duration,
        work: Work,
    ) -> Result<T, SearchExecutionTimeout>
    where
        T: Send + 'static,
        Work: FnOnce() -> WorkFuture + Send + 'static,
        WorkFuture: Future<Output = T> + Send + 'static,
    {
        let permit = self
            .permits
            .clone()
            .acquire_owned()
            .await
            .expect("the search semaphore is never closed");
        tokio::spawn(async move {
            let _permit = permit;
            match tokio::time::timeout(execution_timeout, work()).await {
                Ok(result) => Ok(result),
                Err(_) => {
                    tracing::warn!(
                        ?execution_timeout,
                        "search execution timed out; releasing concurrency permit"
                    );
                    Err(SearchExecutionTimeout)
                }
            }
        })
        .await
        .expect("limited search task must not panic")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test]
    async fn concurrent_search_work_never_exceeds_the_named_bound() {
        let limiter = SearchLimiter::new(NonZeroUsize::new(2).expect("two is nonzero"));
        let active = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let mut tasks = Vec::new();

        for _ in 0..8 {
            let limiter = limiter.clone();
            let active = active.clone();
            let peak = peak.clone();
            tasks.push(tokio::spawn(async move {
                limiter
                    .run(move || async move {
                        let current = active.fetch_add(1, Ordering::SeqCst) + 1;
                        peak.fetch_max(current, Ordering::SeqCst);
                        tokio::time::sleep(Duration::from_millis(10)).await;
                        active.fetch_sub(1, Ordering::SeqCst);
                    })
                    .await
                    .expect("search execution stays within timeout");
            }));
        }
        for task in tasks {
            task.await.expect("search task completes");
        }

        assert_eq!(peak.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn cancelled_waiter_does_not_release_started_work_capacity() {
        let limiter =
            SearchLimiter::new(std::num::NonZeroUsize::new(1).expect("fixture limit is nonzero"));
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (finish_tx, finish_rx) = tokio::sync::oneshot::channel();
        let first_limiter = limiter.clone();
        let first = tokio::spawn(async move {
            first_limiter
                .run(move || async move {
                    let _ = started_tx.send(());
                    let _ = finish_rx.await;
                })
                .await
                .expect("search execution stays within timeout");
        });
        started_rx.await.expect("first work started");
        first.abort();

        let second_started = Arc::new(AtomicUsize::new(0));
        let second_counter = second_started.clone();
        let second = tokio::spawn(async move {
            limiter
                .run(move || async move {
                    second_counter.fetch_add(1, Ordering::SeqCst);
                })
                .await
                .expect("search execution stays within timeout");
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(second_started.load(Ordering::SeqCst), 0);

        finish_tx.send(()).expect("first work is still running");
        second.await.expect("replacement work completes");
        assert_eq!(second_started.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn timed_out_detached_work_releases_its_permit() {
        let limiter = SearchLimiter::new(NonZeroUsize::new(1).expect("one is nonzero"));
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let first_limiter = limiter.clone();
        let first = tokio::spawn(async move {
            first_limiter
                .run_with_execution_timeout(Duration::from_millis(10), move || async move {
                    let _ = started_tx.send(());
                    std::future::pending::<()>().await;
                })
                .await
        });
        started_rx.await.expect("first work started");
        first.abort();

        tokio::time::timeout(Duration::from_millis(100), async {
            limiter
                .run(|| async {})
                .await
                .expect("replacement work completes");
        })
        .await
        .expect("timed-out work releases its permit");
    }
}
