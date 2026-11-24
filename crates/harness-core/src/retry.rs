use std::time::Duration;

use crate::control::CancellationToken;
use crate::model::BoxFuture;

/// Host-provided asynchronous timer. It must wake its future when the delay ends.
pub trait RetryTimer: Send + Sync {
    fn sleep(&self, duration: Duration) -> BoxFuture<'_, ()>;
}

/// A bounded exponential backoff for one logical model iteration.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ModelRetryPolicy {
    max_retries: u32,
    initial_delay: Duration,
    max_delay: Duration,
}

impl ModelRetryPolicy {
    pub fn new(max_retries: u32, initial_delay: Duration, max_delay: Duration) -> Option<Self> {
        (max_retries > 0 && !initial_delay.is_zero() && initial_delay <= max_delay).then_some(
            Self {
                max_retries,
                initial_delay,
                max_delay,
            },
        )
    }

    pub(crate) fn max_retries(self) -> u32 {
        self.max_retries
    }

    pub(crate) fn initial_delay(self) -> Duration {
        self.initial_delay
    }

    pub(crate) fn next_delay(self, previous: Duration) -> Duration {
        previous.saturating_mul(2).min(self.max_delay)
    }
}

pub(crate) async fn wait_for_retry(
    timer: &dyn RetryTimer,
    mut remaining: Duration,
    cancellation: &CancellationToken,
) {
    while !remaining.is_zero() && !cancellation.is_cancelled() {
        let slice = remaining.min(Duration::from_millis(10));
        timer.sleep(slice).await;
        remaining -= slice;
    }
}
