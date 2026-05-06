You are creating an implementation plan and posting it as a new GitHub issue.

## Reference (optional)

{ref}

## Instructions

{instructions}

## Gather context

If a reference was provided above, fetch it:

- If it looks like an issue number (e.g. `123` or `#123`): fetch with `gh issue view <number> --json title,body,labels,comments`
- If it looks like a PR number or PR URL: fetch with `gh pr view <ref> --json title,body,files,comments`
- If it looks like a URL: fetch the content
- If no reference was provided, use the instructions and codebase to understand the task

Read any relevant code in the repo to inform the plan.

## Create the plan

Write a detailed implementation plan structured as:

```
## Context
What problem are we solving and why.

## Approach
High-level strategy. Why this approach over alternatives.

## Tasks
- [ ] Task 1: concrete, specific description
- [ ] Task 2: concrete, specific description
- [ ] Task 3: concrete, specific description

## Open questions
Anything that needs discussion before implementation.
```

## Post to GitHub

Post the plan as a **new GitHub issue** using `gh issue create`.

- Derive a clear, concise title from the plan
- If the plan references an existing issue/PR, mention it in the body (e.g. "Ref #123")
- Add relevant labels if appropriate
- After creating, output the issue URL so the user can see it
