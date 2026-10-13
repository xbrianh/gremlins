/// Turn-level backoff for retryable stream errors (429 / 5xx). Much shorter
/// than `STREAM_IDLE_BACKOFF` — rate limits typically resolve within seconds,
/// and we retry the *same turn* without discarding history, so a long sleep
/// would just stall the loop.
pub(crate) const TURN_RETRY_BACKOFF: [f64; 3] = [2.0, 5.0, 10.0];
