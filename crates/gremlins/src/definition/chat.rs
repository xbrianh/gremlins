//! A [`ChatDefinition`] implementing [`GremlinDefinition`] for the
//! persistent chat-session gremlin.
//!
//! Unlike [`StaticDefinition`], this is not a cursor over a YAML stage list —
//! it is a stage *factory*. Each call to [`next_stage`] consumes a pending
//! operator prompt (set by the supervisor via [`set_pending`]) and builds an
//! [`ExecutorStage::Agent`] on demand, named `chat-NNNN` (4-digit zero-padded
//! turn counter). When no prompt is pending, it returns [`ExecutorStage::Done`].
//!
//! [`StaticDefinition`]: super::StaticDefinition
//! [`next_stage`]: GremlinDefinition::next_stage
//! [`set_pending`]: ChatDefinition::set_pending

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use async_trait::async_trait;

use crate::clients::client::ModelURI;
use crate::executor::agent_runner::SANDBOX_ALLOW_ALL_KEY;
use crate::schemas::bootstrap::Bootstrap;
use crate::stage_spec::agent::Agent;

use super::{DefinitionError, ExecutorStage, GremlinDefinition};

/// A stage-factory definition for the persistent chat-session gremlin.
///
/// State: a pending operator prompt (set by the supervisor before each turn),
/// a default model, a system prompt, and a zero-based turn counter. The first
/// `set_pending` + `next_stage` produces stage `chat-0001`; the artifact
/// written afterward is `conversation/0001.json`.
#[derive(Debug, Clone)]
pub struct ChatDefinition {
    /// The definition's identity — always `"chat"`.
    name: String,
    /// A placeholder path (chat definitions are never loaded from disk).
    path: PathBuf,
    /// The model used unless the stage declares its own.
    default_model: ModelURI,
    /// The base system prompt, carrying the repository-context block.
    /// Kept for potential fallback use; currently the supervisor always
    /// provides a per-turn system prompt via [`set_pending`].
    _system_prompt: String,
    /// Zero-based turn counter. Incremented by [`next_stage`].
    turn: usize,
    /// Operator input queued by the supervisor, consumed by the next
    /// `next_stage` call: `(prompt_text, model, system_prompt)`.
    pending: Option<(String, ModelURI, String)>,
}

impl ChatDefinition {
    /// Create a fresh definition with no pending prompt.
    pub fn new(
        name: String,
        path: PathBuf,
        default_model: ModelURI,
        system_prompt: String,
    ) -> Self {
        Self {
            name,
            path,
            default_model,
            _system_prompt: system_prompt,
            turn: 0,
            pending: None,
        }
    }

    /// Store an operator prompt for consumption by the next [`next_stage`] call.
    ///
    /// The `model` and `system_prompt` are consumed by the next
    /// [`next_stage`](GremlinDefinition::next_stage) call to build the agent
    /// stage. The stored `default_model` on the definition is left unchanged.
    pub fn set_pending(&mut self, prompt: &str, model: &ModelURI, system_prompt: &str) {
        self.pending = Some((prompt.to_string(), model.clone(), system_prompt.to_string()));
    }

    /// The zero-based turn counter (how many stages have been produced so far).
    pub fn turn_count(&self) -> usize {
        self.turn
    }
}

#[async_trait]
impl GremlinDefinition for ChatDefinition {
    fn name(&self) -> &str {
        &self.name
    }

    fn default_model(&self) -> &ModelURI {
        &self.default_model
    }

    fn bootstrap(&self) -> &Bootstrap {
        // Chat gremlins run no bootstrap.
        static EMPTY: LazyLock<Bootstrap> = LazyLock::new(|| Bootstrap {
            cmds: Vec::new(),
            launch_cmds: Vec::new(),
            cli_out: std::collections::HashMap::new(),
            env: String::new(),
            source: None,
        });
        &EMPTY
    }

    fn land(&self) -> Option<ExecutorStage> {
        None
    }

    fn clean_cmds(&self) -> &[String] {
        &[]
    }

