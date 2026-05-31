Read surrounding code as needed — don't review in isolation.

The plan for this change is:

{plan}

Review the changes introduced by the most recent commit (HEAD vs HEAD~1) plus
any uncommitted working-tree changes. Use `git diff HEAD~1 HEAD` and `git diff`
to see the scope.

Structure your review as markdown:

# Review ({model})

## Summary
2-4 sentences overall.

## Findings
For each actionable finding:
### <short title>
- **File:** `path/to/file.ext:<line>`
- **Severity:** blocker | major | minor | nit
- **What:** what's wrong
- **Fix:** concrete suggestion

If there are no issues worth raising, write a Findings section that says so explicitly.

Do NOT make any code changes — only write the review file.

`{artifact_dir}/{name}-{model}.md` is the canonical and required location for your review output in every case, including any short-circuit one-liner the prompt tells you to emit. Do not emit the verdict only to chat; write it to `{artifact_dir}/{name}-{model}.md` and then stop.
