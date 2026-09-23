//! Recipe builders — free functions that return pre-configured stages matching
//! the bundled `gremlins:plan`, `gremlins:implement`, and `gremlins:verify`
//! YAML recipes.
//!
//! Each function loads its defaults from the bundled YAML assets in
//! `assets/data/stages/` via [`assets::RECIPES`], substitutes caller-provided
//! values for `{{prompt}}` / `{{options.*}}` placeholders, and parses the
//! stages through the existing [`RunnableStage::parse_stages`] path (which
//! calls the `from_dict` constructors).

use crate::assets;
use crate::builders::composite::LoopBuilder;
use crate::stages::node::{RunnableStage, StageError};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Load a bundled recipe YAML, substitute caller-provided values for
/// `{{prompt}}`, `{{options.cmds}}`, `{{options.max_iterations | default(N)}}`,
/// and `{{options.timeout | default(N)}}` placeholders, then parse the stage
/// list through the existing `RunnableStage::parse_stages` path.
fn load_recipe_stages(
    recipe_name: &str,
    prompts: &[String],
    cmds: Option<&[String]>,
    max_iterations: Option<u32>,
) -> Result<Vec<RunnableStage>, StageError> {
    let yaml_str = assets::RECIPES
        .get(recipe_name)
        .ok_or_else(|| StageError::Message(format!("bundled recipe {recipe_name:?} not found")))?;

    let mut root: serde_yaml::Value = serde_yaml::from_str(yaml_str)
        .map_err(|e| StageError::Message(format!("failed to parse recipe {recipe_name}: {e}")))?;

    // Substitute placeholders before parsing.
    substitute_recipe_placeholders(&mut root, prompts, cmds, max_iterations);

    let stages = root
        .get_mut("stages")
        .and_then(|v| v.as_sequence_mut())
        .ok_or_else(|| StageError::Message(format!("recipe {recipe_name}: missing 'stages'")))?;

    RunnableStage::parse_stages(stages, 0)
}