    fn is_at_start(&self) -> bool {
        self.turn == 0
    }

    /// Always `false` — chat definitions never trigger the YAML-loading path
    /// in [`init_runtime`].
    fn is_stub(&self) -> bool {
        false
    }

    fn with_model(&mut self, model: &ModelURI) {
        self.default_model = model.clone();
    }

    /// Deep-clone including the turn counter and pending state. The gremlin is
    /// taken from and re-inserted into the session map between turns; a shallow
    /// clone would lose state.
    fn clone_box(&self) -> Box<dyn GremlinDefinition> {
        Box::new(self.clone())
    }

    fn first_stage_name(&self) -> &str {
        // Chat definitions have no pre-declared stage list; the first stage
        // name is the definition name itself.  This is only used for parallel
        // child identification, which chat definitions never participate in.
        &self.name
    }

    fn path(&self) -> &Path {
        &self.path
    }

    /// Parse `chat-NNNN` to extract the turn index and reposition the counter
    /// for rollback.  Unknown names reset to turn 0.
    fn goto(&mut self, stage: &str) {
        if let Some(num_str) = stage.strip_prefix("chat-") {
            if let Ok(num) = num_str.parse::<usize>() {
                if num > 0 {
                    // Stage names are 1-based; internal counter is 0-based.
                    // `goto("chat-0003")` sets turn=3 so the *next*
                    // `next_stage` call produces `chat-0004`.
                    self.turn = num;
                    return;
                }
            }
        }
        self.turn = 0;
    }

