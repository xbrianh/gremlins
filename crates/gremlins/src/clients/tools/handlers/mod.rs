//! Concrete tool handlers dispatched by [`crate::clients::tools::invoke_with_conversation`].
//!
//! The runtime layer in `clients::tools` owns gate/validate/audit/dispatch.
//! Each tool implementation lives in its own focused module below, so a
//! dependency (landscape, skills, regex, …) is paid only by the tool that
//! needs it.

pub(crate) mod bash;
pub(crate) mod done;
pub(crate) mod edit;
pub(crate) mod glob;
pub(crate) mod grep;
pub(crate) mod landscape;
pub(crate) mod mischief;
pub(crate) mod read;
pub(crate) mod skills;
pub(crate) mod task;
pub(crate) mod websearch;
pub(crate) mod write;
use std::sync::Arc;

use crate::clients::tools::registry::ToolRegistry;

pub(crate) fn default_registry() -> ToolRegistry {
    ToolRegistry::new(vec![
        ("Read", Arc::new(read::ReadTool)),
        ("Edit", Arc::new(edit::EditTool)),
        ("Bash", Arc::new(bash::BashTool)),
        ("Write", Arc::new(write::WriteTool)),
        ("Grep", Arc::new(grep::GrepTool)),
        ("Glob", Arc::new(glob::GlobTool)),
        ("Landscape", Arc::new(landscape::LandscapeTool)),
        ("WebSearch", Arc::new(websearch::WebSearchTool)),
        ("LoadSkill", Arc::new(skills::LoadSkillTool)),
        ("Task", Arc::new(task::TaskTool)),
        ("Done", Arc::new(done::DoneTool)),
        ("Mischief", Arc::new(mischief::MischiefTool)),
    ])
}
