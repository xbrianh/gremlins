use async_trait::async_trait;

use gremlins_magic_bus::emit_corr_event_async;
use gremlins_magic_bus::EventKind;

use crate::clients::conversation::Conversation;
use crate::clients::tools::registry::ToolHandler;
use crate::clients::tools::runtime::{AgentLoopRuntime, ToolOutcome};
use crate::clients::tools::util::parse_args;

pub(crate) struct LoadSkillTool;

#[async_trait]
impl ToolHandler for LoadSkillTool {
    fn skills_gated(&self) -> bool {
        true
    }

    async fn invoke(
        &self,
        _id: &str,
        ctx: &AgentLoopRuntime,
        _conv: &dyn Conversation,
        args_json: &str,
    ) -> ToolOutcome {
        let args = match parse_args(args_json) {
            Ok(v) => v,
            Err(e) => return ToolOutcome::text(e),
        };
        ToolOutcome::text(load_skill_invoke(ctx, &args).await)
    }
}

/// Invoke handler for the LoadSkill tool.
pub(crate) async fn load_skill_invoke(ctx: &AgentLoopRuntime, args: &serde_json::Value) -> String {
    let name = match args.get("name").and_then(|v| v.as_str()) {
        Some(n) => n,
        None => return "Error: missing 'name' for LoadSkill tool".to_string(),
    };

    let Some(discovered) = ctx.discovered_skills.as_ref() else {
        return "Error: LoadSkill is only available in chat sessions".to_string();
    };
    // Look up the skill by name.
    let loaded = match crate::skills::find_skill(discovered, name, true) {
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

    // Announce the activation on the bus. The supervisor subscribes and
    // folds the skill body into the next turn's system prompt. The actor
    // deduplicates by name, so repeated LoadSkill calls for the same skill
    // are idempotent (though the model receives a success acknowledgement
    // each time — there is no "already active" error in this design).
    //
    // Emit the canonical name from the resolved skill, not the raw
    // invocation argument, in case find_skill resolved an alias.
    emit_corr_event_async(
        &ctx.corr_id,
        EventKind::SkillLoaded {
            id: ctx.message_prefix.clone(),
            name: loaded.skill.name.clone(),
        },
    )
    .await;

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
