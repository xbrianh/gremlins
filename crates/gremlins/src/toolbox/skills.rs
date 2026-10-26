use async_trait::async_trait;

use crate::clients::tools::{parse_args, AgentLoopRuntime, ToolHandler};

pub(crate) struct LoadSkillTool;

#[async_trait]
impl ToolHandler for LoadSkillTool {
    async fn invoke(&self, _id: &str, ctx: &AgentLoopRuntime, args_json: &str) -> String {
        let args = match parse_args(args_json) {
            Ok(v) => v,
            Err(e) => return e,
        };
        load_skill_invoke(ctx, &args)
    }
}

/// Invoke handler for the LoadSkill tool.
pub(crate) fn load_skill_invoke(ctx: &AgentLoopRuntime, args: &serde_json::Value) -> String {
    let name = match args.get("name").and_then(|v| v.as_str()) {
        Some(n) => n,
        None => return "Error: missing 'name' for LoadSkill tool".to_string(),
    };

    let Some(discovered) = super::skills_registry::get_skills() else {
        return "Error: LoadSkill is only available in chat sessions".to_string();
    };
    let Some(active) = &ctx.active_skills else {
        return "Error: LoadSkill is only available in chat sessions".to_string();
    };
    // Look up the skill by name.
    let loaded = match crate::skills::find_skill(&discovered, name, true) {
        Some(l) => l,
        None => return format!("Error: skill {name:?} not found"),
    };

    // Reject skills that disable model invocation. Discovery retains them
    // and `render_index` only hides them, so a model that learns the name
    // from a file or user message could otherwise load and activate an
    // operator-only skill. `user-invocable: false` is still allowed — the
    // model self-selects skills.
    if loaded.skill.disable_model_invocation {
        return format!("Error: skill {name:?} is not available for model invocation");
    }

    // Single critical section: check containment AND push so two
    // concurrent LoadSkill calls for the same name cannot both pass
    // the check and both push.
    let mut active_guard = match active.lock() {
        Ok(g) => g,
        Err(e) => return format!("Error: internal lock error: {e}"),
    };
    if active_guard.contains(&name.to_string()) {
        return format!("Skill {name:?} is already active.");
    }
    active_guard.push(name.to_string());
    drop(active_guard);

    // Return a body-free acknowledgement. The skill body is injected as a
    // trusted system-prompt amendment at the next turn boundary, not written
    // into the transcript. Putting the body here would leak it into archived
    // records and cross-turn history, where later turns would replay it
    // alongside the trusted amendment (doubling the body in context).
    let n_files = loaded.files.len();
    let mut result = format!("Skill {name:?} loaded successfully.");
    if n_files > 0 {
        result.push_str(&format!(" {n_files} bundled file(s) available."));
    }
    result.push_str(&format!(
        "\n\n**Skill directory:** `{}`\n",
        loaded.skill.dir.display()
    ));
    result
}
