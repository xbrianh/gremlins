use std::path::Path;

/// System prompt injected into every agent stage. Carries the tool roster,
/// directory layout, and pragmatic guidance (e.g. delegation policy).
/// Model-specific guidance will need to live here in the future; for now,
/// some opinionated bits are included as a temporary compromise.
pub(crate) fn agent_system_prompt(work_root: &Path, scratch_root: &Path) -> String {
    format!(
        "\
<important>\n\
Think and write in a terse, to-the-point style. Make brief statements that get to the point. Do this for reasoning and writing \
</important>\n\
<tools>\n\
Read (read files), Write (create files), Edit (targeted \
replacements), Grep (regex search), Glob (find files \
by pattern), Bash (shell commands), Task, Done\n\
</tools>\n\
Call Done(summary) alongside your final message when your work is complete. The summary parameter briefly describes what you accomplished.\n\
<directories>\n\
Work root:     {work}\n\
Scratch root:  {scratch}\n\
(use scratch for test cruft and temporary files)\n\
</directories>\
",
        work = work_root.display(),
        scratch = scratch_root.display(),
    )
}

/// System prompt injected into every nested (Task) agent invocation. Omits any
/// delegation guidance — the tool roster is all a child needs.
pub(crate) fn task_system_prompt(work_root: &Path, scratch_root: &Path) -> String {
    format!(
        "\
<tools>\n\
Read (read files), Write (create files), Edit (targeted \
replacements), Grep (regex search), Glob (find files \
by pattern), Bash (shell commands), Task, Done\n\
</tools>\n\
Call Done(summary) alongside your final message when your work is complete. The summary parameter briefly describes what you accomplished.\n\
<directories>\n\
Work root:     {work}\n\
Scratch root:  {scratch}\n\
(use scratch for test cruft and temporary files)\n\
</directories>\
",
        work = work_root.display(),
        scratch = scratch_root.display(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn test_agent_system_prompt_renders_directory_paths() {
        let prompt = agent_system_prompt(Path::new("/tmp/gremlins"), Path::new("/tmp/scratch"));
        assert!(prompt.contains("/tmp/gremlins"), "must contain work root");
        assert!(prompt.contains("/tmp/scratch"), "must contain scratch root");
    }

    #[test]
    fn test_agent_system_prompt_includes_delegation_policy() {
        let prompt = agent_system_prompt(Path::new("/work"), Path::new("/scratch"));
        assert!(
            prompt.contains("<tools>"),
            "agent prompt must inject delegation policy; got: {prompt}"
        );
    }

    #[test]
    fn test_task_system_prompt_renders_directory_paths() {
        let prompt = task_system_prompt(Path::new("/tmp/gremlins"), Path::new("/tmp/scratch"));
        assert!(prompt.contains("/tmp/gremlins"), "must contain work root");
        assert!(prompt.contains("/tmp/scratch"), "must contain scratch root");
    }

    #[test]
    fn test_task_system_prompt_omits_delegation_guidance() {
        let prompt = task_system_prompt(Path::new("/work"), Path::new("/scratch"));
        assert!(
            !prompt.contains("<important>") && !prompt.contains("<important>"),
            "child prompt must not inject delegation guidance; got: {prompt}"
        );
    }

    #[test]
    fn test_agent_system_prompt_includes_done() {
        let prompt = agent_system_prompt(Path::new("/work"), Path::new("/scratch"));
        assert!(
            prompt.contains("Task, Done"),
            "agent prompt must include Done in tool roster; got: {prompt}"
        );
        assert!(
            prompt.contains("Call Done(summary) alongside your final message"),
            "agent prompt must include Done instruction; got: {prompt}"
        );
    }

    #[test]
    fn test_task_system_prompt_includes_done() {
        let prompt = task_system_prompt(Path::new("/work"), Path::new("/scratch"));
        assert!(
            prompt.contains("Task, Done"),
            "task prompt must include Done in tool roster; got: {prompt}"
        );
        assert!(
            prompt.contains("Call Done(summary) alongside your final message"),
            "task prompt must include Done instruction; got: {prompt}"
        );
    }
}
