//! Cancellation token shared by the loop, providers, and tools.
//!
//! Lightweight cloneable flag built on `event-listener`: cloning shares the
//! state, dropping a clone is safe at every await point, and `cancelled()`
//! waits for the flag. Timers are runtime-agnostic via `futures-timer`.

use crate::error::HarnessError;
use event_listener::Event;
use futures_timer::Delay;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

#[derive(Default)]
pub struct CancelToken {
    inner: Arc<Inner>,
}

#[derive(Default)]
struct Inner {
    cancelled: AtomicBool,
    event: Event,
}

impl Clone for CancelToken {
    fn clone(&self) -> Self {
        CancelToken {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl std::fmt::Debug for CancelToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CancelToken")
            .field("cancelled", &self.is_cancelled())
            .finish()
    }
}

impl CancelToken {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_cancelled(&self) -> bool {
        self.inner.cancelled.load(Ordering::Acquire)
    }

    /// Trigger cancellation and wake every current and future waiter.
    pub fn cancel(&self) {
        self.inner.cancelled.store(true, Ordering::Release);
        self.inner.event.notify(usize::MAX);
    }

    /// Fail fast if already cancelled.
    pub fn check(&self) -> Result<(), HarnessError> {
        if self.is_cancelled() {
            Err(HarnessError::Cancelled)
        } else {
            Ok(())
        }
    }
    /// Wait until cancelled; resolves immediately when already set.
    /// Returns a pinned future so callers can `select` it without Unpin.
    pub fn cancelled(&self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        let this = CancelToken {
            inner: Arc::clone(&self.inner),
        };
        Box::pin(async move {
            loop {
                if this.is_cancelled() {
                    return;
                }
                let listener = this.inner.event.listen();
                if this.is_cancelled() {
                    return;
                }
                listener.await;
            }
        })
    }

    /// Race cancellation against a delay; `Ok(())` on timeout, error when cancelled.
    pub async fn race_delay(&self, d: Duration) -> Result<(), HarnessError> {
        let delay = Delay::new(d);
        futures::pin_mut!(delay);
        match futures::future::select(delay, self.cancelled()).await {
            futures::future::Either::Left(_) => Ok(()),
            futures::future::Either::Right(_) => Err(HarnessError::Cancelled),
        }
    }
}

/// Cancellable backoff sleep for retry loops.
pub async fn backoff_sleep(token: &CancelToken, d: Duration) -> Result<(), HarnessError> {
    token.race_delay(d).await
}
