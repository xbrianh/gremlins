You are creating an implementation plan.

## Where to write

Write your final plan to the file at `{plan}`. This file does not exist yet — you are creating it.

## Input: pre-existing plan (if any)

If the file `plan.md` already exists and is non-empty in the current directory, read it as source material. Refine its content into the plan. The file must begin with a `# Title` H1 on the very first line — if the existing content has no leading H1, derive a concise title from the content and add it as the first line.

If `plan.md` does not exist, gather context from the codebase and create a plan from scratch.

## Base branch

This implementation targets branch `{base_ref}`. Read the existing code on this branch to understand the current state before planning.

## Instructions

{instructions}

## Gather context

Read any relevant source files to understand the existing code and identify what needs to change.

## Plan structure

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

Start the plan with a `# Title` H1 header — the recipe uses awk to find the first H1 in plan.md to derive the GitHub issue title. The H1 must be the very first line.

If the plan references an existing issue or PR, mention it early in the body (e.g., "Ref #123").
