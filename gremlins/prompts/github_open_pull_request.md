You are composing a GitHub pull request for changes on a detached HEAD. Write three files:

- `{session_dir}/pr-branch.txt`: one line — the branch name to push. If `{plan_uri}` matches `gh://issue/N`, use `issue-N-<short-slug>`; otherwise use a short descriptive slug based on the changes.
- `{session_dir}/pr-title.txt`: one line — the PR title.
- `{session_dir}/pr-body.md`: the PR body in markdown. If `{plan_uri}` matches `gh://issue/N`, include `Closes #N` on its own line. If `{plan_uri}` is empty, do NOT include any 'Closes' or 'Fixes' line.

The PR will target `{base_ref_to_open_pr}`. Do NOT push or call `gh pr create` — another stage handles that.
