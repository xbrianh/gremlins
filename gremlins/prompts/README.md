# Prompt interpolation reference

Every placeholder in the templates below is a runtime value supplied by the
calling stage module via Python's `.format(...)`. Static text lives in separate
`.md` fragments composed via the YAML `prompt:` list. Literal braces that must
survive `.format(...)` unchanged (e.g. `{owner}` in command examples) are
escaped as `{{owner}}` in the template source.

## Placeholder table

| Placeholder | What it contains | Provided by | Notes |
|---|---|---|---|
| `{plan_text}` | Full text of the implementation plan | `implement.py` | Multi-line; may be empty when spec drives the work |
| `{diff_text}` | `git diff` of uncommitted changes | `verify.py` | Multi-line; empty on clean worktree |
| `{verify_output}` | Captured stdout+stderr of the failed check/test run | `verify.py` | Multi-line |
| `{failure_output}` | Captured CI check failure logs from the PR | `github_wait_ci.py` | Multi-line |
| `{commands_section}` | Markdown-formatted list of commands that were run | `verify.py` | Single or multi-line |
| `{instructions}` | User-supplied task instructions | `plan.py`, `ghplan.py` | Multi-line |
| `{plan_file}` | Absolute path where the plan should be written | `plan.py` | File path string |
| `{ref}` | Optional issue/PR reference passed to the plan stage | `ghplan.py` | May be empty string |
| `{model}` | Model identifier extracted from the review filename | `github_address_pull_request_reviews.py` | e.g. `claude-opus-4-7` |
| `{text}` | Full concatenated text of one or more review files | `github_address_pull_request_reviews.py` | Multi-line |
| `{spec_block}` | Rendered overarching chain spec block | `implement.py` | Multi-line; empty string when no spec |
| `{plan_source_label}` | Human-readable label for where the plan came from | `implement.py` | e.g. `"from the GitHub issue"` or `"below"` |
| `{plan_location_note}` | Sentence about where the plan lives | `implement.py` | Appended to the implement preamble |
