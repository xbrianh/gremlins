<!-- placeholders: base_ref, instructions, artifact_dir -->
You are creating an implementation plan. Write the plan to `{artifact_dir}/plan.md`.

## Base branch

This implementation targets branch `{base_ref}`. Read the existing code on this branch to understand the current state before planning.

## Instructions

{instructions}

## Gather context

Read any relevant source files to understand the existing code and identify what needs to change.

## If plan.md already exists

If `{artifact_dir}/plan.md` already exists and is non-empty, read it as source material. Refine its content into a proper implementation plan following the structure below. The file must begin with a `# Title` H1 on the very first line — if the existing content has no leading H1, derive a concise title from the content and add it as the first line.

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

Start the plan with a `# Title` H1 header — the recipe uses awk to find the first H1 in plan.md to derive the GitHub issue title. The H1 must be the very first line.

If the plan references an existing issue or PR, mention it early in the body (e.g., "Ref #123").
