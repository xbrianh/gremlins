use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use rig_core::error::ProviderError;
use tokio::sync::Notify;

use super::error::ClientError;

#[derive(Debug)]
pub struct CancelToken {
    flag: AtomicBool,
    notify: Notify,
}

impl CancelToken {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            flag: AtomicBool::new(false),
            notify: Notify::new(),
        })
    }

    pub fn cancel(&self) {
        self.flag.store(true, Ordering::Relaxed);
        // Cancellation is broadcast: an active chat turn has both
        // `run_chat_turn` and the tool-phase select arm waiting on this
        // token. `notify_waiters()` wakes every waiter so the outer
        // `finish()` path is never left asleep while the inner waiter
        // unwinds the turn.
        self.notify.notify_waiters();
    }

    pub fn is_cancelled(&self) -> bool {
        self.flag.load(Ordering::Relaxed)
    }

    pub async fn cancelled(&self) {
        let notified = self.notify.notified();
        if self.flag.load(Ordering::Relaxed) {
            return;
        }
        notified.await;
    }
}

pub(crate) type ErrorClassifier = fn(ProviderError) -> ClientError;

pub(crate) fn default_classify(err: ProviderError) -> ClientError {
    if let Some(status) = err.provider_response_status() {
        let code = status.as_u16();
        // Only retry 5xx and 429.
        if (500..600).contains(&code) || code == 429 {
            return ClientError::ApiServerError {
                message: err.to_string(),
            };
        }
        return ClientError::Runtime {
            message: err.to_string(),
        };
    }
    // No HTTP status — mid-stream SSE error (e.g. X.AI response.failed).
    // Retry it; log at WARNING so we can spot provider patterns later.
    log::info!("retrying provider error (no HTTP status): {}", err);
    ClientError::ApiServerError {
        message: err.to_string(),
    }
}

/// Classify a provider error and report whether it is retryable at the turn
/// boundary (429 / 5xx / no-HTTP-status mid-stream drops). Reuses the same
/// `default_classify` (or the caller-supplied classifier) used for the
/// terminal error path, so the retry decision stays consistent with what the
/// backend would otherwise surface.
pub(crate) fn is_retryable(err: &ProviderError, classify_error: Option<ErrorClassifier>) -> bool {
    let classify = classify_error.unwrap_or(default_classify);
    matches!(classify(err.clone()), ClientError::ApiServerError { .. })
}