/// Walk a YAML value tree and replace `{{prompt}}`, `{{options.cmds}}`,
/// `{{options.max_iterations | default(N)}}`, and
/// `{{options.timeout | default(N)}}` placeholder strings with the
/// caller-supplied values.
fn substitute_recipe_placeholders(
    value: &mut serde_yaml::Value,
    prompts: &[String],
    cmds: Option<&[String]>,
    max_iterations: Option<u32>,
) {
    match value {
        serde_yaml::Value::Sequence(seq) => {
            // If any element is the {{prompt}} placeholder, replace the
            // entire sequence with the caller's prompts (the YAML stores
            // `prompt: ["{{prompt}}"]` — a list with one placeholder).
            if seq.iter().any(|v| v.as_str() == Some("{{prompt}}")) {
                *value = serde_yaml::Value::Sequence(
                    prompts
                        .iter()
                        .map(|p| serde_yaml::Value::String(p.clone()))
                        .collect(),
                );
                return;
            }
            for item in seq {
                substitute_recipe_placeholders(item, prompts, cmds, max_iterations);
            }
        }
        serde_yaml::Value::String(s) => {
            if s == "{{options.cmds}}" {
                if let Some(c) = cmds {
                    *value = serde_yaml::Value::String(c.join(" ; "));
                }
            } else if let Some(rest) = s.strip_prefix("{{options.max_iterations | default(") {
                if let Some(n) = max_iterations {
                    *value = serde_yaml::Value::Number(n.into());
                } else if let Some(default_str) = rest.strip_suffix(")}}") {
                    if let Ok(n) = default_str.parse::<u64>() {
                        *value = serde_yaml::Value::Number(n.into());
                    }
                }
            } else if let Some(rest) = s.strip_prefix("{{options.timeout | default(") {
                if let Some(default_str) = rest.strip_suffix(")}}") {
                    if let Ok(n) = default_str.parse::<f64>() {
                        *value = serde_yaml::Value::Number((n as i64).into());
                    }
                }
            }
        }
        serde_yaml::Value::Mapping(map) => {
            for (_k, v) in map.iter_mut() {
                substitute_recipe_placeholders(v, prompts, cmds, max_iterations);
            }
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// plan
// ---------------------------------------------------------------------------

/// Return the two stages from the `gremlins:plan` recipe, loaded from the
/// bundled `plan.yaml` asset.
///
/// `prompts` replaces the `{{prompt}}` placeholder.
pub fn plan(prompts: Vec<String>) -> Vec<RunnableStage> {
    load_recipe_stages("plan", &prompts, None, None).expect("bundled plan recipe must be valid")
}

// ---------------------------------------------------------------------------
// implement
// ---------------------------------------------------------------------------

/// Return the three stages from the `gremlins:implement` recipe, loaded from
/// the bundled `implement.yaml` asset.
///
/// `prompts` replaces the `{{prompt}}` placeholder.
pub fn implement(prompts: Vec<String>) -> Vec<RunnableStage> {
    load_recipe_stages("implement", &prompts, None, None)
        .expect("bundled implement recipe must be valid")
}

// ---------------------------------------------------------------------------
// verify
// ---------------------------------------------------------------------------

/// Return a pre-configured [`LoopBuilder`] matching the `gremlins:verify`
/// recipe, loaded from the bundled `verify.yaml` asset.
///
/// `cmds` replaces the `{{options.cmds}}` placeholder.  `max_iterations`
/// replaces `{{options.max_iterations | default(3)}}`.  `fix_prompts` replaces
/// the `{{prompt}}` placeholder for the fix agent.
pub fn verify(cmds: Vec<String>, max_iterations: u32, fix_prompts: Vec<String>) -> LoopBuilder {
    let mut stages = load_recipe_stages("verify", &fix_prompts, Some(&cmds), Some(max_iterations))
        .expect("bundled verify recipe must be valid");

    assert_eq!(stages.len(), 1, "verify recipe must have exactly one stage");
    let stage = stages.remove(0);
    LoopBuilder::from_parsed(stage)
}

// ---------------------------------------------------------------------------
// plan_gh
// ---------------------------------------------------------------------------

/// Return the four stages from the `plan-gh` recipe (the GitHub-aware plan
/// variant).  This recipe has no bundled YAML asset — it is constructed
/// programmatically.
///
/// `prompts` replaces the `{{prompt}}` placeholder.
pub fn plan_gh(prompts: Vec<String>) -> Vec<RunnableStage> {
    // Load the base plan recipe and add the GitHub-specific stages.
    let mut base = load_recipe_stages("plan", &prompts, None, None)
        .expect("bundled plan recipe must be valid");

    // The base plan recipe has two stages: plan, set-description.
    // plan_gh inserts resolve-plan-source and publish-as-issue between them.
    assert!(base.len() >= 2, "plan recipe must have at least 2 stages");
    let set_description = base.pop().unwrap(); // set-description

    use crate::builders::artifacts::artifact;
    use crate::builders::exec::ExecBuilder;

    let resolve_plan_source = ExecBuilder::new("resolve-plan-source")
        .interpolate("plan", "artifact://plan.md")
        .cmd("gh_resolve_plan_source \"{plan}\" \"{plan_issue_number}\"")
        .bind(
            "plan_issue_number?",
            artifact("artifact://plan-issue-number.txt"),
        )
        .build();

    let publish_as_issue = ExecBuilder::new("publish-as-issue")
        .interpolate("plan", "artifact://plan.md")
        .skip_if_exists("artifact://plan-issue-number.txt")
        .cmd("gh_publish_issue \"{plan}\" > \"{plan_issue_number}\"")
        .bind(
            "plan_issue_number",
            artifact("artifact://plan-issue-number.txt"),
        )
        .build();

    base.push(resolve_plan_source);
    base.push(publish_as_issue);
    base.push(set_description);
    base
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_recipe_has_two_stages() {
        let stages = plan(vec!["write a plan".to_string()]);
        assert_eq!(stages.len(), 2);
        assert_eq!(stages[0].name(), "plan");
        assert_eq!(stages[0].stage_type(), "agent");
        assert_eq!(stages[0].skip_if_exists(), "artifact://plan.md");
        assert_eq!(stages[1].name(), "set-description");
        assert_eq!(stages[1].stage_type(), "exec");
    }

    #[test]
    fn implement_recipe_has_three_stages() {
        let stages = implement(vec!["implement the plan".to_string()]);
        assert_eq!(stages.len(), 3);
        assert_eq!(stages[0].name(), "implement");
        assert_eq!(stages[0].stage_type(), "agent");
        assert_eq!(stages[1].name(), "git-commit");
        assert_eq!(stages[1].stage_type(), "exec");
        assert_eq!(stages[2].name(), "require-impl-progress");
        assert_eq!(stages[2].stage_type(), "exec");
    }

    #[test]
    fn verify_recipe_builds_loop() {
        let lp = verify(
            vec!["make test".to_string()],
            5,
            vec!["fix the tests".to_string()],
        );
        let stage = lp.build();
        assert_eq!(stage.name(), "verify");
        assert_eq!(stage.stage_type(), "loop");
        let body = stage.body();
        assert_eq!(body.len(), 2);
        assert_eq!(body[0].name(), "cmd");
        assert_eq!(body[0].stage_type(), "exec");
        assert_eq!(body[1].name(), "fix");
        assert_eq!(body[1].stage_type(), "agent");
    }

    #[test]
    fn plan_gh_recipe_has_four_stages() {
        let stages = plan_gh(vec!["write a plan".to_string()]);
        assert_eq!(stages.len(), 4);
        assert_eq!(stages[0].name(), "plan");
        assert_eq!(stages[1].name(), "resolve-plan-source");
        assert_eq!(stages[2].name(), "publish-as-issue");
        assert_eq!(stages[3].name(), "set-description");
    }
}
