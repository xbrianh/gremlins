use std::future::Future;
use std::time::Duration;

use tokio::time::sleep;

pub(crate) async fn with_retry<F, Fut, T, E>(
    backoff: &[f64],
    classify: impl Fn(&E) -> bool,
    mut on_retry: impl FnMut(usize, &E, f64),
    mut f: F,
) -> Result<T, E>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, E>>,
{
    let max_attempts = backoff.len() + 1;
    for attempt in 0..max_attempts {
        match f().await {
            Ok(val) => return Ok(val),
            Err(e) => {
                if attempt == backoff.len() || !classify(&e) {
                    return Err(e);
                }
                let wait = backoff[attempt];
                on_retry(attempt, &e, wait);
                sleep(Duration::from_secs_f64(wait)).await;
            }
        }
    }
    unreachable!()
}

pub(crate) const STREAM_IDLE_BACKOFF: [f64; 3] = [60.0, 300.0, 600.0];

pub(crate) fn validate_max_retries(max_retries: usize) -> Result<(), String> {
    if max_retries > STREAM_IDLE_BACKOFF.len() {
        Err(format!(
            "max_retries={max_retries} exceeds backoff schedule length {}",
            STREAM_IDLE_BACKOFF.len()
        ))
    } else {
        Ok(())
    }
}

const TRANSIENT_SUBSTRINGS: &[&str] = &[
    "capacity",
    "rate limit",
    "rate_limit",
    "too many requests",
    "try again",
    "please retry",
    "server error",
    "service unavailable",
    "bad gateway",
    "gateway timeout",
    "overloaded",
    "timed out in queue",
    " 529",
    // reqwest transport errors (transient)
    "http client error",
    "error sending request",
    "error decoding response body",
    "connection reset",
    "connection refused",
    "dns error",
    "tls handshake",
];

pub(crate) fn is_transient_stream_error(message: &str) -> bool {
    let lower = message.to_lowercase();
    TRANSIENT_SUBSTRINGS.iter().any(|s| lower.contains(s))
}
