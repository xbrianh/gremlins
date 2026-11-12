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
