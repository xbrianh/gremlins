<!-- placeholders: base_ref, instructions, session_dir -->
You are creating an implementation plan. Write the plan to `{session_dir}/plan.md`.

## Base branch

This implementation targets branch `{base_ref}`. Read the existing code on this branch to understand the current state before planning.

## Instructions

{instructions}

## Gather context

Read any relevant source files to understand the existing code and identify what needs to change.

## Create the plan

Write a detailed implementation plan structured as:

```
# <concise title summarizing the work>

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

Start the plan with a `# Title` H1 header — the recipe uses `head -1 plan.md` to derive the GitHub issue title. The H1 must be the very first line.

If the plan references an existing issue or PR, mention it early in the body (e.g., "Ref #123").
