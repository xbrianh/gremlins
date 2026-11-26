//! Request accounting is owned by the stream, not by its caller's future.
#[cfg(test)]
#[path = "request_usage_tests.rs"]
mod tests;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use futures::Stream;
use gremlins_magic_bus::{emit_corr_event, CorrId, EventKind, ModelCallUsage};
use rig_core::completion::{CompletionResponse, Usage};
use rig_core::operation::Completion;
use rig_core::streaming::Streamed;

pub(crate) struct RequestUsage {
    corr_id: CorrId,
    record: Option<ModelCallUsage>,
}

impl RequestUsage {
    pub(crate) fn new(corr_id: &CorrId, provider: &str, model: &str, chain: &str) -> Self {
        Self {
            corr_id: corr_id.clone(),
            record: Some(ModelCallUsage {
                request_id: uuid::Uuid::new_v4().to_string(),
                provider: provider.into(),
                model: model.into(),
                agent_id: chain.into(),
                request_kind: String::new(),
                reported: false,
                completed: false,
                raw: serde_json::Value::Null,
                usage: super::usage_normalize::normalize(&Usage::default(), false),
            }),
        }
    }

    pub(crate) fn mark_compaction(&mut self) {
        self.record
            .as_mut()
            .expect("unpublished request")
            .request_kind = "compaction".into();
    }

    pub(crate) fn capture_usage(
        &mut self,
        usage: &Usage,
        raw: &serde_json::Value,
        completed: bool,
        completed_turn: bool,
    ) {
        let record = self.record.as_mut().expect("unpublished request");
        record.completed = completed;
        record.reported = usage.is_reported();
        record.usage = super::usage_normalize::normalize(usage, completed && completed_turn);
        // Never copy response prose, including error messages.
        let mut metadata = serde_json::Map::new();
        for key in ["usage", "usageMetadata", "usage_metadata", "copilot_usage"] {
            if let Some(value) = raw.get(key) {
                metadata.insert(key.into(), value.clone());
            }
        }
        record.raw = if metadata.is_empty() {
            serde_json::to_value(usage).unwrap_or_default()
        } else {
            serde_json::Value::Object(metadata)
        };
    }
}

impl Drop for RequestUsage {
    fn drop(&mut self) {
        if let Some(usage) = self.record.take() {
            emit_corr_event(&self.corr_id, EventKind::ModelCallUsage { usage });
        }
    }
}

/// Only this request's final counters; never recovered from a broadcast bus.
#[derive(Clone, Default)]
pub(crate) struct FinalUsage(Arc<Mutex<Option<Usage>>>);

impl FinalUsage {
    pub(crate) fn take(&self) -> Option<Usage> {
        self.0.lock().unwrap().take()
    }
}

pub(crate) struct UsageStream {
    stream: Streamed<Completion>,
    usage: RequestUsage,
    failed: bool,
    ended: bool,
    observation: FinalUsage,
}

impl UsageStream {
    pub(crate) fn new(stream: Streamed<Completion>, usage: RequestUsage) -> Self {
        Self {
            stream,
            usage,
            failed: false,
            ended: false,
            observation: FinalUsage::default(),
        }
    }

    pub(crate) fn observation(&self) -> FinalUsage {
        self.observation.clone()
    }

    pub(crate) fn partial(&self) -> CompletionResponse {
        self.stream.partial()
    }
}

impl Stream for UsageStream {
    type Item = <Streamed<Completion> as Stream>::Item;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        let result = Pin::new(&mut this.stream).poll_next(cx);
        if matches!(result, Poll::Ready(Some(Err(_)))) {
            this.failed = true;
        }
        if matches!(result, Poll::Ready(None)) {
            this.ended = true;
        }
        result
    }
}

impl Drop for UsageStream {
    fn drop(&mut self) {
        let partial = self.stream.partial();
        let completed =
            self.ended && !self.failed && partial.error.is_none() && partial.aborted.is_none();
        self.usage
            .capture_usage(&partial.usage, &partial.raw, completed, true);
        *self.observation.0.lock().unwrap() = Some(partial.usage);
    }
}
