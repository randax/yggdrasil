//! Typed graceful-shutdown broadcast shared by worker loops.

use tokio::sync::watch;
use tokio::time::Instant;

/// Why the process started draining workers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShutdownCause {
    /// An operator requested shutdown with SIGINT or SIGTERM.
    Signal,
    /// A supervised component stopped or returned an error.
    Failure,
}

/// The typed worker-drain request shared by every shutdown subscriber.
#[derive(Clone, Copy, Debug)]
pub struct ShutdownRequest {
    work_deadline: Instant,
    cause: ShutdownCause,
}

impl ShutdownRequest {
    /// The cutoff after which active work must release its lease.
    pub fn work_deadline(self) -> Instant {
        self.work_deadline
    }

    /// The event that initiated this process drain.
    pub fn cause(self) -> ShutdownCause {
        self.cause
    }
}

#[derive(Clone, Copy, Debug)]
enum ShutdownState {
    Running,
    Requested(ShutdownRequest),
}

/// Sender for the one process-wide worker shutdown request.
#[derive(Clone)]
pub struct ShutdownTrigger {
    sender: watch::Sender<ShutdownState>,
}

/// A clonable shutdown subscription for a worker or maintenance loop.
#[derive(Clone)]
pub struct Shutdown {
    receiver: watch::Receiver<ShutdownState>,
}

/// Create the typed broadcast used to stop all worker loops from one
/// process-wide shutdown request.
pub fn shutdown_channel() -> (ShutdownTrigger, Shutdown) {
    let (sender, receiver) = watch::channel(ShutdownState::Running);
    (ShutdownTrigger { sender }, Shutdown { receiver })
}

impl ShutdownTrigger {
    /// Stop new work and give active leased work until `work_deadline`
    /// to finish before it must return its claim to the queue.
    ///
    /// Returns whether this call installed the process-wide request.
    pub fn request(&self, work_deadline: Instant, cause: ShutdownCause) -> bool {
        self.sender.send_if_modified(|state| match state {
            ShutdownState::Running => {
                *state = ShutdownState::Requested(ShutdownRequest {
                    work_deadline,
                    cause,
                });
                true
            }
            ShutdownState::Requested(_) => false,
        })
    }
}

impl Shutdown {
    /// The process-wide request, if shutdown was already initiated.
    pub fn request(&self) -> Option<ShutdownRequest> {
        match *self.receiver.borrow() {
            ShutdownState::Running => None,
            ShutdownState::Requested(request) => Some(request),
        }
    }

    /// The active work cutoff, if shutdown was already requested.
    pub fn deadline(&self) -> Option<Instant> {
        self.request().map(ShutdownRequest::work_deadline)
    }

    /// Wait until shutdown is requested and return the shared typed request.
    pub async fn requested(&mut self) -> ShutdownRequest {
        loop {
            if let Some(request) = self.request() {
                return request;
            }
            if self.receiver.changed().await.is_err() {
                std::future::pending::<()>().await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn first_shutdown_request_preserves_its_typed_cause_and_deadline() {
        let (trigger, mut shutdown) = shutdown_channel();
        let signal_deadline = Instant::now() + Duration::from_secs(28);
        let failure_deadline = Instant::now() + Duration::from_secs(2);

        assert!(trigger.request(signal_deadline, ShutdownCause::Signal));
        assert!(!trigger.request(failure_deadline, ShutdownCause::Failure));

        let request = shutdown.requested().await;
        assert_eq!(request.cause(), ShutdownCause::Signal);
        assert_eq!(request.work_deadline(), signal_deadline);
    }
}
