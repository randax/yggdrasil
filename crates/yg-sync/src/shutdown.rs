//! Typed graceful-shutdown broadcast shared by worker loops.

use tokio::sync::watch;
use tokio::time::Instant;

#[derive(Clone, Copy, Debug)]
enum ShutdownState {
    Running,
    Requested { work_deadline: Instant },
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

/// Create the typed broadcast used to stop all worker loops off one
/// process signal.
pub fn shutdown_channel() -> (ShutdownTrigger, Shutdown) {
    let (sender, receiver) = watch::channel(ShutdownState::Running);
    (ShutdownTrigger { sender }, Shutdown { receiver })
}

impl ShutdownTrigger {
    /// Stop new work and give active leased work until `work_deadline`
    /// to finish before it must return its claim to the queue.
    pub fn request(&self, work_deadline: Instant) {
        self.sender.send_if_modified(|state| match state {
            ShutdownState::Running => {
                *state = ShutdownState::Requested { work_deadline };
                true
            }
            ShutdownState::Requested { .. } => false,
        });
    }
}

impl Shutdown {
    /// The active work cutoff, if shutdown was already requested.
    pub fn deadline(&self) -> Option<Instant> {
        match *self.receiver.borrow() {
            ShutdownState::Running => None,
            ShutdownState::Requested { work_deadline } => Some(work_deadline),
        }
    }

    /// Wait until shutdown is requested and return its shared cutoff.
    pub async fn requested(&mut self) -> Instant {
        loop {
            if let Some(deadline) = self.deadline() {
                return deadline;
            }
            if self.receiver.changed().await.is_err() {
                std::future::pending::<()>().await;
            }
        }
    }
}
