use std::sync::OnceLock;

use log::LevelFilter;

/// Cached log‑level threshold, read once from `GREMLINS_LOG_LEVEL`.
pub(crate) fn log_level_threshold() -> LevelFilter {
    static THRESHOLD: OnceLock<LevelFilter> = OnceLock::new();
    *THRESHOLD.get_or_init(|| {
        std::env::var("GREMLINS_LOG_LEVEL")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(LevelFilter::Info)
    })
}

/// Send a line through the per-gremlin log channel when the given
/// level meets the `GREMLINS_LOG_LEVEL` threshold.
pub(crate) fn send_log(
    tx: &Option<tokio::sync::mpsc::UnboundedSender<String>>,
    level: log::Level,
    msg: impl AsRef<str>,
) {
    if level > log_level_threshold() {
        return;
    }
    if let Some(tx) = tx {
        let _ = tx.send(msg.as_ref().to_string());
    }
}
