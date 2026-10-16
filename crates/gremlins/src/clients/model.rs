use std::io::{self, Write};

use rig_core::completion::{CompletionRequest, CompletionResponse};
use rig_core::driver::DynModel;
use rig_core::error::ProviderError;
use rig_core::operation::Completion;
use rig_core::streaming::Streamed;
use rig_core::wire::Capabilities;

/// A [`DynModel<Completion>`] guarded by a global prompt-size ceiling.
///
/// The wrapper measures the normalized [`CompletionRequest`] as serialized
/// JSON *before* it reaches rig. Requests over the configured limit fail with
/// [`GremlinsModelError::TooLarge`] and never touch the inner model, so no
/// wire bytes are ever sent for an oversized prompt.
#[derive(Clone)]
pub struct GremlinsModel {
    inner: DynModel<Completion>,
    max_prompt_bytes: u64,
}

impl GremlinsModel {
    /// Wrap an erased rig completion model with `max_prompt_bytes` as its
    /// hard prompt-size limit.
    pub(crate) fn new(inner: DynModel<Completion>, max_prompt_bytes: u64) -> Self {
        Self {
            inner,
            max_prompt_bytes,
        }
    }

    /// Measure `request` and delegate it to the inner model when it fits.
    ///
    /// An oversized request returns [`GremlinsModelError::TooLarge`] without
    /// calling the inner model. Provider failures from the inner model are
    /// preserved in [`GremlinsModelError::Provider`].
    pub(crate) fn stream(
        &self,
        request: CompletionRequest,
    ) -> Result<Streamed<Completion>, GremlinsModelError> {
        let actual = self.measure(&request)?;
        if actual > self.max_prompt_bytes {
            return Err(GremlinsModelError::TooLarge {
                limit: self.max_prompt_bytes,
                actual,
            });
        }
        self.inner
            .stream(request)
            .map_err(|e| GremlinsModelError::Provider(Box::new(e)))
    }

    /// Measure `request` and delegate it to the inner model when it fits.
    ///
    /// This mirrors `stream` for the unary path so both model surfaces stay
    /// governed by the same size check.
    #[allow(dead_code)]
    pub(crate) async fn call(
        &self,
        request: CompletionRequest,
    ) -> Result<CompletionResponse, GremlinsModelError> {
        let actual = self.measure(&request)?;
        if actual > self.max_prompt_bytes {
            return Err(GremlinsModelError::TooLarge {
                limit: self.max_prompt_bytes,
                actual,
            });
        }
        self.inner
            .call(request)
            .await
            .map_err(|e| GremlinsModelError::Provider(Box::new(e)))
    }

    pub(crate) fn name(&self) -> &str {
        self.inner.name()
    }

    pub(crate) fn id(&self) -> Option<&str> {
        self.inner.id()
    }

    #[allow(dead_code)]
    pub(crate) fn capabilities(&self) -> Capabilities {
        self.inner.capabilities()
    }

    fn measure(&self, request: &CompletionRequest) -> Result<u64, GremlinsModelError> {
        let mut counter = ByteCounter::default();
        serde_json::to_writer(&mut counter, request).map_err(|e| {
            GremlinsModelError::Provider(Box::new(ProviderError::Provider(format!(
                "failed to serialize completion request for prompt-size check: {e}"
            ))))
        })?;
        Ok(counter.count)
    }
}

/// An [`io::Write`] sink that counts serialized bytes without buffering them.
#[derive(Default)]
struct ByteCounter {
    count: u64,
}

impl Write for ByteCounter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.count += buf.len() as u64;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl std::fmt::Debug for GremlinsModel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GremlinsModel")
            .field("name", &self.name())
            .field("id", &self.inner.id())
            .field("max_prompt_bytes", &self.max_prompt_bytes)
            .finish()
    }
}

/// Errors produced by [`GremlinsModel`].
#[derive(Debug)]
pub(crate) enum GremlinsModelError {
    /// The serialized request exceeded the configured prompt-size ceiling.
    TooLarge { limit: u64, actual: u64 },
    /// The inner model rejected the request with a provider error.
    Provider(Box<ProviderError>),
}

impl std::fmt::Display for GremlinsModelError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GremlinsModelError::TooLarge { limit, actual } => write!(
                f,
                "prompt size {actual} bytes exceeds configured limit of {limit} bytes"
            ),
            GremlinsModelError::Provider(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for GremlinsModelError {}

#[cfg(test)]
mod tests {
    use super::*;
    use rig_core::test_utils::{MockCompletionModel, MockStreamEvent};

    #[test]
    fn rejects_oversized_prompt_without_touching_inner_model() {
        let inner = MockCompletionModel::from_stream_turns([vec![MockStreamEvent::text("ok")]]);
        let model = GremlinsModel::new(inner.clone().erase(), 16);
        let request = CompletionRequest::new("x".repeat(10_000));
        let serialized_len = serde_json::to_vec(&request).unwrap().len() as u64;
        assert!(serialized_len > 16);

        let err = match model.stream(request) {
            Ok(_) => panic!("oversized prompt must be rejected"),
            Err(err) => err,
        };
        match err {
            GremlinsModelError::TooLarge { limit, actual } => {
                assert_eq!(limit, 16);
                assert_eq!(actual, serialized_len);
            }
            GremlinsModelError::Provider(e) => {
                panic!("oversized prompt must not reach provider, got: {e}")
            }
        }
        assert!(
            inner.requests().is_empty(),
            "oversized prompt must not be dispatched to the inner model"
        );
    }

    #[test]
    fn delegates_under_limit_prompt_to_inner_model() {
        let inner = MockCompletionModel::from_stream_turns([vec![MockStreamEvent::text("ok")]]);
        let model = GremlinsModel::new(inner.clone().erase(), 1_048_576);
        let request = CompletionRequest::new("hello");

        assert!(
            model.stream(request).is_ok(),
            "under-limit prompt must reach the inner model"
        );
    }
}
