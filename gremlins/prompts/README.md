# Prompt interpolation reference

Every placeholder in the templates below is a runtime value supplied by the
calling stage module via Python's `.format(...)`. Static text lives in separate
`.md` fragments composed via the YAML `prompt:` list. Literal braces that must
survive `.format(...)` unchanged (e.g. `{owner}` in command examples) are
escaped as `{{owner}}` in the template source.

## Placeholder table

| Placeholder | What it contains | Provided by | Notes |
|---|---|---|---|
| `{plan_text}` | Full text of the implementation plan | `implement.py` (local) | Multi-line; may be empty when spec drives the work |
| `{diff_text}` | `git diff` of uncommitted changes | `verify.py` | Multi-line; empty on clean worktree |
| `{verify_output}` | Captured stdout+stderr of the failed check/test run | `verify.py` | Multi-line |
| `{failure_output}` | Captured CI check failure logs from the PR | `wait_ci.py` | Multi-line |
| `{commands_section}` | Markdown-formatted list of commands that were run | `verify.py` | Single or multi-line |
| `{commit_instr}` | Instruction fragment: whether to commit after fixing | `verify.py` | Empty string or a sentence |
| `{address_commit_instr}` | Instruction fragment: whether to commit after addressing | `address_code.py` | Empty string or a sentence |
| `{impl_commit_instr}` | Instruction fragment for committing during implementation | `implement.py` (local) | A period (`.`) or a loaded prompt fragment |
| `{pr_url}` | Full GitHub PR URL | `ghreview.py`, `ghaddress.py` | e.g. `https://github.com/owner/repo/pull/N` |
| `{issue_body}` | Full text of the GitHub issue body (the plan) | `implement.py` (gh) | Multi-line |
| `{instructions}` | User-supplied task instructions | `plan.py`, `ghplan.py` | Multi-line |
| `{plan_file}` | Absolute path where the plan should be written | `plan.py` | File path string |
| `{ref}` | Optional issue/PR reference passed to the plan stage | `ghplan.py` | May be empty string |
| `{model}` | Model identifier extracted from the review filename | `address_code.py` | e.g. `claude-opus-4-7` |
| `{text}` | Full concatenated text of one or more review files | `address_code.py` | Multi-line |
| `{spec_block}` | Rendered overarching chain spec block | `implement.py` | Multi-line; empty string when no spec |
| `{plan_source_label}` | Human-readable label for where the plan came from | `implement.py` (gh) | e.g. `"from the GitHub issue"` or `"below"` |
| `{plan_location_note}` | Sentence about where the plan lives | `implement.py` (gh) | Appended to the implement preamble |
