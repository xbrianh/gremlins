/// Turn-level backoff for retryable stream errors (429 / 5xx). Much shorter
/// than idle-timeout backoffs — rate limits typically resolve within seconds,
/// and we retry the *same turn* without discarding history, so a long sleep
/// would just stall the loop.
pub(crate) const TURN_RETRY_BACKOFF: [f64; 3] = [2.0, 5.0, 10.0];

/// Idle-timeout retry schedule. Longer than the 429/5xx schedule because an
/// idle stream may indicate the model is genuinely still working; a short
/// backoff would fire-and-retry before the model has a chance to produce a
/// token. Still much shorter than the old `STREAM_IDLE_BACKOFF` that restarted
/// the entire loop from scratch.
pub(crate) const TIMEOUT_RETRY_BACKOFF: [f64; 3] = [5.0, 15.0, 30.0];
