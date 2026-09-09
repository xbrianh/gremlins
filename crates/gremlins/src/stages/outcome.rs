use thiserror::Error;

/// Marker type returned when a stage completes without bailing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Done;

/// A stage bailed — the reason is recorded for the caller.
#[derive(Error, Debug, Clone, PartialEq, Eq)]
#[error("bail: {reason}")]
pub struct Bail {
    pub reason: String,
}

/// The return type of a stage run: `Done` (success) or `Bail` (failure).
/// In Rust callers this will become `Result<Done, Bail>` once callers are
/// ported.  For now we keep the Python convention where `Outcome = Done`
/// and bail is an exception.
pub type Outcome = Done;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn done_is_send_sync() {
        fn assert_send<T: Send>() {}
        fn assert_sync<T: Sync>() {}
        assert_send::<Done>();
        assert_sync::<Done>();
    }

    #[test]
    fn bail_reason_is_accessible() {
        let b = Bail {
            reason: "test".into(),
        };
        assert_eq!(b.reason, "test");
        assert_eq!(b.to_string(), "bail: test");
    }

    #[test]
    fn outcome_is_done() {
        // Just a compile-time assertion that Outcome = Done
        let _: Outcome = Done;
    }
}
