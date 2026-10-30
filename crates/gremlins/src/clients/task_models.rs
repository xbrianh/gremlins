use std::collections::HashMap;
use std::sync::Arc;

use super::model::GremlinsModel;
use crate::task_kind::TaskKind;

/// Build a [`TaskModelSelector`] from the raw `task-models` config map and a
/// provider-agnostic model factory.
///
/// Returns `None` when the map is empty (the common case). Resolution is
/// deliberately lazy: `factory` is only invoked for a matching task kind, so a
/// configured entry whose kind never runs does not force that provider's
/// backend or credentials to resolve.
pub(crate) fn build_task_selector(
    factory: TaskModelFactory,
    by_kind: &HashMap<TaskKind, String>,
) -> Option<TaskModelSelector> {
    let by_kind = by_kind.clone();
    TaskModelSelector::new(by_kind, factory)
}

/// A model selected for a Task invocation, paired with the request options
/// resolved by the backend that built it.
///
/// `temperature`, `max_tokens`, and `extra` are the target provider's resolved
/// values — never the parent's — so a cross-provider override does not leak an
/// Anthropic `cache_control`/128 000-token default onto an OpenAI model.
#[derive(Clone, Debug)]
pub(crate) struct TaskModel {
    pub(crate) model: GremlinsModel,
    pub(crate) temperature: Option<f64>,
    pub(crate) max_tokens: Option<u64>,
    pub(crate) extra: Option<serde_json::Value>,
    /// Override for the child backend's reasoning-replay preference.
    /// `None` means "inherit the parent's value".
    pub(crate) preserve_reasoning: Option<bool>,
}

/// Builds the model named by a matching `task-models` entry, for use by one
/// Task invocation. Returns `Err` with a reason when the entry cannot be
/// resolved (for example, its backend cannot be built because a credential is
/// missing).
pub(crate) type TaskModelFactory = Arc<dyn Fn(&str) -> Result<TaskModel, String> + Send + Sync>;

/// Lookup map for `task-models`, keyed by the exact [`TaskKind`] spelling.
/// Shared behind an `Arc` so a Task fan-out clones one pointer per invocation
/// rather than the whole map.
#[derive(Debug)]
pub(crate) struct TaskModelOverrides {
    pub(crate) by_kind: HashMap<TaskKind, String>,
}

impl TaskModelOverrides {
    /// The spec configured for `kind`, if any. Lookup is an exact key match.
    pub(crate) fn spec_for(&self, kind: TaskKind) -> Option<&str> {
        self.by_kind.get(&kind).map(|s| s.as_str())
    }
}

/// Everything a Task invocation needs to pick a model from `task-models`: the
/// lookup maps and a factory that builds the model a matching entry names.
///
/// `None` at a call site means no `task-models` is configured, so every task
/// keeps the parent's model. Cloning a selector is O(1) — both halves sit behind
/// an `Arc` — which matters because a Task fan-out clones one per invocation.
#[derive(Clone)]
pub struct TaskModelSelector {
    overrides: Arc<TaskModelOverrides>,
    factory: TaskModelFactory,
}

impl std::fmt::Debug for TaskModelSelector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TaskModelSelector")
            .field("overrides", &self.overrides)
            .finish_non_exhaustive()
    }
}

impl TaskModelSelector {
    /// Assemble a selector from parsed config. Returns `None` when the map is
    /// empty — the common case, where nothing ever needs building.
    pub(crate) fn new(
        by_kind: HashMap<TaskKind, String>,
        factory: TaskModelFactory,
    ) -> Option<Self> {
        if by_kind.is_empty() {
            return None;
        }
        Some(Self {
            overrides: Arc::new(TaskModelOverrides { by_kind }),
            factory,
        })
    }

    /// The [`TaskModel`] for a Task of `kind`, or a clone of `default` when no
    /// entry matches.
    ///
    /// Returns `(model, spec_string)` where `spec_string` is the matched
    /// `task-models` entry (for logging), or `None` when no entry matched. A
    /// matched entry that fails to build is an `Err` — it must surface as a
    /// task result, not silently fall back to `default`.
    pub(crate) fn model_for(
        &self,
        kind: TaskKind,
        default: &TaskModel,
    ) -> Result<(TaskModel, Option<String>), String> {
        match self.overrides.spec_for(kind) {
            Some(spec) => match (self.factory)(spec) {
                Ok(m) => Ok((m, Some(spec.to_string()))),
                Err(reason) => Err(reason),
            },
            None => Ok((default.clone(), None)),
        }
    }
}
