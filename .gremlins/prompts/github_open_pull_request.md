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
Write exactly the following three files using the provided content, then stop:

- `{pr_branch}` — One line: the branch name to push. If `{plan_source_issue_number}` is non-empty, use `issue-{plan_source_issue_number}-<short-slug>`; otherwise derive a short descriptive slug from the plan or diff summary.
- `{pr_title}` — One line: the PR title. Derive from the plan heading.
- `{pr_body}` — The PR body in markdown. Summarize the plan briefly, then reference the changes from the diff summary. Include `Closes #<number>` on its own line, using whichever issue number is non-empty: prefer `{plan_source_issue_number}`, then fall back to `{plan_issue_number}`. If both are empty, do NOT include any 'Closes' or 'Fixes' line.

The PR will target `{base_ref_to_open_pr}`. Do NOT push or call `gh pr create` — another stage handles that.