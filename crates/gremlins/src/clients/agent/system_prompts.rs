use std::path::Path;

// ---------------------------------------------------------------------------
// System prompts
// ---------------------------------------------------------------------------

const COMPLETION_GUIDANCE: &str = "`Done` ends your assignment. Before calling it, check what you were asked to do against what you have actually done. If any part remains, keep working; do not use Done to report progress or describe what you intend to do next. Call Done(summary) alongside your final message when the request is fulfilled. Summarize the result and make Done the only tool call in that message.";

/// System prompt injected into every agent stage. Carries the tool roster,
/// directory layout, and pragmatic guidance (e.g. delegation policy).
/// Model-specific guidance will need to live here in the future; for now,
/// some opinionated bits are included as a temporary compromise.
pub(crate) fn agent_system_prompt(work_root: &Path) -> String {
    format!(
        "\
<important>\n\
Think and write in a terse, to-the-point style. Make brief statements that get to the point. Do this for reasoning and writing \
</important>\n\
<discovery>\n\
Landscape is your most powerful discovery tool. Use it when starting a new \
task, exploring an unfamiliar directory, finding where something lives, or \
getting oriented before reading files. The standard discovery pattern is \
Landscape first for structure, then Grep for targeted search.\n\
</discovery>\n\
<tools>\n\
Landscape (structural outline), Read (read files), Write (create files), Edit (targeted \
replacements), Grep (regex search), Glob (find files \
by pattern), Bash (shell commands), Task, Done\n\
Edit is for code changes (not Bash scripts/sed). Read/Grep/Glob are for file inspection (not shell sed/grep/tail).\n\
</tools>\n\
{COMPLETION_GUIDANCE}\n\
ALWAYS use Landscape first when exploring unknown code — it maps structure without reading files.\n\
<directories>\n\
Work root:     {work}\n\
</directories>\n\
",
        work = work_root.display(),
    )
}

/// System prompt injected into every nested (Task) agent invocation. Omits any
/// delegation guidance — the tool roster is all a child needs.
pub(crate) fn task_system_prompt(work_root: &Path) -> String {
    format!(
        "\
<important>\n\
Think and write in a terse, to-the-point style. Make brief statements that get to the point. Do this for reasoning and writing \
</important>\n\
<discovery>\n\
Landscape is your most powerful discovery tool. Use it when starting a new \
task, exploring an unfamiliar directory, finding where something lives, or \
getting oriented before reading files. The standard discovery pattern is \
Landscape first for structure, then Grep for targeted search.\n\
</discovery>\n\
<tools>\n\
Landscape (structural outline), Read (read files), Write (create files), Edit (targeted \
replacements), Grep (regex search), Glob (find files \
by pattern), Bash (shell commands), Task, Done\n\
Edit is for code changes (not Bash scripts/sed). Read/Grep/Glob are for file inspection (not shell sed/grep/tail).\n\
</tools>\n\
{COMPLETION_GUIDANCE}\n\
ALWAYS use Landscape first when exploring unknown code — it maps structure without reading files.\n\
<directories>\n\
Work root:     {work}\n\
</directories>\
",
        work = work_root.display(),
    )
}

/// System prompt injected into a forked (Fork) agent invocation. The fork
/// inherits the parent's full conversation history, so this preamble is
/// minimal: it lists only the non-Task tools and tells the child not to fork
/// again.
pub(crate) fn fork_system_prompt(work_root: &Path) -> String {
    format!(
        "\
<tools>\n\
Landscape (structural outline), Read (read files), Write (create files), Edit (targeted \
replacements), Grep (regex search), Glob (find files \
by pattern), Bash (shell commands), Done\n\
</tools>\n\
{COMPLETION_GUIDANCE}\n\
Use Landscape first when exploring code — it maps structure without reading files.\nThis is a forked child agent with the parent's full context. Do not call \
Task.\n\
<directories>\n\
Work root:     {work}\n\
</directories>\
",
        work = work_root.display(),
    )
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn test_agent_system_prompt_renders_directory_paths() {
        let prompt = agent_system_prompt(Path::new("/tmp/gremlins"));
        assert!(prompt.contains("/tmp/gremlins"), "must contain work root");
    }

    #[test]
    fn test_agent_system_prompt_includes_delegation_policy() {
        let prompt = agent_system_prompt(Path::new("/work"));
        assert!(
            prompt.contains("<tools>"),
            "agent prompt must inject delegation policy; got: {prompt}"
        );
    }

    #[test]
    fn test_task_system_prompt_renders_directory_paths() {
        let prompt = task_system_prompt(Path::new("/tmp/gremlins"));
        assert!(prompt.contains("/tmp/gremlins"), "must contain work root");
    }

    #[test]
    fn test_task_system_prompt_includes_important_style() {
        let prompt = task_system_prompt(Path::new("/work"));
        assert!(
            prompt.contains("<important>"),
            "task prompt must include the terse-style guidance block; got: {prompt}"
        );
        assert!(
            prompt.contains("terse, to-the-point style"),
            "task prompt must include terse style text; got: {prompt}"
        );
    }

    #[test]
    fn test_agent_system_prompt_includes_done() {
        let prompt = agent_system_prompt(Path::new("/work"));
        assert!(
            prompt.contains("Task, Done"),
            "agent prompt must include Done in tool roster; got: {prompt}"
        );
        assert!(
            prompt.contains("Call Done(summary) alongside your final message"),
            "agent prompt must include Done instruction; got: {prompt}"
        );
        assert!(
            prompt.contains("Edit is for code changes (not Bash scripts/sed). Read/Grep/Glob are for file inspection (not shell sed/grep/tail)."),
            "agent prompt must include anti-pattern tool guidance; got: {prompt}"
        );
    }

    #[test]
    fn test_task_system_prompt_includes_done() {
        let prompt = task_system_prompt(Path::new("/work"));
        assert!(
            prompt.contains("Task, Done"),
            "task prompt must include Done in tool roster; got: {prompt}"
        );
        assert!(
            prompt.contains("Call Done(summary) alongside your final message"),
            "task prompt must include Done instruction; got: {prompt}"
        );
        assert!(
            prompt.contains("Edit is for code changes (not Bash scripts/sed). Read/Grep/Glob are for file inspection (not shell sed/grep/tail)."),
            "task prompt must include anti-pattern tool guidance; got: {prompt}"
        );
    }

    #[test]
    fn test_fork_system_prompt_lists_no_task_and_forbids_forking() {
        let prompt = fork_system_prompt(Path::new("/work"));
        assert!(
            prompt.contains("Done"),
            "fork prompt must include Done in tool roster; got: {prompt}"
        );
        assert!(
            !prompt.contains("Task,"),
            "fork prompt must not list Task; got: {prompt}"
        );
        assert!(
            prompt.contains("Do not call Task.\n"),
            "fork prompt must forbid further task calls; got: {prompt}"
        );
        assert!(
            !prompt.contains("kind=") && !prompt.contains("Fork\""),
            "fork prompt must not name a specific Task kind; got: {prompt}"
        );
        assert!(prompt.contains("/work"));
    }

    #[test]
    fn test_all_prompts_share_completion_guidance() {
        let agent = agent_system_prompt(Path::new("/work"));
        let task = task_system_prompt(Path::new("/work"));
        let fork = fork_system_prompt(Path::new("/work"));
        for (name, prompt) in [("agent", &agent), ("task", &task), ("fork", &fork)] {
            assert!(
                prompt.contains(COMPLETION_GUIDANCE),
                "{name} prompt must include the completion protocol"
            );
        }
    }
}
