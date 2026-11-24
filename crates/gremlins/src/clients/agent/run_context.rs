use std::path::PathBuf;

use crate::clients::backend::RunParams;
use crate::clients::cancel::ErrorClassifier;
use crate::clients::conversation::Conversation;
use crate::clients::tools::tool_specs::ToolMode;

pub(crate) fn execution_corr_id(
    run_id: &str,
    session: &crate::clients::interactive::InteractiveSession,
) -> gremlins_magic_bus::CorrId {
    let mut corr = gremlins_magic_bus::CorrId::new();
    if !run_id.is_empty() {
        corr = corr.with_run(run_id);
    }
    if let Some(id) = session.snapshot().stage_run_id() {
        corr = corr.with_stage_run_id(id);
    }
    corr
}

#[cfg(test)]
mod tests {
    use super::execution_corr_id;
    use crate::clients::interactive::{InteractiveSession, InteractiveSessionFactory};

    #[test]
    fn producer_identity_is_captured_from_the_executing_session() {
        let (mut sessions, _) = InteractiveSessionFactory::for_chat();
        let first = sessions.next_session();
        let expected = first.snapshot().stage_run_id().unwrap().to_owned();
        let corr = execution_corr_id("producer-run", &first);
        let successor = sessions.next_session();
        assert_ne!(successor.snapshot().stage_run_id(), Some(expected.as_str()));
        assert_eq!(corr.run(), Some("producer-run"));
        assert_eq!(corr.stage_run_id(), Some(expected.as_str()));
        assert_eq!(execution_corr_id("producer-run", &first), corr);
    }

    #[test]
    fn unattached_producer_does_not_invent_stage_run_identity() {
        let corr = execution_corr_id("", &InteractiveSession::unattached());
        assert_eq!(corr.run(), None);
        assert_eq!(corr.stage_run_id(), None);
    }
}

#[derive(Clone)]
pub(crate) struct RunContext {
    pub(crate) params: RunParams,
    pub(crate) prefix: String,
    pub(crate) idle_timeout: f64,
    pub(crate) expected_artifact_paths: Vec<PathBuf>,
    pub(crate) reminder_budget: usize,
    pub(crate) completion_nudge_budget: usize,
    pub(crate) prepopulated_conversation: Box<dyn Conversation>,
}

impl RunContext {
    pub(crate) fn from_params(params: RunParams) -> Self {
        let idle_timeout = params
            .idle_timeout
            .unwrap_or_else(crate::config::stream_idle_timeout);
        let prefix = if params.label.is_empty() {
            String::new()
        } else {
            format!("[{}] ", params.label)
        };
        Self {
            idle_timeout,
            prefix,
            expected_artifact_paths: params.expected_artifact_paths.clone(),
            reminder_budget: crate::config::artifact_reminder_budget(),
            completion_nudge_budget: crate::config::completion_nudge_budget(),
            prepopulated_conversation: params.prepopulated_conversation.clone(),
            params,
        }
    }
}

pub(crate) struct LoopOpts<'a> {
    pub(crate) extra: Option<serde_json::Value>,
    pub(crate) tool_filter: Option<&'a [String]>,
    pub(crate) tool_mode: ToolMode,
    pub(crate) classify_error: Option<ErrorClassifier>,
    pub(crate) temperature: Option<f64>,
    pub(crate) max_tokens: Option<u64>,
    pub(crate) preserve_reasoning_in_history: bool,
    /// Fallback system preamble for naked runs that have no conversation
    /// system prompt (e.g. local compaction). Ignored when the rendered turn
    /// already carries a system prompt.
    pub(crate) preamble: Option<String>,
    /// When true, the agent loop will not loop on reasoning-only or empty
    /// turns — it immediately exhausts. Used by single-turn naked
    /// completions (local compaction, etc.) where the conversation has no
    /// record_turn support and budgets are zero.
    pub(crate) single_turn: bool,
}
