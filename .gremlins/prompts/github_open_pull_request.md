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

- `{pr_branch}` — One line: the branch name to push. If `{plan_issue_number}` is non-empty, use `issue-{plan_issue_number}-<short-slug>`; otherwise derive a short descriptive slug from the plan or diff summary.
- `{pr_title}` — One line: the PR title. Derive from the plan heading.
- `{pr_body}` — The PR body in markdown. Summarize the plan briefly, then reference the changes from the diff summary. If `{plan_issue_number}` is non-empty, include `Closes #{plan_issue_number}` on its own line. If empty, do NOT include any 'Closes' or 'Fixes' line.

The PR will target `{base_ref_to_open_pr}`. Do NOT push or call `gh pr create` — another stage handles that.