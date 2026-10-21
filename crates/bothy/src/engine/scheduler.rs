//! Background sync: debounced after local writes, periodic for pulls, backoff on errors.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{oneshot, watch};
use tokio::time::{Instant, sleep_until};

use crate::database::{Inner, SyncReport};
use crate::error::ErrorKind;

/// Timing of background sync.
#[derive(Clone, Copy, Debug)]
pub struct AutoSync {
    /// Quiet time after a local write before syncing.
    pub debounce: Duration,
    /// Longest wait after the first unsynced local write, even if writes keep coming.
    pub max_delay: Duration,
    /// Pull interval when nothing happens locally.
    pub poll_interval: Duration,
    /// Cap of the exponential backoff after failures.
    pub max_backoff: Duration,
}

impl Default for AutoSync {
    fn default() -> Self {
        Self {
            debounce: Duration::from_secs(2),
            max_delay: Duration::from_secs(10),
            poll_interval: Duration::from_secs(30),
            max_backoff: Duration::from_secs(15 * 60),
        }
    }
}

impl AutoSync {
    /// Settings for background or embedded use: 5-minute polling.
    pub fn background() -> Self {
        Self {
            poll_interval: Duration::from_secs(5 * 60),
            ..Self::default()
        }
    }
}

/// Outcome of the latest background sync.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SyncStatus {
    /// No sync has finished yet.
    Idle,
    /// The last sync succeeded.
    Ok(SyncReport),
    /// The last sync failed; it will be retried after a backoff.
    Failed {
        /// Error category.
        kind: ErrorKind,
        /// Error message.
        message: String,
    },
}

/// Controls a background sync task. Dropping it stops the task.
#[derive(Debug)]
pub struct SyncHandle {
    stop: Option<oneshot::Sender<()>>,
    status: watch::Receiver<SyncStatus>,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl SyncHandle {
    /// Latest status. Use [`watch::Receiver::changed`] on [`SyncHandle::watch`] to wait for updates.
    pub fn status(&self) -> SyncStatus {
        self.status.borrow().clone()
    }

    /// A receiver that is notified after every background sync.
    pub fn watch(&self) -> watch::Receiver<SyncStatus> {
        self.status.clone()
    }

    /// Stops the task and waits for a running sync to finish.
    pub async fn stop(mut self) {
        if let Some(tx) = self.stop.take() {
            let _ = tx.send(());
        }
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
}

impl Drop for SyncHandle {
    fn drop(&mut self) {
        if let Some(tx) = self.stop.take() {
            let _ = tx.send(());
        }
    }
}

pub(crate) fn start(inner: Arc<Inner>, opts: AutoSync) -> SyncHandle {
    let (stop_tx, mut stop_rx) = oneshot::channel();
    let (status_tx, status_rx) = watch::channel(SyncStatus::Idle);
    let task = tokio::spawn(async move {
        let mut backoff = Duration::ZERO;
        let mut next_poll = Instant::now();
        loop {
            // Wait for a local write, the poll timer or a retry after a failure.
            let local = tokio::select! {
                _ = &mut stop_rx => return,
                _ = inner.poke.notified() => true,
                _ = sleep_until(next_poll) => false,
            };
            if local {
                let deadline = Instant::now() + opts.max_delay;
                loop {
                    let quiet = Instant::now() + opts.debounce;
                    tokio::select! {
                        _ = &mut stop_rx => return,
                        _ = inner.poke.notified() => {
                            if Instant::now() >= deadline { break; }
                        }
                        _ = sleep_until(quiet.min(deadline)) => break,
                    }
                }
            }
            let status = match crate::engine::sync(&inner).await {
                Ok(report) => {
                    backoff = Duration::ZERO;
                    SyncStatus::Ok(report)
                }
                Err(e) => {
                    backoff = (backoff * 2).clamp(Duration::from_secs(1), opts.max_backoff);
                    SyncStatus::Failed {
                        kind: e.kind(),
                        message: e.message().to_owned(),
                    }
                }
            };
            let _ = status_tx.send(status);
            // After a failure the next attempt waits for the backoff, not the poll interval.
            next_poll = Instant::now()
                + if backoff.is_zero() {
                    opts.poll_interval
                } else {
                    backoff
                };
        }
    });
    SyncHandle {
        stop: Some(stop_tx),
        status: status_rx,
        task: Some(task),
    }
}
