//! Ownership boundaries shared by exact-input and targeted-chat adapters.
use std::{future::Future, time::Duration};

pub(super) const WRITE_TIMEOUT: Duration = Duration::from_secs(5);

/// ACK must reach the peer before commands can use the binding.
pub(super) async fn acknowledge<B>(
    bound: B,
    closed: impl Future<Output = ()>,
    write: impl Future<Output = Result<(), String>>,
) -> Option<B> {
    ack_gate(bound, async {
        tokio::select! {
            biased;
            _ = closed => false,
            result = tokio::time::timeout(WRITE_TIMEOUT, write) => {
                matches!(result, Ok(Ok(())))
            }
        }
    })
    .await
}

/// Keep control only when the adapter confirms that its ACK succeeded.
pub(super) async fn ack_gate<B>(bound: B, ack: impl Future<Output = bool>) -> Option<B> {
    if ack.await {
        Some(bound)
    } else {
        drop(bound);
        None
    }
}

/// Enqueue under the claim, but never retain control while awaiting a receipt.
pub(super) fn enqueue_and_release<B, R>(bound: B, enqueue: impl FnOnce(&B) -> R) -> R {
    let receipt = enqueue(&bound);
    drop(bound);
    receipt
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{cell::Cell, rc::Rc};
    use tokio::sync::oneshot;

    #[derive(Default)]
    struct ClaimState {
        claimed: Cell<bool>,
        releases: Cell<usize>,
    }

    struct Claim(Rc<ClaimState>);

    impl ClaimState {
        fn claim(self: &Rc<Self>) -> Option<Claim> {
            if self.claimed.replace(true) {
                None
            } else {
                Some(Claim(self.clone()))
            }
        }
    }

    impl Drop for Claim {
        fn drop(&mut self) {
            assert!(self.0.claimed.replace(false));
            self.0.releases.set(self.0.releases.get() + 1);
        }
    }

    async fn rejected_ack(write: impl Future<Output = Result<(), String>>) {
        let state = Rc::new(ClaimState::default());
        let dispatches = Cell::new(0);
        let pipelined_bail = Some("bail");
        let bound = acknowledge(state.claim().unwrap(), std::future::pending(), write).await;
        if let Some(_bound) = bound {
            assert_eq!(pipelined_bail, Some("bail"));
            dispatches.set(dispatches.get() + 1);
        }
        assert_eq!(dispatches.get(), 0);
        assert_eq!(state.releases.get(), 1);
        assert!(
            state.claim().is_some(),
            "claim must be reusable without EOF"
        );
    }

    #[tokio::test]
    async fn failed_ack_releases_claim_without_pipelined_dispatch() {
        rejected_ack(async { Err("write failed".into()) }).await;
    }

    #[tokio::test(start_paused = true)]
    async fn timed_out_ack_releases_claim_without_pipelined_dispatch() {
        let start = tokio::time::Instant::now();
        rejected_ack(std::future::pending()).await;
        assert_eq!(start.elapsed(), WRITE_TIMEOUT);
    }

    #[tokio::test]
    async fn revocation_wins_over_ready_ack() {
        let state = Rc::new(ClaimState::default());
        assert!(
            acknowledge(state.claim().unwrap(), async {}, async { Ok(()) })
                .await
                .is_none()
        );
        assert_eq!(state.releases.get(), 1);
    }

    #[tokio::test]
    async fn successful_ack_precedes_dispatch_and_retains_claim() {
        let state = Rc::new(ClaimState::default());
        let acked = Cell::new(false);
        let dispatches = Cell::new(0);
        let bound = ack_gate(state.claim().unwrap(), async {
            assert_eq!(dispatches.get(), 0);
            assert!(state.claim().is_none());
            acked.set(true);
            true
        })
        .await
        .unwrap();
        assert!(acked.get());
        assert!(state.claim().is_none());
        assert_eq!(state.releases.get(), 0);
        dispatches.set(dispatches.get() + 1);
        assert_eq!(dispatches.get(), 1);
        drop(bound);
        assert_eq!(state.releases.get(), 1);
    }

    #[tokio::test]
    async fn held_correction_allows_second_claim_before_settlement() {
        let state = Rc::new(ClaimState::default());
        let mut held = Vec::new();
        let mut receipts = Vec::new();
        for _ in 0..2 {
            let mut receipt = enqueue_and_release(state.claim().unwrap(), |_| {
                assert!(state.claim().is_none(), "enqueue must hold the claim");
                let (correction, receipt) = oneshot::channel();
                held.push(correction);
                receipt
            });
            assert!(!state.claimed.get(), "return must release the claim");
            assert_eq!(receipt.try_recv(), Err(oneshot::error::TryRecvError::Empty));
            receipts.push(receipt);
        }
        assert_eq!(state.releases.get(), 2);
        for (index, correction) in held.into_iter().enumerate() {
            correction.send(index).unwrap();
        }
        for (index, receipt) in receipts.into_iter().enumerate() {
            assert_eq!(receipt.await.unwrap(), index);
        }
    }

    #[test]
    fn rejected_enqueue_releases_claim_before_return() {
        let state = Rc::new(ClaimState::default());
        let result = enqueue_and_release(state.claim().unwrap(), |_| {
            assert!(state.claim().is_none());
            Err::<(), _>("full")
        });
        assert_eq!(result, Err("full"));
        assert_eq!(state.releases.get(), 1);
        assert!(state.claim().is_some());
    }
}
