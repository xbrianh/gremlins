use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use super::agent_loop::CancelToken;
use super::backend::ClientError;

/// A thread-safe map of in-flight cancel tokens, keyed by gremlin ID and
/// then by a monotonically-increasing per-backend attempt counter.
///
/// This replaces the inline `Mutex<HashMap<String, HashMap<u64, Arc<CancelToken>>>>`
/// that was duplicated across every backend's run-state struct.
pub(crate) struct CancelsMap(Mutex<HashMap<String, HashMap<u64, Arc<CancelToken>>>>);

impl CancelsMap {
    /// Check for cancellation, then insert the token under lock.
    ///
    /// The check and insert are atomic under the same lock so `reap_all`
    /// cannot remove a gremlin entry between the check and the insert.
    ///
    /// Returns `Err(ClientError::Runtime { message: "cancelled" })` if the
    /// token has already been cancelled.
    pub(crate) fn insert(
        &self,
        gremlin_id: &str,
        id: u64,
        token: Arc<CancelToken>,
    ) -> Result<(), ClientError> {
        let mut guard = self.0.lock().unwrap();
        if token.is_cancelled() {
            return Err(ClientError::Runtime {
                message: "cancelled".into(),
            });
        }
        guard
            .entry(gremlin_id.to_string())
            .or_default()
            .insert(id, token);
        Ok(())
    }

    /// Remove one token by gremlin ID + attempt counter.  Drops the
    /// gremlin entry when it becomes empty.
    pub(crate) fn remove(&self, gremlin_id: &str, id: u64) {
        if let Ok(mut guard) = self.0.lock() {
            if let Some(inner) = guard.get_mut(gremlin_id) {
                inner.remove(&id);
                if inner.is_empty() {
                    guard.remove(gremlin_id);
                }
            }
        }
    }

    /// Remove the gremlin entry, cancel every token it contained, and log at
    /// debug level.
    pub(crate) fn reap_all(&self, gremlin_id: &str, log_label: &str, model: &str) {
        if let Ok(mut guard) = self.0.lock() {
            let tokens: Vec<_> = guard
                .remove(gremlin_id)
                .into_iter()
                .flat_map(|m| m.into_values())
                .collect();
            let count = tokens.len();
            log::debug!(
                "{log_label}::reap_all: cancelling {count} in-flight token(s) for gremlin_id={gremlin_id} (model={model})",
            );
            for token in &tokens {
                token.cancel();
            }
        }
    }
}

impl Default for CancelsMap {
    fn default() -> Self {
        Self(Mutex::new(HashMap::new()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Insert then reap: tokens are cancelled and the gremlin entry is removed.
    #[test]
    fn insert_then_reap_removes_entry() {
        let map = CancelsMap::default();
        let a = CancelToken::new();
        let b = CancelToken::new();
        map.insert("gr-a", 1, a.clone()).unwrap();
        map.insert("gr-a", 2, b.clone()).unwrap();

        map.reap_all("gr-a", "test", "gpt-4");
        assert!(a.is_cancelled());
        assert!(b.is_cancelled());
        // Entry is gone after reap.
        assert!(!map.contains("gr-a"));
    }

    /// Reap only removes its own gremlin; siblings are untouched.
    #[test]
    fn reap_preserves_sibling_entry() {
        let map = CancelsMap::default();
        let a = CancelToken::new();
        let sibling = CancelToken::new();
        map.insert("gr-a", 1, a.clone()).unwrap();
        map.insert("gr-b", 2, sibling.clone()).unwrap();

        map.reap_all("gr-a", "test", "gpt-4");
        assert!(a.is_cancelled());
        assert!(!sibling.is_cancelled());
        assert!(!map.contains("gr-a"));
        assert!(map.contains("gr-b"));
    }

    /// Removing the last token drops the gremlin entry.
    #[test]
    fn remove_drops_empty_entry() {
        let map = CancelsMap::default();
        map.insert("gr-a", 1, CancelToken::new()).unwrap();
        assert!(map.contains("gr-a"));
        map.remove("gr-a", 1);
        assert!(!map.contains("gr-a"));
    }

    /// Removing one of many tokens keeps the entry alive.
    #[test]
    fn remove_keeps_entry_when_tokens_remain() {
        let map = CancelsMap::default();
        map.insert("gr-a", 1, CancelToken::new()).unwrap();
        map.insert("gr-a", 2, CancelToken::new()).unwrap();
        assert!(map.contains("gr-a"));
        map.remove("gr-a", 1);
        assert!(map.contains("gr-a"));
    }

    /// Inserting an already-cancelled token returns Err and inserts nothing.
    #[test]
    fn insert_cancelled_token_returns_err() {
        let map = CancelsMap::default();
        let token = CancelToken::new();
        token.cancel();
        let err = map.insert("gr-a", 1, token).unwrap_err();
        assert!(matches!(err, ClientError::Runtime { .. }));
        assert!(!map.contains("gr-a"));
    }

    /// Remove and reap_all are no-ops for unknown gremlin ids.
    #[test]
    fn remove_and_reap_unknown_id_noop() {
        let map = CancelsMap::default();
        map.remove("nobody", 1);
        map.reap_all("nobody", "test", "gpt-4");
        // No panics, no side effects.
    }

    // ── test helpers ────────────────────────────────────────────────

    impl CancelsMap {
        fn contains(&self, gremlin_id: &str) -> bool {
            self.0.lock().unwrap().contains_key(gremlin_id)
        }
    }
}
