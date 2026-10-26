use std::sync::{Arc, RwLock};

use crate::skills::LoadedSkill;

static SKILLS: RwLock<Option<Arc<Vec<LoadedSkill>>>> = RwLock::new(None);

/// Store the discovered skills for this session. Called by the executor
/// right before kicking off the agent loop.
pub(crate) fn set_skills(skills: Arc<Vec<LoadedSkill>>) {
    *SKILLS.write().expect("skills lock poisoned") = Some(skills);
}

/// Retrieve the discovered skills, or `None` outside a chat session.
pub(crate) fn get_skills() -> Option<Arc<Vec<LoadedSkill>>> {
    SKILLS.read().expect("skills lock poisoned").clone()
}
