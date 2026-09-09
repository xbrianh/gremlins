use std::env;

pub fn stream_idle_timeout() -> f64 {
    env::var("GREMLINS_STREAM_IDLE_TIMEOUT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(600.0)
}

pub const STREAM_IDLE_BACKOFF: [f64; 3] = [60.0, 300.0, 600.0];

pub fn validate_max_retries(max_retries: usize) -> Result<(), String> {
    if max_retries > STREAM_IDLE_BACKOFF.len() {
        Err(format!(
            "max_retries={max_retries} exceeds backoff schedule length {}",
            STREAM_IDLE_BACKOFF.len()
        ))
    } else {
        Ok(())
    }
}

pub fn max_agent_turns() -> usize {
    env::var("GREMLINS_AGENT_MAX_TURNS")
        .ok()
        .and_then(|v| v.parse().ok())
        .or_else(|| {
            env::var("GREMLINS_OPENAI_AGENTS_MAX_TURNS")
                .ok()
                .and_then(|v| v.parse().ok())
        })
        .unwrap_or(1000)
}

pub fn reasoning_effort() -> Option<String> {
    env::var("GREMLINS_REASONING_EFFORT").ok()
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

pub fn is_transient_stream_error(message: &str) -> bool {
    let lower = message.to_lowercase();
    TRANSIENT_SUBSTRINGS.iter().any(|s| lower.contains(s))
}