    /// Consume the pending prompt and build an agent stage.
    ///
    /// Returns [`ExecutorStage::Done`] when no prompt has been queued.
    async fn next_stage(&mut self) -> Result<ExecutorStage, DefinitionError> {
        let (prompt, model, system_prompt) = match self.pending.take() {
            Some(p) => p,
            None => return Ok(ExecutorStage::Done),
        };

        self.turn += 1;
        let stage_name = format!("chat-{:04}", self.turn);

        let mut options: HashMap<String, serde_json::Value> = HashMap::new();
        options.insert(
            "system_prompt".to_string(),
            serde_json::Value::String(system_prompt),
        );
        options.insert(
            SANDBOX_ALLOW_ALL_KEY.to_string(),
            serde_json::Value::Bool(true),
        );

        let agent = Agent {
            name: stage_name.clone(),
            prompts: vec![prompt],
            options,
            interpolation_map: HashMap::new(),
            outputs_map: HashMap::new(),
        };

        Ok(ExecutorStage::Agent {
            stage: agent,
            model: Some(model),
            task_models: None,
        })
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn model(spec: &str) -> ModelURI {
        ModelURI::parse(spec).unwrap()
    }

    #[test]
    fn chat_definition_is_not_a_stub() {
        let def = ChatDefinition::new(
            "chat".into(),
            PathBuf::from("."),
            model("openai/gpt-4"),
            "you are helpful".into(),
        );
        assert!(!def.is_stub());
    }

    #[test]
    fn chat_definition_is_at_start_initially() {
        let def = ChatDefinition::new(
            "chat".into(),
            PathBuf::from("."),
            model("openai/gpt-4"),
            "sys".into(),
        );
        assert!(def.is_at_start());
    }

    #[tokio::test]
    async fn next_stage_returns_done_when_no_pending() {
        let mut def = ChatDefinition::new(
            "chat".into(),
            PathBuf::from("."),
            model("openai/gpt-4"),
            "sys".into(),
        );
        let stage = def.next_stage().await.unwrap();
        assert!(matches!(stage, ExecutorStage::Done));
    }

    #[tokio::test]
    async fn set_pending_then_next_stage_produces_chat_0001() {
        let mut def = ChatDefinition::new(
            "chat".into(),
            PathBuf::from("."),
            model("openai/gpt-4"),
            "sys".into(),
        );
        def.set_pending("hello", &model("openai/gpt-4"), "sys");
        let stage = def.next_stage().await.unwrap();
        assert_eq!(stage.name(), "chat-0001");
        assert_eq!(stage.stage_type(), "agent");
        assert!(!def.is_at_start());

        // No more pending — Done.
        let stage = def.next_stage().await.unwrap();
        assert!(matches!(stage, ExecutorStage::Done));
    }

    #[tokio::test]
    async fn successive_turns_increment_counter() {
        let mut def = ChatDefinition::new(
            "chat".into(),
            PathBuf::from("."),
            model("openai/gpt-4"),
            "sys".into(),
        );
        def.set_pending("turn 1", &model("openai/gpt-4"), "sys");
        assert_eq!(def.next_stage().await.unwrap().name(), "chat-0001");

        def.set_pending("turn 2", &model("openai/gpt-4"), "sys");
        assert_eq!(def.next_stage().await.unwrap().name(), "chat-0002");

        def.set_pending("turn 3", &model("openai/gpt-4"), "sys");
        assert_eq!(def.next_stage().await.unwrap().name(), "chat-0003");
    }

    #[tokio::test]
    async fn goto_repositions_turn_counter() {
        let mut def = ChatDefinition::new(
            "chat".into(),
            PathBuf::from("."),
            model("openai/gpt-4"),
            "sys".into(),
        );
        def.goto("chat-0003");
        // counter is now 3, so next stage is chat-0004
        def.set_pending("turn 4", &model("openai/gpt-4"), "sys");
        assert_eq!(def.next_stage().await.unwrap().name(), "chat-0004");
    }

    #[tokio::test]
    async fn goto_unknown_resets_to_zero() {
        let mut def = ChatDefinition::new(
            "chat".into(),
            PathBuf::from("."),
            model("openai/gpt-4"),
            "sys".into(),
        );
        def.set_pending("t1", &model("openai/gpt-4"), "sys");
        let _ = def.next_stage().await.unwrap();

        def.goto("no-such-stage");
        assert!(def.is_at_start());

        def.set_pending("t1 again", &model("openai/gpt-4"), "sys");
        assert_eq!(def.next_stage().await.unwrap().name(), "chat-0001");
    }

    #[tokio::test]
    async fn clone_box_preserves_pending_state() {
        let mut def = ChatDefinition::new(
            "chat".into(),
            PathBuf::from("."),
            model("openai/gpt-4"),
            "sys".into(),
        );
        def.set_pending("hello", &model("openai/gpt-4"), "sys");

        let mut cloned: Box<dyn GremlinDefinition> = def.clone_box();
        // The clone should still have the pending prompt — verify by
        // checking that next_stage() produces a stage rather than Done.
        assert_eq!(cloned.name(), "chat");
        assert_eq!(cloned.default_model().to_string(), "model://openai/gpt-4");
        // is_at_start checks turn counter (0-based), which is still 0
        // because set_pending doesn't increment it.
        assert!(cloned.is_at_start());

        // The clone should produce a stage from the pending prompt.
        let stage = cloned.next_stage().await.unwrap();
        assert_eq!(stage.name(), "chat-0001");
        assert_eq!(stage.stage_type(), "agent");
        // After consuming the pending prompt, next_stage returns Done.
        let stage = cloned.next_stage().await.unwrap();
        assert!(matches!(stage, ExecutorStage::Done));
    }

    #[test]
    fn with_model_updates_default() {
        let mut def = ChatDefinition::new(
            "chat".into(),
            PathBuf::from("."),
            model("openai/gpt-4"),
            "sys".into(),
        );
        def.with_model(&model("anthropic/claude"));
        assert_eq!(def.default_model().to_string(), "model://anthropic/claude");
    }

    #[test]
    fn serialize_returns_not_implemented() {
        let def = ChatDefinition::new(
            "chat".into(),
            PathBuf::from("."),
            model("openai/gpt-4"),
            "sys".into(),
        );
        assert!(def.serialize().is_err());
    }

    #[test]
    fn name_delegates() {
        let def = ChatDefinition::new(
            "chat".into(),
            PathBuf::from("."),
            model("openai/gpt-4"),
            "sys".into(),
        );
        assert_eq!(def.name(), "chat");
    }
}
