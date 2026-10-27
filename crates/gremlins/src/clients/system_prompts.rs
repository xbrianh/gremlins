use std::path::Path;

// ---------------------------------------------------------------------------
// System prompts
// ---------------------------------------------------------------------------

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
Before every turn, scan the tools you're about to call. When two or more \
calls are independent (neither depends on the other's result), batch them in \
a single message. The harness executes concurrent calls in parallel, so \
batching reduces wall-clock time. Serial chains of single-call turns are the \
most expensive pattern — avoid them. `Done` signals turn completion and must be the sole call in its message — never batch it with other tools.\n\
ALWAYS use Landscape first when exploring unknown code — it maps structure without reading files.\nCall Done(summary) alongside your final message when your work is complete. The summary parameter briefly describes what you accomplished.\n\
<directories>\n\
Work root:     {work}\n\
</directories>\n\
<scratchpad>\n\
You have a persistent scratchpad at {{scratchpad_path}}. Use the Write tool to update it with findings, decisions, and\n\
context you want to preserve across turns — file paths, discovered\n\
patterns, TODO items, architectural notes. Keep it concise.\n\
\n\
The harness injects the scratchpad into your prompt at the start of\n\
every turn, so you never need to Read it. To update it, write the full\n\
revised content with the Write tool.\n\
</scratchpad>\
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
Before every turn, scan the tools you're about to call. When two or more \
calls are independent (neither depends on the other's result), batch them in \
a single message. The harness executes concurrent calls in parallel, so \
batching reduces wall-clock time. Serial chains of single-call turns are the \
most expensive pattern — avoid them. `Done` signals turn completion and must be the sole call in its message — never batch it with other tools.\n\
ALWAYS use Landscape first when exploring unknown code — it maps structure without reading files.\nCall Done(summary) alongside your final message when your work is complete. The summary parameter briefly describes what you accomplished.\n\
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
Before every turn, scan the tools you're about to call. When two or more \
calls are independent (neither depends on the other's result), batch them in \
a single message. The harness executes concurrent calls in parallel, so \
batching reduces wall-clock time. Serial chains of single-call turns are the \
most expensive pattern — avoid them. `Done` signals turn completion and must be the sole call in its message — never batch it with other tools.\n\
Use Landscape first when exploring code — it maps structure without reading files.\nThis is a forked child agent with the parent's full context. Do not call \
Task.\n\
Call Done(summary) alongside your final message when your work is complete. The summary parameter briefly describes what you accomplished.\n\
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
    fn test_agent_system_prompt_includes_scratchpad() {
        let prompt = agent_system_prompt(Path::new("/work"));
        assert!(
            prompt.contains("<scratchpad>"),
            "agent prompt must describe the scratchpad; got: {prompt}"
        );
        assert!(
            prompt.contains("{scratchpad_path}"),
            "agent prompt must name the scratchpad placeholder; got: {prompt}"
        );
    }

    #[test]
    fn test_task_system_prompt_excludes_scratchpad() {
        let prompt = task_system_prompt(Path::new("/work"));
        assert!(
            !prompt.contains("<scratchpad>"),
            "task prompt must not mention scratchpad; nested workers are stateless; got: {prompt}"
        );
        assert!(
            !prompt.contains("{scratchpad_path}"),
            "task prompt must not name the scratchpad placeholder; got: {prompt}"
        );
    }

    #[test]
    fn test_fork_system_prompt_excludes_scratchpad() {
        let prompt = fork_system_prompt(Path::new("/work"));
        assert!(
            !prompt.contains("<scratchpad>"),
            "fork prompt must not mention scratchpad; nested workers are stateless; got: {prompt}"
        );
        assert!(
            !prompt.contains("{scratchpad_path}"),
            "fork prompt must not name the scratchpad placeholder; got: {prompt}"
        );
    }
}
