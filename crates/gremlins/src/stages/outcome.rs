/// Marker type returned when a stage completes without bailing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Done;

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
}
