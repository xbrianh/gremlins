Judge whether the implementation at HEAD satisfies the plan. Do not modify the worktree or implement fixes. Inspect the actual code and tests, using `git diff {base_sha}...HEAD` as a starting point. Check each required behavior, including requested tests; do not infer completeness from a commit message or a green suite. Run focused checks if needed; broad QA follows this gate.

<plan>
{plan}
</plan>

Write your decision to `{verdict}`. The first line must be exactly `COMPLETE` or `RETRY`. Use `COMPLETE` only if you can substantiate all requirements; cite concise evidence. Otherwise use `RETRY` and list the unmet or unverified requirements and concrete next steps for the implementer. Do not demand work outside the plan.
