//! State-layer-neutral per-turn payload for a session's persistent
//! chat-gremlin task.
//!
//! Lives outside both the actor and the `chat` module tree so that
//! [`super::chat_handle::ChatGremlinHandle`] and the `chat` module can share
//! the same prompt queue item without creating an actor ↔ chat dependency
//! cycle.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use crate::clients::client::ModelURI;
use crate::skills::LoadedSkill;

/// One operator turn queued to a
/// [`super::chat_handle::ChatGremlinHandle`]'s task.
///
/// Carries everything the task needs to run a single turn without reaching
/// back into the request handler: the raw prompt text (the dynamic preamble is
/// assembled inside the task), the repository-context snapshot, the model and
/// system prompt for the turn, the operator environment, and the skill set. The gremlin
/// itself is taken out of the session by the task at turn start.
pub(crate) struct PromptMsg {
    /// The operator's raw prompt text (before the dynamic preamble is added).
    pub(crate) text: String,
    /// The TUI-captured repository-context snapshot for this turn.
    pub(crate) context: String,
    /// Client-generated turn id, echoed on every chat event.
    pub(crate) turn: Option<String>,
    /// The model to run this turn with.
    pub(crate) chat_model: ModelURI,
    /// The base system prompt for this turn.
    pub(crate) system_prompt: String,
    /// The operator's project root.
    pub(crate) project_root: PathBuf,
    /// The operator's environment for this turn.
    pub(crate) env: HashMap<String, String>,
    /// Skill names the operator explicitly activated for this turn.
    pub(crate) requested_skills: Vec<String>,
    /// Every skill discovered under the project root this turn.
    pub(crate) all_skills: Arc<Vec<LoadedSkill>>,
}
