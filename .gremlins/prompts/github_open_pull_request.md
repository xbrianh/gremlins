You are composing a GitHub pull request for implementation work that has already been committed.

## Implementation plan

<plan>
{plan}
</plan>

## Changes since base

<diff>
{diff_summary}
</diff>

Do NOT run git commands or inspect the working tree — all the information you need is above.
Write exactly the following two files using the provided content, then stop:

- `{pr_title}` — One line: the PR title. Derive from the plan heading.
- `{pr_body}` — The PR body in markdown. Summarize the plan briefly, then reference the changes from the diff summary. Include `Closes #{plan_issue_number}` on its own line.

The PR will target `{base_ref_to_open_pr}`. The branch name, push, and `gh pr create` are handled by another stage — do not generate a branch name.